//! A workspace member resolves through both entry points.
//!
//! `deno.json` `workspace` entries plus a declared `package.json` dependency
//! are how Deno resolves an internal package without publishing it. Two things
//! had to agree for that to work: the resolver stack discovers the workspace at
//! its canonical directory, and the entry module's `file:` URL must be built
//! from the same canonical spelling.
//!
//! They did not. `run` canonicalized its resolution base before resolving the
//! entry, so it happened to match. `LibdenoRuntime::new` canonicalizes its
//! directory too, but `run_with` passed the caller's entry path through
//! untouched — and on macOS `std::env::temp_dir()` is `/var/...`, a symlink to
//! `/private/var/...`. The entry then lived under one spelling of the tree
//! while `node_modules` was discovered under the other, and npm resolution
//! answered `Import "member" not a dependency` for a declared package.
//!
//! Both cases below use the *non*-canonical entry path on purpose: canonical
//! fixtures would hide the regression.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    // Deliberately not canonicalized; see the module comment.
    let dir = std::env::temp_dir().join(format!("libdeno-ws-member-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// One workspace with a single member, reachable only as `member`.
fn workspace(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir.join("member")).unwrap();
    fs::write(
        dir.join("member/package.json"),
        r#"{"name":"member","version":"1.0.0","type":"module","exports":"./index.js"}"#,
    )
    .unwrap();
    fs::write(dir.join("member/index.js"), "export const v = 'M';").unwrap();
    fs::write(dir.join("deno.json"), r#"{"workspace":["./member"]}"#).unwrap();
    fs::write(
        dir.join("package.json"),
        r#"{"name":"root","private":true,"dependencies":{"member":"1.0.0"}}"#,
    )
    .unwrap();
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "import { v } from 'member';\n\
         Deno.writeTextFileSync(new URL('./out.txt', import.meta.url), v);\n",
    )
    .unwrap();
    entry
}

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    }
}

#[test]
fn run_resolves_a_workspace_member() {
    let dir = temp_dir("run");
    let entry = workspace(&dir);

    let result = libdeno::run(
        &entry,
        &LibdenoOptions {
            cwd: Some(dir.clone()),
            ..options()
        },
    );
    let written = fs::read_to_string(dir.join("out.txt")).ok();
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        result.map_err(|error| error.to_string()),
        Ok(0),
        "a declared workspace member must resolve through `run`"
    );
    assert_eq!(written.as_deref(), Some("M"));
}

#[test]
fn run_with_resolves_a_workspace_member() {
    let dir = temp_dir("run-with");
    let entry = workspace(&dir);

    let build = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = build.block_on(libdeno::LibdenoRuntime::new(&dir)).unwrap();
    drop(build);

    let result = libdeno::run_with(&runtime, &entry, &options());
    let written = fs::read_to_string(dir.join("out.txt")).ok();
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        result.map_err(|error| error.to_string()),
        Ok(0),
        "a declared workspace member must resolve through `run_with` too; \
         `LibdenoRuntime` canonicalizes its directory, so the entry URL must be \
         canonicalized to match"
    );
    assert_eq!(written.as_deref(), Some("M"));
}
