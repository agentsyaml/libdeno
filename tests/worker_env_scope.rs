//! A worker with `--allow-env` rewrites the host process environment.
//!
//! The permission model treats `--allow-env` as a capability of the run, and a
//! worker created with `permissions: 'inherit'` (or no narrowing) holds the
//! same capability. `Deno.env.set` writes the real process environment, so the
//! write is visible to the main thread, to the host, and to every later run in
//! that process.
//!
//! The control-variable deny list still holds inside a worker, which is what
//! keeps this from becoming an arbitrary-code grant: `LIBDENO_HOST_EXE` and the
//! resolver inputs stay refused.
//!
//! Narrowing the worker's permissions removes the capability, so a host that
//! creates workers with an explicit permission set does not inherit this.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use libdeno::LibdenoOptions;

/// These tests read and write process-global environment variables.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("libdeno-workerenv-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn options(dir: &Path) -> LibdenoOptions {
    LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", dir.display()),
            "--allow-env".to_string(),
        ],
        capture_stdout: true,
        capture_stderr: true,
        // A worker that never posts back would hang the run; bound it.
        execution_deadline: Some(Duration::from_secs(10)),
        ..Default::default()
    }
}

#[test]
fn a_worker_with_env_access_rewrites_the_host_environment() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = temp_dir("inherit");
    let mark = "LIBDENO_WORKER_ENV_MARK";
    std::env::remove_var(mark);

    fs::write(
        dir.join("worker.js"),
        format!("Deno.env.set({mark:?}, 'from-worker');\npostMessage('set');\n"),
    )
    .unwrap();
    fs::write(
        dir.join("main.js"),
        format!(
            "const worker = new Worker(new URL('./worker.js', import.meta.url).href, {{ type: 'module' }});\n\
             worker.onmessage = () => {{\n\
             \x20 console.log('main=' + Deno.env.get({mark:?}));\n\
             \x20 worker.terminate();\n\
             }};\n"
        ),
    )
    .unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));

    let output = libdeno::run_in_subprocess_with_output(dir.join("main.js"), &options(&dir))
        .expect("the run must finish");
    let host_value = std::env::var(mark);
    std::env::remove_var(mark);
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "main=from-worker",
        "the worker's write must be visible to the main thread"
    );
    // The run is a subprocess, so the *host* environment is untouched here;
    // what this asserts is that the child process saw one environment, not two.
    assert!(
        host_value.is_err(),
        "a subprocess run must not write the host environment, got {host_value:?}"
    );
}

/// In-process, the worker's write reaches the host process itself.
#[cfg(not(windows))]
#[test]
fn in_process_a_worker_write_reaches_the_host_process() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = temp_dir("in-process");
    let mark = "LIBDENO_WORKER_ENV_HOST_MARK";
    std::env::remove_var(mark);

    fs::write(
        dir.join("worker.js"),
        format!("Deno.env.set({mark:?}, 'leaked');\npostMessage('set');\n"),
    )
    .unwrap();
    fs::write(
        dir.join("main.js"),
        "const worker = new Worker(new URL('./worker.js', import.meta.url).href, { type: 'module' });\n\
         worker.onmessage = () => worker.terminate();\n",
    )
    .unwrap();

    libdeno::run_with_output(dir.join("main.js"), &options(&dir)).expect("the run must finish");
    let host_value = std::env::var(mark);
    std::env::remove_var(mark);
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        host_value.as_deref(),
        Ok("leaked"),
        "an in-process worker no longer writes the host environment; if that is \
         deliberate, update SECURITY.md and delete this test"
    );
}

/// The deny list is enforced inside a worker too, so the environment write
/// cannot reach a control variable.
#[test]
fn a_worker_cannot_write_a_control_variable() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = temp_dir("control");
    fs::write(
        dir.join("worker.js"),
        "const refused = [];\n\
         for (const name of ['LIBDENO_HOST_EXE', 'DENO_DIR', 'JSR_URL']) {\n\
         \x20 try { Deno.env.set(name, '/tmp/evil'); } catch { refused.push(name); }\n\
         }\n\
         postMessage(refused.join(','));\n",
    )
    .unwrap();
    fs::write(
        dir.join("main.js"),
        "const worker = new Worker(new URL('./worker.js', import.meta.url).href, { type: 'module' });\n\
         worker.onmessage = (event) => { console.log(event.data); worker.terminate(); };\n",
    )
    .unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));

    let output = libdeno::run_in_subprocess_with_output(dir.join("main.js"), &options(&dir))
        .expect("the run must finish");
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "LIBDENO_HOST_EXE,DENO_DIR,JSR_URL",
        "a worker must not be able to write a control variable"
    );
}

/// A worker created with an explicit permission set does not hold `--allow-env`.
#[test]
fn a_narrowed_worker_cannot_write_the_environment() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = temp_dir("narrowed");
    let mark = "LIBDENO_WORKER_ENV_NARROW_MARK";
    std::env::remove_var(mark);

    fs::write(
        dir.join("worker.js"),
        format!(
            "try {{ Deno.env.set({mark:?}, 'from-worker'); postMessage('ALLOWED'); }}\n\
             catch (error) {{ postMessage('DENIED'); }}\n"
        ),
    )
    .unwrap();
    fs::write(
        dir.join("main.js"),
        format!(
            "const worker = new Worker(new URL('./worker.js', import.meta.url).href, {{\n\
             \x20 type: 'module',\n\
             \x20 deno: {{ permissions: {{ read: [{dir:?}] }} }},\n\
             }});\n\
             worker.onmessage = (event) => {{ console.log(event.data); worker.terminate(); }};\n",
            dir = dir.to_string_lossy()
        ),
    )
    .unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));

    let output = libdeno::run_in_subprocess_with_output(dir.join("main.js"), &options(&dir))
        .expect("the run must finish");
    let host_value = std::env::var(mark);
    std::env::remove_var(mark);
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "DENIED",
        "a narrowed worker must not hold the parent's env capability"
    );
    assert!(host_value.is_err(), "the write must not have happened");
}

/// A web worker's `console.log` lands in the same capture buffer as the main
/// isolate's.
///
/// `docs/api.md` states this as a consequence of capture being process-wide: a
/// worker is another isolate in the same process, not another process. A host
/// that assumes worker output needs separate plumbing would add a channel it
/// does not need — or, worse, would believe worker output is being dropped.
#[cfg(not(windows))]
#[test]
fn worker_output_lands_in_the_same_capture_buffer() {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let dir = temp_dir("worker-output");
    fs::write(
        dir.join("worker.js"),
        "console.log('FROM-WORKER');\nconsole.error('WORKER-ERR');\n",
    )
    .unwrap();
    // Terminate immediately: this test is about the buffer, not about waiting
    // for a message, and a waiting main script would keep the run alive.
    fs::write(
        dir.join("main.js"),
        "const worker = new Worker(new URL('./worker.js', import.meta.url).href, { type: 'module' });\n\
         worker.terminate();\n\
         console.log('FROM-MAIN');\n",
    )
    .unwrap();

    let output = libdeno::run_with_output(
        dir.join("main.js"),
        &LibdenoOptions {
            allow_all_permissions: true,
            capture_stdout: true,
            capture_stderr: true,
            execution_deadline: Some(Duration::from_secs(10)),
            ..Default::default()
        },
    )
    .expect("the run must succeed");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let _ = fs::remove_dir_all(&dir);

    assert!(
        stdout.contains("FROM-WORKER"),
        "a worker's console.log must reach the run's stdout: {stdout:?}"
    );
    assert!(
        stdout.contains("FROM-MAIN"),
        "the main isolate's output must be in the same buffer: {stdout:?}"
    );
    assert!(
        stderr.contains("WORKER-ERR"),
        "a worker's console.error must reach the run's stderr: {stderr:?}"
    );
}
