use std::path::PathBuf;

use pyo3::prelude::*;

use crate::options::{apply_capture, options_from_python, validate_options};
use crate::output::RunOutput;
use crate::source::{source_bytes, source_lang_from_python};
use crate::{
    base_dir_from_python, check_entry_detached, core_error_to_py, empty_tuple, path_from_python,
    DenoIOError, DenoRuntimeError,
};

#[pyclass(module = "libdeno")]
pub(crate) struct Runtime {
    inner: libdeno::LibdenoRuntime,
    cwd: PathBuf,
}

#[pymethods]
impl Runtime {
    #[new]
    #[pyo3(
        signature = (cwd=None),
        text_signature = "(cwd=None)"
    )]
    fn new(py: Python<'_>, cwd: Option<Bound<'_, PyAny>>) -> PyResult<Self> {
        let cwd = match cwd {
            Some(cwd) => path_from_python(&cwd, "cwd")?,
            None => py.detach(std::env::current_dir).map_err(|error| {
                DenoIOError::new_err(format!("failed to get current directory: {error}"))
            })?,
        };
        let result: Result<(libdeno::LibdenoRuntime, PathBuf), String> = py.detach(|| {
            // Canonicalize like LibdenoRuntime::new so the entry check resolves
            // against the same base the reusable stack uses, and so a later
            // process-cwd change cannot move it.
            let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
            let tokio_runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("failed to create Tokio runtime: {error}"))?;
            let inner = tokio_runtime
                .block_on(libdeno::LibdenoRuntime::new(cwd.clone()))
                .map_err(|error| error.to_string())?;
            Ok((inner, cwd))
        });
        result
            .map(|(inner, cwd)| Self { inner, cwd })
            .map_err(DenoRuntimeError::new_err)
    }

    fn refresh(&self) {
        self.inner.refresh();
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
        max_heap_bytes: Option<Bound<'_, PyAny>>,
        execution_deadline: Option<Bound<'_, PyAny>>,
        features: Option<Py<PyAny>>,
    ) -> PyResult<i32> {
        let entry = path_from_python(entry, "entry")?;
        let options = runtime_options(
            py,
            permissions,
            allow_all_permissions,
            args,
            max_heap_bytes.as_ref(),
            execution_deadline.as_ref(),
            features.as_ref(),
        )?;
        // Same ordering as `run()`: argument validation first, then the entry.
        validate_options(&options)?;
        check_entry_detached(py, &entry, &self.cwd)?;
        let result = py.detach(|| libdeno::run_with(&self.inner, entry, &options));
        result.map_err(core_error_to_py)
    }

    #[pyo3(
        signature = (entry, *, permissions=empty_tuple(), allow_all_permissions=false, args=empty_tuple(), max_heap_bytes=None, execution_deadline=None, features=None, capture_stdout=true, capture_stderr=true, max_capture_bytes=None),
        text_signature = "($self, entry, *, permissions=(), allow_all_permissions=False, args=(), max_heap_bytes=None, execution_deadline=None, features=None, capture_stdout=True, capture_stderr=True, max_capture_bytes=None)"
    )]
    #[allow(clippy::too_many_arguments)]
    fn run_with_output(
        &self,
        py: Python<'_>,
        entry: &Bound<'_, PyAny>,
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
        let entry = path_from_python(entry, "entry")?;
        let mut options = runtime_options(
            py,
            permissions,
            allow_all_permissions,
            args,
            max_heap_bytes.as_ref(),
            execution_deadline.as_ref(),
            features.as_ref(),
        )?;
        validate_options(&options)?;
        apply_capture(
            &mut options,
            capture_stdout,
            capture_stderr,
            max_capture_bytes.as_ref(),
        )?;
        check_entry_detached(py, &entry, &self.cwd)?;
        let result = py.detach(|| libdeno::runtime::run_with_output(&self.inner, entry, &options));
        result.map(RunOutput::from).map_err(core_error_to_py)
    }

    #[pyo3(
        signature = (code, *, lang=None, base_dir=None, permissions=empty_tuple(), allow_all_permissions=false, args=empty_tuple(), max_heap_bytes=None, execution_deadline=None, features=None),
        text_signature = "($self, code, *, lang='javascript', base_dir='.', permissions=(), allow_all_permissions=False, args=(), max_heap_bytes=None, execution_deadline=None, features=None)"
    )]
    #[allow(clippy::too_many_arguments)]
    fn run_source(
        &self,
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
        let options = runtime_options(
            py,
            permissions,
            allow_all_permissions,
            args,
            max_heap_bytes.as_ref(),
            execution_deadline.as_ref(),
            features.as_ref(),
        )?;
        let result = py.detach(|| {
            libdeno::runtime::run_with_source(&self.inner, code, lang, base_dir, &options)
        });
        result.map_err(core_error_to_py)
    }

    #[pyo3(
        signature = (code, *, lang=None, base_dir=None, permissions=empty_tuple(), allow_all_permissions=false, args=empty_tuple(), max_heap_bytes=None, execution_deadline=None, features=None, capture_stdout=true, capture_stderr=true, max_capture_bytes=None),
        text_signature = "($self, code, *, lang='javascript', base_dir='.', permissions=(), allow_all_permissions=False, args=(), max_heap_bytes=None, execution_deadline=None, features=None, capture_stdout=True, capture_stderr=True, max_capture_bytes=None)"
    )]
    #[allow(clippy::too_many_arguments)]
    fn run_source_with_output(
        &self,
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
        let mut options = runtime_options(
            py,
            permissions,
            allow_all_permissions,
            args,
            max_heap_bytes.as_ref(),
            execution_deadline.as_ref(),
            features.as_ref(),
        )?;
        apply_capture(
            &mut options,
            capture_stdout,
            capture_stderr,
            max_capture_bytes.as_ref(),
        )?;
        let result = py.detach(|| {
            libdeno::runtime::run_with_output_source(&self.inner, code, lang, base_dir, &options)
        });
        result.map(RunOutput::from).map_err(core_error_to_py)
    }
}

/// Runtime options never carry `cwd`: the reusable stack is scoped to the
/// runtime's own directory and Rust rejects a mismatched base.
#[allow(clippy::too_many_arguments)]
fn runtime_options(
    py: Python<'_>,
    permissions: Py<PyAny>,
    allow_all_permissions: bool,
    args: Py<PyAny>,
    max_heap_bytes: Option<&Bound<'_, PyAny>>,
    execution_deadline: Option<&Bound<'_, PyAny>>,
    features: Option<&Py<PyAny>>,
) -> PyResult<libdeno::LibdenoOptions> {
    options_from_python(
        permissions.bind(py),
        allow_all_permissions,
        args.bind(py),
        None,
        max_heap_bytes,
        execution_deadline,
        features.map(|value| value.bind(py)),
    )
}
