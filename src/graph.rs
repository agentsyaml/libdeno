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
use deno_npm_installer::graph::NpmDenoGraphResolver;
use deno_resolver::factory::ResolverFactory;
use deno_semver::package::PackageReq;
use node_resolver::NodeResolutionKind;
use node_resolver::ResolutionMode;
use sys_traits::impls::RealSys;

use crate::http::ReqwestHttpClient;
use crate::services::RealNpmInstallerFactory;

pub struct GraphResolver {
    resolver: deno_resolver::graph::DefaultDenoResolverRc<RealSys>,
    npm_resolver: Arc<NpmDenoGraphResolver<ReqwestHttpClient, RealSys>>,
}

impl GraphResolver {
    pub async fn new(
        resolver_factory: Arc<ResolverFactory<RealSys>>,
        npm_installer_factory: Arc<RealNpmInstallerFactory>,
    ) -> Result<Self, AnyError> {
        // Build the resolvers eagerly; the underlying raw resolver is also lazy
        // so constructing it here is cheap.
        let npm_resolver = npm_installer_factory
            .npm_deno_graph_resolver()
            .await?
            .clone();
        Ok(Self {
            resolver: resolver_factory.deno_resolver().await?.clone(),
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
        self.npm_resolver
            .load_and_cache_npm_package_info(package_name);
    }

    async fn resolve_pkg_reqs(&self, package_reqs: &[PackageReq]) -> NpmResolvePkgReqsResult {
        // In BYONM mode the installer is absent and npm: specifiers are
        // rejected; in managed mode this installs/resolves the packages.
        self.npm_resolver.resolve_pkg_reqs(package_reqs).await
    }
}
