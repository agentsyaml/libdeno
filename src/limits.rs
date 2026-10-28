//! Resource limits, execution deadlines, and Node IPC startup handoff.

use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

#[cfg(test)]
thread_local! {
    static FAIL_NEXT_WATCHDOG_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(unix)]
    static FAIL_NEXT_NODE_IPC_FCNTL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(windows)]
    static FAIL_NEXT_NODE_IPC_CLONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
#[cfg(feature = "execution-control")]
fn fail_next_watchdog_spawn() {
    FAIL_NEXT_WATCHDOG_SPAWN.with(|flag| flag.set(true));
}

fn watchdog_spawn<T>(spawn: impl FnOnce() -> std::io::Result<T>) -> std::io::Result<T> {
    #[cfg(test)]
    if FAIL_NEXT_WATCHDOG_SPAWN.with(|flag| flag.replace(false)) {
        return Err(std::io::Error::other("injected watchdog spawn failure"));
    }
    spawn()
}

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
#[cfg(windows)]
use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle};

use deno_core::v8;
use deno_core::v8::IsolateHandle;
use deno_core::ModuleSpecifier;
use deno_runtime::deno_node::ops::ipc::ChildIpcSerialization;
use deno_runtime::worker::MainWorker;

use crate::timing::{ExecutionTiming, Phase};

#[cfg(feature = "execution-control")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CancellationReason {
    User,
    Deadline,
    Shutdown,
}

/// Crate-private best-effort cancellation bridge for experimental executor
/// submissions. A request can arrive before the isolate exists, so the hook
/// stores the flag and registers the isolate handle once bootstrap reaches the
/// execution boundary.
#[derive(Clone)]
pub(crate) struct CancellationContext {
    requested: Arc<AtomicBool>,
    isolate: Arc<Mutex<Option<IsolateHandle>>>,
    notify: Arc<tokio::sync::Notify>,
    #[cfg(feature = "execution-control")]
    reason: Arc<Mutex<Option<CancellationReason>>>,
}

impl CancellationContext {
    #[cfg(feature = "execution-control")]
    pub(crate) fn new() -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(false)),
            isolate: Arc::new(Mutex::new(None)),
            notify: Arc::new(tokio::sync::Notify::new()),
            reason: Arc::new(Mutex::new(None)),
        }
    }

    #[cfg(feature = "execution-control")]
    pub(crate) fn request_with_reason(&self, reason: CancellationReason) {
        self.reason
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_or_insert(reason);
        self.requested.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
        let isolate = self
            .isolate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(isolate) = isolate {
            isolate.terminate_execution();
        }
    }

    #[cfg(feature = "execution-control")]
    pub(crate) fn reason(&self) -> Option<CancellationReason> {
        *self
            .reason
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::SeqCst)
    }

    async fn wait_requested(&self) {
        loop {
            if self.is_requested() {
                return;
            }
            let notified = self.notify.notified();
            if self.is_requested() {
                return;
            }
            notified.await;
        }
    }

    fn register(&self, isolate: IsolateHandle) {
        let should_terminate = self.is_requested();
        self.isolate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .replace(isolate.clone());
        if should_terminate {
            isolate.terminate_execution();
        }
    }

    fn clear(&self) {
        self.isolate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

#[cfg(feature = "execution-control")]
/// Requests deadline cancellation at one absolute instant while in-process
/// executor bootstrap is still running. The worker-side deadline timer starts
/// later, at the V8 boundary; this guard closes that gap without changing the
/// legacy duration-based entry points.
pub(crate) struct AbsoluteDeadlineGuard {
    stop: Option<std::sync::mpsc::Sender<()>>,
    join: Option<std::thread::JoinHandle<()>>,
}

#[cfg(feature = "execution-control")]
impl AbsoluteDeadlineGuard {
    pub(crate) fn new(
        deadline: Option<Instant>,
        cancellation: Option<CancellationContext>,
    ) -> std::io::Result<Self> {
        let Some(deadline) = deadline else {
            return Ok(Self {
                stop: None,
                join: None,
            });
        };
        let Some(cancellation) = cancellation else {
            return Ok(Self {
                stop: None,
                join: None,
            });
        };
        if deadline <= Instant::now() {
            cancellation.request_with_reason(CancellationReason::Deadline);
            return Ok(Self {
                stop: None,
                join: None,
            });
        }
        let (stop, receiver) = std::sync::mpsc::channel();
        let cancellation_for_thread = cancellation.clone();
        let join = watchdog_spawn(|| {
            std::thread::Builder::new()
                .name("libdeno-submission-deadline".to_string())
                .spawn(move || {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if receiver.recv_timeout(remaining).is_err() {
                        cancellation_for_thread.request_with_reason(CancellationReason::Deadline);
                    }
                })
        })?;
        Ok(Self {
            stop: Some(stop),
            join: Some(join),
        })
    }
}

#[cfg(feature = "execution-control")]
impl Drop for AbsoluteDeadlineGuard {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// V8's own resource-constraint tests use an 8 MiB old generation as the
/// smallest deliberately constrained isolate. Rejecting smaller values keeps
/// the option from silently becoming an unusable V8 configuration while still
/// avoiding an embedder-invented upper policy limit.
const MIN_V8_OLD_GENERATION_BYTES: usize = 8 << 20;

/// Validates the optional old-generation heap cap before any permission or V8
/// setup. `None` keeps V8's defaults; there is no arbitrary upper policy cap,
/// but zero/small values and the `usize` sentinel are rejected explicitly.
pub(crate) fn validate_max_heap_bytes(
    max_heap_bytes: Option<usize>,
) -> Result<(), crate::LibdenoError> {
    let Some(bytes) = max_heap_bytes else {
        return Ok(());
    };
    if bytes < MIN_V8_OLD_GENERATION_BYTES {
        return Err(crate::LibdenoError::Configuration(format!(
            "max_heap_bytes={bytes} is too small; use at least \
             {MIN_V8_OLD_GENERATION_BYTES} bytes for a V8 old-generation limit"
        )));
    }
    // The V8 entry point takes `usize`; there is no unit conversion or policy
    // ceiling here. Reject only the one value that cannot be a finite budget.
    if bytes == usize::MAX {
        return Err(crate::LibdenoError::Configuration(
            "max_heap_bytes=usize::MAX cannot be represented as a finite V8 heap budget"
                .to_string(),
        ));
    }
    Ok(())
}

/// V8 isolate creation parameters for a heap cap.
///
/// Maps `max_heap_bytes` to the V8 old-generation limit — the same constraint
/// the CLI applies for `--v8-flags=--max-old-space-size=N` (bytes, whereas the
/// flag takes MB). `WorkerOptions.create_params` feeds these into isolate
/// creation; V8 keeps its defaults for the young generation, initial sizes and
/// code range — only the hard ceiling is pinned. When the heap approaches the
/// cap V8 runs repeated GCs and eventually aborts with out-of-memory.
pub(crate) fn isolate_create_params(max_heap_bytes: Option<usize>) -> Option<v8::CreateParams> {
    max_heap_bytes
        .map(|bytes| v8::CreateParams::default().set_max_old_generation_size_in_bytes(bytes))
}

/// Drives `fut` (the worker run) to completion with an optional hard deadline.
///
/// Returns `Ok(result)` when the run finished before the deadline and
/// `Err(deadline)` when the deadline fired — including a run that only
/// returned because the timeout force-terminated it.
///
/// The terminator is a dedicated OS thread: a tokio timer task can never fire
/// while a busy JS loop is executing, because the current-thread runtime only
/// polls the task that is currently inside the V8 call. `terminate_execution`
/// from another thread is the documented V8 mechanism to interrupt running
/// JavaScript: it throws an uncatchable termination error at the next stack
/// check, `run_event_loop` returns, the future unwinds and the run's lease
/// is released. The `deadline + GRACE` outer timeout additionally bounds the
/// case where the event loop was idle (parked on a far-future timer, with no
/// JS running to throw into); dropping the future then is safe because no JS
/// frames are on the stack.
///
/// The deadline cannot cut through a blocking syscall: `terminate_execution`
/// only fires at the next JS stack check, so a script stuck in a blocking
/// syscall (an NFS-hung file read, a synchronous `Deno.Command` wait) unwinds
/// only when the syscall itself returns — the run may exceed the deadline by
/// the syscall's duration. This is a V8/runtime boundary, not fixable in the
/// embedder.
#[derive(Debug)]
pub(crate) enum ExecutionTermination {
    Deadline(Duration),
    Cancelled,
    Infrastructure(std::io::Error),
}

/// Rejects a finite deadline that cannot be represented by the host clock.
/// Treating the same value as "no deadline" on one backend and "immediate
/// timeout" on another is worse than rejecting the invalid configuration.
pub(crate) fn validate_execution_deadline(
    deadline: Option<Duration>,
) -> Result<(), crate::LibdenoError> {
    if deadline.is_some_and(|duration| Instant::now().checked_add(duration).is_none()) {
        return Err(crate::LibdenoError::Configuration(
            "execution deadline is too large for the host clock".to_string(),
        ));
    }
    Ok(())
}

pub(crate) async fn run_with_deadline_cancellable<F, T, E>(
    fut: F,
    deadline: Option<Duration>,
    isolate_handle: IsolateHandle,
    cancellation: Option<CancellationContext>,
    worker_lifecycle: crate::worker_lifecycle::WorkerLifecycleRegistry,
) -> Result<Result<T, E>, ExecutionTermination>
where
    F: Future<Output = Result<T, E>>,
{
    run_with_termination(
        fut,
        deadline,
        Some(isolate_handle),
        cancellation,
        Some(worker_lifecycle),
    )
    .await
}

async fn run_with_termination<F, T, E>(
    fut: F,
    deadline: Option<Duration>,
    isolate_handle: Option<IsolateHandle>,
    cancellation: Option<CancellationContext>,
    worker_lifecycle: Option<crate::worker_lifecycle::WorkerLifecycleRegistry>,
) -> Result<Result<T, E>, ExecutionTermination>
where
    F: Future<Output = Result<T, E>>,
{
    const GRACE: Duration = Duration::from_secs(2);

    if deadline.is_none() && cancellation.is_none() {
        return Ok(fut.await);
    }

    // An already-expired budget means "do not execute". Without this the
    // future is still polled once (the watchdog needs a scheduling turn and
    // the select below allows GRACE), so a fast script could run to completion
    // or leave side effects despite a zero deadline. Executor submissions
    // reject the same input before dispatch; keep both entry points equal.
    if deadline.is_some_and(|duration| duration.is_zero()) {
        return Err(ExecutionTermination::Deadline(Duration::ZERO));
    }

    if let (Some(cancellation), Some(isolate_handle)) = (&cancellation, &isolate_handle) {
        cancellation.register(isolate_handle.clone());
    }

    // Signal channel: if the run finishes first we send on it so the waiter
    // exits without terminating (harmless either way — terminate_execution
    // returns false on a dropped isolate — but this frees the thread at once
    // instead of leaving it asleep for the rest of the deadline).
    let deadline_fired = Arc::new(AtomicBool::new(false));
    let cancellation_fired = Arc::new(AtomicBool::new(false));
    let cancellation_cleanup = cancellation.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let terminator = {
        let deadline_fired = deadline_fired.clone();
        let cancellation_fired = cancellation_fired.clone();
        let cancellation_for_thread = cancellation.clone();
        let isolate_for_thread = isolate_handle.clone();
        let workers_for_thread = worker_lifecycle.clone();
        let deadline_at = deadline.and_then(|duration| Instant::now().checked_add(duration));
        match watchdog_spawn(|| {
            std::thread::Builder::new()
                .name("libdeno-execution-deadline".into())
                .spawn(move || loop {
                    if cancellation_for_thread
                        .as_ref()
                        .is_some_and(CancellationContext::is_requested)
                    {
                        cancellation_fired.store(true, Ordering::SeqCst);
                        if let Some(workers) = &workers_for_thread {
                            workers.close();
                        }
                        if let Some(isolate) = &isolate_for_thread {
                            isolate.terminate_execution();
                        }
                        break;
                    }
                    let wait = deadline_at
                        .map(|deadline| {
                            deadline
                                .saturating_duration_since(Instant::now())
                                .min(Duration::from_millis(10))
                        })
                        .unwrap_or_else(|| Duration::from_millis(10));
                    match done_rx.recv_timeout(wait) {
                        Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                            if deadline_at.is_some_and(|deadline| deadline <= Instant::now()) {
                                deadline_fired.store(true, Ordering::SeqCst);
                                if let Some(workers) = &workers_for_thread {
                                    workers.close();
                                }
                                if let Some(isolate) = &isolate_for_thread {
                                    isolate.terminate_execution();
                                }
                                break;
                            }
                        }
                    }
                })
        }) {
            Ok(thread) => thread,
            Err(error) => {
                if let Some(cancellation) = &cancellation_cleanup {
                    cancellation.clear();
                }
                return Err(ExecutionTermination::Infrastructure(error));
            }
        }
    };

    tokio::pin!(fut);
    let mut result = None;
    let mut termination = None;
    match (deadline, cancellation.as_ref()) {
        (Some(deadline), Some(cancellation)) => {
            tokio::select! {
                completed = tokio::time::timeout(deadline.saturating_add(GRACE), &mut fut) => {
                    match completed {
                        Ok(completed) => result = Some(completed),
                        Err(_) => termination = Some(ExecutionTermination::Deadline(deadline)),
                    }
                }
                _ = cancellation.wait_requested() => {
                    // Cancellation-only idle work gets a bounded best-effort
                    // grace after the request, not from submission start.
                    match tokio::time::timeout(GRACE, &mut fut).await {
                        Ok(completed) => result = Some(completed),
                        Err(_) => termination = Some(ExecutionTermination::Cancelled),
                    }
                }
            }
        }
        (Some(deadline), None) => {
            match tokio::time::timeout(deadline.saturating_add(GRACE), &mut fut).await {
                Ok(completed) => result = Some(completed),
                Err(_) => termination = Some(ExecutionTermination::Deadline(deadline)),
            }
        }
        (None, Some(cancellation)) => {
            tokio::select! {
                completed = &mut fut => result = Some(completed),
                _ = cancellation.wait_requested() => {
                    // Cancellation-only must also be able to leave an idle
                    // event loop parked on a far-future timer. This is a
                    // bounded best-effort grace, not a claim that blocking
                    // native/syscall/broker work is interruptible.
                    match tokio::time::timeout(GRACE, &mut fut).await {
                        Ok(completed) => result = Some(completed),
                        Err(_) => termination = Some(ExecutionTermination::Cancelled),
                    }
                }
            }
        }
        (None, None) => unreachable!("unbounded execution returned before fast path"),
    }

    let _ = done_tx.send(());
    let _ = terminator.join();
    let cancellation_requested = cancellation
        .as_ref()
        .is_some_and(CancellationContext::is_requested);

    if let Some(cancellation) = &cancellation {
        cancellation.clear();
    }

    // V8 can return its termination error in the same poll in which the
    // cancellation notification becomes ready. Treat that result as the
    // cancellation that caused it rather than leaking a low-level Core error.
    if cancellation_requested {
        return Err(ExecutionTermination::Cancelled);
    }

    if let Some(termination) = termination {
        return Err(termination);
    }
    let result = result.expect("execution must either complete or terminate");

    // `result` alone cannot tell whether the future unwound because the script
    // finished or because the deadline interrupted it; the flag set by the
    // terminator disambiguates for the caller's timeout error.
    //
    // Known small race: a script that completes exactly at the deadline can be
    // reported as timed out if the terminator thread set `fired` in the
    // instant before the completed result was observed. Safety-biased (a false
    // timeout is observable by the caller; a missed deadline is not) and
    // accepted.
    if cancellation_fired.load(Ordering::SeqCst) {
        Err(ExecutionTermination::Cancelled)
    } else if deadline_fired.load(Ordering::SeqCst) {
        Err(ExecutionTermination::Deadline(
            deadline.unwrap_or(Duration::ZERO),
        ))
    } else {
        Ok(result)
    }
}

/// Runs the standard worker lifecycle (main module, event loop, load/unload/
/// exit events) under an optional execution deadline.
pub(crate) async fn run_worker_cancellable(
    worker: &mut MainWorker,
    main_module: &ModuleSpecifier,
    execution_deadline: Option<Duration>,
    isolate_handle: IsolateHandle,
    timing: ExecutionTiming,
    cancellation: Option<CancellationContext>,
    worker_lifecycle: crate::worker_lifecycle::WorkerLifecycleRegistry,
) -> Result<Result<(), crate::LibdenoError>, ExecutionTermination> {
    let _user_execution = timing.span(Phase::UserExecution);
    let op_state = worker.js_runtime.op_state();
    let result = {
        let run = async {
            worker.execute_main_module(main_module).await?;
            worker.dispatch_load_event()?;
            loop {
                worker.run_event_loop(false).await?;
                if worker.dispatch_beforeunload_event()? {
                    continue;
                }
                if worker.dispatch_process_beforeexit_event()? {
                    continue;
                }
                break;
            }
            worker.dispatch_unload_event()?;
            worker.dispatch_process_exit_event()?;
            Ok::<(), crate::LibdenoError>(())
        };
        tokio::pin!(run);
        let run = std::future::poll_fn(|cx| {
            let watcher_exited = || {
                op_state
                    .borrow()
                    .try_borrow::<deno_runtime::deno_os::WatcherExited>()
                    .is_some()
            };
            if watcher_exited() {
                return std::task::Poll::Ready(Ok(()));
            }
            let polled = run.as_mut().poll(cx);
            if watcher_exited() {
                std::task::Poll::Ready(Ok(()))
            } else {
                polled
            }
        });
        run_with_deadline_cancellable(
            run,
            execution_deadline,
            isolate_handle,
            cancellation,
            worker_lifecycle,
        )
        .await
    };
    worker.run_napi_ref_finalizers();
    result
}

#[cfg(unix)]
type OwnedIpc = OwnedFd;
#[cfg(windows)]
type OwnedIpc = OwnedHandle;
type NodeIpcChannel = (OwnedIpc, ChildIpcSerialization);
type NodeIpcState = Option<Option<NodeIpcChannel>>;

// Outer None means no attempt has occurred; every attempt records Some(None)
// before reading the environment, so even a failed attempt is terminal.
static NODE_IPC: OnceLock<Mutex<NodeIpcState>> = OnceLock::new();

fn begin_node_ipc_attempt(state: &mut NodeIpcState) -> bool {
    if state.is_some() {
        false
    } else {
        *state = Some(None);
        true
    }
}

/// Explicit startup handoff for a trusted launcher-provided Node IPC channel.
/// The launcher must provide a valid raw OS handle with unique ownership, not
/// already claimed by another library. Ordinary hosts should call this too:
/// absent NODE_CHANNEL_FD records an initialized no-IPC state.
/// Initialization is one-attempt even when it returns an error. A parseable
/// raw handle is claimed before mode/object/flag validation; later failures
/// close it, so callers must not retry or reclaim it. An unparseable value has
/// no identifiable OS object to claim, but still consumes the one attempt.
///
/// # Safety
/// When NODE_CHANNEL_FD is present, the caller guarantees it names a valid IPC
/// endpoint with unique ownership, transferred exclusively to libdeno.
pub unsafe fn initialize_node_ipc_from_env() -> Result<(), crate::LibdenoError> {
    let state = NODE_IPC.get_or_init(|| Mutex::new(None));
    let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
    if !begin_node_ipc_attempt(&mut state) {
        return Err(crate::LibdenoError::Configuration(
            "Node IPC initialization may only be performed once".into(),
        ));
    }
    let raw = match std::env::var("NODE_CHANNEL_FD") {
        Ok(raw) => raw,
        Err(std::env::VarError::NotPresent) => return Ok(()),
        Err(error) => {
            return Err(crate::LibdenoError::Configuration(format!(
                "invalid NODE_CHANNEL_FD: {error}"
            )))
        }
    };
    // Claim a parseable raw handle before any subsequent fallible validation.
    // Its Owned* guard closes it if mode/object/flag checks fail.
    let owned = claim_node_ipc(&raw)?;
    let mode = match std::env::var("NODE_CHANNEL_SERIALIZATION_MODE") {
        Ok(mode) => Some(mode),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => {
            return Err(crate::LibdenoError::Configuration(format!(
                "invalid NODE_CHANNEL_SERIALIZATION_MODE: {error}"
            )))
        }
    };
    let serialization = match mode.as_deref() {
        None | Some("json") => ChildIpcSerialization::Json,
        Some("advanced") => ChildIpcSerialization::Advanced,
        Some(other) => {
            return Err(crate::LibdenoError::Configuration(format!(
                "invalid NODE_CHANNEL_SERIALIZATION_MODE: {other}"
            )))
        }
    };
    *state = Some(Some((owned, serialization)));
    Ok(())
}

/// Consumes the startup channel once; runs never inspect the process environment.
pub(crate) fn node_ipc_init() -> Option<(i64, ChildIpcSerialization)> {
    let mut state = NODE_IPC
        .get()?
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    take_node_ipc(&mut state)
}

fn take_node_ipc(state: &mut NodeIpcState) -> Option<(i64, ChildIpcSerialization)> {
    state.as_mut()?.take().map(|(handle, serialization)| {
        #[cfg(unix)]
        let raw = handle.into_raw_fd() as i64;
        #[cfg(windows)]
        let raw = handle.into_raw_handle() as usize as i64;
        (raw, serialization)
    })
}

fn claim_node_ipc(raw: &str) -> Result<OwnedIpc, crate::LibdenoError> {
    let fd = parse_node_channel_fd(raw)
        .ok_or_else(|| crate::LibdenoError::Configuration("invalid NODE_CHANNEL_FD".into()))?;
    #[cfg(unix)]
    {
        // SAFETY: the public initializer's contract transfers a valid unique descriptor.
        let owned = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        let socket_type = unsafe {
            let mut value: libc::c_int = 0;
            let mut len = std::mem::size_of_val(&value) as libc::socklen_t;
            if libc::getsockopt(
                owned.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_TYPE,
                &mut value as *mut _ as *mut _,
                &mut len,
            ) != 0
            {
                return Err(crate::LibdenoError::Io(std::io::Error::last_os_error()));
            }
            value
        };
        if socket_type != libc::SOCK_STREAM {
            return Err(crate::LibdenoError::Configuration(
                "NODE_CHANNEL_FD must be a stream socket".into(),
            ));
        }
        let domain = unsafe {
            let mut address: libc::sockaddr_storage = std::mem::zeroed();
            let mut len = std::mem::size_of_val(&address) as libc::socklen_t;
            if libc::getsockname(
                owned.as_raw_fd(),
                &mut address as *mut _ as *mut _,
                &mut len,
            ) != 0
            {
                return Err(crate::LibdenoError::Io(std::io::Error::last_os_error()));
            }
            address.ss_family as libc::c_int
        };
        if domain != libc::AF_UNIX {
            return Err(crate::LibdenoError::Configuration(
                "NODE_CHANNEL_FD must be a local Unix socket".into(),
            ));
        }
        unsafe {
            let mut peer: libc::sockaddr_storage = std::mem::zeroed();
            let mut len = std::mem::size_of_val(&peer) as libc::socklen_t;
            if libc::getpeername(owned.as_raw_fd(), &mut peer as *mut _ as *mut _, &mut len) != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ENOTCONN) {
                    return Err(crate::LibdenoError::Configuration(
                        "NODE_CHANNEL_FD must be a connected local Unix socket".into(),
                    ));
                }
                return Err(crate::LibdenoError::Io(error));
            }
            if peer.ss_family as libc::c_int != libc::AF_UNIX {
                return Err(crate::LibdenoError::Configuration(
                    "NODE_CHANNEL_FD peer must be a local Unix socket".into(),
                ));
            }
        }
        set_node_ipc_cloexec(owned.as_raw_fd())?;
        Ok(owned)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: ownership is exclusively transferred per the initializer contract.
        let original = unsafe { OwnedHandle::from_raw_handle(fd as usize as *mut _) };
        let duplicate =
            duplicate_and_close_node_handle(original).map_err(crate::LibdenoError::Io)?;
        if unsafe { windows_node_ipc::is_pipe(duplicate.as_raw_handle()) } {
            Ok(duplicate)
        } else {
            Err(crate::LibdenoError::Configuration(
                "NODE_CHANNEL_FD must be a pipe handle".into(),
            ))
        }
    }
}

#[cfg(windows)]
fn duplicate_and_close_node_handle(original: OwnedHandle) -> std::io::Result<OwnedHandle> {
    #[cfg(test)]
    if FAIL_NEXT_NODE_IPC_CLONE.with(|flag| flag.replace(false)) {
        return Err(std::io::Error::other(
            "injected Node IPC handle clone failure",
        ));
    }
    let duplicate = original.try_clone()?;
    drop(original);
    Ok(duplicate)
}

#[cfg(unix)]
fn set_node_ipc_cloexec(fd: std::os::fd::RawFd) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_NEXT_NODE_IPC_FCNTL.with(|flag| flag.replace(false)) {
        return Err(std::io::Error::other("injected Node IPC fcntl failure"));
    }
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
mod windows_node_ipc {
    use std::ffi::c_void;
    use std::os::windows::io::RawHandle;

    const FILE_TYPE_PIPE: u32 = 0x0003;
    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "GetFileType"]
        fn get_file_type(handle: *mut c_void) -> u32;
    }

    pub(super) unsafe fn is_pipe(handle: RawHandle) -> bool {
        // SAFETY: caller owns a duplicated handle and checks it before transfer.
        unsafe { get_file_type(handle) == FILE_TYPE_PIPE }
    }
}

/// Parses the inherited Node IPC descriptor without allowing a malformed or
/// out-of-range value to be cast into an invalid OS handle later in bootstrap.
#[cfg(unix)]
fn parse_node_channel_fd(value: &str) -> Option<i64> {
    let fd = value.parse::<i64>().ok()?;
    (0..=i32::MAX as i64).contains(&fd).then_some(fd)
}

#[cfg(windows)]
fn parse_node_channel_fd(value: &str) -> Option<i64> {
    // deno_io consumes this as a raw HANDLE on Windows. Null and
    // INVALID_HANDLE_VALUE are not usable handles; the conversion back to
    // i64 also rejects a value that cannot be represented by the bootstrap
    // tuple on a wider target.
    let handle: usize = value.parse::<u64>().ok()?.try_into().ok()?;
    if handle == 0 || handle == usize::MAX {
        return None;
    }
    i64::try_from(handle).ok()
}

#[cfg(not(any(unix, windows)))]
fn parse_node_channel_fd(value: &str) -> Option<i64> {
    let fd = value.parse::<i64>().ok()?;
    (fd > 0).then_some(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs::File;

    #[test]
    fn isolate_create_params_maps_heap_cap() {
        // P2: max_heap_bytes must reach isolate creation as the V8
        // old-generation ceiling; None leaves V8 defaults untouched.
        assert!(isolate_create_params(None).is_none());
        let bytes = 16 << 20;
        let params = isolate_create_params(Some(bytes)).unwrap();
        assert_eq!(params.max_old_generation_size_in_bytes(), bytes);
    }

    #[test]
    fn invalid_heap_caps_are_rejected_before_v8_configuration() {
        assert!(validate_max_heap_bytes(Some(0)).is_err());
        assert!(validate_max_heap_bytes(Some(MIN_V8_OLD_GENERATION_BYTES - 1)).is_err());
        assert!(validate_max_heap_bytes(Some(usize::MAX)).is_err());
        assert!(validate_max_heap_bytes(Some(MIN_V8_OLD_GENERATION_BYTES)).is_ok());
        assert!(validate_max_heap_bytes(None).is_ok());
    }

    #[test]
    fn overflowing_execution_deadline_is_rejected() {
        assert!(validate_execution_deadline(Some(Duration::MAX)).is_err());
        assert!(validate_execution_deadline(Some(Duration::from_secs(1))).is_ok());
        assert!(validate_execution_deadline(None).is_ok());
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn absolute_deadline_guard_requests_an_expired_deadline() {
        let cancellation = CancellationContext::new();
        let _guard = AbsoluteDeadlineGuard::new(
            Some(Instant::now() - Duration::from_millis(1)),
            Some(cancellation.clone()),
        )
        .unwrap();
        assert!(cancellation.is_requested());
        assert_eq!(cancellation.reason(), Some(CancellationReason::Deadline));
    }

    #[cfg(unix)]
    #[test]
    fn claimed_node_ipc_socket_is_cloexec() {
        const FD_ENV: &str = "LIBDENO_NODE_IPC_CLOEXEC_TEST_FD";
        if let Ok(raw) = std::env::var(FD_ENV) {
            use std::os::fd::IntoRawFd;
            let fd: i32 = raw.parse().unwrap();
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            let owned = claim_node_ipc(&raw).unwrap();
            assert_ne!(
                unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            let raw = owned.into_raw_fd();
            // SAFETY: raw is the uniquely-owned descriptor returned by into_raw_fd.
            drop(unsafe { OwnedFd::from_raw_fd(raw) });
            return;
        }

        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = socket.as_raw_fd();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "limits::tests::claimed_node_ipc_socket_is_cloexec",
                "--nocapture",
            ])
            .env(FD_ENV, "0")
            .env("NODE_CHANNEL_SERIALIZATION_MODE", "json")
            .stdin(std::process::Stdio::null())
            .process_group(0);
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let inherited_flags = libc::fcntl(fd, libc::F_GETFD);
                if inherited_flags < 0
                    || libc::fcntl(fd, libc::F_SETFD, inherited_flags | libc::FD_CLOEXEC) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                let stdio_flags = libc::fcntl(0, libc::F_GETFD);
                if stdio_flags < 0
                    || libc::fcntl(0, libc::F_SETFD, stdio_flags & !libc::FD_CLOEXEC) < 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                panic!("CLOEXEC child test timed out");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn node_ipc_owned_channel_is_taken_once() {
        use std::os::fd::{FromRawFd, IntoRawFd};
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let owned = claim_node_ipc(&socket.into_raw_fd().to_string()).unwrap();
        let mut state = Some(Some((owned, ChildIpcSerialization::Json)));
        let (raw, _mode) = take_node_ipc(&mut state).unwrap();
        // SAFETY: the tuple transfers unique ownership to the receiver.
        drop(unsafe { OwnedFd::from_raw_fd(raw as i32) });
        assert!(take_node_ipc(&mut state).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_node_ipc_owned_channel_is_taken_once_and_reclaimed() {
        use std::ffi::c_void;
        use std::os::windows::io::{FromRawHandle, OwnedHandle};

        #[link(name = "kernel32")]
        extern "system" {
            fn CreatePipe(
                read_pipe: *mut *mut c_void,
                write_pipe: *mut *mut c_void,
                attrs: *mut c_void,
                size: u32,
            ) -> i32;
        }

        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        assert_ne!(
            unsafe { CreatePipe(&mut read, &mut write, std::ptr::null_mut(), 0) },
            0
        );
        let mut state = Some(Some((
            unsafe { OwnedHandle::from_raw_handle(read) },
            ChildIpcSerialization::Json,
        )));
        let (raw, _mode) = take_node_ipc(&mut state).unwrap();
        drop(unsafe { OwnedHandle::from_raw_handle(raw as usize as *mut _) });
        assert!(take_node_ipc(&mut state).is_none());
        drop(unsafe { OwnedHandle::from_raw_handle(write) });
    }

    #[cfg(unix)]
    #[test]
    fn node_ipc_rejects_unconnected_listener_and_closes_it() {
        const CHILD: &str = "LIBDENO_NODE_IPC_UNCONNECTED_LISTENER_TEST";
        if std::env::var(CHILD).ok().as_deref() != Some("1") {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "limits::tests::node_ipc_rejects_unconnected_listener_and_closes_it",
                ])
                .env(CHILD, "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    let output = child.wait_with_output().unwrap();
                    assert!(
                        status.success(),
                        "isolated fd-close test failed: {output:?}"
                    );
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    assert!(
                        stdout.contains("running 1 test") && stdout.contains("1 passed; 0 failed"),
                        "unexpected child test output: {stdout}"
                    );
                    return;
                }
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("isolated fd-close test timed out");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        use std::os::fd::IntoRawFd;
        let path =
            std::env::temp_dir().join(format!("libdeno-ipc-listener-{}", std::process::id()));
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let fd = listener.into_raw_fd();
        assert!(claim_node_ipc(&fd.to_string()).is_err());
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        let _ = std::fs::remove_file(path);
    }

    #[cfg(unix)]
    #[test]
    fn node_ipc_failed_attempt_cannot_claim_reused_descriptor() {
        const MODE: &str = "LIBDENO_NODE_IPC_FAILURE_TEST";
        let selected_mode = std::env::var(MODE).unwrap_or_default();
        if selected_mode == "invalid-mode" || selected_mode == "fcntl-failure" {
            let fd: i32 = std::env::var("NODE_CHANNEL_FD").unwrap().parse().unwrap();
            if selected_mode == "fcntl-failure" {
                FAIL_NEXT_NODE_IPC_FCNTL.with(|flag| flag.set(true));
            }
            assert!(unsafe { initialize_node_ipc_from_env() }.is_err());
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
            let mut fillers = Vec::new();
            let replacement = loop {
                let file = File::open("/dev/null").unwrap();
                let opened = file.as_raw_fd();
                if opened == fd {
                    break file;
                }
                assert!(opened < fd, "descriptor allocation passed the closed slot");
                fillers.push(file);
            };
            assert_eq!(
                replacement.as_raw_fd(),
                fd,
                "failed claim should close its raw fd"
            );
            assert!(unsafe { initialize_node_ipc_from_env() }.is_err());
            assert!(unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0);
            return;
        }

        use std::os::unix::process::CommandExt;
        for (mode, serialization) in [("invalid-mode", "not-json"), ("fcntl-failure", "json")] {
            let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
            let fd = socket.as_raw_fd();
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "limits::tests::node_ipc_failed_attempt_cannot_claim_reused_descriptor",
                    "--nocapture",
                ])
                .env(MODE, mode)
                .env("NODE_CHANNEL_FD", fd.to_string())
                .env("NODE_CHANNEL_SERIALIZATION_MODE", serialization)
                .process_group(0);
            unsafe {
                command.pre_exec(move || {
                    let flags = libc::fcntl(fd, libc::F_GETFD);
                    if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    assert!(status.success(), "mode {mode}: {status}");
                    break;
                }
                if Instant::now() >= deadline {
                    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                    let _ = child.wait();
                    panic!("Node IPC failure test mode {mode} timed out");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_node_ipc_clone_failure_closes_claimed_handle() {
        use std::ffi::c_void;
        use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle};

        #[link(name = "kernel32")]
        extern "system" {
            fn CreatePipe(
                read_pipe: *mut *mut c_void,
                write_pipe: *mut *mut c_void,
                attrs: *mut c_void,
                size: u32,
            ) -> i32;
            fn GetHandleInformation(handle: *mut c_void, flags: *mut u32) -> i32;
        }

        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        let mut attempt = None;
        assert!(begin_node_ipc_attempt(&mut attempt));
        assert_ne!(
            unsafe { CreatePipe(&mut read, &mut write, std::ptr::null_mut(), 0) },
            0
        );
        let owned = unsafe { OwnedHandle::from_raw_handle(read) };
        let raw = owned.into_raw_handle();
        FAIL_NEXT_NODE_IPC_CLONE.with(|flag| flag.set(true));
        assert!(
            duplicate_and_close_node_handle(unsafe { OwnedHandle::from_raw_handle(raw) }).is_err()
        );
        let mut flags = 0;
        assert_eq!(unsafe { GetHandleInformation(raw, &mut flags) }, 0);
        // Close the peer handle created by CreatePipe.
        drop(unsafe { OwnedHandle::from_raw_handle(write) });

        let mut replacement = std::ptr::null_mut();
        let mut replacement_peer = std::ptr::null_mut();
        assert_ne!(
            unsafe {
                CreatePipe(
                    &mut replacement,
                    &mut replacement_peer,
                    std::ptr::null_mut(),
                    0,
                )
            },
            0
        );
        // A failed clone cannot reset the process-attempt state. Later input
        // must not be wrapped or closed, even if Windows reuses the handle value.
        assert!(!begin_node_ipc_attempt(&mut attempt));
        let mut replacement_flags = 0;
        assert_ne!(
            unsafe { GetHandleInformation(replacement, &mut replacement_flags) },
            0
        );
        drop(unsafe { OwnedHandle::from_raw_handle(replacement) });
        drop(unsafe { OwnedHandle::from_raw_handle(replacement_peer) });
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn absolute_deadline_guard_spawn_failure_is_io_error() {
        fail_next_watchdog_spawn();
        let cancellation = CancellationContext::new();
        let result = AbsoluteDeadlineGuard::new(
            Some(Instant::now() + Duration::from_secs(60)),
            Some(cancellation.clone()),
        );
        assert!(result.is_err());
        assert!(!cancellation.is_requested());
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn termination_watchdog_spawn_failure_is_infrastructure_before_poll() {
        let polled = Arc::new(AtomicBool::new(false));
        let polled_by_future = polled.clone();
        let future = std::future::poll_fn(move |_| {
            polled_by_future.store(true, Ordering::SeqCst);
            std::task::Poll::Ready(Ok::<(), std::io::Error>(()))
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        fail_next_watchdog_spawn();
        let result = runtime.block_on(run_with_termination(
            future,
            Some(Duration::from_secs(60)),
            None,
            None,
            None,
        ));
        assert!(matches!(
            result,
            Err(ExecutionTermination::Infrastructure(_))
        ));
        assert!(!polled.load(Ordering::SeqCst));
    }
}
