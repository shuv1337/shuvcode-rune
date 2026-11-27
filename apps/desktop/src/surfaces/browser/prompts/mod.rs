//! In-pane browser overlays and modal key isolation for JS dialogs, permission prompts,
//! HTTP auth challenges, and blocked pop-ups (`packages/gui-plugins/src/browser/{prompts,
//! prompt-keys,page-prompts}.ts`).
//!
//! Drawn natively over `ui.browserSurface` inside `browser::view::surface` so page modals get
//! first-class Rune focus/key isolation without inflating `contract.json` or adding fields to
//! `struct App`.

mod app_link;
mod keys;
mod view;

use browser_pane::{DialogKind, PermissionDecision};
use opencode_kernel::{EscapeLayer, Feature, KernelBuilder, OverlayRegistration, Slot};
use rune::{KeyEvent, TextEdit, Tx};

use crate::App;

pub use app_link::app_link_review_bar;
pub use view::{view_bars, view_modal};

pub(super) const PROMPT_CARD_KEY: &str = "browser-prompt-card";
pub(super) const PROMPT_INPUT_KEY: &str = "browser-prompt-input";
pub(super) const PROMPT_STOP_KEY: &str = "browser-prompt-stop";
pub(super) const PROMPT_CANCEL_KEY: &str = "browser-prompt-cancel";
pub(super) const PROMPT_OK_KEY: &str = "browser-prompt-ok";
pub(super) const AUTH_CARD_KEY: &str = "browser-auth-card";
pub(super) const AUTH_USER_KEY: &str = "browser-auth-user";
pub(super) const AUTH_PASS_KEY: &str = "browser-auth-pass";
pub(super) const AUTH_CANCEL_KEY: &str = "browser-auth-cancel";
pub(super) const AUTH_SUBMIT_KEY: &str = "browser-auth-submit";

/// Per-window UI buffers for the active browser page prompt or HTTP auth challenge.
#[derive(Default)]
pub struct PromptUiState {
    /// Last visible browser tab id in the side panel (re-arms `InputGuard` on reveal/switch).
    pub visible_tab: Option<u64>,
    /// `(tab_id, dialog_id)` currently synced into `prompt_input` & `stop_dialogs`.
    pub dialog_target: Option<(u64, u64)>,
    pub prompt_input: TextEdit,
    pub stop_dialogs: bool,
    /// `(tab_id, auth_id)` currently synced into `auth_user` & `auth_pass`.
    pub auth_target: Option<(u64, u64)>,
    pub auth_user: TextEdit,
    pub auth_pass: TextEdit,
    /// `(tab_id, request_id)` for the active site permission bar (`prompts.tsx:467` focus handoff).
    pub perm_target: Option<(u64, u64)>,
}

pub static BROWSER_PROMPTS: Slot<PromptUiState> = Slot::new();

pub struct BrowserFeature;

impl Feature<App> for BrowserFeature {
    fn id(&self) -> &'static str {
        "browser-prompts"
    }

    fn register(&self, builder: &mut KernelBuilder<App>) {
        builder.register_slot(&BROWSER_PROMPTS);
        builder.register_slot(&super::extensions::BROWSER_EXTENSIONS);
        // Page JS dialogs and HTTP auth challenges isolate keys inside the MODAL band so shell
        // shortcuts cannot leak behind them (`prompt-keys.ts`).
        builder.register_overlay(
            OverlayRegistration::new("browser-page-prompt")
                .with_key_in(crate::overlays::key_band::MODAL, keys::modal_key),
        );
        builder.register_escape_layer(EscapeLayer::new(
            "browser-page-bar",
            crate::overlays::escape_band::CHROME,
            has_dismissible_bar,
            dismiss_bar,
        ));
        builder.add_visibility("browser.visibility", |s, shown, _| {
            s.on_browser_window_visibility(shown);
        });
    }
}

/// Whether `key` is a control owned by an open in-pane browser modal.
pub(super) fn is_modal_key(key: Option<&str>) -> bool {
    matches!(
        key,
        Some(
            PROMPT_CARD_KEY
                | PROMPT_INPUT_KEY
                | PROMPT_STOP_KEY
                | PROMPT_CANCEL_KEY
                | PROMPT_OK_KEY
                | AUTH_CARD_KEY
                | AUTH_USER_KEY
                | AUTH_PASS_KEY
                | AUTH_CANCEL_KEY
                | AUTH_SUBMIT_KEY
        )
    )
}

fn is_perm_key(key: Option<&str>) -> bool {
    matches!(
        key,
        Some(
            "browser-perm-never"
                | "browser-perm-block"
                | "browser-perm-allow-once"
                | "browser-perm-allow-always"
                | "browser-perm-allow-session"
                | "browser-perm-dismiss"
        )
    )
}

/// Immediately clears credentials from `PromptUiState` once submitted or cancelled (`prompts.tsx:320`).
pub(super) fn clear_auth_fields(s: &mut App) {
    let ui = s.kernel.slots.get_mut(&BROWSER_PROMPTS);
    ui.auth_user = TextEdit::default();
    ui.auth_pass = TextEdit::default();
}

/// Ticks prompt arming/timeout timers, syncs modal `TextEdit` fields, and interrupts DevTools
/// calls blocked behind an open page dialog (`dialog-machine.ts`).
pub fn poll(s: &mut App, tx: &mut Tx, now: rune::time::Instant) -> bool {
    let (mut changed, wake) = s.workspace_panes.browser.host.tick_prompts(now);
    if let Some(at) = wake {
        tx.wake_after(at.saturating_duration_since(now));
    }
    changed |= super::extensions::poll(s);
    sync_ui(s, tx);
    if !s.workspace_panes.browser.devtools.is_empty() {
        let blocked: Vec<u64> = s
            .workspace_panes
            .browser
            .devtools
            .iter()
            .filter(|(_, pending)| {
                !pending.is_dialog
                    && s.workspace_panes
                        .browser
                        .host
                        .tab(pending.tab_id)
                        .is_some_and(|t| t.prompts.dialog.open().is_some())
            })
            .map(|(&req, _)| req)
            .collect();
        for req in blocked {
            if let Some(pending) = s.workspace_panes.browser.devtools.remove(&req) {
                let tab = pending.tab_id;
                let msg = format!(
                    "[browser.dialog_interrupted] A JavaScript dialog opened in tab {tab} while the action was running. Answer or dismiss the dialog before continuing."
                );
                pending.done.complete(rune_luau::Data::Map(std::collections::BTreeMap::from([(
                    "error".to_owned(),
                    rune_luau::Data::String(msg),
                )])));
                changed = true;
            }
        }
    }
    changed
}

/// Syncs prompt/auth `TextEdit` state and keyboard focus when the active tab's dialog or auth
/// challenge opens, changes, or closes (`page-prompts.ts:212`).
pub fn sync_ui(s: &mut App, tx: &mut Tx) {
    let tab_id = (s.side_open() && !s.narrow())
        .then(|| s.active_browser_tab_id())
        .flatten();
    let prev_visible = s.kernel.slots.get(&BROWSER_PROMPTS).visible_tab;
    if prev_visible != tab_id {
        s.kernel.slots.get_mut(&BROWSER_PROMPTS).visible_tab = tab_id;
        if let Some(tid) = tab_id {
            s.workspace_panes.browser.host.rearm_tab_prompts(tid);
        }
    }
    let dialog_now = tab_id.and_then(|id| {
        let t = s.workspace_panes.browser.host.tab(id)?;
        let d = t.prompts.dialog.open()?;
        Some((id, d.id, d.kind, d.default_text.clone(), t.prompts.agent_driven))
    });
    let auth_now = tab_id.and_then(|id| {
        let t = s.workspace_panes.browser.host.tab(id)?;
        let c = t.prompts.auth.active()?;
        Some((id, c.challenge.id, t.prompts.agent_driven))
    });
    let perm_now = tab_id.and_then(|id| {
        let t = s.workspace_panes.browser.host.tab(id)?;
        let q = t.prompts.permission.active()?;
        let trustworthy = browser_pane::prompts::trustworthy_origin(&q.request.origin)
            && !q.request.origin.to_ascii_lowercase().starts_with("oc-preview:");
        let keep_profile = !q.agent_driven && t.preview.is_none() && t.context.is_default() && trustworthy;
        Some((id, q.request.id, q.agent_driven, keep_profile))
    });
    let in_browser = s.focused_browser_pane(false).is_some()
        || is_modal_key(tx.focused())
        || is_perm_key(tx.focused())
        || tx.focused() == Some("browser-surface");
    let user_elsewhere = s.composer.focused || (tx.focused().is_some() && !in_browser);
    let mut focus_target: Option<&'static str> = None;
    let mut restore_surface = false;
    let has_dialog = dialog_now.is_some();
    let has_auth = auth_now.is_some();

    let ui = s.kernel.slots.get_mut(&BROWSER_PROMPTS);
    match dialog_now {
        Some((tid, did, kind, default_text, agent)) => {
            if ui.dialog_target != Some((tid, did)) {
                ui.dialog_target = Some((tid, did));
                ui.stop_dialogs = false;
                ui.prompt_input = TextEdit::new(default_text);
                if kind == DialogKind::Prompt {
                    let len = ui.prompt_input.text.len();
                    ui.prompt_input.set_selection(0, len);
                }
                // Match `page-prompts.ts:212`: take focus when the page had it or a human tab opened it,
                // never auto-focusing Accept (`PROMPT_OK_KEY`) so a held Enter/Space cannot auto-confirm.
                if in_browser || (!agent && !user_elsewhere) {
                    focus_target = Some(match kind {
                        DialogKind::Prompt => PROMPT_INPUT_KEY,
                        DialogKind::Alert => PROMPT_CARD_KEY,
                        DialogKind::Confirm | DialogKind::Leave => PROMPT_CANCEL_KEY,
                    });
                }
            }
        }
        None => {
            if ui.dialog_target.take().is_some() {
                ui.prompt_input = TextEdit::default();
                ui.stop_dialogs = false;
                if is_modal_key(tx.focused()) && auth_now.is_none() {
                    restore_surface = true;
                }
            }
        }
    }
    match auth_now {
        Some((tid, aid, agent)) => {
            if ui.auth_target != Some((tid, aid)) {
                ui.auth_target = Some((tid, aid));
                ui.auth_user = TextEdit::default();
                ui.auth_pass = TextEdit::default();
                if !has_dialog && (in_browser || (!agent && !user_elsewhere)) {
                    focus_target = Some(AUTH_USER_KEY);
                }
            }
        }
        None => {
            if ui.auth_target.take().is_some() {
                ui.auth_user = TextEdit::default();
                ui.auth_pass = TextEdit::default();
                if is_modal_key(tx.focused()) && !has_dialog {
                    restore_surface = true;
                }
            }
        }
    }
    match perm_now {
        Some((tid, pid, agent, keep_profile)) => {
            if ui.perm_target != Some((tid, pid)) {
                ui.perm_target = Some((tid, pid));
                // Match `prompts.tsx:467` & `page-prompts.ts:213` (`bar = page && !agent`): never
                // pull keyboard focus on an agent-driven tab, and only focus the first (deny) button.
                if !agent && !has_dialog && !has_auth && (in_browser || !user_elsewhere) {
                    focus_target = Some(if keep_profile { "browser-perm-never" } else { "browser-perm-block" });
                }
            }
        }
        None => {
            if ui.perm_target.take().is_some() && is_perm_key(tx.focused()) && !has_dialog && !has_auth {
                restore_surface = true;
            }
        }
    }
    if let Some(target) = focus_target {
        tx.focus(target, true);
    } else if restore_surface {
        tx.focus("browser-surface", false);
    }
}

fn has_dismissible_bar(s: &App) -> bool {
    if !s.side_open() || s.narrow() {
        return false;
    }
    let Some(id) = s.active_browser_tab_id() else { return false };
    let Some(t) = s.workspace_panes.browser.host.tab(id) else { return false };
    t.prompts.popup.blocked().is_some() || t.prompts.permission.active().is_some()
}

fn dismiss_bar(s: &mut App, _ev: &KeyEvent, _tx: &mut Tx) {
    let Some(id) = s.active_browser_tab_id() else { return };
    if s.workspace_panes.browser.host.dismiss_blocked_popup(id) {
        return;
    }
    if let Some(req_id) = s
        .workspace_panes
        .browser
        .host
        .tab(id)
        .and_then(|t| t.prompts.permission.active().map(|p| p.request.id))
    {
        s.workspace_panes
            .browser
            .host
            .answer_permission(id, req_id, PermissionDecision::Dismiss);
    }
}
