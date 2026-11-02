//! Local `deno.lock` handling: frozen mode, malformed input, and no writes.
//!
//! `docs/npm-support.md` states that `deno.lock` supplies and validates npm
//! checksums, that frozen mode rejects changes, and that libdeno never writes
//! lockfile updates. These tests pin all three for the local (non-remote) case;
//! the remote-fetch cases live in `remote_lockfile.rs`.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

/// Output capture is process-global for in-process runs.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("libdeno-local-lock-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(name: &str, deno_json: &str, deno_lock: Option<&str>) -> (PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('ran');").unwrap();
    fs::write(dir.join("deno.json"), deno_json).unwrap();
    if let Some(lock) = deno_lock {
        fs::write(dir.join("deno.lock"), lock).unwrap();
    }
    (dir, entry)
}

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    }
}

fn assert_runs(entry: &Path, context: &str) {
    let output = libdeno::run_with_output(entry, &options())
        .unwrap_or_else(|error| panic!("{context}: expected the run to succeed, got: {error}"));
    assert_eq!(output.exit_code, 0, "{context}");
}

const EMPTY_LOCK: &str = r#"{"version":"4","specifiers":{},"jsr":{},"npm":{}}"#;

#[test]
fn a_script_without_dependencies_ignores_frozen_mode() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Frozen mode only rejects *changes*; a run that needs no new entry has
    // nothing to reject.
    for (name, json, lock) in [
        ("frozen-no-lock", r#"{"lock":{"frozen":true}}"#, None),
        (
            "frozen-empty-lock",
            r#"{"lock":{"frozen":true}}"#,
            Some(EMPTY_LOCK),
        ),
        (
            "unfrozen-empty-lock",
            r#"{"lock":{"frozen":false}}"#,
            Some(EMPTY_LOCK),
        ),
    ] {
        let (dir, entry) = setup(name, json, lock);
        assert_runs(&entry, name);
        let _ = fs::remove_dir_all(dir);
    }
}

#[cfg(not(windows))]
#[test]
fn a_malformed_lockfile_is_rejected_regardless_of_frozen_mode() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Frozen is not the trigger here: an unreadable lockfile is an error in
    // both modes, because the run cannot know what it is allowed to resolve.
    for (name, json) in [
        ("malformed-frozen", r#"{"lock":{"frozen":true}}"#),
        ("malformed-unfrozen", r#"{"lock":{"frozen":false}}"#),
    ] {
        let (dir, entry) = setup(name, json, Some("{ not json }"));
        let error = libdeno::run_with_output(&entry, &options()).unwrap_err();
        assert!(
            error.to_string().contains("Failed reading lockfile"),
            "{name}: expected a lockfile read error, got: {error}"
        );
        let _ = fs::remove_dir_all(dir);
    }
}

#[test]
fn libdeno_never_writes_the_lockfile() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The lockfile is an input only. Byte-for-byte equality is the assertion;
    // a rewrite that happened to preserve semantics would still be a contract
    // break for hosts that commit the file.
    for (name, json) in [
        ("write-check-unfrozen", r#"{"lock":{"frozen":false}}"#),
        ("write-check-frozen", r#"{"lock":{"frozen":true}}"#),
    ] {
        let (dir, entry) = setup(name, json, Some(EMPTY_LOCK));
        let before = fs::read(dir.join("deno.lock")).unwrap();
        assert_runs(&entry, name);
        let after = fs::read(dir.join("deno.lock")).unwrap();
        assert_eq!(before, after, "{name}: the lockfile must not change");
        let _ = fs::remove_dir_all(dir);
    }
}

/// Frozen mode rejects a dependency the lockfile does not cover.
///
/// `docs/npm-support.md` says frozen mode "rejects changes". The error arrives
/// as `Core`, the same variant as a script failure, so a host that wants to
/// distinguish "your lockfile is stale" from "your script threw" has to read
/// the message — worth pinning so the wording does not drift unnoticed.
#[cfg(not(windows))]
#[test]
fn frozen_mode_rejects_a_dependency_the_lockfile_does_not_cover() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, entry) = setup(
        "frozen-stale",
        r#"{"lock":{"frozen":true},"nodeModulesDir":"auto"}"#,
        Some(EMPTY_LOCK),
    );
    fs::write(
        dir.join("package.json"),
        r#"{"name":"frozen-stale-fixture","dependencies":{"left-pad":"1.3.0"}}"#,
    )
    .unwrap();
    fs::write(&entry, "import pad from 'left-pad';\n").unwrap();

    let error = libdeno::run_with_output(&entry, &options())
        .expect_err("a stale lockfile must fail the run in frozen mode");
    let message = error.to_string();
    let _ = fs::remove_dir_all(&dir);

    assert!(
        matches!(error, libdeno::LibdenoError::Core(_)),
        "a stale lockfile is reported as Core, not a dedicated variant: {error:?}"
    );
    assert!(
        message.contains("lockfile is out of date"),
        "the message must name the lockfile so a host can tell it from a script failure: {message}"
    );
}
