//! App-side `oc-preview://` tab orchestration and async remote/artifact proxy queue on
//! [`BrowserHost`], with fail-closed fallback for older engines lacking `Ready.preview`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::client::{BrowserHost, BrowserTab};
use crate::frame::TabSurface;
use crate::machine::SharedEngine;
use crate::protocol::{ContextKind, CursorKind, HostMsg, PreviewKindTag, PreviewSpec};

/// Initial end-to-end byte credit per in-flight `PreviewFetch` (`1 MiB` max in flight per proxy
/// across host outbox + AF_UNIX socket + engine `PreviewRegistry`; replenished strictly when CEF
/// consumes bytes via `EngineMsg::PreviewCredit`).
const INITIAL_PREVIEW_CREDIT_BYTES: usize = 1024 * 1024;
/// Upper ceiling on accumulated credit per proxy (defensive against malformed peer messages).
const MAX_PREVIEW_CREDIT_BYTES: usize = 2 * 1024 * 1024;
/// Maximum time a host fetch worker waits for CEF to consume buffered bytes and return credit
/// before aborting the fetch and releasing its `PreviewFetchPool` worker (`20 s`, matching
/// `PREVIEW_PER_READ_TIMEOUT` so unconsumed `fetch()` or paused media cannot pin workers forever).
const PREVIEW_CREDIT_WAIT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Default)]
struct FlowEntry {
    tab_id: u64,
    credit: usize,
    canceled: bool,
}

/// End-to-end per-proxy byte-credit flow control between host `PreviewFetchPool` workers and
/// engine `ProxyResourceHandler::read`. Control messages NEVER wait on this budget.
#[derive(Default)]
pub(crate) struct PreviewChunkBudget {
    credit_enabled: AtomicBool,
    entries: Mutex<HashMap<u64, FlowEntry>>,
    ready: Condvar,
}

impl PreviewChunkBudget {
    #[cfg_attr(not(feature = "engine"), allow(dead_code))]
    pub(crate) fn set_credit_enabled(&self, enabled: bool) {
        self.credit_enabled.store(enabled, Ordering::Release);
        self.ready.notify_all();
    }

    #[cfg_attr(not(feature = "engine"), allow(dead_code))]
    pub(crate) fn on_fetch(&self, req_id: u64, tab_id: u64) {
        let mut map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        map.insert(req_id, FlowEntry { tab_id, credit: INITIAL_PREVIEW_CREDIT_BYTES, canceled: false });
    }

    #[cfg_attr(not(feature = "engine"), allow(dead_code))]
    pub(crate) fn on_credit(&self, req_id: u64, bytes: usize, cancel: bool) {
        let mut map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = map.get_mut(&req_id) {
            if cancel {
                entry.canceled = true;
            } else {
                entry.credit = entry.credit.saturating_add(bytes).min(MAX_PREVIEW_CREDIT_BYTES);
            }
            self.ready.notify_all();
        }
    }

    pub(crate) fn cancel_tab(&self, tab_id: u64) {
        let mut map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let mut woke = false;
        for entry in map.values_mut() {
            if entry.tab_id == tab_id {
                entry.canceled = true;
                woke = true;
            }
        }
        if woke {
            self.ready.notify_all();
        }
    }

    pub(crate) fn finish(&self, req_id: u64) {
        let mut map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if map.remove(&req_id).is_some() {
            self.ready.notify_all();
        }
    }

    pub(crate) fn acquire(
        &self,
        req_id: u64,
        raw_len: usize,
        alive: &AtomicBool,
        shared: &Mutex<SharedEngine>,
    ) -> bool {
        let need = raw_len.max(1);
        let deadline = Instant::now() + PREVIEW_CREDIT_WAIT_TIMEOUT;
        let mut map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if !alive.load(Ordering::Acquire)
                || !shared.lock().unwrap_or_else(PoisonError::into_inner).is_connected()
            {
                return false;
            }
            let Some(entry) = map.get_mut(&req_id) else {
                return false;
            };
            if entry.canceled {
                return false;
            }
            // Pre-credit `#281`-era engines (`Ready.preview == true, preview_credit == false`) do
            // not emit `EngineMsg::PreviewCredit`: skip credit blocking so >1 MiB previews never
            // deadlock (engine's own queue cap + pool caps still bound in-flight data).
            if !self.credit_enabled.load(Ordering::Acquire) {
                return true;
            }
            if entry.credit >= need {
                entry.credit -= need;
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                entry.canceled = true;
                return false;
            }
            let wait = deadline.saturating_duration_since(now).min(Duration::from_millis(50));
            let (next, _) = self
                .ready
                .wait_timeout(map, wait)
                .unwrap_or_else(PoisonError::into_inner);
            map = next;
        }
    }

    #[cfg_attr(not(feature = "engine"), allow(dead_code))]
    pub(crate) fn reset(&self) {
        self.credit_enabled.store(false, Ordering::Release);
        let mut map = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        map.clear();
        self.ready.notify_all();
    }
}

/// Thread-safe handle allowing background remote/artifact preview HTTP readers to stream
/// response headers and 64 KiB chunks directly to the engine sidecar with end-to-end byte-credit
/// back-pressure.
#[derive(Clone)]
pub struct PreviewResponder {
    shared: Arc<Mutex<SharedEngine>>,
    budget: Arc<PreviewChunkBudget>,
}

impl PreviewResponder {
    fn send(&self, msg: HostMsg) -> bool {
        let guard = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        guard.outbox().is_some_and(|tx| tx.send(msg).is_ok())
    }

    pub fn send_head(
        &self,
        req_id: u64,
        ok: bool,
        status: u16,
        mime: String,
        content_length: Option<u64>,
        content_range: Option<String>,
    ) -> bool {
        if !ok {
            self.budget.finish(req_id);
        }
        self.send(HostMsg::PreviewResponseHead {
            req_id,
            ok,
            status,
            mime,
            content_length,
            content_range,
        })
    }

    /// Streams one base64 chunk gated by per-proxy engine byte credits; returns `false`
    /// immediately if `alive` flips `false`, the engine cancels the proxy, or it disconnects.
    pub fn send_chunk(&self, req_id: u64, raw_len: usize, data_b64: String, alive: &AtomicBool) -> bool {
        if !self.budget.acquire(req_id, raw_len, alive, &self.shared) {
            return false;
        }
        if !self.send(HostMsg::PreviewResponseChunk { req_id, data_b64 }) {
            self.budget.finish(req_id);
            return false;
        }
        true
    }

    pub fn send_end(&self, req_id: u64, ok: bool) -> bool {
        self.budget.finish(req_id);
        self.send(HostMsg::PreviewResponseEnd { req_id, ok })
    }

    pub fn finish_request(&self, req_id: u64) {
        self.budget.finish(req_id);
    }

    pub fn cancel_tab(&self, tab_id: u64) {
        self.budget.cancel_tab(tab_id);
    }
}

/// Pending remote workspace or artifact fetch emitted by the engine (`EngineMsg::PreviewFetch`),
/// answered by the desktop app host using strictly the owning session's server credentials.
#[derive(Clone, Debug)]
pub struct PreviewFetchRequest {
    pub req_id: u64,
    pub tab_id: u64,
    pub kind: PreviewKindTag,
    pub path: String,
    pub method: String,
    pub range: Option<String>,
}

impl PreviewSpec {
    pub fn tag(&self) -> PreviewKindTag {
        match self {
            Self::Local { .. } => PreviewKindTag::Local,
            Self::Remote { .. } => PreviewKindTag::Remote,
            Self::Artifact { .. } => PreviewKindTag::Artifact,
        }
    }

    /// App-side isolated [`ContextKind`] tag matching the engine's isolated preview/artifact scope
    /// so `t.context.is_isolated()` is always `true` for preview and artifact tabs.
    pub fn context_kind(&self, tab_id: u64) -> ContextKind {
        match self {
            Self::Local { root, .. } => ContextKind::Preview {
                workspace: root.clone(),
                grant: format!("tab-{tab_id}"),
            },
            Self::Remote { root, session_prefix, .. } => ContextKind::Preview {
                workspace: format!("{session_prefix}{root}"),
                grant: format!("tab-{tab_id}"),
            },
            Self::Artifact { artifact_id, .. } => ContextKind::Artifact {
                artifact_id: format!("{artifact_id}:tab-{tab_id}"),
            },
        }
    }
}

impl BrowserHost {
    /// Opens a new isolated preview tab (`local`, `remote`, or `artifact`).
    ///
    /// Fail-closed capability gating (`notes/review-rd281.md` §9):
    /// - While the engine is not yet `Ready`, the tab waits in `pending_opens` until `Ready` arrives.
    /// - When `Ready.preview == true`, the engine opens an isolated `oc-preview://` context.
    /// - When connected to an older engine (`Ready.preview == false`), NEVER falls back to `file://`;
    ///   refuses cleanly with `"browser.preview.unsupported"`.
    pub fn open_preview_tab(&mut self, display_url: &str, spec: PreviewSpec, agent_driven: bool) -> u64 {
        rune::marks::mark("browser.tab.open");
        let id = self.alloc_tab_id();
        let surface = TabSurface::new();
        if let Some(w) = &self.waker {
            crate::frame::wake_window(&surface, w);
        }
        self.surfaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, surface.clone());

        let ((width_pt, height_pt), scale) = self.last_viewport;
        self.clear_stale_fault_if_empty();
        let connected = self.is_connected();
        let ready = !matches!(self.caps, crate::machine::EngineCaps::Unknown);
        let can_send = connected && ready;
        let tag = spec.tag();
        let ctx_kind = spec.context_kind(id);

        let (initial_url, loading, refusal, send_open) = if can_send {
            if self.engine_preview() {
                (display_url.to_owned(), true, None, true)
            } else {
                ("about:blank".into(), false, Some("browser.preview.unsupported".to_owned()), false)
            }
        } else {
            (display_url.to_owned(), true, None, false)
        };

        let mut prompts = crate::prompts::TabPromptState::default();
        if agent_driven {
            prompts.agent_driven = true;
            prompts.driver = Some(crate::prompts::TabDriver::Agent);
        }
        self.tabs.push(BrowserTab {
            id,
            url: initial_url,
            title: String::new(),
            loading,
            audible: false,
            can_go_back: false,
            can_go_forward: false,
            zoom: 1.0,
            cursor: CursorKind::Default,
            scroll_cursor: Default::default(),
            load_error: None,
            favicon: None,
            preview: Some(tag),
            preview_spec: Some(spec.clone()),
            refusal: refusal.clone(),
            rejected: u64::from(refusal.is_some()),
            inspecting: false,
            files: Vec::new(),
            prompts,
            find: Default::default(),
            surface,
            surface_origin: (0.0, 0.0),
            view_zoom: 1.0,
            context: ctx_kind.clone(),
            allow_extension_pages: false,
            file_roots: Vec::new(),
            viewport_pt: (width_pt, height_pt),
            device: None,
            scale,
            sent_resize: can_send.then_some((width_pt, height_pt, scale)),
            has_engine_tab: send_open || !can_send,
            engine_confirmed: false,
        });
        self.sync_has_tabs();
        self.focused_tab = Some(id);

        if can_send {
            if send_open {
                self.send_open_tab(
                    id,
                    "about:blank".into(),
                    width_pt,
                    height_pt,
                    scale,
                    true,
                    Vec::new(),
                    ctx_kind,
                    Some(spec),
                    agent_driven,
                );
            }
        } else {
            self.pending_opens.push((id, display_url.to_owned()));
            self.ensure_connected();
        }
        id
    }

    /// Replaces the CEF browser behind `tab_id` when navigating between a preview tab and a normal
    /// web tab (or vice versa), so web pages always run in the user's normal signed-in context and
    /// previews always run in a fresh isolated preview context.
    pub fn reopen_tab_as_web(&mut self, tab_id: u64, url: &str, mut context: ContextKind) {
        self.preview_chunk_budget.cancel_tab(tab_id);
        self.cancel_download_save_requests(tab_id);
        let focused = self.focused_tab == Some(tab_id);
        let ready = self.engine_caps_known();
        let proxy_err = self.resolve_remote_context(&mut context);
        let ctx_ok = !ready || context.is_supported(self.engine_partitions(), self.engine_remote_proxy());
        let refused = proxy_err
            .map(str::to_owned)
            .or_else(|| (!ctx_ok).then(|| "browser.partitions.unsupported".to_owned()));
        let ((width_pt, height_pt), scale, had_engine_tab, agent_driven) = {
            let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
            let had = t.has_engine_tab;
            t.preview = None;
            t.preview_spec = None;
            t.context = context.clone();
            t.file_roots.clear();
            t.files.clear();
            t.load_error = None;
            t.engine_confirmed = false;
            t.prompts.reset_for_context_switch();
            if let Some(code) = &refused {
                t.url = "about:blank".into();
                t.loading = false;
                t.refusal = Some(code.clone());
                t.rejected += 1;
                t.has_engine_tab = false;
            } else {
                t.url = url.to_owned();
                t.loading = true;
                t.refusal = None;
                t.has_engine_tab = true;
            }
            (t.viewport_pt, t.scale, had, t.prompts.agent_driven)
        };
        if refused.is_none() && !had_engine_tab && self.tabs.iter().filter(|t| t.has_engine_tab).count() == 1 {
            self.clear_stale_fault();
        }
        let connected = self.is_connected();
        self.sync_has_tabs();
        let had_live_engine = had_engine_tab && !self.pending_opens.iter().any(|(id, _)| *id == tab_id);
        if refused.is_some() {
            self.pending_opens.retain(|(id, _)| *id != tab_id);
            if connected && had_live_engine {
                *self.reopening_tabs.entry(tab_id).or_default() += 1;
                self.send(HostMsg::CloseTab { tab_id });
            }
            return;
        }
        if connected && (context.is_default() || ready) {
            if had_live_engine {
                *self.reopening_tabs.entry(tab_id).or_default() += 1;
                self.send(HostMsg::CloseTab { tab_id });
            }
            self.pending_opens.retain(|(id, _)| *id != tab_id);
            self.send_open_tab(
                tab_id,
                url.to_owned(),
                width_pt,
                height_pt,
                scale,
                focused,
                Vec::new(),
                context,
                None,
                agent_driven,
            );
            self.replay_tab_lifecycle(tab_id);
        } else {
            if connected && had_live_engine {
                *self.reopening_tabs.entry(tab_id).or_default() += 1;
                self.send(HostMsg::CloseTab { tab_id });
            }
            self.pending_opens.retain(|(id, _)| *id != tab_id);
            self.pending_opens.push((tab_id, url.to_owned()));
            self.ensure_connected();
        }
    }

    /// Replaces the CEF browser behind `tab_id` with a fresh isolated preview context.
    pub fn reopen_tab_as_preview(&mut self, tab_id: u64, display_url: &str, spec: PreviewSpec) {
        self.preview_chunk_budget.cancel_tab(tab_id);
        self.cancel_download_save_requests(tab_id);
        let focused = self.focused_tab == Some(tab_id);
        self.clear_stale_fault_if_empty();
        let connected = self.is_connected();
        let ready = self.engine_caps_known();
        let engine_preview = self.engine_preview();
        let ctx_kind = spec.context_kind(tab_id);
        let ((width_pt, height_pt), scale, had_engine_tab, agent_driven) = {
            let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
            let had = t.has_engine_tab;
            t.preview = Some(spec.tag());
            t.preview_spec = Some(spec.clone());
            t.context = ctx_kind.clone();
            t.file_roots.clear();
            t.files.clear();
            t.url = display_url.to_owned();
            t.loading = true;
            t.load_error = None;
            t.refusal = None;
            t.engine_confirmed = false;
            t.prompts.reset_for_context_switch();
            t.has_engine_tab = !(connected && ready && !engine_preview);
            (t.viewport_pt, t.scale, had, t.prompts.agent_driven)
        };
        self.sync_has_tabs();
        let had_live_engine = had_engine_tab && !self.pending_opens.iter().any(|(id, _)| *id == tab_id);
        if connected && ready {
            if engine_preview {
                if had_live_engine {
                    *self.reopening_tabs.entry(tab_id).or_default() += 1;
                    self.send(HostMsg::CloseTab { tab_id });
                }
                self.pending_opens.retain(|(id, _)| *id != tab_id);
                self.send_open_tab(
                    tab_id,
                    "about:blank".into(),
                    width_pt,
                    height_pt,
                    scale,
                    focused,
                    Vec::new(),
                    ctx_kind,
                    Some(spec),
                    agent_driven,
                );
                self.replay_tab_lifecycle(tab_id);
            } else {
                if had_live_engine {
                    *self.reopening_tabs.entry(tab_id).or_default() += 1;
                    self.send(HostMsg::CloseTab { tab_id });
                }
                self.pending_opens.retain(|(id, _)| *id != tab_id);
                if let Some(t) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    t.url = "about:blank".into();
                    t.loading = false;
                    t.refusal = Some("browser.preview.unsupported".to_owned());
                    t.rejected += 1;
                }
            }
        } else {
            if connected && had_live_engine {
                *self.reopening_tabs.entry(tab_id).or_default() += 1;
                self.send(HostMsg::CloseTab { tab_id });
            }
            self.pending_opens.retain(|(id, _)| *id != tab_id);
            self.pending_opens.push((tab_id, display_url.to_owned()));
            self.ensure_connected();
        }
    }

    pub fn has_preview_fetches(&self) -> bool {
        !self.preview_fetches.is_empty()
    }

    /// Drains pending remote/artifact preview fetch requests queued by the engine.
    pub fn drain_preview_fetches(&mut self) -> Vec<PreviewFetchRequest> {
        std::mem::take(&mut self.preview_fetches)
    }

    /// Drains `(tab_id, document)` pairs for local preview tabs that waited for an older engine
    /// lacking `Ready.preview` so the app can switch them to the Files source viewer instead of
    /// ever falling back to `file://`.
    pub fn drain_unsupported_local_previews(&mut self) -> Vec<(u64, String)> {
        std::mem::take(&mut self.unsupported_local_previews)
    }

    /// Inserts a locally refused tab (e.g., `browser.preview.unsupported` or policy refusal)
    /// WITHOUT sending any `HostMsg::OpenTab` to the engine (`notes/review-rd281.md` PR 2 Blocker #4).
    pub fn open_refused_tab(&mut self, code: &str, context: ContextKind) -> u64 {
        let id = self.alloc_tab_id();
        let surface = TabSurface::new();
        self.surfaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id, surface.clone());
        let ((width_pt, height_pt), scale) = self.last_viewport;
        self.tabs.push(BrowserTab {
            id,
            url: "about:blank".into(),
            title: String::new(),
            loading: false,
            audible: false,
            can_go_back: false,
            can_go_forward: false,
            zoom: 1.0,
            cursor: CursorKind::Default,
            scroll_cursor: Default::default(),
            load_error: None,
            favicon: None,
            preview: None,
            preview_spec: None,
            refusal: Some(code.to_owned()),
            rejected: 1,
            inspecting: false,
            files: Vec::new(),
            prompts: Default::default(),
            find: Default::default(),
            surface,
            surface_origin: (0.0, 0.0),
            view_zoom: 1.0,
            context,
            allow_extension_pages: false,
            file_roots: Vec::new(),
            viewport_pt: (width_pt, height_pt),
            device: None,
            scale,
            sent_resize: None,
            has_engine_tab: false,
            engine_confirmed: false,
        });
        self.focused_tab = Some(id);
        id
    }

    /// Returns a cloneable responder that streams preview responses directly to the engine outbox
    /// with bounded chunk back-pressure.
    pub fn preview_responder(&self) -> PreviewResponder {
        PreviewResponder {
            shared: Arc::clone(&self.shared),
            budget: Arc::clone(&self.preview_chunk_budget),
        }
    }

    /// Sets an explicit load error on a tab (for example `browser.artifact.missing`).
    pub fn set_tab_load_error(&mut self, id: u64, error: Option<String>) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.load_error = error;
            t.loading = false;
        }
    }

    pub fn set_mock_preview_clear(&mut self, id: u64) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.preview = None;
            t.preview_spec = None;
        }
    }

    pub fn send_preview_response_head(
        &self,
        req_id: u64,
        ok: bool,
        status: u16,
        mime: String,
        content_length: Option<u64>,
        content_range: Option<String>,
    ) {
        self.send(HostMsg::PreviewResponseHead {
            req_id,
            ok,
            status,
            mime,
            content_length,
            content_range,
        });
    }

    pub fn send_preview_response_chunk(&self, req_id: u64, data_b64: String) {
        self.send(HostMsg::PreviewResponseChunk { req_id, data_b64 });
    }

    pub fn send_preview_response_end(&self, req_id: u64, ok: bool) {
        self.send(HostMsg::PreviewResponseEnd { req_id, ok });
    }

    /// Marks a mock tab as a preview (stories and fixture tests).
    pub fn set_mock_preview(&mut self, id: u64, kind: PreviewKindTag, spec: PreviewSpec) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.preview = Some(kind);
            t.preview_spec = Some(spec);
        }
    }

    /// Creates a synthetic preview tab without spawning CEF, used by deterministic Storybook stages
    /// and headless UI tests.
    pub fn insert_mock_tab(&mut self, url: &str, title: &str, status: crate::installer::EngineStatus) -> u64 {
        let id = self.alloc_tab_id();
        let surface = crate::frame::preview();
        if let Some(w) = &self.waker {
            crate::frame::wake_window(&surface, w);
        }
        self.surfaces.lock().unwrap_or_else(PoisonError::into_inner).insert(id, surface.clone());
        self.tabs.push(BrowserTab {
            id,
            url: url.to_owned(),
            title: title.to_owned(),
            loading: false,
            audible: false,
            can_go_back: true,
            can_go_forward: false,
            zoom: 1.0,
            cursor: CursorKind::Default,
            scroll_cursor: Default::default(),
            load_error: None,
            favicon: None,
            preview: None,
            preview_spec: None,
            refusal: None,
            rejected: 0,
            inspecting: false,
            files: Vec::new(),
            prompts: Default::default(),
            find: Default::default(),
            surface,
            surface_origin: (0.0, 0.0),
            view_zoom: 1.0,
            context: ContextKind::Default,
            allow_extension_pages: false,
            file_roots: Vec::new(),
            viewport_pt: (800.0, 600.0),
            device: None,
            scale: 2.0,
            sent_resize: None,
            has_engine_tab: false,
            engine_confirmed: false,
        });
        self.focused_tab = Some(id);
        self.last_polled_status = status.clone();
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .step(crate::machine::EngineEvent::MockStatus(status));
        id
    }

    /// Shows `url` titled `title` in mock tab `id`, loaded at once, as an engine would report a
    /// finished navigation (stories, captures and tests have no engine).
    pub fn set_mock_page(&mut self, id: u64, url: &str, title: &str) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.can_go_back |= !t.url.is_empty() && t.url != "about:blank";
            t.url = url.to_owned();
            t.title = title.to_owned();
            t.loading = false;
            t.refusal = None;
        }
    }

    pub fn set_mock_refusal(&mut self, id: u64, reason: &str) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.loading = false;
            t.refusal = Some(reason.to_owned());
            t.rejected += 1;
        }
    }

    pub fn clear_refusal(&mut self, id: u64) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.refusal = None;
        }
    }

    /// Sets whether mock tab `id` has an earlier and a later page.
    pub fn set_mock_history(&mut self, id: u64, back: bool, forward: bool) {
        if let Some(t) = self.tabs.iter_mut().find(|t| t.id == id) {
            t.can_go_back = back;
            t.can_go_forward = forward;
        }
    }

    /// Sends every message to the returned receiver instead of an engine (tests).
    pub fn capture_sent(&mut self) -> std::sync::mpsc::Receiver<HostMsg> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .step(crate::machine::EngineEvent::Capture(tx));
        rx
    }

    pub fn set_mock_status(&mut self, status: crate::installer::EngineStatus) {
        self.last_polled_status = status.clone();
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .step(crate::machine::EngineEvent::MockStatus(status));
    }
}
