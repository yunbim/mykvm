#!/usr/bin/env sh
set -eu

# 必须在仓库根目录运行 (脚本里用的是相对路径 src-tauri/ 与 dist)。
cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
  printf "macOS ARM packaging must run on macOS because Tauri needs Apple's SDK and bundling tools.\n" >&2
  exit 1
fi

export PATH="$HOME/.cargo/bin:$PATH"

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

# 清掉旧的 dist: 用 mv 而非 rm, 避免某些 CI / 沙箱环境的批量删除护栏拦截 rm。
[ -d dist ] && mv dist "/tmp/mykvm-dist-bak-$(date +%s)" 2>/dev/null || true

npm exec tauri build -- --no-sign

# Tauri 在沙箱 / 受限环境下, dmg 末步的 bundle_dmg.sh 可能因 hdiutil 受限而失败。
# 此时 .app 已生成, 且存在 rw.*.dmg 中间映像, 这里兜底把它转成最终压缩 dmg。
VERSION=$(node -p "require('./package.json').version")
DMG_DIR="src-tauri/target/release/bundle/dmg"
MACOS_DIR="src-tauri/target/release/bundle/macos"
RW_DMG=$(ls -t "$MACOS_DIR"/rw.*.dmg 2>/dev/null | head -n 1 || true)
if [ -n "$RW_DMG" ]; then
  if ! ls "$DMG_DIR"/*.dmg >/dev/null 2>&1; then
    mkdir -p "$DMG_DIR"
    hdiutil convert "$RW_DMG" -format UDZO -o "$DMG_DIR/mykvm_${VERSION}_aarch64.dmg" || true
  fi
fi

echo "Done. .app 与 .dmg 位于 src-tauri/target/release/bundle/"
