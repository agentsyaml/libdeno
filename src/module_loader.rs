// deno_core ModuleLoader backed by the official deno module graph pipeline.
//
//   prepare_load: build the module graph (deno_graph) rooted at the requested
//                 specifier. Resolution inside the graph uses the official
//                 deno resolver stack (DenoResolver + node resolver + BYONM
//                 npm resolver); fetching uses the FileFetcher (file: and
//                 remote https/http with the disk cache).
//   load:         read the prepared module from the graph via the
//                 deno_resolver ModuleLoader, which handles TypeScript
//                 transpilation, CJS -> ESM translation, JSON and WASM.
//
// Gains over the previous hand-rolled loader: CJS packages, remote modules,
// import maps from deno.json, jsr: and wasm come from the graph.

use std::borrow::Cow;
use std::future::Future;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use deno_core::ModuleLoadOptions;
use deno_core::ModuleLoadReferrer;
use deno_core::ModuleLoader;
use deno_core::ModuleResolveResponse;
use deno_core::ModuleSource;
use deno_core::ModuleSourceCode;
use deno_core::ModuleSpecifier;
use deno_core::ModuleType;
use deno_core::RequestedModuleType;
use deno_core::ResolutionKind;
use deno_error::JsErrorBox;
use deno_graph::BuildOptions;
use deno_graph::CheckJsOption;
use deno_graph::GraphKind;
use deno_graph::ModuleEntryRef;
use deno_graph::ModuleGraph;
use deno_graph::WalkOptions;
use deno_media_type::MediaType;
use deno_resolver::loader::LoadedModuleOrAsset;
use deno_resolver::loader::LoadedModuleSource;
use deno_resolver::loader::RequestedModuleType as ResolverRequestedModuleType;
use node_resolver::NodeResolutionKind;
use node_resolver::ResolutionMode;
use url::Url;

use deno_runtime::deno_permissions::PermissionsContainer;

use crate::node_loader::FsCjsAnalysisSourceProvider;
use crate::services::{RealFileFetcher, RealGraphLoader, RuntimeServices, SharedServices};
use crate::timing::{ExecutionTiming, Phase};

pub struct GraphModuleLoader {
    shared: Arc<SharedServices>,
    /// Per-run permission-bound fetch pipeline: the file fetcher (permission-
    /// gated reads), the graph loader and the module graph all come from the
    /// owning run/worker. Worker loaders use their own graph while reusing the
    /// resolver and cache state in `shared`.
    file_fetcher: Arc<RealFileFetcher>,
    graph_loader: Arc<RealGraphLoader>,
    graph: Arc<tokio::sync::Mutex<ModuleGraph>>,
    /// Canonical runtime cwd used when deno_core supplies no referrer.
    cwd: PathBuf,
    /// Live permissions container (shallow clone) used for permission-gated
    /// reads during CJS analysis — revocations stay honored; do NOT deep-clone.
    permissions: PermissionsContainer,
    timing: ExecutionTiming,
}

struct RuntimeJsrUrlProvider<'a>(&'a Url);

impl deno_graph::source::JsrUrlProvider for RuntimeJsrUrlProvider<'_> {
    fn url(&self) -> &Url {
        self.0
    }
}

impl GraphModuleLoader {
    fn normalize_node_builtin(url: ModuleSpecifier) -> Result<ModuleSpecifier, JsErrorBox> {
        use node_resolver::IsBuiltInNodeModuleChecker;
        if !deno_resolver::DenoIsBuiltInNodeModuleChecker.is_builtin_node_module(url.path()) {
            return Err(JsErrorBox::generic(format!("Unknown node builtin: {url}")));
        }
        let mut canonical = url;
        canonical.set_query(None);
        canonical.set_fragment(None);
        Ok(canonical)
    }

    fn validate_node_builtin_identity(url: &ModuleSpecifier) -> Result<(), JsErrorBox> {
        let canonical = Self::normalize_node_builtin(url.clone())?;
        if &canonical != url {
            return Err(JsErrorBox::generic(format!(
                "Non-canonical node builtin identity: {url}"
            )));
        }
        Ok(())
    }

    pub fn new(runtime: Arc<RuntimeServices>, permissions: PermissionsContainer) -> Self {
        let graph_loader = runtime.graph_loader.clone();
        let graph = runtime.graph.clone();
        Self::with_graph_loader_and_graph(
            runtime.clone(),
            permissions,
            runtime.file_fetcher.clone(),
            graph_loader,
            graph,
        )
    }

    /// Like [`new`], but binds graph builds to `graph_loader` instead of the
    /// run's loader. Web workers pass a loader built from their OWN
    /// permissions container, so a worker-triggered graph build is gated by
    /// the worker's grants, not the main run's — otherwise the main run's
    /// `--allow-read` scope leaks to workers that declared narrower
    /// permissions. This constructor also gives the worker a fresh
    /// `CodeOnly` graph; resolver, HTTP/file cache, analysis cache, and npm
    /// state remain shared through `RuntimeServices::shared`.
    pub fn with_graph_loader(
        runtime: Arc<RuntimeServices>,
        permissions: PermissionsContainer,
        file_fetcher: Arc<RealFileFetcher>,
        graph_loader: Arc<RealGraphLoader>,
    ) -> Self {
        let graph = Arc::new(tokio::sync::Mutex::new(ModuleGraph::new(
            GraphKind::CodeOnly,
        )));
        Self::with_graph_loader_and_graph(runtime, permissions, file_fetcher, graph_loader, graph)
    }

    fn with_graph_loader_and_graph(
        runtime: Arc<RuntimeServices>,
        permissions: PermissionsContainer,
        file_fetcher: Arc<RealFileFetcher>,
        graph_loader: Arc<RealGraphLoader>,
        graph: Arc<tokio::sync::Mutex<ModuleGraph>>,
    ) -> Self {
        let cwd = runtime
            .shared
            .resolver_factory
            .workspace_factory()
            .initial_cwd()
            .clone();
        Self {
            shared: runtime.shared.clone(),
            file_fetcher,
            graph_loader,
            graph,
            cwd,
            permissions,
            timing: runtime.timing.clone(),
        }
    }

    fn resolve_referrer(referrer: &str, cwd: &Path) -> Result<ModuleSpecifier, JsErrorBox> {
        // deno_core synchronous side-loads use "." instead of a referrer URL.
        if matches!(referrer, "" | ".") {
            return ModuleSpecifier::from_directory_path(cwd)
                .map_err(|_| JsErrorBox::generic("Invalid cwd for module resolution"));
        }
        ModuleSpecifier::parse(referrer)
            .map_err(|e| JsErrorBox::generic(format!("Invalid referrer: {e}")))
    }

    fn resolve_specifier(
        &self,
        specifier: &str,
        referrer: &str,
        kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, JsErrorBox> {
        if matches!(kind, ResolutionKind::MainModule) {
            let url = ModuleSpecifier::parse(specifier)
                .map_err(|e| JsErrorBox::generic(format!("Invalid main module URL: {e}")))?;
            return if url.scheme() == "node" {
                Self::normalize_node_builtin(url)
            } else {
                Ok(url)
            };
        }

        // node: is resolved by deno_core's builtin module map. Other absolute
        // URLs go through the shared resolver so import maps apply consistently.
        if let Ok(url) = ModuleSpecifier::parse(specifier) {
            if url.scheme() == "node" {
                return Self::normalize_node_builtin(url);
            }
        }

        // jsr: and npm: specifiers are handled by the graph; pass them through.
        if specifier.starts_with("jsr:") || specifier.starts_with("npm:") {
            return ModuleSpecifier::parse(specifier)
                .map_err(|e| JsErrorBox::generic(format!("Invalid module specifier: {e}")));
        }

        let referrer_url = Self::resolve_referrer(referrer, &self.cwd)?;

        let mode = ResolutionMode::Import;

        let resolved = self
            .shared
            .graph_resolver
            .resolve(
                specifier,
                &referrer_url,
                mode,
                NodeResolutionKind::Execution,
            )
            .map_err(|e| JsErrorBox::generic(format!("Cannot resolve \"{specifier}\": {e}")))?;
        if resolved.scheme() == "node" {
            Self::normalize_node_builtin(resolved)
        } else {
            Ok(resolved)
        }
    }
}

impl ModuleLoader for GraphModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        kind: ResolutionKind,
    ) -> ModuleResolveResponse {
        self.resolve_specifier(specifier, referrer, kind)
    }

    fn prepare_load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<String>,
        _maybe_content: Option<String>,
        _options: ModuleLoadOptions,
    ) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>>>> {
        let services = self.shared.clone();
        let graph_loader = self.graph_loader.clone();
        let graph = self.graph.clone();
        let specifier = module_specifier.clone();
        let timing = self.timing.clone();
        Box::pin(async move {
            if matches!(specifier.scheme(), "node") {
                Self::validate_node_builtin_identity(&specifier)?;
                return Ok(());
            }
            build_graph(
                &services,
                graph_loader.as_ref(),
                &graph,
                &specifier,
                &timing,
            )
            .await
        })
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        maybe_referrer: Option<&ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> deno_core::ModuleLoadResponse {
        if module_specifier.scheme() == "node" {
            if let Err(error) = Self::validate_node_builtin_identity(module_specifier) {
                return deno_core::ModuleLoadResponse::Sync(Err(error));
            }
            // Empty JavaScript fallback; the real node builtin sources come from
            // the deno_node extension module map (snapshot).
            return deno_core::ModuleLoadResponse::Sync(Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String("".to_string().into()),
                module_specifier,
                None,
            )));
        }

        let services = self.shared.clone();
        let file_fetcher = self.file_fetcher.clone();
        let graph = self.graph.clone();
        let permissions = self.permissions.clone();
        let requested_specifier = module_specifier.clone();
        let maybe_referrer = maybe_referrer.map(|r| r.specifier.clone());
        let requested_module_type = options.requested_module_type.clone();

        deno_core::ModuleLoadResponse::Async(Box::pin(async move {
            // npm: specifiers (from package.json deps or bare imports) resolve to
            // the installed package file before loading, like the CLI does.
            let specifier = if let Ok(reference) =
                deno_semver::npm::NpmPackageReqReference::from_specifier(&requested_specifier)
            {
                let referrer = maybe_referrer
                    .clone()
                    .unwrap_or_else(|| requested_specifier.clone());
                services
                    .graph_resolver
                    .resolve_non_workspace_npm_req_ref_to_file(
                        &reference,
                        &referrer,
                        node_resolver::ResolutionMode::Import,
                        node_resolver::NodeResolutionKind::Execution,
                    )
                    .map_err(|e| {
                        JsErrorBox::generic(format!(
                            "Cannot resolve \"{requested_specifier}\": {e}"
                        ))
                    })?
                    .into_url()
                    .map_err(|e| {
                        JsErrorBox::generic(format!(
                            "Cannot resolve \"{requested_specifier}\": {e}"
                        ))
                    })?
            } else {
                requested_specifier.clone()
            };

            let graph_guard = graph.lock().await;
            let requested = as_deno_resolver_requested_module_type(&requested_module_type);
            let in_npm_pkg_checker = services
                .resolver_factory
                .in_npm_package_checker()
                .map_err(|e| JsErrorBox::generic(e.to_string()))?
                .clone();
            let loaded = services
                .resolver_factory
                .module_loader()
                .map_err(|e| JsErrorBox::generic(e.to_string()))?
                .load(
                    &graph_guard,
                    &specifier,
                    maybe_referrer.as_ref(),
                    &requested,
                    Some(&FsCjsAnalysisSourceProvider::new(
                        permissions.clone(),
                        in_npm_pkg_checker,
                    )),
                )
                .await
                .map_err(JsErrorBox::from_err)?;

            match loaded {
                LoadedModuleOrAsset::Module(module) => {
                    let code = match module.source {
                        LoadedModuleSource::ArcStr(text) => ModuleSourceCode::String(text.into()),
                        LoadedModuleSource::ArcBytes(bytes) => {
                            ModuleSourceCode::Bytes(bytes.into())
                        }
                        LoadedModuleSource::String(text) => {
                            ModuleSourceCode::String(text.into_owned().into())
                        }
                        LoadedModuleSource::Bytes(bytes) => ModuleSourceCode::Bytes(
                            deno_core::ModuleCodeBytes::Arc(bytes.into_owned().into()),
                        ),
                    };
                    let module_type = module_type_from_media_and_requested_type(
                        module.media_type,
                        &requested_module_type,
                    );
                    // The requested specifier may be an npm:/jsr: URL that resolved to
                    // the module's concrete URL; register both via the redirect alias.
                    Ok(ModuleSource::new_with_redirect(
                        module_type,
                        code,
                        &requested_specifier,
                        &module.specifier,
                        None,
                    ))
                }
                LoadedModuleOrAsset::ExternalAsset {
                    specifier: asset_specifier,
                    ..
                } => {
                    let asset_specifier = asset_specifier.into_owned();
                    // The graph guard was only needed for the module lookup
                    // above (the loader's result borrows the graph via its
                    // specifier Cow, now detached). Drop it before the asset
                    // fetch so a slow remote asset download does not hold the
                    // graph lock and block every other concurrent load/build.
                    drop(graph_guard);
                    // SECURITY: never use fetch_bypass_permissions here. An
                    // external asset is reachable from arbitrary JS imports and
                    // this fetch must honor the same live permissions container
                    // that gated this module load. Keep the unstable_*_imports
                    // flags OFF — this fix is what makes them safe to flip later.
                    let file = file_fetcher
                        .fetch(&asset_specifier, &permissions)
                        .await
                        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
                    let media_type = file.resolve_media_type_and_charset().0;
                    let module_type = module_type_from_media_and_requested_type(
                        media_type,
                        &requested_module_type,
                    );
                    Ok(ModuleSource::new(
                        module_type,
                        ModuleSourceCode::Bytes(file.source.into()),
                        &asset_specifier,
                        None,
                    ))
                }
            }
        }))
    }
}

/// Builds (or extends) the per-run module graph rooted at `specifier`.
async fn build_graph(
    shared: &SharedServices,
    graph_loader: &dyn deno_graph::source::Loader,
    graph: &tokio::sync::Mutex<ModuleGraph>,
    specifier: &ModuleSpecifier,
    timing: &ExecutionTiming,
) -> Result<(), JsErrorBox> {
    let _graph_build = timing.span(Phase::GraphBuild);
    let jsr_version_resolver = shared
        .resolver_factory
        .jsr_version_resolver()
        .map_err(|e| JsErrorBox::generic(e.to_string()))?;
    let jsr_url_provider = RuntimeJsrUrlProvider(&shared.jsr_url);
    let graph_resolver = shared.graph_resolver.clone();
    // ponytail: the graph lock covers the whole build below, including the
    // slow work (remote module fetches with 30s connect / 300s total timeouts,
    // npm resolve_pkg_reqs installs). It cannot be narrowed: ModuleGraph::build
    // takes &mut self and runs the loader/npm pipeline internally, interleaved
    // with graph mutation, so there is no public "fetch first, insert later"
    // split (and no graph merge API to build into a private graph). A RwLock
    // would not help either — build still needs the write side — and the
    // graph field type is fixed in services.rs. A slow module therefore still
    // serializes concurrent builds of *other* roots; revisit only if deno_graph
    // gains a lock-free incremental build API.
    let mut graph = graph.lock().await;
    let lockfile = shared.lockfile.as_ref();
    if let Some(lockfile) = lockfile {
        lockfile.fill_graph(&mut graph);
        lockfile
            .error_if_changed()
            .map_err(|error| JsErrorBox::generic(error.to_string()))?;
    }

    // Already loaded successfully by a previous prepare_load (e.g. an earlier
    // root built the whole transitive graph). Avoid rebuilding the tree — but
    // deno_graph records load failures in the graph's error table (not on the
    // module node), so "node exists" alone is not "loaded fine": a transient
    // failure would otherwise poison the module for the rest of the run and
    // every worker sharing the graph. Validate the reachable root before
    // taking the shortcut; a failure falls through to a fresh build/retry.
    if graph.get(specifier).is_some() {
        let failed = graph.module_errors().any(|e| e.specifier() == specifier);
        if !failed && validate_root(&graph, specifier).is_ok() {
            if let Some(lockfile) = lockfile {
                lockfile
                    .error_if_changed()
                    .map_err(|error| JsErrorBox::generic(error.to_string()))?;
            }
            return Ok(());
        }
    }

    let mut locker = lockfile.map(|lockfile| lockfile.as_deno_graph_locker());
    let options = BuildOptions {
        is_dynamic: false,
        skip_dynamic_deps: false,
        unstable_bytes_imports: false,
        unstable_text_imports: false,
        unstable_css_imports: false,
        unstable_config_imports: false,
        executor: Default::default(),
        locker: locker
            .as_mut()
            .map(|locker| locker as &mut dyn deno_graph::source::Locker),
        file_system: &shared.sys,
        jsr_url_provider: &jsr_url_provider,
        jsr_version_resolver: Cow::Borrowed(&**jsr_version_resolver),
        passthrough_jsr_specifiers: false,
        module_analyzer: &*shared.module_info_cache,
        module_info_cacher: &*shared.module_info_cache,
        npm_resolver: Some(&*graph_resolver),
        reporter: None,
        resolver: Some(&*graph_resolver),
        jsr_metadata_store: None,
    };
    let mut retry = vec![specifier.clone()];
    retry.extend(
        graph
            .walk(
                std::iter::once(specifier),
                WalkOptions {
                    check_js: CheckJsOption::True,
                    kind: GraphKind::CodeOnly,
                    follow_dynamic: false,
                    prefer_fast_check_graph: false,
                },
            )
            .filter(|(_, entry)| matches!(entry, ModuleEntryRef::Err(_)))
            .map(|(url, _)| url.clone()),
    );
    if graph.get(specifier).is_none() {
        retry.extend(
            graph
                .module_errors()
                .map(|e| e.specifier().clone())
                .filter(|url| url == specifier),
        );
    }
    retry = retry
        .into_iter()
        .map(|url| graph.resolve(&url).clone())
        .collect();
    retry.sort();
    retry.dedup();
    if retry
        .iter()
        .any(|url| graph.get(url).is_some() || graph.module_errors().any(|e| e.specifier() == url))
    {
        graph.reload(retry, graph_loader, options).await;
    } else {
        graph
            .build(vec![specifier.clone()], vec![], graph_loader, options)
            .await;
    }

    // ModuleGraph::build records load and resolution failures in the graph
    // instead of returning a Result. Validate only this root so unrelated
    // graph roots do not poison this prepare_load call. Keep the graph error
    // as the source error.
    validate_root(&graph, specifier)?;
    if let Some(lockfile) = lockfile {
        lockfile
            .error_if_changed()
            .map_err(|error| JsErrorBox::generic(error.to_string()))?;
    }
    Ok(())
}

fn validate_root(graph: &ModuleGraph, specifier: &ModuleSpecifier) -> Result<(), JsErrorBox> {
    graph
        .walk(
            std::iter::once(specifier),
            WalkOptions {
                check_js: CheckJsOption::True,
                kind: GraphKind::CodeOnly,
                follow_dynamic: false,
                prefer_fast_check_graph: false,
            },
        )
        .validate()
        .map_err(JsErrorBox::from_err)
}

fn as_deno_resolver_requested_module_type<'a>(
    requested: &'a RequestedModuleType,
) -> ResolverRequestedModuleType<'a> {
    match requested {
        RequestedModuleType::None => ResolverRequestedModuleType::None,
        RequestedModuleType::Json => ResolverRequestedModuleType::Json,
        RequestedModuleType::Text => ResolverRequestedModuleType::Text,
        RequestedModuleType::Bytes => ResolverRequestedModuleType::Bytes,
        RequestedModuleType::Other(ty) => ResolverRequestedModuleType::Other(ty),
    }
}

fn module_type_from_media_and_requested_type(
    media_type: MediaType,
    requested: &RequestedModuleType,
) -> ModuleType {
    match requested {
        RequestedModuleType::None => match media_type {
            MediaType::Wasm => ModuleType::Wasm,
            _ => ModuleType::JavaScript,
        },
        RequestedModuleType::Json => ModuleType::Json,
        RequestedModuleType::Text => ModuleType::Text,
        RequestedModuleType::Bytes => ModuleType::Bytes,
        RequestedModuleType::Other(ty) => ModuleType::Other(ty.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deno_graph::source::{LoadError, LoadFuture, LoadOptions, LoadResponse, Loader};
    use std::cell::{Cell, RefCell};
    use std::sync::Arc;
    use std::time::SystemTime;
    struct TestDir(std::path::PathBuf);
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    struct CountingLoader {
        a: ModuleSpecifier,
        b: ModuleSpecifier,
        sibling: ModuleSpecifier,
        requests: RefCell<Vec<ModuleSpecifier>>,
        b_calls: Cell<usize>,
    }
    impl Loader for CountingLoader {
        fn load(&self, specifier: &ModuleSpecifier, _options: LoadOptions) -> LoadFuture {
            self.requests.borrow_mut().push(specifier.clone());
            if specifier == &self.a {
                return Box::pin(std::future::ready(Ok(Some(LoadResponse::Redirect {
                    specifier: self.b.clone(),
                }))));
            }
            let content = if specifier == &self.b {
                self.b_calls.set(self.b_calls.get() + 1);
                if self.b_calls.get() == 1 {
                    return Box::pin(std::future::ready(Err(LoadError::Other(Arc::new(
                        JsErrorBox::generic("controlled first failure"),
                    )))));
                }
                b"export const retryValue = 42;".as_slice()
            } else if specifier == &self.sibling {
                b"export const sibling = 1;".as_slice()
            } else {
                return Box::pin(std::future::ready(Err(LoadError::Other(Arc::new(
                    JsErrorBox::generic(format!("unexpected loader request: {specifier}")),
                )))));
            };
            Box::pin(std::future::ready(Ok(Some(LoadResponse::Module {
                content: Arc::from(content),
                mtime: None,
                maybe_headers: None,
                specifier: specifier.clone(),
            }))))
        }
    }

    #[test]
    fn retry_reload_resolves_redirect_before_deduplicating_and_preserves_sibling() {
        let _semantic = crate::npm_cache::semantic_probe_test_lock().lock().unwrap();
        let _resolver_env = crate::npm_cache::resolver_env_test_lock().lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "libdeno-retry-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("deno.json"), r#"{"lock":false}"#).unwrap();
        let dir = TestDir(path);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cwd = dir.0.clone();
        let shared = runtime
            .block_on(crate::services::SharedServices::new(cwd.clone(), vec![cwd]))
            .unwrap();
        assert!(shared.lockfile.is_none());
        let [sibling, a, b] = ["s.js", "a.js", "b.js"]
            .map(|path| ModuleSpecifier::parse(&format!("http://127.0.0.1/{path}")).unwrap());
        let loader = CountingLoader {
            a: a.clone(),
            b: b.clone(),
            sibling: sibling.clone(),
            requests: RefCell::new(Vec::new()),
            b_calls: Cell::new(0),
        };
        let g = tokio::sync::Mutex::new(ModuleGraph::new(GraphKind::CodeOnly));
        let t = ExecutionTiming::disabled();
        runtime
            .block_on(build_graph(&shared, &loader, &g, &sibling, &t))
            .unwrap();
        assert!(runtime
            .block_on(build_graph(&shared, &loader, &g, &a, &t))
            .is_err());
        runtime.block_on(async {
            let graph = g.lock().await;
            assert_eq!(graph.resolve(&a), &b);
            assert!(graph
                .walk(
                    std::iter::once(&a),
                    WalkOptions {
                        check_js: CheckJsOption::True,
                        kind: GraphKind::CodeOnly,
                        follow_dynamic: false,
                        prefer_fast_check_graph: false
                    }
                )
                .any(|(_, entry)| matches!(entry, ModuleEntryRef::Err(_))));
        });
        runtime
            .block_on(build_graph(&shared, &loader, &g, &a, &t))
            .unwrap();
        runtime.block_on(async {
            let graph = g.lock().await;
            assert!(graph.get(&a).is_some() && graph.get(&b).is_some());
            validate_root(&graph, &a).unwrap();
            assert!(graph.get(&sibling).is_some());
            assert_eq!(loader.requests.into_inner(), vec![sibling, a, b.clone(), b]);
        });
    }

    #[test]
    fn module_type_maps_media_and_requested_type() {
        use deno_core::RequestedModuleType;
        use deno_media_type::MediaType;

        // No requested type: infer from media type (wasm vs JS).
        assert_eq!(
            module_type_from_media_and_requested_type(MediaType::Wasm, &RequestedModuleType::None,),
            ModuleType::Wasm
        );
        assert_eq!(
            module_type_from_media_and_requested_type(
                MediaType::JavaScript,
                &RequestedModuleType::None,
            ),
            ModuleType::JavaScript
        );
        assert_eq!(
            module_type_from_media_and_requested_type(
                MediaType::TypeScript,
                &RequestedModuleType::None,
            ),
            ModuleType::JavaScript
        );

        // Requested type wins over media type.
        assert_eq!(
            module_type_from_media_and_requested_type(
                MediaType::JavaScript,
                &RequestedModuleType::Json,
            ),
            ModuleType::Json
        );
        assert_eq!(
            module_type_from_media_and_requested_type(
                MediaType::JavaScript,
                &RequestedModuleType::Text,
            ),
            ModuleType::Text
        );
        assert_eq!(
            module_type_from_media_and_requested_type(
                MediaType::JavaScript,
                &RequestedModuleType::Bytes,
            ),
            ModuleType::Bytes
        );
    }

    #[test]
    fn requested_module_type_maps_between_apis() {
        use deno_core::RequestedModuleType;

        assert!(matches!(
            as_deno_resolver_requested_module_type(&RequestedModuleType::None),
            ResolverRequestedModuleType::None
        ));
        assert!(matches!(
            as_deno_resolver_requested_module_type(&RequestedModuleType::Json),
            ResolverRequestedModuleType::Json
        ));
        assert!(matches!(
            as_deno_resolver_requested_module_type(&RequestedModuleType::Text),
            ResolverRequestedModuleType::Text
        ));
        assert!(matches!(
            as_deno_resolver_requested_module_type(&RequestedModuleType::Bytes),
            ResolverRequestedModuleType::Bytes
        ));
    }

    #[test]
    fn empty_and_side_load_referrers_use_runtime_cwd() {
        let runtime_cwd = std::env::temp_dir().join("libdeno-runtime-cwd");
        let host_cwd = std::env::temp_dir().join("libdeno-host-cwd");
        let runtime_referrer = GraphModuleLoader::resolve_referrer("", &runtime_cwd).unwrap();
        let host_referrer = GraphModuleLoader::resolve_referrer("", &host_cwd).unwrap();

        assert_eq!(runtime_referrer.to_file_path().unwrap(), runtime_cwd);
        assert_ne!(runtime_referrer, host_referrer);
        assert_eq!(
            GraphModuleLoader::resolve_referrer(".", &runtime_cwd).unwrap(),
            runtime_referrer
        );
        assert!(GraphModuleLoader::resolve_referrer("invalid-relative", &runtime_cwd).is_err());

        let explicit = ModuleSpecifier::parse("file:///explicit/referrer.js").unwrap();
        assert_eq!(
            GraphModuleLoader::resolve_referrer(explicit.as_str(), &host_cwd).unwrap(),
            explicit
        );
    }

    #[test]
    fn node_builtin_urls_normalize_to_canonical_identity() {
        for input in ["node:path", "node:path?probe", "node:path#probe"] {
            let url = ModuleSpecifier::parse(input).unwrap();
            let canonical = GraphModuleLoader::normalize_node_builtin(url).unwrap();
            assert_eq!(canonical.as_str(), "node:path");
            GraphModuleLoader::validate_node_builtin_identity(&canonical).unwrap();
        }

        let suffixed = ModuleSpecifier::parse("node:path?probe").unwrap();
        assert!(GraphModuleLoader::validate_node_builtin_identity(&suffixed).is_err());
        let unknown = ModuleSpecifier::parse("node:not-a-real-builtin?probe").unwrap();
        assert!(GraphModuleLoader::normalize_node_builtin(unknown).is_err());
    }

    #[test]
    fn jsr_url_provider_uses_runtime_registry() {
        use deno_graph::source::JsrUrlProvider;

        let url = Url::parse("http://127.0.0.1:4545/").unwrap();
        let provider = RuntimeJsrUrlProvider(&url);
        assert_eq!(provider.url(), &url);
    }
}
