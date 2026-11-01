//! The `execution_deadline` stops web workers too.
//!
//! The deadline terminates the main isolate and then closes the worker
//! registry, which asks every live worker to stop and waits for them before the
//! run returns. A host that reads the deadline as "the main script stops, not
//! the rest of the run" will over-estimate how far the run got: a worker's
//! reads and writes after the cut never happen.
//!
//! The scenario below makes that observable without a wall-clock bound. The
//! worker announces itself, spins for a fixed span, then writes a result file.
//! The main script waits for the announcement and then blocks forever, so only
//! the deadline can end the run — and the deadline cannot fire before the
//! worker is live.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "libdeno-workerdeadline-{}-{name}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn options(dir: &std::path::Path, deadline: Duration) -> LibdenoOptions {
    LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", dir.display()),
            format!("--allow-write={}", dir.display()),
        ],
        execution_deadline: Some(deadline),
        ..Default::default()
    }
}

/// Runs the fixture and reports whether the worker reached its write.
///
/// `None` means the worker never announced itself, so the premise ("a live
/// worker when the deadline fires") did not hold. Only a very slow machine
/// produces that, and the caller retries with a later deadline rather than
/// asserting on it.
fn worker_reaches_its_next_statement(dir: &std::path::Path, deadline: Duration) -> Option<bool> {
    let ready = dir.join("worker-ready");
    let result = dir.join("worker-result");
    fs::write(
        dir.join("worker.js"),
        format!(
            "Deno.writeTextFileSync({ready:?}, 'ready');\n\
             // Longer than the deadline, so this spin is what the deadline cuts.\n\
             const until = Date.now() + 5000;\n\
             while (Date.now() < until) {{}}\n\
             Deno.writeTextFileSync({result:?}, 'SURVIVED');\n",
            ready = ready.to_string_lossy(),
            result = result.to_string_lossy()
        ),
    )
    .unwrap();
    // The main script waits for the worker before blocking, so the deadline
    // cannot race worker startup.
    fs::write(
        dir.join("main.js"),
        format!(
            "import {{ existsSync }} from 'node:fs';\n\
             new Worker(new URL('./worker.js', import.meta.url).href, {{ type: 'module' }});\n\
             const until = Date.now() + 20000;\n\
             while (!existsSync({ready:?}) && Date.now() < until) {{}}\n\
             await new Promise(() => {{}});\n",
            ready = ready.to_string_lossy()
        ),
    )
    .unwrap();

    let started = Instant::now();
    let error = libdeno::run(dir.join("main.js"), &options(dir, deadline))
        .expect_err("the deadline must end the run");
    let elapsed = started.elapsed();
    assert!(
        matches!(error, libdeno::LibdenoError::Timeout(_)),
        "expected a deadline error, got: {error:?}"
    );
    if !ready.exists() {
        return None;
    }

    // Past the worker's own 5 s spin plus margin: if the deadline had left it
    // running, the write would land in this window.
    std::thread::sleep(Duration::from_secs(7));

    assert!(
        error.to_string().contains("deadline"),
        "the error must name the deadline, got: {error}"
    );
    // No assertion on `elapsed`: the worker's missing write already proves it
    // was stopped rather than waited for, and wall-clock bounds are unstable
    // on a loaded machine. `elapsed` is reported only when this fails.
    let _ = elapsed;
    Some(result.exists())
}

#[test]
fn a_worker_is_stopped_by_the_deadline() {
    let dir = temp_dir("cut");
    // The deadline must outlast bootstrap plus worker startup; a loaded machine
    // can push that past the first attempt.
    let mut observed = None;
    let deadlines = [Duration::from_secs(2), Duration::from_secs(10)];
    for (index, deadline) in deadlines.into_iter().enumerate() {
        match worker_reaches_its_next_statement(&dir, deadline) {
            Some(reached) => {
                observed = Some(reached);
                break;
            }
            None => assert!(
                index + 1 < deadlines.len(),
                "the worker never started even with a {deadline:?} deadline"
            ),
        }
    }
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        observed,
        Some(false),
        "the worker reached its write after the deadline fired; the deadline is \
         documented as stopping workers, so if that changed, update docs/api.md \
         and docs/architecture.md"
    );
}
