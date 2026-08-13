// Process-level npm resolution snapshot cache.
//
// A managed npm project without a deno.lock re-resolves its dependency graph
// from scratch on every run (deno_npm_installer's `resolve_npm_resolution_snapshot`
// callback returning None falls through to an empty resolution, so each npm
// import triggers registry metadata fetches). For such projects we keep the
// last resolved snapshot in-process, keyed by the project's identity: the
// canonical cwd plus fingerprints of package.json (content hash), the project
// .npmrc and the global npmrc, and the effective registry URL. A later run
// with an unchanged project reuses the snapshot and skips the re-resolution
// entirely; switching registries (NPM_CONFIG_REGISTRY, or a `registry=` line
// in either .npmrc) invalidates the key.
//
// Projects with a deno.lock are never cached here: they already reuse the
// on-disk lockfile via ResolveFromLockfile, so the cache would be dead weight.
//
// The key is cheap (<1ms: a few metadata() calls, one small file read), the
// capacity is bounded (FIFO of 8), and concurrent misses just re-resolve
// (last-wins insert).

use std::hash::Hash;
use std::hash::Hasher;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

use deno_npm::resolution::ValidSerializedNpmResolutionSnapshot;
use deno_npmrc::NpmRegistryUrl;
use sys_traits::impls::RealSys;
use sys_traits::EnvHomeDir;

/// Identity of a lockfile-free npm project for snapshot caching.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct NpmCacheKey {
    /// Canonical project cwd.
    pub cwd: PathBuf,
    /// Whether a node_modules directory exists at the project root.
    pub node_modules_exists: bool,
    /// package.json content hash, None when absent. A hash — not (mtime,
    /// size) — so an edit preserving both still invalidates the cache.
    pub package_json: Option<u64>,
    /// Project .npmrc (mtime, size) fingerprint, None when absent.
    pub project_npmrc: Option<(u64, u64)>,
    /// Global npmrc ($NPM_CONFIG_USERCONFIG, else ~/.npmrc) fingerprint, None
    /// when absent. Covers registry/scope switches made outside the project.
    pub global_npmrc: Option<(u64, u64)>,
    /// Effective default registry URL: NPM_CONFIG_REGISTRY (trailing-slash
    /// normalized) or the npm default. Registries set via a `registry=` line
    /// in the .npmrc files are covered by the two fingerprints above.
    pub registry: String,
}

/// Computes the cache key for a project cwd. Lightweight: two stats, one
/// small file read and an env lookup, far below the cost of a re-resolution.
/// Canonicalizes so symlinked cwds (e.g. /var -> /private/var on macOS)
/// always map to one key.
pub fn compute_key(cwd: &Path) -> NpmCacheKey {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    NpmCacheKey {
        cwd: canonical.clone(),
        node_modules_exists: canonical.join("node_modules").exists(),
        package_json: content_hash(&canonical.join("package.json")),
        project_npmrc: stat_fingerprint(&canonical.join(".npmrc")),
        global_npmrc: global_npmrc_path().and_then(|p| stat_fingerprint(&p)),
        registry: NpmRegistryUrl::for_npm(&RealSys).url.to_string(),
    }
}

/// The global npmrc file: $NPM_CONFIG_USERCONFIG when set, else ~/.npmrc.
fn global_npmrc_path() -> Option<PathBuf> {
    std::env::var_os("NPM_CONFIG_USERCONFIG")
        .map(PathBuf::from)
        .or_else(|| RealSys.env_home_dir().map(|h| h.join(".npmrc")))
}

/// FIFO cache of recent snapshots; bounded so a long-lived process serving
/// many projects does not grow without limit. The mutex only guards the map
/// operations; key computation and resolution stay outside the lock.
static CACHE: OnceLock<Mutex<Vec<(NpmCacheKey, ValidSerializedNpmResolutionSnapshot)>>> =
    OnceLock::new();
const MAX_ENTRIES: usize = 8;

fn cache() -> &'static Mutex<Vec<(NpmCacheKey, ValidSerializedNpmResolutionSnapshot)>> {
    CACHE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Returns a clone of the snapshot cached for `key`, if any.
pub fn get(key: &NpmCacheKey) -> Option<ValidSerializedNpmResolutionSnapshot> {
    let entries = cache().lock().unwrap_or_else(|e| e.into_inner());
    entries
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, snapshot)| snapshot.clone())
}

/// Caches `snapshot` under `key`, replacing an existing entry with the same
/// key. On overflow the oldest entry is dropped (FIFO).
pub fn insert(key: NpmCacheKey, snapshot: ValidSerializedNpmResolutionSnapshot) {
    let mut entries = cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(entry) = entries.iter_mut().find(|(k, _)| *k == key) {
        entry.1 = snapshot;
        return;
    }
    entries.push((key, snapshot));
    if entries.len() > MAX_ENTRIES {
        entries.remove(0);
    }
}

/// (mtime, size) fingerprint of a file, None when absent or unreadable.
fn stat_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64;
    Some((mtime, meta.len()))
}

/// Content hash of a small file, None when absent or unreadable.
///
/// DefaultHasher is non-cryptographic and its output is not guaranteed stable
/// across Rust versions or platforms, so this value is only comparable within
/// a single process. That is safe today: it is used solely for in-process
/// config fingerprint comparison (runtime.rs's config_fingerprint). If the
/// fingerprint is ever persisted to disk or compared across processes, switch
/// to a stable hash (e.g. xxh3 or sha256).
pub(crate) fn content_hash(path: &Path) -> Option<u64> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut hasher);
    Some(hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_eviction_and_replace() {
        let key = |n: u64| NpmCacheKey {
            cwd: PathBuf::from(format!("/proj/{n}")),
            node_modules_exists: false,
            package_json: Some(n),
            project_npmrc: None,
            global_npmrc: None,
            registry: String::new(),
        };
        // 9 inserts into a capacity-8 cache: the oldest must be evicted.
        for n in 0..9 {
            insert(key(n), ValidSerializedNpmResolutionSnapshot::default());
        }
        assert!(get(&key(0)).is_none(), "oldest entry should be evicted");
        assert!(get(&key(8)).is_some(), "newest entry should be present");
        // Replacing an existing key must not grow the vec.
        insert(key(8), ValidSerializedNpmResolutionSnapshot::default());
        assert!(get(&key(8)).is_some());
        insert(key(9), ValidSerializedNpmResolutionSnapshot::default());
        assert!(get(&key(1)).is_none(), "next oldest should be evicted");
        assert!(get(&key(9)).is_some());
    }
}
