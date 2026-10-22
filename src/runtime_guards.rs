use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use deno_core::v8;
use deno_core::OpState;
use deno_core::ResourceId;
use deno_core::ToV8;
use deno_error::JsErrorBox;

#[path = "runtime_guards/require.rs"]
mod require_guards;

#[cfg(test)]
static TEST_ROUTE_PROBE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
struct TestRouteProbe;

#[derive(Clone)]
struct JsValue(v8::Global<v8::Value>);

impl<'s> ToV8<'s> for JsValue {
    type Error = std::convert::Infallible;
    fn to_v8(
        self,
        scope: &mut v8::PinScope<'s, '_>,
    ) -> Result<v8::Local<'s, v8::Value>, Self::Error> {
        Ok(v8::Local::new(scope, &self.0))
    }
}

#[derive(Clone)]
struct NetDelegates {
    tcp: v8::Global<v8::Function>,
    tls: v8::Global<v8::Function>,
}

fn delegate(
    state: &Rc<RefCell<OpState>>,
    feature: &str,
    select: fn(&NetDelegates) -> &v8::Global<v8::Function>,
) -> Result<(bool, v8::Global<v8::Function>), JsErrorBox> {
    let state = state.borrow();
    let checker = state
        .try_borrow::<Arc<deno_runtime::FeatureChecker>>()
        .ok_or_else(|| feature_error("runtime feature state is unavailable"))?;
    let delegates = state
        .try_borrow::<NetDelegates>()
        .ok_or_else(|| feature_error("runtime guard delegate state is unavailable"))?;
    Ok((checker.check(feature), select(delegates).clone()))
}

fn feature_error(message: &'static str) -> JsErrorBox {
    JsErrorBox::new("Error", message)
}

fn denied_feature(state: &OpState, feature: &str, message: &'static str) -> JsErrorBox {
    match state.try_borrow::<Arc<deno_runtime::FeatureChecker>>() {
        Some(checker) if checker.check(feature) => feature_error("runtime feature guard mismatch"),
        Some(_) => feature_error(message),
        None => feature_error("runtime feature state is unavailable"),
    }
}

#[deno_core::op2]
#[smi]
fn op_libdeno_install_net_delegates(
    state: &mut OpState,
    #[scoped] tcp: v8::Global<v8::Function>,
    #[scoped] tls: v8::Global<v8::Function>,
) -> u32 {
    state.put(NetDelegates { tcp, tls });
    #[cfg(test)]
    if state.try_borrow::<TestRouteProbe>().is_some() {
        let mut mask = 0;
        #[cfg(unix)]
        {
            mask |= 1;
        }
        #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
        {
            mask |= 2;
        }
        return mask;
    }
    0
}

#[deno_core::op2(reentrant)]
fn op_net_listen_tcp_guard<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: Rc<RefCell<OpState>>,
    #[scoped] addr: deno_runtime::deno_net::ops::IpAddr,
    reuse_port: bool,
    load_balanced: bool,
    tcp_backlog: i32,
) -> Result<JsValue, JsErrorBox> {
    let (net_enabled, callback) = delegate(&state, "net", |d| &d.tcp)?;
    if reuse_port && !net_enabled {
        return Err(JsErrorBox::new(
            "Error",
            "reusePort requires the net feature",
        ));
    }
    let addr = addr.to_v8(scope).map_err(JsErrorBox::from_err)?;
    let args = [
        addr,
        v8::Boolean::new(scope, reuse_port).into(),
        v8::Boolean::new(scope, load_balanced).into(),
        v8::Integer::new(scope, tcp_backlog).into(),
    ];
    call_original(scope, callback, &args)
}

#[deno_core::op2(reentrant)]
fn op_net_listen_tls_guard<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: Rc<RefCell<OpState>>,
    #[scoped] addr: deno_runtime::deno_net::ops::IpAddr,
    args: v8::Local<'s, v8::Object>,
    keys: v8::Local<'s, v8::Value>,
) -> Result<JsValue, JsErrorBox> {
    let (net_enabled, callback) = delegate(&state, "net", |d| &d.tls)?;
    let (reuse_port, args) = match normalize_tls_args(scope, args) {
        NormalizedTlsArgs::Ready(reuse_port, args) => (reuse_port, args),
        NormalizedTlsArgs::Thrown(exception) => return Ok(JsValue(exception)),
    };
    if reuse_port && !net_enabled {
        return Err(JsErrorBox::new(
            "Error",
            "reusePort requires the net feature",
        ));
    }
    let addr = addr.to_v8(scope).map_err(JsErrorBox::from_err)?;
    let args = v8::Local::new(scope, &args);
    call_original(scope, callback, &[addr, args.into(), keys])
}

#[deno_core::op2(reentrant, stack_trace)]
fn op_net_listen_udp_denied(
    _state: &mut OpState,
    #[scoped] _addr: deno_runtime::deno_net::ops::IpAddr,
    _reuse_address: bool,
    _loopback: bool,
) -> Result<(), JsErrorBox> {
    Err(denied_feature(
        _state,
        "net",
        "Deno.listenDatagram requires the net feature",
    ))
}

#[deno_core::op2(reentrant, stack_trace)]
fn op_net_listen_unixpacket_denied(
    _state: &mut OpState,
    #[string] _path: Option<String>,
) -> Result<(), JsErrorBox> {
    Err(denied_feature(
        _state,
        "net",
        "Deno.listenDatagram requires the net feature",
    ))
}

#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
#[deno_core::op2(reentrant, stack_trace)]
fn op_net_listen_vsock_denied(
    _state: &mut OpState,
    #[smi] _cid: u32,
    #[smi] _port: u32,
) -> Result<(ResourceId, u32, u32), JsErrorBox> {
    Err(denied_feature(
        _state,
        "vsock",
        "Deno.listen requires the vsock feature",
    ))
}

#[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
#[deno_core::op2(reentrant, stack_trace)]
async fn op_net_connect_vsock_denied(
    state: Rc<RefCell<OpState>>,
    #[smi] _cid: u32,
    #[smi] _port: u32,
) -> Result<(), JsErrorBox> {
    Err(denied_feature(
        &state.borrow(),
        "vsock",
        "Deno.connect requires the vsock feature",
    ))
}

#[deno_core::op2(reentrant, stack_trace)]
#[smi]
async fn op_kv_database_open_denied(
    state: Rc<RefCell<OpState>>,
    #[string] _path: Option<String>,
) -> Result<u32, JsErrorBox> {
    Err(denied_feature(
        &state.borrow(),
        "kv",
        "Deno.openKv requires the kv feature",
    ))
}

#[deno_core::op2(reentrant)]
#[smi]
fn op_cron_create_denied(
    state: Rc<RefCell<OpState>>,
    #[string] _name: String,
    #[string] _schedule: String,
    #[scoped] _backoff: Option<Vec<u32>>,
) -> Result<u32, JsErrorBox> {
    Err(denied_feature(
        &state.borrow(),
        "cron",
        "Deno.cron requires the cron feature",
    ))
}

enum NormalizedTlsArgs {
    Ready(bool, v8::Global<v8::Object>),
    Thrown(v8::Global<v8::Value>),
}

fn normalize_tls_args<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    args: v8::Local<'s, v8::Object>,
) -> NormalizedTlsArgs {
    let args = v8::Global::new(scope, args);
    let caught = std::pin::pin!(v8::TryCatch::new(scope));
    let mut caught = caught.init();
    let args = v8::Local::new(&caught, args);
    let mut values = Vec::new();
    for name in ["alpnProtocols", "reusePort", "loadBalanced", "tcpBacklog"] {
        let key = v8::String::new(&caught, name).unwrap();
        let value = args.get(&caught, key.into());
        let Some(value) = value else {
            let exception = caught
                .exception()
                .unwrap_or_else(|| v8::undefined(&caught).into());
            let exception = v8::Global::new(&caught, exception);
            let _ = caught.rethrow();
            return NormalizedTlsArgs::Thrown(exception);
        };
        values.push((name, value));
    }
    let reuse_port = values[1].1.boolean_value(&caught);
    let names = values
        .iter()
        .map(|(name, _)| v8::String::new(&caught, name).unwrap().into())
        .collect::<Vec<_>>();
    let properties = values.iter().map(|(_, value)| *value).collect::<Vec<_>>();
    let null = v8::null(&caught).into();
    let normalized = v8::Object::with_prototype_and_properties(&caught, null, &names, &properties);
    NormalizedTlsArgs::Ready(reuse_port, v8::Global::new(&caught, normalized))
}

fn call_original<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    callback: v8::Global<v8::Function>,
    args: &[v8::Local<'s, v8::Value>],
) -> Result<JsValue, JsErrorBox> {
    let caught = std::pin::pin!(v8::TryCatch::new(scope));
    let mut caught = caught.init();
    let callback = v8::Local::new(&caught, callback);
    let this = v8::undefined(&caught).into();
    match callback.call(&caught, this, args) {
        Some(value) => Ok(JsValue(v8::Global::new(&caught, value))),
        None => {
            let exception = caught
                .exception()
                .unwrap_or_else(|| v8::undefined(&caught).into());
            let _ = caught.rethrow();
            Ok(JsValue(v8::Global::new(&caught, exception)))
        }
    }
}

deno_core::extension!(
    runtime_guards_ext,
    deps = [deno_net],
    ops = [op_libdeno_install_net_delegates],
    esm_entry_point = "ext:runtime_guards_ext/init.js",
);

#[cfg(test)]
deno_core::extension!(
    runtime_guard_test_ext,
    deps = [deno_net, deno_kv, deno_cron],
    esm_entry_point = "ext:runtime_guard_test_ext/test_routes.js",
);

// These extensions are registered at runtime, so they are never baked into the
// startup snapshot. deno_core's `esm = [dir ..., ...]` form records the build
// tree path and reads it when the runtime starts, which only works on the
// machine that compiled the crate. Embed the sources instead.
fn embed_esm(
    extension: &mut deno_core::Extension,
    specifier: &'static str,
    source: deno_core::FastStaticString,
) {
    extension.esm_files =
        std::borrow::Cow::Owned(vec![deno_core::ExtensionFileSource::new(specifier, source)]);
}

fn guards_extension() -> deno_core::Extension {
    let mut extension = runtime_guards_ext::init();
    embed_esm(
        &mut extension,
        "ext:runtime_guards_ext/init.js",
        deno_core::ascii_str_include!("runtime_guards/init.js"),
    );
    extension
}

#[cfg(test)]
fn test_routes_extension() -> deno_core::Extension {
    let mut extension = runtime_guard_test_ext::init();
    embed_esm(
        &mut extension,
        "ext:runtime_guard_test_ext/test_routes.js",
        deno_core::ascii_str_include!("runtime_guards/test_routes.js"),
    );
    extension
}

pub(crate) fn extensions(
    checker: Arc<deno_runtime::FeatureChecker>,
    thread_domain: crate::thread_messaging::ThreadMessagingDomain,
    lifecycle: crate::worker_lifecycle::WorkerLifecycleRegistry,
    registration: Option<crate::worker_lifecycle::WorkerRegistration>,
) -> Vec<deno_core::Extension> {
    #[cfg(test)]
    let test_probe = TEST_ROUTE_PROBE.swap(false, std::sync::atomic::Ordering::SeqCst);
    let net = deno_runtime::deno_net::deno_net::init(None, None);
    let tcp = find_op(&net, "op_net_listen_tcp");
    let tls = find_op(&net, "op_net_listen_tls");
    let tcp_guard = op_net_listen_tcp_guard();
    let tls_guard = op_net_listen_tls_guard();
    let mut extension = guards_extension();
    let mut ops = extension.ops.into_owned();
    let require_ops = require_guards::declarations();
    ops.extend(require_ops.iter().copied());
    ops.extend(require_guards::alias_ops());
    push_alias(
        &mut ops,
        tcp,
        "op_libdeno_net_listen_tcp_alias",
        deno_core::ascii_str!("op_libdeno_net_listen_tcp_alias"),
    );
    push_alias(
        &mut ops,
        tls,
        "op_libdeno_net_listen_tls_alias",
        deno_core::ascii_str!("op_libdeno_net_listen_tls_alias"),
    );
    extension.ops = ops.into();
    extension.op_state_fn = Some(Box::new(move |state| {
        crate::thread_messaging::inject_domain(state, thread_domain);
        state.put(lifecycle);
        if let Some(registration) = registration {
            state.put(registration);
        }
        #[cfg(test)]
        if test_probe {
            state.put(TestRouteProbe);
        }
    }));
    let net_enabled = checker.check("net");
    let vsock_enabled = checker.check("vsock");
    let kv_enabled = checker.check("kv");
    let cron_enabled = checker.check("cron");
    let thread_ops = crate::thread_messaging::replacement_op_decls();
    extension.middleware_fn = Some(Box::new(move |op| {
        if let Some(replacement) = thread_ops.iter().find(|decl| decl.name == op.name) {
            return replace_op(op, *replacement);
        }
        if let Some(guard) = require_ops.iter().find(|decl| {
            matches!(
                op.name,
                "op_require_stat"
                    | "op_require_real_path"
                    | "op_require_package_imports_resolve"
                    | "op_require_read_package_scope"
            ) && decl.name == require_guards::guard_name(op.name)
        }) {
            return replace_op(op, *guard);
        }
        let guard = match op.name {
            "op_net_listen_tcp" => Some(tcp_guard),
            "op_net_listen_tls" => Some(tls_guard),
            "op_net_listen_udp" if !net_enabled => Some(op_net_listen_udp_denied()),
            #[cfg(unix)]
            "op_net_listen_unixpacket" if !net_enabled => Some(op_net_listen_unixpacket_denied()),
            #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
            "op_net_connect_vsock" if !vsock_enabled => Some(op_net_connect_vsock_denied()),
            #[cfg(any(target_os = "android", target_os = "linux", target_os = "macos"))]
            "op_net_listen_vsock" if !vsock_enabled => Some(op_net_listen_vsock_denied()),
            "op_kv_database_open" if !kv_enabled => Some(op_kv_database_open_denied()),
            "op_cron_create" if !cron_enabled => Some(op_cron_create_denied()),
            _ => None,
        };
        guard.map_or(op, |guard| replace_op(op, guard))
    }));
    #[cfg(test)]
    {
        let mut extensions = vec![extension];
        if test_probe {
            extensions.push(test_routes_extension());
        }
        extensions
    }
    #[cfg(not(test))]
    vec![extension]
}

fn find_op(extension: &deno_core::Extension, name: &str) -> deno_core::OpDecl {
    *extension.ops.iter().find(|op| op.name == name).unwrap()
}

fn push_alias(
    ops: &mut Vec<deno_core::OpDecl>,
    mut original: deno_core::OpDecl,
    name: &'static str,
    fast_name: deno_core::FastStaticString,
) {
    original.name = name;
    original.name_fast = fast_name;
    ops.push(original);
}

fn replace_op(original: deno_core::OpDecl, guard: deno_core::OpDecl) -> deno_core::OpDecl {
    let mut replaced = original.with_implementation_from(&guard);
    replaced.is_async = guard.is_async;
    replaced.is_reentrant = guard.is_reentrant;
    replaced.arg_count = guard.arg_count;
    replaced.no_side_effects = guard.no_side_effects;
    replaced.constructable = guard.constructable;
    replaced.accessor_type = guard.accessor_type;
    replaced.symbol_for = guard.symbol_for;
    replaced
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_guard_sources_are_embedded_in_the_binary() {
        for extension in [guards_extension(), test_routes_extension()] {
            for file in extension
                .esm_files
                .iter()
                .chain(extension.js_files.iter())
                .chain(extension.lazy_loaded_esm_files.iter())
                .chain(extension.lazy_loaded_js_files.iter())
            {
                assert!(
                    file.is_runtime_loadable(),
                    "{} is read from the build tree at runtime",
                    file.specifier
                );
            }
        }
    }

    #[test]
    fn missing_feature_checker_fails_closed() {
        let state = OpState::new(None);
        assert!(denied_feature(&state, "net", "denied")
            .to_string()
            .contains("unavailable"));
    }

    #[test]
    fn internal_disabled_feature_ops_are_catchable() {
        const CHILD: &str = "LIBDENO_INTERNAL_GUARD_PROBE";
        if std::env::var_os(CHILD).is_some() {
            TEST_ROUTE_PROBE.store(true, std::sync::atomic::Ordering::SeqCst);
            let dir =
                std::env::temp_dir().join(format!("libdeno-internal-guard-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let entry = dir.join("main.js");
            std::fs::write(
                &entry,
                "await globalThis.__libdeno_guard_probe__(); Deno.exit(0);",
            )
            .unwrap();
            assert_eq!(
                crate::run(
                    &entry,
                    &crate::LibdenoOptions {
                        allow_all_permissions: true,
                        features: Some(vec![]),
                        ..Default::default()
                    }
                )
                .unwrap(),
                0
            );
            let _ = std::fs::remove_dir_all(dir);
            return;
        }
        use std::process::{Command, Stdio};
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "runtime_guards::tests::internal_disabled_feature_ops_are_catchable",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                let output = child.wait_with_output().unwrap();
                assert!(
                    status.success(),
                    "internal op probe failed: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                break;
            }
            if std::time::Instant::now() >= deadline {
                #[cfg(unix)]
                unsafe {
                    libc::kill(-(child.id() as i32), libc::SIGKILL);
                }
                #[cfg(not(unix))]
                child.kill().unwrap();
                let _ = child.wait();
                panic!("internal op probe exceeded 15 seconds");
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
