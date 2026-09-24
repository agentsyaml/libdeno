// Implements deno_graph's `Resolver` and `NpmResolver` traits so the module
// graph can be built with the official deno resolver pipeline. This is the
// embedder glue the CLI keeps in cli/graph_util.rs (not published).

use std::sync::Arc;

use deno_core::error::AnyError;
use deno_error::JsErrorBox;
use deno_graph::source::NpmResolvePkgReqsResult;
use deno_graph::source::ResolutionKind;
use deno_graph::source::ResolveError;
use deno_graph::source::Resolver;
use deno_graph::ModuleSpecifier;
use deno_graph::Range;
#[cfg(feature = "npm")]
use deno_npm_installer::graph::NpmDenoGraphResolver;
use deno_resolver::factory::ResolverFactory;
use deno_semver::package::PackageReq;
use node_resolver::NodeResolutionKind;
use node_resolver::ResolutionMode;
use sys_traits::impls::RealSys;

#[cfg(feature = "npm")]
use crate::http::ReqwestHttpClient;
#[cfg(feature = "npm")]
use crate::services::RealNpmInstallerFactory;

pub struct GraphResolver {
    resolver: deno_resolver::graph::DefaultDenoResolverRc<RealSys>,
    #[cfg(feature = "npm")]
    npm_resolver: Option<Arc<NpmDenoGraphResolver<ReqwestHttpClient, RealSys>>>,
}

impl GraphResolver {
    pub async fn new(
        resolver_factory: Arc<ResolverFactory<RealSys>>,
        // With `npm` off the parameter is compiled out entirely (the npm
        // installer type does not exist in that configuration); callers pass
        // `()` as the second argument instead.
        #[cfg(feature = "npm")] npm_installer_factory: Option<Arc<RealNpmInstallerFactory>>,
        #[cfg(not(feature = "npm"))] _no_npm_marker: (),
    ) -> Result<Self, AnyError> {
        // Build the resolvers eagerly; the underlying raw resolver is also lazy
        // so constructing it here is cheap.
        #[cfg(feature = "npm")]
        let npm_resolver = match npm_installer_factory {
            Some(factory) => Some(factory.npm_deno_graph_resolver().await?.clone()),
            None => None,
        };
        Ok(Self {
            resolver: resolver_factory.deno_resolver().await?.clone(),
            #[cfg(feature = "npm")]
            npm_resolver,
        })
    }
}

impl std::fmt::Debug for GraphResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GraphResolver").finish()
    }
}

impl GraphResolver {
    /// Synchronous resolution for the deno_core module loader (outside of
    /// graph building).
    pub fn resolve(
        &self,
        raw_specifier: &str,
        referrer: &ModuleSpecifier,
        mode: ResolutionMode,
        kind: NodeResolutionKind,
    ) -> Result<ModuleSpecifier, JsErrorBox> {
        self.resolver
            .resolve(
                raw_specifier,
                referrer,
                deno_graph::Position::new(0, 0),
                mode,
                kind,
            )
            .map_err(|e| JsErrorBox::generic(format!("Cannot resolve \"{raw_specifier}\": {e}")))
    }

    /// Resolves an npm: specifier to its installed package file (managed mode).
    pub fn resolve_non_workspace_npm_req_ref_to_file(
        &self,
        npm_req_ref: &deno_semver::npm::NpmPackageReqReference,
        referrer: &ModuleSpecifier,
        mode: ResolutionMode,
        kind: NodeResolutionKind,
    ) -> Result<node_resolver::UrlOrPath, JsErrorBox> {
        self.resolver
            .resolve_non_workspace_npm_req_ref_to_file(npm_req_ref, referrer, mode, kind)
            .map_err(|e| JsErrorBox::generic(format!("Cannot resolve \"{npm_req_ref}\": {e}")))
    }
}

impl Resolver for GraphResolver {
    fn resolve(
        &self,
        specifier_text: &str,
        referrer_range: &Range,
        kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, ResolveError> {
        // jsr: and npm: specifiers are handled by the graph itself
        // (load_jsr_specifier / load_npm_specifier); pass them through as URLs.
        if specifier_text.starts_with("jsr:") || specifier_text.starts_with("npm:") {
            return ModuleSpecifier::parse(specifier_text)
                .map_err(|e| ResolveError::Other(deno_error::JsErrorBox::generic(e.to_string())));
        }
        let resolution_kind = match kind {
            ResolutionKind::Execution => NodeResolutionKind::Execution,
            ResolutionKind::Types => NodeResolutionKind::Types,
        };
        self.resolver
            .resolve(
                specifier_text,
                &referrer_range.specifier,
                referrer_range.range.start,
                ResolutionMode::Import,
                resolution_kind,
            )
            .map_err(|e| e.into_deno_graph_error())
    }
}

#[async_trait::async_trait(?Send)]
impl deno_graph::source::NpmResolver for GraphResolver {
    fn load_and_cache_npm_package_info(&self, package_name: &str) {
        #[cfg(feature = "npm")]
        if let Some(npm_resolver) = &self.npm_resolver {
            npm_resolver.load_and_cache_npm_package_info(package_name);
        }
        #[cfg(not(feature = "npm"))]
        {
            // npm installation machinery is not compiled into this build;
            // package info warm-up is a no-op.
            let _ = package_name;
        }
    }

    async fn resolve_pkg_reqs(&self, package_reqs: &[PackageReq]) -> NpmResolvePkgReqsResult {
        // With the `npm` feature this always delegates: `npm_resolver` is
        // `Some` in both managed and BYONM mode (BYONM is served by the same
        // `NpmDenoGraphResolver`).
        #[cfg(feature = "npm")]
        if let Some(npm_resolver) = &self.npm_resolver {
            return npm_resolver.resolve_pkg_reqs(package_reqs).await;
        }
        let _ = package_reqs;
        // npm support disabled in this build: fail fast with a clear error
        // instead of panicking.
        NpmResolvePkgReqsResult {
            results: package_reqs
                .iter()
                .map(|_| {
                    // Must be a per-req error: deno_graph's builder only
                    // surfaces `results` entries into the graph's module
                    // slots for static imports; `dep_graph_result` is stored
                    // on the graph but never read by this crate, so putting
                    // the message there would never reach the user.
                    Err(deno_graph::NpmLoadError::PackageReqResolution(Arc::new(
                        JsErrorBox::generic(
                            "npm support is disabled in this build (enable the `npm` feature)",
                        ),
                    )))
                })
                .collect(),
            // deno_graph calls this on every build even with no npm: imports;
            // an empty request list means the script is npm-free, so there is
            // nothing to fail and poisoning the graph's public
            // `npm_dep_graph_result` would be wrong.
            dep_graph_result: if package_reqs.is_empty() {
                Ok(())
            } else {
                Err(Arc::new(JsErrorBox::generic(
                    "npm support is disabled in this build (enable the `npm` feature)",
                )))
            },
        }
    }
}
