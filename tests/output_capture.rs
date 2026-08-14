//! Output capture and tokio-context embedding tests.
//!
//! Regression guards for downstream reports:
//! - run() errored when called from inside a tokio runtime (async hosts had
//!   to build a std::thread::spawn + join escape themselves); it now routes
//!   onto a fresh thread automatically.
//! - run() had no way to capture the script's stdout/stderr; embedders were
//!   forced into file-exchange protocols. fd-level redirection + RunOutput
//!   close that gap.
//!
//! Windows skip: capture redirects the CRT fd, but Rust std's stdout/stderr
//! on Windows write via GetStdHandle and bypass it — the feature is rejected
//! with a Configuration error there (see run_sync_output), so the capture
//! tests are unix-only.
#![cfg(not(windows))]

use std::fs;
use std::path::PathBuf;

use libdeno::{run, run_with_output, LibdenoOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-cap-{}-{}", std::process::id(), name));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn capture_stdout_gets_console_log() {
    // The redirection is fd-level and process-global, so parallel test-harness
    // reports ("test ... ok") can land in the buffer too; assert the script's
    // line is present rather than exact equality.
    let dir = temp_dir("out");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('hello capture');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    };
    let out = run_with_output(&entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("hello capture\n"),
        "captured stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn capture_stderr_gets_console_error() {
    // Same contains-style assertion as the stdout test: the capture is
    // process-global, so in-process harness/panic output can land in it.
    let dir = temp_dir("err");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.error('oops');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stderr: true,
        ..Default::default()
    };
    let out = run_with_output(&entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("oops\n"),
        "captured stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn without_capture_output_is_empty() {
    let dir = temp_dir("nocap");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('not captured');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let out = run_with_output(&entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.is_empty(),
        "stdout must not be captured by default"
    );
    assert!(out.stderr.is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn run_works_from_inside_tokio_context() {
    // Regression: run() used to error with "cannot be called from inside a
    // tokio runtime", forcing async hosts to spawn a thread themselves. It
    // now does that internally; the run must still complete and the script's
    // output must still be capturable.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let code = rt.block_on(async {
        let dir = temp_dir("tokio");
        let entry = dir.join("main.js");
        fs::write(&entry, "console.log('tokio ok'); Deno.exit(0);").unwrap();
        let options = LibdenoOptions {
            allow_all_permissions: true,
            capture_stdout: true,
            ..Default::default()
        };
        let out = run_with_output(&entry, &options).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("tokio ok\n"),
            "capture must work in tokio path, got: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        out.exit_code
    });
    assert_eq!(code, 0);
}

#[test]
fn run_inside_tokio_keeps_error_reporting() {
    // Errors must still surface (not panic) when routed through the tokio path.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = temp_dir("tokio-err");
        let entry = dir.join("main.js");
        fs::write(&entry, "throw new Error('boom from tokio');").unwrap();
        let err = run(
            &entry,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("boom from tokio"),
            "unexpected error: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    });
}
