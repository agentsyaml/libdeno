//! The two process-global environment knobs are read once and then frozen.
//!
//! Each is backed by a `OnceLock`, so a value set after the first run cannot
//! take effect and an unparsable value silently falls back to the default.
//! `docs/api.md` documents this; the test pins it so a future change to either
//! knob's lifetime is visible.

use std::fs;
use std::path::PathBuf;

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-knob-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// An invalid knob value must not turn a normal run into an error.
#[test]
fn an_unparsable_cache_knob_falls_back_to_the_default() {
    let dir = temp_dir("cache");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('ok');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };

    // Whatever this binary already did, the knob is now frozen; the point of
    // the test is that none of these values can break a run.
    for value in ["not-a-number", "0", "-1", "999999999999999999999999"] {
        std::env::set_var("LIBDENO_ANALYSIS_CACHE_ENTRIES", value);
        assert_eq!(
            libdeno::run(&entry, &options).expect("a bad cache knob must not fail the run"),
            0,
            "knob value {value:?}"
        );
    }
    std::env::remove_var("LIBDENO_ANALYSIS_CACHE_ENTRIES");
    let _ = fs::remove_dir_all(dir);
}

/// The tarball threshold only rejects npm downloads, so a bad value must not
/// affect a run that never fetches a tarball.
#[test]
fn an_unparsable_tarball_knob_does_not_affect_local_runs() {
    let dir = temp_dir("tarball");
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log('ok');").unwrap();
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    std::env::set_var("LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES", "not-a-number");
    assert_eq!(libdeno::run(&entry, &options).unwrap(), 0);
    std::env::remove_var("LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES");
    let _ = fs::remove_dir_all(dir);
}
