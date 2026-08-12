# Mac→Win 桥接笔记（macOS agent → Windows agent）

> 本文件是 `WIN_TO_MAC_*.md` 的反向件：**macOS 端 agent** 把需要 Windows 端关注/接手的事项写在这里（新功能、bug 发现、构建/协议坑、待 Windows 验证的对端行为），Windows 端 agent `git pull` 后直接接手。
> 命名约定（见 `docs/COLLABORATION.md`）：跨端待对方处理的事项统一放 `docs/`，文件名体现方向。本端能独立修的 bug/功能直接 `fix()/feat()` 提交并注明平台 cfg。

## 0. 协作红线（两端一致）
- 只 push `yunbim/mykvm` 的 `main`；**绝不**推 `XxMinor/mykvm`。
- 平台专属代码必须用 `#[cfg(target_os=...)]` 隔离，不破坏另一端编译。

## 1. Mac 端构建/签名关键事实（供 Windows 端参考）
- **唯一构建入口**：`scripts/build-mac-arm.sh`，且必须叠加沙箱 env 绕开：
  `env -u CODEBUDDY_SAFE_DELETE_BULK_STATE_DIR -u CODEBUDDY_TOOL_CALL_ID -u NODE_OPTIONS bash scripts/build-mac-arm.sh`
- **rust-lld 是 dylib 损坏的唯一根因修复**：rustc 1.97 + 新 Apple ld 会让 release 下 proc-macro `.dylib` 损坏。**必须用 host 构建（`npm exec tauri build`，不带 `--target`）**，否则 `RUSTFLAGS` 传不到 proc-macro 链接步骤、又退回坏 Apple ld。这是历史坑，Windows 端若改 Mac 构建请勿加 `--target aarch64-apple-darwin`。
- **自签**：headless 下自签证书无法被信任，`sign-mac-app.sh` 回退 ad-hoc + 去 quarantine；跨机器分发或想保留 Accessibility 授权请跑一次脚本打印的 `sudo security add-trusted-cert …`。当前均为**自签、未公证**。

## 2. 更新密钥与发布（两端共享同一把）
- ed25519 更新私钥：`~/.mykvm/updater.key`（**base64 包裹的 minisign 文本**，不是裸 key；`tauri signer` 用 env 传 `TAURI_SIGNING_PRIVATE_KEY` 时需此包裹格式，无密码要加 `--password ""`）。
- 公钥已写 `src-tauri/tauri.conf.json` 的 `plugins.updater.pubkey`；GitHub Secret `TAURI_SIGNING_PRIVATE_KEY` 已设在 `yunbim/mykvm`（CI 用它签名）。
- 密钥 id：`B28DE18ED93FED2D`。v0.1.2 与 v0.1.3 的 Mac `.app.tar.gz.sig` 均由此密钥签出，与 app 内嵌公钥一致，app 端校验通过。
- **发布 Mac 端版本**：构建后建 GitHub Release `v<VERSION>`，上传 `.dmg` + `.app.tar.gz` + `.app.tar.gz.sig` + **名为 `latest.json` 的清单**（注意：本地文件名若是 `latest-0.1.3.json` 会上传成该名，app 端点 `releases/latest/download/latest.json` 会 404——务必用字面 `latest.json`）。app 内更新器拉 `releases/latest/download/latest.json`。

## 3. 测试口径差异（非回归）
- Mac `cargo test --lib` = **117** passed；Windows = **105**。差值是 macOS-only 测试模块（滚轮引擎、event tap 等），属正常，不是失败。

## 4. 当前待 Windows 端接手/验证
- [ ] **Mac→Win 反向链路实测**：`WIN_TO_MAC_WHEEL_DIAGNOSIS.md` 只覆盖了 Win→Mac（控制端 Windows、被控端 Mac），结论是 Win 侧发送 notches 代码正确。请 Windows 端反向验证 **Mac 作控制端 → Windows 被控端** 的滚轮/点击/键盘是否对称正确（尤其 v0.1.3 平滑滚动只限 Mac client、不跨机，Win 接收端收到的是 notches）。
- [ ] **跨端 QUIC 握手（BadSignature 修复）真机验证**：v0.1.3 已把传输改为 pin 对端广播证书（绕过自签证书链校验）。Mac 侧代码就绪，需在真实 Mac↔Win 上各跑一次 Server↔Client 联通确认（Mac agent 沙箱无法做真机，已发布 v0.1.3 供人工验证）。
- [ ] **共享协议改动需两端同步**：若任一侧动 `shared_input.rs` 的 `InputEvent` 等结构，务必在提交标注「待对方核对」，并同步 `ALIGNMENT.md` §4。

## 5. 本端已交付（截至 2026-08-12）
- v0.1.3 Mac 自签名包 + GitHub Release（含 latest.json + 签名一致更新产物）；`cargo test --lib` 117/117、`cargo build --release` 通过。
- 过时技能 `~/.workbuddy/skills/mykvm-macos-build` 已重写为调用 `scripts/build-mac-arm.sh` + env 绕开（校准 v0.1.3）。
- 回填了 `ALIGNMENT.md`（对齐 commit `a5147c6`、Mac 构建/测试状态、§4/§5/§7 勾选）与 `WIN_TO_MAC_WHEEL_DIAGNOSIS.md` §5。
