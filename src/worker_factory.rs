// `new Worker(...)` factory: builds the create-worker callback the main
// worker uses to spawn web workers. Nested workers reuse the same shared
// services and snapshot, so spawning is recursive.

use std::rc::Rc;
use std::sync::Arc;

use deno_resolver::cjs::CjsTrackerRc;
use deno_resolver::npm::DenoInNpmPackageChecker;
use deno_resolver::npm::NpmResolver;
use deno_runtime::deno_fs::FileSystem;
use deno_runtime::deno_inspector_server::MainInspectorSessionChannel;
use deno_runtime::deno_node::NodeExtInitServices;
use deno_runtime::deno_web::InMemoryBroadcastChannel;
use deno_runtime::ops::worker_host::CreateWebWorkerArgs;
use deno_runtime::web_worker::WebWorker;
use deno_runtime::web_worker::WebWorkerOptions;
use deno_runtime::web_worker::WebWorkerServiceOptions;
use deno_runtime::BootstrapOptions;
use deno_runtime::WorkerExecutionMode;
use node_resolver::DenoIsBuiltInNodeModuleChecker;
use node_resolver::NodeResolverRc;
use node_resolver::PackageJsonResolverRc;
use sys_traits::impls::RealSys;

use crate::module_loader::GraphModuleLoader;
use crate::node_loader::SimpleNodeRequireLoader;
use crate::services::SharedServices;
use crate::RESIDUAL_LAZY_ESM;
use crate::RESIDUAL_LAZY_JS;
use crate::STARTUP_SNAPSHOT;

/// Immutable state shared with every spawned worker (and nested factories).
/// The trailing resolver instances are resolved once at factory construction;
/// nested factories reuse them instead of re-running fallible resolver factory
/// init, so the worker-spawning path has no error left to panic on.
type WebWorkerFactoryShared = (
    Arc<dyn deno_runtime::deno_web::BlobStoreTrait>,
    InMemoryBroadcastChannel,
    Arc<deno_runtime::FeatureChecker>,
    Arc<dyn FileSystem>,
    Arc<SharedServices>,
    MainInspectorSessionChannel,
    BootstrapOptions,
    // V8 heap cap (bytes) inherited from the main worker's LibdenoOptions.
    Option<usize>,
    CjsTrackerRc<DenoInNpmPackageChecker, RealSys>,
    NodeResolverRc<
        DenoInNpmPackageChecker,
        DenoIsBuiltInNodeModuleChecker,
        NpmResolver<RealSys>,
        RealSys,
    >,
    PackageJsonResolverRc<RealSys>,
    DenoInNpmPackageChecker,
);

/// Builds the `new Worker(...)` factory. Each spawned worker builds its own
/// Rc-based loader/node services from the shared `Arc<RuntimeServices>`, so
/// nothing non-Send crosses threads. Nested workers get their own factory
/// built from the same shared state, so spawning is recursive. `max_heap_bytes`
/// (the main worker's `LibdenoOptions` heap cap) is forwarded so `new Worker()`
/// cannot bypass the heap limit.
#[allow(clippy::too_many_arguments)]
pub fn create_web_worker_factory(
    blob_store: Arc<dyn deno_runtime::deno_web::BlobStoreTrait>,
    broadcast_channel: InMemoryBroadcastChannel,
    feature_checker: Arc<deno_runtime::FeatureChecker>,
    fs: Arc<dyn FileSystem>,
    services: Arc<SharedServices>,
    main_inspector_session_tx: MainInspectorSessionChannel,
    bootstrap_base: BootstrapOptions,
    max_heap_bytes: Option<usize>,
) -> deno_core::anyhow::Result<Arc<deno_runtime::ops::worker_host::CreateWebWorkerCb>> {
    // All fallible resolver init runs up front so the factory construction
    // itself can fail cleanly; the per-spawn callback below stays infallible.
    let cjs_tracker = services.resolver_factory.cjs_tracker()?.clone();
    let node_resolver = services.resolver_factory.node_resolver()?.clone();
    let pkg_json_resolver = services.resolver_factory.pkg_json_resolver().clone();
    let in_npm_pkg_checker = services.resolver_factory.in_npm_package_checker()?.clone();

    let shared: Arc<WebWorkerFactoryShared> = Arc::new((
        blob_store,
        broadcast_channel,
        feature_checker,
        fs,
        services,
        // The inspector session channel is intentionally SHARED across workers:
        // each worker sends its own session-pair proxy through this one sender,
        // which is how a single attached inspector reaches all workers. Do not
        // give each worker a fresh channel (worker inspection would silently die).
        main_inspector_session_tx,
        bootstrap_base,
        max_heap_bytes,
        cjs_tracker,
        node_resolver,
        pkg_json_resolver,
        in_npm_pkg_checker,
    ));

    Ok(build_web_worker_factory(shared))
}

/// Infallible half of the factory. All fallible resolver init already ran
/// (and is memoized on the shared ResolverFactory) in `create_web_worker_factory`,
/// so building — and recursively rebuilding — nested factories from the same
/// shared state and the same resolver instances can never fail. The script-
/// reachable `new Worker(...)` path therefore has no error to panic on.
fn build_web_worker_factory(
    shared: Arc<WebWorkerFactoryShared>,
) -> Arc<deno_runtime::ops::worker_host::CreateWebWorkerCb> {
    Arc::new(move |args: CreateWebWorkerArgs| {
        // Worker's own permissions container: each worker carries the
        // permissions captured at `new Worker(...)` time (args.permissions),
        // NOT the parent's. Grabbed first so the move of args.permissions
        // below is safe.
        let worker_permissions = args.permissions.clone();
        let (
            blob_store,
            broadcast_channel,
            feature_checker,
            fs,
            services,
            main_inspector_session_tx,
            bootstrap_base,
            max_heap_bytes,
            cjs_tracker,
            node_resolver,
            pkg_json_resolver,
            in_npm_pkg_checker,
        ) = &*shared;
        // Nested factory for the spawned worker's own workers: built from the
        // same shared state and the same already-initialized resolvers, so
        // this call cannot fail (no script-reachable error path, no panic).
        let nested_cb = build_web_worker_factory(shared.clone());
        let module_loader: Rc<dyn deno_core::ModuleLoader> = Rc::new(GraphModuleLoader::new(
            services.clone(),
            worker_permissions.clone(),
        ));
        let node_require_loader: deno_runtime::deno_node::NodeRequireLoaderRc = Rc::new(
            SimpleNodeRequireLoader::new(cjs_tracker.clone(), in_npm_pkg_checker.clone()),
        );
        let node_resolver = node_resolver.clone();
        let pkg_json_resolver = pkg_json_resolver.clone();
        let sys = services.sys.clone();
        WebWorker::bootstrap_from_options(
            WebWorkerServiceOptions {
                blob_store: blob_store.clone(),
                broadcast_channel: broadcast_channel.clone(),
                deno_rt_native_addon_loader: None,
                compiled_wasm_module_store: None,
                feature_checker: feature_checker.clone(),
                fs: fs.clone(),
                main_inspector_session_tx: main_inspector_session_tx.clone(),
                module_loader,
                node_services: Some(NodeExtInitServices {
                    node_require_loader,
                    node_resolver,
                    pkg_json_resolver,
                    sys,
                }),
                npm_process_state_provider: Some(services.npm_process_state_provider.clone()),
                // Op-level checks use the worker's own permissions container
                // (args.permissions), as deno_runtime expects.
                // Residual: worker-triggered graph fetches are checked against
                // the shared live parent container (matches the CLI's
                // shared-graph behavior; a module already fetched into the
                // graph is not re-checked per worker).
                permissions: args.permissions,
                root_cert_store_provider: None,
                shared_array_buffer_store: None,
                bundle_provider: None,
            },
            WebWorkerOptions {
                name: args.name,
                main_module: args.main_module.clone(),
                worker_id: args.worker_id,
                bootstrap: BootstrapOptions {
                    mode: WorkerExecutionMode::Worker,
                    location: Some(args.main_module),
                    close_on_idle: args.close_on_idle,
                    ..bootstrap_base.clone()
                },
                extensions: vec![],
                startup_snapshot: Some(STARTUP_SNAPSHOT),
                residual_lazy_js_sources: RESIDUAL_LAZY_JS,
                residual_lazy_esm_sources: RESIDUAL_LAZY_ESM,
                unsafely_ignore_certificate_errors: None,
                create_params: crate::limits::isolate_create_params(*max_heap_bytes),
                seed: None,
                create_web_worker_cb: nested_cb,
                format_js_error_fn: None,
                worker_type: args.worker_type,
                cache_storage_dir: None,
                stdio: deno_runtime::deno_io::Stdio::default(),
                trace_ops: None,
                close_on_idle: args.close_on_idle,
                maybe_worker_metadata: args.maybe_worker_metadata,
                maybe_main_module_blob: None,
                maybe_coverage_dir: None,
                maybe_cpu_prof_config: None,
                enable_raw_imports: false,
                enable_stack_trace_arg_in_ops: false,
                wait_for_debugger_on_start: false,
                wait_for_page_wait_for_debugger: false,
            },
        )
    })
}
