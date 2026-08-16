//! Cross-run module-analysis cache correctness.
//!
//! The process-level analysis cache (src/analysis_cache.rs) is keyed by
//! (specifier, source hash) and survives across `run` calls. These tests pin
//! the correctness contract: a source change must invalidate the cached
//! analysis — a stale dependency list would break the second run (it would
//! try to load a dependency the new source no longer imports, or miss one it
//! does).

use std::fs;
use std::path::PathBuf;

use libdeno::{run, LibdenoOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-ac-{}-{}", std::process::id(), name));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn changed_source_invalidates_cached_analysis() {
    let dir = temp_dir("invalidate");
    let entry = dir.join("main.ts");
    let dep = dir.join("dep.ts");

    // v1: imports ./dep.ts (present). The analysis cache stores v1's
    // dependency list under (main.ts, hash(v1)).
    fs::write(&dep, "export const v = 1;").unwrap();
    fs::write(
        &entry,
        "import { v } from './dep.ts'; console.log('v1', v);",
    )
    .unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(run(&entry, &options).unwrap(), 0, "v1 must run");

    // v2: drops the import entirely and dep.ts is gone. If the cache served
    // v1's analysis for v2, the graph would still try to load ./dep.ts and
    // fail — the run must succeed only because the cache invalidated.
    fs::remove_file(&dep).unwrap();
    fs::write(&entry, "console.log('v2', 'no-dep');").unwrap();
    assert_eq!(
        run(&entry, &options).unwrap(),
        0,
        "v2 must run with no stale dep"
    );

    // v3: new dependency appears; the run must pick it up (not serve v2's
    // no-dep analysis).
    fs::write(&dep, "export const w = 2;").unwrap();
    fs::write(
        &entry,
        "import { w } from './dep.ts'; console.log('v3', w);",
    )
    .unwrap();
    assert_eq!(
        run(&entry, &options).unwrap(),
        0,
        "v3 must run with the new dep"
    );

    let _ = fs::remove_dir_all(&dir);
}
