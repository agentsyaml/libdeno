// Subprocess execution mode: runs a script in a child process so that
// `Deno.exit(n)` or a hard runtime failure terminates only the child while
// the host process keeps running.

use std::path::Path;
use std::path::PathBuf;

#[cfg(feature = "execution-control")]
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
#[cfg(feature = "execution-control")]
use std::process::{Child, ExitStatus, Stdio};
#[cfg(feature = "execution-control")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "execution-control")]
use std::sync::{mpsc, Arc, Mutex};
#[cfg(feature = "execution-control")]
use std::time::{Duration, Instant};

use crate::limits::LIBDENO_SPAWNED_IPC;
use crate::run;
use crate::timing::{ExecutionTiming, Phase};
use crate::LibdenoError;
use crate::LibdenoOptions;
#[cfg(feature = "execution-control")]
use crate::RunOutput;

#[cfg(feature = "execution-control")]
use crate::limits::CancellationContext;
#[cfg(feature = "execution-control")]
use crate::supervisor::{
    decode_payload, encode_payload, read_frame, read_frame_after_first_byte,
    read_frame_with_cancellation, validate_supervisor_terminal_shape, write_frame, CancelReason,
    CleanupStrength, FrameDirection, FrameKind, SupervisorCancellation, SupervisorChildSession,
    SupervisorFailureCategory, SupervisorFrame, SupervisorFrameEvent, SupervisorParentSession,
    SupervisorRequest, SupervisorTerminal, SupervisorToken, SUPERVISOR_CANCEL_GRACE,
    SUPERVISOR_CAPTURE_BYTES_PER_STREAM, SUPERVISOR_CHILD_EXIT_GRACE, SUPERVISOR_CONNECT_TIMEOUT,
    SUPERVISOR_ENDPOINT_ENV, SUPERVISOR_FRAME_TIMEOUT, SUPERVISOR_MAX_CAPTURE_BYTES_PER_STREAM,
    SUPERVISOR_MODE_ENV, SUPERVISOR_TOKEN_ENV,
};

/// The request writer is detached because a blocking `write_all` cannot be
/// joined safely after the handshake deadline. Bound those retained threads
/// instead of allowing every stalled child to consume another thread forever.
const MAX_ACTIVE_HANDSHAKE_WRITERS: usize = 32;
const HANDSHAKE_WRITER_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);
static ACTIVE_HANDSHAKE_WRITERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

struct HandshakeWriterBudget;

impl Drop for HandshakeWriterBudget {
    fn drop(&mut self) {
        let previous = ACTIVE_HANDSHAKE_WRITERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        debug_assert!(
            previous > 0,
            "handshake writer budget released without a reservation"
        );
    }
}

#[cfg(test)]
fn active_handshake_writers() -> usize {
    ACTIVE_HANDSHAKE_WRITERS.load(std::sync::atomic::Ordering::Acquire)
}

fn reserve_handshake_writer() -> Result<HandshakeWriterBudget, std::io::Error> {
    use std::sync::atomic::Ordering::{AcqRel, Acquire};

    let mut active = ACTIVE_HANDSHAKE_WRITERS.load(Acquire);
    loop {
        if active >= MAX_ACTIVE_HANDSHAKE_WRITERS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!(
                    "subprocess handshake writer resource budget exhausted: {} active, maximum {}",
                    active, MAX_ACTIVE_HANDSHAKE_WRITERS
                ),
            ));
        }
        match ACTIVE_HANDSHAKE_WRITERS.compare_exchange_weak(active, active + 1, AcqRel, Acquire) {
            Ok(_) => return Ok(HandshakeWriterBudget),
            Err(next) => active = next,
        }
    }
}

/// Environment variable marking a process spawned by [`run_in_subprocess`].
const LIBDENO_CHILD_MODE: &str = "LIBDENO_CHILD_MODE";

/// Environment variable carrying the per-run auth token a child must present
/// to prove it was spawned by [`run_in_subprocess`].
const LIBDENO_CHILD_TOKEN: &str = "LIBDENO_CHILD_TOKEN";

/// Environment variable overriding the executable [`run_in_subprocess`]
/// spawns (defaults to the current executable). Lets tests point the child
/// run at a dedicated host binary.
const LIBDENO_HOST_EXE: &str = "LIBDENO_HOST_EXE";

/// Maximum child-mode request size. The reader consumes at most one extra byte
/// so it can distinguish a payload exactly at the ceiling from an oversized
/// payload without ever allocating an unbounded stdin buffer.
const MAX_CHILD_REQUEST_BYTES: usize = 1024 * 1024; // 1 MiB

/// Request payload serialized to the child process's stdin by
/// [`run_in_subprocess`]. The `token` must match the [`LIBDENO_CHILD_TOKEN`]
/// environment variable the parent set on the child; without it the child
/// refuses to run.
///
/// `entry` and `cwd` serialize as `PathBuf`, so a non-UTF-8 path fails the
/// serialization (an error the parent surfaces) rather than being silently
/// mangled by `to_string_lossy`.
#[derive(serde::Serialize, serde::Deserialize)]
struct ChildRunRequest {
    entry: PathBuf,
    permissions: Vec<String>,
    allow_all_permissions: bool,
    prompt: bool,
    args: Vec<String>,
    cwd: PathBuf,
    /// Per-run auth token, verified against [`LIBDENO_CHILD_TOKEN`].
    token: String,
    /// Safety options are forwarded verbatim: a host that bounds an
    /// untrusted script with `max_heap_bytes` / `execution_deadline` (or
    /// shrinks `features` to the default unstable set) must get the same
    /// bounds in child mode — the subprocess entry point exists for
    /// isolation, and silently dropping these would run the child unbounded
    /// on the full unstable API surface. The capture flags are deliberately
    /// NOT forwarded: the child inherits the parent's fds, so captured
    /// output in the child would be computed and discarded — capture
    /// belongs on the parent side (redirect its own fds around the call).
    features: Option<Vec<String>>,
    max_heap_bytes: Option<usize>,
    execution_deadline: Option<std::time::Duration>,
}

/// Generates a fresh 32-hex-char auth token for a child run.
///
/// The token authenticates the same-user subprocess handshake, not a security
/// boundary (see [`run_in_subprocess`]); it still needs real entropy so a
/// stdin-only third party cannot guess it.
fn child_token() -> Result<String, LibdenoError> {
    let mut buf = [0u8; 16];
    getrandom::fill(&mut buf).map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Constant-time token compare: both tokens are the same fixed format
/// (32 hex chars) generated by [`child_token`], so a plain byte XOR fold over
/// the equal-length representations avoids the timing channel of `!=` on
/// attacker-influenced data. Not a security boundary (same-user handshake) —
/// cheap defense, not the load-bearing check.
fn token_matches(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // Fail closed on empty tokens: a legitimate token is always the 32-hex
    // string child_token() generates, so an empty token (e.g. a hand-rolled
    // host with LIBDENO_CHILD_TOKEN set-but-empty) must never authenticate.
    !a.is_empty()
        && a.len() == b.len()
        && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Runs `entry` in a child process and returns its exit code.
///
/// The script runs inside a subprocess, so `Deno.exit(n)` or a hard runtime
/// failure terminates only the child — the host process keeps running. The
/// host binary must call [`maybe_handle_child_mode`] at the very start of its
/// `main()` for the child request to be serviced.
///
/// The child inherits stdout/stderr, so script output still appears. Entry,
/// permissions, prompt, args and cwd are passed over stdin as JSON, together
/// with a fresh per-run auth `token`; the serialized request is bounded at
/// 1 MiB before the child is spawned. The same token is handed to the child
/// via the `LIBDENO_CHILD_TOKEN` environment variable; the child refuses to
/// run unless the request token matches, so a process that can set
/// `LIBDENO_CHILD_MODE` and write the child's stdin cannot inject a request
/// of its own.
///
/// The payload write is bounded at 10s (aligned with the child side's stdin
/// deadline): a host that never services child mode (never reads stdin)
/// otherwise blocks the write once the pipe buffer fills; on timeout the
/// child is killed and `LibdenoError::Timeout` is returned. Note that a
/// small payload against a non-servicing host still succeeds the write — the
/// bound only protects once the request exceeds the pipe buffer.
///
/// # Security
///
/// Child mode turns a host process into an arbitrary-code-execution server
/// for anything that can set the two environment variables and write stdin.
/// Do NOT run a host with child mode enabled under elevated privileges
/// (setuid, service daemons, admin/root): the token authenticates only the
/// same user's subprocess, it is not a privilege boundary.
pub fn run_in_subprocess(
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
) -> Result<i32, LibdenoError> {
    run_in_subprocess_inner(entry.as_ref(), options, None, ExecutionTiming::disabled())
}

pub(crate) fn run_in_subprocess_with_executable_observed(
    host_executable: &Path,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
) -> Result<i32, LibdenoError> {
    run_in_subprocess_inner(entry.as_ref(), options, Some(host_executable), timing)
}

fn run_in_subprocess_inner(
    entry: &Path,
    options: &LibdenoOptions,
    explicit_host: Option<&Path>,
    timing: ExecutionTiming,
) -> Result<i32, LibdenoError> {
    // In-process runs never switch the process cwd (cwd is a resolution base
    // only), so the child's cwd can be pinned from a plain read with no
    // synchronization. The lock is NOT held across child.wait() — that would
    // deadlock hosts that run long-lived children (plugins/daemons): the
    // first child would hold a process-global lock forever and block every
    // later run_in_subprocess or run() call in the same process.
    let (payload, mut child) = spawn_child_request(
        entry,
        options,
        explicit_host,
        std::process::Stdio::inherit(),
        std::process::Stdio::inherit(),
    )?;
    write_child_request(&mut child, payload, timing.clone())?;
    // The writer thread dropped the last handle to the child's stdin after the
    // payload write, so a script reading process.stdin sees EOF instead of
    // blocking forever on the still-open pipe.
    let status = {
        let _reap = timing.span(Phase::CancelKillReap);
        child.wait().map_err(LibdenoError::Io)?
    };
    Ok(status.code().unwrap_or(1))
}

/// Spawns the host executable in child mode with the run request payload
/// ready to write. `stdout`/`stderr` pick the stdio mode: inherit for
/// [`run_in_subprocess`], piped for [`run_in_subprocess_with_output`].
///
/// The child's cwd is pinned from a plain read with no synchronization:
/// in-process runs never switch the process cwd (cwd is a resolution base
/// only), and the request payload carries its own cwd for the child side.
fn spawn_child_request(
    entry: &Path,
    options: &LibdenoOptions,
    explicit_host: Option<&Path>,
    stdout: std::process::Stdio,
    stderr: std::process::Stdio,
) -> Result<(Vec<u8>, std::process::Child), LibdenoError> {
    let token = child_token()?;
    let cwd = options.cwd.clone().unwrap_or(std::env::current_dir()?);
    let request = ChildRunRequest {
        entry: entry.to_path_buf(),
        permissions: options.permissions.clone(),
        allow_all_permissions: options.allow_all_permissions,
        prompt: options.prompt,
        args: options.args.clone(),
        cwd: cwd.clone(),
        token: token.clone(),
        features: options.features.clone(),
        max_heap_bytes: options.max_heap_bytes,
        execution_deadline: options.execution_deadline,
    };
    let payload = deno_core::serde_json::to_vec(&request)
        .map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))?;
    // Validate before resolving the executable or spawning a process: an
    // oversized request must have no child-side effects.
    validate_child_request_size(&payload)?;
    let exe = match explicit_host {
        Some(host_executable) => host_executable.to_path_buf(),
        None => std::env::var_os(LIBDENO_HOST_EXE)
            .map(PathBuf::from)
            .unwrap_or(std::env::current_exe()?),
    };
    let child = std::process::Command::new(exe)
        .env(LIBDENO_CHILD_MODE, "1")
        .env(LIBDENO_SPAWNED_IPC, "1")
        .env(LIBDENO_CHILD_TOKEN, &token)
        .current_dir(&cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(LibdenoError::Io)?;
    Ok((payload, child))
}

/// Bounded payload write with the 10s handshake timeout. The payload write
/// can block forever when the host never services child mode (a host that
/// does not call maybe_handle_child_mode never reads stdin): once the pipe
/// buffer (~64 KiB) fills, write_all blocks until the child reads. The
/// blocking write runs on a detached thread bounded at 10s (aligned with
/// the child side's own stdin deadline). On timeout the child is killed
/// (closing its pipe end, which unblocks the writer) and a Timeout error is
/// surfaced. The thread is deliberately not joined: after the kill it can
/// still be blocked inside write_all, and the process reaps it at exit.
///
/// On write failure the child is not leaked (or left a zombie): it may be
/// blocked reading stdin or already dead. If it already exited (e.g. its
/// 10s stdin deadline fired, or it was killed), the write failure is
/// downstream of that — surface the child's state instead of a bare
/// Broken pipe.
fn write_child_request(
    child: &mut std::process::Child,
    payload: Vec<u8>,
    timing: ExecutionTiming,
) -> Result<(), LibdenoError> {
    use std::io::Write;
    let (write_result, writer_done) = match child.stdin.take() {
        Some(mut stdin) => {
            // Reserve before spawning. If the bound is reached, kill/reap the
            // already-created child rather than returning an error with a
            // live child and no writer capable of completing its handshake.
            let writer_budget = match reserve_handshake_writer() {
                Ok(budget) => budget,
                Err(error) => {
                    let _cleanup = timing.span(Phase::CancelKillReap);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(LibdenoError::Io(error));
                }
            };
            let (tx, rx) = std::sync::mpsc::channel::<std::io::Result<()>>();
            let writer = std::thread::Builder::new()
                .name("libdeno-subprocess-handshake-writer".to_string())
                .spawn(move || {
                    let result = stdin.write_all(&payload);
                    // Do not report completion until the stdin handle is
                    // dropped and this detached writer is really done.
                    drop(stdin);
                    drop(writer_budget);
                    let _ = tx.send(result);
                });
            match writer {
                Ok(_) => {
                    let result = match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(e)) => Err(LibdenoError::Io(e)),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            Err(LibdenoError::Timeout(
                                "subprocess handshake timed out after 10s: \
                                     host did not service child mode (stdin not read)"
                                    .to_string(),
                            ))
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            Err(LibdenoError::Runtime(deno_core::anyhow::anyhow!(
                                "child stdin writer terminated unexpectedly"
                            )))
                        }
                    };
                    // This receiver is retained on the error path so a kill
                    // can be followed by a bounded wait for the actual writer
                    // completion. It is deliberately not a JoinHandle join:
                    // a platform pipe may remain uncloseable after the child
                    // is gone, and the budget must reflect that truth.
                    (result, Some(rx))
                }
                Err(error) => (Err(LibdenoError::Io(error)), None),
            }
        }
        None => (
            Err(LibdenoError::Runtime(deno_core::anyhow::anyhow!(
                "child has no stdin"
            ))),
            None,
        ),
    };
    if let Err(e) = write_result {
        let error = match child.try_wait() {
            Ok(Some(status)) => LibdenoError::Runtime(deno_core::anyhow::anyhow!(
                "child exited with {status} before accepting the run request \
                 (request write failed: {e})"
            )),
            _ => {
                let _cleanup = timing.span(Phase::CancelKillReap);
                let _ = child.kill();
                let _ = child.wait();
                e
            }
        };
        if let Some(writer_done) = writer_done {
            let _ = writer_done.recv_timeout(HANDSHAKE_WRITER_CLEANUP_TIMEOUT);
        }
        // Preserve the original error variant/message; if the detached writer
        // is still blocked after the bounded cleanup wait it remains counted
        // and a future budget error exposes that retained resource.
        Err(error)
    } else {
        Ok(())
    }
}

/// Runs `entry` in a child process and returns the exit code together with
/// the child's captured stdout/stderr — the subprocess answer to output
/// capture.
///
/// Unlike in-process capture — which is fd-level redirection of the
/// process-global stdout/stderr and therefore **exclusive** (any concurrent
/// run is rejected with `Configuration`, and it does not work on Windows) —
/// this capture is per-process: the child's own fds are piped back to the
/// parent and read concurrently with `wait()` (a verbose child can never
/// deadlock on a full pipe buffer). It runs in parallel with any other run,
/// on every platform, Windows included.
///
/// `capture_stdout` / `capture_stderr` are always implied (both streams are
/// returned); `max_capture_bytes` caps each stream — excess is dropped (the
/// reader keeps draining so the child never blocks) and
/// [`RunOutput::capture_truncated`](crate::RunOutput::capture_truncated) is
/// set. Every other option (permissions, features, max_heap_bytes,
/// execution_deadline, ...) behaves exactly as in [`run_in_subprocess`].
pub fn run_in_subprocess_with_output(
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
) -> Result<crate::RunOutput, LibdenoError> {
    run_in_subprocess_with_output_inner(
        entry.as_ref(),
        options,
        None,
        true,
        true,
        ExecutionTiming::disabled(),
    )
}

pub(crate) fn run_in_subprocess_with_selective_output_and_executable_observed(
    host_executable: &Path,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
) -> Result<crate::RunOutput, LibdenoError> {
    run_in_subprocess_with_output_inner(
        entry.as_ref(),
        options,
        Some(host_executable),
        options.capture_stdout,
        options.capture_stderr,
        timing,
    )
}

fn run_in_subprocess_with_output_inner(
    entry: &Path,
    options: &LibdenoOptions,
    explicit_host: Option<&Path>,
    capture_stdout: bool,
    capture_stderr: bool,
    timing: ExecutionTiming,
) -> Result<crate::RunOutput, LibdenoError> {
    let (payload, mut child) = spawn_child_request(
        entry,
        options,
        explicit_host,
        if capture_stdout {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::inherit()
        },
        if capture_stderr {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::inherit()
        },
    )?;
    write_child_request(&mut child, payload, timing.clone())?;
    // Drain both pipes concurrently with wait(): a child that fills a pipe
    // buffer blocks on write, so the parent must read while waiting or the
    // run deadlocks. Each stream keeps the first `max_capture_bytes` bytes
    // and keeps draining (dropping) the rest so the child never blocks; a
    // truncated stream sets the flag, mirroring in-process capture.
    let max = options.max_capture_bytes.unwrap_or(usize::MAX);
    let output_readers =
        match SubprocessOutputReaders::take(&mut child, capture_stdout, capture_stderr, max) {
            Ok(readers) => readers,
            Err(error) => {
                let _cleanup = timing.span(Phase::CancelKillReap);
                let _ = child.kill();
                let _ = child.wait();
                return Err(LibdenoError::Io(error));
            }
        };
    let _output_drain = (capture_stdout || capture_stderr).then(|| timing.span(Phase::OutputDrain));
    let status = {
        let _reap = timing.span(Phase::CancelKillReap);
        child.wait().map_err(LibdenoError::Io)?
    };
    let capture = output_readers.collect(None);
    if let Some(error) = capture.error {
        return Err(LibdenoError::Io(error));
    }
    Ok(crate::RunOutput {
        exit_code: status.code().unwrap_or(1),
        stdout: capture.stdout,
        stderr: capture.stderr,
        capture_truncated: capture.truncated,
    })
}

/// Reads a child pipe to EOF, keeping the first `max` bytes; excess is
/// drained and dropped (a truncated stream still unblocks the child). Read
/// errors other than `Interrupted` are returned to the caller.
#[cfg(test)]
fn drain_pipe<R: std::io::Read>(
    mut pipe: R,
    max: usize,
) -> Result<(Vec<u8>, bool), std::io::Error> {
    let mut out = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 64 * 1024];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let room = max.saturating_sub(out.len());
                let keep = room.min(n);
                if keep > 0 {
                    out.extend_from_slice(&buf[..keep]);
                }
                if n > keep {
                    truncated = true;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok((out, truncated))
}

/// Rejects an oversized child-mode request before JSON deserialization can
/// allocate based on attacker-controlled input.
fn validate_child_request_size(payload: &[u8]) -> Result<(), LibdenoError> {
    if payload.len() > MAX_CHILD_REQUEST_BYTES {
        return Err(LibdenoError::Runtime(deno_core::anyhow::anyhow!(format!(
            "child-mode request exceeds the {}-byte limit",
            MAX_CHILD_REQUEST_BYTES
        ))));
    }
    Ok(())
}

/// Services a child-run request when this process was spawned by
/// [`run_in_subprocess`].
///
/// Call this at the very start of `main()`. For a normal host launch it
/// returns `false` immediately; in child mode it executes the requested
/// script and exits the process with the script's exit code (including
/// `Deno.exit(n)`), so it does not return.
///
/// Child mode is serviced only when both `LIBDENO_CHILD_MODE` and
/// `LIBDENO_CHILD_TOKEN` are set and the request's token matches the
/// environment token — the handshake [`run_in_subprocess`] sets up. A missing
/// or mismatched token exits with an error rather than falling through to a
/// host run, which could otherwise execute the host at elevated privilege.
///
/// # Security
///
/// A process in child mode executes whatever request arrives on its stdin,
/// authenticated only by the token. Never run a host with child mode enabled
/// under elevated privileges (setuid, service daemon, admin/root).
///
/// The stdin request read is capped at 1 MiB and 10 seconds; a caller that
/// sets `LIBDENO_CHILD_MODE` but never sends (or never closes) stdin gets the
/// process exited rather than blocked forever.
///
/// In child mode the child's stdin is a pipe (consumed by the request JSON),
/// so the interactive prompter's terminal check denies without reading —
/// `prompt: true` in a child is equivalent to fail-closed deny; real
/// interaction only makes sense for in-process `run`.
pub fn maybe_handle_child_mode() -> bool {
    // Only an explicit `LIBDENO_CHILD_MODE=1` enters child mode; a stale or
    // accidental value must not turn the host into a request server.
    if std::env::var(LIBDENO_CHILD_MODE).as_deref() != Ok("1") {
        return false;
    }
    let Some(env_token) = std::env::var_os(LIBDENO_CHILD_TOKEN) else {
        eprintln!(
            "libdeno: {LIBDENO_CHILD_MODE} is set but {LIBDENO_CHILD_TOKEN} is missing; \
             refusing to service an unauthenticated child request"
        );
        std::process::exit(1);
    };
    let result: Result<i32, LibdenoError> = (|| {
        // The child-mode request read is bounded to both 1 MiB and 10 seconds:
        // a caller that sets LIBDENO_CHILD_MODE but never writes (or never
        // closes) stdin must not pin this process forever.
        // Stdin is a 'static handle and Send, so the blocking read moves to a
        // dedicated thread and the main thread waits with a deadline.
        //
        // In the normal flow the parent writes the payload (and closes stdin)
        // before the child reaches this read, so the data is already in the
        // pipe and the 10s bound never fires. It can only false-fire in the
        // degenerate case where the parent is suspended between spawn and
        // write (SIGSTOP / debugger attach) for over 10s — then the child
        // exits 1 and the parent's write fails against a dead child with a
        // confusing Io error. Accepted trade-off for bounding an
        // attacker-held-open stdin.
        let request: ChildRunRequest = {
            let (tx, rx) = std::sync::mpsc::channel::<Result<ChildRunRequest, LibdenoError>>();
            std::thread::spawn(move || {
                use std::io::Read;
                let result = (|| {
                    let mut payload = Vec::with_capacity(MAX_CHILD_REQUEST_BYTES + 1);
                    std::io::stdin()
                        .take((MAX_CHILD_REQUEST_BYTES as u64) + 1)
                        .read_to_end(&mut payload)
                        .map_err(LibdenoError::Io)?;
                    validate_child_request_size(&payload)?;
                    deno_core::serde_json::from_slice(&payload)
                        .map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))
                })();
                let _ = tx.send(result);
            });
            match rx.recv_timeout(std::time::Duration::from_secs(10)) {
                Ok(Ok(request)) => request,
                Ok(Err(e)) => return Err(e),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    eprintln!(
                        "libdeno: child-mode request timed out waiting on stdin; refusing to run"
                    );
                    std::process::exit(1);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(LibdenoError::Runtime(deno_core::anyhow::anyhow!(
                        "child-mode stdin reader terminated unexpectedly"
                    )));
                }
            }
        };
        if !token_matches(&request.token, &env_token.to_string_lossy()) {
            return Err(LibdenoError::Runtime(deno_core::anyhow::anyhow!(
                "child request token does not match {LIBDENO_CHILD_TOKEN}"
            )));
        }
        let options = LibdenoOptions {
            permissions: request.permissions,
            allow_all_permissions: request.allow_all_permissions,
            prompt: request.prompt,
            args: request.args,
            cwd: Some(request.cwd),
            // Forwarded safety options: the child must run under the same
            // bounds the host configured (see ChildRunRequest). `Some([])`
            // (the minimal surface, worker-options only) and `None` (the
            // full default unstable surface) survive the round-trip
            // distinctly — serde keeps `[]` vs `null` apart.
            features: request.features,
            max_heap_bytes: request.max_heap_bytes,
            execution_deadline: request.execution_deadline,
            ..Default::default()
        };
        // The request has been authenticated; strip the child-mode markers
        // before running so any subprocess the script spawns (Deno.Command,
        // child_process.spawn, exec) inherits a clean environment. Without
        // this, every grandchild enters child mode with a consumed stdin and
        // dies with an unactionable "stdin reader terminated" error — the
        // exact break that would hit a plugin shelling out to git/compilers.
        std::env::remove_var(LIBDENO_CHILD_MODE);
        std::env::remove_var(LIBDENO_CHILD_TOKEN);
        run(&request.entry, &options)
    })();
    match result {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("libdeno child run failed: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(feature = "execution-control")]
const SUPERVISOR_REQUEST_ID: u64 = 1;

#[cfg(feature = "execution-control")]
fn supervisor_error(message: impl Into<String>) -> LibdenoError {
    LibdenoError::Runtime(deno_core::anyhow::anyhow!(message.into()))
}

#[cfg(feature = "execution-control")]
fn supervisor_category_error(category: SupervisorFailureCategory) -> LibdenoError {
    LibdenoError::Runtime(deno_core::anyhow::anyhow!(category.summary()))
}

#[cfg(feature = "execution-control")]
fn supervisor_child_failure_category(error: &LibdenoError) -> SupervisorFailureCategory {
    if matches!(error, LibdenoError::Permission(_)) || error.is_permission_error() {
        SupervisorFailureCategory::Permission
    } else if matches!(error, LibdenoError::Configuration(_) | LibdenoError::Io(_)) {
        SupervisorFailureCategory::Infrastructure
    } else {
        SupervisorFailureCategory::Runtime
    }
}

#[cfg(feature = "execution-control")]
fn supervisor_session_error_is_timeout(error: &LibdenoError) -> bool {
    matches!(error, LibdenoError::Timeout(_))
        || matches!(error, LibdenoError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut)
}

#[cfg(feature = "execution-control")]
fn supervisor_session_error_is_natural_exit(error: &LibdenoError) -> bool {
    matches!(
        error,
        LibdenoError::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::BrokenPipe
            )
    )
}

#[cfg(feature = "execution-control")]
fn supervisor_session_error_is_protocol(error: &LibdenoError) -> bool {
    matches!(error, LibdenoError::Runtime(_))
        || matches!(error, LibdenoError::Io(error) if error.kind() == std::io::ErrorKind::InvalidData)
}

#[cfg(feature = "execution-control")]
fn supervisor_cancel_error(reason: CancelReason) -> LibdenoError {
    match reason {
        CancelReason::Deadline => {
            LibdenoError::Timeout("supervisor execution deadline exceeded".to_string())
        }
        CancelReason::User | CancelReason::Shutdown => {
            LibdenoError::Runtime(deno_core::anyhow::anyhow!("supervisor execution cancelled"))
        }
    }
}

#[cfg(feature = "execution-control")]
fn supervisor_exit_code_for_status(exit_code: i32) -> i32 {
    #[cfg(unix)]
    {
        exit_code.rem_euclid(256)
    }
    #[cfg(not(unix))]
    {
        exit_code
    }
}

#[cfg(feature = "execution-control")]
fn supervisor_capture_limit(
    capture_stdout: bool,
    capture_stderr: bool,
    max_capture_bytes: Option<usize>,
) -> Result<Option<usize>, LibdenoError> {
    if !capture_stdout && !capture_stderr {
        return Ok(None);
    }
    let limit = max_capture_bytes.unwrap_or(SUPERVISOR_CAPTURE_BYTES_PER_STREAM);
    if limit > SUPERVISOR_MAX_CAPTURE_BYTES_PER_STREAM {
        return Err(LibdenoError::Configuration(format!(
            "supervisor capture limit {limit} exceeds the {}-byte per-stream maximum",
            SUPERVISOR_MAX_CAPTURE_BYTES_PER_STREAM
        )));
    }
    Ok(Some(limit))
}

#[cfg(feature = "execution-control")]
fn supervisor_request(
    entry: &Path,
    options: &LibdenoOptions,
    cwd: PathBuf,
) -> Result<SupervisorRequest, LibdenoError> {
    let max_capture_bytes = supervisor_capture_limit(
        options.capture_stdout,
        options.capture_stderr,
        options.max_capture_bytes,
    )?;
    Ok(SupervisorRequest {
        entry: entry.to_path_buf(),
        cwd,
        permissions: options.permissions.clone(),
        allow_all_permissions: options.allow_all_permissions,
        prompt: options.prompt,
        args: options.args.clone(),
        features: options.features.clone(),
        max_heap_bytes: options.max_heap_bytes,
        execution_deadline: options.execution_deadline,
        capture_stdout: options.capture_stdout,
        capture_stderr: options.capture_stderr,
        max_capture_bytes,
    })
}

#[cfg(feature = "execution-control")]
fn supervisor_host_executable(explicit: Option<&Path>) -> Result<PathBuf, LibdenoError> {
    match explicit {
        Some(path) => Ok(path.to_path_buf()),
        None => std::env::var_os(LIBDENO_HOST_EXE)
            .map(PathBuf::from)
            .map(Ok)
            .unwrap_or_else(|| std::env::current_exe().map_err(LibdenoError::Io)),
    }
}

#[cfg(feature = "execution-control")]
fn spawn_supervisor_child(
    host_executable: &Path,
    cwd: &Path,
    endpoint: &str,
    token: &SupervisorToken,
    capture_stdout: bool,
    capture_stderr: bool,
) -> Result<Child, LibdenoError> {
    std::process::Command::new(host_executable)
        .env(SUPERVISOR_MODE_ENV, "1")
        .env(SUPERVISOR_ENDPOINT_ENV, endpoint)
        .env(SUPERVISOR_TOKEN_ENV, token.to_hex())
        // Keep the existing Node IPC pairing exactly as the legacy child path
        // does. Supervisor variables are removed by the child handler; this
        // marker is intentionally left for deno_node/fork descendants.
        .env(LIBDENO_SPAWNED_IPC, "1")
        .current_dir(cwd)
        .stdin(Stdio::inherit())
        .stdout(if capture_stdout {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stderr(if capture_stderr {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .spawn()
        .map_err(LibdenoError::Io)
}

/// A child can inherit a captured pipe and keep it open after the direct child
/// exits. Keep the collection wait bounded, and cap the number of detached
/// readers/fds retained by those descendants just like in-process capture.
const SUBPROCESS_OUTPUT_COLLECTION_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(500);
const MAX_ACTIVE_SUBPROCESS_OUTPUT_READERS: usize = 64;
static ACTIVE_SUBPROCESS_OUTPUT_READERS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

struct SubprocessOutputReaderBudget;

impl Drop for SubprocessOutputReaderBudget {
    fn drop(&mut self) {
        let previous =
            ACTIVE_SUBPROCESS_OUTPUT_READERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        debug_assert!(
            previous > 0,
            "subprocess output reader budget released without a reservation"
        );
    }
}

#[cfg(test)]
fn active_subprocess_output_readers() -> usize {
    ACTIVE_SUBPROCESS_OUTPUT_READERS.load(std::sync::atomic::Ordering::Acquire)
}

fn reserve_subprocess_output_reader() -> Result<SubprocessOutputReaderBudget, std::io::Error> {
    use std::sync::atomic::Ordering::{AcqRel, Acquire};

    let mut active = ACTIVE_SUBPROCESS_OUTPUT_READERS.load(Acquire);
    loop {
        if active >= MAX_ACTIVE_SUBPROCESS_OUTPUT_READERS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!(
                    "subprocess output reader/pipe resource budget exhausted: {} active, maximum {}",
                    active, MAX_ACTIVE_SUBPROCESS_OUTPUT_READERS
                ),
            ));
        }
        match ACTIVE_SUBPROCESS_OUTPUT_READERS.compare_exchange_weak(
            active,
            active + 1,
            AcqRel,
            Acquire,
        ) {
            Ok(_) => return Ok(SubprocessOutputReaderBudget),
            Err(next) => active = next,
        }
    }
}

enum SubprocessOutputReaderMessage {
    Data(Vec<u8>),
    Finished,
    Failed(std::io::Error),
}

struct SubprocessOutputReader {
    receiver: std::sync::mpsc::Receiver<SubprocessOutputReaderMessage>,
    overflow: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SubprocessOutputReader {
    fn spawn<R: std::io::Read + Send + 'static>(mut pipe: R, max: usize) -> std::io::Result<Self> {
        let reader_budget = reserve_subprocess_output_reader()?;
        let (sender, receiver) = std::sync::mpsc::channel();
        let overflow = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let overflow_for_thread = overflow.clone();
        let reader = std::thread::Builder::new()
            .name("libdeno-subprocess-output-reader".to_string())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drain_pipe_to_channel(&mut pipe, max, &sender, &overflow_for_thread)
                }));
                drop(pipe);
                drop(reader_budget);
                match result {
                    Ok(Ok(())) => {
                        let _ = sender.send(SubprocessOutputReaderMessage::Finished);
                    }
                    Ok(Err(error)) => {
                        let _ = sender.send(SubprocessOutputReaderMessage::Failed(error));
                    }
                    Err(_) => {
                        let _ = sender.send(SubprocessOutputReaderMessage::Failed(
                            std::io::Error::other("subprocess output reader panicked"),
                        ));
                    }
                }
            });
        reader?;
        // The reader owns its pipe and remains bounded by the collection
        // timeout/resource budget; joining it after a descendant inherits the
        // write end can block forever.
        Ok(Self { receiver, overflow })
    }
}

fn drain_pipe_to_channel(
    pipe: &mut impl std::io::Read,
    max: usize,
    sender: &std::sync::mpsc::Sender<SubprocessOutputReaderMessage>,
    overflow: &std::sync::atomic::AtomicBool,
) -> std::io::Result<()> {
    let mut retained = 0usize;
    let mut discarding = false;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match pipe.read(&mut buf) {
            Ok(n) => n,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if n == 0 {
            return Ok(());
        }
        if discarding {
            continue;
        }
        let keep = max.saturating_sub(retained).min(n);
        if keep > 0 {
            if sender
                .send(SubprocessOutputReaderMessage::Data(buf[..keep].to_vec()))
                .is_err()
            {
                // The collector timed out and dropped its receiver. Stop
                // reading so a completed reader releases its fd promptly.
                return Ok(());
            }
            retained += keep;
        }
        if n > keep {
            overflow.store(true, std::sync::atomic::Ordering::Relaxed);
            discarding = true;
        }
    }
}

struct SubprocessOutputReaders {
    stdout: Option<SubprocessOutputReader>,
    stderr: Option<SubprocessOutputReader>,
}

struct SubprocessOutputCapture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
    #[cfg_attr(not(feature = "execution-control"), allow(dead_code))]
    incomplete: bool,
    #[cfg_attr(not(feature = "execution-control"), allow(dead_code))]
    reader_error: bool,
    #[cfg_attr(not(feature = "execution-control"), allow(dead_code))]
    deadline_won: bool,
    error: Option<std::io::Error>,
}

#[cfg(feature = "execution-control")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SupervisorPartialOutput {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) truncated: bool,
    pub(crate) incomplete: bool,
    pub(crate) reader_error: bool,
}

#[cfg(feature = "execution-control")]
impl SupervisorPartialOutput {
    fn empty() -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
            incomplete: false,
            reader_error: false,
        }
    }

    fn reader_error() -> Self {
        Self {
            incomplete: true,
            reader_error: true,
            ..Self::empty()
        }
    }
}

#[cfg(feature = "execution-control")]
#[derive(Debug)]
pub(crate) struct SupervisorRunResult {
    pub(crate) exit_code: i32,
    pub(crate) partial_output: Option<SupervisorPartialOutput>,
    pub(crate) cleanup_strength: CleanupStrength,
    pub(crate) transport_status: crate::supervisor::SupervisorTransportStatus,
}

#[cfg(feature = "execution-control")]
impl SubprocessOutputCapture {
    fn into_supervisor_partial(self) -> SupervisorPartialOutput {
        SupervisorPartialOutput {
            stdout: self.stdout,
            stderr: self.stderr,
            truncated: self.truncated,
            incomplete: self.incomplete,
            reader_error: self.reader_error,
        }
    }
}

struct SubprocessOutputStream {
    receiver: std::sync::mpsc::Receiver<SubprocessOutputReaderMessage>,
    overflow: std::sync::Arc<std::sync::atomic::AtomicBool>,
    name: &'static str,
    output: Vec<u8>,
    error: Option<std::io::Error>,
    done: bool,
}

impl SubprocessOutputStream {
    fn new(reader: SubprocessOutputReader, name: &'static str) -> Self {
        Self {
            receiver: reader.receiver,
            overflow: reader.overflow,
            name,
            output: Vec::new(),
            error: None,
            done: false,
        }
    }

    fn accept(&mut self, message: SubprocessOutputReaderMessage) {
        match message {
            SubprocessOutputReaderMessage::Data(block) => self.output.extend_from_slice(&block),
            SubprocessOutputReaderMessage::Finished => self.done = true,
            SubprocessOutputReaderMessage::Failed(error) => {
                self.error = Some(std::io::Error::other(format!(
                    "{} reader failed: {error}",
                    self.name
                )));
                self.done = true;
            }
        }
    }

    fn disconnected(&mut self) {
        self.error = Some(std::io::Error::other(format!(
            "{} reader disconnected unexpectedly",
            self.name
        )));
        self.done = true;
    }

    fn try_receive(&mut self) -> bool {
        match self.receiver.try_recv() {
            Ok(message) => {
                self.accept(message);
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.disconnected();
                true
            }
        }
    }

    fn wait_receive(&mut self, timeout: std::time::Duration) -> bool {
        match self.receiver.recv_timeout(timeout) {
            Ok(message) => {
                self.accept(message);
                true
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => false,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                self.disconnected();
                true
            }
        }
    }
}

impl SubprocessOutputReaders {
    fn take(
        child: &mut std::process::Child,
        capture_stdout: bool,
        capture_stderr: bool,
        max: usize,
    ) -> std::io::Result<Self> {
        let stdout_pipe =
            if capture_stdout {
                Some(child.stdout.take().ok_or_else(|| {
                    std::io::Error::other("subprocess stdout pipe was not created")
                })?)
            } else {
                None
            };
        let stderr_pipe =
            if capture_stderr {
                Some(child.stderr.take().ok_or_else(|| {
                    std::io::Error::other("subprocess stderr pipe was not created")
                })?)
            } else {
                None
            };
        let stdout = stdout_pipe
            .map(|pipe| SubprocessOutputReader::spawn(pipe, max))
            .transpose()?;
        let stderr = stderr_pipe
            .map(|pipe| SubprocessOutputReader::spawn(pipe, max))
            .transpose()?;
        Ok(Self { stdout, stderr })
    }

    fn collect(self, effective_deadline: Option<std::time::Instant>) -> SubprocessOutputCapture {
        let fixed_deadline = std::time::Instant::now()
            .checked_add(SUBPROCESS_OUTPUT_COLLECTION_TIMEOUT)
            .unwrap_or_else(std::time::Instant::now);
        let deadline =
            effective_deadline.map_or(fixed_deadline, |deadline| fixed_deadline.min(deadline));
        let mut streams = [
            self.stdout
                .map(|reader| SubprocessOutputStream::new(reader, "subprocess stdout")),
            self.stderr
                .map(|reader| SubprocessOutputStream::new(reader, "subprocess stderr")),
        ];
        loop {
            let mut progress = false;
            for stream in streams.iter_mut().flatten() {
                if !stream.done {
                    progress |= stream.try_receive();
                }
            }
            if streams
                .iter()
                .all(|stream| stream.as_ref().is_none_or(|stream| stream.done))
            {
                break;
            }
            if deadline <= std::time::Instant::now() {
                break;
            }
            if !progress {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if let Some(stream) = streams.iter_mut().flatten().find(|stream| !stream.done) {
                    let _ =
                        stream.wait_receive(remaining.min(std::time::Duration::from_millis(10)));
                }
            }
        }
        let incomplete = streams.iter().flatten().any(|stream| !stream.done);
        let reader_error = streams
            .iter()
            .flatten()
            .any(|stream| stream.error.is_some());
        let deadline_won = incomplete
            && !reader_error
            && effective_deadline.is_some_and(|deadline| deadline <= fixed_deadline);
        let (stdout, stdout_truncated, stdout_error) = streams[0]
            .take()
            .map(|stream| {
                (
                    stream.output,
                    stream.overflow.load(std::sync::atomic::Ordering::Relaxed),
                    stream.error,
                )
            })
            .unwrap_or_default();
        let (stderr, stderr_truncated, stderr_error) = streams[1]
            .take()
            .map(|stream| {
                (
                    stream.output,
                    stream.overflow.load(std::sync::atomic::Ordering::Relaxed),
                    stream.error,
                )
            })
            .unwrap_or_default();
        let error = match (stdout_error, stderr_error) {
            (Some(stdout), Some(stderr)) => {
                Some(std::io::Error::other(format!("{stdout}; {stderr}")))
            }
            (Some(error), None) | (None, Some(error)) => Some(error),
            (None, None) => None,
        };
        SubprocessOutputCapture {
            stdout,
            stderr,
            truncated: stdout_truncated || stderr_truncated,
            incomplete: incomplete || reader_error,
            reader_error,
            deadline_won,
            error,
        }
    }
}

#[cfg(feature = "execution-control")]
struct SupervisorSessionOutcome {
    terminal: SupervisorTerminal,
    cancellation_before_terminal: Option<CancelReason>,
    transport_status: crate::supervisor::SupervisorTransportStatus,
}

#[cfg(feature = "execution-control")]
pub(crate) struct SupervisedSubprocessError {
    pub(crate) error: LibdenoError,
    pub(crate) category: Option<SupervisorFailureCategory>,
    pub(crate) partial_output: Option<SupervisorPartialOutput>,
    pub(crate) cleanup_strength: Option<CleanupStrength>,
    pub(crate) transport_status: Option<crate::supervisor::SupervisorTransportStatus>,
}

#[cfg(feature = "execution-control")]
impl From<LibdenoError> for SupervisedSubprocessError {
    fn from(error: LibdenoError) -> Self {
        Self {
            error,
            category: None,
            partial_output: None,
            cleanup_strength: None,
            transport_status: None,
        }
    }
}

#[cfg(feature = "execution-control")]
impl SupervisedSubprocessError {
    fn with_metadata_and_partial(
        error: LibdenoError,
        partial_output: Option<SupervisorPartialOutput>,
        cleanup_strength: Option<CleanupStrength>,
        transport_status: Option<crate::supervisor::SupervisorTransportStatus>,
    ) -> Self {
        Self {
            error,
            category: None,
            partial_output,
            cleanup_strength,
            transport_status,
        }
    }

    fn with_category_and_partial(
        category: SupervisorFailureCategory,
        partial_output: Option<SupervisorPartialOutput>,
        cleanup_strength: Option<CleanupStrength>,
        transport_status: Option<crate::supervisor::SupervisorTransportStatus>,
    ) -> Self {
        Self {
            error: if category == SupervisorFailureCategory::Timeout {
                LibdenoError::Timeout(category.summary().to_string())
            } else {
                supervisor_category_error(category)
            },
            category: Some(category),
            partial_output,
            cleanup_strength,
            transport_status,
        }
    }
}

#[cfg(feature = "execution-control")]
struct SupervisorControlWorker {
    stop: mpsc::Sender<()>,
    terminal_seen: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<std::io::Result<()>>>,
}

#[cfg(feature = "execution-control")]
impl SupervisorControlWorker {
    fn spawn(
        stream: &TcpStream,
        state: Arc<Mutex<SupervisorParentSession>>,
        cancellation: SupervisorCancellation,
        deadline: Option<Instant>,
    ) -> Result<Self, LibdenoError> {
        let stream = stream.try_clone().map_err(LibdenoError::Io)?;
        let (stop, stop_rx) = mpsc::channel();
        let terminal_seen = Arc::new(AtomicBool::new(false));
        let terminal_seen_for_thread = terminal_seen.clone();
        let join = std::thread::Builder::new()
            .name("libdeno-supervisor-control".to_string())
            .spawn(move || {
                supervisor_control_loop(
                    stream,
                    stop_rx,
                    state,
                    cancellation,
                    deadline,
                    terminal_seen_for_thread,
                )
            })
            .map_err(LibdenoError::Io)?;
        Ok(Self {
            stop,
            terminal_seen,
            join: Some(join),
        })
    }

    fn mark_terminal(&self) {
        self.terminal_seen.store(true, Ordering::Release);
    }

    fn stop_and_join(self) -> std::io::Result<()> {
        let _ = self.stop.send(());
        self.join()
    }

    fn join(mut self) -> std::io::Result<()> {
        match self
            .join
            .take()
            .expect("supervisor worker join handle")
            .join()
        {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::other("supervisor control worker panicked")),
        }
    }
}

#[cfg(feature = "execution-control")]
fn supervisor_control_loop(
    mut stream: TcpStream,
    stop_rx: mpsc::Receiver<()>,
    state: Arc<Mutex<SupervisorParentSession>>,
    cancellation: SupervisorCancellation,
    deadline: Option<Instant>,
    terminal_seen: Arc<AtomicBool>,
) -> std::io::Result<()> {
    stream.set_write_timeout(Some(SUPERVISOR_FRAME_TIMEOUT))?;
    loop {
        if stop_rx.recv_timeout(Duration::from_millis(10)).is_ok() {
            return Ok(());
        }
        if terminal_seen.load(Ordering::Acquire) {
            return Ok(());
        }
        if !cancellation.is_requested() {
            if deadline.is_some_and(|at| at <= Instant::now()) {
                cancellation.request(CancelReason::Deadline);
            } else {
                continue;
            }
        }

        if terminal_seen.load(Ordering::Acquire) {
            return Ok(());
        }
        let reason = cancellation.reason();
        let should_send = state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .send_cancel(reason)?;
        if should_send {
            let payload = encode_payload(&reason)?;
            let frame = SupervisorFrame::new(FrameKind::Cancel, SUPERVISOR_REQUEST_ID, payload)?;
            if let Err(error) = write_frame(&mut stream, &frame) {
                let _ = stream.shutdown(Shutdown::Both);
                return Err(error);
            }
        }

        let grace_deadline = supervisor_deadline_after(SUPERVISOR_CANCEL_GRACE);
        loop {
            let remaining = grace_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = stream.shutdown(Shutdown::Both);
                return Ok(());
            }
            match stop_rx.recv_timeout(remaining.min(Duration::from_millis(10))) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if terminal_seen.load(Ordering::Acquire) {
                return Ok(());
            }
        }
    }
}

#[cfg(feature = "execution-control")]
fn supervisor_deadline_after(duration: Duration) -> Instant {
    Instant::now()
        .checked_add(duration)
        .unwrap_or_else(Instant::now)
}

#[cfg(feature = "execution-control")]
fn effective_supervisor_deadline(
    started: Instant,
    parent_deadline: Option<Instant>,
    execution_deadline: Option<Duration>,
) -> Result<Option<Instant>, LibdenoError> {
    let option_deadline = execution_deadline
        .map(|duration| {
            started.checked_add(duration).ok_or_else(|| {
                LibdenoError::Configuration(
                    "execution deadline is too large for the host clock".to_string(),
                )
            })
        })
        .transpose()?;
    Ok(match (parent_deadline, option_deadline) {
        (Some(parent), Some(option)) => Some(parent.min(option)),
        (Some(parent), None) => Some(parent),
        (None, Some(option)) => Some(option),
        (None, None) => None,
    })
}

#[cfg(feature = "execution-control")]
fn phase_frame_deadline(effective_deadline: Option<Instant>) -> Instant {
    let frame_deadline = supervisor_deadline_after(SUPERVISOR_FRAME_TIMEOUT);
    effective_deadline.map_or(frame_deadline, |deadline| deadline.min(frame_deadline))
}

#[cfg(feature = "execution-control")]
fn set_supervisor_write_timeout(
    stream: &TcpStream,
    effective_deadline: Option<Instant>,
    cancellation: &SupervisorCancellation,
) -> std::io::Result<()> {
    let deadline = phase_frame_deadline(effective_deadline);
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        if effective_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            cancellation.request(CancelReason::Deadline);
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "supervisor frame deadline exceeded",
        ));
    }
    stream.set_write_timeout(Some(remaining))
}

#[cfg(feature = "execution-control")]
fn read_supervisor_handshake_frame(
    stream: &mut TcpStream,
    effective_deadline: Option<Instant>,
    cancellation: &SupervisorCancellation,
) -> std::io::Result<SupervisorFrame> {
    let result = read_frame_with_cancellation(
        stream,
        FrameDirection::ChildToParent,
        Some(phase_frame_deadline(effective_deadline)),
        Some(cancellation),
    );
    if result
        .as_ref()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
        && effective_deadline.is_some_and(|deadline| deadline <= Instant::now())
        && cancellation.requested_reason().is_none()
    {
        cancellation.request(CancelReason::Deadline);
    }
    result
}

#[cfg(feature = "execution-control")]
fn write_supervisor_frame(
    stream: &mut TcpStream,
    frame: &SupervisorFrame,
    effective_deadline: Option<Instant>,
    cancellation: &SupervisorCancellation,
) -> std::io::Result<()> {
    set_supervisor_write_timeout(stream, effective_deadline, cancellation)?;
    let result = write_frame(stream, frame);
    if result
        .as_ref()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
        && effective_deadline.is_some_and(|deadline| deadline <= Instant::now())
        && cancellation.requested_reason().is_none()
    {
        cancellation.request(CancelReason::Deadline);
    }
    result
}

#[cfg(feature = "execution-control")]
fn accept_supervisor_peer(
    listener: &TcpListener,
    effective_deadline: Option<Instant>,
    cancellation: &SupervisorCancellation,
) -> std::io::Result<TcpStream> {
    listener.set_nonblocking(true)?;
    let connect_deadline = supervisor_deadline_after(SUPERVISOR_CONNECT_TIMEOUT);
    let deadline = effective_deadline.map_or(connect_deadline, |value| value.min(connect_deadline));
    loop {
        if let Some(reason) = cancellation.requested_reason() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                format!("supervisor connection cancelled ({reason:?})"),
            ));
        }
        match listener.accept() {
            Ok((stream, peer)) => {
                if !peer.ip().is_loopback() {
                    continue;
                }
                stream.set_nonblocking(false)?;
                return Ok(stream);
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    if effective_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                        cancellation.request(CancelReason::Deadline);
                    }
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "supervisor connect deadline exceeded",
                    ));
                }
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Err(error) => return Err(error),
        }
    }
}

#[cfg(feature = "execution-control")]
fn cancellation_reason_if_due(
    cancellation: &SupervisorCancellation,
    deadline: Option<Instant>,
) -> Option<CancelReason> {
    if let Some(reason) = cancellation.requested_reason() {
        return Some(reason);
    }
    if deadline.is_some_and(|at| at <= Instant::now()) {
        cancellation.request(CancelReason::Deadline);
        return Some(CancelReason::Deadline);
    }
    None
}

#[cfg(feature = "execution-control")]
fn send_supervisor_cancel(
    stream: &mut TcpStream,
    state: &Arc<Mutex<SupervisorParentSession>>,
    reason: CancelReason,
) -> std::io::Result<()> {
    let should_send = state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .send_cancel(reason)?;
    if should_send {
        let payload = encode_payload(&reason)?;
        let frame = SupervisorFrame::new(FrameKind::Cancel, SUPERVISOR_REQUEST_ID, payload)?;
        stream.set_write_timeout(Some(Duration::from_millis(100)))?;
        write_frame(stream, &frame)?;
    }
    Ok(())
}

#[cfg(feature = "execution-control")]
fn read_supervisor_terminal(
    stream: &mut TcpStream,
    state: &Arc<Mutex<SupervisorParentSession>>,
    effective_deadline: Option<Instant>,
    started_phase_deadline: Instant,
    cancellation: &SupervisorCancellation,
    read_cancellation: Option<&SupervisorCancellation>,
    on_started: Option<&dyn Fn()>,
) -> std::io::Result<SupervisorTerminal> {
    let terminal = loop {
        let (parent_state, started_received) = {
            let state = state.lock().unwrap_or_else(|error| error.into_inner());
            (state.state(), state.started_received())
        };
        let waiting_started = parent_state
            == crate::supervisor::SupervisorParentState::AwaitStarted
            || (parent_state == crate::supervisor::SupervisorParentState::Cancelling
                && !started_received);
        let cancelled_before_start =
            parent_state == crate::supervisor::SupervisorParentState::CancellingBeforeStart;
        let (first_byte_deadline, assembly_deadline) = if waiting_started {
            // STARTED is one phase: all bytes share the same absolute bound.
            (Some(started_phase_deadline), Some(started_phase_deadline))
        } else if cancelled_before_start {
            let grace_deadline = supervisor_deadline_after(SUPERVISOR_CANCEL_GRACE);
            let terminal_deadline =
                effective_deadline.map_or(grace_deadline, |deadline| deadline.min(grace_deadline));
            (Some(terminal_deadline), Some(terminal_deadline))
        } else {
            // After STARTED, the control worker owns execution cancellation;
            // keep waiting for a cooperative TERMINAL during its grace. Once
            // that byte arrives, read_frame_after_first_byte creates its own
            // absolute SUPERVISOR_FRAME_TIMEOUT assembly bound.
            (None, None)
        };
        let phase_deadline = if waiting_started || cancelled_before_start {
            effective_deadline
        } else {
            None
        };
        let frame = read_frame_after_first_byte(
            stream,
            FrameDirection::ChildToParent,
            first_byte_deadline,
            assembly_deadline,
            (!cancelled_before_start)
                .then_some(read_cancellation)
                .flatten(),
        )
        .inspect_err(|error| {
            if error.kind() == std::io::ErrorKind::TimedOut
                && phase_deadline.is_some_and(|deadline| deadline <= Instant::now())
                && cancellation.requested_reason().is_none()
            {
                cancellation.request(CancelReason::Deadline);
            }
        })?;
        let event = state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .receive_child_frame(&frame)?;
        match event {
            SupervisorFrameEvent::Terminal => {
                break state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .terminal()
                    .expect("terminal event must store a terminal")
                    .clone();
            }
            SupervisorFrameEvent::Accepted => {}
            SupervisorFrameEvent::Started => {
                if let Some(on_started) = on_started {
                    on_started();
                }
            }
        }
    };

    // A child normally closes immediately after its one terminal frame. Give
    // a peer a short bounded window to prove an identical duplicate is benign
    // while still rejecting a conflicting duplicate.
    let duplicate_deadline = supervisor_deadline_after(Duration::from_millis(100));
    loop {
        match read_frame(stream, FrameDirection::ChildToParent, duplicate_deadline) {
            Ok(frame) => {
                let event = state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .receive_child_frame(&frame)?;
                if !matches!(event, SupervisorFrameEvent::Terminal) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "supervisor frame follows TERMINAL",
                    ));
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(error) => return Err(error),
        }
    }
    Ok(terminal)
}

#[cfg(feature = "execution-control")]
fn drive_supervisor_session(
    listener: TcpListener,
    request_payload: Vec<u8>,
    token: SupervisorToken,
    cancellation: SupervisorCancellation,
    deadline: Option<Instant>,
    on_started: Option<&dyn Fn()>,
) -> Result<SupervisorSessionOutcome, LibdenoError> {
    let mut stream =
        accept_supervisor_peer(&listener, deadline, &cancellation).map_err(LibdenoError::Io)?;
    let state = Arc::new(Mutex::new(SupervisorParentSession::new(
        SUPERVISOR_REQUEST_ID,
    )));

    let hello = read_supervisor_handshake_frame(&mut stream, deadline, &cancellation)
        .map_err(LibdenoError::Io)?;
    state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .accept_hello(&hello, &token)
        .map_err(LibdenoError::Io)?;

    let request = SupervisorFrame::new(FrameKind::Request, SUPERVISOR_REQUEST_ID, request_payload)
        .map_err(LibdenoError::Io)?;
    state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .send_request()
        .map_err(LibdenoError::Io)?;
    write_supervisor_frame(&mut stream, &request, deadline, &cancellation)
        .map_err(LibdenoError::Io)?;

    let accepted = read_supervisor_handshake_frame(&mut stream, deadline, &cancellation)
        .map_err(LibdenoError::Io)?;
    let accepted_event = state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .receive_child_frame(&accepted)
        .map_err(LibdenoError::Io)?;
    if accepted_event != SupervisorFrameEvent::Accepted {
        return Err(supervisor_error("supervisor child did not accept REQUEST"));
    }

    let _ = cancellation_reason_if_due(&cancellation, deadline);
    let start_authorized = state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .authorize_start(&cancellation)
        .map_err(LibdenoError::Io)?;
    // START authorization begins an independent supervisor phase. The
    // execution deadline is owned by the control worker from this point on.
    let started_phase_deadline = phase_frame_deadline(None);
    let mut control_worker = None;
    if start_authorized {
        let start = SupervisorFrame::new(FrameKind::Start, SUPERVISOR_REQUEST_ID, Vec::new())
            .map_err(LibdenoError::Io)?;
        write_supervisor_frame(&mut stream, &start, deadline, &cancellation)
            .map_err(LibdenoError::Io)?;
        control_worker = Some(SupervisorControlWorker::spawn(
            &stream,
            state.clone(),
            cancellation.clone(),
            deadline,
        )?);
    } else {
        let reason = cancellation.reason();
        send_supervisor_cancel(&mut stream, &state, reason).map_err(LibdenoError::Io)?;
    }

    // After START authorization, the control worker owns cancellation and its
    // fixed grace; the terminal reader must not turn that request into an
    // immediate Interrupted result.
    let terminal = read_supervisor_terminal(
        &mut stream,
        &state,
        deadline,
        started_phase_deadline,
        &cancellation,
        (!start_authorized).then_some(&cancellation),
        on_started,
    );
    let cancellation_before_terminal = state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .cancellation_before_terminal();
    let (terminal, transport_status) = match (terminal, control_worker) {
        (Ok(terminal), Some(worker)) => {
            // receive_child_frame has already linearized TERMINAL in the
            // parent state. Publish that fact before stopping the worker: it
            // suppresses a late CANCEL, while an already-sent CANCEL exits its
            // grace wait as soon as this cooperative terminal arrives.
            worker.mark_terminal();
            let worker_result = worker.stop_and_join();
            let status = if worker_result.is_ok() {
                crate::supervisor::SupervisorTransportStatus::Clean
            } else {
                crate::supervisor::SupervisorTransportStatus::Failed
            };
            (terminal, status)
        }
        (Ok(terminal), None) => (
            terminal,
            crate::supervisor::SupervisorTransportStatus::Clean,
        ),
        (Err(error), Some(worker)) => {
            if cancellation.is_requested() {
                // Do not send the stop signal: the worker must finish its
                // cancellation grace and close the peer itself.
                let _ = worker.join();
            } else {
                let _ = worker.stop_and_join();
            }
            return Err(LibdenoError::Io(error));
        }
        (Err(error), None) => return Err(LibdenoError::Io(error)),
    };
    Ok(SupervisorSessionOutcome {
        terminal,
        cancellation_before_terminal,
        transport_status,
    })
}

#[cfg(feature = "execution-control")]
struct SupervisorChildReap {
    status: ExitStatus,
    forced_kill: bool,
}

#[cfg(feature = "execution-control")]
fn reap_supervisor_child(child: &mut Child) -> std::io::Result<SupervisorChildReap> {
    // Give a child that has not produced a TERMINAL the same short chance to
    // exit naturally as a child that has produced one. This closes the EOF /
    // try_wait race without allowing a stuck child to avoid parent cleanup.
    let deadline = supervisor_deadline_after(SUPERVISOR_CHILD_EXIT_GRACE);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok(SupervisorChildReap {
                    status,
                    forced_kill: false,
                });
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                return kill_and_wait_supervisor_child(child).map(|status| SupervisorChildReap {
                    status,
                    forced_kill: true,
                });
            }
            Err(error) => {
                return match kill_and_wait_supervisor_child(child) {
                    Ok(status) => Err(std::io::Error::other(format!(
                        "supervisor child inspection failed: {error}; cleanup reaped exit status {status}"
                    ))),
                    Err(cleanup) => Err(std::io::Error::other(format!(
                        "supervisor child inspection failed: {error}; cleanup failed: {cleanup}"
                    ))),
                };
            }
        }
    }
}

#[cfg(feature = "execution-control")]
fn supervisor_session_failure_category(
    error: &LibdenoError,
    cleanup: &Result<SupervisorChildReap, std::io::Error>,
    cancellation: &SupervisorCancellation,
) -> Option<SupervisorFailureCategory> {
    if cleanup.is_err() {
        Some(SupervisorFailureCategory::Infrastructure)
    } else if cancellation.is_requested() {
        match cancellation.reason() {
            CancelReason::Deadline => Some(SupervisorFailureCategory::Timeout),
            CancelReason::User | CancelReason::Shutdown => None,
        }
    } else if cleanup.as_ref().is_ok_and(|reap| {
        !reap.forced_kill
            && (supervisor_session_error_is_natural_exit(error)
                || (supervisor_session_error_is_timeout(error)
                    && !supervisor_session_error_is_protocol(error)))
    }) {
        Some(SupervisorFailureCategory::ChildCrash)
    } else if supervisor_session_error_is_timeout(error) {
        Some(SupervisorFailureCategory::Timeout)
    } else {
        Some(SupervisorFailureCategory::Infrastructure)
    }
}

#[cfg(feature = "execution-control")]
fn kill_and_wait_supervisor_child(child: &mut Child) -> std::io::Result<ExitStatus> {
    let kill_result = child.kill();
    let wait_result = child.wait();
    match (kill_result, wait_result) {
        (Ok(()), Ok(status)) => Ok(status),
        (Err(kill), Ok(status)) => Err(std::io::Error::other(format!(
            "supervisor child kill failed after reaping exit status {status}: {kill}"
        ))),
        (Ok(()), Err(wait)) => Err(std::io::Error::other(format!(
            "supervisor child was killed but wait failed: {wait}"
        ))),
        (Err(kill), Err(wait)) => Err(std::io::Error::other(format!(
            "supervisor child kill failed: {kill}; wait failed: {wait}"
        ))),
    }
}

#[cfg(feature = "execution-control")]
fn validate_supervisor_terminal(
    terminal: &SupervisorTerminal,
    _request: &SupervisorRequest,
) -> Result<(), LibdenoError> {
    validate_supervisor_terminal_shape(terminal).map_err(LibdenoError::Io)?;
    Ok(())
}

#[cfg(feature = "execution-control")]
#[cfg(test)]
fn map_supervisor_terminal(
    terminal: SupervisorTerminal,
    status: ExitStatus,
    request: &SupervisorRequest,
    forced_kill: bool,
) -> Result<RunOutput, LibdenoError> {
    map_supervisor_terminal_with_cancellation(terminal, status, request, forced_kill, None)
}

#[cfg(feature = "execution-control")]
fn validate_supervisor_terminal_status(
    terminal: &SupervisorTerminal,
    status: ExitStatus,
    forced_kill: bool,
) -> Result<(), LibdenoError> {
    if forced_kill {
        return Ok(());
    }
    let expected = match terminal.outcome {
        crate::supervisor::SupervisorOutcome::Completed => {
            supervisor_exit_code_for_status(terminal.exit_code.expect("validated terminal"))
        }
        crate::supervisor::SupervisorOutcome::Cancelled
        | crate::supervisor::SupervisorOutcome::Deadline
        | crate::supervisor::SupervisorOutcome::Failed => 1,
    };
    if status.code() != Some(expected) {
        let outcome = match terminal.outcome {
            crate::supervisor::SupervisorOutcome::Completed => "completed",
            crate::supervisor::SupervisorOutcome::Cancelled => "cancelled",
            crate::supervisor::SupervisorOutcome::Deadline => "deadline",
            crate::supervisor::SupervisorOutcome::Failed => "failed",
        };
        return Err(supervisor_error(format!(
            "{outcome} supervisor TERMINAL disagrees with direct-child exit status"
        )));
    }
    Ok(())
}

#[cfg(feature = "execution-control")]
fn map_supervisor_terminal_with_cancellation(
    terminal: SupervisorTerminal,
    status: ExitStatus,
    request: &SupervisorRequest,
    forced_kill: bool,
    cancellation_before_terminal: Option<CancelReason>,
) -> Result<RunOutput, LibdenoError> {
    validate_supervisor_terminal(&terminal, request)?;
    if let Some(reason) = cancellation_before_terminal {
        // The parent state records only cancellations that won the state lock
        // before TERMINAL. Keep validating the child payload, then reconcile
        // to the first cancellation reason instead of returning Completed.
        validate_supervisor_terminal_status(&terminal, status, forced_kill)?;
        return Err(supervisor_cancel_error(reason));
    }
    match terminal.outcome {
        crate::supervisor::SupervisorOutcome::Completed => {
            let exit_code = terminal.exit_code.expect("validated completed terminal");
            if !forced_kill && status.code() != Some(supervisor_exit_code_for_status(exit_code)) {
                return Err(supervisor_error(
                    "supervisor TERMINAL disagrees with direct-child exit status",
                ));
            }
            Ok(RunOutput {
                exit_code,
                stdout: terminal.stdout,
                stderr: terminal.stderr,
                capture_truncated: terminal.truncated,
            })
        }
        crate::supervisor::SupervisorOutcome::Cancelled => {
            if !forced_kill && status.code() != Some(1) {
                return Err(supervisor_error(
                    "cancelled supervisor TERMINAL disagrees with direct-child exit status",
                ));
            }
            Err(supervisor_cancel_error(CancelReason::User))
        }
        crate::supervisor::SupervisorOutcome::Deadline => {
            if !forced_kill && status.code() != Some(1) {
                return Err(supervisor_error(
                    "deadline supervisor TERMINAL disagrees with direct-child exit status",
                ));
            }
            Err(supervisor_cancel_error(CancelReason::Deadline))
        }
        crate::supervisor::SupervisorOutcome::Failed => {
            if !forced_kill && status.code() != Some(1) {
                return Err(supervisor_error(
                    "failed supervisor TERMINAL disagrees with direct-child exit status",
                ));
            }
            Err(supervisor_error("supervised child execution failed"))
        }
    }
}

/// Runs one opt-in supervisor-protocol child. This is hidden and feature
/// gated until the executor lane owns backend selection; legacy subprocess
/// helpers above remain on their original stdin-JSON protocol.
#[cfg(feature = "execution-control")]
#[doc(hidden)]
pub fn run_in_supervised_subprocess(
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError> {
    let host = supervisor_host_executable(None)?;
    let cancellation = CancellationContext::new();
    let result = run_supervised_subprocess_with_executable_observed(
        &host,
        entry.as_ref(),
        options,
        Some(cancellation),
        CancelReason::User,
        None,
        ExecutionTiming::disabled(),
    )
    .map_err(|error| error.error)?;
    if result.transport_status != crate::supervisor::SupervisorTransportStatus::Clean
        || result
            .partial_output
            .as_ref()
            .is_some_and(|partial| partial.incomplete || partial.reader_error)
    {
        return Err(supervisor_category_error(
            SupervisorFailureCategory::Infrastructure,
        ));
    }
    let partial = result
        .partial_output
        .unwrap_or_else(SupervisorPartialOutput::empty);
    Ok(crate::RunOutput {
        exit_code: result.exit_code,
        stdout: partial.stdout,
        stderr: partial.stderr,
        capture_truncated: partial.truncated,
    })
}

#[cfg(feature = "execution-control")]
pub(crate) fn run_supervised_subprocess_with_executable_observed(
    host_executable: &Path,
    entry: &Path,
    options: &LibdenoOptions,
    cancellation: Option<CancellationContext>,
    default_cancel_reason: CancelReason,
    parent_deadline: Option<Instant>,
    timing: ExecutionTiming,
) -> Result<SupervisorRunResult, SupervisedSubprocessError> {
    run_supervised_subprocess_with_executable_observed_and_started(
        host_executable,
        entry,
        options,
        cancellation,
        default_cancel_reason,
        parent_deadline,
        timing,
        None,
    )
}

#[cfg(feature = "execution-control")]
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_supervised_subprocess_with_executable_observed_and_started(
    host_executable: &Path,
    entry: &Path,
    options: &LibdenoOptions,
    cancellation: Option<CancellationContext>,
    default_cancel_reason: CancelReason,
    parent_deadline: Option<Instant>,
    _timing: ExecutionTiming,
    on_started: Option<&dyn Fn()>,
) -> Result<SupervisorRunResult, SupervisedSubprocessError> {
    let started = Instant::now();
    let capture_requested = options.capture_stdout || options.capture_stderr;
    let cancellation = SupervisorCancellation::new(
        cancellation.unwrap_or_else(CancellationContext::new),
        default_cancel_reason,
    );
    let deadline =
        effective_supervisor_deadline(started, parent_deadline, options.execution_deadline)
            .map_err(|error| {
                SupervisedSubprocessError::with_metadata_and_partial(
                    error,
                    capture_requested.then(SupervisorPartialOutput::empty),
                    None,
                    None,
                )
            })?;
    if let Some(reason) = cancellation.requested_reason() {
        return Err(SupervisedSubprocessError::with_metadata_and_partial(
            supervisor_cancel_error(reason),
            capture_requested.then(SupervisorPartialOutput::empty),
            None,
            None,
        ));
    }
    if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
        cancellation.request(CancelReason::Deadline);
        return Err(SupervisedSubprocessError::with_metadata_and_partial(
            supervisor_cancel_error(cancellation.reason()),
            capture_requested.then(SupervisorPartialOutput::empty),
            None,
            None,
        ));
    }

    let cwd =
        options
            .cwd
            .clone()
            .unwrap_or(
                std::env::current_dir()
                    .map_err(LibdenoError::Io)
                    .map_err(|error| {
                        SupervisedSubprocessError::with_metadata_and_partial(
                            error,
                            capture_requested.then(SupervisorPartialOutput::empty),
                            None,
                            None,
                        )
                    })?,
            );
    let request = supervisor_request(entry, options, cwd.clone()).map_err(|error| {
        SupervisedSubprocessError::with_metadata_and_partial(
            error,
            capture_requested.then(SupervisorPartialOutput::empty),
            None,
            None,
        )
    })?;
    let request_payload = encode_payload(&request)
        .map_err(LibdenoError::Io)
        .map_err(|_| {
            SupervisedSubprocessError::with_category_and_partial(
                SupervisorFailureCategory::Infrastructure,
                capture_requested.then(SupervisorPartialOutput::empty),
                None,
                None,
            )
        })?;
    let token = SupervisorToken::generate()
        .map_err(|error| LibdenoError::Runtime(deno_core::anyhow::anyhow!(error)))
        .map_err(|_| {
            SupervisedSubprocessError::with_category_and_partial(
                SupervisorFailureCategory::Infrastructure,
                capture_requested.then(SupervisorPartialOutput::empty),
                None,
                None,
            )
        })?;
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(LibdenoError::Io)
        .map_err(|_| {
            SupervisedSubprocessError::with_category_and_partial(
                SupervisorFailureCategory::Infrastructure,
                capture_requested.then(SupervisorPartialOutput::empty),
                None,
                None,
            )
        })?;
    let endpoint = listener
        .local_addr()
        .map_err(LibdenoError::Io)
        .map_err(|_| {
            SupervisedSubprocessError::with_category_and_partial(
                SupervisorFailureCategory::Infrastructure,
                capture_requested.then(SupervisorPartialOutput::empty),
                None,
                None,
            )
        })?
        .to_string();
    // Supervisor capture is parent-owned on every platform. The child never
    // puts output in TERMINAL; one bounded pair of pipes is authoritative.
    let parent_pipe_capture = capture_requested;
    let mut child = spawn_supervisor_child(
        host_executable,
        &cwd,
        &endpoint,
        &token,
        parent_pipe_capture && request.capture_stdout,
        parent_pipe_capture && request.capture_stderr,
    )
    .map_err(|_| {
        SupervisedSubprocessError::with_category_and_partial(
            SupervisorFailureCategory::Infrastructure,
            parent_pipe_capture.then(SupervisorPartialOutput::empty),
            None,
            None,
        )
    })?;
    let output_readers = if parent_pipe_capture {
        let max = request
            .max_capture_bytes
            .unwrap_or(SUPERVISOR_CAPTURE_BYTES_PER_STREAM);
        match SubprocessOutputReaders::take(
            &mut child,
            request.capture_stdout,
            request.capture_stderr,
            max,
        ) {
            Ok(readers) => Some(readers),
            Err(_error) => {
                let _ = kill_and_wait_supervisor_child(&mut child);
                return Err(SupervisedSubprocessError::with_category_and_partial(
                    SupervisorFailureCategory::Infrastructure,
                    Some(SupervisorPartialOutput::reader_error()),
                    Some(CleanupStrength::DirectChild),
                    Some(crate::supervisor::SupervisorTransportStatus::Failed),
                ));
            }
        }
    } else {
        None
    };

    let session = drive_supervisor_session(
        listener,
        request_payload,
        token,
        cancellation.clone(),
        deadline,
        on_started,
    );
    let cleanup = reap_supervisor_child(&mut child);
    let (partial_output, collection_deadline_won) = match output_readers {
        Some(readers) => {
            let capture = readers.collect(deadline);
            let deadline_won = capture.deadline_won;
            (Some(capture.into_supervisor_partial()), deadline_won)
        }
        None => (None, false),
    };

    let session = match session {
        Ok(session) => session,
        Err(error) => {
            let category = supervisor_session_failure_category(&error, &cleanup, &cancellation);
            return Err(match category {
                Some(category) => SupervisedSubprocessError::with_category_and_partial(
                    category,
                    partial_output.clone(),
                    Some(CleanupStrength::DirectChild),
                    Some(crate::supervisor::SupervisorTransportStatus::Failed),
                ),
                None => SupervisedSubprocessError::with_metadata_and_partial(
                    supervisor_cancel_error(cancellation.reason()),
                    partial_output.clone(),
                    Some(CleanupStrength::DirectChild),
                    Some(crate::supervisor::SupervisorTransportStatus::Failed),
                ),
            });
        }
    };
    let reap = match cleanup {
        Ok(reap) => reap,
        Err(_error) => {
            return Err(SupervisedSubprocessError::with_category_and_partial(
                SupervisorFailureCategory::Infrastructure,
                partial_output.clone(),
                Some(CleanupStrength::DirectChild),
                Some(crate::supervisor::SupervisorTransportStatus::Failed),
            ))
        }
    };
    let status = reap.status;
    let capture_transport_failed = partial_output
        .as_ref()
        .is_some_and(|output| output.incomplete || output.reader_error);
    let transport_status = if capture_transport_failed {
        crate::supervisor::SupervisorTransportStatus::Failed
    } else {
        session.transport_status
    };
    let terminal = session.terminal;
    let cancellation_before_terminal = session.cancellation_before_terminal;
    let terminal_for_category = terminal.clone();
    let output = match map_supervisor_terminal_with_cancellation(
        terminal,
        status,
        &request,
        reap.forced_kill,
        cancellation_before_terminal,
    ) {
        Ok(_output) if collection_deadline_won => {
            return Err(match cancellation.requested_reason() {
                Some(reason @ (CancelReason::User | CancelReason::Shutdown)) => {
                    SupervisedSubprocessError::with_metadata_and_partial(
                        supervisor_cancel_error(reason),
                        partial_output.clone(),
                        Some(CleanupStrength::DirectChild),
                        Some(transport_status),
                    )
                }
                Some(CancelReason::Deadline) | None => {
                    SupervisedSubprocessError::with_category_and_partial(
                        SupervisorFailureCategory::Timeout,
                        partial_output.clone(),
                        Some(CleanupStrength::DirectChild),
                        Some(transport_status),
                    )
                }
            });
        }
        Ok(output) => output,
        Err(error) => {
            let category = if matches!(error, LibdenoError::Timeout(_))
                && terminal_for_category.outcome == crate::supervisor::SupervisorOutcome::Deadline
                && !matches!(
                    cancellation_before_terminal,
                    Some(CancelReason::User | CancelReason::Shutdown)
                ) {
                Some(SupervisorFailureCategory::Timeout)
            } else if matches!(
                cancellation_before_terminal,
                Some(CancelReason::User | CancelReason::Shutdown)
            ) || terminal_for_category.outcome
                == crate::supervisor::SupervisorOutcome::Cancelled
                || matches!(error, LibdenoError::Timeout(_))
            {
                None
            } else if terminal_for_category.outcome == crate::supervisor::SupervisorOutcome::Failed
                && validate_supervisor_terminal(&terminal_for_category, &request).is_ok()
                && validate_supervisor_terminal_status(
                    &terminal_for_category,
                    status,
                    reap.forced_kill,
                )
                .is_ok()
            {
                terminal_for_category.category
            } else {
                Some(SupervisorFailureCategory::Infrastructure)
            };
            return Err(match category {
                Some(category) => SupervisedSubprocessError::with_category_and_partial(
                    category,
                    partial_output.clone(),
                    Some(CleanupStrength::DirectChild),
                    Some(transport_status),
                ),
                None => SupervisedSubprocessError::with_metadata_and_partial(
                    error,
                    partial_output.clone(),
                    Some(CleanupStrength::DirectChild),
                    Some(transport_status),
                ),
            });
        }
    };
    Ok(SupervisorRunResult {
        exit_code: output.exit_code,
        partial_output,
        cleanup_strength: CleanupStrength::DirectChild,
        transport_status,
    })
}

#[cfg(feature = "execution-control")]
fn parse_supervisor_endpoint(value: &str) -> Result<SocketAddr, LibdenoError> {
    let endpoint = value
        .parse::<SocketAddr>()
        .map_err(|_| supervisor_error("malformed supervisor endpoint"))?;
    if !endpoint.ip().is_loopback() || endpoint.port() == 0 {
        return Err(supervisor_error("supervisor endpoint is not loopback"));
    }
    Ok(endpoint)
}

#[cfg(feature = "execution-control")]
#[allow(clippy::too_many_arguments)]
fn supervisor_child_exit(
    stream: &mut TcpStream,
    state: &mut SupervisorChildSession,
    outcome: crate::supervisor::SupervisorOutcome,
    exit_code: Option<i32>,
    category: Option<SupervisorFailureCategory>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
) -> Result<i32, LibdenoError> {
    let terminal = SupervisorTerminal {
        outcome,
        exit_code,
        category,
        stdout,
        stderr,
        truncated,
    };
    state
        .mark_terminal(terminal.clone())
        .map_err(LibdenoError::Io)?;
    let request_id = state
        .request_id()
        .ok_or_else(|| supervisor_error("supervisor child has no request ID"))?;
    let payload = encode_payload(&terminal).map_err(LibdenoError::Io)?;
    let frame =
        SupervisorFrame::new(FrameKind::Terminal, request_id, payload).map_err(LibdenoError::Io)?;
    write_frame(stream, &frame).map_err(LibdenoError::Io)?;
    Ok(match outcome {
        crate::supervisor::SupervisorOutcome::Completed => exit_code.unwrap_or(1),
        crate::supervisor::SupervisorOutcome::Failed
        | crate::supervisor::SupervisorOutcome::Cancelled
        | crate::supervisor::SupervisorOutcome::Deadline => 1,
    })
}

#[cfg(feature = "execution-control")]
fn start_supervisor_child_control(
    stream: &TcpStream,
    request_id: u64,
    cancellation: SupervisorCancellation,
    stop_rx: mpsc::Receiver<()>,
) -> Result<std::thread::JoinHandle<std::io::Result<()>>, LibdenoError> {
    let mut stream = stream.try_clone().map_err(LibdenoError::Io)?;
    std::thread::Builder::new()
        .name("libdeno-supervisor-child-control".to_string())
        .spawn(move || {
            stream.set_read_timeout(Some(Duration::from_millis(100)))?;
            loop {
                if stop_rx.try_recv().is_ok() {
                    return Ok(());
                }
                match read_frame(
                    &mut stream,
                    FrameDirection::ParentToChild,
                    Instant::now() + Duration::from_millis(100),
                ) {
                    Ok(frame) => {
                        if frame.kind != FrameKind::Cancel || frame.request_id != request_id {
                            cancellation.request(CancelReason::User);
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "invalid supervisor control frame after START",
                            ));
                        }
                        let reason: CancelReason = match decode_payload(&frame.payload) {
                            Ok(reason) => reason,
                            Err(error) => {
                                cancellation.request(CancelReason::User);
                                return Err(error);
                            }
                        };
                        let previous = cancellation.reason();
                        if cancellation.is_requested() && previous != reason {
                            cancellation.request(CancelReason::User);
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "conflicting supervisor cancellation reason",
                            ));
                        }
                        cancellation.request(reason);
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) => {}
                    Err(error) => {
                        cancellation.request(CancelReason::User);
                        return Err(error);
                    }
                }
            }
        })
        .map_err(LibdenoError::Io)
}

#[cfg(feature = "execution-control")]
fn run_supervisor_child(endpoint: SocketAddr, token: SupervisorToken) -> Result<i32, LibdenoError> {
    let mut stream = TcpStream::connect_timeout(&endpoint, SUPERVISOR_CONNECT_TIMEOUT)
        .map_err(LibdenoError::Io)?;
    stream
        .set_write_timeout(Some(SUPERVISOR_FRAME_TIMEOUT))
        .map_err(LibdenoError::Io)?;
    let mut state = SupervisorChildSession::new();

    let hello = SupervisorFrame::new(FrameKind::Hello, 0, token.as_bytes().to_vec())
        .map_err(LibdenoError::Io)?;
    write_frame(&mut stream, &hello).map_err(LibdenoError::Io)?;
    let request_frame = read_frame(
        &mut stream,
        FrameDirection::ParentToChild,
        Instant::now() + SUPERVISOR_FRAME_TIMEOUT,
    )
    .map_err(LibdenoError::Io)?;
    state
        .receive_parent_frame(&request_frame)
        .map_err(LibdenoError::Io)?;
    let request: SupervisorRequest =
        decode_payload(&request_frame.payload).map_err(LibdenoError::Io)?;
    let request_id = state
        .request_id()
        .ok_or_else(|| supervisor_error("supervisor REQUEST did not establish an ID"))?;
    let accepted = SupervisorFrame::new(FrameKind::Accepted, request_id, Vec::new())
        .map_err(LibdenoError::Io)?;
    write_frame(&mut stream, &accepted).map_err(LibdenoError::Io)?;

    let next = read_frame(
        &mut stream,
        FrameDirection::ParentToChild,
        Instant::now() + SUPERVISOR_FRAME_TIMEOUT,
    )
    .map_err(LibdenoError::Io)?;
    let event = state
        .receive_parent_frame(&next)
        .map_err(LibdenoError::Io)?;
    if next.kind == FrameKind::Cancel {
        let reason = state.cancel_reason().unwrap_or(CancelReason::User);
        return supervisor_child_exit(
            &mut stream,
            &mut state,
            match reason {
                CancelReason::Deadline => crate::supervisor::SupervisorOutcome::Deadline,
                CancelReason::User | CancelReason::Shutdown => {
                    crate::supervisor::SupervisorOutcome::Cancelled
                }
            },
            None,
            None,
            Vec::new(),
            Vec::new(),
            false,
        );
    }
    if event != Some(SupervisorFrameEvent::Started) || next.kind != FrameKind::Start {
        return Err(supervisor_error(
            "supervisor START barrier was not satisfied",
        ));
    }

    let (stop_tx, stop_rx) = mpsc::channel();
    let cancellation = SupervisorCancellation::new(CancellationContext::new(), CancelReason::User);
    let control =
        start_supervisor_child_control(&stream, request_id, cancellation.clone(), stop_rx)?;
    // STARTED is sent only after the control reader exists. No runtime call is
    // made before this frame is successfully written.
    let started = SupervisorFrame::new(FrameKind::Started, request_id, Vec::new())
        .map_err(LibdenoError::Io)?;
    if let Err(error) = write_frame(&mut stream, &started) {
        let _ = stop_tx.send(());
        let _ = control.join();
        return Err(LibdenoError::Io(error));
    }

    // The supervisor environment is deliberately stripped only after the
    // authenticated control setup and before the first runtime entry. The
    // legacy LIBDENO_SPAWNED_IPC marker is not touched. Capture stays in the
    // parent on every platform, so the terminal cannot become a second output
    // channel and the child keeps no capture budget.
    let options = LibdenoOptions {
        permissions: request.permissions,
        allow_all_permissions: request.allow_all_permissions,
        prompt: request.prompt,
        args: request.args,
        cwd: Some(request.cwd),
        max_heap_bytes: request.max_heap_bytes,
        execution_deadline: request.execution_deadline,
        capture_stdout: false,
        capture_stderr: false,
        max_capture_bytes: None,
        features: request.features,
    };
    let runtime_result = {
        let _user_execution = ExecutionTiming::disabled().span(Phase::UserExecution);
        crate::run_with_output_observed_cancellable(
            &request.entry,
            &options,
            ExecutionTiming::disabled(),
            Some(cancellation.context()),
        )
    };
    let _ = stop_tx.send(());
    let control_result = control.join();

    let reason = cancellation.reason();
    let (outcome, category, output) = match (reason, runtime_result) {
        (CancelReason::Deadline, _) => (crate::supervisor::SupervisorOutcome::Deadline, None, None),
        (CancelReason::User | CancelReason::Shutdown, _) if cancellation.is_requested() => {
            (crate::supervisor::SupervisorOutcome::Cancelled, None, None)
        }
        (_, Ok(output)) => (
            crate::supervisor::SupervisorOutcome::Completed,
            None,
            Some(output),
        ),
        (_, Err(LibdenoError::Timeout(_))) => {
            (crate::supervisor::SupervisorOutcome::Deadline, None, None)
        }
        (_, Err(error)) => (
            crate::supervisor::SupervisorOutcome::Failed,
            Some(supervisor_child_failure_category(&error)),
            None,
        ),
    };
    if control_result.is_err() && output.is_some() {
        // A control EOF after user code completed is secondary to the runtime
        // result; the parent records transport failure independently.
    }
    let (exit_code, stdout, stderr, truncated) = match output {
        Some(output) => (Some(output.exit_code), Vec::new(), Vec::new(), false),
        None => (None, Vec::new(), Vec::new(), false),
    };
    supervisor_child_exit(
        &mut stream,
        &mut state,
        outcome,
        exit_code,
        category,
        stdout,
        stderr,
        truncated,
    )
}

/// Services a child created by the distinct supervisor protocol. It is public
/// only so a tiny host binary can call it before its normal argument handling.
#[cfg(feature = "execution-control")]
#[doc(hidden)]
pub fn maybe_handle_supervisor_mode() -> bool {
    let Some(marker) = std::env::var_os(SUPERVISOR_MODE_ENV) else {
        return false;
    };
    if marker != "1" {
        eprintln!("libdeno: {SUPERVISOR_MODE_ENV} is malformed; refusing supervisor mode");
        std::env::remove_var(SUPERVISOR_MODE_ENV);
        std::env::remove_var(SUPERVISOR_ENDPOINT_ENV);
        std::env::remove_var(SUPERVISOR_TOKEN_ENV);
        std::process::exit(1);
    }

    let endpoint_value = std::env::var(SUPERVISOR_ENDPOINT_ENV).ok();
    let token_value = std::env::var(SUPERVISOR_TOKEN_ENV).ok();
    std::env::remove_var(SUPERVISOR_MODE_ENV);
    std::env::remove_var(SUPERVISOR_ENDPOINT_ENV);
    std::env::remove_var(SUPERVISOR_TOKEN_ENV);

    let endpoint = match endpoint_value {
        Some(value) => match parse_supervisor_endpoint(&value) {
            Ok(endpoint) => endpoint,
            Err(error) => {
                eprintln!("libdeno: supervisor endpoint rejected: {error}");
                std::process::exit(1);
            }
        },
        None => {
            eprintln!("libdeno: supervisor endpoint is missing; refusing supervisor mode");
            std::process::exit(1);
        }
    };
    let token = match token_value {
        Some(value) => match SupervisorToken::from_hex(&value) {
            Ok(token) => token,
            Err(_) => {
                eprintln!("libdeno: supervisor token is malformed; refusing supervisor mode");
                std::process::exit(1);
            }
        },
        None => {
            eprintln!("libdeno: supervisor token is missing; refusing supervisor mode");
            std::process::exit(1);
        }
    };

    match run_supervisor_child(endpoint, token) {
        Ok(code) => std::process::exit(code),
        Err(_) => {
            eprintln!(
                "libdeno supervisor child failed: {}",
                SupervisorFailureCategory::Infrastructure.summary()
            );
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::active_handshake_writers;
    use super::active_subprocess_output_readers;
    use super::drain_pipe;
    use super::reserve_handshake_writer;
    use super::reserve_subprocess_output_reader;
    use super::token_matches;
    use super::validate_child_request_size;
    use super::write_child_request;
    use super::ExecutionTiming;
    use super::LibdenoError;
    use super::SubprocessOutputReader;
    use super::SubprocessOutputReaderMessage;
    use super::SubprocessOutputReaders;
    use super::MAX_ACTIVE_HANDSHAKE_WRITERS;
    use super::MAX_CHILD_REQUEST_BYTES;
    #[cfg(feature = "execution-control")]
    use super::{
        drive_supervisor_session, effective_supervisor_deadline, encode_payload, supervisor_request,
    };
    #[cfg(feature = "execution-control")]
    use super::{
        map_supervisor_terminal, validate_supervisor_terminal, SupervisorRequest,
        SupervisorTerminal,
    };
    #[cfg(feature = "execution-control")]
    use super::{read_supervisor_terminal, SupervisorCancellation, SupervisorParentSession};
    #[cfg(feature = "execution-control")]
    use crate::supervisor::{
        read_frame, write_frame, CancelReason, FrameDirection, FrameKind,
        SupervisorFailureCategory, SupervisorFrame, SupervisorOutcome, SupervisorToken,
        SupervisorTransportStatus, SUPERVISOR_CAPTURE_BYTES_PER_STREAM,
        SUPERVISOR_MAX_CAPTURE_BYTES_PER_STREAM, SUPERVISOR_VERSION,
    };
    #[cfg(feature = "execution-control")]
    use crate::LibdenoOptions;
    #[cfg(feature = "execution-control")]
    use std::time::{Duration, Instant};

    static HANDSHAKE_WRITER_BUDGET_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static SUBPROCESS_OUTPUT_READER_BUDGET_TEST_LOCK: std::sync::Mutex<()> =
        std::sync::Mutex::new(());

    #[test]
    fn child_request_size_accepts_small_payload() {
        assert!(validate_child_request_size(b"{}").is_ok());
    }

    #[test]
    fn child_request_size_accepts_payload_at_limit() {
        let payload = vec![0u8; MAX_CHILD_REQUEST_BYTES];
        assert!(validate_child_request_size(&payload).is_ok());
    }

    #[test]
    fn child_request_size_rejects_payload_over_limit() {
        let payload = vec![0u8; MAX_CHILD_REQUEST_BYTES + 1];
        let error = validate_child_request_size(&payload).unwrap_err();
        assert!(matches!(&error, LibdenoError::Runtime(_)));
        assert!(error
            .to_string()
            .contains(&format!("{}-byte limit", MAX_CHILD_REQUEST_BYTES)));
    }

    #[test]
    fn drain_pipe_propagates_non_interrupted_read_errors() {
        struct ErrorReader;

        impl std::io::Read for ErrorReader {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "synthetic pipe failure",
                ))
            }
        }

        let error = drain_pipe(ErrorReader, usize::MAX).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(error.to_string(), "synthetic pipe failure");
    }

    #[test]
    fn subprocess_output_collection_preserves_partial_bytes_and_reader_errors() {
        let _lock = SUBPROCESS_OUTPUT_READER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(SubprocessOutputReaderMessage::Data(b"partial".to_vec()))
            .unwrap();
        sender
            .send(SubprocessOutputReaderMessage::Failed(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "synthetic pipe failure",
            )))
            .unwrap();
        drop(sender);
        let reader = SubprocessOutputReader {
            receiver,
            overflow: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let capture = SubprocessOutputReaders {
            stdout: Some(reader),
            stderr: None,
        }
        .collect(None);
        assert_eq!(capture.stdout, b"partial");
        assert!(!capture.truncated);
        assert!(capture.incomplete);
        assert!(capture.reader_error);
        let error = capture.error.unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(error.to_string().contains("synthetic pipe failure"));
    }

    #[test]
    fn subprocess_output_reader_panics_are_reported() {
        let _lock = SUBPROCESS_OUTPUT_READER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let (reader_dropped_sender, reader_dropped_receiver) = std::sync::mpsc::channel();
        struct PanicReader {
            reader_dropped_sender: std::sync::mpsc::Sender<()>,
        }

        impl std::io::Read for PanicReader {
            fn read(&mut self, _buffer: &mut [u8]) -> std::io::Result<usize> {
                panic!("synthetic reader panic");
            }
        }

        impl Drop for PanicReader {
            fn drop(&mut self) {
                self.reader_dropped_sender.send(()).unwrap();
            }
        }

        let reader = SubprocessOutputReader::spawn(
            PanicReader {
                reader_dropped_sender,
            },
            usize::MAX,
        )
        .unwrap();
        reader_dropped_receiver.recv().unwrap();
        let capture = SubprocessOutputReaders {
            stdout: Some(reader),
            stderr: None,
        }
        .collect(None);
        assert!(capture.incomplete);
        assert!(capture.reader_error);
        let error = capture.error.unwrap();
        assert!(error.to_string().contains("reader panicked"));
    }

    #[test]
    fn subprocess_output_capture_keeps_truncation_independent_from_completion() {
        let _lock = SUBPROCESS_OUTPUT_READER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let reader =
            SubprocessOutputReader::spawn(std::io::Cursor::new(b"output".to_vec()), 0).unwrap();
        let capture = SubprocessOutputReaders {
            stdout: Some(reader),
            stderr: None,
        }
        .collect(None);
        assert!(capture.truncated);
        assert!(!capture.incomplete);
        assert!(!capture.reader_error);
        assert!(capture.error.is_none());
        assert!(capture.stdout.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_output_capture_marks_descendant_held_pipe_incomplete() {
        let _lock = SUBPROCESS_OUTPUT_READER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let mut child = std::process::Command::new("sh")
            .args(["-c", "printf prefix; (sleep 1) & exit 0"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let readers = SubprocessOutputReaders::take(&mut child, true, false, 64).unwrap();
        assert!(child.wait().unwrap().success());
        let capture = readers.collect(None);
        assert_eq!(capture.stdout, b"prefix");
        assert!(!capture.truncated);
        assert!(capture.incomplete);
        assert!(!capture.reader_error);
        assert!(capture.error.is_none());
    }

    #[test]
    fn subprocess_output_collection_honors_absolute_deadline_and_keeps_prefix() {
        let _lock = SUBPROCESS_OUTPUT_READER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());

        struct BlockingReader {
            ready: std::sync::mpsc::Sender<()>,
            release: std::sync::mpsc::Receiver<()>,
            sent: bool,
        }

        impl std::io::Read for BlockingReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if !self.sent {
                    let prefix = b"prefix";
                    buffer[..prefix.len()].copy_from_slice(prefix);
                    self.sent = true;
                    self.ready.send(()).unwrap();
                    return Ok(prefix.len());
                }
                let _ = self.release.recv();
                Ok(0)
            }
        }

        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let baseline = active_subprocess_output_readers();
        let reader = SubprocessOutputReader::spawn(
            BlockingReader {
                ready: ready_sender,
                release: release_receiver,
                sent: false,
            },
            usize::MAX,
        )
        .unwrap();
        ready_receiver
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();

        let started = std::time::Instant::now();
        let capture = SubprocessOutputReaders {
            stdout: Some(reader),
            stderr: None,
        }
        .collect(Some(started + std::time::Duration::from_millis(75)));
        assert_eq!(capture.stdout, b"prefix");
        assert!(!capture.truncated);
        assert!(capture.incomplete);
        assert!(!capture.reader_error);
        assert!(capture.deadline_won);
        assert!(started.elapsed() < super::SUBPROCESS_OUTPUT_COLLECTION_TIMEOUT);

        release_sender.send(()).unwrap();
        for _ in 0..100 {
            if active_subprocess_output_readers() == baseline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(active_subprocess_output_readers(), baseline);
    }

    #[test]
    fn subprocess_output_reader_budget_is_released_before_collection_returns() {
        let _lock = SUBPROCESS_OUTPUT_READER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let before = active_subprocess_output_readers();
        let reader =
            SubprocessOutputReader::spawn(std::io::Cursor::new(b"output".to_vec()), usize::MAX)
                .unwrap();
        let capture = SubprocessOutputReaders {
            stdout: Some(reader),
            stderr: None,
        }
        .collect(None);
        assert_eq!(active_subprocess_output_readers(), before);
        assert_eq!(capture.stdout, b"output");
        assert!(!capture.incomplete);
        assert!(!capture.reader_error);
    }

    #[test]
    fn subprocess_output_reader_budget_is_finite_and_releases_exactly() {
        let _lock = SUBPROCESS_OUTPUT_READER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let baseline = active_subprocess_output_readers();
        let available = super::MAX_ACTIVE_SUBPROCESS_OUTPUT_READERS - baseline;
        let mut reservations = Vec::with_capacity(available);
        for _ in 0..available {
            reservations.push(reserve_subprocess_output_reader().unwrap());
        }
        assert_eq!(
            active_subprocess_output_readers(),
            super::MAX_ACTIVE_SUBPROCESS_OUTPUT_READERS
        );
        let error = match reserve_subprocess_output_reader() {
            Ok(_) => panic!("subprocess output reader budget must reject a new reservation"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(error.to_string().contains("reader/pipe resource budget"));
        drop(reservations);
        assert_eq!(active_subprocess_output_readers(), baseline);
    }

    #[test]
    fn token_matches_compares_tokens() {
        let token = "0123456789abcdef0123456789abcdef";
        assert!(token_matches(token, token));
        // One byte difference must fail.
        assert!(!token_matches(token, "0123456789abcdef0123456789abcdee"));
        // Length mismatch fails closed without panicking.
        assert!(!token_matches(token, "abc"));
        assert!(!token_matches("abc", token));
        // Empty tokens fail closed: a legitimate token is never empty.
        assert!(!token_matches("", ""));
        assert!(!token_matches(token, ""));
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn supervisor_deadline_uses_the_earlier_absolute_bound() {
        let started = std::time::Instant::now();
        let parent = started + std::time::Duration::from_secs(2);
        let execution = std::time::Duration::from_millis(20);
        let effective = effective_supervisor_deadline(started, Some(parent), Some(execution))
            .unwrap()
            .expect("one deadline must remain");
        assert_eq!(effective, started + execution);
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn overflowing_supervisor_deadline_is_rejected() {
        assert!(effective_supervisor_deadline(Instant::now(), None, Some(Duration::MAX)).is_err());
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn supervisor_capture_boundaries_are_exact_and_bounded() {
        let mut options = LibdenoOptions {
            capture_stdout: true,
            ..Default::default()
        };
        let request = supervisor_request(
            std::path::Path::new("entry.js"),
            &options,
            std::path::PathBuf::from("."),
        )
        .unwrap();
        assert_eq!(
            request.max_capture_bytes,
            Some(SUPERVISOR_CAPTURE_BYTES_PER_STREAM)
        );

        for value in [0, 17, SUPERVISOR_MAX_CAPTURE_BYTES_PER_STREAM] {
            options.max_capture_bytes = Some(value);
            let request = supervisor_request(
                std::path::Path::new("entry.js"),
                &options,
                std::path::PathBuf::from("."),
            )
            .unwrap();
            assert_eq!(request.max_capture_bytes, Some(value));
        }

        options.max_capture_bytes = Some(SUPERVISOR_MAX_CAPTURE_BYTES_PER_STREAM + 1);
        let error = supervisor_request(
            std::path::Path::new("entry.js"),
            &options,
            std::path::PathBuf::from("."),
        )
        .unwrap_err();
        assert!(matches!(error, LibdenoError::Configuration(_)));

        options.capture_stdout = false;
        options.max_capture_bytes = Some(usize::MAX);
        assert_eq!(
            supervisor_request(
                std::path::Path::new("entry.js"),
                &options,
                std::path::PathBuf::from("."),
            )
            .unwrap()
            .max_capture_bytes,
            None
        );
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn supervisor_terminal_validation_rejects_incoherent_result_and_capture() {
        let request = SupervisorRequest {
            entry: "entry.js".into(),
            cwd: ".".into(),
            permissions: Vec::new(),
            allow_all_permissions: true,
            prompt: false,
            args: Vec::new(),
            features: None,
            max_heap_bytes: None,
            execution_deadline: None,
            capture_stdout: true,
            capture_stderr: false,
            max_capture_bytes: Some(4),
        };
        assert!(validate_supervisor_terminal(
            &SupervisorTerminal {
                outcome: SupervisorOutcome::Completed,
                exit_code: None,
                category: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: false,
            },
            &request,
        )
        .is_err());
        assert!(validate_supervisor_terminal(
            &SupervisorTerminal {
                outcome: SupervisorOutcome::Cancelled,
                exit_code: Some(1),
                category: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: false,
            },
            &request,
        )
        .is_err());
        assert!(validate_supervisor_terminal(
            &SupervisorTerminal {
                outcome: SupervisorOutcome::Completed,
                exit_code: Some(0),
                category: None,
                stdout: vec![0; 5],
                stderr: Vec::new(),
                truncated: false,
            },
            &request,
        )
        .is_err());
        let error = validate_supervisor_terminal(
            &SupervisorTerminal {
                outcome: SupervisorOutcome::Completed,
                exit_code: Some(0),
                category: None,
                stdout: Vec::new(),
                stderr: vec![1],
                truncated: false,
            },
            &request,
        )
        .unwrap_err();
        assert!(error.to_string().contains("must not carry output"));
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn child_failure_categories_keep_host_failures_infrastructure() {
        assert_eq!(
            super::supervisor_child_failure_category(&LibdenoError::Permission(
                "permission details".to_string()
            )),
            SupervisorFailureCategory::Permission
        );
        assert_eq!(
            super::supervisor_child_failure_category(&LibdenoError::Runtime(
                deno_core::anyhow::anyhow!("runtime details")
            )),
            SupervisorFailureCategory::Runtime
        );
        assert_eq!(
            super::supervisor_child_failure_category(&LibdenoError::Configuration(
                "configuration details".to_string()
            )),
            SupervisorFailureCategory::Infrastructure
        );
        assert_eq!(
            super::supervisor_child_failure_category(&LibdenoError::Io(std::io::Error::other(
                "io details",
            ))),
            SupervisorFailureCategory::Infrastructure
        );
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn cleanup_failure_overrides_supervisor_cancellation() {
        let cancellation = SupervisorCancellation::new(
            crate::limits::CancellationContext::new(),
            CancelReason::User,
        );
        cancellation.request(CancelReason::User);
        let cleanup = Err(std::io::Error::other("synthetic reap failure"));
        assert_eq!(
            super::supervisor_session_failure_category(
                &LibdenoError::Io(std::io::Error::other("session failure")),
                &cleanup,
                &cancellation,
            ),
            Some(SupervisorFailureCategory::Infrastructure)
        );
    }

    #[cfg(unix)]
    #[cfg(feature = "execution-control")]
    #[test]
    fn no_terminal_reap_grace_distinguishes_natural_and_forced_exit() {
        let cancellation = SupervisorCancellation::new(
            crate::limits::CancellationContext::new(),
            CancelReason::User,
        );
        let session_error = LibdenoError::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "synthetic session EOF",
        ));

        let mut natural = std::process::Command::new("sh")
            .args(["-c", "sleep 0.02"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let natural_reap = super::reap_supervisor_child(&mut natural).unwrap();
        assert!(!natural_reap.forced_kill);
        let natural_category = super::supervisor_session_failure_category(
            &session_error,
            &Ok(natural_reap),
            &cancellation,
        );
        assert_eq!(
            natural_category,
            Some(SupervisorFailureCategory::ChildCrash)
        );

        let mut forced = std::process::Command::new("sh")
            .args(["-c", "sleep 5"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let forced_reap = super::reap_supervisor_child(&mut forced).unwrap();
        assert!(forced_reap.forced_kill);
        let forced_cancellation = SupervisorCancellation::new(
            crate::limits::CancellationContext::new(),
            CancelReason::Deadline,
        );
        forced_cancellation.request(CancelReason::Deadline);
        assert_eq!(
            super::supervisor_session_failure_category(
                &session_error,
                &Ok(forced_reap),
                &forced_cancellation,
            ),
            Some(SupervisorFailureCategory::Timeout)
        );

        let mut forced_user = std::process::Command::new("sh")
            .args(["-c", "sleep 5"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let forced_user_reap = super::reap_supervisor_child(&mut forced_user).unwrap();
        assert!(forced_user_reap.forced_kill);
        let forced_user_cancellation = SupervisorCancellation::new(
            crate::limits::CancellationContext::new(),
            CancelReason::User,
        );
        forced_user_cancellation.request(CancelReason::User);
        assert_eq!(
            super::supervisor_session_failure_category(
                &session_error,
                &Ok(forced_user_reap),
                &forced_user_cancellation,
            ),
            None,
            "a successful forced user cancellation is mapped to Cancelled by the executor"
        );
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn forced_child_cleanup_accepts_valid_terminal_but_natural_mismatch_fails() {
        let request = SupervisorRequest {
            entry: "entry.js".into(),
            cwd: ".".into(),
            permissions: Vec::new(),
            allow_all_permissions: true,
            prompt: false,
            args: Vec::new(),
            features: None,
            max_heap_bytes: None,
            execution_deadline: None,
            capture_stdout: false,
            capture_stderr: false,
            max_capture_bytes: None,
        };
        let status = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "exit", "0"])
                .status()
                .unwrap()
        } else {
            std::process::Command::new("sh")
                .args(["-c", "exit 0"])
                .status()
                .unwrap()
        };
        let completed = SupervisorTerminal {
            outcome: SupervisorOutcome::Completed,
            exit_code: Some(3),
            category: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
        };
        assert!(map_supervisor_terminal(completed.clone(), status, &request, false).is_err());
        let output = map_supervisor_terminal(completed, status, &request, true).unwrap();
        assert_eq!(output.exit_code, 3);

        for outcome in [
            SupervisorOutcome::Cancelled,
            SupervisorOutcome::Deadline,
            SupervisorOutcome::Failed,
        ] {
            let terminal = SupervisorTerminal {
                outcome,
                exit_code: None,
                category: (outcome == SupervisorOutcome::Failed)
                    .then_some(SupervisorFailureCategory::Runtime),
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: false,
            };
            assert!(
                map_supervisor_terminal(terminal.clone(), status, &request, false)
                    .unwrap_err()
                    .to_string()
                    .contains("disagrees")
            );
            assert!(!map_supervisor_terminal(terminal, status, &request, true)
                .unwrap_err()
                .to_string()
                .contains("disagrees"));
        }
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn cancellation_before_terminal_is_reconciled_before_completed_mapping() {
        let request = SupervisorRequest {
            entry: "entry.js".into(),
            cwd: ".".into(),
            permissions: Vec::new(),
            allow_all_permissions: true,
            prompt: false,
            args: Vec::new(),
            features: None,
            max_heap_bytes: None,
            execution_deadline: None,
            capture_stdout: false,
            capture_stderr: false,
            max_capture_bytes: None,
        };
        let status = if cfg!(windows) {
            std::process::Command::new("cmd")
                .args(["/C", "exit", "0"])
                .status()
                .unwrap()
        } else {
            std::process::Command::new("sh")
                .args(["-c", "exit 0"])
                .status()
                .unwrap()
        };
        let terminal = SupervisorTerminal {
            outcome: SupervisorOutcome::Completed,
            exit_code: Some(0),
            category: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
        };
        let deadline_error = super::map_supervisor_terminal_with_cancellation(
            terminal.clone(),
            status,
            &request,
            false,
            Some(CancelReason::Deadline),
        )
        .unwrap_err();
        assert!(matches!(deadline_error, LibdenoError::Timeout(_)));

        for reason in [CancelReason::User, CancelReason::Shutdown] {
            let error = super::map_supervisor_terminal_with_cancellation(
                terminal.clone(),
                status,
                &request,
                false,
                Some(reason),
            )
            .unwrap_err();
            assert!(!matches!(error, LibdenoError::Timeout(_)));
        }
        let output = super::map_supervisor_terminal_with_cancellation(
            terminal, status, &request, false, None,
        )
        .unwrap();
        assert_eq!(output.exit_code, 0);
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn post_start_reader_does_not_turn_cancellation_into_interrupted() {
        use std::io::Write;
        use std::net::{TcpListener, TcpStream};
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};

        let token =
            crate::supervisor::SupervisorToken::from_hex("00112233445566778899aabbccddeeff")
                .unwrap();
        let mut parent = SupervisorParentSession::new(1);
        parent
            .accept_hello(
                &SupervisorFrame::new(FrameKind::Hello, 0, token.as_bytes().to_vec()).unwrap(),
                &token,
            )
            .unwrap();
        parent.send_request().unwrap();
        parent
            .receive_child_frame(&SupervisorFrame::new(FrameKind::Accepted, 1, Vec::new()).unwrap())
            .unwrap();
        parent.send_start().unwrap();
        let state = Arc::new(Mutex::new(parent));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut frame = Vec::new();
            frame.extend_from_slice(b"LDSV");
            frame.push(SUPERVISOR_VERSION);
            frame.push(6);
            frame.extend_from_slice(&1u64.to_be_bytes());
            frame.extend_from_slice(&0u32.to_be_bytes());
            stream.write_all(&frame[..1]).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            let _ = stream.write_all(&frame[1..]);
        });
        let mut stream = TcpStream::connect(address).unwrap();
        let cancellation = SupervisorCancellation::new(
            crate::limits::CancellationContext::new(),
            CancelReason::User,
        );
        cancellation.request(CancelReason::User);
        let phase_deadline = Instant::now() + Duration::from_millis(25);
        let started = Instant::now();
        let error = read_supervisor_terminal(
            &mut stream,
            &state,
            None,
            phase_deadline,
            &cancellation,
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_millis(500));
        peer.join().unwrap();
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn deadline_cancel_accepts_terminal_after_delayed_started() {
        use std::net::{TcpListener, TcpStream};
        use std::time::{Duration, Instant};

        let token = SupervisorToken::from_hex("00112233445566778899aabbccddeeff").unwrap();
        let token_bytes = token.as_bytes().to_vec();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let peer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            let hello = SupervisorFrame::new(FrameKind::Hello, 0, token_bytes).unwrap();
            write_frame(&mut stream, &hello).unwrap();
            let request = read_frame(
                &mut stream,
                FrameDirection::ParentToChild,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(request.kind, FrameKind::Request);
            let accepted =
                SupervisorFrame::new(FrameKind::Accepted, request.request_id, Vec::new()).unwrap();
            write_frame(&mut stream, &accepted).unwrap();
            let start = read_frame(
                &mut stream,
                FrameDirection::ParentToChild,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(start.kind, FrameKind::Start);
            std::thread::sleep(Duration::from_millis(800));
            let started =
                SupervisorFrame::new(FrameKind::Started, request.request_id, Vec::new()).unwrap();
            write_frame(&mut stream, &started).unwrap();
            let cancel = read_frame(
                &mut stream,
                FrameDirection::ParentToChild,
                Instant::now() + Duration::from_secs(2),
            )
            .unwrap();
            assert_eq!(cancel.kind, FrameKind::Cancel);
            let terminal = SupervisorTerminal {
                outcome: SupervisorOutcome::Deadline,
                exit_code: None,
                category: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: false,
            };
            let terminal = SupervisorFrame::new(
                FrameKind::Terminal,
                request.request_id,
                encode_payload(&terminal).unwrap(),
            )
            .unwrap();
            write_frame(&mut stream, &terminal).unwrap();
        });

        let result = drive_supervisor_session(
            listener,
            Vec::new(),
            token,
            SupervisorCancellation::new(
                crate::limits::CancellationContext::new(),
                CancelReason::Deadline,
            ),
            Some(Instant::now() + Duration::from_millis(500)),
            None,
        )
        .unwrap();
        assert_eq!(
            result.transport_status,
            SupervisorTransportStatus::Clean,
            "cooperative TERMINAL must finish the worker cleanly"
        );
        assert_eq!(result.terminal.outcome, SupervisorOutcome::Deadline);
        peer.join().unwrap();
    }

    #[test]
    fn handshake_writer_budget_is_finite_and_releases_exactly() {
        let _lock = HANDSHAKE_WRITER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let baseline = active_handshake_writers();
        assert!(baseline <= MAX_ACTIVE_HANDSHAKE_WRITERS);
        let available = MAX_ACTIVE_HANDSHAKE_WRITERS - baseline;
        let mut reservations = Vec::with_capacity(available);
        for _ in 0..available {
            reservations.push(reserve_handshake_writer().unwrap());
        }
        assert_eq!(active_handshake_writers(), MAX_ACTIVE_HANDSHAKE_WRITERS);
        let error = match reserve_handshake_writer() {
            Ok(_) => panic!("handshake writer budget must reject a new reservation at its limit"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(error
            .to_string()
            .contains("handshake writer resource budget"));
        drop(reservations);
        assert_eq!(active_handshake_writers(), baseline);
    }

    #[cfg(unix)]
    #[test]
    fn handshake_writer_releases_after_normal_completion() {
        use std::process::Stdio;

        let _lock = HANDSHAKE_WRITER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let baseline = active_handshake_writers();
        let mut child = std::process::Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        write_child_request(
            &mut child,
            b"normal completion".to_vec(),
            ExecutionTiming::disabled(),
        )
        .unwrap();
        assert!(child.wait().unwrap().success());
        for _ in 0..100 {
            if active_handshake_writers() == baseline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(active_handshake_writers(), baseline);
    }

    #[cfg(unix)]
    #[test]
    fn handshake_writer_releases_after_write_error() {
        use std::process::Stdio;

        let _lock = HANDSHAKE_WRITER_BUDGET_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let baseline = active_handshake_writers();
        let mut child = std::process::Command::new("true")
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let _ = write_child_request(&mut child, vec![b'x'; 4096], ExecutionTiming::disabled());
        let _ = child.wait();
        for _ in 0..100 {
            if active_handshake_writers() == baseline {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(active_handshake_writers(), baseline);
    }
}
