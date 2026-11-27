//! Extension lifecycle, DNR filter-startup gate, anchored OSR popup state machine, and
//! invisible engine restart-and-restore for [`crate::BrowserHost`].
//!
//! Mirrors upstream `packages/gui-plugins/src/browser/profile/{filter-startup.ts,extension-api/popup.ts}`:
//! - Opening the browser pane or an `about:blank` / preview / isolated tab never waits for
//!   extensions (`0 ms` hold).
//! - When a DNR filter extension (`declarativeNetRequest` / `declarativeNetRequestWithHostAccess`)
//!   is active on startup, local `http(s)` navigations hold (with the pane already painted and
//!   cancellable via Stop or replacement navigation) until `EngineMsg::ExtensionFiltersReady`
//!   arrives or the 5 s advisory ceiling elapses.
//! - Anchored popups follow Chrome's `[25..800]×[25..600]` bounds, RTL mirroring, and toolbar
//!   floor clamping so a popup never covers the app's own chrome.

use std::collections::{HashSet, VecDeque};
use std::sync::PoisonError;
use std::time::Duration;

use rune::time::Instant;
use rune::{Align, Drag, El, Key, KeyEvent, Phase, PointerButton, PointerEvent, WheelEvent, el};

use crate::client::BrowserHost;
use crate::frame::{self, TabSurface};
use crate::protocol::{EngineMsg, ExtensionActionSnapshot, HostMsg, PointerBtn, pack_mods};

/// Verified `(id, sha256, directory)` tuple passed on `--extension` when spawning the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartupExtensionSpec {
    pub id: String,
    pub digest: String,
    pub directory: String,
}

impl StartupExtensionSpec {
    pub fn cli_arg(&self) -> String {
        format!("{}:{}:{}", self.id, self.digest, self.directory)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl From<browser_extensions::VerifiedStartupSpec> for StartupExtensionSpec {
    fn from(spec: browser_extensions::VerifiedStartupSpec) -> Self {
        Self { id: spec.id, digest: spec.sha256, directory: spec.dir }
    }
}

/// Chrome's minimum and maximum extension popup dimensions in CSS points (`popup.ts:5-7`).
pub const POPUP_MIN_W: u32 = 25;
pub const POPUP_MIN_H: u32 = 25;
pub const POPUP_MAX_W: u32 = 800;
pub const POPUP_MAX_H: u32 = 600;

/// Shared advisory budget for DNR filter startup readiness (`filter-startup.ts:8`).
pub const FILTER_READY_DEADLINE: Duration = Duration::from_secs(5);

/// Anchor rectangle in window content points (`popup.ts:16-21`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PopupAnchor {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Where an extension popup belongs inside the window's content (`popup.ts:16-21`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PopupPlacement {
    pub anchor: PopupAnchor,
    pub top: f32,
    pub page_top: Option<f32>,
    pub rtl: bool,
}

/// Resolved pixel-aligned popup rect in window content points (`popup.ts:32-47`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PopupRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// Computes clamped popup bounds within window `content` (`popup.ts:32-47`).
/// Never lets a popup overlap above `anchor.bottom` or `page_top` (the app's own toolbar/bars).
pub fn popup_bounds(
    placement: PopupPlacement,
    size: (u32, u32),
    content: (f32, f32),
) -> PopupRect {
    let cw = whole_f(content.0, 1, 16_384);
    let ch = whole_f(content.1, 1, 16_384);
    let width = whole_i(size.0 as i32, POPUP_MIN_W as i32, (POPUP_MAX_W as i32).min(cw));
    let height = whole_i(size.1 as i32, POPUP_MIN_H as i32, (POPUP_MAX_H as i32).min(ch));
    let left = whole_f(placement.anchor.x, 0, cw);
    let right = whole_f(placement.anchor.x + placement.anchor.width, left, cw);
    let anchor_bottom = whole_f(placement.anchor.y + placement.anchor.height, 0, ch);
    let page_floor = placement.page_top.map_or(0, |t| whole_f(t, 0, ch));
    let floor = anchor_bottom.max(page_floor);
    let preferred_y = whole_f(placement.top, floor, ch);
    let y = preferred_y.min(ch - height).max(floor);
    let preferred_x = if placement.rtl { left } else { right - width };
    let x = whole_i(preferred_x, 0, (cw - width).max(0));
    let clipped_h = height.min((ch - y).max(0)).max(0);
    PopupRect { x, y, width: width.max(0) as u32, height: clipped_h as u32 }
}

fn whole_f(value: f32, low: i32, high: i32) -> i32 {
    let rounded = if value.is_finite() { value.round() as i32 } else { low };
    whole_i(rounded, low, high)
}

fn whole_i(value: i32, low: i32, high: i32) -> i32 {
    value.clamp(low, high.max(low))
}

/// Explicit state machine for the one-at-a-time anchored extension popup.
#[derive(Clone, Debug, Default)]
pub enum PopupState {
    #[default]
    Closed,
    AwaitingUrl {
        ext_id: String,
        placement: PopupPlacement,
        about_tab: Option<u64>,
    },
    Open {
        ext_id: String,
        tab_id: u64,
        url: String,
        placement: PopupPlacement,
        size: (u32, u32),
        about_tab: Option<u64>,
        surface: TabSurface,
    },
}

/// View of a currently open extension popup for desktop rendering.
pub struct OpenExtensionPopup<'a> {
    pub ext_id: &'a str,
    pub tab_id: u64,
    pub placement: PopupPlacement,
    pub size: (u32, u32),
    pub about_tab: Option<u64>,
}

/// Explicit state machine holding local `http(s)` navigations until startup DNR filters are ready
/// (`filter-startup.ts`).
#[derive(Clone, Debug, Default)]
pub enum FilterGateState {
    #[default]
    Ready,
    /// Engine with DNR filter extensions is starting; hold `http(s)` navigations without ticking
    /// the 5 s filter deadline until `EngineMsg::Ready` arrives.
    PendingReady {
        held: Vec<(u64, String)>,
    },
    /// Engine is `Ready`; hold `http(s)` navigations until `EngineMsg::ExtensionFiltersReady` or
    /// `deadline` (5 s advisory ceiling).
    Waiting {
        deadline: Instant,
        held: Vec<(u64, String)>,
    },
}

impl FilterGateState {
    pub fn arm_pending(&mut self) {
        let held = match std::mem::take(self) {
            Self::PendingReady { held } | Self::Waiting { held, .. } => held,
            Self::Ready => Vec::new(),
        };
        *self = Self::PendingReady { held };
    }

    pub fn start_deadline(&mut self, now: Instant) {
        let held = match std::mem::take(self) {
            Self::PendingReady { held } => held,
            Self::Waiting { deadline, held } => {
                *self = Self::Waiting { deadline, held };
                return;
            }
            Self::Ready => return,
        };
        *self = Self::Waiting { deadline: now + FILTER_READY_DEADLINE, held };
    }

    pub fn is_waiting(&self) -> bool {
        !matches!(self, Self::Ready)
    }

    pub fn is_holding(&self, tab_id: u64) -> bool {
        match self {
            Self::Ready => false,
            Self::PendingReady { held } | Self::Waiting { held, .. } => {
                held.iter().any(|(id, _)| *id == tab_id)
            }
        }
    }

    pub fn held_url(&self, tab_id: u64) -> Option<&str> {
        let held = match self {
            Self::Ready => return None,
            Self::PendingReady { held } | Self::Waiting { held, .. } => held,
        };
        held.iter().find(|(id, _)| *id == tab_id).map(|(_, u)| u.as_str())
    }

    pub fn deadline(&self) -> Option<Instant> {
        match self {
            Self::Ready | Self::PendingReady { .. } => None,
            Self::Waiting { deadline, .. } => Some(*deadline),
        }
    }

    /// Holds `url` for `tab_id` if this gate is waiting and `url` is an `http(s)` navigation.
    /// Returns `true` when held (caller must NOT send `HostMsg::Navigate` yet).
    pub fn maybe_hold(&mut self, tab_id: u64, url: &str) -> bool {
        let held = match self {
            Self::Ready => return false,
            Self::PendingReady { held } | Self::Waiting { held, .. } => held,
        };
        if !is_http_url(url) {
            return false;
        }
        if let Some(existing) = held.iter_mut().find(|(id, _)| *id == tab_id) {
            existing.1 = url.to_owned();
        } else {
            held.push((tab_id, url.to_owned()));
        }
        true
    }

    /// Drops any held navigation for `tab_id` (user pressed Stop, closed the tab, or navigated
    /// to a non-HTTP URL).
    pub fn cancel_tab(&mut self, tab_id: u64) {
        if let Self::PendingReady { held } | Self::Waiting { held, .. } = self {
            held.retain(|(id, _)| *id != tab_id);
        }
    }

    /// Releases all held navigations immediately (engine reported `ExtensionFiltersReady` or
    /// the 5 s advisory ceiling elapsed).
    pub fn release(&mut self) -> Vec<(u64, String)> {
        match std::mem::replace(self, Self::Ready) {
            Self::Ready => Vec::new(),
            Self::PendingReady { held } | Self::Waiting { held, .. } => held,
        }
    }

    /// Advances the advisory deadline; returns released `(tab_id, url)` pairs if it expired.
    pub fn tick(&mut self, now: Instant) -> Vec<(u64, String)> {
        if let Self::Waiting { deadline, .. } = self
            && now >= *deadline
        {
            return self.release();
        }
        Vec::new()
    }
}

/// Extension-initiated `chrome.tabs.create` / `runtime.openOptionsPage` request waiting for the
/// desktop app to open a managed pane tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtensionOpenRequest {
    pub request_id: u64,
    pub ext_id: String,
    pub url: String,
    pub active: bool,
}

/// Runtime result of an `UninstallExtension` or `ReloadExtension` command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionLifecycleEvent {
    Uninstalled { ext_id: String, ok: bool, error: Option<String> },
    Reloaded { ext_id: String, ok: bool, error: Option<String> },
}

/// Extension `chrome.notifications` OS delivery request queued for the desktop app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionNotificationOp {
    Show { ext_id: String, ext_name: String, notif_id: String, body: String },
    Clear { ext_id: String, notif_id: String },
}

/// Extension state owned by [`BrowserHost`].
#[derive(Default)]
pub(crate) struct BrowserExtensionsState {
    pub startup_specs: Vec<StartupExtensionSpec>,
    pub startup_has_filters: bool,
    /// IDs passed on `--extension` when the currently running engine spawned (eligible for
    /// zero-restart `ReloadExtension` after a runtime disable).
    pub launched_ext_ids: HashSet<String>,
    pub actions: Vec<ExtensionActionSnapshot>,
    pub popup: PopupState,
    pub popup_focused: bool,
    pub popup_needs_focus: bool,
    pub popup_refocus_page: bool,
    pub pane_size: Option<(f32, f32)>,
    pub window_size: Option<(f32, f32)>,
    pub filter_gate: FilterGateState,
    pub open_requests: VecDeque<ExtensionOpenRequest>,
    pub focus_requests: VecDeque<u64>,
    pub integrity_failures: VecDeque<String>,
    pub lifecycle_events: VecDeque<ExtensionLifecycleEvent>,
    pub notification_ops: VecDeque<ExtensionNotificationOp>,
    pub notifications_enabled: Option<bool>,
}

pub(crate) fn is_http_url(url: &str) -> bool {
    let t = url.trim();
    (t.len() >= 7 && t.as_bytes()[..7].eq_ignore_ascii_case(b"http://"))
        || (t.len() >= 8 && t.as_bytes()[..8].eq_ignore_ascii_case(b"https://"))
}

impl BrowserHost {
    /// Whether the running engine was started with extensions (`--extension`): it then keeps its
    /// pages' `Page.*` DevTools events to itself (the engine's `cdp_ledger`).
    pub fn extensions_active(&self) -> bool {
        !self.extensions.launched_ext_ids.is_empty()
    }

    /// Whether the connected engine advertises MV3 extension support (`Ready.extensions`).
    pub fn engine_extensions(&self) -> bool {
        self.caps.extensions()
    }

    /// Sets the verified `--extension <id>:<sha256>:<dir>` list passed on future engine spawns.
    /// `has_filters` is true when at least one verified extension declares `declarativeNetRequest`.
    /// Returns `true` only when the startup spec list or filter flag actually changed (idempotent).
    pub fn set_startup_extensions(&mut self, specs: Vec<StartupExtensionSpec>, has_filters: bool) -> bool {
        if self.extensions.startup_specs == specs && self.extensions.startup_has_filters == has_filters {
            return false;
        }
        self.extensions.startup_specs = specs;
        self.extensions.startup_has_filters = has_filters;
        true
    }

    #[cfg_attr(not(feature = "engine"), allow(dead_code))]
    pub(crate) fn startup_extension_args(&self) -> Vec<String> {
        self.extensions.startup_specs.iter().map(StartupExtensionSpec::cli_arg).collect()
    }

    #[cfg_attr(not(feature = "engine"), allow(dead_code))]
    pub(crate) fn record_launched_extensions(&mut self) {
        self.extensions.launched_ext_ids = self
            .extensions
            .startup_specs
            .iter()
            .map(|s| s.id.clone())
            .collect();
        if self.extensions.startup_has_filters {
            self.extensions.filter_gate.arm_pending();
        } else {
            let _ = self.extensions.filter_gate.release();
        }
    }

    /// Whether `url` is a `chrome-extension://<id>/...` page belonging to a currently configured
    /// or loaded extension.
    pub(crate) fn is_loaded_extension_url(&self, url: &str) -> bool {
        let t = url.trim();
        if t.len() <= 19 || !t.as_bytes()[..19].eq_ignore_ascii_case(b"chrome-extension://") {
            return false;
        }
        let rest = &t[19..];
        let Some((host, _)) = rest.split_once('/') else {
            return false;
        };
        if !browser_extensions::crx::is_valid_ext_id(host) {
            return false;
        }
        self.extensions.actions.iter().any(|a| a.ext_id == host)
            || self.extensions.startup_specs.iter().any(|s| s.id == host)
    }

    /// Opens an options or extension page tab for a user click (never isolated, never agent-driven).
    pub fn open_extension_page_tab(&mut self, raw_url: &str) -> u64 {
        self.open_tab_full(
            raw_url,
            Vec::new(),
            crate::protocol::ContextKind::Default,
            false,
            false,
            true,
            None,
        )
    }

    /// Live toolbar action snapshots for loaded extensions.
    pub fn extension_actions(&self) -> &[ExtensionActionSnapshot] {
        &self.extensions.actions
    }

    /// Whether a DNR filter readiness gate is currently holding `http(s)` navigations.
    pub fn waiting_for_filters(&self) -> bool {
        self.extensions.filter_gate.is_waiting()
    }

    /// Ticks the 5 s DNR filter-readiness advisory ceiling and flushes held `http(s)` navigations
    /// when it expires; returns `(changed, next_wake)`.
    pub fn tick_extensions(&mut self, now: Instant) -> (bool, Option<Instant>) {
        let released = self.extensions.filter_gate.tick(now);
        let changed = !released.is_empty();
        for (tab_id, url) in released {
            self.send(HostMsg::Navigate { tab_id, url });
        }
        (changed, self.extensions.filter_gate.deadline())
    }

    /// User clicked extension `ext_id` in the toolbar puzzle menu (`popup.ts:75`).
    pub fn click_extension(&mut self, ext_id: &str, placement: PopupPlacement, about_tab: Option<u64>) {
        if !self.engine_extensions() {
            return;
        }
        // Clicking the already-open popup's extension toggles it closed.
        if let PopupState::Open { ext_id: open_id, .. } = &self.extensions.popup
            && open_id == ext_id
        {
            self.close_extension_popup();
            return;
        }
        self.close_extension_popup();
        self.extensions.popup = PopupState::AwaitingUrl {
            ext_id: ext_id.to_owned(),
            placement,
            about_tab,
        };
        self.send(HostMsg::ClickExtension { ext_id: ext_id.to_owned() });
    }

    /// Closes any open anchored extension popup and releases its OSR browser & surface.
    pub fn close_extension_popup(&mut self) -> bool {
        let was_focused = self.extensions.popup_focused;
        self.extensions.popup_focused = false;
        self.extensions.popup_needs_focus = false;
        match std::mem::replace(&mut self.extensions.popup, PopupState::Closed) {
            PopupState::Closed => false,
            PopupState::AwaitingUrl { .. } => true,
            PopupState::Open { tab_id, surface, .. } => {
                if was_focused {
                    self.extensions.popup_refocus_page = true;
                }
                surface.clear();
                self.surfaces
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&tab_id);
                self.send(HostMsg::CloseTab { tab_id });
                true
            }
        }
    }

    /// Closes any open anchored extension popup if its backing tab matches `tab_id`.
    pub(crate) fn close_popup_if_tab(&mut self, tab_id: u64) -> bool {
        if let PopupState::Open { tab_id: open_id, .. } = &self.extensions.popup
            && *open_id == tab_id
        {
            return self.close_extension_popup();
        }
        false
    }

    pub fn popup_tab_id(&self) -> Option<u64> {
        match &self.extensions.popup {
            PopupState::Open { tab_id, .. } => Some(*tab_id),
            _ => None,
        }
    }

    pub fn popup_focused(&self) -> bool {
        self.extensions.popup_focused && self.popup_tab_id().is_some()
    }

    pub fn set_popup_focused(&mut self, focused: bool) {
        self.extensions.popup_focused = focused && self.popup_tab_id().is_some();
    }

    pub fn take_popup_needs_focus(&mut self) -> bool {
        std::mem::take(&mut self.extensions.popup_needs_focus) && self.popup_tab_id().is_some()
    }

    pub fn take_popup_refocus_page(&mut self) -> bool {
        std::mem::take(&mut self.extensions.popup_refocus_page) && self.popup_tab_id().is_none()
    }

    pub fn set_pane_size(&mut self, width: f32, height: f32) {
        if width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0 {
            self.extensions.pane_size = Some((width, height));
        }
    }

    /// Closes any open extension popup when the host window resizes (`popup.ts:55`).
    pub fn observe_window_size(&mut self, width: f32, height: f32) -> bool {
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            return false;
        }
        let prev = self.extensions.window_size.replace((width, height));
        if let Some((pw, ph)) = prev
            && ((pw - width).abs() >= 1.0 || (ph - height).abs() >= 1.0)
            && self.popup_tab_id().is_some()
        {
            return self.close_extension_popup();
        }
        false
    }

    /// Active anchored extension popup for layout/rendering, if one is open.
    pub fn extension_popup(&self) -> Option<OpenExtensionPopup<'_>> {
        match &self.extensions.popup {
            PopupState::Open { ext_id, tab_id, placement, size, about_tab, .. } => Some(OpenExtensionPopup {
                ext_id,
                tab_id: *tab_id,
                placement: *placement,
                size: *size,
                about_tab: *about_tab,
            }),
            _ => None,
        }
    }

    /// Disables/uninstalls a running extension with zero engine restart.
    pub fn disable_extension_live(&mut self, ext_id: &str) {
        if let PopupState::Open { ext_id: open_id, .. } | PopupState::AwaitingUrl { ext_id: open_id, .. } =
            &self.extensions.popup
            && open_id == ext_id
        {
            self.close_extension_popup();
        }
        if self.engine_extensions() {
            self.send(HostMsg::UninstallExtension { ext_id: ext_id.to_owned() });
        }
    }

    /// Attempts zero-restart runtime re-enable of an extension that was already exempted at engine
    /// startup. Returns `false` if a brand-new install or unexempted extension requires
    /// [`Self::restart_engine_for_extensions`].
    pub fn reenable_extension_live(&mut self, ext_id: &str) -> bool {
        if !self.is_connected() || !self.engine_extensions() {
            return false;
        }
        if !self.extensions.launched_ext_ids.contains(ext_id) {
            return false;
        }
        self.send(HostMsg::ReloadExtension { ext_id: ext_id.to_owned() });
        true
    }

    /// Cleanly restarts the sidecar engine with the updated `startup_extensions`, transparently
    /// restoring every open tab (`url`, `file_roots`, `device`, `isolated`, `allow_extension_pages`,
    /// `focused_tab`) without dropping any tab from the user's strip.
    pub fn restart_engine_for_extensions(&mut self) {
        self.close_extension_popup();
        self.extensions.actions.clear();
        let _ = self.extensions.filter_gate.release();
        if !self.is_connected() && self.tabs.is_empty() {
            return;
        }
        self.shutdown_sidecar();
        if !self.tabs.is_empty() {
            self.requeue_tabs();
            self.ensure_connected();
        }
    }

    /// Opens a managed pane tab answering an `EngineMsg::ExtensionOpenTab` request.
    pub fn answer_extension_open_tab(&mut self, request_id: u64, raw_url: &str, file_roots: Vec<String>) -> u64 {
        self.open_tab_full(
            raw_url,
            file_roots,
            crate::protocol::ContextKind::Default,
            false,
            false,
            true,
            Some(request_id),
        )
    }

    /// Refuses an `EngineMsg::ExtensionOpenTab` request cleanly.
    pub fn cancel_extension_open_tab(&mut self, request_id: u64) {
        self.send(HostMsg::CancelExtensionOpenTab { request_id });
    }

    pub fn has_extension_events(&self) -> bool {
        !self.extensions.open_requests.is_empty()
            || !self.extensions.focus_requests.is_empty()
            || !self.extensions.integrity_failures.is_empty()
            || !self.extensions.lifecycle_events.is_empty()
    }

    pub fn drain_extension_open_requests(&mut self) -> impl Iterator<Item = ExtensionOpenRequest> + '_ {
        self.extensions.open_requests.drain(..)
    }

    pub fn drain_extension_focus_tabs(&mut self) -> impl Iterator<Item = u64> + '_ {
        self.extensions.focus_requests.drain(..)
    }

    pub fn drain_extension_integrity_failures(&mut self) -> impl Iterator<Item = String> + '_ {
        self.extensions.integrity_failures.drain(..)
    }

    pub fn drain_extension_lifecycle_events(&mut self) -> impl Iterator<Item = ExtensionLifecycleEvent> + '_ {
        self.extensions.lifecycle_events.drain(..)
    }

    pub fn drain_extension_notification_ops(&mut self) -> impl Iterator<Item = ExtensionNotificationOp> + '_ {
        self.extensions.notification_ops.drain(..)
    }

    pub fn send_extension_notification_event(&self, ext_id: String, notif_id: String, clicked: bool, by_user: bool) {
        if self.caps.extensions() {
            self.send(HostMsg::ExtensionNotificationEvent { ext_id, notif_id, clicked, by_user });
        }
    }

    pub fn set_extension_notifications_enabled(&mut self, enabled: bool) {
        if self.extensions.notifications_enabled == Some(enabled) {
            return;
        }
        self.extensions.notifications_enabled = Some(enabled);
        if self.caps.extensions() {
            self.send(HostMsg::SetNotificationsEnabled { enabled });
        }
    }

    /// Renders the open anchored extension popup's OSR surface (`[25..800]×[25..600]`) with
    /// pointer, wheel, and keyboard forwarding (Escape dismisses the popup).
    pub fn popup_surface_el<S: 'static>(
        &self,
        key: impl Into<String>,
        scale: f32,
        view_zoom: f32,
        access: fn(&mut S) -> &mut BrowserHost,
    ) -> Option<El> {
        let PopupState::Open { tab_id, surface, size, placement, .. } = &self.extensions.popup else {
            return None;
        };
        let tab_id = *tab_id;
        let z = view_zoom.max(0.01);
        let (w_pt, h_pt) = if let Some((pw, ph)) = self.extensions.pane_size {
            let content = ((pw - 16.0).max(POPUP_MIN_W as f32), (ph - 12.0).max(POPUP_MIN_H as f32));
            let rect = popup_bounds(*placement, *size, content);
            (rect.width.max(POPUP_MIN_W) as f32, rect.height.max(POPUP_MIN_H) as f32)
        } else {
            (size.0 as f32, size.1 as f32)
        };
        let node = el()
            .key(key.into())
            .w(w_pt)
            .h(h_pt)
            .focusable(true)
            .pointer_buttons([PointerButton::Middle]);
        Some(
            frame::draw(node, surface)
                .surface_anchor(Align::Start, Align::Start)
                .surface_unscaled(true)
                .on_layout_tx(move |s: &mut S, [_, _, w, h], tx| {
                    let host = access(s);
                    let width_pt = (w * z).max(POPUP_MIN_W as f32);
                    let height_pt = (h * z).max(POPUP_MIN_H as f32);
                    let sc = scale.max(1.0);
                    host.send(HostMsg::Resize { tab_id, width_pt, height_pt, scale: sc });
                    tx.live_only();
                })
                .on_pointer_move_local(move |s: &mut S, p: PointerEvent, tx| {
                    access(s).send(HostMsg::MouseMove {
                        tab_id,
                        x: p.x * z,
                        y: p.y * z,
                        mods: pack_mods(tx.mods()),
                        leave: false,
                    });
                    tx.live_only();
                })
                .on_hover_change(move |s: &mut S, inside: bool, tx| {
                    if !inside {
                        access(s).send(HostMsg::MouseMove {
                            tab_id,
                            x: -1.0,
                            y: -1.0,
                            mods: 0,
                            leave: true,
                        });
                    }
                    tx.live_only();
                })
                .on_drag(move |s: &mut S, d: &mut Drag, tx| {
                    let mods = pack_mods(tx.mods());
                    let btn = PointerBtn::from(tx.button());
                    let host = access(s);
                    match d.phase {
                        Phase::Down => {
                            host.set_popup_focused(true);
                            host.send(HostMsg::MouseButton {
                                tab_id,
                                x: d.x * z,
                                y: d.y * z,
                                button: btn,
                                up: false,
                                clicks: d.clicks.clamp(1, 3),
                                mods,
                            });
                        }
                        Phase::Move => {
                            host.send(HostMsg::MouseMove {
                                tab_id,
                                x: d.x * z,
                                y: d.y * z,
                                mods,
                                leave: false,
                            });
                            tx.live_only();
                        }
                        Phase::Up => {
                            host.send(HostMsg::MouseButton {
                                tab_id,
                                x: d.x * z,
                                y: d.y * z,
                                button: btn,
                                up: true,
                                clicks: d.clicks.clamp(1, 3),
                                mods,
                            });
                            tx.live_only();
                        }
                    }
                })
                .on_wheel(move |s: &mut S, w: WheelEvent, tx| {
                    access(s).send(HostMsg::Wheel {
                        tab_id,
                        x: w.x * z,
                        y: w.y * z,
                        dx: w.dx,
                        dy: w.dy,
                        mods: pack_mods(w.mods),
                    });
                    tx.live_only();
                    true
                })
                .on_key(move |s: &mut S, ev: &KeyEvent, tx| {
                    if ev.key == Key::Escape {
                        access(s).close_extension_popup();
                        return true;
                    }
                    match access(s).forward_key(tab_id, ev, true) {
                        crate::client::KeyForward::Ignored => false,
                        crate::client::KeyForward::Forwarded => {
                            tx.live_only();
                            true
                        }
                        crate::client::KeyForward::StateChanged => true,
                    }
                })
                .on_key_up(move |s: &mut S, ev: &KeyEvent, tx| {
                    if ev.key == Key::Escape {
                        tx.live_only();
                        return true;
                    }
                    match access(s).forward_key(tab_id, ev, false) {
                        crate::client::KeyForward::Ignored => false,
                        crate::client::KeyForward::Forwarded => {
                            tx.live_only();
                            true
                        }
                        crate::client::KeyForward::StateChanged => true,
                    }
                }),
        )
    }

    pub(crate) fn handle_extension_msg(&mut self, msg: EngineMsg) -> bool {
        match msg {
            EngineMsg::ExtensionsState { extensions, .. } => {
                self.extensions.actions = extensions;
                true
            }
            EngineMsg::ExtensionAction { action } => {
                if let Some(existing) = self.extensions.actions.iter_mut().find(|a| a.ext_id == action.ext_id) {
                    *existing = action;
                } else {
                    self.extensions.actions.push(action);
                }
                true
            }
            EngineMsg::ExtensionPopupRequested { ext_id, url } => {
                let (placement, about_tab) = match std::mem::replace(&mut self.extensions.popup, PopupState::Closed) {
                    PopupState::AwaitingUrl { ext_id: wanted, placement, about_tab } if wanted == ext_id => {
                        (placement, about_tab)
                    }
                    PopupState::Open { tab_id, surface, .. } => {
                        surface.clear();
                        self.surfaces
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(&tab_id);
                        self.send(HostMsg::CloseTab { tab_id });
                        (PopupPlacement::default(), self.focused_tab_id())
                    }
                    _ => (PopupPlacement::default(), self.focused_tab_id()),
                };
                // Only allow opening a page of `ext_id`'s own `chrome-extension://<ext_id>/` origin.
                let prefix = format!("chrome-extension://{ext_id}/");
                if !url.starts_with(&prefix) {
                    return true;
                }
                let tab_id = self.alloc_tab_id();
                let surface = TabSurface::new();
                if let Some(w) = &self.waker {
                    frame::wake_window(&surface, w);
                }
                self.surfaces
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(tab_id, surface.clone());
                let scale = self.last_viewport.1.max(1.0);
                let init_size = (320u32, 420u32);
                self.extensions.popup = PopupState::Open {
                    ext_id,
                    tab_id,
                    url: url.clone(),
                    placement,
                    size: init_size,
                    about_tab,
                    surface,
                };
                self.extensions.popup_needs_focus = true;
                self.extensions.popup_focused = true;
                self.extensions.popup_refocus_page = false;
                self.send(HostMsg::OpenTab {
                    tab_id,
                    url,
                    width_pt: init_size.0 as f32,
                    height_pt: init_size.1 as f32,
                    scale,
                    focused: true,
                    file_roots: Vec::new(),
                    context: crate::protocol::ContextKind::Default,
                    preview: None,
                    agent_driven: false,
                    allow_extension_pages: true,
                    is_popup: true,
                    ext_open_request_id: None,
                });
                true
            }
            EngineMsg::ExtensionPopupResized { ext_id, width, height } => {
                let w = width.clamp(POPUP_MIN_W, POPUP_MAX_W);
                let h = height.clamp(POPUP_MIN_H, POPUP_MAX_H);
                let mut resized_tab = None;
                if let PopupState::Open { ext_id: open_id, tab_id, size, .. } = &mut self.extensions.popup
                    && open_id == &ext_id
                    && *size != (w, h)
                {
                    *size = (w, h);
                    resized_tab = Some(*tab_id);
                }
                if let Some(tab_id) = resized_tab {
                    let scale = self.last_viewport.1.max(1.0);
                    self.send(HostMsg::Resize { tab_id, width_pt: w as f32, height_pt: h as f32, scale });
                    return true;
                }
                false
            }
            EngineMsg::ExtensionOpenTab { request_id, ext_id, url, active } => {
                // Bound pending extension tab-open requests (`review-rd274.md` binding condition).
                if self.extensions.open_requests.len() >= 16 {
                    self.send(HostMsg::CancelExtensionOpenTab { request_id });
                    return false;
                }
                self.extensions.open_requests.push_back(ExtensionOpenRequest { request_id, ext_id, url, active });
                true
            }
            EngineMsg::ExtensionFocusTab { tab_id } => {
                self.extensions.focus_requests.push_back(tab_id);
                true
            }
            EngineMsg::ExtensionFiltersReady => {
                for (tab_id, url) in self.extensions.filter_gate.release() {
                    self.send(HostMsg::Navigate { tab_id, url });
                }
                true
            }
            EngineMsg::ExtensionUninstalled { ext_id, ok, error } => {
                if ok {
                    self.extensions.actions.retain(|a| a.ext_id != ext_id);
                }
                self.extensions.lifecycle_events.push_back(ExtensionLifecycleEvent::Uninstalled { ext_id, ok, error });
                true
            }
            EngineMsg::ExtensionReloaded { ext_id, ok, error } => {
                self.extensions.lifecycle_events.push_back(ExtensionLifecycleEvent::Reloaded { ext_id, ok, error });
                true
            }
            EngineMsg::ExtensionIntegrityFailed { ext_id } => {
                self.extensions.integrity_failures.push_back(ext_id);
                true
            }
            EngineMsg::ExtensionNotificationShow { ext_id, ext_name, notif_id, body } => {
                if self.extensions.notification_ops.len() >= 64 {
                    self.extensions.notification_ops.pop_front();
                }
                self.extensions.notification_ops.push_back(ExtensionNotificationOp::Show { ext_id, ext_name, notif_id, body });
                true
            }
            EngineMsg::ExtensionNotificationClear { ext_id, notif_id } => {
                if self.extensions.notification_ops.len() >= 64 {
                    self.extensions.notification_ops.pop_front();
                }
                self.extensions.notification_ops.push_back(ExtensionNotificationOp::Clear { ext_id, notif_id });
                true
            }
            _ => false,
        }
    }

    pub(crate) fn on_extensions_ready(&mut self, extensions: bool) {
        if extensions
            && let Some(enabled) = self.extensions.notifications_enabled
        {
            self.send(HostMsg::SetNotificationsEnabled { enabled });
        }
        if !extensions || !self.extensions.startup_has_filters {
            for (tab_id, url) in self.extensions.filter_gate.release() {
                self.send(HostMsg::Navigate { tab_id, url });
            }
            return;
        }
        self.extensions.filter_gate.start_deadline(rune::time::Instant::now());
        self.send(HostMsg::ListExtensions { request_id: 0 });
    }

    pub(crate) fn on_tab_closed_extensions(&mut self, id: u64) {
        self.extensions.filter_gate.cancel_tab(id);
        if matches!(self.extensions.popup, PopupState::Open { about_tab: Some(tid), .. } | PopupState::AwaitingUrl { about_tab: Some(tid), .. } if tid == id) {
            self.close_extension_popup();
        }
    }
}
