//! A host that forgets `maybe_handle_child_mode()` used to turn one
//! `run_in_subprocess` call into an unbounded process tree: the spawned child
//! re-ran the host's own `main()`, which called `run_in_subprocess` again.
//!
//! This test binary is exactly such a host. Without the guard it would fork
//! until the machine's process table filled; with it, the nested call fails
//! fast with a Configuration error.

use std::fs;
use std::path::PathBuf;

use libdeno::{LibdenoError, LibdenoOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-nesting-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_child_that_never_serviced_child_mode_cannot_spawn_another() {
    let dir = temp_dir("guard");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('SCRIPT-RAN');").unwrap();

    // No LIBDENO_HOST_EXE: the spawner falls back to current_exe(), which is
    // this test binary. It does not call maybe_handle_child_mode(), so the
    // spawned process re-enters this test with the child markers set.
    if std::env::var_os("LIBDENO_CHILD_MODE").is_some() {
        let nested = libdeno::run_in_subprocess_with_output(
            &entry,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            },
        );
        let error = nested.expect_err("a nested spawn must be refused");
        assert!(
            matches!(error, LibdenoError::Configuration(_)),
            "unexpected error variant: {error:?}"
        );
        assert!(
            error.to_string().contains("maybe_handle_child_mode"),
            "the error must name the fix: {error}"
        );
        // Exit non-zero so the parent can tell this apart from a real run.
        std::process::exit(0);
    }

    std::env::remove_var("LIBDENO_HOST_EXE");
    // The child is this same test binary, which exits as soon as its own test
    // finishes — it never reads the run request. The parent's write can
    // therefore lose the race and fail with "child exited with exit status: 0
    // before accepting the run request". That is not the property under test
    // (the nested spawn being refused is), so retry until a write lands. Each
    // attempt starts a fresh child, so the loop cannot hang on one.
    let mut output = None;
    let mut last_error = None;
    for _ in 0..20 {
        match libdeno::run_in_subprocess_with_output(
            &entry,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            },
        ) {
            Ok(result) => {
                output = Some(result);
                break;
            }
            Err(error) => {
                let text = error.to_string();
                assert!(
                    text.contains("before accepting the run request"),
                    "only the write/exit race may be retried, got: {text}"
                );
                last_error = Some(text);
            }
        }
    }
    let output = output.unwrap_or_else(|| {
        panic!("every attempt lost the startup race; last error: {last_error:?}")
    });
    // The spawned process ran this test again instead of the script, which is
    // the documented consequence of not servicing child mode. What must not
    // happen is that it spawns a third level.
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("SCRIPT-RAN"),
        "the script cannot have run: the host never serviced child mode"
    );
    let _ = fs::remove_dir_all(&dir);
}
