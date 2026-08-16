//! Windows-only: output capture is rejected with a `Configuration` error
//! (Rust std's stdout/stderr on Windows write via GetStdHandle, bypassing
//! the CRT fd that dup2 redirects, so in-process capture cannot work there).
//! Runs only on the Windows CI leg.

#![cfg(windows)]

use libdeno::{run_with_output, LibdenoOptions};

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
