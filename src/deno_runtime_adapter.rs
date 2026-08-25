use std::rc::Rc;
use std::sync::Arc;

use deno_core::ModuleLoader;
use deno_core::ModuleSpecifier;
use deno_runtime::deno_fs::FileSystem;
use deno_runtime::deno_inspector_server::MainInspectorSessionChannel;
use deno_runtime::deno_node::NodeExtInitServices;
use deno_runtime::deno_web::BlobStore;
use deno_runtime::deno_web::InMemoryBroadcastChannel;
use deno_runtime::worker::MainWorker;
use deno_runtime::worker::WorkerOptions;
use deno_runtime::worker::WorkerServiceOptions;
use deno_runtime::BootstrapOptions;

use crate::node_loader::SimpleNodeRequireLoader;
use crate::services::RuntimeServices;
use crate::worker_factory::create_web_worker_factory;
use crate::LibdenoError;
use crate::LibdenoOptions;

/// Inputs assembled by the run lifecycle for the direct Deno main-worker
/// construction boundary.
pub(crate) struct MainWorkerInput<'a> {
    pub(crate) main_module: &'a ModuleSpecifier,
    pub(crate) options: &'a LibdenoOptions,
    pub(crate) has_node_modules_dir: bool,
    pub(crate) fs: Arc<dyn FileSystem>,
    pub(crate) module_loader: Rc<dyn ModuleLoader>,
    pub(crate) permissions: deno_runtime::deno_permissions::PermissionsContainer,
    pub(crate) services: Arc<RuntimeServices>,
}

/// Builds the main worker and returns the isolate handle used by the existing
/// deadline and watcher/exit lifecycle.
pub(crate) fn build_main_worker(
    input: MainWorkerInput<'_>,
) -> Result<(MainWorker, deno_core::v8::IsolateHandle), LibdenoError> {
    let MainWorkerInput {
        main_module,
        options,
        has_node_modules_dir,
        fs,
        module_loader,
        permissions,
        services,
    } = input;

    let node_resolver = services.shared.resolver_factory.node_resolver()?.clone();
    let pkg_json_resolver = services.shared.resolver_factory.pkg_json_resolver().clone();
    let cjs_tracker = services.shared.resolver_factory.cjs_tracker()?.clone();
    let in_npm_pkg_checker = services
        .shared
        .resolver_factory
        .in_npm_package_checker()?
        .clone();
    let node_require_loader: deno_runtime::deno_node::NodeRequireLoaderRc = Rc::new(
        SimpleNodeRequireLoader::new(cjs_tracker, in_npm_pkg_checker),
    );
    let node_sys = services.shared.sys.clone();
    let node_services = Some(NodeExtInitServices {
        node_require_loader: node_require_loader.clone(),
        node_resolver: node_resolver.clone(),
        pkg_json_resolver: pkg_json_resolver.clone(),
        sys: node_sys.clone(),
    });

    let blob_store = BlobStore::default_arc();
    let broadcast_channel = InMemoryBroadcastChannel::default();
    // Default: "everything enabled" like `deno run --unstable` — an embedded
    // runtime has no flag surface unless the host opts in via
    // `LibdenoOptions.features`. worker-options gates the worker
    // `permissions`/`env`/`net` options in `new Worker(...)` (op_create_worker
    // exits the process if the feature is off), so a custom set that omits it
    // breaks worker permission narrowing. JS namespace IDs and
    // FeatureChecker names must stay in sync.
    let enabled_features: Vec<&str> = match &options.features {
        Some(features) => {
            // worker-options is force-enabled regardless of the custom set:
            // it gates the worker `permissions`/`env`/`net` options, and
            // op_create_worker EXITS the process when the feature is off —
            // a host shrinking the surface for untrusted plugins must not
            // hand a plugin a way to kill the host.
            let mut names = static_feature_names(features).map_err(|bad| {
                LibdenoError::Configuration(format!("unknown runtime feature: {bad}"))
            })?;
            if !names.contains(&"worker-options") {
                names.push("worker-options");
            }
            names
        }
        None => DEFAULT_RUNTIME_FEATURES.to_vec(),
    };
    let feature_checker = {
        let mut fc = deno_runtime::FeatureChecker::default();
        for feature in &enabled_features {
            fc.enable_feature(feature);
        }
        Arc::new(fc)
    };
    let unstable_ids: Vec<i32> = deno_features::UNSTABLE_FEATURES
        .iter()
        .filter(|f| enabled_features.contains(&f.name))
        .map(|f| f.id)
        .collect();

    let worker_fs = fs.clone();
    let worker_services = WorkerServiceOptions {
        blob_store: blob_store.clone(),
        broadcast_channel: broadcast_channel.clone(),
        deno_rt_native_addon_loader: None,
        feature_checker: feature_checker.clone(),
        fs,
        module_loader,
        node_services,
        npm_process_state_provider: Some(services.shared.npm_process_state_provider.clone()),
        permissions,
        root_cert_store_provider: None,
        fetch_dns_resolver: deno_runtime::deno_fetch::dns::Resolver::default(),
        shared_array_buffer_store: None,
        compiled_wasm_module_store: None,
        // In-process V8 code cache, keyed by specifier + source hash (limits.rs).
        v8_code_cache: Some(crate::limits::in_process_code_cache()),
        bundle_provider: None,
    };

    // Must match deno_runtime's release tag: scripts feature-detect
    // Deno.version.deno. deno_runtime 0.265.0 == Deno v2.9.5.
    const DENO_VERSION: &str = "2.9.5";

    let main_bootstrap = BootstrapOptions {
        deno_version: DENO_VERSION.to_string(),
        user_agent: format!(
            "libdeno/{}/Deno/{}",
            env!("CARGO_PKG_VERSION"),
            DENO_VERSION
        ),
        has_node_modules_dir,
        location: Some(main_module.clone()),
        args: options.args.clone(),
        unstable_features: unstable_ids,
        // fork() IPC pipe; gated on the marker captured at run() entry (limits.rs).
        node_ipc_init: crate::limits::node_ipc_init(),
        ..Default::default()
    };

    // Factory for `new Worker(...)`; nested workers reuse the same loader,
    // services and snapshot, and inherit the main worker's heap cap.
    let create_web_worker_cb: Arc<deno_runtime::ops::worker_host::CreateWebWorkerCb> =
        create_web_worker_factory(
            blob_store,
            broadcast_channel,
            feature_checker,
            worker_fs,
            services.clone(),
            MainInspectorSessionChannel::default(),
            main_bootstrap.clone(),
            options.max_heap_bytes,
        )?;

    let worker_options = WorkerOptions {
        bootstrap: main_bootstrap,
        create_web_worker_cb,
        extensions: vec![],
        startup_snapshot: Some(crate::STARTUP_SNAPSHOT),
        residual_lazy_js_sources: crate::RESIDUAL_LAZY_JS,
        residual_lazy_esm_sources: crate::RESIDUAL_LAZY_ESM,
        create_params: crate::limits::isolate_create_params(options.max_heap_bytes),
        ..Default::default()
    };

    let mut worker =
        MainWorker::bootstrap_from_options(main_module, worker_services, worker_options);
    let isolate_handle = worker.js_runtime.v8_isolate().thread_safe_handle();
    Ok((worker, isolate_handle))
}

/// The default unstable runtime surface. Keep this as the only default list;
/// tests exercise it through the runtime rather than mirroring the names.
const DEFAULT_RUNTIME_FEATURES: &[&str] = &["kv", "cron", "ffi", "webgpu", "worker-options"];

/// FeatureChecker::enable_feature requires `&'static str`, while a host's
/// feature list arrives as owned `String`s. The deno registry already owns the
/// static names, so map through it and retain first-seen order without leaking
/// host-provided strings or retaining the input set.
fn static_feature_names(features: &[String]) -> Result<Vec<&'static str>, String> {
    let mut names = Vec::with_capacity(features.len().min(deno_features::UNSTABLE_FEATURES.len()));
    for feature in features {
        let Some(name) = deno_features::UNSTABLE_FEATURES
            .iter()
            .find(|definition| definition.name == feature)
            .map(|definition| definition.name)
        else {
            return Err(feature.clone());
        };
        // FeatureChecker rejects duplicate enables; preserve the old
        // duplicate-tolerant behavior while keeping caller order.
        if !names.contains(&name) {
            names.push(name);
        }
    }
    Ok(names)
}
