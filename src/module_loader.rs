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
use deno_media_type::MediaType;
use deno_resolver::loader::LoadedModuleOrAsset;
use deno_resolver::loader::LoadedModuleSource;
use deno_resolver::loader::RequestedModuleType as ResolverRequestedModuleType;
use node_resolver::NodeResolutionKind;
use node_resolver::ResolutionMode;

use deno_runtime::deno_permissions::PermissionsContainer;

use crate::node_loader::FsCjsAnalysisSourceProvider;
use crate::services::SharedServices;

pub struct GraphModuleLoader {
    services: Arc<SharedServices>,
    /// Live permissions container (shallow clone) used for permission-gated
    /// reads during CJS analysis — revocations stay honored; do NOT deep-clone.
    permissions: PermissionsContainer,
}

impl GraphModuleLoader {
    pub fn new(services: Arc<SharedServices>, permissions: PermissionsContainer) -> Self {
        Self {
            services,
            permissions,
        }
    }

    fn resolve_specifier(
        &self,
        specifier: &str,
        referrer: &str,
        kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, JsErrorBox> {
        if matches!(kind, ResolutionKind::MainModule) {
            return ModuleSpecifier::parse(specifier)
                .map_err(|e| JsErrorBox::generic(format!("Invalid main module URL: {e}")));
        }

        // Already absolute: file:, node:, data:.
        if let Ok(url) = ModuleSpecifier::parse(specifier) {
            if matches!(url.scheme(), "file" | "node" | "data") {
                return Ok(url);
            }
        }

        // jsr: and npm: specifiers are handled by the graph; pass them through.
        if specifier.starts_with("jsr:") || specifier.starts_with("npm:") {
            return ModuleSpecifier::parse(specifier)
                .map_err(|e| JsErrorBox::generic(format!("Invalid module specifier: {e}")));
        }

        let referrer_url = if referrer.is_empty() {
            ModuleSpecifier::from_directory_path(
                std::env::current_dir().map_err(|e| JsErrorBox::generic(e.to_string()))?,
            )
            .map_err(|_| JsErrorBox::generic("Invalid cwd for module resolution"))?
        } else {
            ModuleSpecifier::parse(referrer)
                .map_err(|e| JsErrorBox::generic(format!("Invalid referrer: {e}")))?
        };

        let mode = ResolutionMode::Import;

        self.services
            .graph_resolver
            .resolve(
                specifier,
                &referrer_url,
                mode,
                NodeResolutionKind::Execution,
            )
            .map_err(|e| JsErrorBox::generic(format!("Cannot resolve \"{specifier}\": {e}")))
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
        let services = self.services.clone();
        let specifier = module_specifier.clone();
        Box::pin(async move {
            if matches!(specifier.scheme(), "node") {
                // node: builtins come from the extension module map; nothing to load.
                return Ok(());
            }
            build_graph(&services, &specifier)
                .await
                .map_err(|e| JsErrorBox::generic(format!("Failed to load \"{specifier}\": {e}")))
        })
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        maybe_referrer: Option<&ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> deno_core::ModuleLoadResponse {
        if module_specifier.scheme() == "node" {
            // Empty JavaScript fallback; the real node builtin sources come from
            // the deno_node extension module map (snapshot).
            return deno_core::ModuleLoadResponse::Sync(Ok(ModuleSource::new(
                ModuleType::JavaScript,
                ModuleSourceCode::String("".to_string().into()),
                module_specifier,
                None,
            )));
        }

        let services = self.services.clone();
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

            let graph = services.graph.lock().await;
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
                    &graph,
                    &specifier,
                    maybe_referrer.as_ref(),
                    &requested,
                    Some(&FsCjsAnalysisSourceProvider::new(
                        permissions.clone(),
                        in_npm_pkg_checker,
                    )),
                )
                .await
                .map_err(|e| JsErrorBox::generic(e.to_string()))?;

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
                    drop(graph);
                    // SECURITY: never use fetch_bypass_permissions here. An
                    // external asset is reachable from arbitrary JS imports and
                    // this fetch must honor the same live permissions container
                    // that gated this module load. Keep the unstable_*_imports
                    // flags OFF — this fix is what makes them safe to flip later.
                    let file = services
                        .file_fetcher
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

/// Builds (or extends) the shared module graph rooted at `specifier`.
async fn build_graph(
    services: &SharedServices,
    specifier: &ModuleSpecifier,
) -> Result<(), deno_core::anyhow::Error> {
    let jsr_version_resolver = services.resolver_factory.jsr_version_resolver()?;
    let graph_resolver = services.graph_resolver.clone();
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
    let mut graph = services.graph.lock().await;

    // Already loaded successfully by a previous prepare_load (e.g. an earlier
    // root built the whole transitive graph). Avoid rebuilding the tree — but
    // deno_graph records load failures in the graph's error table (not on the
    // module node), so "node exists" alone is not "loaded fine": a transient
    // failure would otherwise poison the module for the rest of the run and
    // every worker sharing the graph. Retry only when the node exists without
    // an error for this specifier.
    if graph.get(specifier).is_some() {
        let failed = graph.module_errors().any(|e| e.specifier() == specifier);
        if !failed {
            return Ok(());
        }
    }

    graph
        .build(
            vec![specifier.clone()],
            vec![],
            &*services.graph_loader,
            BuildOptions {
                is_dynamic: false,
                skip_dynamic_deps: false,
                unstable_bytes_imports: false,
                unstable_text_imports: false,
                unstable_css_imports: false,
                unstable_config_imports: false,
                executor: Default::default(),
                locker: None,
                file_system: &services.sys,
                jsr_url_provider: &deno_graph::source::DefaultJsrUrlProvider,
                jsr_version_resolver: Cow::Borrowed(&**jsr_version_resolver),
                passthrough_jsr_specifiers: false,
                module_analyzer: &deno_graph::ast::DefaultModuleAnalyzer,
                module_info_cacher: &deno_graph::source::NullModuleInfoCacher,
                npm_resolver: Some(&*graph_resolver),
                reporter: None,
                resolver: Some(&*graph_resolver),
                jsr_metadata_store: None,
            },
        )
        .await;
    Ok(())
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
}
