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
