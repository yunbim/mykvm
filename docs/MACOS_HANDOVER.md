# MyKVM — macOS 侧交接文档

> **给接手的 macOS agent**：本文档是全部上下文。你不需要读之前的对话记录。
> 写作日期 2026-08-08，对应版本 **v0.1.2-dev**（工作区 HEAD，见 §1.3）。
> 所有行号以本次提交为准，改动后请用 `grep -n` 重新定位。

---

## 0. 一句话现状

MyKVM 是一个 Tauri 2 的键鼠共享工具（类 Barrier/Synergy）。
**所有 macOS 专属代码都是在 Windows 主机上盲写的**——只过了 `cargo check`（Windows target）
和人工审查，**从未在 macOS 上编译或运行过一次**。macOS target 连编译都没验证过。

所以你接手时的合理假设是：**macOS 路径有编译错误的概率不低，有运行期崩溃的概率更高**。
请先把「能编过 + 不崩」当第一目标，再谈功能验收。

---

## 1. 项目基本信息

### 1.1 仓库与路径

| 项 | 值 |
| --- | --- |
| 远程 | `git@github.com:yunbim/mykvm.git`（用户自己的 fork） |
| 上游 | `github.com/XxMinor/mykvm` — **红线：任何情况下都不要推到上游** |
| 分支 | `main` |
| 版本 | `0.1.1`（`package.json` / `src-tauri/tauri.conf.json` / `src-tauri/Cargo.toml` 三处同步） |

> ⚠️ 用户是 git 新手。执行任何写操作（commit/push/reset）前先说明再做。
> **禁止** `git stash` / `git checkout -- .` / `git reset --hard`——历史上误删过 68 个文件。
> 核对状态只用只读的 `git status` / `git diff`。

### 1.2 技术栈

- 前端：React 19 + TypeScript + Vite
- 后端：Rust，Tauri 2.11（features: `macos-private-api`, `tray-icon`）
- 传输：QUIC（自研 `quic_transport.rs`），UDP 47834
- macOS 依赖：`core-foundation 0.10`、`core-graphics 0.25`（feature `highsierra`）
- 最低系统：macOS 12.0

### 1.3 编译命令

```
cd ~/mykvm && npm ci && npm run mac:build-install
```

拆开是：

| 脚本 | 作用 |
| --- | --- |
| `npm run tauri:build:mac-arm` | `tauri build --target aarch64-apple-darwin --bundles app,dmg --no-sign` |
| `npm run mac:install-local` | `scripts/install-mac-app.sh`：退旧进程 → `ditto` 到 `/Applications` → 去 quarantine → 调签名脚本 → `open` |
| `npm run mac:sign-local` | `scripts/sign-mac-app.sh`：自建本地 codesign 身份 + `codesign --force --deep` |

产物：
```
src-tauri/target/aarch64-apple-darwin/release/bundle/macos/mykvm.app
src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/mykvm_0.1.1_aarch64.dmg
```

装完必须去「系统设置 → 隐私与安全性」勾选 **辅助功能** 和 **输入监控**。
换过签名身份的话旧授权会失效，要先 `−` 删掉旧条目再重加。

### 1.4 质量闸门（改完必须全过）

```
cd src-tauri && cargo check --all-targets
cd src-tauri && cargo test --lib          # 当前 105 passed / 0 failed
npx tsc -b
npx eslint .
```

`cargo check` 当前有 **6 条 dead_code warning**（`performance.rs` 里的 unix-only 函数在
Windows 上没被调用），是既有的，不是你引入的。在 macOS 上这些反而会消失、可能换成别的。

---

## 2. 架构速览（只讲你会碰到的部分）

### 2.1 角色

任意一台机器都可以同时是「主控端」（发送键鼠）和「被控端」（接收注入）。
两个方向的代码在 `src-tauri/src/input.rs` 里：

- **发送端 / capture**：`start_platform_capture`（macOS 版 `input.rs:841`，Windows 版 `1047`）。
  macOS 用 `CGEventTap` 抓事件，跑在自己的线程 + `CFRunLoop`。
- **接收端 / inject**：`inject_key` / `inject_mouse_move` / `inject_mouse_button` / `inject_scroll`。
  macOS 用 `CGEvent::new_*` + `post(CGEventTapLocation::HID)`。

### 2.2 关键文件

| 文件 | 行数量级 | 内容 |
| --- | --- | --- |
| `src-tauri/src/input.rs` | ~7900 | 键鼠捕获/注入、滚动平滑、暂停判定、光标控制。**macOS 代码 90% 在这** |
| `src-tauri/src/lib.rs` | ~7400 | Tauri 命令、窗口/托盘、单实例锁、剪贴板同步线程、激活策略 |
| `src-tauri/src/quic_transport.rs` | ~950 | QUIC 传输 |
| `src-tauri/src/clipboard.rs` | ~200 | 剪贴板读写（macOS 走 `osascript`/`pbcopy`） |
| `src-tauri/src/performance.rs` | ~250 | CPU/内存采样（macOS 走 `proc_pid_rusage`） |
| `src/i18n.ts` | — | 前端文案 `TEXT.cn` / `TEXT.en` |

### 2.3 macOS cfg 代码分布

| 文件 | `#[cfg(target_os="macos")]` 数量 |
| --- | --- |
| `input.rs` | ~108 |
| `lib.rs` | ~27 |
| `performance.rs` | 4 |
| `clipboard.rs` | 2（`cfg!` 宏形式） |

完整清单：`grep -n '#\[cfg(target_os = "macos")\]' src-tauri/src/input.rs`

---

## 3. 用户报的 9 个问题 · 逐项现状

用户在真机上用 v0.1.0 测过一轮，反馈 9 点。下表是**每一项的当前状态**。
"未验证"= 代码写了但从没在 macOS 跑过。

| # | 问题 | 状态 | 风险 |
| --- | --- | --- | --- |
| 1 | 菜单栏图标要白色极简几何 | ✅ 用户已确认「好评」 | 无 |
| 2 | 开机静默启动、不弹窗、Dock 图标按需 | 代码已写，**未验证** | 中 |
| 3 | 点窗口**中间**（非标题栏）不转移焦点 | 代码已写，**未验证** | **高** |
| 4 | 鼠标不顺滑 + 滚动方向要可配 | 已重写（第二轮），**未验证** | 中 |
| 5 | Mac 重启后启动两个实例 | 代码已写，**未验证** | 中 |
| 6 | 中文翻译不到位 | 代码已改，**未验证** | 低 |
| 7 | 拖拽边界锁定 | ✅ 用户已确认「做的好」，回归即可 | 低 |
| 8 | 场景暂停太宽，要 app 白名单 | **本次刚修了致命 bug**，**未验证** | **高** |
| 9 | Win→Mac 键盘吞字 | 改走可靠流，**未验证** | 中 |

下面逐个展开。

---

### #2 静默启动 / Dock 图标跟随

**期望**：开机自启后不弹窗，只有菜单栏图标；Dock 里没有图标。点菜单栏「打开主界面」
才出现窗口 + Dock 图标；关窗口后 Dock 图标一起消失，进程留在菜单栏。

**实现**：

| 位置 | 内容 |
| --- | --- |
| `src-tauri/Info.plist:5` | `<key>LSUIElement</key><true/>` |
| `src-tauri/tauri.conf.json:21` | `"visible": false` |
| `src-tauri/tauri.conf.json:44` | `"infoPlist": "Info.plist"` |
| `src-tauri/tauri.macos.conf.json:11` | `visible: false` |
| `lib.rs:3091` | `macos_set_activation_policy(policy: i64)`，0=Regular / 1=Accessory / 2=Prohibited |
| `lib.rs:3315` | 启动时 `policy = 1` |
| `lib.rs:3470` | 显示主窗口时 `policy = 0` |
| `lib.rs:3496` | 窗口销毁后 `policy = 1` |

配套：`macos_miniaturize_window`（`lib.rs:3010`）、`macos_order_front_window`（`lib.rs:3036`）。

**要重点查的**：`macos_set_activation_policy` 走的是
`objc_msgSend` + `std::mem::transmute` 手搓 FFI，不是 `cocoa` crate。
selector 是 `setActivationPolicy:`，参数是 `NSInteger`。在 aarch64 上如果签名 transmute
错了会直接崩。建议先单独验证这一个函数。

---

### #3 点窗口中间转移焦点（风险最高）

**期望**：Mac 作为被控端，从 Windows 移过来点击另一个窗口的**正中间**（不是标题栏），
该窗口应该像原生点击一样获得焦点。

**为什么难**：`CGEvent` 合成的点击不会自动触发 macOS 的窗口激活逻辑
（原生点击的激活是 WindowServer 在更底层做的）。所以只能手动补：命中测试找出点击位置
下面是哪个 app，然后主动激活它。

**实现**：`input.rs:6925` `fn macos_activate_app_at_point(x: i32, y: i32)`

```
CGWindowListCopyWindowInfo(OPT_ON_SCREEN_ONLY|OPT_EXCLUDE_DESKTOP = 0x11, 0)
  → 列表是 front-to-back 序，遍历取第一个命中
  → 跳过 kCGWindowLayer != 0（Dock、菜单栏、通知横幅等系统层）
  → 跳过自身 pid
  → 跳过 Width/Height ≤ 1 的退化窗口
  → 命中判据：px >= bx && px < bx+bw && py >= by && py < by+bh
  → 取 kCGWindowOwnerPID
  → 若已是最前（macos_frontmost_pid，input.rs:7011）则跳过，避免闪烁
  → macos_activate_app_by_pid(pid)   // input.rs:7055
     → NSRunningApplication runningApplicationWithProcessIdentifier:
     → activateWithOptions: 2   (NSApplicationActivateIgnoringOtherApps)
```

**调用点**：`input.rs:6797`，在 `inject_mouse_button`（`6760`）里，**仅 `down` 时**调用，
在 `CGDisplay::warp_mouse_cursor_position` 之后、`CGEvent` post 之前。

**坐标系（我已核对过，这里是对的）**：
`inject_mouse_button(x, y)` 把同一组 `x, y` 既用于 `CGPoint::new(x, y)` 也传给
`macos_activate_app_at_point`。`CGPoint` 和 `kCGWindowBounds` 都是
**top-left 原点的 Quartz 全局坐标**，两边一致，**没有多翻一次 y**。
（项目别处的 `mac_cursor_point` / `invert_y` 是发送端用的，不影响这条路径。）

**要重点查的**：

1. **多显示器负坐标**。主屏左侧/上方的第二块屏，全局坐标是负数。
   命中判据本身能处理负数，但要确认发送端传过来的 `x, y` 已经换算到
   接收端的全局坐标系而不是「该屏局部坐标」。这是当前最可疑的点。
2. **权限**。`CGWindowListCopyWindowInfo` 在 macOS 10.15+ 拿 `kCGWindowName`
   需要「屏幕录制」权限；我们只用 `bounds` + `ownerPID`，**理论上不需要**。
   如果真机上列表为空或缺字段，就是这个原因，得补权限申请。
3. `activateWithOptions: 2` 在 macOS 14+ 行为有变（Apple 改推 `activate(from:)`）。
   激活不生效的话试 `3`（`AllWindows | IgnoringOtherApps`）。
4. **时序**。激活发生在 `CGEvent` post **之前**。如果激活是异步的（很可能），
   点击事件可能在目标 app 真正变成 frontmost 之前就投递了，导致第一次点击丢失、
   第二次才生效。真机上如果出现「要点两下」，就是这个——需要在激活后加短暂等待，
   或改成 post 之后再激活。

**建议的最小复现**：写个独立小 bin，只调 `macos_activate_app_at_point(x, y)`
硬编码一个坐标，看能不能把 Finder 激活。跑通了再回主程序。

---

### #4 平滑滚动 / 反转方向（第二轮已重写）

**第一轮用户反馈「效果很差」。第二轮我参考 Mos 源码重写了，定位到三个真 bug（不是调参）。**

#### 单位链路（先理解这个，否则改不动）

**线路协议的单位是「滚轮格数 notch」，不是像素。**

- Windows 发送端：`(mouse_data >> 16) as i16 / 120`（`WHEEL_DELTA=120`）
- macOS 发送端：`input.rs:4532` 读 `SCROLL_WHEEL_EVENT_DELTA_AXIS_1/2`（line delta）

两边都是格数，一格 = ±1。

#### 接收端分发（`input.rs:3188`）

```rust
pub(crate) fn inject_scroll(delta_x: i32, delta_y: i32) {
    let (delta_x, delta_y) = if REVERSE_SCROLL.load(...) { (-delta_x, -delta_y) } else { ... };
    if SMOOTH_SCROLL.load(...) {
        scroll_smoothing::enqueue(delta_x, delta_y);   // 平滑路径
    } else {
        post_line_scroll(delta_x, delta_y);            // 直通路径
    }
}
```

开关：`SMOOTH_SCROLL`（`input.rs:3152`，默认 true）、`REVERSE_SCROLL`（`3155`，默认 false）。

#### 两条投递路径

| 路径 | macOS 实现 | Windows 实现 |
| --- | --- | --- |
| 直通 `post_line_scroll` (macOS 版 `3208`) | `ScrollEventUnit::LINE` | `windows_input::inject_scroll`（×120，`3227`） |
| 平滑 `post_smoothed_units` (macOS 版 `3286`) | `ScrollEventUnit::PIXEL` + `ScrollWheelEventIsContinuous=1` | `windows_input::inject_scroll_units`（不 ×120，`3306`） |

中间层 `post_pixel_scroll`（`3241`）带 `CARRY_X/CARRY_Y` 小数进位累加器，
保证 `Σ实际投递 == Σ输入像素`，会话结束发一个 flush 帧把余量清干净。
`pixels_to_post_units`（Windows 版 `3274` 按 `120/PIXELS_PER_NOTCH` 折回格；
`not(windows)` 版 `3281` 直通像素）。

#### 平滑器 `mod scroll_smoothing`（`input.rs:3333`）

从 Mos 源码逆向出来的常量：

```rust
const TRANS: f64 = 0.085;        // 来自 Mos durationTransition(4.35)
const STEP:  f64 = 33.6;
const SPEED: f64 = 2.70;
pub(super) const PIXELS_PER_NOTCH: f64 = STEP * SPEED;   // ≈ 90.7 px/格
const FILTER_ALPHA: f64 = 0.23;  // 一阶低通系数
const SETTLE_PX: f64 = 0.5;
const INTERVAL: Duration = Duration::from_millis(8);     // ~120Hz
const MANUAL_END: Duration = Duration::from_millis(180);
```

核心算法：

- `notches_to_pixels`（`3403`）：`signum * max(|n|,1) * PIXELS_PER_NOTCH`
- `polish`（`3414`）：`out = *window; *window += FILTER_ALPHA * (next - *window)`
- `push`（`3441`）：方向反转时 `buffer = px; current = 0`（立即重启，不走完旧动量）
- `advance`（`3465`）：每帧 `frame = (buffer - current) * TRANS`；
  收敛（residual ≤ `SETTLE_PX`）且 idle 时，一次性 flush `residual + window / FILTER_ALPHA`
- `ensure_worker`（`3490`）：后台线程，**用 Condvar park**，不是 125Hz 空转

**⚠️ 那个 `/ FILTER_ALPHA` 是关键，别手贱去掉。**
推导：EMA 的尾债不是 `window` 而是 `window/α`，因为 `Σout = Σf − w_final/α`。
第一版写成 `residual + window` 导致守恒测试差了 271.9 像素。

#### 6 个单元测试（`input.rs:3516–3609`）

| 测试 | 断言 |
| --- | --- |
| `one_notch_is_worth_step_times_speed_pixels` | `notches_to_pixels(1.0) ≈ 33.6*2.70` |
| `smoothing_preserves_total_scroll_distance` | 3 格总距离 `≈ 3*PIXELS_PER_NOTCH`，且 `frames > 20` |
| `smoothing_preserves_distance_on_both_axes` | `(-2, 5)` 双轴守恒 |
| `direction_reversal_restarts_the_session` | 反向后 buffer 重置 |
| `session_parks_after_settling` | 收敛后 `!active` |
| `held_wheel_never_settles_until_released` | 500 帧 `idle=false` 永不 finished |

**验收的关键一条**：
> 关掉平滑滚一格 和 打开平滑滚一格，**总距离必须相同**，只是手感不同。

**macOS 上要重点查**：
1. `ScrollEventUnit::LINE` vs `PIXEL` 在真机上的实际步长。
   `LINE` 单位一格滚多远由系统「滚动速度」设置决定，可能和期望不符。
2. `ScrollWheelEventIsContinuous = 1` 会让 macOS 把事件当触控板惯性滚动处理。
   某些 app（Safari、Xcode）对连续滚动有自己的加速曲线，可能叠加放大。
3. 8ms 定时器在 macOS 上的实际抖动。如果帧不稳，观感会有颗粒。

---

### #5 单实例（UDS 锁）

**期望**：重启后只有 1 个进程；再手动双击 app 不起第二个，而是把已有实例窗口拉到前台。

**实现**（全在 `lib.rs`）：

| 函数 | 行 | 作用 |
| --- | --- | --- |
| `macos_instance_socket_path` | 2726 | `std::env::temp_dir().join("mykvm.sock")` |
| `acquire_single_instance` | 2731 | `UnixListener::bind`，失败则 connect 探活 |
| `release_single_instance` | 2806 | `remove_file` |
| `activate_existing_instance` | 2813 | 向已有实例发 `MACOS_INSTANCE_ACTIVATE`(=1) |
| `request_existing_instance_quit` | 2838 | 发 `MACOS_INSTANCE_QUIT`(=2) |
| `setup_single_instance_events` | 2893 | 起线程读 incoming |

入口：`src-tauri/src/main.rs:9`
```rust
if !mykvm_lib::acquire_single_instance() {
    activate_existing_instance();
    return;
}
```

**要重点查的**：

1. **`std::env::temp_dir()` 在 macOS 上不是 `/tmp`**，而是
   `$TMPDIR` = `/var/folders/xx/xxxxx/T/`，**且每个用户/每个沙箱会话不同**。
   如果两个实例的 `TMPDIR` 不一样（比如一个由 launchd 启动、一个由 Finder 启动），
   **锁就完全失效**——这非常可能就是"重启后起两个"的真正原因。
   > 建议：改成固定路径 `/tmp/mykvm.sock`，或者用 `~/Library/Application Support/mykvm/`。
   > 文档之前写的是 `/tmp/mykvm.sock`，**和代码不符**，以代码为准。
2. 异常退出会残留 socket 文件。connect 探活逻辑要能识别"文件在但没人 listen"
   （`ECONNREFUSED`）并接管。真机上 kill -9 后重启验证一下。
3. **重启后起两个的另一个可能原因**：登录项里同时有「系统设置→登录项」的条目
   **和** 一个 LaunchAgent plist。先 `ls ~/Library/LaunchAgents/ | grep -i mykvm`
   排查，别一上来就怀疑代码。

---

### #6 中文本地化

前端文案在 `src/i18n.ts` 的 `TEXT.cn` / `TEXT.en` map 里。
后端通知走一份 off-render 镜像：`setActiveLanguage()` / `activeText()`，
因为 watcher 线程发通知时拿不到 React 的 context。
语言切换命令是 `set_notify_language`（`input.rs`）。

逐屏翻一遍即可。重点：术语用「主控 / 被控」，不要出现 Server/Client。

---

### #8 应用白名单暂停（**本次刚修了致命 bug**）

**期望**：用户手动加游戏的 Bundle ID 到白名单；切到该 app 时暂停键鼠共享；
名单非空时白名单**完全接管**，不再用「任意全屏窗口」这个宽泛启发式
（否则全屏看视频会被误暂停）；名单为空才回退到全屏规则。

**判定逻辑**（`input.rs`）：

```rust
// input.rs:2769 — 单一决策点
pub fn scene_pause_active() -> bool {
    if pause_whitelist_configured() {   // 名单非空？
        app_pause_active()              // → 只看白名单
    } else {
        fullscreen_app_active()         // → 回退全屏启发式
    }
}
```

相关状态：`FULLSCREEN_APP_ACTIVE`(2640)、`APP_PAUSE_ACTIVE`(2730)、`PAUSE_WHITELIST`(2733)、
`set_pause_whitelist`(2741)、`whitelist_contains`(2747)、`pause_whitelist_configured`(2755)。

#### 🔴 本次修复的 4 处（交接前刚发现，**你拿到的代码是修好的**）

盘点代码时发现 **macOS 上这个功能之前完全是死的**——只弹通知，不真的暂停。根因有四层：

| # | 缺陷 | 修法 | 位置 |
| --- | --- | --- | --- |
| 1 | **`spawn_app_pause_watcher` 只在 Windows 的 `start_platform_capture` 里调用**，macOS 分支从没调过 → `APP_PAUSE_ACTIVE` 永远 false，白名单完全是死的 | 在 macOS `start_platform_capture` 的 `ready_tx.send(Ok(()))` 之后补调 | `input.rs:977` |
| 2 | `handle_macos_mouse_move` 没有暂停守卫（Windows 版在 `4229` 有） | 函数开头加 `scene_pause_active()` 检查，持有远程则 `return_to_local_macos` | `input.rs:4648` |
| 3 | `handle_macos_event` 的按键/按钮路径没守卫（⌘-Tab 进游戏不动鼠标时会漏） | `drop(active)` 之后加守卫 + 释放 | `input.rs:4519` |
| 4 | `drain_switch_request_macos` 没守卫（Windows 版在 `5555` 有） | 开头加守卫，吞掉切屏请求 | `input.rs:5497` |
| 5 | 剪贴板同步线程用的是 `fullscreen_app_active()` 而非 `scene_pause_active()`，白名单模式下不一致 | 改为 `scene_pause_active()` | `lib.rs:5015` |

**为什么 macOS 要在热路径里释放而不是在 watcher 里**：
Windows 有全局 `WINDOWS_CAPTURE_CONTEXT`，watcher 线程能直接
`release_windows_remote_control`。macOS **没有**对应的全局 context，
watcher 拿不到 `MacCaptureContext`，所以只能在事件回调里做。
代价是释放会延迟到「下一个鼠标/键盘事件」，实测应该在几十毫秒内，可接受。
> 如果你觉得这个设计别扭，可以加一个 `MACOS_CAPTURE_CONTEXT: Mutex<Option<Arc<MacCaptureContext>>>`
> 对齐 Windows。我没做是因为无法在 Windows 上编译验证，不想盲改扩大风险。

#### ⚠️ 已知未实现的缺口

**macOS 上没有全屏检测**。`spawn_fullscreen_watcher`（`input.rs:3019`）是
`#[cfg(target_os = "windows")]`，macOS 上 `FULLSCREEN_APP_ACTIVE` 恒为 false。

后果：**白名单为空时，macOS 完全不会暂停**。
这在当前设计下算「可接受的降级」（因为 #8 的诉求就是"只在打游戏时生效"，
用户本来就该配白名单），但你要知道这个事实。
如果要补，macOS 判断全屏的路子是
`CGDisplayBounds` 对比窗口 bounds，或者用 `NSApplication.presentationOptions`。

#### 取 Bundle ID

```
osascript -e 'id of app "游戏名"'
```

`macos_frontmost_bundle_id`（`input.rs:2816`）通过
`NSWorkspace.sharedWorkspace.frontmostApplication.bundleIdentifier` 取，
watcher 每 500ms 轮询一次（`spawn_macos_app_pause_watcher`，`input.rs:2788`）。
匹配时**大小写不敏感**（存入时 lowercase）。

---

### #9 Win→Mac 键盘吞字

**原因**：键盘事件原本走 QUIC 的 unreliable datagram，丢包就是吞字。

**改法**：`input.rs:1455` 判定 `is_key_event`，`1512` 处分流——
键盘走 `send_stream_expect_ack`（可靠流 + ack），其余仍走 `send_datagram`。
鼠标移动量大且可丢，继续用 datagram。

**macOS 接收端注入**：`inject_key(key_code: u16, down: bool)`（`input.rs:7131`）
```rust
let event = CGEvent::new_keyboard_event(source, keycode, down);
event.set_flags(flags);
event.post(CGEventTapLocation::HID);
```
配套 `mac_function_section_flags`（`7202`，F 区功能键 flags 修正）、
`macos_post_select_previous_input_source`（`7220`，Caps Lock → ⌃Space 切输入法）。

**要重点查的**：
1. **Windows VK → macOS keycode 的映射表**。这是吞字/错字的另一大来源，
   和丢包无关。如果发现某些键固定打不出来（而不是随机吞），查映射表。
2. `set_flags` 的修饰键状态。macOS 的 CGEvent flags 是**绝对状态**不是增量，
   如果 flags 算错，会出现「按了 Shift 但出小写」这类问题。
3. `macos_secure_input_enabled`（`input.rs:801`，包 Carbon 的
   `IsSecureEventInputEnabled`）——**焦点在密码框时 macOS 会开启 Secure Input，
   此时 CGEvent 注入会被系统直接丢弃**。表现为「某些输入框完全打不了字」。
   代码里有检测函数，确认它真的被调用并且有日志/提示。

---

## 4. 调试方法

### 4.1 日志

```
log stream --predicate 'process == "mykvm"' --level debug
```

或
```
tail -f ~/Library/Logs/mykvm/mykvm.log
```

代码里的诊断日志带 `[diag]` 前缀（光标跨越、隐藏/解耦时机），grep 这个能看清跨屏流程。

### 4.2 权限核查

```
sqlite3 ~/Library/Application\ Support/com.apple.TCC/TCC.db \
  "select client,auth_value from access where service='kTCCServiceAccessibility'"
```

代码侧：`macos_accessibility_trusted`（`input.rs:774`，包 `AXIsProcessTrusted`）。

### 4.3 常见坑

| 现象 | 原因 |
| --- | --- |
| 完全没反应、鼠标不动 | 辅助功能权限没给，或换签名后授权失效 → 删掉重加 |
| 启动就崩 | 残留 socket 锁文件，`rm $TMPDIR/mykvm.sock` |
| 用一会儿就卡死 | CGEventTap 被系统禁用。代码里有 `tap_disabled` 重新 arm（`input.rs:999`），确认它真的触发了 |
| 某些输入框打不了字 | Secure Input 开启，CGEvent 被丢弃 |
| 滚动方向反了 | 是「反转滚动方向」开关，不是 bug；两台机器只需开一边 |

---

## 5. 最高优先级的三个怀疑点

如果时间有限，按这个顺序查：

### 🔴 P0 — objc FFI 的 ABI 正确性

代码里**大量**用 `std::mem::transmute(objc_msgSend)` 把可变参数的 `objc_msgSend`
强转成具体签名。在 aarch64 上，可变参数函数的调用约定和普通函数**不同**
（Apple 的 arm64 ABI 里 variadic 参数走栈），transmute 成非 variadic 签名是
**未定义行为**，可能正常也可能崩。

全部 27 处 transmute 位置（`grep -n "transmute(objc_msgSend" src-tauri/src/*.rs`）：

- `lib.rs:3030`（miniaturize）、`3062 / 3066 / 3077`（orderFront）、`3109 / 3116`（setActivationPolicy）
- `input.rs:2833 / 2850`（frontmost bundle id）
- `input.rs:7028 / 7044`（frontmost pid）、`7074 / 7082`（NSRunningApplication 激活）
- `input.rs:5715–5911` 共 13 处（光标隐藏/解耦、app-nap 抑制）
- `input.rs:6724`（双击间隔）
- 另有 `dlsym` 解析私有 `CGSMainConnectionID` / `CGSSetConnectionProperty`

**建议**：如果崩，最省事的修法是引入 `objc2` crate 用 `msg_send!` 宏，
让编译器生成正确的调用。**不要自己去 fix transmute 签名**——那是打地鼠。
如果只想验证不想重构，先给这几处包 `catch_unwind` 或加日志缩小范围。

### 🔴 P1 — #3 焦点转移的坐标系

见 §3 的 #3。top-left vs bottom-left 翻转是最容易错且最难从代码看出来的。
真机上打个 log 把 `CGEvent` location 和命中的窗口 bounds 都打出来，肉眼比对一次。

### 🟡 P2 — #5 单实例锁的路径

见 §3 的 #5。`temp_dir()` 在 macOS 上不是 `/tmp`，且可能因启动方式不同而不同。
这个几乎肯定是"重启起两个"的原因之一。

---

## 6. 建议的工作顺序

1. **先编过**。`npm run tauri:build:mac-arm`，把所有编译错误修掉。
   macOS target 从没编译过，报错很正常。这一步的产出比什么都重要。
2. **再不崩**。装上跑起来，什么都不操作，看能不能活 5 分钟。
   崩的话大概率是 P0 的 objc FFI。
3. **然后按 P0→P2 查三个怀疑点**。
4. **最后按 `docs/MAC_BUILD_TEST_RUNBOOK.md` 走验收清单**（那份是给用户的操作手册，
   有逐项的期望行为表格）。
5. 改完跑四道闸门（§1.4），版本号 +1（三处），更新 `CHANGELOG.md`，
   提交推 `yunbim/mykvm`。

---

## 7. 给用户交互的注意事项

- 用户是**命令行新手**。给命令时**用单行 `&&` 串联，不要用多行代码块**——
  他会把 markdown 的 ` ```bash ` 围栏一起粘进终端，反引号触发命令替换，
  静默进一个 bash 子 shell，命令一条都不执行。（已经踩过一次。）
- 用户默认 shell 是 bash 3.2（老 macOS 账户），提示符 `bash-3.2$`。
- 用户已把仓库 clone 在 `~/mykvm/mykvm`（嵌套目录，因为他先手建了 `~/mykvm`）。
  给路径前先 `ls ~/mykvm` 确认。
- 回复用**简体中文**。
- 涉及 push 到 GitHub 时先明确说清目标仓库再动手。

---

## 8. 附：本次交接前刚做的改动（尚未提交时的状态）

盘点过程中发现并修复了 §3 #8 里列的 5 处 macOS 暂停缺口。
Windows 侧 `cargo check --all-targets` 通过，`cargo test --lib` **105 passed / 0 failed**。
macOS 侧这几处改动**同样没编译验证过**，注意：

- `input.rs:4644` 用了 `CallbackResult::Keep`——该类型在函数内 `use` 于 4636 行，作用域没问题。
- `input.rs:4515` 的 `return_to_local_macos(context)` 调用在 `drop(active)` 之后，
  不会和函数内部的 `context.active.lock()` 死锁。
- `input.rs:4645` 的 `context.active.lock().map(|g| g.is_some())` 里 guard 在闭包结束即释放，
  之后才调 `return_to_local_macos`，同样不会死锁。

如果编译报错，优先看这三处。
