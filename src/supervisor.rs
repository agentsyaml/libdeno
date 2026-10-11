#![cfg(feature = "execution-control")]
#![allow(dead_code)]

use std::fmt;
use std::io::{self, Read};
use std::net::TcpStream;
use std::path::PathBuf;
#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub(crate) const SUPERVISOR_MAGIC: [u8; 4] = *b"LDSV";
pub(crate) const SUPERVISOR_VERSION: u8 = 2;
pub(crate) const MAX_SUPERVISOR_FRAME_PAYLOAD: usize = 1 << 20;
pub(crate) const SUPERVISOR_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const SUPERVISOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SUPERVISOR_CANCEL_GRACE: Duration = Duration::from_secs(2);
pub(crate) const SUPERVISOR_CHILD_EXIT_GRACE: Duration = Duration::from_millis(250);
/// Default bytes retained per requested stream by the parent's bounded-pipe
/// capture reader; excess output is drained and discarded.
pub(crate) const SUPERVISOR_CAPTURE_BYTES_PER_STREAM: usize = 64 * 1024;
/// Maximum explicit per-stream retention for the parent's bounded-pipe capture.
pub(crate) const SUPERVISOR_MAX_CAPTURE_BYTES_PER_STREAM: usize = 96 * 1024;

pub(crate) const SUPERVISOR_MODE_ENV: &str = "LIBDENO_SUPERVISOR_MODE";
pub(crate) const SUPERVISOR_ENDPOINT_ENV: &str = "LIBDENO_SUPERVISOR_ENDPOINT";
pub(crate) const SUPERVISOR_TOKEN_ENV: &str = "LIBDENO_SUPERVISOR_TOKEN";

const SUPERVISOR_HEADER_LEN: usize = 4 + 1 + 1 + 8 + 4;
const SUPERVISOR_TOKEN_LEN: usize = 16;

pub(crate) fn prepare_supervisor_stream(_stream: &TcpStream) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    // MSG_DONTWAIT alone still waited for send-space on the host probe. Set
    // nonblocking once before any clone; read/write paths then poll readiness.
    _stream.set_nonblocking(true)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameDirection {
    ParentToChild,
    ChildToParent,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameKind {
    Request,
    Start,
    Cancel,
    Hello,
    Accepted,
    Started,
    Terminal,
}

impl fmt::Debug for FrameKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Request => "REQUEST",
            Self::Start => "START",
            Self::Cancel => "CANCEL",
            Self::Hello => "HELLO",
            Self::Accepted => "ACCEPTED",
            Self::Started => "STARTED",
            Self::Terminal => "TERMINAL",
        })
    }
}

impl FrameKind {
    fn to_byte(self) -> u8 {
        match self {
            Self::Request => 1,
            Self::Start => 2,
            Self::Cancel => 3,
            Self::Hello => 4,
            Self::Accepted => 5,
            Self::Started => 6,
            Self::Terminal => 7,
        }
    }

    fn from_byte(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Request,
            2 => Self::Start,
            3 => Self::Cancel,
            4 => Self::Hello,
            5 => Self::Accepted,
            6 => Self::Started,
            7 => Self::Terminal,
            _ => return None,
        })
    }

    pub(crate) fn direction(self) -> FrameDirection {
        match self {
            Self::Request | Self::Start | Self::Cancel => FrameDirection::ParentToChild,
            Self::Hello | Self::Accepted | Self::Started | Self::Terminal => {
                FrameDirection::ChildToParent
            }
        }
    }

    pub(crate) fn validate_direction(self, expected: FrameDirection) -> io::Result<()> {
        if self.direction() == expected {
            Ok(())
        } else {
            Err(invalid_data("supervisor frame has the wrong direction"))
        }
    }
}

pub(crate) struct SupervisorFrame {
    pub(crate) kind: FrameKind,
    pub(crate) request_id: u64,
    pub(crate) payload: Vec<u8>,
}

impl fmt::Debug for SupervisorFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SupervisorFrame")
            .field("kind", &self.kind)
            .field("request_id", &self.request_id)
            .field("payload_len", &self.payload.len())
            .finish()
    }
}

impl SupervisorFrame {
    pub(crate) fn new(kind: FrameKind, request_id: u64, payload: Vec<u8>) -> io::Result<Self> {
        if payload.len() > MAX_SUPERVISOR_FRAME_PAYLOAD {
            return Err(invalid_data("supervisor frame payload is too large"));
        }
        Ok(Self {
            kind,
            request_id,
            payload,
        })
    }
}

pub(crate) fn read_frame(
    stream: &mut TcpStream,
    expected_direction: FrameDirection,
    deadline: Instant,
) -> io::Result<SupervisorFrame> {
    read_frame_with_deadline(stream, expected_direction, Some(deadline), None)
}

pub(crate) fn read_frame_with_cancellation(
    stream: &mut TcpStream,
    expected_direction: FrameDirection,
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
) -> io::Result<SupervisorFrame> {
    read_frame_with_deadline(stream, expected_direction, deadline, cancellation)
}

/// Reads a frame whose first byte is governed by `first_byte_deadline`. Once
/// that byte arrives, the rest of the frame gets one absolute assembly
/// deadline; a slow peer cannot extend it one byte at a time.
pub(crate) fn read_frame_after_first_byte(
    stream: &mut TcpStream,
    expected_direction: FrameDirection,
    first_byte_deadline: Option<Instant>,
    assembly_deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
) -> io::Result<SupervisorFrame> {
    let mut magic = [0u8; 4];
    read_until(stream, &mut magic[..1], first_byte_deadline, cancellation)?;
    let frame_started = Instant::now();
    let assembly_deadline = assembly_deadline.unwrap_or_else(|| {
        frame_started
            .checked_add(SUPERVISOR_FRAME_TIMEOUT)
            .unwrap_or(frame_started)
    });
    read_until(
        stream,
        &mut magic[1..],
        Some(assembly_deadline),
        cancellation,
    )?;
    read_frame_with_magic(
        stream,
        expected_direction,
        magic,
        Some(assembly_deadline),
        cancellation,
    )
}

fn read_frame_with_deadline(
    stream: &mut TcpStream,
    expected_direction: FrameDirection,
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
) -> io::Result<SupervisorFrame> {
    let mut magic = [0u8; 4];
    read_until(stream, &mut magic, deadline, cancellation)?;
    read_frame_with_magic(stream, expected_direction, magic, deadline, cancellation)
}

fn read_frame_with_magic(
    stream: &mut TcpStream,
    expected_direction: FrameDirection,
    magic: [u8; 4],
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
) -> io::Result<SupervisorFrame> {
    if magic != SUPERVISOR_MAGIC {
        return Err(invalid_data("invalid supervisor frame magic"));
    }

    let mut version = [0u8; 1];
    read_until(stream, &mut version, deadline, cancellation)?;
    if version[0] != SUPERVISOR_VERSION {
        return Err(invalid_data("unsupported supervisor frame version"));
    }

    let mut kind_byte = [0u8; 1];
    read_until(stream, &mut kind_byte, deadline, cancellation)?;
    let kind = FrameKind::from_byte(kind_byte[0])
        .ok_or_else(|| invalid_data("unknown supervisor frame kind"))?;
    kind.validate_direction(expected_direction)?;

    let mut request_id = [0u8; 8];
    read_until(stream, &mut request_id, deadline, cancellation)?;
    let request_id = u64::from_be_bytes(request_id);

    let mut payload_len = [0u8; 4];
    read_until(stream, &mut payload_len, deadline, cancellation)?;
    let payload_len = u32::from_be_bytes(payload_len);
    if payload_len > MAX_SUPERVISOR_FRAME_PAYLOAD as u32 {
        return Err(invalid_data("supervisor frame payload is too large"));
    }

    let mut payload = vec![0u8; payload_len as usize];
    read_until(stream, &mut payload, deadline, cancellation)?;
    Ok(SupervisorFrame {
        kind,
        request_id,
        payload,
    })
}

pub(crate) fn write_frame(stream: &mut TcpStream, frame: &SupervisorFrame) -> io::Result<()> {
    let deadline = Instant::now()
        .checked_add(SUPERVISOR_FRAME_TIMEOUT)
        .unwrap_or_else(Instant::now);
    write_frame_until(stream, frame, Some(deadline), None)
}

pub(crate) fn write_frame_until(
    stream: &mut TcpStream,
    frame: &SupervisorFrame,
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
) -> io::Result<()> {
    if frame.payload.len() > MAX_SUPERVISOR_FRAME_PAYLOAD {
        return Err(invalid_data("supervisor frame payload is too large"));
    }

    let header = encode_header(frame)?;
    let result = write_until(stream, &header, deadline, cancellation, false)
        .and_then(|()| write_until(stream, &frame.payload, deadline, cancellation, true));
    if result.is_err() {
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    result
}

fn encode_header(frame: &SupervisorFrame) -> io::Result<[u8; SUPERVISOR_HEADER_LEN]> {
    let payload_len = u32::try_from(frame.payload.len())
        .map_err(|_| invalid_data("supervisor frame payload length does not fit"))?;
    let mut header = [0u8; SUPERVISOR_HEADER_LEN];
    header[..4].copy_from_slice(&SUPERVISOR_MAGIC);
    header[4] = SUPERVISOR_VERSION;
    header[5] = frame.kind.to_byte();
    header[6..14].copy_from_slice(&frame.request_id.to_be_bytes());
    header[14..18].copy_from_slice(&payload_len.to_be_bytes());
    Ok(header)
}

fn check_write_conditions(
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
) -> io::Result<()> {
    if cancellation.is_some_and(SupervisorCancellation::is_requested) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "supervisor cancellation requested",
        ));
    }
    if deadline.is_some_and(|at| at <= frame_write_now()) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "supervisor frame deadline exceeded",
        ));
    }
    Ok(())
}

#[cfg(test)]
const NO_FRAME_WRITE_CLOCK_ADVANCE: u64 = u64::MAX;

#[cfg(test)]
struct FrameWriteTestClock {
    origin: Instant,
    elapsed_ms: AtomicU64,
    advance_after_next_payload_send_to_ms: AtomicU64,
}

#[cfg(test)]
impl FrameWriteTestClock {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            elapsed_ms: AtomicU64::new(0),
            advance_after_next_payload_send_to_ms: AtomicU64::new(NO_FRAME_WRITE_CLOCK_ADVANCE),
        }
    }

    fn now(&self) -> Instant {
        self.origin + Duration::from_millis(self.elapsed_ms.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
thread_local! {
    static FRAME_WRITE_TEST_CLOCK: std::cell::RefCell<Option<Arc<FrameWriteTestClock>>> = const { std::cell::RefCell::new(None) };
    static FRAME_WRITE_TEST_WOULDBLOCK_GATE: std::cell::RefCell<Option<std::sync::mpsc::Receiver<()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn frame_write_now() -> Instant {
    FRAME_WRITE_TEST_CLOCK.with(|clock| {
        clock
            .borrow()
            .as_ref()
            .map_or_else(Instant::now, |clock| clock.now())
    })
}

#[cfg(not(test))]
fn frame_write_now() -> Instant {
    Instant::now()
}

#[cfg(test)]
#[derive(Debug, Clone, Copy)]
enum FrameWriteTestEvent {
    PayloadSend {
        offset: usize,
        count: usize,
        at: Instant,
    },
    WouldBlock {
        offset: usize,
        at: Instant,
    },
}

#[cfg(test)]
thread_local! {
    static FRAME_WRITE_TEST_EVENTS: std::cell::RefCell<Option<Vec<FrameWriteTestEvent>>> = const { std::cell::RefCell::new(None) };
    static FRAME_WRITE_TEST_LIVE_SENDER: std::cell::RefCell<Option<std::sync::mpsc::Sender<FrameWriteTestEvent>>> = const { std::cell::RefCell::new(None) };
    static FRAME_WRITE_TEST_FINAL_PAUSE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn observe_frame_write(event: FrameWriteTestEvent) {
    FRAME_WRITE_TEST_EVENTS.with(|events| {
        if let Some(events) = events.borrow_mut().as_mut() {
            events.push(event);
        }
    });
    FRAME_WRITE_TEST_LIVE_SENDER.with(|sender| {
        if let Some(sender) = sender.borrow().as_ref() {
            let _ = sender.send(event);
        }
    });
    if matches!(event, FrameWriteTestEvent::PayloadSend { .. }) {
        FRAME_WRITE_TEST_CLOCK.with(|clock| {
            if let Some(clock) = clock.borrow().as_ref() {
                let advance = clock
                    .advance_after_next_payload_send_to_ms
                    .swap(NO_FRAME_WRITE_CLOCK_ADVANCE, Ordering::SeqCst);
                if advance != NO_FRAME_WRITE_CLOCK_ADVANCE {
                    clock.elapsed_ms.store(advance, Ordering::SeqCst);
                }
            }
        });
    }
    if matches!(event, FrameWriteTestEvent::WouldBlock { .. }) {
        FRAME_WRITE_TEST_WOULDBLOCK_GATE.with(|gate| {
            if let Some(gate) = gate.borrow().as_ref() {
                let _ = gate.recv_timeout(Duration::from_secs(5));
            }
        });
    }
}

#[cfg(test)]
fn pause_before_final_write_check() {
    FRAME_WRITE_TEST_FINAL_PAUSE.with(|pause| {
        if let Some(pause) = pause.borrow_mut().take() {
            pause();
        }
    });
}

#[cfg(unix)]
fn write_until(
    stream: &mut TcpStream,
    bytes: &[u8],
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
    payload: bool,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    #[cfg(not(test))]
    let _ = payload;

    #[cfg(target_vendor = "apple")]
    {
        let enabled: libc::c_int = 1;
        let result = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&enabled as *const libc::c_int).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let mut offset = 0;
    while offset < bytes.len() {
        check_write_conditions(deadline, cancellation)?;
        let flags = libc::MSG_DONTWAIT;
        #[cfg(not(target_vendor = "apple"))]
        let flags = flags | libc::MSG_NOSIGNAL;
        let sent = unsafe {
            libc::send(
                stream.as_raw_fd(),
                bytes[offset..].as_ptr().cast(),
                bytes.len() - offset,
                flags,
            )
        };
        if sent > 0 {
            let sent = sent as usize;
            #[cfg(test)]
            if payload {
                observe_frame_write(FrameWriteTestEvent::PayloadSend {
                    offset,
                    count: sent,
                    at: frame_write_now(),
                });
            }
            offset += sent;
            check_write_conditions(deadline, cancellation)?;
            continue;
        }
        if sent == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write supervisor frame",
            ));
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() == io::ErrorKind::WouldBlock {
            #[cfg(test)]
            if payload {
                observe_frame_write(FrameWriteTestEvent::WouldBlock {
                    offset,
                    at: frame_write_now(),
                });
            }
            poll_writable(stream, deadline, cancellation)?;
            continue;
        }
        return Err(error);
    }
    #[cfg(test)]
    if payload {
        pause_before_final_write_check();
    }
    check_write_conditions(deadline, cancellation)
}

#[cfg(unix)]
fn poll_writable(
    stream: &TcpStream,
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    loop {
        check_write_conditions(deadline, cancellation)?;
        let remaining = deadline.map(|at| at.saturating_duration_since(frame_write_now()));
        let wait = match (remaining, cancellation.is_some()) {
            (Some(remaining), true) => Some(remaining.min(Duration::from_millis(10))),
            (Some(remaining), false) => Some(remaining),
            (None, true) => Some(Duration::from_millis(10)),
            (None, false) => None,
        };
        let timeout_ms = wait.map_or(-1, duration_to_poll_timeout_ms);
        let mut descriptor = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLOUT,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            continue;
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        if descriptor.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            let mut socket_error: libc::c_int = 0;
            let mut length = std::mem::size_of_val(&socket_error) as libc::socklen_t;
            let result = unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut socket_error as *mut libc::c_int).cast(),
                    &mut length,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            return Err(if socket_error != 0 {
                io::Error::from_raw_os_error(socket_error)
            } else {
                io::Error::new(io::ErrorKind::BrokenPipe, "supervisor peer closed")
            });
        }
        if descriptor.revents & libc::POLLOUT != 0 {
            return Ok(());
        }
    }
}

#[cfg(unix)]
fn duration_to_poll_timeout_ms(duration: Duration) -> libc::c_int {
    let round_up = u128::from(!duration.subsec_nanos().is_multiple_of(1_000_000));
    let millis = duration.as_millis().saturating_add(round_up);
    millis.min(libc::c_int::MAX as u128) as libc::c_int
}

#[cfg(windows)]
fn write_until(
    stream: &mut TcpStream,
    bytes: &[u8],
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
    payload: bool,
) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    #[cfg(not(test))]
    let _ = payload;
    use windows_sys::Win32::Foundation::{GetLastError, HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
    use windows_sys::Win32::Networking::WinSock::{
        WSACloseEvent, WSACreateEvent, WSAGetLastError, WSAGetOverlappedResult, WSASend, WSABUF,
        WSAEINTR, WSAEVENT, WSAEWOULDBLOCK, WSA_INVALID_EVENT, WSA_IO_PENDING,
    };
    use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};
    use windows_sys::Win32::System::IO::{CancelIoEx, OVERLAPPED};

    let socket = stream.as_raw_socket() as usize;
    let mut offset = 0;
    while offset < bytes.len() {
        check_write_conditions(deadline, cancellation)?;
        let event: WSAEVENT = unsafe { WSACreateEvent() };
        if event == WSA_INVALID_EVENT {
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        let event_handle = event as usize as HANDLE;
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        overlapped.hEvent = event_handle;
        let remaining = bytes.len() - offset;
        let mut buffer = WSABUF {
            len: remaining.min(u32::MAX as usize) as u32,
            buf: bytes[offset..].as_ptr() as *mut u8,
        };
        // TcpStream's Windows sockets are created overlapped by std. Keep both
        // the borrowed byte slice and this OVERLAPPED alive until its event is
        // signaled and WSAGetOverlappedResult has harvested completion.
        let result = unsafe {
            WSASend(
                socket,
                &mut buffer,
                1,
                std::ptr::null_mut(),
                0,
                &mut overlapped,
                None,
            )
        };
        if result == -1 {
            let error = unsafe { WSAGetLastError() };
            if error == WSA_IO_PENDING {
                #[cfg(test)]
                if payload {
                    observe_frame_write(FrameWriteTestEvent::WouldBlock {
                        offset,
                        at: frame_write_now(),
                    });
                }
            } else {
                unsafe { WSACloseEvent(event) };
                if error == WSAEINTR {
                    continue;
                }
                if error == WSAEWOULDBLOCK {
                    #[cfg(test)]
                    if payload {
                        observe_frame_write(FrameWriteTestEvent::WouldBlock {
                            offset,
                            at: frame_write_now(),
                        });
                    }
                    poll_socket_writable(socket, deadline, cancellation)?;
                    continue;
                }
                return Err(io::Error::from_raw_os_error(error));
            }
        }

        let mut cancelled = None;
        let transferred = loop {
            if let Err(error) = check_write_conditions(deadline, cancellation) {
                cancelled = Some(error);
                cancel_and_reap_socket_write(socket, event_handle, &mut overlapped);
                break None;
            }
            let timeout = match (
                deadline.map(|at| at.saturating_duration_since(frame_write_now())),
                cancellation.is_some(),
            ) {
                (Some(remaining), true) => Some(remaining.min(Duration::from_millis(10))),
                (Some(remaining), false) => Some(remaining),
                (None, true) => Some(Duration::from_millis(10)),
                (None, false) => None,
            };
            let timeout_ms = timeout.map_or(INFINITE, duration_to_win_timeout_ms);
            let wait_result = unsafe { WaitForSingleObject(event_handle, timeout_ms) };
            if wait_result == WAIT_OBJECT_0 {
                let mut transferred = 0;
                let mut flags = 0;
                let completed = unsafe {
                    WSAGetOverlappedResult(socket, &overlapped, &mut transferred, 0, &mut flags)
                };
                if completed == 0 {
                    let error = unsafe { WSAGetLastError() };
                    cancelled = Some(io::Error::from_raw_os_error(error));
                    break None;
                }
                break Some(transferred as usize);
            }
            if wait_result == WAIT_FAILED {
                let error = io::Error::from_raw_os_error(unsafe { GetLastError() } as i32);
                cancel_and_reap_socket_write(socket, event_handle, &mut overlapped);
                cancelled = Some(error);
                break None;
            }
            if wait_result != windows_sys::Win32::Foundation::WAIT_TIMEOUT {
                let error = io::Error::other("unexpected overlapped send wait result");
                cancel_and_reap_socket_write(socket, event_handle, &mut overlapped);
                cancelled = Some(error);
                break None;
            }
        };
        unsafe { WSACloseEvent(event) };
        if let Some(error) = cancelled {
            return Err(error);
        }
        let transferred = transferred.expect("completed overlapped send has a byte count");
        if transferred == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "failed to write supervisor frame",
            ));
        }
        offset += transferred;
        #[cfg(test)]
        if payload {
            observe_frame_write(FrameWriteTestEvent::PayloadSend {
                offset: offset - transferred,
                count: transferred,
                at: frame_write_now(),
            });
        }
        check_write_conditions(deadline, cancellation)?;
    }
    #[cfg(test)]
    if payload {
        pause_before_final_write_check();
    }
    check_write_conditions(deadline, cancellation)
}

#[cfg(windows)]
fn cancel_and_reap_socket_write(
    socket: usize,
    event_handle: windows_sys::Win32::Foundation::HANDLE,
    overlapped: &mut windows_sys::Win32::System::IO::OVERLAPPED,
) {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
    use windows_sys::Win32::Networking::WinSock::WSAGetOverlappedResult;
    use windows_sys::Win32::System::Threading::{WaitForSingleObject, INFINITE};
    use windows_sys::Win32::System::IO::CancelIoEx;
    let handle = socket as *mut std::ffi::c_void;
    unsafe {
        let cancelled = CancelIoEx(handle, overlapped);
        if cancelled == 0 {
            let error = GetLastError();
            if error != ERROR_NOT_FOUND {
                // Cancellation failure cannot permit returning while the
                // operation may still borrow the frame buffer. Keep waiting
                // for completion; this is safety-bounded, not hard real-time.
            }
        }
        // ERROR_NOT_FOUND means completion may have won the cancellation race;
        // either way, wait for and harvest the operation before locals expire.
        if WaitForSingleObject(event_handle, INFINITE)
            != windows_sys::Win32::Foundation::WAIT_OBJECT_0
        {
            // Returning would invalidate OVERLAPPED and its borrowed buffer while
            // Winsock could still own them. This can only indicate a corrupted
            // event handle; fail-stop rather than permit kernel use-after-return.
            std::process::abort();
        }
        let mut transferred = 0;
        let mut flags = 0;
        let _ = WSAGetOverlappedResult(socket, overlapped, &mut transferred, 0, &mut flags);
    }
}

#[cfg(windows)]
fn poll_socket_writable(
    socket: usize,
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
) -> io::Result<()> {
    use windows_sys::Win32::Networking::WinSock::{
        WSAPoll, POLLERR, POLLHUP, POLLNVAL, POLLWRNORM, WSAPOLLFD,
    };

    loop {
        check_write_conditions(deadline, cancellation)?;
        let remaining = deadline.map(|at| at.saturating_duration_since(frame_write_now()));
        let timeout = match (remaining, cancellation.is_some()) {
            (Some(remaining), true) => Some(remaining.min(Duration::from_millis(10))),
            (Some(remaining), false) => Some(remaining),
            (None, true) => Some(Duration::from_millis(10)),
            (None, false) => None,
        };
        let millis = timeout.map_or(-1, duration_to_poll_timeout_ms);
        let mut descriptor = WSAPOLLFD {
            fd: socket,
            events: POLLWRNORM,
            revents: 0,
        };
        let result = unsafe { WSAPoll(&mut descriptor, 1, millis) };
        if result == -1 {
            let error = unsafe { windows_sys::Win32::Networking::WinSock::WSAGetLastError() };
            if error == windows_sys::Win32::Networking::WinSock::WSAEINTR {
                continue;
            }
            return Err(io::Error::from_raw_os_error(error));
        }
        if result == 0 {
            continue;
        }
        if descriptor.revents & POLLNVAL != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid supervisor socket",
            ));
        }
        if descriptor.revents & (POLLERR | POLLHUP) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "supervisor peer closed",
            ));
        }
        if descriptor.revents & POLLWRNORM != 0 {
            return Ok(());
        }
    }
}

#[cfg(windows)]
fn duration_to_win_timeout_ms(duration: Duration) -> u32 {
    let millis = duration.as_millis();
    let round_up = u128::from((duration.subsec_nanos() % 1_000_000 != 0) as u8);
    let rounded = millis.saturating_add(round_up);
    rounded.min((u32::MAX - 1) as u128) as u32
}

#[cfg(windows)]
fn duration_to_poll_timeout_ms(duration: Duration) -> i32 {
    let millis = duration.as_millis();
    let round_up = u128::from((duration.subsec_nanos() % 1_000_000 != 0) as u8);
    let rounded = millis.saturating_add(round_up);
    rounded.min(i32::MAX as u128) as i32
}

#[cfg(not(any(unix, windows)))]
fn write_until(
    stream: &mut TcpStream,
    bytes: &[u8],
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
    _payload: bool,
) -> io::Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        check_write_conditions(deadline, cancellation)?;
        match stream.write(&bytes[offset..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write supervisor frame",
                ))
            }
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        check_write_conditions(deadline, cancellation)?;
    }
    check_write_conditions(deadline, cancellation)
}

#[cfg(not(any(unix, windows)))]
use std::io::Write;

fn read_until(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
) -> io::Result<()> {
    let mut offset = 0;
    while offset < buffer.len() {
        if cancellation.is_some_and(|cancellation| cancellation.is_requested()) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "supervisor cancellation requested",
            ));
        }
        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|remaining| remaining.is_zero()) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "supervisor frame deadline exceeded",
            ));
        }
        // Set the timeout for each syscall. `read_exact` would keep one socket
        // timeout across its internal reads, allowing a peer to reset the
        // effective deadline by sending one byte at a time.
        #[cfg(not(target_vendor = "apple"))]
        {
            let timeout = match (remaining, cancellation.is_some()) {
                (Some(remaining), true) => Some(remaining.min(Duration::from_millis(10))),
                (Some(remaining), false) => Some(remaining),
                (None, true) => Some(Duration::from_millis(10)),
                (None, false) => None,
            };
            stream.set_read_timeout(timeout)?;
        }
        match stream.read(&mut buffer[offset..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "truncated supervisor frame",
                ))
            }
            Ok(count) => offset += count,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                #[cfg(target_vendor = "apple")]
                poll_readable(stream, deadline, cancellation)?;
                #[cfg(not(target_vendor = "apple"))]
                {
                    if cancellation.is_some_and(|cancellation| cancellation.is_requested()) {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "supervisor cancellation requested",
                        ));
                    }
                    if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "supervisor frame deadline exceeded",
                        ));
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(target_vendor = "apple")]
pub(crate) fn poll_readable(
    stream: &TcpStream,
    deadline: Option<Instant>,
    cancellation: Option<&SupervisorCancellation>,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    loop {
        if cancellation.is_some_and(SupervisorCancellation::is_requested) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "supervisor cancellation requested",
            ));
        }
        let remaining = deadline.map(|at| at.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|remaining| remaining.is_zero()) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "supervisor frame deadline exceeded",
            ));
        }
        let wait = match (remaining, cancellation.is_some()) {
            (Some(remaining), true) => Some(remaining.min(Duration::from_millis(10))),
            (Some(remaining), false) => Some(remaining),
            (None, true) => Some(Duration::from_millis(10)),
            (None, false) => None,
        };
        let timeout = wait.map_or(-1, duration_to_poll_timeout_ms);
        let mut descriptor = libc::pollfd {
            fd: stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, timeout) };
        if ready < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready == 0 {
            continue;
        }
        if descriptor.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::from_raw_os_error(libc::EBADF));
        }
        if descriptor.revents & libc::POLLERR != 0 {
            let mut socket_error: libc::c_int = 0;
            let mut length = std::mem::size_of_val(&socket_error) as libc::socklen_t;
            let result = unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut socket_error as *mut libc::c_int).cast(),
                    &mut length,
                )
            };
            if result != 0 {
                return Err(io::Error::last_os_error());
            }
            if socket_error != 0 {
                return Err(io::Error::from_raw_os_error(socket_error));
            }
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "supervisor read poll failed",
            ));
        }
        if descriptor.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            return Ok(());
        }
    }
}

pub(crate) struct SupervisorToken([u8; SUPERVISOR_TOKEN_LEN]);

impl fmt::Debug for SupervisorToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SupervisorToken(REDACTED)")
    }
}

impl SupervisorToken {
    pub(crate) fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; SUPERVISOR_TOKEN_LEN];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    pub(crate) fn authenticate(&self, candidate: &[u8]) -> bool {
        let mut difference = (candidate.len() != self.0.len()) as u8;
        for index in 0..self.0.len() {
            difference |= self.0[index] ^ candidate.get(index).copied().unwrap_or_default();
        }
        difference == 0
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub(crate) fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    pub(crate) fn from_hex(value: &str) -> io::Result<Self> {
        if value.len() != SUPERVISOR_TOKEN_LEN * 2 {
            return Err(invalid_data("invalid supervisor token"));
        }
        let mut bytes = [0u8; SUPERVISOR_TOKEN_LEN];
        for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            bytes[index] = (hex_digit(pair[0])? << 4) | hex_digit(pair[1])?;
        }
        Ok(Self(bytes))
    }
}

fn hex_digit(value: u8) -> io::Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(invalid_data("invalid supervisor token")),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SupervisorRequest {
    pub(crate) entry: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) permissions: Vec<String>,
    pub(crate) allow_all_permissions: bool,
    pub(crate) prompt: bool,
    pub(crate) args: Vec<String>,
    pub(crate) features: Option<Vec<String>>,
    pub(crate) max_heap_bytes: Option<usize>,
    pub(crate) execution_deadline: Option<Duration>,
    pub(crate) capture_stdout: bool,
    pub(crate) capture_stderr: bool,
    pub(crate) max_capture_bytes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SupervisorTerminal {
    pub(crate) outcome: SupervisorOutcome,
    pub(crate) exit_code: Option<i32>,
    #[serde(default)]
    pub(crate) category: Option<SupervisorFailureCategory>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SupervisorOutcome {
    Completed,
    Failed,
    Cancelled,
    Deadline,
}

/// Closed supervisor failure categories. The enum is shared by the terminal
/// wire payload and the hidden execution-control error surface; no child
/// error text crosses the supervisor boundary.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorFailureCategory {
    Permission,
    Timeout,
    Runtime,
    Infrastructure,
    ChildCrash,
}

impl SupervisorFailureCategory {
    pub(crate) const fn summary(self) -> &'static str {
        match self {
            Self::Permission => "supervisor child permission failure",
            Self::Timeout => "supervisor session timed out",
            Self::Runtime => "supervisor child runtime failure",
            Self::Infrastructure => "supervisor infrastructure failure",
            Self::ChildCrash => "supervisor child exited without a terminal result",
        }
    }
}

impl fmt::Display for SupervisorFailureCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str((*self).summary())
    }
}

/// Validates the part of a TERMINAL payload that is independent of the
/// request's capture settings.
pub(crate) fn validate_supervisor_terminal_shape(terminal: &SupervisorTerminal) -> io::Result<()> {
    let valid = match terminal.outcome {
        SupervisorOutcome::Completed => terminal.exit_code.is_some() && terminal.category.is_none(),
        SupervisorOutcome::Failed => {
            terminal.exit_code.is_none()
                && matches!(
                    terminal.category,
                    Some(
                        SupervisorFailureCategory::Permission
                            | SupervisorFailureCategory::Runtime
                            | SupervisorFailureCategory::Infrastructure,
                    )
                )
        }
        SupervisorOutcome::Cancelled | SupervisorOutcome::Deadline => {
            terminal.exit_code.is_none() && terminal.category.is_none()
        }
    };
    if !valid {
        Err(invalid_data("invalid supervisor TERMINAL result fields"))
    } else if !terminal.stdout.is_empty() || !terminal.stderr.is_empty() || terminal.truncated {
        Err(invalid_data("supervisor v2 TERMINAL must not carry output"))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CancelReason {
    User,
    Deadline,
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SupervisorTransportStatus {
    Clean,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CleanupStrength {
    DirectChild,
    ProcessGroup,
    WindowsJob,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SupervisorParentState {
    AwaitHello,
    AwaitAccepted,
    Accepted,
    AwaitStarted,
    Started,
    CancellingBeforeAccepted,
    CancellingBeforeStart,
    Cancelling,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SupervisorChildState {
    AwaitRequest,
    AwaitStart,
    Started,
    Cancelling,
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SupervisorFrameEvent {
    Accepted,
    Started,
    Terminal,
}

/// Parent-side protocol transitions. The transport code deliberately keeps
/// lifecycle validation separate from process cleanup so a future executor
/// lane can reuse the same fail-closed rules.
#[derive(Debug)]
pub(crate) struct SupervisorParentSession {
    request_id: u64,
    state: SupervisorParentState,
    request_sent: bool,
    cancel_sent: bool,
    started_received: bool,
    cancel_reason: Option<CancelReason>,
    terminal: Option<SupervisorTerminal>,
}

impl SupervisorParentSession {
    pub(crate) fn new(request_id: u64) -> Self {
        Self {
            request_id,
            state: SupervisorParentState::AwaitHello,
            request_sent: false,
            cancel_sent: false,
            started_received: false,
            cancel_reason: None,
            terminal: None,
        }
    }

    pub(crate) fn state(&self) -> SupervisorParentState {
        self.state
    }

    pub(crate) fn started_received(&self) -> bool {
        self.started_received
    }

    pub(crate) fn terminal(&self) -> Option<&SupervisorTerminal> {
        self.terminal.as_ref()
    }

    /// Returns the cancellation that linearized before the stored TERMINAL.
    /// A cancellation attempted after TERMINAL is deliberately not recorded
    /// by `send_cancel`, so a terminal that wins the state lock remains the
    /// authoritative result.
    pub(crate) fn cancellation_before_terminal(&self) -> Option<CancelReason> {
        self.terminal.as_ref().and(self.cancel_reason)
    }

    pub(crate) fn accept_hello(
        &mut self,
        frame: &SupervisorFrame,
        token: &SupervisorToken,
    ) -> io::Result<()> {
        if self.state != SupervisorParentState::AwaitHello
            || frame.kind != FrameKind::Hello
            || frame.request_id != 0
            || !token.authenticate(&frame.payload)
        {
            return Err(invalid_data("invalid supervisor HELLO"));
        }
        self.state = SupervisorParentState::AwaitAccepted;
        Ok(())
    }

    pub(crate) fn send_request(&mut self) -> io::Result<()> {
        if self.state == SupervisorParentState::AwaitAccepted && !self.request_sent {
            self.request_sent = true;
            Ok(())
        } else {
            Err(invalid_data("supervisor REQUEST is out of order"))
        }
    }

    pub(crate) fn send_start(&mut self) -> io::Result<()> {
        if self.state != SupervisorParentState::Accepted {
            return Err(invalid_data("supervisor START is out of order"));
        }
        self.state = SupervisorParentState::AwaitStarted;
        Ok(())
    }

    pub(crate) fn send_cancel(&mut self, reason: CancelReason) -> io::Result<bool> {
        if let Some(previous) = self.cancel_reason {
            if previous != reason {
                return Err(invalid_data("conflicting supervisor cancellation reason"));
            }
            if self.state == SupervisorParentState::Terminal || self.cancel_sent {
                return Ok(false);
            }
            self.cancel_sent = true;
            return Ok(true);
        }
        match self.state {
            SupervisorParentState::AwaitAccepted => {
                self.state = SupervisorParentState::CancellingBeforeAccepted;
            }
            SupervisorParentState::Accepted => {
                self.state = SupervisorParentState::CancellingBeforeStart;
            }
            SupervisorParentState::AwaitStarted | SupervisorParentState::Started => {
                self.state = SupervisorParentState::Cancelling;
            }
            SupervisorParentState::CancellingBeforeAccepted
            | SupervisorParentState::CancellingBeforeStart
            | SupervisorParentState::Cancelling => {}
            SupervisorParentState::Terminal => {
                // A late duplicate cancellation cannot change a terminal
                // result and is therefore idempotent, like a duplicate
                // terminal frame.
                return Ok(false);
            }
            SupervisorParentState::AwaitHello => {
                return Err(invalid_data("supervisor CANCEL is out of order"));
            }
        }
        self.cancel_reason = Some(reason);
        self.cancel_sent = true;
        Ok(true)
    }

    /// Atomically chooses cancellation over START while the parent still has
    /// a plain `ACCEPTED` state. Once this returns `true`, START authorization
    /// has linearized and a later cancellation is post-start.
    pub(crate) fn authorize_start(
        &mut self,
        cancellation: &SupervisorCancellation,
    ) -> io::Result<bool> {
        if self.state != SupervisorParentState::Accepted {
            return Err(invalid_data("supervisor START is out of order"));
        }
        if let Some(reason) = cancellation.requested_reason() {
            self.state = SupervisorParentState::CancellingBeforeStart;
            self.cancel_reason = Some(reason);
            return Ok(false);
        }
        self.state = SupervisorParentState::AwaitStarted;
        Ok(true)
    }

    pub(crate) fn receive_child_frame(
        &mut self,
        frame: &SupervisorFrame,
    ) -> io::Result<SupervisorFrameEvent> {
        if frame.request_id != self.request_id
            && !(frame.kind == FrameKind::Hello && frame.request_id == 0)
        {
            return Err(invalid_data("supervisor request ID mismatch"));
        }
        match frame.kind {
            FrameKind::Accepted if frame.payload.is_empty() => match self.state {
                SupervisorParentState::AwaitAccepted => {
                    self.state = SupervisorParentState::Accepted;
                    Ok(SupervisorFrameEvent::Accepted)
                }
                SupervisorParentState::CancellingBeforeAccepted => {
                    self.state = SupervisorParentState::CancellingBeforeStart;
                    Ok(SupervisorFrameEvent::Accepted)
                }
                _ => Err(invalid_data(
                    "duplicate or out-of-order supervisor ACCEPTED",
                )),
            },
            FrameKind::Started if frame.payload.is_empty() => match self.state {
                SupervisorParentState::AwaitStarted => {
                    self.started_received = true;
                    self.state = SupervisorParentState::Started;
                    Ok(SupervisorFrameEvent::Started)
                }
                // Cancellation after START authorization is post-start even
                // if STARTED is still in flight. Pre-START cancellation is
                // fail-closed and can never accept this frame.
                SupervisorParentState::Cancelling if !self.started_received => {
                    self.started_received = true;
                    Ok(SupervisorFrameEvent::Started)
                }
                _ => Err(invalid_data("duplicate or out-of-order supervisor STARTED")),
            },
            FrameKind::Terminal => {
                let terminal: SupervisorTerminal = decode_payload(&frame.payload)?;
                validate_supervisor_terminal_shape(&terminal)?;
                if let Some(previous) = &self.terminal {
                    if previous == &terminal {
                        return Ok(SupervisorFrameEvent::Terminal);
                    }
                    return Err(invalid_data("conflicting supervisor TERMINAL frames"));
                }
                if self.state == SupervisorParentState::CancellingBeforeStart {
                    let expected = match self.cancel_reason {
                        Some(CancelReason::Deadline) => SupervisorOutcome::Deadline,
                        Some(CancelReason::User | CancelReason::Shutdown) => {
                            SupervisorOutcome::Cancelled
                        }
                        None => {
                            return Err(invalid_data(
                                "pre-start supervisor cancellation has no reason",
                            ))
                        }
                    };
                    if terminal.outcome != expected {
                        return Err(invalid_data(
                            "pre-start supervisor cancellation has an invalid outcome",
                        ));
                    }
                }
                if !matches!(
                    self.state,
                    SupervisorParentState::Started
                        | SupervisorParentState::CancellingBeforeStart
                        | SupervisorParentState::Cancelling
                ) {
                    return Err(invalid_data("supervisor TERMINAL is out of order"));
                }
                self.terminal = Some(terminal);
                self.state = SupervisorParentState::Terminal;
                Ok(SupervisorFrameEvent::Terminal)
            }
            _ => Err(invalid_data("invalid supervisor child frame")),
        }
    }
}

/// Child-side protocol transitions. The child cannot enter `Started` until a
/// valid START frame has arrived, which is the user-code barrier.
#[derive(Debug)]
pub(crate) struct SupervisorChildSession {
    request_id: Option<u64>,
    state: SupervisorChildState,
    cancel_reason: Option<CancelReason>,
    terminal: Option<SupervisorTerminal>,
}

impl SupervisorChildSession {
    pub(crate) fn new() -> Self {
        Self {
            request_id: None,
            state: SupervisorChildState::AwaitRequest,
            cancel_reason: None,
            terminal: None,
        }
    }

    pub(crate) fn state(&self) -> SupervisorChildState {
        self.state
    }

    pub(crate) fn request_id(&self) -> Option<u64> {
        self.request_id
    }

    pub(crate) fn receive_parent_frame(
        &mut self,
        frame: &SupervisorFrame,
    ) -> io::Result<Option<SupervisorFrameEvent>> {
        if let Some(request_id) = self.request_id {
            if frame.request_id != request_id {
                return Err(invalid_data("supervisor request ID mismatch"));
            }
        }
        match frame.kind {
            FrameKind::Request if self.state == SupervisorChildState::AwaitRequest => {
                if frame.request_id == 0 {
                    return Err(invalid_data("invalid supervisor request ID"));
                }
                self.request_id = Some(frame.request_id);
                self.state = SupervisorChildState::AwaitStart;
                Ok(None)
            }
            FrameKind::Start
                if self.state == SupervisorChildState::AwaitStart && frame.payload.is_empty() =>
            {
                self.state = SupervisorChildState::Started;
                Ok(Some(SupervisorFrameEvent::Started))
            }
            FrameKind::Cancel => {
                let reason: CancelReason = decode_payload(&frame.payload)?;
                if let Some(previous) = self.cancel_reason {
                    if previous != reason {
                        return Err(invalid_data("conflicting supervisor cancellation reason"));
                    }
                    return Ok(None);
                }
                if !matches!(
                    self.state,
                    SupervisorChildState::AwaitStart
                        | SupervisorChildState::Started
                        | SupervisorChildState::Cancelling
                ) {
                    return Err(invalid_data("supervisor CANCEL is out of order"));
                }
                self.cancel_reason = Some(reason);
                self.state = SupervisorChildState::Cancelling;
                Ok(None)
            }
            _ => Err(invalid_data("invalid supervisor parent frame")),
        }
    }

    pub(crate) fn mark_terminal(&mut self, terminal: SupervisorTerminal) -> io::Result<bool> {
        validate_supervisor_terminal_shape(&terminal)?;
        if let Some(previous) = &self.terminal {
            if previous == &terminal {
                return Ok(false);
            }
            return Err(invalid_data("conflicting supervisor TERMINAL frames"));
        }
        if !matches!(
            self.state,
            SupervisorChildState::Cancelling | SupervisorChildState::Started
        ) {
            return Err(invalid_data("supervisor TERMINAL is out of order"));
        }
        self.terminal = Some(terminal);
        self.state = SupervisorChildState::Terminal;
        Ok(true)
    }

    pub(crate) fn cancel_reason(&self) -> Option<CancelReason> {
        self.cancel_reason
    }
}

/// Cancellation state shared by the parent control worker and the existing
/// runtime cancellation bridge. The reason is kept separately because the
/// runtime bridge intentionally carries only a boolean request.
#[derive(Clone)]
pub(crate) struct SupervisorCancellation {
    context: crate::limits::CancellationContext,
    reason: Arc<Mutex<Option<CancelReason>>>,
    default_reason: CancelReason,
}

impl SupervisorCancellation {
    pub(crate) fn new(
        context: crate::limits::CancellationContext,
        default_reason: CancelReason,
    ) -> Self {
        let inherited_reason = context.reason().map(cancel_reason_from_context);
        Self {
            context,
            reason: Arc::new(Mutex::new(inherited_reason)),
            default_reason,
        }
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.context.is_requested()
    }

    pub(crate) fn request(&self, reason: CancelReason) {
        let reason = self.requested_reason().unwrap_or(reason);
        self.reason
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_or_insert(reason);
        self.context
            .request_with_reason(cancel_reason_to_context(reason));
    }

    pub(crate) fn reason(&self) -> CancelReason {
        self.requested_reason().unwrap_or(self.default_reason)
    }

    pub(crate) fn requested_reason(&self) -> Option<CancelReason> {
        self.reason
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .or_else(|| self.context.reason().map(cancel_reason_from_context))
    }

    pub(crate) fn context(&self) -> crate::limits::CancellationContext {
        self.context.clone()
    }
}

fn cancel_reason_from_context(reason: crate::limits::CancellationReason) -> CancelReason {
    match reason {
        crate::limits::CancellationReason::User => CancelReason::User,
        crate::limits::CancellationReason::Deadline => CancelReason::Deadline,
        crate::limits::CancellationReason::Shutdown => CancelReason::Shutdown,
    }
}

fn cancel_reason_to_context(reason: CancelReason) -> crate::limits::CancellationReason {
    match reason {
        CancelReason::User => crate::limits::CancellationReason::User,
        CancelReason::Deadline => crate::limits::CancellationReason::Deadline,
        CancelReason::Shutdown => crate::limits::CancellationReason::Shutdown,
    }
}

pub(crate) fn encode_payload<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    let payload = deno_core::serde_json::to_vec(value)
        .map_err(|error| invalid_data(format!("invalid supervisor payload: {error}")))?;
    if payload.len() > MAX_SUPERVISOR_FRAME_PAYLOAD {
        return Err(invalid_data("supervisor payload is too large"));
    }
    Ok(payload)
}

pub(crate) fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> io::Result<T> {
    if payload.len() > MAX_SUPERVISOR_FRAME_PAYLOAD {
        return Err(invalid_data("supervisor payload is too large"));
    }
    deno_core::serde_json::from_slice(payload)
        .map_err(|error| invalid_data(format!("invalid supervisor payload: {error}")))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn shrink_socket_buffer(stream: &TcpStream, option: libc::c_int) {
        use std::os::fd::AsRawFd;

        shrink_raw_socket_buffer(stream.as_raw_fd(), option).unwrap();
    }

    #[cfg(unix)]
    fn shrink_raw_socket_buffer(socket: libc::c_int, option: libc::c_int) -> io::Result<()> {
        let size: libc::c_int = 4096;
        let result = unsafe {
            libc::setsockopt(
                socket,
                libc::SOL_SOCKET,
                option,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of_val(&size) as libc::socklen_t,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(unix)]
    fn prepare_test_listener(listener: &std::net::TcpListener) {
        use std::os::fd::AsRawFd;

        shrink_raw_socket_buffer(listener.as_raw_fd(), libc::SO_RCVBUF).unwrap();
    }

    fn begin_write_observation(sender: std::sync::mpsc::Sender<FrameWriteTestEvent>) {
        FRAME_WRITE_TEST_EVENTS.with(|events| *events.borrow_mut() = Some(Vec::new()));
        FRAME_WRITE_TEST_LIVE_SENDER.with(|live| *live.borrow_mut() = Some(sender));
    }

    fn take_write_observation() -> Vec<FrameWriteTestEvent> {
        let _ = FRAME_WRITE_TEST_LIVE_SENDER.with(|sender| sender.borrow_mut().take());
        FRAME_WRITE_TEST_EVENTS.with(|events| events.borrow_mut().take().unwrap_or_default())
    }

    fn with_frame_write_test_clock<T>(
        clock: Arc<FrameWriteTestClock>,
        run: impl FnOnce() -> T,
    ) -> T {
        let previous = FRAME_WRITE_TEST_CLOCK.with(|current| current.replace(Some(clock)));
        let result = run();
        FRAME_WRITE_TEST_CLOCK.with(|current| {
            current.replace(previous);
        });
        result
    }

    fn with_frame_write_wouldblock_gate<T>(
        gate: std::sync::mpsc::Receiver<()>,
        run: impl FnOnce() -> T,
    ) -> T {
        let previous = FRAME_WRITE_TEST_WOULDBLOCK_GATE.with(|current| current.replace(Some(gate)));
        let result = run();
        FRAME_WRITE_TEST_WOULDBLOCK_GATE.with(|current| {
            current.replace(previous);
        });
        result
    }

    fn connect_test_stream(address: std::net::SocketAddr) -> io::Result<TcpStream> {
        #[cfg(unix)]
        {
            use std::os::fd::FromRawFd;

            let address = match address {
                std::net::SocketAddr::V4(address) => address,
                std::net::SocketAddr::V6(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "IPv4 test address required",
                    ))
                }
            };
            let socket = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
            if socket < 0 {
                return Err(io::Error::last_os_error());
            }
            let configure = (|| {
                shrink_raw_socket_buffer(socket, libc::SO_SNDBUF)?;
                #[cfg(target_vendor = "apple")]
                {
                    let enabled: libc::c_int = 1;
                    let result = unsafe {
                        libc::setsockopt(
                            socket,
                            libc::SOL_SOCKET,
                            libc::SO_NOSIGPIPE,
                            (&enabled as *const libc::c_int).cast(),
                            std::mem::size_of_val(&enabled) as libc::socklen_t,
                        )
                    };
                    if result != 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                let mut target: libc::sockaddr_in = unsafe { std::mem::zeroed() };
                #[cfg(target_vendor = "apple")]
                {
                    target.sin_len = std::mem::size_of_val(&target) as u8;
                }
                target.sin_family = libc::AF_INET as _;
                target.sin_port = address.port().to_be();
                target.sin_addr.s_addr = u32::from_ne_bytes(address.ip().octets());
                let connected = unsafe {
                    libc::connect(
                        socket,
                        (&target as *const libc::sockaddr_in).cast(),
                        std::mem::size_of_val(&target) as libc::socklen_t,
                    )
                };
                if connected != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            })();
            if let Err(error) = configure {
                unsafe { libc::close(socket) };
                return Err(error);
            }
            let stream = unsafe { TcpStream::from_raw_fd(socket) };
            prepare_supervisor_stream(&stream)?;
            Ok(stream)
        }
        #[cfg(not(unix))]
        {
            let stream = TcpStream::connect(address)?;
            prepare_supervisor_stream(&stream)?;
            Ok(stream)
        }
    }

    fn accept_test_stream(
        listener: &std::net::TcpListener,
    ) -> io::Result<(TcpStream, std::net::SocketAddr)> {
        let (stream, address) = listener.accept()?;
        prepare_supervisor_stream(&stream)?;
        Ok((stream, address))
    }

    fn wait_test_readable(stream: &TcpStream, deadline: Instant) -> io::Result<()> {
        #[cfg(target_vendor = "apple")]
        return poll_readable(stream, Some(deadline), None);
        #[cfg(not(target_vendor = "apple"))]
        {
            let _ = (stream, deadline);
            Ok(())
        }
    }

    fn terminal(outcome: SupervisorOutcome) -> SupervisorTerminal {
        SupervisorTerminal {
            outcome,
            exit_code: (outcome == SupervisorOutcome::Completed).then_some(0),
            category: (outcome == SupervisorOutcome::Failed)
                .then_some(SupervisorFailureCategory::Runtime),
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
        }
    }

    #[test]
    fn parent_state_enforces_start_barrier_and_terminal_idempotence() {
        let token = SupervisorToken::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let mut state = SupervisorParentSession::new(1);
        let bad_hello = SupervisorFrame::new(FrameKind::Hello, 0, vec![0; 16]).unwrap();
        assert!(state.accept_hello(&bad_hello, &token).is_err());

        let hello = SupervisorFrame::new(FrameKind::Hello, 0, token.as_bytes().to_vec()).unwrap();
        state.accept_hello(&hello, &token).unwrap();
        state.send_request().unwrap();
        let terminal_before_start = SupervisorFrame::new(
            FrameKind::Terminal,
            1,
            encode_payload(&terminal(SupervisorOutcome::Completed)).unwrap(),
        )
        .unwrap();
        assert!(state.receive_child_frame(&terminal_before_start).is_err());

        let accepted = SupervisorFrame::new(FrameKind::Accepted, 1, Vec::new()).unwrap();
        assert_eq!(
            state.receive_child_frame(&accepted).unwrap(),
            SupervisorFrameEvent::Accepted
        );
        state.send_start().unwrap();
        let started = SupervisorFrame::new(FrameKind::Started, 1, Vec::new()).unwrap();
        assert_eq!(
            state.receive_child_frame(&started).unwrap(),
            SupervisorFrameEvent::Started
        );
        let terminal_frame = SupervisorFrame::new(
            FrameKind::Terminal,
            1,
            encode_payload(&terminal(SupervisorOutcome::Completed)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            state.receive_child_frame(&terminal_frame).unwrap(),
            SupervisorFrameEvent::Terminal
        );
        assert_eq!(
            state.receive_child_frame(&terminal_frame).unwrap(),
            SupervisorFrameEvent::Terminal
        );
        let conflicting = SupervisorFrame::new(
            FrameKind::Terminal,
            1,
            encode_payload(&terminal(SupervisorOutcome::Failed)).unwrap(),
        )
        .unwrap();
        assert!(state.receive_child_frame(&conflicting).is_err());
    }

    #[test]
    fn parent_cancel_wins_before_start_and_terminal_needs_start_or_cancel() {
        let token = SupervisorToken::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let mut state = SupervisorParentSession::new(1);
        state
            .accept_hello(
                &SupervisorFrame::new(FrameKind::Hello, 0, token.as_bytes().to_vec()).unwrap(),
                &token,
            )
            .unwrap();
        state.send_request().unwrap();
        let accepted = SupervisorFrame::new(FrameKind::Accepted, 1, Vec::new()).unwrap();
        state.receive_child_frame(&accepted).unwrap();

        let plain_terminal = SupervisorFrame::new(
            FrameKind::Terminal,
            1,
            encode_payload(&terminal(SupervisorOutcome::Completed)).unwrap(),
        )
        .unwrap();
        assert!(state.receive_child_frame(&plain_terminal).is_err());

        assert!(state.send_cancel(CancelReason::User).unwrap());
        assert_eq!(state.state(), SupervisorParentState::CancellingBeforeStart);
        let started = SupervisorFrame::new(FrameKind::Started, 1, Vec::new()).unwrap();
        assert!(state.receive_child_frame(&started).is_err());
        let cancelled_terminal = SupervisorFrame::new(
            FrameKind::Terminal,
            1,
            encode_payload(&SupervisorTerminal {
                outcome: SupervisorOutcome::Cancelled,
                exit_code: None,
                category: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: false,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            state.receive_child_frame(&cancelled_terminal).unwrap(),
            SupervisorFrameEvent::Terminal
        );
        assert_eq!(state.state(), SupervisorParentState::Terminal);
    }

    #[test]
    fn request_and_start_are_single_use_and_typed_reason_wins_default() {
        let token = SupervisorToken::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let context = crate::limits::CancellationContext::new();
        context.request_with_reason(crate::limits::CancellationReason::Shutdown);
        let cancellation = SupervisorCancellation::new(context, CancelReason::User);
        assert_eq!(cancellation.reason(), CancelReason::Shutdown);

        let mut state = SupervisorParentSession::new(1);
        state
            .accept_hello(
                &SupervisorFrame::new(FrameKind::Hello, 0, token.as_bytes().to_vec()).unwrap(),
                &token,
            )
            .unwrap();
        state.send_request().unwrap();
        assert!(state.send_request().is_err());
        state
            .receive_child_frame(&SupervisorFrame::new(FrameKind::Accepted, 1, Vec::new()).unwrap())
            .unwrap();
        assert!(!state.authorize_start(&cancellation).unwrap());
        assert_eq!(state.state(), SupervisorParentState::CancellingBeforeStart);
        assert!(state.send_cancel(CancelReason::Shutdown).unwrap());
        assert!(!state.send_cancel(CancelReason::Shutdown).unwrap());
    }

    #[test]
    fn cancellation_is_idempotent_but_conflicts_fail_closed() {
        let mut state = SupervisorParentSession::new(1);
        let token = SupervisorToken::from_hex("00112233445566778899aabbccddeeff").unwrap();
        state
            .accept_hello(
                &SupervisorFrame::new(FrameKind::Hello, 0, token.as_bytes().to_vec()).unwrap(),
                &token,
            )
            .unwrap();
        state.send_request().unwrap();
        assert!(state.send_cancel(CancelReason::User).unwrap());
        assert!(!state.send_cancel(CancelReason::User).unwrap());
        assert!(state.send_cancel(CancelReason::Deadline).is_err());
    }

    #[test]
    fn cancellation_wins_only_when_it_linearizes_before_terminal() {
        let token = SupervisorToken::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let hello = SupervisorFrame::new(FrameKind::Hello, 0, token.as_bytes().to_vec()).unwrap();
        let accepted = SupervisorFrame::new(FrameKind::Accepted, 1, Vec::new()).unwrap();
        let started = SupervisorFrame::new(FrameKind::Started, 1, Vec::new()).unwrap();
        let completed = SupervisorFrame::new(
            FrameKind::Terminal,
            1,
            encode_payload(&terminal(SupervisorOutcome::Completed)).unwrap(),
        )
        .unwrap();

        let mut cancellation_first = SupervisorParentSession::new(1);
        cancellation_first.accept_hello(&hello, &token).unwrap();
        cancellation_first.send_request().unwrap();
        cancellation_first.receive_child_frame(&accepted).unwrap();
        cancellation_first.send_start().unwrap();
        cancellation_first.receive_child_frame(&started).unwrap();
        cancellation_first
            .send_cancel(CancelReason::Deadline)
            .unwrap();
        cancellation_first.receive_child_frame(&completed).unwrap();
        assert_eq!(
            cancellation_first.cancellation_before_terminal(),
            Some(CancelReason::Deadline)
        );

        let mut terminal_first = SupervisorParentSession::new(1);
        terminal_first.accept_hello(&hello, &token).unwrap();
        terminal_first.send_request().unwrap();
        terminal_first.receive_child_frame(&accepted).unwrap();
        terminal_first.send_start().unwrap();
        terminal_first.receive_child_frame(&started).unwrap();
        terminal_first.receive_child_frame(&completed).unwrap();
        assert!(!terminal_first.send_cancel(CancelReason::User).unwrap());
        assert_eq!(terminal_first.cancellation_before_terminal(), None);
    }

    #[test]
    fn child_state_does_not_start_before_start_frame() {
        let mut state = SupervisorChildSession::new();
        let request = SupervisorFrame::new(FrameKind::Request, 1, b"request".to_vec()).unwrap();
        assert_eq!(state.receive_parent_frame(&request).unwrap(), None);
        assert_eq!(state.state(), SupervisorChildState::AwaitStart);
        let start = SupervisorFrame::new(FrameKind::Start, 1, Vec::new()).unwrap();
        assert_eq!(
            state.receive_parent_frame(&start).unwrap(),
            Some(SupervisorFrameEvent::Started)
        );
        assert_eq!(state.state(), SupervisorChildState::Started);
    }

    #[test]
    fn frame_payload_bound_is_checked_before_encoding() {
        assert!(SupervisorFrame::new(
            FrameKind::Request,
            1,
            vec![0; MAX_SUPERVISOR_FRAME_PAYLOAD + 1]
        )
        .is_err());
    }

    #[test]
    fn terminal_shape_keeps_outcome_fields_disjoint() {
        assert!(
            validate_supervisor_terminal_shape(&terminal(SupervisorOutcome::Completed)).is_ok()
        );
        assert!(validate_supervisor_terminal_shape(&terminal(SupervisorOutcome::Failed)).is_ok());
        for outcome in [SupervisorOutcome::Cancelled, SupervisorOutcome::Deadline] {
            assert!(validate_supervisor_terminal_shape(&terminal(outcome)).is_ok());
        }

        let mut invalid = terminal(SupervisorOutcome::Completed);
        invalid.category = Some(SupervisorFailureCategory::Runtime);
        assert!(validate_supervisor_terminal_shape(&invalid).is_err());
        invalid = terminal(SupervisorOutcome::Failed);
        invalid.category = Some(SupervisorFailureCategory::Infrastructure);
        assert!(validate_supervisor_terminal_shape(&invalid).is_ok());
        invalid.category = Some(SupervisorFailureCategory::ChildCrash);
        assert!(validate_supervisor_terminal_shape(&invalid).is_err());
        invalid = terminal(SupervisorOutcome::Deadline);
        invalid.category = Some(SupervisorFailureCategory::Timeout);
        assert!(validate_supervisor_terminal_shape(&invalid).is_err());

        invalid = terminal(SupervisorOutcome::Completed);
        invalid.stdout = b"child output".to_vec();
        assert!(validate_supervisor_terminal_shape(&invalid).is_err());
        invalid.stdout.clear();
        invalid.stderr = b"child error".to_vec();
        assert!(validate_supervisor_terminal_shape(&invalid).is_err());
        invalid.stderr.clear();
        invalid.truncated = true;
        assert!(validate_supervisor_terminal_shape(&invalid).is_err());
    }

    #[test]
    fn version_one_frame_is_rejected_before_state_transition() {
        use std::io::Write;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let writer = std::thread::spawn(move || {
            let (mut stream, _) = accept_test_stream(&listener).unwrap();
            let mut frame = Vec::new();
            frame.extend_from_slice(&SUPERVISOR_MAGIC);
            frame.push(1);
            frame.push(FrameKind::Hello.to_byte());
            frame.extend_from_slice(&0u64.to_be_bytes());
            frame.extend_from_slice(&0u32.to_be_bytes());
            stream.write_all(&frame).unwrap();
        });

        let mut stream = connect_test_stream(address).unwrap();
        let error = read_frame(
            &mut stream,
            FrameDirection::ChildToParent,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        writer.join().unwrap();
    }

    #[test]
    fn first_byte_frame_assembly_uses_one_absolute_deadline() {
        use std::io::{self, Write};
        use std::net::TcpListener;
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let writer = std::thread::spawn(move || {
            let (mut stream, _) = accept_test_stream(&listener).unwrap();
            let mut frame = Vec::new();
            frame.extend_from_slice(&SUPERVISOR_MAGIC);
            frame.push(SUPERVISOR_VERSION);
            frame.push(FrameKind::Started.to_byte());
            frame.extend_from_slice(&1u64.to_be_bytes());
            frame.extend_from_slice(&0u32.to_be_bytes());
            stream.write_all(&frame[..1]).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            let _ = stream.write_all(&frame[1..]);
        });
        let mut stream = connect_test_stream(address).unwrap();
        let started = Instant::now();
        let result = read_frame_after_first_byte(
            &mut stream,
            FrameDirection::ChildToParent,
            Some(Instant::now() + Duration::from_secs(1)),
            Some(Instant::now() + Duration::from_millis(25)),
            None,
        );
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(500));
        writer.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn frame_write_uses_absolute_deadline_under_backpressure() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        prepare_test_listener(&listener);
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = accept_test_stream(&listener).unwrap();
            shrink_socket_buffer(&stream, libc::SO_RCVBUF);
            ready_tx.send(()).unwrap();
            let released = release_rx.recv_timeout(Duration::from_secs(2)).is_ok();
            let mut buf = [0; 4096];
            if released {
                loop {
                    if wait_test_readable(&stream, Instant::now() + Duration::from_secs(1)).is_err()
                    {
                        break;
                    }
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        });
        let mut stream = connect_test_stream(address).unwrap();
        let watchdog_stream = stream.try_clone().unwrap();
        let ready = ready_rx.recv_timeout(Duration::from_secs(1));
        let frame = SupervisorFrame::new(
            FrameKind::Request,
            1,
            vec![0x5a; MAX_SUPERVISOR_FRAME_PAYLOAD],
        )
        .unwrap();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let (live_tx, _live_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            begin_write_observation(live_tx);
            let started = Instant::now();
            let result = write_frame_until(
                &mut stream,
                &frame,
                Some(started + Duration::from_millis(150)),
                None,
            );
            result_tx
                .send((
                    result.map_err(|error| error.kind()),
                    started.elapsed(),
                    take_write_observation(),
                ))
                .unwrap();
        });
        let result = result_rx.recv_timeout(Duration::from_secs(1));
        let _ = release_tx.send(());
        let _ = watchdog_stream.shutdown(std::net::Shutdown::Both);
        let writer_join = writer.join();
        peer.join().unwrap();
        assert!(ready.is_ok(), "peer did not reach the blocked-read barrier");
        let late_result = if result.is_err() {
            result_rx.try_recv().ok()
        } else {
            None
        };
        assert!(
            result.is_ok(),
            "writer watchdog expired; late result after cleanup: {late_result:?}; join={writer_join:?}"
        );
        let (write_result, elapsed, events) = result.unwrap();
        assert!(
            write_result == Err(io::ErrorKind::TimedOut),
            "write did not time out under constrained buffers: {write_result:?}; writer={writer_join:?}"
        );
        let would_blocks: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                FrameWriteTestEvent::WouldBlock { offset, at } => Some((*offset, *at)),
                _ => None,
            })
            .collect();
        assert!(
            !would_blocks.is_empty(),
            "no real WouldBlock observed: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, FrameWriteTestEvent::PayloadSend { .. })),
            "no payload progress observed: {events:?}"
        );
        assert!(elapsed < Duration::from_millis(750), "elapsed {elapsed:?}");
    }

    #[cfg(unix)]
    #[test]
    fn slow_drain_partial_writes_share_one_absolute_deadline() {
        use std::io::Read;
        use std::net::TcpListener;

        enum PeerCommand {
            Drain(usize),
            Stop,
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        prepare_test_listener(&listener);
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (drain_tx, drain_rx) = std::sync::mpsc::channel();
        let (drained_tx, drained_rx) = std::sync::mpsc::channel::<Result<usize, io::ErrorKind>>();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = accept_test_stream(&listener).unwrap();
            shrink_socket_buffer(&stream, libc::SO_RCVBUF);
            ready_tx.send(()).unwrap();
            let mut buf = [0; 4096];
            let mut drains = 0;
            while let Ok(command) = drain_rx.recv_timeout(Duration::from_secs(4)) {
                match command {
                    PeerCommand::Stop => break,
                    PeerCommand::Drain(quota) => {
                        let mut remaining = quota;
                        let mut count = 0;
                        while remaining > 0 {
                            if let Err(error) =
                                wait_test_readable(&stream, Instant::now() + Duration::from_secs(1))
                            {
                                let _ = drained_tx.send(Err(error.kind()));
                                return drains;
                            }
                            let limit = remaining.min(buf.len());
                            match stream.read(&mut buf[..limit]) {
                                Ok(0) => {
                                    let _ = drained_tx.send(Err(io::ErrorKind::UnexpectedEof));
                                    return drains;
                                }
                                Ok(read) => {
                                    count += read;
                                    remaining -= read;
                                }
                                Err(error) => {
                                    let _ = drained_tx.send(Err(error.kind()));
                                    return drains;
                                }
                            }
                        }
                        drains += 1;
                        if drained_tx.send(Ok(count)).is_err() {
                            return drains;
                        }
                    }
                }
            }
            loop {
                if wait_test_readable(&stream, Instant::now() + Duration::from_secs(1)).is_err() {
                    break;
                }
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            drains
        });
        let mut stream = connect_test_stream(address).unwrap();
        let watchdog_stream = stream.try_clone().unwrap();
        let ready = ready_rx.recv_timeout(Duration::from_secs(1));
        let frame = SupervisorFrame::new(
            FrameKind::Request,
            1,
            vec![0x5a; MAX_SUPERVISOR_FRAME_PAYLOAD],
        )
        .unwrap();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let (live_tx, live_rx) = std::sync::mpsc::channel();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        let clock = Arc::new(FrameWriteTestClock::new());
        let writer_clock = Arc::clone(&clock);
        let writer = std::thread::spawn(move || {
            with_frame_write_test_clock(writer_clock, || {
                with_frame_write_wouldblock_gate(gate_rx, || {
                    begin_write_observation(live_tx);
                    let started = frame_write_now();
                    let real_started = Instant::now();
                    let result = write_frame_until(
                        &mut stream,
                        &frame,
                        Some(started + Duration::from_millis(250)),
                        None,
                    );
                    result_tx
                        .send((
                            result.map_err(|error| error.kind()),
                            frame_write_now().duration_since(started),
                            real_started.elapsed(),
                            take_write_observation(),
                        ))
                        .unwrap();
                });
            });
        });
        let event_deadline = Instant::now() + Duration::from_secs(2);
        let mut trace = Vec::new();
        let mut first_block = None;
        let mut payload_sends = Vec::new();
        while ready.is_ok() && Instant::now() < event_deadline {
            match live_rx.recv_timeout(event_deadline.saturating_duration_since(Instant::now())) {
                Ok(event @ FrameWriteTestEvent::PayloadSend { offset, count, at }) => {
                    payload_sends.push((offset, count, at));
                    trace.push(event);
                }
                Ok(event @ FrameWriteTestEvent::WouldBlock { offset, .. }) => {
                    trace.push(event);
                    if !payload_sends.is_empty() && offset < MAX_SUPERVISOR_FRAME_PAYLOAD {
                        first_block = Some(offset);
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let mut drain_counts = Vec::new();
        let mut drain_errors = Vec::new();
        let mut next_block = None;
        let mut sends_after_first_block = 0;
        let mut consumed = 0usize;
        if let Some(blocked_at) = first_block {
            // Advance logical time only after observing backpressure; the
            // socket send/read/poll operations remain real.
            clock.elapsed_ms.store(75, Ordering::SeqCst);
            let quota = SUPERVISOR_HEADER_LEN + blocked_at - consumed;
            let _ = drain_tx.send(PeerCommand::Drain(quota));
            match drained_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(count)) => {
                    drain_counts.push(count);
                    consumed += count;
                    let _ = gate_tx.send(());
                    let progress_deadline = Instant::now() + Duration::from_secs(1);
                    let mut progressed = false;
                    while Instant::now() < progress_deadline {
                        match live_rx.recv_timeout(
                            progress_deadline.saturating_duration_since(Instant::now()),
                        ) {
                            Ok(event @ FrameWriteTestEvent::PayloadSend { offset, count, at }) => {
                                if offset >= blocked_at {
                                    progressed = true;
                                    sends_after_first_block += 1;
                                    payload_sends.push((offset, count, at));
                                }
                                trace.push(event);
                            }
                            Ok(event @ FrameWriteTestEvent::WouldBlock { offset, .. }) => {
                                trace.push(event);
                                if progressed && offset > blocked_at {
                                    next_block = Some(offset);
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
                Ok(Err(kind)) => {
                    drain_errors.push(format!("peer read failed: {kind:?}"));
                    let _ = gate_tx.send(());
                }
                Err(error) => {
                    drain_errors.push(format!("peer acknowledgement failed: {error:?}"));
                    let _ = gate_tx.send(());
                }
            }
        }
        if let Some(blocked_at) = next_block {
            clock.elapsed_ms.store(150, Ordering::SeqCst);
            clock
                .advance_after_next_payload_send_to_ms
                .store(251, Ordering::SeqCst);
            let quota = SUPERVISOR_HEADER_LEN + blocked_at - consumed;
            let _ = drain_tx.send(PeerCommand::Drain(quota));
            match drained_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(Ok(count)) => {
                    drain_counts.push(count);
                    let _ = gate_tx.send(());
                    let progress_deadline = Instant::now() + Duration::from_secs(1);
                    while Instant::now() < progress_deadline {
                        match live_rx.recv_timeout(
                            progress_deadline.saturating_duration_since(Instant::now()),
                        ) {
                            Ok(event @ FrameWriteTestEvent::PayloadSend { offset, count, at }) => {
                                if offset >= blocked_at {
                                    sends_after_first_block += 1;
                                    payload_sends.push((offset, count, at));
                                    trace.push(event);
                                    break;
                                }
                                trace.push(event);
                            }
                            Ok(event) => trace.push(event),
                            Err(_) => break,
                        }
                    }
                }
                Ok(Err(kind)) => {
                    drain_errors.push(format!("peer read failed: {kind:?}"));
                    let _ = gate_tx.send(());
                }
                Err(error) => {
                    drain_errors.push(format!("peer acknowledgement failed: {error:?}"));
                    let _ = gate_tx.send(());
                }
            }
        }
        let result = result_rx.recv_timeout(Duration::from_secs(3));
        drop(gate_tx);
        let _ = drain_tx.send(PeerCommand::Stop);
        let _ = watchdog_stream.shutdown(std::net::Shutdown::Both);
        let writer_join = writer.join();
        let drains = peer.join().unwrap();
        assert!(ready.is_ok(), "peer did not reach the drain barrier");
        assert!(
            first_block.is_some(),
            "no incomplete-payload WouldBlock: {trace:?}"
        );
        assert_eq!(
            drains, 2,
            "expected two complete peer drains: counts={drain_counts:?}, errors={drain_errors:?}, trace={trace:?}"
        );
        assert_eq!(
            drain_counts.len(),
            2,
            "drains: {drain_counts:?}; errors={drain_errors:?}; trace={trace:?}"
        );
        assert!(
            drain_counts.iter().all(|count| *count > 0),
            "peer reported an empty drain: {drain_counts:?}; trace={trace:?}"
        );
        assert!(
            sends_after_first_block >= 2,
            "insufficient progress after the initial WouldBlock: {trace:?}"
        );
        let late_result = if result.is_err() {
            result_rx.try_recv().ok()
        } else {
            None
        };
        assert!(
            result.is_ok(),
            "writer watchdog expired; late result after cleanup: {late_result:?}; trace={trace:?}; join={writer_join:?}"
        );
        let (write_result, logical_elapsed, real_elapsed, events) =
            result.expect("writer watchdog expired");
        assert_eq!(write_result, Err(io::ErrorKind::TimedOut));
        assert!(
            payload_sends.len() >= 3,
            "insufficient live partial sends: {trace:?}; events={events:?}"
        );
        let would_blocks: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                FrameWriteTestEvent::WouldBlock { offset, at } => Some((*offset, *at)),
                _ => None,
            })
            .collect();
        assert!(
            !would_blocks.is_empty(),
            "expected backpressure between payload sends: {events:?}"
        );
        assert!(
            payload_sends
                .windows(2)
                .all(|pair| pair[1].0 >= pair[0].0 + pair[0].1),
            "payload offsets did not advance: {payload_sends:?}; trace={trace:?}"
        );
        assert!(
            payload_sends.iter().any(|(_, _, at)| {
                *at == clock.origin + Duration::from_millis(75)
            }) && payload_sends.iter().any(|(_, _, at)| {
                *at == clock.origin + Duration::from_millis(150)
            }),
            "native payload progress missed logical 75/150ms stages: {payload_sends:?}; trace={trace:?}"
        );
        assert!(
            payload_sends.last().unwrap().0 + payload_sends.last().unwrap().1
                < MAX_SUPERVISOR_FRAME_PAYLOAD,
            "frame unexpectedly completed before deadline: {payload_sends:?}"
        );
        assert_eq!(logical_elapsed, Duration::from_millis(251));
        assert!(
            real_elapsed < Duration::from_secs(3),
            "writer watchdog window exceeded: {real_elapsed:?}; writer={writer_join:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn request_write_cancels_while_payload_is_backpressured() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        prepare_test_listener(&listener);
        let address = listener.local_addr().unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = accept_test_stream(&listener).unwrap();
            shrink_socket_buffer(&stream, libc::SO_RCVBUF);
            ready_tx.send(()).unwrap();
            let _ = resume_rx.recv_timeout(Duration::from_secs(2));
            let mut buf = [0; 4096];
            let mut payload_bytes = 0;
            loop {
                if wait_test_readable(&stream, Instant::now() + Duration::from_secs(1)).is_err() {
                    break;
                }
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => payload_bytes += count,
                }
            }
            payload_bytes
        });
        let mut stream = connect_test_stream(address).unwrap();
        let watchdog_stream = stream.try_clone().unwrap();
        let cancellation = SupervisorCancellation::new(
            crate::limits::CancellationContext::new(),
            CancelReason::User,
        );
        let writer_cancellation = cancellation.clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let (live_tx, live_rx) = std::sync::mpsc::channel();
        let frame = SupervisorFrame::new(
            FrameKind::Request,
            1,
            vec![0x5a; MAX_SUPERVISOR_FRAME_PAYLOAD],
        )
        .unwrap();
        let writer = std::thread::spawn(move || {
            begin_write_observation(live_tx);
            let result = write_frame_until(
                &mut stream,
                &frame,
                Some(Instant::now() + Duration::from_secs(2)),
                Some(&writer_cancellation),
            );
            result_tx
                .send((
                    result.map_err(|error| error.kind()),
                    take_write_observation(),
                ))
                .unwrap();
        });
        let ready = ready_rx.recv_timeout(Duration::from_secs(1));
        let mut payload_progress = false;
        let mut blocked_offset = None;
        let event_deadline = Instant::now() + Duration::from_secs(1);
        while ready.is_ok() && Instant::now() < event_deadline {
            match live_rx.recv_timeout(event_deadline.saturating_duration_since(Instant::now())) {
                Ok(FrameWriteTestEvent::PayloadSend { .. }) => payload_progress = true,
                Ok(FrameWriteTestEvent::WouldBlock { offset, .. })
                    if offset < MAX_SUPERVISOR_FRAME_PAYLOAD =>
                {
                    blocked_offset = Some(offset);
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        if blocked_offset.is_some() && payload_progress {
            cancellation.request(CancelReason::User);
        }
        // Keep the peer from draining until the writer has returned on cancel.
        let result = result_rx.recv_timeout(Duration::from_secs(1));
        let _ = watchdog_stream.shutdown(std::net::Shutdown::Both);
        let _ = resume_tx.send(());
        let writer_join = writer.join();
        let payload_bytes = peer.join().unwrap();
        assert!(ready.is_ok(), "peer did not reach the no-drain barrier");
        let late_result = if result.is_err() {
            result_rx.try_recv().ok()
        } else {
            None
        };
        assert!(
            result.is_ok(),
            "writer watchdog expired; late result after cleanup: {late_result:?}; join={writer_join:?}"
        );
        assert!(
            payload_progress,
            "no live payload-send event before cancellation"
        );
        assert!(
            blocked_offset.is_some(),
            "no live incomplete-payload WouldBlock before cancellation"
        );
        assert!(payload_bytes < MAX_SUPERVISOR_FRAME_PAYLOAD);
        assert!(
            matches!(&result, Ok((Err(io::ErrorKind::Interrupted), _))),
            "REQUEST did not stop on cancellation: {result:?}; writer={writer_join:?}"
        );
        let (_, events) = result.unwrap();
        let would_blocks: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                FrameWriteTestEvent::WouldBlock { offset, at } => Some((*offset, *at)),
                _ => None,
            })
            .collect();
        assert!(
            !would_blocks.is_empty(),
            "REQUEST was not observed backpressured: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, FrameWriteTestEvent::PayloadSend { .. })),
            "REQUEST had no payload sends: {events:?}"
        );
    }

    #[test]
    fn cancelled_execution_does_not_block_control_or_terminal_writes() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let mut stream = connect_test_stream(address).unwrap();
            let cancel = read_frame(
                &mut stream,
                FrameDirection::ParentToChild,
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
            let terminal = read_frame(
                &mut stream,
                FrameDirection::ChildToParent,
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap();
            (cancel.kind, terminal.kind)
        });
        let (mut stream, _) = accept_test_stream(&listener).unwrap();
        let cancellation = SupervisorCancellation::new(
            crate::limits::CancellationContext::new(),
            CancelReason::User,
        );
        cancellation.request(CancelReason::User);
        let cancel = SupervisorFrame::new(FrameKind::Cancel, 1, Vec::new()).unwrap();
        write_frame_until(
            &mut stream,
            &cancel,
            Some(Instant::now() + Duration::from_secs(1)),
            None,
        )
        .unwrap();
        let terminal = SupervisorFrame::new(FrameKind::Terminal, 1, Vec::new()).unwrap();
        write_frame_until(
            &mut stream,
            &terminal,
            Some(Instant::now() + Duration::from_secs(1)),
            None,
        )
        .unwrap();
        assert_eq!(
            peer.join().unwrap(),
            (FrameKind::Cancel, FrameKind::Terminal)
        );
    }

    #[test]
    fn execution_cancellation_is_checked_before_start_frame_write() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let mut stream = connect_test_stream(address).unwrap();
            let mut byte = [0];
            wait_test_readable(&stream, Instant::now() + Duration::from_secs(1)).unwrap();
            stream.read(&mut byte).unwrap()
        });
        let (mut stream, _) = accept_test_stream(&listener).unwrap();
        let cancellation = SupervisorCancellation::new(
            crate::limits::CancellationContext::new(),
            CancelReason::User,
        );
        cancellation.request(CancelReason::User);
        let start = SupervisorFrame::new(FrameKind::Start, 1, Vec::new()).unwrap();
        assert_eq!(
            write_frame_until(
                &mut stream,
                &start,
                Some(Instant::now() + Duration::from_secs(1)),
                Some(&cancellation),
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
        assert_eq!(peer.join().unwrap(), 0);
    }

    #[test]
    fn cancellation_after_last_send_is_caught_before_frame_returns() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let mut stream = connect_test_stream(address).unwrap();
            read_frame(
                &mut stream,
                FrameDirection::ParentToChild,
                Instant::now() + Duration::from_secs(2),
            )
            .map(|frame| frame.kind)
        });
        let (mut stream, _) = accept_test_stream(&listener).unwrap();
        let watchdog_stream = stream.try_clone().unwrap();
        let (pause_reached_tx, pause_reached_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let cancellation = SupervisorCancellation::new(
            crate::limits::CancellationContext::new(),
            CancelReason::User,
        );
        let writer_cancellation = cancellation.clone();
        let (live_tx, _live_rx) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            begin_write_observation(live_tx);
            FRAME_WRITE_TEST_FINAL_PAUSE.with(|pause| {
                *pause.borrow_mut() = Some(Box::new(move || {
                    pause_reached_tx.send(()).unwrap();
                    let _ = resume_rx.recv_timeout(Duration::from_secs(2));
                }));
            });
            let frame = SupervisorFrame::new(FrameKind::Request, 1, vec![7; 32]).unwrap();
            let result = write_frame_until(
                &mut stream,
                &frame,
                Some(Instant::now() + Duration::from_secs(2)),
                Some(&writer_cancellation),
            );
            result_tx
                .send((
                    result.map_err(|error| error.kind()),
                    take_write_observation(),
                ))
                .unwrap();
        });
        let pause_reached = pause_reached_rx.recv_timeout(Duration::from_secs(1));
        if pause_reached.is_ok() {
            cancellation.request(CancelReason::User);
        }
        let _ = resume_tx.send(());
        let write_result = result_rx.recv_timeout(Duration::from_secs(1));
        let _ = watchdog_stream.shutdown(std::net::Shutdown::Both);
        let writer_join = writer.join();
        let received = peer.join().unwrap();
        assert!(
            pause_reached.is_ok(),
            "writer did not reach final-return barrier"
        );
        assert_eq!(received.unwrap(), FrameKind::Request);
        let (result, events) = write_result.expect("writer watchdog expired");
        assert_eq!(result, Err(io::ErrorKind::Interrupted));
        assert!(events
            .iter()
            .any(|event| matches!(event, FrameWriteTestEvent::PayloadSend { .. })));
        assert!(writer_join.is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn failed_partial_frame_shuts_down_stream() {
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        prepare_test_listener(&listener);
        let address = listener.local_addr().unwrap();
        let (header_tx, header_rx) = std::sync::mpsc::channel();
        let (close_tx, close_rx) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = accept_test_stream(&listener).unwrap();
            shrink_socket_buffer(&stream, libc::SO_RCVBUF);
            let mut header = [0; SUPERVISOR_HEADER_LEN];
            read_until(
                &mut stream,
                &mut header,
                Some(Instant::now() + Duration::from_secs(1)),
                None,
            )
            .unwrap();
            header_tx.send(()).unwrap();
            let _ = close_rx.recv_timeout(Duration::from_secs(2));
            stream.shutdown(std::net::Shutdown::Both).unwrap();
        });
        let mut stream = connect_test_stream(address).unwrap();
        let watchdog_stream = stream.try_clone().unwrap();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let frame = SupervisorFrame::new(
            FrameKind::Request,
            1,
            vec![0x5a; MAX_SUPERVISOR_FRAME_PAYLOAD],
        )
        .unwrap();
        let writer = std::thread::spawn(move || {
            let result = write_frame_until(
                &mut stream,
                &frame,
                Some(Instant::now() + Duration::from_secs(1)),
                None,
            );
            result_tx
                .send(result.map_err(|error| error.kind()))
                .unwrap();
        });
        let header_seen = header_rx.recv_timeout(Duration::from_secs(1));
        let _ = close_tx.send(());
        let result = result_rx.recv_timeout(Duration::from_secs(1));
        let _ = watchdog_stream.shutdown(std::net::Shutdown::Both);
        let writer_join = writer.join();
        peer.join().unwrap();
        assert!(header_seen.is_ok(), "peer did not observe the frame header");
        assert!(
            matches!(result, Ok(Err(kind)) if kind != io::ErrorKind::TimedOut),
            "expected a transport error after the partial header, got {result:?}; writer={writer_join:?}"
        );
    }

    #[test]
    fn token_debug_is_redacted() {
        let token = SupervisorToken::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let debug = format!("{token:?}");
        assert_eq!(debug, "SupervisorToken(REDACTED)");
        assert!(!debug.contains("001122"));
    }
}
