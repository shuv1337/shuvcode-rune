use super::*;
use crate::popup::MenuButton as _;
use crate::popup::{self, Item, Menu};
use opencode_kit::servers::ServerKey;
use opencode_i18n::t;

// ---------------------------------------------------------------------------------------------
// Home's projects (`home/projects/view.tsx`, `view.css`, `menu.tsx`): the column beside the
// sessions, its rows with their hover actions and menu, and Settings and Help under them; on a
// narrow window the same rows in a dropdown.

/// Add project with one server listed (`onChooseProject(props.servers[0])`): this computer's
/// folder dialog for the app's own server, the server's own folders for another.
fn add_project(s: &mut App, tx: &mut Tx) {
    let key = crate::servers::runtime::active(s).clone();
    super::servers::choose_project(s, &key, tx);
}

/// `HomeProjectsPanel`. Wide: 280 pt, 56 down and sticking there (`lg:sticky lg:top-14 lg:mt-14`),
/// its header 52 below that, on the first group's line (`lg:pt-[52px]`), and Settings and Help
/// under the list; as tall as the page leaves under those 56 (`lg:h-[calc(100cqh-56px)]`, the page
/// `page_h` tall when known), its rows scrolling inside it, so it stays put at any height.
/// Stacked above the sessions: 10 down (`mt-2.5`), Settings and Help at the foot.
pub(super) fn column(s: &App, p: &Pal, selected: Option<&model::Project>, wide: bool, page_h: f32) -> El {
    let used = s.used_projects();
    let server = crate::servers::runtime::active(s);
    let failed = s.kernel.slots.get(&crate::projects::HOME_STATE).servers.iter().any(|v| v.key == *server && v.failed());
    // `h-7 pl-1.5 pr-3`: Projects in muted 530, and, with one server, the large ghost-muted
    // folder-add button (disabled while it's down).
    let mut header = row().key("home-projects-header").w_full().h(28.0).pl(6.0).pr(12.0).center_y().child(text(t!("home.projects")).font(HOME_MED).color(p.muted)).child(spacer());
    if s.kernel.slots.get(&crate::projects::HOME_STATE).servers.len() <= 1 {
        let add = el().key("home-add-project").role("button").label_static(t!("home.project.add")).help(t!("home.project.add")).size(28.0, 28.0).radius(6.0).focus_inset(p).center();
        let add = if failed { add.opacity(0.5).disabled(true).hit().cursor(rune::Cursor::NotAllowed) } else { add.hover(p.hover).press(p.pressed).on_press_tx(|s: &mut App, tx: &mut Tx| add_project(s, tx)) };
        header = header.child(add.child(icon(&ic::FOLDER_ADD_LEFT, 16.0).color(p.icon_muted)));
    }
    // `pr-3`, rows `gap-1`; a row is as wide as the list leaves it, less the scroll bar's gutter
    // while the rows overflow.
    let row_w = PROJECTS_W - 12.0 - if wide { session_view::scroller::gutter("home-projects-scroll") } else { 0.0 };
    let mut rows = col().key("home-projects-list").w_full().pr(12.0).gap(4.0);
    if super::servers::grouped(s) {
        rows = rows.child(super::servers::groups(s, p, false, row_w));
    } else if used.is_empty() {
        // `HomeProjectEmpty`: Add project, disabled (60%) while the server is down (`view.tsx:619`).
        let add = nav_row(p, "home-add-project-empty", &ic::FOLDER_ADD_LEFT, t!("home.project.add")).w_full();
        rows = rows.child(if failed {
            add.opacity(0.6).disabled(true)
        } else {
            add.on_press_tx(|s: &mut App, tx: &mut Tx| add_project(s, tx))
        });
    } else {
        for proj in used {
            rows = rows.child(project_row(s, p, server, proj, selected.is_some_and(|sel| sel.id == proj.id), row_w));
        }
    }
    if !wide {
        return col().key("home-projects").w_full().mt(10.0).gap(16.0).child(header).child(rows);
    }
    // `home-projects-viewport`: the rows scroll in what the column leaves them, their edges
    // masked 16 pt while rows hide past them (`index.css:353-376`), with a plain
    // `overflow-y-auto` box's bar (`view.tsx:227`).
    let viewport = col().w_full().shrink().min_h(0.0).scroll_fade(rune::ScrollFade::strength(16.0).linear().full_at(0.01));
    let rows = session_view::scroller::scroller("home-projects-scroll").build::<App>(p, viewport, rows);
    let c = col().key("home-projects").w(PROJECTS_W).gap(16.0).child(header).child(rows);
    let c = if page_h > 56.0 { c.h(page_h - 56.0).self_start() } else { c };
    // `HomeUtilityNav` `mb-8 mt-4`, after the panel's `gap-4`.
    c.mt(56.0).pt(52.0).sticky_top(56.0).child(utility(s, p, true).mt(16.0).mb(32.0))
}

/// `HomeUtilityNav`: Settings and Help. Under the projects (`lg:flex-col lg:pr-3`, full-width
/// rows `gap-1`), or along Home's foot, one at each end.
pub(super) fn utility(s: &App, p: &Pal, stacked: bool) -> El {
    let automations = crate::automations::available(s).then(|| {
        nav_row(p, "home-automations", &crate::automations::CLOCK, t!("automations.title"))
            .on_press_tx(|s: &mut App, tx: &mut Tx| crate::automations::open(s, tx))
    });
    let settings = nav_row(p, "home-settings", &ic::SETTINGS_GEAR, t!("sidebar.settings")).on_press_tx(|s: &mut App, tx: &mut Tx| {
        if !s.settings_open() {
            s.toggle_settings(tx);
        }
    });
    let help = nav_row(p, "home-help", &ic::HELP, t!("sidebar.help")).on_press_tx(|_: &mut App, tx: &mut Tx| {
        crate::open_in::open_external(tx, "https://opencode.ai/desktop-feedback");
    });
    if stacked {
        return col()
            .key("home-utility")
            .w_full()
            .pr(12.0)
            .gap(4.0)
            .children(automations.map(|a| a.w_full()))
            .child(settings.w_full())
            .child(help.w_full());
    }
    // `[&>button]:w-auto`: each as wide as its icon and label.
    row().w_full().gap(4.0).justify_between().children(automations).child(settings).child(help)
}

/// `HomeProjectNavButton`: 28 tall, `px-1.5 gap-2`, the normal (16 pt) icon in icon-muted, the
/// label muted at 440 going to the text colour over the layer-01 hover.
fn nav_row(p: &Pal, key: &str, glyph: &'static Icon, label: &str) -> El {
    row()
        .key(key)
        .role("button")
        .label(label)
        .h(28.0)
        .px(6.0)
        .gap(8.0)
        .center_y()
        .radius(6.0)
        .focus_inset(p)
        .hover_group()
        .hover(p.layer1)
        .child(icon(glyph, 16.0).key("icon").color(p.icon_muted))
        // `text-left` on a `flex-1` label: at the left edge right to left too.
        .child(text(label).font(HOME).color(p.muted).group_color(p.text).grow(1.0).shrink().truncate().align(text_left()))
}

/// The project menu's trigger: the menu opens below its end, 6 away (`gutter={6}`).
fn menu_key(id: &str) -> String {
    format!("hp-menu-{id}")
}

/// `HomeProjectRow` in a column `width` wide: the avatar and name as the nav button (layer-01
/// hovered, layer-03 selected), and at its end, while hovered or its menu is open, More options
/// and New session (small ghost-muted IconButtons, `right-1 gap-1 pl-2`) over an 8 pt fade into
/// the row's surface. A click scopes Home to it; a right-click opens its menu.
pub(super) fn project_row(s: &App, p: &Pal, server: &ServerKey, proj: &model::Project, active: bool, width: f32) -> El {
    let menu = menu_key(&proj.id);
    let menu_open = s.popup.open_at(&menu);
    let (menu_id, ctx_id, new_dir, dir) = (proj.id.clone(), proj.id.clone(), proj.directory.clone(), proj.directory.clone());
    let (menu_server, ctx_server, new_server, server) = (server.clone(), server.clone(), server.clone(), server.clone());
    let surface = if active { p.layer3 } else { p.layer1 };
    // `pl-2`, two 20 pt buttons `gap-1`: the fade runs over the first 8.
    let actions_w = 8.0 + 20.0 + 4.0 + 20.0;
    let actions = row()
        .key("actions")
        // 2 clear of the row's top and bottom, where the row's inset focus ring runs.
        .pin(None, Some(2.0), Some(4.0), Some(2.0))
        .w(actions_w)
        .pl(8.0)
        .gap(4.0)
        .center_y()
        .linear_gradient(std::f32::consts::FRAC_PI_2, [(0.0, surface.alpha(0.0)), (8.0 / actions_w, surface), (1.0, surface)])
        .child(
            icon_button(p, &menu, t!("common.moreOptions"), &ic::OUTLINE_DOTS, menu_open)
                .menu_button(menu.clone(), move |s: &mut App, tx: &mut Tx| open_menu(s, &menu_server, &menu_id, tx)),
        )
        .child(icon_button(p, &format!("hp-new-{}", proj.id), t!("command.session.new"), &ic::EDIT, false).on_press_tx(move |s: &mut App, tx: &mut Tx| {
            crate::servers::runtime::switch(s, &new_server);
            s.new_session_in(&new_dir, tx);
        }));
    let failed = s.kernel.slots.get(&crate::projects::HOME_STATE).servers.iter().any(|v| v.key == server && v.failed());
    // The nav button: hovered itself, its label goes to the text colour; the pointer on the
    // actions leaves it muted while the row keeps its hover surface. Unreachable servers dim and
    // disable selection (`home/projects/view.tsx`: `disabled={serverUnreachable()}` `disabled:opacity-60`).
    let nav = row()
        .key("nav")
        .role("button")
        .label(proj.name.clone())
        .grow(1.0)
        .shrink()
        .min_w(0.0)
        .h(28.0)
        .px(6.0)
        .gap(8.0)
        .center_y()
        .radius(6.0)
        .focus_inset(p)
        .hover_group()
        .child(crate::icons::badge(s, p, Some(proj), &proj.name))
        .child(text(proj.name.clone()).key("label").font(HOME).color(if active { p.text } else { p.muted }).group_color(p.text).grow(1.0).shrink().truncate().align(text_left()));
    let nav = if failed {
        nav.opacity(0.6).disabled(true)
    } else {
        nav.on_press_tx(move |s: &mut App, tx: &mut Tx| {
            tx.instant();
            crate::projects::home::toggle(s, &server, &dir);
            s.project_state.home_menu_open = false;
            super::picked(s, tx);
        })
    };
    let mut r = row()
        .key(format!("hp-{}", proj.id))
        .w(width)
        .h(28.0)
        .center_y()
        .radius(6.0)
        .hover_group()
        .hover(surface)
        .on_secondary_press(move |s: &mut App, _: rune::PointerPress, tx: &mut Tx| open_menu(s, &ctx_server, &ctx_id, tx))
        .child(nav)
        .child(if menu_open || session_view::style::hover_none() { actions } else { actions.group_opacity(0.0, 1.0) });
    if active || menu_open {
        r = r.fill(surface);
    }
    r
}

/// A small ghost-muted IconButton: 20 pt, `rounded-[4px]`, the 16 pt icon in icon-muted, the
/// overlay hovered and the pressed overlay (with icon-base) pressed or while its menu is open.
pub(crate) fn icon_button(p: &Pal, key: &str, label: &str, glyph: &'static Icon, expanded: bool) -> El {
    let b = el()
        .key(key.to_owned())
        .role("button")
        .label(label)
        .expanded(expanded)
        .size(20.0, 20.0)
        .radius(4.0)
        .center()
        .hover_motion(INSTANT, INSTANT)
        .child(icon(glyph, 16.0).color(if expanded { p.icon } else { p.icon_muted }));
    if expanded { b.fill(p.pressed) } else { b.hover(p.hover).press(p.pressed) }
}

/// `ProjectMenuItems`: New session, Import session, Edit project, Reveal in File Explorer (a
/// local server's), Clear notifications (while any of its sessions is unread), then Close.
/// Server `server`'s project `id`: the menu reads it there, and its picks act there.
fn open_menu(s: &mut App, server: &ServerKey, id: &str, tx: &mut Tx) {
    crate::servers::runtime::with_server(s, server, |s| open_project_menu(s, id, menu_key(id), tx));
}

/// `open_menu` under the element keyed `anchor`, for the server the window works against.
pub(crate) fn open_project_menu(s: &mut App, id: &str, anchor: String, tx: &mut Tx) {
    let Some(proj) = s.project(id).cloned() else { return };
    let server = crate::servers::runtime::active(s).clone();
    let unread = project_unread(s, &proj).next().is_some();
    let local = matches!(&*s.source, crate::Source::Live { server, .. } if server.is_local());
    // `ProjectMenuItems` (`home/projects/menu.tsx:28-64`): New session, View history (when
    // project navigation is on), Import session, Edit project, Reveal in ..., Clear notifications,
    // separator, Close.
    let mut menu = Menu::new(t!("command.category.project"))
        .min_width(160.0)
        .align(Align::End)
        .gap(6.0)
        .item(Item::new("new-session", t!("command.session.new")));
    if s.project_navigation() {
        menu = menu.item(Item::new("history", t!("project.browse.history")));
    }
    menu = menu
        .item(Item::new("import", t!("command.session.import")).enabled(!s.project_state.import.importing()))
        .item(Item::new("edit", t!("dialog.project.edit.title")));
    if local {
        menu = menu.item(Item::new("reveal", super::super::location_card::reveal_label()));
    }
    menu = menu.item(Item::new("clear", t!("sidebar.project.clearNotifications")).enabled(unread)).separator().item(Item::new("close", t!("common.close")));
    tx.instant();
    popup::open_drawn(s, menu, anchor, tx, move |s: &mut App, picked, tx| {
        // A New session leaves Home for the project's server; the rest act on it from here.
        if picked == Some("new-session") {
            crate::servers::runtime::switch(s, &server);
        }
        crate::servers::runtime::with_server(s, &server, |s| match picked {
            Some("new-session") => s.new_session_in(&proj.directory, tx),
            Some("import") => s.import_session(&proj.directory, tx),
            Some("edit") => s.edit_project(&proj.directory, tx),
            Some("history") => super::browse::history(s, &server, &proj, tx),
            Some("reveal") => {
                let target = super::super::location_card::reveal_target(s, &proj.directory);
                if let Err(e) = target.and_then(|t| super::super::location_card::reveal(&t)) {
                    if e == super::super::location_card::MISSING_DIR_ERR {
                        let name = super::super::project_or_dir(Some(&proj), &proj.directory);
                        crate::status_toast::fail(
                            s,
                            opencode_i18n::t!("home.project.missing.title"),
                            opencode_i18n::t!("home.project.missing.description", name = name),
                        );
                    } else {
                        crate::status_toast::fail(s, opencode_i18n::t!("common.requestFailed"), e);
                    }
                }
            }
            Some("clear") => s.clear_project_notifications(&proj, tx),
            Some("close") => s.hide_project(&proj.id, tx),
            _ => {}
        });
    });
}

/// `proj`'s unread root sessions among those this window has loaded, in its folder
/// (`notification.project.hasUnread`: the data store's sessions, not Home's index).
fn project_unread<'a>(s: &'a App, proj: &'a model::Project) -> impl Iterator<Item = &'a SessionInfo> + 'a {
    let dir = crate::projects::clean_dir(&proj.directory);
    s.sessions.iter().filter(move |i| {
        i.parent.is_none() && i.archived.is_none() && (i.unread() || crate::session_menu::unread::manual(s, crate::servers::runtime::active(s), &i.id)) && crate::projects::clean_dir(&i.directory) == dir
    })
}

impl App {
    /// A project's New session: a draft in its folder (`openProjectNewSession`).
    pub(crate) fn new_session_in(&mut self, dir: &str, tx: &mut Tx) {
        self.close_settings();
        self.project_state.home_menu_open = false;
        *self.default_dir = dir.to_owned();
        *self.opening = None;
        self.open_draft_tab();
        tx.settle();
    }

    /// Whether any of `proj`'s sessions is unread (`notification.project.hasUnread`).
    pub(crate) fn project_has_unread(&self, proj: &model::Project) -> bool {
        project_unread(self, proj).next().is_some()
    }

    /// Clear notifications: its sessions read, here and on the server.
    pub(crate) fn clear_project_notifications(&mut self, proj: &model::Project, tx: &mut Tx) {
        let ids: Vec<String> = project_unread(self, proj).map(|i| i.id.clone()).collect();
        tx.instant();
        for id in ids {
            crate::session_menu::unread::read(self, &id);
        }
    }
}

/// The focused server's name, when the picker names it in place of "All projects": under
/// Consolidated, with several servers and no project chosen (`scopedServer`, opencode#66265).
fn scoped_server(s: &App, selected: Option<&model::Project>) -> Option<String> {
    if selected.is_some() || !s.settings.navigation_prototype() {
        return None;
    }
    let servers = &s.kernel.slots.get(&crate::projects::HOME_STATE).servers;
    let key = crate::projects::home::server(s);
    (servers.len() > 1).then(|| servers.iter().find(|v| v.key == key).map(|v| v.label.clone())).flatten()
}

/// The picker's label (`label()`): the chosen project, else the scoped server, else All projects.
fn picker_label(selected: Option<&model::Project>, scoped: Option<&str>) -> String {
    selected.map(|proj| proj.name.as_str()).or(scoped).unwrap_or(t!("home.projects.all")).to_owned()
}

/// The narrow window's projects dropdown (`home-projects-dropdown`): 40 tall and full width,
/// `px-1.5 gap-2`, bg-base going to layer-01 hovered; the selected project's avatar (a small
/// folder for all of them), its name, and a small chevron in icon-muted.
pub(super) fn dropdown_trigger(s: &App, p: &Pal, selected: Option<&model::Project>) -> El {
    let open = s.project_state.home_menu_open;
    let scoped = scoped_server(s, selected);
    let lead = match selected {
        Some(proj) => crate::icons::badge(s, p, Some(proj), &proj.name),
        None => icon(if scoped.is_some() { &ic::SERVER } else { &ic::FOLDER }, 16.0).color(p.text),
    };
    let label = picker_label(selected, scoped.as_deref());
    // With several servers, the focused one's name under it, muted at 70%
    // (`leading-[var(--line-height-compact)]`); not when the first line already names it.
    let name = text(label).key("name").font(UI).color(p.text).truncate().dir(rune::Direction::Auto);
    let server = (scoped.is_none() && s.kernel.slots.get(&crate::projects::HOME_STATE).servers.len() > 1).then(|| {
        let key = crate::projects::home::server(s);
        s.kernel.slots.get(&crate::projects::HOME_STATE).servers.iter().find(|v| v.key == key).map(|v| text(v.label.clone()).key("server").font(UI).color(p.muted).opacity(0.7).truncate().dir(rune::Direction::Auto))
    });
    let labels = col().key("labels").grow(1.0).shrink().min_w(0.0).child(name).children(server.flatten());
    row()
        .key("home-projects-trigger")
        .role("button")
        .label_static(t!("home.projects"))
        .expanded(open)
        .w_full()
        .h(40.0)
        .px(6.0)
        .gap(8.0)
        .center_y()
        .radius(6.0)
        .focus_inset(p)
        .fill(if open { p.layer1 } else { p.bg_base })
        .hover(p.layer1)
        .on_layout_tx(|s: &mut App, r: [f32; 4], tx: &mut Tx| {
            if (s.project_state.trigger_w - r[2]).abs() > 0.5 {
                s.project_state.trigger_w = r[2];
            } else {
                tx.live_only();
            }
        })
        .on_press_tx(|s: &mut App, tx: &mut Tx| {
            tx.instant();
            s.project_state.home_menu_open = !s.project_state.home_menu_open;
        })
        .child(lead)
        .child(labels)
        .child(icon(&ic::CHEVRON_DOWN, 16.0).color(p.icon_muted))
}

/// Consolidated navigation's compact project picker at the end of Home's search field
/// (`HomeProjectsView`'s `scope`, `home-project-scope`): includes selected project in accessible
/// label and exposes `aria-expanded`.
pub(super) fn scope_trigger(s: &App, p: &Pal, selected: Option<&model::Project>) -> El {
    let open = s.project_state.home_menu_open;
    let scoped = scoped_server(s, selected);
    let label = picker_label(selected, scoped.as_deref());
    let lead = match selected {
        Some(proj) => Some(crate::icons::badge(s, p, Some(proj), &proj.name)),
        None => scoped.is_some().then(|| icon(&ic::SERVER, 16.0).color(p.icon_muted)),
    };
    row()
        .key("home-projects-trigger")
        .role("button")
        .label(format!("{}: {label}", t!("home.projects")))
        .expanded(open)
        .h(28.0)
        .max_w(220.0)
        .shrink()
        .min_w(0.0)
        .mr(-4.0)
        .pl(8.0)
        .pr(6.0)
        .gap(6.0)
        .center_y()
        .radius(4.0)
        .preserve_focus_on_press(true)
        .focus_inset(p)
        .fill(if open { p.pressed } else { rune::Color::TRANSPARENT })
        .hover(if open { rune::Color::TRANSPARENT } else { p.hover })
        .hover_group()
        .on_press_tx(|s: &mut App, tx: &mut Tx| {
            tx.instant();
            s.project_state.home_menu_open = !s.project_state.home_menu_open;
        })
        .children(lead)
        .child(text(label).key("name").font(UI).color(if open { p.text } else { p.muted }).group_color(p.text).truncate().dir(rune::Direction::Auto))
        .child(icon(&ic::CHEVRON_DOWN, 16.0).color(p.icon_muted))
}

/// The dropdown's popover: as wide as its trigger and 6 below it, `rounded-[10px] bg-base p-1.5`,
/// the floating elevation; All projects, the project rows, then Add project.
pub(crate) fn home_projects_popover(s: &App, p: &Pal) -> Option<[El; 2]> {
    if s.route != Route::Home || !s.project_state.home_menu_open {
        return None;
    }
    let selected_id = s.home_selected().map(|p| p.id.as_str());
    let scope = super::scoped(s);
    let width = if scope { 280.0 } else { s.project_state.trigger_w.max(160.0) };
    let server = crate::servers::runtime::active(s);
    let failed = s.kernel.slots.get(&crate::projects::HOME_STATE).servers.iter().any(|v| v.key == *server && v.failed());
    // The same `home-projects-viewport` as the column's, its bar too.
    let viewport = col().w_full().max_h(320.0).scroll_fade(rune::ScrollFade::strength(16.0).linear().full_at(1.0));
    let scroll = |viewport: El, rows: El| session_view::scroller::scroller("home-projects-menu-scroll").build::<App>(p, viewport, rows);
    let row_w = width - 12.0 - session_view::scroller::gutter("home-projects-menu-scroll");
    // Grouped by server, the rows are the whole panel (`view.tsx:236-325`): no All projects, no
    // Add project row.
    let (all, body, add) = if super::servers::grouped(s) {
        // `max-h-[min(60dvh,…)]`: the whole panel is the list.
        let window_h = rune::memo::layout_with("window", |r| r[3]).unwrap_or(800.0);
        (None, scroll(viewport.max_h(0.6 * window_h - 12.0), super::servers::groups(s, p, true, row_w)), None)
    } else {
        let mut all = nav_row(p, "hpm-all", &ic::FOLDER, t!("home.projects.all")).w_full().mb(4.0).on_press_tx(|s: &mut App, tx: &mut Tx| {
            s.clear_home_project(tx);
            super::picked(s, tx);
        });
        if selected_id.is_none() {
            all = all.fill(p.layer3);
        }
        let mut list = col().key("home-projects-options").w_full().gap(4.0);
        for proj in s.used_projects() {
            list = list.child(project_row(s, p, server, proj, selected_id == Some(proj.id.as_str()), row_w));
        }
        let add = nav_row(p, "hpm-add", &ic::FOLDER_ADD_LEFT, t!("home.project.add")).w_full().mt(4.0);
        let add = if failed {
            add.opacity(0.6).disabled(true)
        } else {
            add.on_press_tx(|s: &mut App, tx: &mut Tx| add_project(s, tx))
        };
        (Some(all), scroll(viewport, list), Some(add))
    };
    let menu = col()
        .key("home-projects-popover")
        .role("menu")
        .label_static(t!("home.projects"))
        .anchored("home-projects-trigger", Side::Bottom, if scope { Align::End } else { Align::Start }, if scope { 8.0 } else { 6.0 })
        .w(width)
        .padding(6.0)
        .radius(10.0)
        .fill(p.bg_base)
        .elevate(p.floating)
        .transition(rune::Transition::none())
        .motion(Spring::NONE)
        .on_press(|_: &mut App| {})
        .children(all)
        .child(body)
        .children(add);
    let dismiss = el().key("home-projects-dismiss").absolute(0.0, 0.0).w_full().h_full().on_press_tx(|s: &mut App, tx: &mut Tx| {
        tx.instant();
        s.project_state.home_menu_open = false;
    });
    Some([dismiss, menu])
}
