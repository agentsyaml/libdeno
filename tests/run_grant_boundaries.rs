//! `--allow-run` grants a name, and the loader environment can veto a scoped
//! grant.
//!
//! Two behaviours a host has to know before it relies on a scoped `--allow-run`:
//!
//! 1. The name resolves through `PATH`. A grant on `mytool` runs whatever
//!    `mytool` means at spawn time, so a script-writable directory on the
//!    host's `PATH` hands the script the binary. A script cannot smuggle its
//!    own `PATH` past the check — that combination is refused.
//! 2. Any non-empty `LD_*` / `DYLD_*` in the host environment makes a scoped
//!    grant unusable: those variables can redirect the dynamic loader, so Deno
//!    requires a global `--allow-run` to pass them to a child.
//!
//! Both match Deno CLI 2.9.8. Each test clears `LD_*` / `DYLD_*` first, because
//! `cargo test` sets `DYLD_FALLBACK_LIBRARY_PATH` on macOS and that would mask
//! what the test measures.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

/// These tests mutate `PATH` and the loader variables, which are process-wide;
/// the test harness runs them on parallel threads. Serialize them.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The temp root as the script will see it: `/var` is a symlink to
/// `/private/var` on macOS, and a grant written with the unresolved spelling
/// does not match a resolved script path.
fn temp_root() -> PathBuf {
    std::env::temp_dir().canonicalize().unwrap()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = temp_root().join(format!("libdeno-rungrant-{}-{name}", std::process::id()));
    fs::create_dir_all(dir.join("bin")).unwrap();
    dir
}

fn executable(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn clear_loader_env() {
    let loader: Vec<String> = std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with("LD_") || key.starts_with("DYLD_"))
        .collect();
    for key in loader {
        std::env::remove_var(key);
    }
}

/// Runs the entry with `permissions`, returning `(exit_code, stdout, stderr)`.
fn run_script(dir: &Path, script: &str, permissions: Vec<String>) -> (i32, String, String) {
    fs::write(dir.join("main.js"), script).unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    let output = libdeno::run_in_subprocess_with_output(
        dir.join("main.js"),
        &LibdenoOptions {
            permissions,
            capture_stdout: true,
            capture_stderr: true,
            ..Default::default()
        },
    )
    .expect("the script must run");
    (
        output.exit_code,
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
        String::from_utf8_lossy(&output.stderr).trim().to_string(),
    )
}

const SPAWN_MYTOOL: &str =
    "const out = new Deno.Command('mytool', { stdout: 'piped' }).outputSync();\n\
     console.log(new TextDecoder().decode(out.stdout).trim());\n";

#[test]
fn a_run_grant_pins_a_name_resolved_through_path() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    clear_loader_env();
    let dir = temp_dir("path");
    executable(&dir.join("bin/mytool"), "#!/bin/sh\necho HIJACKED\n");
    let previous_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var(
        "PATH",
        format!("{}:{previous_path}", dir.join("bin").display()),
    );
    let read = format!("--allow-read={}", dir.display());

    let (granted_code, granted_out, _) = run_script(
        &dir,
        SPAWN_MYTOOL,
        vec![read.clone(), "--allow-run=mytool".to_string()],
    );
    let (other_code, _, other_err) = run_script(
        &dir,
        SPAWN_MYTOOL,
        vec![read, "--allow-run=other".to_string()],
    );
    std::env::set_var("PATH", previous_path);
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        (granted_code, granted_out.as_str()),
        (0, "HIJACKED"),
        "a grant on the name must run whatever PATH resolves it to"
    );
    assert_ne!(other_code, 0, "a grant on another name must deny this one");
    assert!(
        other_err.contains("Requires run access"),
        "unexpected denial: {other_err}"
    );
}

/// A script that supplies its own `PATH` does not get to pick the binary.
#[test]
fn a_script_supplied_path_does_not_redirect_a_run_grant() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    clear_loader_env();
    let dir = temp_dir("own-path");
    executable(&dir.join("bin/mytool"), "#!/bin/sh\necho EVIL\n");
    let script = format!(
        "const out = new Deno.Command('mytool', {{\n\
         \x20 stdout: 'piped',\n\
         \x20 env: {{ PATH: {path:?} }},\n\
         }}).outputSync();\n\
         console.log(new TextDecoder().decode(out.stdout).trim());\n",
        path = format!("{}:/usr/bin:/bin", dir.join("bin").display())
    );
    let (code, stdout, stderr) = run_script(
        &dir,
        &script,
        vec![
            format!("--allow-read={}", dir.display()),
            "--allow-run=mytool".to_string(),
        ],
    );
    let _ = fs::remove_dir_all(&dir);

    assert_ne!(code, 0, "the script must not reach its own PATH: {stdout}");
    assert!(
        stderr.contains("Requires run access"),
        "unexpected denial: {stderr}"
    );
}

/// The loader variables are a veto, not a silent pass-through.
#[test]
fn a_loader_variable_requires_a_global_run_grant() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = temp_dir("loader");
    let read = format!("--allow-read={}", dir.display());
    let spawn_echo = "const out = new Deno.Command('/bin/echo', { args: ['hi'], stdout: 'piped' }).outputSync();\n\
         console.log(new TextDecoder().decode(out.stdout).trim());\n";

    clear_loader_env();
    let (clean_code, clean_out, _) = run_script(
        &dir,
        spawn_echo,
        vec![read.clone(), "--allow-run=/bin/echo".to_string()],
    );

    std::env::set_var("LD_LIBRARY_PATH", "/opt/homebrew/lib");
    let (vetoed_code, _, vetoed_err) = run_script(
        &dir,
        spawn_echo,
        vec![read.clone(), "--allow-run=/bin/echo".to_string()],
    );
    let (global_code, _, _) = run_script(
        &dir,
        spawn_echo,
        vec![read.clone(), "--allow-run".to_string()],
    );
    // An empty value is not a loader redirect.
    std::env::set_var("LD_LIBRARY_PATH", "");
    let (empty_code, _, _) = run_script(
        &dir,
        spawn_echo,
        vec![read, "--allow-run=/bin/echo".to_string()],
    );
    std::env::remove_var("LD_LIBRARY_PATH");
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        (clean_code, clean_out.as_str()),
        (0, "hi"),
        "a scoped grant must work without loader variables"
    );
    assert_ne!(
        vetoed_code, 0,
        "a scoped grant must not pass LD_LIBRARY_PATH to a child"
    );
    assert!(
        vetoed_err.contains("LD_LIBRARY_PATH"),
        "the denial must name the variable: {vetoed_err}"
    );
    assert_eq!(global_code, 0, "a global --allow-run may pass the variable");
    assert_eq!(empty_code, 0, "an empty value is not a redirect");
}
