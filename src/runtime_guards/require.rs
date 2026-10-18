use std::borrow::Cow;
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use deno_core::v8;
use deno_core::OpState;
use deno_core::ToV8;
use deno_error::JsErrorBox;
use deno_resolver::npm::{DenoInNpmPackageChecker, NpmResolver};
use deno_runtime::deno_permissions::OpenAccessKind;
use deno_runtime::deno_permissions::PermissionsContainer;
use sys_traits::impls::RealSys;

#[derive(Clone)]
struct Delegates {
    stat: v8::Global<v8::Function>,
    real_path: v8::Global<v8::Function>,
    package_scope: v8::Global<v8::Function>,
    package_imports: v8::Global<v8::Function>,
}

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

#[deno_core::op2]
pub fn op_libdeno_install_require_delegates(
    state: &mut OpState,
    #[scoped] stat: v8::Global<v8::Function>,
    #[scoped] real_path: v8::Global<v8::Function>,
    #[scoped] package_scope: v8::Global<v8::Function>,
    #[scoped] package_imports: v8::Global<v8::Function>,
) {
    state.put(Delegates {
        stat,
        real_path,
        package_scope,
        package_imports,
    });
}

fn delegate(
    state: &Rc<RefCell<OpState>>,
    select: fn(&Delegates) -> &v8::Global<v8::Function>,
) -> Result<v8::Global<v8::Function>, JsErrorBox> {
    state
        .borrow()
        .try_borrow::<Delegates>()
        .map(|delegates| select(delegates).clone())
        .ok_or_else(|| JsErrorBox::new("Error", "require op delegates are unavailable"))
}

fn checked_path(
    state: &Rc<RefCell<OpState>>,
    path: &str,
) -> Result<std::path::PathBuf, JsErrorBox> {
    let path = Path::new(path);
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let checked = {
        let state = state.borrow();
        let permissions = state
            .try_borrow::<PermissionsContainer>()
            .ok_or_else(|| JsErrorBox::new("Error", "permissions are unavailable"))?;
        permissions
            .check_open(Cow::Owned(canonical), OpenAccessKind::Read, Some("require"))
            .map_err(JsErrorBox::from_err)?
            .into_owned_path()
    };
    Ok(checked)
}

fn call<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    callback: v8::Global<v8::Function>,
    args: &[v8::Local<'s, v8::Value>],
) -> Result<JsValue, JsErrorBox> {
    let caught = std::pin::pin!(v8::TryCatch::new(scope));
    let mut caught = caught.init();
    let callback = v8::Local::new(&caught, callback);
    match callback.call(&caught, v8::undefined(&caught).into(), args) {
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

fn path_string<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    path: &Path,
) -> Result<v8::Local<'s, v8::Value>, JsErrorBox> {
    let path = path
        .to_str()
        .ok_or_else(|| JsErrorBox::new("Error", "require path is not valid UTF-8"))?;
    v8::String::new(scope, path)
        .map(Into::into)
        .ok_or_else(|| JsErrorBox::new("Error", "failed to encode require path"))
}

fn string_value<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    value: &str,
) -> Result<v8::Local<'s, v8::Value>, JsErrorBox> {
    v8::String::new(scope, value)
        .map(Into::into)
        .ok_or_else(|| JsErrorBox::new("Error", "failed to encode require argument"))
}

#[deno_core::op2(reentrant, stack_trace)]
pub fn op_require_stat_guard<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: Rc<RefCell<OpState>>,
    #[string] path: &str,
) -> Result<JsValue, JsErrorBox> {
    let callback = delegate(&state, |d| &d.stat)?;
    if !crate::permission_broker::is_installed() {
        let path = string_value(scope, path)?;
        return call(scope, callback, &[path]);
    }
    let path = checked_path(&state, path)?;
    let path = path_string(scope, &path)?;
    call(scope, callback, &[path])
}

#[deno_core::op2(reentrant, stack_trace)]
pub fn op_require_real_path_guard<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: Rc<RefCell<OpState>>,
    #[string] path: &str,
) -> Result<JsValue, JsErrorBox> {
    let callback = delegate(&state, |d| &d.real_path)?;
    if !crate::permission_broker::is_installed() {
        let path = string_value(scope, path)?;
        return call(scope, callback, &[path]);
    }
    let path = checked_path(&state, path)?;
    let path = path_string(scope, &path)?;
    call(scope, callback, &[path])
}

#[deno_core::op2(reentrant, stack_trace)]
pub fn op_require_package_imports_guard<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: Rc<RefCell<OpState>>,
    #[string] referrer: &str,
    #[string] request: &str,
) -> Result<JsValue, JsErrorBox> {
    let callback = delegate(&state, |d| &d.package_imports)?;
    if !crate::permission_broker::is_installed() {
        let args = [
            string_value(scope, referrer)?,
            string_value(scope, request)?,
        ];
        return call(scope, callback, &args);
    }
    let path = checked_path(&state, referrer)?;
    let args = [path_string(scope, &path)?, string_value(scope, request)?];
    call(scope, callback, &args)
}

#[deno_core::op2(reentrant, stack_trace)]
pub fn op_require_package_scope_guard<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    state: Rc<RefCell<OpState>>,
    #[string] path: &str,
) -> Result<JsValue, JsErrorBox> {
    let callback = delegate(&state, |d| &d.package_scope)?;
    if !crate::permission_broker::is_installed() {
        let path = string_value(scope, path)?;
        return call(scope, callback, &[path]);
    }
    let path = Path::new(path);
    if path.file_name() != Some("package.json".as_ref()) {
        return serde_value(
            scope,
            None::<deno_fs::sync::MaybeArc<deno_runtime::deno_node::PackageJson>>,
        );
    }
    let checked = checked_path(
        &state,
        path.to_str()
            .ok_or_else(|| JsErrorBox::new("Error", "require path is not valid UTF-8"))?,
    )?;
    let package = state
        .borrow()
        .borrow::<node_resolver::PackageJsonResolverRc<RealSys>>()
        .load_package_json(&checked)
        .ok()
        .flatten();
    serde_value(scope, package)
}

fn serde_value<'s, T: serde::Serialize>(
    scope: &mut v8::PinScope<'s, '_>,
    value: T,
) -> Result<JsValue, JsErrorBox> {
    let value = deno_core::serde_v8::to_v8(scope, value).map_err(JsErrorBox::from_err)?;
    Ok(JsValue(v8::Global::new(scope, value)))
}

pub(crate) fn alias_ops() -> Vec<deno_core::OpDecl> {
    let mut ops = vec![
        deno_runtime::deno_node::ops::require::op_require_stat::<RealSys>(),
        deno_runtime::deno_node::ops::require::op_require_real_path::<RealSys>(),
        deno_runtime::deno_node::ops::require::op_require_read_package_scope::<RealSys>(),
        deno_runtime::deno_node::ops::require::op_require_package_imports_resolve::<
            DenoInNpmPackageChecker,
            NpmResolver<RealSys>,
            RealSys,
        >(),
    ];
    for (op, (name, fast_name)) in ops.iter_mut().zip([
        (
            "op_libdeno_require_stat_alias",
            deno_core::ascii_str!("op_libdeno_require_stat_alias"),
        ),
        (
            "op_libdeno_require_real_path_alias",
            deno_core::ascii_str!("op_libdeno_require_real_path_alias"),
        ),
        (
            "op_libdeno_require_package_scope_alias",
            deno_core::ascii_str!("op_libdeno_require_package_scope_alias"),
        ),
        (
            "op_libdeno_require_package_imports_alias",
            deno_core::ascii_str!("op_libdeno_require_package_imports_alias"),
        ),
    ]) {
        op.name = name;
        op.name_fast = fast_name;
    }
    ops
}

pub(crate) fn guard_name(op: &str) -> &'static str {
    match op {
        "op_require_stat" => "op_require_stat_guard",
        "op_require_real_path" => "op_require_real_path_guard",
        "op_require_package_imports_resolve" => "op_require_package_imports_guard",
        "op_require_read_package_scope" => "op_require_package_scope_guard",
        _ => "",
    }
}

pub(crate) fn declarations() -> Vec<deno_core::OpDecl> {
    require_guard_ops::init().ops.into_owned()
}

deno_core::extension!(
    require_guard_ops,
    ops = [
        op_libdeno_install_require_delegates,
        op_require_stat_guard,
        op_require_real_path_guard,
        op_require_package_imports_guard,
        op_require_package_scope_guard
    ],
);
