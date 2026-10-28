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
fn literal_dynamic_import_honors_revoked_read() {
    let fixture = Fixture::new("dynamic-revoke");
    let dir = fs::canonicalize(&fixture.0).unwrap();
    fs::write(fixture.0.join("secret.js"), "export const secret = 1;").unwrap();
    // Both spellings must fail: the literal one is already cached in the graph
    // from the root build, and the computed one is only resolved at import
    // time. The permission check must happen when the import runs either way.
    let code = r#"
      await Deno.permissions.revoke({ name: 'read' });
      let literalDenied = false;
      try { await import('./secret.js'); } catch { literalDenied = true; }
      const computed = './' + 'secret.js';
      let computedDenied = false;
      try { await import(computed); } catch { computedDenied = true; }
      if (!literalDenied || !computedDenied) Deno.exit(9);
    "#;
    let restricted = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    assert_eq!(
        run_source(code, SourceLang::JavaScript, &dir, &restricted).unwrap(),
        0
    );
}

#[test]
fn literal_dynamic_import_loads_without_revocation() {
    let fixture = Fixture::new("dynamic-granted");
    let dir = fs::canonicalize(&fixture.0).unwrap();
    fs::write(fixture.0.join("secret.js"), "export const secret = 1;").unwrap();
    let restricted = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        ..Default::default()
    };
    assert_eq!(
        run_source(
            "const m = await import('./secret.js'); if (m.secret !== 1) Deno.exit(9);",
            SourceLang::JavaScript,
            &dir,
            &restricted
        )
        .unwrap(),
        0
    );
}

#[cfg(unix)]
#[test]
fn installed_broker_denies_npm_esm_source_read() {
    const CHILD: &str = "LIBDENO_LOADER_BROKER_CHILD";
    if std::env::var_os(CHILD).is_some() {
        run_npm_broker_case();
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "installed_broker_denies_npm_esm_source_read",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "isolated broker test failed: {status}");
            return;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("isolated broker test exceeded 90 seconds");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The npm module loader in `deno_resolver` reads package files with a raw
/// `sys.fs_read`; an installed broker must still decide that read.
#[cfg(unix)]
fn run_npm_broker_case() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    let fixture = Fixture::new("npm-broker");
    let root = &fixture.0;
    let package = root.join("node_modules/pkg");
    fs::create_dir_all(&package).unwrap();
    fs::write(root.join("deno.json"), r#"{"nodeModulesDir":"manual"}"#).unwrap();
    fs::write(
        root.join("package.json"),
        r#"{"name":"npm-broker-fixture","dependencies":{"pkg":"1.0.0"}}"#,
    )
    .unwrap();
    fs::write(
        package.join("package.json"),
        r#"{"name":"pkg","version":"1.0.0","type":"module","exports":"./index.mjs"}"#,
    )
    .unwrap();
    fs::write(
        package.join("index.mjs"),
        "globalThis.npmSecretRan = true; export const value = 1;",
    )
    .unwrap();
    let target = fs::canonicalize(package.join("index.mjs")).unwrap();

    // AF_UNIX paths are limited to ~104 bytes; keep the socket out of the
    // (long) fixture path.
    let socket = std::env::temp_dir().join(format!(
        "ldb-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let listener = UnixListener::bind(&socket).unwrap();
    let denied = Arc::new(AtomicBool::new(false));
    let observed = denied.clone();
    let (stop, stopped) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(true).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        loop {
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    let request: deno_core::serde_json::Value =
                        deno_core::serde_json::from_str(line.trim()).unwrap();
                    line.clear();
                    let id = request["id"].as_u64().unwrap();
                    let is_target = request["value"]
                        .as_str()
                        .is_some_and(|value| std::path::Path::new(value) == target.as_path());
                    if is_target {
                        observed.store(true, Ordering::SeqCst);
                    }
                    let result = if is_target { "deny" } else { "allow" };
                    writeln!(stream, "{{\"id\":{id},\"result\":\"{result}\"}}").unwrap();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if stopped.try_recv().is_ok() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    libdeno::install_permission_broker(&socket).unwrap();
    let status = run_source(
        "let denied = false; try { await import('pkg'); } catch { denied = true; } \
         if (!denied || globalThis.npmSecretRan) Deno.exit(3);",
        SourceLang::JavaScript,
        root,
        &options(),
    )
    .unwrap();
    assert!(
        denied.load(Ordering::SeqCst),
        "the broker never denied the npm source read"
    );
    assert_eq!(status, 0);
    let _ = stop.send(());
    server.join().unwrap();
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
