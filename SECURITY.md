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

libdeno depends on the deno stack with **caret ranges** (e.g. `deno_graph
^0.110.1`) to preserve downstream resolution flexibility — this was
deliberately relaxed after a downstream version conflict. Stricter supply-chain
practices (exact pins, `cargo-vet`, `cargo-deny`) are therefore a
**downstream-consumer decision**, made at the consumer's lockfile level.

libdeno's own guardrails are:

- CI runs `cargo audit --deny warnings` against the committed `Cargo.lock`,
  with the upstream-tracked entries listed below ignored in
  [`.cargo/audit.toml`](.cargo/audit.toml).
- Every CI build and the crates.io publish run with `--locked`, so the lockfile
  cannot silently drift from what was reviewed.

If you need stronger guarantees, pin your own lockfile and run `cargo-vet` /
`cargo-deny` in your own pipeline.

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
