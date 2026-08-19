# Permissions

libdeno's permission model mirrors the Deno CLI: capability strings in
`--allow-*` format, parsed into a `PermissionsContainer`.

## Default stance

Since v0.2.0 the model is **explicit opt-in**: an **empty** `permissions`
list is a **construction error** (`LibdenoError::Configuration`) — it grants
nothing. To run with every capability, either set
`LibdenoOptions.allow_all_permissions = true` (the `-A` equivalent) or pass
`-A`/`--allow-all` in the `permissions` list. Use the allow-all escape hatch
only for code you trust (see SECURITY.md).

Passing **any** other entry restricts the runtime to the declared
capabilities. A flag without a value allows that capability globally
(`--allow-read` == read anywhere); with a comma-separated value only the
listed descriptors are allowed (`--allow-read=./src,./public`).

## Supported flags

| Flag | Value (comma-separated) | Example |
|---|---|---|
| `--allow-read` | paths | `--allow-read=./src,./public` |
| `--allow-write` | paths | `--allow-write=/tmp` |
| `--allow-env` | env var names | `--allow-env=HOME,PATH` |
| `--allow-net` | hosts, optionally `:port` | `--allow-net=example.com:8080` |
| `--allow-import` | import hosts (see note) | `--allow-import=deno.land` |
| `--allow-run` | executable names | `--allow-run=git` |
| `--allow-ffi` | native library paths | `--allow-ffi=./libfoo.so` |
| `--allow-sys` | system API names | `--allow-sys=getpid` |
| `-A` / `--allow-all` | — | allow everything (same as `allow_all_permissions: true`) |

`--allow-import` gates **remote module loading** (`https:`/`jsr:` specifiers)
exactly like the CLI: there is **no `--allow-net` fallback** for module
fetches, so `--allow-net` alone does not enable them. A value is an import
host descriptor in `--allow-net` style (`deno.land`, `jsr.io`); full URLs
(`https://…`) are rejected by the upstream parser, as in the CLI. Without a
value, import access is granted globally.

**File imports** (`import ... from "./x.js"` or a `file://` URL, static and
dynamic) are gated by `--allow-read`: the graph loader checks each file
against the declared read scope (and the broker/hook), so a static import
outside the scope fails with a `NotCapable` error. This matches the strict
`--allow-read` guarantee the runtime ops provide.

Deny/ignore forms (`--deny-*`, `--ignore-*`) are not currently exposed, and
any unrecognized flag is rejected with a permission error rather than silently
ignored (an unknown flag must never silently widen the granted scope).

## Interactive prompting (`prompt`)

By default `prompt` is off: anything not granted is denied. Set
`LibdenoOptions.prompt = true` to mirror `deno run`'s default interactive
behavior: a non-granted check prints to stderr and reads allow/deny from
stdin, blocking the run while it waits. The upstream prompter requires a
terminal stdin (`is_terminal()`); a headless host without one sees every such
query denied without reading.

Three combinations:

| `permissions` | `prompt` | Behavior |
|---|---|---|
| empty | `false` | construction error (the v0.2.0 default) |
| empty | `true` | every access is asked interactively |
| flags | `true` | flags grant, everything else is asked |
| flags | `false` | flags grant, everything else is denied |

In subprocess mode (`run_in_subprocess`) the child's stdin is a pipe (consumed
by the request JSON), so the prompter's terminal check denies without reading —
`prompt: true` in a child is equivalent to fail-closed deny; real interaction
only makes sense for in-process `run`.

Repeated flags accumulate like the CLI: `--allow-read=./a --allow-read=./b`
grants both paths. A flag with an empty value (`--allow-read=`) is an error,
not a silent grant.

## Broker hooks

`install_permission_broker(path)` and `install_permission_hook(hook)` install a
process-global, install-once permission decision hook (see
[docs/api.md](api.md)). Once installed, the broker/hook is the **sole
authority** for every permission check in the process — including
already-granted capabilities — so local flags are no longer consulted (upstream
deno semantics). `install_permission_broker` talks to an external process over
a Unix socket / Windows named pipe (the JSON-line protocol deno uses for
jupyter/LSP); `install_permission_hook` serves an in-process closure the same
way (Unix only). The two are mutually exclusive, and neither can be installed
more than once per process.

Decision priority: broker/hook (if installed) → flag grants → interactive
prompt when `prompt: true` → deny.

The external broker has an upstream process-lifetime limitation: its
`PermissionBroker::new` constructor is not fallible and may call
`process::exit(87)` when the initial connection fails. Later broker/bridge
communication failures may also terminate the host through the upstream error
path; these exits cannot be caught as `LibdenoError`. Do not use
`install_permission_broker` with an untrusted or unreliable endpoint, or where
the host must remain alive after broker failure. The in-process hook has the
same synchronous/blocking semantics; a blocking hook stalls permission checks,
while an unwinding panic is caught at the bridge boundary and fails closed as
deny. A `panic = "abort"` build or a panic hook that aborts/exits cannot be
caught by `catch_unwind` and may terminate the host. A blocked hook can still
prevent `execution_deadline` from interrupting the run; the external broker's
upstream constructor/communication exits remain uncatchable.

## How it works

`build_permissions` (`src/permissions.rs`) splits each flag on `=`, parses the
comma-separated value, and fills a `PermissionsOptions` struct. If
`LibdenoOptions.allow_all_permissions` is set (or the `-A`/`--allow-all`
string appears), it returns `PermissionsContainer::allow_all` immediately.
If no `--allow-*` flag was seen, it returns a configuration error — an empty
list no longer silently grants everything. Otherwise it constructs a
`Permissions` from the options and wraps it.

The parsed container is:

- handed to the file fetcher (`DenoGraphLoaderOptions.permissions`) for
  module fetching,
- handed to `WorkerServiceOptions.permissions` for the main worker,
- cloned (shallow — the container wraps an `Arc`, so the clone is live, not
  a snapshot) into `RuntimeServices` and passed to web workers via
  `CreateWebWorkerArgs.permissions`. Permission revocations stay honored in
  the workers; do not deep-clone here.

The `CjsAnalysisSourceProvider` and `SimpleNodeRequireLoader` enforce the
same read permissions as `Deno.readTextFile`: a fully-granted read
(`query_read_all`) skips the check, npm-managed files under `node_modules`
are trusted, and everything else must satisfy the declared `--allow-read`
scope. `require()` cannot bypass the restrictions.

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
