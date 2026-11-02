//! A failing script is reported in a different shape per backend. These tests
//! pin the current shapes so a change is deliberate rather than silent.

use std::fs;
use std::path::PathBuf;

use libdeno::{ExecutionBackend, ExecutionRequest, Executor, LibdenoOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-backend-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn executor(dir: &PathBuf, backend: ExecutionBackend) -> Executor {
    let mut builder = Executor::builder(dir);
    if matches!(backend, ExecutionBackend::Subprocess) {
        builder = builder
            .backend(ExecutionBackend::Subprocess)
            .host_executable(env!("CARGO_BIN_EXE_child_host"));
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(builder.build())
        .unwrap()
}

/// In-process capture is refused on Windows, so partial output cannot be
/// observed there; the subprocess case below covers the same shape.
#[cfg(not(windows))]
#[test]
fn in_process_backend_reports_a_thrown_error_as_failure_with_partial_output() {
    let dir = temp_dir("in-process");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "console.log('before-throw'); throw new Error('script-boom');",
    )
    .unwrap();
    let failure = executor(&dir, ExecutionBackend::InProcess)
        .execute(ExecutionRequest::new(
            &entry,
            LibdenoOptions {
                allow_all_permissions: true,
                capture_stdout: true,
                ..Default::default()
            },
        ))
        .unwrap_err();
    assert!(failure.error().to_string().contains("script-boom"));
    let partial = failure
        .partial_output()
        .expect("in-process capture must survive a thrown error");
    assert!(String::from_utf8_lossy(partial.stdout()).contains("before-throw"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn subprocess_backend_reports_a_thrown_error_as_an_exit_code() {
    let dir = temp_dir("subprocess");
    let entry = dir.join("main.js");
    fs::write(&entry, "throw new Error('script-boom');").unwrap();
    // A script exception becomes the child's exit status, so the caller sees a
    // successful execution with exit code 1 and the diagnostic on stderr. It is
    // indistinguishable from an explicit `Deno.exit(1)`.
    let result = executor(&dir, ExecutionBackend::Subprocess)
        .execute(ExecutionRequest::new(
            &entry,
            LibdenoOptions {
                allow_all_permissions: true,
                capture_stderr: true,
                ..Default::default()
            },
        ))
        .expect("the subprocess backend reports a script exception as an exit code");
    assert_eq!(result.exit_code(), 1);
    let stderr = String::from_utf8_lossy(result.output().stderr()).to_string();
    assert!(stderr.contains("libdeno child run failed"), "{stderr}");
    assert!(stderr.contains("script-boom"), "{stderr}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn subprocess_backend_cannot_distinguish_a_crash_from_deno_exit_one() {
    let dir = temp_dir("subprocess-exit-one");
    let crashed = dir.join("crashed.js");
    fs::write(&crashed, "throw new Error('script-boom');").unwrap();
    let exited = dir.join("exited.js");
    fs::write(&exited, "Deno.exit(1);").unwrap();
    let executor = executor(&dir, ExecutionBackend::Subprocess);
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let crashed_code = executor
        .execute(ExecutionRequest::new(&crashed, options.clone()))
        .unwrap()
        .exit_code();
    let exited_code = executor
        .execute(ExecutionRequest::new(&exited, options))
        .unwrap()
        .exit_code();
    assert_eq!(crashed_code, exited_code);
    let _ = fs::remove_dir_all(dir);
}
