//! Desktop host adapter for `opencode-feature-panels` (`features/panels`).

pub use opencode_feature_panels::*;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

use opencode_kernel::{CommandEntry, Feature, KernelBuilder, Slot};
use opencode_ui::keymap::{Bind, Chord};
use rune::{Drag, El, Ime, KeyEvent, Tx};
use rune_knobs::Knobs;
use session_view::palette::Pal;

use crate::{App, Route};

pub(crate) static SIDE: Slot<SideState> = Slot::new();

impl PanelsHost for App {
    fn side(&self) -> &SideState {
        self.kernel.slots.get(&SIDE)
    }

    fn side_mut(&mut self) -> &mut SideState {
        self.kernel.slots.get_mut(&SIDE)
    }

    fn session_directory(&self, sid: &str) -> Option<&str> {
        self.session(sid).map(|s| s.directory.as_str())
    }

    fn session_project_id(&self, sid: &str) -> String {
        match self.session(sid) {
            Some(info) => self
                .project_for_session(info)
                .map(|p| p.id.clone())
                .or_else(|| (!info.project.is_empty() && info.project != "global").then(|| info.project.clone()))
                .unwrap_or_default(),
            None => String::new(),
        }
    }

    fn session_has_header(&self, sid: &str) -> bool {
        crate::session_state::view(self, sid) == crate::session_state::View::Session
    }

    fn settings_open(&self) -> bool {
        App::settings_open(self)
    }

    fn narrow(&self) -> bool {
        App::narrow(self)
    }

    fn stage_active(&self) -> bool {
        self.fixture_run.stage.is_some()
    }

    fn sidebar_shown_w(&self) -> f32 {
        App::sidebar_shown_w(self)
    }

    fn dock_placement(&self) -> DockPlacement {
        self.settings.terminal_placement
    }

    fn shared_side_panel(&self) -> bool {
        self.settings.shared_side_panel
    }

    fn panel_chrome(&self) -> PanelChrome {
        let c = crate::view::chrome();
        let mut h = DefaultHasher::new();
        (
            c.lights_end.to_bits(),
            c.row_h.to_bits(),
            c.row_pad.to_bits(),
            c.caption_w.to_bits(),
            c.hairline.to_bits(),
            std::mem::discriminant(&c.layout),
            c.narrow,
            c.bottom,
        )
            .hash(&mut h);
        PanelChrome { top: c.top(), foot: c.foot(), stamp: h.finish() }
    }

    fn knobs(&self) -> &Knobs {
        &self.diag.knobs
    }

    fn composer_focused(&self) -> bool {
        self.composer.focused
    }

    fn composer_focused_untracked(&self) -> bool {
        rune::Tracked::untracked(&self.composer).focused
    }

    fn set_composer_focused(&mut self, focused: bool) {
        self.composer.focused = focused;
    }

    fn popup_holds_focus(&self) -> bool {
        self.popup.holds_focus()
    }

    fn plugin_field_focused(&self, plugin: &str) -> bool {
        crate::plugin_host::editors::plugin_focused(self, plugin)
    }

    fn blur_panel_fields(&mut self) {
        crate::plugin_host::blur_fields(self);
    }

    fn command_chord(&self, id: &str) -> Option<Chord> {
        crate::keymap::command_chord(self, id)
    }

    fn command_strokes(&self, id: &str) -> Vec<Chord> {
        crate::keymap::command_strokes(self, id)
    }

    fn resolve_closed_icon(&self, key: &str, name: Option<&str>) -> &'static rune::Icon {
        let Some(name) = name else { return opencode_feature_panels::icon_by_name(None) };
        let plugin = key.split_once(':').map_or("", |(p, _)| p);
        crate::plugin_host::icons::resolve(plugin, name)
    }

    fn plugin_surfaces_version(&self) -> u64 {
        crate::plugin_host::surfaces_version(self)
    }

    fn panel_tabs_stamp(&self) -> u64 {
        use rune::Tracked;
        let slots = &self.kernel.slots;
        let mut h = DefaultHasher::new();
        // The strip's session and the routed one, which its panels list for.
        (self.panel_session_id(), self.active_session_id()).hash(&mut h);
        crate::servers::runtime::active(self).hash(&mut h);
        let server_base = match Tracked::untracked(&self.source) {
            crate::Source::Live { server, allow_send, .. } => {
                (true, server.base.as_str(), server.is_local(), *allow_send).hash(&mut h);
                server.base.as_str()
            }
            crate::Source::Fixture => {
                (false, "", false, false).hash(&mut h);
                "fixture"
            }
        };
        if let Some(sid) = self.panel_session_id() {
            let wk = crate::terminal::workspace_key(server_base, sid);
            match slots.untracked(&crate::terminal::TERMINAL).workspaces.get(&wk) {
                Some(ws) => {
                    (1u8, &ws.active, &ws.failure, ws.tabs.len()).hash(&mut h);
                    ws.tabs.iter().for_each(|t| (&t.id, &t.title, t.title_number).hash(&mut h));
                }
                None => 0u8.hash(&mut h),
            }
        }
        let pickers = Tracked::untracked(&self.composer_ext.pickers);
        (
            crate::plugin_host::surfaces_version(self),
            opencode_i18n::I18N.locale(),
            crate::plugin_host::services::servers_version(self),
            crate::updater::revision_untracked(self),
            pickers.models.len(),
            pickers.limits.len(),
            pickers.paid.len(),
        )
            .hash(&mut h);
        [
            (slots.cell_id(&SIDE), slots.version(&SIDE)),
            (Tracked::id(&self.workspace_panes.browser), Tracked::version(&self.workspace_panes.browser)),
            (slots.cell_id(&crate::files::FILES), slots.version(&crate::files::FILES)),
            (slots.cell_id(&crate::review::REVIEW), slots.version(&crate::review::REVIEW)),
            (slots.cell_id(&crate::line_comments::LINE_COMMENTS), slots.version(&crate::line_comments::LINE_COMMENTS)),
            (slots.cell_id(&crate::new_worktree::NEW_WORKTREE), slots.version(&crate::new_worktree::NEW_WORKTREE)),
            (Tracked::id(&self.diag.debug), Tracked::version(&self.diag.debug)),
            (Tracked::id(&self.sessions), Tracked::version(&self.sessions)),
            (Tracked::id(&self.projects), Tracked::version(&self.projects)),
            (Tracked::id(&self.running), Tracked::version(&self.running)),
            (Tracked::id(&self.inboxes), Tracked::version(&self.inboxes)),
            (Tracked::id(&self.context_windows), Tracked::version(&self.context_windows)),
            (Tracked::id(&self.transcripts), Tracked::version(&self.transcripts)),
            (Tracked::id(&self.settings), Tracked::version(&self.settings)),
            (Tracked::id(&self.plugins), Tracked::version(&self.plugins)),
            (Tracked::id(&self.layout), Tracked::version(&self.layout)),
            (Tracked::id(&self.workspace_panes.panel_sidebar), Tracked::version(&self.workspace_panes.panel_sidebar)),
            (Tracked::id(&self.default_dir), Tracked::version(&self.default_dir)),
        ]
            .hash(&mut h);
        h.finish()
    }

    fn panel_providers(&self) -> Vec<PanelRegistration> {
        crate::plugin_host::panels::providers(self)
    }

    fn panel_list(&self, ctx: &PanelSessionCtx<'_>) -> Vec<PanelTabDesc> {
        crate::plugin_host::panels::list(self, ctx)
    }

    fn panel_render(&self, p: &Pal, ctx: &PanelViewCtx<'_>) -> El {
        crate::plugin_host::panels::render(self, p, ctx)
    }

    fn panel_is_wide(&self, plugin: &str) -> bool {
        crate::plugin_host::panels::wide(self, plugin)
    }

    fn panel_launchers(&self, ctx: &PanelSessionCtx<'_>) -> Vec<PanelLauncherDesc> {
        crate::plugin_host::panels::launchers(self, ctx)
    }

    fn open_panel_entry(&mut self, plugin: &str, id: &str, pane: SessionPane, tx: &mut Tx) {
        crate::plugin_host::panels::open_entry(self, plugin, id, pane, tx);
    }

    fn ensure_panel_plugin(&mut self, plugin: &str) {
        crate::plugin_host::ensure_plugin_running(self, plugin);
    }

    fn hover_panel_entry(&mut self, plugin: &str, id: &str, tx: &mut Tx) {
        crate::plugin_host::panels::hover_entry(self, plugin, id, tx);
    }

    fn panel_focused(&mut self, plugin: &str, session: &str, tab: &str, restored: bool) {
        crate::plugin_host::panels::focused(self, plugin, session, tab, restored);
    }

    fn panel_closed(&mut self, plugin: &str, session: &str, tab: &str, tx: &mut Tx) {
        crate::plugin_host::panels::closed(self, plugin, session, tab, tx);
    }

    fn panel_closed_info(&self, plugin: &str, session: &str, tab: &PanelTabDesc) -> ClosedTabInfo {
        crate::plugin_host::panels::closed_info(self, plugin, session, tab)
    }

    fn panel_restore(&mut self, plugin: &str, session: &str, tab: &str, data: Option<&str>, tx: &mut Tx) -> bool {
        crate::plugin_host::panels::restore(self, plugin, session, tab, data, tx)
    }

    fn panel_tab_lead(&self, p: &Pal, plugin: &str, tab: &PanelTabDesc, selected: bool) -> Option<El> {
        crate::plugin_host::panels::tab_lead(self, p, plugin, tab, selected)
    }

    fn panel_tab_lead_stamp(&self, p: &Pal, plugin: &str, tab: &PanelTabDesc, selected: bool) -> Option<u64> {
        crate::plugin_host::panels::tab_lead_stamp(self, p, plugin, tab, selected)
    }

    fn panel_tab_extra_w(&self, plugin: &str, tab: &PanelTabDesc) -> f32 {
        #[cfg(not(target_arch = "wasm32"))]
        if plugin == crate::surfaces::browser::PLUGIN
            && tab.id.parse::<u64>().ok().is_some_and(|id| self.browser_tab_asking(id) || self.browser_tab_unseen(id))
        {
            return 12.0;
        }
        let _ = (plugin, tab);
        0.0
    }

    fn panel_home_pane(&self, plugin: &str) -> Option<SessionPane> {
        crate::plugin_host::panels::home_pane(self, plugin)
    }

    fn wake_panel_launchers(&mut self) -> bool {
        crate::plugin_host::dormant::wake_launchers(self)
    }

    fn handle_panel_ime(&mut self, ime: &Ime) -> bool {
        crate::plugin_host::native::on_ime(self, ime)
    }

    fn handle_panel_command_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        crate::plugin_host::commands::panel_key(self, ev, tx)
    }

    fn handle_panel_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        crate::plugin_host::native::on_key(self, ev, tx)
    }

    fn place_narrow(&mut self, session_id: &str, key: &str, was: (Visibility, bool)) -> bool {
        App::place_narrow(self, session_id, key, was)
    }

    fn on_side_tab_selected(&mut self, key: &str) {
        if !key.starts_with("file:") {
            self.kernel.slots.get_mut(&crate::files::FILES).filter.focused = false;
        }
    }

    fn on_side_tab_closing(&mut self, session_id: &str, key: &str) {
        if let Some(path) = key.strip_prefix("file:").and_then(crate::files::data::file_tab_path) {
            if let Some(ws) = self.kernel.slots.get_mut(&crate::files::FILES).sessions.get_mut(session_id) {
                ws.close_file(&path);
            }
            crate::files::watch::unwatch(self, session_id, &path);
        }
    }

    fn browser_tab_external_url(&self, id: u64) -> Option<String> {
        App::browser_tab_external_url(self, id)
    }

    fn open_browser_tab_externally(&mut self, id: u64, tx: &mut Tx) {
        App::open_browser_tab_externally(self, id, tx);
    }
}

impl App {
    pub fn all_panels(&self) -> Vec<PanelRegistration> {
        all_panels(self)
    }

    pub fn find_panel(&self, plugin: &str) -> Option<PanelRegistration> {
        find_panel(self, plugin)
    }

    pub fn active_session_id(&self) -> Option<&str> {
        match &*self.route {
            Route::Session { id, .. } => Some(id.as_str()),
            _ => None,
        }
    }

    /// The session whose side panel strip the routed session shows: its root's when a tab holds
    /// that root, so a subagent's page shows its parent's tabs and a tab picked there is the
    /// parent's too (official `session-layout.ts` `panelKey`, opencode#65957).
    pub fn panel_session_id(&self) -> Option<&str> {
        self.active_session_id().map(|id| self.panel_root(id))
    }

    /// [`Self::panel_session_id`] for session `sid` of the server the window works against
    /// (official `host-apis.tsx` `panelSessionID`: the root of the tab that holds it). A session
    /// whose parents aren't loaded, or whose root has no tab, keeps its own.
    pub fn panel_root<'a>(&'a self, sid: &'a str) -> &'a str {
        let server = crate::servers::runtime::active(self);
        let tab = |id: &str| self.tabs.iter().any(|t| t.id == id && !t.is_draft() && t.server == *server);
        // A tab's own session, the usual page, with no look at its family.
        if tab(sid) {
            return sid;
        }
        let root = crate::launch::saved::page_tab::root_of(self, sid);
        if root != sid && tab(root) { root } else { sid }
    }

    /// Whether session `sid`'s side panel is the one on the page (`AppEnv::panel_shows`).
    pub fn panel_shows(&self, sid: &str) -> bool {
        opencode_kernel::AppEnv::panel_shows(self, sid)
    }

    pub fn side_open(&self) -> bool {
        side_open(self)
    }

    pub fn side_stored_open(&self) -> bool {
        side_stored_open(self)
    }

    pub fn side_mounted(&self) -> bool {
        side_mounted(self)
    }

    pub fn dock_placement(&self) -> DockPlacement {
        PanelsHost::dock_placement(self)
    }

    pub fn bottom_open(&self) -> bool {
        bottom_open(self)
    }

    pub fn bottom_stored_open(&self) -> bool {
        bottom_stored_open(self)
    }

    pub fn show_pane_tab(&mut self, tx: &mut Tx, show: impl FnOnce(&mut Self)) {
        show_pane_tab(self, tx, show);
    }

    pub fn session_side(&self, session_id: &str) -> Option<&SessionSideState> {
        session_side(self, session_id)
    }

    pub fn session_side_mut(&mut self, session_id: &str) -> &mut SessionSideState {
        session_side_mut(self, session_id)
    }

    pub fn resolve_side_region(&self) -> Option<Rc<ResolvedRegion>> {
        resolve_side_region(self)
    }

    pub fn shown_new_tab(&self, session_id: &str, pane: SessionPane, pane_active: Option<&str>) -> Option<&NewTabEntry> {
        shown_new_tab(self, session_id, pane, pane_active)
    }

    /// A New tab page is drawn in place of the side region's active tab: the tab is still the
    /// strip's active one, but nothing of it shows, so its own keys have nothing to act on.
    pub fn new_tab_covers_active(&self) -> bool {
        self.resolve_side_region().is_some_and(|reg| self.shown_new_tab(&reg.session_id, reg.active_pane, reg.active.as_deref()).is_some())
    }

    pub fn live_new_tabs<'a>(
        &'a self,
        session_id: &str,
        pane: SessionPane,
        pane_active: Option<&str>,
    ) -> Vec<&'a NewTabEntry> {
        live_new_tabs(self, session_id, pane, pane_active)
    }

    pub fn open_side_new_tab(&mut self, pane: SessionPane) -> String {
        open_side_new_tab(self, pane)
    }

    pub fn dismiss_side_new_tab(&mut self, new_id: &str) {
        dismiss_side_new_tab(self, new_id);
    }

    pub fn toggle_side_region(&mut self, tx: &mut Tx) {
        let was_open = self.side_open();
        toggle_side_region(self, tx);
        if !was_open
            && self.is_review_active()
            && let Some(sid) = self.active_session_id().map(str::to_owned)
        {
            crate::plugin_host::native::diff::open_review(self, &sid);
        }
    }

    pub fn toggle_bottom_region(&mut self) {
        toggle_bottom_region(self);
    }

    pub fn open_side_tab_for_session(
        &mut self,
        session_id: &str,
        key: &str,
        target_pane: Option<SessionPane>,
        preview: bool,
        first: bool,
    ) {
        open_side_tab_for_session(self, session_id, key, target_pane, preview, first);
    }

    pub fn tab_state(&self, session: &str, key: &str) -> u64 {
        tab_state(self, session, key)
    }

    pub fn pane_shown_tab(&self, pane: SessionPane) -> Option<String> {
        pane_shown_tab(self, pane)
    }

    pub fn side_tab_visible(&self, key: &str) -> bool {
        side_tab_visible(self, key)
    }

    pub fn hide_side_tab(&mut self, key: &str, tx: &mut Tx) {
        hide_side_tab(self, key, tx);
    }

    pub fn keeping_keyboard<R>(&mut self, open: impl FnOnce(&mut Self) -> R) -> R {
        keeping_keyboard(self, open)
    }

    pub fn show_side_tab(&mut self, session_id: &str, key: &str, target: Option<SessionPane>) {
        show_side_tab(self, session_id, key, target);
    }

    pub fn open_side_tab(
        &mut self,
        key: &str,
        target_pane: Option<SessionPane>,
        preview: bool,
        first: bool,
        tx: &mut Tx,
    ) {
        open_side_tab(self, key, target_pane, preview, first, tx);
    }

    pub fn select_side_tab(&mut self, key: &str, tx: &mut Tx) {
        select_side_tab(self, key, tx);
        if key == crate::review::REVIEW_KEY
            && let Some(sid) = self.active_session_id().map(str::to_owned)
        {
            crate::plugin_host::native::diff::open_review(self, &sid);
        }
    }

    pub fn move_side_tab(&mut self, key: &str, pane: SessionPane, before: Option<&str>, tx: &mut Tx) {
        move_side_tab(self, key, pane, before, tx);
    }

    pub fn promote_preview_tab(&mut self, key: &str) {
        promote_preview_tab(self, key);
    }

    pub fn close_side_tab(&mut self, key: &str, tx: &mut Tx) {
        close_side_tab(self, key, tx);
    }

    pub fn reopen_closed_side_tab(&mut self, key: Option<&str>, tx: &mut Tx) -> bool {
        let reopened = reopen_closed_side_tab(self, key, tx);
        if reopened
            && self.is_review_active()
            && let Some(sid) = self.active_session_id().map(str::to_owned)
        {
            crate::plugin_host::native::diff::open_review(self, &sid);
        }
        reopened
    }

    pub fn select_adjacent_side_tab(&mut self, offset: i32, tx: &mut Tx) -> bool {
        select_adjacent_side_tab(self, offset, tx)
    }

    pub fn closing_side_entry(&self, reg: &ResolvedRegion, pane: SessionPane) -> Option<&ResolvedEntry> {
        closing_side_entry(self, reg, pane)
    }

    pub fn poll_panels(&mut self, tx: &mut Tx) -> bool {
        poll_panels(self, tx)
    }

    pub(crate) fn poll_plugins(&mut self, tx: &mut Tx) -> bool {
        let stamp = plugin_watched_stamp(self);
        let watched_changed = {
            let seen = &self.kernel.slots.untracked(&PLUGIN_WATCHED_STAMP).0;
            let changed = seen.get() != Some(stamp);
            seen.set(Some(stamp));
            changed
        };
        crate::plugin_host::poll(self, watched_changed, tx)
    }

    pub fn panels_ime(&mut self, ime: &Ime) -> bool {
        panels_ime(self, ime)
    }

    pub fn panels_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        panels_key(self, ev, tx)
    }

    pub(crate) fn bottom_has_candidate(&self) -> bool {
        dock::bottom_has_candidate(self)
    }

    pub fn choose_side_new_tab(&mut self, pane: SessionPane, id: &str, tx: &mut Tx) {
        drag::choose_side_new_tab(self, pane, id, tx);
    }

    pub fn side_new_tab_gesture(&mut self, pane: SessionPane, id: &str, shown: bool, d: &Drag, tx: &mut Tx) {
        drag::side_new_tab_gesture(self, pane, id, shown, d, tx);
    }

    pub fn side_tab_gesture(&mut self, key: &str, title: &str, draggable: bool, shown: bool, d: &Drag, tx: &mut Tx) {
        drag::side_tab_gesture(self, key, title, draggable, shown, d, tx);
    }

    pub fn session_row_available(&self, zoomed_w: f32) -> f32 {
        width::session_row_available(self, zoomed_w)
    }

    pub fn side_wide(&self) -> bool {
        width::side_wide(self)
    }

    pub fn chat_width(&self, zoomed_w: f32) -> f32 {
        width::chat_width(self, zoomed_w)
    }

    pub fn side_width(&self, zoomed_w: f32) -> f32 {
        width::side_width(self, zoomed_w)
    }

    pub fn chat_width_max(&self, zoomed_w: f32) -> f32 {
        width::chat_width_max(self, zoomed_w)
    }

    pub fn split_ratio(&self) -> f32 {
        width::split_ratio(self)
    }

    pub fn bottom_height(&self) -> f32 {
        width::bottom_height(self)
    }
}

/// Last observed stamp of every host input read by `plugin_host::services::watch::fire_watched`.
#[derive(Default)]
struct PluginWatchedStamp(std::cell::Cell<Option<u64>>);

static PLUGIN_WATCHED_STAMP: Slot<PluginWatchedStamp> = Slot::new();

/// Stamp of every host input watched by Luau plugin signals (`plugin_host::services::watch`):
/// `Sessions.Changed`, `Sessions.RoutedChanged`, `Browser.Changed`, `Models.Changed`,
/// `Theme.Changed`, `I18n.Changed`, `App.LocaleChanged`, `Server.Connected`, `Servers.Changed`,
/// `ctx.FocusChanged`, `Updater.StateChanged`, `editor.*.{Changed,FocusChanged}`,
/// `Store.*.Changed`, `Diff.*.Changed`, and `Contracts.*.Changed`.
fn plugin_watched_stamp(app: &App) -> u64 {
    use rune::Tracked;
    let stamp = |cell_id: u32, cell_ver: u64| (cell_id, cell_ver);
    let slots = &app.kernel.slots;
    let mut h = DefaultHasher::new();
    crate::servers::runtime::active(app).hash(&mut h);
    [
        stamp(Tracked::id(&app.sessions), Tracked::version(&app.sessions)),
        stamp(Tracked::id(&app.projects), Tracked::version(&app.projects)),
        stamp(Tracked::id(&app.running), Tracked::version(&app.running)),
        stamp(Tracked::id(&app.inboxes), Tracked::version(&app.inboxes)),
        stamp(Tracked::id(&app.context_windows), Tracked::version(&app.context_windows)),
        stamp(Tracked::id(&app.transcripts), Tracked::version(&app.transcripts)),
        stamp(Tracked::id(&app.route), Tracked::version(&app.route)),
        stamp(Tracked::id(&app.workspace_panes.browser), Tracked::version(&app.workspace_panes.browser)),
        stamp(slots.cell_id(&crate::review::REVIEW), slots.version(&crate::review::REVIEW)),
        stamp(Tracked::id(&app.services.server_links), Tracked::version(&app.services.server_links)),
        stamp(Tracked::id(&app.plugins), Tracked::version(&app.plugins)),
    ]
        .hash(&mut h);
    let pickers = Tracked::untracked(&app.composer_ext.pickers);
    (pickers.models.len(), pickers.limits.len(), pickers.paid.len()).hash(&mut h);
    match Tracked::untracked(&app.source) {
        crate::Source::Live { server, .. } => (true, server.base.as_str()).hash(&mut h),
        crate::Source::Fixture => (false, "").hash(&mut h),
    }
    let p = crate::palette::pal(app.plugins.dark.get());
    (
        p.theme,
        p.generation,
        p.dark,
        opencode_i18n::I18N.locale(),
        crate::plugin_host::services::servers_version(app),
        app.plugins.window_blurred.get(),
        crate::updater::revision_untracked(app),
        crate::plugin_host::epoch(),
    )
        .hash(&mut h);
    // Editor focus can change through interior mutability (`editors::{focus,blur}`) before
    // `requests::carry_out` bumps the plugin epoch; XOR makes the hash order-independent.
    let mut editor_focus = 0u64;
    for (key, edit) in app.plugins.editors.borrow().iter() {
        let mut eh = DefaultHasher::new();
        (key, edit.focused).hash(&mut eh);
        editor_focus ^= eh.finish();
    }
    editor_focus.hash(&mut h);
    // Lazily created signals (`Store`, `Contracts`, `editor`, `Browser.Changed`) register via
    // `RefCell` without bumping `epoch()`; hashing each plugin's signal count ensures
    // `fire_watched` baselines new signals before their first real mutation arrives.
    for p in &app.plugins.list {
        (p.half.id.as_ref(), p.half.signals.borrow().len()).hash(&mut h);
    }
    h.finish()
}

/// Kernel feature registration for the session side, split, and bottom panels engine.
pub struct PanelsFeature;

impl Feature<App> for PanelsFeature {
    fn id(&self) -> &'static str {
        "feature.panels"
    }

    fn register(&self, builder: &mut KernelBuilder<App>) {
        builder
            .register_slot(&SIDE)
            .register_slot(&PLUGIN_WATCHED_STAMP);
        builder.register_overlay(
            opencode_kernel::OverlayRegistration::<App>::new("overlay.panel_menus")
                .with_render(crate::overlays::layer::FLOATING_CHROME, |s, cx| {
                    let p = crate::palette::pal(s.dark(cx));
                    view::context_menus(s, p).into_iter().collect()
                })
                .above("overlay.line_comment_toolbar"),
        );
        builder.register_overlay(
            opencode_kernel::OverlayRegistration::<App>::new("overlay.panel_drag")
                .with_render(crate::overlays::layer::FLOATING_CHROME, |s, cx| {
                    let p = crate::palette::pal(s.dark(cx));
                    drag::layer(s, p).into_iter().collect()
                })
                .above("overlay.panel_menus"),
        );
        // After Copy project ID and before the message keys, as Desktop registers it (`use-session-commands.tsx:610`).
        builder.add_command(
            CommandEntry::<App>::from_key(
                "side",
                opencode_i18n::key!("command.review.toggle"),
                crate::commands::order::SIDE,
                |s, tx| s.toggle_side_region(tx),
            )
            .bind_id(Bind::SideToggle.id())
            .when(App::in_root_session),
        );
        // A shortcut from the composer that moved the keyboard into the side region (its command
        // may run a frame later) leaves the selected tab's label muted (`FocusMode::Quiet`).
        // Known: `egg.poll` (in `INGEST_LOCAL`) and `notifications.poll` can focus the composer;
        // precaution: `speech.poll` adjusts focus before composer focus sync.
        builder
            .schedule(crate::tick::stage::INGEST_DISPATCH, "panels.sync_composer", |s, _, _| {
                let composing = s.composer.focused;
                let side = s.kernel.slots.get(&SIDE);
                let in_terminal = !composing
                    && side.focus.is_armed()
                    && side.focus.is_focused()
                    && s.terminal_focus().is_some();
                let sync = SideFocusEvent::SyncComposer { composing, in_terminal };
                // Written only when it changes: a write to the side state rebuilds every region reading it.
                if s.kernel.slots.get(&SIDE).focus.would_change(sync) {
                    s.kernel.slots.get_mut(&SIDE).focus.sync_composer(composing, in_terminal);
                }
                false
            })
            .after("speech.poll");
        builder.schedule(crate::tick::stage::REACT_PANELS, "panels.poll", |s, _, tx| {
            s.poll_panels(tx)
        });
    }
}
