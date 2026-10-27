# OpenBuild

把 **OpenBuild 内核** 接到 **Rust TUI** 上：TUI 仍然是原来的全屏界面、鼠标命中、滚动与选区，后端换成通过 **ACP over stdio** 运行的内核。

```
openbuild.exe (Rust TUI)
        │  NDJSON / JSON-RPC 2.0 (ACP)
        ▼
openbuild-kernel.exe (Bun)
        │  HTTP + SSE（本机回环）
        ▼
OpenBuild core（会话、工具、权限、MCP、Skill、模型）
```

本仓库是 `docs/openbuild-implementation-plan.md` 的落地实现：**M0 骨架 + M1 主链路 + M2 权限 + M3 会话/模型 + M4 压缩可见性 + M5 诊断**，并包含计划中的 **P1–P3 内核补丁**。

## 当前状态

已通过真实运行验证：

- `openbuild-kernel.exe acp` 能响应 `initialize`，返回 `agentCapabilities`、9 个可用模型、3 个命令。
- `session/new` 返回 `sessionId` 与 `configOptions`（model / effort / mode）。
- Rust 侧子进程桥 `openbuild-backend` 能完成 `initialize → session/new` 往返。
- `cargo test -p openbuild-backend`：35 个单测全部通过。
- `cargo check -p xai-grok-pager`：通过。
- `bun run typecheck`（packages/cli）：通过（1,886 文件）。

已实现的映射/补丁：

| 项 | 内容 |
| --- | --- |
| P1 | 内核每条 `session/update` 在 **通知层** 注入 `promptId` / `turnStartMs` / `streamStartMs` / `eventId`；`session/load` 重放注入 `isReplay` |
| P2 | `websearch` 的 `kind` 改为 `search`，title 改为 `Web search: <query>` |
| P3 | `initialize` 返回 `_meta.modelState`（模型、上下文窗口、图片能力、effort）与 `availableCommands` |
| G1 | `openbuild-backend`：profile / 子进程 transport / 工具名归一 / `todowrite`→`Plan` / shell 结构化输出 |
| G3 | `connect()` 按 `OPENBUILD_BACKEND` 或同目录内核分派；`AgentLocation::Subprocess` |
| G4 | 事件循环入口统一 `normalize_message`，不修改 3,011 行的 tracker |
| M3 | `config` 模块把内核 `configOptions` 翻译成模型/effort/模式；`/model`、`/effort`、Shift+Tab 在子进程后端走 `session/set_config_option`；`config_option_update` 回写 `ModelState` |
| M4 | `adapter/compaction` 把内核压缩标记翻译成 TUI 的 `x.ai/session_notification`，压缩过程与失败原因可见 |
| M5 | 内核崩溃可见且可恢复：`health` 模块归集退出原因与 stderr，事件循环推系统块（退出码 + stderr 尾部），`/reconnect` 重启内核并对每个会话 `session/load` 重放；`/backend` 显示后端类型、内核路径与已声明能力 |
| 品牌 | 全部用户可见名称统一走 `branding` 模块（`OpenBuild`）；crate 名、环境变量与 `~/.grok` 路径保持上游拼写 |

### 品牌覆盖范围

`OpenBuild` 出现在所有用户可见的位置：CLI 的 `--version` / `--help` / `version`、
欢迎页、隐私横幅、设置弹窗、通知标题、`doctor` 报告、`update` 提示、权限弹窗标题、
`/backend` 输出。

**故意保留上游拼写**的地方（改动会破坏兼容性）：

| 保留项 | 原因 |
| --- | --- |
| crate 名（`xai-grok-*`） | 影响 `Cargo.toml` / 依赖图，与用户无关 |
| 环境变量（`GROK_*`） | 用户脚本与 CI 已在用 |
| `~/.grok` 目录 | 已有用户数据在此 |
| `# >>> grok doctor >>>` 标记 | 写入用户 shell 配置的托管块标记，改动会导致历史块无法识别 |
| `xAI.GrokBuild`（WinGet 包 ID） | 上游包标识 |
| 模型名（`Grok 4.5` 等） | 模型标识，不是产品名 |
| 主题名（`Grok Night` / `Grok Day`） | 主题标识 |

## 目录

```
openbuild/
├── README.md
├── build.ps1              # 构建内核 + TUI，产出 dist/
├── run.ps1                # 启动 TUI（自动指向 dist 内核）
├── verify.ps1             # 后端单测 + 真实 ACP 握手
├── dist/                  # openbuild.exe / openbuild-kernel.exe
├── patches/               # 两个上游仓库的完整改动（可复现）
├── vendor/opencode/       # 内核源码 @ b252621 + P1–P3
└── tui/grok-build/        # TUI 源码 @ 2bdd1d6 + openbuild-backend
```

## 快速开始

前置：Windows、PowerShell 7、Bun 1.4.4、Rust 1.94+、protoc 29.x。

```powershell
# 1) 构建（首次会安装依赖，耗时较长）
.\build.ps1

# 2) 启动
.\run.ps1
```

`run.ps1` 会设置：

```powershell
$env:OPENBUILD_BACKEND = "opencode"
$env:OPENBUILD_KERNEL  = "<repo>\dist\openbuild-kernel.exe"
```

然后启动 `dist\openbuild.exe`。

> 本工作区附带的 `dist\openbuild.exe` 是调试构建（已用 `editbin /STACK:33554432` 扩大主线程栈）；`build.ps1` 默认生成 release 构建。

### 只验证内核

```powershell
'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}' |
  .\dist\openbuild-kernel.exe acp
```

### 只验证 ACP 桥

```powershell
cd tui\grok-build
$env:PROTOC = "<path-to-protoc.exe>"
cargo run -p openbuild-backend --example handshake -- ..\..\dist\openbuild-kernel.exe ..\..
```

可选 `OPENBUILD_PROMPT="say hi"` 跑一轮 prompt。

## 环境变量

| 变量 | 作用 |
| --- | --- |
| `OPENBUILD_BACKEND` | `opencode` 强制走子进程；`grok` 强制走原 grok shell；未设置时若同目录存在内核则自动走 opencode |
| `OPENBUILD_KERNEL` | 内核可执行文件路径 |
| `OPENBUILD_PROMPT` | 仅 handshake 示例使用：发送一轮 prompt |
| `PROTOC` | 构建期 protoc 路径（Windows 必需） |
| `OPENCODE_VERSION` | 构建内核时覆盖版本号（本仓库使用 `2.1.4`，避免 detached 版本触发 free-tier 门槛） |

## 在 TUI 里查看后端

输入 `/backend` 会打印当前后端、内核路径和已声明能力。能力决定菜单项是否存在：
`config options` 关闭时模型/模式切换会退回 `session/set_model`，`queue extension`
关闭时 `/queue` 不可用，等等。排查「某个功能不见了」时先看这里。

## 内核崩溃与 `/reconnect`

内核是一个独立进程。它意外退出时（任务管理器结束、崩溃、被系统回收）：

1. 界面上出现一个系统块，写明「OpenBuild kernel exited with code N」，
   并附上内核 stderr 的最后几行；
2. 正在跑的轮次会被收回，输入框不会假装还在等待；
3. 在修复原因后输入 `/reconnect`：TUI 会启动一个新的内核、重新握手，
   并对每个打开的会话做一次 `session/load`，历史由内核重放回来。

退出原因和 stderr 由 `openbuild-backend` 的 `health` 模块归集；TUI 主动退出
不会被误报成崩溃（那种情况记为 `UserCancel`，直接退出）。

也可以脱离 TUI 单独验证这条链路：

```powershell
cd tui\grok-build
$env:PROTOC = "<path-to-protoc.exe>"
cargo run -p openbuild-backend --example crash -- ..\..\dist\openbuild-kernel.exe ..\..
```

它会握手、杀掉内核、打印捕获到的退出码与 stderr，并额外验证「主动停止不算崩溃」。

## 构建期修复（Windows）

上游构建脚本有两处 Unix 假设，本仓库已修：

1. `crates/build/xai-proto-build/src/lib.rs` 用 `/dev/stdout`、`/dev/null` 做 protoc 依赖输出；Windows 上改为临时文件。
2. `Cargo.lock` 中 `cc 1.2.48` 在 Rust 1.94 / Windows 上类型不匹配；升级到 `cc 1.10.0`。

## 已验证证据

以下输出来自本机真实运行（Windows 11 / PowerShell 7 / Bun 1.4.4 / Rust 1.94.0）：

```text
opencode v2.1.4
PROFILE BackendProfile { kind: OpenCode, ... child_session_updates: true, config_options: true, ext_queue: false ... }
SESSION {"sessionId":"ses_...","configOptions":[{"id":"model",...},{"id":"effort",...},{"id":"mode",...}]}
NOTIFY {"_meta":{"eventId":"ses_...-1","promptId":"openbuild-handshake-29200","streamStartMs":...,"turnStartMs":...},
        "sessionId":"ses_...","update":{"sessionUpdate":"agent_message_chunk",
        "content":{"type":"text","text":"Hi! 👋 ..."}}}
PROMPT {"stopReason":"end_turn","usage":{"totalTokens":5252,...},"_meta":{}}
```

- `cargo test -p openbuild-backend`：35 passed。
- `cargo run -p openbuild-backend --example crash`：`PASS kernel exit detected (code: Some(1))` + `PASS a user stop stays a user stop`。
- `cargo test -p xai-grok-update`：137 passed。
- `cargo check -p xai-grok-pager`：Finished。
- `bun run typecheck`（packages/cli）：No type errors in 1,886 files。
- `cargo test -p xai-grok-pager --lib`：**10,090 passed / 104 failed**（失败集合与改动前逐条一致，全部为 Windows 平台差异，见「已知限制」）。
- 内核 `--version`：`opencode v2.1.4`（内核自身版本号沿用上游标识，不影响 UI 品牌）。
- TUI `--version`：`openbuild 1.0.45`。

## 已知限制

- **权限弹窗**只提供「一次性 / 总是 / 拒绝」三选一。内核的权限模型是 action/resource 规则，
  与 grok 的「命令词高亮」不同构，因此「允许这条命令」的精细行不显示。
- **压缩摘要**只显示开始/完成/失败，不带 token 前后对比（内核不在标记里给这两个数）。
- **交互式 TUI 的 PTY 级验收**尚未执行。上游的 `xai-grok-pager-pty-harness`
  在 Windows 上本来就跑不起来（上游自带的 `welcome_screen` 用例同样零输出），
  所以「杀内核 → 提示 → 重连」的传输层由 `openbuild-backend --example crash` 覆盖，
  TUI 层需人工确认一次。
- **Windows 上的上游测试失败**：`cargo test -p xai-grok-pager --lib` 有 104 个用例失败，
  全部与本次改动无关，按根因分为：
  - `InputOutcome::Changed` 断言（15）：依赖 Unix 终端按键解码。
  - 路径断言（`CwdNotAbsolute`、`file://`、`/tmp/...`、`Temp\.tmpXXX` 与 `plugin-source` 比较）：
    集中在 `app::dispatch`、`app::agent_view`、`scrollback`、`diagnostics::fix`。
  - `PlatformUnsupported`（2）与 `sh`/`printf` 缺失（4，`notifications::hooks`）。
  - 其余为 Ctrl+i 快捷键表、`doctor`/`git_info`/`disk_usage` 的 Unix 命令输出差异。

  品牌相关模块全部通过：`views::welcome` 105、`views::settings_modal` 190、
  `views::privacy_banner` 5、`branding` 2、`notifications::protocol` 14。

## 许可证

- 内核（源自 opencode）：MIT
- TUI（源自 grok-build）：Apache-2.0

分发时保留两份 LICENSE 与 `THIRD-PARTY-NOTICES`。