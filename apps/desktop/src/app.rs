//! Window state (`App`), launch-mode helpers, fixture/live bootstrapping, keyboard shortcuts, and
//! idle memory eviction (`App::rest`).

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender};

use opencode_kit::client::{DEFAULT_SERVER, Incoming, Request, Server, Write};
use opencode_kit::model::{Inbox, Pending, Project, SessionInfo, Transcript};
use opencode_kit::servers::ServerKey;
use rune::{KeyEvent, TextEdit, Tx, Waker};
use serde_json::Value;

use crate::pickers::PickerHost as _;
use crate::shell::app_state::{
    AppearanceState, ComposerExt, DiagState, FixtureRun, PlatformServices, QueueState,
    ShellGestures, TimelineFx, WorkspacePanes,
};
use crate::tabs::Tab;
use crate::{
    Route, appearance, background, commands, context_window, demo, dictation, dirs, events, files,
    find, history, inbox_model, jobs, keymap, knobs, launch, layout_state, model_prefs,
    plugin_host, popup, projects, prompt_history, recent_page, restore, send_failed, servers,
    session_catalog, session_pages, session_status, settings, shortcuts, slash, start_location, status_toast, stop,
    switcher, tabs, telemetry, turn_diffs, view, wall_ms, windows,
};

// A static, not a const: a const's bytes are copied into every codegen unit that names it.
pub(crate) static FIXTURE: &str = include_str!("../assets/fixture.json");

pub use opencode_runtime::TabStatus;

#[derive(Default)]
pub enum Source {
    Live { server: Server, requests: Option<Sender<Request>>, writes: Option<Sender<Write>>, allow_send: bool },
    #[default]
    Fixture,
}

/// Kernel state slot for background tool-output save error notifications.
#[derive(Default)]
pub(crate) struct OutputErrors(pub(crate) Option<(Sender<String>, Receiver<String>)>);

pub(crate) static OUTPUT_ERRORS: opencode_kernel::Slot<OutputErrors> = opencode_kernel::Slot::new();

/// Kernel state slot counting how many times the root view has run (read by `SwitcherHost::view_runs`).
#[derive(Default)]
pub(crate) struct ViewRuns(pub(crate) std::cell::Cell<u64>);

pub(crate) static VIEW_RUNS: opencode_kernel::Slot<ViewRuns> = opencode_kernel::Slot::new();

/// Per-code-block soft-wrap overrides keyed by `"{row_key}:{block_index}"` (the timeline's Wrap button).
pub(crate) static CODE_WRAP: opencode_kernel::Slot<std::collections::HashMap<String, bool>> = opencode_kernel::Slot::new();
/// This window's display math: its wake when an equation finishes, and its say in when cached
/// bitmaps go (`rune_kit::math::Client`).
pub(crate) static MATH: opencode_kernel::Slot<rune_kit::math::Client> = opencode_kernel::Slot::new();

#[derive(Default)]
pub struct App {
    /// Decoupled shell kernel registries (lifecycle scheduler, tracked feature slots, overlays,
    /// and commands).
    pub kernel: opencode_kernel::Kernel<App>,
    pub source: rune::Tracked<Source>,
    pub(crate) inbox: rune::Tracked<Option<Receiver<Incoming>>>,
    pub(crate) pages: rune::Tracked<session_pages::State>,
    pub(crate) opening: rune::Tracked<Option<session_pages::Opening>>,
    /// The server's projects and sessions. This and the other server data (transcripts, running,
    /// stops, requests, pending, inboxes) are the server's copy, which every window of the server
    /// holds in turn (`windows::peer`).
    pub projects: rune::Tracked<Vec<Project>>,
    pub sessions: rune::Tracked<Vec<SessionInfo>>,
    pub recent_page: rune::Tracked<recent_page::Page>,
    pub recent_reload: recent_page::Reload,
    pub(crate) session_search: switcher::search_index::SessionSearch,
    pub route: rune::Tracked<Route>,
    /// Where ⌘[ and ⌘] go (`history.rs`).
    pub nav: rune::Tracked<history::History>,
    pub transcripts: rune::Tracked<HashMap<String, Transcript>>,
    pub(crate) transcript_edits: events::TranscriptEdits,
    /// Pinned session tabs, in sidebar order (see `tabs.rs` for where they come from).
    pub tabs: rune::Tracked<Vec<Tab>>,
    /// The tab layout's switch and the collapsed sidebar's peek (`layout_state.rs`).
    pub layout: rune::Tracked<layout_state::Shell>,
    /// The Inbox layout's request times, branches and height (`inbox_model.rs`).
    pub sidebar_inbox: rune::Tracked<inbox_model::SidebarInbox>,
    /// UI zoom, 1.0 = 100% (see `view::app`).
    pub zoom: rune::Tracked<f32>,
    /// Changed files' diffs (`turn_diffs.rs`).
    pub turn_diffs: rune::Tracked<turn_diffs::TurnDiffs>,
    /// Which of each session's messages Desktop would hold, for the Context tab (`context_window`).
    pub context_windows: rune::Tracked<context_window::Windows>,
    /// Sessions running right now (any depth; a tab is busy when anything in its family runs).
    pub running: rune::Tracked<HashSet<String>>,
    /// Runs stopped on a press, until the server confirms (`stop.rs`).
    pub stops: rune::Tracked<stop::Stops>,
    /// Pending permission and form requests: (session, request id).
    pub requests: rune::Tracked<HashSet<(String, String)>>,
    /// Which sessions' requests this connection has read (`session_status.rs`).
    pub request_reads: rune::Tracked<session_status::RequestReads>,
    /// The open permission requests and forms we know the details of.
    pub pending: rune::Tracked<Vec<Pending>>,
    /// Prompts sent while a session was busy that the server hasn't delivered yet, by session.
    pub inboxes: rune::Tracked<HashMap<String, Inbox>>,
    /// This window's place in its server's peer (`windows::peer`).
    pub store: rune::Tracked<Box<windows::peer::Link>>,
    /// Where a new draft starts, and where paths resolve off a session: the folder last picked.
    /// The open draft's own folder is `App::draft_dir`.
    pub default_dir: rune::Tracked<String>,
    /// The home folder paths show as `~`: the user's, or the fixture's in a fixture run.
    pub home: rune::Tracked<String>,
    pub sidebar_open: rune::Tracked<bool>,
    pub sidebar_w: rune::Tracked<f32>,
    pub composer: rune::Tracked<TextEdit>,
    pub search: rune::Tracked<TextEdit>,
    pub project_state: rune::Tracked<Box<projects::ProjectsState>>,
    pub settings: rune::Tracked<settings::Settings>,
    /// Server requests on their way back (`jobs`).
    pub(crate) jobs: rune::Tracked<jobs::Jobs>,
    /// Prompts sent and not yet seen back, for rolling a failed one back (`send_failed.rs`).
    pub unsent: rune::Tracked<send_failed::Log>,
    /// Wall-clock milliseconds for relative times (fixture: its own fixed "now").
    pub now_ms: rune::Tracked<i64>,
    pub(crate) clock: rune::Tracked<f64>,
    pub(crate) waker: rune::Tracked<Option<Waker>>,
    pub(crate) launch: rune::Tracked<launch::Launch>,
    /// The plugins: their panels, commands, menus, settings and MCP tools (`plugin_host`).
    pub plugins: rune::Tracked<Box<plugin_host::PluginHost>>,
    /// The server the window works against now and its halves of the others (`servers::runtime`).
    pub checkout: rune::Tracked<servers::runtime::Checkout>,
    /// The session header's location card: each folder's branch once read.
    pub location: rune::Tracked<view::location::Location>,
    /// Which window this is, and what it keeps of itself (`windows`).
    pub window: rune::Tracked<windows::Window>,
    /// Fixture playback, demo script, and storybook playground state.
    pub fixture_run: FixtureRun,
    /// Diagnostics, debug bar, live knobs, frame recorder, and motion log state.
    pub diag: DiagState,
    /// The drawn menu or picker open now (off macOS).
    pub popup: rune::Tracked<Box<popup::Popup>>,
    /// Window status toast stack.
    pub toast: rune::Tracked<status_toast::StatusToast>,
    /// Side/bottom workspace surfaces, panes, external openers, and client MCP hub.
    pub workspace_panes: WorkspacePanes,
    /// Shell gestures: tab drag/preview/reopen, pinch zoom, sidebar resize, and Home return tab.
    pub gestures: ShellGestures,
    /// Timeline presentation, animations, disclosures, hover state, plan drawer, find, and nav.
    pub timeline_fx: TimelineFx,
    /// Theme, motion, text rendering, background, locale, project icons, and window frame state.
    pub appearance_state: AppearanceState,
    /// Composer auxiliary inputs: history, attachments, quotes, comments, pickers, mentions, slash,
    /// suggestions, dictation, and toast undo stacks.
    pub composer_ext: ComposerExt,
    /// Prompt queue mutations, drag/rewrite runs, held sends, and optimistic session starts.
    pub queue: QueueState,
    /// Platform and connectivity services: notifications, deep links, updater, reconnect links,
    /// and session availability states.
    pub services: PlatformServices,
}

pub(crate) fn arg(name: &str) -> Option<String> {
    let a = rune::extra_args();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

pub(crate) fn flag(name: &str) -> bool {
    rune::extra_args().iter().any(|x| x == name)
}

/// A run that renders without a person at it (snapshots, trees, scripts, videos). It can read a
/// live server but never sends to it or writes the saved tabs, zoom, session and drafts, so a
/// test can't post or type as Kit.
pub(crate) use rune::is_headless as headless;

/// `--harness-writes`, honoured only when `RUNE_DIRS_ROOT` moves every folder this app writes:
/// a headless run (the parity harness's `--realtime` probes) sends, stops and saves as a window
/// does, into its own root and to the server it was given.
pub(crate) fn harness_writes() -> bool {
    flag("--harness-writes") && dirs::rooted()
}

/// `--realtime`: a headless run on the wall clock, the harness's stand-in for a window. Rune takes
/// the flag for itself, so it is not among `rune::extra_args` and [`flag`] never sees it.
pub(crate) fn realtime() -> bool {
    std::env::args().any(|a| a == "--realtime")
}

/// A person, or the harness standing in for one, is at this run: it may write.
pub(crate) fn attended() -> bool {
    !headless() || harness_writes()
}

/// On a touch screen (`touch_controls()`), only a direct tap inside a text field or a hardware key
/// (`tx.by_keys()`) puts focus in the composer; opening a session or tab, leaving Settings or a
/// dialog, or tapping a toolbar button must not pop up the OS keyboard.
pub(crate) fn auto_focus_composer(tx: &Tx) -> bool {
    !session_view::style::touch_controls() || tx.by_keys()
}

/// As in Desktop: the server's image for a project or its letter, never a file found in its
/// folder (a repo may hold another app's icon, `~/code` every app's).
fn desktop_icons() -> project_icon::Icons {
    let mut icons = project_icon::Icons::default();
    icons.set_repo_icons(false);
    icons
}

impl App {
    pub fn toggle_expanded(&mut self, key: &str) -> bool {
        session_view::disclosure::toggle_expanded(self, key)
    }

    pub fn shutting(&self, key: &str) -> bool {
        session_view::disclosure::is_shutting(self, key)
    }

    pub fn mark_copied(&mut self, key: impl Into<String>, tx: &mut Tx) {
        session_view::copied::mark_copied(self, key, tx);
    }

    pub fn copied(&self, key: &str) -> bool {
        session_view::copied::is_copied(self, key)
    }

    pub(crate) fn view_runs(&self) -> u64 {
        self.kernel.slots.try_untracked(&VIEW_RUNS).map_or(0, |r| r.0.get())
    }

    pub(crate) fn bump_view_runs(&self) {
        if let Some(r) = self.kernel.slots.try_untracked(&VIEW_RUNS) {
            r.0.set(r.0.get().wrapping_add(1));
        }
    }

    pub(crate) fn empty() -> App {
        crate::init_features();
        let home = dirs::home();
        App {
            kernel: crate::kernel::build(),
            default_dir: arg("--directory").unwrap_or_else(|| home.clone()).into(),
            home: home.into(),
            sidebar_open: (!flag("--sidebar-closed")).into(),
            sidebar_w: view::SIDEBAR_W.into(),
            zoom: arg("--zoom").and_then(|z| z.parse::<f32>().ok()).map_or(1.0, |z| z.clamp(shortcuts::ZOOM_MIN, shortcuts::ZOOM_MAX)).into(),
            composer: TextEdit::new("").into(),
            search: TextEdit::new("").into(),
            appearance_state: AppearanceState {
                reduced_motion: flag("--reduced-motion").into(),
                motion: appearance::Motion::new(flag("--reduced-motion")).into(),
                icons: desktop_icons().into(),
                ..Default::default()
            },
            diag: DiagState {
                debug: flag("--debug").into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Back to Home, dropping a session that was still opening.
    pub(crate) fn go_home(&mut self, tx: &mut Tx) {
        let arrived = self.route != Route::Home;
        if arrived {
            *self.gestures.home_return_tab = self.current_tab();
        }
        self.close_settings();
        *self.opening = None;
        *self.route = Route::Home;
        // Consolidated navigation has no search dialog, so Home's search field starts focused (#64668).
        if arrived && self.settings.navigation_prototype() {
            self.focus_home_search(tx);
        }
        tx.settle();
    }

    fn load(window: windows::Window) -> App {
        let mut app = Self::empty();
        *app.window = window;
        let fixture_path = arg("--fixture");
        let server = launch::initial_server();
        let deferred = fixture_path.is_none() && launch::deferred();
        let want_live = fixture_path.is_none() && !demo::requested() && (deferred || flag("--live") || (!headless()));
        if want_live && app.load_live(server, false) { return app; }
        let src = match &fixture_path {
            Some(p) => std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{p}: {e}")),
            None => FIXTURE.to_owned(),
        };
        app.load_fixture(&src);
        // The demo plays on Kit's own zoom; a fixture run never saves it.
        if demo::requested() && arg("--zoom").is_none() && !headless() && let Some(z) = tabs::load_zoom() {
            *app.zoom = z.clamp(shortcuts::ZOOM_MIN, shortcuts::ZOOM_MAX);
        }
        app
    }

    pub(crate) fn new() -> App {
        // A harness run that writes keeps a real window's rows, so its tabs and drafts come back.
        Self::new_window(if harness_writes() { windows::first_saved() } else { windows::Window::unsaved() })
    }

    /// `new`, for window `window`: its own tabs and page.
    pub(crate) fn new_window(window: windows::Window) -> App {
        rune::marks::mark("state.load");
        let mut app = telemetry::startup("launch.load", || Self::load(window));
        appearance::apply_args(&mut app);
        if let Some(layout) = arg("--tab-layout") {
            app.settings.tab_layout = layout_state::TabLayout::parse(Some(&layout));
            *app.layout = layout_state::Shell::at_rest(app.settings.effective_tab_layout());
        }
        app.apply_appearance();
        if let Some(name) = arg("--preset") {
            knobs::apply_preset(&mut app, &name);
        }
        // `--knob <name>=<value>` (repeatable): a run starts with that knob set, by its key or its
        // panel name and a choice's label or a number, as a capture needs an experiment on.
        let all = rune::extra_args();
        for pair in all.iter().zip(all.iter().skip(1)).filter(|(k, _)| *k == "--knob").map(|(_, v)| v) {
            knobs::apply_arg(&mut app, pair);
        }
        session_view::scroller::set_device_scale(windows::view_scale(&app));
        rune::marks::mark("state.restored");
        app
    }

    pub(crate) fn load_fixture(&mut self, src: &str) {
        let v: Value = serde_json::from_str(src).unwrap_or_else(|e| panic!("fixture JSON: {e}"));
        self.composer_ext.pickers.load(&v);
        self.kernel.slots.get_mut(&crate::mentions::MENTIONS).load(&v);
        self.kernel.slots.get_mut(&crate::add_menu::ADD_MENU).slash.commands = slash::commands(&v["commands"]);
        *self.fixture_run.base_ms = v["now"].as_i64().unwrap_or(0);
        *self.now_ms = *self.fixture_run.base_ms;
        *self.projects = opencode_kit::model::parse_projects(&v["projects"]);
        *self.sessions = opencode_kit::model::parse_sessions(&v["sessions"]);
        if let Some(map) = v["messages"].as_object() {
            for (id, list) in map {
                let msgs = opencode_kit::model::parse_messages(&serde_json::json!({ "data": list }));
                self.transcripts.insert(id.clone(), Transcript { session: id.clone(), messages: msgs });
            }
        }
        let ids: Vec<String> = v["tabs"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned)).collect();
        *self.tabs = ids.iter().map(|id| Tab::session(id.clone(), "", "")).collect();
        *self.running = v["running"].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_owned)).collect();
        for r in v["requests"].as_array().into_iter().flatten() {
            if let (Some(s), Some(id)) = (r["sessionID"].as_str(), r["id"].as_str()) {
                self.requests.insert((s.to_owned(), id.to_owned()));
            }
        }
        *self.diag.status = "Fixture".into();
        #[cfg(not(target_arch = "wasm32"))]
        {
            *self.kernel.slots.get_mut(&crate::open_in::OPEN_IN) = crate::open_in::OpenIn::with(std::sync::Arc::new(crate::open_in::Fixed(vec!["vscode", "cursor", "zed", "terminal", "ghostty", "xcode", "powershell"])));
            self.kernel.slots.get_mut(&crate::open_in::OPEN_IN).detect();
        }
        if let Some(home) = v["home"].as_str() {
            *self.home = home.to_owned();
        }
        *self.fixture_run.fixture = v;
        *self.source = Source::Fixture;
        self.init_used_projects(None);
        self.sort_projects();
        if arg("--directory").is_none()
            && let Some(p) = self.projects.first()
        {
            *self.default_dir = p.directory.clone();
        }
    }

    pub(crate) fn load_live(&mut self, server: Server, fallback_fixture: bool) -> bool {
        // A headless snapshot reads the server now; a window reads it and probes after its first frame.
        let deferred = launch::deferred();
        let first = if deferred {
            None
        } else {
            match launch::First::fetch(&server) {
                Ok(f) => Some(f),
                Err(_) if fallback_fixture => return false,
                // No answer before the first frame: the launch tries again from `poll`.
                Err(_) => {
                    self.launch.set_phase(ServerKey::SIDECAR, launch::Phase::Offline { retries: 0, retry_at: 0.0 });
                    None
                }
            }
        };
        // The window starts on the app's own server; its tabs' other servers join as they're listed.
        let server = servers::runtime::bind(self, server);
        let doc = telemetry::startup("launch.window_doc", || restore::read_window(self));
        let saved_tabs = doc.tabs.as_ref().map(|list| list.iter().map(|t| Tab::from_doc(t, doc.info(&t.key()))).collect());
        self.kernel.slots.get_mut(&restore::RESTORE).unread_tabs = doc.unread_tabs.clone();
        // Desktop's tabs come into a fresh profile's first window only.
        let beta = self.window.fresh && server.base == DEFAULT_SERVER;
        match saved_tabs.or_else(|| (beta && !deferred).then(tabs::from_beta).flatten()) {
            Some(tabs) => *self.tabs = tabs,
            None if deferred => self.launch.set_phase(ServerKey::SIDECAR, launch::Phase::Waiting { beta_tabs: beta, fallback_fixture }),
            None => {}
        }
        if deferred && self.launch.phase(ServerKey::SIDECAR).is_none() {
            self.launch.set_phase(ServerKey::SIDECAR, launch::Phase::Waiting { beta_tabs: false, fallback_fixture });
        }
        if let Some(first) = first {
            self.apply_first(first);
            // Read before the source goes live: this client's used projects, none on a fresh profile.
            self.init_used_projects(Some(tabs::load_projects(&server.base).unwrap_or_default()));
        }
        if let Some(z) = tabs::load_zoom() {
            *self.zoom = z.clamp(shortcuts::ZOOM_MIN, shortcuts::ZOOM_MAX);
        }
        *self.settings = settings::load();
        *self.layout = layout_state::Shell::at_rest(self.settings.effective_tab_layout());
        tabs::pins::restore(self);
        tabs::navigation_switch::restore(self);
        view::sidebar::recent_view::restore(self);
        self.load_open_in();
        // Not `persists()`: the source goes live below, so that reads false here in every launch.
        if attended() {
            let wrap = files::markdown::WrapPrefs::load(self);
            self.kernel.slots.get_mut(&files::FILES).wrap = wrap;
            *self.workspace_panes.panel_sidebar = plugin_host::native::sidebar::PanelSidebar::load();
        }
        *self.kernel.slots.get_mut(&prompt_history::PROMPTS) = prompt_history::PromptHistory::load(attended());
        self.composer_ext.pickers.prefs = model_prefs::load();
        crate::pickers::saved::restore(self);
        telemetry::startup("launch.restore_ui", || self.restore_ui(&doc, &server.base));
        launch::saved::page_tab::note(self);
        tabs::groups::restore(self);
        tabs::auto_groups::restore(self);
        tabs::sections::restore(self);
        *self.fixture_run.base_ms = wall_ms();
        *self.now_ms = *self.fixture_run.base_ms;
        let allow_send = !flag("--read-only") && attended();
        *self.source = Source::Live { server, requests: None, writes: None, allow_send };
        self.open_outbox();
        // A window whose server answers after its first frame lists what that server last listed.
        if deferred {
            let _ = launch::saved_list::restore(self);
            launch::saved_catalog::restore(self);
            launch::saved_gutters::restore(self);
        }
        // After the saved list: a restored session's folder is its info's.
        crate::location_missing::restore(self);
        *self.diag.status = self.live_status();
        self.resume_settings_page();
        true
    }

    pub(crate) fn sort_projects(&mut self) {
        // Projects by their latest session, then by activity.
        let latest: HashMap<&str, i64> = self.sessions.iter().fold(HashMap::new(), |mut m, s| {
            let e = m.entry(s.project.as_str()).or_insert(0);
            *e = (*e).max(self.recent_page.active_ms(s));
            m
        });
        let key = |p: &Project| latest.get(p.id.as_str()).copied().unwrap_or(0).max(p.active);
        self.projects.sort_by_key(|p| std::cmp::Reverse(key(p)));
    }

    pub fn set_zoom(&mut self, dir: i32) {
        *self.zoom = shortcuts::step_zoom(*self.zoom, dir);
        self.gestures.zoom_pinch.leveled();
        if self.persists() {
            tabs::save_zoom(*self.zoom);
        }
    }

    /// OpenCode Desktop's key defaults; true when `ev` was one.
    ///
    /// Called from focused text inputs (`on_input_key`), which run ahead of `keys::on_key` and
    /// therefore include `dictation::key`. `keys::on_key` already invokes `dictation::key` earlier
    /// (between `key_band::MID` and `knobs::key`) and calls [`Self::shortcut_without_dictation`] so
    /// one key press never evaluates `dictation::key` twice.
    pub fn shortcut(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        // A key chord in progress, or one this key starts, takes the key before any shortcut.
        crate::key_sequence::field_key(self, ev, tx) || self.shortcut_inner(ev, tx, true)
    }

    pub(crate) fn shortcut_without_dictation(&mut self, ev: &KeyEvent, tx: &mut Tx) -> bool {
        self.shortcut_inner(ev, tx, false)
    }

    fn shortcut_inner(&mut self, ev: &KeyEvent, tx: &mut Tx, check_dictation: bool) -> bool {
        if settings::pages::shortcuts::run_bound(self, ev, tx)
            || (check_dictation && dictation::key(self, ev, tx))
            || background::key(self, ev, tx)
            || find::shortcut(self, ev, tx)
            || commands::key(self, ev, tx)
            || plugin_host::key(self, ev, tx)
        {
            return true;
        }
        if tx.focused().is_some() && keymap::Bind::PastePlain.matches_tx(ev, tx) {
            tx.request_paste();
            return true;
        }
        if let Some(forward) = history::bracket(ev, tx.physical_key()) {
            self.navigate(forward, tx);
            return true;
        }
        shortcuts::run(self, ev, tx)
    }

    pub fn live(&self) -> bool {
        matches!(*self.source, Source::Live { .. })
    }

    /// Saves tabs, zoom, the open session and drafts: live, with a person at it.
    pub fn persists(&self) -> bool {
        self.live() && attended()
    }

    /// Asks the server for one session's info (live only).
    pub fn fetch_info(&self, id: &str) {
        if let Source::Live { requests: Some(r), .. } = &*self.source {
            r.send(Request::Info(id.to_owned())).ok();
        }
    }

    /// `App::on_rest`: the window rests (10 s without a frame, or out of sight). What it holds
    /// for sessions no tab, route or run shows goes: their transcripts, their pages in the
    /// reader's cache, and the images only those held. A session opened again is read again,
    /// as one opened for the first time is. Nothing on screen changes.
    pub(crate) fn rest(s: &mut App, tx: &mut rune::Tx) -> bool {
        crate::shell::memory_reclaim::rest_all(s, tx)
    }

    /// `id` and its loaded parents, up to 32 steps.
    pub(crate) fn ancestors<'a>(&'a self, id: &'a str) -> impl Iterator<Item = &'a str> {
        std::iter::successors(Some(id), |cur| self.session(cur)?.parent.as_deref()).take(33)
    }

    /// The first session in `id`'s parent chain (itself included) that isn't loaded, so a
    /// running subagent can be traced to its tab.
    pub(crate) fn missing_ancestor(&self, id: &str) -> Option<String> {
        self.ancestors(id).take(32).last().filter(|cur| self.session(cur).is_none()).map(str::to_owned)
    }

    pub fn session(&self, id: &str) -> Option<&SessionInfo> {
        if self.sessions.len() >= 512 {
            return self
                .kernel
                .slots
                .untracked(&session_catalog::CATALOG)
                .lookup(&self.sessions, id)
                .map(|(i, _)| &self.sessions[i]);
        }
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.iter().find(|p| p.id == id)
    }

    /// Whether `id`'s inbox has queued/steered prompts or a pending compaction waiting to run
    /// (matches official `sessionBusy`: `packages/app/src/shell/layout/project-avatar-state.ts:109-114`).
    pub(crate) fn inbox_busy(&self, id: &str) -> bool {
        self.inboxes.get(id).is_some_and(|i| !i.items.is_empty() || i.compaction.is_some())
    }

    fn last_settled_message<'a>(&self, t: &'a Transcript) -> Option<&'a opencode_kit::model::Message> {
        t.messages.iter().rev().find(|m| {
            !matches!(m, opencode_kit::model::Message::Notice(_))
                && !matches!(
                    self.unsent.state(m.id()),
                    Some(send_failed::State::Waiting | send_failed::State::Unconfirmed | send_failed::State::Failed),
                )
        })
    }

    /// Whether `t` ends in an unanswered user prompt or an assistant step that has neither
    /// completed nor errored.
    pub(crate) fn transcript_unsettled(&self, t: &Transcript) -> bool {
        matches!(self.last_settled_message(t), Some(opencode_kit::model::Message::User { .. }))
            || matches!(self.last_settled_message(t), Some(opencode_kit::model::Message::Assistant(a)) if a.completed.is_none() && a.error.is_none())
    }

    /// Whether session `id` itself is busy: running on the server, waiting on queued/steered
    /// prompts or pending compaction in its inbox, starting a fresh session, or — before
    /// `/api/session/active` has synced on a cold open/reconnect (or in fixture mode) — holding
    /// an unsettled turn in its transcript.
    pub fn session_busy(&self, id: &str) -> bool {
        if self.stops.pending(id) {
            return false;
        }
        if self.running.contains(id) || self.inbox_busy(id) || self.queue.starts.has(id) {
            return true;
        }
        if self.stops.has(id) {
            return false;
        }
        let transcript = rune::Tracked::untracked(&self.transcripts).get(id);
        if !self.live() || !self.stops.active_sync.synced() {
            return transcript.is_some_and(|t| self.transcript_unsettled(t));
        }
        // Keep optimistic busy for a freshly submitted prompt before `session.execution.started`
        // arrives, while never treating an idle server session's uncompleted assistant turn as busy.
        transcript.is_some_and(|t| matches!(self.last_settled_message(t), Some(opencode_kit::model::Message::User { .. })))
    }

    /// Whether `id` or any descendant sub-agent is busy (running, waiting on inbox/compaction
    /// work, or waiting on a permission request). Used by the timeline so a parent with a running
    /// or approval-waiting sub-agent never drops to idle/Copy (`session-request-tree.ts`,
    /// `project-avatar-state.ts:98-114`).
    pub fn subtree_busy(&self, id: &str) -> bool {
        if self.session_busy(id) {
            return true;
        }
        if self.stops.pending(id) {
            return false;
        }
        let root = self.root_of(id);
        self.running.iter().any(|x| x != id && self.descends(x, id))
            || self.inboxes.iter().any(|(x, inbox)| x != id && (!inbox.items.is_empty() || inbox.compaction.is_some()) && self.descends(x, id))
            || self.pending.iter().any(|p| matches!(p, Pending::Permission { .. }) && self.root_of(p.session()) == root)
    }

    /// The composer's own session, as Desktop's session status (`sessionBusy` for `id`, plus
    /// unsettled-turn fallback while `/api/session/active` has not yet synced). Detached
    /// background helpers keep the activity chip busy, not this session's Send button.
    pub fn busy(&self, id: &str) -> bool {
        self.session_busy(id)
    }

    pub(crate) fn route_busy(&self) -> bool {
        matches!(&*self.route, Route::Session { id, .. } if self.busy(id))
    }

    /// `+`, ⌘T and New session: a new "New session" draft tab, as Desktop's `openDraft`, where
    /// Settings › New sessions start in says (`start_location`): the server's default directory,
    /// or the open session's or draft's project. On Home and in Settings that's Home's project,
    /// else the last used or the first the server lists, and with neither it does nothing.
    pub fn new_session(&mut self, tx: &mut Tx) {
        let home = self.settings_open() || self.route == Route::Home;
        if self.project_navigation() && home {
            return view::home::browse::start(self, tx);
        }
        let here = if home {
            self.home_selected().or_else(|| self.last_used_project()).or_else(|| self.projects.first()).map(|p| p.directory.clone())
        } else {
            match &*self.route {
                Route::Session { id, .. } => self.session(id).map(|s| s.directory.clone()),
                Route::Draft(_) => Some(self.draft_dir().to_owned()),
                Route::Home | Route::Automations => None,
            }
        };
        let dir = start_location::resolve(self, here);
        if home && dir.is_none() {
            return;
        }
        self.close_settings();
        self.close_picker(false);
        self.close_project_switcher(false);
        self.project_state.home_menu_open = false;
        if let Some(dir) = dir.filter(|d| !d.is_empty()) {
            let server = crate::servers::runtime::active(self).clone();
            self.sidebar_inbox.branches.enqueue(&server, [dir.clone()]);
            *self.default_dir = dir;
        }
        *self.opening = None;
        self.open_draft_tab();
        if self.fixture_run.stage.is_none() && auto_focus_composer(tx) {
            self.composer.focused = true;
        }
        tx.settle();
    }
}
