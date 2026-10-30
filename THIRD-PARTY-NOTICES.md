# 第三方组件与来源声明

OpenBuild 由两棵上游源码树改造而来，二者都直接存放在本仓库中并持续维护。
分发本仓库时请保留以下授权与出处。

## 1. 内核 —— `vendor/opencode`

- 上游：<https://github.com/anomalyco/opencode>
- 基线提交：`b2526211ec33d1b5a0d5af21e0c7e2e20e6b1a3c`
- 授权：**MIT**，全文见 [`vendor/opencode/LICENSE`](vendor/opencode/LICENSE)
- 版权：Copyright (c) 2025 opencode

本仓库对内核的改动包括：ACP 适配层扩展、配置发现层、插件检查、出站代理、
内置系统提示词品牌化、应用常量与数据库文件名改为 `openbuild`。

## 2. 终端界面 —— `tui/grok-build`

- 上游：<https://github.com/xai-org/grok-build>
- 基线提交：`2bdd1d6`
- 授权：**Apache License 2.0**，全文见 [`tui/grok-build/LICENSE`](tui/grok-build/LICENSE)
- 版权：Copyright 2023-2026 SpaceXAI
- 附加声明：见 [`tui/grok-build/THIRD-PARTY-NOTICES`](tui/grok-build/THIRD-PARTY-NOTICES)

本仓库对 TUI 的改动包括：品牌替换、`openbuild-backend` ACP 桥接 crate、
`openbuild-server` 中继、主题导入、多 profile 并行、子智能体接管、
`/uncensored` 模式、`/login` 提供商向导与子代理思考视图。

## 3. 目录内其他第三方代码

`vendor/opencode` 与 `tui/grok-build` 各自带有完整的依赖授权文件：
每个 package 或 crate 目录下的 `LICENSE`、`NOTICE`、`THIRD-PARTY-NOTICES`
保持原样，未作修改。

## 4. 商标

`opencode`、`grok`、`Grok`、`xAI` 等名称与标识归其各自权利人所有。
本仓库以 OpenBuild 名义分发，不代表上游项目，也未获得上游背书。

## 5. 本仓库自有代码

除上述两棵上游树之外，本仓库自行编写的部分（构建脚本、`openbuild-backend`、
`openbuild-server`、以及标注为 OpenBuild 的改动）同样以 **MIT** 授权发布，
全文见根目录 [`LICENSE`](LICENSE)。
