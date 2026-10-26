# Building libdeno on musl

`deno_runtime` 0.267.0 and `deno_node` 0.197.0 need a local patch to compile
for `*-unknown-linux-musl`: they call glibc-only `libc::malloc_trim`, and
`deno_node` registers an argv initializer with a signature that does not match
musl's `void(void)` init-array ABI. The patch excludes that initializer on
musl; consequently process-title argv rewriting is disabled there. Linux
`PR_SET_NAME` process naming remains available. Nothing here changes the
published crates.

The patch changes three sites in three files:

```
deno_runtime/worker.rs            +1  exclude malloc_trim on musl
deno_runtime/ops/worker_host.rs   ~1  exclude malloc_trim on musl
deno_node/ops/process.rs          +1  exclude argv init-array constructor on musl
```

## 1. Vendor and patch the two crates

```bash
mkdir -p vendor
cp -R ~/.cargo/registry/src/*/deno_runtime-0.267.0 vendor/deno_runtime
cp -R ~/.cargo/registry/src/*/deno_node-0.197.0 vendor/deno_node
(cd vendor && patch -p1 < ../patches/deno-musl-0.267.0-0.197.0.patch)
```

Then point Cargo at the copies:

```toml
[patch.crates-io]
deno_runtime = { path = "vendor/deno_runtime" }
deno_node = { path = "vendor/deno_node" }
```

Both crates must be complete copies; `[patch]` replaces a whole package.

## 2. Build natively in a musl environment

The snapshot has to be produced by a binary running under musl, so a
`--target x86_64-unknown-linux-musl` build on a glibc host is not enough. Use
an Alpine container or a musl machine, and export the bundle:

The `v8` 150.4.0 build script requires Python 3, curl, git, and C/C++ build
tools; it obtains GN/Ninja, Chromium's Rust toolchain, and (unless a compatible
Clang is configured) Clang itself. Its bindgen step requires libclang 19 or
newer; set `LIBCLANG_PATH` to that installation's `lib` directory when it is
not discoverable. These requirements are from the pinned build script; package
names and availability vary by Alpine release. Install a Rust toolchain for
the container's native musl target, then build with source mode enabled:

```sh
export V8_FROM_SOURCE=1
export LIBDENO_SNAPSHOT_EXPORT_DIR="$PWD/musl-bundle"
cargo build --release --lib
```

Run this inside an Alpine container or other musl system, and use the same
target architecture as the bundle consumer. The source build downloads and
compiles V8; the required disk space and elapsed time depend on the host. This
container recipe has not been validated end-to-end.

## 3. Reuse the bundle

The result is a normal snapshot bundle. Use it the way a published one is
used, on any build with the same target and the same libdeno and deno
versions:

```bash
LIBDENO_SNAPSHOT_DIR=$PWD/musl-bundle cargo build --release --target x86_64-unknown-linux-musl
```

Builds for that target skip snapshot creation entirely, so the cross build no
longer needs to run musl code. The dependency patch is still required: the
snapshot does not replace compiling `deno_runtime` and `deno_node`.

## Notes

- The patch is tied to the exact crate versions. Re-check the three sites when
  `deno_runtime` or `deno_node` moves; the argv constructor exclusion depends
  on the target's init-array ABI.
- Bundle identity does not cover patched sources, so a patched tree and a
  stock bundle with the same versions look identical to the manifest. Keep
  musl bundles separate from the published ones.
- Snapshot bundles exist since 0.3.7; earlier releases only have the
  cross-architecture guard.
