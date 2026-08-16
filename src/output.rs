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
    pub(crate) fn new(
        capture_stdout: bool,
        capture_stderr: bool,
        max_bytes: Option<usize>,
    ) -> Result<Self, std::io::Error> {
        let stdout = if capture_stdout {
            Some(StreamCapture::new(1, max_bytes)?)
        } else {
            None
        };
        let stderr = if capture_stderr {
            Some(StreamCapture::new(2, max_bytes)?)
        } else {
            None
        };
        Ok(Self { stdout, stderr })
    }

    /// Restores the original fds and returns (captured stdout, captured
    /// stderr, whether either stream hit its byte cap and was truncated).
    /// Call unconditionally after the run, even on error, so the redirection
    /// never outlives the run.
    pub(crate) fn finish(mut self) -> (Vec<u8>, Vec<u8>, bool) {
        let (stdout, trunc_stdout) = self.stdout.take().map(|s| s.finish()).unwrap_or_default();
        let (stderr, trunc_stderr) = self.stderr.take().map(|s| s.finish()).unwrap_or_default();
        (stdout, stderr, trunc_stdout || trunc_stderr)
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
    /// `None` once restored (taken by finish / Drop), so a stale fd number
    /// can never be dup2'd again after the OS reused it for another file.
    saved: Option<libc::c_int>,
    /// Captured blocks from the (detached) reader thread.
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    /// Set by the reader thread when the byte cap was hit (truncated).
    overflow: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl StreamCapture {
    fn new(fd: libc::c_int, max_bytes: Option<usize>) -> Result<Self, std::io::Error> {
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
        let saved = Some(dup_fd);
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
        let overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let overflow_flag = overflow.clone();
        // Dropping the handle detaches: finish() drains the channel with a
        // timeout, and joining could block forever while a detached child
        // holds the pipe write end (macOS posix_spawn ignores FD_CLOEXEC).
        // The thread ends on its own once EOF arrives, the receiver
        // disconnects, or the byte cap is hit.
        let _ = std::thread::spawn(move || {
            let mut buf = [0u8; 65536];
            let mut total: usize = 0;
            let mut discarding = false;
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
                // After the cap fires the thread keeps draining and
                // discarding (never closing the read end): the pipe's write
                // end is the script's fd 1/2, so closing it here would make
                // the script's subsequent writes fail with EPIPE mid-run.
                // Draining also keeps a blocked writer flowing. The drain
                // ends when finish() restores the fds (EOF) or the receiver
                // disconnects.
                if discarding {
                    continue;
                }
                total += n as usize;
                // On the block that crosses the cap, keep the part that fits
                // and drop the rest — the buffer never exceeds max_bytes and
                // still contains "the first max_bytes".
                if let Some(max) = max_bytes {
                    if total > max {
                        let over = total - max;
                        let keep = n as usize - over;
                        if keep > 0 {
                            let _ = tx.send(buf[..keep].to_vec());
                        }
                        overflow_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                        discarding = true;
                        continue;
                    }
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
            overflow,
        })
    }

    /// Restores the original fd (which closes the last pipe write end and so
    /// unblocks the reader thread) and collects the captured bytes.
    ///
    /// The wait is capped at a *total* budget from entry: on macOS
    /// `posix_spawn` inherits every fd regardless of FD_CLOEXEC, so a child
    /// the script spawned and `unref()`ed (detached, not awaited by the event
    /// loop) may hold the pipe write end open after the run, and EOF would
    /// never arrive — a daemonized child would hang the host forever. Linux
    /// closes the pipe in the exec'd child via CLOEXEC; macOS needs the
    /// timeout, which returns whatever was captured so far. The budget is
    /// total, not per-block: a child that *keeps writing* (a logging daemon)
    /// would otherwise reset the idle timer on every block and stall the
    /// caller forever. A child that never exits also leaves the detached
    /// reader thread blocked in read() until it does (one thread + fd per
    /// such run — documented trade-off).
    fn finish(mut self) -> (Vec<u8>, bool) {
        // Defensive: finish() consumes self, so this can only trigger on a
        // double-finish bookkeeping bug. Never dup2 a closed fd number.
        let Some(saved) = self.saved.take() else {
            return (
                Vec::new(),
                self.overflow.load(std::sync::atomic::Ordering::Relaxed),
            );
        };
        // SAFETY: `saved` is a valid open fd; failure to restore leaves the
        // host's fd 1/2 pointed at the pipe with no way back — the host's
        // stdio is permanently corrupted and every later println would be
        // swallowed, then block when the pipe fills. Abort rather than
        // continue in a wedged state.
        if unsafe { libc::dup2(saved, self.target) } < 0 {
            eprintln!(
                "libdeno: failed to restore fd {} after output capture: {}; aborting to avoid \
                 silent stdio corruption",
                self.target,
                std::io::Error::last_os_error()
            );
            std::process::abort();
        }
        // The original fd's content was copied back to `target`; the spare
        // copy is no longer needed.
        unsafe {
            libc::close(saved);
        }
        // Collect blocks until EOF (channel closed) or the budget expires.
        // The budget only fires when a spawned child holds the write end
        // open; on every normal run EOF arrives right after the restore
        // above.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(500);
        let mut out = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            match self.rx.recv_timeout(remaining) {
                Ok(block) => out.extend_from_slice(&block),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        (
            out,
            self.overflow.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

impl Drop for StreamCapture {
    fn drop(&mut self) {
        // Partial-setup safety: if the *second* capture's construction fails,
        // the first (already redirected) StreamCapture is dropped without
        // finish() ever running, leaving the host's fd 1/2 pointed at a pipe
        // with no reader — host stdout would be swallowed, then block once
        // the pipe buffer fills. Restore the fd here so the redirect never
        // outlives its capture. `take()` guards against double-restore: after
        // finish() ran, `saved` is None, so the (possibly reused) fd number
        // is never touched again.
        if let Some(saved) = self.saved.take() {
            unsafe {
                libc::dup2(saved, self.target);
                libc::close(saved);
            }
        }
    }
}
