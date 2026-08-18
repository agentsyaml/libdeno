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
];

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
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "{scenario}\t{iteration}\t{elapsed_ns}\t{status}")
        .expect("write benchmark TSV row");
}

fn record(scenario: &str, iteration: usize, start: Instant, result: Result<(), String>) {
    let elapsed_ns = start.elapsed().as_nanos();
    match result {
        Ok(()) => row(scenario, iteration, elapsed_ns, "ok"),
        Err(error) => {
            row(scenario, iteration, elapsed_ns, "error");
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

    println!("scenario\titeration\telapsed_ns\tstatus");
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
            _ => unreachable!("validated benchmark scenario"),
        }
    }
}
