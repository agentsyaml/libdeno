# Python API

`pip install libdeno` installs the same runtime as the Rust crate; both are
released together at the same version. The binding is alpha: it is synchronous
and covers script execution only.

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
cwd is never changed. `cwd` is a `run()` argument only.

## Arguments

All arguments after `entry` are keyword-only:

| argument | meaning |
|---|---|
| `permissions` | CLI-style `--allow-*` strings. An empty sequence raises `ConfigurationError` unless `allow_all_permissions=True`; prompting is not exposed. |
| `allow_all_permissions` | Equivalent to `--allow-all`. |
| `args` | Script arguments exposed through `process.argv`. |
| `cwd` | Base directory for entry and imports (`run()` only). Defaults to the process cwd. |
| `features` | Unstable Deno features to enable; `None` keeps the runtime defaults (`kv`, `cron`, `ffi`, `webgpu`, `worker-options`). |
| `max_heap_bytes` | V8 old-generation constraint. Below 8 MiB raises `ConfigurationError`. It bounds neither native allocations nor RSS. |
| `execution_deadline` | Deadline in seconds; must be finite and non-negative. Best effort: blocking system calls, native code, and blocked permission paths can overrun it. |

## Errors

`LibdenoError` is the base class:

- `EntryError` — entry resolution failed.
- `DenoPermissionError` — permission flags are invalid or a run was denied.
- `ConfigurationError` — the arguments cannot form a valid run.
- `DenoRuntimeError` — runtime startup or JavaScript failure.
- `DenoIOError` — host filesystem or I/O failure.
- `DenoTimeoutError` — the execution deadline fired.

## Not exposed

Output capture, async APIs, subprocess helpers, permission prompts and hooks,
and `.node` native addons. For those, use the Rust crate.

To build a wheel from this repository, see
[python/README.md](../python/README.md). The page also lists the current
limitations (no sdist).
