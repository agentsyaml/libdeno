//! Concurrency protocol tests: ordinary in-process runs execute in parallel
//! (no global serialization), while a captured run is exclusive — any
//! overlapping run (captured or not) is rejected with `Configuration`.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use libdeno::{run, LibdenoOptions};
#[cfg(not(windows))]
use libdeno::{run_with_output, LibdenoError};

/// The three tests in this file must not overlap each other: the captured
/// run's exclusivity lease rejects any concurrent run, and the capture test
/// would race the parallel tests. Each test takes this file-level lock
/// (parallelism *within* a test is unaffected — the lock is only about
/// tests not stepping on each other).
static FILE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "libdeno-concurrency-{}-{}",
        std::process::id(),
        name
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Two ordinary runs from two host threads must both observe the other run's
/// started marker: this proves overlap directly instead of relying on a wall
/// clock threshold that is sensitive to CI load.
#[test]
fn parallel_runs_overlap_in_time() {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir_a = temp_dir("par-a");
    let dir_b = temp_dir("par-b");
    let entry_a = dir_a.join("main.js");
    let entry_b = dir_b.join("main.js");
    let started_a = dir_a.join("started");
    let started_b = dir_b.join("started");
    let script = |started: &PathBuf, other: &PathBuf| {
        format!(
            "Deno.writeTextFileSync({started:?}, '1');\n\
             let overlap = false;\n\
             const deadline = Date.now() + 5000;\n\
             while (Date.now() < deadline) {{\n\
               try {{ Deno.statSync({other:?}); overlap = true; break; }}\n\
               catch {{ await new Promise((resolve) => setTimeout(resolve, 10)); }}\n\
             }}\n\
             Deno.exit(overlap ? 0 : 42);"
        )
    };
    fs::write(&entry_a, script(&started_a, &started_b)).unwrap();
    fs::write(&entry_b, script(&started_b, &started_a)).unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let (a, b) = std::thread::scope(|s| {
        let ha = s.spawn(|| run(&entry_a, &options).unwrap());
        let hb = s.spawn(|| run(&entry_b, &options).unwrap());
        (ha.join().unwrap(), hb.join().unwrap())
    });
    assert_eq!(a, 0);
    assert_eq!(b, 0);
    let _ = fs::remove_dir_all(&dir_a);
    let _ = fs::remove_dir_all(&dir_b);
}

/// A captured run is exclusive: a second run started while it is active is
/// rejected with `Configuration` (not silently serialized — capture is
/// fd-level process-global redirection and would steal the other run's
/// output). Once the captured run finishes, ordinary runs work again.
/// (Capture itself is rejected on Windows — the sync capture tests are
/// file-gated, this mixed file gates just the captured case.)
#[cfg(not(windows))]
#[test]
fn captured_run_rejects_concurrent_ordinary_run() {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("cap-excl");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        // Marker file (absolute path via import.meta.url) proves the script —
        // and therefore the capture lease — is active before the second run
        // is attempted; then stay alive long enough to be caught mid-flight.
        "Deno.writeTextFileSync(new URL('./started', import.meta.url), '1');\n\
         await new Promise(r => setTimeout(r, 3000));\n\
         console.log('done');",
    )
    .unwrap();
    let marker = dir.join("started");
    let captured_options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    };

    let worker_entry = entry.clone();
    let worker_options = captured_options.clone();
    let worker =
        std::thread::spawn(move || run_with_output(&worker_entry, &worker_options).unwrap());

    // Wait until the script is provably running (capture lease held).
    let deadline = Instant::now() + std::time::Duration::from_secs(15);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "captured script never started");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // An ordinary run overlapping the captured one must be rejected.
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let err = run(&entry, &options).unwrap_err();
    assert!(
        matches!(err, LibdenoError::Configuration(_)),
        "expected Configuration rejection, got: {err}"
    );

    // And a second captured run must be rejected too.
    let err = run_with_output(&entry, &captured_options.clone()).unwrap_err();
    assert!(matches!(err, LibdenoError::Configuration(_)));

    let output = worker.join().unwrap();
    assert_eq!(output.exit_code, 0);
    assert!(
        output.stdout.windows(4).any(|w| w == b"done"),
        "captured stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );

    // Lease released: ordinary runs work again.
    assert_eq!(run(&entry, &options).unwrap(), 0);
    let _ = fs::remove_dir_all(&dir);
}

/// Ordinary runs do not reject each other even under heavy concurrency —
/// the lease only guards the capture marker.
#[test]
fn many_parallel_runs_all_succeed() {
    let _g = FILE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("par-many");
    let entry = dir.join("main.js");
    fs::write(&entry, "1 + 1;").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let runs: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
    std::thread::scope(|s| {
        for _ in 0..4 {
            let entry = entry.clone();
            let options = options.clone();
            let runs = runs.clone();
            s.spawn(move || {
                for _ in 0..3 {
                    assert_eq!(run(&entry, &options).unwrap(), 0);
                    runs.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    assert_eq!(runs.load(Ordering::Relaxed), 12);
    let _ = fs::remove_dir_all(&dir);
}
