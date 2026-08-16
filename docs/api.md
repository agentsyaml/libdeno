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

Behavior notes:

- Each call builds its own current-thread tokio runtime and worker. Ordinary
  runs execute **in parallel** — each has its own thread, isolate, and graph,
  sharing nothing mutable. The one exception: output capture (see below) is
  exclusive, so a captured run rejects any overlapping run.
- The call blocks until the script finishes (module execution + event loop +
  lifecycle events: `load`, `beforeunload`, `unload`, `process.beforeExit`,
  `process.exit`).
- Since v0.2.0 a directory/`package.json` entry whose `main` field escapes
  the package directory (absolute path, or `..` walking above it) is rejected
  with `LibdenoError::Entry`; `..` that stays inside (e.g. `src/../lib/index.js`)
  is still accepted.
- Since v0.2.0 every remote module fetch is capped at 256 MiB regardless of
  the declared `Content-Length`; the 1 GiB tier applies only to npm registry
  (tarball) downloads.
- Safe to call from inside a tokio runtime: the run executes on a fresh
  thread (tokio forbids nested runtimes) and joins back. Same for
  `run_with_output` and `run_with`.

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

Like `run`, but captures the script's stdout/stderr into `RunOutput` when
`LibdenoOptions.capture_stdout` / `capture_stderr` are set. The capture is
fd-level (deno_core's print op has no injectable writer), so `console.log`,
`console.error`, `Deno.stderr.write` and direct fd writes all land in the
buffer. Caveat: while captured, *other host threads* printing to stdout/stderr
during the run are captured too. Capture is fd-level process-global
redirection, so a captured run is **exclusive**: any overlapping run (captured
or not) is rejected with `LibdenoError::Configuration`. For captured runs
alongside parallel execution use `run_in_subprocess`, where each process has
its own fds.

## `LibdenoRuntime` / `run_with` / `run_with_output`

```rust
pub struct LibdenoRuntime { /* Clone + Send + Sync; its only state is an Arc<Mutex<...>> */ }

impl LibdenoRuntime {
  pub async fn new(cwd: impl AsRef<Path>) -> Result<Self, LibdenoError>;
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
```

`run` rebuilds the resolver stack (workspace / resolver / npm-installer
factories, graph resolver, npm process state) on every call. For long-lived
hosts running many scripts in the same project, `LibdenoRuntime::new(cwd)`
builds the permission-free half of that stack once and `run_with` /
`run_with_output` reuse it across runs:

- `LibdenoRuntime::new` is **async** — stack construction needs a tokio
  context. The stack is rebuilt automatically when the config chain changes
  (deno.json / deno.jsonc / import_map.json / package.json / .npmrc /
  node_modules at the project root and its ancestors): `run_with` recomputes
  a fingerprint and swaps the stack when it diverges.
- `run_with` semantics match `run` — ordinary runs are fully parallel, tokio
  re-entry handled automatically (fresh thread inside a tokio runtime),
  `Deno.exit(n)` / exit codes / deadlines identical. The script runs in the
  host's cwd; `LibdenoOptions.cwd` is **ignored** as a resolution base (the
  stack is scoped to the runtime's directory), and the process cwd is never
  switched. Permissions come from `options` per run: the permission-bound
  file fetcher / graph loader / graph are rebuilt each call, so one run's
  grants never leak into another.
- `run_with` does **not** honor `capture_stdout` / `capture_stderr` (it
  returns only the exit code). Use `run_with_output(&runtime, ...)` for
  capture on the reusable stack; everything else matches `run_with_output`
  (`LibdenoOptions.cwd` ignored, permissions per run, fd-level capture with
  the same exclusivity lease, Windows rejection and per-stream byte cap).
- `LibdenoRuntime` is `Clone` + `Send` + `Sync`, so it can be shared across
  host threads; ordinary runs through it are fully parallel (only a captured
  run is exclusive — see `run_with_output`). It is single-threaded by design:
  the module loader stack is `Rc<...>`-based and every run executes on a
  fresh current-thread tokio runtime. The async/sync split is deliberate —
  `new` is async, `run_with` is sync and does its own `block_on` on that
  fresh runtime.

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
}
```

### `permissions`

Permission capability strings in CLI `--allow-*` format.

- Since v0.2.0 an **empty list is a construction error** — it grants nothing
  and `run` returns `LibdenoError::Configuration` (the message spells out the
  v0.2.0 semantic change and the migration options). To run with every
  capability, either set `allow_all_permissions: true` or pass `-A`/`--allow-all`.
- Passing any entry restricts the runtime to the declared capabilities.
- A flag without a value allows that capability globally
  (`--allow-read` == read anywhere).
- A flag with a comma-separated value allows only the listed descriptors
  (`--allow-read=./src,./public`).

Supported flags:

| Flag | Value (comma-separated) |
|---|---|
| `--allow-read` | paths |
| `--allow-write` | paths |
| `--allow-env` | environment variable names |
| `--allow-net` | hosts (optionally with `:port`) |
| `--allow-import` | import hosts (e.g. `deno.land`); gates remote `https:`/`jsr:` module loading — there is no `--allow-net` fallback |
| `--allow-run` | executable names |
| `--allow-ffi` | `.so`/`.dylib` paths |
| `--allow-sys` | system API names |
| `-A` / `--allow-all` | — (equivalent to `allow_all_permissions`) |

Static and dynamic file imports are gated by `--allow-read` (the graph loader
checks each file against the declared scope).

### `allow_all_permissions`

Grants every capability (`-A` equivalent). Required to run scripts with an
empty `permissions` list — since v0.2.0 that is a construction error unless
this flag is set. Use it only for code you trust (see SECURITY.md).

### `prompt`

Interactive permission prompting for non-granted queries, mirroring `deno run`'s
default behavior: a check prints to stderr and reads allow/deny from stdin,
blocking the run while it waits. The upstream prompter requires a terminal
stdin (`is_terminal()`); a headless host without one sees every such query
denied without reading.

The three combinations:

| `permissions` | `prompt` | Behavior |
|---|---|---|
| empty | `false` | construction error (the v0.2.0 default) |
| empty | `true` | every access is asked interactively |
| flags | `true` | flags grant, everything else is asked |
| flags | `false` | flags grant, everything else is denied |

In subprocess mode (`run_in_subprocess`) the child's stdin is a pipe (consumed
by the request JSON), so the prompter's terminal check denies without reading —
`prompt: true` in a child is equivalent to fail-closed deny; real interaction
only makes sense for in-process `run`.

### `args`

Arguments exposed to the script via `process.argv` (after argv[0]).

### `cwd`

Resolution base that relative paths (entry, permissions, `node_modules`
discovery) resolve against. Defaults to the process current directory.

The process cwd is **never switched** (chdir is process-global and would
serialize or corrupt concurrent runs, which are otherwise fully parallel). The
script observes the host's cwd: `Deno.cwd()` and relative filesystem
operations inside the script resolve against it. Scripts that need a specific
working directory should use absolute paths or `run_in_subprocess` (the
child's cwd is pinned at spawn).

### `capture_stdout` / `capture_stderr`

Redirect the script's stdout (fd 1, e.g. `console.log`) / stderr (fd 2, e.g.
`console.error`) into `RunOutput::stdout` / `RunOutput::stderr` instead of the
host's terminal. Off by default (output passes through). While active the
redirection is fd-level and process-global: other host threads printing to
stdout/stderr during the run are captured too, and the run is **exclusive** —
any concurrent run (captured or not) is rejected with
`LibdenoError::Configuration`. For captured runs alongside parallel execution
use `run_in_subprocess`, where each process has its own fds.

### `max_capture_bytes`

Per-stream cap on captured output (stdout and stderr each get this budget).
When a stream exceeds it, capture stops, the excess is dropped, and
`RunOutput::capture_truncated` is set — a verbose or hostile script can no
longer grow host memory without limit. `None` (default) captures without a
bound.

### `features`

Runtime feature flags exposed to the script, overriding the default set (`kv`,
`cron`, `ffi`, `webgpu`, `worker-options`). Feature names must be valid deno
unstable-feature names (see deno's `--unstable-*` flags); they gate which JS
namespace IDs and feature checks are wired into the runtime. `None` (default)
enables the default set. An embedder running untrusted plugins can shrink the
surface (e.g. `Some(vec!["ffi".into()])`); the ops themselves stay
permission-gated regardless.

## `LibdenoError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum LibdenoError {
  Entry(AnyError),                       // entry module resolution failed
  Permission(String),                    // invalid permission flag strings
  Configuration(String),                 // options cannot form a valid configuration (e.g. empty permission list without opt-in since v0.2.0)
  Runtime(AnyError),                     // runtime startup / script failure
  Core(deno_core::error::CoreError),     // JS exception escaped event loop
  Io(std::io::Error),                    // host I/O failure (e.g. cwd, output capture setup)
  Timeout(String),                     // deadline exceeded / subprocess handshake timed out (message explains which)
}
```

All variants implement `std::error::Error`; `Runtime`, `Core`, and `Io`
use `#[from]` so `?` works naturally.

## `run_in_subprocess`

```rust
pub fn run_in_subprocess(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>
```

Runs `entry` in a **child process** and returns the child's exit code.

Why: the embedded `run()` shares the host process, so a script calling
`Deno.exit(n)` — or a hard runtime failure — terminates the host process
itself (`deno_os::exit` → `std::process::exit`). Subprocess mode contains
that: `Deno.exit(n)` kills only the child, and the host keeps running and
observes `n` as the return value.

Requirements:

- The host binary must call `maybe_handle_child_mode()` at the very start of
  its `main()` so child requests are serviced.
- The child inherits stdout/stderr, so script output still appears.
- The child's cwd is pinned to `options.cwd` at spawn (via
  `Command::current_dir`), so the script sees it as its working directory —
  unlike in-process runs, where the script observes the host's cwd.
- Entry, permissions, args, and cwd are passed over the child's stdin as
  JSON; relative entry paths resolve against `options.cwd`.
- `features`, `max_heap_bytes`, and `execution_deadline` are forwarded to
  the child verbatim: a host that bounds or shrinks an untrusted script gets
  the same bounds in child mode (the child never silently runs unbounded on
  the full unstable surface). The capture flags are not forwarded — the
  child writes to the inherited fds, so capture the parent side instead
  (redirect the host's own fds around the call).

`LIBDENO_HOST_EXE` overrides the executable spawned (defaults to
`current_exe()`); integration tests use this to point at a dedicated host.

## `maybe_handle_child_mode`

```rust
pub fn maybe_handle_child_mode() -> bool
```

Services a child-run request when the process was spawned by
`run_in_subprocess`.

- Normal host launch: returns `false` immediately.
- Child mode: executes the requested script and exits the process with the
  script's exit code (including `Deno.exit(n)`); it does not return.

Call it as the first line of `main()`:

```rust
fn main() {
  libdeno::maybe_handle_child_mode();
  // ... normal host logic ...
}
```

## `install_permission_broker`

```rust
pub fn install_permission_broker(path: impl AsRef<Path>) -> Result<(), LibdenoError>
```

Installs an external permission broker process at `path` (raw deno_permissions
capability): a Unix socket (or Windows named pipe) serving the JSON-line
protocol — a request `{v, pid, id, datetime, permission, value}` line, a
response `{id, result: "allow"|"deny", reason}` line.

- Process-global and install-once; a second install returns
  `LibdenoError::Permission`. Once installed, the broker is the **sole
  authority** for every permission check in the process — granted or not — so
  local `--allow-*` flags are no longer consulted (upstream deno semantics).
- Checks are synchronous and blocking: the run stalls until the broker answers
  each query.
- `PermissionBroker::new` exits the process (code 87) if it cannot connect to
  `path` — upstream deno_permissions behavior, not libdeno's. Install at
  startup so a bad socket fails loudly.
- Works across `run_in_subprocess` children when the host binary installs it in
  `main()` before `maybe_handle_child_mode()`.

## `install_permission_hook`

```rust
pub type PermissionPrompt = Arc<dyn Fn(&PermissionRequest) -> bool + Send + Sync>;

pub fn install_permission_hook(hook: PermissionPrompt) -> Result<(), LibdenoError>
```

Installs an in-process permission hook (Unix only; on Windows use
`install_permission_broker` with an external broker process). Return `true` to
allow, `false` to deny.

- Same semantics as `install_permission_broker`: process-global, install-once,
  the sole authority for every permission check once installed, and mutually
  exclusive with `install_permission_broker`.
- The hook is served on an internal thread through a temp-dir Unix socket
  (private 0700 dir, unlinked once the connection is established). The
  `PermissionRequest` carries the capability name (`"read"`, `"net"`, ...) and
  the stringified access value (path, host, env name, ...; `None` for unary
  checks).
- The hook must return quickly and must not block: checks are synchronous and
  blocking, so a stalled hook stalls every permission check in the process. A
  panicking hook terminates the process (upstream broker error path).
- The hook decides checks, not construction: an empty `permissions` list still
  fails at `run` construction time unless `prompt: true` (or flags /
  `allow_all_permissions`) is set — the minimal hook configuration is hook +
  `prompt: true` (the all-Prompt container routes every check to the hook).

## Permission decision priority

broker/hook (if installed, the sole authority) → flag grants → interactive
prompt when `prompt: true` → deny.

## Example host binary

`examples/demo.rs` is the reference host. It calls
`maybe_handle_child_mode()` on startup and supports both embedded and
subprocess execution. Usage:

```text
demo [permission flags...] <entry> [script args...]
```

It also understands the translated argument style used by
`child_process.fork` (`demo run -A --unstable-... script.js`): the `run`
subcommand and flags are skipped when picking the entry.
