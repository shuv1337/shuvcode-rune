//! Luau plugins' panels in a session's side region. Each running plugin that adds panels is a
//! provider of its own (`App::all_panels`), keyed as Desktop keys a plugin's tabs: a panel
//! without `list` is one tab, `<plugin>:<panel>`; an instance panel's tabs are
//! `<plugin>:<instance>`, as its `list` names them, and share one mounted render.
//!
//! Its `session.panel` menu items are its entries in the `+` menu and the New tab view.

use mlua::Function;
use opencode_plugin_spec::{PanelPane, PanelSpec, PanelTab};
use rune::{El, Tx, col};
use rune_luau::Data;

use super::surfaces::render as render_surface;
use crate::App;
use crate::palette::Pal;
use crate::panels::{ClosedPanelTab, ClosedTabInfo, PanelLauncherDesc, PanelRegistration, PanelSessionCtx, PanelTabDesc, PanelViewCtx, SessionPane};
use crate::plugin_host::icons::{intern, resolve};
use crate::plugin_host::services::map;
use crate::plugin_host::{Plugin, commands, cx, dormant, epoch};

/// One panel of a plugin: its spec and the functions it handed over.
struct Panel {
    spec: PanelSpec,
    list: Option<Function>,
    render: Option<Function>,
    label: Option<Function>,
    on_focus: Option<Function>,
    on_close: Option<Function>,
    is_wide: Option<Function>,
    tab_title: Option<Function>,
    enabled: Option<Function>,
    closed_tab: Option<Function>,
    restore: Option<Function>,
}

pub(crate) type AnswerEntry = (u64, Option<Data>, Vec<cx::Dep>);

/// `f(args)` of plugin `i`, the last answer while nothing it read changed.
pub(crate) fn answer(app: &App, i: usize, key: &str, f: &Function, args: impl mlua::IntoLuaMulti) -> Option<Data> {
    use std::hash::{Hash, Hasher};
    let p = &app.plugins.list[i];
    let script = p.script()?;
    let base = |deps: &[cx::Dep]| {
        let mut h = std::hash::DefaultHasher::new();
        (p.half.surface_version(Some(script)), epoch(), cx::stamp(app, deps)).hash(&mut h);
        h.finish()
    };
    let key = format!("{}:{key}", p.half.id);
    if let Some(found) = app.plugins.panel_answers.borrow().get(&key).filter(|(v, _, deps)| *v == base(deps)).map(|(_, d, _)| d.clone()) {
        return found;
    }
    let t0 = crate::files::trace::on().then(rune::time::Instant::now);
    let (out, deps) = cx::recording(|| cx::enter(app, || script.eval_fn::<Data>(f, args).ok()));
    if let Some(t0) = t0 {
        crate::files::trace::luau(&p.half.id, "answer", &key, "", t0.elapsed().as_micros() as u64);
    }
    let v = base(&deps);
    app.plugins.panel_answers.borrow_mut().insert(key, (v, out.clone(), deps));
    out
}

/// A provider per running plugin with panels or `session.panel` menu items, ordered by its
/// panels' lowest `order`.
pub fn providers(app: &App) -> Vec<PanelRegistration> {
    app.plugins
        .listed()
        .filter_map(|(_, p)| {
            let c = p.half.contribs.borrow();
            let items = c.menu_items.iter().any(|m| m.spec.menu == opencode_plugin_spec::MenuName::SessionPanel);
            if c.panels.is_empty() && !items {
                return None;
            }
            let order = c.panels.iter().filter_map(|c| c.spec.order).min().unwrap_or(40) as i32;
            // A transient panel's tabs leave the strip once its `list` no longer names them.
            let transient = c.panels.iter().any(|c| c.spec.transient == Some(true));
            Some(PanelRegistration::new(intern(&p.manifest.id), order, pane(c.panels.first().and_then(|f| f.spec.pane)), transient))
        })
        .collect()
}

fn pane(p: Option<PanelPane>) -> SessionPane {
    match p {
        Some(PanelPane::Bottom) => SessionPane::Bottom,
        _ => SessionPane::Side,
    }
}

/// The running plugin `plugin` and its panels.
fn panels(app: &App, plugin: &str) -> Option<(usize, Vec<Panel>)> {
    let (i, p) = app.plugins.running().find(|(_, p)| p.manifest.id == plugin)?;
    let c = p.half.contribs.borrow();
    let all = c
        .panels
        .iter()
        .map(|c| Panel {
            spec: c.spec.clone(),
            list: c.f("list").cloned(),
            render: c.f("render").cloned(),
            label: c.f("label").cloned(),
            on_focus: c.f("onFocus").cloned(),
            on_close: c.f("onClose").cloned(),
            is_wide: c.f("isWide").cloned(),
            tab_title: c.f("tabTitle").cloned(),
            enabled: c.f("enabled").cloned(),
            closed_tab: c.f("closedTab").cloned(),
            restore: c.f("restore").cloned(),
        })
        .collect();
    Some((i, all))
}

/// Whether plugin `i` offers panel `p` for `session` (`PanelSpec.enabled`): its function's
/// answer for the session ([`answer`]), else its value.
fn offered(app: &App, i: usize, p: &Panel, session: &str) -> bool {
    let Some(f) = &p.enabled else { return p.spec.enabled != Some(false) };
    answer(app, i, &format!("enabled:{}:{session}", p.spec.id), f, session.to_owned()) == Some(Data::Bool(true))
}

/// The panel that draws tab `tab`: the panel of that id, else the instance panel.
fn owner(all: &[Panel], tab: &str) -> Option<usize> {
    all.iter().position(|p| p.list.is_none() && p.spec.id == tab).or_else(|| all.iter().position(|p| p.list.is_some()))
}

fn icon_of(plugin: &str, spec: &PanelSpec) -> &'static rune::Icon {
    resolve(plugin, spec.icon.as_deref().unwrap_or("window-analytics"))
}

/// The plugin's tabs for the routed session: one per single panel, and the instance panels'
/// open tabs as their `list` names them.
pub fn list(app: &App, ctx: &PanelSessionCtx) -> Vec<PanelTabDesc> {
    let Some((i, all)) = panels(app, ctx.plugin) else { return Vec::new() };
    let mut out = Vec::new();
    for p in all.iter().filter(|p| offered(app, i, p, ctx.session_id)) {
        let Some(list) = &p.list else {
            let title = match p.tab_title.as_ref().and_then(|f| answer(app, i, &format!("title:{}:{}", p.spec.id, ctx.session_id), f, ctx.session_id.to_owned())) {
                Some(Data::String(t)) => t,
                _ => p.spec.title.clone(),
            };
            let mut tab = PanelTabDesc::new(p.spec.id.clone(), title).icon(icon_of(ctx.plugin, &p.spec));
            if p.spec.transient == Some(true) {
                tab = tab.transient();
            }
            if p.spec.first == Some(true) {
                tab = tab.first();
            }
            if p.spec.fallback == Some(true) {
                tab = tab.fallback();
            }
            out.push(tab);
            continue;
        };
        let open: Vec<String> = ctx.open_ids.iter().filter(|id| !all.iter().any(|q| q.list.is_none() && q.spec.id == **id)).cloned().collect();
        if open.is_empty() {
            continue;
        }
        let tabs = listed(app, &app.plugins.list[i], list, ctx.plugin, &p.spec.id, ctx.session_id, &open);
        let group = intern(&p.spec.id);
        out.extend(tabs.into_iter().filter(|t| open.contains(&t.instance)).map(|t| instance_tab(ctx.plugin, &p.spec, group, t)));
    }
    out
}

/// An instance panel's `list` answer and what it was asked on: the script's version, the open
/// tabs, and what the call read (`cx::Dep`) as it read it.
pub(crate) struct Listed {
    asked: u64,
    reads: Vec<cx::Dep>,
    read: u64,
    tabs: Vec<PanelTab>,
}

/// `list(session, open)` of an instance panel, asked again only when the script ran, the open
/// tabs changed, or something the last call read did: the strip is built every frame, and an
/// unchanged frame enters no VM.
fn listed(app: &App, owner: &Plugin, list: &Function, plugin: &str, panel: &str, session: &str, open: &[String]) -> Vec<PanelTab> {
    let Some(script) = owner.script() else { return Vec::new() };
    let mut h = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(&(owner.half.surface_version(Some(script)), open), &mut h);
    let asked = std::hash::Hasher::finish(&h);
    let key = (plugin.to_owned(), panel.to_owned(), session.to_owned());
    let hit = app.plugins.panel_listed.borrow().get(&key).filter(|l| l.asked == asked && cx::stamp(app, &l.reads) == l.read).map(|l| l.tabs.clone());
    if let Some(tabs) = hit {
        return tabs;
    }
    let t0 = crate::files::trace::on().then(rune::time::Instant::now);
    let (answer, reads) = cx::recording(|| cx::enter(app, || script.eval_fn::<Data>(list, (session.to_owned(), open.to_vec())).ok()));
    if let Some(t0) = t0 {
        crate::files::trace::luau(plugin, "list", &format!("{panel}:{session}"), &format!("{open:?}"), t0.elapsed().as_micros() as u64);
    }
    let tabs: Vec<PanelTab> = answer.and_then(|d| d.decode().ok()).unwrap_or_default();
    let read = cx::stamp(app, &reads);
    app.plugins.panel_listed.borrow_mut().insert(key, Listed { asked, reads, read, tabs: tabs.clone() });
    tabs
}

fn instance_tab(plugin: &str, spec: &PanelSpec, group: &'static str, t: PanelTab) -> PanelTabDesc {
    let icon = match (&t.icon, &t.file) {
        (Some(name), _) => resolve(plugin, name),
        (None, Some(file)) => rune_theme_opencode::file_icons::file(opencode_kit::path::basename(file)),
        (None, None) => icon_of(plugin, spec),
    };
    let mut tab = PanelTabDesc::new(t.instance, t.title).icon(icon).group(group).missing(t.missing == Some(true));
    if let Some(file) = t.file {
        tab = tab.file(file);
    }
    if t.pinned == Some(true) {
        tab = tab.pinned();
    }
    if t.transient == Some(true) {
        tab = tab.transient();
    }
    if t.draggable == Some(false) {
        tab = tab.undraggable();
    }
    if t.fallback == Some(true) {
        tab = tab.fallback();
    }
    if t.hidden == Some(true) {
        tab = tab.hidden();
    }
    tab
}

/// What a panel's functions receive about tab `tab` of `session`.
fn props(app: &App, panel: &Panel, session: &str, tab: &str, active: bool, placement: &str, size: (f32, f32)) -> Data {
    let directory = app.session(session).map_or("", |s| s.directory.as_str()).to_owned();
    let instance = if panel.list.is_some() { Data::String(tab.to_owned()) } else { Data::Nil };
    map([
        ("sessionId", Data::String(session.to_owned())),
        ("directory", Data::String(directory)),
        ("instance", instance),
        ("active", Data::Bool(active)),
        ("placement", Data::String(placement.into())),
        ("width", Data::Integer(size.0.round() as i64)),
        ("height", Data::Integer(size.1.round() as i64)),
    ])
}

/// A tab's content.
pub fn render(app: &App, p: &Pal, ctx: &PanelViewCtx) -> El {
    let Some((i, all)) = panels(app, ctx.plugin) else { return rune::el() };
    let Some(k) = owner(&all, &ctx.tab.id) else { return rune::el() };
    let Some(f) = &all[k].render else { return rune::el() };
    let placement = if ctx.mobile { "mobile" } else { "side" };
    struct RestoreScale<'a>(&'a crate::plugin_host::PanelScale, f32);
    impl Drop for RestoreScale<'_> {
        fn drop(&mut self) {
            self.0.replace(self.1);
        }
    }
    let _restore_scale = RestoreScale(&app.plugins.panel_scale, app.plugins.panel_scale.replace(ctx.env_scale));
    let mut data = props(app, &all[k], ctx.session_id, &ctx.tab.id, ctx.visible, placement, ctx.pane_size);
    // The nested view it opened in the More drawer (`Layout:OpenDrawerView`).
    let more = &app.layout.narrow.more;
    if let (true, Some(nested), Data::Map(fields)) = (ctx.mobile, more.nested(), &mut data)
        && more.drawn_view() == Some(crate::panels::panel_key(ctx.plugin, &all[k].spec.id).as_str())
    {
        fields.insert("drawer".to_owned(), crate::plugin_host::services::json_to_data(&nested.data));
    }
    let instance = if all[k].list.is_some() { ctx.tab.id.as_str() } else { "" };
    let suffix = if ctx.mobile { ":mobile" } else { "" };
    let spec = &all[k].spec;
    #[cfg(not(target_arch = "wasm32"))]
    let track_surface = (ctx.visible && !ctx.mobile).then(|| {
        (
            app.plugins.renders.get(),
            crate::surfaces::browser::poll::begin_panel_surface_render(),
        )
    });
    let body = render_surface(app, p, i, &format!("panel:{}:{}:{instance}{suffix}", spec.id, ctx.session_id), f, data);
    #[cfg(not(target_arch = "wasm32"))]
    if let Some((renders_before, guard)) = track_surface {
        guard.finish(
            app,
            app.panel_root(ctx.session_id).to_owned(),
            crate::panels::panel_key(ctx.plugin, &ctx.tab.id),
            app.plugins.renders.get() != renders_before,
        );
    }
    let key = if instance.is_empty() { format!("plugin-{}-{}", ctx.plugin, spec.id) } else { format!("plugin-{}-{}-{instance}", ctx.plugin, spec.id) };
    #[cfg(not(target_arch = "wasm32"))]
    let review_bar = if ctx.plugin == crate::surfaces::browser::PLUGIN
        && app.plugins.is_builtin_plugin(ctx.plugin)
        && ctx.visible
    {
        instance
            .parse::<u64>()
            .ok()
            .and_then(|tab_id| crate::browser::prompts::app_link_review_bar(app, p, tab_id))
    } else {
        None
    };
    #[cfg(target_arch = "wasm32")]
    let review_bar: Option<El> = None;
    col().key(key).w_full().h_full().children(review_bar).child(body.grow(1.0).min_h(0.0).clip_soft(0.0))
}

/// A tab's own label in the strip (`PanelSpec.label`).
pub fn tab_lead(app: &App, p: &Pal, plugin: &str, tab: &PanelTabDesc, selected: bool) -> Option<El> {
    let (i, all) = panels(app, plugin)?;
    let k = owner(&all, &tab.id)?;
    let label = all[k].label.as_ref()?;
    let session = app.active_session_id().unwrap_or_default();
    let instance = if all[k].list.is_some() { Data::String(tab.id.clone()) } else { Data::Nil };
    let data = map([("sessionId", Data::String(session.to_owned())), ("instance", instance), ("active", Data::Bool(selected)), ("preview", Data::Bool(false))]);
    Some(render_surface(app, p, i, &format!("label:{}:{session}:{}", all[k].spec.id, tab.id), label, data))
}

/// Version stamp of `tab`'s custom strip label (`None` when the tab draws the standard icon + title).
pub fn tab_lead_stamp(app: &App, p: &Pal, plugin: &str, tab: &PanelTabDesc, selected: bool) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let (i, all) = panels(app, plugin)?;
    let k = owner(&all, &tab.id)?;
    all[k].label.as_ref()?;
    let plug = &app.plugins.list[i];
    let script = plug.script()?;
    let session = app.active_session_id().unwrap_or_default();
    let surface = format!("label:{}:{session}:{}", all[k].spec.id, tab.id);
    let reads = plug.half.reads.borrow().get(&surface).cloned();
    // Until the label has rendered once and recorded its reads, force a build.
    let Some(reads) = reads else {
        return Some(app.plugins.renders.get().wrapping_add(1) | (1u64 << 63));
    };
    let mut h = std::hash::DefaultHasher::new();
    (plug.half.surface_version(Some(script)), epoch(), p.theme, p.generation, p.dark, cx::stamp(app, &reads), session, &tab.id, selected).hash(&mut h);
    Some(h.finish())
}

/// The plugin's `+` and New tab entries: each single panel that wants a launcher, then its
/// `session.panel` menu items (`menu:<key>`).
pub fn launchers(app: &App, ctx: &PanelSessionCtx) -> Vec<PanelLauncherDesc> {
    let Some((i, all)) = panels(app, ctx.plugin) else { return asleep_launchers(app, ctx) };
    let manifest = &app.plugins.list[i].manifest;
    let own = all.iter().filter(|p| p.list.is_none() && p.spec.launcher != Some(false) && offered(app, i, p, ctx.session_id)).map(|p| {
        let description = p.spec.description.clone().or_else(|| manifest.description.clone());
        PanelLauncherDesc::new(intern(&p.spec.id), p.spec.title.clone(), icon_of(ctx.plugin, &p.spec), p.spec.order.unwrap_or(0) as i32)
            .description(description)
            .opened(ctx.open_ids.contains(&p.spec.id))
    });
    let mut out: Vec<PanelLauncherDesc> = own.collect();
    out.extend(commands::panel_menu_items(app, i, ctx.session_id));
    out
}

/// [`launchers`] of a dormant plugin, from its record: its single panels and `session.panel` items,
/// but for those that answer `enabled` or `when`, which wait for it to start (in idle time).
fn asleep_launchers(app: &App, ctx: &PanelSessionCtx) -> Vec<PanelLauncherDesc> {
    let Some((i, p)) = app.plugins.listed().find(|(_, p)| p.manifest.id == ctx.plugin && !p.running()) else { return Vec::new() };
    let id = p.manifest.id.as_str();
    let fns = |key: &str, name: &str| dormant::recorded_fn(app, id, "panels", key, name);
    let c = p.half.contribs.borrow();
    if c.panels.iter().any(|q| fns(q.key, "enabled")) || c.menu_items.iter().any(|m| dormant::recorded_fn(app, id, "menu_items", m.key, "when")) {
        dormant::want_now(app, id);
    }
    let mut out: Vec<PanelLauncherDesc> = c
        .panels
        .iter()
        .filter(|q| !fns(q.key, "list") && !fns(q.key, "enabled") && q.spec.launcher != Some(false) && q.spec.enabled != Some(false))
        .map(|q| {
            let description = q.spec.description.clone().or_else(|| p.manifest.description.clone());
            PanelLauncherDesc::new(intern(&q.spec.id), q.spec.title.clone(), icon_of(id, &q.spec), q.spec.order.unwrap_or(0) as i32).description(description).opened(ctx.open_ids.contains(&q.spec.id))
        })
        .collect();
    drop(c);
    out.extend(commands::panel_menu_items(app, i, ctx.session_id));
    out
}

/// Opens entry `id` of plugin `plugin`: runs its `session.panel` item `id` (whose row keys as
/// Desktop's launcher does, `side-launch-<plugin>-<id>`), else opens panel `id`'s tab in `pane`.
pub fn open_entry(app: &mut App, plugin: &str, id: &str, pane: SessionPane, tx: &mut Tx) {
    dormant::wake(app, plugin, dormant::Why::Use);
    let session = app.active_session_id().unwrap_or_default().to_owned();
    if !commands::run_menu_item(app, &format!("{plugin}.{id}"), &session, tx) {
        app.open_side_tab(&crate::panels::panel_key(plugin, id), Some(pane), false, false, tx);
    }
}

/// The pointer came onto entry `id` of plugin `plugin`: its `session.panel` item's `hover` runs
/// with the session.
pub fn hover_entry(app: &mut App, plugin: &str, id: &str, tx: &mut Tx) {
    let session = app.active_session_id().unwrap_or_default().to_owned();
    commands::hover_menu_item(app, &format!("{plugin}.{id}"), &session, tx);
}

/// Tab `tab` of `session` became selected, or the side region opened onto it: its panel's
/// `onFocus` runs at the next poll.
pub fn focused(app: &mut App, plugin: &str, session: &str, tab: &str, restored: bool) {
    let Some((i, all)) = panels(app, plugin) else { return };
    let Some(k) = owner(&all, tab) else { return };
    let Some(f) = &all[k].on_focus else { return };
    let mut data = props(app, &all[k], session, tab, true, "side", (0.0, 0.0));
    if let Data::Map(m) = &mut data {
        m.insert("restored".into(), Data::Bool(restored || app.kernel.slots.get(&crate::panels::SIDE).background_open.is_active()));
    }
    if let Some(script) = app.plugins.list[i].script() {
        cx::enter(app, || script.spawn_fn(f, data, None).ok());
    }
    app.plugins.asked.set(true);
}

/// Tab `tab` left `session`'s strip: its panel's `onClose` runs.
pub fn closed(app: &mut App, plugin: &str, session: &str, tab: &str, tx: &mut Tx) {
    #[cfg(not(target_arch = "wasm32"))]
    if plugin == crate::surfaces::browser::PLUGIN
        && let Ok(id) = tab.parse::<u64>()
    {
        app.workspace_panes.browser.closing_tabs.insert(id);
    }
    let Some((i, all)) = panels(app, plugin) else { return };
    let Some(k) = owner(&all, tab) else { return };
    let Some(f) = all[k].on_close.clone() else { return };
    let data = props(app, &all[k], session, tab, false, "side", (0.0, 0.0));
    commands::spawn(app, i, &f, data, tx);
}

/// What Recently closed keeps of `tab`: what its panel's `closedTab` answers (nothing for nil),
/// else its panel's `closed`, else the tab's own title.
pub fn closed_info(app: &App, plugin: &str, session: &str, tab: &PanelTabDesc) -> ClosedTabInfo {
    let Some((i, all)) = panels(app, plugin) else { return ClosedTabInfo::Default };
    let Some(k) = owner(&all, &tab.id) else { return ClosedTabInfo::Default };
    let closed = match &all[k].closed_tab {
        Some(f) => {
            let props = props(app, &all[k], session, &tab.id, false, "side", (0.0, 0.0));
            let answer = app.plugins.list[i].script().and_then(|s| cx::enter(app, || s.eval_fn::<Data>(f, props).ok()));
            match answer.and_then(|d| d.decode::<opencode_plugin_spec::ClosedTab>().ok()) {
                Some(c) => c,
                None => return ClosedTabInfo::Deny,
            }
        }
        None => match all[k].spec.closed.clone() {
            Some(c) => c,
            None => return ClosedTabInfo::Default,
        },
    };
    let icon_name = closed.icon.map(|name| {
        if !name.contains(':') && crate::plugin_host::icons::has_icon(plugin, &name) {
            format!("{plugin}:{name}")
        } else {
            name
        }
    });
    ClosedTabInfo::Allow(ClosedPanelTab { title: closed.title, icon_name, detail: closed.detail, data: closed.data })
}

/// Brings `tab` of `session` back from Recently closed with its panel's `restore`; false when
/// the panel has none, so its key opens again.
pub fn restore(app: &mut App, plugin: &str, session: &str, tab: &str, data: Option<&str>, tx: &mut Tx) -> bool {
    let Some((i, all)) = panels(app, plugin) else { return false };
    let Some(k) = owner(&all, tab) else { return false };
    let Some(f) = all[k].restore.clone() else { return false };
    let props = props(app, &all[k], session, tab, false, "side", (0.0, 0.0));
    commands::spawn(app, i, &f, (props, data.map(str::to_owned)), tx);
    true
}

/// Whether one of the plugin's panels asks for the wider session minimum now.
pub fn wide(app: &App, plugin: &str) -> bool {
    let Some((i, all)) = panels(app, plugin) else { return false };
    all.iter().any(|p| match &p.is_wide {
        Some(f) => answer(app, i, &format!("wide:{}", p.spec.id), f, ()) == Some(Data::Bool(true)),
        None => p.spec.wide == Some(true),
    })
}

/// The pane the plugin's first panel names for a new tab; `dock` asks Settings › Terminal.
pub fn home_pane(app: &App, plugin: &str) -> Option<SessionPane> {
    let (_, all) = panels(app, plugin)?;
    Some(match all.first()?.spec.pane {
        Some(PanelPane::Dock) => match app.dock_placement() {
            crate::panels::DockPlacement::Side => SessionPane::Side,
            crate::panels::DockPlacement::Bottom => SessionPane::Bottom,
        },
        other => pane(other),
    })
}

/// The narrow-window views the plugins' panels offer the routed session (`PanelSpec.mobile`),
/// each keyed by its panel's tab key, `<plugin>:<panel>`.
pub fn narrow_views(app: &App) -> Vec<(String, opencode_plugin_spec::PanelMobileView)> {
    let session = app.active_session_id().unwrap_or_default();
    let ids: Vec<String> = app.plugins.running().map(|(_, p)| p.manifest.id.clone()).collect();
    ids.iter()
        .filter_map(|id| panels(app, id).map(|(i, all)| (id, i, all)))
        .flat_map(|(id, i, all)| all.into_iter().filter(|p| offered(app, i, p, session)).filter_map(|p| Some((crate::panels::panel_key(id, &p.spec.id), p.spec.mobile?))).collect::<Vec<_>>())
        .collect()
}

/// The tab key of plugin `plugin`'s panel `panel`, or of its instance `instance`.
pub fn tab_key(plugin: &str, panel: &str, instance: Option<&str>) -> String {
    match instance {
        Some(i) if !i.is_empty() => crate::panels::panel_key(plugin, i),
        _ => crate::panels::panel_key(plugin, panel),
    }
}
