# libdeno

[![crates.io](https://img.shields.io/crates/v/libdeno.svg)](https://crates.io/crates/libdeno)
[![docs.rs](https://docs.rs/libdeno/badge.svg)](https://docs.rs/libdeno)
[![CI](https://github.com/agentsyaml/libdeno/actions/workflows/ci.yml/badge.svg)](https://github.com/agentsyaml/libdeno/actions/workflows/ci.yml)

> Embed the Deno runtime in Rust with direct `npm:` specifier support.

libdeno is a Rust crate that embeds a full Deno runtime (V8 + the official module graph pipeline) inside your program. Your JS/TS code can `import` npm packages, remote modules, `jsr:` and `node:` builtins directly, all handled by the official Deno resolver stack — the same behavior as `deno run`, but running inside your process.

中文版文档：[README.zh-CN.md](README.zh-CN.md)

---

## Features

- **Official module graph pipeline**: `npm:`, `jsr:`, remote `https://`, `node:`, local files, JSON, WASM, import maps (from `deno.json`), and TypeScript transpilation are all handled by `deno_graph` + `deno_resolver`.
- **npm integration**: automatically discovers and uses an existing `node_modules` (BYONM); installs on demand otherwise (managed mode). Supports CJS packages and `.node` native addons. npm lifecycle scripts do not run by default (matching deno CLI 2.x).
- **`child_process.fork` support**: the npm resolution snapshot propagates to child processes.
- **Web Workers**: `new Worker(...)` nested workers reuse the same module loader and snapshot.
- **Permission model**: CLI-style `--allow-*` capability strings; an empty list allows everything by default.
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
let options = LibdenoOptions::default();
let exit_code = run_with(&runtime, "app.js", &options).unwrap();
```

Scripts run in the runtime's cwd (`LibdenoOptions.cwd` is ignored there); each `run_with` still rebuilds the permission-bound file fetcher / graph loader / graph from its own `options.permissions`, so one run's grants can never leak into another.

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
| `run(entry, &options) -> Result<i32, LibdenoError>` | Runs the entry to completion and returns the exit code the script requested. Each call builds its own current-thread runtime and worker; invocations share the process cwd (serialized via an internal lock, restored afterwards), the on-disk npm/HTTP caches, and `DENO_DIR`. |
| `LibdenoRuntime::new(cwd)` | Builds the resolver stack for a project directory once (async). Reused by `run_with`; rebuilt automatically when the config chain (deno.json / deno.jsonc / import_map.json / package.json / .npmrc / node_modules) changes. |
| `run_with(&runtime, entry, &options) -> Result<i32, LibdenoError>` | Like `run`, but reuses `runtime`'s resolver stack. Semantics identical to `run` (cwd lock, tokio re-entry check, exit codes, deadlines); the script runs in the runtime's cwd and permission-bound components are rebuilt per call. |
| `run_in_subprocess(entry, &options) -> Result<i32, LibdenoError>` | Runs the entry in a child process. `Deno.exit(n)` then terminates only the child; the host stays alive and observes `n`. The host must call `maybe_handle_child_mode()` at the start of `main()`. |
| `maybe_handle_child_mode() -> bool` | Services `run_in_subprocess` child requests. Returns `false` on a normal host launch; in child mode it executes the script and exits with its code. |
| `LibdenoOptions.permissions: Vec<String>` | `--allow-*` capability strings. An empty list allows everything; passing any entry restricts the runtime to the declared capabilities. |
| `LibdenoOptions.args: Vec<String>` | Arguments exposed to the script via `process.argv` (after argv[0]). |
| `LibdenoOptions.cwd: Option<PathBuf>` | Working directory that relative paths (entry, permissions, `node_modules` discovery) resolve against. Defaults to the process current directory. |
| `LibdenoOptions.max_heap_bytes: Option<usize>` | Hard cap on the V8 old-generation heap in bytes; V8 aborts with OOM when hit. Applies to the main worker **and** web workers spawned via `new Worker(...)`. |
| `LibdenoOptions.execution_deadline: Option<Duration>` | Hard wall-clock limit; on expiry the isolate is force-terminated and the run fails with `LibdenoError::Timeout`. Does **not** interrupt blocking system calls (NFS-hung file reads, synchronous `Deno.Command` waits) — those unwind only when the syscall itself returns, so the run can exceed the deadline by the syscall's duration. |
| `LibdenoError` | Enum: `Entry` (entry resolution failed), `Permission` (invalid permission flag), `Runtime`, `Core`, `Js` (script exception), `Io`, `Timeout` (deadline exceeded, isolate terminated). |

Supported permission flags: `--allow-read[=paths] --allow-write[=paths] --allow-env[=names] --allow-net[=hosts] --allow-run[=names] --allow-ffi[=paths] --allow-sys[=names]`, plus `-A` / `--allow-all`.

Full API documentation: [`docs/api.md`](docs/api.md).

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
