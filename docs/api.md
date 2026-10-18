# API Reference

## `run`

```rust
pub fn run(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>
```

Runs `entry` (a file, a directory, or a `package.json`) to completion and
returns the exit code the script requested (`0` on normal completion).

`run` blocks until the script and its event loop finish. Each call gets a fresh
worker and module graph, so independent runs can execute in parallel. Calling
`run` inside a tokio runtime is supported; libdeno runs it on a worker thread.
Output capture is process-wide and exclusive; see [`run_with_output`].

Directory and `package.json` entries resolve through their `main` field (or
`index.js`). A `main` path that escapes the package directory is rejected.
HTTP operations have a 300-second budget; this is separate from
`execution_deadline`. npm lifecycle scripts are disabled by default.

## `run_with_output`

```rust
pub fn run_with_output(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>

pub struct RunOutput {
  pub exit_code: i32,
  pub stdout: Vec<u8>, // populated only when options.capture_stdout is set
  pub stderr: Vec<u8>, // populated only when options.capture_stderr is set
  pub capture_truncated: bool, // true when a captured stream hit max_capture_bytes
}
```

Captures stdout/stderr selected by `capture_stdout` and `capture_stderr` into
`RunOutput`. Capture redirects process file descriptors, so output from other
host threads may also be captured. A captured run is exclusive; use
`run_in_subprocess_with_output` to capture one child while other runs continue.

If capture ends before a pipe reaches EOF, the run returns a timed-out I/O
error rather than reporting incomplete output as success. `capture_truncated`
indicates that the output byte cap was reached; it is distinct from incomplete
pipe collection.

## `run_source`

```rust
pub enum SourceLang {
  JavaScript, // virtual entry gets a .js extension
  TypeScript, // virtual entry gets a .ts extension and is transpiled
}

pub fn run_source(
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>

pub fn run_source_with_output(
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>

pub async fn run_source_async(
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>

pub async fn run_source_with_output_async(
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>
```

Runs JS/TS bytes directly without a temporary file. `base_dir` must be an
existing directory; relative paths resolve against `options.cwd` when set,
otherwise the process cwd. Relative imports and config discovery start at
`base_dir`.

The virtual entry gets a unique `$deno$__libdeno_virtual_` file URL under
`base_dir`. `import.meta.url` and `Deno.mainModule` expose that URL. The entry
uses the same read-permission check as an imported file, so a read grant on
`base_dir` covers it and sibling imports. Subprocess and `Executor` APIs accept
file paths only. Other options follow the corresponding path-based API.

## `run_async` / `run_with_output_async`

```rust
pub async fn run_async(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>

pub async fn run_with_output_async(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>
```

Runs `entry` on the caller's tokio runtime instead of spawning a worker thread.

The future is `!Send` and must not be interleaved with another async run on the
same thread; a V8 isolate stays on its creation thread. Await runs one at a
time. On a multi-thread runtime, use a `LocalSet`:

- Call from a tokio runtime; outside one, use `run`.
- Enable tokio's time driver when using `execution_deadline`.
- For parallel runs, use `run` or `run_in_subprocess`.

On a multi-thread runtime, pin the `!Send` future to one thread with a
`LocalSet`:

```rust
use libdeno::{run_with_output_async, LibdenoOptions};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), libdeno::LibdenoError> {
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    };
    let local = tokio::task::LocalSet::new();
    let out = local
        .run_until(run_with_output_async("app.js", &options))
        .await?;
    println!("exit={} stdout={:?}", out.exit_code, out.stdout);
    Ok(())
}
```

On a current-thread runtime, await the future directly.

## `LibdenoRuntime` / reusable sync and async runs

```rust
pub struct LibdenoRuntime { /* Clone + Send + Sync; its only state is an Arc<Mutex<...>> */ }

impl LibdenoRuntime {
  pub async fn new(cwd: impl AsRef<Path>) -> Result<Self, LibdenoError>;
  pub fn refresh(&self);

  pub async fn run_async(
    &self,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
  ) -> Result<i32, LibdenoError>

  pub async fn run_with_output_async(
    &self,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
  ) -> Result<RunOutput, LibdenoError>

  pub async fn run_with_source_async(
    &self,
    code: impl AsRef<[u8]>,
    lang: SourceLang,
    base_dir: impl AsRef<Path>,
    options: &LibdenoOptions,
  ) -> Result<i32, LibdenoError>

  pub async fn run_with_output_source_async(
    &self,
    code: impl AsRef<[u8]>,
    lang: SourceLang,
    base_dir: impl AsRef<Path>,
    options: &LibdenoOptions,
  ) -> Result<RunOutput, LibdenoError>
}

pub fn run_with(
  runtime: &LibdenoRuntime,
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>

pub fn run_with_output(
  runtime: &LibdenoRuntime,
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>

pub fn run_with_source(
  runtime: &LibdenoRuntime,
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>

pub fn run_with_output_source(
  runtime: &LibdenoRuntime,
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>
```

Use `LibdenoRuntime::new(cwd)` for repeated runs in one project. It builds the
resolver stack once and `run_with` / `run_with_output` reuse it:

- `new` is async. The stack refreshes when tracked config, lockfile, registry,
  or managed root `node_modules` identity changes. Call `refresh()` after
  changes outside the discovered paths, such as nested `node_modules`.
- Each run gets fresh permissions, a module graph, and a V8 isolate. `cwd` is a
  resolution base and, when set, must match the runtime's directory.
- `run_with` returns only an exit code and rejects capture flags. Use
  `run_with_output` to capture output; capture remains process-wide and
  exclusive.
- `run_async` methods use the caller's tokio runtime and return `!Send`
  futures. Do not interleave runs on one thread; use `LocalSet` on a
  multi-thread runtime. Each call gets fresh permissions, graph, and isolate.
- `run_async` returns only the exit code. Use `run_with_output_async` to get
  captured bytes; capture is exclusive and unsupported on Windows.
- The `LibdenoRuntime` handle is `Clone + Send + Sync` and can be shared across
  threads. Ordinary runs can execute in parallel.

## `LibdenoOptions`

```rust
#[derive(Debug, Clone, Default)]
pub struct LibdenoOptions {
  pub permissions: Vec<String>,
  pub allow_all_permissions: bool,
  pub prompt: bool,
  pub args: Vec<String>,
  pub cwd: Option<PathBuf>,
  pub max_heap_bytes: Option<usize>,
  pub execution_deadline: Option<Duration>,
  pub capture_stdout: bool,
  pub capture_stderr: bool,
  pub max_capture_bytes: Option<usize>,
  pub features: Option<Vec<String>>,
  pub extensions: Option<ExtensionFactory>,
}
```

### `permissions`

Permission capability strings use CLI `--allow-*` syntax. Any permission flag
restricts access to the listed capabilities. A flag without a value grants
that capability globally; comma-separated values scope it, for example
`--allow-read=./src,./public`. An empty list requires `allow_all_permissions`
or `prompt`.

Supported flags:

| Flag | Value (comma-separated) |
|---|---|
| `--allow-read` | paths |
| `--allow-write` | paths |
| `--allow-env` | environment variable names |
| `--allow-net` | hosts (optionally with `:port`) |
| `--allow-import` | remote module hosts |
| `--allow-run` | executable names |
| `--allow-ffi` | `.so`/`.dylib` paths |
| `--allow-sys` | system API names |
| `-A` / `--allow-all` | — (equivalent to `allow_all_permissions`) |

Remote `https:`/`jsr:` imports require `--allow-import`; `--allow-net` does not
grant module-import permission.

Static and dynamic file imports are checked against `--allow-read`.

### `allow_all_permissions`

Grants every capability (`-A` equivalent). Use it only for trusted code.

### `prompt`

Prompts on stdin for checks not granted by flags. Prompting requires terminal
stdin; without a terminal, checks are denied.

In subprocess mode the child's stdin carries the run request, so interactive
prompting is unavailable and non-granted checks are denied.

### `args`

Arguments exposed to the script via `process.argv` (after argv[0]).

### `max_heap_bytes`

Best-effort limit for the V8 old-generation heap. Values below 8 MiB and
`usize::MAX` are rejected. It does not limit native memory, RSS, CPU, or child
processes.

### `execution_deadline`

Best-effort in-process deadline. V8 is terminated at an interruptible stack
check; a blocking system call, native code, child wait, or permission callback
can make the run exceed it. The HTTP timeout is separate.

### `cwd`

Resolution base for relative entry paths, permission paths, and `node_modules`
discovery. Defaults to the process cwd. In-process runs do not change the host
cwd; use absolute paths or a subprocess API when the script needs another cwd.

### `capture_stdout` / `capture_stderr`

Capture the script's stdout and/or stderr in `RunOutput`; output passes through
by default. See [`run_with_output`] for capture exclusivity and subprocess
capture.

### `max_capture_bytes`

Per-stream byte limit. Excess bytes are drained and dropped, and
`RunOutput::capture_truncated` is set. `None` is unbounded for legacy and
in-process capture. The execution-control supervisor backend defaults to 64 KiB
per stream and accepts explicit limits up to 96 KiB; these limits do not apply
to `run_in_subprocess_with_output`, which is unbounded unless
`max_capture_bytes` is set. The supervisor sends no output in `TERMINAL` frames:
the parent captures from pipes. In-process capture is unsupported on Windows;
use `run_in_subprocess_with_output`.

### `features`

Runtime feature flags exposed to the script, overriding the default set (`kv`,
`cron`, `ffi`, `webgpu`, `worker-options`). Feature names must be valid deno
unstable-feature names (see deno's `--unstable-*` flags); they gate which JS
namespace IDs and feature checks are wired into the runtime. `None` (default)
enables the default set. An embedder running untrusted plugins can shrink the
surface (e.g. `Some(vec!["ffi".into()])`); the ops themselves stay
permission-gated regardless.

### `extensions`

An optional `ExtensionFactory` that creates fresh native `deno_core::Extension`
values for the main runtime and each web worker, including nested workers. The
factory runs on the runtime thread; each runtime has its own `OpState`, though
the factory may deliberately capture shared host state. Native ops are not
automatically permission-gated. Extension factories cannot cross subprocess or
supervisor boundaries; runs with a factory are rejected before spawning. See
the [native extension guide](native-extensions.md) for initialization examples.

## Resource and supervision boundaries

- Remote module bodies and npm metadata are capped at 256 MiB; `.tgz` bodies at
  1 GiB. These caps apply to reqwest-returned, transport-decoded bytes, and the
  decoder may allocate before libdeno checks them. The `.tgz` gzip `ISIZE`
  check is only a size hint, not a decompression-output limit.
- HTTP operations have a 300-second budget. Remote redirects are handled by
  the file fetcher; the HTTP client follows redirects only for npm downloads.
- npm lifecycle scripts are disabled.
- Subprocess runs supervise the direct child, not its entire process tree.
  Heap and execution limits are not OS-level RSS or CPU limits. Windows
  overlapped-write cancellation may finish after the requested deadline to
  preserve buffer lifetime.

## `LibdenoError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum LibdenoError {
  Entry(AnyError),                       // entry module resolution failed
  Permission(String),                    // invalid permission flags
  Configuration(String),                 // invalid options
  Runtime(AnyError),                     // runtime startup or script failure
  Core(deno_core::error::CoreError),     // deno_core execution failure
  Io(std::io::Error),                    // host I/O failure
  Timeout(String),                       // deadline or subprocess handshake timeout
}
```

`is_permission_error()` is true only when a typed Rust permission-denial source
is preserved. JavaScript names, classes, causes, and messages do not establish
denial provenance. A JS failure without typed evidence may be `Runtime` or
`Core`; both map to `DenoRuntimeError` in Python.

## `run_in_subprocess`

```rust
pub fn run_in_subprocess(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>
```

Runs `entry` in a child process and returns its exit code. The child contains
`Deno.exit` and many process-level failures, but native crashes can still affect
the host; use an OS sandbox for untrusted code.

Call `maybe_handle_child_mode()` during startup, after any required trusted
initialization (such as Node IPC setup or installing a broker/hook) and before
normal host logic. The child inherits stdout/stderr and uses `options.cwd`.
Entry, permissions, args, and cwd are sent as JSON over stdin. The request is
capped at 1 MiB and its handshake at 10 seconds. Only the direct child is
reaped; descendants are not fully supervised. Use `run_in_subprocess_with_output`
to capture child output.

`LIBDENO_HOST_EXE` overrides the executable spawned (defaults to
`current_exe()`); integration tests use this to point at a dedicated host.

## `run_in_subprocess_with_output`

```rust
pub fn run_in_subprocess_with_output(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>
```

Runs `entry` in a child process and captures both stdout and stderr through
per-child pipes. This capture works in parallel with other runs, including on
Windows. In-process capture is exclusive and unsupported on Windows.

- Both streams are returned; `capture_stdout` / `capture_stderr` are implied.
- `max_capture_bytes` limits retained bytes per stream. Excess bytes are
  drained and dropped; `capture_truncated` indicates truncation.
- This helper is unbounded unless `max_capture_bytes` is set, including when
  execution control is enabled. The 64 KiB default and 96 KiB maximum apply only
  to the execution-control supervisor backend.

## `maybe_handle_child_mode`

```rust
pub fn maybe_handle_child_mode() -> bool
```

Services a child-run request when the process was spawned by
`run_in_subprocess`.

- Normal host launch: returns `false` immediately.
- Child mode: executes the requested script and exits the process with the
  script's exit code (including `Deno.exit(n)`); it does not return.

Call it after required trusted startup initialization and before normal host
logic:

```rust
fn main() {
  libdeno::maybe_handle_child_mode();
  // ... normal host logic ...
}
```

## `initialize_node_ipc_from_env`

```rust
pub unsafe fn initialize_node_ipc_from_env() -> Result<(), LibdenoError>
```

For a Node `child_process.fork` host, explicitly adopts the connected endpoint
identified by `NODE_CHANNEL_FD`. Call it during trusted startup, before creating
threads, then call `maybe_handle_child_mode()` before normal host logic. The
caller transfers unique ownership of the raw descriptor/handle to libdeno.

Initialization is one-shot, including failed attempts. A claimed endpoint is
closed if later validation fails, so it cannot be retried. If the variable is
absent, initialization records that no IPC channel is available. Ordinary
`run()` neither reads nor claims a live environment channel. Do not let
untrusted input select a raw OS handle.

## `install_permission_broker`

```rust
pub fn install_permission_broker(path: impl AsRef<Path>) -> Result<(), LibdenoError>
```

Installs an external permission broker at `path` (Unix socket or Windows named
pipe). It is process-global and install-once; after installation it decides
every permission check, replacing local flag grants. Checks block until the
broker responds.

The upstream constructor may call `process::exit(87)` if the initial connection
fails; later bridge failures may also terminate the process. These exits cannot
be returned as `LibdenoError`, so use a trusted, reliable broker. To pass the
broker to subprocess runs, install it during trusted startup before
`maybe_handle_child_mode()` dispatches child mode.

## `install_permission_hook`

```rust
pub type PermissionPrompt = Arc<dyn Fn(&PermissionRequest) -> bool + Send + Sync>;

pub fn install_permission_hook(hook: PermissionPrompt) -> Result<(), LibdenoError>
```

Installs an in-process hook on Unix; on Windows, use
`install_permission_broker`. Return `true` to allow a request and `false` to
deny it. Like the broker, the hook is process-global, install-once, and the sole
authority for permission checks once installed. It cannot be combined with the
broker.

The hook runs synchronously on an internal thread; a slow callback stalls
permission checks and can delay `execution_deadline`. An unwinding panic is
denied, but a `panic = "abort"` build cannot recover. The shared upstream
broker bridge can still terminate the host on connection failure (`exit(87)`);
that termination is not returned through `Result`. With no permission flags,
set `prompt: true` so checks reach the hook.

Do not fork and continue using an installed broker or hook. Exec immediately,
then install it during trusted startup in the new process; reinstalling in the
fork child is not supported.

## Permission decision priority

broker/hook (if installed, the sole authority) → flag grants → interactive
prompt when `prompt: true` → deny.

## Example host binary

`examples/demo.rs` shows a host that calls `maybe_handle_child_mode()` at
startup and supports embedded and subprocess execution. Usage:

```text
demo [permission flags...] <entry> [script args...]
```

For Node `child_process.fork` startup and inherited npm-state limits, see
[npm support](npm-support.md#child_processfork).
