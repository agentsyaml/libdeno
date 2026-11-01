//! What a path grant covers, and what it does not.
//!
//! Grants are checked against the path a script names, so lexical escapes are
//! rejected while links are followed. Deno CLI 2.9.8 behaves the same way; the
//! point of these tests is to keep libdeno from drifting and to make the
//! boundary explicit for callers who hand out write grants.

use std::fs;
use std::path::{Path, PathBuf};

use libdeno::LibdenoOptions;

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-grant-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn options(allowed: &Path) -> LibdenoOptions {
    LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", allowed.display()),
            format!("--allow-write={}", allowed.display()),
        ],
        args: vec![allowed.to_string_lossy().to_string()],
        capture_stdout: true,
        ..Default::default()
    }
}

/// In-process capture redirects the process-global fds and holds the exclusive
/// run lease, so these runs cannot overlap with each other or with any other
/// capturing test in this binary.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn run(entry: &Path, options: &LibdenoOptions) -> String {
    let _guard = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let output = libdeno::run_with_output(entry, options).unwrap();
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn a_lexical_escape_is_denied() {
    let dir = temp_dir("lexical");
    let allowed = dir.join("allowed");
    fs::create_dir_all(&allowed).unwrap();
    fs::write(dir.join("secret.txt"), "ORIGINAL").unwrap();
    let entry = allowed.join("main.js");
    fs::write(
        &entry,
        "try { Deno.writeTextFileSync(Deno.args[0] + '/../secret.txt', 'PWNED'); console.log('OK'); }\n\
         catch (e) { console.log('DENIED ' + e.constructor.name); }\n",
    )
    .unwrap();

    let stdout = run(&entry, &options(&allowed));
    assert!(stdout.contains("DENIED NotCapable"), "got: {stdout}");
    assert_eq!(
        fs::read_to_string(dir.join("secret.txt")).unwrap(),
        "ORIGINAL"
    );
    let _ = fs::remove_dir_all(dir);
}

/// Links need an API std exposes only on Unix, and the behaviour under test is
/// the POSIX one: Deno's path check follows a link inside the grant.
#[cfg(unix)]
#[test]
fn a_symlink_inside_the_granted_root_is_followed() {
    let dir = temp_dir("symlink");
    let allowed = dir.join("allowed");
    let outside = dir.join("outside");
    fs::create_dir_all(&allowed).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("target.txt"), "ORIGINAL").unwrap();
    std::os::unix::fs::symlink(outside.join("target.txt"), allowed.join("file")).unwrap();
    std::os::unix::fs::symlink(&outside, allowed.join("dir")).unwrap();
    let entry = allowed.join("main.js");
    fs::write(
        &entry,
        "const a = Deno.args[0];\n\
         Deno.writeTextFileSync(a + '/file', 'THROUGH-FILE');\n\
         Deno.writeTextFileSync(a + '/dir/through-dir.txt', 'THROUGH-DIR');\n\
         console.log('DONE');\n",
    )
    .unwrap();

    let stdout = run(&entry, &options(&allowed));
    assert!(stdout.contains("DONE"), "got: {stdout}");
    assert_eq!(
        fs::read_to_string(outside.join("target.txt")).unwrap(),
        "THROUGH-FILE"
    );
    assert_eq!(
        fs::read_to_string(outside.join("through-dir.txt")).unwrap(),
        "THROUGH-DIR"
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_hard_link_inside_the_granted_root_is_followed() {
    let dir = temp_dir("hardlink");
    let allowed = dir.join("allowed");
    fs::create_dir_all(&allowed).unwrap();
    fs::write(dir.join("secret.txt"), "ORIGINAL").unwrap();
    fs::hard_link(dir.join("secret.txt"), allowed.join("hard.txt")).unwrap();
    let entry = allowed.join("main.js");
    fs::write(
        &entry,
        "try { Deno.writeTextFileSync(Deno.args[0] + '/hard.txt', 'PWNED'); console.log('OK'); }\n\
         catch (e) { console.log('DENIED ' + e.constructor.name); }\n",
    )
    .unwrap();

    let stdout = run(&entry, &options(&allowed));
    assert!(stdout.contains("OK"), "got: {stdout}");
    assert_eq!(fs::read_to_string(dir.join("secret.txt")).unwrap(), "PWNED");
    let _ = fs::remove_dir_all(dir);
}
