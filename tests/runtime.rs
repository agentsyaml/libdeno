//! Wave 2: the reusable resolver stack — `LibdenoRuntime::new` + `run_with`.
//! Covers stack reuse (identical results across runs), config-change
//! fingerprint invalidation, and the per-run permission isolation guarantee.

use std::fs;
use std::path::PathBuf;

use libdeno::{run_with, LibdenoOptions, LibdenoRuntime};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-runtime-{}-{}", std::process::id(), name));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Builds a `LibdenoRuntime` on a throwaway tokio runtime, then drops the
/// runtime: `run_with` rejects calls from inside a tokio runtime.
fn build_runtime(cwd: &std::path::Path) -> LibdenoRuntime {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = rt.block_on(LibdenoRuntime::new(cwd)).unwrap();
    drop(rt);
    runtime
}

#[test]
fn run_with_happy_path_returns_exit_code() {
    // Wave 2 basic usability: a plain script runs to completion with exit 0.
    // The process cwd is never switched (cwd is a resolution base only), so
    // the script observes the host's cwd via Deno.cwd().
    let dir = temp_dir("happy");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        // Absolute path via import.meta.url: relative paths would resolve
        // against the host cwd, not the project dir.
        "Deno.writeTextFileSync(new URL('./out.txt', import.meta.url), Deno.cwd());",
    )
    .unwrap();
    let runtime = build_runtime(&dir);
    let code = run_with(
        &runtime,
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0);
    let host_cwd = fs::canonicalize(std::env::current_dir().unwrap())
        .unwrap()
        .display()
        .to_string();
    // Windows canonicalize returns a \\?\ verbatim path; Deno.cwd() never
    // has the prefix — strip it so the comparison is apples-to-apples.
    #[cfg(windows)]
    let host_cwd = host_cwd
        .strip_prefix(r"\\?\")
        .unwrap_or(&host_cwd)
        .to_string();
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), host_cwd);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn run_with_default_permissions_is_rejected() {
    // The v0.2.0 default-permission semantics apply through the reusable
    // runtime entry point too: empty permissions without the opt-in must fail
    // with a Configuration error, never silently allow-all.
    let dir = temp_dir("default-perm");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('never runs');").unwrap();
    let runtime = build_runtime(&dir);
    let err = run_with(&runtime, &entry, &LibdenoOptions::default()).unwrap_err();
    assert!(
        matches!(err, libdeno::LibdenoError::Configuration(_)),
        "expected a configuration error, got: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn run_with_reuse_produces_identical_results() {
    // The resolver stack is reused across run_with calls: two runs of the
    // same script on the same runtime must behave identically.
    let dir = temp_dir("reuse");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "Deno.writeTextFileSync(new URL('./out.txt', import.meta.url), 'same-result');",
    )
    .unwrap();
    let runtime = build_runtime(&dir);
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(
        fs::read_to_string(dir.join("out.txt")).unwrap(),
        "same-result"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn run_with_rebuilds_when_config_changes() {
    // The config fingerprint is content-hashed, so a same-size edit to
    // deno.json ("./a.js" -> "./b.js") must rebuild the stack and the new
    // import map must take effect on the next run.
    let dir = temp_dir("fp-invalid");
    fs::write(dir.join("a.js"), "export const marker = 'a';").unwrap();
    fs::write(dir.join("b.js"), "export const marker = 'b';").unwrap();
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "import { marker } from '#mod';\nDeno.writeTextFileSync(new URL('./out.txt', import.meta.url), marker);",
    )
    .unwrap();
    let write_config = |target: &str| {
        // Same-size content edit: the content-hashed fingerprint must catch it
        // even though (mtime, size) would not.
        fs::write(
            dir.join("deno.json"),
            format!("{{\"imports\": {{\"#mod\": \"./{target}.js\"}}}}"),
        )
        .unwrap();
    };
    write_config("a");
    let runtime = build_runtime(&dir);
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "a");
    write_config("b");
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "b");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn run_with_does_not_leak_permissions_between_runs() {
    // Security boundary: run 1's allow-all grants must never leak into run 2
    // with restricted grants on the same LibdenoRuntime — the permission-bound
    // file fetcher / graph loader are rebuilt per run.
    //
    // File imports are read-gated by --allow-read (the graph loader checks
    // each file against the run's container), so run 2's dynamic import of
    // shared.js (outside `--allow-read=<sub>`) is denied at module load —
    // proving run 1's grants did not leak into run 2. The runtime read API
    // that follows is the same check; either denial counts.
    let dir = temp_dir("perm-isolation");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("shared.js"), "export const secret = 'leaked';").unwrap();
    let shared_path = fs::canonicalize(dir.join("shared.js")).unwrap();
    let entry = dir.join("sub").join("main.js");
    fs::write(
        &entry,
        format!(
            "await import('../shared.js');\nDeno.readTextFileSync({:?});",
            shared_path.display().to_string(),
        ),
    )
    .unwrap();
    let runtime = build_runtime(&dir);
    // Run 1: allow-all (allow_all_permissions opt-in) -> the external module
    // loads and reads.
    let code = run_with(
        &runtime,
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0);
    // Run 2: read granted only inside sub/ -> the read of shared.js (outside
    // sub/) must be denied (permission state is rebuilt per run, never leaked).
    let sub = fs::canonicalize(dir.join("sub")).unwrap();
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", sub.display())],
        ..Default::default()
    };
    let err = run_with(&runtime, &entry, &options).unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("Requires read access") || msg.contains("NotCapable"),
        "unexpected error: {msg}"
    );
    let _ = fs::remove_dir_all(&dir);
}
