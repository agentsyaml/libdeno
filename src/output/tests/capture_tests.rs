use super::super::{active_capture_readers, SavedFd, StreamCapture};

#[cfg(unix)]
fn capped_output_reader_stops_after_collection(max_bytes: usize) {
    use std::os::fd::AsRawFd;
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let target = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
        .unwrap();
    let fd = target.as_raw_fd();
    let baseline = active_capture_readers();
    let capture = StreamCapture::new(fd, Some(max_bytes), SavedFd::duplicate(fd).unwrap()).unwrap();
    let overflow = capture.overflow.clone();
    let writer_fd = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    assert!(writer_fd >= 0);
    let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let block = [b'x'; 4096];
        let mut started = false;
        loop {
            let written = unsafe { libc::write(writer_fd, block.as_ptr().cast(), block.len()) };
            if written >= 0 {
                if !started {
                    started_tx.send(()).unwrap();
                    started = true;
                }
                continue;
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            unsafe { libc::close(writer_fd) };
            done_tx
                .send(error.kind() == std::io::ErrorKind::BrokenPipe)
                .unwrap();
            break;
        }
    });
    started_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    let error = capture.finish().unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    assert!(overflow.load(std::sync::atomic::Ordering::Relaxed));
    assert!(done_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap());
    writer.join().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while active_capture_readers() != baseline && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(active_capture_readers(), baseline);
}

#[cfg(unix)]
#[test]
fn capped_output_reader_stops_after_collection_in_isolated_child() {
    const CHILD: &str = "LIBDENO_CAPPED_OUTPUT_STOP_CHILD";
    if std::env::var(CHILD).ok().as_deref() == Some("1") {
        capped_output_reader_stops_after_collection(0);
        capped_output_reader_stops_after_collection(1);
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "output::tests::capture_tests::capped_output_reader_stops_after_collection_in_isolated_child",
        ])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            assert!(
                status.success(),
                "isolated capture-stop test failed: {output:?}"
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(stdout.contains("running 1 test") && stdout.contains("1 passed; 0 failed"));
            return;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("isolated capture-stop test timed out");
        }
        std::thread::yield_now();
    }
}
