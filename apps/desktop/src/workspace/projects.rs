//! Desktop host adapter for `opencode-feature-projects` (`features/projects`).

pub use opencode_feature_projects::*;

use opencode_kernel::{CommandEntry, EscapeLayer, Feature, KernelBuilder, OverlayRegistration, Slot};
use opencode_kit::client::{Request, Server, Write};
use opencode_kit::model::{LocationInfo, Project, SessionInfo, Transcript};
use opencode_kit::servers::{Route as KitRoute, ServerKey};
use opencode_servers::ServerConn;
use opencode_servers::health::Health;
use opencode_kit::wire_error::WireError;
use rune::{El, FileDialogOptions, Tx};
use serde_json::Value;
use session_view::palette::Pal;

use crate::pickers::PickerHost as _;
use crate::{App, Route, Source};

pub(crate) static REMOTE_PICKER: Slot<remote_picker::Picker> = Slot::new();
pub(crate) static HOME_STATE: Slot<home::Home> = Slot::new();

impl ProjectsHost for App {
    fn project_state(&self) -> &ProjectsState {
        &self.project_state
    }

    fn project_state_mut(&mut self) -> &mut ProjectsState {
        &mut self.project_state
    }

    fn home_state(&self) -> &home::Home {
        self.kernel.slots.get(&HOME_STATE)
    }

    fn home_state_mut(&mut self) -> &mut home::Home {
        self.kernel.slots.get_mut(&HOME_STATE)
    }

    fn remote_picker(&self) -> &remote_picker::Picker {
        self.kernel.slots.get(&REMOTE_PICKER)
    }

    fn remote_picker_mut(&mut self) -> &mut remote_picker::Picker {
        self.kernel.slots.get_mut(&REMOTE_PICKER)
    }

    fn projects(&self) -> &[Project] {
        &self.projects
    }

    fn projects_mut(&mut self) -> &mut Vec<Project> {
        &mut self.projects
    }

    fn project(&self, id: &str) -> Option<&Project> {
        App::project(self, id)
    }

    fn has_worktree_at(&self, project: &Project, dir: &str) -> bool {
        App::has_worktree_at(self, project, dir)
    }

    fn list_worktrees(&mut self, project_id: &str) {
        App::list_worktrees(self, project_id);
    }

    fn sort_projects(&mut self) {
        App::sort_projects(self);
    }

    fn sessions(&self) -> &[SessionInfo] {
        &self.sessions
    }

    fn sessions_mut(&mut self) -> Option<&mut Vec<SessionInfo>> {
        Some(&mut self.sessions)
    }

    fn session(&self, id: &str) -> Option<&SessionInfo> {
        App::session(self, id)
    }

    fn session_active_ms(&self, session: &SessionInfo) -> i64 {
        self.recent_page.active_ms(session)
    }

    fn ensure_transcript(&mut self, session_id: &str) {
        self.transcripts
            .entry(session_id.to_owned())
            .or_insert_with(|| Transcript { session: session_id.to_owned(), messages: Vec::new() });
    }

    fn default_selection(&self) -> DefaultSelection {
        let sel = self.selection();
        DefaultSelection { model: sel.model, agent: sel.agent }
    }

    fn tabs(&self) -> Vec<TabSummary> {
        self.tab_summaries()
    }

    fn current_tab(&self) -> Option<TabSummary> {
        let uid = App::current_tab(self)?;
        self.tab(&uid).map(crate::tabs::Tab::summary)
    }

    fn restore_project_tab_uid(&self) -> Option<String> {
        self.restore_project_tab().map(crate::tabs::Tab::uid)
    }

    fn select_tab(&mut self, uid: &str, tx: &mut Tx) {
        App::select_tab(self, uid, tx);
    }

    fn home_dir(&self) -> &str {
        &self.home
    }

    fn default_dir(&self) -> &str {
        &self.default_dir
    }

    fn set_default_dir(&mut self, dir: String) {
        *self.default_dir = dir;
    }

    fn set_draft_dir(&mut self, dir: String) {
        App::set_draft_dir(self, dir);
    }

    fn current_route(&self) -> KitRoute {
        match &*self.route {
            Route::Home | Route::Automations => KitRoute::Home,
            Route::Draft(id) => KitRoute::Draft(id.clone()),
            Route::Session { server, id } => KitRoute::Session { server: server.clone(), session_id: id.clone() },
        }
    }

    fn is_home_route(&self) -> bool {
        *self.route == Route::Home
    }

    fn settings_open(&self) -> bool {
        App::settings_open(self)
    }

    fn close_settings(&mut self) {
        App::close_settings(self);
    }

    fn project_navigation(&self) -> bool {
        App::project_navigation(self)
    }

    fn active_server(&self) -> ServerKey {
        crate::servers::runtime::active(self).clone()
    }

    fn live_server(&self) -> Option<&Server> {
        match &*self.source {
            Source::Live { server, .. } => Some(server),
            Source::Fixture => None,
        }
    }

    fn own_server_base(&self) -> Option<String> {
        crate::servers::runtime::own_server().map(|o| o.base)
    }

    fn listed_servers(&self) -> Vec<ListedServer> {
        crate::servers::registry::list(self)
            .into_iter()
            .map(|e| ListedServer { conn: e.conn, name: e.name })
            .collect()
    }

    fn server_is_listed_in_runtime(&self, key: &ServerKey) -> bool {
        crate::servers::runtime::listed().iter().any(|l| l.key == *key)
    }

    fn server_health(&self, key: &ServerKey) -> Option<Health> {
        crate::servers::health::get(key)
    }

    fn server_label(&self, conn: &ServerConn) -> String {
        crate::servers::registry::label(conn)
    }

    fn server_label_of(&self, key: &ServerKey) -> String {
        crate::servers::registry::label_of(self, key)
    }

    fn server_client(&self, conn: &ServerConn) -> Option<Server> {
        crate::servers::registry::server_for(conn)
    }

    fn is_server_connected(&self, key: &ServerKey) -> bool {
        matches!(self.launch.phase(key.as_str()), Some(crate::launch::Phase::Connected))
    }

    fn lists_saved_sessions(&self, key: &ServerKey) -> bool {
        key.is_sidecar() && self.launch.saved_list.is_some()
    }

    fn server_sources_hydrated(&self) -> bool {
        crate::plugin_host::server_sources::pending_sources().is_empty()
    }

    fn home_sync_stamp(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        use rune::Tracked;
        let mut h = std::collections::hash_map::DefaultHasher::new();
        crate::servers::version().hash(&mut h);
        crate::servers::runtime::version().hash(&mut h);
        crate::servers::runtime::sidecar_stamp().hash(&mut h);
        crate::plugin_host::server_sources::version().hash(&mut h);
        crate::servers::health::version().hash(&mut h);
        opencode_i18n::I18N.locale().hash(&mut h);
        match Tracked::untracked(&self.source) {
            Source::Live { server, .. } => (true, server.base.as_str()).hash(&mut h),
            Source::Fixture => (false, "").hash(&mut h),
        }
        (
            (Tracked::id(&self.projects), Tracked::version(&self.projects)),
            (Tracked::id(&self.sessions), Tracked::version(&self.sessions)),
            (Tracked::id(&self.recent_page), Tracked::version(&self.recent_page)),
            Tracked::untracked(&self.project_state).home_sync_stamp(),
            Tracked::untracked(&self.launch).connected_stamp(),
            (
                self.kernel.slots.cell_id(&crate::new_worktree::NEW_WORKTREE),
                self.kernel.slots.version(&crate::new_worktree::NEW_WORKTREE),
            ),
            Tracked::untracked(&self.checkout).home_data_stamp(),
        )
            .hash(&mut h);
        h.finish()
    }

    fn can_write(&self) -> bool {
        App::can_write(self)
    }

    fn is_offline(&self) -> bool {
        crate::launch::offline(self)
    }

    fn is_stage(&self) -> bool {
        self.fixture_run.stage.is_some()
    }

    fn switch_server(&mut self, key: &ServerKey) {
        crate::servers::runtime::switch(self, key);
    }

    fn with_server(&mut self, server: &ServerKey, f: &mut dyn FnMut(&mut dyn ProjectsHost)) -> bool {
        crate::servers::runtime::with_server(self, server, |s| f(s)).is_some()
    }

    fn write_server(&mut self, write: Write) -> bool {
        App::write(self, write)
    }

    fn request_projects(&self) {
        if let Source::Live { requests: Some(r), .. } = &*self.source {
            r.send(Request::Projects).ok();
        }
    }

    fn retry_servers(&mut self, tx: &mut Tx) {
        crate::servers::health::retry(self, tx);
        if crate::launch::offline(self) {
            crate::launch::retry(self, tx);
        }
    }

    fn attended(&self) -> bool {
        crate::attended()
    }

    fn storage_read_json(&self, namespace: &str, key: &str) -> Option<Value> {
        crate::storage::json(namespace, key)
    }

    fn storage_write_json(&mut self, namespace: &str, key: &str, value: &Value) {
        crate::storage::set_json(namespace, key, value);
    }

    fn set_status(&mut self, status: String) {
        *self.diag.status = status;
    }

    fn toast_fail(&mut self, title: &str, description: &str) {
        crate::status_toast::fail(self, title, description);
    }

    fn toast_say(&mut self, title: &str, description: &str) {
        crate::status_toast::say(self, title, description);
    }

    fn toast_waiting(&mut self, message: &str) -> u64 {
        crate::status_toast::waiting(self, message)
    }

    fn toast_dismiss(&mut self, id: u64) {
        crate::status_toast::dismiss_id(self, id);
    }

    fn format_wire_error(&self, what: &str, error: &WireError) -> String {
        crate::errors::said(self, what, error)
    }

    fn before_open_project_dialog(&mut self) -> bool {
        // Carry the palette's focus origin across both Open and Dispatching: closing the palette
        // returns focus to the composer before the command runs (`palette.ts:210-219`,
        // `dialog-machine.ts:20-27,167-181,229-253`, `workspaces/selection/picker.tsx:30-37,58-75`).
        let restore_composer = self.composer.focused
            || self.kernel.slots.get(&crate::switcher::SWITCHER).restore_composer()
            || self.composer_ext.pickers.open().is_some();
        self.close_picker(false);
        self.close_switcher(false);
        self.close_project_switcher(false);
        self.set_settings_open(false);
        self.kernel.slots.get_mut(&crate::session_menu::MENUS).on = None;
        crate::subagents::shells::set_open(self, false);
        *self.kernel.slots.get_mut(&crate::todos::TODO_OPEN) = false;
        restore_composer
    }

    fn restore_composer_focus(&mut self) {
        if self.fixture_run.stage.is_none() {
            self.composer.focused = true;
        }
    }

    fn open_folder_dialog(&mut self, options: FileDialogOptions, tx: &mut Tx, on_done: FolderDialogDone) {
        tx.open_folder(options, move |s: &mut App, result, tx| {
            on_done(s, result.map_err(|e| e.to_string()), tx);
        });
    }

    fn open_file_dialog(&mut self, options: FileDialogOptions, tx: &mut Tx, on_done: FileDialogDone) {
        tx.open_file(options, move |s: &mut App, result, tx| {
            on_done(s, result.map_err(|e| e.to_string()), tx);
        });
    }

    fn open_project_dialog(&mut self, tx: &mut Tx) {
        open_project_dialog(self, tx);
    }

    fn open_draft_in(&mut self, server: &ServerKey, directory: &str, _tx: &mut Tx) {
        let dir = clean_dir(directory);
        if let Route::Draft(id) = &*self.route
            && let Some(tab) = self.tabs.iter_mut().find(|t| t.id == *id)
        {
            tab.server = server.clone();
            crate::servers::runtime::switch(self, server);
            self.set_draft_dir(dir);
            self.save_tabs();
            return;
        }
        crate::servers::runtime::switch(self, server);
        *self.default_dir = dir;
        self.open_draft_tab();
    }

    fn session_location_picked(&mut self, attempt: rune::machine::Attempt, directory: Option<String>, tx: &mut Tx) {
        crate::location_missing::picker_settled(self, attempt, directory, tx);
    }

    fn open_session(&mut self, server: &ServerKey, id: &str, tx: &mut Tx) {
        crate::servers::runtime::with_server(self, server, |s| s.open(id, tx));
    }

    fn open_session_now(&mut self, id: &str, tx: &mut Tx) {
        self.open_now(id, tx);
    }

    fn project_badge(&self, p: &Pal, project: Option<&Project>, name: &str) -> El {
        crate::icons::badge(self, p, project, name)
    }

    fn spawn_job(&mut self, job: ProjectsJob) {
        match job {
            ProjectsJob::ImportSession { attempt, payload } => {
                self.job(move |srv| {
                    let reply = execute_import(srv, attempt, payload);
                    Box::new(move |s: &mut App, tx: &mut Tx| apply_reply(s, reply, tx)) as crate::jobs::Apply
                });
            }
            ProjectsJob::Remote { server, request } => {
                let deliver = self.deliverer();
                crate::plugin_host::tasks::run(move || {
                    let reply = execute_remote(&server, request);
                    deliver(Box::new(move |s: &mut App, tx: &mut Tx| apply_reply(s, reply, tx)));
                });
            }
        }
    }
}

impl App {
    pub(crate) fn init_used_projects(&mut self, saved: Option<Vec<String>>) {
        init_used_projects(self, saved);
    }

    pub(crate) fn touch_project(&mut self, id: &str) {
        touch_project(self, id);
    }

    pub fn used_projects(&self) -> Vec<&Project> {
        used_projects(self)
    }

    pub fn last_used_project(&self) -> Option<&Project> {
        last_used_project(self)
    }

    pub fn other_projects(&self) -> Vec<&Project> {
        other_projects(self)
    }

    pub fn hide_project(&mut self, id: &str, tx: &mut Tx) {
        hide_project(self, id, tx);
    }

    pub fn toggle_home_project(&mut self, id: &str, tx: &mut Tx) {
        toggle_home_project(self, id, tx);
    }

    pub fn clear_home_project(&mut self, tx: &mut Tx) {
        clear_home_project(self, tx);
    }

    pub fn project_at(&self, dir: &str) -> Option<&Project> {
        project_at(self, dir)
    }

    pub fn project_for_session(&self, s: &SessionInfo) -> Option<&Project> {
        project_for_session(self, s)
    }

    pub fn open_project_dialog(&mut self, tx: &mut Tx) {
        open_project_dialog(self, tx);
    }

    pub fn open_project_dir(&mut self, raw_dir: &str, start_session: bool, tx: &mut Tx) {
        open_project_dir(self, raw_dir, start_session, tx);
    }

    pub(crate) fn apply_project_opened(
        &mut self,
        location: LocationInfo,
        projects: Vec<Project>,
        session: Option<SessionInfo>,
        tx: &mut Tx,
    ) {
        apply_project_opened(self, location, projects, session, tx);
    }

    /// Consolidated navigation under side tabs: the sidebar already lists sessions, so Home drops
    /// its projects column and picks the project from the search field (`route.tsx`'s `projectScope`).
    pub(crate) fn home_project_scope(&self) -> bool {
        self.settings.navigation_prototype() && self.tab_layout() == crate::layout_state::TabLayout::Vertical
    }

    pub fn home_selected(&self) -> Option<&Project> {
        home::selected(self)
    }

    pub fn home_selected_id(&self) -> Option<String> {
        home::selected_id(self)
    }

    pub(crate) fn import_session(&mut self, directory: &str, tx: &mut Tx) {
        import::import_session(self, directory, tx);
    }
}

/// Kernel feature registration for project tracking, Home, import, and remote directory picker.
pub struct ProjectsFeature;

impl Feature<App> for ProjectsFeature {
    fn id(&self) -> &'static str {
        "feature.projects"
    }

    fn register(&self, builder: &mut KernelBuilder<App>) {
        builder
            .register_slot(&REMOTE_PICKER)
            .register_slot(&HOME_STATE);
        #[cfg(not(target_arch = "wasm32"))]
        builder.add_command(
            CommandEntry::<App>::from_key(
                "open-project",
                opencode_i18n::key!("command.project.open"),
                crate::commands::order::OPEN_PROJECT,
                |s, tx| s.open_project_dialog(tx),
            ),
        );
        builder.add_command(
            CommandEntry::<App>::from_key(
                "project.copyID",
                opencode_i18n::key!("command.project.copyID"),
                crate::commands::order::COPY_PROJECT,
                |s, tx| {
                    let id = s.active_session_id().and_then(|id| s.session(id)).map(|i| i.project.clone());
                    s.copy_id(id, tx);
                },
            )
            .when(|s| s.active_session_id().and_then(|id| s.session(id)).is_some_and(|i| !i.project.is_empty())),
        );
        builder.register_overlay(
            OverlayRegistration::<App>::new(crate::view::home::browse::OVERLAY_ID)
                .with_render(crate::overlays::layer::SURFACE_POPOVER, |s, cx| {
                    let p = crate::palette::pal(s.dark(cx));
                    crate::view::home::browse::view(s, p, cx)
                })
                .with_key(crate::view::home::browse::key)
                .above("overlay.add_menu")
                .with_dismiss(crate::view::home::browse::close),
        );
        builder.register_overlay(
            OverlayRegistration::<App>::new("overlay.home_projects")
                .with_render(crate::overlays::layer::SURFACE_POPOVER, |s, cx| {
                    let p = crate::palette::pal(s.dark(cx));
                    crate::view::home::home_projects_popover(s, p).into_iter().flatten().collect()
                })
                .above("overlay.worktree_picker"),
        );
        builder.register_escape_layer(EscapeLayer::new(
            "escape.home_menu",
            crate::overlays::escape_band::PAGE_POPOVER,
            |s| s.project_state.home_menu_open,
            |s, _, tx| {
                tx.instant();
                s.project_state.home_menu_open = false;
            },
        ));
        builder.register_overlay(
            OverlayRegistration::<App>::new(REMOTE_PICKER_OVERLAY_ID)
                .with_render(crate::overlays::layer::ELEVATED_DIALOG, |s, cx| {
                    let p = crate::palette::pal(s.dark(cx));
                    remote_picker::view::view(s, p, cx)
                })
                .above(crate::settings::PROVIDER_REMOVAL_OVERLAY_ID)
                .with_key(|s, ev, tx| remote_picker::overlay_key(s, ev, tx))
                .with_dismiss(|s, tx| remote_picker::close(s, tx)),
        );
        // Known/precaution: reads `checkout`, `servers::runtime::sidecar_stamp`, and listed server state.
        builder
            .schedule(crate::tick::stage::INGEST_SERVERS, "projects.home", |s, _, tx| {
                home::poll(s, tx)
            })
            .after("servers.health");
    }
}

rune_register::register!(crate::scenes::SCENE_LIST: crate::scenes::Scene = [
    ("remote-folders", |s| remote_picker::open_preview(s)),
]);
