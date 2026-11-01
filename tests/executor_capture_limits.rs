//! The same executor, backend, and request produce different capture limits
//! depending on whether the call goes through `execute` or the execution-control
//! `submit` scheduler: `submit` routes to the supervisor, which caps capture at
//! 64 KiB by default and rejects anything above 96 KiB.

#![cfg(feature = "execution-control")]

use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use libdeno::{
    AdmissionConfig, ExecutionBackend, ExecutionError, ExecutionRequest, Executor, LibdenoError,
    LibdenoOptions, SubmissionOptions,
};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-cap-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn executor(dir: &Path) -> (Executor, tokio::runtime::Runtime) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executor = runtime
        .block_on(
            Executor::builder(dir)
                .backend(ExecutionBackend::Subprocess)
                .host_executable(env!("CARGO_BIN_EXE_child_host"))
                .admission(AdmissionConfig::new(NonZeroUsize::new(1).unwrap(), 0))
                .build(),
        )
        .unwrap();
    (executor, runtime)
}

/// 200 KiB of stdout, one console.log per 100-byte line.
fn verbose_entry(dir: &Path) -> PathBuf {
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "const line = 'y'.repeat(99);\n\
         for (let i = 0; i < 2000; i++) console.log(line);",
    )
    .unwrap();
    entry
}

#[test]
fn submit_caps_capture_at_the_supervisor_default_while_execute_does_not() {
    let dir = temp_dir("default");
    let entry = verbose_entry(&dir);
    let (executor, runtime) = executor(&dir);
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    };

    let executed = executor
        .execute(ExecutionRequest::new(&entry, options.clone()))
        .unwrap();
    assert_eq!(executed.output().stdout().len(), 200_000);
    assert!(!executed.output().capture_truncated());

    let handle = executor
        .submit(
            ExecutionRequest::new(&entry, options),
            SubmissionOptions::default(),
        )
        .unwrap();
    let submitted = runtime.block_on(handle.result()).unwrap();
    assert_eq!(submitted.output().stdout().len(), 64 * 1024);
    assert!(submitted.output().capture_truncated());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn submit_rejects_a_capture_cap_that_execute_accepts() {
    let dir = temp_dir("large-cap");
    let entry = verbose_entry(&dir);
    let (executor, runtime) = executor(&dir);
    // 200 KiB is fine in-process and through the legacy subprocess path, but
    // above the supervisor's 96 KiB per-stream maximum.
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        max_capture_bytes: Some(200 * 1024),
        ..Default::default()
    };

    let executed = executor
        .execute(ExecutionRequest::new(&entry, options.clone()))
        .unwrap();
    assert_eq!(executed.output().stdout().len(), 200_000);

    let handle = executor
        .submit(
            ExecutionRequest::new(&entry, options),
            SubmissionOptions::default(),
        )
        .unwrap();
    let failure = runtime.block_on(handle.result()).unwrap_err();
    match failure.error() {
        ExecutionError::Libdeno(LibdenoError::Configuration(message)) => {
            assert!(message.contains("per-stream maximum"), "{message}");
        }
        other => panic!("expected a Configuration error, got: {other:?}"),
    }
    let _ = fs::remove_dir_all(&dir);
}
