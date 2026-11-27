//! Files panel (`packages/gui-extensions/src/file/`): workspace tree, file loading,
//! live watching, Markdown / HTML / table / Mermaid / image / video previews, inline-code path
//! resolution, and [`CodeViewer`](opencode_code_view::CodeViewer) integration.

#![forbid(unsafe_code)]

pub mod code_paths;
pub mod content;
pub mod data;
pub mod delimited;
pub mod host;
pub mod html;
pub mod image;
mod label_find;
pub mod link_tip;
pub mod links;
pub mod markdown;
mod mermaid;
pub mod narrow;
pub mod ops;
pub mod path_index;
pub mod poll;
mod table_view;
pub mod trace;
pub mod video;
pub mod view;
pub mod viewer;
pub mod watch;

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use opencode_code_view::CodeDocState;
use rune::{Latest, TextEdit, Waker};
use rune_kit::{FileNode, TreeModel};

use data::build_workspace_tree;

pub use code_paths::{
    CodePaths, PathCheck, check_code_paths, open_code_path, open_file_picker, reply_code_links,
};
pub use host::{
    FilesHost, FilesJob, FilesKeyMatch, FilesReply, MenuPick, OpenInCurrent, OpenInMenuOption,
    ResolvedCodeLink,
};
pub use links::{Link, link_directory, open_link, open_reply_link};
pub use markdown::{
    MdView, PreviewMode, PreviewModeEvent, READ_ICON, READ_W, WRAP_STORAGE_KEY, WRAP_STORAGE_NS,
    WrapPrefs, file_find_step, scroll_to_heading, set_file_wrap, sync_rendered_find,
    toggle_rendered,
};
pub use ops::{
    apply_reply, ensure_file_loaded, ensure_workspace_files, execute_job, filtered_files,
    filtered_sidebar_files, open_workspace_file, open_workspace_file_at,
    open_workspace_file_for_session, open_workspace_file_source, read_file, relist_workspace_dir, request_file_search,
    request_sidebar_file_search, start_read, submit_files_filter, toggle_workspace_dir,
};
pub use poll::{key_files, poll_files, poll_reply_links};

pub struct SessionFiles {
    pub docs: HashMap<String, CodeDocState>,
    pub known_files: BTreeSet<String>,
    pub(crate) paths: path_index::LivePaths,
    pub known_dirs: BTreeSet<String>,
    pub expanded_dirs: BTreeSet<String>,
    pub listed_dirs: BTreeSet<String>,
    pub tree: TreeModel<FileNode<bool>>,
    pub recent: Vec<String>,
    pub highlighted_filter_path: Option<String>,
    /// The server's search for the filter's query.
    pub search: Search,
    pub pending_jump: HashMap<String, (u32, u32)>,
    /// Files a link asked for, opened once they load.
    pub link_opens: HashMap<String, LinkOpen>,
    pub md: HashMap<String, markdown::MdView>,
    /// Decoded CSV/TSV documents. A failed or replaced read drops the previous revision.
    pub(crate) tables: HashMap<String, std::sync::Arc<delimited::Table>>,
    pub(crate) table_find: label_find::TableFind,
    pub(crate) html: HashMap<String, std::sync::Arc<html::Page>>,
    pub(crate) html_find: label_find::LabelFind<std::sync::Arc<html::Page>>,
    pub(crate) diagram_find: label_find::DiagramFind,
    pub(crate) diagram: mermaid::Stage,
    /// Each Markdown file's inline code paths, checked (`code_paths`).
    pub code_paths: HashMap<String, code_paths::CodePaths>,
    /// Picture files (and SVGs) as decoded images, with their zoom.
    pub images: HashMap<String, image::FileImage>,
    /// Video files, each with its player and controls.
    pub videos: HashMap<String, video::FileVideo>,
    /// Root workspace directory listing lifecycle (`packages/app/src/workspaces/files/tree-machine.ts`).
    pub tree_load: TreeLoad,
    /// What the narrow window's Files view shows (`narrow`).
    pub narrow: narrow::NarrowFiles,
}

/// Root workspace directory listing lifecycle (`packages/app/src/workspaces/files/tree-machine.ts`
/// `DirectoryState` for the root `""`: `idle` -> `loading` -> `ready`, or `idle` -> `ready` for
/// fixtures).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TreeLoad {
    /// Root listing has not been requested yet.
    #[default]
    Idle,
    /// Live root listing request is in flight.
    Loading,
    /// Initial workspace listing (or fixture) has landed.
    Ready,
}

/// Events driving [`TreeLoad`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeLoadEvent {
    /// Request a live listing of the workspace root.
    RequestLive,
    /// A directory listing or fixture load completed.
    Loaded,
}

/// Effects returned by [`TreeLoad::step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeLoadEffect {
    None,
    /// Issue the root directory fetch against the live server.
    FetchRoot,
}

impl TreeLoad {
    pub fn is_idle(self) -> bool {
        matches!(self, Self::Idle)
    }

    pub fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }

    pub fn step(&mut self, event: TreeLoadEvent) -> TreeLoadEffect {
        match (*self, event) {
            (Self::Idle, TreeLoadEvent::RequestLive) => {
                *self = Self::Loading;
                TreeLoadEffect::FetchRoot
            }
            (Self::Loading | Self::Ready, TreeLoadEvent::RequestLive) => TreeLoadEffect::None,
            (_, TreeLoadEvent::Loaded) => {
                *self = Self::Ready;
                TreeLoadEffect::None
            }
        }
    }
}

/// The server's search for the files a filter names, as Desktop's
/// `gui-plugins/src/file/search-machine.ts` (`FileSearchState`): idle, searching a query (the
/// last results stay listed meanwhile, as its `previous`), or that query's results.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Search {
    #[default]
    Idle,
    Searching { query: String, previous: Vec<String> },
    Ready { query: String, files: Vec<String> },
}

impl Search {
    /// The query asked last, answered or not.
    pub fn asked(&self) -> Option<&str> {
        match self {
            Search::Idle => None,
            Search::Searching { query, .. } | Search::Ready { query, .. } => Some(query),
        }
    }

    /// The files found: the last answer, while the next query loads too.
    pub fn found(&self) -> &[String] {
        match self {
            Search::Idle => &[],
            Search::Searching { previous, .. } => previous,
            Search::Ready { files, .. } => files,
        }
    }

    /// Asks for query; false when it was asked already.
    pub(crate) fn ask(&mut self, query: &str) -> bool {
        if self.asked() == Some(query) {
            return false;
        }
        let previous = std::mem::take(self).found().to_vec();
        *self = Search::Searching { query: query.to_owned(), previous };
        true
    }

    /// The answer to query; an answer to an earlier query is stale. True when it landed.
    pub(crate) fn answered(&mut self, query: &str, files: Vec<String>) -> bool {
        if !matches!(self, Search::Searching { query: q, .. } if q == query) {
            return false;
        }
        *self = Search::Ready { query: query.to_owned(), files };
        true
    }
}

/// Where a link asked to land in the file it opens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkOpen {
    pub lines: Option<(u32, u32)>,
    pub heading: Option<String>,
}

impl Default for SessionFiles {
    fn default() -> Self {
        Self {
            docs: HashMap::new(),
            known_files: BTreeSet::new(),
            paths: Default::default(),
            known_dirs: BTreeSet::new(),
            expanded_dirs: BTreeSet::new(),
            listed_dirs: BTreeSet::new(),
            tree: TreeModel::new().key("files-tree"),
            recent: Vec::new(),
            highlighted_filter_path: None,
            search: Search::Idle,
            pending_jump: HashMap::new(),
            link_opens: HashMap::new(),
            md: HashMap::new(),
            tables: HashMap::new(),
            table_find: Default::default(),
            html: HashMap::new(),
            html_find: Default::default(),
            diagram_find: Default::default(),
            diagram: Default::default(),
            code_paths: HashMap::new(),
            images: HashMap::new(),
            videos: HashMap::new(),
            tree_load: TreeLoad::Idle,
            narrow: narrow::NarrowFiles::default(),
        }
    }
}

impl SessionFiles {
    /// Whether watcher unlinks in `dir` should relist it (`tree-machine.ts::canRefreshDirectory`).
    pub(crate) fn can_relist(&self, dir: &str) -> bool {
        (dir.is_empty() && !self.tree_load.is_idle()) || self.listed_dirs.contains(dir)
    }

    /// A reread starts with no rendered document. Drop the projection at the same time as the
    /// source: two revisions with the same path, dimensions and query can have different hits.
    pub fn invalidate_document(&mut self, path: &str) {
        self.close_preview(path);
        self.docs.remove(path);
        if let Some(previous) = self.tables.remove(path) {
            self.table_find.forget(&previous);
        }
        if let Some(previous) = self.html.remove(path) {
            self.html_find.forget(&previous);
        }
        self.md.remove(path);
    }

    /// Only a loaded, shown table is a Find target. A table in the source view keeps CodeViewer's
    /// ordinary selection, scroll and match ranges.
    pub(crate) fn shown_table(&self, path: &str) -> Option<&std::sync::Arc<delimited::Table>> {
        if self.md.get(path).is_some_and(|v| v.mode.is_forced_source()) {
            return None;
        }
        self.tables.get(path).filter(|_| {
            self.docs
                .get(path)
                .is_some_and(|d| d.content().is_text())
        })
    }

    /// (Active index, count) for both the visible bar and its polite announcement.
    pub fn file_matches(&self, path: &str, doc: &CodeDocState) -> (usize, usize) {
        if let Some(page) = self.shown_html(path) {
            self.html_find.count(page)
        } else if let Some(table) = self.shown_table(path) {
            self.table_find.count(table)
        } else if let Some(drawing) = self.shown_diagram(path) {
            self.diagram_find.count(&drawing)
        } else if markdown::shows_rendered(self, path) {
            self.md.get(path).map_or((0, 0), |v| (v.active, v.matches.len()))
        } else {
            (doc.active_match_index(), doc.match_count())
        }
    }

    /// Resets rendered-preview state when switching to source view while keeping the loaded
    /// document and decoded media in memory.
    pub fn close_preview(&mut self, path: &str) {
        if let Some(doc) = self.docs.get(path)
            && let Some(drawing) = self.diagram.current(doc)
        {
            self.diagram_find.forget(&drawing);
            self.diagram.clear(doc);
        }
        if let Some(table) = self.tables.get(path) {
            self.table_find.forget(table);
        }
        if let Some(page) = self.html.get(path) {
            self.html_find.forget(page);
        }
        self.md.remove(path);
        if let Some(doc) = self.docs.get_mut(path) {
            doc.set_find_open(false);
        }
    }

    /// Releases all buffers (code, decoded images, video players, tables, HTML) when a file side
    /// tab is closed.
    pub fn close_file(&mut self, path: &str) {
        self.close_preview(path);
        self.tables.remove(path);
        self.html.remove(path);
        self.images.remove(path);
        self.videos.remove(path);
        self.docs.remove(path);
    }

    pub(crate) fn shown_diagram(&self, path: &str) -> Option<std::rc::Rc<mermaid::Diagram>> {
        if self.md.get(path).is_some_and(|v| v.mode.is_forced_source()) {
            return None;
        }
        self.diagram.current(self.docs.get(path)?)
    }

    pub(crate) fn shown_html(&self, path: &str) -> Option<&std::sync::Arc<html::Page>> {
        if !markdown::shows_rendered(self, path) {
            return None;
        }
        self.html.get(path)
    }

    pub(crate) fn previewable(&self, path: &str) -> bool {
        self.docs.get(path).is_some_and(|d| {
            d.content().is_text()
                && (markdown::is_markdown(path)
                    || image::is_svg(path)
                    || self.html.contains_key(path)
                    || self.tables.contains_key(path)
                    || self.diagram.current(d).is_some())
        })
    }

    pub fn local_files(&self, q: &str, limit: usize) -> Vec<String> {
        self.paths.query(q, limit)
    }

    /// The known files changed: their index builds again, off the UI thread.
    pub fn reindex(&mut self, waker: Option<&Waker>) {
        if let Some(w) = waker {
            self.paths.set_waker(w.clone());
        }
        self.paths.changed(self.known_files.iter().cloned().collect());
    }

    pub fn rebuild_tree(&mut self) {
        self.tree = build_workspace_tree(&self.known_files, &self.known_dirs, &self.expanded_dirs);
    }

    pub fn push_recent(&mut self, path: &str) {
        self.recent.retain(|p| p != path);
        self.recent.push(path.to_owned());
        if self.recent.len() > 30 {
            self.recent.remove(0);
        }
    }
}

pub(crate) type DirListing = (String, String, Vec<(String, bool)>);

pub struct FilesPane {
    pub(crate) replies: code_paths::Replies,
    /// Last count read for the active find bar; don't repeat it for every typed character.
    pub(crate) spoken_match: Option<(String, String, usize, usize)>,
    pub sessions: HashMap<String, SessionFiles>,
    pub empty_doc: CodeDocState,
    pub filter: TextEdit,
    pub wrap: markdown::WrapPrefs,
    pub(crate) dir_fetch: Latest<DirListing>,
    /// Each file's reads (`content`).
    pub reads: content::Reads,
    /// Authenticated remote video downloads (`Authorization` header -> `TempMediaFile`).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) auth_videos: video::AuthVideoDownloads,
    /// The server watches the open files hold (`watch`).
    pub watches: watch::Watches,
    /// The file tab shown last, so a tab that comes to show arms its watch again.
    pub(crate) shown: Option<(String, String)>,
    /// The session on screen last, so one that comes to show reads what changed meanwhile.
    pub(crate) shown_session: Option<String>,
    /// Folders listed again after a file in them went, each on its own worker.
    pub(crate) relists: (
        std::sync::mpsc::Sender<DirListing>,
        std::sync::mpsc::Receiver<DirListing>,
    ),
    pub(crate) find_fetch: Latest<(String, String, Vec<String>)>,
    /// Decodes picture files, once one opens.
    images: Option<rune::ImageCache>,
    pub(crate) waker: Option<Waker>,
    /// The zoom a picture's stage acts on while no picture is open.
    pub spare_zoom: rune_kit::StageZoom,
    /// The open picture's stage, as last laid out.
    pub stage_size: std::rc::Rc<std::cell::Cell<[f32; 2]>>,
}

impl Default for FilesPane {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            replies: Default::default(),
            spoken_match: None,
            empty_doc: CodeDocState::default(),
            filter: TextEdit::new(""),
            wrap: markdown::WrapPrefs::default(),
            dir_fetch: Latest::default(),
            reads: Default::default(),
            #[cfg(not(target_arch = "wasm32"))]
            auth_videos: Default::default(),
            watches: Default::default(),
            shown: None,
            shown_session: None,
            relists: std::sync::mpsc::channel(),
            find_fetch: Latest::default().debounce(Duration::from_millis(100)),
            images: None,
            waker: None,
            spare_zoom: rune_kit::StageZoom::default(),
            stage_size: Default::default(),
        }
    }
}

impl FilesPane {
    pub fn reply_version(&self, sid: &str) -> u64 {
        self.replies.version(sid)
    }

    pub fn retain_reply_links(&self, keep: impl Fn(&str) -> bool) {
        self.replies.retain(keep);
    }

    pub fn set_waker(&mut self, waker: Waker) {
        self.dir_fetch.set_waker(waker.clone());
        self.reads.set_waker(waker.clone());
        self.find_fetch.set_waker(waker.clone());
        self.waker = Some(waker);
    }

    /// `bytes` decoding on the image cache's workers.
    pub(crate) fn picture(&mut self, bytes: Vec<u8>, svg: bool) -> image::FileImage {
        let waker = self.waker.clone().unwrap_or_else(|| std::sync::Arc::new(|| {}));
        image::FileImage::new(self.images.get_or_insert_with(|| rune::ImageCache::new(waker)), bytes, svg)
    }
}
