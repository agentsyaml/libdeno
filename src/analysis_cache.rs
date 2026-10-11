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
//! `ModuleInfoCache` is process-global; CJS analysis is resolver-owned because
//! upstream's CJS key omits package metadata such as `type`. The caches remain
//! memory-only and size-capped (env
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

/// Creates a CJS analysis cache for one resolver stack. Its lifetime must
/// match the resolver because package `type` affects analysis but is not part
/// of the upstream cache key.
pub fn node_analysis_cache() -> Arc<MemoryNodeAnalysisCache> {
    Arc::new(MemoryNodeAnalysisCache::new())
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
        // `Esm` is produced by SourceOnly mode and lacks exports. The cache
        // trait has no mode key, so retaining it could poison a later richer
        // SourceImportsAndExports analysis for the same source.
        if matches!(analysis, DenoCjsAnalysis::Esm) {
            return;
        }
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() >= cache_capacity() {
            entries.clear();
        }
        entries.insert((specifier.clone(), source_hash.0), analysis.clone());
    }
}

use deno_resolver::cjs::analyzer::DenoCjsAnalysis;
use deno_resolver::cjs::analyzer::NodeAnalysisCacheSourceHash;

#[cfg(test)]
mod tests {
    use super::*;
    use deno_resolver::cjs::analyzer::ModuleExportsAndReExports;
    use deno_resolver::cjs::analyzer::NodeAnalysisCache;

    #[test]
    fn does_not_cache_incomplete_esm_analysis() {
        let cache = MemoryNodeAnalysisCache::new();
        let specifier = ModuleSpecifier::parse("file:///mod.js").unwrap();
        let source = "export const value = 1;";
        let hash = cache.compute_source_hash(source);

        cache.set_cjs_analysis(&specifier, hash, &DenoCjsAnalysis::Esm);

        assert!(cache.get_cjs_analysis(&specifier, hash).is_none());
    }

    #[test]
    fn caches_complete_analyses_with_the_original_hash() {
        let cache = MemoryNodeAnalysisCache::new();
        let specifier = ModuleSpecifier::parse("file:///mod.js").unwrap();
        let source = "export const value = 1;";
        let hash = cache.compute_source_hash(source);
        let analysis = ModuleExportsAndReExports {
            exports: vec!["value".to_string()],
            reexports: vec!["./other.js".to_string()],
            member_reexports: vec![],
        };

        for expected in [
            DenoCjsAnalysis::Cjs(analysis.clone()),
            DenoCjsAnalysis::EsmAnalysis(analysis.clone()),
        ] {
            cache.set_cjs_analysis(&specifier, hash, &expected);
            assert_eq!(cache.get_cjs_analysis(&specifier, hash), Some(expected));
        }
        assert!(cache
            .get_cjs_analysis(&specifier, cache.compute_source_hash("changed"))
            .is_none());
    }

    #[test]
    fn old_resolver_package_type_analysis_does_not_reach_new_resolver() {
        let old_resolver_cache = node_analysis_cache();
        let new_resolver_cache = node_analysis_cache();
        let specifier = ModuleSpecifier::parse("file:///pkg/index.js").unwrap();
        let source = "module.exports = { value: 1 };";
        let hash = old_resolver_cache.compute_source_hash(source);
        let old_module_analysis = DenoCjsAnalysis::EsmAnalysis(ModuleExportsAndReExports {
            exports: vec!["value".to_string()],
            reexports: vec![],
            member_reexports: vec![],
        });

        // This complete result was produced under the old resolver's package
        // type. A new resolver must not see it for the same URL and bytes.
        old_resolver_cache.set_cjs_analysis(&specifier, hash, &old_module_analysis);

        assert_eq!(
            old_resolver_cache.get_cjs_analysis(&specifier, hash),
            Some(old_module_analysis)
        );
        assert!(new_resolver_cache
            .get_cjs_analysis(&specifier, hash)
            .is_none());
    }
}
