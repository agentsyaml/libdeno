//! Test host: installs an external permission broker before servicing
//! child-mode requests, so smoke tests can exercise the broker path end to
//! end. The broker is an external process listening on the Unix socket named
//! by LIBDENO_TEST_BROKER_PATH; this process is the connector
//! (PermissionBroker::new connects to that socket).
fn main() {
    let path =
        std::env::var("LIBDENO_TEST_BROKER_PATH").expect("LIBDENO_TEST_BROKER_PATH must be set");
    libdeno::install_permission_broker(path).unwrap();
    libdeno::maybe_handle_child_mode();
    eprintln!("broker_host: not a child-mode process; nothing to do");
    std::process::exit(2);
}
