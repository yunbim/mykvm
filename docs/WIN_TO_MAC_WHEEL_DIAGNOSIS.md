# Win→Mac 鼠标滚轮失效 — 诊断与修复（2026-08-12）

> 结论先行：**这不是 Windows 侧代码 bug，Win→Mac 滚轮的整条链路在两端代码上都正确、且与好版本行为等价。失效是 Mac 侧运行时/授权问题。**
> 本文给 Mac 上的 agent：哪些代码已确认正确（无需改），以及 Mac 侧真正要做的修复。

## 1. 已逐行验证「正确、无需改」的链路

控制方向：Windows = 控制端（capture 并发送），Mac = 被控端（receive 并注入）。

1. **Win 发送端** `src-tauri/src/input.rs` `handle_windows_scroll`（~4952）
   - `delta = ((mouse_data >> 16) as i16 / 120) as i32` → **notch 计数**。
   - `WM_MOUSEWHEEL` → `InputEvent::Scroll { delta_x: 0, delta_y: delta }`。
   - 无 `machine_role` 门控，控制端永远转发。✅

2. **共享线协议** `src-tauri/src/shared_input.rs:18`
   - `Scroll { delta_x: i32, delta_y: i32 }`，注释明确为「wheel notches」。两端同一结构体。✅
   - `INPUT_PROTOCOL = "mykvm.input.v1"`（`input.rs:25`）两端一致。✅

3. **Mac 接收派发** `handle_input_datagram`（~1857）→ `input_event_to_command`（~2383）→ `dispatch_input_command`（~2268）→ `inject_input_command`（~2388）→ `inject_scroll`（Mac，~3314）。
   - 接收路径**无任何 `machine_role` 门控**，scroll 不会被丢弃。✅

4. **Mac 注入** `inject_scroll`（macos cfg, ~3314）→ `macos_scroll::inject_notches`（~3938）
   - 与好版本 `3be2eb0` 的 `inject_scroll`（`scroll_smoothing::enqueue(delta_x, delta_y)`）**最终都走到 `submit((px_y, px_x))`，轴映射一致、行为等价**。✅
   - `enqueue_pixels` 是 `#[cfg(target_os="macos")]`，`mod scroll_smoothing` 未被平台屏蔽（全平台编译），**Mac 能正常编译**。✅

5. **Mac 本地 event tap 透传** `scroll_tap_callback`（~4059）
   - 我们注入的远端 PIXEL 滚动事件带了 `SELF_TAG`（`tag_own_event`），回调在 ~4091 识别后 `return raw` 透传——**不会吞掉、也不会反馈循环**。✅

6. **Mac 接收端 `mark_remote_input`**（`macos_receiver_activity`, ~6416）只动原子量 + 取消 App Nap，**不阻塞注入**。✅

## 2. 真实根因（Mac 侧）

最典型签名：**鼠标能移动，但滚轮/点击/键盘失效** = Mac 上的 MyKVM 未授予 **辅助功能（Accessibility）+ 输入监控（Input Monitoring）** 授权。

- 鼠标移动走 `CGWarpMouseCursorPosition`，**不需要**授权 → 所以「能动」。
- 滚轮/点击/键盘走 `CGEvent::post`，**必须**有 Accessibility + Input Monitoring 授权，否则 **静默失败**（无报错、无异常，事件直接丢弃）→ 表现就是「滚轮压根没作用 / 遥控不了」。

次要可能：Mac 上跑的是**旧二进制**（Mac agent 本地编译修复未合入 `main`，或 `058835c` 之前的构建）。当前 `main` 的注入代码已正确，从最新 `main` 重新构建 Mac 即可。

## 3. Mac 侧修复步骤（agent 执行）

1. **授权（最关键）**：系统设置 → 隐私与安全性 →
   - 「辅助功能」勾选 MyKVM；
   - 「输入监控」勾选 MyKVM；
   - 两项都勾上后**完全退出并重启 MyKVM**（仅重开窗口不够，需进程退出再启动）。
2. **确保是最新构建**：`git pull` 到 `main`（`bc186c1` / `deb0fb7` 对齐），按 `scripts/build-mac-arm.sh` 重新构建 Mac（rust-lld 链接器防 dylib 损坏）。
3. **验证**：Mac 作为被控端，从 Windows 滚动 → Mac 应滚；同时测 Mac 本地滚轮（验证引擎 tap 也正常）。
4. 若仍失败，开日志看是否有
   `local scroll engine tap could not be created (accessibility permission?)`（来自 `run_tap_loop` ~4039）——有则授权没到位。

## 4. Windows 侧本次已交付

- 已 `git pull` 到最新 `deb0fb7`（v0.1.3）。
- 质量闸门全过：`cargo check` / `tsc -b` / `eslint .` 均无错误（仅预存 dead_code 警告）。
- 已出包并安装 **`mykvm_0.1.3_x64-setup.exe`**（`C:\Users\shadow_\AppData\Local\mykvm\mykvm.exe`），与 Mac v0.1.3 对齐。
- Windows 侧发送 notches 的协议未变，无需改动。

## 5. Mac agent 回填（2026-08-12）

- [x] **Mac 已从最新 `main`（v0.1.3 / `a5147c6`）重建**：`cargo test --lib` 117/117 通过（含 macOS-only 代码），`cargo build --release` 通过（rust-lld 链接，无 dylib 损坏）；已出 v0.1.3 自签名 `.app`/`.dmg` 并发布 GitHub Release v0.1.3（含 `latest.json` + `.sig`，更新私钥签名一致，app 端可校验通过）。「旧二进制」根因已排除。
- [ ] **授权（人工步骤，agent 无法代做）**：需在系统设置 → 隐私与安全性 →「辅助功能」「输入监控」勾选 MyKVM，并**完全退出重启 app**。完成后 Win→Mac 滚轮/点击/键盘应恢复（否则 `CGEvent::post` 静默失败）。请在真机验证后回报。
- [ ] **实机验证 Win→Mac 滚轮**：Mac 作为被控端，从 Windows 滚动 → 确认 Mac 滚动。若授权+最新构建后仍失败，抓 `log`/控制台输出回报（重点看 `local scroll engine tap could not be created (accessibility permission?)`），再决定是否远端注入改走 LINE 非连续路径。
