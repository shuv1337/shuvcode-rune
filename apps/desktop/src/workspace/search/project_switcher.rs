//! Desktop host adapter for `opencode_feature_switcher::project` (`features/switcher`).

pub use opencode_feature_switcher::project::*;

use opencode_kit::model::Project;
use opencode_kit::servers::ServerKey;
use opencode_servers::ServerConn;
use rune::machine::Attempt;
use rune::{El, KeyEvent, Tx};
use session_view::palette::Pal;

use crate::pickers::PickerHost as _;
use crate::{App, Route};

pub(crate) static PROJECT_SWITCHER: opencode_kernel::Slot<ProjectSwitcher> = opencode_kernel::Slot::new();

impl ProjectSwitcherHost for App {
    fn project_switcher(&self) -> &ProjectSwitcher {
        self.kernel.slots.get(&PROJECT_SWITCHER)
    }

    fn project_switcher_mut(&mut self) -> &mut ProjectSwitcher {
        self.kernel.slots.get_mut(&PROJECT_SWITCHER)
    }

    fn project(&self, id: &str) -> Option<&Project> {
        App::project(self, id)
    }

    fn used_projects(&self) -> Vec<&Project> {
        App::used_projects(self)
    }

    fn other_projects(&self) -> Vec<&Project> {
        App::other_projects(self)
    }

    fn project_worktrees(&self, id: &str, root: &str) -> Vec<String> {
        App::project_worktrees(self, id, root)
    }

    fn is_draft_route(&self) -> bool {
        matches!(*self.route, Route::Draft(_))
    }

    fn is_stage(&self) -> bool {
        self.fixture_run.stage.is_some()
    }

    fn open_draft_tab(&mut self) {
        App::open_draft_tab(self);
    }

    fn set_composer_focused(&mut self, focused: bool) {
        self.composer.focused = focused;
    }

    fn auto_focus_composer(&mut self, tx: &mut Tx) {
        if self.fixture_run.stage.is_none() && crate::auto_focus_composer(tx) {
            self.composer.focused = true;
        }
    }

    fn draft_base(&self) -> Option<&str> {
        App::draft_base(self)
    }

    fn draft_creates(&self) -> bool {
        App::draft_creates(self)
    }

    fn draft_project(&self) -> Option<&Project> {
        App::draft_project(self)
    }

    fn draft_dir(&self) -> Option<&str> {
        Some(App::draft_dir(self))
    }

    fn open_branch_picker(&mut self, tx: &mut Tx) {
        App::open_branch_picker(self, tx);
    }

    fn close_branch_picker(&mut self, tx: &mut Tx) {
        App::close_branch_picker(self, tx);
    }

    fn choose_new_worktree(&mut self, project_id: &str, directory: String) {
        App::choose_new_worktree(self, project_id, directory);
    }

    fn set_draft_base(&mut self, base: Option<String>) {
        App::set_draft_base(self, base);
    }

    fn set_draft_dir(&mut self, directory: String) {
        App::set_draft_dir(self, directory);
    }

    fn note_existing_worktree(&mut self, project_id: &str, directory: String) {
        App::note_existing_worktree(self, project_id, directory);
    }

    fn touch_project(&mut self, project_id: &str) {
        App::touch_project(self, project_id);
    }

    fn sort_projects(&mut self) {
        App::sort_projects(self);
    }

    fn open_project_dir(&mut self, directory: &str, open_session: bool, tx: &mut Tx) {
        App::open_project_dir(self, directory, open_session, tx);
    }

    fn open_project_dialog(&mut self, tx: &mut Tx) {
        App::open_project_dialog(self, tx);
    }

    fn enqueue_branch_dir(&mut self, directory: String) {
        let server = crate::servers::runtime::active(self).clone();
        self.sidebar_inbox.branches.enqueue(&server, [directory]);
    }

    fn checkout_branch(&self, directory: &str) -> Option<&str> {
        self.sidebar_inbox.branches.get(crate::servers::runtime::active(self), directory)
    }

    fn fetch_branches(&mut self, server: Option<&ServerKey>, directory: String, query: String, attempt: Attempt) -> bool {
        let key = server.cloned().unwrap_or_else(|| crate::servers::runtime::active(self).clone());
        let issued = crate::servers::runtime::with_server(self, &key, |s| {
            if !s.live() {
                return false;
            }
            s.job(move |srv| {
                let result = srv.branches(&directory, &query).ok();
                Box::new(move |s: &mut App, _| apply_branch_answer(s, attempt, result))
            });
            true
        });
        issued == Some(true)
    }

    fn active_server(&self) -> ServerKey {
        crate::servers::runtime::active(self).clone()
    }

    fn read_listed_servers(&mut self, drilled_project: Option<&str>) -> Vec<ListedServer> {
        let all = crate::servers::registry::list(self);
        if all.len() <= 1 || !self.live() {
            return Vec::new();
        }
        let drilled = drilled_project.map(str::to_owned);
        let mut out = Vec::new();
        for e in all.into_iter().filter(|e| e.conn.http().is_some()) {
            let key = e.key();
            if opencode_servers::health::is_failed(crate::servers::health::get(&key).as_ref()) {
                continue;
            }
            let kind = match &e.conn {
                ServerConn::Sidecar { .. } => ServerKind::Current,
                ServerConn::Plugin { .. } => ServerKind::Plugin,
                ServerConn::Http { .. } => ServerKind::Remote,
            };
            let drilled = drilled.clone();
            let read = move |s: &mut App| {
                let used: Vec<Project> = s.used_projects().into_iter().cloned().collect();
                let others: Vec<Project> = s.other_projects().into_iter().cloned().collect();
                let worktrees = drilled
                    .iter()
                    .filter_map(|id| s.project(id).map(|p| (id.clone(), s.project_worktrees(id, &p.directory))))
                    .collect();
                let active = crate::servers::runtime::active(s);
                let branch = drilled
                    .as_ref()
                    .and_then(|id| s.project(id))
                    .and_then(|p| s.sidebar_inbox.branches.get(active, &p.directory).map(|b| (p.directory.clone(), b.to_owned())));
                (used, others, worktrees, branch)
            };
            let Some((used, others, worktrees, checkout_branch)) = crate::servers::runtime::with_server(self, &key, read) else {
                continue;
            };
            out.push(ListedServer {
                name: crate::servers::registry::label(&e.conn),
                hint: crate::servers::registry::hint(&e.conn),
                key,
                kind,
                used,
                others,
                worktrees,
                checkout_branch,
            });
        }
        if out.len() <= 1 { Vec::new() } else { out }
    }

    fn list_worktrees_on(&mut self, server: Option<&ServerKey>, project_id: &str) {
        let key = server.cloned().unwrap_or_else(|| crate::servers::runtime::active(self).clone());
        crate::servers::runtime::with_server(self, &key, |s| {
            s.list_worktrees(project_id);
            if let Some(dir) = s.project(project_id).map(|p| p.directory.clone()) {
                let active = crate::servers::runtime::active(s).clone();
                s.sidebar_inbox.branches.enqueue(&active, [dir]);
            }
        });
    }

    fn select_server_location(&mut self, server: &ServerKey, directory: String, tx: &mut Tx) {
        if let Route::Draft(id) = &*self.route
            && let Some(tab) = self.tabs.iter_mut().find(|t| t.id == *id)
        {
            tab.server = server.clone();
            crate::servers::runtime::switch(self, server);
            self.set_draft_dir(directory);
            self.save_tabs();
            return;
        }
        crate::servers::runtime::switch(self, server);
        self.open_project_dir(&directory, true, tx);
    }

    fn pick_remote_project_folder(&mut self, server: &ServerKey, title: String, tx: &mut Tx) {
        let key = server.clone();
        crate::projects::remote_picker::pick_with(self, server, title, tx, move |h, dir, tx| {
            if matches!(h.current_route(), opencode_kit::servers::Route::Draft(_)) {
                let _ = crate::projects::on_server(h, &key, |h| crate::projects::open_project_dir(h, &dir, false, tx));
                h.open_draft_in(&key, &dir, tx);
                return;
            }
            h.switch_server(&key);
            crate::projects::open_project_dir(h, &dir, true, tx);
        });
    }

    fn before_project_switcher_open(&mut self) -> bool {
        // Carry the palette's focus origin across both Open and Dispatching: cancelling
        // `NewSessionLocationDialog` returns focus to the composer (`palette.ts:210-219`,
        // `dialog-machine.ts:20-27,167-181,229-253`, `new-session/location/dialog.tsx:52-64`,
        // `new-session/screen.tsx:24,41-42`).
        let restore = self.composer.focused
            || self.kernel.slots.get(&crate::switcher::SWITCHER).restore_composer()
            || self.composer_ext.pickers.open().is_some();
        self.close_picker(false);
        self.close_switcher(false);
        self.project_state.home_menu_open = false;
        self.set_settings_open(false);
        self.search.focused = false;
        self.composer.focused = false;
        restore
    }

    fn after_project_switcher_close(&mut self, restore_composer: bool) {
        if restore_composer && self.fixture_run.stage.is_none() {
            self.composer.focused = true;
        }
    }

    fn project_badge(&self, p: &Pal, project: Option<&Project>, name: &str) -> El {
        crate::icons::badge(self, p, project, name)
    }

    fn handle_shortcut(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        self.shortcut(ev, tx)
    }

    fn worktree_picked(&mut self, pick: WorktreePick, tx: &mut Tx) {
        crate::location_missing::worktree_picked(self, pick, tx);
    }
}

impl App {
    pub fn project_switcher_rows(&self) -> Vec<SwitcherRow> {
        rows(self)
    }

    pub fn project_switcher_active(&self, list: &[SwitcherRow]) -> Option<String> {
        active(self, list)
    }

    pub fn toggle_project_switcher(&mut self, tx: &mut Tx) {
        toggle(self, tx);
    }

    pub fn open_project_switcher(&mut self) {
        open(self);
    }

    pub fn open_project_worktrees(&mut self, project: &str, tx: &mut Tx) {
        open_worktrees(self, project, tx);
    }

    pub fn close_project_switcher(&mut self, restore: bool) {
        close(self, restore);
    }

    pub fn drill_project_worktrees(&mut self, project_id: &str, tx: &mut Tx) {
        drill_worktrees(self, project_id, tx);
    }

    pub fn project_switcher_back(&mut self, tx: &mut Tx) {
        back(self, tx);
    }

    pub fn project_switcher_choose(&mut self, key: &str, tx: &mut Tx) {
        choose(self, key, tx);
    }

    pub fn project_switcher_key(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        key(self, ev, tx)
    }

    pub(crate) fn open_git_location(&mut self, base: bool, tx: &mut Tx) {
        open_git_location(self, base, tx);
    }
}
