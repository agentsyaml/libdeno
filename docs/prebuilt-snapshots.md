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
- `libdeno-snapshot.manifest` — identity, sizes, and fingerprints for the
  snapshot, residual table, and every referenced residual source.
- `libdeno-snapshot.sources` — wrapper-generated target, manifest binding, and
  snapshot-relevant source identity.

The manifest uses format `libdeno-snapshot-2`. Format v1 bundles are rejected;
export a new bundle with the same build inputs. Fingerprints detect accidental
corruption, not compatibility, authenticity, or trustworthiness.

## Export

Run the wrapper from the consumer package directory on a machine that can run
the target. Point it at the consumer manifest: the wrapper walks that resolved
dependency graph to find the reachable Deno crates, including any consumer
patches and feature selection. A virtual workspace manifest without one package
root is not sufficient.

```bash
LIBDENO_REPO=/path/to/libdeno
target="$(rustc -vV | sed -n 's/^host: //p')"
python3 "$LIBDENO_REPO/scripts/snapshot_compat.py" export \
  --manifest-path Cargo.toml --target "$target" --bundle /tmp/libdeno-bundle -- \
  cargo build --locked --manifest-path Cargo.toml --target "$target"
```

Run this from the consumer's current working directory; the Cargo command and
wrapper must use the same package manifest, target, and feature flags. Add
`--release` to the Cargo command for a release bundle. Cargo creates the bundle
contents; the wrapper adds its source-provenance sidecar.

Exports stage files first, then take a fixed lock directory in the destination,
invalidate the old sidecar, copy the staged files, and publish a new sidecar
last. Concurrent publishers fail fast. This is not transactional rollback: an
interrupted or failed publication may leave partial, untrusted files, but no
valid sidecar. A crash can leave the lock directory behind; remove it manually
only after confirming no exporter is still active. The wrapper never steals a
stale lock automatically.

## Reuse

```bash
LIBDENO_REPO=/path/to/libdeno
target="$(rustc -vV | sed -n 's/^host: //p')"
python3 "$LIBDENO_REPO/scripts/snapshot_compat.py" import \
  --manifest-path Cargo.toml --target "$target" --bundle /tmp/libdeno-bundle -- \
  cargo build --locked --manifest-path Cargo.toml --target "$target"
```

Use the bundle's target triple for both the wrapper and Cargo; for a native
build derive it with `rustc -vV`, while cross-target builds must use the target
named by the bundle. The wrapper sets `LIBDENO_SNAPSHOT_DIR` for Cargo, verifies
the bundle against the consumer's resolved Deno dependency sources, and
provides a transient compatibility declaration to the build script. Put the
consumer's feature flags on the Cargo command after `--`; the wrapper forwards
them to metadata, so identity uses the consumer's resolved feature graph rather
than a hard-coded libdeno-only feature list. The target still needs its linker
and native libraries; only snapshot generation is removed.

To avoid building and linking Deno/V8 into the build-script host, use the
additive import-only feature (the default `snapshot` feature is disabled):

```bash
LIBDENO_REPO=/path/to/libdeno
target="$(rustc -vV | sed -n 's/^host: //p')"
python3 "$LIBDENO_REPO/scripts/snapshot_compat.py" import \
  --manifest-path Cargo.toml --target "$target" --bundle /tmp/libdeno-bundle -- \
  cargo build --locked --manifest-path Cargo.toml --target "$target" \
  --no-default-features --features app-snapshot-import
```

The wrapper verifies source provenance before Cargo starts and rejects a bundle
without it. Import-only builds avoid compiling or linking Deno/V8 in the
build-script dependency tree. `app-snapshot-import` must be a feature defined
by the consumer (for example, forwarding to `libdeno/snapshot-import`); pass
the same flags used by that consumer's build instead of assuming direct crate
feature names.

## Compatibility

A bundle retains these existing manifest identity checks; the source sidecar
supplements them rather than replacing them:

| Field | Source |
|---|---|
| `target` | Cargo target triple |
| `v8` | V8 version linked into this build |
| `deno-core`, `deno-runtime` | `[package.metadata.libdeno.upstream]` in libdeno's `Cargo.toml` |
| `typescript` | TypeScript version used to create the snapshot |
| `libdeno` | This crate's package version |

A bundle keeps the `libdeno-snapshot-2` manifest and adds
`libdeno-snapshot.sources` (`libdeno-snapshot-sources-1`). The sidecar records
the target triple, FNV hash of the manifest, and SHA-256 source identity.
The compatibility feature is unreleased and does not change the main manifest
to v3. Its expanded source-family hash differs from earlier development
sidecars; regenerate bundles produced by the narrower hash before importing.
`snapshot_compat.py` starts at the unique reachable `libdeno` package in the
consumer graph, finds its unique `deno_core` and `deno_runtime`, then selects
their non-dev runtime/build dependency closure. It hashes every selected
`deno_*` package plus `v8` and `serde_v8`; `deno_core`, `deno_runtime`, and
`deno_node` are required. This includes `deno_web`, not just three base
packages. All regular files, including Rust, JavaScript, and embedded assets,
plus metadata-reported package features are included; the
producer-only `deno_runtime/snapshot` feature is excluded. Unrelated consumer
packages and libdeno-only facilities such as npm installers are outside this
family. The scope is not the full native dependency closure or an ABI proof.

A local macOS metadata probe selected 60 packages and included all 26 literal
external init-provider names in the current runtime `snapshot_info`. These are
measurements of that stack/platform, not invariants of future graphs. Native
`aarch64-apple-darwin` export/import and 66 import-only smoke tests passed. A
real consumer's same-version `deno_web` config patch was rejected before
compilation, including with `--assume-compatible`. No cross-platform result
is implied.

The wrapper checks the source hash, target, and manifest binding before Cargo
runs, then checks that the graph did not change before the command ended. It
passes a temporary `libdeno-snapshot-compat-1` declaration (`verified` mode,
canonical bundle path, target, manifest hash, and source hash) to the build
script. Importing by setting `LIBDENO_SNAPSHOT_DIR` directly is rejected.
The declaration is supplied through `LIBDENO_SNAPSHOT_COMPATIBILITY_FILE`.
The Cargo command must contain exactly one matching `--target`; duplicate
target options fail. `--config VALUE` and `--config=VALUE` work before or after
`build`/`test` and are forwarded to metadata in the same order. The wrapper keeps
the caller's working directory and environment. Other unsupported Cargo
arguments still fail rather than being guessed; this is not full compiler-flag
compatibility.

This is a compatibility gate, not a complete native ABI, transitive dependency,
host-target, or toolchain guarantee. The sidecar and declaration are not signed;
a manually fabricated declaration is trusted input, not protection from an
adversarial bundle. The wrapper detects source changes across the build, but
cannot detect a file changed and restored before the final check. The manifest's
snapshot, residual table, and referenced residual files retain their existing
size and fingerprint checks.

The build script also verifies residual files in the bundle before copying,
then hashes the actual copied `OUT_DIR` contents against the same manifest
before reporting snapshot reuse. This detects a file replaced between source
verification and copy; it is an accidental concurrent-publisher check, not an
adversarial sandbox.

New imports require the sidecar. A legacy v2 bundle with no sidecar can be
imported only with the explicit wrapper option `--assume-compatible`, and only
when you independently know its source dependencies match. This option cannot
override an existing but mismatched sidecar. The published v0.3.7 assets use
manifest format v1 and remain rejected. The v0.3.7 tag predates this wrapper,
so it cannot regenerate a guarded bundle; the existing assets cannot be
transparently upgraded or blessed. Generate a new bundle from a source revision
or release containing this guard and the matching Deno source combination.
Crate-version changes also require a new bundle: the manifest records the
libdeno version, so a bundle built for one release is rejected by another.

The manifest still records declared ledger versions, not the full Cargo graph.
The wrapper supplements it with hashes of the selected snapshot package family,
but does not hash every transitive or native dependency. Published bundles use
the root `Cargo.lock` and snapshot build settings from the corresponding release
source revision (`cargo build --locked --release --lib`); the downstream
application does not inherit that lockfile. Other snapshot-relevant dependency
or toolchain changes remain a reason to regenerate and validate a bundle.

A snapshot is executable input, not data: it defines the JavaScript builtins
and the extension sources a runtime starts with. Treat a bundle you did not
build as you would a binary.

## Published bundles

Release assets include `libdeno-<version>-<target>-snapshot.tar.gz` with a
`.sha256` beside it. Extract the archive, then invoke the wrapper from the
guard-containing libdeno source revision from the consumer package directory,
using the consumer manifest and matching target; do not set
`LIBDENO_SNAPSHOT_DIR` directly.

The repository helper downloads, verifies, and unpacks a release asset:

```bash
./scripts/fetch-snapshot.sh 0.3.8
# v0.3.7 and earlier assets use manifest format v1 and are rejected.
```

The printed `LIBDENO_SNAPSHOT_DIR` assignment remains a convenient way to
discover the extracted directory, but exporting it alone no longer authorizes
an import. Pass that directory as the wrapper's `--bundle`; the v0.3.7 asset
still fails because it uses manifest v1. Generate a replacement from a source
revision or release containing this guard and the matching Deno source
combination. Builds never download a bundle on their own; cache and trust
decisions stay with the caller.

`*-unknown-linux-musl` is not published: the Deno crates do not compile for
musl without a local patch. [musl.md](musl.md) walks through building and
exporting your own bundle there.
