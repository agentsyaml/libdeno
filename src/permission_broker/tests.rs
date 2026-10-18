use super::*;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

struct TestTempDir(PathBuf);

impl TestTempDir {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!("libdeno-{label}-{}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run_in_isolated_test(name: &str, mode: &str, child: impl FnOnce()) {
    const CHILD: &str = "LIBDENO_PERMISSION_BROKER_TEST_CHILD";
    if std::env::var(CHILD).ok().as_deref() == Some(mode) {
        child();
        return;
    }
    let mut process = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, mode)
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        if let Some(status) = process.try_wait().unwrap() {
            assert!(status.success(), "isolated test failed: {status}");
            return;
        }
        if std::time::Instant::now() >= deadline {
            let _ = process.kill();
            let _ = process.wait();
            panic!("isolated permission test timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn request(id: u32, value: &str) -> String {
    format!(r#"{{"v":1,"pid":2,"id":{id},"datetime":"now","permission":"env","value":"{value}"}}"#)
}

#[test]
fn hook_socket_directory_guard_cleans_bind_failure() {
    use std::os::unix::fs::DirBuilderExt;
    let dir = std::env::temp_dir().join(format!("libdeno-hook-cleanup-{}", std::process::id()));
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700).create(&dir).unwrap();
    let socket = dir.join("x".repeat(256));
    let guard = HookSocketDir {
        dir: dir.clone(),
        socket: socket.clone(),
    };
    assert!(std::os::unix::net::UnixListener::bind(&socket).is_err());
    drop(guard);
    assert!(!dir.exists());
}

fn round_trip(hook: PermissionPrompt) -> (u32, String) {
    let (mut client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    client
        .set_write_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    let bridge = std::thread::spawn(move || serve_broker_connection(server, &hook));
    client
        .write_all(format!("{}\n", request(7, "value")).as_bytes())
        .unwrap();
    let mut reader = BufReader::new(&client);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    drop(reader);
    drop(client);
    bridge.join().unwrap();
    let response: deno_core::serde_json::Value =
        deno_core::serde_json::from_str(line.trim()).unwrap();
    (
        response["id"].as_u64().unwrap() as u32,
        response["result"].as_str().unwrap().to_string(),
    )
}

#[test]
fn hook_bridge_allows_and_denies() {
    assert_eq!(round_trip(Arc::new(|_| true)), (7, "allow".to_string()));
    assert_eq!(round_trip(Arc::new(|_| false)), (7, "deny".to_string()));
}

#[test]
fn panicking_hook_fails_closed_as_deny() {
    assert_eq!(
        round_trip(Arc::new(|_| -> bool { panic!("synthetic hook panic") })),
        (7, "deny".to_string())
    );
}

#[test]
fn uncommitted_thread_guard_closes_and_joins_without_running_hook() {
    let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let thread_called = called.clone();
    let (start, wait) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        if wait.recv().is_ok() {
            thread_called.store(true, Ordering::SeqCst);
        }
    });
    drop(BrokerThreadGuard {
        start: Some(start),
        thread: Some(thread),
    });
    assert!(!called.load(Ordering::SeqCst));
}

#[test]
fn oversized_request_is_denied_and_next_request_is_served() {
    let (mut client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    client
        .set_write_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = calls.clone();
    let hook: PermissionPrompt = Arc::new(move |_| {
        hook_calls.fetch_add(1, Ordering::SeqCst);
        true
    });
    let bridge = std::thread::spawn(move || serve_broker_connection(server, &hook));
    let huge = request(
        7,
        &format!(
            "{}\\n fake \\\"id\\\":88",
            "x".repeat(MAX_BROKER_LINE_BYTES)
        ),
    );
    client
        .write_all(format!("{huge}\n{}\n", request(8, "ok")).as_bytes())
        .unwrap();
    let mut reader = BufReader::new(&client);
    let mut lines = [String::new(), String::new()];
    for line in &mut lines {
        reader.read_line(line).unwrap();
    }
    let a: deno_core::serde_json::Value = deno_core::serde_json::from_str(lines[0].trim()).unwrap();
    let b: deno_core::serde_json::Value = deno_core::serde_json::from_str(lines[1].trim()).unwrap();
    assert_eq!(
        (a["id"].as_u64(), a["result"].as_str()),
        (Some(7), Some("deny"))
    );
    assert_eq!(
        (b["id"].as_u64(), b["result"].as_str()),
        (Some(8), Some("allow"))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    drop(reader);
    drop(client);
    bridge.join().unwrap();
}

#[test]
fn broker_line_boundary_and_id_parsing() {
    use std::io::Cursor;
    let exact = format!(
        "{}{}\n",
        request(u32::MAX, ""),
        " ".repeat(MAX_BROKER_LINE_BYTES - request(u32::MAX, "").len() - 1)
    );
    assert_eq!(exact.len(), MAX_BROKER_LINE_BYTES);
    assert!(matches!(
        read_broker_line(&mut Cursor::new(exact.as_bytes())).unwrap(),
        Some(BrokerLine::Request(_))
    ));
    let over = format!(
        "{}{}\n",
        request(u32::MAX, ""),
        " ".repeat(MAX_BROKER_LINE_BYTES - request(u32::MAX, "").len())
    );
    assert!(matches!(
        read_broker_line(&mut Cursor::new(over.as_bytes())).unwrap(),
        Some(BrokerLine::Oversized(Some(u32::MAX)))
    ));
    assert_eq!(
        broker_request_id(br#"{"id":7,"value":"line\n fake \"id\":9"}"#, false),
        Some(7)
    );
    assert_eq!(
        broker_request_id(br#"{"id":7,"value":"truncated""#, true),
        Some(7)
    );
    assert_eq!(broker_request_id(br#"{"id":7} garbage"#, false), None);
    assert_eq!(broker_request_id(br#"{"id":7,"value":!}"#, true), None);
    assert_eq!(
        broker_request_id(br#"{"id":7,"value":"truncated""#, false),
        None
    );
    assert_eq!(
        broker_request_id(br#"{"id":7,"id":8,"value":"x"}"#, true),
        None
    );
    assert_eq!(broker_request_id(br#"{"id":"7"}"#, true), None);
    assert_eq!(broker_request_id(br#"{"id":4294967296}"#, true), None);
    assert_eq!(broker_request_id(br#"{"value":{"id":7}}"#, true), None);
    assert!(read_broker_line(&mut Cursor::new(b"{}".as_slice())).is_err());
}

#[test]
fn failed_hook_start_can_retry_install() {
    run_in_isolated_test(
        "permission_broker::tests::failed_hook_start_can_retry_install",
        "retry",
        || {
            let calls = Arc::new(AtomicUsize::new(0));
            let failed_calls = calls.clone();
            FAIL_HOOK_THREAD_SPAWN.store(true, Ordering::SeqCst);
            assert!(install_permission_hook(Arc::new(move |_| {
                failed_calls.fetch_add(1, Ordering::SeqCst);
                false
            }))
            .is_err());
            assert!(!has_broker());
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            let accepted = calls.clone();
            install_permission_hook(Arc::new(move |_| {
                accepted.fetch_add(1, Ordering::SeqCst);
                true
            }))
            .unwrap();
            let dir = TestTempDir::new("permission-retry");
            let entry = dir.0.join("main.js");
            std::fs::write(&entry, "Deno.env.get('LIBDENO_PERMISSION_RETRY');").unwrap();
            crate::run(
                &entry,
                &crate::LibdenoOptions {
                    allow_all_permissions: true,
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(calls.load(Ordering::SeqCst) > 0);
        },
    );
}

fn run_install_case(mode: &str) {
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;
    use std::sync::Barrier;
    let dir = TestTempDir::new("permission-race");
    let path = dir.0.join("broker.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_calls = Arc::new(AtomicUsize::new(0));
    let server_stop = stop.clone();
    let served = server_calls.clone();
    let server = std::thread::spawn(move || loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                let timeout = Some(std::time::Duration::from_secs(60));
                stream.set_read_timeout(timeout).unwrap();
                stream.set_write_timeout(timeout).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                    let request: deno_core::serde_json::Value =
                        deno_core::serde_json::from_str(line.trim()).unwrap();
                    served.fetch_add(1, Ordering::SeqCst);
                    writeln!(stream, "{{\"id\":{},\"result\":\"allow\"}}", request["id"]).unwrap();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if server_stop.load(Ordering::SeqCst) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            Err(error) => panic!("broker accept failed: {error}"),
        }
    });
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(3));
    let hook_counter = hook_calls.clone();
    let hook_call = move || {
        let calls = hook_counter.clone();
        install_permission_hook(Arc::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            true
        }))
    };
    let (hook_result, broker_result) = match mode {
        "hook-first" => {
            let winner = hook_call();
            let loser = std::thread::spawn(move || install_permission_broker(path));
            (winner, loser.join().unwrap())
        }
        "broker-first" => {
            let winner = install_permission_broker(&path);
            let loser = std::thread::spawn(hook_call);
            (loser.join().unwrap(), winner)
        }
        _ => {
            let hook_barrier = barrier.clone();
            let hook = std::thread::spawn(move || {
                hook_barrier.wait();
                hook_call()
            });
            let broker_barrier = barrier.clone();
            let broker = std::thread::spawn(move || {
                broker_barrier.wait();
                install_permission_broker(path)
            });
            barrier.wait();
            (hook.join().unwrap(), broker.join().unwrap())
        }
    };
    assert_ne!(hook_result.is_ok(), broker_result.is_ok());
    assert!(hook_result.is_ok() || matches!(&hook_result, Err(LibdenoError::Permission(_))));
    assert!(broker_result.is_ok() || matches!(&broker_result, Err(LibdenoError::Permission(_))));
    let entry = dir.0.join("main.js");
    std::fs::write(&entry, "Deno.env.get('LIBDENO_PERMISSION_RACE');").unwrap();
    crate::run(
        &entry,
        &crate::LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
    .unwrap();
    if hook_result.is_ok() {
        assert!(hook_calls.load(Ordering::SeqCst) > 0);
        assert_eq!(server_calls.load(Ordering::SeqCst), 0);
        stop.store(true, Ordering::SeqCst);
        server.join().unwrap();
    } else {
        assert!(server_calls.load(Ordering::SeqCst) > 0);
    }
    assert_eq!(
        broker_result.is_ok(),
        server_calls.load(Ordering::SeqCst) > 0
    );
}

#[test]
fn hook_first_install_wins_in_isolated_process() {
    run_in_isolated_test(
        "permission_broker::tests::hook_first_install_wins_in_isolated_process",
        "hook-first",
        || run_install_case("hook-first"),
    );
}

#[test]
fn broker_first_install_wins_in_isolated_process() {
    run_in_isolated_test(
        "permission_broker::tests::broker_first_install_wins_in_isolated_process",
        "broker-first",
        || run_install_case("broker-first"),
    );
}

#[test]
fn concurrent_install_apis_are_exclusive_and_usable() {
    run_in_isolated_test(
        "permission_broker::tests::concurrent_install_apis_are_exclusive_and_usable",
        "race",
        || {
            run_install_case("race");
        },
    );
}
