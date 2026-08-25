//! Crate-private execution observations.
//!
//! This is deliberately a small internal sensor, not a tracing or metrics
//! API.  Disabled timings are used by the legacy entry points, while the
//! executor can opt in and attach the snapshot to its existing report.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One phase that can be accumulated in an execution observation.
#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Admission,
    ResolverManifestProbe,
    ResolverReuse,
    ResolverRebuild,
    PermissionRuntimeServices,
    GraphBuild,
    MainWorkerBootstrap,
    UserExecution,
    OutputDrain,
    CancelKillReap,
}

/// Safe process-resource values.  No paths, arguments, environment values, or
/// runtime state are retained here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResourceSample {
    pub(crate) threads: Option<u64>,
    pub(crate) fds: Option<u64>,
    pub(crate) rss_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ResourceTiming {
    pub(crate) start: ResourceSample,
    pub(crate) end: ResourceSample,
}

/// The crate-private phase/resource snapshot attached to an execution report.
///
/// `admission` and `queue_wait` are always present so the admission scheduler
/// can report its wait without adding another report type.  Optional phases are
/// absent when that work did not happen on the observed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PhaseTiming {
    pub(crate) admission: Duration,
    pub(crate) queue_wait: Duration,
    pub(crate) resolver_manifest_probe: Option<Duration>,
    pub(crate) resolver_reuse: Option<Duration>,
    pub(crate) resolver_rebuild: Option<Duration>,
    pub(crate) permission_runtime_services: Option<Duration>,
    pub(crate) graph_build: Option<Duration>,
    pub(crate) main_worker_bootstrap: Option<Duration>,
    pub(crate) user_execution: Option<Duration>,
    pub(crate) output_drain: Option<Duration>,
    pub(crate) cancel_kill_reap: Option<Duration>,
    pub(crate) resources: ResourceTiming,
}

/// Experimental, feature-gated diagnostic data derived from [`PhaseTiming`].
///
/// This intentionally contains only phase durations in nanoseconds and
/// bounded parent-process resource counters. It is not a stable product
/// contract.
#[cfg(feature = "phase-diagnostics")]
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseDiagnostics {
    pub admission_ns: u128,
    pub queue_wait_ns: u128,
    pub resolver_manifest_probe_ns: Option<u128>,
    pub resolver_reuse_ns: Option<u128>,
    pub resolver_rebuild_ns: Option<u128>,
    pub permission_runtime_services_ns: Option<u128>,
    pub graph_build_ns: Option<u128>,
    pub main_worker_bootstrap_ns: Option<u128>,
    pub user_execution_ns: Option<u128>,
    pub output_drain_ns: Option<u128>,
    pub cancel_kill_reap_ns: Option<u128>,
    pub parent_threads_before: Option<u64>,
    pub parent_threads_after: Option<u64>,
    pub parent_fds_before: Option<u64>,
    pub parent_fds_after: Option<u64>,
    pub parent_rss_bytes_before: Option<u64>,
    pub parent_rss_bytes_after: Option<u64>,
}

impl Default for PhaseTiming {
    fn default() -> Self {
        Self {
            admission: Duration::ZERO,
            queue_wait: Duration::ZERO,
            resolver_manifest_probe: None,
            resolver_reuse: None,
            resolver_rebuild: None,
            permission_runtime_services: None,
            graph_build: None,
            main_worker_bootstrap: None,
            user_execution: None,
            output_drain: None,
            cancel_kill_reap: None,
            resources: ResourceTiming::default(),
        }
    }
}

#[cfg(feature = "phase-diagnostics")]
impl PhaseTiming {
    /// Produces the deliberately sanitized feature-gated diagnostic view.
    pub(crate) fn sanitized_snapshot(&self) -> PhaseDiagnostics {
        let duration_ns = |duration: Option<Duration>| duration.map(|value| value.as_nanos());
        PhaseDiagnostics {
            admission_ns: self.admission.as_nanos(),
            queue_wait_ns: self.queue_wait.as_nanos(),
            resolver_manifest_probe_ns: duration_ns(self.resolver_manifest_probe),
            resolver_reuse_ns: duration_ns(self.resolver_reuse),
            resolver_rebuild_ns: duration_ns(self.resolver_rebuild),
            permission_runtime_services_ns: duration_ns(self.permission_runtime_services),
            graph_build_ns: duration_ns(self.graph_build),
            main_worker_bootstrap_ns: duration_ns(self.main_worker_bootstrap),
            user_execution_ns: duration_ns(self.user_execution),
            output_drain_ns: duration_ns(self.output_drain),
            cancel_kill_reap_ns: duration_ns(self.cancel_kill_reap),
            parent_threads_before: self.resources.start.threads,
            parent_threads_after: self.resources.end.threads,
            parent_fds_before: self.resources.start.fds,
            parent_fds_after: self.resources.end.fds,
            parent_rss_bytes_before: self.resources.start.rss_bytes,
            parent_rss_bytes_after: self.resources.end.rss_bytes,
        }
    }
}

struct TimingState {
    phases: PhaseTiming,
    #[cfg(feature = "execution-control")]
    queue_wait_recorded: bool,
}

/// Shared mutable sink used by sync, async, and subprocess wrapper paths.
/// Cloning it does not duplicate observations; all clones update one report.
#[derive(Clone)]
pub(crate) struct ExecutionTiming {
    state: Option<Arc<Mutex<TimingState>>>,
}

impl ExecutionTiming {
    /// Creates an enabled observation with monotonic phase clocks and a safe
    /// resource sample at entry.
    pub(crate) fn enabled() -> Self {
        Self {
            state: Some(Arc::new(Mutex::new(TimingState {
                phases: PhaseTiming {
                    resources: ResourceTiming {
                        start: resource_sample(),
                        end: ResourceSample::default(),
                    },
                    ..PhaseTiming::default()
                },
                #[cfg(feature = "execution-control")]
                queue_wait_recorded: false,
            }))),
        }
    }

    /// Legacy paths keep the same result/output/error behavior and do not pay
    /// for observation state or resource probes.
    pub(crate) fn disabled() -> Self {
        Self { state: None }
    }

    pub(crate) fn span(&self, phase: Phase) -> TimingSpan {
        TimingSpan {
            timing: self.clone(),
            phase,
            started: self.state.as_ref().map(|_| Instant::now()),
        }
    }

    /// Records the monotonic wait from submission until admission.  Queue
    /// terminal paths use the same method so every report gets one value even
    /// when a task never reaches the backend.
    #[cfg(feature = "execution-control")]
    pub(crate) fn record_queue_wait(&self, submitted: Instant) {
        let Some(state) = &self.state else {
            return;
        };
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.queue_wait_recorded {
            state.phases.queue_wait = submitted.elapsed();
            state.queue_wait_recorded = true;
        }
    }

    fn add(&self, phase: Phase, elapsed: Duration) {
        let Some(state) = &self.state else {
            return;
        };
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        match phase {
            Phase::Admission => state.phases.admission += elapsed,
            Phase::ResolverManifestProbe => {
                add_optional(&mut state.phases.resolver_manifest_probe, elapsed)
            }
            Phase::ResolverReuse => add_optional(&mut state.phases.resolver_reuse, elapsed),
            Phase::ResolverRebuild => add_optional(&mut state.phases.resolver_rebuild, elapsed),
            Phase::PermissionRuntimeServices => {
                add_optional(&mut state.phases.permission_runtime_services, elapsed)
            }
            Phase::GraphBuild => add_optional(&mut state.phases.graph_build, elapsed),
            Phase::MainWorkerBootstrap => {
                add_optional(&mut state.phases.main_worker_bootstrap, elapsed)
            }
            Phase::UserExecution => add_optional(&mut state.phases.user_execution, elapsed),
            Phase::OutputDrain => add_optional(&mut state.phases.output_drain, elapsed),
            Phase::CancelKillReap => add_optional(&mut state.phases.cancel_kill_reap, elapsed),
        }
    }

    /// Takes a safe report snapshot.  The public report only receives the
    /// resulting durations/counts, never the timing sink or any execution
    /// inputs.
    pub(crate) fn snapshot(&self) -> PhaseTiming {
        let Some(state) = &self.state else {
            return PhaseTiming::default();
        };
        let mut state = state.lock().unwrap_or_else(|error| error.into_inner());
        state.phases.resources.end = resource_sample();
        state.phases.clone()
    }
}

fn add_optional(value: &mut Option<Duration>, elapsed: Duration) {
    *value = Some(value.unwrap_or_default() + elapsed);
}

/// RAII phase recorder.  Dropping it is the only place where a phase duration
/// is committed, which keeps error/early-return paths observable too.
pub(crate) struct TimingSpan {
    timing: ExecutionTiming,
    phase: Phase,
    started: Option<Instant>,
}

impl Drop for TimingSpan {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            self.timing.add(self.phase, started.elapsed());
        }
    }
}

fn resource_sample() -> ResourceSample {
    ResourceSample {
        threads: thread_count(),
        fds: fd_count(),
        rss_bytes: rss_bytes(),
    }
}

#[cfg(target_os = "linux")]
fn thread_count() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("Threads:")?.trim().parse().ok())
}

#[cfg(not(target_os = "linux"))]
fn thread_count() -> Option<u64> {
    // Keep platform-specific process inspection out of the library's normal
    // execution path.  The diagnostic benchmark may report NA when the host
    // does not expose a stable thread counter.
    None
}

#[cfg(unix)]
fn fd_count() -> Option<u64> {
    let directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    Some(std::fs::read_dir(directory).ok()?.count() as u64)
}

#[cfg(not(unix))]
fn fd_count() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn rss_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    let kilobytes: u64 = text
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:")?.split_whitespace().next())?
        .parse()
        .ok()?;
    kilobytes.checked_mul(1024)
}

#[cfg(target_os = "macos")]
fn rss_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    let usage = unsafe { usage.assume_init() };
    u64::try_from(usage.ru_maxrss).ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn rss_bytes() -> Option<u64> {
    None
}
