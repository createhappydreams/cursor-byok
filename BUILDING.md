# Building from source · 从源码构建

This project is a Tauri 2 desktop app (Rust backend + TypeScript frontend).
本项目是一个 Tauri 2 桌面应用（Rust 后端 + TypeScript 前端）。

## Prerequisites · 前置要求

| Tool | Version | Notes |
|---|---|---|
| Rust | stable `x86_64-pc-windows-msvc` | install via [rustup](https://rustup.rs) |
| Node.js | 22.x | frontend toolchain |
| MSVC | VS 2022 Build Tools, workload "Desktop development with C++" | Windows only |
| WebView2 | preinstalled on Windows 11 / Edge | runtime only |

macOS: install Xcode Command Line Tools. Linux: see the [Tauri prerequisites](https://tauri.app/start/prerequisites/).

macOS 需安装 Xcode Command Line Tools；Linux 请参考 Tauri 的前置依赖说明。

## Steps · 构建步骤

```bash
npm --prefix apps/desktop install
npm --prefix apps/desktop run build          # produces apps/desktop/dist
cd apps/desktop
npm run tauri -- build --no-bundle           # produces the executable
```

Artifact · 产物：`target/release/cursor-byok-desktop.exe`（约 68 MB）

> `--no-bundle` skips installer packaging. Drop it if you also want an NSIS installer
> (that additionally requires a Tauri signing key).
> `--no-bundle` 只产出可执行文件；若需要 NSIS 安装包则去掉该参数（此时还需 Tauri 签名密钥）。

### Distributing the binary · 对外分发

Rust embeds the absolute source path of every crate into panic messages, so a
default build carries the builder's Windows user name
(`C:\Users\<name>\.cargo\registry\src\...`). Two steps remove it · Rust 会把每个
crate 的绝对源码路径写进 panic 信息，默认构建产物里带着构建者的 Windows 用户名，
两步清理：

```bash
# 1) rustc 自身的路径（Git Bash）
export RUSTFLAGS="--remap-path-prefix=$USERPROFILE=/build"

# 2) C 依赖用 __FILE__ 写死的路径（aws-lc / onig / mimalloc）
python scripts/strip-builder-paths.py target/release/cursor-byok-desktop.exe --name "$USERNAME"
```

Step 2 is an equal-length byte replacement inside string constants, so the PE
layout does not move; it is only cosmetic panic text. After it,
`grep -c "$USERNAME" cursor-byok-desktop.exe` must return 0.
第 2 步是定长的字符串常量替换，不改变 PE 结构，只影响诊断文本；执行后产物里不应再出现用户名。

`RUSTFLAGS` invalidates the cargo cache, so the first build with it recompiles
every dependency · 加了 `RUSTFLAGS` 会使 cargo 缓存失效，首次会重编全部依赖。
If that build appears to hang with idle `rustc`/`cl.exe` processes, cap the
parallelism (`CARGO_BUILD_JOBS=4`) — the jobserver deadlocks on some Windows
setups when the job count is high.
若构建卡住但 `rustc`/`cl.exe` 的 CPU 为 0，用 `CARGO_BUILD_JOBS=4` 限制并行度，
某些 Windows 环境下高并行会触发 jobserver 死锁。

Also ship the `.exe` alone — never the whole `target/release` folder (it contains
`.pdb` debug symbols with full paths) and never `~/.cursor-byok-v3` (it stores
provider API keys, the local CA private key and OAuth tokens in plaintext).
只分发 `.exe` 单文件：不要带上整个 `target/release`（含带完整路径的 `.pdb` 调试符号），
也不要分发 `~/.cursor-byok-v3`（里面有明文的模型密钥、本地 CA 私钥和 OAuth token）。

## Two pitfalls · 两个坑

### 1. Do not use `cargo build --release` · 不要直接用 cargo 编译

The frontend router is attached behind `#[cfg(not(dev))]` in
`apps/desktop/src-tauri/src/desktop.rs`, and the `dev` cfg is injected by the
Tauri CLI. Calling cargo directly makes the build look like a dev build, so the
embedded frontend assets are never compiled in. The app still launches, but the
window shows `HTTP ERROR 502` on `127.0.0.1` — while `http://127.0.0.1:<port>/`
returns 200, which makes it look like a proxy problem.

前端路由挂在 `desktop.rs` 的 `#[cfg(not(dev))]` 分支下，而 `dev` 这个 cfg 由
Tauri CLI 注入。直接调用 cargo 会被判定为 dev 构建，前端资源不会编进二进制。
应用仍能启动，但窗口会显示 `HTTP ERROR 502`；同时 `http://127.0.0.1:<port>/`
却返回 200，很容易误判成代理问题。

### 2. Don't pipe cargo output through `tail` (Git Bash) · 别把 cargo 输出接管道

In a Git Bash background job, `cargo build | tail -n 40` can deadlock the
jobserver: `rustc` is spawned but sits at ~0 CPU time forever. Redirect to a
file instead.

在 Git Bash 的后台任务里，`cargo build | tail -n 40` 可能触发 jobserver 死锁：
`rustc` 被派生出来却一直挂着，CPU 时间几乎为 0。改为重定向到文件：

```bash
cargo build --release > build.log 2>&1
```

## Verify the artifact · 校验产物

```bash
ls -l target/release/cursor-byok-desktop.exe                       # ~68 MB
grep -ac "assets/index-" target/release/cursor-byok-desktop.exe    # must be > 0
```

A ~59 MB binary with no `assets/index-` match is the broken variant described
above. Delete `target/` and rebuild with the Tauri CLI.

约 59 MB 且 `assets/index-` 匹配数为 0 的产物就是上面那种坏版本，
删掉 `target/` 后用 Tauri CLI 重新构建即可。

## Project layout · 目录结构

```
server/                  Rust backend: Cursor protocol, run engine, providers
apps/desktop/            Tauri shell + TypeScript UI
apps/desktop/src-tauri/  Desktop crate (window, tray, updater)
crates/semble-core/      shared code
```

- `server/src/cursor/` — Cursor protocol translation, checkpoint decoding
- `server/src/run/` — the run engine (one `Cursor Run` per user turn)
- `server/src/provider/` — OpenAI / Anthropic adapters
