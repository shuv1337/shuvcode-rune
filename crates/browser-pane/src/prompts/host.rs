//! `BrowserHost` prompt, permission, HTTP auth, and pop-up coordination methods.

use rune::time::Instant;

use super::{
    AuthGate, BlockedGuardReason, PermissionDecision, PermissionScope, PermissionStore, PopupGate,
    PopupRequest, SitePermissions, SiteSetting, TabDriver,
};
use crate::protocol::{EngineMsg, HostMsg, SitePermissionEntry};

impl crate::BrowserHost {
    pub fn remember_site_permissions(&mut self, store: PermissionStore) {
        let changed = self.site_permissions.reload_profile(store);
        // Clear any revoked/changed visit cache on Default tabs so revocations take effect immediately.
        if !changed.is_empty() {
            for t in &mut self.tabs {
                if t.context.is_default() {
                    for pair in &changed {
                        t.prompts.visit_permissions.remove(pair);
                    }
                }
            }
        }
        self.sync_site_permissions();
    }

    pub fn engine_permission_sync(&self) -> bool {
        self.caps.permission_sync()
    }

    /// Syncs Chromium's `Default` `HostContentSettingsMap` to match Rune's persisted
    /// `rune.sitePermissions` (`Default` `"Always allow"` / `"Never allow"` decisions).
    pub(crate) fn sync_site_permissions(&mut self) {
        if !self.is_connected() || !self.caps.permission_sync() {
            return;
        }
        let _ = self.site_permissions.drain_pending_resets();
        let entries: Vec<SitePermissionEntry> = self
            .site_permissions
            .list()
            .iter()
            .map(|d| SitePermissionEntry {
                origin: d.origin.clone(),
                kind: d.permission,
                allow: d.setting == SiteSetting::Allow,
            })
            .collect();
        self.send(HostMsg::SyncSitePermissions { entries });
    }

    pub fn site_permissions(&self) -> &SitePermissions {
        &self.site_permissions
    }

    pub fn site_permissions_mut(&mut self) -> &mut SitePermissions {
        &mut self.site_permissions
    }

    pub fn engine_prompts(&self) -> bool {
        self.caps.prompts()
    }

    pub fn engine_popups(&self) -> bool {
        self.caps.popups()
    }

    pub fn set_agent_driven(&mut self, tab_id: u64, agent_driven: bool) {
        if agent_driven {
            self.set_tab_driver(tab_id, TabDriver::Agent);
            return;
        }
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
        if !t.prompts.agent_driven {
            return;
        }
        t.prompts.agent_driven = false;
        t.prompts.driver = None;
        self.send(HostMsg::SetAgentDriven { tab_id, agent_driven: false });
    }

    /// Marks `tab_id` controlled by `driver` (`Agent` or third-party `Plugin(name)`), notifying
    /// the engine if `agent_driven` was not yet enabled. `Agent` takes precedence over `Plugin`.
    pub fn set_tab_driver(&mut self, tab_id: u64, driver: TabDriver) {
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
        t.allow_extension_pages = false;
        if t.prompts.driver != Some(TabDriver::Agent) {
            t.prompts.driver = Some(driver);
        }
        if !t.prompts.agent_driven {
            t.prompts.agent_driven = true;
            self.send(HostMsg::SetAgentDriven { tab_id, agent_driven: true });
        }
    }

    pub fn dismiss_blocked_guard_notice(&mut self, tab_id: u64) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            t.prompts.clear_blocked_guard_notice();
        }
    }

    pub fn rearm_tab_prompts(&mut self, tab_id: u64) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            t.prompts.rearm_visible(Instant::now());
        }
    }

    pub fn answer_dialog(
        &mut self,
        tab_id: u64,
        id: u64,
        accept: bool,
        text: String,
        stop: bool,
    ) -> bool {
        let now = Instant::now();
        let replies = self
            .tabs
            .iter_mut()
            .find(|t| t.id == tab_id)
            .map(|t| t.prompts.dialog.answer(id, accept, text, stop, now))
            .unwrap_or_default();
        if replies.is_empty() {
            return false;
        }
        for (reply_id, accept, text) in replies {
            self.send(HostMsg::AnswerDialog { tab_id, id: reply_id, accept, text });
        }
        true
    }

    pub fn answer_permission(
        &mut self,
        tab_id: u64,
        id: u64,
        decision: PermissionDecision,
    ) -> bool {
        let now = Instant::now();
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return false };
        let Some((item, ids)) = t.prompts.permission.take_active(id, now) else { return false };
        let scope = PermissionScope::from_context(&t.context);
        let origin = item.request.origin;
        let kinds = item.request.kinds;
        if decision == PermissionDecision::Dismiss {
            // "Not now" (`x`) or Escape keeps nothing in kept/session/visit decisions so the site
            // can ask again (`prompts.tsx:468`, `page-prompts.ts:498-506`).
            for req_id in ids {
                self.send(HostMsg::AnswerPermission {
                    tab_id,
                    id: req_id,
                    allow: false,
                    dismiss: true,
                    persist: false,
                });
            }
            return true;
        }
        let allow = matches!(decision, PermissionDecision::AllowOnce | PermissionDecision::AllowAlways);
        let trustworthy = super::trustworthy_origin(&origin)
            && !origin.to_ascii_lowercase().starts_with("oc-preview:");
        let can_keep_profile = !item.agent_driven
            && matches!(scope, PermissionScope::Profile)
            && t.preview.is_none()
            && trustworthy;
        let can_keep_session = !item.agent_driven
            && matches!(scope, PermissionScope::Session(_))
            && t.preview.is_none()
            && trustworthy;
        let can_keep = can_keep_profile || can_keep_session;
        let permanent_block = matches!(decision, PermissionDecision::BlockAlways) && can_keep_profile;
        // Agent-driven and preview tabs never persist permissions; remote-server decisions stay in
        // RAM until app quit (`PermissionScope::Session`); an allow on an agent-driven tab applies
        // only once (`page-prompts.ts:498-506`).
        if !(item.agent_driven && allow) {
            match decision {
                PermissionDecision::AllowAlways if can_keep => {
                    for kind in kinds {
                        self.site_permissions.set_scoped(&scope, origin.clone(), kind, SiteSetting::Allow);
                    }
                }
                PermissionDecision::BlockAlways if can_keep => {
                    for kind in kinds {
                        self.site_permissions.set_scoped(&scope, origin.clone(), kind, SiteSetting::Block);
                    }
                }
                PermissionDecision::AllowOnce | PermissionDecision::AllowAlways => {
                    for kind in kinds {
                        t.prompts.visit_permissions.insert((origin.clone(), kind), SiteSetting::Allow);
                    }
                }
                PermissionDecision::BlockOnce | PermissionDecision::BlockAlways => {
                    for kind in kinds {
                        t.prompts.visit_permissions.insert((origin.clone(), kind), SiteSetting::Block);
                    }
                }
                PermissionDecision::Dismiss => {}
            }
        }
        // Use `CEF_PERMISSION_RESULT_DISMISS` for non-permanent denies (`BlockOnce`, agent tabs,
        // visit-only and RAM-only session contexts) so Chromium does not permanently block on disk.
        let dismiss = !allow && !permanent_block;
        let persist = can_keep_profile
            && matches!(decision, PermissionDecision::AllowAlways | PermissionDecision::BlockAlways);
        for req_id in ids {
            self.send(HostMsg::AnswerPermission { tab_id, id: req_id, allow, dismiss, persist });
        }
        true
    }

    pub fn answer_auth(
        &mut self,
        tab_id: u64,
        id: u64,
        credentials: Option<(String, String)>,
    ) -> bool {
        let now = Instant::now();
        let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return false };
        let Some((item, ids)) = t.prompts.auth.take_active(id, now) else { return false };
        let (username, password) = match credentials {
            Some((u, p)) => (Some(u), Some(p)),
            None => {
                t.prompts.auth_dismissed.insert(item.key);
                (None, None)
            }
        };
        for challenge_id in ids {
            self.send(HostMsg::AnswerAuth {
                tab_id,
                id: challenge_id,
                username: username.clone(),
                password: password.clone(),
            });
        }
        true
    }

    pub fn dismiss_blocked_popup(&mut self, tab_id: u64) -> bool {
        self.tabs
            .iter_mut()
            .find(|t| t.id == tab_id)
            .is_some_and(|t| t.prompts.popup.dismiss())
    }

    pub fn take_blocked_popup_url(&mut self, tab_id: u64) -> Option<String> {
        let t = self.tabs.iter_mut().find(|t| t.id == tab_id)?;
        let PopupGate::Blocked { url, input, .. } = &t.prompts.popup else { return None };
        if !input.is_live() {
            return None;
        }
        let url = url.clone();
        t.prompts.popup = PopupGate::Idle;
        Some(url)
    }

    /// The person's Copy image results (PNG, base64), for the app's clipboard.
    pub fn drain_copied_images(&mut self) -> impl Iterator<Item = String> + '_ {
        self.copied_images.drain(..)
    }

    /// Tabs whose first navigation became the person's download and has its place.
    pub fn drain_download_only_tabs(&mut self) -> impl Iterator<Item = u64> + '_ {
        self.download_only_tabs.drain(..)
    }

    pub fn has_popup_opens(&self) -> bool {
        !self.popup_opens.is_empty()
    }

    pub fn drain_popup_opens(&mut self) -> impl Iterator<Item = PopupRequest> + '_ {
        self.popup_opens.drain(..)
    }

    pub fn has_app_links(&self) -> bool {
        !self.app_links.is_empty()
    }

    pub fn drain_app_links(&mut self) -> impl Iterator<Item = (u64, String)> + '_ {
        self.app_links.drain(..)
    }

    /// Arms guarded prompt buttons and expires timed-out agent permission prompts.
    /// Returns `(changed, next_wake_at)` so idle frames wake zero times.
    pub fn tick_prompts(&mut self, now: Instant) -> (bool, Option<Instant>) {
        let mut changed = false;
        let mut expired: Vec<(u64, u64)> = Vec::new();
        for t in &mut self.tabs {
            changed |= t.prompts.dialog.tick(now);
            let (perm_changed, timed_out) = t.prompts.permission.tick(now);
            changed |= perm_changed;
            for id in timed_out {
                expired.push((t.id, id));
            }
            changed |= t.prompts.auth.tick(now);
            changed |= t.prompts.popup.tick(now);
            if let Some(guard) = t.prompts.blocked_guard_input.as_mut() {
                changed |= guard.tick(now);
            }
        }
        for (tab_id, id) in expired {
            self.send(HostMsg::AnswerPermission { tab_id, id, allow: false, dismiss: true, persist: false });
        }
        for t in &mut self.tabs {
            if let crate::scroll_cursor::ScrollCursorEffect::Apply(cursor) =
                t.scroll_cursor.step(crate::scroll_cursor::ScrollCursorEvent::Tick { now })
                && t.cursor != cursor
            {
                t.cursor = cursor;
                changed = true;
            }
        }
        let prompt_next = self.tabs.iter().filter_map(|t| t.prompts.wake_at()).min();
        let cursor_next = self.tabs.iter().filter_map(|t| t.scroll_cursor.wake_at()).min();
        let next = prompt_next.into_iter().chain(cursor_next).min();
        (changed, next)
    }

    /// Clears open dialogs/permissions/auth/pop-ups across tabs on engine disconnect/crash so
    /// stale modals and typed auth credentials clear immediately without waiting for reconnect.
    pub(crate) fn clear_prompts_on_disconnect(&mut self) {
        for t in &mut self.tabs {
            t.prompts.reset_for_engine();
            t.scroll_cursor = Default::default();
        }
        self.context_menus.clear();
        self.popup_opens.clear();
    }

    pub(crate) fn push_context_menu(&mut self, tab_id: u64, menu: crate::protocol::PageContextMenu) {
        if self.context_menus.len() >= 16 {
            self.context_menus.pop_front();
        }
        self.context_menus.push_back((tab_id, menu));
    }

    pub(crate) fn push_diagnostic(&mut self, tab_id: u64, level: String, event: String, fields: serde_json::Value) {
        if self.diagnostics.len() >= 128 {
            self.diagnostics.pop_front();
        }
        self.diagnostics.push_back(crate::find::PageDiagnostic { tab_id, level, event, fields });
    }

    pub fn take_agent_passkey_refused(&mut self, tab_id: u64) -> Option<String> {
        self.tabs
            .iter_mut()
            .find(|t| t.id == tab_id)
            .and_then(|t| t.prompts.agent_passkey_refused.take())
    }

    pub fn take_agent_file_chooser_blocked(&mut self, tab_id: u64) -> bool {
        self.tabs
            .iter_mut()
            .find(|t| t.id == tab_id)
            .is_some_and(|t| std::mem::take(&mut t.prompts.agent_file_chooser_blocked))
    }

    pub(crate) fn handle_prompt_msg(&mut self, msg: EngineMsg) -> bool {
        let now = Instant::now();
        match msg {
            EngineMsg::Dialog { tab_id, dialog } => {
                let auto = self
                    .tabs
                    .iter_mut()
                    .find(|t| t.id == tab_id)
                    .and_then(|t| t.prompts.dialog.opened(dialog, now));
                if let Some((id, accept, text)) = auto {
                    self.send(HostMsg::AnswerDialog { tab_id, id, accept, text });
                    false
                } else {
                    true
                }
            }
            EngineMsg::DialogsReset { tab_id } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    let had = t.prompts.dialog.open().is_some();
                    t.prompts.dialog.reset();
                    had
                } else {
                    false
                }
            }
            EngineMsg::PermissionRequest { tab_id, request } => {
                let id = request.id;
                let auto = if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    if t.preview.is_some() {
                        Some((false, true, false))
                    } else {
                        let agent = t.prompts.agent_driven;
                        let scope = PermissionScope::from_context(&t.context);
                        t.prompts.permission.evaluate(
                            request,
                            agent,
                            &scope,
                            &self.site_permissions,
                            &t.prompts.visit_permissions,
                            now,
                        )
                    }
                } else {
                    Some((false, true, false))
                };
                if let Some((allow, dismiss, persist)) = auto {
                    self.send(HostMsg::AnswerPermission { tab_id, id, allow, dismiss, persist });
                    false
                } else {
                    true
                }
            }
            EngineMsg::PermissionDismissed { tab_id, id } => self
                .tabs
                .iter_mut()
                .find(|t| t.id == tab_id)
                .is_some_and(|t| t.prompts.permission.dismiss_by_id(id, now)),
            EngineMsg::AuthRequest { tab_id, challenge } => {
                let (cancel_id, ui_changed) = if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    if t.prompts.agent_driven {
                        // Defense in depth against older engines where `OpenTab` doesn't carry
                        // `agent_driven` yet and a fast 401 on `TID_IO` races `SetAgentDriven`.
                        t.prompts.agent_auth_required = Some(challenge.origin);
                        t.prompts.set_blocked_guard_notice(BlockedGuardReason::Action, now);
                        (Some(challenge.id), true)
                    } else {
                        let cancel = t.prompts.auth_challenge(challenge, now);
                        (cancel, cancel.is_none())
                    }
                } else {
                    (Some(challenge.id), false)
                };
                if let Some(id) = cancel_id {
                    self.send(HostMsg::AnswerAuth { tab_id, id, username: None, password: None });
                }
                ui_changed
            }
            EngineMsg::AuthReset { tab_id } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    let had = t.prompts.auth.active().is_some();
                    t.prompts.auth = AuthGate::Idle;
                    had
                } else {
                    false
                }
            }
            EngineMsg::AuthAgentRequired { tab_id, origin } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    t.prompts.agent_auth_required = Some(origin);
                    t.prompts.set_blocked_guard_notice(BlockedGuardReason::Action, now);
                    true
                } else {
                    false
                }
            }
            EngineMsg::PopupOpen { tab_id, url, background } => {
                if self.popup_opens.len() < 32 {
                    self.popup_opens.push_back(PopupRequest { opener_tab_id: tab_id, url, background });
                }
                true
            }
            EngineMsg::PopupBlocked { tab_id, url } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    t.prompts.popup.record(url, now);
                    true
                } else {
                    false
                }
            }
            EngineMsg::AppLinkBlocked { tab_id, url } => {
                if self.tabs.iter().any(|t| t.id == tab_id) {
                    if self.app_links.len() >= 64 {
                        self.app_links.pop_front();
                    }
                    self.app_links.push_back((tab_id, url));
                    true
                } else {
                    false
                }
            }
            EngineMsg::PasskeyRefused { tab_id, origin, main_frame, .. } => {
                let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) else {
                    return false;
                };
                let main_origin = crate::policy::destination_origin(&t.url);
                if !main_frame && (origin.is_none() || origin != main_origin) {
                    return false;
                }
                let mut ui_changed = false;
                if t.prompts.agent_driven && !t.prompts.passkey_interrupted {
                    t.prompts.passkey_interrupted = true;
                    let site = main_origin
                        .as_deref()
                        .or(origin.as_deref())
                        .and_then(|o| o.split_once("://").map(|(_, rest)| rest))
                        .unwrap_or("")
                        .to_owned();
                    t.prompts.agent_passkey_refused = Some(site);
                    ui_changed = true;
                }
                if !t.prompts.passkey_noticed {
                    t.prompts.passkey_noticed = true;
                    t.prompts.set_blocked_guard_notice(
                        BlockedGuardReason::Passkey {
                            native: cfg!(windows),
                            url: Some(t.url.clone()),
                        },
                        now,
                    );
                    ui_changed = true;
                }
                ui_changed
            }
            EngineMsg::CopiedImage { tab_id, png_b64 } => {
                // Only for a tab this app opened; at most a few waiting (each is one person's click).
                if self.tabs.iter().any(|t| t.id == tab_id) && self.copied_images.len() < 4 {
                    self.copied_images.push_back(png_b64);
                }
                true
            }
            EngineMsg::DownloadOnlyTab { tab_id } => {
                if self.tabs.iter().any(|t| t.id == tab_id) && !self.download_only_tabs.contains(&tab_id) {
                    self.download_only_tabs.push_back(tab_id);
                }
                true
            }
            EngineMsg::FileChooserBlocked { tab_id } => {
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    t.prompts.file_chooser_blocked = t.prompts.file_chooser_blocked.saturating_add(1);
                    if t.prompts.agent_driven {
                        t.prompts.agent_file_chooser_blocked = true;
                    }
                    t.prompts.set_blocked_guard_notice(BlockedGuardReason::Action, now);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }
}
