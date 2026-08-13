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
use deno_cache_dir::GlobalHttpCacheRc;
use deno_cache_dir::GlobalOrLocalHttpCache;
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

/// Serialized npm resolution state handed to npm subprocesses (spawned via
/// child_process.fork). WARNING: when the registry URL is of the form
/// `https://user:pass@host/`, those credentials ride along in this serialized
/// string. The hand-off is same-process/same-user (not a privilege boundary),
/// but the string must never be logged or shipped across machines.
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
                // PATH is `;`-separated on Windows, `:`-separated elsewhere.
                let sep = if cfg!(windows) { ';' } else { ':' };
                let path = format!(
                    "{}{sep}{}{sep}{}",
                    pkg_bin.display(),
                    root_bin.display(),
                    std::env::var("PATH").unwrap_or_default(),
                );
                let name = pkg.package.id.nv.name.as_str();
                // npm lifecycle scripts run via sh on unix, cmd on Windows
                // (mirrors npm's own script-shell default per-platform).
                let mut cmd =
                    tokio::process::Command::new(if cfg!(windows) { "cmd" } else { "sh" });
                // /d skips AutoRun, /s keeps nested quotes intact (npm uses
                // `cmd /d /s /c`); on unix `sh -c`.
                #[cfg(windows)]
                cmd.args(["/d", "/s", "/C"]);
                #[cfg(not(windows))]
                cmd.arg("-c");
                let status = cmd
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
/// deno_lib::npm::create_npm_process_state_provider. The serialized snapshot
/// (see `NpmProcessState`) may embed registry credentials — the resulting
/// string is handed to forked npm child processes, so it must never be logged
/// or sent off-machine.
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

/// Resolves the project lockfile path exactly as deno_npm_installer's
/// maybe_lockfile does (deno_resolver::lockfile): deno.json's `lock` setting,
/// else `<package.json dir>/deno.lock`, else None.
fn resolve_lockfile_path(resolver_factory: &Arc<ResolverFactory<RealSys>>) -> Option<PathBuf> {
    resolver_factory
        .workspace_factory()
        .workspace_directory()
        .ok()
        .and_then(|dir| dir.workspace.resolve_lockfile_path().ok().flatten())
}

/// The permission-free half of the resolver pipeline, built once and reused
/// across runs (and web workers): the workspace/resolver factories, the
/// graph resolver, the npm process state provider, and the disk caches the
/// per-run file fetcher / graph loader borrow. The npm installer factory is
/// excluded — it holds a non-Send `Box<dyn Fn()>` (the lockfile snapshot
/// resolver) — and so is every permission-bound piece (the file fetcher, the
/// graph loader and the module graph are per-run, see `RuntimeServices`).
pub struct SharedServices {
    pub sys: RealSys,
    pub resolver_factory: Arc<ResolverFactory<RealSys>>,
    /// One reqwest client shared by the npm installer and every per-run file
    /// fetcher: reqwest::Client is Arc-backed, so all clones share the
    /// connection pool and the single builder config from http.rs.
    pub http_client: Arc<ReqwestHttpClient>,
    /// Disk-backed module caches; borrowed by the per-run file fetcher /
    /// graph loader (which are themselves per-run: they bind permissions).
    pub http_cache: Arc<GlobalOrLocalHttpCache<RealSys>>,
    pub global_http_cache: GlobalHttpCacheRc<RealSys>,
    /// In-memory virtual files (deno_resolver MemoryFiles) backing the
    /// per-run file fetcher.
    pub memory_files: Arc<MemoryFiles>,
    /// Implements deno_graph's `Resolver` and `NpmResolver` traits for graph
    /// building.
    pub graph_resolver: Arc<GraphResolver>,
    /// npm process state for `child_process.fork` (npm snapshot propagation).
    pub npm_process_state_provider: deno_runtime::deno_process::NpmProcessStateProviderRc,
}

impl SharedServices {
    /// Builds the permission-free resolver stack once: workspace/resolver
    /// factories, the shared http client and disk caches, the npm installer
    /// factory (including the Wave 1 snapshot-cache resolution callback), the
    /// initialized npm resolution, the graph resolver and the npm process
    /// state provider. The permission-bound file fetcher / graph loader /
    /// module graph are NOT built here — `RuntimeServices::new` rebuilds them
    /// per run so one run's grants can never leak into another.
    pub async fn new(
        initial_cwd: PathBuf,
        config_start_paths: Vec<PathBuf>,
    ) -> deno_core::anyhow::Result<Arc<Self>> {
        let sys = RealSys;

        let workspace_factory = Arc::new(WorkspaceFactory::new(
            sys.clone(),
            initial_cwd.clone(),
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

        // Lockfile path exactly as deno_npm_installer's maybe_lockfile
        // resolves it (deno_resolver::lockfile): deno.json's `lock` setting,
        // else `<package.json dir>/deno.lock`, else None. Projects with a
        // lockfile keep the original ResolveFromLockfile path; the in-process
        // snapshot cache only serves lockfile-free projects.
        let deno_lock_path = resolve_lockfile_path(&resolver_factory);

        // The installer factory is intentionally non-Send: it holds a
        // `Box<dyn Fn()>` (the lockfile snapshot resolver) and is used only
        // during stack construction here; the graph resolver and per-run
        // pieces share the resolver/graph state via `SharedServices` instead.
        let http_client = Arc::new(ReqwestHttpClient::new()?);
        #[allow(clippy::arc_with_non_send_sync)]
        let npm_installer_factory = Arc::new(NpmInstallerFactory::new(
            resolver_factory.clone(),
            http_client.clone(),
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
                // Lockfile-free managed projects reuse the last resolved
                // snapshot from the process-level cache; a lockfile project
                // returns None so deno_npm_installer goes through its
                // maybe_lockfile -> ResolveFromLockfile path unchanged.
                resolve_npm_resolution_snapshot: Box::new({
                    let lock_path = deno_lock_path.clone();
                    let cwd = initial_cwd.clone();
                    move || {
                        if lock_path.as_deref().is_some_and(|p| p.exists()) {
                            return Ok(None);
                        }
                        let key = crate::npm_cache::compute_key(&cwd);
                        Ok(crate::npm_cache::get(&key))
                    }
                }),
            },
        ));

        let memory_files = Arc::new(MemoryFiles::default());
        let http_cache = Arc::new(workspace_factory.http_cache()?.clone());
        let global_http_cache = workspace_factory.global_http_cache()?.clone();

        // Load the lockfile/package.json npm snapshot so that managed npm
        // resolution is initialized before the graph asks for packages. The
        // resolved snapshot is cached AFTER a successful run (see
        // RuntimeServices::save_npm_snapshot_cache): on a cache miss the
        // initializer returns Ok(None) without resolving anything, so caching
        // here would store an empty resolution forever.
        npm_installer_factory
            .initialize_npm_resolution_if_managed()
            .await?;
        let graph_resolver = Arc::new(
            GraphResolver::new(resolver_factory.clone(), npm_installer_factory.clone()).await?,
        );
        let npm_process_state_provider = create_npm_process_state_provider(&resolver_factory)?;

        Ok(Arc::new(Self {
            sys,
            resolver_factory,
            http_client,
            http_cache,
            global_http_cache,
            memory_files,
            graph_resolver,
            npm_process_state_provider,
        }))
    }
}

/// Per-run services: everything permission-bound that must be rebuilt for
/// every run so one run's grants can never leak into another. Cheap to build
/// (the expensive factory construction lives in [`SharedServices::new`]).
pub struct RuntimeServices {
    /// The permission-free resolver stack (shared with web workers).
    pub shared: Arc<SharedServices>,
    /// Canonical project cwd; the identity half of the npm snapshot cache key.
    cwd: PathBuf,
    /// Per-run file fetcher (permission-gated reads; the live permissions
    /// container is passed per fetch call).
    pub file_fetcher: Arc<RealFileFetcher>,
    /// Per-run graph loader: FileFetcher wrapper implementing
    /// deno_graph::source::Loader, bound to this run's permissions.
    pub graph_loader: Arc<RealGraphLoader>,
    /// Per-run module graph. `prepare_load` builds it, `load` reads from it.
    pub graph: Arc<tokio::sync::Mutex<ModuleGraph>>,
}

impl RuntimeServices {
    /// Saves the fully-resolved npm snapshot into the in-process cache for
    /// lockfile-free managed projects. Called after a successful [`crate::run`]
    /// (from run_inner): only then has the graph build populated the
    /// resolution — the installer's initialize_npm_resolution_if_managed
    /// returns Ok(None) on a cache miss without resolving anything, so saving
    /// earlier would store an empty snapshot. Lockfile projects skip this:
    /// their snapshot comes from the on-disk lockfile (and the resolve
    /// callback never serves them).
    pub fn save_npm_snapshot_cache(&self) {
        let lock_path = resolve_lockfile_path(&self.shared.resolver_factory);
        if lock_path.as_deref().is_some_and(|p| p.exists()) {
            return;
        }
        use deno_resolver::npm::NpmResolver;
        let Ok(NpmResolver::Managed(managed)) = self.shared.resolver_factory.npm_resolver() else {
            return;
        };
        let key = crate::npm_cache::compute_key(&self.cwd);
        crate::npm_cache::insert(key, managed.resolution().serialized_valid_snapshot());
    }

    /// Builds the per-run permission-bound components over an existing
    /// [`SharedServices`] stack: the file fetcher, the graph loader (bound to
    /// this run's live permissions container) and a fresh module graph.
    pub fn new(
        shared: Arc<SharedServices>,
        cwd: PathBuf,
        permissions: deno_runtime::deno_permissions::PermissionsContainer,
    ) -> deno_core::anyhow::Result<Self> {
        let in_npm_pkg_checker = shared.resolver_factory.in_npm_package_checker()?.clone();
        let file_fetcher = Arc::new(PermissionedFileFetcher::new(
            NullBlobStore,
            shared.http_cache.clone(),
            (*shared.http_client).clone(),
            shared.memory_files.clone(),
            shared.sys.clone(),
            PermissionedFileFetcherOptions {
                allow_remote: true,
                cache_setting: CacheSetting::Use,
            },
        ));
        let graph_loader = Arc::new(deno_resolver::file_fetcher::DenoGraphLoader::new(
            file_fetcher.clone(),
            shared.global_http_cache.clone(),
            in_npm_pkg_checker,
            shared.sys.clone(),
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
        Ok(Self {
            shared,
            cwd,
            file_fetcher,
            graph_loader,
            graph,
        })
    }
}
