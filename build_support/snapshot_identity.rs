use std::path::Path;

pub fn declared_version(manifest: &str, key: &str) -> String {
    manifest
        .lines()
        .filter_map(|line| line.trim().strip_prefix(key))
        .find_map(|rest| {
            rest.strip_prefix(" = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .map(str::to_string)
        .unwrap_or_else(|| {
            panic!(
                "Cargo.toml is missing {key}; it belongs under [package.metadata.libdeno.upstream]"
            )
        })
}

pub fn ledger(manifest_path: &Path, key: &str) -> String {
    let manifest = std::fs::read_to_string(manifest_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", manifest_path.display()));
    declared_version(&manifest, key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_only_the_requested_ledger_value() {
        let ledger = "[package.metadata.libdeno.upstream]\nv8-version = \"15.0.245.2-rusty\"\n";
        assert_eq!(declared_version(ledger, "v8-version"), "15.0.245.2-rusty");
        assert!(std::panic::catch_unwind(|| declared_version(ledger, "missing")).is_err());
    }
}
