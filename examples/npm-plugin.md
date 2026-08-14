# Example: run an npm-powered plugin and capture its output

The minimal end-to-end flow for the most common embedding shape: your Rust
host runs a JS plugin that uses an npm package, and you want the plugin's
stdout/stderr back instead of it printing to your terminal.

This mirrors a real-world setup (a "npm plugin" runner): the plugin is a
small CommonJS file that `require`s a package from `node_modules`, and the
host captures the output via `run_with_output`.

## 1. The plugin

```js
// plugin.cjs — a plugin that uses an npm dependency from node_modules.
const chalk = require('chalk'); // or any npm package
console.log(chalk.green('plugin ok'));
```

Put it in your project next to a `node_modules` directory (BYONM mode:
libdeno uses an existing `node_modules`; if none exists it installs
`npm:` specifiers on demand in managed mode).

## 2. The host

```rust
use libdeno::{run_with_output, LibdenoOptions, LibdenoError};

fn run_plugin(entry: &std::path::Path) -> Result<i32, LibdenoError> {
    let out = run_with_output(entry, &LibdenoOptions {
        // Scope permissions to the project directory (fail-closed default:
        // an empty permissions list is an error since v0.2.0).
        permissions: vec![
            format!("--allow-read={}", entry.parent().unwrap().display()),
            // Native addons (.node) additionally need the ffi grant:
            format!("--allow-ffi={}", entry.parent().unwrap().display()),
        ],
        // --allow-net if the plugin fetches remote modules or makes
        // requests; --allow-env for config via env vars.
        // allow_all_permissions: true, // only for fully trusted plugins
        capture_stdout: true,
        capture_stderr: true,
        ..Default::default()
    })?;
    println!("exit={}", out.exit_code);
    println!("stdout: {}", String::from_utf8_lossy(&out.stdout));
    println!("stderr: {}", String::from_utf8_lossy(&out.stderr));
    Ok(out.exit_code)
}
```

Safe from async hosts too: `run_with_output` detects a tokio context and
executes on a fresh thread, so you can call it directly from tokio/axum
handlers without `spawn_blocking` gymnastics.

## 3. Native addons

If the plugin (or its npm dependencies) load a `.node` native addon:

- The host binary must **export the `napi_*` symbols** the addon links
  against at dlopen time. Call `deno_napi::print_linker_flags("<host-binary-name>")`
  from your own `build.rs` (see README "Build" section).
- The addon file must actually be on disk and complete — install
  `optionalDependencies` / platform sub-packages the way the package expects
  (`npm install` under BYONM). A missing native binary typically surfaces as
  a package-internal error like "Failed to load native module for
  <platform>-<arch>", which is an install problem, not a libdeno one.
- `--allow-ffi` is required to load any `.node` file (checked per addon
  path).

## 4. Caveats

- Output capture is fd-level and process-global for the run's duration:
  other host threads printing during the run land in the captured buffer
  too (runs are serialized internally).
- The runtime's console takes the process-global `std::io::stdout()/stderr()`
  locks; don't hold those across an await boundary while calling libdeno.
- Runs are serialized on an internal cwd lock — for high-frequency small
  tasks, batch them into one script rather than spawning many `run` calls.
