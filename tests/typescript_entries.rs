//! TypeScript entries: what the transpiler does and does not do.
//!
//! `SourceLang::TypeScript` selects the transpiler only; there is no type
//! checking, matching `deno run`. These tests pin that so a future change to
//! the loader cannot silently start rejecting type-mismatched scripts.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::{LibdenoError, LibdenoOptions, SourceLang};

/// Output capture is process-global, so these tests cannot overlap.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-ts-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    }
}

fn write(dir: &Path, name: &str, code: &str) -> PathBuf {
    let entry = dir.join(name);
    fs::write(&entry, code).unwrap();
    entry
}

#[cfg(not(windows))]
#[test]
fn type_errors_run_and_syntax_errors_do_not() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("types");

    // A type mismatch is stripped, not rejected.
    let entry = write(
        &dir,
        "type.ts",
        "const x: number = 's'; console.log(typeof x);",
    );
    let output = libdeno::run_with_output(&entry, &options()).unwrap();
    assert_eq!(output.exit_code, 0);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line == "string"),
        "expected the transpiled script to print `string`, got: {stdout:?}"
    );

    // A syntax error is a load-time failure.
    let entry = write(&dir, "syntax.ts", "const x: number = ;");
    let error = libdeno::run_with_output(&entry, &options()).unwrap_err();
    assert!(
        matches!(error, LibdenoError::Core(_)),
        "a syntax error should be a Core error, got: {error:?}"
    );
    assert!(error.to_string().contains("SyntaxError"), "got: {error}");
    let _ = fs::remove_dir_all(dir);
}

#[cfg(not(windows))]
#[test]
fn typescript_only_syntax_transpiles() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("syntax");
    for (name, code, expected) in [
        (
            "enum.ts",
            "enum E { A, B } console.log('enum=' + E.B);",
            "enum=1",
        ),
        (
            "decorator.ts",
            "function d(t: any) { return t; } @d class C {} console.log('decorated');",
            "decorated",
        ),
        (
            "generic.ts",
            "function id<T>(v: T): T { return v; } console.log('generic=' + id(7));",
            "generic=7",
        ),
    ] {
        let entry = write(&dir, name, code);
        let output = libdeno::run_with_output(&entry, &options()).unwrap();
        // Capture redirects process-global fds, so the test harness's own
        // progress lines can land in the buffer; match the script's line.
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.lines().any(|line| line == expected),
            "entry {name}: expected `{expected}` in {stdout:?}"
        );
    }
    let _ = fs::remove_dir_all(dir);
}

#[cfg(not(windows))]
#[test]
fn a_js_entry_is_not_transpiled() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("js");
    // The same bytes are valid under `SourceLang::TypeScript` and invalid under
    // `SourceLang::JavaScript`: the lang tag picks the transpiler, and the
    // `.js` extension of a path entry does the same.
    let code = "const x: number = 5; console.log('v=' + x);";
    let entry = write(&dir, "typed.js", code);
    let error = libdeno::run_with_output(&entry, &options()).unwrap_err();
    assert!(
        error.to_string().contains("SyntaxError"),
        "a .js entry must not be transpiled, got: {error}"
    );

    let output = libdeno::run_source_with_output(code, SourceLang::TypeScript, &dir, &options())
        .expect("the same bytes transpile under the TypeScript tag");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.lines().any(|line| line == "v=5"),
        "expected `v=5` in {stdout:?}"
    );

    let error = libdeno::run_source_with_output(code, SourceLang::JavaScript, &dir, &options())
        .unwrap_err();
    assert!(
        error.to_string().contains("SyntaxError"),
        "the JavaScript tag must not transpile, got: {error}"
    );
    let _ = fs::remove_dir_all(dir);
}
