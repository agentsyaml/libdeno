# Security Policy

## Trusted Execution Model — read this first

libdeno **executes the JavaScript/Node code the host embeds it with**, by
design. The library itself provides **no in-process memory-safety boundary for
untrusted code**, for two reasons that are both upstream (deno stack) issues:

- The embedded V8 (`v8` 150.x, June 2026 baseline) trails upstream by ~2 major
  versions. The Chrome 151 series carries 17+ publicly disclosed high-severity
  CVEs (use-after-free / type confusion, CVSS ~8.8) that are unfixed in this
  baseline.
- `deno_core` does not enable the V8 sandbox — the `v8_enable_sandbox` build
  flag is not passed through — so a triggered memory corruption can escape the
  V8 heap.

libdeno tracks the rolling deno stack releases and inherits fixes as they land;
there is no action libdeno itself can take to close these gaps.

**Therefore: only feed code you trust into the runtime.** If you must execute
untrusted third-party code (user input, external npm packages), wrap the host
in a process-level sandbox: a container, seccomp, or Apple Seatbelt. The
permission flags alone (see [Resource limits](#resource-limits)) are not a
security boundary against memory-unsafety in the interpreter.

## Dependency Pin Trade-off

Since **v0.2.0**, the crates whose types cross the libdeno/deno stack
boundary directly — `deno_error`, `deno_graph`, `deno_semver`,
`deno_media_type` — are **exact-pinned** to match the deno stack's own
internal pins (e.g. `deno_resolver` 0.88.0 pins `deno_graph` =0.110.1 and
`deno_semver` =0.10.1; `deno_core`/`deno_runtime` pin `deno_error` =0.7.1).
Without the exact pin, a `cargo update` can resolve a second version of one
of these crates, and the duplicated types produce unlocatable
trait-mismatch compile errors. All other deno dependencies keep caret
ranges to preserve downstream resolution flexibility.

libdeno's own guardrails are:

- CI runs `cargo audit --deny warnings` against the committed `Cargo.lock`,
  with the upstream-tracked entries listed below ignored in
  [`.cargo/audit.toml`](.cargo/audit.toml).
- Every CI build and the crates.io publish run with `--locked`, so the lockfile
  cannot silently drift from what was reviewed.

If you need stronger guarantees, pin your own lockfile and run `cargo-vet` /
`cargo-deny` in your own pipeline.

## Permission Defaults — v0.2.0 Breaking Change

Since **v0.2.0**, an **empty** `LibdenoOptions.permissions` list **no longer
implicitly grants all permissions**: `run` / `run_with` /
`run_in_subprocess` return `LibdenoError::Configuration` instead.

Embedders must do one of the following:

- pass explicit `--allow-*` capability strings in `permissions` (e.g.
  `["--allow-read", "--allow-env"]`), or
- set `LibdenoOptions.allow_all_permissions = true` (the `-A` /
  `--allow-all` capability string remains equivalent).

This closes the "forgot to pass permissions and silently got everything
open" footgun. Releases ≤0.1.4 defaulted to allowing everything. See
[docs/permissions.md](docs/permissions.md) for details.

Beyond the default stance, libdeno also exposes:

- `LibdenoOptions.prompt` — mirrors `deno run`'s interactive prompts:
  non-granted checks print to stderr and read allow/deny from stdin; an empty
  `permissions` list with `prompt: true` asks for every access instead of
  erroring.
- `install_permission_broker` / `install_permission_hook` — install a
  process-global permission decision hook (install-once). Once installed it is
  the **sole authority** for all permission checks in the process, overriding
  the flags. Hooks are closures and cannot be passed across a subprocess
  boundary; cross-process decision-making uses the filesystem-socket broker
  instead.

None of these change the core conclusion above: permissions are not a
memory-safety boundary.

## Known Upstream-Tracked Advisories

Currently ignored in `.cargo/audit.toml` because only the upstream deno stack
can fix them. libdeno follows rolling updates and removes an entry once the fix
flows into the tree.

| Advisory | Crate | Issue | Upstream |
|---|---|---|---|
| RUSTSEC-2026-0118 | hickory-proto 0.25.x | NSEC3 denial-of-service, `patched = []` | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2026-0118) / [GHSA-3v94-mw7p-v465](https://github.com/hickory-dns/hickory-dns/security/advisories/GHSA-3v94-mw7p-v465) |
| RUSTSEC-2026-0119 | hickory-proto 0.25.x | Denial-of-service, fixed `>= 0.26.1` (deno main not moved off 0.25) | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2026-0119) / [GHSA-q2qq-hmj6-3wpp](https://github.com/hickory-dns/hickory-dns/security/advisories/GHSA-q2qq-hmj6-3wpp) |
| RUSTSEC-2023-0071 | rsa 0.9.x | CVE-2023-49092 (Marvin timing attack), `patched = []` — no stable fixed release | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2023-0071) / [RustCrypto/RSA#626](https://github.com/RustCrypto/RSA/issues/626) |
| RUSTSEC-2026-0285 | rustls 0.23.40 | TLS 1.3 handshake messages incorrectly accepted across encryption-level boundaries, fixed `>= 0.23.45` — `deno_tls` 0.244.0 exact-pins `rustls = "=0.23.40"` | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2026-0285) |

Informational (no vulnerability): bincode 1.x (RUSTSEC-2025-0141),
rustls-pemfile (RUSTSEC-2025-0134), smartstring (RUSTSEC-2026-0249), paste
(RUSTSEC-2024-0436) — all unmaintained; rand 0.8 (RUSTSEC-2026-0097) —
unsound, no fix. All are deno-stack transitive dependencies; libdeno cannot
replace them.

Separately, **rusty_v8** (the V8 binding) rolls continuously: the exact V8
version shipped in a given libdeno release depends on the
`deno_core`/`deno_runtime` pin.

## Reporting a Vulnerability

Private vulnerability reporting is **not yet enabled** on this repository.
Please report via:

- **GitHub Issues**: <https://github.com/agentsyaml/libdeno/issues> — open an
  issue, prefer including the `security` label and as much detail as possible
  (affected version, repro, impact).

Alternatively, contact the maintainers and ask to enable **Private
vulnerability reporting** (repo → Settings → Code security and analysis →
Private vulnerability reporting); once enabled, the private channel is the
preferred route for anything that could be exploited before disclosure.

## Resource Limits

libdeno exposes the Deno CLI's permission model via `--allow-*` capability
strings (`--allow-read`, `--allow-write`, `--allow-net`, `--allow-env`,
`--allow-run`, `--allow-ffi`, `--allow-sys`, `--allow-import`), enforced
per-operation through deno_runtime's `PermissionsContainer`. Web workers carry
the permissions captured at `new Worker(...)` time.

The resource options are in-process, best-effort controls, not a security
boundary:

- `max_heap_bytes` requests a V8 old-generation heap ceiling for the isolate.
  Values below 8 MiB are rejected, but the control remains best effort. It
  does not cap total V8 memory, native allocations, V8 external memory, host
  allocations, RSS, CPU, or memory used by child processes. It is not a cgroup,
  container, or OS-enforced process-memory limit.
- `execution_deadline` can interrupt JavaScript when V8 reaches an
  interruptible stack check. It cannot interrupt a blocking syscall, a wait on
  a child process, native code, or a blocked permission broker/hook, and it
  cannot bound host-side memory or other work after the run leaves an
  interruptible V8 frame. A run can therefore exceed the requested deadline;
  this is not a CPU-time limit and does not replace the HTTP client's separate
  transport budget.

Output capture has its own boundary:

- In-process fd capture is process-global and exclusive. Without
  `max_capture_bytes`, each captured stream is unbounded for the duration of
  the run and can grow host memory; the per-stream byte cap retains only the
  configured prefix and marks truncation. In-process capture is rejected on
  Windows.
- `run_in_subprocess_with_output` pipes and drains the direct child's stdout
  and stderr on every platform. Its cap bounds the retained parent buffers,
  not the child's CPU, memory, or descendants.
- Execution-control supervisor capture is bounded separately because terminal
  output retention is configured independently: `None` retains at most 64 KiB
  per requested stream, explicit values up to 96 KiB are honored, and larger
  explicit values are rejected before spawning. Supervisor `TERMINAL` frames
  do not carry stdout/stderr; the parent drains the capture pipes. The separate
  1 MiB control-frame limit is not the reason for these retention values. This
  does not change legacy or in-process capture semantics above.

Network and package-install guards are also finite but not isolation controls:

- Remote module bodies and npm metadata are capped at 256 MiB in the body
  returned by reqwest after transport decoding, not at the wire; upstream
  decoding may allocate before this check. Explicit npm `.tgz` paths are
  capped at 1 GiB of reqwest-returned, transport-decoded response-body bytes,
  also not wire bytes. For a gzip response on that path, libdeno checks the
  final four body bytes as an `ISIZE` hint: the final member's size modulo
  2^32. The default 1 GiB threshold (`LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES`
  overrides it) is not an output budget and does not identify the first
  consumed member or total extraction. The downstream fallback has no hard
  decompressed-output budget; wrapper and multi-member cases are not precisely
  accounted. This is not an RSS or decompression hard limit.
- Each HTTP operation has a 300-second wall-clock budget covering retries,
  backoff, applicable redirects, and body reads. This does not impose a CPU or
  process lifetime limit on the runtime.
- npm lifecycle scripts are currently disabled. This contract does not promise
  that a system shell executes them by default.

None of these options provides CPU isolation, a complete execution-time limit,
an RSS cap, or an in-process sandbox. They do not replace a process-level sandbox and resource
policy (for example a container, cgroup, seccomp, Apple Seatbelt, or suitable
`ulimit`) for untrusted code — see [Trusted Execution Model](#trusted-execution-model--read-this-first).

## Subprocess and broker limits

`run_in_subprocess` isolates the direct child from the host's `Deno.exit` and
hard runtime termination, but it does not guarantee process-tree containment.
Descendants spawned by the script can outlive the direct child, retain file
descriptors, or continue consuming CPU and memory; libdeno does not provide a
cross-platform process-group / Windows Job Object kill-tree guarantee. Use an
OS-level supervisor or sandbox when descendant containment matters.

On Windows, cancellation of an overlapped write can wait beyond the requested
deadline while completion is observed so kernel-owned buffers remain valid.
This is a safety requirement, not a hard real-time cleanup guarantee.

Node `child_process.fork` hosts must explicitly initialize IPC during trusted
startup with `initialize_node_ipc_from_env()`, before creating threads. The
unsafe call takes exclusive ownership of a trusted, connected raw endpoint;
it is a one-shot attempt, and a claimed endpoint is closed on subsequent
validation failure. Ordinary runs do not claim or rewrite the environment
channel. Never treat an untrusted `NODE_CHANNEL_FD` value as authority to claim
an OS handle; see [the API contract](docs/api.md#initialize_node_ipc_from_env).

`install_permission_broker` and `install_permission_hook` are process-global,
install-once, synchronous decision paths, not isolation mechanisms. A blocked
hook or broker stalls permission checks; an unwinding panic in an in-process
hook is caught at the bridge boundary and fails closed as deny. A `panic =
"abort"` build or a panic hook that aborts/exits cannot be caught by
`catch_unwind` and may terminate the host. The upstream
`deno_permissions::PermissionBroker::new` constructor
may call `process::exit(87)` on initial connection failure, and later broker
communication failures may also terminate through the upstream error path.
These exits are not catchable as `LibdenoError`, so do not use an untrusted or
unreliable broker endpoint where the host must remain alive.
