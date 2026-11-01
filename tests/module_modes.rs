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

/// An existing `node_modules` directory does not switch the mode to `manual`.
///
/// `docs/npm-support.md` says the default is managed `auto` and that "an
/// existing `node_modules` directory does not select `manual`". A host that
/// ships a prebuilt tree would otherwise get `manual` behaviour by accident:
/// the same import would resolve from a directory the host never declared as
/// the source of truth.
#[test]
fn an_existing_node_modules_directory_does_not_select_manual_mode() {
    let fixture = Fixture::new("default-auto");
    let root = &fixture.0;
    let package = root.join("node_modules/undeclared-fixture");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("package.json"),
        r#"{"name":"undeclared-fixture","version":"1.0.0","type":"module","exports":"./index.js"}"#,
    )
    .unwrap();
    fs::write(
        package.join("index.js"),
        "export default 'from-node-modules';\n",
    )
    .unwrap();
    // No deno.json: the mode stays at the managed default.
    fs::write(
        root.join("main.js"),
        "import value from 'undeclared-fixture';\nconsole.log(value);\n",
    )
    .unwrap();

    let error = libdeno::run(
        root.join("main.js"),
        &LibdenoOptions {
            allow_all_permissions: true,
            cwd: Some(root.clone()),
            ..Default::default()
        },
    )
    .expect_err("managed mode must not resolve an undeclared package from node_modules");

    assert!(
        error.to_string().contains("not a dependency"),
        "expected the managed-mode rejection, got: {error}"
    );
}

/// `nodeModulesDir: "none"` drops local `node_modules` from resolution.
///
/// `docs/npm-support.md` says `none` removes the BYONM tree and the managed
/// root. It is not an npm switch: the import still goes through managed
/// resolution. A host that reads `none` as "no npm at all" would expect a
/// different outcome here.
#[test]
fn node_modules_dir_none_ignores_a_complete_local_tree() {
    let fixture = Fixture::new("mode-none");
    let root = &fixture.0;
    let package = root.join("node_modules/fixture");
    fs::create_dir_all(&package).unwrap();
    fs::write(
        package.join("package.json"),
        r#"{"name":"fixture","version":"1.0.0","type":"module","exports":"./index.js"}"#,
    )
    .unwrap();
    fs::write(package.join("index.js"), "export default 'local';\n").unwrap();
    fs::write(
        root.join("package.json"),
        r#"{"name":"mode-none-fixture","dependencies":{"fixture":"1.0.0"}}"#,
    )
    .unwrap();
    fs::write(
        root.join("main.js"),
        "import value from 'fixture';\nconsole.log('got ' + value);\n",
    )
    .unwrap();

    // Subprocess mode: this binary's other tests hold the process-global
    // in-process capture lease in parallel. A script failure there is `Ok` with
    // a non-zero exit code, so the assertion is on that pair.
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    // (exit code, stdout, first stderr line): the failure wording is the point
    // of the `auto`/`none` assertions below, and it lands on stderr.
    let run_with_mode = |mode: &str| -> (i32, String, String) {
        fs::write(
            root.join("deno.json"),
            format!(r#"{{"nodeModulesDir":"{mode}"}}"#),
        )
        .unwrap();
        let output = libdeno::run_in_subprocess_with_output(
            root.join("main.js"),
            &LibdenoOptions {
                allow_all_permissions: true,
                cwd: Some(root.clone()),
                capture_stdout: true,
                capture_stderr: true,
                ..Default::default()
            },
        )
        .expect("the run must start");
        (
            output.exit_code,
            String::from_utf8_lossy(&output.stdout).trim().to_string(),
            String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("")
                .to_string(),
        )
    };

    let manual = run_with_mode("manual");
    let none = run_with_mode("none");
    let auto = run_with_mode("auto");

    assert_eq!(
        (manual.0, manual.1.as_str()),
        (0, "got local"),
        "manual mode must use the local tree"
    );
    assert_ne!(
        none.0, 0,
        "none mode must not resolve the local tree; stdout was {:?}",
        none.1
    );
    // `auto` also fails here, and with the registry's wording rather than a
    // local-resolution error: it consulted the local tree, considered it
    // incomplete for this dependency, and asked the registry — which has no
    // such package. `docs/npm-support.md` states this pair explicitly.
    assert_ne!(auto.0, 0, "auto mode must not resolve the local tree");
    for (mode, output) in [("auto", &auto), ("none", &none)] {
        assert!(
            output
                .2
                .contains("Could not find npm package 'fixture' matching '1.0.0'"),
            "{mode} mode must fail with the registry wording, got {:?}",
            output.2
        );
    }
}

/// `manual` is the offline mode: a missing local package fails without a
/// registry request.
///
/// The run has no `--allow-net`, so a registry attempt would fail with a
/// permission error instead. The assertion is on the wording that names the
/// `node_modules` directory, which only the local-resolution path produces.
#[test]
fn manual_mode_does_not_reach_the_registry() {
    let fixture = Fixture::new("manual-offline");
    let root = &fixture.0;
    fs::write(
        root.join("package.json"),
        r#"{"name":"manual-offline-fixture","dependencies":{"left-pad":"1.3.0"}}"#,
    )
    .unwrap();
    fs::write(root.join("deno.json"), r#"{"nodeModulesDir":"manual"}"#).unwrap();
    fs::write(
        root.join("main.js"),
        "import pad from 'left-pad';\nconsole.log(typeof pad);\n",
    )
    .unwrap();

    let error = libdeno::run(
        root.join("main.js"),
        &LibdenoOptions {
            permissions: vec![format!("--allow-read={}", root.display())],
            cwd: Some(root.clone()),
            ..Default::default()
        },
    )
    .expect_err("a missing local package must fail in manual mode");

    let message = error.to_string();
    assert!(
        message.contains("node_modules") && !message.contains("Requires net access"),
        "manual mode must fail on the local tree, not on the network: {message}"
    );
}
