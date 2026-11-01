//! `max_heap_bytes` behaves the same way on both backends: the script is
//! terminated and the host survives.
//!
//! In-process that is `Err` with V8's termination message; in a subprocess it is
//! `Ok(exit_code: 1)` with the same message on the child's stderr — the usual
//! backend difference. Either way the cap must never abort the embedder, and the
//! host must stay usable for later runs.

use std::fs;
use std::path::PathBuf;

use libdeno::LibdenoOptions;

/// Output capture is process-global for in-process runs.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-heapcap-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn capped() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        capture_stderr: true,
        max_heap_bytes: Some(64 << 20),
        ..Default::default()
    }
}

#[test]
fn an_in_process_heap_cap_terminates_the_script_not_the_host() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("in-process");
    let hog = dir.join("hog.js");
    fs::write(
        &hog,
        "const k = []; for (;;) { k.push(new Array(100000).fill(1.5)); }",
    )
    .unwrap();

    // Twice, to prove the host is still usable after the first termination.
    for round in 0..2 {
        let error = libdeno::run_with_output(&hog, &capped()).unwrap_err();
        assert!(
            error.to_string().contains("execution terminated"),
            "round {round}: expected V8's termination message, got: {error}"
        );
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_subprocess_heap_cap_terminates_the_child_and_keeps_the_host_usable() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("subprocess");
    let hog = dir.join("hog.js");
    fs::write(
        &hog,
        "const k = []; for (;;) { k.push(new Array(100000).fill(1.5)); }",
    )
    .unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));

    for round in 0..2 {
        let output = libdeno::run_in_subprocess_with_output(&hog, &capped())
            .expect("a terminated child is not a parent-side error");
        assert_eq!(output.exit_code, 1, "round {round}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("execution terminated"),
            "round {round}: expected the termination message on stderr, got: {stderr}"
        );
    }

    // The host still works after two terminated children.
    let ok = dir.join("ok.js");
    fs::write(&ok, "console.log('alive');").unwrap();
    let output = libdeno::run_in_subprocess_with_output(&ok, &capped()).unwrap();
    assert_eq!(output.exit_code, 0);
    let _ = fs::remove_dir_all(dir);
}

/// The cap is a V8 *old generation* budget, not a memory limit.
///
/// `docs/api.md` says ArrayBuffers are not counted against `max_heap_bytes`.
/// That sentence is the only thing standing between "the cap bounds memory"
/// and what it actually does, so it needs a test: a 128 MiB backing store under
/// an 8 MiB cap must run to completion. If V8 ever started counting external
/// allocations, the documented guidance ("lower the cap to bound RSS") would
/// silently stop working.
#[test]
fn an_array_buffer_larger_than_the_cap_does_not_terminate_the_run() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("external-allocation");
    let entry = dir.join("buffer.js");
    fs::write(
        &entry,
        "const b = new ArrayBuffer(128 * 1024 * 1024);\n\
         new Uint8Array(b)[0] = 1;\n\
         console.log('SURVIVED', b.byteLength);",
    )
    .unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        capture_stderr: true,
        max_heap_bytes: Some(8 << 20),
        ..Default::default()
    };
    let output = libdeno::run_in_subprocess_with_output(&entry, &options)
        .expect("an external allocation must not terminate the child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let _ = fs::remove_dir_all(dir);
    assert_eq!(output.exit_code, 0, "stderr: {stderr}");
    assert!(
        stdout.contains("SURVIVED 134217728"),
        "an ArrayBuffer is external allocation, not old-generation heap: {stdout:?}"
    );
}
