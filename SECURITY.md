# Security Policy

## Execution model

libdeno executes the JavaScript and TypeScript supplied by its host inside the
host process. Treat loaded code as trusted: Deno permissions restrict runtime
operations, but do not isolate V8 or protect the host from interpreter memory
corruption. Run untrusted code in a separate process with an OS-level sandbox
and resource policy, such as a container, seccomp, or Apple Seatbelt.

## Dependency updates

CI and crate publication run `cargo audit --deny warnings` against the checked-in
lockfiles; `.cargo/audit.toml` lists upstream advisories that currently lack a
usable fix in the Deno dependency stack. Builds and publication use `--locked`.
Applications should also review and audit their own lockfiles and sandbox code
they do not trust.

## Permissions

Permissions are opt-in. Supply explicit `--allow-*` capabilities; an empty
`LibdenoOptions.permissions` list returns `LibdenoError::Configuration` unless
`prompt` or `allow_all_permissions` is enabled. `-A` / `--allow-all` grants
every capability and should be reserved for trusted code. `prompt: true` asks
for decisions interactively.

`install_permission_broker` and `install_permission_hook` install a
process-global, install-once decision path that becomes the authority for
permission checks. Hooks cannot cross a subprocess boundary; use the broker
when decisions must involve another process. Install before runs or between
runs after workers stop: libdeno rejects installation during a run and rejects
new runs while installation is underway. Direct upstream/runtime broker
installations are outside this coordination and require caller-side
synchronization. See [Permission behavior](docs/permissions.md).

`LibdenoError::is_permission_error()` trusts concrete Rust permission error
types carried by `Runtime` or `Core` errors. JavaScript exception names, class
labels and `cause` fields do not identify permission denials. The Python API
uses this classification when mapping `Runtime` / `Core` errors to
`DenoPermissionError`.

## Known Upstream-Tracked Advisories

The existing entries are ignored in `.cargo/audit.toml` because only the
upstream Deno stack can fix them. Advisories without a static ignore use
separate, expiring CI exceptions described after the table. Remove exceptions as
fixes flow into the tree.

| Advisory | Crate | Issue | Upstream |
|---|---|---|---|
| RUSTSEC-2026-0118 | hickory-proto 0.25.x | NSEC3 denial-of-service, `patched = []` | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2026-0118) / [GHSA-3v94-mw7p-v465](https://github.com/hickory-dns/hickory-dns/security/advisories/GHSA-3v94-mw7p-v465) |
| RUSTSEC-2026-0119 | hickory-proto 0.25.x | Denial-of-service, fixed `>= 0.26.1` (deno main not moved off 0.25) | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2026-0119) / [GHSA-q2qq-hmj6-3wpp](https://github.com/hickory-dns/hickory-dns/security/advisories/GHSA-q2qq-hmj6-3wpp) |
| RUSTSEC-2023-0071 | rsa 0.9.x | CVE-2023-49092 (Marvin timing attack), `patched = []` — no stable fixed release | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2023-0071) / [RustCrypto/RSA#626](https://github.com/RustCrypto/RSA/issues/626) |
| RUSTSEC-2026-0285 | rustls 0.23.40 | TLS 1.3 handshake messages incorrectly accepted across encryption-level boundaries, fixed `>= 0.23.45` — `deno_tls` 0.246.0 exact-pins `rustls = "=0.23.40"` | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2026-0285) |
| RUSTSEC-2026-0346 | rustls 0.23.40 | Custom session-store `Debug` output can expose TLS secrets, fixed `>= 0.23.46` | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2026-0346) |
| RUSTSEC-2026-0348 | rustls 0.23.40 | Brotli certificate decompression can allocate up to 1 GiB before authentication, fixed `>= 0.23.46` | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2026-0348) |
| RUSTSEC-2026-0349 | rustls 0.23.40 | Unauthenticated ServerHello can downgrade later connections to classical key exchange, fixed `>= 0.23.46` | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2026-0349) |
| RUSTSEC-2026-0387 | crypto-bigint 0.5.5 | Secret-dependent branches in supposedly constant-time arithmetic, fixed `>= 0.7.6` — `elliptic-curve` 0.13 in the Deno stack still depends on crypto-bigint 0.5 | [rustsec.org](https://rustsec.org/advisories/RUSTSEC-2026-0387) |

The CI wrapper (`audit-with-temporary-exceptions.py`) applies time-limited
exceptions, each only while its exact pinned version is in the lockfile:

| Exception | Advisories | Expires (UTC) |
|---|---|---|
| rustls 0.23.40 | RUSTSEC-2026-0346, -0348, -0349 | 2026-11-17 |
| crypto-bigint 0.5.5 | RUSTSEC-2026-0387 | 2026-12-31 |
| zstd 0.13.2, zstd-safe 7.2.4, zstd-sys 2.0.16+zstd.1.5.7 | RUSTSEC-2026-0391, -0395, -0396, -0397, -0398, -0399, -0400, -0401, -0402, -0403, -0404, -0405, -0406, -0410 | 2026-11-26 |

On or after an expiry date the wrapper fails the audit instead of ignoring the
advisory; other audit findings remain fatal throughout. These are real risks:
custom session-store logs may expose secrets, Brotli-enabled certificate
processing can be exhausted by a malicious peer, affected connections may lose
post-quantum key exchange, and secret integers used in the affected
crypto-bigint operations may leak through timing. Remove each exception as soon
as an upstream Deno update moves past the pinned version (rustls >=0.23.46,
crypto-bigint >=0.7.6). The accepted zstd risks include memory-safety defects in
streaming/seekable APIs and a WebAssembly allocator-shim corruption on allocation
failure; they are not fixed locally. Deno node 0.197.0 pins this chain, and 0.198.0
plus upstream main still retain it, so the fixes remain unavailable through
Deno. Remove this exception when upstream Deno selects zstd >=0.14.3,
zstd-safe >=8.2.0, and zstd-sys >=2.1.2, or by 2026-11-26.

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

## Runtime boundaries

`--allow-*` permissions are checked per operation. A Web Worker created without
explicit `deno.permissions` shares the parent's live permission container, so
parent revocations apply inside it; a Worker created with explicit permission
arguments gets a forked copy that later parent changes do not affect.

`max_heap_bytes` requests a best-effort V8 old-generation heap limit (minimum
8 MiB) for the main worker and Web Workers. It does not cap total V8/native/host
memory, RSS, CPU, or child-process memory. `execution_deadline` can interrupt
JavaScript at V8 interrupt checks, but not blocking system calls, native code,
subprocess waits, or blocked permission hooks. A run can outlast the deadline;
neither option is an OS resource limit.

In-process stdout/stderr capture redirects process-wide file descriptors and
allows only one run at a time; host threads writing to those descriptors are
captured too. `max_capture_bytes` limits each stream, but `None` leaves
in-process capture unbounded. In-process fd capture is unavailable on Windows.

`run_in_subprocess_with_output` pipes the direct child's streams on all
platforms and can run alongside other calls. Its cap limits retained output,
not child resources or descendants. The execution-control supervisor retains
64 KiB per stream by default and accepts explicit limits up to 96 KiB.

HTTP module bodies and npm metadata have a 256 MiB cap on bytes returned by
reqwest after transport decoding. Explicit npm `.tgz` responses have a 1 GiB
cap on the returned (still compressed) body. For gzip tarballs, an `ISIZE`
precheck compares the final four bytes—the final member's size modulo 2^32—to
`LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES` (default 1 GiB). This is only a hint:
it does not bound total decompressed output, including multi-member archives.
Upstream decoding can allocate before body checks. Each HTTP operation has a
300-second wall-clock budget, including retries and body reads; it is separate
from `execution_deadline`. npm lifecycle scripts are disabled by default.

These limits do not provide CPU or RSS isolation. Use an OS-level sandbox and
resource policy for untrusted code.

## Subprocess supervision

`run_in_subprocess` confines `Deno.exit` and runtime termination to the direct
child. It does not kill or contain the full descendant process tree; use an
OS-level supervisor when descendant cleanup matters. On Windows, waiting for
an overlapped write to finish can extend cleanup beyond the requested deadline.

Node `child_process.fork` hosts must call unsafe
`initialize_node_ipc_from_env()` from trusted startup code before creating
threads. The launcher must supply a valid connected `NODE_CHANNEL_FD` and
transfer unique ownership of its handle. Initialization is a one-time attempt,
including failures; after a handle is claimed, later validation failures close
it. Ordinary runs do not claim the environment channel. Never treat an
untrusted environment value as authority to claim an OS handle; see [the API
contract](docs/api.md#initialize_node_ipc_from_env).

Permission brokers and hooks are synchronous, process-global, install-once
decision paths. They override permission flags; a blocked callback stalls
permission checks. Panics that unwind from an in-process hook fail closed as a
denial, but aborting panic behavior can terminate the host. The upstream broker
can also terminate the process on connection failures; these exits cannot be
returned as `LibdenoError`.
