//! An in-process permission hook does not cross the subprocess boundary.
//!
//! The hook is process-global but process-local: `run_in_subprocess` starts a
//! fresh process that never installed it, so the child enforces its own flags.
//! This is documented in `docs/api.md`; the test pins the behavior so a future
//! change to the child handshake cannot silently make it look covered.
//!
//! Lives in its own binary: installing a hook is process-global and install-once.

#![cfg(unix)]

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use libdeno::{install_permission_hook, LibdenoOptions};

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("libdeno-hook-scope-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_deny_all_hook_does_not_restrict_subprocess_runs() {
    let dir = temp_dir("scope");
    let secret = dir.join("secret.txt");
    let result = dir.join("result.txt");
    fs::write(&secret, "SECRET").unwrap();
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        format!(
            "let out;\n\
             try {{ out = 'READ:' + Deno.readTextFileSync({secret:?}).trim(); }}\n\
             catch (e) {{ out = 'DENIED:' + e.name; }}\n\
             Deno.writeTextFileSync({result:?}, out);"
        ),
    )
    .unwrap();

    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    install_permission_hook(Arc::new(move |_request| {
        counter.fetch_add(1, Ordering::SeqCst);
        false
    }))
    .expect("hook install");

    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };

    // In-process: the hook is the sole authority, so the read is denied and the
    // script's own write fails too (nothing is recorded).
    let _ = libdeno::run(&entry, &options);
    assert!(!result.exists(), "the deny hook must block every check");
    assert!(
        calls.load(Ordering::SeqCst) > 0,
        "the in-process run must consult the hook"
    );

    // Subprocess: a fresh process that never installed the hook, so allow-all
    // is a real grant.
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    let before = calls.load(Ordering::SeqCst);
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 0, "the child has no hook and must run normally");
    assert_eq!(
        fs::read_to_string(&result).unwrap(),
        "READ:SECRET",
        "the child's own flags decide; the host hook is not consulted"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        before,
        "a subprocess run must never call the host's hook"
    );
    let _ = fs::remove_dir_all(dir);
}
