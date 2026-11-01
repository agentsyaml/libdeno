//! libdeno starts with no import host granted; the Deno CLI does not.
//!
//! `docs/permissions.md` lists the hosts `deno run` pre-populates —
//! `deno.land:443`, `jsr.io:443`, `esm.sh:443`, and four more — and says
//! libdeno pre-populates none of them, so the same run that works under the CLI
//! is denied here until the host is named. That sentence is the one place the
//! permission surface is *narrower* than the CLI, which makes it both the most
//! likely to regress and the least likely to be noticed if it did: a runtime
//! that quietly seeded those hosts would look like it was working.

use std::fs;
use std::path::PathBuf;

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("libdeno-importgrant-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_import(name: &str, specifier: &str, permissions: Vec<String>) -> Result<i32, String> {
    let dir = temp_dir(name);
    let entry = dir.join("main.js");
    fs::write(&entry, format!("await import({specifier:?});\n")).unwrap();
    let mut permissions = permissions;
    permissions.push(format!("--allow-read={}", dir.display()));
    let result = libdeno::run(
        &entry,
        &LibdenoOptions {
            permissions,
            ..Default::default()
        },
    )
    .map_err(|error| error.to_string());
    let _ = fs::remove_dir_all(dir);
    result
}

#[test]
fn a_cli_default_import_host_is_denied_without_an_explicit_grant() {
    // `jsr.io:443` is in the CLI's default allowlist, so `deno run` reaches it
    // with only `--allow-read`. libdeno must not.
    let error = run_import("jsr", "https://jsr.io/@std/assert/1.0.0/mod.ts", Vec::new())
        .expect_err("libdeno must not pre-populate the CLI's import hosts");
    assert!(
        error.contains("Requires import access to \"jsr.io:443\""),
        "expected the documented denial, got: {error}"
    );
}

#[test]
fn granting_the_host_by_name_admits_it() {
    // The other half: the denial is about the missing grant, not about an
    // unreachable specifier. A host-scoped grant moves the outcome past the
    // permission check entirely (it may still fail on the network, which this
    // asserts is not a permission error).
    let outcome = run_import(
        "jsr-granted",
        "https://jsr.io/@std/assert/1.0.0/mod.ts",
        vec!["--allow-import=jsr.io".to_string()],
    );
    match outcome {
        Ok(_) => {}
        Err(message) => assert!(
            !message.contains("Requires import access"),
            "a named host must clear the import check, got: {message}"
        ),
    }
}
