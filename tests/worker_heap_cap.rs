//! `max_heap_bytes` is forwarded to web workers, and the overflow shape differs
//! by isolate.
//!
//! A worker is a separate V8 isolate. The cap reaches it (otherwise
//! `new Worker(...)` would be a way around the limit), but the error surfaces
//! through the worker's own uncaught-error path rather than as the main
//! isolate's `execution terminated`. `docs/api.md` documents both shapes.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("libdeno-workerheap-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

const ALLOCATE_PAST_CAP: &str = "const keep = [];\n\
     for (let i = 0; i < 2000; i++) { keep.push(new Array(100000).fill(i)); }\n\
     console.log('SURVIVED');\n";

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        max_heap_bytes: Some(8 << 20),
        // A worker that never reports back would hang the run; bound it.
        execution_deadline: Some(Duration::from_secs(10)),
        ..Default::default()
    }
}

#[test]
fn a_worker_overflow_surfaces_as_the_workers_own_error() {
    let dir = temp_dir("worker");
    fs::write(dir.join("worker.js"), ALLOCATE_PAST_CAP).unwrap();
    fs::write(
        dir.join("main.js"),
        "const worker = new Worker(new URL('./worker.js', import.meta.url).href, { type: 'module' });\n\
         // The worker dies on its own; the main script just waits for it.\n\
         await new Promise((resolve) => setTimeout(resolve, 6000));\n",
    )
    .unwrap();

    let error = libdeno::run(dir.join("main.js"), &options())
        .expect_err("the worker must not survive an allocation past the cap");
    let message = error.to_string();
    let _ = fs::remove_dir_all(&dir);

    assert!(
        message.contains("Unhandled error in child worker"),
        "a worker overflow arrives through the worker's error path, got: {message}"
    );
    // The termination text is the inner cause and does not reach the rendered
    // message; the distinguishing marker is the worker wrapper above. The main
    // isolate keeps `execution terminated`, which the other test asserts.
    assert!(
        !message.contains("execution terminated\n"),
        "a worker overflow must not render as a bare termination: {message}"
    );
}

/// The same allocation in the main isolate keeps the documented shape, so the
/// two paths are distinguishable and the difference is not an artifact of the
/// cap being ignored.
#[test]
fn a_main_isolate_overflow_keeps_the_execution_terminated_shape() {
    let dir = temp_dir("main");
    fs::write(dir.join("main.js"), ALLOCATE_PAST_CAP).unwrap();

    let error = libdeno::run(dir.join("main.js"), &options())
        .expect_err("the main isolate must not survive an allocation past the cap");
    let message = error.to_string();
    let _ = fs::remove_dir_all(&dir);

    assert!(
        message.contains("execution terminated"),
        "expected the main-isolate termination shape, got: {message}"
    );
    assert!(
        !message.contains("Unhandled error in child worker"),
        "the main isolate must not report a worker error, got: {message}"
    );
}
