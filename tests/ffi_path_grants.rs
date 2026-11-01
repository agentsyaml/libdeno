//! `--allow-ffi=<path>` follows names, not inodes.
//!
//! A grant on a directory covers a symbolic or hard link inside it, so a
//! script can load a library that physically lives outside the granted root by
//! naming the link. Deno CLI 2.9.8 behaves identically; the point of this test
//! is to keep a host from reading `--allow-ffi=<dir>` as "only libraries in
//! this directory", because loading a library is arbitrary native code in the
//! host process.
//!
//! The test builds its own library files: it copies a system library into the
//! granted directory, then links to the copy from outside.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

/// A real dylib on the host. `/usr/lib/*.dylib` is not usable: those paths live
/// in the dyld shared cache and `dlopen` on a copy of the name fails.
fn system_library() -> Option<PathBuf> {
    let candidates = [
        "/opt/homebrew/Cellar/libuv/1.53.0/lib/libuv.1.0.0.dylib",
        "/opt/homebrew/lib/libuv.1.0.0.dylib",
        "/usr/local/lib/libuv.1.dylib",
    ];
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-ffigrant-{}-{name}", std::process::id()));
    fs::create_dir_all(dir.join("allowed")).unwrap();
    dir
}

/// Runs `script` with `--allow-ffi=<grant>` and returns the first stdout line.
fn run_script(dir: &Path, script: &str, grant: &Path) -> String {
    let entry = dir.join("main.js");
    fs::write(&entry, script).unwrap();
    let options = LibdenoOptions {
        permissions: vec![
            format!("--allow-ffi={}", grant.display()),
            "--allow-read".to_string(),
        ],
        capture_stdout: true,
        ..Default::default()
    };
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    let output =
        libdeno::run_in_subprocess_with_output(&entry, &options).expect("the script must run");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn open_attempt(path: &Path) -> String {
    format!(
        "try {{\n\
         \x20 const lib = Deno.dlopen({path:?}, {{}});\n\
         \x20 lib.close();\n\
         \x20 console.log('OPENED');\n\
         }} catch (error) {{\n\
         \x20 console.log('DENIED: ' + error.message);\n\
         }}\n",
        path = path.to_string_lossy()
    )
}

#[test]
fn a_link_inside_a_granted_directory_loads_a_library_outside_it() {
    let Some(library) = system_library() else {
        eprintln!("skipping: no system dylib found to link against");
        return;
    };
    let dir = temp_dir("links");
    let allowed = dir.join("allowed");

    // The library physically outside the granted root.
    let outside = dir.join("outside.dylib");
    fs::copy(&library, &outside).unwrap();
    // Links inside the granted directory that resolve to it.
    let symlink = allowed.join("via-symlink.dylib");
    let hardlink = allowed.join("via-hardlink.dylib");
    std::os::unix::fs::symlink(&outside, &symlink).unwrap();
    fs::hard_link(&outside, &hardlink).unwrap();

    let direct = run_script(&dir, &open_attempt(&outside), &allowed);
    let through_symlink = run_script(&dir, &open_attempt(&symlink), &allowed);
    let through_hardlink = run_script(&dir, &open_attempt(&hardlink), &allowed);

    let _ = fs::remove_dir_all(&dir);
    assert!(
        direct.starts_with("DENIED"),
        "a path outside the grant must be denied, got: {direct}"
    );
    assert_eq!(
        through_symlink, "OPENED",
        "the grant no longer follows symlinks; if that is deliberate, update \
         SECURITY.md and delete this assertion"
    );
    assert_eq!(
        through_hardlink, "OPENED",
        "the grant no longer follows hard links; if that is deliberate, update \
         SECURITY.md and delete this assertion"
    );
}
