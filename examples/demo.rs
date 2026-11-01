//! Minimal embedded host demonstrating `libdeno::run`.
//!
//! Usage: `cargo run --example demo [--allow-read=...] <entry> [args...]`
//!
//! The binary also serves as the napi symbol carrier for .node native addons
//! (see build.rs). A real host must export the same symbols by calling
//! `deno_napi::print_linker_flags("<host-binary-name>")` in its own build.rs.

use libdeno::{run, LibdenoError, LibdenoOptions};

fn main() {
    // SAFETY: a host launching this binary must exclusively transfer its IPC handle.
    unsafe { libdeno::initialize_node_ipc_from_env() }.unwrap();
    // Service `run_in_subprocess` child requests (spawned with
    // LIBDENO_CHILD_MODE=1). In child mode this executes the script and exits
    // with its code, so the host process stays alive on `Deno.exit(n)`.
    // Returns false immediately in a normal host launch.
    libdeno::maybe_handle_child_mode();

    let args: Vec<String> = std::env::args().collect();
    // Only the leading host flags count: `demo app.js --help` passes `--help`
    // to the script, the way `deno run app.js --help` does.
    let is_host_flag = |a: &&String| a.as_str() == "run" || a.starts_with('-');
    if args
        .iter()
        .skip(1)
        .take_while(is_host_flag)
        .any(|a| a == "--help" || a == "-h")
    {
        println!("libdeno demo host: run a JS/TS entry (file, dir, or package.json)");
        println!("usage: demo [permission flags...] <entry> [script args...]");
        println!(
            "permission flags: --allow-read[=paths] --allow-write[=paths] --allow-env[=names]"
        );
        println!(
            "  --allow-net[=hosts] --allow-import[=hosts] --allow-run[=names] --allow-ffi[=paths] --allow-sys[=names]"
        );
        return;
    }

    // Args before the entry are host flags. Anything unrecognized is an error,
    // not something to skip: `--deny-read=/etc`, `-R`, or a typo like
    // `--alow-read=/etc` would otherwise leave `permissions` empty, and the
    // allow-all fallback below would run the script with every capability.
    let (entry, permissions, script_args) = match split_args(&args) {
        Ok(parsed) => parsed,
        Err(bad) => {
            eprintln!("unknown flag: {bad} (supported: --allow-*, -A)");
            std::process::exit(2);
        }
    };

    // Since v0.2.0 an empty permission list is a construction error, not
    // allow-all. Grant everything only when the user passed no --allow-*
    // flags; explicit flags keep their restrictive meaning.
    let allow_all = permissions.is_empty();
    let options = LibdenoOptions {
        permissions,
        allow_all_permissions: allow_all,
        args: script_args,
        cwd: None,
        ..Default::default()
    };
    match run(&entry, &options) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            print_error(&e);
            std::process::exit(1);
        }
    }
}

/// Splits argv into `(entry, permission flags, script args)`.
///
/// Errors on any flag it does not recognize, because the caller treats "no
/// permission flags" as allow-all: silently skipping `--deny-read=/etc` would
/// invert the user's intent into a full grant.
fn split_args(args: &[String]) -> Result<(String, Vec<String>, Vec<String>), String> {
    let mut entry = None;
    let mut permissions = Vec::new();
    let mut script_args = Vec::new();
    for arg in args.iter().skip(1) {
        let is_flag = arg.starts_with('-');
        if entry.is_none() && (arg.as_str() == "run" || is_flag) {
            // `child_process.fork` spawns `demo run -A --unstable-... script.js`
            // (deno-style translated args): `run` and the unstable bootstrap
            // flags are accepted and ignored.
            if arg.as_str() != "run" && !arg.starts_with("--unstable-") {
                if arg.starts_with("--allow") || arg.starts_with("-A") {
                    // Forwarded as-is so libdeno, not this parser, reports a
                    // bad value (`-A=1`, `--allow-read=`).
                    permissions.push(arg.clone());
                } else {
                    return Err(arg.clone());
                }
            }
        } else if entry.is_none() {
            entry = Some(arg.clone());
        } else {
            script_args.push(arg.clone());
        }
    }
    Ok((
        entry.unwrap_or_else(|| ".".to_string()),
        permissions,
        script_args,
    ))
}

fn print_error(e: &LibdenoError) {
    // Branch on the variant, never on the message: a script can forge any text.
    // `is_permission_error()` is the only trusted denial signal, and it is
    // separate from the variant because a denial raised inside the script
    // arrives as `Core`, not `Permission`.
    let kind = if e.is_permission_error() {
        "permission denied by the host"
    } else {
        "script or host failure"
    };
    match e {
        LibdenoError::Entry(e) => eprintln!("entry error: {e}"),
        LibdenoError::Permission(e) => eprintln!("permission error: {e}"),
        LibdenoError::Configuration(e) => eprintln!("configuration error: {e}"),
        LibdenoError::Runtime(e) => eprintln!("runtime error ({kind}): {e}"),
        LibdenoError::Core(e) => eprintln!("core error ({}): {e}", core_kind(&e.0)),
        LibdenoError::Io(e) => eprintln!("io error: {e}"),
        LibdenoError::Timeout(msg) => eprintln!("timeout: {msg}"),
    }
}

/// `LibdenoError::Core` carries deno_core's own classification. Matching on it
/// beats parsing the message: a host can tell "the script threw" from "a module
/// would not load" without depending on upstream wording. The variants belong
/// to deno_core, so an upgrade can move a case between them — widen the wildcard
/// arm rather than assuming it never fires.
fn core_kind(kind: &libdeno::deno_core::error::CoreErrorKind) -> &'static str {
    use libdeno::deno_core::error::CoreErrorKind;
    match kind {
        CoreErrorKind::Js(_) => "script threw",
        CoreErrorKind::JsBox(_) => "module load or host-side failure",
        CoreErrorKind::ExecutionTerminated => "execution terminated",
        _ => "other",
    }
}

#[cfg(test)]
mod host_flag_tests {
    use super::split_args;

    fn argv(args: &[&str]) -> Vec<String> {
        // Position 0 is the program name; `split_args` skips it.
        let mut all = vec!["demo".to_string()];
        all.extend(args.iter().map(|s| s.to_string()));
        all
    }

    #[test]
    fn unknown_flags_are_rejected_not_skipped() {
        // Skipping these would leave the permission list empty, and an empty
        // list means allow-all in this host.
        for bad in ["--deny-read=/etc", "-R", "--alow-read=/etc"] {
            let args = argv(&[bad, "app.js"]);
            assert_eq!(
                split_args(&args).unwrap_err(),
                bad,
                "{bad} must not be silently dropped"
            );
        }
    }

    #[test]
    fn allow_flags_reach_libdeno_unparsed() {
        // Malformed values are libdeno's to report, not this parser's.
        let args = argv(&["-A=1", "--allow-read=", "app.js"]);
        let (entry, permissions, script_args) = split_args(&args).unwrap();
        assert_eq!(entry, "app.js");
        assert_eq!(permissions, vec!["-A=1", "--allow-read="]);
        assert!(script_args.is_empty());
    }

    #[test]
    fn fork_translated_args_are_tolerated() {
        let args = argv(&[
            "run",
            "-A",
            "--unstable-bare-node-builtins",
            "app.js",
            "extra",
        ]);
        let (entry, permissions, script_args) = split_args(&args).unwrap();
        assert_eq!(entry, "app.js");
        assert_eq!(permissions, vec!["-A"]);
        assert_eq!(script_args, vec!["extra"]);
    }

    #[test]
    fn flags_after_the_entry_are_script_args() {
        let args = argv(&["app.js", "--flag-for-the-script"]);
        let (entry, permissions, script_args) = split_args(&args).unwrap();
        assert_eq!(entry, "app.js");
        assert!(permissions.is_empty());
        assert_eq!(script_args, vec!["--flag-for-the-script"]);
    }

    #[test]
    fn no_entry_falls_back_to_the_current_directory() {
        let (entry, permissions, script_args) = split_args(&argv(&["-A"])).unwrap();
        assert_eq!(entry, ".");
        assert_eq!(permissions, vec!["-A"]);
        assert!(script_args.is_empty());
    }
}
