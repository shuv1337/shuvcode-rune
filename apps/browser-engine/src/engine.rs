//! Multi-tab CEF OSR manager inside the sidecar process.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cef::{
    Browser, BrowserHost, CefString, ImplBrowser, ImplBrowserHost, ImplFrame,
    MouseButtonType, MouseEvent, PaintElementType, Registration, RequestContext, WindowInfo, sys,
};
use serde_json::{Value, json};

pub use crate::cef_runtime::CefRuntime;
use crate::handlers::{
    CdpPending, FrameRing, PendingClear, TabCdpObserver, TabDisplayHandler, TabExtPolicy,
    TabLoadHandler, TabRenderHandler, TabRequestHandler, ViewRectState, cef_mods, event_flag,
};
use crate::keys::send_key;
use crate::preview::{PreviewGrant, PreviewRegistry};
use crate::tab_client::SidecarTabClient;
use crate::wire::{
    CursorKind, EngineMsg, HostMsg, PointerBtn, PreviewSpec, TabSnapshot,
};

/// Opaque page background (ARGB) for pages that specify none. macOS uses Rune's dark base.
const BASE_COLOR: u32 = if cfg!(target_os = "macos") { 0xFF11_1318 } else { 0xFFFF_FFFF };

pub(crate) struct ManagedTab {
    pub(crate) browser: Browser,
    pub(crate) host: BrowserHost,
    file_roots: Arc<Mutex<Vec<String>>>,
    allowed_ext_ids: Arc<Mutex<Vec<String>>>,
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    allow_extension_pages: bool,
    pub(crate) isolated: bool,
    pub(crate) is_popup: bool,
    pub(crate) preview: Option<Arc<PreviewGrant>>,
    #[cfg(target_os = "linux")]
    preview_spec: Option<PreviewSpec>,
    view_state: Arc<Mutex<ViewRectState>>,
    pub(crate) snap: Arc<Mutex<TabSnapshot>>,
    pub(crate) cdp_id_map: Arc<Mutex<HashMap<i32, CdpPending>>>,
    pub(crate) cdp_ledger: Arc<Mutex<crate::cdp_ledger::EngineCdpLedger>>,
    pub(crate) passkeys: Arc<crate::passkeys::TabPasskeyGate>,
    pub(crate) media_gate: Arc<crate::media_gate::TabMediaGate>,
    pub(crate) notification_gate: Arc<crate::notification_gate::TabNotificationGate>,
    pub(crate) clipboard_gate: Arc<crate::clipboard_gate::TabClipboardGate>,
    pub(crate) input_guard: Arc<Mutex<crate::input_guard::InputGuard>>,
    pub(crate) emulation_scale: Arc<std::sync::atomic::AtomicU32>,
    internal_picked: Arc<Mutex<Vec<i64>>>,
    pending_clears: Arc<Mutex<Vec<PendingClear>>>,
    pub(crate) prompts: Arc<crate::prompts::TabPrompts>,
    menu: Arc<crate::menu::TabMenuState>,
    download_gen: u64,
    download_alive: Arc<AtomicBool>,
    context: crate::wire::ContextKind,
    /// Last seen `loading`, so a finished load marks its side of the cookie mirror dirty.
    last_loading: bool,
    generation: u64,
    deferred_initial: Arc<AtomicBool>,
    _cdp_reg: Option<Registration>,
    held_buttons: u32,
    pub(crate) render_mode: lifecycle::TabRenderMode,
    pub(crate) awake: Arc<AtomicBool>,
    /// Linux: whether it's currently visible in a desktop pane, to reopen it so on transport reset.
    #[cfg(target_os = "linux")]
    shown: bool,
    #[cfg(any(windows, target_os = "linux"))]
    frames: FrameRing,
}

pub struct EngineManager {
    tabs: HashMap<u64, ManagedTab>,
    visible_tabs: HashSet<u64>,
    unthrottled_tabs: HashSet<u64>,
    focused_tab: Option<u64>,
    lifecycle_mode: lifecycle::LifecycleMode,
    mach_port: Arc<AtomicU32>,
    out_tx: crate::output::Output,
    events: std::sync::mpsc::Sender<crate::Event>,
    extensions: Option<crate::extensions::ExtensionHost>,
    rejected_exts: Vec<String>,
    app_handles_ext_tabs: bool,
    next_ext_open_req: u64,
    pending_ext_opens: HashMap<u64, (String, i64, String)>,
    unmanaged_ext_tabs: usize,
    seed_probe: Option<BrowserHost>,
    next_ext_tab_id: u64,
    pub(super) next_cdp_id: Arc<AtomicI32>,
    next_eref: u64,
    /// Agent commands waiting, in arrival order, for their tab's `WebAuthn.enable` reply.
    passkey_held: Vec<HostMsg>,
    default_locked: bool, // `context::lock_chrome_services` held on `Default`
    pub(super) ext_clipboard_write_ids: Arc<Mutex<std::collections::HashSet<String>>>,
    pub(super) registered_ext_ids: Arc<Mutex<std::collections::HashSet<String>>>,
    /// The app's scheme, once it said (`HostMsg::SetColorScheme`).
    dark: Option<bool>,
    /// The user's home directory, for filtering overly broad workspace `file_roots`.
    home: Option<String>,
    /// The pages' downloads, in the profile's `opencode-page-downloads` folder.
    downloads: Arc<crate::downloads::Downloads>,
    /// Engine profile directory holding `opencode-page-downloads` and `opencode-page-uploads`.
    profile_dir: std::path::PathBuf,
    /// Shared prompt-handling toggle (`Hello.prompts`) and monotonic ID counter.
    prompt_shared: crate::prompts::PromptShared,
    /// Tracks persistent `"Always"` and per-tab `"Allow this time"` site permissions.
    permissions: Arc<crate::permissions::PermissionTracker>,
    /// Isolated `oc-preview://` grant & async remote/artifact proxy registry.
    previews: Arc<PreviewRegistry>,
    /// Shared pool of isolated `CefRequestContext`s for previews and remote servers.
    contexts: crate::context::ContextPool,
    /// `CefCookieManager` list/import/delete/probe/clear and the two-way remote cookie mirror.
    cookies: crate::cookies::CookieController,
    /// The `Default` profile's cache path, where the startup probe cookie is verified on disk.
    profile_cache: String,
    /// Windows: the D3D11 device frames are copied on; None renders on the CPU instead.
    #[cfg(windows)]
    gpu: Option<Arc<crate::d3d_frames::Gpu>>,
    /// Linux: dmabufs, read-back or CPU paints once they can't work, and where tabs report a
    /// failed frame (`crate::Event::FramesFailed`).
    #[cfg(target_os = "linux")]
    transport: crate::dmabuf_frames::Transport,
}

impl EngineManager {
    /// Windows: `gpu` is the D3D11 device frames are copied on (None renders on the CPU).
    pub fn new(
        out_tx: crate::output::Output,
        downloads: crate::downloads::DownloadRoots,
        events: std::sync::mpsc::Sender<crate::Event>,
        previews: Arc<PreviewRegistry>,
        #[cfg(windows)] gpu: Option<Arc<crate::d3d_frames::Gpu>>,
        #[cfg(target_os = "linux")] transport: crate::dmabuf_frames::Transport,
        extensions: Option<crate::extensions::ExtensionHost>,
        rejected_exts: Vec<String>,
    ) -> Self {
        if let Some(global) = cef::request_context_get_global_context() {
            crate::permissions::block_device_permission_guards(&global);
            crate::permissions::scrub_legacy_content_settings(&global);
            crate::context::disable_non_proxied_udp(&global);
        }
        // Chrome services official lacks, off on `Default` too; else `Default` tabs are refused (N9).
        let default_locked = cef::request_context_get_global_context().is_some_and(|g| crate::context::lock_chrome_services(&g));
        let ext_clipboard_write_ids = Arc::new(Mutex::new(extensions.as_ref().map(|e| e.clipboard_write_ext_ids()).unwrap_or_default()));
        let registered_ext_ids = Arc::new(Mutex::new(extensions.as_ref().map(|e| e.registered_ext_ids()).unwrap_or_default()));
        Self {
            tabs: HashMap::new(),
            visible_tabs: HashSet::new(),
            unthrottled_tabs: HashSet::new(),
            focused_tab: None,
            lifecycle_mode: lifecycle::LifecycleMode::Legacy,
            mach_port: Arc::new(AtomicU32::new(0)),
            out_tx,
            events: events.clone(),
            extensions,
            rejected_exts,
            app_handles_ext_tabs: false,
            next_ext_open_req: 1,
            pending_ext_opens: HashMap::new(),
            unmanaged_ext_tabs: 0,
            seed_probe: None,
            next_ext_tab_id: 1_000_000,
            next_cdp_id: Arc::new(AtomicI32::new(100)),
            next_eref: 1,
            passkey_held: Vec::new(),
            default_locked,
            ext_clipboard_write_ids,
            registered_ext_ids,
            dark: None,
            home: crate::profile::home_dir(),
            downloads: crate::downloads::Downloads::open(&downloads),
            profile_dir: downloads.profile.clone(),
            prompt_shared: crate::prompts::PromptShared::default(),
            permissions: Arc::new(crate::permissions::PermissionTracker::default()),
            previews,
            contexts: crate::context::ContextPool::new(events.clone()),
            cookies: crate::cookies::CookieController::new(events),
            profile_cache: downloads.profile.join("Default").to_string_lossy().into_owned(),
            #[cfg(windows)]
            gpu,
            #[cfg(target_os = "linux")]
            transport,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    pub fn on_host_disconnected(&mut self) {
        self.previews.cancel_all_proxies();
        self.downloads.cancel_all_pending_saves();
        self.on_disconnect_lifecycle();
    }

    pub fn handle(&mut self, msg: HostMsg) -> bool {
        if self.refuse_if_controlled_tab_blocked(&msg) {
            return true;
        }
        let Some(msg) = self.wait_for_passkey_refusal(msg) else {
            return true;
        };
        self.handle_msg(msg)
    }

    fn handle_msg(&mut self, msg: HostMsg) -> bool {
        match msg {
            HostMsg::Hello { mach_service, prompts, extensions, download_save, tab_lifecycle, .. } => {
                self.prompt_shared.enabled.store(prompts, Ordering::Relaxed);
                self.app_handles_ext_tabs = extensions;
                self.downloads.set_download_save_enabled(download_save);
                self.on_hello_lifecycle(tab_lifecycle);
                #[cfg(not(target_os = "macos"))]
                let _ = mach_service;
                #[cfg(target_os = "macos")]
                if let Some(name) = mach_service {
                    let port = crate::handlers::lookup_mach_port(&name);
                    self.mach_port.store(port, Ordering::Relaxed);
                }
                let _ = self.out_tx.send(EngineMsg::Ready {
                    chromium_major: sys::CHROME_VERSION_MAJOR as u32,
                    pid: std::process::id(),
                    downloads: true,
                    policy: true,
                    prompts,
                    popups: true,
                    find: true,
                    context_menu: prompts,
                    partitions: true,
                    remote_proxy: true,
                    agent_tools: true,
                    preview: true,
                    permission_sync: true,
                    preview_credit: true,
                    frame_authority: true,
                    extensions: true,
                    cookies: true,
                    agent_files: true,
                    passkeys: true,
                    trace_isolation: true,
                    menu_extras: true,
                    tab_lifecycle: true,
                });
                for (&tab_id, t) in &self.tabs {
                    t.menu.cancel_all();
                    if prompts {
                        t.prompts.replay_pending(tab_id, &self.out_tx);
                    } else {
                        t.prompts.dismiss_all();
                    }
                }
                for ext_id in &self.rejected_exts {
                    let _ = self.out_tx.send(EngineMsg::ExtensionIntegrityFailed {
                        ext_id: ext_id.clone(),
                    });
                }
                if self.extensions.as_ref().is_none_or(|e| e.filters_ready()) {
                    let _ = self.out_tx.send(EngineMsg::ExtensionFiltersReady);
                }
                if !self.tabs.is_empty() {
                    let list: Vec<TabSnapshot> = self
                        .tabs
                        .values()
                        .map(|t| t.snap.lock().unwrap().clone())
                        .collect();
                    let _ = self.out_tx.send(EngineMsg::TabList { tabs: list });
                    for t in self.tabs.values() {
                        #[cfg(windows)]
                        if let Some(ring) = &t.frames {
                            ring.lock().unwrap().forget_reads();
                        }
                        #[cfg(target_os = "linux")]
                        if let Some(ring) = &t.frames {
                            let _ = ring.lock().unwrap().step(crate::dmabuf_frames::Event::Released);
                        }
                        if !tab_lifecycle && t.render_mode.is_awake() {
                            t.host.invalidate(PaintElementType::default());
                        }
                    }
                }
            }
            HostMsg::OpenTab {
                tab_id,
                url,
                width_pt,
                height_pt,
                scale,
                focused,
                file_roots: _,
                context,
                preview,
                agent_driven,
                isolated,
                allow_extension_pages,
                is_popup,
                ext_open_request_id,
            } => {
                // Raw `file://` is banned unconditionally engine-wide; local workspace previews use
                // isolated `oc-preview://` grants instead of `file_roots`.
                self.open_tab(
                    tab_id,
                    &url,
                    width_pt,
                    height_pt,
                    scale,
                    focused,
                    Vec::new(),
                    context,
                    preview,
                    agent_driven,
                    isolated,
                    allow_extension_pages,
                    is_popup,
                    ext_open_request_id,
                );
            }
            HostMsg::ClickExtension { ext_id } => self.cmd_click_extension(&ext_id),
            HostMsg::ListExtensions { request_id } => self.cmd_list_extensions(request_id),
            HostMsg::UninstallExtension { ext_id } => self.cmd_uninstall_extension(&ext_id),
            HostMsg::ReloadExtension { ext_id } => self.cmd_reload_extension(&ext_id),
            HostMsg::CancelExtensionOpenTab { request_id } => self.cmd_cancel_extension_open_tab(request_id),
            HostMsg::ExtensionMetrics { request_id } => self.cmd_extension_metrics(request_id),
            HostMsg::SetTabPolicy { tab_id, file_roots: _ } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    t.file_roots.lock().unwrap().clear();
                }
            }
            HostMsg::CloseTab { tab_id } => self.close_managed_tab(tab_id),
            HostMsg::FocusTab { tab_id } => self.handle_focus_tab(tab_id),
            HostMsg::SetVisibleTabs { tab_ids } => self.handle_set_visible_tabs(tab_ids),
            HostMsg::SetTabUnthrottled { tab_id, active } => self.handle_set_tab_unthrottled(tab_id, active),
            HostMsg::Navigate { tab_id, url } => {
                if let Some((is_preview, is_popup, snap_url, is_agent, isolated, ext_allowed, roots)) =
                    self.tabs.get(&tab_id).map(|t| {
                        let snap_url = t.snap.lock().unwrap().url.clone();
                        let is_agent = t.prompts.agent_driven.load(Ordering::Relaxed);
                        let ext_allowed = crate::extensions::manifest::extension_id_from_url(&url)
                            .is_some_and(|id| t.allowed_ext_ids.lock().unwrap().iter().any(|a| a == id));
                        (
                            t.preview.is_some(),
                            t.is_popup,
                            snap_url,
                            is_agent,
                            t.isolated,
                            ext_allowed,
                            t.file_roots.lock().unwrap().clone(),
                        )
                    })
                {
                    if is_preview {
                        // A preview tab stays inside its isolated context; web navigations open in a normal context.
                        let _ = self.out_tx.send(EngineMsg::Refused {
                            tab_id,
                            reason: browser_policy::Refusal::Workspace.code().to_owned(),
                        });
                        return true;
                    }
                    // Popups stay strictly on their own `chrome-extension://<id>/` origin.
                    if is_popup {
                        let own = crate::extensions::manifest::extension_id_from_url(&snap_url);
                        let target = crate::extensions::manifest::extension_id_from_url(&url);
                        if own != target || target.is_none() {
                            let _ = self.out_tx.send(EngineMsg::Refused {
                                tab_id,
                                reason: browser_policy::Refusal::Blocked.code().to_owned(),
                            });
                            return true;
                        }
                    }
                    let ext_ok = !is_agent && !isolated && ext_allowed;
                    if !ext_ok && let Some(why) = browser_policy::frame_guard(&url, true, &roots) {
                        let _ = self.out_tx.send(EngineMsg::Refused {
                            tab_id,
                            reason: why.code().to_owned(),
                        });
                    } else if is_agent
                        && !isolated
                        && self.tabs.get(&tab_id).is_some_and(|t| t.preview.is_none())
                        && self.permissions.has_inherited_default_grant(&url)
                    {
                        let _ = self.out_tx.send(EngineMsg::ControlledPermissionRefused {
                            tab_id,
                            url: Some(url),
                        });
                    } else {
                        match self.contexts.gate_navigation(tab_id, &url) {
                            Ok(true) => {
                                if let Some(t) = self.tabs.get(&tab_id) {
                                    t.deferred_initial.store(false, Ordering::Release);
                                    if !t.clipboard_gate.replace_held_first_load(&url) && let Some(frame) = t.browser.main_frame() {
                                        t.prompts.set_pending_top_url(Some(url.clone()));
                                        frame.load_url(Some(&CefString::from(url.as_str())));
                                    }
                                }
                            }
                            Ok(false) => {
                                if let Some(t) = self.tabs.get(&tab_id) {
                                    t.deferred_initial.store(
                                        !url.trim().eq_ignore_ascii_case("about:blank"),
                                        Ordering::Release,
                                    );
                                }
                            }
                            Err("browser.proxy.required") => {
                                // Transient proxy suspension: hold navigation & report refusal
                                // without destroying the user's open page.
                                let _ = self.out_tx.send(EngineMsg::Refused {
                                    tab_id,
                                    reason: "browser.proxy.required".to_owned(),
                                });
                            }
                            Err(reason) => self.close_and_refuse_tab(tab_id, reason),
                        }
                    }
                }
            }
            HostMsg::Back { tab_id } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    if self.contexts.is_tab_ready(tab_id) {
                        if !self.refuse_history_nav_if_inherited(tab_id, t, -1) {
                            t.browser.go_back();
                        }
                    } else if self.contexts.is_tab_suspended(tab_id) {
                        let _ = self.out_tx.send(EngineMsg::Refused {
                            tab_id,
                            reason: "browser.proxy.required".to_owned(),
                        });
                    }
                }
            }
            HostMsg::Forward { tab_id } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    if self.contexts.is_tab_ready(tab_id) {
                        if !self.refuse_history_nav_if_inherited(tab_id, t, 1) {
                            t.browser.go_forward();
                        }
                    } else if self.contexts.is_tab_suspended(tab_id) {
                        let _ = self.out_tx.send(EngineMsg::Refused {
                            tab_id,
                            reason: "browser.proxy.required".to_owned(),
                        });
                    }
                }
            }
            HostMsg::Reload { tab_id, ignore_cache } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    if self.contexts.is_tab_ready(tab_id) {
                        if ignore_cache {
                            t.browser.reload_ignore_cache();
                        } else {
                            t.browser.reload();
                        }
                    } else if self.contexts.is_tab_suspended(tab_id) {
                        let _ = self.out_tx.send(EngineMsg::Refused {
                            tab_id,
                            reason: "browser.proxy.required".to_owned(),
                        });
                    }
                }
            }
            HostMsg::Stop { tab_id } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    t.browser.stop_load();
                    t.prompts.clear_auth_on_nav(tab_id, &self.out_tx);
                }
            }
            HostMsg::SetZoom { tab_id, factor } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    let level = factor.max(0.25).ln() / 1.2f64.ln();
                    t.host.set_zoom_level(level);
                    let mut s = t.snap.lock().unwrap();
                    s.zoom = factor;
                    let _ = self.out_tx.send(EngineMsg::TabUpdate { tab: s.clone() });
                }
            }
            // The pages' `prefers-color-scheme` follows the app's scheme, not the system's: the
            // context's Chrome colour scheme doesn't reach windowless pages, DevTools emulation does.
            HostMsg::SetColorScheme { dark } => {
                self.dark = Some(dark);
                for t in self.tabs.values() { self.send_color_scheme(&t.host, dark); }
            }
            HostMsg::SetFrameRate { fps } => self.set_frame_rate(fps),
            HostMsg::Emulate { tab_id, device, scale } => {
                if let Some(t) = self.tabs.get(&tab_id)
                    && self.contexts.is_tab_ready(tab_id)
                {
                    let applied = if device.is_some() && scale.is_finite() { scale.clamp(0.25, 4.0) } else { 1.0 };
                    t.emulation_scale.store(applied.to_bits(), Ordering::Release);
                    emulation::apply(device.as_ref(), scale, |method, params| self.send_internal_cdp(&t.host, method, params));
                }
            }
            HostMsg::Resize { tab_id, width_pt, height_pt, scale } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    let changed = {
                        let mut v = t.view_state.lock().unwrap();
                        let d = (v.width_pt - width_pt).abs() > 0.5
                            || (v.height_pt - height_pt).abs() > 0.5
                            || (v.scale - scale).abs() > 0.01;
                        if d {
                            v.width_pt = width_pt.max(1.0);
                            v.height_pt = height_pt.max(1.0);
                            v.scale = scale.max(1.0);
                        }
                        d
                    };
                    if changed {
                        t.host.notify_move_or_resize_started();
                        t.host.notify_screen_info_changed();
                        t.host.was_resized();
                        self.on_tab_resize_extensions(tab_id, width_pt, height_pt);
                    }
                }
            }
            HostMsg::MouseMove { tab_id, x, y, mods, leave } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    let ev = MouseEvent {
                        x: x.round() as i32,
                        y: y.round() as i32,
                        modifiers: cef_mods(mods) | t.held_buttons,
                    };
                    t.host.send_mouse_move_event(Some(&ev), i32::from(leave));
                }
            }
            HostMsg::MouseButton { tab_id, x, y, button, up, clicks, mods } => {
                // The person's press takes the tab back before the click reaches the page, so a
                // file picker this click opens is theirs (official `policy.ts:121-125`).
                if !up {
                    self.focused_tab = Some(tab_id);
                    self.set_user_reclaimed(tab_id, true);
                }
                if let Some(t) = self.tabs.get_mut(&tab_id) {
                    let (btn, flag): (MouseButtonType, u32) = match button {
                        PointerBtn::Secondary => (
                            sys::cef_mouse_button_type_t::MBT_RIGHT.into(),
                            event_flag(sys::cef_event_flags_t::EVENTFLAG_RIGHT_MOUSE_BUTTON),
                        ),
                        PointerBtn::Middle => (
                            sys::cef_mouse_button_type_t::MBT_MIDDLE.into(),
                            event_flag(sys::cef_event_flags_t::EVENTFLAG_MIDDLE_MOUSE_BUTTON),
                        ),
                        PointerBtn::Primary => (
                            sys::cef_mouse_button_type_t::MBT_LEFT.into(),
                            event_flag(sys::cef_event_flags_t::EVENTFLAG_LEFT_MOUSE_BUTTON),
                        ),
                    };
                    if up {
                        t.held_buttons &= !flag;
                    } else {
                        t.held_buttons |= flag;
                        t.host.set_focus(1);
                        t.prompts.popup_credit.store(true, Ordering::Release);
                        t.prompts.menu_suppressed.store(false, Ordering::Release);
                    }
                    let ev = MouseEvent {
                        x: x.round() as i32,
                        y: y.round() as i32,
                        modifiers: cef_mods(mods) | t.held_buttons,
                    };
                    t.host.send_mouse_click_event(Some(&ev), btn, i32::from(up), clicks as i32);
                }
            }
            HostMsg::Wheel { tab_id, x, y, dx, dy, mods } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    let ev = MouseEvent {
                        x: x.round() as i32,
                        y: y.round() as i32,
                        modifiers: cef_mods(mods) | t.held_buttons,
                    };
                    let (dx, dy) = crate::handlers::wheel_delta(dx, dy);
                    t.host.send_mouse_wheel_event(Some(&ev), dx.round() as i32, dy.round() as i32);
                }
            }
            HostMsg::Key { tab_id, down, text, named, mods } => {
                if down {
                    self.set_user_reclaimed(tab_id, true);
                }
                if let Some(t) = self.tabs.get(&tab_id) {
                    if down {
                        t.prompts.popup_credit.store(true, Ordering::Release);
                        t.prompts.menu_suppressed.store(false, Ordering::Release);
                    }
                    send_key(&t.host, down, text.as_deref(), named.as_deref(), cef_mods(mods));
                }
            }
            HostMsg::EditCmd { tab_id, cmd } => {
                if let Some(t) = self.tabs.get(&tab_id)
                    && let Some(frame) = t.browser.main_frame()
                {
                    match cmd.as_str() {
                        "select_all" => frame.select_all(),
                        "copy" => frame.copy(),
                        "paste" => frame.paste(),
                        "cut" => frame.cut(),
                        "undo" => frame.undo(),
                        "redo" => frame.redo(),
                        _ => {}
                    }
                }
            }
            HostMsg::Ime { tab_id, commit, preedit } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    if let Some(c) = commit {
                        t.host.ime_commit_text(Some(&CefString::from(c.as_str())), None, 0);
                    } else if let Some(p) = preedit {
                        if p.is_empty() {
                            t.host.ime_cancel_composition();
                        } else {
                            t.host.ime_set_composition(
                                Some(&CefString::from(p.as_str())),
                                None,
                                None,
                                None,
                            );
                        }
                    }
                }
            }
            HostMsg::SetInspect { tab_id, enabled } => self.handle_set_inspect(tab_id, enabled),
            HostMsg::HighlightNode { tab_id, backend_node_id } => self.handle_highlight_node(tab_id, backend_node_id),
            HostMsg::CdpCall { tab_id, request_id, method, params, session_id } => {
                self.set_user_reclaimed(tab_id, false);
                if let Some(t) = self.tabs.get(&tab_id) {
                    t.prompts.agent_command(method.starts_with("Input."));
                }
                self.handle_cdp_call(tab_id, request_id, method, params, session_id);
            }
            HostMsg::SiteCookies { tab_id, request_id } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    let not_ready = (!self.contexts.is_tab_ready(tab_id)).then(|| self.contexts.not_ready_reason(tab_id));
                    crate::cdp_ledger::handle_site_cookies(tab_id, request_id, not_ready, t, &self.next_cdp_id, &self.out_tx);
                }
            }
            HostMsg::ClearSite { tab_id, request_id } => self.handle_clear_site(tab_id, request_id),
            #[cfg(target_os = "linux")]
            HostMsg::FrameRead { tab_id, pool, frame_id } => {
                if let Some(ring) = self.tabs.get(&tab_id).and_then(|t| t.frames.as_ref()) {
                    self.run_frames(tab_id, ring.lock().unwrap().step(crate::dmabuf_frames::Event::Read { pool, frame_id }));
                }
            }
            #[cfg(target_os = "linux")]
            HostMsg::DmabufFailed { reason } => self.frames_failed(crate::dmabuf_frames::Failure::Share(format!("the app: {reason}"))),
            #[cfg(not(target_os = "linux"))]
            HostMsg::FrameRead { .. } | HostMsg::DmabufFailed { .. } => {}
            HostMsg::AnswerDialog { tab_id, id, accept, text } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    t.prompts.answer_dialog(id, accept, &text);
                }
            }
            HostMsg::AnswerPermission { tab_id, id, allow, dismiss, persist } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    let ctx_scope = crate::permissions::context_scope_id(&t.context, tab_id);
                    let ctx = t.host.request_context().or_else(|| {
                        t.context
                            .is_default()
                            .then(cef::request_context_get_global_context)
                            .flatten()
                    });
                    t.prompts.answer_permission(
                        tab_id,
                        id,
                        allow,
                        dismiss,
                        persist,
                        ctx_scope.as_deref(),
                        ctx.as_ref(),
                        &self.permissions,
                    );
                    for raw in t.prompts.take_gate_answers() {
                        t.host.send_dev_tools_message(Some(raw.as_bytes()));
                    }
                    if allow && ctx_scope.is_none() {
                        self.flush_bfcache_for_default_tabs();
                    }
                }
            }
            HostMsg::ResetPermission { origin, kind } => {
                self.permissions.reset_profile_permission(&origin, kind);
                // No unused media-gate Allow outlives a change to the site's permissions.
                self.tabs.values().for_each(|t| t.prompts.clear_media_gate_allows());
            }
            HostMsg::SyncSitePermissions { entries } => {
                self.permissions.sync_profile_permissions(&entries);
                self.tabs.values().for_each(|t| t.prompts.clear_media_gate_allows());
                if entries.iter().any(|e| e.allow) {
                    self.flush_bfcache_for_default_tabs();
                }
            }
            HostMsg::AnswerAuth { tab_id, id, username, password } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    t.prompts.answer_auth(id, username.as_deref(), password.as_deref());
                }
            }
            HostMsg::AnswerDownloadSave { tab_id, id, path } => {
                let ready = self.tabs.contains_key(&tab_id) && self.contexts.is_tab_ready(tab_id);
                // A tab whose first navigation became this download has nothing to show once the
                // download has its place or was cancelled (official `chromium.ts:1076-1083`); the app
                // closes it if a page opened it (`closesUnshown`, `window-open.ts`).
                if self.downloads.answer_save(tab_id, id, path.as_deref(), ready)
                    && let Some(t) = self.tabs.get(&tab_id)
                    && !t.is_popup
                    && t.snap.lock().unwrap().document == 0
                    && t.browser.can_go_back() == 0 && !t.prompts.agent_owns_dialogs()
                {
                    let _ = self.out_tx.send(EngineMsg::DownloadOnlyTab { tab_id });
                }
            }
            HostMsg::SetAgentDriven { tab_id, agent_driven } => {
                self.handle_set_agent_driven(tab_id, agent_driven);
            }
            HostMsg::TypedCdpCall { tab_id, request_id, method, params, session_id, frame_id } => {
                self.set_user_reclaimed(tab_id, false);
                if let Some(t) = self.tabs.get(&tab_id) {
                    t.prompts.agent_command(method.starts_with("Input."));
                    let not_ready = (!self.contexts.is_tab_ready(tab_id)).then(|| self.contexts.not_ready_reason(tab_id));
                    crate::cdp_ledger::handle_typed_cdp_call(
                        tab_id, request_id, not_ready, t, &self.prompt_shared, &self.permissions, &self.next_cdp_id,
                        &self.out_tx, self.extensions.is_some(), method, params, session_id, frame_id,
                    );
                }
            }
            HostMsg::UploadFiles { tab_id, request_id, backend_node_id, files, session_id, frame_id } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    t.prompts.agent_command(false);
                    let not_ready = (!self.contexts.is_tab_ready(tab_id)).then(|| self.contexts.not_ready_reason(tab_id));
                    crate::agent::handle_upload_files(t, &self.permissions, &self.profile_dir, tab_id, request_id, not_ready, backend_node_id, &files, session_id, frame_id, &self.next_cdp_id, &self.out_tx);
                }
            }
            HostMsg::DropFiles { tab_id, request_id, x, y, files } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    t.prompts.agent_input();
                    let not_ready = (!self.contexts.is_tab_ready(tab_id))
                        .then(|| self.contexts.not_ready_reason(tab_id))
                        .or_else(|| self.whole_tab_granted(t));
                    crate::agent::handle_drop_files(t, &self.permissions, &self.profile_dir, tab_id, request_id, not_ready, x, y, &files, &self.next_cdp_id, &self.out_tx);
                }
            }
            HostMsg::ClearSiteCache { tab_id, request_id } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    if !self.contexts.is_tab_ready(tab_id) {
                        let reason = self.contexts.not_ready_reason(tab_id);
                        let _ = self.out_tx.send(EngineMsg::CdpResponse {
                            tab_id,
                            request_id,
                            ok: false,
                            result: json!({ "message": reason }),
                        });
                        return true;
                    }
                    let url = t.snap.lock().unwrap().url.clone();
                    if let Some(origin) = browser_policy::destination_origin(&url) {
                        self.send_internal_cdp(
                            &t.host,
                            "Storage.clearDataForOrigin",
                            json!({
                                "origin": origin,
                                "storageTypes": "cache_storage,service_workers,shader_cache",
                            }),
                        );
                        self.send_internal_cdp(&t.host, "Network.clearBrowserCache", json!({}));
                        t.browser.reload_ignore_cache();
                    }
                    let _ = self.out_tx.send(EngineMsg::CdpResponse {
                        tab_id,
                        request_id,
                        ok: true,
                        result: json!({ "cleared": true }),
                    });
                }
            }
            HostMsg::Find { tab_id, query, forward, match_case, find_next, user_initiated } => {
                self.handle_find(tab_id, &query, forward, match_case, find_next, user_initiated);
            }
            HostMsg::StopFind { tab_id, clear_selection } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    t.host.stop_finding(i32::from(clear_selection));
                }
            }
            HostMsg::ContextMenuAction { tab_id, id, action } => {
                if let Some(t) = self.tabs.get(&tab_id) {
                    if !self.contexts.is_tab_ready(tab_id) {
                        t.menu.cancel_all();
                        return true;
                    }
                    let roots = t.file_roots.lock().unwrap().clone();
                    let block_downloads = t.preview.is_some()
                        || matches!(
                            t.context,
                            crate::wire::ContextKind::Preview { .. }
                                | crate::wire::ContextKind::Artifact { .. }
                        );
                    let follow_up = t.menu.execute(tab_id, t.download_gen, id, action, &t.browser, &t.host, &roots, block_downloads, &self.downloads);
                    match follow_up {
                        Some(crate::menu::MenuFollowUp::Extension { ext_id, item_id, target }) => {
                            if !t.isolated
                                && !t.is_popup
                                && t.preview.is_none()
                                && let Some(ext) = self.extensions.as_mut()
                            {
                                let effects = ext.on_context_menu_clicked(tab_id, &ext_id, &item_id, &target);
                                self.run_ext_effects(effects);
                            }
                        }
                        Some(crate::menu::MenuFollowUp::CopyImage { url, frame_id }) => {
                            // The menu's own frame when the engine knows it on the tab's session,
                            // else the main frame (an image in an out-of-process frame is not read).
                            let frame = {
                                let ledger = t.cdp_ledger.lock().unwrap();
                                frame_id.filter(|f| ledger.is_root_session_frame(f)).or_else(|| ledger.main_frame_id())
                            };
                            if let Some(frame) = frame {
                                let msg_id = self.next_cdp_id.fetch_add(1, Ordering::Relaxed);
                                t.cdp_id_map.lock().unwrap().insert(msg_id, CdpPending::CopyImageContent { frame_id: frame.clone() });
                                let raw = crate::menu_extras::resource_request(msg_id, &frame, &url);
                                t.host.send_dev_tools_message(Some(raw.as_bytes()));
                            }
                        }
                        None => {}
                    }
                }
            }
            HostMsg::SetPartitionProxy { server_id, proxy } => {
                let sid = server_id.trim();
                match self.contexts.set_partition_proxy(sid, proxy) {
                    Ok(crate::context::ProxyUpdate::Applied { config, activate_ready }) => {
                        for t in self.tabs.values_mut() {
                            if let crate::wire::ContextKind::RemoteServer { server_id: id, proxy: cur } = &mut t.context
                                && id.trim() == sid
                            {
                                *cur = Some(config.clone());
                                *t.prompts.proxy.lock().unwrap() = Some(config.clone());
                            }
                        }
                        if let Some(ready) = activate_ready {
                            ready.store(true, Ordering::Release);
                        }
                    }
                    Ok(crate::context::ProxyUpdate::Suspended(tabs)) => {
                        // Transient proxy loss: keep live tabs & unsaved state intact while blocking
                        // every request/CDP/menu/navigation fail-closed until a new proxy arrives.
                        for tid in tabs {
                            self.downloads.cancel_pending_saves_for_tab(tid);
                            if let Some(t) = self.tabs.get_mut(&tid) {
                                if let crate::wire::ContextKind::RemoteServer { proxy: cur, .. } = &mut t.context {
                                    *cur = None;
                                }
                                *t.prompts.proxy.lock().unwrap() = None;
                                t.deferred_initial.store(false, Ordering::Release);
                                let mut s = t.snap.lock().unwrap();
                                if s.loading {
                                    s.loading = false;
                                    let _ = self.out_tx.send(EngineMsg::TabUpdate { tab: s.clone() });
                                }
                            }
                            let _ = self.out_tx.send(EngineMsg::Refused {
                                tab_id: tid,
                                reason: "browser.proxy.required".to_owned(),
                            });
                        }
                    }
                    Err(fatal) => {
                        for tid in fatal.close_tabs {
                            self.close_and_refuse_tab(tid, fatal.reason);
                        }
                    }
                }
            }
            HostMsg::DropContext { context } => {
                let live_tabs = self.contexts.tabs_for_context(&context);
                for tid in live_tabs {
                    self.close_managed_tab(tid);
                }
                if let crate::wire::ContextKind::RemoteServer { server_id, .. } = &context {
                    self.cookies.on_partition_dropped(server_id);
                }
                self.contexts.drop_context(&context);
            }
            msg @ (HostMsg::ProfileCookiesList { .. }
            | HostMsg::ProfileCookiesSet { .. }
            | HostMsg::ProfileCookieDelete { .. }
            | HostMsg::ProfileProbeCookies { .. }
            | HostMsg::ClearProfileData { .. }
            | HostMsg::SetShareSignIns { .. }
            | HostMsg::SyncRemoteCookies { .. }) => self.handle_profile_cookies(msg),
            HostMsg::PreviewResponseHead { req_id, ok, status, mime, content_length, content_range } => {
                self.previews.deliver_head(req_id, ok, status, &mime, content_length, content_range);
            }
            HostMsg::PreviewResponseChunk { req_id, data_b64 } => {
                self.previews.deliver_chunk(req_id, &data_b64);
            }
            HostMsg::PreviewResponseEnd { req_id, ok } => {
                self.previews.deliver_end(req_id, ok);
            }
            HostMsg::ExtensionNotificationEvent { ext_id, notif_id, clicked, by_user } => self.notification_event(&ext_id, &notif_id, clicked, by_user),
            HostMsg::SetNotificationsEnabled { enabled } => {
                if let Some(ext) = &mut self.extensions {
                    ext.set_notifications_enabled(enabled);
                }
            }
            HostMsg::Shutdown => {
                self.permissions.shutdown();
                let drained: Vec<_> = self.tabs.drain().collect();
                for (tab_id, t) in drained {
                    self.release_preview(tab_id, t.preview.as_ref());
                    t.menu.cancel_all();
                    t.prompts.dismiss_all();
                    t.download_alive.store(false, Ordering::Release);
                    t.host.close_browser(1);
                }
                self.downloads.close_all();
                self.previews.cancel_all_proxies();
                self.contexts.release_all();
                return false;
            }
        }
        true
    }

    fn release_preview(&self, tab_id: u64, grant: Option<&Arc<PreviewGrant>>) {
        if let Some(g) = grant {
            g.revoke();
        }
        self.previews.cancel_tab_proxies(tab_id);
    }

    #[allow(clippy::too_many_arguments)]
    fn open_tab(
        &mut self,
        tab_id: u64,
        url: &str,
        width_pt: f32,
        height_pt: f32,
        scale: f32,
        focused: bool,
        file_roots: Vec<String>,
        context: crate::wire::ContextKind,
        preview_spec: Option<PreviewSpec>,
        agent_driven: bool,
        isolated_flag: bool,
        allow_extension_pages: bool,
        is_popup: bool,
        ext_open_request_id: Option<u64>,
    ) {
        // Cleanly release any prior browser/grant/context on duplicate `tab_id` before opening.
        if let Some(prev) = self.tabs.remove(&tab_id) {
            self.permissions.on_tab_closed(tab_id);
            self.release_preview(tab_id, prev.preview.as_ref());
            prev.menu.cancel_all();
            prev.prompts.dismiss_all();
            prev.download_alive.store(false, Ordering::Release);
            prev.host.close_browser(1);
            self.downloads.close_tab(tab_id, prev.download_gen);
            self.release_tab_context(tab_id);
            if !prev.isolated && !prev.is_popup
                && let Some(ext) = &mut self.extensions
            {
                ext.on_tab_closed(tab_id);
            }
        }
        // Normalize legacy `isolated: true` into a dedicated per-tab `ContextKind::Private`.
        let mut effective_context = if isolated_flag && context.is_default() && preview_spec.is_none() {
            crate::wire::ContextKind::Private
        } else {
            context
        };
        let mut preview_grant: Option<Arc<PreviewGrant>> = None;
        let mut refused_reason: Option<String> = None;
        let (mut refused_inherited_url, mut first_load_url): (Option<String>, Option<String>) = (None, None);
        let mut req_ctx: Option<RequestContext>;
        let context_ready;
        let deferred_initial;
        let tab_generation;
        let mut proxy_cfg = None;
        let mut is_allowed_ext_url: Option<String> = None;
        let mut allowed_ext_list: Vec<String> = Vec::new();
        let (initial_url, display_url, isolated, allow_extension_pages, is_popup) = if let Some(spec) = preview_spec.clone() {
            let (grant, internal, display) = match self.previews.create_grant(tab_id, spec, self.home.as_deref()) {
                Ok(v) => v,
                Err(code) => {
                    // Fail closed immediately — never fall through to `Default` context!
                    let _ = self.out_tx.send(EngineMsg::Refused {
                        tab_id,
                        reason: code.to_owned(),
                    });
                    return;
                }
            };
            let ctx_kind = grant.context_kind();
            let hooks = crate::preview::context::preview_context_hooks(
                grant.clone(),
                self.previews.clone(),
                self.out_tx.clone(),
            );
            let resolved_ctx = match self.contexts.resolve_with_hooks(&ctx_kind, tab_id, &internal, hooks) {
                Ok(r) => r,
                Err(fatal) => {
                    self.release_preview(tab_id, Some(&grant));
                    for tid in fatal.close_tabs {
                        self.close_and_refuse_tab(tid, fatal.reason);
                    }
                    let _ = self.out_tx.send(EngineMsg::Refused {
                        tab_id,
                        reason: fatal.reason.to_owned(),
                    });
                    return;
                }
            };
            effective_context = ctx_kind;
            preview_grant = Some(grant);
            req_ctx = resolved_ctx.request_context;
            context_ready = resolved_ctx.ready;
            deferred_initial = resolved_ctx.deferred_initial;
            tab_generation = resolved_ctx.generation;
            let init = if resolved_ctx.defer_initial_load {
                "about:blank".to_owned()
            } else {
                internal
            };
            (init, display, true, false, false)
        } else {
            let isolated = effective_context.is_isolated();
            let allow_ext = allow_extension_pages && !isolated && !agent_driven;
            let is_popup_valid = is_popup && allow_ext;
            let loaded_ext_list: Vec<String> = if allow_ext {
                self.extensions
                    .as_ref()
                    .map(|e| e.list_actions().into_iter().map(|a| a.ext_id).collect())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            let matched_ext = crate::extensions::manifest::extension_id_from_url(url)
                .filter(|id| loaded_ext_list.iter().any(|allowed| allowed == id))
                .map(str::to_owned);
            if is_popup && (!is_popup_valid || matched_ext.is_none()) {
                let _ = self.out_tx.send(EngineMsg::Refused {
                    tab_id,
                    reason: browser_policy::Refusal::Blocked.code().to_owned(),
                });
                return;
            }
            // Only tabs opened directly on an extension URL may navigate main frames inside that
            // extension's origin; regular web tabs keep `allowed_ext_list` empty.
            allowed_ext_list = matched_ext.clone().into_iter().collect();
            is_allowed_ext_url = matched_ext;
            let target_url = if is_allowed_ext_url.is_some() {
                url
            } else if let Some(why) = browser_policy::frame_guard(url, true, &file_roots) {
                refused_reason = Some(why.code().to_owned());
                "about:blank"
            } else if agent_driven
                && !isolated
                && self.permissions.has_inherited_default_grant(url)
            {
                refused_inherited_url = Some(url.to_owned());
                "about:blank"
            } else {
                url
            };
            let resolved_ctx = match self.contexts.resolve(&effective_context, tab_id, target_url) {
                Ok(r) => r,
                Err(fatal) => {
                    for tid in fatal.close_tabs {
                        self.close_and_refuse_tab(tid, fatal.reason);
                    }
                    let _ = self.out_tx.send(EngineMsg::Refused {
                        tab_id,
                        reason: fatal.reason.to_owned(),
                    });
                    return;
                }
            };
            if let Some((sid, applied, activate_ready)) = resolved_ctx.proxy_updated {
                for t in self.tabs.values_mut() {
                    if let crate::wire::ContextKind::RemoteServer { server_id: id, proxy: cur } = &mut t.context
                        && id.trim() == sid
                    {
                        *cur = Some(applied.clone());
                        *t.prompts.proxy.lock().unwrap() = Some(applied.clone());
                    }
                }
                if let Some(ready) = activate_ready {
                    ready.store(true, Ordering::Release);
                }
            }
            proxy_cfg = resolved_ctx.proxy;
            req_ctx = resolved_ctx.request_context;
            context_ready = resolved_ctx.ready;
            deferred_initial = resolved_ctx.deferred_initial;
            tab_generation = resolved_ctx.generation;
            let (init, display) = crate::clipboard_gate::first_load_urls(agent_driven, resolved_ctx.defer_initial_load, is_allowed_ext_url.is_some(), target_url, &mut first_load_url);
            (init, display, isolated, allow_ext, is_popup_valid)
        };
        let is_preview = preview_grant.is_some();
        let file_roots = Arc::new(Mutex::new(if is_preview { Vec::new() } else { file_roots }));
        let allowed_ext_ids = Arc::new(Mutex::new(allowed_ext_list));
        let view_state = Arc::new(Mutex::new(ViewRectState {
            width_pt: width_pt.max(1.0),
            height_pt: height_pt.max(1.0),
            scale: scale.max(1.0),
        }));
        let snap = Arc::new(Mutex::new(TabSnapshot {
            tab_id,
            url: display_url,
            title: "New tab".into(),
            loading: refused_reason.is_none(),
            can_go_back: false,
            can_go_forward: false,
            zoom: 1.0,
            cursor: CursorKind::Default,
            load_error: None,
            favicon: None,
            context: effective_context.redacted(),
            preview: preview_grant.as_ref().map(|g| g.kind.tag()),
            agent_driven,
            document: 0,
            audible: false,
        }));
        #[cfg(windows)]
        let frames: FrameRing = self
            .gpu
            .clone()
            .map(|gpu| Arc::new(Mutex::new(crate::d3d_frames::TabFrames::new(gpu, tab_id))));
        #[cfg(target_os = "linux")]
        let frames: FrameRing = self.transport.frames().map(|f| Arc::new(Mutex::new(f)));
        #[cfg(target_os = "macos")]
        let frames: FrameRing = ();
        #[cfg(any(windows, target_os = "linux"))]
        let shared_texture = frames.is_some();
        #[cfg(target_os = "macos")]
        let shared_texture = true;
        let awake = Arc::new(AtomicBool::new(true));
        let render = TabRenderHandler::new(
            tab_id,
            view_state.clone(),
            self.mach_port.clone(),
            self.out_tx.clone(),
            Arc::new(AtomicU64::new(0)),
            awake.clone(),
            frames.clone(),
            #[cfg(target_os = "linux")]
            self.events.clone(),
            #[cfg(not(target_os = "linux"))]
            (),
        );
        let prompts = Arc::new(crate::prompts::TabPrompts::default());
        prompts.open_as(agent_driven, preview_grant.is_some());
        *prompts.proxy.lock().unwrap() = proxy_cfg;
        let favicon = Arc::new(crate::favicon::TabFavicon::default());
        let diagnostics = Arc::new(crate::diagnostics::TabDiagnostics::default());
        let menu_state = Arc::new(crate::menu::TabMenuState::default());
        let scope_id = crate::permissions::context_scope_id(&effective_context, tab_id);
        if scope_id.is_some()
            && let Some(ctx) = req_ctx.as_ref()
        {
            crate::permissions::block_device_permission_guards_for_origin(ctx, &initial_url);
        }
        let display = TabDisplayHandler::new(
            tab_id,
            preview_grant.clone(),
            snap.clone(),
            prompts.clone(),
            self.permissions.clone(),
            scope_id.clone(),
            favicon,
            self.out_tx.clone(),
            self.events.clone(),
        );
        let load = TabLoadHandler::new(
            tab_id,
            file_roots.clone(),
            preview_grant.clone(),
            snap.clone(),
            prompts.clone(),
            diagnostics.clone(),
            deferred_initial.clone(),
            self.out_tx.clone(),
            self.events.clone(),
        );
        let input_guard = Arc::new(Mutex::new(crate::input_guard::InputGuard::default()));
        let emulation_scale = Arc::new(std::sync::atomic::AtomicU32::new(1.0f32.to_bits()));
        let request = TabRequestHandler::new(
            tab_id,
            file_roots.clone(),
            TabExtPolicy {
                allowed_ext_ids: allowed_ext_ids.clone(),
                popup_ext_id: is_popup.then(|| is_allowed_ext_url.clone()).flatten(),
                isolated,
            },
            preview_grant.clone(),
            context_ready.clone(),
            snap.clone(),
            self.prompt_shared.clone(),
            prompts.clone(),
            input_guard.clone(),
            emulation_scale.clone(),
            self.next_cdp_id.clone(),
            self.permissions.clone(),
            scope_id,
            diagnostics,
            self.out_tx.clone(),
            self.events.clone(),
        );
        let download_gen = self.downloads.alloc_tab_gen();
        let download_alive = Arc::new(AtomicBool::new(true));
        let block_downloads = is_popup
            || is_preview
            || matches!(
                effective_context,
                crate::wire::ContextKind::Preview { .. } | crate::wire::ContextKind::Artifact { .. }
            );
        let download = crate::downloads::TabDownloadHandler::new(
            tab_id,
            download_gen,
            download_alive.clone(),
            block_downloads,
            effective_context.is_isolated(),
            context_ready.clone(),
            prompts.clone(),
            self.downloads.clone(),
            self.out_tx.clone(),
        );
        let life_span = crate::prompts::TabLifeSpanHandler::new(
            tab_id,
            is_preview,
            context_ready.clone(),
            prompts.clone(),
            self.out_tx.clone(),
        );
        let jsdialog = crate::prompts::TabDialogHandler::new(
            tab_id,
            self.prompt_shared.clone(),
            prompts.clone(),
            preview_grant.clone(),
            self.out_tx.clone(),
        );
        let permission = crate::prompts::TabPermissionHandler::new(
            tab_id,
            is_preview,
            context_ready.clone(),
            self.prompt_shared.clone(),
            prompts.clone(),
            self.out_tx.clone(),
        );
        let dialog = crate::prompts::TabFileDialogHandler::new(
            tab_id,
            is_preview,
            prompts.clone(),
            self.out_tx.clone(),
        );
        let find = crate::find::TabFindHandler::new(
            tab_id,
            self.out_tx.clone(),
            std::sync::Arc::new(crate::find::FindState::default()),
        );
        let ext_menus = (!isolated && !is_popup && preview_grant.is_none())
            .then(|| self.extensions.as_ref().map(|e| e.context_menu_catalog()))
            .flatten();
        let context_menu = crate::menu::TabContextMenuHandler::new(
            tab_id,
            self.prompt_shared.clone(),
            prompts.clone(),
            file_roots.clone(),
            preview_grant.clone(),
            ext_menus,
            menu_state.clone(),
            self.out_tx.clone(),
        );
        let mut client = SidecarTabClient::new(
            render, display, load, request, download, life_span, jsdialog, permission, dialog, find,
            context_menu, crate::tab_client::TabFrameHandler::new(tab_id, prompts.clone(), self.events.clone()), prompts.clone(),
        );

        let mut win_info = WindowInfo::default().set_as_windowless(Default::default());
        win_info.shared_texture_enabled = i32::from(shared_texture);
        win_info.external_begin_frame_enabled = 0;

        let browser_settings = crate::clipboard_gate::browser_settings(BASE_COLOR, is_preview || agent_driven);

        // Hard fail-closed invariant: an isolated or preview tab must NEVER open without a
        // dedicated `CefRequestContext` (falling back to `None` would open it inside `Default`).
        if ((effective_context.is_isolated() || is_preview) && req_ctx.is_none()) || (req_ctx.is_none() && !self.default_locked) {
            self.release_preview(tab_id, preview_grant.as_ref());
            self.release_tab_context(tab_id);
            let _ = self.out_tx.send(EngineMsg::Refused {
                tab_id,
                reason: "browser.context.unavailable".to_owned(),
            });
            return;
        }

        // Tell ExtensionHost what kind of OSR target CEF is about to create synchronously.
        self.expect_ext_tab_target(isolated, is_preview, is_popup, is_allowed_ext_url.as_deref());

        let Some(browser) = cef::browser_host_create_browser_sync(
            Some(&win_info),
            Some(&mut client),
            Some(&CefString::from(initial_url.as_str())),
            Some(&browser_settings),
            crate::tab_client::print_gate_info(agent_driven || is_preview).as_mut(),
            req_ctx.as_mut(),
        ) else {
            self.release_preview(tab_id, preview_grant.as_ref());
            self.release_tab_context(tab_id);
            if let Some(ext) = &mut self.extensions {
                ext.cancel_expected_target();
            }
            if let Some(req_id) = ext_open_request_id
                && let Some((sid, rpc_id, _)) = self.pending_ext_opens.remove(&req_id)
                && let Some(ext) = &mut self.extensions
            {
                ext.fail_tab_create(&sid, rpc_id, "Could not create browser tab.");
            }
            let _ = self.out_tx.send(EngineMsg::Refused {
                tab_id,
                reason: "browser.context.unavailable".to_owned(),
            });
            return;
        };
        let Some(host) = browser.host() else {
            self.release_preview(tab_id, preview_grant.as_ref());
            self.release_tab_context(tab_id);
            if let Some(ext) = &mut self.extensions {
                ext.cancel_expected_target();
            }
            let _ = self.out_tx.send(EngineMsg::Refused {
                tab_id,
                reason: "browser.context.unavailable".to_owned(),
            });
            return;
        };
        // Initial visibility, FPS throttling, and GPU frame ring release are applied
        // uniformly via `self.on_tab_opened` right after insertion into `self.tabs`.

        let cdp_id_map = Arc::new(Mutex::new(HashMap::new()));
        let cdp_ledger = Arc::new(Mutex::new(crate::cdp_ledger::EngineCdpLedger::with_registered_exts(self.registered_ext_ids.clone())));
        let passkeys = Arc::new(crate::passkeys::TabPasskeyGate::new(tab_id, agent_driven));
        let media_gate = Arc::new(crate::media_gate::TabMediaGate::new());
        let notification_gate = Arc::new(crate::notification_gate::TabNotificationGate::new());
        let ext_cw = is_allowed_ext_url.as_deref().is_some_and(|id| self.ext_clipboard_write_ids.lock().unwrap().contains(id));
        let clipboard_gate = Arc::new(crate::clipboard_gate::TabClipboardGate::new(is_popup, is_preview, agent_driven, ext_cw, self.ext_clipboard_write_ids.clone(), first_load_url));
        let internal_picked = Arc::new(Mutex::new(Vec::new()));
        let pending_clears = Arc::new(Mutex::new(Vec::new()));
        let mut observer = TabCdpObserver::new(
            tab_id, is_popup, isolated, is_preview, preview_grant.clone(), snap.clone(), cdp_id_map.clone(),
            internal_picked.clone(), pending_clears.clone(), cdp_ledger.clone(), passkeys.clone(),
            media_gate.clone(), notification_gate.clone(), clipboard_gate.clone(), allowed_ext_ids.clone(), context_ready, self.prompt_shared.clone(), input_guard.clone(), emulation_scale.clone(),
            self.next_cdp_id.clone(), prompts.clone(), self.permissions.clone(),
            self.extensions.is_some(), self.events.clone(), self.out_tx.clone(),
        );
        let cdp_reg = host.add_dev_tools_message_observer(Some(&mut observer));
        // Extension tabs/popups use `arm_extension_tab` (`ExtensionDeferred`, #695-B1) so gates keep `Page`/`Runtime` while `Target.setAutoAttach` waits for any web commit.
        let mut arm = if is_popup || is_allowed_ext_url.is_some() { cdp_ledger.lock().unwrap().arm_extension_tab() } else { cdp_ledger.lock().unwrap().arm() };
        arm.extend(passkeys.attach_messages(None));
        arm.extend(media_gate.attach_messages(None));
        arm.extend(notification_gate.attach_messages(None));
        arm.extend(clipboard_gate.attach_messages(None));
        if agent_driven || is_preview {
            arm.push(passkeys.file_chooser_intercept_msg(true, None));
        }
        if !passkeys.native() || agent_driven {
            arm.extend(passkeys.enforce_messages());
        }
        for raw in arm {
            host.send_dev_tools_message(Some(raw.as_bytes()));
        }
        if let Some(dark) = self.dark {
            self.send_color_scheme(&host, dark);
        }

        self.seed_remote_partition(&effective_context);
        self.tabs.insert(
            tab_id,
            ManagedTab {
                browser,
                host,
                file_roots,
                allowed_ext_ids,
                allow_extension_pages,
                isolated,
                is_popup,
                preview: preview_grant,
                #[cfg(target_os = "linux")]
                preview_spec,
                view_state,
                snap,
                cdp_id_map,
                cdp_ledger,
                passkeys,
                media_gate,
                notification_gate,
                clipboard_gate,
                input_guard,
                emulation_scale,
                internal_picked,
                pending_clears,
                prompts,
                menu: menu_state,
                download_gen,
                download_alive,
                context: effective_context,
                last_loading: false,
                generation: tab_generation,
                deferred_initial,
                _cdp_reg: cdp_reg,
                held_buttons: 0,
                render_mode: lifecycle::TabRenderMode::AwakeBackground,
                awake,
                #[cfg(target_os = "linux")]
                shown: focused,
                #[cfg(any(windows, target_os = "linux"))]
                frames,
            },
        );
        self.on_tab_opened(tab_id, focused, is_popup);
        if !isolated && !is_preview && !is_popup {
            self.notify_ext_tab_opened(tab_id, &initial_url, width_pt, height_pt, focused, ext_open_request_id, agent_driven);
        }
        if let Some(reason) = refused_reason {
            let _ = self.out_tx.send(EngineMsg::Refused { tab_id, reason });
        }
        if let Some(refused_url) = refused_inherited_url {
            let _ = self.out_tx.send(EngineMsg::ControlledPermissionRefused {
                tab_id,
                url: Some(refused_url),
            });
        }
    }

    fn close_managed_tab(&mut self, tab_id: u64) {
        self.on_tab_closed_lifecycle(tab_id);
        self.permissions.on_tab_closed(tab_id);
        if let Some(tab) = self.tabs.remove(&tab_id) {
            if self.prompt_shared.release_trace(tab_id) {
                self.send_internal_cdp(&tab.host, "Tracing.end", json!({}));
            }
            self.release_preview(tab_id, tab.preview.as_ref());
            tab.menu.cancel_all();
            tab.prompts.dismiss_all();
            tab.download_alive.store(false, Ordering::Release);
            tab.host.close_browser(1);
            self.downloads.close_tab(tab_id, tab.download_gen);
            crate::agent::remove_tab_uploads(&self.profile_dir, tab_id);
            self.release_tab_context(tab_id);
            if !tab.isolated && !tab.is_popup
                && let Some(ext) = &mut self.extensions
            {
                ext.on_tab_closed(tab_id);
            }
            let _ = self.out_tx.send(EngineMsg::TabClosed { tab_id });
        } else {
            self.release_tab_context(tab_id);
        }
    }

    fn close_and_refuse_tab(&mut self, tab_id: u64, reason: &str) {
        let _ = self.out_tx.send(EngineMsg::Refused {
            tab_id,
            reason: reason.to_owned(),
        });
        self.close_managed_tab(tab_id);
    }

    fn send_color_scheme(&self, host: &BrowserHost, dark: bool) {
        let value = if dark { "dark" } else { "light" };
        self.send_internal_cdp(host, "Emulation.setEmulatedMedia", json!({ "features": [{ "name": "prefers-color-scheme", "value": value }] }));
    }

    pub(super) fn send_internal_cdp(&self, host: &BrowserHost, method: &str, params: Value) {
        let id = self.next_cdp_id.fetch_add(1, Ordering::Relaxed);
        let raw = json!({ "id": id, "method": method, "params": params }).to_string();
        let _ = host.send_dev_tools_message(Some(raw.as_bytes()));
    }
}

impl Drop for EngineManager {
    fn drop(&mut self) {
        self.permissions.shutdown();
        self.downloads.close_all();
    }
}

pub(crate) use extensions::{Surface, classify_surface};
mod emulation;
mod extensions;
mod frames;
mod lifecycle;
mod picks;
mod profile_cookies;
