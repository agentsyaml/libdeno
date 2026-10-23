//! Discovery of extension sources that snapshot creation reads from disk.
//! Shared by `build.rs` (diagnostic) and `tests/snapshot_sources.rs` (invariant).

use std::path::Path;

use deno_runtime::deno_core::Extension;
#[allow(
    deprecated,
    reason = "matching the deprecated variant we still inspect"
)]
use deno_runtime::deno_core::ExtensionFileSourceCode;

/// Every extension source that is read from the filesystem at snapshot time.
///
/// The `extension!` macro records all of them by absolute path, so an
/// incomplete vendor copy of any deno crate surfaces here and not at compile
/// time.
pub fn fs_sources(extensions: &[Extension]) -> Vec<&'static str> {
    extensions
        .iter()
        .flat_map(|extension| {
            extension
                .js_files
                .iter()
                .chain(extension.esm_files.iter())
                .chain(extension.lazy_loaded_js_files.iter())
                .chain(extension.lazy_loaded_esm_files.iter())
        })
        .filter_map(|source| match &source.code {
            ExtensionFileSourceCode::LoadedFromFsDuringSnapshot(path) => Some(*path),
            _ => None,
        })
        .collect()
}

/// The subset of [`fs_sources`] that does not exist on this machine.
pub fn missing(extensions: &[Extension]) -> Vec<&'static str> {
    let mut missing: Vec<&'static str> = fs_sources(extensions)
        .into_iter()
        .filter(|path| !Path::new(path).is_file())
        .collect();
    missing.sort_unstable();
    missing.dedup();
    missing
}
