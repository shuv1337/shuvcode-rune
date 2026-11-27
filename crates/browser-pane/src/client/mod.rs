//! Sidecar connection and multi-tab state, including the Rune frame handle for each tab.

mod poll;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, PoisonError};

use rune::{Ime, Key, KeyEvent, Waker};
use serde_json::Value;

use crate::address::resolve_address;
use crate::frame::{self, TabSurface};
use crate::installer::EngineStatus;
use crate::keys::{is_shell_shortcut, named_key_str, page_zoom_dir};
#[cfg(all(feature = "engine", target_os = "macos"))]
use crate::mach_pipe::MachReceiver;
#[cfg(feature = "engine")]
use crate::machine::UpdateCheck;
use crate::machine::{EngineCaps, EngineEffect, EngineEvent, SharedEngine};
use crate::site_zoom::{SiteZoom, site_of};
use crate::protocol::{
    CursorKind, DownloadFile, EngineMsg, HostMsg, PickedElement, TabSnapshot, pack_mods,
};

#[derive(Clone, Debug)]
pub struct BrowserTab {
    pub device: Option<crate::device::Device>,
    pub id: u64,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub audible: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub zoom: f64,
    pub cursor: CursorKind,
    /// Wheel-scroll cursor deferral state machine (avoids full-window UI rebuilds on scroll).
    pub(crate) scroll_cursor: crate::scroll_cursor::ScrollCursor,
    /// Why the page shows no document (`TabSnapshot::load_error`).
    pub load_error: Option<String>,
    /// Site favicon as a bounded `data:image/png;base64,...` data URL (`UP-109` `#65094`).
    pub favicon: Option<String>,
    /// Set on isolated `oc-preview://` tabs so chrome renders them as previews.
    pub preview: Option<crate::protocol::PreviewKindTag>,
    /// Live preview grant specification (reused across reconnects & remote/artifact proxy validation).
    pub preview_spec: Option<crate::protocol::PreviewSpec>,
    /// Why the last navigation on this page was refused while keeping the document (`policy::Refusal`).
    pub refusal: Option<String>,
    /// Count of refused navigations on this page, so repeated refusals end the address field's hold.
    pub rejected: u64,
    pub inspecting: bool,
    /// What the page downloaded, oldest first (`DownloadFile`), for the agent's tools.
    pub files: Vec<DownloadFile>,
    /// Page dialogs, permissions, HTTP auth, and blocked pop-up state (`prompts`).
    pub prompts: crate::prompts::TabPromptState,
    /// Per-tab in-page search state (`find.rs`).
    pub find: crate::find::FindState,
    pub surface: TabSurface,
    /// Surface layout origin in zoomed window points (for anchoring page context menus).
    pub surface_origin: (f32, f32),
    pub view_zoom: f32,
    /// Storage/cookie/extension isolation context (`Default`, `Preview`, `Artifact`, `Private`, or `RemoteServer`).
    pub context: crate::protocol::ContextKind,
    /// User-opened tab allowed to host loaded `chrome-extension://` pages (never agent/isolated).
    pub allow_extension_pages: bool,
    pub(crate) file_roots: Vec<String>,
    pub(crate) viewport_pt: (f32, f32),
    pub(crate) scale: f32,
    pub(crate) sent_resize: Option<(f32, f32, f32)>,
    /// Whether this tab has an open (or queued) backing tab in the engine (`false` for fail-closed
    /// unproxied/unsupported/refused-open tabs so subsequent `navigate` promotes cleanly).
    pub(crate) has_engine_tab: bool,
    /// True once the engine has emitted at least one `TabUpdate`/`TabList` snapshot for this tab.
    pub(crate) engine_confirmed: bool,
}

impl BrowserTab {
    /// The pixel size the engine renders this tab's frames at: its viewport in whole points (the
    /// engine's view rect) times its scale.
    pub fn frame_px(&self) -> (u32, u32) {
        let px = |pt: f32| (pt.round().max(1.0) * self.scale).ceil() as u32;
        (px(self.viewport_pt.0), px(self.viewport_pt.1))
    }

    /// Active `(emulation_scale, is_mobile)` for pointer resolution (`chromium.ts:2486-2499`).
    pub fn emulation_info(&self) -> (f64, bool) {
        self.device.as_ref().map_or((1.0, false), |d| {
            (f64::from(d.fit([self.viewport_pt.0, self.viewport_pt.1])).clamp(0.25, 4.0), d.profile != crate::device::Profile::Desktop)
        })
    }
}

/// A tab's downloads at most (the engine's `downloads::MAX_TAB_FILES`).
pub const MAX_TAB_FILES: usize = 64;

/// One DevTools Protocol event forwarded from the browser engine (`EngineMsg::CdpEvent`).
#[derive(Clone, Debug)]
pub struct CdpEvent {
    pub tab_id: u64,
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

/// Bounded queue capacity for unconsumed CDP events so diagnostics never grow unboundedly.
const MAX_QUEUED_CDP_EVENTS: usize = 2_048;

/// Result of forwarding a keyboard event to a browser tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyForward {
    /// Not handled by the browser surface (e.g. a shell shortcut or blocked by a modal).
    Ignored,
    /// Sent directly to the browser engine; no Rune view state changed (`tx.live_only()`).
    Forwarded,
    /// Mutated browser-pane UI state (zoom, inspect, reload); requires a Rune view commit.
    StateChanged,
}

pub struct BrowserHost {
    pub(crate) tabs: Vec<BrowserTab>,
    pub(crate) focused_tab: Option<u64>,
    pub(crate) visible_tabs: Vec<u64>,
    pub(crate) unthrottled_tabs: HashSet<u64>,
    pub(crate) surfaces: Arc<Mutex<HashMap<u64, TabSurface>>>,
    #[cfg(all(feature = "engine", target_os = "linux"))]
    pub(crate) dmabuf_pools: Arc<Mutex<Option<Arc<Mutex<crate::dmabuf::Pools>>>>>,
    /// Shared sidecar connection state (`generation`, `status`, `outbox`, child PID, and early opens).
    pub(crate) shared: Arc<Mutex<SharedEngine>>,
    /// Status last observed by `poll()`, for change detection in `quiet()` and `poll()`.
    pub(crate) last_polled_status: EngineStatus,
    #[cfg_attr(not(feature = "engine"), allow(dead_code))]
    pub(crate) inbox_tx: Sender<EngineMsg>,
    inbox_rx: Receiver<EngineMsg>,
    /// Messages quiet read off the inbox, which poll handles first.
    held: std::cell::RefCell<std::collections::VecDeque<EngineMsg>>,
    pub(crate) waker: Option<Waker>,
    /// What the engine's messages wake, when the app gave one (set_message_waker).
    pub(crate) message_waker: Option<Waker>,
    shell_shortcut: Option<fn(&KeyEvent) -> bool>,
    pub(crate) socket_override: Option<PathBuf>,
    #[cfg_attr(not(feature = "engine"), allow(dead_code))]
    pub(crate) profile_dir: Option<PathBuf>,
    /// Capabilities advertised by the running (or last run) engine's `Ready`.
    pub(crate) caps: EngineCaps,
    next_tab_id: u64,
    pub(crate) next_req_id: u64,
    pub(crate) pending_opens: Vec<(u64, String)>,
    pub(crate) preview_fetches: Vec<crate::preview::PreviewFetchRequest>,
    /// Shared outbox back-pressure budget for streaming `HostMsg::PreviewResponseChunk` frames.
    pub(crate) preview_chunk_budget: Arc<crate::preview::PreviewChunkBudget>,
    /// Local preview tabs that waited for `Ready` on an older engine (`!Ready.preview`): the host
    /// closes the pending browser tab and opens the file in the Files source viewer ("show source").
    pub(crate) unsupported_local_previews: Vec<(u64, String)>,
    /// Expected `TabClosed` acks from `reopen_tab_as_{web,preview}` so reopening a tab in a
    /// different isolation context does not delete its `BrowserTab` entry or `TabSurface`.
    pub(crate) reopening_tabs: HashMap<u64, u32>,
    pub(crate) cdp_responses: HashMap<u64, Result<Value, Value>>,
    pub(crate) cdp_response_order: VecDeque<u64>,
    pub(crate) cancelled_cdp: VecDeque<u64>,
    pub(crate) cdp_events: VecDeque<CdpEvent>,
    picked: VecDeque<PickedElement>,
    pub(crate) popup_opens: VecDeque<crate::prompts::PopupRequest>,
    /// The person's Copy image results (PNG, base64), drained by the app for its clipboard.
    pub(crate) copied_images: VecDeque<String>,
    /// Tabs whose first navigation became the person's download, now placed: drained by the app.
    pub(crate) download_only_tabs: VecDeque<u64>,
    pub(crate) app_links: VecDeque<(u64, String)>,
    pub(crate) context_menus: VecDeque<(u64, crate::protocol::PageContextMenu)>,
    pub(crate) download_save_requests: VecDeque<crate::protocol::DownloadSaveRequest>,
    pub(crate) diagnostics: VecDeque<crate::find::PageDiagnostic>,
    pub(crate) profile_events: VecDeque<crate::cookies::ProfileEvent>,
    pub(crate) share_sign_ins: bool,
    /// Tab IDs closed by the engine (`EngineMsg::TabClosed`) paired with `fatal_close` (`true` only
    /// on unrecoverable proxy/context failure, never explicit `DropContext` of a suspended tab).
    pub(crate) closed_tabs: VecDeque<(u64, bool)>,
    /// Live loopback tunnel proxy configs registered per remote server identity (`ServerKey::as_str()`).
    pub(crate) remote_proxies: HashMap<String, crate::protocol::RemoteProxyConfig>,
    pub(crate) site_permissions: crate::prompts::SitePermissions,
    pub(crate) site_zoom: SiteZoom,
    pub(crate) keep_engine: bool,
    /// The scheme pages draw in: 0 not given yet, 1 light, 2 dark.
    color_scheme: std::sync::atomic::AtomicU8,
    /// Target OSR frame rate matching the window's display refresh rate in Hz (0 = not given yet).
    display_fps: std::sync::atomic::AtomicU32,
    /// The last laid-out page size, so a new tab opens at it instead of 800 × 600 and a resize.
    pub(crate) last_viewport: ((f32, f32), f32),
    #[cfg(all(feature = "engine", target_os = "macos"))]
    pub(crate) mach: Option<MachReceiver>,
    /// The engine download job, background or asked for: the process's one, which every
    /// window's host shares.
    #[cfg(feature = "engine")]
    pub(crate) prefetch: Arc<crate::prefetch::Prefetch>,
    /// Whether this run looked for a newer engine yet (`check_for_update_once`).
    #[cfg(feature = "engine")]
    pub(crate) update_check: UpdateCheck,
    /// Tabs opened so far, and `prepare` calls: an engine started for a page that didn't come
    /// stops once its hold ends with neither changed.
    pub(crate) opened: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(feature = "engine")]
    pub(crate) prepared: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) extensions: crate::extensions::BrowserExtensionsState,
    /// Per-tab `(last_engine_favicon, url -> favicon)` cache (bounded to 32 URLs/tab) so same-origin
    /// bfcache Back/Forward restores each page's icon when CEF omits `on_favicon_urlchange`.
    pub(crate) tab_favicons: HashMap<u64, (Option<String>, HashMap<String, String>)>,
}

impl Default for BrowserHost {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserHost {
    pub fn new() -> Self {
        let (inbox_tx, inbox_rx) = mpsc::channel();
        let initial_status = Self::initial_status();
        Self {
            tabs: Vec::new(),
            focused_tab: None,
            visible_tabs: Vec::new(),
            unthrottled_tabs: HashSet::new(),
            surfaces: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(all(feature = "engine", target_os = "linux"))]
            dmabuf_pools: Arc::new(Mutex::new(None)),
            last_polled_status: initial_status.clone(),
            shared: Arc::new(Mutex::new(SharedEngine::new(initial_status))),
            inbox_tx,
            inbox_rx,
            held: Default::default(),
            waker: None,
            message_waker: None,
            shell_shortcut: None,
            socket_override: None,
            profile_dir: None,
            caps: EngineCaps::Unknown,
            next_tab_id: 1,
            next_req_id: 1,
            pending_opens: Vec::new(),
            preview_fetches: Vec::new(),
            preview_chunk_budget: Arc::default(),
            unsupported_local_previews: Vec::new(),
            reopening_tabs: HashMap::new(),
            cdp_responses: HashMap::new(),
            cdp_response_order: VecDeque::new(),
            cancelled_cdp: VecDeque::new(),
            cdp_events: VecDeque::new(),
            picked: VecDeque::new(),
            popup_opens: VecDeque::new(),
            copied_images: VecDeque::new(),
            download_only_tabs: VecDeque::new(),
            app_links: VecDeque::new(),
            context_menus: VecDeque::new(),
            download_save_requests: VecDeque::new(),
            diagnostics: VecDeque::new(),
            profile_events: VecDeque::new(),
            share_sign_ins: false,
            closed_tabs: VecDeque::new(),
            remote_proxies: HashMap::new(),
            site_permissions: crate::prompts::SitePermissions::default(),
            site_zoom: SiteZoom::default(),
            keep_engine: false,
            color_scheme: std::sync::atomic::AtomicU8::new(0),
            display_fps: std::sync::atomic::AtomicU32::new(0),
            last_viewport: ((800.0, 600.0), 2.0),
            #[cfg(all(feature = "engine", target_os = "macos"))]
            mach: None,
            #[cfg(feature = "engine")]
            prefetch: crate::prefetch::Prefetch::global(),
            #[cfg(feature = "engine")]
            update_check: UpdateCheck::Pending,
            opened: Arc::default(),
            #[cfg(feature = "engine")]
            prepared: Arc::default(),
            extensions: Default::default(),
            tab_favicons: HashMap::new(),
        }
    }

    /// Remembers each site's page zoom in `store` (the app's, as JSON text) and opens its pages at it again.
    pub fn remember_site_zoom(&mut self, store: crate::ZoomStore) {
        self.site_zoom = SiteZoom::remember(store);
    }

    /// Pages draw in the app's scheme (light or dark); the engine hears of it once it's up and at
    /// each change. Cheap to call every frame.
    pub fn set_color_scheme(&self, dark: bool) {
        let want = if dark { 2 } else { 1 };
        if self.color_scheme.swap(want, std::sync::atomic::Ordering::Relaxed) != want {
            self.send(HostMsg::SetColorScheme { dark });
        }
    }

    /// Matches off-screen browser rendering to the window's display refresh rate (60..=240 Hz).
    pub fn set_display_hz(&self, hz: f64) {
        if !hz.is_finite() || hz < 30.0 {
            return;
        }
        let fps = (hz.round() as u32).clamp(60, 240);
        if self.display_fps.swap(fps, std::sync::atomic::Ordering::Relaxed) != fps {
            self.send(HostMsg::SetFrameRate { fps });
        }
    }

    /// Notes active wheel scrolling on `tab_id` so cursor-only `TabUpdate`s defer until settle.
    pub(crate) fn note_wheel_scroll(&mut self, tab_id: u64) {
        let now = rune::time::Instant::now();
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            t.scroll_cursor.step(crate::scroll_cursor::ScrollCursorEvent::Wheel { now });
        }
    }

    /// Earliest deadline at which a deferred scroll-time cursor must settle (used by `browser_waker`
    /// to arm `tx.wake_after` when `poll()` returns `false`).
    pub fn scroll_cursor_wake_at(&self) -> Option<rune::time::Instant> {
        self.tabs.iter().filter_map(|t| t.scroll_cursor.wake_at()).min()
    }

    /// Leaves the engine running when the last tab closes, so the next page skips its start and
    /// reuses its warm connections; the engine still quits on its own after a minute with no tabs.
    pub fn set_keep_engine(&mut self, keep: bool) {
        self.keep_engine = keep;
    }

    /// Keeps the engine's cookies and sign-ins in `dir` instead of this app's own profile
    /// (`OPENCODE_BROWSER_PROFILE` in the environment still wins). Takes effect at the next spawn.
    /// The engine that runs (or last ran) keeps a page's downloads, for the agent's tools.
    pub fn engine_downloads(&self) -> bool {
        self.caps.downloads()
    }

    pub fn engine_policy(&self) -> bool {
        self.caps.policy()
    }

    /// Whether the connected engine has completed `EngineMsg::Ready` capability handshake.
    /// Returns `false` while disconnected OR still in `Connecting` before `Ready` arrives.
    pub fn engine_caps_known(&self) -> bool {
        !matches!(self.caps, EngineCaps::Unknown)
    }

    /// Whether the connected engine serves isolated `oc-preview://` tabs.
    pub fn engine_preview(&self) -> bool {
        self.caps.preview()
    }

    pub fn set_profile_dir(&mut self, dir: PathBuf) {
        self.profile_dir = Some(dir);
    }

    /// Marks a mock tab loading or done (stories and tests).
    pub fn set_mock_loading(&mut self, id: u64, loading: bool) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.loading = loading;
        }
    }

    pub fn set_shell_shortcut(&mut self, check: fn(&KeyEvent) -> bool) {
        self.shell_shortcut = Some(check);
    }

    /// Whether this build has any way to run an engine: a download source, a trust rule for an
    /// installed version, or an explicit engine/socket override in the environment.
    pub fn engine_configured() -> bool {
        #[cfg(feature = "engine")]
        {
            let set = |k| crate::installer::dev_override(k).is_some();
            crate::source::configured()
                || crate::source::has_trust()
                || set("OPENCODE_BROWSER_ENGINE_BIN")
                || set("OPENCODE_BROWSER_ENGINE_APP")
                || set("OPENCODE_BROWSER_SOCKET")
        }
        #[cfg(not(feature = "engine"))]
        {
            false
        }
    }

    /// Without the engine there is nothing to fetch (`EngineStatus::Unsupported` already).
    #[cfg(not(feature = "engine"))]
    pub fn fetch_in_background(&self) {}

    #[cfg(not(feature = "engine"))]
    pub fn network_changed(&self) {}

    pub fn with_socket(socket: PathBuf) -> Self {
        let mut h = Self::new();
        h.socket_override = Some(socket);
        #[cfg(feature = "engine")]
        {
            h.last_polled_status = EngineStatus::Unchecked;
            h.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::MockStatus(EngineStatus::Unchecked));
        }
        h
    }

    pub fn has_socket_override(&self) -> bool {
        self.socket_override.is_some()
    }

    pub fn has_live_sidecar(&self) -> bool {
        self.is_connected()
    }

    pub fn set_waker(&mut self, waker: Waker) {
        for tab in &self.tabs {
            frame::wake_window(&tab.surface, &waker);
        }
        if let crate::extensions::PopupState::Open { surface, .. } = &self.extensions.popup {
            frame::wake_window(surface, &waker);
        }
        self.waker = Some(waker);
    }

    /// What an engine started from now on wakes when a message arrives, instead of the window's
    /// waker: the app reads it and steps what waits on it at once, with no frame unless the pane
    /// changed. Pages' frames still wake the window (set_waker).
    pub fn set_message_waker(&mut self, waker: Waker) {
        self.message_waker = Some(waker);
    }

    /// The engine messages' waker (set_message_waker), else the window's.
    pub fn message_waker(&self) -> Option<Waker> {
        self.message_waker.clone().or_else(|| self.waker.clone())
    }

    pub fn status(&self) -> EngineStatus {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner).status()
    }

    pub fn tabs(&self) -> &[BrowserTab] {
        &self.tabs
    }

    pub fn focused_tab(&self) -> Option<&BrowserTab> {
        let id = self.focused_tab?;
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn focused_tab_id(&self) -> Option<u64> {
        self.focused_tab
    }

    pub fn tab(&self, id: u64) -> Option<&BrowserTab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub fn open_tab(&mut self, raw_url: &str) -> u64 {
        self.open_tab_with_context(raw_url, Vec::new(), crate::protocol::ContextKind::Default, true, false)
    }

    pub fn open_tab_with_roots(&mut self, raw_url: &str, file_roots: Vec<String>, from_address: bool) -> u64 {
        self.open_tab_with_context(raw_url, file_roots, crate::protocol::ContextKind::Default, from_address, false)
    }

    pub fn open_tab_with_context(
        &mut self,
        raw_url: &str,
        file_roots: Vec<String>,
        context: crate::protocol::ContextKind,
        from_address: bool,
        agent_driven: bool,
    ) -> u64 {
        self.open_tab_full(raw_url, file_roots, context, from_address, agent_driven, false, None)
    }

    pub fn set_tab_policy(&mut self, tab_id: u64, _file_roots: Vec<String>) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id)
            && !t.file_roots.is_empty()
        {
            t.file_roots.clear();
            if self.is_connected() && self.engine_policy() {
                self.send(HostMsg::SetTabPolicy { tab_id, file_roots: Vec::new() });
            }
        }
    }

    pub fn focus_tab(&mut self, id: u64) {
        if self.tabs.iter().any(|t| t.id == id) {
            if self.focused_tab != Some(id) {
                self.close_extension_popup();
                self.rearm_tab_prompts(id);
            }
            self.focused_tab = Some(id);
            self.send(HostMsg::FocusTab { tab_id: id });
        }
    }

    pub fn close_tab(&mut self, id: u64) {
        self.preview_chunk_budget.cancel_tab(id);
        self.tab_favicons.remove(&id);
        self.visible_tabs.retain(|&x| x != id);
        self.unthrottled_tabs.remove(&id);
        self.close_dmabuf_pool(id);
        if let Some(pos) = self.tabs.iter().position(|t| t.id == id) {
            let removed = self.tabs.remove(pos);
            removed.surface.clear();
            self.surfaces.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
            self.pending_opens.retain(|(tid, _)| *tid != id);
            self.on_tab_closed_extensions(id);
            self.cancel_download_save_requests(id);
            self.send(HostMsg::CloseTab { tab_id: id });
            if self.focused_tab == Some(id) {
                self.focused_tab = if self.tabs.is_empty() {
                    None
                } else {
                    Some(self.tabs[pos.min(self.tabs.len() - 1)].id)
                };
                if let Some(next_id) = self.focused_tab {
                    self.send(HostMsg::FocusTab { tab_id: next_id });
                }
            }
            self.sync_has_tabs();
            if self.tabs.is_empty() && !self.keep_engine {
                self.shutdown_sidecar();
            }
        }
    }

    pub(crate) fn has_engine_tabs(&self) -> bool {
        self.tabs.iter().any(|t| t.has_engine_tab)
    }

    pub(crate) fn sync_has_tabs(&self) {
        #[cfg(feature = "engine")]
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .step(EngineEvent::SetHasTabs(self.has_engine_tabs()));
    }

    pub(crate) fn clear_stale_fault_if_empty(&self) {
        #[cfg(feature = "engine")]
        if !self.has_engine_tabs() {
            self.clear_stale_fault();
        }
    }

    pub(crate) fn clear_stale_fault(&self) {
        #[cfg(feature = "engine")]
        {
            let effect = self
                .shared
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .step(EngineEvent::ClearStaleFault);
            if let EngineEffect::EndHung { kill_pid, sock, outbox } = effect {
                if let Some(pid) = kill_pid {
                    crate::transport::kill_pid(pid);
                }
                if let Some(tx) = outbox {
                    tx.send(HostMsg::Shutdown).ok();
                }
                if let Some(sock) = sock.or_else(|| self.engine_socket().ok()) {
                    std::fs::remove_file(sock).ok();
                }
            }
        }
    }

    pub(crate) fn note_tab_confirmed(&self) {
        #[cfg(feature = "engine")]
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .step(EngineEvent::TabConfirmed);
    }

    pub fn navigate(&mut self, tab_id: u64, raw_url: &str) {
        self.navigate_with_policy(tab_id, raw_url, true);
    }

    pub fn navigate_with_policy(&mut self, tab_id: u64, raw_url: &str, from_address: bool) {
        rune::marks::mark("browser.page.navigate");
        #[cfg(feature = "engine")]
        crate::transport::paint_event(crate::transport::PaintEvent::Navigate);
        let url = if from_address { resolve_address(raw_url) } else { crate::policy::with_scheme(raw_url) };
        let connected = self.is_connected();
        let ready = self.status() == EngineStatus::Ready;
        let engine_partitions = self.engine_partitions();
        let engine_remote_proxy = self.engine_remote_proxy();
        let loaded_ext = self.is_loaded_extension_url(&url) && (self.engine_extensions() || !connected);
        let had_engine_tabs = self.has_engine_tabs();
        let remote_proxies = &self.remote_proxies;
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) else {
            return;
        };
        let allow_ext =
            t.allow_extension_pages && !t.context.is_isolated() && !t.prompts.agent_driven && loaded_ext;
        if !allow_ext
            && let Some(why) = crate::policy::refusal(&url, &[])
        {
            t.loading = false;
            t.refusal = Some(why.code().to_owned());
            t.rejected += 1;
            return;
        }
        // If this tab never opened in the engine (e.g. `RemoteServer` opened before proxy was live),
        // either promote it now that a valid proxy exists or keep failing closed (never infinite spin).
        let mut promote_open: Option<((f32, f32), f32, Vec<String>, crate::protocol::ContextKind, bool, bool)> = None;
        if !t.has_engine_tab {
            if let crate::protocol::ContextKind::RemoteServer { server_id, proxy } = &mut t.context {
                if proxy.is_none() {
                    *proxy = remote_proxies.get(server_id.trim()).cloned();
                }
                let valid = proxy.as_ref().is_some_and(|cfg| {
                    crate::policy::validate_proxy_endpoint(
                        &cfg.host,
                        cfg.port,
                        cfg.username.as_deref(),
                        cfg.password.as_deref(),
                    )
                    .is_ok()
                });
                if !valid {
                    t.loading = false;
                    t.refusal = Some("browser.proxy.required".to_owned());
                    t.rejected += 1;
                    return;
                }
            }
            if ready && !t.context.is_supported(engine_partitions, engine_remote_proxy) {
                t.loading = false;
                t.refusal = Some("browser.partitions.unsupported".to_owned());
                t.rejected += 1;
                return;
            }
            t.has_engine_tab = true;
            promote_open = Some((
                t.viewport_pt,
                t.scale,
                t.file_roots.clone(),
                t.context.clone(),
                t.prompts.agent_driven,
                t.allow_extension_pages,
            ));
        } else if matches!(t.context, crate::protocol::ContextKind::RemoteServer { proxy: None, .. }) {
            t.loading = false;
            t.refusal = Some("browser.proxy.required".to_owned());
            t.rejected += 1;
            return;
        }
        let tab_isolated = t.context.is_isolated() || t.preview.is_some();
        t.refusal = None;
        if from_address || !t.prompts.agent_driven {
            t.prompts.on_user_nav();
        }
        t.loading = true;
        if let Some(((width_pt, height_pt), scale, _file_roots, context, agent_driven, allow_ext_pages)) = promote_open {
            if !had_engine_tabs {
                self.clear_stale_fault();
            }
            let connected = self.is_connected();
            let ready = self.status() == EngineStatus::Ready;
            self.sync_has_tabs();
            if connected && (context.is_default() || ready) {
                self.send_open_tab_with_ext(
                    tab_id,
                    url,
                    width_pt,
                    height_pt,
                    scale,
                    self.focused_tab == Some(tab_id),
                    Vec::new(),
                    context,
                    None,
                    agent_driven,
                    allow_ext_pages,
                    None,
                );
            } else {
                self.pending_opens.push((tab_id, url));
                self.ensure_connected();
            }
            return;
        }
        // If `tab_id` is still queued in `pending_opens` (e.g. `connected && !ready` for isolated
        // tabs), update the queued URL instead of sending `Navigate` ahead of `OpenTab`.
        if let Some((_, pending_url)) = self.pending_opens.iter_mut().find(|(id, _)| *id == tab_id) {
            *pending_url = url;
        } else if connected {
            if !tab_isolated && self.extensions.filter_gate.maybe_hold(tab_id, &url) {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    t.url = url;
                }
                return;
            }
            self.extensions.filter_gate.cancel_tab(tab_id);
            self.send(HostMsg::Navigate { tab_id, url });
        } else {
            self.pending_opens.push((tab_id, url));
        }
    }

    pub fn back(&mut self, tab_id: u64) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            t.prompts.on_user_nav();
        }
        self.send(HostMsg::Back { tab_id });
    }

    pub fn forward(&mut self, tab_id: u64) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            t.prompts.on_user_nav();
        }
        self.send(HostMsg::Forward { tab_id });
    }

    pub fn reload(&mut self, tab_id: u64) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            t.prompts.on_user_nav();
        }
        self.send(HostMsg::Reload { tab_id, ignore_cache: false });
    }

    pub fn stop(&mut self, tab_id: u64) {
        if self.extensions.filter_gate.is_holding(tab_id) {
            self.extensions.filter_gate.cancel_tab(tab_id);
            if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                t.loading = false;
            }
        }
        self.send(HostMsg::Stop { tab_id });
    }

    pub fn step_zoom(&mut self, tab_id: u64, dir: i32) {
        // Chromium's 17 zoom presets (`packages/gui-plugins/src/browser/chromium.ts:118, 144-150`).
        const STEPS: [f64; 17] = [
            0.25, 0.33, 0.5, 0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0, 4.0, 5.0,
        ];
        let next = if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            let cur = t.zoom;
            let z = if dir == 0 {
                1.0
            } else if dir > 0 {
                STEPS.into_iter().find(|&s| s > cur + 0.001).unwrap_or(cur)
            } else {
                STEPS.into_iter().rev().find(|&s| s < cur - 0.001).unwrap_or(cur)
            };
            if (z - cur).abs() < 0.0005 {
                return;
            }
            t.zoom = z;
            if !t.context.is_isolated() {
                self.site_zoom.set(&t.url, t.zoom);
            }
            t.zoom
        } else {
            return;
        };
        self.send(HostMsg::SetZoom { tab_id, factor: next });
        if let Some(w) = &self.waker {
            w();
        }
    }

    pub fn toggle_inspect(&mut self, tab_id: u64) {
        let next = if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            t.inspecting = !t.inspecting;
            t.inspecting
        } else {
            return;
        };
        self.send(HostMsg::SetInspect { tab_id, enabled: next });
    }

    pub fn highlight_node(&self, tab_id: u64, backend_node_id: Option<i64>) {
        self.send(HostMsg::HighlightNode { tab_id, backend_node_id });
    }

    /// Counts cookies for tab `tab_id`'s HTTP(S) destination without exposing cookie values.
    pub fn site_cookies(&mut self, tab_id: u64) -> Result<u64, String> {
        let url = self
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .map(|t| t.url.clone())
            .ok_or_else(|| format!("No browser tab with tabID {tab_id}"))?;
        let request_id = self.next_req_id;
        self.next_req_id += 1;
        if !self.engine_policy() || crate::policy::destination_origin(&url).is_none() {
            self.cdp_responses.insert(request_id, Ok(serde_json::json!({ "cookies": 0 })));
            return Ok(request_id);
        }
        self.send(HostMsg::SiteCookies { tab_id, request_id });
        Ok(request_id)
    }

    /// Deletes cookies and origin storage for tab `tab_id`'s HTTP(S) destination and reloads.
    pub fn clear_site(&mut self, tab_id: u64) -> Result<u64, String> {
        if !self.tabs.iter().any(|t| t.id == tab_id) {
            return Err(format!("No browser tab with tabID {tab_id}"));
        }
        if !self.engine_policy() {
            return Err("Clearing site data requires an updated browser engine.".into());
        }
        let request_id = self.next_req_id;
        self.next_req_id += 1;
        self.send(HostMsg::ClearSite { tab_id, request_id });
        Ok(request_id)
    }

    pub fn take_cdp_response(&mut self, request_id: u64) -> Option<Result<Value, Value>> {
        let out = self.cdp_responses.remove(&request_id);
        if out.is_some()
            && let Some(pos) = self.cdp_response_order.iter().position(|&id| id == request_id)
        {
            self.cdp_response_order.remove(pos);
        }
        out
    }

    /// Whether the page picked an element since the last drain.
    pub fn has_picked(&self) -> bool {
        !self.picked.is_empty()
    }

    pub fn drain_picked_elements(&mut self) -> impl Iterator<Item = PickedElement> + '_ {
        self.picked.drain(..)
    }

    pub fn retry_install(&mut self) {
        self.start_install();
    }

    /// The engine went away with pages open: they open again in the next one, where they were.
    #[cfg_attr(not(feature = "engine"), allow(dead_code))]
    pub(crate) fn requeue_tabs(&mut self) {
        for t in &self.tabs {
            if t.has_engine_tab && !self.pending_opens.iter().any(|(id, _)| *id == t.id) {
                self.pending_opens.push((t.id, t.url.clone()));
            }
        }
    }

    /// Sends the tabs that waited for the engine, as soon as it's connected: the engine opens
    /// them right after it says it's ready, without a turn of the app in between.
    fn flush_pending_opens(&mut self) {
        let (ready, early) = {
            let mut shared = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
            if !shared.is_connected() {
                return;
            }
            let ready = shared.status() == EngineStatus::Ready;
            let early = shared.take_early();
            (ready, early)
        };
        // Those the connect sent already (`transport::Link::opens`): one closed meanwhile closes;
        // one given another address or laid out at another size meanwhile gets them.
        for (&tab_id, (sent_url, sent_size)) in &early {
            let pending_url = self.pending_opens.iter().find(|(id, _)| *id == tab_id).map(|(_, u)| u.clone());
            let (size, nav_url, agent_driven, isolated) = {
                let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) else {
                    self.send(HostMsg::CloseTab { tab_id });
                    continue;
                };
                let size = (t.viewport_pt.0, t.viewport_pt.1, t.scale);
                t.sent_resize = Some(size);
                let held_same = self.extensions.filter_gate.held_url(tab_id) == pending_url.as_deref();
                let nav = match pending_url {
                    Some(url) if &url != sent_url && !held_same => {
                        if crate::policy::is_file_scheme(&url) {
                            t.loading = false;
                            t.refusal = Some(crate::policy::Refusal::Workspace.code().to_owned());
                            t.rejected += 1;
                            None
                        } else {
                            Some(url)
                        }
                    }
                    _ => None,
                };
                (size, nav, t.prompts.agent_driven, t.context.is_isolated() || t.preview.is_some())
            };
            if agent_driven {
                self.send(HostMsg::SetAgentDriven { tab_id, agent_driven: true });
            }
            if size != *sent_size {
                self.send(HostMsg::Resize { tab_id, width_pt: size.0, height_pt: size.1, scale: size.2 });
            }
            if let Some(url) = nav_url {
                if !isolated && self.extensions.filter_gate.maybe_hold(tab_id, &url) {
                    continue;
                }
                self.send(HostMsg::Navigate { tab_id, url });
            }
        }
        let engine_partitions = self.engine_partitions();
        let engine_remote_proxy = self.engine_remote_proxy();
        let engine_preview = self.engine_preview();
        let mut deferred = Vec::new();
        let engine_extensions = self.engine_extensions();
        for (tab_id, url) in std::mem::take(&mut self.pending_opens).into_iter().filter(|(id, _)| !early.contains_key(id)) {
            let is_ext_url = crate::policy::extension_origin(&url);
            let needs_ready = crate::policy::is_file_scheme(&url)
                || is_ext_url
                || self.tabs.iter().any(|t| t.id == tab_id && (!t.context.is_default() || t.preview_spec.is_some()));
            if needs_ready && !ready {
                deferred.push((tab_id, url));
                continue;
            }
            let mut skip_open = false;
            let (open_url, ((width_pt, height_pt), scale), context, preview, emulate, agent_driven, allow_ext_pages) =
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    t.sent_resize = Some((t.viewport_pt.0, t.viewport_pt.1, t.scale));
                    let (open_url, preview) = if let Some(spec) = t.preview_spec.clone() {
                        if engine_preview {
                            ("about:blank".into(), Some(spec))
                        } else {
                            if let crate::protocol::PreviewSpec::Local { document, .. } = spec {
                                self.unsupported_local_previews.push((tab_id, document));
                            }
                            t.url = "about:blank".into();
                            t.loading = false;
                            t.has_engine_tab = false;
                            t.refusal = Some("browser.preview.unsupported".into());
                            t.rejected += 1;
                            skip_open = true;
                            ("about:blank".into(), None)
                        }
                    } else {
                        if let crate::protocol::ContextKind::RemoteServer { server_id, proxy } = &mut t.context {
                            if proxy.is_none() {
                                *proxy = self.remote_proxies.get(server_id.trim()).cloned();
                            }
                            let valid = proxy.as_ref().is_some_and(|cfg| {
                                crate::policy::validate_proxy_endpoint(
                                    &cfg.host,
                                    cfg.port,
                                    cfg.username.as_deref(),
                                    cfg.password.as_deref(),
                                )
                                .is_ok()
                            });
                            if !valid {
                                t.url = "about:blank".into();
                                t.loading = false;
                                t.has_engine_tab = false;
                                t.refusal = Some("browser.proxy.required".into());
                                t.rejected += 1;
                                skip_open = true;
                            }
                        }
                        if !skip_open && !t.context.is_supported(engine_partitions, engine_remote_proxy) {
                            t.url = "about:blank".into();
                            t.loading = false;
                            t.has_engine_tab = false;
                            t.refusal = Some("browser.partitions.unsupported".into());
                            t.rejected += 1;
                            skip_open = true;
                            ("about:blank".into(), None)
                        } else if !skip_open && crate::policy::is_file_scheme(&url) {
                            t.url = "about:blank".into();
                            t.loading = false;
                            t.has_engine_tab = false;
                            t.refusal = Some(crate::policy::Refusal::Workspace.code().to_owned());
                            t.rejected += 1;
                            skip_open = true;
                            ("about:blank".into(), None)
                        } else if !skip_open && is_ext_url && !engine_extensions {
                            t.url = "about:blank".into();
                            t.loading = false;
                            t.refusal = Some(crate::policy::Refusal::Web.code().to_owned());
                            t.rejected += 1;
                            ("about:blank".into(), None)
                        } else if skip_open {
                            ("about:blank".into(), None)
                        } else {
                            (url, None)
                        }
                    };
                    let emulate = t.device.as_ref().map(|d| (d.clone(), d.fit([t.viewport_pt.0, t.viewport_pt.1])));
                    if !skip_open {
                        t.has_engine_tab = true;
                    }
                    (
                        open_url,
                        (t.viewport_pt, t.scale),
                        t.context.clone(),
                        preview,
                        emulate,
                        t.prompts.agent_driven,
                        t.allow_extension_pages,
                    )
                } else {
                    // Tab was closed before flush: never synthesize a `ContextKind::Default` open!
                    continue;
                };
            if skip_open {
                continue;
            }
            self.send_open_tab_with_ext(
                tab_id,
                open_url,
                width_pt,
                height_pt,
                scale,
                self.focused_tab == Some(tab_id),
                Vec::new(),
                context,
                preview,
                agent_driven,
                allow_ext_pages,
                None,
            );
            if let Some((device, emu_scale)) = emulate {
                self.send(HostMsg::Emulate { tab_id, device: Some(device), scale: emu_scale });
            }
        }
        self.pending_opens = deferred;
        self.sync_has_tabs();
    }

    pub fn ime(&mut self, ime: &Ime) -> bool {
        let Some(tab_id) = self.popup_tab_id().filter(|_| self.popup_focused()).or(self.focused_tab) else {
            return false;
        };
        match ime {
            Ime::Commit(s) => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    t.prompts.record_document_input();
                }
                self.send(HostMsg::Ime { tab_id, commit: Some(s.clone()), preedit: None });
                true
            }
            Ime::Preedit(s, ..) => {
                self.send(HostMsg::Ime { tab_id, commit: None, preedit: Some(s.clone()) });
                true
            }
            _ => false,
        }
    }

    /// Builds a focusable Rune `El` displaying `tab_id`'s zero-copy `NativeSurface` and forwarding
    /// layout resizes, pointer clicks/drags/wheel, cursor shapes, and non-shell keys.
    pub fn shutdown_sidecar(&mut self) {
        self.close_extension_popup();
        let effect = self.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::Shutdown);
        #[cfg(feature = "engine")]
        {
            let active_sock = match &effect {
                EngineEffect::Shutdown { sock, .. } => sock.clone(),
                _ => None,
            };
            if let Some(sock) = active_sock.or_else(|| self.engine_socket().ok()) {
                std::fs::remove_file(sock).ok();
            }
        }
        if let EngineEffect::Shutdown { outbox: Some(tx), .. } = effect {
            tx.send(HostMsg::Shutdown).ok();
        }
    }

    pub(crate) fn forward_key(&mut self, tab_id: u64, ev: &KeyEvent, down: bool) -> KeyForward {
        if self.tab(tab_id).is_some_and(|t| t.prompts.has_blocking_modal()) {
            return KeyForward::Ignored;
        }
        if ev.key == Key::Escape
            && !(ev.mods.ctrl || ev.mods.cmd || ev.mods.alt || ev.mods.shift)
            && self.tab(tab_id).is_some_and(|t| t.inspecting)
        {
            if down {
                self.toggle_inspect(tab_id);
                return KeyForward::StateChanged;
            }
            return KeyForward::Forwarded;
        }
        if self.shell_shortcut.map_or_else(|| is_shell_shortcut(ev), |f| f(ev)) {
            return KeyForward::Ignored;
        }
        if let Some(dir) = page_zoom_dir(ev) {
            if down {
                self.step_zoom(tab_id, dir);
                return KeyForward::StateChanged;
            }
            return KeyForward::Forwarded;
        }
        if down && let Key::Command(c) = ev.key {
            let cmd = match c {
                'a' => Some("select_all"),
                'c' => Some("copy"),
                'v' => Some("paste"),
                'x' => Some("cut"),
                'z' if ev.mods.shift => Some("redo"),
                'z' => Some("undo"),
                'r' if ev.mods.shift => {
                    self.reload_ignore_cache(tab_id);
                    return KeyForward::StateChanged;
                }
                'r' => {
                    self.reload(tab_id);
                    return KeyForward::StateChanged;
                }
                _ => None,
            };
            if let Some(cmd) = cmd {
                self.send(HostMsg::EditCmd { tab_id, cmd: cmd.into() });
                return KeyForward::Forwarded;
            }
        }
        if down && !cfg!(target_os = "macos") && ev.mods.ctrl && !ev.mods.alt && !ev.mods.cmd && ev.key == Key::Function(5) {
            self.reload_ignore_cache(tab_id);
            return KeyForward::StateChanged;
        }
        // Every other chord and the function keys go to the page, as official leaves them there
        // (`keys.ts` `pageShortcut` forwards only the shell's own): Ctrl+P / S / U, Ctrl+Shift+I,
        // F1-F12. The engine refuses Chrome's own command for each (#767), so only the page sees
        // them. Alt+F4 still closes the window.
        let (text, named) = match &ev.key {
            Key::Text(s) => (Some(s.clone()), None),
            Key::Command(c) => (Some(c.to_string()), None),
            Key::Function(n @ 1..=12) if !ev.mods.alt => (None, Some(format!("F{n}"))),
            other => (None, named_key_str(other).map(str::to_owned)),
        };
        if text.is_none() && named.is_none() {
            return KeyForward::Ignored;
        }
        self.send(HostMsg::Key {
            tab_id,
            down,
            text,
            named,
            mods: pack_mods(ev.mods),
        });
        KeyForward::Forwarded
    }

    pub(crate) fn alloc_tab_id(&mut self) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    pub(crate) fn is_connected(&self) -> bool {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner).is_connected()
    }

    pub(crate) fn send(&self, msg: HostMsg) {
        if let Some(tx) = self.shared.lock().unwrap_or_else(PoisonError::into_inner).outbox() {
            tx.send(msg).ok();
        }
    }

    pub(crate) fn upsert_tab(&mut self, snap: TabSnapshot) -> bool {
        if !snap.loading && (snap.url.starts_with("http") || snap.url.starts_with("file:")) {
            rune::marks::mark("browser.page.loaded");
        }
        self.next_tab_id = self.next_tab_id.max(snap.tab_id + 1);
        let want_zoom = self.site_zoom.get(&snap.url);
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == snap.tab_id) {
            // While an `http(s)` navigation is held behind startup DNR filter readiness, the
            // engine paints an initial `about:blank` shell; keep the tab showing its target URL
            // and spinner so the address bar never flickers.
            if self.extensions.filter_gate.is_holding(snap.tab_id) && snap.url == "about:blank" {
                return false;
            }
            let blank_after_inherited = snap.url.eq_ignore_ascii_case("about:blank")
                && matches!(
                    t.prompts.blocked_guard_notice,
                    Some(crate::prompts::BlockedGuardReason::InheritedPermission { .. })
                );
            let url_changed = t.url != snap.url;
            let new_site = site_of(&snap.url).is_some() && site_of(&t.url) != site_of(&snap.url);
            let clear_refusal =
                !blank_after_inherited && (url_changed || snap.load_error.is_some()) && t.refusal.is_some();
            t.prompts.on_snapshot_document(snap.document);
            let preserved_notice = blank_after_inherited
                .then(|| (t.prompts.blocked_guard_notice.clone(), t.prompts.blocked_guard_input));
            let (cancel_perms, cancel_auth) = if url_changed {
                t.prompts.on_document_committed(&t.url, &snap.url, snap.document == 0)
            } else {
                (Vec::new(), Vec::new())
            };
            if let Some((Some(notice), guard)) = preserved_notice {
                t.prompts.blocked_guard_notice = Some(notice);
                t.prompts.blocked_guard_input = guard;
            }
            let (last_eng_fav, url_favs) = self.tab_favicons.entry(snap.tab_id).or_default();
            let engine_fav_changed = *last_eng_fav != snap.favicon;
            *last_eng_fav = snap.favicon.clone();
            let resolved_favicon = if engine_fav_changed {
                snap.favicon
            } else if url_changed {
                url_favs.get(&snap.url).cloned().or(snap.favicon)
            } else {
                t.favicon.clone()
            };
            // A page that shares its icon with the one before it never reports a changed favicon,
            // so each settled page's icon is kept too, not only the ones that changed.
            if (engine_fav_changed || (!url_changed && !snap.loading))
                && let Some(fav) = &resolved_favicon
                && !snap.url.is_empty()
                && snap.url != "about:blank"
            {
                if url_favs.len() >= 32 && !url_favs.contains_key(&snap.url) {
                    url_favs.clear();
                }
                url_favs.insert(snap.url.clone(), fav.clone());
            }
            let non_cursor_changed = url_changed
                || t.title != snap.title
                || t.loading != snap.loading
                || t.audible != snap.audible
                || t.can_go_back != snap.can_go_back
                || t.can_go_forward != snap.can_go_forward
                || (t.zoom - snap.zoom).abs() > 0.001
                || t.load_error != snap.load_error
                || t.favicon != resolved_favicon
                || clear_refusal
                || !cancel_perms.is_empty()
                || !cancel_auth.is_empty();
            let cursor_changed = if non_cursor_changed {
                t.scroll_cursor.step(crate::scroll_cursor::ScrollCursorEvent::CoalescedCommit);
                let diff = t.cursor != snap.cursor;
                t.cursor = snap.cursor;
                diff
            } else if t.scroll_cursor == crate::scroll_cursor::ScrollCursor::Idle {
                let diff = t.cursor != snap.cursor;
                t.cursor = snap.cursor;
                diff
            } else {
                let now = rune::time::Instant::now();
                match t.scroll_cursor.step(crate::scroll_cursor::ScrollCursorEvent::CursorChanged {
                    cursor: snap.cursor,
                    now,
                }) {
                    crate::scroll_cursor::ScrollCursorEffect::Apply(c) => {
                        let diff = t.cursor != c;
                        t.cursor = c;
                        diff
                    }
                    crate::scroll_cursor::ScrollCursorEffect::None => false,
                }
            };
            let changed = non_cursor_changed || cursor_changed;
            if clear_refusal {
                t.refusal = None;
            }
            if url_changed {
                t.find.step(crate::find::FindEvent::DocumentChanged);
            }
            t.url = snap.url;
            t.title = snap.title;
            t.loading = snap.loading;
            t.audible = snap.audible;
            t.can_go_back = snap.can_go_back;
            t.can_go_forward = snap.can_go_forward;
            t.zoom = snap.zoom;
            let keep_artifact_missing = t.load_error.as_deref() == Some("browser.artifact.missing")
                && (t.preview == Some(crate::protocol::PreviewKindTag::Artifact)
                    || snap.preview == Some(crate::protocol::PreviewKindTag::Artifact));
            t.load_error = if keep_artifact_missing {
                Some("browser.artifact.missing".into())
            } else {
                snap.load_error
            };
            t.favicon = resolved_favicon;
            if snap.preview.is_some() {
                t.preview = snap.preview;
            }
            t.prompts.agent_driven |= snap.agent_driven;
            if t.prompts.agent_driven && t.prompts.driver.is_none() {
                t.prompts.driver = Some(crate::prompts::TabDriver::Agent);
            }
            t.has_engine_tab = true;
            t.engine_confirmed = true;
            let mut changed = changed;
            if new_site && !t.context.is_isolated() {
                changed |= (t.zoom - want_zoom).abs() > 0.001;
                t.zoom = want_zoom;
                self.send(HostMsg::SetZoom { tab_id: snap.tab_id, factor: want_zoom });
            }
            for id in cancel_perms {
                self.send(HostMsg::AnswerPermission { tab_id: snap.tab_id, id, allow: false, dismiss: true, persist: false });
            }
            for id in cancel_auth {
                self.send(HostMsg::AnswerAuth { tab_id: snap.tab_id, id, username: None, password: None });
            }
            self.sync_has_tabs();
            self.note_tab_confirmed();
            return changed;
        }
        let surface = self.surfaces.lock().unwrap_or_else(PoisonError::into_inner).entry(snap.tab_id).or_default().clone();
        if let Some(w) = &self.waker {
            frame::wake_window(&surface, w);
        }
        // Restore context from engine snapshot (re-attaching cached proxy for `RemoteServer`).
        let mut restored_ctx = snap.context;
        if let crate::protocol::ContextKind::RemoteServer { server_id, proxy } = &mut restored_ctx {
            let sid = server_id.trim().to_owned();
            *server_id = sid.clone();
            if proxy.is_none() {
                *proxy = self.remote_proxies.get(&sid).cloned();
            }
        }
        let is_site = !restored_ctx.is_isolated() && site_of(&snap.url).is_some();
        if is_site && (snap.zoom - want_zoom).abs() > 0.001 {
            self.send(HostMsg::SetZoom { tab_id: snap.tab_id, factor: want_zoom });
        }
        let mut prompts = crate::prompts::TabPromptState::default();
        prompts.document = snap.document;
        prompts.agent_driven = snap.agent_driven;
        if snap.agent_driven {
            prompts.driver = Some(crate::prompts::TabDriver::Agent);
        }
        if let Some(fav) = &snap.favicon && !snap.url.is_empty() && snap.url != "about:blank" {
            let (last, map) = self.tab_favicons.entry(snap.tab_id).or_default();
            *last = Some(fav.clone());
            map.insert(snap.url.clone(), fav.clone());
        }
        self.tabs.push(BrowserTab {
            id: snap.tab_id,
            url: snap.url,
            title: snap.title,
            loading: snap.loading,
            audible: snap.audible,
            can_go_back: snap.can_go_back,
            can_go_forward: snap.can_go_forward,
            zoom: if is_site { want_zoom } else { snap.zoom },
            cursor: snap.cursor,
            scroll_cursor: Default::default(),
            load_error: snap.load_error,
            favicon: snap.favicon,
            preview: snap.preview,
            preview_spec: None,
            refusal: None,
            rejected: 0,
            inspecting: false,
            files: Vec::new(),
            prompts,
            find: Default::default(),
            surface,
            surface_origin: (0.0, 0.0),
            view_zoom: 1.0,
            context: restored_ctx,
            allow_extension_pages: false,
            file_roots: Vec::new(),
            viewport_pt: (800.0, 600.0),
            device: None,
            scale: 2.0,
            sent_resize: None,
            has_engine_tab: true,
            engine_confirmed: true,
        });
        self.sync_has_tabs();
        self.note_tab_confirmed();
        true
    }
}

#[cfg(not(feature = "engine"))]
impl BrowserHost {
    pub(crate) fn initial_status() -> EngineStatus {
        EngineStatus::Unsupported
    }

    /// No engine runs on this platform yet: tabs stay mock-only and the status stays as it is.
    pub fn start_install(&mut self) {}

    pub(crate) fn ensure_connected(&mut self) {}

    pub fn prepare(&mut self, _hold: std::time::Duration) {}
}
