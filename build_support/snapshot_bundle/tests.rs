use super::*;

fn identity() -> Identity {
    Identity {
        libdeno: "0.3.6".into(),
        target: "aarch64-apple-darwin".into(),
        v8: "14.2.1".into(),
        typescript: "6.0.3".into(),
        deno_core: "0.412.0".into(),
        deno_runtime: "0.267.0".into(),
    }
}

#[test]
fn manifest_roundtrip() {
    let manifest = describe(&identity(), b"snapshot", b"table");
    assert_eq!(parse(&render(&manifest)).unwrap(), manifest);
}

#[test]
fn verify_accepts_a_matching_bundle() {
    let manifest = describe(&identity(), b"snapshot", b"table");
    verify(&manifest, &identity(), b"snapshot", b"table").unwrap();
}

#[test]
fn verify_names_the_mismatched_field() {
    let manifest = describe(&identity(), b"snapshot", b"table");
    let mut other = identity();
    other.target = "x86_64-pc-windows-msvc".into();
    let error = verify(&manifest, &other, b"snapshot", b"table").unwrap_err();
    assert!(error.contains("target"), "{error}");
    assert!(error.contains("aarch64-apple-darwin"), "{error}");
}

#[test]
fn verify_rejects_a_truncated_snapshot() {
    let manifest = describe(&identity(), b"snapshot", b"table");
    let error = verify(&manifest, &identity(), b"snapsho", b"table").unwrap_err();
    assert!(error.contains("snapshot does not match"), "{error}");
}

#[test]
fn parse_rejects_unknown_keys() {
    let text = render(&describe(&identity(), b"", b"")).replace(
        &format!("format={FORMAT}"),
        &format!("format={FORMAT}\nextra=1"),
    );
    assert!(parse(&text).unwrap_err().contains("extra"));
}

#[test]
fn rejects_legacy_format_with_regeneration_instruction() {
    let text = render(&describe(&identity(), b"", b"")).replace(FORMAT, "libdeno-snapshot-1");
    assert!(parse(&text).unwrap_err().contains("regenerate"));
}

#[test]
fn residual_integrity_roundtrip_and_failures() {
    let dir = std::env::temp_dir().join(format!("libdeno-residual-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let table = "include_str!(\"residual.js\")";
    std::fs::write(dir.join("residual.js"), b"abc").unwrap();
    let mut manifest = describe(&identity(), b"snapshot", table.as_bytes());
    manifest.residuals = describe_residuals(table, &dir).unwrap();
    let parsed = parse(&render(&manifest)).unwrap();
    verify_residuals(&parsed, table, &dir).unwrap();
    std::fs::write(dir.join("residual.js"), b"abd").unwrap();
    assert!(verify_residuals(&parsed, table, &dir)
        .unwrap_err()
        .contains("does not match"));
    std::fs::remove_file(dir.join("residual.js")).unwrap();
    assert!(verify_residuals(&parsed, table, &dir)
        .unwrap_err()
        .contains("missing"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn copied_residuals_are_checked_against_the_manifest() {
    let root = std::env::temp_dir().join(format!(
        "libdeno-copy-residual-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let source = root.join("source");
    let output = root.join("out");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&output).unwrap();
    let table = "include_str!(\"residual.js\")";
    std::fs::write(source.join("residual.js"), b"abc").unwrap();
    let mut manifest = describe(&identity(), b"snapshot", table.as_bytes());
    manifest.residuals = describe_residuals(table, &source).unwrap();
    verify_residuals(&manifest, table, &source).unwrap();
    copy_verified_residuals(&manifest, table, &source, &output).unwrap();
    assert_eq!(std::fs::read(output.join("residual.js")).unwrap(), b"abc");

    std::fs::write(source.join("residual.js"), b"abd").unwrap();
    let error = copy_verified_residuals(&manifest, table, &source, &output).unwrap_err();
    assert!(
        error.contains("residual source residual.js does not match"),
        "{error}"
    );
    assert_eq!(std::fs::read(output.join("residual.js")).unwrap(), b"abd");

    std::fs::remove_file(source.join("residual.js")).unwrap();
    let error = copy_verified_residuals(&manifest, table, &source, &output).unwrap_err();
    assert!(
        error.contains("does not contain the residual source residual.js"),
        "{error}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn table_sources_lists_included_files() {
    let table = "pub static A: &[(&str, &str)] = &[\n  (\"a\", include_str!(concat!(env!(\"OUT_DIR\"), \"/a_b.js\"))),\n];\n";
    assert_eq!(table_sources(table), vec!["a_b.js"]);
}

#[test]
fn residual_names_cannot_escape_the_bundle_directory() {
    // The manifest is caller-supplied input and both verify and copy do
    // `dir.join(name)`. Without this check a crafted name reads arbitrary
    // files and copies them into OUT_DIR.
    for bad in [
        "../outside.txt",
        "a/../../outside.txt",
        "/etc/passwd",
        "\\windows\\system32\\x",
        "C:/windows/x",
        "./relative.js",
        "a//b.js",
        "",
    ] {
        assert!(
            validate_residual_name(bad).is_err(),
            "{bad:?} should be rejected"
        );
    }
    for good in ["a_b.js", "residual_sources/file.js", "nested/dir/x.ts"] {
        assert!(
            validate_residual_name(good).is_ok(),
            "{good:?} should be accepted"
        );
    }
}

#[cfg(unix)]
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "libdeno-bundle-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(unix)]
fn manifest_for(name: &str, table: &str, bytes: &[u8]) -> Manifest {
    let mut manifest = describe(&identity(), b"snapshot", table.as_bytes());
    manifest.residuals = vec![Residual {
        name: name.into(),
        bytes: bytes.len(),
        hash: fingerprint(bytes),
    }];
    manifest
}

#[cfg(unix)]
#[test]
fn a_symlinked_residual_cannot_read_outside_the_bundle() {
    let root = scratch("symlink-file");
    let bundle = root.join("bundle");
    let outside = root.join("outside.txt");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(&outside, b"secret").unwrap();
    std::os::unix::fs::symlink(&outside, bundle.join("residual.js")).unwrap();

    let table = "include_str!(\"residual.js\")";
    // The manifest is attacker-supplied too, so its hash matches the outside
    // file; only containment can refuse this.
    let manifest = manifest_for("residual.js", table, b"secret");

    let error = verify_residuals(&manifest, table, &bundle).unwrap_err();
    assert!(error.contains("outside the bundle directory"), "{error}");
    let error = describe_residuals(table, &bundle).unwrap_err();
    assert!(error.contains("outside the bundle directory"), "{error}");

    let output = root.join("out");
    std::fs::create_dir_all(&output).unwrap();
    let error = copy_verified_residuals(&manifest, table, &bundle, &output).unwrap_err();
    assert!(error.contains("outside the bundle directory"), "{error}");
    assert!(
        !output.join("residual.js").exists(),
        "the outside file was copied into OUT_DIR"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn verify_rejects_a_traversing_residual_name() {
    // Both the table and the manifest are attacker-controlled in a bundle;
    // a matching pair must still be refused before any file is read.
    let name = "../outside.txt";
    let table = format!(
        "pub static A: &[(&str, &str)] = &[\n  (\"a\", include_str!(concat!(env!(\"OUT_DIR\"), \"/{name}\"))),\n];\n"
    );
    let mut manifest = describe(&identity(), b"snapshot", b"table");
    manifest.residuals = vec![Residual {
        name: name.into(),
        bytes: 0,
        hash: "0".repeat(16),
    }];
    let dir = std::env::temp_dir();
    let error = verify_residuals(&manifest, &table, &dir).unwrap_err();
    assert!(error.contains("not a plain relative path"), "{error}");
}

#[cfg(unix)]
#[test]
fn a_symlinked_directory_cannot_smuggle_a_residual_out() {
    let root = scratch("symlink-dir");
    let bundle = root.join("bundle");
    let outside = root.join("outside");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("file.js"), b"secret").unwrap();
    std::os::unix::fs::symlink(&outside, bundle.join("residual_sources")).unwrap();

    let name = "residual_sources/file.js";
    let table = format!("include_str!(\"{name}\")");
    let manifest = manifest_for(name, &table, b"secret");
    for error in [
        verify_residuals(&manifest, &table, &bundle).unwrap_err(),
        describe_residuals(&table, &bundle).unwrap_err(),
    ] {
        assert!(error.contains("outside the bundle directory"), "{error}");
    }

    let output = root.join("out");
    std::fs::create_dir_all(&output).unwrap();
    let error = copy_verified_residuals(&manifest, &table, &bundle, &output).unwrap_err();
    assert!(error.contains("outside the bundle directory"), "{error}");
    assert!(
        !output.join(name).exists(),
        "the outside file was copied into OUT_DIR"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn nested_and_internally_symlinked_residuals_still_work() {
    let root = scratch("symlink-inside");
    let bundle = root.join("bundle");
    std::fs::create_dir_all(bundle.join("nested/dir")).unwrap();
    std::fs::write(bundle.join("nested/dir/x.js"), b"nested").unwrap();
    // A symlink that resolves inside the bundle is a normal build-tree layout;
    // containment is about where it resolves, not that it is a symlink.
    std::os::unix::fs::symlink(bundle.join("nested/dir/x.js"), bundle.join("alias.js")).unwrap();

    let table = "include_str!(\"nested/dir/x.js\")\ninclude_str!(\"alias.js\")";
    let mut manifest = describe(&identity(), b"snapshot", table.as_bytes());
    manifest.residuals = describe_residuals(table, &bundle).unwrap();
    verify_residuals(&manifest, table, &bundle).unwrap();

    let output = root.join("out");
    copy_verified_residuals(&manifest, table, &bundle, &output).unwrap();
    assert_eq!(
        std::fs::read(output.join("nested/dir/x.js")).unwrap(),
        b"nested"
    );
    assert_eq!(std::fs::read(output.join("alias.js")).unwrap(), b"nested");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_directory_is_not_a_residual_source() {
    let dir = std::env::temp_dir().join(format!(
        "libdeno-bundle-dirname-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("residual.js")).unwrap();
    let table = "include_str!(\"residual.js\")";
    let error = describe_residuals(table, &dir).unwrap_err();
    assert!(error.contains("not a regular file"), "{error}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn table_paths_use_forward_slashes() {
    assert_eq!(
        table_path(Path::new("residual_sources/file.js")),
        "/residual_sources/file.js"
    );
    // Windows separators must not reach the generated literal, where they
    // would be escaped to `\\` and read back verbatim by table_sources.
    let windows = table_path(Path::new(r"residual_sources\file.js"));
    assert_eq!(windows, "/residual_sources/file.js");
    assert!(!windows.contains('\\'), "{windows}");
    assert_eq!(table_path(Path::new(r"a\b\c.js")), "/a/b/c.js");
}

#[test]
fn missing_sources_reports_only_absent_files() {
    let dir = std::env::temp_dir().join(format!(
        "libdeno-bundle-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("present.js"), b"x").unwrap();
    let table = "(\"a\", include_str!(concat!(env!(\"OUT_DIR\"), \"/present.js\"))),\n\
                 (\"b\", include_str!(concat!(env!(\"OUT_DIR\"), \"/absent.js\"))),\n";
    assert_eq!(missing_sources(table, &dir), vec!["absent.js"]);
    std::fs::remove_dir_all(&dir).unwrap();
}
