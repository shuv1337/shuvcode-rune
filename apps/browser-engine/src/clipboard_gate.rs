//! Per-tab clipboard write gate matching official `site-permissions.ts:152-157`,
//! `page-prompts.ts:453,468`, and `policy.ts:95,111,132-135` (READER-7 N5).
//!
//! Chromium's Chrome runtime auto-grants `clipboard-sanitized-write` on user activation without
//! invoking CEF's `on_show_permission_prompt`, so an agent's synthetic click (`Input.dispatch*`)
//! would let a page overwrite the person's OS clipboard via `navigator.clipboard.writeText()`,
//! `navigator.clipboard.write()`, or `document.execCommand("copy"|"cut")`.
//!
//! Parity contract:
//! - Agent-driven tabs (`agent_driven == true`, staying refused even after human takeover /
//!   `user_reclaimed`, matching official `policy.ts:132-135`) and preview tabs (`is_preview`):
//!   every page clipboard write is refused (`NotAllowedError` / `execCommand -> false` /
//!   `permissions.query({ name: "clipboard-write" }) -> "denied"`). Tabs born `agent_driven` or
//!   `is_preview` also disable `BrowserSettings.javascript_access_clipboard` &
//!   `javascript_dom_paste` in Blink C++. Capture-phase `copy`/`cut` listeners call
//!   `stopImmediatePropagation()` ONLY (never `preventDefault()`), so the person's own `Ctrl+C` /
//!   right-click Copy still copies the selected text while page `copy` listeners cannot poison
//!   `ev.clipboardData`.
//! - Human-driven tabs (`!is_preview && !agent_driven`): same-origin trustworthy contexts
//!   (`https:` or loopback) work as in Chrome; cross-origin frames and insecure origins are refused.
//! - Extension pages and popups: allowed iff the extension declared `"clipboardWrite"` in its
//!   manifest (and the tab is non-isolated, non-preview, non-agent-driven).
//! - The same script makes `window.print()` a no-op on blocked web pages (agent-driven and preview
//!   tabs; extension pages excepted): see the `print` block in `clipboard_gate_script`.
//! - Clipboard reads remain separately gated by `TabPermissionHandler::on_show_permission_prompt`
//!   (`CEF_PERMISSION_TYPE_CLIPBOARD` -> `PermissionKind::ClipboardRead`) and inherited-grant checks.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use serde::Deserialize;
use serde_json::{Value, json};

/// Internal CDP message ID range owned by `TabClipboardGate` (disjoint above
/// `TabPasskeyGate` `1_500_000_000..1_550_000_000` and `TabMediaGate`
/// `1_550_000_000..1_575_000_000`, below `frame_authority::INTERNAL_ID_BASE = 1_600_000_000`).
pub const CLIPBOARD_GATE_ID_BASE: i32 = 1_575_000_000;

/// Builds `BrowserSettings` for a pane tab, permanently disabling Blink C++
/// `javascript_access_clipboard` & `javascript_dom_paste` on tabs born preview or agent-driven (N5).
pub fn browser_settings(background_color: u32, disable_js_clipboard: bool) -> cef::BrowserSettings {
    let clip = if disable_js_clipboard {
        cef::sys::cef_state_t::STATE_DISABLED
    } else {
        cef::sys::cef_state_t::STATE_DEFAULT
    };
    cef::BrowserSettings {
        windowless_frame_rate: crate::input_fps::pane_fps(),
        background_color,
        javascript_access_clipboard: clip.into(),
        javascript_dom_paste: clip.into(),
        ..Default::default()
    }
}

/// Page-side clipboard write gate injected via `Page.addScriptToEvaluateOnNewDocument`.
///
/// Security invariants:
/// - `secret` is closed inside the IIFE and never stored on `globalThis`.
/// - `answer` and `sync_fn` are defined non-writable, non-configurable, non-enumerable, and
///   require `tok === secret`, so page JS cannot forge an allow or unblock even after inspecting
///   `Object.getOwnPropertyNames(globalThis)`.
/// - `isSameOriginTop` and `isTrustworthy` are frozen ONCE at IIFE boot before page JS runs,
///   defending against `Object.defineProperty(window, "isSecureContext" | "origin", ...)` spoofing.
/// - Binding payloads are built with string concatenation (`'{"id":' + id + '}'`), avoiding
///   `JSON.stringify` / `Object.prototype.toJSON` tampering.
/// - Capture-phase `copy`/`cut` listeners call `stopImmediatePropagation()` ONLY (never
///   `preventDefault()`), preserving human `Ctrl+C` / right-click Copy selection copying while
///   preventing page listeners from overwriting `ev.clipboardData`.
pub fn clipboard_gate_script(
    binding: &str,
    answer: &str,
    sync_fn: &str,
    secret: &str,
    initial_blocked: bool,
    ext_clipboard_write: bool,
) -> String {
    let binding_json = serde_json::to_string(binding).unwrap_or_else(|_| "\"_cb\"".to_owned());
    let answer_json = serde_json::to_string(answer).unwrap_or_else(|_| "\"_ca\"".to_owned());
    let sync_json = serde_json::to_string(sync_fn).unwrap_or_else(|_| "\"_cs\"".to_owned());
    let secret_json = serde_json::to_string(secret).unwrap_or_else(|_| "\"_ck\"".to_owned());
    let init_js = if initial_blocked { "true" } else { "false" };
    let ext_js = if ext_clipboard_write { "true" } else { "false" };
    format!(
        r#"(() => {{
  "use strict"
  const secret = {secret_json}
  const ask = globalThis[{binding_json}]
  if (typeof ask === "function") {{
    try {{ delete globalThis[{binding_json}] }} catch {{}}
  }}
  if (Object.getOwnPropertyDescriptor(globalThis, {sync_json})) {{
    try {{ globalThis[{sync_json}]({init_js}, {ext_js}, secret) }} catch {{}}
    return
  }}
  const apply = Reflect.apply
  const reflectGet = Reflect.get
  const define = Object.defineProperty
  const ProxyCtor = Proxy
  const StringCtor = String
  const fnBind = Function.prototype.bind
  const strToLower = String.prototype.toLowerCase
  const MapCtor = Map
  const mapGet = Map.prototype.get
  const mapSet = Map.prototype.set
  const mapDel = Map.prototype.delete
  const PromiseCtor = Promise
  const promiseThen = Promise.prototype.then
  const promiseReject = Promise.reject.bind(Promise)
  const DomExc = DOMException
  const addListener = EventTarget.prototype.addEventListener
  const stopImm = Event.prototype.stopImmediatePropagation
  const waiting = new MapCtor()
  let next = 0
  let blocked = {init_js}
  let extWriteAllowed = {ext_js}
  const denied = () => new DomExc("Write permission denied.", "NotAllowedError")

  // Freeze frame-origin and secure-context checks ONCE at document boot (before page JS can
  // redefine configurable `window.isSecureContext` or `[Replaceable]` `window.origin`).
  const isSameOriginTop = (() => {{
    try {{
      if (globalThis.top === globalThis) return true
      return globalThis.top.origin === globalThis.origin && globalThis.origin !== "null"
    }} catch {{
      return false
    }}
  }})()

  const isExtPage = (() => {{
    try {{
      return !!(globalThis.location && globalThis.location.protocol === "chrome-extension:")
    }} catch {{
      return false
    }}
  }})()

  const isSecureWeb = (() => {{
    try {{
      return !isExtPage && globalThis.isSecureContext === true
    }} catch {{
      return false
    }}
  }})()

  define(globalThis, {answer_json}, {{
    value: (id, allowed, tok) => {{
      if (tok !== secret) return
      const resolve = apply(mapGet, waiting, [id])
      if (!resolve) return
      apply(mapDel, waiting, [id])
      resolve(allowed === true)
    }},
    writable: false,
    configurable: false,
    enumerable: false,
  }})

  define(globalThis, {sync_json}, {{
    value: (nextBlocked, nextExtAllowed, tok) => {{
      if (tok !== secret) return
      blocked = nextBlocked === true
      if (typeof nextExtAllowed === "boolean") extWriteAllowed = nextExtAllowed
    }},
    writable: false,
    configurable: false,
    enumerable: false,
  }})

  const canWriteSync = () => !blocked && isSameOriginTop && (isExtPage ? extWriteAllowed : isSecureWeb)

  const clipProto = globalThis.Clipboard && Clipboard.prototype
  if (clipProto) {{
    for (const name of ["writeText", "write"]) {{
      const orig = clipProto[name]
      if (typeof orig !== "function") continue
      const wrapped = new ProxyCtor(orig, {{
        apply(target, self, args) {{
          if (!canWriteSync() || typeof ask !== "function") {{
            return promiseReject(denied())
          }}
          const p = new PromiseCtor((resolve) => {{
            const id = ++next
            apply(mapSet, waiting, [id, resolve])
            ask('{{"id":' + id + '}}')
          }})
          return apply(promiseThen, p, [
            (allowed) => (allowed ? apply(target, self, args) : promiseReject(denied())),
          ])
        }},
      }})
      define(clipProto, name, {{ value: wrapped, writable: true, configurable: true, enumerable: true }})
    }}
  }}

  const docProto = globalThis.Document && Document.prototype
  if (docProto && typeof docProto.execCommand === "function") {{
    const origExec = docProto.execCommand
    const wrappedExec = new ProxyCtor(origExec, {{
      apply(target, self, args) {{
        let cmd = ""
        try {{
          cmd = apply(strToLower, StringCtor(args[0] ?? ""), [])
        }} catch {{
          return false
        }}
        if ((cmd === "copy" || cmd === "cut") && !canWriteSync()) {{
          return false
        }}
        return apply(target, self, args)
      }},
    }})
    define(docProto, "execCommand", {{ value: wrappedExec, writable: true, configurable: true, enumerable: true }})
  }}

  // Block page `copy`/`cut` listeners from hijacking `ev.clipboardData` while blocked, WITHOUT
  // calling `preventDefault()` so the person's own Ctrl+C / right-click Copy still copies selection.
  for (const evName of ["copy", "cut"]) {{
    apply(addListener, globalThis, [
      evName,
      (ev) => {{
        if (!canWriteSync()) {{
          try {{ apply(stopImm, ev, []) }} catch {{}}
        }}
      }},
      true,
    ])
  }}

  // Print (WS-1): on the same tabs a page's `print()` does nothing, in every frame this script
  // reaches (new documents and `about:blank` child frames alike). Its native dialog stops the whole
  // engine until a person closes it, and on these tabs nobody may be there. `print` is an own
  // property of each window (no prototype copy to recover), redefined non-writable and
  // non-configurable before page JS runs. Extension pages keep printing. The browser's own print
  // command is refused in the engine (`tab_client::TabCommandHandler`). Tabs born gated were locked
  // already in the renderer (`opencode_browser_engine_helper::lock_print`, no longer configurable):
  // this covers a tab that becomes gated later.
  const origPrint = globalThis.print
  const printDesc = Object.getOwnPropertyDescriptor(globalThis, "print")
  if (typeof origPrint === "function" && printDesc && printDesc.configurable) {{
    define(globalThis, "print", {{
      value: new ProxyCtor(origPrint, {{
        apply(target, self, args) {{
          if (blocked && !isExtPage) return undefined
          return apply(target, self, args)
        }},
      }}),
      writable: false,
      configurable: false,
      enumerable: true,
    }})
  }}

  const permProto = globalThis.Permissions && Permissions.prototype
  if (permProto && typeof permProto.query === "function") {{
    const origQuery = permProto.query
    const statusNameGetter = globalThis.PermissionStatus && Object.getOwnPropertyDescriptor(PermissionStatus.prototype, "name")?.get
    const wrappedQuery = new ProxyCtor(origQuery, {{
      apply(target, self, args) {{
        let name = ""
        try {{
          name = args[0] && typeof args[0] === "object" ? StringCtor(args[0].name ?? "") : ""
        }} catch {{}}
        return apply(promiseThen, apply(target, self, args), [
          (status) => {{
            let resolvedName = ""
            if (status && statusNameGetter) {{
              try {{ resolvedName = apply(statusNameGetter, status, []) }} catch {{}}
            }}
            const isClipWrite = name === "clipboard-write" || resolvedName === "clipboard-write" || resolvedName === "clipboard_write"
            if (!isClipWrite || canWriteSync()) return status
            return new ProxyCtor(status, {{
              get(t, prop) {{
                if (prop === "state") return "denied"
                const v = reflectGet(t, prop, t)
                return typeof v === "function" ? apply(fnBind, v, [t]) : v
              }},
            }})
          }},
        ])
      }},
    }})
    define(permProto, "query", {{ value: wrappedQuery, writable: true, configurable: true, enumerable: true }})
  }}
}})()"#
    )
}

#[derive(Deserialize)]
struct Ask {
    id: u32,
}

/// A pending async `navigator.clipboard.write{Text}` call awaiting the browser-process verdict.
pub struct ClipboardAsk {
    pub origin: Option<String>,
    pub allow_msg: String,
    pub deny_msg: String,
}

#[derive(Default)]
struct ContextState {
    /// Live `(session_id, execution_context_id)` -> origin (default V8 worlds only).
    contexts: HashMap<(String, i64), String>,
    /// Live attached sessions (`""` for root + every non-extension web OOPIF `session_id`).
    sessions: HashSet<String>,
    /// Current `Page.addScriptToEvaluateOnNewDocument` identifier per session.
    script_ids: HashMap<String, String>,
    /// In-flight `Page.addScriptToEvaluateOnNewDocument` CDP msg id -> `(session_id, generation)`.
    pending_adds: HashMap<i32, (String, u64)>,
}

/// The URL a new tab must hold until its gate script is in place (#643 first-load race, READER-7 22 Nov): a tab born
/// agent-driven opens on `about:blank` and `TabClipboardGate::take_first_load` loads it once the root script is
/// confirmed, so its first document never runs before the wrap. Not for tabs whose context already defers the first
/// load, extension URLs, or `about:blank` itself.
pub fn first_load_hold(agent_driven: bool, context_defers: bool, extension_url: bool, url: &str) -> Option<String> {
    (agent_driven && !context_defers && !extension_url && !url.trim().eq_ignore_ascii_case("about:blank")).then(|| url.to_owned())
}

/// A new tab's `(initial URL, address-bar URL)`, setting `hold` to the URL it must hold (`first_load_hold`).
pub fn first_load_urls(agent_driven: bool, context_defers: bool, extension_url: bool, url: &str, hold: &mut Option<String>) -> (String, String) {
    *hold = first_load_hold(agent_driven, context_defers, extension_url, url);
    let init = if context_defers || hold.is_some() { "about:blank" } else { url };
    (init.to_owned(), hold.as_deref().unwrap_or(init).to_owned())
}

pub struct TabClipboardGate {
    pass_through_popup: bool,
    is_preview: bool,
    blocked: AtomicBool,
    ext_clipboard_write_ids: Arc<Mutex<HashSet<String>>>,
    generation: AtomicU64,
    binding: String,
    answer: String,
    sync_fn: String,
    secret: String,
    next_id: AtomicI32,
    state: Mutex<ContextState>,
    /// The first URL of a tab born agent-driven: it opens on `about:blank` and loads this only
    /// once the root session's gate script is confirmed, so the first document cannot keep a
    /// `Clipboard.prototype.writeText` reference from before the wrap (#643 first-load race).
    first_load: Mutex<Option<String>>,
    root_armed: AtomicBool,
}

impl TabClipboardGate {
    pub fn new(
        is_popup: bool,
        is_preview: bool,
        agent_driven: bool,
        initial_ext_cw: bool,
        ext_clipboard_write_ids: Arc<Mutex<HashSet<String>>>,
        first_load: Option<String>,
    ) -> Self {
        // Extension popups that explicitly declared `"clipboardWrite"` (e.g. Bitwarden) pass
        // through without page-side wrapping; popups without `"clipboardWrite"` stay gated.
        let pass_through_popup = is_popup && initial_ext_cw && !is_preview && !agent_driven;
        let initially_blocked = !pass_through_popup && (is_preview || agent_driven || (is_popup && !initial_ext_cw));
        Self {
            pass_through_popup,
            is_preview,
            blocked: AtomicBool::new(initially_blocked),
            ext_clipboard_write_ids,
            generation: AtomicU64::new(0),
            binding: crate::passkeys::random_token(),
            answer: crate::passkeys::random_token(),
            sync_fn: crate::passkeys::random_token(),
            secret: crate::passkeys::random_token(),
            next_id: AtomicI32::new(0),
            state: Mutex::new(ContextState::default()),
            first_load: Mutex::new(first_load),
            root_armed: AtomicBool::new(false),
        }
    }

    /// A navigation while the first load is still held replaces it and waits for the same
    /// confirmation (`true`: the caller must not load). Otherwise any held URL is dropped and the
    /// caller loads as usual (`false`).
    pub fn replace_held_first_load(&self, url: &str) -> bool {
        let mut held = self.first_load.lock().unwrap();
        if held.is_none() {
            return false;
        }
        if self.root_armed.load(Ordering::Acquire) {
            *held = None;
            return false;
        }
        *held = Some(url.to_owned());
        true
    }

    /// The held first URL, once, after the root session's gate script is confirmed.
    pub fn take_first_load(&self) -> Option<String> {
        if !self.root_armed.load(Ordering::Acquire) {
            return None;
        }
        self.first_load.lock().unwrap().take()
    }

    pub fn ext_has_clipboard_write(&self, ext_id: &str) -> bool {
        self.ext_clipboard_write_ids.lock().unwrap().contains(ext_id)
    }

    fn build_cdp(&self, method: &str, params: Value, session_id: Option<&str>) -> (i32, String) {
        // Strictly bounded to `1_575_000_000..1_590_000_000`, disjoint from `TabPasskeyGate`
        // (`1_500M..1_550M`), `TabMediaGate` (`1_550M..1_575M`), and `SW_WAKE_ID_BASE` (`1_590M`).
        let id = CLIPBOARD_GATE_ID_BASE + self.next_id.fetch_add(1, Ordering::Relaxed).rem_euclid(15_000_000);
        let mut payload = json!({ "id": id, "method": method, "params": params });
        if let Some(sid) = session_id.filter(|s| !s.is_empty()) {
            payload["sessionId"] = Value::String(sid.to_owned());
        }
        (id, payload.to_string())
    }

    fn add_script_msg(&self, session_id: Option<&str>, blocked: bool, epoch: u64, run_immediately: bool) -> String {
        // Non-popup extension contexts start fail-closed (`false`) and receive their exact
        // per-extension verdict in `Runtime.executionContextCreated` (`#643-B4`).
        let source = clipboard_gate_script(&self.binding, &self.answer, &self.sync_fn, &self.secret, blocked, false);
        let (id, msg) = self.build_cdp(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": source, "runImmediately": run_immediately }),
            session_id,
        );
        let sid = session_id.unwrap_or("").to_owned();
        let mut st = self.state.lock().unwrap();
        st.sessions.insert(sid.clone());
        st.pending_adds.insert(id, (sid, epoch));
        msg
    }

    fn remove_script_msg(&self, session_id: Option<&str>, identifier: &str) -> String {
        self.build_cdp(
            "Page.removeScriptToEvaluateOnNewDocument",
            json!({ "identifier": identifier }),
            session_id,
        )
        .1
    }

    fn sync_context_msg_with_ext(&self, blocked: bool, ext_ok: bool, context_id: i64, session_id: Option<&str>) -> String {
        let sync_json = serde_json::to_string(&self.sync_fn).unwrap_or_else(|_| "\"\"".to_owned());
        let secret_json = serde_json::to_string(&self.secret).unwrap_or_else(|_| "\"\"".to_owned());
        self.build_cdp(
            "Runtime.evaluate",
            json!({
                "expression": format!("globalThis[{sync_json}]?.({blocked}, {ext_ok}, {secret_json})"),
                "contextId": context_id,
            }),
            session_id,
        )
        .1
    }

    fn ext_ok_for_origin(&self, origin: &str) -> bool {
        crate::extensions::manifest::extension_id_from_url(origin)
            .is_some_and(|id| self.ext_has_clipboard_write(id))
    }

    fn answer_msg(&self, ask_id: u32, allowed: bool, context_id: i64, session_id: Option<&str>) -> String {
        let answer_json = serde_json::to_string(&self.answer).unwrap_or_else(|_| "\"\"".to_owned());
        let secret_json = serde_json::to_string(&self.secret).unwrap_or_else(|_| "\"\"".to_owned());
        self.build_cdp(
            "Runtime.evaluate",
            json!({
                "expression": format!("globalThis[{answer_json}]?.({ask_id}, {allowed}, {secret_json})"),
                "contextId": context_id,
            }),
            session_id,
        )
        .1
    }

    /// Installs the hidden binding and page-side clipboard gate on root (`None`) or a paused web
    /// OOPIF session (`Some(sid)`). No-op for extension popups with declared `"clipboardWrite"`.
    pub fn attach_messages(&self, session_id: Option<&str>) -> Vec<String> {
        if self.pass_through_popup {
            return Vec::new();
        }
        let blocked = self.blocked.load(Ordering::Acquire);
        let epoch = self.generation.load(Ordering::Acquire);
        vec![
            self.binding_msg(session_id).expect("gated binding"),
            self.add_script_msg(session_id, blocked, epoch, true),
        ]
    }

    /// `Runtime.addBinding` for the wrapper's channel. Re-sent after any `Runtime.disable`.
    pub fn binding_msg(&self, session_id: Option<&str>) -> Option<String> {
        if self.pass_through_popup {
            return None;
        }
        Some(self.build_cdp("Runtime.addBinding", json!({ "name": self.binding }), session_id).1)
    }

    /// Updates the tab's blocked state (`is_preview || agent_driven`), sticky once `true` for the
    /// life of the tab (matching official `policy.ts:132-135`), returning CDP messages that
    /// immediately update every live execution context AND replace each session's
    /// `Page.addScriptToEvaluateOnNewDocument` script for future navigations and `about:blank`
    /// child iframes.
    pub fn set_blocked_messages(&self, next_blocked: bool) -> Vec<String> {
        if self.pass_through_popup {
            return Vec::new();
        }
        let prev = self.blocked.load(Ordering::Acquire);
        let effective = prev || self.is_preview || next_blocked;
        if prev == effective {
            return Vec::new();
        }
        self.blocked.store(effective, Ordering::Release);
        self.broadcast_state(effective)
    }

    /// Updates ONLY contexts whose recorded origin belongs to `ext_id` (preventing any
    /// cross-extension clobber on reload/uninstall; READER-7 `#643-B4`).
    pub fn sync_ext_contexts_for(&self, ext_id: &str, ext_allowed: bool) -> Vec<String> {
        if self.pass_through_popup {
            return Vec::new();
        }
        let blocked = self.blocked.load(Ordering::Acquire);
        let matching: Vec<(String, i64)> = self
            .state
            .lock()
            .unwrap()
            .contexts
            .iter()
            .filter(|(_, origin)| crate::extensions::manifest::extension_id_from_url(origin) == Some(ext_id))
            .map(|(k, _)| k.clone())
            .collect();
        matching
            .into_iter()
            .map(|(sid, ctx_id)| {
                let sess = (!sid.is_empty()).then_some(sid.as_str());
                self.sync_context_msg_with_ext(blocked, ext_allowed, ctx_id, sess)
            })
            .collect()
    }

    fn broadcast_state(&self, effective: bool) -> Vec<String> {
        let epoch = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let (contexts, sessions, old_scripts): (Vec<(String, i64, String)>, Vec<String>, Vec<(String, String)>) = {
            let mut st = self.state.lock().unwrap();
            let contexts = st.contexts.iter().map(|((s, id), o)| (s.clone(), *id, o.clone())).collect();
            let sessions = st.sessions.iter().cloned().collect();
            let old_scripts = st.script_ids.drain().collect();
            (contexts, sessions, old_scripts)
        };
        let mut out = Vec::with_capacity(contexts.len() + sessions.len() + old_scripts.len());
        for (sid, ctx_id, origin) in contexts {
            let sess = (!sid.is_empty()).then_some(sid.as_str());
            let ext_ok = self.ext_ok_for_origin(&origin);
            out.push(self.sync_context_msg_with_ext(effective, ext_ok, ctx_id, sess));
        }
        for (sid, ident) in old_scripts {
            let sess = (!sid.is_empty()).then_some(sid.as_str());
            out.push(self.remove_script_msg(sess, &ident));
        }
        for sid in sessions {
            let sess = (!sid.is_empty()).then_some(sid.as_str());
            out.push(self.add_script_msg(sess, effective, epoch, false));
        }
        out
    }

    /// Handles a reply to a `CLIPBOARD_GATE_ID_BASE` CDP call, storing the script identifier or
    /// returning a `Page.removeScriptToEvaluateOnNewDocument` message if the reply belongs to a
    /// superseded generation.
    pub fn on_reply(&self, msg_id: i32, ok: bool, body: Option<&Value>) -> Option<String> {
        let (sid, epoch) = self.state.lock().unwrap().pending_adds.remove(&msg_id)?;
        if !ok {
            return None;
        }
        let ident = body
            .and_then(|v| v.get("identifier"))
            .and_then(Value::as_str)?
            .to_owned();
        if epoch < self.generation.load(Ordering::Acquire) {
            let sess = (!sid.is_empty()).then_some(sid.as_str());
            return Some(self.remove_script_msg(sess, &ident));
        }
        let mut st = self.state.lock().unwrap();
        if !st.sessions.contains(&sid) {
            return None;
        }
        let replaced = st.script_ids.insert(sid.clone(), ident);
        drop(st);
        if sid.is_empty() {
            self.root_armed.store(true, Ordering::Release);
        }
        replaced.map(|old| {
            let sess = (!sid.is_empty()).then_some(sid.as_str());
            self.remove_script_msg(sess, &old)
        })
    }

    /// Observes a CDP event. Returns `(consumed, sync_msgs, ask)`:
    /// - `consumed`: true when this event was our private binding call (never forwarded to app).
    /// - `sync_msgs`: immediate `Runtime.evaluate` sync for newly created contexts after a toggle.
    /// - `ask`: pending `navigator.clipboard.write{Text}` call awaiting browser-process verdict.
    pub fn observe_event(
        &self,
        method: &str,
        params: &Value,
        session_id: Option<&str>,
    ) -> (bool, Vec<String>, Option<ClipboardAsk>) {
        if self.pass_through_popup {
            return (false, Vec::new(), None);
        }
        let sid = session_id.unwrap_or("").to_owned();
        match method {
            "Target.detachedFromTarget" => {
                if let Some(detached) = params.get("sessionId").and_then(Value::as_str) {
                    let mut st = self.state.lock().unwrap();
                    st.contexts.retain(|(s, _), _| s != detached);
                    st.sessions.remove(detached);
                    st.script_ids.remove(detached);
                    st.pending_adds.retain(|_, (s, _)| s != detached);
                }
                (false, Vec::new(), None)
            }
            "Runtime.executionContextCreated" => {
                let ctx = &params["context"];
                let mut sync = Vec::new();
                if ctx["auxData"]["isDefault"].as_bool() == Some(true)
                    && let Some(id) = ctx["id"].as_i64()
                {
                    let origin = ctx["origin"].as_str().unwrap_or("").to_owned();
                    let is_ext_ctx = crate::extensions::manifest::is_extension_scheme(&origin);
                    let ext_ok = is_ext_ctx && self.ext_ok_for_origin(&origin);
                    self.state.lock().unwrap().contexts.insert((sid.clone(), id), origin);
                    // Always push per-extension `ext_ok` on `chrome-extension://<id>` contexts
                    // (covers web tabs navigated to extension pages; `#643-B4`), and also push
                    // on any context created after a `blocked` generation swap.
                    if is_ext_ctx || self.generation.load(Ordering::Acquire) > 0 {
                        let sess = session_id.filter(|s| !s.is_empty());
                        sync.push(self.sync_context_msg_with_ext(self.blocked.load(Ordering::Acquire), ext_ok, id, sess));
                    }
                }
                (false, sync, None)
            }
            "Runtime.executionContextDestroyed" => {
                if let Some(id) = params["executionContextId"].as_i64() {
                    self.state.lock().unwrap().contexts.remove(&(sid, id));
                }
                (false, Vec::new(), None)
            }
            "Runtime.executionContextsCleared" => {
                self.state.lock().unwrap().contexts.retain(|(s, _), _| s != &sid);
                (false, Vec::new(), None)
            }
            "Runtime.bindingCalled" => {
                if params["name"].as_str() != Some(self.binding.as_str()) {
                    return (false, Vec::new(), None);
                }
                let payload = params["payload"].as_str().unwrap_or("");
                let (Some(ctx_id), Ok(ask)) = (params["executionContextId"].as_i64(), serde_json::from_str::<Ask>(payload)) else {
                    return (true, Vec::new(), None);
                };
                if payload.len() > 128 || ask.id == 0 {
                    return (true, Vec::new(), None);
                }
                let origin = self
                    .state
                    .lock()
                    .unwrap()
                    .contexts
                    .get(&(sid, ctx_id))
                    .cloned()
                    .filter(|o| !o.is_empty() && o != "null");
                let session = session_id.filter(|s| !s.is_empty());
                (
                    true,
                    Vec::new(),
                    Some(ClipboardAsk {
                        origin,
                        allow_msg: self.answer_msg(ask.id, true, ctx_id, session),
                        deny_msg: self.answer_msg(ask.id, false, ctx_id, session),
                    }),
                )
            }
            _ => (false, Vec::new(), None),
        }
    }
}
