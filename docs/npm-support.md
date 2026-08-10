# npm & Module Resolution

libdeno runs the official Deno module graph pipeline end to end. Everything
the CLI's loader does — `npm:`, `jsr:`, remote modules, import maps, CJS
packages, WASM, JSON — comes from the graph.

## npm modes

`node_modules_dir` is `Auto` (the CLI default):

- **BYONM** — if a `node_modules` directory exists at the project root, it is
  used as-is. No install happens; bare imports resolve against the existing
  tree.
- **Managed** — otherwise, packages are installed into `node_modules` on
  demand (lazy caching, npm cache enabled). This is what you observe when
  running the demo: the first run of `examples/demo-app/index.js` installs
  `chalk` into `examples/demo-app/node_modules`.

npm lifecycle scripts (`preinstall`/`install`/`postinstall`) do **not** run
by default, matching the deno CLI 2.x default: installing a dependency must
never execute arbitrary shell code without an explicit opt-in. Packages that
need a build step at install time (e.g. `esbuild`) will not work out of the
box.

The npm resolution snapshot is initialized before the module graph starts
requesting packages
(`initialize_npm_resolution_if_managed`).

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

The npm process state (resolution snapshot or BYONM marker, plus the
`node_modules` path) is serialized and handed to the child via
`NODE_CHANNEL_FD` (+ serialization mode), mirroring the CLI's `node_ipc_init`.
The forked child restores the npm resolution snapshot from that state.

## Module graph

- A single shared `ModuleGraph` (guarded by a mutex) is built on demand:
  `prepare_load` builds the tree rooted at the requested specifier;
  `load` reads from it. Already-loaded specifiers are not rebuilt.
- Graph building uses the official `deno_graph::ModuleGraph::build` with the
  `GraphResolver` (resolver + npm resolver) and the `DenoGraphLoader` (file
  fetcher).
- Remote modules are fetched into the HTTP cache; redirects are handled by
  the file fetcher (the HTTP client does not follow redirects).

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
