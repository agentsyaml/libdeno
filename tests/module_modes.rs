use std::fs;
use std::path::PathBuf;

use libdeno::{run_source, LibdenoOptions, SourceLang};

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "libdeno-module-mode-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    }
}

#[test]
fn manual_node_modules_resolves_local_package_with_npm_disabled() {
    let fixture = Fixture::new("manual-local");
    let root = &fixture.0;
    let package = root.join("node_modules/local-fixture");
    fs::create_dir_all(&package).unwrap();
    fs::write(root.join("deno.json"), r#"{"nodeModulesDir":"manual"}"#).unwrap();
    fs::write(
        root.join("package.json"),
        r#"{"name":"module-mode-fixture","dependencies":{"local-fixture":"1.0.0"}}"#,
    )
    .unwrap();
    fs::write(
        package.join("package.json"),
        r#"{"name":"local-fixture","version":"1.0.0","type":"module","exports":"./index.js"}"#,
    )
    .unwrap();
    fs::write(package.join("index.js"), "export const value = 42;").unwrap();

    let code = run_source(
        "import { value } from 'local-fixture'; if (value !== 42) Deno.exit(3);",
        SourceLang::JavaScript,
        root,
        &options(),
    )
    .unwrap();
    assert_eq!(code, 0);
}

#[test]
fn manual_missing_local_package_fails_without_registry_access() {
    let fixture = Fixture::new("manual-missing");
    let root = &fixture.0;
    fs::write(root.join("deno.json"), r#"{"nodeModulesDir":"manual"}"#).unwrap();
    fs::write(
        root.join("package.json"),
        r#"{"name":"module-mode-missing","dependencies":{"missing-local-fixture":"1.0.0"}}"#,
    )
    .unwrap();

    let result = run_source(
        "import 'missing-local-fixture';",
        SourceLang::JavaScript,
        root,
        &options(),
    );
    assert!(
        result.is_err(),
        "missing BYONM package unexpectedly resolved"
    );
}
