//! Typing or pasting on a page with a composer when nothing that takes text has focus goes to the
//! composer, as Desktop's `session/composer/region.tsx` does: a printable key focuses it and lands
//! at the end, and ⌘V pastes there (files and images attach). Every other handler sees the key
//! first, so shortcuts, focused fields and open menus keep theirs.

use rune::{ClipboardContent, Key, KeyEvent, Tx};
use session_view::composer::ComposerHost;

use crate::App;

/// Runs last in `keys::on_key`; true when the key went to the composer.
pub(crate) fn key(s: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if !s.takes_stray_keys() {
        return false;
    }
    if ev.is_command('v') && !ev.mods.alt && !ev.mods.shift {
        s.focus_for_stray_key(tx);
        tx.read_clipboard(|s: &mut App, content, tx| paste(s, content, tx));
        return true;
    }
    // Space pages the timeline in Desktop; it never starts a prompt.
    let Key::Text(text) = &ev.key else { return false };
    if ev.mods.cmd || ev.mods.ctrl || ev.mods.logo {
        return false;
    }
    if text.is_empty() || text == " " || text.chars().any(char::is_control) {
        return false;
    }
    s.focus_for_stray_key(tx);
    s.composer.key(ev, None);
    edited(s, tx);
    true
}

/// The composer's own paste, from outside it: files and images attach, text lands at the caret.
fn paste(s: &mut App, content: &ClipboardContent, tx: &mut Tx) {
    if !s.has_composer() || crate::free_limit::replaces_composer(s) || crate::location_missing::replaces_composer(s) {
        return;
    }
    if session_view::composer::attachments::paste(s, content, tx) {
        return;
    }
    if content.text().is_empty() {
        return;
    }
    match content.html().filter(|_| s.composer.is_rich()) {
        Some(html) => s.composer.paste_html(html, content.text()),
        None => s.composer.paste(content.text()),
    };
    edited(s, tx);
}

fn edited(s: &mut App, tx: &mut Tx) {
    tx.instant();
    s.composer.mark_edited();
    s.composer_on_change(tx);
}

impl App {
    /// A page whose composer shows, with no field, panel, menu or dialog that keys belong to.
    pub(crate) fn takes_stray_keys(&self) -> bool {
        let menus = self.kernel.slots.get(&crate::session_menu::MENUS);
        self.has_composer()
            && !crate::free_limit::replaces_composer(self)
            && !crate::location_missing::replaces_composer(self)
            && !self.settings_open()
            && !self.kernel.slots.get(&crate::panels::SIDE).focus.is_focused()
            && !self.kernel.slots.get(&crate::switcher::SWITCHER).is_open()
            && !self.kernel.slots.get(&crate::project_switcher::PROJECT_SWITCHER).is_open()
            && !self.kernel.slots.get(&crate::fork::FORK).is_open()
            && !self.kernel.slots.get(&crate::message_nav::JUMP).open()
            && !self.kernel.slots.get(&crate::find::FIND).input.focused
            && !self.search.focused
            && !crate::plugin_host::field_focused(self)
            && menus.on.is_none()
            && menus.renaming().is_none()
            && menus.confirming().is_none()
            && self.composer_ext.pickers.open().is_none()
            && self.kernel.slots.get(&crate::rewind::REWIND).menu.is_none()
            && !self.popup.is_open()
            && !self.project_state.home_menu_open
            && !self.kernel.slots.get(&crate::subagents::SHELLS).activity.is_open()
            && !*self.kernel.slots.get(&crate::todos::TODO_OPEN)
            && !self.kernel.slots.get(&crate::connect::CONNECT).is_modal_open()
            && self.kernel.slots.get(&crate::mcp::MCP).open.is_none()
            && !self.plugins.dialog_open()
            && !self.timeline_fx.diagram.is_open()
            && !crate::deep_links::asking(self)
            && !self.question_shown()
            && !self.dock_shown()
    }

    /// Focuses the composer with its caret at the end, unless it already has focus (a second
    /// key before the field took it lands after the first).
    fn focus_for_stray_key(&mut self, tx: &mut Tx) {
        if !self.composer.focused {
            let end = self.composer.text.len();
            self.composer.set_selection(end, end);
        }
        self.focus_composer(tx);
    }
}

