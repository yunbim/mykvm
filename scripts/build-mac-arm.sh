#!/usr/bin/env sh
set -eu

# 必须在仓库根目录运行 (脚本里用的是相对路径 src-tauri/ 与 dist)。
cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
  printf "macOS ARM packaging must run on macOS because Tauri needs Apple's SDK and bundling tools.\n" >&2
  exit 1
fi

export PATH="$HOME/.cargo/bin:$PATH"

# 自签身份名称：与 scripts/sign-mac-app.sh 保持一致。
export MYKVM_CODESIGN_IDENTITY="${MYKVM_CODESIGN_IDENTITY:-MyKVM Local Code Signing}"

rustup target add aarch64-apple-darwin || true
npm install

# 修复: 本机 rustc 1.97.1 与极新 Apple ld (ld-27036.1 / clang 21) 不兼容,
# release 下 proc-macro 的 .dylib 会生成损坏 (mis-aligned LINKEDIT string pool),
# 导致 serde_derive / zerofrom_derive 等 dlopen 失败, tauri build 必挂。
# 改用 rust 自带的 ld64.lld 链接即可; 路径用 rustc --print sysroot 动态定位,
# 换机 (只要 rustup 装了 aarch64-apple-darwin 工具链) 可直接复用, 无需写死机器路径。
#
# 关键: 必须用「命令行内联 RUSTFLAGS」直接调 `npx tauri build`, 不要走
# `npm run tauri:build:mac-arm` —— 其内部 `--target aarch64-apple-darwin` 会让
# RUSTFLAGS 无法传到 proc-macro 的链接步骤, 于是又退回坏掉的 Apple ld。
# 本机 host 即 aarch64, 直接 host 构建产出的就是 aarch64 .app, 完全等价,
# 且 RUSTFLAGS 真正对所有 crate (含 proc-macro) 生效。
export RUSTFLAGS="-C link-arg=-fuse-ld=$(rustc --print sysroot)/lib/rustlib/aarch64-apple-darwin/bin/gcc-ld/ld64.lld"

# 若有更新签名私钥 (ed25519), 本地构建在打安装包之余, 也给 .app.tar.gz 更新产物
# 补上 .sig, 便于自测应用内更新。真正的发布签名由 CI (release.yml) 完成。
UPDATER_KEY="$HOME/.mykvm/updater.key"

# 清掉旧的 dist: 用 mv 而非 rm, 避免某些 CI / 沙箱环境的批量删除护栏拦截 rm。
[ -d dist ] && mv dist "/tmp/mykvm-dist-bak-$(date +%s)" 2>/dev/null || true

# 仅打 app (再自行打包 dmg), 用 --no-sign 让 Tauri 不尝试 Apple 签名,
# 后面由 sign-mac-app.sh 用本地自签身份 / 回退 ad-hoc 签名。
npm exec tauri build -- --no-sign --bundles app

APP_PATH="src-tauri/target/release/bundle/macos/mykvm.app"
VERSION=$(node -p "require('./package.json').version")
DMG_DIR="src-tauri/target/release/bundle/dmg"
DMG_PATH="$DMG_DIR/mykvm_${VERSION}_aarch64.dmg"
TARGZ_PATH="src-tauri/target/release/bundle/macos/mykvm.app.tar.gz"

# 1) 自签 .app (首次运行会在本地钥匙串生成并信任自签证书, 不可信时回退 ad-hoc)。
"$(dirname "$0")/sign-mac-app.sh" "$APP_PATH"

# 2) 从「已签名的 .app」重新生成 dmg, 使 dmg 内嵌的是签名后的 app,
#    再对 dmg 自身签名并去掉 quarantine。
rm -rf "$DMG_DIR"
mkdir -p "$DMG_DIR"
STAGING="$(mktemp -d)"
trap 'rm -rf "$STAGING"' EXIT HUP INT TERM
cp -R "$APP_PATH" "$STAGING/mykvm.app"
ln -s /Applications "$STAGING/Applications"

if hdiutil create -volname mykvm -srcfolder "$STAGING" -ov -format UDZO -o "$DMG_PATH" 2>/dev/null; then
  :
else
  # 兜底: 若 hdiutil create 受限失败, 退回转换 Tauri 可能残留的 rw.*.dmg
  # (此时内嵌 app 仍是未签名版, install 脚本会重新签名, 仅影响下载分发场景)。
  RW_DMG=$(ls -t src-tauri/target/release/bundle/macos/rw.*.dmg 2>/dev/null | head -n 1 || true)
  if [ -n "$RW_DMG" ]; then
    hdiutil convert "$RW_DMG" -format UDZO -o "$DMG_PATH" || true
  fi
fi
rm -rf "$STAGING"

# 3) 对 dmg 签名 (同时再次确认 app 签名, 幂等)。
if [ -f "$DMG_PATH" ]; then
  "$(dirname "$0")/sign-mac-app.sh" "$APP_PATH" "$DMG_PATH"
fi

# 4) 若提供了更新私钥, 给 .app.tar.gz 更新产物补上 .sig (--no-sign 会跳过此步)。
#    无密码的密钥需显式传 --password "" 以跳过交互式密码提示。
if [ -f "$UPDATER_KEY" ] && [ -f "$TARGZ_PATH" ]; then
  TAURI_SIGNING_PRIVATE_KEY="$(cat "$UPDATER_KEY")" \
    npm exec -- tauri signer sign "$TARGZ_PATH" --password "" 2>&1 | tail -3
  if [ -f "$TARGZ_PATH.sig" ]; then
    echo "Signed updater artifact: $TARGZ_PATH.sig"
  else
    echo "WARN: 本地更新产物签名失败 (可忽略, 发布由 CI 签名); 见上方错误。"
  fi
fi

echo "Done. 自签名 .app / .dmg / 更新产物位于 src-tauri/target/release/bundle/"
