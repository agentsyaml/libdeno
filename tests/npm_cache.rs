//! End-to-end npm snapshot-cache test (P1-1 fix): a local mock npm registry
//! proves that a second run in the same process skips the network entirely —
//! the registry is taken down between runs and the second run still succeeds.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use libdeno::{run, LibdenoOptions};

/// Serializes the two tests in this file: NPM_CONFIG_REGISTRY / DENO_DIR are
/// process-global env vars and the in-process snapshot cache is shared state,
/// so env-dependent tests must not interleave.
static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn env_lock() -> &'static Mutex<()> {
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-npm-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Minimal HTTP server for one mock package (`mock-pkg`): any request path
/// other than the exact packument URL is answered with the tarball bytes, so
/// the request order and retry count do not matter. `shutdown` takes the
/// registry down; later connections are refused.
struct MockRegistry {
    base_url: String,
    shutdown: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl MockRegistry {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}/", addr.port());
        let (packument, tarball) = make_package(&base_url);
        let (shutdown, done_rx) = std::sync::mpsc::channel::<()>();
        listener.set_nonblocking(true).unwrap();
        let thread = std::thread::spawn(move || loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let request = read_request(&mut stream);
                    let path = request
                        .split(|b| *b == b' ')
                        .nth(1)
                        .map(|p| String::from_utf8_lossy(p).into_owned())
                        .unwrap_or_default();
                    let response = if path == "/mock-pkg" {
                        http_response(200, "application/json", packument.as_bytes())
                    } else {
                        http_response(200, "application/octet-stream", &tarball)
                    };
                    let _ = stream.write_all(&response);
                    let _ = stream.flush();
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if done_rx.try_recv().is_ok() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(_) => break,
            }
        });
        Self {
            base_url,
            shutdown,
            thread,
        }
    }

    fn url(&self) -> &str {
        &self.base_url
    }

    /// Takes the registry down and waits until the port is released; later
    /// connections are refused.
    fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.thread.join();
    }
}

fn read_request(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                request.extend_from_slice(&buf[..n]);
                if request.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    request
}

fn http_response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

/// Packument + gzipped tarball for a single mock package (`mock-pkg@1.0.0`).
/// The tarball mirrors a real npm artifact: a ustar archive whose entries
/// carry the leading `package/` component that the extractor strips.
fn make_package(base_url: &str) -> (String, Vec<u8>) {
    let packument = format!(
        r#"{{"name":"mock-pkg","dist-tags":{{"latest":"1.0.0"}},"versions":{{"1.0.0":{{"name":"mock-pkg","version":"1.0.0","main":"index.js","dist":{{"tarball":"{base_url}mock-pkg-1.0.0.tgz"}}}}}}}}"#
    );
    let package_json = br#"{"name":"mock-pkg","version":"1.0.0","main":"index.js"}"#;
    let index_js = br#"module.exports = "hello-from-mock-pkg";"#;
    let tar = tar_archive(&[
        ("package/package.json", package_json),
        ("package/index.js", index_js),
    ]);
    (packument, gzip(&tar))
}

fn tar_archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, data) in entries {
        out.extend_from_slice(&tar_header(name, data.len()));
        out.extend_from_slice(data);
        let pad = (512 - (data.len() % 512)) % 512;
        out.extend(std::iter::repeat_n(0u8, pad));
    }
    out.extend([0u8; 1024]); // end-of-archive marker (two zero blocks)
    out
}

fn tar_header(name: &str, size: usize) -> [u8; 512] {
    let mut header = [0u8; 512];
    let name_bytes = name.as_bytes();
    header[..name_bytes.len()].copy_from_slice(name_bytes);
    header[100..108].copy_from_slice(b"0000644\0"); // mode
    header[108..116].copy_from_slice(b"0000000\0"); // uid
    header[116..124].copy_from_slice(b"0000000\0"); // gid
    header[124..136].copy_from_slice(format!("{:011o}\0", size).as_bytes());
    header[136..148].copy_from_slice(b"00000000000\0"); // mtime
    header[148..156].copy_from_slice(b"        "); // checksum placeholder
    header[156] = b'0'; // typeflag: regular file
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let checksum: u32 = header.iter().map(|&b| b as u32).sum();
    header[148..156].copy_from_slice(format!("{:06o}\0 ", checksum).as_bytes());
    header
}

/// A valid gzip stream built from deflate "stored" (uncompressed) blocks plus
/// a hand-computed CRC32/ISIZE trailer — no compression library needed.
fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 0x08, 0x00, 0, 0, 0, 0, 0, 0x03];
    let mut pos = 0;
    loop {
        let remaining = data.len() - pos;
        let chunk = remaining.min(65535);
        let is_final = chunk == remaining;
        out.push(is_final as u8); // BFINAL; BTYPE=00 (stored block)
        let len = chunk as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(&data[pos..pos + chunk]);
        pos += chunk;
        if is_final {
            break;
        }
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&((data.len() as u32).to_le_bytes()));
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (n, slot) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFF;
    for &byte in data {
        crc = table[((crc ^ byte as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

/// Accepts every connection and immediately drops it while counting attempts.
/// Used by the negative control to prove the mock registry is genuinely
/// required: the run must fail *and* the network must actually be reached — a
/// resolution served from a cache would make zero connections.
struct RefusingRegistry {
    base_url: String,
    count: Arc<AtomicUsize>,
    shutdown: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl RefusingRegistry {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}/", addr.port());
        let count = Arc::new(AtomicUsize::new(0));
        let (shutdown, done_rx) = std::sync::mpsc::channel::<()>();
        listener.set_nonblocking(true).unwrap();
        let thread = {
            let count = count.clone();
            std::thread::spawn(move || loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        count.fetch_add(1, Ordering::SeqCst);
                        drop(stream); // break the connection -> client sees a network failure
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if done_rx.try_recv().is_ok() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            })
        };
        Self {
            base_url,
            count,
            shutdown,
            thread,
        }
    }

    fn url(&self) -> &str {
        &self.base_url
    }

    fn connection_count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.thread.join();
    }
}

fn write_project(dir: &Path) {
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"proj","dependencies":{"mock-pkg":"1.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("main.js"),
        "import pkg from 'npm:mock-pkg';\n\
         if (pkg !== 'hello-from-mock-pkg') throw new Error('unexpected pkg: ' + pkg);\n\
         Deno.writeTextFileSync('out.txt', pkg);",
    )
    .unwrap();
}

#[test]
fn first_run_requires_the_registry() {
    // Negative control: prove the mock registry is genuinely what satisfies
    // run 1 (the cache-hit test would be vacuous otherwise). A registry that
    // refuses every connection must make the very first run fail, and the
    // packument fetch must actually be attempted over the network.
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("registry-down");
    let deno_dir = temp_dir("registry-down-deno");
    write_project(&dir);
    let registry = RefusingRegistry::new();
    std::env::set_var("NPM_CONFIG_REGISTRY", registry.url());
    std::env::set_var("DENO_DIR", &deno_dir);
    let entry = dir.join("main.js");
    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        ..Default::default()
    };
    let err = run(&entry, &options).unwrap_err();
    let connections = registry.connection_count();
    registry.shutdown();
    // The run failed AND the network was reached. The surfaced message is a
    // snapshot-lookup error ("Could not find constraint") rather than the raw
    // connection error: the module loader resolves npm: -> file via a pure
    // snapshot lookup, and the graph build's failed registry fetch leaves the
    // resolution empty, so the lookup reports the package as missing.
    assert!(
        connections > 0,
        "resolution never attempted the network (was it served from a cache?)"
    );
    assert!(
        err.to_string().contains("mock-pkg"),
        "expected an npm-resolution failure, got: {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&deno_dir);
}

#[test]
fn cached_snapshot_skips_network_on_second_run() {
    // P1-1: run 1 resolves through the live mock registry (and populates the
    // in-process snapshot cache + the on-disk npm cache). After the registry
    // is taken down and node_modules is deleted, run 2 must still succeed —
    // re-resolution is served from the snapshot cache, re-install from the
    // on-disk tarball cache, with zero network traffic.
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let dir = temp_dir("cache-hit");
    let deno_dir = temp_dir("cache-hit-deno");
    write_project(&dir);
    let entry = dir.join("main.js");
    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        ..Default::default()
    };

    let registry = MockRegistry::new();
    let registry_url = registry.url().to_string();
    std::env::set_var("NPM_CONFIG_REGISTRY", &registry_url);
    std::env::set_var("DENO_DIR", &deno_dir);

    // Run 1: resolves + installs through the live mock registry.
    assert_eq!(run(&entry, &options).unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "hello-from-mock-pkg"
    );

    // Registry down + node_modules gone: run 2 is served entirely by caches.
    registry.shutdown();
    std::fs::remove_dir_all(dir.join("node_modules")).unwrap();

    assert_eq!(run(&entry, &options).unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "hello-from-mock-pkg"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&deno_dir);
}
