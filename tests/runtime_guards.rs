use libdeno::{run, LibdenoOptions};
use std::fs;

fn run_js(dir: &std::path::Path, name: &str, code: &str, options: LibdenoOptions) -> i32 {
    let entry = dir.join(name);
    fs::write(&entry, code).unwrap();
    run(entry, &options).unwrap()
}

fn options(dir: &std::path::Path, features: Option<Vec<String>>) -> LibdenoOptions {
    LibdenoOptions {
        permissions: vec![format!("--allow-read={}", dir.display())],
        features,
        ..Default::default()
    }
}

#[test]
fn tcp_guard_blocks_fatal_feature_exit_and_preserves_permission_checks() {
    let dir = std::env::temp_dir().join(format!("libdeno-tcp-guard-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let blocked = r#"
try { Deno.listen({ hostname: "127.0.0.1", port: 0, reusePort: true }); throw Error("unexpected success"); }
catch (error) { if (!error.message.includes("requires the net feature")) throw error; }
try { const ops = await import("ext:core/ops"); if (["op_libdeno_net_listen_tcp_alias", "op_libdeno_require_stat_alias", "op_libdeno_require_real_path_alias", "op_libdeno_require_package_scope_alias", "op_libdeno_require_package_imports_alias", "op_libdeno_install_require_delegates"].some(name => name in ops)) throw Error("alias import leaked"); }
catch (error) { if (error.message.includes("alias import leaked")) throw error; }
"#;
    assert_eq!(run_js(&dir, "blocked.js", blocked, options(&dir, None)), 0);

    let mut granted = options(&dir, None);
    granted.permissions.push("--allow-net=127.0.0.1".into());
    assert_eq!(
        run_js(
            &dir,
            "tcp.js",
            "Deno.listen({ hostname: '127.0.0.1', port: 0 }).close();",
            granted
        ),
        0
    );

    let mut node_udp = options(&dir, None);
    node_udp.permissions.push("--allow-net=127.0.0.1".into());
    assert_eq!(run_js(&dir, "node-udp.js", "import dgram from 'node:dgram'; const socket=dgram.createSocket('udp4'); await new Promise((resolve,reject)=>{socket.once('error',reject);socket.bind(0,'127.0.0.1',resolve);}); socket.close();", node_udp), 0);

    let enabled = options(&dir, Some(vec!["net".into()]));
    let denied = "try { Deno.listen({ hostname: '127.0.0.1', port: 0, reusePort: true }); throw Error('permission bypass'); } catch (error) { if (error.name !== 'NotCapable') throw error; }";
    assert_eq!(run_js(&dir, "denied.js", denied, enabled), 0);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn tcp_guard_is_installed_in_web_workers() {
    let dir = std::env::temp_dir().join(format!("libdeno-tcp-worker-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("worker.js"), "let denied=''; try { Deno.listen({ hostname: '127.0.0.1', port: 0, reusePort: true }); } catch (e) { denied=e.message; } Deno.listen({ hostname: '127.0.0.1', port: 0 }).close(); postMessage(denied);").unwrap();
    let main = "const worker = new Worker(new URL('./worker.js', import.meta.url).href, { type: 'module' }); worker.onmessage = (event) => { if (!event.data.includes('requires the net feature')) Deno.exit(2); Deno.exit(0); }; worker.onerror = () => Deno.exit(3);";
    let mut permission = options(&dir, None);
    permission.permissions.push("--allow-net=127.0.0.1".into());
    assert_eq!(run_js(&dir, "main.js", main, permission), 0);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn net_feature_ops_still_reach_upstream_permission_checks() {
    let dir = std::env::temp_dir().join(format!(
        "libdeno-enabled-feature-guards-{}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    let mut source = String::from(
        r#"
try { Deno.listenDatagram({ transport: "udp", hostname: "127.0.0.1", port: 0 }); throw new Error("UDP permission bypass"); }
catch (error) { if (error.name !== "NotCapable") throw error; }
"#,
    );
    if cfg!(unix) {
        source.push_str(r#"
try { Deno.listenDatagram({ transport: "unixpacket", path: "/tmp/libdeno-guard.sock" }); throw new Error("unixpacket permission bypass"); }
catch (error) { if (error.name !== "NotCapable") throw error; }
"#);
    }
    if cfg!(any(
        target_os = "android",
        target_os = "linux",
        target_os = "macos"
    )) {
        source.push_str(r#"
for (const call of [() => Deno.listen({ transport: "vsock", cid: 2, port: 1024 }), () => Deno.connect({ transport: "vsock", cid: 2, port: 1024 })]) {
  try { await call(); throw new Error("vsock permission bypass"); }
  catch (error) { if (error.name !== "NotCapable") throw error; }
}
"#);
    }
    assert_eq!(
        run_js(
            &dir,
            "enabled.js",
            &source,
            options(&dir, Some(vec!["net".into(), "vsock".into()]))
        ),
        0
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn enabled_cron_route_keeps_the_upstream_implementation() {
    let dir = std::env::temp_dir().join(format!("libdeno-cron-guard-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    assert_eq!(
        run_js(
            &dir,
            "cron.js",
            "Deno.cron('guard-test', '* * * * *', () => {}); Deno.exit(0);",
            LibdenoOptions {
                allow_all_permissions: true,
                ..Default::default()
            },
        ),
        0
    );
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn tcp_guard_is_installed_in_node_worker_threads() {
    let dir =
        std::env::temp_dir().join(format!("libdeno-node-worker-guard-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let source = r#"
import { Worker } from "node:worker_threads";
const worker = new Worker(`
  const { parentPort } = require("node:worker_threads");
  let denied = "";
  try { Deno.listen({ hostname: "127.0.0.1", port: 0, reusePort: true }); }
  catch (error) { denied = error.message; }
  Deno.listen({ hostname: "127.0.0.1", port: 0 }).close();
  parentPort.postMessage(denied);
`, { eval: true });
const message = await new Promise((resolve, reject) => { worker.once("message", resolve); worker.once("error", reject); });
if (!message.includes("requires the net feature")) throw new Error(`unexpected Node worker result: ${message}`);
await worker.terminate();
"#;
    let mut permission = options(&dir, None);
    permission.permissions.push("--allow-net=127.0.0.1".into());
    assert_eq!(run_js(&dir, "node-worker.js", source, permission), 0);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn tls_guard_normalizes_reuse_port_once_and_preserves_listening() {
    let dir = std::env::temp_dir().join(format!("libdeno-tls-guard-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let cert = fs::read_to_string(fixtures.join("runtime-guard-localhost.crt")).unwrap();
    let key = fs::read_to_string(fixtures.join("runtime-guard-localhost.key")).unwrap();
    let cert_js = format!("const cert = {cert:?}; const key = {key:?};\n");
    let blocked = format!(
        "{cert_js}try {{ Deno.listenTls({{hostname:'127.0.0.1',port:0,cert,key,reusePort:true}}); throw Error('unexpected success'); }} catch (error) {{ if (!error.message.includes('requires the net feature')) throw error; }}"
    );
    assert_eq!(run_js(&dir, "blocked.js", &blocked, options(&dir, None)), 0);

    let prototype = format!(
        "{cert_js}globalThis.setterCalls=0; Object.defineProperty(Object.prototype,'reusePort',{{configurable:true,set(_value){{setterCalls++;}}}}); try {{ Deno.listenTls({{hostname:'127.0.0.1',port:0,cert,key,reusePort:false}}); throw Error('expected permission denial'); }} catch (error) {{ if (error.name!=='NotCapable') throw error; }} if (setterCalls!==0) throw Error('normalization invoked inherited setter');"
    );
    assert_eq!(
        run_js(&dir, "prototype.js", &prototype, options(&dir, None)),
        0
    );

    let denied = format!(
        "{cert_js}try {{ Deno.listenTls({{hostname:'127.0.0.1',port:0,cert,key,reusePort:true}}); throw Error('permission bypass'); }} catch (error) {{ if (error.name!=='NotCapable') throw error; }}"
    );
    assert_eq!(
        run_js(
            &dir,
            "denied.js",
            &denied,
            options(&dir, Some(vec!["net".into()]))
        ),
        0
    );

    let mut granted = options(&dir, None);
    granted.permissions.push("--allow-net=127.0.0.1".into());
    let normalized = format!(
        "{cert_js}globalThis.setterCalls=0; Object.defineProperty(Object.prototype,'reusePort',{{configurable:true,set(_value){{setterCalls++;}}}}); let reads=0; const opts={{hostname:'127.0.0.1',port:0,cert,key,get reusePort(){{reads++;return false;}}}}; Deno.listenTls(opts).close(); if (reads!==1) throw Error('reusePort getter read '+reads+' times'); if (setterCalls!==0) throw Error('normalization invoked inherited setter');"
    );
    assert_eq!(run_js(&dir, "allowed.js", &normalized, granted), 0);
    let _ = fs::remove_dir_all(dir);
}
