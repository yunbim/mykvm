# Changelog

This file feeds the GitHub Release notes. Keep entries user-facing: describe what
changed for someone *using* MyKVM, not the internal/CI plumbing. The release
workflow publishes whatever is under `## [Unreleased]`, so move those entries
under a version heading when you cut a release (or just leave them — the next
release will reuse them).

## [Unreleased]

_Nothing yet._

## v0.1.3

### Added

- **macOS local smooth-scroll engine (replaces Mos).** A new macOS-only "平滑滚动" settings card lets you tune scrolling on the Mac itself — smooth scrolling, reverse direction, Option-to-accelerate, Shift-to-horizontal, Command-to-bypass, plus step/speed/transition/interval knobs. It runs independently of the KVM connection (low overhead while idle) and deliberately does **not** sync across machines, so it fully replaces third-party scroll enhancers without double-applying.
- **Pause for specific apps.** You can now list the apps that should pause input sharing (a bundle id on macOS, an executable name on Windows). Once the list has anything in it, it fully replaces the old "pause for any fullscreen app" rule, so watching a fullscreen video or using a fullscreen browser no longer steals the pointer back. Leave the list empty to keep the previous fullscreen behaviour.
- **Reverse scroll direction.** A per-machine toggle that makes content follow your finger, the way macOS "natural scrolling" does — handy when the two machines disagree about scroll direction.

### Changed

- **Self-signed macOS builds + in-app updater now target your fork.** `scripts/build-mac-arm.sh` self-signs the `.app`/`.dmg` so they open without the "unidentified developer" Gatekeeper block (ad-hoc by default; run the printed `sudo` one-liner once to enable a trusted cert that also preserves Accessibility / Input Monitoring grants across updates). The Tauri updater endpoint and public key now point at `yunbim/mykvm` instead of the upstream repo, so in-app updates fetch and verify against your own releases.

### Fixed

- **macOS Secure Keyboard Entry no longer spams on launch.** Detection is now on-demand and fails silently / shows a single non-blocking notice instead of a recurring modal that could lock the UI.
- **Cross-screen cursor no longer stutters after a pause (Win → Mac).** The backgrounded injection task on the receiving Mac was being App-Nap-throttled because App Nap was only suppressed while *this* Mac was the controller. Receiver activity now folds into the same single App Nap decision, and the per-move `CGEventSource` is cached per worker thread instead of rebuilt on every move.
- **macOS window close now drops the Dock icon immediately.** Deactivation is performed on the main thread instead of being deferred.
- **Endpoint ownership cleaned up.** "指定应用时暂停" and "全屏应用自动暂停" are now server-only (hidden on clients and not applied on the client); the macOS scroll engine stays client-only. See `docs/ENDPOINT_OWNERSHIP.md` for the full per-setting ownership and sync strategy.
- **Smooth scrolling no longer changes how far you scroll.** One wheel notch now travels the same distance whether smoothing is on or off. Previously the smoothed path lost distance to rounding on every frame, and on a Windows receiver it *multiplied* each notch by roughly 33× because the interpolator's pixel output was posted as whole wheel notches.
- **Smooth scrolling actually eases now.** The curve filter was a no-op pass-through; it now matches Mos' behaviour, and the interpolator flushes what it still owes when a scroll settles instead of swallowing the last pixel.
- Unsmoothed scrolling on macOS moved a single pixel per notch (it was posting a notch count as a pixel delta). It now posts a proper line scroll.
- The smooth-scroll worker no longer wakes 125 times a second forever — it parks between scrolls and costs nothing while idle.
- Keyboard input from Windows to macOS could drop characters under load. Key events now travel over an acknowledged reliable stream instead of unreliable datagrams, with datagrams kept only as a fallback.
- macOS: clicking the *middle* of another window (not just its title bar) now transfers focus, matching native behaviour. Dock, menu bar, and notification-centre layers are correctly excluded from the hit test.
- macOS: launching at login no longer pops the window open, and the Dock icon now appears only while the window is actually visible.
- macOS: a reboot no longer starts two copies of the app.
- Pause/resume notifications now follow the app language instead of always being Chinese, and several untranslated strings in the UI ("Server"/"Client" badges, unknown platform label, a `bate`/`beta` typo) were fixed.
- Keyboard, mouse, and clipboard could fail to connect between machines — the QUIC handshake rejected the peer with `invalid peer certificate: BadSignature`. The transport now pins the device's advertised certificate directly instead of running brittle chain validation over a self-signed certificate, which fixes cross-platform (macOS ↔ Windows) handshakes.
- macOS: the **app-specific pause list** (pause input sharing when a chosen app is frontmost) now actually works. It was silently disabled on macOS because the frontmost-app watcher never started and the capture hot paths skipped the pause check, so the list did nothing. Sharing now correctly steps aside for a whitelisted app, and returns control to the local machine when you switch into or click inside it.

## v0.4.0

### Added

- Update indicator in the title bar: a download icon appears next to "MyKVM" when a newer version is available — click it to open the update panel.

### Fixed

- "Latest version" in Settings now shows the latest released version once a check completes, instead of staying blank when you are already up to date.
- Corrected the clipboard sync description: images are synced too; only file clipboards are unsupported.

## v0.3.4

### Added

- Encrypted QUIC transport for keyboard, mouse, and clipboard traffic (TLS 1.3, pinned to the paired device's certificate).
- In-app updates: check GitHub Releases and install the latest version without leaving MyKVM.
- Clipboard image sync — copy a picture on one machine and paste it on the other (text was already supported).
- Roam across a remote machine's multiple monitors.
- Cross-platform installers for macOS, Windows, and Linux, built automatically on each release.
- Signed macOS builds, so the Accessibility permission survives app updates.

### Improved

- Smoother, more seamless mouse hand-off when crossing between machines and displays.
- Better modifier-key remapping between macOS and Windows.
- Smoother slide-back when MyKVM is not the front window on macOS.
- More reliable LAN discovery and manual peer connection.

### Fixed

- Trackpad two-finger scrolling on the Settings page.
- Faster, more reliable Windows clipboard sync.

## v0.1.0

- Added server/client onboarding and display layout editing.
- Added LAN discovery, manual peer connection, and shared input transport.
- Added text clipboard sync.
- Added English and Simplified Chinese UI strings.
- Added light, dark, and system theme modes.
- Added configurable single-port UDP transport with fallback.
- Added opt-in app performance monitoring.
- Added GitHub Actions CI and tag-based desktop release builds.
