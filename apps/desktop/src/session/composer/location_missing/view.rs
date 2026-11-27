//! What the card draws (`location-missing.tsx`, `location-missing.css`): a raised body with the
//! warning, the folder that is gone and what to do, over a footer tray with Choose worktree (a
//! Git project's) and Choose directory. The button whose move runs reads "Moving session…".

use opencode_i18n::t;
use opencode_ui::palette::{Elevate as _, Pal, kit};
use opencode_ui::style::dock_w;
use rune::{El, TextStyle, col, el, icon, row, text};
use rune_kit::Loader;
use rune_theme_opencode::icons as ic;
use session_view::dock::{Variant, button};

use super::card::{Card, Source};
use super::{CHOOSE, LOCATION_MISSING, WORKTREE};
use crate::App;

/// `location-title`: 13 on the base line height at 530.
const TITLE: TextStyle = TextStyle::new(13.0, 530.0).lh(20.0);
/// `location-path`: the code face at 12.
const PATH: TextStyle = TextStyle::new(12.0, 400.0).mono().lh(20.0);
/// `location-description`: 13 at 440.
const COPY: TextStyle = TextStyle::new(13.0, 440.0).lh(20.0);
/// `button-v2`'s label.
const BUTTON: TextStyle = TextStyle::new(13.0, 530.0).lh(20.0).tracking(-0.04).features("\"tnum\"");
/// How far the tray tucks up under the body (`margin-block-start: -9px`).
const TUCK: f32 = 9.0;

/// `SessionLocationMissing`, in the composer's place.
pub(crate) fn panel(s: &App, p: &Pal) -> El {
    let st = s.kernel.slots.get(&LOCATION_MISSING);
    // A frame before the poll that starts its steps draws it at rest.
    let Some((subject, card)) = st.card.clone().or_else(|| super::subject(s).map(|subject| (subject, Card::default()))) else { return el() };
    // `light-dark(yellow-800, yellow-700)`.
    let yellow = opencode_ui::palette::token(p.dark, if p.dark { "yellow-700" } else { "yellow-800" }).unwrap_or(p.warning);
    let description = t!("session.location.description");
    let status = row()
        .key("location-status")
        .w_full()
        .items_start()
        .gap(8.0)
        .padding(4.0)
        .child(icon(&ic::WARNING, 16.0).color(yellow))
        .child(
            col()
                .key("location-details")
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(4.0)
                .child(text(t!("session.location.unavailable")).key("location-title").role("status").font(TITLE).color(p.text).wrap())
                .child(text(subject.directory.clone()).key("location-path").font(PATH).color(p.faint).wrap_anywhere().selectable(true).selection_style(p.field_selection))
                .child(text(description).key("location-description").font(COPY).color(p.muted).wrap()),
        );
    let body = col().key("location-body").w_full().padding(8.0).radius(10.0).fill(p.bg_base).elevate(p.raised).child(status);

    let git = subject.project.as_deref().is_some_and(opencode_kit::model::project_git);
    let live = !card.disabled();
    let worktree = git.then(|| match card.pending() {
        Some(Source::Worktree) => moving(p, WORKTREE),
        _ => {
            let b = button(p, WORKTREE, t!("session.location.worktree"), Variant::Neutral, None, live);
            if live { b.on_press_tx(super::choose_worktree) } else { b }
        }
    });
    let choose = match card.pending() {
        Some(Source::Directory) => moving(p, CHOOSE),
        _ => {
            let b = button(p, CHOOSE, t!("session.location.choose"), Variant::Submit, None, live);
            if live { b.on_press_tx(super::choose_directory) } else { b }
        }
    };
    // The tray (`location-footer`): `layer-01` with a 0.5 border, under the body's bottom edge so
    // the body's shadow falls on it (the Classic question's tray, `dock/question/classic.rs`).
    let footer = row()
        .key("location-footer")
        .w_full()
        .mt(-TUCK)
        .pt(16.0)
        .pb(8.0)
        .px(12.0)
        .gap(8.0)
        .flex_wrap()
        .center_y()
        .justify_end()
        .corners([0.0, 0.0, 10.0, 10.0])
        .fill(p.layer1)
        .border_sides([0.5, 0.5, 0.5, 0.5], p.border)
        .border_layout()
        .paint_order(-1)
        .children(worktree)
        .child(choose);
    col()
        .key("session-location-missing")
        .role("group")
        .labelled_by("location-title")
        .description(format!("{} {description}", subject.directory))
        .w_full()
        .max_w(dock_w())
        .child(body)
        .child(footer)
}

/// A button while its move runs (`variant="loading"`): the loader, then "Moving session…".
fn moving(p: &Pal, key: &str) -> El {
    let label = t!("session.location.moving");
    row()
        .key(key.to_owned())
        .role("button")
        .label_static(label)
        .h(32.0)
        .px(15.0)
        .gap(6.0)
        .center_y()
        .radius(6.0)
        .fill(p.layer2)
        .box_shadow(Pal::ring_in(p.border_muted))
        .disabled(true)
        .focusable(false)
        .child(Loader::new().size(16.0).build(kit(p)))
        .child(text(label).font(BUTTON).color(p.text))
}
