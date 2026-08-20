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
// The key is cheap (<1ms: a few metadata() calls and small file reads), the
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
    /// Project .npmrc content hash, None when absent or unreadable. A hash —
    /// not (mtime, size) — so an edit preserving both still invalidates the
    /// cache.
    pub project_npmrc: Option<u64>,
    /// Global npmrc (`$HOME/.npmrc`) fingerprint. The canonical path is
    /// retained even when the file is absent; the content hash catches
    /// same-size/same-mtime edits in place.
    pub global_npmrc: Option<NpmrcFingerprint>,
    /// Effective default registry URL: NPM_CONFIG_REGISTRY (trailing-slash
    /// normalized) or the npm default. Registries set via a `registry=` line
    /// in the .npmrc files are covered by the two fingerprints above.
    pub registry: String,
}

/// Bounded fingerprint for the global npmrc. Only the path and a small-file
/// content hash are retained; no npmrc parser or unbounded data is added to
/// the cache key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct NpmrcFingerprint {
    pub path: PathBuf,
    pub content: Option<u64>,
}

/// Computes the cache key for a project cwd. Lightweight: metadata checks,
/// small file reads, and an env lookup are far below the cost of a
/// re-resolution. Canonicalizes so symlinked cwds (e.g. /var -> /private/var
/// on macOS) always map to one key.
pub fn compute_key(cwd: &Path) -> NpmCacheKey {
    let canonical = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    NpmCacheKey {
        cwd: canonical.clone(),
        node_modules_exists: canonical.join("node_modules").exists(),
        package_json: content_hash(&canonical.join("package.json")),
        project_npmrc: content_hash(&canonical.join(".npmrc")),
        global_npmrc: global_npmrc_fingerprint(),
        registry: NpmRegistryUrl::for_npm(&RealSys).url.to_string(),
    }
}

/// The global npmrc read by deno_resolver 0.88: `$HOME/.npmrc`.
fn global_npmrc_path() -> Option<PathBuf> {
    RealSys.env_home_dir().map(|h| h.join(".npmrc"))
}

fn global_npmrc_fingerprint() -> Option<NpmrcFingerprint> {
    let path = canonical_path(&global_npmrc_path()?);
    Some(NpmrcFingerprint {
        content: content_hash(&path),
        path,
    })
}

/// Canonicalizes an existing path and preserves a distinct absolute identity
/// for a missing path by canonicalizing its nearest existing parent.
fn canonical_path(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) {
        return path;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let Some(file_name) = absolute.file_name() else {
        return absolute;
    };
    absolute
        .parent()
        .and_then(|parent| std::fs::canonicalize(parent).ok())
        .map(|parent| parent.join(file_name))
        .unwrap_or(absolute)
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

/// Content hash of a small file, None when absent or unreadable.
///
/// DefaultHasher is non-cryptographic and its output is not guaranteed stable
/// across Rust versions or platforms, so this value is only comparable within
/// a single process. That is safe today: it is used solely for in-process
/// cache/config fingerprint comparisons (NpmCacheKey and runtime.rs's
/// config_fingerprint). If the fingerprint is ever persisted to disk or
/// compared across processes, switch to a stable hash (e.g. xxh3 or sha256).
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

    #[test]
    fn key_tracks_home_npmrc_content_and_ignores_userconfig() {
        static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let dir = std::env::temp_dir().join(format!("libdeno-npm-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let home_npmrc = home.join(".npmrc");
        let userconfig_a = dir.join("npmrc-a");
        let userconfig_b = dir.join("npmrc-b");
        let home_content_a = "registry=https://home-a.example/\n";
        let home_content_b = "registry=https://home-b.example/\n";
        let userconfig_content_a = "# userconfig-a\n";
        let userconfig_content_b = "# userconfig-b\n";
        assert_eq!(home_content_a.len(), home_content_b.len());
        assert_eq!(userconfig_content_a.len(), userconfig_content_b.len());
        std::fs::write(&home_npmrc, home_content_a).unwrap();
        std::fs::write(&userconfig_a, userconfig_content_a).unwrap();
        std::fs::write(&userconfig_b, userconfig_content_b).unwrap();

        let old_registry = std::env::var_os("NPM_CONFIG_REGISTRY");
        let old_userconfig = std::env::var_os("NPM_CONFIG_USERCONFIG");
        let old_home = std::env::var_os("HOME");
        std::env::remove_var("NPM_CONFIG_REGISTRY");
        std::env::set_var("HOME", &home);
        std::env::set_var("NPM_CONFIG_USERCONFIG", &userconfig_a);
        let first = compute_key(&dir);
        let first_global = first.global_npmrc.clone().unwrap();
        assert_eq!(first_global.path, canonical_path(&home_npmrc));
        assert!(first_global.content.is_some());

        std::env::set_var("NPM_CONFIG_USERCONFIG", &userconfig_b);
        let after_userconfig = compute_key(&dir);
        assert!(
            first == after_userconfig,
            "NPM_CONFIG_USERCONFIG must not change the resolver key"
        );

        // An in-place HOME .npmrc edit with the same size and restored mtime
        // must invalidate the key through the content hash even though the
        // unrelated NPM_CONFIG_USERCONFIG file changed independently.
        let before_len = std::fs::metadata(&home_npmrc).unwrap().len();
        let before_modified = modified_time(&home_npmrc);
        std::fs::write(&home_npmrc, home_content_b).unwrap();
        set_modified_time(&home_npmrc, before_modified);
        assert_eq!(before_len, std::fs::metadata(&home_npmrc).unwrap().len());
        assert_eq!(before_modified, modified_time(&home_npmrc));
        let after_home_edit = compute_key(&dir);
        let after_home_global = after_home_edit.global_npmrc.clone().unwrap();
        assert_eq!(after_home_global.path, canonical_path(&home_npmrc));
        assert_ne!(first_global.content, after_home_global.content);
        assert!(first != after_home_edit);

        // Project .npmrc content hashing remains independent of the global
        // npmrc fingerprint.
        let project_npmrc = dir.join(".npmrc");
        let project_content_a = "registry=https://project-a.example/\n";
        let project_content_b = "registry=https://project-b.example/\n";
        assert_eq!(project_content_a.len(), project_content_b.len());
        std::fs::write(&project_npmrc, project_content_a).unwrap();
        let before_project = compute_key(&dir);
        let before_project_len = std::fs::metadata(&project_npmrc).unwrap().len();
        let before_project_modified = modified_time(&project_npmrc);
        std::fs::write(&project_npmrc, project_content_b).unwrap();
        set_modified_time(&project_npmrc, before_project_modified);
        assert_eq!(
            before_project_len,
            std::fs::metadata(&project_npmrc).unwrap().len()
        );
        assert_eq!(before_project_modified, modified_time(&project_npmrc));
        let after_project = compute_key(&dir);
        assert_ne!(before_project.project_npmrc, after_project.project_npmrc);
        assert!(before_project != after_project);

        match old_registry {
            Some(value) => std::env::set_var("NPM_CONFIG_REGISTRY", value),
            None => std::env::remove_var("NPM_CONFIG_REGISTRY"),
        }
        match old_userconfig {
            Some(value) => std::env::set_var("NPM_CONFIG_USERCONFIG", value),
            None => std::env::remove_var("NPM_CONFIG_USERCONFIG"),
        }
        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    fn modified_time(path: &Path) -> std::time::SystemTime {
        std::fs::metadata(path).unwrap().modified().unwrap()
    }

    fn set_modified_time(path: &Path, modified: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }
}
