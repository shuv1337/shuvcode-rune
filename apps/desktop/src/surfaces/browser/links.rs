//! Policy-first link routing into the embedded browser pane matching official `origin/dev`
//! `b9a60d9d` (`packages/app/src/runtime/plugin/{app-links.ts,render.tsx}`,
//! `packages/gui-plugins/src/browser/{refusal-notice.ts,refusal-bar.tsx,notice-actions.ts}`),
//! plus the host-owned 500 ms guarded app-link review lifecycle (60 s expiring 128-bit CSPRNG
//! token + live `fits()` full-visibility check).

#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

#[cfg(not(target_arch = "wasm32"))]
use browser_policy::{is_confirmed_app_scheme_url, is_strict_rfc3986_uri};
use browser_policy::{
    AppSchemeAction, AppSchemeSource, classify_app_link, destination_origin, is_file_scheme,
    is_http_with_credentials,
};
#[cfg(not(target_arch = "wasm32"))]
use rune::time::Instant;
use rune::{PointerButton, Tx};

use crate::App;

/// Where a clicked web or app-scheme link originated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkSource {
    Chat,
    FileViewer,
    Terminal,
}

/// Host-owned single-use app-scheme review bound to one browser tab and the exact URL displayed
/// on the native 500 ms guarded review bar (`prompts/app_link.rs`), matching official
/// `notice-actions.ts:16-50` (60 s expiring random token) and `refusal-bar.tsx:18-63` (`fits()`
/// live full-visibility downgrade to Copy).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub struct AppLinkReview {
    /// 128-bit CSPRNG hex token (never exposed to Luau plugins).
    pub token: String,
    pub tab_id: u64,
    pub url: String,
    pub source: AppSchemeSource,
    pub action: AppSchemeAction,
    /// Single-use Open authorization expires 60 s after creation (`notice-actions.ts:21-25`),
    /// downgrading the notice to Copy-only.
    pub expires_at: Instant,
    /// Tab URL at the moment the review was created; navigation immediately invalidates the bar.
    pub tab_url: String,
    /// Last drawn rect `[x, y, w, h]` of the bar reported by `El::on_drawn_layout_tx`.
    pub last_rect: Option<[f32; 4]>,
    /// Last drawn viewport rect `[x, y, w, h]` of the target box (`max_h = 80`), window rect, and
    /// full target text size `[w, h]` for official's `fits()` check (`refusal-bar.tsx:22-26`).
    pub target_box_rect: Option<[f32; 4]>,
    pub window_rect: Option<[f32; 4]>,
    pub target_text_size: Option<[f32; 2]>,
    /// True ONLY when the entire target URL fits unclipped inside the target box AND inside the
    /// window viewport (`fits()`). Starts `false` until layouts confirm full visibility; any clip
    /// downgrades the action to Copy-only.
    pub target_fits: bool,
    /// When the bar became visible and stationary in the focused window (`None` while hidden or
    /// moving; resets whenever `target_fits` transitions to `true`, matching `refusal-bar.tsx:63`).
    pub visible_since: Option<Instant>,
    /// Timestamp of a pointer-down on the Open/Copy or Dismiss button (only recorded when
    /// `visible_since` has already elapsed 500 ms, matching `activation-delay.ts:13-53`).
    pub action_pressed_at: Option<Instant>,
    pub dismiss_pressed_at: Option<Instant>,
    /// True once the bar has been drawn on screen; switching tabs or closing the side panel after
    /// visibility immediately kills the review.
    pub was_visible: bool,
    /// True once the owning tab was observed not loading (`!tab.loading`). Prevents initial
    /// `about:blank` or mid-load tab state from destroying a conversation review before it draws.
    pub saw_settled: bool,
    /// True once `tx.wake_after` was armed for the 60 s TTL expiry so idle bars flip to Copy.
    pub ttl_wake_scheduled: bool,
}

#[cfg(not(target_arch = "wasm32"))]
const REVIEW_GUARD_MS: u64 = 500;
#[cfg(not(target_arch = "wasm32"))]
const REVIEW_TTL_SECS: u64 = 60;

#[cfg(not(target_arch = "wasm32"))]
fn random_review_token() -> Option<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).ok()?;
    let mut out = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    Some(out)
}

impl App {
    /// Routes a web or app-scheme URL clicked in a session through `browser_policy` and
    /// `Settings > Browser` (`Open links in OpenCode`).
    ///
    /// - `http:` / `https:` without credentials: opens in the embedded browser pane when
    ///   `browser_links_enabled()` is on, a session is active, the window is wide, the browser
    ///   pane is available (an engine can run — not `Unsupported` or `Fault::NotPublished`), and
    ///   no modifier (`Shift`/`Ctrl`/`Cmd`/`Alt`) or middle-click forced the system browser;
    ///   otherwise opens in the OS default browser (live runs only).
    /// - `http:` / `https:` with credentials (`user:pass@host`): refused (never opened).
    /// - `file:`: never handed to the OS browser (`ours.md` §4).
    /// - `mailto:` (matching official `app-links.ts:22-29` + `render.tsx:212`, stricter RFC 3986
    ///   char set & RFC 6068 key allow-list): opens directly in the OS mail client (whose compose
    ///   window displays every recipient) when all query keys are in `{to, cc, bcc, subject, body}`;
    ///   any other key copies only; any forbidden RFC 3986 character is refused.
    /// - Other app-scheme links (`slack:`, `obsidian:`, `vscode:`, etc.): strict RFC 3986 chars
    ///   required; conversation editor schemes (`PAGE_ONLY`) and non-open `obsidian:` copy only;
    ///   allowed 18 conversation `APPS` open the native 500 ms guarded review bar (60 s CSPRNG
    ///   token + `fits()` visibility downgrade to Copy); denied schemes are silently refused.
    pub fn route_web_or_app_link(
        &mut self,
        sid: Option<&str>,
        href: &str,
        source: LinkSource,
        shift_override: Option<bool>,
        tx: &mut Tx,
    ) {
        self.route_web_or_app_link_inner(sid, href, source, shift_override, true, false, tx);
    }

    /// Routes a web/app link initiated by a third-party plugin or tool call (`Links:Open`):
    /// refuses all `artifact://` and same-server `/api/experimental/artifacts/...` links so
    /// untrusted plugins or tools can never mint artifact preview grants (`notes/review-rd281.md`).
    pub fn route_plugin_web_or_app_link(
        &mut self,
        sid: Option<&str>,
        href: &str,
        source: LinkSource,
        agent: bool,
        tx: &mut Tx,
    ) {
        self.route_web_or_app_link_inner(sid, href, source, None, false, agent, tx);
    }

    fn route_web_or_app_link_inner(
        &mut self,
        sid: Option<&str>,
        href: &str,
        _source: LinkSource,
        shift_override: Option<bool>,
        allow_artifacts: bool,
        agent: bool,
        tx: &mut Tx,
    ) {
        let trimmed = href.trim();
        if trimmed.is_empty()
            || is_file_scheme(trimmed)
            || browser_policy::preview::is_preview_scheme(trimmed)
            || browser_policy::preview::is_workspace_scheme(trimmed)
        {
            return;
        }
        if !allow_artifacts {
            if browser_policy::preview::is_artifact_scheme(trimmed) {
                return;
            }
            #[cfg(not(target_arch = "wasm32"))]
            if let Some((_, srv)) = self.preview_server_for(sid)
                && browser_policy::preview::artifact_link(trimmed, &srv.base).is_some()
            {
                return;
            }
        }
        // Artifact links (`artifact://<id>/<path>` or same-server `/api/experimental/artifacts/<id>/view/...`)
        // open directly in an isolated preview tab when a session is active (built-in/user only).
        if allow_artifacts && let Some(sid) = sid {
            let was = crate::panels::panes_shown(self);
            if self.try_open_artifact_link(sid, trimmed) {
                crate::panels::state::finish_show_pane_tab(self, was, tx);
                return;
            }
        }
        let is_http = trimmed.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("http://"))
            || trimmed.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://"));
        if is_http {
            if is_http_with_credentials(trimmed) {
                crate::status_toast::fail(
                    self,
                    opencode_i18n::t!("plugin.browser.refused.credentials").to_owned(),
                    String::new(),
                );
                return;
            }
            if destination_origin(trimmed).is_none() {
                return;
            }
            // Match official (`packages/app/src/runtime/plugin/render.tsx:229`): Shift, Ctrl, Cmd,
            // Alt, or middle-click bypasses the pane and opens in the OS default browser. Terminal
            // clicks pass `shift_override = Some(shift)` because Ctrl/Cmd is the terminal link
            // activation gesture there.
            let modifier_external = shift_override.unwrap_or_else(|| {
                let m = tx.mods();
                m.shift || m.ctrl || m.cmd || m.alt || m.logo
            });
            let middle = tx.button() == PointerButton::Middle;
            let open_in_pane = !modifier_external
                && !middle
                && sid.is_some()
                && !self.narrow()
                && self.browser_links_enabled()
                && !self.browser_unsupported_for_links();
            if open_in_pane {
                // Land the tab switch immediately when the side region is already open;
                // fly open on SIDE_SPRING when it was closed (matching files/review/terminal).
                self.show_pane_tab(tx, |s| {
                    #[cfg(not(target_arch = "wasm32"))]
                    if agent {
                        s.open_new_agent_browser_tab_for_session(sid, trimmed, None, true);
                    } else {
                        s.open_new_browser_tab_for_session(sid, trimmed, None, true);
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        let _ = agent;
                        s.open_new_browser_tab_for_session(sid, trimmed, None, true);
                    }
                });
            } else if !agent && self.live() {
                if self.external_url_allowed_for_session(sid, trimmed) {
                    crate::open_in::open_external(tx, trimmed);
                } else {
                    crate::status_toast::fail(
                        self,
                        opencode_i18n::t!("rune.browser.menu.pageUnavailable").to_owned(),
                        String::new(),
                    );
                }
            }
            return;
        }

        if agent {
            return;
        }
        // Non-HTTP(S) scheme: validate the ORIGINAL string without trimming away unsafe characters.
        match classify_app_link(href, AppSchemeSource::Conversation) {
            // Matching official `app-links.ts:22-29` & `render.tsx:212`: safe `mailto:` opens
            // directly in the OS mail client (which displays every recipient in its compose window).
            Some(AppSchemeAction::Mail) => {
                if self.live() {
                    crate::open_in::open_external(tx, href);
                }
            }
            Some(AppSchemeAction::Open) => {
                if let Some(sid) = sid
                    && !self.narrow()
                    && !self.browser_unsupported_for_links()
                    && self.queue_browser_app_link_review(sid, href, AppSchemeSource::Conversation)
                {
                    return;
                }
                tx.copy(href.to_owned());
                crate::status_toast::say(self, opencode_i18n::t!("common.copied").to_owned(), String::new());
            }
            Some(AppSchemeAction::Copy) => {
                tx.copy(href.to_owned());
                crate::status_toast::say(self, opencode_i18n::t!("common.copied").to_owned(), String::new());
            }
            None => {}
        }
    }

    fn browser_unsupported_for_links(&self) -> bool {
        #[cfg(target_arch = "wasm32")]
        {
            true
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.browser_unsupported()
        }
    }

    pub(crate) fn queue_browser_app_link_review(
        &mut self,
        sid: &str,
        href: &str,
        source: AppSchemeSource,
    ) -> bool {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (sid, href, source);
            false
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if classify_app_link(href, source).is_none() {
                return false;
            }
            // Never fall back to another session's focused browser tab (`review-rd-tools.md` §2).
            let existing_in_session = self.active_browser_tab_for_session(sid).or_else(|| {
                self.workspace_panes
                    .browser
                    .session_tabs
                    .get(sid)?
                    .iter()
                    .rev()
                    .copied()
                    .find(|&id| self.workspace_panes.browser.host.tab(id).is_some())
            });
            let tab_id = if let Some(existing) = existing_in_session {
                let key = crate::panels::panel_key(super::PLUGIN, &existing.to_string());
                self.open_side_tab_for_session(sid, &key, None, false, false);
                existing
            } else {
                self.open_new_browser_tab_for_session(Some(sid), "about:blank", None, true)
            };
            self.queue_browser_app_link_review_for_tab(tab_id, href, source)
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn queue_browser_app_link_review_for_tab(
        &mut self,
        tab_id: u64,
        href: &str,
        source: AppSchemeSource,
    ) -> bool {
        let Some(tab) = self.workspace_panes.browser.host.tab(tab_id) else {
            return false;
        };
        // Defense-in-depth (Finding 4): page-origin app links are permitted ONLY on default,
        // non-agent, non-preview tabs (never `RemoteServer`, `Private`, `Artifact`, or `Preview`).
        if source == AppSchemeSource::Page
            && (!matches!(tab.context, browser_pane::ContextKind::Default)
                || tab.prompts.agent_driven
                || tab.preview.is_some())
        {
            return false;
        }
        if !is_strict_rfc3986_uri(href) {
            return false;
        }
        let Some(mut action) = classify_app_link(href, source) else {
            return false;
        };
        // Match official (`refusal-notice.ts:52-54`, `chromium.ts:377,399,424,871,887,907`):
        // page-initiated editor schemes (`vscode:`, `cursor:`, `zed:`, `jetbrains:`) downgrade to
        // Copy unless the host recorded direct user pointer/key input to this document.
        if source == AppSchemeSource::Page
            && !tab.prompts.document_input
            && browser_policy::is_page_only_app_scheme(href)
        {
            action = AppSchemeAction::Copy;
        }
        let Some(token) = random_review_token() else {
            return false;
        };
        let tab_url = tab.url.clone();
        let saw_settled = !tab.loading;
        let now = Instant::now();
        self.workspace_panes.browser.active_app_reviews.borrow_mut().insert(
            tab_id,
            AppLinkReview {
                token,
                tab_id,
                url: href.to_owned(),
                source,
                action,
                expires_at: now + Duration::from_secs(REVIEW_TTL_SECS),
                tab_url,
                last_rect: None,
                target_box_rect: None,
                window_rect: None,
                target_text_size: None,
                target_fits: false,
                visible_since: None,
                action_pressed_at: None,
                dismiss_pressed_at: None,
                was_visible: false,
                saw_settled,
                ttl_wake_scheduled: false,
            },
        );
        crate::plugin_host::bump();
        true
    }

    /// Drains page-origin `EngineMsg::AppLinkBlocked` notices and prunes/expires active reviews.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn poll_app_link_reviews(&mut self) -> bool {
        let mut changed = false;
        if self.workspace_panes.browser.host.has_app_links() {
            let links: Vec<_> = self.workspace_panes.browser.host.drain_app_links().collect();
            for (tab_id, url) in links {
                if self.queue_browser_app_link_review_for_tab(tab_id, &url, AppSchemeSource::Page) {
                    changed = true;
                }
            }
        }
        changed |= self.sync_app_link_reviews();
        changed
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn is_browser_tab_currently_selected_and_open(&self, tab_id: u64) -> bool {
        !self.plugins.window_blurred.get()
            && self.side_open()
            && self.browser_tab_session(tab_id).is_some_and(|owner| self.panel_shows(owner))
            && self.active_browser_tab_id() == Some(tab_id)
    }

    /// Tears down any review whose tab closed, navigated, started loading, switched tabs,
    /// switched sessions, closed the side panel, or lost window focus, and downgrades any review
    /// whose 60 s token TTL expired (`notice-actions.ts:21-25`) to Copy-only.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn sync_app_link_reviews(&mut self) -> bool {
        if self.workspace_panes.browser.active_app_reviews.borrow().is_empty() {
            return false;
        }
        if self.plugins.window_blurred.get() {
            self.workspace_panes.browser.active_app_reviews.borrow_mut().clear();
            crate::plugin_host::bump();
            return true;
        }
        let side_open = self.side_open();
        let active_tab = self.active_browser_tab_id();
        let now = Instant::now();
        let mut changed = false;
        let before = self.workspace_panes.browser.active_app_reviews.borrow().len();
        self.workspace_panes.browser.active_app_reviews.borrow_mut().retain(|&tab_id, r| {
            let Some(tab) = self.workspace_panes.browser.host.tab(tab_id) else {
                return false;
            };
            // Match official (`pane.ts:470-481`, `chromium.ts:886-890`): a committed navigation
            // (`tab.url != r.tab_url`) clears a settled notice, while a freshly opened
            // `about:blank` or already mid-load tab does NOT destroy a conversation review while
            // its in-flight load finishes (`!r.saw_settled`). Page-origin reviews always clear on
            // URL change or a new load after settling.
            if r.source == AppSchemeSource::Conversation && !r.saw_settled {
                r.tab_url = tab.url.clone();
            } else if tab.url != r.tab_url
                || (r.source == AppSchemeSource::Page && r.saw_settled && tab.loading)
            {
                return false;
            }
            if !tab.loading {
                r.saw_settled = true;
            }
            if !self.browser_tab_session(tab_id).is_some_and(|owner| self.panel_shows(owner)) {
                return false;
            }
            if r.was_visible && (!side_open || active_tab != Some(tab_id)) {
                return false;
            }
            if now >= r.expires_at && matches!(r.action, AppSchemeAction::Open | AppSchemeAction::Mail) {
                r.action = AppSchemeAction::Copy;
                changed = true;
            }
            true
        });
        changed |= self.workspace_panes.browser.active_app_reviews.borrow().len() != before;
        if changed {
            crate::plugin_host::bump();
        }
        changed
    }

    /// Tracks the drawn screen rect of an active review bar so the 500 ms activation timer starts
    /// ONLY once the bar is visible AND stationary in the focused window.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn observe_app_link_review_layout(&mut self, token: &str, rect: [f32; 4], tx: &mut Tx) {
        tx.live_only();
        if self.plugins.window_blurred.get() {
            self.clear_app_link_reviews();
            return;
        }
        let target_tab = self
            .workspace_panes
            .browser
            .active_app_reviews
            .borrow()
            .values()
            .find(|r| r.token == token)
            .map(|r| r.tab_id);
        let Some(tab_id) = target_tab else {
            return;
        };
        let visible_now = self.is_browser_tab_currently_selected_and_open(tab_id)
            && rect[2] > 0.5
            && rect[3] > 0.5;
        let mut reviews = self.workspace_panes.browser.active_app_reviews.borrow_mut();
        let Some(review) = reviews.get_mut(&tab_id).filter(|r| r.token == token) else {
            return;
        };
        if !visible_now {
            review.last_rect = None;
            review.visible_since = None;
            review.action_pressed_at = None;
            review.dismiss_pressed_at = None;
            return;
        }
        let stationary = review.last_rect.is_some_and(|prev| {
            (prev[0] - rect[0]).abs() <= 0.5
                && (prev[1] - rect[1]).abs() <= 0.5
                && (prev[2] - rect[2]).abs() <= 0.5
                && (prev[3] - rect[3]).abs() <= 0.5
        });
        review.was_visible = true;
        let now = Instant::now();
        if !stationary || review.visible_since.is_none() {
            review.last_rect = Some(rect);
            review.visible_since = Some(now);
            review.action_pressed_at = None;
            review.dismiss_pressed_at = None;
        }
        let mut expired_now = false;
        if matches!(review.action, AppSchemeAction::Open | AppSchemeAction::Mail) {
            if now >= review.expires_at {
                review.action = AppSchemeAction::Copy;
                expired_now = true;
            } else if !review.ttl_wake_scheduled {
                review.ttl_wake_scheduled = true;
                tx.wake_after(review.expires_at.saturating_duration_since(now));
            }
        }
        drop(reviews);
        if expired_now {
            crate::plugin_host::bump();
            tx.instant();
        }
    }

    /// Records pointer-down (`Phase::Down`) on the Open/Copy or Dismiss button so the 500 ms
    /// activation guard is enforced when the press BEGINS (matching official `activation-delay.ts:13-53`),
    /// never allowing a press started before 500 ms and released after 500 ms to activate.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn note_app_link_pointer(
        &mut self,
        token: &str,
        is_action: bool,
        phase: rune::Phase,
        cancelled: bool,
        tx: &mut Tx,
    ) {
        tx.live_only();
        let mut reviews = self.workspace_panes.browser.active_app_reviews.borrow_mut();
        let Some(review) = reviews.values_mut().find(|r| r.token == token) else {
            return;
        };
        let slot = if is_action { &mut review.action_pressed_at } else { &mut review.dismiss_pressed_at };
        match phase {
            rune::Phase::Down => {
                let now = Instant::now();
                let allowed = review
                    .visible_since
                    .is_some_and(|shown| now.saturating_duration_since(shown) >= Duration::from_millis(REVIEW_GUARD_MS));
                *slot = allowed.then_some(now);
            }
            rune::Phase::Up if cancelled => {
                *slot = None;
            }
            _ => {}
        }
    }

    /// Re-evaluates `fits()` whenever the window viewport resizes while a review bar is active,
    /// so shrinking a window past the target box immediately downgrades Open to Copy.
    pub(crate) fn observe_app_link_window_rect(&mut self, win_rect: [f32; 4], tx: &mut Tx) -> bool {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (win_rect, tx);
            false
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut flipped = false;
            {
                let mut reviews = self.workspace_panes.browser.active_app_reviews.borrow_mut();
                if reviews.is_empty() {
                    return false;
                }
                for review in reviews.values_mut() {
                    review.window_rect = Some(win_rect);
                    let next_fits = match (review.target_box_rect, review.target_text_size) {
                        (Some([bx, by, bw, bh]), Some([tw, th])) => {
                            let inside_box = bw > 0.5 && bh > 0.5 && tw <= bw + 0.5 && th <= bh + 0.5;
                            let inside_win = win_rect[2] > 0.5
                                && win_rect[3] > 0.5
                                && bx >= win_rect[0] - 1.0
                                && by >= win_rect[1] - 1.0
                                && bx + bw <= win_rect[0] + win_rect[2] + 1.0
                                && by + bh <= win_rect[1] + win_rect[3] + 1.0;
                            inside_box && inside_win
                        }
                        _ => false,
                    };
                    if next_fits != review.target_fits {
                        review.target_fits = next_fits;
                        review.visible_since = Some(Instant::now());
                        review.action_pressed_at = None;
                        review.dismiss_pressed_at = None;
                        flipped = true;
                    }
                }
            }
            if flipped {
                crate::plugin_host::bump();
                tx.instant();
            }
            flipped
        }
    }

    /// Live `fits()` viewport-box observation (`refusal-bar.tsx:18-61`), including window viewport
    /// clipping so a short or narrow window downgrades Open to Copy.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn observe_app_link_target_box(
        &mut self,
        token: &str,
        rect: [f32; 4],
        win_rect: Option<[f32; 4]>,
        tx: &mut Tx,
    ) {
        self.update_app_link_target_fit(token, Some(rect), win_rect, None, tx);
    }

    /// Live `fits()` full target-text observation (`refusal-bar.tsx:18-61`).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn observe_app_link_target_text(&mut self, token: &str, rect: [f32; 4], tx: &mut Tx) {
        self.update_app_link_target_fit(token, None, None, Some([rect[2], rect[3]]), tx);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn update_app_link_target_fit(
        &mut self,
        token: &str,
        box_rect: Option<[f32; 4]>,
        win_rect: Option<[f32; 4]>,
        text_size: Option<[f32; 2]>,
        tx: &mut Tx,
    ) {
        let mut flipped = false;
        {
            let mut reviews = self.workspace_panes.browser.active_app_reviews.borrow_mut();
            if let Some(review) = reviews.values_mut().find(|r| r.token == token) {
                if let Some(b) = box_rect {
                    review.target_box_rect = Some(b);
                }
                if let Some(w) = win_rect {
                    review.window_rect = Some(w);
                }
                if let Some(t) = text_size {
                    review.target_text_size = Some(t);
                }
                let next_fits = match (review.target_box_rect, review.target_text_size) {
                    (Some([bx, by, bw, bh]), Some([tw, th])) => {
                        let inside_box = bw > 0.5 && bh > 0.5 && tw <= bw + 0.5 && th <= bh + 0.5;
                        let inside_win = match review.window_rect {
                            Some([wx, wy, ww, wh]) if ww > 0.5 && wh > 0.5 => {
                                bx >= wx - 1.0
                                    && by >= wy - 1.0
                                    && bx + bw <= wx + ww + 1.0
                                    && by + bh <= wy + wh + 1.0
                            }
                            _ => true,
                        };
                        inside_box && inside_win
                    }
                    _ => false,
                };
                if next_fits != review.target_fits {
                    review.target_fits = next_fits;
                    // Restart the 500 ms activation delay whenever the button flips between Copy
                    // and Open (`refusal-bar.tsx:63`).
                    review.visible_since = Some(Instant::now());
                    review.action_pressed_at = None;
                    review.dismiss_pressed_at = None;
                    flipped = true;
                }
            }
        }
        if flipped {
            crate::plugin_host::bump();
            tx.instant();
        } else {
            tx.live_only();
        }
    }

    /// Clears all active app-link reviews immediately (called on window blur).
    pub(crate) fn clear_app_link_reviews(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if !self.workspace_panes.browser.active_app_reviews.borrow().is_empty() {
                self.workspace_panes.browser.active_app_reviews.borrow_mut().clear();
                crate::plugin_host::bump();
            }
        }
    }

    /// Handles a native click/key activation (`El::on_press_tx`) on the review bar's Open/Copy
    /// button. Never reachable from hover, timers, signals, third-party plugins, or MCP tools.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn confirm_app_link_review(&mut self, token: &str, tx: &mut Tx) {
        self.sync_app_link_reviews();
        if self.plugins.window_blurred.get() {
            return;
        }
        let by_keys = tx.by_keys();
        let matched = {
            let mut reviews = self.workspace_panes.browser.active_app_reviews.borrow_mut();
            reviews
                .values_mut()
                .find(|r| r.token == token)
                .map(|r| {
                    let pressed_at = r.action_pressed_at.take();
                    let armed = if by_keys {
                        r.visible_since
                            .is_some_and(|s| s.elapsed() >= Duration::from_millis(REVIEW_GUARD_MS))
                    } else {
                        pressed_at.is_some_and(|p| {
                            r.visible_since.is_some_and(|s| {
                                p.saturating_duration_since(s) >= Duration::from_millis(REVIEW_GUARD_MS)
                            })
                        })
                    };
                    (r.tab_id, armed)
                })
        };
        let Some((tab_id, armed)) = matched else {
            return;
        };
        if !armed || !self.is_browser_tab_currently_selected_and_open(tab_id) {
            return;
        }
        let Some(review) = self.workspace_panes.browser.active_app_reviews.borrow_mut().remove(&tab_id) else {
            return;
        };
        if review.token != token {
            return;
        }
        let can_open = matches!(review.action, AppSchemeAction::Open | AppSchemeAction::Mail)
            && review.target_fits
            && Instant::now() < review.expires_at;
        if can_open && self.live() && is_confirmed_app_scheme_url(&review.url, review.source) {
            crate::open_in::open_confirmed_app_url(tx, &review.url, review.source);
        } else {
            tx.copy(review.url);
            crate::status_toast::say(self, opencode_i18n::t!("common.copied").to_owned(), String::new());
        }
        crate::plugin_host::bump();
        tx.instant();
    }

    /// Handles a native click/key activation (`El::on_press_tx`) on the review bar's Dismiss button.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn dismiss_app_link_review(&mut self, token: &str, tx: &mut Tx) {
        self.sync_app_link_reviews();
        let by_keys = tx.by_keys();
        let tab_id = {
            let mut reviews = self.workspace_panes.browser.active_app_reviews.borrow_mut();
            reviews
                .values_mut()
                .find(|r| r.token == token)
                .and_then(|r| {
                    let pressed_at = r.dismiss_pressed_at.take();
                    let armed = if by_keys {
                        r.visible_since
                            .is_some_and(|s| s.elapsed() >= Duration::from_millis(REVIEW_GUARD_MS))
                    } else {
                        pressed_at.is_some_and(|p| {
                            r.visible_since.is_some_and(|s| {
                                p.saturating_duration_since(s) >= Duration::from_millis(REVIEW_GUARD_MS)
                            })
                        })
                    };
                    armed.then_some(r.tab_id)
                })
        };
        if let Some(tab_id) = tab_id {
            self.workspace_panes.browser.active_app_reviews.borrow_mut().remove(&tab_id);
            crate::plugin_host::bump();
            tx.instant();
        }
    }
}
