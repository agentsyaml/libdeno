# libdeno

[![crates.io](https://img.shields.io/crates/v/libdeno.svg)](https://crates.io/crates/libdeno)
[![docs.rs](https://docs.rs/libdeno/badge.svg)](https://docs.rs/libdeno)
[![CI](https://github.com/agentsyaml/libdeno/actions/workflows/ci.yml/badge.svg)](https://github.com/agentsyaml/libdeno/actions/workflows/ci.yml)

Embed the Deno JavaScript/TypeScript runtime in a Rust application, with support for npm packages and Deno module resolution.

libdeno runs scripts inside your process and provides file-based and in-memory entry points. Its API focuses on embedding scripts; use the Deno CLI for command-specific behavior outside that scope.

中文版文档：[README.zh-CN.md](README.zh-CN.md)

---

## What it supports

- File and in-memory JavaScript/TypeScript entry points.
- Module resolution for local files, `npm:`, `jsr:`, remote URLs, `node:` built-ins, JSON, and WASM, subject to permissions and runtime support.
- Reusable project resolver state through `LibdenoRuntime`.
- Native Rust extensions through per-runtime `ExtensionFactory` values; see the
  [native extension guide](docs/native-extensions.md).
- Optional output capture and child-process execution.
- npm lifecycle scripts are disabled by default. The `npm` Cargo feature can be disabled at build time; see [Build](#build).

---

## Quick Start

Save this as `app.js`:

```js
console.log("Hello from Deno");
```

Then call it from a Rust host:

```rust
use libdeno::{run, LibdenoError, LibdenoOptions};

fn main() -> Result<(), LibdenoError> {
    let options = LibdenoOptions {
        permissions: vec!["--allow-read=.".into()],
        ..Default::default()
    };
    let exit_code = run("app.js", &options)?;
    println!("exit code: {exit_code}");
    Ok(())
}
```

`run` accepts an entry file, an application directory, or its `package.json`.
Directory entries use `package.json`'s `main` field, defaulting to `index.js`.

### Run in-memory source

Use `run_source(code, lang, base_dir, &options)` when the entry is held in
memory. `code` accepts `AsRef<[u8]>`; `lang` selects JavaScript or TypeScript.

```rust
use libdeno::{run_source, SourceLang, LibdenoOptions};

let options = LibdenoOptions {
    permissions: vec!["--allow-read=./my-app".into()],
    ..Default::default()
};
run_source("console.log('hi')", SourceLang::JavaScript, "./my-app", &options).unwrap();
run_source("const x: number = 1 + 2; console.log(x)", SourceLang::TypeScript, "./my-app", &options).unwrap();
```

The source gets a unique virtual `file:` URL under `base_dir`. Relative imports
and project configuration resolve from that directory. The entry itself is not
read from disk, but its virtual path is subject to the same read-permission
check. In-memory entry points are available through in-process APIs only.

Use the corresponding `run_source_with_output` or async API when needed.

### Choose a run API

- Use `run` for a one-off entry. Ordinary calls can run concurrently.
- Use `LibdenoRuntime` with `run_with` when a long-lived host runs many entries
  in one project and can reuse its resolver state. Call `refresh()` after
  resolver changes that it does not detect automatically.
- Use `run_async` in a Tokio task when the caller needs async execution. Its
  future is `!Send`; do not overlap `run_async` calls on the same thread.
- Use `run_in_subprocess` when the script needs a separate working directory
  or `Deno.exit` must not exit the host. It supervises the direct child only.
- Use `run_with_output` for simple output capture; use
  `run_in_subprocess_with_output` for capture alongside parallel runs.

```rust
use libdeno::{LibdenoRuntime, LibdenoOptions, run_with};

let runtime = LibdenoRuntime::new("./my-app").await.unwrap();
let options = LibdenoOptions {
    permissions: vec!["--allow-read=.".into()],
    ..Default::default()
};
let exit_code = run_with(&runtime, "app.js", &options).unwrap();
```

`run_with` resolves entries against the runtime's directory and creates
permission-bound loading components from that call's options. The process
working directory remains the host's; `options.cwd`, when set, must match the
runtime directory.

---

## API

| Item | Description |
|---|---|
| `LibdenoOptions.permissions` | Explicit `--allow-*` capabilities. An empty list is a configuration error unless `allow_all_permissions` or `prompt` is enabled. |
| `LibdenoOptions.args` / `cwd` | Script arguments and the base directory for resolving entries and imports. In-process scripts observe the host process's current directory. |
| `LibdenoOptions.features` | Overrides the default unstable API set (`kv`, `cron`, `ffi`, `webgpu`, `worker-options`). |
| `LibdenoError` | Errors for entry resolution, permissions, configuration, runtime execution, I/O, or deadlines. See the [API reference](docs/api.md) for variants. |

Remote module imports require `--allow-import`; file imports require
`--allow-read`. See [Permissions](docs/permissions.md) for the full list.

For Tokio hosts, prefer `run_async` / `run_with_output_async`. Their future is
`!Send`; await async runs one at a time per thread. Synchronous `run` calls
block the calling Tokio worker until the script returns.

### Output capture

Set `capture_stdout` / `capture_stderr` and call `run_with_output` to receive a
`RunOutput`. For platform support, limits, and concurrency behavior, see
[Security and resource boundaries](SECURITY.md#runtime-boundaries).

For untrusted scripts, use an OS-level sandbox. Permissions and resource
options have limits; see [SECURITY.md](SECURITY.md). Fork hosts that need Node
IPC must initialize it during trusted startup ([API contract](docs/api.md#initialize_node_ipc_from_env)).

See the [full API reference](docs/api.md) or the
[npm plugin example](examples/npm-plugin.md).

---

## Build

- Requires Rust 1.95 or newer; the crate uses edition 2021. The first build compiles the dependencies and V8 snapshot.
- Hosts that load `.node` addons must export `napi_*` symbols; see [`docs/getting-started.md`](docs/getting-started.md) for linker setup.

### Cargo feature: `npm`

The default features include `npm`. To build without npm installation support,
use `--no-default-features --features snapshot`. `npm:` imports then fail; an
existing local dependency tree can use `{"nodeModulesDir":"manual"}`.

---

## Documentation

- [English docs](docs/):
  - [Getting Started](docs/getting-started.md)
  - [API Reference](docs/api.md)
  - [Native Rust extensions](docs/native-extensions.md)
  - [Architecture](docs/architecture.md)
  - [npm & Module Resolution](docs/npm-support.md)
  - [Permissions](docs/permissions.md)

---

## License

MIT
