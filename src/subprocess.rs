// Subprocess execution mode: runs a script in a child process so that
// `Deno.exit(n)` or a hard runtime failure terminates only the child while
// the host process keeps running.

use std::path::Path;
use std::path::PathBuf;

use crate::limits::LIBDENO_SPAWNED_IPC;
use crate::run;
use crate::LibdenoError;
use crate::LibdenoOptions;

/// Environment variable marking a process spawned by [`run_in_subprocess`].
const LIBDENO_CHILD_MODE: &str = "LIBDENO_CHILD_MODE";

/// Environment variable carrying the per-run auth token a child must present
/// to prove it was spawned by [`run_in_subprocess`].
const LIBDENO_CHILD_TOKEN: &str = "LIBDENO_CHILD_TOKEN";

/// Environment variable overriding the executable [`run_in_subprocess`]
/// spawns (defaults to the current executable). Lets tests point the child
/// run at a dedicated host binary.
const LIBDENO_HOST_EXE: &str = "LIBDENO_HOST_EXE";

/// Request payload serialized to the child process's stdin by
/// [`run_in_subprocess`]. The `token` must match the [`LIBDENO_CHILD_TOKEN`]
/// environment variable the parent set on the child; without it the child
/// refuses to run.
#[derive(serde::Serialize, serde::Deserialize)]
struct ChildRunRequest {
    entry: String,
    permissions: Vec<String>,
    args: Vec<String>,
    cwd: PathBuf,
    /// Per-run auth token, verified against [`LIBDENO_CHILD_TOKEN`].
    token: String,
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

/// Runs `entry` in a child process and returns its exit code.
///
/// The script runs inside a subprocess, so `Deno.exit(n)` or a hard runtime
/// failure terminates only the child — the host process keeps running. The
/// host binary must call [`maybe_handle_child_mode`] at the very start of its
/// `main()` for the child request to be serviced.
///
/// The child inherits stdout/stderr, so script output still appears. Entry,
/// permissions, args and cwd are passed over stdin as JSON, together with a
/// fresh per-run auth `token`. The same token is handed to the child via the
/// `LIBDENO_CHILD_TOKEN` environment variable; the child refuses to run
/// unless the request token matches, so a process that can set
/// `LIBDENO_CHILD_MODE` and write the child's stdin cannot inject a request
/// of its own.
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
    // Take CWD_LOCK only around the cwd read + spawn (both fast): a
    // concurrent in-process run() may switch the process-global cwd
    // mid-flight, and pinning the child's cwd from a stale read would run it
    // in the wrong tree. The lock is NOT held across child.wait() — that
    // would deadlock hosts that run long-lived children (plugins/daemons):
    // the first child would hold the process-global lock forever and block
    // every later run_in_subprocess or run() call in the same process.
    let token = child_token()?;
    let (payload, mut child) = {
        let _lock = crate::CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let cwd = options.cwd.clone().unwrap_or(std::env::current_dir()?);
        let request = ChildRunRequest {
            entry: entry.as_ref().to_string_lossy().into_owned(),
            permissions: options.permissions.clone(),
            args: options.args.clone(),
            cwd: cwd.clone(),
            token: token.clone(),
        };
        let payload = deno_core::serde_json::to_vec(&request)
            .map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))?;
        let exe = std::env::var_os(LIBDENO_HOST_EXE)
            .map(PathBuf::from)
            .unwrap_or(std::env::current_exe()?);
        let child = std::process::Command::new(exe)
            .env(LIBDENO_CHILD_MODE, "1")
            .env(LIBDENO_SPAWNED_IPC, "1")
            .env(LIBDENO_CHILD_TOKEN, &token)
            .current_dir(&cwd)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(LibdenoError::Io)?;
        (payload, child)
    };
    {
        use std::io::Write;
        let write_result = match child.stdin.as_mut() {
            Some(stdin) => stdin.write_all(&payload).map_err(LibdenoError::Io),
            None => Err(LibdenoError::Runtime(deno_core::anyhow::anyhow!(
                "child has no stdin"
            ))),
        };
        if let Err(e) = write_result {
            // Don't leak the child (or a zombie) on this error path: it may be
            // blocked reading stdin or already dead.
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
    }
    // Close the child's stdin so a script reading process.stdin sees EOF
    // instead of blocking forever on the still-open pipe.
    drop(child.stdin.take());
    let status = child.wait().map_err(LibdenoError::Io)?;
    Ok(status.code().unwrap_or(1))
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
        let request: ChildRunRequest = deno_core::serde_json::from_reader(std::io::stdin())
            .map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))?;
        if request.token != env_token.to_string_lossy() {
            return Err(LibdenoError::Runtime(deno_core::anyhow::anyhow!(
                "child request token does not match {LIBDENO_CHILD_TOKEN}"
            )));
        }
        let options = LibdenoOptions {
            permissions: request.permissions,
            args: request.args,
            cwd: Some(request.cwd),
            ..Default::default()
        };
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
