//! The composer's + menu and what it opens, as in Desktop (`composer/editor/editor.tsx`
//! `ComposerEditorAddMenu`, `suggestions/machine.ts`, `composer/submit.ts`): ⌘U's file picker,
//! the `/` command popover, `@` context and `!` shell mode.
//!
//! A `/name` the server knows sends its arguments to `POST …/command`; shell mode sends the
//! composer's text to `POST …/shell` and can't wait in the queue.

use opencode_ui::focus::FocusExt;
use std::collections::HashMap;

use rune::{Color, El, Key, KeyEvent, Tx, col, el, icon, row, spacer, text};
use rune_theme_opencode::icons as ic;
use session_view::composer::mode::{self, Effect, Event, Mode};

use opencode_kit::client::{Prompt, Write};
use opencode_kit::model::Delivery;

use crate::palette::{Elevate as _, Pal};
use crate::view::UI;
use crate::slash::Slash;
use crate::{App, Route, Source};
use opencode_i18n::t;

const SHORTCUT: rune::TextStyle = rune::TextStyle::new(11.0, 530.0).lh(11.0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Item {
    Attach,
    Commands,
    Context,
    Shell,
}

impl Item {
    pub const ALL: [Item; 4] = [Item::Attach, Item::Commands, Item::Context, Item::Shell];

    fn key(self) -> &'static str {
        match self {
            Item::Attach => "attach",
            Item::Commands => "commands",
            Item::Context => "context",
            Item::Shell => "shell",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Item::Attach => t!("ui.promptInput.attachments"),
            Item::Commands => t!("ui.promptInput.commands"),
            Item::Context => t!("ui.promptInput.context"),
            Item::Shell => t!("ui.promptInput.shell"),
        }
    }

    fn shortcut(self) -> String {
        match self {
            Item::Attach => crate::keymap::Bind::Attach.label(),
            Item::Commands => "/".into(),
            Item::Context => "@".into(),
            Item::Shell => "!".into(),
        }
    }
}

/// The + menu, as Desktop's dropdown menu machine runs it (`ui/src/navigation/menu/machine.ts`):
/// shut, or open with the item hovered or moved to, if any, and whether the keys opened it
/// (`MenuOpenModality`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Menu {
    #[default]
    Closed,
    Open { active: Option<Item>, keyboard: bool },
}

/// Why the menu closed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Close {
    /// Escape: focus goes back to the +, its ring only for a menu the keys opened
    /// (`selectMenuRestoreTriggerWithoutRing`, `pointerFocusReturn` in `editor.tsx`).
    Keys,
    /// A press outside: focus stays where it landed.
    Outside,
    /// A pick: what the item does decides focus.
    Picked,
}

pub enum MenuEvent {
    Opened { keyboard: bool },
    Lit(Item),
    Step(i64),
    Closed(Close),
}

#[derive(Debug, PartialEq, Eq)]
pub enum MenuEffect {
    FocusTrigger { ring: bool },
}

pub fn step_menu(menu: &mut Menu, event: MenuEvent) -> Option<MenuEffect> {
    match (*menu, event) {
        (Menu::Closed, MenuEvent::Opened { keyboard }) => {
            *menu = Menu::Open { active: None, keyboard };
            None
        }
        (Menu::Open { keyboard, .. }, MenuEvent::Lit(item)) => {
            *menu = Menu::Open { active: Some(item), keyboard };
            None
        }
        (Menu::Open { active, keyboard }, MenuEvent::Step(d)) => {
            let n = Item::ALL.len() as i64;
            let at = active.and_then(|i| Item::ALL.iter().position(|x| *x == i)).map_or(if d > 0 { -1 } else { 0 }, |p| p as i64);
            *menu = Menu::Open { active: Some(Item::ALL[(at + d).rem_euclid(n) as usize]), keyboard };
            None
        }
        (Menu::Open { keyboard, .. }, MenuEvent::Closed(why)) => {
            *menu = Menu::Closed;
            (why == Close::Keys).then_some(MenuEffect::FocusTrigger { ring: keyboard })
        }
        _ => None,
    }
}

#[derive(Default)]
pub struct AddMenu {
    pub menu: Menu,
    /// The open draft's mode: prompts or shell commands (see [`mode`]).
    pub mode: Mode,
    /// The other drafts' shell modes, by draft key: Desktop keeps the mode in each draft.
    parked: HashMap<String, Mode>,
    pub slash: Slash,
}

pub(crate) static ADD_MENU: opencode_kernel::Slot<AddMenu> = opencode_kernel::Slot::new();

impl AddMenu {
    pub fn open(&self) -> bool {
        matches!(self.menu, Menu::Open { .. })
    }

    fn active(&self) -> Option<Item> {
        match self.menu {
            Menu::Open { active, .. } => active,
            Menu::Closed => None,
        }
    }

    /// The composer sends shell commands instead of prompts.
    pub fn shell(&self) -> bool {
        self.mode == Mode::Shell
    }

    /// Draft `key`'s mode, kept while another draft shows (or come back from the last run).
    pub fn parked(&self) -> impl Iterator<Item = (&str, Mode)> {
        self.parked.iter().map(|(k, m)| (k.as_str(), *m))
    }

    /// Draft `key` keeps `mode` until it shows: the saved drafts at launch.
    pub fn park(&mut self, key: String, mode: Mode) {
        if mode == Mode::Shell { self.parked.insert(key, mode); } else { self.parked.remove(&key); }
    }

    /// The composer moves from draft `from` to draft `to`: each keeps its own mode.
    pub fn swap_draft(&mut self, from: Option<&str>, to: Option<&str>) {
        if let Some(from) = from {
            if self.shell() { self.parked.insert(from.to_owned(), Mode::Shell); } else { self.parked.remove(from); }
        }
        let next = to.and_then(|k| self.parked.remove(k)).unwrap_or_default();
        mode::step(&mut self.mode, Event::Synced(next));
    }
}

impl App {
    /// The open page has a composer: a draft, or a session that isn't a subagent's.
    pub fn has_composer(&self) -> bool {
        match &*self.route {
            Route::Draft(_) => true,
            Route::Session { id, .. } => self.session(id).is_none_or(|i| i.parent.is_none()),
            Route::Home | Route::Automations => false,
        }
    }

    pub fn add_menu(&mut self, event: MenuEvent, tx: &mut Tx) {
        if let Some(MenuEffect::FocusTrigger { ring }) = step_menu(&mut self.kernel.slots.get_mut(&ADD_MENU).menu, event) {
            self.composer.focused = false;
            tx.focus("add", ring);
        }
    }

    pub fn open_add_menu(&mut self, tx: &mut Tx) {
        if self.kernel.slots.get(&ADD_MENU).shell() {
            return;
        }
        let keyboard = tx.focus_visible();
        self.add_menu(MenuEvent::Opened { keyboard }, tx);
    }

    pub fn close_add_menu(&mut self, why: Close, tx: &mut Tx) {
        self.add_menu(MenuEvent::Closed(why), tx);
    }

    pub fn choose_add(&mut self, item: Item, tx: &mut Tx) {
        tx.instant();
        self.close_add_menu(Close::Picked, tx);
        match item {
            Item::Attach => self.pick_attachment(tx),
            Item::Commands => self.open_commands(tx),
            Item::Context => self.open_context(tx),
            Item::Shell => {
                tx.animated();
                self.enter_shell(tx);
            }
        }
    }

    /// Steps the composer's mode and runs what it asks; true when it took the key.
    pub fn composer_mode(&mut self, event: Event) -> bool {
        let was = self.kernel.slots.get(&ADD_MENU).shell();
        let step = mode::step(&mut self.kernel.slots.get_mut(&ADD_MENU).mode, event);
        for effect in step.effects {
            match effect {
                Effect::ClosePopovers => self.kernel.slots.get_mut(&ADD_MENU).menu = Menu::Closed,
                Effect::ClearDraft => self.composer.clear(),
            }
        }
        // The suggestion lists hear the mode when it changes (`mode.shell`, `mode.normal`), not
        // on every keystroke, which would close the @ list a shell command's `@` opened.
        if self.kernel.slots.get(&ADD_MENU).shell() != was {
            self.suggest_mode();
        }
        step.handled
    }

    pub fn enter_shell(&mut self, tx: &Tx) {
        self.composer_mode(Event::Shell);
        if crate::auto_focus_composer(tx) {
            self.composer.focused = true;
        }
    }

    pub fn exit_shell(&mut self) {
        self.composer_mode(Event::Normal);
    }

    /// The draft became exactly `!`: prompt mode switches to shell (`inputChanged`).
    pub fn composer_typed(&mut self) -> bool {
        let bang = self.composer.text == "!" && self.composer.preedit().is_none();
        self.composer_mode(Event::Input { bang })
    }

    /// Keys for the + menu, ⌘U, shell mode and the command popover. The composer's handler
    /// and the window's both call it.
    pub fn add_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        if let Menu::Open { active, .. } = self.kernel.slots.get(&ADD_MENU).menu {
            match ev.key {
                Key::Escape => {
                    tx.instant();
                    self.close_add_menu(Close::Keys, tx);
                }
                Key::Down => self.add_menu(MenuEvent::Step(1), tx),
                Key::Up => self.add_menu(MenuEvent::Step(-1), tx),
                Key::Enter => {
                    if let Some(item) = active {
                        self.choose_add(item, tx);
                    }
                }
                _ => {}
            }
            return true;
        }
        if crate::keymap::Bind::Attach.matches_tx(ev, tx) && self.has_composer() {
            if !self.kernel.slots.get(&ADD_MENU).shell() {
                self.pick_attachment(tx);
            }
            return true;
        }
        if !self.composer.focused {
            return false;
        }
        // With no list open, Escape and Backspace reach the machine, which leaves shell mode.
        let event = match ev.key {
            Key::Escape => Event::Escape,
            Key::Backspace => Event::Backspace { empty: self.composer.text.is_empty() },
            _ => return false,
        };
        if self.composer.preedit().is_some() || self.kernel.slots.get(&crate::suggestions::SUGGEST).state.is_open() || !self.composer_mode(event) {
            return false;
        }
        // The + button and the pickers fade back in (`editor.tsx:178-182`).
        tx.animated();
        true
    }

    /// Runs a `/command` prompt: in the open session (steered or queued while it works, by
    /// the server), or in a new one from a draft.
    pub fn send_command(&mut self, mut prompt: Prompt, alternate: bool, tx: &mut Tx) {
        match (*self.route).clone() {
            Route::Session { .. } if matches!(*self.source, Source::Fixture) => {
                crate::status_toast::say(self, t!("rune.composer.commandsNeedServer"), t!("rune.composer.needServer.description"));
                return;
            }
            Route::Session { id, .. } => {
                if self.busy(&id) {
                    let queue = alternate != (self.settings.follow_up == crate::settings::FollowUp::Queue);
                    prompt.delivery = Some(if queue { Delivery::Queue } else { Delivery::Steer });
                }
                let staged = self.session(&id).and_then(|i| i.revert.as_ref()).is_some();
                let w = if staged {
                    Write::SendFromRevert { session: id.clone(), prompt }
                } else {
                    Write::Send { session: id.clone(), prompt }
                };
                if !self.write(w) {
                    return;
                }
                if staged {
                    self.commit_staged_revert(&id);
                }
            }
            Route::Draft(_) if self.live() => {
                if !self.write(Write::Start { directory: self.draft_dir().to_owned(), prompt }) {
                    return;
                }
                *self.diag.status = t!("rune.composer.starting").into();
            }
            _ => {
                crate::status_toast::say(self, t!("rune.composer.openSession"), "");
                return;
            }
        }
        tx.instant();
        self.composer.clear();
        self.composer_ext.attach.files.clear();
    }

    /// Sends shell mode's command: now, in the open session, never queued.
    pub fn submit_shell(&mut self, alternate: bool, tx: &mut Tx) {
        let command = self.composer.text.trim().to_owned();
        if command.is_empty() {
            return;
        }
        let Route::Session { id, .. } = (*self.route).clone() else {
            crate::status_toast::say(self, t!("rune.composer.shell.needsSession"), t!("rune.composer.shell.needsSession.description"));
            return;
        };
        if alternate && self.busy(&id) {
            crate::status_toast::say(self, t!("rune.composer.shell.noQueue"), t!("rune.composer.shell.noQueue.description"));
            return;
        }
        if matches!(*self.source, Source::Fixture) {
            crate::status_toast::say(self, t!("rune.composer.shell.needsServer"), t!("rune.composer.needServer.description"));
            return;
        }
        let w = Write::Shell { session: id, id: Some(opencode_kit::client::message_id()), command };
        if self.write(w) {
            tx.instant();
            self.composer.clear();
            self.exit_shell();
        }
    }
}

/// The + button at the start of the toolbar: `icon-button-v2` ghost-muted at size large
/// (`editor.tsx:977-987`, `icon-button.css:36-39,115-142`), pressed with its icon in
/// `icon-icon-base` while the menu is open. Shell mode fades it out with the pickers
/// ([`shell_hidden`]). The pickers start 4 after it (`ms-1`): the toolbar's 2 pt gap and its
/// own 2.
pub fn trigger(s: &App, p: &Pal) -> El {
    let open = s.kernel.slots.get(&ADD_MENU).open();
    let mut b = el()
        .key("add")
        .role("button")
        .label_static(t!("ui.promptInput.add"))
        .expanded(open)
        .size(28.0, 28.0)
        .mr(2.0)
        .radius(6.0)
        .center()
        .focus_inset(p)
        .hover_motion(session_view::style::INSTANT, session_view::style::INSTANT)
        .child(icon(&ic::PLUS, 16.0).color(if open { p.icon } else { p.icon_muted }))
        .on_press_tx(|s: &mut App, tx: &mut Tx| {
            if s.kernel.slots.get(&ADD_MENU).open() { s.close_add_menu(Close::Outside, tx) } else { s.open_add_menu(tx) }
            crate::popover::motion::toggled(&s.diag.knobs, tx, s.kernel.slots.get(&ADD_MENU).open());
        });
    if crate::popup::PRESS_ON_DOWN {
        b = b.press_on_down();
    }
    if open {
        b = b.fill(p.pressed);
    } else {
        // Tooltip hides while menu is open (`editor.tsx:1011`).
        b = b
            .help(t!("ui.promptInput.add"))
            .help_keys(crate::keymap::Bind::Attach.keycaps())
            .help_above()
            .hover(p.hover)
            .press(p.pressed);
    }
    shell_hidden(s, b)
}

/// The + button and the pickers in shell mode: still in their places, faded out over 200 ms
/// and out of reach of the pointer, Tab and assistive technology (`editor.tsx:178-182`,
/// `aria-hidden` and `inert` at 426-460).
pub fn shell_hidden(s: &App, el: El) -> El {
    let el = el.opacity_motion(rune::Anim::new(rune::Curve::tween(0.2, rune::Easing::EASE)));
    if s.kernel.slots.get(&ADD_MENU).shell() { el.opacity(0.0).inert().a11y_hidden() } else { el.opacity(1.0) }
}

/// "Exit Esc" before Send in shell mode (`editor.tsx:499-516`): `button-v2` ghost-faint at size
/// small (`button.css:46-50,304-326`), 24 tall, 6 in and the word 6 from the keys, 4 pt
/// corners, 12 before Send (`me-3`: the toolbar's 2 pt gap and 10), its ring 2 inside
/// (`editor.css:70-73`). The keys show only in a window at least 640 wide (`hidden sm:block`).
/// It goes back to prompts and the caret to the composer.
pub fn exit_shell_button(p: &Pal, wide: bool) -> El {
    let word = text(t!("ui.promptInput.exitShell")).font(crate::view::UI_MED).color(p.faint);
    row()
        .key("exit-shell")
        .role("button")
        .label_static(t!("ui.promptInput.exitShell"))
        .h(24.0)
        .px(6.0)
        .mr(10.0)
        .gap(6.0)
        .center_y()
        .radius(4.0)
        .hover(p.hover)
        .press(p.pressed)
        .focus_inset(p)
        // It shows and goes at once with all it holds, as Desktop's `<Show>` does.
        .transition(rune::Transition::none())
        .instant_changes()
        .on_press_tx(|s: &mut App, tx: &mut Tx| {
            s.exit_shell();
            if crate::auto_focus_composer(tx) {
                s.composer.focused = true;
            }
        })
        .child(word)
        .children(wide.then(|| crate::keymap::chips(p, vec![t!("common.key.esc").to_owned()])))
}

/// The menu over the + button, and the scrim that closes it as siblings so the anchored menu
/// keeps its overlay paint rank above composer suggestion popovers (`update_layer_paint_order`).
pub fn menu(s: &App, p: &Pal) -> Option<[El; 2]> {
    if !s.kernel.slots.get(&ADD_MENU).open() {
        return None;
    }
    // `menu-v2-content` (`menu.css:6-21`): layer 1, a 2 pt inset, 6 pt corners and
    // `--v2-elevation-floating`.
    let mut menu = col()
        .key("add-menu")
        .role("menu")
        .label_static(t!("ui.common.add"))
        .above("add", 6.0)
        .min_w(180.0)
        .padding(2.0)
        .radius(6.0)
        .fill(p.layer1)
        .elevate(p.floating)
        .transition(crate::popover::motion::transition(&s.diag.knobs, rune::Side::Top, rune::Align::Start))
        // What it holds comes in with it, not on fades of its own.
        .instant_changes()
        .on_press(|_: &mut App| {});
    for item in Item::ALL {
        if item == Item::Commands {
            // `menu-v2-separator`: edge to edge, through the menu's 2 pt inset.
            menu = menu.child(el().key("add-separator").h(1.0).mt(2.0).mb(2.0).ml(-2.0).mr(-2.0).fill(p.border_muted));
        }
        let active = s.kernel.slots.get(&ADD_MENU).active() == Some(item);
        menu = menu.child(
            row()
                .key(format!("add-{}", item.key()))
                .role("menuitem")
                .label(item.label())
                .h(28.0)
                .w_full()
                .pl(12.0)
                .pr(6.0)
                .gap(8.0)
                .center_y()
                .radius(4.0)
                .fill(if active { p.hover } else { Color::TRANSPARENT })
                .press(p.pressed)
                .on_hover_change(move |s: &mut App, inside, tx: &mut Tx| {
                    if inside && s.kernel.slots.get(&ADD_MENU).open() {
                        tx.instant();
                        s.add_menu(MenuEvent::Lit(item), tx);
                    }
                })
                .on_press_tx(move |s: &mut App, tx: &mut Tx| s.choose_add(item, tx))
                .child(text(item.label()).font(UI).color(p.text))
                .child(spacer())
                .child(row().key("shortcut").min_w(20.0).center().child(text(item.shortcut()).font(SHORTCUT).color(p.faint))),
        );
    }
    let scrim = el().key("add-dismiss")
        .transition(rune::Transition::none())
        .absolute(0.0, 0.0)
        .w_full()
        .h_full()
        .focusable(false)
        .on_press_tx(|s: &mut App, tx: &mut Tx| {
            tx.instant();
            s.close_add_menu(Close::Outside, tx);
        });
    let scrim = if crate::popup::PRESS_ON_DOWN { scrim.press_on_down() } else { scrim };
    Some([scrim, menu])
}

