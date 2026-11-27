//! The app's registry of servers besides its own (Desktop's `Server` registry): the ones plugins'
//! server sources list (`ServerSource:SetEntries`), keyed `<plugin>:<id>`, ordered by
//! their source's `order`, checked for health once they're ready (their version, as Desktop's
//! health poll reads it); then the ones added by address (`crate::servers`), keyed by it. Every
//! window shares it: servers are added to the app, not switched to.

use std::cell::RefCell;
use std::collections::HashMap;

use opencode_kit::servers::HttpBase;
use opencode_servers::plugin_server::{self, PluginEntry, PluginStage};
use opencode_plugin_spec::ServerState;
use rune::Tx;
use rune::machine::Attempt;

use crate::App;

pub use opencode_plugin_runtime::server_sources::ServerRec;

/// A server in the registry with what the app knows of it.
#[derive(Clone, Debug)]
pub struct Contributed {
    /// `<plugin>:<id>`, or the address of one added by address.
    pub key: String,
    pub rec: ServerRec,
    /// Added by address (`crate::servers`), rather than by a plugin.
    address: bool,
}

impl Contributed {
    /// Added by address, rather than by a plugin.
    pub fn by_address(&self) -> bool {
        self.address
    }
}

/// Every contributed server, its source's `order` first, then as listed.
pub fn all(_app: &App) -> Vec<Contributed> {
    // Server sources' entries, shared by every window; a window half draws their rows.
    let mut out: Vec<(i64, Contributed)> = super::server_sources::all().into_iter().map(|(plugin, order, rec)| (order, Contributed { key: format!("{plugin}:{}", rec.id), rec, address: false })).collect();
    out.sort_by_key(|(order, _)| *order);
    // Then the servers added by address, as Desktop lists its stored ones after those provided.
    let by_address = crate::servers::list().into_iter().map(|s| Contributed {
        key: s.url.clone(),
        rec: ServerRec { source: "http".into(), id: s.url.clone(), name: s.label(), label: String::new(), state: ServerState::Ready, url: s.url, password: s.password.unwrap_or_default(), listed: true, managed: false },
        address: true,
    });
    out.into_iter().map(|(_, c)| c).chain(by_address).collect()
}

/// What sources list changed: checks the health of servers that became ready, and forgets it
/// for the ones that aren't any more (a Luau source whose list isn't complete keeps it).
pub fn changed(app: &mut App, tx: &mut Tx) {
    tx.instant();
    let servers = all(app);
    let pending = super::server_sources::pending_sources();
    let facts = &mut app.plugins.shared.facts;
    facts.health.retain(|key, _| servers.iter().any(|s| &s.key == key && s.rec.state == ServerState::Ready) || pending.iter().any(|(plugin, _)| key.starts_with(&format!("{plugin}:"))));
    let fresh: Vec<Contributed> = servers.into_iter().filter(|s| !s.by_address() && s.rec.state == ServerState::Ready && !s.rec.url.is_empty() && !app.plugins.checking.contains(&s.key)).filter(|s| !app.plugins.shared.facts.health.contains_key(&s.key)).collect();
    for s in fresh {
        app.plugins.checking.insert(s.key.clone());
        let deliver = app.deliverer();
        crate::plugin_host::tasks::run(move || {
            let server = opencode_kit::client::Server::new(&s.rec.url).with_password(&s.rec.password);
            let version = server.get("/api/info").ok().and_then(|v| v["version"].as_str().map(str::to_owned));
            deliver(Box::new(move |app: &mut App, tx: &mut Tx| {
                app.plugins.checking.remove(&s.key);
                let still = all(app).into_iter().any(|c| c.key == s.key && c.rec.url == s.rec.url && c.rec.state == ServerState::Ready);
                if let (Some(v), true) = (version, still) {
                    app.plugins.shared.facts.health.insert(s.key.clone(), v);
                    crate::plugin_host::bump();
                    tx.instant();
                }
            }));
        });
    }
}

/// Reads the app's own server's version (`/api/info`) for plugins that compare against it.
pub fn read_own_version(app: &mut App) {
    let crate::Source::Live { server, .. } = &*app.source else { return };
    let (server, deliver) = (server.clone(), app.deliverer());
    crate::plugin_host::tasks::run(move || {
        let info = server.get("/api/info").ok();
        let field = |k: &str| info.as_ref().and_then(|v| v[k].as_str().map(str::to_owned));
        let version = field("version");
        deliver(Box::new(move |app: &mut App, _: &mut Tx| {
            if let Some(v) = version {
                app.plugins.shared.facts.server_version = Some(v);
                crate::plugin_host::bump();
            }
        }));
    });
}

/// Home shows plugin server `key` once it's ready and listed (`Servers:Connect`, kept as the
/// design's deprecated alias): every listed server is live at once (`servers::runtime`), so
/// nothing connects or switches.
pub fn connect(app: &mut App, key: &str, tx: &mut Tx) {
    // One that waits for a sign-in starts it, as Home's Authenticate does.
    sync_actions(app);
    if can_authenticate(key) {
        return authenticate(app, key, tx);
    }
    if !all(app).into_iter().any(|s| !s.by_address() && s.key == key && s.rec.listed && s.rec.state == ServerState::Ready) {
        return;
    }
    app.focus_server(&opencode_kit::servers::ServerKey::new(key), tx);
}

/// A plugin server's stage (any state but ready).
pub fn stage(state: ServerState) -> PluginStage {
    match state {
        ServerState::Starting => PluginStage::Starting,
        ServerState::Auth => PluginStage::Auth,
        ServerState::Failed => PluginStage::Failed,
        ServerState::Incompatible => PluginStage::Incompatible,
        ServerState::Stopped | ServerState::Ready => PluginStage::Stopped,
    }
}

/// What the app reads of plugin server `c`'s entry (`PluginEntrySnapshot`).
fn entry(c: &Contributed) -> PluginEntry {
    if c.rec.state != ServerState::Ready {
        return PluginEntry::staged(stage(c.rec.state));
    }
    if c.rec.url.is_empty() {
        return PluginEntry { ready: true, stage: PluginStage::Starting, http: None };
    }
    let http = HttpBase::new(&c.rec.url);
    PluginEntry::ready(if c.rec.password.is_empty() { http } else { http.with_password(&c.rec.password) })
}

/// One plugin server's sign-in (`PluginServerState`, [`plugin_server`]) and its timeout, and
/// whether its managed stream was last seen up.
#[derive(Clone, Debug, PartialEq)]
struct Action {
    machine: plugin_server::State,
    timeout: Timeout,
    link: Link,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Timeout {
    Off,
    Due { attempt: Attempt, at: f64 },
}

/// A managed server's reach, as its health says: a drop runs the plugin's `reconnect` once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Link {
    Unknown,
    Up,
    Down,
}

thread_local! {
    static ACTIONS: RefCell<HashMap<String, Action>> = RefCell::default();
    /// `connect` tasks that returned, as `(key, attempt, ready)`, for the next poll.
    static SETTLED: RefCell<Vec<(String, Attempt, bool)>> = const { RefCell::new(Vec::new()) };
}

/// Signs in to plugin server `key`, which waits for it (Home's Authenticate, a signed-out tab's
/// Sign in, `plugins.authenticate`, `Servers:Connect`): its source's `connect` runs once, a
/// second press joins it, and 120 s bound the wait.
pub fn authenticate(app: &mut App, key: &str, tx: &mut Tx) {
    sync_actions(app);
    let effects = ACTIONS.with_borrow_mut(|a| a.get_mut(key).map(|slot| plugin_server::step(&mut slot.machine, plugin_server::Event::Authenticate { on_connected: None })));
    run_actions(app, key, effects.unwrap_or_default(), tx);
}

/// Whether plugin server `key` waits for a sign-in it can start, or one is on its way.
pub fn can_authenticate(key: &str) -> bool {
    ACTIONS.with_borrow(|a| a.get(key).is_some_and(|s| s.machine.can_authenticate()))
}

/// Whether a sign-in of plugin server `key` is on its way.
pub fn authenticating(key: &str) -> bool {
    ACTIONS.with_borrow(|a| a.get(key).is_some_and(|s| matches!(s.machine, plugin_server::State::Authenticating { .. })))
}

/// `poll`: each plugin server's machine follows its entry, a sign-in that ran out of time ends,
/// and a managed server whose health dropped runs its plugin's `reconnect`.
pub fn poll_actions(app: &mut App, tx: &mut Tx) {
    sync_actions(app);
    let settings_open = app.settings_open();
    if app.plugins.cover_visit.moved(&app.route, settings_open) {
        app.plugins.cover_visit.follow(&app.route, settings_open);
    }
    for (key, attempt, ready) in SETTLED.take() {
        let effects = ACTIONS.with_borrow_mut(|a| a.get_mut(&key).map(|slot| plugin_server::step(&mut slot.machine, plugin_server::Event::ConnectSettled { attempt, ready })));
        run_actions(app, &key, effects.unwrap_or_default(), tx);
    }
    let now = *app.clock;
    let due: Vec<(String, Attempt)> = ACTIONS.with_borrow(|a| a.iter().filter_map(|(k, s)| match s.timeout {
        Timeout::Due { attempt, at } if now >= at => Some((k.clone(), attempt)),
        _ => None,
    }).collect());
    for (key, attempt) in due {
        let effects = ACTIONS.with_borrow_mut(|a| {
            let slot = a.get_mut(&key)?;
            slot.timeout = Timeout::Off;
            Some(plugin_server::step(&mut slot.machine, plugin_server::Event::ConnectTimedOut { attempt }))
        });
        run_actions(app, &key, effects.unwrap_or_default(), tx);
    }
    let managed: Vec<String> = all(app).into_iter().filter(|c| !c.by_address() && c.rec.managed && c.rec.state == ServerState::Ready).map(|c| c.key).collect();
    let dropped: Vec<String> = ACTIONS.with_borrow_mut(|a| {
        a.iter_mut()
            .filter_map(|(key, slot)| {
                let managed = managed.contains(key);
                let health = crate::servers::health::get(&opencode_kit::servers::ServerKey::new(key));
                let now = match health {
                    Some(opencode_servers::health::Health::Probe(opencode_servers::health::ProbeResult::Healthy { .. })) => Link::Up,
                    Some(opencode_servers::health::Health::Probe(opencode_servers::health::ProbeResult::Unreachable)) => Link::Down,
                    _ => slot.link,
                };
                let was = std::mem::replace(&mut slot.link, now);
                (managed && was == Link::Up && now == Link::Down).then(|| key.clone())
            })
            .collect()
    });
    for key in dropped {
        super::server_sources::call(app, &key, "reconnect", None, tx);
    }
    if let Some(at) = ACTIONS.with_borrow(|a| a.values().filter_map(|s| match s.timeout {
        Timeout::Due { at, .. } => Some(at),
        Timeout::Off => None,
    }).reduce(f64::min)) {
        tx.wake_at(at);
    }
}

/// Every plugin server has a machine following its entry; one no longer listed is disposed.
fn sync_actions(app: &App) {
    let servers: Vec<(String, PluginEntry)> = all(app).into_iter().filter(|c| !c.by_address()).map(|c| (c.key.clone(), entry(&c))).collect();
    ACTIONS.with_borrow_mut(|a| {
        a.retain(|key, _| servers.iter().any(|(k, _)| k == key));
        for (key, e) in servers {
            match a.get_mut(&key) {
                Some(slot) => {
                    let fx = plugin_server::step(&mut slot.machine, plugin_server::Event::EntryUpdated(e));
                    if fx.into_iter().any(|f| matches!(f, plugin_server::Effect::TimeoutCancel)) {
                        slot.timeout = Timeout::Off;
                    }
                }
                None => {
                    a.insert(key, Action { machine: plugin_server::State::new(&e), timeout: Timeout::Off, link: Link::Unknown });
                }
            }
        }
    });
}

fn run_actions(app: &mut App, key: &str, effects: rune::machine::Effects<plugin_server::Effect>, tx: &mut Tx) {
    for effect in effects {
        match effect {
            plugin_server::Effect::Connect { attempt, timeout_ms } => {
                let at = *app.clock + timeout_ms as f64 / 1000.0;
                ACTIONS.with_borrow_mut(|a| {
                    if let Some(slot) = a.get_mut(key) {
                        slot.timeout = Timeout::Due { attempt, at };
                    }
                });
                tx.wake_at(at);
                let answer = key.to_owned();
                let settle: super::server_sources::Settle = Box::new(move |ready| SETTLED.with_borrow_mut(|q| q.push((answer, attempt, ready))));
                // A source without `connect` can't sign in: the attempt ends at once.
                if !super::server_sources::call(app, key, "connect", Some(settle), tx) {
                    SETTLED.with_borrow_mut(|q| q.push((key.to_owned(), attempt, false)));
                }
            }
            plugin_server::Effect::TimeoutCancel => ACTIONS.with_borrow_mut(|a| {
                if let Some(slot) = a.get_mut(key) {
                    slot.timeout = Timeout::Off;
                }
            }),
            plugin_server::Effect::NotifyConnected(_) => {}
        }
    }
    tx.instant();
}

/// The window's visits: each page change starts a new one (Desktop's routed path, the SSH
/// window machine's `visit.changed`), on which a cover may offer a sign-in once per server.
#[derive(Default)]
pub struct CoverVisit {
    page: Option<(crate::Route, bool)>,
    visit: i64,
}

impl CoverVisit {
    /// The page is `route`, Settings open over it or not.
    /// Whether the window went somewhere ollow hasn't seen: polls write only then.
    fn moved(&self, route: &crate::Route, settings: bool) -> bool {
        self.page.as_ref().is_none_or(|(r, open)| r != route || *open != settings)
    }

    fn follow(&mut self, route: &crate::Route, settings: bool) {
        if self.moved(route, settings) {
            self.page = Some((route.clone(), settings));
            self.visit += 1;
        }
    }

    pub fn visit(&self) -> i64 {
        self.visit
    }
}

/// A dialog shows over the window (`plugin.dialogs.active()`): a cover offers no sign-in then.
pub fn dialog_shows(app: &App) -> bool {
    app.plugins.dialog_open()
        || app.kernel.slots.get(&crate::switcher::SWITCHER).is_open()
        || app.kernel.slots.get(&crate::project_switcher::PROJECT_SWITCHER).is_open()
        || app.kernel.slots.get(&crate::fork::FORK).is_open()
        || app.kernel.slots.get(&crate::message_nav::JUMP).open()
        || app.kernel.slots.get(&crate::session_menu::MENUS).confirming().is_some()
        || app.kernel.slots.get(&crate::connect::CONNECT).is_modal_open()
        || app.kernel.slots.get(&crate::mcp::MCP).open.is_some()
        || crate::servers::dialog::is_open(app)
        || app.kernel.slots.get(&crate::projects::REMOTE_PICKER).is_open()
}

/// The plugin cover over the routed session or draft tab (`PluginServerCover`): drawn by the
/// window half of the plugin whose server the tab is on while that server isn't ready and its
/// source has a `cover`. None otherwise; the tab stays mounted under it.
pub fn cover(app: &App, p: &crate::palette::Pal) -> Option<rune::El> {
    let (server, tab) = match &*app.route {
        crate::Route::Session { server, id } => (server.clone(), format!("{}:{id}", server.as_str())),
        crate::Route::Draft(id) => (app.tabs.iter().find(|t| t.id == *id)?.server.clone(), id.clone()),
        crate::Route::Home | crate::Route::Automations => return None,
    };
    let c = all(app).into_iter().find(|c| !c.by_address() && c.key == server.as_str() && c.rec.state != ServerState::Ready)?;
    crate::plugin_host::surfaces::server_cover(app, p, &c.key, &c.rec, &tab)
}

/// Contributed server `c`'s connection row in Settings › Servers, as its plugin draws it.
pub fn row(app: &App, p: &crate::palette::Pal, c: &Contributed) -> Option<rune::El> {
    if c.by_address() {
        return crate::servers::list().into_iter().find(|s| s.url == c.key).map(|s| {
            let key = opencode_servers::ServerConn::http_server(&s.url)
                .map_or_else(|| opencode_kit::servers::ServerKey::new(&s.url), |conn| conn.key());
            let health = crate::servers::health::get(&key);
            let http = crate::settings::HttpServer { url: s.url, password: s.password, name: s.name };
            crate::settings::pages::servers::http_row::<App>(p, &http, health.as_ref())
        });
    }
    crate::plugin_host::surfaces::server_row(app, p, &c.key, &c.rec)
}
