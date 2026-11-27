//! The project/worktree/branch switcher dialog view (`new-session/location/dialog.tsx`):
//! `<Dialog size="large" fit>` on `bg-base` with `--v2-elevation-overlay` and 12 pt corners.

use opencode_i18n::t;
use opencode_ui::focus::FocusExt as _;
use opencode_ui::keymap::{Bind, keycaps};
use opencode_ui::widgets::DialogSize;
use rune::{Color, Cx, Direction, El, PointerEvent, Span, Spring, TextStyle, Transition, Tx, col, el, icon, row, text};
use rune_theme_opencode::icons as ic;
use opencode_ui::palette::{Elevate as _, Pal};
use opencode_ui::style::{UI, field};

use super::{ProjectSwitcherHost, RowKind, StepMode, SwitcherRow, active, branches, rows, servers};
use crate::search_fold;

/// A row's title: `palette-menu-title`, 13 at 530.
const TITLE: TextStyle = UI;

fn highlighted(s: &str, q: &str, style: TextStyle, color: Color, accent: Color) -> El {
    let t = text(s.to_owned()).font(style).color(color).truncate();
    match search_fold().find(s, q.trim()).first() {
        Some(r) => t.spans(vec![Span::new(r.start, r.end).color(accent)]),
        None => t,
    }
}

/// The dialog (`<Dialog size="large" fit>`, `new-session/location/dialog.tsx`): large, on bg-base
/// with `--v2-elevation-overlay` and 12 pt corners, as tall as it needs up to the size's height,
/// growing down from the size's top edge. The search field (28 tall, transparent, the placeholder
/// in the base ink at half strength), a hairline 8 below it, and the rows 4 apart under it.
pub fn view<H: ProjectSwitcherHost + 'static>(s: &H, p: &Pal, cx: &Cx<H>) -> [El; 2] {
    let host: &dyn ProjectSwitcherHost = s;
    let [win_w, win_h] = cx
        .layout_size("window")
        .or_else(|| cx.layout_size("zoomed"))
        .unwrap_or([1280.0 / host.zoom().max(1e-3), 820.0 / host.zoom().max(1e-3)]);
    let size = DialogSize::Large;
    let w = size.width(win_w);
    let max_h = size.height(win_h);
    let Some(open) = host.project_switcher().state.open_state() else {
        return [el().key("project-switcher-scrim"), el().key("project-switcher")];
    };
    let q = host.project_switcher().query();
    let list_rows = rows(host);
    let lit = active(host, &list_rows);
    let (placeholder, step_title) = step_meta(&open.mode);

    // Every step past the first steps back from the search field's arrow (`dialog.tsx`:
    // `view().ownsEscape`), the worktree step too (opencode#66090). A dialog opened on one
    // project's worktrees alone has no step to go back to (`step().history.length > 0`,
    // opencode#66096).
    let leading = if !matches!(open.mode, StepMode::Projects) && !open.history.is_empty() {
        search_back_shell(p).on_press_tx(|s: &mut H, tx: &mut Tx| super::back(s, tx))
    } else {
        icon(&ic::MAGNIFYING_GLASS, 16.0).color(p.icon_muted)
    };
    let input = search_input_shell(host, p, placeholder)
        .on_edit(|s: &mut H| &mut s.project_switcher_mut().search)
        .on_input_change(|s: &mut H, tx: &mut Tx| super::branch_search(s, tx))
        .on_input_key(|s: &mut H, ev, tx| super::key_or_shortcut(s, ev, tx));
    let search = search_row_shell(leading, input);

    // `command-palette-results` (`location/dialog.css:51-62`): 6 in at the sides and foot, rows 2
    // apart, 4 above the first row unless a group title starts the group.
    let mut list = col().key("project-switcher-results").w_full().px(6.0).pb(6.0).gap(2.0).pt(if step_title.is_some() { 0.0 } else { 4.0 });
    let mut content_h = if step_title.is_some() { 6.0_f32 } else { 10.0 };
    // `currentDestination`: the drilled project's row the draft runs in shows a check.
    let current = match &open.mode {
        StepMode::Worktrees { project, .. } if host.draft_project().is_some_and(|d| &d.id == project) => host.draft_dir(),
        _ => None,
    };
    if let Some(title) = step_title {
        // The list's heading, not a control or a Tab stop (opencode#66090): it was the step's
        // back button.
        list = list.role("group").label_static(title).child(step_title_shell(p, title));
        content_h += 32.0;
    }
    if list_rows.is_empty() {
        let waiting = matches!(open.mode, StepMode::Branches { list: branches::List::Loading(_), .. });
        list = list.child(empty_shell(p, waiting));
        content_h += 120.0;
    }
    let mut shown_other_heading = false;
    for (idx, r) in list_rows.iter().enumerate() {
        if !q.is_empty() && open.mode == StepMode::Projects && r.section == "Other projects" && !shown_other_heading {
            shown_other_heading = true;
            list = list.child(other_heading_shell(p));
            content_h += 32.0;
        }
        // A hairline across the dialog before Add project, and between New worktree and the
        // worktrees (`dialog.tsx:371-382`).
        let after_actions = idx > 0 && matches!(r.kind, RowKind::Worktree { main: false, .. }) && !matches!(list_rows[idx - 1].kind, RowKind::Worktree { main: false, .. });
        if idx > 0 && (r.kind == RowKind::Add || after_actions) {
            // `new-session-location-action-divider`: a 1 pt rule at half its height, 2 above and below (`5 pt` + `2 pt` flex gap = `7 pt`).
            list = list.child(rune::hairline(p.border).key(format!("psw-divider-{idx}")).my(2.0).mx(-6.0));
            content_h += 7.0;
        }
        let checked = current.is_some_and(|dir| matches!(&r.kind, RowKind::Worktree { directory, .. } if same_dir(directory, dir)));
        list = list.child(draw_row(s, p, r, lit.as_deref() == Some(r.key.as_str()), q, w).children(checked.then(|| icon(&ic::CHECK, 16.0).color(p.icon_muted))));
        content_h += 34.0;
    }
    content_h -= 2.0;

    // The search block (6 above, the field) and `new-session-location-divider` (1 pt, 6 above, 2 below).
    let head_h = 6.0 + 28.0 + 6.0 + 1.0 + 2.0;
    let scroll_h = content_h.min(max_h - head_h).max(0.0);
    // `MenuScroll` (`new-session/location/dialog.tsx:335`): its thumb shows while the list scrolls.
    let scroll = opencode_ui::scroller::scroller("project-switcher-scroll")
        .bar(opencode_ui::scroller::Bar::MenuScroll)
        .build::<H>(p, col().role("listbox").w_full().h(scroll_h), list);
    let dialog = build_dialog_card(p, placeholder, w, head_h + scroll_h, search, scroll).modal(true, |s: &mut H| super::close(s, true));
    let y = size.top(win_h);
    let scrim = el().key("project-switcher-scrim").absolute(0.0, 0.0).w_full().h_full().fill(p.scrim).transition(Transition::none());
    [scrim, dialog.absolute((win_w - w) / 2.0, y)]
}

fn step_meta(mode: &StepMode) -> (&'static str, Option<&'static str>) {
    match mode {
        StepMode::Projects => (t!("session.new.project.search"), None),
        StepMode::Others => (t!("session.new.project.other.search"), None),
        StepMode::Servers => (t!("session.new.location.servers.search"), None),
        StepMode::Branches { .. } => (t!("session.new.workspace.branch.search.placeholder"), Some(t!("session.new.workspace.runIn"))),
        StepMode::Worktrees { .. } => (t!("session.new.workspace.search.placeholder"), Some(t!("session.new.workspace.runIn"))),
    }
}

fn search_back_shell(p: &Pal) -> El {
    el().key("project-switcher-search-back")
        .role("button")
        .label_static(t!("session.new.location.back"))
        .size(24.0, 24.0)
        .mx(-4.0)
        .center()
        .radius(4.0)
        .focus_inset(p)
        .hover(p.hover)
        .child(icon(&ic::ARROW_LEFT, 16.0).color(p.icon_muted).mirror_in_rtl())
}

fn search_input_shell(s: &dyn ProjectSwitcherHost, p: &Pal, placeholder: &'static str) -> El {
    field(&s.project_switcher().search, UI)
        .key("project-switcher-input")
        .single_line()
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .color(p.text)
        .placeholder(placeholder, p.text.alpha(p.text.a * 0.5))
        .caret(p.text, p.selection)
        .selection_style(p.field_selection)
}

fn search_row_shell(leading: El, input: El) -> El {
    row()
        .key("project-switcher-field")
        .h(28.0)
        .pl(6.0)
        .pr(6.0)
        .gap(6.0)
        .center_y()
        .child(leading)
        .child(input)
}

/// `command-palette-group-title` in the location dialog (`location/dialog.css:70-80`): 10 above
/// and 2 below an 18 pt line, 8 in, 12 pt `text-faint`.
const GROUP_TITLE: TextStyle = TextStyle::new(12.0, 440.0).lh(18.0).tracking(0.05);

fn group_title_shell(key: &'static str) -> El {
    row().key(key).h(30.0).px(8.0).pt(10.0).pb(2.0).center_y()
}

fn step_title_shell(p: &Pal, title: &'static str) -> El {
    group_title_shell("project-switcher-title").child(text(title).font(GROUP_TITLE).color(p.faint))
}

fn empty_shell(p: &Pal, waiting: bool) -> El {
    col()
        .key("project-switcher-empty")
        .w_full()
        .h(120.0)
        .center()
        .children((!waiting).then(|| text(t!("palette.empty")).font(UI).color(p.muted)))
}

fn other_heading_shell(p: &Pal) -> El {
    group_title_shell("psw-other-heading").child(text(t!("session.new.location.otherProjects")).font(GROUP_TITLE).color(p.faint))
}

/// `sameDirectory`: one folder however its separators and trailing slash are written.
fn same_dir(a: &str, b: &str) -> bool {
    let norm = |s: &str| s.trim_end_matches(['/', '\\']).replace('\\', "/");
    norm(a) == norm(b)
}

fn build_dialog_card(p: &Pal, placeholder: &'static str, w: f32, total_h: f32, search: El, scroll: El) -> El {
    col()
        .key("project-switcher")
        .role("dialog")
        .label(placeholder)
        .w(w)
        .h(total_h)
        .radius(12.0)
        .fill(p.bg_base)
        .elevate(p.overlay)
        .clip_soft(0.0)
        .transition(Transition::none())
        .motion(Spring::NONE)
        .child(col().key("project-switcher-search").w_full().pt(6.0).px(6.0).child(search))
        // A 1 pt rule drawn at half its height (`scaleY(0.5)`), 6 under the field and 2 above the rows.
        .child(rune::hairline(p.border).key("project-switcher-divider").w_full().mt(6.0).mb(2.0))
        .child(scroll)
}

/// A row (`palette-menu-row` at `data-size="compact"`, `dialog.css:97-121`): 28 tall, 6 in, 6 pt
/// corners, the hover overlay while lit; its glyph, then its title (13 at 530) and its folder
/// or description in faint, both cut with an ellipsis, and a chevron where it steps in.
fn draw_row<H: ProjectSwitcherHost + 'static>(s: &H, p: &Pal, r: &SwitcherRow, active: bool, q: &str, w: f32) -> El {
    let host: &dyn ProjectSwitcherHost = s;
    let key = r.key.clone();
    let hover = r.key.clone();
    let multi = !host.project_switcher().servers().is_empty();
    let main = row_main(host, p, r, q, w);
    let line = row_shell(p, r, active, main)
        // Only a moving pointer takes the row (`dialog.tsx:395-399`), so rows the keys scroll
        // under a still one leave the highlight alone.
        .on_pointer_move_local(move |s: &mut H, ev: PointerEvent, tx: &mut Tx| super::row_pointer_move(s, &hover, ev, tx))
        .on_press_tx(move |s: &mut H, tx: &mut Tx| super::choose(s, &key, tx));
    match &r.kind {
        RowKind::Add if multi => line
            .child(keycaps(p, Bind::OpenProject))
            .child(drill(p, "psw-drill-servers", t!("session.new.location.servers"), |s: &mut H, tx: &mut Tx| super::choose(s, "+add", tx))),
        RowKind::Add => line.child(keycaps(p, Bind::OpenProject)),
        RowKind::Other { .. } => line.child(drill(p, "psw-drill-other", t!("session.new.location.otherProjects"), |s: &mut H, tx: &mut Tx| super::choose(s, "other", tx))),
        RowKind::Project { id, drillable: true, server, .. } => {
            let (pid, server, key) = (id.clone(), server.clone(), r.key.clone());
            line.child(drill(p, &format!("psw-drill-{id}"), t!("session.new.location.worktrees"), move |s: &mut H, tx: &mut Tx| {
                super::drill_server_worktrees(s, server.clone(), &pid, key.clone(), tx)
            }))
        }
        _ => line,
    }
}

fn row_main(s: &dyn ProjectSwitcherHost, p: &Pal, r: &SwitcherRow, q: &str, w: f32) -> El {
    let option = matches!(r.kind, RowKind::Other { .. } | RowKind::Add);
    let title_ink = if option { p.muted } else { p.text };
    let title = highlighted(&r.title, q, TITLE, title_ink, p.accent);
    let description = |t: &str| highlighted(t, q, UI, p.faint, p.accent).grow(1.0).shrink().min_w(0.0);
    // The name keeps up to 55% of its text (a worktree's title 60%) while the folder gives way:
    // the row less its insets, glyph and chevron.
    let text_w = w - 12.0 - 12.0 - 24.0 - if matches!(r.kind, RowKind::Project { drillable: true, .. }) { 24.0 } else { 0.0 };
    let named = |t: El, share: f32| row().key("name").max_w(text_w * share).center_y().child(t.shrink().min_w(0.0));
    let main = row().key("main").grow(1.0).shrink().min_w(0.0).gap(8.0).center_y();
    match &r.kind {
        RowKind::Project { id, server, .. } | RowKind::Worktree { project_id: id, main: true, server, .. } => {
            // Another server's project shows its server's tag after its name (`ServerTag`).
            let (project, tag) = match server {
                Some(k) => (servers::project(s, k, id), servers::tag(s, p, k)),
                None => (s.project(id), None),
            };
            let branch = (matches!(r.kind, RowKind::Worktree { main: true, .. }) && tag.is_none()).then(|| branches::checkout(s, server.as_ref(), &r.subtitle)).flatten();
            let name = row()
                .key("name")
                .max_w(text_w * 0.55)
                .gap(6.0)
                .center_y()
                .child(title.shrink().min_w(0.0))
                .children(tag)
                .children(branch.map(|b| row().gap(4.0).center_y().child(icon(&ic::BRANCH, 16.0).color(p.icon_muted)).child(text(b.to_owned()).font(UI).color(p.muted).truncate())));
            main.child(s.project_badge(p, project, &r.title))
                .child(row().key("text").grow(1.0).shrink().min_w(0.0).gap(6.0).center_y().child(name).child(description(&r.subtitle)))
        }
        RowKind::Branch => main.child(icon(&ic::BRANCH_OUT, 16.0).color(p.icon_muted)).child(title.dir(Direction::Auto)),
        RowKind::Server { key } => main.child(icon(servers::glyph(servers::kind(s, key)), 16.0).color(p.icon_muted)).child(
            row()
                .key("text")
                .grow(1.0)
                .shrink()
                .min_w(0.0)
                .gap(6.0)
                .center_y()
                .child(title.shrink().min_w(0.0))
                .children((!r.subtitle.is_empty()).then(|| description(&r.subtitle))),
        ),
        RowKind::Worktree { main: false, .. } | RowKind::Create { .. } => {
            let glyph = if matches!(r.kind, RowKind::Create { .. }) { &ic::PLUS } else { &ic::OUTLINE_WORKTREE };
            main.child(icon(glyph, 16.0).color(p.icon_muted))
                .child(row().key("text").grow(1.0).shrink().min_w(0.0).gap(6.0).center_y().child(named(title, 0.6)).child(description(&r.subtitle)))
        }
        RowKind::Other { count } => main
            .child(icon(&ic::MENU, 16.0).color(p.icon_muted))
            .child(title.shrink().min_w(0.0))
            .child(text(count.to_string()).font(UI.features("\"tnum\"")).color(p.faint)),
        RowKind::Add => main.child(icon(&ic::PLUS, 16.0).color(p.icon_muted)).child(title.shrink().min_w(0.0)),
    }
}

fn row_shell(p: &Pal, r: &SwitcherRow, active: bool, main: El) -> El {
    row()
        .key(format!("psw-{}", r.key))
        .role("option")
        .label(r.title.clone())
        .w_full()
        .h(32.0)
        .pl(8.0)
        .pr(6.0)
        .gap(8.0)
        .center_y()
        .radius(6.0)
        .transition(Transition::none())
        .motion(Spring::NONE)
        .fill(if active { p.hover } else { Color::TRANSPARENT })
        .child(main)
}

fn drill_shell(p: &Pal, key: &str, label: &str) -> El {
    el().key(key.to_owned())
        .role("button")
        .label(label.to_owned())
        .size(20.0, 20.0)
        .mx(-2.0)
        .radius(4.0)
        .center()
        .focus_inset(p)
        .hover_group()
        .hover(p.hover)
        .child(icon(&ic::CHEVRON_RIGHT, 16.0).mirror_in_rtl().color(p.icon_faint).group_color(p.icon))
}

/// `new-session-location-drill`: a 20 pt chevron, faint, that lights with its own hover.
fn drill<H: ProjectSwitcherHost + 'static>(p: &Pal, key: &str, label: &str, on: impl Fn(&mut H, &mut Tx) + 'static) -> El {
    drill_shell(p, key, label).on_press_tx(on)
}
