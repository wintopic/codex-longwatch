<p align="center">
  <img src="packaging/icon.png" width="92" alt="Longwatch for Codex 图标">
</p>

<h1 align="center">Longwatch for Codex</h1>

<p align="center"><strong>Stay with the task.</strong></p>

<p align="center">
  一个原生桌面守候工具，让同一条 Codex 任务在过载、断连、重启与漫长等待中继续前进，同时严格避免并发回合。
</p>

<p align="center">
  <a href="https://github.com/wintopic/codex-longwatch/actions/workflows/ci.yml"><img src="https://github.com/wintopic/codex-longwatch/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/wintopic/codex-longwatch/releases"><img src="https://img.shields.io/github/v/release/wintopic/codex-longwatch?display_name=tag&sort=semver" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Apache--2.0-blue.svg" alt="Apache-2.0"></a>
</p>

<p align="center"><a href="README.md">English</a></p>

![Longwatch for Codex Windows 实机界面](docs/assets/longwatch-windows.png)

## 为什么做 Longwatch

长时间运行的 Codex 任务偶尔会遇到容量繁忙、事件流断开，或者电脑休眠后中途停住。手动盯着重试很打断工作，机械地重复提交又可能造成同一任务重复执行，甚至并发修改同一份工程。

Longwatch 接管的是“等待与恢复”这段工作。它只发送你填写的那一条真实任务，绑定一个持久 Codex 线程，根据最终结果决定成功、等待、重试或暂停，并在收到有效回复后提醒你。

它不会生成凑数提示词，不会切换账号，不会绕过限制，也不会同时开启多个回合。

## 核心能力

- **一个持久线程，一个活动回合。** 确定性的状态机从结构上阻止重叠请求。
- **崩溃后安全对账。** 在 `turn/start` 前先持久化“不确定提交”标记；重启后先恢复线程并向 Codex 核对真实状态，再决定是否允许下一次发送。
- **优先使用正式协议。** 默认启动 `codex app-server --listen stdio://`，通过按行分隔的 JSON-RPC 通信。
- **克制的重试节奏。** 普通瞬时故障默认从 30 秒起步，1.7 倍增长，10 分钟封顶，带 ±15% 抖动，并保留 15 秒硬下限。
- **理解 Codex 的不同结果。** 容量繁忙、HTTP 瞬时错误、内部重试、空回复、断流、鉴权失败与策略拒绝不会被一概而论。
- **休眠唤醒保护。** 系统恢复后至少再等 60 秒，不会把错过的重试一次性补发。
- **原生桌面体验。** GPUI 界面、系统通知、Windows 托盘、结果打开、诊断与崩溃报告。
- **明确授权的兼容回退。** GUI 自动化默认关闭，只有用户显式打开后才可能使用，绝不静默启用。
- **跨平台发布。** GitHub Actions 自动构建 Windows 安装程序与便携 ZIP、Linux DEB/AppImage/tar.gz，以及面向 Intel 和 Apple Silicon 的 macOS DMG 与便携应用包。
- **macOS 主题图标。** 应用运行时随系统浅色/深色模式切换白底深标与黑底白标 Dock 图标。

## 实现原理

### 状态机

```text
Idle
  └─ 开始 ─> Connecting ─> Sending ─> Waiting
                 ▲             │           │
                 │             │           ├─ 有效最终回复 ─> Success
                 │             │           ├─ 可重试结果 ─> Backoff ─┐
                 │             │           └─ 需要人工处理 ─> Paused/FatalError
                 └─────────────┴──────────────────────────────────────┘
```

所有状态变化都由一个 Tokio 控制任务串行处理。`QueueMachine::begin_sending` 在存在活动回合时直接拒绝新发送，迟到的旧回合事件也不会把已经结束的状态重新激活。

### 与 Codex app-server 的通信

Longwatch 默认启动本地 Codex CLI 的 app-server，并通过 stdio 交换按行分隔的 JSON-RPC：

```text
initialize
thread/start 或 thread/resume
turn/start
item/agentMessage/delta          流式预览
item/completed                   权威消息项
turn/completed                   最终判定点
turn/interrupt                   暂停、停止或超时中断
```

协议载荷使用宽容的 `serde_json::Value` 解码，不强依赖每个可选字段，降低相邻 Codex CLI 版本变化带来的兼容风险。

### 如何避免重启后重复提交

最危险的时间窗口位于 `turn/start` 已发出、确认响应尚未返回之间。此时程序如果崩溃，客户端无法仅凭本地内存判断服务器究竟有没有收到这次请求。

Longwatch 用一套很小的预写式恢复协议解决它：

1. 在本地记录本轮尝试。
2. 原子保存 `submissionUncertain = true`。
3. 保存成功后才允许调用 `turn/start`。
4. 收到回合 ID 或完成线程对账后清除标记。
5. 重启后先执行 `thread/resume`，禁止直接补发。
6. Codex 若报告正在进行或已经完成的回合，就以服务端状态为准。
7. 如果依然无法证明上次是否提交成功，则进入普通退避，而不是马上制造重复请求。

如果关键状态无法落盘，Longwatch 会在越过进程边界前暂停。

### 重试判定

| 结果 | 处理方式 |
| --- | --- |
| `willRetry = true` | 当前回合仍归 Codex 所有，不额外发送 |
| 当前高需求固定提示 | 静默立即衔接，不触发红色重试提醒 |
| Codex 内部重试次数耗尽 | 短暂稳定后开始下一轮外部尝试 |
| 429 / 502 / 503 / 504、过载文本、进程或事件流瞬断 | 进入克制的自动重试 |
| 成功状态但回复为空 | 在配置上限内继续重试 |
| 鉴权、额度、策略或配置错误 | 暂停并等待用户处理 |
| 协议不变量破坏 | 进入 `FatalError` |

普通应用层重试公式为：

```text
delay(n) = min(30 秒 × 1.7^(n-1), 10 分钟) × random(0.85 … 1.15)
```

app-server 自身的连接恢复采用独立的 5 秒、15 秒、60 秒封顶序列。

### 持久化模型

Longwatch 使用操作系统标准配置目录，并通过临时文件加原子替换写入 JSON：

- `config.json` 保存用户设置，也会保存任务原文，便于重启后继续。
- `state.json` 保存线程/回合 ID、状态、计数器、下次时间、任务 SHA-256 摘要，以及最多 4,000 字符的回复预览。
- 流式 token 只更新内存，最终事件与安全关键边界才落盘，避免每个 token 都触发磁盘同步。
- 损坏或来自未来版本的文档会被隔离为 `*.corrupt-<时间戳>`，随后加载安全默认值。
- 检测到旧 CodexQueue 配置目录时会尽力自动迁移。

## 架构

```text
GPUI / TDesign 桌面界面
        │ 命令 + 状态快照
        ▼
QueueRuntime ── QueueMachine ── 结果分类 / 退避 / 唤醒检测
        │
        ├── Codex app-server 传输层，默认
        │      └── stdio JSON-RPC + 持久线程对账
        │
        └── GUI 兼容传输层，显式开启
               ├── 操作系统无障碍接口提交
               └── 增量观察 Codex 会话 JSONL

平台门面：通知 · 托盘 · 打开结果 · 强提醒 · 诊断
持久化：原子配置/状态 JSON · 按日日志 · 崩溃报告
```

| 文件 | 责任 |
| --- | --- |
| `src/runtime.rs` | 串行控制器、恢复协议、超时与持久化边界 |
| `src/queue.rs` | 确定性状态机与单回合不变量 |
| `src/app_server.rs` | Codex app-server 生命周期和 JSON-RPC |
| `src/classifier.rs` | 成功、重试、暂停结果分类 |
| `src/backoff.rs` | 有界退避、抖动、尝试记录与唤醒检测 |
| `src/config.rs` | 版本化配置/状态、迁移与原子写入 |
| `src/gui_fallback.rs` / `src/jsonl.rs` | 显式 GUI 回退与增量会话观察 |
| `src/ui.rs` | 原生 GPUI 桌面界面 |
| `crates/gpui_platform` | 通知、托盘、自动化边界和平台图标行为 |

## 使用条件

- Windows 10 1809 及以上、macOS 12 及以上，或较新的 Linux 桌面环境。
- `PATH` 中存在可用的 `codex`，或者在高级设置中配置 Codex 可执行文件路径。
- 已经完成 Codex 登录。Longwatch 不负责替你认证。

## 安装

前往 [GitHub Releases](https://github.com/wintopic/codex-longwatch/releases) 下载：

- Windows：安装程序或便携 ZIP
- Linux：DEB 安装包、AppImage 或便携 tar.gz
- macOS：arm64/x64 DMG 或便携 `.app.tar.gz`

发布资产同时提供 SHA-256 校验文件。当前自动构建尚未签名或公证，Windows 与 macOS 可能显示系统常见的未知发布者提示。

### 从源码构建

安装 Rust 1.85+ 与 GPUI 所需的本地图形依赖：

```bash
git clone https://github.com/wintopic/codex-longwatch.git
cd codex-longwatch
cargo build --locked --release --all-features
```

生成文件：

```text
target/release/codex-longwatch.exe   # Windows
target/release/codex-longwatch       # macOS / Linux
```

Linux 还需要 Fontconfig、X11/XCB、Wayland、EGL 和 OpenGL 开发包，具体可参考 CI 工作流里的 Ubuntu 安装列表。

## 使用方法

1. 输入一条真实、完整的 Codex 任务。
2. 按需选择工作目录。
3. 按 `Ctrl+Enter` 或点击主按钮。
4. 让 Longwatch 保持运行，它会维持单回合串行并处理可重试故障。
5. 完成后从系统通知、主界面或 Windows 托盘打开结果。

任务运行期间，任务文本和重试参数会锁定；提醒选项与 GUI 兼容开关可以实时修改。点击“暂停”会尝试中断当前回合但保留可恢复状态，点击“停止”会清除 Longwatch 保存的线程关联。

## GUI 兼容回退

这一通道默认关闭，且只有用户明确打开后才会被尝试。

- **Windows：** 通过 UI Automation 找到 Codex 输入框，临时使用剪贴板提交一次，再恢复文字剪贴板。
- **macOS：** 先请求 Accessibility 权限，再查找并提交到 Codex 输入框。
- **Linux：** 使用 AT-SPI 定位并通过 X11 发送一次 Return；Wayland 会话，包括 XWayland，会明确拒绝全局输入注入。

完成状态来自 `CODEX_HOME/sessions/**/*.jsonl` 的新增记录。启动时先建立已有文件偏移基线，并处理半行、轮换与重复事件。由于 GUI 通道无法可靠切换 Codex 工作目录，配置工作目录时会明确拒绝回退。

## 隐私与安全边界

- Longwatch 完全在本机运行，只与本机安装的 Codex CLI 通信。
- 它继承现有 `CODEX_HOME`、供应商设置与 Codex 登录状态。
- 它不读取、不索取、也不保存 API key。
- 项目不包含 WebView、Node 运行时、本地 HTTP 服务、远程控制服务、遥测客户端或账号切换器。
- 一键复制的支持报告会排除任务原文、配置文件、环境变量和凭据，但可能包含应用/系统版本、队列元数据、线程 ID、有限长度的回复/错误预览与近期 Longwatch 日志。
- GUI 自动化必须显式开启，并受平台无障碍接口约束。Windows 回退目前只保证恢复文字剪贴板内容。

公开分享 `config.json` 或日志前请先自行检查，其中可能包含任务原文、工作目录与运行细节。

## 故障排查

### Codex CLI 不可用

先在正常终端中执行 `codex --version`，再确认高级设置中的可执行文件路径。Longwatch 继承启动它的进程环境。

### 一直显示正在重连

从高级设置打开日志目录，查看最新的 `longwatch.log.*`。其中包含 app-server stderr 与连接、重试时间线。

### 状态保存失败

检查磁盘空间、目录权限与安全软件。普通瞬时写入失败不会立刻终止内存中的任务；如果失败发生在提交前的关键边界，Longwatch 会暂停以避免重复发送。

### 配置损坏

Longwatch 会把无效文件改名为 `.corrupt-<时间戳>` 并使用安全默认值启动。

### 配置与日志位置

准确路径由系统决定。常见位置为 Windows 的 `%APPDATA%\Longwatch\config`、macOS 的 `~/Library/Application Support/Longwatch`，以及 Linux 的 `~/.config/longwatch`。日志与崩溃报告位于同一个 Longwatch 目录下；已有安装会自动迁移旧版 Longwatch 或 CodexQueue 配置目录。

## 开发与验证

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-features --all
cargo build --locked --release --all-features
```

集成测试自带假 app-server，覆盖持久线程恢复、权威完成项、高需求重试、内部重试所有权、进程崩溃、中断、不确定提交对账与禁止重复发送等场景。CI 覆盖 Windows、Linux、macOS Intel 与 Apple Silicon；推送版本标签后会自动生成各平台安装包、便携包与 SHA-256 校验文件。

贡献方式见 [CONTRIBUTING.md](CONTRIBUTING.md)，安全问题见 [SECURITY.md](SECURITY.md)。

## 项目状态

Longwatch 仍处于早期阶段。Codex CLI 的 app-server 协议可能变化，自动发布也尚未完成签名或公证。升级前请阅读发布说明，并为重要工作保留备份。

## 名称与商标声明

Longwatch for Codex 是独立、非官方的开源项目，与 OpenAI 不存在隶属、授权、赞助、背书或官方支持关系。Codex、ChatGPT、OpenAI 及相关名称和标志归各自权利人所有。仓库中的标志仅用于说明产品兼容性，不代表官方身份。

## 许可证

本项目采用 [Apache License 2.0](LICENSE)。

## 致谢

- 基于 [GPUI](https://crates.io/crates/gpui) 与 [TDesign-GPUI](https://github.com/wintopic/TDesign-GPUI) 构建。
- 深链、GUI 输入和会话观察方向受到 [CodexPanel](https://github.com/wintopic/CodexPanel) 启发；Longwatch 为独立实现，不包含其非商业许可证约束下的源码。
