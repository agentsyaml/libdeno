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

## `LibdenoOptions`

```rust
#[derive(Debug, Clone, Default)]
pub struct LibdenoOptions {
  pub permissions: Vec<String>,
  pub args: Vec<String>,
  pub cwd: Option<PathBuf>,
}
```

### `permissions`

Permission capability strings in CLI `--allow-*` format.

- An empty list grants everything (the default).
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
| `-A` / `--allow-all` | — (allow everything, the default stance) |

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
  Permission(String),                    // invalid permission flag
  Runtime(AnyError),                     // runtime startup / script failure
  Core(deno_core::error::CoreError),     // JS exception escaped event loop
  Js(Box<deno_core::error::JsError>),    // JS exception in lifecycle dispatch
  Io(std::io::Error),                    // host I/O failure (e.g. cwd)
}
```

All variants implement `std::error::Error`; `Runtime`, `Core`, `Js`, and `Io`
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
