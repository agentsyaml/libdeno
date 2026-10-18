# Architecture

libdeno combines the Deno runtime, resolver, and module-graph crates to run
JavaScript and TypeScript inside a Rust process.

## Source layout

```
src/
  lib.rs           Public API and run assembly
  permissions.rs   Permission parsing
  subprocess.rs    Child-process execution
  worker_factory.rs Web worker creation
  services.rs      Resolver and runtime services
  graph.rs         Graph resolver adapters
  module_loader.rs Module loading
  node_loader.rs   Node require() and CJS analysis
  http.rs          HTTP requests for modules and npm
build.rs           V8 runtime snapshot + residual lazy-source tables
```

## Run flow

1. Resolve the file, directory, or `package.json` entry.
2. Build resolver services and parse permission options.
3. Build the module graph and fetch its dependencies.
4. Bootstrap a worker from the V8 snapshot and execute the script lifecycle.

## The resolver stack (`services.rs`)

`SharedServices` builds the workspace and resolver factories, the optional npm
installer, permissioned file fetcher, and `ModuleGraph` loader. The resolver
handles import maps, TypeScript, `jsr:`, Node package resolution, and CJS
classification. Configured `nodeModulesDir` modes are respected; an existing
`node_modules` directory alone does not select BYONM. npm lifecycle scripts are
disabled. The main run owns a code-only module graph; each web worker gets its
own graph and permission-bound loader.

### Feature gates (two layers, must be kept in sync)

An unstable API is available only when both the op's `FeatureChecker` and the
JavaScript namespace are enabled. `deno_runtime_adapter.rs` supplies both from
one list (`DEFAULT_RUNTIME_FEATURES`, overridable through
`LibdenoOptions.features`). The defaults include `kv`, `cron`, `ffi`, `webgpu`,
and `worker-options`.

## Module loading (`module_loader.rs`)

- **`resolve`** — sends local and `node:` specifiers to the graph resolver;
  `npm:` and `jsr:` are handled by `deno_graph`.
- **`prepare_load`** — builds or extends the owning graph. Each worker keeps a
  separate graph; `node:` builtins come from the runtime snapshot.
- **`load`** — reads prepared modules through `deno_resolver`, which handles
  TypeScript, CJS-to-ESM, JSON, and WASM. npm URLs resolve to installed package
  files; remote modules are fetched through the permissioned file fetcher.

## Node compat (`node_loader.rs`)

- **`SimpleNodeRequireLoader`** — CJS detection delegates to the official CJS
  tracker (package.json `"type"` aware). Without a broker, fully granted reads
  and npm-managed `node_modules` files bypass local checks; with a broker,
  source reads and require resolver metadata are checked at each op boundary.
- **`FsCjsAnalysisSourceProvider`** — permission-gated source reads for
  recursive CommonJS analysis (`require()` chains inside CJS modules).
- The CJS `NodeAnalysisCache` is resolver-local. The separate
  `ModuleInfoCache` used for module graph analysis is process-global; these are
  distinct caches with different ownership.

## Web Workers

`new Worker(...)` gets its own loader, permission-bound fetcher, and code-only
module graph. Nested workers are supported. Workers share resolver factories,
HTTP/file caches, analysis caches, the V8 snapshot, and npm process state, but
not module graphs or permissions.

## HTTP client (`http.rs`)

A minimal reqwest client that deliberately does **not** follow redirects
(redirect handling belongs to the file fetcher). Implements both
`deno_cache_dir::file_fetcher::HttpClient` (remote modules) and
`deno_npm_cache::NpmCacheHttpClient` (npm registry, with ETag/If-None-Match
and optional Authorization).

## Snapshot (`build.rs`)

The generated snapshot contains the runtime extensions. Any extension source
needed later is included in the binary as a residual source table.

## N-API / `.node` addons

Hosts loading `.node` addons must export N-API symbols. See
[Getting Started](getting-started.md#writing-your-own-host-binary).
