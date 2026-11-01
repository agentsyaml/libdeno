//! A process the script spawns outlives an in-process run.
//!
//! Neither `Deno.exit()` nor a thrown error signals a process created with
//! `Deno.Command`, so it keeps running with the host's privileges. Deno CLI
//! 2.9.8 behaves the same way. A host that assumes the opposite leaks one
//! process per run, so the behaviour is pinned rather than left implicit.
//!
//! Subprocess mode is different and covered separately: the whole child
//! process exits, and the process the script spawned is reparented to init.
//!
//! The tests kill what they spawned. If an assertion fails because the child
//! died with the run, that is a behaviour change: update `docs/api.md` and
//! delete the test instead of removing the cleanup.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use libdeno::LibdenoOptions;

/// In-process capture is process-global and exclusive.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-spawnleak-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn alive(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|out| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
        .unwrap_or(false)
}

fn kill(pid: u32) {
    let _ = std::process::Command::new("kill")
        .arg(pid.to_string())
        .status();
}

/// The script records the child pid in a file, because a failing run's
/// captured output is not returned to the caller.
fn run_and_read_pid(dir: &Path, tail: &str) -> u32 {
    let _guard = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let pidfile = dir.join("pid.txt");
    let script = format!(
        "const c = new Deno.Command('/bin/sleep', {{\n\
         \x20 args: ['30'], stdin: 'null', stdout: 'null', stderr: 'null', clearEnv: true }});\n\
         const p = c.spawn();\n\
         Deno.writeTextFileSync({pidfile:?}, String(p.pid));\n\
         {tail}\n",
        pidfile = pidfile.to_string_lossy()
    );
    fs::write(dir.join("main.js"), script).unwrap();
    let options = LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", dir.display()),
            format!("--allow-write={}", dir.display()),
            "--allow-run=/bin/sleep".to_string(),
        ],
        capture_stdout: true,
        ..Default::default()
    };
    let _ = libdeno::run_with_output(dir.join("main.js"), &options);
    fs::read_to_string(&pidfile)
        .expect("the script must record the child pid")
        .trim()
        .parse()
        .expect("the recorded pid must parse")
}

#[test]
fn deno_exit_does_not_reap_a_script_spawned_process() {
    let dir = temp_dir("exit");
    let pid = run_and_read_pid(&dir, "Deno.exit(3);");
    std::thread::sleep(Duration::from_millis(800));
    let still_alive = alive(pid);
    kill(pid);
    let _ = fs::remove_dir_all(&dir);
    assert!(
        still_alive,
        "the spawned process was reaped with the run; if this is deliberate, \
         update docs/api.md and delete this test"
    );
}

#[test]
fn a_thrown_error_does_not_reap_a_script_spawned_process() {
    let dir = temp_dir("throw");
    let pid = run_and_read_pid(&dir, "throw new Error('boom');");
    std::thread::sleep(Duration::from_millis(800));
    let still_alive = alive(pid);
    kill(pid);
    let _ = fs::remove_dir_all(&dir);
    assert!(
        still_alive,
        "the spawned process was reaped with the run; if this is deliberate, \
         update docs/api.md and delete this test"
    );
}
