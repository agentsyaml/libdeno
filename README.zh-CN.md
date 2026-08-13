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
| `run(entry, &options) -> Result<i32, LibdenoError>` | 运行入口到完成，返回脚本请求的退出码。每次调用构建独立的 current-thread 运行时与 worker，多次调用完全隔离。 |
| `run_in_subprocess(entry, &options) -> Result<i32, LibdenoError>` | 在子进程中运行入口。此时 `Deno.exit(n)` 只会终止子进程；宿主进程保持存活并拿到 `n`。宿主需在 `main()` 开头调用 `maybe_handle_child_mode()`。 |
| `maybe_handle_child_mode() -> bool` | 服务 `run_in_subprocess` 的子进程请求。正常宿主启动时立即返回 `false`；子进程模式下执行脚本并以脚本退出码退出进程。 |
| `LibdenoOptions.permissions: Vec<String>` | `--allow-*` 能力字符串。空列表是构造错误（`LibdenoError::Permission`）——请传入能力标志或设置 `allow_all_permissions`；传任意项则只放行声明的能力。 |
| `LibdenoOptions.allow_all_permissions: bool` | 放行一切能力（等价于 `-A`）。空 `permissions` 列表要运行脚本必须设置它。只用于你信任的代码（见 SECURITY.md）。 |
| `LibdenoOptions.args: Vec<String>` | 通过 `process.argv`（argv[0] 之后）暴露给脚本的参数。 |
| `LibdenoOptions.cwd: Option<PathBuf>` | 相对路径（入口、权限、node_modules 发现）解析的工作目录，默认进程当前目录。 |
| `LibdenoError` | 枚举：`Entry`（入口解析失败）、`Permission`（权限字符串非法 / 空列表未显式选择）、`Runtime`、`Core`（脚本异常）、`Io`。 |

支持的权限标志：`--allow-read[=paths] --allow-write[=paths] --allow-env[=names] --allow-net[=hosts] --allow-import[=hosts] --allow-run[=names] --allow-ffi[=paths] --allow-sys[=names]`，以及 `-A` / `--allow-all`。`--allow-import` 管控远程模块加载（没有 `--allow-net` 兜底）；静态与动态文件导入由 `--allow-read` 管控。

完整 API 文档见 [`docs/api.md`](docs/api.md)（英文）。

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
