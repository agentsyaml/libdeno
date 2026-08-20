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
pub struct NpmProcessStateProviderImpl {
    kind: NpmProcessStateProviderKind,
    local_node_modules_path: Option<String>,
}

enum NpmProcessStateProviderKind {
    Managed(deno_resolver::npm::managed::NpmResolutionCellRc),
    Byonm,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum NpmProcessStateKind {
    Snapshot(deno_npm::resolution::SerializedNpmResolutionSnapshot),
    Byonm,
}

/// Serialized npm resolution state handed to npm subprocesses (spawned via
/// child_process.fork). WARNING: when the registry URL is of the form
/// `https://user:pass@host/`, those credentials ride along in this serialized
/// string. The hand-off is same-process/same-user (not a privilege boundary),
/// but the string must never be logged or shipped across machines. The
/// provider keeps only the resolution cell and serializes it at fork time, so
/// it does not retain or expose an obsolete snapshot through its debug
/// representation.
#[derive(serde::Serialize, serde::Deserialize)]
struct NpmProcessState {
    kind: NpmProcessStateKind,
    local_node_modules_path: Option<String>,
}

impl std::fmt::Debug for NpmProcessStateProviderImpl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NpmProcessStateProviderImpl")
            .field(
                "kind",
                &match &self.kind {
                    NpmProcessStateProviderKind::Managed(_) => "managed",
                    NpmProcessStateProviderKind::Byonm => "byonm",
                },
            )
            .field("local_node_modules_path", &self.local_node_modules_path)
            .finish()
    }
}

impl deno_runtime::deno_process::NpmProcessStateProvider for NpmProcessStateProviderImpl {
    fn get_npm_process_state(&self) -> String {
        let kind = match &self.kind {
            NpmProcessStateProviderKind::Managed(resolution) => NpmProcessStateKind::Snapshot(
                resolution.serialized_valid_snapshot().into_serialized(),
            ),
            NpmProcessStateProviderKind::Byonm => NpmProcessStateKind::Byonm,
        };
        // The string is written to a private inherited fd by deno_process. Do
        // not log serialization failures: the serialized state may contain
        // registry credentials.
        deno_core::serde_json::to_string(&NpmProcessState {
            kind,
            local_node_modules_path: self.local_node_modules_path.clone(),
        })
        .unwrap_or_else(|_| "{}".to_string())
    }
}

/// Runs npm lifecycle scripts (preinstall/install/postinstall) through the
/// system shell, exactly like npm does (`sh -c <script>` with the npm_*
/// environment and the package's `.bin` on PATH). Scripts that need a
/// runtime (node, node-gyp, ...) must be available on PATH.
#[derive(Debug)]
pub struct ShellLifecycleScriptsExecutor;

/// A lifecycle script is arbitrary package code. Keep one script from
/// pinning the npm install forever, while leaving enough room for ordinary
/// native-addon builds. The direct child is supervised; descendants are not
/// claimed to be part of this boundary until platform process-tree support is
/// added.
// ponytail: direct-child supervision now; add process groups/Job Objects after
// the platform-specific lifecycle research is available.
const LIFECYCLE_SCRIPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const LIFECYCLE_KILL_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

async fn supervise_lifecycle_script(
    mut child: tokio::process::Child,
    package: &str,
    event: &str,
    timeout: std::time::Duration,
) -> anyhow::Result<std::process::ExitStatus> {
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) => Err(anyhow::anyhow!(
            "lifecycle script `{event}` for package `{package}` wait failed: {error}"
        )),
        Err(_) => {
            let kill_error = child.start_kill().err();
            let wait_result = tokio::time::timeout(LIFECYCLE_KILL_WAIT_TIMEOUT, child.wait()).await;
            match (kill_error, wait_result) {
                (None, Ok(Ok(status))) => Err(anyhow::anyhow!(
                    "lifecycle script `{event}` for package `{package}` timed out after \
                     {timeout:?}; direct child killed with {status}; process-tree descendants \
                     are not supervised"
                )),
                (Some(kill_error), Ok(Ok(status))) => Err(anyhow::anyhow!(
                    "lifecycle script `{event}` for package `{package}` timed out after \
                     {timeout:?}; direct-child kill failed: {kill_error}; wait returned {status}; \
                     process-tree descendants are not supervised"
                )),
                (kill_error, Ok(Err(wait_error))) => Err(anyhow::anyhow!(
                    "lifecycle script `{event}` for package `{package}` timed out after \
                     {timeout:?}; kill result: {}; wait failed: {wait_error}; process-tree \
                     descendants are not supervised",
                    kill_error
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "sent".to_string())
                )),
                (kill_error, Err(_)) => Err(anyhow::anyhow!(
                    "lifecycle script `{event}` for package `{package}` timed out after \
                     {timeout:?}; kill result: {}; direct child did not exit within \
                     {LIFECYCLE_KILL_WAIT_TIMEOUT:?}; process-tree descendants are not supervised",
                    kill_error
                        .map(|error| error.to_string())
                        .unwrap_or_else(|| "sent".to_string())
                )),
            }
        }
    }
}

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
                let child = cmd
                    .arg(script)
                    .current_dir(cwd)
                    .env("PATH", &path)
                    .env("INIT_CWD", cwd)
                    .env("npm_lifecycle_event", event)
                    .env("npm_lifecycle_script", script)
                    .env("npm_package_name", name)
                    .env("npm_package_version", pkg.package.id.nv.version.to_string())
                    .spawn()
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "lifecycle script `{event}` for package `{name}` could not start: \
                             {error}"
                        )
                    })?;
                let status =
                    supervise_lifecycle_script(child, name, event, LIFECYCLE_SCRIPT_TIMEOUT)
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
/// (see `NpmProcessState`) may embed registry credentials — it is generated
/// only when deno_process is about to fork and must never be logged or sent
/// off-machine.
pub fn create_npm_process_state_provider(
    resolver_factory: &Arc<ResolverFactory<RealSys>>,
) -> deno_core::anyhow::Result<deno_runtime::deno_process::NpmProcessStateProviderRc> {
    use deno_resolver::npm::NpmResolver;
    match resolver_factory.npm_resolver()? {
        NpmResolver::Managed(managed) => {
            Ok(deno_fs::sync::MaybeArc::new(NpmProcessStateProviderImpl {
                kind: NpmProcessStateProviderKind::Managed(
                    resolver_factory.npm_resolution().clone(),
                ),
                local_node_modules_path: managed
                    .root_node_modules_path()
                    .map(|p| p.to_string_lossy().to_string()),
            }))
        }
        NpmResolver::Byonm(byonm) => {
            Ok(deno_fs::sync::MaybeArc::new(NpmProcessStateProviderImpl {
                kind: NpmProcessStateProviderKind::Byonm,
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
    /// Cross-run module analysis cache (deno_graph `ModuleAnalyzer` +
    /// `ModuleInfoCacher`): re-parses and re-analyzes of the transitive graph
    /// are skipped when (specifier, source hash) is unchanged since the last
    /// run.
    pub module_info_cache: Arc<crate::analysis_cache::ModuleInfoCache>,
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

        // Cross-run analysis caches wired into the resolver stack (see
        // analysis_cache.rs): in-memory CJS analysis, a process-global
        // singleton so `run()` (which builds a fresh SharedServices per
        // call) still reuses it.
        let node_analysis_cache: deno_resolver::cjs::analyzer::NodeAnalysisCacheRc =
            crate::analysis_cache::node_analysis_cache();

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
                // Cross-run CJS analysis cache (content-hash keyed, process-
                // global — see analysis_cache.rs). node_resolution_cache and
                // package_json_cache stay None deliberately: upstream's
                // thread-local stores are keyed by path only with no
                // invalidation path, so in-process filesystem changes (an
                // `npm install` between runs, an edited package.json) would
                // serve stale resolutions forever.
                node_analysis_cache: Some(node_analysis_cache),
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
        let module_info_cache = crate::analysis_cache::module_info_cache();

        Ok(Arc::new(Self {
            sys,
            resolver_factory,
            http_client,
            http_cache,
            global_http_cache,
            memory_files,
            graph_resolver,
            module_info_cache,
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
                // Gate file-scheme module imports with check_open (api name
                // "import"), so static file imports honor the --allow-read
                // scope AND the broker/hook, exactly like dynamic import().
                // With None, upstream falls back to check_specifier, whose
                // file branch exempts CheckSpecifierKind::Static entirely —
                // a static `import ... from "file:///..."` (including relative
                // imports, which are file scheme) would bypass --allow-read.
                file_permission_api_name: Some("import"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use deno_runtime::deno_process::NpmProcessStateProvider;

    #[test]
    fn npm_process_state_provider_reads_latest_snapshot_for_repeated_forks() {
        let resolution = deno_fs::sync::MaybeArc::new(
            deno_resolver::npm::managed::NpmResolutionCell::from_serialized(None),
        );
        let provider = NpmProcessStateProviderImpl {
            kind: NpmProcessStateProviderKind::Managed(resolution.clone()),
            local_node_modules_path: None,
        };
        let initial = provider.get_npm_process_state();

        let id = deno_npm::NpmPackageId::from_serialized("state-pkg@1.0.0").unwrap();
        let snapshot = deno_npm::resolution::SerializedNpmResolutionSnapshot {
            root_packages: std::collections::HashMap::new(),
            packages: vec![
                deno_npm::resolution::SerializedNpmResolutionSnapshotPackage {
                    id,
                    system: Default::default(),
                    dist: None,
                    dependencies: std::collections::HashMap::new(),
                    optional_dependencies: std::collections::HashSet::new(),
                    optional_peer_dependencies: std::collections::HashSet::new(),
                    extra: None,
                    is_deprecated: false,
                    has_bin: false,
                    has_scripts: false,
                },
            ],
        }
        .into_valid()
        .unwrap();
        resolution.set_snapshot(deno_npm::resolution::NpmResolutionSnapshot::new(snapshot));

        // Each call represents a separate fork request; both must observe the
        // cell after managed npm resolution has completed.
        let first_fork = provider.get_npm_process_state();
        let second_fork = provider.get_npm_process_state();
        assert_ne!(initial, first_fork);
        assert_eq!(first_fork, second_fork);
        assert!(first_fork.contains("state-pkg@1.0.0"));
        assert!(!format!("{provider:?}").contains("state-pkg@1.0.0"));
    }

    #[cfg(unix)]
    #[test]
    fn lifecycle_supervision_times_out_and_reaps_direct_child() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = runtime.block_on(async {
            let child = tokio::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap();
            supervise_lifecycle_script(
                child,
                "lifecycle-test-package",
                "install",
                std::time::Duration::from_millis(10),
            )
            .await
            .unwrap_err()
        });
        let message = error.to_string();
        assert!(message.contains("lifecycle-test-package"));
        assert!(message.contains("install"));
        assert!(message.contains("timed out"));
        assert!(message.contains("direct child"));
    }
}
