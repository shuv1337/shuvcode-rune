//! The page a window shows (Desktop's `LayoutRoute`, `shell/state/layout.tsx:72-78`): Home, a
//! draft tab's new-session page, or a session on one of the servers the app keeps live at once
//! (`/server/<base64 server>/session/<id>`).

use opencode_kit::servers::ServerKey;

impl crate::App {
    /// Desktop's `shell/project-navigation.ts` (#64727): Customize's side tabs, with retired
    /// Projects gone and Consolidated keeping Home. All navigation entry points use the same
    /// predicate, including when the vertical sidebar is collapsed.
    pub(crate) fn project_navigation(&self) -> bool {
        let width = self.layout.narrow.width();
        !cfg!(target_arch = "wasm32") && (width == 0.0 || width >= 768.0)
            && self.tab_layout() == crate::layout_state::TabLayout::Vertical
            && self.settings.sidebar_organize.effective() == crate::settings::Organize::Customize
    }

    pub(crate) fn restore_project_tab(&self) -> Option<&crate::tabs::Tab> {
        self.current_tab().and_then(|id| self.tab(&id))
            .or_else(|| self.gestures.home_return_tab.as_ref().and_then(|id| self.tab(id)))
            .or_else(|| self.tabs.last())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Route {
    #[default]
    Home,
    /// Experimental Automations screen (`/automations`, opencode#66393).
    Automations,
    /// Session `id` on `server`. The window works against `server` while it shows it
    /// (`servers::runtime`'s checkout).
    Session { server: ServerKey, id: String },
    /// "New session": an empty page with the composer, shown by the draft tab with this id.
    Draft(String),
}

impl Route {
    /// Session `id` on the server the window is working against now (the checkout), the server
    /// every session id this turn reads or hears comes from.
    pub fn here(id: String) -> Route {
        Route::Session { server: crate::servers::runtime::current(), id }
    }

    pub fn session(server: ServerKey, id: impl Into<String>) -> Route {
        Route::Session { server, id: id.into() }
    }

    /// Whether it shows session `id` (a session route is always on the checked-out server).
    pub fn is_session(&self, id: &str) -> bool {
        matches!(self, Route::Session { id: open, .. } if open == id)
    }

    /// The server a session route is on.
    pub fn server(&self) -> Option<&ServerKey> {
        match self {
            Route::Session { server, .. } => Some(server),
            Route::Home | Route::Automations | Route::Draft(_) => None,
        }
    }
}
