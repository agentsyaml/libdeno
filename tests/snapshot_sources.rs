#[path = "../build_support/snapshot_sources.rs"]
mod snapshot_sources;

// Guards the build-script pre-check: if deno_runtime changes how extension
// sources are declared, the enumeration must keep seeing them, otherwise a
// missing source goes back to being an opaque `os error 2`.
#[test]
fn extension_sources_are_enumerable_and_present() {
    let extensions = deno_runtime::snapshot_info::get_extensions_in_snapshot();
    let sources = snapshot_sources::fs_sources(&extensions);
    assert!(
        sources.len() > 200,
        "expected hundreds of filesystem-loaded extension sources, found {}",
        sources.len()
    );
    let missing = snapshot_sources::missing(&extensions);
    assert!(missing.is_empty(), "missing extension sources: {missing:?}");
}
