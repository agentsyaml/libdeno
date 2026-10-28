# Permissions

libdeno's permission model mirrors the Deno CLI: capability strings in
`--allow-*` format, parsed into a `PermissionsContainer`.

## Default stance

An empty `permissions` list is a configuration error unless `prompt: true` or
`allow_all_permissions: true` is set. `allow_all_permissions` and `-A`/
`--allow-all` grant every capability; use them only for trusted code.

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
| `--allow-run` | executable names | `--allow-run=git` |
| `--allow-ffi` | native library paths | `--allow-ffi=./libfoo.so` |
| `--allow-sys` | system API names | `--allow-sys=getpid` |
| `-A` / `--allow-all` | — | allow everything (same as `allow_all_permissions: true`) |

`--allow-import` gates remote module loading (`https:` and `jsr:`); `--allow-net`
does not grant module-import permission. Values are host descriptors, such as
`deno.land` or `jsr.io`.

File imports, including static and dynamic imports, require `--allow-read` for
each file. The entry file is checked too; a read grant on its directory covers
the entry and its imports.

A dynamic import is checked when it runs, not when the importing module loads.
Revoking read after startup therefore denies a later `import("./file.js")`,
whether the specifier is a literal or computed.

npm package sources are checked the same way: with a broker or hook installed,
the module file under `node_modules` goes through the broker before it is read,
so a denied read fails the import.

Unknown flags are rejected. `--deny-*` and `--ignore-*` are not supported.

`LibdenoError::is_permission_error()` recognizes typed Rust permission denials.
JavaScript names, classes, causes, and messages are not denial evidence. A JS
failure without typed provenance is reported as `Runtime` or `Core` in Rust and
`DenoRuntimeError` in Python. Invalid permission flags retain their typed error.

## Interactive prompting (`prompt`)

`prompt` is off by default, so ungranted access is denied. Set
`LibdenoOptions.prompt = true` to ask on terminal stdin; without a terminal,
checks are denied. With flags and prompting enabled, flags grant their listed
capabilities and the prompt handles the rest.

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
