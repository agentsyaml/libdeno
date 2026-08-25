// Process-level reuse of the resolver stack: `LibdenoRuntime` builds the
// permission-free half of the module pipeline (workspace/resolver/npm
// installer factories, graph resolver, npm process state) once, and
// `run_with` reuses it across script runs instead of rebuilding it every
// time. The stack is rebuilt automatically when the accepted resolver input
// manifest changes. Permission-bound pieces (the file fetcher,
// the graph loader and the module graph) stay strictly per-run in
// `RuntimeServices` — see services.rs. The async methods use the caller's
// tokio runtime while reusing the same shared resolver stack.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::npm_cache::ResolverInputManifest;
use crate::services::SharedServices;
use crate::timing::{ExecutionTiming, Phase};
use crate::LibdenoError;
use crate::LibdenoOptions;
use crate::RunLease;

/// A reusable resolver stack scoped to a project directory.
///
/// [`LibdenoRuntime::new`] builds the permission-free half of the module
/// pipeline once; [`run_with`], [`Self::run_async`], and
/// [`Self::run_with_output_async`] then reuse it across runs. The stack is
/// rebuilt automatically when the accepted resolver input manifest changes.
/// The manifest comes from the actual discovered workspace, parsed config /
/// package semantics, effective auth-free npm routing, `JSR_URL`, lockfile, and
/// BYONM
/// node_modules inputs. Managed installation output is deliberately excluded
/// from the BYONM probe. deno_resolver 0.88 reads `$HOME/.npmrc` and does not
/// honor `NPM_CONFIG_USERCONFIG`.
///
/// The runtime is single-threaded by design: the module loader stack is
/// `Rc<dyn ModuleLoader>`-based. Synchronous `run_with` executes on a fresh
/// current-thread tokio runtime, while the reusable async methods use the
/// caller's runtime. `LibdenoRuntime` itself is `Clone` + `Send` + `Sync`; its
/// state is an `Arc<Mutex<RuntimeState>>` around the resolver stack plus a
/// per-runtime rebuild gate, so it can be shared across host threads and used
/// concurrently — ordinary runs are fully parallel; only a captured run is
/// exclusive (see `RunLease`). The async methods return `!Send` futures and
/// must be awaited without interleaving on their V8-pinned thread; use
/// `LocalSet` when the caller owns a multi-thread tokio runtime.
#[derive(Clone)]
pub struct LibdenoRuntime {
    cwd: PathBuf,
    /// The current resolver stack and the accepted manifest it was built for;
    /// `run_with` swaps `shared` under this short-lived guard when a candidate
    /// passes its publish checks. No guard is held across an await.
    state: Arc<std::sync::Mutex<RuntimeState>>,
    /// Singleflight gate for this runtime only. `tokio::sync::Mutex` is a
    /// synchronization primitive rather than a handle/task-local resource,
    /// so its lock future may be awaited by callers on different current-
    /// thread Tokio runtimes. Different `LibdenoRuntime` values have different
    /// gates and still build in parallel.
    rebuild_lock: Arc<tokio::sync::Mutex<()>>,
}

struct RuntimeState {
    input_manifest: ResolverInputManifest,
    shared: Arc<SharedServices>,
    /// Monotonic invalidation generation requested by [`LibdenoRuntime::refresh`].
    refresh_generation: u64,
    /// Generation covered by the currently installed resolver stack.
    built_generation: u64,
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
        let input_manifest = shared.input_manifest.clone();
        Ok(Self {
            cwd,
            state: Arc::new(std::sync::Mutex::new(RuntimeState {
                input_manifest,
                shared,
                refresh_generation: 0,
                built_generation: 0,
            })),
            rebuild_lock: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Marks the resolver stack stale. The next [`run_with`],
    /// [`Self::run_async`], or [`Self::run_with_output_async`] call rebuilds
    /// the permission-free stack before executing. Use this after filesystem
    /// changes below the discovered resolver inputs, such as edits inside
    /// nested `node_modules`; npm routing and workspace changes are detected by
    /// the accepted manifest. This is an explicit bounded
    /// invalidation mechanism, not a recursive watcher or tree hash.
    pub fn refresh(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.refresh_generation = state.refresh_generation.wrapping_add(1);
    }

    /// Runs `entry` on the caller's tokio runtime using this runtime's shared
    /// resolver stack and returns its exit code.
    ///
    /// Like [`crate::run_async`], this method does not spawn a worker thread.
    /// The returned future is `!Send`; await reusable async runs strictly one
    /// at a time on a thread. Each call still creates fresh permission-bound
    /// services, a module graph, and a V8 isolate. Output-capture flags are
    /// honored and discarded, matching [`crate::run_async`]; use
    /// [`Self::run_with_output_async`] to receive captured bytes.
    pub async fn run_async(
        &self,
        entry: impl AsRef<Path>,
        options: &LibdenoOptions,
    ) -> Result<i32, LibdenoError> {
        self.run_with_output_async(entry, options)
            .await
            .map(|output| output.exit_code)
    }

    /// Runs `entry` on the caller's tokio runtime using this runtime's shared
    /// resolver stack and returns captured stdout/stderr when requested.
    ///
    /// The returned future is `!Send` and must not be interleaved with another
    /// reusable async run on the same thread; a cancelled future releases the
    /// same thread-local guard used by [`crate::run_async`]. Capture uses the
    /// process-global `RunLease` and is exclusive. Permissions, the graph,
    /// and the V8 isolate remain per-run and are never shared.
    pub async fn run_with_output_async(
        &self,
        entry: impl AsRef<Path>,
        options: &LibdenoOptions,
    ) -> Result<crate::RunOutput, LibdenoError> {
        run_with_output_async_observed(self, entry, options, ExecutionTiming::disabled()).await
    }
}

pub(crate) async fn run_with_output_async_observed(
    runtime: &LibdenoRuntime,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
) -> Result<crate::RunOutput, LibdenoError> {
    crate::check_async_context()?;
    reject_unusable_cwd(runtime, options)?;
    let entry = entry.as_ref().to_path_buf();
    crate::run_with_output_async_guarded(options, timing.clone(), async move {
        let shared = shared_for_run(runtime, timing.clone()).await?;
        crate::run_inner_with(shared, runtime.cwd.clone(), &entry, options, timing).await
    })
    .await
}

/// Runs `entry` through a prebuilt [`LibdenoRuntime`]'s resolver stack.
///
/// Semantics match [`crate::run`]: the run observes the host cwd (never
/// switched), tokio re-entry is handled automatically (the run executes on a
/// fresh thread when called from inside a tokio runtime), `Deno.exit(n)` /
/// exit codes / deadlines behave identically, and the run's permissions come
/// from `options` — each run rebuilds its permission-bound file fetcher /
/// graph loader / graph, so one run's grants can never leak into another.
///
/// The script runs in the host's cwd; the process cwd is never switched —
/// same semantics as [`crate::run`], where `cwd` is a resolution base only.
/// Because the resolver stack is scoped to the runtime's directory,
/// `LibdenoOptions.cwd` is honored only when it matches the runtime's
/// directory (canonicalize-aware comparison); a mismatched `cwd` is
/// **rejected** with `LibdenoError::Configuration` instead of silently
/// resolving against a different directory. Omit `cwd`, or build the runtime
/// for that directory, to run through the reusable stack.
///
/// `LibdenoOptions.capture_stdout` / `capture_stderr` are **rejected** by
/// `run_with` with `LibdenoError::Configuration` (it returns only the exit
/// code); use [`run_with_output`] for capture on the reusable stack.
///
/// The async/sync split is deliberate: [`LibdenoRuntime::new`] is async
/// because resolver stack construction needs a tokio context, while `run_with`
/// is sync and does its own `block_on` on a fresh current-thread runtime.
pub fn run_with(
    runtime: &LibdenoRuntime,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
) -> Result<i32, LibdenoError> {
    // run_with never captures (it returns only the exit code) — reject the
    // flags instead of silently ignoring them (matching the Windows-capture
    // rejection pattern; a silent no-op here would also reject concurrent
    // runs for no benefit under the capture-exclusivity protocol).
    if options.capture_stdout || options.capture_stderr {
        return Err(LibdenoError::Configuration(
            "run_with does not support output capture (it returns only the \
             exit code); use run_with_output for capture on the reusable \
             stack"
                .to_string(),
        ));
    }
    // options.cwd is a resolution base that the reusable stack ignores (it
    // is scoped to the runtime's directory) — reject a mismatched base
    // instead of silently resolving against a different directory.
    reject_unusable_cwd(runtime, options)?;
    // Take the capture-exclusivity lease before the run starts (see
    // RunLease); ordinary runs are otherwise fully parallel. run_with never
    // captures (rejected above), so the lease is taken as a plain parallel
    // run.
    let _lease = RunLease::acquire(false)?;
    // Capture the entry-time child-IPC marker (fork children inherit it).
    crate::limits::capture_spawned_ipc_marker();
    let entry = entry.as_ref().to_path_buf();
    let options = options.clone();
    let runtime = runtime.clone();
    // Same tokio re-entry handling as run(): building a runtime inside a
    // tokio runtime panics, so run on a fresh thread instead of rejecting
    // async hosts.
    let timing = ExecutionTiming::disabled();
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || run_with_sync(&runtime, &entry, &options, timing))
            .join()
            .map_err(|_| {
                LibdenoError::Runtime(deno_core::anyhow::anyhow!("libdeno worker thread panicked"))
            })?
    } else {
        run_with_sync(&runtime, &entry, &options, timing)
    }
}

/// Rejects options the reusable stack cannot honor: `LibdenoOptions.cwd` is
/// a resolution base the stack ignores (it is scoped to the runtime's
/// directory), so a cwd that resolves differently would silently run the
/// script against a different base. Comparison is canonicalize-aware to
/// avoid path-form false positives; an unresolvable `options.cwd` is
/// tolerated (the base is unused on this path).
fn reject_unusable_cwd(
    runtime: &LibdenoRuntime,
    options: &LibdenoOptions,
) -> Result<(), LibdenoError> {
    if let Some(cwd) = &options.cwd {
        let resolved = |p: &std::path::Path| std::fs::canonicalize(p).ok();
        if resolved(cwd) != resolved(&runtime.cwd) {
            return Err(LibdenoError::Configuration(format!(
                "run_with ignores LibdenoOptions.cwd (the resolver stack is \
                 scoped to the runtime's directory {}); build the runtime for \
                 that directory instead, or omit cwd",
                runtime.cwd.display()
            )));
        }
    }
    Ok(())
}

/// Runs `entry` through a prebuilt [`LibdenoRuntime`]'s resolver stack and
/// returns the exit code together with the captured stdout/stderr when
/// `LibdenoOptions.capture_stdout` / `capture_stderr` are set — the
/// long-lived-host equivalent of [`crate::run_with_output`] (which rebuilds
/// the resolver stack on every call).
///
/// Everything else matches [`run_with`]: the capture-exclusivity lease (see
/// `RunLease`), tokio re-entry handled automatically, mismatched
/// `LibdenoOptions.cwd` rejected (matching cwd accepted), permissions
/// per-run. Capture semantics
/// (fd-level redirection, byte cap, Windows rejection) are identical to
/// [`crate::run_with_output`].
pub fn run_with_output(
    runtime: &LibdenoRuntime,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
) -> Result<crate::RunOutput, LibdenoError> {
    run_with_output_observed(runtime, entry, options, ExecutionTiming::disabled())
}

pub(crate) fn run_with_output_observed(
    runtime: &LibdenoRuntime,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
) -> Result<crate::RunOutput, LibdenoError> {
    run_with_output_observed_cancellable(runtime, entry, options, timing, None)
}

pub(crate) fn run_with_output_observed_cancellable(
    runtime: &LibdenoRuntime,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
    cancellation: Option<crate::limits::CancellationContext>,
) -> Result<crate::RunOutput, LibdenoError> {
    reject_unusable_cwd(runtime, options)?;
    let _lease = {
        let _admission = timing.span(Phase::Admission);
        RunLease::acquire(options.capture_stdout || options.capture_stderr)?
    };
    crate::limits::capture_spawned_ipc_marker();
    let entry = entry.as_ref().to_path_buf();
    let options = options.clone();
    let runtime = runtime.clone();
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::spawn(move || {
            run_with_sync_output_cancellable(&runtime, &entry, &options, timing, cancellation)
        })
        .join()
        .map_err(|_| {
            LibdenoError::Runtime(deno_core::anyhow::anyhow!("libdeno worker thread panicked"))
        })?
    } else {
        run_with_sync_output_cancellable(&runtime, &entry, &options, timing, cancellation)
    }
}

/// Capture + [`run_with_sync`]; the Windows rejection and byte-cap handling
/// mirror `crate::run_sync_output`.
fn run_with_sync_output_cancellable(
    runtime: &LibdenoRuntime,
    entry: &Path,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
    cancellation: Option<crate::limits::CancellationContext>,
) -> Result<crate::RunOutput, LibdenoError> {
    #[cfg(windows)]
    if options.capture_stdout || options.capture_stderr {
        return Err(LibdenoError::Configuration(
            crate::CAPTURE_UNSUPPORTED_ON_WINDOWS.to_string(),
        ));
    }
    let capture = crate::output::OutputCapture::new(
        options.capture_stdout,
        options.capture_stderr,
        options.max_capture_bytes,
    )
    .map_err(LibdenoError::Io)?;
    let result = run_with_sync_cancellable(runtime, entry, options, timing.clone(), cancellation);
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
            Ok(crate::RunOutput {
                exit_code,
                stdout,
                stderr,
                capture_truncated,
            })
        }
    }
}

/// The actual run against a prebuilt resolver stack: its own block_on on a
/// fresh current-thread runtime. Must not be called from inside a tokio
/// runtime; [`run_with`] routes such callers onto a fresh thread first.
fn run_with_sync(
    runtime: &LibdenoRuntime,
    entry: &Path,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
) -> Result<i32, LibdenoError> {
    run_with_sync_cancellable(runtime, entry, options, timing, None)
}

fn run_with_sync_cancellable(
    runtime: &LibdenoRuntime,
    entry: &Path,
    options: &LibdenoOptions,
    timing: ExecutionTiming,
    cancellation: Option<crate::limits::CancellationContext>,
) -> Result<i32, LibdenoError> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| LibdenoError::Runtime(deno_core::anyhow::anyhow!(e)))?;
    rt.block_on(async {
        let shared = shared_for_run(runtime, timing.clone()).await?;
        crate::run_inner_with_cancellation(
            shared,
            runtime.cwd.clone(),
            entry,
            options,
            timing,
            cancellation,
        )
        .await
    })
}

/// Returns the current shared resolver stack, rebuilding it when the accepted
/// resolver input manifest changes. The helper is shared by sync and async
/// reusable entry points; all permission-bound run state remains per-call.
async fn shared_for_run(
    runtime: &LibdenoRuntime,
    timing: ExecutionTiming,
) -> Result<Arc<SharedServices>, LibdenoError> {
    let initial_shared = {
        let (manifest, refresh_generation, built_generation, shared) = {
            let state = runtime.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                state.input_manifest.clone(),
                state.refresh_generation,
                state.built_generation,
                state.shared.clone(),
            )
        };
        let probe_changed = {
            let _probe = timing.span(Phase::ResolverManifestProbe);
            !manifest.is_reusable().unwrap_or(false)
        };
        if singleflight_build_generation(probe_changed, refresh_generation, built_generation)
            .is_none()
        {
            let _reuse = timing.span(Phase::ResolverReuse);
            Some(shared)
        } else {
            None
        }
    };
    if let Some(shared) = initial_shared {
        return Ok(shared);
    }

    // The first probe only avoids taking the gate on the common path. A
    // caller that waited for another builder must probe and check state again
    // after acquiring the gate; otherwise it could duplicate the build.
    let _rebuild_guard = runtime.rebuild_lock.lock().await;
    let Some(build_generation) = ({
        let (manifest, refresh_generation, built_generation) = {
            let state = runtime.state.lock().unwrap_or_else(|e| e.into_inner());
            (
                state.input_manifest.clone(),
                state.refresh_generation,
                state.built_generation,
            )
        };
        let probe_changed = {
            let _probe = timing.span(Phase::ResolverManifestProbe);
            !manifest.is_reusable().unwrap_or(false)
        };
        singleflight_build_generation(probe_changed, refresh_generation, built_generation)
    }) else {
        let _reuse = timing.span(Phase::ResolverReuse);
        return Ok(runtime
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shared
            .clone());
    };

    let cwd = runtime.cwd.clone();
    let _rebuild = timing.span(Phase::ResolverRebuild);
    let candidate = SharedServices::new_with_timing(cwd.clone(), vec![cwd], Some(timing.clone()))
        .await
        .map_err(LibdenoError::Runtime)?;

    let mut state = runtime.state.lock().unwrap_or_else(|e| e.into_inner());
    // SharedServices::new returns only a stable candidate. A refresh arriving
    // during that build still lets this run use it, but prevents publication;
    // the next entry observes the old generation and rebuilds.
    if can_publish(build_generation, state.refresh_generation) {
        state.input_manifest = candidate.input_manifest.clone();
        state.shared = candidate.clone();
        state.built_generation = build_generation;
    }
    Ok(candidate)
}

/// Returns the refresh generation a singleflight builder should capture, or
/// `None` when the post-gate double-check can reuse the published state.
fn singleflight_build_generation(
    manifest_changed: bool,
    refresh_generation: u64,
    built_generation: u64,
) -> Option<u64> {
    needs_rebuild(manifest_changed, refresh_generation, built_generation)
        .then_some(refresh_generation)
}

/// Pure publication gate for a candidate already accepted by the stable
/// builder.
fn can_publish(build_generation: u64, current_generation: u64) -> bool {
    build_generation == current_generation
}

fn needs_rebuild(manifest_changed: bool, refresh_generation: u64, built_generation: u64) -> bool {
    manifest_changed || built_generation != refresh_generation
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

#[cfg(test)]
mod tests {
    use super::can_publish;
    use super::needs_rebuild;
    use super::singleflight_build_generation;

    #[test]
    fn publish_rejects_refresh_during_build() {
        assert!(!can_publish(0, 1));
        assert!(needs_rebuild(false, 1, 0));
    }

    #[test]
    fn stable_builder_hands_generation_gate_a_publishable_candidate() {
        // Probe stability is now guaranteed by SharedServices::new before
        // this generation-only publication gate is reached.
        assert!(can_publish(0, 0));
    }

    #[test]
    fn second_singleflight_caller_reuses_published_candidate() {
        // Caller one wins the gate and builds generation zero.
        assert_eq!(singleflight_build_generation(true, 0, 0), Some(0));
        // After it publishes, caller two's post-gate double-check does not
        // start another build.
        assert_eq!(singleflight_build_generation(false, 0, 0), None);
    }

    #[test]
    fn refresh_generation_remains_effective_after_rejected_publish() {
        assert_eq!(singleflight_build_generation(false, 1, 0), Some(1));
        assert!(needs_rebuild(false, 1, 0));
        assert_eq!(singleflight_build_generation(false, 1, 1), None);
    }
}
