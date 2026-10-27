# Python API

`pip install libdeno` installs the same runtime as the Rust crate; both are
released together at the same version. The binding is alpha: it runs scripts
synchronously, including in-memory sources, and can capture their output.

```bash
python -m pip install libdeno
python -c 'import libdeno; print(libdeno.run("app.js", allow_all_permissions=True))'
```

Wheels require Python 3.9+ (stable ABI) and exist for Linux x86_64/aarch64,
macOS x86_64/arm64, and Windows x64.

## Run a script

```python
import libdeno

exit_code = libdeno.run("app.js", permissions=("--allow-read=.",))
```

`run()` blocks until the script and its event loop finish, returns the exit
code, and releases the GIL while Rust runs. `entry` accepts a file, a
directory, or its `package.json`, as `str`, `bytes`, or `os.PathLike`
including non-ASCII paths. Directory entries use `package.json`'s `main`,
defaulting to `index.js`.

## Reuse project state

```python
runtime = libdeno.Runtime("my-app")
runtime.run("app.js", allow_all_permissions=True)
runtime.run("app.js", permissions=("--allow-read=.",))
```

`Runtime` fixes its directory when constructed and reuses the resolver stack
across runs; permissions, module graphs, and isolates stay per run. The process
cwd is never changed. `cwd` exists only on the path entry points (`run`,
`run_with_output`).

## Capture output

```python
output = libdeno.run_with_output("app.js", allow_all_permissions=True)
print(output.exit_code, output.stdout, output.stderr, output.capture_truncated)
```

`run_with_output()` returns a `RunOutput` with `exit_code`, `stdout`, `stderr`
(all captured bytes) and `capture_truncated`. Both streams are captured by
default; pass `capture_stdout=False` or `capture_stderr=False` to let one
through to the terminal, in which case that buffer is empty.
`max_capture_bytes` caps each stream and sets `capture_truncated` when a stream
is cut. Capture redirects process-global file descriptors, so a captured run is
exclusive: a concurrent run raises `ConfigurationError`. It is not supported on
Windows.

## Run source from memory

```python
libdeno.run_source("console.log('hi');", allow_all_permissions=True)
output = libdeno.run_source_with_output(
    "const answer: number = 42;",
    lang=libdeno.SourceLang.TypeScript,
    allow_all_permissions=True,
)
```

`run_source()` and `run_source_with_output()` take `code` as `str`, `bytes`, or
`bytearray`, and place the virtual entry inside `base_dir` (default `.`), so
relative imports resolve against it. `base_dir` must exist, otherwise
`ConfigurationError`. `lang` accepts `SourceLang.JavaScript`,
`SourceLang.TypeScript`, or the strings `"javascript"`/`"typescript"`
(case-insensitive). `Runtime.run_source()` and
`Runtime.run_source_with_output()` use the runtime's directory instead.

## Arguments

All arguments after `entry` (or `code`) are keyword-only:

| argument | meaning |
|---|---|
| `permissions` | A list or tuple of CLI-style `--allow-*` strings; other types raise `ConfigurationError`. An empty sequence raises `ConfigurationError` unless `allow_all_permissions=True`; prompting is not exposed. |
| `allow_all_permissions` | Equivalent to `--allow-all`. |
| `args` | A list or tuple of script arguments exposed through `process.argv`. |
| `cwd` | Base directory for entry and imports (path entry points only). Defaults to the process cwd. |
| `lang` | Source language for the in-memory entry points: `SourceLang.JavaScript` (default) or `SourceLang.TypeScript`. |
| `base_dir` | Directory the virtual source entry lives in; relative imports resolve against it. Defaults to `.`. |
| `capture_stdout` / `capture_stderr` | Capture flags on the `*_with_output` entry points; both default to `True`. |
| `max_capture_bytes` | Per-stream capture cap; truncation sets `capture_truncated`. |
| `features` | A list or tuple of unstable Deno features to enable; `None` keeps the runtime defaults (`kv`, `cron`, `ffi`, `webgpu`, `worker-options`). |
| `max_heap_bytes` | V8 old-generation constraint. Must be an integer (not `bool`); below 8 MiB raises `ConfigurationError`. It bounds neither native allocations nor RSS. |
| `execution_deadline` | Deadline in seconds; must be finite and non-negative. Best effort: blocking system calls, native code, and blocked permission paths can overrun it. |

`Runtime` methods do not accept `cwd`; the runtime's directory is fixed at
construction and Rust rejects a mismatched base, so the argument is omitted
rather than silently ignored.

## Errors

`LibdenoError` is the base class:

- `EntryError` — the entry path does not exist or entry resolution fails, such as
  when `package.json` is malformed or `main` escapes its directory. If `main`
  names a missing file, module loading fails with `DenoRuntimeError`.
- `DenoPermissionError` — permission flags are invalid or a run was denied.
- `ConfigurationError` — the arguments cannot form a valid run.
- `DenoRuntimeError` — runtime startup or JavaScript failure.
- `DenoIOError` — host filesystem or I/O failure.
- `DenoTimeoutError` — the execution deadline fired.

## Not exposed

- Async APIs. The Rust futures are `!Send`, so a Python bridge would need a
  dedicated thread per call. Use `asyncio.to_thread(libdeno.run, ...)` instead;
  it already releases the GIL for the whole run.
- Subprocess helpers. Child mode must be serviced by the host process at
  startup, which a Python interpreter cannot do.
- Permission prompts and hooks.
- `.node` native addons.

For those, use the Rust crate.

To build a source distribution or wheel from this repository, see
[python/README-pypi.md](../python/README-pypi.md).
