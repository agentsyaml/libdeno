//! Install semantics for the in-process permission hook (hook-first ordering).
//!
//! Lives in its own integration-test binary because the hook installs a
//! process-global broker: a successful install here would poison every other
//! test in the same binary. This binary tests the hook-first sequence (hook
//! installed first, then the broker and a second hook are rejected). The
//! broker-first sequence lives in permission_broker_install.rs — the two
//! orderings are mutually exclusive in one process.

#![cfg(unix)]

use std::fs;
use std::sync::Arc;

use libdeno::{
    install_permission_broker, install_permission_hook, run, LibdenoError, LibdenoOptions,
    PermissionPrompt, PermissionRequest,
};

#[test]
fn hook_install_is_install_once_and_excludes_the_broker() {
    let dir = std::env::temp_dir().join(format!("libdeno-hook-install-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let allow: PermissionPrompt = Arc::new(|_req: &PermissionRequest| true);

    // First install succeeds; a second is rejected (install-once).
    install_permission_hook(allow.clone()).unwrap();
    let err = install_permission_hook(allow).unwrap_err();
    assert!(
        matches!(err, LibdenoError::Permission(_)),
        "a second hook install must error, got: {err:?}"
    );

    // The hook and the broker are mutually exclusive: once the hook (and its
    // internal broker) is installed, an external broker must be refused. The
    // path never needs to exist — the install-once check fires first.
    let err = install_permission_broker(dir.join("never-bound.sock")).unwrap_err();
    assert!(
        matches!(err, LibdenoError::Permission(_)),
        "a broker install after a hook must error, got: {err:?}"
    );

    // A hook (or broker) decides checks, not construction: an empty
    // permissions list still fails at run-construction time with a Permission
    // error before any check could reach the hook.
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('never runs');").unwrap();
    let err = run(&entry, &LibdenoOptions::default()).unwrap_err();
    assert!(
        matches!(err, LibdenoError::Permission(_)),
        "a hook must not decide construction, got: {err:?}"
    );

    let _ = fs::remove_dir_all(&dir);
}
