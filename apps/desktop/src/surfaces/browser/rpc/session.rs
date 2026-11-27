//! Per-session `experimental.browser` v4 attachment state, bijective `tab_<uuid> <-> host_id`
//! isolation, per-tab element refs (with OOPIF session tracking), diagnostics, bounded captures,
//! and bounded ordered FIFO outbound worker.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use browser_pane::tools::diagnostics::TabDiagnostics;
use opencode_kit::browser_rpc::{
    AbortHandle, AttachMachine, BrowserState, LocationRef, Outcome, RpcCallError, Tab,
    failure_code, new_file_id, new_tab_id,
};
use opencode_kit::client::Server;
use opencode_kit::servers::ServerKey;
use rune::time::Instant;
use serde_json::Value;

use super::exec::ActiveCommand;
use super::frames::TabFrameState;
use crate::App;

const STATE_RETRY_MIN_MS: u64 = 250;
const STATE_RETRY_MAX_MS: u64 = 10_000;
const MAX_OUTBOUND_QUEUE: usize = 128;
const MAX_CANCELLED_IDS: usize = 256;
const MAX_SESSION_CAPTURE_BYTES: usize = 32 * 1024 * 1024;

/// Messages sent from the UI thread to one attachment's ordered FIFO HTTP worker.
pub(super) enum OutboundMsg {
    State(BrowserState),
    Result { request_id: String, outcome: Outcome },
    Stop,
}

mod refs;
pub(super) use refs::TabRefs;


/// One desktop-owned capture artifact owned by a single tab inside a single session.
#[derive(Clone)]
pub(super) struct CapturedFile {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
    pub resources: Vec<String>,
}

/// Hard cap on distinct URLs accumulated into one active trace/CPU recording before failing closed.
const MAX_RECORDING_RESOURCES: usize = 256;

/// In-flight or auto-stopped Chromium performance trace for one tab (`profiling-machine.ts`).
pub(super) struct TraceRecording {
    pub host_id: u64,
    pub key: String,
    pub started_at: Instant,
    pub duration_ms: u64,
    pub stop_at: Option<Instant>,
    pub renderer_pid: Option<u64>,
    pub events: Vec<Value>,
    pub resources: Vec<String>,
    pub resources_overflow: bool,
    pub complete: bool,
    pub data_loss: bool,
    pub auto_stop_req: Option<u64>,
    pub auto_stop_ended: bool,
    pub auto_stop_deadline: Option<Instant>,
    pub auto_stopped: Option<Result<(String, f64, bool), String>>,
}

/// In-flight or auto-stopped V8 CPU profile for one tab (`CPU_MAX_DURATION_MS = 30 s`).
pub(super) struct CpuRecording {
    pub key: String,
    pub started_at: Instant,
    pub stop_at: Option<Instant>,
    pub resources: Vec<String>,
    pub resources_overflow: bool,
    pub auto_stop_req: Option<u64>,
    pub auto_stop_deadline: Option<Instant>,
    pub auto_stopped: Option<Result<(String, f64), String>>,
}

fn push_recording_url(resources: &mut Vec<String>, overflow: &mut bool, url: &str) {
    let trimmed = url.trim();
    if trimmed.is_empty() || resources.iter().any(|u| u == trimmed) {
        return;
    }
    if resources.len() >= MAX_RECORDING_RESOURCES {
        *overflow = true;
        return;
    }
    resources.push(trimmed.to_owned());
}

/// Complete per-session state for one `experimental.browser` v4 attachment.
pub(super) struct SessionAttachment {
    pub server_key: ServerKey,
    pub session_id: String,
    pub location: LocationRef,
    pub server: Server,
    pub attach: AttachMachine,
    pub attach_abort: Option<(u64, AbortHandle)>,
    pub outbound: Option<(String, SyncSender<OutboundMsg>, AbortHandle, Arc<Mutex<VecDeque<String>>>)>,
    pub fetch_aborts: HashMap<String, AbortHandle>,
    pub wire_to_host: BTreeMap<String, u64>,
    pub host_to_wire: BTreeMap<u64, String>,
    pub generations: HashMap<u64, u64>,
    pub last_urls: HashMap<u64, String>,
    pub refs: HashMap<u64, TabRefs>,
    pub frames: HashMap<u64, TabFrameState>,
    pub cdp_bootstrapped: HashSet<u64>,
    pub bootstrap_frame_reqs: Vec<(u64, u64, Option<String>)>,
    pub diagnostics: HashMap<u64, TabDiagnostics>,
    pub captures: HashMap<u64, Vec<CapturedFile>>,
    pub download_wire_ids: HashMap<(u64, String), String>,
    pub trace_recording: Option<TraceRecording>,
    pub cpu_started: HashMap<u64, CpuRecording>,
    pub heap_chunks: HashMap<u64, (String, bool)>,
    pub download_origins: HashMap<(u64, String), Vec<String>>,
    pub pending_close_tabs: Vec<u64>,
    pub agent_tab_id: Option<String>,
    pub active_commands: Vec<ActiveCommand>,
    pub hold_touched: Vec<u64>,
    pub diag_scope_seed: u64,
}

impl SessionAttachment {
    pub fn new(
        server_key: ServerKey,
        session_id: String,
        location: LocationRef,
        server: Server,
        diag_scope_seed: u64,
    ) -> Self {
        Self {
            server_key,
            session_id,
            location,
            server,
            attach: AttachMachine::new(),
            attach_abort: None,
            outbound: None,
            fetch_aborts: HashMap::new(),
            wire_to_host: BTreeMap::new(),
            host_to_wire: BTreeMap::new(),
            generations: HashMap::new(),
            last_urls: HashMap::new(),
            refs: HashMap::new(),
            frames: HashMap::new(),
            cdp_bootstrapped: HashSet::new(),
            bootstrap_frame_reqs: Vec::new(),
            diagnostics: HashMap::new(),
            captures: HashMap::new(),
            download_wire_ids: HashMap::new(),
            trace_recording: None,
            cpu_started: HashMap::new(),
            heap_chunks: HashMap::new(),
            download_origins: HashMap::new(),
            pending_close_tabs: Vec::new(),
            agent_tab_id: None,
            active_commands: Vec::new(),
            hold_touched: Vec::new(),
            diag_scope_seed,
        }
    }

    pub fn stop_outbound(&mut self) {
        if let Some((_, tx, abort, _)) = self.outbound.take() {
            abort.abort();
            let _ = tx.try_send(OutboundMsg::Stop);
        }
        for (_, abort) in self.fetch_aborts.drain() {
            abort.abort();
        }
    }

    /// Ends any active commands, traces, and CPU/heap profiling on session disposal so Chromium's
    /// browser-wide tracing controller never stays wedged.
    pub fn dispose_cleanup(&mut self, app: &mut App) {
        let cmds = std::mem::take(&mut self.active_commands);
        for mut cmd in cmds {
            cmd.cancel_cleanup(self, app, None);
        }
        if let Some(rec) = self.trace_recording.take()
            && rec.auto_stopped.is_none()
        {
            super::exec::profile::abort_trace(app, rec.host_id);
        }
        for (hid, rec) in self.cpu_started.drain() {
            if rec.auto_stopped.is_none() && app.workspace_panes.browser.host.tab_open_in_engine(hid) {
                let _ = app.workspace_panes.browser.host.send_typed_cdp(hid, "Profiler.stop", serde_json::json!({}), None);
                let _ = app.workspace_panes.browser.host.send_typed_cdp(hid, "Profiler.disable", serde_json::json!({}), None);
            }
        }
        for (hid, _) in self.heap_chunks.drain() {
            if app.workspace_panes.browser.host.tab_open_in_engine(hid) {
                let _ = app.workspace_panes.browser.host.send_typed_cdp(hid, "HeapProfiler.disable", serde_json::json!({}), None);
            }
        }
        self.stop_outbound();
        if let Some((_, abort)) = self.attach_abort.take() {
            abort.abort();
        }
    }

    pub fn ensure_outbound(&mut self, connection_id: &str) {
        if self.outbound.as_ref().is_some_and(|(cid, _, _, _)| cid == connection_id) {
            return;
        }
        self.stop_outbound();
        let (tx, rx) = mpsc::sync_channel(MAX_OUTBOUND_QUEUE);
        let abort = AbortHandle::new();
        let cancelled = Arc::new(Mutex::new(VecDeque::new()));
        spawn_outbound_worker(
            self.server.clone(),
            self.location.clone(),
            self.session_id.clone(),
            connection_id.to_owned(),
            rx,
            abort.clone(),
            cancelled.clone(),
        );
        self.outbound = Some((connection_id.to_owned(), tx, abort, cancelled));
    }

    /// FIFO-evicting cancellation tracker so a 256th cancel never clears the 255th entry.
    pub fn mark_cancelled(&mut self, request_id: &str) {
        if let Some(abort) = self.fetch_aborts.remove(request_id) {
            abort.abort();
        }
        if let Some((_, _, _, cancelled)) = &self.outbound {
            let mut ring = cancelled.lock().unwrap_or_else(PoisonError::into_inner);
            if !ring.iter().any(|r| r == request_id) {
                if ring.len() >= MAX_CANCELLED_IDS {
                    ring.pop_front();
                }
                ring.push_back(request_id.to_owned());
            }
        }
    }

    pub fn enqueue_state(&mut self, connection_id: &str, state: BrowserState, hash: u64) {
        self.ensure_outbound(connection_id);
        self.attach.mark_state_published(hash);
        if let Some((_, tx, _, _)) = &self.outbound {
            let _ = tx.try_send(OutboundMsg::State(state));
        }
    }

    pub fn enqueue_result(&mut self, connection_id: &str, request_id: String, outcome: Outcome) {
        self.ensure_outbound(connection_id);
        if let Some((_, tx, _, _)) = &self.outbound {
            let _ = tx.try_send(OutboundMsg::Result { request_id, outcome });
        }
    }

    pub fn bump_generation(&mut self, host_id: u64) {
        let entry = self.generations.entry(host_id).or_insert(1);
        *entry = entry.saturating_add(1);
        if let Some(r) = self.refs.get_mut(&host_id) {
            r.clear_on_nav();
        }
    }

    /// Accumulates a frame/navigation URL into any active (not yet stopped) trace or CPU
    /// recording on `host_id` so `trace.stop`/`cpu.stop` authorize against every visited URL
    /// (`packages/gui-plugins/src/browser/profiling.ts:295`).
    pub fn record_recording_url(&mut self, host_id: u64, url: &str) {
        if let Some(rec) = self.trace_recording.as_mut()
            && rec.host_id == host_id
            && rec.auto_stopped.is_none()
        {
            push_recording_url(&mut rec.resources, &mut rec.resources_overflow, url);
        }
        if let Some(rec) = self.cpu_started.get_mut(&host_id)
            && rec.auto_stopped.is_none()
        {
            push_recording_url(&mut rec.resources, &mut rec.resources_overflow, url);
        }
    }

    /// Bootstraps `Page.enable`, `Runtime.enable`, `Target.setAutoAttach({ flatten: true })`, AND
    /// an initial `Page.getFrameTree` once per tab so `main_frame_id` and pre-existing frames are
    /// populated immediately.
    pub fn ensure_tab_cdp(&mut self, app: &mut App, host_id: u64) {
        if self.cdp_bootstrapped.contains(&host_id)
            || !app.workspace_panes.browser.host.engine_agent_tools()
            || !app.workspace_panes.browser.host.tab_open_in_engine(host_id)
        {
            return;
        }
        let host = &mut app.workspace_panes.browser.host;
        let _ = host.send_typed_cdp(host_id, "Page.enable", serde_json::json!({}), None);
        let _ = host.send_typed_cdp(host_id, "Runtime.enable", serde_json::json!({}), None);
        let _ = host.send_typed_cdp(host_id, "Log.enable", serde_json::json!({}), None);
        let _ = host.send_typed_cdp(
            host_id,
            "Network.enable",
            serde_json::json!({
                "maxTotalBufferSize": 5_242_880,
                "maxResourceBufferSize": 1_048_576,
                "maxPostDataSize": 20_000,
            }),
            None,
        );
        if !host.engine_frame_authority() {
            let _ = host.send_typed_cdp(
                host_id,
                "Target.setAutoAttach",
                serde_json::json!({
                    "autoAttach": true,
                    "waitForDebuggerOnStart": false,
                    "flatten": true,
                }),
                None,
            );
        }
        if let Ok(req_id) = host.send_typed_cdp(host_id, "Page.getFrameTree", serde_json::json!({}), None) {
            self.bootstrap_frame_reqs.push((host_id, req_id, None));
        }
        self.cdp_bootstrapped.insert(host_id);
    }

    /// Drains any completed bootstrap `Page.getFrameTree` responses (top-level and OOPIF) into `self.frames`.
    /// Returns `true` when an isolated context was still initializing and should retry bootstrap.
    pub fn drain_bootstrap_frames(&mut self, app: &mut App) -> bool {
        let mut retry_needed = false;
        let pending = std::mem::take(&mut self.bootstrap_frame_reqs);
        let mut remaining = Vec::new();
        for (host_id, req_id, sid) in pending {
            if let Some(resp) = app.workspace_panes.browser.host.take_cdp_response(req_id) {
                match resp {
                    Ok(val) => {
                        if self.frames.entry(host_id).or_default().ingest_frame_tree(&val, sid.as_deref()) {
                            self.bump_generation(host_id);
                        }
                    }
                    Err(err)
                        if sid.is_none()
                            && matches!(
                                err.get("message").and_then(serde_json::Value::as_str),
                                Some("browser.context.initializing" | "browser.proxy.required")
                            ) =>
                    {
                        self.cdp_bootstrapped.remove(&host_id);
                        retry_needed = true;
                    }
                    Err(_) => {}
                }
            } else {
                remaining.push((host_id, req_id, sid));
            }
        }
        self.bootstrap_frame_reqs = remaining;
        retry_needed
    }

    /// Returns all known document URLs in `host_id` (main document + all in-process & OOPIF
    /// subframes). Fails closed if the 256-frame tracking cap overflowed.
    pub fn source_urls(&self, app: &App, host_id: u64) -> Result<Vec<String>, String> {
        let main_url = app
            .workspace_panes
            .browser
            .host
            .tab(host_id)
            .map(|t| t.url.as_str())
            .unwrap_or("");
        match self.frames.get(&host_id) {
            Some(f) => f.source_urls(main_url),
            None if !main_url.is_empty() => Ok(vec![main_url.to_owned()]),
            None => Ok(Vec::new()),
        }
    }

    pub fn wire_download_id(&mut self, host_id: u64, engine_id: &str) -> String {
        self.download_wire_ids
            .entry((host_id, engine_id.to_owned()))
            .or_insert_with(new_file_id)
            .clone()
    }

    pub fn resolve_download_engine_id(&self, host_id: u64, wire_id: &str) -> Option<String> {
        self.download_wire_ids
            .iter()
            .find(|((hid, _), wid)| *hid == host_id && wid.as_str() == wire_id)
            .map(|((_, eid), _)| eid.clone())
    }

    /// Returns the download's origin URL(s), preferring the engine-recorded `DownloadFile.source_url`
    /// captured in `on_before_download` before any post-download redirect.
    pub fn download_resources(&self, app: &App, host_id: u64, wire_id: &str) -> Result<Vec<String>, String> {
        if let Some(eid) = self.resolve_download_engine_id(host_id, wire_id) {
            if let Some(src) = app
                .workspace_panes
                .browser
                .host
                .tab(host_id)
                .and_then(|t| t.files.iter().find(|f| f.id == eid))
                .and_then(|f| f.source_url.as_ref())
                .filter(|u| !u.is_empty())
            {
                return Ok(vec![src.clone()]);
            }
            if let Some(urls) = self.download_origins.get(&(host_id, eid))
                && !urls.is_empty()
            {
                return Ok(urls.clone());
            }
        }
        self.source_urls(app, host_id)
    }

    /// Reconciles this session's `tab_<uuid> <-> host_tab_id` map against `BrowserHost` strictly
    /// scoped to `self.session_id`, excluding `ContextKind::Private` tabs.
    pub fn sync_tabs(&mut self, app: &mut App) -> (BrowserState, u64) {
        let host_ids: Vec<u64> = app
            .workspace_panes
            .browser
            .session_tabs
            .get(&self.session_id)
            .map(|ids| {
                ids.iter()
                    .copied()
                    .filter(|&id| {
                        app.workspace_panes
                            .browser
                            .host
                            .tab(id)
                            .is_some_and(|t| !matches!(t.context, browser_pane::ContextKind::Private))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let stale: Vec<u64> = self
            .host_to_wire
            .keys()
            .copied()
            .filter(|id| !host_ids.contains(id))
            .collect();
        for host_id in stale {
            if let Some(wire) = self.host_to_wire.remove(&host_id) {
                self.wire_to_host.remove(&wire);
                if self.agent_tab_id.as_deref() == Some(wire.as_str()) {
                    self.agent_tab_id = None;
                }
            }
            self.generations.remove(&host_id);
            self.last_urls.remove(&host_id);
            self.refs.remove(&host_id);
            self.frames.remove(&host_id);
            self.cdp_bootstrapped.remove(&host_id);
            self.bootstrap_frame_reqs.retain(|(h, _, _)| *h != host_id);
            self.diagnostics.remove(&host_id);
            self.captures.remove(&host_id);
            self.download_wire_ids.retain(|(hid, _), _| *hid != host_id);
            if let Some(rec) = self.cpu_started.remove(&host_id)
                && rec.auto_stopped.is_none()
                && app.workspace_panes.browser.host.tab_open_in_engine(host_id)
            {
                let _ = app.workspace_panes.browser.host.send_typed_cdp(host_id, "Profiler.stop", serde_json::json!({}), None);
                let _ = app.workspace_panes.browser.host.send_typed_cdp(host_id, "Profiler.disable", serde_json::json!({}), None);
            }
            self.heap_chunks.remove(&host_id);
            if self.trace_recording.as_ref().is_some_and(|r| r.host_id == host_id)
                && let Some(rec) = self.trace_recording.take()
                && rec.auto_stopped.is_none()
            {
                super::exec::profile::abort_trace(app, host_id);
            }
            super::exec::files::cleanup_tab_uploads(app, host_id);
            self.download_origins.retain(|(hid, _), _| *hid != host_id);
        }

        let mut tabs = Vec::with_capacity(host_ids.len());
        for host_id in &host_ids {
            let wire_id = self
                .host_to_wire
                .entry(*host_id)
                .or_insert_with(|| {
                    let wid = new_tab_id();
                    self.wire_to_host.insert(wid.clone(), *host_id);
                    wid
                })
                .clone();
            let Some(tab) = app.workspace_panes.browser.host.tab(*host_id) else { continue };
            let url_changed = self
                .last_urls
                .get(host_id)
                .is_some_and(|prev| prev != &tab.url && !tab.url.is_empty());
            if url_changed {
                self.bump_generation(*host_id);
                self.record_recording_url(*host_id, &tab.url);
            }
            if !self.last_urls.contains_key(host_id) || url_changed {
                self.last_urls.insert(*host_id, tab.url.clone());
            }
            let generation = *self.generations.entry(*host_id).or_insert(1);
            self.diagnostics
                .entry(*host_id)
                .or_insert_with(|| TabDiagnostics::new(self.diag_scope_seed ^ host_id));
            self.frames.entry(*host_id).or_default();
            for f in &tab.files {
                let key = (*host_id, f.id.clone());
                self.download_wire_ids.entry(key.clone()).or_insert_with(new_file_id);
                if !self.download_origins.contains_key(&key) {
                    let mut origins = if let Some(src) = f.source_url.as_ref().filter(|u| !u.is_empty()) {
                        vec![src.clone()]
                    } else {
                        self.frames
                            .get(host_id)
                            .and_then(|fr| fr.source_urls(&tab.url).ok())
                            .unwrap_or_else(|| if tab.url.is_empty() { Vec::new() } else { vec![tab.url.clone()] })
                    };
                    origins.sort();
                    origins.dedup();
                    self.download_origins.insert(key, origins);
                }
            }
            tabs.push(build_wire_tab(&wire_id, tab, generation));
        }

        let active_wire = app
            .active_browser_tab_for_session(&self.session_id)
            .and_then(|hid| self.host_to_wire.get(&hid).cloned());
        let focused_tab_id = self
            .agent_tab_id
            .as_ref()
            .filter(|id| self.wire_to_host.contains_key(id.as_str()))
            .cloned()
            .or(active_wire)
            .or_else(|| tabs.first().map(|t| t.id.clone()));

        let state = BrowserState { tabs, focused_tab_id };
        let hash = hash_state(&state);
        (state, hash)
    }

    pub fn resolve_tab(&self, app: &App, wire_tab_id: &str) -> Result<u64, String> {
        let Some(&host_id) = self.wire_to_host.get(wire_tab_id) else {
            return Err(
                "Browser tab is unavailable. Call browser.tabs.list({}) and use an existing tabID from this session; a closed tab is not replaced automatically."
                    .into(),
            );
        };
        if app.browser_tab_session(host_id) != Some(self.session_id.as_str()) {
            return Err(
                "Browser tab belongs to another session. Call browser.tabs.list({}) and use a tabID owned by this session."
                    .into(),
            );
        }
        let Some(tab) = app.workspace_panes.browser.host.tab(host_id) else {
            return Err(
                "Browser tab is unavailable. Call browser.tabs.list({}) and use an existing tabID from this session."
                    .into(),
            );
        };
        if matches!(tab.context, browser_pane::ContextKind::Private) {
            return Err("Agent browser tools cannot access private tabs.".into());
        }
        Ok(host_id)
    }

    pub fn wire_tab(&self, app: &App, host_id: u64) -> Option<Tab> {
        let wire_id = self.host_to_wire.get(&host_id)?;
        let tab = app.workspace_panes.browser.host.tab(host_id)?;
        let generation = *self.generations.get(&host_id).unwrap_or(&1);
        Some(build_wire_tab(wire_id, tab, generation))
    }

    pub fn generation_of(&self, host_id: u64) -> u64 {
        *self.generations.get(&host_id).unwrap_or(&1)
    }

    /// Stores a capture while bounding BOTH per-tab count (<= 16) and session-wide memory
    /// (<= 32 MiB) so repeated captures across many tabs cannot OOM the app.
    pub fn record_capture(&mut self, host_id: u64, file: CapturedFile) {
        let list = self.captures.entry(host_id).or_default();
        list.push(file);
        while list.len() > 16 {
            list.remove(0);
        }
        loop {
            let total: usize = self
                .captures
                .values()
                .flat_map(|v| v.iter())
                .map(|c| c.bytes.len())
                .sum();
            if total <= MAX_SESSION_CAPTURE_BYTES {
                break;
            }
            let victim_tab = self
                .captures
                .iter()
                .filter(|(_, v)| !v.is_empty())
                .max_by_key(|(_, v)| v.len())
                .map(|(&tid, _)| tid);
            let Some(tid) = victim_tab else { break };
            if let Some(v) = self.captures.get_mut(&tid)
                && !v.is_empty()
            {
                v.remove(0);
            } else {
                break;
            }
        }
    }
}

impl Drop for SessionAttachment {
    fn drop(&mut self) {
        self.stop_outbound();
        if let Some((_, abort)) = self.attach_abort.take() {
            abort.abort();
        }
    }
}

pub(super) fn build_wire_tab(wire_id: &str, tab: &browser_pane::BrowserTab, generation: u64) -> Tab {
    Tab {
        id: wire_id.to_owned(),
        url: truncate_str(&tab.url, 16_384),
        title: truncate_str(&tab.title, 2_048),
        loading: tab.loading,
        load_error: tab.load_error.as_deref().map(|s| truncate_str(s, 2_048)),
        can_go_back: tab.can_go_back,
        can_go_forward: tab.can_go_forward,
        generation,
    }
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    s.chars().take(max).collect()
}

pub(super) fn hash_state(state: &BrowserState) -> u64 {
    let mut h = DefaultHasher::new();
    state.focused_tab_id.hash(&mut h);
    state.tabs.len().hash(&mut h);
    for t in &state.tabs {
        t.id.hash(&mut h);
        t.url.hash(&mut h);
        t.title.hash(&mut h);
        t.loading.hash(&mut h);
        t.load_error.hash(&mut h);
        t.can_go_back.hash(&mut h);
        t.can_go_forward.hash(&mut h);
        t.generation.hash(&mut h);
    }
    h.finish()
}

fn spawn_outbound_worker(
    server: Server,
    location: LocationRef,
    session_id: String,
    connection_id: String,
    rx: Receiver<OutboundMsg>,
    abort: AbortHandle,
    cancelled: Arc<Mutex<VecDeque<String>>>,
) {
    let is_cancelled = move |req_id: &str| {
        cancelled
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|r| r == req_id)
    };
    std::thread::Builder::new()
        .name("browser-rpc-out".into())
        .spawn(move || {
            while let Ok(msg) = rx.recv() {
                if abort.is_aborted() {
                    break;
                }
                match msg {
                    OutboundMsg::Stop => break,
                    OutboundMsg::State(mut state) => {
                        let mut deferred: Option<OutboundMsg> = None;
                        while let Ok(next) = rx.try_recv() {
                            match next {
                                OutboundMsg::State(newer) => state = newer,
                                other => {
                                    deferred = Some(other);
                                    break;
                                }
                            }
                        }
                        send_state_with_retry(&server, &location, &session_id, &connection_id, &state, &abort);
                        if let Some(d) = deferred {
                            match d {
                                OutboundMsg::Stop => break,
                                OutboundMsg::Result { request_id, outcome } => {
                                    if !abort.is_aborted() && !is_cancelled(&request_id) {
                                        send_result(&server, &location, &session_id, &connection_id, &request_id, &outcome);
                                    }
                                }
                                OutboundMsg::State(_) => {}
                            }
                        }
                    }
                    OutboundMsg::Result { request_id, outcome } => {
                        if !abort.is_aborted() && !is_cancelled(&request_id) {
                            send_result(&server, &location, &session_id, &connection_id, &request_id, &outcome);
                        }
                    }
                }
            }
        })
        .ok();
}

fn send_state_with_retry(
    server: &Server,
    location: &LocationRef,
    session_id: &str,
    connection_id: &str,
    state: &BrowserState,
    abort: &AbortHandle,
) {
    let mut delay_ms = STATE_RETRY_MIN_MS;
    loop {
        if abort.is_aborted() {
            return;
        }
        match server.browser_rpc_state(location, session_id, connection_id, state) {
            Ok(()) => return,
            Err(RpcCallError::Unavailable(_) | RpcCallError::Unsupported(_) | RpcCallError::InvalidInput(_)) => return,
            Err(RpcCallError::Aborted) => return,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(delay_ms));
                delay_ms = (delay_ms * 2).min(STATE_RETRY_MAX_MS);
            }
        }
    }
}

fn send_result(
    server: &Server,
    location: &LocationRef,
    session_id: &str,
    connection_id: &str,
    request_id: &str,
    outcome: &Outcome,
) {
    let outcome = match server.browser_rpc_result(location, session_id, connection_id, request_id, outcome) {
        Ok(()) => return,
        Err(RpcCallError::InvalidInput(msg)) => Outcome::fail(failure_code::OPERATION_FAILED, msg),
        Err(_) => return,
    };
    let _ = server.browser_rpc_result(location, session_id, connection_id, request_id, &outcome);
}
