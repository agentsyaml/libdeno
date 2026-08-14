# Changelog

All notable changes to libdeno. Following the [keep a changelog] convention;
breaking changes are highlighted per release with migration notes.

[keep a changelog]: https://keepachangelog.com/en/1.1.0/

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
