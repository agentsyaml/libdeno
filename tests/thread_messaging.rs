use libdeno::{run, LibdenoOptions};
use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;

fn prepare(
    dir: &std::path::Path,
    label: &str,
    peer_ready: &std::path::Path,
    target_id: &std::path::Path,
    tail: &str,
) {
    fs::create_dir_all(dir).unwrap();
    let worker = r#"
const { parentPort, workerData, postMessageToThread, threadId } = require('node:worker_threads');
Deno.writeTextFileSync(workerData.idFile, String(threadId));
const send = value => postMessageToThread(0, value).then(() => parentPort.postMessage('sent:' + value));
process.on('workerMessage', value => { if (value === 'sibling-received') parentPort.postMessage('sibling-replied'); });
parentPort.on('message', async message => { if (message === 'start') { await send('payload-' + workerData.label); if (workerData.label === 'A') { while (true) { try { Deno.statSync(workerData.targetId); break; } catch { await new Promise(r => setTimeout(r, 5)); } } try { await postMessageToThread(Number(Deno.readTextFileSync(workerData.targetId)), 'cross-run', 100); parentPort.postMessage('cross-allowed'); } catch { parentPort.postMessage('cross-denied'); } } } else if (message.startsWith('sibling:')) { await postMessageToThread(Number(message.slice(8)), 'sibling-ping'); } else if (message === 'again') await send('payload-B-after-A'); });
"#;
    fs::write(dir.join("worker.js"), worker).unwrap();
    fs::write(dir.join("sibling.js"), "const { parentPort, workerData, threadId, postMessageToThread } = require('node:worker_threads'); parentPort.on('message', () => {}); process.on('workerMessage', value => { if (value === 'sibling-ping') postMessageToThread(Number(Deno.readTextFileSync(workerData.targetId)), 'sibling-received'); }); Deno.writeTextFileSync(workerData.idFile, String(threadId)); parentPort.postMessage('sibling-ready');").unwrap();
    fs::write(dir.join("main.js"), format!(
        "const {{ Worker, postMessageToThread }} = await import('node:worker_threads');\n\
         Deno.writeTextFileSync({:?}, 'ready'); while (true) {{ try {{ Deno.statSync({:?}); break; }} catch {{ await new Promise(r => setTimeout(r, 5)); }} }}\n\
         const received=new Set(); process.on('workerMessage', value=>received.add(value));\n\
         const wait=async value=>{{while(!received.has(value))await new Promise(r=>setTimeout(r,5));}};\n\
         const worker=new Worker(new URL('./worker.js',import.meta.url),{{workerData:{{label:{label:?},idFile:{:?},targetId:{:?}}}}});\n\
         const replies=new Set(); worker.on('message',value=>replies.add(value)); const waitReply=async value=>{{while(!replies.has(value))await new Promise(r=>setTimeout(r,5));}};\n\
         worker.postMessage('start'); await wait('payload-'+{label:?}); await waitReply('sent:payload-'+{label:?}); {tail}",
        dir.join("ready").display().to_string(), peer_ready.display().to_string(), dir.join("worker.id").display().to_string(), target_id.display().to_string()
    )).unwrap();
}

fn run_pair() {
    let root = std::env::temp_dir().join(format!("libdeno-thread-domain-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let a = root.join("a");
    let b = root.join("b");
    let a_done = root.join("a.done");
    let a_tail = "await waitReply('cross-denied'); Deno.exit(0);".to_string();
    let b_tail = format!(
        "const sibling=new Worker(new URL('./sibling.js',import.meta.url),{{workerData:{{idFile:{:?},targetId:{:?}}}}}); await new Promise((resolve,reject)=>{{sibling.once('message',resolve);sibling.once('error',reject);}}); worker.postMessage('sibling:'+Deno.readTextFileSync({:?})); await waitReply('sibling-replied');\n\
         while (true) {{ try {{ Deno.statSync({:?}); break; }} catch {{ await new Promise(r => setTimeout(r, 5)); }} }} worker.postMessage('again'); await wait('payload-B-after-A'); await waitReply('sent:payload-B-after-A'); Deno.exit(0);",
        b.join("sibling.id").display().to_string(), b.join("worker.id").display().to_string(), b.join("sibling.id").display().to_string(), a_done.display().to_string()
    );
    prepare(&a, "A", &b.join("ready"), &b.join("worker.id"), &a_tail);
    prepare(&b, "B", &a.join("ready"), &a.join("worker.id"), &b_tail);
    let barrier = Arc::new(Barrier::new(3));
    let start = |dir: std::path::PathBuf, barrier: Arc<Barrier>| {
        thread::spawn(move || {
            barrier.wait();
            run(
                dir.join("main.js"),
                &LibdenoOptions {
                    allow_all_permissions: true,
                    ..Default::default()
                },
            )
        })
    };
    let first = start(a.clone(), barrier.clone());
    let second = start(b.clone(), barrier.clone());
    barrier.wait();
    assert_eq!(first.join().unwrap().unwrap(), 0);
    fs::write(&a_done, "done").unwrap();
    assert_eq!(second.join().unwrap().unwrap(), 0);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn post_message_domains_are_run_local_and_survive_sibling_exit() {
    const CHILD: &str = "LIBDENO_THREAD_DOMAIN_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        run_pair();
        return;
    }
    use std::process::{Command, Stdio};
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "post_message_domains_are_run_local_and_survive_sibling_exit",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            assert!(
                status.success(),
                "isolated thread-domain case failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        if std::time::Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            #[cfg(not(unix))]
            child.kill().unwrap();
            let _ = child.wait();
            panic!("isolated thread-domain case exceeded 20 seconds");
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
}
