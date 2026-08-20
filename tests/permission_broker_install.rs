//! Install semantics for `install_permission_broker` (broker-first ordering).
//!
//! Lives in its own integration-test binary because the broker is
//! process-global and install-once: a successful install here would poison
//! every other test in the same binary. This binary tests the broker-first
//! sequence (broker installed first, then a second broker and the hook are
//! rejected) plus the construction error a broker does not decide. The
//! hook-first sequence lives in permission_install.rs — the two orderings are
//! mutually exclusive in one process.

#![cfg(unix)]

use std::fs;
use std::io::BufRead;
use std::io::Write;
use std::os::unix::net::UnixListener;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use libdeno::{
    install_permission_broker, install_permission_hook, run, LibdenoError, LibdenoOptions,
    PermissionPrompt, PermissionRequest,
};

#[test]
fn broker_install_is_install_once_and_excludes_the_hook() {
    let dir = std::env::temp_dir().join(format!("libdeno-broker-install-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("broker.sock");

    // PermissionBroker::new is the connector, so it needs a live listener:
    // bind one and answer the JSON-line protocol on it (deny by default, so
    // even an unexpected check cannot deadlock the run below).
    let listener = UnixListener::bind(&socket).unwrap();
    let server_deny = Arc::new(AtomicBool::new(true));
    let server = {
        let server_deny = server_deny.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break, // broker disconnected
                        Ok(_) => {}
                    }
                    // The only field we need back is the echoed id (the
                    // upstream broker validates the id match).
                    let id = line
                        .find("\"id\":")
                        .and_then(|i| {
                            let digits: String = line[i + 5..]
                                .chars()
                                .take_while(|c| c.is_ascii_digit())
                                .collect();
                            digits.parse::<u32>().ok()
                        })
                        .unwrap_or(0);
                    let result = if server_deny.load(Ordering::SeqCst) {
                        "deny"
                    } else {
                        "allow"
                    };
                    writeln!(stream, "{{\"id\":{id},\"result\":\"{result}\"}}").unwrap();
                }
            }
        })
    };

    // First install succeeds; a second is rejected (install-once).
    install_permission_broker(&socket).unwrap();
    let err = install_permission_broker(&socket).unwrap_err();
    assert!(
        matches!(err, LibdenoError::Permission(_)),
        "a second broker install must error, got: {err:?}"
    );

    // The broker and the hook are mutually exclusive: once a broker is
    // installed, the in-process hook must be refused.
    let allow: PermissionPrompt = Arc::new(|_req: &PermissionRequest| true);
    let err = install_permission_hook(allow).unwrap_err();
    assert!(
        matches!(err, LibdenoError::Permission(_)),
        "a hook install after a broker must error, got: {err:?}"
    );

    // A broker decides checks, not construction: an empty permissions list
    // still fails at run-construction time with a Configuration error before
    // any check could reach the broker.
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('never runs');").unwrap();
    let err = run(&entry, &LibdenoOptions::default()).unwrap_err();
    assert!(
        matches!(err, LibdenoError::Configuration(_)),
        "a broker must not decide construction, got: {err:?}"
    );

    // Invalid V8 heap constraints are rejected as configuration before the
    // entry module executes, rather than being passed through to V8 as 0.
    let invalid_heap = LibdenoOptions {
        allow_all_permissions: true,
        max_heap_bytes: Some(0),
        ..Default::default()
    };
    let err = run(&entry, &invalid_heap).unwrap_err();
    assert!(
        matches!(err, LibdenoError::Configuration(ref message) if message.contains("max_heap_bytes")),
        "invalid heap size must be a Configuration error, got: {err:?}"
    );

    drop(server);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_external_broker_preserves_upstream_exit_87_boundary() {
    // PermissionBroker::new is upstream's non-fallible constructor: an
    // initial connection failure exits with 87 rather than returning a
    // LibdenoError. Keep this process separate because exit(87) is deliberate.
    let path = std::env::temp_dir().join(format!(
        "libdeno-missing-broker-{}-{}.sock",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_broker_host"))
        .env("LIBDENO_TEST_BROKER_PATH", &path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(87));
}
