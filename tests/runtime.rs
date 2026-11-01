//! Wave 2: the reusable resolver stack — `LibdenoRuntime::new` + `run_with`.
//! Covers stack reuse (identical results across runs), config-change
//! fingerprint invalidation, and the per-run permission isolation guarantee.

use std::fs;
use std::future::Future;
use std::path::PathBuf;

use libdeno::runtime::run_with_source;
use libdeno::{run_with, LibdenoOptions, LibdenoRuntime, SourceLang};

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
fn refresh_rebuilds_after_nested_node_modules_change() {
    let dir = temp_dir("refresh-nested-node-modules");
    let nested_pkg = dir.join("node_modules").join("nested-pkg");
    let nested_dep = nested_pkg.join("node_modules").join("nested-dep");
    fs::create_dir_all(&nested_dep).unwrap();
    fs::write(
        dir.join("package.json"),
        r#"{"name":"refresh-project","private":true}"#,
    )
    .unwrap();
    fs::write(
        nested_pkg.join("package.json"),
        r#"{"name":"nested-pkg","version":"1.0.0","type":"module","main":"index.js"}"#,
    )
    .unwrap();
    fs::write(
        nested_pkg.join("index.js"),
        "import { marker } from 'nested-dep';\nexport { marker };",
    )
    .unwrap();
    fs::write(
        nested_dep.join("package.json"),
        r#"{"name":"nested-dep","version":"1.0.0","type":"module","main":"a.js"}"#,
    )
    .unwrap();
    fs::write(nested_dep.join("a.js"), "export const marker = 'a';").unwrap();
    fs::write(nested_dep.join("b.js"), "export const marker = 'b';").unwrap();
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "import { marker } from './node_modules/nested-pkg/index.js';\nDeno.writeTextFileSync(new URL('./out.txt', import.meta.url), marker);",
    )
    .unwrap();

    let runtime = build_runtime(&dir);
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "a");

    // A nested package's `package.json` edit is picked up without `refresh()`:
    // the probe covers the `node_modules` directories above the workspace, and
    // each run re-reads the module contents. This assertion used to sit behind
    // a `refresh()` call, which hid whether the probe actually covered the
    // case — it did, so the call proved nothing.
    fs::write(
        nested_dep.join("package.json"),
        r#"{"name":"nested-dep","version":"1.0.0","type":"module","main":"b.js"}"#,
    )
    .unwrap();
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(
        fs::read_to_string(dir.join("out.txt")).unwrap(),
        "b",
        "a nested package.json edit must be visible without an explicit refresh"
    );

    // `refresh()` still forces a rebuild, which is what it is for: changes the
    // probe cannot see. Prove it rebuilds by mutating state the probe does not
    // read — the module file's contents are re-read anyway, so the observable
    // is that the run still succeeds and observes the edit.
    fs::write(nested_dep.join("b.js"), "export const marker = 'c';").unwrap();
    runtime.refresh();
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "c");
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
fn run_with_rejects_capture_flags() {
    // run_with returns only the exit code — capture flags were a silent
    // no-op (and bought exclusivity rejections on busy hosts for nothing);
    // they must be rejected like every other unusable option.
    let dir = temp_dir("reject-cap");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('never captures');").unwrap();
    let runtime = build_runtime(&dir);
    let err = run_with(
        &runtime,
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            capture_stdout: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, libdeno::LibdenoError::Configuration(_)),
        "expected a configuration error, got: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn run_with_rejects_mismatched_cwd() {
    // The resolver stack is scoped to the runtime's directory; a cwd that
    // resolves elsewhere would silently run against a different base.
    let dir = temp_dir("reject-cwd");
    let other = temp_dir("reject-cwd-other");
    let entry = dir.join("main.js");
    fs::write(&entry, "1 + 1;").unwrap();
    let runtime = build_runtime(&dir);
    let err = run_with(
        &runtime,
        &entry,
        &LibdenoOptions {
            cwd: Some(other.clone()),
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(
        matches!(err, libdeno::LibdenoError::Configuration(_)),
        "expected a configuration error, got: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&other);
}

#[test]
fn run_with_accepts_matching_cwd() {
    // A cwd that resolves to the runtime's directory (same dir, any path
    // form) is fine.
    let dir = temp_dir("ok-cwd");
    let entry = dir.join("main.js");
    fs::write(&entry, "1 + 1;").unwrap();
    let runtime = build_runtime(&dir);
    let code = run_with(
        &runtime,
        &entry,
        &LibdenoOptions {
            cwd: Some(dir.clone()),
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0);
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

#[test]
fn reusable_async_run_executes_twice_on_same_runtime() {
    let dir = temp_dir("async-reuse-basic");
    let entry = dir.join("main.js");
    fs::write(&entry, "1 + 1;").unwrap();
    let runtime = build_runtime(&dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let codes = rt.block_on(async {
        let first = runtime.run_async(&entry, &options).await.unwrap();
        let second = runtime.run_async(&entry, &options).await.unwrap();
        (first, second)
    });
    assert_eq!(codes, (0, 0));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reusable_async_run_does_not_leak_permissions_between_runs() {
    let dir = temp_dir("async-perm-isolation");
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("shared.js"), "export const secret = 'leaked';").unwrap();
    let entry = dir.join("sub").join("main.js");
    let shared_path = fs::canonicalize(dir.join("shared.js")).unwrap();
    fs::write(
        &entry,
        format!(
            "await import('../shared.js');\nDeno.readTextFileSync({:?});",
            shared_path.display().to_string(),
        ),
    )
    .unwrap();
    let runtime = build_runtime(&dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let allow_all = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let sub = fs::canonicalize(dir.join("sub")).unwrap();
    let restricted = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", sub.display())],
        ..Default::default()
    };
    let (first, second) = rt.block_on(async {
        let first = runtime.run_async(&entry, &allow_all).await;
        let second = runtime.run_async(&entry, &restricted).await;
        (first, second)
    });
    assert_eq!(first.unwrap(), 0);
    let error = second.unwrap_err().to_string();
    assert!(
        error.contains("Requires read access") || error.contains("NotCapable"),
        "unexpected error: {error}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reusable_async_run_rebuilds_when_config_changes() {
    let dir = temp_dir("async-fp-invalid");
    fs::write(dir.join("a.js"), "export const marker = 'a';").unwrap();
    fs::write(dir.join("b.js"), "export const marker = 'b';").unwrap();
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "import { marker } from '#mod';\nDeno.writeTextFileSync(new URL('./out.txt', import.meta.url), marker);",
    )
    .unwrap();
    let write_config = |target: &str| {
        fs::write(
            dir.join("deno.json"),
            format!("{{\"imports\": {{\"#mod\": \"./{target}.js\"}}}}"),
        )
        .unwrap();
    };
    write_config("a");
    let runtime = build_runtime(&dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(rt.block_on(runtime.run_async(&entry, &options)).unwrap(), 0);
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "a");
    write_config("b");
    assert_eq!(rt.block_on(runtime.run_async(&entry, &options)).unwrap(), 0);
    assert_eq!(fs::read_to_string(dir.join("out.txt")).unwrap(), "b");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reusable_runtime_runs_in_memory_source() {
    // run_with_source: in-memory sources through the reusable stack — the exit
    // code comes back, and a relative import resolves through the shared stack.
    let dir = temp_dir("run-with-source");
    fs::write(dir.join("sibling.js"), "export const v = 7;").unwrap();
    let runtime = build_runtime(&dir);
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let code = run_with_source(
        &runtime,
        "import { v } from './sibling.js';\nif (v !== 7) Deno.exit(3);",
        SourceLang::JavaScript,
        &dir,
        &options,
    )
    .unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reusable_runtime_memory_entry_in_manual_node_modules_is_synthetic() {
    let dir = temp_dir("source-manual-node-modules");
    fs::write(dir.join("deno.json"), r#"{"nodeModulesDir":"manual"}"#).unwrap();
    let package_dir = dir.join("node_modules/pkg");
    fs::create_dir_all(&package_dir).unwrap();
    fs::write(package_dir.join("package.json"), r#"{"name":"pkg"}"#).unwrap();
    let result_file = dir.join("result.txt");
    let result_path = result_file.display().to_string();
    let runtime = build_runtime(&dir);
    let code = run_with_source(
        &runtime,
        format!("Deno.writeTextFileSync({result_path:?}, 'executed');"),
        SourceLang::JavaScript,
        &package_dir,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0);
    assert_eq!(fs::read_to_string(result_file).unwrap(), "executed");
    assert!(fs::read_dir(&package_dir).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("$deno$")
    }));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_in_memory_runs_on_one_runtime_succeed() {
    // Two concurrent in-memory runs on one runtime each get their own
    // per-run `memory_files` map, so both must complete with exit code 0.
    let dir = temp_dir("concurrent-source");
    let runtime = build_runtime(&dir);
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let codes = std::thread::scope(|s| {
        let h1 = s.spawn(|| {
            run_with_source(
                &runtime,
                "console.log('concurrent a');",
                SourceLang::JavaScript,
                &dir,
                &options,
            )
        });
        let h2 = s.spawn(|| {
            run_with_source(
                &runtime,
                "console.log('concurrent b');",
                SourceLang::JavaScript,
                &dir,
                &options,
            )
        });
        (h1.join().unwrap(), h2.join().unwrap())
    });
    assert_eq!(codes.0.unwrap(), 0);
    assert_eq!(codes.1.unwrap(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reusable_async_run_rejects_mismatched_cwd() {
    let dir = temp_dir("async-reject-cwd");
    let other = temp_dir("async-reject-cwd-other");
    let entry = dir.join("main.js");
    fs::write(&entry, "1 + 1;").unwrap();
    let runtime = build_runtime(&dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = rt
        .block_on(runtime.run_async(
            &entry,
            &LibdenoOptions {
                cwd: Some(other.clone()),
                allow_all_permissions: true,
                ..Default::default()
            },
        ))
        .unwrap_err();
    assert!(matches!(error, libdeno::LibdenoError::Configuration(_)));
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::remove_dir_all(&other);
}

#[test]
fn reusable_async_interleave_is_rejected_and_guard_releases_on_drop() {
    let dir = temp_dir("async-reuse-interleave");
    let entry = dir.join("main.js");
    fs::write(&entry, "await new Promise(r => setTimeout(r, 5000));").unwrap();
    let runtime = build_runtime(&dir);
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = rt.block_on(async {
        let mut future = Box::pin(runtime.run_async(&entry, &options));
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        assert!(future.as_mut().poll(&mut context).is_pending());
        let error = runtime.run_async(&entry, &options).await.unwrap_err();
        drop(future);
        error
    });
    assert!(matches!(error, libdeno::LibdenoError::Configuration(_)));
    assert_eq!(rt.block_on(runtime.run_async(&entry, &options)).unwrap(), 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reusable_async_execution_deadline_still_applies() {
    let dir = temp_dir("async-deadline");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "await new Promise((resolve) => setTimeout(resolve, 60_000));",
    )
    .unwrap();
    let runtime = build_runtime(&dir);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = rt
        .block_on(runtime.run_async(
            &entry,
            &LibdenoOptions {
                allow_all_permissions: true,
                execution_deadline: Some(std::time::Duration::from_millis(100)),
                ..Default::default()
            },
        ))
        .unwrap_err();
    assert!(
        matches!(error, libdeno::LibdenoError::Timeout(_)),
        "expected Timeout, got: {error}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// The permission-isolation guarantee in both directions and across
/// `refresh()`.
///
/// The existing tests only check that a wide grant does not leak into a
/// following narrow run. The opposite direction matters too: a narrow run must
/// not leave the resolver stack in a state where the next wide grant is
/// ignored, and `refresh()` (which rebuilds the stack) must not carry either
/// run's permissions forward.
#[test]
fn permissions_do_not_leak_in_either_direction_across_refresh() {
    let root = temp_dir("perm-both-ways");
    let scripts = root.join("scripts");
    let secret_dir = root.join("secret");
    let out = root.join("out");
    for dir in [&scripts, &secret_dir, &out] {
        fs::create_dir_all(dir).unwrap();
    }
    let secret = secret_dir.join("secret.txt");
    fs::write(&secret, "SECRET").unwrap();
    let entry = scripts.join("main.js");
    fs::write(
        &entry,
        "const [secret, marker] = Deno.args;\n\
         let result;\n\
         try { result = 'READ ' + Deno.readTextFileSync(secret); }\n\
         catch (e) { result = 'DENIED ' + e.constructor.name; }\n\
         Deno.writeTextFileSync(marker, result);\n",
    )
    .unwrap();
    let marker = out.join("marker.txt");

    let runtime = build_runtime(&root);
    let attempt = |grant_secret: bool| -> String {
        let _ = fs::remove_file(&marker);
        let mut permissions = vec![
            format!("--allow-read={}", scripts.display()),
            format!("--allow-write={}", out.display()),
        ];
        if grant_secret {
            permissions.push(format!("--allow-read={}", secret_dir.display()));
        }
        let options = LibdenoOptions {
            permissions,
            args: vec![
                secret.to_string_lossy().to_string(),
                marker.to_string_lossy().to_string(),
            ],
            ..Default::default()
        };
        run_with(&runtime, &entry, &options).unwrap();
        fs::read_to_string(&marker)
            .unwrap_or_default()
            .trim()
            .to_string()
    };

    assert_eq!(attempt(true), "READ SECRET", "wide run");
    assert_eq!(attempt(false), "DENIED NotCapable", "narrow run");
    assert_eq!(attempt(true), "READ SECRET", "wide run after narrow");
    runtime.refresh();
    assert_eq!(
        attempt(false),
        "DENIED NotCapable",
        "narrow run after refresh"
    );
    assert_eq!(attempt(true), "READ SECRET", "wide run after refresh");
    let _ = fs::remove_dir_all(&root);
}

/// The probe covers more than the docs used to claim, and a test that calls
/// `refresh()` cannot tell the difference.
///
/// Mutates resolver input and asserts the next run on the same runtime observes
/// it **without** `refresh()`. If a future change narrows the probe, this
/// fails — which is the point: the previous version of
/// `refresh_rebuilds_after_nested_node_modules_change` called `refresh()`, so it
/// passed whether or not the probe covered the case.
#[test]
fn resolver_probe_sees_changes_without_an_explicit_refresh() {
    let dir = temp_dir("probe-covers");
    fs::write(dir.join("a.js"), "export const v = 'A';").unwrap();
    fs::write(dir.join("b.js"), "export const v = 'B';").unwrap();
    let marker = dir.join("out.txt");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "import { v } from 'alias';\n\
         Deno.writeTextFileSync(new URL('./out.txt', import.meta.url), v);\n",
    )
    .unwrap();
    let runtime = build_runtime(&dir);
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };

    // Import-map edit with the same byte length ("./a.js" -> "./b.js"): a
    // length-based fingerprint would miss it, so this pins content hashing.
    fs::write(dir.join("deno.json"), r#"{"imports":{"alias":"./a.js"}}"#).unwrap();
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(fs::read_to_string(&marker).unwrap(), "A");

    fs::write(dir.join("deno.json"), r#"{"imports":{"alias":"./b.js"}}"#).unwrap();
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        "B",
        "an import-map edit must be visible without refresh"
    );

    // Module contents are not part of the resolver probe, but each run gets a
    // fresh module graph, so an edit two levels inside a package is re-read
    // anyway. `docs/api.md` states this; without the assertion a future
    // module-graph cache would make the doc wrong silently.
    let package = dir.join("node_modules/deep-pkg");
    fs::create_dir_all(package.join("lib")).unwrap();
    fs::write(
        package.join("package.json"),
        r#"{"name":"deep-pkg","version":"1.0.0","type":"module","exports":"./index.js"}"#,
    )
    .unwrap();
    fs::write(
        package.join("index.js"),
        "export { value } from './lib/deep.js';\n",
    )
    .unwrap();
    fs::write(package.join("lib/deep.js"), "export const value = 'V1';\n").unwrap();
    fs::write(dir.join("deno.json"), r#"{"nodeModulesDir":"manual"}"#).unwrap();
    fs::write(
        dir.join("package.json"),
        r#"{"dependencies":{"deep-pkg":"1.0.0"}}"#,
    )
    .unwrap();
    fs::write(
        &entry,
        "import { value } from 'deep-pkg';\n\
         Deno.writeTextFileSync(new URL('./out.txt', import.meta.url), value);\n",
    )
    .unwrap();
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(fs::read_to_string(&marker).unwrap(), "V1");

    fs::write(package.join("lib/deep.js"), "export const value = 'V2';\n").unwrap();
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        "V2",
        "an edit inside a dependency must be re-read without refresh"
    );

    let _ = fs::remove_dir_all(&dir);
}

/// `LibdenoRuntime::new` tolerates a directory that does not exist yet.
///
/// `LibdenoOptions.cwd` takes the opposite stance for the same class of
/// mistake: a missing path is a `Configuration` error, so a host cannot
/// silently run against the host cwd while its grants point somewhere else. The
/// reusable runtime falls back to the raw path instead, which makes `new(dir)`
/// usable before `create_dir_all(dir)` — a real convenience for a host that
/// prepares a workspace lazily — but it also means a typo in that path is not
/// reported. `run_with` then fails later, at module load, on a path that
/// mentions the missing directory.
///
/// The two behaviours are documented; this pins the asymmetry so a change to
/// either side has to be deliberate.
#[test]
fn reusable_runtime_accepts_a_directory_that_does_not_exist_yet() {
    let dir = temp_dir("new-missing");
    let _ = fs::remove_dir_all(&dir);
    let runtime = build_runtime(&dir);
    // Creating it afterwards is the supported order.
    fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('ran');").unwrap();
    let code = run_with(
        &runtime,
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            cwd: Some(dir.clone()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(code, 0);
    let _ = fs::remove_dir_all(&dir);
}

/// `run_with_source` rejects capture flags, exactly like `run_with`.
///
/// Both go through `check_source_preconditions` → `reject_capture_flags`, but
/// only the path-based `run_with` had a test. The in-memory variant takes a
/// different branch (`capture_flags_allowed: false`), so a change to that
/// argument would leave `run_with` green while the in-memory entry silently
/// accepted capture flags it can never honor — and paid the exclusivity
/// rejection on a busy host for nothing.
///
/// The guard is applied twice (once here, once in `run_with_exit_code`), so
/// removing either one alone keeps this test green. That redundancy is
/// deliberate — the source says it is "idempotent and cheap" — and the mutation
/// that matters is removing both.
#[test]
fn run_with_source_rejects_capture_flags() {
    let dir = temp_dir("reject-src-cap");
    let runtime = build_runtime(&dir);
    let error = run_with_source(
        &runtime,
        "console.log('never captures');",
        SourceLang::JavaScript,
        &dir,
        &LibdenoOptions {
            allow_all_permissions: true,
            capture_stdout: true,
            ..Default::default()
        },
    )
    .unwrap_err();
    let _ = fs::remove_dir_all(&dir);
    assert!(
        matches!(error, libdeno::LibdenoError::Configuration(_)),
        "expected a configuration error, got: {error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("run_with does not support output capture"),
        "expected the documented message, got: {error}"
    );
}
