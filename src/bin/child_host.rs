//! Minimal host binary that services `run_in_subprocess` child requests.
//!
//! Integration tests point `LIBDENO_HOST_EXE` at this binary so
//! `run_in_subprocess` spawns a real host (the test harness binary itself is
//! not a host and does not call `maybe_handle_child_mode`).

fn main() {
    libdeno::maybe_handle_child_mode();
}
