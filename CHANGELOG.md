# Changelog

All notable changes to libdeno. Following the [keep a changelog] convention;
breaking changes are highlighted per release with migration notes.

[keep a changelog]: https://keepachangelog.com/en/1.1.0/

## 0.3.1 (Unreleased)

### Phase 1 / Phase 2 behavior changes

- **Phase 1 resolver state**: reusable runtimes can be explicitly refreshed,
  and their resolver fingerprint now includes the effective npm registry,
  project `.npmrc`, and the resolver-supported `$HOME/.npmrc`; forked npm
  children read the latest managed resolution rather than an
  initialization-time snapshot. `deno_resolver` 0.88 does not honor
  `NPM_CONFIG_USERCONFIG`.
- **Phase 2 resource boundaries**: child requests, permission-broker lines,
  captured-output readers, subprocess handshake writers, tarball metadata, and
  lifecycle scripts now have bounded failure paths instead of silently growing
  or waiting forever. These bounds are compatibility safeguards, not process
  isolation guarantees.

### Security / Correctness

- **Bounded child requests**: child-mode requests are capped at 1 MiB, and the
  parent validates the serialized request before spawning the child.
- **Failure propagation**: capture-reader and fd failures, negative
  `NODE_CHANNEL_FD` values, and random-suffix RNG failures are surfaced rather
  than silently treated as valid state.
- **Fail-closed permission hooks**: unwinding panics from an in-process
  permission hook are caught at the bridge boundary and return deny. Builds
  using `panic = "abort"`, or panic hooks that abort/exit, cannot be recovered
  by `catch_unwind` and may still terminate the host.
- **Documented boundaries**: heap and execution-deadline controls are
  best-effort; the external broker's upstream `exit(87)` behavior and
  descendant-process limitations are explicit.

### Compatibility / API

- **Python PathLike support**: Python bindings accept `str`, `bytes`, and
  custom `PathLike` values while preserving platform path semantics.
- **Permission and subprocess boundary coverage**: added negative capability,
  symlink-escape, and subprocess-boundary tests.
- **Reusable output documentation**: corrected examples to use the public
  `libdeno::runtime::run_with_output` path.

### Performance

- **Benchmark measurement**: benchmark coverage is available for the relevant
  run paths, but no production performance rewrite was made without evidence.
  Further graph, cache, isolate, and global-cache optimization work remains
  deferred.

### Known limitations

- There is no cross-platform process-tree / Windows Job Object kill-tree
  guarantee and no subprocess wall-time API.
- Captured output is still returned only on success; partial output is not
  carried with an error.
- External broker upstream constructor and communication `exit(87)` paths are
  not catchable as `LibdenoError`.
- The offline Deno/TypeScript version ledger checks local declarations only;
  matching the corresponding upstream Deno tag remains a manual review step.
- Trusted Publishing required reviewers and environment tag/repository
  restrictions are configured outside this repository's workflow YAML.
- Rust/crates.io and Python/PyPI publication is not atomic; a later retry may
  be needed if one publisher succeeds before the other fails.

## 0.3.0

> **Breaking (runtime behavior)**: the process cwd is never switched anymore
> and in-process runs are no longer globally serialized. `LibdenoOptions.cwd`
> is now a resolution base only, and output capture is exclusive — overlapping
> runs are rejected instead of queued. Both changes are intentional
> consequences of allowing ordinary runs to execute in parallel; migration
> notes below.

### Breaking changes (migration guide)

- **The process cwd is never switched.** `CWD_LOCK` and `CwdGuard` are
  removed. `LibdenoOptions.cwd` is now a **resolution base only**: relative
  paths for entry resolution, permission grants, and `node_modules` discovery
  still resolve against it, but the script at runtime observes the host's cwd
  — `Deno.cwd()` / `process.cwd()` and relative filesystem operations inside
  the script resolve against the host process's cwd. Migration: scripts that
  relied on seeing `options.cwd` as their working directory should use
  absolute paths, or run through `run_in_subprocess` (the child's cwd is
  pinned at spawn).
- **Ordinary in-process runs are fully parallel.** Each run owns its thread,
  isolate, and graph and shares nothing mutable; the old process-global
  serialization is gone, so runs no longer queue behind each other.
- **Output capture is exclusive.** Capture is fd-level redirection of the
  process-global stdout/stderr, so a captured run rejects **any** concurrent
  run (captured or not) with `LibdenoError::Configuration` instead of letting
  the capture reader steal its output. Enforced by a lock-free atomic state
  machine (`RunLease` / `RUN_STATE`) with no spurious serialization; `run` /
  `run_with_output` / `run_async` / `run_with_output_async` / `run_with` /
  `runtime::run_with_output` all take the lease. Previously overlapping
  captured runs queued on the mutex. Migration: for captured runs alongside
  parallel execution use `run_in_subprocess_with_output`, which pipes the
  child's own fds back to the parent.
- **`run_in_subprocess` no longer takes a cwd lock** (it existed only to guard
  against concurrent chdir, which no longer exists); the child's cwd is still
  pinned at spawn via `Command::current_dir`. A long-lived child can no longer
  hold a process-global lock that blocks every other run.
- **`run_in_subprocess` forwards `features`, `max_heap_bytes`, and
  `execution_deadline` to the child** (previously silently dropped — the
  child ran on the full default unstable surface with no bounds). Capture
  flags are deliberately not forwarded: the child writes to the inherited
  fds; use `run_in_subprocess_with_output` to pipe them back.
- **`run_with` rejects what it cannot honor** (previously a silent ignore):
  `capture_stdout` / `capture_stderr` are refused with
  `LibdenoError::Configuration` (it returns only the exit code — use
  `runtime::run_with_output`), and a `LibdenoOptions.cwd` that does not match
  the runtime's directory (canonicalize-aware comparison) is refused the same
  way; omit `cwd`, or build the runtime for that directory.

### Added

- **`run_async` / `run_with_output_async`**: async entry points that execute
  the run on the **caller's** tokio runtime — no spawned thread, removing
  the per-run OS-thread cost of `run()`'s tokio re-entry escape. Must be
  called from inside a tokio context; the future is not `Send` and must not
  be interleaved with another `run_async` (a V8 isolate is pinned to its
  creating thread — v8 0.150 `PinnedRef` — so interleaved runs abort the
  process; enforced by a thread-local RAII guard that rejects a second
  `run_async` on one thread with `Configuration`). `execution_deadline`
  needs the caller runtime's time driver (`enable_time`/`enable_all`);
  parallel runs use `run()` or `run_in_subprocess`.
- **`run_in_subprocess_with_output`**: subprocess-mode output capture — the
  child's own stdout/stderr fds are piped back to the parent and read
  concurrently with `wait()`. Per-process capture: runs in parallel with any
  other run (no exclusivity), works on Windows, both streams always returned,
  `max_capture_bytes` caps each stream (excess drained + dropped,
  `capture_truncated` set). All other options forward like
  `run_in_subprocess`.
- **Child-mode env strip**: `maybe_handle_child_mode` removes
  `LIBDENO_CHILD_MODE` / `LIBDENO_CHILD_TOKEN` before running the script, so
  subprocesses the script spawns (git, compilers) inherit a clean environment
  instead of entering child mode with a consumed stdin.
- **deno_resolver `sync` feature** (resolver stack is `Send`-capable, deno
  CLI parity; behavior-equivalent — this is the enabling step for future
  async work, no API change).
- **Concurrency-protocol tests**: parallel ordinary runs overlap in time,
  captured runs reject concurrent runs (`Configuration`), many parallel runs
  all succeed, subprocess option forwarding (features / execution_deadline),
  async entry points on current-thread and multi-thread (`LocalSet`)
  runtimes.

### Known limitations

- **Captured output is lost when a run errors**: `run_with_output` /
  `run_with_output_async` / `run_in_subprocess_with_output` return the
  captured bytes only on success; on an error the partial output is dropped
  (an output-on-error API is not shipped yet).
- **`#[non_exhaustive]` is not applied** to `LibdenoOptions` / `RunOutput`:
  adding fields in future releases stays a source-breaking change for code
  using full struct literals or exhaustive matches.
- **No env-injection option yet**: scripts see the host's environment; there
  is no per-run `env` setting on `LibdenoOptions`.
- **The analysis cache clears entirely on overflow** (default 8192 entries,
  `LIBDENO_ANALYSIS_CACHE_ENTRIES`): a full reset is cheaper and more
  predictable than an LRU on this hot path, and a clear only costs the next
  run one rebuild (deliberate — see `src/analysis_cache.rs`).
- **Disk code cache is untested and keyed with `DefaultHasher`**, whose
  output is not stable across rustc upgrades: after a toolchain update the
  on-disk entries are simply recompiled (a wasted compile, never a wrong hit
  — perf-only).
- **`features` behavior is covered**: tests cover default and custom
  in-process feature sets and subprocess forwarding.

## 0.2.2

> **Breaking (source-compat)**: `LibdenoOptions` gained `max_capture_bytes`
> and `features` fields, `RunOutput` gained `capture_truncated`, and
> `runtime` is now a public module — code constructing these structs with
> full field literals (or matching exhaustively) must be updated. No runtime
> behavior changes for existing option sets.

### Added

- **`LibdenoOptions::max_capture_bytes`**: per-stream cap on captured output
  (stdout and stderr each get the budget). When a stream exceeds it, capture
  stops, the excess is dropped, and `RunOutput::capture_truncated` is set —
  a verbose or hostile script can no longer grow host memory without limit.
- **`LibdenoOptions::features`**: overrides the default unstable-feature set
  (`kv`, `cron`, `ffi`, `webgpu`, `worker-options`) that gates JS namespace
  IDs and feature checks. An embedder running untrusted plugins can shrink
  the surface (e.g. `Some(vec!["ffi".into()])`); ops stay permission-gated
  regardless. `worker-options` is always enabled even when omitted — without
  it `new Worker(...)` with worker options terminates the host process.
- **Capture on the reusable runtime**: `libdeno::runtime::run_with_output()`
  is the `LibdenoRuntime` equivalent of `run_with_output()` — long-lived
  hosts get both resolver-stack reuse and per-run captured output. `run_with`
  itself still returns only the exit code; its capture flags are documented
  as unsupported (previously a silent no-op).

### Fixed

- **Output capture no longer hangs on a *writing* child** (macOS/Windows
  daemonized children): the finish wait is now a total 500 ms budget from
  entry, not a per-block idle timeout — a child that keeps writing (e.g. a
  logging daemon) previously reset the idle timer on every block and stalled
  the caller forever.
- **No fd-reuse corruption on capture teardown**: the original fd is now
  `take()`n on restore, so a stale fd number (reused by the OS for another
  file while a detached child stalled the drain) can never be dup2'd back
  onto the host's stdout/stderr.
- **Restore failure aborts instead of silently wedging**: if the captured
  fd cannot be restored, the host's stdio would be permanently swallowed;
  the process now aborts with a clear message rather than continuing
  corrupted.
- **Reader thread buffer raised to 64 KiB** (was 8 KiB); on hitting
  `max_capture_bytes` the crossing block keeps the bytes that fit and the
  thread keeps draining (discarding) instead of closing the read end — a
  verbose script's later writes keep working instead of failing with EPIPE,
  and the buffer never exceeds the cap.
- **`LibdenoOptions` / `RunOutput` rustdoc synced** (`capture_truncated`,
  byte-cap semantics) and worker-thread panic messages no longer claim to
  come from `run()` specifically.
- `docs/api.md` now documents `capture_stdout`/`capture_stderr`/
  `max_capture_bytes`/`features` and the `LibdenoRuntime`/`run_with`/
  `runtime::run_with_output` family.

### Performance

- **Cross-run module analysis cache**: deno_graph's `ModuleAnalyzer` +
  `ModuleInfoCacher` seams are now backed by a process-global cache keyed by
  (specifier, source hash), so warm runs — on every entry point, `run` or
  `run_with` — skip re-parsing and re-analyzing the transitive graph
  (previously the dominant per-run cost after the resolver stack was made
  reusable). Size-capped; cap configurable via
  `LIBDENO_ANALYSIS_CACHE_ENTRIES` (default 8192; overflow clears the cache).
- **CJS analysis cache wired in**: the in-memory CJS analysis cache (content-
  hash keyed, process-global) is enabled on the resolver stack's
  `NodeAnalysisCache` seam. The node resolution / package.json caches stay
  `None` deliberately: upstream's thread-local stores are keyed by path only
  with no invalidation path, so in-process filesystem changes between runs
  (an `npm install`, an edited package.json) would serve stale resolutions.
- **npm tarball ISIZE guard**: gzip tarballs whose ISIZE trailer claims a
  decompressed size above the budget are rejected at download time, closing
  the documented host-OOM exposure for standard npm registry tarball URLs.
  Budget configurable via `LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES` (default
  1 GiB). Coarse by design: the check keys off the `.tgz` URL path (a
  registry that serves tarballs without the extension bypasses it), and
  multi-member gzip files can still decompress past a single member's ISIZE —
  upstream's streaming fallback (no reservation) remains the precise bound.
- **Disk-backed V8 code cache**: compiled script bytes now survive process
  restarts — CLI-style hosts (every npm-plugin invocation is a fresh process)
  skip recompilation on cold starts. Located at `LIBDENO_CODE_CACHE_DIR` or
  `<DENO_DIR>/code_cache`; without either the cache stays in-memory (tests
  are always in-memory). Keyed by (specifier, type, source hash) so stale or
  cross-project entries can never be served; V8 validates code-cache data
  itself, and all disk I/O is best-effort (a read-only cache dir never fails
  a run). Directory hygiene: wiped when it exceeds the in-memory entry cap.

### Tests

- ISIZE guard unit tests (reject inflated trailer, pass small trailer, ignore
  non-tarball paths and non-gzip bytes).
- Cross-run analysis-cache invalidation test: a source change must drop the
  cached dependency list (v1 imports a dep, v2 drops it with the dep removed,
  v3 adds a new dep — all three runs must behave correctly).
- Windows-only test asserting capture is rejected with a `Configuration`
  error (runs on the Windows CI leg).
- CI job verifying the pinned `DENO_VERSION` / `TS_VERSION` constants exist
  upstream (deno tag + typescript npm version).

## 0.2.1

### Added

- **Output capture**: `run_with_output()` returns a `RunOutput` (`exit_code` +
  captured `stdout`/`stderr` bytes) when `LibdenoOptions::capture_stdout` /
  `capture_stderr` are set. The capture is fd-level and process-global for the
  duration of the run — other host threads printing during the run land in the
  captured buffer too (runs are serialized internally).
- **Tokio re-entry is handled automatically**: `run()` / `run_with()` /
  `run_with_output()` no longer error when called from inside a tokio runtime;
  they execute on a fresh thread instead. Async hosts (tokio/axum) previously
  had to build a `std::thread::spawn` + `join` escape themselves.
- **`LibdenoError::Configuration`** variant for host/configuration-level
  problems (e.g. an empty permission list without `allow_all_permissions`).
  The error message now spells out the v0.2.0 semantic change: an empty list
  no longer grants everything.

## 0.2.0

### Breaking changes (migration guide)

- **Empty permission list semantics inverted** (0.1.4 → 0.2.0): in 0.1.x an
  empty `LibdenoOptions.permissions` list granted *everything* (fail-open);
  since 0.2.0 it is a construction error unless `allow_all_permissions` is set
  (fail-closed). Migration: set `allow_all_permissions: true` for the old
  behavior, pass explicit `--allow-*` flags, or set `prompt: true` for
  interactive prompting.
- **`LibdenoError::Js` removed**: JS exceptions now surface as
  `LibdenoError::Runtime` / `LibdenoError::Core`.
- **`LibdenoError::Timeout` payload changed** from `Duration` to `String` (the
  human-readable reason: deadline exceeded, or subprocess handshake timeout).
- **Static file imports are permission-gated**: a static `import "./x.js"` now
  goes through the same read-permission check as dynamic access, honoring the
  `--allow-read` scope and the permission broker/hook. Previously such imports
  were served unconditionally.
- **`--allow-import` is accepted again** (it was rejected in 0.1.4): remote
  (`https:`/`jsr:`) module loading is gated by it, matching the deno CLI. There
  is no `--allow-net` fallback for module loading.
- **Web workers use their own permission container**: module loading inside
  `new Worker()` is gated by the worker's grants, not the main run's. The
  `worker-options` unstable feature is enabled.

### Added

- Fail-closed permission model with `prompt` mode (mirrors `deno run`).
- Permission broker / hook (`install_permission_hook`,
  `run_in_subprocess` child mode) with token authentication, deadline, and
  stdin-EOF semantics.
- Native addon (`require(".node")`) support verified end-to-end; `--allow-ffi`
  gates addon loading, matching `deno run`.
- Subprocess parent-side write deadline: a host that never services child mode
  yields `LibdenoError::Timeout` instead of hanging.
- 90+ tests including broker E2E, hook install semantics, child-mode auth,
  and install-once/mutual-exclusion coverage.
