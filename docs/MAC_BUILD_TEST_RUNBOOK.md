# macOS 编译 / 安装 / 验收手册

> 适用于 fork `yunbim/mykvm`，v0.1.1 起。
> Windows 侧无法交叉编译 `aarch64-apple-darwin`，所有 macOS 专属代码路径（Dock 策略、
> 焦点转移、单实例、Mos 滚动）**必须在真机上跑一遍本手册**才算验证通过。

---

## 0. 一次性环境准备

```bash
# Xcode 命令行工具
xcode-select --install

# Rust（已装则跳过）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup target add aarch64-apple-darwin

# Node 20+
node -v
```

> ⚠️ **复制粘贴注意**：本文档的命令写在 ``` 代码块里，**只复制块内的命令行**，
> 不要把开头的 ```` ```bash ```` 和结尾的 ```` ``` ```` 一起粘进终端。反引号在 shell 里是
> 「命令替换」，整块粘进去会启动一个 `bash` 子 shell（提示符变成 `bash-3.2$`），
> 命令根本没执行。已经中招的话按 `Ctrl-D` 或输入 `exit` 退回 zsh 即可。

## 1. 拉取代码

首次 clone（**在家目录执行，不要先 `mkdir ~/mykvm` 再进去 clone**，否则会得到嵌套的
`~/mykvm/mykvm`）：

```bash
cd ~
git clone git@github.com:yunbim/mykvm.git
cd ~/mykvm
```

已经 clone 过就直接更新：

```bash
cd ~/mykvm
git fetch origin && git checkout main && git pull --ff-only
```

确认拿到的是最新版（应输出 `0.1.1` 和对应 commit）：

```bash
git log --oneline -1
node -p "require('./package.json').version"
```

## 2. 编译 + 安装（一条命令）

先确认工具链就位：`node -v` ≥ 20，`rustup target list --installed | grep darwin`
应含 `aarch64-apple-darwin`。

```bash
npm ci
npm run mac:build-install
```

这条命令等价于 `tauri build --target aarch64-apple-darwin --bundles app,dmg --no-sign`
后接 `scripts/install-mac-app.sh`，产物在：

```
src-tauri/target/aarch64-apple-darwin/release/bundle/macos/mykvm.app
src-tauri/target/aarch64-apple-darwin/release/bundle/dmg/mykvm_0.1.1_aarch64.dmg
```

### 权限（每次替换 .app 后都要确认）

系统设置 → 隐私与安全性：

- **辅助功能**：勾选 mykvm（键鼠注入、CGEventTap 都依赖它）
- **输入监控**：勾选 mykvm

如果换了签名身份，macOS 会把旧授权作废，需要先在列表里 `−` 删掉旧条目再重新添加。
本地签名可用 `npm run mac:sign-local`，能让授权跨版本保留。

---

## 3. 验收清单

按顺序执行。每项都标注了对应的问题编号，方便回报。

### #2 静默启动 / Dock 图标

| 步骤 | 期望 |
| --- | --- |
| 「登录项」里加入 mykvm，重启 Mac | 登录后**不弹窗**，只有菜单栏出现白色几何图标 |
| 此时看 Dock | **没有** mykvm 图标 |
| 点菜单栏图标 →「打开主界面」 | 窗口出现，同时 Dock 图标出现 |
| 关闭窗口（红灯 / ⌘W） | 窗口消失，**Dock 图标一并消失**，进程仍在菜单栏活着 |

> 实现：`Info.plist` 的 `LSUIElement=true` + 运行时 `setActivationPolicy:`
> （0=Regular 显示 Dock / 1=Accessory 隐藏）。

### #5 单实例

| 步骤 | 期望 |
| --- | --- |
| 重启 Mac，登录后运行 `pgrep -al mykvm` | **只有 1 个**进程 |
| 再手动双击 `/Applications/mykvm.app` | 不会起第二个进程；已有实例把窗口带到前台 |
| 退出后运行 `ls -l /tmp/mykvm.sock` | 文件已被清理（不存在） |

> 实现：`/tmp/mykvm.sock` Unix domain socket 抢占锁（先 bind，失败再 connect 探活）。

### #3 焦点转移（点窗口"中间"）

前置：Mac 作为**被控端**，Windows 作为主控端。

| 步骤 | 期望 |
| --- | --- |
| Mac 上开两个窗口（如 Finder + 浏览器），让 Finder 在前 | — |
| 从 Windows 把鼠标移到 Mac，点击**浏览器窗口正中间**（不是标题栏） | 浏览器立刻获得焦点，标题栏变亮，键盘输入进浏览器 |
| 点击 Dock、菜单栏、通知中心 | **不应**被误判成窗口点击 |
| 点击当前已是最前的窗口 | 不重复激活（不闪烁） |

> 实现：`CGWindowListCopyWindowInfo` 按前后顺序命中测试，跳过 `kCGWindowLayer != 0`
> 的系统层与自身 pid，命中后 `NSRunningApplication.activateWithOptions:2`。

### #4 平滑滚动 / 反转方向

Mac 既要测**作为被控端**（接收 Windows 滚轮），也要测反向。

| 步骤 | 期望 |
| --- | --- |
| 设置里关掉「Mos 风格滚动」，在 Mac 上滚一格 | 滚动距离 ≈ 本机鼠标滚一格 |
| 打开「Mos 风格滚动」，滚一格 | **总距离和上一步一致**，但是渐进动画、有缓出，不跳格 |
| 连续快速滚 5 格 | 累加成一段连续滚动，不抖、不回弹 |
| 滚到底后反向滚 | 立刻反向，没有"先把旧动量走完"的迟滞 |
| 打开「反转滚动方向」 | 内容跟手指走（macOS 自然滚动方向） |
| 停手后 1 秒观察 `Activity Monitor` 里 mykvm 的 CPU | 回到 ~0%（插值线程已 park，不再 125Hz 空转） |

> 关键回归点：**开/关平滑滚动，一格的总距离必须相同**。之前的实现会缩水，
> 而且在 Windows 接收端会放大 33 倍。

### #8 应用白名单暂停

| 步骤 | 期望 |
| --- | --- |
| 设置 → 「指定应用时暂停」里添加游戏的 Bundle ID | 列表出现该条目 |
| 查 Bundle ID：`osascript -e 'id of app "游戏名"'` | — |
| 切到该游戏 | 通知「已暂停」；边界切换、切屏快捷键、剪贴板同步全部停用 |
| 切回其他 app | 通知「已恢复」 |
| **名单非空时**，把任意 app 全屏（如全屏视频） | **不再暂停**（全屏规则已让位给白名单） |
| 清空名单后再全屏 | 回退到旧的全屏规则，会暂停 |

### #9 键盘可靠性（Win → Mac）

| 步骤 | 期望 |
| --- | --- |
| 从 Windows 控制 Mac，在文本框里快速盲打一段长中文/英文 | **不吞字**、不乱序 |
| 按住方向键 3 秒 | 连发均匀，松开立刻停（无粘键） |
| 弱网下（可用 `Network Link Conditioner` 加 100ms 延迟 + 1% 丢包）重复 | 仍不吞字，可能整体延迟但不丢 |

> 实现：键盘事件从 unreliable datagram 改走 `send_stream_expect_ack` 可靠流，
> datagram 仅作降级兜底。

### #6 中文本地化

| 步骤 | 期望 |
| --- | --- |
| 语言切到中文，逐屏翻看设置 / 引导 / 设备列表 | 无英文残留；"主控/被控"而非 Server/Client |
| 触发一次暂停通知 | 通知正文是中文 |
| 语言切到 English 再触发暂停通知 | 通知正文变英文 |

### #7 拖拽边界锁定（回归）

| 步骤 | 期望 |
| --- | --- |
| 按住左键从一台机器拖到边界 | 光标被锁在边界，需按住超过设定毫秒才越界 |

---

## 4. 出问题时的排查

```bash
# 实时日志
log stream --predicate 'process == "mykvm"' --level debug

# 或看应用日志文件
tail -f ~/Library/Logs/mykvm/mykvm.log

# 确认权限真的给了
sqlite3 ~/Library/Application\ Support/com.apple.TCC/TCC.db \
  "select client,auth_value from access where service='kTCCServiceAccessibility'" 2>/dev/null
```

常见问题：

- **完全没反应 / 鼠标不动** → 辅助功能权限没给，或换签名后授权失效，删掉重加。
- **启动就崩** → 先 `rm /tmp/mykvm.sock` 再启动（上次异常退出残留锁文件）。
- **滚动方向反了** → 是「反转滚动方向」开关，不是 bug；两台机器只需要开一边。

---

## 5. 提交回报格式

回报时按编号给结论即可，例如：

```
#2 OK
#3 OK
#4 NG — 平滑打开后一格约为关闭时的 1.5 倍
#5 OK
#8 OK
#9 OK
```

NG 的项请附上 `log stream` 里对应时间点的几行日志。
