#[path = "../build_support/snapshot_bundle.rs"]
mod snapshot_bundle;

#[test]
fn bundle_identity_rejects_a_different_v8_version() {
    let identity = snapshot_bundle::Identity {
        libdeno: "0.3.7".into(),
        target: "x86_64-apple-darwin".into(),
        v8: "15.0.245.2-rusty".into(),
        typescript: "6.0.3".into(),
        deno_core: "0.412.0".into(),
        deno_runtime: "0.267.0".into(),
    };
    let manifest = snapshot_bundle::describe(&identity, b"snapshot", b"table");
    let mut different = identity;
    different.v8 = "different-v8".into();
    assert!(
        snapshot_bundle::verify(&manifest, &different, b"snapshot", b"table")
            .unwrap_err()
            .contains("v8")
    );
}
