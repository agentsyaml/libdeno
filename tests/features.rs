//! Runtime feature behavior tests. The default feature names live only in
//! lib.rs; these tests exercise the public behavior instead of mirroring them.

use std::fs;
use std::path::PathBuf;

use libdeno::{run, LibdenoOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-feature-{}-{}", std::process::id(), name));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn default_runtime_features_are_enabled() {
    let dir = temp_dir("default");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "if (typeof Deno.openKv !== 'function') throw new Error('default feature missing');",
    )
    .unwrap();
    run(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn custom_features_accept_duplicates_and_order_changes() {
    let dir = temp_dir("duplicates");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        "if (typeof Deno.openKv !== 'function') throw new Error('kv missing');",
    )
    .unwrap();

    for features in [
        vec!["ffi", "kv", "ffi", "kv"],
        vec!["kv", "ffi", "kv", "ffi"],
    ] {
        let options = LibdenoOptions {
            features: Some(features.into_iter().map(String::from).collect()),
            allow_all_permissions: true,
            ..Default::default()
        };
        run(&entry, &options).unwrap();
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn large_feature_combinations_do_not_accumulate_host_strings() {
    let dir = temp_dir("large");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('feature list accepted');").unwrap();
    let known: Vec<&str> = deno_features::UNSTABLE_FEATURES
        .iter()
        .filter(|feature| matches!(feature.kind, deno_features::UnstableFeatureKind::Runtime))
        .map(|feature| feature.name)
        .collect();
    assert!(!known.is_empty());
    let features = (0..4096)
        .map(|index| known[index % known.len()].to_string())
        .collect();
    run(
        &entry,
        &LibdenoOptions {
            features: Some(features),
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    let _ = fs::remove_dir_all(&dir);
}

/// `worker-options` is force-enabled even when the host passes a set that
/// omits it: `op_create_worker` exits the process when the feature is off, so
/// a host shrinking the surface for untrusted plugins would otherwise hand
/// them a way to kill the host.
#[test]
fn a_minimal_feature_set_cannot_kill_the_host_with_a_worker() {
    let dir = temp_dir("worker-options");
    let entry = dir.join("main.js");
    // No capture: in-process capture is process-global and this binary's other
    // tests run in parallel. The script writes a marker file instead.
    let marker = dir.join("survived");
    // The gate is consulted by `Worker.deno.permissions` (the nested `deno`
    // options object), not by a plain worker: without one of those options the
    // check is never reached, and the test would pass even with the
    // force-enable removed.
    let script = format!(
        "const w = new Worker(import.meta.url, {{ type: 'module', deno: {{ permissions: 'inherit' }} }});\n\
         w.terminate();\n\
         Deno.writeTextFileSync({:?}, 'SURVIVED');\n",
        marker.to_string_lossy()
    );
    fs::write(&entry, script).unwrap();
    let exit_code = run(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            features: Some(vec!["webgpu".to_string()]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(exit_code, 0);
    assert_eq!(fs::read_to_string(&marker).unwrap(), "SURVIVED");
    let _ = fs::remove_dir_all(dir);
}

/// `ffi` and `webgpu` are stable APIs in Deno 2.9.x: `FeatureChecker` has no
/// check point for either, so dropping the names from a custom set does not
/// remove `Deno.dlopen` or `navigator.gpu`. A host that shrinks `features` for
/// untrusted plugins must not read that as an FFI control — the permission is.
///
/// Both halves matter. The first says the feature list is not a boundary; the
/// second says the boundary that does exist still holds under an empty set.
#[test]
fn shrinking_features_does_not_remove_stable_ffi_or_webgpu() {
    let dir = temp_dir("stable-ffi");
    let entry = dir.join("main.js");
    // Marker files instead of capture: this binary's tests run in parallel and
    // in-process capture is process-global.
    let opened = dir.join("opened");
    let denied = dir.join("denied");
    fs::write(
        &entry,
        format!(
            "if (typeof navigator.gpu === 'undefined') throw new Error('webgpu gone');\n\
             if (typeof Deno.dlopen !== 'function') throw new Error('ffi gone');\n\
             Deno.writeTextFileSync({opened:?}, 'PRESENT');\n\
             try {{\n\
             \x20 Deno.dlopen('/usr/lib/libSystem.B.dylib', {{}});\n\
             }} catch (error) {{\n\
             \x20 Deno.writeTextFileSync({denied:?}, String(error.message));\n\
             }}\n",
            opened = opened.to_string_lossy(),
            denied = denied.to_string_lossy()
        ),
    )
    .unwrap();

    // Read/write for the markers, no `--allow-ffi`.
    let exit_code = run(
        &entry,
        &LibdenoOptions {
            permissions: vec![
                format!("--allow-read={}", dir.display()),
                format!("--allow-write={}", dir.display()),
            ],
            features: Some(vec![]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(exit_code, 0);
    assert_eq!(fs::read_to_string(&opened).unwrap(), "PRESENT");
    let message = fs::read_to_string(&denied).unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert!(
        message.contains("Requires ffi access"),
        "an empty feature set must not grant FFI; dlopen reported: {message}"
    );
}

/// `net` and `raw-imports` gate their namespaces but are not in the default
/// set, so they only matter when a host adds them.
///
/// `docs/api.md` documents this in the "Effect of omitting it" table. Without a
/// test, a future change to `DEFAULT_RUNTIME_FEATURES` that added `net` would
/// silently widen the default surface — `WebSocketStream` and `WebTransport`
/// would start existing for every embedder.
#[test]
fn net_and_raw_imports_gate_only_when_added() {
    let dir = temp_dir("net-raw");
    let marker = dir.join("out.txt");
    let probe = format!(
        "Deno.writeTextFileSync({marker:?}, [\n\
         \x20 typeof WebSocketStream,\n\
         \x20 typeof CSSStyleSheet,\n\
         ].join(','));\n",
        marker = marker.to_string_lossy()
    );
    let entry = dir.join("main.js");
    let run_with = |features: Option<Vec<String>>| -> String {
        fs::write(&entry, &probe).unwrap();
        run(
            &entry,
            &LibdenoOptions {
                features,
                allow_all_permissions: true,
                ..Default::default()
            },
        )
        .expect("the run must succeed");
        fs::read_to_string(&marker).unwrap()
    };

    // The default set omits both, so neither namespace exists.
    let defaults = run_with(None);
    // Adding them is what makes them appear.
    let added = run_with(Some(vec!["net".into(), "raw-imports".into()]));
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        defaults, "undefined,undefined",
        "net and raw-imports must not be enabled by default"
    );
    assert_eq!(
        added, "function,function",
        "adding net and raw-imports must expose their namespaces"
    );
}

/// An unknown feature name is rejected on both entry points, with the same text.
///
/// The subprocess side validates before spawning (`validate_child_options`)
/// and has a test; the in-process path validates inside
/// `build_main_worker`. If only the subprocess check existed, an in-process run
/// would still fail — but later, from a different layer, and a host that
/// branches on `Configuration` versus `Runtime` would take a different path.
#[test]
fn an_unknown_feature_name_is_a_configuration_error_in_process() {
    let dir = temp_dir("unknown-feature");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('never');").unwrap();
    let error = run(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            features: Some(vec!["not-a-feature".to_string()]),
            ..Default::default()
        },
    )
    .unwrap_err();
    let _ = fs::remove_dir_all(dir);
    assert!(
        matches!(error, libdeno::LibdenoError::Configuration(_)),
        "expected Configuration, got: {error:?}"
    );
    assert!(
        error
            .to_string()
            .contains("unknown runtime feature: not-a-feature"),
        "expected the feature name in the message, got: {error}"
    );
}
