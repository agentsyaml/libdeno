// Process-level reuse of the resolver stack: `LibdenoRuntime` builds the
// permission-free half of the module pipeline (workspace/resolver/npm
// installer factories, graph resolver, npm process state) once, and
// `run_with` reuses it across script runs instead of rebuilding it every
// time. The stack is rebuilt automatically when the project's config chain
// changes (fingerprint check). Permission-bound pieces (the file fetcher,
// the graph loader and the module graph) stay strictly per-run in
// `RuntimeServices` — see services.rs.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::services::SharedServices;
use crate::LibdenoError;
use crate::LibdenoOptions;
use crate::CWD_LOCK;

/// A reusable resolver stack scoped to a project directory.
///
/// [`LibdenoRuntime::new`] builds the permission-free half of the module
/// pipeline once; [`run_with`] then reuses it across runs. The stack is
/// rebuilt automatically when the config discovery chain changes (deno.json /
/// deno.jsonc / import_map.json / package.json / .npmrc / node_modules at the
/// project root and its ancestors), so long-lived hosts serving the same
/// project skip the per-run factory construction entirely.
///
/// The runtime is single-threaded by design: the module loader stack is
/// `Rc<dyn ModuleLoader>`-based and every run executes on a fresh
/// current-thread tokio runtime. `LibdenoRuntime` itself is `Clone` + `Send` +
/// `Sync` (its only state is an `Arc<Mutex<RuntimeState>>` around the resolver
/// stack), so it can be shared across host threads and serialized by
/// [`run_with`]'s cwd lock.
#[derive(Clone)]
pub struct LibdenoRuntime {
    cwd: PathBuf,
    /// The current resolver stack and the fingerprint it was built for;
    /// `run_with` recomputes the fingerprint and swaps `shared` under the
    /// guard when they diverge (the rebuild itself happens outside the lock).
    state: Arc<std::sync::Mutex<RuntimeState>>,
}

struct RuntimeState {
    fingerprint: Vec<(u64, u64)>,
    shared: Arc<SharedServices>,
}

impl LibdenoRuntime {
    /// Builds the resolver stack for `cwd` once. Later [`run_with`] calls on
    /// this runtime skip the factory construction unless the config chain
    /// changed. `cwd` is canonicalized; it becomes the working directory of
    /// every script run through this runtime.
    pub async fn new(cwd: impl AsRef<Path>) -> Result<Self, LibdenoError> {
        let cwd =
            std::fs::canonicalize(cwd.as_ref()).unwrap_or_else(|_| cwd.as_ref().to_path_buf());
        // Discovery starts at the runtime's cwd: scripts run inside it resolve
        // against the same deno.json / package.json / node_modules chain.
        let shared = SharedServices::new(cwd.clone(), vec![cwd.clone()])
            .await
            .map_err(LibdenoError::Runtime)?;
        let fingerprint = config_fingerprint(&cwd);
        Ok(Self {
            cwd,
            state: Arc::new(std::sync::Mutex::new(RuntimeState {
                fingerprint,
                shared,
            })),
        })
    }
}

/// Runs `entry` through a prebuilt [`LibdenoRuntime`]'s resolver stack.
///
/// Semantics match [`crate::run`]: the run is serialized on the process cwd
/// lock, tokio re-entry is handled automatically (the run executes on a
/// fresh thread when called from inside a tokio runtime), `Deno.exit(n)` /
/// exit codes / deadlines behave identically, and the run's permissions come
/// from `options` — each run rebuilds its permission-bound file fetcher /
/// graph loader / graph, so one run's grants can never leak into another.
///
/// The script runs in the runtime's cwd; `LibdenoOptions.cwd` is ignored
/// (the resolver stack is scoped to the runtime's directory).
///
/// `LibdenoOptions.capture_stdout` / `capture_stderr` are **not** honored by
/// `run_with` (it returns only the exit code); use [`run_with_output`] for
/// capture on the reusable stack.
///
/// The async/sync split is deliberate: [`LibdenoRuntime::new`] is async
/// because resolver stack construction needs a tokio context, while `run_with`
/// is sync and does its own `block_on` on a fresh current-thread runtime.
pub fn run_with(
    runtime: &LibdenoRuntime,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
) -> Result<i32, LibdenoError> {
    // The process cwd is process-global; serialize like run() (see CWD_LOCK).
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Capture the entry-time child-IPC marker (fork children inherit it).
    crate::limits::capture_spawned_ipc_marker();
    let entry = entry.as_ref().to_path_buf();
    let options = options.clone();
    let runtime = runtime.clone();
    // Same tokio re-entry handling as run(): building a runtime inside a
    // tokio runtime panics, so run on a fresh thread instead of rejecting
    // async hosts.
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || run_with_sync(&runtime, &entry, &options))
            .join()
            .map_err(|_| {
                LibdenoError::Runtime(deno_core::anyhow::anyhow!("libdeno worker thread panicked"))
            })?
    } else {
        run_with_sync(&runtime, &entry, &options)
    }
}

/// Runs `entry` through a prebuilt [`LibdenoRuntime`]'s resolver stack and
/// returns the exit code together with the captured stdout/stderr when
/// `LibdenoOptions.capture_stdout` / `capture_stderr` are set — the
/// long-lived-host equivalent of [`crate::run_with_output`] (which rebuilds
/// the resolver stack on every call).
///
/// Everything else matches [`run_with`]: serialized on the cwd lock, tokio
/// re-entry handled automatically, `LibdenoOptions.cwd` ignored, permissions
/// per-run. Capture semantics (fd-level redirection, byte cap, Windows
/// rejection) are identical to [`crate::run_with_output`].
pub fn run_with_output(
    runtime: &LibdenoRuntime,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
) -> Result<crate::RunOutput, LibdenoError> {
    let _lock = CWD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    crate::limits::capture_spawned_ipc_marker();
    let entry = entry.as_ref().to_path_buf();
    let options = options.clone();
    let runtime = runtime.clone();
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || run_with_sync_output(&runtime, &entry, &options))
            .join()
            .map_err(|_| {
                LibdenoError::Runtime(deno_core::anyhow::anyhow!("libdeno worker thread panicked"))
            })?
    } else {
        run_with_sync_output(&runtime, &entry, &options)
    }
}

/// Capture + [`run_with_sync`]; the Windows rejection and byte-cap handling
/// mirror `crate::run_sync_output`.
fn run_with_sync_output(
    runtime: &LibdenoRuntime,
    entry: &Path,
    options: &LibdenoOptions,
) -> Result<crate::RunOutput, LibdenoError> {
    #[cfg(windows)]
    if options.capture_stdout || options.capture_stderr {
        return Err(LibdenoError::Configuration(
            "output capture is not supported on Windows (std stdout/stderr \
             bypass the redirected CRT fd); use run_in_subprocess and pipe \
             the child's output instead"
                .to_string(),
        ));
    }
    let capture = crate::output::OutputCapture::new(
        options.capture_stdout,
        options.capture_stderr,
        options.max_capture_bytes,
    )
    .map_err(LibdenoError::Io)?;
    let result = run_with_sync(runtime, entry, options);
    let (stdout, stderr, capture_truncated) = capture.finish();
    Ok(crate::RunOutput {
        exit_code: result?,
        stdout,
        stderr,
        capture_truncated,
    })
}

/// The actual run against a prebuilt resolver stack: its own block_on on a
/// fresh current-thread runtime. Must not be called from inside a tokio
/// runtime; [`run_with`] routes such callers onto a fresh thread first.
fn run_with_sync(
    runtime: &LibdenoRuntime,
    entry: &Path,
    options: &LibdenoOptions,
) -> Result<i32, LibdenoError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))?;
    rt.block_on(async {
        // Fresh fingerprint of the config discovery chain. The swap lock
        // covers only the check and the swap, never the rebuild: a slow
        // factory rebuild must not be serialized behind anything but CWD_LOCK.
        let fp = config_fingerprint(&runtime.cwd);
        let shared = {
            let stale = {
                let state = runtime.state.lock().unwrap_or_else(|e| e.into_inner());
                fp != state.fingerprint
            };
            if stale {
                let cwd = runtime.cwd.clone();
                let rebuilt = SharedServices::new(cwd.clone(), vec![cwd])
                    .await
                    .map_err(LibdenoError::Runtime)?;
                let mut state = runtime.state.lock().unwrap_or_else(|e| e.into_inner());
                state.fingerprint = fp;
                state.shared = rebuilt.clone();
                rebuilt
            } else {
                runtime
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .shared
                    .clone()
            }
        };
        crate::run_inner_with(shared, runtime.cwd.clone(), entry, options).await
    })
}

/// Fingerprint of the config discovery chain rooted at `cwd`: walking up from
/// the project directory, the content hash of every small config file
/// (deno.json / deno.jsonc / import_map.json / package.json / .npmrc), the
/// (mtime, size) of deno.lock (potentially large, so no content read), plus
/// the (mtime, 0) of every node_modules directory (its mtime moves on direct
/// package add/remove, flipping BYONM <-> managed). `run_with` rebuilds the
/// resolver stack when this changes. The walk order is deterministic, so Vec
/// equality is the comparison.
// ponytail: the node_modules entry only reflects *direct* children (a nested
// package install deep inside the tree does not touch the root dir's mtime);
// add a content tree hash if that case needs invalidation.
fn config_fingerprint(cwd: &Path) -> Vec<(u64, u64)> {
    const CONFIG_FILES: [&str; 5] = [
        "deno.json",
        "deno.jsonc",
        "import_map.json",
        "package.json",
        ".npmrc",
    ];
    let mut entries = Vec::new();
    let mut dir = Some(cwd.to_path_buf());
    while let Some(dir_path) = dir {
        for name in CONFIG_FILES {
            if let Some(fp) = file_fingerprint(&dir_path.join(name)) {
                entries.push(fp);
            }
        }
        // deno.lock is read once at stack construction; an external update
        // (e.g. `deno install`) must rebuild even when package.json is
        // untouched. (mtime, size) is enough — lockfiles are not edited
        // in-place, so same-size same-mtime writes do not occur here.
        if let Some(fp) = lock_fingerprint(&dir_path.join("deno.lock")) {
            entries.push(fp);
        }
        if let Ok(meta) = std::fs::metadata(dir_path.join("node_modules")) {
            if meta.is_dir() {
                if let Some(fp) = meta_fingerprint(&meta) {
                    entries.push((fp, 0));
                }
            }
        }
        let parent = dir_path.parent().map(|p| p.to_path_buf());
        if parent.as_deref() == Some(dir_path.as_path()) {
            break; // reached the filesystem root
        }
        dir = parent;
    }
    entries
}

/// Content hash of a small config file — catches same-size same-mtime edits
/// that (mtime, size) would miss. Config files are tiny, so the read is
/// negligible per `run_with` entry.
fn file_fingerprint(path: &Path) -> Option<(u64, u64)> {
    crate::npm_cache::content_hash(path).map(|hash| (hash, 0))
}

/// (mtime, size) fingerprint for deno.lock: content-hashing a potentially
/// large lockfile on every `run_with` is not worth it (see `config_fingerprint`).
fn lock_fingerprint(path: &Path) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta_fingerprint(&meta)?;
    Some((mtime, meta.len()))
}

fn meta_fingerprint(meta: &std::fs::Metadata) -> Option<u64> {
    meta.modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_nanos() as u64)
}

/// True when the script called `Deno.exit(n)`: op_exit terminated the isolate
/// with the WatcherExited marker set, and the requested code is in the ExitCode
/// op state. (`Deno.exit(0)` is indistinguishable from natural completion.)
pub(crate) fn has_watcher_exited(worker: &deno_runtime::worker::MainWorker) -> bool {
    worker
        .js_runtime
        .op_state()
        .borrow()
        .try_borrow::<deno_runtime::deno_os::WatcherExited>()
        .is_some()
}
