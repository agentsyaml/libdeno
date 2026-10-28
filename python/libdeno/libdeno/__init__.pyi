from os import PathLike
from typing import ClassVar, List, Tuple, Union


_PathArg = Union[str, bytes, PathLike[str], PathLike[bytes]]
_SourceArg = Union[str, bytes, bytearray]
# The binding accepts only list/tuple here (a dict would be iterated as keys).
_StrList = Union[List[str], Tuple[str, ...]]


class LibdenoError(Exception): ...


class EntryError(LibdenoError): ...


class DenoPermissionError(LibdenoError): ...


class ConfigurationError(LibdenoError): ...


class DenoRuntimeError(LibdenoError): ...


class DenoIOError(LibdenoError): ...


class DenoTimeoutError(LibdenoError): ...


class SourceLang:
    JavaScript: ClassVar[SourceLang]
    TypeScript: ClassVar[SourceLang]


_LangArg = Union[SourceLang, str]


class RunOutput:
    @property
    def exit_code(self) -> int: ...
    @property
    def stdout(self) -> bytes: ...
    @property
    def stderr(self) -> bytes: ...
    @property
    def capture_truncated(self) -> bool: ...
    def __repr__(self) -> str: ...


def run(
    entry: _PathArg,
    *,
    permissions: _StrList = ...,
    allow_all_permissions: bool = ...,
    args: _StrList = ...,
    cwd: Union[_PathArg, None] = ...,
    max_heap_bytes: Union[int, None] = ...,
    execution_deadline: Union[float, None] = ...,
    features: Union[_StrList, None] = ...,
) -> int: ...


def run_with_output(
    entry: _PathArg,
    *,
    permissions: _StrList = ...,
    allow_all_permissions: bool = ...,
    args: _StrList = ...,
    cwd: Union[_PathArg, None] = ...,
    max_heap_bytes: Union[int, None] = ...,
    execution_deadline: Union[float, None] = ...,
    features: Union[_StrList, None] = ...,
    capture_stdout: bool = ...,
    capture_stderr: bool = ...,
    max_capture_bytes: Union[int, None] = ...,
) -> RunOutput: ...


def run_source(
    code: _SourceArg,
    *,
    lang: _LangArg = SourceLang.JavaScript,
    base_dir: _PathArg = ".",
    permissions: _StrList = ...,
    allow_all_permissions: bool = ...,
    args: _StrList = ...,
    max_heap_bytes: Union[int, None] = ...,
    execution_deadline: Union[float, None] = ...,
    features: Union[_StrList, None] = ...,
) -> int: ...


def run_source_with_output(
    code: _SourceArg,
    *,
    lang: _LangArg = SourceLang.JavaScript,
    base_dir: _PathArg = ".",
    permissions: _StrList = ...,
    allow_all_permissions: bool = ...,
    args: _StrList = ...,
    max_heap_bytes: Union[int, None] = ...,
    execution_deadline: Union[float, None] = ...,
    features: Union[_StrList, None] = ...,
    capture_stdout: bool = ...,
    capture_stderr: bool = ...,
    max_capture_bytes: Union[int, None] = ...,
) -> RunOutput: ...


class Runtime:
    def __init__(self, cwd: Union[_PathArg, None] = ...) -> None: ...

    def refresh(self) -> None: ...

    def run(
        self,
        entry: _PathArg,
        *,
        permissions: _StrList = ...,
        allow_all_permissions: bool = ...,
        args: _StrList = ...,
        max_heap_bytes: Union[int, None] = ...,
        execution_deadline: Union[float, None] = ...,
        features: Union[_StrList, None] = ...,
    ) -> int: ...

    def run_with_output(
        self,
        entry: _PathArg,
        *,
        permissions: _StrList = ...,
        allow_all_permissions: bool = ...,
        args: _StrList = ...,
        max_heap_bytes: Union[int, None] = ...,
        execution_deadline: Union[float, None] = ...,
        features: Union[_StrList, None] = ...,
        capture_stdout: bool = ...,
        capture_stderr: bool = ...,
        max_capture_bytes: Union[int, None] = ...,
    ) -> RunOutput: ...

    def run_source(
        self,
        code: _SourceArg,
        *,
        lang: _LangArg = SourceLang.JavaScript,
        base_dir: _PathArg = ".",
        permissions: _StrList = ...,
        allow_all_permissions: bool = ...,
        args: _StrList = ...,
        max_heap_bytes: Union[int, None] = ...,
        execution_deadline: Union[float, None] = ...,
        features: Union[_StrList, None] = ...,
    ) -> int: ...

    def run_source_with_output(
        self,
        code: _SourceArg,
        *,
        lang: _LangArg = SourceLang.JavaScript,
        base_dir: _PathArg = ".",
        permissions: _StrList = ...,
        allow_all_permissions: bool = ...,
        args: _StrList = ...,
        max_heap_bytes: Union[int, None] = ...,
        execution_deadline: Union[float, None] = ...,
        features: Union[_StrList, None] = ...,
        capture_stdout: bool = ...,
        capture_stderr: bool = ...,
        max_capture_bytes: Union[int, None] = ...,
    ) -> RunOutput: ...
