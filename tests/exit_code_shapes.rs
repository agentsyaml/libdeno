//! `exit_code` is the script's requested value in-process, but a real process
//! status in a subprocess. Both halves are pinned here because the difference
//! is invisible in the return type.
//!
//! How much of the value survives depends on the platform: a POSIX wait status
//! keeps the low 8 bits, while a Windows process exit code is the full 32-bit
//! value the script passed. Deno CLI 2.9.8 matches both.

use std::fs;
use std::path::PathBuf;

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-exit-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_case(script: &str, subprocess: bool) -> i32 {
    let dir = temp_dir(if subprocess { "sub" } else { "in" });
    let entry = dir.join("main.js");
    fs::write(&entry, script).unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    let options = LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    };
    let code = if subprocess {
        libdeno::run_in_subprocess(&entry, &options).unwrap()
    } else {
        libdeno::run(&entry, &options).unwrap()
    };
    let _ = fs::remove_dir_all(dir);
    code
}

#[test]
fn in_process_keeps_the_requested_exit_code() {
    assert_eq!(run_case("Deno.exit(300);", false), 300);
    assert_eq!(run_case("Deno.exit(-1);", false), -1);
    assert_eq!(run_case("Deno.exit(256);", false), 256);
}

/// Same values the `deno` CLI produces for these scripts.
#[cfg(unix)]
#[test]
fn subprocess_reports_the_posix_truncated_status() {
    assert_eq!(run_case("Deno.exit(300);", true), 44);
    assert_eq!(run_case("Deno.exit(-1);", true), 255);
    assert_eq!(run_case("Deno.exit(256);", true), 0);
}

/// A Windows process exit code is 32-bit, so nothing is truncated.
#[cfg(windows)]
#[test]
fn subprocess_reports_the_full_windows_status() {
    assert_eq!(run_case("Deno.exit(300);", true), 300);
    assert_eq!(run_case("Deno.exit(-1);", true), -1);
    assert_eq!(run_case("Deno.exit(256);", true), 256);
}
