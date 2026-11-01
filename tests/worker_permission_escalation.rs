//! A worker's explicit permission set can narrow the parent's grants but never
//! widen them.
//!
//! `SECURITY.md` documents this: asking for more than the parent holds fails
//! the run instead of clamping silently. The distinction matters because a
//! silent clamp would report success while the worker ran with less than the
//! script asked for — and a silent widen would be a sandbox escape.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("libdeno-workerperm-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn options(dir: &std::path::Path) -> LibdenoOptions {
    LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        execution_deadline: Some(Duration::from_secs(10)),
        ..Default::default()
    }
}

/// `read: true` means "everywhere", which the parent did not grant.
#[test]
fn a_worker_cannot_widen_the_parent_read_grant() {
    let dir = temp_dir("widen");
    fs::write(dir.join("worker.js"), "postMessage('started');\n").unwrap();
    fs::write(
        dir.join("main.js"),
        "const worker = new Worker(new URL('./worker.js', import.meta.url).href, {\n\
         \x20 type: 'module',\n\
         \x20 deno: { permissions: { read: true } },\n\
         });\n\
         worker.onmessage = () => console.log('WORKER STARTED');\n\
         await new Promise((resolve) => setTimeout(resolve, 600));\n",
    )
    .unwrap();

    let error = libdeno::run(dir.join("main.js"), &options(&dir))
        .expect_err("a widened worker permission set must fail the run");
    let message = error.to_string();
    let _ = fs::remove_dir_all(&dir);

    assert!(
        message.contains("Can't escalate parent thread permissions"),
        "the failure must name escalation, not look like a script bug: {message}"
    );
}

/// The same worker with no explicit set inherits and starts normally.
#[test]
fn a_worker_without_an_explicit_set_inherits_and_starts() {
    let dir = temp_dir("inherit");
    let marker = dir.join("started.txt");
    fs::write(
        dir.join("worker.js"),
        format!(
            "Deno.writeTextFileSync({marker:?}, 'worker ran');\npostMessage('done');\n",
            marker = marker.to_string_lossy()
        ),
    )
    .unwrap();
    fs::write(
        dir.join("main.js"),
        "const worker = new Worker(new URL('./worker.js', import.meta.url).href, { type: 'module' });\n\
         await new Promise((resolve) => { worker.onmessage = resolve; setTimeout(resolve, 3000); });\n\
         worker.terminate();\n",
    )
    .unwrap();

    libdeno::run(
        dir.join("main.js"),
        &LibdenoOptions {
            permissions: vec![
                format!("--allow-read={}", dir.display()),
                format!("--allow-write={}", dir.display()),
            ],
            execution_deadline: Some(Duration::from_secs(10)),
            ..Default::default()
        },
    )
    .expect("an inheriting worker must start");
    let observed = fs::read_to_string(&marker).ok();
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        observed.as_deref(),
        Some("worker ran"),
        "an inheriting worker runs with the parent's grants"
    );
}
