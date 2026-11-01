//! A module is evaluated once per run, however many specifiers reach it.
//!
//! ES modules are keyed by resolved URL, so a re-export barrel, a static
//! import and a dynamic import of the same file must all observe one instance
//! with one evaluation. The distinction matters for a host that runs code with
//! module-level side effects: a second evaluation would double-register
//! handlers or reset state.

use std::fs;
use std::path::PathBuf;

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-modid-{}-{name}", std::process::id()));
    fs::create_dir_all(dir.join("sub")).unwrap();
    dir
}

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    }
}

#[test]
fn one_evaluation_across_static_reexport_and_dynamic_imports() {
    let dir = temp_dir("once");
    let marker = dir.join("out.txt");
    fs::write(
        dir.join("count.js"),
        "globalThis.__evaluations = (globalThis.__evaluations || 0) + 1;\n\
         export const n = globalThis.__evaluations;\n",
    )
    .unwrap();
    fs::write(dir.join("sub/alias.js"), "export * from '../count.js';\n").unwrap();
    fs::write(
        dir.join("main.js"),
        format!(
            "import {{ n }} from './count.js';\n\
             import {{ n as viaAlias }} from './sub/alias.js';\n\
             const dynamic = await import('./count.js');\n\
             Deno.writeTextFileSync({:?}, `${{n}} ${{viaAlias}} ${{dynamic.n}} ${{globalThis.__evaluations}}`);\n",
            marker.to_string_lossy()
        ),
    )
    .unwrap();

    libdeno::run(dir.join("main.js"), &options()).unwrap();
    let observed = fs::read_to_string(&marker).unwrap();
    let _ = fs::remove_dir_all(&dir);

    // Four views of one module, one evaluation.
    assert_eq!(observed, "1 1 1 1");
}

#[test]
fn two_dynamic_imports_return_the_same_namespace() {
    let dir = temp_dir("dynamic");
    let marker = dir.join("out.txt");
    fs::write(
        dir.join("count.js"),
        "globalThis.__evaluations = (globalThis.__evaluations || 0) + 1;\n\
         export const n = globalThis.__evaluations;\n",
    )
    .unwrap();
    fs::write(
        dir.join("main.js"),
        format!(
            "const first = await import('./count.js');\n\
             const second = await import('./count.js');\n\
             Deno.writeTextFileSync({:?}, `${{first === second}} ${{globalThis.__evaluations}}`);\n",
            marker.to_string_lossy()
        ),
    )
    .unwrap();

    libdeno::run(dir.join("main.js"), &options()).unwrap();
    let observed = fs::read_to_string(&marker).unwrap();
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(observed, "true 1");
}

#[test]
fn a_cycle_fails_with_a_temporal_dead_zone_error() {
    // Matches Deno CLI: a cycle is not an error by itself, but reading a
    // `const` binding from the other module during evaluation hits the TDZ.
    let dir = temp_dir("cycle");
    fs::write(
        dir.join("a.js"),
        "import { b } from './b.js';\nexport const a = 'A';\nexport const seen = b;\n",
    )
    .unwrap();
    fs::write(
        dir.join("b.js"),
        "import { a } from './a.js';\nexport const b = 'B';\nexport const sawA = typeof a;\n",
    )
    .unwrap();
    fs::write(
        dir.join("main.js"),
        "import { seen } from './a.js';\nconsole.log(seen);\n",
    )
    .unwrap();

    let error = libdeno::run(dir.join("main.js"), &options())
        .unwrap_err()
        .to_string();
    let _ = fs::remove_dir_all(&dir);

    assert!(
        error.contains("Cannot access") && error.contains("before initialization"),
        "expected a TDZ error, got: {error}"
    );
}

/// The counterpart of the test above: the graph is per run, not per process.
///
/// `run` documents "each call gets a fresh worker and module graph". Without
/// that, a module with top-level side effects would evaluate once and every
/// later run would see stale state — a host that reloads a plugin by running it
/// again would silently keep the old version.
///
/// The result goes to a file rather than captured output: in-process capture is
/// process-global and this binary's other tests run in parallel.
#[test]
fn each_run_gets_a_fresh_module_graph() {
    let dir = temp_dir("fresh-graph");
    let result = dir.join("result.txt");
    fs::write(
        dir.join("counter.js"),
        "globalThis.__libdenoEvals = (globalThis.__libdenoEvals ?? 0) + 1;\n\
         export const evaluations = globalThis.__libdenoEvals;\n",
    )
    .unwrap();
    // Two specifiers for one file: within a run it must evaluate once.
    fs::write(
        dir.join("main.js"),
        format!(
            "import {{ evaluations }} from './counter.js';\n\
             import {{ evaluations as again }} from './counter.js';\n\
             Deno.writeTextFileSync({result:?}, evaluations + '/' + again);\n",
            result = result.to_string_lossy()
        ),
    )
    .unwrap();

    let mut observed = Vec::new();
    for _ in 0..3 {
        libdeno::run(dir.join("main.js"), &options()).expect("the run must succeed");
        observed.push(fs::read_to_string(&result).unwrap());
    }
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        observed,
        vec!["1/1", "1/1", "1/1"],
        "a run must evaluate the module once and must not reuse a previous \
         run's graph; if this is deliberate, update docs/api.md and delete this test"
    );
}
