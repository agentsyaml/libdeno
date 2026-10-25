# Prebuilt snapshots

A snapshot bundle lets one machine build the V8 startup snapshot and another
machine reuse it. It exists for cross-architecture builds and for build hosts
that cannot run the target binary at all: the snapshot is normally created by
booting a Deno runtime, which requires executing target code.

A bundle contains:

- `CLI_SNAPSHOT.bin` — the V8 startup snapshot.
- `EXTENSION_RESIDUAL_SOURCES.rs` — the generated table of lazy extension
  sources that are not compiled into the snapshot.
- `residual_sources/**` — those sources, already transpiled.
- `libdeno-snapshot.manifest` — identity, sizes, and fingerprints.

## Export

Build natively on a machine that can run the target:

```bash
LIBDENO_SNAPSHOT_EXPORT_DIR=/tmp/libdeno-bundle cargo build --locked --lib
```

The directory then holds a bundle. Copy it to the build host that compiles for
the target.

## Reuse

```bash
LIBDENO_SNAPSHOT_DIR=/tmp/libdeno-bundle \
  cargo build --locked --target aarch64-unknown-linux-musl
```

With `LIBDENO_SNAPSHOT_DIR` set, the build script skips snapshot creation and
the cross-architecture guard. The target still needs its linker and native
libraries; only the snapshot step is removed.

Set both variables on the same build and the import wins; export is ignored.

## Compatibility

A bundle is accepted only when all of these match the current build:

| Field | Source |
|---|---|
| `target` | Cargo's `TARGET` triple, e.g. `aarch64-unknown-linux-musl` |
| `v8` | V8 version linked into this build |
| `deno-core`, `deno-runtime` | `[package.metadata.libdeno.upstream]` in `Cargo.toml` |
| `typescript` | TS compiler version the snapshot was created with |
| `libdeno` | Crate version |

The snapshot and the residual table must also match their recorded size and
fingerprint, and every file the table references must be present.

Because the crate version is part of the identity, a new libdeno release
needs a new bundle.

The manifest cannot see modifications to the dependency crates. If you patch
`deno_core`, `deno_runtime`, or `deno_node`, the declared versions still match
and a stock bundle will be accepted even though its snapshot no longer matches
your code. Generate your own bundle on a matching-architecture machine in that
case.

A snapshot is executable input, not data: it defines the JavaScript builtins
and the extension sources a runtime starts with. Treat a bundle you did not
build as you would a binary.

## Published bundles

Release assets include `libdeno-<version>-<target>-snapshot.tar.gz` with a
`.sha256` beside it. Extract the archive and point `LIBDENO_SNAPSHOT_DIR` at the
extracted directory.

The repository ships a helper that does the three steps for the host target:

```bash
./scripts/fetch-snapshot.sh 0.3.7
# download, verify sha256, extract, then print the variable to export
export LIBDENO_SNAPSHOT_DIR=...   # printed by the script
```

Builds never download a bundle on their own. Cache and trust decisions stay
with the caller.