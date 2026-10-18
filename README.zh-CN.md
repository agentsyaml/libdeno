# libdeno

在 Rust 应用中嵌入 Deno JavaScript/TypeScript 运行时，并使用 npm 包和 Deno 模块解析。

libdeno 在宿主进程中执行文件或内存中的源码。它的 API 面向嵌入式脚本运行；需要其他命令行功能时，请使用 Deno CLI。

English: [README.md](README.md)

---

## 支持的功能

- 文件和内存中的 JavaScript/TypeScript 入口。
- 本地文件、`npm:`、`jsr:`、远程 URL、`node:` 内置模块、JSON 和 WASM 模块解析，具体能力受权限与运行时支持限制。
- 使用 `LibdenoRuntime` 复用项目解析状态。
- 可选输出捕获和子进程执行。
- npm 生命周期脚本默认关闭；构建时也可关闭 `npm` Cargo feature，见[构建](#构建)。

---

## 快速开始

将下面内容保存为 `app.js`：

```js
console.log("Hello from Deno");
```

然后在 Rust 宿主中调用：

```rust
use libdeno::{run, LibdenoError, LibdenoOptions};

fn main() -> Result<(), LibdenoError> {
    let options = LibdenoOptions {
        permissions: vec!["--allow-read=.".into()],
        ..Default::default()
    };
    let exit_code = run("app.js", &options)?;
    println!("exit code: {exit_code}");
    Ok(())
}
```

`run` 接受入口文件、应用目录或 `package.json`。目录入口使用其中的
`main` 字段；未设置时默认为 `index.js`。

### 运行内存源码：`run_source`

入口源码在内存中时使用 `run_source(code, lang, base_dir, &options)`。
`code` 实现 `AsRef<[u8]>`；`lang` 选择 JavaScript 或 TypeScript：

```rust
use libdeno::{run_source, SourceLang, LibdenoOptions};

let options = LibdenoOptions {
    permissions: vec!["--allow-read=./my-app".into()],
    ..Default::default()
};
run_source("console.log('hi')", SourceLang::JavaScript, "./my-app", &options).unwrap();
run_source("const x: number = 1 + 2; console.log(x)", SourceLang::TypeScript, "./my-app", &options).unwrap();
```

源码会映射到 `base_dir` 下唯一的虚拟 `file:` URL。相对导入和项目配置
从该目录解析。入口本身不从磁盘读取，但虚拟路径仍会经过读权限检查。
内存入口仅支持进程内 API。

需要捕获输出或异步执行时，可使用相应的 `run_source_with_output` 和 async API。

### 选择运行 API

- 单次运行使用 `run`；普通调用可以并发执行。
- 长驻宿主在同一项目运行多个入口时，可用 `LibdenoRuntime` 配合
  `run_with` 复用解析状态。对于未自动检测到的解析器变化，调用 `refresh()`。
- Tokio 宿主可用 `run_async`。future 是 `!Send`；同一线程上的 async
  运行须依次 await，不要交错执行。
- 需要独立工作目录或避免脚本的 `Deno.exit` 退出宿主时，用
  `run_in_subprocess`。它只监督直接子进程。
- 简单输出捕获用 `run_with_output`；需要并行运行时捕获输出用
  `run_in_subprocess_with_output`。

```rust
use libdeno::{LibdenoRuntime, LibdenoOptions, run_with};

let runtime = LibdenoRuntime::new("./my-app").await.unwrap();
let options = LibdenoOptions {
    permissions: vec!["--allow-read=.".into()],
    ..Default::default()
};
let exit_code = run_with(&runtime, "app.js", &options).unwrap();
```

`run_with` 按 runtime 目录解析入口，并根据本次调用的选项创建权限相关的
加载组件。进程工作目录保持为宿主目录；设置 `options.cwd` 时须与 runtime
目录一致。

---

## API

| 项 | 说明 |
|---|---|
| `LibdenoOptions.permissions` | 显式传入 `--allow-*` 能力。空列表会报配置错误，除非启用 `allow_all_permissions` 或 `prompt`。 |
| `LibdenoOptions.args` / `cwd` | 传给脚本的参数，以及用于解析入口和导入的基准目录。进程内脚本看到的是宿主进程的当前目录。 |
| `LibdenoOptions.features` | 覆盖默认 unstable API 集（`kv`、`cron`、`ffi`、`webgpu`、`worker-options`）。 |
| `LibdenoError` | 表示入口、权限、配置、运行时、I/O 或时限错误。错误变体详见[API 文档](docs/api.md)。 |

远程模块导入需要 `--allow-import`，文件导入需要 `--allow-read`；完整权限列表见
[权限文档](docs/permissions.md)。

Tokio 宿主可使用 `run_async` / `run_with_output_async`；future 是 `!Send`，同一线程上的运行须依次 await。同步 `run` API 在 Tokio 任务中会阻塞当前 worker 线程。

### 输出捕获

设置 `capture_stdout` / `capture_stderr` 并调用 `run_with_output`，即可
在 `RunOutput` 中获取脚本输出。平台支持、并发规则与缓冲区限制见
[安全与运行边界](SECURITY.md#runtime-boundaries)。

运行不受信任的代码时，请使用进程级 sandbox。需要 Node `child_process.fork` IPC 的宿主须在可信启动时调用 unsafe `initialize_node_ipc_from_env()`（[API 契约](docs/api.md#initialize_node_ipc_from_env)）。

完整接口见英文[API 文档](docs/api.md)；常见 npm 插件嵌入方式见英文[示例](examples/npm-plugin.md)。

---

## 构建

- 需要 Rust 1.95 或更新版本，crate 使用 edition 2021。首次构建会编译依赖并生成 V8 快照。
- 加载 `.node` 插件的宿主需要导出 `napi_*` 符号；链接配置见英文[入门文档](docs/getting-started.md)。

### Cargo feature：`npm`

默认 features 包含 `npm`。若要关闭 npm 安装支持，使用
`--no-default-features --features snapshot`。此时 `npm:` 导入不可用；已有
本地依赖树可在 `deno.json` 中设置 `{"nodeModulesDir":"manual"}`。

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
