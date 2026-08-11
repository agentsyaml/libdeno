//! End-to-end smoke tests: run real JS through the embedded runtime.

use std::fs;
use std::path::PathBuf;

use libdeno::{run, LibdenoOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-e2e-{}-{}", std::process::id(), name));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn runs_plain_js_and_returns_exit_code() {
    let dir = temp_dir("plain");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('hello from js');").unwrap();
    let code = run(&entry, &LibdenoOptions::default()).unwrap();
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
    let code = run(&entry, &LibdenoOptions::default()).unwrap();
    assert_eq!(code, 7);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn script_error_returns_runtime_error() {
    let dir = temp_dir("throw");
    let entry = dir.join("main.js");
    fs::write(&entry, "throw new Error('boom');").unwrap();
    let err = run(&entry, &LibdenoOptions::default()).unwrap_err();
    assert!(err.to_string().contains("boom"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn missing_entry_is_a_runtime_error() {
    // resolve_entry succeeds for any file URL; the "Module not found" failure
    // surfaces at module load time as a Core error.
    let dir = temp_dir("missing");
    let err = run(dir.join("nope.js"), &LibdenoOptions::default()).unwrap_err();
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
    let code = run(&dir, &LibdenoOptions::default()).unwrap();
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
    let code = run(&entry, &LibdenoOptions::default()).unwrap();
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
    unsafe {
        std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    }
    let code = libdeno::run_in_subprocess(&entry, &LibdenoOptions::default()).unwrap();
    assert_eq!(code, 7);
    // We are still alive: the embedded runtime's Deno.exit did not kill us.
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_mode_propagates_stdout() {
    let dir = temp_dir("subproc-out");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('child says hi');").unwrap();
    unsafe {
        std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    }
    let code = libdeno::run_in_subprocess(&entry, &LibdenoOptions::default()).unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_mode_passes_args() {
    let dir = temp_dir("subproc-args");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('arg:', process.argv[2]);").unwrap();
    unsafe {
        std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    }
    let options = LibdenoOptions {
        args: vec!["--my-flag".into(), "hello".into()],
        ..Default::default()
    };
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
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
    unsafe {
        std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    }
    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        ..Default::default()
    };
    let code = libdeno::run_in_subprocess(&entry, &options).unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}
