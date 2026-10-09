# npm & Module Resolution

libdeno runs the official Deno module graph pipeline end to end. Everything
the CLI's loader does — `npm:`, `jsr:`, remote modules, import maps, CJS
packages, WASM, JSON — comes from the graph.

## Optional `npm` Cargo feature

The npm installer / npm-cache host machinery is behind a default-on `npm`
feature:

```toml
[features]
default = ["snapshot", "npm"]
npm = ["dep:deno_npm_installer", "dep:deno_npm_cache"]
```

Building with `--no-default-features --features snapshot` removes
`deno_npm_installer` and `deno_npm_cache` (and the npm lifecycle-script
executor) from the dependency graph. In such a build, `npm:` imports fail with
a clear `npm support is disabled in this build (enable the \`npm\` feature)`
error instead of attempting an install.

The same clear error covers a **managed** project with the feature off: a
`package.json` dependency imported by bare specifier (`import chalk from
"chalk"`) is rewritten by the resolver to `npm:chalk@5` and then rejected by
`GraphResolver::resolve_pkg_reqs` with the message above, even though
`initialize_npm_resolution_if_managed` never runs. A bare specifier that is *not*
in `package.json` keeps the accurate `Import "x" not a dependency` error
instead — with npm compiled out there is nothing it could resolve to, but
blaming npm there would be wrong (deno does not auto-add undeclared
dependencies with npm support enabled either).

Caveats:

- `deno_npm` and `deno_npmrc` are unconditional `deno_resolver` dependencies and
  cannot be removed; `node_modules` / BYONM resolution keeps working.
- This is a dependency / compile-surface reduction, not a binary-size win — the
  statically linked V8 runtime dominates binary size.
- Consumers building with `default-features = false` must add `"npm"` to keep
  `npm:` install support.

## npm modes

The configured `nodeModulesDir` mode is respected: `manual` uses the local
`node_modules` tree, `auto` uses managed npm resolution, and `none` disables
`node_modules` handling. JSON `null` means no mode was configured; it is not a
mode value. Merely having a `node_modules` directory does not
select BYONM. When no mode is configured, libdeno preserves its managed `Auto`
default; the first run of `examples/demo-app/index.js` installs `chalk` into
`examples/demo-app/node_modules`.

When building without the `npm` Cargo feature, configure
`deno.json` as `{"nodeModulesDir":"manual"}` to use an existing local
dependency tree. A missing package is a local resolution error; there is no
automatic installer. Unconfigured projects remain in managed `auto` mode, and
declared npm dependencies require the `npm` feature.

npm lifecycle scripts (`preinstall`/`install`/`postinstall`) do **not** run
by default, matching the deno CLI 2.x default: installing a dependency must
never execute arbitrary shell code without an explicit opt-in. Packages that
need a build step at install time (e.g. `esbuild`) will not work out of the
box.

The npm resolution snapshot is initialized before the module graph starts
requesting packages
(`initialize_npm_resolution_if_managed`).

When present, `deno.lock` is read as resolver input. Available checksums and
integrity data are checked, and frozen-lockfile settings reject changes.
Without frozen mode, a lockfile change is not necessarily an error. libdeno
always skips lockfile writes and never persists updates.

## Bare imports vs `npm:` specifiers

- **Bare imports** (`import chalk from "chalk"`): resolved by the node
  resolver against `package.json` dependencies, then loaded from the
  installed package.
- **`npm:` specifiers** (`import chalk from "npm:chalk@5"`): pass through the
  module loader and are resolved to the installed package file
  (`resolve_non_workspace_npm_req_ref_to_file`), with the npm: URL registered
  as a redirect alias for the concrete file URL.

## CJS support

- CJS packages are detected via the official CJS tracker, which is
  `package.json` `"type"` aware. User files outside npm packages are treated
  as ESM (`IsCjsResolutionMode::Disabled` — the CLI default; only `deno node`
  enables implicit CJS).
- CJS→ESM translation is enabled in the module loader
  (`NodeCodeTranslatorMode::ModuleLoader`).
- `require()` chains inside CJS modules that reference further CJS files are
  analyzed with `FsCjsAnalysisSourceProvider` (recursive CJS analysis).

## Lifecycle scripts

`preinstall` / `install` / `postinstall` run through the system shell exactly
like npm does:

```
sh -c <script>
```

with `INIT_CWD`, `npm_lifecycle_event`, `npm_lifecycle_script`,
`npm_package_name`, `npm_package_version` set, and the package's
`node_modules/.bin` + the root `.bin` prepended to `PATH`. Scripts that need
a runtime (`node`, `node-gyp`, ...) must be available on `PATH`.

## `child_process.fork`

The npm process-state provider serializes the current resolution snapshot (or
BYONM marker and `node_modules` path) for deno_process's private inherited state
file descriptor/handle, exposed through
`DENO_DONT_USE_INTERNAL_NODE_COMPAT_STATE_FD`. This is separate from Node's
`NODE_CHANNEL_FD` IPC channel. This release has no trusted child-startup
receiver that adopts and restores the inherited state, so automatic npm
snapshot restoration in forked children is unsupported. libdeno does not read,
seek, adopt, or close an unknown inherited state descriptor/handle. A fork
child may still resolve packages from its local `node_modules` or lockfile;
that is ordinary resolution, not proof that the inherited snapshot was
restored. Supporting restoration requires an explicit host-owned startup
handoff contract.

## Module graph

- A single shared `ModuleGraph` (guarded by a mutex) is built on demand:
  `prepare_load` builds the tree rooted at the requested specifier;
  `load` reads from it. Already-loaded specifiers are not rebuilt.
- Graph building uses the official `deno_graph::ModuleGraph::build` with the
  `GraphResolver` (resolver + npm resolver) and the `DenoGraphLoader` (file
  fetcher).
- Remote modules are fetched into the HTTP cache; redirects are handled by
  the file fetcher (the HTTP client does not follow redirects).
- Dynamic Blob imports use the run's Blob store. Blob-backed workers receive
  their captured root Blob locally, so that root can finish loading after URL
  revocation; revoked dependencies are not restored and the root is not
  reintroduced into the shared Blob store for that run.

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
