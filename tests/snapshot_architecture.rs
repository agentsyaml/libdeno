#[path = "../build_support/snapshot_arch.rs"]
mod snapshot_arch;

#[test]
fn same_target_triple_is_allowed() {
    for (arch, target) in [
        ("x86_64", "x86_64-apple-darwin"),
        ("aarch64", "aarch64-apple-darwin"),
        ("x86_64", "x86_64-unknown-linux-gnu"),
        ("aarch64", "aarch64-unknown-linux-gnu"),
        ("x86_64", "x86_64-pc-windows-msvc"),
        ("x86_64", "x86_64-unknown-linux-musl"),
    ] {
        assert!(snapshot_arch::ensure_compatible(arch, arch, target, target).is_ok());
    }
}

#[test]
fn cross_target_builds_are_rejected() {
    for (host, target) in [
        // Same architecture, different OS: host-compiled V8 cannot produce a
        // snapshot labelled for another platform.
        ("x86_64-apple-darwin", "x86_64-unknown-linux-gnu"),
        // Same OS family, incompatible ABI.
        ("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl"),
        // Cross-architecture.
        ("x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"),
        ("aarch64-apple-darwin", "x86_64-apple-darwin"),
        ("aarch64-apple-darwin", "aarch64-unknown-linux-gnu"),
    ] {
        fn arch(triple: &str) -> &str {
            triple.split('-').next().unwrap()
        }
        let error =
            snapshot_arch::ensure_compatible(arch(host), arch(target), host, target).unwrap_err();
        assert!(error.contains(&format!("HOST={host}")), "{error}");
        assert!(error.contains(&format!("TARGET={target}")), "{error}");
        assert!(error.contains("LIBDENO_SNAPSHOT_DIR"), "{error}");
        assert!(error.contains("docs/prebuilt-snapshots.md"), "{error}");
    }
}

#[cfg(feature = "snapshot")]
#[test]
fn build_script_rejects_cross_target_before_creating_snapshot_outputs() {
    use std::path::PathBuf;
    use std::process::Command;

    let host_arch = std::env::consts::ARCH;
    // Same architecture, different OS: the case an arch-only check let through.
    let host = format!("{host_arch}-apple-darwin");
    let target = format!("{host_arch}-unknown-linux-gnu");
    let marker = PathBuf::from(env!("LIBDENO_SNAPSHOT_BUILD_SCRIPT_MARKER"));
    let script = PathBuf::from(std::fs::read_to_string(marker).unwrap().trim());
    assert!(
        script.is_file(),
        "Cargo build script executable not found: {script:?}"
    );

    let out_dir =
        std::env::temp_dir().join(format!("libdeno-snapshot-arch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();
    let output = Command::new(script)
        .env("OUT_DIR", &out_dir)
        .env("HOST", &host)
        .env("TARGET", &target)
        .env("CARGO_CFG_TARGET_ARCH", host_arch)
        .env_remove("LIBDENO_SNAPSHOT_DIR")
        .env_remove("DOCS_RS")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("cross-target"), "{diagnostic}");
    assert!(diagnostic.contains(&format!("HOST={host}")), "{diagnostic}");
    assert!(
        diagnostic.contains(&format!("TARGET={target}")),
        "{diagnostic}"
    );
    assert!(std::fs::read_dir(&out_dir).unwrap().next().is_none());
    let _ = std::fs::remove_dir_all(out_dir);
}
