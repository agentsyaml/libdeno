//! File writes are in-place, so a concurrent reader can see a partial file.
//!
//! `Deno.writeTextFileSync` and `Deno.writeFileSync` truncate and rewrite the
//! existing file. `writeTextFileSync` + `renameSync` replaces the directory
//! entry instead. That difference is what makes the rename pattern safe, and
//! it is observable without racing: an in-place rewrite keeps the inode, a
//! rename installs a new one.
//!
//! A direct measurement of the consequence (a reader seeing a length between
//! the two written sizes) is recorded in `docs/api.md`: 2 of 200 reads for
//! text, 43 of 400 for binary, 0 of 400 through rename. It is not asserted
//! here because that count is a race, not an invariant — the inode check below
//! is deterministic and fails for the same reason a regression would.

// Inode identity is how this test says "the same file was rewritten, not
// replaced"; std exposes it only on Unix.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-filewrite-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn options(dir: &Path) -> LibdenoOptions {
    LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", dir.display()),
            format!("--allow-write={}", dir.display()),
        ],
        ..Default::default()
    }
}

fn inode(path: &Path) -> u64 {
    fs::metadata(path).unwrap().ino()
}

/// Runs `script` to completion in a subprocess and asserts it succeeded.
fn run_ok(dir: &Path, script: &str) {
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    let entry = dir.join("main.js");
    fs::write(&entry, script).unwrap();
    let code = libdeno::run_in_subprocess(&entry, &options(dir)).expect("the script must run");
    assert_eq!(code, 0, "script failed: {script}");
}

#[test]
fn write_text_file_sync_rewrites_in_place() {
    let dir = temp_dir("text");
    let target = dir.join("target.txt");
    fs::write(&target, "original").unwrap();
    let before = inode(&target);
    run_ok(
        &dir,
        &format!(
            "Deno.writeTextFileSync({:?}, 'replacement content');",
            target.to_string_lossy()
        ),
    );
    let after = inode(&target);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        before, after,
        "the inode changed, so writeTextFileSync now replaces the file; if that \
         is deliberate, update docs/api.md and delete this test"
    );
}

#[test]
fn write_file_sync_rewrites_in_place() {
    let dir = temp_dir("binary");
    let target = dir.join("target.bin");
    fs::write(&target, "original").unwrap();
    let before = inode(&target);
    run_ok(
        &dir,
        &format!(
            "Deno.writeFileSync({:?}, new TextEncoder().encode('replacement'));",
            target.to_string_lossy()
        ),
    );
    let after = inode(&target);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        before, after,
        "the inode changed, so writeFileSync now replaces the file; if that is \
         deliberate, update docs/api.md and delete this test"
    );
}

/// The pattern the docs recommend: the target is replaced, not rewritten, so a
/// reader either opens the old file or the new one.
#[test]
fn a_rename_swap_installs_a_new_inode() {
    let dir = temp_dir("rename");
    let target = dir.join("target.txt");
    fs::write(&target, "original").unwrap();
    let before = inode(&target);
    let temporary = dir.join("target.txt.tmp");
    run_ok(
        &dir,
        &format!(
            "Deno.writeTextFileSync({tmp:?}, 'replacement content');\n\
             Deno.renameSync({tmp:?}, {target:?});",
            tmp = temporary.to_string_lossy(),
            target = target.to_string_lossy()
        ),
    );
    let after = inode(&target);
    let content = fs::read_to_string(&target).unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert_ne!(
        before, after,
        "rename did not replace the directory entry, so it is not an atomic \
         swap here and docs/api.md recommends the wrong fix"
    );
    assert_eq!(content, "replacement content");
}
