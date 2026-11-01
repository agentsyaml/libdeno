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

/// A warm analysis cache must not act as a permission bypass.
///
/// The cache stores `ModuleInfo` (dependency lists, media type) keyed by
/// `(specifier, source hash)`, and it is process-global. A run with a narrower
/// grant that hits a warm entry must still be refused by the file fetcher: the
/// cached analysis says what the module contains, not whether this run may read
/// it. If that ever inverted, one privileged run would unlock a file for every
/// later unprivileged run in the same process.
#[test]
fn a_warm_analysis_cache_does_not_bypass_read_permissions() {
    let dir = temp_dir("perm");
    let entry = dir.join("main.js");
    let dep = dir.join("dep.js");
    fs::write(&dep, "export const value = 'secret';").unwrap();
    fs::write(
        &entry,
        "try {\n\
         \x20 const module = await import('./dep.js');\n\
         \x20 console.log('READ ' + module.value);\n\
         } catch {\n\
         \x20 console.log('DENIED');\n\
         }\n",
    )
    .unwrap();

    // Warm the cache with a grant that covers dep.js. Output goes through a
    // marker file: in-process capture is process-global and other tests in
    // this binary run in parallel.
    let marker = dir.join("out.txt");
    let run_with = |permissions: Vec<String>| -> String {
        let _ = fs::remove_file(&marker);
        fs::write(
            &entry,
            format!(
                "try {{\n\
                 \x20 const module = await import('./dep.js');\n\
                 \x20 Deno.writeTextFileSync({marker:?}, 'READ ' + module.value);\n\
                 }} catch {{\n\
                 \x20 Deno.writeTextFileSync({marker:?}, 'DENIED');\n\
                 }}\n",
                marker = marker.to_string_lossy()
            ),
        )
        .unwrap();
        run(
            &entry,
            &LibdenoOptions {
                permissions,
                cwd: Some(dir.clone()),
                ..Default::default()
            },
        )
        .expect("the run must not fail at the host level");
        fs::read_to_string(&marker).unwrap_or_default()
    };

    // Both runs need write access for the marker; only the read scope differs.
    let write = format!("--allow-write={}", dir.display());
    let granted = run_with(vec![
        format!("--allow-read={}", dir.display()),
        write.clone(),
    ]);
    // Same source, so the analysis cache entry is a hit. Only the entry file is
    // granted for reading this time.
    let narrowed = run_with(vec![format!("--allow-read={}", entry.display()), write]);
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(granted, "READ secret", "the granted run must read dep.js");
    assert_eq!(
        narrowed, "DENIED",
        "a cache hit must not let a narrower run read a file it was not granted"
    );
}
