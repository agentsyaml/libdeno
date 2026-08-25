//! Windows-only: output capture is rejected with a `Configuration` error
//! (Rust std's stdout/stderr on Windows write via GetStdHandle, bypassing
//! the CRT fd that dup2 redirects, so in-process capture cannot work there).
//! Runs only on the Windows CI leg.

#![cfg(windows)]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::PathBuf;

use libdeno::{run_in_subprocess_with_output, run_with_output, LibdenoOptions};

struct EnvVarGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

#[test]
fn capture_is_rejected_on_windows() {
    let entry = std::env::temp_dir().join(format!("libdeno-capture-win-{}.js", std::process::id()));
    std::fs::write(&entry, "console.log('x');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    };
    let err = run_with_output(&entry, &options).unwrap_err();
    assert!(
        err.to_string().contains("not supported on Windows"),
        "unexpected error: {err}"
    );
    let _ = std::fs::remove_file(&entry);
}

#[test]
fn subprocess_capture_returns_both_streams_and_exit_code_on_windows() {
    // The test harness is not a child-mode host, so use the dedicated binary
    // that calls maybe_handle_child_mode() before its normal main. This is
    // exercised by the Windows compatibility runner; non-Windows developers
    // cannot run this cfg-gated test locally.
    static HOST_EXE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _host_exe_lock = HOST_EXE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _host_exe = EnvVarGuard::set("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));

    let dir: PathBuf = std::env::temp_dir().join(format!(
        "libdeno-capture-win-subprocess-{}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "console.log('windows stdout'); console.error('windows stderr'); Deno.exit(23);",
    )
    .unwrap();

    let result = run_in_subprocess_with_output(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    );

    let _ = fs::remove_dir_all(&dir);

    let output = result.unwrap();
    assert_eq!(output.exit_code, 23);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("windows stdout"),
        "stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("windows stderr"),
        "stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}
