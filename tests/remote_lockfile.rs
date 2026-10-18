use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    stop: mpsc::Sender<()>,
    thread: Option<thread::JoinHandle<()>>,
    stats: Arc<Mutex<ServerStats>>,
}

#[derive(Default)]
struct ServerStats {
    accepted: usize,
    requests: usize,
    responses: usize,
    errors: Vec<String>,
}

impl LocalModuleServer {
    fn start(body: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stop_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let stats = Arc::new(Mutex::new(ServerStats::default()));
        let thread_stats = stats.clone();
        let thread = thread::spawn(move || {
            let _ = ready_tx.send(());
            loop {
                match stop_rx.recv_timeout(Duration::from_millis(10)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        thread_stats
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .accepted += 1;
                        match serve_module(&mut stream, &body, &thread_stats) {
                            Ok(()) => {}
                            Err(error) => thread_stats
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .errors
                                .push(error),
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("local module server did not start");
        Self {
            url: format!("http://{address}/mod.js"),
            stop,
            thread: Some(thread),
            stats,
        }
    }

    fn diagnostics(&self) -> String {
        let stats = self.stats.lock().unwrap_or_else(|error| error.into_inner());
        format!(
            "accepted={}, requests={}, responses={}, errors={:?}",
            stats.accepted, stats.requests, stats.responses, stats.errors
        )
    }

    fn response_count(&self) -> usize {
        self.stats
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .responses
    }
}

impl Drop for LocalModuleServer {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

const MAX_REQUEST_HEADER_BYTES: usize = 16 * 1024;

fn read_request_headers(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            request.truncate(end + 4);
            return Ok(request);
        }
        if request.len() == MAX_REQUEST_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP request headers exceeded the size limit",
            ));
        }
        let read_capacity = (MAX_REQUEST_HEADER_BYTES - request.len()).min(buffer.len());
        let count = reader.read(&mut buffer[..read_capacity])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before HTTP request headers completed",
            ));
        }
        request.extend_from_slice(&buffer[..count]);
    }
}

fn serve_module(
    stream: &mut TcpStream,
    body: &str,
    stats: &Mutex<ServerStats>,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("set request read timeout: {error}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| format!("set response write timeout: {error}"))?;
    read_request_headers(stream).map_err(|error| format!("read request headers: {error}"))?;
    stats
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .requests += 1;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/javascript\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream
        .write_all(response.as_bytes())
        .map_err(|error| format!("write module response: {error}"))?;
    stats
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .responses += 1;
    Ok(())
}

#[test]
fn request_headers_accept_fragmented_reads_and_server_waits_for_controlled_request() {
    struct ChunkedReader<'a> {
        input: &'a [u8],
        offset: usize,
    }

    impl Read for ChunkedReader<'_> {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let count = output.len().min(3).min(self.input.len() - self.offset);
            output[..count].copy_from_slice(&self.input[self.offset..self.offset + count]);
            self.offset += count;
            Ok(count)
        }
    }

    let request = b"GET /mod.js HTTP/1.1\r\nHost: localhost\r\n\r\n";
    assert_eq!(
        read_request_headers(&mut ChunkedReader {
            input: request,
            offset: 0,
        })
        .unwrap(),
        request
    );
    assert_eq!(
        read_request_headers(&mut &b"GET /mod.js HTTP/1.1\r\n"[..])
            .unwrap_err()
            .kind(),
        io::ErrorKind::UnexpectedEof
    );

    let server = LocalModuleServer::start("export const value = 42;".to_string());
    let (release_tx, release_rx) = mpsc::channel();
    let url = server.url.clone();
    let client = thread::spawn(move || {
        release_rx.recv().unwrap();
        let address = url
            .strip_prefix("http://")
            .unwrap()
            .split('/')
            .next()
            .unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .write_all(b"GET /mod.js HTTP/1.1\r\nHost: localhost\r\n")
            .unwrap();
        stream.write_all(b"Connection: close\r\n\r\n").unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        response
    });

    // Keep the server idle until the test explicitly releases the client.
    assert!(server.diagnostics().contains("accepted=0"));
    release_tx.send(()).unwrap();
    let response = client.join().unwrap();
    assert!(String::from_utf8_lossy(&response).contains("export const value = 42;"));
    assert!(server.diagnostics().contains("responses=1"));
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
    assert_eq!(
        result.unwrap_or_else(|error| panic!("{error:#}; server: {}", server.diagnostics())),
        0
    );
    assert!(
        server.response_count() > 0,
        "server did not return the module: {}",
        server.diagnostics()
    );
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
        "expected a checksum/integrity failure, got: {error}; server: {}",
        server.diagnostics()
    );
    assert!(
        server.response_count() > 0,
        "checksum case did not receive a server response: {}",
        server.diagnostics()
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
        "expected a frozen/out-of-date lockfile error, got: {error}; server: {}",
        server.diagnostics()
    );
    assert!(!fixture.0.join("marker").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("deno.lock")).unwrap(),
        original
    );
}

#[test]
fn frozen_failures_do_not_pollute_the_same_run_graph() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let fixture = Fixture::new();
    let _env = EnvGuard::isolated_deno_dir(&fixture.0.join("deno-cache"));
    let server = LocalModuleServer::start("export const value = 42;".to_string());
    let original = lockfile(&server.url, None);
    setup_lock(
        &fixture,
        r#"{"lock":{"path":"deno.lock","frozen":true}}"#,
        &original,
    );
    std::fs::write(fixture.0.join("local.js"), "export const local = true;").unwrap();
    let source = format!(
        r#"
const remote = [{:?}, ''].join('');
for (let attempt = 0; attempt < 2; attempt++) {{
  try {{ await import(remote); throw new Error('frozen import unexpectedly succeeded'); }}
  catch (error) {{ if (!String(error).toLowerCase().includes('lockfile')) throw error; }}
}}
const local = await import('./local.js');
if (!local.local) throw new Error('local import was poisoned by failed frozen graph');
"#,
        server.url
    );
    assert_eq!(
        run_source(
            source,
            SourceLang::JavaScript,
            &fixture.0,
            &LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            }
        )
        .unwrap_or_else(|error| panic!("{error}; server={}", server.diagnostics())),
        0
    );
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("deno.lock")).unwrap(),
        original
    );
}

#[test]
fn frozen_graph_candidates_are_isolated_between_concurrent_runs() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let a = Fixture::new();
    let b = Fixture::new();
    let _env = EnvGuard::isolated_deno_dir(&a.0.join("shared-cache"));
    let server_a = LocalModuleServer::start("export const value = 'a';".to_string());
    let server_b = LocalModuleServer::start("export const value = 'b';".to_string());
    let checksum = deno_graph::source::LoaderChecksum::r#gen(b"export const value = 'a';");
    let lock_a = lockfile(&server_a.url, Some(&checksum));
    let lock_b = lockfile(&server_b.url, None);
    setup_lock(
        &a,
        r#"{"lock":{"path":"deno.lock","frozen":true}}"#,
        &lock_a,
    );
    setup_lock(
        &b,
        r#"{"lock":{"path":"deno.lock","frozen":true}}"#,
        &lock_b,
    );
    let spawn = |fixture: PathBuf, url: String| {
        std::thread::spawn(move || {
            let source = format!(
                "import {url:?}; Deno.writeTextFileSync(new URL('./ran', import.meta.url), 'yes');"
            );
            run_source(
                source,
                SourceLang::JavaScript,
                &fixture,
                &LibdenoOptions {
                    allow_all_permissions: true,
                    ..Default::default()
                },
            )
            .map(|code| code == 0)
            .map_err(|error| error.to_string())
        })
    };
    let run_a = spawn(a.0.clone(), server_a.url.clone());
    let run_b = spawn(b.0.clone(), server_b.url.clone());
    assert!(run_a.join().unwrap().unwrap());
    let error = run_b.join().unwrap().unwrap_err();
    assert!(
        error.to_lowercase().contains("lockfile"),
        "unexpected frozen error: {error}"
    );
    assert!(a.0.join("ran").exists());
    assert!(!b.0.join("ran").exists());
    assert_eq!(
        std::fs::read_to_string(a.0.join("deno.lock")).unwrap(),
        lock_a
    );
    assert_eq!(
        std::fs::read_to_string(b.0.join("deno.lock")).unwrap(),
        lock_b
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
    let result = run_import(&fixture, &server.url, &fixture.0.join("marker"));
    assert_eq!(
        result.unwrap_or_else(|error| panic!("{error:#}; server: {}", server.diagnostics())),
        0
    );
    assert!(
        server.response_count() > 0,
        "server did not return the module: {}",
        server.diagnostics()
    );
    assert_eq!(std::fs::read(fixture.0.join("marker")).unwrap(), b"ran");
}
