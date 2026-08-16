# Changelog

All notable changes to libdeno. Following the [keep a changelog] convention;
breaking changes are highlighted per release with migration notes.

[keep a changelog]: https://keepachangelog.com/en/1.1.0/

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
