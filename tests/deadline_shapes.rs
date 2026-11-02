//! A deadline that fires inside a subprocess is reported as exit code 1, not as
//! `LibdenoError::Timeout`. The only machine-readable trace is the child's
//! stderr, and only when stderr was captured.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-deadline-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(name: &str) -> (PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('started');\nfor (;;) {}").unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    (dir, entry)
}

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        execution_deadline: Some(Duration::from_millis(300)),
        ..Default::default()
    }
}

#[cfg(not(windows))]
#[test]
fn in_process_reports_a_deadline_as_a_timeout_error() {
    let (dir, entry) = setup("in-process");
    let error = libdeno::run_with_output(&entry, &options()).unwrap_err();
    assert!(
        matches!(error, libdeno::LibdenoError::Timeout(_)),
        "expected Timeout, got: {error:?}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn subprocess_reports_the_same_deadline_as_exit_code_one() {
    let (dir, entry) = setup("subprocess");
    let mut opts = options();
    opts.capture_stdout = true;
    opts.capture_stderr = true;
    let output = libdeno::run_in_subprocess_with_output(&entry, &opts)
        .expect("a deadline inside a child is not a parent-side error");
    assert_eq!(output.exit_code, 1);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("execution deadline") && stderr.contains("exceeded"),
        "the only timeout signal is the child's stderr, got: {stderr}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn subprocess_deadline_is_indistinguishable_from_deno_exit_one() {
    let (dir, entry) = setup("ambiguous");
    let exited = dir.join("exited.js");
    fs::write(&exited, "Deno.exit(1);").unwrap();
    let mut opts = options();
    opts.capture_stderr = true;
    let timed_out = libdeno::run_in_subprocess_with_output(&entry, &opts).unwrap();
    let plain_exit = libdeno::run_in_subprocess_with_output(&exited, &opts).unwrap();
    assert_eq!(timed_out.exit_code, plain_exit.exit_code);
    let _ = fs::remove_dir_all(dir);
}

/// A deadline that fires while JS is running returns promptly.
///
/// `docs/api.md` states two different latencies: a busy loop returns within
/// milliseconds of the deadline, while an event loop parked on a timer waits a
/// fixed grace period because there is no JS on the stack to interrupt. Only
/// the second one is observable as "the deadline fired late", so pin the first:
/// if `terminate_execution` ever stops interrupting running JS, the busy case
/// silently inherits the grace offset and hosts notice a 3x overrun on their
/// timeout budget.
#[cfg(not(windows))]
#[test]
fn a_running_script_returns_close_to_the_deadline() {
    let dir = temp_dir("busy-latency");
    let deadline = Duration::from_millis(500);

    // Busy loop: V8 has JS on the stack, so the watchdog interrupts it and the
    // run returns at the deadline. Best of three: the measurement starts before
    // isolate bootstrap, and a machine under load can add seconds to that on
    // any single attempt. The minimum is what an unloaded run costs, which is
    // the quantity being compared.
    let busy = dir.join("busy.js");
    fs::write(&busy, "for (;;) {}").unwrap();
    let mut busy_elapsed = Duration::MAX;
    let mut busy_error = None;
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let error = libdeno::run_with_output(
            &busy,
            &LibdenoOptions {
                allow_all_permissions: true,
                execution_deadline: Some(deadline),
                ..Default::default()
            },
        )
        .unwrap_err();
        busy_elapsed = busy_elapsed.min(started.elapsed());
        busy_error = Some(error);
    }
    let busy_error = busy_error.expect("three busy attempts ran");

    // Parked event loop: nothing to interrupt, so the run is dropped only after
    // the fixed grace period. This is the reference the assertion compares
    // against, which keeps the test independent of how expensive isolate
    // bootstrap happens to be on the machine running it.
    let parked = dir.join("parked.js");
    fs::write(&parked, "await new Promise((r) => setTimeout(r, 60000));").unwrap();
    // Best of three for the same reason: one sample can include a scheduling
    // stall that has nothing to do with the interrupt path.
    let mut parked_elapsed = Duration::MAX;
    let mut parked_error = None;
    for _ in 0..3 {
        let started = std::time::Instant::now();
        let error = libdeno::run_with_output(
            &parked,
            &LibdenoOptions {
                allow_all_permissions: true,
                execution_deadline: Some(deadline),
                ..Default::default()
            },
        )
        .unwrap_err();
        parked_elapsed = parked_elapsed.min(started.elapsed());
        parked_error = Some(error);
    }
    let parked_error = parked_error.expect("three parked attempts ran");
    let _ = fs::remove_dir_all(dir);

    assert!(
        matches!(busy_error, libdeno::LibdenoError::Timeout(_)),
        "expected Timeout from the busy loop, got: {busy_error:?}"
    );
    assert!(
        matches!(parked_error, libdeno::LibdenoError::Timeout(_)),
        "expected Timeout from the parked loop, got: {parked_error:?}"
    );
    // The documented split is "busy returns at the deadline, parked returns
    // after the grace". Both measurements share the same bootstrap cost, so
    // their difference isolates the interrupt path: losing it would make the
    // busy case inherit the grace and the two would be nearly equal.
    assert!(
        busy_elapsed + Duration::from_millis(1000) < parked_elapsed,
        "a busy loop must unwind at the deadline, not after the 2 s grace \
         (busy {busy_elapsed:?}, parked {parked_elapsed:?})"
    );
}
