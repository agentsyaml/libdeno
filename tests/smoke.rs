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
fn invalid_heap_fails_at_entry_without_polluting_a_concurrent_valid_run() {
    use std::sync::Arc;
    use std::sync::Barrier;

    let dir = temp_dir("heap-entry-validation");
    let valid_entry = dir.join("valid.js");
    let invalid_entry = dir.join("invalid.js");
    fs::write(&valid_entry, "Deno.exit(0);").unwrap();
    fs::write(&invalid_entry, "Deno.exit(0);").unwrap();

    let start = Arc::new(Barrier::new(2));
    let valid_start = start.clone();
    let valid_handle = std::thread::spawn(move || {
        valid_start.wait();
        run(
            &valid_entry,
            &LibdenoOptions {
                allow_all_permissions: true,
                max_heap_bytes: Some(128 << 20),
                ..Default::default()
            },
        )
    });

    start.wait();
    let invalid = run(
        &invalid_entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            max_heap_bytes: Some(0),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        matches!(invalid, libdeno::LibdenoError::Configuration(ref message) if message.contains("max_heap_bytes")),
        "invalid heap must fail at the run entry: {invalid:?}"
    );
    assert_eq!(valid_handle.join().unwrap().unwrap(), 0);
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
    assert!(
        err.to_string().contains("Module not found") || err.to_string().contains("No such file"),
        "graph build failure lost its root cause: {err}"
    );
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
    // `LibdenoOptions::default()` must fail with a Configuration error instead
    // of silently running the script with every capability.
    let dir = temp_dir("default-perm");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('never runs');").unwrap();
    let err = run(&entry, &LibdenoOptions::default()).unwrap_err();
    assert!(
        matches!(err, libdeno::LibdenoError::Configuration(_)),
        "expected a configuration error, got: {err}"
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
fn permissions_reject_ungranted_capabilities() {
    // Grant only entry/read access so each operation below reaches its own
    // capability check without touching the network, shell, or host state.
    let dir = temp_dir("perm-capabilities");
    let permission = format!("--allow-read={}", dir.display());
    let write_target = dir.join("blocked-write.txt");
    let cases = vec![
        (
            "write",
            format!("Deno.writeTextFileSync({write_target:?}, 'blocked');"),
        ),
        (
            "env",
            "Deno.env.get('LIBDENO_PHASE2_UNGRANTED_ENV');".to_string(),
        ),
        (
            "net",
            "const listener = Deno.listen({hostname: '127.0.0.1', port: 0}); listener.close();"
                .to_string(),
        ),
        (
            "run",
            "new Deno.Command(Deno.execPath()).outputSync();".to_string(),
        ),
        ("sys", "Deno.osRelease();".to_string()),
        (
            "import",
            "await import('https://example.invalid/libdeno-phase2.js');".to_string(),
        ),
    ];

    for (name, source) in cases {
        let entry = dir.join(format!("{name}.js"));
        fs::write(&entry, source).unwrap();
        let err = run(
            &entry,
            &LibdenoOptions {
                permissions: vec![permission.clone()],
                ..Default::default()
            },
        )
        .unwrap_err();
        if name == "import" {
            assert!(
                err.to_string().contains("Requires import access"),
                "import unexpectedly returned a non-permission error: {err}"
            );
        } else {
            assert!(
                err.is_permission_error(),
                "{name} unexpectedly returned a non-permission error: {err}"
            );
        }
    }
    assert!(
        !write_target.exists(),
        "denied write must not create a file"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn permissions_allow_symlink_inside_granted_read_scope() {
    use std::os::unix::fs::symlink;

    let dir = temp_dir("perm-symlink-inside");
    let allowed = dir.join("allowed");
    let outside = dir.join("outside");
    fs::create_dir_all(&allowed).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let allowed = fs::canonicalize(&allowed).unwrap();
    let outside = fs::canonicalize(&outside).unwrap();
    fs::write(outside.join("secret.txt"), "outside").unwrap();
    let link = allowed.join("outside-link");
    symlink(&outside, &link).unwrap();
    let entry = allowed.join("main.js");
    let target = link.join("secret.txt");
    fs::write(&entry, format!("Deno.readTextFileSync({target:?});")).unwrap();

    let code = run(
        &entry,
        &LibdenoOptions {
            permissions: vec![format!("--allow-read={}", allowed.display())],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn permissions_reject_symlink_outside_granted_read_scope() {
    use std::os::unix::fs::symlink;

    let dir = temp_dir("perm-symlink-outside");
    let allowed = dir.join("allowed");
    let outside = dir.join("outside");
    fs::create_dir_all(&allowed).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let allowed = fs::canonicalize(&allowed).unwrap();
    let outside = fs::canonicalize(&outside).unwrap();
    let target = allowed.join("secret.txt");
    fs::write(&target, "inside").unwrap();
    let link = outside.join("inside-link");
    symlink(&target, &link).unwrap();
    // Keep the entry inside the granted directory so any failure comes from
    // checking the outside symlink path, not from loading the entry itself.
    let entry = allowed.join("main.js");
    fs::write(&entry, format!("Deno.readTextFileSync({link:?});")).unwrap();

    let err = run(
        &entry,
        &LibdenoOptions {
            permissions: vec![format!("--allow-read={}", allowed.display())],
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        err.is_permission_error(),
        "outside symlink unexpectedly returned: {err}"
    );
    assert!(
        err.to_string().contains(&link.display().to_string()),
        "entry read was denied instead of the outside symlink: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cwd_option_is_resolution_base_not_process_cwd() {
    // options.cwd is a resolution base only: the process cwd is never
    // switched, so the script observes the host's cwd (Deno.cwd/process.cwd)
    // and the host cwd is untouched throughout.
    let dir = temp_dir("cwd-opt");
    fs::create_dir_all(&dir).unwrap();
    let original_cwd = std::env::current_dir().unwrap();
    let entry = dir.join("main.js");
    // Canonicalize: Deno.cwd()/process.cwd() come from getcwd, which resolves
    // symlinks (e.g. /var -> /private/var on macOS), while temp_dir() paths
    // are not canonicalized.
    let host_cwd = fs::canonicalize(&original_cwd)
        .unwrap()
        .display()
        .to_string();
    // Windows canonicalize returns a \\?\ verbatim path; Deno.cwd() never
    // has the prefix — strip it so the comparison is apples-to-apples.
    #[cfg(windows)]
    let host_cwd = host_cwd
        .strip_prefix(r"\\?\")
        .unwrap_or(&host_cwd)
        .to_string();
    fs::write(
        &entry,
        format!("if (Deno.cwd() !== {host_cwd:?}) throw new Error('cwd mismatch');"),
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

#[test]
fn subprocess_mode_accepts_256kib_arg_request() {
    // The child-side 1 MiB bound must not reject a realistic large argument,
    // and the real child host must receive the complete payload.
    let dir = temp_dir("subproc-large-arg");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "if (process.argv[2].length !== 256 * 1024) Deno.exit(1);",
    )
    .unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let code = libdeno::run_in_subprocess(
        &entry,
        &LibdenoOptions {
            args: vec!["x".repeat(256 * 1024)],
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn oversized_subprocess_request_fails_before_spawn() {
    // An invalid executable makes an accidental spawn observable: the size
    // validation must run first and return the request error instead of an IO
    // error from Command::spawn.
    let dir = temp_dir("subproc-oversized-request");
    let entry = dir.join("main.js");
    fs::write(&entry, "Deno.exit(0);").unwrap();
    let _host_exe = set_host_exe("/definitely/not-a-libdeno-host");
    let error = libdeno::run_in_subprocess(
        &entry,
        &LibdenoOptions {
            args: vec!["x".repeat(1024 * 1024)],
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("child-mode request exceeds the 1048576-byte limit"),
        "unexpected oversized-request error: {error}"
    );
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_child_env_is_clean() {
    // Child-mode markers must be stripped before the script runs: a script
    // spawning its own subprocesses (git, compilers, helpers) must not pass
    // LIBDENO_CHILD_MODE/TOKEN down, or every grandchild would enter child
    // mode with a consumed stdin and die.
    let dir = temp_dir("subproc-env-clean");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "if (Deno.env.get('LIBDENO_CHILD_MODE') !== undefined ||\n\
             Deno.env.get('LIBDENO_CHILD_TOKEN') !== undefined) {\n\
           throw new Error('child-mode markers leaked into the script env');\n\
         }",
    )
    .unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn subprocess_grandchild_spawns_cleanly() {
    // End-to-end: a script in child mode spawns its own subprocess and the
    // grandchild runs normally (would enter child mode and die without the
    // env strip).
    let dir = temp_dir("subproc-grandchild");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "const out = new Deno.Command('/bin/echo', { args: ['grandchild-ok'], stdout: 'piped' }).outputSync();\n\
         if (new TextDecoder().decode(out.stdout).trim() !== 'grandchild-ok') {\n\
           throw new Error('grandchild did not run cleanly');\n\
         }",
    )
    .unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_output_captures_both_streams() {
    // The subprocess answer to output capture: the child's own fds are
    // piped, so both streams come back — on every platform, Windows
    // included (in-process capture is rejected there).
    let dir = temp_dir("subproc-out-cap");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "console.log('child-out');\nconsole.error('child-err');",
    )
    .unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let output = libdeno::run_in_subprocess_with_output(&entry, &options).unwrap();
    assert_eq!(output.exit_code, 0);
    assert!(
        output.stdout.windows(9).any(|w| w == b"child-out"),
        "stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.windows(9).any(|w| w == b"child-err"),
        "stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.capture_truncated);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_output_respects_capture_budget() {
    // max_capture_bytes caps each stream; excess is drained and dropped so
    // the child never blocks on a full pipe, and the run still succeeds.
    let dir = temp_dir("subproc-out-cap-budget");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "console.log('x'.repeat(2000));\nconsole.error('y'.repeat(2000));",
    )
    .unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let options = LibdenoOptions {
        allow_all_permissions: true,
        max_capture_bytes: Some(64),
        ..Default::default()
    };
    let output = libdeno::run_in_subprocess_with_output(&entry, &options).unwrap();
    assert_eq!(
        output.exit_code, 0,
        "the child must not be blocked by the budget"
    );
    assert!(
        output.capture_truncated,
        "2000 bytes over a 64-byte budget must truncate"
    );
    assert!(
        output.stdout.len() <= 64,
        "stdout budget exceeded: {}",
        output.stdout.len()
    );
    assert!(
        output.stderr.len() <= 64,
        "stderr budget exceeded: {}",
        output.stderr.len()
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_mode_forwards_features() {
    // Safety options must reach the child: a host that shrinks `features`
    // for an untrusted plugin gets the same shrink in child mode, not the
    // full default unstable surface.
    let dir = temp_dir("subproc-features");
    let entry = dir.join("main.js");
    // kv is not in the ["ffi"] set: openKv must be absent in the child.
    fs::write(
        &entry,
        "if (typeof Deno.openKv !== 'undefined') throw new Error('kv unexpectedly enabled');",
    )
    .unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let options = LibdenoOptions {
        features: Some(vec!["ffi".into()]),
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(libdeno::run_in_subprocess(&entry, &options).unwrap(), 0);

    // Default (None) means the full unstable surface: openKv exists.
    fs::write(
        &entry,
        "if (typeof Deno.openKv === 'undefined') throw new Error('kv missing by default');",
    )
    .unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(libdeno::run_in_subprocess(&entry, &options).unwrap(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_mode_forwards_execution_deadline() {
    // A runaway script in child mode must be cut down by the host's
    // execution_deadline — the isolation entry point must not run unbounded.
    let dir = temp_dir("subproc-deadline");
    let entry = dir.join("main.js");
    fs::write(&entry, "while (true) {}").unwrap();
    let _host_exe = set_host_exe(env!("CARGO_BIN_EXE_child_host"));
    let options = LibdenoOptions {
        execution_deadline: Some(std::time::Duration::from_secs(1)),
        allow_all_permissions: true,
        ..Default::default()
    };
    let start = std::time::Instant::now();
    // The child times out → its host exits 1 → the parent observes exit 1.
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    let elapsed = start.elapsed();
    assert_eq!(code, 1, "deadline child should exit 1, got {code}");
    assert!(
        elapsed.as_secs() < 10,
        "deadline did not cut the child down: {elapsed:?}"
    );
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
    // run_in_subprocess must not hold any process-global lock across the
    // child's lifetime (a long-lived child must not block later calls in
    // the same process).
    let dir = temp_dir("subproc-cwd");
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.js");
    // Semantic cwd check instead of string comparison: Windows path forms
    // differ between APIs (canonicalize returns \\?\ verbatim + resolves
    // 8.3 short names, Deno.cwd() echoes the process cwd form), so compare
    // behavior — a relative read that only resolves if cwd == dir.
    fs::write(dir.join("marker.txt"), "subproc-cwd-ok").unwrap();
    fs::write(
        &entry,
        "if (Deno.readTextFileSync('./marker.txt') !== 'subproc-cwd-ok') \
         throw new Error('cwd mismatch');",
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
        r#"{{"entry":"{}","permissions":[],"allow_all_permissions":true,"prompt":false,"args":[],"cwd":"{}","token":"00000000000000000000000000000000","features":null,"max_heap_bytes":null,"execution_deadline":null}}"#,
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
        // A narrowed custom feature set must still keep worker-options enabled:
        // otherwise creating this permissions-bearing Worker terminates the
        // host instead of delivering the worker's own read denial.
        features: Some(vec!["kv".into()]),
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
fn worker_does_not_reuse_main_graph_entries_across_permissions() {
    // Regression for the worker graph residual: the main worker loads secret.js
    // first, then a worker whose read grant contains only its entry directory
    // tries to import that same secret. With the old shared ModuleGraph this
    // import was returned from the main graph without the worker's permission
    // check.
    let dir = temp_dir("worker-perms-graph-residual");
    let worker_dir = dir.join("worker");
    fs::create_dir_all(&worker_dir).unwrap();
    fs::write(dir.join("secret.js"), "export const secret = 'loaded';").unwrap();
    let worker_entry = worker_dir.join("entry.js");
    fs::write(
        &worker_entry,
        r#"postMessage('entry-started');
try {
  await import('../secret.js');
  postMessage('secret-loaded');
} catch (error) {
  const message = String(error);
  const is_permission_error = error?.name === 'NotCapable'
    || message.includes('NotCapable')
    || message.includes('Requires read access');
  postMessage(is_permission_error ? 'permission-error' : 'other-error');
}"#,
    )
    .unwrap();
    let worker_dir_abs = fs::canonicalize(&worker_dir).unwrap().display().to_string();
    let worker_dir_json = deno_core::serde_json::to_string(&worker_dir_abs).unwrap();
    let dir = fs::canonicalize(&dir).unwrap();
    fs::write(
        dir.join("main.js"),
        format!(
            r#"import {{ secret }} from './secret.js';
const result = await new Promise((resolve) => {{
  let entry_started = false;
  let settled = false;
  const finish = (value) => {{
    if (!settled) {{
      settled = true;
      resolve(value);
    }}
  }};
  const worker_entry = new URL('worker/entry.js', import.meta.url);
  const w = new Worker(worker_entry, {{
    type: 'module',
    deno: {{ permissions: {{ read: [{worker_dir_json}] }} }},
  }});
  w.onmessage = (event) => {{
    if (event.data === 'entry-started') {{
      entry_started = true;
    }} else if (event.data === 'permission-error') {{
      finish(entry_started ? 'permission-error' : 'permission-before-entry');
    }} else if (event.data === 'secret-loaded') {{
      finish('secret-loaded');
    }} else {{
      finish('other-message');
    }}
  }};
  w.onerror = (event) => {{
    event.preventDefault();
    finish(entry_started ? 'worker-error' : 'entry-error');
  }};
  setTimeout(() => {{ w.terminate(); finish('timeout'); }}, 10_000);
}});
// `secret` is intentionally referenced so the main graph definitely loads it
// before the worker starts.
if (secret !== 'loaded') throw new Error('main secret did not load');
Deno.exit(result === 'permission-error' ? 0
  : result === 'secret-loaded' ? 42
  : result === 'entry-error' ? 43
  : result === 'worker-error' ? 44
  : result === 'permission-before-entry' ? 45
  : result === 'other-message' ? 46
  : 47);"#,
        ),
    )
    .unwrap();
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    let code = run(dir.join("main.js"), &options).unwrap();
    assert_eq!(
        code, 0,
        "a worker must re-check a main-loaded module with its own graph \
         (42=secret loaded, 43=entry failed, 44=worker error, \
          45=permission error before entry marker, 46=other error, 47=timeout)"
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
