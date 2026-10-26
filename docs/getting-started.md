# Getting Started

This guide walks you through embedding the Deno runtime in a Rust program with
libdeno.

## Prerequisites

- A recent stable Rust toolchain (edition 2021).
- Linux, macOS, or Windows.
- Network access on the first build (crates.io) and, at runtime, whenever a
  remote module or npm package needs to be fetched.

## Build the demo

The first build compiles V8 and the Deno dependency tree. Later builds are
incremental.

```bash
cargo build --example demo
```

This produces `target/debug/examples/demo`, a small host binary that parses
`--allow-*` permission flags, an entry path, and script arguments.

The V8 snapshot is created by the build-host process. A build whose target CPU
architecture differs from the host is rejected unless a prebuilt bundle is
supplied: export one with `LIBDENO_SNAPSHOT_EXPORT_DIR` on a machine that can
run the target, then build with `LIBDENO_SNAPSHOT_DIR` pointing at it. See
[Prebuilt snapshots](prebuilt-snapshots.md). Matching architecture alone does
not certify cross-OS or ABI snapshot compatibility.

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
requested. Each call gets a fresh worker and module graph; independent runs can
execute in parallel.

## Platform notes

In-process output capture is not supported on Windows. Use
`run_in_subprocess_with_output` to capture a child's stdout and stderr.

## Entry resolution

`run` accepts:

| Entry | Meaning |
|---|---|
| `app.ts` | A file, run directly. |
| `./my-app` | A directory. If it contains `package.json`, its `main` field is used (default `index.js`); otherwise `index.js`. |
| `./my-app/package.json` | The package itself; `main` field is used. |

Relative paths resolve against `LibdenoOptions.cwd`, which defaults to the
process current directory. In-process runs do not change the host process cwd;
the script sees that host cwd through `Deno.cwd()`. Use absolute paths or a
subprocess API when the script needs a different process cwd.

## Writing your own host binary

For `.node` native addons, your host must export the N-API symbols. Call this
from the host crate's `build.rs` (`deno_napi` goes in `[build-dependencies]`),
because it emits `cargo:rustc-link-arg-bin` directives that only a build script
can print:

```rust
// build.rs
fn main() {
  // Scope the exported N-API symbols to this one binary target.
  deno_napi::print_linker_flags("<your-host-binary-name>");
}
```

## Next steps

- [API Reference](api.md) — `LibdenoOptions`, `LibdenoError`, permission flags.
- [Architecture](architecture.md) — how the runtime is assembled.
- [npm & Module Resolution](npm-support.md) — npm modes, lifecycle scripts,
  `child_process.fork`.
- [Permissions](permissions.md) — the permission model in detail.
- [Python API](python.md) — the `pip install libdeno` binding.
- [Prebuilt snapshots](prebuilt-snapshots.md) — export a snapshot on one
  machine and reuse it for a cross-architecture build.
