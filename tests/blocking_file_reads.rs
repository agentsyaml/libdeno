//! A blocking file read is outside the deadline's reach.
//!
//! `execution_deadline` terminates V8 at an interruptible stack check, so a
//! run parked in a blocking syscall never reaches one. A FIFO with no writer is
//! the cheapest way to prove it: opening it blocks in `open(2)` until a writer
//! appears.
//!
//! The probes run in a separate process, because a run that never returns would
//! otherwise hang the whole test binary.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-blocking-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_fifo(path: &std::path::Path) {
    let status = Command::new("mkfifo").arg(path).status().unwrap();
    assert!(status.success(), "mkfifo failed");
}

/// Re-enters this test binary to run one probe in a process the parent can
/// kill. Returns whether the child exited within `limit`.
fn run_probe(dir: &std::path::Path, probe: &str, limit: Duration) -> bool {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            probe,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LIBDENO_FIFO_PROBE_DIR", dir)
        .spawn()
        .unwrap();

    let started = Instant::now();
    let exited = loop {
        match child.try_wait().unwrap() {
            Some(_) => break true,
            None if started.elapsed() > limit => break false,
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    exited
}

fn probe_options(dir: &std::path::Path, deadline: Duration) -> libdeno::LibdenoOptions {
    libdeno::LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        args: vec![dir.to_string_lossy().to_string()],
        execution_deadline: Some(deadline),
        ..Default::default()
    }
}

#[test]
fn a_sync_fifo_read_never_returns() {
    let dir = temp_dir("sync");
    make_fifo(&dir.join("pipe"));
    fs::write(
        dir.join("main.js"),
        "Deno.readTextFileSync(Deno.args[0] + '/pipe');\nconsole.log('UNREACHABLE');\n",
    )
    .unwrap();

    let exited = run_probe(&dir, "probe_sync", Duration::from_secs(6));
    let _ = fs::remove_dir_all(&dir);

    assert!(
        !exited,
        "the run returned, so a blocking FIFO read is now interruptible; \
         update docs/api.md and this test"
    );
}

#[test]
fn an_async_fifo_read_reports_the_deadline_but_does_not_return() {
    let dir = temp_dir("async");
    make_fifo(&dir.join("pipe"));
    fs::write(
        dir.join("main.js"),
        "await Deno.readTextFile(Deno.args[0] + '/pipe');\nconsole.log('UNREACHABLE');\n",
    )
    .unwrap();

    // The async form opens on the blocking pool, so the deadline does fire and
    // the child would report `Timeout` — but shutdown waits for that pool, so
    // the process still does not exit. Same observable outcome for a caller.
    let exited = run_probe(&dir, "probe_async", Duration::from_secs(6));
    let _ = fs::remove_dir_all(&dir);

    assert!(
        !exited,
        "the async run returned; the blocking pool no longer delays shutdown, \
         so update docs/api.md and this test"
    );
}

/// The async form's own deadline is honored, but the call still does not
/// return until the blocking pool drains.
///
/// `docs/api.md` claims this with a measurement ("a 1 s deadline and a writer
/// that appeared after 6 s: the error was correct, the return took 6 s") and
/// cites `tests/blocking_file_reads.rs`. The two probes above cover the
/// never-returns half; this one covers the "returns after the writer" half, so
/// the citation is real.
///
/// The writer is released at 3 s, longer than the 1 s deadline but short
/// enough to keep the test quick. The assertion is on ordering, not on an exact
/// duration: the run must fail with `Timeout` *and* must still be blocked at
/// the deadline. A machine can stretch absolute times, but it cannot make a
/// 3 s writer finish before a 1 s deadline.
#[test]
fn the_async_form_waits_for_the_late_writer_before_returning() {
    use std::process::{Command, Stdio};

    let dir = temp_dir("async-late-writer");
    let fifo = dir.join("pipe");
    make_fifo(&fifo);
    fs::write(
        dir.join("main.js"),
        "await Deno.readTextFile(Deno.args[0] + '/pipe');\nconsole.log('UNREACHABLE');\n",
    )
    .unwrap();

    let writer = std::thread::spawn({
        let fifo = fifo.clone();
        move || {
            std::thread::sleep(Duration::from_secs(3));
            let _ = fs::write(&fifo, "late");
        }
    });

    let started = Instant::now();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "probe_async_late_writer",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("LIBDENO_FIFO_PROBE_DIR", &dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Still running well past the 1 s deadline: shutdown is waiting on the
    // blocking pool, which is the documented behaviour.
    std::thread::sleep(Duration::from_millis(2000));
    let still_blocked = child.try_wait().unwrap().is_none();

    let status = child.wait().unwrap();
    let elapsed = started.elapsed();
    let _ = writer.join();
    let _ = fs::remove_dir_all(&dir);

    assert!(
        still_blocked,
        "the run returned before the writer appeared; the deadline now also \
         interrupts the blocking pool, so update docs/api.md and this test"
    );
    assert!(
        status.success(),
        "the probe must exit cleanly once the writer releases it"
    );
    assert!(
        elapsed >= Duration::from_secs(3),
        "the run returned in {elapsed:?}, before the writer appeared"
    );
}

/// Not a test: the sync probe the parent re-enters.
#[test]
#[ignore]
fn probe_sync() {
    let Ok(dir) = std::env::var("LIBDENO_FIFO_PROBE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let _ = libdeno::run(
        dir.join("main.js"),
        &probe_options(&dir, Duration::from_secs(1)),
    );
}

/// Not a test: the async probe used by the late-writer test.
#[test]
#[ignore]
fn probe_async_late_writer() {
    let Ok(dir) = std::env::var("LIBDENO_FIFO_PROBE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    // The child reports its own outcome; the parent asserts on timing.
    let _ = libdeno::run(
        dir.join("main.js"),
        &probe_options(&dir, Duration::from_secs(1)),
    );
}

/// Not a test: the async probe the parent re-enters.
#[test]
#[ignore]
fn probe_async() {
    let Ok(dir) = std::env::var("LIBDENO_FIFO_PROBE_DIR") else {
        return;
    };
    let dir = PathBuf::from(dir);
    let _ = libdeno::run(
        dir.join("main.js"),
        &probe_options(&dir, Duration::from_secs(1)),
    );
}
