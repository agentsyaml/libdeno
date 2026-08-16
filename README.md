# libdeno

[![crates.io](https://img.shields.io/crates/v/libdeno.svg)](https://crates.io/crates/libdeno)
[![docs.rs](https://docs.rs/libdeno/badge.svg)](https://docs.rs/libdeno)
[![CI](https://github.com/agentsyaml/libdeno/actions/workflows/ci.yml/badge.svg)](https://github.com/agentsyaml/libdeno/actions/workflows/ci.yml)

> Embed the Deno runtime in Rust with direct `npm:` specifier support.

libdeno is a Rust crate that embeds a full Deno runtime (V8 + the official module graph pipeline) inside your program. Your JS/TS code can `import` npm packages, remote modules, `jsr:` and `node:` builtins directly, all handled by the official Deno resolver stack — the same behavior as `deno run`, but running inside your process. Remote (`https:`/`jsr:`) module loading is permission-gated exactly like the CLI: it needs `--allow-import` (or `allow_all_permissions` / `prompt`).

中文版文档：[README.zh-CN.md](README.zh-CN.md)

---

## Features

- **Official module graph pipeline**: `npm:`, `jsr:`, remote `https://`, `node:`, local files, JSON, WASM, import maps (from `deno.json`), and TypeScript transpilation are all handled by `deno_graph` + `deno_resolver`. Remote (`https:`/`jsr:`) imports require `--allow-import` (or `allow_all_permissions`/`prompt`) — there is no `--allow-net` fallback for module loading, matching `deno run`.
- **npm integration**: automatically discovers and uses an existing `node_modules` (BYONM); installs on demand otherwise (managed mode). Supports CJS packages and `.node` native addons. npm lifecycle scripts do not run by default (matching deno CLI 2.x).
- **`child_process.fork` support**: the npm resolution snapshot propagates to child processes.
- **Web Workers**: `new Worker(...)` nested workers reuse the same module loader and snapshot.
- **Permission model**: CLI-style `--allow-*` capability strings; permissions are opt-in — an empty list is a construction error unless `allow_all_permissions` is set.
- **Unstable APIs enabled out of the box**: `Deno.openKv`, cron, FFI, WebGPU, etc. (an "everything enabled" stance, like `deno run --unstable`).
- **Prebuilt V8 snapshot**: runtime extensions are compiled into a snapshot at build time for faster cold start.

---

## Quick Start

```rust
use libdeno::{LibdenoOptions, run};

let options = LibdenoOptions {
  permissions: vec!["--allow-read=.".into(), "--allow-net=example.com".into()],
  args: vec![],
  cwd: None,
  ..Default::default()
};
let exit_code = run("app.js", &options).unwrap();
```

`run` accepts three kinds of entry:

- A file: `run("app.ts", ...)`
- A directory: `run("./my-app", ...)` (uses its `package.json` `main`, default `index.js`)
- A `package.json` itself: `run("./my-app/package.json", ...)`

### Reuse the resolver stack: `LibdenoRuntime` + `run_with`

`run()` rebuilds the resolver stack (workspace / resolver / npm-installer factories, graph resolver) on every call. For long-lived hosts running many scripts in the same project, build the stack once and reuse it — it is rebuilt automatically when the config chain changes:

```rust
use libdeno::{LibdenoRuntime, LibdenoOptions, run_with};

let runtime = LibdenoRuntime::new("./my-app").await.unwrap();
let options = LibdenoOptions { allow_all_permissions: true, ..Default::default() };
let exit_code = run_with(&runtime, "app.js", &options).unwrap();
```

Scripts resolve relative paths against the runtime's cwd (the process cwd is never switched, and `LibdenoOptions.cwd` is ignored as a resolution base) — the script itself observes the host's cwd. Each `run_with` still rebuilds the permission-bound file fetcher / graph loader / graph from its own `options.permissions`, so one run's grants can never leak into another.

### Run the demo

```bash
# Build (takes a few minutes the first time: V8 snapshot + full dependency tree)
cargo build --example demo

# Run an app that mixes an npm package, a node builtin, a local module, and a JSON import
./target/debug/examples/demo examples/demo-app/index.js
# npm package (chalk) works
# node builtin (node:path): a/b/c
# local module: 1 + 2 = 3
# json import: name=demo-app deps=1

# TypeScript entry
./target/debug/examples/demo examples/demo-app/tschalk_test.ts
# ts entry + chalk: ok

# A directory entry
cd examples/demo-app && ../../target/debug/examples/demo .
```

---

## API

| Item | Description |
|---|---|
| `run(entry, &options) -> Result<i32, LibdenoError>` | Runs the entry to completion and returns the exit code the script requested. Each call builds its own current-thread runtime and worker; ordinary runs execute fully in parallel — own thread, isolate, and graph, sharing nothing mutable (the process-global analysis / npm-snapshot / on-disk caches are safe shared state; the process cwd is never switched). Safe to call from inside a tokio runtime — the run executes on a fresh thread there (see below). |
| `run_with_output(entry, &options) -> Result<RunOutput, LibdenoError>` | Like `run`, but also captures the script's stdout/stderr into `RunOutput` when `capture_stdout` / `capture_stderr` are set. |
| `run_async(entry, &options) -> Result<i32, LibdenoError>` | Async entry point: runs the script on the **caller's** tokio runtime — no spawned thread. Must be awaited inside a tokio context; the future is not `Send` and a second `run_async` on one thread is rejected with `LibdenoError::Configuration` (interleaved runs would abort the process). Await one at a time — use `run` for parallel runs. |
| `LibdenoRuntime::new(cwd)` | Builds the resolver stack for a project directory once (async). Reused by `run_with`; rebuilt automatically when the config chain (deno.json / deno.jsonc / import_map.json / package.json / .npmrc / node_modules) changes. |
| `run_with(&runtime, entry, &options) -> Result<i32, LibdenoError>` | Like `run`, but reuses `runtime`'s resolver stack. Semantics identical to `run` (parallel ordinary runs, tokio re-entry handling, exit codes, deadlines); relative paths resolve against the runtime's cwd and permission-bound components are rebuilt per call. Capture flags and a mismatched `options.cwd` are rejected with `LibdenoError::Configuration`. |
| `run_with_output(&runtime, entry, &options) -> Result<RunOutput, LibdenoError>` | Like `run_with`, but also captures the script's stdout/stderr into `RunOutput` when `capture_stdout` / `capture_stderr` are set — the long-lived-host equivalent of `run_with_output` (which rebuilds the resolver stack every call). Same semantics as `run_with` otherwise. |
| `run_in_subprocess(entry, &options) -> Result<i32, LibdenoError>` | Runs the entry in a child process. `Deno.exit(n)` then terminates only the child; the host stays alive and observes `n`. The host must call `maybe_handle_child_mode()` at the start of `main()`. |
| `run_in_subprocess_with_output(entry, &options) -> Result<RunOutput, LibdenoError>` | Like `run_in_subprocess`, but pipes the child's own stdout/stderr back into `RunOutput`: per-process capture that runs in parallel with other runs and works on Windows. Both streams are always returned; `max_capture_bytes` caps each stream and `RunOutput.capture_truncated` is set when truncated. |
| `maybe_handle_child_mode() -> bool` | Services `run_in_subprocess` child requests. Returns `false` on a normal host launch; in child mode it executes the script and exits with its code. |
| `LibdenoOptions.permissions: Vec<String>` | `--allow-*` capability strings. An empty list is a construction error (`LibdenoError::Configuration`) — since v0.2.0 it grants nothing; pass capability flags, set `allow_all_permissions`, or set `prompt: true`. |
| `LibdenoOptions.allow_all_permissions: bool` | Grants every capability (`-A` equivalent). Required to run scripts with an empty `permissions` list. Use only for code you trust (see SECURITY.md). |
| `LibdenoOptions.capture_stdout` / `capture_stderr: bool` | Redirect the script's stdout/stderr (fd 1/2) into `RunOutput` instead of the host's terminal. While active the redirection is process-global: other host threads printing during the run are captured too, and the run is **exclusive** — any concurrent run (captured or not) is rejected with `LibdenoError::Configuration` (use `run_in_subprocess_with_output` for capture alongside parallel runs). |
| `LibdenoOptions.max_capture_bytes: Option<usize>` | Cap on captured output per stream (stdout and stderr each get this budget); when a stream exceeds it, capture stops, the excess is dropped, and `RunOutput.capture_truncated` is set. `None` (default) captures without a bound. |
| `LibdenoOptions.features: Option<Vec<String>>` | Overrides the default unstable feature set (`kv`, `cron`, `ffi`, `webgpu`, `worker-options`). Feature names must be valid deno unstable-feature names; `None` (default) enables the default set. An embedder running untrusted plugins can shrink the surface; the ops themselves stay permission-gated regardless. |
| `LibdenoOptions.args: Vec<String>` | Arguments exposed to the script via `process.argv` (after argv[0]). |
| `LibdenoOptions.cwd: Option<PathBuf>` | Resolution base that relative paths (entry, permissions, `node_modules` discovery) resolve against. Defaults to the process current directory. The process cwd is never switched — scripts observe the host's cwd (`Deno.cwd()`), so use `run_in_subprocess` for a per-run working directory. |
| `LibdenoOptions.max_heap_bytes: Option<usize>` | Hard cap on the V8 old-generation heap in bytes; V8 aborts with OOM when hit. Applies to the main worker **and** web workers spawned via `new Worker(...)`. |
| `LibdenoOptions.execution_deadline: Option<Duration>` | Hard wall-clock limit; on expiry the isolate is force-terminated and the run fails with `LibdenoError::Timeout`. Does **not** interrupt blocking system calls (NFS-hung file reads, synchronous `Deno.Command` waits) — those unwind only when the syscall itself returns, so the run can exceed the deadline by the syscall's duration. |
| `LibdenoError` | Enum: `Entry` (entry resolution failed), `Permission` (invalid permission flags), `Configuration` (options that cannot form a valid configuration, e.g. an empty permission list without opt-in), `Runtime` (runtime startup / script failure — JS exceptions surface here), `Core` (reserved; never constructed for script errors), `Io`, `Timeout` (deadline exceeded / subprocess handshake timed out; the message says which). |

### Async hosts (tokio/axum)

Prefer `run_async` / `run_with_output_async` inside a tokio context: they
execute the run on the caller's runtime with no spawned thread. Their future
is not `Send`, so on a multi-thread runtime run them through a
`tokio::task::LocalSet`; a single `run_async` may be awaited on a
current-thread runtime directly. They must not be interleaved with another
`run_async` on the same thread — a second one is rejected with
`LibdenoError::Configuration` (interleaved runs would abort the process). For
parallel runs use `run` (each run gets its own thread + isolate).

The sync entry points (`run` / `run_with` / `run_with_output`) remain safe to
call from inside a tokio runtime as a fallback: tokio forbids starting a
second runtime on the same thread, so the run executes on a fresh thread and
joins back. Note the calling task's thread is parked for the whole run (it is
a synchronous call); on a single-threaded runtime every other task is stalled
meanwhile, and on multi-threaded runtimes each concurrent run parks one
worker. Ordinary runs proceed fully in parallel; only output capture is
exclusive (a captured run rejects any overlapping run).

### Output capture

```rust
let out = libdeno::run_with_output(&entry, &LibdenoOptions {
    allow_all_permissions: true,
    capture_stdout: true,
    capture_stderr: true,
    ..Default::default()
})?;
println!("exit={} stdout={:?}", out.exit_code, out.stdout);
```

The capture is fd-level: `console.log` / `console.error` / `Deno.stderr.write`
and any direct fd writes land in `RunOutput`. Caveat: while a run is being
captured, *other host threads* that print to stdout/stderr during the run are
captured too. Capture is also **exclusive**: because it is process-global fd
redirection, a captured run rejects any concurrent run (captured or not) with
`LibdenoError::Configuration` — for captured runs alongside parallel execution
use `run_in_subprocess_with_output`, which pipes the child's own fds back to
the parent. Relatedly, the
runtime's console output takes the process-global
`std::io::stdout()/stderr()` locks — hosts holding those locks across an await
boundary (e.g. a custom `Write` impl used process-wide) should not hold them
while calling into `libdeno`.

`LibdenoOptions.max_capture_bytes` caps each stream's buffer (stdout and stderr
each get the budget): when a stream exceeds it, capture stops and
`RunOutput.capture_truncated` is set, so a verbose or hostile script can no
longer grow host memory without limit. `None` (default) captures without a
bound.

Output capture is unix-only: on Windows Rust std's stdout/stderr bypass the
redirected CRT fd, so `capture_stdout`/`capture_stderr` fail with a
`LibdenoError::Configuration` error there (use `run_in_subprocess_with_output`
instead — the child's own fds are piped, so it works on Windows). `run_with`
does not support capture at all — use `run_with_output`, or the
reusable-stack variant `run_with_output(&runtime, ...)` for a long-lived host
running many scripts.

Supported permission flags: `--allow-read[=paths] --allow-write[=paths] --allow-env[=names] --allow-net[=hosts] --allow-import[=hosts] --allow-run[=names] --allow-ffi[=paths] --allow-sys[=names]`, plus `-A` / `--allow-all`. `--allow-import` gates remote module loading (there is no `--allow-net` fallback); static and dynamic file imports are gated by `--allow-read`.

Full API documentation: [`docs/api.md`](docs/api.md).

End-to-end walkthrough for the common embedding shape (npm-powered plugin +
output capture): [`examples/npm-plugin.md`](examples/npm-plugin.md).

---

## Build

- Rust edition 2021. Dependencies match the official Deno stack: `deno_runtime 0.265`, `deno_core 0.410`, `deno_resolver 0.88`, `deno_graph 0.110`.
- The build script (`build.rs`) generates the V8 snapshot and pre-transpiles residual lazy-load sources; `DENO_SNAPSHOT_MINIFY_SOURCES` triggers source minification.
- The first build is slow (V8 snapshot + full dependency tree). Release debug symbols are disabled in `Cargo.toml`.
- `.node` native addon symbol export: the example host uses `.cargo/config.toml` (dev-only) to export `napi_*`. Real embedders should call `deno_napi::print_linker_flags("<host-binary-name>")` in their own `build.rs`.

---

## Documentation

- [English docs](docs/):
  - [Getting Started](docs/getting-started.md)
  - [API Reference](docs/api.md)
  - [Architecture](docs/architecture.md)
  - [npm & Module Resolution](docs/npm-support.md)
  - [Permissions](docs/permissions.md)

---

## License

MIT
