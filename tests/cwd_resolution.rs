//! An explicit `LibdenoOptions.cwd` that does not resolve is a broken sandbox
//! pin, and is rejected instead of silently running the script elsewhere.
//!
//! Before this, in-process runs fell back to the raw path and executed with the
//! host's cwd while the subprocess path failed on spawn: the same request had
//! two outcomes depending on the backend, and the in-process one ignored the
//! caller's sandbox pin.

use std::fs;
use std::path::PathBuf;

use libdeno::{LibdenoError, LibdenoOptions};

/// Capture redirects process-global fds, so these tests cannot overlap.
static CAPTURE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The path without Windows' `\\?\` verbatim prefix.
///
/// `canonicalize` returns the verbatim form on Windows, but a process's own
/// current directory (which is what a script's relative path resolves against)
/// is the plain form. A grant spelled one way does not cover a path spelled the
/// other, so the fixture uses the plain form for both.
fn plain(path: PathBuf) -> PathBuf {
    match path.to_str().and_then(|text| text.strip_prefix(r"\\?\")) {
        Some(rest) => PathBuf::from(rest),
        None => path,
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-cwd-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(name: &str) -> (PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let entry = dir.join("main.js");
    fs::write(&entry, "console.log(Deno.cwd());").unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));
    (dir, entry)
}

fn options(cwd: PathBuf) -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        cwd: Some(cwd),
        ..Default::default()
    }
}

fn assert_unusable<T: std::fmt::Debug>(result: Result<T, LibdenoError>, needle: &str) {
    match result {
        Err(LibdenoError::Configuration(message)) => {
            assert!(
                message.contains(needle),
                "expected `{needle}` in: {message}"
            );
        }
        other => panic!("expected a Configuration error, got: {other:?}"),
    }
}

#[cfg(not(windows))]
#[test]
fn missing_cwd_is_rejected_on_both_backends() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, entry) = setup("missing");
    let ghost = dir.join("does-not-exist");
    assert_unusable(
        libdeno::run_with_output(&entry, &options(ghost.clone())),
        "is not a usable directory",
    );
    assert_unusable(
        libdeno::run_in_subprocess_with_output(&entry, &options(ghost)),
        "is not a usable directory",
    );
    let _ = fs::remove_dir_all(dir);
}

#[cfg(not(windows))]
#[test]
fn file_as_cwd_is_rejected_on_both_backends() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, entry) = setup("file");
    let file = dir.join("afile");
    fs::write(&file, "x").unwrap();
    assert_unusable(
        libdeno::run_with_output(&entry, &options(file.clone())),
        "is not a directory",
    );
    assert_unusable(
        libdeno::run_in_subprocess_with_output(&entry, &options(file)),
        "is not a directory",
    );
    let _ = fs::remove_dir_all(dir);
}

#[cfg(not(windows))]
#[test]
fn existing_cwd_is_still_accepted() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let (dir, entry) = setup("ok");
    let sub = dir.join("sub");
    fs::create_dir_all(&sub).unwrap();
    // In-process keeps the documented behavior: cwd is a resolution base, not
    // a chdir, so the script still observes the host cwd.
    let output = libdeno::run_with_output(&entry, &options(sub.clone())).unwrap();
    assert_eq!(output.exit_code, 0);
    // The subprocess child really switches, so it observes the pinned cwd.
    let output = libdeno::run_in_subprocess_with_output(&entry, &options(sub.clone())).unwrap();
    let printed = String::from_utf8_lossy(&output.stdout);
    assert!(
        printed.contains(sub.canonicalize().unwrap().to_string_lossy().as_ref()),
        "child should run in the pinned cwd, got: {printed}"
    );
    let _ = fs::remove_dir_all(dir);
}

#[cfg(not(windows))]
#[test]
fn a_script_cannot_chdir_the_host_process() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("chdir-denied");
    let target = dir.join("target");
    fs::create_dir_all(&target).unwrap();
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        format!(
            "let outcome = 'allowed';\n\
             try {{ Deno.chdir({:?}); }} catch (error) {{ outcome = error.name; }}\n\
             console.log(`chdir=${{outcome}}`);",
            target.display().to_string()
        ),
    )
    .unwrap();
    let host_before = std::env::current_dir().unwrap();
    let options = LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        capture_stdout: true,
        ..Default::default()
    };
    let output = libdeno::run_with_output(&entry, &options).unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("chdir=NotSupported"),
        "Deno.chdir must be refused, got: {stdout}"
    );
    // The whole point: the host's working directory is untouched, so the
    // host's own relative paths and concurrent runs still resolve normally.
    assert_eq!(
        std::env::current_dir().unwrap(),
        host_before,
        "the script must not be able to change the host's working directory"
    );
    let _ = fs::remove_dir_all(dir);
}

/// `cwd` is the base for relative permission paths.
///
/// `docs/api.md` says `cwd` is the resolution base for "relative entry paths,
/// permission paths, and `node_modules` discovery". The permission half is the
/// easy one to lose: a `--allow-read=.` that stops tracking `cwd` would either
/// widen a grant to the host's directory or deny a script that was granted its
/// own tree.
///
/// Subprocess mode: this binary's other tests hold the process-global
/// in-process capture lease in parallel.
#[test]
fn relative_permission_paths_resolve_against_cwd() {
    let root = std::env::temp_dir().canonicalize().unwrap();
    let dir = root.join(format!("libdeno-cwdgrant-{}", std::process::id()));
    let inside = dir.join("inside");
    let outside = dir.join("outside");
    fs::create_dir_all(&inside).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("data.txt"), "outside-content").unwrap();
    fs::write(
        inside.join("main.js"),
        format!(
            "try {{ console.log('READ ' + Deno.readTextFileSync({target:?})); }}\n\
             catch {{ console.log('DENIED'); }}\n",
            target = outside.join("data.txt").to_string_lossy()
        ),
    )
    .unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));

    let run_with = |cwd: PathBuf| -> String {
        let output = libdeno::run_in_subprocess_with_output(
            inside.join("main.js"),
            &LibdenoOptions {
                permissions: vec![
                    "--allow-read=.".to_string(),
                    format!("--allow-read={}", inside.display()),
                ],
                cwd: Some(cwd),
                capture_stdout: true,
                ..Default::default()
            },
        )
        .expect("the run must succeed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };

    // `cwd = outside`, grant `.` — the grant covers `outside`, so the read works
    // even though the entry lives in `inside` (granted absolutely).
    let granted = run_with(outside.clone());
    // `cwd = inside`, grant `.` — now `.` is `inside`, so `outside` is denied.
    let denied = run_with(inside.clone());
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        granted, "READ outside-content",
        "a `.` grant must follow cwd, not the entry's directory"
    );
    assert_eq!(
        denied, "DENIED",
        "the same grant with another cwd must not cover the sibling directory"
    );
}

/// A script's own relative paths do not follow `options.cwd` in-process.
///
/// `docs/api.md` says in-process runs do not change the host cwd. The
/// consequence is sharper than "`Deno.cwd()` reports the host directory": a
/// script that opens `local.txt` gets the *host* file in-process and the
/// `cwd`-pinned file in a subprocess, with identical options. A host that
/// pins `cwd` for isolation and grants `.` therefore grants one directory
/// while the script reads another.
///
/// The answer is written to a file, not stdout: in-process capture redirects
/// the process-global fd 1, so a parallel test's harness output would land in
/// the captured buffer.
#[test]
fn script_relative_paths_follow_the_process_cwd_in_process_only() {
    // This test changes the process cwd, which is as process-global as the
    // capture lease; reuse that lock so the two cannot overlap.
    let _guard = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Canonicalize: `temp_dir` is `/var/...` on macOS, a symlink to
    // `/private/var`, and a grant on the symlink does not match the path the
    // resolver canonicalizes to.
    let dir = plain(temp_dir("script-relative").canonicalize().unwrap());
    let host = dir.join("host");
    let pinned = dir.join("pinned");
    fs::create_dir_all(&host).unwrap();
    fs::create_dir_all(&pinned).unwrap();
    fs::write(host.join("local.txt"), "from-process-cwd").unwrap();
    fs::write(pinned.join("local.txt"), "from-options-cwd").unwrap();
    std::env::set_var("LIBDENO_HOST_EXE", env!("CARGO_BIN_EXE_child_host"));

    let script = |marker: &std::path::Path| {
        format!(
            "Deno.writeTextFileSync({:?}, Deno.readTextFileSync('local.txt').trim());",
            marker.display().to_string()
        )
    };
    let options = || LibdenoOptions {
        permissions: vec![
            format!("--allow-read={}", host.display()),
            format!("--allow-read={}", pinned.display()),
            format!("--allow-write={}", dir.display()),
        ],
        cwd: Some(pinned.clone()),
        ..Default::default()
    };
    let run = |backend: &str| -> String {
        let marker = dir.join(format!("{backend}.txt"));
        fs::write(pinned.join("app.js"), script(&marker)).unwrap();
        match backend {
            "in-process" => libdeno::run("app.js", &options()).map(|_| ()),
            _ => libdeno::run_in_subprocess("app.js", &options()).map(|_| ()),
        }
        .expect("the run must succeed");
        fs::read_to_string(&marker).unwrap().trim().to_string()
    };

    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(&host).unwrap();
    let in_process = run("in-process");
    let subprocess = run("subprocess");
    std::env::set_current_dir(previous).unwrap();
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(
        in_process, "from-process-cwd",
        "in-process: `cwd` is a resolution base, not a chdir, so the script's own \
         relative paths still resolve against the host process directory"
    );
    assert_eq!(
        subprocess, "from-options-cwd",
        "subprocess: the child really runs in `options.cwd`, so the same script \
         reads a different file"
    );
}

/// The in-memory source entries apply the same `cwd` rule as the path ones.
///
/// `run_source` / `run_source_with_output` resolve `base_dir` through
/// `effective_cwd(options)`, so a bad `cwd` is rejected before the virtual
/// entry is built — but only the path entry points had a test for it. A
/// regression that moved the check after `entry_source_memory` would let the
/// virtual entry resolve against the host cwd first, which is exactly the
/// fallback the option exists to prevent.
#[cfg(not(windows))]
#[test]
fn in_memory_source_entries_reject_an_unusable_cwd() {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("source-cwd");
    let ghost = dir.join("does-not-exist");
    let file = dir.join("afile");
    fs::write(&file, "x").unwrap();
    let options = |cwd: PathBuf| LibdenoOptions {
        allow_all_permissions: true,
        cwd: Some(cwd),
        capture_stdout: true,
        ..Default::default()
    };

    for (bad, needle) in [
        (ghost.clone(), "is not a usable directory"),
        (file.clone(), "is not a directory"),
    ] {
        assert_unusable(
            libdeno::run_source(
                "console.log(1);",
                libdeno::SourceLang::JavaScript,
                &dir,
                &options(bad.clone()),
            ),
            needle,
        );
        assert_unusable(
            libdeno::run_source_with_output(
                "console.log(1);",
                libdeno::SourceLang::JavaScript,
                &dir,
                &options(bad),
            ),
            needle,
        );
    }
    let _ = fs::remove_dir_all(dir);
}
