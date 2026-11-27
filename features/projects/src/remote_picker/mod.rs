//! Choosing a folder on a server (`workspaces/selection/picker.tsx`, `dialog.tsx`, `domain.ts`):
//! this computer's folder dialog for the app's own server (and one on its loopback port), and
//! for any other server, whose folders aren't this computer's (WSL, SSH, another machine),
//! Desktop's `DirectoryPickerDialog`: a path field with suggestions, ~ / Root / Parent, the
//! server's recent projects, and its folders as a tree read through `/api/fs/list`, then Select
//! folder. The dialog closes, cancelling the pick, if its server goes.
//!
//! The dialog is a state machine ([`Tree`], [`Suggest`]): every listing carries its attempt, and
//! an answer for another changes nothing.

pub mod view;

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use opencode_kit::client::Server;
use opencode_kit::model::{Project, parse_project, project_git};
use opencode_kit::servers::ServerKey;
use opencode_kit::servers::fs::{absolute_path, canonical};
use opencode_search::query::percent_encode;
use rune::machine::{Attempt, Seq};
use rune::{FileDialogOptions, Key, KeyEvent, TextEdit, Tx};

use crate::host::{ProjectsHost, on_server};
use crate::jobs::{ProjectsJob, RemoteRequest};
use crate::{REMOTE_PICKER_OVERLAY_ID, clean_dir};

/// What runs with the folder picked.
pub type OnPick = Rc<dyn Fn(&mut dyn ProjectsHost, String, &mut Tx)>;
/// What runs when a pick ends with nothing.
pub type OnCancel = Rc<dyn Fn(&mut dyn ProjectsHost, &mut Tx)>;

/// The dialog's open/closed phase (`DirectoryPickerDialog`).
#[derive(Default)]
pub enum PickerState {
    #[default]
    Closed,
    Open(Box<Open>),
}

/// The remote directory picker dialog state, holding a fallback editor so late `on_edit`
/// callbacks never panic if fired after close.
#[derive(Default)]
pub struct Picker {
    pub state: PickerState,
    scratch_input: TextEdit,
}

impl Picker {
    pub fn is_open(&self) -> bool {
        matches!(self.state, PickerState::Open(_))
    }

    pub fn open(&self) -> Option<&Open> {
        match &self.state {
            PickerState::Open(o) => Some(o),
            PickerState::Closed => None,
        }
    }

    pub fn open_mut(&mut self) -> Option<&mut Open> {
        match &mut self.state {
            PickerState::Open(o) => Some(o),
            PickerState::Closed => None,
        }
    }

    pub fn set_open(&mut self, open: Open) {
        self.state = PickerState::Open(Box::new(open));
    }

    pub fn take_open(&mut self) -> Option<Box<Open>> {
        match std::mem::take(&mut self.state) {
            PickerState::Open(o) => Some(o),
            PickerState::Closed => None,
        }
    }

    pub(crate) fn input_mut(&mut self) -> &mut TextEdit {
        match &mut self.state {
            PickerState::Open(o) => &mut o.input,
            PickerState::Closed => &mut self.scratch_input,
        }
    }
}

pub struct Open {
    pub server: ServerKey,
    conn: Server,
    pub title: String,
    /// The server's name, under the title (`directory-picker-subtitle`).
    pub subtitle: String,
    /// The server's home folder, for `~` (`sync.data.path.home`).
    pub home: String,
    pub tree: Tree,
    pub input: TextEdit,
    pub suggest: Suggest,
    pub recent: Vec<Project>,
    /// Git projects the server knows, which their folders' rows mark.
    git: BTreeSet<String>,
    seq: Seq,
    on_pick: OnPick,
    on_cancel: Option<OnCancel>,
}

/// One folder's children (`PickerNode`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    Loading { attempt: Attempt },
    Loaded(Vec<Dir>),
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dir {
    pub name: String,
    pub absolute: String,
}

/// The folder tree (`tree-machine.ts`): where the server is, then a root and its folders.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tree {
    /// Asking the server for its home and location.
    Locating { attempt: Attempt },
    Browsing { root: String, nodes: BTreeMap<String, Node>, expanded: BTreeSet<String>, selected: Option<String> },
    Failed,
}

/// The path field's suggestions (`combobox-machine.ts`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Suggest {
    #[default]
    Closed,
    Searching { attempt: Attempt, query: String },
    Shown { items: Vec<String>, active: Option<usize> },
    MoreProjects { active: usize },
}

impl Tree {
    /// The folder Select folder picks: the one selected, else the root.
    pub fn result(&self) -> Option<&str> {
        match self {
            Tree::Browsing { selected: Some(s), .. } => Some(s),
            Tree::Browsing { root, .. } => Some(root),
            _ => None,
        }
    }
}

/// Picks a folder on server `server`, then runs `on_pick` with it.
pub fn pick(host: &mut dyn ProjectsHost, server: &ServerKey, title: String, tx: &mut Tx, on_pick: OnPick) {
    pick_settling(host, server, title, tx, on_pick, None);
}

/// [`pick`], and `on_cancel` runs when the pick ends with nothing: Cancel, Escape, a press
/// outside, a folder dialog that failed, another pick taking its place, or its server going.
pub fn pick_settling(host: &mut dyn ProjectsHost, server: &ServerKey, title: String, tx: &mut Tx, on_pick: OnPick, on_cancel: Option<OnCancel>) {
    let entry = host.listed_servers().into_iter().find(|e| e.key() == *server);
    let own = host.own_server_base();
    let local = entry.as_ref().is_none_or(|e| e.conn.local(own.as_deref())) || !host.is_live();
    tx.instant();
    if local {
        let options = FileDialogOptions { title, ..Default::default() };
        host.open_folder_dialog(
            options,
            tx,
            Box::new(move |h, result, tx| {
                match result {
                    Ok(Some(path)) => return on_pick(h, clean_dir(&path.to_string_lossy()), tx),
                    Ok(None) => {}
                    Err(e) => h.toast_fail(opencode_i18n::t!("rune.status.folderPickerFailed"), &e),
                }
                if let Some(cancel) = &on_cancel {
                    cancel(h, tx);
                }
            }),
        );
        return;
    }
    // Without a connection to it the picker would open on nothing: the pick ends there.
    let Some(conn) = entry.as_ref().and_then(|e| host.server_client(&e.conn)) else {
        if let Some(cancel) = &on_cancel {
            cancel(host, tx);
        }
        return;
    };
    let (recent, git) = on_server(host, server, |h| {
        let mut seen = BTreeSet::new();
        // `[...projects.list(), ...sync.data.project]`: the server's projects in its own order, most
        // recently active first, then by id (`desc(time_active), asc(id)`, `sync.tsx:136-138`).
        let mut all: Vec<&Project> = h.projects().iter().collect();
        all.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| a.id.cmp(&b.id)));
        let recent: Vec<Project> = all
            .into_iter()
            .filter(|p| p.id != "global" && !p.directory.is_empty() && seen.insert(clean_dir(&p.directory)))
            .take(10)
            .cloned()
            .collect();
        let git = h.projects().iter().filter(|p| project_git(&p.id)).map(|p| canonical(&p.directory)).collect();
        (recent, git)
    })
    .unwrap_or_default();
    let was_open = host.remote_picker().is_open();
    // The pick this one takes the place of ends with nothing.
    if let Some(cancel) = host.remote_picker_mut().take_open().and_then(|old| old.on_cancel) {
        cancel(host, tx);
    }
    let return_focus = tx.focused().map(str::to_owned);
    let subtitle = host.server_label_of(server);
    let mut seq = Seq::default();
    let attempt = seq.next();
    let mut input = TextEdit::new("");
    input.focused = true;
    host.remote_picker_mut().set_open(Open {
        server: server.clone(),
        subtitle,
        conn: conn.clone(),
        title,
        home: String::new(),
        tree: Tree::Locating { attempt },
        input,
        suggest: Suggest::Closed,
        recent,
        git,
        seq,
        on_pick,
        on_cancel,
    });
    host.project_state_mut().home_menu_open = false;
    if !was_open {
        host.on_overlay_opened(REMOTE_PICKER_OVERLAY_ID, return_focus);
    }
    host.spawn_job(ProjectsJob::Remote { server: conn, request: RemoteRequest::Locate { attempt } });
}

/// Convenience wrapper around [`pick`] taking a closure directly.
pub fn pick_with(
    host: &mut dyn ProjectsHost,
    server: &ServerKey,
    title: String,
    tx: &mut Tx,
    on_pick: impl Fn(&mut dyn ProjectsHost, String, &mut Tx) + 'static,
) {
    pick(host, server, title, tx, Rc::new(on_pick));
}

/// Opens a static Storybook preview of the remote folder picker dialog (`remote-folders` scene).
pub fn open_preview(host: &mut dyn ProjectsHost) {
    let was_open = host.remote_picker().is_open();
    let root = "/workspace".to_owned();
    let recent: Vec<Project> = ["desktop", "docs", "runtime", "tools", "website"]
        .iter()
        .filter_map(|name| parse_project(&serde_json::json!({ "id": name, "canonical": format!("{root}/{name}") })))
        .collect();
    let dirs = recent.iter().map(|p| Dir { name: p.name.clone(), absolute: p.directory.clone() }).collect();
    let tree = Tree::Browsing {
        root: root.clone(),
        nodes: BTreeMap::from([(root.clone(), Node::Loaded(dirs))]),
        expanded: BTreeSet::new(),
        selected: None,
    };
    let mut open = Open {
        server: host.active_server(),
        conn: Server::anonymous("http://localhost"),
        title: opencode_i18n::t!("command.project.open").into(),
        subtitle: "Development server".into(),
        home: root.clone(),
        tree,
        recent,
        input: TextEdit::new(""),
        suggest: Suggest::Closed,
        git: BTreeSet::new(),
        seq: Seq::default(),
        on_pick: Rc::new(|h, dir, _| h.set_draft_dir(dir)),
        on_cancel: None,
    };
    open.sync_input(&root);
    host.remote_picker_mut().set_open(open);
    if !was_open {
        host.on_overlay_opened(REMOTE_PICKER_OVERLAY_ID, None);
    }
}

/// The server said where it is: the tree opens on its home (`start`).
pub(crate) fn apply_located(
    host: &mut dyn ProjectsHost,
    attempt: Attempt,
    home: Option<String>,
    location: Option<String>,
    tx: &mut Tx,
) {
    let start = {
        let Some(o) = host.remote_picker_mut().open_mut() else { return };
        if o.tree != (Tree::Locating { attempt }) {
            return;
        }
        let home = home.map(|h| canonical(&h)).unwrap_or_default();
        let start = if home.is_empty() { location.map(|l| canonical(&l)).unwrap_or_default() } else { home.clone() };
        o.home = home;
        if start.is_empty() {
            o.tree = Tree::Failed;
            return tx.instant();
        }
        start
    };
    navigate(host, start, tx);
}

/// `inferHomeDirectory`: `/Users/<you>`, `/home/<you>`, `/root` or `C:/Users/<you>` that `dir`
/// starts with.
pub(crate) fn infer_home(dir: &str) -> String {
    canonical(&opencode_kit::path::infer_home(dir))
}

/// The tree opens on `root`, which is selected (`navigate`).
pub fn navigate(host: &mut dyn ProjectsHost, root: String, tx: &mut Tx) {
    let (conn, root, attempt) = {
        let Some(o) = host.remote_picker_mut().open_mut() else { return };
        let attempt = o.seq.next();
        let root = canonical(&root);
        o.sync_input(&root);
        o.tree = Tree::Browsing {
            root: root.clone(),
            nodes: BTreeMap::from([(root.clone(), Node::Loading { attempt })]),
            expanded: BTreeSet::new(),
            selected: None,
        };
        o.suggest = Suggest::Closed;
        (o.conn.clone(), root, attempt)
    };
    tx.instant();
    host.spawn_job(ProjectsJob::Remote { server: conn, request: RemoteRequest::ListDirs { dir: root, attempt } });
}

/// A folder's row: it's selected, and opens or shuts; opening reads it once.
pub fn toggle(host: &mut dyn ProjectsHost, dir: &str, tx: &mut Tx) {
    let job = {
        let Some(o) = host.remote_picker_mut().open_mut() else { return };
        o.sync_input(dir);
        let Tree::Browsing { nodes, expanded, selected, .. } = &mut o.tree else { return };
        *selected = Some(dir.to_owned());
        tx.instant();
        if !expanded.remove(dir) {
            expanded.insert(dir.to_owned());
            if matches!(nodes.get(dir), None | Some(Node::Failed)) {
                let attempt = o.seq.next();
                nodes.insert(dir.to_owned(), Node::Loading { attempt });
                Some((o.conn.clone(), dir.to_owned(), attempt))
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some((conn, dir, attempt)) = job {
        host.spawn_job(ProjectsJob::Remote { server: conn, request: RemoteRequest::ListDirs { dir, attempt } });
    }
}

pub(crate) fn read_dirs(conn: &Server, dir: &str) -> Option<Vec<Dir>> {
    let v = conn.get(&format!("/api/fs/list?location%5Bdirectory%5D={}&path={}", percent_encode(dir), percent_encode(dir))).ok()?;
    let location = v["location"]["directory"].as_str().unwrap_or(dir).to_owned();
    let mut dirs: Vec<Dir> = v["data"]
        .as_array()?
        .iter()
        .filter(|e| e["type"].as_str() == Some("directory"))
        .filter_map(|e| {
            let absolute = absolute_path(e["path"].as_str()?, &location);
            Some(Dir { name: opencode_kit::path::basename(&absolute).to_owned(), absolute })
        })
        .collect();
    dirs.sort_by(|a, b| rune::text::compare_names(&a.name, &b.name));
    Some(dirs)
}

pub(crate) fn apply_listed(
    host: &mut dyn ProjectsHost,
    dir: String,
    attempt: Attempt,
    dirs: Option<Vec<Dir>>,
    tx: &mut Tx,
) {
    let Some(o) = host.remote_picker_mut().open_mut() else { return };
    if let Suggest::Searching { attempt: a, query } = &o.suggest
        && *a == attempt
    {
        let prefix = query.rsplit('/').next().unwrap_or_default().to_lowercase();
        let items: Vec<String> = dirs
            .unwrap_or_default()
            .into_iter()
            .filter(|d| d.name.to_lowercase().starts_with(&prefix))
            .take(5)
            .map(|d| d.absolute)
            .collect();
        let folder = o.input.text == "~" || o.input.text.ends_with(['/', '\\']);
        o.suggest = if items.is_empty() { Suggest::Closed } else { Suggest::Shown { items, active: (!folder).then_some(0) } };
        return tx.instant();
    }
    let Tree::Browsing { nodes, .. } = &mut o.tree else { return };
    if nodes.get(&dir) != Some(&Node::Loading { attempt }) {
        return;
    }
    nodes.insert(dir, dirs.map_or(Node::Failed, Node::Loaded));
    tx.instant();
}

/// The path field changed: its folder's matching folders are suggested.
pub fn typed(host: &mut dyn ProjectsHost, tx: &mut Tx) {
    let job = {
        let Some(o) = host.remote_picker_mut().open_mut() else { return };
        let value = o.input.text.trim().to_owned();
        tx.instant();
        if value.is_empty() {
            o.suggest = Suggest::Closed;
            return;
        }
        let base = o.tree.result().map(str::to_owned).unwrap_or_else(|| o.home.clone());
        let absolute = resolve(&value, &o.home, &base);
        let parent = if value.ends_with('/') || value.ends_with('\\') {
            absolute.clone()
        } else {
            opencode_kit::path::parent(&absolute).to_owned()
        };
        let attempt = o.seq.next();
        o.suggest = Suggest::Searching { attempt, query: absolute };
        (o.conn.clone(), canonical(&parent), attempt)
    };
    host.spawn_job(ProjectsJob::Remote { server: job.0, request: RemoteRequest::ListDirs { dir: job.1, attempt: job.2 } });
}

/// `pickerAbsoluteInput`: `~` and `~/…` from home, a rooted path as it is, else from `base`.
fn resolve(value: &str, home: &str, base: &str) -> String {
    let value = value.replace('\\', "/");
    if value == "~" {
        return home.to_owned();
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return absolute_path(rest, home);
    }
    absolute_path(&value, base)
}

/// The path field's keys: ↑ ↓ move through suggestions, ↩ opens the highlighted one or the
/// typed path, Escape closes the suggestions, then the dialog.
pub fn key(host: &mut dyn ProjectsHost, ev: &KeyEvent, tx: &mut Tx) -> bool {
    enum Action {
        Handled,
        Ignored,
        Choose(String),
        Close,
    }
    let action = {
        let Some(o) = host.remote_picker_mut().open_mut() else { return false };
        if o.input.is_composing() {
            return false;
        }
        match (&mut o.suggest, &ev.key) {
            (Suggest::Shown { items, active }, Key::Down | Key::Up) => {
                if !items.is_empty() {
                    let n = items.len();
                    *active = Some(if ev.key == Key::Down {
                        active.map_or(0, |i| (i + 1) % n)
                    } else {
                        active.map_or(n - 1, |i| (i + n - 1) % n)
                    });
                    tx.instant();
                }
                Action::Handled
            }
            (Suggest::Shown { items, active: Some(active) }, Key::Enter) => match items.get(*active).cloned() {
                Some(path) => Action::Choose(path),
                None => Action::Handled,
            },
            (Suggest::Shown { .. }, Key::Escape) => {
                o.suggest = Suggest::Closed;
                tx.instant();
                Action::Handled
            }
            (Suggest::MoreProjects { .. }, Key::Escape) => {
                o.suggest = Suggest::Closed;
                tx.instant();
                tx.focus("directory-picker-more-projects", true);
                Action::Handled
            }
            (_, Key::Enter) if !o.input.text.trim().is_empty() => {
                let base = o.tree.result().map(str::to_owned).unwrap_or_else(|| o.home.clone());
                Action::Choose(resolve(o.input.text.trim(), &o.home, &base))
            }
            (_, Key::Escape) => {
                let selected = o.tree.result().unwrap_or("").to_owned();
                if o.input.text != display(&selected, &o.home) {
                    o.sync_input(&selected);
                    tx.instant();
                    Action::Handled
                } else {
                    Action::Close
                }
            }
            _ => Action::Ignored,
        }
    };
    match action {
        Action::Handled => true,
        Action::Ignored => false,
        Action::Choose(path) => {
            choose_input(host, path, tx);
            true
        }
        Action::Close => {
            close(host, tx);
            true
        }
    }
}

/// A suggestion or a typed path: the tree opens on it (`directory.chosen`, `fromInput`).
pub fn choose_input(host: &mut dyn ProjectsHost, path: String, tx: &mut Tx) {
    if let Some(o) = host.remote_picker_mut().open_mut() {
        o.input = TextEdit::new("");
        o.input.focused = true;
    }
    navigate(host, path, tx);
}

/// Select folder: the pick ends with the folder.
pub fn resolve_pick(host: &mut dyn ProjectsHost, tx: &mut Tx) {
    let Some(o) = host.remote_picker_mut().take_open() else { return };
    let typed = o.input.text.trim();
    let path = match &o.suggest {
        Suggest::Shown { items, active: Some(i) } => items.get(*i).cloned(),
        _ if !typed.is_empty() => Some(resolve(typed, &o.home, o.tree.result().unwrap_or(&o.home))),
        _ => o.tree.result().map(str::to_owned),
    };
    let Some(path) = path else {
        host.remote_picker_mut().state = PickerState::Open(o);
        return;
    };
    host.on_overlay_closed(REMOTE_PICKER_OVERLAY_ID);
    tx.instant();
    // The server's own separators: a Windows server's folder in backslashes, as it lists them.
    let path = if path.as_bytes().get(1) == Some(&b':') { path.replace('/', "\\") } else { path };
    (o.on_pick)(host, path, tx);
}

/// Dismisses the topmost layer inside the remote picker: closes the `"More projects"` overflow
/// menu (returning focus to its trigger) or path suggestions first, then closes the dialog.
pub fn dismiss(host: &mut dyn ProjectsHost, tx: &mut Tx) {
    let closed_sub = {
        let Some(o) = host.remote_picker_mut().open_mut() else { return };
        match o.suggest {
            Suggest::MoreProjects { .. } => {
                o.suggest = Suggest::Closed;
                tx.instant();
                tx.focus("directory-picker-more-projects", true);
                true
            }
            Suggest::Shown { .. } => {
                o.suggest = Suggest::Closed;
                tx.instant();
                true
            }
            _ => false,
        }
    };
    if !closed_sub {
        close(host, tx);
    }
}

/// Cancel, Escape, or a press outside: the pick ends with nothing.
pub fn close(host: &mut dyn ProjectsHost, tx: &mut Tx) {
    if let Some(o) = host.remote_picker_mut().take_open() {
        host.on_overlay_closed(REMOTE_PICKER_OVERLAY_ID);
        tx.instant();
        if let Some(cancel) = &o.on_cancel {
            cancel(host, tx);
        }
    }
}

/// Each poll: a picker whose server left the list closes (`picker.tsx:61-63`).
pub fn poll(host: &mut dyn ProjectsHost, tx: &mut Tx) -> bool {
    let Some(key) = host.remote_picker().open().map(|o| o.server.clone()) else { return false };
    if !host.is_live() || host.server_is_listed_in_runtime(&key) {
        return false;
    }
    close(host, tx);
    true
}

pub(crate) fn input_edit(host: &mut dyn ProjectsHost) -> &mut TextEdit {
    host.remote_picker_mut().input_mut()
}

/// Dialog-level keys for the remote picker's "More projects" overflow menu (`role="menu"`):
/// Down on `"More projects"` opens the menu; Up/Down/Home/End step `menuitem` rows, Enter/Space
/// opens the highlighted project, Tab closes the menu and lets focus advance, and Escape closes
/// the menu and returns focus to the `"More projects"` trigger.
pub fn overlay_key(host: &mut dyn ProjectsHost, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let is_space = matches!(&ev.key, Key::Text(t) if t == " ") && ev.mods == Default::default();
    let chosen = {
        let Some(o) = host.remote_picker_mut().open_mut() else { return false };
        if matches!(o.suggest, Suggest::Closed)
            && ev.key == Key::Down
            && ev.mods == Default::default()
            && tx.focused() == Some("directory-picker-more-projects")
            && o.recent.len() > 3
        {
            o.suggest = Suggest::MoreProjects { active: 0 };
            o.input.focused = false;
            tx.instant();
            tx.focus("more-project-0", true);
            return true;
        }
        let Suggest::MoreProjects { active } = &mut o.suggest else { return false };
        let count = o.recent.len().saturating_sub(3);
        if count == 0 {
            o.suggest = Suggest::Closed;
            return false;
        }
        match ev.key {
            Key::Tab => {
                o.suggest = Suggest::Closed;
                tx.instant();
                return false;
            }
            Key::Down => {
                *active = (*active + 1) % count;
                tx.instant();
                tx.focus(format!("more-project-{}", *active), true);
                return true;
            }
            Key::Up => {
                *active = (*active + count - 1) % count;
                tx.instant();
                tx.focus(format!("more-project-{}", *active), true);
                return true;
            }
            Key::Home => {
                *active = 0;
                tx.instant();
                tx.focus("more-project-0", true);
                return true;
            }
            Key::End => {
                *active = count - 1;
                tx.instant();
                tx.focus(format!("more-project-{}", *active), true);
                return true;
            }
            Key::Escape => {
                o.suggest = Suggest::Closed;
                tx.instant();
                tx.focus("directory-picker-more-projects", true);
                return true;
            }
            Key::Enter => o.recent.get(3 + *active).map(|p| p.directory.clone()),
            _ if is_space => o.recent.get(3 + *active).map(|p| p.directory.clone()),
            _ => return false,
        }
    };
    if let Some(dir) = chosen {
        navigate(host, dir, tx);
        return true;
    }
    false
}

pub(crate) fn toggle_more_projects(host: &mut dyn ProjectsHost, tx: &mut Tx) {
    if let Some(o) = host.remote_picker_mut().open_mut() {
        let opening = !matches!(o.suggest, Suggest::MoreProjects { .. });
        o.suggest = if opening && o.recent.len() > 3 {
            Suggest::MoreProjects { active: 0 }
        } else {
            Suggest::Closed
        };
        tx.instant();
        if opening && o.recent.len() > 3 {
            o.input.focused = false;
            tx.focus("more-project-0", tx.by_keys() || tx.focus_visible());
        } else {
            tx.focus("directory-picker-more-projects", tx.by_keys() || tx.focus_visible());
        }
    }
}

impl Open {
    fn sync_input(&mut self, dir: &str) {
        let value = display(dir, &self.home);
        self.input = TextEdit::new(&value);
        self.input.anchor = value.len();
        self.input.focus = value.len();
        self.input.focused = true;
        self.suggest = Suggest::Closed;
    }

    /// The folder's row shows "git repo" (`renderRowDecoration`).
    pub fn git(&self, dir: &str) -> bool {
        self.git.contains(&canonical(dir))
    }
}

/// `displayPickerPath`: a Windows path with backslashes, else `~` for home.
pub fn display(path: &str, home: &str) -> String {
    let windows = |p: &str| p.as_bytes().get(1) == Some(&b':');
    if windows(path) || windows(home) {
        return path.replace('/', "\\");
    }
    if !home.is_empty() && path == home {
        return "~".into();
    }
    match path.strip_prefix(&format!("{home}/")).filter(|_| !home.is_empty()) {
        Some(rest) => format!("~/{rest}"),
        None => path.to_owned(),
    }
}

/// `shortenPickerRecentPath`: the last two folders after `~/…/`, `/<first>/…/` or the drive.
pub fn shorten(path: &str, home: &str) -> String {
    let shown = display(path, home);
    let keep = |prefix: &str, rest: &str, sep: &str| {
        let parts: Vec<&str> = rest.split(sep).filter(|p| !p.is_empty()).collect();
        if parts.len() <= 2 { None } else { Some(format!("{prefix}…{sep}{}", parts[parts.len() - 2..].join(sep))) }
    };
    if let Some(rest) = shown.strip_prefix("~/") {
        return keep("~/", rest, "/").unwrap_or(shown);
    }
    if shown.starts_with('/') && !shown.starts_with("//") {
        let rest = &shown[1..];
        let first = rest.split('/').next().unwrap_or_default();
        return keep(&format!("/{first}/"), rest.trim_start_matches(first), "/").unwrap_or(shown);
    }
    if shown.as_bytes().get(1) == Some(&b':')
        && shown.split('\\').filter(|p| !p.is_empty()).count() > 3
        && let (Some(drive), Some(rest)) = (shown.get(..3), shown.get(3..))
    {
        return keep(drive, rest, "\\").unwrap_or(shown);
    }
    shown
}

/// `pickerRoot`: `/`, `C:/`, `//server/share`.
pub fn root_of(path: &str) -> String {
    if path.starts_with("//") {
        return path.splitn(5, '/').take(4).collect::<Vec<_>>().join("/");
    }
    if path.as_bytes().get(1) == Some(&b':')
        && let Some(drive) = path.get(..2)
    {
        return format!("{drive}/");
    }
    "/".into()
}

/// `pickerParent`.
pub fn parent_of(path: &str) -> String {
    let root = root_of(path);
    if path == root || path.len() <= root.len() {
        return root;
    }
    match path.rfind('/') {
        Some(i) if i >= root.len() => path[..i].to_owned(),
        _ => root,
    }
}
