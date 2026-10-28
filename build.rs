// Copyright 2018-2026 the Deno authors. MIT license.
// Runtime snapshot build script, adapted from deno's cli/snapshot/build.rs.
// Creates a V8 snapshot with all runtime extension JS compiled in, and
// emits the residual lazy-load source tables for the main binary.

use std::env;
use std::path::PathBuf;

#[cfg(feature = "snapshot")]
use std::collections::HashSet;
#[cfg(feature = "snapshot")]
use std::io::Write;
#[cfg(any(feature = "snapshot", feature = "snapshot-import"))]
use std::path::Path;

#[cfg(feature = "snapshot")]
use deno_runtime::ops::bootstrap::SnapshotOptions;
#[cfg(feature = "snapshot")]
use deno_runtime::snapshot::create_runtime_snapshot;
#[cfg(feature = "snapshot")]
use deno_runtime::snapshot::LazyExtensionFileKind;

#[cfg(feature = "snapshot")]
#[path = "build_support/snapshot_arch.rs"]
mod snapshot_arch;

#[cfg(any(feature = "snapshot", feature = "snapshot-import"))]
#[path = "build_support/snapshot_bundle.rs"]
mod snapshot_bundle;
#[cfg(any(feature = "snapshot", feature = "snapshot-import"))]
#[path = "build_support/snapshot_compat.rs"]
mod snapshot_compat;
#[cfg(any(feature = "snapshot", feature = "snapshot-import"))]
#[path = "build_support/snapshot_identity.rs"]
mod snapshot_identity;
#[cfg(feature = "snapshot")]
#[path = "build_support/snapshot_sources.rs"]
mod snapshot_sources;

fn main() {
    // Read in the no-snapshot branch below; without this directive, removing
    // DOCS_RS reuses the cached build-script output and keeps the placeholder.
    println!("cargo:rerun-if-env-changed=DOCS_RS");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    #[cfg(any(feature = "snapshot", feature = "snapshot-import"))]
    {
        println!("cargo:rerun-if-env-changed=LIBDENO_SNAPSHOT_DIR");
        println!("cargo:rerun-if-env-changed=LIBDENO_SNAPSHOT_COMPATIBILITY_FILE");
        let prebuilt = env::var_os("LIBDENO_SNAPSHOT_DIR");
        #[cfg(feature = "snapshot")]
        println!("cargo:rerun-if-env-changed=DENO_SNAPSHOT_MINIFY_SOURCES");
        #[cfg(feature = "snapshot")]
        println!("cargo:rerun-if-env-changed=LIBDENO_SNAPSHOT_EXPORT_DIR");
        #[cfg(feature = "snapshot")]
        if prebuilt.is_none() {
            if let Some(bundle) = env::var_os("LIBDENO_SNAPSHOT_EXPORT_DIR") {
                let sidecar = Path::new(&bundle).join(snapshot_compat::SIDECAR);
                if sidecar.exists() {
                    std::fs::remove_file(&sidecar).unwrap_or_else(|e| {
                        panic!("failed to remove stale {}: {e}", sidecar.display())
                    });
                }
            }
        }
        #[cfg(all(feature = "snapshot-import", not(feature = "snapshot")))]
        if prebuilt.is_none() {
            panic!("the `snapshot-import` feature requires LIBDENO_SNAPSHOT_DIR");
        }
        #[cfg(feature = "snapshot")]
        if prebuilt.is_none() {
            ensure_snapshot_architecture();
        }
        let expected_v8 = snapshot_identity::ledger(
            &PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("Cargo.toml"),
            "v8-version",
        );
        #[cfg(feature = "snapshot")]
        assert_eq!(
            deno_runtime_version_string(),
            expected_v8,
            "linked V8 version differs from Cargo.toml v8-version ledger"
        );
        println!("cargo:rustc-env=LIBDENO_EXPECTED_V8_VERSION={expected_v8}");
        #[cfg(feature = "snapshot")]
        {
            // The integration smoke test invokes this exact build-script binary.
            let marker = out_dir.join("SNAPSHOT_BUILD_SCRIPT_PATH");
            std::fs::write(
                &marker,
                env::current_exe()
                    .unwrap()
                    .as_os_str()
                    .to_string_lossy()
                    .as_bytes(),
            )
            .unwrap();
            println!(
                "cargo:rustc-env=LIBDENO_SNAPSHOT_BUILD_SCRIPT_MARKER={}",
                marker.display()
            );
            match prebuilt {
                Some(dir) => install_prebuilt_snapshot(Path::new(&dir), &out_dir),
                None => {
                    let snapshot_path = out_dir.join("CLI_SNAPSHOT.bin");
                    let residual_path = out_dir.join("EXTENSION_RESIDUAL_SOURCES.rs");
                    create_cli_snapshot(&snapshot_path, &residual_path, &out_dir);
                    if let Some(dir) = env::var_os("LIBDENO_SNAPSHOT_EXPORT_DIR") {
                        export_snapshot_bundle(Path::new(&dir), &snapshot_path, &residual_path);
                    }
                }
            }
        }
        #[cfg(all(feature = "snapshot-import", not(feature = "snapshot")))]
        install_prebuilt_snapshot(Path::new(&prebuilt.unwrap()), &out_dir);
    }

    // Node-API symbols: the example host binary re-exports napi_* via
    // .cargo/config.toml rustflags (dev-only). Real embedders must export them
    // from their own binary, e.g. in their build.rs:
    //   deno_napi::print_linker_flags("<host-binary-name>");

    #[cfg(not(any(feature = "snapshot", feature = "snapshot-import")))]
    {
        // Guard: a no-default-features build produces an empty snapshot and
        // residual tables — the crate compiles but every run panics. The only
        // legitimate consumer of this branch is docs.rs (which only rustdoc's
        // the lib). Fail the build for everyone else so "forgot the snapshot
        // feature" surfaces at compile time instead of at the first run.
        // Presence check (not equality against "1"): the tree convention —
        // v8's own build script included — is `var_os("DOCS_RS").is_some()`.
        if env::var_os("DOCS_RS").is_none() {
            panic!(
                "libdeno requires the `snapshot` feature (enabled by default) to build \
                 a runnable crate; the no-default-features build exists only for \
                 docs.rs documentation builds (DOCS_RS)"
            );
        }

        // Placeholder build for docs.rs. The snapshot pipeline links the Deno
        // runtime (and thus V8) into the build script binary; docs.rs builds
        // offline, and the v8 crate skips emitting its static library under
        // DOCS_RS=1, so that link fails. docs.rs only rustdoc's the lib —
        // never links or runs it — so an empty snapshot and empty residual
        // tables keep lib.rs compiling (include! / include_bytes! only need
        // the files to exist with matching types).
        std::fs::write(out_dir.join("CLI_SNAPSHOT.bin"), b"").unwrap();
        std::fs::write(
            out_dir.join("EXTENSION_RESIDUAL_SOURCES.rs"),
            "// @generated by build.rs (docs.rs placeholder) - do not edit.\n\n\
             pub static RESIDUAL_LAZY_JS: &[(&str, &str)] = &[];\n\n\
             pub static RESIDUAL_LAZY_ESM: &[(&str, &str)] = &[];\n",
        )
        .unwrap();
    }
}

#[cfg(feature = "snapshot")]
fn ensure_snapshot_architecture() {
    let host = env::var("HOST").expect("Cargo must provide HOST to the snapshot build script");
    let target =
        env::var("TARGET").expect("Cargo must provide TARGET to the snapshot build script");
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH")
        .expect("Cargo must provide CARGO_CFG_TARGET_ARCH to the snapshot build script");
    snapshot_arch::ensure_compatible(env::consts::ARCH, &target_arch, &host, &target)
        .unwrap_or_else(|error| panic!("{error}"));
}

// The TypeScript version bundled in deno_runtime 0.267.0's snapshot.
// No crate in the dep tree exposes the bundled TypeScript compiler version
// (deno_ast::VERSION is the deno_ast crate version, not the TS compiler's), so
// it cannot be machine-verified against a dependency. Source of truth: the deno
// repo's cli/snapshot/shared.rs TS_VERSION at the tag for deno_runtime 0.267.0
// (v2.9.7) — update in lockstep with the deno_runtime bump.
//
// The checked-in Cargo.toml ledger and the offline CI/release checks catch
// local declaration drift, but they cannot prove upstream provenance. When
// bumping deno_runtime, copy TS_VERSION from the matching deno tag and update
// the ledger together. Snapshot bundles record this value, so a stale entry
// also rejects bundles built by another version.
#[cfg(feature = "snapshot")]
fn typescript_version() -> &'static str {
    const TS_VERSION: &str = "6.0.3";
    // Only catches malformed values, not a wrong-but-well-formed one.
    let parts: Vec<&str> = TS_VERSION.split('.').collect();
    assert!(
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())),
        "TS_VERSION {TS_VERSION:?} must be a bare x.y.z version; \
         sync it by hand with the deno tag when bumping deno_runtime"
    );
    TS_VERSION
}

/// Snapshot creation reads every extension source from disk, and deno_core
/// reports a missing one as a bare `os error 2` with no path. An incompletely
/// copied `[patch.crates-io]` path dependency is the usual cause, so name the
/// files instead of leaving the error opaque.
#[cfg(feature = "snapshot")]
fn ensure_extension_sources_exist() {
    let collect = || {
        let extensions = deno_runtime::snapshot_info::get_extensions_in_snapshot();
        snapshot_sources::missing(&extensions)
    };
    // Diagnostic only: never turn a panic in the enumeration itself into a
    // build failure that did not exist before.
    let Ok(missing) = std::panic::catch_unwind(collect) else {
        return;
    };
    if missing.is_empty() {
        return;
    }
    let list = missing
        .iter()
        .map(|path| format!("  {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    panic!(
        "libdeno: {} extension source file(s) that snapshot creation reads from disk are \
         missing:\n{list}\n\
         This usually means a [patch.crates-io] path dependency was copied without all \
         of its files; restore them or drop the patch.",
        missing.len()
    );
}

#[cfg(feature = "snapshot")]
fn create_cli_snapshot(snapshot_path: &Path, residual_path: &Path, out_dir: &Path) {
    ensure_extension_sources_exist();

    let snapshot_options = SnapshotOptions {
        ts_version: typescript_version().to_string(),
        v8_version: deno_runtime::deno_core::v8::VERSION_STRING,
        target: env::var("TARGET").unwrap(),
    };

    // Snapshot creation boots a full Deno runtime, which reads extension
    // sources from the cargo registry. A missing file there surfaces only as
    // "os error 2" from V8's runtime init, which is impossible to act on, so
    // print the environment that decides those paths before re-panicking.
    let output = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        create_runtime_snapshot(snapshot_path.to_path_buf(), snapshot_options, vec![])
    })) {
        Ok(output) => output,
        Err(payload) => {
            eprintln!("libdeno: snapshot creation failed before the crate compiled");
            for name in [
                "TARGET",
                "HOST",
                "CARGO_HOME",
                "CARGO_MANIFEST_DIR",
                "OUT_DIR",
            ] {
                if let Some(value) = env::var_os(name) {
                    eprintln!("  {name}={}", PathBuf::from(value).display());
                }
            }
            if let Ok(cwd) = env::current_dir() {
                eprintln!("  cwd={}", cwd.display());
            }
            eprintln!(
                "  hint: ensure the cargo registry checkout under CARGO_HOME is readable \
                 (offline mirrors, moved vendor directories, junctions, and antivirus \
                 quarantine can hide it)."
            );
            std::panic::resume_unwind(payload);
        }
    };

    let consumed: HashSet<&str> = output
        .consumed_lazy_specifiers
        .iter()
        .map(String::as_str)
        .collect();

    let residual_sources_dir = out_dir.join("residual_sources");
    std::fs::create_dir_all(&residual_sources_dir).unwrap();

    let mut residual_js: Vec<(&str, PathBuf)> = Vec::new();
    let mut residual_esm: Vec<(&str, PathBuf)> = Vec::new();
    for file in &output.lazy_extension_files {
        if consumed.contains(file.specifier.as_str()) {
            continue;
        }
        // Make sure we rebuild when residual sources change.
        println!("cargo:rerun-if-changed={}", file.path.display());
        // Both lazy_loaded_js and lazy_loaded_esm skip transpilation at runtime,
        // so pre-transpile here (node builtins are TypeScript).
        let transpiled_path =
            transpile_residual_source(&residual_sources_dir, &file.specifier, &file.path);
        match file.kind {
            LazyExtensionFileKind::Js => {
                wrap_residual_js_source(&transpiled_path);
                residual_js.push((file.specifier.as_str(), transpiled_path));
            }
            LazyExtensionFileKind::Esm => {
                residual_esm.push((file.specifier.as_str(), transpiled_path));
            }
        }
    }

    let mut f = std::fs::File::create(residual_path).unwrap();
    writeln!(f, "// @generated by build.rs - do not edit.\n").unwrap();
    write_residual_table(&mut f, out_dir, "RESIDUAL_LAZY_JS", &residual_js);
    write_residual_table(&mut f, out_dir, "RESIDUAL_LAZY_ESM", &residual_esm);
}

#[cfg(feature = "snapshot")]
fn transpile_residual_source(out_dir: &Path, specifier: &str, src_path: &Path) -> PathBuf {
    use deno_runtime::deno_core::ModuleCodeString;
    use deno_runtime::deno_core::ModuleName;
    use deno_runtime::transpile::maybe_transpile_source;

    let source = std::fs::read_to_string(src_path).unwrap_or_else(|e| {
        panic!(
            "failed to read residual lazy source {}: {e}",
            src_path.display()
        )
    });
    let name = ModuleName::from(specifier.to_string());
    let (transpiled, _source_map) = maybe_transpile_source(name, ModuleCodeString::from(source))
        .unwrap_or_else(|e| panic!("failed to transpile residual lazy source {specifier}: {e}"));

    let sanitized: String = specifier
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let out_path = out_dir.join(format!("{sanitized}.js"));
    std::fs::write(&out_path, transpiled.as_bytes()).unwrap();
    out_path
}

#[cfg(feature = "snapshot")]
fn wrap_residual_js_source(path: &Path) {
    use deno_runtime::deno_core::wrap_lazy_ext_script;
    let source = std::fs::read_to_string(path).unwrap();
    let wrapped = wrap_lazy_ext_script(&source);
    std::fs::write(path, wrapped.as_bytes()).unwrap();
}

#[cfg(feature = "snapshot")]
fn write_residual_table(
    f: &mut std::fs::File,
    out_dir: &Path,
    name: &str,
    entries: &[(&str, PathBuf)],
) {
    writeln!(f, "pub static {name}: &[(&str, &str)] = &[").unwrap();
    let mut entries = entries.to_vec();
    entries.sort_by_key(|(specifier, _)| *specifier);
    for (specifier, transpiled_path) in entries {
        let rel = transpiled_path.strip_prefix(out_dir).unwrap();
        writeln!(
            f,
            "  ({specifier:?}, include_str!(concat!(env!(\"OUT_DIR\"), {:?}))),",
            snapshot_bundle::table_path(rel),
        )
        .unwrap();
    }
    writeln!(f, "];\n").unwrap();
}

#[cfg(feature = "snapshot")]
fn snapshot_identity() -> snapshot_bundle::Identity {
    let (deno_core, deno_runtime) = declared_deno_versions();
    snapshot_bundle::Identity {
        libdeno: env!("CARGO_PKG_VERSION").to_string(),
        target: env::var("TARGET").expect("Cargo must provide TARGET to the build script"),
        v8: deno_runtime_version_string(),
        typescript: typescript_version().to_string(),
        deno_core,
        deno_runtime,
    }
}

#[cfg(any(feature = "snapshot", feature = "snapshot-import"))]
fn expected_snapshot_identity() -> snapshot_bundle::Identity {
    let (deno_core, deno_runtime) = declared_deno_versions();
    snapshot_bundle::Identity {
        libdeno: env!("CARGO_PKG_VERSION").to_string(),
        target: env::var("TARGET").expect("Cargo must provide TARGET to the build script"),
        v8: snapshot_identity::ledger(
            &PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("Cargo.toml"),
            "v8-version",
        ),
        typescript: expected_typescript_version(),
        deno_core,
        deno_runtime,
    }
}
#[cfg(feature = "snapshot")]
fn expected_typescript_version() -> String {
    typescript_version().to_string()
}
#[cfg(all(feature = "snapshot-import", not(feature = "snapshot")))]
fn expected_typescript_version() -> String {
    snapshot_identity::ledger(
        &PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("Cargo.toml"),
        "typescript-version",
    )
}
#[cfg(feature = "snapshot")]
fn deno_runtime_version_string() -> String {
    deno_runtime::deno_core::v8::VERSION_STRING.to_string()
}

/// These are declared ledger versions, not Cargo's resolved dependency graph.
/// They detect version changes but cannot prove that snapshot-relevant
/// dependency versions, features, or source patches match the bundle.
#[cfg(any(feature = "snapshot", feature = "snapshot-import"))]
fn declared_deno_versions() -> (String, String) {
    let manifest_path =
        Path::new(&env::var("CARGO_MANIFEST_DIR").expect("Cargo must provide CARGO_MANIFEST_DIR"))
            .join("Cargo.toml");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    (
        snapshot_identity::ledger(&manifest_path, "deno-core-version"),
        snapshot_identity::ledger(&manifest_path, "deno-runtime-version"),
    )
}

#[cfg(any(feature = "snapshot", feature = "snapshot-import"))]
fn install_prebuilt_snapshot(bundle: &Path, out_dir: &Path) {
    let identity = expected_snapshot_identity();
    println!("cargo:rerun-if-changed={}", bundle.display());
    let manifest_path = bundle.join(snapshot_bundle::MANIFEST_FILE);
    let text = std::fs::read_to_string(&manifest_path).unwrap_or_else(|e| {
        panic!(
            "libdeno: LIBDENO_SNAPSHOT_DIR={} has no readable {}: {e}\n\
             Build a bundle with the `snapshot` feature and LIBDENO_SNAPSHOT_EXPORT_DIR.",
            bundle.display(),
            snapshot_bundle::MANIFEST_FILE
        )
    });
    let manifest = snapshot_bundle::parse(&text)
        .unwrap_or_else(|e| panic!("libdeno: invalid {}: {e}", manifest_path.display()));
    snapshot_compat::verify(bundle, &identity.target, text.as_bytes())
        .unwrap_or_else(|e| panic!("libdeno: snapshot compatibility check failed: {e}"));
    let read = |name: &str| {
        std::fs::read(bundle.join(name)).unwrap_or_else(|e| {
            panic!(
                "libdeno: bundle {} is missing {name}: {e}",
                bundle.display()
            )
        })
    };
    let snapshot = read(snapshot_bundle::SNAPSHOT_FILE);
    let table_bytes = read(snapshot_bundle::TABLE_FILE);
    snapshot_bundle::verify(&manifest, &identity, &snapshot, &table_bytes).unwrap_or_else(|e| {
        panic!(
            "libdeno: the prebuilt snapshot in {} cannot be used: {e}\n\
             Build the bundle on a machine with this target and the same libdeno and \
             deno versions, or unset LIBDENO_SNAPSHOT_DIR.",
            bundle.display()
        )
    });
    let table = String::from_utf8_lossy(&table_bytes);
    snapshot_bundle::verify_residuals(&manifest, &table, bundle).unwrap_or_else(|e| {
        panic!(
            "libdeno: the prebuilt snapshot in {} cannot be used: {e}",
            bundle.display()
        )
    });
    std::fs::write(out_dir.join(snapshot_bundle::SNAPSHOT_FILE), &snapshot).unwrap();
    std::fs::write(out_dir.join(snapshot_bundle::TABLE_FILE), &table_bytes).unwrap();
    snapshot_bundle::copy_verified_residuals(&manifest, &table, bundle, out_dir)
        .unwrap_or_else(|e| panic!("libdeno: bundle {} {e}", bundle.display()));
    println!(
        "cargo:warning=libdeno: reused the prebuilt snapshot in {} (target {})",
        bundle.display(),
        identity.target
    );
}

#[cfg(feature = "snapshot")]
fn export_snapshot_bundle(bundle: &Path, snapshot_path: &Path, table_path: &Path) {
    std::fs::create_dir_all(bundle)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", bundle.display()));
    let snapshot = std::fs::read(snapshot_path).unwrap();
    let table_bytes = std::fs::read(table_path).unwrap();
    let table = String::from_utf8_lossy(&table_bytes);
    for name in snapshot_bundle::table_sources(&table) {
        let source = snapshot_path.parent().unwrap().join(&name);
        let target = bundle.join(&name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::copy(&source, &target).unwrap_or_else(|e| {
            panic!("failed to export residual source {}: {e}", source.display())
        });
    }
    std::fs::write(bundle.join(snapshot_bundle::SNAPSHOT_FILE), &snapshot).unwrap();
    std::fs::write(bundle.join(snapshot_bundle::TABLE_FILE), &table_bytes).unwrap();
    let mut manifest = snapshot_bundle::describe(&snapshot_identity(), &snapshot, &table_bytes);
    manifest.residuals =
        snapshot_bundle::describe_residuals(&table, snapshot_path.parent().unwrap())
            .unwrap_or_else(|e| panic!("failed to fingerprint exported residual source: {e}"));
    std::fs::write(
        bundle.join(snapshot_bundle::MANIFEST_FILE),
        snapshot_bundle::render(&manifest),
    )
    .unwrap();
    println!(
        "cargo:warning=libdeno: wrote a reusable snapshot bundle to {}",
        bundle.display()
    );
}
