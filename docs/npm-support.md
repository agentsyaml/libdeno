# npm & Module Resolution

libdeno composes the official Deno module-graph and resolver crates for
`npm:`, `jsr:`, remote modules, import maps, CJS, WASM, and JSON imports.

## Optional `npm` Cargo feature

The npm installer / npm-cache host machinery is behind a default-on `npm`
feature:

```toml
[features]
default = ["snapshot", "npm"]
npm = ["dep:deno_npm_installer", "dep:deno_npm_cache"]
```

Build with `--no-default-features --features snapshot` to omit managed npm
installation. In that build, `npm:` imports fail; bare imports from a local
`node_modules` tree still work in `manual` mode. Add `npm` to
`default-features = false` builds to enable managed installs.

## npm modes

`nodeModulesDir` selects `manual` (use local `node_modules`), `auto` (managed
resolution), or `none` (disable `node_modules`). Without a setting, libdeno uses
managed `auto`; an existing `node_modules` directory does not select `manual`.

If present, `deno.lock` supplies and validates npm checksums. Frozen mode rejects
changes; libdeno never writes lockfile updates.

## Bare imports vs `npm:` specifiers

- **Bare imports** (`import chalk from "chalk"`) resolve package names from
  `package.json` dependencies.
- **`npm:` imports** (`import chalk from "npm:chalk@5"`) select a package
  version directly.

## CJS support

- npm packages use package.json `"type"` to detect CJS and load through the
  CJS-to-ESM translator. Files outside npm packages default to ESM.
- Recursive `require()` chains inside CJS packages are analyzed.

## Lifecycle scripts

Lifecycle scripts are disabled.

## `child_process.fork`

Forked children do not restore deno_process's inherited npm snapshot. They may
still resolve packages from local `node_modules` or `deno.lock`.
`DENO_DONT_USE_INTERNAL_NODE_COMPAT_STATE_FD` is deno_process's private npm
state handle; it is separate from Node's `NODE_CHANNEL_FD`.

Hosts that need Node IPC must call unsafe
`initialize_node_ipc_from_env()` during trusted startup, before creating
threads, and transfer exclusive ownership of a valid connected raw handle.
Initialization is one-shot, including failure; a claimed handle is closed if
later validation fails. Ordinary `run()` does not inspect or claim the live
environment channel. See the [API contract](api.md#initialize_node_ipc_from_env).

## Module graph

The main run builds its module graph on demand. Each web worker has a separate
graph and permission-bound loader; resolver and cache state are shared. Remote
modules use the HTTP cache, with redirects handled by the file fetcher. Blob
workers receive their root Blob for loading; revoked dependencies are not
restored.

## JSON imports

JSON imports require an import attribute, matching modern Deno:

```js
import pkg from "./pkg.json" with { type: "json" };
```

## Node builtins

`node:` builtins are served by the extension module map baked into the
snapshot — nothing is loaded from disk. The module loader returns an empty
JavaScript stub for `node:` specifiers and the real sources come from the
extension.
