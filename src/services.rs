// Runtime services assembly: the official deno resolver pipeline
// (WorkspaceFactory + ResolverFactory + FileFetcher + NpmInstallerFactory)
// plus the shared module graph.
//
// npm mode follows the CLI default (`node_modules_dir: Auto`): if a
// node_modules directory exists at the project root it is used as-is
// (BYONM), otherwise packages are installed into it on demand (managed).
//
// This mirrors how the deno CLI composes its resolver stack; the CLI's
// glue lives in cli/graph_util.rs, which is not published, so we assemble
// the same pieces here.

use std::path::PathBuf;
use std::sync::Arc;

use deno_cache_dir::file_fetcher::CacheSetting;
use deno_cache_dir::file_fetcher::NullBlobStore;
use deno_config::deno_json::NodeModulesDirMode;
use deno_graph::GraphKind;
use deno_graph::ModuleGraph;
use deno_npm_installer::graph::NpmCachingStrategy;
use deno_npm_installer::lifecycle_scripts::LifecycleScriptsExecutorOptions;
use deno_npm_installer::LogReporter;
use deno_npm_installer::NpmInstallerFactory;
use deno_npm_installer::NpmInstallerFactoryOptions;
use deno_resolver::cjs::IsCjsResolutionMode;
use deno_resolver::deno_json::CompilerOptionsOverrides;
use deno_resolver::factory::ConfigDiscoveryOption;
use deno_resolver::factory::ResolverFactory;
use deno_resolver::factory::ResolverFactoryOptions;
use deno_resolver::factory::WorkspaceFactory;
use deno_resolver::factory::WorkspaceFactoryOptions;
use deno_resolver::file_fetcher::PermissionedFileFetcher;
use deno_resolver::file_fetcher::PermissionedFileFetcherOptions;
use deno_resolver::loader::AllowJsonImports;
use deno_resolver::loader::MemoryFiles;
use node_resolver::analyze::NodeCodeTranslatorMode;
use sys_traits::impls::RealSys;

use crate::graph::GraphResolver;
use crate::http::ReqwestHttpClient;

/// npm process state propagated to `child_process.fork` children (mirrors
/// deno_lib's `NpmProcessStateProvider` so the forked child can restore the
/// npm resolution snapshot).
#[derive(Debug)]
pub struct NpmProcessStateProviderImpl {
    kind: NpmProcessStateKind,
    local_node_modules_path: Option<String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
enum NpmProcessStateKind {
    Snapshot(deno_npm::resolution::SerializedNpmResolutionSnapshot),
    Byonm,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct NpmProcessState {
    kind: NpmProcessStateKind,
    local_node_modules_path: Option<String>,
}

impl deno_runtime::deno_process::NpmProcessStateProvider for NpmProcessStateProviderImpl {
    fn get_npm_process_state(&self) -> String {
        match deno_core::serde_json::to_string(&NpmProcessState {
            kind: self.kind.clone(),
            local_node_modules_path: self.local_node_modules_path.clone(),
        }) {
            Ok(json) => json,
            Err(e) => {
                eprintln!("libdeno: failed to serialize npm process state: {e}");
                "{}".to_string()
            }
        }
    }
}

/// Runs npm lifecycle scripts (preinstall/install/postinstall) through the
/// system shell, exactly like npm does (`sh -c <script>` with the npm_*
/// environment and the package's `.bin` on PATH). Scripts that need a
/// runtime (node, node-gyp, ...) must be available on PATH.
#[derive(Debug)]
pub struct ShellLifecycleScriptsExecutor;

#[async_trait::async_trait(?Send)]
impl deno_npm_installer::lifecycle_scripts::LifecycleScriptsExecutor
    for ShellLifecycleScriptsExecutor
{
    async fn execute(&self, options: LifecycleScriptsExecutorOptions<'_>) -> anyhow::Result<()> {
        let root_bin = options.root_node_modules_dir_path.join(".bin");
        for pkg in options.packages_with_scripts {
            for event in ["preinstall", "install", "postinstall"] {
                let Some(script) = pkg.scripts.get(event) else {
                    continue;
                };
                let cwd = pkg
                    .init_cwds
                    .first()
                    .map(|p| p.as_path())
                    .unwrap_or(pkg.package_folder.as_path());
                let pkg_bin = pkg.package_folder.join("node_modules").join(".bin");
                let path = format!(
                    "{}:{}:{}",
                    pkg_bin.display(),
                    root_bin.display(),
                    std::env::var("PATH").unwrap_or_default(),
                );
                let name = pkg.package.id.nv.name.as_str();
                let status = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(script)
                    .current_dir(cwd)
                    .env("PATH", &path)
                    .env("INIT_CWD", cwd)
                    .env("npm_lifecycle_event", event)
                    .env("npm_lifecycle_script", script)
                    .env("npm_package_name", name)
                    .env("npm_package_version", pkg.package.id.nv.version.to_string())
                    .status()
                    .await?;
                if !status.success() {
                    return Err(anyhow::anyhow!(
                        "lifecycle script `{event}` for package `{name}` failed with {status}"
                    ));
                }
            }
            (options.on_ran_pkg_scripts)(pkg.package)
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
        }
        Ok(())
    }
}

/// Builds the provider from the resolver factory's npm resolver, matching
/// deno_lib::npm::create_npm_process_state_provider.
pub fn create_npm_process_state_provider(
    resolver_factory: &Arc<ResolverFactory<RealSys>>,
) -> deno_core::anyhow::Result<deno_runtime::deno_process::NpmProcessStateProviderRc> {
    use deno_resolver::npm::NpmResolver;
    match resolver_factory.npm_resolver()? {
        NpmResolver::Managed(managed) => {
            let resolution = managed.resolution();
            Ok(deno_fs::sync::MaybeArc::new(NpmProcessStateProviderImpl {
                kind: NpmProcessStateKind::Snapshot(
                    resolution.serialized_valid_snapshot().into_serialized(),
                ),
                local_node_modules_path: managed
                    .root_node_modules_path()
                    .map(|p| p.to_string_lossy().to_string()),
            }))
        }
        NpmResolver::Byonm(byonm) => {
            Ok(deno_fs::sync::MaybeArc::new(NpmProcessStateProviderImpl {
                kind: NpmProcessStateKind::Byonm,
                local_node_modules_path: byonm
                    .root_node_modules_path()
                    .map(|p| p.to_string_lossy().to_string()),
            }))
        }
    }
}

pub type RealFileFetcher = PermissionedFileFetcher<NullBlobStore, RealSys, ReqwestHttpClient>;
pub type RealGraphLoader =
    deno_resolver::file_fetcher::DenoGraphLoader<NullBlobStore, RealSys, ReqwestHttpClient>;
pub type RealNpmInstallerFactory = NpmInstallerFactory<ReqwestHttpClient, LogReporter, RealSys>;

/// Thread-safe subset of `RuntimeServices` shared with web workers. The
/// npm installer factory is excluded: it holds a non-Send `Box<dyn Fn()>`
/// (the lockfile snapshot resolver), so only the resolver/graph pieces cross
/// worker boundaries.
pub struct SharedServices {
    pub sys: RealSys,
    pub resolver_factory: Arc<ResolverFactory<RealSys>>,
    pub file_fetcher: Arc<RealFileFetcher>,
    /// The official graph loader: FileFetcher wrapper implementing
    /// deno_graph::source::Loader.
    pub graph_loader: Arc<RealGraphLoader>,
    /// Shared module graph. `prepare_load` builds it, `load` reads from it.
    pub graph: Arc<tokio::sync::Mutex<ModuleGraph>>,
    /// Implements deno_graph's `Resolver` and `NpmResolver` traits for graph
    /// building.
    pub graph_resolver: Arc<GraphResolver>,
    /// npm process state for `child_process.fork` (npm snapshot propagation).
    pub npm_process_state_provider: deno_runtime::deno_process::NpmProcessStateProviderRc,
}

pub struct RuntimeServices {
    /// The Send+Sync core shared with the main worker and web workers.
    pub shared: Arc<SharedServices>,
}

impl RuntimeServices {
    pub async fn new(
        initial_cwd: PathBuf,
        config_start_paths: Vec<PathBuf>,
        permissions: deno_runtime::deno_permissions::PermissionsContainer,
    ) -> deno_core::anyhow::Result<Self> {
        let sys = RealSys;

        let workspace_factory = Arc::new(WorkspaceFactory::new(
            sys.clone(),
            initial_cwd,
            WorkspaceFactoryOptions {
                // Discover deno.json (import maps, jsx, etc.) walking up from the
                // main module's directory, like the CLI does.
                config_discovery: ConfigDiscoveryOption::Discover {
                    start_paths: config_start_paths,
                },
                // Auto == the CLI default: use the existing node_modules if present
                // (BYONM), otherwise install into it on demand (managed).
                node_modules_dir: Some(NodeModulesDirMode::Auto),
                ..Default::default()
            },
        ));

        let resolver_factory = Arc::new(ResolverFactory::new(
            workspace_factory.clone(),
            ResolverFactoryOptions {
                compiler_options_overrides: CompilerOptionsOverrides {
                    no_transpile: false,
                    force_check_js: false,
                    source_map_base: None,
                    preserve_jsx: false,
                    // Untyped execution cannot honor verbatim semantics safely.
                    force_disable_verbatim_module_syntax: true,
                },
                // CJS detection is Disabled by default in the CLI (only `deno node`
                // enables ImplicitTypeCommonJs). Ambiguous user files are treated as
                // ESM; CJS npm packages are still detected via the in_npm_package
                // branch of the CJS tracker.
                is_cjs_resolution_mode: IsCjsResolutionMode::Disabled,
                // Enable CJS -> ESM translation in the module loader.
                node_code_translator_mode: NodeCodeTranslatorMode::ModuleLoader,
                node_resolver_options: Default::default(),
                npm_system_info: Default::default(),
                allow_json_imports: AllowJsonImports::WithAttribute,
                require_modules: vec![],
                newest_dependency_date: None,
                node_analysis_cache: None,
                node_resolution_cache: None,
                package_json_cache: None,
                package_json_dep_resolution: None,
                specified_import_map: None,
                unstable_sloppy_imports: false,
                on_mapped_resolution_diagnostic: None,
            },
        ));

        // The installer factory is intentionally non-Send: it holds a
        // `Box<dyn Fn()>` (the lockfile snapshot resolver) and is used only
        // on the main worker thread; workers share the resolver/graph pieces
        // via `SharedServices` instead.
        #[allow(clippy::arc_with_non_send_sync)]
        let npm_installer_factory = Arc::new(NpmInstallerFactory::new(
            resolver_factory.clone(),
            Arc::new(ReqwestHttpClient::default()),
            Arc::new(ShellLifecycleScriptsExecutor),
            LogReporter,
            None,
            NpmInstallerFactoryOptions {
                cache_setting: deno_npm_cache::NpmCacheSetting::Use,
                caching_strategy: NpmCachingStrategy::Lazy,
                clean_on_install: false,
                dedup_lockfile_peer_variants: false,
                lifecycle_scripts_config: deno_npm_installer::LifecycleScriptsConfig {
                    // Match the deno CLI 2.x default: lifecycle scripts (preinstall /
                    // install / postinstall) do NOT run unless explicitly opted in.
                    // Running them by default would execute arbitrary shell code from
                    // npm dependencies with no opt-in surface in the embedder.
                    allowed: deno_npm_installer::PackagesAllowedScripts::None,
                    denied: vec![],
                    initial_cwd: Default::default(),
                    root_dir: Default::default(),
                    explicit_install: false,
                },
                production: false,
                skip_types: true,
                resolve_npm_resolution_snapshot: Box::new(|| Ok(None)),
            },
        ));

        let memory_files = Arc::new(MemoryFiles::default());
        let http_cache = Arc::new(workspace_factory.http_cache()?.clone());
        let file_fetcher = Arc::new(PermissionedFileFetcher::new(
            NullBlobStore,
            http_cache,
            ReqwestHttpClient::default(),
            memory_files,
            sys.clone(),
            PermissionedFileFetcherOptions {
                allow_remote: true,
                cache_setting: CacheSetting::Use,
            },
        ));

        let in_npm_pkg_checker = resolver_factory.in_npm_package_checker()?.clone();
        let global_http_cache = workspace_factory.global_http_cache()?.clone();
        let graph_loader = Arc::new(deno_resolver::file_fetcher::DenoGraphLoader::new(
            file_fetcher.clone(),
            global_http_cache,
            in_npm_pkg_checker,
            sys.clone(),
            deno_resolver::file_fetcher::DenoGraphLoaderOptions {
                file_header_overrides: Default::default(),
                include_npm_sources: false,
                // Live container (clone shares the Arc<Mutex<Permissions>>,
                // unlike deep_clone which forks a private copy) so
                // Deno.permissions.revoke is honored by graph fetches, matching
                // the CLI. Do NOT revert to deep_clone.
                permissions: Some(permissions.clone()),
                file_permission_api_name: None,
                reporter: None,
            },
        ));

        let graph = Arc::new(tokio::sync::Mutex::new(ModuleGraph::new(
            GraphKind::CodeOnly,
        )));
        // Load the lockfile/package.json npm snapshot so that managed npm
        // resolution is initialized before the graph asks for packages.
        npm_installer_factory
            .initialize_npm_resolution_if_managed()
            .await?;
        let graph_resolver = Arc::new(
            GraphResolver::new(resolver_factory.clone(), npm_installer_factory.clone()).await?,
        );
        let npm_process_state_provider = create_npm_process_state_provider(&resolver_factory)?;

        Ok(Self {
            shared: Arc::new(SharedServices {
                sys,
                resolver_factory,
                file_fetcher,
                graph_loader,
                graph,
                graph_resolver,
                npm_process_state_provider,
            }),
        })
    }
}
