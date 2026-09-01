//! Native addon (.node) loading E2E.
//!
//! Regression guard for the "native modules cannot load" report: libdeno
//! builds the runtime with `deno_rt_native_addon_loader: None`. Upstream
//! semantics (denort_helper) are that this loader only materializes
//! VFS-embedded addons for `deno compile`; with None the on-disk path is
//! dlopen'd directly, exactly like `deno run`. So require() of a plain
//! .node addon must work, gated by --allow-ffi.
//!
//! Windows skip (how to avoid Windows CI failures here):
//! - The fixture addon is compiled at test runtime with the cc-discovered C
//!   toolchain. On unix that is clang/gcc, which builds a shared library with
//!   plain `-shared -fPIC` and no environment prerequisites.
//! - On Windows the MSVC cl.exe needs the vcvars environment (INCLUDE/LIB)
//!   injected before it can compile AND link; a bare GitHub Actions runner
//!   does not provide it, so `cl /LD` fails (link errors surface on stdout,
//!   not stderr — see the assert below printing both). cc's compiler.env()
//!   does not reliably carry the vcvars values.
//! - Workaround: skip the whole file on Windows (`#![cfg(not(windows))]`).
//!   The load path under test (op_napi_open: ffi check, dlopen, symbol
//!   resolution) is platform-independent and covered on unix; if Windows
//!   coverage is ever needed, drive clang-cl instead (PATH-provided on the
//!   runner) or ship a prebuilt fixture .node per target.
#![cfg(not(windows))]

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use libdeno::{run, run_with_output, LibdenoOptions};

static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
const FINALIZER_MARKER_ENV: &str = "LIBDENO_NATIVE_ADDON_FINALIZER_MARKER";

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
        // macOS's two-level namespace requires undefined napi_* symbols
        // (resolved from the host at dlopen time) to be allowed at link time,
        // exactly like real node-gyp addons build.
        #[cfg(target_os = "macos")]
        let cc_args = vec!["-shared", "-fPIC", "-undefined", "dynamic_lookup"];
        #[cfg(not(target_os = "macos"))]
        let cc_args = vec!["-shared", "-fPIC"];
        cmd.args(cc_args).arg(&src).arg("-o").arg(&addon);
    }
    let out = cmd.output().expect("failed to spawn C compiler");
    assert!(
        out.status.success(),
        "compiling the fixture addon failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
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

fn assert_one_finalizer_marker(path: &Path) {
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "native addon finalizer\n"
    );
}

#[test]
fn node_addon_requires_and_loads() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Canonicalize the dir: permission checks use canonical paths (read via
    // the require loader, ffi via op_napi_open), so grant the canonical form
    // to avoid macOS /var -> /private/var mismatches.
    let dir = fs::canonicalize(temp_dir("ok")).unwrap();
    let addon = build_addon(&dir);
    let entry = dir.join("main.cjs");
    fs::write(
        &entry,
        "const m = require('./addon.node');\n\
         if (typeof m.add !== 'function') throw new Error('add not exported');\n\
         if (m.add(2, 3) !== 5) throw new Error('add(2, 3) !== 5');\n\
         globalThis.addEventListener('unload', () => console.error('native unload'));\n\
         process.on('exit', () => console.error('native process exit'));\n\
         console.log('addon works');",
    )
    .unwrap();
    let options = LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", dir.display()),
            // op_napi_open checks the ffi permission on the addon path.
            format!("--allow-ffi={}", dir.display()),
        ],
        capture_stderr: true,
        ..Default::default()
    };
    let output = run_with_output(&entry, &options).unwrap();
    assert_eq!(output.exit_code, 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    for marker in [
        "native unload\n",
        "native process exit\n",
        "native addon finalizer\n",
    ] {
        assert_eq!(
            stderr.matches(marker).count(),
            1,
            "expected exactly one {marker:?}: {stderr:?}"
        );
    }
    let unload = stderr
        .find("native unload\n")
        .expect("unload listener did not run");
    let process_exit = stderr
        .find("native process exit\n")
        .expect("process exit listener did not run");
    let finalizer = stderr
        .find("native addon finalizer\n")
        .expect("native addon finalizer did not run");
    assert!(
        unload < process_exit && process_exit < finalizer,
        "shutdown ordering must be unload -> process exit -> finalizer: {stderr:?}"
    );
    let _ = addon;
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn node_addon_without_ffi_permission_is_rejected() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

#[test]
fn node_addon_finalizer_runs_after_runtime_error() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = fs::canonicalize(temp_dir("runtime-error")).unwrap();
    build_addon(&dir);
    let marker = dir.join("finalizer.marker");
    std::env::set_var(FINALIZER_MARKER_ENV, &marker);
    let entry = dir.join("main.cjs");
    fs::write(
        &entry,
        "require('./addon.node'); throw new Error('native addon runtime failure');",
    )
    .unwrap();
    let options = LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", dir.display()),
            format!("--allow-ffi={}", dir.display()),
        ],
        ..Default::default()
    };
    let error = run(&entry, &options).unwrap_err();
    std::env::remove_var(FINALIZER_MARKER_ENV);
    assert!(
        error.to_string().contains("native addon runtime failure"),
        "expected original runtime error, got: {error}"
    );
    assert_one_finalizer_marker(&marker);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn node_addon_finalizer_runs_after_execution_deadline() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = fs::canonicalize(temp_dir("deadline")).unwrap();
    build_addon(&dir);
    let marker = dir.join("finalizer.marker");
    std::env::set_var(FINALIZER_MARKER_ENV, &marker);
    let entry = dir.join("main.cjs");
    fs::write(&entry, "require('./addon.node'); while (true) {}\n").unwrap();
    let options = LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", dir.display()),
            format!("--allow-ffi={}", dir.display()),
        ],
        execution_deadline: Some(std::time::Duration::from_millis(200)),
        ..Default::default()
    };
    let error = run(&entry, &options).unwrap_err();
    std::env::remove_var(FINALIZER_MARKER_ENV);
    assert!(
        matches!(error, libdeno::LibdenoError::Timeout(_)),
        "expected Timeout, got: {error}"
    );
    assert_one_finalizer_marker(&marker);
    let _ = fs::remove_dir_all(&dir);
}
