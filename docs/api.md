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

- Each call builds its own current-thread tokio runtime and worker, so
  multiple invocations are fully independent.
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
}
```

### `permissions`

Permission capability strings in CLI `--allow-*` format.

- Since v0.2.0 an **empty list is a construction error** — it grants nothing
  and `run` returns `LibdenoError::Permission`. To run with every capability,
  either set `allow_all_permissions: true` or pass `-A`/`--allow-all`.
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
| `--allow-run` | executable names |
| `--allow-ffi` | `.so`/`.dylib` paths |
| `--allow-sys` | system API names |
| `-A` / `--allow-all` | — (equivalent to `allow_all_permissions`) |

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

Working directory that relative paths (entry, permissions, `node_modules`
discovery) resolve against. Defaults to the process current directory.

## `LibdenoError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum LibdenoError {
  Entry(AnyError),                       // entry module resolution failed
  Permission(String),                    // invalid permission flags / empty list without opt-in
  Runtime(AnyError),                     // runtime startup / script failure
  Core(deno_core::error::CoreError),     // JS exception escaped event loop
  Io(std::io::Error),                    // host I/O failure (e.g. cwd)
  Timeout(Duration),                     // execution deadline exceeded, isolate terminated
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
- Entry, permissions, args, and cwd are passed over the child's stdin as
  JSON; relative entry paths resolve against `options.cwd`.

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
