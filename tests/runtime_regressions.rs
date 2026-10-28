use libdeno::{run, run_async, run_with, run_with_output, LibdenoOptions, LibdenoRuntime};
use std::fs;
use std::path::Path;

fn write_cjs(entry: &Path, marker: &Path, value: &str, pad: usize) -> usize {
    let mut source = format!(
        "require('node:fs').writeFileSync({:?}, {:?}); /*",
        marker.display().to_string(),
        value
    );
    source.push_str(&"x".repeat(pad));
    source.push_str("*/");
    fs::write(entry, &source).unwrap();
    source.len()
}

#[test]
fn zero_execution_deadline_does_not_start_the_script() {
    let dir = std::env::temp_dir().join(format!("libdeno-zero-deadline-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.js");
    let marker = dir.join("started.txt");
    fs::write(
        &entry,
        format!(
            "Deno.writeTextFileSync({:?}, 'started');",
            marker.display().to_string()
        ),
    )
    .unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        execution_deadline: Some(std::time::Duration::ZERO),
        ..Default::default()
    };
    // A zero budget means "do not execute": it must be rejected before resolver
    // setup or V8 bootstrap, so no script side effect can happen.
    let error = run(&entry, &options).unwrap_err();
    assert!(
        matches!(error, libdeno::LibdenoError::Timeout(_)),
        "{error}"
    );
    assert!(error.to_string().contains("script was not started"));
    assert!(
        !marker.exists(),
        "a zero deadline must not start the script"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn long_cjs_edits_execute_for_run_and_reusable_runtime() {
    let dir = std::env::temp_dir().join(format!("libdeno-cjs-regression-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.cjs");
    let marker = dir.join("marker.txt");
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };

    for (value, expected) in [("11", "11"), ("22", "22")] {
        write_cjs(&entry, &marker, value, 21_000);
        assert_eq!(run(&entry, &options).unwrap(), 0);
        assert_eq!(fs::read_to_string(&marker).unwrap(), expected);
    }

    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = tokio.block_on(LibdenoRuntime::new(&dir)).unwrap();
    for (value, expected) in [("11", "11"), ("22", "22")] {
        write_cjs(&entry, &marker, value, 21_000);
        assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
        assert_eq!(fs::read_to_string(&marker).unwrap(), expected);
    }

    let short = dir.join("short.cjs");
    for (value, expected) in [("33", "33"), ("44", "44")] {
        write_cjs(&short, &marker, value, 16);
        assert_eq!(run(&short, &options).unwrap(), 0);
        assert_eq!(fs::read_to_string(&marker).unwrap(), expected);
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn deno_exit_finishes_with_or_without_active_worker() {
    const CHILD: &str = "LIBDENO_EXIT_WORKER_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        run_exit_cases();
        return;
    }

    use std::process::{Command, Stdio};
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "deno_exit_finishes_with_or_without_active_worker",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            #[cfg(not(unix))]
            child.kill().unwrap();
            let _ = child.wait();
            panic!("Deno.exit worker child exceeded 30 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let output = child.wait_with_output().unwrap();
    assert!(
        status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn run_exit_cases() {
    let dir = std::env::temp_dir().join(format!("libdeno-exit-worker-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let main = dir.join("timer.js");
    fs::write(
        &main,
        "setInterval(() => {}, 1000); setTimeout(() => Deno.exit(7), 10);",
    )
    .unwrap();
    assert_eq!(run(&main, &options).unwrap(), 7);

    let worker = dir.join("worker.js");
    let late = dir.join("late.txt");
    let close_called = dir.join("close-called.txt");
    fs::write(
        &worker,
        format!("Object.defineProperty(globalThis,'close',{{configurable:true,get(){{Deno.writeTextFileSync({:?},'getter');return ()=>{{}};}}}}); postMessage('ready'); const end = Date.now() + 1200; while (Date.now() < end) {{}} Deno.writeTextFileSync({:?}, 'late');", close_called.display().to_string(), late.display().to_string()),
    )
    .unwrap();
    fs::write(
        &main,
        "const worker = new Worker(new URL('./worker.js', import.meta.url).href, { type: 'module' }); worker.onerror = (event) => { console.error(event.message); Deno.exit(2); }; worker.onmessage = () => Deno.exit(7);",
    )
    .unwrap();
    assert_eq!(run(&main, &options).unwrap(), 7);
    assert!(!late.exists(), "worker wrote after its run returned");
    assert!(!close_called.exists(), "worker close getter was invoked");
    std::thread::sleep(std::time::Duration::from_millis(1400));
    assert!(!late.exists(), "terminated worker resumed after run return");

    let nested_late = dir.join("nested-late.txt");
    let nested_started = dir.join("nested-started.txt");
    fs::write(dir.join("nested.js"), format!(
        "Deno.writeTextFileSync({:?}, 'started'); const end=Date.now()+1200; while(Date.now()<end){{}} Deno.writeTextFileSync({:?}, 'nested-late');",
        nested_started.display().to_string(),
        nested_late.display().to_string()
    )).unwrap();
    // Post 'ready' only after the nested worker is actually running, so the
    // exit below races a live worker rather than its startup.
    fs::write(&worker, format!(
        "new Worker(new URL('./nested.js', import.meta.url).href, {{type:'module'}}); const limit=Date.now()+5000; while(Date.now()<limit){{ try {{ Deno.statSync({:?}); break; }} catch {{}} }} postMessage('ready'); const end=Date.now()+1200; while(Date.now()<end){{}} Deno.writeTextFileSync({:?}, 'late');",
        nested_started.display().to_string(),
        late.display().to_string()
    )).unwrap();
    fs::write(&main, "const worker = new Worker(new URL('./worker.js', import.meta.url).href, {type:'module'}); worker.onmessage=()=>Deno.exit(7); worker.onerror=()=>Deno.exit(2);").unwrap();
    assert_eq!(run(&main, &options).unwrap(), 7);
    assert!(
        !late.exists() && !nested_late.exists(),
        "nested worker tree survived normal exit"
    );

    let started = dir.join("exception-worker-started");
    fs::write(&worker, format!(
        "globalThis.close=()=>{{throw Error('user close invoked')}}; Deno.writeTextFileSync({:?}, 'started'); const end=Date.now()+1200; while(Date.now()<end){{}} Deno.writeTextFileSync({:?}, 'late');",
        started.display().to_string(), late.display().to_string()
    )).unwrap();
    fs::write(&main, format!(
        "new Worker(new URL('./worker.js', import.meta.url).href, {{type:'module'}}); const end=Date.now()+2000; while(!(()=>{{try{{Deno.statSync({started:?});return true;}}catch{{return false;}}}})() && Date.now()<end){{}} throw new Error('main failure');"
    )).unwrap();
    let failure = run(&main, &options).unwrap_err();
    assert!(
        failure.to_string().contains("main failure"),
        "unexpected failure: {failure}"
    );
    assert!(started.exists());
    assert!(!late.exists(), "exception returned before worker cleanup");
    assert!(
        !close_called.exists(),
        "worker close function was invoked after exception"
    );

    let deadline_started = dir.join("deadline-worker-started");
    let deadline_ready = dir.join("deadline-main-ready");
    let deadline_late = dir.join("deadline-late.txt");
    fs::write(&worker, format!(
        "globalThis.close=()=>{{}}; Deno.writeTextFileSync({:?}, 'started'); postMessage('ready'); const end=Date.now()+1500; while(Date.now()<end){{}} Deno.writeTextFileSync({:?}, 'late');",
        deadline_started.display().to_string(), deadline_late.display().to_string()
    )).unwrap();
    fs::write(&main, format!(
        "const worker = new Worker(new URL('./worker.js', import.meta.url).href, {{type:'module'}}); await new Promise(resolve => worker.onmessage=resolve); Deno.writeTextFileSync({:?}, 'ready'); while(true){{}}",
        deadline_ready.display().to_string()
    )).unwrap();
    let deadline = LibdenoOptions {
        execution_deadline: Some(std::time::Duration::from_millis(100)),
        ..options.clone()
    };
    assert!(matches!(
        run(&main, &deadline),
        Err(libdeno::LibdenoError::Timeout(_))
    ));
    assert!(
        deadline_started.exists() && deadline_ready.exists(),
        "deadline did not interrupt while worker was active"
    );
    assert!(
        !deadline_late.exists(),
        "deadline returned before child worker cleanup"
    );
    assert!(
        !close_called.exists(),
        "worker close function was invoked after deadline"
    );

    let ready = dir.join("async-ready.txt");
    fs::write(&worker, format!(
        "globalThis.close=()=>{{Deno.writeTextFileSync({:?},'busy'); const closeEnd=Date.now()+2500; while(Date.now()<closeEnd){{}}}}; postMessage('ready'); const end=Date.now()+1200; while(Date.now()<end){{}} Deno.writeTextFileSync({:?}, 'late');",
        close_called.display().to_string(), late.display().to_string()
    )).unwrap();
    fs::write(&main, format!(
        "new Worker(new URL('./worker.js', import.meta.url).href, {{ type: 'module' }}); Deno.writeTextFileSync({:?}, 'ready'); await new Promise(() => {{}});",
        ready.display().to_string()
    )).unwrap();
    let capture_entry = dir.join("capture.js");
    fs::write(&capture_entry, "console.log('capture-ok');").unwrap();
    let tokio = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let capture_error = tokio.block_on(async {
        let mut run = Box::pin(run_async(&main, &options));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if ready.exists() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "async worker never bootstrapped"
            );
            if tokio::time::timeout(std::time::Duration::from_millis(5), &mut run)
                .await
                .is_ok()
            {
                panic!("pending async run completed before cancellation");
            }
        }
        drop(run);
        run_with_output(
            &capture_entry,
            &LibdenoOptions {
                permissions: vec![format!("--allow-read={}", dir.display())],
                capture_stdout: true,
                ..Default::default()
            },
        )
        .unwrap_err()
    });
    assert!(matches!(
        capture_error,
        libdeno::LibdenoError::Configuration(_)
    ));
    std::thread::sleep(std::time::Duration::from_millis(1400));
    assert!(
        !late.exists(),
        "dropped async run left worker JavaScript active"
    );
    assert!(
        !close_called.exists(),
        "worker close function was invoked after future drop"
    );
    let final_options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    #[cfg(not(windows))]
    assert_eq!(
        run_with_output(
            &capture_entry,
            &LibdenoOptions {
                capture_stdout: true,
                ..final_options.clone()
            }
        )
        .unwrap()
        .exit_code,
        0
    );
    #[cfg(windows)]
    assert_eq!(run(&capture_entry, &final_options).unwrap(), 0);
    let _ = fs::remove_dir_all(dir);
}

#[cfg(feature = "execution-control")]
#[test]
fn executor_permit_waits_for_cancelled_worker_tree() {
    const CHILD: &str = "LIBDENO_CANCEL_WORKER_TREE_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        run_executor_cancel_case();
        return;
    }
    use std::process::{Command, Stdio};
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "executor_permit_waits_for_cancelled_worker_tree",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            assert!(
                status.success(),
                "executor child failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        if std::time::Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            #[cfg(not(unix))]
            child.kill().unwrap();
            let _ = child.wait();
            panic!("executor worker cleanup exceeded 20 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(feature = "execution-control")]
fn run_executor_cancel_case() {
    use libdeno::{AdmissionConfig, ExecutionError, ExecutionRequest, Executor, SubmissionOptions};
    use std::num::NonZeroUsize;

    let dir = std::env::temp_dir().join(format!(
        "libdeno-executor-worker-tree-{}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let ready = dir.join("worker-ready");
    let late = dir.join("worker-late");
    let close_called = dir.join("worker-close-called");
    fs::write(dir.join("worker.js"), format!(
        "globalThis.close=()=>{{Deno.writeTextFileSync({:?},'called')}}; Deno.writeTextFileSync({:?}, 'ready'); const end=Date.now()+1500; while(Date.now()<end){{}} Deno.writeTextFileSync({:?}, 'late');",
        close_called.display().to_string(), ready.display().to_string(), late.display().to_string()
    )).unwrap();
    let entry = dir.join("main.js");
    fs::write(&entry, "new Worker(new URL('./worker.js', import.meta.url).href, {type:'module'}); await new Promise(() => {});").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let executor = rt
        .block_on(
            Executor::builder(&dir)
                .admission(AdmissionConfig::new(NonZeroUsize::new(1).unwrap(), 0))
                .build(),
        )
        .unwrap();
    let request = ExecutionRequest::new(
        &entry,
        LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    );
    let active = executor
        .submit(request, SubmissionOptions::default())
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !ready.exists() {
        assert!(std::time::Instant::now() < deadline, "worker did not start");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(executor
        .submit(
            ExecutionRequest::new(
                &entry,
                LibdenoOptions {
                    allow_all_permissions: true,
                    ..Default::default()
                }
            ),
            SubmissionOptions::default()
        )
        .is_err());
    assert_eq!(active.cancel(), libdeno::CancelOutcome::Requested);
    assert!(matches!(
        rt.block_on(active.result()).unwrap_err().error(),
        ExecutionError::Cancelled
    ));
    assert!(!late.exists(), "cancel returned before worker cleanup");
    assert!(
        !close_called.exists(),
        "worker close function was invoked after cancellation"
    );
    fs::write(dir.join("next.js"), "Deno.exit(0);").unwrap();
    let next = executor
        .submit(
            ExecutionRequest::new(
                dir.join("next.js"),
                LibdenoOptions {
                    allow_all_permissions: true,
                    ..Default::default()
                },
            ),
            SubmissionOptions::default(),
        )
        .unwrap();
    assert_eq!(rt.block_on(next.result()).unwrap().exit_code(), 0);
    let _ = fs::remove_dir_all(dir);
}
