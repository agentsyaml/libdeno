"""Python bindings for the libdeno runtime.

`permissions`, `args`, and `features` accept a list or tuple of strings;
any other sequence type (including `str` itself) raises `ConfigurationError`.
"""

# Maturin's requested `python-source = "libdeno"` treats this directory as
# the source root, so mirror the facade for the wheel's `libdeno` package.
from ._libdeno import (
    ConfigurationError,
    DenoIOError,
    DenoPermissionError,
    DenoRuntimeError,
    DenoTimeoutError,
    EntryError,
    LibdenoError,
    RunOutput,
    Runtime,
    SourceLang,
    run,
    run_source,
    run_source_with_output,
    run_with_output,
)

__all__ = [
    "run",
    "run_with_output",
    "run_source",
    "run_source_with_output",
    "Runtime",
    "RunOutput",
    "SourceLang",
    "LibdenoError",
    "EntryError",
    "DenoPermissionError",
    "ConfigurationError",
    "DenoRuntimeError",
    "DenoIOError",
    "DenoTimeoutError",
]
