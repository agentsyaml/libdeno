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

/// Waits for a spawned child with a hard deadline, so a regression that
/// blocks (e.g. the child-mode stdin wait not being bounded) fails the test
/// instead of hanging it.
fn wait_with_deadline(child: &mut std::process::Child, secs: u64) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "child did not exit within {secs}s"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[test]
fn permission_hook_overrides_already_granted_reads() {
    // The hook is the sole authority — it is consulted before the granted
    // state, so it overrides even an explicitly granted capability. A deny
    // hook must block a read of a file inside `--allow-read`'s scope; an
    // allow hook must let an arbitrary read through with allow-all. Runs in a
    // subprocess because the broker is process-global and install-once.
    let dir = temp_dir("hook-grant");
    fs::create_dir_all(&dir).unwrap();
    let secret = dir.join("granted-secret.txt");
    fs::write(&secret, "secret").unwrap();
    let entry = dir.join("main.js");
    let secret_abs = fs::canonicalize(&secret).unwrap().display().to_string();
    fs::write(
        &entry,
        format!("Deno.readTextFileSync({secret_abs:?}); console.log('hook allowed the read');"),
    )
    .unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_hook_host"));

    // Allow-hook + allow-all (no flag grants): the read succeeds — the hook
    // is consulted even though allow-all alone would grant it.
    std::env::remove_var("LIBDENO_TEST_HOOK_DENY");
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 0, "the allow-hook must let the read through");

    // Deny-hook + a flag that grants the exact file: the hook overrides the
    // grant and the read must fail (child exits 1).
    std::env::set_var("LIBDENO_TEST_HOOK_DENY", "1");
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 1, "the deny-hook must override the granted read");
    std::env::remove_var("LIBDENO_TEST_HOOK_DENY");
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn permission_broker_decides_all_checks_end_to_end() {
    // End-to-end external broker: the broker_host child installs a broker at
    // a socket path; PermissionBroker::new is the CONNECTOR, so this test
    // process binds the listener and answers the JSON-line protocol. The
    // broker is consulted before the granted state, so it overrides even a
    // granted read (deny mode) and permits reads outside the grant (allow
    // mode).
    use std::io::BufRead;
    use std::io::Write;
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    let dir = temp_dir("broker");
    fs::create_dir_all(&dir).unwrap();
    // Inside the grant: a deny-broker must refuse it anyway (sole authority).
    let inside = dir.join("granted-secret.txt");
    fs::write(&inside, "inside").unwrap();
    // Outside the grant: an allow-broker must permit it.
    let outside = dir
        .parent()
        .unwrap()
        .join(format!("libdeno-broker-secret-{}.txt", std::process::id()));
    fs::write(&outside, "outside").unwrap();
    let entry = dir.join("main.js");
    let inside_abs = fs::canonicalize(&inside).unwrap().display().to_string();
    let outside_abs = fs::canonicalize(&outside).unwrap().display().to_string();
    fs::write(
        &entry,
        format!(
            "let ok = true;\n\
             try {{ Deno.readTextFileSync({inside_abs:?}); }} catch (e) {{ ok = false; }}\n\
             try {{ Deno.readTextFileSync({outside_abs:?}); }} catch (e) {{ ok = false; }}\n\
             Deno.exit(ok ? 0 : 1);"
        ),
    )
    .unwrap();

    // Bind the listener before the child connects (install_permission_broker
    // fails hard — exit 87 — if nothing is listening).
    let socket = dir.join("broker.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let deny = Arc::new(AtomicBool::new(false));
    let server = {
        let server_deny = deny.clone();
        std::thread::spawn(move || {
            // One connection per broker_host child; serve each until EOF.
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
                        Ok(0) | Err(_) => break, // child exited: connection EOF
                        Ok(_) => {}
                    }
                    // The upstream broker validates the echoed id, so reply on
                    // the same request id; parse it without serde_json (not a
                    // dependency of the test harness).
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

    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_broker_host"));
    std::env::set_var("LIBDENO_TEST_BROKER_PATH", &socket);
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    // Deny mode: the broker overrides even the granted read -> child exits 1.
    deny.store(true, Ordering::SeqCst);
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 1, "the deny-mode broker must refuse the reads");
    // Allow mode: the broker permits even the read outside the grant.
    deny.store(false, Ordering::SeqCst);
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 0, "the allow-mode broker must let the reads through");

    std::env::remove_var("LIBDENO_TEST_BROKER_PATH");
    drop(server);
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_file(&outside);
}

#[test]
fn child_mode_without_token_exits_immediately() {
    // LIBDENO_CHILD_MODE with no LIBDENO_CHILD_TOKEN must fail fast (exit 1)
    // instead of waiting the 10s stdin deadline or falling through to a host
    // run: an unauthenticated child request is refused before stdin is read.
    use std::io::Read;
    use std::process::Command;
    use std::process::Stdio;

    let mut child = Command::new(env!("CARGO_BIN_EXE_child_host"))
        .env("LIBDENO_CHILD_MODE", "1")
        .env_remove("LIBDENO_CHILD_TOKEN")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let status = wait_with_deadline(&mut child, 10);
    assert_eq!(
        status.code(),
        Some(1),
        "missing token must exit 1, not wait for stdin"
    );
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(
        err.contains("LIBDENO_CHILD_TOKEN"),
        "unexpected stderr: {err}"
    );
}

#[test]
fn child_mode_with_wrong_token_is_rejected() {
    // A child-mode request whose token does not match LIBDENO_CHILD_TOKEN must
    // be refused: the request is read (so a well-formed payload is needed) and
    // then rejected with exit 1 before any script runs.
    use std::io::Read;
    use std::io::Write;
    use std::process::Command;
    use std::process::Stdio;

    let dir = temp_dir("wrong-token");
    let entry = dir.join("main.js");
    fs::write(&entry, "Deno.exit(0);").unwrap();
    let entry_abs = entry.display().to_string();
    let cwd = fs::canonicalize(&dir).unwrap().display().to_string();
    // The request schema of ChildRunRequest, with a token that does NOT match
    // the env token below. Escape backslashes/quotes: on Windows the path is
    // `D:\a\...` and a raw backslash is an invalid JSON escape.
    let json_escape = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let payload = format!(
        r#"{{"entry":"{}","permissions":[],"allow_all_permissions":true,"prompt":false,"args":[],"cwd":"{}","token":"00000000000000000000000000000000"}}"#,
        json_escape(&entry_abs),
        json_escape(&cwd)
    );
    let mut child = Command::new(env!("CARGO_BIN_EXE_child_host"))
        .env("LIBDENO_CHILD_MODE", "1")
        .env("LIBDENO_CHILD_TOKEN", "0123456789abcdef0123456789abcdef")
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.as_bytes())
        .unwrap();
    let status = wait_with_deadline(&mut child, 10);
    assert_eq!(
        status.code(),
        Some(1),
        "a mismatched token must be rejected"
    );
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    assert!(
        err.contains("token does not match"),
        "unexpected stderr: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_child_stdin_reaches_eof() {
    // run_in_subprocess drops the child's stdin after writing the request, so
    // a script reading process stdin must see EOF (and finish) instead of
    // blocking forever on the still-open pipe.
    let dir = temp_dir("stdin-eof");
    let entry = dir.join("main.js");
    let result_file = dir.join("stdin-bytes.txt");
    let result_abs = result_file.display().to_string();
    fs::write(
        &entry,
        format!(
            "let total = 0;\n\
             for await (const chunk of Deno.stdin.readable) {{ total += chunk.length; }}\n\
             Deno.writeTextFileSync({result_abs:?}, String(total));"
        ),
    )
    .unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let code = libdeno::run_in_subprocess(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0, "the script must finish once stdin reaches EOF");
    let _bytes: u64 = fs::read_to_string(&result_file)
        .unwrap()
        .trim()
        .parse()
        .expect("the script must have written a valid byte count");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_missing_entry_exits_one() {
    // A request for a nonexistent entry must terminate the child with exit 1
    // (the run error surfaced through maybe_handle_child_mode), not hang or
    // fall through.
    let dir = temp_dir("subproc-missing");
    let entry = dir.join("nope.js");
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let code = libdeno::run_in_subprocess(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 1, "a missing entry must exit the child with 1");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_passes_environment_to_child() {
    // The child inherits the parent's environment (run_in_subprocess only
    // adds LIBDENO_CHILD_MODE / LIBDENO_CHILD_TOKEN / LIBDENO_SPAWNED_IPC), so
    // a variable set by the host must be visible to the script.
    let dir = temp_dir("subproc-env");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "Deno.exit(Deno.env.get('LIBDENO_TEST_ENV_PASSTHROUGH') === 'passthrough-42' ? 0 : 9);",
    )
    .unwrap();
    std::env::set_var("LIBDENO_TEST_ENV_PASSTHROUGH", "passthrough-42");
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let code = libdeno::run_in_subprocess(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    std::env::remove_var("LIBDENO_TEST_ENV_PASSTHROUGH");
    assert_eq!(
        code, 0,
        "the child must see the parent's environment (exit 9 = not passed)"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_host_exe_returns_io_error() {
    // A spawn failure (nonexistent LIBDENO_HOST_EXE) must surface as
    // LibdenoError::Io, not a panic or a misleading runtime error.
    let dir = temp_dir("bad-exe");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('never runs');").unwrap();
    let _host_exe = set_host_exe("/nonexistent/libdeno-host-binary");
    let err = libdeno::run_in_subprocess(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, libdeno::LibdenoError::Io(_)),
        "a missing host exe must map to Io, got: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn prompt_denies_outside_grants_without_a_terminal() {
    // prompt: true asks interactively only when stdin AND stderr are
    // terminals. Under cargo test both are pipes, so the prompter must deny
    // immediately (no blocking stdin read, no deadlock) and the run must fail
    // with a NotCapable error — the fail-closed headless behavior.
    // The entry sits inside the grant so the module load passes; the runtime
    // read of /etc/hostname (outside the grant) is the Prompt-state check.
    let dir = temp_dir("prompt");
    let entry = dir.join("main.js");
    fs::write(&entry, "Deno.readTextFileSync('/etc/hostname');").unwrap();
    let options = LibdenoOptions {
        prompt: true,
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    let err = run(&entry, &options).unwrap_err();
    assert!(
        err.to_string().contains("NotCapable"),
        "a non-terminal prompt must deny, got: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn static_file_imports_honor_allow_read_scope() {
    // H1: static file imports must go through check_open (the graph loader's
    // file_permission_api_name = "import"), so --allow-read scope is enforced
    // on module imports, not just runtime read ops. A relative import with
    // `..` is file scheme too and must be caught the same way.
    let dir = temp_dir("static-import");
    let granted = dir.join("granted");
    fs::create_dir_all(&granted).unwrap();
    fs::write(dir.join("outside.js"), "export const x = 1;").unwrap();
    fs::write(granted.join("inside.js"), "export const y = 2;").unwrap();

    // Import outside the grant (via a relative `../` — same file scheme) must
    // fail with NotCapable.
    let entry = granted.join("main.js");
    fs::write(
        &entry,
        "import { x } from '../outside.js';\nconsole.log(x);",
    )
    .unwrap();
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", granted.display())],
        ..Default::default()
    };
    let err = run(&entry, &options).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("Requires read access") || msg.contains("NotCapable"),
        "an import outside the --allow-read scope must be denied, got: {msg}"
    );

    // A static import inside the grant succeeds.
    let entry = granted.join("main-ok.js");
    fs::write(
        &entry,
        "import { y } from './inside.js';\nconsole.log('ok');",
    )
    .unwrap();
    let code = run(&entry, &options).unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn worker_module_imports_use_the_worker_own_permissions() {
    // M2: a worker that declares `permissions: []` must not inherit the main
    // run's --allow-read scope for its module loads. The worker's entry
    // (worker.js, inside the main run's grant) is NOT in the shared graph, so
    // its first load triggers a worker-side graph build — which must be gated
    // by the worker's empty container and denied.
    let dir = temp_dir("worker-perms");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("secret.js"), "export const z = 1;").unwrap();
    fs::write(
        dir.join("worker.js"),
        "import { z } from './secret.js';\npostMessage('worker ran');",
    )
    .unwrap();
    fs::write(
        dir.join("main.js"),
        r#"const result = await new Promise((resolve) => {
  const w = new Worker(new URL("worker.js", import.meta.url), { type: "module", deno: { permissions: [] } });
  w.onmessage = () => resolve("started");
  w.onerror = (e) => { e.preventDefault(); resolve("error"); };
  setTimeout(() => { w.terminate(); resolve("timeout"); }, 10_000);
});
Deno.exit(result === "error" ? 0 : (result === "started" ? 42 : 43));"#,
    )
    .unwrap();
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    let code = run(dir.join("main.js"), &options).unwrap();
    assert_eq!(
        code, 0,
        "the empty-permissions worker's module load must be denied \
         (exit 42 = the main run's --allow-read leaked to the worker, \
         43 = watchdog timeout)"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_write_times_out_when_host_never_services_child_mode() {
    // M1: a host that never reads stdin (does not call maybe_handle_child_mode)
    // must not hang run_in_subprocess once the request exceeds the pipe
    // buffer: the payload write is bounded at 10s, the child is killed, and a
    // Timeout error with a clear message is returned.
    let dir = temp_dir("no-service");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('never runs');").unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_dummy_host"));
    // > 64 KiB pipe buffer, so write_all blocks against the non-reading host.
    let big_arg = "x".repeat(256 * 1024);
    let options = LibdenoOptions {
        args: vec![big_arg],
        allow_all_permissions: true,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let err = libdeno::run_in_subprocess(&entry, &options).unwrap_err();
    let elapsed = started.elapsed();
    assert!(
        matches!(err, libdeno::LibdenoError::Timeout(_)),
        "expected Timeout, got: {err}"
    );
    assert!(
        err.to_string().contains("child mode"),
        "the message must explain the cause, got: {err}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "the bounded write must return in ~10s, took {elapsed:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}
