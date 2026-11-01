//! Entry resolution: which inputs resolve, which are `Entry`, which are `Core`.
//!
//! `docs/api.md` draws a specific line: `Entry` covers *resolution* failures
//! (unreadable/malformed `package.json`, a `main` that escapes the package
//! directory), while a path that simply does not exist still resolves to a
//! `file:` URL and fails at module load as `Core`. These tests pin that line —
//! it is the difference between "your argument is wrong" and "your file is
//! missing", and a caller may handle the two differently.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::{LibdenoError, LibdenoOptions};

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-entry-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    }
}

fn write(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
}

#[test]
fn directory_entries_resolve_to_index_js_or_package_main() {
    let dir = temp_dir("dirs");

    // Directory without package.json: index.js.
    let plain = dir.join("plain");
    fs::create_dir_all(&plain).unwrap();
    write(&plain.join("index.js"), "console.log('plain');");
    assert_eq!(libdeno::run(&plain, &options()).unwrap(), 0);

    // package.json `main` wins over index.js.
    let pkg = dir.join("pkg");
    fs::create_dir_all(&pkg).unwrap();
    write(&pkg.join("package.json"), r#"{"main":"start.js"}"#);
    write(&pkg.join("start.js"), "console.log('start');");
    write(
        &pkg.join("index.js"),
        "throw new Error('index.js should not run');",
    );
    assert_eq!(libdeno::run(&pkg, &options()).unwrap(), 0);
    // Naming package.json directly resolves the same way.
    assert_eq!(
        libdeno::run(pkg.join("package.json"), &options()).unwrap(),
        0
    );

    // No `main` falls back to index.js.
    let bare = dir.join("bare");
    fs::create_dir_all(&bare).unwrap();
    write(&bare.join("package.json"), "{}");
    write(&bare.join("index.js"), "console.log('default');");
    assert_eq!(libdeno::run(&bare, &options()).unwrap(), 0);

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_directory_without_an_entry_is_a_module_load_error() {
    let dir = temp_dir("empty-dir");
    let empty = dir.join("empty");
    fs::create_dir_all(&empty).unwrap();
    // Resolution succeeds (index.js is the candidate); loading it fails.
    let error = libdeno::run(&empty, &options()).unwrap_err();
    assert!(
        matches!(error, LibdenoError::Core(_)),
        "a missing index.js is a module-load error, got: {error:?}"
    );
    assert!(error.to_string().contains("Module not found"), "{error}");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_package_main_that_escapes_the_directory_is_an_entry_error() {
    let dir = temp_dir("escape");
    for (name, main) in [("relative", "../outside.js"), ("absolute", "/etc/passwd")] {
        let pkg = dir.join(format!("pkg-{name}"));
        fs::create_dir_all(&pkg).unwrap();
        write(
            &pkg.join("package.json"),
            &format!(r#"{{"main":"{main}"}}"#),
        );
        let error = libdeno::run(&pkg, &options()).unwrap_err();
        assert!(
            matches!(error, LibdenoError::Entry(_)),
            "{name}: an escaping main must be an Entry error, got: {error:?}"
        );
        assert!(
            error.to_string().contains("escapes the package directory"),
            "{name}: got: {error}"
        );
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_malformed_package_json_is_an_entry_error() {
    let dir = temp_dir("malformed");
    let pkg = dir.join("pkg");
    fs::create_dir_all(&pkg).unwrap();
    write(&pkg.join("package.json"), "{ nope }");
    let error = libdeno::run(&pkg, &options()).unwrap_err();
    assert!(
        matches!(error, LibdenoError::Entry(_)),
        "a malformed package.json is an Entry error, got: {error:?}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_missing_entry_file_resolves_and_then_fails_at_load() {
    let dir = temp_dir("missing");
    let error = libdeno::run(dir.join("nope.js"), &options()).unwrap_err();
    assert!(
        matches!(error, LibdenoError::Core(_)),
        "a missing file is a load error, not a resolution error, got: {error:?}"
    );
    assert!(error.to_string().contains("Module not found"), "{error}");
    let _ = fs::remove_dir_all(dir);
}

/// `LibdenoError::Core` carries deno_core's own classification, and a host can
/// branch on it instead of matching message text.
///
/// `docs/api.md` documents this: a thrown script error lands on
/// `CoreErrorKind::Js`, while a missing module, a parse error and an unmapped
/// specifier each land on `JsBox` with a different inner error. This test pins
/// the four apart so a host can rely on the distinction — and so a deno_core
/// upgrade that flattens them shows up here rather than in a host's logs.
#[test]
fn core_errors_are_distinguishable_by_type_not_message() {
    use deno_core::error::CoreErrorKind;

    let dir = temp_dir("core-kinds");
    let options = options();
    let cases = [
        ("threw", "throw new Error('boom');"),
        ("missing", "import './nope.js';"),
        ("syntax", "const = ;"),
        ("unmapped", "import value from 'not-a-real-specifier';"),
    ];
    let mut kinds = Vec::new();
    for (name, source) in cases {
        let entry = dir.join(format!("{name}.js"));
        fs::write(&entry, source).unwrap();
        let error = libdeno::run(&entry, &options).expect_err("every case must fail the run");
        let libdeno::LibdenoError::Core(core) = error else {
            panic!("{name}: expected a Core error, got: {error:?}");
        };
        kinds.push(match core.0.as_ref() {
            CoreErrorKind::Js(_) => "js".to_string(),
            CoreErrorKind::JsBox(inner) => format!("jsbox:{:?}", inner.get_inner_ref().is_some()),
            other => format!("other:{other:?}"),
        });
    }
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        kinds[0], "js",
        "a thrown script error must stay distinguishable from a loader failure"
    );
    assert!(
        kinds[1..].iter().all(|kind| kind.starts_with("jsbox:true")),
        "loader failures must arrive as JsBox with an inner error: {kinds:?}"
    );
}

/// A malformed `package.json` reports the parser's own message.
///
/// `Entry` is the only variant that wraps its payload in a prefix
/// (`failed to resolve entry module: …`), so a host that logs `e.to_string()`
/// gets a self-describing line while a host that matches on the variant gets
/// the inner error. Both are documented; the pairing is what makes
/// `LibdenoError::Entry` distinguishable from `Runtime` at the text level, and
/// no test asserted the wrapper before.
#[test]
fn a_malformed_package_json_keeps_the_parser_message_inside_the_entry_prefix() {
    let dir = temp_dir("malformed-text");
    let pkg = dir.join("pkg");
    fs::create_dir_all(&pkg).unwrap();
    write(&pkg.join("package.json"), "{ nope }");
    let error = libdeno::run(&pkg, &options()).unwrap_err();
    let text = error.to_string();
    let _ = fs::remove_dir_all(dir);
    assert!(
        matches!(error, LibdenoError::Entry(_)),
        "expected Entry, got: {error:?}"
    );
    assert!(
        text.starts_with("failed to resolve entry module: "),
        "the Entry prefix is what separates this variant from `Runtime` at the \
         text level; got: {text}"
    );
    assert!(
        text.contains("key must be a string"),
        "the parser's own message must survive, got: {text}"
    );
}
