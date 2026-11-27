use super::*;
use crate::session_menu::{Spot, Target};
use opencode_i18n::t;
use rune_kit::{ScrollBar, ScrollBarStyle};

pub(crate) mod fold;
pub(crate) mod browse;
pub(crate) mod list;
pub(crate) mod projects;
mod search;
mod servers;
pub(crate) mod tip;

pub(crate) use projects::home_projects_popover;

// ---------------------------------------------------------------------------------------------
// Home (`home/route.tsx`): projects (`home/projects/view.tsx`) beside recent sessions by day
// (`home/sessions/view.tsx`), in a centred `max-w-[1056px]` grid that scrolls as one.

/// Tailwind's `lg` and `md` breakpoints, on the window's width as Desktop's media queries are.
const LG: f32 = 1024.0;
const MD: f32 = 768.0;

/// Which of Desktop's three Home layouts the window's width picks (`route.tsx`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tier {
    /// Under 768: the projects as a dropdown above the sessions.
    Narrow,
    /// 768 to 1023: the projects column has no room beside the sessions, so the picker is in
    /// the search field (`route.tsx` `tablet`); Settings and Help along the foot.
    Medium,
    /// 1024 and up: `grid-cols-[256px_minmax(0,720px)]`, Settings and Help under the projects.
    Wide,
}

impl Tier {
    fn of(window_w: f32) -> Tier {
        if window_w >= LG {
            Tier::Wide
        } else if window_w >= MD {
            Tier::Medium
        } else {
            Tier::Narrow
        }
    }

    /// The sticky search header's height plus the list's top padding, where the group headers
    /// stick and the New session button sits (`top-[68px] md:top-[108px]`).
    fn header_top(self) -> f32 {
        match self {
            Tier::Narrow => 68.0,
            Tier::Medium | Tier::Wide => 108.0,
        }
    }
}

/// Whether the project picker sits in the search field, not in a column or a bar of its own
/// (`route.tsx` `scoped`): Consolidated navigation's Side tabs, or a window between `md` and
/// `lg`. For code outside the page's own build, from the window's width as last laid out.
pub(super) fn scoped(s: &App) -> bool {
    s.home_project_scope() || rune::memo::layout_with("zoom", |r| r[2]).is_some_and(|w| Tier::of(w) == Tier::Medium)
}

/// A pick from the picker in the search field (`route.tsx` `onPick`): the search closes, as a
/// pick from the projects column does by pressing outside it. An inline search filters the list
/// in place, so its query carries over to the new project.
pub(super) fn picked(s: &mut App, tx: &mut Tx) {
    if scoped(s) && !s.settings.navigation_prototype() && !s.search.text.is_empty() {
        s.clear_home_search(false, tx);
    }
}

/// `max-w-[1056px]`, and the 256 pt projects column 32 from the sessions (`lg:gap-8`): the
/// columns Settings' sidebar and content line up with (opencode#65998).
/// Home's labels: Desktop's rows, headings and project list leave letter-spacing normal (13 px at
/// 440 or 530), where its controls take -0.04 (`UI`), on Desktop's 24 px line: a 16 pt line centred
/// in the row put the text half a device pixel lower at 1.875x, a pixel lower once rounded.
const HOME: rune::TextStyle = rune::TextStyle::new(13.0, 440.0).lh(24.0);
const HOME_MED: rune::TextStyle = rune::TextStyle::new(13.0, 530.0).lh(24.0);

const GRID_MAX: f32 = 1056.0;
/// Consolidated navigation's single-column width (`max-w-[768px]`).
const SCOPED_MAX: f32 = 768.0;
const PROJECTS_W: f32 = 256.0;
const GRID_GAP: f32 = 32.0;
const SESSIONS_MAX: f32 = 720.0;

/// Home in the panel `panel` big, in a window `window` big (zeros when unknown: a story).
pub(crate) fn home(s: &App, p: &Pal, [panel_w, panel_h]: [f32; 2], window: [f32; 2]) -> El {
    if s.project_navigation() { return browse::landing(s, p); }
    // One server that didn't answer shows the setup card, unless it answered HTTP 401 (`Signed out`);
    // with several servers, Home lists them.
    let sidecar_auth = crate::servers::health::is_unauthorized(&opencode_kit::servers::ServerKey::sidecar());
    if crate::launch::offline(s) && !servers::grouped(s) && !sidecar_auth {
        return no_server_home(s, p);
    }
    let selected = s.home_selected();
    let panel = if panel_w > 0.0 { panel_w } else { 1280.0 / s.zoom.max(1e-3) - s.sidebar_shown_w() - 16.0 };
    let window_w = if window[0] > 0.0 { window[0] } else { panel + s.sidebar_shown_w() + 16.0 };
    let window_h = if window[1] > 0.0 { window[1] } else { 800.0 };
    let tier = Tier::of(window_w);
    // `px-3 lg:px-6`.
    let pad = if tier == Tier::Wide { 24.0 } else { 12.0 };
    let project_scope = s.home_project_scope();
    // `scoped`: between md and lg the picker moves into the search field too.
    let scope = project_scope || tier == Tier::Medium;
    let grid_max = if scope { SCOPED_MAX } else { GRID_MAX };
    let grid_w = panel.min(grid_max) - 2.0 * pad;
    let column_w = if tier == Tier::Wide && !scope { (grid_w - PROJECTS_W - GRID_GAP).min(SESSIONS_MAX) } else { grid_w };
    let sessions = sessions_column(s, p, selected, tier, scope, column_w, window_h);
    let grid = match tier {
        _ if scope => col().key("home-grid").w_full().max_w(SCOPED_MAX).px(pad).child(sessions),
        Tier::Wide => row().key("home-grid").w_full().max_w(GRID_MAX).px(pad).gap(GRID_GAP).child(projects::column(s, p, selected, true, panel_h)).child(sessions),
        Tier::Medium | Tier::Narrow => col().key("home-grid").w_full().max_w(GRID_MAX).px(pad).child(sessions),
    };
    // Out of the Tab order, as Desktop's `ScrollView` (`tabIndex={-1}`, opencode#65758): the items
    // inside take Tab focus, and a click still focuses it so the keys scroll it. No ring
    // (opencode#65690).
    let viewport = col().w_full().shrink().min_h(0.0).clip_soft(0.0).focusable(true).tab_index(-1).role("region").label_static(opencode_i18n::t!("ui.scrollView.ariaLabel")).no_focus_ring();
    // The thumb sits in the sessions column's 12 pt gutter under its sticky header, in a track as
    // long as the page below it, shown while the pointer is on that column (`route.tsx:34-40`,
    // `sessions/view.tsx`'s `home-session-scroll-track`).
    let bar = ScrollBarStyle::scroll_view(crate::palette::kit(p)).hover_target().track_offset(tier.header_top(), gutter_end(panel, grid_max, pad));
    let scroller = session_view::scroller::scroller("home")
        .bar(session_view::scroller::Bar::Styled(bar))
        .build::<App>(p, viewport, col().key("home-content").w_full().items_center().child(grid))
        .grow(1.0);
    let mut page = col().key("home-page").grow(1.0).w_full().h_full();
    if tier == Tier::Narrow && !project_scope {
        // `relative z-40 -mb-3 shrink-0 px-3 pt-3`.
        // Stacks over the Home scroller only, not over the shell's drawer scrim.
        page = page.child(row().key("home-projects-bar").w_full().px(12.0).pt(12.0).mb(-12.0).child(projects::dropdown_trigger(s, p, selected)).paint_order(1));
    }
    // The scroller takes what the dropdown and the foot leave; the provider tip floats over its foot (#64885).
    page = page.child(col().key("home-body").w_full().grow(1.0).shrink().min_h(0.0).child(scroller).children(tip::tip(s, p)));
    if tier == Tier::Medium && !project_scope {
        // `hidden shrink-0 px-3 py-2 md:block lg:hidden`: a row, Settings and Help at its ends.
        page = page.child(projects::utility(s, p, false).key("home-utility-foot").px(12.0).py(8.0));
    }
    page
}

/// How far in from the page's end the sessions column's gutter ends: the grid is centred at most
/// `grid_max` wide, the column ends `pad` in from it, and its 12 pt track lies past the column
/// (`-mr-3`).
fn gutter_end(panel: f32, grid_max: f32, pad: f32) -> f32 {
    (panel - panel.min(grid_max)) / 2.0 + pad - 12.0
}

/// `HomeSessionsView`: the sticky search header, then the sessions grouped by day.
fn sessions_column(s: &App, p: &Pal, selected: Option<&model::Project>, tier: Tier, scoped: bool, width: f32, window_h: f32) -> El {
    let show_project = selected.is_none();
    let can_create = selected.is_some() || !s.used_projects().is_empty();
    let open = s.kernel.slots.get(&crate::restore::RESTORE).inbox.home_automations;
    let groups = s.home_groups(selected);
    let nav_ids = std::rc::Rc::new(groups.iter().flat_map(|g| g.shown_ids(open).map(str::to_owned)).collect::<Vec<_>>());
    // Each row: `ps-3 pe-3`, the 16 pt avatar and `gap-1.5` before its labels.
    let labels_w = (width - 12.0 - 12.0 - 16.0 - ROW_GAP).max(0.0);

    // `sticky top-0 bg-base pb-1 pt-6 md:pb-3 md:pt-12`.
    let mut header = col()
        .key("home-sessions-header")
        .w_full()
        .pt(if tier == Tier::Narrow { 24.0 } else { 48.0 })
        .pb(if tier == Tier::Narrow { 4.0 } else { 12.0 })
        .fill(p.bg_base)
        .sticky_top(0.0);
    header = header.child(search::field(s, p, selected, width, window_h, tier == Tier::Narrow, scoped));
    if !groups.is_empty() && can_create {
        // `absolute right-0 top-[108px] z-20`: on the first group's line, staying while the list
        // scrolls, under the search's results (`z-30`), and after the search in Tab order, as in
        // Desktop's DOM. `right-0` is physical, so it stays on the right right to left as well.
        // Pinned to the line's end, which mirrors: Desktop's `right-0` stays physical in a right-to-left
        // interface and lands on the first group's name; this sits at the left, clear of it.
        header = header.child(new_session_button(p).pin(None, Some(tier.header_top()), Some(0.0), None).paint_order(-1));
    }

    // The groups' `pt-1 md:pt-3 pb-16`; the empty state sits straight in the list.
    let count = groups.len();
    // While the pointer is on the list its order holds, so no row moves under it (`list.rs`).
    // Every row is a Tab stop of its own, as Desktop's row buttons are; arrows don't move between them.
    let mut list = col().key("groups").role("list").w_full().on_hover_change(|s: &mut App, inside, tx: &mut Tx| s.hold_home_order(inside, tx));
    if count > 0 {
        list = list.pt(if tier == Tier::Narrow { 4.0 } else { 12.0 }).pb(64.0);
    }
    // Headers and rows are siblings, as Desktop's are, so a stuck header stays until the next
    // one, drawn over it, takes its place; its title fades out as that one comes up
    // (`home/scroll.ts`: by `stickyTop + 16` against the next header's offset in the list, which
    // starts `list_top` down the page, over 16).
    let list_top = tier.header_top() - if tier == Tier::Narrow { 4.0 } else { 12.0 };
    let row_of = |ses: &SessionInfo| {
        let index = nav_ids.iter().position(|x| x == &ses.id).unwrap_or(0);
        list_row(s, p, ses, &nav_ids, index, show_project, labels_w, tier)
    };
    for (n, group) in groups.iter().enumerate() {
        let title = group.title;
        // `sticky h-7 ps-1.5 md:ps-3`, the label `text-muted [font-weight:440]`.
        let head = row()
            .key(format!("h-{title}"))
            .w_full()
            .h(28.0)
            .pl(if tier == Tier::Narrow { 6.0 } else { 12.0 })
            .center_y()
            .fill(p.bg_base)
            .sticky_top(tier.header_top())
            .stuck_fade(list_top + 16.0, 16.0)
            .child(text(title).font(HOME).color(p.muted))
            // `.home-session-group-header::before` and `::after` (`index.css:208-245`): a 12 pt
            // cover over the gap above it, and a 16 pt eased gradient into the rows under it; only
            // the title fades as the next header comes up.
            .child(el().key("cover").absolute(0.0, -12.0).w_full().h(12.0).fill(p.bg_base))
            .child(el().key("shade").absolute(0.0, 28.0).w_full().h(16.0).linear_gradient(std::f32::consts::PI, session_view::style::eased_fade(p.bg_base)));
        // `gap-px pt-2 md:pt-4`, `mb-6` but the last. Rows are the list's own children, so one
        // that changes day glides to its new group rather than leaving one and entering another.
        list = list.child(head).child(el().key(format!("pad-{title}")).w_full().h(if tier == Tier::Narrow { 8.0 } else { 16.0 }));
        for ses in &group.rows {
            list = list.child(row_of(ses).mb(1.0));
        }
        for f in &group.folds {
            let folded = if open { f.sessions.iter().map(|ses| row_of(ses)).collect() } else { vec![] };
            list = list.child(fold::view(p, title, f, open, folded, tier).mb(1.0));
        }
        if n + 1 < count {
            list = list.child(el().key(format!("end-{title}")).w_full().h(23.0));
        }
    }
    let filtering = s.settings.navigation_prototype() && !s.search.text.trim().is_empty();
    if count == 0 && filtering && !s.home_search_loading() {
        let query = s.search.text.trim();
        list = list.child(text(t!("home.sessions.search.noResults", query = query)).key("home-search-empty").font(UI).color(p.muted).px(8.0).pt(24.0));
    } else if count == 0 && !crate::launch::waiting(s) {
        list = list.child(empty(p, can_create));
    }
    // The focused server's list phase (`load-machine.ts`): placeholder rows while it connects, a
    // notice while it can't be reached, signs us out or is too old, or its list failed.
    use opencode_servers::home_load::{Failure, ListView, Unavailable};
    let list = match crate::projects::home::list_view(s) {
        ListView::Ready { .. } => list,
        ListView::Loading { skeleton } => {
            // A server that took the launch's connection and hasn't answered says so (`Link::Slow`).
            let slow = matches!(crate::reconnect::link_of(s, crate::servers::runtime::active(s).as_str()), opencode_kit::servers::link::Link::Slow { .. });
            let note = slow.then(|| rune::text(t!("rune.session.slowCopy").to_owned()).key("home-slow").font(crate::view::UI_SMALL).color(p.faint).mb(8.0));
            col().key("groups").w_full().children(note).children(skeleton.then(|| placeholder_rows(p, tier, show_project)))
        }
        ListView::Unavailable(reason) => {
            let title = match reason {
                Unavailable::Offline => t!("session.error.unreachable"),
                Unavailable::Auth => t!("session.error.signedOut"),
                Unavailable::Incompatible => t!("session.error.incompatible"),
            };
            let can_retry = reason == Unavailable::Offline
                || (reason == Unavailable::Auth && crate::projects::home::server(s).is_sidecar());
            col().key("groups").w_full().child(unavailable(p, title, None, can_retry))
        }
        ListView::Error(failure) => {
            let why = match failure {
                Failure::Timeout => t!("home.sessions.load.timeout").to_owned(),
                Failure::Rejected(message) => message,
            };
            col().key("groups").w_full().child(unavailable(p, t!("home.sessions.load.failed"), Some(why), true))
        }
    };
    // `HomeSessionsView`'s `<section>` reveals the page's thumb (`thumbHoverTarget`). Only the
    // wide grid caps it (`lg:grid-cols-[280px_minmax(0,720px)]`); below that it takes the grid.
    let column = col().key("home-column").grow(1.0).shrink().min_w(0.0);
    let column = if tier == Tier::Wide { column.max_w(SESSIONS_MAX) } else { column };
    column.child(header).child(list).scroll_bar_hover("home")
}

/// The ghost-muted New session button (`h-7 px-2 [font-weight:530]`, the `edit` icon).
fn new_session_button(p: &Pal) -> El {
    row()
        .key("new-session")
        .role("button")
        .label_static(t!("command.session.new"))
        .h(28.0)
        .px(8.0)
        .gap(6.0)
        .center_y()
        .radius(6.0)
        .hover(p.hover)
        .press(p.pressed)
        .on_press_tx(|s: &mut App, tx: &mut Tx| s.new_session(tx))
        .child(icon(&ic::EDIT, 16.0).color(p.icon_muted))
        // A `button-v2`: figures at one width (`button.css:21`).
        .child(text(t!("command.session.new")).font(UI_MED.features("\"tnum\"")).color(p.muted))
}

/// `HomeSessionsEmpty`: centred 52 below the list's top, 16 apart: the title, the line under it
/// and, when a session can start, a neutral New session button. Official's `home-sessions-list`
/// carries `-mr-3` while `HomeSessionsEmpty` uses `px-6` without the non-empty list's `pr-3`,
/// shifting its centre +6 pt (`pl(36.0).pr(24.0)`).
fn empty(p: &Pal, can_create: bool) -> El {
    let mut e = col()
        .key("home-empty")
        .w_full()
        .pl(36.0)
        .pr(24.0)
        .pt(52.0)
        .gap(16.0)
        .items_center()
        .child(text(t!("home.sessions.empty")).font(UI_MED).color(p.text))
        .child(text(t!("home.sessions.empty.description")).font(UI.lh(20.0)).color(p.muted).mb(4.0));
    if can_create {
        e = e.child(
            row()
                .key("home-empty-new")
                .role("button")
                .label_static(t!("command.session.new"))
                .h(28.0)
                .px(11.0)
                .gap(6.0)
                .center_y()
                .radius(6.0)
                .fill(p.q.neutral)
                .box_shadow(Pal::ring_out(p.q.neutral_ring))
                .shadow(1.0, 1.5, p.q.neutral_shadow)
                .hover(p.hover)
                .press(p.pressed)
                .on_press_tx(|s: &mut App, tx: &mut Tx| s.new_session(tx))
                .child(icon(&ic::EDIT, 16.0).color(p.text))
                .child(text(t!("command.session.new")).font(UI_MED.lh(20.0)).color(p.text)),
        );
    }
    e
}

/// `HomeSessionsUnavailable`: centred 52 below the list's top, 16 apart: the title (13 at 530),
/// what went wrong (13 on 20, muted), and Try again (a neutral button).
fn unavailable(p: &Pal, title: &str, description: Option<String>, retry: bool) -> El {
    let mut e = col().key("home-sessions-unavailable").role("status").w_full().px(24.0).pt(52.0).gap(16.0).items_center().child(text(title.to_owned()).font(UI_MED).color(p.text));
    if let Some(d) = description {
        e = e.child(text(d).font(UI.lh(20.0)).color(p.muted).wrap().align(rune::Align::Center).mb(4.0));
    }
    if retry {
        e = e.child(
            crate::widgets::button(t!("common.retry"), rune_kit::ButtonVariant::Neutral)
                .on_press_tx(|s: &mut App, tx: &mut Tx| crate::projects::home::retry(s, tx))
                .build(crate::palette::kit(p))
                .key("home-sessions-retry"),
        );
    }
    e
}

/// `HomeSessionSkeleton`: past the 28 pt header's room, six 40 pt rows `gap-px` (`pt-2
/// md:pt-4`): a 16 pt square and a bar, each in layer-02 (the project's bar in layer-01 when
/// rows show it), the bars as wide as Desktop's.
fn placeholder_rows(p: &Pal, tier: Tier, show_project: bool) -> El {
    const WIDTHS: [f32; 6] = [56.0, 38.0, 66.0, 46.0, 30.0, 52.0];
    let mut rows = col().key("home-session-skeleton").w_full().pt(if tier == Tier::Narrow { 4.0 } else { 12.0 }).pr(12.0).child(el().h(28.0)).child(el().h(if tier == Tier::Narrow { 8.0 } else { 16.0 }));
    for (i, width) in WIDTHS.iter().enumerate() {
        let bar = row().key("bar").grow(1.0).shrink().min_w(0.0).child(el().w_frac(width / 100.0).h(8.0).radius(4.0).fill(p.layer2));
        rows = rows.child(
            row()
                .key(format!("placeholder-{i}"))
                .w_full()
                .h(if tier == Tier::Narrow && show_project { 48.0 } else { 40.0 })
                .pl(if tier == Tier::Narrow { 6.0 } else { 12.0 })
                .pr(if tier == Tier::Narrow { 12.0 } else { 40.0 })
                .gap(if tier == Tier::Narrow { 8.0 } else { 6.0 })
                .mb(1.0)
                .center_y()
                .child(el().size(16.0, 16.0).radius(4.0).fill(p.layer2))
                .child(bar)
                .children(show_project.then(|| el().w(64.0).h(8.0).radius(4.0).fill(p.layer1))),
        );
    }
    rows
}

/// One session in Home's list (`HomeSessionRow`): 40 tall, `ps-3 pe-3`, the overlay hover,
/// its project avatar (the open-tab bar before it, the unread dot on it), the title, the
/// project, and its worktree and branch while the pointer is on it; the activity ring at its
/// end while it works. It opens on a click and has Desktop's right-click menu; while it's being
/// renamed the title is the field.
/// [`list_row`] as a wide Home draws it, for the storybook.
#[cfg(feature = "stories")]
pub(crate) fn session_row(s: &App, p: &Pal, ses: &SessionInfo, nav_ids: &std::rc::Rc<Vec<String>>, index: usize) -> El {
    list_row(s, p, ses, nav_ids, index, true, SESSIONS_MAX - 46.0, Tier::Wide)
}

#[allow(clippy::too_many_arguments)]
fn list_row(s: &App, p: &Pal, ses: &SessionInfo, nav_ids: &std::rc::Rc<Vec<String>>, index: usize, show_project: bool, labels_w: f32, tier: Tier) -> El {
    let id = ses.id.clone();
    let hover_id = id.clone();
    let nav = nav_ids.clone();
    let target = Target::on(crate::servers::runtime::active(s), &ses.id, Spot::Row);
    let marked = s.kernel.slots.get(&crate::session_menu::MENUS).on.as_ref() == Some(&target);
    let stacked = tier == Tier::Narrow && show_project;
    let parts = Parts::of(s, p, ses, labels_w, show_project, stacked, ROW_GAP);
    let mut r = row()
        .key(format!("r-{}", ses.id))
        .role("button")
        .label(ses.title.clone())
        .focusable(true)
        .focus_selected(index == 0)
        .w_full()
        .h(if stacked { 48.0 } else { 40.0 })
        .pl(if tier == Tier::Narrow { 6.0 } else { 12.0 })
        .pr(12.0)
        .gap(ROW_GAP)
        .center_y()
        .radius(6.0)
        .focus_inset(p)
        .hover_group()
        .hover(p.hover)
        .press(p.pressed)
        .on_hover_change(move |s: &mut App, inside, tx: &mut Tx| hover_session(s, &hover_id, inside, tx))
        .on_key(move |s: &mut App, ev, _: &mut Tx| s.prefetch_navigation(&nav, index, ev))
        .child(parts.avatar);
    if marked {
        // Keep the single hover wash while the drawn menu covers its trigger. The pointer
        // can still be over the row; do not composite the translucent ink a second time.
        r = r.fill(p.hover).hover(rune::Color::TRANSPARENT);
    }
    // Home's rename field is 13 on 24 (home/sessions/view.tsx's InlineInput in its 24 pt row).
    let labels = match crate::session_menu_view::field(s, p, &target, UI_MED.lh(24.0)) {
        Some(field) => row().key("labels").grow(1.0).shrink().min_w(0.0).gap(ROW_GAP).center_y().child(field).children(parts.project),
        None => {
            r = r.pointer_buttons([rune::PointerButton::Middle]).on_press_tx(move |s: &mut App, tx: &mut Tx| {
                if opens_in_background(tx) {
                    return s.open_in_background(&id);
                }
                s.clear_home_search(false, tx);
                s.open(&id, tx);
            });
            parts.labels
        }
    };
    crate::session_menu::on_home(r.child(labels).children(parts.ring), target)
}

/// A middle click, or ⌘-click (Ctrl+click off macOS) without Shift or Alt, opens a session as a
/// tab behind the page, as a browser opens a link (`home/sessions/open.ts`).
pub(super) fn opens_in_background(tx: &Tx) -> bool {
    match tx.button() {
        rune::PointerButton::Middle => true,
        rune::PointerButton::Primary => {
            let m = tx.mods();
            !m.shift && !m.alt && m.cmd && if cfg!(target_os = "macos") { !m.ctrl } else { !m.logo }
        }
        _ => false,
    }
}

/// The pointer on a session row or search result: it shows its worktree and branch, and its
/// page starts loading.
pub(super) fn hover_session(s: &mut App, id: &str, inside: bool, tx: &mut Tx) {
    if inside {
        s.prefetch_session(id);
        if s.kernel.slots.get(&crate::home_search::HOME_SEARCH).hovered.as_deref() != Some(id) {
            s.kernel.slots.get_mut(&crate::home_search::HOME_SEARCH).hovered = Some(id.to_owned());
            tx.instant();
            return;
        }
    } else if s.kernel.slots.get(&crate::home_search::HOME_SEARCH).hovered.as_deref() == Some(id) {
        s.kernel.slots.get_mut(&crate::home_search::HOME_SEARCH).hovered = None;
        tx.instant();
        return;
    }
    tx.live_only();
}

/// `createAltHold`: while Alt is held on Desktop, every Home row and search result shows its
/// worktree and branch, and their branches load (`alt-hold.ts`, `onRevealLocations`).
pub(crate) fn home_modifiers(s: &mut App, mods: rune::Mods, tx: &mut Tx) {
    *s.kernel.slots.get_mut(&crate::attachments::SHIFT_HELD) = mods.shift;
    crate::tabs::sections::modifiers(s, mods);
    // The quote comment editor shows its alternate send while Mod is held, and tab avatars show
    // 1..9 shortcut numbers after holding Mod alone for 400 ms (#63089).
    let quote = crate::quote::editor::modifiers(s, mods, tx);
    let line_comment = crate::line_comments::modifiers(s, mods, tx);
    #[cfg(not(target_arch = "wasm32"))]
    let browser_comment = {
        s.workspace_panes.browser.shift_held = mods.shift;
        crate::surfaces::browser::comment::modifiers(s, mods, tx)
    };
    #[cfg(target_arch = "wasm32")]
    let browser_comment = false;
    let hints = crate::tab_hints::modifiers(s, mods, tx);
    if s.kernel.slots.get(&crate::home_search::HOME_SEARCH).alt_held == mods.alt {
        if quote || line_comment || browser_comment || hints {
            return;
        }
        return tx.live_only();
    }
    s.kernel.slots.get_mut(&crate::home_search::HOME_SEARCH).alt_held = mods.alt;
    tx.instant();
}

/// The web app's rows are `gap-2` where Desktop's are `gap-1.5` (`view.tsx`'s `!props.desktop`), and
/// their labels `contents`, so its title and project name are 8 apart too.
const WEB: bool = cfg!(target_arch = "wasm32");
const ROW_GAP: f32 = if WEB { 8.0 } else { 6.0 };

/// What a session row and a search result share.
pub(super) struct Parts {
    pub avatar: El,
    pub labels: El,
    /// The project name alone, for the rename field to sit before.
    pub project: Option<El>,
    pub ring: Option<El>,
}

impl Parts {
    /// `ses`'s pieces for labels `labels_w` wide. `stacked` on a narrow window: the project
    /// under the title, the row 48 tall (`view.css`'s mobile rule).
    pub fn of(s: &App, p: &Pal, ses: &SessionInfo, labels_w: f32, show_project: bool, stacked: bool, gap: f32) -> Parts {
        let project = s.project_for_session(ses);
        let name = project_or_dir(project, &ses.directory);
        // `HomeSessionIndicators`: a busy session shows its activity ring instead of the dot.
        let marks = s.marks(&ses.id);
        let busy = marks.busy;
        let open = s.tabs.iter().any(|t| t.id == ses.id);
        let avatar = avatar(s, p, project, &name, open, marks.unread && !busy);
        // `max-w-[min(70%,480px)] flex-[0_1_auto]` beside the project, else the whole room.
        // Each in its own direction (`view.tsx:908`, `:959`), so a Latin project name runs to the
        // row's far end right to left.
        // Stacked on a narrow window, the two labels take 16 pt lines (`view.css`'s mobile rule).
        let (title_font, project_font) = if stacked { (HOME_MED.lh(16.0), HOME.lh(16.0)) } else { (HOME_MED, HOME) };
        let title = text(title_of(ses)).font(title_font).color(p.text).truncate().shrink().min_w(0.0).dir(rune::Direction::Auto);
        // The room after the title grows; the name keeps to its start, beside the title, whichever way it
        // reads (a left-to-right name in a right-to-left row would otherwise sit at the row's far end).
        let project_el = show_project.then(|| row().key("project-room").grow(1.0).shrink().min_w(0.0).child(text(name.clone()).key("project-name").font(project_font).color(p.muted).truncate().shrink().min_w(0.0).dir(rune::Direction::Auto)));
        if stacked && show_project {
            // Official centres its 32 pt labels in the 48 pt row (`items-center`): 8 above them.
            let labels = col().key("labels").grow(1.0).shrink().min_w(0.0).self_start().mt(8.0).child(title).children(project_el.clone());
            return Parts { avatar, labels, project: project_el, ring: busy.then(|| activity_ring(p).key("ring")) };
        }
        let title = if show_project { title.max_w((labels_w * 0.7).min(480.0)) } else { title.grow(1.0) };
        let mut labels = row().key("labels").grow(1.0).shrink().min_w(0.0).gap(gap).center_y().child(title).children(project_el.clone());
        // The web app shows no worktree and branch on a row (`HomeSessionLocation`'s `!desktop`).
        if WEB {
            return Parts { avatar, labels, project: project_el, ring: busy.then(|| activity_ring(p).key("ring")) };
        }
        // Shown while the row is hovered or Alt is held; it hides at once when the pointer moves to another row.
        let hs = s.kernel.slots.get(&crate::home_search::HOME_SEARCH);
        let hovered = hs.hovered.as_deref();
        let show = hs.alt_held || hovered == Some(ses.id.as_str());
        let instant_hide = !hs.alt_held && hovered.is_some_and(|h| h != ses.id);
        labels = labels.child(location(s, p, ses, labels_w, show, instant_hide));
        Parts { avatar, labels, project: project_el, ring: busy.then(|| activity_ring(p).key("ring")) }
    }
}

/// `HomeSessionLocation`: the worktree's folder and its branch, each after its icon, `gap-1.5`,
/// at most 60% of the labels. It opens and fades in over 120 ms (iew.tsx:921-938), or hides at
/// once (`instant_hide`).
fn location(s: &App, p: &Pal, ses: &SessionInfo, labels_w: f32, show: bool, instant_hide: bool) -> El {
    let folder = opencode_kit::path::basename(&ses.directory);
    let worktree = if folder.is_empty() { ses.directory.clone() } else { folder.to_owned() };
    let fade = if instant_hide { rune::Anim::INSTANT } else { rune::Anim::new(Curve::tween(0.12, Easing::EASE_OUT)) };
    let mut l = row()
        .key("location")
        .max_w(if show { labels_w * 0.6 } else { 0.0 })
        .ml(if show { 0.0 } else { -6.0 })
        .opacity(if show { 1.0 } else { 0.0 })
        .opacity_motion(fade)
        .motion(if instant_hide { Spring::NONE } else { Spring::new(0.12, 0.0) })
        .shrink()
        .min_w(0.0)
        .gap(6.0)
        .center_y()
        .clip_soft(0.0)
        .dir(rune::Direction::Ltr)
        .child(icon(&ic::OUTLINE_WORKTREE, 16.0).color(p.icon_muted))
        .child(text(worktree).font(HOME).color(p.muted).truncate().shrink().min_w(0.0));
    if let Some(branch) = s.sidebar_inbox.branches.get(crate::servers::runtime::active(s), &ses.directory) {
        l = l.child(icon(&ic::BRANCH, 16.0).color(p.icon_muted)).child(text(branch.to_owned()).font(HOME).color(p.muted).truncate().shrink().min_w(0.0));
    }
    l
}

/// `HomeSessionLeading`: the 16 pt project avatar; the open-tab bar 4 before it (`h-3 w-0.5
/// rounded-[2px] bg-layer-04`); and the unread dot on its corner (`project-avatar.css`: the
/// avatar cut clear 4.5 around a 6 pt `bg-accent` dot), a real cutout, so it holds over the
/// row's hover and selection.
pub(super) fn avatar(s: &App, p: &Pal, project: Option<&model::Project>, name: &str, open: bool, unread: bool) -> El {
    crate::icons::session_avatar(s, p, project, name, open, unread).key("avatar")
}
/// `--service` writes the server's address and password to `service.json`, where the app finds them;
/// a plain `opencode serve` (2.x) makes up a password the app can't know, and shows Signed out.
const SERVE_CMD: &str = "opencode serve --service";

fn no_server_home(s: &App, p: &Pal) -> El {
    let addr = match (&s.launch.story_addr, &*s.source) {
        (Some(a), _) => a.as_str(),
        (None, crate::Source::Live { server, .. }) => server.base.as_str(),
        (None, crate::Source::Fixture) => opencode_kit::client::DEFAULT_SERVER,
    };
    // Only the local default is a server to start; any other address is one to reach.
    let local = addr == opencode_kit::client::DEFAULT_SERVER;
    let checking = crate::launch::checking(s);
    let copy_key = "no-server-copy";
    let cmd_row = row()
        .key("no-server-cmd")
        .w_full()
        .h(36.0)
        .pl(12.0)
        .pr(6.0)
        .gap(8.0)
        .center_y()
        .radius(6.0)
        .fill(p.bg_base)
        .box_shadow(Pal::ring_out(p.ring))
        .child(text(SERVE_CMD).font(CODE).color(p.text).grow(1.0).shrink().truncate_fade())
        .child(
            row()
                .key(copy_key)
                .role("button")
                .label_static(t!("rune.home.noServer.copyCommand"))
                .help(t!("rune.home.noServer.copyCommand"))
                .size(24.0, 24.0)
                .center()
                .radius(4.0)
                .hover(p.hover)
                .press(p.pressed)
                .hover_motion(INSTANT, INSTANT)
                .on_press_tx(move |s: &mut App, tx: &mut Tx| {
                    tx.copy(SERVE_CMD);
                    s.mark_copied(copy_key, tx);
                })
                .child(crate::copied::glyph(s, copy_key, 14.0, p.icon_muted)),
        );

    let mut retry = row()
        .key("no-server-retry")
        .role("button")
        .label(if checking { t!("rune.home.noServer.checking") } else { t!("rune.home.noServer.retry") })
        .h(30.0)
        .px(12.0)
        .center()
        .radius(6.0)
        .fill(p.bg_base)
        .box_shadow(Pal::ring_out(p.ring))
        .hover(if checking { p.bg_base } else { p.hover })
        .hover_motion(INSTANT, INSTANT)
        .child(text(if checking { t!("error.page.action.checking") } else { t!("rune.home.noServer.retryShort") }).font(UI_MED).color(if checking { p.faint } else { p.text }));
    if !checking {
        retry = retry.press(p.pressed).on_press_tx(|s: &mut App, tx: &mut Tx| crate::launch::retry(s, tx));
    }

    let card = col()
        .key("no-server")
        .w_full()
        .max_w(460.0)
        .padding(20.0)
        .gap(14.0)
        .radius(10.0)
        .fill(p.layer2)
        .box_shadow(Pal::ring_out(p.ring))
        .child(
            col()
                .w_full()
                .gap(6.0)
                .child(text(if local { t!("rune.home.noServer.title") } else { t!("rune.home.noServer.remoteTitle") }).font(H2).color(p.text))
                .child(
                    text(if local { t!("rune.home.noServer.description").to_owned() } else { t!("rune.home.noServer.remoteDescription", address = addr) })
                        .font(PROMPT)
                        .color(p.muted)
                        .wrap(),
                ),
        )
        .children(local.then_some(cmd_row))
        .child(row().w_full().child(retry));

    // `connect-server`: a plain `overflow: auto` box's bar (`servers/connect/screen.css:5-7`).
    session_view::scroller::scroller("home").build::<App>(p, col().grow(1.0).w_full().clip_soft(0.0), col().key("home-content").w_full().center().px(24.0).pt(96.0).child(card))
}

mod no_server_story {
    use rune::{El, Play, col};

    use crate::App;
    use crate::palette::Pal;
    use crate::stories::{Story, Trigger, replay, stage};

    const VARIANTS: &[(&str, usize)] = &[("Disconnected", 0), ("Checking", 1), ("Other address", 2)];
    const KEYS: &[Trigger] = &[
        replay(open),
        Trigger {
            key: 'c',
            label: "Toggle checking",
            run: |s, tx| {
                tx.instant();
                let checking = !crate::launch::checking(s);
                crate::launch::story_offline(s, checking);
            },
        },
    ];

    fn setup(s: &mut App) {
        *s.route = crate::Route::Home;
        s.sessions.clear();
        s.tabs.clear();
        s.projects.clear();
        crate::launch::story_offline(s, s.fixture_run.variant == 1);
        s.launch.story_addr = (s.fixture_run.variant == 2).then(|| "http://127.0.0.1:14096".to_owned());
    }

    fn open(s: &mut App, tx: &mut Play<App>) {
        setup(s);
        tx.instant();
    }

    fn view(s: &App, p: &Pal) -> El {
        col()
            .key("story-no-server")
            .w(760.0)
            .h(440.0)
            .radius(10.0)
            .fill(p.bg_base)
            .box_shadow(Pal::ring_out(p.ring))
            .clip_soft(0.0)
            .child(super::home(s, p, [760.0, 440.0], [0.0, 0.0]))
    }

    rune_register::register!(crate::stories::STORY_LIST: Story = [
        Story { path: "Home/No server", scene: |s| { setup(s); stage(s, view); }, knobs: &[], keys: KEYS, variants: VARIANTS },
    ]);

    rune_register::register!(crate::scenes::SCENE_LIST: crate::scenes::Scene = [
        ("no-server", setup),
        ("no-server-remote", |s| { *s.fixture_run.variant = 2; setup(s) }),
    ]);
}
