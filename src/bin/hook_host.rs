//! Test host: installs a process-global permission hook before servicing
//! child-mode requests, so smoke tests can exercise the broker path end to
//! end. LIBDENO_TEST_HOOK_DENY=1 makes the hook deny everything.
use std::sync::Arc;

fn main() {
    let deny = std::env::var("LIBDENO_TEST_HOOK_DENY").as_deref() == Ok("1");
    libdeno::install_permission_hook(Arc::new(move |_req| !deny)).unwrap();
    libdeno::maybe_handle_child_mode();
    eprintln!("hook_host: not a child-mode process; nothing to do");
    std::process::exit(2);
}
