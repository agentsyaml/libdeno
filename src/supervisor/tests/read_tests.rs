use super::super::*;
use super::tcp_pair;

#[test]
fn retries_syscall_interruption_without_losing_partial_frame_bytes() {
    let (mut stream, _peer) = tcp_pair();
    let mut calls = 0;
    let mut requested = Vec::new();
    let mut bytes = [0; 3];
    read_until_with(&mut stream, &mut bytes, None, None, None, |_, buffer| {
        calls += 1;
        requested.push(buffer.len());
        match calls {
            1 => {
                buffer[0] = b'A';
                Ok(1)
            }
            2 => Err(io::ErrorKind::Interrupted.into()),
            _ => {
                buffer[..2].copy_from_slice(b"BC");
                Ok(2)
            }
        }
    })
    .unwrap();
    assert_eq!(&bytes, b"ABC");
    assert_eq!(requested, [3, 2, 2]);
}

#[test]
fn cancellation_interrupt_is_not_retried_as_a_syscall_interrupt() {
    let (mut stream, _peer) = tcp_pair();
    let cancellation = SupervisorCancellation::new(
        crate::limits::CancellationContext::new(),
        CancelReason::User,
    );
    let mut calls = 0;
    let error = read_until_with(
        &mut stream,
        &mut [0],
        None,
        Some(&cancellation),
        None,
        |_, _| {
            calls += 1;
            cancellation.request(CancelReason::User);
            Err(io::ErrorKind::Interrupted.into())
        },
    )
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    assert_eq!(calls, 1);
}
