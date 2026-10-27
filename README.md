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

本仓库是 `docs/openbuild-implementation-plan.md` 的落地实现：**M0 骨架 + M1 主链路 + M2 权限 + M3 会话/模型 + M4 压缩可见性 + M5 诊断**，并包含计划中的 **P1–P3 内核补丁**。M6 已落地主题导入、`/kernel`、子智能体接管桥接、ACP 服务端与 HTTP 直连。

## 当前状态

已通过真实运行验证：

- `openbuild-kernel.exe acp` 能响应 `initialize`，返回 `agentCapabilities`、9 个可用模型、3 个命令。
- `session/new` 返回 `sessionId` 与 `configOptions`（model / effort / mode）。
- Rust 侧子进程桥 `openbuild-backend` 能完成 `initialize → session/new` 往返。
- `cargo test -p openbuild-backend`：59 个单测全部通过（含子智能体与 HTTP feed 用例）。
- `cargo check -p xai-grok-pager`：通过。
- `cargo build -p openbuild-server --release`：通过；`openbuild-server --version` → `openbuild-server 0.1.0`。
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
| M6-D | `adapter/child.rs` 把内核子会话事件展开成 grok 的 `subagent_spawned` / `subagent_finished` 与按子会话 `sessionId` 重投递的更新，复用现有 `subagent_takeover` |
| M6-F | `openbuild-server` crate：把内核作为 ACP agent 暴露给 Zed/Neovim 的 stdio 中继；`build.ps1` / `install.ps1` 已纳入 |
| M6-G | HTTP 直连侧通道：ACP 之外只读订阅 `session.tool.progress` / `session.diff` / 完整 usage；`/backend` 显示连接状态、事件数与最近事件 |
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
├── build.ps1              # 构建内核 + TUI + ACP 服务端，产出 dist/
├── run.ps1                # 启动 TUI（自动指向 dist 内核）
├── verify.ps1             # 后端单测 + 真实 ACP 握手
├── dist/                  # openbuild.exe / openbuild-kernel.exe / openbuild-server.exe
├── patches/               # 两个上游仓库的完整改动（可复现）
├── vendor/opencode/       # 内核源码 @ b2526211ec
└── tui/grok-build/        # TUI 源码 @ 2bdd1d6 + openbuild-backend + openbuild-server
```

`patches/` 里是 13 个按主题拆分的补丁（内核 4 个 + TUI 9 个），
应用顺序与内容见 [`patches/README.md`](patches/README.md)。

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

### 安装到本机（可选）

```powershell
.\install.ps1              # 构建 + 安装
.\install.ps1 -SkipBuild   # 直接安装 dist\ 里现有的产物
```

安装到 `%LOCALAPPDATA%\OpenBuild`，并把该目录加入用户 PATH，之后任意终端里
直接敲 `openbuild` 即可。内核与 TUI 放在同一目录，TUI 会自己找到它，
不再需要设置 `OPENBUILD_KERNEL`。

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
| `OPENBUILD_THEME_DIR` | 覆盖导入主题的存放目录（默认 `~/.grok/themes`） |
| `OPENBUILD_HTTP_FEED` | 设为 `0` / `off` 可关闭只读 HTTP 事件侧通道（默认开启） |
| `PROTOC` | 构建期 protoc 路径（Windows 必需） |
| `OPENCODE_VERSION` | 构建内核时覆盖版本号（本仓库使用 `2.1.4`，避免 detached 版本触发 free-tier 门槛） |

## 在 TUI 里查看后端

输入 `/backend` 会打印当前后端、内核路径和已声明能力。能力决定菜单项是否存在：
`config options` 关闭时模型/模式切换会退回 `session/set_model`，`queue extension`
关闭时 `/queue` 不可用，等等。排查「某个功能不见了」时先看这里。

同一份输出还会显示 HTTP 直连侧通道的状态，例如：

```text
HTTP feed: connected (128 events, 0 dropped, 0 reconnects, last: session.diff)
```

它连接的是 ACP 内核正在使用的那个 `serve` 进程，只读订阅 `/api/event`，
用来补上 ACP 不携带的 `session.tool.progress`、`session.diff` 与完整 usage。
连接失败只会让这行显示 `stopped` / `reconnecting`，不影响对话主链路。

## 作为 ACP 服务端使用（Zed / Neovim）

`openbuild-server.exe` 把内核包装成一个 ACP agent，走 stdio 与外部客户端通信。
它自己拉起内核，所以只需要指向内核路径（默认找同目录的 `openbuild-kernel.exe`）：

```powershell
.\dist\openbuild-server.exe --kernel .\dist\openbuild-kernel.exe --cwd .
```

在 Zed / Neovim 的 ACP agent 配置里，把启动命令指向这个二进制即可。
服务端转发 `initialize`、`session/*`、`prompt` / `cancel` 与权限请求；
通知直接透传，不做 TUI 侧的渲染适配。

> 这是单客户端 stdio 中继：一个 server 进程对应一个外部客户端和一个内核。
> 真实 Zed/Neovim 接入尚未做端到端验收。

## 接入第三方模型提供商

内置模型之外的服务（OpenAI 兼容中转站、自建网关、Ollama / vLLM 本地推理、
OpenRouter 等）通过内核配置文件接入，**不需要改 TUI**：

```powershell
notepad "$env:USERPROFILE\.config\opencode\openbuild.jsonc"
```

> 文件名是 `openbuild.jsonc`（本分支已从 `opencode.jsonc` 改名），
> 旧名仍兼容读取但优先级更低。把配置放到 `%USERPROFILE%\.config\openbuild\`
> 也可以 —— TUI 启动时发现该目录存在就会自动指向它。

```jsonc
{
  "providers": {
    "myprovider": {
      "name": "My Provider",
      "activation": "enabled",
      "package": "@opencode/ai/providers/openai-compatible",
      "env": ["MY_API_KEY"],
      "settings": { "baseURL": "https://api.example.com/v1" },
      "models": {
        "my-model": { "name": "My Model", "limit": { "context": 128000, "output": 16384 } }
      }
    }
  },
  "model": "myprovider/my-model"
}
```

验证：

```powershell
"{}" | .\dist\openbuild-kernel.exe models   # 应出现 myprovider/my-model
```

两个最容易踩的坑：

- **`activation: "enabled"` 不能省**。只写 `settings.apiKey` 而省略 `activation`
  时 provider 会被静默隐藏（已实测）。
- **删除 provider 不会立即生效**。内核常驻背景服务是单向热加载：新增立即出现，
  删除仍会列出。要清掉旧条目跑 `openbuild-kernel service restart`。

完整字段说明、各服务商配方、排错表见 [`docs/openbuild-providers.md`](../docs/openbuild-providers.md)。

## 主题：导入 opencode / VSCode 配色

内置主题之外，可以直接导入外部主题文件。支持三种格式：

| 格式 | 识别依据 | 例子 |
| --- | --- | --- |
| opencode TUI（v1） | 顶层 `theme` 对象 | `vendor/opencode/packages/theme/` 的 `*.json` |
| opencode 桌面主题 | 顶层 `light` / `dark` + `palette` | `vendor/opencode/packages/ui/src/theme/themes/*.json` |
| VSCode 主题 | 顶层 `colors` 对象 | 任意 VSCode `*-color-theme.json` |

```text
/theme import ~/themes/catppuccin.json    # 转换 + 保存 + 立即应用
/theme list                               # 列出内置与已导入，标出当前生效的
/theme catppuccin                         # 以后按 slug 切回
/theme grokday                            # 切回内置主题（同时撤下导入覆盖）
```

导入的主题落在 `~/.grok/themes/<slug>.json`，选择记录在 `~/.grok/themes/active`；
两者都可用 `OPENBUILD_THEME_DIR` 改到别处。

两个实现上的要点：

- **透明色会被合成到背景上**，而不是直接丢弃 alpha。VSCode 常用 `#RRGGBBAA`
  表达叠层，丢 alpha 会把 10% 的红色叠层变成纯红，diff 行会看起来像报错。
- **导入主题不新增 `ThemeKind` 变体**。`ThemeKind` 是 `Copy`、被十几处穷尽匹配、
  并以 `as u8` 存进原子变量；导入主题改用一个运行时覆盖层（`theme::custom`）
  叠在当前选中主题之上，渲染管线（对比度增强、量化、ANSI16 降级）完全复用。
  代价是主题选择器里预览内置主题时会临时挂起覆盖层，否则每行看起来都一样。

## 插件与 MCP：`/kernel`

内核本身已有 `plugin` / `mcp` 子命令，TUI 不重复实现，直接调用并展示结果：

```text
/kernel                       # 同时列出插件与 MCP 服务器
/kernel plugin list
/kernel plugin add <package>
/kernel plugin remove <name>
/kernel mcp list
/kernel mcp add <name> -- <command...>
/kernel mcp add <name> --url <url>
/kernel mcp remove <name>
/kernel restart               # 重启内核服务，让「删除」生效
```

> **为什么需要 `/kernel restart`**：内核的常驻服务是单向热加载 —— 新增插件或
> provider 立即生效，**删除不会**。这与 provider 的行为一致（见上一节）。

内核可执行文件按 `OPENBUILD_KERNEL` → TUI 同目录 的顺序查找，与启动时一致。
每个命令都有 120 秒超时保护：`plugin add` 可能要走网络，但内核卡死不应该连带卡住 TUI。

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

- `cargo test -p openbuild-backend`：59 passed（含 9 个 `adapter::child` 用例与 HTTP feed 用例）。
- `cargo run -p openbuild-backend --example crash`：`PASS kernel exit detected (code: Some(1))` + `PASS a user stop stays a user stop`。
- `cargo build -p openbuild-server --release`：Finished；`openbuild-server --version` → `openbuild-server 0.1.0`。
- `cargo run -p openbuild-server --example relay -- <kernel> <cwd>`：`INITIALIZE agent=OpenCode` → `SESSION ses_...` → `PROMPT stopReason=EndTurn updates=3` → PASS。
- `cargo test -p xai-grok-pager-render`：1,163 passed / 31 failed（共 1,194，31 个均为 Windows 平台差异，与改动前逐条一致）。
- `cargo test -p xai-grok-update`：137 passed。
- `cargo check -p xai-grok-pager`：Finished。
- `bun run typecheck`（packages/cli）：No type errors in 1,886 files。
- `cargo test -p xai-grok-pager --lib`：**10,101 passed / 104 failed**（失败集合与改动前逐条一致，全部为 Windows 平台差异，见「已知限制」）。跑之前必须设 `OPENBUILD_KERNEL` 指向真实内核，否则会多一个环境依赖失败（10,100 / 105）。
- 内核 `--version`：`opencode v2.1.4`（内核自身版本号沿用上游标识，不影响 UI 品牌）。
- TUI `--version`：`openbuild 1.0.45`。

## 已知限制

- **权限弹窗**只提供「一次性 / 总是 / 拒绝」三选一。内核的权限模型是 action/resource 规则，
  与 grok 的「命令词高亮」不同构，因此「允许这条命令」的精细行不显示。
- **压缩摘要**只显示开始/完成/失败，不带 token 前后对比（内核不在标记里给这两个数）。
- **子智能体 takeover** 的桥接层已有单测；真实 `task` 会话的观感仍需人工验收一次。
- **HTTP feed 的 UI 消费**尚未做：`/backend` 能看到连接状态与事件计数，但 `session.diff`
  还没有 diff 视图，完整 usage 也还没有用量面板。
- **ACP 服务端**是单客户端 stdio 中继；relay 示例已覆盖 initialize / session/new / prompt，
  但 Zed/Neovim 客户端接入尚未实测。
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