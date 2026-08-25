//! Minimal test host for subprocess requests and `child_process.fork`.
//!
//! Integration tests point `LIBDENO_HOST_EXE` at this binary so
//! `run_in_subprocess` spawns a real host (the test harness binary itself is
//! not a host and does not call `maybe_handle_child_mode`).

fn main() {
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
    let options = libdeno::LibdenoOptions {
        allow_all_permissions: true,
        args: args[entry_index + 1..].to_vec(),
        ..Default::default()
    };
    match libdeno::run(&args[entry_index], &options) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
