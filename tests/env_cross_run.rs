//! A script with `--allow-env` rewrites host-process state, and a later run in
//! the same process inherits it.
//!
//! `PATH` and `HOME` cannot be denied without breaking ordinary scripts, so
//! libdeno documents them instead (`SECURITY.md`, "Residual risk: `HOME` and
//! `PATH`"). This test pins the consequence so the documentation keeps a
//! checkable claim: a run that only has `--allow-run=git` executes the script's
//! own `git` after an earlier run rewrote `PATH`.
//!
//! Subprocess runs are unaffected: the child gets a copy of the environment.

// The redirect works through a chmod +x'd `git` on `PATH`; Windows has no
// execute bit, so the mechanism does not exist there.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

/// These tests mutate process-global environment; the harness runs them in
/// parallel threads.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    // `temp_dir()` is a symlink on macOS; grants must use the resolved form.
    let dir = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("libdeno-envcross-{}-{name}", std::process::id()));
    fs::create_dir_all(dir.join("evil")).unwrap();
    dir
}

/// Deno refuses a scoped `--allow-run` while any `LD_*` / `DYLD_*` is set
/// (`cargo test` sets `DYLD_FALLBACK_LIBRARY_PATH` on macOS).
fn clear_loader_env() {
    let loader: Vec<String> = std::env::vars()
        .map(|(key, _)| key)
        .filter(|key| key.starts_with("LD_") || key.starts_with("DYLD_"))
        .collect();
    for key in loader {
        std::env::remove_var(key);
    }
}

fn options(dir: &Path) -> LibdenoOptions {
    LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", dir.display()),
            "--allow-env".to_string(),
            "--allow-run=git".to_string(),
        ],
        capture_stdout: true,
        capture_stderr: true,
        ..Default::default()
    }
}

#[test]
fn a_rewritten_path_redirects_a_later_runs_run_grant() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    clear_loader_env();
    let dir = temp_dir("path");
    let evil = dir.join("evil/git");
    fs::write(&evil, "#!/bin/sh\necho HIJACKED-GIT\n").unwrap();
    fs::set_permissions(&evil, fs::Permissions::from_mode(0o755)).unwrap();

    fs::write(
        dir.join("setter.js"),
        format!(
            "Deno.env.set('PATH', {path:?});\nconsole.log('PATH rewritten');\n",
            path = format!("{}:/usr/bin:/bin", dir.join("evil").display())
        ),
    )
    .unwrap();
    // Only `--allow-run=git`: the grant is a name, so PATH decides the binary.
    fs::write(
        dir.join("user.js"),
        "const out = new Deno.Command('git', { args: ['--version'], stdout: 'piped' }).outputSync();\n\
         console.log(new TextDecoder().decode(out.stdout).trim());\n",
    )
    .unwrap();

    let previous_path = std::env::var_os("PATH");
    let setter = libdeno::run_with_output(dir.join("setter.js"), &options(&dir))
        .expect("the setter must run");
    let user = libdeno::run_with_output(dir.join("user.js"), &options(&dir));
    if let Some(path) = previous_path {
        std::env::set_var("PATH", path);
    }
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        String::from_utf8_lossy(&setter.stdout).trim(),
        "PATH rewritten"
    );
    let user = user.expect("the second run must execute the granted name");
    assert_eq!(
        String::from_utf8_lossy(&user.stdout).trim(),
        "HIJACKED-GIT",
        "PATH no longer steers a `--allow-run` name; if that is deliberate, \
         update SECURITY.md and delete this test"
    );
}

/// The mitigation in `SECURITY.md`: a fresh process is unaffected.
#[test]
fn a_subprocess_run_does_not_inherit_a_rewritten_path() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    clear_loader_env();
    let dir = temp_dir("subprocess");
    let evil = dir.join("evil/git");
    fs::write(&evil, "#!/bin/sh\necho HIJACKED-GIT\n").unwrap();
    fs::set_permissions(&evil, fs::Permissions::from_mode(0o755)).unwrap();

    fs::write(
        dir.join("setter.js"),
        format!(
            "Deno.env.set('PATH', {path:?});\nconsole.log('PATH rewritten');\n",
            path = format!("{}:/usr/bin:/bin", dir.join("evil").display())
        ),
    )
    .unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));

    let previous_path = std::env::var_os("PATH");
    let setter = libdeno::run_in_subprocess_with_output(dir.join("setter.js"), &options(&dir))
        .expect("the setter must run");
    let after = std::env::var_os("PATH");
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        String::from_utf8_lossy(&setter.stdout).trim(),
        "PATH rewritten"
    );
    assert_eq!(
        after, previous_path,
        "a subprocess run must not rewrite the host environment"
    );
}
