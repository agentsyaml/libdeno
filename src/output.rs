//! Output capture: fd-level stdout/stderr redirection for a single run.
//!
//! deno_core's op_print writes to the process-global stdout()/stderr()
//! (fd 1/2) with no injectable writer, so the only way to capture a script's
//! console output in-process is to redirect the fd itself for the duration
//! of the run: dup2 the fd onto a pipe, read the pipe on a side thread, and
//! restore the original fd afterwards.
//!
//! The redirection is process-global while active: other threads of the host
//! that print to fd 1/2 during the run land in the captured buffer too.
//! `run` calls are serialized on CWD_LOCK, so libdeno runs never overlap,
//! but host-side concurrent printing is a documented caveat.

// Fds are plain libc::c_int (CRT fds) on every platform: libc's pipe/dup/
// dup2/read/close all speak CRT fds, and std::os::fd (OwnedFd) does not exist
// on Windows. Closing is explicit at the two exit points (finish / Drop).

/// Redirects fd 1 (stdout) and/or fd 2 (stderr) to pipes for the lifetime of
/// the guard; [`finish`](Self::finish) restores them and returns the bytes.
pub(crate) struct OutputCapture {
    stdout: Option<StreamCapture>,
    stderr: Option<StreamCapture>,
}

impl OutputCapture {
    pub(crate) fn new(capture_stdout: bool, capture_stderr: bool) -> Result<Self, std::io::Error> {
        let stdout = if capture_stdout {
            Some(StreamCapture::new(1)?)
        } else {
            None
        };
        let stderr = if capture_stderr {
            Some(StreamCapture::new(2)?)
        } else {
            None
        };
        Ok(Self { stdout, stderr })
    }

    /// Restores the original fds and returns (captured stdout, captured
    /// stderr). Call unconditionally after the run, even on error, so the
    /// redirection never outlives the run.
    pub(crate) fn finish(mut self) -> (Vec<u8>, Vec<u8>) {
        let stdout = self.stdout.take().map(|s| s.finish()).unwrap_or_default();
        let stderr = self.stderr.take().map(|s| s.finish()).unwrap_or_default();
        (stdout, stderr)
    }
}

impl Drop for OutputCapture {
    fn drop(&mut self) {
        // Panic-safety: restore whatever was not finished.
        self.stdout.take().map(|s| s.finish());
        self.stderr.take().map(|s| s.finish());
    }
}

struct StreamCapture {
    /// The fd being redirected (1 = stdout, 2 = stderr).
    target: libc::c_int,
    /// The original fd, kept open so it can be restored on finish.
    saved: libc::c_int,
    /// Captured blocks from the (detached) reader thread.
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
}

impl StreamCapture {
    fn new(fd: libc::c_int) -> Result<Self, std::io::Error> {
        let mut fds = [0; 2];
        let rc = unsafe {
            #[cfg(unix)]
            {
                libc::pipe(fds.as_mut_ptr())
            }
            #[cfg(windows)]
            {
                // _pipe(fds, size, textmode); binary mode (0) so bytes are raw.
                libc::pipe(fds.as_mut_ptr(), 4096, 0)
            }
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);
        // Mark both pipe ends close-on-exec: without it, a child the script
        // spawns (Deno.Command / child_process inheriting stdout) keeps the
        // write end open after the run, so finish()'s wait would block until
        // every child exits. Note the dup2'd copy at `fd` must be re-marked
        // too: dup2 clears the flag. Unix only: libc has no fcntl on Windows
        // (CRT fds need SetHandleInformation); the finish() timeout below
        // covers the child-holds-pipe case there.
        #[cfg(unix)]
        let set_cloexec = |fd: libc::c_int| {
            // SAFETY: fd was just created by pipe() (or dup2'd to a pipe end).
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags >= 0 {
                unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
            }
        };
        #[cfg(unix)]
        {
            set_cloexec(read_fd);
            set_cloexec(write_fd);
        }
        // Keep a copy of the original fd so it can be restored on finish.
        // dup() may fail (e.g. a daemonized host that closed its stdio and
        // let pipe() reuse fd 1): surface a clean error instead of corrupting
        // the bookkeeping.
        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd < 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            return Err(err);
        }
        let saved = dup_fd;
        if unsafe { libc::dup2(write_fd, fd) } < 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(read_fd);
                libc::close(write_fd);
            }
            return Err(err);
        }
        #[cfg(unix)]
        set_cloexec(fd);
        // The pipe write end now lives at `fd` (dup2 copied it); the original
        // write fd is dropped so the pipe reaches EOF exactly when the run
        // restores `fd` on finish.
        unsafe {
            libc::close(write_fd);
        }
        let (tx, rx) = std::sync::mpsc::channel();
        // Dropping the handle detaches: finish() drains the channel with a
        // timeout, and joining could block forever while a detached child
        // holds the pipe write end (macOS posix_spawn ignores FD_CLOEXEC).
        // The thread ends on its own once EOF arrives or the receiver
        // disconnects.
        let _ = std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                // read()'s count type differs per platform: size_t on unix,
                // unsigned int on Windows. Mutually exclusive cfg bindings
                // (a shadowed binding would trip -D warnings on Windows).
                #[cfg(not(windows))]
                let count = buf.len();
                #[cfg(windows)]
                let count = buf.len() as libc::c_uint;
                let n =
                    unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, count) };
                if n < 0
                    && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted
                {
                    continue; // EINTR (e.g. a signal handler): retry, don't truncate
                }
                if n <= 0 {
                    break; // EOF, or the fd was closed/broken mid-run
                }
                if tx.send(buf[..n as usize].to_vec()).is_err() {
                    break; // receiver gave up (finish timeout); stop reading
                }
            }
            unsafe {
                libc::close(read_fd);
            }
        });
        Ok(Self {
            target: fd,
            saved,
            rx,
        })
    }

    /// Restores the original fd (which closes the last pipe write end and so
    /// unblocks the reader thread) and collects the captured bytes.
    ///
    /// The wait is capped as a defense: on macOS `posix_spawn` inherits every
    /// fd regardless of FD_CLOEXEC, so a child the script spawned and
    /// `unref()`ed (detached, not awaited by the event loop) may hold the
    /// pipe write end open after the run, and EOF would never arrive — a
    /// daemonized child would hang the host forever. Linux closes the pipe in
    /// the exec'd child via CLOEXEC; macOS needs the timeout, which returns
    /// whatever was captured so far. A child that never exits also leaves the
    /// detached reader thread blocked in read() until it does (one thread +
    /// fd per such run — documented trade-off).
    fn finish(self) -> Vec<u8> {
        // SAFETY: `saved` is a valid open fd; failure to restore leaves the
        // host's fd 1/2 pointed at the pipe, so report it rather than
        // silently swallowing.
        let restored = unsafe { libc::dup2(self.saved, self.target) };
        if restored < 0 {
            eprintln!(
                "libdeno: failed to restore fd {} after output capture: {}",
                self.target,
                std::io::Error::last_os_error()
            );
        }
        // The original fd's content was copied back to `target`; the spare
        // copy is no longer needed.
        unsafe {
            libc::close(self.saved);
        }
        // Collect blocks until EOF (channel closed) or the cap expires. The
        // cap only fires when a spawned child holds the write end open; on
        // every normal run EOF arrives right after the restore above.
        let mut out = Vec::new();
        loop {
            match self.rx.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(block) => out.extend_from_slice(&block),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        out
    }
}

impl Drop for StreamCapture {
    fn drop(&mut self) {
        // Partial-setup safety: if the *second* capture's construction fails,
        // the first (already redirected) StreamCapture is dropped without
        // finish() ever running, leaving the host's fd 1/2 pointed at a pipe
        // with no reader — host stdout would be swallowed, then block once
        // the pipe buffer fills. Restore the fd here so the redirect never
        // outlives its capture. Harmless when finish() already ran (its
        // dup2 restored the same fd; saved was closed with self).
        unsafe {
            libc::dup2(self.saved, self.target);
            libc::close(self.saved);
        }
    }
}
