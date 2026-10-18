use libdeno::{run, LibdenoOptions};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct StalledModuleServer {
    url: String,
    requests: Arc<AtomicUsize>,
    stop: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl StalledModuleServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/0.js", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::spawn(move || loop {
            if stopped.try_recv().is_ok() {
                break;
            }
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
                    let mut request = [0; 1024];
                    let _ = stream.read(&mut request);
                    seen.fetch_add(1, Ordering::SeqCst);
                    if stopped.recv_timeout(Duration::from_secs(5)).is_ok() {
                        break;
                    }
                    let body = "import './1.js';";
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        });
        Self {
            url,
            requests,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for StalledModuleServer {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn shutting_down_cancels_a_worker_still_preloading_remote_modules() {
    const CHILD: &str = "LIBDENO_STALLED_WORKER_PRELOAD_CHILD";
    if std::env::var_os(CHILD).is_some() {
        for script in [
            "setTimeout(() => { throw new Error('root failure'); }, 250);",
            "setTimeout(() => Deno.exit(9), 250);",
        ] {
            let server = StalledModuleServer::start();
            let dir =
                std::env::temp_dir().join(format!("libdeno-preload-stop-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let entry = dir.join("main.js");
            std::fs::write(
                &entry,
                format!(
                    "new Worker({:?}, {{ type: 'module' }}); {script}",
                    server.url
                ),
            )
            .unwrap();
            let result = run(
                &entry,
                &LibdenoOptions {
                    allow_all_permissions: true,
                    ..Default::default()
                },
            );
            assert!(
                server.requests.load(Ordering::SeqCst) > 0,
                "worker never began remote preload"
            );
            if script.contains("Deno.exit") {
                assert_eq!(result.unwrap(), 9);
            } else {
                assert!(result.unwrap_err().to_string().contains("root failure"));
            }
            drop(server);
            let _ = std::fs::remove_dir_all(dir);
        }
        return;
    }

    use std::process::{Command, Stdio};
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "shutting_down_cancels_a_worker_still_preloading_remote_modules",
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
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            assert!(
                status.success(),
                "isolated preload test failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        if Instant::now() >= deadline {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            #[cfg(not(unix))]
            child.kill().unwrap();
            let _ = child.wait();
            panic!("worker preload cancellation exceeded 12 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
