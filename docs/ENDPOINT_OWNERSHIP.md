# 功能端归属与跨端同步整改（Task #5）

本文档针对 MyKVM v0.1.2 的全部配置项，依据"真实跨屏操作场景"重新审查
**每项功能应在服务端还是客户端展示、生效、处理，以及是否需要跨端同步**，
并给出整改结果。配套代码改动已随本文落地（见末尾"已落地整改"）。

---

## 1. 判定原则

- **两个角色**：`machine_role` 只有两种——
  - `server`（控制端 / 服务端）：持有布局、发起配对、运行"边缘穿越"判定；
    你正用它的键盘鼠标在操作，是跨屏切换的**决策方**。
  - `client`（被控端 / 客户端）：被动接收输入，是切换的**目标方**。
- **流水线两段式**：输入在 *控制端捕获 → QUIC 传输 → 被控端注入*。
  判定类逻辑（边缘穿越、暂停、热键）**只在控制端运行**；
  注入类效果（滚动引擎、光标定位）**只在被控端生效**。
- **关于"跨端同步"的真相**：本仓库的 `LayoutState` 只持久化在各机本地的
  `layout.json`，运行时 `sync_layout_peer_presence` **仅同步设备在线状态/端口**，
  **不会把整份配置广播给对端**。因此"是否同步"的真实含义是：
  该设置应是*集群统一策略*（需在每台机器保持一致 / 由服务端权威），
  还是*每台机器独立*的本地设置。

---

## 2. 逐项归属与同步策略

分类图例：**S=服务端(控制端)**、**C=客户端(被控端)**、**B=集群统一**、**L=本地每台独立**

| 配置项（i18n 标签） | 展示端 | 生效/处理端 | 同步策略 | 当前状态 | 整改 |
|---|---|---|---|---|---|
| `machine_role` | L | L | 绝不跨端（每台自知） | ✅ 正确 | 保持 |
| `input_mode` (control/receive) | L | L | 随角色，本地 | ✅ 正确 | 保持 |
| `cluster_id` / `pair_secret` / `paired_controllers` | B | B | 集群身份，靠配对流程传播（非广播） | ✅ 正确 | 保持 |
| `devices` / `active_device_id` / `selected_screen_id` | B | B | 集群拓扑，服务端编辑 | ✅ 正确 | 保持 |
| `clipboard_sync` | B | B | 集群功能开关，需一致 | ✅ 正确 | 保持 |
| `file_transfer_enabled` | B | B | 集群功能开关，需一致 | ✅ 正确 | 保持 |
| `modifier_remap` / `modifier_map` | B | S 处理（按目标机 OS 解释） | 跨 OS 映射策略，集群一致 | ✅ 正确 | 保持 |
| `edge_switch_hotkey`（快捷启停） | **S** | **S** | 控制端全局热键 | ⚠️ 后端已 gate（非 server 返回空），UI 仍全展示 | 建议 UI 按 server 隐藏 |
| `screen_switch_hotkeys`（快捷切屏） | **S** | **S** | 控制端全局热键 | ⚠️ 同上 | 建议 UI 按 server 隐藏 |
| `drag_edge_guard` / `drag_crossing_hold_ms`（拖拽时锁定边界） | **S** | **S** | 边缘穿越保护，控制端判定 | ⚠️ UI 全展示 | 建议 UI 按 server 隐藏 |
| `fullscreen_pause`（全屏应用自动暂停） | **S** | **S** | 控制端前台全屏时暂停穿越 | ✅ **已整改** | UI 仅 server 显示；后端 apply 仅 server 生效 |
| `pause_app_whitelist`（指定应用时暂停） | **S** | **S** | 控制端前台应用名单 | ✅ **已整改** | UI 仅 server 显示；后端 apply 仅 server 生效 |
| `mouse_smoothing`（鼠标平滑） | **S** | **S** | 移动增量低通滤波，捕获/发送端处理 | ⚠️ UI 全展示 | 建议 UI 按 server 隐藏 |
| `smooth_scroll` / `reverse_scroll`（平滑滚动 / 反转方向，Windows） | **S** | **S** | 控制端滚动平滑，macOS 已用 `macos_scroll` 替代 | ⚠️ UI 已按 `!isMacos` 隐藏（client 仍可见） | 建议再加 server 角色判断 |
| `macos_scroll`（Mac 平滑滚动引擎） | **C (仅 macOS)** | **C (仅 macOS)** | 被控 Mac 专属，不跨端同步 | ✅ **已整改(Task #2)** | UI 仅 macOS 显示；后端 `set_macos_scroll_settings` 为 `cfg(macos)` 空实现；不参与任何集群策略 |
| `transport_port_mode` / `transport_port` / `quic_port` | L | L | 每台自身网络端口，绝不跨端（对端端口经发现协议独立获取） | ⚠️ 在共享 `LayoutState` 中 | 建议明确为本地；不影响功能因配置不广播 |
| `language` / `theme_mode` / `performance_monitor` | L | L | 纯 UI 偏好，本地 | ⚠️ 在共享 `LayoutState` 中 | 建议明确为本地 |

---

## 3. 真实场景下的归属结论

- **"指定应用时暂停 / 全屏应用暂停"只能是服务端**：
  暂停判定依据的是*你正在操作的这台机器*（控制端）的前台应用。
  被控端（client）在被控时根本不做边缘穿越决策，若把控制端的暂停名单/全屏状态
  套到被控端，会出现"一台机器的前台应用挡住另一台机器切换"的错误行为。
  → 已落地：UI 仅在 `machineRole === "server"` 展示；后端 `apply_*` 仅在
  `machine_role == "server"` 时把值写入输入层原子量，client 强制为 false / 空。

- **`macos_scroll` 只能属于被控 Mac**：
  滚动是被控端的现象，Mac 被 Win/Linux 遥控时由 *本机* 的 Session 级 tap 引擎
  完成平滑/反转/Option 加速等。它既不属于控制端决策，也绝不该跨端同步——
  否则双套平滑会叠加导致"滚动过快 / 反转失效"。
  → 已落地（`Task #2`）：UI 仅 macOS 显示并标注"可完全替代第三方工具、不跨端同步"；
  后端 `cfg(macos)` 实现，非 Mac 为空操作。

- **其余"判定类"开关（热键、拖拽锁边、鼠标平滑、Win 滚动平滑/反转）均属服务端**：
  它们只在控制端的捕获/决策路径被消费，被控端既不展示也不应处理。
  UI 当前把它们也展示给了 client，属展示越界（功能上因 client 不运行对应路径而 harmless，
  但会让用户误以为在被控端配置生效）。建议后续按 `server` 角色隐藏（见上表）。

---

## 4. 已落地整改（本轮代码）

1. `src-tauri/src/lib.rs` `apply_scene_guard_settings` / `apply_input_smoothing_settings`：
   仅当 `layout.machine_role == "server"` 时把 `fullscreen_pause` 与
   `pause_app_whitelist` 写入输入层；client 强制 false / 空。
2. `src/App.tsx`：将"全屏应用自动暂停"与"指定应用时暂停"两块 UI 用
   `{machineRole === "server" && (...)}` 包裹，client 不可见。

> 说明：因配置不跨端广播，server-only 字段即便残留在 client 的 `layout.json`
> 中也不会自动影响对端；本轮整改确保的是**语义正确**（不在 client 展示/处理），
> 与"不跨端同步"目标一致。

---

## 5. 遗留待办（建议，未改动以免扩大范围）

- **UI 角色门控补全**：`edge_switch_hotkey`、`screen_switch_hotkeys`、
  `drag_edge_guard`/`drag_crossing_hold_ms`、`mouse_smoothing`、
  `smooth_scroll`/`reverse_scroll` 在 client 端 UI 也应隐藏（后端已正确 gate）。
- **macOS 暂停检测未接线**：`spawn_macos_app_pause_watcher` /
  `spawn_app_pause_watcher` / `macos_frontmost_bundle_id` 当前**从未被调用**
  （dead code）。仅 Windows 的 `spawn_fullscreen_watcher` 实际消费
  `FULLSCREEN_PAUSE_ENABLED`。即"全屏暂停"在 macOS 控制端目前**无前台检测**，
  需接入 mac 前台 app 轮询后该功能方在 Mac 控制端真正生效。
- **本地字段语义化**：`transport_port*`、`language`、`theme_mode`、
  `performance_monitor` 建议从"集群共享"语义中明确为"每台独立"。
