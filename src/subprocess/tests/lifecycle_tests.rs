use super::super::*;
#[cfg(feature = "execution-control")]
use super::super::{
    peek_control_with_retry, start_supervisor_child_control_initial_idle_observed,
    start_supervisor_child_control_partial_observed, stop_supervisor_child_control,
    ControlPeekOutcome, InterruptedPeekAction, SupervisorCancellation,
};
use super::SUBPROCESS_OUTPUT_READER_BUDGET_TEST_LOCK;
#[cfg(feature = "execution-control")]
use crate::supervisor::{
    prepare_supervisor_stream, read_frame, write_frame, CancelReason, FrameDirection, FrameKind,
    SupervisorFrame, SUPERVISOR_MAGIC,
};
#[cfg(feature = "execution-control")]
use std::time::{Duration, Instant};

#[cfg(feature = "execution-control")]
#[test]
fn control_stop_wakes_idle_reader_without_cancellation() {
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    prepare_supervisor_stream(&peer).unwrap();
    let (receiver, _) = listener.accept().unwrap();
    prepare_supervisor_stream(&receiver).unwrap();
    let cancellation = SupervisorCancellation::new(
        crate::limits::CancellationContext::new(),
        CancelReason::User,
    );
    let (stop_tx, stop_rx) = mpsc::channel();
    let (idle_tx, idle_rx) = mpsc::channel();
    let control = start_supervisor_child_control_initial_idle_observed(
        &receiver,
        73,
        cancellation.clone(),
        stop_rx,
        idle_tx,
    )
    .unwrap();
    idle_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    stop_supervisor_child_control(&receiver, &stop_tx);
    control.join().unwrap().unwrap();
    assert!(!cancellation.is_requested());
}

#[test]
fn capped_reader_stops_after_collection_drop() {
    let _lock = SUBPROCESS_OUTPUT_READER_BUDGET_TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for max_bytes in [0, 1] {
        struct GatedReader {
            reads: usize,
            continued: std::sync::mpsc::Sender<()>,
            release: std::sync::mpsc::Receiver<()>,
            dropped: std::sync::mpsc::Sender<()>,
        }
        impl std::io::Read for GatedReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                if self.reads == 0 {
                    self.reads = 1;
                    buffer[..2].copy_from_slice(b"AB");
                    return Ok(2);
                }
                self.continued.send(()).unwrap();
                self.release
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
                buffer[0] = b'B';
                Ok(1)
            }
        }
        impl Drop for GatedReader {
            fn drop(&mut self) {
                self.dropped.send(()).unwrap();
            }
        }
        let baseline = active_subprocess_output_readers();
        let (continued_tx, continued_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        let reader = SubprocessOutputReader::spawn(
            GatedReader {
                reads: 0,
                continued: continued_tx,
                release: release_rx,
                dropped: dropped_tx,
            },
            max_bytes,
        )
        .unwrap();
        continued_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let capture = SubprocessOutputReaders {
            stdout: Some(reader),
            stderr: None,
        }
        .collect(Some(
            std::time::Instant::now() + std::time::Duration::from_millis(10),
        ));
        assert_eq!(
            capture.stdout,
            if max_bytes == 0 { &b""[..] } else { &b"A"[..] }
        );
        assert!(capture.truncated && capture.incomplete);
        assert!(!capture.reader_error);
        release_tx.send(()).unwrap();
        dropped_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while active_subprocess_output_readers() != baseline && std::time::Instant::now() < deadline
        {
            std::thread::yield_now();
        }
        assert_eq!(active_subprocess_output_readers(), baseline);
    }
}

#[cfg(feature = "execution-control")]
#[test]
fn control_peek_retries_interrupted_and_honors_stop() {
    let mut calls = 0;
    let ready = peek_control_with_retry(
        |_| {
            calls += 1;
            if calls == 1 {
                Err(std::io::ErrorKind::Interrupted.into())
            } else {
                Ok(1)
            }
        },
        || Ok(InterruptedPeekAction::Retry),
    )
    .unwrap();
    assert_eq!(ready, ControlPeekOutcome::Ready);
    assert_eq!(calls, 2);
    let mut calls = 0;
    let stopped = peek_control_with_retry(
        |_| {
            calls += 1;
            Err(std::io::ErrorKind::Interrupted.into())
        },
        || Ok(InterruptedPeekAction::Stopped),
    )
    .unwrap();
    assert_eq!(stopped, ControlPeekOutcome::Stopped);
    assert_eq!(calls, 1);
}

#[cfg(feature = "execution-control")]
#[test]
fn control_stop_wakes_partial_frame_and_preserves_terminal_write() {
    const CHILD: &str = "LIBDENO_CHILD_CONTROL_STOP_TEST";
    if std::env::var(CHILD).ok().as_deref() == Some("1") {
        use std::io::Write;
        use std::net::{TcpListener, TcpStream};
        use std::sync::mpsc;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        prepare_supervisor_stream(&peer).unwrap();
        let (mut receiver, _) = listener.accept().unwrap();
        prepare_supervisor_stream(&receiver).unwrap();
        let cancellation = SupervisorCancellation::new(
            crate::limits::CancellationContext::new(),
            CancelReason::User,
        );
        let (stop_tx, stop_rx) = mpsc::channel();
        let (partial_tx, partial_rx) = mpsc::channel();
        let control = start_supervisor_child_control_partial_observed(
            &receiver,
            73,
            cancellation.clone(),
            stop_rx,
            partial_tx,
        )
        .unwrap();
        peer.write_all(&SUPERVISOR_MAGIC[..1]).unwrap();
        partial_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        stop_supervisor_child_control(&receiver, &stop_tx);
        control.join().unwrap().unwrap();
        assert!(!cancellation.is_requested());
        write_frame(
            &mut receiver,
            &SupervisorFrame::new(FrameKind::Terminal, 73, Vec::new()).unwrap(),
        )
        .unwrap();
        let received = read_frame(
            &mut peer,
            FrameDirection::ChildToParent,
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(received.kind, FrameKind::Terminal);
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "subprocess::tests::lifecycle_tests::control_stop_wakes_partial_frame_and_preserves_terminal_write",
        ])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Windows may only observe the stop when the frame deadline expires.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            assert!(status.success(), "control-stop child failed: {output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!("control-stop child timed out: {output:?}");
        }
        std::thread::yield_now();
    }
}
