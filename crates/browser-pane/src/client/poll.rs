//! Sidecar inbox polling and `EngineMsg` dispatch for [`BrowserHost`].

use std::sync::PoisonError;

use crate::installer::EngineStatus;
use crate::machine::{EngineCaps, EngineEffect, EngineEvent};
use crate::protocol::{EngineMsg, HostMsg};

use super::{BrowserHost, CdpEvent, MAX_QUEUED_CDP_EVENTS, MAX_TAB_FILES};

impl BrowserHost {
    /// Processes incoming messages from the sidecar; returns `true` only when visible UI metadata changed.
    /// Whether poll would find nothing to do: no message, no download to connect for, the
    /// status as last polled. Reads only, so a caller that polls every frame writes nothing.
    pub fn quiet(&self) -> bool {
        if let Ok(msg) = self.inbox_rx.try_recv() {
            self.held.borrow_mut().push_back(msg);
        }
        #[cfg(feature = "engine")]
        let needs_fresh = self.shared.lock().unwrap_or_else(PoisonError::into_inner).needs_fresh_spawn();
        #[cfg(not(feature = "engine"))]
        let needs_fresh = false;
        self.held.borrow().is_empty()
            && self.pending_opens.is_empty()
            && !needs_fresh
            && self.status() == self.last_polled_status
            && self
                .extensions
                .filter_gate
                .deadline()
                .is_none_or(|d| rune::time::Instant::now() < d)
    }

    pub fn poll(&mut self) -> bool {
        let (flushed, _) = self.tick_extensions(rune::time::Instant::now());
        let mut changed = flushed;
        self.flush_pending_opens();
        let mut mail = std::mem::take(&mut *self.held.borrow_mut());
        mail.extend(std::iter::from_fn(|| self.inbox_rx.try_recv().ok()));
        for msg in mail {
            match msg {
                EngineMsg::Ready {
                    downloads,
                    policy,
                    prompts,
                    popups,
                    find,
                    context_menu,
                    partitions,
                    remote_proxy,
                    agent_tools,
                    preview,
                    extensions,
                    permission_sync,
                    frame_authority,
                    cookies,
                    agent_files,
                    passkeys,
                    trace_isolation,
                    menu_extras,
                    tab_lifecycle,
                    ..
                } => {
                    if !matches!(
                        self.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::Ready),
                        EngineEffect::ReadyApplied
                    ) {
                        continue;
                    }
                    self.caps = EngineCaps::Known {
                        downloads, policy, prompts, popups, find, context_menu,
                        partitions, remote_proxy, agent_tools, preview, extensions,
                        permission_sync, frame_authority, cookies, agent_files, passkeys, trace_isolation,
                        menu_extras, tab_lifecycle,
                    };
                    self.sync_site_permissions();
                    if cookies && self.share_sign_ins {
                        self.send(HostMsg::SetShareSignIns { enabled: true });
                    }
                    self.on_extensions_ready(extensions);
                    // A new engine starts with an empty downloads folder and numbers its files
                    // from 1 again: the last one's files are gone. Held prompts replay if live.
                    self.download_save_requests.clear();
                    for t in &mut self.tabs {
                        t.files.clear();
                        t.prompts.reset_for_engine();
                        t.scroll_cursor = Default::default();
                        t.find.step(crate::find::FindEvent::DocumentChanged);
                    }
                    rune::marks::mark("browser.engine.ready");
                    // Before any page loads, so the first draws in the app's scheme.
                    match self.color_scheme.load(std::sync::atomic::Ordering::Relaxed) {
                        0 => {}
                        s => self.send(HostMsg::SetColorScheme { dark: s == 2 }),
                    }
                    let fps = self.display_fps.load(std::sync::atomic::Ordering::Relaxed);
                    if fps != 0 {
                        self.send(HostMsg::SetFrameRate { fps });
                    }

                    // Tabs opened before it: their size and device emulation again
                    // (a reconnect forgets them; `notes/for-dax.md` §0).
                    let tabs_to_sync: Vec<_> = self
                        .tabs
                        .iter_mut()
                        .filter(|t| !self.pending_opens.iter().any(|(id, _)| *id == t.id))
                        .map(|t| {
                            t.sent_resize = None;
                            let emu = t.device.as_ref().map(|d| (d.clone(), d.fit([t.viewport_pt.0, t.viewport_pt.1])));
                            (t.id, emu)
                        })
                        .collect();
                    for (tab_id, emulate) in tabs_to_sync {
                        if let Some((device, scale)) = emulate {
                            self.send(HostMsg::Emulate { tab_id, device: Some(device), scale });
                        }
                        if self.tabs.iter().any(|t| t.id == tab_id && t.prompts.agent_driven) {
                            self.send(HostMsg::SetAgentDriven { tab_id, agent_driven: true });
                        }
                    }
                    if partitions && remote_proxy {
                        let proxies: Vec<_> = self
                            .remote_proxies
                            .iter()
                            .map(|(id, cfg)| (id.clone(), cfg.clone()))
                            .collect();
                        for (server_id, cfg) in proxies {
                            self.send(HostMsg::SetPartitionProxy {
                                server_id,
                                proxy: Some(cfg),
                            });
                        }
                    }
                    self.flush_pending_opens();
                    if tab_lifecycle {
                        self.send(HostMsg::SetVisibleTabs { tab_ids: self.visible_tabs.clone() });
                        let active_holds: Vec<u64> = self
                            .unthrottled_tabs
                            .iter()
                            .copied()
                            .filter(|id| self.tabs.iter().any(|t| t.id == *id))
                            .collect();
                        for tab_id in active_holds {
                            self.send(HostMsg::SetTabUnthrottled { tab_id, active: true });
                        }
                    }
                    changed = true;
                }
                EngineMsg::Refused { tab_id, reason } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        t.loading = false;
                        if !t.engine_confirmed
                            && matches!(
                                reason.as_str(),
                                "browser.proxy.required"
                                    | "browser.proxy.invalid"
                                    | "browser.context.invalid"
                                    | "browser.context.unavailable"
                            )
                        {
                            t.has_engine_tab = false;
                        }
                        t.refusal = Some(reason);
                        t.rejected += 1;
                        changed = true;
                    }
                    self.sync_has_tabs();
                }
                EngineMsg::ControlledPermissionRefused { tab_id, url } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        t.loading = false;
                        t.refusal = Some("browser.controlled.inherited_permission".to_owned());
                        t.rejected += 1;
                        if t.prompts.agent_driven {
                            t.prompts.set_blocked_guard_notice(
                                crate::prompts::BlockedGuardReason::InheritedPermission { url },
                                rune::time::Instant::now(),
                            );
                        }
                        changed = true;
                    }
                }
                EngineMsg::TabList { tabs } => {
                    for snap in tabs {
                        self.upsert_tab(snap);
                    }
                    if self.focused_tab.is_none() {
                        self.focused_tab = self.tabs.last().map(|t| t.id);
                    }
                    changed = true;
                }
                EngineMsg::TabUpdate { tab } => {
                    changed |= self.upsert_tab(tab);
                }
                EngineMsg::TabClosed { tab_id } => {
                    if self.close_popup_if_tab(tab_id) {
                        changed = true;
                        continue;
                    }
                    if let Some(pending) = self.reopening_tabs.get_mut(&tab_id) {
                        if *pending <= 1 {
                            self.reopening_tabs.remove(&tab_id);
                        } else {
                            *pending -= 1;
                        }
                        continue;
                    }
                    self.pending_opens.retain(|(id, _)| *id != tab_id);
                    self.download_save_requests.retain(|r| r.tab_id != tab_id);
                    self.tab_favicons.remove(&tab_id);
                    self.visible_tabs.retain(|&x| x != tab_id);
                    self.unthrottled_tabs.remove(&tab_id);
                    let fatal_close = if let Some(pos) = self.tabs.iter().position(|t| t.id == tab_id) {
                        let removed = self.tabs.remove(pos);
                        removed.surface.clear();
                        self.surfaces.lock().unwrap_or_else(PoisonError::into_inner).remove(&tab_id);
                        self.extensions.filter_gate.cancel_tab(tab_id);
                        if self.focused_tab == Some(tab_id) {
                            self.focused_tab = self.tabs.last().map(|t| t.id);
                        }
                        changed = true;
                        matches!(
                            removed.refusal.as_deref(),
                            Some("browser.proxy.invalid" | "browser.context.unavailable")
                        )
                    } else {
                        false
                    };
                    self.sync_has_tabs();
                    self.closed_tabs.push_back((tab_id, fatal_close));
                }
                EngineMsg::InspectState { tab_id, active, picked } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        t.inspecting = active;
                    }
                    if let Some(el) = picked {
                        self.picked.push_back(el);
                    }
                    changed = true;
                }
                EngineMsg::CdpResponse { request_id, ok, result, .. } => {
                    if let Some(pos) = self.cancelled_cdp.iter().position(|&id| id == request_id) {
                        self.cancelled_cdp.remove(pos);
                        continue;
                    }
                    while self.cdp_responses.len() >= 512 {
                        let Some(oldest) = self.cdp_response_order.pop_front() else { break };
                        self.cdp_responses.remove(&oldest);
                    }
                    self.cdp_response_order.push_back(request_id);
                    self.cdp_responses
                        .insert(request_id, if ok { Ok(result) } else { Err(result) });
                    changed = true;
                }
                EngineMsg::Download { tab_id, mut file } => {
                    file.name = crate::policy::sanitize_download_name(&file.name);
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        // Never expose human-tab downloads in `BrowserTab.files` (defense in depth
                        // for older engines before `DownloadSaveRequest`).
                        if !t.prompts.agent_driven {
                            continue;
                        }
                        match t.files.iter().position(|f| f.id == file.id) {
                            Some(known) => t.files[known] = file,
                            // The engine keeps at most MAX_TAB_FILES a tab; the list holds those and
                            // one more place for the latest it refused, which the pane names.
                            None if t.files.len() <= MAX_TAB_FILES => t.files.push(file),
                            None => {
                                if let Some(last) = t.files.last_mut() {
                                    *last = file;
                                }
                            }
                        }
                        changed = true;
                    }
                }
                EngineMsg::DownloadSaveRequest { tab_id, id, name, mime, executable } => {
                    let known = self.tabs.iter().any(|t| t.id == tab_id);
                    if !known || self.download_save_requests.len() >= 16 {
                        self.send(HostMsg::AnswerDownloadSave { tab_id, id, path: None });
                    } else {
                        let clean_mime = crate::policy::sanitize_download_mime(&mime);
                        let dangerous = executable || crate::policy::is_dangerous_download(&name, &clean_mime);
                        self.download_save_requests.push_back(crate::protocol::DownloadSaveRequest {
                            tab_id,
                            id,
                            name: crate::policy::sanitize_download_name(&name),
                            mime: clean_mime,
                            executable: dangerous,
                        });
                        changed = true;
                    }
                }
                EngineMsg::Dialog { .. } | EngineMsg::DialogsReset { .. }
                | EngineMsg::PermissionRequest { .. } | EngineMsg::PermissionDismissed { .. }
                | EngineMsg::AuthRequest { .. } | EngineMsg::AuthReset { .. } | EngineMsg::AuthAgentRequired { .. }
                | EngineMsg::PopupOpen { .. } | EngineMsg::PopupBlocked { .. }
                | EngineMsg::AppLinkBlocked { .. } | EngineMsg::PasskeyRefused { .. }
                | EngineMsg::FileChooserBlocked { .. } | EngineMsg::CopiedImage { .. }
                | EngineMsg::DownloadOnlyTab { .. } => {
                    changed |= self.handle_prompt_msg(msg);
                }
                EngineMsg::FindResult { tab_id, count, active_match, final_update } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        changed |= t.find.step(crate::find::FindEvent::Result {
                            active: active_match,
                            total: count,
                            final_update,
                        });
                    }
                }
                EngineMsg::ContextMenu { tab_id, menu } => { self.push_context_menu(tab_id, menu); changed = true; }
                EngineMsg::PageDiagnostic { tab_id, level, event, fields } => { self.push_diagnostic(tab_id, level, event, fields); changed = true; }
                EngineMsg::ProfileCookies { request_id, cookies } => {
                    self.profile_events.push_back(crate::cookies::ProfileEvent::CookiesListed {
                        request_id,
                        cookies,
                    });
                    changed = true;
                }
                EngineMsg::ProfileCookiesImported { request_id, imported, failed } => {
                    self.profile_events.push_back(crate::cookies::ProfileEvent::CookiesImported {
                        request_id,
                        imported,
                        failed,
                    });
                    changed = true;
                }
                EngineMsg::ProfileProbeReady { request_id, written, cache_dir } => {
                    self.profile_events.push_back(crate::cookies::ProfileEvent::ProbeReady {
                        request_id,
                        written,
                        cache_dir,
                    });
                    changed = true;
                }
                EngineMsg::ProfileActionDone { request_id, ok } => {
                    self.profile_events.push_back(crate::cookies::ProfileEvent::ActionDone {
                        request_id,
                        ok,
                    });
                    changed = true;
                }
                EngineMsg::PreviewFetch { req_id, tab_id, kind, path, method, range } => {
                    self.preview_fetches.push(crate::preview::PreviewFetchRequest { req_id, tab_id, kind, path, method, range });
                    changed = true;
                }
                EngineMsg::PreviewCredit { req_id, bytes, cancel } => {
                    self.preview_chunk_budget.on_credit(req_id, bytes as usize, cancel);
                }
                EngineMsg::PreviewDenied { tab_id } => {
                    if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                        t.refusal = Some("browser.preview.restricted".into());
                        t.rejected = t.rejected.saturating_add(1);
                        changed = true;
                    }
                }
                EngineMsg::CdpEvent { tab_id, method, params, session_id } => {
                    if self.cdp_events.len() >= MAX_QUEUED_CDP_EVENTS {
                        self.cdp_events.pop_front();
                    }
                    self.cdp_events.push_back(CdpEvent { tab_id, method, params, session_id });
                    changed = true;
                }
                // The socket reader delivers frames directly to the tab's retained Rune surface.
                EngineMsg::SharedFrame { .. } | EngineMsg::CpuFrame { .. } | EngineMsg::DmabufPool { .. } | EngineMsg::DmabufFrame { .. } => {}
                other => {
                    changed |= self.handle_extension_msg(other);
                }
            }
        }
        #[cfg(feature = "engine")]
        self.check_for_update_once();
        #[cfg(feature = "engine")]
        if self.shared.lock().unwrap_or_else(PoisonError::into_inner).needs_fresh_spawn() {
            if self.has_engine_tabs() {
                self.requeue_tabs();
                self.connect_fresh();
            } else {
                self.shared.lock().unwrap_or_else(PoisonError::into_inner).step(EngineEvent::Shutdown);
            }
        }
        // An idle engine exited right as a new tab opened (before it could confirm it), or a
        // download finished while a tab waited for it: requeue unconfirmed tabs and reconnect.
        if !self.is_connected() && self.status() == EngineStatus::Installed && self.has_engine_tabs() {
            self.requeue_tabs();
            self.ensure_connected();
        } else if !self.pending_opens.is_empty()
            && !self.is_connected()
            && matches!(
                self.status(),
                EngineStatus::Installed
                    | EngineStatus::Ready
                    | EngineStatus::Downloading { .. }
                    | EngineStatus::Verifying
                    | EngineStatus::Paused(_)
            )
        {
            self.ensure_connected();
        }
        {
            let status = self.status();
            if status != self.last_polled_status {
                if !self.is_connected() { self.clear_prompts_on_disconnect(); }
                self.last_polled_status = status;
                changed = true;
            }
        }
        changed
    }

    /// Whether the connected engine supports `SetVisibleTabs` and `SetTabUnthrottled`.
    pub fn engine_tab_lifecycle(&self) -> bool {
        self.caps.tab_lifecycle()
    }

    /// Currently tracked visible browser tab ids (read-only fast path for callers behind `Tracked`).
    pub fn visible_tabs(&self) -> &[u64] {
        &self.visible_tabs
    }

    /// Drops Linux dmabuf buffers held by the rx thread while a tab hides (preserving tombstone).
    pub(crate) fn forget_dmabuf_pool(&self, tab_id: u64) {
        #[cfg(all(feature = "engine", target_os = "linux"))]
        {
            if let Some(pools) = self.dmabuf_pools.lock().unwrap_or_else(PoisonError::into_inner).clone() {
                pools.lock().unwrap_or_else(PoisonError::into_inner).forget(tab_id);
            }
        }
        #[cfg(not(all(feature = "engine", target_os = "linux")))]
        let _ = tab_id;
    }

    /// Drops Linux dmabuf buffers and tombstone when a tab closes permanently.
    pub(crate) fn close_dmabuf_pool(&self, tab_id: u64) {
        #[cfg(all(feature = "engine", target_os = "linux"))]
        {
            if let Some(pools) = self.dmabuf_pools.lock().unwrap_or_else(PoisonError::into_inner).clone() {
                pools.lock().unwrap_or_else(PoisonError::into_inner).close(tab_id);
            }
        }
        #[cfg(not(all(feature = "engine", target_os = "linux")))]
        let _ = tab_id;
    }

    /// Replays `SetTabUnthrottled` and `SetVisibleTabs` after a `CloseTab`+`OpenTab` context switch
    /// (`reopen_tab_as_web` / `reopen_tab_as_preview`) so visibility and RPC holds survive reopen.
    pub(crate) fn replay_tab_lifecycle(&self, tab_id: u64) {
        if !self.is_connected() || !self.caps.tab_lifecycle() {
            return;
        }
        if self.unthrottled_tabs.contains(&tab_id) {
            self.send(HostMsg::SetTabUnthrottled { tab_id, active: true });
        }
        if self.visible_tabs.contains(&tab_id) {
            self.send(HostMsg::SetVisibleTabs { tab_ids: self.visible_tabs.clone() });
        }
    }

    /// Updates the exact set of visible browser pane tabs (deduplicated; no-op if unchanged).
    /// Clears desktop-side retained surfaces (and Linux dmabuf pools) only when the engine
    /// supports `Ready.tab_lifecycle` and the hiding tab is not held awake by an RPC hold.
    pub fn set_visible_tabs(&mut self, ids: &[u64]) {
        let mut next = Vec::with_capacity(ids.len());
        for &id in ids {
            if !next.contains(&id) {
                next.push(id);
            }
        }
        if self.visible_tabs == next {
            return;
        }
        let can_release = self.caps.tab_lifecycle();
        for &old_id in &self.visible_tabs {
            if !next.contains(&old_id) && can_release && !self.unthrottled_tabs.contains(&old_id) {
                if let Some(t) = self.tabs.iter().find(|t| t.id == old_id) {
                    t.surface.clear();
                }
                self.forget_dmabuf_pool(old_id);
            }
        }
        self.visible_tabs = next;
        if !self.is_connected() {
            return;
        }
        if self.caps.tab_lifecycle() {
            self.send(HostMsg::SetVisibleTabs { tab_ids: self.visible_tabs.clone() });
        } else if let Some(&first) = self.visible_tabs.first()
            && self.focused_tab != Some(first)
        {
            self.focus_tab(first);
        }
    }

    /// Sets or clears the explicit agent RPC unthrottle hold on `tab_id` (deduplicated per tab,
    /// gated on `Ready.tab_lifecycle`). When a hidden tab releases its hold, clears its retained
    /// surface and Linux dmabuf pool as well.
    pub fn set_tab_unthrottled(&mut self, tab_id: u64, active: bool) {
        let changed = if active {
            self.unthrottled_tabs.insert(tab_id)
        } else {
            self.unthrottled_tabs.remove(&tab_id)
        };
        if !changed {
            return;
        }
        if !active && self.caps.tab_lifecycle() && !self.visible_tabs.contains(&tab_id) {
            if let Some(t) = self.tabs.iter().find(|t| t.id == tab_id) {
                t.surface.clear();
            }
            self.forget_dmabuf_pool(tab_id);
        }
        if self.is_connected() && self.caps.tab_lifecycle() {
            self.send(HostMsg::SetTabUnthrottled { tab_id, active });
        }
    }
}
