//! Feature-name sync test: the op-level feature set enabled in lib.rs
//! (`ENABLED_FEATURES`) and the JS-namespace feature registry
//! (`deno_features::UNSTABLE_FEATURES`) must agree on feature names. lib.rs
//! matches them by string (`.contains(&f.name)`), so a typo in either list
//! silently disables an API instead of failing the build.
//!
//! `ENABLED_FEATURES` is private to lib.rs, so this file mirrors it by hand —
//! keep both in sync when features are added or removed.

// Mirror of lib.rs's `ENABLED_FEATURES` const.
const ENABLED_FEATURES: &[&str] = &["kv", "cron", "ffi", "webgpu"];

#[test]
fn every_enabled_feature_is_a_known_unstable_feature() {
    let known: Vec<&str> = deno_features::UNSTABLE_FEATURES
        .iter()
        .map(|f| f.name)
        .collect();
    assert!(!known.is_empty(), "deno_features registry is empty");
    for name in ENABLED_FEATURES {
        assert!(
            known.contains(name),
            "feature {name:?} is enabled in lib.rs but missing from \
             deno_features::UNSTABLE_FEATURES (typo or stale sync)"
        );
    }
}

#[test]
fn every_enabled_feature_is_a_runtime_feature() {
    // The op-level FeatureChecker gates runtime ops, so a name that lives on
    // the CLI side of the registry would never line up with an op.
    for name in ENABLED_FEATURES {
        let definition = deno_features::UNSTABLE_FEATURES
            .iter()
            .find(|f| f.name == *name)
            .unwrap_or_else(|| panic!("feature {name:?} not in UNSTABLE_FEATURES"));
        assert!(
            matches!(definition.kind, deno_features::UnstableFeatureKind::Runtime),
            "feature {name:?} is a CLI-only feature, not a runtime op feature"
        );
    }
}
