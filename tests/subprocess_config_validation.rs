//! Invalid options are rejected by the parent before a child is spawned.
//!
//! A child that fails its own validation only prints to stderr and exits 1, so
//! without a parent-side check the caller receives `Ok(exit_code: 1)` for a
//! broken configuration — the same shape as a script that exits 1 on purpose.

use std::fs;
use std::path::PathBuf;

use libdeno::{LibdenoError, LibdenoOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-config-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(name: &str) -> (PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('hi');").unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    (dir, entry)
}

fn assert_configuration<T: std::fmt::Debug>(result: Result<T, LibdenoError>, needle: &str) {
    match result {
        Err(LibdenoError::Configuration(message)) => {
            assert!(
                message.contains(needle),
                "expected `{needle}` in: {message}"
            );
        }
        other => panic!("expected a Configuration error containing `{needle}`, got: {other:?}"),
    }
}

/// `Permission` is also a parent-side rejection; only the variant differs.
fn assert_rejected<T: std::fmt::Debug>(result: Result<T, LibdenoError>, needle: &str) {
    match result {
        Err(LibdenoError::Permission(message)) => {
            assert!(
                message.contains(needle),
                "expected `{needle}` in: {message}"
            );
        }
        other => assert_configuration(other, needle),
    }
}

#[test]
fn default_options_are_rejected_before_spawning() {
    let (dir, entry) = setup("defaults");
    // LibdenoOptions::default(): empty permissions, allow_all false.
    assert_configuration(
        libdeno::run_in_subprocess(&entry, &LibdenoOptions::default()),
        "no permission flags provided",
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn undersized_heap_cap_is_rejected_before_spawning() {
    let (dir, entry) = setup("heap");
    let options = LibdenoOptions {
        allow_all_permissions: true,
        max_heap_bytes: Some(1),
        ..Default::default()
    };
    assert_configuration(
        libdeno::run_in_subprocess(&entry, &options),
        "max_heap_bytes=1 is too small",
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prompt_with_captured_stderr_is_rejected_before_spawning() {
    let (dir, entry) = setup("prompt");
    let options = LibdenoOptions {
        allow_all_permissions: true,
        prompt: true,
        capture_stderr: true,
        ..Default::default()
    };
    assert_configuration(
        libdeno::run_in_subprocess_with_output(&entry, &options),
        "prompt and capture_stderr cannot be combined",
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn prompt_with_implied_stderr_capture_names_the_helper() {
    let (dir, entry) = setup("prompt-implied");
    // `prompt: true` without `capture_stderr`: the helper pipes stderr anyway,
    // so the generic advice ("capture stdout only") would not apply.
    let options = LibdenoOptions {
        allow_all_permissions: true,
        prompt: true,
        ..Default::default()
    };
    assert_configuration(
        libdeno::run_in_subprocess_with_output(&entry, &options),
        "prompt cannot be combined with run_in_subprocess_with_output",
    );
    // The exit-code-only helper inherits the terminal and is not rejected: the
    // run itself fails (no terminal to prompt on), not the configuration.
    let prompt_only = LibdenoOptions {
        prompt: true,
        ..Default::default()
    };
    assert_eq!(
        libdeno::run_in_subprocess(&entry, &prompt_only).unwrap(),
        1,
        "prompt with no terminal should fail the run, not the configuration"
    );
    // The documented positive case: `prompt: true` with full grants is valid
    // here *because* the prompt is never consulted, and `docs/api.md` says
    // rejecting it would break a host that sets one policy flag globally.
    // Without this assertion the test only proves the two failures.
    let prompt_with_grants = LibdenoOptions {
        allow_all_permissions: true,
        prompt: true,
        ..Default::default()
    };
    assert_eq!(
        libdeno::run_in_subprocess(&entry, &prompt_with_grants).unwrap(),
        0,
        "prompt with full grants must run: no check is left to prompt for"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn unknown_feature_name_is_rejected_before_spawning() {
    let (dir, entry) = setup("features");
    let options = LibdenoOptions {
        allow_all_permissions: true,
        features: Some(vec!["not-a-feature".to_string()]),
        ..Default::default()
    };
    assert_configuration(
        libdeno::run_in_subprocess(&entry, &options),
        "unknown runtime feature: not-a-feature",
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn zero_deadline_is_rejected_before_spawning() {
    let (dir, entry) = setup("zero-deadline");
    let options = LibdenoOptions {
        allow_all_permissions: true,
        execution_deadline: Some(std::time::Duration::ZERO),
        ..Default::default()
    };
    // `Timeout`, not `Configuration`: the run was refused, not misconfigured.
    match libdeno::run_in_subprocess(&entry, &options) {
        Err(LibdenoError::Timeout(message)) => {
            assert!(message.contains("script was not started"), "{message}");
        }
        other => panic!("expected a Timeout error, got: {other:?}"),
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn unknown_permission_flag_is_rejected_before_spawning() {
    let (dir, entry) = setup("flag");
    let options = LibdenoOptions {
        permissions: vec!["--allow-nonsense".to_string()],
        ..Default::default()
    };
    assert_rejected(
        libdeno::run_in_subprocess(&entry, &options),
        "unknown permission flag",
    );
    let _ = fs::remove_dir_all(dir);
}
