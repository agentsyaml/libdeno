//! Resource limits: V8 heap constraints, execution deadlines, child-mode IPC
//! gating, and the in-process V8 code cache.

use std::fs::File;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Read;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

#[cfg(unix)]
use std::os::fd::AsRawFd;

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

/// Files from older libdeno versions lived directly below the configured
/// directory. Keep that directory untouched and give this cache an owned,
/// versioned namespace instead.
const CODE_CACHE_NAMESPACE: &str = "libdeno-v8-code-cache-v1";
const CODE_CACHE_LOCK_NAME: &str = ".libdeno-v8-code-cache-v1.lock";
const CODE_CACHE_TEMP_PREFIX: &str = ".libdeno-v8-code-cache-v1-tmp-";

// The lock is deliberately nonblocking and best-effort. Ownership is the
// open-handle OS advisory lock, not mtime or file contents; unsupported or
// contended disk operations simply fall back to memory/execution.

static CODE_CACHE_DISK_ID: AtomicU64 = AtomicU64::new(0);

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
    /// rejected at compile time, never mis-executed. Tests inject temporary
    /// roots when exercising the disk layer.
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
    /// Configured root for the code cache: `LIBDENO_CODE_CACHE_DIR` overrides,
    /// else `<DENO_DIR>/code_cache`; cache files live in the versioned
    /// namespace below that root. Without either (and with an empty override)
    /// the cache stays in-memory only.
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

    #[cfg(test)]
    fn with_disk_limits(dir: PathBuf, max_entries: usize, max_bytes: usize) -> Self {
        Self {
            disk_dir: Some(dir),
            limits: (max_entries, max_bytes),
            ..Default::default()
        }
    }

    fn disk_namespace(&self) -> Option<PathBuf> {
        self.disk_dir
            .as_ref()
            .map(|dir| dir.join(CODE_CACHE_NAMESPACE))
    }

    fn ensure_disk_namespace(&self) -> Option<PathBuf> {
        let root = self.disk_dir.as_ref()?;
        if std::fs::create_dir_all(root).is_err() {
            return None;
        }
        let namespace = root.join(CODE_CACHE_NAMESPACE);
        match std::fs::symlink_metadata(&namespace) {
            Ok(metadata) if is_safe_namespace_directory(&metadata) => Some(namespace),
            Ok(_) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::create_dir(&namespace).is_ok() {
                    Some(namespace)
                } else {
                    match std::fs::symlink_metadata(&namespace) {
                        Ok(metadata) if is_safe_namespace_directory(&metadata) => Some(namespace),
                        _ => None,
                    }
                }
            }
            Err(_) => None,
        }
    }

    /// Deterministic file name for a cache key; the specifier itself never
    /// appears in the path (it can contain `/`, `..`, and platform
    /// separators). Source-hash in the key means a changed source writes a
    /// different file, never a stale hit.
    fn disk_path(&self, key: &CodeCacheKey) -> Option<PathBuf> {
        use std::hash::Hasher;
        let dir = self.disk_namespace()?;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        h.write(key.0.as_bytes());
        h.write_u8(match key.1 {
            CodeCacheType::EsModule => 0,
            CodeCacheType::Script => 1,
        });
        h.write_u64(key.2);
        Some(dir.join(format!("{:016x}.bin", h.finish())))
    }
}

#[cfg(windows)]
mod windows_namespace_lock {
    use std::ffi::c_void;
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;

    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;

    #[repr(C)]
    pub(crate) struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut c_void,
    }

    impl Overlapped {
        fn zeroed() -> Self {
            Self {
                internal: 0,
                internal_high: 0,
                offset: 0,
                offset_high: 0,
                event: std::ptr::null_mut(),
            }
        }
    }

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "LockFileEx"]
        fn lock_file_ex(
            file: *mut c_void,
            flags: u32,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
        #[link_name = "UnlockFileEx"]
        fn unlock_file_ex(
            file: *mut c_void,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }

    pub(crate) fn try_lock(file: &File) -> Option<Overlapped> {
        let mut overlapped = Overlapped::zeroed();
        let locked = unsafe {
            lock_file_ex(
                file.as_raw_handle(),
                LOCKFILE_FAIL_IMMEDIATELY | LOCKFILE_EXCLUSIVE_LOCK,
                0,
                1,
                0,
                &mut overlapped,
            ) != 0
        };
        locked.then_some(overlapped)
    }

    pub(crate) fn unlock(file: &File, overlapped: &mut Overlapped) {
        let _ = unsafe { unlock_file_ex(file.as_raw_handle(), 0, 1, 0, overlapped) };
    }
}

struct NamespaceLock {
    file: File,
    #[cfg(windows)]
    overlapped: windows_namespace_lock::Overlapped,
}

impl NamespaceLock {
    #[cfg(not(any(unix, windows)))]
    fn acquire(_namespace: &std::path::Path) -> Option<Self> {
        None
    }

    #[cfg(any(unix, windows))]
    fn acquire(namespace: &std::path::Path) -> Option<Self> {
        let path = namespace.join(CODE_CACHE_LOCK_NAME);
        let file = open_lock_file(&path)?;
        #[cfg(unix)]
        {
            if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return None;
            }
            Some(Self { file })
        }
        #[cfg(windows)]
        {
            let overlapped = windows_namespace_lock::try_lock(&file)?;
            Some(Self { file, overlapped })
        }
    }
}

#[cfg(any(unix, windows))]
fn open_lock_file(path: &std::path::Path) -> Option<File> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if is_safe_owned_file(&metadata) => {}
        Ok(_) => return None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .ok()?;
    if std::fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| is_safe_owned_file(&metadata))
    {
        Some(file)
    } else {
        None
    }
}

#[cfg(any(unix, windows))]
impl Drop for NamespaceLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        #[cfg(windows)]
        windows_namespace_lock::unlock(&self.file, &mut self.overlapped);
    }
}

struct OwnedDiskFile {
    name: String,
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

fn is_lower_hex_bin_name(name: &str) -> bool {
    name.len() == 20
        && name.ends_with(".bin")
        && name.as_bytes()[..16]
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(*byte, b'a'..=b'f'))
}

fn is_owned_temp_name(name: &str) -> bool {
    name.strip_prefix(CODE_CACHE_TEMP_PREFIX)
        .is_some_and(|suffix| !suffix.is_empty())
}

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn is_safe_namespace_directory(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_dir() && !is_reparse_point(metadata)
}

fn is_safe_owned_file(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_file()
        && !metadata.file_type().is_symlink()
        && !is_reparse_point(metadata)
}

fn scan_owned_disk_files(namespace: &std::path::Path) -> std::io::Result<Vec<OwnedDiskFile>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(namespace)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_lower_hex_bin_name(name) {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if !is_safe_owned_file(&metadata) {
            continue;
        }
        files.push(OwnedDiskFile {
            name: name.to_owned(),
            path,
            len: metadata.len(),
            modified: metadata.modified().ok(),
        });
    }
    Ok(files)
}

fn cleanup_owned_temp_files(namespace: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(namespace)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_owned_temp_name(name) {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path)?;
        if !is_safe_owned_file(&metadata) {
            continue;
        }
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn remove_owned_disk_file(path: &std::path::Path) -> std::io::Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error),
    };
    if !is_safe_owned_file(&metadata) {
        return Ok(false);
    }
    std::fs::remove_file(path)?;
    Ok(true)
}

fn sort_owned_disk_files(files: &mut [OwnedDiskFile]) {
    files.sort_by(|left, right| match (left.modified, right.modified) {
        (None, None) => left.name.cmp(&right.name),
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(left_time), Some(right_time)) => left_time
            .cmp(&right_time)
            .then_with(|| left.name.cmp(&right.name)),
    });
}

fn maintain_disk_locked(
    namespace: &std::path::Path,
    max_entries: usize,
    max_bytes: usize,
) -> std::io::Result<()> {
    cleanup_owned_temp_files(namespace)?;
    let mut files = scan_owned_disk_files(namespace)?;
    let max_bytes = u64::try_from(max_bytes).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "code-cache byte limit does not fit in metadata length",
        )
    })?;
    let mut total = files.iter().try_fold(0u64, |total, file| {
        total.checked_add(file.len).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "code-cache metadata length overflow",
            )
        })
    })?;
    sort_owned_disk_files(&mut files);
    while files.len() > max_entries || total > max_bytes {
        let file = files.remove(0);
        let _ = remove_owned_disk_file(&file.path)?;
        // A path that changed into a symlink/directory is no longer one of
        // our owned regular files even when the removal helper preserved it.
        total -= file.len;
    }
    Ok(())
}

fn create_disk_temp_file(namespace: &std::path::Path) -> std::io::Result<(PathBuf, File)> {
    for _ in 0..1024 {
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            CODE_CACHE_DISK_ID.fetch_add(1, Ordering::Relaxed)
        );
        let path = namespace.join(format!("{CODE_CACHE_TEMP_PREFIX}{suffix}"));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "unable to allocate a unique code-cache temporary file",
    ))
}

fn final_path_is_publishable(path: &std::path::Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(is_safe_owned_file(&metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

fn publish_disk_file(
    namespace: &std::path::Path,
    final_path: &std::path::Path,
    data: &[u8],
) -> std::io::Result<bool> {
    if !final_path_is_publishable(final_path)? {
        return Ok(false);
    }
    let (temp_path, mut temp_file) = create_disk_temp_file(namespace)?;
    if let Err(error) = temp_file
        .write_all(data)
        .and_then(|()| temp_file.sync_all())
    {
        drop(temp_file);
        let _ = std::fs::remove_file(&temp_path);
        return Err(error);
    }
    drop(temp_file);

    if !final_path_is_publishable(final_path)? {
        let _ = std::fs::remove_file(&temp_path);
        return Ok(false);
    }

    #[cfg(windows)]
    if std::fs::symlink_metadata(final_path)
        .ok()
        .is_some_and(|metadata| is_safe_owned_file(&metadata))
    {
        // Windows rename does not replace an existing file. Readers take the
        // same namespace lock, so this short remove/rename window cannot
        // expose a partial file; a crash can leave a miss, which is safe.
        std::fs::remove_file(final_path)?;
    }

    match std::fs::rename(&temp_path, final_path) {
        Ok(()) => Ok(true),
        Err(error) => {
            let _ = std::fs::remove_file(&temp_path);
            Err(error)
        }
    }
}

impl InMemoryCodeCache {
    fn read_disk(&self, key: &CodeCacheKey) -> Option<Vec<u8>> {
        let namespace = self.ensure_disk_namespace()?;
        let _lock = NamespaceLock::acquire(&namespace)?;
        let path = self.disk_path(key)?;
        let (max_entries, max_bytes) = self.limits;
        if max_entries == 0 {
            return None;
        }
        let max_bytes = u64::try_from(max_bytes).ok()?;
        let metadata = std::fs::symlink_metadata(&path).ok()?;
        if !is_safe_owned_file(&metadata) {
            return None;
        }
        if metadata.len() > max_bytes {
            let removed = remove_owned_disk_file(&path).ok().unwrap_or(false);
            if removed {
                let _ = maintain_disk_locked(&namespace, max_entries, max_bytes as usize);
            }
            return None;
        }

        let file = File::open(&path).ok()?;
        let mut data = Vec::new();
        if file
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut data)
            .is_err()
        {
            return None;
        }
        if data.len() > max_bytes as usize {
            let removed = remove_owned_disk_file(&path).ok().unwrap_or(false);
            if removed {
                let _ = maintain_disk_locked(&namespace, max_entries, max_bytes as usize);
            }
            return None;
        }
        Some(data)
    }

    fn write_disk(&self, key: &CodeCacheKey, data: &[u8]) {
        let Some(namespace) = self.ensure_disk_namespace() else {
            return;
        };
        let Some(_lock) = NamespaceLock::acquire(&namespace) else {
            return;
        };
        let (max_entries, max_bytes) = self.limits;
        // A complete maintenance pass is required before publishing. If the
        // scan or an eviction fails, the disk tier remains best-effort and the
        // new value is not published with uncertain bounds.
        if maintain_disk_locked(&namespace, max_entries, max_bytes).is_err()
            || max_entries == 0
            || data.len() > max_bytes
        {
            return;
        }
        let Some(path) = self.disk_path(key) else {
            return;
        };
        if let Ok(true) = publish_disk_file(&namespace, &path, data) {
            // The write itself is a mutation; maintain again while still
            // holding the lock so every successful write leaves both
            // production limits enforceable.
            if maintain_disk_locked(&namespace, max_entries, max_bytes).is_err() {
                let _ = remove_owned_disk_file(&path);
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
        self.read_disk(&key)
    }

    fn set_sync(
        &self,
        specifier: ModuleSpecifier,
        code_cache_type: CodeCacheType,
        source_hash: u64,
        data: &[u8],
    ) {
        let key = (specifier.as_str().to_owned(), code_cache_type, source_hash);
        let disk_key = self.disk_dir.as_ref().map(|_| key.clone());
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
        if let Some(disk_key) = disk_key {
            // A zero-capacity or oversized insert cannot be published, but a
            // locked maintenance pass still cleans owned old entries.
            self.write_disk(&disk_key, data);
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
) -> Result<Result<(), crate::LibdenoError>, ExecutionTermination> {
    let _user_execution = timing.span(Phase::UserExecution);
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
    let result =
        run_with_deadline_cancellable(run, execution_deadline, isolate_handle, cancellation).await;
    worker.run_napi_ref_finalizers();
    result
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

    fn disk_test_root(name: &str) -> PathBuf {
        let id = CODE_CACHE_DISK_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "libdeno-v8-code-cache-v3-{}-{id}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn disk_test_specifier(index: u64) -> ModuleSpecifier {
        ModuleSpecifier::parse(&format!("file:///libdeno-v8-code-cache-v3/{index}.js")).unwrap()
    }

    fn disk_test_paths(root: &std::path::Path) -> Vec<PathBuf> {
        let namespace = root.join(CODE_CACHE_NAMESPACE);
        let mut paths = std::fs::read_dir(namespace)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_lower_hex_bin_name)
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    fn disk_test_cleanup(root: PathBuf) {
        let _ = std::fs::remove_dir_all(root);
    }

    fn disk_test_set(cache: &InMemoryCodeCache, index: u64, data: &[u8]) {
        cache.set_sync(
            disk_test_specifier(index),
            CodeCacheType::Script,
            index,
            data,
        );
    }

    fn disk_test_set_modified(cache: &InMemoryCodeCache, index: u64, seconds_ago: u64) {
        let path = cache
            .disk_path(&(
                disk_test_specifier(index).as_str().to_owned(),
                CodeCacheType::Script,
                index,
            ))
            .unwrap();
        File::open(path)
            .unwrap()
            .set_modified(
                SystemTime::now()
                    .checked_sub(Duration::from_secs(seconds_ago))
                    .unwrap(),
            )
            .unwrap();
    }

    #[cfg(any(unix, windows))]
    struct TestChild(std::process::Child);

    #[cfg(any(unix, windows))]
    impl Drop for TestChild {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    #[cfg(any(unix, windows))]
    const DISK_WRITER_ROOT_ENV: &str = "LIBDENO_V8_CODE_CACHE_WRITER_ROOT";
    #[cfg(any(unix, windows))]
    const DISK_WRITER_ID_ENV: &str = "LIBDENO_V8_CODE_CACHE_WRITER_ID";
    #[cfg(any(unix, windows))]
    const DISK_WRITER_KEY_ENV: &str = "LIBDENO_V8_CODE_CACHE_WRITER_KEY";
    #[cfg(any(unix, windows))]
    const DISK_WRITER_BYTE_ENV: &str = "LIBDENO_V8_CODE_CACHE_WRITER_BYTE";
    #[cfg(any(unix, windows))]
    const DISK_WRITER_SIZE_ENV: &str = "LIBDENO_V8_CODE_CACHE_WRITER_SIZE";
    #[cfg(any(unix, windows))]
    const DISK_WRITER_ITERATIONS_ENV: &str = "LIBDENO_V8_CODE_CACHE_WRITER_ITERATIONS";
    #[cfg(any(unix, windows))]
    const DISK_WRITER_MAX_ENTRIES_ENV: &str = "LIBDENO_V8_CODE_CACHE_WRITER_MAX_ENTRIES";
    #[cfg(any(unix, windows))]
    const DISK_WRITER_MAX_BYTES_ENV: &str = "LIBDENO_V8_CODE_CACHE_WRITER_MAX_BYTES";
    #[cfg(any(unix, windows))]
    const DISK_WRITER_READY_PREFIX: &str = ".libdeno-v8-code-cache-writer-ready-";
    #[cfg(any(unix, windows))]
    const DISK_WRITER_START_NAME: &str = ".libdeno-v8-code-cache-writer-start";

    #[cfg(any(unix, windows))]
    #[allow(clippy::too_many_arguments)]
    fn spawn_disk_cache_writer(
        root: &std::path::Path,
        id: u64,
        key: u64,
        byte: u8,
        size: usize,
        iterations: u64,
        max_entries: usize,
        max_bytes: usize,
    ) -> TestChild {
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "limits::tests::disk_cache_process_writer",
                "--nocapture",
            ])
            .env(DISK_WRITER_ROOT_ENV, root)
            .env(DISK_WRITER_ID_ENV, id.to_string())
            .env(DISK_WRITER_KEY_ENV, key.to_string())
            .env(DISK_WRITER_BYTE_ENV, byte.to_string())
            .env(DISK_WRITER_SIZE_ENV, size.to_string())
            .env(DISK_WRITER_ITERATIONS_ENV, iterations.to_string())
            .env(DISK_WRITER_MAX_ENTRIES_ENV, max_entries.to_string())
            .env(DISK_WRITER_MAX_BYTES_ENV, max_bytes.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        TestChild(child)
    }

    #[cfg(any(unix, windows))]
    fn wait_for_disk_cache_writers(root: &std::path::Path, writer_count: u64) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while (0..writer_count).any(|id| {
            !root
                .join(format!("{DISK_WRITER_READY_PREFIX}{id}"))
                .exists()
        }) {
            assert!(
                Instant::now() < deadline,
                "writer children never became ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(any(unix, windows))]
    fn disk_test_owned_temp_paths(root: &std::path::Path) -> Vec<PathBuf> {
        std::fs::read_dir(root.join(CODE_CACHE_NAMESPACE))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(is_owned_temp_name)
            })
            .collect()
    }

    #[test]
    fn disk_cache_repeated_writes_enforce_count_and_bytes() {
        let root = disk_test_root("repeated");
        let cache = InMemoryCodeCache::with_disk_limits(root.clone(), 3, 10);
        for index in 0..12 {
            disk_test_set(&cache, index, &[index as u8; 4]);
            let files = disk_test_paths(&root);
            assert!(files.len() <= 3);
            assert!(
                files
                    .iter()
                    .map(|path| std::fs::metadata(path).unwrap().len())
                    .sum::<u64>()
                    <= 10
            );
        }
        disk_test_cleanup(root);
    }

    #[test]
    fn disk_cache_evicts_oldest_by_count_and_bytes() {
        let count_root = disk_test_root("count-oldest");
        let count_cache = InMemoryCodeCache::with_disk_limits(count_root.clone(), 2, 100);
        disk_test_set(&count_cache, 0, b"zero");
        disk_test_set_modified(&count_cache, 0, 3);
        disk_test_set(&count_cache, 1, b"one");
        disk_test_set_modified(&count_cache, 1, 2);
        disk_test_set(&count_cache, 2, b"two");
        let cold_count = InMemoryCodeCache::with_disk_limits(count_root.clone(), 2, 100);
        assert!(cold_count
            .get_sync(&disk_test_specifier(0), CodeCacheType::Script, 0)
            .is_none());
        assert_eq!(
            cold_count
                .get_sync(&disk_test_specifier(1), CodeCacheType::Script, 1)
                .as_deref(),
            Some(b"one".as_slice())
        );
        assert_eq!(
            cold_count
                .get_sync(&disk_test_specifier(2), CodeCacheType::Script, 2)
                .as_deref(),
            Some(b"two".as_slice())
        );

        let byte_root = disk_test_root("bytes-oldest");
        let byte_cache = InMemoryCodeCache::with_disk_limits(byte_root.clone(), 10, 5);
        disk_test_set(&byte_cache, 0, b"00");
        disk_test_set_modified(&byte_cache, 0, 3);
        disk_test_set(&byte_cache, 1, b"11");
        disk_test_set_modified(&byte_cache, 1, 2);
        disk_test_set(&byte_cache, 2, b"22");
        let cold_bytes = InMemoryCodeCache::with_disk_limits(byte_root.clone(), 10, 5);
        assert!(cold_bytes
            .get_sync(&disk_test_specifier(0), CodeCacheType::Script, 0)
            .is_none());
        assert!(cold_bytes
            .get_sync(&disk_test_specifier(1), CodeCacheType::Script, 1)
            .is_some());
        assert!(cold_bytes
            .get_sync(&disk_test_specifier(2), CodeCacheType::Script, 2)
            .is_some());
        disk_test_cleanup(count_root);
        disk_test_cleanup(byte_root);
    }

    #[test]
    fn disk_cache_equal_mtime_uses_filename_tie_break() {
        let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let path = PathBuf::from("/tmp");
        let mut files = vec![
            OwnedDiskFile {
                name: "000000000000000b.bin".to_string(),
                path: path.clone(),
                len: 1,
                modified: Some(mtime),
            },
            OwnedDiskFile {
                name: "000000000000000a.bin".to_string(),
                path,
                len: 1,
                modified: Some(mtime),
            },
        ];
        sort_owned_disk_files(&mut files);
        assert_eq!(files[0].name, "000000000000000a.bin");

        files[0].modified = None;
        sort_owned_disk_files(&mut files);
        assert!(files[0].modified.is_none());
    }

    #[test]
    fn disk_cache_replacement_is_a_new_write_for_age() {
        let root = disk_test_root("replacement-age");
        let cache = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 100);
        disk_test_set(&cache, 0, b"old");
        disk_test_set_modified(&cache, 0, 4);
        disk_test_set(&cache, 1, b"one");
        disk_test_set_modified(&cache, 1, 3);
        let replacement_path = cache
            .disk_path(&(
                disk_test_specifier(0).as_str().to_owned(),
                CodeCacheType::Script,
                0,
            ))
            .unwrap();
        let before = std::fs::metadata(&replacement_path)
            .unwrap()
            .modified()
            .unwrap();
        disk_test_set(&cache, 0, b"new");
        let after = std::fs::metadata(&replacement_path)
            .unwrap()
            .modified()
            .unwrap();
        assert!(after > before, "replacement must refresh modified time");
        disk_test_set_modified(&cache, 0, 2);
        disk_test_set(&cache, 2, b"two");
        let cold = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 100);
        assert_eq!(
            cold.get_sync(&disk_test_specifier(0), CodeCacheType::Script, 0)
                .as_deref(),
            Some(b"new".as_slice())
        );
        assert!(cold
            .get_sync(&disk_test_specifier(1), CodeCacheType::Script, 1)
            .is_none());
        assert!(cold
            .get_sync(&disk_test_specifier(2), CodeCacheType::Script, 2)
            .is_some());
        disk_test_cleanup(root);
    }

    #[test]
    fn disk_cache_reads_do_not_refresh_age() {
        let root = disk_test_root("read-age");
        let writer = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 100);
        disk_test_set(&writer, 0, b"zero");
        disk_test_set_modified(&writer, 0, 4);
        disk_test_set(&writer, 1, b"one");
        disk_test_set_modified(&writer, 1, 3);
        let path = writer
            .disk_path(&(
                disk_test_specifier(0).as_str().to_owned(),
                CodeCacheType::Script,
                0,
            ))
            .unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let reader = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 100);
        assert!(reader
            .get_sync(&disk_test_specifier(0), CodeCacheType::Script, 0)
            .is_some());
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            before
        );
        disk_test_set(&writer, 2, b"two");
        let cold = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 100);
        assert!(cold
            .get_sync(&disk_test_specifier(0), CodeCacheType::Script, 0)
            .is_none());
        disk_test_cleanup(root);
    }

    #[test]
    fn disk_cache_exact_oversized_and_zero_capacity_limits() {
        let exact_root = disk_test_root("exact");
        let exact = InMemoryCodeCache::with_disk_limits(exact_root.clone(), 1, 4);
        disk_test_set(&exact, 0, b"1234");
        assert_eq!(disk_test_paths(&exact_root).len(), 1);
        let exact_cold = InMemoryCodeCache::with_disk_limits(exact_root.clone(), 1, 4);
        assert_eq!(
            exact_cold
                .get_sync(&disk_test_specifier(0), CodeCacheType::Script, 0)
                .as_deref(),
            Some(b"1234".as_slice())
        );

        let oversized_root = disk_test_root("oversized");
        let oversized = InMemoryCodeCache::with_disk_limits(oversized_root.clone(), 1, 4);
        disk_test_set(&oversized, 0, b"12345");
        assert!(disk_test_paths(&oversized_root).is_empty());

        let zero_root = disk_test_root("zero");
        let zero = InMemoryCodeCache::with_disk_limits(zero_root.clone(), 0, 4);
        disk_test_set(&zero, 0, b"1234");
        assert!(disk_test_paths(&zero_root).is_empty());
        let zero_cold = InMemoryCodeCache::with_disk_limits(zero_root.clone(), 0, 4);
        assert!(zero_cold
            .get_sync(&disk_test_specifier(0), CodeCacheType::Script, 0)
            .is_none());

        disk_test_cleanup(exact_root);
        disk_test_cleanup(oversized_root);
        disk_test_cleanup(zero_root);
    }

    #[test]
    fn disk_cache_bounded_oversized_read_is_a_miss_and_cleanup_candidate() {
        let root = disk_test_root("bounded-read");
        let cache = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 4);
        let key = (
            disk_test_specifier(0).as_str().to_owned(),
            CodeCacheType::Script,
            0,
        );
        let namespace = cache.ensure_disk_namespace().unwrap();
        let path = cache.disk_path(&key).unwrap();
        std::fs::write(&path, [0u8; 32]).unwrap();
        let cold = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 4);
        assert!(cold
            .get_sync(&disk_test_specifier(0), CodeCacheType::Script, 0)
            .is_none());
        assert!(!path.exists());
        let entries = std::fs::read_dir(namespace)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![CODE_CACHE_LOCK_NAME.to_string()]);
        disk_test_cleanup(root);
    }

    #[test]
    fn disk_cache_warm_read_keeps_type_and_hash_in_key() {
        let root = disk_test_root("warm-key");
        let writer = InMemoryCodeCache::with_disk_limits(root.clone(), 4, 100);
        let specifier = disk_test_specifier(0);
        writer.set_sync(specifier.clone(), CodeCacheType::Script, 7, b"warm");
        let reader = InMemoryCodeCache::with_disk_limits(root.clone(), 4, 100);
        assert_eq!(
            reader
                .get_sync(&specifier, CodeCacheType::Script, 7)
                .as_deref(),
            Some(b"warm".as_slice())
        );
        assert!(reader
            .get_sync(&specifier, CodeCacheType::EsModule, 7)
            .is_none());
        assert!(reader
            .get_sync(&specifier, CodeCacheType::Script, 8)
            .is_none());
        disk_test_cleanup(root);
    }

    #[test]
    fn disk_cache_namespace_preserves_foreign_and_unknown_entries() {
        let root = disk_test_root("namespace");
        let legacy = root.join("0123456789abcdef.bin");
        std::fs::write(&legacy, b"legacy").unwrap();
        let sibling = root.join("sibling-cache");
        std::fs::create_dir(&sibling).unwrap();
        let sibling_file = sibling.join("0123456789abcdef.bin");
        std::fs::write(&sibling_file, b"sibling").unwrap();

        let cache = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 100);
        let namespace = cache.ensure_disk_namespace().unwrap();
        let foreign = namespace.join("foreign.bin");
        let uppercase = namespace.join("0123456789ABCDEF.bin");
        let text = namespace.join("0123456789abcdef.txt");
        let unknown_temp = namespace.join("foreign-tmp-keep");
        std::fs::write(&foreign, b"foreign").unwrap();
        std::fs::write(&uppercase, b"uppercase").unwrap();
        std::fs::write(&text, b"text").unwrap();
        std::fs::write(&unknown_temp, b"temp").unwrap();

        let protected_dir = cache
            .disk_path(&(
                disk_test_specifier(0).as_str().to_owned(),
                CodeCacheType::Script,
                0,
            ))
            .unwrap();
        std::fs::create_dir(&protected_dir).unwrap();
        disk_test_set(&cache, 0, b"blocked");
        disk_test_set(&cache, 1, b"owned");

        assert!(protected_dir.is_dir());
        assert_eq!(std::fs::read(&legacy).unwrap(), b"legacy");
        assert_eq!(std::fs::read(&sibling_file).unwrap(), b"sibling");
        assert_eq!(std::fs::read(&foreign).unwrap(), b"foreign");
        assert_eq!(std::fs::read(&uppercase).unwrap(), b"uppercase");
        assert_eq!(std::fs::read(&text).unwrap(), b"text");
        assert_eq!(std::fs::read(&unknown_temp).unwrap(), b"temp");
        disk_test_cleanup(root);
    }

    #[test]
    fn disk_cache_cleans_only_owned_stale_temps() {
        let root = disk_test_root("temp-cleanup");
        let cache = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 100);
        let namespace = cache.ensure_disk_namespace().unwrap();
        let owned_temp = namespace.join(format!("{CODE_CACHE_TEMP_PREFIX}stale"));
        let foreign_temp = namespace.join(".libdeno-v8-code-cache-v0-tmp-stale");
        let owned_dir = namespace.join(format!("{CODE_CACHE_TEMP_PREFIX}directory"));
        std::fs::write(&owned_temp, b"stale").unwrap();
        std::fs::write(&foreign_temp, b"foreign").unwrap();
        std::fs::create_dir(&owned_dir).unwrap();
        disk_test_set(&cache, 0, b"owned");
        assert!(!owned_temp.exists());
        assert!(foreign_temp.exists());
        assert!(owned_dir.is_dir());
        disk_test_cleanup(root);
    }

    #[cfg(unix)]
    #[test]
    fn disk_cache_preserves_final_symlinks() {
        use std::os::unix::fs::symlink;

        let root = disk_test_root("symlink");
        let cache = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 100);
        let target = root.join("target");
        std::fs::write(&target, b"target").unwrap();
        let link = cache
            .disk_path(&(
                disk_test_specifier(0).as_str().to_owned(),
                CodeCacheType::Script,
                0,
            ))
            .unwrap();
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        symlink(&target, &link).unwrap();
        disk_test_set(&cache, 0, b"replacement");
        let metadata = std::fs::symlink_metadata(&link).unwrap();
        assert!(metadata.file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), target);
        assert_eq!(std::fs::read(&target).unwrap(), b"target");
        disk_test_cleanup(root);
    }

    #[cfg(windows)]
    #[test]
    fn disk_cache_preserves_reparse_namespace_junction() {
        let root = disk_test_root("junction-namespace");
        let outside = disk_test_root("junction-target");
        let namespace = root.join(CODE_CACHE_NAMESPACE);
        let outside_file = outside.join("0000000000000000.bin");
        std::fs::write(&outside_file, b"outside").unwrap();
        let command = format!(
            "mklink /J \"{}\" \"{}\"",
            namespace.display(),
            outside.display()
        );
        let status = std::process::Command::new("cmd")
            .args(["/C", &command])
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J failed: {status}");

        let cache = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 100);
        disk_test_set(&cache, 1, b"must-not-follow-junction");

        let metadata = std::fs::symlink_metadata(&namespace).unwrap();
        assert!(is_reparse_point(&metadata));
        assert_eq!(std::fs::read(&outside_file).unwrap(), b"outside");
        let outside_entries = std::fs::read_dir(&outside)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            outside_entries,
            vec![outside_file.file_name().unwrap().to_owned()]
        );
        disk_test_cleanup(root);
        disk_test_cleanup(outside);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn disk_cache_lock_contention_is_nonblocking_and_releases_on_drop() {
        let root = disk_test_root("lock-contention");
        let cache = InMemoryCodeCache::with_disk_limits(root.clone(), 1, 20);
        let namespace = cache.ensure_disk_namespace().unwrap();
        let lock_path = namespace.join(CODE_CACHE_LOCK_NAME);
        let first = NamespaceLock::acquire(&namespace)
            .expect("native lock acquisition must work on the local test filesystem");
        assert!(lock_path.is_file());
        assert!(NamespaceLock::acquire(&namespace).is_none());
        drop(first);
        assert!(NamespaceLock::acquire(&namespace).is_some());
        assert!(lock_path.is_file());
        disk_test_cleanup(root);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn disk_cache_child_death_releases_os_lock() {
        const CHILD_ROOT_ENV: &str = "LIBDENO_V8_CODE_CACHE_LOCK_CHILD_ROOT";
        const READY_NAME: &str = "lock-child-ready";

        if let Some(root) = std::env::var_os(CHILD_ROOT_ENV) {
            let cache = InMemoryCodeCache::with_disk_limits(PathBuf::from(root), 1, 20);
            let namespace = cache.ensure_disk_namespace().unwrap();
            let _lock = NamespaceLock::acquire(&namespace)
                .expect("native lock acquisition must work on the local test filesystem");
            std::fs::write(namespace.parent().unwrap().join(READY_NAME), b"ready").unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }

        let root = disk_test_root("lock-child");
        let cache = InMemoryCodeCache::with_disk_limits(root.clone(), 1, 20);
        let namespace = cache.ensure_disk_namespace().unwrap();
        let ready = root.join(READY_NAME);
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "limits::tests::disk_cache_child_death_releases_os_lock",
                "--nocapture",
            ])
            .env(CHILD_ROOT_ENV, &root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut child = TestChild(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if ready.exists() {
                break;
            }
            if let Some(status) = child.0.try_wait().unwrap() {
                panic!("lock-holder child exited before signaling readiness: {status}");
            }
            assert!(
                Instant::now() < deadline,
                "lock-holder child never became ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(NamespaceLock::acquire(&namespace).is_none());
        let _ = child.0.kill();
        let _ = child.0.wait();
        assert!(NamespaceLock::acquire(&namespace).is_some());
        disk_test_cleanup(root);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn disk_cache_process_writer() {
        let Some(root) = std::env::var_os(DISK_WRITER_ROOT_ENV) else {
            return;
        };
        let root = PathBuf::from(root);
        let id: u64 = std::env::var(DISK_WRITER_ID_ENV).unwrap().parse().unwrap();
        let key: u64 = std::env::var(DISK_WRITER_KEY_ENV).unwrap().parse().unwrap();
        let byte: u8 = std::env::var(DISK_WRITER_BYTE_ENV)
            .unwrap()
            .parse()
            .unwrap();
        let size: usize = std::env::var(DISK_WRITER_SIZE_ENV)
            .unwrap()
            .parse()
            .unwrap();
        let iterations: u64 = std::env::var(DISK_WRITER_ITERATIONS_ENV)
            .unwrap()
            .parse()
            .unwrap();
        let max_entries: usize = std::env::var(DISK_WRITER_MAX_ENTRIES_ENV)
            .unwrap()
            .parse()
            .unwrap();
        let max_bytes: usize = std::env::var(DISK_WRITER_MAX_BYTES_ENV)
            .unwrap()
            .parse()
            .unwrap();
        let cache = InMemoryCodeCache::with_disk_limits(root.clone(), max_entries, max_bytes);
        cache
            .ensure_disk_namespace()
            .expect("native writer child must create a safe namespace");
        std::fs::write(
            root.join(format!("{DISK_WRITER_READY_PREFIX}{id}")),
            b"ready",
        )
        .unwrap();
        let start = root.join(DISK_WRITER_START_NAME);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !start.exists() {
            assert!(
                Instant::now() < deadline,
                "writer parent never released children"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let data = vec![byte; size];
        for _ in 0..iterations {
            cache.set_sync(disk_test_specifier(key), CodeCacheType::Script, key, &data);
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn disk_cache_same_key_writers_publish_complete_values() {
        let root = disk_test_root("same-key");
        let left = vec![b'L'; 4096];
        let right = vec![b'R'; 4096];
        let mut children = vec![
            spawn_disk_cache_writer(&root, 0, 0, b'L', left.len(), 8, 2, 10_000),
            spawn_disk_cache_writer(&root, 1, 0, b'R', right.len(), 8, 2, 10_000),
        ];
        wait_for_disk_cache_writers(&root, 2);
        std::fs::write(root.join(DISK_WRITER_START_NAME), b"start").unwrap();
        for child in &mut children {
            assert!(child.0.wait().unwrap().success());
        }
        let files = disk_test_paths(&root);
        assert_eq!(files.len(), 1);
        let value = std::fs::read(&files[0]).unwrap();
        assert!(value == left || value == right);
        assert!(disk_test_owned_temp_paths(&root).is_empty());
        disk_test_cleanup(root);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn disk_cache_distinct_key_writers_converge_within_caps() {
        let root = disk_test_root("different-keys");
        let mut children = (0..6u64)
            .map(|index| spawn_disk_cache_writer(&root, index, index, index as u8, 64, 4, 3, 192))
            .collect::<Vec<_>>();
        wait_for_disk_cache_writers(&root, 6);
        std::fs::write(root.join(DISK_WRITER_START_NAME), b"start").unwrap();
        for child in &mut children {
            assert!(child.0.wait().unwrap().success());
        }
        let files = disk_test_paths(&root);
        assert!(!files.is_empty(), "concurrent writers must publish a value");
        assert!(files.len() <= 3);
        assert!(
            files
                .iter()
                .map(|path| std::fs::metadata(path).unwrap().len())
                .sum::<u64>()
                <= 192
        );
        for path in files {
            let value = std::fs::read(path).unwrap();
            assert_eq!(value.len(), 64);
            assert!(value.iter().all(|byte| *byte == value[0]));
            assert!(value[0] < 6);
        }
        assert!(disk_test_owned_temp_paths(&root).is_empty());
        disk_test_cleanup(root);
    }

    #[test]
    fn disk_cache_failures_leave_memory_result_available() {
        let invalid_parent = disk_test_root("invalid-parent");
        let invalid_file = invalid_parent.join("not-a-directory");
        std::fs::write(&invalid_file, b"file").unwrap();
        let invalid = InMemoryCodeCache::with_disk_limits(invalid_file, 2, 20);
        let invalid_specifier = disk_test_specifier(1);
        invalid.set_sync(
            invalid_specifier.clone(),
            CodeCacheType::Script,
            1,
            b"memory",
        );
        assert_eq!(
            invalid.get_sync(&invalid_specifier, CodeCacheType::Script, 1),
            Some(b"memory".to_vec())
        );
        disk_test_cleanup(invalid_parent);

        let lock_root = disk_test_root("lock-failure");
        let lock_cache = InMemoryCodeCache::with_disk_limits(lock_root.clone(), 2, 20);
        let namespace = lock_cache.ensure_disk_namespace().unwrap();
        std::fs::create_dir(namespace.join(CODE_CACHE_LOCK_NAME)).unwrap();
        let lock_specifier = disk_test_specifier(2);
        lock_cache.set_sync(
            lock_specifier.clone(),
            CodeCacheType::Script,
            2,
            b"memory-lock",
        );
        assert_eq!(
            lock_cache.get_sync(&lock_specifier, CodeCacheType::Script, 2),
            Some(b"memory-lock".to_vec())
        );
        assert!(namespace.join(CODE_CACHE_LOCK_NAME).is_dir());
        disk_test_cleanup(lock_root);
    }

    #[cfg(unix)]
    #[test]
    fn disk_cache_read_only_namespace_leaves_memory_result_available() {
        use std::os::unix::fs::PermissionsExt;

        let root = disk_test_root("read-only");
        let cache = InMemoryCodeCache::with_disk_limits(root.clone(), 2, 20);
        let namespace = cache.ensure_disk_namespace().unwrap();
        let mut permissions = std::fs::metadata(&namespace).unwrap().permissions();
        permissions.set_mode(0o555);
        std::fs::set_permissions(&namespace, permissions).unwrap();
        let specifier = disk_test_specifier(3);
        cache.set_sync(specifier.clone(), CodeCacheType::Script, 3, b"memory-ro");
        assert_eq!(
            cache.get_sync(&specifier, CodeCacheType::Script, 3),
            Some(b"memory-ro".to_vec())
        );
        let mut permissions = std::fs::metadata(&namespace).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&namespace, permissions).unwrap();
        disk_test_cleanup(root);
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
