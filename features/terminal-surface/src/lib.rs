//! Native Ghostty-VT terminal surface (`⌃\``): grid rendering, workspace tabs, persistence,
//! shell integration prompt marks, inline tab rename, find bar, hyperlinks, paste guard,
//! context menus, narrow mobile view, and theme/knob sync.

#![forbid(unsafe_code)]

pub mod calls;
pub mod draw;
pub mod find;
pub mod grid;
pub mod input;
pub mod integration;
pub mod links;
pub mod menu;
pub mod mount;
pub mod narrow;
pub mod ops;
pub mod osc8;
pub mod paste;
pub mod persist;
pub mod poll;
pub mod rename;
pub mod side_ops;
pub mod streams;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use opencode_kit::client::Server;
use opencode_kit::persistent_pty::{PersistentPtyInfo, TerminalCreate};
use rune::machine::{Attempt, Effects};
use rune::{Drag, KeyEvent, PointerPress, TextEdit, TextStyle, Tx};
use rune_knobs::{Group, Knob, Knobs};
use serde_json::Value;
use opencode_ui::palette::Pal;
use terminal::{StreamHandle, TerminalSession, TerminalTheme};

pub use grid::set_custom_font;
pub use ops::{
    active_terminal_workspace_key, adopt_terminal, apply_reply, close_terminal_tab,
    ensure_workspace_ready, execute_job, focus_terminal_surface, insert_stream_terminal,
    live_terminals, new_terminal_in, open_terminal_for_session, select_adjacent_terminal_tab,
    select_session_terminal, select_terminal_tab, server_id_str, start_terminal, terminal_dark,
    terminal_tab_mut, terminal_waker, terminal_workspace_key_for_session,
};
pub use paste::{PASTE_OVERLAY_ID, answer_paste, paste_into_terminal};
pub use persist::{ensure_terminal_workspace_hydrated, load_persisted_terminals, queue_terminal_save};
pub use poll::poll_terminal;
pub use rename::stamp as rename_stamp;
pub use side_ops::{
    PLUGIN, apply_pty_event, create_terminal_tab, ime_terminal, key_terminal, on_focus, poll_panel,
    surface, tab_label, terminal_focus, toggle_terminal,
};

use persist::StoredWorkspace;

pub const MAX_TERMINAL_WORKSPACES: usize = 20;

/// Default workspace key used by Storybook terminal stages.
pub const STORY_WORKSPACE_KEY: &str = "story\0/workspace/lumen";

/// Tab title style matching Desktop's tabular panel strip (`tabs.css:313`).
pub const TAB_UI: TextStyle = opencode_ui::style::UI.features("\"tnum\"");

pub const TERM_FONT: Knob = Knob::choice(
    "Terminal font",
    &["Commit Mono", "IBM Plex Mono", "SF Mono", "OpenMono"],
    3,
)
.about("Monospace font family in the terminal")
.hints(&[
    "Kit's Ghostty Commit Mono face",
    "System IBM Plex Mono face (falls back to OpenMono)",
    "macOS SF Mono system face",
    "Desktop's bundled OpenMono with Nerd Fonts icons",
]);

pub const TERM_SIZE: Knob = Knob::new("Font size", 11.0, 18.0, 14.0)
    .step(0.5)
    .about("Terminal font size in points");

pub const TERMINAL_KNOBS: &[Knob] = &[TERM_FONT, TERM_SIZE];

/// Knob group registered by the host app in its `⌘⇧K` tuning panel.
pub const TERMINAL_GROUP: Group = Group::new("Terminal", "terminal", TERMINAL_KNOBS);

/// Target pane slot inside a session's side/split/bottom regions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaneTarget {
    Side,
    Split,
    Bottom,
}

/// One entry in a terminal right-click context menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextMenuEntry {
    Item { id: &'static str, label: &'static str, enabled: bool },
    Separator,
}

/// Context menu specification passed to [`TerminalSurfaceHost::open_context_menu`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextMenuSpec {
    pub label: &'static str,
    pub entries: Vec<ContextMenuEntry>,
}

/// Callback invoked when a context menu closes with the picked item ID (or `None` when dismissed).
pub type ContextMenuPick = Box<dyn FnOnce(&mut dyn TerminalSurfaceHost, Option<&str>, &mut Tx)>;

/// Off-thread PTY REST API work scheduled through [`TerminalSurfaceHost::spawn_job`].
#[derive(Clone, Debug)]
pub enum TerminalJob {
    /// Renames persistent PTY `pty_id` (`PUT /api/experimental/persistent-pty/:id`).
    Rename {
        ws: String,
        pty_id: String,
        title: Option<String>,
        attempt: Attempt,
    },
    /// Terminates and removes persistent PTY `pty_id` (`DELETE /api/experimental/persistent-pty/:id`).
    Remove {
        pty_id: String,
    },
    /// Lists `session`'s terminals (`GET /api/experimental/session/:id/terminal`) for workspace `ws`.
    List { ws: String, session: String, order: Vec<String>, active: Option<String> },
    /// Starts a terminal for `session` (`POST /api/experimental/session/:id/terminal`).
    Create { ws: String, session: String, create: TerminalCreate, launch: calls::Launch, after: calls::After },
}

/// Result of a [`TerminalJob`], delivered back on the UI thread via [`apply_reply`].
#[derive(Clone, Debug)]
pub enum TerminalReply {
    Renamed {
        ws: String,
        pty_id: String,
        attempt: Attempt,
        result: Result<PersistentPtyInfo, String>,
    },
    Removed {
        pty_id: String,
    },
    Listed { ws: String, order: Vec<String>, active: Option<String>, result: Result<Vec<PersistentPtyInfo>, String> },
    Created { ws: String, plain: bool, shell: Option<Option<String>>, after: calls::After, result: Result<PersistentPtyInfo, String> },
}

/// Link hover state on a terminal grid cell, replacing `Option<(u16, u16, bool)>`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum HoverLink {
    #[default]
    None,
    /// Cell under the pointer without follow modifier.
    Plain { col: u16, row: u16 },
    /// Cell under the pointer with follow modifier (Ctrl / Cmd) held.
    Followable { col: u16, row: u16 },
}

impl HoverLink {
    pub const fn at(col: u16, row: u16, follow: bool) -> Self {
        if follow {
            Self::Followable { col, row }
        } else {
            Self::Plain { col, row }
        }
    }
}

/// The appearance and theme (`Pal::theme`, `themes::generation`) the open terminals were last painted in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaintedTheme {
    pub dark: bool,
    pub theme: usize,
    pub generation: u64,
}

/// What feeds a terminal tab's VT emulator: a live server persistent PTY, a recorded/story byte
/// stream, or a local fixture shell that echoes input back into its stream.
pub enum TabBackend {
    /// Connected to a persistent PTY on the server (`TerminalSession::spawn_persistent_pty`).
    PersistentPty,
    /// Fed by an in-process byte stream (Storybook stages, offline hydrated recordings).
    Stream(StreamHandle),
    /// Fixture-mode echo shell backed by an in-process byte stream.
    FixtureShell(StreamHandle),
}

impl TabBackend {
    pub fn stream(&self) -> Option<&StreamHandle> {
        match self {
            Self::PersistentPty => None,
            Self::Stream(handle) | Self::FixtureShell(handle) => Some(handle),
        }
    }

    pub fn fixture_stream(&self) -> Option<&StreamHandle> {
        match self {
            Self::FixtureShell(handle) => Some(handle),
            Self::PersistentPty | Self::Stream(_) => None,
        }
    }
}

/// Visual bell flash along the top edge of the terminal pane.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Bell {
    #[default]
    Quiet,
    Ringing { until: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BellEvent {
    Rang { now: f64 },
    Tick { now: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BellEffect {
    Wake(Duration),
    Expired,
}

impl Bell {
    pub fn is_ringing(self, now: f64) -> bool { matches!(self, Self::Ringing { until } if now < until) }
    pub fn expired(self, now: f64) -> bool { matches!(self, Self::Ringing { until } if now >= until) }

    pub fn step(&mut self, event: BellEvent) -> Effects<BellEffect> {
        match (*self, event) {
            (_, BellEvent::Rang { now }) => {
                *self = Self::Ringing { until: now + 0.14 };
                Effects::one(BellEffect::Wake(Duration::from_millis(145)))
            }
            (Self::Ringing { until }, BellEvent::Tick { now }) if now >= until => {
                *self = Self::Quiet;
                Effects::one(BellEffect::Expired)
            }
            _ => Effects::none(),
        }
    }
}

/// Phase-locked cursor blink: `epoch` anchors the 0.5 s cadence; the variant records the last
/// phase observed by polling so each half-period edge triggers one redraw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CursorBlink {
    Lit { epoch: f64 },
    Dim { epoch: f64 },
}

impl Default for CursorBlink {
    fn default() -> Self { Self::Lit { epoch: 0.0 } }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BlinkEvent {
    Reset { now: f64 },
    Tick { now: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlinkEffect {
    Redraw,
    Wake(Duration),
}

impl CursorBlink {
    pub fn epoch(self) -> f64 {
        match self { Self::Lit { epoch } | Self::Dim { epoch } => epoch }
    }

    pub fn is_lit(self, clock: f64) -> bool { ((clock - self.epoch()).max(0.0) % 1.0) < 0.5 }

    pub fn next_edge(self, clock: f64) -> Duration {
        let dt = (clock - self.epoch()).max(0.0);
        Duration::from_secs_f64((0.5 - (dt % 0.5)).clamp(0.01, 0.5))
    }

    pub fn flips_at(self, now: f64) -> bool { self.is_lit(now) != matches!(self, Self::Lit { .. }) }

    pub fn step(&mut self, event: BlinkEvent) -> Effects<BlinkEffect> {
        match event {
            BlinkEvent::Reset { now } => {
                *self = Self::Lit { epoch: now };
                Effects::none()
            }
            BlinkEvent::Tick { now } => {
                let epoch = self.epoch();
                let next = if self.is_lit(now) { Self::Lit { epoch } } else { Self::Dim { epoch } };
                let flipped = next != *self;
                *self = next;
                let wake = self.next_edge(now);
                if flipped { Effects::two(BlinkEffect::Redraw, BlinkEffect::Wake(wake)) } else { Effects::one(BlinkEffect::Wake(wake)) }
            }
        }
    }
}

/// Storybook stage state for `"Terminal/Pane"` (`Inactive` still treats focus as `true` for other stories).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StoryStage {
    #[default]
    Inactive,
    Focused { variant: usize },
    Blurred { variant: usize },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoryStageEvent {
    Build { variant: usize },
    Focus,
    Blur,
}

impl StoryStage {
    pub fn is_focused(self) -> bool { !matches!(self, Self::Blurred { .. }) }

    pub fn needs_rebuild(self, variant: usize) -> bool {
        matches!(self, Self::Focused { variant: built } | Self::Blurred { variant: built } if built != variant)
    }

    pub fn step(&mut self, event: StoryStageEvent) {
        *self = match (*self, event) {
            (_, StoryStageEvent::Build { variant }) => Self::Focused { variant },
            (Self::Blurred { variant }, StoryStageEvent::Focus) => Self::Focused { variant },
            (Self::Focused { variant }, StoryStageEvent::Blur) => Self::Blurred { variant },
            (same, _) => same,
        };
    }
}

/// Pending keyboard-focus handoff to `grid::SCREEN_KEY` (`gui-plugins/src/terminal/focus-machine.ts`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FocusClaim {
    #[default]
    Idle,
    Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusClaimEvent {
    Request,
    Poll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusClaimEffect {
    FocusScreen,
}

impl FocusClaim {
    pub fn is_pending(self) -> bool { matches!(self, Self::Pending) }

    pub fn step(&mut self, event: FocusClaimEvent) -> Effects<FocusClaimEffect> {
        match (*self, event) {
            (_, FocusClaimEvent::Request) => {
                *self = Self::Pending;
                Effects::none()
            }
            (Self::Pending, FocusClaimEvent::Poll) => {
                *self = Self::Idle;
                Effects::one(FocusClaimEffect::FocusScreen)
            }
            (Self::Idle, FocusClaimEvent::Poll) => Effects::none(),
        }
    }
}

pub struct TerminalTab {
    pub id: String,
    pub title: String,
    pub title_number: usize,
    pub custom_title: bool,
    pub session: TerminalSession,
    pub backend: TabBackend,
    pub scroll_px_accum: f32,
    pub drag: mount::TextDrag,
    /// While a drag selecting text is held at the text's top or bottom edge.
    pub edge_scroll: Option<mount::EdgeScroll>,
    /// Whether it has an attachment (`surface-machine.ts`).
    pub mount: mount::Mount,
    pub(crate) fit: Cell<mount::Fit>,
    /// What the input method composes at the cursor, while it composes.
    pub preedit: Option<input::Preedit>,
    /// The folder the shell started in (the server's `cwd`), for links before it reports one.
    pub cwd: String,
    /// The find bar's field, while it's open.
    pub find: Option<TextEdit>,
    /// Whether it had the keyboard at the last poll, for focus reports (`?1004`).
    pub(crate) focused: Cell<bool>,
    /// The cell under the pointer and whether the follow modifier (Ctrl, ⌘ on macOS) was held:
    /// the link there (`links::span`) is underlined, and followable with the modifier.
    pub hover: HoverLink,
    /// Where the quote last taken with its comment editor came from (`grid::quote`).
    pub(crate) quote_source: Cell<Option<grid::QuoteSource>>,
}

impl TerminalTab {
    pub fn display_title(&self) -> String {
        if !self.title.trim().is_empty() {
            self.title.clone()
        } else {
            opencode_i18n::t!("plugin.terminal.title.numbered", number = self.title_number.max(1))
        }
    }

    /// Asks the session for the pane's grid when it changed (the worker debounces it).
    pub fn ensure_fit(&self, cols: u16, rows: u16, cw_px: u32, ch_px: u32) {
        let next = mount::Fit::Fitted { cols, rows, cell_px: (cw_px, ch_px) };
        if self.fit.get() != next {
            self.fit.set(next);
            self.session.resize(cols, rows, cw_px, ch_px);
        }
    }
}

/// One session's terminals (Desktop keeps them per session, on the server).
#[derive(Default)]
pub struct TerminalWorkspace {
    /// Where the session's shells start.
    pub directory: String,
    pub active: Option<String>,
    pub tabs: Vec<TerminalTab>,
    /// Why the server's terminals couldn't be listed or started; shown instead of a shell.
    pub failure: Option<String>,
    /// The server call these terminals wait for (`calls`).
    pub call: calls::Call,
}

impl TerminalWorkspace {
    pub fn next_title_number(&self) -> usize {
        let used: BTreeSet<usize> = self
            .tabs
            .iter()
            .filter_map(|t| {
                if t.title_number > 0 {
                    Some(t.title_number)
                } else {
                    persist::number_from_title(&t.title)
                }
            })
            .collect();
        (1..=used.len() + 1).find(|n| !used.contains(n)).unwrap_or(1)
    }
}

pub struct TerminalPane {
    pub workspaces: HashMap<String, TerminalWorkspace>,
    pub stored: HashMap<String, StoredWorkspace>,
    /// Whether the saved workspaces were taken in.
    pub saved: persist::Saved,
    pub lru_order: Vec<String>,
    pub blink: CursorBlink,
    pub bell: Bell,
    /// The appearance and theme (`Pal::theme`, `themes::generation`) the open terminals were last painted in.
    pub last_theme: Option<PaintedTheme>,
    /// The appearance the window last drew in, resolved from the System setting; state
    /// updates have no `Cx` to ask.
    pub painted_dark: Cell<Option<bool>>,
    pub dirty_flag: Arc<AtomicBool>,
    pub story: StoryStage,
    /// The tab whose title is a field (`TerminalTabLabel`'s inline rename).
    pub renaming: rename::Renames,
    /// The server's connections so far: each reconnect starts the next (`machine.ts` `connections`).
    pub connections: u32,
    /// Takes an edit that lands after its find/rename bar closed: a field's `on_edit` needs somewhere to write.
    pub spare_field: TextEdit,
    /// The terminal took the keyboard (opened, toggled, clicked): keyboard focus goes to its
    /// viewport on the next poll, so Tab and F6 there are the viewport's and never trapped.
    pub focus_claim: FocusClaim,
    /// A terminal asked for before the server listed its session (`side_ops::Asked`).
    pub asked: Option<side_ops::Asked>,
    /// A paste waiting for the user to say it may run (`paste`).
    pub pending_paste: Option<paste::Pending>,
    /// An OSC 8 hyperlink click waiting for target confirmation (`osc8`).
    pub pending_osc8: Option<osc8::Pending>,
    /// Each server's own shell (by its address), once a terminal with marks needed it;
    /// `None` when it couldn't be told.
    pub server_shells: RefCell<HashMap<String, Option<String>>>,
}

impl Default for TerminalPane {
    fn default() -> Self {
        Self {
            workspaces: HashMap::new(),
            stored: HashMap::new(),
            saved: persist::Saved::default(),
            lru_order: Vec::new(),
            blink: CursorBlink::default(),
            bell: Bell::default(),
            last_theme: None,
            painted_dark: Cell::new(None),
            dirty_flag: Arc::new(AtomicBool::new(false)),
            story: StoryStage::default(),
            renaming: rename::Renames::default(),
            connections: 0,
            spare_field: TextEdit::default(),
            focus_claim: FocusClaim::default(),
            asked: None,
            pending_paste: None,
            pending_osc8: None,
            server_shells: RefCell::new(HashMap::new()),
        }
    }
}

impl TerminalPane {
    pub fn cursor_lit(&self, clock: f64) -> bool {
        self.blink.is_lit(clock)
    }

    pub fn reset_blink(&mut self, clock: f64) {
        drop(self.blink.step(BlinkEvent::Reset { now: clock }));
    }

    pub(crate) fn touch_lru(&mut self, key: &str) {
        self.lru_order.retain(|k| k != key);
        self.lru_order.push(key.to_owned());
        while self.lru_order.len() > MAX_TERMINAL_WORKSPACES {
            let oldest = self.lru_order.remove(0);
            self.workspaces.remove(&oldest);
            self.stored.remove(&oldest);
        }
    }

    pub(crate) fn find_field_mut(&mut self, ws_key: &str, tab_id: &str) -> &mut TextEdit {
        let Self { workspaces, spare_field, .. } = self;
        workspaces
            .get_mut(ws_key)
            .and_then(|ws| ws.tabs.iter_mut().find(|t| t.id == tab_id))
            .and_then(|t| t.find.as_mut())
            .unwrap_or(spare_field)
    }

    pub(crate) fn rename_field_on_edit(&mut self) -> &mut TextEdit {
        let Self { renaming, spare_field, .. } = self;
        renaming.on_typed_field_mut().unwrap_or(spare_field)
    }
}

/// Object-safe host boundary for the terminal surface, workspace tabs, persistence,
/// inline tab rename, hyperlinks, context menus, paste guard, and narrow mobile view.
pub trait TerminalSurfaceHost: opencode_kernel::AppEnv + opencode_kernel::AppFeedback + opencode_kernel::AppOverlays {
    /// Terminal pane state (tracked read).
    fn terminal(&self) -> &TerminalPane;
    /// Terminal pane state (tracked write — callers only invoke when mutating).
    fn terminal_mut(&mut self) -> &mut TerminalPane;

    /// Working directory of session `sid`, if loaded.
    fn session_directory(&self, sid: &str) -> Option<&str>;
    /// Working directory of the active draft when no session directory is available.
    fn draft_directory(&self) -> &str {
        ""
    }
    /// Whether the window is currently rendering a Storybook stage rather than a live session.
    fn is_story_stage(&self) -> bool {
        false
    }
    /// Workspace key used while [`Self::is_story_stage`] is `true`.
    fn story_workspace_key(&self) -> &str {
        STORY_WORKSPACE_KEY
    }
    /// Rebuilds the story stage if its variant changed; returns `true` when rebuilt.
    fn poll_story_variant(&mut self) -> bool {
        false
    }

    /// Live OpenCode server handle when connected, or `None` in fixture mode.
    fn live_server(&self) -> Option<&Server>;
    /// Whether this window may create, control, or close PTYs (`false` in `--read-only` / observer mode).
    fn allows_terminal_input(&self) -> bool {
        true
    }

    /// Whether the host window has lost OS focus (for `?1004` focus-out reports).
    fn is_window_blurred(&self) -> bool {
        false
    }
    /// Whether the session composer currently owns keyboard focus.
    fn is_composer_focused(&self) -> bool {
        false
    }
    /// Current narrow-window width fallback when the narrow body has not laid out yet.
    fn narrow_width(&self) -> f32 {
        390.0
    }

    /// Live look-and-feel knobs (`TERM_FONT`, `TERM_SIZE`).
    fn knobs(&self) -> &Knobs;
    /// Configured `(font_choice_index, custom_font_name)` from appearance settings.
    fn terminal_font(&self) -> (usize, &str) {
        (3, "")
    }
    /// Default dark/light appearance before the first frame paints.
    fn terminal_dark_default(&self) -> bool {
        false
    }
    /// Resolves the [`TerminalTheme`] for `dark` appearance.
    fn terminal_theme(&self, dark: bool) -> TerminalTheme {
        theme_from_pal(opencode_ui::palette::pal(dark))
    }
    /// macOS: treat Option as Meta (`Esc` + key) inside the terminal.
    fn terminal_option_meta(&self) -> bool {
        false
    }
    /// Shell IDs (`integration::Shell::id`) enabled for OSC 133 prompt marks.
    fn terminal_marks(&self) -> &[String] {
        &[]
    }
    /// Directory where shell-integration helper scripts (such as `bash.sh`) may be written on local hosts.
    fn shell_integration_dir(&self) -> Option<PathBuf> {
        None
    }
    /// Reads an environment variable for local shell integration (`WSLENV`, `SHELL`).
    fn env_var(&self, name: &str) -> Option<String> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            #[allow(clippy::disallowed_methods)]
            std::env::var(name).ok()
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = name;
            None
        }
    }
    /// On `wasm32`, loads bundled terminal font faces on demand when a terminal first attaches.
    fn fetch_terminal_fonts_if_wasm(&self) {}

    /// Reads a JSON value from host persistence.
    fn read_json(&self, ns: &str, key: &str) -> Option<Value> {
        let _ = (ns, key);
        None
    }
    /// Writes a JSON value to host persistence.
    fn write_json(&self, ns: &str, key: &str, value: &Value) {
        let _ = (ns, key, value);
    }
    /// Triggers async loading of storage namespace `ns` (`plugin.terminal`).
    fn load_storage_namespace(&self, ns: &str) {
        let _ = ns;
    }
    /// Returns every `(key, json_text)` row of namespace `ns` once loaded; `None` while pending.
    fn storage_items(&self, ns: &str) -> Option<Vec<(String, String)>> {
        let _ = ns;
        None
    }
    /// Background-thread batch writer used by debounced terminal persistence (`persist::flush`).
    fn terminal_storage_writer(&self) -> Option<persist::StorageBatchWriter> {
        None
    }

    /// Opens a file or `file://` link clicked in terminal output for session `sid`.
    fn open_link(&mut self, sid: &str, href: &str, tx: &mut Tx) {
        let _ = (sid, href, tx);
    }
    /// Opens a web or app-scheme URL clicked in terminal output for session `sid` (routing
    /// through browser URL policy & Settings > Browser `links` preference; `shift` forces
    /// the OS default browser for valid `http(s)` URLs).
    fn open_web_link(&mut self, sid: &str, href: &str, shift: bool, tx: &mut Tx) {
        let _ = (sid, href, shift, tx);
    }
    /// Opens a right-click context menu at `press` and invokes `on_pick` when it closes.
    fn open_context_menu(
        &mut self,
        _spec: ContextMenuSpec,
        _press: PointerPress,
        _tx: &mut Tx,
        _on_pick: ContextMenuPick,
    ) {}
    /// Reads the system clipboard and pastes into terminal `(ws_key, tab_id)`.
    /// Typically delegates to [`input::read_clipboard_into_terminal`].
    fn read_clipboard_for_paste(&mut self, ws_key: String, tab_id: String, tx: &mut Tx) {
        let _ = (ws_key, tab_id, tx);
    }
    /// Whether `selected` text may currently be quoted into the composer.
    fn can_quote_terminal_selection(&self, selected: &str) -> bool {
        let _ = selected;
        false
    }
    /// Adds `selected` terminal text to the composer as a quote; with `comment`, its comment
    /// editor opens, beside the rows the quote came from.
    fn quote_terminal_selection(&mut self, selected: &str, comment: Option<grid::QuotePlace>, tx: &mut Tx) {
        let _ = (selected, comment, tx);
    }
    /// Notifies assistive speech tracking that a key/IME went to terminal `(ws_key, tab_id)`.
    fn speech_terminal_key(&mut self, ws_key: &str, tab_id: &str) {
        let _ = (ws_key, tab_id);
    }
    /// Optional direct assistive speech announcement hook.
    fn speech_announce(&mut self, text: String) {
        let _ = text;
    }

    /// Whether a registered plugin command claims `ev` before the terminal processes it.
    fn plugin_takes_chord(&self, ev: &KeyEvent, tx: &Tx) -> bool {
        let _ = (ev, tx);
        false
    }
    /// Whether the side/bottom panel region currently holds focus.
    fn is_side_focused(&self) -> bool {
        false
    }
    /// The panel tab key currently shown in the focused side/bottom pane while that pane is open.
    fn focused_pane_shown_tab(&self) -> Option<String> {
        None
    }
    /// Whether side panel tab `key` (`"terminal:{id}"`) is currently visible on screen.
    fn side_tab_visible(&self, key: &str) -> bool {
        let _ = key;
        false
    }
    /// Whether session `sid`'s side tab strip holds any persisted `"terminal:*"` tab keys.
    fn session_has_terminal_tabs(&self, sid: &str) -> bool {
        let _ = sid;
        false
    }
    /// Estimated `(pane_width, pane_height)` in logical points before first layout.
    fn terminal_pane_estimate(&self) -> (f32, f32) {
        (480.0, 744.0 / self.zoom().max(0.5))
    }
    /// Moves shell focus into the side pane (blurring composer) and updates active pane focus
    /// for `(sid, panel_key)` when provided.
    fn focus_side_pane(&mut self, tab: Option<(&str, &str)>) {
        let _ = tab;
    }
    /// Runs `f` while preserving the host's current keyboard focus (background tab opens).
    /// Hosts without separate focus isolation can implement this as `f(self)`.
    fn keeping_keyboard(&mut self, f: &mut dyn FnMut(&mut dyn TerminalSurfaceHost));
    /// Adds/selects side tab `key` in session `sid` (in `target` pane when specified).
    fn show_side_tab(&mut self, sid: &str, key: &str, target: Option<PaneTarget>) {
        let _ = (sid, key, target);
    }
    /// Opens side tab `key` in session `sid`, landing instantly when its pane is already visible.
    fn show_pane_tab(&mut self, sid: &str, key: &str, target: Option<PaneTarget>, tx: &mut Tx) {
        let _ = tx;
        self.show_side_tab(sid, key, target);
    }
    /// Hides the pane currently showing side tab `key`.
    fn hide_side_tab(&mut self, key: &str, tx: &mut Tx) {
        let _ = (key, tx);
    }
    /// Closes side tab `key` in the active session's panel strip.
    fn close_side_tab(&mut self, key: &str, tx: &mut Tx) {
        let _ = (key, tx);
    }
    /// Forwards a tab-title drag gesture to the host's side-panel tab strip.
    fn side_tab_gesture(&mut self, key: &str, title: &str, draggable: bool, shown: bool, drag: &Drag, tx: &mut Tx) {
        let _ = (key, title, draggable, shown, drag, tx);
    }

    /// Schedules an off-thread [`TerminalJob`] against the live server; execute with
    /// [`execute_job`] and deliver the result on the UI thread with [`apply_reply`].
    fn spawn_job(&mut self, job: TerminalJob) {
        let _ = job;
    }
}

pub fn theme_from_pal(p: &Pal) -> TerminalTheme {
    let base = if p.dark {
        TerminalTheme::opencode_dark()
    } else {
        TerminalTheme::opencode_light()
    };
    let fg = terminal::Rgb::from_rune(p.term_fg);
    let bg = terminal::Rgb::from_rune(p.bg_base);
    TerminalTheme {
        dark: p.dark,
        fg,
        bg,
        cursor: fg,
        selection_alpha: if p.dark { 0.25 } else { 0.20 },
        ansi_base: base.ansi_base,
    }
}

/// A session's terminals, keyed by server and session (`TerminalModel.load(session)`).
pub fn workspace_key(server_base: &str, session_id: &str) -> String {
    format!("{server_base}\0{session_id}")
}

/// Canonical panel tab key for terminal `id` (`"terminal:{id}"`).
pub fn panel_key(id: &str) -> String {
    format!("{PLUGIN}:{id}")
}

/// The last path segment of a Windows or POSIX directory.
pub fn dir_name(directory: &str) -> &str {
    Some(opencode_kit::path::basename(directory)).filter(|d| !d.is_empty()).unwrap_or("workspace")
}
