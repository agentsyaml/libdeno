# libdeno Python API

This is a private alpha Python binding for the Rust `libdeno` runtime. The
Python distribution and core Rust crate are both versioned at `0.3.2`. The
contract is deliberately small, synchronous, and fail-closed:

```python
import libdeno

exit_code = libdeno.run(
    "entry.js",
    permissions=("--allow-read=.",),
    args=("app-arg",),
)

runtime = libdeno.Runtime(cwd=None)
exit_code = runtime.run("entry.js", allow_all_permissions=True)
```

`run()` and `Runtime.run()` block until the script and its event loop finish and
return an integer exit code. The native call releases the Python GIL while the
Rust runtime executes. `Runtime` reuses its resolver stack while permissions,
module graphs, and isolates remain per run.

`entry` and `cwd` accept `str`, `bytes`, and `os.PathLike` values, including
non-ASCII paths. `cwd=None` (or an omitted `cwd`) means the process current
directory. For `Runtime`, that directory is fixed when the object is created;
the process cwd is never changed. Options after `entry` are keyword-only.

The Python binding does **not** expose output capture, async APIs, subprocess
helpers, or permission hooks. Script-level Deno APIs still follow the Rust
runtime's permission model.

## Options and limits

- `permissions` contains CLI-style `--allow-*` capability strings. An empty
  sequence is rejected with `ConfigurationError` unless
  `allow_all_permissions=True` is explicit. `-A`/`--allow-all` is equivalent.
- `args` are exposed through `process.argv`; `features` optionally selects
  valid Deno unstable feature names. `None` keeps the runtime defaults.
- `execution_deadline` is a finite, non-negative number of seconds. A busy
  JavaScript run normally raises `DenoTimeoutError` when the deadline fires,
  but this is best effort: blocking system calls, native code, and blocked
  permission paths can make a run exceed it.
- `max_heap_bytes` is an optional V8 old-generation constraint. Values below
  8 MiB are rejected with `ConfigurationError`; the value does not cap native
  allocations, external memory, host memory, RSS, CPU, or child processes.
  It is not an OS/process isolation boundary.

## Exceptions

The facade exports `LibdenoError` and these subclasses:

- `EntryError` — entry resolution failed.
- `DenoPermissionError` — invalid/denied permission access.
- `ConfigurationError` — options cannot form a valid run, including empty
  permissions without an explicit opt-in, invalid deadlines, or an undersized
  heap value.
- `DenoRuntimeError` — runtime startup or JavaScript failure.
- `DenoIOError` — host filesystem/I/O failure.
- `DenoTimeoutError` — the runtime reported an execution deadline timeout.

## Build and install a wheel

Use a virtual environment rather than installing build tools globally:

```bash
cd python
python -m venv .venv
.venv/bin/python -m pip install "maturin>=1.11,<2"
./build-wheel.sh
.venv/bin/python -m pip install target/wheels/*.whl
```

The wheel uses the Python 3.9 stable ABI and targets Python 3.9+. The build
script clears `RUSTC_WRAPPER`, resolves the active compiler dynamically, and
uses Cargo's locked dependency resolution.

Run the stdlib test suite after installing the wheel (from the repository
root, so the source package does not shadow the installed package):

```bash
python -m unittest discover -s python/tests -p 'test_*.py'
```

## Not supported yet

- Prompting and permission hooks.
- Output capture, async APIs, and subprocess execution.
- Python-facing permission/options objects beyond the keyword arguments above.
- Native addons.
- Source distributions (`sdist`); this prototype only defines a wheel build
  path.
- A stable public contract; this is a private alpha API and may change
  without notice.
