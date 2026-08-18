# libdeno Python API

This is a private alpha Python binding prototype for the Rust `libdeno`
runtime. The Python distribution is versioned independently: this package is
currently `0.1.0`, while the core Rust crate is `0.3.0`. It exposes a small,
fail-closed synchronous API backed by the Rust runtime:

```python
import libdeno

exit_code = libdeno.run(
    "entry.js",
    permissions=("--allow-read=.",),
    args=("app-arg",),
)

runtime = libdeno.Runtime(".")
exit_code = runtime.run("entry.js", allow_all_permissions=True)
```

`entry` and `Runtime`'s `cwd` accept strings and `os.PathLike` values. All
options after `entry` are keyword-only. Empty `permissions` is rejected unless
`allow_all_permissions=True` is explicit. `execution_deadline` is a finite,
non-negative number of seconds. The native call releases the Python GIL while
the Rust runtime executes, and a `Runtime` reuses its resolver stack while
keeping permissions and isolates per run.

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

## Exceptions

The facade exports `LibdenoError` and the specific `EntryError`,
`DenoPermissionError`, `ConfigurationError`, `DenoRuntimeError`, `DenoIOError`,
and `DenoTimeoutError` subclasses. Runtime permission failures are recognized
from Deno's typed `NotCapable` error class; other runtime failures remain
`DenoRuntimeError`.

## Not supported yet

- Prompting and permission hooks.
- Output capture, async APIs, and subprocess execution.
- Python-facing permission/options objects beyond the keyword arguments above.
- Native addons.
- Source distributions (`sdist`); this prototype only defines a wheel build
  path.
- A stable public contract; this is a private alpha API and may change
  without notice.
