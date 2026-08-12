# 双 Agent 协作约定（Windows ↔ macOS）

本仓库由两端的 AI 协作开发：**Windows 侧 agent** 与 **macOS 侧 agent**，通过 fork `git@github.com:yunbim/mykvm.git` 的 `main` 分支共享进展、直接接手。

## 工作流
- 两端都**直接 `commit` + `push` 到 `yunbim/mykvm` 的 `main`**（以 fast-forward 为主，避免分叉）。
- 另一端 `git fetch` / `git pull --ff-only` 即可看到对方改动并直接接手，无需中转。
- **绝不** push 到上游 `XxMinor/mykvm`。

## 什么该提交到 git
- **跨端需要对方改进的部分**：诊断文档、协议对齐清单、对方需修的 bug 说明 —— 统一放 `docs/` 下，文件名体现方向，例如 `WIN_TO_MAC_WHEEL_DIAGNOSIS.md`、`MACOS_HANDOVER.md`。
- **自己这端能独立修的 bug / 新功能**：直接提交代码，提交信息用 `fix(...)` / `feat(...)` 风格并写明影响范围（注明是否仅本平台 cfg）。

## 给另一端 agent 的说明
- **Win 侧只能编译 Windows 包**，无法编 macOS(`aarch64-apple-darwin`) 包；Mac 专属代码（`#[cfg(target_os = "macos")]`）靠 `cargo check` + 人工核对，真机验证归 Mac 侧。
- 反之 **Mac 侧无法验证 Windows 运行时**，Win 侧负责 Windows 真机验收。
- **协议层必须两端一致**：`shared_input.rs` 的 `InputEvent` 等共享结构改动需同步，并在提交里标注「待对方核对」。
- 新手用户（命令行不熟）：给他的命令只用单行 `&&` 串联，不要用 markdown 代码围栏（反引号会被 zsh 当成命令替换，静默进入 `bash` 子进程导致命令不执行）。

## 当前文档索引
- `docs/ALIGNMENT.md` — Mac↔Windows 跨平台对齐清单（已实现 / 未对齐项）。
- `docs/MACOS_HANDOVER.md` — Win→Mac 交接总览（架构、9 项问题现状、高优先级疑点 P0/P1/P2）。
- `docs/WIN_TO_MAC_WHEEL_DIAGNOSIS.md` — Win 控制 Mac 滚轮失效的根因分析与 Mac 侧修复步骤。
- `docs/MAC_BUILD_TEST_RUNBOOK.md` — Mac 侧首次构建/测试步骤（含避免嵌套目录、别用代码围栏等新手注意）。
- `docs/COLLABORATION.md` — 本文件（双 agent 协作约定）。
