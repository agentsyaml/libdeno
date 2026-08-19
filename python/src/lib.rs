use std::ffi::OsString;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;

use libdeno::LibdenoError as CoreError;
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyTuple;

create_exception!(_libdeno, LibdenoError, PyException);
create_exception!(_libdeno, EntryError, LibdenoError);
create_exception!(_libdeno, DenoPermissionError, LibdenoError);
create_exception!(_libdeno, ConfigurationError, LibdenoError);
create_exception!(_libdeno, DenoRuntimeError, LibdenoError);
create_exception!(_libdeno, DenoIOError, LibdenoError);
create_exception!(_libdeno, DenoTimeoutError, LibdenoError);

#[pyclass(module = "libdeno")]
struct Runtime {
    inner: libdeno::LibdenoRuntime,
}

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
    max_heap_bytes: Option<usize>,
    execution_deadline: Option<f64>,
    features: Option<Py<PyAny>>,
) -> PyResult<i32> {
    let entry = path_from_python(entry, "entry")?;
    let permissions = permissions.bind(py);
    let args = args.bind(py);
    let features = features.as_ref().map(|value| value.bind(py));
    let options = options_from_python(
        permissions,
        allow_all_permissions,
        args,
        cwd.as_ref(),
        max_heap_bytes,
        execution_deadline,
        features,
    )?;
    let result = py.detach(|| libdeno::run(entry, &options));
    result.map_err(core_error_to_py)
}

#[pymethods]
impl Runtime {
    #[new]
    fn new(py: Python<'_>, cwd: &Bound<'_, PyAny>) -> PyResult<Self> {
        let cwd = path_from_python(cwd, "cwd")?;
        let result: Result<libdeno::LibdenoRuntime, String> = py.detach(|| {
            let tokio_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("failed to create Tokio runtime: {error}"))?;
            tokio_runtime
                .block_on(libdeno::LibdenoRuntime::new(cwd))
                .map_err(|error| error.to_string())
        });
        result
            .map(|inner| Self { inner })
            .map_err(DenoRuntimeError::new_err)
    }

    #[pyo3(
        signature = (entry, *, permissions=empty_tuple(), allow_all_permissions=false, args=empty_tuple(), max_heap_bytes=None, execution_deadline=None, features=None),
        text_signature = "($self, entry, *, permissions=(), allow_all_permissions=False, args=(), max_heap_bytes=None, execution_deadline=None, features=None)"
    )]
    fn run(
        &self,
        py: Python<'_>,
        entry: &Bound<'_, PyAny>,
        permissions: Py<PyAny>,
        allow_all_permissions: bool,
        args: Py<PyAny>,
        max_heap_bytes: Option<usize>,
        execution_deadline: Option<f64>,
        features: Option<Py<PyAny>>,
    ) -> PyResult<i32> {
        let entry = path_from_python(entry, "entry")?;
        let permissions = permissions.bind(py);
        let args = args.bind(py);
        let features = features.as_ref().map(|value| value.bind(py));
        let options = options_from_python(
            permissions,
            allow_all_permissions,
            args,
            None,
            max_heap_bytes,
            execution_deadline,
            features,
        )?;
        let result = py.detach(|| libdeno::run_with(&self.inner, entry, &options));
        result.map_err(core_error_to_py)
    }
}

fn empty_tuple() -> Py<PyAny> {
    Python::attach(|py| PyTuple::empty(py).into_any().unbind())
}

fn path_from_python(value: &Bound<'_, PyAny>, name: &str) -> PyResult<PathBuf> {
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
                ConfigurationError::new_err(format!(
                    "{name} must be str or os.PathLike: {error}"
                ))
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

fn string_sequence(value: &Bound<'_, PyAny>, name: &str) -> PyResult<Vec<String>> {
    value.extract::<Vec<String>>().map_err(|error| {
        ConfigurationError::new_err(format!("{name} must be an iterable of strings: {error}"))
    })
}

fn options_from_python(
    permissions: &Bound<'_, PyAny>,
    allow_all_permissions: bool,
    args: &Bound<'_, PyAny>,
    cwd: Option<&Bound<'_, PyAny>>,
    max_heap_bytes: Option<usize>,
    execution_deadline: Option<f64>,
    features: Option<&Bound<'_, PyAny>>,
) -> PyResult<libdeno::LibdenoOptions> {
    let execution_deadline = execution_deadline
        .map(|seconds| {
            if seconds.is_finite() && seconds >= 0.0 {
                std::time::Duration::try_from_secs_f64(seconds).map_err(|_| {
                    ConfigurationError::new_err(
                        "execution_deadline is outside the supported duration range",
                    )
                })
            } else {
                Err(ConfigurationError::new_err(
                    "execution_deadline must be finite and non-negative",
                ))
            }
        })
        .transpose()?;
    Ok(libdeno::LibdenoOptions {
        permissions: string_sequence(permissions, "permissions")?,
        allow_all_permissions,
        args: string_sequence(args, "args")?,
        cwd: cwd
            .map(|value| path_from_python(value, "cwd"))
            .transpose()?,
        max_heap_bytes,
        execution_deadline,
        features: features
            .map(|value| string_sequence(value, "features"))
            .transpose()?,
        ..Default::default()
    })
}

fn core_error_to_py(error: CoreError) -> PyErr {
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
    module.add_class::<Runtime>()?;
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
