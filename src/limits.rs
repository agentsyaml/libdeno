//! Resource limits: V8 heap constraints, execution deadlines, child-mode IPC
//! gating, and the in-process V8 code cache.

use std::future::Future;
use std::path::PathBuf;
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
    //
    // Known small race: a script that completes exactly at the deadline can be
    // reported as timed out if the terminator thread set `fired` in the
    // instant before the completed result was observed. Safety-biased (a false
    // timeout is observable by the caller; a missed deadline is not) and
    // accepted.
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
}
