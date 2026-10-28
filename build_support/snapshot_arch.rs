/// The snapshot is produced by running host code (the build script links Deno
/// and V8), so it is only valid for the host triple itself: a matching
/// architecture is not enough. macOS -> Linux and glibc -> musl would otherwise
/// ship a host-generated snapshot labelled as a foreign target artifact.
pub fn ensure_compatible(
    host_arch: &str,
    target_arch: &str,
    host: &str,
    target: &str,
) -> Result<(), String> {
    if host == target {
        return Ok(());
    }
    Err(format!(
        "libdeno cannot create its runtime snapshot for a cross-target build: HOST={host} (arch \
         {host_arch}) must equal TARGET={target} (arch {target_arch}). The snapshot is created by \
         running host code, so cross-architecture, cross-OS, and cross-ABI (glibc/musl) builds \
         cannot label a host-generated snapshot as a {target} artifact. Build natively on a \
         {target} host, or import a snapshot bundle built for {target} with \
         scripts/snapshot_compat.py import (see docs/prebuilt-snapshots.md); the wrapper supplies \
         LIBDENO_SNAPSHOT_DIR."
    ))
}
