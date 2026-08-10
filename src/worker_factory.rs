// `new Worker(...)` factory: builds the create-worker callback the main
// worker uses to spawn web workers. Nested workers reuse the same shared
// services and snapshot, so spawning is recursive.

use std::rc::Rc;
use std::sync::Arc;

use deno_runtime::deno_fs::FileSystem;
use deno_runtime::deno_inspector_server::MainInspectorSessionChannel;
use deno_runtime::deno_node::NodeExtInitServices;
use deno_runtime::deno_permissions::PermissionsContainer;
use deno_runtime::deno_web::InMemoryBroadcastChannel;
use deno_runtime::ops::worker_host::CreateWebWorkerArgs;
use deno_runtime::web_worker::WebWorker;
use deno_runtime::web_worker::WebWorkerOptions;
use deno_runtime::web_worker::WebWorkerServiceOptions;
use deno_runtime::BootstrapOptions;
use deno_runtime::WorkerExecutionMode;

use crate::module_loader::GraphModuleLoader;
use crate::node_loader::SimpleNodeRequireLoader;
use crate::services::SharedServices;
use crate::RESIDUAL_LAZY_ESM;
use crate::RESIDUAL_LAZY_JS;
use crate::STARTUP_SNAPSHOT;

/// Immutable state shared with every spawned worker (and nested factories).
type WebWorkerFactoryShared = (
    Arc<dyn deno_runtime::deno_web::BlobStoreTrait>,
    InMemoryBroadcastChannel,
    Arc<deno_runtime::FeatureChecker>,
    Arc<dyn FileSystem>,
    Arc<SharedServices>,
    PermissionsContainer,
    MainInspectorSessionChannel,
    BootstrapOptions,
);

/// Builds the `new Worker(...)` factory. Each spawned worker builds its own
/// Rc-based loader/node services from the shared `Arc<RuntimeServices>`, so
/// nothing non-Send crosses threads. Nested workers get their own factory
/// built from the same shared state, so spawning is recursive.
#[allow(clippy::too_many_arguments)]
pub fn create_web_worker_factory(
    blob_store: Arc<dyn deno_runtime::deno_web::BlobStoreTrait>,
    broadcast_channel: InMemoryBroadcastChannel,
    feature_checker: Arc<deno_runtime::FeatureChecker>,
    fs: Arc<dyn FileSystem>,
    services: Arc<SharedServices>,
    permissions: PermissionsContainer,
    main_inspector_session_tx: MainInspectorSessionChannel,
    bootstrap_base: BootstrapOptions,
) -> Arc<deno_runtime::ops::worker_host::CreateWebWorkerCb> {
    let shared: Arc<WebWorkerFactoryShared> = Arc::new((
        blob_store,
        broadcast_channel,
        feature_checker,
        fs,
        services,
        permissions,
        main_inspector_session_tx,
        bootstrap_base,
    ));

    Arc::new(move |args: CreateWebWorkerArgs| {
        let (
            blob_store,
            broadcast_channel,
            feature_checker,
            fs,
            services,
            permissions,
            main_inspector_session_tx,
            bootstrap_base,
        ) = &*shared;
        let nested_cb = create_web_worker_factory(
            blob_store.clone(),
            broadcast_channel.clone(),
            feature_checker.clone(),
            fs.clone(),
            services.clone(),
            permissions.clone(),
            main_inspector_session_tx.clone(),
            bootstrap_base.clone(),
        );
        let module_loader: Rc<dyn deno_core::ModuleLoader> = Rc::new(GraphModuleLoader::new(
            services.clone(),
            permissions.clone(),
        ));
        let node_require_loader: deno_runtime::deno_node::NodeRequireLoaderRc =
            Rc::new(SimpleNodeRequireLoader::new(
                services
                    .resolver_factory
                    .cjs_tracker()
                    .expect("cjs tracker")
                    .clone(),
            ));
        let node_resolver = services
            .resolver_factory
            .node_resolver()
            .expect("node resolver")
            .clone();
        let pkg_json_resolver = services.resolver_factory.pkg_json_resolver().clone();
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
                create_params: None,
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
