// Manifest for a portable snapshot bundle.
//
// A bundle holds the V8 startup snapshot plus the residual lazy-extension
// sources that the runtime needs alongside it. It is valid only on the same
// target triple, V8 version, and declared dependency versions. Those version
// fields are not a binding to Cargo's resolved graph, features, or source
// patches; callers must use a matching snapshot-relevant dependency setup.

#![allow(dead_code)]

use std::path::Path;

pub const FORMAT: &str = "libdeno-snapshot-2";
pub const MANIFEST_FILE: &str = "libdeno-snapshot.manifest";
pub const SNAPSHOT_FILE: &str = "CLI_SNAPSHOT.bin";
pub const TABLE_FILE: &str = "EXTENSION_RESIDUAL_SOURCES.rs";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub libdeno: String,
    pub target: String,
    pub v8: String,
    pub typescript: String,
    pub deno_core: String,
    pub deno_runtime: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub identity: Identity,
    pub snapshot_bytes: usize,
    pub snapshot_hash: String,
    pub table_bytes: usize,
    pub table_hash: String,
    pub residuals: Vec<Residual>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Residual {
    pub name: String,
    pub bytes: usize,
    pub hash: String,
}

/// FNV-1a over the file bytes. This detects truncated or corrupted transfers;
/// it is not a signature. A bundle is executable input: only use one you
/// produced yourself or downloaded from the release you are building.
pub fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// The `OUT_DIR`-relative include path written into the residual table, with
/// `/` separators on every platform. A `\` would be escaped into the generated
/// Rust literal and then read back verbatim by `table_sources`, so a bundle
/// exported on Windows would name `a\b.js` on a Unix importer.
pub fn table_path(rel: &Path) -> String {
    format!("/{}", rel.to_string_lossy().replace('\\', "/"))
}

/// File names referenced by the generated residual table, as written into
/// `OUT_DIR`. The bundle must carry each of them.
pub fn table_sources(table: &str) -> Vec<String> {
    let mut sources = Vec::new();
    for line in table.lines() {
        if !line.contains("include_str!") {
            continue;
        }
        let Some(end) = line.rfind('"') else { continue };
        let Some(start) = line[..end].rfind('"') else {
            continue;
        };
        sources.push(line[start + 1..end].trim_start_matches('/').to_string());
    }
    sources
}

/// Rejects a residual name that could resolve outside the bundle directory.
///
/// A manifest is caller-supplied input, and both the verify and copy paths do
/// `dir.join(name)`. Without this check a name like `../../etc/passwd` reads
/// arbitrary files and copies them into `OUT_DIR`; a name that is absolute
/// makes `join` discard `dir` entirely. Only plain relative components are
/// accepted, and the separators are normalized first because the manifest is
/// portable text (`table_path` already writes `/` on every platform).
pub fn validate_residual_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("residual name is empty".into());
    }
    let normalized = name.replace('\\', "/");
    if normalized.starts_with('/') {
        return Err(format!("residual name {name:?} is absolute"));
    }
    if normalized
        .split('/')
        .any(|component| component == ".." || component == "." || component.is_empty())
    {
        return Err(format!(
            "residual name {name:?} is not a plain relative path"
        ));
    }
    if normalized.contains(':') {
        // Windows drive prefixes and alternate data streams.
        return Err(format!("residual name {name:?} contains ':'"));
    }
    Ok(())
}

pub fn missing_sources(table: &str, dir: &Path) -> Vec<String> {
    let mut missing: Vec<String> = table_sources(table)
        .into_iter()
        .filter(|name| !dir.join(name).is_file())
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

pub fn describe_residuals(table: &str, dir: &Path) -> Result<Vec<Residual>, String> {
    let mut names = table_sources(table);
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            validate_residual_name(&name)?;
            let bytes = std::fs::read(dir.join(&name))
                .map_err(|e| format!("cannot read residual source {name}: {e}"))?;
            Ok(Residual {
                name,
                bytes: bytes.len(),
                hash: fingerprint(&bytes),
            })
        })
        .collect()
}

pub fn verify_residuals(manifest: &Manifest, table: &str, dir: &Path) -> Result<(), String> {
    let mut expected: Vec<_> = table_sources(table);
    expected.sort();
    expected.dedup();
    let mut recorded: Vec<_> = manifest.residuals.iter().map(|r| r.name.clone()).collect();
    recorded.sort();
    recorded.dedup();
    if recorded != expected || recorded.len() != manifest.residuals.len() {
        return Err("residual source list does not match the residual table".into());
    }
    for residual in &manifest.residuals {
        validate_residual_name(&residual.name)?;
        let bytes = std::fs::read(dir.join(&residual.name))
            .map_err(|e| format!("missing residual source {}: {e}", residual.name))?;
        if bytes.len() != residual.bytes || fingerprint(&bytes) != residual.hash {
            return Err(format!(
                "residual source {} does not match the manifest",
                residual.name
            ));
        }
    }
    Ok(())
}

pub fn copy_verified_residuals(
    manifest: &Manifest,
    table: &str,
    source_dir: &Path,
    output_dir: &Path,
) -> Result<(), String> {
    for name in table_sources(table) {
        validate_residual_name(&name)?;
        let source = source_dir.join(&name);
        let target = output_dir.join(&name);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
        }
        std::fs::copy(&source, &target)
            .map_err(|error| format!("does not contain the residual source {name}: {error}"))?;
    }
    verify_residuals(manifest, table, output_dir)
}

pub fn render(manifest: &Manifest) -> String {
    let identity = &manifest.identity;
    format!(
        "format={FORMAT}\n\
         libdeno={}\n\
         target={}\n\
         v8={}\n\
         typescript={}\n\
         deno-core={}\n\
         deno-runtime={}\n\
         snapshot-bytes={}\n\
         snapshot-hash={}\n\
         table-bytes={}\n\
         table-hash={}\n{}",
        identity.libdeno,
        identity.target,
        identity.v8,
        identity.typescript,
        identity.deno_core,
        identity.deno_runtime,
        manifest.snapshot_bytes,
        manifest.snapshot_hash,
        manifest.table_bytes,
        manifest.table_hash,
        manifest
            .residuals
            .iter()
            .map(|r| format!("residual={}\t{}\t{}\n", r.name, r.bytes, r.hash))
            .collect::<String>(),
    )
}

pub fn describe(identity: &Identity, snapshot: &[u8], table: &[u8]) -> Manifest {
    Manifest {
        identity: identity.clone(),
        snapshot_bytes: snapshot.len(),
        snapshot_hash: fingerprint(snapshot),
        table_bytes: table.len(),
        table_hash: fingerprint(table),
        residuals: Vec::new(),
    }
}

pub fn parse(text: &str) -> Result<Manifest, String> {
    let mut fields = std::collections::BTreeMap::new();
    let mut residuals = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {} is not key=value: {line:?}", index + 1));
        };
        if key == "residual" {
            let mut parts = value.split('\t');
            let name = parts.next().unwrap_or_default();
            let bytes = parts.next().and_then(|v| v.parse().ok());
            let hash = parts.next();
            if name.is_empty() || bytes.is_none() || hash.is_none() || parts.next().is_some() {
                return Err("invalid residual manifest entry".into());
            }
            residuals.push(Residual {
                name: name.into(),
                bytes: bytes.unwrap(),
                hash: hash.unwrap().into(),
            });
            continue;
        }
        match key {
            "format" | "libdeno" | "target" | "v8" | "typescript" | "deno-core"
            | "deno-runtime" | "snapshot-bytes" | "snapshot-hash" | "table-bytes"
            | "table-hash" => {
                fields.insert(key.to_string(), value.to_string());
            }
            other => return Err(format!("unknown manifest key {other:?}")),
        }
    }
    let get = |key: &str| {
        fields
            .get(key)
            .cloned()
            .ok_or_else(|| format!("manifest is missing {key}"))
    };
    if get("format")? != FORMAT {
        return Err(format!(
            "manifest format {} is not {FORMAT}; regenerate the bundle",
            get("format")?
        ));
    }
    let number = |key: &str| -> Result<usize, String> {
        get(key)?
            .parse::<usize>()
            .map_err(|_| format!("manifest {key} is not a byte count"))
    };
    Ok(Manifest {
        identity: Identity {
            libdeno: get("libdeno")?,
            target: get("target")?,
            v8: get("v8")?,
            typescript: get("typescript")?,
            deno_core: get("deno-core")?,
            deno_runtime: get("deno-runtime")?,
        },
        snapshot_bytes: number("snapshot-bytes")?,
        snapshot_hash: get("snapshot-hash")?,
        table_bytes: number("table-bytes")?,
        table_hash: get("table-hash")?,
        residuals,
    })
}

pub fn verify(
    manifest: &Manifest,
    expected: &Identity,
    snapshot: &[u8],
    table: &[u8],
) -> Result<(), String> {
    let found = &manifest.identity;
    for (field, bundle, local) in [
        ("libdeno", &found.libdeno, &expected.libdeno),
        ("target", &found.target, &expected.target),
        ("v8", &found.v8, &expected.v8),
        ("typescript", &found.typescript, &expected.typescript),
        ("deno_core", &found.deno_core, &expected.deno_core),
        ("deno_runtime", &found.deno_runtime, &expected.deno_runtime),
    ] {
        if bundle != local {
            return Err(format!(
                "bundle was built with {field}={bundle}, this build needs {field}={local}"
            ));
        }
    }
    for (name, size, hash, bytes) in [
        (
            "snapshot",
            manifest.snapshot_bytes,
            &manifest.snapshot_hash,
            snapshot,
        ),
        (
            "residual table",
            manifest.table_bytes,
            &manifest.table_hash,
            table,
        ),
    ] {
        if bytes.len() != size || &fingerprint(bytes) != hash {
            return Err(format!(
                "{name} does not match the manifest (expected {size} bytes, hash {hash}; \
                 found {} bytes, hash {})",
                bytes.len(),
                fingerprint(bytes)
            ));
        }
    }
    Ok(())
}
#[cfg(test)]
#[path = "snapshot_bundle/tests.rs"]
mod tests;
