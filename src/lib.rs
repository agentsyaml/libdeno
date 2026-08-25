//! libdeno — embed the Deno runtime in Rust with direct npm support.
//!
//! Runs a JS/TS entry file (or a local package.json project) on the official
//! deno module graph pipeline: npm: specifiers, remote modules, deno.json
//! import maps, jsr:, CJS packages, wasm, .node native addons, web workers
//! and `child_process.fork` are all handled by the graph loader.
//!
//! ```no_run
//! use libdeno::{LibdenoOptions, run};
//!
//! let options = LibdenoOptions {
//!   permissions: vec!["--allow-read=.".into(), "--allow-net=example.com".into()],
//!   args: vec![],
//!   cwd: None,
//!   ..Default::default()
//! };
//! let exit_code = run("app.js", &options).unwrap();
//! ```

mod analysis_cache;
mod deno_resolver_adapter;
mod deno_runtime_adapter;
mod executor;
mod graph;
mod http;
mod limits;
mod module_loader;
mod node_loader;
mod npm_cache;
mod output;
mod permission_broker;
mod permissions;
// Public so `libdeno::runtime::run_with_output` (capture on a reusable
// resolver stack) is reachable; the root re-exports `run_with` /
// `LibdenoRuntime` for the common paths.
pub mod runtime;
mod services;
mod subprocess;
#[cfg(feature = "execution-control")]
mod supervisor;
mod timing;
mod worker_factory;

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use deno_core::error::AnyError;
use deno_core::ModuleSpecifier;
use deno_runtime::deno_fs::FileSystem;
use deno_runtime::deno_fs::RealFs;

#[cfg(feature = "phase-diagnostics")]
#[doc(hidden)]
pub use executor::PhaseDiagnostics;
#[cfg(feature = "execution-control")]
#[doc(hidden)]
pub use executor::{
    AdmissionConfig, CancelOutcome, ExecutionCleanupStrength, ExecutionHandle, ExecutionState,
    ExecutionTransportStatus, ShutdownReport, SubmissionOptions, SubmitError,
};
pub use executor::{
    CapabilityAvailability, CapabilityOutcome, CapabilityReport, ExecutionBackend,
    ExecutionCapability, ExecutionError, ExecutionFailure, ExecutionOutput, ExecutionReport,
    ExecutionRequest, ExecutionResult, Executor, ExecutorBuilder, UnsupportedCapability,
};
pub use permission_broker::{
    install_permission_broker, install_permission_hook, PermissionPrompt, PermissionRequest,
};
pub use runtime::{run_with, LibdenoRuntime};
pub use subprocess::{maybe_handle_child_mode, run_in_subprocess, run_in_subprocess_with_output};
#[cfg(feature = "execution-control")]
#[doc(hidden)]
pub use subprocess::{maybe_handle_supervisor_mode, run_in_supervised_subprocess};

use module_loader::GraphModuleLoader;
use permissions::build_permissions;
use services::{RuntimeServices, SharedServices};
use sys_traits::impls::RealSys;
use timing::{ExecutionTiming, Phase};

/// Concurrency protocol for in-process runs: ordinary runs are fully
/// parallel (each runs its own thread + isolate + graph, sharing nothing
/// mutable — the process-global analysis/npm/on-disk caches are safe shared
/// state), while a captured run is **exclusive** — output capture is
/// fd-level redirection of the process-global stdout/stderr, so any
/// concurrent run (captured or not) would have its output stolen by the
/// capture reader. Rather than serializing everything (the old CWD_LOCK
/// model, which made all concurrent runs wait on the process mutex for the
/// full run duration), a run that needs capture is rejected with a
/// [`LibdenoError::Configuration`] error when any other run is active —
/// capture belongs in the subprocess model, where each process has its own
/// fds.
///
/// State machine in one atomic: `0` idle, `usize::MAX` captured-run
/// exclusive, `n` (1..MAX-1) ordinary runs active. Ordinary runs CAS-count
/// up (rejected while a captured run holds the marker); a captured run
/// CAS-swaps 0 → MAX (rejected otherwise). Lock-free, no spurious
/// serialization.
const CAPTURE_MARKER: usize = usize::MAX;
static RUN_STATE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// RAII lease taken for the duration of a run (before spawning any thread).
struct RunLease {
    /// Whether this lease holds the capture marker (must restore to 0).
    captured: bool,
}

impl RunLease {
    fn acquire(captured: bool) -> Result<Self, LibdenoError> {
        use std::sync::atomic::Ordering::*;
        if captured {
            if RUN_STATE
                .compare_exchange(0, CAPTURE_MARKER, AcqRel, Relaxed)
                .is_err()
            {
                return Err(LibdenoError::Configuration(
                    "output capture is process-global (fd-level redirection) \
                     and cannot coexist with any other run — use \
                     run_in_subprocess_with_output instead, where the \
                     child's own fds are piped and runs stay parallel"
                        .to_string(),
                ));
            }
        } else {
            loop {
                let cur = RUN_STATE.load(Acquire);
                if cur == CAPTURE_MARKER {
                    return Err(LibdenoError::Configuration(
                        "a captured run is active; output capture is \
                         process-global and would steal this run's stdout. \
                         Use run_in_subprocess_with_output for the captured \
                         script, or wait for the captured run to finish"
                            .to_string(),
                    ));
                }
                if RUN_STATE
                    .compare_exchange(cur, cur + 1, AcqRel, Relaxed)
                    .is_ok()
                {
                    break;
                }
            }
        }
        Ok(Self { captured })
    }
}

impl Drop for RunLease {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::*;
        if self.captured {
            RUN_STATE.store(0, Release);
        } else {
            RUN_STATE.fetch_sub(1, AcqRel);
        }
    }
}

// Generated by build.rs: the V8 snapshot and the residual lazy-load sources.
include!(concat!(env!("OUT_DIR"), "/EXTENSION_RESIDUAL_SOURCES.rs"));
static STARTUP_SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/CLI_SNAPSHOT.bin"));

/// Options for a single [`run`] invocation.
#[derive(Debug, Clone, Default)]
pub struct LibdenoOptions {
    /// Permission capability strings in `--allow-*` CLI format, e.g.
    /// `"--allow-read=./src"`, `"--allow-net=example.com:8080"`.
    ///
    /// Since v0.2.0 an empty list is a construction error — it no longer
    /// grants anything — unless [`Self::allow_all_permissions`] is set.
    /// Passing any entry restricts the runtime to the declared capabilities; a
    /// flag without a value allows that capability globally. `-A`/`--allow-all`
    /// is equivalent to `allow_all_permissions`.
    pub permissions: Vec<String>,
    /// Explicitly grant every capability (`-A` equivalent). Required to run
    /// scripts with an empty `permissions` list — since v0.2.0 an empty list
    /// no longer grants anything; it is a construction error unless this flag
    /// is set. Use it only for code you trust (see SECURITY.md).
    pub allow_all_permissions: bool,
    /// Interactive permission prompting for non-granted queries, mirroring
    /// `deno run`'s default behavior: the check prints to stderr and reads
    /// allow/deny from stdin (blocking the run while it waits — the upstream
    /// prompter requires a terminal stdin, so headless hosts see every such
    /// query denied without reading).
    ///
    /// With `prompt: true` and an empty `permissions` list, every access is
    /// asked interactively (the v0.2.0 empty-list error does not apply);
    /// with flags, flags grant and everything else is asked.
    pub prompt: bool,
    /// Arguments exposed to the script via `process.argv` (after `argv[0]`).
    pub args: Vec<String>,
    /// Working directory that relative paths (entry, permissions, node_modules
    /// discovery) resolve against. Defaults to the process current directory.
    ///
    /// This is a **resolution base only** — the process cwd is never switched
    /// (chdir is process-global and would serialize or corrupt concurrent
    /// runs). The script observes the host's cwd: `Deno.cwd()` and relative
    /// filesystem operations inside the script resolve against it, so script
    /// authors should use absolute paths (or [`crate::run_in_subprocess`],
    /// where the child's cwd is pinned at spawn).
    pub cwd: Option<PathBuf>,
    /// In-process, best-effort constraint on the V8 old-generation heap in
    /// bytes (the constraint behind `--v8-flags=--max-old-space-size`). It does
    /// not cap native allocations, V8 external memory, host allocations, or
    /// memory used by child processes; it is not an OS/process memory boundary.
    pub max_heap_bytes: Option<usize>,
    /// In-process, best-effort execution deadline. It can terminate JavaScript
    /// when V8 reaches an interruptible stack check and then report
    /// [`LibdenoError::Timeout`], but it cannot interrupt a blocking syscall,
    /// native code, a child-process wait, or a blocked permission broker/hook;
    /// such a run may exceed the requested deadline.
    pub execution_deadline: Option<std::time::Duration>,
    /// Redirect the script's stdout (fd 1, e.g. `console.log`) into
    /// [`RunOutput::stdout`] instead of the host's terminal. Off by default
    /// (output passes through). While active the redirection is
    /// process-global: other host threads printing to stdout during the run
    /// are captured too, and the run is **exclusive** — any concurrent run
    /// (captured or not) is rejected with [`LibdenoError::Configuration`]
    /// (see `RunLease`). For captured runs alongside parallel execution use
    /// [`run_in_subprocess`], where each process has its own fds.
    pub capture_stdout: bool,
    /// Redirect the script's stderr (fd 2, e.g. `console.error`) into
    /// [`RunOutput::stderr`]; same semantics and caveats as
    /// [`Self::capture_stdout`].
    pub capture_stderr: bool,
    /// Cap on captured output per stream (stdout and stderr each get this
    /// budget). When a stream exceeds it, capture stops, the excess is
    /// dropped, and [`RunOutput::capture_truncated`] is set — a verbose or
    /// hostile script can no longer grow host memory without limit. `None`
    /// (default) captures without a bound for legacy/in-process capture. The
    /// execution-control supervisor path uses a bounded 64 KiB per-stream
    /// default, honors explicit values through 96 KiB, and rejects larger
    /// explicit values with [`LibdenoError::Configuration`] before spawning.
    pub max_capture_bytes: Option<usize>,
    /// Runtime feature flags exposed to the script, overriding the default
    /// set (`kv`, `cron`, `ffi`, `webgpu`, `worker-options`). Feature names
    /// must be valid deno unstable-feature names (see deno's
    /// `--unstable-*` flags); they gate which JS namespace IDs and feature
    /// checks are wired into the runtime. `None` (default) enables the
    /// default set. An embedder running untrusted plugins can shrink the
    /// surface (e.g. `Some(vec!["ffi".into()])`); the ops themselves stay
    /// permission-gated regardless. `worker-options` is always enabled even
    /// when omitted from a custom set — without it `new Worker(...)` with
    /// worker options terminates the host process, which a plugin must never
    /// be able to trigger.
    pub features: Option<Vec<String>>,
}

/// The result of a [`run_with_output`] invocation.
#[derive(Debug, Clone, Default)]
pub struct RunOutput {
    /// The exit code the script requested (0 on normal completion).
    pub exit_code: i32,
    /// Captured stdout bytes; empty unless [`LibdenoOptions::capture_stdout`]
    /// was set. Legacy/in-process capture is unbounded unless
    /// [`LibdenoOptions::max_capture_bytes`] is set; supervisor capture is
    /// bounded by its supervisor-specific default and maximum.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes; empty unless [`LibdenoOptions::capture_stderr`]
    /// was set. Same legacy/in-process unbounded-growth caveat as
    /// [`Self::stdout`]; supervisor capture is bounded by its
    /// supervisor-specific default and maximum.
    pub stderr: Vec<u8>,
    /// True when a captured stream hit [`LibdenoOptions::max_capture_bytes`]
    /// and was truncated. False when no capture was requested or when both
    /// streams fit their budgets.
    pub capture_truncated: bool,
}

/// Errors from a [`run`] invocation.
#[derive(Debug, thiserror::Error)]
pub enum LibdenoError {
    /// The entry path could not be resolved to a module.
    #[error("failed to resolve entry module: {0}")]
    Entry(AnyError),
    /// Permission capability strings could not be parsed.
    #[error("invalid permission flags: {0}")]
    Permission(String),
    /// Host/configuration-level problems: options that cannot be turned into a
    /// valid runtime configuration as given (e.g. an empty permission list
    /// without `allow_all_permissions`, which since v0.2.0 grants nothing).
    /// Distinguished from [`Self::Permission`] so embedders can tell "fix your
    /// option values" apart from "the dependency environment is broken".
    #[error("{0}")]
    Configuration(String),
    /// The runtime failed to start or the script failed.
    #[error("{0}")]
    Runtime(#[from] AnyError),
    /// A JS exception escaped the event loop (module execution / event loop).
    #[error("{0}")]
    Core(#[from] deno_core::error::CoreError),
    /// I/O failure in the host (cwd resolution).
    #[error("{0}")]
    Io(#[from] std::io::Error),
    /// An in-process best-effort deadline was reported, or a
    /// [`run_in_subprocess`] handshake timed out (the host never serviced child
    /// mode). The payload is the human-readable reason; an in-process run can
    /// exceed its requested deadline when it is inside non-interruptible work.
    #[error("{0}")]
    Timeout(String),
}

// The lifecycle event dispatches (`dispatch_load_event` and friends) return
// `Result<_, Box<JsError>>`; funnel those through the Runtime variant via
// anyhow instead of keeping a dedicated (never-constructed) enum variant.
impl From<Box<deno_core::error::JsError>> for LibdenoError {
    fn from(e: Box<deno_core::error::JsError>) -> Self {
        LibdenoError::Runtime(deno_core::anyhow::Error::new(e))
    }
}

impl LibdenoError {
    /// Returns whether this error carries Deno's typed `NotCapable` permission
    /// error class. This deliberately ignores rendered error text so a user
    /// exception mentioning permission wording remains a runtime error.
    pub fn is_permission_error(&self) -> bool {
        match self {
            Self::Runtime(error) => error.chain().any(error_chain_has_not_capable),
            Self::Core(error) => error_chain_has_not_capable(error),
            _ => false,
        }
    }
}

fn error_chain_has_not_capable(error: &(dyn std::error::Error + 'static)) -> bool {
    if let Some(error) = error.downcast_ref::<deno_core::error::JsError>() {
        return js_error_has_not_capable(error);
    }
    if let Some(error) = error.downcast_ref::<Box<deno_core::error::JsError>>() {
        return js_error_has_not_capable(error);
    }
    if let Some(error) = error.downcast_ref::<deno_error::JsErrorBox>() {
        return js_error_class_is_not_capable(error);
    }
    if let Some(error) = error.downcast_ref::<Box<deno_error::JsErrorBox>>() {
        return js_error_class_is_not_capable(error);
    }
    if let Some(error) = error.downcast_ref::<deno_core::error::CoreError>() {
        if js_error_class_is_not_capable(error) {
            return true;
        }
    }
    error.source().is_some_and(error_chain_has_not_capable)
}

fn js_error_class_is_not_capable(error: &dyn deno_error::JsErrorClass) -> bool {
    error.get_class().as_ref() == "NotCapable"
}

fn js_error_has_not_capable(error: &deno_core::error::JsError) -> bool {
    error.name.as_deref() == Some("NotCapable")
        || error.cause.as_deref().is_some_and(js_error_has_not_capable)
        || error
            .aggregated
            .as_ref()
            .is_some_and(|errors| errors.iter().any(js_error_has_not_capable))
}

fn check_entry_read_permission(
    permissions: &deno_runtime::deno_permissions::PermissionsContainer,
    main_module: &ModuleSpecifier,
) -> Result<(), LibdenoError> {
    // Keep entry denials typed before the graph loader turns load errors into
    // generic JsErrorBox values.
    let Some(path) = main_module.to_file_path().ok() else {
        return Ok(());
    };
    permissions
        .check_open(
            std::borrow::Cow::Owned(path),
            deno_runtime::deno_permissions::OpenAccessKind::Read,
            Some("main module"),
        )
        .map(|_| ())
        .map_err(|error| {
            LibdenoError::Core(deno_core::error::CoreError::from(
                deno_error::JsErrorBox::from_err(error),
            ))
        })
}

/// Runs `entry` (a file, a directory, or a package.json) to completion and
/// returns the exit code the script requested (0 on normal completion).
///
/// Each call builds its own current-thread runtime and worker. Ordinary runs
/// execute **in parallel** — they share nothing mutable (the process-global
/// analysis / npm-snapshot / on-disk caches are safe shared state). The one
/// exception: output capture is process-global fd redirection, so a captured
/// run is exclusive and any overlapping run is rejected with
/// [`LibdenoError::Configuration`] (see `RunLease`); use
/// [`run_in_subprocess`] for captured runs, where each process has its own
/// fds.
pub fn run(entry: impl AsRef<Path>, options: &LibdenoOptions) -> Result<i32, LibdenoError> {
    run_with_output(entry, options).map(|o| o.exit_code)
}

/// Runs `entry` to completion, returning the exit code together with the
/// script's stdout/stderr when [`LibdenoOptions::capture_stdout`] /
/// [`LibdenoOptions::capture_stderr`] are set.
///
/// Semantics match [`run`], including the tokio re-entry handling: building a
/// tokio runtime inside a tokio runtime panics, so when called from inside a
/// tokio context the run executes on a fresh thread (exactly the
/// `std::thread::spawn + join` escape async hosts previously had to build
/// themselves). A captured run holds the exclusivity lease across that
/// thread's lifetime (see `RunLease`).
pub fn run_with_output(
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError> {
    run_with_output_observed(entry, options, ExecutionTiming::disabled())
}

/// Internal equivalent of [`run_with_output`] that attaches observations to an
/// existing executor-owned sink.  The sink is crate-private so the legacy API
/// remains unchanged.
pub(crate) fn run_with_output_observed(
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
) -> Result<RunOutput, LibdenoError> {
    // Capture is process-global (fd-level redirection): take the exclusivity
    // lease before the run starts so a concurrent run can be rejected cleanly
    // (see RunLease). Ordinary runs take the lease too — they must not start
    // while a captured run is active — but otherwise run fully in parallel.
    let _lease = {
        let _admission = timing.span(Phase::Admission);
        RunLease::acquire(options.capture_stdout || options.capture_stderr)?
    };
    // Capture the entry-time child-IPC marker (and write it back so fork children inherit it).
    limits::capture_spawned_ipc_marker();
    let entry = entry.as_ref().to_path_buf();
    let options = options.clone();
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || run_sync_output(&entry, &options, timing))
            .join()
            .map_err(|_| {
                LibdenoError::Runtime(deno_core::anyhow::anyhow!("libdeno worker thread panicked"))
            })?
    } else {
        run_sync_output(&entry, &options, timing)
    }
}

#[cfg(feature = "execution-control")]
pub(crate) fn run_with_output_observed_cancellable(
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
    cancellation: Option<limits::CancellationContext>,
) -> Result<RunOutput, LibdenoError> {
    let _lease = {
        let _admission = timing.span(Phase::Admission);
        RunLease::acquire(options.capture_stdout || options.capture_stderr)?
    };
    limits::capture_spawned_ipc_marker();
    let entry = entry.as_ref().to_path_buf();
    let options = options.clone();
    if tokio::runtime::Handle::try_current().is_ok() {
        let cancellation = cancellation.clone();
        std::thread::spawn(move || {
            run_sync_output_cancellable(&entry, &options, timing, cancellation)
        })
        .join()
        .map_err(|_| {
            LibdenoError::Runtime(deno_core::anyhow::anyhow!("libdeno worker thread panicked"))
        })?
    } else {
        run_sync_output_cancellable(&entry, &options, timing, cancellation)
    }
}

#[cfg(feature = "execution-control")]
pub(crate) fn run_with_output_observed_cancellable_until(
    runtime: &LibdenoRuntime,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
    cancellation: Option<limits::CancellationContext>,
    absolute_deadline: Option<std::time::Instant>,
) -> Result<RunOutput, LibdenoError> {
    let deadline_guard =
        limits::AbsoluteDeadlineGuard::new(absolute_deadline, cancellation.clone());
    let result = crate::runtime::run_with_output_observed_cancellable(
        runtime,
        entry,
        options,
        timing,
        cancellation,
    );
    drop(deadline_guard);
    result
}

#[cfg(feature = "execution-control")]
fn run_sync_output_cancellable(
    entry: &Path,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
    cancellation: Option<limits::CancellationContext>,
) -> Result<RunOutput, LibdenoError> {
    #[cfg(windows)]
    if options.capture_stdout || options.capture_stderr {
        return Err(LibdenoError::Configuration(
            CAPTURE_UNSUPPORTED_ON_WINDOWS.to_string(),
        ));
    }
    let capture = output::OutputCapture::new(
        options.capture_stdout,
        options.capture_stderr,
        options.max_capture_bytes,
    )
    .map_err(LibdenoError::Io)?;
    let result = run_sync_cancellable(entry, options, timing.clone(), cancellation);
    let capture_result = capture.finish().map_err(LibdenoError::Io);
    match result {
        Err(error) => Err(error),
        Ok(exit_code) => {
            let (stdout, stderr, capture_truncated) = capture_result?;
            Ok(RunOutput {
                exit_code,
                stdout,
                stderr,
                capture_truncated,
            })
        }
    }
}

#[cfg(feature = "execution-control")]
fn run_sync_cancellable(
    entry: &Path,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
    cancellation: Option<limits::CancellationContext>,
) -> Result<i32, LibdenoError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| LibdenoError::Runtime(deno_core::anyhow::anyhow!(error)))?;
    runtime.block_on(async {
        let cwd_raw = options.cwd.clone().unwrap_or(std::env::current_dir()?);
        let cwd = std::fs::canonicalize(&cwd_raw).unwrap_or(cwd_raw);
        let main_module = resolve_entry(entry, &cwd).map_err(LibdenoError::Entry)?;
        let config_start_paths = main_module
            .to_file_path()
            .ok()
            .and_then(|path| path.parent().map(|dir| vec![dir.to_path_buf()]))
            .unwrap_or_else(|| vec![cwd.clone()]);
        let shared =
            SharedServices::new_with_timing(cwd.clone(), config_start_paths, Some(timing.clone()))
                .await
                .map_err(LibdenoError::Runtime)?;
        run_inner_with_cancellation(shared, cwd, entry, options, timing, cancellation).await
    })
}

/// Shared message for the Windows capture rejection (sync and async entry
/// points in lib.rs and runtime.rs all hit it). Only compiled on Windows —
/// the only platform that references it.
#[cfg(windows)]
pub(crate) const CAPTURE_UNSUPPORTED_ON_WINDOWS: &str =
    "output capture is not supported on Windows (std stdout/stderr \
     bypass the redirected CRT fd); use run_in_subprocess_with_output \
     instead — the child's own fds are piped, so it works on Windows";

fn run_sync_output(
    entry: &Path,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
) -> Result<RunOutput, LibdenoError> {
    // Windows: Rust std's stdout/stderr write via GetStdHandle, bypassing the
    // CRT fd that dup2 redirects, so console output cannot be captured
    // in-process there. Report a clean error instead of silently returning
    // empty buffers.
    #[cfg(windows)]
    if options.capture_stdout || options.capture_stderr {
        return Err(LibdenoError::Configuration(
            CAPTURE_UNSUPPORTED_ON_WINDOWS.to_string(),
        ));
    }
    let capture = output::OutputCapture::new(
        options.capture_stdout,
        options.capture_stderr,
        options.max_capture_bytes,
    )
    .map_err(LibdenoError::Io)?;
    let result = run_sync(entry, options, timing.clone());
    let capture_result = if options.capture_stdout || options.capture_stderr {
        let _output_drain = timing.span(Phase::OutputDrain);
        capture.finish().map_err(LibdenoError::Io)
    } else {
        capture.finish().map_err(LibdenoError::Io)
    };
    match result {
        Err(error) => Err(error),
        Ok(exit_code) => {
            let (stdout, stderr, capture_truncated) = capture_result?;
            Ok(RunOutput {
                exit_code,
                stdout,
                stderr,
                capture_truncated,
            })
        }
    }
}

/// Runs `entry` on the **caller's** tokio runtime. Identical semantics to
/// [`run`], but nothing spawns a thread: the run's async chain (resolver
/// stack build, graph build, event loop) executes on the calling runtime.
/// For a server host that already has a runtime this removes the per-run
/// OS-thread cost of [`run`]'s tokio re-entry escape.
///
/// Requirements:
/// - Must be called from inside a tokio runtime context (`tokio::spawn`,
///   `block_on`, another `async fn`, ...). Outside one, use [`run`].
/// - The returned future is **not `Send`** and — unlike ordinary [`run`]
///   calls — `run_async` futures must not be **interleaved** with each other:
///   a V8 isolate is pinned to the thread that created it (v8 0.150's
///   `PinnedRef`), and deno's worker stack is `Rc`-based, so two `run_async`
///   polled alternately on one thread abort the process (`HandleScope`
///   fatal). This is enforced by a thread-local RAII guard that rejects a
///   second `run_async` on the same thread with
///   [`LibdenoError::Configuration`] (the guard clears on drop, so a
///   cancelled future releases the slot). Await them strictly one at a time;
///   for parallel runs use [`run`] (each run gets its own thread + isolate)
///   or [`run_in_subprocess`]. A single `run_async` may be awaited on a
///   current-thread runtime directly, or on a multi-thread runtime inside
///   `tokio::task::LocalSet::block_on` / `spawn_local`.
/// - Capture, exclusivity, deadlines and every other option behave exactly
///   as in [`run_with_output`] / [`run`]. An `execution_deadline` runs on
///   tokio's time driver, so the caller's runtime must enable it
///   (`enable_time()` / `enable_all()`); a bare
///   `Builder::new_current_thread().build()` has no time driver.
///
/// For a long-lived project resolver stack, use
/// [`LibdenoRuntime::run_async`] instead; it keeps the same caller-runtime and
/// `!Send` constraints while reusing only the permission-free resolver state.
pub async fn run_async(
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
) -> Result<i32, LibdenoError> {
    run_with_output_async(entry, options)
        .await
        .map(|o| o.exit_code)
}

/// Async equivalent of [`run_with_output`]: same captured-output semantics,
/// executed on the caller's runtime with no spawned thread. See
/// [`run_async`] for the tokio-context and non-`Send` requirements, and the
/// `execution_deadline` time-driver requirement (the caller's runtime must
/// be built with `enable_time()` / `enable_all()`). For a prebuilt resolver
/// stack, [`LibdenoRuntime::run_with_output_async`] provides the reusable
/// equivalent without sharing permissions, graphs, or isolates.
pub async fn run_with_output_async(
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError> {
    check_async_context()?;
    let entry = entry.as_ref().to_path_buf();
    let timing = ExecutionTiming::disabled();
    run_with_output_async_guarded(options, timing.clone(), run_inner(&entry, options, timing)).await
}

/// Thread-local guard: one `run_async` at a time per thread (see
/// [`run_with_output_async`]). RAII so a dropped (cancelled) future clears
/// the flag on every exit path.
struct AsyncRunGuard;

thread_local! {
    static RUN_ASYNC_IN_PROGRESS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

impl AsyncRunGuard {
    fn acquire() -> Result<Self, LibdenoError> {
        if RUN_ASYNC_IN_PROGRESS.with(|in_progress| in_progress.replace(true)) {
            return Err(LibdenoError::Configuration(
                "a run_async is already in progress on this thread; run_async \
                 futures must not be interleaved — await them one at a time, or \
                 use run() for parallel runs"
                    .to_string(),
            ));
        }
        Ok(Self)
    }
}

impl Drop for AsyncRunGuard {
    fn drop(&mut self) {
        RUN_ASYNC_IN_PROGRESS.with(|in_progress| in_progress.set(false));
    }
}

/// Rejects async entry points outside a tokio runtime before any V8 work starts.
pub(crate) fn check_async_context() -> Result<(), LibdenoError> {
    // Outside a tokio context the run's async chain would panic deep inside
    // deno (runtime-context assert) — report it like every other invalid
    // usage instead.
    if tokio::runtime::Handle::try_current().is_err() {
        return Err(LibdenoError::Configuration(
            "run_async must be called from inside a tokio runtime context; \
             outside one, use run()"
                .to_string(),
        ));
    }
    Ok(())
}

/// Runs one async execution under the shared thread-local guard and output
/// capture lease. The future is deliberately not `Send`: the worker and V8
/// isolate remain on the caller's thread.
pub(crate) async fn run_with_output_async_guarded<F>(
    options: &LibdenoOptions,
    timing: ExecutionTiming,
    run: F,
) -> Result<RunOutput, LibdenoError>
where
    F: std::future::Future<Output = Result<i32, LibdenoError>>,
{
    // Interleaving two async runs on one thread aborts the process (v8 pins
    // the isolate to its creating thread). The process-global lease cannot see
    // this thread-local hazard, so the RAII guard turns it into a recoverable
    // Configuration error. Dropping the future releases the guard.
    let _guard = AsyncRunGuard::acquire()?;
    // Same lease protocol as the sync path: capture is process-global and
    // exclusive; the lease is an atomic RAII guard, so it is safe to hold
    // across awaits.
    let _lease = {
        let _admission = timing.span(Phase::Admission);
        RunLease::acquire(options.capture_stdout || options.capture_stderr)?
    };
    // Capture the entry-time child-IPC marker (fork children inherit it).
    limits::capture_spawned_ipc_marker();
    #[cfg(windows)]
    if options.capture_stdout || options.capture_stderr {
        return Err(LibdenoError::Configuration(
            CAPTURE_UNSUPPORTED_ON_WINDOWS.to_string(),
        ));
    }
    let capture = output::OutputCapture::new(
        options.capture_stdout,
        options.capture_stderr,
        options.max_capture_bytes,
    )
    .map_err(LibdenoError::Io)?;
    let result = run.await;
    let capture_result = if options.capture_stdout || options.capture_stderr {
        let _output_drain = timing.span(Phase::OutputDrain);
        capture.finish().map_err(LibdenoError::Io)
    } else {
        capture.finish().map_err(LibdenoError::Io)
    };
    match result {
        Err(error) => Err(error),
        Ok(exit_code) => {
            let (stdout, stderr, capture_truncated) = capture_result?;
            Ok(RunOutput {
                exit_code,
                stdout,
                stderr,
                capture_truncated,
            })
        }
    }
}

/// The actual run: a fresh current-thread runtime and the run lifecycle. Must
/// not be called from inside a tokio runtime (tokio panics on nested
/// runtimes); [`run_with_output`] routes such callers onto a fresh thread
/// first.
fn run_sync(
    entry: &Path,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
) -> Result<i32, LibdenoError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))?;
    runtime.block_on(run_inner(entry, options, timing))
}

async fn run_inner(
    entry: &Path,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
) -> Result<i32, LibdenoError> {
    // Canonicalize once so entry resolution, permission grants and
    // node_modules discovery all agree on the resolution base: unix getcwd
    // already canonicalizes (e.g. /var -> /private/var), so a symlinked
    // options.cwd would otherwise split grants/entry (aliased) from the
    // canonical path. This is a resolution-base only — the process cwd is
    // never switched (see the module docs: the script observes the host cwd).
    let cwd_raw = options.cwd.clone().unwrap_or(std::env::current_dir()?);
    let cwd = std::fs::canonicalize(&cwd_raw).unwrap_or(cwd_raw);
    let main_module = resolve_entry(entry, &cwd).map_err(LibdenoError::Entry)?;
    let config_start_paths = main_module
        .to_file_path()
        .ok()
        .and_then(|p| p.parent().map(|d| vec![d.to_path_buf()]))
        .unwrap_or_else(|| vec![cwd.clone()]);
    let shared = {
        let _rebuild = timing.span(Phase::ResolverRebuild);
        SharedServices::new_with_timing(cwd.clone(), config_start_paths, Some(timing.clone()))
            .await
            .map_err(LibdenoError::Runtime)?
    };
    run_inner_with(shared, cwd, entry, options, timing).await
}

/// The per-run half of a [`run`]/[`run_with`] against an already-built
/// resolver stack: the permission-bound services and the run lifecycle.
pub(crate) async fn run_inner_with(
    shared: Arc<SharedServices>,
    cwd: PathBuf,
    entry: &Path,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
) -> Result<i32, LibdenoError> {
    run_inner_with_cancellation(shared, cwd, entry, options, timing, None).await
}

pub(crate) async fn run_inner_with_cancellation(
    shared: Arc<SharedServices>,
    cwd: PathBuf,
    entry: &Path,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
    cancellation: Option<crate::limits::CancellationContext>,
) -> Result<i32, LibdenoError> {
    cancellation_checkpoint(cancellation.as_ref())?;
    // Validate before permission construction, graph setup, or MainWorker/V8
    // bootstrap. All ordinary, reusable, async, and subprocess-backed runs
    // converge here, so invalid resource limits cannot reach backend defaults.
    limits::validate_max_heap_bytes(options.max_heap_bytes)?;
    limits::validate_execution_deadline(options.execution_deadline)?;

    // rustls needs an explicit CryptoProvider (aws-lc-rs and ring are both
    // enabled in the dep graph); the deno CLI does the same. Only install when
    // the host has not already set one — install_default returns Err exactly
    // when a provider is already installed, so with this guard the result can
    // no longer fail silently.
    if deno_runtime::deno_tls::rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = deno_runtime::deno_tls::rustls::crypto::CryptoProvider::install_default(
            deno_runtime::deno_tls::rustls::crypto::aws_lc_rs::default_provider(),
        );
    }
    let main_module = resolve_entry(entry, &cwd).map_err(LibdenoError::Entry)?;
    cancellation_checkpoint(cancellation.as_ref())?;

    let fs: Arc<dyn FileSystem> = Arc::new(RealFs);
    let (permissions, services) = {
        let _permission_services = timing.span(Phase::PermissionRuntimeServices);
        let permission_parser = Arc::new(
            deno_runtime::deno_permissions::RuntimePermissionDescriptorParser::new(RealSys),
        );
        let permissions = build_permissions(
            &options.permissions,
            options.allow_all_permissions,
            options.prompt,
            permission_parser,
            &cwd,
        )?;
        check_entry_read_permission(&permissions, &main_module)?;
        let services = Arc::new(
            RuntimeServices::new(shared, permissions.clone(), timing.clone())
                .map_err(LibdenoError::Runtime)?,
        );
        (permissions, services)
    };
    cancellation_checkpoint(cancellation.as_ref())?;

    // has_node_modules_dir must come AFTER RuntimeServices::new: that runs
    // initialize_npm_resolution_if_managed, which decides Managed vs BYONM.
    let has_node_modules_dir = {
        use deno_resolver::npm::NpmResolver;
        match services
            .shared
            .resolver_factory
            .npm_resolver()
            .map_err(LibdenoError::Runtime)?
        {
            NpmResolver::Managed(managed) => managed.root_node_modules_path().is_some(),
            NpmResolver::Byonm(byonm) => byonm.root_node_modules_path().is_some(),
        }
    };
    cancellation_checkpoint(cancellation.as_ref())?;

    let module_loader: Rc<dyn deno_core::ModuleLoader> = Rc::new(GraphModuleLoader::new(
        services.clone(),
        permissions.clone(),
    ));

    let (mut worker, isolate_handle) = {
        let _bootstrap = timing.span(Phase::MainWorkerBootstrap);
        deno_runtime_adapter::build_main_worker(deno_runtime_adapter::MainWorkerInput {
            main_module: &main_module,
            options,
            has_node_modules_dir,
            fs,
            module_loader,
            permissions,
            services: services.clone(),
        })?
    };
    cancellation_checkpoint(cancellation.as_ref())?;

    // Intercept Deno.exit: a WatcherExitHandle in the OpState makes op_exit
    // terminate the isolate (the CLI's --watch path); ExitCode op state carries n.
    worker
        .js_runtime
        .op_state()
        .borrow_mut()
        .put(deno_runtime::deno_os::WatcherExitHandle(
            isolate_handle.clone(),
        ));

    let run_result = match limits::run_worker_cancellable(
        &mut worker,
        &main_module,
        options.execution_deadline,
        isolate_handle.clone(),
        timing.clone(),
        cancellation,
    )
    .await
    {
        Ok(result) => result,
        // Deadline fired: isolate terminated; the run lease releases on return.
        Err(limits::ExecutionTermination::Deadline(deadline)) => {
            return Err(LibdenoError::Timeout(format!(
                "execution deadline of {deadline:?} exceeded; isolate terminated"
            )))
        }
        Err(limits::ExecutionTermination::Cancelled) => {
            return Err(LibdenoError::Timeout(
                "execution cancellation requested; isolate terminated".to_string(),
            ))
        }
    };

    let exit_code = match run_result {
        Ok(()) => worker.exit_code(),
        Err(_) if crate::runtime::has_watcher_exited(&worker) => worker.exit_code(),
        Err(e) => return Err(e),
    };
    // Cache the resolved npm snapshot (managed, no lockfile) only now: the
    // graph build has populated the resolution, so the snapshot is real.
    services.save_npm_snapshot_cache();
    Ok(exit_code)
}

fn cancellation_checkpoint(
    cancellation: Option<&crate::limits::CancellationContext>,
) -> Result<(), LibdenoError> {
    if cancellation.is_some_and(crate::limits::CancellationContext::is_requested) {
        return Err(LibdenoError::Timeout(
            "execution cancellation requested; isolate terminated".to_string(),
        ));
    }
    Ok(())
}

/// Resolve the entry module: a file path, or a directory / package.json whose
/// `main` (default `index.js`) is used.
fn resolve_entry(path: &Path, cwd: &Path) -> Result<ModuleSpecifier, AnyError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let file_path = if path.is_dir() {
        let pkg = path.join("package.json");
        if pkg.exists() {
            package_main(&pkg)?
        } else {
            path.join("index.js")
        }
    } else if path.file_name().and_then(|n| n.to_str()) == Some("package.json") {
        package_main(&path)?
    } else {
        path
    };
    ModuleSpecifier::from_file_path(&file_path)
        .map_err(|_| deno_core::anyhow::anyhow!("Invalid entry file path: {}", file_path.display()))
}

fn package_main(pkg_path: &Path) -> Result<PathBuf, AnyError> {
    let text = std::fs::read_to_string(pkg_path)?;
    let json: deno_core::serde_json::Value = deno_core::serde_json::from_str(&text)?;
    let main = json
        .get("main")
        .and_then(|v| v.as_str())
        .unwrap_or("index.js");
    let dir = pkg_path.parent().unwrap_or(Path::new("."));
    let main_path = dir.join(main);
    // Reject `main` values that escape the package directory. `Path::join`
    // replaces the base for absolute paths (strip_prefix fails, caught
    // directly); relative `..` components are normalized lexically — a `..`
    // that walks back into the package (`lib/../index.js`) is fine, one that
    // pops above `dir` (`../escape.js`) is not. (Symlinked components are not
    // resolved: the entry directory is host-selected, so this is a sanity
    // guard, not a boundary.)
    use std::path::Component;
    let escaped = match main_path.strip_prefix(dir) {
        Err(_) => true,
        Ok(rest) => {
            let mut depth: i64 = 0;
            let mut escaped = false;
            for c in rest.components() {
                match c {
                    Component::Normal(_) => depth += 1,
                    Component::CurDir => {}
                    Component::ParentDir => {
                        if depth == 0 {
                            escaped = true;
                            break;
                        }
                        depth -= 1;
                    }
                    Component::RootDir | Component::Prefix(_) => {
                        escaped = true;
                        break;
                    }
                }
            }
            escaped
        }
    };
    if escaped {
        return Err(deno_core::anyhow::anyhow!(
            "package.json `main` field {main:?} escapes the package directory"
        ));
    }
    Ok(main_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("libdeno-test-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_entry_file() {
        let dir = temp_dir("entry-file");
        let file = dir.join("app.js");
        std::fs::write(&file, "").unwrap();
        let spec = resolve_entry(&file, &dir).unwrap();
        assert_eq!(spec.to_file_path().unwrap(), file);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_entry_directory_uses_package_main() {
        let dir = temp_dir("entry-dir-pkg");
        std::fs::write(
            dir.join("package.json"),
            r#"{"name":"app","main":"src/main.js"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.js"), "").unwrap();
        let spec = resolve_entry(&dir, &dir).unwrap();
        assert_eq!(spec.to_file_path().unwrap(), dir.join("src/main.js"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_entry_directory_defaults_to_index_js() {
        let dir = temp_dir("entry-dir-default");
        std::fs::write(dir.join("index.js"), "").unwrap();
        let spec = resolve_entry(&dir, &dir).unwrap();
        assert_eq!(spec.to_file_path().unwrap(), dir.join("index.js"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_entry_package_json_directly() {
        let dir = temp_dir("entry-pkgjson");
        std::fs::write(dir.join("package.json"), r#"{"main":"lib/index.js"}"#).unwrap();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/index.js"), "").unwrap();
        let spec = resolve_entry(&dir.join("package.json"), &dir).unwrap();
        assert_eq!(spec.to_file_path().unwrap(), dir.join("lib/index.js"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_entry_relative_path_joins_cwd() {
        let dir = temp_dir("entry-relative");
        std::fs::write(dir.join("app.ts"), "").unwrap();
        let spec = resolve_entry(Path::new("app.ts"), &dir).unwrap();
        assert_eq!(spec.to_file_path().unwrap(), dir.join("app.ts"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_main_escaping_the_package_dir_is_rejected() {
        let dir = temp_dir("pkg-escape");
        std::fs::write(dir.join("package.json"), r#"{"main":"../escape.js"}"#).unwrap();
        let err = resolve_entry(&dir, &dir).unwrap_err();
        assert!(
            err.to_string().contains("escapes"),
            "expected an escape rejection, got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_main_internal_dotdot_is_allowed() {
        // A `..` that normalizes back inside the package (`src/../lib/index.js`)
        // is not an escape: the guard rejects only paths that pop above the
        // package directory, not every ParentDir component.
        let dir = temp_dir("pkg-internal-dotdot");
        std::fs::write(
            dir.join("package.json"),
            r#"{"main":"src/../lib/index.js"}"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/index.js"), "").unwrap();
        let spec = resolve_entry(&dir, &dir).unwrap();
        assert_eq!(
            spec.to_file_path().unwrap(),
            dir.join("src/../lib/index.js")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_main_absolute_main_is_rejected() {
        // `Path::join` replaces the base for absolute paths, so an absolute
        // `main` must be rejected as an escape (strip_prefix fails).
        let dir = temp_dir("pkg-abs-main");
        std::fs::write(dir.join("package.json"), r#"{"main":"/etc/passwd"}"#).unwrap();
        let err = resolve_entry(&dir, &dir).unwrap_err();
        assert!(
            err.to_string().contains("escapes"),
            "expected an escape rejection, got: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
