# Getting Started

This guide walks you through embedding the Deno runtime in a Rust program with
libdeno.

## Prerequisites

- Rust toolchain (edition 2021). A recent stable toolchain is required to
  build the Deno dependency stack.
- Linux / macOS / Windows. The Rust main API and subprocess paths are covered
  by the Windows compatibility CI job.
- Network access on the first build (crates.io) and, at runtime, whenever a
  remote module or npm package needs to be fetched.

## Build the demo

The first build is the slow one: `build.rs` compiles a V8 snapshot and the
full Deno dependency tree is compiled. Subsequent builds are incremental.

```bash
cargo build --example demo
```

This produces `target/debug/examples/demo`, a small host binary that parses
`--allow-*` permission flags, an entry path, and script arguments.

## Run the demo

```bash
# A JS entry that mixes an npm package, a node builtin, a local module,
# and a JSON import:
./target/debug/examples/demo examples/demo-app/index.js
# npm package (chalk) works
# node builtin (node:path): a/b/c
# local module: 1 + 2 = 3
# json import: name=demo-app deps=1

# A TypeScript entry:
./target/debug/examples/demo examples/demo-app/tschalk_test.ts
# ts entry + chalk: ok

# A directory entry (uses package.json "main", default index.js):
cd examples/demo-app && ../../target/debug/examples/demo .
```

The demo app at `examples/demo-app/` imports:

- `chalk` from npm (`node_modules` is installed on demand into the demo app
  directory),
- `node:path` builtin,
- a local ESM module (`math.js`),
- a JSON import with an import attribute.

## Your first embed

```rust
use libdeno::{LibdenoOptions, run};

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let options = LibdenoOptions {
    // Restrict the runtime to reads under the current directory only.
    permissions: vec!["--allow-read=.".into()],
    args: vec!["--flag".into()],
    cwd: None, // default: process current directory
    ..Default::default()
  };
  let exit_code = run("app.js", &options)?;
  Ok(())
}
```

`run` blocks until the script finishes and returns the exit code the script
requested. Each call builds its own current-thread tokio runtime and worker,
so multiple invocations are fully independent.

## Platform notes

Windows supports the Rust main API and the subprocess execution paths.
In-process output capture is the exception: fd-level capture is not supported on
Windows because Rust's standard output handles bypass the redirected CRT fd.
Use `run_in_subprocess_with_output` for per-process stdout/stderr capture; its
child pipes are supported on Windows.

## Entry resolution

`run` accepts:

| Entry | Meaning |
|---|---|
| `app.ts` | A file, run directly. |
| `./my-app` | A directory. If it contains `package.json`, its `main` field is used (default `index.js`); otherwise `index.js`. |
| `./my-app/package.json` | The package itself; `main` field is used. |

Relative paths resolve against `LibdenoOptions.cwd`, which defaults to the
process current directory.

Since v0.3.0 `cwd` is a **resolution base only**: the process cwd is never
switched, so the script itself observes the host's cwd — `Deno.cwd()` and
relative filesystem operations inside the script resolve against the host
process's cwd, not `options.cwd`. Scripts that need a specific working
directory should use absolute paths, or run through `run_in_subprocess` /
`run_in_subprocess_with_output`, where the child's cwd is pinned at spawn.

## Writing your own host binary

The demo host doubles as the napi symbol carrier for `.node` native addons
(see `build.rs`). A real host must export the same symbols; in your own
`build.rs`:

```rust
fn main() {
  deno_napi::print_linker_flags("<your-host-binary-name>");
}
```

The dev-only `.cargo/config.toml` in this repository does this via rustflags
so `cargo run --example demo` works out of the box.

## Next steps

- [API Reference](api.md) — `LibdenoOptions`, `LibdenoError`, permission flags.
- [Architecture](architecture.md) — how the runtime is assembled.
- [npm & Module Resolution](npm-support.md) — npm modes, lifecycle scripts,
  `child_process.fork`.
- [Permissions](permissions.md) — the permission model in detail.
