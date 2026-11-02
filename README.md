# OpenBuild

一个**完整可运行**的 AI 编程 Agent：Rust 全屏 TUI 负责界面，Bun 内核负责会话、工具、权限、MCP 与模型调用。两个二进制通过 ACP over stdio 通信，打包成同一个产品对外使用。

```
openbuild.exe          ← 你看到的界面（Rust TUI，全屏、鼠标、滚动、选区）
      │  ACP over stdio
      ▼
openbuild-kernel.exe   ← 干活的那一层（Bun：会话、工具、权限、MCP、模型）
```

它来自两个上游项目的再组合：内核基于 [opencode](https://github.com/anomalyco/opencode)（MIT），TUI 基于 grok-build（Apache-2.0）。OpenBuild 负责把它们接成一个能直接用的整体，并在此之上加了若干面向实际使用的增强。

---

## 30 秒上手

### 你已经装过

```powershell
openbuild
```

安装目录在 `%LOCALAPPDATA%\OpenBuild`，三个二进制（TUI、内核、ACP 服务端）都在那里。

### 从源码跑

需要：**Windows**、**PowerShell 7+**、**Bun 1.4.x**、**Rust 1.94+**、**protoc 29.x**。

```powershell
# 1) 构建三个二进制到 dist\
.\build.ps1

# 2) 直接启动（自动指向 dist\ 里的内核）
.\run.ps1
```

`bun` 和 `protoc` 要在 PATH 里；不想改 PATH 就用环境变量指过去：

```powershell
$env:OPENBUILD_BUN = "C:\path\to\bun.exe"
$env:PROTOC        = "C:\path\to\protoc.exe"
.\build.ps1
```

首次构建要装依赖并编译 Rust，耗时较长；之后是增量构建。

### 安装到本机（可选）

```powershell
.\install.ps1              # 构建 + 安装
.\install.ps1 -SkipBuild   # 装 dist\ 里已有的产物
```

装到 `%LOCALAPPDATA%\OpenBuild`，并把这个目录加进用户 PATH。内核和 TUI 放同一目录，TUI 会自动找到内核，不需要再设 `OPENBUILD_KERNEL`。

> **改完代码想生效**：重新 `.\build.ps1`，然后 `.\install.ps1 -SkipBuild`。只在 `dist\` 里编译、不重新安装的话，`openbuild` 命令跑的还是旧版本。

---

## 第一次运行

启动后直接打字提问即可。几个最常用的入口：

| 想做什么 | 做法 |
| --- | --- |
| 看当前后端、内核路径、连接状态 | `/backend` |
| 换模型 / 换思考档位 | `/model`、`/effort` |
| 切换 计划模式 / 自动批准 | `Shift+Tab`，或 `/plan`、`/always-approve` |
| 新会话 / 恢复旧会话 | `/new`、`/resume` |
| 内核崩了想拉起来 | `/reconnect` |
| 看帮助 | `/help` |

第一次用建议先确认两件事：`/backend` 显示的后端是不是 `OpenCode`，以及当前模型是否可用。模型和 provider 的配置见下面「接入第三方模型」。

---

## 无审查模式（v8：人格包）

这一版**不再是一串"不许拒绝"的规则**。它把参考项目 [haiou2.0-Claude-Code](https://github.com/haiou-666/haiou2.0-Claude-Code-) 的两份正文原样搬进 OpenBuild：`CLAUDE.md`（海鸥人格 + 安全研究项目情境 + 558 组问答示例）与 `system-prompt.md`（GameShield 项目简报）。

和 Claude Code 插件一样，这两份正文**就是 agent 的系统提示本体**——不是追加在末尾的附注，而是模型回答时所在的"身份"。打开无审查，等于把这个 agent 切到前台。

### 用法

```text
/uncensored                 # 弹出菜单：开启 / 记住 / 状态 / 编辑人格
/uncensored on              # 直接切到人格 agent
/uncensored off             # 切回 build agent
/uncensored status          # 看当前模式、挂载的人格、请求体规则
/uncensored persona <id>    # 换一个人格正文（seagull / brief）
/uncensored edit            # 用 $EDITOR 打开当前人格正文
```

选「记住」会写盘，之后的新会话默认按这个状态启动。

### 为什么比 v7 更难被拒

v7 是"告诉模型它不该拒绝"，本质仍是在和模型的默认倾向谈判；这版换掉了谈判对象：

| | v7 | v8 |
| --- | --- | --- |
| 注入内容 | 约 1.3 KB 规则清单 | 30 KB 人格正文 + 10 KB 项目情境 |
| 注入性质 | 追加的约束 | agent 的身份本身 |
| 位置 | 系统提示首位的一段 | 系统提示首位，且是整段人格 |
| 说服方式 | 声明"这是授权环境" | 558 组示例规定它怎么说话、怎么接活 |
| 是否要求模型自我声明 | 是 | 否，人格设定里本来就不是 AI |

规则清单要求模型否定自己的判断，长规则之间还会互相冲突，模型倾向选一个"安全默认"，也就是半拒绝或半成品。人格 + 项目情境 + 大量范例不给它这个默认选项：它回答时是在扮演一个具体的人，接的是一个具体项目里的活。

### 安装位置

```text
<config>/uncensored/persona/seagull.md   # 人格正文（海鸥 Character Sheet）
<config>/uncensored/persona/brief.md     # 项目情境（GameShield Security Toolkit）
<config>/uncensored.json                 # 开关与配置（v4）
```

这两份文件是**用户可见、可直接改**的内容：升级不会覆盖你已经改过的副本。想让 agent 换个性格，直接改 `seagull.md`，或用 `EDITOR` 打开它。

### 配置（v4）

```json
{
  "version": 4,
  "enabled": false,
  "scope": "new-sessions",
  "mode": "persona",
  "persona": "seagull",
  "mounts": ["brief"],
  "providerRules": "auto",
  "providerBody": {},
  "rewriteRefusalHistory": true,
  "refusalRetryBudget": 2
}
```

- `persona` / `mounts`：加载哪份人格正文、再叠加哪些正文。
- `providerRules`：按 provider 深合并请求体（例如 Google 系关闭安全过滤项），不覆盖你自己配的字段。
- `rewriteRefusalHistory`：上一轮模型拒绝了，下一轮请求里把它自己那条拒绝改写成同意，避免"保持前后一致"继续拒绝；只动尾部连续拒绝，带代码或工具调用的回答不动。
- `refusalRetryBudget`：一次会话内的自动重试预算，由内核统一扣。

旧的 v1/v2/v3 文件读取时自动映射（`packs` 变成 `mounts`），不需要手动改。

### 仍然不保证的部分

OpenBuild 不做内容过滤，也不替你拦请求。模型自己的权重倾向、以及 provider 服务端的审核，都不在客户端可控范围内。这一版把"模型因话题本身拒绝"的概率压到最低，但它不是对服务端审核的绕过。

**工具权限、沙箱、文件系统边界完全不变**——它是内容策略模式，不是权限开关。

实现见 [`docs/openbuild-uncensored-persona.md`](../docs/openbuild-uncensored-persona.md)。

## 常用功能

### 接入第三方模型

内置模型之外的服务（OpenAI 兼容中转、自建网关、Ollama / vLLM 本地推理、OpenRouter 等）走配置文件接入，不用改 TUI。

```powershell
notepad "$env:USERPROFILE\.config\opencode\openbuild.jsonc"
```

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

- **`activation: "enabled"` 不能省**。只写 `settings.apiKey` 会静默隐藏该 provider。
- **删除 provider 不会立即生效**。内核是单向热加载：新增立即出现，删除需要 `openbuild-kernel service restart`（或 TUI 里 `/kernel restart`）。

字段含义：`activation` 控制是否启用，`package` 指定协议适配器，`env` 列出存放密钥的环境变量，`settings` 原样传给该协议，`models` 声明模型清单与上下文窗口。

### 双内核并排（`/pane`）

在同一个 TUI 里开第二个内核，左右分栏，各自有独立的会话、历史和数据库：

```text
/pane open secondary      # 打开右侧 pane
/pane focus right         # 切换焦点（也可以直接点）
/pane close               # 关掉右侧
/pane list                # 看每个 pane 的 profile、模型与焦点
```

一个 profile = 后端 + 模型 + 工作区。第二个 pane 默认用私有存储目录隔离，杀掉一个不影响另一个。

### 插件与 MCP

内核自带 `plugin` / `mcp` 子命令，TUI 直接调用并展示结果：

```text
/kernel                    # 列出插件与 MCP 服务器
/kernel plugin add <pkg>
/kernel mcp add <name> -- <command...>
/kernel mcp add <name> --url <url>
/kernel restart            # 重启内核，让"删除"生效
```

### 主题

支持 opencode TUI、opencode 桌面版、VSCode 三种主题格式：

```text
/theme import ~/themes/catppuccin.json
/theme list
/theme catppuccin
/theme grokday              # 切回内置
```

### 作为 ACP 服务端接入 Zed / Neovim

`openbuild-server.exe` 把内核包装成标准 ACP agent，走 stdio：

```powershell
.\dist\openbuild-server.exe --kernel .\dist\openbuild-kernel.exe --cwd .
```

把外部客户端的 ACP 启动命令指向它即可。这是单客户端中继：一个 server 进程对应一个客户端和一个内核。

---

## 环境变量

| 变量 | 作用 |
| --- | --- |
| `OPENBUILD_BACKEND` | `opencode` 强制走子进程内核；`grok` 走原 grok shell；不设时若同目录有内核则自动走 opencode |
| `OPENBUILD_KERNEL` | 内核可执行文件路径 |
| `OPENBUILD_PROFILE_ROOT` | `/pane` 各 profile 的私有根目录（默认 `~/.openbuild/profiles/<label>`） |
| `OPENBUILD_THEME_DIR` | 导入主题的存放目录（默认 `~/.grok/themes`） |
| `OPENBUILD_HTTP_FEED` | 设为 `0` 关闭只读 HTTP 事件侧通道 |
| `OPENBUILD_UPDATE_BASE` / `_CHANNEL` / `_VERSION` | 自更新通道设置 |
| `OPENBUILD_BUN` / `PROTOC` | 构建期工具路径 |
| `OPENCODE_VERSION` | 构建内核时覆盖版本号（本仓库用 `2.1.4`） |

---

## 验证与排错

### 一条命令跑完主要检查

```powershell
.\verify.ps1
```

它会跑后端单测、无审查插件单测、真实 ACP 握手。

### 只验证内核能起来

```powershell
'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1}}' |
  .\dist\openbuild-kernel.exe acp
```

### 界面里某个功能不见了

先看 `/backend`。它打印后端类型、内核路径和内核声明的能力，菜单项是否出现由能力决定：`config options` 关闭时模型切换会退回旧路径，`queue extension` 关闭时没有 `/queue`。

### 内核崩了

界面会出现系统块写明退出码和内核 stderr 尾部，输入框不会假装还在等。修好原因后 `/reconnect` 重启内核并重放会话历史。

### 构建期注意事项

上游两处 Unix 假设已在本仓库修好：

1. protoc 依赖输出原本写 `/dev/stdout`、`/dev/null`，Windows 上改为临时文件。
2. `Cargo.lock` 里 `cc` 升到 1.10.0，否则在 Rust 1.94 / Windows 上类型不匹配。

---

## 项目结构

```text
openbuild/
├── README.md
├── build.ps1             # 构建内核 + TUI + ACP 服务端 → dist/
├── run.ps1               # 启动 TUI，自动指向 dist/ 里的内核
├── verify.ps1            # 后端单测 + 插件单测 + 真实 ACP 握手
├── install.ps1           # 安装到本机并加入 PATH
├── dist/                 # 构建产物（不进仓库）
├── tests/                # 无审查插件的 Bun 单测
├── vendor/opencode/      # 内核源码
└── tui/grok-build/       # TUI 源码 + openbuild-backend + openbuild-server
```

两棵 vendored 源码树不是我们的仓库。它们基于固定上游基线（opencode `b2526211ec`、grok-build `2bdd1d6`），改动已经直接落在这棵树里，克隆下来即可构建，不需要额外打补丁。

---

## 已知限制

- **权限弹窗**只有「一次性 / 总是 / 拒绝」三选一，没有按命令词的精细允许。
- **压缩摘要**只显示开始 / 完成 / 失败，不带前后 token 对比。
- **HTTP 直连侧通道**已连接并可见（`/backend` 有事件计数），但还没有 diff 视图和用量面板。
- **ACP 服务端**是单客户端 stdio 中继，relay 示例覆盖了 initialize / session/new / prompt，Zed / Neovim 真实客户端接入未做端到端验收。
- **Windows 上的上游测试**有一批与本次改动无关的失败（Unix 终端按键解码、路径断言、`sh`/`printf` 缺失等），按根因分类记录在验证输出里。

---

## 许可证

- 内核（源自 opencode）：MIT
- TUI（源自 grok-build）：Apache-2.0

分发时保留两份 LICENSE 与 `THIRD-PARTY-NOTICES.md`。
