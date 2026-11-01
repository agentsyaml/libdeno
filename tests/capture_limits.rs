//! `max_capture_bytes` counts bytes, and the boundary is exact.
//!
//! The limit is a byte budget, so truncation can split a multi-byte UTF-8
//! sequence. `RunOutput::stdout` therefore returns raw bytes, not text; a
//! caller that wants a string has to tolerate an incomplete trailing character.
//!
//! Exact-byte assertions use `run_in_subprocess_with_output`: in-process
//! capture redirects the process-global fds, so this test binary's own harness
//! output also lands in the buffer and shifts every boundary.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

/// In-process runs take a process-wide lease: a captured run is exclusive and
/// fails while any other run is active, even an uncaptured one. These tests run
/// in parallel threads, so the in-process ones serialize on this lock.
static IN_PROCESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-cap-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(name: &str, script: &str) -> (PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let entry = dir.join("main.js");
    fs::write(&entry, script).unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    (dir, entry)
}

fn options(cap: usize) -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        max_capture_bytes: Some(cap),
        ..Default::default()
    }
}

fn child_shape(entry: &Path, cap: usize) -> (usize, bool) {
    let output = libdeno::run_in_subprocess_with_output(entry, &options(cap))
        .expect("capture within the limit must succeed");
    (output.stdout.len(), output.capture_truncated)
}

#[test]
fn the_limit_is_exact() {
    let (dir, entry) = setup("exact", "console.log('0123456789');");
    // The script writes 11 bytes: 10 digits plus the newline.
    for (cap, expected) in [(9, (9, true)), (10, (10, true)), (11, (11, false))] {
        assert_eq!(child_shape(&entry, cap), expected, "cap {cap}");
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_cap_can_split_a_multibyte_character() {
    let (dir, entry) = setup("emoji", "console.log('😀😀');");
    // Each emoji is 4 bytes; a 5-byte cap keeps one whole emoji plus the first
    // byte of the next, so the captured bytes are not valid UTF-8.
    let output = libdeno::run_in_subprocess_with_output(&entry, &options(5)).unwrap();
    assert_eq!(output.stdout.len(), 5);
    assert!(output.capture_truncated);
    assert!(
        String::from_utf8(output.stdout.clone()).is_err(),
        "a byte cap is not character-aware, so the tail can be incomplete: {:?}",
        output.stdout
    );
    // The documented way to consume it.
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.starts_with('😀'), "got: {text:?}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_zero_cap_keeps_nothing_and_reports_truncation() {
    let (dir, entry) = setup("zero", "console.log('hello');");
    assert_eq!(child_shape(&entry, 0), (0, true));

    // In-process capture also keeps nothing, but the buffer may hold this
    // binary's own harness output first — the cap still bounds it to zero.
    let _in_process = IN_PROCESS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let output = libdeno::run_with_output(&entry, &options(0)).unwrap();
    assert!(output.stdout.is_empty());
    assert!(output.capture_truncated);
    let _ = fs::remove_dir_all(dir);
}

/// `max_capture_bytes` without a capture flag does nothing at all.
///
/// The two options are adjacent in `LibdenoOptions` and read as a pair, so a
/// host can easily set the cap and forget the flag. The result is not a
/// truncated buffer or an error — it is an empty one, which reads as "the
/// script printed nothing". `docs/api.md` says the limit applies only to a
/// captured stream; this pins the flag pairing so the inert combination cannot
/// silently become a default-on capture.
#[test]
fn a_cap_without_its_capture_flag_produces_no_output() {
    let _in_process = IN_PROCESS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, entry) = setup("cap-without-flag", "console.log('hello world');");
    let output = libdeno::run_with_output(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            max_capture_bytes: Some(5),
            // deliberately no capture_stdout
            ..Default::default()
        },
    )
    .unwrap();
    let _ = fs::remove_dir_all(dir);
    assert_eq!(output.exit_code, 0);
    assert!(
        output.stdout.is_empty(),
        "a cap alone must not turn capture on: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !output.capture_truncated,
        "nothing was captured, so nothing was truncated"
    );
}
