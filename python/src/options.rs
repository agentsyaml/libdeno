use std::path::PathBuf;
use std::time::Instant;

use libdeno::LibdenoOptions;
use pyo3::prelude::*;
use pyo3::types::PyBool;

use crate::{path_from_python, string_sequence, ConfigurationError};

/// Mirrors `MIN_V8_OLD_GENERATION_BYTES` in `src/limits.rs`.
const MIN_V8_OLD_GENERATION_BYTES: usize = 8 << 20;

/// Rejects `bool` (an `int` subclass, so `True` would otherwise become 1) and
/// non-integers, so a bad value reports the argument name.
pub(crate) fn optional_usize(
    value: Option<&Bound<'_, PyAny>>,
    name: &str,
) -> PyResult<Option<usize>> {
    value
        .map(|value| {
            if value.is_instance_of::<PyBool>() {
                return Err(ConfigurationError::new_err(format!(
                    "{name} must be an integer"
                )));
            }
            value.extract::<usize>().map_err(|error| {
                ConfigurationError::new_err(format!(
                    "{name} must be a non-negative integer within the platform usize range: {error}"
                ))
            })
        })
        .transpose()
}

fn optional_deadline(value: Option<&Bound<'_, PyAny>>) -> PyResult<Option<std::time::Duration>> {
    value
        .map(|value| {
            let seconds = value.extract::<f64>().map_err(|error| {
                ConfigurationError::new_err(format!("execution_deadline must be a number: {error}"))
            })?;
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
        .transpose()
}

pub(crate) fn options_from_python(
    permissions: &Bound<'_, PyAny>,
    allow_all_permissions: bool,
    args: &Bound<'_, PyAny>,
    cwd: Option<&Bound<'_, PyAny>>,
    max_heap_bytes: Option<&Bound<'_, PyAny>>,
    execution_deadline: Option<&Bound<'_, PyAny>>,
    features: Option<&Bound<'_, PyAny>>,
) -> PyResult<LibdenoOptions> {
    Ok(LibdenoOptions {
        permissions: string_sequence(permissions, "permissions")?,
        allow_all_permissions,
        args: string_sequence(args, "args")?,
        cwd: cwd
            .map(|value| path_from_python(value, "cwd"))
            .transpose()?,
        max_heap_bytes: optional_usize(max_heap_bytes, "max_heap_bytes")?,
        execution_deadline: optional_deadline(execution_deadline)?,
        features: features
            .map(|value| string_sequence(value, "features"))
            .transpose()?,
        ..Default::default()
    })
}

/// Mirrors `validate_max_heap_bytes` and `validate_execution_deadline` in
/// `src/limits.rs` (the source of truth; keep the two in sync). Validating here
/// keeps a missing entry from masking an invalid resource limit, which the
/// Rust API rejects as a configuration error.
pub(crate) fn validate_options(options: &LibdenoOptions) -> PyResult<()> {
    if let Some(bytes) = options.max_heap_bytes {
        if bytes < MIN_V8_OLD_GENERATION_BYTES {
            return Err(ConfigurationError::new_err(format!(
                "max_heap_bytes={bytes} is too small; use at least \
                 {MIN_V8_OLD_GENERATION_BYTES} bytes for a V8 old-generation limit"
            )));
        }
        if bytes == usize::MAX {
            return Err(ConfigurationError::new_err(
                "max_heap_bytes=usize::MAX cannot be represented as a finite V8 heap budget",
            ));
        }
    }
    if options
        .execution_deadline
        .is_some_and(|duration| Instant::now().checked_add(duration).is_none())
    {
        return Err(ConfigurationError::new_err(
            "execution deadline is too large for the host clock",
        ));
    }
    Ok(())
}

/// Applies the `*_with_output` capture keywords.
pub(crate) fn apply_capture(
    options: &mut LibdenoOptions,
    capture_stdout: bool,
    capture_stderr: bool,
    max_capture_bytes: Option<&Bound<'_, PyAny>>,
) -> PyResult<()> {
    options.capture_stdout = capture_stdout;
    options.capture_stderr = capture_stderr;
    options.max_capture_bytes = optional_usize(max_capture_bytes, "max_capture_bytes")?;
    Ok(())
}

/// Resolution base for a path entry: `options.cwd` when set, else the process
/// cwd, mirroring Rust's `effective_cwd`. Returns a plain message so the caller
/// can run it inside `py.detach` and build the Python error after reattaching.
pub(crate) fn entry_base(options: &LibdenoOptions) -> Result<PathBuf, String> {
    let current_dir = || {
        std::env::current_dir().map_err(|error| format!("failed to get current directory: {error}"))
    };
    match options.cwd.as_deref() {
        Some(cwd) if cwd.is_absolute() => Ok(cwd.to_path_buf()),
        Some(cwd) => Ok(current_dir()?.join(cwd)),
        None => current_dir(),
    }
}
