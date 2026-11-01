//! `phase-diagnostics` is only useful if a caller can actually get the data
//! out of a real execution.
//!
//! The unit test in `src/executor.rs` builds an `ExecutionReport` by hand and
//! checks sanitization. This one runs a real script through `Executor::execute`
//! and asserts the phases that must be present are present, and that no secret
//! from the run's options leaks into the debug output.

#![cfg(feature = "phase-diagnostics")]

use std::fs;
use std::path::PathBuf;

use libdeno::{ExecutionBackend, ExecutionRequest, Executor, LibdenoOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-diag-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_real_execution_reports_phase_diagnostics() {
    let dir = temp_dir("real");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('hello');").unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executor = runtime.block_on(Executor::builder(&dir).build()).unwrap();

    let secret = "super-secret-token-value";
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        // A value that must never appear in a diagnostic dump.
        args: vec![secret.to_string()],
        ..Default::default()
    };
    let result = executor
        .execute(ExecutionRequest::new(&entry, options))
        .expect("the script must run");
    let diagnostics = result.report().phase_diagnostics();

    // Phases the in-process path must always observe.
    assert!(
        diagnostics.user_execution_ns.is_some(),
        "the script actually ran, so user_execution must be recorded"
    );
    assert!(
        diagnostics.main_worker_bootstrap_ns.is_some(),
        "an isolate was created, so bootstrap must be recorded"
    );
    // Nothing was queued, so the always-present counters stay zero.
    assert_eq!(diagnostics.queue_wait_ns, 0);

    // Sanitization: options and script contents must not leak into the report.
    let dump = format!("{diagnostics:?}\n{:?}", result.report());
    for forbidden in [secret, "hello", "--allow-read"] {
        assert!(
            !dump.contains(forbidden),
            "diagnostics leaked {forbidden:?}: {dump}"
        );
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn diagnostics_are_available_for_a_subprocess_execution() {
    let dir = temp_dir("subprocess");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('child');").unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executor = runtime
        .block_on(
            Executor::builder(&dir)
                .backend(ExecutionBackend::Subprocess)
                .host_executable(env!("CARGO_BIN_EXE_child_host"))
                .build(),
        )
        .unwrap();
    let result = executor
        .execute(ExecutionRequest::new(
            &entry,
            LibdenoOptions {
                allow_all_permissions: true,
                capture_stdout: true,
                ..Default::default()
            },
        ))
        .expect("the child must run");
    let diagnostics = result.report().phase_diagnostics();
    // The parent runs no script for a subprocess execution: the child owns
    // graph building and user execution, so those phases are absent here and
    // only the parent-side pipe work is recorded.
    assert!(
        diagnostics.user_execution_ns.is_none() && diagnostics.graph_build_ns.is_none(),
        "subprocess phase data describes the parent, not the child"
    );
    assert!(
        diagnostics.output_drain_ns.is_some(),
        "the parent drained the child's pipes, so output_drain must be recorded"
    );
    // Platform-gated resource counters: RSS and thread count are Linux-only,
    // fds are available on Unix.
    #[cfg(target_os = "linux")]
    assert!(diagnostics.parent_rss_bytes_before.is_some());
    #[cfg(target_os = "macos")]
    assert!(
        diagnostics.parent_rss_bytes_before.is_none(),
        "ru_maxrss is a peak, not a current value, so macOS reports nothing"
    );
    #[cfg(unix)]
    assert!(diagnostics.parent_fds_before.is_some());
    let _ = fs::remove_dir_all(dir);
}
