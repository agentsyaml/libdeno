//! Minimal embedded host demonstrating `libdeno::run`.
//!
//! Usage: `cargo run --example demo [--allow-read=...] <entry> [args...]`
//!
//! The binary also serves as the napi symbol carrier for .node native addons
//! (see build.rs). A real host must export the same symbols by calling
//! `deno_napi::print_linker_flags("<host-binary-name>")` in its own build.rs.

use libdeno::{run, LibdenoError, LibdenoOptions};

fn main() {
    // Service `run_in_subprocess` child requests (spawned with
    // LIBDENO_CHILD_MODE=1). In child mode this executes the script and exits
    // with its code, so the host process stays alive on `Deno.exit(n)`.
    // Returns false immediately in a normal host launch.
    libdeno::maybe_handle_child_mode();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("libdeno demo host: run a JS/TS entry (file, dir, or package.json)");
        println!("usage: demo [permission flags...] <entry> [script args...]");
        println!(
            "permission flags: --allow-read[=paths] --allow-write[=paths] --allow-env[=names]"
        );
        println!(
            "  --allow-net[=hosts] --allow-run[=names] --allow-ffi[=paths] --allow-sys[=names]"
        );
        return;
    }

    let entry = args
        .iter()
        .skip(1)
        // `child_process.fork` spawns `demo run -A --unstable-... script.js`
        // (deno-style translated args); skip the `run` subcommand and any flags.
        .find(|a| a.as_str() != "run" && !a.starts_with('-'))
        .cloned()
        .unwrap_or_else(|| ".".to_string());
    let permissions: Vec<String> = args
        .iter()
        .skip(1)
        .filter(|a| a.starts_with("--allow") || *a == "-A")
        .cloned()
        .collect();
    // Script args are the non-flag arguments after the entry; the entry path
    // and permission flags must not leak into process.argv.
    let script_args: Vec<String> = args
        .iter()
        .skip(1)
        .skip_while(|a| a.as_str() == "run" || a.starts_with('-'))
        .skip(1) // the entry itself
        .cloned()
        .collect();

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

fn print_error(e: &LibdenoError) {
    match e {
        LibdenoError::Entry(e) => eprintln!("entry error: {e}"),
        LibdenoError::Permission(e) => eprintln!("permission error: {e}"),
        LibdenoError::Runtime(e) => eprintln!("error: {e}"),
        LibdenoError::Core(e) => eprintln!("runtime error: {e}"),
        LibdenoError::Io(e) => eprintln!("io error: {e}"),
        LibdenoError::Timeout(d) => eprintln!("execution deadline exceeded: {d:?}"),
    }
}
