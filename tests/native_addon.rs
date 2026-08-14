//! Native addon (.node) loading E2E.
//!
//! Regression guard for the "native modules cannot load" report: libdeno
//! builds the runtime with `deno_rt_native_addon_loader: None`. Upstream
//! semantics (denort_helper) are that this loader only materializes
//! VFS-embedded addons for `deno compile`; with None the on-disk path is
//! dlopen'd directly, exactly like `deno run`. So require() of a plain
//! .node addon must work, gated by --allow-ffi.

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use libdeno::{run, LibdenoOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-addon-{}-{}", std::process::id(), name));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Compiles the minimal fixture addon and returns the path of the .node file.
fn build_addon(dir: &Path) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/minimal_addon.c");
    // cc reads TARGET/OPT_LEVEL/etc from the environment; cargo only injects
    // them while compiling, tests run without them, so set them (constant
    // values, safe under parallel tests).
    let target = target_triple();
    std::env::set_var("TARGET", &target);
    std::env::set_var("HOST", &target);
    std::env::set_var("OPT_LEVEL", "0");
    std::env::set_var("DEBUG", "0");
    // Modern cc only builds static archives, so reuse its compiler discovery
    // (clang/gcc on unix, MSVC toolchain env on Windows) and drive the shared
    // build ourselves.
    let mut build = cc::Build::new();
    build.target(&target).cargo_output(false);
    let compiler = build.get_compiler();
    let addon = dir.join("addon.node");
    let mut cmd = std::process::Command::new(compiler.path());
    let envs: Vec<(std::ffi::OsString, std::ffi::OsString)> = compiler.env().to_vec();
    cmd.envs(envs);
    if cfg!(windows) {
        // cl /LD produces a DLL; /Fe names it (a .lib/.exp pair is emitted
        // alongside, harmless).
        cmd.args(["/LD", "/nologo"])
            .arg(&src)
            .arg(format!("/Fe:{}", addon.display()));
    } else {
        cmd.args(["-shared", "-fPIC"])
            .arg(&src)
            .arg("-o")
            .arg(&addon);
    }
    let out = cmd.output().expect("failed to spawn C compiler");
    assert!(
        out.status.success(),
        "compiling the fixture addon failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    addon
}

fn target_triple() -> String {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        ("x86_64", "windows") => "x86_64-pc-windows-msvc",
        ("aarch64", "windows") => "aarch64-pc-windows-msvc",
        (arch, os) => panic!("unsupported test host: {arch}-{os}"),
    }
    .to_string()
}

#[test]
fn node_addon_requires_and_loads() {
    // Canonicalize the dir: permission checks use canonical paths (read via
    // the require loader, ffi via op_napi_open), so grant the canonical form
    // to avoid macOS /var -> /private/var mismatches.
    let dir = fs::canonicalize(temp_dir("ok")).unwrap();
    let addon = build_addon(&dir);
    let entry = dir.join("main.cjs");
    fs::write(
        &entry,
        "const m = require('./addon.node');\n\
         if (typeof m !== 'object' || m === null) throw new Error('bad exports');\n\
         console.log('addon loaded');",
    )
    .unwrap();
    let options = LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", dir.display()),
            // op_napi_open checks the ffi permission on the addon path.
            format!("--allow-ffi={}", dir.display()),
        ],
        ..Default::default()
    };
    let code = run(&entry, &options).unwrap();
    assert_eq!(code, 0);
    let _ = addon;
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn node_addon_without_ffi_permission_is_rejected() {
    // Fail-closed: with read but no ffi grant, loading the addon must error.
    let dir = fs::canonicalize(temp_dir("noperm")).unwrap();
    build_addon(&dir);
    let entry = dir.join("main.cjs");
    fs::write(&entry, "require('./addon.node');").unwrap();
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    let err = run(&entry, &options).unwrap_err();
    assert!(
        err.to_string().contains("ffi access"),
        "unexpected error: {err}"
    );
    let _ = fs::remove_dir_all(&dir);
}
