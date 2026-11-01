# Getting Started

This guide walks you through embedding the Deno runtime in a Rust program with
libdeno.

## Prerequisites

- Rust 1.95 or newer (edition 2021).
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
`--allow-*` permission flags, an entry path, and script arguments. It rejects an
unrecognized flag rather than skipping it, because it grants everything when no
`--allow-*` flag is present.

The V8 snapshot is created by the build-host process. A build whose target CPU
architecture differs from the host is rejected unless a prebuilt bundle is
exported and imported through the compatibility wrapper. Invoke the wrapper
from the consumer package directory, using that package's manifest, target, and
feature selection; setting `LIBDENO_SNAPSHOT_DIR` alone is rejected. See
[Prebuilt snapshots](prebuilt-snapshots.md) for commands. Matching architecture
alone does not certify cross-OS or ABI snapshot compatibility.

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
  println!("script exited with {exit_code}");
  Ok(())
}
```

The permission string resolves against `LibdenoOptions.cwd`, the same base the
relative entry path uses — not the host's process working directory when the two
differ. The script's own relative paths are the exception: in-process they still
follow the host process directory, because `cwd` is a resolution base and not a
directory change (`Deno.cwd()` reports the host cwd). Grant absolute paths, or
run in a subprocess, if the script opens files by relative path.

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
the script sees that host cwd through `Deno.cwd()`, and a relative path it opens
resolves against it — not against `cwd`. The subprocess backend does change
directory, so the same script reads a different file. Use absolute paths or a
subprocess API when the script needs a different process cwd.

## Writing your own host binary

For `.node` native addons, your host must export the N-API symbols. Call this
from the host crate's `build.rs`, because it emits `cargo:rustc-link-arg-bin`
directives that only a build script can print:

```toml
[build-dependencies]
# Match the N-API version inside libdeno's Deno stack, not the latest release.
deno_napi = "0.190.0"
```

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
