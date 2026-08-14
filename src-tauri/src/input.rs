use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex, OnceLock, TryLockError,
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;

use crate::{
    quic_transport,
    shared_input::{
        button_from_mask, mouse_button_mask, InputCommand, InputEvent, MouseButton,
        LEFT_BUTTON_MASK, MIDDLE_BUTTON_MASK, RIGHT_BUTTON_MASK,
    },
    Device, LayoutState, NativeStageStatus, Screen,
};

const INPUT_PROTOCOL: &str = "mykvm.input.v1";
const INPUT_CONTROL_PROTOCOL: &str = "mykvm.input-control.v1";
const EDGE_TOLERANCE: i32 = 80;
// The cursor must reach the very edge pixel before a crossing is considered.
// macOS clamps the pointer to the screen, so the furthest it can sit is
// width-1 (the last pixel); x >= right-1 means "pushed flush against the edge",
// matching how a real extended display only hands off once the cursor is on the
// boundary. CGEvent deltas are raw HID movement, so a positive dx with the
// pointer already pinned at the edge still reads as the user pushing outward —
// that push is what triggers the handoff.
const CROSSING_MARGIN: f64 = 1.0;
const MIN_CROSSING_DELTA: f64 = 1.0;
const CROSSING_AXIS_DOMINANCE: f64 = 0.5;
const CROSSING_ACTIVATION_BAND: f64 = EDGE_TOLERANCE as f64 * 2.0;
// On return to the local machine, land the cursor flush against the entry edge
// (0px inset) for a seamless extended-display feel, mirroring how the cursor
// would sit at the edge of a real second monitor. Re-bounce after a fast
// back-flick is prevented by a time-based return cooldown (last_return), not by
// inset distance, so the cursor can sit exactly on the edge.
const RETURN_EDGE_INSET: f64 = 0.0;
// After returning to local, refuse to cross back into the remote for this long.
// Lets a fast back-flick settle at the edge without bouncing into the remote.
const RETURN_COOLDOWN_MS: u64 = 150;
const MOUSE_MOVE_SEND_INTERVAL_MS: u64 = 8;
const DRAG_MOVE_SEND_INTERVAL_MS: u64 = 8;
// While a physical mouse button is held on this machine the user is almost
// always dragging something — a window into an Aero Snap / split-view hot zone,
// a selection rectangle, a scrollbar. Handing the cursor to the remote mid-drag
// destroys those gestures, so a crossing is refused for the first
// DRAG_CROSSING_HOLD_MS the pointer spends pressed against the shared edge.
// Keep pushing past that and the crossing is allowed, which is what makes
// dragging a file across to the other machine still work: the deliberate
// "shove and hold" reads as intent, a snap-to-edge flick does not.
const DRAG_CROSSING_HOLD_MS: u64 = 800;
#[cfg(target_os = "macos")]
const MACOS_IDLE_CAPTURE_LOOP_MS: u64 = 100;
#[cfg(target_os = "macos")]
const MACOS_VISIBLE_REMOTE_CAPTURE_LOOP_MS: u64 = 16;
#[cfg(target_os = "macos")]
const MACOS_HIDDEN_REMOTE_CAPTURE_LOOP_MS: u64 = 50;
#[cfg(target_os = "macos")]
const MACOS_HIDDEN_WINDOW_CURSOR_HIDE_REASSERT_MS: u64 = 250;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_SYSTEM_DEFINED: u32 = 14;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_ROTATE: u32 = 18;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_BEGIN_GESTURE: u32 = 19;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_END_GESTURE: u32 = 20;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_GESTURE: u32 = 29;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_MAGNIFY: u32 = 30;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_SWIPE: u32 = 31;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_SMART_MAGNIFY: u32 = 32;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_QUICK_LOOK: u32 = 33;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_PRESSURE: u32 = 34;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_DIRECT_TOUCH: u32 = 37;
#[cfg(target_os = "macos")]
const MACOS_NSEVENT_TYPE_CHANGE_MODE: u32 = 38;
#[cfg(target_os = "macos")]
const MACOS_RAW_EVENT_TAP_DISABLED_BY_TIMEOUT: u32 = 0xFFFF_FFFE;
#[cfg(target_os = "macos")]
const MACOS_RAW_EVENT_TAP_DISABLED_BY_USER_INPUT: u32 = 0xFFFF_FFFF;
#[cfg(target_os = "macos")]
const MACOS_RAW_GESTURE_EVENT_TYPES: &[u32] = &[
    MACOS_NSEVENT_TYPE_SYSTEM_DEFINED,
    MACOS_NSEVENT_TYPE_ROTATE,
    MACOS_NSEVENT_TYPE_BEGIN_GESTURE,
    MACOS_NSEVENT_TYPE_END_GESTURE,
    MACOS_NSEVENT_TYPE_GESTURE,
    MACOS_NSEVENT_TYPE_MAGNIFY,
    MACOS_NSEVENT_TYPE_SWIPE,
    MACOS_NSEVENT_TYPE_SMART_MAGNIFY,
    MACOS_NSEVENT_TYPE_QUICK_LOOK,
    MACOS_NSEVENT_TYPE_PRESSURE,
    MACOS_NSEVENT_TYPE_DIRECT_TOUCH,
    MACOS_NSEVENT_TYPE_CHANGE_MODE,
];
#[cfg(target_os = "windows")]
const WINDOWS_DESKTOP_CHECK_INTERVAL_MS: u64 = 250;

/// How often the full pairing credentials (~0.5KB, mostly the base64 transport
/// certificate) ride an input packet. Between refreshes, packets omit them and
/// the receiver authorizes by its per-source cache — cutting steady-state
/// input datagrams from ~0.8KB to ~0.15KB. Must stay below
/// `INPUT_ORIGIN_CACHE_TTL` so the receiver's authorization never lapses
/// mid-session.
const INPUT_FULL_CRED_REFRESH: Duration = Duration::from_secs(2);
/// How long the receiver treats a source address as authorized after a
/// credentialled packet, so it can admit the credential-less packets in
/// between. A peer that never proved the pairing secret from this address
/// never gets an entry, so credential-less packets from it are always rejected.
const INPUT_ORIGIN_CACHE_TTL: Duration = Duration::from_secs(5);

/// True when a full-credential input packet is due for a destination whose last
/// credentialled send was `last_sent` (or never). Pure half of
/// `should_send_full_input_credentials`.
fn credential_send_due(last_sent: Option<Instant>, now: Instant) -> bool {
    last_sent
        .map(|last| now.saturating_duration_since(last) >= INPUT_FULL_CRED_REFRESH)
        .unwrap_or(true)
}

/// True when a source authorized at `authorized_at` is still within the cache
/// TTL. Pure half of `input_origin_recently_authorized`.
fn origin_authorization_fresh(authorized_at: Option<Instant>, now: Instant) -> bool {
    authorized_at
        .map(|at| now.saturating_duration_since(at) < INPUT_ORIGIN_CACHE_TTL)
        .unwrap_or(false)
}

/// Per-destination timestamp of the last full-credential input packet. Keyed by
/// the target's QUIC address so alternating between two targets still refreshes
/// each independently.
fn input_full_cred_tracker() -> &'static Mutex<HashMap<String, Instant>> {
    static TRACKER: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Decides whether to attach full credentials to the packet now being built for
/// `addr`, and records the decision so the next `INPUT_FULL_CRED_REFRESH`
/// window of packets can omit them. On lock poisoning it errs toward including
/// credentials (a larger but always-authorizable packet).
fn should_send_full_input_credentials(addr: &str) -> bool {
    let Ok(mut tracker) = input_full_cred_tracker().lock() else {
        return true;
    };
    let now = Instant::now();
    let due = credential_send_due(tracker.get(addr).copied(), now);
    if due {
        tracker.retain(|_, last| {
            now.saturating_duration_since(*last) < INPUT_ORIGIN_CACHE_TTL.saturating_mul(4)
        });
        tracker.insert(addr.to_string(), now);
    }
    due
}

/// Source addresses that sent a valid credentialled input packet within
/// `INPUT_ORIGIN_CACHE_TTL`, so their credential-less packets can be admitted.
fn authorized_input_origins() -> &'static Mutex<HashMap<SocketAddr, Instant>> {
    static ORIGINS: OnceLock<Mutex<HashMap<SocketAddr, Instant>>> = OnceLock::new();
    ORIGINS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_authorized_input_origin(source: SocketAddr) {
    if let Ok(mut origins) = authorized_input_origins().lock() {
        let now = Instant::now();
        origins.retain(|_, last| now.saturating_duration_since(*last) < INPUT_ORIGIN_CACHE_TTL);
        origins.insert(source, now);
    }
}

fn input_origin_recently_authorized(source: SocketAddr) -> bool {
    let authorized_at = authorized_input_origins()
        .lock()
        .ok()
        .and_then(|origins| origins.get(&source).copied());
    origin_authorization_fresh(authorized_at, Instant::now())
}

/// Last injected remote cursor position, packed x<<32|y, plus held buttons.
/// Plain atomics: these are touched on every received mouse event and a
/// global mutex there is contention for nothing (the fields are independent
/// and a racing reader tolerates one event of skew).
static REMOTE_MOUSE_POSITION: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
static REMOTE_MOUSE_BUTTONS: AtomicU64 = AtomicU64::new(0);
#[cfg(target_os = "macos")]
static MACOS_ACCESSIBILITY_PROMPTED: AtomicBool = AtomicBool::new(false);
#[cfg(target_os = "windows")]
static WINDOWS_INPUT_DESKTOP_DEFAULT_CACHE: AtomicBool = AtomicBool::new(true);

#[derive(Debug, Clone, Copy, PartialEq)]
enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone)]
struct InputTarget {
    device_id: String,
    origin_device_id: String,
    cluster_id: String,
    pair_secret: String,
    target_addr: String,
    target_platform: String,
    transport_public_key: String,
    protocol_version: u16,
    screen_id: String,
    local_screen: Screen,
    layout_local_screen: Screen,
    remote_screen: Screen,
    edge: Edge,
}

#[derive(Debug, Clone)]
struct ActiveTarget {
    target: InputTarget,
    // The remote screen the cursor is currently over and the wire id we send for
    // it. These start as the screen we crossed into and change as the cursor
    // roams across the remote device's other screens. `x`/`y` are coordinates
    // local to `current_screen`.
    current_screen: Screen,
    current_screen_id: String,
    x: f64,
    y: f64,
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    invert_y: bool,
}

#[derive(Debug, Clone)]
pub struct ClipboardTarget {
    pub device_id: String,
    pub addr: String,
    pub transport_public_key: String,
    pub protocol_version: u16,
    pub cluster_id: String,
    pub pair_secret: String,
    pub expires_at: Option<Instant>,
}

fn str_ref_is_empty(value: &&str) -> bool {
    value.is_empty()
}

/// Borrowing serialization mirror of [`InputPacket`]: identical named
/// MessagePack bytes when every field is populated (guarded by a test), but
/// building one clones none of the ~0.8KB of credential strings — send_packet
/// runs per mouse event. The credential fields are also skipped when empty, so
/// steady-state packets (which omit them — see send_packet) drop ~0.5KB of the
/// static pairing block, mostly the base64 transport certificate.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InputPacketRef<'a> {
    protocol: &'a str,
    target_device_id: &'a str,
    #[serde(skip_serializing_if = "str_ref_is_empty")]
    origin_device_id: &'a str,
    origin_port: u16,
    #[serde(skip_serializing_if = "str_ref_is_empty")]
    origin_transport_public_key: &'a str,
    origin_protocol_version: u16,
    #[serde(skip_serializing_if = "str_ref_is_empty")]
    cluster_id: &'a str,
    #[serde(skip_serializing_if = "str_ref_is_empty")]
    pair_secret: &'a str,
    event: &'a InputEvent,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputPacket {
    protocol: String,
    #[serde(default)]
    target_device_id: String,
    #[serde(default)]
    origin_device_id: String,
    #[serde(default)]
    origin_port: u16,
    #[serde(default)]
    origin_transport_public_key: String,
    #[serde(default = "default_protocol_version")]
    origin_protocol_version: u16,
    #[serde(default)]
    cluster_id: String,
    #[serde(default)]
    pair_secret: String,
    event: InputEvent,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InputControlPacket {
    protocol: String,
    #[serde(default)]
    target_device_id: String,
    #[serde(default)]
    origin_device_id: String,
    #[serde(default)]
    origin_transport_public_key: String,
    #[serde(default = "default_protocol_version")]
    origin_protocol_version: u16,
    #[serde(default)]
    cluster_id: String,
    #[serde(default)]
    pair_secret: String,
    command: InputControlCommand,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum InputControlCommand {
    SecureAttention,
}

pub fn stopped_capture_status() -> NativeStageStatus {
    NativeStageStatus {
        state: "stubbed".into(),
        detail: "Input sharing is stopped.".into(),
    }
}

pub fn stopped_inject_status() -> NativeStageStatus {
    NativeStageStatus {
        state: "stubbed".into(),
        detail: "Input injection is stopped.".into(),
    }
}

/// Direction requested by a screen-switch hotkey. Maps onto the `Edge` that a
/// mouse crossing would follow: `Right` means "the remote sits to the right of
/// the local screen", matching `Edge::Right` on the `InputTarget`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwitchDirection {
    Left,
    Right,
    Up,
    Down,
}

impl SwitchDirection {
    fn matches_edge(self, edge: Edge) -> bool {
        matches!(
            (self, edge),
            (SwitchDirection::Left, Edge::Left)
                | (SwitchDirection::Right, Edge::Right)
                | (SwitchDirection::Up, Edge::Top)
                | (SwitchDirection::Down, Edge::Bottom)
        )
    }
}

/// Outcome of a hotkey-driven switch request. The capture loop acts on it: an
/// `Enter` builds an `ActiveTarget` and runs the enter sequence; a `Return`
/// hands control back to the local machine.
enum SwitchOutcome {
    Enter(ActiveTarget),
    LocalMove {
        from_screen_id: String,
        to_screen_id: String,
        x: f64,
        y: f64,
    },
    Return,
    Noop,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct HotkeyModifiers {
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
}

fn screen_switch_hotkey_matches_vk(
    layout_state: &Arc<Mutex<LayoutState>>,
    key_code: u16,
    modifiers: HotkeyModifiers,
) -> bool {
    let Ok(layout) = layout_state.lock() else {
        return false;
    };
    if layout.machine_role != "server" {
        return false;
    }

    screen_switch_hotkeys_match_vk(&layout.screen_switch_hotkeys, key_code, modifiers)
}

fn screen_switch_hotkeys_match_vk(
    hotkeys: &crate::ScreenSwitchHotkeys,
    key_code: u16,
    modifiers: HotkeyModifiers,
) -> bool {
    [
        hotkeys.left.as_str(),
        hotkeys.right.as_str(),
        hotkeys.up.as_str(),
        hotkeys.down.as_str(),
    ]
    .into_iter()
    .any(|hotkey| hotkey_matches_vk(hotkey, key_code, modifiers))
}

fn hotkey_matches_vk(value: &str, key_code: u16, modifiers: HotkeyModifiers) -> bool {
    let normalized = value.trim().to_ascii_lowercase().replace(' ', "");
    if normalized.is_empty()
        || matches!(normalized.as_str(), "disabled" | "disable" | "off" | "none")
    {
        return false;
    }

    let mut required = HotkeyModifiers::default();
    let mut main_key = None;
    for part in normalized.split('+').filter(|part| !part.is_empty()) {
        match part {
            "ctrl" | "control" => required.ctrl = true,
            "alt" | "option" => required.alt = true,
            "shift" => required.shift = true,
            "meta" | "cmd" | "command" | "win" | "windows" | "super" | "os" => {
                required.meta = true;
            }
            key => {
                if main_key.is_some() {
                    return false;
                }
                main_key = hotkey_key_to_windows_vk(key);
            }
        }
    }

    main_key == Some(key_code) && required == modifiers
}

fn hotkey_key_to_windows_vk(key: &str) -> Option<u16> {
    if key.len() == 1 {
        let byte = key.as_bytes()[0];
        if byte.is_ascii_alphabetic() {
            return Some(byte.to_ascii_uppercase() as u16);
        }
        if byte.is_ascii_digit() {
            return Some(byte as u16);
        }
    }

    if let Some(function_number) = key
        .strip_prefix('f')
        .and_then(|value| value.parse::<u16>().ok())
    {
        if (1..=24).contains(&function_number) {
            return Some(0x70 + function_number - 1);
        }
    }

    Some(match key {
        "space" | "spacebar" => 0x20,
        "tab" => 0x09,
        "enter" | "return" => 0x0D,
        "esc" | "escape" => 0x1B,
        "scrolllock" | "scroll" | "scrlk" => 0x91,
        "up" | "arrowup" => 0x26,
        "down" | "arrowdown" => 0x28,
        "left" | "arrowleft" => 0x25,
        "right" | "arrowright" => 0x27,
        _ => return None,
    })
}

/// Resolve a hotkey switch request against the current targets and active
/// state. Called from the capture thread's poll loop.
///
/// - If we are currently local (`active` is `None`): move to a local screen in
///   that direction when one exists, otherwise pick the first online remote
///   target whose `edge` matches the requested direction.
/// - If we are already controlling a remote (`active` is `Some`): request a
///   return to local. The user can then press the direction key again to cross
///   into a different remote.
#[cfg(test)]
fn request_screen_switch(
    direction: SwitchDirection,
    layout_state: &Arc<Mutex<LayoutState>>,
    native_layout: &LayoutState,
    active: &Mutex<Option<ActiveTarget>>,
) -> SwitchOutcome {
    request_screen_switch_from_point(direction, layout_state, native_layout, active, None)
}

fn request_screen_switch_from_point(
    direction: SwitchDirection,
    layout_state: &Arc<Mutex<LayoutState>>,
    native_layout: &LayoutState,
    active: &Mutex<Option<ActiveTarget>>,
    current_point: Option<(f64, f64)>,
) -> SwitchOutcome {
    let currently_remote = active.lock().map(|a| a.is_some()).unwrap_or(false);
    if currently_remote {
        return SwitchOutcome::Return;
    }

    // Rebuild targets from the live layout every time: peers come and go after
    // the capture thread started, so the static snapshot built at startup would
    // miss a device that appeared later.
    let Ok(layout) = layout_state.lock() else {
        return SwitchOutcome::Noop;
    };
    let source_screen_id =
        source_local_screen(&layout, native_layout, current_point).map(|screen| screen.id.clone());
    if let Some(local_move) = local_screen_switch_point(
        direction,
        &layout,
        native_layout,
        source_screen_id.as_deref(),
    ) {
        return SwitchOutcome::LocalMove {
            from_screen_id: local_move.from_screen_id,
            to_screen_id: local_move.to_screen_id,
            x: local_move.x,
            y: local_move.y,
        };
    }
    let targets = build_input_targets(&layout, native_layout);
    drop(layout);

    let target = targets
        .iter()
        .filter(|target| {
            source_screen_id
                .as_deref()
                .map(|id| target.layout_local_screen.id == id)
                .unwrap_or(true)
        })
        .find(|target| direction.matches_edge(target.edge))
        .or_else(|| {
            targets
                .iter()
                .find(|target| direction.matches_edge(target.edge))
        });
    let Some(target) = target else {
        return SwitchOutcome::Noop;
    };

    // Land the remote cursor at the centre of the entry screen — there is no
    // mouse trajectory to derive an entry offset from, so the middle is the
    // least surprising landing spot.
    let remote_x = (target.remote_screen.width as f64 / 2.0)
        .clamp(0.0, (target.remote_screen.width - 1) as f64);
    let remote_y = (target.remote_screen.height as f64 / 2.0)
        .clamp(0.0, (target.remote_screen.height - 1) as f64);

    let mut current_screen = target.remote_screen.clone();
    current_screen.id = target.screen_id.clone();

    SwitchOutcome::Enter(ActiveTarget {
        target: target.clone(),
        current_screen,
        current_screen_id: target.screen_id.clone(),
        x: remote_x,
        y: remote_y,
        invert_y: false,
    })
}

fn source_local_screen<'a>(
    layout: &'a LayoutState,
    native_layout: &LayoutState,
    current_point: Option<(f64, f64)>,
) -> Option<&'a Screen> {
    let local = local_device(layout)?;
    if let Some((x, y)) = current_point {
        if let Some(native_local) = local_device(native_layout) {
            for native_screen in &native_local.screens {
                let native_screen = platform_native_screen(native_screen);
                if point_in_screen(&native_screen, x, y) {
                    if let Some(screen) = local
                        .screens
                        .iter()
                        .find(|screen| screen.id == native_screen.id)
                    {
                        return Some(screen);
                    }
                }
            }
        }
        if let Some(screen) = local
            .screens
            .iter()
            .find(|screen| point_in_screen(screen, x, y))
        {
            return Some(screen);
        }
    }

    local
        .screens
        .iter()
        .find(|screen| screen.is_primary)
        .or_else(|| local.screens.first())
}

struct LocalScreenMove {
    from_screen_id: String,
    to_screen_id: String,
    x: f64,
    y: f64,
}

fn local_screen_switch_point(
    direction: SwitchDirection,
    layout: &LayoutState,
    native_layout: &LayoutState,
    source_screen_id: Option<&str>,
) -> Option<LocalScreenMove> {
    let local = local_device(layout)?;
    let source = source_screen_id
        .and_then(|id| local.screens.iter().find(|screen| screen.id == id))
        .or_else(|| local.screens.iter().find(|screen| screen.is_primary))
        .or_else(|| local.screens.first())?;

    let target = local.screens.iter().find(|screen| {
        screen.id != source.id
            && !screens_overlap(source, screen)
            && touching_edge(source, screen)
                .map(|edge| direction.matches_edge(edge))
                .unwrap_or(false)
    })?;

    let native_target = local_device(native_layout)
        .and_then(|device| device.screens.iter().find(|screen| screen.id == target.id))
        .map(platform_native_screen)
        .unwrap_or_else(|| platform_native_screen(target));
    let (x, y) = screen_center_point(&native_target);
    Some(LocalScreenMove {
        from_screen_id: source.id.clone(),
        to_screen_id: target.id.clone(),
        x,
        y,
    })
}

fn screen_center_point(screen: &Screen) -> (f64, f64) {
    (
        screen.x as f64 + (screen.width as f64 / 2.0).clamp(0.0, (screen.width - 1).max(0) as f64),
        screen.y as f64
            + (screen.height as f64 / 2.0).clamp(0.0, (screen.height - 1).max(0) as f64),
    )
}

fn remembered_local_screen_point(
    points: &Mutex<HashMap<String, (f64, f64)>>,
    from_screen_id: &str,
    to_screen_id: &str,
    current_point: Option<(f64, f64)>,
    fallback: (f64, f64),
) -> (f64, f64) {
    let Ok(mut points) = points.lock() else {
        return fallback;
    };
    if let Some(point) = current_point {
        points.insert(from_screen_id.to_string(), point);
    }
    points.get(to_screen_id).copied().unwrap_or(fallback)
}

pub fn start_input_runtime(
    layout: LayoutState,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    quic_transport: quic_transport::TransportHandle,
    stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    main_window_visible: Arc<AtomicBool>,
    main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> (NativeStageStatus, NativeStageStatus) {
    let inject_status = input_receive_status(&layout, true);
    if layout.input_mode == "receive" {
        remote_active.store(false, Ordering::Relaxed);
        clear_clipboard_target(&clipboard_target);
        start_platform_receive_monitor(stop);
        return (receive_only_status(), inject_status);
    }

    let targets = build_input_targets(&layout, &native_layout);
    let capture_status = start_input_capture(
        targets,
        layout_state,
        native_layout,
        quic_transport,
        stop,
        remote_active,
        main_window_visible,
        main_window_focused,
        clipboard_target,
        input_events,
        switch_request,
    );

    (capture_status, inject_status)
}

pub fn input_runtime_status(
    layout: &LayoutState,
    native_layout: &LayoutState,
) -> (NativeStageStatus, NativeStageStatus) {
    let targets = build_input_targets(layout, native_layout);
    let capture = if layout.input_mode == "receive" {
        receive_only_status()
    } else if targets.is_empty() {
        no_target_status(layout)
    } else if cfg!(any(target_os = "macos", target_os = "windows")) {
        NativeStageStatus {
            state: "ready".into(),
            detail: format!(
                "控制端已就绪，{} 条远端贴边可用于鼠标和键盘切换。",
                targets.len()
            ),
        }
    } else {
        unsupported_capture_status()
    };

    (capture, input_receive_status(layout, false))
}

fn input_receive_status(layout: &LayoutState, request_permission: bool) -> NativeStageStatus {
    let _ = request_permission;

    #[cfg(target_os = "macos")]
    if !macos_accessibility_trusted(request_permission) {
        return NativeStageStatus {
            state: "error".into(),
            detail: "macOS 需要给 MyKVM 辅助功能权限才能注入远端点击和键盘。请到 系统设置 > 隐私与安全性 > 辅助功能 启用 MyKVM，然后完全退出并重新打开应用。".into(),
        };
    }

    // NOTE: Secure Keyboard Entry is deliberately NOT probed here.
    //
    // `IsSecureEventInputEnabled()` is a *system-wide* flag, not a
    // "MyKVM is blocked" flag. `loginwindow` holds it for a while after boot
    // and after every unlock, Terminal holds it whenever its menu item is
    // checked, and password managers hold it whenever their field has focus.
    // Reporting that as a hard `error` from a status function that the UI polls
    // every two seconds produced a modal that reappeared as fast as the user
    // could dismiss it — with nothing actually broken. Detection now happens
    // on demand at the point a key is really injected
    // (`macos_probe_secure_input_on_key_inject`), degrades silently, and at
    // most raises one non-blocking notification per episode.

    NativeStageStatus {
        state: "ready".into(),
        detail: format!(
            "Receiving shared input on QUIC datagrams at UDP {}.",
            normalize_quic_port(layout.transport_port, layout.quic_port)
        ),
    }
}

#[cfg(target_os = "macos")]
fn macos_accessibility_trusted(request_permission: bool) -> bool {
    use core_foundation::{
        base::TCFType, boolean::CFBoolean, dictionary::CFDictionary, string::CFString,
    };

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(
            options: core_foundation::dictionary::CFDictionaryRef,
        ) -> bool;
    }

    if !request_permission || MACOS_ACCESSIBILITY_PROMPTED.swap(true, Ordering::Relaxed) {
        return unsafe { AXIsProcessTrusted() };
    }

    let key = CFString::from_static_string("AXTrustedCheckOptionPrompt");
    let value = CFBoolean::true_value();
    let options = CFDictionary::from_CFType_pairs(&[(key, value)]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef()) }
}

/// Reports whether macOS Secure Keyboard Entry is currently enabled by any
/// process. While it is on, synthetic keyboard events posted via CGEvent are
/// discarded by the window server (mouse events are unaffected).
#[cfg(target_os = "macos")]
fn macos_secure_input_enabled() -> bool {
    #[link(name = "Carbon", kind = "framework")]
    extern "C" {
        // Returns a Carbon `Boolean` (unsigned char); read it as u8 to avoid
        // relying on a non-0/1 value being a valid Rust bool.
        fn IsSecureEventInputEnabled() -> u8;
    }

    unsafe { IsSecureEventInputEnabled() != 0 }
}

/// Minimum spacing between two Secure Keyboard Entry probes. The Carbon call is
/// cheap but it still crosses into HIToolbox, and a held key repeats at up to
/// ~30 Hz, so it is rate limited rather than run per keystroke.
#[cfg(target_os = "macos")]
const SECURE_INPUT_PROBE_INTERVAL_MS: u64 = 2_000;

/// On-demand Secure Keyboard Entry check, called from the key-injection path.
///
/// Runs only when a key is actually being injected — never on a timer and never
/// during startup — so a transient system-wide secure-input holder (loginwindow
/// right after boot, a focused password field, Terminal's "Secure Keyboard
/// Entry" menu item) can no longer surface as a startup error.
///
/// Behaviour when it *is* on: log once, raise a single non-blocking
/// notification per episode, and otherwise degrade silently. The CGEvent is
/// still posted; macOS discards it, which is the documented behaviour and not
/// something the app can work around. The latch resets as soon as a later probe
/// sees secure input switched off, so a genuine recurrence notifies again.
#[cfg(target_os = "macos")]
fn macos_probe_secure_input_on_key_inject() {
    // Monotonic ms since the first probe. 0 is reserved for "never probed".
    static PROBE_EPOCH: OnceLock<Instant> = OnceLock::new();
    static LAST_PROBE_MS: AtomicU64 = AtomicU64::new(0);
    static NOTIFIED: AtomicBool = AtomicBool::new(false);

    let stamp = PROBE_EPOCH
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .saturating_add(1) as u64;
    let last = LAST_PROBE_MS.load(Ordering::Relaxed);
    if last != 0 && stamp.saturating_sub(last) < SECURE_INPUT_PROBE_INTERVAL_MS {
        return;
    }
    LAST_PROBE_MS.store(stamp, Ordering::Relaxed);

    if !macos_secure_input_enabled() {
        NOTIFIED.store(false, Ordering::Relaxed);
        return;
    }
    if NOTIFIED.swap(true, Ordering::Relaxed) {
        return;
    }

    log::info!(
        "macOS Secure Keyboard Entry is enabled system-wide; injected key events are being \
         discarded by the window server until the owning app releases it"
    );
    notify_secure_input_blocked();
}

/// One-shot, non-blocking heads-up that injected keys are being swallowed.
/// Deliberately a notification banner, not a modal: nothing the user does in
/// MyKVM can clear the condition, so it must never take over the window.
#[cfg(target_os = "macos")]
fn notify_secure_input_blocked() {
    let Some(app) = NOTIFY_APP_HANDLE.get() else {
        return;
    };
    let (title, body) = if notify_language_is_en() {
        (
            "mykvm · Keyboard blocked",
            "macOS Secure Keyboard Entry is on, so remote keystrokes are discarded (clicks still \
             work). Leave the password field, or uncheck Terminal > Secure Keyboard Entry.",
        )
    } else {
        (
            "mykvm · 键盘被系统拦截",
            "macOS 安全键盘输入已开启，远端按键会被系统丢弃（鼠标点击不受影响）。离开密码输入框，或取消勾选“终端 > 安全键盘输入”即可恢复。",
        )
    };
    if let Err(error) = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show()
    {
        log::warn!("secure input notification failed: {error}");
    }
}

fn start_input_capture(
    targets: Vec<InputTarget>,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    quic_transport: quic_transport::TransportHandle,
    stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    main_window_visible: Arc<AtomicBool>,
    main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> NativeStageStatus {
    invalidate_input_targets_cache();
    start_platform_capture(
        targets,
        layout_state,
        native_layout,
        quic_transport,
        stop,
        remote_active,
        main_window_visible,
        main_window_focused,
        clipboard_target,
        input_events,
        switch_request,
    )
}

#[cfg(target_os = "macos")]
fn start_platform_capture(
    targets: Vec<InputTarget>,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    quic_transport: quic_transport::TransportHandle,
    stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    main_window_visible: Arc<AtomicBool>,
    _main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> NativeStageStatus {
    use core_foundation::runloop::{kCFRunLoopCommonModes, kCFRunLoopDefaultMode, CFRunLoop};
    use core_graphics::event::{
        CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType,
    };

    let (ready_tx, ready_rx) = mpsc::channel();
    let target_count = targets.len();

    thread::spawn(move || {
        let local_y_bounds = local_y_bounds(&targets);
        let display_snapshots = mac_display_snapshots();
        enable_macos_background_cursor_hide();
        let context = Arc::new(MacCaptureContext {
            quic_transport,
            layout_state,
            native_layout,
            active: Mutex::new(None),
            remote_active,
            main_window_visible,
            clipboard_target,
            input_events,
            targets,
            switch_request,
            anchor: Mutex::new(None),
            cursor_hidden: Mutex::new(false),
            cursor_hide_depth: Mutex::new(0),
            last_cursor_hide_reassert: Mutex::new(None),
            last_mouse_move_sent: Mutex::new(None),
            last_cursor_repin: Mutex::new(None),
            last_return: Mutex::new(None),
            remote_button_mask: AtomicU64::new(0),
            pressed_modifiers: Mutex::new(Vec::new()),
            pressed_keys: Mutex::new(Vec::new()),
            tap_disabled: AtomicBool::new(false),
            just_crossed: AtomicBool::new(false),
            suppress_next_mouse_delta: AtomicBool::new(false),
            hotkey_return_point: Mutex::new(None),
            local_screen_points: Mutex::new(HashMap::new()),
            local_y_bounds,
            display_snapshots,
        });
        let callback_context = Arc::clone(&context);
        let event_types = vec![
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::ScrollWheel,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
        ];

        // SAFETY: the tap is created, used, and dropped on this same thread; the
        // callback only borrows `callback_context` (an Arc that outlives the
        // tap), so it never runs after this thread unwinds.
        let tap = match unsafe {
            CGEventTap::new_unchecked(
                CGEventTapLocation::HID,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                event_types,
                move |_proxy, event_type, event| {
                    handle_macos_event(&callback_context, event_type, event)
                },
            )
        } {
            Ok(tap) => tap,
            Err(_) => {
                let _ = ready_tx.send(Err(
                    "macOS 生产包需要单独授权辅助功能和输入监控。请到 系统设置 > 隐私与安全性 > 辅助功能 / 输入监控 启用 MyKVM，然后完全退出并重新打开应用。".into(),
                ));
                return;
            }
        };

        let loop_source = match tap.mach_port().create_runloop_source(0) {
            Ok(source) => source,
            Err(_) => {
                let _ = ready_tx.send(Err("failed to attach macOS event tap to run loop".into()));
                return;
            }
        };
        CFRunLoop::get_current().add_source(&loop_source, unsafe { kCFRunLoopCommonModes });
        let mut raw_gesture_taps = Vec::new();
        let mut _raw_gesture_loop_sources = Vec::new();
        for location in [CGEventTapLocation::HID, CGEventTapLocation::Session] {
            match RawMacosGestureTap::new(location, Arc::clone(&context)) {
                Ok(raw_tap) => match raw_tap.mach_port().create_runloop_source(0) {
                    Ok(source) => {
                        CFRunLoop::get_current()
                            .add_source(&source, unsafe { kCFRunLoopCommonModes });
                        raw_tap.enable();
                        _raw_gesture_loop_sources.push(source);
                        raw_gesture_taps.push(raw_tap);
                    }
                    Err(_) => {
                        log::warn!(
                            "failed to attach raw macOS gesture event tap {:?} to run loop",
                            location
                        );
                    }
                },
                Err(_) => {
                    log::warn!(
                        "failed to create raw macOS gesture event tap {:?}",
                        location
                    );
                }
            }
        }
        tap.enable();
        let _ = ready_tx.send(Ok(()));
        // Watch the frontmost app so sharing steps aside for a whitelisted one
        // (#8). This mirrors the Windows capture thread; without it the macOS
        // watcher never runs and the whitelist silently does nothing.
        spawn_app_pause_watcher(Arc::clone(&stop));
        let mut app_nap_suppressed = false;

        while !stop.load(Ordering::Relaxed) {
            let was_remote_active = context.remote_active.load(Ordering::Relaxed);
            // Receiver-side activity (this Mac being driven by a remote peer) is
            // folded into the single App Nap decision so a backgrounded
            // injection task is never throttled. See `macos_receiver_activity`.
            let receiver_live = macos_receiver_activity::is_live();
            let app_nap_target = was_remote_active || receiver_live;
            if app_nap_suppressed != app_nap_target {
                set_macos_app_nap_suppressed(app_nap_target);
                app_nap_suppressed = app_nap_target;
            }
            let _ = CFRunLoop::run_in_mode(
                unsafe { kCFRunLoopDefaultMode },
                Duration::from_millis(macos_capture_loop_ms(
                    was_remote_active,
                    context.main_window_visible.load(Ordering::Relaxed),
                )),
                false,
            );
            drain_switch_request_macos(&context);
            // macOS disables a tap whose callback ran too long or that idled out.
            // Without re-enabling it the mouse and keyboard silently freeze until
            // the app restarts, which is the classic "works, then sticks after a
            // while" failure. Re-arm it as soon as we notice.
            if context.tap_disabled.swap(false, Ordering::Relaxed) {
                tap.enable();
                for raw_tap in &raw_gesture_taps {
                    raw_tap.enable();
                }
                log::debug!("[diag] event tap re-enabled after being disabled");
            }
            // While controlling a remote, macOS can re-associate the physical
            // mouse with the local cursor (especially when backgrounded),
            // making the server pointer reappear and follow the mouse.
            // Re-pin it to the anchor and re-assert hide while active.
            let is_remote_active = context.remote_active.load(Ordering::Relaxed);
            // Re-evaluate App Nap with receiver activity folded in (single owner
            // of the activity token — never call set_macos_app_nap_suppressed
            // from two owners or the token leaks). The re-pin below stays on the
            // sender-only `is_remote_active` so receiver state can't yank the
            // local cursor to a stale anchor.
            let app_nap_target = is_remote_active || receiver_live;
            if app_nap_suppressed != app_nap_target {
                set_macos_app_nap_suppressed(app_nap_target);
                app_nap_suppressed = app_nap_target;
            }
            if is_remote_active {
                repin_macos_cursor_while_remote(&context);
            }
        }

        // Critical safety: never leave the cursor decoupled after capture stops,
        // otherwise the user's mouse stays frozen until the app restarts.
        set_macos_cursor_decoupled(false);
        set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
        show_macos_cursor_if_needed(&context);
        if app_nap_suppressed {
            set_macos_app_nap_suppressed(false);
        }
        context.remote_active.store(false, Ordering::Relaxed);
        clear_clipboard_target(&context.clipboard_target);
        // The tap is gone, so button-up will never arrive; drop the drag gate
        // rather than leaving it latched across a restart.
        reset_local_button_mask();
    });

    match ready_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(Ok(())) => NativeStageStatus {
            state: "ready".into(),
            detail: format!("控制端已就绪，{target_count} 条远端贴边可用于鼠标和键盘切换。"),
        },
        Ok(Err(error)) => NativeStageStatus {
            state: "error".into(),
            detail: error,
        },
        Err(_) => NativeStageStatus {
            state: "error".into(),
            detail: "macOS input capture did not become ready.".into(),
        },
    }
}

#[cfg(target_os = "windows")]
fn start_platform_capture(
    targets: Vec<InputTarget>,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    quic_transport: quic_transport::TransportHandle,
    stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    _main_window_visible: Arc<AtomicBool>,
    main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> NativeStageStatus {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MsgWaitForMultipleObjects, PeekMessageW, SetWindowsHookExW, UnhookWindowsHookEx, MSG,
        PM_REMOVE, QS_ALLINPUT, WH_KEYBOARD_LL, WH_MOUSE_LL,
    };

    let target_count = targets.len();
    let (ready_tx, ready_rx) = mpsc::channel();

    thread::spawn(move || {
        refresh_windows_input_desktop_cache();
        let context = Arc::new(WindowsCaptureContext {
            quic_transport,
            layout_state,
            native_layout,
            active: Mutex::new(None),
            remote_active,
            main_window_focused,
            clipboard_target,
            input_events,
            targets,
            switch_request,
            anchor: Mutex::new(None),
            last_point: Mutex::new(None),
            last_mouse_move_sent: Mutex::new(None),
            remote_button_mask: AtomicU64::new(0),
            pressed_keys: Mutex::new(Vec::new()),
            cursor_hide_calls: Mutex::new(0),
            just_crossed: AtomicBool::new(false),
            local_screen_points: Mutex::new(HashMap::new()),
        });

        if let Ok(mut current) = WINDOWS_CAPTURE_CONTEXT.lock() {
            *current = Some(Arc::clone(&context));
        }

        let mouse_hook = unsafe {
            SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(windows_mouse_proc),
                std::ptr::null_mut(),
                0,
            )
        };
        if mouse_hook.is_null() {
            context.remote_active.store(false, Ordering::Relaxed);
            clear_clipboard_target(&context.clipboard_target);
            clear_windows_capture_context();
            let _ = ready_tx.send(Err("failed to install Windows mouse hook".into()));
            return;
        }

        let keyboard_hook = unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(windows_keyboard_proc),
                std::ptr::null_mut(),
                0,
            )
        };
        if keyboard_hook.is_null() {
            unsafe {
                let _ = UnhookWindowsHookEx(mouse_hook);
            }
            context.remote_active.store(false, Ordering::Relaxed);
            clear_clipboard_target(&context.clipboard_target);
            clear_windows_capture_context();
            let _ = ready_tx.send(Err("failed to install Windows keyboard hook".into()));
            return;
        }

        let _ = ready_tx.send(Ok(()));
        // Watch for fullscreen apps taking over the foreground so sharing can
        // step aside for the duration.
        spawn_fullscreen_watcher(Arc::clone(&stop));
        spawn_app_pause_watcher(Arc::clone(&stop));
        let mut message = MSG::default();
        let mut last_desktop_check = Instant::now() - Duration::from_millis(200);
        while !stop.load(Ordering::Relaxed) {
            if last_desktop_check.elapsed() >= Duration::from_millis(100) {
                last_desktop_check = Instant::now();
                if !refresh_windows_input_desktop_cache() {
                    release_windows_remote_control(&context, true);
                }
            }
            drain_switch_request_windows(&context);
            // Low-level hook callbacks are dispatched only while this thread
            // services its message queue. Blocking on the queue (with a short
            // timeout for the desktop/switch checks above) instead of sleeping
            // 10ms between polls removes up to 10-16ms of added latency per
            // input event — the sleep also quantised to ~15.6ms without a
            // timeBeginPeriod call, batching a 1000Hz mouse into ~64Hz bursts.
            // Slow queue servicing is also what makes Windows silently drop
            // low-level hooks.
            unsafe {
                let _ = MsgWaitForMultipleObjects(0, std::ptr::null(), 0, 20, QS_ALLINPUT);
                while PeekMessageW(&mut message, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {}
            }
        }

        unsafe {
            let _ = UnhookWindowsHookEx(mouse_hook);
            let _ = UnhookWindowsHookEx(keyboard_hook);
        }
        show_windows_cursor_if_needed(&context);
        context.remote_active.store(false, Ordering::Relaxed);
        clear_clipboard_target(&context.clipboard_target);
        clear_windows_capture_context();
        // The hooks are gone, so button-up will never arrive; drop the drag
        // gate rather than leaving it latched across a restart.
        reset_local_button_mask();
    });

    match ready_rx.recv_timeout(Duration::from_secs(1)) {
        Ok(Ok(())) => NativeStageStatus {
            state: "ready".into(),
            detail: format!("控制端已就绪，{target_count} 条远端贴边可用于鼠标和键盘切换。"),
        },
        Ok(Err(error)) => NativeStageStatus {
            state: "error".into(),
            detail: error,
        },
        Err(_) => NativeStageStatus {
            state: "error".into(),
            detail: "Windows input capture did not become ready.".into(),
        },
    }
}

#[cfg(not(target_os = "macos"))]
#[cfg(not(target_os = "windows"))]
fn start_platform_capture(
    _targets: Vec<InputTarget>,
    _layout_state: Arc<Mutex<LayoutState>>,
    _native_layout: LayoutState,
    _quic_transport: quic_transport::TransportHandle,
    _stop: Arc<AtomicBool>,
    remote_active: Arc<AtomicBool>,
    _main_window_visible: Arc<AtomicBool>,
    _main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    _input_events: Arc<AtomicU64>,
    _switch_request: Arc<Mutex<Option<SwitchDirection>>>,
) -> NativeStageStatus {
    remote_active.store(false, Ordering::Relaxed);
    clear_clipboard_target(&clipboard_target);
    unsupported_capture_status()
}

#[cfg(target_os = "windows")]
fn start_platform_receive_monitor(stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        refresh_windows_input_desktop_cache();
        while !stop.load(Ordering::Relaxed) {
            refresh_windows_input_desktop_cache();
            thread::sleep(Duration::from_millis(WINDOWS_DESKTOP_CHECK_INTERVAL_MS));
        }
    });
}

#[cfg(not(target_os = "windows"))]
fn start_platform_receive_monitor(_stop: Arc<AtomicBool>) {}

fn no_target_status(layout: &LayoutState) -> NativeStageStatus {
    let remote_count = layout
        .devices
        .iter()
        .filter(|device| device.role != "local")
        .count();
    let online_remote_count = layout
        .devices
        .iter()
        .filter(|device| device.role != "local" && device.online)
        .count();
    let detail = if remote_count == 0 {
        "控制模式已开启，但布局里还没有远端设备。先让对方电脑运行 mykvm，再在 LAN devices 里 Scan 并 Add。"
    } else if online_remote_count == 0 {
        "控制模式已开启，但远端设备都被标记为离线。把要控制的设备切回 online 后再启动运行时。"
    } else {
        "控制模式已开启，且已有在线远端设备，但屏幕还没有和本机贴边。拖动远端显示器贴住本机边缘后才会生成切屏目标。"
    };

    NativeStageStatus {
        state: "idle".into(),
        detail: detail.into(),
    }
}

fn receive_only_status() -> NativeStageStatus {
    NativeStageStatus {
        state: "idle".into(),
        detail: "当前是仅接收模式：会接收远端输入，但不会捕获本机鼠标和键盘。".into(),
    }
}

fn unsupported_capture_status() -> NativeStageStatus {
    NativeStageStatus {
        state: "stubbed".into(),
        detail: "Global input capture is not implemented on this platform.".into(),
    }
}

fn build_input_targets(layout: &LayoutState, native_layout: &LayoutState) -> Vec<InputTarget> {
    let Some(local_device) = layout.devices.iter().find(|device| device.role == "local") else {
        return Vec::new();
    };
    let native_device = native_layout
        .devices
        .iter()
        .find(|device| device.role == "local")
        .or_else(|| native_layout.devices.first());

    let local_screens = &local_device.screens;
    let origin_device_id = crate::local_peer_from_layout(layout).id;
    let mut targets = Vec::new();

    for device in layout.devices.iter().filter(|device| {
        device.role != "local"
            && device.online
            && device.input_ready
            && device.protocol_version == quic_transport::PROTOCOL_VERSION
            && !device.transport_public_key.trim().is_empty()
    }) {
        let quic_port = normalize_quic_port(device.transport_port, device.quic_port);
        for layout_local_screen in local_screens {
            let native_local_screen = native_device
                .and_then(|device| {
                    device
                        .screens
                        .iter()
                        .find(|screen| screen.id == layout_local_screen.id)
                })
                .unwrap_or(layout_local_screen);
            let native_local_screen = platform_native_screen(native_local_screen);

            for remote_screen in &device.screens {
                if screens_overlap(layout_local_screen, remote_screen) {
                    continue;
                }

                if let Some(edge) = touching_edge(layout_local_screen, remote_screen) {
                    targets.push(InputTarget {
                        device_id: device.id.clone(),
                        origin_device_id: origin_device_id.clone(),
                        cluster_id: layout.cluster_id.clone(),
                        pair_secret: layout.pair_secret.clone(),
                        target_addr: format!("{}:{}", device.host, quic_port),
                        target_platform: device.platform.clone(),
                        transport_public_key: device.transport_public_key.clone(),
                        protocol_version: device.protocol_version,
                        screen_id: peer_screen_id(device, remote_screen),
                        local_screen: native_local_screen.clone(),
                        layout_local_screen: layout_local_screen.clone(),
                        remote_screen: remote_screen.clone(),
                        edge,
                    });
                }
            }
        }
    }

    targets
}

/// How long a built target list may be served from cache. Crossing geometry
/// and pairing context only change on layout edits and peer presence flips;
/// a quarter second of staleness is invisible at a screen edge, while
/// rebuilding per mouse move cost a blocking layout lock, a LanPeer build
/// (UDP-socket IP probe) and a dozen String clones inside the event tap.
const INPUT_TARGETS_TTL: Duration = Duration::from_millis(250);

static INPUT_TARGETS_CACHE: Mutex<Option<(Instant, Arc<Vec<InputTarget>>)>> = Mutex::new(None);

fn current_input_targets(
    layout_state: &Arc<Mutex<LayoutState>>,
    native_layout: &LayoutState,
) -> Arc<Vec<InputTarget>> {
    if let Ok(cache) = INPUT_TARGETS_CACHE.lock() {
        if let Some((built_at, targets)) = cache.as_ref() {
            if built_at.elapsed() < INPUT_TARGETS_TTL {
                return Arc::clone(targets);
            }
        }
    }

    // Never block the event tap on a held layout lock (a save may be writing
    // to disk under it): serve the stale cache instead and retry next event.
    match layout_state.try_lock() {
        Ok(layout) => {
            let targets = Arc::new(build_input_targets(&layout, native_layout));
            drop(layout);
            if let Ok(mut cache) = INPUT_TARGETS_CACHE.lock() {
                *cache = Some((Instant::now(), Arc::clone(&targets)));
            }
            targets
        }
        Err(_) => INPUT_TARGETS_CACHE
            .lock()
            .ok()
            .and_then(|cache| cache.as_ref().map(|(_, targets)| Arc::clone(targets)))
            .unwrap_or_default(),
    }
}

/// Drops the cached target list so the next event rebuilds it — called when
/// capture starts, so a fresh session can never act on the previous
/// session's pairing context.
fn invalidate_input_targets_cache() {
    if let Ok(mut cache) = INPUT_TARGETS_CACHE.lock() {
        *cache = None;
    }
}

fn touching_edge(local: &Screen, remote: &Screen) -> Option<Edge> {
    if near(local.x + local.width, remote.x)
        && ranges_overlap(
            local.y,
            local.y + local.height,
            remote.y,
            remote.y + remote.height,
        )
    {
        return Some(Edge::Right);
    }

    if near(local.x, remote.x + remote.width)
        && ranges_overlap(
            local.y,
            local.y + local.height,
            remote.y,
            remote.y + remote.height,
        )
    {
        return Some(Edge::Left);
    }

    if near(local.y + local.height, remote.y)
        && ranges_overlap(
            local.x,
            local.x + local.width,
            remote.x,
            remote.x + remote.width,
        )
    {
        return Some(Edge::Bottom);
    }

    if near(local.y, remote.y + remote.height)
        && ranges_overlap(
            local.x,
            local.x + local.width,
            remote.x,
            remote.x + remote.width,
        )
    {
        return Some(Edge::Top);
    }

    None
}

fn screens_overlap(local: &Screen, remote: &Screen) -> bool {
    local.x < remote.x + remote.width
        && local.x + local.width > remote.x
        && local.y < remote.y + remote.height
        && local.y + local.height > remote.y
}

fn near(a: i32, b: i32) -> bool {
    (a - b).abs() <= EDGE_TOLERANCE
}

fn ranges_overlap(a_start: i32, a_end: i32, b_start: i32, b_end: i32) -> bool {
    i32::min(a_end, b_end) - i32::max(a_start, b_start) > EDGE_TOLERANCE
}

fn peer_screen_id(device: &Device, screen: &Screen) -> String {
    screen
        .id
        .strip_prefix(&format!("{}-", device.id))
        .unwrap_or(&screen.id)
        .to_string()
}

fn send_packet(
    quic_transport: &quic_transport::TransportHandle,
    target: &InputTarget,
    event: InputEvent,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) -> bool {
    let is_key_event = matches!(event, InputEvent::Key { .. });
    let mut packet_context = input_packet_context(target, event, layout_state);
    let Some(peer) = packet_context.peer.take() else {
        return false;
    };
    // Attach the full pairing credentials only ~once per INPUT_FULL_CRED_REFRESH
    // per destination; steady-state packets omit them (empty -> skipped on the
    // wire) and the receiver authorizes them from its per-source cache. This is
    // what actually shrinks the datagram; the borrowed mirror above only kept
    // the omitted-string case allocation-free.
    let include_credentials = should_send_full_input_credentials(&peer.addr);
    let packet = InputPacketRef {
        protocol: INPUT_PROTOCOL,
        target_device_id: &target.device_id,
        origin_device_id: if include_credentials {
            &packet_context.origin_device_id
        } else {
            ""
        },
        origin_port: quic_transport.port(),
        origin_transport_public_key: if include_credentials {
            quic_transport.public_key()
        } else {
            ""
        },
        origin_protocol_version: quic_transport::PROTOCOL_VERSION,
        cluster_id: if include_credentials {
            &packet_context.cluster_id
        } else {
            ""
        },
        pair_secret: if include_credentials {
            &packet_context.pair_secret
        } else {
            ""
        },
        event: &packet_context.event,
    };

    let payload = match rmp_serde::to_vec_named(&packet) {
        Ok(payload) => payload,
        Err(error) => {
            log::warn!(
                "input tx encode failed target={} error={}",
                peer.addr,
                error
            );
            return false;
        }
    };

    // #9: keyboard events go over a reliable QUIC *stream* (acknowledged) so
    // keystrokes are never dropped — datagrams are best-effort and used to
    // silently lose keys under load, and the brief "Connecting" window used to
    // drop datagrams entirely. Mouse moves / scroll stay on datagrams
    // (latest-wins, latency-critical). If the stream send fails for any reason
    // we fall back to the datagram path so a key is still attempted.
    let sent = if is_key_event {
        match quic_transport.send_stream_expect_ack(peer.clone(), payload.clone()) {
            Ok(()) => true,
            Err(_) => quic_transport.send_datagram(peer, payload).is_ok(),
        }
    } else {
        quic_transport.send_datagram(peer, payload).is_ok()
    };
    if sent {
        input_events.fetch_add(1, Ordering::Relaxed);
    } else {
        mark_target_offline(layout_state, target, "input send failed");
    }
    sent
}

pub fn send_secure_attention_control(
    layout: &LayoutState,
    quic_transport: &quic_transport::TransportHandle,
    device_id: &str,
) -> Result<(), String> {
    let Some(target) = layout
        .devices
        .iter()
        .find(|device| device.id == device_id && device.role != "local")
    else {
        return Err("target device is not in the layout".into());
    };
    if target.platform != "windows" {
        return Err("Ctrl+Alt+Del control is only available for Windows targets.".into());
    }
    if !target.online || !target.input_ready {
        return Err("target device is not online and input-ready".into());
    }
    if target.transport_public_key.trim().is_empty() {
        return Err("target device has no QUIC transport key; re-pair it first".into());
    }
    if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        return Err("this device is not paired with the target".into());
    }

    let origin_device_id = origin_peer_id(layout);
    let packet = InputControlPacket {
        protocol: INPUT_CONTROL_PROTOCOL.into(),
        target_device_id: target.id.clone(),
        origin_device_id,
        origin_transport_public_key: quic_transport.public_key().to_string(),
        origin_protocol_version: quic_transport::PROTOCOL_VERSION,
        cluster_id: layout.cluster_id.clone(),
        pair_secret: layout.pair_secret.clone(),
        command: InputControlCommand::SecureAttention,
    };
    let payload = rmp_serde::to_vec_named(&packet)
        .map_err(|error| format!("encode input control packet: {error}"))?;
    let peer = quic_transport.peer(
        format!(
            "{}:{}",
            target.host,
            normalize_quic_port(target.transport_port, target.quic_port)
        ),
        target.transport_public_key.clone(),
        target.protocol_version,
    );

    quic_transport.send_datagram(peer, payload)
}

struct InputPacketContext {
    origin_device_id: String,
    cluster_id: String,
    pair_secret: String,
    peer: Option<quic_transport::PeerEndpoint>,
    event: InputEvent,
}

fn input_packet_context(
    target: &InputTarget,
    event: InputEvent,
    layout_state: &Arc<Mutex<LayoutState>>,
) -> InputPacketContext {
    let fallback_peer = || quic_transport::PeerEndpoint {
        addr: target.target_addr.clone(),
        public_key: target.transport_public_key.clone(),
        protocol_version: target.protocol_version,
    };

    let fallback_context = |event| InputPacketContext {
        origin_device_id: target.origin_device_id.clone(),
        cluster_id: target.cluster_id.clone(),
        pair_secret: target.pair_secret.clone(),
        peer: Some(fallback_peer()),
        event,
    };

    // Mouse events — essentially every packet — always use the context cached
    // on the target at build time: it is at most INPUT_TARGETS_TTL stale and
    // needs no layout lock, no origin re-derivation and no address formatting.
    // Key events still consult the live layout for the modifier remap; they
    // arrive at typing rate, not at mouse rate.
    if !matches!(event, InputEvent::Key { .. }) {
        return fallback_context(event);
    }

    let layout = match layout_state.try_lock() {
        Ok(layout) => layout,
        Err(TryLockError::WouldBlock) => return fallback_context(event),
        Err(TryLockError::Poisoned(_)) => return fallback_context(event),
    };

    let origin_device_id = origin_peer_id(&layout);
    let peer = layout
        .devices
        .iter()
        .find(|device| device.id == target.device_id)
        .and_then(|device| {
            (device.online && device.input_ready).then(|| quic_transport::PeerEndpoint {
                addr: format!(
                    "{}:{}",
                    device.host,
                    normalize_quic_port(device.transport_port, device.quic_port)
                ),
                public_key: device.transport_public_key.clone(),
                protocol_version: device.protocol_version,
            })
        });
    let event = remap_event_for_target_layout(event, target, &layout);

    InputPacketContext {
        origin_device_id,
        cluster_id: layout.cluster_id.clone(),
        pair_secret: layout.pair_secret.clone(),
        peer,
        event,
    }
}

/// Rewrites modifier keys on key events when the controlling machine and the
/// target run different operating systems, so platform shortcut conventions
/// line up (default: Ctrl <-> Cmd). Non-key events and same-platform targets
/// pass through untouched. The wire format is always Windows virtual-key codes.
fn remap_event_for_target_layout(
    event: InputEvent,
    target: &InputTarget,
    layout: &LayoutState,
) -> InputEvent {
    let InputEvent::Key { key_code, down } = event else {
        return event;
    };

    let target_platform = target.target_platform.as_str();
    if target_platform != "macos" && target_platform != "windows" {
        return InputEvent::Key { key_code, down };
    }
    if target_platform == crate::current_platform() {
        return InputEvent::Key { key_code, down };
    }

    let remapped = if layout.modifier_remap {
        remap_modifier_vk(
            key_code,
            &layout.modifier_map.control,
            &layout.modifier_map.alt,
            &layout.modifier_map.meta,
        )
    } else {
        key_code
    };

    InputEvent::Key {
        key_code: remapped,
        down,
    }
}

#[cfg(test)]
fn remap_event_for_target(
    event: InputEvent,
    target: &InputTarget,
    layout_state: &Arc<Mutex<LayoutState>>,
) -> InputEvent {
    match layout_state.lock() {
        Ok(layout) => remap_event_for_target_layout(event, target, &layout),
        Err(_) => event,
    }
}

/// Classifies a Windows virtual-key code into a logical modifier group:
/// 0 = Control, 1 = Alt, 2 = Meta (Windows key / macOS Command).
fn classify_modifier_vk(vk: u16) -> Option<u8> {
    match vk {
        0x11 | 0xA2 | 0xA3 => Some(0),
        0x12 | 0xA4 | 0xA5 => Some(1),
        0x5B | 0x5C => Some(2),
        _ => None,
    }
}

/// Resolves a configured logical target to its canonical Windows virtual-key
/// code. "same" (or any unknown value) returns None so the original key, with
/// its left/right distinction, is preserved.
fn logical_target_vk(target: &str) -> Option<u16> {
    match target {
        "control" => Some(0x11),
        "alt" => Some(0x12),
        "meta" => Some(0x5B),
        _ => None,
    }
}

fn remap_modifier_vk(vk: u16, control: &str, alt: &str, meta: &str) -> u16 {
    let target = match classify_modifier_vk(vk) {
        Some(0) => control,
        Some(1) => alt,
        Some(2) => meta,
        _ => return vk,
    };
    logical_target_vk(target).unwrap_or(vk)
}

fn mark_target_offline(
    layout_state: &Arc<Mutex<LayoutState>>,
    target: &InputTarget,
    _reason: &str,
) {
    let Ok(mut layout) = layout_state.lock() else {
        return;
    };
    let Some(device) = layout
        .devices
        .iter_mut()
        .find(|device| device.id == target.device_id)
    else {
        return;
    };
    if !device.online {
        return;
    }

    device.online = false;
}

/// Handles one received input-plane datagram end to end. Structured for the
/// per-move cost, not readability of the rare cases:
/// - decode runs entirely OUTSIDE the layout lock, and input packets decode
///   first (they outnumber control packets by orders of magnitude; the old
///   control-first order fully parsed every mouse move against the wrong
///   schema before trying the right one)
/// - the layout lock covers only authorization + coordinate mapping;
///   injection is syscalls and runs after the lock is released, so a slow
///   WindowServer round-trip can no longer stretch the critical section every
///   other hot path contends on
pub fn handle_input_datagram(
    layout_state: &Arc<Mutex<LayoutState>>,
    native_layout: &LayoutState,
    payload: &[u8],
    source: SocketAddr,
    input_events: &Arc<AtomicU64>,
    clipboard_target: &Arc<Mutex<Option<ClipboardTarget>>>,
) -> bool {
    if let Some(packet) = decode_input_packet(payload) {
        if packet.protocol != INPUT_PROTOCOL {
            return false;
        }
        // Steady-state datagrams omit the pairing block (it rides ~once per
        // INPUT_FULL_CRED_REFRESH). A credentialled packet is authorized in
        // full and (re)authorizes this source; a credential-less one is
        // admitted only while that authorization is still fresh, so a peer that
        // never proved the pairing secret from this address can never inject.
        let carries_credentials = !packet.pair_secret.trim().is_empty();
        let command = {
            let Ok(layout) = layout_state.lock() else {
                return false;
            };
            if carries_credentials {
                if !packet_authorized(&layout, &packet) {
                    warn_unauthorized_packet(&layout, &packet);
                    return true;
                }
                cache_authorized_input_origin(source);
            } else if !input_origin_recently_authorized(source) {
                return true;
            }
            let local_peer_id = cached_local_peer_id(&layout);
            if !packet_targets_local(&layout, &packet.target_device_id, &local_peer_id) {
                return true;
            }
            // No-op for credential-less packets (empty key); the clipboard
            // target was set by the last credentialled packet and persists.
            refresh_clipboard_target(clipboard_target, &layout, &packet, source);
            input_event_to_command(&layout, native_layout, packet.event)
        };
        let Some(command) = command else {
            return true;
        };
        if dispatch_input_command(command) {
            input_events.fetch_add(1, Ordering::Relaxed);
        }
        return true;
    }

    if let Some(packet) = decode_input_control_packet(payload) {
        let Ok(layout) = layout_state.lock() else {
            return false;
        };
        let local_peer_id = cached_local_peer_id(&layout);
        return handle_control_packet(&layout, packet, source, &local_peer_id);
    }

    false
}

/// The local peer id is derived from the hostname + LAN address and is needed
/// for every received packet; deriving it builds a full LanPeer (screens and
/// all). Cache the id briefly instead of rebuilding it per datagram.
fn cached_local_peer_id(layout: &LayoutState) -> String {
    const LOCAL_PEER_ID_TTL: Duration = Duration::from_secs(5);
    static CACHE: Mutex<Option<(Instant, String)>> = Mutex::new(None);

    if let Ok(mut cached) = CACHE.lock() {
        if let Some((resolved_at, id)) = cached.as_ref() {
            if resolved_at.elapsed() < LOCAL_PEER_ID_TTL {
                return id.clone();
            }
        }
        let id = crate::local_peer_from_layout(layout).id;
        *cached = Some((Instant::now(), id.clone()));
        return id;
    }
    crate::local_peer_from_layout(layout).id
}

/// Persist the controller as our clipboard peer so a copy made on this
/// machine syncs back to it immediately, without needing the remote cursor to
/// re-enter. The target only actually changes on the first packet of a
/// session (or a controller switch); skip the five per-packet allocations the
/// unconditional rewrite used to pay on every mouse move.
fn refresh_clipboard_target(
    clipboard_target: &Arc<Mutex<Option<ClipboardTarget>>>,
    layout: &LayoutState,
    packet: &InputPacket,
    source: SocketAddr,
) {
    if packet.origin_port == 0 || packet.origin_transport_public_key.trim().is_empty() {
        return;
    }
    if !packet.origin_device_id.trim().is_empty() {
        if let Ok(target) = clipboard_target.lock() {
            if target.as_ref().is_some_and(|target| {
                target.device_id == packet.origin_device_id
                    && target.transport_public_key == packet.origin_transport_public_key
                    && target.protocol_version == packet.origin_protocol_version
            }) {
                return;
            }
        }
    }
    let device_id = if packet.origin_device_id.trim().is_empty() {
        source.ip().to_string()
    } else {
        packet.origin_device_id.clone()
    };
    set_clipboard_target(
        clipboard_target,
        device_id,
        format!("{}:{}", source.ip(), packet.origin_port),
        packet.origin_transport_public_key.clone(),
        packet.origin_protocol_version,
        layout.cluster_id.clone(),
        layout.pair_secret.clone(),
        None,
    );
}

fn handle_control_packet(
    layout: &LayoutState,
    packet: InputControlPacket,
    source: SocketAddr,
    local_peer_id: &str,
) -> bool {
    if packet.protocol != INPUT_CONTROL_PROTOCOL {
        return false;
    }

    if !control_packet_authorized(layout, &packet) {
        warn_unauthorized_control_packet(layout, &packet);
        return true;
    }

    if !packet_targets_local(layout, &packet.target_device_id, local_peer_id) {
        return true;
    }

    match packet.command {
        InputControlCommand::SecureAttention => {
            #[cfg(target_os = "windows")]
            if let Err(error) = send_secure_attention_to_helper() {
                log::warn!(
                    "SecureAttention control from {} could not reach input service: {}",
                    source,
                    error
                );
            }

            #[cfg(not(target_os = "windows"))]
            log::warn!(
                "SecureAttention control from {} ignored on non-Windows target",
                source
            );
        }
    }

    true
}

fn packet_authorized(layout: &LayoutState, packet: &InputPacket) -> bool {
    packet_authorized_fields(
        layout,
        &packet.cluster_id,
        &packet.pair_secret,
        &packet.origin_transport_public_key,
        &packet.origin_device_id,
    )
}

fn control_packet_authorized(layout: &LayoutState, packet: &InputControlPacket) -> bool {
    packet_authorized_fields(
        layout,
        &packet.cluster_id,
        &packet.pair_secret,
        &packet.origin_transport_public_key,
        &packet.origin_device_id,
    )
}

fn packet_authorized_fields(
    layout: &LayoutState,
    cluster_id: &str,
    pair_secret: &str,
    origin_transport_public_key: &str,
    origin_device_id: &str,
) -> bool {
    if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        return false;
    }
    if cluster_id != layout.cluster_id || pair_secret != layout.pair_secret {
        return false;
    }

    if layout.paired_controllers.iter().any(|controller| {
        (!origin_transport_public_key.trim().is_empty()
            && controller.transport_public_key == origin_transport_public_key)
            || (!origin_device_id.trim().is_empty() && controller.id == origin_device_id)
    }) {
        return true;
    }

    legacy_local_device_origin_allowed(layout, origin_device_id, origin_transport_public_key)
}

fn legacy_local_device_origin_allowed(
    layout: &LayoutState,
    origin_device_id: &str,
    origin_transport_public_key: &str,
) -> bool {
    layout.machine_role == "client"
        && layout.paired_controllers.len() == 1
        && origin_device_id == "local-device"
        && !origin_transport_public_key.trim().is_empty()
}

fn origin_peer_id(layout: &LayoutState) -> String {
    crate::local_peer_from_layout(layout).id
}

static LAST_UNAUTHORIZED_WARN: OnceLock<Mutex<Instant>> = OnceLock::new();

/// Log (at most once every few seconds, since a single mouse move floods many
/// packets) why a controller's input was rejected. Without this the packets
/// were dropped silently while the device still showed "online", which makes a
/// pairing-credential mismatch impossible to diagnose — exactly the "shows
/// online but the cursor can't cross" trap.
fn warn_unauthorized_packet(layout: &LayoutState, packet: &InputPacket) {
    let reason = if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        "this device has no pairing configured (empty cluster/secret) — pair it with the controller"
    } else if packet.cluster_id != layout.cluster_id || packet.pair_secret != layout.pair_secret {
        "pairing secret/cluster mismatch — controller and this device are not paired with the same credentials; re-pair them (removing/re-adding the device does NOT re-pair)"
    } else {
        "controller is not in this device's paired-controllers list (likely a rotated transport key) — re-pair"
    };

    let cell =
        LAST_UNAUTHORIZED_WARN.get_or_init(|| Mutex::new(Instant::now() - Duration::from_secs(60)));
    if let Ok(mut last) = cell.lock() {
        if last.elapsed() < Duration::from_secs(3) {
            return;
        }
        *last = Instant::now();
    }

    log::warn!(
        "rejected input from controller id={} key={}: {}",
        if packet.origin_device_id.trim().is_empty() {
            "<none>"
        } else {
            packet.origin_device_id.as_str()
        },
        if packet.origin_transport_public_key.trim().is_empty() {
            "<none>"
        } else {
            "<set>"
        },
        reason
    );
}

fn warn_unauthorized_control_packet(layout: &LayoutState, packet: &InputControlPacket) {
    let reason = if layout.cluster_id.trim().is_empty() || layout.pair_secret.trim().is_empty() {
        "this device has no pairing configured"
    } else if packet.cluster_id != layout.cluster_id || packet.pair_secret != layout.pair_secret {
        "pairing secret/cluster mismatch"
    } else {
        "controller is not in this device's paired-controllers list"
    };

    log::warn!(
        "rejected input control from controller id={} key={}: {}",
        if packet.origin_device_id.trim().is_empty() {
            "<none>"
        } else {
            packet.origin_device_id.as_str()
        },
        if packet.origin_transport_public_key.trim().is_empty() {
            "<none>"
        } else {
            "<set>"
        },
        reason
    );
}

fn packet_targets_local(layout: &LayoutState, target_device_id: &str, local_peer_id: &str) -> bool {
    if target_device_id.trim().is_empty() {
        return true;
    }
    if target_device_id == local_peer_id {
        return true;
    }

    layout
        .devices
        .iter()
        .any(|device| device.role == "local" && device.id == target_device_id)
}

fn decode_input_packet(payload: &[u8]) -> Option<InputPacket> {
    rmp_serde::from_slice::<InputPacket>(payload).ok()
}

fn decode_input_control_packet(payload: &[u8]) -> Option<InputControlPacket> {
    rmp_serde::from_slice::<InputControlPacket>(payload).ok()
}

fn default_protocol_version() -> u16 {
    quic_transport::PROTOCOL_VERSION
}

fn normalize_quic_port(transport_port: u16, quic_port: u16) -> u16 {
    if quic_port == 0 {
        transport_port
    } else {
        quic_port
    }
}

fn local_device(layout: &LayoutState) -> Option<&Device> {
    layout
        .devices
        .iter()
        .find(|device| device.role == "local")
        .or_else(|| layout.devices.first())
}

fn local_screen_for_event<'a>(layout: &'a LayoutState, screen_id: &str) -> Option<&'a Screen> {
    let device = local_device(layout)?;
    device
        .screens
        .iter()
        .find(|screen| screen.id == screen_id)
        .or_else(|| device.screens.iter().find(|screen| screen.is_primary))
        .or_else(|| device.screens.first())
}

fn map_relative_to_native_axis(
    relative: i32,
    logical_size: i32,
    native_start: i32,
    native_size: i32,
) -> i32 {
    let ratio = relative as f64 / logical_size.max(1) as f64;
    (native_start as f64 + ratio * native_size.max(1) as f64).round() as i32
}

#[cfg(target_os = "windows")]
fn platform_native_screen(screen: &Screen) -> Screen {
    let scale = if screen.scale.is_finite() && screen.scale > 0.0 {
        screen.scale
    } else {
        1.0
    };

    Screen {
        x: scale_position(screen.x, scale),
        y: scale_position(screen.y, scale),
        width: scale_size(screen.width, scale),
        height: scale_size(screen.height, scale),
        ..screen.clone()
    }
}

#[cfg(not(target_os = "windows"))]
fn platform_native_screen(screen: &Screen) -> Screen {
    screen.clone()
}

#[cfg(target_os = "windows")]
fn scale_position(value: i32, scale: f64) -> i32 {
    (value as f64 * scale)
        .round()
        .clamp(i32::MIN as f64, i32::MAX as f64) as i32
}

#[cfg(target_os = "windows")]
fn scale_size(value: i32, scale: f64) -> i32 {
    (value.max(1) as f64 * scale)
        .round()
        .clamp(1.0, i32::MAX as f64) as i32
}

fn pack_remote_position(x: i32, y: i32) -> i64 {
    ((x as i64) << 32) | (y as u32 as i64)
}

fn unpack_remote_position(packed: i64) -> (i32, i32) {
    ((packed >> 32) as i32, packed as u32 as i32)
}

fn update_remote_mouse_position(x: i32, y: i32) -> Option<MouseButton> {
    REMOTE_MOUSE_POSITION.store(pack_remote_position(x, y), Ordering::Relaxed);
    button_from_mask(REMOTE_MOUSE_BUTTONS.load(Ordering::Relaxed))
}

fn update_remote_mouse_button(button: MouseButton, down: bool) -> (i32, i32) {
    if down {
        REMOTE_MOUSE_BUTTONS.fetch_or(mouse_button_mask(button), Ordering::Relaxed);
    } else {
        REMOTE_MOUSE_BUTTONS.fetch_and(!mouse_button_mask(button), Ordering::Relaxed);
    }
    unpack_remote_position(REMOTE_MOUSE_POSITION.load(Ordering::Relaxed))
}

/// Executes a mapped input command on this machine. Runs outside the layout
/// lock — injection is syscalls and must not stretch the critical section.
fn dispatch_input_command(command: InputCommand) -> bool {
    #[cfg(target_os = "windows")]
    {
        // Inject locally on the normal desktop; hand off to the privileged SYSTEM
        // helper only for the secure desktop (lock screen / UAC) or Ctrl+Alt+Del.
        //
        // The helper is REQUIRED on the secure desktop — the user-mode app has no
        // access to the Winlogon desktop — but it must NOT be used on the normal
        // desktop: the helper's worker runs as SYSTEM, and Windows rejects a
        // SYSTEM-integrity process's synthetic button/key events with
        // ERROR_ACCESS_DENIED when the foreground window is a normal
        // Medium-integrity app (cursor MOVE still lands because it only
        // repositions the window-station-global cursor). That is the "cursor
        // slides but can't click or type" symptom. Local injection runs as the
        // logged-in user at the foreground window's own integrity, so it clicks
        // and types normally. On the secure desktop the foreground is LogonUI
        // (System integrity), so the worker's equal-integrity injection works.
        if should_route_to_windows_helper(&command) {
            match windows_pipe_dispatcher().send(&command) {
                Ok(()) => return true,
                Err(error) => note_windows_helper_unavailable(&error),
            }
        }
        inject_input_command(command);
        return true;
    }

    #[cfg(not(target_os = "windows"))]
    {
        inject_input_command(command);
        true
    }
}

/// Logs (at most once every 10s, since a single mouse move floods many packets)
/// that the privileged input helper could not be reached, so injection fell back
/// to the user-mode path. On the normal desktop the local fallback works; on the
/// secure desktop (lock screen / UAC) it cannot deliver clicks or keystrokes, so
/// this is the breadcrumb that explains a dead lock screen.
#[cfg(target_os = "windows")]
fn note_windows_helper_unavailable(error: &str) {
    static LAST_WARN: OnceLock<Mutex<Instant>> = OnceLock::new();
    let cell = LAST_WARN.get_or_init(|| Mutex::new(Instant::now() - Duration::from_secs(60)));
    if let Ok(mut last) = cell.lock() {
        if last.elapsed() < Duration::from_secs(10) {
            return;
        }
        *last = Instant::now();
    }
    log::info!(
        "input helper unavailable ({error}); injecting locally. Lock-screen / UAC \
         input needs the MyKVM input service — install it from Settings if clicks \
         and keys stop working while the screen is locked."
    );
}

#[cfg(target_os = "windows")]
fn should_route_to_windows_helper(command: &InputCommand) -> bool {
    // SecureAttention (Ctrl+Alt+Del) always needs the privileged helper —
    // SendSAS requires SYSTEM context and cannot be issued from the user app.
    if matches!(command, InputCommand::SecureAttention) {
        return true;
    }
    // Otherwise only the secure desktop (lock screen / UAC) needs the helper.
    // On the normal "Default" desktop we inject locally as the logged-in user,
    // which is the only path that can click/type into Medium-integrity windows
    // (the SYSTEM helper is denied there with ERROR_ACCESS_DENIED).
    !windows_inject_desktop_is_default()
}

/// Cached check of whether the current input desktop is "Default", for the
/// inject path. Probing `OpenInputDesktop` from the mouse/datagram hot path is
/// expensive enough to show up as periodic dropped frames, so capture/receive
/// monitor threads refresh this cache out of band.
#[cfg(target_os = "windows")]
fn windows_inject_desktop_is_default() -> bool {
    cached_windows_input_desktop_is_default()
}

fn input_event_to_command(
    layout: &LayoutState,
    native_layout: &LayoutState,
    event: InputEvent,
) -> Option<InputCommand> {
    match event {
        InputEvent::MouseMove { screen_id, x, y } => {
            if let Some(screen) = local_screen_for_event(layout, &screen_id) {
                let native_screen = local_screen_for_event(native_layout, &screen_id)
                    .map(platform_native_screen)
                    .unwrap_or_else(|| platform_native_screen(screen));
                let absolute_x = map_relative_to_native_axis(
                    x,
                    screen.width,
                    native_screen.x,
                    native_screen.width,
                );
                let absolute_y = map_relative_to_native_axis(
                    y,
                    screen.height,
                    native_screen.y,
                    native_screen.height,
                );
                let drag_button = update_remote_mouse_position(absolute_x, absolute_y);
                return Some(InputCommand::MouseMove {
                    x: absolute_x,
                    y: absolute_y,
                    drag_button,
                });
            }
            None
        }
        InputEvent::MouseButton { button, down } => {
            let (x, y) = update_remote_mouse_button(button, down);
            Some(InputCommand::MouseButton { button, down, x, y })
        }
        InputEvent::Scroll { delta_x, delta_y } => Some(InputCommand::Scroll { delta_x, delta_y }),
        InputEvent::Key { key_code, down } => Some(InputCommand::Key { key_code, down }),
    }
}

fn inject_input_command(command: InputCommand) {
    // Mark this Mac as actively being driven by a remote peer. On macOS this
    // folds into the capture loop's App Nap decision so the backgrounded
    // injection task is scheduled normally instead of being throttled — the
    // root cause of the "first Win->Mac move after a pause is delayed/stuttery"
    // symptom. Receiver-only: local capture on macOS never calls this path.
    #[cfg(target_os = "macos")]
    macos_receiver_activity::mark_remote_input();
    match command {
        InputCommand::MouseMove { x, y, drag_button } => inject_mouse_move(x, y, drag_button),
        InputCommand::MouseButton { button, down, x, y } => inject_mouse_button(button, down, x, y),
        InputCommand::Scroll { delta_x, delta_y } => inject_scroll(delta_x, delta_y),
        InputCommand::Key { key_code, down } => inject_key(key_code, down),
        InputCommand::ReleaseAll | InputCommand::SecureAttention => {}
    }
}

#[cfg(target_os = "windows")]
fn windows_pipe_dispatcher() -> &'static WindowsInputDispatcher {
    static DISPATCHER: OnceLock<WindowsInputDispatcher> = OnceLock::new();
    DISPATCHER.get_or_init(WindowsInputDispatcher::new)
}

#[cfg(target_os = "windows")]
pub fn windows_input_pipe_available() -> bool {
    open_current_session_input_pipe().is_ok()
}

#[cfg(not(target_os = "windows"))]
pub fn windows_input_pipe_available() -> bool {
    false
}

#[cfg(target_os = "windows")]
pub fn send_secure_attention_to_helper() -> Result<(), String> {
    windows_pipe_dispatcher().send(&InputCommand::SecureAttention)
}

#[cfg(not(target_os = "windows"))]
pub fn send_secure_attention_to_helper() -> Result<(), String> {
    Err("Secure Attention Sequence is only available through the Windows input service.".into())
}

#[cfg(target_os = "windows")]
struct WindowsInputDispatcher {
    pipe: Mutex<Option<std::fs::File>>,
    retry_after: Mutex<Instant>,
}

#[cfg(target_os = "windows")]
impl WindowsInputDispatcher {
    fn new() -> Self {
        Self {
            pipe: Mutex::new(None),
            retry_after: Mutex::new(Instant::now()),
        }
    }

    fn send(&self, command: &InputCommand) -> Result<(), String> {
        use std::io::Write;

        let framed = crate::shared_input::encode_input_command(command)?;
        let mut pipe_guard = self
            .pipe
            .lock()
            .map_err(|_| "input helper pipe lock poisoned".to_string())?;

        if pipe_guard.is_none() {
            *pipe_guard = Some(self.open_pipe_with_backoff()?);
        }

        let Some(pipe) = pipe_guard.as_mut() else {
            return Err("input helper pipe unavailable".into());
        };

        if let Err(error) = pipe.write_all(&framed).and_then(|_| pipe.flush()) {
            *pipe_guard = None;
            return Err(format!("write input helper pipe: {error}"));
        }

        Ok(())
    }

    fn open_pipe_with_backoff(&self) -> Result<std::fs::File, String> {
        let now = Instant::now();
        {
            let retry_after = self
                .retry_after
                .lock()
                .map_err(|_| "input helper retry lock poisoned".to_string())?;
            if now < *retry_after {
                return Err("input helper pipe retry is cooling down".into());
            }
        }

        match open_current_session_input_pipe() {
            Ok(file) => Ok(file),
            Err(error) => {
                if let Ok(mut retry_after) = self.retry_after.lock() {
                    *retry_after = Instant::now() + Duration::from_secs(1);
                }
                Err(error)
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn open_current_session_input_pipe() -> Result<std::fs::File, String> {
    use std::fs::OpenOptions;

    let session_id = current_windows_session_id()?;

    let pipe_name = crate::shared_input::input_pipe_name(session_id);
    OpenOptions::new()
        .write(true)
        .open(&pipe_name)
        .map_err(|error| format!("open input helper pipe {pipe_name}: {error}"))
}

#[cfg(target_os = "windows")]
fn current_windows_session_id() -> Result<u32, String> {
    use windows_sys::Win32::System::{
        RemoteDesktop::ProcessIdToSessionId, Threading::GetCurrentProcessId,
    };

    let mut session_id = 0_u32;
    let ok = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session_id) } != 0;
    if ok {
        Ok(session_id)
    } else {
        Err("failed to resolve current Windows session id".into())
    }
}

#[cfg(target_os = "macos")]
struct MacCaptureContext {
    quic_transport: quic_transport::TransportHandle,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    active: Mutex<Option<ActiveTarget>>,
    remote_active: Arc<AtomicBool>,
    main_window_visible: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    targets: Vec<InputTarget>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
    anchor: Mutex<Option<(f64, f64)>>,
    cursor_hidden: Mutex<bool>,
    cursor_hide_depth: Mutex<usize>,
    last_cursor_hide_reassert: Mutex<Option<Instant>>,
    last_mouse_move_sent: Mutex<Option<Instant>>,
    last_cursor_repin: Mutex<Option<Instant>>,
    // Instant we last returned control to the local machine. We now land the
    // cursor flush against the edge (RETURN_EDGE_INSET=0) for a seamless
    // extended-display feel, so without a cooldown a fast back-flick would
    // immediately re-satisfy the crossing test and bounce to the remote. During
    // the cooldown window we refuse to cross, letting the user's slide settle.
    last_return: Mutex<Option<Instant>>,
    remote_button_mask: AtomicU64,
    pressed_modifiers: Mutex<Vec<u16>>,
    // Regular (non-modifier) keys we have forwarded as held, so they can be
    // released if the cursor crosses back to local while a key is still down.
    pressed_keys: Mutex<Vec<u16>>,
    tap_disabled: AtomicBool,
    just_crossed: AtomicBool,
    suppress_next_mouse_delta: AtomicBool,
    hotkey_return_point: Mutex<Option<(f64, f64)>>,
    local_screen_points: Mutex<HashMap<String, (f64, f64)>>,
    local_y_bounds: Option<(f64, f64)>,
    display_snapshots: Vec<MacDisplaySnapshot>,
}

#[cfg(target_os = "macos")]
struct RawMacosGestureTap {
    mach_port: core_foundation::mach_port::CFMachPort,
    _context: Arc<MacCaptureContext>,
}

#[cfg(target_os = "macos")]
impl RawMacosGestureTap {
    fn new(
        location: core_graphics::event::CGEventTapLocation,
        context: Arc<MacCaptureContext>,
    ) -> Result<Self, ()> {
        use core_foundation::base::TCFType;
        use core_foundation::mach_port::CFMachPort;
        use core_graphics::event::{CGEventTapOptions, CGEventTapPlacement};

        let mach_port = unsafe {
            macos_raw_event_tap_create(
                location,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                macos_raw_gesture_event_mask(),
                macos_raw_gesture_event_callback,
                Arc::as_ptr(&context).cast(),
            )
        };
        if mach_port.is_null() {
            return Err(());
        }

        Ok(Self {
            mach_port: unsafe { CFMachPort::wrap_under_create_rule(mach_port) },
            _context: context,
        })
    }

    fn mach_port(&self) -> &core_foundation::mach_port::CFMachPort {
        &self.mach_port
    }

    fn enable(&self) {
        use core_foundation::base::TCFType;

        unsafe {
            macos_raw_event_tap_enable(self.mach_port.as_concrete_TypeRef(), true);
        }
    }
}

#[cfg(target_os = "macos")]
impl Drop for RawMacosGestureTap {
    fn drop(&mut self) {
        use core_foundation::base::TCFType;
        use core_foundation::mach_port::CFMachPortInvalidate;

        unsafe {
            CFMachPortInvalidate(self.mach_port.as_CFTypeRef() as *mut _);
        }
    }
}

#[cfg(target_os = "macos")]
type MacosRawEventTapCallback = unsafe extern "C" fn(
    proxy: core_graphics::event::CGEventTapProxy,
    event_type: u32,
    event: core_graphics::sys::CGEventRef,
    user_info: *const std::ffi::c_void,
) -> core_graphics::sys::CGEventRef;

#[cfg(target_os = "macos")]
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    #[link_name = "CGEventTapCreate"]
    fn macos_raw_event_tap_create(
        tap: core_graphics::event::CGEventTapLocation,
        place: core_graphics::event::CGEventTapPlacement,
        options: core_graphics::event::CGEventTapOptions,
        events_of_interest: u64,
        callback: MacosRawEventTapCallback,
        user_info: *const std::ffi::c_void,
    ) -> core_foundation::mach_port::CFMachPortRef;

    #[link_name = "CGEventTapEnable"]
    fn macos_raw_event_tap_enable(tap: core_foundation::mach_port::CFMachPortRef, enable: bool);
}

#[cfg(target_os = "macos")]
fn macos_raw_gesture_event_mask() -> u64 {
    MACOS_RAW_GESTURE_EVENT_TYPES
        .iter()
        .fold(0_u64, |mask, event_type| mask | (1_u64 << *event_type))
}

#[cfg(target_os = "macos")]
unsafe extern "C" fn macos_raw_gesture_event_callback(
    _proxy: core_graphics::event::CGEventTapProxy,
    event_type: u32,
    event: core_graphics::sys::CGEventRef,
    user_info: *const std::ffi::c_void,
) -> core_graphics::sys::CGEventRef {
    if user_info.is_null() {
        return event;
    }

    let context = unsafe { &*(user_info as *const MacCaptureContext) };
    if matches!(
        event_type,
        MACOS_RAW_EVENT_TAP_DISABLED_BY_TIMEOUT | MACOS_RAW_EVENT_TAP_DISABLED_BY_USER_INPUT
    ) {
        context.tap_disabled.store(true, Ordering::Relaxed);
        return event;
    }

    if context.remote_active.load(Ordering::Relaxed) {
        repin_macos_cursor_while_remote(context);
        log::debug!(
            "remote-active macOS gesture/system event {} was dropped",
            event_type
        );
        return std::ptr::null_mut();
    }

    event
}

#[cfg(target_os = "macos")]
#[derive(Clone)]
struct MacDisplaySnapshot {
    id: core_graphics::display::CGDirectDisplayID,
    origin_x: f64,
    origin_y: f64,
    max_x: f64,
    max_y: f64,
}

#[cfg(target_os = "windows")]
static WINDOWS_CAPTURE_CONTEXT: Mutex<Option<Arc<WindowsCaptureContext>>> = Mutex::new(None);

#[cfg(target_os = "windows")]
struct WindowsCaptureContext {
    quic_transport: quic_transport::TransportHandle,
    layout_state: Arc<Mutex<LayoutState>>,
    native_layout: LayoutState,
    active: Mutex<Option<ActiveTarget>>,
    remote_active: Arc<AtomicBool>,
    main_window_focused: Arc<AtomicBool>,
    clipboard_target: Arc<Mutex<Option<ClipboardTarget>>>,
    input_events: Arc<AtomicU64>,
    targets: Vec<InputTarget>,
    switch_request: Arc<Mutex<Option<SwitchDirection>>>,
    anchor: Mutex<Option<(f64, f64)>>,
    last_point: Mutex<Option<(f64, f64)>>,
    last_mouse_move_sent: Mutex<Option<Instant>>,
    remote_button_mask: AtomicU64,
    pressed_keys: Mutex<Vec<u16>>,
    cursor_hide_calls: Mutex<u8>,
    // Swallow the first post-crossing delta so a fast flick across the edge
    // does not shove the cursor inward on Windows, where we pin by warping.
    just_crossed: AtomicBool,
    local_screen_points: Mutex<HashMap<String, (f64, f64)>>,
}

// ---------------------------------------------------------------------------
// Fullscreen scene detection (Windows)
//
// Exclusive / borderless-fullscreen apps — FPS games above all — must not have
// the pointer yanked out from under them. While one is frontmost we suspend
// edge crossings, the switch hotkeys and clipboard sync, then resume the
// instant the app drops out of fullscreen, minimises or loses focus.
//
// Two signals, either is sufficient:
//   1. SHQueryUserNotificationState() reporting D3D fullscreen or presentation
//      mode. Catches true exclusive fullscreen cheaply.
//   2. The foreground window's rect covering its monitor's full (not work-area)
//      bounds, while not being the desktop or shell. Catches the borderless
//      windowed-fullscreen mode most modern games actually ship with.
// ---------------------------------------------------------------------------

/// Set while a fullscreen app owns the foreground. Read on the hook hot path,
/// written by the polling thread.
static FULLSCREEN_APP_ACTIVE: AtomicBool = AtomicBool::new(false);
// Held once at startup so the (platform-specific) fullscreen watcher thread can
// raise a native notification on pause/resume without threading an AppHandle
// through the capture plumbing.
static NOTIFY_APP_HANDLE: OnceLock<AppHandle> = OnceLock::new();

/// Store the app handle used to surface fullscreen pause/resume notifications.
/// Called a single time during setup, before capture starts.
pub fn set_app_handle_for_notify(handle: AppHandle) {
    let _ = NOTIFY_APP_HANDLE.set(handle);
}

/// UI language mirror ("cn" | "en") so watcher threads can raise notifications
/// in the language picked in Settings instead of hardcoded Chinese (#6).
static NOTIFY_LANGUAGE: Mutex<String> = Mutex::new(String::new());

/// Publishes the UI language to the watcher threads. Called whenever the layout
/// (and therefore the language setting) is applied.
pub fn set_notify_language(language: &str) {
    if let Ok(mut lang) = NOTIFY_LANGUAGE.lock() {
        lang.clear();
        lang.push_str(if language == "en" { "en" } else { "cn" });
    }
}

fn notify_language_is_en() -> bool {
    NOTIFY_LANGUAGE
        .lock()
        .map(|lang| lang.as_str() == "en")
        .unwrap_or(false)
}

/// Raises the "sharing paused / resumed" notification in the user's language.
/// `whitelist` distinguishes the app-whitelist trigger (#8) from the broader
/// fullscreen trigger so the copy tells the user *why* sharing stepped aside.
#[allow(dead_code)]
fn notify_sharing_pause(active: bool, whitelist: bool) {
    let Some(app) = NOTIFY_APP_HANDLE.get() else {
        return;
    };
    let en = notify_language_is_en();
    let (title, body) = match (active, whitelist, en) {
        (true, true, false) => (
            "mykvm · 已暂停",
            "名单中的应用已切到前台，键鼠共享已暂停，不影响你操作。",
        ),
        (true, true, true) => (
            "mykvm · Paused",
            "A whitelisted app is in the foreground — input sharing is paused.",
        ),
        (true, false, false) => (
            "mykvm · 已暂停",
            "检测到全屏应用，已暂停键鼠共享，不影响你操作。",
        ),
        (true, false, true) => (
            "mykvm · Paused",
            "A fullscreen app took the foreground — input sharing is paused.",
        ),
        (false, true, false) => ("mykvm · 已恢复", "名单中的应用已离开前台，键鼠共享已恢复。"),
        (false, true, true) => (
            "mykvm · Resumed",
            "The whitelisted app left the foreground — input sharing is back.",
        ),
        (false, false, false) => ("mykvm · 已恢复", "全屏应用已退出，键鼠共享已自动恢复。"),
        (false, false, true) => (
            "mykvm · Resumed",
            "The fullscreen app exited — input sharing is back.",
        ),
    };
    if let Err(error) = app
        .notification()
        .builder()
        .title(title)
        .body(body)
        .show()
    {
        log::warn!("pause notification failed: {error}");
    }
}
/// How often the watcher samples the foreground window.
#[cfg(target_os = "windows")]
const FULLSCREEN_POLL_INTERVAL_MS: u64 = 600;

pub fn fullscreen_app_active() -> bool {
    FULLSCREEN_APP_ACTIVE.load(Ordering::Relaxed)
}

/// Set while a whitelisted app (e.g. a game) owns the foreground. Read on the
/// input hot path alongside `FULLSCREEN_APP_ACTIVE` so a pause-whitelist match
/// halts sharing exactly like a fullscreen app does.
static APP_PAUSE_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Whitelisted app identifiers (macOS bundle id / Windows exe name, lowercased
/// on store) that pause input sharing when they take the foreground.
static PAUSE_WHITELIST: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn app_pause_active() -> bool {
    APP_PAUSE_ACTIVE.load(Ordering::Relaxed)
}

/// Publishes the pause whitelist (bundle ids / exe names). Stored lowercased so
/// the foreground-app comparison is case-insensitive.
pub fn set_pause_whitelist(list: Vec<String>) {
    if let Ok(mut wl) = PAUSE_WHITELIST.lock() {
        *wl = list.into_iter().map(|s| s.to_lowercase()).collect();
    }
}

fn whitelist_contains(id: &str) -> bool {
    match PAUSE_WHITELIST.lock() {
        Ok(wl) => wl.iter().any(|w| w == &id.to_lowercase()),
        Err(_) => false,
    }
}

/// True once the user has curated at least one app to pause for.
pub fn pause_whitelist_configured() -> bool {
    PAUSE_WHITELIST
        .lock()
        .map(|wl| !wl.is_empty())
        .unwrap_or(false)
}

/// Single decision point for "should input sharing stand aside right now?".
///
/// A configured whitelist wins outright (#8): it is the precise, user-curated
/// signal, so once it exists the broad "any fullscreen window" heuristic is no
/// longer consulted — a fullscreen video or a maximised browser must not steal
/// the pointer back. With an empty whitelist the fullscreen heuristic remains
/// as the zero-configuration fallback.
pub fn scene_pause_active() -> bool {
    if pause_whitelist_configured() {
        app_pause_active()
    } else {
        fullscreen_app_active()
    }
}

/// Polls the foreground app and pauses input sharing when it matches the
/// user's pause whitelist (e.g. a game). Whitelist pause is the primary trigger
/// (#8); the existing fullscreen watcher stays for the "any fullscreen" trigger.
pub fn spawn_app_pause_watcher(stop: Arc<AtomicBool>) {
    #[cfg(target_os = "macos")]
    spawn_macos_app_pause_watcher(stop);
    #[cfg(target_os = "windows")]
    spawn_windows_app_pause_watcher(stop);
}

#[cfg(target_os = "macos")]
fn spawn_macos_app_pause_watcher(stop: Arc<AtomicBool>) {
    use std::time::Duration;

    std::thread::spawn(move || {
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let active = match unsafe { macos_frontmost_bundle_id() } {
                Some(id) => whitelist_contains(&id),
                None => false,
            };
            let previous = APP_PAUSE_ACTIVE.swap(active, Ordering::Relaxed);
            if previous != active {
                log::info!(
                    "pause-whitelist app {} — input sharing {}",
                    if active { "matched" } else { "left" },
                    if active { "paused" } else { "resumed" }
                );
                notify_sharing_pause(active, true);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        APP_PAUSE_ACTIVE.store(false, Ordering::Relaxed);
    });
}

#[cfg(target_os = "macos")]
unsafe fn macos_frontmost_bundle_id() -> Option<String> {
    use std::ffi::{c_void, CStr};
    use std::os::raw::c_char;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    let cls = objc_getClass(b"NSWorkspace\0".as_ptr() as *const c_char);
    if cls.is_null() {
        return None;
    }
    let shared = sel_registerName(b"sharedWorkspace\0".as_ptr() as *const c_char);
    let msg: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
        std::mem::transmute(objc_msgSend as *const ());
    let ws = msg(cls, shared);
    if ws.is_null() {
        return None;
    }
    let front = sel_registerName(b"frontmostApplication\0".as_ptr() as *const c_char);
    let app = msg(ws, front);
    if app.is_null() {
        return None;
    }
    let bid_sel = sel_registerName(b"bundleIdentifier\0".as_ptr() as *const c_char);
    let bid_ns: *mut c_void = msg(app, bid_sel);
    if bid_ns.is_null() {
        return None;
    }
    let utf8 = sel_registerName(b"UTF8String\0".as_ptr() as *const c_char);
    let cstr: extern "C" fn(*mut c_void, *mut c_void) -> *const c_char =
        std::mem::transmute(objc_msgSend as *const ());
    let ptr = cstr(bid_ns, utf8);
    if ptr.is_null() {
        return None;
    }
    Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
}

#[cfg(target_os = "windows")]
fn spawn_windows_app_pause_watcher(stop: Arc<AtomicBool>) {
    use std::time::Duration;

    std::thread::spawn(move || {
        loop {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let active = match windows_foreground_exe_name() {
                Some(name) => whitelist_contains(&name),
                None => false,
            };
            let previous = APP_PAUSE_ACTIVE.swap(active, Ordering::Relaxed);
            if previous != active {
                log::info!(
                    "pause-whitelist app {} — input sharing {}",
                    if active { "matched" } else { "left" },
                    if active { "paused" } else { "resumed" }
                );
                // Never strand the pointer on the remote when a game takes over.
                if active {
                    if let Some(context) = windows_capture_context() {
                        release_windows_remote_control(&context, false);
                    }
                }
                notify_sharing_pause(active, true);
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        APP_PAUSE_ACTIVE.store(false, Ordering::Relaxed);
    });
}

#[cfg(target_os = "windows")]
fn windows_foreground_exe_name() -> Option<String> {
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::{CloseHandle, HWND};
    use windows_sys::Win32::System::ProcessStatus::GetModuleFileNameExW;
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetForegroundWindow, GetWindowThreadProcessId,
    };

    let hwnd: HWND = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return None;
    }
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if pid == 0 {
        return None;
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buf = [0u16; 1024];
    let len = unsafe {
        GetModuleFileNameExW(
            handle,
            std::ptr::null_mut(),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
    };
    unsafe { CloseHandle(handle) };
    if len == 0 {
        return None;
    }
    let name = std::ffi::OsString::from_wide(&buf[..len as usize])
        .to_string_lossy()
        .into_owned();
    Some(
        name.rsplit('\\')
            .next()
            .unwrap_or("")
            .to_string()
            .to_lowercase(),
    )
}

#[cfg(target_os = "windows")]
fn detect_windows_fullscreen_app() -> bool {
    use windows_sys::Win32::Foundation::{HWND, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::Shell::{
        SHQueryUserNotificationState, QUNS_BUSY, QUNS_PRESENTATION_MODE,
        QUNS_RUNNING_D3D_FULL_SCREEN,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetDesktopWindow, GetForegroundWindow, GetShellWindow, GetWindowRect, IsWindowVisible,
    };

    // Signal 1: the shell's own notion of "user is busy in fullscreen".
    // QUNS_BUSY is deliberately excluded from the positive set — plenty of
    // ordinary apps trip it — but D3D fullscreen and presentation mode are
    // unambiguous.
    let mut state = 0i32;
    if unsafe { SHQueryUserNotificationState(&mut state) } == 0 {
        if state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE {
            return true;
        }
        let _ = QUNS_BUSY;
    }

    // Signal 2: borderless windowed fullscreen — foreground window rect equals
    // the monitor's full bounds.
    let hwnd: HWND = unsafe { GetForegroundWindow() };
    if hwnd.is_null() {
        return false;
    }
    // The desktop and the shell (Progman/WorkerW) always "cover" the monitor;
    // treating them as fullscreen would pause the app on a bare desktop.
    if hwnd == unsafe { GetDesktopWindow() } || hwnd == unsafe { GetShellWindow() } {
        return false;
    }
    if unsafe { IsWindowVisible(hwnd) } == 0 {
        return false;
    }

    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    if unsafe { GetWindowRect(hwnd, &mut rect) } == 0 {
        return false;
    }

    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return false;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return false;
    }

    // Compare against rcMonitor (full bounds), not rcWork: a maximised ordinary
    // window matches rcWork but leaves the taskbar visible, and the user still
    // wants MyKVM live in that case. Only a window swallowing the taskbar too
    // counts as fullscreen. A 1px tolerance absorbs games that size themselves
    // slightly off.
    let m = info.rcMonitor;
    (rect.left - m.left).abs() <= 1
        && (rect.top - m.top).abs() <= 1
        && (rect.right - m.right).abs() <= 1
        && (rect.bottom - m.bottom).abs() <= 1
}

/// Samples the foreground window on a background thread and publishes the
/// result to `FULLSCREEN_APP_ACTIVE`. Only transitions are logged, so an idle
/// game session stays quiet in the log.
#[cfg(target_os = "windows")]
fn spawn_fullscreen_watcher(stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            // The whitelist, once configured, supersedes this heuristic (#8).
            // Stay silent rather than raising "fullscreen detected" notices for
            // a pause that `scene_pause_active` will not honour anyway.
            if !FULLSCREEN_PAUSE_ENABLED.load(Ordering::Relaxed) || pause_whitelist_configured() {
                if FULLSCREEN_APP_ACTIVE.swap(false, Ordering::Relaxed) {
                    log::info!("fullscreen pause inactive — input sharing resumed");
                }
                std::thread::sleep(Duration::from_millis(FULLSCREEN_POLL_INTERVAL_MS));
                continue;
            }
            let active = detect_windows_fullscreen_app();
            let previous = FULLSCREEN_APP_ACTIVE.swap(active, Ordering::Relaxed);
            if previous != active {
                log::info!(
                    "fullscreen app {} — input sharing {}",
                    if active { "detected" } else { "dismissed" },
                    if active { "paused" } else { "resumed" }
                );
                // Never strand the pointer on the remote when a game takes over.
                if active {
                    if let Some(context) = windows_capture_context() {
                        release_windows_remote_control(&context, false);
                    }
                }
                // Surface the pause/resume as a native notification so the user
                // knows sharing stepped aside for their game and came back.
                notify_sharing_pause(active, false);
            }
            std::thread::sleep(Duration::from_millis(FULLSCREEN_POLL_INTERVAL_MS));
        }
        FULLSCREEN_APP_ACTIVE.store(false, Ordering::Relaxed);
    });
}

#[cfg(target_os = "windows")]
fn windows_capture_context() -> Option<Arc<WindowsCaptureContext>> {
    WINDOWS_CAPTURE_CONTEXT
        .lock()
        .ok()
        .and_then(|context| context.clone())
}

#[cfg(target_os = "windows")]
fn clear_windows_capture_context() {
    if let Ok(mut context) = WINDOWS_CAPTURE_CONTEXT.lock() {
        *context = None;
    }
}

fn should_send_mouse_move(last_sent: &Mutex<Option<Instant>>, dragging: bool) -> bool {
    let interval = Duration::from_millis(if dragging {
        DRAG_MOVE_SEND_INTERVAL_MS
    } else {
        MOUSE_MOVE_SEND_INTERVAL_MS
    });
    let Ok(mut last_sent) = last_sent.lock() else {
        return true;
    };
    let now = Instant::now();
    if last_sent
        .as_ref()
        .map(|sent| now.duration_since(*sent) < interval)
        .unwrap_or(false)
    {
        return false;
    }
    *last_sent = Some(now);
    true
}

fn mark_mouse_move_sent(last_sent: &Mutex<Option<Instant>>) {
    if let Ok(mut last_sent) = last_sent.lock() {
        *last_sent = Some(Instant::now());
    }
}

fn reset_mouse_move_timer(last_sent: &Mutex<Option<Instant>>) {
    if let Ok(mut last_sent) = last_sent.lock() {
        *last_sent = None;
    }
}

fn remote_button_is_down(mask: &AtomicU64) -> bool {
    mask.load(Ordering::Relaxed) != 0
}

// ---------------------------------------------------------------------------
// Local drag gate
//
// `remote_button_mask` tracks buttons we have *forwarded* to the remote; it
// says nothing about buttons held while the cursor is still local. Window
// dragging happens entirely locally, so it needs its own mask. Without it a
// window shoved into a snap zone satisfies the crossing test (flush against the
// edge, outward delta) and the pointer teleports away mid-gesture.
// ---------------------------------------------------------------------------

/// Physical mouse buttons currently held on *this* machine, remote or not.
static LOCAL_BUTTON_MASK: AtomicU64 = AtomicU64::new(0);
/// When the pointer first satisfied a crossing while a button was held. Reset
/// as soon as the pointer stops pressing outward or the button is released.
static DRAG_EDGE_PRESS_SINCE: Mutex<Option<Instant>> = Mutex::new(None);
/// Settings mirror, refreshed whenever the layout is saved. Kept as atomics so
/// the hook hot path never has to take the layout lock.
static DRAG_EDGE_GUARD_ENABLED: AtomicBool = AtomicBool::new(true);
static DRAG_CROSSING_HOLD: AtomicU64 = AtomicU64::new(DRAG_CROSSING_HOLD_MS);
static FULLSCREEN_PAUSE_ENABLED: AtomicBool = AtomicBool::new(true);

/// Publishes the scene-awareness settings to the capture hot path.
pub fn set_scene_guard_settings(drag_guard: bool, drag_hold_ms: u64, fullscreen_pause: bool) {
    DRAG_EDGE_GUARD_ENABLED.store(drag_guard, Ordering::Relaxed);
    // Clamp: 0 would silently turn the guard into a no-op, and multi-second
    // holds read as "the cursor is stuck" to the user.
    DRAG_CROSSING_HOLD.store(drag_hold_ms.clamp(100, 5_000), Ordering::Relaxed);
    FULLSCREEN_PAUSE_ENABLED.store(fullscreen_pause, Ordering::Relaxed);
    if !drag_guard {
        clear_drag_edge_press();
    }
    if !fullscreen_pause {
        FULLSCREEN_APP_ACTIVE.store(false, Ordering::Relaxed);
    }
}

// --- Input smoothing (#11) -------------------------------------------------
// Two independent, user-toggleable smoothing layers. `MOUSE_SMOOTHING` low-pass
// filters the per-event movement delta (velocity smoothing) so high-frequency
// jitter from the capture layer / network coalescing doesn't reach the remote
// cursor. `SMOOTH_SCROLL` spreads a single wheel tick into a short burst of
// eased, time-spaced scroll events ("Mos"-style smooth scrolling) on the
// receiving side. Both mirror `LayoutState` so the setting is live.
static MOUSE_SMOOTHING: AtomicBool = AtomicBool::new(true);
static SMOOTH_SCROLL: AtomicBool = AtomicBool::new(true);
/// Natural-scroll reversal (Mos-style "reverse scroll"). Negates wheel deltas
/// so the content tracks the finger like a touchscreen. Mirrors `LayoutState`.
static REVERSE_SCROLL: AtomicBool = AtomicBool::new(false);

/// Publishes the input-smoothing settings to the capture/inject hot paths.
pub fn set_input_smoothing(mouse: bool, scroll: bool, reverse: bool) {
    MOUSE_SMOOTHING.store(mouse, Ordering::Relaxed);
    SMOOTH_SCROLL.store(scroll, Ordering::Relaxed);
    REVERSE_SCROLL.store(reverse, Ordering::Relaxed);
}

/// Exponential moving average over the movement delta. `alpha` near 1 tracks
/// the raw delta closely (minimal smoothing); lower values trail more. Kept at
/// 0.5 so direction reversals stay responsive while single-event jitter is
/// damped. Returns the smoothed (dx, dy) for the position accumulator only —
/// callers must still pass the ORIGINAL delta to edge/crossing detection.
fn smooth_mouse_delta(dx: f64, dy: f64) -> (f64, f64) {
    if !MOUSE_SMOOTHING.load(Ordering::Relaxed) {
        return (dx, dy);
    }
    static PREV_DX: Mutex<f64> = Mutex::new(0.0);
    static PREV_DY: Mutex<f64> = Mutex::new(0.0);
    const ALPHA: f64 = 0.5;
    let mut prev_x = PREV_DX.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut prev_y = PREV_DY.lock().unwrap_or_else(|poison| poison.into_inner());
    let smoothed_x = ALPHA * dx + (1.0 - ALPHA) * *prev_x;
    let smoothed_y = ALPHA * dy + (1.0 - ALPHA) * *prev_y;
    *prev_x = smoothed_x;
    *prev_y = smoothed_y;
    (smoothed_x, smoothed_y)
}

/// Receiving-side scroll injection. `delta_x` / `delta_y` are **wheel notches
/// (lines)** — both senders normalise to ±1 per physical detent (Windows
/// divides `mouseData` by `WHEEL_DELTA`, macOS reads the line-delta field).
/// When smooth scrolling is enabled the notch is handed to the Mos-style
/// smoother, which converts it into a pixel budget and drains it over many
/// frames; otherwise it is posted as a plain line scroll. Reverse scrolling is
/// applied here so it works on both platforms and for both paths.
#[cfg(not(target_os = "macos"))]
pub(crate) fn inject_scroll(delta_x: i32, delta_y: i32) {
    let (delta_x, delta_y) = if REVERSE_SCROLL.load(Ordering::Relaxed) {
        (-delta_x, -delta_y)
    } else {
        (delta_x, delta_y)
    };
    if SMOOTH_SCROLL.load(Ordering::Relaxed) {
        scroll_smoothing::enqueue(delta_x, delta_y);
    } else {
        post_line_scroll(delta_x, delta_y);
    }
}

/// macOS routes *every* scroll — the local wheel and remote injection alike —
/// through the local scroll engine, so a single settings block governs both and
/// a wheel tick feels identical whether the Mac is driving itself or being
/// driven from the other machine. This is also why the toggles live on the Mac
/// rather than on whichever machine happens to host the server UI: the settings
/// that matter are the ones on the machine the pixels move on.
#[cfg(target_os = "macos")]
pub(crate) fn inject_scroll(delta_x: i32, delta_y: i32) {
    macos_scroll::inject_notches(delta_x, delta_y);
}

/// Unsmoothed post: one wire notch becomes one native notch/line so the remote
/// feels exactly like the local wheel.
#[cfg(target_os = "macos")]
fn post_line_scroll(delta_x: i32, delta_y: i32) {
    use core_graphics::{
        event::{CGEvent, CGEventTapLocation, ScrollEventUnit},
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        return;
    };
    // LINE (not PIXEL): the payload is a notch count, so posting it as pixels
    // would scroll a single pixel per detent. axis1 = vertical, axis2 = horizontal.
    if let Ok(event) =
        CGEvent::new_scroll_event(source, ScrollEventUnit::LINE, 2, delta_y, delta_x, 0)
    {
        macos_scroll::tag_own_event(&event);
        event.post(CGEventTapLocation::HID);
    }
}

#[cfg(target_os = "windows")]
fn post_line_scroll(delta_x: i32, delta_y: i32) {
    crate::windows_input::inject_scroll(delta_x, delta_y);
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn post_line_scroll(_delta_x: i32, _delta_y: i32) {}

/// Smoothed post. The smoother works in a **pixel** domain (Mos' native unit),
/// but every OS scroll API takes an integer payload, so the fractional
/// remainder is carried across frames. Without the carry each frame would lose
/// up to half a unit to rounding and a smoothed notch would visibly travel a
/// shorter distance than an unsmoothed one — the core reason the previous
/// implementation felt wrong. `flush` drains the carry when a scroll session
/// ends so the totals line up.
fn post_pixel_scroll(px_x: f64, px_y: f64, flush: bool) {
    static CARRY_X: Mutex<f64> = Mutex::new(0.0);
    static CARRY_Y: Mutex<f64> = Mutex::new(0.0);

    let (unit_x, unit_y) = pixels_to_post_units(px_x, px_y);
    let (out_x, out_y) = {
        let mut carry_x = CARRY_X.lock().unwrap_or_else(|poison| poison.into_inner());
        let mut carry_y = CARRY_Y.lock().unwrap_or_else(|poison| poison.into_inner());
        *carry_x += unit_x;
        *carry_y += unit_y;
        let (out_x, out_y) = if flush {
            (carry_x.round(), carry_y.round())
        } else {
            (carry_x.trunc(), carry_y.trunc())
        };
        *carry_x -= out_x;
        *carry_y -= out_y;
        if flush {
            *carry_x = 0.0;
            *carry_y = 0.0;
        }
        (out_x as i32, out_y as i32)
    };
    if out_x != 0 || out_y != 0 {
        post_smoothed_units(out_x, out_y);
    }
}

/// Converts the smoother's pixel output into the unit the platform post below
/// expects. macOS consumes pixels directly; Windows' `mouseData` is in
/// `WHEEL_DELTA` (120) units, so one notch worth of pixels must map back to
/// exactly 120 — otherwise smoothing would silently rescale the scroll speed.
#[cfg(target_os = "windows")]
fn pixels_to_post_units(px_x: f64, px_y: f64) -> (f64, f64) {
    const WHEEL_DELTA: f64 = 120.0;
    let scale = WHEEL_DELTA / scroll_smoothing::PIXELS_PER_NOTCH;
    (px_x * scale, px_y * scale)
}

#[cfg(not(target_os = "windows"))]
fn pixels_to_post_units(px_x: f64, px_y: f64) -> (f64, f64) {
    (px_x, px_y)
}

#[cfg(target_os = "macos")]
fn post_smoothed_units(delta_x: i32, delta_y: i32) {
    use core_graphics::{
        event::{CGEvent, CGEventTapLocation, EventField, ScrollEventUnit},
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        return;
    };
    // Pixel deltas with `isContinuous = 1` so macOS animates the scroll (and
    // rubber-bands at document edges) instead of snapping a notch.
    if let Ok(event) =
        CGEvent::new_scroll_event(source, ScrollEventUnit::PIXEL, 2, delta_y, delta_x, 0)
    {
        event.set_integer_value_field(EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS, 1);
        macos_scroll::tag_own_event(&event);
        event.post(CGEventTapLocation::HID);
    }
}

#[cfg(target_os = "windows")]
fn post_smoothed_units(delta_x: i32, delta_y: i32) {
    crate::windows_input::inject_scroll_units(delta_x, delta_y);
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn post_smoothed_units(_delta_x: i32, _delta_y: i32) {}

/// Mos-style smooth scrolling (port of `ScrollCore` / `ScrollPoster` /
/// `ScrollFilter`).
///
/// A wire notch is first converted into a **pixel budget** (`step * speed`,
/// Mos' defaults) and accumulated into `buffer`. A worker thread then drains
/// `current` toward `buffer` once per frame with a linear interpolation
/// (`current += (buffer - current) * trans`), passes the per-frame delta
/// through Mos' curve filter, and posts the result.
///
/// Three properties matter and are each covered by a test below:
///
/// 1. **Distance is preserved.** `Σ posted == buffer`. The lerp never reaches
///    its target and the filter lags by design, so the session is explicitly
///    *flushed* (residual + filter window emitted in one final frame) once it
///    converges. The integer rounding at the platform boundary is handled by a
///    fractional carry in `post_pixel_scroll`.
/// 2. **Speed is unchanged.** One notch travels `step * speed` pixels whether
///    or not smoothing is on, on both platforms.
/// 3. **No idle spin.** The worker parks on a condvar between sessions instead
///    of waking 125×/s forever.
mod scroll_smoothing {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    /// Reference defaults. `durationTransition` 4.35 maps to a per-frame lerp
    /// factor of ~0.085.
    pub(super) const DEFAULT_TRANS: f64 = 0.085;
    pub(super) const DEFAULT_STEP: f64 = 33.6;
    pub(super) const DEFAULT_SPEED: f64 = 2.70;
    pub(super) const DEFAULT_INTERVAL_MS: u64 = 8;
    /// Pixels one wheel notch is worth at the default tuning. A sub-`step` raw
    /// pixel delta (~10px/notch on macOS) is clamped up to `step` before
    /// `speed` is applied, so a detent scrolls `step * speed` ≈ 90.7px —
    /// reproduced directly because our wire unit is already a notch count.
    ///
    /// The live values are the atomics below; this const stays the reference
    /// point for the tests and for the Windows unit conversion, which is not
    /// user-tunable (the tuning UI is macOS-only).
    pub(super) const PIXELS_PER_NOTCH: f64 = DEFAULT_STEP * DEFAULT_SPEED;

    // Live tunables. Read on the worker/hot path, so they are plain atomics
    // rather than a lock; f64 travels as its bit pattern.
    static STEP_BITS: AtomicU64 = AtomicU64::new(DEFAULT_STEP.to_bits());
    static SPEED_BITS: AtomicU64 = AtomicU64::new(DEFAULT_SPEED.to_bits());
    static TRANS_BITS: AtomicU64 = AtomicU64::new(DEFAULT_TRANS.to_bits());
    static INTERVAL_MS: AtomicU64 = AtomicU64::new(DEFAULT_INTERVAL_MS);

    /// Publishes the user-tunable smoothing parameters. Values are clamped to
    /// the range the interpolator stays stable in: `trans` outside (0, 1] either
    /// never converges or overshoots, and an interval below 4ms would post
    /// faster than any display refreshes.
    pub(super) fn set_tunables(step: f64, speed: f64, transition: f64, interval_ms: u64) {
        STEP_BITS.store(step.clamp(1.0, 500.0).to_bits(), Ordering::Relaxed);
        SPEED_BITS.store(speed.clamp(0.1, 10.0).to_bits(), Ordering::Relaxed);
        TRANS_BITS.store(transition.clamp(0.01, 1.0).to_bits(), Ordering::Relaxed);
        INTERVAL_MS.store(interval_ms.clamp(4, 32), Ordering::Relaxed);
    }

    /// Live pixels-per-notch (`step * speed`).
    pub(super) fn pixels_per_notch() -> f64 {
        f64::from_bits(STEP_BITS.load(Ordering::Relaxed))
            * f64::from_bits(SPEED_BITS.load(Ordering::Relaxed))
    }

    fn trans() -> f64 {
        f64::from_bits(TRANS_BITS.load(Ordering::Relaxed))
    }

    fn interval() -> Duration {
        Duration::from_millis(INTERVAL_MS.load(Ordering::Relaxed))
    }
    /// Mos `ScrollFilter.polish` blends 23% of the new frame into the window.
    const FILTER_ALPHA: f64 = 0.23;
    /// Residual (px) at which an idle session is flushed and the worker parked.
    const SETTLE_PX: f64 = 0.5;
    /// How long after the last wheel tick the session counts as "released".
    const MANUAL_END: Duration = Duration::from_millis(180);

    pub(super) struct State {
        current: (f64, f64),
        buffer: (f64, f64),
        delta: (f64, f64),
        window: (f64, f64),
        last_input: Instant,
        active: bool,
    }

    impl State {
        fn new() -> Self {
            State {
                current: (0.0, 0.0),
                buffer: (0.0, 0.0),
                delta: (0.0, 0.0),
                window: (0.0, 0.0),
                last_input: Instant::now(),
                active: false,
            }
        }

        fn reset(&mut self) {
            self.current = (0.0, 0.0);
            self.buffer = (0.0, 0.0);
            self.delta = (0.0, 0.0);
            self.window = (0.0, 0.0);
            self.active = false;
        }
    }

    static PARK: OnceLock<(Mutex<State>, Condvar)> = OnceLock::new();
    static WORKER_STARTED: AtomicBool = AtomicBool::new(false);

    fn park() -> &'static (Mutex<State>, Condvar) {
        PARK.get_or_init(|| (Mutex::new(State::new()), Condvar::new()))
    }

    fn lock() -> MutexGuard<'static, State> {
        park().0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Converts a wire notch count into Mos' pixel domain. Mos normalises a
    /// sub-`step` tick up to a full `step` so a tiny notch still moves; with an
    /// integer notch count that reduces to `|n| >= 1`, kept explicit so the
    /// intent survives if the wire ever carries fractional ticks.
    fn notches_to_pixels(notches: f64) -> f64 {
        if notches == 0.0 {
            return 0.0;
        }
        notches.signum() * notches.abs().max(1.0) * pixels_per_notch()
    }

    /// Mos `ScrollFilter.polish`: a one-pole blend read one frame late, which
    /// rounds off the start/stop jitter of the raw lerp. It is sum-preserving —
    /// `Σ out == Σ frame - window_final` — and `advance` emits `window_final`
    /// in the flush frame so nothing is lost.
    fn polish(window: &mut f64, next: f64) -> f64 {
        let out = *window;
        *window += FILTER_ALPHA * (next - *window);
        out
    }

    pub fn enqueue(delta_x: i32, delta_y: i32) {
        if delta_x == 0 && delta_y == 0 {
            return;
        }
        // axis mapping: incoming delta_y is vertical (axis1), delta_x horizontal (axis2)
        let px = (
            notches_to_pixels(delta_y as f64),
            notches_to_pixels(delta_x as f64),
        );
        submit(px);
    }

    /// Entry point for the local macOS wheel tap, which has already converted
    /// the raw event into pixels and applied the modifier scaling. `px` is
    /// `(vertical, horizontal)` to match the axis order used internally.
    #[cfg(target_os = "macos")]
    pub(super) fn enqueue_pixels(px_y: f64, px_x: f64) {
        if px_y == 0.0 && px_x == 0.0 {
            return;
        }
        submit((px_y, px_x));
    }

    fn submit(px: (f64, f64)) {
        {
            let mut s = lock();
            push(&mut s, px);
            s.last_input = Instant::now();
        }
        park().1.notify_all();
        ensure_worker();
    }

    /// Mos `ScrollPoster.update()`: ticks in the same direction extend the
    /// current session's target, a direction reversal restarts it so the
    /// remaining momentum of the old direction is dropped immediately.
    pub(super) fn push(s: &mut State, px: (f64, f64)) {
        if px.0 * s.delta.0 > 0.0 {
            s.buffer.0 += px.0;
        } else if px.0 != 0.0 {
            s.buffer.0 = px.0;
            s.current.0 = 0.0;
        }
        if px.1 * s.delta.1 > 0.0 {
            s.buffer.1 += px.1;
        } else if px.1 != 0.0 {
            s.buffer.1 = px.1;
            s.current.1 = 0.0;
        }
        if px.0 != 0.0 {
            s.delta.0 = px.0;
        }
        if px.1 != 0.0 {
            s.delta.1 = px.1;
        }
        s.active = true;
    }

    /// One interpolation frame. Returns `(out_x, out_y, finished)` in pixels;
    /// `finished` marks the flush frame that ends the session.
    pub(super) fn advance(s: &mut State, idle: bool) -> (f64, f64, bool) {
        let trans = trans();
        let frame_y = (s.buffer.0 - s.current.0) * trans;
        let frame_x = (s.buffer.1 - s.current.1) * trans;
        s.current.0 += frame_y;
        s.current.1 += frame_x;
        let mut out_y = polish(&mut s.window.0, frame_y);
        let mut out_x = polish(&mut s.window.1, frame_x);

        let residual_y = s.buffer.0 - s.current.0;
        let residual_x = s.buffer.1 - s.current.1;
        let converged = residual_y.abs() <= SETTLE_PX && residual_x.abs() <= SETTLE_PX;
        if idle && converged {
            // Emit everything the lerp and the filter still owe, in one frame,
            // so the smoothed distance equals the raw distance exactly. The
            // filter's outstanding debt is `window / alpha`, not `window`:
            // summing `w_n = w_{n-1} + alpha * (f_n - w_{n-1})` telescopes to
            // `Σ out = Σ frame - w_final / alpha`.
            out_y += residual_y + s.window.0 / FILTER_ALPHA;
            out_x += residual_x + s.window.1 / FILTER_ALPHA;
            s.reset();
            return (out_x, out_y, true);
        }
        (out_x, out_y, false)
    }

    fn ensure_worker() {
        if WORKER_STARTED.swap(true, Ordering::Relaxed) {
            return;
        }
        thread::spawn(|| loop {
            {
                let (mutex, signal) = park();
                let mut guard = mutex.lock().unwrap_or_else(|poison| poison.into_inner());
                while !guard.active {
                    guard = signal
                        .wait(guard)
                        .unwrap_or_else(|poison| poison.into_inner());
                }
            }
            thread::sleep(interval());
            let (out_x, out_y, finished) = {
                let mut s = lock();
                if !s.active {
                    continue;
                }
                let idle = s.last_input.elapsed() > MANUAL_END;
                advance(&mut s, idle)
            };
            if out_x != 0.0 || out_y != 0.0 || finished {
                super::post_pixel_scroll(out_x, out_y, finished);
            }
        });
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn drain(ticks: &[(i32, i32)]) -> (f64, f64, usize) {
            let mut s = State::new();
            for &(dx, dy) in ticks {
                push(
                    &mut s,
                    (notches_to_pixels(dy as f64), notches_to_pixels(dx as f64)),
                );
            }
            let (mut total_y, mut total_x) = (0.0, 0.0);
            let mut frames = 0usize;
            // `idle = true` mirrors "user released the wheel": the session may
            // settle as soon as it converges.
            loop {
                let (out_x, out_y, finished) = advance(&mut s, true);
                total_x += out_x;
                total_y += out_y;
                frames += 1;
                if finished || frames > 10_000 {
                    break;
                }
            }
            (total_x, total_y, frames)
        }

        #[test]
        fn one_notch_is_worth_step_times_speed_pixels() {
            assert!((notches_to_pixels(1.0) - 33.6 * 2.70).abs() < 1e-9);
            assert!((notches_to_pixels(-1.0) + 33.6 * 2.70).abs() < 1e-9);
            assert_eq!(notches_to_pixels(0.0), 0.0);
        }

        #[test]
        fn smoothing_preserves_total_scroll_distance() {
            let (total_x, total_y, frames) = drain(&[(0, 3)]);
            assert!(
                (total_y - 3.0 * PIXELS_PER_NOTCH).abs() < 1e-6,
                "vertical distance drifted: {total_y}"
            );
            assert!(total_x.abs() < 1e-9, "horizontal axis leaked: {total_x}");
            // Must actually be spread over many frames, not dumped at once.
            assert!(frames > 20, "scroll was not smoothed: {frames} frames");
        }

        #[test]
        fn smoothing_preserves_distance_on_both_axes() {
            let (total_x, total_y, _) = drain(&[(-2, 5)]);
            assert!((total_y - 5.0 * PIXELS_PER_NOTCH).abs() < 1e-6);
            assert!((total_x + 2.0 * PIXELS_PER_NOTCH).abs() < 1e-6);
        }

        #[test]
        fn direction_reversal_restarts_the_session() {
            let mut s = State::new();
            push(&mut s, (notches_to_pixels(1.0), 0.0));
            for _ in 0..5 {
                advance(&mut s, false);
            }
            push(&mut s, (notches_to_pixels(-1.0), 0.0));
            // The reversal drops the leftover downward momentum instead of
            // subtracting from it, so the new target is a clean single notch up.
            assert!((s.buffer.0 + PIXELS_PER_NOTCH).abs() < 1e-9);
            assert_eq!(s.current.0, 0.0);
        }

        #[test]
        fn session_parks_after_settling() {
            let mut s = State::new();
            push(&mut s, (notches_to_pixels(1.0), 0.0));
            let mut frames = 0;
            while frames < 10_000 {
                let (_, _, finished) = advance(&mut s, true);
                frames += 1;
                if finished {
                    break;
                }
            }
            assert!(frames < 10_000, "session never converged");
            assert!(!s.active, "worker would keep spinning after settling");
        }

        #[test]
        fn held_wheel_never_settles_until_released() {
            let mut s = State::new();
            push(&mut s, (notches_to_pixels(1.0), 0.0));
            for _ in 0..500 {
                let (_, _, finished) = advance(&mut s, false);
                assert!(!finished, "settled while the wheel was still turning");
            }
            assert!(s.active);
        }
    }
}

// --- macOS local scroll engine (#2) ----------------------------------------
//
// A self-contained replacement for a third-party scroll enhancer. Running one
// alongside MyKVM meant two independent smoothers fighting over the same wheel:
// each notch got expanded twice, which is why scrolling felt runaway fast, and
// the reverse toggle appeared dead because the other tool re-flipped the axis
// downstream. Owning the whole path removes the conflict and, more importantly,
// puts the settings on the machine they actually take effect on.
//
// Two properties the design has to hold:
//
//  * **Independent of the link.** The engine is started when the app starts and
//    keeps working with the KVM runtime stopped — it is the Mac's own scrolling,
//    not a remote-control feature.
//  * **Idle-free.** The tap is only created once a feature needs it, the
//    callback is a handful of atomic loads, and the interpolation worker parks
//    on a condvar between gestures. Nothing polls.

/// User-facing configuration of the macOS local scroll engine. Defined for all
/// platforms so the settings plumbing in `lib.rs` stays platform-agnostic; only
/// the macOS build acts on it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MacScrollSettings {
    /// Master switch for interpolated scrolling.
    pub smooth: bool,
    /// Natural / reversed wheel direction.
    pub reverse: bool,
    /// Hold Option to scroll faster.
    pub option_accelerate: bool,
    /// Multiplier applied while Option is held.
    pub option_factor: f64,
    /// Hold Shift to turn a vertical wheel into a horizontal scroll.
    pub shift_horizontal: bool,
    /// Hold Command to pass the wheel through untouched — keeps pinch-free
    /// zoom (Cmd+wheel) working in browsers and image editors, which would
    /// otherwise lose the modifier when we re-emit the event.
    pub command_bypass: bool,
    /// Pixels one detent is worth before `speed`.
    pub step: f64,
    /// Multiplier on `step`.
    pub speed: f64,
    /// Per-frame interpolation factor: lower glides longer, higher snaps.
    pub transition: f64,
    /// Frame interval of the interpolation worker, in milliseconds.
    pub interval_ms: u64,
}

impl Default for MacScrollSettings {
    fn default() -> Self {
        MacScrollSettings {
            smooth: true,
            reverse: false,
            option_accelerate: true,
            option_factor: 3.0,
            shift_horizontal: true,
            command_bypass: true,
            step: scroll_smoothing::DEFAULT_STEP,
            speed: scroll_smoothing::DEFAULT_SPEED,
            transition: scroll_smoothing::DEFAULT_TRANS,
            interval_ms: scroll_smoothing::DEFAULT_INTERVAL_MS,
        }
    }
}

/// Publishes the scroll settings. No-op off macOS: the whole feature is
/// macOS-only, and the UI hides it there.
#[cfg(not(target_os = "macos"))]
pub fn set_macos_scroll_settings(_settings: MacScrollSettings) {}

#[cfg(target_os = "macos")]
pub fn set_macos_scroll_settings(settings: MacScrollSettings) {
    macos_scroll::apply(settings);
}

/// Starts the local scroll engine. Called once at app start, independently of
/// the KVM runtime.
#[cfg(not(target_os = "macos"))]
pub fn start_macos_scroll_engine() {}

#[cfg(target_os = "macos")]
pub fn start_macos_scroll_engine() {
    macos_scroll::start_if_needed();
}

#[cfg(target_os = "macos")]
mod macos_scroll {
    use super::{scroll_smoothing, MacScrollSettings};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

    /// Written into every event we emit so the tap recognises its own output
    /// and passes it straight through. Without it the engine would re-smooth
    /// its own frames — an unbounded feedback loop. Also used by the HID
    /// capture tap (see `handle_macos_event`) to skip events MyKVM injects on
    /// behalf of the remote peer, so Win->Mac wheel/keys reach the local apps
    /// instead of being forwarded back or dropped.
    pub(super) const SELF_TAG: i64 = 0x4D_4B_56_43; // "MKVC"
    /// `CGEventType::ScrollWheel`.
    const EVENT_TYPE_SCROLL_WHEEL: u32 = 22;
    /// Raw pixels macOS reports for one detent of a standard wheel. Only used
    /// to recover a notch count from mice that report no line delta.
    const RAW_PIXELS_PER_NOTCH: f64 = 10.0;

    const FLAG_SHIFT: u64 = 0x0002_0000;
    const FLAG_ALTERNATE: u64 = 0x0008_0000;
    const FLAG_COMMAND: u64 = 0x0010_0000;

    static SMOOTH: AtomicBool = AtomicBool::new(true);
    static REVERSE: AtomicBool = AtomicBool::new(false);
    static OPTION_ACCELERATE: AtomicBool = AtomicBool::new(true);
    static OPTION_FACTOR: AtomicU64 = AtomicU64::new((3.0_f64).to_bits());
    static SHIFT_HORIZONTAL: AtomicBool = AtomicBool::new(true);
    static COMMAND_BYPASS: AtomicBool = AtomicBool::new(true);

    static TAP_PORT: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());
    static TAP_STARTED: AtomicBool = AtomicBool::new(false);

    pub(super) fn apply(settings: MacScrollSettings) {
        SMOOTH.store(settings.smooth, Ordering::Relaxed);
        REVERSE.store(settings.reverse, Ordering::Relaxed);
        OPTION_ACCELERATE.store(settings.option_accelerate, Ordering::Relaxed);
        OPTION_FACTOR.store(
            settings.option_factor.clamp(1.0, 20.0).to_bits(),
            Ordering::Relaxed,
        );
        SHIFT_HORIZONTAL.store(settings.shift_horizontal, Ordering::Relaxed);
        COMMAND_BYPASS.store(settings.command_bypass, Ordering::Relaxed);
        scroll_smoothing::set_tunables(
            settings.step,
            settings.speed,
            settings.transition,
            settings.interval_ms,
        );
        if intercepts_local_wheel() {
            ensure_tap();
        }
    }

    /// Whether any enabled feature needs to see the physical wheel. When none
    /// do we never create the tap at all, so a user who turns the engine off
    /// pays nothing.
    fn intercepts_local_wheel() -> bool {
        SMOOTH.load(Ordering::Relaxed)
            || REVERSE.load(Ordering::Relaxed)
            || OPTION_ACCELERATE.load(Ordering::Relaxed)
            || SHIFT_HORIZONTAL.load(Ordering::Relaxed)
    }

    /// Stamps an event as ours. Public to the parent module so both post paths
    /// mark their output.
    pub(super) fn tag_own_event(event: &core_graphics::event::CGEvent) {
        use core_graphics::event::EventField;
        event.set_integer_value_field(EventField::EVENT_SOURCE_USER_DATA, SELF_TAG);
    }

    /// Remote-injected scroll. Modifier handling is deliberately shared with the
    /// local path so Shift/Option behave the same whether the wheel turned on
    /// this Mac or on the machine driving it — the modifier keys are injected
    /// here first, so the session flag state already reflects them.
    pub(super) fn inject_notches(delta_x: i32, delta_y: i32) {
        let flags = current_flags();
        if COMMAND_BYPASS.load(Ordering::Relaxed) && flags & FLAG_COMMAND != 0 {
            super::post_line_scroll(delta_x, delta_y);
            return;
        }
        let (notch_x, notch_y) = transform(delta_x as f64, delta_y as f64, flags);
        if SMOOTH.load(Ordering::Relaxed) {
            // Win->Mac receive path: post the cooked pixels straight to HID
            // instead of enqueuing into the local smoother's worker thread.
            // The Mos-style smoother is for the *physical* wheel on this Mac;
            // remote input is already cooked and must reach the apps
            // immediately. Posting a continuous, tagged pixel event is exactly
            // the shape the engine emits for its own frames, so the engine tap
            // ignores it (no re-smooth, no swallow). Routing received scroll
            // through the smoother queue was the regression that broke Win->Mac
            // wheel after the mos-like feature landed.
            super::post_smoothed_units(
                notches_to_pixels(notch_x) as i32,
                notches_to_pixels(notch_y) as i32,
            );
        } else {
            super::post_line_scroll(notch_x.round() as i32, notch_y.round() as i32);
        }
    }

    /// Shared axis/direction/acceleration transform. Returns `(x, y)` notches.
    fn transform(delta_x: f64, delta_y: f64, flags: u64) -> (f64, f64) {
        let (mut x, mut y) = (delta_x, delta_y);
        if SHIFT_HORIZONTAL.load(Ordering::Relaxed) && flags & FLAG_SHIFT != 0 && x == 0.0 {
            // Only borrow the vertical axis when the device isn't already
            // producing a horizontal delta, otherwise a tilt wheel would cancel
            // itself out.
            x = y;
            y = 0.0;
        }
        if REVERSE.load(Ordering::Relaxed) {
            x = -x;
            y = -y;
        }
        if OPTION_ACCELERATE.load(Ordering::Relaxed) && flags & FLAG_ALTERNATE != 0 {
            let factor = f64::from_bits(OPTION_FACTOR.load(Ordering::Relaxed));
            x *= factor;
            y *= factor;
        }
        (x, y)
    }

    /// Notches to pixels using the live tuning, preserving the "a fraction of a
    /// detent still moves a full step" behaviour of the smoother.
    fn notches_to_pixels(notches: f64) -> f64 {
        if notches == 0.0 {
            return 0.0;
        }
        notches.signum() * notches.abs().max(1.0) * scroll_smoothing::pixels_per_notch()
    }

    fn current_flags() -> u64 {
        // Combined session state so modifiers injected by the remote count too.
        unsafe { CGEventSourceFlagsState(0) }
    }

    // --- event tap ---------------------------------------------------------

    /// Startup entry point. Skips the tap entirely when the user has every
    /// feature off; `apply` installs it later if they turn one back on.
    pub(super) fn start_if_needed() {
        if intercepts_local_wheel() {
            ensure_tap();
        }
    }

    fn ensure_tap() {
        if TAP_STARTED.swap(true, Ordering::Relaxed) {
            return;
        }
        std::thread::Builder::new()
            .name("mac-scroll-engine".into())
            .spawn(run_tap_loop)
            .map(|_| ())
            .unwrap_or_else(|error| {
                TAP_STARTED.store(false, Ordering::Relaxed);
                log::warn!("local scroll engine thread failed to start: {error}");
            });
    }

    fn run_tap_loop() {
        use core_foundation::base::TCFType;
        use core_foundation::mach_port::CFMachPort;
        use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
        use core_graphics::event::{CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement};

        // Session (not HID) location on purpose: HID taps run first, so the
        // capture tap that forwards input to the other machine still gets the
        // wheel untouched while the cursor is remote, and we only ever see the
        // events it let through — i.e. exactly the ones scrolling this Mac.
        let port = unsafe {
            super::macos_raw_event_tap_create(
                CGEventTapLocation::Session,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                1_u64 << EVENT_TYPE_SCROLL_WHEEL,
                scroll_tap_callback,
                std::ptr::null(),
            )
        };
        if port.is_null() {
            // Almost always missing Accessibility permission. Silent degrade:
            // scrolling keeps working, just without the engine.
            TAP_STARTED.store(false, Ordering::Relaxed);
            log::warn!("local scroll engine tap could not be created (accessibility permission?)");
            return;
        }

        let mach_port = unsafe { CFMachPort::wrap_under_create_rule(port) };
        TAP_PORT.store(port.cast(), Ordering::Relaxed);
        let Some(source) = mach_port.create_runloop_source(0).ok() else {
            TAP_PORT.store(std::ptr::null_mut(), Ordering::Relaxed);
            TAP_STARTED.store(false, Ordering::Relaxed);
            log::warn!("local scroll engine run loop source could not be created");
            return;
        };
        CFRunLoop::get_current().add_source(&source, unsafe { kCFRunLoopCommonModes });
        unsafe { super::macos_raw_event_tap_enable(mach_port.as_concrete_TypeRef(), true) };
        log::info!("local scroll engine started");
        // Blocks forever on the run loop: no polling, no wakeups between
        // scroll events.
        CFRunLoop::run_current();
    }

    unsafe extern "C" fn scroll_tap_callback(
        _proxy: core_graphics::event::CGEventTapProxy,
        event_type: u32,
        event: core_graphics::sys::CGEventRef,
        _user_info: *const c_void,
    ) -> core_graphics::sys::CGEventRef {
        if matches!(
            event_type,
            super::MACOS_RAW_EVENT_TAP_DISABLED_BY_TIMEOUT
                | super::MACOS_RAW_EVENT_TAP_DISABLED_BY_USER_INPUT
        ) {
            let port = TAP_PORT.load(Ordering::Relaxed);
            if !port.is_null() {
                unsafe { super::macos_raw_event_tap_enable(port.cast(), true) };
            }
            return event;
        }
        if event_type != EVENT_TYPE_SCROLL_WHEEL || !intercepts_local_wheel() {
            return event;
        }
        unsafe { handle_local_wheel(event) }
    }

    /// Reads the incoming event through the C API rather than wrapping it in a
    /// `CGEvent`: the callback only borrows the event, and the owned wrapper
    /// would release a reference we never retained.
    unsafe fn handle_local_wheel(
        raw: core_graphics::sys::CGEventRef,
    ) -> core_graphics::sys::CGEventRef {
        use core_graphics::event::EventField;

        // Our own output, looping back through the session stream.
        if unsafe { CGEventGetIntegerValueField(raw, EventField::EVENT_SOURCE_USER_DATA) }
            == SELF_TAG
        {
            return raw;
        }
        // Trackpads and Magic Mouse already produce continuous, momentum-scrolled
        // pixel streams. Re-smoothing those would fight Apple's own physics, so
        // they are left alone — the engine is for detented wheels.
        if unsafe {
            CGEventGetIntegerValueField(raw, EventField::SCROLL_WHEEL_EVENT_IS_CONTINUOUS)
        } != 0
        {
            return raw;
        }

        let flags = unsafe { CGEventGetFlags(raw) };
        if COMMAND_BYPASS.load(Ordering::Relaxed) && flags & FLAG_COMMAND != 0 {
            // Untouched: Cmd+wheel zoom keeps its modifier.
            return raw;
        }

        let line_y =
            unsafe { CGEventGetIntegerValueField(raw, EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1) };
        let line_x =
            unsafe { CGEventGetIntegerValueField(raw, EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2) };
        let (notch_y, notch_x) = if line_y != 0 || line_x != 0 {
            (line_y as f64, line_x as f64)
        } else {
            // Some wheels report only a point delta; recover a notch count so
            // the tuning stays in the same unit.
            unsafe {
                (
                    CGEventGetDoubleValueField(
                        raw,
                        EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1,
                    ) / RAW_PIXELS_PER_NOTCH,
                    CGEventGetDoubleValueField(
                        raw,
                        EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2,
                    ) / RAW_PIXELS_PER_NOTCH,
                )
            }
        };
        if notch_y == 0.0 && notch_x == 0.0 {
            return raw;
        }

        let (out_x, out_y) = transform(notch_x, notch_y, flags);
        if SMOOTH.load(Ordering::Relaxed) {
            scroll_smoothing::enqueue_pixels(notches_to_pixels(out_y), notches_to_pixels(out_x));
        } else {
            super::post_line_scroll(out_x.round() as i32, out_y.round() as i32);
        }
        // Swallow the original: the replacement has already been queued.
        std::ptr::null_mut()
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn CGEventSourceFlagsState(state_id: u32) -> u64;
        fn CGEventGetIntegerValueField(
            event: core_graphics::sys::CGEventRef,
            field: core_graphics::event::CGEventField,
        ) -> i64;
        fn CGEventGetDoubleValueField(
            event: core_graphics::sys::CGEventRef,
            field: core_graphics::event::CGEventField,
        ) -> f64;
        fn CGEventGetFlags(event: core_graphics::sys::CGEventRef) -> u64;
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::{Mutex, MutexGuard};

        /// The settings are process-wide atomics, so the cases have to run one
        /// at a time or they read each other's state.
        static SERIAL: Mutex<()> = Mutex::new(());

        fn reset() -> MutexGuard<'static, ()> {
            let guard = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
            SMOOTH.store(true, Ordering::Relaxed);
            REVERSE.store(false, Ordering::Relaxed);
            OPTION_ACCELERATE.store(true, Ordering::Relaxed);
            OPTION_FACTOR.store((3.0_f64).to_bits(), Ordering::Relaxed);
            SHIFT_HORIZONTAL.store(true, Ordering::Relaxed);
            COMMAND_BYPASS.store(true, Ordering::Relaxed);
            guard
        }

        #[test]
        fn shift_moves_the_wheel_onto_the_horizontal_axis() {
            let _guard = reset();
            assert_eq!(transform(0.0, 3.0, FLAG_SHIFT), (3.0, 0.0));
            // A tilt wheel already scrolling sideways is left alone.
            assert_eq!(transform(2.0, 3.0, FLAG_SHIFT), (2.0, 3.0));
            // Disabled -> untouched.
            SHIFT_HORIZONTAL.store(false, Ordering::Relaxed);
            assert_eq!(transform(0.0, 3.0, FLAG_SHIFT), (0.0, 3.0));
        }

        #[test]
        fn reverse_flips_both_axes() {
            let _guard = reset();
            REVERSE.store(true, Ordering::Relaxed);
            assert_eq!(transform(1.0, -2.0, 0), (-1.0, 2.0));
        }

        #[test]
        fn option_scales_by_the_configured_factor() {
            let _guard = reset();
            assert_eq!(transform(0.0, 2.0, FLAG_ALTERNATE), (0.0, 6.0));
            OPTION_ACCELERATE.store(false, Ordering::Relaxed);
            assert_eq!(transform(0.0, 2.0, FLAG_ALTERNATE), (0.0, 2.0));
        }

        #[test]
        fn shift_and_option_compose_after_reversal() {
            let _guard = reset();
            REVERSE.store(true, Ordering::Relaxed);
            // Vertical 1 -> horizontal 1 -> reversed -> tripled.
            assert_eq!(transform(0.0, 1.0, FLAG_SHIFT | FLAG_ALTERNATE), (-3.0, 0.0));
        }

        #[test]
        fn command_bypass_and_feature_gate_control_the_tap() {
            let _guard = reset();
            SMOOTH.store(false, Ordering::Relaxed);
            REVERSE.store(false, Ordering::Relaxed);
            OPTION_ACCELERATE.store(false, Ordering::Relaxed);
            SHIFT_HORIZONTAL.store(false, Ordering::Relaxed);
            assert!(!intercepts_local_wheel());
            SMOOTH.store(true, Ordering::Relaxed);
            assert!(intercepts_local_wheel());
        }
    }
}

fn set_local_button(button: MouseButton, down: bool) {
    let bit = mouse_button_mask(button);
    if down {
        LOCAL_BUTTON_MASK.fetch_or(bit, Ordering::Relaxed);
    } else {
        LOCAL_BUTTON_MASK.fetch_and(!bit, Ordering::Relaxed);
        if LOCAL_BUTTON_MASK.load(Ordering::Relaxed) == 0 {
            clear_drag_edge_press();
        }
    }
}

fn local_button_is_down() -> bool {
    LOCAL_BUTTON_MASK.load(Ordering::Relaxed) != 0
}

fn reset_local_button_mask() {
    LOCAL_BUTTON_MASK.store(0, Ordering::Relaxed);
    clear_drag_edge_press();
}

fn clear_drag_edge_press() {
    if let Ok(mut since) = DRAG_EDGE_PRESS_SINCE.lock() {
        *since = None;
    }
}

/// Gate a would-be crossing that happens while a button is held.
///
/// Returns `true` when the crossing may proceed. The first qualifying event
/// starts a timer; until `DRAG_CROSSING_HOLD_MS` elapses the crossing is
/// refused, which lets Aero Snap / split-view take the gesture. Keep shoving
/// past the deadline and the crossing is allowed so dragging content onto the
/// other machine still works.
fn drag_crossing_allowed() -> bool {
    if !DRAG_EDGE_GUARD_ENABLED.load(Ordering::Relaxed) {
        return true;
    }
    if !local_button_is_down() {
        clear_drag_edge_press();
        return true;
    }

    let Ok(mut since) = DRAG_EDGE_PRESS_SINCE.lock() else {
        // Poisoned lock: fail open rather than wedging the cursor locally.
        return true;
    };
    let hold = Duration::from_millis(DRAG_CROSSING_HOLD.load(Ordering::Relaxed));
    match *since {
        Some(started) => started.elapsed() >= hold,
        None => {
            *since = Some(Instant::now());
            false
        }
    }
}

fn update_remote_button_mask(mask: &AtomicU64, button: MouseButton, down: bool) {
    let bit = mouse_button_mask(button);
    if down {
        mask.fetch_or(bit, Ordering::Relaxed);
    } else {
        mask.fetch_and(!bit, Ordering::Relaxed);
    }
}

fn reset_remote_button_mask(mask: &AtomicU64) {
    mask.store(0, Ordering::Relaxed);
}

/// Sends button-up for every mouse button still marked down on the remote, then
/// clears the mask. Prevents a button getting stuck pressed on the controlled
/// machine when the cursor leaves mid-drag.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn release_remote_buttons(
    quic_transport: &quic_transport::TransportHandle,
    target: &InputTarget,
    mask: &AtomicU64,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) {
    let bits = mask.swap(0, Ordering::Relaxed);
    for (bit, button) in [
        (LEFT_BUTTON_MASK, MouseButton::Left),
        (RIGHT_BUTTON_MASK, MouseButton::Right),
        (MIDDLE_BUTTON_MASK, MouseButton::Middle),
    ] {
        if bits & bit != 0 {
            send_packet(
                quic_transport,
                target,
                InputEvent::MouseButton {
                    button,
                    down: false,
                },
                layout_state,
                input_events,
            );
        }
    }
}

/// Releases everything we are currently holding down on the remote — forwarded
/// modifier keys and mouse buttons — so crossing back to the local machine can
/// never leave a stuck Ctrl/Cmd/Shift or pressed button on the controlled side.
#[cfg(target_os = "macos")]
fn release_held_remote_inputs_macos(context: &MacCaptureContext, target: &InputTarget) {
    let held = context
        .pressed_modifiers
        .lock()
        .map(|modifiers| modifiers.clone())
        .unwrap_or_default();
    for key_code in held {
        send_packet(
            &context.quic_transport,
            target,
            InputEvent::Key {
                key_code,
                down: false,
            },
            &context.layout_state,
            &context.input_events,
        );
    }
    if let Ok(mut modifiers) = context.pressed_modifiers.lock() {
        modifiers.clear();
    }
    let held_keys = context
        .pressed_keys
        .lock()
        .map(|keys| keys.clone())
        .unwrap_or_default();
    for key_code in held_keys {
        send_packet(
            &context.quic_transport,
            target,
            InputEvent::Key {
                key_code,
                down: false,
            },
            &context.layout_state,
            &context.input_events,
        );
    }
    if let Ok(mut pressed) = context.pressed_keys.lock() {
        pressed.clear();
    }
    release_remote_buttons(
        &context.quic_transport,
        target,
        &context.remote_button_mask,
        &context.layout_state,
        &context.input_events,
    );
}

pub fn clear_clipboard_target(target: &Arc<Mutex<Option<ClipboardTarget>>>) {
    if let Ok(mut target) = target.lock() {
        *target = None;
    }
}

pub fn current_clipboard_target(
    target: &Arc<Mutex<Option<ClipboardTarget>>>,
) -> Option<ClipboardTarget> {
    let Ok(mut target) = target.lock() else {
        return None;
    };
    if target
        .as_ref()
        .and_then(|target| target.expires_at)
        .map(|expires_at| Instant::now() >= expires_at)
        .unwrap_or(false)
    {
        *target = None;
        return None;
    }

    target.clone()
}

fn set_clipboard_target(
    target: &Arc<Mutex<Option<ClipboardTarget>>>,
    device_id: String,
    addr: String,
    transport_public_key: String,
    protocol_version: u16,
    cluster_id: String,
    pair_secret: String,
    expires_in: Option<Duration>,
) {
    if let Ok(mut target) = target.lock() {
        *target = Some(ClipboardTarget {
            device_id,
            addr,
            transport_public_key,
            protocol_version,
            cluster_id,
            pair_secret,
            expires_at: expires_in.map(|duration| Instant::now() + duration),
        });
    }
}

fn set_control_clipboard_target(
    target: &Arc<Mutex<Option<ClipboardTarget>>>,
    active: &ActiveTarget,
    layout_state: &Arc<Mutex<LayoutState>>,
) {
    let Ok(layout) = layout_state.lock() else {
        return;
    };
    let Some(device) = layout
        .devices
        .iter()
        .find(|device| device.id == active.target.device_id && device.online && device.input_ready)
    else {
        return;
    };

    set_clipboard_target(
        target,
        active.target.device_id.clone(),
        format!(
            "{}:{}",
            device.host,
            normalize_quic_port(device.transport_port, device.quic_port)
        ),
        device.transport_public_key.clone(),
        device.protocol_version,
        layout.cluster_id.clone(),
        layout.pair_secret.clone(),
        None,
    );
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn windows_mouse_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, MSLLHOOKSTRUCT, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
        WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_XBUTTONDOWN,
        WM_XBUTTONUP,
    };

    if code < 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let Some(context) = windows_capture_context() else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    };
    if !cached_windows_input_desktop_is_default() {
        release_windows_remote_control(&context, true);
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let event = unsafe { *(lparam as *const MSLLHOOKSTRUCT) };
    let message = wparam as u32;

    // Track physical button state before anything else, and regardless of
    // whether the remote is active. The drag gate needs to know a button is
    // held while the cursor is still local — that is exactly the window-drag
    // case it exists to protect.
    if let Some((button, down)) = windows_message_to_button(message, event.mouseData) {
        set_local_button(button, down);
    }

    let handled = match message {
        WM_MOUSEMOVE => handle_windows_mouse_move(&context, event.pt.x as f64, event.pt.y as f64),
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            // For the X (side) buttons the pressed button rides the high word of
            // mouseData (XBUTTON1 = back, XBUTTON2 = forward); other buttons
            // ignore it.
            handle_windows_mouse_button(&context, message, event.mouseData)
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => handle_windows_scroll(&context, message, event.mouseData),
        _ => false,
    };

    if handled {
        1
    } else {
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }
}

#[cfg(target_os = "windows")]
unsafe extern "system" fn windows_keyboard_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, KBDLLHOOKSTRUCT, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    if code < 0 {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let Some(context) = windows_capture_context() else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    };
    if !cached_windows_input_desktop_is_default() {
        release_windows_remote_control(&context, true);
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    }

    let message = wparam as u32;

    let active = context
        .active
        .lock()
        .ok()
        .and_then(|active| active.as_ref().map(|active| active.target.clone()));
    let Some(target) = active else {
        return unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) };
    };

    if matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP) {
        let event = unsafe { *(lparam as *const KBDLLHOOKSTRUCT) };
        let key_code = event.vkCode as u16;
        let down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
        if down && windows_event_matches_screen_switch_hotkey(&context, key_code) {
            log::info!("screen switch hotkey returning to local from keyboard hook");
            release_windows_remote_control(&context, false);
            return 1;
        }
        if send_packet(
            &context.quic_transport,
            &target,
            InputEvent::Key { key_code, down },
            &context.layout_state,
            &context.input_events,
        ) {
            track_forwarded_key(&context.pressed_keys, key_code, down);
            return 1;
        }
    }

    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[cfg(target_os = "windows")]
fn windows_event_matches_screen_switch_hotkey(
    context: &WindowsCaptureContext,
    key_code: u16,
) -> bool {
    screen_switch_hotkey_matches_vk(
        &context.layout_state,
        key_code,
        windows_current_hotkey_modifiers(),
    )
}

#[cfg(target_os = "windows")]
fn windows_current_hotkey_modifiers() -> HotkeyModifiers {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
    };

    fn down(vk: u16) -> bool {
        unsafe { (GetAsyncKeyState(vk as i32) as u16 & 0x8000) != 0 }
    }

    HotkeyModifiers {
        ctrl: down(VK_CONTROL),
        alt: down(VK_MENU),
        shift: down(VK_SHIFT),
        meta: down(VK_LWIN) || down(VK_RWIN),
    }
}

/// Remembers which keys we have forwarded as pressed so they can be released if
/// the cursor returns to the local machine while a key is still held.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn track_forwarded_key(pressed: &Mutex<Vec<u16>>, key_code: u16, down: bool) {
    if let Ok(mut pressed) = pressed.lock() {
        if down {
            if !pressed.contains(&key_code) {
                pressed.push(key_code);
            }
        } else {
            pressed.retain(|code| *code != key_code);
        }
    }
}

/// Sends key-up for every key still marked pressed on the remote, then clears
/// the set. Stops a held Ctrl/Alt/Shift from sticking on the controlled machine
/// after the cursor crosses back.
#[cfg(target_os = "windows")]
fn release_forwarded_keys_windows(context: &WindowsCaptureContext, target: &InputTarget) {
    let held = context
        .pressed_keys
        .lock()
        .map(|pressed| pressed.clone())
        .unwrap_or_default();
    for key_code in held {
        send_packet(
            &context.quic_transport,
            target,
            InputEvent::Key {
                key_code,
                down: false,
            },
            &context.layout_state,
            &context.input_events,
        );
    }
    if let Ok(mut pressed) = context.pressed_keys.lock() {
        pressed.clear();
    }
}

#[cfg(target_os = "windows")]
fn release_windows_remote_control(context: &WindowsCaptureContext, clear_clipboard: bool) {
    let target = context
        .active
        .lock()
        .ok()
        .and_then(|mut active| active.take().map(|active| active.target));

    if let Some(target) = target {
        release_forwarded_keys_windows(context, &target);
        release_remote_buttons(
            &context.quic_transport,
            &target,
            &context.remote_button_mask,
            &context.layout_state,
            &context.input_events,
        );
    } else {
        reset_remote_button_mask(&context.remote_button_mask);
        if let Ok(mut pressed) = context.pressed_keys.lock() {
            pressed.clear();
        }
    }

    context.remote_active.store(false, Ordering::Relaxed);
    context.just_crossed.store(false, Ordering::Relaxed);
    reset_mouse_move_timer(&context.last_mouse_move_sent);
    show_windows_cursor_if_needed(context);
    if let Ok(mut anchor) = context.anchor.lock() {
        *anchor = None;
    }
    if let Ok(mut last_point) = context.last_point.lock() {
        *last_point = None;
    }
    if clear_clipboard {
        clear_clipboard_target(&context.clipboard_target);
    }
}

#[cfg(target_os = "windows")]
fn cached_windows_input_desktop_is_default() -> bool {
    WINDOWS_INPUT_DESKTOP_DEFAULT_CACHE.load(Ordering::Relaxed)
}

#[cfg(target_os = "windows")]
fn refresh_windows_input_desktop_cache() -> bool {
    let value = windows_input_desktop_is_default();
    WINDOWS_INPUT_DESKTOP_DEFAULT_CACHE.store(value, Ordering::Relaxed);
    value
}

#[cfg(target_os = "windows")]
fn windows_input_desktop_is_default() -> bool {
    use windows_sys::Win32::System::StationsAndDesktops::{
        CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_READOBJECTS, UOI_NAME,
    };

    unsafe {
        let desktop = OpenInputDesktop(0, 0, DESKTOP_READOBJECTS);
        if desktop.is_null() {
            return false;
        }

        let mut needed = 0_u32;
        let mut buffer = [0_u16; 256];
        let ok = GetUserObjectInformationW(
            desktop as _,
            UOI_NAME,
            buffer.as_mut_ptr() as *mut _,
            (buffer.len() * std::mem::size_of::<u16>()) as u32,
            &mut needed,
        ) != 0;
        let _ = CloseDesktop(desktop);

        if !ok || needed == 0 {
            return false;
        }

        let mut units = ((needed as usize) / std::mem::size_of::<u16>()).min(buffer.len());
        if units > 0 && buffer[units - 1] == 0 {
            units -= 1;
        }
        let name = String::from_utf16_lossy(&buffer[..units]);

        name.eq_ignore_ascii_case("default")
    }
}

#[cfg(target_os = "windows")]
fn handle_windows_mouse_move(context: &WindowsCaptureContext, x: f64, y: f64) -> bool {
    let mut active = match context.active.lock() {
        Ok(active) => active,
        Err(_) => return false,
    };

    if let Some(active_target) = active.as_mut() {
        let anchor = context
            .anchor
            .lock()
            .ok()
            .and_then(|anchor| *anchor)
            .unwrap_or((x, y));
        let dx = x - anchor.0;
        let dy = y - anchor.1;

        if dx.abs() < 0.1 && dy.abs() < 0.1 {
            return true;
        }

        if context.just_crossed.swap(false, Ordering::Relaxed) {
            // First real movement after crossing carries the flick's residual
            // velocity; re-pin to the anchor and swallow it so the cursor stays
            // at the entry edge instead of darting inward.
            set_windows_cursor(anchor.0.round() as i32, anchor.1.round() as i32);
            return true;
        }

        let (smoothed_dx, smoothed_dy) = smooth_mouse_delta(dx, dy);
        active_target.x += smoothed_dx;
        active_target.y += smoothed_dy;

        if update_active_remote_screen(active_target, dx, dy, &context.layout_state) {
            let point = local_return_point(active_target);
            let target = active_target.target.clone();
            // Control is returning to the local machine: park the controlled
            // cursor in a corner so it doesn't visibly linger at the shared edge.
            let _ = send_remote_cursor_park(
                &context.quic_transport,
                active_target,
                &context.layout_state,
                &context.input_events,
            );
            *active = None;
            context.remote_active.store(false, Ordering::Relaxed);
            // Keep the clipboard peer so copies still sync after returning.
            release_forwarded_keys_windows(context, &target);
            release_remote_buttons(
                &context.quic_transport,
                &target,
                &context.remote_button_mask,
                &context.layout_state,
                &context.input_events,
            );
            reset_mouse_move_timer(&context.last_mouse_move_sent);
            show_windows_cursor_if_needed(context);
            set_windows_cursor(point.0.round() as i32, point.1.round() as i32);
            if let Ok(mut anchor) = context.anchor.lock() {
                *anchor = None;
            }
            return true;
        }

        active_target.x = active_target
            .x
            .clamp(0.0, (active_target.current_screen.width - 1) as f64);
        active_target.y = active_target
            .y
            .clamp(0.0, (active_target.current_screen.height - 1) as f64);
        let dragging = remote_button_is_down(&context.remote_button_mask);
        if should_send_mouse_move(&context.last_mouse_move_sent, dragging) {
            if !send_remote_mouse_move(
                &context.quic_transport,
                active_target,
                &context.layout_state,
                &context.input_events,
            ) {
                *active = None;
                context.remote_active.store(false, Ordering::Relaxed);
                clear_clipboard_target(&context.clipboard_target);
                reset_mouse_move_timer(&context.last_mouse_move_sent);
                reset_remote_button_mask(&context.remote_button_mask);
                if let Ok(mut pressed) = context.pressed_keys.lock() {
                    pressed.clear();
                }
                show_windows_cursor_if_needed(context);
                if let Ok(mut anchor) = context.anchor.lock() {
                    *anchor = None;
                }
                return false;
            }
        }
        hide_windows_cursor_if_needed(context);
        set_windows_cursor(anchor.0.round() as i32, anchor.1.round() as i32);
        return true;
    }

    let previous = context
        .last_point
        .lock()
        .ok()
        .and_then(|last_point| *last_point);
    let (dx, dy) = previous
        .map(|point| (x - point.0, y - point.1))
        .unwrap_or((0.0, 0.0));

    if let Ok(mut last_point) = context.last_point.lock() {
        *last_point = Some((x, y));
    }

    // A paused-scene app (a whitelisted game, or any fullscreen window when no
    // whitelist is configured) owns the screen: never pull the pointer away.
    if scene_pause_active() {
        return false;
    }

    let targets = current_input_targets(&context.layout_state, &context.native_layout);
    if let Some(active_target) = crossing_target(&targets, x, y, dx, dy) {
        // Dragging into a snap zone must not hand the cursor over. Only a
        // sustained shove past the edge counts as a deliberate cross.
        if !drag_crossing_allowed() {
            return false;
        }
        let anchor = local_anchor_point(&active_target);
        hide_windows_cursor_if_needed(context);
        set_windows_cursor(anchor.0.round() as i32, anchor.1.round() as i32);
        if !send_remote_mouse_move(
            &context.quic_transport,
            &active_target,
            &context.layout_state,
            &context.input_events,
        ) {
            reset_mouse_move_timer(&context.last_mouse_move_sent);
            reset_remote_button_mask(&context.remote_button_mask);
            show_windows_cursor_if_needed(context);
            return false;
        }
        mark_mouse_move_sent(&context.last_mouse_move_sent);
        reset_remote_button_mask(&context.remote_button_mask);
        context.remote_active.store(true, Ordering::Relaxed);
        set_control_clipboard_target(
            &context.clipboard_target,
            &active_target,
            &context.layout_state,
        );
        *active = Some(active_target);
        if let Ok(mut anchor_state) = context.anchor.lock() {
            *anchor_state = Some(anchor);
        }
        context.just_crossed.store(true, Ordering::Relaxed);
        return true;
    }

    false
}

/// Maps a low-level mouse message to the button it carries. `WM_XBUTTON*`
/// packs which side button in the high word of `mouseData`.
#[cfg(target_os = "windows")]
fn windows_message_to_button(message: u32, mouse_data: u32) -> Option<(MouseButton, bool)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP,
        WM_XBUTTONDOWN, WM_XBUTTONUP, XBUTTON1,
    };

    let x_button = if (mouse_data >> 16) as u16 == XBUTTON1 as u16 {
        MouseButton::Back
    } else {
        MouseButton::Forward
    };
    Some(match message {
        WM_LBUTTONDOWN => (MouseButton::Left, true),
        WM_LBUTTONUP => (MouseButton::Left, false),
        WM_RBUTTONDOWN => (MouseButton::Right, true),
        WM_RBUTTONUP => (MouseButton::Right, false),
        WM_MBUTTONDOWN => (MouseButton::Middle, true),
        WM_MBUTTONUP => (MouseButton::Middle, false),
        WM_XBUTTONDOWN => (x_button, true),
        WM_XBUTTONUP => (x_button, false),
        _ => return None,
    })
}

#[cfg(target_os = "windows")]
fn handle_windows_mouse_button(context: &WindowsCaptureContext, message: u32, mouse_data: u32) -> bool {
    let active = context
        .active
        .lock()
        .ok()
        .and_then(|active| active.as_ref().cloned());
    let Some(active_target) = active else {
        return false;
    };
    let Some((button, down)) = windows_message_to_button(message, mouse_data) else {
        return false;
    };

    if !send_remote_mouse_move(
        &context.quic_transport,
        &active_target,
        &context.layout_state,
        &context.input_events,
    ) {
        return false;
    }
    mark_mouse_move_sent(&context.last_mouse_move_sent);

    let sent = send_packet(
        &context.quic_transport,
        &active_target.target,
        InputEvent::MouseButton { button, down },
        &context.layout_state,
        &context.input_events,
    );
    if sent {
        update_remote_button_mask(&context.remote_button_mask, button, down);
    }
    sent
}

#[cfg(target_os = "windows")]
fn handle_windows_scroll(context: &WindowsCaptureContext, message: u32, mouse_data: u32) -> bool {
    use windows_sys::Win32::UI::WindowsAndMessaging::{WM_MOUSEHWHEEL, WM_MOUSEWHEEL};

    let active = context
        .active
        .lock()
        .ok()
        .and_then(|active| active.as_ref().cloned());
    let Some(active_target) = active else {
        return false;
    };
    let delta = ((mouse_data >> 16) as i16 / 120) as i32;
    let (delta_x, delta_y) = if message == WM_MOUSEHWHEEL {
        (delta, 0)
    } else if message == WM_MOUSEWHEEL {
        (0, delta)
    } else {
        return false;
    };

    if !send_remote_mouse_move(
        &context.quic_transport,
        &active_target,
        &context.layout_state,
        &context.input_events,
    ) {
        return false;
    }
    mark_mouse_move_sent(&context.last_mouse_move_sent);

    send_packet(
        &context.quic_transport,
        &active_target.target,
        InputEvent::Scroll { delta_x, delta_y },
        &context.layout_state,
        &context.input_events,
    )
}

#[cfg(target_os = "windows")]
fn set_windows_cursor(x: i32, y: i32) {
    unsafe {
        let _ = windows_sys::Win32::UI::WindowsAndMessaging::SetCursorPos(x, y);
    }
}

#[cfg(target_os = "windows")]
fn windows_current_cursor_point() -> Option<(f64, f64)> {
    use windows_sys::Win32::{Foundation::POINT, UI::WindowsAndMessaging::GetCursorPos};

    unsafe {
        let mut point = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut point) == 0 {
            return None;
        }
        Some((point.x as f64, point.y as f64))
    }
}

#[cfg(target_os = "windows")]
fn hide_windows_cursor_if_needed(context: &WindowsCaptureContext) {
    let Ok(mut calls) = context.cursor_hide_calls.lock() else {
        return;
    };
    if *calls != 0 {
        return;
    }

    for _ in 0..8 {
        let count = unsafe { windows_sys::Win32::UI::WindowsAndMessaging::ShowCursor(0) };
        *calls += 1;
        if count < 0 {
            break;
        }
    }
}

#[cfg(target_os = "windows")]
fn show_windows_cursor_if_needed(context: &WindowsCaptureContext) {
    let Ok(mut calls) = context.cursor_hide_calls.lock() else {
        return;
    };

    for _ in 0..*calls {
        unsafe {
            let _ = windows_sys::Win32::UI::WindowsAndMessaging::ShowCursor(1);
        }
    }
    *calls = 0;
}

#[cfg(target_os = "macos")]
fn send_macos_mouse_button(
    context: &MacCaptureContext,
    active_target: &ActiveTarget,
    button: MouseButton,
    down: bool,
) -> bool {
    if !send_remote_mouse_move(
        &context.quic_transport,
        active_target,
        &context.layout_state,
        &context.input_events,
    ) {
        return false;
    }
    mark_mouse_move_sent(&context.last_mouse_move_sent);

    let sent = send_packet(
        &context.quic_transport,
        &active_target.target,
        InputEvent::MouseButton { button, down },
        &context.layout_state,
        &context.input_events,
    );
    if sent {
        update_remote_button_mask(&context.remote_button_mask, button, down);
    }
    sent
}

#[cfg(target_os = "macos")]
fn handle_macos_event(
    context: &MacCaptureContext,
    event_type: core_graphics::event::CGEventType,
    event: &core_graphics::event::CGEvent,
) -> core_graphics::event::CallbackResult {
    use core_graphics::event::{CGEventType, CallbackResult, EventField};

    if matches!(
        event_type,
        CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput
    ) {
        // Flag for the run-loop thread to re-enable; the cursor and remote state
        // are reset there too so we don't get stuck mid-control.
        context.tap_disabled.store(true, Ordering::Relaxed);
        log::info!(
            "[diag] event tap disabled by {:?} — mouse/key events are now DROPPED until re-enabled",
            event_type
        );
        return CallbackResult::Keep;
    }

    // Skip events MyKVM itself injects on behalf of the remote peer. They are
    // posted back through this same HID capture tap and must reach the local
    // apps — not be forwarded to the peer or dropped. Without this guard the
    // Mac-side capture tap (which forwards local input to the other machine)
    // catches the injected Win->Mac wheel/keys and bounces them or swallows
    // them, so the controlled Mac never scrolls/types. Mouse/key/scroll
    // injectors all stamp `macos_scroll::SELF_TAG` via `tag_own_event`.
    if event.get_integer_value_field(EventField::EVENT_SOURCE_USER_DATA)
        == macos_scroll::SELF_TAG
    {
        return CallbackResult::Keep;
    }

    // Track physical button state up front, before the active-target check
    // below can early-return. Window drags happen while the cursor is still
    // local, so this must run whether or not the remote is engaged.
    match event_type {
        CGEventType::LeftMouseDown => set_local_button(MouseButton::Left, true),
        CGEventType::LeftMouseUp => set_local_button(MouseButton::Left, false),
        CGEventType::RightMouseDown => set_local_button(MouseButton::Right, true),
        CGEventType::RightMouseUp => set_local_button(MouseButton::Right, false),
        CGEventType::OtherMouseDown => set_local_button(MouseButton::Middle, true),
        CGEventType::OtherMouseUp => set_local_button(MouseButton::Middle, false),
        _ => {}
    }

    let dx = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_X) as f64;
    let dy = event.get_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y) as f64;

    if matches!(
        event_type,
        CGEventType::MouseMoved
            | CGEventType::LeftMouseDragged
            | CGEventType::RightMouseDragged
            | CGEventType::OtherMouseDragged
    ) {
        return handle_macos_mouse_move(context, event, dx, dy);
    }

    let Ok(active) = context.active.lock() else {
        return CallbackResult::Keep;
    };
    let Some(active_target) = active.as_ref().cloned() else {
        drop(active);
        return handle_macos_modifier_event(context, event_type, event);
    };
    drop(active);
    // Paused scene while the remote still holds control: this happens when the
    // user ⌘-Tabs into a whitelisted app without moving the mouse, so the
    // mouse-move guard has not run yet. Release here as well, otherwise the
    // local cursor stays hidden/decoupled and keystrokes keep going remote.
    if scene_pause_active() {
        log::info!("scene pause active — returning control to local");
        return_to_local_macos(context);
        return CallbackResult::Keep;
    }
    let target = active_target.target.clone();

    let sent = match event_type {
        CGEventType::LeftMouseDown => {
            send_macos_mouse_button(context, &active_target, MouseButton::Left, true)
        }
        CGEventType::LeftMouseUp => {
            send_macos_mouse_button(context, &active_target, MouseButton::Left, false)
        }
        CGEventType::RightMouseDown => {
            send_macos_mouse_button(context, &active_target, MouseButton::Right, true)
        }
        CGEventType::RightMouseUp => {
            send_macos_mouse_button(context, &active_target, MouseButton::Right, false)
        }
        CGEventType::OtherMouseDown => {
            send_macos_mouse_button(context, &active_target, MouseButton::Middle, true)
        }
        CGEventType::OtherMouseUp => {
            send_macos_mouse_button(context, &active_target, MouseButton::Middle, false)
        }
        CGEventType::ScrollWheel => {
            let delta_y =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_1) as i32;
            let delta_x =
                event.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_DELTA_AXIS_2) as i32;
            if !send_remote_mouse_move(
                &context.quic_transport,
                &active_target,
                &context.layout_state,
                &context.input_events,
            ) {
                repin_macos_cursor_while_remote(context);
                return CallbackResult::Drop;
            }
            mark_mouse_move_sent(&context.last_mouse_move_sent);
            send_packet(
                &context.quic_transport,
                &target,
                InputEvent::Scroll { delta_x, delta_y },
                &context.layout_state,
                &context.input_events,
            )
        }
        CGEventType::KeyDown | CGEventType::KeyUp => {
            if matches!(event_type, CGEventType::KeyDown)
                && macos_event_matches_screen_switch_hotkey(context, event)
            {
                log::info!("screen switch hotkey returning to local from input tap");
                return_to_local_macos(context);
                return CallbackResult::Drop;
            }
            let mac_code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            if let Some(key_code) = mac_key_to_windows_vk(mac_code) {
                let down = matches!(event_type, CGEventType::KeyDown);
                let sent = send_packet(
                    &context.quic_transport,
                    &target,
                    InputEvent::Key { key_code, down },
                    &context.layout_state,
                    &context.input_events,
                );
                if sent {
                    track_forwarded_key(&context.pressed_keys, key_code, down);
                }
                sent
            } else {
                false
            }
        }
        CGEventType::FlagsChanged => {
            send_modifier_changes(context, &target, event);
            true
        }
        _ => false,
    };

    repin_macos_cursor_while_remote(context);
    if !sent {
        log::debug!(
            "remote-active local event {:?} was dropped after remote send miss",
            event_type
        );
    }
    CallbackResult::Drop
}

#[cfg(target_os = "macos")]
fn macos_event_matches_screen_switch_hotkey(
    context: &MacCaptureContext,
    event: &core_graphics::event::CGEvent,
) -> bool {
    use core_graphics::event::{CGEventFlags, EventField};

    let mac_code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    let Some(key_code) = mac_key_to_windows_vk(mac_code) else {
        return false;
    };
    let flags = event.get_flags();
    let modifiers = HotkeyModifiers {
        ctrl: flags.contains(CGEventFlags::CGEventFlagControl),
        alt: flags.contains(CGEventFlags::CGEventFlagAlternate),
        shift: flags.contains(CGEventFlags::CGEventFlagShift),
        meta: flags.contains(CGEventFlags::CGEventFlagCommand),
    };

    screen_switch_hotkey_matches_vk(&context.layout_state, key_code, modifiers)
}

#[cfg(target_os = "macos")]
fn handle_macos_mouse_move(
    context: &MacCaptureContext,
    event: &core_graphics::event::CGEvent,
    dx: f64,
    dy: f64,
) -> core_graphics::event::CallbackResult {
    use core_graphics::{event::CallbackResult, geometry::CGPoint};

    let location = event.location();
    // A paused scene (whitelisted game, or any fullscreen window when no
    // whitelist is configured) owns the screen. Hand control back to the local
    // machine before touching anything else, mirroring the Windows hot path —
    // the watcher thread only raises the flag, the release has to happen here
    // because macOS keeps no global capture context to release from.
    if scene_pause_active() {
        let holds_remote = context
            .active
            .lock()
            .map(|active| active.is_some())
            .unwrap_or(false);
        if holds_remote {
            log::info!("scene pause active — returning control to local");
            return_to_local_macos(context);
        }
        return CallbackResult::Keep;
    }
    if let Ok(mut active) = context.active.lock() {
        if let Some(active_target) = active.as_mut() {
            let dy = if active_target.invert_y { -dy } else { dy };
            if context
                .suppress_next_mouse_delta
                .swap(false, Ordering::Relaxed)
            {
                repin_macos_cursor_if_drifted(context, location);
                return CallbackResult::Drop;
            }
            if context.just_crossed.swap(false, Ordering::Relaxed)
                && should_ignore_initial_anchor_warp_delta(active_target.target.edge, dx, dy)
            {
                return CallbackResult::Drop;
            }
            let (smoothed_dx, smoothed_dy) = smooth_mouse_delta(dx, dy);
            active_target.x += smoothed_dx;
            active_target.y += smoothed_dy;

            if update_active_remote_screen(active_target, dx, dy, &context.layout_state) {
                let point = local_return_point(active_target);
                let invert_y = active_target.invert_y;
                let target = active_target.target.clone();
                // Control is returning to the local machine: park the controlled
                // cursor in a corner so it doesn't visibly linger at the shared
                // edge of the controlled (client) screen.
                let _ = send_remote_cursor_park(
                    &context.quic_transport,
                    active_target,
                    &context.layout_state,
                    &context.input_events,
                );
                *active = None;
                context.remote_active.store(false, Ordering::Relaxed);
                context.just_crossed.store(false, Ordering::Relaxed);
                context
                    .suppress_next_mouse_delta
                    .store(false, Ordering::Relaxed);
                // Record the return instant so the crossing test can enforce a
                // short cooldown — without it, landing flush on the edge (inset
                // 0) would let a fast back-flick immediately re-cross.
                if let Ok(mut last_return) = context.last_return.lock() {
                    *last_return = Some(Instant::now());
                }
                // Keep the clipboard peer so copies still sync after returning.
                release_held_remote_inputs_macos(context, &target);
                reset_mouse_move_timer(&context.last_mouse_move_sent);
                reset_cursor_repin_timer(context);
                if let Ok(mut anchor) = context.anchor.lock() {
                    *anchor = None;
                }
                let point = mac_cursor_point(context, point, invert_y);
                // Smooth slide-back: drop the post-warp local-events suppression
                // for just this final warp so the local pointer tracks the mouse
                // immediately instead of freezing for ~0.25s. Re-associating then
                // flushes any suppression still pending from the last re-pin, and
                // the default is restored right after so re-pins keep parking the
                // cursor on the next remote session (a persistent 0 makes the
                // server cursor follow the mouse while not frontmost).
                set_macos_warp_suppression_interval(0.0);
                move_macos_cursor_without_event(context, CGPoint::new(point.0, point.1));
                set_macos_cursor_decoupled(false);
                set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
                log::debug!("[diag] cross BACK to local — showing cursor now");
                show_macos_cursor_if_needed(context);
                return CallbackResult::Drop;
            }

            active_target.x = active_target
                .x
                .clamp(0.0, (active_target.current_screen.width - 1) as f64);
            active_target.y = active_target
                .y
                .clamp(0.0, (active_target.current_screen.height - 1) as f64);
            let dragging = remote_button_is_down(&context.remote_button_mask);
            if should_send_mouse_move(&context.last_mouse_move_sent, dragging) {
                if !send_remote_mouse_move(
                    &context.quic_transport,
                    active_target,
                    &context.layout_state,
                    &context.input_events,
                ) {
                    *active = None;
                    context.remote_active.store(false, Ordering::Relaxed);
                    context.just_crossed.store(false, Ordering::Relaxed);
                    context
                        .suppress_next_mouse_delta
                        .store(false, Ordering::Relaxed);
                    clear_clipboard_target(&context.clipboard_target);
                    reset_mouse_move_timer(&context.last_mouse_move_sent);
                    reset_cursor_repin_timer(context);
                    reset_remote_button_mask(&context.remote_button_mask);
                    if let Ok(mut modifiers) = context.pressed_modifiers.lock() {
                        modifiers.clear();
                    }
                    if let Ok(mut anchor) = context.anchor.lock() {
                        *anchor = None;
                    }
                    set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
                    set_macos_cursor_decoupled(false);
                    show_macos_cursor_if_needed(context);
                    return CallbackResult::Keep;
                }
            }
            if repin_macos_cursor_if_drifted(context, location)
                && !context.main_window_visible.load(Ordering::Relaxed)
            {
                reassert_macos_hidden_window_cursor(context, true);
            }
            // Re-pinning also runs from the capture run loop because mouse-move
            // callbacks can stop arriving once the pointer is over the client.
            return CallbackResult::Drop;
        }
    }

    let targets = current_input_targets(&context.layout_state, &context.native_layout);
    // Return cooldown: the cursor now lands flush on the edge (inset 0) on
    // return, so without this gate a fast back-flick immediately re-satisfies
    // the crossing test and bounces into the remote. Ignore crossings for a
    // short window after returning so the user's slide settles locally.
    if let Ok(last_return) = context.last_return.lock() {
        if let Some(when) = *last_return {
            if when.elapsed() < Duration::from_millis(RETURN_COOLDOWN_MS) {
                return CallbackResult::Keep;
            }
        }
    }
    if let Some(active_target) =
        mac_crossing_target(context, &targets, location.x, location.y, dx, dy)
    {
        // Dragging a window into a split-view / snap hot zone must not hand the
        // cursor to the remote. Only a sustained shove past the edge counts.
        if !drag_crossing_allowed() {
            return CallbackResult::Keep;
        }
        let anchor = mac_cursor_point(
            context,
            local_anchor_point(&active_target),
            active_target.invert_y,
        );
        set_macos_cursor_decoupled(true);
        set_macos_warp_suppression_interval(0.0);
        // Hide BEFORE the anchor warp: when MyKVM is hidden/minimized it runs as a
        // background process, and the WindowServer services a background process's
        // cursor-warp and cursor-hide calls lazily. If we warp first the user sees
        // the pointer flick to the screen edge and linger there until the delayed
        // hide lands — the "cursor sticks at the edge, hides late" stutter, whose
        // visible offset scales with flick speed. Hiding first means the pointer
        // vanishes where it is, then jumps to the anchor invisibly, so no edge
        // stick is ever visible regardless of scheduling latency.
        log::debug!("[diag] cross INTO remote — hiding+decoupling now");
        hide_macos_cursor_if_needed(context);
        move_macos_cursor_without_event(context, CGPoint::new(anchor.0, anchor.1));
        if !send_remote_mouse_move(
            &context.quic_transport,
            &active_target,
            &context.layout_state,
            &context.input_events,
        ) {
            reset_mouse_move_timer(&context.last_mouse_move_sent);
            reset_remote_button_mask(&context.remote_button_mask);
            reset_cursor_repin_timer(context);
            set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
            set_macos_cursor_decoupled(false);
            show_macos_cursor_if_needed(context);
            context.just_crossed.store(false, Ordering::Relaxed);
            return CallbackResult::Keep;
        }
        reset_mouse_move_timer(&context.last_mouse_move_sent);
        reset_cursor_repin_timer(context);
        reset_remote_button_mask(&context.remote_button_mask);
        context.remote_active.store(true, Ordering::Relaxed);
        set_control_clipboard_target(
            &context.clipboard_target,
            &active_target,
            &context.layout_state,
        );
        if let Ok(mut active) = context.active.lock() {
            *active = Some(active_target.clone());
        }
        if let Ok(mut anchor_state) = context.anchor.lock() {
            *anchor_state = Some(anchor);
        }
        context.just_crossed.store(true, Ordering::Relaxed);
        return CallbackResult::Drop;
    }

    CallbackResult::Keep
}

#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn crossing_target(
    targets: &[InputTarget],
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
) -> Option<ActiveTarget> {
    crossing_target_with_transform(targets, x, y, dx, dy, false)
}

fn crossing_target_with_transform(
    targets: &[InputTarget],
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
    invert_y: bool,
) -> Option<ActiveTarget> {
    targets
        .iter()
        .find_map(|target| {
            // No per-move online re-check here: build_input_targets only
            // emits online + input-ready devices and the target cache
            // refreshes within INPUT_TARGETS_TTL, so the check only re-took
            // the layout lock per target per event to learn the same answer.
            crossing_layout_point(target, x, y, dx, dy).map(|point| (target, point))
        })
        .map(|(target, (mapped_x, mapped_y))| {
            let entry_dx = dx * target.layout_local_screen.width.max(1) as f64
                / target.local_screen.width.max(1) as f64;
            let entry_dy = dy * target.layout_local_screen.height.max(1) as f64
                / target.local_screen.height.max(1) as f64;
            let remote_x = match target.edge {
                Edge::Right => 1.0 + entry_dx.max(0.0),
                Edge::Left => (target.remote_screen.width - 2) as f64 + entry_dx.min(0.0),
                _ => (mapped_x - target.remote_screen.x as f64)
                    .clamp(0.0, (target.remote_screen.width - 1) as f64),
            }
            .clamp(0.0, (target.remote_screen.width - 1) as f64);
            let remote_y = match target.edge {
                Edge::Bottom => 1.0 + entry_dy.max(0.0),
                Edge::Top => (target.remote_screen.height - 2) as f64 + entry_dy.min(0.0),
                _ => (mapped_y - target.remote_screen.y as f64)
                    .clamp(0.0, (target.remote_screen.height - 1) as f64),
            }
            .clamp(0.0, (target.remote_screen.height - 1) as f64);

            // The screen we cross into is the entry screen; carry it (with its
            // wire id) as the initial "current" screen so the cursor can later
            // roam onto the remote device's other screens.
            let mut current_screen = target.remote_screen.clone();
            current_screen.id = target.screen_id.clone();

            ActiveTarget {
                target: target.clone(),
                current_screen,
                current_screen_id: target.screen_id.clone(),
                x: remote_x,
                y: remote_y,
                invert_y,
            }
        })
}

fn crossing_layout_point(
    target: &InputTarget,
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
) -> Option<(f64, f64)> {
    if is_crossing_screen(&target.local_screen, target.edge, x, y, dx, dy) {
        return Some(native_to_layout_point(target, x, y));
    }

    let mapped = native_to_layout_point(target, x, y);
    let mapped_dx = dx * target.layout_local_screen.width.max(1) as f64
        / target.local_screen.width.max(1) as f64;
    let mapped_dy = dy * target.layout_local_screen.height.max(1) as f64
        / target.local_screen.height.max(1) as f64;
    if is_crossing_screen(
        &target.layout_local_screen,
        target.edge,
        mapped.0,
        mapped.1,
        mapped_dx,
        mapped_dy,
    ) {
        return Some(mapped);
    }

    None
}

fn native_to_layout_point(target: &InputTarget, x: f64, y: f64) -> (f64, f64) {
    let native = &target.local_screen;
    let layout = &target.layout_local_screen;
    let ratio_x = (x - native.x as f64) / native.width.max(1) as f64;
    let ratio_y = (y - native.y as f64) / native.height.max(1) as f64;

    (
        layout.x as f64 + ratio_x * layout.width.max(1) as f64,
        layout.y as f64 + ratio_y * layout.height.max(1) as f64,
    )
}

fn is_crossing_screen(screen: &Screen, edge: Edge, x: f64, y: f64, dx: f64, dy: f64) -> bool {
    let left = screen.x as f64;
    let right = (screen.x + screen.width) as f64;
    let top = screen.y as f64;
    let bottom = (screen.y + screen.height) as f64;
    let previous_x = x - dx;
    let previous_y = y - dy;

    // Require the previous reconstructed point to already be near the shared
    // edge. This still permits fast edge flicks, but rejects a single huge jump
    // from the middle of the screen that merely lands near the boundary.
    match edge {
        Edge::Right => {
            dx >= MIN_CROSSING_DELTA
                && dx.abs() >= dy.abs() * CROSSING_AXIS_DOMINANCE
                && previous_x >= right - CROSSING_ACTIVATION_BAND
                && x >= right - CROSSING_MARGIN
                && y >= top - CROSSING_MARGIN
                && y <= bottom + CROSSING_MARGIN
        }
        Edge::Left => {
            dx <= -MIN_CROSSING_DELTA
                && dx.abs() >= dy.abs() * CROSSING_AXIS_DOMINANCE
                && previous_x <= left + CROSSING_ACTIVATION_BAND
                && x <= left + CROSSING_MARGIN
                && y >= top - CROSSING_MARGIN
                && y <= bottom + CROSSING_MARGIN
        }
        Edge::Bottom => {
            dy >= MIN_CROSSING_DELTA
                && dy.abs() >= dx.abs() * CROSSING_AXIS_DOMINANCE
                && previous_y >= bottom - CROSSING_ACTIVATION_BAND
                && y >= bottom - CROSSING_MARGIN
                && x >= left - CROSSING_MARGIN
                && x <= right + CROSSING_MARGIN
        }
        Edge::Top => {
            dy <= -MIN_CROSSING_DELTA
                && dy.abs() >= dx.abs() * CROSSING_AXIS_DOMINANCE
                && previous_y <= top + CROSSING_ACTIVATION_BAND
                && y <= top + CROSSING_MARGIN
                && x >= left - CROSSING_MARGIN
                && x <= right + CROSSING_MARGIN
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn local_y_bounds(targets: &[InputTarget]) -> Option<(f64, f64)> {
    let mut min_y: Option<i32> = None;
    let mut max_y: Option<i32> = None;

    for target in targets {
        let top = target.local_screen.y;
        let bottom = target.local_screen.y + target.local_screen.height;
        min_y = Some(min_y.map_or(top, |current| current.min(top)));
        max_y = Some(max_y.map_or(bottom, |current| current.max(bottom)));
    }

    Some((min_y? as f64, max_y? as f64))
}

#[cfg(target_os = "macos")]
fn mac_crossing_target(
    context: &MacCaptureContext,
    targets: &[InputTarget],
    x: f64,
    y: f64,
    dx: f64,
    dy: f64,
) -> Option<ActiveTarget> {
    if let Some(target) =
        crossing_target_with_transform(targets, x, y, dx, dy, false)
    {
        return Some(target);
    }

    let Some((min_y, max_y)) = local_y_bounds(targets).or(context.local_y_bounds) else {
        return None;
    };
    let flipped_y = min_y + max_y - y;
    if (flipped_y - y).abs() < 0.5 {
        return None;
    }

    crossing_target_with_transform(targets, x, flipped_y, dx, -dy, true)
}

#[cfg(target_os = "macos")]
fn mac_cursor_point(context: &MacCaptureContext, point: (f64, f64), invert_y: bool) -> (f64, f64) {
    if !invert_y {
        return point;
    }

    local_y_bounds(&current_input_targets(
        &context.layout_state,
        &context.native_layout,
    ))
    .or(context.local_y_bounds)
    .map(|(min_y, max_y)| (point.0, min_y + max_y - point.1))
    .unwrap_or(point)
}

/// After a raw delta has been applied to `active.x`/`active.y`, reconcile which
/// remote screen the cursor is on. If it has crossed onto another screen of the
/// same remote device, switch to it so control roams across the remote's whole
/// desktop (e.g. onto a client's secondary monitor). Returns `true` when the
/// cursor has left the remote desktop back toward the local machine, in which
/// case the caller should hand control back.
fn update_active_remote_screen(
    active: &mut ActiveTarget,
    dx: f64,
    dy: f64,
    layout_state: &Arc<Mutex<LayoutState>>,
) -> bool {
    // Still within the screen we're already on: nothing to reconcile.
    if point_in_local_bounds(&active.current_screen, active.x, active.y) {
        return false;
    }

    let screens = layout_state
        .lock()
        .map(|layout| remote_device_screens(&layout, &active.target.device_id))
        .unwrap_or_default();

    // Position of the cursor in the remote device's shared layout space.
    let global_x = active.current_screen.x as f64 + active.x;
    let global_y = active.current_screen.y as f64 + active.y;

    // Roam onto an adjacent screen of the same device that holds this point.
    if let Some(screen) = screens.iter().find(|screen| {
        screen.id != active.current_screen.id && point_in_screen(screen, global_x, global_y)
    }) {
        active.x = global_x - screen.x as f64;
        active.y = global_y - screen.y as f64;
        active.current_screen_id = screen.id.clone();
        active.current_screen = screen.clone();
        return false;
    }

    // Off the edge with no neighbor there. Only the entry screen borders the
    // local machine, so only it can hand control back; every other outer edge
    // just clamps the cursor in place.
    let returned_to_local = active.current_screen_id == active.target.screen_id
        && exited_entry_edge(
            active.target.edge,
            &active.current_screen,
            active.x,
            active.y,
            dx,
            dy,
        );
    if returned_to_local {
        pin_active_to_entry_edge(active);
    }

    returned_to_local
}

fn should_ignore_initial_anchor_warp_delta(edge: Edge, dx: f64, dy: f64) -> bool {
    match edge {
        Edge::Right => dx <= -MIN_CROSSING_DELTA && dx.abs() >= dy.abs() * CROSSING_AXIS_DOMINANCE,
        Edge::Left => dx >= MIN_CROSSING_DELTA && dx.abs() >= dy.abs() * CROSSING_AXIS_DOMINANCE,
        Edge::Bottom => dy <= -MIN_CROSSING_DELTA && dy.abs() >= dx.abs() * CROSSING_AXIS_DOMINANCE,
        Edge::Top => dy >= MIN_CROSSING_DELTA && dy.abs() >= dx.abs() * CROSSING_AXIS_DOMINANCE,
    }
}

/// True when local coordinates `x`/`y` are inside `screen`'s bounds.
fn point_in_local_bounds(screen: &Screen, x: f64, y: f64) -> bool {
    x >= 0.0 && x <= (screen.width - 1) as f64 && y >= 0.0 && y <= (screen.height - 1) as f64
}

/// True when a point in shared layout space falls on `screen`.
fn point_in_screen(screen: &Screen, global_x: f64, global_y: f64) -> bool {
    global_x >= screen.x as f64
        && global_x <= (screen.x + screen.width - 1) as f64
        && global_y >= screen.y as f64
        && global_y <= (screen.y + screen.height - 1) as f64
}

/// Whether the cursor has crossed back over the edge it originally entered from
/// (the side bordering the local machine). Mirrors the classic single-screen
/// return-to-local test, applied to the entry screen.
fn exited_entry_edge(edge: Edge, screen: &Screen, x: f64, y: f64, dx: f64, dy: f64) -> bool {
    match edge {
        Edge::Right => x <= 0.0 && dx < 0.0,
        Edge::Left => x >= (screen.width - 1) as f64 && dx > 0.0,
        Edge::Bottom => y <= 0.0 && dy < 0.0,
        Edge::Top => y >= (screen.height - 1) as f64 && dy > 0.0,
    }
}

fn pin_active_to_entry_edge(active: &mut ActiveTarget) {
    active.x = active
        .x
        .clamp(0.0, (active.current_screen.width - 1) as f64);
    active.y = active
        .y
        .clamp(0.0, (active.current_screen.height - 1) as f64);

    match active.target.edge {
        Edge::Right => active.x = 0.0,
        Edge::Left => active.x = (active.current_screen.width - 1) as f64,
        Edge::Bottom => active.y = 0.0,
        Edge::Top => active.y = (active.current_screen.height - 1) as f64,
    }
}

/// The remote device's screens, each carrying the wire screen id that the
/// receiving side matches against (the device-prefixed layout id stripped back
/// to the peer's own screen id).
fn remote_device_screens(layout: &LayoutState, device_id: &str) -> Vec<Screen> {
    layout
        .devices
        .iter()
        .find(|device| device.id == device_id)
        .map(|device| {
            device
                .screens
                .iter()
                .map(|screen| {
                    let mut copy = screen.clone();
                    copy.id = peer_screen_id(device, screen);
                    copy
                })
                .collect()
        })
        .unwrap_or_default()
}

fn local_return_point(active: &ActiveTarget) -> (f64, f64) {
    let local = &active.target.local_screen;
    let layout_local = &active.target.layout_local_screen;
    let remote = &active.target.remote_screen;
    let global_x = remote.x as f64 + active.x;
    let global_y = remote.y as f64 + active.y;
    let ratio_x = (global_x - layout_local.x as f64) / layout_local.width.max(1) as f64;
    let ratio_y = (global_y - layout_local.y as f64) / layout_local.height.max(1) as f64;
    let native_x = local.x as f64 + ratio_x * local.width.max(1) as f64;
    let native_y = local.y as f64 + ratio_y * local.height.max(1) as f64;

    // Land the cursor flush on the entry edge (RETURN_EDGE_INSET=0) for a
    // seamless extended-display feel. A fast back-flick can no longer bounce
    // straight back into the remote because the crossing test is gated by a
    // time-based cooldown (RETURN_COOLDOWN_MS), not by inset distance.
    let inset = RETURN_EDGE_INSET.min((local.width.max(1) - 1) as f64 / 2.0);
    let inset_v = RETURN_EDGE_INSET.min((local.height.max(1) - 1) as f64 / 2.0);
    match active.target.edge {
        Edge::Right => (
            (local.x + local.width - 1) as f64 - inset,
            native_y.clamp(local.y as f64, (local.y + local.height - 1) as f64),
        ),
        Edge::Left => (
            local.x as f64 + inset,
            native_y.clamp(local.y as f64, (local.y + local.height - 1) as f64),
        ),
        Edge::Bottom => (
            native_x.clamp(local.x as f64, (local.x + local.width - 1) as f64),
            (local.y + local.height - 1) as f64 - inset_v,
        ),
        Edge::Top => (
            native_x.clamp(local.x as f64, (local.x + local.width - 1) as f64),
            local.y as f64 + inset_v,
        ),
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn local_center_point(active: &ActiveTarget) -> (f64, f64) {
    let local = &active.target.local_screen;
    (
        local.x as f64 + (local.width as f64 / 2.0).clamp(0.0, (local.width - 1).max(0) as f64),
        local.y as f64 + (local.height as f64 / 2.0).clamp(0.0, (local.height - 1).max(0) as f64),
    )
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn local_hotkey_return_point(
    active: &ActiveTarget,
    recorded_point: Option<(f64, f64)>,
) -> (f64, f64) {
    recorded_point.unwrap_or_else(|| local_center_point(active))
}

fn send_remote_mouse_move(
    quic_transport: &quic_transport::TransportHandle,
    active: &ActiveTarget,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) -> bool {
    send_packet(
        quic_transport,
        &active.target,
        InputEvent::MouseMove {
            screen_id: active.current_screen_id.clone(),
            x: active.x.round() as i32,
            y: active.y.round() as i32,
        },
        layout_state,
        input_events,
    )
}

fn local_anchor_point(active: &ActiveTarget) -> (f64, f64) {
    local_return_point(active)
}

/// When control returns to the local machine, tuck the controlled cursor out
/// of the way. True cursor hiding isn't reliably possible on the controlled
/// side, so tucking it is the seamless-feeling approximation.
///
/// Controlled macOS: park ON a clipping edge, at the end nearest this
/// machine. The arrow's hotspot is its top-left tip and the body extends
/// down-right, so only the RIGHT and BOTTOM screen edges clip it to a barely
/// visible sliver (why the old far-corner park "looked like a dot"); the left
/// and top edges show the full arrow. Corners themselves are avoided: macOS
/// hot corners fire on pointer position alone, so the far-corner park
/// triggered actions like "Show all applications" on every single handoff.
///
/// Controlled Windows (and anything else): keep the long-standing far-corner
/// park unchanged.
#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
fn send_remote_cursor_park(
    quic_transport: &quic_transport::TransportHandle,
    active: &ActiveTarget,
    layout_state: &Arc<Mutex<LayoutState>>,
    input_events: &Arc<AtomicU64>,
) -> bool {
    let (park_x, park_y) = remote_park_point(active);
    send_packet(
        quic_transport,
        &active.target,
        InputEvent::MouseMove {
            screen_id: active.current_screen_id.clone(),
            x: park_x,
            y: park_y,
        },
        layout_state,
        input_events,
    )
}

/// Distance a shared-edge park keeps from the screen corners, so an exit near
/// the top/bottom of the edge cannot land the parked cursor inside a macOS
/// hot-corner trip zone.
const PARK_CORNER_CLEARANCE: i32 = 64;

#[cfg_attr(not(any(target_os = "windows", target_os = "macos")), allow(dead_code))]
fn remote_park_point(active: &ActiveTarget) -> (i32, i32) {
    let width = active.current_screen.width;
    let height = active.current_screen.height;
    if !active.target.target_platform.eq_ignore_ascii_case("macos") {
        return ((width - 1).max(0), (height - 1).max(0));
    }

    // `edge` is the LOCAL screen edge crossed to enter the remote, so this
    // machine sits on the OPPOSITE side of the controlled screen. Pick the
    // clipping edge (right/bottom) closest to that side.
    let clear = |position: i32, extent: i32| {
        position.clamp(
            PARK_CORNER_CLEARANCE.min((extent / 2).max(0)),
            (extent - 1 - PARK_CORNER_CLEARANCE).max((extent / 2).max(0)),
        )
    };
    let x = active.x.round() as i32;
    let y = active.y.round() as i32;
    match active.target.edge {
        // This machine is west of the Mac: bottom edge, west end.
        Edge::Right => (clear(0, width), (height - 1).max(0)),
        // East: the east edge itself clips — keep the exit height.
        Edge::Left => ((width - 1).max(0), clear(y, height)),
        // North: east edge, north end.
        Edge::Bottom => ((width - 1).max(0), clear(0, height)),
        // South: bottom edge — keep the exit x.
        Edge::Top => (clear(x, width), (height - 1).max(0)),
    }
}

#[cfg(target_os = "macos")]
fn enter_remote_target_macos(context: &MacCaptureContext, active_target: ActiveTarget) {
    use core_graphics::geometry::CGPoint;

    let return_point = macos_current_cursor_location().map(|point| (point.x, point.y));
    let anchor = mac_cursor_point(
        context,
        local_anchor_point(&active_target),
        active_target.invert_y,
    );
    if !send_remote_mouse_move(
        &context.quic_transport,
        &active_target,
        &context.layout_state,
        &context.input_events,
    ) {
        reset_mouse_move_timer(&context.last_mouse_move_sent);
        reset_remote_button_mask(&context.remote_button_mask);
        reset_cursor_repin_timer(context);
        set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
        set_macos_cursor_decoupled(false);
        show_macos_cursor_if_needed(context);
        context.just_crossed.store(false, Ordering::Relaxed);
        context
            .suppress_next_mouse_delta
            .store(false, Ordering::Relaxed);
        if let Ok(mut hotkey_return_point) = context.hotkey_return_point.lock() {
            *hotkey_return_point = None;
        }
        return;
    }
    set_macos_cursor_decoupled(true);
    set_macos_warp_suppression_interval(0.0);
    hide_macos_cursor_if_needed(context);
    move_macos_cursor_without_event(context, CGPoint::new(anchor.0, anchor.1));
    reset_mouse_move_timer(&context.last_mouse_move_sent);
    reset_cursor_repin_timer(context);
    reset_remote_button_mask(&context.remote_button_mask);
    context.remote_active.store(true, Ordering::Relaxed);
    set_control_clipboard_target(
        &context.clipboard_target,
        &active_target,
        &context.layout_state,
    );
    if let Ok(mut active) = context.active.lock() {
        *active = Some(active_target);
    }
    if let Ok(mut anchor_state) = context.anchor.lock() {
        *anchor_state = Some(anchor);
    }
    if let Ok(mut hotkey_return_point) = context.hotkey_return_point.lock() {
        *hotkey_return_point = return_point;
    }
    // Hotkey entry lands at the remote screen centre. macOS can still emit one
    // synthetic delta from the local anchor warp; drop only that next delta.
    context.just_crossed.store(false, Ordering::Relaxed);
    context
        .suppress_next_mouse_delta
        .store(true, Ordering::Relaxed);
}

#[cfg(target_os = "macos")]
fn return_to_local_macos(context: &MacCaptureContext) {
    use core_graphics::geometry::CGPoint;

    let active_target = match context.active.lock().ok().and_then(|mut a| a.take()) {
        Some(target) => target,
        None => return,
    };
    let recorded_point = context
        .hotkey_return_point
        .lock()
        .ok()
        .and_then(|mut point| point.take());
    let point = local_hotkey_return_point(&active_target, recorded_point);
    let invert_y = active_target.invert_y;
    let target = active_target.target.clone();
    let _ = send_remote_cursor_park(
        &context.quic_transport,
        &active_target,
        &context.layout_state,
        &context.input_events,
    );
    context.remote_active.store(false, Ordering::Relaxed);
    context.just_crossed.store(false, Ordering::Relaxed);
    context
        .suppress_next_mouse_delta
        .store(false, Ordering::Relaxed);
    if let Ok(mut last_return) = context.last_return.lock() {
        *last_return = Some(Instant::now());
    }
    release_held_remote_inputs_macos(context, &target);
    reset_mouse_move_timer(&context.last_mouse_move_sent);
    reset_cursor_repin_timer(context);
    if let Ok(mut anchor) = context.anchor.lock() {
        *anchor = None;
    }
    let point = if recorded_point.is_some() {
        point
    } else {
        mac_cursor_point(context, point, invert_y)
    };
    set_macos_warp_suppression_interval(0.0);
    move_macos_cursor_without_event(context, CGPoint::new(point.0, point.1));
    set_macos_cursor_decoupled(false);
    set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
    show_macos_cursor_if_needed(context);
}

/// Re-assert cursor decouple + position lock while a remote session is active.
///
/// When MyKVM is backgrounded (the normal state while controlling a remote),
/// macOS can silently re-associate the physical mouse with the on-screen cursor
/// despite an earlier `CGAssociateMouseAndMouseCursorPosition(false)`. The
/// pointer then follows the mouse. Reuse the same drift-limited repin path used
/// by the mouse callback, because the callback can stop firing while the main
/// window is hidden. Do not repeatedly push hide/transparent cursor state here:
/// those APIs are stack-based and must stay one enter paired with one return.
#[cfg(target_os = "macos")]
fn repin_macos_cursor_while_remote(context: &MacCaptureContext) {
    set_macos_cursor_decoupled(true);
    if !context.main_window_visible.load(Ordering::Relaxed) {
        let drifted = if let Some(location) = macos_current_cursor_location() {
            repin_macos_cursor_if_drifted(context, location)
        } else {
            force_repin_macos_cursor_to_anchor(context);
            true
        };
        reassert_macos_hidden_window_cursor(context, drifted);
        return;
    }

    if let Some(location) = macos_current_cursor_location() {
        repin_macos_cursor_if_drifted(context, location);
    }
}

#[cfg(target_os = "macos")]
fn macos_capture_loop_ms(remote_active: bool, main_window_visible: bool) -> u64 {
    if !remote_active {
        return MACOS_IDLE_CAPTURE_LOOP_MS;
    }
    if main_window_visible {
        MACOS_VISIBLE_REMOTE_CAPTURE_LOOP_MS
    } else {
        MACOS_HIDDEN_REMOTE_CAPTURE_LOOP_MS
    }
}

/// Poll the shared switch-request slot and act on it. Called from the capture
/// loop on each iteration. Centralises the macOS enter/return side effects so
/// both the mouse-crossing path and the hotkey path stay in sync.
#[cfg(target_os = "macos")]
fn drain_switch_request_macos(context: &MacCaptureContext) {
    let direction = match context.switch_request.lock() {
        Ok(mut req) => req.take(),
        Err(_) => return,
    };
    let Some(direction) = direction else { return };
    // Paused scene in front: swallow the request rather than queueing it, so a
    // stray hotkey during a game does not fire the moment the game exits.
    if scene_pause_active() {
        log::debug!("screen switch {direction:?} ignored: scene pause active");
        return;
    }
    let current_point = macos_current_cursor_location().map(|point| (point.x, point.y));
    match request_screen_switch_from_point(
        direction,
        &context.layout_state,
        &context.native_layout,
        &context.active,
        current_point,
    ) {
        SwitchOutcome::Enter(active_target) => {
            log::info!(
                "screen switch entering device={}",
                active_target.target.device_id
            );
            enter_remote_target_macos(context, active_target);
        }
        SwitchOutcome::Return => {
            log::info!("screen switch returning to local");
            return_to_local_macos(context);
        }
        SwitchOutcome::LocalMove {
            from_screen_id,
            to_screen_id,
            x,
            y,
        } => {
            let (x, y) = remembered_local_screen_point(
                &context.local_screen_points,
                &from_screen_id,
                &to_screen_id,
                current_point,
                (x, y),
            );
            log::info!("screen switch moving local cursor to ({x:.0}, {y:.0})");
            set_macos_cursor_decoupled(false);
            set_macos_warp_suppression_interval(0.0);
            move_macos_cursor_without_event(context, core_graphics::geometry::CGPoint::new(x, y));
            set_macos_warp_suppression_interval(MACOS_DEFAULT_WARP_SUPPRESSION_SECS);
            show_macos_cursor_if_needed(context);
        }
        SwitchOutcome::Noop => {
            log::warn!("screen switch {direction:?} ignored: no matching online target");
        }
    }
}

#[cfg(target_os = "windows")]
fn drain_switch_request_windows(context: &WindowsCaptureContext) {
    let direction = match context.switch_request.lock() {
        Ok(mut req) => req.take(),
        Err(_) => return,
    };
    let Some(direction) = direction else { return };
    // Paused scene in front: swallow the request rather than queueing it, so a
    // stray hotkey during a game does not fire the moment the game exits.
    if scene_pause_active() {
        log::debug!("screen switch {direction:?} ignored: scene pause active");
        return;
    }
    let current_point = windows_current_cursor_point();
    match request_screen_switch_from_point(
        direction,
        &context.layout_state,
        &context.native_layout,
        &context.active,
        current_point,
    ) {
        SwitchOutcome::Enter(active_target) => {
            log::info!(
                "screen switch entering device={}",
                active_target.target.device_id
            );
            // Mirror the Windows mouse-crossing enter path. Hotkey entry has no
            // physical mouse position at the edge, so we explicitly pin to the
            // local anchor and start sending deltas from there.
            let anchor = local_anchor_point(&active_target);
            hide_windows_cursor_if_needed(context);
            set_windows_cursor(anchor.0.round() as i32, anchor.1.round() as i32);
            if send_remote_mouse_move(
                &context.quic_transport,
                &active_target,
                &context.layout_state,
                &context.input_events,
            ) {
                context.remote_active.store(true, Ordering::Relaxed);
                set_control_clipboard_target(
                    &context.clipboard_target,
                    &active_target,
                    &context.layout_state,
                );
                if let Ok(mut active) = context.active.lock() {
                    *active = Some(active_target);
                }
                if let Ok(mut anchor_state) = context.anchor.lock() {
                    *anchor_state = Some(anchor);
                }
                // Hotkey entry lands at the remote centre. The edge-crossing
                // first-delta guard would eat the user's first real movement.
                context.just_crossed.store(false, Ordering::Relaxed);
            } else {
                reset_mouse_move_timer(&context.last_mouse_move_sent);
                reset_remote_button_mask(&context.remote_button_mask);
                show_windows_cursor_if_needed(context);
            }
        }
        SwitchOutcome::Return => {
            log::info!("screen switch returning to local");
            release_windows_remote_control(context, false);
        }
        SwitchOutcome::LocalMove {
            from_screen_id,
            to_screen_id,
            x,
            y,
        } => {
            let (x, y) = remembered_local_screen_point(
                &context.local_screen_points,
                &from_screen_id,
                &to_screen_id,
                current_point,
                (x, y),
            );
            log::info!("screen switch moving local cursor to ({x:.0}, {y:.0})");
            set_windows_cursor(x.round() as i32, y.round() as i32);
        }
        SwitchOutcome::Noop => {
            log::warn!("screen switch {direction:?} ignored: no matching online target");
        }
    }
}

/// Disconnects (or reconnects) the on-screen cursor from the physical mouse.
/// While controlling a remote screen we decouple them: the mouse keeps emitting
/// HID deltas to our event tap, but the local cursor stays frozen, so we never
/// have to warp it back each event. Warping every move triggers macOS's
/// post-warp local-event suppression (~0.25s), which drops motion and makes the
/// remote cursor drift and stutter. Decoupling is how a real extended display
/// feels seamless. MUST be re-coupled on every exit path or the user's cursor
/// stays frozen.
#[cfg(target_os = "macos")]
fn set_macos_cursor_decoupled(decoupled: bool) {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGAssociateMouseAndMouseCursorPosition(connected: i32) -> i32;
    }

    let connected = if decoupled { 0 } else { 1 };
    unsafe {
        let _ = CGAssociateMouseAndMouseCursorPosition(connected);
    }
}

/// macOS default: local hardware events stay suppressed for 0.25s after a warp.
#[cfg(target_os = "macos")]
const MACOS_DEFAULT_WARP_SUPPRESSION_SECS: f64 = 0.25;

/// Set how long macOS suppresses local hardware mouse events after a cursor
/// warp (`CGWarpMouseCursorPosition` / `CGDisplayMoveCursorToPoint`).
///
/// This is process-wide. Keep it at `0` only while remote control is active so
/// macOS does not swallow hardware deltas after our anchor/re-pin warps, then
/// restore the default on every exit path.
#[cfg(target_os = "macos")]
fn set_macos_warp_suppression_interval(seconds: f64) {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGSetLocalEventsSuppressionInterval(seconds: f64) -> i32;
    }
    unsafe {
        let _ = CGSetLocalEventsSuppressionInterval(seconds);
    }
}

/// Opt the process out of macOS App Nap while input is being captured.
///
/// When MyKVM is not the frontmost app (another window is focused) or the
/// window is minimized, macOS throttles our background capture thread's run
/// loop and coalesces its timers. That throttling is exactly what makes the
/// cursor "stutter" when it slides back from a remote device: forwarded events
/// and cursor re-pinning fall behind, then catch up in a burst at the edge.
///
/// `NSProcessInfo -beginActivityWithOptions:reason:` with a latency-critical,
/// user-initiated activity tells the OS to keep us scheduled normally. We hold
/// the returned (retained) activity token for the whole capture lifetime and
/// end it on teardown. The option set still allows the machine to idle-sleep.
#[cfg(target_os = "macos")]
fn set_macos_app_nap_suppressed(suppress: bool) {
    use std::ffi::c_void;
    use std::os::raw::c_char;
    use std::sync::atomic::AtomicUsize;

    // Retained NSProcessInfo activity token (as usize) held between begin/end.
    // 0 means "no activity currently held".
    static ACTIVITY_TOKEN: AtomicUsize = AtomicUsize::new(0);

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    // NSActivityOptions, from <Foundation/NSProcessInfo.h>:
    //   NSActivityUserInitiatedAllowingIdleSystemSleep = 0x00EFFFFF
    //   NSActivityLatencyCritical                      = 0xFF00000000
    const NS_ACTIVITY_USER_INITIATED_ALLOWING_IDLE_SYSTEM_SLEEP: u64 = 0x00EF_FFFF;
    const NS_ACTIVITY_LATENCY_CRITICAL: u64 = 0xFF_0000_0000;

    unsafe {
        let process_info_class = objc_getClass(b"NSProcessInfo\0".as_ptr() as *const c_char);
        if process_info_class.is_null() {
            return;
        }
        let process_info_sel = sel_registerName(b"processInfo\0".as_ptr() as *const c_char);
        let shared: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let process_info = shared(process_info_class, process_info_sel);
        if process_info.is_null() {
            return;
        }

        if suppress {
            if ACTIVITY_TOKEN.load(Ordering::Relaxed) != 0 {
                return; // already suppressing
            }
            let string_class = objc_getClass(b"NSString\0".as_ptr() as *const c_char);
            let string_sel = sel_registerName(b"stringWithUTF8String:\0".as_ptr() as *const c_char);
            let make_string: extern "C" fn(*mut c_void, *mut c_void, *const c_char) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let reason = make_string(
                string_class,
                string_sel,
                b"MyKVM forwarding keyboard and mouse\0".as_ptr() as *const c_char,
            );

            let begin_sel =
                sel_registerName(b"beginActivityWithOptions:reason:\0".as_ptr() as *const c_char);
            let begin: extern "C" fn(*mut c_void, *mut c_void, u64, *mut c_void) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let options = NS_ACTIVITY_USER_INITIATED_ALLOWING_IDLE_SYSTEM_SLEEP
                | NS_ACTIVITY_LATENCY_CRITICAL;
            let activity = begin(process_info, begin_sel, options, reason);
            if activity.is_null() {
                return;
            }
            // The returned activity is autoreleased; retain it so it survives
            // past the current autorelease pool until we explicitly end it.
            let retain_sel = sel_registerName(b"retain\0".as_ptr() as *const c_char);
            let retain: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
                std::mem::transmute(objc_msgSend as *const ());
            let retained = retain(activity, retain_sel);
            ACTIVITY_TOKEN.store(retained as usize, Ordering::Relaxed);
        } else {
            let token = ACTIVITY_TOKEN.swap(0, Ordering::Relaxed);
            if token == 0 {
                return;
            }
            let activity = token as *mut c_void;
            let end_sel = sel_registerName(b"endActivity:\0".as_ptr() as *const c_char);
            let end: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) =
                std::mem::transmute(objc_msgSend as *const ());
            end(process_info, end_sel, activity);
            let release_sel = sel_registerName(b"release\0".as_ptr() as *const c_char);
            let release: extern "C" fn(*mut c_void, *mut c_void) =
                std::mem::transmute(objc_msgSend as *const ());
            release(activity, release_sel);
        }
    }
}

/// Receiver-side "this Mac is currently being driven by a remote peer" activity.
///
/// This is deliberately separate from the sender-side `remote_active` carried by
/// `MacCaptureContext` (which means "this Mac is *controlling* a remote" and
/// owns an anchor used for cursor re-pin / decoupling). Conflating the two would
/// make the capture loop re-pin the local cursor to a stale anchor and
/// decouple the physical mouse while the Mac is only being controlled.
///
/// The fix for the cross-screen cursor symptom lives here: when the Mac is the
/// *receiver* (e.g. Win -> Mac), nothing previously suppressed App Nap on the
/// backgrounded injection task, so macOS throttled and coalesced its scheduling
/// and the first cursor move after a pause came in late / stuttery. By marking
/// activity here and folding it into the capture loop's single App Nap decision
/// (it is the only other caller of `set_macos_app_nap_suppressed`, so the
/// activity token can never leak), the injection task is scheduled normally for
/// the whole time it is being driven.
#[cfg(target_os = "macos")]
mod macos_receiver_activity {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static ACTIVE: AtomicBool = AtomicBool::new(false);
    static LAST_NS: AtomicU64 = AtomicU64::new(0);

    // After this much silence we consider the remote peer to have stopped driving
    // this Mac and release App Nap again (the activity still allows idle sleep,
    // but there is no reason to keep the process unsuppressed when idle).
    const IDLE_NS: u64 = 250_000_000;

    /// Call on every injected remote input event. Suppresses App Nap
    /// immediately on the first event of a burst so the very first move is not
    /// throttled; subsequent events only refresh the liveness timestamp.
    pub fn mark_remote_input() {
        let now = now_ns();
        LAST_NS.store(now, Ordering::Relaxed);
        if !ACTIVE.swap(true, Ordering::Relaxed) {
            super::set_macos_app_nap_suppressed(true);
        }
    }

    /// Called from the macOS capture loop on every poll. Returns whether the
    /// receiver is still actively being driven and clears `ACTIVE` once it has
    /// gone stale so the loop releases App Nap.
    pub fn is_live() -> bool {
        if !ACTIVE.load(Ordering::Relaxed) {
            return false;
        }
        let now = now_ns();
        if now.saturating_sub(LAST_NS.load(Ordering::Relaxed)) > IDLE_NS {
            ACTIVE.store(false, Ordering::Relaxed);
            false
        } else {
            true
        }
    }

    fn now_ns() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }
}

#[cfg(target_os = "macos")]
fn set_macos_cursor_hidden_with_appkit(hidden: bool) {
    use std::ffi::c_void;
    use std::os::raw::c_char;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    unsafe {
        let class = objc_getClass(b"NSCursor\0".as_ptr() as *const c_char);
        if class.is_null() {
            return;
        }
        let selector = if hidden {
            sel_registerName(b"hide\0".as_ptr() as *const c_char)
        } else {
            sel_registerName(b"unhide\0".as_ptr() as *const c_char)
        };
        let msg_void: extern "C" fn(*mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        msg_void(class, selector);
    }
}

/// Push a fully-transparent cursor onto the AppKit cursor stack while a remote
/// session is active, then pop it on return.
///
/// `CGDisplayHideCursor` / `NSCursor hide` proved unreliable for a background
/// app: WindowServer services them lazily, so the pointer visibly lingers at the
/// shared edge for a fraction of a second on every crossing — even when we
/// re-issue hide every 50ms. A transparent cursor has no hidden/visible state
/// to flip: it just paints nothing, so there is nothing for WindowServer to
/// "un-hide". `push`/`pop` modify this app's active cursor image, which is far
/// more robust than the global hide counter when MyKVM is not frontmost.
#[cfg(target_os = "macos")]
fn set_macos_cursor_transparent(transparent: bool) {
    set_macos_cursor_transparent_inner(transparent, true);
}

#[cfg(target_os = "macos")]
fn set_macos_cursor_transparent_current() {
    set_macos_cursor_transparent_inner(true, false);
}

#[cfg(target_os = "macos")]
fn set_macos_cursor_transparent_inner(transparent: bool, push: bool) {
    use std::ffi::c_void;
    use std::os::raw::{c_char, c_double};

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    // A 16x16 fully-transparent RGBA bitmap. NSImage created from this paints
    // nothing, so the cursor is visually absent without a hide/show call.
    const SIZE: usize = 16;
    static TRANSPARENT_BYTES: [u8; SIZE * SIZE * 4] = [0; SIZE * SIZE * 4];

    unsafe {
        let nscursor = objc_getClass(b"NSCursor\0".as_ptr() as *const c_char);
        let nsimage = objc_getClass(b"NSImage\0".as_ptr() as *const c_char);
        let nsdata = objc_getClass(b"NSData\0".as_ptr() as *const c_char);
        let nssize = objc_getClass(b"NSSize\0".as_ptr() as *const c_char);
        if nscursor.is_null() || nsimage.is_null() || nsdata.is_null() || nssize.is_null() {
            return;
        }

        if !transparent {
            // Pop our transparent cursor to restore the previous cursor image.
            let pop_sel = sel_registerName(b"pop\0".as_ptr() as *const c_char);
            let pop: extern "C" fn(*mut c_void, *mut c_void) =
                std::mem::transmute(objc_msgSend as *const ());
            pop(nscursor, pop_sel);
            return;
        }

        // NSData dataWithBytes:length:
        let data_sel = sel_registerName(b"dataWithBytes:length:\0".as_ptr() as *const c_char);
        let data_with: extern "C" fn(*mut c_void, *mut c_void, *const u8, usize) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let data = data_with(
            nsdata,
            data_sel,
            TRANSPARENT_BYTES.as_ptr(),
            TRANSPARENT_BYTES.len(),
        );
        if data.is_null() {
            return;
        }

        // NSImage initWithData:
        let alloc_sel = sel_registerName(b"alloc\0".as_ptr() as *const c_char);
        let init_sel = sel_registerName(b"initWithData:\0".as_ptr() as *const c_char);
        let alloc: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let init: extern "C" fn(*mut c_void, *mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let image_obj = alloc(nsimage, alloc_sel);
        let image = init(image_obj, init_sel, data);
        if image.is_null() {
            return;
        }

        // NSSize { width, height } value type, laid out as two doubles.
        let size_sel = sel_registerName(b"setSize:\0".as_ptr() as *const c_char);
        let set_size: extern "C" fn(*mut c_void, *mut c_void, c_double, c_double) =
            std::mem::transmute(objc_msgSend as *const ());
        set_size(image, size_sel, SIZE as c_double, SIZE as c_double);

        // NSCursor initWithImage:hotSpot: — hot spot at (0,0), anywhere is fine
        // since the cursor is invisible. hotSpot is an NSPoint (two CGFloats),
        // passed by value; on arm64 that lands in the float argument registers as
        // two doubles after the object-pointer argument.
        let cursor_init_sel =
            sel_registerName(b"initWithImage:hotSpot:\0".as_ptr() as *const c_char);
        let cursor_init: extern "C" fn(
            *mut c_void,
            *mut c_void,
            *mut c_void,
            c_double,
            c_double,
        ) -> *mut c_void = std::mem::transmute(objc_msgSend as *const ());
        let cursor_obj = alloc(nscursor, alloc_sel);
        let cursor = cursor_init(cursor_obj, cursor_init_sel, image, 0.0, 0.0);
        if cursor.is_null() {
            return;
        }

        let apply_sel = if push {
            sel_registerName(b"push\0".as_ptr() as *const c_char)
        } else {
            sel_registerName(b"set\0".as_ptr() as *const c_char)
        };
        let apply: extern "C" fn(*mut c_void, *mut c_void) =
            std::mem::transmute(objc_msgSend as *const ());
        apply(cursor, apply_sel);
    }
}

#[cfg(target_os = "macos")]
fn repin_macos_cursor_if_drifted(
    context: &MacCaptureContext,
    location: core_graphics::geometry::CGPoint,
) -> bool {
    const DRIFT_THRESHOLD_PX: f64 = 1.5;
    const REPIN_INTERVAL_MS: u64 = 8;

    let Ok(anchor) = context.anchor.lock() else {
        return false;
    };
    let Some((x, y)) = *anchor else {
        return false;
    };
    drop(anchor);

    let dx = location.x - x;
    let dy = location.y - y;
    if dx.abs() <= DRIFT_THRESHOLD_PX && dy.abs() <= DRIFT_THRESHOLD_PX {
        return false;
    }

    if !macos_cursor_repin_due(context, Duration::from_millis(REPIN_INTERVAL_MS)) {
        return false;
    }

    // When MyKVM is not frontmost, macOS can re-associate the cursor with the
    // physical mouse despite CGAssociateMouseAndMouseCursorPosition(false).
    // Re-pin only after actual drift and at a capped rate.
    set_macos_cursor_decoupled(true);
    move_macos_cursor_without_event(context, core_graphics::geometry::CGPoint::new(x, y));
    true
}

#[cfg(target_os = "macos")]
fn force_repin_macos_cursor_to_anchor(context: &MacCaptureContext) {
    let Ok(anchor) = context.anchor.lock() else {
        return;
    };
    let Some((x, y)) = *anchor else {
        return;
    };
    drop(anchor);

    move_macos_cursor_without_event(context, core_graphics::geometry::CGPoint::new(x, y));
}

#[cfg(target_os = "macos")]
fn macos_cursor_repin_due(context: &MacCaptureContext, interval: Duration) -> bool {
    let Ok(mut last_repin) = context.last_cursor_repin.lock() else {
        return true;
    };
    let now = Instant::now();
    if last_repin
        .as_ref()
        .map(|last| now.duration_since(*last) < interval)
        .unwrap_or(false)
    {
        return false;
    }
    *last_repin = Some(now);
    true
}

#[cfg(target_os = "macos")]
fn macos_current_cursor_location() -> Option<core_graphics::geometry::CGPoint> {
    use core_graphics::{
        event::CGEvent,
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState).ok()?;
    CGEvent::new(source).ok().map(|event| event.location())
}

#[cfg(target_os = "macos")]
fn reset_cursor_repin_timer(context: &MacCaptureContext) {
    if let Ok(mut last_repin) = context.last_cursor_repin.lock() {
        *last_repin = None;
    }
}

#[cfg(target_os = "macos")]
fn reassert_macos_hidden_window_cursor(context: &MacCaptureContext, transparent_now: bool) {
    let Ok(hidden) = context.cursor_hidden.lock() else {
        return;
    };
    if !*hidden {
        return;
    }
    drop(hidden);

    if transparent_now {
        set_macos_cursor_transparent_current();
    }

    let Ok(mut last_reassert) = context.last_cursor_hide_reassert.lock() else {
        return;
    };
    let now = Instant::now();
    if last_reassert
        .as_ref()
        .map(|last| {
            now.duration_since(*last)
                < Duration::from_millis(MACOS_HIDDEN_WINDOW_CURSOR_HIDE_REASSERT_MS)
        })
        .unwrap_or(false)
    {
        return;
    }
    *last_reassert = Some(now);
    drop(last_reassert);

    set_macos_cursor_transparent_current();
    push_macos_cursor_hide(context);
}

#[cfg(target_os = "macos")]
fn mac_display_snapshots() -> Vec<MacDisplaySnapshot> {
    use core_graphics::display::CGDisplay;

    CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(|display_id| {
            let display = CGDisplay::new(display_id);
            let bounds = display.bounds();
            MacDisplaySnapshot {
                id: display_id,
                origin_x: bounds.origin.x,
                origin_y: bounds.origin.y,
                max_x: bounds.origin.x + bounds.size.width,
                max_y: bounds.origin.y + bounds.size.height,
            }
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn move_macos_cursor_without_event(
    context: &MacCaptureContext,
    point: core_graphics::geometry::CGPoint,
) {
    move_macos_cursor_without_event_on_displays(point, &context.display_snapshots);
}

#[cfg(target_os = "macos")]
fn move_macos_cursor_without_event_on_displays(
    point: core_graphics::geometry::CGPoint,
    displays: &[MacDisplaySnapshot],
) {
    use core_graphics::display::CGDisplay;

    for display in displays {
        if point.x >= display.origin_x
            && point.x <= display.max_x
            && point.y >= display.origin_y
            && point.y <= display.max_y
        {
            let local_point = core_graphics::geometry::CGPoint::new(
                point.x - display.origin_x,
                point.y - display.origin_y,
            );
            if CGDisplay::new(display.id)
                .move_cursor_to_point(local_point)
                .is_ok()
            {
                return;
            }
        }
    }

    let _ = CGDisplay::warp_mouse_cursor_position(point);
}

/// Arms macOS to hide the pointer even when MyKVM is NOT the frontmost app.
///
/// `CGDisplayHideCursor` / `[NSCursor hide]` are normally honored only while the
/// calling app is frontmost, so once MyKVM is minimized / backgrounded / its
/// window is closed, the local cursor reappears at the screen edge during a
/// crossing — the "not seamless, cursor shows up" symptom. Setting the private
/// CGS connection property `SetsCursorInBackground` to true makes the hide stick
/// regardless of focus. The symbols are resolved at runtime via `dlsym` so a
/// macOS build that has moved/removed them (they live in CoreGraphics today,
/// SkyLight on newer systems) degrades gracefully instead of failing to link.
#[cfg(target_os = "macos")]
fn enable_macos_background_cursor_hide() {
    use core_foundation::{base::TCFType, boolean::CFBoolean, string::CFString};
    use std::os::raw::{c_char, c_int, c_void};

    extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    }
    // RTLD_DEFAULT on macOS searches every already-loaded image.
    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

    static ENABLED: AtomicBool = AtomicBool::new(false);
    if ENABLED.swap(true, Ordering::Relaxed) {
        return;
    }

    unsafe {
        let main_conn = dlsym(
            RTLD_DEFAULT,
            b"CGSMainConnectionID\0".as_ptr() as *const c_char,
        );
        let set_prop = dlsym(
            RTLD_DEFAULT,
            b"CGSSetConnectionProperty\0".as_ptr() as *const c_char,
        );
        if main_conn.is_null() || set_prop.is_null() {
            return;
        }

        let main_conn: extern "C" fn() -> c_int = std::mem::transmute(main_conn);
        let set_prop: extern "C" fn(c_int, c_int, *const c_void, *const c_void) -> c_int =
            std::mem::transmute(set_prop);

        let cid = main_conn();
        let key = CFString::from_static_string("SetsCursorInBackground");
        let value = CFBoolean::true_value();
        let _ = set_prop(
            cid,
            cid,
            key.as_concrete_TypeRef() as *const c_void,
            value.as_CFTypeRef() as *const c_void,
        );
        // Hold the CF objects until the call returns.
        drop(key);
        drop(value);
    }
}

#[cfg(target_os = "macos")]
fn hide_macos_cursor_if_needed(context: &MacCaptureContext) {
    let Ok(mut hidden) = context.cursor_hidden.lock() else {
        return;
    };
    if *hidden {
        return;
    }

    // The PRIMARY mechanism is a transparent cursor (set_macos_cursor_transparent):
    // CGDisplayHideCursor / NSCursor hide are unreliable for a background app
    // (WindowServer services them lazily, pointer flickers at the edge). The
    // transparent cursor paints nothing with no hide/show state to flip. We keep
    // the hide calls as a secondary belt-and-suspenders, but they are no longer
    // the thing we rely on.
    enable_macos_background_cursor_hide();
    set_macos_cursor_transparent(true);
    push_macos_cursor_hide(context);
    if let Ok(mut last_reassert) = context.last_cursor_hide_reassert.lock() {
        *last_reassert = None;
    }
    log::debug!("[diag] transparent cursor pushed + hide issued (cursor_hidden false->true)");
    *hidden = true;
}

#[cfg(target_os = "macos")]
fn push_macos_cursor_hide(context: &MacCaptureContext) {
    let Ok(mut depth) = context.cursor_hide_depth.lock() else {
        return;
    };

    set_macos_cursor_hidden_with_appkit(true);
    if context.display_snapshots.is_empty() {
        let _ = core_graphics::display::CGDisplay::main().hide_cursor();
    } else {
        for display in &context.display_snapshots {
            let _ = core_graphics::display::CGDisplay::new(display.id).hide_cursor();
        }
    }
    *depth = depth.saturating_add(1);
}

#[cfg(target_os = "macos")]
fn show_macos_cursor_if_needed(context: &MacCaptureContext) {
    let Ok(mut hidden) = context.cursor_hidden.lock() else {
        return;
    };
    if !*hidden {
        return;
    }

    // Pop the transparent cursor first — this restores the real cursor image
    // and is the reliable inverse of the hide. The CGDisplay/NSCursor show calls
    // balance the secondary hide calls.
    set_macos_cursor_transparent(false);
    drain_macos_cursor_hide(context);
    if let Ok(mut last_reassert) = context.last_cursor_hide_reassert.lock() {
        *last_reassert = None;
    }
    *hidden = false;
    log::debug!("[diag] transparent cursor popped + show issued (cursor_hidden true->false)");
}

#[cfg(target_os = "macos")]
fn drain_macos_cursor_hide(context: &MacCaptureContext) {
    let count = context
        .cursor_hide_depth
        .lock()
        .map(|mut depth| {
            let count = *depth;
            *depth = 0;
            count
        })
        .unwrap_or(0);

    for _ in 0..count {
        if context.display_snapshots.is_empty() {
            let _ = core_graphics::display::CGDisplay::main().show_cursor();
        } else {
            for display in &context.display_snapshots {
                let _ = core_graphics::display::CGDisplay::new(display.id).show_cursor();
            }
        }
        set_macos_cursor_hidden_with_appkit(false);
    }
}

#[cfg(target_os = "macos")]
fn handle_macos_modifier_event(
    context: &MacCaptureContext,
    event_type: core_graphics::event::CGEventType,
    event: &core_graphics::event::CGEvent,
) -> core_graphics::event::CallbackResult {
    if matches!(event_type, core_graphics::event::CGEventType::FlagsChanged) {
        if let Ok(mut pressed) = context.pressed_modifiers.lock() {
            *pressed = mac_modifier_vks(event);
        }
    }

    core_graphics::event::CallbackResult::Keep
}

#[cfg(target_os = "macos")]
fn send_modifier_changes(
    context: &MacCaptureContext,
    target: &InputTarget,
    event: &core_graphics::event::CGEvent,
) {
    use core_graphics::event::EventField;

    let mac_code = event.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
    if mac_code == 57 {
        if let Some(key_code) = mac_key_to_windows_vk(mac_code) {
            send_packet(
                &context.quic_transport,
                target,
                InputEvent::Key {
                    key_code,
                    down: true,
                },
                &context.layout_state,
                &context.input_events,
            );
            send_packet(
                &context.quic_transport,
                target,
                InputEvent::Key {
                    key_code,
                    down: false,
                },
                &context.layout_state,
                &context.input_events,
            );
        }
        return;
    }

    let next = mac_modifier_vks(event);
    let Ok(mut previous) = context.pressed_modifiers.lock() else {
        return;
    };

    for key_code in next.iter().filter(|key_code| !previous.contains(key_code)) {
        send_packet(
            &context.quic_transport,
            target,
            InputEvent::Key {
                key_code: *key_code,
                down: true,
            },
            &context.layout_state,
            &context.input_events,
        );
    }

    for key_code in previous.iter().filter(|key_code| !next.contains(key_code)) {
        send_packet(
            &context.quic_transport,
            target,
            InputEvent::Key {
                key_code: *key_code,
                down: false,
            },
            &context.layout_state,
            &context.input_events,
        );
    }

    *previous = next;
}

#[cfg(target_os = "macos")]
fn mac_modifier_vks(event: &core_graphics::event::CGEvent) -> Vec<u16> {
    use core_graphics::event::CGEventFlags;

    let flags = event.get_flags();
    let mut keys = Vec::new();
    if flags.contains(CGEventFlags::CGEventFlagShift) {
        keys.push(0x10);
    }
    if flags.contains(CGEventFlags::CGEventFlagControl) {
        keys.push(0x11);
    }
    if flags.contains(CGEventFlags::CGEventFlagAlternate) {
        keys.push(0x12);
    }
    if flags.contains(CGEventFlags::CGEventFlagCommand) {
        keys.push(0x5B);
    }
    keys
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_key_to_windows_vk(code: u16) -> Option<u16> {
    Some(match code {
        0 => 0x41,
        1 => 0x53,
        2 => 0x44,
        3 => 0x46,
        4 => 0x48,
        5 => 0x47,
        6 => 0x5A,
        7 => 0x58,
        8 => 0x43,
        9 => 0x56,
        11 => 0x42,
        12 => 0x51,
        13 => 0x57,
        14 => 0x45,
        15 => 0x52,
        16 => 0x59,
        17 => 0x54,
        18 => 0x31,
        19 => 0x32,
        20 => 0x33,
        21 => 0x34,
        22 => 0x36,
        23 => 0x35,
        24 => 0xBB,
        25 => 0x39,
        26 => 0x37,
        27 => 0xBD,
        28 => 0x38,
        29 => 0x30,
        30 => 0xDD,
        31 => 0x4F,
        32 => 0x55,
        33 => 0xDB,
        34 => 0x49,
        35 => 0x50,
        36 => 0x0D,
        37 => 0x4C,
        38 => 0x4A,
        39 => 0xDE,
        40 => 0x4B,
        41 => 0xBA,
        42 => 0xDC,
        43 => 0xBC,
        44 => 0xBF,
        45 => 0x4E,
        46 => 0x4D,
        47 => 0xBE,
        48 => 0x09,
        49 => 0x20,
        50 => 0xC0,
        51 => 0x08,
        53 => 0x1B,
        54 => 0x5C,
        55 => 0x5B,
        56 => 0xA0,
        57 => 0x14,
        58 => 0xA4,
        59 => 0xA2,
        60 => 0xA1,
        61 => 0xA5,
        62 => 0xA3,
        63 => 0x5B,
        64 => 0x80,
        65 => 0x6E,
        67 => 0x6A,
        69 => 0x6B,
        71 => 0x90,
        75 => 0x6F,
        76 => 0x0D,
        78 => 0x6D,
        81 => 0x6D,
        82 => 0x60,
        83 => 0x61,
        84 => 0x62,
        85 => 0x63,
        86 => 0x64,
        87 => 0x65,
        88 => 0x66,
        89 => 0x67,
        91 => 0x68,
        92 => 0x69,
        96 => 0x74,
        97 => 0x75,
        98 => 0x76,
        99 => 0x72,
        100 => 0x77,
        101 => 0x78,
        103 => 0x7A,
        105 => 0x7C,
        106 => 0x7F,
        107 => 0x7D,
        109 => 0x79,
        111 => 0x7B,
        114 => 0x2D,
        115 => 0x24,
        116 => 0x21,
        117 => 0x2E,
        118 => 0x73,
        119 => 0x23,
        120 => 0x71,
        121 => 0x22,
        122 => 0x70,
        123 => 0x25,
        124 => 0x27,
        125 => 0x28,
        126 => 0x26,
        _ => return None,
    })
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn windows_vk_to_mac_key(code: u16) -> Option<u16> {
    mac_key_to_windows_vk_pairs()
        .iter()
        .find(|(_, vk)| *vk == code)
        .map(|(mac, _)| *mac)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn mac_key_to_windows_vk_pairs() -> &'static [(u16, u16)] {
    &[
        (0, 0x41),
        (1, 0x53),
        (2, 0x44),
        (3, 0x46),
        (4, 0x48),
        (5, 0x47),
        (6, 0x5A),
        (7, 0x58),
        (8, 0x43),
        (9, 0x56),
        (11, 0x42),
        (12, 0x51),
        (13, 0x57),
        (14, 0x45),
        (15, 0x52),
        (16, 0x59),
        (17, 0x54),
        (18, 0x31),
        (19, 0x32),
        (20, 0x33),
        (21, 0x34),
        (22, 0x36),
        (23, 0x35),
        (24, 0xBB),
        (25, 0x39),
        (26, 0x37),
        (27, 0xBD),
        (28, 0x38),
        (29, 0x30),
        (30, 0xDD),
        (31, 0x4F),
        (32, 0x55),
        (33, 0xDB),
        (34, 0x49),
        (35, 0x50),
        (36, 0x0D),
        (37, 0x4C),
        (38, 0x4A),
        (39, 0xDE),
        (40, 0x4B),
        (41, 0xBA),
        (42, 0xDC),
        (43, 0xBC),
        (44, 0xBF),
        (45, 0x4E),
        (46, 0x4D),
        (47, 0xBE),
        (48, 0x09),
        (49, 0x20),
        (50, 0xC0),
        (51, 0x08),
        (53, 0x1B),
        (54, 0x5C),
        (55, 0x5B),
        (56, 0x10),
        (56, 0xA0),
        (57, 0x14),
        (58, 0x12),
        (58, 0xA4),
        (59, 0x11),
        (59, 0xA2),
        (60, 0xA1),
        (61, 0xA5),
        (62, 0xA3),
        (63, 0x5B),
        (64, 0x80),
        (65, 0x6E),
        (67, 0x6A),
        (69, 0x6B),
        (71, 0x90),
        (75, 0x6F),
        (76, 0x0D),
        (78, 0x6D),
        (81, 0x6D),
        (82, 0x60),
        (83, 0x61),
        (84, 0x62),
        (85, 0x63),
        (86, 0x64),
        (87, 0x65),
        (88, 0x66),
        (89, 0x67),
        (91, 0x68),
        (92, 0x69),
        (96, 0x74),
        (97, 0x75),
        (98, 0x76),
        (99, 0x72),
        (100, 0x77),
        (101, 0x78),
        (103, 0x7A),
        (105, 0x7C),
        (106, 0x7F),
        (107, 0x7D),
        (109, 0x79),
        (111, 0x7B),
        (114, 0x2D),
        (115, 0x24),
        (116, 0x21),
        (117, 0x2E),
        (118, 0x73),
        (119, 0x23),
        (120, 0x71),
        (121, 0x22),
        (122, 0x70),
        (123, 0x25),
        (124, 0x27),
        (125, 0x28),
        (126, 0x26),
    ]
}

#[cfg(target_os = "macos")]
fn inject_mouse_move(x: i32, y: i32, drag_button: Option<MouseButton>) {
    use core_graphics::{
        display::CGDisplay,
        event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton},
        event_source::{CGEventSource, CGEventSourceStateID},
        geometry::CGPoint,
    };

    let point = CGPoint::new(x as f64, y as f64);
    let (event_type, mouse_button) = match drag_button {
        Some(MouseButton::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
        Some(MouseButton::Right) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
        // Middle and the side buttons are all "other" drags. button_from_mask
        // never actually reports a side button as a drag, so this is only here
        // for exhaustiveness.
        Some(MouseButton::Middle | MouseButton::Back | MouseButton::Forward) => {
            (CGEventType::OtherMouseDragged, CGMouseButton::Center)
        }
        None => (CGEventType::MouseMoved, CGMouseButton::Left),
    };

    // Posted mouse-move events do not always update the visible macOS cursor.
    let _ = CGDisplay::warp_mouse_cursor_position(point);

    // Cache the HID system event source per worker thread. Creating a fresh
    // CGEventSource on every move round-trips through the HID system and was a
    // measurable part of per-move latency during rapid cursor motion. It is
    // immutable once built and only used on the thread that created it.
    let source = HID_EVENT_SOURCE.with(|cached| {
        if cached.borrow().is_none() {
            *cached.borrow_mut() = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok();
        }
        cached.borrow().clone()
    });
    if let Some(source) = source {
        if let Ok(event) = CGEvent::new_mouse_event(source, event_type, point, mouse_button) {
            // Mark as our own so the HID capture tap (which forwards local input
            // to the peer) never bounces this injected move back / drops it.
            macos_scroll::tag_own_event(&event);
            event.post(CGEventTapLocation::HID);
        }
    }
}

// Per-thread cache of the HID-system `CGEventSource` used to build injected
// mouse events. `CGEventSource` is not `Sync`, so it lives in a `thread_local`
// rather than a process-wide `static`; each tokio worker that handles an input
// datagram builds one and reuses it for the life of the thread.
#[cfg(target_os = "macos")]
thread_local! {
    static HID_EVENT_SOURCE: std::cell::RefCell<Option<core_graphics::event_source::CGEventSource>> =
        std::cell::RefCell::new(None);
}

/// One pressed-button record for injected macOS click counting.
#[cfg(target_os = "macos")]
#[derive(Debug, Clone, Copy)]
struct MacClickDown {
    button: MouseButton,
    x: i32,
    y: i32,
    at: Instant,
    count: u8,
}

/// Click counting for injected macOS mouse buttons. macOS does NOT infer
/// double-clicks from timing for synthetic events — every injected Down/Up
/// with `kCGMouseEventClickState` 0 is an independent single click, so remote
/// double-clicks never registered in apps. Replicate the native rule: a press
/// within the system double-click interval and a few px of the previous one
/// raises the click count (capped at triple), and the release repeats the
/// count of the press it pairs with.
#[cfg(target_os = "macos")]
#[derive(Debug, Default)]
struct MacClickTracker {
    last_down: Option<MacClickDown>,
    pressed: [Option<MacClickDown>; 3],
}

#[cfg(target_os = "macos")]
impl MacClickTracker {
    const MAX_DISTANCE_PX: i32 = 8;

    fn event_count(
        &mut self,
        button: MouseButton,
        down: bool,
        x: i32,
        y: i32,
        now: Instant,
        double_click_interval: Duration,
    ) -> i64 {
        let index = match button {
            MouseButton::Left => 0,
            MouseButton::Right => 1,
            MouseButton::Middle => 2,
            // Side (back/forward) buttons are navigation, not click targets:
            // no double-click chaining and no pressed-slot tracking.
            MouseButton::Back | MouseButton::Forward => return i64::from(down),
        };

        if down {
            let count = self
                .last_down
                .filter(|last| {
                    last.button == button
                        && now.saturating_duration_since(last.at) <= double_click_interval
                        && click_points_are_near(last.x, last.y, x, y, Self::MAX_DISTANCE_PX)
                })
                .map(|last| last.count.saturating_add(1).min(3))
                .unwrap_or(1);
            let click = MacClickDown {
                button,
                x,
                y,
                at: now,
                count,
            };
            self.last_down = Some(click);
            self.pressed[index] = Some(click);
            return i64::from(count);
        }

        let Some(click) = self.pressed[index].take() else {
            return 0;
        };
        if click_points_are_near(click.x, click.y, x, y, Self::MAX_DISTANCE_PX) {
            i64::from(click.count)
        } else {
            // The button moved while held: that was a drag, not a click, and
            // it must not chain into a double-click either.
            self.last_down = None;
            0
        }
    }
}

#[cfg(target_os = "macos")]
fn click_points_are_near(x1: i32, y1: i32, x2: i32, y2: i32, max_distance: i32) -> bool {
    let dx = i64::from(x1) - i64::from(x2);
    let dy = i64::from(y1) - i64::from(y2);
    let max = i64::from(max_distance);
    dx * dx + dy * dy <= max * max
}

/// The user's configured double-click speed ([NSEvent doubleClickInterval]),
/// resolved once via the ObjC runtime and clamped to a sane range so a broken
/// answer degrades to the macOS default feel instead of breaking clicks.
#[cfg(target_os = "macos")]
fn macos_double_click_interval() -> Duration {
    static INTERVAL: OnceLock<Duration> = OnceLock::new();
    *INTERVAL.get_or_init(|| {
        use std::ffi::c_void;
        use std::os::raw::c_char;

        #[link(name = "objc")]
        extern "C" {
            fn objc_getClass(name: *const c_char) -> *mut c_void;
            fn sel_registerName(name: *const c_char) -> *mut c_void;
            fn objc_msgSend();
        }

        let seconds = unsafe {
            let class = objc_getClass(b"NSEvent\0".as_ptr() as *const c_char);
            if class.is_null() {
                0.5
            } else {
                let selector = sel_registerName(b"doubleClickInterval\0".as_ptr() as *const c_char);
                let get_interval: extern "C" fn(*mut c_void, *mut c_void) -> f64 =
                    std::mem::transmute(objc_msgSend as *const ());
                get_interval(class, selector)
            }
        };
        Duration::from_secs_f64(if seconds.is_finite() && (0.1..=2.0).contains(&seconds) {
            seconds
        } else {
            0.5
        })
    })
}

#[cfg(target_os = "macos")]
fn macos_click_state(button: MouseButton, down: bool, x: i32, y: i32) -> i64 {
    macos_click_tracker()
        .lock()
        .map(|mut tracker| {
            tracker.event_count(
                button,
                down,
                x,
                y,
                Instant::now(),
                macos_double_click_interval(),
            )
        })
        .unwrap_or(if down { 1 } else { 0 })
}

#[cfg(target_os = "macos")]
fn macos_click_tracker() -> &'static Mutex<MacClickTracker> {
    static TRACKER: OnceLock<Mutex<MacClickTracker>> = OnceLock::new();
    TRACKER.get_or_init(|| Mutex::new(MacClickTracker::default()))
}

#[cfg(target_os = "macos")]
fn inject_mouse_button(button: MouseButton, down: bool, x: i32, y: i32) {
    use core_graphics::{
        display::CGDisplay,
        event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField},
        event_source::{CGEventSource, CGEventSourceStateID},
        geometry::CGPoint,
    };

    let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        return;
    };
    // Side buttons are "other" mouse events distinguished only by their button
    // number (macOS: 2 = middle, 3 = back, 4 = forward); new_mouse_event has no
    // CGMouseButton for 3/4, so create an Other event and stamp the number.
    let (event_type, mouse_button, button_number) = match (button, down) {
        (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left, None),
        (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left, None),
        (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right, None),
        (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right, None),
        (MouseButton::Middle, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, None),
        (MouseButton::Middle, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, None),
        (MouseButton::Back, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, Some(3)),
        (MouseButton::Back, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, Some(3)),
        (MouseButton::Forward, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center, Some(4)),
        (MouseButton::Forward, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center, Some(4)),
    };
    let point = CGPoint::new(x as f64, y as f64);

    let _ = CGDisplay::warp_mouse_cursor_position(point);

    // #3: a native click anywhere in another app's window transfers focus to
    // that app — even when you click the middle, not the title bar. The
    // synthetic press we inject does not do this on its own, so on button-down
    // we activate the app whose window is under the cursor (hit-tested via the
    // on-screen window list). Without this, clicking a non-titlebar region of
    // another window leaves focus on the previously-active app.
    if down {
        macos_activate_app_at_point(x, y);
    }

    if let Ok(event) = CGEvent::new_mouse_event(source, event_type, point, mouse_button) {
        if let Some(number) = button_number {
            event.set_integer_value_field(EventField::MOUSE_EVENT_BUTTON_NUMBER, number);
        }
        event.set_integer_value_field(
            EventField::MOUSE_EVENT_CLICK_STATE,
            macos_click_state(button, down, x, y),
        );
        // Mark as our own so the HID capture tap skips this injected click.
        macos_scroll::tag_own_event(&event);
        event.post(CGEventTapLocation::HID);
    }
}

// ---------------------------------------------------------------------------
// #3 — synthetic clicks must transfer focus like native ones
//
// A real click anywhere inside another app's window activates that app, even
// when you hit the middle of the content instead of the title bar. A CGEvent we
// post ourselves does not: the window server delivers it to whichever app is
// already frontmost. The fix is to hit-test the on-screen window list at the
// click point, find the owning process and activate it *before* the button-down
// is posted, which is exactly what AppKit does internally.
//
// Raw CoreFoundation C entry points are used deliberately instead of the
// `core-foundation` wrapper types: this file is cross-compiled from a Windows
// host for the Mac build, so the fewer generic inference sites the better.
// ---------------------------------------------------------------------------

/// `kCFStringEncodingUTF8`.
#[cfg(target_os = "macos")]
const CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
/// `kCFNumberDoubleType` / `kCFNumberSInt32Type` from CFNumber.h.
#[cfg(target_os = "macos")]
const CF_NUMBER_DOUBLE: i32 = 13;
#[cfg(target_os = "macos")]
const CF_NUMBER_SINT32: i32 = 3;

#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFArrayGetCount(array: *const std::ffi::c_void) -> isize;
    fn CFArrayGetValueAtIndex(
        array: *const std::ffi::c_void,
        index: isize,
    ) -> *const std::ffi::c_void;
    fn CFDictionaryGetValue(
        dict: *const std::ffi::c_void,
        key: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    fn CFStringCreateWithCString(
        alloc: *const std::ffi::c_void,
        c_str: *const std::os::raw::c_char,
        encoding: u32,
    ) -> *const std::ffi::c_void;
    fn CFNumberGetValue(
        number: *const std::ffi::c_void,
        the_type: i32,
        value_ptr: *mut std::ffi::c_void,
    ) -> u8;
    fn CFRelease(cf: *const std::ffi::c_void);
}

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(
        option: u32,
        relative_to_window: u32,
    ) -> *const std::ffi::c_void;
}

/// Reads a `CFNumber` stored under `key` as `f64`. Returns `None` when the key
/// is missing or holds something else.
#[cfg(target_os = "macos")]
unsafe fn cf_dict_f64(dict: *const std::ffi::c_void, key: &[u8]) -> Option<f64> {
    let cf_key = CFStringCreateWithCString(
        std::ptr::null(),
        key.as_ptr() as *const std::os::raw::c_char,
        CF_STRING_ENCODING_UTF8,
    );
    if cf_key.is_null() {
        return None;
    }
    let value = CFDictionaryGetValue(dict, cf_key);
    CFRelease(cf_key);
    if value.is_null() {
        return None;
    }
    let mut out: f64 = 0.0;
    let ok = CFNumberGetValue(
        value,
        CF_NUMBER_DOUBLE,
        &mut out as *mut f64 as *mut std::ffi::c_void,
    );
    (ok != 0).then_some(out)
}

/// Reads a `CFNumber` stored under `key` as `i32` (pids and window layers).
#[cfg(target_os = "macos")]
unsafe fn cf_dict_i32(dict: *const std::ffi::c_void, key: &[u8]) -> Option<i32> {
    let cf_key = CFStringCreateWithCString(
        std::ptr::null(),
        key.as_ptr() as *const std::os::raw::c_char,
        CF_STRING_ENCODING_UTF8,
    );
    if cf_key.is_null() {
        return None;
    }
    let value = CFDictionaryGetValue(dict, cf_key);
    CFRelease(cf_key);
    if value.is_null() {
        return None;
    }
    let mut out: i32 = 0;
    let ok = CFNumberGetValue(
        value,
        CF_NUMBER_SINT32,
        &mut out as *mut i32 as *mut std::ffi::c_void,
    );
    (ok != 0).then_some(out)
}

/// Hit-tests the on-screen window list at `(x, y)` and activates the owning app
/// so a synthetic click in the *middle* of another window transfers focus the
/// way a native click does (#3).
#[cfg(target_os = "macos")]
fn macos_activate_app_at_point(x: i32, y: i32) {
    const OPT_ON_SCREEN_ONLY: u32 = 1 << 0;
    const OPT_EXCLUDE_DESKTOP: u32 = 1 << 4;

    let self_pid = std::process::id() as i32;
    let px = x as f64;
    let py = y as f64;

    unsafe {
        let list = CGWindowListCopyWindowInfo(OPT_ON_SCREEN_ONLY | OPT_EXCLUDE_DESKTOP, 0);
        if list.is_null() {
            return;
        }
        // The list is ordered front-to-back, so the first hit is the window the
        // click actually lands on.
        let count = CFArrayGetCount(list);
        let mut hit_pid: Option<i32> = None;
        for index in 0..count {
            let window = CFArrayGetValueAtIndex(list, index);
            if window.is_null() {
                continue;
            }
            // Layer 0 is the normal application layer. Anything above it is the
            // Dock, the menu bar, notification banners or a screen-saver style
            // overlay — activating those would be wrong.
            if cf_dict_i32(window, b"kCGWindowLayer\0").unwrap_or(1) != 0 {
                continue;
            }
            let Some(pid) = cf_dict_i32(window, b"kCGWindowOwnerPID\0") else {
                continue;
            };
            if pid == self_pid {
                continue;
            }
            let Some(bounds) = cf_dict_ref(window, b"kCGWindowBounds\0") else {
                continue;
            };
            let bx = cf_dict_f64(bounds, b"X\0").unwrap_or(0.0);
            let by = cf_dict_f64(bounds, b"Y\0").unwrap_or(0.0);
            let bw = cf_dict_f64(bounds, b"Width\0").unwrap_or(0.0);
            let bh = cf_dict_f64(bounds, b"Height\0").unwrap_or(0.0);
            if bw <= 1.0 || bh <= 1.0 {
                continue;
            }
            if px >= bx && px < bx + bw && py >= by && py < by + bh {
                hit_pid = Some(pid);
                break;
            }
        }
        CFRelease(list);

        if let Some(pid) = hit_pid {
            // Already frontmost — re-activating would raise/flicker the window
            // for no reason, and would fight a click that is just re-focusing a
            // control inside the current app.
            if macos_frontmost_pid() == Some(pid) {
                return;
            }
            macos_activate_app_by_pid(pid);
        }
    }
}

/// Fetches a nested `CFDictionary` (e.g. `kCGWindowBounds`) without taking
/// ownership — the value belongs to the enclosing dictionary.
#[cfg(target_os = "macos")]
unsafe fn cf_dict_ref(
    dict: *const std::ffi::c_void,
    key: &[u8],
) -> Option<*const std::ffi::c_void> {
    let cf_key = CFStringCreateWithCString(
        std::ptr::null(),
        key.as_ptr() as *const std::os::raw::c_char,
        CF_STRING_ENCODING_UTF8,
    );
    if cf_key.is_null() {
        return None;
    }
    let value = CFDictionaryGetValue(dict, cf_key);
    CFRelease(cf_key);
    (!value.is_null()).then_some(value)
}

/// pid of the app that currently owns the foreground, or `None` if AppKit will
/// not tell us (e.g. during a Space transition).
#[cfg(target_os = "macos")]
fn macos_frontmost_pid() -> Option<i32> {
    use std::ffi::c_void;
    use std::os::raw::c_char;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    unsafe {
        let cls = objc_getClass(b"NSWorkspace\0".as_ptr() as *const c_char);
        if cls.is_null() {
            return None;
        }
        let msg: extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let ws = msg(
            cls,
            sel_registerName(b"sharedWorkspace\0".as_ptr() as *const c_char),
        );
        if ws.is_null() {
            return None;
        }
        let app = msg(
            ws,
            sel_registerName(b"frontmostApplication\0".as_ptr() as *const c_char),
        );
        if app.is_null() {
            return None;
        }
        let pid_msg: extern "C" fn(*mut c_void, *mut c_void) -> i32 =
            std::mem::transmute(objc_msgSend as *const ());
        Some(pid_msg(
            app,
            sel_registerName(b"processIdentifier\0".as_ptr() as *const c_char),
        ))
    }
}

/// `[[NSRunningApplication runningApplicationWithProcessIdentifier: pid]
///   activateWithOptions: NSApplicationActivateIgnoringOtherApps]`
#[cfg(target_os = "macos")]
fn macos_activate_app_by_pid(pid: i32) {
    use std::ffi::c_void;
    use std::os::raw::c_char;

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        fn objc_msgSend();
    }

    unsafe {
        let cls = objc_getClass(b"NSRunningApplication\0".as_ptr() as *const c_char);
        if cls.is_null() {
            return;
        }
        let sel =
            sel_registerName(b"runningApplicationWithProcessIdentifier:\0".as_ptr() as *const c_char);
        let msg: extern "C" fn(*mut c_void, *mut c_void, i32) -> *mut c_void =
            std::mem::transmute(objc_msgSend as *const ());
        let app = msg(cls, sel, pid);
        if app.is_null() {
            return;
        }
        let act = sel_registerName(b"activateWithOptions:\0".as_ptr() as *const c_char);
        // NSApplicationActivateIgnoringOtherApps = 1 << 1
        let msg_opt: extern "C" fn(*mut c_void, *mut c_void, u64) =
            std::mem::transmute(objc_msgSend as *const ());
        msg_opt(app, act, 2);
    }
}

/// Held modifier flags to stamp on injected macOS events. Posting a bare
/// modifier *keycode* does not make the window server apply that modifier to the
/// key events posted after it, so capitals, shifted symbols and every shortcut
/// (including the Ctrl<->Cmd remap) silently failed. We instead track the
/// modifier key-downs/ups we inject and set the matching CGEventFlags on each
/// event.
#[cfg(target_os = "macos")]
static MAC_INJECT_FLAGS: AtomicU64 = AtomicU64::new(0);

/// Latch so a held (auto-repeating) remote Caps Lock toggles the input source
/// exactly once until its key-up arrives.
#[cfg(target_os = "macos")]
static MACOS_CAPS_LOCK_DOWN: AtomicBool = AtomicBool::new(false);

/// Clears the tracked injected-modifier flags. Called when receiving stops so a
/// dropped modifier key-up cannot leave Shift/Ctrl/Cmd stuck on for later keys.
#[cfg(target_os = "macos")]
pub fn reset_injected_modifiers() {
    MAC_INJECT_FLAGS.store(0, Ordering::Relaxed);
    MACOS_CAPS_LOCK_DOWN.store(false, Ordering::Relaxed);
    if let Ok(mut tracker) = macos_click_tracker().lock() {
        *tracker = MacClickTracker::default();
    }
}

#[cfg(not(target_os = "macos"))]
pub fn reset_injected_modifiers() {}

/// Maps a Windows virtual-key modifier (the wire format) to its macOS event
/// flag bits, or `None` for non-modifier keys.
#[cfg(target_os = "macos")]
fn windows_vk_to_mac_flag(vk: u16) -> Option<u64> {
    use core_graphics::event::CGEventFlags;
    let flag = match vk {
        0x10 | 0xA0 | 0xA1 => CGEventFlags::CGEventFlagShift,
        0x11 | 0xA2 | 0xA3 => CGEventFlags::CGEventFlagControl,
        0x12 | 0xA4 | 0xA5 => CGEventFlags::CGEventFlagAlternate,
        0x5B | 0x5C => CGEventFlags::CGEventFlagCommand,
        _ => return None,
    };
    Some(flag.bits())
}

#[cfg(target_os = "macos")]
fn inject_key(key_code: u16, down: bool) {
    use core_graphics::{
        event::{CGEvent, CGEventFlags, CGEventTapLocation},
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    // Rate-limited, on-demand only: this is the one place where Secure Keyboard
    // Entry can actually hurt us, so it is also the only place we look.
    macos_probe_secure_input_on_key_inject();

    // VK_CAPITAL: replicate the macOS "Caps Lock switches input sources"
    // behaviour for remote input. macOS honours that setting only for the
    // physical key — an injected caps keycode toggles neither the IME nor the
    // caps state — so post the system "Select the previous input source"
    // hotkey (⌃Space) instead: HIToolbox then performs the switch exactly as
    // for a physical press, including refreshing the focused app's input
    // session (TISSelectInputSource from a background process updates the
    // menu-bar indicator but the focused app keeps typing in the old source
    // until refocused). Remote Caps Lock therefore never acts as a
    // letter-case toggle on this Mac.
    // ponytail: assumes the ⌃Space symbolic hotkey is enabled (macOS default,
    // verified on this deployment); read com.apple.symbolichotkeys key 60 if
    // this ever needs to adapt.
    if key_code == 0x14 {
        if down {
            if !MACOS_CAPS_LOCK_DOWN.swap(true, Ordering::Relaxed) {
                macos_post_select_previous_input_source();
            }
        } else {
            MACOS_CAPS_LOCK_DOWN.store(false, Ordering::Relaxed);
        }
        return;
    }

    // Keep the running modifier state in sync, so the modifier event itself and
    // every later key carry the right flags.
    if let Some(flag) = windows_vk_to_mac_flag(key_code) {
        let mut flags = MAC_INJECT_FLAGS.load(Ordering::Relaxed);
        if down {
            flags |= flag;
        } else {
            flags &= !flag;
        }
        MAC_INJECT_FLAGS.store(flags, Ordering::Relaxed);
    }

    let Some(mac_code) = windows_vk_to_mac_key(key_code) else {
        log::debug!("inject_key: no mac keycode for windows vk {key_code:#04x}; dropping");
        return;
    };
    let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
        log::warn!("inject_key: failed to create CGEventSource");
        return;
    };
    match CGEvent::new_keyboard_event(source, mac_code, down) {
        Ok(event) => {
            let mut flags = CGEventFlags::from_bits_truncate(MAC_INJECT_FLAGS.load(Ordering::Relaxed));
            // A physical Mac keyboard stamps the function-section flags on
            // arrow/nav keys (arrows additionally carry the numeric-pad bit).
            // Shortcut matching — system ones like ⌃← "move left a space" and
            // app key equivalents like ⌘← — compares those flags, so injected
            // arrows without them fire nothing: the historic "Ctrl+arrows do
            // nothing on the Mac" bug.
            flags |= mac_function_section_flags(mac_code);
            event.set_flags(flags);
            // Mark as our own so the HID capture tap skips this injected key.
            macos_scroll::tag_own_event(&event);
            event.post(CGEventTapLocation::HID);
        }
        Err(_) => log::warn!("inject_key: failed to build keyboard event for mac code {mac_code}"),
    }
}

/// Extra CGEventFlags a physical keyboard sets for function-section keys:
/// arrows (123-126) carry Fn + numeric-pad; Home/End/PageUp/PageDown/forward
/// Delete carry Fn. Everything else gets nothing extra.
#[cfg(target_os = "macos")]
fn mac_function_section_flags(mac_code: u16) -> core_graphics::event::CGEventFlags {
    use core_graphics::event::CGEventFlags;

    match mac_code {
        // Left, Right, Down, Up
        123..=126 => CGEventFlags::CGEventFlagSecondaryFn | CGEventFlags::CGEventFlagNumericPad,
        // Home(115), PageUp(116), forward Delete(117), End(119), PageDown(121)
        115 | 116 | 117 | 119 | 121 => CGEventFlags::CGEventFlagSecondaryFn,
        _ => CGEventFlags::empty(),
    }
}

/// Posts the system input-source toggle hotkey (⌃Space, symbolic hotkey 60)
/// as a full physical-like sequence: Control down, Space down/up, Control up.
/// Flags are set per event and deliberately plain ⌃ — a concurrently held
/// remote modifier would form a different chord and miss the hotkey; the next
/// injected key restores the tracked flags anyway.
#[cfg(target_os = "macos")]
fn macos_post_select_previous_input_source() {
    use core_graphics::{
        event::{CGEvent, CGEventFlags, CGEventTapLocation},
        event_source::{CGEventSource, CGEventSourceStateID},
    };

    const MAC_KEY_CONTROL: u16 = 59; // kVK_Control
    const MAC_KEY_SPACE: u16 = 49; // kVK_Space

    let control = CGEventFlags::CGEventFlagControl;
    let no_flags = CGEventFlags::empty();
    let sequence = [
        (MAC_KEY_CONTROL, true, control),
        (MAC_KEY_SPACE, true, control),
        (MAC_KEY_SPACE, false, control),
        (MAC_KEY_CONTROL, false, no_flags),
    ];
    for (mac_code, down, flags) in sequence {
        let Ok(source) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else {
            log::warn!("caps toggle: failed to create CGEventSource");
            return;
        };
        match CGEvent::new_keyboard_event(source, mac_code, down) {
            Ok(event) => {
                event.set_flags(flags);
                event.post(CGEventTapLocation::HID);
            }
            Err(_) => {
                log::warn!("caps toggle: failed to build keyboard event for mac code {mac_code}");
                return;
            }
        }
    }
    log::info!("[diag] caps: posted input-source toggle (ctrl+space)");
}

#[cfg(target_os = "windows")]
fn inject_mouse_move(x: i32, y: i32, drag_button: Option<MouseButton>) {
    crate::windows_input::inject_mouse_move(x, y, drag_button);
}

#[cfg(target_os = "windows")]
fn inject_mouse_button(button: MouseButton, down: bool, x: i32, y: i32) {
    crate::windows_input::inject_mouse_button(button, down, x, y);
}

#[cfg(target_os = "windows")]
fn inject_key(key_code: u16, down: bool) {
    crate::windows_input::inject_key(key_code, down);
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn inject_mouse_move(_x: i32, _y: i32, _drag_button: Option<MouseButton>) {}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn inject_mouse_button(_button: MouseButton, _down: bool, _x: i32, _y: i32) {}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn inject_key(_key_code: u16, _down: bool) {}

#[cfg(test)]
mod tests {
    use super::*;

    // The drag gate lives in process-wide statics, so every assertion about it
    // runs inside one test to keep the ordering deterministic under the
    // parallel test harness.
    #[test]
    fn drag_gate_blocks_snap_gestures_but_allows_a_sustained_shove() {
        reset_local_button_mask();

        // No button held: crossings are never gated.
        assert!(
            drag_crossing_allowed(),
            "a plain cursor move must cross freely"
        );

        // Button down — this is the window-drag / snap-to-edge case. The first
        // qualifying event only arms the timer, it must not cross.
        set_local_button(MouseButton::Left, true);
        assert!(
            !drag_crossing_allowed(),
            "dragging into the edge must not hand the cursor to the remote"
        );
        assert!(
            !drag_crossing_allowed(),
            "still inside the hold window: keep refusing"
        );

        // Releasing the button clears the timer, so the next drag starts fresh
        // instead of inheriting a nearly-elapsed deadline.
        set_local_button(MouseButton::Left, false);
        assert!(drag_crossing_allowed(), "release re-opens the gate");
        assert!(
            DRAG_EDGE_PRESS_SINCE.lock().unwrap().is_none(),
            "release must clear the armed timer"
        );

        // Keep shoving past the deadline and the crossing is allowed — this is
        // what preserves dragging content onto the other machine.
        set_local_button(MouseButton::Left, true);
        assert!(!drag_crossing_allowed(), "arms the timer");
        *DRAG_EDGE_PRESS_SINCE.lock().unwrap() =
            Some(Instant::now() - Duration::from_millis(DRAG_CROSSING_HOLD_MS + 10));
        assert!(
            drag_crossing_allowed(),
            "a sustained shove past the hold window must cross"
        );

        // Multiple buttons: the gate stays armed until the last one is up.
        reset_local_button_mask();
        set_local_button(MouseButton::Left, true);
        set_local_button(MouseButton::Right, true);
        set_local_button(MouseButton::Left, false);
        assert!(
            local_button_is_down(),
            "right button is still held, drag is still in progress"
        );
        set_local_button(MouseButton::Right, false);
        assert!(!local_button_is_down(), "all buttons released");

        reset_local_button_mask();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn windows_vk_to_mac_flag_covers_modifiers() {
        // Modifiers (incl. sided variants and LWin/RWin -> Command) map to a flag.
        assert!(windows_vk_to_mac_flag(0x10).is_some()); // Shift
        assert!(windows_vk_to_mac_flag(0xA1).is_some()); // Right Shift
        assert!(windows_vk_to_mac_flag(0x11).is_some()); // Control
        assert!(windows_vk_to_mac_flag(0x12).is_some()); // Alt -> Option
        assert!(windows_vk_to_mac_flag(0x5B).is_some()); // LWin -> Command

        // Ordinary keys carry no modifier flag.
        assert!(windows_vk_to_mac_flag(0x41).is_none()); // 'A'
        assert!(windows_vk_to_mac_flag(0x20).is_none()); // Space
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_raw_gesture_mask_covers_trackpad_system_gestures() {
        let mask = macos_raw_gesture_event_mask();

        for event_type in MACOS_RAW_GESTURE_EVENT_TYPES {
            assert_ne!(mask & (1_u64 << *event_type), 0);
        }
        assert_ne!(mask & (1_u64 << MACOS_NSEVENT_TYPE_SWIPE), 0);
        assert_ne!(mask & (1_u64 << MACOS_NSEVENT_TYPE_SYSTEM_DEFINED), 0);
        assert_eq!(mask & (1_u64 << 22), 0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_hidden_remote_loop_is_slower_than_visible_remote_loop() {
        assert_eq!(
            macos_capture_loop_ms(false, false),
            MACOS_IDLE_CAPTURE_LOOP_MS
        );
        assert_eq!(
            macos_capture_loop_ms(true, true),
            MACOS_VISIBLE_REMOTE_CAPTURE_LOOP_MS
        );
        assert_eq!(
            macos_capture_loop_ms(true, false),
            MACOS_HIDDEN_REMOTE_CAPTURE_LOOP_MS
        );
        assert!(MACOS_HIDDEN_REMOTE_CAPTURE_LOOP_MS > MACOS_VISIBLE_REMOTE_CAPTURE_LOOP_MS);
    }

    fn screen(device_id: &str, id: &str, x: i32, y: i32, width: i32, height: i32) -> Screen {
        Screen {
            id: id.into(),
            device_id: device_id.into(),
            name: id.into(),
            x,
            y,
            width,
            height,
            scale: 1.0,
            is_primary: true,
        }
    }

    fn target_for_coordinate_tests() -> InputTarget {
        InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47833".into(),
            target_platform: "windows".into(),
            transport_public_key: "test-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "local-display-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            layout_local_screen: screen(
                "local-device",
                "local-display-1",
                -11960,
                -9000,
                2560,
                1440,
            ),
            remote_screen: screen(
                "peer-device",
                "peer-device-local-display-1",
                -9400,
                -9000,
                2560,
                1440,
            ),
            edge: Edge::Right,
        }
    }

    fn layout_for_target_tests() -> LayoutState {
        LayoutState {
            devices: vec![
                Device {
                    id: "local-device".into(),
                    name: "Local".into(),
                    platform: "macos".into(),
                    host: "192.168.66.92".into(),
                    transport_port: 47833,
                    quic_port: 47834,
                    transport_public_key: "local-public-key".into(),
                    protocol_version: quic_transport::PROTOCOL_VERSION,
                    color: "#2f7af8".into(),
                    online: true,
                    input_ready: false,
                    upgrading: false,
                    upgrading_until_ms: 0,
                    role: "local".into(),
                    source: "detected".into(),
                    screens: vec![screen("local-device", "local-display-1", 0, 0, 1920, 1080)],
                },
                Device {
                    id: "peer-device".into(),
                    name: "Client".into(),
                    platform: "windows".into(),
                    host: "10.0.0.2".into(),
                    transport_port: 52000,
                    quic_port: 52001,
                    transport_public_key: "peer-public-key".into(),
                    protocol_version: quic_transport::PROTOCOL_VERSION,
                    color: "#0f766e".into(),
                    online: true,
                    input_ready: true,
                    upgrading: false,
                    upgrading_until_ms: 0,
                    role: "client".into(),
                    source: "detected".into(),
                    screens: vec![screen(
                        "peer-device",
                        "peer-device-local-display-1",
                        1920,
                        0,
                        1920,
                        1080,
                    )],
                },
            ],
            active_device_id: "local-device".into(),
            selected_screen_id: "local-display-1".into(),
            input_mode: "control".into(),
            machine_role: "server".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            paired_controllers: Vec::new(),
            clipboard_sync: false,
            file_transfer_enabled: true,
            language: "cn".into(),
            theme_mode: "system".into(),
            performance_monitor: false,
            transport_port_mode: "auto".into(),
            transport_port: 47833,
            quic_port: 47834,
            modifier_remap: true,
            modifier_map: crate::default_modifier_map(),
            edge_switch_hotkey: crate::default_edge_switch_hotkey(),
            screen_switch_hotkeys: crate::ScreenSwitchHotkeys::default(),
            drag_edge_guard: true,
            drag_crossing_hold_ms: 800,
            fullscreen_pause: true,
            mouse_smoothing: true,
            smooth_scroll: true,
            reverse_scroll: false,
            pause_app_whitelist: Vec::new(),
            macos_scroll: Default::default(),
        }
    }

    #[test]
    fn cursor_roams_across_remote_device_screens() {
        // Remote device with two stacked screens: a primary and a secondary
        // directly below it (the screenshot's #10086 / #41039 arrangement).
        let device = Device {
            id: "peer-device".into(),
            name: "Client".into(),
            platform: "windows".into(),
            host: "10.0.0.2".into(),
            transport_port: 47833,
            quic_port: 47834,
            transport_public_key: "peer-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            color: "#0f766e".into(),
            online: true,
            input_ready: true,
            upgrading: false,
            upgrading_until_ms: 0,
            role: "client".into(),
            source: "detected".into(),
            screens: vec![
                screen("peer-device", "peer-device-scr-1", 1920, 0, 1920, 1080),
                screen("peer-device", "peer-device-scr-2", 1920, 1080, 1920, 1080),
            ],
        };
        let mut layout = layout_for_target_tests();
        layout.devices.retain(|device| device.id != "peer-device");
        layout.devices.push(device);
        let layout_state = Arc::new(Mutex::new(layout));

        let entry = screen("peer-device", "peer-device-scr-1", 1920, 0, 1920, 1080);
        let target = InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47834".into(),
            target_platform: "windows".into(),
            transport_public_key: "peer-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "scr-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            remote_screen: entry.clone(),
            edge: Edge::Right,
        };
        let mut current_screen = entry.clone();
        current_screen.id = "scr-1".into();
        let mut active = ActiveTarget {
            target,
            current_screen,
            current_screen_id: "scr-1".into(),
            x: 100.0,
            y: 1079.0,
            invert_y: false,
        };

        // Pushing down past the primary's bottom edge roams onto the secondary.
        active.y += 5.0;
        let returned = update_active_remote_screen(&mut active, 0.0, 5.0, &layout_state);
        assert!(
            !returned,
            "crossing onto a sibling screen must not return to local"
        );
        assert_eq!(active.current_screen_id, "scr-2");
        assert!((0.0..1080.0).contains(&active.y));
        assert_eq!(active.x, 100.0);

        // Moving back up crosses back onto the primary screen.
        active.y -= 6.0;
        let returned = update_active_remote_screen(&mut active, 0.0, -6.0, &layout_state);
        assert!(!returned);
        assert_eq!(active.current_screen_id, "scr-1");
    }

    #[test]
    fn cursor_returns_to_local_only_from_entry_edge() {
        let layout_state = Arc::new(Mutex::new(layout_for_target_tests()));
        let entry = screen(
            "peer-device",
            "peer-device-local-display-1",
            1920,
            0,
            1920,
            1080,
        );
        let target = InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47834".into(),
            target_platform: "windows".into(),
            transport_public_key: "peer-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "local-display-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            remote_screen: entry.clone(),
            edge: Edge::Right,
        };
        let mut current_screen = entry.clone();
        current_screen.id = "local-display-1".into();
        let mut active = ActiveTarget {
            target,
            current_screen,
            current_screen_id: "local-display-1".into(),
            x: 0.0,
            y: 500.0,
            invert_y: false,
        };

        // Crossed in via the right edge; moving back left off the entry edge
        // hands control back to the local machine.
        active.x -= 2.0;
        assert!(update_active_remote_screen(
            &mut active,
            -2.0,
            0.0,
            &layout_state
        ));
    }

    #[test]
    fn initial_anchor_warp_delta_does_not_return_to_local() {
        let layout_state = Arc::new(Mutex::new(layout_for_target_tests()));
        let entry = screen(
            "peer-device",
            "peer-device-local-display-1",
            1920,
            0,
            1920,
            1080,
        );
        let target = InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47834".into(),
            target_platform: "windows".into(),
            transport_public_key: "peer-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "local-display-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            remote_screen: entry.clone(),
            edge: Edge::Right,
        };
        let mut current_screen = entry.clone();
        current_screen.id = "local-display-1".into();
        let active = ActiveTarget {
            target,
            current_screen,
            current_screen_id: "local-display-1".into(),
            x: 1.0,
            y: 500.0,
            invert_y: false,
        };
        // Simulate the small leftward delta the entry-anchor warp can inject.
        // (Was -RETURN_EDGE_INSET; now that the inset is 0 for edge-flush returns,
        // use a small fixed delta that still represents the warp's momentum.)
        let dx = -8.0;
        let dy = 0.0;

        let mut unguarded = active.clone();
        unguarded.x += dx;
        assert!(
            update_active_remote_screen(&mut unguarded, dx, dy, &layout_state),
            "without the initial warp guard, the anchor warp delta is mistaken for returning"
        );

        let mut guarded = active.clone();
        let returned = if should_ignore_initial_anchor_warp_delta(guarded.target.edge, dx, dy) {
            false
        } else {
            guarded.x += dx;
            update_active_remote_screen(&mut guarded, dx, dy, &layout_state)
        };

        assert!(!returned);
        assert_eq!(guarded.x, 1.0);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_click_tracker_emits_matching_double_click_counts() {
        let mut tracker = MacClickTracker::default();
        let start = Instant::now();
        let interval = Duration::from_millis(500);

        assert_eq!(
            tracker.event_count(MouseButton::Left, true, 100, 200, start, interval),
            1
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Left,
                false,
                100,
                200,
                start + Duration::from_millis(40),
                interval,
            ),
            1
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Left,
                true,
                102,
                201,
                start + Duration::from_millis(180),
                interval,
            ),
            2,
            "a nearby press inside the interval must raise the click count"
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Left,
                false,
                102,
                201,
                start + Duration::from_millis(220),
                interval,
            ),
            2
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_click_tracker_resets_after_drag_timeout_or_button_change() {
        let mut tracker = MacClickTracker::default();
        let start = Instant::now();
        let interval = Duration::from_millis(500);

        assert_eq!(
            tracker.event_count(MouseButton::Left, true, 10, 10, start, interval),
            1
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Left,
                false,
                30,
                30,
                start + Duration::from_millis(40),
                interval,
            ),
            0,
            "a drag release is not a click"
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Right,
                true,
                10,
                10,
                start + Duration::from_millis(100),
                interval,
            ),
            1,
            "a different button starts its own click chain"
        );
        assert_eq!(
            tracker.event_count(
                MouseButton::Left,
                true,
                10,
                10,
                start + Duration::from_millis(700),
                interval,
            ),
            1,
            "a press after the interval starts over at a single click"
        );
    }

    #[test]
    fn remote_park_point_tucks_mac_at_shared_edge_and_keeps_windows_corner() {
        let target = target_for_coordinate_tests(); // edge=Right, platform=windows, remote 2560x1440
        let remote = target.remote_screen.clone();
        let mut active = ActiveTarget {
            target,
            current_screen: remote.clone(),
            current_screen_id: remote.id.clone(),
            x: 12.0,
            y: 700.0,
            invert_y: false,
        };

        // Controlled Windows keeps the long-standing far-corner park.
        assert_eq!(remote_park_point(&active), (2559, 1439));

        // Controlled macOS entered through OUR right edge (this machine sits
        // to its west): bottom clipping edge, west end, clear of the corner.
        active.target.target_platform = "macos".into();
        assert_eq!(
            remote_park_point(&active),
            (PARK_CORNER_CLEARANCE, 1439)
        );

        // Entered through OUR left edge (this machine to its east): the east
        // edge clips the arrow itself — park there at the exit height.
        active.target.edge = Edge::Left;
        assert_eq!(remote_park_point(&active), (2559, 700));

        // An exit right next to a corner still keeps hot-corner clearance.
        active.y = 1439.0;
        assert_eq!(
            remote_park_point(&active),
            (2559, 1439 - PARK_CORNER_CLEARANCE)
        );
    }

    #[test]
    fn screen_switch_hotkey_matching_requires_exact_modifiers() {
        let hotkeys = crate::ScreenSwitchHotkeys {
            left: "alt+left".into(),
            right: "alt+arrowright".into(),
            up: "disabled".into(),
            down: "alt+shift+down".into(),
        };

        assert!(screen_switch_hotkeys_match_vk(
            &hotkeys,
            0x25,
            HotkeyModifiers {
                alt: true,
                ..HotkeyModifiers::default()
            },
        ));
        assert!(screen_switch_hotkeys_match_vk(
            &hotkeys,
            0x27,
            HotkeyModifiers {
                alt: true,
                ..HotkeyModifiers::default()
            },
        ));
        assert!(screen_switch_hotkeys_match_vk(
            &hotkeys,
            0x28,
            HotkeyModifiers {
                alt: true,
                shift: true,
                ..HotkeyModifiers::default()
            },
        ));
        assert!(!screen_switch_hotkeys_match_vk(
            &hotkeys,
            0x25,
            HotkeyModifiers {
                alt: true,
                shift: true,
                ..HotkeyModifiers::default()
            },
        ));
        assert!(!screen_switch_hotkeys_match_vk(
            &hotkeys,
            0x26,
            HotkeyModifiers {
                alt: true,
                ..HotkeyModifiers::default()
            },
        ));
    }

    #[test]
    fn screen_switch_request_enters_remote_at_screen_center() {
        let layout = layout_for_target_tests();
        let layout_state = Arc::new(Mutex::new(layout.clone()));
        let active = Mutex::new(None);

        match request_screen_switch(SwitchDirection::Right, &layout_state, &layout, &active) {
            SwitchOutcome::Enter(active_target) => {
                assert_eq!(active_target.target.device_id, "peer-device");
                assert_eq!(active_target.x, 960.0);
                assert_eq!(active_target.y, 540.0);
            }
            _ => panic!("expected right quick switch to enter the online client"),
        }
    }

    #[test]
    fn screen_switch_request_moves_between_local_screens() {
        let mut layout = layout_for_target_tests();
        layout.devices[0].screens.push(screen(
            "local-device",
            "local-display-2",
            512,
            1080,
            1512,
            982,
        ));
        let layout_state = Arc::new(Mutex::new(layout.clone()));
        let active = Mutex::new(None);

        match request_screen_switch_from_point(
            SwitchDirection::Down,
            &layout_state,
            &layout,
            &active,
            Some((960.0, 540.0)),
        ) {
            SwitchOutcome::LocalMove {
                from_screen_id,
                to_screen_id,
                x,
                y,
            } => {
                assert_eq!(from_screen_id, "local-display-1");
                assert_eq!(to_screen_id, "local-display-2");
                assert_eq!(x, 1268.0);
                assert_eq!(y, 1571.0);
            }
            _ => panic!("expected down quick switch to move to the lower local screen"),
        }

        match request_screen_switch_from_point(
            SwitchDirection::Up,
            &layout_state,
            &layout,
            &active,
            Some((1268.0, 1571.0)),
        ) {
            SwitchOutcome::LocalMove {
                from_screen_id,
                to_screen_id,
                x,
                y,
            } => {
                assert_eq!(from_screen_id, "local-display-2");
                assert_eq!(to_screen_id, "local-display-1");
                assert_eq!(x, 960.0);
                assert_eq!(y, 540.0);
            }
            _ => panic!("expected up quick switch to move back to the upper local screen"),
        }
    }

    #[test]
    fn local_screen_switch_remembers_points_by_screen_id() {
        let points = Mutex::new(HashMap::new());

        let first_target = remembered_local_screen_point(
            &points,
            "local-display-1",
            "local-display-2",
            Some((333.0, 444.0)),
            (1268.0, 1571.0),
        );
        assert_eq!(first_target, (1268.0, 1571.0));

        let return_target = remembered_local_screen_point(
            &points,
            "local-display-2",
            "local-display-1",
            Some((1200.0, 1500.0)),
            (960.0, 540.0),
        );
        assert_eq!(return_target, (333.0, 444.0));

        let points = points.lock().unwrap();
        assert_eq!(points.get("local-display-1"), Some(&(333.0, 444.0)));
        assert_eq!(points.get("local-display-2"), Some(&(1200.0, 1500.0)));
    }

    #[test]
    fn hotkey_return_uses_recorded_point_then_local_screen_center() {
        let active = crossing_target(&[target_for_coordinate_tests()], 1919.0, 500.0, 40.0, 0.0)
        .expect("target should be active");

        assert_eq!(
            local_hotkey_return_point(&active, Some((321.0, 654.0))),
            (321.0, 654.0)
        );
        assert_eq!(local_hotkey_return_point(&active, None), (960.0, 540.0));
    }

    #[test]
    fn fast_return_pins_remote_cursor_to_entry_edge() {
        let layout_state = Arc::new(Mutex::new(layout_for_target_tests()));
        let entry = screen(
            "peer-device",
            "peer-device-local-display-1",
            1920,
            0,
            1920,
            1080,
        );

        for (edge, x, y, dx, dy, expected_x, expected_y) in [
            (Edge::Right, 240.0, 400.0, -260.0, 18.0, 0.0, 418.0),
            (Edge::Left, 1680.0, 400.0, 260.0, 18.0, 1919.0, 418.0),
            (Edge::Bottom, 500.0, 260.0, 16.0, -300.0, 516.0, 0.0),
            (Edge::Top, 500.0, 820.0, 16.0, 300.0, 516.0, 1079.0),
        ] {
            let target = InputTarget {
                device_id: "peer-device".into(),
                origin_device_id: "peer-local-192-168-66-92".into(),
                cluster_id: "cluster-test".into(),
                pair_secret: "secret-test".into(),
                target_addr: "10.0.0.2:47834".into(),
                target_platform: "windows".into(),
                transport_public_key: "peer-public-key".into(),
                protocol_version: quic_transport::PROTOCOL_VERSION,
                screen_id: "local-display-1".into(),
                local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
                layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
                remote_screen: entry.clone(),
                edge,
            };
            let mut current_screen = entry.clone();
            current_screen.id = "local-display-1".into();
            let mut active = ActiveTarget {
                target,
                current_screen,
                current_screen_id: "local-display-1".into(),
                x: x + dx,
                y: y + dy,
                invert_y: false,
            };

            assert!(update_active_remote_screen(
                &mut active,
                dx,
                dy,
                &layout_state
            ));
            assert_eq!(active.x, expected_x);
            assert_eq!(active.y, expected_y);
        }
    }

    #[test]
    fn input_packet_round_trips_as_messagepack() {
        let packet = InputPacket {
            protocol: INPUT_PROTOCOL.into(),
            target_device_id: "peer-device".into(),
            origin_device_id: "local-device".into(),
            origin_port: 47833,
            origin_transport_public_key: "local-public-key".into(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            event: InputEvent::MouseMove {
                screen_id: "display-1".into(),
                x: 320,
                y: 240,
            },
        };
        let payload = rmp_serde::to_vec_named(&packet).expect("encode input packet");
        let decoded = decode_input_packet(&payload).expect("decode input packet");

        assert_eq!(decoded.protocol, INPUT_PROTOCOL);
        assert_eq!(decoded.target_device_id, "peer-device");
        assert_eq!(decoded.origin_device_id, "local-device");
        assert_eq!(decoded.origin_port, 47833);
        match decoded.event {
            InputEvent::MouseMove { screen_id, x, y } => {
                assert_eq!(screen_id, "display-1");
                assert_eq!(x, 320);
                assert_eq!(y, 240);
            }
            _ => panic!("decoded the wrong input event"),
        }
    }

    #[test]
    fn side_button_events_round_trip_on_the_wire() {
        for button in [MouseButton::Back, MouseButton::Forward] {
            let event = InputEvent::MouseButton { button, down: true };
            let encoded = rmp_serde::to_vec_named(&event).expect("encode side button");
            let decoded: InputEvent = rmp_serde::from_slice(&encoded).expect("decode side button");
            assert_eq!(decoded, InputEvent::MouseButton { button, down: true });
        }
        // Distinct masks so a held side button never aliases another button.
        let masks = [
            mouse_button_mask(MouseButton::Left),
            mouse_button_mask(MouseButton::Right),
            mouse_button_mask(MouseButton::Middle),
            mouse_button_mask(MouseButton::Back),
            mouse_button_mask(MouseButton::Forward),
        ];
        for (i, a) in masks.iter().enumerate() {
            for b in &masks[i + 1..] {
                assert_ne!(a, b, "mouse button masks must be distinct");
            }
        }
    }

    #[test]
    fn borrowed_packet_mirror_encodes_identically_to_input_packet() {
        let packet = InputPacket {
            protocol: INPUT_PROTOCOL.into(),
            target_device_id: "peer-device".into(),
            origin_device_id: "local-device".into(),
            origin_port: 47833,
            origin_transport_public_key: "local-public-key".into(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            event: InputEvent::MouseMove {
                screen_id: "display-1".into(),
                x: 320,
                y: 240,
            },
        };
        let mirror = InputPacketRef {
            protocol: &packet.protocol,
            target_device_id: &packet.target_device_id,
            origin_device_id: &packet.origin_device_id,
            origin_port: packet.origin_port,
            origin_transport_public_key: &packet.origin_transport_public_key,
            origin_protocol_version: packet.origin_protocol_version,
            cluster_id: &packet.cluster_id,
            pair_secret: &packet.pair_secret,
            event: &packet.event,
        };

        assert_eq!(
            rmp_serde::to_vec_named(&packet).expect("encode owned packet"),
            rmp_serde::to_vec_named(&mirror).expect("encode borrowed mirror"),
            "the send-path mirror must stay byte-identical to InputPacket on the wire"
        );
    }

    #[test]
    fn credential_less_packet_omits_the_pairing_block_and_still_decodes() {
        let event = InputEvent::MouseMove {
            screen_id: "display-1".into(),
            x: 320,
            y: 240,
        };
        let full = InputPacketRef {
            protocol: INPUT_PROTOCOL,
            target_device_id: "peer-device",
            origin_device_id: "local-device",
            origin_port: 47833,
            // ~roughly the size of a real base64 transport certificate
            origin_transport_public_key: &"A".repeat(492),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: "cluster-test",
            pair_secret: "secret-test",
            event: &event,
        };
        let lean = InputPacketRef {
            protocol: INPUT_PROTOCOL,
            target_device_id: "peer-device",
            origin_device_id: "",
            origin_port: 47833,
            origin_transport_public_key: "",
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: "",
            pair_secret: "",
            event: &event,
        };

        let full_bytes = rmp_serde::to_vec_named(&full).expect("encode full");
        let lean_bytes = rmp_serde::to_vec_named(&lean).expect("encode lean");
        assert!(
            lean_bytes.len() + 400 < full_bytes.len(),
            "credential-less packet ({} bytes) should be far smaller than full ({} bytes)",
            lean_bytes.len(),
            full_bytes.len()
        );

        // The receiver still decodes it, with the omitted credentials defaulted
        // to empty so it takes the cached-authorization path.
        let decoded = decode_input_packet(&lean_bytes).expect("decode lean packet");
        assert!(decoded.pair_secret.is_empty());
        assert!(decoded.origin_transport_public_key.is_empty());
        assert_eq!(decoded.target_device_id, "peer-device");
        assert_eq!(decoded.origin_port, 47833);
    }

    #[test]
    fn credential_refresh_and_origin_cache_windows() {
        let t0 = Instant::now();
        // Refresh: due when never sent, not due within the window, due after it.
        assert!(credential_send_due(None, t0));
        assert!(!credential_send_due(
            Some(t0),
            t0 + INPUT_FULL_CRED_REFRESH - Duration::from_millis(1)
        ));
        assert!(credential_send_due(Some(t0), t0 + INPUT_FULL_CRED_REFRESH));

        // Cache: never-seen is not fresh; within TTL is fresh; past TTL is not.
        assert!(!origin_authorization_fresh(None, t0));
        assert!(origin_authorization_fresh(
            Some(t0),
            t0 + INPUT_ORIGIN_CACHE_TTL - Duration::from_millis(1)
        ));
        assert!(!origin_authorization_fresh(
            Some(t0),
            t0 + INPUT_ORIGIN_CACHE_TTL
        ));

        // The refresh interval must stay under the cache TTL, or authorization
        // would lapse between credentialled packets.
        assert!(INPUT_FULL_CRED_REFRESH < INPUT_ORIGIN_CACHE_TTL);
    }

    #[test]
    fn input_packet_context_uses_stable_peer_origin_id() {
        let layout = layout_for_target_tests();
        let expected_origin_id = crate::local_peer_from_layout(&layout).id;
        let layout_state = Arc::new(Mutex::new(layout));
        let target = target_for_coordinate_tests();

        // Key events consult the live layout and must resolve the stable id.
        let context = input_packet_context(
            &target,
            InputEvent::Key {
                key_code: 0x41,
                down: true,
            },
            &layout_state,
        );
        assert_ne!(expected_origin_id, "local-device");
        assert_eq!(context.origin_device_id, expected_origin_id);

        // Mouse events take the hot path: the context cached on the target at
        // build time, which carries the same stable id without a layout lock.
        let context = input_packet_context(
            &target,
            InputEvent::MouseMove {
                screen_id: "local-display-1".into(),
                x: 10,
                y: 20,
            },
            &layout_state,
        );
        assert_eq!(context.origin_device_id, target.origin_device_id);
    }

    #[test]
    fn input_packet_context_uses_cached_target_when_layout_lock_is_busy() {
        let layout_state = Arc::new(Mutex::new(layout_for_target_tests()));
        let _held_layout = layout_state.lock().expect("hold layout lock");
        let target = target_for_coordinate_tests();
        let layout_state_for_thread = Arc::clone(&layout_state);
        let (tx, rx) = std::sync::mpsc::channel();

        thread::spawn(move || {
            let context = input_packet_context(
                &target,
                InputEvent::MouseMove {
                    screen_id: "local-display-1".into(),
                    x: 10,
                    y: 20,
                },
                &layout_state_for_thread,
            );
            tx.send(context).expect("send packet context");
        });

        let context = rx
            .recv_timeout(Duration::from_millis(50))
            .expect("packet context should not block on the layout lock");
        assert_eq!(context.origin_device_id, "peer-local-192-168-66-92");
        assert_eq!(context.cluster_id, "cluster-test");
        assert_eq!(context.pair_secret, "secret-test");
        assert!(context.peer.is_some());
    }

    #[test]
    fn input_packet_requires_pair_secret() {
        let mut layout = layout_for_target_tests();
        layout.machine_role = "client".into();
        layout.paired_controllers = vec![crate::PairedController {
            id: "server".into(),
            name: "Server".into(),
            host: "server".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: 1,
        }];
        let mut packet = InputPacket {
            protocol: INPUT_PROTOCOL.into(),
            target_device_id: "local-device".into(),
            origin_device_id: "server".into(),
            origin_port: 47834,
            origin_transport_public_key: "server-key".into(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            pair_secret: "wrong".into(),
            event: InputEvent::MouseMove {
                screen_id: "local-display-1".into(),
                x: 1,
                y: 1,
            },
        };

        assert!(!packet_authorized(&layout, &packet));
        packet.pair_secret = layout.pair_secret.clone();
        assert!(packet_authorized(&layout, &packet));
        packet.origin_transport_public_key = "attacker-key".into();
        packet.origin_device_id = "attacker".into();
        assert!(!packet_authorized(&layout, &packet));
        packet.origin_transport_public_key.clear();
        packet.origin_device_id = "server".into();
        assert!(packet_authorized(&layout, &packet));
    }

    #[test]
    fn input_packet_accepts_legacy_origin_after_transport_key_rotation() {
        let mut layout = layout_for_target_tests();
        layout.machine_role = "client".into();
        layout.paired_controllers = vec![crate::PairedController {
            id: "peer-server-local-10-0-0-1".into(),
            name: "Server".into(),
            host: "server.local".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-old-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: 1,
        }];
        let packet = InputPacket {
            protocol: INPUT_PROTOCOL.into(),
            target_device_id: "local-device".into(),
            origin_device_id: "local-device".into(),
            origin_port: 47834,
            origin_transport_public_key: "server-rotated-key".into(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            pair_secret: layout.pair_secret.clone(),
            event: InputEvent::MouseMove {
                screen_id: "local-display-1".into(),
                x: 1,
                y: 1,
            },
        };

        assert!(packet_authorized(&layout, &packet));

        layout.paired_controllers.push(crate::PairedController {
            id: "peer-other-server".into(),
            name: "Other".into(),
            host: "other.local".into(),
            ip: "10.0.0.3".into(),
            transport_public_key: "other-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: 2,
        });
        assert!(!packet_authorized(&layout, &packet));
    }

    #[test]
    fn input_event_maps_relative_coordinates_to_native_command() {
        let layout = layout_for_target_tests();
        let mut native_layout = layout.clone();
        native_layout.devices[0].screens[0].width = 3840;
        native_layout.devices[0].screens[0].height = 2160;

        let command = input_event_to_command(
            &layout,
            &native_layout,
            InputEvent::MouseMove {
                screen_id: "local-display-1".into(),
                x: 960,
                y: 540,
            },
        )
        .expect("mouse move should map to command");

        assert_eq!(
            command,
            InputCommand::MouseMove {
                x: 1920,
                y: 1080,
                drag_button: None,
            }
        );
    }

    #[test]
    fn input_control_packet_round_trips_as_messagepack() {
        let packet = InputControlPacket {
            protocol: INPUT_CONTROL_PROTOCOL.into(),
            target_device_id: "local-device".into(),
            origin_device_id: "server".into(),
            origin_transport_public_key: "server-key".into(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            command: InputControlCommand::SecureAttention,
        };
        let payload = rmp_serde::to_vec_named(&packet).expect("encode input control packet");
        let decoded = decode_input_control_packet(&payload).expect("decode input control packet");

        assert_eq!(decoded.protocol, INPUT_CONTROL_PROTOCOL);
        assert_eq!(decoded.target_device_id, "local-device");
        assert_eq!(decoded.command, InputControlCommand::SecureAttention);
    }

    #[test]
    fn input_control_packet_uses_pairing_authorization() {
        let mut layout = layout_for_target_tests();
        layout.machine_role = "client".into();
        layout.paired_controllers = vec![crate::PairedController {
            id: "server".into(),
            name: "Server".into(),
            host: "server".into(),
            ip: "10.0.0.1".into(),
            transport_public_key: "server-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            paired_at_ms: 1,
        }];
        let mut packet = InputControlPacket {
            protocol: INPUT_CONTROL_PROTOCOL.into(),
            target_device_id: "local-device".into(),
            origin_device_id: "server".into(),
            origin_transport_public_key: "server-key".into(),
            origin_protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: layout.cluster_id.clone(),
            pair_secret: "wrong".into(),
            command: InputControlCommand::SecureAttention,
        };

        assert!(!control_packet_authorized(&layout, &packet));
        packet.pair_secret = layout.pair_secret.clone();
        assert!(control_packet_authorized(&layout, &packet));
        packet.origin_transport_public_key = "attacker-key".into();
        packet.origin_device_id = "attacker".into();
        assert!(!control_packet_authorized(&layout, &packet));
    }

    #[test]
    fn clipboard_target_expires() {
        let target = Arc::new(Mutex::new(Some(ClipboardTarget {
            device_id: "peer-device".into(),
            addr: "10.0.0.2:47833".into(),
            transport_public_key: "peer-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            expires_at: Some(Instant::now() - Duration::from_millis(1)),
        })));

        assert!(current_clipboard_target(&target).is_none());
        assert!(target.lock().expect("target lock").is_none());
    }

    #[test]
    fn crossing_accepts_native_screen_coordinates() {
        let target = target_for_coordinate_tests();

        // Native width 1920, so the cursor must reach the edge pixel x=1919
        // (CROSSING_MARGIN=1) before a crossing is accepted.
        let mapped = crossing_layout_point(&target, 1919.0, 500.0, 5.0, 0.0)
            .expect("native edge should cross");

        assert!(mapped.0 > -9404.0);
        assert!(mapped.0 <= -9400.0);
    }

    #[test]
    fn fast_crossing_carries_entry_delta_into_remote() {
        let target = target_for_coordinate_tests();
        let active = crossing_target(&[target], 1919.0, 500.0, 40.0, 0.0)
            .expect("fast edge movement should cross");

        assert!(
            active.x > 1.0,
            "dropping the crossing delta makes the cursor feel stuck at the edge"
        );
    }

    #[test]
    fn crossing_rejects_raw_layout_coordinates() {
        let target = target_for_coordinate_tests();

        assert!(crossing_layout_point(&target, -9401.0, -8500.0, 5.0, 0.0).is_none());
    }

    #[test]
    fn crossing_uses_native_edge_before_mapping_to_layout() {
        let target = InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47833".into(),
            target_platform: "windows".into(),
            transport_public_key: "test-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "local-display-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 3840, 2160),
            layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            remote_screen: screen(
                "peer-device",
                "peer-device-local-display-1",
                1920,
                0,
                1728,
                1117,
            ),
            edge: Edge::Right,
        };

        assert!(crossing_layout_point(&target, 1918.0, 600.0, 5.0, 0.0).is_none());

        // Native width 3840, so the edge pixel is x=3839; the cursor must reach
        // it (CROSSING_MARGIN=1) before crossing.
        let mapped = crossing_layout_point(&target, 3839.0, 1200.0, 5.0, 0.0)
            .expect("native edge should cross");

        assert!(mapped.0 > 1916.0);
        assert!(mapped.0 <= 1920.0);
    }

    #[test]
    fn crossing_rejects_fast_jump_from_middle() {
        let target = InputTarget {
            device_id: "peer-device".into(),
            origin_device_id: "peer-local-192-168-66-92".into(),
            cluster_id: "cluster-test".into(),
            pair_secret: "secret-test".into(),
            target_addr: "10.0.0.2:47833".into(),
            target_platform: "windows".into(),
            transport_public_key: "test-public-key".into(),
            protocol_version: quic_transport::PROTOCOL_VERSION,
            screen_id: "local-display-1".into(),
            local_screen: screen("local-device", "local-display-1", 0, 0, 3840, 2160),
            layout_local_screen: screen("local-device", "local-display-1", 0, 0, 1920, 1080),
            remote_screen: screen(
                "peer-device",
                "peer-device-local-display-1",
                1920,
                0,
                1728,
                1117,
            ),
            edge: Edge::Right,
        };

        assert!(crossing_layout_point(&target, 3838.0, 1200.0, 900.0, 0.0).is_none());
    }

    #[test]
    fn modifier_key_mapping_handles_sided_keys_and_caps_lock() {
        assert_eq!(windows_vk_to_mac_key(0x10), Some(56));
        assert_eq!(windows_vk_to_mac_key(0xA0), Some(56));
        assert_eq!(windows_vk_to_mac_key(0xA1), Some(60));
        assert_eq!(windows_vk_to_mac_key(0x11), Some(59));
        assert_eq!(windows_vk_to_mac_key(0xA2), Some(59));
        assert_eq!(windows_vk_to_mac_key(0xA3), Some(62));
        assert_eq!(windows_vk_to_mac_key(0x12), Some(58));
        assert_eq!(windows_vk_to_mac_key(0xA4), Some(58));
        assert_eq!(windows_vk_to_mac_key(0xA5), Some(61));
        assert_eq!(windows_vk_to_mac_key(0x14), Some(57));
        assert_eq!(windows_vk_to_mac_key(0x5B), Some(55));
        assert_eq!(windows_vk_to_mac_key(0x5C), Some(54));

        assert_eq!(mac_key_to_windows_vk(56), Some(0xA0));
        assert_eq!(mac_key_to_windows_vk(60), Some(0xA1));
        assert_eq!(mac_key_to_windows_vk(57), Some(0x14));
        assert_eq!(mac_key_to_windows_vk(58), Some(0xA4));
        assert_eq!(mac_key_to_windows_vk(61), Some(0xA5));
        assert_eq!(mac_key_to_windows_vk(59), Some(0xA2));
        assert_eq!(mac_key_to_windows_vk(62), Some(0xA3));
    }

    #[test]
    fn key_mapping_handles_space_numpad_and_function_keys() {
        assert_eq!(windows_vk_to_mac_key(0x20), Some(49));
        assert_eq!(mac_key_to_windows_vk(49), Some(0x20));

        for (vk, mac) in [
            (0x60, 82),
            (0x61, 83),
            (0x62, 84),
            (0x63, 85),
            (0x64, 86),
            (0x65, 87),
            (0x66, 88),
            (0x67, 89),
            (0x68, 91),
            (0x69, 92),
            (0x6A, 67),
            (0x6B, 69),
            (0x6D, 78),
            (0x6E, 65),
            (0x6F, 75),
        ] {
            assert_eq!(windows_vk_to_mac_key(vk), Some(mac));
        }

        for (vk, mac) in [
            (0x70, 122),
            (0x71, 120),
            (0x72, 99),
            (0x73, 118),
            (0x74, 96),
            (0x75, 97),
            (0x76, 98),
            (0x77, 100),
            (0x78, 101),
            (0x79, 109),
            (0x7A, 103),
            (0x7B, 111),
        ] {
            assert_eq!(windows_vk_to_mac_key(vk), Some(mac));
            assert_eq!(mac_key_to_windows_vk(mac), Some(vk));
        }
    }

    #[test]
    fn default_modifier_map_swaps_control_and_meta() {
        let map = crate::default_modifier_map();

        // Control (any side) -> Meta (Windows key / macOS Command)
        assert_eq!(
            remap_modifier_vk(0x11, &map.control, &map.alt, &map.meta),
            0x5B
        );
        assert_eq!(
            remap_modifier_vk(0xA2, &map.control, &map.alt, &map.meta),
            0x5B
        );
        assert_eq!(
            remap_modifier_vk(0xA3, &map.control, &map.alt, &map.meta),
            0x5B
        );
        // Meta -> Control
        assert_eq!(
            remap_modifier_vk(0x5B, &map.control, &map.alt, &map.meta),
            0x11
        );
        assert_eq!(
            remap_modifier_vk(0x5C, &map.control, &map.alt, &map.meta),
            0x11
        );
        // Alt stays as itself (left/right preserved via "same")
        assert_eq!(
            remap_modifier_vk(0xA4, &map.control, &map.alt, &map.meta),
            0xA4
        );
        // Non-modifier keys are untouched (e.g. the letter C)
        assert_eq!(
            remap_modifier_vk(0x43, &map.control, &map.alt, &map.meta),
            0x43
        );
    }

    #[test]
    fn custom_modifier_map_is_honored() {
        // User keeps Ctrl literal but maps the Windows/Command key to Alt.
        assert_eq!(remap_modifier_vk(0x11, "same", "same", "alt"), 0x11);
        assert_eq!(remap_modifier_vk(0x5B, "same", "same", "alt"), 0x12);
    }

    #[test]
    fn remap_skips_unknown_target_platform() {
        let layout = Arc::new(Mutex::new(layout_for_target_tests()));
        let mut target = {
            let guard = layout.lock().expect("layout lock");
            build_input_targets(&guard, &guard)
                .into_iter()
                .next()
                .expect("one target")
        };

        // An unknown target platform must never be remapped, regardless of the
        // configured map, so we cannot accidentally mangle keys for peers we
        // cannot classify.
        target.target_platform = "unknown".into();
        let event = remap_event_for_target(
            InputEvent::Key {
                key_code: 0x11,
                down: true,
            },
            &target,
            &layout,
        );
        match event {
            InputEvent::Key { key_code, .. } => assert_eq!(key_code, 0x11),
            _ => panic!("expected key event"),
        }
    }

    #[test]
    fn remap_passes_through_non_key_events() {
        let layout = Arc::new(Mutex::new(layout_for_target_tests()));
        let target = {
            let guard = layout.lock().expect("layout lock");
            build_input_targets(&guard, &guard)
                .into_iter()
                .next()
                .expect("one target")
        };

        let event = remap_event_for_target(
            InputEvent::Scroll {
                delta_x: 1,
                delta_y: -2,
            },
            &target,
            &layout,
        );
        assert!(matches!(
            event,
            InputEvent::Scroll {
                delta_x: 1,
                delta_y: -2
            }
        ));
    }

    #[test]
    fn input_targets_use_peer_quic_port() {
        let layout = layout_for_target_tests();
        let targets = build_input_targets(&layout, &layout);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].target_addr, "10.0.0.2:52001");
    }

    #[test]
    fn input_targets_cache_pairing_context_for_hot_path() {
        let layout = layout_for_target_tests();
        let expected_origin_id = crate::local_peer_from_layout(&layout).id;
        let targets = build_input_targets(&layout, &layout);

        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].origin_device_id, expected_origin_id);
        assert_eq!(targets[0].cluster_id, "cluster-test");
        assert_eq!(targets[0].pair_secret, "secret-test");
    }

    #[test]
    fn input_targets_require_peer_input_ready() {
        let mut layout = layout_for_target_tests();
        layout.devices[1].input_ready = false;

        let targets = build_input_targets(&layout, &layout);

        assert!(targets.is_empty());
    }

    #[test]
    fn input_targets_ignore_overlapping_remote_screens() {
        let mut layout = layout_for_target_tests();
        layout.devices[1].screens[0].x = 1860;

        let targets = build_input_targets(&layout, &layout);

        assert!(targets.is_empty());
    }
}
