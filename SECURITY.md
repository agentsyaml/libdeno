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
`run_in_subprocess` return a permission error instead.

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

What it does **not** provide: no V8 heap/memory cap, no CPU limit, no
execution-time limit, and no in-process sandbox. For untrusted code, add
process-level limits (container / cgroup / `ulimit`) on top of the permission
flags — see [Trusted Execution Model](#trusted-execution-model--read-this-first).
