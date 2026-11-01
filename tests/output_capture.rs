//! Output capture and tokio-context embedding tests.
//!
//! Regression guards for downstream reports:
//! - run() errored when called from inside a tokio runtime (async hosts had
//!   to build a std::thread::spawn + join escape themselves); it now routes
//!   onto a fresh thread automatically.
//! - run() had no way to capture the script's stdout/stderr; embedders were
//!   forced into file-exchange protocols. fd-level redirection + RunOutput
//!   close that gap.
//!
//! Windows skip: capture redirects the CRT fd, but Rust std's stdout/stderr
//! on Windows write via GetStdHandle and bypass it — the feature is rejected
//! with a Configuration error there (see run_sync_output), so the capture
//! tests are unix-only.
#![cfg(not(windows))]

use std::fs;
use std::path::PathBuf;

use libdeno::{run, run_source_with_output, run_with_output, LibdenoOptions, SourceLang};

/// Capture is process-global (fd redirection) and exclusive: a captured run
/// rejects any concurrent run. `cargo test` runs tests in parallel, so every
/// capture test must take this test-level mutex — the library's own
/// concurrency rules, enforced at the test-suite level.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-cap-{}-{}", std::process::id(), name));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn capture_stdout_gets_console_log() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The redirection is fd-level and process-global, so parallel test-harness
    // reports ("test ... ok") can land in the buffer too; assert the script's
    // line is present rather than exact equality.
    let dir = temp_dir("out");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('hello capture');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    };
    let out = run_with_output(&entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("hello capture\n"),
        "captured stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn capture_stderr_gets_console_error() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Same contains-style assertion as the stdout test: the capture is
    // process-global, so in-process harness/panic output can land in it.
    let dir = temp_dir("err");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.error('oops');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stderr: true,
        ..Default::default()
    };
    let out = run_with_output(&entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("oops\n"),
        "captured stderr: {:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn without_capture_output_is_empty() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("nocap");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('not captured');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let out = run_with_output(&entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.is_empty(),
        "stdout must not be captured by default"
    );
    assert!(out.stderr.is_empty());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn run_works_from_inside_tokio_context() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Regression: run() used to error with "cannot be called from inside a
    // tokio runtime", forcing async hosts to spawn a thread themselves. It
    // now does that internally; the run must still complete and the script's
    // output must still be capturable.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let code = rt.block_on(async {
        let dir = temp_dir("tokio");
        let entry = dir.join("main.js");
        fs::write(&entry, "console.log('tokio ok'); Deno.exit(0);").unwrap();
        let options = LibdenoOptions {
            allow_all_permissions: true,
            capture_stdout: true,
            ..Default::default()
        };
        let out = run_with_output(&entry, &options).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("tokio ok\n"),
            "capture must work in tokio path, got: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        out.exit_code
    });
    assert_eq!(code, 0);
}

#[test]
fn run_inside_tokio_keeps_error_reporting() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Errors must still surface (not panic) when routed through the tokio path.
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        let dir = temp_dir("tokio-err");
        let entry = dir.join("main.js");
        fs::write(&entry, "throw new Error('boom from tokio');").unwrap();
        let err = run(
            &entry,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("boom from tokio"),
            "unexpected error: {err}"
        );
        let _ = fs::remove_dir_all(&dir);
    });
}

#[test]
fn capture_truncates_at_max_capture_bytes() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // A verbose script must not grow host memory without limit: the byte cap
    // stops capture and flags the truncation instead.
    let dir = temp_dir("cap");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "for (let i = 0; i < 1000; i++) { console.log('x'.repeat(100)); }",
    )
    .unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        max_capture_bytes: Some(256),
        ..Default::default()
    };
    let out = run_with_output(&entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        out.stdout.len() <= 256,
        "captured {} bytes, expected <= 256",
        out.stdout.len()
    );
    assert!(out.capture_truncated, "truncation must be flagged");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn capture_within_budget_is_not_truncated() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("fit");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('small');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        max_capture_bytes: Some(1 << 20),
        ..Default::default()
    };
    let out = run_with_output(&entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("small\n"),
        "captured stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(!out.capture_truncated, "small output must not truncate");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn capture_in_memory_source_gets_console_log() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The in-memory entry path must honor capture the same way file entries
    // do: run_source_with_output pipes stdout through the same fd redirection.
    let dir = temp_dir("src-out");
    let out = run_source_with_output(
        "console.log('captured from memory');",
        SourceLang::JavaScript,
        &dir,
        &LibdenoOptions {
            allow_all_permissions: true,
            capture_stdout: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("captured from memory\n"),
        "captured stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn repeated_capture_does_not_exhaust_reader_budget() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // More sequential captures than the detached-reader budget: normal EOF
    // must release each reader before the next capture is admitted.
    let dir = temp_dir("reader-pressure");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('pressure');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    };
    for _ in 0..70 {
        let out = run_with_output(&entry, &options).unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("pressure\n"));
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn heap_cap_terminates_the_script_instead_of_aborting_the_host() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // A heap ceiling that V8 enforces with FatalProcessOutOfMemory would kill
    // the whole embedder. The near-heap-limit guard turns it into a
    // termination error the caller can handle, so this test can assert both
    // the error AND that the process survived to run a second script.
    let dir = temp_dir("heap-cap");
    let hog = dir.join("hog.js");
    fs::write(
        &hog,
        "const held = [];\n\
         for (let i = 0; i < 4_000_000; i++) held.push({ a: i, b: i + 1, c: 'xxxxxxxx' });\n",
    )
    .unwrap();
    let capped = LibdenoOptions {
        allow_all_permissions: true,
        max_heap_bytes: Some(64 << 20),
        capture_stdout: true,
        ..Default::default()
    };
    let error = run_with_output(&hog, &capped).unwrap_err();
    assert!(
        error.to_string().contains("execution terminated"),
        "expected a termination error, got: {error}"
    );

    // The host is still usable: a normal capped run completes.
    let ok = dir.join("ok.js");
    fs::write(&ok, "console.log('alive');").unwrap();
    let out = run_with_output(&ok, &capped).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "alive\n");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn detached_child_holding_the_capture_pipe_is_reported_as_incomplete() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // A script that detaches a child inheriting stdout leaves the capture
    // pipe open past the run. The script itself exits 0, but the capture
    // cannot reach EOF: that is the documented timed-out I/O error, and the
    // message must name the cause (a detached child), because "collection
    // deadline expired" alone gives a caller nothing to act on.
    let dir = temp_dir("detached-child");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "console.log('before-detach');\n\
         const cmd = new Deno.Command('/bin/sh', {\n\
           args: ['-c', 'sleep 30'],\n\
           stdout: 'inherit',\n\
           stderr: 'inherit',\n\
         });\n\
         cmd.spawn().unref();\n",
    )
    .unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    };
    let error = run_with_output(&entry, &options).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("output capture incomplete"), "{message}");
    assert!(message.contains("detached child"), "{message}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn run_with_reuses_stack_and_captures() {
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // The reusable-runtime path must honor capture too: long-lived hosts get
    // both stack reuse and per-run captured output via
    // libdeno::runtime::run_with_output.
    let dir = temp_dir("rt-out");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('runtime capture');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = rt
        .block_on(libdeno::runtime::LibdenoRuntime::new(&dir))
        .unwrap();
    let out = libdeno::runtime::run_with_output(&runtime, &entry, &options).unwrap();
    assert_eq!(out.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("runtime capture\n"),
        "captured stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    // A second run must reuse the stack and capture again.
    let out2 = libdeno::runtime::run_with_output(&runtime, &entry, &options).unwrap();
    assert!(
        String::from_utf8_lossy(&out2.stdout).contains("runtime capture\n"),
        "second run captured stdout: {:?}",
        String::from_utf8_lossy(&out2.stdout)
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn prompt_and_captured_stderr_are_rejected_in_process() {
    // The documented reason (`docs/api.md`, "prompt") is that the upstream
    // prompter requires stderr to be a terminal, and capture replaces fd 2
    // with a pipe — so the pair would silently deny every ungranted check.
    // Only the subprocess helpers had a test for this; the in-process entry
    // points share the same check, and a regression there would turn prompts
    // into silent denials for every embedding host.
    //
    // Five in-process entry points reach this check — `run_with_output`,
    // `run_source_with_output`, `runtime::run_with_output`, and both
    // `run_with_output_async` variants — all confirmed to reject identically.
    // Testing one is enough to pin the shared message, and
    // `tests/subprocess_config_validation.rs` covers the spawn side.
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("prompt-capture");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('x');").unwrap();
    let error = run_with_output(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            prompt: true,
            capture_stderr: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    let _ = fs::remove_dir_all(&dir);
    assert!(
        error
            .to_string()
            .contains("prompt and capture_stderr cannot be combined"),
        "expected the documented rejection, got: {error}"
    );
}

#[test]
fn prompt_with_only_captured_stdout_runs() {
    // The rejection is narrow: capturing stdout leaves fd 2 alone, so prompt
    // mode stays usable. Without this half, a check that rejected prompting
    // whenever *any* capture is on would still pass the test above.
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("prompt-stdout");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('prompt-safe');").unwrap();
    let out = run_with_output(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            prompt: true,
            capture_stdout: true,
            ..Default::default()
        },
    )
    .expect("capturing only stdout must stay compatible with prompt mode");
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.exit_code, 0);
    assert!(String::from_utf8_lossy(&out.stdout).contains("prompt-safe"));
}

#[test]
fn reusable_runtime_captured_source_returns_output() {
    // `libdeno::runtime::run_with_output_source` is the captured twin of
    // `run_with_source`, and the Python binding calls it directly — a
    // regression here surfaces as a Python failure with no Rust test naming
    // the cause. It lives in this binary, not `tests/runtime.rs`, because
    // capture takes the process-global lease and every test here already
    // serializes on CAPTURE_LOCK.
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("runtime-captured-source");
    fs::write(dir.join("sibling.js"), "export const v = 5;").unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = rt.block_on(libdeno::LibdenoRuntime::new(&dir)).unwrap();
    let out = libdeno::runtime::run_with_output_source(
        &runtime,
        "import { v } from './sibling.js';\nconsole.log('captured ' + v);",
        SourceLang::JavaScript,
        &dir,
        &LibdenoOptions {
            allow_all_permissions: true,
            capture_stdout: true,
            ..Default::default()
        },
    )
    .unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(out.exit_code, 0);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("captured 5"),
        "captured stdout: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn every_in_process_entry_point_rejects_the_prompt_capture_pair() {
    // `docs/api.md` states the rule once, but five in-process entry points must
    // enforce it: `run_with_output`, `run_source_with_output`,
    // `runtime::run_with_output`, and both async captured spellings. They all
    // funnel through `limits::validate_prompt_capture`, so the check itself is
    // shared — this test exists so that a *new* entry point which forgets to
    // call it fails here instead of silently turning prompts into denials.
    //
    // The existing guards are layered (a sync entry passes two of them), so
    // removing any single call keeps this test green; removing all of them
    // flips it. That is the intent: the invariant is "no entry point runs
    // without the check", not "exactly one call site".
    let _g = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("prompt-capture-all");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('x');").unwrap();
    let flags = || LibdenoOptions {
        allow_all_permissions: true,
        prompt: true,
        capture_stderr: true,
        ..Default::default()
    };
    let assert_rejected = |label: &str, error: libdeno::LibdenoError| {
        assert!(
            error
                .to_string()
                .contains("prompt and capture_stderr cannot be combined"),
            "{label} must reject the pair, got: {error}"
        );
    };

    assert_rejected(
        "run_with_output",
        run_with_output(&entry, &flags()).unwrap_err(),
    );
    assert_rejected(
        "run_source_with_output",
        run_source_with_output("console.log(1);", SourceLang::JavaScript, &dir, &flags())
            .map(|_| ())
            .unwrap_err(),
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = rt.block_on(libdeno::LibdenoRuntime::new(&dir)).unwrap();
    assert_rejected(
        "runtime::run_with_output",
        libdeno::runtime::run_with_output(&runtime, &entry, &flags()).unwrap_err(),
    );
    assert_rejected(
        "LibdenoRuntime::run_with_output_async",
        rt.block_on(runtime.run_with_output_async(&entry, &flags()))
            .map(|_| ())
            .unwrap_err(),
    );
    assert_rejected(
        "run_with_output_async",
        rt.block_on(libdeno::run_with_output_async(&entry, &flags()))
            .map(|_| ())
            .unwrap_err(),
    );
    let _ = fs::remove_dir_all(&dir);
}
