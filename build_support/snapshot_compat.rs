use std::collections::BTreeMap;
use std::path::Path;

pub const SIDECAR: &str = "libdeno-snapshot.sources";

fn fields(text: &str) -> Result<BTreeMap<&str, &str>, String> {
    let mut result = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line.split_once('=').ok_or("invalid compatibility record")?;
        if result.insert(key, value).is_some() {
            return Err(format!("duplicate compatibility field {key}"));
        }
    }
    Ok(result)
}

pub fn verify(bundle: &Path, target: &str, manifest: &[u8]) -> Result<(), String> {
    let declaration_path = std::env::var_os("LIBDENO_SNAPSHOT_COMPATIBILITY_FILE")
        .ok_or("snapshot import requires the snapshot compatibility wrapper; see scripts/snapshot_compat.py")?;
    let declaration_path = Path::new(&declaration_path);
    println!("cargo:rerun-if-env-changed=LIBDENO_SNAPSHOT_COMPATIBILITY_FILE");
    println!("cargo:rerun-if-changed={}", declaration_path.display());
    let text = std::fs::read_to_string(declaration_path)
        .map_err(|e| format!("cannot read compatibility declaration: {e}"))?;
    let declaration = fields(&text)?;
    if declaration.keys().any(|key| {
        ![
            "format",
            "mode",
            "target",
            "bundle",
            "source-hash",
            "manifest-hash",
        ]
        .contains(key)
    }) {
        return Err("unknown field in compatibility declaration".into());
    }
    if declaration.get("format") != Some(&"libdeno-snapshot-compat-1") {
        return Err("unsupported snapshot compatibility declaration".into());
    }
    if declaration.get("target") != Some(&target) {
        return Err("compatibility declaration target does not match this build".into());
    }
    if !matches!(
        declaration.get("mode"),
        Some(&"verified") | Some(&"assumed")
    ) {
        return Err("compatibility declaration has an invalid mode".into());
    }
    let declared_bundle = declaration
        .get("bundle")
        .ok_or("declaration is missing bundle")?;
    if Path::new(declared_bundle).canonicalize().ok().is_none()
        || Path::new(declared_bundle).canonicalize().ok() != bundle.canonicalize().ok()
    {
        return Err("compatibility declaration refers to a different bundle".into());
    }
    let source_hash = declaration
        .get("source-hash")
        .ok_or("declaration is missing source-hash")?;
    if source_hash.len() != 64 || !source_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("invalid source identity in compatibility declaration".into());
    }
    if declaration.get("manifest-hash").copied()
        != Some(crate::snapshot_bundle::fingerprint(manifest).as_str())
    {
        return Err("compatibility declaration is bound to a different bundle manifest".into());
    }
    let sidecar_path = bundle.join(SIDECAR);
    println!("cargo:rerun-if-changed={}", sidecar_path.display());
    let sidecar_text = match std::fs::read_to_string(&sidecar_path) {
        Ok(text) => text,
        Err(e) if declaration.get("mode") == Some(&"assumed") && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err("bundle has no source provenance; rerun with --assume-compatible only if its dependencies are known to match".into()),
        Err(e) => return Err(format!("cannot read source provenance sidecar: {e}")),
    };
    let sidecar = fields(&sidecar_text)?;
    if sidecar
        .keys()
        .any(|key| !["format", "target", "manifest-hash", "source-hash"].contains(key))
    {
        return Err("unknown field in source provenance sidecar".into());
    }
    if sidecar.get("format") != Some(&"libdeno-snapshot-sources-1")
        || sidecar.get("target") != Some(&target)
        || sidecar.get("source-hash") != Some(source_hash)
        || sidecar.get("manifest-hash").copied()
            != Some(crate::snapshot_bundle::fingerprint(manifest).as_str())
    {
        return Err(
            "bundle source provenance does not match this source tree, target, or manifest".into(),
        );
    }
    Ok(())
}
