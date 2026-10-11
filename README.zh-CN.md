# libdeno

> 在 Rust 中嵌入 Deno 运行时，原生支持 `npm:` 说明符。

libdeno 是一个 Rust crate，它在你的程序里嵌入一个完整的 Deno 运行时（V8 + 官方模块图管线）。你的 JS/TS 代码可以直接 `import` npm 包、远程模块、`jsr:` 与 `node:` 内置模块，全部经由官方 deno 解析器栈处理——行为与 `deno run` 一致，但运行在你的进程里。远程（`https:`/`jsr:`）模块加载与 CLI 一样受权限约束：需要 `--allow-import`（或 `allow_all_permissions`/`prompt`）。

English: [README.md](README.md)

---

## 特性

- **官方模块图管线**：`npm:`、`jsr:`、远程 `https://`、`node:`、本地文件、JSON、WASM、import map（来自 `deno.json`）、TypeScript 转译，全部由 `deno_graph` + `deno_resolver` 处理。远程（`https:`/`jsr:`）导入需要 `--allow-import`（或 `allow_all_permissions`/`prompt`）——模块加载没有 `--allow-net` 兜底，与 `deno run` 一致。
- **npm 集成**：遵循 `deno.json` 中显式配置的 `nodeModulesDir`（`manual`、`auto` 或 `none`）；仅有 `node_modules` 目录不会自动选择 BYONM。未配置时保留 libdeno 的 managed `auto` 默认模式。支持 CJS 包、`.node` 原生插件。npm 生命周期脚本目前关闭。
- **`child_process.fork`**：Node IPC 与 deno_process 私有继承 npm 状态文件描述符/句柄是分开的。本版本不会自动从该资源恢复快照；本地包/lockfile 解析仍可能独立工作。
- **Web Worker**：每个 `new Worker(...)` 拥有独立模块图和受权限约束的 loader；worker 共享 resolver 状态、缓存及 V8 启动快照。
- **Blob 模块**：支持动态导入 Blob 模块及 Blob-backed worker。worker 可在 URL 撤销后继续使用已捕获的根 Blob；这不会恢复已撤销的依赖 URL，也不会让其他 loader 获得该根模块。
- **只读 lockfile**：`deno.lock` 参与 npm 解析并进行一致性检查，但 libdeno 不会写入它。
- **权限模型**：与 CLI 一致的 `--allow-*` 能力字符串；权限为显式选择——空列表是构造错误，除非设置 `allow_all_permissions`。
- **开箱即用的 unstable API**：`Deno.openKv`、cron、FFI、WebGPU 等默认启用（匹配 `deno run --unstable` 的"全开"立场）。
- **预构建 V8 快照**：构建时把运行时扩展编译进快照，冷启动更快。
- **内存脚本 API**：`run_source` / `run_source_with_output`（及异步变体）直接执行内存中的 JS/TS 源码（`&str`、`String`、`&[u8]`、`Vec<u8>`），无需写临时文件。
- **可选的 `npm` Cargo feature**：npm 安装器 / npm-cache 机制位于默认开启的 `npm` feature 之后，可以编译掉（见[构建](#构建)）。

---

## 快速开始

```rust
use libdeno::{LibdenoOptions, run};

let options = LibdenoOptions {
  permissions: vec!["--allow-read=.".into(), "--allow-net=example.com".into()],
  args: vec![],
  cwd: None,
  ..Default::default()
};
let exit_code = run("app.js", &options).unwrap();
```

`run` 接受三种入口：

- 一个文件：`run("app.ts", ...)`
- 一个目录：`run("./my-app", ...)`（使用其中的 `package.json` 的 `main`，默认 `index.js`）
- 一个 `package.json` 本身：`run("./my-app/package.json", ...)`

### 运行内存源码：`run_source`

`run_source(code, lang, base_dir, &options)` 直接执行内存中的 JS/TS 源码——不写临时文件，入口本身也不落盘。`code` 是任何实现了 `AsRef<[u8]>` 的类型（`&str`、`String`、`&[u8]`、`Vec<u8>`）；`lang` 为 `SourceLang::JavaScript` 或 `SourceLang::TypeScript`：

```rust
use libdeno::{run_source, SourceLang, LibdenoOptions};

let options = LibdenoOptions { allow_all_permissions: true, ..Default::default() };
run_source("console.log('hi')", SourceLang::JavaScript, "./my-app", &options).unwrap();
run_source("const x: number = 1 + 2; console.log(x)", SourceLang::TypeScript, "./my-app", &options).unwrap();
```

源码会注册到 `base_dir` 下一个唯一的虚拟 `file:` URL（例如 `<base_dir>/$deno$__libdeno_virtual_9f2c1ab4d7e5f001.ts`），然后走正常模块管线——模块图构建、TypeScript 转译、CJS、JSON、npm/远程导入——作为类文件模块加载。由此带来的行为：

- 相对导入按 `base_dir` 解析（会从磁盘加载虚拟入口旁边的真实文件）。
- `import.meta.url` / `Deno.mainModule` 显示的是虚拟说明符，而不是调用方指定的名字；URL 每次调用唯一。
- 解析器/配置发现（deno.json / package.json / node_modules）从 `base_dir` 开始。
- 内存入口的虚拟路径位于 `base_dir` 内部，因此与子导入一样，它同样要经过入口读取权限检查（入口本身仍不从磁盘读取）；授予 `base_dir` 的读权限即可同时覆盖两者。
- 子进程 / `Executor` 后端**不支持**内存源码（其载荷是文件系统路径）——请使用进程内 API。

其余语义与基于路径的 API 完全一致：`run_source_with_output` 与 `run_with_output` 一样在独占租约下捕获输出；`run_source_async` / `run_with_source_async` 遵循 `run_async` 的规则（future 非 `Send`，同一线程不可交错运行）。在可复用的 `LibdenoRuntime` 上对应的是 `libdeno::runtime::run_with_source` / `run_with_output_source` / `run_with_source_async` / `run_with_output_source_async`。

### 复用解析器栈：`LibdenoRuntime` + `run_with`

`run()` 每次调用都会重建解析器栈（workspace / resolver / npm-installer 工厂、图解析器）。对在同一项目运行大量脚本的长驻宿主，可以一次构建并复用该栈——项目配置链、`deno.lock`、根目录 `node_modules`、有效 registry 以及 resolver 支持的全局 npmrc（`$HOME/.npmrc`）发生变化时会自动重建。`deno_resolver` 0.88 不读取 `NPM_CONFIG_USERCONFIG`。对于配置链之外的变化（例如嵌套 `node_modules`），可调用 `runtime.refresh()` 强制下一次运行重建；这是显式的有限失效机制，不是递归 watcher/hash：

```rust
use libdeno::{LibdenoRuntime, LibdenoOptions, run_with};

let runtime = LibdenoRuntime::new("./my-app").await.unwrap();
let options = LibdenoOptions { allow_all_permissions: true, ..Default::default() };
let exit_code = run_with(&runtime, "app.js", &options).unwrap();
```

入口与模块的相对路径按 `LibdenoRuntime::new(cwd)` 的 cwd 解析；进程 cwd 从不切换，脚本本身观察到的是宿主的 cwd。`LibdenoOptions.cwd` 可以省略；若提供则必须与 runtime 的目录（按 canonicalize 比较）一致，否则以 `LibdenoError::Configuration` 拒绝。每次 `run_with` 仍用各自的 `options.permissions` 重建权限相关的 file fetcher / graph loader / graph，因此一次运行的授权不会泄漏给另一次。

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
| `run(entry, &options) -> Result<i32, LibdenoError>` | 运行入口到完成，返回脚本请求的退出码。每次调用构建独立的 current-thread 运行时与 worker；普通运行完全并行——各自拥有线程、isolate 与图，不共享任何可变状态（进程级 analysis / npm-snapshot / 磁盘缓存是安全的共享状态；进程 cwd 从不切换）。可在 tokio 运行时内安全调用——自动在独立线程上执行（见下文）。 |
| `run_with_output(entry, &options) -> Result<RunOutput, LibdenoError>` | 同 `run`，但当 `capture_stdout` / `capture_stderr` 开启时把脚本的 stdout/stderr 捕获进 `RunOutput`。 |
| `run_async(entry, &options) -> Result<i32, LibdenoError>` | 异步入口：在**调用方**的 tokio 运行时上执行脚本——不派生子线程。必须在 tokio 上下文中 await；future 不是 `Send`，同一线程上第二个 `run_async` 会被 `LibdenoError::Configuration` 拒绝（交错运行会中止进程）。一次只 await 一个——并行运行请用 `run`。 |
| `LibdenoRuntime::new(cwd)` | 为一个项目目录构建一次解析器栈（async）。被 `run_with` 复用；配置链（deno.json / deno.jsonc / import_map.json / package.json / `.npmrc` / `deno.lock` / `node_modules`）或有效 npm registry / `$HOME/.npmrc` 变化时自动重建。 |
| `LibdenoRuntime::refresh()` | 强制下一次 reusable run 重建解析器栈，例如发现嵌套 `node_modules` 变化时使用。 |
| `run_with(&runtime, entry, &options) -> Result<i32, LibdenoError>` | 同 `run`，但复用 `runtime` 的解析器栈。语义与 `run` 一致（普通运行并行、tokio 重入处理、退出码、超时）；相对路径按 runtime 的 cwd 解析，权限相关组件每次调用重建。捕获标志与不匹配的 `options.cwd` 会被 `LibdenoError::Configuration` 拒绝。 |
| `libdeno::runtime::run_with_output(&runtime, entry, &options) -> Result<RunOutput, LibdenoError>` | 同 `run_with`，但当 `capture_stdout` / `capture_stderr` 开启时把脚本的 stdout/stderr 捕获进 `RunOutput`——长驻宿主对应的 `run_with_output`（后者每次调用重建解析器栈）。其余语义与 `run_with` 一致。 |
| `run_in_subprocess(entry, &options) -> Result<i32, LibdenoError>` | 在子进程中运行入口。此时 `Deno.exit(n)` 只会终止子进程；宿主进程保持存活并拿到 `n`。宿主需在 `main()` 开头调用 `maybe_handle_child_mode()`。 |
| `run_in_subprocess_with_output(entry, &options) -> Result<RunOutput, LibdenoError>` | 同 `run_in_subprocess`，但把子进程自己的 stdout/stderr 管道回传给 `RunOutput`：按进程捕获，可与其他运行并行，Windows 上也可用。两个流始终返回；`max_capture_bytes` 限制每个流，截断时置位 `RunOutput.capture_truncated`。 |
| `maybe_handle_child_mode() -> bool` | 服务 `run_in_subprocess` 的子进程请求。正常宿主启动时立即返回 `false`；子进程模式下执行脚本并以脚本退出码退出进程。 |
| `LibdenoOptions.permissions: Vec<String>` | `--allow-*` 能力字符串。空列表是构造错误（`LibdenoError::Configuration`）——自 v0.2.0 起空列表不再放行任何能力；请传入能力标志、设置 `allow_all_permissions`，或设置 `prompt: true`。 |
| `LibdenoOptions.allow_all_permissions: bool` | 放行一切能力（等价于 `-A`）。空 `permissions` 列表要运行脚本必须设置它。只用于你信任的代码（见 SECURITY.md）。 |
| `LibdenoOptions.capture_stdout` / `capture_stderr: bool` | 把脚本的 stdout/stderr（fd 1/2）重定向进 `RunOutput` 而非宿主终端。捕获期间是进程级全局重定向：运行期间宿主其他线程的打印也会被捕获，且运行是**独占**的——任何并发运行（无论是否捕获）都会被 `LibdenoError::Configuration` 拒绝（并行执行需要捕获时请用 `run_in_subprocess_with_output`）。 |
| `LibdenoOptions.max_capture_bytes: Option<usize>` | 每个流（stdout 与 stderr 各一份额度）的捕获上限；流超限时停止捕获、丢弃多余输出并置位 `RunOutput.capture_truncated`。`None`（默认）不限量。 |
| `LibdenoOptions.features: Option<Vec<String>>` | 覆盖默认 unstable 特性集（`kv`、`cron`、`ffi`、`webgpu`、`worker-options`）。特性名必须是合法的 deno unstable 特性名；`None`（默认）启用默认特性集。运行不受信任插件的嵌入方可缩小暴露面（例如 `Some(vec!["ffi".into()])`）；op 本身始终受权限管控。 |
| `LibdenoOptions.args: Vec<String>` | 通过 `process.argv`（argv[0] 之后）暴露给脚本的参数。 |
| `LibdenoOptions.cwd: Option<PathBuf>` | 相对路径（入口、权限、node_modules 发现）解析的基准目录，默认进程当前目录。进程 cwd 从不切换——脚本观察到的是宿主的 cwd（`Deno.cwd()`），需要按运行指定工作目录时请用 `run_in_subprocess`。 |
| `LibdenoOptions.max_heap_bytes: Option<usize>` | 进程内、尽力而为的 V8 老生代堆约束（字节）；小于 8 MiB 的值会被拒绝。不覆盖 native 分配、V8 external memory、宿主分配、RSS、CPU 或子进程内存，也不是 OS/进程级边界。适用于主 worker **以及** `new Worker(...)` 派生的 web worker。 |
| `LibdenoOptions.execution_deadline: Option<Duration>` | 进程内、尽力而为的时限；当 V8 到达可中断的 JavaScript stack check 时可中断并返回 `LibdenoError::Timeout`。**不能**中断阻塞系统调用、native code、子进程等待或阻塞的 permission broker/hook，因此运行可能超过请求的时限，也不是 CPU 时间上限。 |
| `LibdenoError` | 枚举：`Entry`（入口解析失败）、`Permission`（权限字符串非法）、`Configuration`（选项无法构成合法配置，如 v0.2.0 起空权限列表未显式选择）、`Runtime`（运行时启动或脚本失败）、`Core`（deno_core 执行/事件循环错误，也可能承载 JS 异常）、`Io`、`Timeout`（超时；消息说明具体原因）。JS 异常根据 Rust wrapper 路径可能归为 `Runtime` 或 `Core`。`is_permission_error()` 只信任带类型的 Rust 拒绝来源；JS 异常的名称、class 或 cause 不能证明权限拒绝来源。 |

支持的权限标志：`--allow-read[=paths] --allow-write[=paths] --allow-env[=names] --allow-net[=hosts] --allow-import[=hosts] --allow-run[=names] --allow-ffi[=paths] --allow-sys[=names]`，以及 `-A` / `--allow-all`。`--allow-import` 管控远程模块加载（没有 `--allow-net` 兜底）；静态与动态文件导入由 `--allow-read` 管控。

### 异步宿主（tokio/axum）

在 tokio 上下文中优先使用 `run_async` / `run_with_output_async`：它们在调用方的运行时上执行，不派生子线程。其 future 不是 `Send`，多线程 runtime 上需通过 `tokio::task::LocalSet` 运行；单个 `run_async` 可直接在 current-thread runtime 上 await。同一线程上不得与另一个 `run_async` 交错——第二个会被 `LibdenoError::Configuration` 拒绝（交错运行会中止进程）。并行运行请用 `run`（每次运行拥有自己的线程与 isolate）。

同步入口（`run` / `run_with` / `run_with_output`）在 tokio 运行时内调用依然安全，作为回退方案：tokio 禁止在同一线程再起一个运行时，因此运行会在独立线程执行并 join 回来。注意调用方的线程在整个运行期间被阻塞（这是同步调用）；单线程 runtime 上其他任务会同时停摆，多线程 runtime 上每次并发运行会占用一个 worker。普通运行完全并行；只有输出捕获是独占的（捕获运行会拒绝任何重叠运行）。

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

捕获是 fd 级：`console.log` / `console.error` / `Deno.stderr.write` 及任何直接 fd 写入都会进入 `RunOutput`。注意：捕获运行期间，宿主其他线程写到 stdout/stderr 的内容也会被捕获。捕获还是**独占**的：由于它是进程级 fd 重定向，捕获运行会拒绝任何并发运行（无论是否捕获）并返回 `LibdenoError::Configuration`——需要并行执行捕获时请用 `run_in_subprocess_with_output`，它把子进程自己的 fd 管道回传。

`LibdenoOptions.max_capture_bytes` 限制每个流的缓冲区（stdout 与 stderr 各一份额度）：流超限时停止捕获、丢弃多余输出并置位 `RunOutput.capture_truncated`，防止冗长或恶意的脚本无限撑大宿主内存。`None`（默认）不限量。execution-control supervisor 默认每流保留 64 KiB，显式值上限为 96 KiB；这是输出保留策略。Supervisor `TERMINAL` 帧不含 stdout/stderr，父进程从子进程管道读取并只保留配置的前缀；独立的 1 MiB 控制帧限制并非该捕获预算的原因。legacy capture 若在 EOF 前达到收集期限，会返回 `LibdenoError::Io` 和 `ErrorKind::TimedOut`；这是捕获不完整错误，不是成功的字节截断。

输出捕获仅限 unix：Windows 上 Rust std 的 stdout/stderr 绕过被重定向的 CRT fd，因此 `capture_stdout`/`capture_stderr` 在那里会以 `LibdenoError::Configuration` 错误失败（请改用 `run_in_subprocess_with_output`——子进程自己的 fd 被管道回传，Windows 上也可用）。`run_with` 不支持捕获——请用 `run_with_output`，长驻宿主复用解析器栈时用 `libdeno::runtime::run_with_output(&runtime, ...)`。

### 资源与监督边界

- 远程模块响应体与 npm 元数据限制为 reqwest 传输解码后返回的 256 MiB body；这不是 wire-byte 上限，且上游解码可能在 libdeno 检查前分配内存。明确的 npm `.tgz` 路径同样限制为 1 GiB 的 reqwest 返回、传输解码后响应体字节（不是 wire bytes）。该路径的 gzip 响应会把 body 最后 4 字节作为 `ISIZE` hint 检查：它是最后一个 gzip member 的大小模 2^32。默认 1 GiB threshold（可用 `LIBDENO_MAX_TARBALL_DECOMPRESSED_BYTES` 调整）不是解压输出预算，也不表示实际首先读取的 member 或总提取量。下游 fallback 可能无硬输出预算地 `read_to_end`；gzip wrapper 与多成员边界没有精确核算。这不是解压或 RSS 硬上限。
- 每个 HTTP 操作有 300 秒墙钟预算，覆盖重试、退避、适用的重定向和响应体读取；它与 `execution_deadline` 分离。
- npm 生命周期脚本目前固定关闭；不承诺默认通过系统 shell 执行这些脚本。
- 子进程模式只把 `Deno.exit` 与直接运行时终止限制在直接子进程内，不承诺完整进程树清理，也不提供 RSS 或 CPU 硬隔离；需要这些边界时请使用 OS 级 supervisor 或 sandbox。

### Node `child_process.fork` 宿主初始化

需要 Node IPC 的 fork 宿主必须在可信启动代码中显式调用
`initialize_node_ipc_from_env()`，尽可能早地在 `main()` 中、创建线程之前调用。该 `unsafe` 函数假设 launcher 提供的 `NODE_CHANNEL_FD` 指向有效且已连接的 endpoint，并将其 raw handle 所有权独占转交给 libdeno。普通 `run()` 不会修改 marker，也不会自动认领环境中的 live channel。初始化尝试只能执行一次，失败也会消耗此状态；认领有效 raw handle 后若 mode/validation 失败，handle 会关闭且不可重试。不要把不可信环境值当作 raw handle 权限来源。第三方 fork 宿主需要迁移，详见[API 文档](docs/api.md#initialize_node_ipc_from_env)。

完整 API 文档见 [`docs/api.md`](docs/api.md)（英文）。常见嵌入形态（npm 插件 + 输出捕获）的端到端示例见 [`examples/npm-plugin.md`](examples/npm-plugin.md)（英文）。

---

## 构建

- Rust edition 2021。依赖与 Deno 官方同源：`deno_runtime 0.265`、`deno_core 0.410`、`deno_resolver 0.88`、`deno_graph 0.110`。
- 构建脚本（`build.rs`）生成 V8 快照并预转译残留的懒加载源；`DENO_SNAPSHOT_MINIFY_SOURCES` 环境变量可触发源压缩。
- 首次构建较慢（V8 快照 + 全量依赖）。Release 配置在 `Cargo.toml` 中关闭 debug 符号。
- `.node` 原生插件的符号导出：示例宿主通过 `.cargo/config.toml`（仅开发用）导出 `napi_*`。真实嵌入方应在自己的 `build.rs` 调用 `deno_napi::print_linker_flags("<host-binary-name>")`。

### Cargo feature：`npm`

`npm` 在默认 feature 集中（`default = ["snapshot", "npm"]`）。使用 `--no-default-features --features snapshot` 构建会移除 npm 安装器（`deno_npm_installer`）与 npm-cache（`deno_npm_cache`）机制，包括 npm 生命周期脚本执行器。在这样的构建中，`npm:` 导入会以明确的 "npm support is disabled in this build (enable the `npm` feature)" 错误失败。

需要说明的实话：`deno_resolver`/`node_resolver` 无条件依赖 `deno_npm` 与 `deno_npmrc`，这些 crate 无法从构建中移除；关闭 `npm` 只会去掉裸 `npm:` 说明符的安装支持，`node_modules`/BYONM 解析仍然可用。它是依赖/API 表面与编译时间的缩减——**不是**有意义的二进制体积收益（静态链接的 V8 运行时主导二进制大小）。

关闭 `npm` feature 时，若要使用现有本地依赖树，请在 `deno.json` 中显式配置
`{"nodeModulesDir":"manual"}`。缺少的包会产生本地解析错误，不会自动安装。
未配置的项目仍使用 managed `auto` 模式；声明的 npm 依赖需要启用 `npm` feature。

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
