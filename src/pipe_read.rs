//! Interruptible pipe reads for detached capture readers.
//!
//! A capture reader must be able to give up when its collector does. A plain
//! blocking `read` cannot be woken, so a descendant that inherits the pipe
//! write end and then stays silent would keep the thread (and its budget slot)
//! alive for the rest of the process. Waiting for readability in short slices
//! lets the reader observe the stop flag and exit.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long a reader waits for data before re-checking its stop flag.
const SLICE: Duration = Duration::from_millis(50);

#[cfg(unix)]
pub(crate) type WaitHandle = std::os::fd::RawFd;

#[cfg(unix)]
pub(crate) fn from_std<R: std::os::fd::AsRawFd>(reader: &R) -> WaitHandle {
    std::os::fd::AsRawFd::as_raw_fd(reader)
}

#[cfg(unix)]
pub(crate) fn from_raw_fd(fd: std::os::fd::RawFd) -> WaitHandle {
    fd
}

#[cfg(windows)]
#[derive(Clone, Copy)]
pub(crate) struct WaitHandle(std::os::windows::io::RawHandle);

// SAFETY: the handle is only an OS identifier. The pipe object it names stays
// owned by the reader thread, which receives this value at spawn time, so
// moving the identifier between threads does not move the pipe.
#[cfg(windows)]
unsafe impl Send for WaitHandle {}

#[cfg(windows)]
pub(crate) fn from_std<R: std::os::windows::io::AsRawHandle>(reader: &R) -> WaitHandle {
    WaitHandle(std::os::windows::io::AsRawHandle::as_raw_handle(reader))
}

#[cfg(windows)]
pub(crate) fn from_raw_fd(fd: libc::c_int) -> WaitHandle {
    // SAFETY: `fd` is a live CRT descriptor owned by the caller for at least
    // the lifetime of the returned handle (the reader thread owns the pipe).
    WaitHandle(unsafe { libc::get_osfhandle(fd) as std::os::windows::io::RawHandle })
}

/// Waits until `handle` is readable or `stop` is set.
///
/// `Ok(true)` means a `read` will not block (data or EOF is available).
/// `Ok(false)` means the caller asked to stop.
pub(crate) fn wait_readable(handle: WaitHandle, stop: &AtomicBool) -> io::Result<bool> {
    #[cfg(unix)]
    {
        let mut descriptor = libc::pollfd {
            fd: handle,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(false);
            }
            let ready = unsafe { libc::poll(&mut descriptor, 1, SLICE.as_millis() as i32) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if ready > 0 {
                return Ok(true);
            }
        }
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_BROKEN_PIPE};
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;

        let handle = handle.0;
        loop {
            if stop.load(Ordering::Acquire) {
                return Ok(false);
            }
            let mut available = 0u32;
            // SAFETY: the handle is a live pipe read handle; a zero-length
            // peek with null buffers only reports how many bytes are queued.
            let ok = unsafe {
                PeekNamedPipe(
                    handle,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            };
            if ok != 0 {
                if available > 0 {
                    return Ok(true);
                }
            } else {
                // SAFETY: GetLastError reads the calling thread's last error.
                let error = unsafe { GetLastError() };
                if error == ERROR_BROKEN_PIPE {
                    // All write ends are closed: read returns 0 (EOF).
                    return Ok(true);
                }
                return Err(io::Error::from_raw_os_error(error as i32));
            }
            std::thread::sleep(SLICE);
        }
    }
}
