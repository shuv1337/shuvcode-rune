//! Pure-Rust desktop driver for the OpenCode server's `experimental.browser` v4 RPC protocol
//! (`packages/gui-plugins/src/browser/{pane,pane-machine,machine,model}.ts`, `PRE-08` `#63500`).
//!
//! Lives in `StateSlots` (`BROWSER_RPC`, zero fields on `struct App`), wakes zero Luau VMs, and
//! keeps every session's tab namespace (`tab_<uuid>`) isolated by construction.

mod comes_back;
mod exec;
pub mod express;
mod frames;
mod hold;
mod session;
mod start_gate;
mod suppression;
mod tunnel;
mod workers;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::{self, Receiver, Sender};

use opencode_kernel::{Feature, KernelBuilder, Slot};
use opencode_kit::browser_rpc::{
    AttachEffect, AttachEvent, AttachOutcome, Command, LocationRef, Outcome,
    RpcCallError, decode_control, failure_code,
};
use opencode_kit::client::Server;
use opencode_kit::servers::ServerKey;
use rune::{Tx, Waker, time::Instant};
use serde_json::Value;

pub use self::comes_back::person_came;
use self::session::SessionAttachment;
pub use self::tunnel::grant_remote_navigation;
use self::start_gate::{StartWaiter, drain_start_waiters};
pub use self::start_gate::{hold_start_gate, release_start_waiters};
use self::suppression::{maybe_start_capability_probe, server_suppressed, sync_mcp_suppression};
use crate::{App, Route, Source};

pub static BROWSER_RPC: Slot<BrowserRpcState> = Slot::new();

pub struct BrowserRpcFeature;

impl Feature<App> for BrowserRpcFeature {
    fn id(&self) -> &'static str {
        "browser-rpc"
    }

    fn register(&self, builder: &mut KernelBuilder<App>) {
        builder.register_slot(&BROWSER_RPC);
        builder.add_setup("browser_rpc.waker", |s, w| {
            s.kernel.slots.get_mut(&BROWSER_RPC).waker = Some(w);
        });
        builder
            .schedule(crate::tick::stage::REACT_PANELS, "browser_rpc.poll", |s, _, tx| {
                poll(s, tx)
            })
            .after("browser.poll");
    }
}

pub(super) enum WorkerMsg {
    AttachEnded {
        server_key: ServerKey,
        session_id: String,
        attempt: u64,
        outcome: AttachOutcome,
    },
    /// The session's Location answered its plugin list (`AttachEffect::ListPlugins`): `any` when it names a plugin.
    PluginsListed { server_key: ServerKey, session_id: String, attempt: u64, any: bool },
    /// Prewarmed binary WebSocket stream connected and now owns attachment liveness (`opencode#66217`).
    StreamActivated { server_key: ServerKey, session_id: String, attempt: u64 },
    /// Liveness-owning binary WebSocket stream closed or was replaced (`opencode#66217`).
    StreamEnded { server_key: ServerKey, session_id: String, attempt: u64, replaced: bool, reason: String },
    CommandFetched {
        server_key: ServerKey,
        session_id: String,
        connection_id: String,
        request_id: String,
        result: Result<Command, RpcCallError>,
    },
    ProbeEnded {
        server_key: ServerKey,
        server: Server,
        generation: u64,
        supported: Option<bool>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ServerRpcVerdict {
    Probing,
    Supported,
    LegacyAllowed,
    TransientError,
}

pub struct BrowserRpcState {
    sessions: BTreeMap<(ServerKey, String), SessionAttachment>,
    /// Sessions that attach while they stay open: the ones the person routed into on this window,
    /// and the ones that ran in a tab nobody had opened (`comes_back::unopened`).
    routed_sessions: BTreeSet<(ServerKey, String)>,
    /// The session on screen at the last poll: a change is the person coming to a session.
    on_screen: Option<(ServerKey, String)>,
    /// Starts whose `POST /api/session` returned (`Incoming::PromptGate`), so `attach` never races
    /// the create into `Blocked::Missing`. Pruned once the start leaves `app.queue.starts`.
    created_starts: BTreeSet<(ServerKey, String)>,
    /// New-session starts holding their one prompt (`start_gate.rs`).
    start_waiters: Vec<StartWaiter>,
    sse_connected: BTreeMap<ServerKey, bool>,
    /// Per-server endpoint capability verdict (`LegacyAllowed` stays sticky across empty
    /// sessions/tab close/slow SSE until reconnect or `plugin.updated`).
    server_verdicts: BTreeMap<ServerKey, (Server, u64, ServerRpcVerdict)>,
    probe_gen: u64,
    mcp_suppressed: BTreeMap<ServerKey, (Server, bool)>,
    tunnels: tunnel::RemoteTunnelManager,
    worker_tx: Sender<WorkerMsg>,
    worker_rx: Receiver<WorkerMsg>,
    waker: Option<Waker>,
    next_diag_scope: u64,
    ref_counter: u64,
    /// App-wide single active trace owner (`Some(host_id)` while tracing; official `shared.trace`).
    pub(super) active_trace_host: Option<u64>,
    holds: hold::RpcHoldManager,
}

impl Default for BrowserRpcState {
    fn default() -> Self {
        let (worker_tx, worker_rx) = mpsc::channel();
        Self {
            sessions: BTreeMap::new(),
            routed_sessions: BTreeSet::new(),
            on_screen: None,
            created_starts: BTreeSet::new(),
            start_waiters: Vec::new(),
            sse_connected: BTreeMap::new(),
            server_verdicts: BTreeMap::new(),
            probe_gen: 0,
            mcp_suppressed: BTreeMap::new(),
            tunnels: tunnel::RemoteTunnelManager::default(),
            worker_tx,
            worker_rx,
            waker: None,
            next_diag_scope: 1,
            ref_counter: 0,
            active_trace_host: None,
            holds: hold::RpcHoldManager::default(),
        }
    }
}

/// Whether legacy local `browser_*` MCP tools must be suppressed on `server_key` / `session_id`:
/// fail closed on live servers until the server is proven `Unsupported` or `Unavailable` (via
/// session attach or one-shot capability probe), keeping `LegacyAllowed` sticky per window
/// endpoint until reconnect or `plugin.updated`.
pub fn is_mcp_browser_suppressed(app: &App, server_key: &ServerKey, _session_id: Option<&str>) -> bool {
    if !app.live() {
        return false;
    }
    server_suppressed(app.kernel.slots.get(&BROWSER_RPC), server_key)
}

/// Returns the owning `ServerKey` of any session tracked in `BROWSER_RPC` (including background
/// remote-server sessions not currently active in the window).
pub fn server_for_session(app: &App, session_id: &str) -> Option<ServerKey> {
    app.kernel
        .slots
        .get(&BROWSER_RPC)
        .sessions
        .keys()
        .find(|(_, sid)| sid == session_id)
        .map(|(k, _)| k.clone())
}

/// Whether `session_id` on `server_key` has settled its attachment attempt for `BuiltinPromptGate`.
#[allow(dead_code)]
pub fn session_prompt_gate_settled(app: &App, server_key: &ServerKey, session_id: &str) -> bool {
    app.kernel
        .slots
        .get(&BROWSER_RPC)
        .sessions
        .get(&(server_key.clone(), session_id.to_owned()))
        .is_none_or(|att| att.attach.prompt_gate_settled())
}

static CONNECTED_BASES: std::sync::LazyLock<std::sync::Mutex<BTreeSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(BTreeSet::new()));

fn mark_base_connected(base: &str) {
    CONNECTED_BASES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(base.to_owned());
}

fn clear_base_connected(base: &str) {
    CONNECTED_BASES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(base);
}

fn is_base_connected(base: &str) -> bool {
    CONNECTED_BASES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .contains(base)
}

/// Called when `/api/event` drops (`Incoming::StreamDown`) so stale connected bases and remote
/// tunnels are cleared immediately.
pub fn on_sse_disconnected(app: &mut App, server_key: &ServerKey) {
    if let Some(srv) = live_server_clone(app) {
        clear_base_connected(&srv.base);
    }
    app.kernel
        .slots
        .get_mut(&BROWSER_RPC)
        .sse_connected
        .insert(server_key.clone(), false);
    tunnel::suspend_server_tunnels(app, server_key);
    drain_start_waiters(app);
}

/// Called when `/api/event` opens or reconnects (`Incoming::Opened` / `Incoming::Reconnected`).
pub fn on_sse_connected(app: &mut App, server_key: &ServerKey) {
    let srv = live_server_clone(app);
    if let Some(s) = &srv {
        mark_base_connected(&s.base);
    }
    let now = Instant::now();
    let mut pending_effects = Vec::new();
    {
        let st = app.kernel.slots.get_mut(&BROWSER_RPC);
        let was_connected = st.sse_connected.insert(server_key.clone(), true).is_some();
        let invalidate = was_connected
            || st.server_verdicts.get(server_key).is_some_and(|(prev_srv, _, v)| {
                matches!(v, ServerRpcVerdict::TransientError)
                    || srv.as_ref().is_some_and(|s| s.endpoint_key() != prev_srv.endpoint_key())
            });
        if invalidate {
            st.probe_gen = st.probe_gen.saturating_add(1);
            st.server_verdicts.remove(server_key);
        }
        for ((k, sid), att) in &mut st.sessions {
            if k == server_key {
                let effs = att.attach.step(AttachEvent::ServerConnected, now);
                if !effs.is_empty() {
                    pending_effects.push(((k.clone(), sid.clone()), effs));
                }
            }
        }
    }
    apply_batch_effects(app, pending_effects, None);
    maybe_start_capability_probe(app);
    sync_mcp_suppression(app);
}

/// Called when the server emits `plugin.updated` (`PRE-08` `#63500`): clears any cached server
/// capability verdict and immediately re-probes `Unavailable` / `Unsupported` / `Backoff`
/// attachments (or the server itself when no session is open) on `server_key`.
pub fn on_plugin_updated(app: &mut App, server_key: &ServerKey) {
    let now = Instant::now();
    let mut pending_effects = Vec::new();
    {
        let st = app.kernel.slots.get_mut(&BROWSER_RPC);
        st.probe_gen = st.probe_gen.saturating_add(1);
        st.server_verdicts.remove(server_key);
        for ((k, sid), att) in &mut st.sessions {
            if k == server_key {
                let effs = att.attach.step(AttachEvent::PluginUpdated, now);
                if !effs.is_empty() {
                    pending_effects.push(((k.clone(), sid.clone()), effs));
                }
            }
        }
    }
    apply_batch_effects(app, pending_effects, None);
    maybe_start_capability_probe(app);
    sync_mcp_suppression(app);
}

/// Routes an SSE `rpc.experimental.browser.control` frame to the owning attachment (`connectionID`).
pub fn on_control_event(app: &mut App, server_key: &ServerKey, data: &Value) {
    let Some(conn_id) = data.get("connectionID").and_then(Value::as_str) else { return };
    let now = Instant::now();
    let mut pending_effects = Vec::new();
    {
        let st = app.kernel.slots.get_mut(&BROWSER_RPC);
        for ((k, sid), att) in &mut st.sessions {
            if k == server_key && att.attach.connection_id() == Some(conn_id) {
                let frame = decode_control(conn_id, data);
                let effs = att.attach.step(AttachEvent::Control { frame }, now);
                if !effs.is_empty() {
                    pending_effects.push(((k.clone(), sid.clone()), effs));
                }
                break;
            }
        }
    }
    apply_batch_effects(app, pending_effects, None);
}

/// Called when `session.created`, `session.moved`, or `session.deleted` arrives over SSE.
pub fn on_session_lifecycle(app: &mut App, server_key: &ServerKey, ty: &str, session_id: &str) {
    let event = match ty {
        "session.created" => AttachEvent::SessionCreated,
        "session.moved" => AttachEvent::SessionMoved,
        "session.deleted" => AttachEvent::Dispose,
        _ => return,
    };
    let now = Instant::now();
    let key = (server_key.clone(), session_id.to_owned());
    let effs = {
        let st = app.kernel.slots.get_mut(&BROWSER_RPC);
        let Some(att) = st.sessions.get_mut(&key) else { return };
        att.attach.step(event, now)
    };
    if !effs.is_empty() {
        apply_batch_effects(app, vec![(key.clone(), effs)], None);
    }
    if ty == "session.deleted" {
        let removed = {
            let st = app.kernel.slots.get_mut(&BROWSER_RPC);
            st.routed_sessions.remove(&key);
            st.sessions.remove(&key)
        };
        if let Some(mut att) = removed {
            att.dispose_cleanup(app);
        }
    }
}

/// Main per-frame poll wired into `stage::REACT_PANELS` right after `browser.poll`.
pub fn poll(app: &mut App, tx: &mut Tx) -> bool {
    let now = Instant::now();
    let mut changed = false;

    // 1. Drain worker messages (attach outcomes & fetched commands).
    let msgs: Vec<WorkerMsg> = {
        let st = app.kernel.slots.get_mut(&BROWSER_RPC);
        st.worker_rx.try_iter().collect()
    };
    let mut pending_effects = Vec::new();
    let mut fetched_commands = Vec::new();
    for msg in msgs {
        changed = true;
        match msg {
            WorkerMsg::AttachEnded { server_key, session_id, attempt, outcome } => {
                let key = (server_key, session_id);
                let st = app.kernel.slots.get_mut(&BROWSER_RPC);
                if let Some(att) = st.sessions.get_mut(&key) {
                    let effs = att.attach.step(AttachEvent::AttachEnded { attempt, outcome }, now);
                    if !effs.is_empty() {
                        pending_effects.push((key, effs));
                    }
                }
            }
            WorkerMsg::PluginsListed { server_key, session_id, attempt, any } => {
                let key = (server_key, session_id);
                let st = app.kernel.slots.get_mut(&BROWSER_RPC);
                if let Some(att) = st.sessions.get_mut(&key) {
                    let effs = att.attach.step(AttachEvent::PluginsListed { attempt, any }, now);
                    if !effs.is_empty() {
                        pending_effects.push((key, effs));
                    }
                }
            }
            WorkerMsg::StreamActivated { server_key, session_id, attempt } => {
                let key = (server_key, session_id);
                let st = app.kernel.slots.get_mut(&BROWSER_RPC);
                if let Some(att) = st.sessions.get_mut(&key) {
                    let _ = att.attach.step(AttachEvent::StreamActivated { attempt }, now);
                }
            }
            WorkerMsg::StreamEnded { server_key, session_id, attempt, replaced, reason } => {
                let key = (server_key, session_id);
                let st = app.kernel.slots.get_mut(&BROWSER_RPC);
                if let Some(att) = st.sessions.get_mut(&key) {
                    let effs = att.attach.step(AttachEvent::StreamEnded { attempt, replaced, reason }, now);
                    if !effs.is_empty() {
                        pending_effects.push((key, effs));
                    }
                }
            }
            WorkerMsg::CommandFetched { server_key, session_id, connection_id, request_id, result } => {
                fetched_commands.push(((server_key, session_id), connection_id, request_id, result));
            }
            WorkerMsg::ProbeEnded { server_key, server, generation, supported } => {
                let st = app.kernel.slots.get_mut(&BROWSER_RPC);
                if st
                    .server_verdicts
                    .get(&server_key)
                    .is_some_and(|(_, g, v)| *g == generation && matches!(v, ServerRpcVerdict::Probing))
                {
                    let verdict = match supported {
                        Some(true) => ServerRpcVerdict::Supported,
                        Some(false) => ServerRpcVerdict::LegacyAllowed,
                        None => ServerRpcVerdict::TransientError,
                    };
                    st.server_verdicts.insert(server_key, (server, generation, verdict));
                }
            }
        }
    }
    apply_batch_effects(app, pending_effects, None);

    // 2. Reconcile open sessions against the active live server.
    changed |= reconcile_sessions(app, now);

    // 3. Route CDP events into diagnostics, frames, and active trace/heap buffers.
    if app.workspace_panes.browser.host.has_cdp_events() {
        let events: Vec<_> = app.workspace_panes.browser.host.drain_cdp_events().collect();
        let mut oopif_enables: Vec<(u64, String)> = Vec::new();
        let mut oopif_detaches: Vec<(u64, String)> = Vec::new();
        {
            let st = app.kernel.slots.get_mut(&BROWSER_RPC);
            for ev in events {
                for att in st.sessions.values_mut() {
                    if let Some(diag) = att.diagnostics.get_mut(&ev.tab_id) {
                        diag.ingest_cdp_event(&ev.method, &ev.params, ev.session_id.as_deref());
                    }
                    let mut main_nav = false;
                    if let Some(fr) = att.frames.get_mut(&ev.tab_id) {
                        let eff = fr.ingest_cdp_event(&ev.method, &ev.params, ev.session_id.as_deref());
                        for child_sid in eff.enable_sessions {
                            oopif_enables.push((ev.tab_id, child_sid));
                        }
                        for ext_sid in eff.detach_sessions {
                            oopif_detaches.push((ev.tab_id, ext_sid));
                        }
                        main_nav = eff.main_navigated;
                    }
                    if main_nav && att.host_to_wire.contains_key(&ev.tab_id) {
                        att.bump_generation(ev.tab_id);
                    }
                    if ev.method == "Input.dragIntercepted"
                        && let Some(data) = ev.params.get("data")
                    {
                        exec::ingest_drag_intercepted(att, ev.tab_id, data.clone());
                    }
                    exec::profile::ingest_cdp_event(att, &ev);
                }
            }
        }
        if app.workspace_panes.browser.host.engine_agent_tools() {
            let host = &mut app.workspace_panes.browser.host;
            let engine_auth = host.engine_frame_authority();
            if !engine_auth {
                for (tab_id, ext_sid) in oopif_detaches {
                    let _ = host.send_typed_cdp(tab_id, "Target.detachFromTarget", serde_json::json!({ "sessionId": ext_sid }), None);
                }
            }
            let mut oopif_frame_reqs: Vec<(u64, u64, String)> = Vec::new();
            let auto_attach = serde_json::json!({ "autoAttach": true, "waitForDebuggerOnStart": false, "flatten": true });
            for (tab_id, child_sid) in oopif_enables {
                if !engine_auth {
                    let _ = host.send_typed_cdp(tab_id, "Page.enable", serde_json::json!({}), Some(child_sid.clone()));
                    let _ = host.send_typed_cdp(tab_id, "Target.setAutoAttach", auto_attach.clone(), Some(child_sid.clone()));
                }
                let _ = host.send_typed_cdp(tab_id, "Runtime.enable", serde_json::json!({}), Some(child_sid.clone()));
                // An OOPIF's console and requests, as official enables them per session (`diagnostics.ts:231-238`).
                let _ = host.send_typed_cdp(tab_id, "Log.enable", serde_json::json!({}), Some(child_sid.clone()));
                let net = serde_json::json!({ "maxTotalBufferSize": 5_242_880, "maxResourceBufferSize": 1_048_576, "maxPostDataSize": 20_000 });
                let _ = host.send_typed_cdp(tab_id, "Network.enable", net, Some(child_sid.clone()));
                if let Ok(req_id) = host.send_typed_cdp(tab_id, "Page.getFrameTree", serde_json::json!({}), Some(child_sid.clone())) {
                    oopif_frame_reqs.push((tab_id, req_id, child_sid));
                }
            }
            if !oopif_frame_reqs.is_empty() {
                let st = app.kernel.slots.get_mut(&BROWSER_RPC);
                for (tab_id, req_id, child_sid) in oopif_frame_reqs {
                    for att in st.sessions.values_mut() {
                        if att.host_to_wire.contains_key(&tab_id) {
                            att.bootstrap_frame_reqs.push((tab_id, req_id, Some(child_sid.clone())));
                            break;
                        }
                    }
                }
            }
        }
    }

    // 4. Start newly fetched commands & step in-flight commands without blocking the UI thread.
    // Move `sessions` temporarily out of `Slot` so executors can mutably borrow `App`.
    let (mut sessions, mut holds, mut ref_counter, waker) = {
        let st = app.kernel.slots.get_mut(&BROWSER_RPC);
        (std::mem::take(&mut st.sessions), std::mem::take(&mut st.holds), st.ref_counter, st.waker.clone())
    };

    for (key, conn_id, req_id, res) in fetched_commands {
        if let Some(att) = sessions.get_mut(&key) {
            att.fetch_aborts.remove(&req_id);
            match res {
                Ok(cmd) => exec::start_command(att, app, conn_id, req_id, cmd, now, waker.clone(), tx),
                Err(RpcCallError::InvalidInput(msg)) => {
                    if att.attach.is_request_in_flight(&conn_id, &req_id) {
                        att.enqueue_result(
                            &conn_id,
                            req_id.clone(),
                            Outcome::fail(failure_code::UNSUPPORTED, msg),
                        );
                        let _ = att.attach.step(AttachEvent::CommandFinished { request_id: req_id }, now);
                    }
                }
                Err(_) => {
                    let _ = att.attach.step(AttachEvent::CommandFinished { request_id: req_id }, now);
                }
            }
        }
    }

    let mut tick_effects = Vec::new();
    let mut earliest_wake: Option<Instant> = None;
    for (key, att) in &mut sessions {
        if att.drain_bootstrap_frames(app) {
            let retry_at = now + std::time::Duration::from_millis(25);
            earliest_wake = Some(earliest_wake.map_or(retry_at, |cur| cur.min(retry_at)));
        }
        // Cleanly close any cancelled `close_on_fail` tabs queued during SSE `on_control_event`
        // (eliminating ghost tabs in `session_tabs` and side-panel UI).
        if !att.pending_close_tabs.is_empty() {
            let sid = att.session_id.clone();
            for hid in std::mem::take(&mut att.pending_close_tabs) {
                app.close_browser_tab_for_session(&sid, hid, tx);
            }
            changed = true;
        }
        let (cmd_changed, cmd_wake) =
            exec::poll_commands(att, app, now, &mut ref_counter, waker.clone(), tx);
        changed |= cmd_changed;
        if let Some(w) = cmd_wake {
            earliest_wake = Some(earliest_wake.map_or(w, |cur| cur.min(w)));
        }
        if let Some(w) = exec::profile::poll_auto_stops(att, app, now) {
            earliest_wake = Some(earliest_wake.map_or(w, |cur| cur.min(w)));
        }
        // Sync tab inventory changes while attached & bootstrap OOPIF/context CDP tracking.
        if att.attach.is_attached() {
            let (inv, hash) = att.sync_tabs(app);
            if app.workspace_panes.browser.host.engine_agent_tools() {
                let to_boot: Vec<u64> = att
                    .host_to_wire
                    .keys()
                    .copied()
                    .filter(|hid| !att.cdp_bootstrapped.contains(hid))
                    .collect();
                for hid in to_boot {
                    att.ensure_tab_cdp(app, hid);
                }
            }
            let effs = att.attach.step(AttachEvent::StateChanged { hash }, now);
            if effs.iter().any(|e| matches!(e, AttachEffect::SendState { .. }))
                && let Some(cid) = att.attach.connection_id().map(str::to_owned)
            {
                att.enqueue_state(&cid, inv, hash);
            }
        }
        // Step attachment deadline / backoff timers.
        if let Some(deadline) = att.attach.next_deadline() {
            if now >= deadline {
                let effs = att.attach.step(AttachEvent::Tick, now);
                if !effs.is_empty() {
                    tick_effects.push((key.clone(), effs));
                    changed = true;
                }
            } else {
                earliest_wake = Some(earliest_wake.map_or(deadline, |cur| cur.min(deadline)));
            }
        }
    }

    if let Some(w) = holds.sync(app, &mut sessions, now) {
        earliest_wake = Some(earliest_wake.map_or(w, |cur| cur.min(w)));
    }
    {
        let st = app.kernel.slots.get_mut(&BROWSER_RPC);
        st.sessions = sessions;
        st.holds = holds;
        st.ref_counter = ref_counter;
    }
    apply_batch_effects(app, tick_effects, Some(tx));

    if let Some(at) = earliest_wake {
        // Never spin the UI thread at 0 ms if a deadline just arrived.
        let delay = at
            .saturating_duration_since(now)
            .max(std::time::Duration::from_millis(10));
        tx.wake_after(delay);
    }
    changed
}

fn reconcile_sessions(app: &mut App, now: Instant) -> bool {
    if !app.live() || app.browser_unsupported() {
        return false;
    }
    let Some(server) = live_server_clone(app) else {
        return false;
    };
    let active_server = crate::servers::runtime::active(app).clone();

    // Match official (`packages/gui-plugins/src/browser/model.ts`, `machine.ts`): a session attaches
    // when it is routed on screen or runs in a tab nobody has opened, and stays attached while it
    // stays open. Skip new-session starts until `POST /api/session` returns (`created_starts`).
    let created_starts = {
        let st = app.kernel.slots.get_mut(&BROWSER_RPC);
        st.created_starts.retain(|(_, sid)| app.queue.starts.has(sid));
        st.created_starts.clone()
    };
    let pending_create = |sid: &str| {
        app.queue.starts.has(sid) && !created_starts.contains(&(active_server.clone(), sid.to_owned()))
    };
    let mut open_dirs: BTreeMap<(ServerKey, String), String> = BTreeMap::new();
    for t in &app.tabs {
        if !t.is_draft() && t.server == active_server && !pending_create(&t.id) {
            let dir = session_directory(app, &t.id, &t.directory);
            if !dir.is_empty() {
                open_dirs.insert((active_server.clone(), t.id.clone()), dir);
            }
        }
    }
    let mut routed_now: Option<(ServerKey, String)> = None;
    if let Route::Session { server: rk, id } = &*app.route
        && *rk == active_server
        && !pending_create(id)
    {
        let key = (active_server.clone(), id.clone());
        let dir = session_directory(app, id, "");
        if !dir.is_empty() {
            open_dirs.insert(key.clone(), dir);
            routed_now = Some(key);
        } else if open_dirs.contains_key(&key) {
            routed_now = Some(key);
        }
    }

    comes_back::keep_subagents(app, &active_server, &mut open_dirs);
    let (unopened, released) = (comes_back::unopened(app, &open_dirs), comes_back::released(app));
    let engine_known = app.workspace_panes.browser.host.engine_caps_known();
    let engine_remote_proxy = app.workspace_panes.browser.host.engine_remote_proxy();
    let mut changed = false;
    let mut pending_effects = Vec::new();
    let mut disposed = Vec::new();
    {
        let st = app.kernel.slots.get_mut(&BROWSER_RPC);
        let came = comes_back::came_on_screen(st, routed_now.as_ref());
        st.routed_sessions.extend(routed_now.into_iter().chain(unopened));
        st.routed_sessions
            .retain(|k| k.0 != active_server || open_dirs.contains_key(k));
        let wanted: BTreeMap<(ServerKey, String), String> = open_dirs
            .into_iter()
            .filter(|(k, _)| st.routed_sessions.contains(k))
            .collect();
        let sse_ready = st.sse_connected.get(&active_server).copied().unwrap_or(false)
            || is_base_connected(&server.base);
        let existing_keys: BTreeSet<(ServerKey, String)> = st.sessions.keys().cloned().collect();
        for key in existing_keys {
            if !wanted.contains_key(&key)
                && let Some(mut old) = st.sessions.remove(&key)
            {
                let _ = old.attach.step(AttachEvent::Dispose, now);
                disposed.push(old);
                changed = true;
            }
        }
        for (key, dir) in wanted {
            let same_machine = key.0.is_sidecar() && server.is_local();
            let scope_seed = {
                st.next_diag_scope = st.next_diag_scope.saturating_add(1);
                st.next_diag_scope
            };
            let att = st.sessions.entry(key.clone()).or_insert_with(|| {
                changed = true;
                SessionAttachment::new(
                    key.0.clone(),
                    key.1.clone(),
                    LocationRef::new(dir.clone(), None),
                    server.clone(),
                    scope_seed,
                )
            });
            if att.location.directory != dir {
                att.location.directory = dir;
                let effs = att.attach.step(AttachEvent::SessionMoved, now);
                if !effs.is_empty() {
                    pending_effects.push((key.clone(), effs));
                }
            }
            if !same_machine && engine_known && !engine_remote_proxy {
                // Fail-closed security rule: never attach to remote servers when the running
                // engine lacks `Ready.remote_proxy` confinement.
                let effs = att.attach.step(AttachEvent::RequireRemoteProxy, now);
                if !effs.is_empty() {
                    pending_effects.push((key, effs));
                }
            } else if sse_ready {
                let effs = comes_back::wake(&mut att.attach, came.as_ref() == Some(&key), released.contains(&key), now);
                if !effs.is_empty() {
                    pending_effects.push((key, effs));
                }
            }
        }
    }
    for mut old in disposed {
        old.dispose_cleanup(app);
    }
    apply_batch_effects(app, pending_effects, None);
    maybe_start_capability_probe(app);
    sync_mcp_suppression(app);
    tunnel::sync_remote_tunnels(app);
    drain_start_waiters(app);
    changed
}

fn apply_batch_effects(
    app: &mut App,
    batch: Vec<((ServerKey, String), Vec<AttachEffect>)>,
    mut tx_opt: Option<&mut Tx>,
) {
    if batch.is_empty() {
        return;
    }
    let (worker_tx, waker) = {
        let st = app.kernel.slots.get(&BROWSER_RPC);
        (st.worker_tx.clone(), app.workspace_panes.browser.host.message_waker().or_else(|| st.waker.clone()))
    };
    let mut sessions = std::mem::take(&mut app.kernel.slots.get_mut(&BROWSER_RPC).sessions);
    for (key, effects) in batch {
        let Some(att) = sessions.get_mut(&key) else { continue };
        for effect in effects {
            match effect {
                AttachEffect::StartAttach { attempt, connection_id } => {
                    workers::attach(att, attempt, connection_id, worker_tx.clone(), waker.clone());
                }
                // A Location the server has only just started lists no plugin until its plugins
                // have loaded (`pane.ts` `loaded`, opencode#65720).
                AttachEffect::ListPlugins { attempt } => workers::list_plugins(att, attempt, worker_tx.clone(), waker.clone()),
                AttachEffect::AbortAttach { attempt } => {
                    if att.attach_abort.as_ref().is_some_and(|(a, _)| *a == attempt)
                        && let Some((_, abort)) = att.attach_abort.take()
                    {
                        abort.abort();
                    }
                    att.stop_outbound();
                }
                AttachEffect::SendState { connection_id } => {
                    let (inv, hash) = att.sync_tabs(app);
                    att.enqueue_state(&connection_id, inv, hash);
                }
                AttachEffect::FetchCommand { connection_id, request_id } => {
                    workers::fetch_command(att, connection_id, request_id, worker_tx.clone(), waker.clone());
                }
                AttachEffect::CancelCommands { request_ids } => {
                    for id in &request_ids {
                        att.mark_cancelled(id);
                    }
                    let cmds = std::mem::take(&mut att.active_commands);
                    let mut kept = Vec::with_capacity(cmds.len());
                    for mut cmd in cmds {
                        if request_ids.iter().any(|id| id == &cmd.request_id) {
                            cmd.cancel_cleanup(att, app, tx_opt.as_deref_mut());
                        } else {
                            kept.push(cmd);
                        }
                    }
                    att.active_commands = kept;
                }
                AttachEffect::ActivateTunnel { .. }
                | AttachEffect::ScheduleTimer { .. }
                | AttachEffect::SettlePromptGates
                | AttachEffect::SetMcpSuppressed { .. } => {}
            }
        }
    }
    app.kernel.slots.get_mut(&BROWSER_RPC).sessions = sessions;
    sync_mcp_suppression(app);
    tunnel::sync_remote_tunnels(app);
    drain_start_waiters(app);
}

pub(super) fn live_server_clone(app: &App) -> Option<Server> {
    match &*app.source {
        Source::Live { server, .. } => Some(server.clone()),
        Source::Fixture => None,
    }
}

fn session_directory(app: &App, sid: &str, fallback: &str) -> String {
    if let Some(info) = app.session(sid)
        && !info.directory.is_empty()
    {
        return info.directory.clone();
    }
    // Wait until the session's real directory is known (`prompt-gate.ts:10`: `session.location !== undefined`)
    // instead of attaching under `$HOME` before `Incoming::Sessions` arrives.
    fallback.to_owned()
}
