//! Minimal test host for subprocess requests and `child_process.fork`.
//!
//! Integration tests point `LIBDENO_HOST_EXE` at this binary so
//! `run_in_subprocess` spawns a real host (the test harness binary itself is
//! not a host and does not call `maybe_handle_child_mode`).

use std::sync::OnceLock;
use std::time::Instant;

fn main() {
    let protocol_child = std::env::var_os("LIBDENO_CHILD_MODE").is_some()
        || std::env::var_os("LIBDENO_SUPERVISOR_MODE").is_some();
    if protocol_child && std::env::var_os("NODE_CHANNEL_FD").is_some() {
        eprintln!("child_host: NODE_CHANNEL_FD leaked into ordinary child mode");
        std::process::exit(1);
    }
    if protocol_child && std::env::var_os("NODE_CHANNEL_SERIALIZATION_MODE").is_some() {
        eprintln!("child_host: NODE_CHANNEL_SERIALIZATION_MODE leaked into ordinary child mode");
        std::process::exit(1);
    }
    #[cfg(unix)]
    if protocol_child && std::env::var_os("LIBDENO_TEST_IPC_PROBE").is_some() {
        if let Err(error) = verify_child_ipc_fd_identity() {
            eprintln!("child_host IPC boundary probe failed: {error}");
            std::process::exit(1);
        }
    }
    // SAFETY: this test host is launched only by the trusted libdeno test harness.
    unsafe { libdeno::initialize_node_ipc_from_env() }.unwrap();
    // Test hook: emit startup output before the protocol request is read, so a
    // parent that writes the request before draining the pipes deadlocks on a
    // full pipe buffer.
    if let Some(bytes) = std::env::var_os("LIBDENO_TEST_STARTUP_OUTPUT")
        .and_then(|value| value.to_str().and_then(|value| value.parse::<usize>().ok()))
    {
        use std::io::Write;
        let block = vec![b'x'; 8192];
        let mut stdout = std::io::stdout();
        let mut written = 0;
        while written < bytes {
            let take = block.len().min(bytes - written);
            if stdout.write_all(&block[..take]).is_err() {
                break;
            }
            written += take;
        }
        let _ = stdout.flush();
    }
    libdeno::maybe_handle_child_mode();
    #[cfg(feature = "execution-control")]
    libdeno::maybe_handle_supervisor_mode();

    // `child_process.fork` launches the configured exec path in normal mode
    // with deno-style `run [flags] <entry> [args]` arguments.
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(entry_index) = args
        .iter()
        .position(|arg| arg != "run" && !arg.starts_with('-'))
    else {
        return;
    };

    let ipc_probe_position = args
        .iter()
        .position(|arg| arg == "--libdeno-test-ipc-spawn-c");
    let ipc_probe = ipc_probe_position.and_then(|position| args.get(position + 1));
    if let Some(probe_entry) = ipc_probe {
        #[cfg(unix)]
        if let Err(error) = run_ipc_spawn_probe(probe_entry) {
            eprintln!("child_host IPC spawn probe failed: {error}");
            std::process::exit(1);
        }
        #[cfg(not(unix))]
        {
            eprintln!("child_host IPC spawn probe requires Unix socket identity checks");
            std::process::exit(1);
        }
    }

    let script_args = match ipc_probe_position {
        Some(position) => args[entry_index + 1..position].to_vec(),
        None => args[entry_index + 1..].to_vec(),
    };
    let options = libdeno::LibdenoOptions {
        allow_all_permissions: true,
        args: script_args,
        ..Default::default()
    };
    if ipc_probe_position.is_some() {
        ipc_trace("B_RUN1_BEGIN");
    }
    let code = match libdeno::run(&args[entry_index], &options) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };
    if ipc_probe_position.is_some() {
        ipc_trace("B_RUN1_END");
    }
    if ipc_probe_position.is_some() && code == 0 {
        // Re-run in the same host while NODE_CHANNEL_FD still contains its
        // stale number. The one-shot channel has been taken, so no IPC returns.
        let second_run = libdeno::LibdenoOptions {
            allow_all_permissions: true,
            args: vec!["--libdeno-test-ipc-second-run".into()],
            execution_deadline: Some(std::time::Duration::from_secs(5)),
            ..Default::default()
        };
        ipc_trace("B_RUN2_BEGIN");
        match libdeno::run(&args[entry_index], &second_run) {
            Ok(code) => {
                ipc_trace("B_RUN2_END");
                std::process::exit(code)
            }
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
    std::process::exit(code);
}

fn ipc_trace(stage: &str) {
    static START: OnceLock<Instant> = OnceLock::new();
    let elapsed_ms = START.get_or_init(Instant::now).elapsed().as_secs_f64() * 1000.0;
    eprintln!(
        "IPC_TRACE pid={} mono_ms={elapsed_ms:.3} {stage}",
        std::process::id()
    );
}

#[cfg(unix)]
fn run_ipc_spawn_probe(entry: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::RawFd;

    let fd: RawFd = std::env::var("NODE_CHANNEL_FD")?.parse()?;
    // Capture the endpoint's kernel identity while B still owns the original
    // fork channel. C receives only these scalar expectations, not the fd.
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    ipc_trace(&format!(
        "B_IPC_IDENTITY fd={fd} dev={} ino={} mode={}",
        stat.st_dev, stat.st_ino, stat.st_mode
    ));
    let probe_values = [
        (
            "LIBDENO_TEST_IPC_PROBE",
            Some(std::ffi::OsString::from("1")),
        ),
        (
            "LIBDENO_TEST_IPC_FD",
            Some(std::ffi::OsString::from(fd.to_string())),
        ),
        (
            "LIBDENO_TEST_IPC_DEV",
            Some(std::ffi::OsString::from(stat.st_dev.to_string())),
        ),
        (
            "LIBDENO_TEST_IPC_INO",
            Some(std::ffi::OsString::from(stat.st_ino.to_string())),
        ),
        (
            "LIBDENO_TEST_IPC_MODE",
            Some(std::ffi::OsString::from(stat.st_mode.to_string())),
        ),
    ];
    let previous: Vec<_> = probe_values
        .iter()
        .map(|(name, _)| (*name, std::env::var_os(name)))
        .collect();
    for (name, value) in &probe_values {
        std::env::set_var(name, value.as_ref().unwrap());
    }
    let options = libdeno::LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        execution_deadline: Some(std::time::Duration::from_secs(15)),
        ..Default::default()
    };
    let result = (|| {
        ipc_trace("B_LEGACY_C_BEGIN");
        let legacy = libdeno::run_in_subprocess_with_output(entry, &options)?;
        ipc_trace("B_LEGACY_C_END");
        if legacy.exit_code != 0
            || !String::from_utf8_lossy(&legacy.stdout).contains("IPC_ENV_CLEAN")
        {
            return Err(format!(
                "legacy child IPC probe failed: exit={}, stdout={:?}, stderr={:?}",
                legacy.exit_code,
                String::from_utf8_lossy(&legacy.stdout),
                String::from_utf8_lossy(&legacy.stderr)
            )
            .into());
        }
        #[cfg(feature = "execution-control")]
        {
            ipc_trace("B_SUPERVISOR_C_BEGIN");
            let supervised = libdeno::run_in_supervised_subprocess(entry, &options)?;
            ipc_trace("B_SUPERVISOR_C_END");
            if supervised.exit_code != 0
                || !String::from_utf8_lossy(&supervised.stdout).contains("IPC_ENV_CLEAN")
            {
                return Err(format!(
                    "supervised child IPC probe failed: exit={}, stdout={:?}, stderr={:?}",
                    supervised.exit_code,
                    String::from_utf8_lossy(&supervised.stdout),
                    String::from_utf8_lossy(&supervised.stderr)
                )
                .into());
            }
        }
        Ok(())
    })();
    for (name, value) in previous {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
    result
}

#[cfg(unix)]
fn verify_child_ipc_fd_identity() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::RawFd;

    let parse = |name: &str| {
        std::env::var(name).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{name}: {error}"))
        })
    };
    let fd_raw = parse("LIBDENO_TEST_IPC_FD")?;
    let dev_raw = parse("LIBDENO_TEST_IPC_DEV")?;
    let ino_raw = parse("LIBDENO_TEST_IPC_INO")?;
    let mode_raw = parse("LIBDENO_TEST_IPC_MODE")?;
    let fd: RawFd = fd_raw.parse().map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid probe fd {fd_raw:?}: {error}"),
        )
    })?;
    let expected_dev = dev_raw;
    let expected_ino: u64 = ino_raw.parse().map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid probe ino {ino_raw:?}: {error}"),
        )
    })?;
    let expected_mode: u32 = mode_raw.parse().map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid probe mode {mode_raw:?}: {error}"),
        )
    })?;
    let mut actual = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(fd, actual.as_mut_ptr()) } == 0 {
        let actual = unsafe { actual.assume_init() };
        let matches_ancestor = actual.st_dev.to_string() == expected_dev
            && actual.st_ino == expected_ino
            && actual.st_mode as u64 == expected_mode as u64;
        if matches_ancestor {
            return Err("ordinary child inherited the ancestor IPC socket object".into());
        }
        ipc_trace("C_NATIVE_FD_DIFFERENT_OBJECT");
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EBADF) {
        ipc_trace("C_NATIVE_FD_CLOSED");
        Ok(())
    } else {
        Err(error.into())
    }
}
