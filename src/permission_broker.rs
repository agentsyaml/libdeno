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
#[cfg(unix)]
use std::sync::OnceLock;

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
/// every permission check in the process — and must not panic (a panicking
/// hook closes the bridge, which terminates the process through the upstream
/// broker error path).
///
/// Re-entering any permission-checking code from the hook (e.g. calling
/// [`crate::run`] in-process, or any op that consults permissions) deadlocks
/// the process: the upstream check path blocks on the bridge while the bridge
/// thread is inside the hook waiting for that same check.
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
    // connect to `path` — upstream deno_permissions behavior, not ours.
    // Callers should probe the socket's existence first (e.g. via the
    // process-wide `has_broker()` guard after a successful connect, or by
    // checking the path before installing) to fail cleanly instead.
    let broker = PermissionBroker::new(path.as_ref());
    set_broker(broker);
    Ok(())
}

#[cfg(unix)]
static HOOK: OnceLock<PermissionPrompt> = OnceLock::new();

/// Installs an in-process permission hook — a plain `Fn(&PermissionRequest) ->
/// bool` deciding every check. Served internally through a temp-dir Unix
/// socket, so the host sees the same install-once / sole-authority semantics
/// as [`install_permission_broker`] without running an external process.
///
/// Unix only for now; on Windows use [`install_permission_broker`] with an
/// external broker process.
///
/// # Safety (fork)
///
/// Forking after installation without an immediate exec is unsafe: the child
/// inherits the installed broker state and the socket fd, but has no bridge
/// thread serving it, so the child's first permission check hangs forever.
/// The same applies to an external broker (both endpoints inherited, no
/// reader). Re-install the hook/broker in the child (or exec immediately).
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
fn random_suffix() -> String {
    // getrandom is already in the tree (child-mode auth token entropy).
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).expect("OS RNG is required to install a permission hook");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(unix)]
fn install_permission_hook_unix(hook: PermissionPrompt) -> Result<(), LibdenoError> {
    use std::os::unix::fs::PermissionsExt;
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
    // are unreachable litter, never colliding. Both endpoints are ours, so
    // the broker's connect cannot fail (PermissionBroker::new's exit(87)
    // path is unreachable here).
    //
    // The dir/file names are deliberately short: macOS caps Unix socket paths
    // at 104 bytes (SUN_LEN), and its temp dir is already long.
    let socket_dir = std::env::temp_dir().join(format!("libdeno-hook-{}", random_suffix()));
    std::fs::create_dir(&socket_dir).map_err(|e| {
        LibdenoError::Permission(format!(
            "failed to create hook socket dir {}: {e}",
            socket_dir.display()
        ))
    })?;
    std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700)).map_err(|e| {
        LibdenoError::Permission(format!(
            "failed to restrict hook socket dir {}: {e}",
            socket_dir.display()
        ))
    })?;
    let socket_path = socket_dir.join("h.sock");
    let listener = UnixListener::bind(&socket_path).map_err(|e| {
        LibdenoError::Permission(format!(
            "failed to bind hook socket {}: {e}",
            socket_path.display()
        ))
    })?;
    // Create the broker (which connects) before accepting: accept() blocks
    // until a client connects, and the only client is PermissionBroker::new
    // itself — accept-first would deadlock. The connect enqueues the
    // connection, so the accept below returns immediately.
    let broker = PermissionBroker::new(&socket_path);
    let stream = match listener.accept() {
        Ok((stream, _)) => stream,
        Err(e) => {
            return Err(LibdenoError::Permission(format!(
                "failed to accept hook broker connection: {e}"
            )))
        }
    };
    // The socket file and dir are no longer needed once the connection is
    // established (the connected stream stays usable): unlink now so no stale
    // socket survives process exit.
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_dir(&socket_dir);
    // The hook is recorded only after every fallible step above succeeded, so
    // no partial "hook set but broker missing" state can survive an error.
    HOOK.set(hook).map_err(|_| {
        LibdenoError::Permission(
            "a permission hook is already installed; it can only be installed once per process"
                .to_string(),
        )
    })?;
    set_broker(broker);
    std::thread::spawn(move || {
        serve_broker_connection(stream, HOOK.get().expect("hook was just set"))
    });
    Ok(())
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

#[cfg(unix)]
fn serve_broker_connection(stream: std::os::unix::net::UnixStream, hook: &PermissionPrompt) {
    use std::io::BufRead;
    use std::io::BufReader;
    use std::io::Write;
    let reader_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("libdeno: permission hook bridge: {e}");
            return;
        }
    };
    let mut reader = BufReader::new(reader_stream);
    let mut writer = stream;
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break, // broker disconnected (EOF / error)
            Ok(_) => {}
        }
        let response = match deno_core::serde_json::from_str::<BrokerRequest>(line.trim()) {
            Ok(req) => {
                let allow = (hook)(&PermissionRequest {
                    name: req.permission,
                    value: req.value,
                });
                BrokerResponseLine {
                    id: req.id,
                    result: if allow { "allow" } else { "deny" },
                    reason: None,
                }
            }
            Err(_) => {
                // Unreachable in practice (both endpoints are ours). A
                // well-formed deny response keeps the upstream broker reading;
                // a protocol mismatch ends in the upstream exit(87) path
                // either way — this yields the deterministic "ID mismatch"
                // error rather than a parse failure on EOF.
                BrokerResponseLine {
                    id: 0,
                    result: "deny",
                    reason: None,
                }
            }
        };
        let msg = format!("{}\n", deno_core::serde_json::to_string(&response).unwrap());
        if writer.write_all(msg.as_bytes()).is_err() {
            break;
        }
    }
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use super::*;

    /// Sends one request over a paired Unix stream, feeds the server side to
    /// `serve_broker_connection`, and returns the echoed id + result.
    fn round_trip(hook: PermissionPrompt) -> (u32, String) {
        use std::io::BufRead;
        use std::io::BufReader;
        use std::io::Write;
        let (mut client, server) = std::os::unix::net::UnixStream::pair().unwrap();
        let hook = hook.clone(); // the bridge thread needs a 'static handle
        std::thread::spawn(move || serve_broker_connection(server, &hook));
        let req = r#"{"v":1,"pid":123,"id":7,"datetime":"2026-08-13T00:00:00Z","permission":"read","value":"/etc/passwd"}"#;
        client.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut reader = BufReader::new(&client);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let resp: deno_core::serde_json::Value =
            deno_core::serde_json::from_str(line.trim()).unwrap();
        (
            resp["id"].as_u64().unwrap() as u32,
            resp["result"].as_str().unwrap().to_string(),
        )
    }

    #[test]
    fn hook_bridge_allows_and_denies() {
        // Allow-hook: the response echoes the request id (the upstream broker
        // validates id matching) and says allow.
        let (id, result) = round_trip(Arc::new(|_req: &PermissionRequest| true));
        assert_eq!(id, 7);
        assert_eq!(result, "allow");
        // Deny-hook.
        let (id, result) = round_trip(Arc::new(|_req: &PermissionRequest| false));
        assert_eq!(id, 7);
        assert_eq!(result, "deny");
    }
}
