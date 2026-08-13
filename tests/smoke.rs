//! End-to-end smoke tests: run real JS through the embedded runtime.

use std::fs;
use std::path::PathBuf;

use libdeno::{run, LibdenoOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-e2e-{}-{}", std::process::id(), name));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Sets the process-global `LIBDENO_HOST_EXE` under a shared lock and returns
/// the guard, held for the rest of the test. cargo runs tests on parallel
/// threads in one process and `run_in_subprocess` reads the var at spawn time,
/// so concurrent set_var calls would race (hook_host vs child_host).
fn set_host_exe(exe: &str) -> std::sync::MutexGuard<'static, ()> {
    static HOST_EXE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let guard = HOST_EXE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("LIBDENO_HOST_EXE", exe);
    guard
}

#[test]
fn runs_plain_js_and_returns_exit_code() {
    let dir = temp_dir("plain");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('hello from js');").unwrap();
    let code = run(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn in_process_deno_exit_returns_code() {
    // Deno.exit(n) is intercepted (a WatcherExitHandle lives in the OpState):
    // op_exit terminates the isolate instead of calling std::process::exit, so
    // the host process survives and run() returns the requested code.
    let dir = temp_dir("exitcode");
    let entry = dir.join("main.js");
    fs::write(&entry, "Deno.exit(7);").unwrap();
    let code = run(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 7);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn script_error_returns_runtime_error() {
    let dir = temp_dir("throw");
    let entry = dir.join("main.js");
    fs::write(&entry, "throw new Error('boom');").unwrap();
    let err = run(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("boom"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_entry_is_a_runtime_error() {
    // resolve_entry succeeds for any file URL; the "Module not found" failure
    // surfaces at module load time as a Core error.
    let dir = temp_dir("missing");
    let err = run(
        dir.join("nope.js"),
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(matches!(err, libdeno::LibdenoError::Core(_)));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn directory_entry_uses_package_main() {
    let dir = temp_dir("dir-entry");
    fs::write(
        dir.join("package.json"),
        r#"{"name":"app","main":"lib/start.js"}"#,
    )
    .unwrap();
    fs::create_dir_all(dir.join("lib")).unwrap();
    fs::write(
        dir.join("lib/start.js"),
        "console.log('from package main');",
    )
    .unwrap();
    let code = run(
        &dir,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn permissions_restrict_fs_reads() {
    let dir = temp_dir("perm");
    let entry = dir.join("main.js");
    // Reading outside the allowed path must fail at runtime.
    fs::write(&entry, "Deno.readTextFile('/etc/hostname');").unwrap();
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    let err = run(&entry, &options).unwrap_err();
    // Runtime surfaces the denial as a NotCapable error.
    assert!(
        err.to_string().contains("NotCapable"),
        "unexpected error: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn default_permissions_are_rejected() {
    // v0.2.0 breaking change: an empty permission list is no longer allow-all.
    // `LibdenoOptions::default()` must fail with a Permission error instead of
    // silently running the script with every capability.
    let dir = temp_dir("default-perm");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('never runs');").unwrap();
    let err = run(&entry, &LibdenoOptions::default()).unwrap_err();
    assert!(
        matches!(err, libdeno::LibdenoError::Permission(_)),
        "expected a permission error, got: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn allow_all_flag_string_grants_everything() {
    // The `-A` CLI-style string is still equivalent to the
    // allow_all_permissions opt-in: a read of a host file succeeds with it.
    let dir = temp_dir("dash-a");
    fs::write(dir.join("secret.txt"), "secret").unwrap();
    let entry = dir.join("main.js");
    let secret = dir.join("secret.txt").display().to_string();
    fs::write(
        &entry,
        format!("Deno.readTextFileSync({secret:?}); console.log('read ok');"),
    )
    .unwrap();
    let options = LibdenoOptions {
        permissions: vec!["-A".into()],
        ..Default::default()
    };
    let code = run(&entry, &options).unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn require_traversal_outside_granted_dir_is_denied() {
    // Regression for the node_modules component bypass: a require path with a
    // `node_modules` component was previously exempted from the read check
    // lexically, so `node_modules/../..` traversal could read any file. The
    // resolved (canonicalized) path is what the permission check and the npm
    // exemption see now, so a node_modules-looking prefix with `..` cannot
    // smuggle a non-npm file past the gate.
    let dir = temp_dir("perm-traversal");
    let granted = dir.join("granted");
    fs::create_dir_all(&granted).unwrap();
    // The node_modules dir must exist so the traversal path canonicalizes.
    fs::create_dir_all(granted.join("node_modules")).unwrap();
    fs::write(dir.join("secret.txt"), "secret data").unwrap();
    // .cjs so the module is treated as CommonJS and gets a real `require`
    // (ambiguous .js files are ESM in this runtime).
    let entry = granted.join("main.cjs");
    fs::write(&entry, "require('./node_modules/../../secret.txt');").unwrap();
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", granted.display())],
        ..Default::default()
    };
    let err = run(&entry, &options).unwrap_err();
    assert!(
        err.to_string().contains("NotCapable"),
        "unexpected error: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cwd_option_sets_process_cwd_for_script() {
    // options.cwd must be what the script observes (Deno.cwd/process.cwd),
    // not the host process's cwd, and the host cwd must be restored after.
    let dir = temp_dir("cwd-opt");
    fs::create_dir_all(&dir).unwrap();
    let original_cwd = std::env::current_dir().unwrap();
    let entry = dir.join("main.js");
    // Canonicalize: Deno.cwd()/process.cwd() come from getcwd, which resolves
    // symlinks (e.g. /var -> /private/var on macOS), while temp_dir() paths
    // are not canonicalized.
    let expected = fs::canonicalize(&dir).unwrap().display().to_string();
    fs::write(
        &entry,
        format!("if (Deno.cwd() !== {expected:?}) throw new Error('cwd mismatch');"),
    )
    .unwrap();
    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        allow_all_permissions: true,
        ..Default::default()
    };
    let code = run(&entry, &options).unwrap();
    assert_eq!(code, 0);
    assert_eq!(std::env::current_dir().unwrap(), original_cwd);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn node_builtins_are_available() {
    let dir = temp_dir("node-builtins");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "import { join } from 'node:path'; console.log(join('a','b'));",
    )
    .unwrap();
    let code = run(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_mode_isolates_deno_exit() {
    // Deno.exit(7) inside a subprocess must terminate only the child; the host
    // process (this test) keeps running and observes exit code 7.
    let dir = temp_dir("subproc");
    let entry = dir.join("main.js");
    fs::write(&entry, "Deno.exit(7);").unwrap();

    // Point run_in_subprocess at the child-host binary: the test harness
    // itself is not a host and does not service child requests.
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let code = libdeno::run_in_subprocess(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 7);
    // We are still alive: the embedded runtime's Deno.exit did not kill us.
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_mode_propagates_stdout() {
    let dir = temp_dir("subproc-out");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('child says hi');").unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let code = libdeno::run_in_subprocess(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_mode_passes_args() {
    let dir = temp_dir("subproc-args");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('arg:', process.argv[2]);").unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let options = LibdenoOptions {
        args: vec!["--my-flag".into(), "hello".into()],
        allow_all_permissions: true,
        ..Default::default()
    };
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn permission_hook_decides_all_checks() {
    // The hook is the sole authority: with a restricted flag grant, a read
    // outside the grant would normally be denied — the allow-hook lets it
    // through, the deny-hook blocks it. Runs in a subprocess because the
    // broker is process-global and install-once (a test process can only
    // exercise it once, and installing it in the shared test binary would
    // poison every other test).
    let dir = temp_dir("hook");
    let secret = dir
        .parent()
        .unwrap()
        .join(format!("libdeno-hook-secret-{}.txt", std::process::id()));
    fs::write(&secret, "secret").unwrap();
    let entry = dir.join("main.js");
    let secret_abs = fs::canonicalize(&secret).unwrap().display().to_string();
    fs::write(
        &entry,
        format!("Deno.readTextFileSync({secret_abs:?}); console.log('hook allowed the read');"),
    )
    .unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_hook_host"));
    std::env::remove_var("LIBDENO_TEST_HOOK_DENY");
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 0, "the allow-hook must override the restricted grant");
    // Deny-hook: the same read is refused, child exits 1.
    std::env::set_var("LIBDENO_TEST_HOOK_DENY", "1");
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 1, "the deny-hook must refuse the read");
    std::env::remove_var("LIBDENO_TEST_HOOK_DENY");
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_file(&secret);
}

#[test]
fn subprocess_mode_propagates_restricted_permissions() {
    // The child must receive the parent's permission flags (and the allow-all
    // opt-in) through the ChildRunRequest: a restricted read grant in the
    // parent must deny the same read in the child. The target file exists, so
    // a broken passthrough (child allow-all) would make the read succeed and
    // flip the script to exit 9.
    let dir = temp_dir("subproc-perm");
    let secret = dir.parent().unwrap().join(format!(
        "libdeno-subproc-perm-secret-{}.txt",
        std::process::id()
    ));
    fs::write(&secret, "secret").unwrap();
    let entry = dir.join("main.js");
    let secret_abs = fs::canonicalize(&secret).unwrap().display().to_string();
    fs::write(
        &entry,
        format!(
            "let denied = false;\n\
             try {{ Deno.readTextFileSync({secret_abs:?}); }} catch {{ denied = true; }}\n\
             Deno.exit(denied ? 42 : 9);"
        ),
    )
    .unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(
        code, 42,
        "the child must inherit the restricted read grant (exit 9 = grant lost)"
    );
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_file(&secret);
}

#[cfg(unix)]
#[test]
fn non_utf8_entry_in_subprocess_is_an_error() {
    // Pre-v0.2.0 the entry was serialized with to_string_lossy, silently
    // mangling non-UTF-8 names into `�` paths. Since v0.2.0 it serializes as
    // a PathBuf: JSON cannot represent non-UTF-8, so the request construction
    // fails with a surfaced error before any process is spawned.
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let entry = std::path::PathBuf::from(OsStr::from_bytes(b"/tmp/libdeno-\xff-entry.js"));
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let err = libdeno::run_in_subprocess(&entry, &options).unwrap_err();
    assert!(
        matches!(err, libdeno::LibdenoError::Runtime(_)),
        "expected a surfaced serialization error, got: {err}"
    );
}

#[test]
fn subprocess_child_uses_options_cwd() {
    // The child's working directory must be options.cwd (pinned via
    // Command::current_dir at spawn), never the host process's cwd — and
    // run_in_subprocess must not depend on a process-global cwd lock (a
    // long-lived child must not block later calls in the same process).
    let dir = temp_dir("subproc-cwd");
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.js");
    // Canonicalize: Deno.cwd()/process.cwd() come from getcwd, which resolves
    // symlinks (e.g. /var -> /private/var on macOS).
    let expected = fs::canonicalize(&dir).unwrap().display().to_string();
    fs::write(
        &entry,
        format!("if (Deno.cwd() !== {expected:?}) throw new Error('cwd mismatch');"),
    )
    .unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        allow_all_permissions: true,
        ..Default::default()
    };
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn execution_deadline_terminates_infinite_loop() {
    // P1: a short execution_deadline must force-terminate a busy JS loop
    // (V8 terminate_execution from the deadline thread) and report Timeout
    // instead of hanging the host process.
    let dir = temp_dir("deadline-loop");
    let entry = dir.join("main.js");
    fs::write(&entry, "while (true) {}").unwrap();
    let options = LibdenoOptions {
        execution_deadline: Some(std::time::Duration::from_millis(200)),
        allow_all_permissions: true,
        ..Default::default()
    };
    let err = run(&entry, &options).unwrap_err();
    assert!(
        matches!(err, libdeno::LibdenoError::Timeout(_)),
        "expected Timeout, got: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn execution_deadline_allows_normal_scripts() {
    // Control: a script that finishes well inside the deadline completes
    // normally and returns its exit code.
    let dir = temp_dir("deadline-ok");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('fast');").unwrap();
    let options = LibdenoOptions {
        execution_deadline: Some(std::time::Duration::from_secs(30)),
        allow_all_permissions: true,
        ..Default::default()
    };
    let code = run(&entry, &options).unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn execution_deadline_kills_parked_event_loop() {
    // A script parked on a far-future timer has no JS frames to interrupt, so
    // the deadline's outer timeout (deadline + grace) must fire instead. This
    // covers the second, idle-event-loop half of run_with_deadline.
    let dir = temp_dir("deadline-parked");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "await new Promise((resolve) => setTimeout(resolve, 60_000));",
    )
    .unwrap();
    let options = LibdenoOptions {
        execution_deadline: Some(std::time::Duration::from_millis(100)),
        allow_all_permissions: true,
        ..Default::default()
    };
    let err = run(&entry, &options).unwrap_err();
    assert!(
        matches!(err, libdeno::LibdenoError::Timeout(_)),
        "expected Timeout, got: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}
