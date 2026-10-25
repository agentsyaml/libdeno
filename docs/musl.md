# Building libdeno on musl

`deno_runtime` 0.267.0 and `deno_node` 0.197.0 do not compile for
`*-unknown-linux-musl`: they call `libc::malloc_trim`, which only `libc`
exposes for glibc, and `deno_node` inspects a glibc-specific argv layout.
The Deno project is no longer taking feature work, so do not expect an
upstream fix; musl builds need a local patch and a local bundle. Nothing
here changes the published crates.

The patch is four lines in three files:

```
deno_runtime/worker.rs            +1  #[cfg(not(target_env = "musl"))]
deno_runtime/ops/worker_host.rs   ~1  cfg gated with not(target_env = "musl")
deno_node/ops/process.rs          +2  early return on musl
```

## 1. Vendor and patch the two crates

```bash
mkdir -p vendor
cp -R ~/.cargo/registry/src/*/deno_runtime-0.267.0 vendor/deno_runtime
cp -R ~/.cargo/registry/src/*/deno_node-0.197.0 vendor/deno_node
cd vendor && patch -p1 < ../patches/deno-musl-0.267.0-0.197.0.patch
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

```bash
docker run --rm -v "$PWD:/src" -w /src alpine:3.22 sh -c '
  apk add --no-cache build-base clang cmake ninja python3 curl git linux-headers
  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
  . "$HOME/.cargo/env"
  LIBDENO_SNAPSHOT_EXPORT_DIR=/src/musl-bundle cargo build --release --lib
'
```

V8 has no prebuilt archive for musl, so this builds V8 from source: budget
40+ minutes and a few GiB of disk.

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
  `deno_runtime` or `deno_node` moves; the argv early return in particular
  depends on how `_start` passes `argc`/`argv`.
- Bundle identity does not cover patched sources, so a patched tree and a
  stock bundle with the same versions look identical to the manifest. Keep
  musl bundles separate from the published ones.
- Snapshot bundles exist since 0.3.7; earlier releases only have the
  cross-architecture guard.