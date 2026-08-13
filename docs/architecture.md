# Architecture

libdeno embeds the Deno runtime by composing the same crates the Deno CLI
uses, plus the embedder glue the CLI keeps private (`cli/graph_util.rs`).

## Source layout

```
src/
  lib.rs           Public API (run / LibdenoOptions / LibdenoError) + assembly
  permissions.rs   build_permissions: --allow-* strings -> PermissionsContainer
  subprocess.rs    run_in_subprocess / maybe_handle_child_mode
  worker_factory.rs create_web_worker_factory: new Worker(...) callback
  services.rs      Runtime services assembly (resolver stack, npm installer)
  graph.rs         GraphResolver: deno_graph Resolver + NpmResolver impls
  module_loader.rs GraphModuleLoader: deno_core ModuleLoader over the graph
  node_loader.rs   SimpleNodeRequireLoader for require() + CJS analysis provider
  http.rs          ReqwestHttpClient: HTTP client for remote modules & npm registry
build.rs           V8 runtime snapshot + residual lazy-source tables
```

## Assembly order (`run_inner`)

1. **Crypto provider**: installs the `aws-lc-rs` rustls crypto provider as the
   default. Both `aws-lc-rs` (deno_tls) and `ring` (reqwest) providers are
   enabled in the dependency graph; rustls cannot auto-select, so this must be
   explicit — otherwise `op_fetch` panics. (Idempotent; the `Err` is ignored.)
2. **Entry resolution**: file / directory / `package.json` → `ModuleSpecifier`.
3. **Services**: `RuntimeServices` builds the resolver pipeline (see below).
4. **Permission parser**: `--allow-*` strings → `PermissionsContainer`.
5. **Module loader**: `GraphModuleLoader` wrapping the shared services.
6. **Worker bootstrap**: `MainWorker::bootstrap_from_options` with the
   generated snapshot and residual lazy sources.
7. **Lifecycle**: `execute_main_module` → event loop → `load` →
   `beforeunload` → `unload` → `process.beforeExit` → `process.exit`.

## The resolver stack (`services.rs`)

Mirrors the CLI's composition:

- **`WorkspaceFactory`** — discovers `deno.json` (import maps, JSX, compiler
  options) walking up from the main module's directory
  (`ConfigDiscoveryOption::Discover`). `node_modules_dir` is `Auto`: use the
  existing `node_modules` if present (BYONM), otherwise install into it on
  demand (managed).
- **`ResolverFactory`** — the official resolver: Deno resolver (import maps,
  TS, jsr:), node resolver (bare specifiers, package.json), CJS tracker.
  CJS→ESM translation is enabled in the module loader
  (`NodeCodeTranslatorMode::ModuleLoader`); JSON imports require an import
  attribute (`AllowJsonImports::WithAttribute`), matching modern Deno.
  `force_disable_verbatim_module_syntax` is set because untyped execution
  cannot honor verbatim semantics safely.
- **`NpmInstallerFactory`** — managed npm installs with lazy caching, npm
  cache enabled, and lifecycle scripts disabled by default
  (`PackagesAllowedScripts::None`, matching deno CLI 2.x — packages whose
  install requires scripts, e.g. esbuild, are not usable as-is).
- **`PermissionedFileFetcher`** — fetches remote modules into the HTTP cache;
  redirects are handled by the fetcher, not the HTTP client.
- **`DenoGraphLoader`** — the `FileFetcher` wrapper implementing
  `deno_graph::source::Loader`.
- **`ModuleGraph`** — the shared, mutex-guarded `deno_graph::ModuleGraph`
  (`GraphKind::CodeOnly`).

### Feature gates (two layers, must be kept in sync)

Unstable APIs require two synchronized gates:

1. **Ops layer**: `FeatureChecker` (by name, e.g. `"kv"`) — ops call
   `check_or_exit`.
2. **JS layer**: `BootstrapOptions.unstable_features: Vec<i32>` (by ID —
   cron=4, ffi=6, kv=9, webgpu=25) — decides whether the `Deno.*` namespace
   is exposed.

Enabling only one yields `Deno.openKv is not a function`. libdeno enables
`kv`, `cron`, `ffi`, `webgpu`, `worker-options` by filtering
`deno_features::UNSTABLE_FEATURES` by name and collecting the IDs, keeping
both layers in sync from a single list (`ENABLED_FEATURES` in `lib.rs`).
`worker-options` gates the `deno.permissions` options in `new Worker(...)`.

## Module loading (`module_loader.rs`)

- **`resolve`** — pass-through for absolute (`file:`, `node:`, `data:`) and
  `jsr:`/`npm:` specifiers; everything else goes through the official graph
  resolver.
- **`prepare_load`** — builds (or extends) the shared module graph rooted at
  the requested specifier. `node:` builtins are skipped (they come from the
  extension module map in the snapshot). Already-present specifiers are not
  rebuilt.
- **`load`** — reads the prepared module from the graph via the
  `deno_resolver` module loader, which handles TS transpilation, CJS→ESM
  translation, JSON and WASM. `npm:` specifiers are first resolved to the
  installed package file (`resolve_non_workspace_npm_req_ref_to_file`). The
  requested npm:/jsr: URL is registered alongside the concrete file URL via
  a redirect alias. External assets (e.g. `data:` or fetched-on-demand files)
  are loaded through the file fetcher.

## Node compat (`node_loader.rs`)

- **`SimpleNodeRequireLoader`** — CJS detection delegates to the official CJS
  tracker (package.json `"type"` aware); read permission checks mirror
  deno_lib's npm permission checker (fully granted reads and
  `node_modules` files bypass; everything else must satisfy `--allow-read`).
- **`FsCjsAnalysisSourceProvider`** — permission-gated source reads for
  recursive CommonJS analysis (`require()` chains inside CJS modules).

## Web Workers

`create_web_worker_factory` (in `worker_factory.rs`) builds the
`CreateWebWorkerCb` for `new Worker(...)`. Each spawned worker constructs its
own Rc-based loader/node services from the shared `Arc<SharedServices>` —
nothing non-Send crosses threads. The factory is recursive, so nested workers
work. Workers reuse the same snapshot, residual sources, blob store, and
broadcast channel. Each worker builds its own `DenoGraphLoader` bound to the
worker's permissions container, so a worker's module loads are gated by its
own grants, not the main run's; the module *graph* itself stays shared (a
module the main run already fetched is served to the worker without a
re-check).

## HTTP client (`http.rs`)

A minimal reqwest client that deliberately does **not** follow redirects
(redirect handling belongs to the file fetcher). Implements both
`deno_cache_dir::file_fetcher::HttpClient` (remote modules) and
`deno_npm_cache::NpmCacheHttpClient` (npm registry, with ETag/If-None-Match
and optional Authorization).

## Snapshot (`build.rs`)

Adapted from `deno/cli/snapshot/build.rs`:

- `create_runtime_snapshot` produces `CLI_SNAPSHOT.bin` with all runtime
  extension JS compiled in.
- Residual lazy-load sources (extension files not consumed by the snapshot)
  are pre-transpiled at build time (node builtins are TypeScript) and emitted
  as `EXTENSION_RESIDUAL_SOURCES.rs` tables.
- `DENO_SNAPSHOT_MINIFY_SOURCES` triggers source minification.

## N-API / `.node` addons

The example host re-exports `napi_*` symbols via `.cargo/config.toml`
rustflags (dev-only). Real embedders must export them from their own binary —
`deno_napi::print_linker_flags("<host-binary-name>")` in their `build.rs`.
