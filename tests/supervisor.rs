#![cfg(feature = "execution-control")]

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use libdeno::{run_in_supervised_subprocess, LibdenoError, LibdenoOptions};

static HOST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static HOST_PREWARM: std::sync::Once = std::sync::Once::new();

struct EnvVarGuard {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(name: &'static str, value: impl AsRef<OsStr>) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("libdeno-supervisor-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn allow_all() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    }
}

fn with_child_host<T>(run: impl FnOnce() -> T) -> T {
    let _lock = HOST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    HOST_PREWARM.call_once(|| {
        let _ = Command::new(env!("CARGO_BIN_EXE_child_host"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
    let _host_exe = EnvVarGuard::set("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    run()
}

#[test]
fn handshake_start_barrier_terminal_mapping_and_env_stripping() {
    let dir = temp_dir("handshake");
    let entry = dir.join("main.js");
    let marker = dir.join("env.json");
    fs::write(
        &entry,
        format!(
            "Deno.writeTextFileSync({marker:?}, JSON.stringify({{mode:Deno.env.get('LIBDENO_SUPERVISOR_MODE') ?? 'missing', endpoint:Deno.env.get('LIBDENO_SUPERVISOR_ENDPOINT') ?? 'missing', token:Deno.env.get('LIBDENO_SUPERVISOR_TOKEN') ?? 'missing', spawned:Deno.env.get('LIBDENO_SPAWNED_IPC')}}));\nconsole.log('supervised-out'); console.error('supervised-err'); Deno.exit(3);"
        ),
    )
    .unwrap();

    let output = with_child_host(|| {
        run_in_supervised_subprocess(
            &entry,
            &LibdenoOptions {
                capture_stdout: true,
                capture_stderr: true,
                max_capture_bytes: Some(1024),
                ..allow_all()
            },
        )
    })
    .unwrap();

    assert_eq!(output.exit_code, 3);
    assert!(String::from_utf8_lossy(&output.stdout).contains("supervised-out"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("supervised-err"));
    let env = fs::read_to_string(&marker).unwrap();
    assert!(env.contains(r#""mode":"missing""#));
    assert!(env.contains(r#""endpoint":"missing""#));
    assert!(env.contains(r#""token":"missing""#));
    assert!(env.contains(r#""spawned":"1""#));
    let _ = fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn supervised_exit_code_preserves_raw_value_after_unix_status_normalization() {
    let dir = temp_dir("exit-code-normalization");
    let entry = dir.join("main.js");
    fs::write(&entry, "Deno.exit(256);").unwrap();
    let output = with_child_host(|| run_in_supervised_subprocess(&entry, &allow_all())).unwrap();
    assert_eq!(output.exit_code, 256);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn capture_limit_above_supervisor_bound_is_rejected_before_spawn() {
    let dir = temp_dir("capture-limit-rejected");
    let entry = dir.join("missing.js");
    let error = run_in_supervised_subprocess(
        &entry,
        &LibdenoOptions {
            capture_stdout: true,
            max_capture_bytes: Some(96 * 1024 + 1),
            ..allow_all()
        },
    )
    .unwrap_err();
    assert!(matches!(error, LibdenoError::Configuration(_)));
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn supervisor_capture_default_is_bounded_and_selective() {
    let dir = temp_dir("capture-default");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('x'.repeat(70 * 1024));").unwrap();
    let output = with_child_host(|| {
        run_in_supervised_subprocess(
            &entry,
            &LibdenoOptions {
                capture_stdout: true,
                capture_stderr: false,
                ..allow_all()
            },
        )
    })
    .unwrap();
    assert_eq!(output.stdout.len(), 64 * 1024);
    assert!(output.capture_truncated);
    assert!(output.stderr.is_empty());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cancellation_before_start_does_not_run_user_code() {
    let dir = temp_dir("cancel-before-start");
    let entry = dir.join("main.js");
    let marker = dir.join("started");
    fs::write(
        &entry,
        format!("Deno.writeTextFileSync({marker:?}, 'started');"),
    )
    .unwrap();

    let (started, error) = with_child_host(|| {
        let started = Instant::now();
        let error = run_in_supervised_subprocess(
            &entry,
            &LibdenoOptions {
                execution_deadline: Some(Duration::ZERO),
                ..allow_all()
            },
        )
        .unwrap_err();
        (started, error)
    });
    assert!(matches!(error, LibdenoError::Timeout(_)), "{error}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!marker.exists());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn cancellation_after_start_is_best_effort_for_direct_child() {
    let dir = temp_dir("cancel-after-start");
    let entry = dir.join("main.js");
    let marker = dir.join("started");
    fs::write(
        &entry,
        format!("Deno.writeTextFileSync({marker:?}, 'started'); while (true) {{}}"),
    )
    .unwrap();

    let execution_deadline = Duration::from_secs(3);
    let started = Instant::now();
    let error = with_child_host(|| {
        run_in_supervised_subprocess(
            &entry,
            &LibdenoOptions {
                execution_deadline: Some(execution_deadline),
                ..allow_all()
            },
        )
    })
    .unwrap_err();
    assert!(matches!(error, LibdenoError::Timeout(_)), "{error}");
    assert!(marker.exists(), "the post-start test must cross START");
    // A cold child may spend several seconds in runtime bootstrap before the
    // cancellation grace can be observed; keep the assertion bounded without
    // treating that direct-child startup cost as a protocol failure.
    let max_elapsed = execution_deadline.saturating_mul(10);
    assert!(
        started.elapsed() < max_elapsed,
        "post-start cancellation exceeded {max_elapsed:?}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn malformed_supervisor_marker_fails_closed() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_child_host"))
        .env("LIBDENO_SUPERVISOR_MODE", "not-one")
        .env("LIBDENO_SUPERVISOR_ENDPOINT", "127.0.0.1:1")
        .env(
            "LIBDENO_SUPERVISOR_TOKEN",
            "00000000000000000000000000000000",
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
}

#[test]
fn missing_supervisor_auth_fails_closed() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_child_host"))
        .env("LIBDENO_SUPERVISOR_MODE", "1")
        .env_remove("LIBDENO_SUPERVISOR_ENDPOINT")
        .env_remove("LIBDENO_SUPERVISOR_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
}

#[test]
fn child_crash_without_terminal_is_reaped_and_reported() {
    let dir = temp_dir("crash");
    let entry = dir.join("main.js");
    fs::write(&entry, "Deno.exit(0);").unwrap();

    let _lock = HOST_ENV_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _host_exe = EnvVarGuard::set("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_dummy_host"));
    let error = run_in_supervised_subprocess(&entry, &allow_all()).unwrap_err();
    assert!(error.to_string().contains("supervisor"));
    let _ = fs::remove_dir_all(dir);
}
