//! Object-safe host boundary and pure-data DTOs for `opencode-feature-projects`.
//!
//! Every state machine, project resolver, Home synchronizer, import driver, job executor, and key
//! handler in this crate operates on `&dyn ProjectsHost` / `&mut dyn ProjectsHost`. Thin generic
//! bounds (`H: ProjectsHost + 'static`) appear strictly at `rune::El` view callbacks delegating
//! in one line to non-generic functions.

use std::path::PathBuf;

use opencode_kit::client::{Server, Write};
use opencode_kit::model::{ModelRef, Project, SessionInfo};
use opencode_kit::servers::{Route, ServerKey};
use opencode_servers::ServerConn;
use opencode_servers::health::Health;
use opencode_kit::wire_error::WireError;
use rune::{El, FileDialogOptions, Tx};
use serde_json::{Map, Value};
use opencode_ui::palette::Pal;

pub use opencode_runtime::TabSummary;

use crate::ProjectsState;
use crate::home::Home;
use crate::jobs::ProjectsJob;
use crate::remote_picker::Picker;

/// One server entry in the host's server registry (`servers::registry::Entry`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedServer {
    pub conn: ServerConn,
    pub name: String,
}

impl ListedServer {
    pub fn key(&self) -> ServerKey {
        self.conn.key()
    }
}

/// Default model and agent used when synthesizing a fixture-mode session in a newly opened project.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DefaultSelection {
    pub model: ModelRef,
    pub agent: String,
}

/// Callback invoked once a native folder-chooser dialog completes (`Ok(None)` when cancelled).
pub type FolderDialogDone = Box<dyn FnOnce(&mut dyn ProjectsHost, Result<Option<PathBuf>, String>, &mut Tx)>;

/// Callback invoked once a native file-chooser dialog completes (`Ok(None)` when cancelled).
pub type FileDialogDone = Box<dyn FnOnce(&mut dyn ProjectsHost, Result<Option<PathBuf>, String>, &mut Tx)>;

/// Object-safe host boundary for project tracking, Home across servers, session import, and the
/// remote folder picker dialog.
pub trait ProjectsHost: opencode_kernel::AppEnv + opencode_kernel::AppOverlays {
    // --- Feature state slots ---

    /// Project list, organize sidebar, and import state (tracked read).
    fn project_state(&self) -> &ProjectsState;
    /// Project list, organize sidebar, and import state (tracked write).
    fn project_state_mut(&mut self) -> &mut ProjectsState;

    /// Window's Home state (tracked read).
    fn home_state(&self) -> &Home;
    /// Window's Home state (tracked write).
    fn home_state_mut(&mut self) -> &mut Home;

    /// Remote directory picker dialog state (tracked read).
    fn remote_picker(&self) -> &Picker;
    /// Remote directory picker dialog state (tracked write).
    fn remote_picker_mut(&mut self) -> &mut Picker;

    // --- Projects, sessions, transcripts & tabs ---

    /// Server projects known on the currently checked-out server.
    fn projects(&self) -> &[Project];
    /// Mutable server projects on the currently checked-out server.
    fn projects_mut(&mut self) -> &mut Vec<Project>;
    /// Looks up project `id` on the currently checked-out server.
    fn project(&self, id: &str) -> Option<&Project> {
        self.projects().iter().find(|p| p.id == id)
    }
    /// Whether `project` has a known worktree or sandbox at normalized `dir`.
    fn has_worktree_at(&self, project: &Project, dir: &str) -> bool {
        let _ = (project, dir);
        false
    }
    /// Requests the worktree inventory for `project_id` on the currently checked-out server.
    fn list_worktrees(&mut self, project_id: &str) {
        let _ = project_id;
    }
    /// Re-sorts the host's projects list after project/session mutations.
    fn sort_projects(&mut self) {}

    /// Loaded sessions on the currently checked-out server.
    fn sessions(&self) -> &[SessionInfo] {
        &[]
    }
    /// Mutable sessions on the currently checked-out server.
    fn sessions_mut(&mut self) -> Option<&mut Vec<SessionInfo>> {
        None
    }
    /// Looks up session `id` on the currently checked-out server.
    fn session(&self, id: &str) -> Option<&SessionInfo> {
        self.sessions().iter().find(|s| s.id == id)
    }
    /// Recency timestamp used to order a project's sessions (defaults to `session.updated`).
    fn session_active_ms(&self, session: &SessionInfo) -> i64 {
        session.updated
    }
    /// Ensures an empty transcript entry exists for `session_id` before opening it.
    fn ensure_transcript(&mut self, session_id: &str) {
        let _ = session_id;
    }
    /// Default model and agent for newly created fixture-mode sessions.
    fn default_selection(&self) -> DefaultSelection {
        DefaultSelection::default()
    }

    /// Open window tabs in strip order.
    fn tabs(&self) -> Vec<TabSummary> {
        Vec::new()
    }
    /// Currently selected window tab, if any.
    fn current_tab(&self) -> Option<TabSummary> {
        None
    }
    /// Tab UID to restore when arriving on Home with project navigation enabled.
    fn restore_project_tab_uid(&self) -> Option<String> {
        None
    }
    /// Selects open window tab `uid`.
    fn select_tab(&mut self, uid: &str, tx: &mut Tx) {
        let _ = (uid, tx);
    }

    // --- Directories & routes ---

    /// User home directory displayed as `~`.
    fn home_dir(&self) -> &str {
        ""
    }
    /// Default directory where new drafts start.
    fn default_dir(&self) -> &str {
        ""
    }
    /// Updates the default directory for subsequent drafts.
    fn set_default_dir(&mut self, dir: String) {
        let _ = dir;
    }
    /// Updates the active draft's working directory (and default directory).
    fn set_draft_dir(&mut self, dir: String) {
        self.set_default_dir(dir);
    }
    /// Current window route (`opencode_kit::servers::Route`).
    fn current_route(&self) -> Route {
        Route::Home
    }
    /// Whether the window is currently displaying Home.
    fn is_home_route(&self) -> bool {
        self.current_route() == Route::Home
    }
    /// Whether Settings is currently open over the window.
    fn settings_open(&self) -> bool {
        false
    }
    /// Closes Settings if open.
    fn close_settings(&mut self) {}
    /// Whether project-centric vertical sidebar navigation is active (`project_navigation`).
    fn project_navigation(&self) -> bool {
        false
    }

    // --- Multi-server runtime & health ---

    /// Server key the window is currently checked out on.
    fn active_server(&self) -> ServerKey {
        ServerKey::sidecar()
    }
    /// Live server client on the currently checked-out server, or `None` in fixture mode.
    fn live_server(&self) -> Option<&Server> {
        None
    }
    /// Base URL of the app's own built-in sidecar server, if running.
    fn own_server_base(&self) -> Option<String> {
        None
    }
    /// All known servers in registry order (sidecar, added HTTP servers, listed plugin servers).
    fn listed_servers(&self) -> Vec<ListedServer> {
        Vec::new()
    }
    /// Whether `key` is currently present in the runtime's active server list (used to close a
    /// remote folder picker when its server disappears).
    fn server_is_listed_in_runtime(&self, key: &ServerKey) -> bool {
        self.listed_servers().iter().any(|e| e.key() == *key)
    }
    /// Current health status for server `key`.
    fn server_health(&self, key: &ServerKey) -> Option<Health> {
        let _ = key;
        None
    }
    /// Display label for server connection `conn` (`servers::registry::label`).
    fn server_label(&self, conn: &ServerConn) -> String {
        opencode_servers::registry::server_label(conn, conn.builtin(), None, opencode_i18n::t!("server.thisComputer"))
    }
    /// Display label for server `key` (`servers::registry::label_of`).
    fn server_label_of(&self, key: &ServerKey) -> String {
        self.listed_servers()
            .into_iter()
            .find(|e| e.key() == *key)
            .map_or_else(|| key.as_str().to_owned(), |e| self.server_label(&e.conn))
    }
    /// HTTP client handle for `conn`, or `None` if `conn` has no reachable endpoint yet.
    fn server_client(&self, conn: &ServerConn) -> Option<Server> {
        let http = conn.http()?;
        let server = Server::new(&http.url);
        Some(match &http.password {
            Some(p) => server.with_password(p),
            None => server,
        })
    }
    /// Whether `key`'s first reads have landed (`launch::Phase::Connected`).
    fn is_server_connected(&self, key: &ServerKey) -> bool {
        let _ = key;
        true
    }
    /// Whether the sessions listed for `key` are the ones it listed before this launch, kept in
    /// the app's saved state and shown until its first reads land.
    fn lists_saved_sessions(&self, key: &ServerKey) -> bool {
        let _ = key;
        false
    }
    /// Whether all server sources (including plugin server sources) have finished initial hydration.
    fn server_sources_hydrated(&self) -> bool {
        true
    }
    /// Stamp of every host input [`crate::home::poll`] reads when syncing Home's server list
    /// (server registry, runtime listing, health, locale, active/parked projects, sessions,
    /// recent activity, launch connection phases, and worktrees).
    fn home_sync_stamp(&self) -> u64 {
        0
    }
    /// Whether this window may send mutating writes to the live server.
    fn can_write(&self) -> bool {
        self.is_live() && !self.is_stage()
    }
    /// Whether the active live launch is currently offline.
    fn is_offline(&self) -> bool {
        false
    }
    /// Whether a Storybook stage is mounted over the window.
    fn is_stage(&self) -> bool {
        false
    }
    /// Switches the window's checked-out server to `key`.
    fn switch_server(&mut self, key: &ServerKey) {
        let _ = key;
    }
    /// Runs `f` checked out on a parked `server`, restoring the previous checkout afterwards.
    /// Returns `false` if the window has no half for `server`.
    fn with_server(&mut self, server: &ServerKey, f: &mut dyn FnMut(&mut dyn ProjectsHost)) -> bool {
        let _ = (server, f);
        false
    }
    /// Sends a queued write command (`Write::OpenProject`, etc.) to the active live server.
    fn write_server(&mut self, write: Write) -> bool {
        let _ = write;
        false
    }
    /// Triggers a `/api/project` refresh request on the active live server.
    fn request_projects(&self) {}
    /// Retries server health probes and offline launch reconnection.
    fn retry_servers(&mut self, tx: &mut Tx) {
        let _ = tx;
    }

    // --- Persistence, environment & timing ---

    /// Whether a user (or `--harness-writes`) attends this window and global state may be saved.
    fn attended(&self) -> bool {
        !rune::is_headless()
    }

    // --- Storage ---

    /// Reads a JSON value from `(namespace, key)` in persistent storage.
    fn storage_read_json(&self, namespace: &str, key: &str) -> Option<Value> {
        let _ = (namespace, key);
        None
    }
    /// Persists a JSON value under `(namespace, key)` in persistent storage.
    fn storage_write_json(&mut self, namespace: &str, key: &str, value: &Value) {
        let _ = (namespace, key, value);
    }

    // --- Status, toasts & error formatting ---

    /// Updates the debug status bar text.
    fn set_status(&mut self, status: String) {
        let _ = status;
    }
    /// Shows a failure toast.
    fn toast_fail(&mut self, title: &str, description: &str) {
        let _ = (title, description);
    }
    /// Shows an informational/success toast.
    fn toast_say(&mut self, title: &str, description: &str) {
        let _ = (title, description);
    }
    /// Shows a persistent waiting toast and returns its dismissible ID.
    fn toast_waiting(&mut self, message: &str) -> u64 {
        let _ = message;
        0
    }
    /// Dismisses a previously created toast by ID.
    fn toast_dismiss(&mut self, id: u64) {
        let _ = id;
    }
    /// Formats a typed [`WireError`] into a human-readable sentence naming the server.
    fn format_wire_error(&self, what: &str, error: &WireError) -> String {
        let _ = what;
        error.to_string()
    }

    // --- Native dialogs, navigation & visuals ---

    /// Closes conflicting popovers/dialogs before opening the native folder picker and returns
    /// whether composer focus should be restored if the user cancels.
    fn before_open_project_dialog(&mut self) -> bool {
        false
    }
    /// Restores composer focus after a cancelled folder dialog when appropriate.
    fn restore_composer_focus(&mut self) {}
    /// Opens a native folder chooser and invokes `on_done` with the result.
    fn open_folder_dialog(&mut self, options: FileDialogOptions, tx: &mut Tx, on_done: FolderDialogDone) {
        drop((options, tx, on_done));
    }
    /// Opens a native file chooser and invokes `on_done` with the result.
    fn open_file_dialog(&mut self, options: FileDialogOptions, tx: &mut Tx, on_done: FileDialogDone) {
        drop((options, tx, on_done));
    }
    /// Opens the OS/host project picker dialog (`⌘O`).
    ///
    /// Hosts can override this when opening a project needs host-specific chrome; the crate's
    /// standard flow is [`crate::open_project_dialog`].
    fn open_project_dialog(&mut self, tx: &mut Tx) {
        let _ = tx;
    }
    /// Opens (or updates) a new-session draft in `directory` on `server`.
    fn open_draft_in(&mut self, server: &ServerKey, directory: &str, tx: &mut Tx) {
        let _ = (server, directory, tx);
    }
    /// The folder picked for a session whose own is gone (its card's Choose directory), or none
    /// when the pick ended with nothing; `attempt` is the pick the card asked for.
    fn session_location_picked(&mut self, attempt: rune::machine::Attempt, directory: Option<String>, tx: &mut Tx) {
        let _ = (attempt, directory, tx);
    }
    /// Opens session `id` on `server` (checking out `server` if parked).
    fn open_session(&mut self, server: &ServerKey, id: &str, tx: &mut Tx) {
        let _ = (server, id, tx);
    }
    /// Opens session `id` immediately on the currently checked-out server (`open_now`).
    fn open_session_now(&mut self, id: &str, tx: &mut Tx) {
        let server = self.active_server();
        self.open_session(&server, id, tx);
    }
    /// Renders a project's avatar/badge for remote directory picker recent chips.
    fn project_badge(&self, p: &Pal, project: Option<&Project>, name: &str) -> El {
        let _ = (p, project, name);
        rune::el()
    }

    // --- Jobs ---

    /// Schedules an off-thread server request (session import upload or remote directory listing).
    fn spawn_job(&mut self, job: ProjectsJob) {
        let _ = job;
    }
}

/// Runs `f` checked out on `server`, returning `None` if `host` has no half for `server`.
pub fn on_server<R>(
    host: &mut dyn ProjectsHost,
    server: &ServerKey,
    f: impl FnOnce(&mut dyn ProjectsHost) -> R,
) -> Option<R> {
    if *server == host.active_server() {
        return Some(f(host));
    }
    let mut slot = Some(f);
    let mut out = None;
    let ran = host.with_server(server, &mut |h| {
        if let Some(cb) = slot.take() {
            out = Some(cb(h));
        }
    });
    ran.then_some(out).flatten()
}

/// Reads an object under `(namespace, key)`, mutates it via `f`, and writes back only if changed.
pub(crate) fn storage_update_host(
    host: &mut dyn ProjectsHost,
    namespace: &str,
    key: &str,
    f: impl FnOnce(&mut Map<String, Value>),
) {
    let before = host
        .storage_read_json(namespace, key)
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(Map::new()));
    let mut after = before.clone();
    if let Some(map) = after.as_object_mut() {
        f(map);
    }
    if after != before {
        host.storage_write_json(namespace, key, &after);
    }
}
