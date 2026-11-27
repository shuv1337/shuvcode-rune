//! Extension host effect runner and hidden seed-probe browser for `EngineManager`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cef::{BrowserSettings, CefString, ImplBrowser, ImplBrowserHost, ImplFrame, WindowInfo};
use serde_json::{Value, json};

use super::{BASE_COLOR, EngineManager, ManagedTab};
use crate::handlers::{
    CdpPending, TabDisplayHandler, TabExtPolicy, TabLoadHandler, TabRenderHandler,
    TabRequestHandler, ViewRectState,
};
use crate::output::Output;
use crate::prompts::{
    PromptShared, TabDialogHandler, TabFileDialogHandler, TabLifeSpanHandler,
    TabPermissionHandler, TabPrompts,
};
use crate::passkeys::AgentGate;
use crate::tab_client::SidecarTabClient;
use crate::wire::{ContextKind, CursorKind, EngineMsg, HostMsg, TabSnapshot};

const MAX_PENDING_EXT_OPENS: usize = 16;
const MAX_UNMANAGED_EXT_TABS: usize = 16;

impl EngineManager {
    /// A toast's click or close: a page's own (`page:<tab>`, `notification_gate.rs`) goes back to
    /// its page, and a click brings its tab forward; an extension's goes to the extension host.
    pub(super) fn notification_event(&mut self, ext_id: &str, notif_id: &str, clicked: bool, by_user: bool) {
        let Some(tab_id) = ext_id.strip_prefix("page:").and_then(|id| id.parse::<u64>().ok()) else {
            if let Some(ext) = &mut self.extensions {
                let effects = ext.on_notification_event(ext_id, notif_id, clicked, by_user);
                self.run_ext_effects(effects);
            }
            return;
        };
        let Some(t) = self.tabs.get(&tab_id) else { return };
        let Some(msg) = t.notification_gate.event_msg(notif_id, clicked) else { return };
        t.host.send_dev_tools_message(Some(msg.as_bytes()));
        if clicked {
            let _ = self.out_tx.send(EngineMsg::ExtensionFocusTab { tab_id });
        }
    }

    pub fn shutdown_extensions(&mut self) {
        if let Some(probe) = self.seed_probe.take() {
            probe.close_browser(1);
        }
        if let Some(ext) = &self.extensions {
            ext.close_pipe();
        }
    }

    pub fn on_extensions_verified(&mut self, startup: crate::extensions::integrity::StartupExtensions) {
        let effects = match &mut self.extensions {
            Some(ext) => ext.on_startup_verified(startup),
            None => return,
        };
        self.refresh_ext_mirrors();
        self.run_ext_effects(effects);
    }

    fn refresh_ext_mirrors(&self) {
        let next = self.extensions.as_ref().map(|e| e.clipboard_write_ext_ids()).unwrap_or_default();
        *self.ext_clipboard_write_ids.lock().unwrap() = next;
        let reg = self.extensions.as_ref().map(|e| e.registered_ext_ids()).unwrap_or_default();
        *self.registered_ext_ids.lock().unwrap() = reg;
    }

    fn push_ext_clipboard_write_for_tabs(&self, ext_id: &str, allowed: bool) {
        for t in self.tabs.values() {
            let cur_url = t.snap.lock().unwrap().url.clone();
            if crate::extensions::manifest::extension_id_from_url(&cur_url) == Some(ext_id) {
                for raw in t.clipboard_gate.sync_ext_contexts_for(ext_id, allowed) {
                    t.host.send_dev_tools_message(Some(raw.as_bytes()));
                }
            }
        }
    }

    pub fn poll_extensions(&mut self) -> bool {
        let effects = match &mut self.extensions {
            Some(ext) => ext.drain_inbound(),
            None => return false,
        };
        let had = !effects.is_empty();
        self.run_ext_effects(effects);
        had
    }

    pub fn on_tab_meta(
        &mut self,
        tab_id: u64,
        url: Option<String>,
        title: Option<String>,
        loading: Option<bool>,
    ) {
        let is_eligible = self.tabs.get(&tab_id).is_some_and(|t| !t.isolated && !t.is_popup);
        if !is_eligible {
            return;
        }
        let Some(ext) = &mut self.extensions else { return };
        if let Some(u) = url {
            ext.on_tab_url_changed(tab_id, &u);
        }
        if let Some(t) = title {
            ext.on_tab_title_changed(tab_id, &t);
        }
        if let Some(l) = loading {
            ext.on_tab_loading_changed(tab_id, l);
        }
    }

    pub fn on_web_navigation(&mut self, sig: crate::extensions::web_nav::WebNavSignal) {
        let is_eligible = self.tabs.get(&sig.tab_id).is_some_and(|t| !t.isolated && !t.is_popup);
        if !is_eligible {
            return;
        }
        if let Some(ext) = &mut self.extensions {
            ext.on_web_navigation(sig);
        }
    }

    pub(super) fn on_tab_focus_extensions(&mut self, tab_id: u64) {
        let (is_normal, is_popup) = self
            .tabs
            .get(&tab_id)
            .map(|t| (!t.isolated && !t.is_popup, t.is_popup))
            .unwrap_or((false, false));
        if let Some(ext) = &mut self.extensions {
            if is_normal {
                ext.on_tab_focused(tab_id);
            } else if !is_popup {
                ext.on_window_blurred();
            }
        }
    }

    pub(super) fn on_tab_resize_extensions(&mut self, tab_id: u64, width_pt: f32, height_pt: f32) {
        let is_normal = self.tabs.get(&tab_id).is_some_and(|t| !t.isolated && !t.is_popup);
        if is_normal && let Some(ext) = &mut self.extensions {
            ext.on_tab_resized(tab_id, width_pt.round() as i32, height_pt.round() as i32);
        }
    }

    pub(super) fn cmd_click_extension(&mut self, ext_id: &str) {
        let effects = self.extensions.as_mut().map(|e| e.click_extension(ext_id)).unwrap_or_default();
        self.run_ext_effects(effects);
    }

    pub(super) fn cmd_list_extensions(&self, request_id: u64) {
        let extensions = self.extensions.as_ref().map(|e| e.list_actions()).unwrap_or_default();
        let _ = self.out_tx.send(EngineMsg::ExtensionsState { request_id: Some(request_id), extensions });
    }

    pub(super) fn cmd_uninstall_extension(&mut self, ext_id: &str) {
        let effects = self.extensions.as_mut().map(|e| e.uninstall_extension(ext_id)).unwrap_or_default();
        self.run_ext_effects(effects);
    }

    pub(super) fn cmd_reload_extension(&mut self, ext_id: &str) {
        let effects = self.extensions.as_mut().map(|e| e.reload_extension(ext_id)).unwrap_or_default();
        self.refresh_ext_mirrors();
        self.run_ext_effects(effects);
    }

    pub(super) fn cmd_cancel_extension_open_tab(&mut self, request_id: u64) {
        if let Some((sid, rpc_id, _)) = self.pending_ext_opens.remove(&request_id)
            && let Some(ext) = &mut self.extensions
        {
            ext.fail_tab_create(&sid, rpc_id, "Tab open refused by browser policy.");
        }
    }

    pub(super) fn cmd_extension_metrics(&self, request_id: u64) {
        let metrics = self.extensions.as_ref().map(|e| e.metrics.clone()).unwrap_or_default();
        let _ = self.out_tx.send(EngineMsg::ExtensionMetrics { request_id, metrics });
    }

    pub(super) fn run_ext_effects(&mut self, effects: Vec<crate::extensions::ExtEffect>) {
        use crate::extensions::ExtEffect;
        for effect in effects {
            match effect {
                ExtEffect::OpenTab { ext_id, url, active, reply_to } => {
                    if self.app_handles_ext_tabs {
                        if self.pending_ext_opens.len() >= MAX_PENDING_EXT_OPENS {
                            if let Some((sid, rpc_id, _)) = reply_to
                                && let Some(ext) = &mut self.extensions
                            {
                                ext.fail_tab_create(&sid, rpc_id, "Too many pending tab requests.");
                            }
                            continue;
                        }
                        let request_id = self.next_ext_open_req;
                        self.next_ext_open_req += 1;
                        if let Some(reply) = reply_to {
                            self.pending_ext_opens.insert(request_id, reply);
                        }
                        let _ = self.out_tx.send(EngineMsg::ExtensionOpenTab {
                            request_id,
                            ext_id,
                            url,
                            active,
                        });
                    } else {
                        if self.unmanaged_ext_tabs >= MAX_UNMANAGED_EXT_TABS {
                            if let Some((sid, rpc_id, _)) = reply_to
                                && let Some(ext) = &mut self.extensions
                            {
                                ext.fail_tab_create(&sid, rpc_id, "Too many extension tabs open.");
                            }
                            continue;
                        }
                        self.unmanaged_ext_tabs += 1;
                        let tab_id = self.next_ext_tab_id;
                        self.next_ext_tab_id += 1;
                        self.open_tab(
                            tab_id,
                            &url,
                            800.0,
                            600.0,
                            1.0,
                            active,
                            Vec::new(),
                            ContextKind::Default,
                            None,
                            false,
                            false,
                            true,
                            false,
                            None,
                        );
                        if let Some((sid, req_id, ext_id)) = reply_to
                            && let Some(ext) = &mut self.extensions
                        {
                            ext.finish_tab_create(&sid, req_id, &ext_id, tab_id);
                        }
                    }
                }
                ExtEffect::NavigateTab { engine_tab_id, url } => {
                    if let Some(t) = self.tabs.get(&engine_tab_id) {
                        if t.prompts.agent_driven.load(Ordering::Relaxed) {
                            continue;
                        }
                        if let Some(frame) = t.browser.main_frame() {
                            t.prompts.set_pending_top_url(Some(url.clone()));
                            frame.load_url(Some(&CefString::from(url.as_str())));
                        }
                    }
                }
                ExtEffect::FocusTab { engine_tab_id } => {
                    self.handle_ext_focus_tab(engine_tab_id);
                    if let Some(ext) = &mut self.extensions {
                        ext.on_tab_focused(engine_tab_id);
                    }
                    let _ = self.out_tx.send(EngineMsg::ExtensionFocusTab { tab_id: engine_tab_id });
                }
                ExtEffect::CloseTab { engine_tab_id } => {
                    if self
                        .tabs
                        .get(&engine_tab_id)
                        .is_some_and(|t| t.prompts.agent_driven.load(Ordering::Relaxed))
                    {
                        continue;
                    }
                    if let Some(tab) = self.tabs.remove(&engine_tab_id) {
                        self.on_tab_closed_lifecycle(engine_tab_id);
                        self.permissions.on_tab_closed(engine_tab_id);
                        tab.menu.cancel_all();
                        tab.prompts.dismiss_all();
                        tab.download_alive.store(false, Ordering::Release);
                        tab.host.close_browser(1);
                        self.downloads.close_tab(engine_tab_id, tab.download_gen);
                        self.release_tab_context(engine_tab_id);
                        if engine_tab_id >= 1_000_000 {
                            self.unmanaged_ext_tabs = self.unmanaged_ext_tabs.saturating_sub(1);
                        }
                        if !tab.isolated
                            && !tab.is_popup
                            && let Some(ext) = &mut self.extensions
                        {
                            ext.on_tab_closed(engine_tab_id);
                        }
                        let _ = self.out_tx.send(EngineMsg::TabClosed { tab_id: engine_tab_id });
                    }
                }
                ExtEffect::ReloadTab { engine_tab_id, ignore_cache } => {
                    if let Some(t) = self.tabs.get(&engine_tab_id) {
                        if t.prompts.agent_driven.load(Ordering::Relaxed) {
                            continue;
                        }
                        if ignore_cache {
                            t.browser.reload_ignore_cache();
                        } else {
                            t.browser.reload();
                        }
                    }
                }
                ExtEffect::OpenSeedProbe { url } => self.open_seed_probe(&url),
                ExtEffect::CloseSeedProbe => {
                    if let Some(probe) = self.seed_probe.take() {
                        probe.close_browser(1);
                    }
                }
                ExtEffect::ActionUpdated(action) => {
                    let _ = self.out_tx.send(EngineMsg::ExtensionAction { action });
                }
                ExtEffect::ExtensionsSynced(extensions) => {
                    self.refresh_ext_mirrors();
                    let _ = self.out_tx.send(EngineMsg::ExtensionsState {
                        request_id: None,
                        extensions,
                    });
                }
                ExtEffect::FiltersReady => {
                    let _ = self.out_tx.send(EngineMsg::ExtensionFiltersReady);
                }
                ExtEffect::IntegrityFailed { ext_id } => {
                    if !self.rejected_exts.contains(&ext_id) {
                        self.rejected_exts.push(ext_id.clone());
                    }
                    self.ext_clipboard_write_ids.lock().unwrap().remove(&ext_id);
                    self.registered_ext_ids.lock().unwrap().remove(&ext_id);
                    self.push_ext_clipboard_write_for_tabs(&ext_id, false);
                    let _ = self.out_tx.send(EngineMsg::ExtensionIntegrityFailed { ext_id });
                }
                ExtEffect::PopupResized { ext_id, width, height } => {
                    let _ = self.out_tx.send(EngineMsg::ExtensionPopupResized { ext_id, width, height });
                }
                ExtEffect::OpenPopup { ext_id, url } => {
                    let _ = self.out_tx.send(EngineMsg::ExtensionPopupRequested { ext_id, url });
                }
                ExtEffect::ExtensionUninstalled { ext_id, ok, error } => {
                    if ok {
                        self.ext_clipboard_write_ids.lock().unwrap().remove(&ext_id);
                        self.registered_ext_ids.lock().unwrap().remove(&ext_id);
                        let prefix = format!("chrome-extension://{ext_id}/");
                        let stale_popups: Vec<u64> = self
                            .tabs
                            .iter()
                            .filter(|(_, t)| t.is_popup && t.snap.lock().unwrap().url.starts_with(&prefix))
                            .map(|(&id, _)| id)
                            .collect();
                        for pid in stale_popups {
                            if let Some(tab) = self.tabs.remove(&pid) {
                                tab.menu.cancel_all();
                                tab.prompts.dismiss_all();
                                tab.download_alive.store(false, Ordering::Release);
                                tab.host.close_browser(1);
                                self.downloads.close_tab(pid, tab.download_gen);
                                let _ = self.out_tx.send(EngineMsg::TabClosed { tab_id: pid });
                            }
                        }
                        for t in self.tabs.values() {
                            t.allowed_ext_ids.lock().unwrap().retain(|id| id != &ext_id);
                        }
                        self.push_ext_clipboard_write_for_tabs(&ext_id, false);
                    }
                    let _ = self.out_tx.send(EngineMsg::ExtensionUninstalled { ext_id, ok, error });
                }
                ExtEffect::ExtensionReloaded { ext_id, ok, error } => {
                    // Resync directly from `ExtensionHost` on both `ok` and `!ok` so early reload
                    // failures that leave the existing extension loaded keep its mirror entries
                    // while load/pipe failures that removed it clear them (`#709-B1`).
                    self.refresh_ext_mirrors();
                    let has_cw = self.extensions.as_ref().is_some_and(|e| e.has_clipboard_write(&ext_id));
                    self.push_ext_clipboard_write_for_tabs(&ext_id, has_cw);
                    let _ = self.out_tx.send(EngineMsg::ExtensionReloaded { ext_id, ok, error });
                }
                ExtEffect::NotificationShow { ext_id, ext_name, notif_id, body } => {
                    let _ = self.out_tx.send(EngineMsg::ExtensionNotificationShow { ext_id, ext_name, notif_id, body });
                }
                ExtEffect::NotificationClear { ext_id, notif_id } => {
                    let _ = self.out_tx.send(EngineMsg::ExtensionNotificationClear { ext_id, notif_id });
                }
                ExtEffect::WakeExtensionWorker { ext_id } => {
                    if !self.extensions.as_ref().is_some_and(|e| e.is_loaded_extension(&ext_id)) {
                        continue;
                    }
                    static SW_WAKE_SEQ: AtomicU64 = AtomicU64::new(0);
                    let host = self
                        .tabs
                        .values()
                        .find(|t| !t.isolated && !t.prompts.agent_driven.load(Ordering::Acquire))
                        .or_else(|| self.tabs.values().find(|t| !t.isolated))
                        .map(|t| t.host.clone())
                        .or_else(|| self.seed_probe.clone());
                    if let Some(host) = host {
                        let base = crate::extensions::sw_wake::SW_WAKE_ID_BASE
                            + ((SW_WAKE_SEQ.fetch_add(2, Ordering::Relaxed) % 9_000_000) as i32);
                        let scope = format!("chrome-extension://{ext_id}/");
                        for msg in [
                            json!({ "id": base, "method": "ServiceWorker.enable", "params": {} }),
                            json!({ "id": base + 1, "method": "ServiceWorker.startWorker", "params": { "scopeURL": scope } }),
                        ] {
                            let raw = msg.to_string();
                            let _ = host.send_dev_tools_message(Some(raw.as_bytes()));
                        }
                    }
                }
            }
        }
    }

    pub(super) fn open_seed_probe(&mut self, url: &str) {
        if self.seed_probe.is_some() {
            if let Some(ext) = &mut self.extensions {
                ext.cancel_expected_target();
            }
            return;
        }
        let Some(ext_id) = crate::extensions::manifest::extension_id_from_url(url).map(str::to_owned) else {
            if let Some(ext) = &mut self.extensions {
                ext.cancel_expected_target();
            }
            return;
        };
        // Null output sink so this short-lived internal probe never emits wire messages
        // (`tab_id = u64::MAX`) to the desktop app.
        let null_out = Output::null();
        let prompt_shared = PromptShared::default();
        // Do NOT set `agent_driven = true` (which blocks `chrome-extension://` in `on_before_browse`);
        // instead pin `popup_ext_id = Some(ext_id)` so this probe can ONLY load that single origin.
        let prompts = Arc::new(TabPrompts::default());
        let favicon = Arc::new(crate::favicon::TabFavicon::default());
        let diagnostics = Arc::new(crate::diagnostics::TabDiagnostics::default());
        let menu_state = Arc::new(crate::menu::TabMenuState::default());
        let view_state = Arc::new(Mutex::new(ViewRectState { width_pt: 2.0, height_pt: 2.0, scale: 1.0 }));
        let snap = Arc::new(Mutex::new(TabSnapshot {
            tab_id: u64::MAX,
            url: url.to_owned(),
            title: String::new(),
            loading: true,
            can_go_back: false,
            can_go_forward: false,
            zoom: 1.0,
            cursor: CursorKind::Default,
            load_error: None,
            favicon: None,
            context: ContextKind::Default,
            preview: None,
            agent_driven: false,
            document: 0,
            audible: false,
        }));
        let allowed_ext = vec![ext_id.clone()];
        let file_roots = Arc::new(Mutex::new(Vec::new()));
        let context_ready = Arc::new(AtomicBool::new(true));
        let render = TabRenderHandler::new(
            u64::MAX,
            view_state,
            self.mach_port.clone(),
            null_out.clone(),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(true)),
            #[cfg(windows)]
            None,
            #[cfg(target_os = "linux")]
            None,
            #[cfg(target_os = "macos")]
            (),
            #[cfg(target_os = "linux")]
            self.events.clone(),
            #[cfg(not(target_os = "linux"))]
            (),
        );
        let display = TabDisplayHandler::new(
            u64::MAX,
            None,
            snap.clone(),
            prompts.clone(),
            self.permissions.clone(),
            None,
            favicon,
            null_out.clone(),
            self.events.clone(),
        );
        let load = TabLoadHandler::new(
            u64::MAX,
            file_roots.clone(),
            None,
            snap.clone(),
            prompts.clone(),
            diagnostics.clone(),
            Arc::new(AtomicBool::new(false)),
            null_out.clone(),
            self.events.clone(),
        );
        let request = TabRequestHandler::new(
            u64::MAX,
            file_roots.clone(),
            TabExtPolicy {
                allowed_ext_ids: Arc::new(Mutex::new(allowed_ext)),
                popup_ext_id: Some(ext_id),
                isolated: false,
            },
            None,
            context_ready.clone(),
            snap.clone(),
            prompt_shared.clone(),
            prompts.clone(),
            Arc::new(Mutex::new(crate::input_guard::InputGuard::default())),
            Arc::new(std::sync::atomic::AtomicU32::new(1.0f32.to_bits())),
            self.next_cdp_id.clone(),
            self.permissions.clone(),
            None,
            diagnostics,
            null_out.clone(),
            self.events.clone(),
        );
        let download_gen = self.downloads.alloc_tab_gen();
        let download_alive = Arc::new(AtomicBool::new(true));
        let download = crate::downloads::TabDownloadHandler::new(
            u64::MAX,
            download_gen,
            download_alive,
            true,
            false,
            context_ready.clone(),
            prompts.clone(),
            self.downloads.clone(),
            null_out.clone(),
        );
        let life_span = TabLifeSpanHandler::new(u64::MAX, false, context_ready.clone(), prompts.clone(), null_out.clone());
        let jsdialog = TabDialogHandler::new(
            u64::MAX,
            prompt_shared.clone(),
            prompts.clone(),
            None,
            null_out.clone(),
        );
        let permission = TabPermissionHandler::new(
            u64::MAX,
            false,
            context_ready,
            prompt_shared.clone(),
            prompts.clone(),
            null_out.clone(),
        );
        let dialog = TabFileDialogHandler::new(u64::MAX, false, prompts.clone(), null_out.clone());
        let find = crate::find::TabFindHandler::new(
            u64::MAX,
            null_out.clone(),
            std::sync::Arc::new(crate::find::FindState::default()),
        );
        let context_menu = crate::menu::TabContextMenuHandler::new(
            u64::MAX,
            prompt_shared,
            prompts.clone(),
            file_roots,
            None,
            None,
            menu_state,
            null_out,
        );
        let mut client = SidecarTabClient::new(
            render,
            display,
            load,
            request,
            download,
            life_span,
            jsdialog,
            permission,
            dialog,
            find,
            context_menu,
            crate::tab_client::TabFrameHandler::new(u64::MAX, prompts.clone(), self.events.clone()),
            prompts,
        );
        let mut win_info = WindowInfo::default().set_as_windowless(Default::default());
        win_info.shared_texture_enabled = 0;
        let browser_settings = BrowserSettings {
            windowless_frame_rate: 1,
            background_color: BASE_COLOR,
            ..Default::default()
        };
        if let Some(browser) = cef::browser_host_create_browser_sync(
            Some(&win_info),
            Some(&mut client),
            Some(&CefString::from(url)),
            Some(&browser_settings),
            None,
            None,
        ) {
            if let Some(host) = browser.host() {
                host.was_hidden(1);
                self.seed_probe = Some(host);
            } else if let Some(ext) = &mut self.extensions {
                ext.cancel_expected_target();
            }
        } else if let Some(ext) = &mut self.extensions {
            ext.cancel_expected_target();
        }
    }

    pub(super) fn handle_cdp_call(
        &mut self,
        tab_id: u64,
        request_id: u64,
        method: String,
        params: Value,
        session_id: Option<String>,
    ) {
        let Some(t) = self.tabs.get(&tab_id) else {
            return;
        };
        if !self.contexts.is_tab_ready(tab_id) {
            let reason = self.contexts.not_ready_reason(tab_id);
            let _ = self.out_tx.send(EngineMsg::CdpResponse {
                tab_id,
                request_id,
                ok: false,
                result: json!({ "message": reason }),
            });
            return;
        }
        // Reject any cross-session / OOPIF sessionId so external callers can never target
        // extension iframes or non-page targets (`review-rd274.md` §1).
        if session_id.is_some() {
            let _ = self.out_tx.send(EngineMsg::CdpResponse {
                tab_id,
                request_id,
                ok: false,
                result: json!({ "message": "Cross-session DevTools targeting is not permitted." }),
            });
            return;
        }
        // Fail closed on the entire tab whenever ANY frame (main OR sub-frame) is, or is about to
        // become, an extension frame — checked at dispatch time on both agent-driven tabs and
        // unflagged 3P plugin `DevtoolsAsync` tabs (`review-rd274.md` Round-4 ruling).
        if tab_is_extension_surface(t) {
            let _ = self.out_tx.send(EngineMsg::CdpResponse {
                tab_id,
                request_id,
                ok: false,
                result: json!({ "message": "Agent browser tools cannot read or act on extension pages or tabs with extension frames." }),
            });
            return;
        }
        // Official has no raw DOM or accessibility access at all; with extensions active, a raw
        // caller could reach content scripts' closed shadow roots, so only typed calls may.
        let raw_dom = self.extensions.is_some() && (method.starts_with("DOM.") || method.starts_with("Accessibility."));
        let reach = crate::cdp_ledger::grant_gated(t)
            .then(|| t.cdp_ledger.lock().unwrap().reach(&method, None, None, &params));
        let granted = reach.as_ref().is_some_and(|r| crate::cdp_ledger::granted_origin(t, &self.permissions, r).is_some());
        let checked = if granted {
            Err(crate::permissions::INHERITED_PERMISSION_MSG.to_owned())
        } else if raw_dom {
            Err("DOM and accessibility DevTools calls are not permitted while browser extensions are active.".to_owned())
        } else {
            browser_policy::check_cdp(&method, &params, true)
        };
        if let Err(err) = checked {
            let _ = self.out_tx.send(EngineMsg::CdpResponse {
                tab_id,
                request_id,
                ok: false,
                result: json!({ "message": err }),
            });
            return;
        }
        if method == "Page.handleJavaScriptDialog"
            && (t.prompts.has_leave_dialog() || t.prompts.is_covered())
        {
            let _ = self.out_tx.send(EngineMsg::CdpResponse {
                tab_id,
                request_id,
                ok: false,
                result: json!({ "message": "The user is answering a prompt over this page. Wait for them, or use another tab." }),
            });
            return;
        }
        // Route raw `Input.*` calls through the same serialized `InputGuard` as `TypedCdpCall` so
        // no raw caller can bypass hit-test, frame-owner, focused-frame, or prompt-cover checks.
        if method.starts_with("Input.") {
            if session_id.is_some() {
                let _ = self.out_tx.send(EngineMsg::CdpResponse {
                    tab_id,
                    request_id,
                    ok: false,
                    result: json!({ "message": "Input DevTools calls must target the page root session." }),
                });
                return;
            }
            t.cdp_ledger.lock().unwrap().app_attached = true;
            if let Err(msg) = t
                .input_guard
                .lock()
                .unwrap()
                .enqueue(crate::input_guard::InputCall::single(request_id, method, params))
            {
                let _ = self.out_tx.send(EngineMsg::CdpResponse {
                    tab_id,
                    request_id,
                    ok: false,
                    result: json!({ "message": msg }),
                });
                return;
            }
            crate::input_guard::pump(
                tab_id,
                &t.input_guard,
                &t.browser,
                &t.host,
                t.is_popup,
                t.isolated,
                t.preview.is_some(),
                &t.snap,
                &t.prompts,
                &self.permissions,
                &t.cdp_ledger,
                &t.emulation_scale,
                &t.cdp_id_map,
                &self.next_cdp_id,
                &self.out_tx,
            );
            return;
        }
        // The engine enabled `Runtime` when it armed the tab: disable it first so Chromium
        // reports the existing contexts to this caller too.
        let disable = {
            let mut ledger = t.cdp_ledger.lock().unwrap();
            ledger.app_attached = true;
            (method == "Runtime.enable").then(|| ledger.runtime_disable(None))
        };
        if let Some(disable) = disable {
            t.host.send_dev_tools_message(Some(disable.as_bytes()));
            for binding in [
                Some(t.passkeys.binding_msg(None)),
                Some(t.media_gate.binding_msg(None)),
                Some(t.notification_gate.binding_msg(None)),
                t.clipboard_gate.binding_msg(None),
            ]
            .into_iter()
            .flatten()
            {
                t.host.send_dev_tools_message(Some(binding.as_bytes()));
            }
        }
        let msg_id = self.next_cdp_id.fetch_add(1, Ordering::Relaxed);
        t.cdp_id_map.lock().unwrap().insert(
            msg_id,
            CdpPending::Raw {
                request_id,
                max_bytes: crate::cdp_ledger::max_cdp_response_bytes(&method),
                typed: None,
                reach,
                is_tracing_start: false,
            },
        );
        let payload = json!({
            "id": msg_id,
            "method": method,
            "params": params,
        });
        let raw = payload.to_string();
        if t.host.send_dev_tools_message(Some(raw.as_bytes())) != 1 {
            t.cdp_id_map.lock().unwrap().remove(&msg_id);
            let _ = self.out_tx.send(EngineMsg::CdpResponse {
                tab_id,
                request_id,
                ok: false,
                result: json!({ "error": "send_dev_tools_message rejected" }),
            });
        }
    }

    pub(super) fn handle_set_inspect(&self, tab_id: u64, enabled: bool) {
        let Some(t) = self.tabs.get(&tab_id).filter(|_| self.contexts.is_tab_ready(tab_id)) else {
            return;
        };
        if tab_is_extension_surface(t) {
            return;
        }
        let host = t.host.clone();
        if enabled {
            self.send_internal_cdp(&host, "DOM.enable", json!({}));
            self.send_internal_cdp(&host, "Overlay.enable", json!({}));
            self.send_internal_cdp(
                &host,
                "Overlay.setInspectMode",
                json!({
                    "mode": "searchForNode",
                    "highlightConfig": {
                        "showInfo": true,
                        "contentColor": { "r": 56, "g": 189, "b": 248, "a": 0.24 },
                        "paddingColor": { "r": 52, "g": 211, "b": 153, "a": 0.18 },
                        "borderColor": { "r": 251, "g": 191, "b": 36, "a": 0.90 }
                    }
                }),
            );
        } else {
            self.send_internal_cdp(
                &host,
                "Overlay.setInspectMode",
                json!({ "mode": "none", "highlightConfig": {} }),
            );
            self.send_internal_cdp(&host, "Overlay.hideHighlight", json!({}));
        }
        let _ = self.out_tx.send(EngineMsg::InspectState {
            tab_id,
            active: enabled,
            picked: None,
        });
    }

    pub(super) fn handle_highlight_node(&self, tab_id: u64, backend_node_id: Option<i64>) {
        let Some(t) = self.tabs.get(&tab_id).filter(|_| self.contexts.is_tab_ready(tab_id)) else {
            return;
        };
        if tab_is_extension_surface(t) {
            return;
        }
        let host = t.host.clone();
        match backend_node_id {
            Some(id) => {
                self.send_internal_cdp(&host, "DOM.enable", json!({}));
                self.send_internal_cdp(&host, "Overlay.enable", json!({}));
                self.send_internal_cdp(
                    &host,
                    "Overlay.highlightNode",
                    json!({
                        "backendNodeId": id,
                        "highlightConfig": {
                            "showInfo": false,
                            "contentColor": { "r": 56, "g": 189, "b": 248, "a": 0.25 },
                            "borderColor": { "r": 56, "g": 189, "b": 248, "a": 0.9 }
                        }
                    }),
                );
            }
            None => {
                self.send_internal_cdp(&host, "Overlay.hideHighlight", json!({}));
            }
        }
    }

    pub(super) fn handle_find(
        &self,
        tab_id: u64,
        query: &str,
        forward: bool,
        match_case: bool,
        find_next: bool,
        user_initiated: bool,
    ) {
        let Some(t) = self.tabs.get(&tab_id) else {
            return;
        };
        if !self.contexts.is_tab_ready(tab_id) || (!user_initiated && tab_is_extension_surface(t)) {
            t.host.stop_finding(1);
            return;
        }
        if query.is_empty() {
            t.host.stop_finding(1);
        } else {
            t.host.find(
                Some(&CefString::from(query)),
                i32::from(forward),
                i32::from(match_case),
                i32::from(find_next),
            );
        }
    }

    /// Guard 1a (deny-by-default single dispatch choke point): exhaustively classifies every
    /// `HostMsg` with no `_ =>` catch-all. Non-exempt tab-addressed commands are checked against
    /// the tab's committed origin only (never destination, so navigating away always succeeds).
    pub(super) fn refuse_if_controlled_tab_blocked(&self, msg: &crate::wire::HostMsg) -> bool {
        use crate::wire::HostMsg as M;
        let (tab_id, req_id, is_find, is_menu) = match msg {
            // Engine-global / non-tab-addressed messages.
            M::Hello { .. }
            | M::ClickExtension { .. }
            | M::ListExtensions { .. }
            | M::UninstallExtension { .. }
            | M::ReloadExtension { .. }
            | M::CancelExtensionOpenTab { .. }
            | M::ExtensionMetrics { .. }
            | M::SetColorScheme { .. }
            | M::SetFrameRate { .. }
            | M::SetVisibleTabs { .. }
            | M::SetPartitionProxy { .. }
            | M::DropContext { .. }
            | M::DmabufFailed { .. }
            | M::ResetPermission { .. }
            | M::SyncSitePermissions { .. }
            | M::PreviewResponseHead { .. }
            | M::PreviewResponseChunk { .. }
            | M::PreviewResponseEnd { .. }
            // Profile cookie store messages from Settings > Browser (`#286`): not tab-addressed.
            | M::ProfileCookiesList { .. }
            | M::ProfileCookiesSet { .. }
            | M::ProfileCookieDelete { .. }
            | M::ProfileProbeCookies { .. }
            | M::ClearProfileData { .. }
            | M::SetShareSignIns { .. }
            | M::SyncRemoteCookies { .. }
            | M::Shutdown => return false,
            // Exempt tab-addressed lifecycle, cleanup, layout/zoom, human input, prompt answers,
            // and `OpenTab`/`Navigate`/`Back`/`Forward` (guarded by destination pre-check +
            // `refuse_history_nav_if_inherited` + Guard 2 commit check so a controlled tab already
            // on a granted origin can leave to any ungranted site via Navigate, Back, or Forward).
            M::OpenTab { .. }
            | M::Navigate { .. }
            | M::Back { .. }
            | M::Forward { .. }
            | M::CloseTab { .. }
            | M::FocusTab { .. }
            | M::SetTabUnthrottled { .. }
            | M::Stop { .. }
            | M::StopFind { .. }
            | M::SetTabPolicy { .. }
            | M::SetAgentDriven { .. }
            | M::SetZoom { .. }
            | M::Resize { .. }
            | M::Emulate { .. }
            | M::MouseMove { .. }
            | M::MouseButton { .. }
            | M::Wheel { .. }
            | M::Key { .. }
            | M::EditCmd { .. }
            | M::Ime { .. }
            | M::FrameRead { .. }
            | M::AnswerDialog { .. }
            | M::AnswerPermission { .. }
            | M::AnswerAuth { .. }
            | M::AnswerDownloadSave { .. }
            | M::SetInspect { enabled: false, .. }
            | M::HighlightNode { backend_node_id: None, .. }
            | M::ContextMenuAction { action: None, .. }
            | M::ExtensionNotificationEvent { .. }
            | M::SetNotificationsEnabled { .. } => return false,
            // `user_initiated` is set only by the bundled find bar (`#312`), whose `FindResult`
            // never reaches agents or third-party plugins.
            M::Find { query, user_initiated, .. } if query.is_empty() || *user_initiated => return false,
            // Guarded tab-addressed commands (default for any new command added to `HostMsg`).
            M::CdpCall { tab_id, request_id, .. }
            | M::TypedCdpCall { tab_id, request_id, .. }
            | M::UploadFiles { tab_id, request_id, .. }
            | M::DropFiles { tab_id, request_id, .. }
            | M::SiteCookies { tab_id, request_id }
            | M::ClearSite { tab_id, request_id }
            | M::ClearSiteCache { tab_id, request_id } => (*tab_id, Some(*request_id), false, false),
            M::Reload { tab_id, .. } => (*tab_id, None, false, false),
            M::Find { tab_id, .. } => (*tab_id, None, true, false),
            M::SetInspect { tab_id, enabled: true }
            | M::HighlightNode { tab_id, backend_node_id: Some(_) } => (*tab_id, None, false, false),
            M::ContextMenuAction { tab_id, action: Some(_), .. } => (*tab_id, None, false, true),
        };
        let Some(t) = self.tabs.get(&tab_id) else {
            return false;
        };
        let Some(blocked_url) = self.permissions.tab_blocked_url(
            Some(&t.browser),
            t.isolated,
            t.preview.is_some(),
            &t.snap,
            &t.prompts,
        ) else {
            return false;
        };
        if is_find {
            t.host.stop_finding(1);
        }
        if is_menu {
            t.menu.cancel_all();
        }
        let _ = self.out_tx.send(EngineMsg::ControlledPermissionRefused {
            tab_id,
            url: blocked_url,
        });
        if let Some(request_id) = req_id {
            let _ = self.out_tx.send(EngineMsg::CdpResponse {
                tab_id,
                request_id,
                ok: false,
                result: json!({ "message": crate::permissions::INHERITED_PERMISSION_MSG }),
            });
        }
        true
    }

    pub(super) fn refuse_history_nav_if_inherited(&self, tab_id: u64, t: &ManagedTab, delta: i32) -> bool {
        if t.isolated || t.preview.is_some() || !t.prompts.agent_driven.load(Ordering::Acquire) {
            return false;
        }
        let can_nav = if delta < 0 {
            t.browser.can_go_back() != 0
        } else {
            t.browser.can_go_forward() != 0
        };
        if !can_nav {
            return false;
        }
        let Some(target) = crate::permissions::history_entry_url(&t.host, delta) else {
            // Fail closed if Chromium reports a history entry whose URL could not be inspected.
            let _ = self.out_tx.send(EngineMsg::ControlledPermissionRefused {
                tab_id,
                url: None,
            });
            return true;
        };
        if !self.permissions.has_inherited_default_grant(&target) {
            return false;
        }
        let _ = self.out_tx.send(EngineMsg::ControlledPermissionRefused {
            tab_id,
            url: Some(target),
        });
        true
    }

    /// Evicts Chromium's BFCache when at least one controlled `Default` tab exists alongside an
    /// inherited grant so neither `Back`/`Forward` nor JS `history.back()` can restore a cached
    /// granted document without passing `on_before_browse`. Zero-cost when no controlled `Default`
    /// tab or no inherited grant exists.
    pub(super) fn flush_bfcache_for_default_tabs(&self) {
        if !self.permissions.has_any_default_inherited_grant() {
            return;
        }
        if let Some(t) = self.tabs.values().find(|t| {
            !t.isolated && t.preview.is_none() && t.prompts.agent_driven.load(Ordering::Acquire)
        }) {
            self.send_internal_cdp(
                &t.host,
                "Memory.simulatePressureNotification",
                json!({ "level": "critical" }),
            );
        }
    }

    pub(super) fn handle_set_agent_driven(&mut self, tab_id: u64, agent_driven: bool) {
        if let Some(t) = self.tabs.get(&tab_id) {
            let was = t.prompts.agent_driven.swap(agent_driven, Ordering::AcqRel);
            t.prompts.user_reclaimed.store(false, Ordering::Release);
            t.snap.lock().unwrap().agent_driven = agent_driven;
            sync_file_choosers(t);
            sync_clipboard_gate(t);
            if agent_driven {
                // The person's unused media-gate Allow is theirs: the agent never inherits it.
                t.prompts.clear_media_gate_allows();
                // Agent-driven tabs permanently refuse WebAuthn/passkeys (`chromium.ts:834-845`).
                for raw in t.passkeys.enforce_messages() {
                    t.host.send_dev_tools_message(Some(raw.as_bytes()));
                }
                // Agent-driven tabs may never navigate to `chrome-extension://` pages.
                t.allowed_ext_ids.lock().unwrap().clear();
                if !was && !t.isolated && t.preview.is_none() {
                    self.flush_bfcache_for_default_tabs();
                }
            }
            if let Some(ext) = &mut self.extensions {
                ext.set_tab_agent_driven(tab_id, agent_driven);
            }
        }
    }

    /// An agent command first makes its tab refuse passkeys in the browser process, and runs only
    /// once Chromium answered `WebAuthn.enable` (official `afterRefusal`, `webauthn-gate.ts:121-127`,
    /// `chromium.ts:1552-1554`): held until then, refused if it failed. `None` when held or refused.
    pub(super) fn wait_for_passkey_refusal(&mut self, msg: HostMsg) -> Option<HostMsg> {
        let (tab_id, agent_only) = match &msg {
            HostMsg::CdpCall { tab_id, .. } => (*tab_id, true),
            HostMsg::TypedCdpCall { tab_id, .. } | HostMsg::UploadFiles { tab_id, .. } | HostMsg::DropFiles { tab_id, .. } => {
                (*tab_id, false)
            }
            _ => return Some(msg),
        };
        let Some(t) = self.tabs.get(&tab_id) else {
            return Some(msg);
        };
        if agent_only && !t.prompts.agent_driven.load(Ordering::Acquire) {
            return Some(msg);
        }
        for raw in t.passkeys.enforce_messages() {
            t.host.send_dev_tools_message(Some(raw.as_bytes()));
        }
        // A command after held ones waits behind them, so the tab's commands keep their order.
        if self.passkey_held.iter().any(|held| held_tab(held) == Some(tab_id)) {
            self.passkey_held.push(msg);
            return None;
        }
        match t.passkeys.agent_gate() {
            AgentGate::Go => Some(msg),
            AgentGate::Wait => {
                self.passkey_held.push(msg);
                None
            }
            AgentGate::Refuse => {
                self.refuse_passkeys_unrestricted(&msg);
                None
            }
        }
    }

    /// After CEF's work: stops each Windows tab whose `WebAuthn.enable` failed (official
    /// `end({ cause: "passkeys-unrestricted" })`, `chromium.ts:839`), then runs or refuses the
    /// agent commands whose tab has its answer.
    pub fn release_passkey_held(&mut self) {
        for t in self.tabs.values() {
            if t.passkeys.take_end() {
                t.browser.stop_load();
                if let Some(f) = t.browser.main_frame() {
                    f.load_url(Some(&CefString::from("about:blank")));
                }
            }
        }
        if self.passkey_held.is_empty() {
            return;
        }
        let mut waiting: Vec<u64> = Vec::new();
        for msg in std::mem::take(&mut self.passkey_held) {
            let tab_id = held_tab(&msg).unwrap_or_default();
            let gate = self.tabs.get(&tab_id).map(|t| t.passkeys.agent_gate());
            if waiting.contains(&tab_id) || gate == Some(AgentGate::Wait) {
                waiting.push(tab_id);
                self.passkey_held.push(msg);
                continue;
            }
            if gate == Some(AgentGate::Refuse) {
                self.refuse_passkeys_unrestricted(&msg);
                continue;
            }
            if !self.refuse_if_controlled_tab_blocked(&msg) {
                self.handle_msg(msg);
            }
        }
    }

    /// The refusal for an agent file upload or drop on a controlled tab where any frame's origin
    /// holds an inherited grant: the input or drop target can be in any frame (READER-9 #411 C3).
    pub(super) fn whole_tab_granted(&self, t: &ManagedTab) -> Option<&'static str> {
        let reach = crate::frame_authority::Reach::WholeTab;
        (crate::cdp_ledger::grant_gated(t) && crate::cdp_ledger::granted_origin(t, &self.permissions, &reach).is_some())
            .then_some(crate::permissions::INHERITED_PERMISSION_MSG)
    }

    fn refuse_passkeys_unrestricted(&self, msg: &HostMsg) {
        let (HostMsg::CdpCall { tab_id, request_id, .. }
        | HostMsg::TypedCdpCall { tab_id, request_id, .. }
        | HostMsg::UploadFiles { tab_id, request_id, .. }
        | HostMsg::DropFiles { tab_id, request_id, .. }) = msg
        else {
            return;
        };
        let _ = self.out_tx.send(EngineMsg::CdpResponse {
            tab_id: *tab_id,
            request_id: *request_id,
            ok: false,
            result: json!({ "message": crate::passkeys::PASSKEYS_UNRESTRICTED }),
        });
    }

    /// The person's own click or key took the tab back (`user_reclaimed` false -> true), or the
    /// agent acted again (true -> false): the renderer's file-chooser intercept follows.
    pub(super) fn set_user_reclaimed(&self, tab_id: u64, reclaimed: bool) {
        let Some(t) = self.tabs.get(&tab_id) else { return };
        let was = t.prompts.user_reclaimed.swap(reclaimed, Ordering::AcqRel);
        if was != reclaimed && t.prompts.agent_driven.load(Ordering::Acquire) {
            sync_file_choosers(t);
        }
    }

    pub(super) fn expect_ext_tab_target(
        &mut self,
        isolated: bool,
        is_preview: bool,
        is_popup: bool,
        allowed_ext_url: Option<&str>,
    ) {
        let Some(ext) = &mut self.extensions else {
            return;
        };
        if isolated || is_preview {
            ext.expect_isolated_tab();
        } else if let Some(ext_id) = allowed_ext_url {
            if is_popup {
                ext.expect_popup(ext_id);
            } else {
                ext.expect_extension_page(ext_id);
            }
        } else {
            ext.expect_web_tab();
        }
    }

    pub(super) fn notify_ext_tab_opened(
        &mut self,
        tab_id: u64,
        initial_url: &str,
        width_pt: f32,
        height_pt: f32,
        focused: bool,
        ext_open_request_id: Option<u64>,
        agent_driven: bool,
    ) {
        let effects = self
            .extensions
            .as_mut()
            .map(|ext| {
                ext.on_tab_opened(
                    tab_id,
                    initial_url,
                    width_pt.round() as i32,
                    height_pt.round() as i32,
                    focused,
                    agent_driven,
                )
            })
            .unwrap_or_default();
        self.run_ext_effects(effects);
        if let Some(req_id) = ext_open_request_id
            && let Some((sid, rpc_id, ext_id)) = self.pending_ext_opens.remove(&req_id)
            && let Some(ext) = &mut self.extensions
        {
            ext.finish_tab_create(&sid, rpc_id, &ext_id, tab_id);
        }
    }
}

/// Fail-closed extension guard checked at dispatch AND reply time for `HostMsg::CdpCall`,
/// `HostMsg::Find`, `SetInspect`, and `HighlightNode` (`review-rd274.md` Round-4 & Round-5).
pub(crate) fn tab_is_extension_surface(t: &ManagedTab) -> bool {
    classify_surface(Some(&t.browser), t.is_popup, &t.snap, &t.prompts, &t.cdp_ledger) != Surface::Clean
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Surface {
    Clean,
    /// A web page with an extension frame in it: typed calls go on under the ledger's rules.
    Subframes,
    /// An extension page, a pop-up, or a tab that can't be inspected: everything is refused.
    WholeTab,
}

pub(crate) fn classify_surface(
    browser: Option<&cef::Browser>,
    is_popup: bool,
    snap: &Mutex<TabSnapshot>,
    prompts: &TabPrompts,
    ledger: &Mutex<crate::cdp_ledger::EngineCdpLedger>,
) -> Surface {
    let ext = crate::extensions::manifest::is_extension_scheme;
    if is_popup || ext(&snap.lock().unwrap().url) || prompts.pending_top_url().is_some_and(|p| ext(&p)) {
        return Surface::WholeTab;
    }
    let Some(browser) = browser else {
        return Surface::WholeTab;
    };
    if browser.main_frame().is_some_and(|f| ext(&CefString::from(&f.url()).to_string())) {
        return Surface::WholeTab;
    }
    if ledger.lock().unwrap().has_excluded() {
        return Surface::Subframes;
    }
    let mut ids = cef::CefStringList::new();
    browser.frame_identifiers(Some(&mut ids));
    let mut live_frames: HashMap<String, String> = HashMap::new();
    for id in ids {
        let Some(frame) = browser.frame_by_identifier(Some(&CefString::from(id.as_str()))) else {
            continue;
        };
        let furl = CefString::from(&frame.url()).to_string();
        if ext(&furl) {
            return Surface::Subframes;
        }
        live_frames.insert(id, furl);
    }
    if prompts.has_pending_ext_subframes(&live_frames) { Surface::Subframes } else { Surface::Clean }
}

/// Sends `Page.setInterceptFileChooserDialog` (official `file-choosers.ts:19-27`) to the tab's root
/// and every attached web OOPIF session: on while the agent owns the tab's dialogs or the tab is a
/// preview, off once the person has taken it back.
pub(super) fn sync_file_choosers(t: &ManagedTab) {
    let enabled = t.preview.is_some() || t.prompts.agent_owns_dialogs();
    let root = t.passkeys.file_chooser_intercept_msg(enabled, None);
    t.host.send_dev_tools_message(Some(root.as_bytes()));
    let child_sids = t.cdp_ledger.lock().unwrap().web_session_ids();
    for sid in child_sids {
        let raw = t.passkeys.file_chooser_intercept_msg(enabled, Some(&sid));
        t.host.send_dev_tools_message(Some(raw.as_bytes()));
    }
}

/// Keeps `TabClipboardGate` aligned with `is_preview || agent_driven` across all live
/// execution contexts and sessions (official `site-permissions.ts:152-157`, `policy.ts:132-135`,
/// READER-7 N5). Deliberately ignores `user_reclaimed` so human takeover never re-enables page
/// clipboard writes on an agent-driven tab.
pub(super) fn sync_clipboard_gate(t: &ManagedTab) {
    let blocked = t.preview.is_some() || t.prompts.agent_driven.load(Ordering::Acquire);
    if blocked {
        t.prompts.print_gated.store(true, Ordering::Release);
    }
    for raw in t.clipboard_gate.set_blocked_messages(blocked) {
        t.host.send_dev_tools_message(Some(raw.as_bytes()));
    }
}
fn held_tab(msg: &HostMsg) -> Option<u64> {
    match msg {
        HostMsg::CdpCall { tab_id, .. }
        | HostMsg::TypedCdpCall { tab_id, .. }
        | HostMsg::UploadFiles { tab_id, .. }
        | HostMsg::DropFiles { tab_id, .. } => Some(*tab_id),
        _ => None,
    }
}