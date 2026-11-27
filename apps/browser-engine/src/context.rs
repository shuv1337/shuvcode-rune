//! Shared partitioned `CefRequestContext` pool (`apps/browser-engine/src/context.rs`) — the
//! **sole** caller of `cef::request_context_create_context` across WS-1, PREVIEWS (`#281`), WS-9
//! (`#274`), and BROWSER-TOOLS.
//!
//! ## Scope Partitioning & Canonical Keys
//! - `ContextKind::Default` -> Global persistent profile (`OPENCODE_BROWSER_PROFILE/Default`,
//!   `persist_session_cookies: 1`); tracked in `default_tabs` (ready immediately).
//! - `ContextKind::Preview { workspace, grant }` -> `ContextKey::Preview { workspace, grant }`
//!   (trimmed). Distinct per workspace and grant.
//! - `ContextKind::Artifact { artifact_id }` -> `ContextKey::Artifact(artifact_id)` (trimmed).
//!   Separate enum variant and key namespace from `Preview`.
//! - `ContextKind::Private` -> `ContextKey::Private(tab_id)`. Unique per private tab.
//! - `ContextKind::RemoteServer { server_id, proxy }` -> `ContextKey::RemoteServer(server_id)`
//!   (trimmed canonical `ServerKey::as_str()` identity, never display name). **ALWAYS requires a
//!   live authenticated loopback IP-literal tunnel proxy** (`browser.proxy.required` if absent,
//!   `browser.proxy.invalid` if non-IP / non-loopback / missing credentials).
//!
//! ## Fail-Closed Readiness Gate (`#281`), Readback Verification & Transient vs Fatal Lifecycle (`#292`)
//! - **Single fail-closed readiness gate (`is_tab_ready(tab_id)`):**
//!   - Returns `false` for unknown, closed, uninitialized, failed-preference, or proxy-suspended tabs.
//!   - Returns `true` for live `Default` tabs and live isolated tabs ONLY when
//!     `initialized && prefs_ok && !suspended` (for `RemoteServer`, that guarantees the loopback
//!     proxy is live and verified via `ctx.preference(...)` read-back).
//!   - While `!ready`, `PreInitBlockResourceHandler` (context-wide), `TabRequestHandler`,
//!     `TabResourceRequestHandler`, and `TabLifeSpanHandler` cancel every non-`about:blank`
//!     request/navigation/popup, and `EngineManager` rejects `CdpCall`, `ContextMenuAction`,
//!     `ClearSite*`, `SiteCookies`, `SetInspect`, `HighlightNode`, `Emulate`, `Back`, `Forward`,
//!     and `Reload`.
//! - **Preference read-back verification (`apply_context_prefs`):** After `set_preference`, every
//!   required preference (`proxy` dictionary `mode`/`server`/`bypass_list`,
//!   `webrtc.ip_handling_policy`, `net.network_prediction_options`, `net.quic_allowed`) is read
//!   back via `ctx.preference(...)` and asserted to equal the written value (fail closed on any
//!   mismatch; zero `mode: "direct"` fallback for `RemoteServer`).
//! - **Transient tunnel loss vs fatal/revocation teardown:**
//!   - `SetPartitionProxy(None)` (transient tunnel drop/sleep/reconnect): suspends the live context
//!     (`suspended = true`, `ready = false`), closes connections via `NoopCompletion`, and emits
//!     `Refused("browser.proxy.required")` so zero bytes escape while preserving open user tabs
//!     and unsaved JS state until the tunnel reconnects (`SetPartitionProxy(Some(new_cfg))`).
//!   - Fatal preference failure (`ProxyFatal`) or explicit revocation (`DropContext`): refuses and
//!     closes affected live tabs and tears down the context.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use cef::rc::Rc as _;
use cef::{
    Browser, Callback, CefString, CompletionCallback, Frame, ImplCompletionCallback,
    ImplCookieManager, ImplDictionaryValue, ImplListValue, ImplPreferenceManager, ImplRequest,
    ImplRequestContext, ImplRequestContextHandler, ImplResourceRequestHandler, ImplValue, Request,
    RequestContext, RequestContextHandler, RequestContextSettings, ResourceRequestHandler,
    Response, ReturnValue, WrapCompletionCallback, WrapRequestContextHandler,
    WrapResourceRequestHandler, sys, wrap_completion_callback, wrap_request_context_handler,
    wrap_resource_request_handler,
};

use crate::wire::{ContextKind, RemoteProxyConfig};

const DEFAULT_PROXY_REALM: &str = "OpenCode Browser Proxy";

// CEF 156.0.26 dereferences a null `CefCompletionCallback*` (`0xC0000005` at `libcef.dll+0x2e7810`)
// when `NetworkContext::CloseAllConnections` completes on a live `CefBrowserContext`.
wrap_completion_callback! {
    struct NoopCompletion;
    impl CompletionCallback {
        fn on_complete(&self) {}
    }
}

fn close_connections(ctx: &RequestContext) {
    let mut cb = NoopCompletion::new();
    ctx.close_all_connections(Some(&mut cb));
}

/// Canonical pool key guaranteeing every isolation scope occupies its own disjoint namespace.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ContextKey {
    Preview { workspace: String, grant: String },
    Artifact(String),
    Private(u64),
    RemoteServer(String),
}

/// Optional context-wide `CefResourceRequestHandler` callback (used by PREVIEWS `#281` to confine
/// Web Workers, `sendBeacon`, WebSockets, and subresources on `oc-preview://` contexts).
pub type ResourceHook = Arc<
    dyn Fn(
            Option<&mut Browser>,
            Option<&mut Frame>,
            Option<&mut Request>,
            i32,
            i32,
            Option<&CefString>,
            Option<&mut i32>,
        ) -> Option<ResourceRequestHandler>
        + Send
        + Sync,
>;

/// Per-creation hooks allowing callers such as PREVIEWS (`#281`) to register custom scheme
/// factories and context-wide resource filters without duplicating `request_context_create_context`.
#[derive(Default)]
pub struct ContextHooks {
    pub resource_filter: Option<ResourceHook>,
    pub exclude_default_cookie_schemes: bool,
    /// Retained for compatibility with `#281` callers; all isolated contexts enforce verified
    /// lockdown preferences unconditionally.
    #[allow(dead_code)]
    pub require_lockdown_prefs: bool,
    /// Invoked inside `on_request_context_initialized` ONLY AFTER `apply_context_prefs` succeeds
    /// (e.g. registering `PreviewSchemeHandlerFactory` only once lockdown prefs are verified).
    pub on_initialized: Option<Box<dyn FnOnce(&RequestContext) + Send>>,
}

/// Validates that a `RemoteProxyConfig` names a strict loopback IP literal (`127.0.0.1` / `::1`,
/// never a hostname like `localhost`), non-zero port, and non-empty Basic credentials.
pub fn validate_proxy(cfg: &RemoteProxyConfig) -> Result<(), &'static str> {
    browser_policy::validate_proxy_endpoint(
        &cfg.host,
        cfg.port,
        cfg.username.as_deref(),
        cfg.password.as_deref(),
    )
}

/// Formats `cfg`'s loopback IP and port into a valid Chromium `fixed_servers` URL (bracketing IPv6).
fn format_proxy_server(cfg: &RemoteProxyConfig) -> String {
    let host = cfg.host.trim();
    let bracketed = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    format!("http://{bracketed}:{}", cfg.port)
}

/// Checks whether a CEF `auth_credentials(is_proxy == 1)` challenge matches `cfg` (`network.ts:57-66`).
pub fn proxy_matches_challenge(
    cfg: &RemoteProxyConfig,
    scheme: &str,
    host: &str,
    port: i32,
    realm: &str,
) -> bool {
    let want_realm = cfg.realm.as_deref().unwrap_or(DEFAULT_PROXY_REALM);
    let cfg_host = cfg.host.trim().trim_start_matches('[').trim_end_matches(']');
    let chal_host = host.trim().trim_start_matches('[').trim_end_matches(']');
    scheme.eq_ignore_ascii_case("basic")
        && chal_host.eq_ignore_ascii_case(cfg_host)
        && port == i32::from(cfg.port)
        && realm == want_realm
}

/// Applies full privacy/network lockdown (and mandatory `fixed_servers` + `<-loopback>` proxy for
/// `RemoteServer` contexts) AND reads every required preference back via `ctx.preference(...)` to
/// verify the written values actually took effect. Never falls back to `mode: "direct"`.
/// Disables non-proxied UDP WebRTC ICE host gathering on `ctx` (used on the global `Default`
/// context alongside `--disable-features=MediaRouter,DialMediaRouteProvider,CastMediaRouteProvider,WebRtcHideLocalIpsWithMdns`
/// so `NetworkService` opens zero non-loopback UDP listeners and exposes zero local LAN IPs).
pub fn disable_non_proxied_udp(ctx: &RequestContext) {
    let _ = set_and_verify_string_pref(ctx, "webrtc.ip_handling_policy", "disable_non_proxied_udp");
    let _ = set_optional_bool_pref(ctx, "webrtc.multiple_routes_enabled", false);
    let _ = set_optional_bool_pref(ctx, "webrtc.nonproxied_udp_enabled", false);
}

fn apply_context_prefs(
    ctx: &RequestContext,
    proxy: Option<&RemoteProxyConfig>,
    require_proxy: bool,
) -> bool {
    let mut ok = true;
    match proxy {
        Some(cfg) => {
            if !set_and_verify_proxy_pref(ctx, cfg) {
                ok = false;
            }
        }
        None if require_proxy => {
            return false;
        }
        None => {}
    }
    let webrtc_ok = set_and_verify_string_pref(ctx, "webrtc.ip_handling_policy", "disable_non_proxied_udp");
    // 2 == NETWORK_PREDICTION_NEVER in Chromium's PrefService (supersedes `dns_prefetching.enabled`).
    let pred_ok = set_and_verify_int_pref(ctx, "net.network_prediction_options", 2);
    let quic_ok = set_and_verify_bool_pref(ctx, "net.quic_allowed", false);
    if std::env::var_os("OPENCODE_BROWSER_ENGINE_TRACE").is_some() {
        eprintln!(
            "context_prefs: proxy_ok={ok} webrtc.ip_handling_policy={webrtc_ok} net.network_prediction_options={pred_ok} net.quic_allowed={quic_ok}"
        );
    }
    ok &= webrtc_ok && pred_ok && quic_ok;
    // Optional legacy/global prefs: enforced + verified if registered on a CEF build.
    ok &= set_optional_bool_pref(ctx, "webrtc.multiple_routes_enabled", false);
    ok &= set_optional_bool_pref(ctx, "webrtc.nonproxied_udp_enabled", false);
    ok &= set_optional_string_pref(ctx, "dns_over_https.mode", "off");
    ok &= set_optional_bool_pref(ctx, "dns_prefetching.enabled", false);
    // Lock all 7 hardware-device and File System Access guards to `BLOCK` (2) on isolated contexts.
    for pref in crate::permissions::DEVICE_GUARD_PREFS {
        ok &= set_optional_int_pref(ctx, pref, 2);
    }
    ok &= lock_chrome_services(ctx);
    ok
}

fn set_and_verify_proxy_pref(ctx: &RequestContext, cfg: &RemoteProxyConfig) -> bool {
    let key = CefString::from("proxy");
    if ctx.has_preference(Some(&key)) == 0 || ctx.can_set_preference(Some(&key)) == 0 {
        return false;
    }
    let Some(mut dict) = cef::dictionary_value_create() else { return false };
    let server = format_proxy_server(cfg);
    dict.set_string(Some(&CefString::from("mode")), Some(&CefString::from("fixed_servers")));
    dict.set_string(Some(&CefString::from("server")), Some(&CefString::from(server.as_str())));
    dict.set_string(Some(&CefString::from("bypass_list")), Some(&CefString::from("<-loopback>")));
    let Some(mut val) = cef::value_create() else { return false };
    val.set_dictionary(Some(&mut dict));
    // `CefString::default()` maps to NULL in `cef-rs`, which CEF's `SetPreference` rejects.
    let mut err = CefString::from(" ");
    if ctx.set_preference(Some(&key), Some(&mut val), Some(&mut err)) == 0 {
        eprintln!("context: set_preference(proxy) failed: {err}");
        return false;
    }
    let Some(read_val) = ctx.preference(Some(&key)) else { return false };
    let Some(read_dict) = read_val.dictionary() else { return false };
    let got_mode = CefString::from(&read_dict.string(Some(&CefString::from("mode")))).to_string();
    let got_server = CefString::from(&read_dict.string(Some(&CefString::from("server")))).to_string();
    let got_bypass = CefString::from(&read_dict.string(Some(&CefString::from("bypass_list")))).to_string();
    got_mode == "fixed_servers" && got_server == server && got_bypass == "<-loopback>"
}

fn set_and_verify_string_pref(ctx: &RequestContext, name: &str, value: &str) -> bool {
    set_and_verify_pref(
        ctx,
        name,
        |v| {
            v.set_string(Some(&CefString::from(value)));
        },
        |v| CefString::from(&v.string()).to_string() == value,
    )
}

fn set_and_verify_bool_pref(ctx: &RequestContext, name: &str, value: bool) -> bool {
    set_and_verify_pref(
        ctx,
        name,
        |v| {
            v.set_bool(i32::from(value));
        },
        |v| (v.bool() != 0) == value,
    )
}

fn set_and_verify_int_pref(ctx: &RequestContext, name: &str, value: i32) -> bool {
    set_and_verify_pref(
        ctx,
        name,
        |v| {
            v.set_int(value);
        },
        |v| v.int() == value,
    )
}

/// Sets `name` and reads it back. A pref that cannot be set (a company policy, or one Chromium
/// keeps read-only on an off-the-record context) passes only if it already holds the value: an
/// admin who turned the service off must not lose the pane (READER-7 #624-B1). Every refusal names
/// the pref in the engine log.
fn set_and_verify_pref(
    ctx: &RequestContext,
    name: &str,
    write: impl Fn(&mut cef::Value),
    holds: impl Fn(&cef::Value) -> bool,
) -> bool {
    let key = CefString::from(name);
    let current = || ctx.preference(Some(&key)).is_some_and(|v| holds(&v));
    if ctx.has_preference(Some(&key)) == 0 {
        return refuse_pref(name, "is not registered");
    }
    if !pref_settable(ctx, &key, name) {
        return current() || refuse_pref(name, "is read-only at another value");
    }
    let Some(mut val) = cef::value_create() else { return refuse_pref(name, "has no value to write") };
    write(&mut val);
    let mut err = CefString::from(" ");
    if ctx.set_preference(Some(&key), Some(&mut val), Some(&mut err)) == 0 {
        return refuse_pref(name, &format!("could not be set ({err})"));
    }
    current() || refuse_pref(name, "did not keep the value")
}

fn pref_settable(ctx: &RequestContext, key: &CefString, _name: &str) -> bool {
    // Debug builds only: stands in for a policy-locked pref in the policy proof.
    #[cfg(debug_assertions)]
    if std::env::var("OPENCODE_BROWSER_DEBUG_READONLY_PREFS").is_ok_and(|v| v.split(',').any(|n| n == name)) {
        return false;
    }
    ctx.can_set_preference(Some(key)) != 0
}

fn refuse_pref(name: &str, why: &str) -> bool {
    eprintln!("engine-prefs: pref {name} {why}; refusing the request context");
    false
}
fn set_optional_string_pref(ctx: &RequestContext, name: &str, value: &str) -> bool {
    let key = CefString::from(name);
    if ctx.has_preference(Some(&key)) == 0 {
        return true;
    }
    set_and_verify_string_pref(ctx, name, value)
}

fn set_optional_bool_pref(ctx: &RequestContext, name: &str, value: bool) -> bool {
    let key = CefString::from(name);
    if ctx.has_preference(Some(&key)) == 0 {
        return true;
    }
    set_and_verify_bool_pref(ctx, name, value)
}

fn set_optional_int_pref(ctx: &RequestContext, name: &str, value: i32) -> bool {
    let key = CefString::from(name);
    // Absent from this build: nothing to set. Present, not settable and not already at the
    // value: fails (READER-7 N9, #624-B1).
    if ctx.has_preference(Some(&key)) == 0 {
        return true;
    }
    set_and_verify_int_pref(ctx, name, value)
}

/// Turns off, on `ctx`, the Chrome services official's Electron does not contain (READER-7's
/// non-extension audit, `notes/review-rd274.md`), as prefs and content-setting locks; the
/// switches and feature flags are in `cef_runtime.rs`. Applied to `Default` and every isolated
/// context. A pref this build registers but will not take, and does not already hold, fails the
/// whole call, so the caller refuses the context instead of running with the service on (N9).
pub fn lock_chrome_services(ctx: &RequestContext) -> bool {
    const OFF: [&str; 23] = [
        // N1 Safe Browsing.
        "safebrowsing.enabled",
        "safebrowsing.enhanced",
        "safebrowsing.scout_reporting_enabled",
        // N3 sign-in, password manager, local autofill.
        "signin.allowed",
        "credentials_enable_service",
        "credentials_enable_autosignin",
        // The leak check sends a hash of each submitted sign-in to `passwordsleakcheck-pa.googleapis.com`;
        // in Chromium 156 only this pref turns it off (`PasswordLeakDetection` is no longer a feature).
        "profile.password_manager_leak_detection",
        "autofill.profile_enabled",
        "autofill.credit_card_enabled",
        "autofill.enabled",
        "signin.allowed_on_next_startup",
        // N7 payments.
        "payments.can_make_payment_enabled",
        // N6a Privacy Sandbox.
        "privacy_sandbox.m1.topics_enabled",
        "privacy_sandbox.m1.fledge_enabled",
        "privacy_sandbox.m1.ad_measurement_enabled",
        "privacy_sandbox.apis_enabled",
        // N11 Translate and the spelling service (spellcheck stays on, with local dictionaries).
        "translate.enabled",
        "spellcheck.use_spelling_service",
        // N14 the rest of Google's per-page lookups.
        "alternate_error_pages.enabled",
        "search.suggest_enabled",
        "url_keyed_anonymized_data_collection.enabled",
        "domain_reliability.allowed_by_policy",
        "dns_prefetching.enabled",
    ];
    let mut ok = true;
    for name in OFF {
        ok &= set_optional_bool_pref(ctx, name, false);
    }
    // PDFs show inline, top-level and framed, as in official's Electron 44 (its built-in viewer,
    // `navigator.pdfViewerEnabled` true). Set back explicitly: profiles from revisions 7-16 stored
    // `true`, which sent every PDF to the Save dialog.
    ok &= set_optional_bool_pref(ctx, "plugins.always_open_pdf_externally", false);
    // 2 == NETWORK_PREDICTION_NEVER.
    ok &= set_optional_int_pref(ctx, "net.network_prediction_options", 2);
    // The content settings, as their default-scope prefs (`Default` also gets them directly).
    for pref in crate::permissions::chrome_service_setting_prefs() {
        ok &= set_optional_int_pref(ctx, &pref, 2);
    }
    ok
}

/// Outcome produced once a deferred context finishes `on_request_context_initialized`.
pub enum DeferredOutcome {
    Load { tab_id: u64, generation: u64, url: String },
    Refuse { tab_id: u64, generation: u64, reason: &'static str },
}

impl DeferredOutcome {
    pub fn tab_id(&self) -> u64 {
        match self {
            Self::Load { tab_id, .. } | Self::Refuse { tab_id, .. } => *tab_id,
        }
    }
}

/// Result of updating a remote server's loopback proxy via `set_partition_proxy`.
pub enum ProxyUpdate {
    /// New proxy verified and applied (and old connections closed); caller MUST sync
    /// `t.prompts.proxy` and `t.context` on live tabs BEFORE calling `activate_ready.store(true)`.
    Applied {
        config: RemoteProxyConfig,
        activate_ready: Option<Arc<AtomicBool>>,
    },
    /// Transient proxy loss (`proxy: None`): live tabs are suspended (`ready = false`, network
    /// blocked fail-closed) without destroying open pages or unsaved user state.
    Suspended(Vec<u64>),
}

/// Failure from `set_partition_proxy` or `resolve_with_hooks` proxy update; `close_tabs` lists
/// any live tabs left in a broken context that MUST be refused & closed immediately.
pub struct ProxyFatal {
    pub reason: &'static str,
    pub close_tabs: Vec<u64>,
}

struct ContextEntry {
    ctx: RequestContext,
    tabs: HashSet<u64>,
    require_proxy: bool,
    initialized: Arc<AtomicBool>,
    prefs_ok: Arc<AtomicBool>,
    suspended: Arc<AtomicBool>,
    /// True ONLY when `initialized && prefs_ok && !suspended` (and `on_initialized` has run).
    /// For `RemoteServer`, `require_proxy` guarantees a verified live proxy is active whenever `ready`.
    ready: Arc<AtomicBool>,
    proxy: Arc<Mutex<Option<RemoteProxyConfig>>>,
    /// `(tab_id, generation, url)` loads waiting for `on_request_context_initialized`.
    deferred_loads: Arc<Mutex<Vec<(u64, u64, String)>>>,
}

impl ContextEntry {
    fn teardown(&self) {
        self.suspended.store(true, Ordering::Release);
        self.ready.store(false, Ordering::Release);
        self.deferred_loads.lock().unwrap().clear();
        self.ctx.clear_scheme_handler_factories();
        close_connections(&self.ctx);
        if let Some(mgr) = self.ctx.cookie_manager(None) {
            let _ = mgr.delete_cookies(None, None, None);
        }
    }
}

// Fail-closed pre-initialization & suspended resource filter attached by `IsolatedContextHandler`
// while `!ready` so NO non-`about:blank` subresource, worker, beacon, or redirect can leave an
// isolated context before `on_request_context_initialized` verifies preferences or while suspended.
wrap_resource_request_handler! {
    struct PreInitBlockResourceHandler;

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            let url = request.map(|r| CefString::from(&r.url()).to_string()).unwrap_or_default();
            if url.trim().eq_ignore_ascii_case("about:blank") {
                ReturnValue::CONTINUE
            } else {
                ReturnValue::CANCEL
            }
        }

        fn on_resource_redirect(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _response: Option<&mut Response>,
            new_url: Option<&mut CefString>,
        ) {
            if let Some(target) = new_url {
                *target = CefString::from("");
            }
        }

        fn on_protocol_execution(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            allow_os_execution: Option<&mut ::std::os::raw::c_int>,
        ) {
            if let Some(allow) = allow_os_execution {
                *allow = 0;
            }
        }
    }
}

wrap_request_context_handler! {
    struct IsolatedContextHandler {
        require_proxy: bool,
        initialized: Arc<AtomicBool>,
        prefs_ok: Arc<AtomicBool>,
        suspended: Arc<AtomicBool>,
        ready: Arc<AtomicBool>,
        proxy: Arc<Mutex<Option<RemoteProxyConfig>>>,
        ready_loads: Arc<Mutex<Vec<DeferredOutcome>>>,
        deferred_loads: Arc<Mutex<Vec<(u64, u64, String)>>>,
        on_initialized: Arc<Mutex<Option<Box<dyn FnOnce(&RequestContext) + Send>>>>,
        resource_filter: Option<ResourceHook>,
        pump: Sender<crate::Event>,
    }

    impl RequestContextHandler {
        fn on_request_context_initialized(&self, request_context: Option<&mut RequestContext>) {
            let proxy = self.proxy.lock().unwrap().clone();
            // Preview and artifact contexts (the ones with a resource filter) never print: nobody
            // need be watching them, and a native print dialog stops the whole engine (#662).
            let applied = request_context.as_deref().is_some_and(|ctx| {
                apply_context_prefs(ctx, proxy.as_ref(), self.require_proxy)
                    && (self.resource_filter.is_none() || set_and_verify_bool_pref(ctx, "printing.enabled", false))
            });
            if applied
                && let Some(ctx) = request_context.as_deref()
            {
                crate::preview::handler::ban_file_scheme(ctx);
                if let Some(hook) = self.on_initialized.lock().unwrap().take() {
                    hook(ctx);
                }
            }
            let is_suspended = self.suspended.load(Ordering::Acquire);
            self.prefs_ok.store(applied, Ordering::Release);
            self.ready.store(applied && !is_suspended, Ordering::Release);
            self.initialized.store(true, Ordering::Release);
            let pending = std::mem::take(&mut *self.deferred_loads.lock().unwrap());
            if !pending.is_empty() {
                let mut out = self.ready_loads.lock().unwrap();
                for (tab_id, generation, url) in pending {
                    if applied {
                        if !is_suspended {
                            out.push(DeferredOutcome::Load { tab_id, generation, url });
                        }
                    } else {
                        let reason = if self.require_proxy {
                            "browser.proxy.invalid"
                        } else {
                            "browser.context.unavailable"
                        };
                        out.push(DeferredOutcome::Refuse { tab_id, generation, reason });
                    }
                }
                let _ = self.pump.send(crate::Event::Pump(0));
            }
        }

        fn resource_request_handler(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            is_navigation: ::std::os::raw::c_int,
            is_download: ::std::os::raw::c_int,
            request_initiator: Option<&CefString>,
            disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            // Cancel every non-`about:blank` request before `on_request_context_initialized`
            // finishes or while a remote context is suspended (`!ready`).
            if !self.ready.load(Ordering::Acquire) {
                return Some(PreInitBlockResourceHandler::new());
            }
            self.resource_filter.as_ref().and_then(|hook| {
                hook(
                    browser,
                    frame,
                    request,
                    is_navigation,
                    is_download,
                    request_initiator,
                    disable_default_handling,
                )
            })
        }
    }
}

fn is_blocked_file_url(url: &str) -> bool {
    let mut cur = url.trim();
    loop {
        if cur.len() >= 5 && cur.as_bytes()[..5].eq_ignore_ascii_case(b"blob:") {
            cur = cur[5..].trim();
        } else if cur.len() >= 11 && cur.as_bytes()[..11].eq_ignore_ascii_case(b"filesystem:") {
            cur = cur[11..].trim();
        } else {
            break;
        }
    }
    browser_policy::is_file_scheme(cur)
}

wrap_resource_request_handler! {
    // Blocks all `file://` (and `blob:file://`) requests and redirects initiated by extension
    // service workers, offscreen documents, or extension pages (`review-rd274.md` §2).
    struct ExtensionFileBlockHandler;

    impl ResourceRequestHandler {
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            let url = request.map(|r| CefString::from(&r.url()).to_string()).unwrap_or_default();
            if is_blocked_file_url(&url) {
                ReturnValue::CANCEL
            } else {
                ReturnValue::CONTINUE
            }
        }

        fn on_resource_redirect(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _response: Option<&mut Response>,
            new_url: Option<&mut CefString>,
        ) {
            if let Some(new_url) = new_url
                && is_blocked_file_url(&new_url.to_string())
            {
                *new_url = CefString::from("");
            }
        }

        fn on_protocol_execution(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            allow_os_execution: Option<&mut ::std::os::raw::c_int>,
        ) {
            if let Some(allow) = allow_os_execution {
                *allow = 0;
            }
        }
    }
}

/// Returns the normalized absolute path matching Chromium's `base::MakeAbsoluteFilePath`
/// (`\\?\` stripped and uppercase drive letter on Windows, canonical UTF-8 on Unix) so both
/// `Extensions.loadUnpacked` and `generate_id_for_path` see the exact same path.
pub fn canonical_extension_load_path(dir: &std::path::Path) -> Option<String> {
    let canon = std::fs::canonicalize(dir).ok()?;
    #[cfg(windows)]
    {
        let mut s = canon.to_string_lossy().into_owned();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            s = format!(r"\\{rest}");
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            s = rest.to_owned();
        }
        while s.len() > 3 && s.ends_with('\\') {
            s.pop();
        }
        let bytes = s.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_lowercase() {
            let mut chars: Vec<char> = s.chars().collect();
            chars[0] = chars[0].to_ascii_uppercase();
            s = chars.into_iter().collect();
        }
        Some(s)
    }
    #[cfg(not(windows))]
    {
        let mut s = canon.to_str()?.to_owned();
        while s.len() > 1 && s.ends_with('/') {
            s.pop();
        }
        Some(s)
    }
}

/// Computes Chromium's `crx_file::id_util::GenerateIdForPath(path)` directly from a normalized
/// path string produced by [`canonical_extension_load_path`] (SHA-256 of UTF-16LE `wchar_t` code
/// units on Windows or UTF-8 bytes on POSIX, first 16 bytes nibble-encoded as `'a'..='p'`).
pub fn generate_id_for_normalized_path(norm: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let bytes: Vec<u8> = {
        #[cfg(windows)]
        {
            let mut out = Vec::with_capacity(norm.len() * 2);
            for u in norm.encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
            out
        }
        #[cfg(not(windows))]
        {
            norm.as_bytes().to_vec()
        }
    };
    let digest = Sha256::digest(&bytes);
    let mut id = String::with_capacity(32);
    for &b in &digest[..16] {
        id.push((b'a' + (b >> 4)) as char);
        id.push((b'a' + (b & 0x0f)) as char);
    }
    id
}

/// Disables `file://` scheme access in Chromium's `ExtensionPrefs`
/// (`extensions.settings.<id>.newAllowFileAccess = false`) under BOTH the path-derived ID
/// (`GenerateIdForPath` computed directly from the single `normalized_path` string passed to
/// `Extensions.loadUnpacked`, eliminating double-canonicalization races) AND the key-derived
/// `ext_id`. Returns `true` only when the preference write succeeds (`review-rd274.md` Round-3 §1).
pub fn disallow_extension_file_access(ext_id: &str, normalized_path: &str) -> bool {
    if normalized_path.is_empty() {
        return false;
    }
    let path_id = generate_id_for_normalized_path(normalized_path);
    let Some(ctx) = cef::request_context_get_global_context() else {
        return false;
    };
    let key = CefString::from("extensions.settings");
    if ctx.has_preference(Some(&key)) == 0 || ctx.can_set_preference(Some(&key)) == 0 {
        return false;
    }
    let existing = ctx.preference(Some(&key));
    let mut root_dict = existing
        .as_ref()
        .and_then(|v| v.dictionary())
        .and_then(|d| d.copy(0))
        .or_else(cef::dictionary_value_create);
    let Some(mut root_dict) = root_dict.take() else {
        return false;
    };
    let mut target_ids = vec![path_id];
    if !target_ids.iter().any(|id| id == ext_id) {
        target_ids.push(ext_id.to_owned());
    }
    for tid in &target_ids {
        let ext_key = CefString::from(tid.as_str());
        let mut ext_dict = root_dict
            .dictionary(Some(&ext_key))
            .and_then(|d| d.copy(0))
            .or_else(cef::dictionary_value_create);
        let Some(mut ext_dict) = ext_dict.take() else {
            return false;
        };
        ext_dict.set_bool(Some(&CefString::from("newAllowFileAccess")), 0);
        root_dict.set_dictionary(Some(&ext_key), Some(&mut ext_dict));
    }
    let Some(mut val) = cef::value_create() else {
        return false;
    };
    val.set_dictionary(Some(&mut root_dict));
    let mut err = CefString::from(" ");
    ctx.set_preference(Some(&key), Some(&mut val), Some(&mut err)) != 0
}

/// Defense-in-depth browser-process lockdown of Chromium native messaging preferences on the
/// global `CefRequestContext` (`native_messaging.blocklist = ["*"]`, empty allowlist, user-level
/// hosts disabled). Runs alongside C++ `PermissionsData` stripping of `nativeMessaging`.
pub fn lockdown_global_native_messaging() {
    let Some(ctx) = cef::request_context_get_global_context() else {
        return;
    };
    let set_list = |name: &str, items: &[&str]| {
        let key = CefString::from(name);
        if ctx.has_preference(Some(&key)) == 0 || ctx.can_set_preference(Some(&key)) == 0 {
            return;
        }
        let Some(mut list) = cef::list_value_create() else { return };
        for (i, item) in items.iter().enumerate() {
            list.set_string(i, Some(&CefString::from(*item)));
        }
        let Some(mut val) = cef::value_create() else { return };
        val.set_list(Some(&mut list));
        let mut err = CefString::from(" ");
        let _ = ctx.set_preference(Some(&key), Some(&mut val), Some(&mut err));
    };
    set_list("native_messaging.blocklist", &["*"]);
    set_list("native_messaging.allowlist", &[]);
    let _ = set_optional_bool_pref(&ctx, "native_messaging.user_level_hosts", false);
}

/// Post-load fail-closed verification (`review-rd274.md` Round-3 §1a + deny-by-default lockdown):
/// reads back Chromium's persisted `extensions.settings.<loaded_id>` after `Extensions.loadUnpacked`
/// succeeds and requires ALL THREE:
/// 1. `creation_flags` is present AND bit `1 << 2` (`0x4`, `Extension::ALLOW_FILE_ACCESS`) is `0`,
/// 2. `newAllowFileAccess` is present AND `false`,
/// 3. DENY BY DEFAULT: every entry in BOTH `active_permissions.api` and `granted_permissions.api`
///    is strictly on [`crate::extensions::manifest::ALLOWED_CHROMIUM_PERMS`].
///
/// Catches any path-normalization drift or manifest-stripping failure immediately so the caller
/// uninstalls the extension before a single line of SW JS runs.
pub fn verify_extension_file_access_disabled(loaded_id: &str) -> bool {
    const EXTENSION_ALLOW_FILE_ACCESS: i32 = 1 << 2;
    let Some(ctx) = cef::request_context_get_global_context() else {
        return false;
    };
    let key = CefString::from("extensions.settings");
    let Some(val) = ctx.preference(Some(&key)) else {
        return false;
    };
    let Some(root_dict) = val.dictionary() else {
        return false;
    };
    let Some(ext_dict) = root_dict.dictionary(Some(&CefString::from(loaded_id))) else {
        return false;
    };
    let flags_key = CefString::from("creation_flags");
    if ext_dict.has_key(Some(&flags_key)) == 0
        || *ext_dict.get_type(Some(&flags_key)).as_ref() != sys::cef_value_type_t::VTYPE_INT
    {
        return false;
    }
    let creation_flags = ext_dict.int(Some(&flags_key));
    if (creation_flags & EXTENSION_ALLOW_FILE_ACCESS) != 0 {
        return false;
    }
    let allow_key = CefString::from("newAllowFileAccess");
    if ext_dict.has_key(Some(&allow_key)) == 0
        || *ext_dict.get_type(Some(&allow_key)).as_ref() != sys::cef_value_type_t::VTYPE_BOOL
    {
        return false;
    }
    if ext_dict.bool(Some(&allow_key)) != 0 {
        return false;
    }
    // Fail closed (deny-by-default) if Chromium's own C++ ExtensionPrefs granted ANY native API
    // permission outside our exact `ALLOWED_CHROMIUM_PERMS` allow-list OR any `manifest_permissions`
    // (READER-7 C-S1: e.g. `chrome_settings_overrides`, `automation`, `sockets`).
    for perm_bucket in ["active_permissions", "granted_permissions"] {
        let Some(bucket) = ext_dict.dictionary(Some(&CefString::from(perm_bucket))) else {
            continue;
        };
        if let Some(api_list) = bucket.list(Some(&CefString::from("api"))) {
            for i in 0..api_list.size() {
                let api_name = CefString::from(&api_list.string(i)).to_string();
                if !crate::extensions::manifest::is_allowed_chromium_perm(&api_name) {
                    return false;
                }
            }
        }
        if let Some(mf_list) = bucket.list(Some(&CefString::from("manifest_permissions")))
            && mf_list.size() > 0
        {
            return false;
        }
    }
    true
}

wrap_request_context_handler! {
    // Attached to CEF's default/global `CefRequestContext` via
    // `BrowserProcessHandler::default_request_context_handler`: cancels every `file://` fetch
    // and redirect from extension service workers, offscreen documents, and extension-origin
    // initiators (`review-rd274.md` §2).
    pub struct DefaultRequestContextHandler;

    impl RequestContextHandler {
        fn resource_request_handler(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            let initiator = request_initiator.map(CefString::to_string).unwrap_or_default();
            if browser.is_none() || browser_policy::extension_origin(&initiator) {
                return Some(ExtensionFileBlockHandler::new());
            }
            None
        }
    }
}

/// Pool of non-default partitioned `CefRequestContext` instances managed by `EngineManager`.
pub struct ContextPool {
    entries: HashMap<ContextKey, ContextEntry>,
    /// Tracks which pooled context each open isolated `tab_id` belongs to for immediate teardown.
    tab_keys: HashMap<u64, ContextKey>,
    /// Monotonic generation per isolated `tab_id` guarding deferred loads against `tab_id` reuse.
    tab_gens: HashMap<u64, u64>,
    /// Live `ContextKind::Default` tabs (ready immediately) so `is_tab_ready(tab_id)` stays
    /// fail-closed (`false`) for any unknown or already-torn-down `tab_id`.
    default_tabs: HashSet<u64>,
    /// Pre-configured remote server proxies kept across tab close/reopen until cleared/dropped.
    remote_proxies: HashMap<String, RemoteProxyConfig>,
    /// Outcomes promoted by `on_request_context_initialized` ready for `EngineManager::poll_picks`.
    ready_loads: Arc<Mutex<Vec<DeferredOutcome>>>,
    next_generation: u64,
    pump: Sender<crate::Event>,
}

/// Result of resolving a tab's `ContextKind` before creating its CEF browser.
pub struct ResolvedContext {
    pub request_context: Option<RequestContext>,
    pub proxy: Option<RemoteProxyConfig>,
    /// Shared readiness flag (`true` immediately for `Default`; becomes `true` for isolated
    /// contexts ONLY when initialized, preferences verified via read-back, AND proxy is live).
    pub ready: Arc<AtomicBool>,
    /// True while a newly opened tab sits on placeholder `about:blank` waiting for its deferred
    /// initial URL so `TabLoadHandler` does not settle `loading: false` prematurely.
    pub deferred_initial: Arc<AtomicBool>,
    /// Monotonic generation token guarding deferred load/refusal outcomes against `tab_id` reuse.
    pub generation: u64,
    /// When `true`, open on `about:blank` and defer `initial_url` until context preferences verify.
    pub defer_initial_load: bool,
    /// Present when `resolve_with_hooks` rotated/unsuspended an existing `RemoteServer` context's
    /// proxy: `(server_id, new_config, activate_ready)`. Caller MUST sync `t.context` and
    /// `t.prompts.proxy` on all matching live tabs BEFORE flipping `activate_ready` to `true`.
    pub proxy_updated: Option<(String, RemoteProxyConfig, Option<Arc<AtomicBool>>)>,
}

impl ContextPool {
    pub fn new(pump: Sender<crate::Event>) -> Self {
        Self {
            entries: HashMap::new(),
            tab_keys: HashMap::new(),
            tab_gens: HashMap::new(),
            default_tabs: HashSet::new(),
            remote_proxies: HashMap::new(),
            ready_loads: Arc::new(Mutex::new(Vec::new())),
            next_generation: 1,
            pump,
        }
    }

    /// Fail-closed readiness check for any tab (`#281` & `#292`):
    /// - `false` for any unknown, closed, uninitialized, failed-pref, or proxy-suspended tab;
    /// - `true` for live `ContextKind::Default` tabs;
    /// - `e.ready` (`initialized && prefs_ok && !suspended && on_initialized ran`) for isolated tabs.
    pub fn is_tab_ready(&self, tab_id: u64) -> bool {
        if self.default_tabs.contains(&tab_id) {
            return true;
        }
        let Some(key) = self.tab_keys.get(&tab_id) else {
            return false;
        };
        self.entries
            .get(key)
            .is_some_and(|e| e.ready.load(Ordering::Acquire))
    }

    /// Explains why `!is_tab_ready(tab_id)` refused an operation.
    pub fn not_ready_reason(&self, tab_id: u64) -> &'static str {
        let Some(key) = self.tab_keys.get(&tab_id) else {
            return "browser.context.unavailable";
        };
        let Some(entry) = self.entries.get(key) else {
            return "browser.context.unavailable";
        };
        if entry.suspended.load(Ordering::Acquire) {
            "browser.proxy.required"
        } else if !entry.initialized.load(Ordering::Acquire) {
            if entry.require_proxy {
                "browser.proxy.required"
            } else {
                "browser.context.initializing"
            }
        } else if !entry.prefs_ok.load(Ordering::Acquire) {
            if entry.require_proxy {
                "browser.proxy.invalid"
            } else {
                "browser.context.unavailable"
            }
        } else {
            "browser.context.unavailable"
        }
    }

    /// Whether `tab_id` belongs to a remote context currently suspended by transient proxy loss.
    pub fn is_tab_suspended(&self, tab_id: u64) -> bool {
        self.tab_keys
            .get(&tab_id)
            .and_then(|k| self.entries.get(k))
            .is_some_and(|e| e.suspended.load(Ordering::Acquire))
    }

    /// Prunes any queued `ready_loads` entry for `tab_id` (prevents stale deferred load clobber
    /// if a ready tab receives a fresh `HostMsg::Navigate` before `poll_picks` drains).
    pub fn clear_pending_load(&self, tab_id: u64) {
        self.ready_loads
            .lock()
            .unwrap()
            .retain(|outcome| outcome.tab_id() != tab_id);
    }

    fn alloc_generation(&mut self) -> u64 {
        let g = self.next_generation;
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        g
    }

    /// Resolves (or creates) the partitioned `CefRequestContext` for `kind` with default hooks.
    pub fn resolve(
        &mut self,
        kind: &ContextKind,
        tab_id: u64,
        initial_url: &str,
    ) -> Result<ResolvedContext, ProxyFatal> {
        self.resolve_with_hooks(kind, tab_id, initial_url, ContextHooks::default())
    }

    /// Resolves (or creates) the partitioned `CefRequestContext` for `kind`, attaching optional
    /// PREVIEWS scheme/resource hooks. Always fails closed (never falls back to `Default` and
    /// never opens a `RemoteServer` tab without a valid authenticated loopback IP proxy).
    pub fn resolve_with_hooks(
        &mut self,
        kind: &ContextKind,
        tab_id: u64,
        initial_url: &str,
        hooks: ContextHooks,
    ) -> Result<ResolvedContext, ProxyFatal> {
        let fatal = |reason: &'static str| ProxyFatal {
            reason,
            close_tabs: Vec::new(),
        };
        let generation = self.alloc_generation();
        let (key, explicit_proxy, require_proxy) = match kind {
            ContextKind::Default => {
                self.release_tab(tab_id);
                self.default_tabs.insert(tab_id);
                return Ok(ResolvedContext {
                    request_context: None,
                    proxy: None,
                    ready: Arc::new(AtomicBool::new(true)),
                    deferred_initial: Arc::new(AtomicBool::new(false)),
                    generation,
                    defer_initial_load: false,
                    proxy_updated: None,
                });
            }
            ContextKind::Preview { workspace, grant } => {
                let ws = workspace.trim();
                let gr = grant.trim();
                if ws.is_empty() || gr.is_empty() {
                    return Err(fatal("browser.context.invalid"));
                }
                (
                    ContextKey::Preview {
                        workspace: ws.to_owned(),
                        grant: gr.to_owned(),
                    },
                    None,
                    false,
                )
            }
            ContextKind::Artifact { artifact_id } => {
                let aid = artifact_id.trim();
                if aid.is_empty() {
                    return Err(fatal("browser.context.invalid"));
                }
                (ContextKey::Artifact(aid.to_owned()), None, false)
            }
            ContextKind::Private => (ContextKey::Private(tab_id), None, false),
            ContextKind::RemoteServer { server_id, proxy } => {
                let sid = server_id.trim();
                if sid.is_empty() {
                    return Err(fatal("browser.context.invalid"));
                }
                if let Some(cfg) = proxy {
                    validate_proxy(cfg).map_err(fatal)?;
                }
                let effective = match proxy {
                    Some(cfg) => {
                        self.remote_proxies.insert(sid.to_owned(), cfg.clone());
                        cfg.clone()
                    }
                    None => match self.remote_proxies.get(sid).cloned() {
                        Some(cfg) => {
                            validate_proxy(&cfg).map_err(fatal)?;
                            cfg
                        }
                        None => return Err(fatal("browser.proxy.required")),
                    },
                };
                (ContextKey::RemoteServer(sid.to_owned()), Some(effective), true)
            }
        };

        let mut proxy_updated: Option<(String, RemoteProxyConfig, Option<Arc<AtomicBool>>)> = None;
        if !self.entries.contains_key(&key) {
            let entry = create_isolated_context(
                explicit_proxy.clone(),
                require_proxy,
                self.ready_loads.clone(),
                self.pump.clone(),
                hooks,
            )
            .map_err(fatal)?;
            self.entries.insert(key.clone(), entry);
        } else if let Some(cfg) = explicit_proxy {
            let sid = match &key {
                ContextKey::RemoteServer(s) => s.clone(),
                _ => String::new(),
            };
            let (needs_teardown, updated) = {
                let existing = self.entries.get_mut(&key).unwrap();
                let changed = existing.proxy.lock().unwrap().as_ref() != Some(&cfg)
                    || existing.suspended.load(Ordering::Acquire);
                if changed {
                    *existing.proxy.lock().unwrap() = Some(cfg.clone());
                    if existing.initialized.load(Ordering::Acquire) {
                        // Gate off BEFORE rotating prefs and closing old connections; caller flips
                        // `ready = true` LAST only after syncing `t.prompts.proxy` on all tabs.
                        existing.ready.store(false, Ordering::Release);
                        let ok = apply_context_prefs(&existing.ctx, Some(&cfg), true);
                        existing.prefs_ok.store(ok, Ordering::Release);
                        existing.suspended.store(!ok, Ordering::Release);
                        close_connections(&existing.ctx);
                        (!ok, ok.then(|| (sid, cfg, Some(existing.ready.clone()))))
                    } else {
                        existing.suspended.store(false, Ordering::Release);
                        (false, Some((sid, cfg, None)))
                    }
                } else {
                    (false, None)
                }
            };
            if needs_teardown {
                let close_tabs = self.teardown_key(&key);
                return Err(ProxyFatal {
                    reason: "browser.proxy.invalid",
                    close_tabs,
                });
            }
            proxy_updated = updated;
        }

        let is_init = self.entries.get(&key).unwrap().initialized.load(Ordering::Acquire);
        let prefs_ok = self.entries.get(&key).unwrap().prefs_ok.load(Ordering::Acquire);
        if is_init && !prefs_ok {
            let close_tabs = self.teardown_key(&key);
            return Err(ProxyFatal {
                reason: if require_proxy {
                    "browser.proxy.invalid"
                } else {
                    "browser.context.unavailable"
                },
                close_tabs,
            });
        }

        // Only release any prior mapping for `tab_id` AFTER the new context validated cleanly.
        self.default_tabs.remove(&tab_id);
        if self.tab_keys.get(&tab_id).is_some_and(|prev| prev != &key) {
            self.release_tab(tab_id);
        }

        let entry = self.entries.get_mut(&key).unwrap();
        let effective_proxy = entry.proxy.lock().unwrap().clone();
        entry.tabs.insert(tab_id);
        self.tab_keys.insert(tab_id, key);
        self.tab_gens.insert(tab_id, generation);

        // Defer EVERY uninitialized isolated context onto `about:blank` until
        // `on_request_context_initialized` verifies prefs and sets `ready = true`.
        let need_defer = !is_init;
        let defer_non_blank = need_defer && !initial_url.eq_ignore_ascii_case("about:blank");
        if need_defer {
            let mut loads = entry.deferred_loads.lock().unwrap();
            loads.retain(|(id, _, _)| *id != tab_id);
            loads.push((tab_id, generation, initial_url.to_owned()));
        }

        Ok(ResolvedContext {
            request_context: Some(entry.ctx.clone()),
            proxy: effective_proxy,
            ready: entry.ready.clone(),
            deferred_initial: Arc::new(AtomicBool::new(defer_non_blank)),
            generation,
            defer_initial_load: need_defer,
            proxy_updated,
        })
    }

    /// Gates `HostMsg::Navigate` for `tab_id` using the unified fail-closed readiness state:
    /// - `Default` tabs: returns `Ok(true)` immediately.
    /// - Unknown / torn-down tabs: fails closed with `Err("browser.context.unavailable")`.
    /// - Proxy-suspended remote tabs: fails closed with `Err("browser.proxy.required")`.
    /// - Uninitialized isolated tabs: queues the latest URL with the tab's generation (`Ok(false)`).
    /// - Ready isolated tabs: prunes any stale `ready_loads` and returns `Ok(true)`.
    pub fn gate_navigation(&mut self, tab_id: u64, url: &str) -> Result<bool, &'static str> {
        if self.default_tabs.contains(&tab_id) {
            return Ok(true);
        }
        self.clear_pending_load(tab_id);
        let Some(key) = self.tab_keys.get(&tab_id) else {
            return Err("browser.context.unavailable");
        };
        let Some(entry) = self.entries.get(key) else {
            return Err("browser.context.unavailable");
        };
        if entry.suspended.load(Ordering::Acquire) {
            return Err("browser.proxy.required");
        }
        if !entry.initialized.load(Ordering::Acquire) {
            let generation = self.tab_gens.get(&tab_id).copied().unwrap_or(0);
            let mut loads = entry.deferred_loads.lock().unwrap();
            loads.retain(|(id, _, _)| *id != tab_id);
            loads.push((tab_id, generation, url.to_owned()));
            return Ok(false);
        }
        if !entry.ready.load(Ordering::Acquire) {
            return Err(if entry.require_proxy {
                "browser.proxy.invalid"
            } else {
                "browser.context.unavailable"
            });
        }
        Ok(true)
    }

    /// Releases `tab_id` from its pooled context and prunes any queued `ready_loads` for `tab_id`.
    /// When the context's last tab closes, removes the entry immediately, clears scheme factories,
    /// closes connections, wipes in-memory cookies, and drops the `CefRequestContext` handle.
    /// Returns the torn-down `ContextKey` if the last tab closed (for the remote cookie mirror).
    pub fn release_tab(&mut self, tab_id: u64) -> Option<ContextKey> {
        self.default_tabs.remove(&tab_id);
        self.clear_pending_load(tab_id);
        self.tab_gens.remove(&tab_id);
        let key = self.tab_keys.remove(&tab_id)?;
        let empty = if let Some(entry) = self.entries.get_mut(&key) {
            entry.tabs.remove(&tab_id);
            entry.deferred_loads.lock().unwrap().retain(|(id, _, _)| *id != tab_id);
            entry.tabs.is_empty()
        } else {
            false
        };
        if empty && let Some(entry) = self.entries.remove(&key) {
            entry.teardown();
            return Some(key);
        }
        None
    }

    /// Tears down every pooled isolated context on engine shutdown.
    pub fn release_all(&mut self) {
        self.default_tabs.clear();
        self.tab_keys.clear();
        self.tab_gens.clear();
        self.remote_proxies.clear();
        self.ready_loads.lock().unwrap().clear();
        for (_, entry) in self.entries.drain() {
            entry.teardown();
        }
    }

    /// Updates or suspends the local loopback proxy on a remote server partition:
    /// - `proxy: None` (transient tunnel loss): suspends the live context (`ready = false`) without
    ///   destroying open user tabs or unsaved JS state (blocking all requests/CDP/menu/nav fail-closed).
    /// - `proxy: Some(cfg)`: validates strictly, applies & verifies read-back, and sets `ready = true`;
    ///   on fatal verification failure tears down the broken context and returns `close_tabs`.
    pub fn set_partition_proxy(
        &mut self,
        server_id: &str,
        proxy: Option<RemoteProxyConfig>,
    ) -> Result<ProxyUpdate, ProxyFatal> {
        let sid = server_id.trim();
        if sid.is_empty() {
            return Err(ProxyFatal {
                reason: "browser.context.invalid",
                close_tabs: Vec::new(),
            });
        }
        let key = ContextKey::RemoteServer(sid.to_owned());
        let Some(cfg) = proxy else {
            self.remote_proxies.remove(sid);
            let mut suspended_tabs = Vec::new();
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.suspended.store(true, Ordering::Release);
                entry.ready.store(false, Ordering::Release);
                *entry.proxy.lock().unwrap() = None;
                entry.deferred_loads.lock().unwrap().clear();
                close_connections(&entry.ctx);
                suspended_tabs.extend(entry.tabs.iter().copied());
            }
            for &tid in &suspended_tabs {
                self.clear_pending_load(tid);
            }
            return Ok(ProxyUpdate::Suspended(suspended_tabs));
        };
        validate_proxy(&cfg).map_err(|reason| ProxyFatal {
            reason,
            close_tabs: Vec::new(),
        })?;
        self.remote_proxies.insert(sid.to_owned(), cfg.clone());
        let (failed, activate_ready) = if let Some(entry) = self.entries.get_mut(&key) {
            *entry.proxy.lock().unwrap() = Some(cfg.clone());
            if entry.initialized.load(Ordering::Acquire) {
                // Gate off BEFORE applying new proxy & closing old sockets; caller flips
                // `ready = true` LAST only after syncing `t.prompts.proxy` on every live tab.
                entry.ready.store(false, Ordering::Release);
                let ok = apply_context_prefs(&entry.ctx, Some(&cfg), true);
                entry.prefs_ok.store(ok, Ordering::Release);
                entry.suspended.store(!ok, Ordering::Release);
                close_connections(&entry.ctx);
                (!ok, ok.then(|| entry.ready.clone()))
            } else {
                entry.suspended.store(false, Ordering::Release);
                (false, None)
            }
        } else {
            (false, None)
        };
        if failed {
            let close_tabs = self.teardown_key(&key);
            return Err(ProxyFatal {
                reason: "browser.proxy.invalid",
                close_tabs,
            });
        }
        Ok(ProxyUpdate::Applied {
            config: cfg,
            activate_ready,
        })
    }

    /// Lists all live `tab_id`s currently attached to `kind`'s pooled context.
    pub fn tabs_for_context(&self, kind: &ContextKind) -> Vec<u64> {
        let Some(key) = Self::key_for_drop(kind) else {
            return Vec::new();
        };
        self.entries
            .get(&key)
            .map(|e| e.tabs.iter().copied().collect())
            .unwrap_or_default()
    }

    fn key_for_drop(kind: &ContextKind) -> Option<ContextKey> {
        match kind {
            ContextKind::Default | ContextKind::Private => None,
            ContextKind::Preview { workspace, grant } => Some(ContextKey::Preview {
                workspace: workspace.trim().to_owned(),
                grant: grant.trim().to_owned(),
            }),
            ContextKind::Artifact { artifact_id } => {
                Some(ContextKey::Artifact(artifact_id.trim().to_owned()))
            }
            ContextKind::RemoteServer { server_id, .. } => {
                Some(ContextKey::RemoteServer(server_id.trim().to_owned()))
            }
        }
    }

    fn teardown_key(&mut self, key: &ContextKey) -> Vec<u64> {
        let Some(entry) = self.entries.remove(key) else {
            return Vec::new();
        };
        let tabs: Vec<u64> = entry.tabs.iter().copied().collect();
        for &tid in &tabs {
            self.tab_keys.remove(&tid);
            self.tab_gens.remove(&tid);
            self.clear_pending_load(tid);
        }
        entry.teardown();
        tabs
    }

    /// Returns the live `RequestContext` for a canonical `ContextKey`.
    #[allow(dead_code)]
    pub fn context_for_key(&self, key: &ContextKey) -> Option<RequestContext> {
        self.entries.get(key).map(|e| e.ctx.clone())
    }

    /// Returns the live `RequestContext` for a remote server identity (for Phase 3b cookie mirror).
    pub fn remote_context(&self, server_id: &str) -> Option<RequestContext> {
        self.entries
            .get(&ContextKey::RemoteServer(server_id.trim().to_owned()))
            .map(|e| e.ctx.clone())
    }

    /// Returns all active remote server partitions and their `RequestContext`s.
    pub fn remote_partitions(&self) -> Vec<(String, RequestContext)> {
        self.entries
            .iter()
            .filter_map(|(k, e)| match k {
                ContextKey::RemoteServer(id) => Some((id.clone(), e.ctx.clone())),
                _ => None,
            })
            .collect()
    }

    /// Explicitly drops a pooled context (for revocation / sign-out / server removal).
    /// Callers MUST close `tabs_for_context(kind)` first so Chromium drops the `BrowserContext`.
    pub fn drop_context(&mut self, kind: &ContextKind) {
        if let ContextKind::RemoteServer { server_id, .. } = kind {
            self.remote_proxies.remove(server_id.trim());
        }
        if let Some(k) = Self::key_for_drop(kind) {
            let _ = self.teardown_key(&k);
        }
    }

    /// Drains any initial tab URLs (or fail-closed refusals) whose `RequestContext` finished init.
    pub fn drain_ready_loads(&self) -> Vec<DeferredOutcome> {
        std::mem::take(&mut *self.ready_loads.lock().unwrap())
    }
}

fn create_isolated_context(
    proxy: Option<RemoteProxyConfig>,
    require_proxy: bool,
    ready_loads: Arc<Mutex<Vec<DeferredOutcome>>>,
    pump: Sender<crate::Event>,
    hooks: ContextHooks,
) -> Result<ContextEntry, &'static str> {
    let initialized = Arc::new(AtomicBool::new(false));
    let prefs_ok = Arc::new(AtomicBool::new(false));
    let suspended = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let proxy_arc = Arc::new(Mutex::new(proxy));
    let deferred_loads = Arc::new(Mutex::new(Vec::new()));
    let mut handler = IsolatedContextHandler::new(
        require_proxy,
        initialized.clone(),
        prefs_ok.clone(),
        suspended.clone(),
        ready.clone(),
        proxy_arc.clone(),
        ready_loads,
        deferred_loads.clone(),
        Arc::new(Mutex::new(hooks.on_initialized)),
        hooks.resource_filter,
        pump,
    );
    // Empty `cache_path` + `persist_session_cookies: 0` creates a strictly ephemeral in-memory
    // storage/cookie/cache/SW partition isolated from `OPENCODE_BROWSER_PROFILE` and extensions.
    let settings = RequestContextSettings {
        size: std::mem::size_of::<sys::_cef_request_context_settings_t>(),
        cache_path: CefString::default(),
        persist_session_cookies: 0,
        accept_language_list: CefString::default(),
        cookieable_schemes_list: CefString::default(),
        cookieable_schemes_exclude_defaults: i32::from(hooks.exclude_default_cookie_schemes),
    };
    let Some(ctx) = cef::request_context_create_context(Some(&settings), Some(&mut handler)) else {
        return Err("browser.context.unavailable");
    };
    Ok(ContextEntry {
        ctx,
        tabs: HashSet::new(),
        require_proxy,
        initialized,
        prefs_ok,
        suspended,
        ready,
        proxy: proxy_arc,
        deferred_loads,
    })
}
