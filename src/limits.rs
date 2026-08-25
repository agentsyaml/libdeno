//! Resource limits: V8 heap constraints, execution deadlines, child-mode IPC
//! gating, and the in-process V8 code cache.

use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use deno_core::v8;
use deno_core::v8::IsolateHandle;
use deno_core::ModuleSpecifier;
use deno_runtime::code_cache::CodeCache;
use deno_runtime::code_cache::CodeCacheType;
use deno_runtime::deno_node::ops::ipc::ChildIpcSerialization;
use deno_runtime::worker::MainWorker;

use crate::timing::{ExecutionTiming, Phase};

#[cfg(feature = "execution-control")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    ) -> Self {
        let Some(deadline) = deadline else {
            return Self {
                stop: None,
                join: None,
            };
        };
        let Some(cancellation) = cancellation else {
            return Self {
                stop: None,
                join: None,
            };
        };
        if deadline <= Instant::now() {
            cancellation.request_with_reason(CancellationReason::Deadline);
            return Self {
                stop: None,
                join: None,
            };
        }
        let (stop, receiver) = std::sync::mpsc::channel();
        let cancellation_for_thread = cancellation.clone();
        let join = std::thread::Builder::new()
            .name("libdeno-submission-deadline".to_string())
            .spawn(move || {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if receiver.recv_timeout(remaining).is_err() {
                    cancellation_for_thread.request_with_reason(CancellationReason::Deadline);
                }
            })
            .ok();
        if join.is_none() {
            cancellation.request_with_reason(CancellationReason::Deadline);
        }
        Self {
            stop: Some(stop),
            join,
        }
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

/// Process-wide, in-memory V8 code cache, keyed by `(specifier, code-cache
/// type, source hash)`, bounded by FIFO eviction so a script evaling
/// unbounded distinct sources cannot grow memory without limit.
///
/// The source hash is computed by deno_runtime from the actual source text, so
/// the same specifier with different content (edits, a different project, a
/// different runtime version) never collides. On top of that, V8 itself
/// validates a code cache against the source it is about to compile and
/// silently recompiles on mismatch — a stale entry can only cost a wasted
/// lookup, never wrong behavior.
///
/// Note: this hooks deno_runtime's `WorkerServiceOptions.v8_code_cache` seam,
/// which covers eval-context (script) compilation. ES-module code caching in
/// deno_runtime rides on the `ModuleLoader::get_code_cache`/`code_cache_ready`
/// seam instead (module_loader.rs), which is out of scope here.
const CODE_CACHE_MAX_ENTRIES: usize = 1024;

/// Process-wide byte ceiling for compiled code cache entries; combined with
/// the entry-count cap so a script evaling many distinct large sources cannot
/// pin unbounded memory in the process (the cache lives in a process-wide
/// OnceLock for the lifetime of the host).
const CODE_CACHE_MAX_BYTES: usize = 256 * 1024 * 1024; // 256 MiB

/// (specifier, cache type, source hash) -> compiled script bytes.
type CodeCacheKey = (String, CodeCacheType, u64);
type CodeCacheEntry = (CodeCacheKey, Vec<u8>);

struct InMemoryCodeCache {
    /// (entries FIFO oldest-first, total byte size of all entries).
    state: Mutex<(Vec<CodeCacheEntry>, usize)>,
    /// (max entries, max total bytes); tuned small in tests via `with_limits`.
    limits: (usize, usize),
    /// Optional disk-backed layer: compiled bytes survive process restarts
    /// (CLI-style hosts — every npm-plugin invocation is a fresh process),
    /// keyed by a hash of (specifier, type, source hash) so stale or
    /// cross-project entries can never be served for the wrong source. V8
    /// validates code-cache data itself, so corrupted/tampered files are
    /// rejected at compile time, never mis-executed. `None` in tests.
    disk_dir: Option<PathBuf>,
}

impl Default for InMemoryCodeCache {
    fn default() -> Self {
        Self {
            state: Mutex::new((Vec::new(), 0)),
            limits: (CODE_CACHE_MAX_ENTRIES, CODE_CACHE_MAX_BYTES),
            disk_dir: None,
        }
    }
}

#[cfg(test)]
impl InMemoryCodeCache {
    fn with_limits(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            limits: (max_entries, max_bytes),
            ..Default::default()
        }
    }
}

impl InMemoryCodeCache {
    /// Disk directory for the code cache: `LIBDENO_CODE_CACHE_DIR` overrides,
    /// else `<DENO_DIR>/code_cache`. Without either (and with an empty
    /// override) the cache stays in-memory only.
    fn disk_dir_from_env() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("LIBDENO_CODE_CACHE_DIR") {
            return if dir.is_empty() {
                None
            } else {
                Some(PathBuf::from(dir))
            };
        }
        std::env::var_os("DENO_DIR").map(|d| PathBuf::from(d).join("code_cache"))
    }

    fn with_disk(dir: PathBuf) -> Self {
        Self {
            disk_dir: Some(dir),
            ..Default::default()
        }
    }

    /// Deterministic file name for a cache key; the specifier itself never
    /// appears in the path (it can contain `/`, `..`, and platform
    /// separators). Source-hash in the key means a changed source writes a
    /// different file, never a stale hit.
    fn disk_path(&self, key: &CodeCacheKey) -> Option<PathBuf> {
        use std::hash::Hasher;
        let dir = self.disk_dir.as_ref()?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        h.write(key.0.as_bytes());
        h.write_u8(match key.1 {
            CodeCacheType::EsModule => 0,
            CodeCacheType::Script => 1,
        });
        h.write_u64(key.2);
        Some(dir.join(format!("{:016x}.bin", h.finish())))
    }
    /// Bounded disk hygiene: on the first write of a process, delete files
    /// matching this cache's own naming scheme (16 hex chars + `.bin`) if
    /// there are more of them than the in-memory entry cap (eviction is
    /// best-effort anyway; a clear only costs one cold run). The check runs
    /// once per process, so a long-lived host's disk tier can grow past the
    /// in-memory cap between checks — cache loss at worst (V8 validates
    /// every code-cache payload), never wrong execution.
    fn maybe_clean_disk(&self) {
        static CHECKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let Some(dir) = &self.disk_dir else { return };
        if CHECKED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let bin_entries: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(entries) => entries
                .flatten()
                .map(|e| e.path())
                // Only this cache's own naming scheme (16 hex chars + .bin)
                // is ever touched: other tools' .bin files in a shared
                // LIBDENO_CODE_CACHE_DIR, or a concurrent libdeno process's
                // entries, survive.
                .filter(|p| {
                    p.extension().is_some_and(|ext| ext == "bin")
                        && p.file_stem().is_some_and(|stem| {
                            let s = stem.to_str().unwrap_or("");
                            s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
                        })
                })
                .collect(),
            Err(_) => return,
        };
        if bin_entries.len() > CODE_CACHE_MAX_ENTRIES {
            for path in bin_entries {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

impl CodeCache for InMemoryCodeCache {
    fn get_sync(
        &self,
        specifier: &ModuleSpecifier,
        code_cache_type: CodeCacheType,
        source_hash: u64,
    ) -> Option<Vec<u8>> {
        let key = (specifier.as_str().to_owned(), code_cache_type, source_hash);
        if let Some(data) = self
            .state
            .lock()
            .unwrap()
            .0
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, data)| data.clone())
        {
            return Some(data);
        }
        // Disk miss path: a fresh process with a warm disk cache reads once
        // per module here. No memory backfill — by the time get_sync runs the
        // compile will set_sync anyway (or the cached code is used and the
        // next process reads the disk again; either way the file is correct).
        std::fs::read(self.disk_path(&key)?).ok()
    }

    fn set_sync(
        &self,
        specifier: ModuleSpecifier,
        code_cache_type: CodeCacheType,
        source_hash: u64,
        data: &[u8],
    ) {
        let key = (specifier.as_str().to_owned(), code_cache_type, source_hash);
        let disk_path = self.disk_path(&key);
        let (max_entries, max_bytes) = self.limits;
        let mut state = self.state.lock().unwrap();
        let (entries, total) = &mut *state;
        if let Some(entry) = entries.iter_mut().find(|(k, _)| *k == key) {
            // Replacing an existing key adjusts the running byte total — and
            // falls through to the eviction loop: a larger replacement could
            // push the total past the byte cap, and the invariant "total <=
            // max_bytes" must hold on every path out of set_sync. (Today the
            // key includes the source hash, so same key ⇒ same size; keeping
            // the loop uniform costs nothing and makes the cap unconditional.)
            *total = *total - entry.1.len() + data.len();
            entry.1 = data.to_vec();
        } else {
            entries.push((key, data.to_vec()));
            *total += data.len();
        }
        // Evict oldest-first past either the entry cap or the byte cap. A
        // single entry larger than max_bytes is evicted by its own insert
        // (uncacheable scripts simply never cache) — intended: the cap is
        // unconditional on every path out of set_sync.
        while !entries.is_empty() && (entries.len() > max_entries || *total > max_bytes) {
            let removed = entries.remove(0);
            *total -= removed.1.len();
        }
        // Disk write is best-effort: a read-only cache dir, a full disk, or
        // a missing parent must never fail the run — the code cache is a
        // pure optimization. Entries the in-memory tier just evicted as
        // "uncacheable" (larger than max_bytes) are skipped, keeping the
        // disk tier's per-entry bound identical to memory.
        drop(state);
        if let Some(path) = disk_path {
            if data.len() <= max_bytes {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&path, data);
                self.maybe_clean_disk();
            }
        }
    }
}

static CODE_CACHE: OnceLock<Arc<InMemoryCodeCache>> = OnceLock::new();

/// Shared, process-wide code cache; repeated [`crate::run`] calls reuse the
/// same instance so warm runs hit. Backed by disk when
/// `LIBDENO_CODE_CACHE_DIR` or `DENO_DIR` is set, so cold process starts
/// (CLI-style hosts) reuse compiled script bytes across invocations too.
pub(crate) fn in_process_code_cache() -> Arc<dyn CodeCache> {
    CODE_CACHE
        .get_or_init(|| {
            Arc::new(match InMemoryCodeCache::disk_dir_from_env() {
                Some(dir) => InMemoryCodeCache::with_disk(dir),
                None => InMemoryCodeCache::default(),
            })
        })
        .clone()
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionTermination {
    Deadline(Duration),
    Cancelled,
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
) -> Result<Result<T, E>, ExecutionTermination>
where
    F: Future<Output = Result<T, E>>,
{
    const GRACE: Duration = Duration::from_secs(2);

    if deadline.is_none() && cancellation.is_none() {
        return Ok(fut.await);
    }

    if let Some(cancellation) = &cancellation {
        cancellation.register(isolate_handle.clone());
    }

    // Signal channel: if the run finishes first we send on it so the waiter
    // exits without terminating (harmless either way — terminate_execution
    // returns false on a dropped isolate — but this frees the thread at once
    // instead of leaving it asleep for the rest of the deadline).
    let deadline_fired = Arc::new(AtomicBool::new(false));
    let cancellation_fired = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let terminator = {
        let deadline_fired = deadline_fired.clone();
        let cancellation_fired = cancellation_fired.clone();
        let cancellation = cancellation.clone();
        let deadline_at = deadline.and_then(|duration| Instant::now().checked_add(duration));
        std::thread::spawn(move || loop {
            if cancellation
                .as_ref()
                .is_some_and(CancellationContext::is_requested)
            {
                cancellation_fired.store(true, Ordering::SeqCst);
                isolate_handle.terminate_execution();
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
                        isolate_handle.terminate_execution();
                        break;
                    }
                }
            }
        })
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

    if let Some(cancellation) = &cancellation {
        cancellation.clear();
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
) -> Result<Result<(), crate::LibdenoError>, ExecutionTermination> {
    let _user_execution = timing.span(Phase::UserExecution);
    let run = async {
        worker.execute_main_module(main_module).await?;
        worker.run_event_loop(false).await?;
        worker.dispatch_load_event()?;
        worker.run_event_loop(false).await?;
        worker.dispatch_beforeunload_event()?;
        worker.dispatch_unload_event()?;
        worker.dispatch_process_beforeexit_event()?;
        worker.dispatch_process_exit_event()?;
        Ok::<(), crate::LibdenoError>(())
    };
    run_with_deadline_cancellable(run, execution_deadline, isolate_handle, cancellation).await
}

/// Environment marker pairing an IPC child with its spawner: set on the
/// subprocess child by [`crate::run_in_subprocess`]; for `child_process.fork`
/// children it is written into this process's own env (see
/// [`capture_spawned_ipc_marker`]) so the fork child inherits it.
pub(crate) const LIBDENO_SPAWNED_IPC: &str = "LIBDENO_SPAWNED_IPC";

/// The marker value present at process entry, captured once on the first
/// [`crate::run`] call: true for a subprocess/fork child, false for a regular
/// host. `node_ipc_init` reads this, never the live env — the live env always
/// carries the marker after capture (we write it back for fork children to
/// inherit), so a live read would make a regular host adopt a stray/foreign
/// `NODE_CHANNEL_FD` as its IPC pipe.
static NODE_IPC_MARKER: OnceLock<bool> = OnceLock::new();

/// Captures the original [`LIBDENO_SPAWNED_IPC`] marker at process entry,
/// then writes it into our own environment so `child_process.fork` children
/// (which inherit the env and carry deno_node's `NODE_CHANNEL_FD`) honor their
/// IPC channel. Called from [`crate::run`] with the run lease held.
/// The env write (edition 2021: `set_var` is safe; runs are concurrent but
/// the written value is the constant captured at process entry, and `set_var`
/// is internally synchronized on the target platforms).
///
/// Known tradeoff: the write also means ordinary subprocesses the host spawns
/// afterwards inherit LIBDENO_SPAWNED_IPC=1; the entry-time capture (never a
/// live read) still blocks the mainstream misuse of a foreign NODE_CHANNEL_FD.
pub(crate) fn capture_spawned_ipc_marker() {
    NODE_IPC_MARKER.get_or_init(|| {
        let spawned = std::env::var(LIBDENO_SPAWNED_IPC).as_deref() == Ok("1");
        std::env::set_var(LIBDENO_SPAWNED_IPC, "1");
        spawned
    });
}

/// Node IPC pipe for `child_process.fork`/`spawn(stdio: ["ipc"])`, gated on
/// the spawning side's marker.
///
/// `NODE_CHANNEL_FD` is only honored when the spawning side (subprocess.rs,
/// the libdeno child-mode lane, or a `child_process.fork` spawn) set
/// `LIBDENO_SPAWNED_IPC=1`. Node itself sets `NODE_CHANNEL_FD` when *it*
/// spawns children, and adopting a stray/foreign FD as our IPC pipe could
/// connect the runtime to an unrelated process. Without the paired marker the
/// variable is ignored entirely.
pub(crate) fn node_ipc_init() -> Option<(i64, ChildIpcSerialization)> {
    // The entry-time capture, NOT a live env read: after capture the env
    // always says 1, so a live read would defeat the gating above.
    if !NODE_IPC_MARKER.get().copied().unwrap_or(false) {
        return None;
    }
    let fd = parse_node_channel_fd(&std::env::var("NODE_CHANNEL_FD").ok()?)?;
    let serialization = match std::env::var("NODE_CHANNEL_SERIALIZATION_MODE").as_deref() {
        Ok("advanced") => ChildIpcSerialization::Advanced,
        _ => ChildIpcSerialization::Json,
    };
    Some((fd, serialization))
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
        );
        assert!(cancellation.is_requested());
        assert_eq!(cancellation.reason(), Some(CancellationReason::Deadline));
    }

    #[test]
    fn code_cache_fifo_evicts_oldest_entry() {
        // P2-3: inserting past CODE_CACHE_MAX_ENTRIES must evict the oldest
        // entry (FIFO) instead of growing without bound. 1025 inserts are a
        // few trivial vec pushes, so the real 1024-entry cap is exercised.
        let cache = InMemoryCodeCache::default();
        // Pure URL string, not from_file_path: a drive-letter-less absolute
        // path is not a file URL on Windows (from_file_path returns Err there),
        // while Url::parse of "file:///..." succeeds identically on every
        // platform. The specifier is only ever used as a cache key here.
        let spec = |i: u64| {
            ModuleSpecifier::parse(&format!("file:///libdeno-code-cache-test/{i}.js")).unwrap()
        };
        let hash = 7u64;
        for i in 0..=CODE_CACHE_MAX_ENTRIES as u64 {
            cache.set_sync(spec(i), CodeCacheType::EsModule, hash, &[0u8, 1, 2]);
        }
        assert!(
            cache
                .get_sync(&spec(0), CodeCacheType::EsModule, hash)
                .is_none(),
            "oldest entry must be evicted"
        );
        assert!(
            cache
                .get_sync(
                    &spec(CODE_CACHE_MAX_ENTRIES as u64),
                    CodeCacheType::EsModule,
                    hash
                )
                .is_some(),
            "newest entry must be present"
        );
        // A different source hash is a different key: no false hits.
        assert!(cache
            .get_sync(&spec(5), CodeCacheType::EsModule, 999)
            .is_none());
    }

    #[test]
    fn code_cache_replace_and_type_keying() {
        let cache = InMemoryCodeCache::default();
        // URL string (not from_file_path): platform-independent, see the FIFO
        // test above.
        let spec = ModuleSpecifier::parse("file:///libdeno-code-cache-test/update.js").unwrap();
        // Re-setting the same key replaces the value without growing the vec.
        cache.set_sync(spec.clone(), CodeCacheType::Script, 1, b"old");
        cache.set_sync(spec.clone(), CodeCacheType::Script, 1, b"new");
        assert_eq!(
            cache.get_sync(&spec, CodeCacheType::Script, 1).unwrap(),
            b"new"
        );
        // CodeCacheType is part of the key: EsModule and Script are distinct.
        cache.set_sync(spec.clone(), CodeCacheType::EsModule, 1, b"esm");
        assert_eq!(
            cache.get_sync(&spec, CodeCacheType::Script, 1).unwrap(),
            b"new"
        );
        assert_eq!(
            cache.get_sync(&spec, CodeCacheType::EsModule, 1).unwrap(),
            b"esm"
        );
    }

    #[test]
    fn code_cache_byte_cap_evicts_oldest() {
        // P2-3: the byte ceiling must evict oldest-first, exactly like the
        // entry cap — a script evaling many distinct large sources cannot pin
        // unbounded memory in the process-wide cache. Keys differ per round by
        // source_hash (i), so each insert is a new entry.
        let cache = InMemoryCodeCache::with_limits(1024, 100);
        let spec = |i: u64| {
            ModuleSpecifier::parse(&format!("file:///libdeno-byte-cap-test/{i}.js")).unwrap()
        };
        for i in 0..10 {
            cache.set_sync(spec(i), CodeCacheType::Script, i, &[i as u8; 20]);
        }
        // 每条约 20 字节，100 字节上限只能容纳约 5 条；最旧条目必须被逐出。
        assert!(cache.get_sync(&spec(0), CodeCacheType::Script, 0).is_none());
        assert!(cache.get_sync(&spec(9), CodeCacheType::Script, 9).is_some());
    }

    #[test]
    fn code_cache_replace_still_enforces_byte_cap() {
        // Regression: the replace path used to return before the eviction
        // loop, so growing a same-key entry could leave the total above the
        // byte cap. The invariant "total <= max_bytes" must hold on every
        // path out of set_sync.
        let cache = InMemoryCodeCache::with_limits(1024, 100);
        let spec = |i: u64| {
            ModuleSpecifier::parse(&format!("file:///libdeno-replace-cap-test/{i}.js")).unwrap()
        };
        // 5 × 20 bytes = exactly 100 (the cap).
        for i in 0..5 {
            cache.set_sync(spec(i), CodeCacheType::Script, i, &[i as u8; 20]);
        }
        // Replacing the newest entry with a 60-byte value pushes the total to
        // 140: the oldest entries must be evicted until it fits again.
        cache.set_sync(spec(4), CodeCacheType::Script, 4, &[4u8; 60]);
        assert!(cache.get_sync(&spec(0), CodeCacheType::Script, 0).is_none());
        assert!(cache.get_sync(&spec(1), CodeCacheType::Script, 1).is_none());
        assert_eq!(
            cache.get_sync(&spec(4), CodeCacheType::Script, 4).unwrap(),
            vec![4u8; 60]
        );
    }

    #[test]
    fn node_ipc_requires_paired_spawn_marker() {
        // P2 security: NODE_CHANNEL_FD alone must NOT enable IPC — only the
        // LIBDENO_SPAWNED_IPC marker captured at process entry does. Env vars
        // are process-global, but no other unit test in this binary touches
        // these names or the marker, so the sequencing below is race-free.
        std::env::set_var("NODE_CHANNEL_FD", "10");
        // Marker not yet captured: a stray FD from some other spawner is ignored.
        assert!(node_ipc_init().is_none());
        // The spawned side captures the marker; the same FD is now honored.
        std::env::set_var("LIBDENO_SPAWNED_IPC", "1");
        capture_spawned_ipc_marker();
        assert_eq!(node_ipc_init().map(|(fd, _)| fd), Some(10));
        assert!(matches!(
            node_ipc_init(),
            Some((10, ChildIpcSerialization::Json))
        ));
        // Advanced serialization mode is propagated.
        std::env::set_var("NODE_CHANNEL_SERIALIZATION_MODE", "advanced");
        assert!(matches!(
            node_ipc_init(),
            Some((10, ChildIpcSerialization::Advanced))
        ));
        // A non-numeric FD is rejected, never adopted.
        std::env::set_var("NODE_CHANNEL_FD", "not-a-fd");
        assert!(node_ipc_init().is_none());
    }

    #[test]
    fn parse_node_channel_fd_rejects_negative_values() {
        assert_eq!(parse_node_channel_fd("-1"), None);
    }

    #[test]
    fn parse_node_channel_fd_rejects_non_numeric_values() {
        assert_eq!(parse_node_channel_fd("not-a-fd"), None);
    }

    #[test]
    fn parse_node_channel_fd_accepts_normal_values() {
        assert_eq!(parse_node_channel_fd("10"), Some(10));
    }

    #[cfg(unix)]
    #[test]
    fn parse_node_channel_fd_rejects_values_outside_raw_fd_range() {
        assert_eq!(
            parse_node_channel_fd(&(i32::MAX as i64 + 1).to_string()),
            None
        );
        assert_eq!(parse_node_channel_fd(&i64::MAX.to_string()), None);
    }

    #[cfg(windows)]
    #[test]
    fn parse_node_channel_fd_rejects_null_and_invalid_handles() {
        assert_eq!(parse_node_channel_fd("0"), None);
        assert_eq!(parse_node_channel_fd(&u64::MAX.to_string()), None);
    }
}
