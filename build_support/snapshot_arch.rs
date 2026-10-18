pub fn ensure_compatible(
    host_arch: &str,
    target_arch: &str,
    host: &str,
    target: &str,
) -> Result<(), String> {
    if host_arch == target_arch {
        return Ok(());
    }
    Err(format!(
        "libdeno cannot build its runtime snapshot for cross-architecture TARGET={target}: HOST={host} (arch {host_arch}) differs from target arch {target_arch}. Snapshot creation runs on the build host; build natively for the target architecture. Prebuilt snapshots are not supported."
    ))
}
