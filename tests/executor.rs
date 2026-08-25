use std::fs;
use std::future::Future;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[cfg(feature = "execution-control")]
use std::process::{Command, Stdio};

#[cfg(feature = "execution-control")]
use std::num::NonZeroUsize;

use libdeno::{
    CapabilityAvailability, CapabilityOutcome, ExecutionBackend, ExecutionCapability,
    ExecutionError, ExecutionRequest, Executor, LibdenoError, LibdenoOptions,
};

#[cfg(feature = "execution-control")]
use libdeno::{
    AdmissionConfig, CancelOutcome, ExecutionCleanupStrength, ExecutionState,
    ExecutionTransportStatus, SubmissionOptions, SubmitError,
};

static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(feature = "execution-control")]
static HOST_PREWARM: std::sync::Once = std::sync::Once::new();

#[cfg(feature = "execution-control")]
fn prewarm_child_host() {
    HOST_PREWARM.call_once(|| {
        let _ = Command::new(env!("CARGO_BIN_EXE_child_host"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-executor-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn build(executor: libdeno::ExecutorBuilder) -> Executor {
    #[cfg(feature = "execution-control")]
    prewarm_child_host();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(executor.build())
        .unwrap()
}

fn allow_all() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    }
}

#[test]
fn capability_report_and_process_pool_are_typed() {
    let report = Executor::builder(".").capability_report();
    assert_eq!(
        report.availability(ExecutionCapability::Backend(ExecutionBackend::InProcess)),
        CapabilityAvailability::Available
    );
    assert_eq!(
        report.availability(ExecutionCapability::Backend(ExecutionBackend::Subprocess)),
        CapabilityAvailability::Available
    );
    assert_eq!(
        report.availability(ExecutionCapability::Backend(ExecutionBackend::ProcessPool)),
        CapabilityAvailability::Unsupported
    );
    assert_eq!(
        report.availability(ExecutionCapability::HardSandbox),
        CapabilityAvailability::Unsupported
    );
    #[cfg(feature = "execution-control")]
    assert!(report.cleanup_strength().is_some());
    #[cfg(not(feature = "execution-control"))]
    assert!(report.cleanup_strength().is_none());

    let dir = temp_dir("pool");
    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(
            Executor::builder(&dir)
                .backend(ExecutionBackend::ProcessPool)
                .build(),
        )
        .unwrap_err();
    match error {
        ExecutionError::Unsupported(unsupported) => {
            assert_eq!(
                unsupported.capability(),
                ExecutionCapability::Backend(ExecutionBackend::ProcessPool)
            );
            assert!(unsupported.reason().contains("not available"));
            assert!(unsupported.to_string().contains("unsupported capability"));
        }
        other => panic!("expected typed unsupported error, got {other:?}"),
    }
    let _ = fs::remove_dir_all(dir);
}

#[cfg(not(windows))]
#[test]
fn inprocess_sync_and_async_selective_capture() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("inprocess-capture");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "console.log('executor-out'); console.error('executor-err'); Deno.exit(4);",
    )
    .unwrap();
    let executor = build(Executor::builder(&dir));
    assert_eq!(executor.project_dir(), fs::canonicalize(&dir).unwrap());
    assert_eq!(executor.backend(), ExecutionBackend::InProcess);

    let sync = executor
        .execute(ExecutionRequest::new(
            &entry,
            LibdenoOptions {
                capture_stdout: true,
                ..allow_all()
            },
        ))
        .unwrap();
    assert_eq!(sync.exit_code(), 4);
    assert!(String::from_utf8_lossy(sync.output().stdout()).contains("executor-out"));
    assert!(sync.output().stderr().is_empty());
    assert_eq!(
        sync.report()
            .outcome(ExecutionCapability::Backend(ExecutionBackend::InProcess)),
        CapabilityOutcome::Used
    );
    assert_eq!(
        sync.report().dispatched_backend(),
        Some(ExecutionBackend::InProcess)
    );
    assert!(!sync.report().elapsed().is_zero());

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let asynchronous = runtime
        .block_on(executor.execute_async(ExecutionRequest::new(
            &entry,
            LibdenoOptions {
                capture_stderr: true,
                ..allow_all()
            },
        )))
        .unwrap();
    assert_eq!(asynchronous.exit_code(), 4);
    assert!(asynchronous.output().stdout().is_empty());
    assert!(String::from_utf8_lossy(asynchronous.output().stderr()).contains("executor-err"));
    assert_eq!(
        asynchronous
            .report()
            .outcome(ExecutionCapability::HardSandbox),
        CapabilityOutcome::NotRequested
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn subprocess_sync_uses_explicit_host_and_reports_dispatch() {
    let dir = temp_dir("subprocess-sync");
    let entry = dir.join("main.js");
    fs::write(&entry, "Deno.exit(6);").unwrap();
    let executor = build(
        Executor::builder(&dir)
            .backend(ExecutionBackend::Subprocess)
            .host_executable(env!("CARGO_BIN_EXE_child_host")),
    );
    let result = executor
        .execute(ExecutionRequest::new(&entry, allow_all()))
        .unwrap();
    assert_eq!(result.exit_code(), 6);
    assert!(result.output().stdout().is_empty());
    assert!(result.output().stderr().is_empty());
    assert_eq!(
        result.report().requested_backend(),
        ExecutionBackend::Subprocess
    );
    assert_eq!(
        result.report().dispatched_backend(),
        Some(ExecutionBackend::Subprocess)
    );
    assert_eq!(
        result
            .report()
            .outcome(ExecutionCapability::Backend(ExecutionBackend::Subprocess)),
        CapabilityOutcome::Used
    );
    assert!(result.report().cleanup_strength().is_none());
    assert!(result.report().transport_status().is_none());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn subprocess_async_uses_explicit_host_and_selective_capture() {
    let dir = temp_dir("subprocess-async");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('subprocess-async');").unwrap();
    let executor = build(
        Executor::builder(&dir)
            .backend(ExecutionBackend::Subprocess)
            .host_executable(env!("CARGO_BIN_EXE_child_host")),
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime
        .block_on(executor.execute_async(ExecutionRequest::new(
            &entry,
            LibdenoOptions {
                capture_stdout: true,
                ..allow_all()
            },
        )))
        .unwrap();
    assert_eq!(result.exit_code(), 0);
    assert!(String::from_utf8_lossy(result.output().stdout()).contains("subprocess-async"));
    assert!(result.output().stderr().is_empty());
    assert_eq!(
        result.report().dispatched_backend(),
        Some(ExecutionBackend::Subprocess)
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn subprocess_selective_capture_does_not_report_unrequested_truncation() {
    let dir = temp_dir("subprocess-selective");
    let entry = dir.join("main.js");
    // The requested stdout fits in the four-byte budget (`ok\n`); the one
    // short stderr marker does not. The selective helper must inherit stderr
    // rather than pipe it, so the marker is visible in the test process while
    // never becoming returned output or affecting truncation.
    fs::write(&entry, "console.log('ok'); console.error('ERRNOISE');").unwrap();
    let executor = build(
        Executor::builder(&dir)
            .backend(ExecutionBackend::Subprocess)
            .host_executable(env!("CARGO_BIN_EXE_child_host")),
    );
    let result = executor
        .execute(ExecutionRequest::new(
            &entry,
            LibdenoOptions {
                capture_stdout: true,
                max_capture_bytes: Some(4),
                ..allow_all()
            },
        ))
        .unwrap();
    assert!(String::from_utf8_lossy(result.output().stdout()).contains("ok"));
    assert!(result.output().stderr().is_empty());
    assert!(!result.output().capture_truncated());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cwd_mismatch_fails_before_dispatch() {
    let project = temp_dir("cwd-project");
    let other = temp_dir("cwd-other");
    let entry = project.join("main.js");
    fs::write(&entry, "Deno.exit(0);").unwrap();
    let executor = build(
        Executor::builder(&project)
            .backend(ExecutionBackend::Subprocess)
            .host_executable(env!("CARGO_BIN_EXE_child_host")),
    );
    let failure = executor
        .execute(ExecutionRequest::new(
            &entry,
            LibdenoOptions {
                cwd: Some(other.clone()),
                ..allow_all()
            },
        ))
        .unwrap_err();
    assert!(matches!(failure.error(), ExecutionError::Libdeno(_)));
    assert!(failure.error().to_string().contains("does not match"));
    assert_eq!(
        failure.report().requested_backend(),
        ExecutionBackend::Subprocess
    );
    assert_eq!(failure.report().dispatched_backend(), None);
    assert_eq!(
        failure
            .report()
            .outcome(ExecutionCapability::Backend(ExecutionBackend::Subprocess)),
        CapabilityOutcome::Failed
    );
    assert!(failure.partial_output().is_none());
    let _ = fs::remove_dir_all(project);
    let _ = fs::remove_dir_all(other);
}

#[test]
fn runtime_failure_has_no_partial_output() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("runtime-failure");
    let entry = dir.join("main.js");
    fs::write(&entry, "throw new Error('executor-failure');").unwrap();
    let executor = build(Executor::builder(&dir));
    let failure = executor
        .execute(ExecutionRequest::new(&entry, allow_all()))
        .unwrap_err();
    assert!(matches!(failure.error(), ExecutionError::Libdeno(_)));
    assert!(failure.error().to_string().contains("executor-failure"));
    assert!(failure.partial_output().is_none());
    assert_eq!(
        failure.report().dispatched_backend(),
        Some(ExecutionBackend::InProcess)
    );
    assert_eq!(
        failure
            .report()
            .outcome(ExecutionCapability::Backend(ExecutionBackend::InProcess)),
        CapabilityOutcome::Failed
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn execution_failure_exposes_standard_source_chain() {
    use std::error::Error;

    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("failure-source");
    let entry = dir.join("main.js");
    fs::write(&entry, "throw new Error('source-chain');").unwrap();
    let executor = build(Executor::builder(&dir));
    let failure = executor
        .execute(ExecutionRequest::new(&entry, allow_all()))
        .unwrap_err();

    assert!(matches!(failure.error(), ExecutionError::Libdeno(_)));
    let failure_error: &dyn Error = &failure;
    let execution_error = failure_error
        .source()
        .expect("ExecutionFailure must expose ExecutionError as its source");
    assert!(execution_error.downcast_ref::<ExecutionError>().is_some());
    let libdeno_error = execution_error
        .source()
        .expect("ExecutionError must expose LibdenoError as its source");
    assert!(libdeno_error.downcast_ref::<LibdenoError>().is_some());
    assert!(libdeno_error.to_string().contains("source-chain"));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn dropped_async_subprocess_future_does_not_stop_started_child() {
    let dir = temp_dir("subprocess-drop");
    let entry = dir.join("main.js");
    let started = dir.join("started.marker");
    let completed = dir.join("completed.marker");
    fs::write(
        &entry,
        format!(
            "Deno.writeTextFileSync({started:?}, 'started');\n\
             await new Promise((resolve) => setTimeout(resolve, 100));\n\
             Deno.writeTextFileSync({completed:?}, 'completed');"
        ),
    )
    .unwrap();
    let executor = build(
        Executor::builder(&dir)
            .backend(ExecutionBackend::Subprocess)
            .host_executable(env!("CARGO_BIN_EXE_child_host")),
    );
    let request = ExecutionRequest::new(&entry, allow_all());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // This intentionally drops the !Send execute_async future after the child
    // proves it started. It is evidence for spawn_blocking's documented
    // behavior, not a cancellation contract: the child must still complete.
    let started_deadline = Instant::now() + Duration::from_secs(15);
    runtime.block_on(async {
        let mut future = Box::pin(executor.execute_async(request));
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        loop {
            assert!(
                Instant::now() < started_deadline,
                "subprocess did not create its started marker"
            );
            if started.exists() {
                break;
            }
            match future.as_mut().poll(&mut context) {
                std::task::Poll::Pending => {}
                std::task::Poll::Ready(result) => {
                    panic!("subprocess completed before its started marker: {result:?}")
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        drop(future);
    });

    let completion_deadline = Instant::now() + Duration::from_secs(5);
    while !completed.exists() {
        assert!(
            Instant::now() < completion_deadline,
            "started child did not write its completion marker after future drop"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(fs::read_to_string(&completed).unwrap(), "completed");
    let _ = fs::remove_dir_all(dir);
}

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn executor_is_send_and_sync() {
    assert_send_sync::<Executor>();
}

#[cfg(feature = "execution-control")]
mod execution_control {
    use super::*;

    fn build_admission(dir: &PathBuf, active_limit: usize, queue_limit: usize) -> Executor {
        build(Executor::builder(dir).admission(AdmissionConfig::new(
            NonZeroUsize::new(active_limit).unwrap(),
            queue_limit,
        )))
    }

    fn wait_for(path: &PathBuf) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !path.exists() {
            assert!(Instant::now() < deadline, "timed out waiting for {path:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn blocking_script(started: &PathBuf, completed: &PathBuf, delay_ms: u64) -> String {
        format!(
            "Deno.writeTextFileSync({started:?}, 'started');\n\
             await new Promise((resolve) => setTimeout(resolve, {delay_ms}));\n\
             Deno.writeTextFileSync({completed:?}, 'completed');"
        )
    }

    fn busy_script(started: &PathBuf) -> String {
        format!(
            "Deno.writeTextFileSync({started:?}, 'started');\n\
             while (true) {{}}"
        )
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn admission_rejects_third_before_backend_and_fifo_runs() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-admission");
        let first_entry = dir.join("first.js");
        let second_entry = dir.join("second.js");
        let first_started = dir.join("first.started");
        let first_completed = dir.join("first.completed");
        let second_marker = dir.join("second.marker");
        let third_marker = dir.join("third.marker");
        fs::write(
            &first_entry,
            blocking_script(&first_started, &first_completed, 150),
        )
        .unwrap();
        fs::write(
            &second_entry,
            format!("Deno.writeTextFileSync({second_marker:?}, 'second');"),
        )
        .unwrap();
        let executor = build_admission(&dir, 1, 1);
        let first = executor
            .submit(
                ExecutionRequest::new(&first_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        wait_for(&first_started);
        let second = executor
            .submit(
                ExecutionRequest::new(&second_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        let third = executor.submit(
            ExecutionRequest::new(dir.join("third.js"), allow_all()),
            SubmissionOptions::default(),
        );
        assert!(matches!(third, Err(SubmitError::QueueFull)));

        let rt = runtime();
        assert!(rt.block_on(first.result()).is_ok());
        assert!(rt.block_on(second.result()).is_ok());
        assert!(second_marker.exists());
        assert!(!third_marker.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn preexpired_inprocess_submission_does_not_start_user_code() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-preexpired-inprocess");
        let entry = dir.join("main.js");
        let started = dir.join("started");
        fs::write(
            &entry,
            format!("Deno.writeTextFileSync({started:?}, 'started');"),
        )
        .unwrap();
        let executor = build_admission(&dir, 1, 0);
        let handle = executor
            .submit(
                ExecutionRequest::new(&entry, allow_all()),
                SubmissionOptions::new(Some(Duration::ZERO)),
            )
            .unwrap();
        let failure = runtime().block_on(handle.result()).unwrap_err();
        assert!(matches!(
            failure.error(),
            ExecutionError::Libdeno(LibdenoError::Timeout(_))
        ));
        assert!(!started.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn overflowing_request_timeout_is_rejected_before_admission() {
        let dir = temp_dir("control-overflow-timeout");
        let executor = build_admission(&dir, 1, 0);
        let result = executor.submit(
            ExecutionRequest::new(dir.join("missing.js"), allow_all()),
            SubmissionOptions::new(Some(Duration::MAX)),
        );
        assert!(matches!(result, Err(SubmitError::InvalidTimeout)));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn queued_work_is_fifo() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-fifo");
        let first_entry = dir.join("first.js");
        let second_entry = dir.join("second.js");
        let third_entry = dir.join("third.js");
        let first_started = dir.join("first.started");
        let first_completed = dir.join("first.completed");
        let order = dir.join("order.txt");
        fs::write(
            &first_entry,
            blocking_script(&first_started, &first_completed, 120),
        )
        .unwrap();
        fs::write(
            &second_entry,
            format!("Deno.writeTextFileSync({order:?}, '2', {{ append: true }});"),
        )
        .unwrap();
        fs::write(
            &third_entry,
            format!("Deno.writeTextFileSync({order:?}, '3', {{ append: true }});"),
        )
        .unwrap();
        let executor = build_admission(&dir, 1, 2);
        let first = executor
            .submit(
                ExecutionRequest::new(&first_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        wait_for(&first_started);
        let second = executor
            .submit(
                ExecutionRequest::new(&second_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        let third = executor
            .submit(
                ExecutionRequest::new(&third_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        let rt = runtime();
        assert!(rt.block_on(first.result()).is_ok());
        assert!(rt.block_on(second.result()).is_ok());
        assert!(rt.block_on(third.result()).is_ok());
        assert_eq!(fs::read_to_string(order).unwrap(), "23");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn error_terminal_releases_active_permit() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-error-release");
        let error_entry = dir.join("error.js");
        let success_entry = dir.join("success.js");
        fs::write(&error_entry, "throw new Error('control-error');").unwrap();
        fs::write(&success_entry, "Deno.exit(0);").unwrap();
        let executor = build_admission(&dir, 1, 0);
        let failed = executor
            .submit(
                ExecutionRequest::new(&error_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        let rt = runtime();
        assert!(rt.block_on(failed.result()).is_err());
        let success = executor
            .submit(
                ExecutionRequest::new(&success_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        assert!(rt.block_on(success.result()).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn queued_cancel_and_deadline_do_not_start_and_release_queue_capacity() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-queued-cancel");
        let first_entry = dir.join("first.js");
        let second_entry = dir.join("second.js");
        let deadline_entry = dir.join("deadline.js");
        let third_entry = dir.join("third.js");
        let started = dir.join("first.started");
        let completed = dir.join("first.completed");
        let cancelled_marker = dir.join("cancelled.marker");
        let deadline_marker = dir.join("deadline.marker");
        let third_marker = dir.join("third.marker");
        fs::write(&first_entry, blocking_script(&started, &completed, 250)).unwrap();
        fs::write(
            &second_entry,
            format!("Deno.writeTextFileSync({cancelled_marker:?}, 'cancelled');"),
        )
        .unwrap();
        fs::write(
            &deadline_entry,
            format!("Deno.writeTextFileSync({deadline_marker:?}, 'deadline');"),
        )
        .unwrap();
        fs::write(
            &third_entry,
            format!("Deno.writeTextFileSync({third_marker:?}, 'third');"),
        )
        .unwrap();
        let executor = build_admission(&dir, 1, 1);
        let first = executor
            .submit(
                ExecutionRequest::new(&first_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        wait_for(&started);
        let cancelled = executor
            .submit(
                ExecutionRequest::new(&second_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        assert_eq!(cancelled.cancel(), CancelOutcome::Requested);
        assert_eq!(cancelled.state(), ExecutionState::Terminated);

        let deadline = executor
            .submit(
                ExecutionRequest::new(&deadline_entry, allow_all()),
                SubmissionOptions::new(Some(Duration::from_millis(30))),
            )
            .unwrap();
        let rt = runtime();
        assert!(matches!(
            rt.block_on(cancelled.result()).unwrap_err().error(),
            ExecutionError::Cancelled
        ));
        assert!(matches!(
            rt.block_on(deadline.result()).unwrap_err().error(),
            ExecutionError::Libdeno(LibdenoError::Timeout(_))
        ));
        assert!(!cancelled_marker.exists());
        assert!(!deadline_marker.exists());

        let third = executor
            .submit(
                ExecutionRequest::new(&third_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        assert!(rt.block_on(first.result()).is_ok());
        assert!(rt.block_on(third.result()).is_ok());
        assert!(third_marker.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn started_cancel_and_deadline_are_terminal_and_best_effort() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-started-cancel");
        let cancel_entry = dir.join("cancel.js");
        let deadline_entry = dir.join("deadline.js");
        let cancel_started = dir.join("cancel.started");
        let deadline_started = dir.join("deadline.started");
        fs::write(&cancel_entry, busy_script(&cancel_started)).unwrap();
        fs::write(&deadline_entry, busy_script(&deadline_started)).unwrap();
        let executor = build_admission(&dir, 1, 0);
        let cancelled = executor
            .submit(
                ExecutionRequest::new(&cancel_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        wait_for(&cancel_started);
        assert_eq!(cancelled.cancel(), CancelOutcome::Requested);
        let rt = runtime();
        assert!(matches!(
            rt.block_on(cancelled.result()).unwrap_err().error(),
            ExecutionError::Cancelled
        ));

        let deadline = executor
            .submit(
                ExecutionRequest::new(&deadline_entry, allow_all()),
                // The request timeout includes admission and bootstrap; leave
                // enough budget for the started marker before testing the
                // deadline path itself.
                SubmissionOptions::new(Some(Duration::from_secs(2))),
            )
            .unwrap();
        wait_for(&deadline_started);
        assert!(matches!(
            rt.block_on(deadline.result()).unwrap_err().error(),
            ExecutionError::Libdeno(LibdenoError::Timeout(_))
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cancellation_only_idle_event_loop_has_bounded_grace() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-idle-cancel");
        let entry = dir.join("idle.js");
        let started = dir.join("idle.started");
        fs::write(
            &entry,
            format!(
                "Deno.writeTextFileSync({started:?}, 'started');\n\
                 await new Promise((resolve) => setTimeout(resolve, 60000));"
            ),
        )
        .unwrap();
        let executor = build_admission(&dir, 1, 0);
        let handle = executor
            .submit(
                ExecutionRequest::new(&entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        wait_for(&started);

        assert_eq!(handle.cancel(), CancelOutcome::Requested);
        let started_waiting = Instant::now();
        let rt = runtime();
        let result = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), handle.result()).await
        });
        let failure = result
            .expect("idle cancellation exceeded its bounded grace")
            .unwrap_err();
        assert!(matches!(failure.error(), ExecutionError::Cancelled));
        assert!(
            started_waiting.elapsed() < Duration::from_secs(5),
            "idle cancellation was not bounded: {:?}",
            started_waiting.elapsed()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cancellation_context_without_cancel_does_not_impose_deadline() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-no-cancel-timeout");
        let entry = dir.join("timer.js");
        fs::write(
            &entry,
            "await new Promise((resolve) => setTimeout(resolve, 2200));",
        )
        .unwrap();
        let executor = build_admission(&dir, 1, 0);
        let handle = executor
            .submit(
                ExecutionRequest::new(&entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        let rt = runtime();
        let result = rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(8), handle.result()).await
        });
        assert!(result
            .expect("ordinary timer run exceeded test bound")
            .is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cloned_handles_share_repeatable_terminal_result() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-repeatable-result");
        let entry = dir.join("main.js");
        fs::write(&entry, "Deno.exit(7);").unwrap();
        let executor = build_admission(&dir, 1, 0);
        let handle = executor
            .submit(
                ExecutionRequest::new(&entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        let first = handle.clone();
        let second = handle.clone();
        let rt = runtime();
        let (first, second) = rt.block_on(async { tokio::join!(first.result(), second.result()) });
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(first.exit_code(), 7);
        assert_eq!(second.exit_code(), 7);
        assert_eq!(
            first.report().requested_backend(),
            second.report().requested_backend()
        );
        assert_eq!(rt.block_on(handle.result()).unwrap().exit_code(), 7);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cloned_failed_results_preserve_error_source_and_partial_output_semantics() {
        use std::error::Error;

        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-repeatable-failure");
        let entry = dir.join("main.js");
        fs::write(&entry, "throw new Error('repeatable-failure');").unwrap();
        let executor = build_admission(&dir, 1, 0);
        let handle = executor
            .submit(
                ExecutionRequest::new(&entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        let first_handle = handle.clone();
        let second_handle = handle.clone();
        let rt = runtime();
        let (first, second) =
            rt.block_on(async { tokio::join!(first_handle.result(), second_handle.result()) });
        let (first_error, _, first_output) = first.unwrap_err().into_parts();
        let (second_error, _, second_output) = second.unwrap_err().into_parts();
        let (third_error, _, third_output) = rt.block_on(handle.result()).unwrap_err().into_parts();

        for error in [&first_error, &second_error, &third_error] {
            assert!(matches!(
                error,
                ExecutionError::Libdeno(LibdenoError::Core(_))
            ));
            assert!(error.to_string().contains("repeatable-failure"));
            let source = (error as &dyn Error)
                .source()
                .expect("cloned ExecutionError must retain its LibdenoError source");
            assert!(source.downcast_ref::<LibdenoError>().is_some());
        }
        // Current legacy backends do not expose error-time bytes; repeated
        // ownership must preserve that exact None semantics rather than drop
        // a potentially shared buffer.
        assert!(first_output.is_none());
        assert!(second_output.is_none());
        assert!(third_output.is_none());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn cloned_permission_failures_preserve_typed_classification_and_source() {
        use std::error::Error;

        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-repeatable-permission-failure");
        let entry = dir.join("main.js");
        fs::write(&entry, "Deno.exit(0);").unwrap();
        let executor = build_admission(&dir, 1, 0);
        let handle = executor
            .submit(
                ExecutionRequest::new(
                    &entry,
                    LibdenoOptions {
                        permissions: vec!["--allow-env".to_string()],
                        ..Default::default()
                    },
                ),
                SubmissionOptions::default(),
            )
            .unwrap();
        let first_handle = handle.clone();
        let second_handle = handle.clone();
        let rt = runtime();
        let (first, second) =
            rt.block_on(async { tokio::join!(first_handle.result(), second_handle.result()) });
        let failures = [
            first.unwrap_err(),
            second.unwrap_err(),
            rt.block_on(handle.result()).unwrap_err(),
        ];

        for failure in failures {
            let (error, _, partial_output) = failure.into_parts();
            let libdeno_error = match &error {
                ExecutionError::Libdeno(error) => error,
                other => panic!("expected a libdeno permission failure, got {other:?}"),
            };
            assert!(libdeno_error.is_permission_error());
            assert!(
                (libdeno_error as &dyn Error)
                    .source()
                    .and_then(|source| source.downcast_ref::<deno_core::error::CoreError>())
                    .is_some(),
                "permission failure lost its CoreError source"
            );

            let mut source = Some(libdeno_error as &(dyn Error + 'static));
            let mut has_permission_check_source = false;
            while let Some(error) = source {
                if error
                    .downcast_ref::<deno_runtime::deno_permissions::PermissionCheckError>()
                    .is_some()
                {
                    has_permission_check_source = true;
                    break;
                }
                source = error.source();
            }
            assert!(
                has_permission_check_source,
                "permission failure lost PermissionCheckError source"
            );
            assert!(partial_output.is_none());
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn release_and_submit_race_preserves_fifo() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-release-submit-fifo");
        let first_entry = dir.join("first.js");
        let second_entry = dir.join("second.js");
        let third_entry = dir.join("third.js");
        let first_started = dir.join("first.started");
        let order = dir.join("order.txt");
        fs::write(
            &first_entry,
            format!(
                "Deno.writeTextFileSync({first_started:?}, 'started');\n\
                 await new Promise((resolve) => setTimeout(resolve, 20));\n\
                 Deno.writeTextFileSync({order:?}, '1');"
            ),
        )
        .unwrap();
        fs::write(
            &second_entry,
            format!("Deno.writeTextFileSync({order:?}, '2', {{ append: true }});"),
        )
        .unwrap();
        fs::write(
            &third_entry,
            format!("Deno.writeTextFileSync({order:?}, '3', {{ append: true }});"),
        )
        .unwrap();
        let executor = build_admission(&dir, 1, 2);
        let rt = runtime();

        for _ in 0..16 {
            let _ = fs::remove_file(&first_started);
            let _ = fs::remove_file(&order);
            let first = executor
                .submit(
                    ExecutionRequest::new(&first_entry, allow_all()),
                    SubmissionOptions::default(),
                )
                .unwrap();
            wait_for(&first_started);
            let second = executor
                .submit(
                    ExecutionRequest::new(&second_entry, allow_all()),
                    SubmissionOptions::default(),
                )
                .unwrap();
            assert!(rt.block_on(first.result()).is_ok());

            // This submit races the manager's wake-up after the active permit
            // is released. It must remain behind the already queued request.
            let third = executor
                .submit(
                    ExecutionRequest::new(&third_entry, allow_all()),
                    SubmissionOptions::default(),
                )
                .unwrap();
            assert!(rt.block_on(second.result()).is_ok());
            assert!(rt.block_on(third.result()).is_ok());
            assert_eq!(fs::read_to_string(&order).unwrap(), "123");
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn dropping_all_caller_handles_does_not_release_permit_early() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-detach");
        let entry = dir.join("main.js");
        let started = dir.join("started");
        let completed = dir.join("completed");
        fs::write(&entry, blocking_script(&started, &completed, 200)).unwrap();
        let executor = build_admission(&dir, 1, 0);
        let handle = executor
            .submit(
                ExecutionRequest::new(&entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        wait_for(&started);
        assert!(matches!(
            executor.submit(
                ExecutionRequest::new(&entry, allow_all()),
                SubmissionOptions::default()
            ),
            Err(SubmitError::QueueFull)
        ));
        let future = handle.result();
        drop(future);
        drop(handle);
        wait_for(&completed);
        let deadline = Instant::now() + Duration::from_secs(5);
        let next = loop {
            match executor.submit(
                ExecutionRequest::new(&entry, allow_all()),
                SubmissionOptions::default(),
            ) {
                Ok(handle) => break handle,
                Err(SubmitError::QueueFull) => {
                    assert!(Instant::now() < deadline, "active permit was not released");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("unexpected submission error: {error}"),
            }
        };
        let rt = runtime();
        assert!(rt.block_on(next.result()).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn shutdown_stops_acceptance_cancels_queue_and_requests_active_cancel() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-shutdown");
        let active_entry = dir.join("active.js");
        let queued_entry = dir.join("queued.js");
        let active_started = dir.join("active.started");
        let queued_marker = dir.join("queued.marker");
        fs::write(&active_entry, busy_script(&active_started)).unwrap();
        fs::write(
            &queued_entry,
            format!("Deno.writeTextFileSync({queued_marker:?}, 'queued');"),
        )
        .unwrap();
        let executor = build_admission(&dir, 1, 1);
        let active = executor
            .submit(
                ExecutionRequest::new(&active_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        wait_for(&active_started);
        let queued = executor
            .submit(
                ExecutionRequest::new(&queued_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        let report = executor.shutdown(Duration::from_millis(20));
        assert_eq!(report.queued_cancelled(), 1);
        assert_eq!(report.active_cancel_requested(), 1);
        assert!(matches!(
            executor.submit(
                ExecutionRequest::new(&queued_entry, allow_all()),
                SubmissionOptions::default()
            ),
            Err(SubmitError::Shutdown)
        ));
        let rt = runtime();
        assert!(matches!(
            rt.block_on(queued.result()).unwrap_err().error(),
            ExecutionError::Cancelled
        ));
        assert!(matches!(
            rt.block_on(active.result()).unwrap_err().error(),
            ExecutionError::Cancelled
        ));
        assert!(!queued_marker.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn shutdown_returns_after_grace_for_already_cancelling_subprocess() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-shutdown-cancelling");
        let active_entry = dir.join("active.js");
        let queued_entry = dir.join("queued.js");
        let active_started = dir.join("active.started");
        let active_completed = dir.join("active.completed");
        let queued_marker = dir.join("queued.marker");
        fs::write(
            &active_entry,
            blocking_script(&active_started, &active_completed, 500),
        )
        .unwrap();
        fs::write(
            &queued_entry,
            format!("Deno.writeTextFileSync({queued_marker:?}, 'queued');"),
        )
        .unwrap();
        let executor = build(
            Executor::builder(&dir)
                .backend(ExecutionBackend::Subprocess)
                .host_executable(env!("CARGO_BIN_EXE_child_host"))
                .admission(AdmissionConfig::new(NonZeroUsize::new(1).unwrap(), 1)),
        );
        let active = executor
            .submit(
                ExecutionRequest::new(&active_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        wait_for(&active_started);
        let queued = executor
            .submit(
                ExecutionRequest::new(&queued_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        assert_eq!(active.cancel(), CancelOutcome::Requested);

        let shutdown_started = Instant::now();
        let report = executor.shutdown(Duration::from_millis(20));
        assert!(
            shutdown_started.elapsed() < Duration::from_millis(250),
            "shutdown remained blocked on the cancelling backend"
        );
        assert_eq!(report.queued_cancelled(), 1);
        assert_eq!(report.active_cancel_requested(), 0);
        assert!(matches!(
            active.state(),
            ExecutionState::Cancelling | ExecutionState::Terminated
        ));

        let rt = runtime();
        assert!(matches!(
            rt.block_on(active.result()).unwrap_err().error(),
            ExecutionError::Cancelled
        ));
        assert!(matches!(
            rt.block_on(queued.result()).unwrap_err().error(),
            ExecutionError::Cancelled
        ));
        assert!(!queued_marker.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn shutdown_partitions_accepted_and_started_work_deterministically() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-shutdown-partition");
        let executor = build_admission(&dir, 4, 0);
        let mut handles = Vec::new();
        for index in 0..4 {
            let entry = dir.join(format!("busy-{index}.js"));
            fs::write(&entry, "while (true) {}").unwrap();
            handles.push(
                executor
                    .submit(
                        ExecutionRequest::new(&entry, allow_all()),
                        SubmissionOptions::default(),
                    )
                    .unwrap(),
            );
        }

        let report = executor.shutdown(Duration::ZERO);
        assert_eq!(report.queued_cancelled(), 0);
        assert_eq!(
            report.accepted_not_started_cancelled() + report.active_cancel_requested(),
            handles.len(),
            "shutdown must classify every admitted task exactly once"
        );

        let rt = runtime();
        for handle in &handles {
            let result = rt.block_on(async {
                tokio::time::timeout(Duration::from_secs(5), handle.result()).await
            });
            assert!(matches!(
                result
                    .expect("shutdown task did not reach terminal state")
                    .unwrap_err()
                    .error(),
                ExecutionError::Cancelled
            ));
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn tokio_reentry_submission_has_no_deadlock() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-reentry");
        let entry = dir.join("main.js");
        fs::write(&entry, "Deno.exit(0);").unwrap();
        let executor = build_admission(&dir, 1, 0);
        let rt = runtime();
        let result = rt.block_on(async {
            let handle = executor
                .submit(
                    ExecutionRequest::new(&entry, allow_all()),
                    SubmissionOptions::default(),
                )
                .unwrap();
            handle.result().await
        });
        assert!(result.is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn started_subprocess_cancel_reports_requested_and_waits() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-subprocess-cancel");
        let entry = dir.join("main.js");
        let started = dir.join("started");
        let completed = dir.join("completed");
        let queued_entry = dir.join("queued.js");
        let queued_marker = dir.join("queued.marker");
        fs::write(&entry, blocking_script(&started, &completed, 100)).unwrap();
        fs::write(
            &queued_entry,
            format!("Deno.writeTextFileSync({queued_marker:?}, 'queued');"),
        )
        .unwrap();
        let executor = build(
            Executor::builder(&dir)
                .backend(ExecutionBackend::Subprocess)
                .host_executable(env!("CARGO_BIN_EXE_child_host"))
                .admission(AdmissionConfig::new(NonZeroUsize::new(1).unwrap(), 1)),
        );
        let handle = executor
            .submit(
                ExecutionRequest::new(&entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        wait_for(&started);
        assert_eq!(handle.cancel(), CancelOutcome::Requested);
        assert_eq!(handle.state(), ExecutionState::Cancelling);
        let queued = executor
            .submit(
                ExecutionRequest::new(&queued_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        let rt = runtime();
        let failure = rt.block_on(handle.result()).unwrap_err();
        assert!(matches!(failure.error(), ExecutionError::Cancelled));
        assert_eq!(
            failure.report().cleanup_strength(),
            Some(ExecutionCleanupStrength::DirectChild)
        );
        assert!(matches!(
            failure.report().transport_status(),
            Some(ExecutionTransportStatus::Clean | ExecutionTransportStatus::Failed)
        ));
        assert!(rt.block_on(queued.result()).is_ok());
        assert!(queued_marker.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn queued_subprocess_cancel_never_reaches_started() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-subprocess-queued-cancel");
        let active_entry = dir.join("active.js");
        let queued_entry = dir.join("queued.js");
        let active_started = dir.join("active.started");
        let active_completed = dir.join("active.completed");
        let queued_marker = dir.join("queued.marker");
        fs::write(
            &active_entry,
            blocking_script(&active_started, &active_completed, 250),
        )
        .unwrap();
        fs::write(
            &queued_entry,
            format!("Deno.writeTextFileSync({queued_marker:?}, 'queued');"),
        )
        .unwrap();
        let executor = build(
            Executor::builder(&dir)
                .backend(ExecutionBackend::Subprocess)
                .host_executable(env!("CARGO_BIN_EXE_child_host"))
                .admission(AdmissionConfig::new(NonZeroUsize::new(1).unwrap(), 1)),
        );
        let active = executor
            .submit(
                ExecutionRequest::new(&active_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        wait_for(&active_started);
        let queued = executor
            .submit(
                ExecutionRequest::new(&queued_entry, allow_all()),
                SubmissionOptions::default(),
            )
            .unwrap();
        assert_eq!(queued.state(), ExecutionState::Queued);
        assert_eq!(queued.cancel(), CancelOutcome::Requested);
        assert_eq!(queued.state(), ExecutionState::Terminated);

        let rt = runtime();
        assert!(matches!(
            rt.block_on(queued.result()).unwrap_err().error(),
            ExecutionError::Cancelled
        ));
        assert!(!queued_marker.exists());
        assert!(rt.block_on(active.result()).is_ok());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn supervised_submit_reports_started_barrier_and_metadata() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp_dir("control-subprocess-report");
        let entry = dir.join("main.js");
        let started = dir.join("started");
        fs::write(
            &entry,
            format!(
                "Deno.writeTextFileSync({started:?}, 'started');\n\
                 await new Promise((resolve) => setTimeout(resolve, 1000));\n\
                 console.log('supervised-submit');"
            ),
        )
        .unwrap();
        let executor = build(
            Executor::builder(&dir)
                .backend(ExecutionBackend::Subprocess)
                .host_executable(env!("CARGO_BIN_EXE_child_host")),
        );
        let handle = executor
            .submit(
                ExecutionRequest::new(
                    &entry,
                    LibdenoOptions {
                        capture_stdout: true,
                        ..allow_all()
                    },
                ),
                SubmissionOptions::default(),
            )
            .unwrap();
        wait_for(&started);
        assert_eq!(handle.state(), ExecutionState::Started);

        let result = runtime().block_on(handle.result()).unwrap();
        assert_eq!(result.exit_code(), 0);
        assert!(String::from_utf8_lossy(result.output().stdout()).contains("supervised-submit"));
        assert!(result.report().cleanup_strength().is_some());
        assert!(result.report().transport_status().is_some());

        let failure_entry = dir.join("failure.js");
        fs::write(&failure_entry, "throw new Error('supervised-failure');").unwrap();
        let failure = runtime()
            .block_on(
                executor
                    .submit(
                        ExecutionRequest::new(&failure_entry, allow_all()),
                        SubmissionOptions::default(),
                    )
                    .unwrap()
                    .result(),
            )
            .unwrap_err();
        assert_eq!(
            failure.report().cleanup_strength(),
            Some(ExecutionCleanupStrength::DirectChild)
        );
        assert_eq!(
            failure.report().transport_status(),
            Some(ExecutionTransportStatus::Clean)
        );
        let _ = fs::remove_dir_all(dir);
    }
}
