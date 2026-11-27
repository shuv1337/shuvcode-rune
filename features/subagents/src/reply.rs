//! Subagent session bottom status bar (`packages/app/src/session/composer/subagent-bar.tsx`).
//!
//! A subagent session takes no prompts, so its composer slot is one quiet status line on the
//! timeline's own surface: the run's state and duration, the agent, provider / model and variant,
//! and the way back (`Main session` / `Esc`).

use opencode_kit::client::{Prompt, Write};
use opencode_kit::model::{self, SessionInfo};
use rune::{El, Key, KeyEvent, TextStyle, Tx, col, el, icon, row, text};
use rune_theme_opencode::icons as ic;
use session_view::focus::FocusExt;
use session_view::palette::Pal;
use session_view::style::{INSTANT, activity_ring_sized};

use crate::{SubagentModel, SubagentsHost, open_parent, run_clock, run_time, subtree_running};

/// The open page's session when it's a subagent's.
pub fn subagent_page(host: &dyn SubagentsHost) -> Option<&SessionInfo> {
    let id = host.active_session_id()?;
    host.session_info(id).filter(|i| i.parent.is_some())
}

/// The write that sends `prompt` to `session`, a subagent's as any other.
pub fn send_write(_host: &dyn SubagentsHost, session: &str, prompt: Prompt) -> Write {
    Write::Send { session: session.to_owned(), prompt }
}

/// Whether the open page is a subagent's with nothing typed in its composer.
pub fn composer_empty(host: &dyn SubagentsHost) -> bool {
    subagent_page(host).is_some() && host.composer_is_empty()
}

/// Esc on a subagent's page: back to the session that started it
/// (`packages/app/src/session/composer/region.tsx:201-205`).
pub fn esc(host: &mut dyn SubagentsHost, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let m = ev.mods;
    ev.key == Key::Escape && !m.cmd && !m.ctrl && !m.alt && !m.shift && subagent_page(host).is_some() && open_parent(host, tx)
}

/// The bar's words (`subagent-bar.tsx`): 12 on 16 at 440, tabular, faint.
const BAR: TextStyle = TextStyle::new(12.0, 440.0).lh(16.0).tracking(-0.04).features("\"tnum\"");
/// The back button's label (`Button size="small"`): 13 on 20 at 530.
const BACK: TextStyle = TextStyle::new(13.0, 530.0).lh(20.0).tracking(-0.04);
/// A keycap's label (`keybind-v2`): 11 at 530, upper case.
const KEY: TextStyle = TextStyle::new(11.0, 530.0).lh(11.0).tracking(0.05);

/// Subagent status bar in the composer dock slot (`packages/app/src/session/composer/subagent-bar.tsx`):
/// status icon, status & duration, agent, provider / model & variant, and `Main session` (`Esc`).
/// Only what is known shows, with one `·` separator between neighbours.
pub fn status_line<H: SubagentsHost + 'static>(host: &H, p: &Pal, id: &str) -> Option<El> {
    if host.session_info(id).is_none_or(|i| i.parent.is_none()) {
        return None;
    }
    let back = build_back_shell(p, host.shows_esc_keycap()).on_press_tx(|s: &mut H, tx: &mut Tx| back_to_parent(s, tx));
    build_status_line(host, p, id, back)
}

fn back_to_parent(host: &mut dyn SubagentsHost, tx: &mut Tx) {
    tx.instant();
    let _ = open_parent(host, tx);
}

fn build_back_shell(p: &Pal, show_esc: bool) -> El {
    row()
        .key("back")
        .role("button")
        .label_static(opencode_i18n::t!("session.child.backToParent"))
        .h(24.0)
        .px(6.0)
        .gap(6.0)
        .center_y()
        .radius(4.0)
        .focus_default(p)
        .hover(p.hover)
        .press(p.pressed)
        .hover_motion(INSTANT, INSTANT)
        .child(text(opencode_i18n::t!("session.child.backToParent")).font(BACK).color(p.faint))
        // The key shows only in a window at least 640 wide (`hidden sm:block`, `subagent-bar.tsx:110`).
        .children(show_esc.then(|| row().key("esc").h(14.0).min_w(14.0).px(4.0).center().radius(2.0).fill(p.layer3).child(text("ESC").font(KEY).color(p.muted))))
}

fn model_view(p: &Pal, m: SubagentModel) -> El {
    let mut span = row().key("subagent-model").shrink().min_w(0.0).center_y();
    if let Some(prov) = m.provider.filter(|s| !s.trim().is_empty()) {
        span = span
            .child(text(prov).key("provider").font(BAR).color(p.faint).single_line())
            // `Slash gap={4}` (`subagent-bar.tsx:89`): 4 px each side less glyph side bearings.
            .child(text("/").key("slash").font(BAR).color(p.icon_faint).ml(3.25).mr(3.16));
    }
    span = span.child(text(m.name).key("model").font(BAR).color(p.faint).truncate().shrink().min_w(0.0));
    // Like message metadata: variant follows the name, dimmer, with no separator (`subagent-bar.tsx:96-101`).
    if let Some(var) = m.variant.filter(|v| !v.trim().is_empty() && v != "default") {
        span = span.child(text(model::capitalize(&var)).key("variant").font(BAR).color(p.faint).opacity(0.66).ml(4.0).single_line());
    }
    span
}

fn build_status_line(host: &dyn SubagentsHost, p: &Pal, id: &str, back: El) -> Option<El> {
    let info = host.session_info(id).filter(|i| i.parent.is_some())?;
    let running = subtree_running(host, id);
    let (glyph, status) = if running {
        (None, Some(opencode_i18n::t!("session.child.status.running")))
    } else {
        match info.outcome.as_deref() {
            Some("succeeded") => (Some(&ic::CHECK), Some(opencode_i18n::t!("session.child.status.succeeded"))),
            // Official `subagent-bar.tsx:53-55` uses `close` for both failed and interrupted runs.
            Some("failed") => (Some(&ic::OUTLINE_XMARK), Some(opencode_i18n::t!("session.child.status.failed"))),
            Some("interrupted") => (Some(&ic::OUTLINE_XMARK), Some(opencode_i18n::t!("session.child.status.interrupted"))),
            _ => (None, None),
        }
    };
    let words = |t: String| text(t).font(BAR).color(p.faint).single_line();
    let mut summary = row().key("summary").grow(1.0).shrink().min_w(0.0).gap(6.0).center_y();
    let mut count = 0usize;
    let mut sep = |s: El, n: &mut usize| -> El {
        let out = if *n > 0 { s.child(words("\u{b7}".into()).key(format!("dot-{n}"))) } else { s };
        *n += 1;
        out
    };
    if let Some(st) = status {
        let end = if running { run_clock(host) } else { info.idle.unwrap_or(info.updated) };
        summary = sep(summary, &mut count).child(words(st.to_owned()).key("status"));
        summary = sep(summary, &mut count).child(run_time(format!("bar-time-{id}"), end - info.created.min(end), BAR, p.faint));
    }
    if !info.agent.trim().is_empty() {
        summary = sep(summary, &mut count).child(words(model::capitalize(info.agent.trim())).key("agent"));
    }
    if let Some(m) = host.subagent_model(id) {
        summary = sep(summary, &mut count).child(model_view(p, m));
    }
    let mut line = row()
        .key("subagent-bar")
        .w_full()
        .max_w(session_view::style::dock_w())
        .h(32.0)
        .pl(12.0)
        .pr(6.0)
        .gap(8.0)
        .center_y()
        .fill(p.bg_base);
    if status.is_some() {
        let mark = el().key("status-icon").size(16.0, 16.0).center();
        line = line.child(match glyph {
            Some(g) => mark.child(icon(g, 14.0).color(p.icon_faint)),
            None => mark.child(activity_ring_sized(p, 16.0)),
        });
    }
    Some(col().key("subagent-status").w_full().center().child(line.child(summary).child(back)))
}
