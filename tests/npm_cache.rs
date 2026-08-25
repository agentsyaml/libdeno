//! End-to-end npm snapshot-cache test (P1-1 fix): a local mock npm registry
//! proves that a second run in the same process skips the network entirely —
//! the registry is taken down between runs and the second run still succeeds.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock};

use libdeno::{run, run_with, LibdenoOptions, LibdenoRuntime};

/// Serializes the tests in this file: NPM_CONFIG_REGISTRY / DENO_DIR are
/// process-global env vars and the in-process snapshot cache is shared state,
/// so env-dependent tests must not interleave.
static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn env_lock() -> &'static Mutex<()> {
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl EnvGuard {
    fn new(names: &[&'static str]) -> Self {
        Self(
            names
                .iter()
                .map(|&name| (name, std::env::var_os(name)))
                .collect(),
        )
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (name, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }
}

fn clean_home(name: &str) -> (EnvGuard, PathBuf) {
    let home = temp_dir(name);
    let guard = EnvGuard::new(&["HOME"]);
    std::env::set_var("HOME", &home);
    (guard, home)
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("libdeno-npm-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn set_modified_time(path: &Path, modified: std::time::SystemTime) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
}

/// Minimal HTTP server for the mock packages used by these tests. It serves
/// packuments and tarballs by request path; `shutdown` takes the registry down
/// so later connections are refused.
struct MockRegistry {
    base_url: String,
    requests: Arc<AtomicUsize>,
    shutdown: std::sync::mpsc::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

impl MockRegistry {
    fn new() -> Self {
        Self::with_marker("hello-from-mock-pkg")
    }

    fn with_marker(marker: &str) -> Self {
        Self::with_packages(&[("mock-pkg", marker)])
    }

    fn with_packages(packages: &[(&str, &str)]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}/", addr.port());
        let responses = packages
            .iter()
            .flat_map(|(name, marker)| {
                let (packument, tarball) = make_named_package(&base_url, name, marker);
                [
                    (
                        format!("/{name}"),
                        ("application/json", packument.into_bytes()),
                    ),
                    (
                        format!("/{name}-1.0.0.tgz"),
                        ("application/octet-stream", tarball),
                    ),
                ]
            })
            .collect::<HashMap<_, _>>();
        let requests = Arc::new(AtomicUsize::new(0));
        let (shutdown, done_rx) = std::sync::mpsc::channel::<()>();
        listener.set_nonblocking(true).unwrap();
        let requests_for_thread = requests.clone();
        let thread = std::thread::spawn(move || loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    requests_for_thread.fetch_add(1, Ordering::SeqCst);
                    let _ = stream.set_nonblocking(false);
                    let request = read_request(&mut stream);
                    let path = request
                        .split(|b| *b == b' ')
                        .nth(1)
                        .map(|p| String::from_utf8_lossy(p).into_owned())
                        .unwrap_or_default();
                    let response = responses
                        .get(&path)
                        .map(|(content_type, body)| http_response(200, content_type, body))
                        .unwrap_or_else(|| http_response(404, "text/plain", b"not found"));
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
            requests,
            shutdown,
            thread,
        }
    }

    fn url(&self) -> &str {
        &self.base_url
    }

    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
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

/// Packument + gzipped tarball for a single mock package (`name@1.0.0`).
/// The tarball mirrors a real npm artifact: a ustar archive whose entries
/// carry the leading `package/` component that the extractor strips.
fn make_named_package(base_url: &str, name: &str, marker: &str) -> (String, Vec<u8>) {
    let packument = format!(
        r#"{{"name":"{name}","dist-tags":{{"latest":"1.0.0"}},"versions":{{"1.0.0":{{"name":"{name}","version":"1.0.0","main":"index.js","dist":{{"tarball":"{base_url}{name}-1.0.0.tgz"}}}}}}}}"#
    );
    let package_json = format!(r#"{{"name":"{name}","version":"1.0.0","main":"index.js"}}"#);
    let index_js = format!("module.exports = {marker:?};");
    let tar = tar_archive(&[
        ("package/package.json", package_json.as_bytes()),
        ("package/index.js", index_js.as_bytes()),
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
    write_project_with_expected(dir, "hello-from-mock-pkg");
}

fn write_project_with_expected(dir: &Path, expected: &str) {
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"proj","dependencies":{"mock-pkg":"1.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("main.js"),
        format!(
            "import pkg from 'npm:mock-pkg';\n\
         if (pkg !== {expected:?}) throw new Error('unexpected pkg: ' + pkg);\n\
         // Absolute path via import.meta.url: in-process runs never chdir, so\n\
         // a relative path would resolve against the host cwd.\n\
         Deno.writeTextFileSync(new URL('./out.txt', import.meta.url), pkg);"
        ),
    )
    .unwrap();
}

fn write_project_that_switches_registry(dir: &Path, expected: &str, next_registry: &str) {
    std::fs::write(
        dir.join("package.json"),
        r#"{"name":"proj","dependencies":{"mock-pkg":"1.0.0"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("main.js"),
        format!(
            "import pkg from 'npm:mock-pkg';\n\
         if (pkg !== {expected:?}) throw new Error('unexpected pkg: ' + pkg);\n\
         Deno.writeTextFileSync(new URL('./.npmrc', import.meta.url), {npmrc:?});\n\
         Deno.writeTextFileSync(new URL('./out.txt', import.meta.url), pkg);",
            npmrc = format!("registry={next_registry}\n"),
        ),
    )
    .unwrap();
}

fn write_workspace_member(dir: &Path, package_name: &str, expected: &str) {
    std::fs::write(
        dir.join("package.json"),
        format!(
            r#"{{"name":"{package_name}-project","dependencies":{{"{package_name}":"1.0.0"}}}}"#
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("main.js"),
        format!(
            "import pkg from 'npm:{package_name}';\n\
         if (pkg !== {expected:?}) throw new Error('unexpected pkg: ' + pkg);\n\
         Deno.writeTextFileSync(new URL('./out.txt', import.meta.url), pkg);"
        ),
    )
    .unwrap();
}

fn build_runtime(cwd: &Path) -> LibdenoRuntime {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = rt.block_on(LibdenoRuntime::new(cwd)).unwrap();
    drop(rt);
    runtime
}

#[test]
fn first_run_requires_the_registry() {
    // Negative control: prove the mock registry is genuinely what satisfies
    // run 1 (the cache-hit test would be vacuous otherwise). A registry that
    // refuses every connection must make the very first run fail, and the
    // packument fetch must actually be attempted over the network.
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let (_home_env, home) = clean_home("registry-down-home");
    let dir = temp_dir("registry-down");
    let deno_dir = temp_dir("registry-down-deno");
    write_project(&dir);
    let registry = RefusingRegistry::new();
    std::env::set_var("NPM_CONFIG_REGISTRY", registry.url());
    std::env::set_var("DENO_DIR", &deno_dir);
    let entry = dir.join("main.js");
    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        allow_all_permissions: true,
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
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn cached_snapshot_skips_network_on_second_run() {
    // P1-1: run 1 resolves through the live mock registry (and populates the
    // in-process snapshot cache + the on-disk npm cache). After the registry
    // is taken down and node_modules is deleted, run 2 must still succeed —
    // re-resolution is served from the snapshot cache, re-install from the
    // on-disk tarball cache, with zero network traffic.
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let (_home_env, home) = clean_home("cache-hit-home");
    let dir = temp_dir("cache-hit");
    let deno_dir = temp_dir("cache-hit-deno");
    write_project(&dir);
    let entry = dir.join("main.js");
    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        allow_all_permissions: true,
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
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn snapshot_save_uses_original_resolver_key_when_npmrc_changes_during_run() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new(&["NPM_CONFIG_REGISTRY", "DENO_DIR", "HOME"]);
    let home = temp_dir("save-original-key-home");
    std::env::set_var("HOME", &home);
    let dir = temp_dir("save-original-key");
    let deno_dir = temp_dir("save-original-key-deno");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&deno_dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&deno_dir).unwrap();

    let registry_a = MockRegistry::with_marker("from-registry-a");
    let registry_b = MockRegistry::with_marker("from-registry-b");
    std::env::remove_var("NPM_CONFIG_REGISTRY");
    std::env::set_var("DENO_DIR", &deno_dir);
    std::fs::write(
        dir.join(".npmrc"),
        format!("registry={}\n", registry_a.url()),
    )
    .unwrap();
    write_project_that_switches_registry(&dir, "from-registry-a", registry_b.url());

    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(run(dir.join("main.js"), &options).unwrap(), 0);

    registry_a.shutdown();
    std::fs::remove_dir_all(dir.join("node_modules")).unwrap();
    write_project_that_switches_registry(&dir, "from-registry-b", registry_b.url());

    assert_eq!(run(dir.join("main.js"), &options).unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "from-registry-b"
    );
    let requests = registry_b.request_count();
    registry_b.shutdown();
    assert!(
        requests > 0,
        "the new npmrc identity was served from the old key"
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&deno_dir);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn workspace_discovery_scopes_do_not_share_snapshots() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new(&["NPM_CONFIG_REGISTRY", "DENO_DIR", "HOME"]);
    let home = temp_dir("workspace-scope-home");
    std::env::set_var("HOME", &home);
    let root = temp_dir("workspace-scope");
    let deno_dir = temp_dir("workspace-scope-deno");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&deno_dir);
    let member_a = root.join("member-a");
    let member_b = root.join("member-b");
    std::fs::create_dir_all(&member_a).unwrap();
    std::fs::create_dir_all(&member_b).unwrap();
    std::fs::create_dir_all(&deno_dir).unwrap();

    let registry = MockRegistry::with_packages(&[
        ("scope-a-pkg", "from-member-a"),
        ("scope-b-pkg", "from-member-b"),
    ]);
    std::env::remove_var("NPM_CONFIG_REGISTRY");
    std::env::set_var("DENO_DIR", &deno_dir);
    std::fs::write(
        root.join("deno.json"),
        r#"{"workspace":["./member-a","./member-b"]}"#,
    )
    .unwrap();
    std::fs::write(
        root.join(".npmrc"),
        format!("registry={}\n", registry.url()),
    )
    .unwrap();
    write_workspace_member(&member_a, "scope-a-pkg", "from-member-a");
    write_workspace_member(&member_b, "scope-b-pkg", "from-member-b");

    let options = LibdenoOptions {
        cwd: Some(root.clone()),
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(run(member_a.join("main.js"), &options).unwrap(), 0);
    std::fs::remove_dir_all(root.join("node_modules")).unwrap();
    assert_eq!(run(member_b.join("main.js"), &options).unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(member_b.join("out.txt")).unwrap(),
        "from-member-b"
    );

    // The third run must use member A's own snapshot after the registry is
    // gone. A cwd-only key would have been overwritten by member B above.
    registry.shutdown();
    std::fs::remove_dir_all(root.join("node_modules")).unwrap();
    assert_eq!(run(member_a.join("main.js"), &options).unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(member_a.join("out.txt")).unwrap(),
        "from-member-a"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&deno_dir);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn reusable_runtime_rebuilds_for_same_size_workspace_member_package_and_config_edit() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new(&["NPM_CONFIG_REGISTRY", "DENO_DIR", "HOME"]);
    let home = temp_dir("workspace-runtime-home");
    std::env::set_var("HOME", &home);
    let root = temp_dir("workspace-runtime");
    let deno_dir = temp_dir("workspace-runtime-deno");
    let member = root.join("member");
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&deno_dir);
    std::fs::create_dir_all(&member).unwrap();
    std::fs::create_dir_all(&deno_dir).unwrap();

    let registry =
        MockRegistry::with_packages(&[("scope-a-pkg", "from-a"), ("scope-b-pkg", "from-b")]);
    std::env::set_var("NPM_CONFIG_REGISTRY", registry.url());
    std::env::set_var("DENO_DIR", &deno_dir);
    std::fs::write(root.join("deno.json"), r#"{"workspace":["./member"]}"#).unwrap();
    write_workspace_member(&member, "scope-a-pkg", "from-a");
    let member_config = member.join("deno.json");
    let config_a = r#"{"lint":{"include":["a.js"]}}"#;
    let config_b = r#"{"lint":{"include":["b.js"]}}"#;
    assert_eq!(config_a.len(), config_b.len());
    std::fs::write(&member_config, config_a).unwrap();

    let runtime = build_runtime(&root);
    let options = LibdenoOptions {
        cwd: Some(root.clone()),
        allow_all_permissions: true,
        ..Default::default()
    };
    let entry = member.join("main.js");
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(member.join("out.txt")).unwrap(),
        "from-a"
    );

    let package_json = member.join("package.json");
    let package_len = std::fs::metadata(&package_json).unwrap().len();
    write_workspace_member(&member, "scope-b-pkg", "from-b");
    assert_eq!(std::fs::metadata(&package_json).unwrap().len(), package_len);
    std::fs::write(&member_config, config_b).unwrap();
    assert_eq!(
        std::fs::metadata(&member_config).unwrap().len(),
        config_a.len() as u64
    );
    assert_eq!(run_with(&runtime, &entry, &options).unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(member.join("out.txt")).unwrap(),
        "from-b"
    );
    let requests = registry.request_count();
    registry.shutdown();
    assert!(requests > 0);

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&deno_dir);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn reusable_runtime_rebuilds_for_home_npmrc_change_with_independent_userconfig() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new(&[
        "NPM_CONFIG_REGISTRY",
        "NPM_CONFIG_USERCONFIG",
        "DENO_DIR",
        "HOME",
        "USERPROFILE",
    ]);
    let dir = temp_dir("runtime-registry-change");
    let deno_dir = temp_dir("runtime-registry-change-deno");
    let home = dir.join("home");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&deno_dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&deno_dir).unwrap();

    let home_npmrc = home.join(".npmrc");
    let userconfig_a = dir.join("npmrc-a");
    let userconfig_b = dir.join("npmrc-b");
    let registry_a = MockRegistry::with_marker("from-registry-a");
    let registry_b = MockRegistry::with_marker("from-registry-b");
    let config_a = format!("registry={}\n", registry_a.url());
    let config_b = format!("registry={}\n", registry_b.url());
    let config_len = config_a.len().max(config_b.len()) + 16;
    let pad_config = |config: String| {
        format!(
            "{config}{}",
            "#".repeat(config_len.saturating_sub(config.len()))
        )
    };
    let home_content_a = pad_config(config_a);
    let home_content_b = pad_config(config_b);
    let userconfig_content_a = "# unrelated userconfig-a\n";
    let userconfig_content_b = "# unrelated userconfig-b\n";
    std::fs::write(&home_npmrc, &home_content_a).unwrap();
    std::fs::write(&userconfig_a, userconfig_content_a).unwrap();
    std::fs::write(&userconfig_b, userconfig_content_b).unwrap();
    assert_eq!(userconfig_content_a.len(), userconfig_content_b.len());
    // deno_resolver 0.88 reads `$HOME/.npmrc` (or `%USERPROFILE%\.npmrc` on
    // Windows); keep NPM_CONFIG_USERCONFIG on a separate, unrelated file and
    // change it independently below.
    std::env::remove_var("NPM_CONFIG_REGISTRY");
    std::env::set_var("HOME", &home);
    std::env::set_var("USERPROFILE", &home);
    std::env::set_var("NPM_CONFIG_USERCONFIG", &userconfig_a);
    std::env::set_var("DENO_DIR", &deno_dir);
    write_project_with_expected(&dir, "from-registry-a");

    let runtime = build_runtime(&dir);
    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        allow_all_permissions: true,
        ..Default::default()
    };
    assert_eq!(
        run_with(&runtime, dir.join("main.js"), &options).unwrap(),
        0
    );

    // Changing only NPM_CONFIG_USERCONFIG must not select a different npmrc
    // for the resolver.
    std::env::set_var("NPM_CONFIG_USERCONFIG", &userconfig_b);
    write_project_with_expected(&dir, "from-registry-a");
    assert_eq!(
        run_with(&runtime, dir.join("main.js"), &options).unwrap(),
        0
    );

    registry_a.shutdown();
    std::fs::remove_dir_all(dir.join("node_modules")).unwrap();
    let home_modified = std::fs::metadata(&home_npmrc).unwrap().modified().unwrap();
    std::fs::write(&home_npmrc, &home_content_b).unwrap();
    set_modified_time(&home_npmrc, home_modified);
    assert_eq!(
        home_content_a.len() as u64,
        std::fs::metadata(&home_npmrc).unwrap().len()
    );
    assert_eq!(
        home_modified,
        std::fs::metadata(&home_npmrc).unwrap().modified().unwrap()
    );
    write_project_with_expected(&dir, "from-registry-b");

    assert_eq!(
        run_with(&runtime, dir.join("main.js"), &options).unwrap(),
        0
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "from-registry-b"
    );
    let requests = registry_b.request_count();
    registry_b.shutdown();
    assert!(requests > 0, "the rebuilt runtime never reached registry B");

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&deno_dir);
}

#[test]
fn child_process_fork_uses_the_parent_npm_snapshot() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let (_home_env, home) = clean_home("fork-snapshot-home");
    let dir = temp_dir("fork-snapshot");
    let deno_dir = temp_dir("fork-snapshot-deno");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&deno_dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&deno_dir).unwrap();

    let old_registry = std::env::var_os("NPM_CONFIG_REGISTRY");
    let old_userconfig = std::env::var_os("NPM_CONFIG_USERCONFIG");
    let old_deno_dir = std::env::var_os("DENO_DIR");
    let registry = MockRegistry::new();
    std::env::set_var("NPM_CONFIG_REGISTRY", registry.url());
    std::env::remove_var("NPM_CONFIG_USERCONFIG");
    std::env::set_var("DENO_DIR", &deno_dir);

    let quote = |path: &Path| format!("{:?}", path.to_string_lossy());
    let ready = dir.join("ready");
    let release = dir.join("release");
    let done = dir.join("done");
    let child = dir.join("fork-child.cjs");
    let parent = dir.join("fork-parent.cjs");
    let child_host = PathBuf::from(env!("CARGO_BIN_EXE_child_host"));

    std::fs::write(
        &child,
        r#"(async () => {
const pkg = (await import('npm:mock-pkg')).default;
if (!process.send) throw new Error('fork child has no IPC channel');
process.send(pkg);
})().catch((error) => { console.error(error); process.exitCode = 1; });"#,
    )
    .unwrap();
    std::fs::write(
        &parent,
        format!(
            r#"(async () => {{
const fs = require('node:fs');
const {{ fork }} = require('node:child_process');
const pkg = (await import('npm:mock-pkg')).default;
fs.writeFileSync({ready}, pkg);
while (!fs.existsSync({release})) await new Promise((resolve) => setTimeout(resolve, 10));
const child = fork({child}, [], {{ execPath: {child_host} }});
await new Promise((resolve, reject) => {{
  let gotMessage = false;
  let settled = false;
  let timer;
  const fail = (error) => {{
    if (!settled) {{ settled = true; clearTimeout(timer); reject(error); }}
    child.kill();
  }};
  child.on('message', (message) => {{
    if (message !== pkg) return fail(new Error('unexpected fork result: ' + message));
    gotMessage = true;
    if (child.connected) child.disconnect();
  }});
  child.on('error', fail);
  child.on('exit', (code) => {{
    if (settled) return;
    if (!gotMessage || code !== 0) return fail(new Error('fork exited before IPC success: ' + code));
    settled = true;
    clearTimeout(timer);
    resolve();
  }});
  timer = setTimeout(() => fail(new Error('fork timed out')), 10000);
}});
fs.writeFileSync({done}, pkg);
}})().catch((error) => {{ console.error(error); process.exitCode = 1; }});"#,
            ready = quote(&ready),
            release = quote(&release),
            child = quote(&child),
            child_host = quote(&child_host),
            done = quote(&done),
        ),
    )
    .unwrap();

    let entry = parent.clone();
    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        allow_all_permissions: true,
        ..Default::default()
    };
    let handle = std::thread::spawn(move || run(&entry, &options));
    for _ in 0..600 {
        if ready.is_file() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(ready.is_file(), "parent never resolved npm:mock-pkg");
    assert!(
        registry.request_count() > 0,
        "parent never reached the registry"
    );

    // The parent has already resolved the package. Removing its installed
    // tree and taking the registry down makes a successful fork depend on the
    // serialized npm snapshot plus the on-disk tarball cache.
    registry.shutdown();
    std::fs::remove_dir_all(dir.join("node_modules")).unwrap();
    std::fs::write(&release, "go").unwrap();
    assert_eq!(handle.join().unwrap().unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(&done).unwrap(),
        "hello-from-mock-pkg"
    );

    match old_registry {
        Some(value) => std::env::set_var("NPM_CONFIG_REGISTRY", value),
        None => std::env::remove_var("NPM_CONFIG_REGISTRY"),
    }
    match old_userconfig {
        Some(value) => std::env::set_var("NPM_CONFIG_USERCONFIG", value),
        None => std::env::remove_var("NPM_CONFIG_USERCONFIG"),
    }
    match old_deno_dir {
        Some(value) => std::env::set_var("DENO_DIR", value),
        None => std::env::remove_var("DENO_DIR"),
    }
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&deno_dir);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn reusable_runtime_first_managed_npm_use_concurrent_calls_share_cold_rebuild() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _env = EnvGuard::new(&["NPM_CONFIG_REGISTRY", "DENO_DIR", "HOME"]);
    let home = temp_dir("runtime-cold-npm-home");
    std::env::set_var("HOME", &home);
    let dir = temp_dir("runtime-cold-npm");
    let deno_dir = temp_dir("runtime-cold-npm-deno");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&deno_dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&deno_dir).unwrap();

    let registry = MockRegistry::new();
    std::env::set_var("NPM_CONFIG_REGISTRY", registry.url());
    std::env::set_var("DENO_DIR", &deno_dir);

    // Build the reusable runtime before the managed project exists. Adding
    // the package manifest leaves its accepted inputs cold; the barrier then
    // releases both first calls against that same invalidation.
    let runtime = build_runtime(&dir);
    write_project(&dir);
    let entry = dir.join("main.js");
    let options = LibdenoOptions {
        cwd: Some(dir.clone()),
        allow_all_permissions: true,
        ..Default::default()
    };
    let start = Arc::new(Barrier::new(2));
    let (first, second) = std::thread::scope(|scope| {
        let start_for_thread = start.clone();
        let runtime_for_thread = &runtime;
        let entry_for_thread = &entry;
        let options_for_thread = &options;
        let first = scope.spawn(move || {
            start_for_thread.wait();
            run_with(runtime_for_thread, entry_for_thread, options_for_thread)
        });
        start.wait();
        let second = run_with(&runtime, &entry, &options);
        (first.join().unwrap(), second)
    });

    assert_eq!(first.unwrap(), 0);
    assert_eq!(second.unwrap(), 0);
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt")).unwrap(),
        "hello-from-mock-pkg"
    );
    assert!(
        registry.request_count() > 0,
        "the first managed-npm use never reached the mock registry"
    );
    registry.shutdown();

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&deno_dir);
    let _ = std::fs::remove_dir_all(&home);
}
