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
//! The run holds the exclusivity lease (`RunLease`) — any concurrent
//! run is rejected with `Configuration` rather than letting the capture
//! reader steal its output; host-side concurrent printing is a documented
//! caveat.

/// One detached reader and its pipe are kept for every captured stream. A
/// child that inherits a pipe write end can keep that reader blocked after the
/// run's 500ms collection budget; bound those retained resources instead of
/// allowing an unbounded number of blocked threads/fds.
const MAX_ACTIVE_CAPTURE_READERS: usize = 64;
static ACTIVE_CAPTURE_READERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

type CaptureReaderBudget = crate::budget::BudgetGuard;

#[cfg(test)]
fn active_capture_readers() -> usize {
    crate::budget::active(&ACTIVE_CAPTURE_READERS)
}

fn reserve_capture_reader() -> Result<CaptureReaderBudget, std::io::Error> {
    crate::budget::reserve(
        &ACTIVE_CAPTURE_READERS,
        MAX_ACTIVE_CAPTURE_READERS,
        "output capture reader/pipe",
    )
}

// Fds are plain libc::c_int (CRT fds) on every platform: libc's pipe/dup/
// dup2/read/close all speak CRT fds, and std::os::fd (OwnedFd) does not exist
// on Windows. Closing is explicit at the two exit points (finish / Drop).

/// Redirects fd 1 (stdout) and/or fd 2 (stderr) to pipes for the lifetime of
/// the guard; [`finish`](Self::finish) restores them and returns the bytes or
/// a reader I/O error.
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
        // Flush the host's Rust-level stdio buffers *before* the fd changes
        // hands: anything still buffered would otherwise be written to the
        // pipe and reported as the script's output. Nothing is redirected yet,
        // so a flush failure is safe to return as-is.
        if capture_stdout {
            std::io::Write::flush(&mut std::io::stdout())?;
        }
        if capture_stderr {
            std::io::Write::flush(&mut std::io::stderr())?;
        }
        // Validate targets on Unix; all internal descriptors are kept >= 3.
        let saved_stdout = if capture_stdout {
            Some(SavedFd::duplicate(1)?)
        } else {
            None
        };
        let saved_stderr = if capture_stderr {
            Some(SavedFd::duplicate(2)?)
        } else {
            None
        };
        let stdout = if capture_stdout {
            Some(StreamCapture::new(1, max_bytes, saved_stdout.unwrap())?)
        } else {
            None
        };
        let stderr = if capture_stderr {
            Some(StreamCapture::new(2, max_bytes, saved_stderr.unwrap())?)
        } else {
            None
        };
        Ok(Self { stdout, stderr })
    }

    /// Restores the original fds and returns (captured stdout, captured
    /// stderr, whether either stream hit its byte cap and was truncated).
    /// Call unconditionally after the run, even on error, so the redirection
    /// never outlives the run. Reader errors are returned after both streams
    /// have had a chance to restore their fds.
    pub(crate) fn finish(self) -> Result<(Vec<u8>, Vec<u8>, bool), std::io::Error> {
        self.restore().collect()
    }

    /// Restores both fds on the calling thread and returns the still-pending
    /// reader collection. Async hosts use this to keep the (up to 500ms,
    /// blocking) drain off their executor thread while never letting the fd
    /// redirect outlive the run.
    pub(crate) fn restore(mut self) -> PendingCapture {
        let stdout = self.stdout.take().map(|mut stream| {
            stream.restore();
            stream
        });
        let stderr = self.stderr.take().map(|mut stream| {
            stream.restore();
            stream
        });
        PendingCapture { stdout, stderr }
    }
}

/// A capture whose fds are restored; only the reader drain is left.
pub(crate) struct PendingCapture {
    stdout: Option<StreamCapture>,
    stderr: Option<StreamCapture>,
}

impl PendingCapture {
    /// Same contract as [`OutputCapture::finish`] minus the redirection.
    pub(crate) fn collect(self) -> Result<(Vec<u8>, Vec<u8>, bool), std::io::Error> {
        let stdout = self
            .stdout
            .map(|stream| stream.collect())
            .unwrap_or_else(|| Ok((Vec::new(), false)));
        let stderr = self
            .stderr
            .map(|stream| stream.collect())
            .unwrap_or_else(|| Ok((Vec::new(), false)));
        match (stdout, stderr) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok((stdout, trunc_stdout)), Ok((stderr, trunc_stderr))) => {
                Ok((stdout, stderr, trunc_stdout || trunc_stderr))
            }
        }
    }
}

impl Drop for OutputCapture {
    fn drop(&mut self) {
        // Panic-safety: restore whatever was not finished.
        if let Some(stream) = self.stdout.take() {
            drop(stream);
        }
        if let Some(stream) = self.stderr.take() {
            drop(stream);
        }
    }
}

/// Bytes a run had already printed when it failed.
pub(crate) struct CapturedOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) truncated: bool,
}

/// A failed captured run. The output travels with the error because a crash
/// after a few log lines is undebuggable if the lines are dropped at the
/// capture boundary.
pub(crate) struct CapturedRunError {
    pub(crate) error: crate::LibdenoError,
    /// `None` when capture never started or its reader failed, so there is
    /// nothing truthful to report.
    pub(crate) output: Option<CapturedOutput>,
}

impl CapturedRunError {
    pub(crate) fn new(error: crate::LibdenoError) -> Self {
        Self {
            error,
            output: None,
        }
    }

    pub(crate) fn into_error(self) -> crate::LibdenoError {
        self.error
    }
}

/// Shared tail of every capture wrapper: restore the fds, keep the bytes, and
/// fold the run outcome into one value so the four wrappers cannot drift.
/// Error precedence: the run's own error wins over a capture error, since a
/// failed run is the reason the capture ended early.
pub(crate) fn finish_capture(
    capture: OutputCapture,
    result: Result<i32, crate::LibdenoError>,
    options: &crate::LibdenoOptions,
    timing: &crate::ExecutionTiming,
) -> Result<crate::RunOutput, CapturedRunError> {
    let capture_result = if options.capture_stdout || options.capture_stderr {
        let _output_drain = timing.span(crate::timing::Phase::OutputDrain);
        capture.finish()
    } else {
        capture.finish()
    };
    combine_capture(result, capture_result)
}

/// Folds a run outcome and its capture outcome into one value.
pub(crate) fn combine_capture(
    result: Result<i32, crate::LibdenoError>,
    capture_result: Result<(Vec<u8>, Vec<u8>, bool), std::io::Error>,
) -> Result<crate::RunOutput, CapturedRunError> {
    match (result, capture_result) {
        (Ok(exit_code), Ok((stdout, stderr, truncated))) => Ok(crate::RunOutput {
            exit_code,
            stdout,
            stderr,
            capture_truncated: truncated,
        }),
        (Ok(_), Err(error)) => Err(CapturedRunError::new(crate::LibdenoError::Io(error))),
        // A failure with no captured bytes has nothing to add; reporting an
        // empty buffer as "partial output" would be noise for every plain
        // exception.
        (Err(error), Ok((stdout, stderr, truncated)))
            if !stdout.is_empty() || !stderr.is_empty() || truncated =>
        {
            Err(CapturedRunError {
                error,
                output: Some(CapturedOutput {
                    stdout,
                    stderr,
                    truncated,
                }),
            })
        }
        (Err(error), Ok(_)) => Err(CapturedRunError::new(error)),
        (Err(error), Err(_)) => Err(CapturedRunError::new(error)),
    }
}

struct StreamCapture {
    /// The fd being redirected (1 = stdout, 2 = stderr).
    target: libc::c_int,
    /// The original fd, kept open so it can be restored on finish.
    /// `None` once restored (taken by finish / Drop), so a stale fd number
    /// can never be dup2'd again after the OS reused it for another file.
    saved: Option<libc::c_int>,
    #[cfg(unix)]
    target_flags: libc::c_int,
    /// Captured blocks from the (detached) reader thread.
    rx: std::sync::mpsc::Receiver<Result<Vec<u8>, std::io::Error>>,
    /// Set by the reader thread when the byte cap was hit (truncated).
    overflow: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Deferred stdio flush failure, reported by [`Self::collect`].
    flush_error: Option<std::io::Error>,
}

struct SavedFd(libc::c_int);

impl SavedFd {
    fn duplicate(fd: libc::c_int) -> Result<Self, std::io::Error> {
        #[cfg(unix)]
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        #[cfg(windows)]
        let duplicate = unsafe { libc::dup(fd) };
        if duplicate < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(Self(duplicate))
        }
    }
    fn into_raw(self) -> libc::c_int {
        let fd = self.0;
        std::mem::forget(self);
        fd
    }
}

impl Drop for SavedFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

impl StreamCapture {
    fn new(
        fd: libc::c_int,
        max_bytes: Option<usize>,
        dup_fd: SavedFd,
    ) -> Result<Self, std::io::Error> {
        #[cfg(test)]
        if std::env::var("LIBDENO_TEST_FAIL_CAPTURE_FD")
            .ok()
            .as_deref()
            == Some(fd.to_string().as_str())
        {
            return Err(std::io::Error::other(
                "injected output capture setup failure",
            ));
        }
        #[cfg(unix)]
        let target_flags = {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 {
                return Err(std::io::Error::last_os_error());
            }
            flags
        };
        let reader_budget = reserve_capture_reader()?;
        let dup_fd = dup_fd.into_raw();
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
            unsafe {
                libc::close(dup_fd);
            }
            return Err(std::io::Error::last_os_error());
        }
        #[cfg(unix)]
        for slot in 0..2 {
            if fds[slot] < 3 {
                let relocated = unsafe { libc::fcntl(fds[slot], libc::F_DUPFD_CLOEXEC, 3) };
                if relocated < 0 {
                    let error = std::io::Error::last_os_error();
                    unsafe {
                        libc::close(fds[0]);
                        libc::close(fds[1]);
                        libc::close(dup_fd);
                    }
                    return Err(error);
                }
                unsafe {
                    libc::close(fds[slot]);
                }
                fds[slot] = relocated;
            }
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
        let set_cloexec = |fd: libc::c_int| -> Result<(), std::io::Error> {
            // SAFETY: fd was just created by pipe() (or dup2'd to a pipe end).
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
            {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        };
        #[cfg(unix)]
        {
            if let Err(error) = set_cloexec(read_fd).and_then(|_| set_cloexec(write_fd)) {
                unsafe {
                    libc::close(read_fd);
                    libc::close(write_fd);
                    libc::close(dup_fd);
                }
                return Err(error);
            }
        }
        let saved = Some(dup_fd);
        if unsafe { libc::dup2(write_fd, fd) } < 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(dup_fd);
                libc::close(read_fd);
                libc::close(write_fd);
            }
            return Err(err);
        }
        #[cfg(unix)]
        if let Err(error) = set_cloexec(fd) {
            if let Err(restore_error) = restore_target(fd, dup_fd, target_flags) {
                abort_restore(fd, &restore_error);
            }
            unsafe {
                libc::close(dup_fd);
                libc::close(read_fd);
                libc::close(write_fd);
            }
            return Err(error);
        }
        // The pipe write end now lives at `fd` (dup2 copied it); the original
        // write fd is dropped so the pipe reaches EOF exactly when the run
        // restores `fd` on finish.
        unsafe {
            libc::close(write_fd);
        }
        let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>();
        let overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let overflow_flag = overflow.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_flag = stop.clone();
        // Dropping the handle detaches: finish() drains the channel with a
        // timeout, and joining could block forever while a detached child
        // holds the pipe write end (macOS posix_spawn ignores FD_CLOEXEC).
        // The thread ends on its own once EOF arrives, the receiver
        // disconnects, or the byte cap is hit.
        let reader = std::thread::Builder::new()
            .name("libdeno-output-reader".to_string())
            .spawn(move || {
                let mut buf = [0u8; 65536];
                let mut total: usize = 0;
                let mut discarding = false;
                let mut read_error = None;
                // Interruptible wait: a plain blocking read cannot observe the
                // stop flag, so a descendant holding the pipe write end would
                // pin this thread and its budget slot forever.
                let wait_handle = crate::pipe_read::from_raw_fd(read_fd);
                loop {
                    match crate::pipe_read::wait_readable(wait_handle, &stop_flag) {
                        Ok(true) => {}
                        Ok(false) => break,
                        Err(error) => {
                            read_error = Some(error);
                            break;
                        }
                    }
                    // read()'s count type differs per platform: size_t on unix,
                    // unsigned int on Windows. Mutually exclusive cfg bindings
                    // (a shadowed binding would trip -D warnings on Windows).
                    #[cfg(not(windows))]
                    let count = buf.len();
                    #[cfg(windows)]
                    let count = buf.len() as libc::c_uint;
                    let n = unsafe {
                        libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, count)
                    };
                    if stop_flag.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    if n < 0 {
                        let error = std::io::Error::last_os_error();
                        if error.kind() == std::io::ErrorKind::Interrupted {
                            continue; // EINTR (e.g. a signal handler): retry, don't truncate
                        }
                        read_error = Some(error);
                        break;
                    }
                    if n == 0 {
                        break; // EOF
                    }
                    // After the cap fires the thread keeps draining and
                    // discarding (never closing the read end): the pipe's write
                    // end is the script's fd 1/2, so closing it here would make
                    // the script's subsequent writes fail with EPIPE mid-run.
                    // Draining keeps an active writer flowing. finish/drop sets
                    // stop only after the run ends; the reader checks it after
                    // the next read so a capped stream cannot retain its fd.
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
                            if keep > 0 && tx.send(Ok(buf[..keep].to_vec())).is_err() {
                                break;
                            }
                            overflow_flag.store(true, std::sync::atomic::Ordering::Relaxed);
                            discarding = true;
                            continue;
                        }
                    }
                    if tx.send(Ok(buf[..n as usize].to_vec())).is_err() {
                        break; // receiver gave up (finish timeout); stop reading
                    }
                }
                unsafe {
                    libc::close(read_fd);
                }
                // Release the budget only after the pipe read end has actually
                // closed. In particular, a timed-out detached child keeps this
                // reservation while the reader remains blocked in read().
                drop(reader_budget);
                if let Some(error) = read_error {
                    let _ = tx.send(Err(error));
                }
            });
        if let Err(error) = reader {
            // The closure (and its budget reservation) is dropped when thread
            // creation fails. Restore the target fd and close the reader end
            // so a failed capture cannot leave stdio redirected or leak a
            // pipe.
            #[cfg(unix)]
            if let Err(restore_error) = restore_target(fd, dup_fd, target_flags) {
                abort_restore(fd, &restore_error);
            }
            #[cfg(windows)]
            if unsafe { libc::dup2(dup_fd, fd) } < 0 {
                abort_restore(fd, &std::io::Error::last_os_error());
            }
            unsafe {
                libc::close(dup_fd);
                libc::close(read_fd);
            }
            return Err(error);
        }
        Ok(Self {
            target: fd,
            saved,
            #[cfg(unix)]
            target_flags,
            rx,
            overflow,
            stop,
            flush_error: None,
        })
    }

    /// Restores the original fd (which closes the last pipe write end and so
    /// unblocks the reader thread) and returns the captured bytes.
    ///
    /// Split from [`Self::restore`] so a host can restore stdio immediately
    /// and move the (up to 500ms) drain to a blocking pool.
    ///
    /// The wait is capped at a *total* budget from entry: on macOS
    /// `posix_spawn` inherits every fd regardless of FD_CLOEXEC, so a child
    /// the script spawned and `unref()`ed (detached, not awaited by the event
    /// loop) may hold the pipe write end open after the run, and EOF would
    /// never arrive — a daemonized child would hang the host forever. Linux
    /// closes the pipe in the exec'd child via CLOEXEC; macOS needs the
    /// timeout, which reports an incomplete capture. The budget is
    /// total, not per-block: a child that *keeps writing* (a logging daemon)
    /// would otherwise reset the idle timer on every block and stall the
    /// caller forever. A silent descendant that keeps the write end open can
    /// still leave the detached reader blocked in read() until it writes or
    /// exits (one thread + fd per such run — documented trade-off).
    fn restore(&mut self) {
        // Defensive: restore() may only run once; the (possibly reused) fd
        // number must never be dup2'd a second time.
        let Some(saved) = self.saved.take() else {
            return;
        };
        // Flush host-level stdio buffers while the redirect is still in place:
        // bytes the host wrote during the run belong to the captured stream,
        // and flushing after the restore would leak them into the host's real
        // stdout out of order. A flush failure must not skip the restore
        // below, so it is recorded and reported only if collection succeeded.
        self.flush_error = if self.target == 1 {
            std::io::Write::flush(&mut std::io::stdout()).err()
        } else {
            std::io::Write::flush(&mut std::io::stderr()).err()
        };
        // SAFETY: `saved` is a valid open fd; failure to restore leaves the
        // host's fd 1/2 pointed at the pipe with no way back — the host's
        // stdio is permanently corrupted and every later println would be
        // swallowed, then block when the pipe fills. Abort rather than
        // continue in a wedged state.
        #[cfg(unix)]
        if let Err(error) = restore_target(self.target, saved, self.target_flags) {
            abort_restore(self.target, &error);
        }
        #[cfg(windows)]
        if unsafe { libc::dup2(saved, self.target) } < 0 {
            abort_restore(self.target, &std::io::Error::last_os_error());
        }
        // The original fd's content was copied back to `target`; the spare
        // copy is no longer needed.
        unsafe {
            libc::close(saved);
        }
    }

    /// Restore + collect in one step (test-only; production paths use the
    /// split `restore`/`collect` pair).
    #[cfg(test)]
    fn finish(mut self) -> Result<(Vec<u8>, bool), std::io::Error> {
        self.restore();
        self.collect()
    }

    /// Drains the reader after [`Self::restore`]; reports a deferred flush
    /// error only when collection itself succeeded.
    fn collect(mut self) -> Result<(Vec<u8>, bool), std::io::Error> {
        let flush_error = self.flush_error.take();
        collect_blocks(&self.rx, &self.overflow).and_then(|collected| match flush_error {
            Some(error) => Err(error),
            None => Ok(collected),
        })
    }
}

/// Collects reader messages until EOF or the detached-child wait budget.
/// Reader errors are not interchangeable with EOF: callers must see them.
fn collect_blocks(
    rx: &std::sync::mpsc::Receiver<Result<Vec<u8>, std::io::Error>>,
    overflow: &std::sync::atomic::AtomicBool,
) -> Result<(Vec<u8>, bool), std::io::Error> {
    collect_blocks_until(
        rx,
        overflow,
        std::time::Instant::now() + std::time::Duration::from_millis(500),
    )
}

fn collect_blocks_until(
    rx: &std::sync::mpsc::Receiver<Result<Vec<u8>, std::io::Error>>,
    overflow: &std::sync::atomic::AtomicBool,
    deadline: std::time::Instant,
) -> Result<(Vec<u8>, bool), std::io::Error> {
    let mut out = Vec::new();
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(capture_incomplete_error());
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(Ok(block)) => out.extend_from_slice(&block),
            Ok(Err(error)) => return Err(error),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                return Err(capture_incomplete_error());
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok((out, overflow.load(std::sync::atomic::Ordering::Relaxed)))
}

fn capture_incomplete_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "output capture incomplete: collection deadline expired before EOF",
    )
}

impl Drop for StreamCapture {
    fn drop(&mut self) {
        // Partial-setup safety: if the *second* capture's construction fails,
        // the first (already redirected) StreamCapture is dropped without
        // finish() ever running, leaving the host's fd 1/2 pointed at a pipe
        // with no reader — host stdout would be swallowed, then block once
        // the pipe buffer fills. Restore the fd here so the redirect never
        // outlives its capture. `restore()` is a no-op after it already ran.
        self.restore();
        self.stop.store(true, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(unix)]
fn restore_target(
    target: libc::c_int,
    saved: libc::c_int,
    flags: libc::c_int,
) -> Result<(), std::io::Error> {
    loop {
        #[cfg(test)]
        if let Some(error) = injected_restore_error(RestoreCall::Dup2) {
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if unsafe { libc::dup2(saved, target) } >= 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    loop {
        #[cfg(test)]
        if let Some(error) = injected_restore_error(RestoreCall::SetFd) {
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if unsafe { libc::fcntl(target, libc::F_SETFD, flags) } >= 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(all(unix, test))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum RestoreCall {
    Dup2,
    SetFd,
}

#[cfg(all(unix, test))]
thread_local! {
    static RESTORE_FAULT: std::cell::Cell<Option<(RestoreCall, std::io::ErrorKind)>> = const { std::cell::Cell::new(None) };
}

#[cfg(all(unix, test))]
fn injected_restore_error(call: RestoreCall) -> Option<std::io::Error> {
    RESTORE_FAULT.with(|fault| match fault.get() {
        Some((next, kind)) if next == call => {
            fault.set(None);
            Some(std::io::Error::from(kind))
        }
        _ => None,
    })
}

fn abort_restore(target: libc::c_int, error: &std::io::Error) -> ! {
    eprintln!("libdeno: failed to restore fd {target}: {error}; aborting to avoid silent stdio corruption");
    std::process::abort()
}

#[cfg(test)]
mod tests {
    use super::active_capture_readers;
    use super::collect_blocks;
    use super::collect_blocks_until;
    use super::combine_capture;
    use super::reserve_capture_reader;
    use super::MAX_ACTIVE_CAPTURE_READERS;

    #[cfg(unix)]
    #[path = "capture_tests.rs"]
    mod capture_tests;

    #[cfg(unix)]
    #[test]
    fn restore_target_retries_dup_and_fd_flag_eintr() {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let saved = unsafe { libc::fcntl(fds[1], libc::F_DUPFD_CLOEXEC, 3) };
        assert!(saved >= 0);
        let flags = unsafe { libc::fcntl(fds[1], libc::F_GETFD) } | libc::FD_CLOEXEC;
        for call in [super::RestoreCall::Dup2, super::RestoreCall::SetFd] {
            super::RESTORE_FAULT
                .with(|fault| fault.set(Some((call, std::io::ErrorKind::Interrupted))));
            super::restore_target(fds[1], saved, flags).unwrap();
            assert_eq!(unsafe { libc::fcntl(fds[1], libc::F_GETFD) }, flags);
        }
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
            libc::close(saved);
        }
    }

    #[cfg(unix)]
    #[test]
    fn permanent_restore_failure_remains_fail_stop() {
        const CHILD: &str = "LIBDENO_RESTORE_FAILURE_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let capture = super::OutputCapture::new(true, false, None).unwrap();
            super::RESTORE_FAULT.with(|fault| {
                fault.set(Some((super::RestoreCall::Dup2, std::io::ErrorKind::Other)))
            });
            let _ = capture.finish();
            panic!("permanent fd restore failure must abort");
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "output::tests::permanent_restore_failure_remains_fail_stop",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(!status.success());
    }

    static CAPTURE_READER_BUDGET_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn reader_error_is_not_treated_as_eof() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "synthetic capture read failure",
        )))
        .unwrap();
        let overflow = std::sync::atomic::AtomicBool::new(false);
        let error = collect_blocks(&rx, &overflow).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "synthetic capture read failure");
    }

    #[test]
    fn expired_collection_deadline_with_queued_data_is_timed_out() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Ok(b"pending".to_vec())).unwrap();
        let overflow = std::sync::atomic::AtomicBool::new(false);
        let error = collect_blocks_until(&rx, &overflow, std::time::Instant::now()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("output capture incomplete"));
        assert_eq!(rx.try_recv().unwrap().unwrap(), b"pending");
    }

    #[test]
    fn empty_reader_times_out_before_eof() {
        let (tx, rx) = std::sync::mpsc::channel();
        let overflow = std::sync::atomic::AtomicBool::new(false);
        let error = collect_blocks_until(
            &rx,
            &overflow,
            std::time::Instant::now() + std::time::Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        drop(tx);
    }

    #[test]
    fn reader_error_propagates_even_when_queued() {
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "synthetic capture read failure",
        )))
        .unwrap();
        let overflow = std::sync::atomic::AtomicBool::new(false);
        let error = collect_blocks_until(
            &rx,
            &overflow,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn disconnected_reader_completes_normally() {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(tx);
        let overflow = std::sync::atomic::AtomicBool::new(false);
        let (bytes, truncated) = collect_blocks(&rx, &overflow).unwrap();
        assert!(bytes.is_empty());
        assert!(!truncated);
    }

    #[test]
    fn failed_run_keeps_captured_bytes_and_bare_failures_stay_bare() {
        let error = || crate::LibdenoError::Runtime(deno_core::anyhow::anyhow!("boom"));
        let failed = combine_capture(
            Err(error()),
            Ok((b"printed".to_vec(), b"warned".to_vec(), true)),
        )
        .unwrap_err();
        let output = failed.output.expect("captured bytes must survive");
        assert_eq!(output.stdout, b"printed");
        assert_eq!(output.stderr, b"warned");
        assert!(output.truncated);
        assert_eq!(failed.error.to_string(), "boom");

        // A plain exception with no output must not fabricate an empty buffer.
        let bare = combine_capture(Err(error()), Ok((Vec::new(), Vec::new(), false))).unwrap_err();
        assert!(bare.output.is_none());
        assert_eq!(bare.error.to_string(), "boom");
    }

    #[test]
    fn capture_error_does_not_mask_the_run_error() {
        let failed = combine_capture(
            Err(crate::LibdenoError::Timeout("deadline".into())),
            Err(std::io::Error::other("reader failed")),
        )
        .unwrap_err();
        assert!(failed.output.is_none());
        assert!(matches!(failed.error, crate::LibdenoError::Timeout(_)));
    }

    #[test]
    fn reader_budget_is_finite_and_releases_exactly() {
        let _lock = CAPTURE_READER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let baseline = active_capture_readers();
        assert!(baseline <= MAX_ACTIVE_CAPTURE_READERS);
        let available = MAX_ACTIVE_CAPTURE_READERS - baseline;
        let mut reservations = Vec::with_capacity(available);
        for _ in 0..available {
            reservations.push(reserve_capture_reader().unwrap());
        }
        assert_eq!(active_capture_readers(), MAX_ACTIVE_CAPTURE_READERS);
        let error = match reserve_capture_reader() {
            Ok(_) => panic!("reader budget must reject a new reservation at its limit"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("reader/pipe resource budget"));
        drop(reservations);
        assert_eq!(active_capture_readers(), baseline);
    }

    #[cfg(unix)]
    #[test]
    fn active_capture_preserves_closed_unselected_stdio() {
        const CHILD: &str = "LIBDENO_CLOSED_STDIO_CHILD";
        if let Some(selection) = std::env::var_os(CHILD) {
            let selection = selection.to_str().unwrap();
            let (closed, capture_stdout, target) = match selection {
                "closed0-stdout" => (0, true, 1),
                "closed1-stderr" => (1, false, 2),
                "closed2-stdout" => (2, true, 1),
                other => panic!("unknown child selection: {other}"),
            };
            eprintln!("LIBDENO_CLOSED_STDIO_CHILD_STARTED:{selection}");
            unsafe {
                libc::close(closed);
            }
            let capture = super::OutputCapture::new(capture_stdout, !capture_stdout, None).unwrap();
            assert!(unsafe { libc::fcntl(closed, libc::F_GETFD) } < 0);
            let probe = b"capture";
            assert_eq!(
                unsafe { libc::write(target, probe.as_ptr().cast(), probe.len()) },
                probe.len() as isize
            );
            let (stdout, stderr, _) = capture.finish().unwrap();
            assert_eq!(if capture_stdout { stdout } else { stderr }, probe);
            assert!(unsafe { libc::fcntl(closed, libc::F_GETFD) } < 0);
            if closed == 1 {
                std::process::exit(0);
            }
            return;
        }
        for selection in ["closed0-stdout", "closed1-stderr", "closed2-stdout"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "output::tests::active_capture_preserves_closed_unselected_stdio",
                    "--nocapture",
                ])
                .env(CHILD, selection)
                .output()
                .unwrap();
            let marker = format!("LIBDENO_CLOSED_STDIO_CHILD_STARTED:{selection}");
            let diagnostics = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                diagnostics.contains(&marker),
                "missing child marker {marker:?}: {diagnostics}"
            );
            assert!(
                output.status.success(),
                "isolated capture child failed for {selection}: {diagnostics}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn capture_restores_target_descriptor_flags_in_isolated_children() {
        const CHILD: &str = "LIBDENO_FD_FLAGS_CHILD";
        if let Some(mode) = std::env::var_os(CHILD) {
            let mode = mode.to_str().unwrap();
            let original_flags = if mode.ends_with("cloexec") {
                libc::FD_CLOEXEC
            } else {
                0
            };
            assert_eq!(unsafe { libc::fcntl(1, libc::F_SETFD, original_flags) }, 0);
            if mode.contains("eintr-dup2") {
                super::RESTORE_FAULT.with(|fault| {
                    fault.set(Some((
                        super::RestoreCall::Dup2,
                        std::io::ErrorKind::Interrupted,
                    )))
                });
            } else if mode.contains("eintr-setfd") {
                super::RESTORE_FAULT.with(|fault| {
                    fault.set(Some((
                        super::RestoreCall::SetFd,
                        std::io::ErrorKind::Interrupted,
                    )))
                });
            }
            if mode.starts_with("partial") {
                std::env::set_var("LIBDENO_TEST_FAIL_CAPTURE_FD", "2");
                assert!(super::OutputCapture::new(true, true, None).is_err());
                assert_eq!(unsafe { libc::fcntl(1, libc::F_GETFD) }, original_flags);
                return;
            }
            let capture = super::OutputCapture::new(true, false, None).unwrap();
            assert_ne!(
                unsafe { libc::fcntl(1, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            if mode.starts_with("drop") {
                drop(capture);
            } else {
                capture.finish().unwrap();
            }
            assert_eq!(unsafe { libc::fcntl(1, libc::F_GETFD) }, original_flags);
            return;
        }
        for mode in [
            "finish-zero",
            "finish-cloexec",
            "drop-zero",
            "drop-cloexec",
            "partial-zero",
            "partial-cloexec",
            "finish-eintr-dup2-cloexec",
            "drop-eintr-setfd-zero",
            "partial-eintr-dup2-cloexec",
        ] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "output::tests::capture_restores_target_descriptor_flags_in_isolated_children",
                    "--nocapture",
                ])
                .env(CHILD, mode)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{mode}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn host_buffered_output_does_not_leak_into_the_capture() {
        const CHILD: &str = "LIBDENO_STDIO_FLUSH_CHILD";
        if std::env::var(CHILD).ok().as_deref() == Some("1") {
            use std::io::Write;
            // No newline: stays in Rust's line buffer until flushed.
            print!("host-before-capture");
            let capture = super::OutputCapture::new(true, false, None).unwrap();
            print!("script-line");
            let (stdout, _, _) = capture.finish().unwrap();
            // The script's buffered bytes must be captured (flushed into the
            // pipe before the restore), and the host's pre-capture bytes must
            // not be (flushed to the real fd before the redirect).
            assert_eq!(String::from_utf8_lossy(&stdout), "script-line");
            let mut stderr = std::io::stderr();
            writeln!(stderr, "LIBDENO_STDIO_FLUSH_OK").unwrap();
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "output::tests::host_buffered_output_does_not_leak_into_the_capture",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "isolated stdio flush child failed: stdout={stdout} stderr={stderr}"
        );
        assert!(stderr.contains("LIBDENO_STDIO_FLUSH_OK"), "{stderr}");
        assert!(stdout.contains("host-before-capture"), "{stdout}");
        assert!(!stdout.contains("script-line"), "{stdout}");
    }

    #[cfg(unix)]
    #[test]
    fn reader_budget_releases_after_normal_eof() {
        use std::os::fd::AsRawFd;

        let _lock = CAPTURE_READER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let target = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .unwrap();
        let baseline = active_capture_readers();
        let capture = super::StreamCapture::new(
            target.as_raw_fd(),
            None,
            super::SavedFd::duplicate(target.as_raw_fd()).unwrap(),
        )
        .unwrap();
        assert_eq!(active_capture_readers(), baseline + 1);

        let message = b"normal eof";
        let written = unsafe {
            libc::write(
                target.as_raw_fd(),
                message.as_ptr() as *const libc::c_void,
                message.len(),
            )
        };
        assert_eq!(written, message.len() as isize);
        let (captured, truncated) = capture.finish().unwrap();
        assert_eq!(captured, message);
        assert!(!truncated);
        assert_eq!(active_capture_readers(), baseline);
    }
}
