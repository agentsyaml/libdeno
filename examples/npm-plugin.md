# Example: run an npm-powered plugin and capture its output

This example runs a CommonJS plugin that requires an npm package and captures
its stdout/stderr in the Rust host.

## 1. The plugin

```js
// plugin.cjs — a plugin that uses an npm dependency from node_modules.
const chalk = require('chalk'); // use a CommonJS-compatible release, e.g. chalk@4
console.log(chalk.green('plugin ok'));
```

Put it in your project with its package configuration. `nodeModulesDir` mode is
respected; without an explicit setting, libdeno uses managed `auto`. Merely
having a `node_modules` directory does not select BYONM.

## 2. The host

```rust
use libdeno::{run_with_output, LibdenoOptions, LibdenoError};

fn run_plugin(entry: &std::path::Path) -> Result<i32, LibdenoError> {
    let out = run_with_output(entry, &LibdenoOptions {
        // Scope permissions to the project directory (fail-closed default:
        // an empty permissions list is an error since v0.2.0).
        permissions: vec![
            format!("--allow-read={}", entry.parent().unwrap().display()),
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

`run_with_output` can be called from a tokio host; it runs the synchronous
operation on a fresh thread. Add `--allow-ffi=<path>` only if a dependency loads
a native `.node` addon.

## 3. Native addons

For a `.node` native addon:

- Grant `--allow-ffi` to the addon path.
- Export the host's N-API symbols with
  `deno_napi::print_linker_flags("<host-binary-name>")` in `build.rs`.
- Install the package's native binary for the current platform and architecture.

## 4. Caveats

- Capture redirects process file descriptors and is exclusive. Other host
  threads writing during the run may also be captured. Use
  `run_in_subprocess_with_output` for parallel per-child capture.
- Avoid holding Rust stdout/stderr locks across an `.await` while calling libdeno.
- For many small plugin operations, batch them into one script to amortize
  runtime startup.
