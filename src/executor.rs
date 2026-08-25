//! Opt-in execution backend API.
//!
//! The stable surface remains the small Phase 2B backend dispatcher: in-process
//! execution uses one reusable resolver runtime, subprocess execution uses the
//! existing authenticated child protocol, and process pools / hard sandboxes
//! are reported as unsupported rather than being represented by weaker claims.
//! The opt-in `execution-control` feature adds only the Phase 1A admission and
//! request-lifecycle experiment; it does not change the legacy dispatcher.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(feature = "execution-control")]
use std::collections::{HashMap, VecDeque};
#[cfg(feature = "execution-control")]
use std::num::NonZeroUsize;
#[cfg(feature = "execution-control")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "execution-control")]
use std::sync::{Condvar, Mutex, Weak};

use crate::timing::{ExecutionTiming, Phase, PhaseTiming};
use crate::{LibdenoError, LibdenoOptions, LibdenoRuntime, RunOutput};

#[cfg(feature = "phase-diagnostics")]
#[doc(hidden)]
pub use crate::timing::PhaseDiagnostics;

/// Backend selected for an execution request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExecutionBackend {
    /// Execute in the embedding process using a fresh isolate per request.
    InProcess,
    /// Execute through the existing authenticated child-process protocol.
    Subprocess,
    /// A process pool is reserved for a later execution phase.
    ProcessPool,
}

impl fmt::Display for ExecutionBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::InProcess => "in-process",
            Self::Subprocess => "subprocess",
            Self::ProcessPool => "process-pool",
        };
        formatter.write_str(name)
    }
}

/// Capability that can be queried before building or executing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExecutionCapability {
    /// Availability of one execution backend.
    Backend(ExecutionBackend),
    /// A hard OS-level sandbox, not supplied by Phase 2B.
    HardSandbox,
}

impl fmt::Display for ExecutionCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Backend(backend) => write!(formatter, "{backend} backend"),
            Self::HardSandbox => formatter.write_str("hard sandbox"),
        }
    }
}

/// Cleanup strength reported by an experimental supervised subprocess.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionCleanupStrength {
    DirectChild,
    ProcessGroup,
    WindowsJob,
}

/// Control-transport status reported by an experimental supervised subprocess.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionTransportStatus {
    Clean,
    Failed,
}

/// Whether a capability can be provided by this API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CapabilityAvailability {
    /// The capability is available for use.
    Available,
    /// The capability is not implemented by this execution phase.
    Unsupported,
}

/// Outcome of a capability for one execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CapabilityOutcome {
    /// The request did not ask for this capability.
    NotRequested,
    /// The capability was dispatched and completed its backend operation.
    Used,
    /// The capability was requested or dispatched but execution failed.
    Failed,
}

/// Phase 2B capability availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapabilityReport {
    backends: [CapabilityAvailability; 3],
    hard_sandbox: CapabilityAvailability,
    cleanup_strength: Option<ExecutionCleanupStrength>,
}

impl Default for CapabilityReport {
    fn default() -> Self {
        Self {
            backends: [
                CapabilityAvailability::Available,
                CapabilityAvailability::Available,
                CapabilityAvailability::Unsupported,
            ],
            hard_sandbox: CapabilityAvailability::Unsupported,
            cleanup_strength: if cfg!(feature = "execution-control") {
                Some(ExecutionCleanupStrength::DirectChild)
            } else {
                None
            },
        }
    }
}

impl CapabilityReport {
    /// Returns the availability of `capability`.
    pub fn availability(&self, capability: ExecutionCapability) -> CapabilityAvailability {
        match capability {
            ExecutionCapability::Backend(ExecutionBackend::InProcess) => self.backends[0],
            ExecutionCapability::Backend(ExecutionBackend::Subprocess) => self.backends[1],
            ExecutionCapability::Backend(ExecutionBackend::ProcessPool) => self.backends[2],
            ExecutionCapability::HardSandbox => self.hard_sandbox,
        }
    }

    /// Returns the cleanup strength available to the experimental supervised
    /// subprocess lane, when compiled in.
    #[doc(hidden)]
    pub fn cleanup_strength(&self) -> Option<ExecutionCleanupStrength> {
        self.cleanup_strength
    }
}

/// One owned execution request.
#[derive(Debug, Clone)]
pub struct ExecutionRequest {
    entry: PathBuf,
    options: LibdenoOptions,
}

#[cfg(feature = "execution-control")]
// The legacy execute/execute_async paths bypass this scheduler, so these
// finite defaults only bound the opt-in submission API.
const DEFAULT_ACTIVE_LIMIT: NonZeroUsize = match NonZeroUsize::new(16) {
    Some(value) => value,
    None => unreachable!(),
};
#[cfg(feature = "execution-control")]
const DEFAULT_QUEUE_LIMIT: usize = 64;

#[cfg(feature = "execution-control")]
/// Immutable active/queued admission limits for the experimental submission
/// API.
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct AdmissionConfig {
    /// Maximum number of accepted tasks with an active permit.
    pub active_limit: NonZeroUsize,
    /// Maximum number of tasks waiting in the FIFO queue.
    pub queue_limit: usize,
}

#[cfg(feature = "execution-control")]
impl fmt::Debug for AdmissionConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmissionConfig")
            .field("active_limit", &self.active_limit)
            .field("queue_limit", &self.queue_limit)
            .finish()
    }
}

#[cfg(feature = "execution-control")]
impl AdmissionConfig {
    /// Creates immutable admission settings.
    #[doc(hidden)]
    pub fn new(active_limit: NonZeroUsize, queue_limit: usize) -> Self {
        Self {
            active_limit,
            queue_limit,
        }
    }
}

#[cfg(feature = "execution-control")]
impl Default for AdmissionConfig {
    /// Uses 16 active permits and a FIFO queue of up to 64 waiting tasks.
    fn default() -> Self {
        Self {
            active_limit: DEFAULT_ACTIVE_LIMIT,
            queue_limit: DEFAULT_QUEUE_LIMIT,
        }
    }
}

#[cfg(feature = "execution-control")]
/// Request-local controls for an experimental submission.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SubmissionOptions {
    /// Wall-clock budget measured from `Executor::submit` through terminal
    /// state. It includes time spent in the admission queue.
    pub request_timeout: Option<Duration>,
}

#[cfg(feature = "execution-control")]
impl SubmissionOptions {
    /// Creates submission options with a wall-clock request timeout.
    #[doc(hidden)]
    pub fn new(request_timeout: Option<Duration>) -> Self {
        Self { request_timeout }
    }
}

#[cfg(feature = "execution-control")]
/// Lifecycle state of an experimental submission.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ExecutionState {
    Queued,
    AcceptedNotStarted,
    Started,
    Cancelling,
    Terminated,
    Completed,
    Failed,
}

#[cfg(feature = "execution-control")]
/// Result of an idempotent cancellation request.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CancelOutcome {
    Requested,
    AlreadyRequested,
    AlreadyTerminal,
    NotEnforceableForStartedSubprocess,
}

#[cfg(feature = "execution-control")]
/// Failure to admit a new experimental submission.
#[doc(hidden)]
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubmitError {
    #[error("executor is shut down")]
    Shutdown,
    #[error("execution queue is full")]
    QueueFull,
    #[error("execution request timeout is too large")]
    InvalidTimeout,
    #[error("internal executor error: {0}")]
    Internal(String),
}

#[cfg(feature = "execution-control")]
/// Summary of one idempotent executor shutdown request.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    queued_cancelled: usize,
    accepted_not_started_cancelled: usize,
    active_cancel_requested: usize,
}

#[cfg(feature = "execution-control")]
impl ShutdownReport {
    /// Number of queued tasks cancelled without backend creation.
    #[doc(hidden)]
    pub fn queued_cancelled(&self) -> usize {
        self.queued_cancelled
    }

    /// Number of accepted tasks that had not reached `Started` when shutdown
    /// linearized. These are cancelled without consuming active grace.
    #[doc(hidden)]
    pub fn accepted_not_started_cancelled(&self) -> usize {
        self.accepted_not_started_cancelled
    }

    /// Number of active tasks for which shutdown requested cancellation after
    /// the grace interval.
    #[doc(hidden)]
    pub fn active_cancel_requested(&self) -> usize {
        self.active_cancel_requested
    }
}

impl ExecutionRequest {
    /// Creates a request for `entry` with the supplied runtime options.
    pub fn new(entry: impl AsRef<Path>, options: LibdenoOptions) -> Self {
        Self {
            entry: entry.as_ref().to_path_buf(),
            options,
        }
    }

    /// Returns the request entry path.
    pub fn entry(&self) -> &Path {
        &self.entry
    }

    /// Returns the request's owned runtime options.
    pub fn options(&self) -> &LibdenoOptions {
        &self.options
    }
}

/// Captured output from one execution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    truncated: bool,
}

impl ExecutionOutput {
    /// Returns captured stdout bytes.
    pub fn stdout(&self) -> &[u8] {
        &self.stdout
    }

    /// Returns captured stderr bytes.
    pub fn stderr(&self) -> &[u8] {
        &self.stderr
    }

    /// Returns whether captured output exceeded its configured budget.
    pub fn capture_truncated(&self) -> bool {
        self.truncated
    }
}

/// Report for a completed or failed dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionReport {
    requested_backend: ExecutionBackend,
    dispatched_backend: Option<ExecutionBackend>,
    elapsed: Duration,
    backend_outcome: CapabilityOutcome,
    timing: Box<PhaseTiming>,
    cleanup_strength: Option<ExecutionCleanupStrength>,
    transport_status: Option<ExecutionTransportStatus>,
}

impl ExecutionReport {
    /// Returns the backend requested when the executor was built.
    pub fn requested_backend(&self) -> ExecutionBackend {
        self.requested_backend
    }

    /// Returns the backend actually dispatched, or `None` when validation
    /// failed before dispatch.
    pub fn dispatched_backend(&self) -> Option<ExecutionBackend> {
        self.dispatched_backend
    }

    /// Returns the elapsed dispatch time.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// Returns the outcome of `capability` for this execution.
    pub fn outcome(&self, capability: ExecutionCapability) -> CapabilityOutcome {
        match capability {
            ExecutionCapability::Backend(backend) if backend == self.requested_backend => {
                self.backend_outcome
            }
            _ => CapabilityOutcome::NotRequested,
        }
    }

    /// Returns the truthful cleanup strength, when the backend reported one.
    #[doc(hidden)]
    pub fn cleanup_strength(&self) -> Option<ExecutionCleanupStrength> {
        self.cleanup_strength
    }

    /// Returns the supervisor transport status, when the backend reported one.
    #[doc(hidden)]
    pub fn transport_status(&self) -> Option<ExecutionTransportStatus> {
        self.transport_status
    }

    /// Experimental feature-gated phase diagnostics. This is a sanitized
    /// observation surface, not a stable product contract.
    #[cfg(feature = "phase-diagnostics")]
    #[doc(hidden)]
    pub fn phase_diagnostics(&self) -> PhaseDiagnostics {
        self.timing.sanitized_snapshot()
    }

    /// Crate-private phase/resource observations for later execution phases.
    #[allow(dead_code)]
    pub(crate) fn phase_timing(&self) -> &PhaseTiming {
        self.timing.as_ref()
    }
}

/// Successful execution result. A nonzero script exit code is still a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionResult {
    exit_code: i32,
    output: ExecutionOutput,
    report: ExecutionReport,
}

impl ExecutionResult {
    /// Returns the script's exit code.
    pub fn exit_code(&self) -> i32 {
        self.exit_code
    }

    /// Returns captured output, or empty streams when capture was not asked
    /// for.
    pub fn output(&self) -> &ExecutionOutput {
        &self.output
    }

    /// Returns the execution report.
    pub fn report(&self) -> &ExecutionReport {
        &self.report
    }

    /// Splits the result into its exit code, output, and report.
    pub fn into_parts(self) -> (i32, ExecutionOutput, ExecutionReport) {
        (self.exit_code, self.output, self.report)
    }
}

/// A capability that could not be provided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedCapability {
    capability: ExecutionCapability,
    reason: String,
}

impl UnsupportedCapability {
    fn new(capability: ExecutionCapability, reason: impl Into<String>) -> Self {
        Self {
            capability,
            reason: reason.into(),
        }
    }

    /// Returns the unsupported capability.
    pub fn capability(&self) -> ExecutionCapability {
        self.capability
    }

    /// Returns the reason the capability is unsupported.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for UnsupportedCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unsupported capability {}: {}",
            self.capability, self.reason
        )
    }
}

impl std::error::Error for UnsupportedCapability {}

/// Errors raised while building or dispatching an executor request.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExecutionError {
    /// An unchanged legacy libdeno error.
    #[error("{0}")]
    Libdeno(#[from] LibdenoError),
    /// The requested execution capability is not implemented.
    #[error("{0}")]
    Unsupported(UnsupportedCapability),
    /// An executor task or internal join failed without a legacy error.
    #[error("internal executor error: {0}")]
    Internal(String),
    /// An experimental submission reached its cancellation terminal state.
    #[cfg(feature = "execution-control")]
    #[error("execution cancelled")]
    Cancelled,
}

/// Details for a failed execution.
#[derive(Debug, Clone)]
pub struct ExecutionFailure {
    error: Arc<ExecutionError>,
    report: Arc<ExecutionReport>,
    partial_output: Option<Arc<ExecutionOutput>>,
}

impl fmt::Display for ExecutionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for ExecutionFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

impl ExecutionFailure {
    fn new(
        error: ExecutionError,
        report: ExecutionReport,
        partial_output: Option<ExecutionOutput>,
    ) -> Self {
        Self {
            error: Arc::new(error),
            report: Arc::new(report),
            partial_output: partial_output.map(Arc::new),
        }
    }

    /// Returns the execution error.
    pub fn error(&self) -> &ExecutionError {
        self.error.as_ref()
    }

    /// Returns the failure report.
    pub fn report(&self) -> &ExecutionReport {
        self.report.as_ref()
    }

    /// Returns partial output when a backend can provide it. Phase 2B's
    /// existing capture paths do not expose error-time partial output, so this
    /// is always `None` for current backends.
    pub fn partial_output(&self) -> Option<&ExecutionOutput> {
        self.partial_output.as_deref()
    }

    /// Splits the failure into its error, report, and optional partial output.
    pub fn into_parts(self) -> (ExecutionError, ExecutionReport, Option<ExecutionOutput>) {
        let error =
            Arc::try_unwrap(self.error).unwrap_or_else(|error| clone_execution_error(&error));
        let report = Arc::try_unwrap(self.report).unwrap_or_else(|report| (*report).clone());
        let partial_output = self
            .partial_output
            .map(|output| Arc::try_unwrap(output).unwrap_or_else(|output| (*output).clone()));
        (error, report, partial_output)
    }
}

/// A cloneable source used only when a public failure has more than one owner.
/// It keeps the original typed error reachable instead of flattening it into a
/// message-only substitute.
#[derive(Debug)]
struct SharedError {
    owner: Arc<ExecutionError>,
    message: String,
    class: String,
    source: SharedErrorSource,
}

#[derive(Debug, Clone, Copy)]
enum SharedErrorSource {
    Owner,
    CoreKind,
    CoreJsBoxInner,
}

impl SharedError {
    fn new(
        owner: Arc<ExecutionError>,
        error: &(dyn std::error::Error + 'static),
        class: impl Into<String>,
        source: SharedErrorSource,
    ) -> Self {
        Self {
            owner,
            message: error.to_string(),
            class: class.into(),
            source,
        }
    }
}

impl fmt::Display for SharedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for SharedError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.source {
            SharedErrorSource::Owner => Some(self.owner.as_ref()),
            SharedErrorSource::CoreKind => match self.owner.as_ref() {
                ExecutionError::Libdeno(LibdenoError::Core(error)) => Some(error.0.as_ref()),
                _ => Some(self.owner.as_ref()),
            },
            SharedErrorSource::CoreJsBoxInner => match self.owner.as_ref() {
                ExecutionError::Libdeno(LibdenoError::Core(error)) => match error.0.as_ref() {
                    deno_core::error::CoreErrorKind::JsBox(error) => error
                        .get_inner_ref()
                        .map(|error| error as &(dyn std::error::Error + 'static))
                        .or(Some(error)),
                    _ => Some(error.0.as_ref()),
                },
                _ => Some(self.owner.as_ref()),
            },
        }
    }
}

impl deno_error::JsErrorClass for SharedError {
    fn get_class(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Owned(self.class.clone())
    }

    fn get_message(&self) -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Owned(self.message.clone())
    }

    fn get_additional_properties(&self) -> deno_error::AdditionalProperties {
        Box::new(std::iter::empty())
    }

    fn get_ref(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }
}

fn clone_libdeno_error(error: &LibdenoError, owner: Arc<ExecutionError>) -> LibdenoError {
    match error {
        LibdenoError::Entry(error) => LibdenoError::Entry(deno_core::anyhow::Error::new(
            SharedError::new(owner, error.as_ref(), "Error", SharedErrorSource::Owner),
        )),
        LibdenoError::Permission(message) => LibdenoError::Permission(message.clone()),
        LibdenoError::Configuration(message) => LibdenoError::Configuration(message.clone()),
        LibdenoError::Runtime(error) => LibdenoError::Runtime(deno_core::anyhow::Error::new(
            SharedError::new(owner, error.as_ref(), "Error", SharedErrorSource::Owner),
        )),
        LibdenoError::Core(error) => LibdenoError::Core(
            deno_core::error::CoreErrorKind::JsBox(deno_error::JsErrorBox::from_err(
                SharedError::new(
                    owner,
                    error,
                    deno_error::JsErrorClass::get_class(error).into_owned(),
                    if matches!(error.0.as_ref(), deno_core::error::CoreErrorKind::JsBox(_)) {
                        SharedErrorSource::CoreJsBoxInner
                    } else {
                        SharedErrorSource::CoreKind
                    },
                ),
            ))
            .into_box(),
        ),
        LibdenoError::Io(error) => LibdenoError::Io(std::io::Error::new(
            error.kind(),
            SharedError::new(owner, error, "Error", SharedErrorSource::Owner),
        )),
        LibdenoError::Timeout(message) => LibdenoError::Timeout(message.clone()),
    }
}

fn clone_execution_error(error: &Arc<ExecutionError>) -> ExecutionError {
    match error.as_ref() {
        ExecutionError::Libdeno(libdeno_error) => {
            ExecutionError::Libdeno(clone_libdeno_error(libdeno_error, error.clone()))
        }
        ExecutionError::Unsupported(error) => ExecutionError::Unsupported(error.clone()),
        ExecutionError::Internal(message) => ExecutionError::Internal(message.clone()),
        #[cfg(feature = "execution-control")]
        ExecutionError::Cancelled => ExecutionError::Cancelled,
    }
}

#[cfg(feature = "execution-control")]
struct ResultCell {
    completion: Mutex<Option<Result<ExecutionResult, ExecutionFailure>>>,
    notify: tokio::sync::Notify,
}

#[cfg(feature = "execution-control")]
impl ResultCell {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            completion: Mutex::new(None),
            notify: tokio::sync::Notify::new(),
        })
    }

    fn publish(&self, result: Result<ExecutionResult, ExecutionFailure>) {
        let mut completion = self
            .completion
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if completion.is_none() {
            *completion = Some(result);
            self.notify.notify_waiters();
        }
    }

    async fn wait(&self) -> Result<ExecutionResult, ExecutionFailure> {
        loop {
            let notified = self.notify.notified();
            if let Some(result) = self
                .completion
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
            {
                return result;
            }
            notified.await;
        }
    }
}

#[cfg(feature = "execution-control")]
/// Experimental owner/handle for one admitted execution.
///
/// Dropping this handle, or the future returned by [`Self::result`], only
/// detaches the caller. The scheduler-owned task owner retains its permit and
/// continues until a terminal state.
#[doc(hidden)]
#[derive(Clone)]
pub struct ExecutionHandle {
    control: Arc<TaskControl>,
    result: Arc<ResultCell>,
}

#[cfg(feature = "execution-control")]
impl fmt::Debug for ExecutionHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ExecutionHandle")
            .field("id", &self.control.id)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "execution-control")]
impl ExecutionHandle {
    /// Requests cancellation. The request is idempotent and does not imply a
    /// hard interrupt for in-process or legacy subprocess execution.
    #[doc(hidden)]
    pub fn cancel(&self) -> CancelOutcome {
        self.control.request_cancel(CancelReason::User)
    }

    /// Returns the current lifecycle state.
    #[doc(hidden)]
    pub fn state(&self) -> ExecutionState {
        self.control.state()
    }

    /// Waits for the shared terminal result. The same or cloned handles may
    /// await the result repeatedly; dropping this future detaches only.
    #[doc(hidden)]
    pub async fn result(&self) -> Result<ExecutionResult, ExecutionFailure> {
        self.result.wait().await
    }

    /// Returns the submission identifier for diagnostics and FIFO assertions.
    #[doc(hidden)]
    pub fn id(&self) -> u64 {
        self.control.id
    }
}

#[cfg(feature = "execution-control")]
#[derive(Clone)]
struct ExecutorDispatch {
    project_dir: PathBuf,
    backend: ExecutionBackend,
    state: ExecutorBackend,
    #[cfg(test)]
    panic_on_execute: bool,
}

#[cfg(feature = "execution-control")]
#[derive(Clone, Copy)]
enum CancelReason {
    User,
    Deadline,
    Shutdown,
}

#[cfg(feature = "execution-control")]
fn cancellation_reason(reason: CancelReason) -> crate::limits::CancellationReason {
    match reason {
        CancelReason::User => crate::limits::CancellationReason::User,
        CancelReason::Deadline => crate::limits::CancellationReason::Deadline,
        CancelReason::Shutdown => crate::limits::CancellationReason::Shutdown,
    }
}

#[cfg(feature = "execution-control")]
fn supervisor_cancel_reason(reason: CancelReason) -> crate::supervisor::CancelReason {
    match reason {
        CancelReason::User => crate::supervisor::CancelReason::User,
        CancelReason::Deadline => crate::supervisor::CancelReason::Deadline,
        CancelReason::Shutdown => crate::supervisor::CancelReason::Shutdown,
    }
}

#[cfg(feature = "execution-control")]
struct ControlState {
    lifecycle: ExecutionState,
    cancel_reason: Option<CancelReason>,
}

#[cfg(feature = "execution-control")]
struct TaskControl {
    id: u64,
    backend: ExecutionBackend,
    submitted: Instant,
    timing: ExecutionTiming,
    deadline: Option<Instant>,
    scheduler: Weak<AdmissionScheduler>,
    cancellation: crate::limits::CancellationContext,
    state: Mutex<ControlState>,
}

#[cfg(feature = "execution-control")]
impl TaskControl {
    fn new(
        id: u64,
        backend: ExecutionBackend,
        submitted: Instant,
        deadline: Option<Instant>,
        timing: ExecutionTiming,
        scheduler: Weak<AdmissionScheduler>,
    ) -> Self {
        Self {
            id,
            backend,
            submitted,
            timing,
            deadline,
            scheduler,
            cancellation: crate::limits::CancellationContext::new(),
            state: Mutex::new(ControlState {
                lifecycle: ExecutionState::Queued,
                cancel_reason: None,
            }),
        }
    }

    fn state(&self) -> ExecutionState {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .lifecycle
    }

    fn mark_accepted(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.lifecycle == ExecutionState::Queued {
            state.lifecycle = ExecutionState::AcceptedNotStarted;
        }
    }

    fn is_terminal(state: ExecutionState) -> bool {
        matches!(
            state,
            ExecutionState::Terminated | ExecutionState::Completed | ExecutionState::Failed
        )
    }

    fn begin_start(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if Self::is_terminal(state.lifecycle) || state.cancel_reason.is_some() {
            return false;
        }
        if self
            .deadline
            .is_some_and(|deadline| deadline <= Instant::now())
        {
            state.cancel_reason = Some(CancelReason::Deadline);
            state.lifecycle = ExecutionState::Cancelling;
            drop(state);
            self.cancellation
                .request_with_reason(crate::limits::CancellationReason::Deadline);
            return false;
        }
        // Supervised subprocesses publish Started only after the child emits
        // valid STARTED. In-process submission retains its existing start
        // point here.
        if self.backend != ExecutionBackend::Subprocess {
            state.lifecycle = ExecutionState::Started;
        }
        true
    }

    fn mark_started(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.lifecycle == ExecutionState::AcceptedNotStarted && state.cancel_reason.is_none() {
            state.lifecycle = ExecutionState::Started;
            true
        } else {
            false
        }
    }

    fn request_cancel(&self, reason: CancelReason) -> CancelOutcome {
        let Some(scheduler) = self.scheduler.upgrade() else {
            return if Self::is_terminal(self.state()) {
                CancelOutcome::AlreadyTerminal
            } else {
                CancelOutcome::AlreadyRequested
            };
        };
        scheduler.request_cancel(self, reason)
    }

    fn mark_cancelling(&self, reason: CancelReason) -> CancelOutcome {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if Self::is_terminal(state.lifecycle) {
            return CancelOutcome::AlreadyTerminal;
        }
        if state.cancel_reason.is_some() {
            return CancelOutcome::AlreadyRequested;
        }
        state.cancel_reason = Some(reason);
        state.lifecycle = ExecutionState::Cancelling;
        drop(state);
        self.cancellation
            .request_with_reason(cancellation_reason(reason));
        CancelOutcome::Requested
    }

    fn mark_terminal(&self, lifecycle: ExecutionState) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if Self::is_terminal(state.lifecycle) {
            return false;
        }
        state.lifecycle = lifecycle;
        true
    }

    fn resolve_backend_result(
        &self,
        backend_result: Result<ExecutionResult, ExecutionFailure>,
        dispatched: ExecutionBackend,
    ) -> Option<(ExecutionState, Result<ExecutionResult, ExecutionFailure>)> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if Self::is_terminal(state.lifecycle) {
            return None;
        }
        let (cleanup_strength, transport_status) = match &backend_result {
            Ok(result) => (
                result.report.cleanup_strength,
                result.report.transport_status,
            ),
            Err(failure) => (
                failure.report.cleanup_strength,
                failure.report.transport_status,
            ),
        };
        let (lifecycle, result) = match state.cancel_reason {
            Some(CancelReason::Deadline) => (
                ExecutionState::Terminated,
                Err(self.deadline_failure_with_metadata(
                    Some(dispatched),
                    cleanup_strength,
                    transport_status,
                )),
            ),
            Some(CancelReason::User | CancelReason::Shutdown) => (
                ExecutionState::Terminated,
                Err(self.cancelled_failure_with_metadata(
                    Some(dispatched),
                    cleanup_strength,
                    transport_status,
                )),
            ),
            None => match backend_result {
                Ok(result) => (ExecutionState::Completed, Ok(result)),
                Err(failure) => {
                    let timed_out = matches!(
                        failure.error(),
                        ExecutionError::Libdeno(LibdenoError::Timeout(_))
                    );
                    (
                        if timed_out {
                            ExecutionState::Terminated
                        } else {
                            ExecutionState::Failed
                        },
                        Err(failure),
                    )
                }
            },
        };
        state.lifecycle = lifecycle;
        Some((lifecycle, result))
    }

    fn cancellation_reason(&self) -> Option<CancelReason> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cancel_reason
    }

    fn record_queue_wait(&self) {
        self.timing.record_queue_wait(self.submitted);
    }

    fn report(
        &self,
        dispatched: Option<ExecutionBackend>,
        timing: ExecutionTiming,
    ) -> ExecutionReport {
        self.report_with_metadata(dispatched, timing, None, None)
    }

    fn report_with_metadata(
        &self,
        dispatched: Option<ExecutionBackend>,
        timing: ExecutionTiming,
        cleanup_strength: Option<ExecutionCleanupStrength>,
        transport_status: Option<ExecutionTransportStatus>,
    ) -> ExecutionReport {
        self.record_queue_wait();
        ExecutionReport {
            requested_backend: self.backend,
            dispatched_backend: dispatched,
            elapsed: self.submitted.elapsed(),
            backend_outcome: CapabilityOutcome::Failed,
            timing: Box::new(timing.snapshot()),
            cleanup_strength,
            transport_status,
        }
    }

    fn cancelled_failure(&self, dispatched: Option<ExecutionBackend>) -> ExecutionFailure {
        self.cancelled_failure_with_metadata(dispatched, None, None)
    }

    fn cancelled_failure_with_metadata(
        &self,
        dispatched: Option<ExecutionBackend>,
        cleanup_strength: Option<ExecutionCleanupStrength>,
        transport_status: Option<ExecutionTransportStatus>,
    ) -> ExecutionFailure {
        ExecutionFailure::new(
            ExecutionError::Cancelled,
            self.report_with_metadata(
                dispatched,
                self.timing.clone(),
                cleanup_strength,
                transport_status,
            ),
            None,
        )
    }

    fn deadline_failure(&self, dispatched: Option<ExecutionBackend>) -> ExecutionFailure {
        self.deadline_failure_with_metadata(dispatched, None, None)
    }

    fn deadline_failure_with_metadata(
        &self,
        dispatched: Option<ExecutionBackend>,
        cleanup_strength: Option<ExecutionCleanupStrength>,
        transport_status: Option<ExecutionTransportStatus>,
    ) -> ExecutionFailure {
        ExecutionFailure::new(
            ExecutionError::Libdeno(LibdenoError::Timeout(
                "execution request deadline exceeded".to_string(),
            )),
            self.report_with_metadata(
                dispatched,
                self.timing.clone(),
                cleanup_strength,
                transport_status,
            ),
            None,
        )
    }
}

#[cfg(feature = "execution-control")]
struct ActivePermit {
    scheduler: Arc<AdmissionScheduler>,
    id: u64,
    released: bool,
}

#[cfg(feature = "execution-control")]
impl ActivePermit {
    fn new(scheduler: Arc<AdmissionScheduler>, id: u64) -> Self {
        Self {
            scheduler,
            id,
            released: false,
        }
    }
}

#[cfg(feature = "execution-control")]
impl Drop for ActivePermit {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            self.scheduler.release_active(self.id);
        }
    }
}

#[cfg(feature = "execution-control")]
struct QueuePermit;

#[cfg(feature = "execution-control")]
enum TaskPermit {
    Active(ActivePermit),
    Queued(QueuePermit),
}

#[cfg(feature = "execution-control")]
struct TaskOwner {
    id: u64,
    request: ExecutionRequest,
    submission: SubmissionOptions,
    deadline: Option<Instant>,
    control: Arc<TaskControl>,
    runner: ExecutorDispatch,
    timing: ExecutionTiming,
    result: Arc<ResultCell>,
    permit: Option<TaskPermit>,
}

#[cfg(feature = "execution-control")]
impl TaskOwner {
    fn promote(&mut self, scheduler: Arc<AdmissionScheduler>) {
        let permit = self
            .permit
            .take()
            .expect("queued task owner must hold a permit");
        self.permit = Some(match permit {
            TaskPermit::Queued(_) => TaskPermit::Active(ActivePermit::new(scheduler, self.id)),
            TaskPermit::Active(active) => TaskPermit::Active(active),
        });
    }

    fn send_terminal(
        &mut self,
        lifecycle: ExecutionState,
        result: Result<ExecutionResult, ExecutionFailure>,
    ) {
        if !self.control.mark_terminal(lifecycle) {
            return;
        }
        self.publish_terminal(result);
    }

    fn publish_terminal(&mut self, result: Result<ExecutionResult, ExecutionFailure>) {
        // Terminal state is published before the internal permit is released;
        // release it before waking the caller so an awaited result observes a
        // scheduler that can already reuse the slot.
        let permit = self.permit.take();
        drop(permit);
        self.result.publish(result);
    }

    fn run(&mut self) {
        if !self.control.begin_start() {
            let result = match self.control.cancellation_reason() {
                Some(CancelReason::Deadline) => Err(self.control.deadline_failure(None)),
                Some(CancelReason::User | CancelReason::Shutdown) => {
                    Err(self.control.cancelled_failure(None))
                }
                None => Err(ExecutionFailure::new(
                    ExecutionError::Internal("submission was not startable".to_string()),
                    self.control.report(None, self.timing.clone()),
                    None,
                )),
            };
            self.send_terminal(ExecutionState::Terminated, result);
            return;
        }

        let started_control = self.control.clone();
        let on_started = || {
            started_control.mark_started();
        };
        let backend_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.runner.execute_submission(
                self.request.clone(),
                self.submission,
                self.deadline,
                self.control.cancellation.clone(),
                self.control
                    .cancellation_reason()
                    .unwrap_or(CancelReason::User),
                self.timing.clone(),
                Some(&on_started),
            )
        }))
        .unwrap_or_else(|_| {
            Err(ExecutionFailure::new(
                ExecutionError::Internal("submission backend panicked".to_string()),
                self.control
                    .report(Some(self.runner.backend), self.timing.clone()),
                None,
            ))
        });

        if let Some((_lifecycle, result)) = self
            .control
            .resolve_backend_result(backend_result, self.runner.backend)
        {
            self.publish_terminal(result);
        }
    }
}

#[cfg(feature = "execution-control")]
struct SchedulerState {
    accepting: bool,
    queue: VecDeque<TaskOwner>,
    ready: VecDeque<TaskOwner>,
    active: HashMap<u64, Arc<TaskControl>>,
    next_id: u64,
}

#[cfg(feature = "execution-control")]
struct AdmissionScheduler {
    active_limit: NonZeroUsize,
    queue_limit: usize,
    state: Mutex<SchedulerState>,
    wake: Condvar,
    manager_started: AtomicBool,
}

#[cfg(feature = "execution-control")]
impl AdmissionScheduler {
    fn new(config: AdmissionConfig) -> Arc<Self> {
        Arc::new(Self {
            active_limit: config.active_limit,
            queue_limit: config.queue_limit,
            state: Mutex::new(SchedulerState {
                accepting: true,
                queue: VecDeque::new(),
                ready: VecDeque::new(),
                active: HashMap::new(),
                next_id: 1,
            }),
            wake: Condvar::new(),
            manager_started: AtomicBool::new(false),
        })
    }

    fn submit(
        self: &Arc<Self>,
        runner: ExecutorDispatch,
        request: ExecutionRequest,
        submission: SubmissionOptions,
    ) -> Result<ExecutionHandle, SubmitError> {
        let submitted = Instant::now();
        let deadline = match submission.request_timeout {
            Some(timeout) => Some(
                submitted
                    .checked_add(timeout)
                    .ok_or(SubmitError::InvalidTimeout)?,
            ),
            None => None,
        };
        let timing = ExecutionTiming::enabled();
        let result = ResultCell::new();
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state.accepting {
            return Err(SubmitError::Shutdown);
        }
        let id = state.next_id;
        state.next_id = state.next_id.wrapping_add(1).max(1);
        let control = Arc::new(TaskControl::new(
            id,
            runner.backend,
            submitted,
            deadline,
            timing.clone(),
            Arc::downgrade(self),
        ));
        // A released slot and an older queued task can coexist briefly while
        // the manager is waking. Keep the new task behind that queue rather
        // than admitting it directly into `ready` and bypassing FIFO.
        let permit = if state.active.len() < self.active_limit.get() && state.queue.is_empty() {
            control.mark_accepted();
            state.active.insert(id, control.clone());
            control.record_queue_wait();
            TaskPermit::Active(ActivePermit::new(self.clone(), id))
        } else {
            if state.queue.len() >= self.queue_limit {
                return Err(SubmitError::QueueFull);
            }
            TaskPermit::Queued(QueuePermit)
        };
        let owner = TaskOwner {
            id,
            request,
            submission,
            deadline,
            control: control.clone(),
            runner,
            timing,
            result: result.clone(),
            permit: Some(permit),
        };
        if matches!(owner.permit.as_ref(), Some(TaskPermit::Active(_))) {
            state.ready.push_back(owner);
        } else {
            state.queue.push_back(owner);
        }
        drop(state);
        self.wake.notify_one();
        if let Err(error) = self.ensure_manager() {
            self.fail_pending(error.to_string());
            return Err(error);
        }
        Ok(ExecutionHandle { control, result })
    }

    fn ensure_manager(self: &Arc<Self>) -> Result<(), SubmitError> {
        if self
            .manager_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Ok(());
        }
        let scheduler = self.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("libdeno-execution-manager".to_string())
            .spawn(move || scheduler.manager_loop())
        {
            self.manager_started.store(false, Ordering::Release);
            return Err(SubmitError::Internal(format!(
                "failed to start execution manager: {error}"
            )));
        }
        Ok(())
    }

    fn manager_loop(self: Arc<Self>) {
        loop {
            let mut expired = Vec::new();
            let mut dispatch = None;
            let mut deadline_controls = Vec::new();
            let mut guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let now = Instant::now();

            let mut retained = VecDeque::with_capacity(guard.queue.len());
            while let Some(owner) = guard.queue.pop_front() {
                if owner.deadline.is_some_and(|deadline| deadline <= now) {
                    let _ = owner.control.mark_cancelling(CancelReason::Deadline);
                    expired.push(owner);
                } else {
                    retained.push_back(owner);
                }
            }
            guard.queue = retained;

            let mut retained = VecDeque::with_capacity(guard.ready.len());
            while let Some(owner) = guard.ready.pop_front() {
                if owner.deadline.is_some_and(|deadline| deadline <= now) {
                    let _ = owner.control.mark_cancelling(CancelReason::Deadline);
                    expired.push(owner);
                } else {
                    retained.push_back(owner);
                }
            }
            guard.ready = retained;

            for control in guard.active.values() {
                if control.deadline.is_some_and(|deadline| deadline <= now)
                    && matches!(
                        control.state(),
                        ExecutionState::AcceptedNotStarted | ExecutionState::Started
                    )
                {
                    deadline_controls.push(control.clone());
                }
            }
            for control in deadline_controls {
                let _ = control.mark_cancelling(CancelReason::Deadline);
            }

            while guard.active.len() < self.active_limit.get() {
                let Some(mut owner) = guard.queue.pop_front() else {
                    break;
                };
                owner.promote(self.clone());
                owner.control.mark_accepted();
                guard.active.insert(owner.id, owner.control.clone());
                owner.control.record_queue_wait();
                guard.ready.push_back(owner);
            }
            if let Some(owner) = guard.ready.pop_front() {
                dispatch = Some(owner);
            }

            let should_exit = !guard.accepting
                && guard.active.is_empty()
                && guard.queue.is_empty()
                && guard.ready.is_empty();
            drop(guard);

            for mut owner in expired {
                let control = owner.control.clone();
                let result = Err(control.deadline_failure(None));
                owner.send_terminal(ExecutionState::Terminated, result);
            }
            if should_exit {
                break;
            }
            if dispatch.is_none() {
                let guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
                if guard.active.is_empty() && guard.queue.is_empty() && guard.ready.is_empty() {
                    // Release the start flag while holding the scheduler
                    // state lock. A concurrent submit then either observes
                    // this flag and starts a replacement manager, or arrives
                    // before this check and keeps this manager alive.
                    self.manager_started.store(false, Ordering::Release);
                    drop(guard);
                    return;
                }
                let wait = next_deadline(&guard).map_or(Duration::from_millis(100), |deadline| {
                    deadline.saturating_duration_since(Instant::now())
                });
                let (_guard, _) = self
                    .wake
                    .wait_timeout(guard, wait)
                    .unwrap_or_else(|error| error.into_inner());
                continue;
            }
            if let Some(owner) = dispatch {
                spawn_task(owner);
            }
        }
        self.manager_started.store(false, Ordering::Release);
    }

    fn request_cancel(&self, control: &TaskControl, reason: CancelReason) -> CancelOutcome {
        let mut owner = None;
        let outcome;
        let mut guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if Self::terminal(control.state()) {
            return CancelOutcome::AlreadyTerminal;
        }
        if let Some(index) = guard.queue.iter().position(|task| task.id == control.id) {
            owner = guard.queue.remove(index);
            outcome = control.mark_cancelling(reason);
        } else if let Some(index) = guard.ready.iter().position(|task| task.id == control.id) {
            owner = guard.ready.remove(index);
            outcome = control.mark_cancelling(reason);
        } else {
            outcome = control.mark_cancelling(reason);
        }
        drop(guard);
        if let Some(mut owner) = owner {
            let owner_control = owner.control.clone();
            let result = match reason {
                CancelReason::Deadline => Err(owner_control.deadline_failure(None)),
                CancelReason::User | CancelReason::Shutdown => {
                    Err(owner_control.cancelled_failure(None))
                }
            };
            owner.send_terminal(ExecutionState::Terminated, result);
        }
        self.wake.notify_all();
        outcome
    }

    fn release_active(&self, id: u64) {
        let mut guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
        guard.active.remove(&id);
        drop(guard);
        self.wake.notify_all();
    }

    fn shutdown(&self, grace: Duration) -> ShutdownReport {
        let (mut queued, mut ready, accepted_not_started_cancelled) = {
            let mut guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
            guard.accepting = false;
            let queued = guard.queue.drain(..).collect::<Vec<_>>();
            let ready = guard.ready.drain(..).collect::<Vec<_>>();
            let mut accepted_not_started_cancelled = ready.len();
            for owner in &ready {
                let _ = owner.control.mark_cancelling(CancelReason::Shutdown);
            }
            // The manager may already have removed an owner from `ready` and
            // be between admission and OS-thread start. Linearize those
            // owners as pre-start cancellation under the scheduler lock, so
            // they do not consume active grace or start user code.
            for control in guard.active.values() {
                if control.state() == ExecutionState::AcceptedNotStarted
                    && matches!(
                        control.mark_cancelling(CancelReason::Shutdown),
                        CancelOutcome::Requested
                    )
                {
                    accepted_not_started_cancelled += 1;
                }
            }
            (queued, ready, accepted_not_started_cancelled)
        };
        let queued_cancelled = queued.len();
        queued.append(&mut ready);
        for mut owner in queued {
            let control = owner.control.clone();
            owner.send_terminal(
                ExecutionState::Terminated,
                Err(control.cancelled_failure(None)),
            );
        }
        self.wake.notify_all();

        let deadline = Instant::now().checked_add(grace);
        loop {
            let mut guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if guard.active.is_empty() {
                return ShutdownReport {
                    queued_cancelled,
                    accepted_not_started_cancelled,
                    active_cancel_requested: 0,
                };
            }
            let Some(deadline) = deadline else {
                // A duration that cannot be represented by `Instant` is an
                // effectively unbounded grace period, not an immediate one.
                let _guard = self
                    .wake
                    .wait(guard)
                    .unwrap_or_else(|error| error.into_inner());
                continue;
            };
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop(guard);
                break;
            }
            let (next, timeout) = self
                .wake
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|error| error.into_inner());
            guard = next;
            if timeout.timed_out() {
                drop(guard);
                break;
            }
        }

        let controls = {
            let guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
            guard
                .active
                .values()
                .filter(|control| control.state() == ExecutionState::Started)
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut active_cancel_requested = 0;
        for control in controls {
            if !Self::terminal(control.state()) {
                let outcome = control.request_cancel(CancelReason::Shutdown);
                if !matches!(
                    outcome,
                    CancelOutcome::AlreadyTerminal | CancelOutcome::AlreadyRequested
                ) {
                    active_cancel_requested += 1;
                }
            }
        }

        // A cancellation request is not backend completion. Started
        // subprocesses and blocking native work remain owned by their task
        // thread, which releases its permit only after terminal completion.
        ShutdownReport {
            queued_cancelled,
            accepted_not_started_cancelled,
            active_cancel_requested,
        }
    }

    fn fail_pending(&self, message: String) {
        let mut pending = Vec::new();
        {
            let mut guard = self.state.lock().unwrap_or_else(|error| error.into_inner());
            pending.extend(guard.queue.drain(..));
            pending.extend(guard.ready.drain(..));
        }
        for mut owner in pending {
            let control = owner.control.clone();
            let timing = owner.timing.clone();
            owner.send_terminal(
                ExecutionState::Failed,
                Err(ExecutionFailure::new(
                    ExecutionError::Internal(message.clone()),
                    control.report(None, timing),
                    None,
                )),
            );
        }
    }

    fn terminal(state: ExecutionState) -> bool {
        TaskControl::is_terminal(state)
    }
}

#[cfg(feature = "execution-control")]
fn next_deadline(state: &SchedulerState) -> Option<Instant> {
    state
        .queue
        .iter()
        .chain(state.ready.iter())
        .filter_map(|owner| owner.deadline)
        .chain(
            state
                .active
                .values()
                .filter(|control| {
                    matches!(
                        control.state(),
                        ExecutionState::AcceptedNotStarted | ExecutionState::Started
                    )
                })
                .filter_map(|control| control.deadline),
        )
        .min()
}

#[cfg(feature = "execution-control")]
fn spawn_task(owner: TaskOwner) {
    let slot = Arc::new(Mutex::new(Some(owner)));
    let thread_slot = slot.clone();
    match std::thread::Builder::new()
        .name("libdeno-execution-task".to_string())
        .spawn(move || {
            if let Some(mut owner) = thread_slot
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                let control = owner.control.clone();
                let timing = owner.timing.clone();
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.run())).is_err() {
                    owner.send_terminal(
                        ExecutionState::Failed,
                        Err(ExecutionFailure::new(
                            ExecutionError::Internal("submission task panicked".to_string()),
                            control.report(None, timing),
                            None,
                        )),
                    );
                }
            }
        }) {
        Ok(_) => {}
        Err(error) => {
            // The owner was admitted before this OS task was attempted. Close
            // its result first, then let its ActivePermit release the slot.
            if let Some(mut owner) = slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let control = owner.control.clone();
                let timing = owner.timing.clone();
                owner.send_terminal(
                    ExecutionState::Failed,
                    Err(ExecutionFailure::new(
                        ExecutionError::Internal(format!(
                            "failed to start execution task: {error}"
                        )),
                        control.report(None, timing),
                        None,
                    )),
                );
            }
        }
    }
}

#[cfg(feature = "execution-control")]
impl ExecutorDispatch {
    #[allow(clippy::too_many_arguments)]
    fn execute_submission(
        &self,
        request: ExecutionRequest,
        _submission: SubmissionOptions,
        deadline: Option<Instant>,
        cancellation: crate::limits::CancellationContext,
        default_cancel_reason: CancelReason,
        timing: ExecutionTiming,
        on_started: Option<&dyn Fn()>,
    ) -> Result<ExecutionResult, ExecutionFailure> {
        #[cfg(test)]
        if self.panic_on_execute {
            panic!("synthetic executor backend panic");
        }
        let started = Instant::now();
        let normalized = {
            let _admission = timing.span(Phase::Admission);
            self.normalize_options(&request)
        };
        let mut options = match normalized {
            Ok(options) => options,
            Err(error) => return Err(self.failure(started, None, error, timing)),
        };

        let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
        if remaining.is_some_and(|timeout| timeout.is_zero()) {
            return Err(self.failure(
                started,
                None,
                LibdenoError::Timeout("execution request deadline exceeded".to_string()),
                timing,
            ));
        }
        options.execution_deadline = min_deadline(options.execution_deadline, remaining);
        let option_deadline = match options.execution_deadline {
            Some(duration) => match started.checked_add(duration) {
                Some(deadline) => Some(deadline),
                None => {
                    return Err(self.failure(
                        started,
                        None,
                        LibdenoError::Configuration(
                            "execution deadline is too large for the host clock".to_string(),
                        ),
                        timing,
                    ))
                }
            },
            None => None,
        };
        let in_process_deadline = if matches!(&self.state, ExecutorBackend::InProcess(_)) {
            min_absolute_deadline(deadline, option_deadline)
        } else {
            None
        };
        if in_process_deadline.is_some_and(|at| at <= Instant::now()) {
            return Err(self.failure(
                started,
                None,
                LibdenoError::Timeout("execution request deadline exceeded".to_string()),
                timing,
            ));
        }
        match &self.state {
            ExecutorBackend::InProcess(runtime) => {
                let result = crate::run_with_output_observed_cancellable_until(
                    runtime,
                    &request.entry,
                    &options,
                    timing.clone(),
                    Some(cancellation),
                    in_process_deadline,
                );
                self.finish(started, result, &options, timing)
            }
            ExecutorBackend::Subprocess(host) => {
                let result = crate::subprocess::run_supervised_subprocess_with_executable_observed_and_started(
                    host,
                    &request.entry,
                    &options,
                    Some(cancellation),
                    supervisor_cancel_reason(default_cancel_reason),
                    deadline,
                    timing.clone(),
                    on_started,
                );
                self.finish_supervised(started, result, &options, timing)
            }
        }
    }

    fn finish_supervised(
        &self,
        started: Instant,
        result: Result<
            crate::supervisor::SupervisorRunResult,
            crate::subprocess::SupervisedSubprocessError,
        >,
        options: &LibdenoOptions,
        timing: ExecutionTiming,
    ) -> Result<ExecutionResult, ExecutionFailure> {
        match result {
            Ok(supervised) => {
                let cleanup_strength = supervisor_cleanup_strength(supervised.cleanup_strength);
                let transport_status = supervisor_transport_status(supervised.transport_status);
                Ok(self.result_with_metadata(
                    started,
                    Some(ExecutionBackend::Subprocess),
                    CapabilityOutcome::Used,
                    supervised.output,
                    options,
                    timing,
                    Some(cleanup_strength),
                    Some(transport_status),
                ))
            }
            Err(error) => {
                let crate::subprocess::SupervisedSubprocessError {
                    error,
                    cleanup_strength,
                    transport_status,
                } = error;
                Err(ExecutionFailure::new(
                    ExecutionError::Libdeno(error),
                    self.report_with_metadata(
                        started,
                        Some(ExecutionBackend::Subprocess),
                        CapabilityOutcome::Failed,
                        timing,
                        cleanup_strength.map(supervisor_cleanup_strength),
                        transport_status.map(supervisor_transport_status),
                    ),
                    None,
                ))
            }
        }
    }

    fn normalize_options(
        &self,
        request: &ExecutionRequest,
    ) -> Result<LibdenoOptions, LibdenoError> {
        let mut options = request.options.clone();
        crate::limits::validate_execution_deadline(options.execution_deadline)?;
        if let Some(cwd) = options.cwd.as_ref() {
            let canonical = std::fs::canonicalize(cwd).map_err(LibdenoError::Io)?;
            if canonical != self.project_dir {
                return Err(LibdenoError::Configuration(format!(
                    "execution request cwd {} does not match executor project directory {}",
                    cwd.display(),
                    self.project_dir.display()
                )));
            }
        }
        options.cwd = Some(self.project_dir.clone());
        Ok(options)
    }

    fn finish(
        &self,
        started: Instant,
        result: Result<RunOutput, LibdenoError>,
        options: &LibdenoOptions,
        timing: ExecutionTiming,
    ) -> Result<ExecutionResult, ExecutionFailure> {
        match result {
            Ok(output) => Ok(self.result_with_metadata(
                started,
                Some(self.backend),
                CapabilityOutcome::Used,
                output,
                options,
                timing,
                None,
                None,
            )),
            Err(error) => Err(self.failure(started, Some(self.backend), error, timing)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn result_with_metadata(
        &self,
        started: Instant,
        dispatched: Option<ExecutionBackend>,
        outcome: CapabilityOutcome,
        output: RunOutput,
        options: &LibdenoOptions,
        timing: ExecutionTiming,
        cleanup_strength: Option<ExecutionCleanupStrength>,
        transport_status: Option<ExecutionTransportStatus>,
    ) -> ExecutionResult {
        ExecutionResult {
            exit_code: output.exit_code,
            output: ExecutionOutput {
                stdout: if options.capture_stdout {
                    output.stdout
                } else {
                    Vec::new()
                },
                stderr: if options.capture_stderr {
                    output.stderr
                } else {
                    Vec::new()
                },
                truncated: output.capture_truncated,
            },
            report: self.report_with_metadata(
                started,
                dispatched,
                outcome,
                timing,
                cleanup_strength,
                transport_status,
            ),
        }
    }

    fn report(
        &self,
        started: Instant,
        dispatched: Option<ExecutionBackend>,
        outcome: CapabilityOutcome,
        timing: ExecutionTiming,
    ) -> ExecutionReport {
        self.report_with_metadata(started, dispatched, outcome, timing, None, None)
    }

    fn report_with_metadata(
        &self,
        started: Instant,
        dispatched: Option<ExecutionBackend>,
        outcome: CapabilityOutcome,
        timing: ExecutionTiming,
        cleanup_strength: Option<ExecutionCleanupStrength>,
        transport_status: Option<ExecutionTransportStatus>,
    ) -> ExecutionReport {
        ExecutionReport {
            requested_backend: self.backend,
            dispatched_backend: dispatched,
            elapsed: started.elapsed(),
            backend_outcome: outcome,
            timing: Box::new(timing.snapshot()),
            cleanup_strength,
            transport_status,
        }
    }

    fn failure(
        &self,
        started: Instant,
        dispatched: Option<ExecutionBackend>,
        error: LibdenoError,
        timing: ExecutionTiming,
    ) -> ExecutionFailure {
        ExecutionFailure::new(
            ExecutionError::Libdeno(error),
            self.report(started, dispatched, CapabilityOutcome::Failed, timing),
            None,
        )
    }
}

#[cfg(feature = "execution-control")]
fn supervisor_cleanup_strength(
    strength: crate::supervisor::CleanupStrength,
) -> ExecutionCleanupStrength {
    match strength {
        crate::supervisor::CleanupStrength::DirectChild => ExecutionCleanupStrength::DirectChild,
        crate::supervisor::CleanupStrength::ProcessGroup => ExecutionCleanupStrength::ProcessGroup,
        crate::supervisor::CleanupStrength::WindowsJob => ExecutionCleanupStrength::WindowsJob,
    }
}

#[cfg(feature = "execution-control")]
fn supervisor_transport_status(
    status: crate::supervisor::SupervisorTransportStatus,
) -> ExecutionTransportStatus {
    match status {
        crate::supervisor::SupervisorTransportStatus::Clean => ExecutionTransportStatus::Clean,
        crate::supervisor::SupervisorTransportStatus::Failed => ExecutionTransportStatus::Failed,
    }
}

#[cfg(feature = "execution-control")]
fn min_deadline(backend: Option<Duration>, request: Option<Duration>) -> Option<Duration> {
    match (backend, request) {
        (Some(backend), Some(request)) => Some(backend.min(request)),
        (Some(backend), None) => Some(backend),
        (None, Some(request)) => Some(request),
        (None, None) => None,
    }
}

#[cfg(feature = "execution-control")]
fn min_absolute_deadline(first: Option<Instant>, second: Option<Instant>) -> Option<Instant> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (Some(first), None) => Some(first),
        (None, Some(second)) => Some(second),
        (None, None) => None,
    }
}

#[derive(Clone)]
enum ExecutorBackend {
    InProcess(LibdenoRuntime),
    Subprocess(PathBuf),
}

/// Immutable execution owner for a fixed project directory and backend.
#[derive(Clone)]
pub struct Executor {
    project_dir: PathBuf,
    backend: ExecutionBackend,
    state: ExecutorBackend,
    #[cfg(feature = "execution-control")]
    admission: Arc<AdmissionScheduler>,
}

impl fmt::Debug for Executor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("Executor");
        debug.field("project_dir", &self.project_dir);
        debug.field("backend", &self.backend);
        #[cfg(feature = "execution-control")]
        debug.field(
            "admission",
            &format_args!(
                "active_limit={}, queue_limit={}",
                self.admission.active_limit, self.admission.queue_limit
            ),
        );
        debug.finish_non_exhaustive()
    }
}

impl Executor {
    /// Starts an executor builder rooted at `project_dir`.
    pub fn builder(project_dir: impl AsRef<Path>) -> ExecutorBuilder {
        ExecutorBuilder {
            project_dir: project_dir.as_ref().to_path_buf(),
            backend: ExecutionBackend::InProcess,
            host_executable: None,
            #[cfg(feature = "execution-control")]
            admission: AdmissionConfig::default(),
        }
    }

    /// Returns the canonical project directory fixed at build time.
    pub fn project_dir(&self) -> &Path {
        &self.project_dir
    }

    /// Returns the configured backend.
    pub fn backend(&self) -> ExecutionBackend {
        self.backend
    }

    /// Returns the backend and sandbox capability availability.
    pub fn capability_report(&self) -> CapabilityReport {
        CapabilityReport::default()
    }

    /// Submits one request to the experimental admission scheduler.
    #[cfg(feature = "execution-control")]
    #[doc(hidden)]
    pub fn submit(
        &self,
        request: ExecutionRequest,
        submission: SubmissionOptions,
    ) -> Result<ExecutionHandle, SubmitError> {
        self.admission.submit(self.dispatch(), request, submission)
    }

    /// Requests scheduler shutdown. Queued work is cancelled immediately;
    /// active work gets the supplied drain grace before best-effort cancel is
    /// requested. The internal permit remains held until terminal completion.
    #[cfg(feature = "execution-control")]
    #[doc(hidden)]
    pub fn shutdown(&self, grace: Duration) -> ShutdownReport {
        self.admission.shutdown(grace)
    }

    #[cfg(feature = "execution-control")]
    fn dispatch(&self) -> ExecutorDispatch {
        ExecutorDispatch {
            project_dir: self.project_dir.clone(),
            backend: self.backend,
            state: self.state.clone(),
            #[cfg(test)]
            panic_on_execute: false,
        }
    }

    /// Executes a request synchronously.
    pub fn execute(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionFailure> {
        self.execute_sync(request)
    }

    /// Executes a request asynchronously.
    ///
    /// In-process execution remains a non-`Send` future because the reusable
    /// runtime's V8 worker is pinned to the polling thread. Subprocess
    /// execution uses `spawn_blocking` around the same synchronous dispatch.
    /// Dropping the returned subprocess future does not cancel the blocking
    /// task or kill a child that it has already spawned.
    pub async fn execute_async(
        &self,
        request: ExecutionRequest,
    ) -> Result<ExecutionResult, ExecutionFailure> {
        let started = Instant::now();
        let requested = self.backend;
        let timing = ExecutionTiming::enabled();
        let normalized_options = {
            let _admission = timing.span(Phase::Admission);
            self.normalize_options(&request)
        };
        let options = match normalized_options {
            Ok(options) => options,
            Err(error) => {
                return Err(self.failure(started, None, CapabilityOutcome::Failed, error, timing))
            }
        };

        if let Err(error) = {
            let _admission = timing.span(Phase::Admission);
            crate::check_async_context()
        } {
            return Err(self.failure(started, None, CapabilityOutcome::Failed, error, timing));
        }

        match &self.state {
            ExecutorBackend::InProcess(runtime) => {
                let dispatched = Some(ExecutionBackend::InProcess);
                match crate::runtime::run_with_output_async_observed(
                    runtime,
                    &request.entry,
                    &options,
                    timing.clone(),
                )
                .await
                {
                    Ok(output) => Ok(self.result(
                        started,
                        dispatched,
                        CapabilityOutcome::Used,
                        output,
                        &options,
                        timing,
                    )),
                    Err(error) => Err(self.failure(
                        started,
                        dispatched,
                        CapabilityOutcome::Failed,
                        error,
                        timing,
                    )),
                }
            }
            ExecutorBackend::Subprocess(_host) => {
                let executor = self.clone();
                let timing_for_task = timing.clone();
                let join = tokio::task::spawn_blocking(move || {
                    executor.execute_normalized_sync(request, options, timing_for_task)
                })
                .await;
                match join {
                    Ok(result) => result,
                    Err(error) => Err(ExecutionFailure::new(
                        ExecutionError::Internal(format!(
                            "subprocess dispatch task failed: {error}"
                        )),
                        ExecutionReport {
                            requested_backend: requested,
                            dispatched_backend: None,
                            elapsed: started.elapsed(),
                            backend_outcome: CapabilityOutcome::Failed,
                            timing: Box::new(timing.snapshot()),
                            cleanup_strength: None,
                            transport_status: None,
                        },
                        // ponytail: the blocking task owns child cleanup; a
                        // dropped join handle cannot safely claim partial data.
                        None,
                    )),
                }
            }
        }
    }

    fn execute_sync(&self, request: ExecutionRequest) -> Result<ExecutionResult, ExecutionFailure> {
        let started = Instant::now();
        let timing = ExecutionTiming::enabled();
        let normalized_options = {
            let _admission = timing.span(Phase::Admission);
            self.normalize_options(&request)
        };
        let options = match normalized_options {
            Ok(options) => options,
            Err(error) => {
                return Err(self.failure(started, None, CapabilityOutcome::Failed, error, timing))
            }
        };
        self.execute_normalized_sync(request, options, timing)
    }

    fn execute_normalized_sync(
        &self,
        request: ExecutionRequest,
        options: LibdenoOptions,
        timing: ExecutionTiming,
    ) -> Result<ExecutionResult, ExecutionFailure> {
        let started = Instant::now();
        match &self.state {
            ExecutorBackend::InProcess(runtime) => {
                let result = crate::runtime::run_with_output_observed(
                    runtime,
                    &request.entry,
                    &options,
                    timing.clone(),
                );
                self.finish_sync_result(started, result, &options, timing)
            }
            ExecutorBackend::Subprocess(host) => {
                if options.capture_stdout || options.capture_stderr {
                    let result = crate::subprocess::run_in_subprocess_with_selective_output_and_executable_observed(
                        host,
                        &request.entry,
                        &options,
                        timing.clone(),
                    );
                    self.finish_sync_result(started, result, &options, timing)
                } else {
                    let result = crate::subprocess::run_in_subprocess_with_executable_observed(
                        host,
                        &request.entry,
                        &options,
                        timing.clone(),
                    );
                    match result {
                        Ok(exit_code) => Ok(ExecutionResult {
                            exit_code,
                            output: ExecutionOutput::default(),
                            report: self.report(
                                started,
                                Some(ExecutionBackend::Subprocess),
                                CapabilityOutcome::Used,
                                timing,
                            ),
                        }),
                        Err(error) => Err(self.failure(
                            started,
                            Some(ExecutionBackend::Subprocess),
                            CapabilityOutcome::Failed,
                            error,
                            timing,
                        )),
                    }
                }
            }
        }
    }

    fn finish_sync_result(
        &self,
        started: Instant,
        result: Result<RunOutput, LibdenoError>,
        options: &LibdenoOptions,
        timing: ExecutionTiming,
    ) -> Result<ExecutionResult, ExecutionFailure> {
        match result {
            Ok(output) => Ok(self.result(
                started,
                Some(self.backend),
                CapabilityOutcome::Used,
                output,
                options,
                timing,
            )),
            Err(error) => Err(self.failure(
                started,
                Some(self.backend),
                CapabilityOutcome::Failed,
                error,
                timing,
            )),
        }
    }

    fn result(
        &self,
        started: Instant,
        dispatched: Option<ExecutionBackend>,
        outcome: CapabilityOutcome,
        output: RunOutput,
        options: &LibdenoOptions,
        timing: ExecutionTiming,
    ) -> ExecutionResult {
        ExecutionResult {
            exit_code: output.exit_code,
            output: ExecutionOutput {
                stdout: if options.capture_stdout {
                    output.stdout
                } else {
                    Vec::new()
                },
                stderr: if options.capture_stderr {
                    output.stderr
                } else {
                    Vec::new()
                },
                truncated: output.capture_truncated,
            },
            report: self.report(started, dispatched, outcome, timing),
        }
    }

    fn normalize_options(
        &self,
        request: &ExecutionRequest,
    ) -> Result<LibdenoOptions, LibdenoError> {
        let mut options = request.options.clone();
        if let Some(cwd) = options.cwd.as_ref() {
            let canonical = std::fs::canonicalize(cwd).map_err(LibdenoError::Io)?;
            if canonical != self.project_dir {
                return Err(LibdenoError::Configuration(format!(
                    "execution request cwd {} does not match executor project directory {}",
                    cwd.display(),
                    self.project_dir.display()
                )));
            }
        }
        // The fixed absolute path is used for both the resolver base and the
        // subprocess child cwd. In-process execution still leaves the host cwd
        // and Deno.cwd() unchanged; runtime cwd is a resolver base only.
        options.cwd = Some(self.project_dir.clone());
        Ok(options)
    }

    fn report(
        &self,
        started: Instant,
        dispatched: Option<ExecutionBackend>,
        backend_outcome: CapabilityOutcome,
        timing: ExecutionTiming,
    ) -> ExecutionReport {
        ExecutionReport {
            requested_backend: self.backend,
            dispatched_backend: dispatched,
            elapsed: started.elapsed(),
            backend_outcome,
            timing: Box::new(timing.snapshot()),
            cleanup_strength: None,
            transport_status: None,
        }
    }

    fn failure(
        &self,
        started: Instant,
        dispatched: Option<ExecutionBackend>,
        outcome: CapabilityOutcome,
        error: LibdenoError,
        timing: ExecutionTiming,
    ) -> ExecutionFailure {
        ExecutionFailure::new(
            ExecutionError::Libdeno(error),
            self.report(started, dispatched, outcome, timing),
            // ponytail: Phase 2B legacy capture paths return no error-time
            // partial buffers, so None is the only truthful result.
            None,
        )
    }
}

/// Builder for an [`Executor`].
#[derive(Debug, Clone)]
pub struct ExecutorBuilder {
    project_dir: PathBuf,
    backend: ExecutionBackend,
    host_executable: Option<PathBuf>,
    #[cfg(feature = "execution-control")]
    admission: AdmissionConfig,
}

impl ExecutorBuilder {
    /// Selects the backend used by the executor.
    pub fn backend(mut self, backend: ExecutionBackend) -> Self {
        self.backend = backend;
        self
    }

    /// Fixes the subprocess host executable at build time.
    pub fn host_executable(mut self, executable: impl AsRef<Path>) -> Self {
        self.host_executable = Some(executable.as_ref().to_path_buf());
        self
    }

    /// Fixes the experimental admission limits at build time.
    #[cfg(feature = "execution-control")]
    #[doc(hidden)]
    pub fn admission(mut self, config: AdmissionConfig) -> Self {
        self.admission = config;
        self
    }

    /// Returns the capabilities available to this builder.
    pub fn capability_report(&self) -> CapabilityReport {
        CapabilityReport::default()
    }

    /// Builds the executor and, for the in-process backend, its one reusable
    /// resolver runtime.
    pub async fn build(self) -> Result<Executor, ExecutionError> {
        if self.backend == ExecutionBackend::ProcessPool {
            return Err(ExecutionError::Unsupported(UnsupportedCapability::new(
                ExecutionCapability::Backend(ExecutionBackend::ProcessPool),
                "process pool backend is not available",
            )));
        }

        let project_dir = std::fs::canonicalize(&self.project_dir).map_err(LibdenoError::Io)?;
        let state = match self.backend {
            ExecutionBackend::InProcess => {
                ExecutorBackend::InProcess(LibdenoRuntime::new(&project_dir).await?)
            }
            ExecutionBackend::Subprocess => {
                let host = match self.host_executable {
                    Some(host) if host.is_absolute() => host,
                    Some(host) => std::env::current_dir()
                        .map_err(LibdenoError::Io)
                        .map(|cwd| cwd.join(host))?,
                    None => std::env::current_exe().map_err(LibdenoError::Io)?,
                };
                ExecutorBackend::Subprocess(host)
            }
            ExecutionBackend::ProcessPool => unreachable!("process-pool is rejected above"),
        };

        Ok(Executor {
            project_dir,
            backend: self.backend,
            state,
            #[cfg(feature = "execution-control")]
            admission: AdmissionScheduler::new(self.admission),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_observation_is_empty_and_secret_free() {
        let timing = ExecutionTiming::disabled();
        assert_eq!(timing.snapshot(), PhaseTiming::default());

        let report = ExecutionReport {
            requested_backend: ExecutionBackend::InProcess,
            dispatched_backend: Some(ExecutionBackend::InProcess),
            elapsed: Duration::from_nanos(1),
            backend_outcome: CapabilityOutcome::Used,
            timing: Box::new(ExecutionTiming::enabled().snapshot()),
            cleanup_strength: None,
            transport_status: None,
        };
        let debug = format!("{report:?}");
        for forbidden in [
            "user:password@registry.example",
            "Authorization",
            "_authToken",
            "npmrc-secret",
            "NpmProcessState",
            "source text",
            "--allow-read=/private",
        ] {
            assert!(!debug.contains(forbidden), "report leaked {forbidden}");
        }
        assert_eq!(report.phase_timing().queue_wait, Duration::ZERO);
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn task_control_linearizes_backend_and_cancellation_and_preserves_reason() {
        fn control(
            scheduler: &Arc<AdmissionScheduler>,
            id: u64,
            backend: ExecutionBackend,
        ) -> Arc<TaskControl> {
            Arc::new(TaskControl::new(
                id,
                backend,
                Instant::now(),
                None,
                ExecutionTiming::disabled(),
                Arc::downgrade(scheduler),
            ))
        }

        fn result() -> ExecutionResult {
            ExecutionResult {
                exit_code: 0,
                output: ExecutionOutput::default(),
                report: ExecutionReport {
                    requested_backend: ExecutionBackend::InProcess,
                    dispatched_backend: Some(ExecutionBackend::InProcess),
                    elapsed: Duration::ZERO,
                    backend_outcome: CapabilityOutcome::Used,
                    timing: Box::new(PhaseTiming::default()),
                    cleanup_strength: None,
                    transport_status: None,
                },
            }
        }

        let scheduler =
            AdmissionScheduler::new(AdmissionConfig::new(NonZeroUsize::new(1).unwrap(), 0));

        let backend_first = control(&scheduler, 1, ExecutionBackend::InProcess);
        backend_first.mark_accepted();
        assert!(backend_first.begin_start());
        let resolved = backend_first
            .resolve_backend_result(Ok(result()), ExecutionBackend::InProcess)
            .expect("backend must publish the first terminal result");
        assert_eq!(resolved.0, ExecutionState::Completed);
        assert_eq!(backend_first.state(), ExecutionState::Completed);
        assert_eq!(
            backend_first.request_cancel(CancelReason::User),
            CancelOutcome::AlreadyTerminal
        );

        let cancellation_first = control(&scheduler, 2, ExecutionBackend::InProcess);
        cancellation_first.mark_accepted();
        assert!(cancellation_first.begin_start());
        assert_eq!(
            cancellation_first.request_cancel(CancelReason::Deadline),
            CancelOutcome::Requested
        );
        assert_eq!(
            cancellation_first.cancellation.reason(),
            Some(crate::limits::CancellationReason::Deadline)
        );
        assert_eq!(
            cancellation_first.request_cancel(CancelReason::User),
            CancelOutcome::AlreadyRequested
        );
        assert!(!cancellation_first.mark_started());
        let resolved = cancellation_first
            .resolve_backend_result(Ok(result()), ExecutionBackend::InProcess)
            .expect("cancellation must publish the first terminal result");
        assert_eq!(resolved.0, ExecutionState::Terminated);
        assert!(matches!(
            resolved.1.unwrap_err().error(),
            ExecutionError::Libdeno(LibdenoError::Timeout(_))
        ));
        assert_eq!(cancellation_first.state(), ExecutionState::Terminated);

        let prestart = control(&scheduler, 3, ExecutionBackend::Subprocess);
        prestart.mark_accepted();
        assert!(prestart.begin_start());
        assert_eq!(prestart.state(), ExecutionState::AcceptedNotStarted);
        assert_eq!(
            prestart.request_cancel(CancelReason::User),
            CancelOutcome::Requested
        );
        assert!(!prestart.mark_started());
        assert_eq!(prestart.state(), ExecutionState::Cancelling);

        let started = control(&scheduler, 4, ExecutionBackend::Subprocess);
        started.mark_accepted();
        assert!(started.begin_start());
        assert!(started.mark_started());
        assert_eq!(started.state(), ExecutionState::Started);
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn cancelling_active_work_is_not_a_scheduler_deadline() {
        let scheduler =
            AdmissionScheduler::new(AdmissionConfig::new(NonZeroUsize::new(1).unwrap(), 0));
        let control = Arc::new(TaskControl::new(
            1,
            ExecutionBackend::InProcess,
            Instant::now(),
            Some(Instant::now() + Duration::from_secs(60)),
            ExecutionTiming::disabled(),
            Arc::downgrade(&scheduler),
        ));
        control.mark_accepted();
        assert!(control.begin_start());
        assert_eq!(
            control.request_cancel(CancelReason::User),
            CancelOutcome::Requested
        );

        let mut active = HashMap::new();
        active.insert(control.id, control);
        let state = SchedulerState {
            accepting: true,
            queue: VecDeque::new(),
            ready: VecDeque::new(),
            active,
            next_id: 2,
        };
        assert_eq!(next_deadline(&state), None);
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn idle_manager_releases_start_flag_for_future_submissions() {
        let scheduler =
            AdmissionScheduler::new(AdmissionConfig::new(NonZeroUsize::new(1).unwrap(), 0));
        scheduler.manager_started.store(true, Ordering::Release);
        let manager = scheduler.clone();
        let join = std::thread::spawn(move || manager.manager_loop());
        let deadline = Instant::now() + Duration::from_secs(1);
        while scheduler.manager_started.load(Ordering::Acquire) {
            assert!(
                Instant::now() < deadline,
                "idle manager did not release its flag"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        join.join().unwrap();
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn overflowing_shutdown_grace_waits_for_active_release() {
        let scheduler =
            AdmissionScheduler::new(AdmissionConfig::new(NonZeroUsize::new(1).unwrap(), 0));
        let control = Arc::new(TaskControl::new(
            1,
            ExecutionBackend::InProcess,
            Instant::now(),
            None,
            ExecutionTiming::disabled(),
            Arc::downgrade(&scheduler),
        ));
        control.mark_accepted();
        assert!(control.begin_start());
        scheduler
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active
            .insert(control.id, control);

        let release = scheduler.clone();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(40));
            release.release_active(1);
        });
        let started = Instant::now();
        let report = scheduler.shutdown(Duration::MAX);
        assert!(started.elapsed() >= Duration::from_millis(20));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(report.active_cancel_requested(), 0);
        releaser.join().unwrap();
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn panicking_task_releases_active_permit_after_scheduler_poison() {
        let scheduler =
            AdmissionScheduler::new(AdmissionConfig::new(NonZeroUsize::new(1).unwrap(), 0));
        let timing = ExecutionTiming::enabled();
        let control = Arc::new(TaskControl::new(
            1,
            ExecutionBackend::InProcess,
            Instant::now(),
            None,
            timing.clone(),
            Arc::downgrade(&scheduler),
        ));
        control.mark_accepted();
        let result = ResultCell::new();

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = scheduler.state.lock().unwrap();
            panic!("synthetic scheduler poison");
        }));
        assert!(poisoned.is_err());

        scheduler
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active
            .insert(control.id, control.clone());
        let owner = TaskOwner {
            id: control.id,
            request: ExecutionRequest::new(".", LibdenoOptions::default()),
            submission: SubmissionOptions::default(),
            deadline: None,
            control: control.clone(),
            runner: ExecutorDispatch {
                project_dir: PathBuf::new(),
                backend: ExecutionBackend::InProcess,
                state: ExecutorBackend::Subprocess(PathBuf::new()),
                panic_on_execute: true,
            },
            timing,
            result: result.clone(),
            permit: Some(TaskPermit::Active(ActivePermit::new(
                scheduler.clone(),
                control.id,
            ))),
        };

        spawn_task(owner);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let failure = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(5), result.wait())
                .await
                .expect("panicking task did not publish a terminal result")
                .unwrap_err()
        });
        assert!(
            matches!(failure.error(), ExecutionError::Internal(message) if message.contains("panicked"))
        );
        assert_eq!(control.state(), ExecutionState::Failed);
        assert!(scheduler
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active
            .is_empty());
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn queued_submission_records_wait_after_active_blocker() {
        let scheduler =
            AdmissionScheduler::new(AdmissionConfig::new(NonZeroUsize::new(1).unwrap(), 1));
        let blocker = Arc::new(TaskControl::new(
            1,
            ExecutionBackend::InProcess,
            Instant::now(),
            None,
            ExecutionTiming::disabled(),
            Arc::downgrade(&scheduler),
        ));
        blocker.mark_accepted();
        scheduler
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .active
            .insert(blocker.id, blocker);
        let blocker_permit = ActivePermit::new(scheduler.clone(), 1);

        let handle = scheduler
            .submit(
                ExecutorDispatch {
                    project_dir: PathBuf::new(),
                    backend: ExecutionBackend::Subprocess,
                    state: ExecutorBackend::Subprocess(PathBuf::new()),
                    panic_on_execute: true,
                },
                ExecutionRequest::new(".", LibdenoOptions::default()),
                SubmissionOptions::default(),
            )
            .unwrap();
        let queued_id = handle.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let queued = scheduler
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .queue
                .iter()
                .any(|owner| owner.id == queued_id);
            if queued {
                break;
            }
            assert!(Instant::now() < deadline, "submission was not queued");
            std::thread::yield_now();
        }
        drop(blocker_permit);

        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), handle.result())
                    .await
                    .expect("queued submission did not reach a terminal state")
            });
        let failure = result.unwrap_err();
        assert!(matches!(
            failure.error(),
            ExecutionError::Internal(message) if message.contains("panicked")
        ));
        assert!(failure.report().phase_timing().queue_wait > Duration::ZERO);
    }

    #[cfg(feature = "execution-control")]
    #[test]
    fn shared_failed_results_keep_typed_error_and_partial_output() {
        use std::error::Error;

        let output = ExecutionOutput {
            stdout: b"partial".to_vec(),
            stderr: Vec::new(),
            truncated: true,
        };
        let report = ExecutionReport {
            requested_backend: ExecutionBackend::InProcess,
            dispatched_backend: Some(ExecutionBackend::InProcess),
            elapsed: Duration::from_nanos(1),
            backend_outcome: CapabilityOutcome::Failed,
            timing: Box::new(ExecutionTiming::enabled().snapshot()),
            cleanup_strength: None,
            transport_status: None,
        };
        let cell = ResultCell::new();
        cell.publish(Err(ExecutionFailure::new(
            ExecutionError::Libdeno(LibdenoError::Configuration("shared failure".to_string())),
            report,
            Some(output.clone()),
        )));

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (first, second) = runtime.block_on(async { tokio::join!(cell.wait(), cell.wait()) });
        for failure in [first.unwrap_err(), second.unwrap_err()] {
            let (error, _, partial_output) = failure.into_parts();
            assert!(matches!(
                error,
                ExecutionError::Libdeno(LibdenoError::Configuration(ref message))
                    if message == "shared failure"
            ));
            assert!((&error as &dyn Error)
                .source()
                .and_then(|source| source.downcast_ref::<LibdenoError>())
                .is_some());
            assert_eq!(partial_output, Some(output.clone()));
        }
    }

    #[cfg(feature = "phase-diagnostics")]
    #[test]
    fn phase_diagnostics_snapshot_is_readable_and_sanitized() {
        let timing = ExecutionTiming::enabled();
        {
            let _graph_build = timing.span(Phase::GraphBuild);
        }
        let report = ExecutionReport {
            requested_backend: ExecutionBackend::Subprocess,
            dispatched_backend: Some(ExecutionBackend::Subprocess),
            elapsed: Duration::from_nanos(1),
            backend_outcome: CapabilityOutcome::Used,
            timing: Box::new(timing.snapshot()),
            cleanup_strength: None,
            transport_status: None,
        };

        let snapshot = report.phase_diagnostics();
        assert!(snapshot.graph_build_ns.is_some());
        assert_eq!(snapshot.queue_wait_ns, 0);
        let output = format!("{snapshot:?}\n{report:?}");
        for forbidden in [
            "user:password@registry.example",
            "Authorization",
            "_authToken",
            "npmrc-secret",
            "NpmProcessState",
            "source text",
            "--allow-read=/private",
        ] {
            assert!(
                !output.contains(forbidden),
                "diagnostics leaked {forbidden}"
            );
        }
    }
}
