//! Async entry points: `run_async` / `run_with_output_async` execute the run
//! on the caller's tokio runtime (no spawned thread). The returned future is
//! not `Send` (deno's worker stack is Rc-based), so tests use a
//! current-thread runtime, and the multi-thread case exercises the
//! documented `LocalSet` pattern.

use std::fs;
use std::future::Future;
use std::path::PathBuf;

use libdeno::{run_async, run_with_output_async, LibdenoOptions};

/// The capture test's exclusivity lease rejects any concurrent run, so tests
/// in this file (which cargo test runs in parallel) must take this lock —
/// the library's concurrency rules, enforced at the test-suite level.
static FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-async-{}-{}", std::process::id(), name));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn async_run_executes_script() {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("basic");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "Deno.writeTextFileSync(new URL('./out.txt', import.meta.url), 'async-ok');",
    )
    .unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let code = rt
        .block_on(run_async(
            &entry,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            },
        ))
        .unwrap();
    assert_eq!(code, 0);
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "async-ok");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn async_run_reports_script_error() {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("err");
    let entry = dir.join("main.js");
    fs::write(&entry, "throw new Error('async-boom');").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let err = rt
        .block_on(run_async(
            &entry,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            },
        ))
        .unwrap_err();
    assert!(
        err.to_string().contains("async-boom"),
        "expected the script error, got: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn async_capture_returns_output() {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("cap");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('async-hello');").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = rt
        .block_on(run_with_output_async(
            &entry,
            &LibdenoOptions {
                allow_all_permissions: true,
                capture_stdout: true,
                ..Default::default()
            },
        ))
        .unwrap();
    assert_eq!(output.exit_code, 0);
    assert!(
        output.stdout.windows(11).any(|w| w == b"async-hello"),
        "captured stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let _ = fs::remove_dir_all(&dir);
}

/// Interleaving two run_async futures on one thread aborts the process (v8
/// pins the isolate to its creating thread); the thread-local guard turns
/// that crash into a recoverable Configuration error, and the guard clears
/// when the first future is dropped (cancelled).
#[test]
fn async_interleave_is_rejected_and_guard_releases_on_drop() {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("interleave");
    let entry = dir.join("main.js");
    fs::write(&entry, "await new Promise(r => setTimeout(r, 5000));").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let err = rt.block_on(async {
        // Poll the first run_async once (it acquires the thread-local guard,
        // then parks on the run's await chain).
        let mut fut = Box::pin(run_async(&entry, &options));
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        assert!(fut.as_mut().poll(&mut cx).is_pending());
        // A second run_async on the same thread must be rejected cleanly.
        let err = run_async(&entry, &options).await.unwrap_err();
        drop(fut); // cancelling the first future releases the guard
        err
    });
    assert!(
        matches!(err, libdeno::LibdenoError::Configuration(_)),
        "expected Configuration rejection, got: {err}"
    );
    // Guard released: a fresh run_async succeeds after the cancel.
    let code = rt
        .block_on(run_async(
            &entry,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            },
        ))
        .unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

/// Multi-thread host pattern: `LocalSet` carries the non-Send future and a
/// single run executes on the local task set. (Two `run_async` futures must
/// not be interleaved on one thread — v8 pins the isolate to its creating
/// thread — so parallel runs go through `run()` or `run_in_subprocess`.)
#[test]
fn async_run_on_multi_thread_runtime_via_local_set() {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("mt");
    let entry = dir.join("main.js");
    fs::write(&entry, "await new Promise(r => setTimeout(r, 200)); 1 + 1;").unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let entry_c = entry.clone();
    let options_c = options.clone();
    let code = local
        .block_on(&rt, async move { run_async(entry_c, &options_c).await })
        .unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}
