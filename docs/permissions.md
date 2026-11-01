# Permissions

libdeno's permission model mirrors the Deno CLI: capability strings in
`--allow-*` format, parsed into a `PermissionsContainer`.

## Default stance

An empty `permissions` list is a configuration error unless `prompt: true` or
`allow_all_permissions: true` is set. `allow_all_permissions` and `-A`/
`--allow-all` grant every capability; use them only for trusted code.

`allow_all_permissions: true` takes precedence over `permissions`: a
non-empty list is not applied, it is discarded. A host that fills one field from
a policy decision and the other from a per-request flag therefore gets the wider
result, not the intersection. Set capabilities in one place.

Permission flags grant only the declared capabilities. A flag without a value
grants the capability globally; comma-separated values scope it, e.g.
`--allow-read=./src,./public`.

## Supported flags

| Flag | Value (comma-separated) | Example |
|---|---|---|
| `--allow-read` | paths | `--allow-read=./src,./public` |
| `--allow-write` | paths | `--allow-write=/tmp` |
| `--allow-env` | env var names | `--allow-env=HOME,PATH` |
| `--allow-net` | hosts, optionally `:port` | `--allow-net=example.com:8080` |
| `--allow-import` | import hosts (see note) | `--allow-import=deno.land` |
| `--allow-run` | executable names or paths | `--allow-run=git` |
| `--allow-ffi` | native library paths | `--allow-ffi=./libfoo.so` |
| `--allow-sys` | system API names | `--allow-sys=getpid` |
| `-A` / `--allow-all` | — | allow everything (same as `allow_all_permissions: true`) |

`--allow-import` gates remote module loading (`https:` and `jsr:`); `--allow-net`
does not grant module-import permission. Values are host descriptors, such as
`deno.land` or `jsr.io`.

**No import host is granted by default.** The `deno run` CLI pre-populates
`--allow-import` with `deno.land:443`, `jsr.io:443`, `esm.sh:443`,
`raw.esm.sh:443`, `cdn.jsdelivr.net:443`, `raw.githubusercontent.com:443`, and
`gist.githubusercontent.com:443`, so a CLI run with only `--allow-read=.` can
still import from those registries. libdeno starts from nothing: the same run
fails with `Requires import access to "deno.land:443"` until the host is named,
either individually or with a bare `--allow-import`. This is deliberate — an
embedded runtime should not silently reach the network on the caller's behalf —
and it is the one place where the permission surface is narrower than the CLI.

File imports, including static and dynamic imports, require `--allow-read` for
each file. The entry file is checked too; a read grant on its directory covers
the entry and its imports.

Grants are checked against the path as resolved by `canonicalize`, which follows
symlinks. A symlink inside a granted directory that points outside it therefore
reaches the target under the grant. This matches the Deno CLI (verified against
Deno 2.9.8) and is not an libdeno-specific widening; if a grant must not cover
symlink targets, do not put writable symlinks in a granted tree, or use a
narrower path grant. `tests/path_grant_boundaries.rs`.

A dynamic import is checked when it runs, not when the importing module loads.
Revoking read after startup therefore denies a later `import("./file.js")`,
whether the specifier is a literal or computed.

npm package sources are checked the same way: with a broker or hook installed,
the module file under `node_modules` goes through the broker before it is read,
so a denied read fails the import.

Unknown flags are rejected. `--deny-*` and `--ignore-*` are not supported.

### What each grant actually covers

Three grants are easy to read as narrower than they are. All three match Deno
CLI 2.9.8; the measurements are in the tests named below.

**`--allow-run=<name>` pins a name, resolved through `PATH`.** A grant on `git`
runs whatever `git` means at spawn time, so a script-writable directory on the
host's `PATH` hands the script the binary. A script cannot smuggle its own
`PATH` through `Deno.Command`'s `env` — that combination is refused with
`Requires run access`. Grant an absolute path when the binary matters.
`tests/run_grant_boundaries.rs`.

A scoped `--allow-run` also stops working when the host environment carries any
non-empty `LD_*` or `DYLD_*` variable: those can redirect the dynamic loader, so
Deno requires a global `--allow-run` to pass them to a child. The error names
the variable. This bites under `cargo test`, which sets
`DYLD_FALLBACK_LIBRARY_PATH` on macOS.

**`--allow-ffi` follows links like every other path grant.** The rule above
covers it: a symbolic or hard link inside a granted directory loads a library
that lives outside it. The consequence is worse here than for reads — loading a
library is arbitrary native code in the host process — so scope `--allow-ffi`
to files the script cannot replace rather than to a directory.
`tests/ffi_path_grants.rs`.

**`--allow-env` is process state, not run state.** `Deno.env.set` writes the
real environment. In-process, the write is visible to the host and to every
later run in that process; a web worker without an explicit permission set
inherits the capability and writes the same environment. The deny list for
libdeno's control variables still applies inside a worker, which is what keeps
this from becoming an arbitrary-code grant. `tests/env_cross_run.rs`,
`tests/worker_env_scope.rs`.

These checks run in the parent for subprocess execution too, so a bad flag or an
empty grant list is a `Configuration` / `Permission` error before any child
starts, not an opaque `Ok(exit_code: 1)`.

`LibdenoError::is_permission_error()` recognizes typed Rust permission denials.
JavaScript names, classes, causes, and messages are not denial evidence. A JS
failure without typed provenance is reported as `Runtime` or `Core` in Rust and
`DenoRuntimeError` in Python. Invalid permission flags retain their typed error.

## Interactive prompting (`prompt`)

`prompt` is off by default, so ungranted access is denied. Set
`LibdenoOptions.prompt = true` to ask on terminal stdin; without a terminal,
checks are denied. With flags and prompting enabled, flags grant their listed
capabilities and the prompt handles the rest.

Prompting and stderr capture are mutually exclusive. The upstream prompter
requires both stdin and stderr to be terminals, and stderr capture replaces
fd 2 with a pipe — combining them would silently deny every ungranted check
instead of asking. `run_with_output` with `prompt: true` and
`capture_stderr: true` is rejected with a `Configuration` error.

libdeno's own control variables are permanently denied to scripts, even with a
bare `--allow-env`: `LIBDENO_HOST_EXE` (which binary a subprocess run executes),
the child/supervisor handshake variables, and the two cache knobs. The same
deny list covers the resolver inputs libdeno re-reads for every run — `DENO_DIR`,
`JSR_URL`, and the npm registry variables — because rewriting one steers a
*later* run at a cache or registry the script chose. See
[Environment variables](api.md#environment-variables).

Subprocess stdin carries the run request, not interactive input, so prompting
there denies ungranted access. Repeated flags accumulate; an empty flag value
is an error.

## Broker hooks

`install_permission_broker(path)` and `install_permission_hook(hook)` are
process-global, install-once authorities for all permission checks. They
decide resource-access checks even when local flags grant access; installing
one does not revoke those grants or change the permission state reported by
`Deno.permissions.query()` and `request()`. They are mutually exclusive. The
broker uses a Unix socket or Windows named pipe; the in-process hook is
Unix-only. See the [API reference](api.md) for setup and failure behavior.

Install before starting runs or between runs after all workers have stopped.
An active run or lingering worker makes installation return a configuration
error immediately; runs are also rejected while installation is in progress.
This coordination covers libdeno's run and install APIs. Callers that install
through upstream or other runtime APIs must coordinate those calls themselves.

Decision priority: broker/hook (if installed) → flag grants → interactive
prompt when `prompt: true` → deny.

The same permission container guards module fetches, file imports, and worker
loads. A Worker created without explicit `deno.permissions` shares the parent's
live `PermissionsContainer`, so a revocation in the parent also applies inside
that worker. A Worker created with explicit permission arguments gets a forked
copy: grants are narrowed to what the parent already allows, and later parent
revocations do not reach it. `require()` follows the same read rules.

## Relative paths

Relative paths in permission values resolve against `LibdenoOptions.cwd`
(default: process current directory) — the same base used for entry
resolution and `node_modules` discovery.

## Example

```rust
use libdeno::{LibdenoOptions, run};

// Restrict to: reads under ./src, writes under /tmp, network to
// example.com, and the HOME/PATH environment variables.
let options = LibdenoOptions {
  permissions: vec![
    "--allow-read=./src".into(),
    "--allow-write=/tmp".into(),
    "--allow-net=example.com".into(),
    "--allow-env=HOME,PATH".into(),
  ],
  args: vec![],
  cwd: None,
  ..Default::default()
};
```
