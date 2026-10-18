#![cfg(unix)]

use libdeno::{run, ExtensionFactory, LibdenoOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

deno_core::extension!(broker_probe,
    ops = [op_install_permission_broker],
    esm_entry_point = "ext:broker_probe/init.js",
    esm = [dir "tests/permission_broker_require_probe", "init.js"],
);

#[deno_core::op2(fast)]
fn op_install_permission_broker(#[string] path: &str) {
    libdeno::install_permission_broker(path).unwrap();
}

struct BrokerServer {
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
    denied_target: Arc<AtomicBool>,
    denied_scope: Arc<AtomicBool>,
    denied_worker: Arc<AtomicBool>,
    denied_nested: Arc<AtomicBool>,
    denied_stat: Arc<AtomicBool>,
    denied_realpath: Arc<AtomicBool>,
    denied_main: Arc<AtomicBool>,
    denied_import: Arc<AtomicBool>,
}

impl BrokerServer {
    fn start(socket: &std::path::Path, denied_path: String) -> Self {
        let listener = UnixListener::bind(socket).unwrap();
        let denied_target = Arc::new(AtomicBool::new(false));
        let observed = denied_target.clone();
        let denied_scope_observed = Arc::new(AtomicBool::new(false));
        let scope_observed = denied_scope_observed.clone();
        let denied_worker = Arc::new(AtomicBool::new(false));
        let worker_observed = denied_worker.clone();
        let denied_nested = Arc::new(AtomicBool::new(false));
        let nested_observed = denied_nested.clone();
        let denied_stat = Arc::new(AtomicBool::new(false));
        let stat_observed = denied_stat.clone();
        let denied_realpath = Arc::new(AtomicBool::new(false));
        let realpath_observed = denied_realpath.clone();
        let denied_main = Arc::new(AtomicBool::new(false));
        let main_observed = denied_main.clone();
        let denied_import = Arc::new(AtomicBool::new(false));
        let import_observed = denied_import.clone();
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let mut target_checks = std::collections::HashMap::<String, usize>::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) =>
                    {
                        if stopped.try_recv().is_ok() {
                            break;
                        }
                        continue;
                    }
                    Err(_) => break,
                    Ok(_) => {}
                }
                let request: deno_core::serde_json::Value =
                    deno_core::serde_json::from_str(line.trim()).unwrap();
                let id = request["id"].as_u64().unwrap();
                let target = request["value"]
                    .as_str()
                    .filter(|value| value.ends_with(&denied_path));
                let denied_scope = request["value"]
                    .as_str()
                    .is_some_and(|value| value.ends_with("/denied-scope/package.json"));
                let denied_import = request["value"]
                    .as_str()
                    .is_some_and(|value| value.ends_with("/imports-referrer.cjs"));
                let denied = denied_scope
                    || denied_import
                    || target.is_some_and(|value| {
                        let count = target_checks.entry(value.to_string()).or_default();
                        *count += 1;
                        let limit = if value.ends_with("stat-pkg.json") {
                            1
                        } else if value.ends_with("realpath-pkg.json") {
                            2
                        } else {
                            3
                        };
                        *count >= limit
                    });
                if denied
                    && request["value"]
                        .as_str()
                        .is_some_and(|value| value.ends_with("/worker-pkg.json"))
                {
                    worker_observed.store(true, Ordering::SeqCst);
                }
                if denied
                    && request["value"]
                        .as_str()
                        .is_some_and(|value| value.ends_with("/nested-pkg.json"))
                {
                    nested_observed.store(true, Ordering::SeqCst);
                }
                if denied
                    && request["value"]
                        .as_str()
                        .is_some_and(|value| value.ends_with("/stat-pkg.json"))
                {
                    stat_observed.store(true, Ordering::SeqCst);
                }
                if denied
                    && request["value"]
                        .as_str()
                        .is_some_and(|value| value.ends_with("/realpath-pkg.json"))
                {
                    realpath_observed.store(true, Ordering::SeqCst);
                }
                if denied
                    && request["value"]
                        .as_str()
                        .is_some_and(|value| value.ends_with("/pkg.json"))
                {
                    main_observed.store(true, Ordering::SeqCst);
                }
                if denied {
                    observed.store(true, Ordering::SeqCst);
                }
                if denied_scope {
                    scope_observed.store(true, Ordering::SeqCst);
                }
                if denied_import && denied {
                    import_observed.store(true, Ordering::SeqCst);
                }
                let result = if denied { "deny" } else { "allow" };
                writeln!(stream, "{{\"id\":{id},\"result\":\"{result}\"}}").unwrap();
            }
        });
        Self {
            stop,
            thread: Some(thread),
            denied_target,
            denied_scope: denied_scope_observed,
            denied_worker,
            denied_nested,
            denied_stat,
            denied_realpath,
            denied_main,
            denied_import,
        }
    }
}

impl Drop for BrokerServer {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn installed_broker_denies_cjs_json_require_despite_allow_all() {
    let dir = std::env::temp_dir().join(format!("libdeno-broker-require-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("pkg.json"), r#"{"secret":"must-not-load"}"#).unwrap();
    std::fs::write(dir.join("stat-pkg.json"), "{}").unwrap();
    std::fs::write(dir.join("realpath-pkg.json"), "{}").unwrap();
    std::fs::write(dir.join("worker-pkg.json"), r#"{"secret":"worker"}"#).unwrap();
    std::fs::write(dir.join("nested-pkg.json"), r#"{"secret":"nested"}"#).unwrap();
    std::fs::write(dir.join("imports-referrer.cjs"), "module.exports = 1;").unwrap();
    std::fs::create_dir_all(dir.join("imports-app")).unwrap();
    std::fs::write(
        dir.join("imports-app/package.json"),
        r##"{"imports":{"#alias":"./target.cjs"}}"##,
    )
    .unwrap();
    std::fs::write(dir.join("imports-app/referrer.cjs"), "module.exports = 1;").unwrap();
    std::fs::write(dir.join("imports-app/target.cjs"), "module.exports = 2;").unwrap();
    std::fs::create_dir_all(dir.join("linked")).unwrap();
    std::fs::write(
        dir.join("linked/package-data"),
        r#"{"type":"commonjs","main":"index.js"}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink("package-data", dir.join("linked/package.json")).unwrap();
    std::fs::write(
        dir.join("linked/index.js"),
        "module.exports = 'symlink-package';",
    )
    .unwrap();
    std::fs::write(
        dir.join("nested.mjs"),
        r#"
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
try { require('./nested-pkg.json'); postMessage(false); }
catch { postMessage(true); }
"#,
    )
    .unwrap();
    std::fs::write(dir.join("worker.mjs"), r#"
import { createRequire } from 'node:module';
const require = createRequire(import.meta.url);
const denied = () => { try { require('./worker-pkg.json'); return false; } catch { return true; } };
self.onmessage = () => {
  const nested = new Worker(new URL('./nested.mjs', import.meta.url), { type: 'module', deno: { permissions: { read: true } } });
  nested.onmessage = event => postMessage({ worker: denied(), nested: event.data });
};
    "#).unwrap();
    std::fs::create_dir_all(dir.join("denied-scope")).unwrap();
    std::fs::write(
        dir.join("denied-scope/package.json"),
        r#"{"type":"commonjs","main":"index.js"}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("denied-scope/index.js"),
        "module.exports = 'scope';",
    )
    .unwrap();
    let socket = dir.join("broker.sock");
    let entry = dir.join("main.mjs");
    let script =
         "import { createRequire } from 'node:module';\n\
         const require = createRequire(import.meta.url);\n\
         const descriptor = { name: 'read', path: new URL(import.meta.url).pathname };\n\
         if ((await Deno.permissions.query(descriptor)).state !== 'granted') throw Error('read query changed');\n\
         if ((await Deno.permissions.request(descriptor)).state !== 'granted') throw Error('read request changed');\n\
         globalThis.__installPermissionBroker('__BROKER_SOCKET__');\n\
         if ((await Deno.permissions.query(descriptor)).state !== 'granted') throw Error('broker changed read query');\n\
         if ((await Deno.permissions.request(descriptor)).state !== 'granted') throw Error('broker changed read request');\n\
         try { require('./pkg.json'); throw Error('main require bypassed broker'); } catch (error) { if (error.message === 'main require bypassed broker') throw error; }\n\
         for (const name of ['./stat-pkg.json', './realpath-pkg.json']) { try { require(name); throw Error(name + ' bypassed broker'); } catch (error) { if (error.message.includes('bypassed broker')) throw error; } }\n\
         try { globalThis.__brokerPackageScope(new URL('./denied-scope/package.json', import.meta.url).pathname); throw Error('package-scope read bypassed broker'); } catch (error) { if (error.message === 'package-scope read bypassed broker') throw error; }\n\
         if (globalThis.__brokerPackageScope(new URL('./linked/package.json', import.meta.url).pathname)?.main !== 'index.js') throw Error('package.json symlink was not read');\n\
         try { globalThis.__brokerPackageImports(new URL('./imports-referrer.cjs', import.meta.url).pathname, '#preserved-request'); throw Error('package-import referrer bypassed broker'); } catch (error) { if (error.message === 'package-import referrer bypassed broker') throw error; }\n\
         const mapped = globalThis.__brokerPackageImports(new URL('./imports-app/referrer.cjs', import.meta.url).pathname, '#alias');\n\
         if (!mapped?.endsWith('/imports-app/target.cjs')) throw Error('package-import request was not preserved');\n\
         if (require('./linked') !== 'symlink-package') throw Error('package.json symlink was not followed');\n\
         const worker = new Worker(new URL('./worker.mjs', import.meta.url), { type: 'module', deno: { permissions: { read: true } } });\n\
         const result = await new Promise((resolve, reject) => { worker.onmessage = event => resolve(event.data); worker.onerror = reject; worker.postMessage(null); });\n\
         if (!result.worker || !result.nested) throw Error('worker require escaped the broker');\n\
          worker.terminate();";
    std::fs::write(
        &entry,
        script.replace("__BROKER_SOCKET__", socket.to_str().unwrap()),
    )
    .unwrap();
    let broker = BrokerServer::start(&socket, "pkg.json".to_string());

    let status = run(
        &entry,
        &LibdenoOptions {
            allow_all_permissions: true,
            extensions: Some(ExtensionFactory::new(|| vec![broker_probe::init()])),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        broker.denied_target.load(Ordering::SeqCst),
        "the broker never denied the require source read"
    );
    assert!(
        broker.denied_scope.load(Ordering::SeqCst),
        "the broker never received the package-scope check"
    );
    assert!(
        broker.denied_worker.load(Ordering::SeqCst),
        "the explicit-read worker's source read did not reach the broker"
    );
    assert!(
        broker.denied_nested.load(Ordering::SeqCst),
        "the nested worker's source read did not reach the broker"
    );
    assert!(
        broker.denied_stat.load(Ordering::SeqCst),
        "require stat did not reach the broker"
    );
    assert!(
        broker.denied_realpath.load(Ordering::SeqCst),
        "require realpath did not reach the broker"
    );
    assert!(
        broker.denied_main.load(Ordering::SeqCst),
        "main CJS source read did not reach the broker"
    );
    assert!(
        broker.denied_import.load(Ordering::SeqCst),
        "package-import referrer did not reach the broker"
    );
    assert_eq!(status, 0);
    drop(broker);
    let _ = std::fs::remove_dir_all(dir);
}
