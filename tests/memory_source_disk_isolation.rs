//! In-memory source is never materialized on disk.
//!
//! `run_source` registers the entry in a per-run in-memory file map, so the
//! virtual entry exists only for the duration of the run. That matters for a
//! host that executes generated code: the source can contain secrets, and both
//! the working directory and `DENO_DIR` outlive the run.
//!
//! Local runs do not populate the disk cache at all, so the cache scan below
//! guards against a future change that starts writing there rather than
//! proving the cache is in use.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::{LibdenoOptions, SourceLang};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-memsrc-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn contains(path: &Path, needle: &str, hits: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            contains(&path, needle, hits);
        } else if let Ok(bytes) = fs::read(&path) {
            if bytes
                .windows(needle.len())
                .any(|window| window == needle.as_bytes())
            {
                hits.push(path);
            }
        }
    }
}

#[test]
fn in_memory_source_is_not_materialized_on_disk() {
    let root = temp_dir("disk");
    let cache = root.join("deno_dir");
    let work = root.join("work");
    fs::create_dir_all(&cache).unwrap();
    fs::create_dir_all(&work).unwrap();

    // The cache location is read from the environment at run time.
    let previous = std::env::var_os("DENO_DIR");
    std::env::set_var("DENO_DIR", &cache);

    let marker = "UNIQUE_IN_MEMORY_SOURCE_MARKER_9f3a2b";
    let code = format!("const secret = '{marker}';\nconsole.log(secret.length);\n");
    libdeno::run_source(
        &code,
        SourceLang::JavaScript,
        &work,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();

    let mut hits = Vec::new();
    contains(&cache, marker, &mut hits);
    contains(&work, marker, &mut hits);
    // The virtual entry is named `$deno$__libdeno_virtual_<hex>.<ext>`; none of
    // those may survive the run either.
    let mut virtual_files = Vec::new();
    contains(&work, "$deno$__libdeno_virtual_", &mut virtual_files);
    hits.extend(virtual_files);

    match previous {
        Some(value) => std::env::set_var("DENO_DIR", value),
        None => std::env::remove_var("DENO_DIR"),
    }
    let _ = fs::remove_dir_all(&root);

    assert!(
        hits.is_empty(),
        "in-memory source was materialized on disk: {hits:?}"
    );
}
