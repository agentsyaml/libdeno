from os import PathLike
from typing import Sequence, Union


_PathArg = Union[str, bytes, PathLike[str], PathLike[bytes]]


class LibdenoError(Exception): ...


class EntryError(LibdenoError): ...


class DenoPermissionError(LibdenoError): ...


class ConfigurationError(LibdenoError): ...


class DenoRuntimeError(LibdenoError): ...


class DenoIOError(LibdenoError): ...


class DenoTimeoutError(LibdenoError): ...


def run(
    entry: _PathArg,
    *,
    permissions: Sequence[str] = ...,
    allow_all_permissions: bool = ...,
    args: Sequence[str] = ...,
    cwd: Union[_PathArg, None] = ...,
    max_heap_bytes: Union[int, None] = ...,
    execution_deadline: Union[float, None] = ...,
    features: Union[Sequence[str], None] = ...,
) -> int: ...


class Runtime:
    def __init__(self, cwd: Union[_PathArg, None] = ...) -> None: ...

    def run(
        self,
        entry: _PathArg,
        *,
        permissions: Sequence[str] = ...,
        allow_all_permissions: bool = ...,
        args: Sequence[str] = ...,
        max_heap_bytes: Union[int, None] = ...,
        execution_deadline: Union[float, None] = ...,
        features: Union[Sequence[str], None] = ...,
    ) -> int: ...
