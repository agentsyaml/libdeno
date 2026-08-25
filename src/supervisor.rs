#![cfg(feature = "execution-control")]
#![allow(dead_code)]

use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

pub(crate) const SUPERVISOR_MAGIC: [u8; 4] = *b"LDSV";
pub(crate) const SUPERVISOR_VERSION: u8 = 1;
pub(crate) const MAX_SUPERVISOR_FRAME_PAYLOAD: usize = 1 << 20;
pub(crate) const SUPERVISOR_FRAME_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const SUPERVISOR_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const SUPERVISOR_CANCEL_GRACE: Duration = Duration::from_secs(2);
pub(crate) const SUPERVISOR_CHILD_EXIT_GRACE: Duration = Duration::from_millis(250);
/// Default retained bytes per requested capture stream. Supervisor TERMINAL
/// payloads encode bytes as JSON numbers inside a 1 MiB frame.
pub(crate) const SUPERVISOR_CAPTURE_BYTES_PER_STREAM: usize = 64 * 1024;
/// Conservative explicit per-stream ceiling: two 96 KiB JSON byte arrays,
/// including their worst-case four-byte-per-input expansion, remain below the
/// supervisor frame payload limit with terminal metadata overhead.
pub(crate) const SUPERVISOR_MAX_CAPTURE_BYTES_PER_STREAM: usize = 96 * 1024;

pub(crate) const SUPERVISOR_MODE_ENV: &str = "LIBDENO_SUPERVISOR_MODE";
pub(crate) const SUPERVISOR_ENDPOINT_ENV: &str = "LIBDENO_SUPERVISOR_ENDPOINT";
pub(crate) const SUPERVISOR_TOKEN_ENV: &str = "LIBDENO_SUPERVISOR_TOKEN";

const SUPERVISOR_HEADER_LEN: usize = 4 + 1 + 1 + 8 + 4;
const SUPERVISOR_TOKEN_LEN: usize = 16;

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
    if frame.payload.len() > MAX_SUPERVISOR_FRAME_PAYLOAD {
        return Err(invalid_data("supervisor frame payload is too large"));
    }

    let payload_len = u32::try_from(frame.payload.len())
        .map_err(|_| invalid_data("supervisor frame payload length does not fit"))?;
    let mut header = [0u8; SUPERVISOR_HEADER_LEN];
    header[..4].copy_from_slice(&SUPERVISOR_MAGIC);
    header[4] = SUPERVISOR_VERSION;
    header[5] = frame.kind.to_byte();
    header[6..14].copy_from_slice(&frame.request_id.to_be_bytes());
    header[14..18].copy_from_slice(&payload_len.to_be_bytes());
    stream.write_all(&header)?;
    stream.write_all(&frame.payload)
}

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
        let timeout = match (remaining, cancellation.is_some()) {
            (Some(remaining), true) => Some(remaining.min(Duration::from_millis(10))),
            (Some(remaining), false) => Some(remaining),
            (None, true) => Some(Duration::from_millis(10)),
            (None, false) => None,
        };
        // Set the timeout for each syscall. `read_exact` would keep one socket
        // timeout across its internal reads, allowing a peer to reset the
        // effective deadline by sending one byte at a time.
        stream.set_read_timeout(timeout)?;
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
            Err(error) => return Err(error),
        }
    }
    Ok(())
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

#[derive(Debug)]
pub(crate) struct SupervisorRunResult {
    pub(crate) output: crate::RunOutput,
    pub(crate) cleanup_strength: CleanupStrength,
    pub(crate) transport_status: SupervisorTransportStatus,
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

    fn terminal(outcome: SupervisorOutcome) -> SupervisorTerminal {
        SupervisorTerminal {
            outcome,
            exit_code: Some(0),
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
    fn first_byte_frame_assembly_uses_one_absolute_deadline() {
        use std::io::{self, Write};
        use std::net::{TcpListener, TcpStream};
        use std::time::{Duration, Instant};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let writer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
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
        let mut stream = TcpStream::connect(address).unwrap();
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

    #[test]
    fn token_debug_is_redacted() {
        let token = SupervisorToken::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let debug = format!("{token:?}");
        assert_eq!(debug, "SupervisorToken(REDACTED)");
        assert!(!debug.contains("001122"));
    }
}
