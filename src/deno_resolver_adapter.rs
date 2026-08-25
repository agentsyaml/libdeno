//! Crate-local boundary for the unstable upstream Deno resolver assembly.
//!
//! Keep the resolver options and mode/path decisions in one place so normal
//! and authoritative construction cannot drift. Higher-level manifest and
//! cache policy stays in `npm_cache`; service-specific serialization stays in
//! `services`.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use deno_config::deno_json::NodeModulesDirMode;
use deno_resolver::cjs::IsCjsResolutionMode;
use deno_resolver::deno_json::CompilerOptionsOverrides;
use deno_resolver::factory::ConfigDiscoveryOption;
use deno_resolver::factory::ResolverFactory;
use deno_resolver::factory::ResolverFactoryOptions;
use deno_resolver::factory::WorkspaceFactory;
use deno_resolver::factory::WorkspaceFactoryOptions;
use deno_resolver::loader::AllowJsonImports;
use node_resolver::analyze::NodeCodeTranslatorMode;
use sys_traits::impls::RealSys;

pub(crate) fn new_workspace_factory(
    initial_cwd: PathBuf,
    config_start_paths: Vec<PathBuf>,
) -> Arc<WorkspaceFactory<RealSys>> {
    new_workspace_factory_with_node_modules_dir(
        initial_cwd,
        config_start_paths,
        Some(NodeModulesDirMode::Auto),
    )
}

pub(crate) fn new_workspace_factory_with_node_modules_dir(
    initial_cwd: PathBuf,
    config_start_paths: Vec<PathBuf>,
    node_modules_dir: Option<NodeModulesDirMode>,
) -> Arc<WorkspaceFactory<RealSys>> {
    node_resolver::PackageJsonThreadLocalCache::clear();
    Arc::new(WorkspaceFactory::new(
        RealSys,
        initial_cwd,
        WorkspaceFactoryOptions {
            config_discovery: ConfigDiscoveryOption::Discover {
                start_paths: config_start_paths,
            },
            node_modules_dir,
            ..Default::default()
        },
    ))
}

pub(crate) fn new_authoritative_workspace_factory(
    initial_cwd: PathBuf,
    config_start_paths: Vec<PathBuf>,
    original: &WorkspaceFactory<RealSys>,
) -> deno_core::anyhow::Result<Arc<WorkspaceFactory<RealSys>>> {
    Ok(new_workspace_factory_with_node_modules_dir(
        initial_cwd,
        config_start_paths,
        Some(original.node_modules_dir_mode()?),
    ))
}

/// Constructs the resolver with the exact option set shared by normal stack
/// construction and authoritative manifest validation.
pub(crate) fn new_resolver_factory(
    workspace_factory: Arc<WorkspaceFactory<RealSys>>,
    node_analysis_cache: deno_resolver::cjs::analyzer::NodeAnalysisCacheRc,
) -> Arc<ResolverFactory<RealSys>> {
    Arc::new(ResolverFactory::new(
        workspace_factory,
        ResolverFactoryOptions {
            compiler_options_overrides: CompilerOptionsOverrides {
                no_transpile: false,
                force_check_js: false,
                source_map_base: None,
                preserve_jsx: false,
                force_disable_verbatim_module_syntax: true,
            },
            is_cjs_resolution_mode: IsCjsResolutionMode::Disabled,
            node_code_translator_mode: NodeCodeTranslatorMode::ModuleLoader,
            node_resolver_options: Default::default(),
            npm_system_info: Default::default(),
            allow_json_imports: AllowJsonImports::WithAttribute,
            require_modules: vec![],
            newest_dependency_date: None,
            node_analysis_cache: Some(node_analysis_cache),
            node_resolution_cache: None,
            package_json_cache: None,
            package_json_dep_resolution: None,
            specified_import_map: None,
            unstable_sloppy_imports: false,
            on_mapped_resolution_diagnostic: None,
        },
    ))
}

pub(crate) fn resolver_byonm_and_root_node_modules_path(
    resolver_factory: &ResolverFactory<RealSys>,
) -> deno_core::anyhow::Result<(bool, Option<PathBuf>)> {
    let byonm = resolver_factory.use_byonm()?;
    let root_node_modules_path = resolver_factory
        .npm_resolver()?
        .root_node_modules_path()
        .map(Path::to_path_buf);
    Ok((byonm, root_node_modules_path))
}

/// Returns only upstream resolver data needed by the service-owned process
/// state serializer. The provider implementation remains in `services`.
pub(crate) fn npm_process_state_inputs(
    resolver_factory: &Arc<ResolverFactory<RealSys>>,
) -> deno_core::anyhow::Result<(
    Option<deno_resolver::npm::managed::NpmResolutionCellRc>,
    Option<String>,
)> {
    use deno_resolver::npm::NpmResolver;

    match resolver_factory.npm_resolver()? {
        NpmResolver::Managed(managed) => Ok((
            Some(resolver_factory.npm_resolution().clone()),
            managed
                .root_node_modules_path()
                .map(|path| path.to_string_lossy().to_string()),
        )),
        NpmResolver::Byonm(byonm) => Ok((
            None,
            byonm
                .root_node_modules_path()
                .map(|path| path.to_string_lossy().to_string()),
        )),
    }
}
