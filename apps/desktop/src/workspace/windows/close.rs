//! ⌘W and ⇧⌘W, as Safari and Chrome have them. ⌘W closes the tab in front; a window with no
//! tab to close (an empty one showing Home) closes instead, unless it's the app's only window,
//! where ⌘W does nothing. ⇧⌘W always closes the window. On macOS the last window hides rather
//! than closing, so the app keeps running and its Dock icon brings the window back (Rune's
//! reopen shows the window it hid last).

use rune::Tx;

use super::OPEN;
use crate::App;

/// What ⌘W does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseKey {
    Tab(String),
    Window,
    Nothing,
}

/// What Close Window does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseWindow {
    Close,
    /// The last window on macOS: it goes from the screen, and the app keeps running.
    Hide,
}

/// ⌘W: the tab in front; else, in a window with no tabs while another window is open, the window.
pub fn close_key(current_tab: Option<String>, has_tabs: bool, other_windows: bool) -> CloseKey {
    match current_tab {
        Some(id) => CloseKey::Tab(id),
        None if !has_tabs && other_windows => CloseKey::Window,
        None => CloseKey::Nothing,
    }
}

/// ⇧⌘W: the window closes, except the last one on macOS, which hides.
pub fn close_window(other_windows: bool, mac: bool) -> CloseWindow {
    if mac && !other_windows && !rune::quitting() { CloseWindow::Hide } else { CloseWindow::Close }
}

impl App {
    /// Whether another window of the app is open (a torn-out tab's window still following the
    /// pointer doesn't count).
    pub(crate) fn other_windows(&self) -> bool {
        let id = &rune::Tracked::untracked(&self.window).id;
        OPEN.with(|o| o.borrow().iter().any(|w| w.id != *id && w.life.saved()))
    }

    /// ⌘W and File › Close Tab.
    pub(crate) fn close_tab_key(&mut self, tx: &mut Tx) {
        match close_key(self.current_tab(), !self.tabs.is_empty(), self.other_windows()) {
            CloseKey::Tab(id) => self.close_tab(&id, tx),
            CloseKey::Window => self.close_window(tx),
            CloseKey::Nothing => {}
        }
    }

    /// ⇧⌘W and Close Window.
    pub(crate) fn close_window(&mut self, tx: &mut Tx) {
        match close_window(self.other_windows(), cfg!(target_os = "macos")) {
            CloseWindow::Close => tx.close_window(),
            // A run without the window list (headless) has nothing to hide with.
            CloseWindow::Hide => match self.app_handle() {
                Some(app) => {
                    self.window.hidden = true;
                    self.sync_visible_browser_tabs();
                    app.hide();
                }
                None => tx.close_window(),
            },
        }
    }

    /// ⌥⇧⌘W and File › Close All (AppKit's alternate of Close Window): every other window closes,
    /// and this one goes as Close Window takes the last (on macOS it hides; the app keeps running).
    pub(crate) fn close_all_windows(&mut self, tx: &mut Tx) {
        let id = self.window.id.clone();
        let others: Vec<_> = OPEN.with(|o| o.borrow().iter().filter(|w| w.id != id && w.life.saved()).map(|w| w.app.clone()).collect());
        for app in others {
            app.close();
        }
        match self.app_handle() {
            Some(app) if cfg!(target_os = "macos") && !rune::quitting() => {
                self.window.hidden = true;
                self.sync_visible_browser_tabs();
                app.hide();
            }
            _ => tx.close_window(),
        }
    }
    /// A menu item acts in this window: one the last ⇧⌘W hid comes back first. True when it did.
    pub(crate) fn reveal(&mut self) -> bool {
        if !std::mem::take(&mut self.window.hidden) {
            return false;
        }
        if let Some(app) = self.app_handle() {
            app.open();
        }
        true
    }

    fn app_handle(&self) -> Option<rune::AppHandle<App>> {
        let id = &self.window.id;
        OPEN.with(|o| o.borrow().iter().find(|w| &w.id == id).map(|w| w.app.clone()))
    }
}
