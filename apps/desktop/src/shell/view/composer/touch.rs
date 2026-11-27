//! `ComposerEditorTouchBar` (editor.tsx): the action bar belongs to the active editor, but is
//! laid out at the shell's foot, below the panel and bottom navigation. No second editor or
//! duplicate submit path. Its actions keep the field's focus so the keyboard doesn't collapse.
use super::*;
use crate::add_menu::Item;
use session_view::composer::{ready, stopping};

const LABEL: TextStyle = TextStyle::new(15.0, 530.0).lh(20.0);
const SYMBOL: TextStyle = TextStyle::new(17.0, 440.0).mono().lh(20.0);

pub(super) fn mounted(s: &App) -> bool {
    if !session_view::style::touch_controls() || s.settings_open() || s.fixture_run.stage.is_some() { return false; }
    match &*s.route {
        Route::Home | Route::Automations => false,
        Route::Draft(_) => true,
        Route::Session { id, .. } => (!s.narrow() || s.narrow_view() == crate::narrow::machine::CONVERSATION)
            && crate::session_state::view(s, id) == crate::session_state::View::Session
            && session_view::dock::shown(s).is_none(),
    }
}

#[derive(Clone, Copy)]
enum Delivery { Send, Stop, Steer, Queue }

impl Delivery {
    fn label(self) -> &'static str {
        match self {
            Self::Send => t!("ui.promptInput.send"), Self::Stop => t!("ui.promptInput.stop"),
            Self::Steer => t!("ui.promptInput.steer"), Self::Queue => t!("ui.promptInput.queue"),
        }
    }
}

pub(in crate::shell::view) fn bar(s: &App, p: &Pal) -> Option<El> {
    if !mounted(s) { return None; }
    let shell = s.kernel.slots.get(&crate::add_menu::ADD_MENU).shell();
    // `h-14` under its 1 pt rule, `px-2`; the page's `p-px` root (`view::shell`) keeps it 1 pt in.
    let mut bar = row().key("composer-touch-bar").w_full().min_h(57.0).h(57.0).pt(1.0).px(8.0).gap(4.0).center_y()
        .fill(p.bg_base).border_sides([1.0, 0.0, 0.0, 0.0], p.border);
    if shell {
        bar = bar.child(control(p, "exit-shell", t!("ui.promptInput.exitShell"))
            .px(12.0).on_press_tx(|s: &mut App, tx| {
                s.exit_shell();
                if crate::auto_focus_composer(tx) {
                    s.focus_composer(tx);
                }
            })
            .child(text(t!("ui.promptInput.exitShell")).font(LABEL).color(p.muted)));
    } else {
        for (key, label, item, symbol) in [
            ("attach", t!("ui.promptInput.add"), Item::Attach, None),
            ("commands", t!("ui.promptInput.commands"), Item::Commands, Some("/")),
            ("context", t!("ui.promptInput.context"), Item::Context, Some("@")),
            ("shell", t!("ui.promptInput.shell"), Item::Shell, Some("!")),
        ] {
            let mark = symbol.map_or_else(|| icon(&ic::PLUS, 16.0).color(p.icon_muted), |glyph| text(glyph).font(SYMBOL).color(p.icon_muted));
            bar = bar.child(control(p, key, label).w(44.0).on_press_tx(move |s: &mut App, tx| s.choose_add(item, tx)).child(mark));
        }
    }
    let sendable = ready(s) || s.composer_dictating();
    let editing = s.composer_editing();
    let (action, alternate) = if stopping(s) && !editing { (Delivery::Stop, None) }
        else if !shell && editing { (Delivery::Queue, sendable.then_some(Delivery::Steer)) }
        else if !shell && sendable && s.composer_busy() {
            if s.settings.follow_up == crate::settings::FollowUp::Queue { (Delivery::Queue, Some(Delivery::Steer)) }
            else { (Delivery::Steer, Some(Delivery::Queue)) }
        } else { (Delivery::Send, None) };
    bar = bar.child(spacer().min_w(0.0));
    if let Some(alt) = alternate {
        bar = bar.child(control(p, "alternate", alt.label()).radius(12.0).px(16.0).fill(p.layer1)
            .on_press_tx(|s: &mut App, tx| s.submit(true, tx)).child(text(alt.label()).font(LABEL).color(p.text)));
    }
    let submit = control(p, "touch-submit", action.label()).radius(12.0).px(16.0).gap(6.0);
    let stop = matches!(action, Delivery::Stop);
    let ink = if stop { p.text } else { p.q.submit_text };
    let submit = if stop { submit.fill(p.layer1) } else { session_view::composer::submit_surface(submit, p, sendable) };
    let mark = icon(if stop { &ic::STOP } else if shell { &ic::ARROW_UNDO_DOWN } else { &ic::ARROW_UP }, 16.0).color(ink);
    let submit = if stop { submit.child(mark).child(text(action.label()).font(LABEL).color(ink)) }
        else { submit.child(text(action.label()).font(LABEL).color(ink)).child(mark) };
    Some(bar.child(if sendable || stop { submit.on_press_tx(session_view::composer::submit_or_stop::<App>) } else { submit.disabled(true) }))
}

fn control(p: &Pal, key: &'static str, label: &'static str) -> El {
    row().key(key).role("button").label(label).h(44.0).min_w(44.0).center().radius(8.0)
        .preserve_focus_on_press(true).hover(p.hover).press(p.pressed)
}
