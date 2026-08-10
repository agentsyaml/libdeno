// Subprocess execution mode: runs a script in a child process so that
// `Deno.exit(n)` or a hard runtime failure terminates only the child while
// the host process keeps running.

use std::path::Path;
use std::path::PathBuf;

use crate::run;
use crate::LibdenoError;
use crate::LibdenoOptions;

/// Environment variable marking a process spawned by [`run_in_subprocess`].
const LIBDENO_CHILD_MODE: &str = "LIBDENO_CHILD_MODE";

/// Environment variable overriding the executable [`run_in_subprocess`]
/// spawns (defaults to the current executable). Lets tests point the child
/// run at a dedicated host binary.
const LIBDENO_HOST_EXE: &str = "LIBDENO_HOST_EXE";

/// Request payload serialized to the child process's stdin by
/// [`run_in_subprocess`].
#[derive(serde::Serialize, serde::Deserialize)]
struct ChildRunRequest {
    entry: String,
    permissions: Vec<String>,
    args: Vec<String>,
    cwd: Option<PathBuf>,
}

/// Runs `entry` in a child process and returns its exit code.
///
/// The script runs inside a subprocess, so `Deno.exit(n)` or a hard runtime
/// failure terminates only the child — the host process keeps running. The
/// host binary must call [`maybe_handle_child_mode`] at the very start of its
/// `main()` for the child request to be serviced.
///
/// The child inherits stdout/stderr, so script output still appears. Entry,
/// permissions, args and cwd are passed over stdin as JSON.
pub fn run_in_subprocess(
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
) -> Result<i32, LibdenoError> {
    let cwd = options.cwd.clone().unwrap_or(std::env::current_dir()?);
    let request = ChildRunRequest {
        entry: entry.as_ref().to_string_lossy().into_owned(),
        permissions: options.permissions.clone(),
        args: options.args.clone(),
        cwd: Some(cwd),
    };
    let payload = deno_core::serde_json::to_vec(&request)
        .map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))?;

    let exe = std::env::var_os(LIBDENO_HOST_EXE)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_exe()?);

    let mut child = std::process::Command::new(exe)
        .env(LIBDENO_CHILD_MODE, "1")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(LibdenoError::Io)?;
    {
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .ok_or_else(|| LibdenoError::Runtime(deno_core::anyhow::anyhow!("child has no stdin")))?
            .write_all(&payload)
            .map_err(LibdenoError::Io)?;
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
pub fn maybe_handle_child_mode() -> bool {
    if std::env::var_os(LIBDENO_CHILD_MODE).is_none() {
        return false;
    }
    let result: Result<i32, LibdenoError> = (|| {
        let request: ChildRunRequest = deno_core::serde_json::from_reader(std::io::stdin())
            .map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))?;
        let options = LibdenoOptions {
            permissions: request.permissions,
            args: request.args,
            cwd: request.cwd,
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
