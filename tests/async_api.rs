//! Async entry points: `run_async` / `run_with_output_async` execute the run
//! on the caller's tokio runtime (no spawned thread). The returned future is
//! not `Send` (deno's worker stack is Rc-based), so tests use a
//! current-thread runtime, and the multi-thread case exercises the
//! documented `LocalSet` pattern.

use std::fs;
use std::future::Future;
use std::path::PathBuf;

#[cfg(not(windows))]
use libdeno::run_with_output_async;
use libdeno::{run_async, run_source_async, LibdenoOptions, LibdenoRuntime, SourceLang};

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

#[cfg(not(windows))]
#[cfg(not(windows))]
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

#[cfg(not(windows))]
#[cfg(not(windows))]
#[test]
fn reusable_async_capture_returns_output() {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("runtime-cap");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('runtime-async-hello');").unwrap();
    let build_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = build_rt.block_on(LibdenoRuntime::new(&dir)).unwrap();
    drop(build_rt);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = rt
        .block_on(runtime.run_with_output_async(
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
        output
            .stdout
            .windows(19)
            .any(|w| w == b"runtime-async-hello"),
        "captured stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(windows)]
#[cfg(not(windows))]
#[test]
fn reusable_async_capture_is_rejected_on_windows() {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("runtime-cap-win");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('runtime-async-hello');").unwrap();
    let build_rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = build_rt.block_on(LibdenoRuntime::new(&dir)).unwrap();
    drop(build_rt);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = rt
        .block_on(runtime.run_with_output_async(
            &entry,
            &LibdenoOptions {
                allow_all_permissions: true,
                capture_stdout: true,
                ..Default::default()
            },
        ))
        .unwrap_err();
    assert!(matches!(error, libdeno::LibdenoError::Configuration(_)));
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

#[test]
fn run_source_async_smoke() {
    // In-memory source on the caller's tokio runtime: same !Send / guard
    // rules as run_async — one sequential run is enough for the smoke.
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("source-async");
    fs::write(dir.join("sibling.js"), "export const v = 7;").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let code = rt
        .block_on(run_source_async(
            "import { v } from './sibling.js';\nif (v !== 7) Deno.exit(3);",
            SourceLang::JavaScript,
            &dir,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            },
        ))
        .unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

/// In-process capture is refused on Windows (std's stdout/stderr bypass the
/// redirected CRT fd), so the happy path only exists on Unix. The refusal has
/// its own test above.
#[cfg(not(windows))]
#[cfg(not(windows))]
#[test]
fn run_source_with_output_async_captures_from_an_in_memory_entry() {
    // Only the "reject outside a tokio context" path had a test; the happy
    // path — an in-memory entry, its relative imports, and captured output —
    // was uncovered. Capture is exclusive, so this serializes with the other
    // capturing tests in this binary.
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("source-async-output");
    fs::write(dir.join("sibling.js"), "export const v = 7;").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let output = rt
        .block_on(libdeno::run_source_with_output_async(
            "import { v } from './sibling.js';\n\
             console.log('out ' + v);\n\
             console.error('err ' + v);\n",
            SourceLang::JavaScript,
            &dir,
            &LibdenoOptions {
                allow_all_permissions: true,
                capture_stdout: true,
                capture_stderr: true,
                ..Default::default()
            },
        ))
        .unwrap();
    assert_eq!(output.exit_code, 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = fs::remove_dir_all(&dir);
    assert!(stdout.contains("out 7"), "stdout: {stdout:?}");
    assert!(stderr.contains("err 7"), "stderr: {stderr:?}");
}

#[cfg(not(windows))]
#[test]
fn reusable_runtime_runs_in_memory_source_with_output() {
    // `LibdenoRuntime::run_with_source_async` and its captured variant were
    // public but unreferenced by any test: only the free `run_source_async`
    // functions were covered. The reusable path builds the virtual entry
    // against `base_dir` while the resolver stack stays scoped to the
    // runtime's directory, so a relative import has to resolve through the
    // *shared* stack — that is the part worth pinning.
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("runtime-source-async");
    fs::write(dir.join("sibling.js"), "export const v = 11;").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = rt.block_on(LibdenoRuntime::new(&dir)).unwrap();
    let output = rt
        .block_on(runtime.run_with_output_source_async(
            "import { v } from './sibling.js';\nconsole.log('out ' + v);",
            SourceLang::JavaScript,
            &dir,
            &LibdenoOptions {
                allow_all_permissions: true,
                capture_stdout: true,
                ..Default::default()
            },
        ))
        .unwrap();
    assert_eq!(output.exit_code, 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let code = rt
        .block_on(runtime.run_with_source_async(
            "if (1 + 1 !== 2) Deno.exit(3);",
            SourceLang::JavaScript,
            &dir,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            },
        ))
        .unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert!(stdout.contains("out 11"), "stdout: {stdout:?}");
    assert_eq!(code, 0);
}

#[test]
fn reusable_runtime_source_async_rejects_a_mismatched_cwd() {
    // The shared stack is scoped to the runtime's directory, so an
    // `options.cwd` that resolves elsewhere cannot be honored. Both methods
    // share `check_source_preconditions_async`, but only a test keeps that
    // wired: silently ignoring the pin would run the script against the
    // runtime's directory instead of the caller's.
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("runtime-source-cwd");
    let other = temp_dir("runtime-source-cwd-other");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = rt.block_on(LibdenoRuntime::new(&dir)).unwrap();
    let err = rt
        .block_on(runtime.run_with_source_async(
            "console.log('never');",
            SourceLang::JavaScript,
            &dir,
            &LibdenoOptions {
                allow_all_permissions: true,
                cwd: Some(other.clone()),
                ..Default::default()
            },
        ))
        .unwrap_err();
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&other);
    assert!(
        err.to_string()
            .contains("run_with ignores LibdenoOptions.cwd"),
        "expected a cwd rejection, got: {err}"
    );
}
