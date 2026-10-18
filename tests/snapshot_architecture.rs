#[path = "../build_support/snapshot_arch.rs"]
mod snapshot_arch;

#[test]
fn same_architecture_is_allowed() {
    for arch in ["x86_64", "aarch64", "x86", "arm"] {
        assert!(snapshot_arch::ensure_compatible(arch, arch, "host", "target").is_ok());
    }
}

#[test]
fn distinct_architectures_are_rejected() {
    for (host_arch, target_arch) in [
        ("x86_64", "aarch64"),
        ("aarch64", "x86_64"),
        ("x86_64", "x86"),
        ("arm", "aarch64"),
    ] {
        let error =
            snapshot_arch::ensure_compatible(host_arch, target_arch, "host", "target").unwrap_err();
        assert!(error.contains("HOST=host"));
        assert!(error.contains("TARGET=target"));
        assert!(error.contains(host_arch) && error.contains(target_arch));
        assert!(error.contains("Prebuilt snapshots are not supported"));
    }
}

#[cfg(feature = "snapshot")]
#[test]
fn build_script_rejects_cross_arch_before_creating_snapshot_outputs() {
    use std::path::PathBuf;
    use std::process::Command;

    let host_arch = std::env::consts::ARCH;
    let target_arch = if host_arch == "aarch64" {
        "x86_64"
    } else {
        "aarch64"
    };
    let target = format!("{target_arch}-unknown-linux-gnu");
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
        .env("HOST", format!("{host_arch}-host"))
        .env("TARGET", &target)
        .env("CARGO_CFG_TARGET_ARCH", target_arch)
        .env_remove("DOCS_RS")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let diagnostic = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostic.contains("cross-architecture"), "{diagnostic}");
    assert!(
        diagnostic.contains(&format!("HOST={host_arch}-host")),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains(&format!("TARGET={target}")),
        "{diagnostic}"
    );
    assert!(std::fs::read_dir(&out_dir).unwrap().next().is_none());
    let _ = std::fs::remove_dir_all(out_dir);
}
