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
    jsx_config: deno_resolver::deno_json::JsxImportSourceConfigResolver,
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
            jsx_config: deno_resolver::deno_json::JsxImportSourceConfigResolver::from_compiler_options_resolver(
                resolver_factory.compiler_options_resolver()?,
            )?,
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
    fn default_jsx_import_source(&self, referrer: &url::Url) -> Option<String> {
        self.jsx_config.for_specifier(referrer).and_then(|config| {
            config
                .import_source
                .as_ref()
                .map(|source| source.specifier.clone())
        })
    }

    fn default_jsx_import_source_types(&self, referrer: &url::Url) -> Option<String> {
        self.jsx_config.for_specifier(referrer).and_then(|config| {
            config
                .import_source_types
                .as_ref()
                .map(|source| source.specifier.clone())
        })
    }

    fn jsx_import_source_module(&self, referrer: &url::Url) -> &str {
        self.jsx_config
            .for_specifier(referrer)
            .map(|config| config.module.as_str())
            .unwrap_or(deno_graph::source::DEFAULT_JSX_IMPORT_SOURCE_MODULE)
    }

    fn resolve(
        &self,
        specifier_text: &str,
        referrer_range: &Range,
        kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, ResolveError> {
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
            let mut result = npm_resolver.resolve_pkg_reqs(package_reqs).await;
            propagate_dep_graph_error(&mut result);
            return result;
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

/// deno_graph 0.111.0 stores `dep_graph_result` on the graph but uses each
/// per-request result to populate npm module slots. Associate an overall
/// install failure with only this batch, preserving existing request errors.
#[cfg(any(feature = "npm", test))]
pub(crate) fn propagate_dep_graph_error(result: &mut NpmResolvePkgReqsResult) {
    let Err(error) = &result.dep_graph_result else {
        return;
    };
    for request_result in &mut result.results {
        if request_result.is_ok() {
            *request_result = Err(deno_graph::NpmLoadError::PackageReqResolution(Arc::clone(
                error,
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    #[test]
    fn jsx_import_source_uses_referrers_workspace_settings() {
        let _guard = crate::npm_cache::resolver_env_test_lock().lock().unwrap();
        let dir = std::env::temp_dir().join(format!("libdeno-jsx-resolver-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("deno.json"), r#"{"lock":false,"imports":{"npm:shim":"./shim.js","jsr:shim":"./shim.js"},"compilerOptions":{"jsx":"react-jsx","jsxImportSource":"./runtime","jsxImportSourceTypes":"./types"}}"#).unwrap();
        std::fs::write(dir.join("shim.js"), "export const shim = true;").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let shared = runtime
            .block_on(crate::services::SharedServices::new(
                dir.clone(),
                vec![dir.clone()],
            ))
            .unwrap();
        let referrer = Url::from_file_path(dir.join("main.tsx")).unwrap();
        let resolver = &*shared.graph_resolver;
        assert_eq!(
            resolver.default_jsx_import_source(&referrer).as_deref(),
            Some("./runtime")
        );
        assert_eq!(
            resolver
                .default_jsx_import_source_types(&referrer)
                .as_deref(),
            Some("./types")
        );
        assert_eq!(resolver.jsx_import_source_module(&referrer), "jsx-runtime");
        let range = Range {
            specifier: referrer.clone(),
            range: deno_graph::PositionRange {
                start: deno_graph::Position::zeroed(),
                end: deno_graph::Position::zeroed(),
            },
            resolution_mode: None,
        };
        for raw in ["npm:shim", "jsr:shim"] {
            assert_eq!(
                deno_graph::source::Resolver::resolve(
                    resolver,
                    raw,
                    &range,
                    ResolutionKind::Execution
                )
                .unwrap(),
                Url::from_file_path(dir.join("shim.js")).unwrap()
            );
        }
        std::fs::write(dir.join("deno.json"), r#"{"lock":false,"compilerOptions":{"jsx":"react-jsxdev","jsxImportSource":"./runtime"}}"#).unwrap();
        let dev = runtime
            .block_on(crate::services::SharedServices::new(
                dir.clone(),
                vec![dir.clone()],
            ))
            .unwrap();
        assert_eq!(
            dev.graph_resolver.jsx_import_source_module(&referrer),
            "jsx-dev-runtime"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn overall_npm_failure_reaches_only_ok_requests_and_preserves_causes() {
        let overall_error: Arc<dyn deno_error::JsErrorClass> =
            Arc::new(JsErrorBox::generic("controlled install failure"));
        let individual_error: Arc<dyn deno_error::JsErrorClass> =
            Arc::new(JsErrorBox::generic("controlled request failure"));
        let mut result = NpmResolvePkgReqsResult {
            results: vec![
                Ok(()),
                Err(deno_graph::NpmLoadError::PackageReqResolution(
                    individual_error.clone(),
                )),
                Ok(()),
            ],
            dep_graph_result: Err(overall_error.clone()),
        };

        propagate_dep_graph_error(&mut result);

        assert!(Arc::ptr_eq(
            result.dep_graph_result.as_ref().unwrap_err(),
            &overall_error
        ));
        for index in [0, 2] {
            match &result.results[index] {
                Err(deno_graph::NpmLoadError::PackageReqResolution(error)) => {
                    assert!(Arc::ptr_eq(error, &overall_error));
                    assert_eq!(error.to_string(), "controlled install failure");
                }
                other => panic!("overall error was not attached to request: {other:?}"),
            }
        }
        match &result.results[1] {
            Err(deno_graph::NpmLoadError::PackageReqResolution(error)) => {
                assert!(Arc::ptr_eq(error, &individual_error));
            }
            other => panic!("individual request error was replaced: {other:?}"),
        }
    }

    #[test]
    fn successful_npm_batch_is_unchanged() {
        let mut result = NpmResolvePkgReqsResult {
            results: vec![Ok(())],
            dep_graph_result: Ok(()),
        };

        propagate_dep_graph_error(&mut result);

        assert!(result.results[0].is_ok());
        assert!(result.dep_graph_result.is_ok());
    }
}
