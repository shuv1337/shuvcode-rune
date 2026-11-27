//! `ui.panelSidebar`: the frame Review and Files share (`SessionReviewV2`, `SessionFilePanelV2`):
//! the file sidebar at the panel's start or end, resized by a handle on its inner edge (a double
//! click resets it), the panel's content beside it, and the tree toggle pinned at the panel's
//! top corner on the sidebar's side. Whether the sidebar shows, how wide it is and which edge it
//! sits on are one preference every panel shares, as Desktop's `PanelSidebar`
//! (`createPanelSidebar`, `review-panel-v2`), which ⌘\ toggles.

use opencode_i18n::t;
use rune::{Align, Cursor, Drag, El, KeyEvent, Phase, PointerPress, PopupAnchor, Tx, col, el, row};
use rune_luau::NodeArgs;

use super::p;
use crate::App;
use crate::keymap::Bind;
use crate::popup::{self, Item, Menu};
use crate::review::chrome;

/// `SESSION_REVIEW_V2_SIDEBAR_WIDTH_*`.
pub const DEFAULT_W: f32 = 240.0;
pub const MIN_W: f32 = 200.0;
pub const MAX_W: f32 = 480.0;

/// The resize handle's gesture.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Resize {
    #[default]
    Idle,
    /// Dragging, from the width the sidebar had and what the user had set then.
    Dragging { from: f32, was: Option<f32> },
}

/// What moves the handle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ResizeEvent {
    Down { width: f32 },
    Move { dx: f32, min: f32, max: f32 },
    Up { cancelled: bool },
    Reset,
}

/// The edge of the panel the sidebar sits on (`sidebarSide`): the inline start (left in a
/// left-to-right layout) or the inline end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Side {
    #[default]
    Start,
    End,
}

impl Side {
    pub fn name(self) -> &'static str {
        match self {
            Side::Start => "start",
            Side::End => "end",
        }
    }

    pub fn parse(name: &str) -> Side {
        if name == "end" { Side::End } else { Side::Start }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PanelSidebar {
    pub opened: bool,
    /// The width the user dragged it to; the default until then.
    pub width: Option<f32>,
    pub side: Side,
    pub resize: Resize,
}

impl Default for PanelSidebar {
    fn default() -> Self {
        Self { opened: true, width: None, side: Side::Start, resize: Resize::Idle }
    }
}

impl PanelSidebar {
    /// The preference kept across launches (Desktop's `Persist.global("review-panel-v2")`:
    /// `sidebarOpened`, `sidebarWidth` within its bounds, and `sidebarSide`).
    pub fn load() -> Self {
        let v = crate::storage::json("opencode.global.dat", "review-panel-v2").unwrap_or_default();
        Self {
            opened: v["sidebarOpened"].as_bool().unwrap_or(true),
            width: v["sidebarWidth"].as_f64().map(|w| (w as f32).clamp(MIN_W, MAX_W)),
            side: Side::parse(v["sidebarSide"].as_str().unwrap_or_default()),
            resize: Resize::Idle,
        }
    }

    fn save(&self) {
        // As the app's other preferences: a person's run, or a harness run that may write
        // (`--harness-writes` in a root of its own).
        if crate::attended() {
            crate::storage::set_json("opencode.global.dat", "review-panel-v2", &serde_json::json!({ "sidebarOpened": self.opened, "sidebarWidth": self.width.unwrap_or(DEFAULT_W), "sidebarSide": self.side.name() }));
        }
    }

    pub fn toggle(&mut self) {
        self.opened = !self.opened;
        self.save();
    }

    /// Puts the sidebar on `side`, for every side panel (`PanelSidebar.move`).
    pub fn move_to(&mut self, side: Side) {
        if self.side != side {
            self.side = side;
            self.save();
        }
    }

    /// Steps the resize gesture; a width it settles on is kept.
    pub fn step(&mut self, event: ResizeEvent) {
        let before = self.width;
        self.step_resize(event);
        if self.resize == Resize::Idle && self.width != before || matches!(event, ResizeEvent::Up { cancelled: false }) {
            self.save();
        }
    }

    fn step_resize(&mut self, event: ResizeEvent) {
        self.resize = match (self.resize, event) {
            (_, ResizeEvent::Down { width }) => Resize::Dragging { from: width, was: self.width },
            (Resize::Dragging { from, was }, ResizeEvent::Move { dx, min, max }) => {
                self.width = Some((from + dx).clamp(min, max));
                Resize::Dragging { from, was }
            }
            (Resize::Dragging { was, .. }, ResizeEvent::Up { cancelled: true }) => {
                self.width = was;
                Resize::Idle
            }
            (_, ResizeEvent::Reset) => {
                self.width = None;
                Resize::Idle
            }
            (_, ResizeEvent::Up { .. }) | (Resize::Idle, ResizeEvent::Move { .. }) => Resize::Idle,
        };
    }
}

fn toggle(s: &mut App, tx: &mut Tx) {
    tx.instant();
    s.workspace_panes.panel_sidebar.toggle();
    crate::plugin_host::bump();
}

/// "Sidebar position" on a right-click of the sidebar or its toggle, or `Shift+F10` on the
/// toggle (`SessionFilePanelV2SideMenu`, `packages/session-ui/src/v2/components/session-file-panel-v2.tsx:96-117`,
/// `opencode#66372`): Left and Right, the physical edges, so right is the end edge left to right
/// and the start edge right to left. A text field in the sidebar keeps its own menu.
fn open_side_menu(s: &mut App, at: PointerPress, tx: &mut Tx) {
    open_side_menu_at(s, at.into(), tx);
}

fn open_side_menu_at(s: &mut App, anchor: PopupAnchor, tx: &mut Tx) {
    let rtl = opencode_i18n::direction() == opencode_i18n::Direction::Rtl;
    let right = (s.workspace_panes.panel_sidebar.side == Side::End) != rtl;
    let edges = Menu::new(t!("ui.sessionReviewV2.sidebarPosition"))
        .item(Item::new("left", t!("ui.sessionReviewV2.sidebarPosition.left")).radio(!right))
        .item(Item::new("right", t!("ui.sessionReviewV2.sidebarPosition.right")).radio(right));
    let menu = Menu::new(t!("ui.sessionReviewV2.sidebarPosition"))
        .align(Align::Start)
        .submenu("position", t!("ui.sessionReviewV2.sidebarPosition"), edges);
    tx.instant();
    popup::open(s, menu, anchor, tx, move |s: &mut App, picked, tx| {
        let right = match picked {
            Some("left") => false,
            Some("right") => true,
            _ => return,
        };
        tx.instant();
        s.workspace_panes.panel_sidebar.move_to(if right != rtl { Side::End } else { Side::Start });
        crate::plugin_host::bump();
    });
}

/// ⌘\ while a panel with the sidebar shows (Review, a file).
pub fn key(app: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    // Under a New tab page the tab is still the active one, but its tree isn't drawn.
    if !Bind::TreeToggle.matches_tx(ev, tx) || !(app.is_review_active() || app.active_file_tab().is_some()) || app.new_tab_covers_active() {
        return false;
    }
    toggle(app, tx);
    true
}

/// `ui.panelSidebar(props, { sidebar, main })`.
pub fn view(a: &mut NodeArgs) -> Result<El, String> {
    crate::plugin_host::cx::with_app(|app| {
        let pal = p(a);
        let id = a.string("id").unwrap_or("panel").to_owned();
        let min = a.number("minWidth").map_or(MIN_W, |n| n as f32);
        let max = a.number("maxWidth").map_or(MAX_W, |n| n as f32).max(min);
        let default = a.number("width").map_or(DEFAULT_W, |n| n as f32);
        let mut children = std::mem::take(&mut a.children).into_iter();
        let (side, main) = (children.next().unwrap_or_else(el), children.next().unwrap_or_else(el));
        let st = &app.workspace_panes.panel_sidebar;
        // Files' Open file launcher keeps the tree open, its toggle disabled.
        let locked = a.flag("locked");
        let opened = st.opened || locked;
        let w = st.width.unwrap_or(default).clamp(min, max);
        let at = st.side;
        let mut body = row().key(format!("{id}-body")).w_full().grow(1.0).min_h(0.0);
        let main = col().key(format!("{id}-main")).grow(1.0).min_w(0.0).h_full().child(main);
        // `session-review-v2-sidebar`: its width holds its 1 pt `border-base` inner edge, which takes
        // its room from the contents (`border-inline-end`, border-box; `border-inline-start` on the
        // end edge), so the tree ends inside it.
        let edge = match at {
            Side::Start => [0.0, 1.0, 0.0, 0.0],
            Side::End => [0.0, 0.0, 0.0, 1.0],
        };
        let sidebar = opened.then(|| {
            // Only pointer right-clicks open the sidebar-position menu on the sidebar body;
            // keyboard `Shift+F10` inside the sidebar (such as the filter field) keeps its own menu
            // (`packages/session-ui/src/v2/components/session-file-panel-v2.tsx:35,96`, `opencode#66372`).
            col()
                .key(format!("{id}-sidebar"))
                .w(w)
                .h_full()
                .fill(pal.bg_base)
                .border_sides(edge, pal.border)
                .border_layout()
                .on_secondary_press(|s: &mut App, at: PointerPress, tx: &mut Tx| {
                    if !tx.by_keys() {
                        open_side_menu(s, at, tx);
                    }
                })
                .child(side)
        });
        // On the end edge the body is `row-reverse`: tree, toggle, border and handle all swap sides.
        body = match at {
            Side::Start => body.children(sidebar).child(main),
            Side::End => body.child(main).children(sidebar),
        };
        // The resize handle lies over the edge, above the content beside it.
        if opened {
            let dragging = st.resize != Resize::Idle;
            let resize_inputs = ((pal.theme, pal.generation, pal.dark), w.to_bits(), min.to_bits(), max.to_bits(), dragging, at);
            body = body.child(rune::memo(format!("{id}-sidebar-resize"), resize_inputs, || handle(&pal, w, min, max, dragging, at)));
        }
        let toggle_key = crate::plugin_host::icons::intern(&format!("{id}-tree-toggle"));
        col()
            .key(format!("{id}-panel"))
            .w_full()
            .grow(1.0)
            .min_h(0.0)
            .fill(pal.bg_base)
            .child(body)
            .child(
                chrome::sidebar_toggle(&pal, toggle_key, opened, locked, at == Side::End, toggle)
                    // Keyboard `Shift+F10` / Menu key anchors directly to the toggle's start-bottom
                    // corner (`opencode#66372`, `session-file-panel-v2.tsx:96-117`), while pointer
                    // right-click opens at the pointer.
                    .on_secondary_press(move |s: &mut App, at: PointerPress, tx: &mut Tx| {
                        let anchor = if tx.by_keys() { PopupAnchor::Key(toggle_key.to_owned()) } else { at.into() };
                        open_side_menu_at(s, anchor, tx);
                    }),
            )
    })
    .ok_or_else(|| "ui.panelSidebar draws only in a render".into())
}

/// Desktop's `ResizeHandle` over the sidebar's inner edge (the one facing away from the panel's
/// edge it sits on): 8 pt wide, centred on the edge, its 4 × 40 pill (`border-muted`, as the
/// review sidebar colours it) showing under the pointer and while dragging.
fn handle(p: &crate::palette::Pal, width: f32, min: f32, max: f32, dragging: bool, at: Side) -> El {
    let pill = el().key("pill").size(4.0, 40.0).radius(2.0).fill(p.border_muted);
    let pill = if dragging { pill } else { pill.group_opacity(0.0, 1.0) };
    let pinned = match at {
        Side::Start => el().pin(Some(width - 4.0), Some(0.0), None, Some(0.0)),
        Side::End => el().pin(None, Some(0.0), Some(width - 4.0), Some(0.0)),
    };
    pinned
        .w(8.0)
        .center()
        .cursor(Cursor::ResizeColumn)
        .hover_group()
        .hover_motion(rune::Anim::INSTANT, rune::Anim::INSTANT)
        .on_double_press_tx(|s: &mut App, tx: &mut Tx| {
            tx.instant();
            s.workspace_panes.panel_sidebar.step(ResizeEvent::Reset);
        })
        .on_drag(move |s: &mut App, d: &mut Drag, tx: &mut Tx| {
            tx.instant();
            // The pointer's travel towards the panel's middle widens it: left to right from the
            // start edge, and the other way from the end edge or in a right-to-left layout.
            let rtl = opencode_i18n::direction() == opencode_i18n::Direction::Rtl;
            let dx = if (at == Side::End) != rtl { -d.dx } else { d.dx };
            let event = match d.phase {
                Phase::Down => ResizeEvent::Down { width },
                Phase::Move => ResizeEvent::Move { dx, min, max },
                Phase::Up => ResizeEvent::Up { cancelled: d.cancelled() },
            };
            s.workspace_panes.panel_sidebar.step(event);
        })
        .child(pill)
}
