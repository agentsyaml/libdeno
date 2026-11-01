//! Web worker failures: what the caller can see, and what only a `log` logger
//! sees.
//!
//! Upstream reports a worker's uncaught error with `log::error!` from
//! `run_web_worker`; libdeno installs no logger, so the detail is dropped and
//! the run fails with the generic message. These tests pin that contract so a
//! future change to either side is visible.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

/// Output capture is process-global, so these tests cannot overlap.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-worker-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_entry(dir: &Path, script: &str) -> PathBuf {
    let entry = dir.join("main.js");
    fs::write(&entry, script).unwrap();
    entry
}

fn allow_all() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    }
}

#[test]
fn unhandled_worker_error_reports_only_the_generic_message() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("generic");
    let entry = write_entry(
        &dir,
        "const src = 'function f(){ throw new Error(\"worker-boom-detail\"); } f();';\n\
         new Worker('data:application/javascript,' + encodeURIComponent(src), { type: 'module' });\n\
         await new Promise((r) => setTimeout(r, 300));",
    );
    let error = libdeno::run(&entry, &allow_all()).unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("Unhandled error in child worker"),
        "expected the generic worker message, got: {message}"
    );
    assert!(
        !message.contains("worker-boom-detail"),
        "the worker's own message must not appear in the run error; \
         it is only emitted through the `log` crate (see docs/api.md): {message}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn worker_onerror_handler_can_keep_the_run_alive() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("handler");
    let entry = write_entry(
        &dir,
        "const src = 'throw new Error(\"handled-in-worker\");';\n\
         const w = new Worker('data:application/javascript,' + encodeURIComponent(src), { type: 'module' });\n\
         w.onerror = (event) => { event.preventDefault(); console.log('handled:' + event.message); w.terminate(); };",
    );
    let output = libdeno::run_with_output(
        &entry,
        &LibdenoOptions {
            capture_stdout: true,
            ..allow_all()
        },
    )
    .expect("a prevented worker error must not fail the run");
    assert_eq!(output.exit_code, 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("handled:") && stdout.contains("handled-in-worker"),
        "the handler should see the worker's message, got: {stdout}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_live_worker_keeps_the_event_loop_alive() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("alive");
    let entry = write_entry(
        &dir,
        "new Worker('data:application/javascript,self.onmessage=()=>{}', { type: 'module' });\n\
         console.log('spawned');",
    );
    // Without a deadline this would hang forever, matching `deno run`.
    let options = LibdenoOptions {
        capture_stdout: true,
        execution_deadline: Some(std::time::Duration::from_millis(400)),
        ..allow_all()
    };
    let error = libdeno::run_with_output(&entry, &options).unwrap_err();
    assert!(
        matches!(error, libdeno::LibdenoError::Timeout(_)),
        "a live worker should be cut by the deadline, got: {error:?}"
    );
    let _ = fs::remove_dir_all(dir);
}
