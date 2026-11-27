//! `PluginHost`: plugins as `--!strict` Luau source and a `plugin.toml` against the
//! engine-agnostic contract (`opencode-plugin-spec`), run on `rune-luau`, one VM per plugin half.
//!
//! - **Two hosts.** A window's [`PluginHost`] (`WindowPluginHost`) runs every plugin's window
//!   half, `init.luau`; the process's `AppPluginHost` ([`app_half`]) runs each `app.luau` once,
//!   polled once per tick whatever the number of windows.
//! - **Loading** ([`runtime::boot`], [`sources`]): the built-ins are compiled into the binary (a
//!   debug build runs them from the repository, so saving one reloads it); `--plugin-dir`,
//!   overrides and installed plugins come from their folders, and one from outside the app waits
//!   for a one-time OK ([`trust`]) keyed by its files' SHA-256.
//! - **Lifecycle** ([`runtime::lifecycle`]): `drive`, `reload`, `poll`, `toggle`, and `step_trust`.
//! - **Contributions** reach the app's surfaces ([`surfaces`], [`contrib`], [`panels`],
//!   [`commands`], [`ui::view`]).
//! - **Rendering** ([`surfaces`]): each contribution's render is memoized on the script's
//!   version, its props, the theme and a stamp of what it read last time ([`cx::Dep`]); an
//!   unchanged frame enters no VM.
//! - **Host-kept state** ([`bridge::shared`]): each plugin's stores, what the app knows that
//!   plugins ask about, and their images; processes ([`process`]) and background work ([`tasks`])
//!   off the app's thread; icons and strings interned for Rune's `&'static` data ([`icons`]).

pub(crate) mod bridge;
pub mod native;
pub(crate) mod runtime;
pub mod services;
pub(crate) mod ui;

pub use opencode_plugin_runtime::{catalogs, flyout, icons, images, machines, process, tasks};

pub use bridge::{commands, console, cx, project_request, requests, server_sources, servers, stores};
pub(crate) use bridge::contrib;
pub use runtime::{app_half, sources, trust};
#[cfg(not(target_arch = "wasm32"))]
pub use runtime::types;
pub(crate) use runtime::{dormant, memory, plugin, vm};
pub use ui::{panels, surfaces};
pub(crate) use ui::editors;

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::rc::Rc;

use opencode_plugin_spec::Manifest;
use rune::{KeyEvent, Tx};
use rune_luau::{Data, Script};

pub use catalogs::Catalogs;
pub use commands::{commands, key, run_command, run_slash, slash_commands};
pub use contrib::{
    code_block_actions, mcp_call, mcp_tools, mcp_tools_generation, menubar_items, open_link, run_menu_item,
    run_menubar, server_menu_items, settings_search,
};
pub use flyout::{Flyout, FlyoutEvent, Overlay};
pub use machines::{Hot, HotEffect, Life, LifeEffect, LifeEvent, Trust, TrustEffect, TrustEvent};
pub use plugin::Half;
pub use runtime::boot::{boot_builtin, doze_owed, ensure_plugin_running, first_frame, setup};
pub use runtime::lifecycle::{drive, fire_named, poll, reload, step_trust, toggle};
pub use servers::{
    ServerRec, all as contributed_servers, authenticating as server_authenticating, changed as servers_changed,
    connect as connect_server, cover as server_cover, row as server_row, stage as servers_stage,
};
pub use sources::{Origin, Tier};
pub use surfaces::{
    dialog_card, dialog_view_in, file_viewer, has_live_settings_section, pill_views, popover_view, settings_page,
    settings_page_actions, settings_page_icon, settings_page_ids, settings_page_key, settings_page_title,
    settings_pages, sidebar_items, slot, slot_with, titlebar_items,
};
pub use trust::dialog as trust_dialog;
pub use ui::view::settings_section;

use crate::App;

thread_local! {
    static EPOCH: Cell<u64> = const { Cell::new(0) };
}

/// Notes that something every plugin surface may show changed outside a render.
pub fn bump() {
    EPOCH.set(EPOCH.get().wrapping_add(1));
}

pub fn epoch() -> u64 {
    EPOCH.get()
}

/// What every plugin surface shows outside `Tracked`: the epoch, and each plugin's script and
/// half versions, as the surfaces' own memos key on them (`surfaces::render`). A region showing
/// plugin surfaces takes it as an input, so a plugin that changed what it draws between polls
/// builds that region again.
pub fn surfaces_version(app: &App) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::hash::DefaultHasher::new();
    epoch().hash(&mut h);
    crate::updater::revision(app).hash(&mut h);
    app.plugins.titlebar_hover.hash(&mut h);
    app.plugins.titlebar_drops.hash(&mut h);
    app.plugins.titlebar_focus.hash(&mut h);
    for p in app.plugins.list.iter() {
        p.half.surface_version(p.script()).hash(&mut h);
    }
    h.finish()
}

/// One plugin in this window.
pub struct Plugin {
    pub manifest: Manifest,
    pub origin: Origin,
    pub tier: Tier,
    pub life: Life,
    pub trust: Trust,
    pub hot: Hot,
    pub half: Rc<Half>,
}

impl Plugin {
    pub fn running(&self) -> bool {
        self.life.running()
    }

    /// The window half's script, while it has one.
    pub fn script(&self) -> Option<&Script> {
        self.life.script()
    }
}

/// The plugin dialog on screen.
pub struct OpenDialog {
    pub plugin: Rc<str>,
    pub id: String,
    pub data: Data,
}

/// A plugin's numbers for `Diagnostics:Metrics` and the console.
pub struct PluginMetrics {
    pub memory_kb: f64,
    pub render_us: f64,
    pub memo_hit_rate: f64,
}

/// The pane's display scale while a panel renders (`PanelViewCtx.env_scale`), defaulting to `1.0`.
pub(crate) struct PanelScale(Cell<f32>);

impl Default for PanelScale {
    fn default() -> Self {
        Self(Cell::new(1.0))
    }
}

impl PanelScale {
    pub(crate) fn get(&self) -> f32 {
        self.0.get()
    }

    pub(crate) fn replace(&self, scale: f32) -> f32 {
        self.0.replace(scale)
    }
}

/// Model-picker inline annotations (`"{providerId}:{modelId}" -> (label, critical)`) from one plugin.
pub(crate) type ModelBadgeMap = HashMap<String, (String, bool)>;

/// The window's plugin host (`WindowPluginHost`).
#[derive(Default)]
pub struct PluginHost {
    pub list: Vec<Plugin>,
    /// The plugins turned off in Settings › Plugins.
    disabled: BTreeSet<String>,
    /// Plugins from outside the app the user OKed: plugin id → the SHA-256 of their files.
    trusted: BTreeMap<String, String>,
    shared: bridge::shared::Shared,
    /// Contributed servers whose health check is on its way.
    checking: HashSet<String>,
    /// The window's page visits, for server covers' one sign-in offer per visit.
    pub cover_visit: servers::CoverVisit,
    /// Where booting this window's plugins stands (unstarted, or started with folder scan,
    /// deferred built-ins, and after-frame doze latch).
    boot: runtime::boot::Boot,
    /// The built-ins waiting for their first use, and their records.
    pub dormant: dormant::Dormant,
    /// Plugins whose start ran out of time, waiting for their one restart.
    pub(crate) restarts: runtime::lifecycle::Restarts,
    /// Per-plugin model-picker row annotations (`Models:SetBadges`), keyed by plugin id and tagged
    /// with the VM id that set them so reload/stop drops stale entries.
    pub(crate) model_badges: BTreeMap<String, (u64, ModelBadgeMap)>,
    pub overlay: Overlay,
    pub dialog: Option<OpenDialog>,
    /// The titlebar item the pointer is over, whose pill opens to show its label.
    pub titlebar_hover: Option<&'static str>,
    /// How many times each titlebar item's arrow has dropped in, which keys the arrow it shows.
    pub titlebar_drops: BTreeMap<&'static str, u32>,
    /// The titlebar item holding keyboard focus, whose pill opens to show its label (`focus-within`).
    pub titlebar_focus: Option<String>,
    /// The window lost the system's focus (`ctx.FocusChanged`); a new window has it.
    pub window_blurred: Cell<bool>,
    pub editors: editors::Editors,
    /// The window's waker, for process output and finished work.
    pub waker: Option<rune::Waker>,
    /// The appearance the last surface rendered in.
    pub dark: Cell<bool>,
    /// The pointer's heading over a popover's flyout rows, and where their open list is.
    pub aim: flyout::Aim,
    /// The debug bar's FOCUS: every control draws its keyboard focus ring.
    pub force_focus: bool,
    /// A UI entry without a transaction asked for something: the next poll carries it out.
    pub asked: Cell<bool>,
    /// Actions plugins queued for this window during a VM entry, carried out by `requests::carry_out`.
    pub(crate) pending_requests: RefCell<Vec<(Rc<str>, cx::Request)>>,
    /// Per-window memo cache for reactive command flags (`cx::answer`).
    pub(crate) command_answers: RefCell<HashMap<String, cx::Answer>>,
    /// Per-window memo cache for panel listing callbacks (`panels::answer`).
    pub(crate) panel_answers: RefCell<HashMap<String, panels::AnswerEntry>>,
    /// Per-window memo cache for instance panel tab lists (`panels::listed`).
    pub(crate) panel_listed: RefCell<HashMap<(String, String, String), panels::Listed>>,
    /// Per-window cache of each session's last step with token usage (`services::session`).
    pub(crate) last_usage: RefCell<HashMap<String, services::CachedUsage>>,
    /// Display scale of the panel currently rendering (`ui.terminalSurface`, `ui.browserSurface`).
    pub(crate) panel_scale: PanelScale,
    /// In-flight MCP tool calls owned by this window's plugin VMs, with their deadlines.
    pub(crate) mcp_calls: RefCell<HashMap<u64, (opencode_kit::client_mcp::McpCall, rune::time::Instant)>>,
    /// Next MCP tool call id for this window.
    pub(crate) next_mcp_call: Cell<u64>,
    /// Server events plugins listen for, waiting for the next poll.
    events: RefCell<Vec<(String, serde_json::Value)>>,
    watcher: Option<runtime::hot::Watcher>,
    /// Surface renders that ran a script, and those a memo answered, for the metrics.
    pub renders: Cell<u64>,
    pub hits: Cell<u64>,
    /// A plugin's `Layout:OpenProject` waiting for its server, and the last request's attempt.
    pub project: project_request::State,
    pub project_attempt: rune::machine::Attempt,
    /// The poll clock of the last poll.
    clock: Cell<f64>,
    /// The epoch the registries read off the app's state were last mirrored at.
    synced: Cell<u64>,
    /// `--luau-metrics`: each plugin's (VM calls, renders) and the surfaces' (hits, misses) at the mark.
    baseline: RefCell<Vec<(String, (u64, u64))>>,
    mark_hits: Cell<(u64, u64)>,
}

impl PluginHost {
    /// Whether a plugin dialog is open.
    pub fn dialog_open(&self) -> bool {
        self.dialog.is_some()
    }

    /// The short inline annotation any plugin attached to model `key` (`"{providerId}:{modelId}"`).
    /// Prefers a critical badge when multiple plugins annotate the same model.
    pub fn model_badge(&self, key: &str) -> Option<(String, bool)> {
        let mut fallback: Option<&(String, bool)> = None;
        for (_, badges) in self.model_badges.values() {
            if let Some(entry) = badges.get(key) {
                if entry.1 {
                    return Some(entry.clone());
                }
                if fallback.is_none() {
                    fallback = Some(entry);
                }
            }
        }
        fallback.cloned()
    }

    /// Replaces `plugin`'s inline model-picker badges (an empty map clears them).
    pub fn set_model_badges(&mut self, plugin: &str, vm: u64, badges: HashMap<String, (String, bool)>) {
        if badges.is_empty() {
            self.model_badges.remove(plugin);
        } else {
            self.model_badges.insert(plugin.to_owned(), (vm, badges));
        }
    }

    /// Drops `plugin`'s model badges unless they were written by `keep_vm`.
    pub fn clear_model_badges(&mut self, plugin: &str, keep_vm: Option<u64>) {
        if self.model_badges.get(plugin).is_some_and(|(vm, _)| Some(*vm) != keep_vm) {
            self.model_badges.remove(plugin);
        }
    }

    pub fn half(&self, id: &str) -> Option<&Rc<Half>> {
        self.list.iter().find(|p| &*p.half.id == id).map(|p| &p.half)
    }

    pub fn find(&self, id: &str) -> Option<usize> {
        self.list.iter().position(|p| p.manifest.id == id)
    }

    /// Whether `id` is a live, genuinely bundled built-in plugin (`Tier::Bundled`, never a user,
    /// override, or dev plugin shadowing a built-in ID).
    pub fn is_builtin_plugin(&self, id: &str) -> bool {
        self.list.iter().any(|p| p.half.is_builtin(id) && p.running())
    }

    /// Whether the user explicitly disabled plugin `id`.
    pub fn is_disabled(&self, id: &str) -> bool {
        self.disabled.contains(id)
    }

    /// Whether plugin `id` is still queued in deferred post-first-frame boot (`boot.later`).
    pub fn is_deferred(&self, id: &str) -> bool {
        self.boot.later().iter().any(|f| f.manifest.id == id)
    }

    /// Whether plugin `id` is running, dormant (ready to wake on first use), or a bundled
    /// built-in queued for deferred boot and not disabled by the user.
    pub fn plugin_available(&self, id: &str) -> bool {
        if let Some(p) = self.list.iter().find(|p| p.manifest.id == id) {
            return p.life.listed();
        }
        !self.disabled.contains(id)
            && self
                .boot
                .later()
                .iter()
                .any(|f| f.manifest.id == id && f.tier == Tier::Bundled && f.error.is_none())
    }

    /// The running plugins.
    pub fn running(&self) -> impl Iterator<Item = (usize, &Plugin)> {
        self.list.iter().enumerate().filter(|(_, p)| p.running())
    }

    /// The plugins whose contributions are listed: the running ones, and the dormant ones from their
    /// records (`dormant`), whose functions are none until they start.
    pub fn listed(&self) -> impl Iterator<Item = (usize, &Plugin)> {
        self.list.iter().enumerate().filter(|(_, p)| p.life.listed())
    }

    pub fn metrics(&self, id: &str) -> Option<PluginMetrics> {
        let script = self.list.iter().find(|p| p.manifest.id == id)?.script()?;
        let m = script.metrics();
        let calls = m.renders.max(1) as f64;
        Some(PluginMetrics {
            memory_kb: script.memory() as f64 / 1024.0,
            render_us: m.vm_time.as_secs_f64() * 1e6 / m.vm_calls.max(1) as f64,
            memo_hit_rate: self.hits.get() as f64 / (self.hits.get() as f64 + calls),
        })
    }
}

/// The composer picker `kind` is opening: wakes or adopts any built-in listening to
/// `Composer.PickerChanged` and fires it synchronously in this same turn so plugin-provided
/// picker annotations and refreshes run with zero frame delay.
pub fn notify_picker_opening(app: &mut App, kind: crate::pickers::Kind, tx: &mut Tx) {
    notify_picker_inner(app, kind, false, tx);
}

/// Completes picker notification after deferred boot finishes when a picker opened before
/// stage B storage was ready: fires only on plugins that just started, avoiding duplicate
/// delivery to plugins already notified by [`notify_picker_opening`].
pub(crate) fn notify_picker_after_boot(app: &mut App, kind: crate::pickers::Kind, tx: &mut Tx) {
    notify_picker_inner(app, kind, true, tx);
}

fn notify_picker_inner(app: &mut App, kind: crate::pickers::Kind, only_new: bool, tx: &mut Tx) {
    use std::hash::{Hash, Hasher};
    const SIGNAL: &str = "Composer.PickerChanged";
    let already_running: HashSet<String> = if only_new {
        app.plugins.running().map(|(_, p)| p.manifest.id.clone()).collect()
    } else {
        HashSet::new()
    };
    runtime::boot::doze_owed(app);
    runtime::boot::adopt_later_signal(app, SIGNAL);
    dormant::wake_signal(app, SIGNAL);
    let key = kind.key();
    let payload = Data::String(key.to_owned());
    let mut h = std::hash::DefaultHasher::new();
    Some(key).hash(&mut h);
    let seen_v = h.finish();
    let mut fired = false;
    for i in 0..app.plugins.list.len() {
        let Some(vm) = app.plugins.list[i].script().and_then(Script::vm_id) else { continue };
        if only_new && already_running.contains(&app.plugins.list[i].manifest.id) {
            continue;
        }
        let has_signal = app.plugins.list[i].half.signals.borrow().contains_key(&(vm, SIGNAL.to_owned()));
        if !has_signal {
            continue;
        }
        app.plugins.list[i].half.seen.borrow_mut().insert(SIGNAL.to_owned(), seen_v);
        fired |= fire_named(app, i, SIGNAL, payload.clone());
    }
    if fired {
        requests::carry_out(app, tx);
    }
}

/// Queues a server event for the plugins listening for its type, delivered on the next poll.
pub fn server_event(app: &mut App, ty: &str, data: &serde_json::Value) {
    if dormant::listens(app, ty) {
        dormant::buffer_event(app, ty, data);
    }
    let host = &app.plugins;
    let wanted = host
        .running()
        .any(|(_, p)| p.half.events.borrow().iter().any(|(_, prefix, _)| ty.starts_with(prefix.as_str())));
    if wanted {
        host.events.borrow_mut().push((ty.to_owned(), data.clone()));
    }
}

/// Every plugin field gives up the keyboard (the side region switched tabs).
pub fn blur_fields(app: &mut App) {
    for edit in app.plugins.editors.get_mut().values_mut() {
        edit.focused = false;
    }
}

/// Whether a plugin's field has the keyboard.
pub fn field_focused(app: &App) -> bool {
    editors::any_focused(app)
}

/// A key a plugin's field hands over (`UiFieldProps.keys`): its editor's `KeyPressed` fires with
/// the key's name.
pub fn key_pressed(app: &mut App, plugin: &str, index: usize, key: &str, tx: &mut Tx) {
    if let Some(p) = app.plugins.list.iter().find(|p| p.manifest.id == plugin) {
        services::fire(app, p, &format!("editor.{index}.KeyPressed"), |lua| {
            mlua::IntoLuaMulti::into_lua_multi(key.to_owned(), lua)
        });
    }
    requests::carry_out(app, tx);
}

/// Enter in a plugin's field: its editor's `Submitted` fires with the text.
pub fn submitted(app: &mut App, plugin: &str, index: usize, tx: &mut Tx) {
    let text = editors::text_in(app, plugin, index);
    if let Some(p) = app.plugins.list.iter().find(|p| p.manifest.id == plugin) {
        services::fire(app, p, &format!("editor.{index}.Submitted"), |lua| {
            mlua::IntoLuaMulti::into_lua_multi(text.clone(), lua)
        });
    }
    requests::carry_out(app, tx);
}

/// Text changed in a plugin's bare field: fires `editor.{index}.Changed` synchronously so
/// address-bar inline completion and suggestion rows update in the same frame as the keystroke,
/// then syncs the editor & address-selection watch stamps against the post-handler state.
pub fn input_changed(app: &mut App, plugin: &str, index: usize, tx: &mut Tx) {
    let text = editors::text_in(app, plugin, index);
    if let Some(p) = app.plugins.list.iter().find(|p| p.manifest.id == plugin) {
        let name = format!("editor.{index}.Changed");
        services::fire(app, p, &name, |lua| {
            mlua::IntoLuaMulti::into_lua_multi(text.clone(), lua)
        });
        services::sync_editor_watches(app, p, index);
    }
    requests::carry_out(app, tx);
}

/// `--plugin-index`: each plugin, where it runs from, its state, and what it contributed.
pub fn print_index(app: &App) {
    for p in &app.plugins.list {
        eprintln!(
            "plugin index: plugin: {} {} (contract {}, {}, {}) from {}",
            p.manifest.id,
            p.manifest.version,
            p.manifest.contract,
            p.tier.label(),
            p.life.label(),
            p.origin.describe()
        );
        if let Some(e) = p.life.why() {
            eprintln!("plugin index: error: {}: {e}", p.manifest.id);
        }
        let c = p.half.contribs.borrow();
        for x in &c.panels {
            eprintln!(
                "plugin index: panel: {} {}{}",
                x.key,
                x.spec.title,
                if x.spec.launcher == Some(false) { ", no launcher" } else { "" }
            );
        }
        for x in &c.commands {
            eprintln!(
                "plugin index: command: {} {} [{}] section {:?}{}",
                x.key,
                x.spec.title,
                x.spec.bind.as_deref().unwrap_or_default(),
                x.spec.section,
                if x.spec.hidden == Some(true) { ", hidden" } else { "" }
            );
        }
        for x in &c.slots {
            eprintln!("plugin index: slot: {} {} (order {})", x.key, x.spec.at, x.spec.order.unwrap_or(0));
        }
        for x in &c.popovers {
            eprintln!("plugin index: popover: {}", x.key);
        }
        for x in &c.settings {
            eprintln!("plugin index: setting: {} {}", x.key, x.spec.title);
        }
    }
}

/// `--luau-metrics`: as the window goes, each plugin's VM entries, renders, memo hits, rows,
/// time in the VM and heap, in all and since the first poll at or after `--luau-metrics-from
/// <seconds>` (the idle window a proof measures).
impl Drop for PluginHost {
    fn drop(&mut self) {
        dormant::close(self);
        if !crate::flag("--luau-metrics") {
            return;
        }
        for p in &self.list {
            let Some(s) = p.script() else { continue };
            let m = s.metrics();
            let base = self.baseline.borrow().iter().find(|(id, _)| *id == p.manifest.id).map(|(_, b)| *b);
            let (calls, renders) = base.map_or((m.vm_calls, m.renders), |b| (m.vm_calls - b.0, m.renders - b.1));
            eprintln!(
                "luau metrics: {} vm_calls {} renders {} rows {} vm_time_us {:.0} heap_kb {:.0} reloads {} | since mark: vm_calls {calls} renders {renders}",
                p.manifest.id,
                m.vm_calls,
                m.renders,
                m.rows,
                m.vm_time.as_secs_f64() * 1e6,
                s.memory() as f64 / 1024.0,
                m.reloads,
            );
        }
        let (hits, misses) = (self.hits.get(), self.renders.get());
        let mark = self.mark_hits.get();
        eprintln!(
            "luau metrics: surfaces memo hits {hits} misses {misses} | since mark: hits {} misses {}",
            hits - mark.0,
            misses - mark.1
        );
    }
}

/// Notes the counters at the mark `--luau-metrics-from` names, once.
pub(super) fn mark(host: &PluginHost, now: f64) {
    if !host.baseline.borrow().is_empty() || host.mark_hits.get() != (0, 0) {
        return;
    }
    let Some(from) = crate::arg("--luau-metrics-from").and_then(|s| s.parse::<f64>().ok()) else { return };
    if now < from {
        return;
    }
    let base = host
        .list
        .iter()
        .filter_map(|p| Some((p.manifest.id.clone(), p.script().map(|s| (s.metrics().vm_calls, s.metrics().renders))?)))
        .collect();
    *host.baseline.borrow_mut() = base;
    host.mark_hits.set((host.hits.get(), host.renders.get()));
    eprintln!("luau metrics: mark at {now:.2} s");
}

/// Esc closes the open plugin popover (stepping back from Site settings view 2 to view 1 first).
pub fn popover_key(app: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.key != rune::Key::Escape || !app.plugins.overlay.is_open() {
        return false;
    }
    #[cfg(not(target_arch = "wasm32"))]
    if matches!(&app.plugins.overlay, Overlay::Popover { plugin, id, .. } if &**plugin == "browser" && id == "site")
        && app.workspace_panes.browser.site_detailed
    {
        app.workspace_panes.browser.site_detailed = false;
        for i in 0..app.plugins.list.len() {
            if app.plugins.list[i].half.is_builtin("browser") {
                let _ = runtime::lifecycle::fire_named(app, i, "Browser.SiteBack", rune_luau::Data::Nil);
            }
        }
        tx.instant();
        return true;
    }
    requests::close_popover(app, tx);
    true
}

/// The pane's passkey notice asks the bundled browser plugin for its import dialog on the page's
/// `site`: chosen already when `choose` (a tab of the person's), and `tab` loads again once cookies
/// came in (official `panel.tsx:603-618`).
pub fn browser_import_site(app: &mut App, site: Option<String>, choose: bool, tab: u64, tx: &mut Tx) {
    let payload = rune_luau::Data::Map(std::collections::BTreeMap::from([
        ("site".to_owned(), site.map_or(rune_luau::Data::Nil, rune_luau::Data::String)),
        ("choose".to_owned(), rune_luau::Data::Bool(choose)),
        ("tab".to_owned(), rune_luau::Data::String(tab.to_string())),
    ]));
    let mut fired = false;
    for i in 0..app.plugins.list.len() {
        if app.plugins.list[i].half.is_builtin("browser") {
            fired |= runtime::lifecycle::fire_named(app, i, "Browser.ImportSite", payload.clone());
        }
    }
    tx.instant();
    if fired {
        requests::carry_out(app, tx);
    }
}

/// Esc closes the open plugin dialog, telling its plugin (yielding when another overlay sits above it).
pub fn dialog_key(app: &mut App, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.key != rune::Key::Escape || app.plugins.dialog.is_none() {
        return false;
    }
    if app.kernel.overlays.top().is_some_and(|top| top.id != "plugin.dialog") {
        return false;
    }
    requests::dismiss_dialog(app, tx);
    true
}

/// Kernel feature registration for the Luau plugin host's overlays and idle memory/dormancy rest hook.
pub struct PluginHostFeature;

impl opencode_kernel::Feature<App> for PluginHostFeature {
    fn id(&self) -> &'static str {
        "feature.plugin-host"
    }

    fn register(&self, builder: &mut opencode_kernel::KernelBuilder<App>) {
        builder.register_overlay(
            opencode_kernel::OverlayRegistration::<App>::new("plugin.popover")
                .with_render(crate::overlays::layer::PLUGIN_TOP, |s, cx| {
                    let p = crate::palette::pal(s.dark(cx));
                    popover_view(s, p)
                })
                .with_key(popover_key),
        );
        builder.register_overlay(
            opencode_kernel::OverlayRegistration::<App>::new("plugin.dialog.standard")
                .with_render(crate::overlays::layer::DIALOG, |s, cx| {
                    let p = crate::palette::pal(s.dark(cx));
                    dialog_view_in(s, p, cx, opencode_plugin_spec::DialogLayer::Standard)
                })
                .above(crate::mcp::SIGN_IN_OVERLAY_ID),
        );
        builder.register_overlay(
            opencode_kernel::OverlayRegistration::<App>::new("plugin.dialog")
                .with_render(crate::overlays::layer::ELEVATED_DIALOG, |s, cx| {
                    let p = crate::palette::pal(s.dark(cx));
                    dialog_view_in(s, p, cx, opencode_plugin_spec::DialogLayer::Elevated)
                })
                .above("dialog.move_recovery")
                .with_key(dialog_key)
                .with_dismiss(requests::dismiss_dialog),
        );
        builder.register_overlay(
            opencode_kernel::OverlayRegistration::<App>::new("plugin.trust")
                .with_render(crate::overlays::layer::PLUGIN_TOP, |s, cx| {
                    let p = crate::palette::pal(s.dark(cx));
                    trust_dialog(s, p, cx)
                })
                .above("plugin.popover"),
        );
        builder.add_rest("plugin_host.rest", |s, keep| {
            s.plugins.shared.images.borrow_mut().rest();
            s.plugins.last_usage.borrow_mut().retain(|id, _| keep(id));
            s.plugins.panel_listed.borrow_mut().retain(|(_, _, sid), _| sid.is_empty() || keep(sid));
            for p in s.plugins.list.iter() {
                p.half.resources.borrow_mut().retain(|key, _| {
                    let path = key.split_once('\n').map_or(key.as_str(), |(_, p)| p);
                    match path.strip_prefix("/api/session/").and_then(|r| r.split(['/', '?']).next()) {
                        Some(sid) if !sid.is_empty() && !matches!(sid, "active" | "status") => keep(sid),
                        _ => true,
                    }
                });
            }
            if s.tabs.is_empty() && matches!(&*s.route, crate::Route::Home) {
                dormant::sleep_unused(s);
            }
            memory::collect_garbage(s);
            #[cfg(feature = "alloc-sites")]
            memory::publish_vms(s);
        });
    }
}
