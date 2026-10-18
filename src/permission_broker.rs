//! Permission broker hooks (process-global, install-once).
//!
//! Exposes deno_permissions' broker mechanism — the same channel the deno CLI
//! uses for jupyter/LSP permission decisions. Once a broker (or the in-process
//! hook built on it) is installed, it is the **sole authority** for every
//! permission check in the process: even already-granted capabilities are
//! delegated to it (upstream deno semantics), so local `--allow-*` flags are
//! no longer consulted.

use std::path::Path;
use std::sync::Arc;

use deno_runtime::deno_permissions::broker::has_broker;
use deno_runtime::deno_permissions::broker::set_broker;
use deno_runtime::deno_permissions::broker::PermissionBroker;

use crate::LibdenoError;

/// A single permission decision handed to a hook: the capability name and the
/// stringified access value (path, host, env name, …; None for unary checks).
#[derive(Debug, Clone)]
pub struct PermissionRequest {
    pub name: String,
    pub value: Option<String>,
}

/// Permission decision hook: return `true` to allow, `false` to deny.
///
/// Called synchronously from the permission-check path on the broker bridge
/// thread: it must return quickly and must not block — a blocked hook stalls
/// every permission check in the process. Unwinding panics are caught at the
/// bridge boundary and fail closed as a deny response. This does not apply to
/// `panic = "abort"` builds or panic hooks that abort or exit the process:
/// `catch_unwind` cannot capture those, and they may terminate the host. Hooks
/// still must not panic because the panic hook may log noisy diagnostics and
/// the callback is a security boundary for the decision.
///
/// Re-entering any permission-checking code from the hook (e.g. calling
/// [`crate::run`] in-process, or any op that consults permissions) deadlocks
/// the process: the upstream check path blocks on the bridge while the bridge
/// thread is inside the hook waiting for that same check.
///
/// The hook is invoked inline. There is intentionally no detached timeout
/// worker: an arbitrary `Fn` cannot be safely cancelled, and leaving it alive
/// after a timeout would let it retain state or make a decision after the
/// request was closed. A hook that does not return therefore produces no allow
/// response and stalls the upstream check, as documented above.
///
/// A blocked hook also defeats `execution_deadline`: the permission-check
/// thread is parked in the bridge read, so `terminate_execution` has no JS
/// stack to throw into and the event loop (and its timers) are never polled.
/// The run keeps going past the deadline until the hook returns.
pub type PermissionPrompt = Arc<dyn Fn(&PermissionRequest) -> bool + Send + Sync>;

/// Serializes broker installation: both install functions check `has_broker()`
/// and then call `set_broker()`, and upstream `set_broker` asserts (panics) on
/// a second install — a concurrent double-install must not reach that assert.
static INSTALL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(crate) fn is_installed() -> bool {
    has_broker()
}

/// Installs an external permission broker process at `socket_path` (raw
/// deno_permissions capability): a Unix socket (or Windows named pipe) serving
/// the JSON-line protocol — a request `{v, pid, id, datetime, permission,
/// value}` line, a response `{id, result: "allow"|"deny", reason}` line.
///
/// Process-global and install-once; every subsequent permission check in this
/// process — granted or not — is delegated to the broker. Works across
/// `run_in_subprocess` children when the host binary installs it in `main()`
/// before `maybe_handle_child_mode()`. A second install returns an error.
///
/// `Result` covers only failures libdeno can report, such as duplicate
/// installation or local setup errors. It does **not** make the upstream
/// connection operation catchable: `deno_permissions::broker::PermissionBroker::new`
/// returns `Self` and may call `process::exit(87)` when the initial connection
/// fails. Later broker/bridge communication failures may also terminate the
/// process through the upstream error path, and the caller cannot catch either
/// kind of termination. Do not use this API with an untrusted or unreliable
/// endpoint, or where the host is required to remain alive after broker
/// failure.
///
/// Note: the broker decides checks, not construction — an empty `permissions`
/// list without `prompt: true` (or flags / `allow_all_permissions`) still
/// fails at `run` construction time before any check reaches the broker.
pub fn install_permission_broker(path: impl AsRef<Path>) -> Result<(), LibdenoError> {
    let _install = INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if has_broker() {
        return Err(LibdenoError::Permission(
            "a permission broker is already installed; it can only be installed once per process"
                .to_string(),
        ));
    }
    // NOTE: PermissionBroker::new exits the process (code 87) if it cannot
    // connect to `path` — upstream deno_permissions behavior, not ours. The
    // upstream constructor is not try_new, so this cannot be converted into a
    // catchable LibdenoError here; later bridge failures have the same external
    // termination risk.
    let broker = PermissionBroker::new(path.as_ref());
    set_broker(broker);
    Ok(())
}

#[cfg(unix)]
struct HookSocketDir {
    dir: std::path::PathBuf,
    socket: std::path::PathBuf,
}

#[cfg(unix)]
struct BrokerThreadGuard {
    start: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl BrokerThreadGuard {
    fn commit(mut self) {
        self.start.take().unwrap().send(()).unwrap();
        drop(self.thread.take());
    }
}

#[cfg(unix)]
impl Drop for BrokerThreadGuard {
    fn drop(&mut self) {
        self.start.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(unix)]
impl Drop for HookSocketDir {
    fn drop(&mut self) {
        // Best-effort cleanup is limited to this install's socket and directory.
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// Installs an in-process permission hook — a plain `Fn(&PermissionRequest) ->
/// bool` deciding every check. Served internally through a temp-dir Unix
/// socket, so the host sees the same install-once / sole-authority semantics
/// as [`install_permission_broker`] without running an external process.
///
/// Unix only for now; on Windows use [`install_permission_broker`] with an
/// external broker process.
///
/// The hook uses the same upstream broker bridge as an external broker. It
/// establishes its local socket connection before publishing the broker and
/// returns thread-start failures as `LibdenoError`; later bridge failures still
/// follow upstream's non-recoverable error path. The install lock serializes
/// this crate's install APIs, but cannot serialize direct upstream
/// `set_broker` calls made concurrently by other code.
///
/// # Safety (fork)
///
/// Forking after installation without an immediate exec is unsafe: the child
/// inherits broker connection state that must not be reused after fork. For an
/// in-process hook, the serving thread does not survive, so permission checks
/// hang. Exec immediately, then install the broker or hook during trusted
/// startup in the new process; reinstalling in the fork child is not possible.
pub fn install_permission_hook(hook: PermissionPrompt) -> Result<(), LibdenoError> {
    #[cfg(not(unix))]
    {
        let _ = hook;
        Err(LibdenoError::Permission(
            "install_permission_hook is not supported on this platform yet; \
             use install_permission_broker with an external broker process"
                .to_string(),
        ))
    }
    #[cfg(unix)]
    {
        install_permission_hook_unix(hook)
    }
}

#[cfg(unix)]
fn random_suffix() -> Result<String, LibdenoError> {
    // getrandom is already in the tree (child-mode auth token entropy).
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn install_permission_hook_unix(hook: PermissionPrompt) -> Result<(), LibdenoError> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;

    let _install = INSTALL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if has_broker() {
        return Err(LibdenoError::Permission(
            "a permission broker is already installed; install_permission_broker and \
             install_permission_hook are mutually exclusive and both install-once"
                .to_string(),
        ));
    }
    // Private 0700 dir under temp: the socket file inherits the directory's
    // permissions, so a world-writable /tmp (Linux, default umask would
    // otherwise leave the socket world-connectable) cannot expose the hook to
    // other local users — who could query it as a policy oracle or race the
    // connect window to starve the real broker connection. The random suffix
    // blocks pre-creation of a predictable path (a local DoS of this opt-in
    // install); no pid component is needed. Stale dirs from crashed installs
    // are unreachable litter, never colliding. The listener and path are
    // created by this process, which lowers normal connection-failure risk,
    // but local resource/connection errors can still reach the upstream
    // constructor and its uncapturable process::exit(87) path.
    //
    // The dir/file names are deliberately short: macOS caps Unix socket paths
    // at 104 bytes (SUN_LEN), and its temp dir is already long.
    let socket_dir = std::env::temp_dir().join(format!("libdeno-hook-{}", random_suffix()?));
    let socket_path = socket_dir.join("h.sock");
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(&socket_dir).map_err(|e| {
        LibdenoError::Permission(format!(
            "failed to create hook socket dir {}: {e}",
            socket_dir.display()
        ))
    })?;
    let _cleanup = HookSocketDir {
        dir: socket_dir,
        socket: socket_path.clone(),
    };
    let listener = UnixListener::bind(&socket_path).map_err(|e| {
        LibdenoError::Permission(format!(
            "failed to bind hook socket {}: {e}",
            socket_path.display()
        ))
    })?;
    // PermissionBroker::new connects to this listener, so construct it before
    // accept; accepting first would wait forever for this same connection.
    let broker = PermissionBroker::new(&socket_path);
    let stream = match listener.accept() {
        Ok((stream, _)) => stream,
        Err(e) => {
            return Err(LibdenoError::Permission(format!(
                "failed to accept hook broker connection: {e}"
            )))
        }
    };
    let (start_tx, start_rx) = std::sync::mpsc::channel();
    let thread = spawn_hook_thread(move || {
        if start_rx.recv().is_ok() {
            serve_broker_connection(stream, &hook);
        }
    })
    .map_err(LibdenoError::Io)?;
    let worker = BrokerThreadGuard {
        start: Some(start_tx),
        thread: Some(thread),
    };
    set_broker(broker);
    worker.commit();
    Ok(())
}

#[cfg(all(unix, test))]
static FAIL_HOOK_THREAD_SPAWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
fn spawn_hook_thread<F>(run: F) -> std::io::Result<std::thread::JoinHandle<()>>
where
    F: FnOnce() + Send + 'static,
{
    #[cfg(test)]
    if FAIL_HOOK_THREAD_SPAWN.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err(std::io::Error::other(
            "injected permission-hook thread spawn failure",
        ));
    }
    std::thread::Builder::new()
        .name("libdeno-permission-hook".to_string())
        .spawn(run)
}

// The broker JSON-line protocol, mirrored from deno_permissions' broker.rs
// (serde camelCase both ways). `datetime`/`pid`/`v` are echo-only on our side:
// we only need `id` to reply on the right request and `permission`/`value` to
// consult the hook.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)] // v/pid/datetime are wire-protocol fields, parsed for shape only
struct BrokerRequest {
    v: u32,
    pid: u32,
    id: u32,
    datetime: String,
    permission: String,
    value: Option<String>,
}

#[cfg(unix)]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct BrokerResponseLine {
    id: u32,
    result: &'static str,
    reason: Option<String>,
}

/// Maximum bytes in one newline-delimited broker request, including the
/// newline. `deno_permissions` currently reads its response with an
/// unbounded `read_line`; this local hook endpoint must bound the request it
/// accepts so an untrusted peer cannot make the bridge allocate forever.
#[cfg(unix)]
const MAX_BROKER_LINE_BYTES: usize = 64 * 1024;

#[cfg(unix)]
enum BrokerLine {
    Request(Vec<u8>),
    Oversized(Option<u32>),
}

#[cfg(unix)]
fn broker_request_id(prefix: &[u8], allow_truncated_eof: bool) -> Option<u32> {
    use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
    struct IdSeed<'a>(&'a mut Option<u32>);
    impl<'de> DeserializeSeed<'de> for IdSeed<'_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
            struct IdVisitor<'a>(&'a mut Option<u32>);
            impl<'de> Visitor<'de> for IdVisitor<'_> {
                type Value = ();
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("a broker request object")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
                    while let Some(key) = map.next_key::<String>()? {
                        if key == "id" {
                            if self.0.is_some() {
                                *self.0 = None;
                                return Err(serde::de::Error::custom("duplicate id"));
                            }
                            *self.0 = Some(map.next_value::<u32>()?);
                        } else {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                    Ok(())
                }
            }
            deserializer.deserialize_map(IdVisitor(self.0))
        }
    }
    let mut id = None;
    let mut deserializer = deno_core::serde_json::Deserializer::from_slice(prefix);
    match IdSeed(&mut id).deserialize(&mut deserializer) {
        Ok(()) if deserializer.end().is_ok() => id,
        Err(error) if allow_truncated_eof && error.is_eof() => id,
        _ => None,
    }
}

#[cfg(unix)]
fn read_broker_line<R: std::io::BufRead>(reader: &mut R) -> std::io::Result<Option<BrokerLine>> {
    let mut prefix = Vec::with_capacity(MAX_BROKER_LINE_BYTES.min(4096));
    let mut oversized = false;
    loop {
        let available = match reader.fill_buf() {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if available.is_empty() {
            if prefix.is_empty() {
                return Ok(None);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "unterminated broker request",
            ));
        }
        let count = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |i| i + 1);
        if !oversized {
            let room = MAX_BROKER_LINE_BYTES + 1 - prefix.len();
            prefix.extend_from_slice(&available[..count.min(room)]);
            oversized = prefix.len() > MAX_BROKER_LINE_BYTES;
        }
        let newline = available[..count].last() == Some(&b'\n');
        reader.consume(count);
        if newline {
            return if oversized {
                Ok(Some(BrokerLine::Oversized(broker_request_id(
                    &prefix, true,
                ))))
            } else {
                Ok(Some(BrokerLine::Request(prefix)))
            };
        }
    }
}

#[cfg(unix)]
fn serve_broker_connection(stream: std::os::unix::net::UnixStream, hook: &PermissionPrompt) {
    use std::io::BufReader;
    use std::io::Write;
    let mut reader = BufReader::new(stream);
    while let Ok(Some(line)) = read_broker_line(&mut reader) {
        let (id, request) = match line {
            BrokerLine::Oversized(Some(id)) => (id, None),
            BrokerLine::Oversized(None) => break,
            BrokerLine::Request(line) => {
                match deno_core::serde_json::from_slice::<BrokerRequest>(&line) {
                    Ok(req) => (req.id, Some((req.permission, req.value))),
                    Err(_) => match broker_request_id(&line, false) {
                        Some(id) => (id, None),
                        None => break,
                    },
                }
            }
        };
        let allow = if let Some((name, value)) = request {
            let request = PermissionRequest { name, value };
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (hook)(&request))) {
                Ok(allow) => allow,
                Err(_) => {
                    let mut stderr = std::io::stderr().lock();
                    let _ = writeln!(
                        stderr,
                        "libdeno: permission hook panicked; denying the request"
                    );
                    false
                }
            }
        } else {
            false
        };
        let response = BrokerResponseLine {
            id,
            result: if allow { "allow" } else { "deny" },
            reason: None,
        };
        let msg = format!("{}\n", deno_core::serde_json::to_string(&response).unwrap());
        if reader.get_mut().write_all(msg.as_bytes()).is_err() {
            break;
        }
    }
}

#[cfg(all(unix, test))]
#[path = "permission_broker/tests.rs"]
mod tests;
