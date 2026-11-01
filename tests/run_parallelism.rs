//! Ordinary in-process runs really overlap.
//!
//! `docs/api.md` promises "independent runs can execute in parallel" for both
//! `run` and a shared `LibdenoRuntime`. The failure mode that would break it is
//! a process-global lease, which is what in-process capture already takes — see
//! `capture_parallelism.rs`, where a captured run rejects a concurrent one.
//! Every existing test of the plain path only checks that a run is *not
//! rejected*, which an accidental `RunLease` would also satisfy: it would queue
//! instead of failing.
//!
//! The rendezvous below makes overlap observable without a wall-clock bound.
//! Each script writes its own marker and then spins until the peer's marker
//! exists. Serialized runs never see each other's marker, so the first one
//! exits 7 after its wait expires.

use std::fs;
use std::path::PathBuf;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-parrun-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn rendezvous_entry(dir: &std::path::Path) -> PathBuf {
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "const [mine, peer] = Deno.args;\n\
         Deno.writeTextFileSync(mine, 'ready');\n\
         const until = Date.now() + 5000;\n\
         while (Date.now() < until) {\n\
         \x20 try { Deno.statSync(peer); Deno.exit(0); } catch (_) {}\n\
         }\n\
         Deno.exit(7);\n",
    )
    .unwrap();
    entry
}

fn options_for(mine: &std::path::Path, peer: &std::path::Path) -> libdeno::LibdenoOptions {
    libdeno::LibdenoOptions {
        allow_all_permissions: true,
        args: vec![
            mine.to_string_lossy().into_owned(),
            peer.to_string_lossy().into_owned(),
        ],
        ..Default::default()
    }
}

fn markers(dir: &std::path::Path) -> [PathBuf; 2] {
    [dir.join("a.ready"), dir.join("b.ready")]
}

/// Runs both halves on separate threads and returns the two exit codes.
fn rendezvous(
    run: impl Fn(usize, &std::path::Path, &std::path::Path) -> Result<i32, libdeno::LibdenoError>
        + Send
        + Sync
        + 'static,
    dir: &std::path::Path,
) -> Vec<i32> {
    let run = std::sync::Arc::new(run);
    let markers = markers(dir);
    let handles: Vec<_> = (0..2)
        .map(|index| {
            let run = run.clone();
            let mine = markers[index].clone();
            let peer = markers[1 - index].clone();
            std::thread::spawn(move || run(index, &mine, &peer))
        })
        .collect();
    handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap())
        .collect()
}

#[test]
fn two_ordinary_runs_rendezvous_with_each_other() {
    let dir = temp_dir("rendezvous");
    let entry = rendezvous_entry(&dir);
    let codes = rendezvous(
        move |_, mine, peer| libdeno::run(&entry, &options_for(mine, peer)),
        &dir,
    );
    let _ = fs::remove_dir_all(dir);
    assert_eq!(
        codes,
        vec![0, 0],
        "a run exited 7, which means it never saw the other run's marker: plain runs \
         are serialized behind a process-global lease"
    );
}

#[test]
fn two_runs_on_one_shared_runtime_rendezvous_with_each_other() {
    let dir = temp_dir("shared-runtime");
    let entry = rendezvous_entry(&dir);
    let build = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let shared = build.block_on(libdeno::LibdenoRuntime::new(&dir)).unwrap();
    drop(build);

    let codes = rendezvous(
        move |_, mine, peer| libdeno::run_with(&shared, &entry, &options_for(mine, peer)),
        &dir,
    );
    let _ = fs::remove_dir_all(dir);
    assert_eq!(
        codes,
        vec![0, 0],
        "two runs on one LibdenoRuntime were serialized; `docs/api.md` promises they \
         execute in parallel, and only the rebuild path takes a lock"
    );
}
