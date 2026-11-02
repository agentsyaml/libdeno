//! Subprocess capture is per-process and therefore parallel.
//!
//! In-process capture redirects the process-global fds, so it takes an
//! exclusive lease and a concurrent run is rejected. `run_in_subprocess_with_output`
//! captures the child's own pipes, so it takes no such lease: several can run
//! at once, alongside ordinary in-process runs.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use libdeno::LibdenoOptions;

/// Output capture is process-global for in-process runs, so these tests are
/// serialized: one of them holds the exclusive lease while it runs.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-par-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn entry(dir: &std::path::Path, millis: u64) -> PathBuf {
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        format!("await new Promise((r) => setTimeout(r, {millis})); console.log('done');"),
    )
    .unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    entry
}

fn subprocess_capture() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    }
}

fn plain() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    }
}

#[test]
fn three_subprocess_captures_run_at_the_same_time() {
    // Serialized against the in-process capture test below.
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("parallel");
    let entry = entry(&dir, 300);
    let options = subprocess_capture();
    // Warm the child host so the first spawn's setup does not dominate.
    libdeno::run_in_subprocess_with_output(&entry, &options).unwrap();

    // The load-bearing assertion is that none is *rejected*: a process-global
    // lease would fail two of the three with a Configuration error, exactly as
    // it does for in-process capture below. Wall-clock is not asserted, since
    // a loaded machine makes any such bound flaky without proving anything.
    let handles: Vec<_> = (0..3)
        .map(|_| {
            let entry = entry.clone();
            let options = options.clone();
            std::thread::spawn(move || {
                libdeno::run_in_subprocess_with_output(&entry, &options)
                    .expect("no subprocess capture may be rejected")
                    .exit_code
            })
        })
        .collect();
    let codes: Vec<i32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(codes, vec![0, 0, 0]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn subprocess_capture_coexists_with_an_in_process_run() {
    // Serialized against the in-process capture test below.
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("coexist");
    let entry = entry(&dir, 300);
    let captured = subprocess_capture();
    let plain = plain();
    // Warm the child host first.
    libdeno::run_in_subprocess_with_output(&entry, &captured).unwrap();

    let capture = {
        let entry = entry.clone();
        std::thread::spawn(move || {
            libdeno::run_in_subprocess_with_output(&entry, &captured).map(|o| o.exit_code)
        })
    };
    let ordinary = {
        let entry = entry.clone();
        std::thread::spawn(move || libdeno::run(&entry, &plain))
    };
    assert_eq!(capture.join().unwrap().unwrap(), 0);
    assert_eq!(
        ordinary.join().unwrap().unwrap(),
        0,
        "subprocess capture takes no process-global lease, so a plain run must not be blocked"
    );
    let _ = fs::remove_dir_all(dir);
}

#[cfg(not(windows))]
#[test]
fn in_process_capture_still_rejects_a_concurrent_run() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("exclusive");
    // The script announces that it is running, so the assertion below cannot
    // race the capture's lease acquisition.
    let marker = dir.join("started");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        format!(
            "Deno.writeTextFileSync({marker:?}, 'started');\n\
             await new Promise((r) => setTimeout(r, 400));\n\
             console.log('done');"
        ),
    )
    .unwrap();
    let captured = LibdenoOptions {
        capture_stdout: true,
        ..plain()
    };
    let plain = plain();

    let capture = {
        let entry = entry.clone();
        std::thread::spawn(move || libdeno::run_with_output(&entry, &captured).map(|o| o.exit_code))
    };
    // A watchdog, not a latency assertion: the captured run has to bootstrap an
    // isolate first, and on a loaded machine that alone can outlast 10 s.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the captured run never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let blocked = libdeno::run(&entry, &plain).unwrap_err();
    assert!(
        blocked.to_string().contains("process-global"),
        "expected the exclusivity error, got: {blocked}"
    );
    assert_eq!(capture.join().unwrap().unwrap(), 0);
    let _ = fs::remove_dir_all(dir);
}
