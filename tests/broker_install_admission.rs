//! Installing a permission broker or hook while a run is active is refused.
//!
//! `docs/api.md` states it twice: installation "returns
//! `LibdenoError::Configuration` immediately if a run or worker is active", and
//! the reverse direction is refused too. Without the guard the installer would
//! hand the process a new permission authority in the middle of a run, so some
//! checks in that run would answer to the old authority and some to the new one.
//!
//! This lives in its own test binary on purpose: installation is process-global
//! and install-once, so it must not race another test that installs one. A
//! refused install does not consume the once — the guard runs before
//! `set_broker` — but the cleaner isolation still helps.
//!
//! The tests below still run one at a time: the guard is a process-wide lease,
//! and two concurrent attempts make each one see the *other* attempt's lease
//! rather than the run's. The failure then reads "the run never started",
//! because the two runs starve each other instead of the install being refused
//! for the documented reason.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use libdeno::{LibdenoError, LibdenoOptions};

/// Serializes the cases below; the lease each one exercises is process-global.
static ADMISSION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn admission_lock() -> std::sync::MutexGuard<'static, ()> {
    ADMISSION_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("libdeno-brokeradmit-{}-{name}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn options() -> LibdenoOptions {
    LibdenoOptions {
        allow_all_permissions: true,
        ..Default::default()
    }
}

/// Runs a script that blocks until `release` exists, calls `install` while it
/// is running, and asserts the refusal. Returns the run's exit code.
fn install_while_running(
    name: &str,
    install: impl FnOnce(&std::path::Path) -> Result<(), LibdenoError>,
) -> i32 {
    let _serial = admission_lock();
    let dir = temp_dir(name);
    let started = dir.join("started");
    let release = dir.join("release");
    let entry = dir.join("main.js");
    fs::write(
        &entry,
        format!(
            "Deno.writeTextFileSync({started:?}, '1');\n\
             const until = Date.now() + 60_000;\n\
             while (Date.now() < until) {{\n\
             \x20 try {{ Deno.statSync({release:?}); Deno.exit(0); }} catch (_) {{}}\n\
             }}\n\
             Deno.exit(9);\n"
        ),
    )
    .unwrap();

    let run = {
        let entry = entry.clone();
        std::thread::spawn(move || libdeno::run(&entry, &options()))
    };
    // A watchdog, not a latency assertion: isolate bootstrap under load can take
    // far longer than the run itself, so the window only has to be wide enough
    // that "the run never started" means it really never will.
    let deadline = Instant::now() + Duration::from_secs(60);
    while !started.exists() {
        assert!(Instant::now() < deadline, "the run never started");
        std::thread::sleep(Duration::from_millis(10));
    }

    // The path does not need to exist: the admission guard must reject before
    // anything tries to connect.
    let error = install(&dir.join("no-such-broker.sock"))
        .expect_err("installing during an active run must be refused");

    fs::write(&release, "1").unwrap();
    let code = run.join().unwrap().unwrap();
    let _ = fs::remove_dir_all(dir);

    match error {
        LibdenoError::Configuration(message) => assert!(
            message.contains("no active runs or workers"),
            "expected the documented admission message, got: {message}"
        ),
        other => panic!("expected Configuration, got: {other:?}"),
    }
    code
}

#[test]
fn a_broker_cannot_be_installed_while_a_run_is_active() {
    let code = install_while_running("broker", |path| libdeno::install_permission_broker(path));
    assert_eq!(code, 0, "the run must finish normally once released");
}

#[cfg(unix)]
#[test]
fn a_hook_cannot_be_installed_while_a_run_is_active() {
    let code = install_while_running("hook", |_| {
        libdeno::install_permission_hook(std::sync::Arc::new(|_| true))
    });
    assert_eq!(code, 0, "the run must finish normally once released");
}
