//! What happens to a process the script spawns, when the run ends.
//!
//! Dropping the run tears down the resource table, and `deno_process`'s
//! `ChildResource::drop` sends `SIGKILL` to every child that still has
//! `kill_on_drop` set. That is the default; a script calls `unref()` (or
//! passes `detached: true`) to opt a process out.
//!
//! Both directions are pinned here because the sentence in `docs/api.md` was
//! wrong in a way no test caught: it said the child outlives the run, and the
//! old test agreed only because it read `ps -o stat=`, which prints `Z` for a
//! zombie — a non-empty string that looked like "still alive".
//!
//! The fixture is a grandchild rather than the direct child, because a killed
//! process can linger as a zombie (its exit status is unreaped) even though it
//! is no longer running. What the doc claim is about is whether the work
//! continues, so the check is whether the grandchild's side effect happens.
//!
//! Deno CLI 2.9.8 behaves the same way in both directions.

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

/// Runs a script that spawns `/bin/sh -c 'sleep 2; touch <marker>'` and then
/// executes `tail`. Returns whether the marker appeared within 4 s, i.e.
/// whether the spawned shell survived the end of the run.
fn spawned_work_outlives(dir: &Path, tail: &str) -> bool {
    let _guard = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let marker = dir.join("marker");
    let _ = fs::remove_file(&marker);
    fs::write(
        dir.join("main.js"),
        format!(
            "const c = new Deno.Command('/bin/sh', {{\n\
             \x20 args: ['-c', 'sleep 2; touch ' + {marker:?}],\n\
             \x20 stdin: 'null', stdout: 'null', stderr: 'null', clearEnv: true }});\n\
             const p = c.spawn();\n\
             {tail}\n",
            marker = marker.to_string_lossy()
        ),
    )
    .unwrap();
    let options = LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", dir.display()),
            format!("--allow-write={}", dir.display()),
            "--allow-run=/bin/sh".to_string(),
        ],
        capture_stdout: true,
        ..Default::default()
    };
    let _ = libdeno::run(dir.join("main.js"), &options);
    std::thread::sleep(Duration::from_secs(4));
    marker.exists()
}

#[test]
fn deno_exit_kills_a_script_spawned_process() {
    let dir = temp_dir("exit");
    let survived = spawned_work_outlives(&dir, "Deno.exit(3);");
    let _ = fs::remove_dir_all(&dir);
    assert!(
        !survived,
        "the spawned process outlived the run; if that is deliberate, update \
         docs/api.md and invert this assertion"
    );
}

#[test]
fn a_thrown_error_kills_a_script_spawned_process() {
    let dir = temp_dir("throw");
    let survived = spawned_work_outlives(&dir, "throw new Error('boom');");
    let _ = fs::remove_dir_all(&dir);
    assert!(
        !survived,
        "the spawned process outlived the run; if that is deliberate, update \
         docs/api.md and invert this assertion"
    );
}

/// The documented escape hatch: `unref()` opts the child out of
/// `kill_on_drop`, so its work continues after the run has returned.
#[test]
fn unref_lets_a_script_spawned_process_outlive_the_run() {
    let dir = temp_dir("unref");
    let survived = spawned_work_outlives(&dir, "p.unref();\nDeno.exit(3);");
    let _ = fs::remove_dir_all(&dir);
    assert!(
        survived,
        "an unref'd child must keep running; if this is a deliberate change, \
         update docs/api.md"
    );
}
