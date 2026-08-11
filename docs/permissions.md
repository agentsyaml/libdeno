# Permissions

libdeno's permission model mirrors the Deno CLI: capability strings in
`--allow-*` format, parsed into a `PermissionsContainer`.

## Default stance

An **empty** `permissions` list grants **everything**. This matches the
CLI's `-A`/`--allow-all` and is intentional for an embedded runtime: you opt
into restrictions.

Passing **any** entry restricts the runtime to the declared capabilities.
A flag without a value allows that capability globally
(`--allow-read` == read anywhere); with a comma-separated value only the
listed descriptors are allowed (`--allow-read=./src,./public`).

## Supported flags

| Flag | Value (comma-separated) | Example |
|---|---|---|
| `--allow-read` | paths | `--allow-read=./src,./public` |
| `--allow-write` | paths | `--allow-write=/tmp` |
| `--allow-env` | env var names | `--allow-env=HOME,PATH` |
| `--allow-net` | hosts, optionally `:port` | `--allow-net=example.com:8080` |
| `--allow-run` | executable names | `--allow-run=git` |
| `--allow-ffi` | native library paths | `--allow-ffi=./libfoo.so` |
| `--allow-sys` | system API names | `--allow-sys=getpid` |
| `-A` / `--allow-all` | — | allow everything (the default) |

Deny/ignore forms (`--deny-*`, `--ignore-*`) are not currently exposed, and
any unrecognized flag is rejected with a permission error rather than silently
ignored (an unknown flag must never silently widen the granted scope).
`prompt` is always off (no interactive prompts in an embedded runtime).

Repeated flags accumulate like the CLI: `--allow-read=./a --allow-read=./b`
grants both paths. A flag with an empty value (`--allow-read=`) is an error,
not a silent grant.

## How it works

`build_permissions` (`src/permissions.rs`) splits each flag on `=`, parses the
comma-separated value, and fills a `PermissionsOptions` struct. If no
`--allow-*` flag was seen, it returns
`PermissionsContainer::allow_all`. Otherwise it constructs a
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
};
```
