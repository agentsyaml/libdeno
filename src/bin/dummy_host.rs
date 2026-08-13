//! Test host: a host that NEVER services child-mode requests.
//!
//! Deliberately does not call `maybe_handle_child_mode()` and never reads
//! stdin — it just parks for a while. `run_in_subprocess` writes the run
//! request to such a host's stdin; once the payload exceeds the pipe buffer
//! the parent's bounded write must time out (and kill this process) instead
//! of blocking forever.

fn main() {
    std::thread::sleep(std::time::Duration::from_secs(120));
}
