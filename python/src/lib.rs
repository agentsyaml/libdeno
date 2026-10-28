// The pyfunction signatures mirror the Rust API keyword-for-keyword, so the
// argument count is part of the contract rather than a smell.
#![allow(clippy::too_many_arguments)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;

use libdeno::LibdenoError as CoreError;
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyList, PyTuple};

mod options;
mod output;
mod runtime;
mod source;

use options::{apply_capture, entry_base, options_from_python, validate_options};
use output::RunOutput;
use runtime::Runtime;
use source::{source_bytes, source_lang_from_python, SourceLang};

create_exception!(_libdeno, LibdenoError, PyException);
create_exception!(_libdeno, EntryError, LibdenoError);
create_exception!(_libdeno, DenoPermissionError, LibdenoError);
create_exception!(_libdeno, ConfigurationError, LibdenoError);
create_exception!(_libdeno, DenoRuntimeError, LibdenoError);
create_exception!(_libdeno, DenoIOError, LibdenoError);
create_exception!(_libdeno, DenoTimeoutError, LibdenoError);

#[pyfunction]
#[pyo3(
    signature = (entry, *, permissions=empty_tuple(), allow_all_permissions=false, args=empty_tuple(), cwd=None, max_heap_bytes=None, execution_deadline=None, features=None),
    text_signature = "(entry, *, permissions=(), allow_all_permissions=False, args=(), cwd=None, max_heap_bytes=None, execution_deadline=None, features=None)"
)]
fn run(
    py: Python<'_>,
    entry: &Bound<'_, PyAny>,
    permissions: Py<PyAny>,
    allow_all_permissions: bool,
    args: Py<PyAny>,
    cwd: Option<Bound<'_, PyAny>>,
    max_heap_bytes: Option<Bound<'_, PyAny>>,
    execution_deadline: Option<Bound<'_, PyAny>>,
    features: Option<Py<PyAny>>,
) -> PyResult<i32> {
    let entry = path_from_python(entry, "entry")?;
    let options = options_from_python(
        permissions.bind(py),
        allow_all_permissions,
        args.bind(py),
        cwd.as_ref(),
        max_heap_bytes.as_ref(),
        execution_deadline.as_ref(),
        features.as_ref().map(|value| value.bind(py)),
    )?;
    // Option validation comes first so a bad deadline/heap still reports
    // ConfigurationError, matching the documented argument contract.
    validate_options(&options)?;
    let base = py
        .detach(|| entry_base(&options))
        .map_err(DenoIOError::new_err)?;
    check_entry_detached(py, &entry, &base)?;
    let result = py.detach(|| libdeno::run(entry, &options));
    result.map_err(core_error_to_py)
}

#[pyfunction]
#[pyo3(
    signature = (entry, *, permissions=empty_tuple(), allow_all_permissions=false, args=empty_tuple(), cwd=None, max_heap_bytes=None, execution_deadline=None, features=None, capture_stdout=true, capture_stderr=true, max_capture_bytes=None),
    text_signature = "(entry, *, permissions=(), allow_all_permissions=False, args=(), cwd=None, max_heap_bytes=None, execution_deadline=None, features=None, capture_stdout=True, capture_stderr=True, max_capture_bytes=None)"
)]
#[allow(clippy::too_many_arguments)]
fn run_with_output(
    py: Python<'_>,
    entry: &Bound<'_, PyAny>,
    permissions: Py<PyAny>,
    allow_all_permissions: bool,
    args: Py<PyAny>,
    cwd: Option<Bound<'_, PyAny>>,
    max_heap_bytes: Option<Bound<'_, PyAny>>,
    execution_deadline: Option<Bound<'_, PyAny>>,
    features: Option<Py<PyAny>>,
    capture_stdout: bool,
    capture_stderr: bool,
    max_capture_bytes: Option<Bound<'_, PyAny>>,
) -> PyResult<RunOutput> {
    let entry = path_from_python(entry, "entry")?;
    let mut options = options_from_python(
        permissions.bind(py),
        allow_all_permissions,
        args.bind(py),
        cwd.as_ref(),
        max_heap_bytes.as_ref(),
        execution_deadline.as_ref(),
        features.as_ref().map(|value| value.bind(py)),
    )?;
    validate_options(&options)?;
    apply_capture(
        &mut options,
        capture_stdout,
        capture_stderr,
        max_capture_bytes.as_ref(),
    )?;
    let base = py
        .detach(|| entry_base(&options))
        .map_err(DenoIOError::new_err)?;
    check_entry_detached(py, &entry, &base)?;
    let result = py.detach(|| libdeno::run_with_output(entry, &options));
    result.map(RunOutput::from).map_err(core_error_to_py)
}

#[pyfunction]
#[pyo3(
    signature = (code, *, lang=None, base_dir=None, permissions=empty_tuple(), allow_all_permissions=false, args=empty_tuple(), max_heap_bytes=None, execution_deadline=None, features=None),
    text_signature = "(code, *, lang='javascript', base_dir='.', permissions=(), allow_all_permissions=False, args=(), max_heap_bytes=None, execution_deadline=None, features=None)"
)]
#[allow(clippy::too_many_arguments)]
fn run_source(
    py: Python<'_>,
    code: &Bound<'_, PyAny>,
    lang: Option<Bound<'_, PyAny>>,
    base_dir: Option<Bound<'_, PyAny>>,
    permissions: Py<PyAny>,
    allow_all_permissions: bool,
    args: Py<PyAny>,
    max_heap_bytes: Option<Bound<'_, PyAny>>,
    execution_deadline: Option<Bound<'_, PyAny>>,
    features: Option<Py<PyAny>>,
) -> PyResult<i32> {
    let code = source_bytes(code)?;
    let lang = source_lang_from_python(lang.as_ref())?;
    let base_dir = base_dir_from_python(base_dir.as_ref())?;
    let options = options_from_python(
        permissions.bind(py),
        allow_all_permissions,
        args.bind(py),
        None,
        max_heap_bytes.as_ref(),
        execution_deadline.as_ref(),
        features.as_ref().map(|value| value.bind(py)),
    )?;
    let result = py.detach(|| libdeno::run_source(code, lang, base_dir, &options));
    result.map_err(core_error_to_py)
}

#[pyfunction]
#[pyo3(
    signature = (code, *, lang=None, base_dir=None, permissions=empty_tuple(), allow_all_permissions=false, args=empty_tuple(), max_heap_bytes=None, execution_deadline=None, features=None, capture_stdout=true, capture_stderr=true, max_capture_bytes=None),
    text_signature = "(code, *, lang='javascript', base_dir='.', permissions=(), allow_all_permissions=False, args=(), max_heap_bytes=None, execution_deadline=None, features=None, capture_stdout=True, capture_stderr=True, max_capture_bytes=None)"
)]
#[allow(clippy::too_many_arguments)]
fn run_source_with_output(
    py: Python<'_>,
    code: &Bound<'_, PyAny>,
    lang: Option<Bound<'_, PyAny>>,
    base_dir: Option<Bound<'_, PyAny>>,
    permissions: Py<PyAny>,
    allow_all_permissions: bool,
    args: Py<PyAny>,
    max_heap_bytes: Option<Bound<'_, PyAny>>,
    execution_deadline: Option<Bound<'_, PyAny>>,
    features: Option<Py<PyAny>>,
    capture_stdout: bool,
    capture_stderr: bool,
    max_capture_bytes: Option<Bound<'_, PyAny>>,
) -> PyResult<RunOutput> {
    let code = source_bytes(code)?;
    let lang = source_lang_from_python(lang.as_ref())?;
    let base_dir = base_dir_from_python(base_dir.as_ref())?;
    let mut options = options_from_python(
        permissions.bind(py),
        allow_all_permissions,
        args.bind(py),
        None,
        max_heap_bytes.as_ref(),
        execution_deadline.as_ref(),
        features.as_ref().map(|value| value.bind(py)),
    )?;
    apply_capture(
        &mut options,
        capture_stdout,
        capture_stderr,
        max_capture_bytes.as_ref(),
    )?;
    let result = py.detach(|| libdeno::run_source_with_output(code, lang, base_dir, &options));
    result.map(RunOutput::from).map_err(core_error_to_py)
}

/// `base_dir` defaults to the process cwd, which is what `"."` resolves to.
pub(crate) fn base_dir_from_python(value: Option<&Bound<'_, PyAny>>) -> PyResult<PathBuf> {
    match value {
        Some(value) => path_from_python(value, "base_dir"),
        None => Ok(PathBuf::from(".")),
    }
}

pub(crate) fn empty_tuple() -> Py<PyAny> {
    Python::attach(|py| PyTuple::empty(py).into_any().unbind())
}

pub(crate) fn path_from_python(value: &Bound<'_, PyAny>, name: &str) -> PyResult<PathBuf> {
    let os = value.py().import("os").map_err(|error| {
        ConfigurationError::new_err(format!("{name} must be str or os.PathLike: {error}"))
    })?;
    let fspath = os.call_method1("fspath", (value,)).map_err(|error| {
        ConfigurationError::new_err(format!("{name} must be str or os.PathLike: {error}"))
    })?;
    // `fsencode` preserves Python's filesystem bytes/surrogateescape semantics
    // instead of routing paths through lossy UTF-8 conversion.
    let encoded = os.call_method1("fsencode", (fspath,)).map_err(|error| {
        ConfigurationError::new_err(format!("{name} must be str or os.PathLike: {error}"))
    })?;

    #[cfg(unix)]
    {
        let bytes = encoded.extract::<Vec<u8>>().map_err(|error| {
            ConfigurationError::new_err(format!("{name} must be str or os.PathLike: {error}"))
        })?;
        Ok(PathBuf::from(OsString::from_vec(bytes)))
    }

    #[cfg(windows)]
    {
        // Windows paths are represented as UTF-16. Decode through Python's
        // filesystem codec, then encode with surrogatepass so undecodable
        // filesystem bytes are not replaced or dropped.
        let decoded = os.call_method1("fsdecode", (encoded,)).map_err(|error| {
            ConfigurationError::new_err(format!("{name} must be str or os.PathLike: {error}"))
        })?;
        let utf16 = decoded
            .call_method1("encode", ("utf-16-le", "surrogatepass"))
            .map_err(|error| {
                ConfigurationError::new_err(format!("{name} must be str or os.PathLike: {error}"))
            })?;
        let bytes = utf16.extract::<Vec<u8>>().map_err(|error| {
            ConfigurationError::new_err(format!("{name} must be str or os.PathLike: {error}"))
        })?;
        if bytes.len() % 2 != 0 {
            return Err(ConfigurationError::new_err(format!(
                "{name} must be str or os.PathLike: invalid UTF-16 path"
            )));
        }
        let wide: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        Ok(PathBuf::from(OsString::from_wide(&wide)))
    }
}

// Only list/tuple: a dict is iterable, so a plain extract would silently take
// its keys and drop the values.
pub(crate) fn string_sequence(value: &Bound<'_, PyAny>, name: &str) -> PyResult<Vec<String>> {
    if !(value.is_instance_of::<PyList>() || value.is_instance_of::<PyTuple>()) {
        return Err(ConfigurationError::new_err(format!(
            "{name} must be a list or tuple of strings"
        )));
    }
    value.extract::<Vec<String>>().map_err(|error| {
        ConfigurationError::new_err(format!(
            "{name} must be a list or tuple of strings: {error}"
        ))
    })
}

// `resolve_entry` only builds a file URL, so a missing file surfaces later as
// a "Module not found" module-graph error (a Core error). Check the path up
// front so the documented EntryError case is actually reachable.
// shortcut: only the path itself is checked, so a directory whose package.json
// `main` names a missing file still reports DenoRuntimeError; upgrade by
// resolving `main` here if that case must be EntryError too.
pub(crate) fn check_entry_exists(entry: &Path, cwd: &Path) -> Result<(), String> {
    let resolved = if entry.is_absolute() {
        entry.to_path_buf()
    } else {
        cwd.join(entry)
    };
    if resolved.exists() {
        Ok(())
    } else {
        Err(format!(
            "failed to resolve entry module: {} does not exist",
            resolved.display()
        ))
    }
}

/// Runs the entry stat with the GIL released: a stalled network filesystem must
/// not freeze every other Python thread during preparation.
pub(crate) fn check_entry_detached(py: Python<'_>, entry: &Path, cwd: &Path) -> PyResult<()> {
    py.detach(|| check_entry_exists(entry, cwd))
        .map_err(EntryError::new_err)
}

pub(crate) fn core_error_to_py(error: CoreError) -> PyErr {
    let is_permission = error.is_permission_error();
    let message = error.to_string();
    match error {
        CoreError::Entry(_) => EntryError::new_err(message),
        CoreError::Permission(_) => DenoPermissionError::new_err(message),
        CoreError::Configuration(_) => ConfigurationError::new_err(message),
        CoreError::Runtime(_) if is_permission => DenoPermissionError::new_err(message),
        CoreError::Runtime(_) => DenoRuntimeError::new_err(message),
        CoreError::Core(_) if is_permission => DenoPermissionError::new_err(message),
        CoreError::Core(_) => DenoRuntimeError::new_err(message),
        CoreError::Io(_) => DenoIOError::new_err(message),
        CoreError::Timeout(_) => DenoTimeoutError::new_err(message),
    }
}

#[pymodule]
fn _libdeno(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(run, module)?)?;
    module.add_function(wrap_pyfunction!(run_with_output, module)?)?;
    module.add_function(wrap_pyfunction!(run_source, module)?)?;
    module.add_function(wrap_pyfunction!(run_source_with_output, module)?)?;
    module.add_class::<Runtime>()?;
    module.add_class::<RunOutput>()?;
    module.add_class::<SourceLang>()?;
    let py = module.py();
    module.add("LibdenoError", py.get_type::<LibdenoError>())?;
    module.add("EntryError", py.get_type::<EntryError>())?;
    module.add("DenoPermissionError", py.get_type::<DenoPermissionError>())?;
    module.add("ConfigurationError", py.get_type::<ConfigurationError>())?;
    module.add("DenoRuntimeError", py.get_type::<DenoRuntimeError>())?;
    module.add("DenoIOError", py.get_type::<DenoIOError>())?;
    module.add("DenoTimeoutError", py.get_type::<DenoTimeoutError>())?;
    Ok(())
}
