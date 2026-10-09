use std::fs;
use std::path::PathBuf;

use libdeno::{run_source, LibdenoOptions, SourceLang};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "libdeno-cjs-analysis-{}-{}",
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

#[test]
fn esm_then_cjs_barrel_preserves_named_esm_exports() {
    let fixture = Fixture::new();
    let root = &fixture.0;
    let package = root.join("node_modules/mode-fixture");
    fs::create_dir_all(&package).unwrap();
    fs::write(root.join("deno.json"), r#"{"nodeModulesDir":"manual"}"#).unwrap();
    fs::write(
        root.join("package.json"),
        r#"{"name":"analysis-modes-fixture","dependencies":{"mode-fixture":"1.0.0"}}"#,
    )
    .unwrap();
    fs::write(
        package.join("package.json"),
        r#"{"name":"mode-fixture","version":"1.0.0","type":"module"}"#,
    )
    .unwrap();
    fs::write(package.join("entry.js"), "export const namedValue = 42;").unwrap();
    fs::write(
        package.join("barrel.cjs"),
        "module.exports = require('./entry.js');",
    )
    .unwrap();

    let code = r#"
      const first = await import('./node_modules/mode-fixture/entry.js');
      if (first.namedValue !== 42) Deno.exit(10);
      const barrelTarget = './node_modules/' + 'mode-fixture/barrel.cjs';
      const barrel = await import(barrelTarget);
      if (!Object.prototype.hasOwnProperty.call(barrel, 'namedValue') || barrel.namedValue !== 42) Deno.exit(11);
    "#;
    assert_eq!(
        run_source(
            code,
            SourceLang::JavaScript,
            root,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            },
        )
        .unwrap(),
        0
    );
}
