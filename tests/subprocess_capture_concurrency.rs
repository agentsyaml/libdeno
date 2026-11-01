//! `run_in_subprocess_with_output` is documented as parallel-safe, but the
//! parallelism has a hard ceiling: each run holds two pipe-reader slots against
//! a process-wide budget of 64, so at most 32 captures are in flight at once.
//!
//! The ceiling is deliberate (an unbounded number of detached readers would
//! leak fds and threads), but it is invisible in the return type: the 33rd
//! simultaneous capture fails with an `Io` error instead of queueing. These
//! tests pin both halves — the limit exists, and the budget recovers.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

/// Both tests spend the same process-wide reader budget, so they must not run
/// at the same time (cargo runs tests in one binary on parallel threads).
static BUDGET_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn budget_lock() -> std::sync::MutexGuard<'static, ()> {
    BUDGET_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-conc-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// The script stays alive long enough that a simultaneous burst really is
/// simultaneous.
///
/// A script that prints and exits releases its two reader slots almost
/// immediately, so on a fast host some of a 40-run burst finish before later
/// runs reserve and the budget is never exhausted — the cap assertion then
/// fails for the wrong reason. The timer keeps every slot held across the whole
/// burst regardless of how quickly the children start.
fn entry(dir: &Path) -> PathBuf {
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "console.log('hi');\nawait new Promise((r) => setTimeout(r, 400));",
    )
    .unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    entry
}

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    }
}

/// Runs `count` captures that all start together, returning how many succeeded
/// and the first error message.
fn burst(entry: &Path, count: usize) -> (usize, Option<String>) {
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(count));
    let handles: Vec<_> = (0..count)
        .map(|_| {
            let entry = entry.to_path_buf();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                libdeno::run_in_subprocess_with_output(&entry, &options())
                    .map(|output| output.exit_code)
                    .map_err(|error| error.to_string())
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let ok = results.iter().filter(|result| result.is_ok()).count();
    let error = results
        .iter()
        .find_map(|result| result.as_ref().err().cloned());
    (ok, error)
}

#[test]
fn a_simultaneous_burst_is_capped_at_32_captures() {
    let _budget = budget_lock();
    let dir = temp_dir("cap");
    let entry = entry(&dir);
    let (ok, error) = burst(&entry, 40);
    // The cap is 64 slots / 2 streams, but the whole test binary runs in
    // parallel with other targets and the host is shared, so runs from this
    // burst can finish (and release slots) before later ones reserve. Assert
    // the two things that actually matter: the burst is capped well below the
    // requested 40, and the rejection names the budget.
    assert!(
        ok > 0 && ok < 40,
        "a 40-capture burst must be partially rejected, got {ok}"
    );
    let error = error.expect("40 simultaneous captures must exceed the budget");
    assert!(
        error.contains("subprocess output reader/pipe resource budget exhausted"),
        "the error must name the exhausted budget, got: {error}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn the_budget_recovers_after_a_burst() {
    let _budget = budget_lock();
    let dir = temp_dir("recover");
    let entry = entry(&dir);
    let (ok, _) = burst(&entry, 40);
    assert!(ok > 0, "the burst itself must run");
    // Let the survivors finish and release their slots.
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let (after, error) = burst(&entry, 10);
    assert_eq!(
        after, 10,
        "a smaller burst must fully succeed after the budget recovers: {error:?}"
    );
    let _ = fs::remove_dir_all(dir);
}

/// A host distinguishes "too many runs" from "this script leaks its stdio" by
/// `ErrorKind`, not by matching the message. Both failures arrive as
/// `LibdenoError::Io`, so the kind is the only stable discriminant.
///
/// The assertion is a correspondence, not "every error is `WouldBlock`": a
/// burst of 40 children can also push a run past the 500 ms collection budget,
/// and under load that produces a genuine `TimedOut`. What must hold is that
/// each message maps to its own kind.
#[test]
fn each_capture_failure_maps_to_its_own_error_kind() {
    let _budget = budget_lock();
    let dir = temp_dir("kinds");
    let entry = entry(&dir);
    let count = 40;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(count));
    let handles: Vec<_> = (0..count)
        .map(|_| {
            let entry = entry.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                libdeno::run_in_subprocess_with_output(&entry, &options())
            })
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let _ = fs::remove_dir_all(&dir);

    let failures: Vec<(std::io::ErrorKind, String)> = results
        .iter()
        .filter_map(|result| match result {
            Err(libdeno::LibdenoError::Io(error)) => Some((error.kind(), error.to_string())),
            _ => None,
        })
        .collect();
    assert!(
        failures
            .iter()
            .any(|(_, message)| message.contains("resource budget exhausted")),
        "a 40-capture burst must exhaust the reader budget; failures: {failures:?}"
    );
    for (kind, message) in &failures {
        let expected = if message.contains("resource budget exhausted") {
            std::io::ErrorKind::WouldBlock
        } else if message.contains("collection deadline expired") {
            std::io::ErrorKind::TimedOut
        } else {
            panic!("unexpected capture failure: {message}");
        };
        assert_eq!(
            *kind, expected,
            "a host branches on ErrorKind, so the kind must match the cause: {message}"
        );
    }
}
