# libdeno

> 在 Rust 中嵌入 Deno 运行时，原生支持 `npm:` 说明符。

libdeno 是一个 Rust crate，它在你的程序里嵌入一个完整的 Deno 运行时（V8 + 官方模块图管线）。你的 JS/TS 代码可以直接 `import` npm 包、远程模块、`jsr:` 与 `node:` 内置模块，全部经由官方 deno 解析器栈处理——行为与 `deno run` 一致，但运行在你的进程里。远程（`https:`/`jsr:`）模块加载与 CLI 一样受权限约束：需要 `--allow-import`（或 `allow_all_permissions`/`prompt`）。

English: [README.md](README.md)

---

## 特性

- **官方模块图管线**：`npm:`、`jsr:`、远程 `https://`、`node:`、本地文件、JSON、WASM、import map（来自 `deno.json`）、TypeScript 转译，全部由 `deno_graph` + `deno_resolver` 处理。远程（`https:`/`jsr:`）导入需要 `--allow-import`（或 `allow_all_permissions`/`prompt`）——模块加载没有 `--allow-net` 兜底，与 `deno run` 一致。
- **npm 集成**：自动发现并使用 `node_modules`（BYONM）；没有时按需安装（managed mode）。支持 CJS 包、`.node` 原生插件。npm 生命周期脚本默认不执行（与 deno CLI 2.x 一致）。
- **`child_process.fork` 支持**：npm 解析快照随子进程传播。
- **Web Worker**：`new Worker(...)` 嵌套 worker 复用同一模块加载器与快照。
- **权限模型**：与 CLI 一致的 `--allow-*` 能力字符串；权限为显式选择——空列表是构造错误，除非设置 `allow_all_permissions`。
- **开箱即用的 unstable API**：`Deno.openKv`、cron、FFI、WebGPU 等默认启用（匹配 `deno run --unstable` 的"全开"立场）。
- **预构建 V8 快照**：构建时把运行时扩展编译进快照，冷启动更快。

---

## 快速开始

```rust
use libdeno::{LibdenoOptions, run};

let options = LibdenoOptions {
  permissions: vec!["--allow-read=.".into(), "--allow-net=example.com".into()],
  args: vec![],
  cwd: None,
};
let exit_code = run("app.js", &options).unwrap();
```

`run` 接受三种入口：

- 一个文件：`run("app.ts", ...)`
- 一个目录：`run("./my-app", ...)`（使用其中的 `package.json` 的 `main`，默认 `index.js`）
- 一个 `package.json` 本身：`run("./my-app/package.json", ...)`

### 复用解析器栈：`LibdenoRuntime` + `run_with`

`run()` 每次调用都会重建解析器栈（workspace / resolver / npm-installer 工厂、图解析器）。对在同一项目运行大量脚本的长驻宿主，可以一次构建并复用该栈——配置链变化时会自动重建：

```rust
use libdeno::{LibdenoRuntime, LibdenoOptions, run_with};

let runtime = LibdenoRuntime::new("./my-app").await.unwrap();
let options = LibdenoOptions { allow_all_permissions: true, ..Default::default() };
let exit_code = run_with(&runtime, "app.js", &options).unwrap();
```

脚本在 runtime 的 cwd 中运行（那里忽略 `LibdenoOptions.cwd`）；每次 `run_with` 仍用各自的 `options.permissions` 重建权限相关的 file fetcher / graph loader / graph，因此一次运行的授权不会泄漏给另一次。

### 运行示例

```bash
# 编译（首次需要几分钟，V8 快照 + 全量依赖）
cargo build --example demo

# 运行一个同时使用 npm 包、node 内置模块、本地模块和 JSON import 的应用
./target/debug/examples/demo examples/demo-app/index.js
# npm package (chalk) works
# node builtin (node:path): a/b/c
# local module: 1 + 2 = 3
# json import: name=demo-app deps=1

# TypeScript 入口
./target/debug/examples/demo examples/demo-app/tschalk_test.ts
# ts entry + chalk: ok

# 把 demo-app 目录本身作为入口
cd examples/demo-app && ../../target/debug/examples/demo .
```

---

## API

| 项 | 说明 |
|---|---|
| `run(entry, &options) -> Result<i32, LibdenoError>` | 运行入口到完成，返回脚本请求的退出码。每次调用构建独立的 current-thread 运行时与 worker，多次调用完全隔离。可在 tokio 运行时内安全调用——自动在独立线程上执行（见下文）。 |
| `run_with_output(entry, &options) -> Result<RunOutput, LibdenoError>` | 同 `run`，但当 `capture_stdout` / `capture_stderr` 开启时把脚本的 stdout/stderr 捕获进 `RunOutput`。 |
| `LibdenoRuntime::new(cwd)` | 为一个项目目录构建一次解析器栈（async）。被 `run_with` 复用；配置链（deno.json / deno.jsonc / import_map.json / package.json / .npmrc / node_modules）变化时自动重建。 |
| `run_with(&runtime, entry, &options) -> Result<i32, LibdenoError>` | 同 `run`，但复用 `runtime` 的解析器栈。语义与 `run` 一致（cwd 锁、tokio 重入处理、退出码、超时）；脚本在 runtime 的 cwd 中运行，权限相关组件每次调用重建。 |
| `run_with_output(&runtime, entry, &options) -> Result<RunOutput, LibdenoError>` | 同 `run_with`，但当 `capture_stdout` / `capture_stderr` 开启时把脚本的 stdout/stderr 捕获进 `RunOutput`——长驻宿主对应的 `run_with_output`（后者每次调用重建解析器栈）。其余语义与 `run_with` 一致。 |
| `run_in_subprocess(entry, &options) -> Result<i32, LibdenoError>` | 在子进程中运行入口。此时 `Deno.exit(n)` 只会终止子进程；宿主进程保持存活并拿到 `n`。宿主需在 `main()` 开头调用 `maybe_handle_child_mode()`。 |
| `maybe_handle_child_mode() -> bool` | 服务 `run_in_subprocess` 的子进程请求。正常宿主启动时立即返回 `false`；子进程模式下执行脚本并以脚本退出码退出进程。 |
| `LibdenoOptions.permissions: Vec<String>` | `--allow-*` 能力字符串。空列表是构造错误（`LibdenoError::Configuration`）——自 v0.2.0 起空列表不再放行任何能力；请传入能力标志、设置 `allow_all_permissions`，或设置 `prompt: true`。 |
| `LibdenoOptions.allow_all_permissions: bool` | 放行一切能力（等价于 `-A`）。空 `permissions` 列表要运行脚本必须设置它。只用于你信任的代码（见 SECURITY.md）。 |
| `LibdenoOptions.capture_stdout` / `capture_stderr: bool` | 把脚本的 stdout/stderr（fd 1/2）重定向进 `RunOutput` 而非宿主终端。捕获期间是进程级全局重定向：运行期间宿主其他线程的打印也会被捕获（运行内部串行）。 |
| `LibdenoOptions.max_capture_bytes: Option<usize>` | 每个流（stdout 与 stderr 各一份额度）的捕获上限；流超限时停止捕获、丢弃多余输出并置位 `RunOutput.capture_truncated`。`None`（默认）不限量。 |
| `LibdenoOptions.features: Option<Vec<String>>` | 覆盖默认 unstable 特性集（`kv`、`cron`、`ffi`、`webgpu`、`worker-options`）。特性名必须是合法的 deno unstable 特性名；`None`（默认）启用默认特性集。运行不受信任插件的嵌入方可缩小暴露面（例如 `Some(vec!["ffi".into()])`）；op 本身始终受权限管控。 |
| `LibdenoOptions.args: Vec<String>` | 通过 `process.argv`（argv[0] 之后）暴露给脚本的参数。 |
| `LibdenoOptions.cwd: Option<PathBuf>` | 相对路径（入口、权限、node_modules 发现）解析的工作目录，默认进程当前目录。 |
| `LibdenoOptions.max_heap_bytes: Option<usize>` | V8 老生代堆的硬上限（字节）；超过时 V8 以 OOM 中止。适用于主 worker **以及** `new Worker(...)` 派生的 web worker。 |
| `LibdenoOptions.execution_deadline: Option<Duration>` | 硬墙钟时限；到期后 isolate 被强制终止，运行以 `LibdenoError::Timeout` 失败。**不会**中断阻塞的系统调用（NFS 挂起的文件读取、同步 `Deno.Command` 等待）——这些调用只有在其自身返回后才结束，因此运行可能超出时限一个系统调用的时长。 |
| `LibdenoError` | 枚举：`Entry`（入口解析失败）、`Permission`（权限字符串非法）、`Configuration`（选项无法构成合法配置，如 v0.2.0 起空权限列表未显式选择）、`Runtime`、`Core`（脚本异常）、`Io`、`Timeout`（超时；消息说明具体原因）。 |

支持的权限标志：`--allow-read[=paths] --allow-write[=paths] --allow-env[=names] --allow-net[=hosts] --allow-import[=hosts] --allow-run[=names] --allow-ffi[=paths] --allow-sys[=names]`，以及 `-A` / `--allow-all`。`--allow-import` 管控远程模块加载（没有 `--allow-net` 兜底）；静态与动态文件导入由 `--allow-read` 管控。

### 异步宿主（tokio/axum）

`run()` / `run_with()` / `run_with_output()` 可在 tokio 运行时内安全调用：tokio 禁止在同一线程再起一个运行时，因此运行会在独立线程执行并 join 回来。注意调用方的线程在整个运行期间被阻塞（这是同步调用）；单线程 runtime 上其他任务会同时停摆，多线程 runtime 上每次并发运行会占用一个 worker。运行仍在内部 cwd 锁上串行，不会重叠。

### 输出捕获

```rust
let out = libdeno::run_with_output(&entry, &LibdenoOptions {
    allow_all_permissions: true,
    capture_stdout: true,
    capture_stderr: true,
    ..Default::default()
})?;
println!("exit={} stdout={:?}", out.exit_code, out.stdout);
```

捕获是 fd 级：`console.log` / `console.error` / `Deno.stderr.write` 及任何直接 fd 写入都会进入 `RunOutput`。注意：捕获运行期间，宿主其他线程写到 stdout/stderr 的内容也会被捕获。

`LibdenoOptions.max_capture_bytes` 限制每个流的缓冲区（stdout 与 stderr 各一份额度）：流超限时停止捕获、丢弃多余输出并置位 `RunOutput.capture_truncated`，防止冗长或恶意的脚本无限撑大宿主内存。`None`（默认）不限量。

输出捕获仅限 unix：Windows 上 Rust std 的 stdout/stderr 绕过被重定向的 CRT fd，因此 `capture_stdout`/`capture_stderr` 在那里会以 `LibdenoError::Configuration` 错误失败（请改用 `run_in_subprocess` 并接管子进程输出）。`run_with` 不支持捕获——请用 `run_with_output`，长驻宿主复用解析器栈时用 `run_with_output(&runtime, ...)`。

完整 API 文档见 [`docs/api.md`](docs/api.md)（英文）。常见嵌入形态（npm 插件 + 输出捕获）的端到端示例见 [`examples/npm-plugin.md`](examples/npm-plugin.md)（英文）。

---

## 构建

- Rust edition 2021。依赖与 Deno 官方同源：`deno_runtime 0.265`、`deno_core 0.410`、`deno_resolver 0.88`、`deno_graph 0.110`。
- 构建脚本（`build.rs`）生成 V8 快照并预转译残留的懒加载源；`DENO_SNAPSHOT_MINIFY_SOURCES` 环境变量可触发源压缩。
- 首次构建较慢（V8 快照 + 全量依赖）。Release 配置在 `Cargo.toml` 中关闭 debug 符号。
- `.node` 原生插件的符号导出：示例宿主通过 `.cargo/config.toml`（仅开发用）导出 `napi_*`。真实嵌入方应在自己的 `build.rs` 调用 `deno_napi::print_linker_flags("<host-binary-name>")`。

---

## 文档

- [英文文档](docs/)：
  - [Getting Started](docs/getting-started.md)
  - [API Reference](docs/api.md)
  - [Architecture](docs/architecture.md)
  - [npm & Module Resolution](docs/npm-support.md)
  - [Permissions](docs/permissions.md)

---

## 许可

MIT
