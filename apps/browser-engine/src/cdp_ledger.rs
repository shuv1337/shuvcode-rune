//! Typed and raw agent CDP dispatch, reply checks and event forwarding for one tab. Every frame,
//! session, context and object rule is decided by `EngineCdpLedger` (`frame_authority.rs`); the
//! app never decides one.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cef::rc::Rc as _;
use cef::{
    Browser, DevToolsMessageObserver, ImplBrowserHost, ImplDevToolsMessageObserver,
    WrapDevToolsMessageObserver, wrap_dev_tools_message_observer,
};
use crate::output::Output;
use crate::wire::{EngineMsg, TabSnapshot};

use serde_json::Value;

pub use crate::frame_authority::EngineCdpLedger;
use crate::frame_authority::{Dispatch, Post, Reach};

const EXTENSION_SURFACE: &str = "Agent browser tools cannot read or act on extension pages or tabs with extension frames.";

/// Hard engine-side byte cap on `Runtime.evaluate` / `Runtime.callFunctionOn` CDP responses
/// (enforced BEFORE JSON parsing and IPC forwarding so giant eval results cannot OOM engine or UI).
pub(crate) const MAX_CDP_EVAL_RESPONSE_BYTES: usize = 1024 * 1024;
/// Hard engine-side byte cap on all other CDP responses (`Page.captureScreenshot`, AX tree, etc.).
pub(crate) const MAX_CDP_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn max_cdp_response_bytes(method: &str) -> usize {
    if matches!(method, "Runtime.evaluate" | "Runtime.callFunctionOn") {
        MAX_CDP_EVAL_RESPONSE_BYTES
    } else {
        MAX_CDP_RESPONSE_BYTES
    }
}

/// Sends what a tab's observer queued (resumes, nested auto-attach, frame-tree fetches) after
/// each `cef::do_message_loop_work`, looping until empty (capped at 16 passes) in case a send
/// synchronously delivers another callback into `outbox`.
pub(crate) fn flush_outbox(ledger: &Mutex<EngineCdpLedger>, host: &cef::BrowserHost) {
    for _ in 0..16 {
        let out = std::mem::take(&mut ledger.lock().unwrap().outbox);
        if out.is_empty() {
            break;
        }
        for m in out {
            host.send_dev_tools_message(Some(m.as_bytes()));
        }
    }
}

/// Whether a tab's agent calls are checked against inherited `Default` grants: a controlled,
/// non-isolated, non-preview tab (Guard 1's scope).
pub(crate) fn grant_gated(t: &crate::engine::ManagedTab) -> bool {
    !t.isolated && t.preview.is_none() && t.prompts.agent_driven.load(std::sync::atomic::Ordering::Acquire)
}

/// The granted origin `reach` touches in tab `t`, if any (`Some(None)`: cannot tell, refused).
pub(crate) fn granted_origin(
    t: &crate::engine::ManagedTab,
    permissions: &crate::permissions::PermissionTracker,
    reach: &Reach,
) -> Option<Option<String>> {
    t.cdp_ledger.lock().unwrap().granted_origin(reach, &|o| permissions.has_inherited_default_grant(o))
}

/// Validates and dispatches one `HostMsg::TypedCdpCall`: whole extension tabs are refused,
/// `Input.*` routes through the serialized per-tab [`crate::input_guard::InputGuard`],
/// `Page.handleJavaScriptDialog` refuses while a user prompt/leave dialog is active, profiling is
/// refused while extensions are active (a recorded difference), and the ledger decides the rest.
#[allow(clippy::too_many_arguments)]
pub(crate) fn handle_typed_cdp_call(
    tab_id: u64,
    request_id: u64,
    not_ready_reason: Option<&str>,
    t: &crate::engine::ManagedTab,
    prompt_shared: &crate::prompts::PromptShared,
    permissions: &crate::permissions::PermissionTracker,
    next_cdp_id: &std::sync::atomic::AtomicI32,
    out_tx: &crate::output::Output,
    extensions_active: bool,
    method: String,
    mut params: Value,
    session_id: Option<String>,
    frame_id: Option<String>,
) {
    use cef::{ImplBrowser, ImplBrowserHost};
    use serde_json::json;
    use std::sync::atomic::Ordering;

    let surface = crate::engine::classify_surface(Some(&t.browser), t.is_popup, &t.snap, &t.prompts, &t.cdp_ledger);
    let profiling = ["HeapProfiler.", "Profiler.", "Tracing."].iter().any(|p| method.starts_with(p));
    let reach = grant_gated(t).then(|| {
        t.cdp_ledger.lock().unwrap().reach(&method, session_id.as_deref(), frame_id.as_deref(), &params)
    });
    let oopif_inherited_blocked = reach.as_ref().is_some_and(|r| granted_origin(t, permissions, r).is_some());
    let reply = |ok: bool, result: Value| {
        let _ = out_tx.send(EngineMsg::CdpResponse { tab_id, request_id, ok, result });
    };
    if method == "Page.handleJavaScriptDialog" && (t.prompts.has_leave_dialog() || t.prompts.is_covered()) {
        return reply(
            false,
            json!({ "message": "The user is answering a prompt over this page. Wait for them, or use another tab." }),
        );
    }
    if method.starts_with("Input.") {
        if let Some(reason) = not_ready_reason {
            return reply(false, json!({ "message": reason }));
        }
        if oopif_inherited_blocked {
            return reply(false, json!({ "message": crate::permissions::INHERITED_PERMISSION_MSG }));
        }
        if session_id.is_some() {
            return reply(false, json!({ "message": "Input DevTools calls must target the page root session." }));
        }
        if let Err(e) = browser_policy::check_typed_cdp(&method, &params, true) {
            return reply(false, json!({ "message": e }));
        }
        t.cdp_ledger.lock().unwrap().app_attached = true;
        if t.render_mode.is_awake() {
            crate::input_fps::boost(tab_id, &t.host);
        }
        if let Err(msg) = t
            .input_guard
            .lock()
            .unwrap()
            .enqueue(crate::input_guard::InputCall::single(request_id, method, params))
        {
            return reply(false, json!({ "message": msg }));
        }
        crate::input_guard::pump(
            tab_id,
            &t.input_guard,
            &t.browser,
            &t.host,
            t.is_popup,
            t.isolated,
            t.preview.is_some(),
            &t.snap,
            &t.prompts,
            permissions,
            &t.cdp_ledger,
            &t.emulation_scale,
            &t.cdp_id_map,
            next_cdp_id,
            out_tx,
        );
        return;
    }
    let trace_start_pid = if method == "Tracing.start" {
        let pid = t.prompts.committed_main_renderer_pid(Some(&t.browser));
        if pid == 0 && let Some(f) = t.browser.main_frame() {
            crate::tab_client::ping_renderer_pid(&f);
        }
        pid
    } else {
        0
    };
    let checked = if let Some(reason) = not_ready_reason {
        Err(reason.to_owned())
    } else if oopif_inherited_blocked {
        Err(crate::permissions::INHERITED_PERMISSION_MSG.into())
    } else if surface == crate::engine::Surface::WholeTab {
        Err(EXTENSION_SURFACE.into())
    } else if profiling && extensions_active {
        Err("Agent browser tools cannot profile pages while browser extensions are active.".into())
    } else if profiling && session_id.is_some() {
        Err("Agent profiling commands only target the tab's own main renderer process.".into())
    } else if method == "Tracing.start" && params.get("transferMode").and_then(Value::as_str) != Some("ReportEvents") {
        Err("Tracing.start only permits transferMode 'ReportEvents'.".into())
    } else if method == "Tracing.start" && trace_start_pid == 0 {
        Err("Tab renderer process ID is not ready yet; wait for the page to load and retry browser.trace.start.".into())
    } else if method == "Tracing.end" && !prompt_shared.is_trace_owner(tab_id) {
        Err("No active performance trace is owned by this tab.".into())
    } else if let Err(e) = browser_policy::check_typed_cdp(&method, &params, true) {
        Err(e)
    } else {
        t.cdp_ledger.lock().unwrap().verify_typed_call(&method, &mut params, session_id.as_deref(), frame_id.as_deref())
    };
    let post = match checked {
        Err(err) => return reply(false, json!({ "message": err })),
        Ok(Dispatch::ReplayWebTargets { replay }) => {
            for (params, session_id) in replay {
                let _ = out_tx.send(EngineMsg::CdpEvent { tab_id, method: "Target.attachedToTarget".into(), params, session_id });
            }
            return reply(true, json!({}));
        }
        Ok(Dispatch::ReEnable) => {
            let disable = t.cdp_ledger.lock().unwrap().runtime_disable(session_id.as_deref());
            t.host.send_dev_tools_message(Some(disable.as_bytes()));
            for binding in [
                Some(t.passkeys.binding_msg(session_id.as_deref())),
                Some(t.media_gate.binding_msg(session_id.as_deref())),
                Some(t.notification_gate.binding_msg(session_id.as_deref())),
                t.clipboard_gate.binding_msg(session_id.as_deref()),
            ]
            .into_iter()
            .flatten()
            {
                t.host.send_dev_tools_message(Some(binding.as_bytes()));
            }
            Post::None
        }
        Ok(Dispatch::Send(post)) => post,
    };
    let started_trace = method == "Tracing.start";
    if started_trace {
        if let Err(owner) = prompt_shared.try_claim_trace(tab_id) {
            let msg = if owner == tab_id {
                "A performance trace is already active in this tab. Use browser.trace.stop({tabID}) to finish it before starting another."
            } else {
                "Another tab owns the active performance trace. Wait for its owner to finish; do not stop or replace another tab's recording."
            };
            return reply(false, json!({ "message": msg }));
        }
        t.prompts.trace_pid.store(trace_start_pid, Ordering::Release);
    }
    let msg_id = next_cdp_id.fetch_add(1, Ordering::Relaxed);
    t.cdp_id_map.lock().unwrap().insert(
        msg_id,
        CdpPending::Raw {
            request_id,
            max_bytes: max_cdp_response_bytes(&method),
            typed: Some((post, session_id.clone())),
            reach,
            is_tracing_start: started_trace,
        },
    );
    let mut payload = json!({ "id": msg_id, "method": method, "params": params });
    if let Some(sid) = session_id {
        payload["sessionId"] = Value::String(sid);
    }
    let raw = payload.to_string();
    if t.host.send_dev_tools_message(Some(raw.as_bytes())) != 1 {
        if started_trace {
            t.prompts.trace_pid.store(0, Ordering::Release);
            prompt_shared.release_trace(tab_id);
        }
        t.cdp_id_map.lock().unwrap().remove(&msg_id);
        reply(false, json!({ "message": "send_dev_tools_message rejected" }));
    }
}

pub(crate) fn handle_site_cookies(
    tab_id: u64,
    request_id: u64,
    not_ready_reason: Option<&str>,
    t: &crate::engine::ManagedTab,
    next_cdp_id: &std::sync::atomic::AtomicI32,
    out_tx: &crate::output::Output,
) {
    use cef::ImplBrowserHost;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    if let Some(reason) = not_ready_reason {
        let _ = out_tx.send(crate::wire::EngineMsg::CdpResponse {
            tab_id,
            request_id,
            ok: false,
            result: json!({ "message": reason }),
        });
        return;
    }
    let url = t.snap.lock().unwrap().url.clone();
    if browser_policy::destination_origin(&url).is_none() {
        let _ = out_tx.send(crate::wire::EngineMsg::CdpResponse {
            tab_id,
            request_id,
            ok: true,
            result: json!({ "cookies": 0 }),
        });
        return;
    }
    let msg_id = next_cdp_id.fetch_add(1, Ordering::Relaxed);
    t.cdp_id_map.lock().unwrap().insert(msg_id, crate::handlers::CdpPending::CookieCount(request_id));
    let raw = json!({
        "id": msg_id,
        "method": "Network.getCookies",
        "params": { "urls": [url] },
    })
    .to_string();
    if t.host.send_dev_tools_message(Some(raw.as_bytes())) != 1 {
        t.cdp_id_map.lock().unwrap().remove(&msg_id);
        let _ = out_tx.send(crate::wire::EngineMsg::CdpResponse {
            tab_id,
            request_id,
            ok: false,
            result: json!({ "message": "send_dev_tools_message rejected" }),
        });
    }
}

/// Top-level JSON envelope fields of a raw CDP message extracted without building a `Value` AST.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TopLevelCdp {
    pub id: Option<i32>,
    pub has_method: bool,
    pub has_session_id: bool,
}

/// Streaming depth-tracking scanner that inspects ONLY root-level (`depth == 1`) JSON keys so
/// nested `"id"`/`"sessionId"` inside `params` or `result` can never spoof reply/event routing.
pub(crate) fn scan_top_level_cdp(raw: &[u8]) -> TopLevelCdp {
    let mut out = TopLevelCdp::default();
    let n = raw.len();
    let mut i = 0usize;
    while i < n && raw[i].is_ascii_whitespace() {
        i += 1;
    }
    if i >= n || raw[i] != b'{' {
        return out;
    }
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escape = false;
    let mut expect_key = false;
    while i < n {
        let b = raw[i];
        if in_str {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'{' => {
                depth += 1;
                if depth == 1 {
                    expect_key = true;
                }
                i += 1;
            }
            b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    break;
                }
                i += 1;
            }
            b',' => {
                if depth == 1 {
                    expect_key = true;
                }
                i += 1;
            }
            b'"' if depth == 1 && expect_key => {
                expect_key = false;
                let key_start = i + 1;
                i += 1;
                let mut esc = false;
                while i < n {
                    if esc {
                        esc = false;
                    } else if raw[i] == b'\\' {
                        esc = true;
                    } else if raw[i] == b'"' {
                        break;
                    }
                    i += 1;
                }
                let key = &raw[key_start..i.min(n)];
                if i < n {
                    i += 1;
                }
                while i < n && raw[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i < n && raw[i] == b':' {
                    i += 1;
                    while i < n && raw[i].is_ascii_whitespace() {
                        i += 1;
                    }
                    if key == b"method" {
                        out.has_method = true;
                        break;
                    } else if key == b"sessionId" {
                        out.has_session_id = true;
                    } else if key == b"id" {
                        let num_start = i;
                        if i < n && raw[i] == b'-' {
                            i += 1;
                        }
                        while i < n && raw[i].is_ascii_digit() {
                            i += 1;
                        }
                        if let Ok(s) = std::str::from_utf8(&raw[num_start..i]) {
                            out.id = s.parse::<i32>().ok();
                        }
                    }
                }
            }
            b'"' => {
                in_str = true;
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}


pub enum CdpPending {
    /// An app call. `typed` carries what a typed reply still needs and the call's session; a raw
    /// call (`None`) is refused at reply time on any extension frame.
    /// `reach` is what an agent call on a controlled `Default` tab can act in, checked again
    /// against inherited grants when the reply comes back (`None`: not checked).
    Raw { request_id: u64, max_bytes: usize, typed: Option<(Post, Option<String>)>, reach: Option<Reach>, is_tracing_start: bool },
    InputGuard,
    CookieCount(u64),
    ClearSite { request_id: u64, origin: String },
    /// The person's Copy image (`menu_extras.rs`): the image's bytes, an isolated world in its
    /// frame, then the PNG re-encode. Nothing of it reaches an agent.
    CopyImageContent { frame_id: String },
    CopyImageWorld { b64: String },
    CopyImageEncode,
}

pub struct PendingClear {
    pub tab_id: u64,
    pub request_id: u64,
    pub origin: String,
    pub cookies: Vec<(String, String, String)>,
}

wrap_dev_tools_message_observer! {
    pub struct TabCdpObserver {
        tab_id: u64,
        is_popup: bool,
        isolated: bool,
        is_preview: bool,
        preview: Option<Arc<crate::preview::PreviewGrant>>,
        snap: Arc<Mutex<TabSnapshot>>,
        id_map: Arc<Mutex<HashMap<i32, CdpPending>>>,
        internal_picked: Arc<Mutex<Vec<i64>>>,
        pending_clears: Arc<Mutex<Vec<PendingClear>>>,
        cdp_ledger: Arc<Mutex<crate::cdp_ledger::EngineCdpLedger>>,
        passkeys: Arc<crate::passkeys::TabPasskeyGate>,
        media_gate: Arc<crate::media_gate::TabMediaGate>,
        notification_gate: Arc<crate::notification_gate::TabNotificationGate>,
        clipboard_gate: Arc<crate::clipboard_gate::TabClipboardGate>,
        allowed_ext_ids: Arc<Mutex<Vec<String>>>,
        context_ready: Arc<std::sync::atomic::AtomicBool>,
        prompt_shared: crate::prompts::PromptShared,
        input_guard: Arc<Mutex<crate::input_guard::InputGuard>>,
        emulation_scale: Arc<std::sync::atomic::AtomicU32>,
        next_cdp_id: Arc<std::sync::atomic::AtomicI32>,
        prompts: Arc<crate::prompts::TabPrompts>,
        permissions: Arc<crate::permissions::PermissionTracker>,
        extensions_active: bool,
        events: std::sync::mpsc::Sender<crate::Event>,
        out_tx: Output,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            browser: Option<&mut Browser>,
            message_id: i32,
            success: i32,
            result: Option<&[u8]>,
        ) {
            if message_id >= crate::frame_authority::INTERNAL_ID_BASE {
                let body = result.and_then(|b| serde_json::from_slice::<Value>(b).ok());
                let send = self.cdp_ledger.lock().unwrap().on_internal_reply(message_id, success != 0, body.as_ref());
                self.send_all(browser.as_deref(), send.unwrap_or_default());
                return;
            }
            if message_id >= crate::extensions::sw_wake::SW_WAKE_ID_BASE {
                return;
            }
            if message_id >= crate::passkeys::PASSKEY_ID_BASE {
                if message_id >= crate::clipboard_gate::CLIPBOARD_GATE_ID_BASE {
                    let body = result.and_then(|b| serde_json::from_slice::<Value>(b).ok());
                    if let Some(rm) = self.clipboard_gate.on_reply(message_id, success != 0, body.as_ref()) {
                        self.send_all(browser.as_deref(), vec![rm]);
                    }
                    if let Some(url) = self.clipboard_gate.take_first_load()
                        && let Some(frame) = browser.as_deref().and_then(cef::ImplBrowser::main_frame)
                    {
                        self.prompts.set_pending_top_url(Some(url.clone()));
                        cef::ImplFrame::load_url(&frame, Some(&cef::CefString::from(url.as_str())));
                    }
                    return;
                }
                if self.passkeys.on_reply(message_id, success != 0) {
                    let _ = self.out_tx.send(EngineMsg::Refused {
                        tab_id: self.tab_id,
                        reason: "browser.passkeys.unrestricted".to_owned(),
                    });
                }
                // Agent commands held for this reply run (or are refused) on the loop's next turn
                // (`EngineManager::release_passkey_held`).
                let _ = self.events.send(crate::Event::Pump(0));
                return;
            }
            let Some(pending) = self.id_map.lock().unwrap().remove(&message_id) else {
                return;
            };
            if matches!(pending, CdpPending::InputGuard) {
                if let Some(req_id) = self.input_guard.lock().unwrap().active_request_id(message_id)
                    && self.refuse_reply(browser.as_deref(), req_id, Some(&Reach::WholeTab))
                {
                    let (evict, wake) = self.input_guard.lock().unwrap().abort_msg_id(self.tab_id, message_id, &self.out_tx, None);
                    {
                        let mut map = self.id_map.lock().unwrap();
                        for mid in evict { map.remove(&mid); }
                    }
                    if wake {
                        let _ = self.events.send(crate::Event::Pump(0));
                    }
                    return;
                }
                let val = result.and_then(|b| serde_json::from_slice::<Value>(b).ok()).unwrap_or(Value::Null);
                let wake = self.input_guard.lock().unwrap().on_reply(
                    self.tab_id,
                    message_id,
                    success != 0,
                    val,
                    browser.as_deref(),
                    self.is_popup,
                    self.isolated,
                    self.is_preview,
                    &self.snap,
                    &self.prompts,
                    &self.permissions,
                    &self.cdp_ledger,
                    &self.emulation_scale,
                    &self.next_cdp_id,
                    &self.out_tx,
                );
                if wake {
                    let _ = self.events.send(crate::Event::Pump(0));
                }
                return;
            }
            if let CdpPending::CopyImageContent { .. } | CdpPending::CopyImageWorld { .. } | CdpPending::CopyImageEncode = pending {
                // A reply larger than any image the engine sends on is dropped before it is parsed.
                let fits = result.is_some_and(|b| b.len() <= crate::menu_extras::MAX_COPIED_IMAGE_B64 + 65_536);
                if success != 0 && fits {
                    let val = result.and_then(|b| serde_json::from_slice::<Value>(b).ok()).unwrap_or(Value::Null);
                    self.copy_image_step(browser.as_deref(), pending, &val);
                }
                return;
            }
            // Guard 1b (reply-side choke point): re-check committed origin uniformly before ANY
            // `CdpPending` variant (`Raw`, `CookieCount`, `ClearSite`) leaves the engine.
            let (CdpPending::Raw { request_id, .. }
            | CdpPending::CookieCount(request_id)
            | CdpPending::ClearSite { request_id, .. }) = &pending
            else {
                unreachable!()
            };
            let reach = match &pending {
                CdpPending::Raw { reach, .. } => reach.as_ref(),
                _ => None,
            };
            let is_tracing_start = matches!(pending, CdpPending::Raw { is_tracing_start: true, .. });
            if self.refuse_reply(browser.as_deref(), *request_id, reach) {
                if is_tracing_start {
                    self.release_trace_start(browser.as_deref(), success != 0);
                }
                return;
            }
            if let CdpPending::Raw { request_id, max_bytes, typed, reach, is_tracing_start } = pending {
                // Enforce hard byte cap BEFORE parsing JSON or forwarding across IPC.
                if result.is_some_and(|b| b.len() > max_bytes) {
                    if is_tracing_start {
                        self.release_trace_start(browser.as_deref(), success != 0);
                    }
                    return self.too_big(request_id, max_bytes);
                }
                let val = result
                    .and_then(|b| serde_json::from_slice::<Value>(b).ok())
                    .unwrap_or(Value::Null);
                return self.finish_reply(browser.as_deref(), request_id, typed, reach.is_some(), is_tracing_start, success != 0, val);
            }
            let val = result
                .and_then(|b| serde_json::from_slice::<Value>(b).ok())
                .unwrap_or(Value::Null);
            match pending {
                CdpPending::Raw { .. }
                | CdpPending::InputGuard
                | CdpPending::CopyImageContent { .. }
                | CdpPending::CopyImageWorld { .. }
                | CdpPending::CopyImageEncode => unreachable!(),
                CdpPending::CookieCount(req_id) => {
                    let count = val.get("cookies").and_then(Value::as_array).map_or(0, Vec::len);
                    let _ = self.out_tx.send(EngineMsg::CdpResponse {
                        tab_id: self.tab_id,
                        request_id: req_id,
                        ok: success != 0,
                        result: serde_json::json!({ "cookies": count }),
                    });
                }
                CdpPending::ClearSite { request_id, origin } => {
                    if success == 0 {
                        let _ = self.out_tx.send(EngineMsg::CdpResponse {
                            tab_id: self.tab_id,
                            request_id,
                            ok: false,
                            result: val,
                        });
                        return;
                    }
                    let cookies = val
                        .get("cookies")
                        .and_then(Value::as_array)
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|c| {
                                    Some((
                                        c.get("name")?.as_str()?.to_owned(),
                                        c.get("domain")?.as_str()?.to_owned(),
                                        c.get("path")?.as_str().unwrap_or("/").to_owned(),
                                    ))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    self.pending_clears.lock().unwrap().push(PendingClear {
                        tab_id: self.tab_id,
                        request_id,
                        origin,
                        cookies,
                    });
                }
            }
        }

        fn on_dev_tools_message(
            &self,
            browser: Option<&mut Browser>,
            message: Option<&[u8]>,
        ) -> ::std::os::raw::c_int {
            let Some(raw) = message else { return 0 };
            // Pre-parse size gate using strict depth-1 top-level JSON scanning (zero AST allocation):
            // - Events (`method` at depth 1) over `MAX_CDP_RESPONSE_BYTES` (16 MiB) are dropped
            //   UNPARSED before `serde_json::from_slice` so `console.log("A".repeat(100e6))` never
            //   allocates a giant JSON AST (all bookkeeping events stay well below 16 MiB; a
            //   `Target.attachedToTarget` carries a URL Chromium caps at 2 MiB, so its resume below
            //   is never skipped).
            // - Top-level replies (`id`, no `method`, no `sessionId`) return `0` immediately so
            //   CEF delivers them ONCE to `on_dev_tools_method_result` (pre-parse cap check).
            // - Flattened OOPIF replies (`id` + `sessionId`) over `max_bytes` reject BEFORE parse.
            if raw.len() > crate::cdp_ledger::MAX_CDP_EVAL_RESPONSE_BYTES {
                let top = crate::cdp_ledger::scan_top_level_cdp(raw);
                if top.has_method {
                    if raw.len() > crate::cdp_ledger::MAX_CDP_RESPONSE_BYTES {
                        return 1;
                    }
                } else if let Some(msg_id) = top.id {
                    if !top.has_session_id {
                        return 0;
                    }
                    let over = {
                        let mut map = self.id_map.lock().unwrap();
                        match map.get(&msg_id) {
                            Some(CdpPending::Raw { max_bytes, .. }) if raw.len() > *max_bytes => {
                                map.remove(&msg_id)
                            }
                            Some(CdpPending::InputGuard)
                                if raw.len() > crate::cdp_ledger::MAX_CDP_RESPONSE_BYTES =>
                            {
                                map.remove(&msg_id)
                            }
                            _ => None,
                        }
                    };
                    if let Some(CdpPending::Raw { request_id, max_bytes, .. }) = over {
                        self.too_big(request_id, max_bytes);
                        return 1;
                    }
                    if matches!(over, Some(CdpPending::InputGuard)) {
                        let (evict, wake) = self.input_guard.lock().unwrap().abort_msg_id(
                            self.tab_id,
                            msg_id,
                            &self.out_tx,
                            Some("Input probe response exceeded size limit."),
                        );
                        {
                            let mut map = self.id_map.lock().unwrap();
                            for mid in evict { map.remove(&mid); }
                        }
                        if wake {
                            let _ = self.events.send(crate::Event::Pump(0));
                        }
                        return 1;
                    }
                    if raw.len() > crate::cdp_ledger::MAX_CDP_RESPONSE_BYTES {
                        return 1;
                    }
                } else if raw.len() > crate::cdp_ledger::MAX_CDP_RESPONSE_BYTES {
                    return 1;
                }
            }
            let Ok(mut val) = serde_json::from_slice::<Value>(raw) else { return 0 };
            // Flattened OOPIF responses carry both `"id"` and `"sessionId"`; CEF's
            // `on_dev_tools_method_result` omits them, so handle them directly here.
            if let Some(msg_id) = val.get("id").and_then(Value::as_i64) {
                if val.get("sessionId").is_none() {
                    return 0;
                }
                let (ok, payload) = match val.get_mut("error") {
                    Some(err) => (false, err.take()),
                    None => (true, val.get_mut("result").map(Value::take).unwrap_or(Value::Null)),
                };
                let msg_id = msg_id as i32;
                if msg_id >= crate::frame_authority::INTERNAL_ID_BASE {
                    let send = self.cdp_ledger.lock().unwrap().on_internal_reply(msg_id, ok, Some(&payload));
                    self.send_all(browser.as_deref(), send.unwrap_or_default());
                    return 1;
                }
                if msg_id >= crate::extensions::sw_wake::SW_WAKE_ID_BASE {
                    return 1;
                }
                if msg_id >= crate::clipboard_gate::CLIPBOARD_GATE_ID_BASE {
                    if let Some(rm) = self.clipboard_gate.on_reply(msg_id, ok, Some(&payload)) {
                        self.send_all(browser.as_deref(), vec![rm]);
                    }
                    return 1;
                }
                if msg_id >= crate::passkeys::PASSKEY_ID_BASE {
                    return 1;
                }
                let pending = self.id_map.lock().unwrap().remove(&msg_id);
                if matches!(pending, Some(CdpPending::InputGuard)) {
                    if let Some(req_id) = self.input_guard.lock().unwrap().active_request_id(msg_id)
                        && self.refuse_reply(browser.as_deref(), req_id, Some(&Reach::WholeTab))
                    {
                        let (evict, wake) = self.input_guard.lock().unwrap().abort_msg_id(self.tab_id, msg_id, &self.out_tx, None);
                        {
                            let mut map = self.id_map.lock().unwrap();
                            for mid in evict { map.remove(&mid); }
                        }
                        if wake {
                            let _ = self.events.send(crate::Event::Pump(0));
                        }
                        return 1;
                    }
                    let wake = self.input_guard.lock().unwrap().on_reply(
                        self.tab_id,
                        msg_id,
                        ok,
                        payload,
                        browser.as_deref(),
                        self.is_popup,
                        self.isolated,
                        self.is_preview,
                        &self.snap,
                        &self.prompts,
                        &self.permissions,
                        &self.cdp_ledger,
                        &self.emulation_scale,
                        &self.next_cdp_id,
                        &self.out_tx,
                    );
                    if wake {
                        let _ = self.events.send(crate::Event::Pump(0));
                    }
                    return 1;
                }
                let Some(CdpPending::Raw { request_id, max_bytes, typed, reach, .. }) = pending else {
                    return 0;
                };
                if self.refuse_reply(browser.as_deref(), request_id, reach.as_ref()) {
                    return 1;
                }
                if raw.len() > max_bytes {
                    self.too_big(request_id, max_bytes);
                    return 1;
                }
                self.finish_reply(browser.as_deref(), request_id, typed, reach.is_some(), false, ok, payload);
                return 1;
            }
            let Some(method) = val.get("method").and_then(Value::as_str).map(str::to_owned) else {
                return 0;
            };
            let session_id = val.get("sessionId").and_then(Value::as_str).map(str::to_owned);
            let mut params = val
                .as_object_mut()
                .and_then(|o| o.remove("params"))
                .unwrap_or(Value::Null);
            if method == "Overlay.inspectNodeRequested"
                && let Some(backend_id) = params["backendNodeId"].as_i64()
            {
                self.internal_picked.lock().unwrap().push(backend_id);
            }
            if method == "Target.detachedFromTarget"
                && let Some(detached_sid) = params.get("sessionId").and_then(Value::as_str)
            {
                let (evict, wake) = self
                    .input_guard
                    .lock()
                    .unwrap()
                    .abort_session(self.tab_id, detached_sid, &self.out_tx);
                if !evict.is_empty() {
                    let mut map = self.id_map.lock().unwrap();
                    for mid in evict { map.remove(&mid); }
                }
                if wake {
                    let _ = self.events.send(crate::Event::Pump(0));
                }
            }
            // The ledger sees every event first; a paused child's resume goes out from here,
            // before any filter below.
            let (mut obs, app_attached) = {
                let mut ledger = self.cdp_ledger.lock().unwrap();
                let obs = ledger.observe_event(&method, &params, session_id.as_deref());
                (obs, ledger.app_attached)
            };
            if let Some(child_sid) = obs.new_web_session.as_deref() {
                // Attach the passkey wrapper (and renderer file-chooser intercept when controlled
                // or preview) on the paused OOPIF BEFORE `Runtime.runIfWaitingForDebugger` (the
                // last message in `obs.send`), matching `chromium.ts:619`.
                let resume = obs.send.pop();
                obs.send.extend(self.passkeys.attach_messages(Some(child_sid)));
                obs.send.extend(self.media_gate.attach_messages(Some(child_sid)));
                obs.send.extend(self.notification_gate.attach_messages(Some(child_sid)));
                obs.send.extend(self.clipboard_gate.attach_messages(Some(child_sid)));
                if self.is_preview || self.prompts.agent_owns_dialogs() {
                    obs.send.push(self.passkeys.file_chooser_intercept_msg(true, Some(child_sid)));
                }
                if let Some(r) = resume {
                    obs.send.push(r);
                }
            }
            // Only a picker the intercept cancelled is reported (official `file-choosers.ts:9-11`).
            if method == "Page.fileChooserOpened" && (self.is_preview || self.prompts.agent_owns_dialogs()) {
                let _ = self.out_tx.send(EngineMsg::FileChooserBlocked { tab_id: self.tab_id });
            }
            let (pk_send, pk_consumed) = self.passkeys.observe_event(
                &method,
                &params,
                session_id.as_deref(),
                &self.snap,
                &self.out_tx,
            );
            obs.send.extend(pk_send);
            let (mg_consumed, mg_ask) = self.media_gate.observe_event(&method, &params, session_id.as_deref());
            if let Some(ask) = mg_ask {
                if let Some(deny) = self.hold_media_ask(browser.as_deref(), ask) {
                    obs.send.push(deny);
                }
            }
            let (ng_consumed, ng_effect) = self.notification_gate.observe_event(&method, &params, session_id.as_deref());
            match ng_effect {
                // `page:<tab>` keeps them apart from extensions' own; the origin goes under the
                // page's text, so a page can't pass for the app or another site.
                Some(crate::notification_gate::NotificationEffect::Show { notif_id, origin, title, body }) => {
                    let host = url::Url::parse(&origin).ok().and_then(|u| u.host_str().map(str::to_owned)).unwrap_or(origin);
                    let _ = self.out_tx.send(EngineMsg::ExtensionNotificationShow {
                        ext_id: format!("page:{}", self.tab_id),
                        ext_name: title,
                        notif_id,
                        body: if body.is_empty() { host } else { format!("{body}\n{host}") },
                    });
                }
                Some(crate::notification_gate::NotificationEffect::Close { notif_id }) => {
                    let _ = self.out_tx.send(EngineMsg::ExtensionNotificationClear { ext_id: format!("page:{}", self.tab_id), notif_id });
                }
                None => {}
            }
            let (cb_consumed, cb_sync, cb_ask) = self.clipboard_gate.observe_event(&method, &params, session_id.as_deref());
            obs.send.extend(cb_sync);
            if let Some(ask) = cb_ask {
                obs.send.push(self.answer_clipboard_ask(browser.as_deref(), ask));
            }
            self.send_all(browser.as_deref(), obs.send);
            if pk_consumed || mg_consumed || ng_consumed || cb_consumed || !app_attached {
                return 1;
            }
            for (frame_id, sid) in obs.excluded {
                let _ = self.out_tx.send(EngineMsg::CdpEvent {
                    tab_id: self.tab_id,
                    method: "Rune.frameExcluded".into(),
                    params: serde_json::json!({ "frameId": frame_id, "sessionId": sid }),
                    session_id: None,
                });
            }
            let whole_tab = self.surface(browser.as_deref()) == crate::engine::Surface::WholeTab;
            let blocked = self.blocked_url(browser.as_deref()).is_some();
            if !whole_tab && !blocked {
                for (attach, parent_sid) in obs.announce {
                    let _ = self.out_tx.send(EngineMsg::CdpEvent {
                        tab_id: self.tab_id,
                        method: "Target.attachedToTarget".into(),
                        params: attach,
                        session_id: parent_sid,
                    });
                }
            }
            // `#274` Round-5 §4: with extensions active, `Page.*` events stay in the engine (frames
            // reach the app through pruned `Page.getFrameTree` replies), and pop-up/navigation
            // requests are never forwarded.
            if !obs.forward
                || method.starts_with("ServiceWorker.")
                || (self.extensions_active && method.starts_with("Page."))
                || matches!(method.as_str(), "Page.frameRequestedNavigation" | "Page.windowOpen")
                || whole_tab
                || blocked
            {
                return 1;
            }
            // Browser-wide CDP trace isolation (official `packages/gui-plugins/src/browser/profiling.ts:221-240`):
            // filter `Tracing.dataCollected` strictly to this tab's renderer OS PID before events
            // ever cross IPC (zero browser/GPU/other-tab/remote/preview events).
            if method == "Tracing.dataCollected" {
                let target_pid = self.prompts.trace_pid.load(std::sync::atomic::Ordering::Acquire);
                if target_pid == 0 {
                    return 1;
                }
                let want = u64::from(target_pid);
                let kept = if let Some(arr) = params.get_mut("value").and_then(Value::as_array_mut) {
                    arr.retain(|ev| ev.get("pid").and_then(Value::as_u64) == Some(want));
                    !arr.is_empty()
                } else {
                    false
                };
                if !kept {
                    return 1;
                }
                if let Some(obj) = params.as_object_mut() {
                    obj.insert("rendererPid".into(), serde_json::json!(target_pid));
                }
            } else if method == "Tracing.tracingComplete" {
                let start_pid = self.prompts.trace_pid.swap(0, std::sync::atomic::Ordering::AcqRel);
                self.prompt_shared.release_trace(self.tab_id);
                if start_pid == 0 {
                    return 1;
                }
                let cur_pid = self.prompts.committed_main_renderer_pid(browser.as_deref());
                if let Some(obj) = params.as_object_mut() {
                    if cur_pid != start_pid {
                        obj.insert("dataLossOccurred".into(), serde_json::json!(true));
                    }
                    obj.insert("rendererPid".into(), serde_json::json!(start_pid));
                }
            }
            if let Some(grant) = &self.preview {
                scrub_preview_urls(grant, &mut params);
            }
            let _ = self.out_tx.send(EngineMsg::CdpEvent {
                tab_id: self.tab_id,
                method,
                params,
                session_id,
            });
            1
        }
    }
}

impl TabCdpObserver {
    /// Holds a gated `getUserMedia` for the bar, or returns the deny answer to send at once: a
    /// preview tab, prompts off, an unknown or insecure context, or an embedded frame of another
    /// origin (the same rules as `TabPermissionHandler::on_request_media_access_permission`).
    fn hold_media_ask(&self, browser: Option<&Browser>, ask: crate::media_gate::MediaAsk) -> Option<String> {
        // The live main frame, as `on_request_media_access_permission` reads it: the snapshot can
        // lag a navigation that just committed.
        let live = browser
            .and_then(cef::ImplBrowser::main_frame)
            .map(|f| cef::CefString::from(&cef::ImplFrame::url(&f)).to_string());
        let top = live
            .filter(|u| !u.trim().is_empty())
            .or_else(|| Some(self.snap.lock().unwrap().url.clone()))
            .and_then(|u| crate::prompts::origin_of(&u));
        let origin = ask.origin.as_deref().and_then(crate::prompts::origin_of);
        let ok = !self.is_preview
            && self.context_ready.load(std::sync::atomic::Ordering::Acquire)
            && self.prompt_shared.enabled.load(std::sync::atomic::Ordering::Relaxed)
            && origin.is_some()
            && origin == top
            && origin.as_deref().is_some_and(crate::prompts::is_secure_origin);
        let (Some(origin), Some(top), true) = (origin, top, ok) else {
            return Some(ask.deny_msg);
        };
        let prompt = crate::wire::PermissionPrompt {
            id: self.prompt_shared.next_id(),
            origin,
            top_origin: top,
            kinds: ask.kinds,
            is_media: true,
        };
        self.prompts.hold_media_gate(self.tab_id, prompt, ask.allow_msg, ask.deny_msg, &self.out_tx)
    }

    /// Authoritative browser-process verdict for `navigator.clipboard.write{Text}` matching
    /// official `site-permissions.ts:152-157`, `page-prompts.ts:453,468`, and
    /// `policy.ts:95,111,132-135` (READER-7 N5).
    fn answer_clipboard_ask(&self, browser: Option<&Browser>, ask: crate::clipboard_gate::ClipboardAsk) -> String {
        // Gate on `agent_driven` itself (NOT `agent_owns_dialogs()`): official keeps clipboard
        // writes refused even after human takeover (`control === "reclaimed"`, `policy.ts:132-135`).
        if self.is_preview
            || self.prompts.agent_driven.load(std::sync::atomic::Ordering::Acquire)
            || !self.context_ready.load(std::sync::atomic::Ordering::Acquire)
        {
            return ask.deny_msg;
        }
        let live = browser
            .and_then(cef::ImplBrowser::main_frame)
            .map(|f| cef::CefString::from(&cef::ImplFrame::url(&f)).to_string());
        let top_raw = live
            .filter(|u| !u.trim().is_empty())
            .or_else(|| Some(self.snap.lock().unwrap().url.clone()))
            .unwrap_or_default();
        let req_raw = ask.origin.as_deref().unwrap_or("");
        if crate::extensions::manifest::is_extension_scheme(req_raw) {
            let req_id = crate::extensions::manifest::extension_id_from_url(req_raw);
            let top_id = crate::extensions::manifest::extension_id_from_url(&top_raw);
            let allowed = !self.isolated
                && req_id.is_some()
                && req_id == top_id
                && req_id.is_some_and(|id| {
                    self.clipboard_gate.ext_has_clipboard_write(id)
                        && self.allowed_ext_ids.lock().unwrap().iter().any(|a| a == id)
                });
            return if allowed { ask.allow_msg } else { ask.deny_msg };
        }
        let top = crate::prompts::origin_of(&top_raw);
        let origin = crate::prompts::origin_of(req_raw);
        let allowed = origin.is_some()
            && origin == top
            && origin.as_deref().is_some_and(crate::prompts::is_secure_origin);
        if allowed { ask.allow_msg } else { ask.deny_msg }
    }

    fn surface(&self, browser: Option<&Browser>) -> crate::engine::Surface {
        crate::engine::classify_surface(browser, self.is_popup, &self.snap, &self.prompts, &self.cdp_ledger)
    }

    /// Guard 1b: the tab's committed origin holds an inherited `Default` grant (controlled tabs only).
    fn blocked_url(&self, browser: Option<&Browser>) -> Option<Option<String>> {
        self.permissions
            .tab_blocked_url(browser, self.isolated, self.is_preview, &self.snap, &self.prompts)
    }

    /// The next step of the person's Copy image, from the last step's reply (`menu_extras.rs`).
    /// Each step's message waits in the ledger's outbox, like every observer-made message.
    fn copy_image_step(&self, browser: Option<&Browser>, step: CdpPending, reply: &Value) {
        let next_id = || self.next_cdp_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let next = match step {
            CdpPending::CopyImageContent { frame_id } => crate::menu_extras::resource_base64(reply).map(|b64| {
                let id = next_id();
                (id, CdpPending::CopyImageWorld { b64 }, crate::menu_extras::world_request(id, &frame_id))
            }),
            CdpPending::CopyImageWorld { b64 } => {
                let id = next_id();
                crate::menu_extras::encode_request(id, reply, &b64).map(|msg| (id, CdpPending::CopyImageEncode, msg))
            }
            _ => {
                if let Some(png_b64) = crate::menu_extras::png_result(reply) {
                    let _ = self.out_tx.send(EngineMsg::CopiedImage { tab_id: self.tab_id, png_b64 });
                }
                None
            }
        };
        if let Some((id, pending, msg)) = next {
            self.id_map.lock().unwrap().insert(id, pending);
            self.send_all(browser, vec![msg]);
        }
    }

    /// The granted origin an agent call's `reach` touches now, on a tab Guard 1 covers.
    fn reach_granted(&self, reach: &Reach) -> Option<Option<String>> {
        if self.isolated || self.is_preview || !self.prompts.agent_driven.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let ledger = self.cdp_ledger.lock().unwrap();
        ledger.granted_origin(reach, &|o| self.permissions.has_inherited_default_grant(o))
    }

    /// Replaces a reply with the generic refusal when Guard 1b holds, or when a frame the call
    /// reached holds an inherited grant by the time it answers (a grant or navigation mid-flight);
    /// `true` when refused.
    fn refuse_reply(&self, browser: Option<&Browser>, request_id: u64, reach: Option<&Reach>) -> bool {
        let url = match self.blocked_url(browser) {
            Some(url) => url,
            None => match reach.and_then(|r| self.reach_granted(r)) {
                Some(origin) => origin,
                None => return false,
            },
        };
        let _ = self.out_tx.send(EngineMsg::ControlledPermissionRefused { tab_id: self.tab_id, url });
        let _ = self.out_tx.send(EngineMsg::CdpResponse {
            tab_id: self.tab_id,
            request_id,
            ok: false,
            result: serde_json::json!({ "message": crate::permissions::INHERITED_PERMISSION_MSG }),
        });
        true
    }

    /// CEF's DevTools observer list forbids re-entrancy, so nothing is sent from a callback: the
    /// messages wait in the ledger's outbox for [`flush_outbox`] after this loop turn, and an
    /// immediate `Event::Pump(0)` wakes the loop outside busy windows (e.g. macOS idle).
    fn release_trace_start(&self, browser: Option<&Browser>, end_chromium: bool) {
        self.prompts.trace_pid.store(0, std::sync::atomic::Ordering::Release);
        self.prompt_shared.release_trace(self.tab_id);
        if end_chromium {
            let id = self.next_cdp_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let msg = serde_json::json!({ "id": id, "method": "Tracing.end", "params": {} }).to_string();
            self.send_all(browser, vec![msg]);
        }
    }

    fn send_all(&self, _browser: Option<&Browser>, messages: Vec<String>) {
        if !messages.is_empty() {
            self.cdp_ledger.lock().unwrap().outbox.extend(messages);
            let _ = self.events.send(crate::Event::Pump(0));
        }
    }

    fn too_big(&self, request_id: u64, max_bytes: usize) {
        let _ = self.out_tx.send(EngineMsg::CdpResponse {
            tab_id: self.tab_id,
            request_id,
            ok: false,
            result: serde_json::json!({
                "message": format!("CDP response exceeded maximum allowed size ({max_bytes} bytes).")
            }),
        });
    }

    /// Re-checks the tab at reply time so an extension frame appearing mid-flight never leaks into
    /// a response (`review-rd274.md` Round-5 §2): a raw call is refused on any extension frame, a
    /// typed call on a whole extension tab or when the ledger's after-check fails.
    #[allow(clippy::too_many_arguments)]
    fn finish_reply(
        &self,
        browser: Option<&Browser>,
        request_id: u64,
        typed: Option<(Post, Option<String>)>,
        grants_checked: bool,
        is_tracing_start: bool,
        ok: bool,
        mut val: Value,
    ) {
        // A hit test answers with the frame under the point, which the call itself never named.
        if grants_checked
            && let Some((Post::Hit, _)) = &typed
            && let Some(fid) = val.get("frameId").and_then(Value::as_str)
            && self.refuse_reply(browser, request_id, Some(&Reach::Frames(vec![fid.to_owned()])))
        {
            return;
        }
        let surface = self.surface(browser);
        let refused = match &typed {
            None => surface != crate::engine::Surface::Clean,
            Some(_) => surface == crate::engine::Surface::WholeTab,
        };
        let (checked, wake) = if refused {
            (Err(EXTENSION_SURFACE.to_owned()), false)
        } else if let (Some((post, sid)), true) = (&typed, ok) {
            let mut ledger = self.cdp_ledger.lock().unwrap();
            let before = ledger.outbox.len();
            let res = ledger.finish_typed(post, sid.as_deref(), &mut val);
            (res, ledger.outbox.len() > before)
        } else {
            (Ok(()), false)
        };
        if wake {
            let _ = self.events.send(crate::Event::Pump(0));
        }
        if is_tracing_start && (!ok || checked.is_err()) {
            self.release_trace_start(browser, ok);
        }
        let (ok, result) = match checked {
            Ok(()) => (ok, val),
            Err(message) => (false, serde_json::json!({ "message": message })),
        };
        let _ = self.out_tx.send(EngineMsg::CdpResponse { tab_id: self.tab_id, request_id, ok, result });
    }
}

/// A preview's internal `oc-preview://` addresses reach the agent as the page's display URL, as
/// official maps console text, sources and request URLs (`diagnostics.ts:43,47,157`).
fn scrub_preview_urls(grant: &crate::preview::PreviewGrant, value: &mut Value) {
    match value {
        Value::String(s) if s.to_ascii_lowercase().contains("oc-preview:") => *s = replace_preview_urls(grant, s),
        Value::Array(items) => items.iter_mut().for_each(|v| scrub_preview_urls(grant, v)),
        Value::Object(map) => map.values_mut().for_each(|v| scrub_preview_urls(grant, v)),
        _ => {}
    }
}

fn replace_preview_urls(grant: &crate::preview::PreviewGrant, s: &str) -> String {
    // ASCII lowercasing keeps every byte offset.
    let lower = s.to_ascii_lowercase();
    let mut out = String::with_capacity(s.len());
    let mut at = 0;
    while let Some(off) = lower[at..].find("oc-preview:") {
        let start = at + off;
        out.push_str(&s[at..start]);
        let end = s[start..].find(|c: char| c.is_whitespace() || "\"'<>()[]".contains(c)).map_or(s.len(), |e| start + e);
        out.push_str(&grant.sanitize_reported_url(&s[start..end]));
        at = end;
    }
    out.push_str(&s[at..]);
    out
}