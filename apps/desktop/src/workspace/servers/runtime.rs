//! Every server the app knows, live at once, as OpenCode Desktop keeps them
//! (`packages/app/src/runtime/server/machine.ts`, `runtime.tsx:62-138`): the app's own server
//! (`"sidecar"`), the servers added by address (`servers.json`) and plugins' ready servers, each
//! listed with a controller ([`opencode_kit::servers::controllers`]) and its one shared copy of
//! the session data and event stream ([`ServerPeer`]), held for as long as it's listed, whatever
//! windows come and go.
//!
//! A window holds a half of its own for each server ([`Half`]: its reads' and writes' channels,
//! page loads, jobs, model catalog and used projects), and works against one server at a time,
//! the one its page shows ([`desired`]): that server's half and copy are checked out into the
//! window's plain fields, so the code that reads `s.sessions` or sends on `s.source` reads and
//! sends on that server. [`with_server`] checks another server out for a moment (its stream's
//! events, its replies, its launch), and [`switch`] changes the page's server. Every turn starts
//! checked out on the page's server ([`enter`]).
//!
//! What a window shows of the servers it isn't checked out on (its tabs' marks and titles) is
//! read whenever each was last checked out ([`Checkout::snap`]).

#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::Receiver;

use opencode_kit::client::{Incoming, Server};
use opencode_kit::model::Project;
use opencode_kit::servers::ServerKey;
use opencode_servers::health::Health;
use opencode_servers::{ServerConn, controllers};
use rune::Tx;
use rune::machine::Attempt;

use crate::windows::peer::{self, ServerPeer};
use crate::{App, Route, Source, TabStatus};

/// A listed server, as windows connect to it.
#[derive(Clone)]
pub struct Listed {
    pub key: ServerKey,
    /// Its controller's attempt: a new one (new HTTP credentials) is a new connection.
    pub attempt: Attempt,
    pub server: Server,
    pub conn: ServerConn,
    /// What lists and panels call it.
    pub name: String,
    /// It answered our credentials with 401: it stays listed, with no connection, until it
    /// takes them again or gets new ones (`ServerControllerState.unauthorized`).
    pub signed_out: bool,
}

#[derive(Default)]
struct Runtime {
    controllers: controllers::State,
    listed: Vec<Listed>,
    /// Each listed server's copy and stream, held while it's listed.
    peers: HashMap<ServerKey, Rc<ServerPeer>>,
    /// The app's own server, as launch found it.
    sidecar: Option<Server>,
    /// What the last sync listed, to step the controllers only on a change.
    inputs: Vec<(ServerConn, String)>,
    version: u64,
    #[cfg(not(target_arch = "wasm32"))]
    sidecar_published: u64,
}

thread_local! {
    static RUNTIME: RefCell<Runtime> = RefCell::default();
    /// The server the running turn is checked out on (`Route::here`, `Tab::session`).
    static CURRENT: RefCell<ServerKey> = RefCell::new(ServerKey::sidecar());
}

/// The server the running turn works against.
pub fn current() -> ServerKey {
    CURRENT.with_borrow(Clone::clone)
}

fn set_current(key: &ServerKey) {
    CURRENT.with_borrow_mut(|c| {
        if c != key {
            *c = key.clone();
        }
    });
}

/// The app's own server, as the first window's launch found it.
pub fn set_sidecar(server: &Server) {
    RUNTIME.with_borrow_mut(|r| {
        let changed = r.sidecar.as_ref().is_none_or(|s| s.base != server.base || s.auth_header() != server.auth_header());
        if changed {
            r.sidecar = Some(server.clone());
        }
    });
}

/// The app's own server (`"sidecar"`), whichever server a window shows.
pub fn own_server() -> Option<Server> {
    RUNTIME.with_borrow(|r| r.sidecar.clone())
}

/// The listed servers, the app's own first.
pub fn listed() -> Vec<Listed> {
    RUNTIME.with_borrow(|r| r.listed.clone())
}

/// Changes whenever the listed servers do.
pub fn version() -> u64 {
    RUNTIME.with_borrow(|r| r.version)
}

/// Changes whenever the sidecar's endpoint or credentials do, even before `sync` runs.
pub(crate) fn sidecar_stamp() -> u64 {
    use std::hash::{Hash, Hasher};
    RUNTIME.with_borrow(|r| {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        r.sidecar.as_ref().map(|s| (&s.base, s.auth_header())).hash(&mut h);
        h.finish()
    })
}

pub(crate) fn peer(key: &ServerKey) -> Option<Rc<ServerPeer>> {
    RUNTIME.with_borrow(|r| r.peers.get(key).cloned())
}

/// Weak handles to every `Pages` prefetch cache in this window (active and parked servers).
pub(crate) fn all_cache_refs(s: &App) -> Vec<crate::session_pages::CacheRef> {
    s.pages
        .cache_ref()
        .into_iter()
        .chain(s.checkout.parked.iter().filter_map(|(_, _, half)| half.pages.cache_ref()))
        .collect()
}

/// True when `refs` already matches the exact set of `Pages` pools in this window (avoids
/// reallocating `Open::pages` on every turn).
pub(crate) fn same_cache_refs(s: &App, refs: &[crate::session_pages::CacheRef]) -> bool {
    let expected = usize::from(s.pages.has_pool()) + s.checkout.parked.iter().filter(|(_, _, h)| h.pages.has_pool()).count();
    if refs.len() != expected {
        return false;
    }
    let mut i = 0;
    if s.pages.has_pool() {
        if !s.pages.same_pool(&refs[i]) { return false; }
        i += 1;
    }
    for (_, _, half) in &s.checkout.parked {
        if half.pages.has_pool() {
            if !half.pages.same_pool(&refs[i]) { return false; }
            i += 1;
        }
    }
    true
}

/// User message ids held by every parked server half's `Pages` cache and local transcripts.
pub(crate) fn parked_user_message_ids(s: &App) -> Vec<String> {
    s.checkout
        .parked
        .iter()
        .flat_map(|(_, _, half)| half.pages.user_message_ids().into_iter().chain(half.store.local_user_message_ids()))
        .collect()
}

/// User message ids held by every other server's shared peer (outside `active`), so `App::rest`
/// never prunes process-global user-message metadata belonging to another server's transcripts.
pub(crate) fn other_peer_user_message_ids(active: &ServerKey) -> Vec<String> {
    RUNTIME.with_borrow(|r| {
        r.peers
            .iter()
            .filter(|(k, _)| *k != active)
            .flat_map(|(_, peer)| peer.user_message_ids())
            .collect()
    })
}

/// A window started `key`'s copy and stream: the runtime holds them while `key` is listed.
pub(crate) fn keep_peer(key: &ServerKey, peer: &Rc<ServerPeer>) {
    RUNTIME.with_borrow_mut(|r| {
        if r.listed.iter().any(|l| l.key == *key) {
            r.peers.insert(key.clone(), peer.clone());
        }
    });
}

/// What panels and lists call server `key` (`servers.label`, `registry.tsx:325-331`): its name;
/// the app's own by the machine its `/api/info` names, else "This computer"; another by its
/// address.
pub fn label(key: &ServerKey) -> String {
    let named = RUNTIME.with_borrow(|r| r.listed.iter().find(|l| l.key == *key).map(|l| l.name.clone())).filter(|n| !n.is_empty());
    if let Some(name) = named {
        return name;
    }
    if key.is_sidecar() {
        return super::health::hostname(key).unwrap_or_else(|| opencode_i18n::t!("server.thisComputer").to_owned());
    }
    key.as_str().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_owned()
}

/// What tells server `key` apart where tabs of several servers mix (`servers.tag`,
/// `registry.tsx:357-359`): its name, while more than one server is listed and it isn't the
/// app's own.
pub fn tag(key: &ServerKey) -> Option<String> {
    let all = RUNTIME.with_borrow(|r| r.listed.len());
    opencode_servers::registry::server_tag(all, key.is_sidecar(), || label(key))
}

/// Server `key`'s health (`global.servers.health[key]`).
pub fn health(key: &ServerKey) -> Option<Health> {
    super::health::get(key)
}

/// Whether server `key` refused our credentials (`selectServerSignedOut`).
pub fn signed_out(key: &ServerKey) -> bool {
    RUNTIME.with_borrow(|r| r.controllers.signed_out(key))
}

/// Lists the app's servers anew (`servers.synced`): the controllers start a connection for each
/// new server, end the ones gone, and start again for an HTTP server with new credentials.
fn sync(s: &App) {
    let Some(own) = own_server() else { return };
    // The servers with an endpoint: a plugin's server that isn't ready has nothing to connect to.
    let inputs: Vec<(ServerConn, String)> = super::registry::list(s).into_iter().filter(|e| e.conn.http().is_some()).map(|e| (e.conn, e.name)).collect();
    RUNTIME.with_borrow_mut(|r| {
        if r.inputs == inputs {
            return;
        }
        r.inputs = inputs.clone();
        r.version += 1;
        let conns = inputs.iter().map(|(c, _)| c.clone()).collect();
        let fx = controllers::step(&mut r.controllers, controllers::Event::ServersSynced(conns));
        apply(r, fx, &own);
        // A server that keeps its controller may still move (the sidecar, a plugin's port): its
        // connection follows, and the copy starts again on the new address.
        for (conn, name) in &inputs {
            let key = conn.key();
            let Some(l) = r.listed.iter_mut().find(|l| l.key == key) else { continue };
            let server = server_for(conn, &own);
            if server.base != l.server.base || server.auth_header() != l.server.auth_header() {
                r.peers.remove(&key);
            }
            l.server = server;
            l.conn = conn.clone();
            l.name = name.clone();
        }
        let order: Vec<ServerKey> = inputs.iter().map(|(c, _)| c.key()).collect();
        r.listed.sort_by_key(|l| order.iter().position(|k| *k == l.key));
    });
}

/// Runs the controllers' effects: a disposed server leaves the listing and drops its copy (one
/// that refused our credentials stays, signed out); a created one joins it.
fn apply(r: &mut Runtime, fx: rune::machine::Effects<controllers::Effect>, own: &Server) {
    for effect in fx {
        match effect {
            // One that refused our credentials stays listed, signed out (Home's lock, the tab's
            // Signed out panel), with no copy or stream.
            controllers::Effect::Dispose { key, attempt } => {
                r.peers.remove(&key);
                if r.controllers.signed_out(&key) {
                    r.listed.iter_mut().filter(|l| l.key == key).for_each(|l| l.signed_out = true);
                } else {
                    r.listed.retain(|l| !(l.key == key && l.attempt == attempt));
                }
            }
            controllers::Effect::Create { key, attempt, conn } => {
                let name = r.inputs.iter().find(|(c, _)| c.key() == key).map(|(_, n)| n.clone()).unwrap_or_default();
                r.listed.retain(|l| l.key != key);
                r.listed.push(Listed { key: key.clone(), attempt, server: server_for(&conn, own), conn, name, signed_out: false });
                controllers::step(&mut r.controllers, controllers::Event::ControllerCreated { key, attempt });
            }
        }
    }
    let order: Vec<ServerKey> = r.inputs.iter().map(|(c, _)| c.key()).collect();
    r.listed.sort_by_key(|l| order.iter().position(|k| *k == l.key));
}

/// A health probe answered (`health.reported`): a server that rejects its credentials stops its
/// connection, and starts it again once a probe with the same credentials answers.
pub fn reported(probe: &opencode_servers::health::Probe) {
    let Some(own) = own_server() else { return };
    RUNTIME.with_borrow_mut(|r| {
        let event = controllers::Event::HealthReported { key: probe.key.clone(), auth: probe.auth.clone(), result: probe.result.clone() };
        let fx = controllers::step(&mut r.controllers, event);
        if fx.is_empty() {
            return;
        }
        apply(r, fx, &own);
        r.version += 1;
    });
}

/// The connection for `conn`: the app's own server as launch found it; another with its
/// password, its files another machine's unless it's on the app's own loopback port.
fn server_for(conn: &ServerConn, own: &Server) -> Server {
    let Some(http) = conn.http().filter(|_| !conn.builtin()) else { return own.clone() };
    // Added and plugin servers have the same passive page readers as the initial server.
    let mut server = Server::new(&http.url);
    if let Some(p) = &http.password {
        server = server.with_password(p);
    }
    if conn.local(Some(&own.base)) { server } else { server.with_remote_files() }
}

/// Which listing of its server a window's half connects to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Listing {
    /// Made before its server was listed: the app's own at launch, a tab's server not listed
    /// yet. The first listing with its address takes it over.
    #[default]
    Unlisted,
    /// The controller's attempt it connects to; another attempt is a new connection.
    Listed(Attempt),
}

/// What ties a window's half to its server's listing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Meta {
    listing: Listing,
    base: String,
    auth: Option<String>,
}

/// A window's own part of one server, checked out into its plain fields while it works against
/// that server.
#[derive(Default)]
pub struct Half {
    source: Source,
    /// Its file reader, in-flight reads and cached pictures belong to this server.
    media: crate::attachments::MediaStore,
    inbox: Option<Receiver<Incoming>>,
    pages: crate::session_pages::State,
    opening: Option<crate::session_pages::Opening>,
    store: peer::Link,
    jobs: crate::jobs::Jobs,
    request_reads: crate::session_status::RequestReads,
    used: Vec<String>,
    /// The server's models, providers and agents (the pickers' catalog).
    pickers: crate::pickers::Pickers,
    /// Its folders' git branches (the inbox's, the new-session page's).
    branches: crate::inbox_model::Branches,
}

impl Half {
    /// Trades places with the window's fields: no copies.
    fn swap(&mut self, s: &mut App) {
        use std::mem::swap;
        swap(&mut self.source, &mut s.source);
        swap(&mut self.media, &mut s.composer_ext.attach.store);
        if s.composer_ext.attach.waker.is_none() && let Some(wake) = s.waker.as_ref() {
            s.composer_ext.attach.set_waker(wake.clone());
        }
        swap(&mut self.inbox, &mut s.inbox);
        swap(&mut self.pages, &mut s.pages);
        swap(&mut self.opening, &mut s.opening);
        swap(&mut self.store, &mut s.store);
        self.jobs.swap_server(&mut s.jobs);
        swap(&mut self.request_reads, &mut s.request_reads);
        swap(&mut self.used, &mut s.project_state.used);
        swap(&mut self.branches, &mut s.sidebar_inbox.branches);
        let (a, b) = (&mut self.pickers, &mut s.composer_ext.pickers);
        swap(&mut a.models, &mut b.models);
        swap(&mut a.providers, &mut b.providers);
        swap(&mut a.provider_order, &mut b.provider_order);
        swap(&mut a.configured, &mut b.configured);
        swap(&mut a.limits, &mut b.limits);
        swap(&mut a.paid, &mut b.paid);
        swap(&mut a.agents, &mut b.agents);
        swap(&mut a.agent_models, &mut b.agent_models);
    }
}

/// How far the window's start is: its servers' first reads wait for its first frame
/// (`on_ready`) in a windowed run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Boot {
    #[default]
    Before,
    Ready,
}

/// A window's servers: the one checked out into its fields, and its halves of the others.
pub struct Checkout {
    active: ServerKey,
    meta: Meta,
    parked: Vec<(ServerKey, Meta, Half)>,
    /// Each tab's mark and project on a server the window isn't checked out on, as of its last
    /// checkout; of a draft's tab, its project alone.
    snaps: HashMap<(ServerKey, String), Snap>,
    pub boot: Boot,
    synced: u64,
}

/// What a tab of a server the window isn't checked out on shows, as of its last checkout.
#[derive(Clone, Debug, Default)]
struct Snap {
    read_menu: Option<bool>,
    status: Option<TabStatus>,
    project: Option<Project>,
    /// Its server has its session.
    known: bool,
}

impl Default for Checkout {
    fn default() -> Self {
        Checkout { active: ServerKey::sidecar(), meta: Meta::default(), parked: Vec::new(), snaps: HashMap::new(), boot: Boot::default(), synced: 0 }
    }
}

impl Checkout {
    pub(crate) fn read_menu(&self, server: &ServerKey, id: &str) -> Option<bool> {
        self.snaps.get(&(server.clone(), id.to_owned()))?.read_menu
    }
    /// Tab `id`'s mark on `server`, as of when the window last worked against it.
    pub fn snap(&self, server: &ServerKey, id: &str) -> Option<TabStatus> {
        self.snaps.get(&(server.clone(), id.to_owned())).and_then(|s| s.status)
    }

    /// Tab `id`'s project on `server` (its session's, else its folder's), as of when the window
    /// last worked against it.
    pub fn project(&self, server: &ServerKey, id: &str) -> Option<&Project> {
        self.snaps.get(&(server.clone(), id.to_owned())).and_then(|s| s.project.as_ref())
    }

    /// Whether `server` had tab `id`'s session when the window last worked against it.
    pub fn known(&self, server: &ServerKey, id: &str) -> bool {
        self.snaps.get(&(server.clone(), id.to_owned())).is_some_and(|s| s.known)
    }

    /// Every server the window has a half of, the checked-out one first.
    pub fn keys(&self) -> Vec<ServerKey> {
        std::iter::once(self.active.clone()).chain(self.parked.iter().map(|(k, ..)| k.clone())).collect()
    }

    /// Version stamp of the active checkout key and every parked server's Home-visible data
    /// (`used` project IDs and `projects`/`sessions`/`recent_page` `Tracked` versions).
    pub(crate) fn home_data_stamp(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.active.hash(&mut h);
        self.synced.hash(&mut h);
        self.parked.len().hash(&mut h);
        // `swap_to` rotates `self.parked` on every `with_server`, so combine entries order-independently.
        let mut parked_xor = 0u64;
        for (key, meta, half) in &self.parked {
            let mut eh = std::collections::hash_map::DefaultHasher::new();
            (key, &meta.base, &half.used, half.store.home_data_stamp()).hash(&mut eh);
            parked_xor ^= eh.finish();
        }
        parked_xor.hash(&mut h);
        h.finish()
    }
}

/// The server the window works against now.
pub fn active(s: &App) -> &ServerKey {
    &s.checkout.active
}

/// Resolves the canonical `ServerKey` that owns session `sid` across the checked-out server and
/// parked servers' snapshots (returns `None` when `sid` is unknown to the window).
#[cfg(not(target_arch = "wasm32"))]
pub fn server_for_session(s: &App, sid: &str) -> Option<ServerKey> {
    if let Some(tab) = s
        .tabs
        .iter()
        .find(|t| t.id == sid && t.kind == crate::tabs::TabKind::Session)
    {
        return Some(tab.server.clone());
    }
    if s.session(sid).is_some() {
        return Some(s.checkout.active.clone());
    }
    for ((key, id), snap) in &s.checkout.snaps {
        if id == sid && snap.known {
            return Some(key.clone());
        }
    }
    None
}

/// Resolves the [`ServerKey`] that owns session `sid` (checking open tabs, active route, active
/// store, and parked server stores — never silently substituting `active` for a parked session).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn session_server_key(s: &App, sid: &str) -> Option<ServerKey> {
    if let Some(key) = server_for_session(s, sid) {
        return Some(key);
    }
    if let Route::Session { id, server } = &*s.route
        && id == sid
    {
        return Some(server.clone());
    }
    if s.sessions.iter().any(|x| x.id == sid) {
        return Some(s.checkout.active.clone());
    }
    s.checkout
        .parked
        .iter()
        .find(|(_, _, half)| half.store.has_session(sid))
        .map(|(k, ..)| k.clone())
}

/// Returns the live [`Server`] handle for `key` if checked out or parked and not signed out.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn live_server_for_key(s: &App, key: &ServerKey) -> Option<Server> {
    if signed_out(key) {
        return None;
    }
    if *key == s.checkout.active {
        return match &*s.source {
            Source::Live { server, .. } => Some(server.clone()),
            _ => None,
        };
    }
    s.checkout
        .parked
        .iter()
        .find(|(k, ..)| k == key)
        .and_then(|(_, _, half)| match &half.source {
            Source::Live { server, .. } => Some(server.clone()),
            _ => None,
        })
}

impl App {
    /// The server the window works against, when it isn't the app's own (a plugin's `<plugin>:<id>`,
    /// an added server's address): what plugins see as the current server.
    pub fn server_key(&self) -> Option<String> {
        (!self.checkout.active.is_sidecar()).then(|| self.checkout.active.as_str().to_owned())
    }
}

/// The server the window's page is on: Settings' own on its per-server pages, else a session's,
/// a draft tab's, else Home's.
pub fn desired(s: &App) -> ServerKey {
    // Settings' per-server pages work with Settings' own server while it's listed.
    let sp = s.kernel.slots.get(&crate::settings::SETTINGS_PAGES);
    if s.settings_open() && sp.page.per_server() && listed().iter().any(|l| l.key == sp.server.key) {
        return sp.server.key.clone();
    }
    match &*s.route {
        Route::Session { server, .. } => server.clone(),
        Route::Draft(id) => s.tabs.iter().find(|t| t.id == *id).map_or_else(|| crate::projects::home::server(s), |t| t.server.clone()),
        Route::Home | Route::Automations => crate::projects::home::server(s),
    }
}

/// `App::turn_scope`: the turn starts checked out on the page's server.
pub fn enter(s: &mut App) {
    let want = desired(s);
    if want != s.checkout.active && s.live() {
        peer::enter(s);
        snap(s);
        peer::leave(s);
        swap_to(s, &want);
    }
    set_current(&s.checkout.active);
    peer::enter(s);
}

/// `App::turn_scope`: the turn ends; the copy goes back.
pub fn leave(s: &mut App) {
    crate::windows::sync_shown(s);
    peer::leave(s);
}

/// The window works against `key` from now on, mid-turn (a tab or a session of another server
/// opens).
pub fn switch(s: &mut App, key: &ServerKey) {
    if *key == s.checkout.active || !s.live() {
        return;
    }
    snap(s);
    peer::leave(s);
    swap_to(s, key);
    set_current(key);
    peer::enter(s);
}

/// Runs `f` checked out on `key`, then checks the window's own server out again. None when the
/// window has no half of `key`.
pub fn with_server<R>(s: &mut App, key: &ServerKey, f: impl FnOnce(&mut App) -> R) -> Option<R> {
    if *key == s.checkout.active {
        return Some(f(s));
    }
    if !s.checkout.parked.iter().any(|(k, ..)| k == key) {
        return None;
    }
    let back = s.checkout.active.clone();
    peer::leave(s);
    swap_to(s, key);
    set_current(key);
    peer::enter(s);
    let r = f(s);
    snap(s);
    peer::leave(s);
    swap_to(s, &back);
    set_current(&back);
    peer::enter(s);
    Some(r)
}

/// Moves the window's fields to `key`'s half (made for it when the window has none), parking the
/// checked-out one. Nothing may be checked out of a peer.
fn swap_to(s: &mut App, key: &ServerKey) {
    let (meta, mut half) = match s.checkout.parked.iter().position(|(k, ..)| k == key) {
        Some(at) => {
            let (_, meta, half) = s.checkout.parked.remove(at);
            (meta, half)
        }
        None => new_half(s, key, true),
    };
    peer::stash(s);
    half.swap(s);
    peer::unstash(s);
    let old_meta = std::mem::replace(&mut s.checkout.meta, meta);
    let old = std::mem::replace(&mut s.checkout.active, key.clone());
    s.checkout.parked.push((old, old_meta, half));
}

/// A half for server `key`: connected to its listing, or, while it isn't listed (a tab's plugin
/// server still starting), offline until it is. `adopt`: it reads nothing of its own yet, so it
/// starts in the server's live copy, if there is one.
fn new_half(s: &mut App, key: &ServerKey, adopt: bool) -> (Meta, Half) {
    // A restored page's server may be listed before the window's first poll lists it.
    if !listed().iter().any(|l| l.key == *key) {
        sync(s);
    }
    // A server that refused our credentials gets no connection until it takes them again.
    let listed = listed().into_iter().find(|l| l.key == *key && !l.signed_out);
    let allow_send = matches!(*s.source, Source::Live { allow_send: true, .. });
    let (meta, server) = match &listed {
        Some(l) => (Meta { listing: Listing::Listed(l.attempt), base: l.server.base.clone(), auth: l.server.auth_header().map(str::to_owned) }, l.server.clone()),
        None if key.is_sidecar() => (Meta::default(), own_server().unwrap_or_else(|| Server::new(opencode_kit::client::DEFAULT_SERVER))),
        None => (Meta::default(), Server::new(&format!("http://{}.invalid", key.as_str().replace([':', '/'], "-"))).with_remote_files()),
    };
    let phase = if listed.is_some() { crate::launch::Phase::Waiting { beta_tabs: false, fallback_fixture: false } } else { crate::launch::Phase::Offline { retries: 0, retry_at: f64::INFINITY } };
    s.launch.set_phase(key.as_str(), phase);
    let wake = s.launch.inbox_wake.clone().or_else(|| (*s.waker).clone());
    let store = peer(key).filter(|_| adopt).map(|p| peer::member(p, wake)).unwrap_or_default();
    (meta, Half { source: Source::Live { server, requests: None, writes: None, allow_send }, store, ..Half::default() })
}

/// Notes the marks of the checked-out server's tabs, and their titles, for while it isn't.
fn snap(s: &mut App) {
    let key = s.checkout.active.clone();
    let ids: Vec<String> = s.tabs.iter().filter(|t| !t.is_draft() && t.server == key).map(|t| t.id.clone()).collect();
    for id in ids {
        let status = s.tab_status(&id);
        if let Some((title, dir)) = s.session(&id).map(|i| (i.title.clone(), i.directory.clone()))
            && let Some(t) = s.tabs.iter_mut().find(|t| t.id == id && t.server == key)
        {
            t.title = title;
            t.directory = dir;
        }
        let dir = s.tabs.iter().find(|t| t.id == id && t.server == key).map(|t| t.directory.clone()).unwrap_or_default();
        let project = s.session(&id).and_then(|i| s.project_for_session(i)).or_else(|| s.project_at(&dir)).cloned();
        let known = s.session(&id).is_some();
        let marks = s.marks(s.root_of(&id));
        let read_menu = (!marks.attention && !marks.busy && (marks.unread || s.session(&id).is_some_and(|i| i.idle.is_some()))).then_some(marks.unread);
        s.checkout.snaps.insert((key.clone(), id.clone()), Snap { status, project, known, read_menu });
    }
    // A draft's project is its folder's on its own server (official
    // `packages/app/src/shell/titlebar/auto-tab-sections.tsx:39-42`): kept too, so its tab stays
    // in its group while the window works against another server.
    let drafts: Vec<(String, String)> = s.tabs.iter().filter(|t| t.is_draft() && t.server == key).map(|t| (t.id.clone(), t.directory.clone())).collect();
    for (id, dir) in drafts {
        let project = s.project_at(&dir).cloned();
        s.checkout.snaps.insert((key.clone(), id), Snap { project, ..Snap::default() });
    }
    crate::inbox_model::snap(s);
}

/// `poll`: the listing's changes reach the window's halves, the servers it hasn't started yet
/// start, and each server it isn't checked out on takes what came in. True when what the window
/// shows of them changed.
pub fn poll(s: &mut App, tx: &mut Tx) -> bool {
    if !s.live() {
        return false;
    }
    #[cfg(not(target_arch = "wasm32"))]
    if super::sidecar::managed() {
        let pv = super::sidecar::published_version();
        if pv != RUNTIME.with_borrow(|r| r.sidecar_published) {
            RUNTIME.with_borrow_mut(|r| r.sidecar_published = pv);
            if let Some(actual) = super::sidecar::current() {
                set_sidecar(&actual);
            }
        }
    }
    sync(s);
    if s.checkout.synced != version() {
        s.checkout.synced = version();
        refit(s);
    }
    let mut changed = false;
    for key in s.checkout.keys() {
        if s.checkout.boot == Boot::Ready && s.launch.phase(key.as_str()).is_some_and(|p| matches!(p, crate::launch::Phase::Waiting { .. })) {
            with_server(s, &key, |s| {
                s.open_outbox();
                crate::launch::start(s, tx);
            });
        }
        if key != s.checkout.active {
            changed |= with_server(s, &key, |s| background(s, tx)).unwrap_or(false);
        }
    }
    // Another server's inbox rows, as `snap` took them.
    changed || s.sidebar_inbox.redraw
}

/// A server the window isn't checked out on: runs every system marked `.per_server(...)` in the
/// global resolved schedule order. True when its tabs' marks, titles, or inbox branches changed.
fn background(s: &mut App, tx: &mut Tx) -> bool {
    let before = marks(s);
    let scheduler = s.kernel.scheduler.clone();
    let redraw = scheduler.run_per_server(s, *s.clock, tx);
    before != marks(s) || redraw
}

fn marks(s: &App) -> Vec<(Option<TabStatus>, String)> {
    let key = &s.checkout.active;
    s.tabs.iter().filter(|t| t.server == *key && !t.is_draft()).map(|t| (s.tab_status(&t.id), s.session(&t.id).map_or_else(|| t.title.clone(), |i| i.title.clone()))).collect()
}

/// The listing changed: halves of servers no longer listed, or listed anew, go (their tabs stay,
/// as Desktop keeps a tab while its server is away); listed servers the window has no half of
/// get one.
fn refit(s: &mut App) {
    // A server signed out has no connection: windows hold an offline half of it.
    let listed: Vec<Listed> = listed().into_iter().filter(|l| !l.signed_out).collect();
    let stale = |key: &ServerKey, meta: &Meta| match listed.iter().find(|l| l.key == *key) {
        Some(l) => meta.base != l.server.base || meta.auth.as_deref() != l.server.auth_header() || matches!(meta.listing, Listing::Listed(a) if a != l.attempt),
        None => meta.listing != Listing::Unlisted,
    };
    let gone: Vec<ServerKey> = s.checkout.parked.iter().filter(|(k, m, _)| stale(k, m)).map(|(k, ..)| k.clone()).collect();
    for key in gone {
        with_server(s, &key, peer::leave_server);
        s.checkout.parked.retain(|(k, ..)| *k != key);
        s.launch.forget(key.as_str());
        s.services.server_links.forget(key.as_str());
    }
    let key = s.checkout.active.clone();
    let probing_own = key.is_sidecar() && matches!(s.launch.phase(key.as_str()), Some(crate::launch::Phase::Probing { .. }));
    if !probing_own && stale(&key, &s.checkout.meta) {
        // The same server under a new listing (it signed us out, moved, or took us back): what the
        // window read of it stays on screen in the new half, rather than blanking until it reloads.
        let kept = peer::reading(s);
        peer::leave_server(s);
        let (meta, mut half) = new_half(s, &key, false);
        half.swap(s);
        peer::keep_reading(s, kept);
        s.checkout.meta = meta;
        peer::enter(s);
    }
    if let Some(l) = listed.iter().find(|l| l.key == key) {
        s.checkout.meta.listing = Listing::Listed(l.attempt);
    }
    for l in &listed {
        if l.key != s.checkout.active && !s.checkout.parked.iter().any(|(k, ..)| *k == l.key) {
            let (meta, half) = new_half(s, &l.key, true);
            s.checkout.parked.push((l.key.clone(), meta, half));
        }
    }
    for (k, meta, _) in &mut s.checkout.parked {
        if let Some(l) = listed.iter().find(|l| l.key == *k) {
            meta.listing = Listing::Listed(l.attempt);
        }
    }
}

/// The window starts on the app's own server (`App::load_live`): `server`, or where an earlier
/// window's launch found it.
pub fn bind(s: &mut App, server: Server) -> Server {
    let server = own_server().unwrap_or(server);
    s.checkout.meta.base = server.base.clone();
    s.checkout.meta.auth = server.auth_header().map(str::to_owned);
    set_sidecar(&server);
    server
}

/// The app's own server moved (`service.json` named another port): the checked-out half follows
/// it without starting again.
pub fn moved(s: &mut App, server: &Server) {
    if s.checkout.active.is_sidecar() {
        s.checkout.meta.base = server.base.clone();
        s.checkout.meta.auth = server.auth_header().map(str::to_owned);
    }
    set_sidecar(server);
}

/// The window closes: it leaves every server's copy, which the other windows and the runtime keep.
pub fn leave_all(s: &mut App) {
    for key in s.checkout.keys() {
        with_server(s, &key, peer::leave_server);
    }
}
