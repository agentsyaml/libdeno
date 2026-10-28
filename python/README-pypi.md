# libdeno Python API

The Python distribution and Rust crate are versioned at `0.3.8`. The Python
API is alpha and provides synchronous access to the runtime:

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

`run()` and `Runtime.run()` block until the script and its event loop finish,
return an exit code, and release the GIL while Rust runs. `Runtime` reuses the
resolver stack; permissions, module graphs, and isolates remain per run.

`entry` and `cwd` accept `str`, `bytes`, and `os.PathLike` values, including
non-ASCII paths. `cwd=None` (or an omitted `cwd`) means the process current
directory. For `Runtime`, that directory is fixed when the object is created;
the process cwd is never changed. Options after `entry` are keyword-only.

`run_with_output()` captures the script's stdout/stderr into a `RunOutput`
(`exit_code`, `stdout`, `stderr`, `capture_truncated`). Both streams are
captured by default; disable one with `capture_stdout=False` /
`capture_stderr=False`, and bound each with `max_capture_bytes`. Capture
redirects process-global file descriptors, so a captured run is exclusive and
a concurrent run raises `ConfigurationError`; it is not supported on Windows.

`run_source()` and `run_source_with_output()` run code held in memory. `code`
is `str` (encoded as UTF-8), `bytes`, or `bytearray`; `lang` is
`SourceLang.JavaScript`, `SourceLang.TypeScript`, or the strings
`"javascript"`/`"typescript"`; `base_dir` is the existing directory the virtual
entry lives in, so relative imports resolve against it.

```python
output = libdeno.run_source_with_output(
    "const answer: number = 42;\nconsole.log(answer);",
    lang=libdeno.SourceLang.TypeScript,
    allow_all_permissions=True,
)
print(output.stdout)
```

The Python binding does **not** expose async APIs, subprocess helpers,
permission hooks, or native addons. Script-level Deno APIs still follow the
Rust runtime's permission model.

Async callers should wrap the blocking entry points:
`await asyncio.to_thread(libdeno.run, "entry.js", allow_all_permissions=True)`.
The GIL is already released for the whole run, so this only moves the call off
the event loop thread.

## Options and limits

- `permissions` contains CLI-style `--allow-*` strings in a list or tuple; any
  other type (including a mapping) raises `ConfigurationError`. An empty
  sequence requires `allow_all_permissions=True`; prompting is not exposed.
  `-A`/`--allow-all` grants all capabilities.
- `args` and `features` are lists or tuples too, exposed through
  `process.argv` and the Deno unstable feature set respectively. `features`
  defaults to `None`, which keeps the runtime defaults.
- `capture_stdout`, `capture_stderr` (both `True` by default) and
  `max_capture_bytes` apply to the `*_with_output` entry points. Truncation
  sets `RunOutput.capture_truncated`.
- `lang` and `base_dir` apply to the in-memory entry points; `base_dir` must be
  an existing directory, otherwise `ConfigurationError`.
- `execution_deadline` is a finite, non-negative number of seconds. A busy
  JavaScript run normally raises `DenoTimeoutError` when the deadline fires,
  but this is best effort: blocking system calls, native code, and blocked
  permission paths can make a run exceed it.
- `max_heap_bytes` is an optional V8 old-generation constraint. It must be an
  integer (`bool` is rejected). Values below 8 MiB are rejected with
  `ConfigurationError`; the value does not cap native allocations, external
  memory, host memory, RSS, CPU, or child processes.
  It is not an OS/process isolation boundary.
- HTTP body limits apply to reqwest-returned, transport-decoded response-body
  bytes, not wire bytes; upstream decoding may allocate before the limit is
  checked. The explicit npm `.tgz` path has the same 1 GiB response-body cap.
  For a gzip response, the final four body bytes are checked as an `ISIZE` hint
  (the final member's size modulo 2^32); this threshold is not a decompressed-
  output budget and does not identify the first consumed member or total
  extraction. Fallback extraction has no hard output budget, so this is not a
  decompression or RSS cap.

## Exceptions

The facade exports `LibdenoError` and these subclasses:

- `EntryError` — the entry path does not exist or entry resolution fails, such
  as when `package.json` is malformed or `main` escapes its directory. If
  `main` names a missing file, module loading fails with `DenoRuntimeError`.
- `DenoPermissionError` — invalid permission flags or denials carrying typed
  Rust permission provenance. JavaScript exception names/classes/causes are
  not trusted as denial evidence; a JS failure, including a denial whose typed
  provenance was lost, is reported as `DenoRuntimeError`.
- `ConfigurationError` — options cannot form a valid run, including empty
  permissions without an explicit opt-in, invalid deadlines, or an undersized
  heap value.
- `DenoRuntimeError` — runtime startup or JavaScript failure.
- `DenoIOError` — host filesystem/I/O failure.
- `DenoTimeoutError` — the runtime reported an execution deadline timeout.

## Install from the next source release

The next libdeno release will include an sdist. After that release is
published, install it with:

```bash
python -m pip install --no-binary=libdeno "libdeno==<next-release-version>"
```

Building requires Rust 1.95 or newer, a C/C++ toolchain, and libclang (for
example, `libclang-dev` on Debian/Ubuntu). Cargo and the V8 build download
platform-specific dependencies; an sdist does not provide cross-compilation.
The tested release wheel targets are Linux x86_64/aarch64, macOS x86_64/arm64,
and Windows x86_64.

## Build and install from a local source archive

Use a virtual environment rather than installing build tools globally:

```bash
cd python
python -m venv .venv
. .venv/bin/activate
python -m pip install "maturin==1.13.3"
maturin sdist --out target/sdist
python -m pip install target/sdist/*.tar.gz
cd ..
python -m unittest discover -s python/tests -p 'test_*.py'
```

The sdist contains the Rust crates and build support needed to compile outside
the checkout. For a local prebuilt wheel instead, run `./build-wheel.sh` from
`python/` and install a wheel from `target/wheels/`. Wheels use the Python 3.9
stable ABI and target Python 3.9+. The wheel build script clears
`RUSTC_WRAPPER`, resolves the active compiler dynamically, and uses Cargo's
locked dependency resolution; it requires Bash.

To build and install that wheel in a virtual environment:

```bash
cd python
python -m venv .venv
. .venv/bin/activate
python -m pip install "maturin==1.11.5"
./build-wheel.sh
python -m pip install target/wheels/*.whl
cd ..
python -m unittest discover -s python/tests -p 'test_*.py'
```

Run the stdlib test suite after installing the wheel from the repository root,
so the source package does not shadow the installed package. Keep the same
virtual environment activated so `python` refers to its interpreter.

## Limitations

- Prompting and permission hooks.
- Async APIs (use `asyncio.to_thread`) and subprocess execution.
- Python-facing permission/options objects beyond the keyword arguments above.
- Native addons.
- The Python API remains alpha and may change.
