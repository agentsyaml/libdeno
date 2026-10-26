use pyo3::prelude::*;
use pyo3::types::{PyByteArray, PyBytes, PyString};

use crate::ConfigurationError;

/// Language of an in-memory entry source; mirrors `libdeno::SourceLang`.
#[pyclass(module = "libdeno", eq, eq_int, from_py_object)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SourceLang {
    JavaScript,
    TypeScript,
}

impl SourceLang {
    fn into_core(self) -> libdeno::SourceLang {
        match self {
            Self::JavaScript => libdeno::SourceLang::JavaScript,
            Self::TypeScript => libdeno::SourceLang::TypeScript,
        }
    }
}

/// Accepts a `SourceLang` member or the strings "javascript"/"typescript"
/// (case-insensitive); `None` keeps the JavaScript default.
pub(crate) fn source_lang_from_python(
    value: Option<&Bound<'_, PyAny>>,
) -> PyResult<libdeno::SourceLang> {
    let Some(value) = value else {
        return Ok(libdeno::SourceLang::JavaScript);
    };
    if let Ok(lang) = value.extract::<SourceLang>() {
        return Ok(lang.into_core());
    }
    if let Ok(text) = value.cast::<PyString>() {
        // `to_str` is unavailable under the abi3-py39 limited API.
        let text = text.to_cow().map_err(|error| {
            ConfigurationError::new_err(format!("lang must be valid UTF-8 text: {error}"))
        })?;
        return match text.to_ascii_lowercase().as_str() {
            "javascript" => Ok(libdeno::SourceLang::JavaScript),
            "typescript" => Ok(libdeno::SourceLang::TypeScript),
            _ => Err(invalid_lang()),
        };
    }
    Err(invalid_lang())
}

fn invalid_lang() -> PyErr {
    ConfigurationError::new_err(
        "lang must be SourceLang.JavaScript, SourceLang.TypeScript, 'javascript', or 'typescript'",
    )
}

/// `code` accepts `str` (encoded as UTF-8), `bytes`, or `bytearray`.
pub(crate) fn source_bytes(value: &Bound<'_, PyAny>) -> PyResult<Vec<u8>> {
    if let Ok(text) = value.cast::<PyString>() {
        let text = text.to_cow().map_err(|error| {
            ConfigurationError::new_err(format!("code must be valid UTF-8 text: {error}"))
        })?;
        return Ok(text.as_bytes().to_vec());
    }
    if let Ok(bytes) = value.cast::<PyBytes>() {
        return Ok(bytes.as_bytes().to_vec());
    }
    if let Ok(bytearray) = value.cast::<PyByteArray>() {
        return Ok(bytearray.to_vec());
    }
    Err(ConfigurationError::new_err(
        "code must be str, bytes, or bytearray",
    ))
}
