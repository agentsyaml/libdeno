# API Reference

## `run`

```rust
pub fn run(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>
```

Runs `entry` (a file, a directory, or a `package.json`) to completion and
returns the exit code the script requested (`0` on normal completion).

`run` blocks until the script and its event loop finish. Each call gets a fresh
worker and module graph, so independent runs can execute in parallel. Calling
`run` inside a tokio runtime is supported; libdeno runs it on a worker thread.
Output capture is process-wide and exclusive; see [`run_with_output`].

Directory and `package.json` entries resolve through their `main` field (or
`index.js`). A `main` path that escapes the package directory is rejected.

The failure shape depends on how far resolution got, which matters if the host
branches on the error type:

| Case | Error |
|---|---|
| the entry path does not exist | `Entry` |
| `package.json` is unreadable, malformed, or its `main` escapes the package | `Entry` |
| the entry resolves, but the file it points at does not exist | `Core` (`Module not found`) |

A directory with no `index.js` lands in the third row: the directory itself
resolves, and the missing file is a module-load failure. Deno CLI reports it
the same way. A host that wants "the caller gave me a bad entry" as one
category has to accept both.

HTTP operations have a 300-second budget; this is separate from
`execution_deadline`. npm lifecycle scripts are disabled by default.

## `run_with_output`

```rust
pub fn run_with_output(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>

pub struct RunOutput {
  pub exit_code: i32, // as requested by the script; not clamped to a process exit byte
  pub stdout: Vec<u8>, // populated only when options.capture_stdout is set
  pub stderr: Vec<u8>, // populated only when options.capture_stderr is set
  pub capture_truncated: bool, // true when a captured stream hit max_capture_bytes
}
```

`exit_code` is the value the script asked for, unclamped: `Deno.exit(300)` is
`300` and `Deno.exitCode = -1` is `-1`. The Deno CLI would have the OS truncate
those to `44` and `255`; libdeno keeps the requested value so a host can tell
"exited 300" from "exited 44". Clamp it yourself before handing it to
`std::process::exit`.

That holds for the in-process entry points (`run`, `run_with_output`). A
subprocess run reports the child's real process status, so the OS has already
decided the value. On POSIX that status keeps the low 8 bits, so
`Deno.exit(300)` reads back as `44`, `Deno.exit(-1)` as `255`, and
`Deno.exit(256)` as `0`. On Windows the exit code is the full 32-bit value, so
all three read back unchanged. Either way this matches what `deno run`
produces. A caller that compares an exit code against a value above 255
therefore matches in-process and, on POSIX, silently never matches in a
subprocess.

Captures stdout/stderr selected by `capture_stdout` and `capture_stderr` into
`RunOutput`. Capture redirects process file descriptors, so output from other
host threads may also be captured. A captured run is exclusive; use
`run_in_subprocess_with_output` to capture one child while other runs continue.

If capture ends before a pipe reaches EOF, the run returns a timed-out I/O
error rather than reporting incomplete output as success. `capture_truncated`
indicates that the output byte cap was reached; it is distinct from incomplete
pipe collection.

Capture owns fd 1/2 process-wide for the duration of the run. Two consequences
follow from "process-wide": a web worker's `console.log` lands in the same
buffer as the main isolate's, because a worker is another isolate in the same
process, not another process; and `stdout: "inherit"` in a `Deno.Command` does
**not** inherit the host's terminal — it inherits the capture pipe.

A child the script detaches (`unref()`, or otherwise outliving the run)
therefore keeps that pipe's write end open past the run, and two things follow:

- The run reports the timed-out error above, even when the script itself exited
  `0`, and the bytes it already printed are discarded. The collection budget is
  500 ms per captured stream, so this starts at roughly half a second of child
  lifetime.
- Once collection gives up, the pipe read end closes. A detached child that
  later writes to inherited stdout receives `EPIPE`/`SIGPIPE`, which by default
  terminates it.

`run_in_subprocess_with_output` has the same pipe-lifetime behavior; moving the
run into a child does not make detached stdio safe. Daemons the script detaches
must redirect their own output — `stdout: "null", stderr: "null"`, or a log
file they open themselves. `unref()` alone does not detach standard streams.

`run_with_output` still returns `Err` when the script fails, so the bytes it
printed before failing are reachable only through the executor API:
`Executor::execute` reports them as `ExecutionFailure::partial_output()`. That
holds for the in-process backend; the subprocess backend reports a script
failure as a successful execution with a nonzero exit code (see
[backend failure shapes](#backend-failure-shapes)).

## `run_source`

```rust
pub enum SourceLang {
  JavaScript, // virtual entry gets a .js extension
  TypeScript, // virtual entry gets a .ts extension and is transpiled
}

pub fn run_source(
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>

pub fn run_source_with_output(
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>

pub async fn run_source_async(
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>

pub async fn run_source_with_output_async(
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>
```

Runs JS/TS bytes directly without a temporary file. `base_dir` must be an
existing directory; relative paths resolve against `options.cwd` when set,
otherwise the process cwd. Relative imports and config discovery start at
`base_dir`.

The virtual entry gets a unique `$deno$__libdeno_virtual_` file URL under
`base_dir`. `import.meta.url` and `Deno.mainModule` expose that URL. The entry
uses the same read-permission check as an imported file, so a read grant on
`base_dir` covers it and sibling imports. Subprocess and `Executor` APIs accept
file paths only. Other options follow the corresponding path-based API.

`SourceLang::TypeScript` only selects the transpiler; there is no type checking
(like `deno run`, which strips types). A type error such as
`const x: number = "s"` runs and prints `string`. A syntax error is a load-time
`SyntaxError` on the in-process path and, for a subprocess run, exit code 1 —
the same backend difference as any other script error.

## `run_async` / `run_with_output_async`

```rust
pub async fn run_async(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>

pub async fn run_with_output_async(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>
```

Runs `entry` on the caller's tokio runtime instead of spawning a worker thread.

The future is `!Send` and must not be interleaved with another async run on the
same thread; a V8 isolate stays on its creation thread. Await runs one at a
time. On a multi-thread runtime, use a `LocalSet`:

- Call from a tokio runtime; outside one, use `run`.
- Enable tokio's time driver when using `execution_deadline`.
- For parallel runs, use `run` or `run_in_subprocess`.

On a multi-thread runtime, pin the `!Send` future to one thread with a
`LocalSet`:

```rust
use libdeno::{run_with_output_async, LibdenoOptions};

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), libdeno::LibdenoError> {
    let options = LibdenoOptions {
        allow_all_permissions: true,
        capture_stdout: true,
        ..Default::default()
    };
    let local = tokio::task::LocalSet::new();
    let out = local
        .run_until(run_with_output_async("app.js", &options))
        .await?;
    println!("exit={} stdout={:?}", out.exit_code, out.stdout);
    Ok(())
}
```

On a current-thread runtime, await the future directly.

## `LibdenoRuntime` / reusable sync and async runs

```rust
pub struct LibdenoRuntime { /* Clone + Send + Sync; holds `cwd` plus two Arcs: resolver state and rebuild gate */ }

impl LibdenoRuntime {
  pub async fn new(cwd: impl AsRef<Path>) -> Result<Self, LibdenoError>;
  pub fn refresh(&self);

  pub async fn run_async(
    &self,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
  ) -> Result<i32, LibdenoError>

  pub async fn run_with_output_async(
    &self,
    entry: impl AsRef<Path>,
    options: &LibdenoOptions,
  ) -> Result<RunOutput, LibdenoError>

  pub async fn run_with_source_async(
    &self,
    code: impl AsRef<[u8]>,
    lang: SourceLang,
    base_dir: impl AsRef<Path>,
    options: &LibdenoOptions,
  ) -> Result<i32, LibdenoError>

  pub async fn run_with_output_source_async(
    &self,
    code: impl AsRef<[u8]>,
    lang: SourceLang,
    base_dir: impl AsRef<Path>,
    options: &LibdenoOptions,
  ) -> Result<RunOutput, LibdenoError>
}

pub fn run_with(
  runtime: &LibdenoRuntime,
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>

pub fn run_with_output(
  runtime: &LibdenoRuntime,
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>

pub fn run_with_source(
  runtime: &LibdenoRuntime,
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>

pub fn run_with_output_source(
  runtime: &LibdenoRuntime,
  code: impl AsRef<[u8]>,
  lang: SourceLang,
  base_dir: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>
```

Use `LibdenoRuntime::new(cwd)` for repeated runs in one project. It builds the
resolver stack once and `run_with` / `run_with_output` reuse it. The saving is
real but bounded: 20 runs of a one-import entry took 3.9 s with a fresh runtime
each time and 1.65 s on one runtime (194 ms vs 82 ms per run, debug build).
What remains per run is the isolate and module graph, which reuse cannot skip —
every run still gets a fresh one.

The reusable surface:

- `new` is async. Before each run the stack is probed and rebuilt when a
  tracked config, lockfile, registry setting, or `node_modules` directory
  identity changed. That probe covers the `node_modules` directories above the
  workspace and each discovered scope directory, including a nested
  `node_modules` next to the entry.
- Module contents are not part of that probe, but they do not need to be: each
  run gets a fresh module graph, so an edited dependency file is re-read on the
  next run. Measured by editing a file two levels inside a package and reusing
  one runtime — the next run observed the new contents with no `refresh()`.
- `refresh()` marks the stack stale so the next run rebuilds unconditionally.
  It is a forcing function, not a requirement: every scenario tried — a
  same-size import-map edit, a symlinked config's target changing, a package
  `main` edit, a file two levels inside a dependency, adding and removing a
  workspace member — was observed on the next run without it, because the probe
  and the per-run module graph already cover them. Reach for `refresh()` when
  forcing a rebuild is cheaper than reasoning about whether the probe covers
  your change, or when a change lands somewhere the probe does not read. It is
  a bounded invalidation, not a watcher or a tree hash.
- Each run gets fresh permissions, a module graph, and a V8 isolate. `cwd` is a
  resolution base and, when set, must match the runtime's directory.
- `run_with` returns only an exit code and rejects capture flags. Use
  `run_with_output` to capture output; capture remains process-wide and
  exclusive.
- `run_async` methods use the caller's tokio runtime and return `!Send`
  futures. Do not interleave runs on one thread; use `LocalSet` on a
  multi-thread runtime. Each call gets fresh permissions, graph, and isolate.
- `run_async` returns only the exit code. Use `run_with_output_async` to get
  captured bytes; capture is exclusive and unsupported on Windows.
- The `LibdenoRuntime` handle is `Clone + Send + Sync` and can be shared across
  threads. Ordinary runs can execute in parallel.

## `LibdenoOptions`

```rust
#[derive(Debug, Clone, Default)]
pub struct LibdenoOptions {
  pub permissions: Vec<String>,
  pub allow_all_permissions: bool,
  pub prompt: bool,
  pub args: Vec<String>,
  pub cwd: Option<PathBuf>,
  pub max_heap_bytes: Option<usize>,
  pub execution_deadline: Option<Duration>,
  pub capture_stdout: bool,
  pub capture_stderr: bool,
  pub max_capture_bytes: Option<usize>,
  pub features: Option<Vec<String>>,
  pub extensions: Option<ExtensionFactory>,
}
```

### `permissions`

Permission capability strings use CLI `--allow-*` syntax. Any permission flag
restricts access to the listed capabilities. A flag without a value grants
that capability globally; comma-separated values scope it, for example
`--allow-read=./src,./public`. An empty list requires `allow_all_permissions`
or `prompt`.

Supported flags:

| Flag | Value (comma-separated) |
|---|---|
| `--allow-read` | paths |
| `--allow-write` | paths |
| `--allow-env` | environment variable names |
| `--allow-net` | hosts (optionally with `:port`) |
| `--allow-import` | remote module hosts |
| `--allow-run` | executable names or paths |
| `--allow-ffi` | `.so`/`.dylib` paths |
| `--allow-sys` | system API names |
| `-A` / `--allow-all` | — (equivalent to `allow_all_permissions`) |

`--allow-run` names resolve through the child's `PATH`, so the grant pins a
name rather than a file. A scoped grant is also refused outright while the host
environment carries a non-empty `LD_*` or `DYLD_*` variable. Both behaviours,
and what they mean for a host, are in
[Permissions](permissions.md#what-each-grant-actually-covers).

Remote `https:`/`jsr:` imports require `--allow-import`; `--allow-net` does not
grant module-import permission. No import host is granted by default: unlike
`deno run`, which pre-populates `--allow-import` with `deno.land`, `jsr.io`,
`esm.sh`, `raw.esm.sh`, `cdn.jsdelivr.net`, `raw.githubusercontent.com`, and
`gist.githubusercontent.com`, libdeno denies every remote import until a host is
named. Pass the hosts you need, or a bare `--allow-import` to grant all of them.

Static and dynamic file imports are checked against `--allow-read`.

### `allow_all_permissions`

Grants every capability (`-A` equivalent). Use it only for trusted code.

It wins over `permissions`: setting both runs with every capability, and the
list is not applied as a narrowing filter. A host that builds one from user
input and the other from a policy flag gets the wider behaviour, so pick one
site to set and leave the other at its default.

### `prompt`

Prompts on stdin for checks not granted by flags. Prompting requires terminal
stdin; without a terminal, checks are denied.

In subprocess mode the child's stdin carries the run request, so interactive
prompting is unavailable and non-granted checks are denied. `run_in_subprocess`
does not reject that combination: `prompt: true` with full grants is valid there
because the prompt is never consulted, and erroring would break it.
`run_in_subprocess_with_output` does reject it — it always pipes stderr, so the
prompt could never be shown (see below).

`prompt` cannot be combined with `capture_stderr`. The upstream prompter writes
to stderr and refuses to run unless *both* stdin and stderr are terminals;
stderr capture replaces fd 2 with a pipe, so the pair would turn every
ungranted check into a silent deny with no prompt ever shown. The combination
is rejected with `Configuration` before the run starts. Capture stdout only, or
grant the capabilities up front.

### `args`

Arguments for the script, in order. They appear in `process.argv` after the
executable and the entry module — so `process.argv[2]` is the first one,
matching `deno run app.js a b` — and `Deno.args` is the same list without those
two leading entries. Both backends agree; `tests/args_contract.rs` pins it.

### `max_heap_bytes`

Best-effort limit for the V8 old-generation heap. Values below 8 MiB and
`usize::MAX` are rejected. It does not limit native memory, RSS, CPU, or child
processes.

Exceeding the cap terminates the script: the run fails with the V8 termination
error (`execution terminated`) and the host process keeps running. It does not
fail with a dedicated out-of-memory error — V8 reports the termination, not the
cause.

The cap is forwarded to web workers, so a worker cannot bypass it. The failure
shape differs by where the allocation happened: a main-isolate overflow reads
`execution terminated`, while a worker overflow surfaces as the worker's own
uncaught error — `Unhandled error in child worker.` at
`ext:runtime/11_workers.js`, with no termination text in the rendered message.
Both are Deno's shapes, not libdeno's. See `tests/worker_heap_cap.rs` for both
paths.

V8 raises the effective ceiling once to reach the next JS stack check, so peak
heap can briefly exceed the requested value before the script unwinds.
ArrayBuffers and other external allocations are not counted against this cap:
a 128 MiB `ArrayBuffer` runs to completion under an 8 MiB cap, so lowering the
cap does not bound RSS. `tests/heap_cap_shapes.rs` pins that.
A subprocess run reports the same termination as `Ok(exit_code: 1)` with
`libdeno child run failed: … execution terminated` on the child's stderr — the
usual [backend difference](#backend-failure-shapes); the host still survives.

### `execution_deadline`

Best-effort deadline. V8 is terminated at an interruptible stack check; a
blocking system call, native code, child wait, or permission callback can make
the run exceed it. The HTTP timeout is separate.

The deadline stops workers too. It terminates the main isolate and then closes
the worker registry, which asks every live worker to stop and waits for them:
the run does not return until all of them have unwound. A worker that was still
executing when the deadline fired therefore never reaches its next statement —
its writes, reads and messages after that point never happen.

The registry close is what makes the run wait: `close_and_wait` has no timeout
of its own, so its cost is whatever each worker takes to unwind. Both shapes
were measured to stop and never write afterwards — a worker in
`while (...) {}` and one parked on a 6 s timer — so the wait is bounded by how
quickly each worker reaches an interruptible point, not by the deadline. A
worker stuck in a blocking syscall holds the run the same way a blocking read
holds the main isolate (see below).

A worker that finishes before the deadline is not affected. The run still
returns `Timeout` — the deadline is reported regardless of what the script did
— and whatever the worker wrote before it stopped is on disk.

A read that never completes is the extreme case, and the deadline cannot bound
it in either API. Opening a FIFO with no writer blocks in `open(2)`:

- `Deno.readTextFileSync` (and every other `*Sync` filesystem call, including
  the write side) runs on the isolate thread, so the run never reaches a stack
  check and never returns.
- `Deno.readTextFile` and the other asynchronous forms open on the blocking
  pool, so the deadline does fire and the result is `Timeout` — but the call
  still does not return until the blocked thread finishes, because shutdown
  waits for the pool. Measured with a 1 s deadline and a writer that appeared
  after 3 s: the error was correct, and the return took the full 3 s
  (`tests/blocking_file_reads.rs`, both halves).

A subprocess run is not an escape either: the parent's deadline cannot cut down
a child stuck in a syscall. A host that reads paths it does not control should
resolve them with its own timeout before handing them to a script.

Network I/O and child processes are not affected. `Deno.connect` to an
unroutable address, a `Deno.Conn.write` blocked by a peer that never reads, and
a `Deno.Command` child's stdin blocked by a child that never reads all stop at
the deadline.

In-process, expiry is `LibdenoError::Timeout`. In a subprocess the deadline
still cuts the child down, but the parent reports `Ok` with `exit_code == 1` and
the reason on the child's stderr (`libdeno child run failed: execution deadline
of 300ms exceeded; isolate terminated`). Exit code 1 is therefore ambiguous
between "deadline fired" and "the script called `Deno.exit(1)`"; capture stderr
to tell them apart, or use the execution-control supervisor, which reports
`Supervisor(Timeout)`.

`Some(Duration::ZERO)` is not a deadline that expires at runtime: it refuses to
start the script, and returns `LibdenoError::Timeout` on both backends without
spawning a child.

When the deadline fires while JavaScript is running, the run returns shortly
after it: a busy loop under an 800 ms deadline comes back at 0.87 s on an idle
host. When the event loop is instead parked on a timer, there is no JS on the
stack to interrupt, so the run waits for a fixed 2 s grace period before the
future is dropped. Measured in-process, capturing both streams:

| Script under a deadline | Deadline 100 ms | 800 ms | 2 s |
|---|---|---|---|
| `for (;;) {}` | 0.46 s | 0.87 s | 2.07 s |
| `await new Promise(r => setTimeout(r, 60000))` | 2.18 s | 2.87 s | 4.08 s |
| `setInterval(...); await new Promise(() => {})` | 1.08 s | 1.08 s | 2.08 s |

The timer rows are `deadline + 2 s`. Budget for that: a host that uses a 1 s
deadline to bound an await-heavy script is really bounding it at 3 s. The busy
row has no such offset because `terminate_execution` throws at the next stack
check; its absolute value still moves with host load (a busy machine measured
1.4 s for the 800 ms column), while the timer rows stay within a few
milliseconds of `deadline + 2 s` because the grace is a timer, not work.
`tests/deadline_shapes.rs` pins the split between the two shapes; the grace
offset is `limits.rs`'s `GRACE`.

### `cwd`

Resolution base for relative entry paths, permission paths, and `node_modules`
discovery. Defaults to the process cwd. In-process runs do not change the host
cwd; use absolute paths or a subprocess API when the script needs another cwd.
`LibdenoRuntime::new(cwd)` is the exception to that rule: it accepts a path that
does not exist yet and falls back to the raw path, so `new(dir)` may run before
`create_dir_all(dir)`. A typo in that path is therefore not reported when the
runtime is built; the first `run_with` fails at module load instead, naming the
missing directory. A path that exists but is a file is rejected immediately
(I/O error). `tests/runtime.rs` pins the missing-directory case.

An explicit `cwd` must resolve to a directory: a missing path or a file is
rejected with `Configuration` on every backend, instead of falling back to the
host cwd while the permission grants still point at the missing directory.

`Deno.chdir()` is refused in-process with `NotSupported`. It would change the
host process's working directory, redirecting the host's own relative paths and
corrupting any concurrent run that resolves a relative specifier — the exact
thing this option promises never happens. Use `run_in_subprocess` when the
script needs a different working directory; the child gets its own.

In-process, `cwd` is a base for what libdeno resolves — entry, permissions,
config, `node_modules` — not a change of directory, so the script's own
relative paths still follow the *host process* cwd. The subprocess backend
really changes directory, so the same script under the same options opens a
different file:

```rust
// cwd = /srv/plugin, host process cwd = /srv/host, both granted read.
// /srv/plugin/app.js: Deno.readTextFileSync("local.txt")
run(...)                        // reads /srv/host/local.txt
run_in_subprocess(...)          // reads /srv/plugin/local.txt
```

Pass absolute paths inside the script, or use the subprocess backend, when the
distinction matters. `tests/cwd_resolution.rs` pins both sides.

### `capture_stdout` / `capture_stderr`

Capture the script's stdout and/or stderr in `RunOutput`; output passes through
by default. See [`run_with_output`] for capture exclusivity and subprocess
capture.

### `max_capture_bytes`

Per-stream byte limit. Excess bytes are drained and dropped, and
`RunOutput::capture_truncated` is set. It only applies to a stream that is
actually being captured: setting it without `capture_stdout` (or
`capture_stderr`) changes nothing, and the run reports empty output rather than
a truncated one. `tests/capture_limits.rs` covers the flag pairing. The limit
counts bytes, not characters,
so a truncation can split a multi-byte UTF-8 sequence; `RunOutput::stdout`
returns the raw bytes, and a caller that needs a string must handle the
trailing incomplete character (for example `String::from_utf8_lossy`).
`None` is unbounded for legacy and
in-process capture. The execution-control supervisor backend defaults to 64 KiB
per stream and accepts explicit limits up to 96 KiB; these limits do not apply
to `run_in_subprocess_with_output`, which is unbounded unless
`max_capture_bytes` is set. The supervisor sends no output in `TERMINAL` frames:
the parent captures from pipes. In-process capture is unsupported on Windows;
use `run_in_subprocess_with_output`.

### `features`

Unstable feature names to enable, overriding the default set (`kv`, `cron`,
`ffi`, `webgpu`, `worker-options`). Names must be valid Deno unstable-feature
names. `None` enables the default set.

A name only has an effect where Deno still gates something on it, and in Deno
2.9.x that is a shorter list than the names suggest. Measured with
`Some(vec![])` against this build (`tests/features.rs`):

| Name | Effect of omitting it |
|---|---|
| `kv` | `Deno.openKv` becomes `undefined` |
| `cron` | `Deno.cron` becomes `undefined` |
| `worker-options` | force-enabled regardless — `op_create_worker` exits the host when the feature is off |
| `net` | `WebSocketStream` / `WebTransport` stay absent unless added |
| `raw-imports` | `CSSStyleSheet` stays absent unless added |
| `ffi` | **nothing** — `Deno.dlopen` is a stable API in Deno 2.9.x and is present either way |
| `webgpu` | **nothing** — `navigator.gpu` is present either way |

`ffi` and `webgpu` are accepted for compatibility but no longer gate their
namespace, so shrinking `features` does **not** remove FFI or WebGPU from a
plugin's surface. The control for FFI is the `--allow-ffi` permission: with
`features: Some(vec![])` and no grant, `Deno.dlopen` throws `Requires ffi
access`; with `--allow-ffi` it opens the library. Deno CLI 2.9.8 behaves the
same way — `deno run -A` exposes `Deno.dlopen` and `navigator.gpu`, and the
`--unstable-*` flags no longer exist.

### `extensions`

An optional `ExtensionFactory` that creates fresh native `deno_core::Extension`
values for the main runtime and each web worker, including nested workers. The
factory runs on the runtime thread; each runtime has its own `OpState`, though
the factory may deliberately capture shared host state. Native ops are not
automatically permission-gated. Extension factories cannot cross subprocess or
supervisor boundaries; runs with a factory are rejected before spawning. See
the [native extension guide](native-extensions.md) for initialization examples.

## Environment variables

Two process-global knobs are read once, on first use, and then frozen for the
life of the process (each is backed by a `OnceLock`). Set them during trusted
startup, before the first run:

| Variable | Default | Effect |
|---|---|---|
| `LIBDENO_ANALYSIS_CACHE_ENTRIES` | `8192` | Entry cap for the module-info/analysis caches. Larger values trade memory for fewer re-parses on very large projects. A non-numeric or `0` value falls back to the default. |
| `LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES` | `1 GiB` | Rejection threshold for npm tarballs whose gzip `ISIZE` trailer claims a larger member. This is a cheap pre-check against a declared ~4 GiB member, not a decompressed-output budget. A non-numeric value falls back to the default; `0` does **not** — it becomes a real budget, so every tarball is rejected. The two knobs disagree on zero: the cache knob treats `0` as "use the default", this one treats it literally. |

Changing either after the first run has no effect. `LIBDENO_HOST_EXE` (see
[`run_in_subprocess`](#run_in_subprocess)) is read per spawn instead, so it can
change between runs.

**These names are denied to scripts.** The variables above, plus
`LIBDENO_HOST_EXE` and the child/supervisor handshake variables
(`LIBDENO_CHILD_MODE`, `LIBDENO_CHILD_TOKEN`, `LIBDENO_SUPERVISOR_MODE`,
`LIBDENO_SUPERVISOR_ENDPOINT`, `LIBDENO_SUPERVISOR_TOKEN`), and the resolver
inputs (`DENO_DIR`, `DENO_NO_PACKAGE_JSON`, `JSR_URL`,
`NPM_CONFIG_REGISTRY`, `NPM_CONFIG_REPLACE_REGISTRY_HOST` and their lowercase
aliases), are on a permanent env deny list: `Deno.env.set`/`get`/`delete` on
them raise `NotCapable` even under a bare `--allow-env`. Without this,
`--allow-env` would be an arbitrary-code grant — a script could point
`LIBDENO_HOST_EXE` at its own binary and the next `run_in_subprocess` would
execute it, or repoint `DENO_DIR`/the npm registry so a later run fetches and
caches attacker-controlled modules. `-A` / `allow_all_permissions` still
bypasses the list, as it bypasses every other permission.

## Phase diagnostics (`phase-diagnostics`)

An experimental, `#[doc(hidden)]` surface, not a stable contract. It exposes
per-execution phase durations and parent-process resource counters through
`ExecutionReport::phase_diagnostics()`. Read the following before using it for
attribution:

- **A subprocess execution reports the parent's phases only.** `user_execution`
  and `graph_build` happen in the child and are `None`; the parent records
  `admission`, `output_drain`, and `cancel_kill_reap`. Absent is not "did not
  run".
- **Resource counters are platform-gated.** Threads are Linux-only. Current RSS
  is Linux-only: on macOS `getrusage` returns `ru_maxrss`, a peak since process
  start, so reporting it as a before/after pair would show a false flat line and
  the field stays `None`. File descriptors are available on Unix.
- **Durations only.** The report is sanitized: option values, permission flags,
  script source, and registry credentials never appear in it.

## Resource and supervision boundaries

- Remote module bodies and npm metadata are capped at 256 MiB; `.tgz` bodies at
  1 GiB. These caps apply to reqwest-returned, transport-decoded bytes, and the
  decoder may allocate before libdeno checks them. The `.tgz` gzip `ISIZE`
  check is only a size hint, not a decompression-output limit.
- HTTP operations have a 300-second budget. Remote redirects are handled by
  the file fetcher; the HTTP client follows redirects only for npm downloads.
- npm lifecycle scripts are disabled.
- `max_heap_bytes` caps V8's old generation, not the module graph. The loader
  keeps module source and metadata outside that budget: a run of 10,000 small
  modules held about 230 MB RSS under a 32 MB cap. Budget process memory by
  module count, not only by the heap cap.
- Repeated in-process runs of a large entry file grow the host's resident set
  even though nothing is retained: about one source-size worth of RSS per run
  for a 10 MB file, and the growth does not stop. This is the macOS large-block
  allocator, not libdeno. Each run allocates and frees buffers of slightly
  different sizes, and the allocator keeps the freed blocks in its large zone
  instead of returning the pages (`vmmap` shows them under `Malloc Large
  (empty)`; `leaks` finds only a few kilobytes). Plain Rust that repeatedly
  allocates and frees differently sized 10 MB blocks reproduces it exactly,
  while fixed-size allocation does not. Nothing in libdeno can release those
  pages; `malloc_zone_pressure_relief` frees 0 bytes. Subprocess runs do not
  accumulate, because each child exits and returns its memory to the OS, so a
  host that runs large scripts repeatedly should prefer
  `run_in_subprocess_with_output`.
- Static `import` chains are evaluated recursively, so a chain of roughly
  2,300 modules fails with `RangeError: Maximum call stack size exceeded` in a
  debug build (release builds have smaller frames and go deeper). Chains built
  with dynamic `import()` are not affected. The failure is an ordinary run
  error: in-process runs return `Err`, subprocess runs exit non-zero with the
  message on stderr, and later runs in the same process still work.
- Subprocess runs supervise the direct child, not its entire process tree.
  Heap and execution limits are not OS-level RSS or CPU limits. Windows
  overlapped-write cancellation may finish after the requested deadline to
  preserve buffer lifetime.
- Every file write truncates and rewrites in place; none is atomic.
  `Deno.writeTextFileSync` / `writeTextFile` and the binary
  `Deno.writeFileSync` / `writeFile` both let a concurrent reader of the same
  path observe a truncated or half-written file. Measured with one run
  rewriting a 200 KB file while another read it: 2 of 200 reads (text) and 43
  of 400 reads (binary) saw a length between the two written sizes
  (`tests/concurrent_file_write.rs`). Deno CLI
  2.9.8 is worse on the same test (20 of 200). Hosts that share a file between
  runs must write to a temporary path and rename: the same measurement through
  `writeTextFileSync` + `renameSync` produced 0 torn reads out of 400, because
  `rename` replaces the directory entry atomically on one filesystem.
- A process the *script* spawns with `Deno.Command` is killed when the run
  ends, unless the script called `unref()` on it. `Deno.exit()`, a thrown
  error, and an expired deadline all tear down the resource table, and
  `ChildResource::drop` sends `SIGKILL` to every child that has not been
  unref'd. Measured both ways: after `Deno.exit()` the child is a zombie,
  while an `unref()`'d child keeps running. Set `detached: true` or call
  `unref()` when the process is meant to outlive the run. Deno CLI 2.9.8
  behaves identically.
  A host that runs untrusted scripts must still contain the *unref'd* case
  itself — a cgroup or process group it tears down afterwards — because
  neither libdeno nor Deno tracks what a script detached.

## `LibdenoError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum LibdenoError {
  Entry(AnyError),                       // entry path could not be resolved to a module
  Permission(String),                    // invalid permission flags
  Configuration(String),                 // invalid options
  Runtime(AnyError),                     // runtime startup or script failure
  Core(deno_core::error::CoreError),     // deno_core execution failure
  Io(std::io::Error),                    // host I/O failure
  Timeout(String),                       // deadline or subprocess handshake timeout
}
```

`Core` carries deno_core's own classification, and it is reachable: match
`LibdenoError::Core(core)` and read `core.0`, a `CoreErrorKind`. A script that
throws lands on `Js`; a missing module, a parse error and an unmapped specifier
each land on `JsBox` with a different inner error (`ModuleError(Missing)`,
`ModuleError(Parse)`, `ResolutionError`). That is a type-level discriminant, so
a host does not have to match message text to tell "your module is missing"
from "your script threw". The variants belong to deno_core and can change with
its version; the rendered text is no more stable, but the type is at least
checkable at compile time.

This holds for the plain `run` / `run_with_output` paths. A subprocess run
reports a script failure as `Ok` with a non-zero exit code instead, so there is
no `Core` to inspect. On the execution-control path the error can be cloned
when the failure is still shared (`ExecutionReport::into_parts`), and a cloned
`Core` is re-wrapped as `JsBox`: the class and message survive, the inner
`CoreErrorKind` does not. Inspect `core.0` where the run happened in-process and
the error was not cloned.

`is_permission_error()` is true only when a typed Rust permission-denial source
is preserved. JavaScript names, classes, causes, and messages do not establish
denial provenance. A JS failure without typed evidence may be `Runtime` or
`Core`; both map to `DenoRuntimeError` in Python.

`Entry` covers entry *resolution*: a `package.json` that cannot be read or
parsed, a `main` that escapes the package directory, and an invalid virtual
in-memory path. A path entry that simply does not exist still resolves to a
`file:` URL and fails at module load as `Core` (`Module not found`), matching
the Deno CLI. The Python binding checks the entry path up front and raises
`EntryError` for it instead.

Two variants wrap their payload in a fixed prefix; the rest forward it
unchanged. That prefix is the only text-level signal separating them from
`Runtime`, which carries the same inner types:

| Variant | Rendered text |
|---|---|
| `Entry` | `failed to resolve entry module: <inner>` |
| `Permission` | `invalid permission flags: <inner>` |
| `Configuration` / `Runtime` / `Core` / `Io` / `Timeout` | `<inner>` |

Branch on the variant, not on the prefix — a host that parses the text gets a
string the inner error also controls. The prefix is for logs.
`tests/entry_resolution.rs` pins the `Entry` one.

## `run_in_subprocess`

```rust
pub fn run_in_subprocess(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<i32, LibdenoError>
```

Runs `entry` in a child process and returns its exit code. The child contains
`Deno.exit` and many process-level failures, but native crashes can still affect
the host; use an OS sandbox for untrusted code.

Call `maybe_handle_child_mode()` during startup, after any required trusted
initialization (such as Node IPC setup or installing a broker/hook) and before
normal host logic. The child inherits stdout/stderr and uses `options.cwd`.
Entry, permissions, args, and cwd are sent as JSON over stdin. The request is
capped at 1 MiB and its handshake at 10 seconds. Only the direct child is
reaped; descendants are not fully supervised. Use `run_in_subprocess_with_output`
to capture child output.

**The run request consumes stdin.** The pipe is closed after the payload write,
so a script that reads stdin sees EOF — not the host's stdin, and not an error.
The supervisor backend (execution-control `submit`, `run_in_supervised_subprocess`)
inherits the host's stdin instead, so the same script reads real input there.
Redirect a script's input through a file or a socket if it must work on both.

`LIBDENO_HOST_EXE` overrides the executable spawned (defaults to
`current_exe()`); integration tests use this to point at a dedicated host.

The default host is `current_exe()`, so a host that never calls
`maybe_handle_child_mode()` spawns a copy of itself that ignores the request and
runs its own `main()` instead. The call still returns `Ok`, carrying whatever
that process printed — not the script's output. Such a process refuses to spawn
a further child (the call fails with `Configuration` and names the missing
call), so the mistake surfaces as one wrong result instead of an unbounded
process tree.

## `run_in_subprocess_with_output`

```rust
pub fn run_in_subprocess_with_output(
  entry: impl AsRef<Path>,
  options: &LibdenoOptions,
) -> Result<RunOutput, LibdenoError>
```

Runs `entry` in a child process and captures both stdout and stderr through
per-child pipes. This capture works in parallel with other runs, including on
Windows. In-process capture is exclusive and unsupported on Windows.

"Parallel" is not unlimited: the reader budget below is process-wide, so the
ceiling is a property of the helper, not of the caller's thread pool. It is
still parallel in the sense that matters here — two captures do not fight over
a file descriptor, so neither one's output lands in the other's buffer.

- Both streams are returned; `capture_stdout` / `capture_stderr` are implied.
- `prompt: true` is rejected: this helper always pipes stderr, and permission
  prompts require stderr to be a terminal. Use `run_in_subprocess` (which
  inherits stdio) when prompting is required.
- `max_capture_bytes` limits retained bytes per stream. Excess bytes are
  drained and dropped; `capture_truncated` indicates truncation.
- This helper is unbounded unless `max_capture_bytes` is set, including when
  execution control is enabled. The 64 KiB default and 96 KiB maximum apply only
  to the execution-control supervisor backend.
- **At most 32 runs can be in flight at once.** Each run holds two slots (one
  reader per stream) against a process-wide budget of 64. A 33rd simultaneous
  capture fails with an `Io` error reading `subprocess output reader/pipe
  resource budget exhausted`; it is not queued. The budget is released as each
  run finishes, so a later burst succeeds — size a worker pool at 32 or below,
  or use `run_in_subprocess` (no pipe readers) when more concurrency is needed.

  The two `Io` failures this helper produces are distinguishable by
  `ErrorKind` without matching on message text:

  | `ErrorKind` | Cause | Retryable |
  |---|---|---|
  | `WouldBlock` | the reader budget is exhausted | yes, once runs finish |
  | `TimedOut` | a descendant held the capture pipe open past the 500 ms collection budget | no; fix the script's stdio |

  `WouldBlock` is a scheduling decision and `TimedOut` is a defect in the
  script, so a host should back off on the first and alert on the second.
  `tests/subprocess_capture_concurrency.rs` and `tests/capture_limits.rs` cover
  them.

## Backend failure shapes

The same failing script is reported differently depending on the backend, and
the difference is invisible in the return type:

| Backend | A thrown script error | Captured prefix |
|---|---|---|
| In-process | `Err(ExecutionFailure)`, message is the script's error | `partial_output()` |
| Subprocess | `Ok(ExecutionResult)` with `exit_code == 1` | `output()` |

`run_with_output` and `run_in_subprocess_with_output` differ the same way: the
in-process helper returns `Err`, the subprocess helper returns `RunOutput`
whose stderr contains `libdeno child run failed: <error>`.

The subprocess side cannot do better without changing the child protocol: the
child reports its outcome through its exit status, so a script exception is
indistinguishable from an explicit `Deno.exit(1)`. A caller that must tell
"crashed" from "chose to exit 1" has to run in-process, or inspect the
`libdeno child run failed:` prefix on stderr.

Configuration errors are not part of that ambiguity: the subprocess parent
validates `permissions`, `max_heap_bytes`, `execution_deadline`, `features`,
and `prompt` + `capture_stderr` before spawning, and returns the same
`Configuration` / `Permission` / `Timeout` error the in-process path would
raise. An invalid configuration therefore never reaches the child and never
shows up as `Ok(exit_code: 1)`.

The execution-control supervisor backend does distinguish the two: `submit`
reports a script error as `ExecutionError::Supervisor(Runtime)`.

That category is a closed enum of five variants, and each renders a fixed
sentence — the child's own error text never crosses the supervisor boundary:

| Category | `to_string()` |
|---|---|
| `Permission` | `supervisor child permission failure` |
| `Timeout` | `supervisor session timed out` |
| `Runtime` | `supervisor child runtime failure` |
| `Infrastructure` | `supervisor infrastructure failure` |
| `ChildCrash` | `supervisor child exited without a terminal result` |

`Timeout` is the only one that arrives as `LibdenoError::Timeout`; the other
four arrive as `Runtime`, which is what `run_in_supervised_subprocess` returns.
`ExecutionError::Supervisor(category)` carries the category itself, so a host
using `submit` should branch on that instead of the text.
`ChildCrash` means the child died after `START` without a `TERMINAL` frame —
`Deno.kill(Deno.pid, "SIGKILL")` produces it. A child that succeeds writes a
`TERMINAL`, so a crash cannot be confused with `Deno.exit(0)`.

`Permission` only covers denials the *host* hits before user code runs, such as
a grant that excludes the entry file itself. A denial raised inside the script
arrives as a `NotCapable` JS exception, so the typed Rust provenance the
classification needs is already gone; that lands on `Runtime`, the same as any
other throw. The host cannot separate "the script tried something it was not
allowed to do" from "the script threw" — inspect the child's stderr for that,
where the exception text still exists.

Type fidelity follows the same split. `LibdenoError::Core` carries deno_core's
`CoreErrorKind`, which separates a thrown script error (`Js`) from a loader
failure (`JsBox` with a `ModuleError` or `ResolutionError` inside) — but only
where the error survives as a `Core`:

| Backend | `Core` with `CoreErrorKind` |
|---|---|
| In-process `run` / `run_with_output` | yes |
| Subprocess | no — there is no `Core`, only an exit code |
| Execution-control `into_parts` | may be flattened to `JsBox` when the failure is cloned |

A host that classifies failures by type should do it on the in-process path and
treat the other two as "failed", not try to recover the kind from text.

## Web worker errors

An unhandled error in a web worker fails the run with
`Error: Unhandled error in child worker.` — on both backends. The worker's own
message, stack, and source line are not part of that error: upstream reports
them with `log::error!` from `run_web_worker`, and libdeno installs no `log`
logger, so the record is discarded.

To surface the detail, install a `log` logger in the host and enable at least
`Warn`:

```rust
log::set_logger(&MyLogger)?; // any log::Log impl writing to stderr
log::set_max_level(log::LevelFilter::Warn);
```

The record then arrives on stderr as
`error: Uncaught (in worker "") <message>`, while the run's `Err` still carries
only the generic message. Setting `worker.onerror` inside the script is the
other option: the handler receives the worker's `ErrorEvent`, and calling
`preventDefault()` keeps the run alive.

A worker that is never terminated keeps the event loop alive, exactly like
`deno run`: the run then ends only through `execution_deadline`.

## Choosing between `execute` and `submit`

`Executor::execute` and the execution-control `submit` scheduler are two
different dispatchers with different behavior for the same request, even on the
same `Executor` and backend. `execute` is the documented surface above; `submit`
routes to the supervisor.

| Behavior | `execute` (subprocess) | `submit` (supervisor) |
|---|---|---|
| `max_capture_bytes: None` | unbounded | 64 KiB per stream, `capture_truncated` set |
| `max_capture_bytes: Some(200 KiB)` | accepted | `Configuration`: above the 96 KiB maximum |
| Thrown script error | `Ok`, exit code 1 | `Err`, `Supervisor(Runtime)` |
| Exit code above 255 | OS-truncated | OS-truncated |
| Script reads stdin | EOF (stdin carried the run request) | host stdin, inherited |

Pick one dispatcher per call site. A caller that switches from `execute` to
`submit` for cancellation or admission control silently loses capture beyond
64 KiB, starts receiving `Err` where it used to receive `Ok`, and starts
forwarding the host's stdin to the script where it previously got EOF.

## `maybe_handle_child_mode`

```rust
pub fn maybe_handle_child_mode() -> bool
```

Services a child-run request when the process was spawned by
`run_in_subprocess`.

- Normal host launch: returns `false` immediately.
- Child mode: executes the requested script and exits the process with the
  script's exit code (including `Deno.exit(n)`); it does not return.

Call it after required trusted startup initialization and before normal host
logic:

```rust
fn main() {
  libdeno::maybe_handle_child_mode();
  // ... normal host logic ...
}
```

## `initialize_node_ipc_from_env`

```rust
pub unsafe fn initialize_node_ipc_from_env() -> Result<(), LibdenoError>
```

For a Node `child_process.fork` host, explicitly adopts the connected endpoint
identified by `NODE_CHANNEL_FD`. Call it during trusted startup, before creating
threads, then call `maybe_handle_child_mode()` before normal host logic. The
caller transfers unique ownership of the raw descriptor/handle to libdeno.

Initialization is one-shot, including failed attempts. A claimed endpoint is
closed if later validation fails, so it cannot be retried. If the variable is
absent, initialization records that no IPC channel is available. Ordinary
`run()` neither reads nor claims a live environment channel. Do not let
untrusted input select a raw OS handle.

## `install_permission_broker`

```rust
pub fn install_permission_broker(path: impl AsRef<Path>) -> Result<(), LibdenoError>
```

Installs an external permission broker at `path` (Unix socket or Windows named
pipe). It is process-global and install-once; after installation it decides
resource-access checks even when local flags grant access. It does not revoke
local grants or change permission state reported by `Deno.permissions.query()`
and `request()`. Checks block until the broker responds.

Install only before starting runs or between runs after all workers have
stopped. Installation returns `LibdenoError::Configuration` immediately if a
run or worker is active; new runs likewise return a configuration error while
installation is in progress. The crate serializes its own install APIs and run
admission, but direct calls to upstream `set_broker` or runtime APIs are outside
that coordination and must be synchronized by their caller.

The upstream constructor may call `process::exit(87)` if the initial connection
fails; later bridge failures may also terminate the process. These exits cannot
be returned as `LibdenoError`, so use a trusted, reliable broker. To pass the
broker to subprocess runs, install it during trusted startup before
`maybe_handle_child_mode()` dispatches child mode.

## `install_permission_hook`

```rust
pub type PermissionPrompt = Arc<dyn Fn(&PermissionRequest) -> bool + Send + Sync>;

pub fn install_permission_hook(hook: PermissionPrompt) -> Result<(), LibdenoError>
```

Installs an in-process hook on Unix; on Windows, use
`install_permission_broker`. Return `true` to allow a request and `false` to
deny it. Like the broker, the hook is process-global, install-once, and the sole
authority for permission checks once installed. It cannot be combined with the
broker.

Like broker installation, hook installation is rejected during active runs or
while workers remain, and runs are rejected until installation completes.

The hook runs synchronously on an internal thread; a slow callback stalls
permission checks and can delay `execution_deadline`. An unwinding panic is
denied, but a `panic = "abort"` build cannot recover. The shared upstream
broker bridge can still terminate the host on connection failure (`exit(87)`);
that termination is not returned through `Result`. With no permission flags,
set `prompt: true` so checks reach the hook.

A hook lives in the process that installed it. `run_in_subprocess` starts a
fresh process that knows nothing about it, so the child enforces its own flags
instead: an in-process hook that denies everything does not restrict a
subprocess run, and `allow_all_permissions: true` in that child is a full
grant. Install the hook in the child's startup path — before
`maybe_handle_child_mode()` dispatches child mode, as the broker note above
describes — if subprocess runs must be covered too.

Do not fork and continue using an installed broker or hook. Exec immediately,
then install it during trusted startup in the new process; reinstalling in the
fork child is not supported.

## Permission decision priority

broker/hook (if installed, the sole authority) → flag grants → interactive
prompt when `prompt: true` → deny.

## Example host binary

`examples/demo.rs` shows a host that calls `maybe_handle_child_mode()` at
startup, classifies errors by variant, and supports embedded and subprocess
execution. Usage:

```text
demo [permission flags...] <entry> [script args...]
```

The argument parser is deliberately strict because the example grants
everything when no `--allow-*` flag is present. An unrecognized or misspelled
flag exits `2` with `unknown flag: …` instead of being skipped — otherwise
`--deny-read=/etc`, `-R`, or `--alow-read=/etc` would leave the permission list
empty and hand the script every capability. The allow-all default is the
example's choice; libdeno itself does the opposite and rejects an empty list
unless `allow_all_permissions` is set. A host that keeps a CLI should either
validate its flags the same way or use the fail-closed default.

For Node `child_process.fork` startup and inherited npm-state limits, see
[npm support](npm-support.md#child_processfork).
