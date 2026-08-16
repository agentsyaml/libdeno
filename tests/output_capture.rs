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

/// Capture is process-global (fd redirection) and exclusive: a captured run
/// rejects any concurrent run. `cargo test` runs tests in parallel, so every
/// capture test must take this test-level mutex — the library's own
/// concurrency rules, enforced at the test-suite level.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-cap-{}-{}", std::process::id(), name));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn capture_stdout_gets_console_log() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

#[test]
fn capture_truncates_at_max_capture_bytes() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // A verbose script must not grow host memory without limit: the byte cap
    // stops capture and flags the truncation instead.
    let dir = temp_dir("cap");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "for (let i = 0; i < 1000; i++) { console.log('x'.repeat(100)); }",
    )
    .unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        max_capture_bytes: Some(256),
        ..Default::default()
    };
    let out = run_with_output(&entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.len() <= 256,
        "captured {} bytes, expected <= 256",
        out.stdout.len()
    );
    assert!(out.capture_truncated, "truncation must be flagged");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn capture_within_budget_is_not_truncated() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("fit");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('small');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        max_capture_bytes: Some(1 << 20),
        ..Default::default()
    };
    let out = run_with_output(&entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("small\n"),
        "captured stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!out.capture_truncated, "small output must not truncate");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn run_with_reuses_stack_and_captures() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The reusable-runtime path must honor capture too: long-lived hosts get
    // both stack reuse and per-run captured output via
    // libdeno::runtime::run_with_output.
    let dir = temp_dir("rt-out");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('runtime capture');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = rt
        .block_on(libdeno::runtime::LibdenoRuntime::new(&dir))
        .unwrap();
    let out = libdeno::runtime::run_with_output(&runtime, &entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("runtime capture\n"),
        "captured stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    // A second run must reuse the stack and capture again.
    let out2 = libdeno::runtime::run_with_output(&runtime, &entry, &options).unwrap();
    assert!(
        String::from_utf8_lossy(&out2.stdout).contains("runtime capture\n"),
        "second run captured stdout: {:?}",
        String::from_utf8_lossy(&out2.stdout)
    );
    let _ = fs::remove_dir_all(&dir);
}
