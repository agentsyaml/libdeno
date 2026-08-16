//! Cross-run analysis caches.
//!
//! Every `run`/`run_with` rebuilds its module graph, and deno_graph
//! re-parses + re-analyzes every module in the transitive graph each time —
//! the dominant warm-run cost after the resolver stack was made reusable.
//! The CLI wires caches into the two seams deno_graph exposes:
//!
//! - `ModuleAnalyzer` (per module analysis) + `ModuleInfoCacher` (incremental
//!   update notifications), backed by a process-level cache keyed by
//!   (specifier, source hash);
//! - the resolver stack's `NodeAnalysisCache` / `NodeResolutionCache` /
//!   `PackageJsonCache` seams, which cache CJS analysis and node resolution
//!   results.
//!
//! These caches are process-global singletons (see [`module_info_cache`] /
//! [`node_analysis_cache`]): they survive across runs of the same process on
//! *every* entry point — `run` builds a fresh `SharedServices` per call, so
//! an instance stored on the services would be rebuilt (and emptied) with it.
//! Content-hash keying makes process-global sharing safe: a changed source
//! hashes differently and never hits a stale entry. All are size-capped (env
//! `LIBDENO_ANALYSIS_CACHE_ENTRIES`, default 8192; on overflow the cache
//! clears — a simple reset is cheaper and more predictable than an LRU for
//! this hot path, and a clear only costs the next run one rebuild).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use deno_error::JsErrorBox;
use deno_graph::analysis::ModuleAnalyzer;
use deno_graph::analysis::ModuleInfo;
use deno_graph::ast::DefaultModuleAnalyzer;
use deno_graph::source::ModuleInfoCacher;
use deno_graph::ModuleSpecifier;
use deno_media_type::MediaType;

/// Entry cap for every cache in this module; env-overridable so hosts with
/// very large projects can size up (or down to bound memory).
fn cache_capacity() -> usize {
    const DEFAULT: usize = 8192;
    static CAPACITY: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAPACITY.get_or_init(|| {
        std::env::var("LIBDENO_ANALYSIS_CACHE_ENTRIES")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT)
    })
}

/// A 64-bit source-content hash. DefaultHasher's output is not guaranteed
/// stable across Rust releases, but this cache lives entirely inside one
/// process, so only same-process consistency is required.
fn source_hash(source: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(source);
    h.finish()
}

/// Hash for the module-info cache: folds the media type in with the source
/// content (deno CLI's cache keys on (specifier, media_type, source_hash)).
/// For extensionless remote modules the media type comes from the response
/// content-type, which a server could change for identical bytes; without
/// this, analysis parsed under one type could be served under another. The
/// Debug encoding of MediaType is unique per variant (its own
/// `as_ts_extension` maps Json and Jsonc to the same string) and needs no
/// maintenance when upstream adds variants.
fn module_info_hash(media_type: MediaType, source: &[u8]) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    h.write(format!("{media_type:?}").as_bytes());
    h.write(source);
    h.finish()
}

/// deno_graph's analyzer and cacher in one object: `analyze` consults the
/// cache first (keyed by specifier + source hash) and falls back to the
/// default SWC-based analyzer, storing the result; `cache_module_info`
/// refreshes entries when the graph updates a module in place.
///
/// One lock guards the map with short critical sections (lookup / insert),
/// never across an await — concurrent `analyze` calls (deno_graph builds
/// modules concurrently) serialize only on the map access.
pub struct ModuleInfoCache {
    entries: Mutex<HashMap<ModuleSpecifier, (u64, ModuleInfo)>>,
}

impl Default for ModuleInfoCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ModuleInfoCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

/// Process-global module analysis cache; every run path shares it (see
/// module docs).
pub fn module_info_cache() -> Arc<ModuleInfoCache> {
    static CACHE: OnceLock<Arc<ModuleInfoCache>> = OnceLock::new();
    CACHE
        .get_or_init(|| Arc::new(ModuleInfoCache::new()))
        .clone()
}

#[async_trait::async_trait(?Send)]
impl ModuleAnalyzer for ModuleInfoCache {
    async fn analyze(
        &self,
        specifier: &ModuleSpecifier,
        source: Arc<str>,
        media_type: MediaType,
    ) -> Result<ModuleInfo, JsErrorBox> {
        let hash = module_info_hash(media_type, source.as_bytes());
        if let Some((cached_hash, info)) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(specifier)
        {
            if *cached_hash == hash {
                return Ok(info.clone());
            }
        }
        let info = DefaultModuleAnalyzer
            .analyze(specifier, source, media_type)
            .await?;
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= cache_capacity() {
            entries.clear(); // ponytail: full reset, not LRU; see module docs
        }
        entries.insert(specifier.clone(), (hash, info.clone()));
        Ok(info)
    }
}

impl ModuleInfoCacher for ModuleInfoCache {
    fn cache_module_info(
        &self,
        specifier: &ModuleSpecifier,
        media_type: MediaType,
        source: &Arc<[u8]>,
        module_info: &ModuleInfo,
    ) {
        let hash = module_info_hash(media_type, source);
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= cache_capacity() {
            entries.clear();
        }
        entries.insert(specifier.clone(), (hash, module_info.clone()));
    }
}

/// In-memory `NodeAnalysisCache`: caches CJS-vs-ESM analysis per
/// (specifier, source hash), so npm packages are not re-analyzed on every
/// run of the same project. deno_resolver ships only a `NullNodeAnalysisCache`
/// and the CLI's disk-backed variant, so this is the embedded-runtime
/// equivalent (memory-only, size-capped).
pub struct MemoryNodeAnalysisCache {
    entries: Mutex<HashMap<(ModuleSpecifier, u64), DenoCjsAnalysis>>,
}

impl Default for MemoryNodeAnalysisCache {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryNodeAnalysisCache {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

/// Process-global CJS analysis cache (see module docs).
pub fn node_analysis_cache() -> Arc<MemoryNodeAnalysisCache> {
    static CACHE: OnceLock<Arc<MemoryNodeAnalysisCache>> = OnceLock::new();
    CACHE
        .get_or_init(|| Arc::new(MemoryNodeAnalysisCache::new()))
        .clone()
}

impl deno_resolver::cjs::analyzer::NodeAnalysisCache for MemoryNodeAnalysisCache {
    fn compute_source_hash(&self, source: &str) -> NodeAnalysisCacheSourceHash {
        NodeAnalysisCacheSourceHash(source_hash(source.as_bytes()))
    }

    fn get_cjs_analysis(
        &self,
        specifier: &url::Url,
        source_hash: NodeAnalysisCacheSourceHash,
    ) -> Option<DenoCjsAnalysis> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(specifier.clone(), source_hash.0))
            .cloned()
    }

    fn set_cjs_analysis(
        &self,
        specifier: &url::Url,
        source_hash: NodeAnalysisCacheSourceHash,
        analysis: &DenoCjsAnalysis,
    ) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= cache_capacity() {
            entries.clear();
        }
        entries.insert((specifier.clone(), source_hash.0), analysis.clone());
    }
}

use deno_resolver::cjs::analyzer::DenoCjsAnalysis;
use deno_resolver::cjs::analyzer::NodeAnalysisCacheSourceHash;
