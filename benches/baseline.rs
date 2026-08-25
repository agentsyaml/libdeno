//! Small, dependency-free performance diagnostics for the public execution paths.
//!
//! This is a diagnostic tool, not a CI performance gate.  Its output is TSV so
//! callers can consume it without parsing Cargo's benchmark harness output.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const CHILD_ENV: &str = "LIBDENO_BENCH_CHILD";
const FIXTURE_ENV: &str = "LIBDENO_BENCH_FIXTURE";
const ALL_SCENARIOS: &[&str] = &[
    "first_run_in_process",
    "warm_run",
    "cold_process_run",
    "run_async",
    "four_way_parallel",
    "runtime_reuse",
    "runtime_construct",
    "fresh_runtime_run",
    "runtime_reuse_async",
    "large_inprocess_phase",
    "large_subprocess_capture_off",
    "large_subprocess_capture_on",
    "large_subprocess_parallel_1",
    "large_subprocess_parallel_4",
    "large_subprocess_parallel_n",
];

const LARGE_MODULE_COUNT: usize = 64;

struct Fixture {
    root: PathBuf,
    entry: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("libdeno-baseline-{}-{stamp}", std::process::id()));
        fs::create_dir(&root).expect("create benchmark fixture directory");

        let fixture = Self {
            entry: root.join("main.js"),
            root,
        };
        // Keep the fixture silent: stdout belongs exclusively to the TSV rows.
        fs::write(
            fixture.root.join("dependency.js"),
            "export const fixtureValue = 42;\n",
        )
        .expect("write benchmark dependency fixture");
        fs::write(
            &fixture.entry,
            "import { fixtureValue } from './dependency.js';\n\
             if (fixtureValue !== 42) throw new Error('fixture mismatch');\n",
        )
        .expect("write benchmark entry fixture");
        fixture
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.root) {
            eprintln!(
                "benchmark fixture cleanup failed ({}): {error}",
                self.root.display()
            );
        }
    }
}

struct LargeFixture {
    root: PathBuf,
    entry: PathBuf,
}

impl LargeFixture {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "libdeno-baseline-large-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(root.join("node_modules/fixture-cjs")).expect("create large fixture");

        for index in 0..LARGE_MODULE_COUNT {
            let source = match index + 1 < LARGE_MODULE_COUNT {
                true => format!(
                    "import {{ value as next }} from './module_{:03}.ts';\n\
                     export const value = next + 1;\n",
                    index + 1
                ),
                false => "export const value = 1;\n".to_string(),
            };
            fs::write(root.join(format!("module_{index:03}.ts")), source)
                .expect("write large TypeScript module");
        }
        fs::write(
            root.join("main.ts"),
            format!(
                "import {{ value }} from './module_000.ts';\n\
                 import './node_modules/fixture-cjs/index.cjs';\n\
                 if (value !== {LARGE_MODULE_COUNT}) throw new Error('large fixture mismatch');\n"
            ),
        )
        .expect("write large fixture entry");
        fs::write(
            root.join("package.json"),
            r#"{"name":"libdeno-large-fixture","private":true,"dependencies":{"fixture-cjs":"1.0.0"}}"#,
        )
        .expect("write large fixture package manifest");
        fs::write(
            root.join("node_modules/fixture-cjs/package.json"),
            r#"{"name":"fixture-cjs","version":"1.0.0","main":"index.cjs","type":"commonjs"}"#,
        )
        .expect("write deterministic CJS package manifest");
        fs::write(
            root.join("node_modules/fixture-cjs/index.cjs"),
            "module.exports = require('./leaf.cjs') + 1;\n",
        )
        .expect("write deterministic CJS entry");
        fs::write(
            root.join("node_modules/fixture-cjs/leaf.cjs"),
            "module.exports = 41;\n",
        )
        .expect("write deterministic CJS dependency");

        Self {
            entry: root.join("main.ts"),
            root,
        }
    }
}

impl Drop for LargeFixture {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.root) {
            eprintln!(
                "large benchmark fixture cleanup failed ({}): {error}",
                self.root.display()
            );
        }
    }
}

#[derive(Clone, Copy, Default)]
struct ResourceSample {
    threads: Option<u64>,
    fds: Option<u64>,
    rss_bytes: Option<u64>,
}

fn resource_sample() -> ResourceSample {
    ResourceSample {
        threads: benchmark_thread_count(),
        fds: benchmark_fd_count(),
        rss_bytes: benchmark_rss_bytes(),
    }
}

#[cfg(target_os = "linux")]
fn benchmark_thread_count() -> Option<u64> {
    let text = fs::read_to_string("/proc/self/status").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("Threads:")?.trim().parse().ok())
}

#[cfg(target_os = "macos")]
fn benchmark_thread_count() -> Option<u64> {
    let output = Command::new("ps")
        .args(["-M", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    output.status.success().then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .skip(1)
            .count() as u64
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn benchmark_thread_count() -> Option<u64> {
    None
}

#[cfg(unix)]
fn benchmark_fd_count() -> Option<u64> {
    let directory = if cfg!(target_os = "linux") {
        "/proc/self/fd"
    } else {
        "/dev/fd"
    };
    Some(fs::read_dir(directory).ok()?.count() as u64)
}

#[cfg(not(unix))]
fn benchmark_fd_count() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn benchmark_rss_bytes() -> Option<u64> {
    let text = fs::read_to_string("/proc/self/status").ok()?;
    let kilobytes: u64 = text
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:")?.split_whitespace().next())?
        .parse()
        .ok()?;
    kilobytes.checked_mul(1024)
}

#[cfg(target_os = "macos")]
fn benchmark_rss_bytes() -> Option<u64> {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    let usage = unsafe { usage.assume_init() };
    u64::try_from(usage.ru_maxrss).ok()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn benchmark_rss_bytes() -> Option<u64> {
    None
}

fn resource_value(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "NA".to_string())
}

#[derive(Clone, Copy, Default)]
struct PhaseFields {
    admission_ns: Option<u128>,
    queue_wait_ns: Option<u128>,
    resolver_manifest_probe_ns: Option<u128>,
    resolver_reuse_ns: Option<u128>,
    resolver_rebuild_ns: Option<u128>,
    permission_runtime_services_ns: Option<u128>,
    graph_build_ns: Option<u128>,
    main_worker_bootstrap_ns: Option<u128>,
    user_execution_ns: Option<u128>,
    output_drain_ns: Option<u128>,
    cancel_kill_reap_ns: Option<u128>,
    parent_threads_before: Option<u64>,
    parent_threads_after: Option<u64>,
    parent_fds_before: Option<u64>,
    parent_fds_after: Option<u64>,
    parent_rss_bytes_before: Option<u64>,
    parent_rss_bytes_after: Option<u64>,
}

impl PhaseFields {
    fn add_worker(&mut self, worker: Self) {
        self.admission_ns = sum_phase(self.admission_ns, worker.admission_ns);
        self.queue_wait_ns = sum_phase(self.queue_wait_ns, worker.queue_wait_ns);
        self.resolver_manifest_probe_ns = sum_phase(
            self.resolver_manifest_probe_ns,
            worker.resolver_manifest_probe_ns,
        );
        self.resolver_reuse_ns = sum_phase(self.resolver_reuse_ns, worker.resolver_reuse_ns);
        self.resolver_rebuild_ns = sum_phase(self.resolver_rebuild_ns, worker.resolver_rebuild_ns);
        self.permission_runtime_services_ns = sum_phase(
            self.permission_runtime_services_ns,
            worker.permission_runtime_services_ns,
        );
        self.graph_build_ns = sum_phase(self.graph_build_ns, worker.graph_build_ns);
        self.main_worker_bootstrap_ns = sum_phase(
            self.main_worker_bootstrap_ns,
            worker.main_worker_bootstrap_ns,
        );
        self.user_execution_ns = sum_phase(self.user_execution_ns, worker.user_execution_ns);
        self.output_drain_ns = sum_phase(self.output_drain_ns, worker.output_drain_ns);
        self.cancel_kill_reap_ns = sum_phase(self.cancel_kill_reap_ns, worker.cancel_kill_reap_ns);
        // Parallel phase rows aggregate durations; one worker's resource
        // snapshot must not be presented as the whole parallel round.
        self.parent_threads_before = None;
        self.parent_threads_after = None;
        self.parent_fds_before = None;
        self.parent_fds_after = None;
        self.parent_rss_bytes_before = None;
        self.parent_rss_bytes_after = None;
    }
}

fn sum_phase(left: Option<u128>, right: Option<u128>) -> Option<u128> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn phase_value(value: Option<u128>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "NA".to_string())
}

#[cfg(feature = "phase-diagnostics")]
fn phase_fields(report: &libdeno::ExecutionReport) -> PhaseFields {
    let snapshot = report.phase_diagnostics();
    PhaseFields {
        admission_ns: Some(snapshot.admission_ns),
        queue_wait_ns: Some(snapshot.queue_wait_ns),
        resolver_manifest_probe_ns: snapshot.resolver_manifest_probe_ns,
        resolver_reuse_ns: snapshot.resolver_reuse_ns,
        resolver_rebuild_ns: snapshot.resolver_rebuild_ns,
        permission_runtime_services_ns: snapshot.permission_runtime_services_ns,
        graph_build_ns: snapshot.graph_build_ns,
        main_worker_bootstrap_ns: snapshot.main_worker_bootstrap_ns,
        user_execution_ns: snapshot.user_execution_ns,
        output_drain_ns: snapshot.output_drain_ns,
        cancel_kill_reap_ns: snapshot.cancel_kill_reap_ns,
        parent_threads_before: snapshot.parent_threads_before,
        parent_threads_after: snapshot.parent_threads_after,
        parent_fds_before: snapshot.parent_fds_before,
        parent_fds_after: snapshot.parent_fds_after,
        parent_rss_bytes_before: snapshot.parent_rss_bytes_before,
        parent_rss_bytes_after: snapshot.parent_rss_bytes_after,
    }
}

#[cfg(not(feature = "phase-diagnostics"))]
fn phase_fields(_report: &libdeno::ExecutionReport) -> PhaseFields {
    PhaseFields::default()
}

fn options(root: &Path) -> libdeno::LibdenoOptions {
    libdeno::LibdenoOptions {
        allow_all_permissions: true,
        cwd: Some(root.to_path_buf()),
        ..Default::default()
    }
}

fn iterations() -> usize {
    let value = std::env::var("LIBDENO_BENCH_ITERS").unwrap_or_else(|_| "3".to_string());
    let parsed = value.parse::<usize>().unwrap_or_else(|_| {
        panic!("LIBDENO_BENCH_ITERS must be a positive integer, got {value:?}")
    });
    if parsed == 0 {
        panic!("LIBDENO_BENCH_ITERS must be a positive integer");
    }
    parsed
}

fn selected_scenarios() -> Vec<&'static str> {
    let Some(value) = std::env::var_os("LIBDENO_BENCH_SCENARIOS") else {
        return ALL_SCENARIOS.to_vec();
    };
    let requested: Vec<&str> = value
        .to_str()
        .expect("LIBDENO_BENCH_SCENARIOS must be UTF-8")
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect();
    if requested.is_empty() {
        panic!("LIBDENO_BENCH_SCENARIOS must name at least one scenario");
    }
    for name in &requested {
        if !ALL_SCENARIOS.contains(name) {
            panic!("unknown benchmark scenario {name:?}");
        }
    }
    ALL_SCENARIOS
        .iter()
        .copied()
        .filter(|name| requested.contains(name))
        .collect()
}

fn row(scenario: &str, iteration: usize, elapsed_ns: u128, status: &str) {
    row_with_resources_and_phase(
        scenario,
        iteration,
        elapsed_ns,
        status,
        ResourceSample::default(),
        ResourceSample::default(),
        PhaseFields::default(),
    );
}

fn row_with_resources(
    scenario: &str,
    iteration: usize,
    elapsed_ns: u128,
    status: &str,
    before: ResourceSample,
    after: ResourceSample,
) {
    row_with_resources_and_phase(
        scenario,
        iteration,
        elapsed_ns,
        status,
        before,
        after,
        PhaseFields::default(),
    );
}

fn row_with_resources_and_phase(
    scenario: &str,
    iteration: usize,
    elapsed_ns: u128,
    status: &str,
    before: ResourceSample,
    after: ResourceSample,
    phase: PhaseFields,
) {
    let mut stdout = io::stdout().lock();
    let fields = [
        resource_value(before.threads),
        resource_value(after.threads),
        resource_value(before.fds),
        resource_value(after.fds),
        resource_value(before.rss_bytes),
        resource_value(after.rss_bytes),
        phase_value(phase.admission_ns),
        phase_value(phase.queue_wait_ns),
        phase_value(phase.resolver_manifest_probe_ns),
        phase_value(phase.resolver_reuse_ns),
        phase_value(phase.resolver_rebuild_ns),
        phase_value(phase.permission_runtime_services_ns),
        phase_value(phase.graph_build_ns),
        phase_value(phase.main_worker_bootstrap_ns),
        phase_value(phase.user_execution_ns),
        phase_value(phase.output_drain_ns),
        phase_value(phase.cancel_kill_reap_ns),
        resource_value(phase.parent_threads_before),
        resource_value(phase.parent_threads_after),
        resource_value(phase.parent_fds_before),
        resource_value(phase.parent_fds_after),
        resource_value(phase.parent_rss_bytes_before),
        resource_value(phase.parent_rss_bytes_after),
    ];
    writeln!(
        stdout,
        "{scenario}\t{iteration}\t{elapsed_ns}\t{status}\t{}",
        fields.join("\t"),
    )
    .expect("write benchmark TSV row");
}

fn record(scenario: &str, iteration: usize, start: Instant, result: Result<(), String>) {
    record_with_phase(
        scenario,
        iteration,
        start,
        result.map(|()| PhaseFields::default()),
    );
}

fn record_with_phase(
    scenario: &str,
    iteration: usize,
    start: Instant,
    result: Result<PhaseFields, String>,
) {
    let elapsed_ns = start.elapsed().as_nanos();
    match result {
        Ok(phase) => row_with_resources_and_phase(
            scenario,
            iteration,
            elapsed_ns,
            "ok",
            ResourceSample::default(),
            ResourceSample::default(),
            phase,
        ),
        Err(error) => {
            row(scenario, iteration, elapsed_ns, "error");
            panic!("{scenario} iteration {iteration} failed: {error}");
        }
    }
}

fn record_with_resources_and_phase(
    scenario: &str,
    iteration: usize,
    start: Instant,
    before: ResourceSample,
    result: Result<PhaseFields, String>,
) {
    let elapsed_ns = start.elapsed().as_nanos();
    let after = resource_sample();
    match result {
        Ok(phase) => row_with_resources_and_phase(
            scenario, iteration, elapsed_ns, "ok", before, after, phase,
        ),
        Err(error) => {
            row_with_resources(scenario, iteration, elapsed_ns, "error", before, after);
            panic!("{scenario} iteration {iteration} failed: {error}");
        }
    }
}

fn check_run(result: Result<i32, libdeno::LibdenoError>) -> Result<(), String> {
    match result {
        Ok(0) => Ok(()),
        Ok(code) => Err(format!("libdeno::run returned exit code {code}")),
        Err(error) => Err(error.to_string()),
    }
}

fn timed_run(
    scenario: &str,
    iteration: usize,
    fixture: &Fixture,
    options: &libdeno::LibdenoOptions,
) {
    let start = Instant::now();
    let result = std::hint::black_box(libdeno::run(&fixture.entry, options));
    record(scenario, iteration, start, check_run(result));
}

fn first_run_in_process(iters: usize, fixture: &Fixture, options: &libdeno::LibdenoOptions) {
    // No libdeno run happens before this scenario.  Iteration 0 is the one
    // process-first call; later rows are same-process follow-ups because a
    // process has only one first run.  Use iters=1 for a single first-run row.
    for iteration in 0..iters {
        timed_run("first_run_in_process", iteration, fixture, options);
    }
}

fn warm_run(iters: usize, fixture: &Fixture, options: &libdeno::LibdenoOptions) {
    // Keep this scenario meaningful when selected by itself as well as after
    // first_run_in_process: the measured calls always follow one same-process
    // warm-up run.
    check_run(std::hint::black_box(libdeno::run(&fixture.entry, options)))
        .unwrap_or_else(|error| panic!("warm_run warm-up failed: {error}"));
    for iteration in 0..iters {
        timed_run("warm_run", iteration, fixture, options);
    }
}

fn cold_process_run(iters: usize, fixture: &Fixture) {
    let executable = std::env::current_exe().expect("resolve benchmark executable");
    for iteration in 0..iters {
        let start = Instant::now();
        let result = Command::new(&executable)
            .env(CHILD_ENV, "1")
            .env(FIXTURE_ENV, &fixture.root)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| error.to_string())
            .and_then(|status| {
                if status.success() {
                    Ok(())
                } else {
                    Err(format!("child exited with status {status}"))
                }
            });
        // The timer intentionally covers process spawn, the child's first
        // libdeno::run, and waiting for that child to finish.
        record("cold_process_run", iteration, start, result);
    }
}

fn run_async(iters: usize, fixture: &Fixture, options: &libdeno::LibdenoOptions) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build current-thread Tokio runtime");
    // block_on calls are deliberately sequential: run_async futures must not
    // be interleaved on their V8-pinned thread.
    for iteration in 0..iters {
        let start = Instant::now();
        let result =
            std::hint::black_box(runtime.block_on(libdeno::run_async(&fixture.entry, options)));
        record("run_async", iteration, start, check_run(result));
    }
}

fn four_way_parallel(iters: usize, fixture: &Fixture, options: &libdeno::LibdenoOptions) {
    for iteration in 0..iters {
        let barrier = Arc::new(Barrier::new(5));
        let mut workers = Vec::with_capacity(4);
        for _ in 0..4 {
            let barrier = Arc::clone(&barrier);
            let entry = fixture.entry.clone();
            let options = options.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                check_run(std::hint::black_box(libdeno::run(entry, &options)))
            }));
        }

        // All workers and the parent wait on the same barrier.  Starting the
        // timer immediately before the release gives one common run origin;
        // joining all workers records the time until the last one completes.
        let start = Instant::now();
        barrier.wait();
        let mut result = Ok(());
        for worker in workers {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => result = Err(error),
                Err(_) => result = Err("parallel worker panicked".to_string()),
            }
        }
        record("four_way_parallel", iteration, start, result);
    }
}

fn build_reusable_runtime(root: &Path) -> libdeno::LibdenoRuntime {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime-construction Tokio runtime");
    let reusable = runtime
        .block_on(libdeno::LibdenoRuntime::new(root))
        .unwrap_or_else(|error| panic!("build LibdenoRuntime: {error}"));
    drop(runtime);
    reusable
}

fn runtime_reuse(iters: usize, fixture: &Fixture, options: &libdeno::LibdenoOptions) {
    // Construction is intentionally outside the rows: each measured run uses
    // the same resolver stack, matching a long-lived host.
    let reusable = build_reusable_runtime(&fixture.root);
    for iteration in 0..iters {
        let start = Instant::now();
        let result = std::hint::black_box(libdeno::run_with(&reusable, &fixture.entry, options));
        record("runtime_reuse", iteration, start, check_run(result));
    }
}

fn runtime_construct(iters: usize, fixture: &Fixture) {
    let caller_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime-construction Tokio runtime");
    for iteration in 0..iters {
        let start = Instant::now();
        let result = std::hint::black_box(
            caller_runtime.block_on(libdeno::LibdenoRuntime::new(&fixture.root)),
        );
        let result = result
            .map(|runtime| {
                std::hint::black_box(runtime);
            })
            .map_err(|error| error.to_string());
        record("runtime_construct", iteration, start, result);
    }
}

fn fresh_runtime_run(iters: usize, fixture: &Fixture, options: &libdeno::LibdenoOptions) {
    let caller_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build fresh-runtime Tokio runtime");
    for iteration in 0..iters {
        let start = Instant::now();
        let result = match std::hint::black_box(
            caller_runtime.block_on(libdeno::LibdenoRuntime::new(&fixture.root)),
        ) {
            Ok(runtime) => check_run(std::hint::black_box(libdeno::run_with(
                &runtime,
                &fixture.entry,
                options,
            ))),
            Err(error) => Err(error.to_string()),
        };
        record("fresh_runtime_run", iteration, start, result);
    }
}

fn runtime_reuse_async(iters: usize, fixture: &Fixture, options: &libdeno::LibdenoOptions) {
    // Construction is excluded so these rows measure the caller-runtime async
    // run against one reusable resolver stack.
    let reusable = build_reusable_runtime(&fixture.root);
    let caller_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build reusable-async Tokio runtime");
    for iteration in 0..iters {
        let start = Instant::now();
        let result = std::hint::black_box(
            caller_runtime.block_on(reusable.run_async(&fixture.entry, options)),
        );
        record("runtime_reuse_async", iteration, start, check_run(result));
    }
}

fn large_options(root: &Path, capture_stdout: bool) -> libdeno::LibdenoOptions {
    libdeno::LibdenoOptions {
        allow_all_permissions: true,
        cwd: Some(root.to_path_buf()),
        capture_stdout,
        ..Default::default()
    }
}

fn build_subprocess_executor(fixture: &LargeFixture) -> libdeno::Executor {
    let host = std::env::current_exe().expect("resolve benchmark host executable");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build subprocess-executor Tokio runtime");
    runtime
        .block_on(
            libdeno::Executor::builder(&fixture.root)
                .backend(libdeno::ExecutionBackend::Subprocess)
                .host_executable(host)
                .build(),
        )
        .unwrap_or_else(|error| panic!("build subprocess executor: {error}"))
}

fn build_inprocess_executor(fixture: &LargeFixture) -> libdeno::Executor {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build in-process-executor Tokio runtime");
    runtime
        .block_on(
            libdeno::Executor::builder(&fixture.root)
                .backend(libdeno::ExecutionBackend::InProcess)
                .build(),
        )
        .unwrap_or_else(|error| panic!("build in-process executor: {error}"))
}

fn check_executor(
    result: Result<libdeno::ExecutionResult, libdeno::ExecutionFailure>,
) -> Result<PhaseFields, String> {
    match result {
        Ok(result) if result.exit_code() == 0 => Ok(phase_fields(result.report())),
        Ok(result) => Err(format!(
            "executor returned exit code {}",
            result.exit_code()
        )),
        Err(error) => Err(error.to_string()),
    }
}

fn check_inprocess_phase(
    result: Result<libdeno::ExecutionResult, libdeno::ExecutionFailure>,
) -> Result<PhaseFields, String> {
    let phase = check_executor(result)?;
    #[cfg(feature = "phase-diagnostics")]
    if !(phase.resolver_manifest_probe_ns.is_some() || phase.resolver_reuse_ns.is_some())
        || phase.graph_build_ns.is_none()
        || phase.main_worker_bootstrap_ns.is_none()
        || phase.user_execution_ns.is_none()
    {
        return Err(
            "large_inprocess_phase did not report all required executor phases".to_string(),
        );
    }
    Ok(phase)
}

fn large_inprocess_phase(iters: usize, fixture: &LargeFixture) {
    let executor = build_inprocess_executor(fixture);
    let options = large_options(&fixture.root, false);
    for iteration in 0..iters {
        let before = resource_sample();
        let start = Instant::now();
        let result = std::hint::black_box(executor.execute(libdeno::ExecutionRequest::new(
            &fixture.entry,
            options.clone(),
        )));
        record_with_resources_and_phase(
            "large_inprocess_phase",
            iteration,
            start,
            before,
            check_inprocess_phase(result),
        );
    }
}

fn large_subprocess_sequential(
    scenario: &str,
    iters: usize,
    fixture: &LargeFixture,
    capture_stdout: bool,
) {
    let executor = build_subprocess_executor(fixture);
    let options = large_options(&fixture.root, capture_stdout);
    for iteration in 0..iters {
        let before = resource_sample();
        let start = Instant::now();
        let result = std::hint::black_box(executor.execute(libdeno::ExecutionRequest::new(
            &fixture.entry,
            options.clone(),
        )));
        record_with_resources_and_phase(scenario, iteration, start, before, check_executor(result));
    }
}

fn parallelism_n() -> usize {
    let value = std::env::var("LIBDENO_BENCH_PARALLELISM").unwrap_or_else(|_| "8".to_string());
    let parsed = value.parse::<usize>().unwrap_or_else(|_| {
        panic!("LIBDENO_BENCH_PARALLELISM must be a positive integer, got {value:?}")
    });
    if parsed == 0 {
        panic!("LIBDENO_BENCH_PARALLELISM must be a positive integer");
    }
    parsed
}

fn large_subprocess_parallel(
    scenario: &str,
    iters: usize,
    fixture: &LargeFixture,
    capture_stdout: bool,
    concurrency: usize,
) {
    let executor = build_subprocess_executor(fixture);
    let options = large_options(&fixture.root, capture_stdout);
    for iteration in 0..iters {
        let before = resource_sample();
        let barrier = Arc::new(Barrier::new(concurrency + 1));
        let mut workers = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            let barrier = Arc::clone(&barrier);
            let executor = executor.clone();
            let entry = fixture.entry.clone();
            let options = options.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                check_executor(std::hint::black_box(
                    executor.execute(libdeno::ExecutionRequest::new(entry, options)),
                ))
            }));
        }

        let start = Instant::now();
        barrier.wait();
        // `elapsed_ns` below is the parent makespan; phase columns are the
        // explicit sum of every worker report, not the last joined worker.
        let mut result = Ok(());
        let mut phase = PhaseFields::default();
        for worker in workers {
            match worker.join() {
                Ok(Ok(worker_phase)) => phase.add_worker(worker_phase),
                Ok(Err(error)) => result = Err(error),
                Err(_) => result = Err("parallel subprocess worker panicked".to_string()),
            }
        }
        record_with_resources_and_phase(scenario, iteration, start, before, result.map(|()| phase));
    }
}

fn with_large_fixture(run: impl FnOnce(&LargeFixture)) {
    let fixture = LargeFixture::new();
    let root = fixture.root.clone();
    run(&fixture);
    drop(fixture);
    assert!(
        !root.exists(),
        "benchmark fixture must be removed after the scenario"
    );
}

fn large_subprocess_scenario(scenario: &str, iters: usize) {
    with_large_fixture(|fixture| match scenario {
        "large_subprocess_capture_off" => {
            large_subprocess_sequential(scenario, iters, fixture, false)
        }
        "large_subprocess_capture_on" => {
            large_subprocess_sequential(scenario, iters, fixture, true)
        }
        "large_subprocess_parallel_1" => {
            large_subprocess_parallel(scenario, iters, fixture, false, 1)
        }
        "large_subprocess_parallel_4" => {
            large_subprocess_parallel(scenario, iters, fixture, false, 4)
        }
        "large_subprocess_parallel_n" => {
            large_subprocess_parallel(scenario, iters, fixture, false, parallelism_n())
        }
        _ => unreachable!("validated large benchmark scenario"),
    });
}

fn large_inprocess_scenario(iters: usize) {
    with_large_fixture(|fixture| large_inprocess_phase(iters, fixture));
}

fn child_main() -> ! {
    let root = PathBuf::from(
        std::env::var_os(FIXTURE_ENV).expect("child benchmark fixture path is missing"),
    );
    let fixture = Fixture {
        entry: root.join("main.js"),
        root,
    };
    let result = check_run(std::hint::black_box(libdeno::run(
        &fixture.entry,
        &options(&fixture.root),
    )));
    match result {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("cold_process_run child failed: {error}");
            std::process::exit(1);
        }
    }
}

fn main() {
    // The benchmark executable is also the explicit host for the real
    // Executor::Subprocess scenarios. Legacy benchmark child mode remains a
    // separate fixture path below.
    if libdeno::maybe_handle_child_mode() {
        unreachable!("child mode exits from maybe_handle_child_mode");
    }
    if std::env::var("LIBDENO_BENCH_CHILD").ok().as_deref() == Some("1")
        && std::env::var_os(FIXTURE_ENV).is_some()
    {
        child_main();
    }

    let iters = iterations();
    let scenarios = selected_scenarios();
    eprintln!(
        "libdeno baseline diagnostic: pid={} iters={} scenarios={}",
        std::process::id(),
        iters,
        scenarios.join(",")
    );
    eprintln!("results are diagnostic data, not a CI performance gate");

    println!(
        "scenario\titeration\telapsed_ns\tstatus\tthreads_before\tthreads_after\tfds_before\tfds_after\trss_bytes_before\trss_bytes_after\tphase_admission_ns\tphase_queue_wait_ns\tphase_resolver_manifest_probe_ns\tphase_resolver_reuse_ns\tphase_resolver_rebuild_ns\tphase_permission_runtime_services_ns\tphase_graph_build_ns\tphase_main_worker_bootstrap_ns\tphase_user_execution_ns\tphase_output_drain_ns\tphase_cancel_kill_reap_ns\tparent_threads_before\tparent_threads_after\tparent_fds_before\tparent_fds_after\tparent_rss_bytes_before\tparent_rss_bytes_after"
    );
    let fixture = Fixture::new();
    let options = options(&fixture.root);
    for scenario in scenarios {
        match scenario {
            "first_run_in_process" => first_run_in_process(iters, &fixture, &options),
            "warm_run" => warm_run(iters, &fixture, &options),
            "cold_process_run" => cold_process_run(iters, &fixture),
            "run_async" => run_async(iters, &fixture, &options),
            "four_way_parallel" => four_way_parallel(iters, &fixture, &options),
            "runtime_reuse" => runtime_reuse(iters, &fixture, &options),
            "runtime_construct" => runtime_construct(iters, &fixture),
            "fresh_runtime_run" => fresh_runtime_run(iters, &fixture, &options),
            "runtime_reuse_async" => runtime_reuse_async(iters, &fixture, &options),
            "large_inprocess_phase" => large_inprocess_scenario(iters),
            scenario @ ("large_subprocess_capture_off"
            | "large_subprocess_capture_on"
            | "large_subprocess_parallel_1"
            | "large_subprocess_parallel_4"
            | "large_subprocess_parallel_n") => large_subprocess_scenario(scenario, iters),
            _ => unreachable!("validated benchmark scenario"),
        }
    }
    let fixture_root = fixture.root.clone();
    drop(fixture);
    assert!(
        !fixture_root.exists(),
        "benchmark fixture must be removed after all scenarios"
    );
}
