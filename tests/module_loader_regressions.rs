use std::fs;
use std::path::PathBuf;

use libdeno::{run_source, LibdenoOptions, SourceLang};

struct Fixture(PathBuf);

impl Fixture {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "libdeno-loader-{name}-{}-{}",
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
fn dynamically_failed_module_can_be_imported_after_creation() {
    let fixture = Fixture::new("reload");
    fs::write(
        fixture.0.join("sibling.js"),
        "globalThis.siblingRuns = (globalThis.siblingRuns ?? 0) + 1; export const sibling = true;",
    )
    .unwrap();
    let code = r#"
      globalThis.runs ??= 0;
      await import('./sibling.js');
      const target = './' + 'later.js';
      let rejected = false;
      try { await import(target); } catch (e) { rejected = true; }
      if (!rejected) Deno.exit(8);
      await Deno.writeTextFile(new URL(target, import.meta.url), 'globalThis.runs++; export const ok = true;');
      const m = await import(target);
      if (!m.ok || globalThis.runs !== 1 || globalThis.siblingRuns !== 1) Deno.exit(9);
    "#;
    assert_eq!(
        run_source(code, SourceLang::JavaScript, &fixture.0, &options()).unwrap(),
        0
    );
}

#[test]
fn absolute_file_import_map_applies_to_dynamic_and_static_imports() {
    let fixture = Fixture::new("import-map");
    let a = fixture.0.join("a.js");
    let b = fixture.0.join("b.js");
    fs::write(&a, "export const value = 'A';").unwrap();
    fs::write(&b, "export const value = 'B';").unwrap();
    let map = deno_core::serde_json::json!({
        "imports": {
            url::Url::from_file_path(&a).unwrap().as_str(): url::Url::from_file_path(&b).unwrap().as_str()
        }
    }).to_string();
    fs::write(fixture.0.join("deno.json"), map).unwrap();
    let url = url::Url::from_file_path(&a).unwrap();
    let source = format!("import {{ value }} from '{}'; if(value !== 'B') Deno.exit(4); const target = '{}'.slice(0); const m=await import(target); if(m.value !== 'B') Deno.exit(5);", url, url);
    assert_eq!(
        run_source(&source, SourceLang::JavaScript, &fixture.0, &options()).unwrap(),
        0
    );
}

#[test]
fn unknown_node_builtin_is_rejected_and_known_builtin_loads() {
    let fixture = Fixture::new("builtins");
    let unknown = run_source(
        "for (const suffix of ['', '?probe', '#probe']) { const target = 'node:' + 'no-such-builtin' + suffix; let caught = false; try { await import(target); } catch (e) { if (!String(e).includes('Unknown node builtin')) throw e; caught = true; } if (!caught) Deno.exit(6); }",
        SourceLang::JavaScript,
        &fixture.0,
        &options()
    );
    assert_eq!(unknown.unwrap(), 0);
    assert_eq!(
        run_source(
            "for (const suffix of ['', '?probe', '#probe']) { const target = 'node:' + 'path' + suffix; const path = await import(target); if (path.basename('/a/b') !== 'b') Deno.exit(7); }",
            SourceLang::JavaScript,
            &fixture.0,
            &options()
        )
        .unwrap(),
        0
    );
}

#[test]
fn import_map_node_builtin_aliases_use_canonical_identity() {
    let fixture = Fixture::new("node-aliases");
    let map = deno_core::serde_json::json!({
        "imports": {
            "#plain": "node:path",
            "#query": "node:path?probe",
            "#fragment": "node:path#probe"
        }
    });
    fs::write(fixture.0.join("deno.json"), map.to_string()).unwrap();
    let source = "for (const name of ['plain', 'query', 'fragment']) { const target = '#' + name; const path = await import(target); if (path.basename('/a/b') !== 'b') throw new Error('node:path alias did not resolve to the builtin'); }";
    assert_eq!(
        run_source(source, SourceLang::JavaScript, &fixture.0, &options()).unwrap(),
        0
    );
}
