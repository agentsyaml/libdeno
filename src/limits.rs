//! Resource limits: V8 heap constraints, execution deadlines, child-mode IPC
//! gating, and the in-process V8 code cache.

use std::future::Future;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use deno_core::v8;
use deno_core::v8::IsolateHandle;
use deno_core::ModuleSpecifier;
use deno_runtime::code_cache::CodeCache;
use deno_runtime::code_cache::CodeCacheType;
use deno_runtime::deno_node::ops::ipc::ChildIpcSerialization;
use deno_runtime::worker::MainWorker;

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

/// (specifier, cache type, source hash) -> compiled script bytes.
type CodeCacheKey = (String, CodeCacheType, u64);
type CodeCacheEntry = (CodeCacheKey, Vec<u8>);

struct InMemoryCodeCache {
    /// FIFO vec (oldest first); the entry cap doubles as the eviction order.
    entries: Mutex<Vec<CodeCacheEntry>>,
}

impl Default for InMemoryCodeCache {
    fn default() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
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
        self.entries
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, data)| data.clone())
    }

    fn set_sync(
        &self,
        specifier: ModuleSpecifier,
        code_cache_type: CodeCacheType,
        source_hash: u64,
        data: &[u8],
    ) {
        let key = (specifier.as_str().to_owned(), code_cache_type, source_hash);
        let mut entries = self.entries.lock().unwrap();
        if let Some(entry) = entries.iter_mut().find(|(k, _)| *k == key) {
            entry.1 = data.to_vec();
            return;
        }
        entries.push((key, data.to_vec()));
        if entries.len() > CODE_CACHE_MAX_ENTRIES {
            entries.remove(0);
        }
    }
}

static CODE_CACHE: OnceLock<Arc<InMemoryCodeCache>> = OnceLock::new();

/// Shared, process-wide code cache; repeated [`crate::run`] calls reuse the
/// same instance so warm runs hit.
pub(crate) fn in_process_code_cache() -> Arc<dyn CodeCache> {
    CODE_CACHE
        .get_or_init(|| Arc::new(InMemoryCodeCache::default()))
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
/// check, `run_event_loop` returns, the future unwinds and the run's cwd lock
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
pub(crate) async fn run_with_deadline<F, T, E>(
    fut: F,
    deadline: Option<Duration>,
    isolate_handle: IsolateHandle,
) -> Result<Result<T, E>, Duration>
where
    F: Future<Output = Result<T, E>>,
{
    const GRACE: Duration = Duration::from_secs(2);

    let Some(deadline) = deadline else {
        return Ok(fut.await);
    };

    // Signal channel: if the run finishes first we send on it so the waiter
    // exits without terminating (harmless either way — terminate_execution
    // returns false on a dropped isolate — but this frees the thread at once
    // instead of leaving it asleep for the rest of the deadline).
    let fired = Arc::new(AtomicBool::new(false));
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let terminator = {
        let fired = fired.clone();
        std::thread::spawn(move || match done_rx.recv_timeout(deadline) {
            Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                fired.store(true, Ordering::SeqCst);
                isolate_handle.terminate_execution();
            }
        })
    };

    let result = match tokio::time::timeout(deadline.saturating_add(GRACE), fut).await {
        Ok(result) => result,
        Err(_) => return Err(deadline),
    };

    let _ = done_tx.send(());
    let _ = terminator.join();

    // `result` alone cannot tell whether the future unwound because the script
    // finished or because the deadline interrupted it; the flag set by the
    // terminator disambiguates for the caller's timeout error.
    if fired.load(Ordering::SeqCst) {
        Err(deadline)
    } else {
        Ok(result)
    }
}

/// Runs the standard worker lifecycle (main module, event loop, load/unload/
/// exit events) under an optional execution deadline.
pub(crate) async fn run_worker(
    worker: &mut MainWorker,
    main_module: &ModuleSpecifier,
    execution_deadline: Option<Duration>,
    isolate_handle: IsolateHandle,
) -> Result<Result<(), crate::LibdenoError>, Duration> {
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
    run_with_deadline(run, execution_deadline, isolate_handle).await
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
/// IPC channel. Called from [`crate::run`] under CWD_LOCK, which serializes
/// the env write (edition 2021: `set_var` is safe; concurrent runs in one
/// process are already excluded).
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
    let fd = std::env::var("NODE_CHANNEL_FD").ok()?.parse::<i64>().ok()?;
    let serialization = match std::env::var("NODE_CHANNEL_SERIALIZATION_MODE").as_deref() {
        Ok("advanced") => ChildIpcSerialization::Advanced,
        _ => ChildIpcSerialization::Json,
    };
    Some((fd, serialization))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolate_create_params_maps_heap_cap() {
        // P2: max_heap_bytes must reach isolate creation as the V8
        // old-generation ceiling; None leaves V8 defaults untouched.
        assert!(isolate_create_params(None).is_none());
        let params = isolate_create_params(Some(12345)).unwrap();
        assert_eq!(params.max_old_generation_size_in_bytes(), 12345);
    }

    #[test]
    fn code_cache_fifo_evicts_oldest_entry() {
        // P2-3: inserting past CODE_CACHE_MAX_ENTRIES must evict the oldest
        // entry (FIFO) instead of growing without bound. 1025 inserts are a
        // few trivial vec pushes, so the real 1024-entry cap is exercised.
        let cache = InMemoryCodeCache::default();
        let spec = |i: u64| {
            ModuleSpecifier::from_file_path(format!("/libdeno-code-cache-test/{i}.js")).unwrap()
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
        let spec = ModuleSpecifier::from_file_path("/libdeno-code-cache-test/update.js").unwrap();
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
}
