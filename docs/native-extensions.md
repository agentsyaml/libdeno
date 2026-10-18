# Native Rust extensions

`LibdenoOptions::extensions` accepts an `ExtensionFactory` that returns fresh
`deno_core::Extension` values. The factory runs on the runtime thread once for
the main worker and once for every web worker, including nested workers. The
extensions are appended after Deno's built-ins and libdeno's runtime guards, so
declare dependencies on extensions that precede yours.

```rust
use libdeno::{deno_core, ExtensionFactory, LibdenoOptions, run};

#[deno_core::op2]
#[string]
fn op_host_greeting() -> String {
    "hello from Rust".to_string()
}

deno_core::extension!(host_greeting,
    ops = [op_host_greeting],
    esm_entry_point = "ext:host_greeting/init.js",
    esm = [dir "examples/native_extension", "init.js"],
);

let options = LibdenoOptions {
    allow_all_permissions: true,
    extensions: Some(ExtensionFactory::new(|| vec![host_greeting::init()])),
    ..Default::default()
};
run("app.js", &options)?;
```

Use an extension ESM entry point to install a JS-facing function; for example,
the entry point can import `core` from `ext:core/mod.js` and assign
`core.ops.op_host_greeting` to a global. See `examples/native_extension/` for a
runnable example. The `#[op2]` and `extension!` macros refer to `deno_core`
directly in generated code, so downstream hosts must declare it as a direct
dependency as well as using libdeno. This API is available in libdeno 0.3.5. To
build a local checkout from source, point `libdeno` at the checkout and keep
`deno_core` aligned with its Deno stack (currently 0.412.0). For example, with
the checkout at `../libdeno`:

```toml
[dependencies]
libdeno = { path = "../libdeno" }
deno_core = "0.412.0"
```

Cargo's version requirement is compatible rather than exact; update both
dependencies together when moving to a different Deno stack version. The
downstream compile fixture at
`tests/fixtures/native-extension-consumer/` checks this setup without relying
on the repository's own direct `deno_core` dependency. From the repository
root, compile it with the root lockfile and shared target directory:

```sh
cp Cargo.lock tests/fixtures/native-extension-consumer/Cargo.lock
CARGO_TARGET_DIR="$PWD/target" cargo check --offline --manifest-path tests/fixtures/native-extension-consumer/Cargo.toml
```

Entry points initialize after the built-in default snapshot; custom extensions
are not included in that snapshot. Add only your extension's JS initialization
there—do not copy or reinitialize JS sources already included in Deno's
snapshot. This feature does not build a custom snapshot.

Run the example from the repository root:

```sh
cargo run --example native_extension
```

Expected output:

```text
hello from Rust
```

Run the native runtime integration tests with:

```sh
cargo test --test native_extensions
```

Expected result: `2 passed` (or `3 passed` with `execution-control` enabled).

Each worker has its own `OpState`. A factory closure may capture shared `Arc`
state deliberately, but that sharing is the embedder's responsibility. Native
ops are not automatically permission-gated: enforce any required permissions
or other host policy inside the operation. Rust extension factories cannot be
serialized to child or supervisor subprocesses; those backends reject options
containing one before spawning.
