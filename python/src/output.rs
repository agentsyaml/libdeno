use pyo3::prelude::*;
use pyo3::types::PyBytes;

/// Captured result of a run; mirrors `libdeno::RunOutput`.
#[pyclass(module = "libdeno", frozen)]
pub(crate) struct RunOutput {
    #[pyo3(get)]
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    #[pyo3(get)]
    capture_truncated: bool,
}

impl From<libdeno::RunOutput> for RunOutput {
    fn from(output: libdeno::RunOutput) -> Self {
        Self {
            exit_code: output.exit_code,
            stdout: output.stdout,
            stderr: output.stderr,
            capture_truncated: output.capture_truncated,
        }
    }
}

#[pymethods]
impl RunOutput {
    #[getter]
    fn stdout<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.stdout)
    }

    #[getter]
    fn stderr<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.stderr)
    }

    fn __repr__(&self) -> String {
        format!(
            "RunOutput(exit_code={}, stdout={} bytes, stderr={} bytes, capture_truncated={})",
            self.exit_code,
            self.stdout.len(),
            self.stderr.len(),
            if self.capture_truncated {
                "True"
            } else {
                "False"
            },
        )
    }
}
