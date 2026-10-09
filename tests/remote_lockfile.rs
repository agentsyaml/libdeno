use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread;

use libdeno::{run_source, LibdenoOptions, SourceLang};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard(Option<std::ffi::OsString>);

impl EnvGuard {
    fn isolated_deno_dir(path: &std::path::Path) -> Self {
        let old = std::env::var_os("DENO_DIR");
        std::env::set_var("DENO_DIR", path);
        Self(old)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.0.take() {
            Some(value) => std::env::set_var("DENO_DIR", value),
            None => std::env::remove_var("DENO_DIR"),
        }
    }
}

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "libdeno-remote-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct LocalModuleServer {
    url: String,
    thread: Option<thread::JoinHandle<()>>,
}

impl LocalModuleServer {
    fn start(body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let thread = thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        serve_module(&mut stream, &body);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url: format!("http://{address}/mod.js"),
            thread: Some(thread),
        }
    }
}

impl Drop for LocalModuleServer {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve_module(stream: &mut TcpStream, body: &str) {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut request = [0; 4096];
    let _ = stream.read(&mut request);
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .unwrap();
}

fn run_import(
    fixture: &Fixture,
    url: &str,
    marker: &std::path::Path,
) -> Result<i32, libdeno::LibdenoError> {
    let source = format!(
        "import {url:?}; Deno.writeTextFileSync({:?}, 'ran');",
        marker.to_string_lossy()
    );
    run_source(
        source,
        SourceLang::JavaScript,
        &fixture.0,
        &LibdenoOptions {
            allow_all_permissions: true,
            ..Default::default()
        },
    )
}

fn lockfile(url: &str, checksum: Option<&str>) -> String {
    let remote = checksum
        .map(|checksum| format!(r#""{url}":"{checksum}""#))
        .unwrap_or_default();
    format!(r#"{{"version":"5","remote":{{{remote}}}}}"#)
}

fn setup_lock(fixture: &Fixture, config: &str, lock: &str) {
    std::fs::write(fixture.0.join("deno.json"), config).unwrap();
    std::fs::write(fixture.0.join("deno.lock"), lock).unwrap();
}

#[test]
fn lockfile_matching_remote_checksum_loads_module() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let _env = EnvGuard::isolated_deno_dir(&fixture.0.join("deno-cache"));
    let body = "export const value = 42;";
    let server = LocalModuleServer::start(body.to_string());
    let checksum = deno_graph::source::LoaderChecksum::r#gen(body.as_bytes());
    setup_lock(
        &fixture,
        r#"{"lock":"deno.lock"}"#,
        &lockfile(&server.url, Some(&checksum)),
    );
    let result = run_import(&fixture, &server.url, &fixture.0.join("marker"));
    assert_eq!(result.unwrap(), 0);
    assert_eq!(std::fs::read(fixture.0.join("marker")).unwrap(), b"ran");
}

#[test]
fn lockfile_wrong_remote_checksum_fails_before_module_executes() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let _env = EnvGuard::isolated_deno_dir(&fixture.0.join("deno-cache"));
    let remote_marker = fixture.0.join("remote-marker");
    let body = format!(
        "Deno.writeTextFileSync({:?}, 'ran'); export {{}};",
        remote_marker.to_string_lossy()
    );
    let wrong_body = "export const different = true;";
    let server = LocalModuleServer::start(body);
    setup_lock(
        &fixture,
        r#"{"lock":"deno.lock"}"#,
        &lockfile(
            &server.url,
            Some(&deno_graph::source::LoaderChecksum::r#gen(
                wrong_body.as_bytes(),
            )),
        ),
    );
    let marker = fixture.0.join("marker");
    let error = run_import(&fixture, &server.url, &marker)
        .expect_err("mismatching valid checksum unexpectedly loaded");
    let error = format!("{error:#}").to_lowercase();
    assert!(
        error.contains("checksum") || error.contains("integrity"),
        "expected a checksum/integrity failure, got: {error}"
    );
    assert!(!marker.exists());
    assert!(!remote_marker.exists());
}

#[test]
fn frozen_lockfile_rejects_new_remote_without_changing_lock_bytes() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let _env = EnvGuard::isolated_deno_dir(&fixture.0.join("deno-cache"));
    let body = "export const value = 42;";
    let server = LocalModuleServer::start(body.to_string());
    let original = lockfile(&server.url, None);
    setup_lock(
        &fixture,
        r#"{"lock":{"path":"deno.lock","frozen":true}}"#,
        &original,
    );
    let result = run_import(&fixture, &server.url, &fixture.0.join("marker"));
    let error = result.expect_err("frozen lockfile accepted a new remote dependency");
    let error = format!("{error:#}").to_lowercase();
    assert!(
        error.contains("lockfile") && (error.contains("out of date") || error.contains("frozen")),
        "expected a frozen/out-of-date lockfile error, got: {error}"
    );
    assert!(!fixture.0.join("marker").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("deno.lock")).unwrap(),
        original
    );
}

#[test]
fn lock_false_does_not_apply_remote_checksum_gate() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let _env = EnvGuard::isolated_deno_dir(&fixture.0.join("deno-cache"));
    let body = "export const value = 42;";
    let server = LocalModuleServer::start(body.to_string());
    let mismatching = deno_graph::source::LoaderChecksum::r#gen(b"different module body");
    setup_lock(
        &fixture,
        r#"{"lock":false}"#,
        &lockfile(&server.url, Some(&mismatching)),
    );
    assert_eq!(
        run_import(&fixture, &server.url, &fixture.0.join("marker")).unwrap(),
        0
    );
    assert_eq!(std::fs::read(fixture.0.join("marker")).unwrap(), b"ran");
}
