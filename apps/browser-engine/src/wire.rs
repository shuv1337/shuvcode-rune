//! Wire message definitions matching `crates/browser-pane/src/protocol.rs` without pulling `rune`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[path = "../../../crates/browser-pane/src/device.rs"]
pub mod device;

pub const MOD_SHIFT: u8 = 1 << 0;
pub const MOD_CTRL: u8 = 1 << 1;
pub const MOD_ALT: u8 = 1 << 2;
pub const MOD_CMD: u8 = 1 << 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerBtn {
    #[default]
    Primary,
    Secondary,
    Middle,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorKind {
    #[default]
    Default,
    Pointer,
    Text,
    Crosshair,
    ResizeColumn,
    ResizeRow,
    Grab,
    Grabbing,
    NotAllowed,
    Move,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TabSnapshot {
    pub tab_id: u64,
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_go_back: bool,
    pub can_go_forward: bool,
    pub zoom: f64,
    pub cursor: CursorKind,
    /// Why the page shows no document: Chromium's net error for a main-frame load that failed, or
    /// `page_crashed` when its renderer went away. Cleared by the next document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_error: Option<String>,
    /// Site favicon as a bounded `data:image/png;base64,...` data URL (`UP-109` `#65094`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favicon: Option<String>,
    /// Isolation scope of this tab (with proxy credentials stripped), preserved across reconnects.
    #[serde(default, skip_serializing_if = "ContextKind::is_default")]
    pub context: ContextKind,
    /// Set on isolated `oc-preview://` tabs so browser chrome marks them as previews.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<PreviewKindTag>,
    /// Whether this tab is agent-driven (preserved across reconnects so file choosers / HTTP auth stay guarded).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent_driven: bool,
    /// Counts main-frame document loads (`on_load_start`); unchanged by `history.pushState`,
    /// `replaceState` and hash changes. `0` from engines before it existed.
    #[serde(default)]
    pub document: u64,
    /// Whether this tab is producing audible audio (reserved for Phase 2 hibernation protection).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub audible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreviewKindTag {
    Local,
    Remote,
    Artifact,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PreviewSpec {
    Local { root: String, document: String },
    Remote { root: String, session_prefix: String, document: String },
    Artifact { artifact_id: String, path: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PickedElement {
    pub tab_id: u64,
    pub eref: String,
    pub backend_node_id: i64,
    pub selector: String,
    pub label: String,
    pub role: Option<String>,
    pub name: Option<String>,
    pub text: Option<String>,
    pub rect: [f32; 4],
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMsg {
    /// `prompts`: the app shows and answers page dialogs, permission prompts, and HTTP auth;
    /// older apps leave it false so the engine suppresses dialogs and denies permissions/auth.
    /// `extensions`: the app handles extension-initiated tab opens (`EngineMsg::ExtensionOpenTab`).
    /// `download_save`: the app handles `EngineMsg::DownloadSaveRequest` via a native Save dialog.
    Hello {
        mach_service: Option<String>,
        pid: u32,
        #[serde(default)]
        prompts: bool,
        #[serde(default)]
        extensions: bool,
        #[serde(default)]
        download_save: bool,
        #[serde(default)]
        tab_lifecycle: bool,
    },
    OpenTab {
        tab_id: u64,
        url: String,
        width_pt: f32,
        height_pt: f32,
        scale: f32,
        focused: bool,
        #[serde(default)]
        file_roots: Vec<String>,
        /// Partitioned `CefRequestContext` (`Default`, `IsolatedPreview`, `RemoteServer`, or
        /// `Private`). Non-default contexts are strictly invisible to extensions.
        #[serde(default)]
        context: ContextKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<PreviewSpec>,
        /// Set before browser creation so cold-start / restarted agent tabs never race OS file choosers or HTTP auth.
        #[serde(default)]
        agent_driven: bool,
        /// Legacy shorthand for `ContextKind::Private` when `context` is `Default`.
        #[serde(default)]
        isolated: bool,
        /// Allows navigating to loaded `chrome-extension://` pages (user tabs & popups only;
        /// never set for agent-driven or non-default context tabs).
        #[serde(default)]
        allow_extension_pages: bool,
        /// True when this tab is an anchored extension popup (enables content auto-sizing).
        #[serde(default)]
        is_popup: bool,
        /// When answering `EngineMsg::ExtensionOpenTab`, correlates this newly opened pane tab.
        #[serde(default)]
        ext_open_request_id: Option<u64>,
    },
    ClickExtension { ext_id: String },
    ListExtensions { request_id: u64 },
    UninstallExtension { ext_id: String },
    ReloadExtension { ext_id: String },
    CancelExtensionOpenTab { request_id: u64 },
    ExtensionMetrics { request_id: u64 },
    SetTabPolicy { tab_id: u64, file_roots: Vec<String> },
    CloseTab { tab_id: u64 },
    FocusTab { tab_id: u64 },
    /// Exact set of currently visible pane tabs (`vec![]` when panel closed, on Home/Settings, or hidden).
    SetVisibleTabs { tab_ids: Vec<u64> },
    /// Explicit agent RPC unthrottle hold, decoupled from security-sticky `agent_driven`.
    SetTabUnthrottled { tab_id: u64, active: bool },
    Navigate { tab_id: u64, url: String },
    Back { tab_id: u64 },
    Forward { tab_id: u64 },
    Reload {
        tab_id: u64,
        #[serde(default)]
        ignore_cache: bool,
    },
    Stop { tab_id: u64 },
    SetZoom { tab_id: u64, factor: f64 },
    SetColorScheme { dark: bool },
    SetFrameRate { fps: u32 },
    Resize { tab_id: u64, width_pt: f32, height_pt: f32, scale: f32 },
    Emulate { tab_id: u64, device: Option<device::Device>, scale: f32 },
    MouseMove { tab_id: u64, x: f32, y: f32, mods: u8, leave: bool },
    MouseButton { tab_id: u64, x: f32, y: f32, button: PointerBtn, up: bool, clicks: u32, mods: u8 },
    Wheel { tab_id: u64, x: f32, y: f32, dx: f32, dy: f32, mods: u8 },
    Key { tab_id: u64, down: bool, text: Option<String>, named: Option<String>, mods: u8 },
    EditCmd { tab_id: u64, cmd: String },
    Ime { tab_id: u64, commit: Option<String>, preedit: Option<String> },
    SetInspect { tab_id: u64, enabled: bool },
    HighlightNode { tab_id: u64, backend_node_id: Option<i64> },
    CdpCall { tab_id: u64, request_id: u64, method: String, params: Value, session_id: Option<String> },
    SiteCookies { tab_id: u64, request_id: u64 },
    ClearSite { tab_id: u64, request_id: u64 },
    /// Clears origin-scoped cache storage, service workers, and HTTP cache while keeping cookies.
    ClearSiteCache { tab_id: u64, request_id: u64 },
    /// In-page text search (`FindHandler`).
    Find {
        tab_id: u64,
        query: String,
        forward: bool,
        match_case: bool,
        find_next: bool,
        #[serde(default)]
        user_initiated: bool,
    },
    StopFind { tab_id: u64, clear_selection: bool },
    /// Executes or cancels a held page right-click context menu (`EngineMsg::ContextMenu`).
    ContextMenuAction { tab_id: u64, id: u64, action: Option<ContextMenuCommand> },
    /// Updates or clears the local loopback tunnel proxy on a remote server partition.
    SetPartitionProxy {
        #[serde(alias = "partition")]
        server_id: String,
        proxy: Option<RemoteProxyConfig>,
    },
    /// Closes all connections and drops a non-default preview or remote server context.
    DropContext { context: ContextKind },
    /// Lists all cookies in the default profile (or a `RemoteServer` partition), excluding probe.
    ProfileCookiesList {
        request_id: u64,
        #[serde(default)]
        partition: Option<String>,
    },
    /// Imports/writes cookies into the default profile (or a `RemoteServer` partition) and flushes.
    ProfileCookiesSet {
        request_id: u64,
        #[serde(default)]
        partition: Option<String>,
        cookies: Vec<WireCookie>,
    },
    /// Deletes a single cookie by URL and name and flushes the store.
    ProfileCookieDelete {
        request_id: u64,
        #[serde(default)]
        partition: Option<String>,
        url: String,
        name: String,
    },
    /// Writes a short-lived probe cookie and flushes so the host can verify disk persistence.
    ProfileProbeCookies { request_id: u64 },
    /// "Sign out everywhere": deletes all cookies, HTTP auth credentials, cert exceptions, and
    /// site storage/caches across the default profile and every active remote server partition.
    /// `sites`: origins the clear is checked against afterwards (every site the profile had cookies
    /// for); older apps send none, and only the cookies are checked.
    ClearProfileData {
        request_id: u64,
        #[serde(default)]
        sites: Vec<String>,
    },
    /// Toggles two-way cookie mirroring between the default profile and remote server partitions.
    SetShareSignIns { enabled: bool },
    /// Explicitly reconciles a remote server partition's cookies with the default profile.
    SyncRemoteCookies {
        request_id: u64,
        partition: String,
        #[serde(default)]
        from_remote: bool,
    },
    /// Linux: the app's GPU has finished reading `frame_id` (and every frame before it) of
    /// buffer set `pool`; a read of a set the tab no longer has frees nothing.
    FrameRead { tab_id: u64, pool: u64, frame_id: u64 },
    /// Linux: the app can't import the engine's dmabufs; frames come as BGRA paints from now on.
    DmabufFailed { reason: String },
    /// The app's answer to a held JavaScript dialog or `beforeunload` (`EngineMsg::Dialog`).
    AnswerDialog { tab_id: u64, id: u64, accept: bool, text: String },
    /// The app's answer to a held permission request (`EngineMsg::PermissionRequest`).
    AnswerPermission {
        tab_id: u64,
        id: u64,
        allow: bool,
        /// When `true` and `!allow`, answers with `CEF_PERMISSION_RESULT_DISMISS` so Chromium does
        /// not permanently block the origin (for `"Not now"`, timeouts, and origin changes).
        #[serde(default)]
        dismiss: bool,
        /// When `true` (`"Always allow"` / `"Never allow"` in `Default`), keeps the grant/block
        /// persistent in Chromium's `HostContentSettingsMap` until changed in `Settings > Browser`.
        /// When `false` (`"Allow this time"`), tracked as a per-tab visit grant and reset on
        /// cross-origin navigation, tab close, or shutdown.
        #[serde(default)]
        persist: bool,
    },
    /// Resets Chromium's `HostContentSettingsMap` entry for `(origin, kind)` back to `DEFAULT`
    /// when a site permission is revoked or changed in `Settings > Browser`.
    ResetPermission { origin: String, kind: PermissionKind },
    /// Syncs Chromium's `Default` `HostContentSettingsMap` to match Rune's persisted
    /// `rune.sitePermissions` (`Default` `"Always allow"` / `"Never allow"` decisions).
    SyncSitePermissions { entries: Vec<SitePermissionEntry> },
    /// The app's answer to an HTTP 401 auth challenge (`EngineMsg::AuthRequest`); `None` cancels.
    AnswerAuth { tab_id: u64, id: u64, username: Option<String>, password: Option<String> },
    /// The app's answer to a human download Save dialog (`EngineMsg::DownloadSaveRequest`); `None` cancels.
    AnswerDownloadSave { tab_id: u64, id: u64, path: Option<String> },
    /// Marks a tab as agent-driven (cancels native file choosers, skips `beforeunload`, rejects HTTP auth).
    SetAgentDriven { tab_id: u64, agent_driven: bool },
    /// Typed CDP call issued strictly by Rune's native Rust `experimental.browser` executors
    /// (`browser_policy::check_typed_cdp`), never exposed to untrusted Luau plugins.
    TypedCdpCall {
        tab_id: u64,
        request_id: u64,
        method: String,
        params: Value,
        session_id: Option<String>,
        /// The frame an element ref came from; the engine checks it belongs to `session_id`.
        #[serde(default)]
        frame_id: Option<String>,
    },
    /// Sets files on a `<input type="file">` strictly from `<profile>/opencode-page-uploads/<tab_id>/`.
    UploadFiles {
        tab_id: u64,
        request_id: u64,
        backend_node_id: i64,
        files: Vec<String>,
        #[serde(default)]
        session_id: Option<String>,
        #[serde(default)]
        frame_id: Option<String>,
    },
    /// Drops staged files onto `(x, y)` strictly from `<profile>/opencode-page-uploads/<tab_id>/`.
    DropFiles { tab_id: u64, request_id: u64, x: f64, y: f64, files: Vec<String> },
    /// App-proxied response headers for a remote workspace or artifact `EngineMsg::PreviewFetch`.
    PreviewResponseHead {
        req_id: u64,
        ok: bool,
        status: u16,
        mime: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_length: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_range: Option<String>,
    },
    /// App-proxied base64 body chunk for a pending `EngineMsg::PreviewFetch`.
    PreviewResponseChunk { req_id: u64, data_b64: String },
    /// Marks EOF (`ok: true`) or failure (`ok: false`) for a pending `EngineMsg::PreviewFetch`.
    PreviewResponseEnd { req_id: u64, ok: bool },
    /// User clicked or dismissed an OS notification posted by an extension (`chrome.notifications`).
    ExtensionNotificationEvent {
        ext_id: String,
        notif_id: String,
        clicked: bool,
        by_user: bool,
    },
    /// Whether the app's OS notification sink & Settings > Notifications allow extension toasts.
    SetNotificationsEnabled { enabled: bool },
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EngineMsg {
    /// `downloads`: keeps page downloads; `policy`: enforces URL/CDP policy; `prompts`: holds
    /// dialogs/permissions/auth for the app; `popups`: routes pop-ups to managed tabs; `find`:
    /// supports in-page `FindHandler`; `context_menu`: routes right-click menus to the app;
    /// `partitions` / `remote_proxy`: isolated `CefRequestContext` partitions; `agent_tools`:
    /// supports `TypedCdpCall` and flattened OOPIF `sessionId` CDP events; `preview`: serves
    /// isolated `oc-preview://` tabs; `extensions`: hosts verified MV3 extensions over the private
    /// in-process CDP pipe; `agent_files`: confined `UploadFiles` and `DropFiles`.
    Ready {
        chromium_major: u32,
        pid: u32,
        downloads: bool,
        policy: bool,
        #[serde(default)]
        prompts: bool,
        #[serde(default)]
        popups: bool,
        #[serde(default)]
        find: bool,
        #[serde(default)]
        context_menu: bool,
        #[serde(default)]
        partitions: bool,
        #[serde(default)]
        remote_proxy: bool,
        #[serde(default)]
        agent_tools: bool,
        #[serde(default)]
        preview: bool,
        /// The engine tracks one-time vs persistent permissions and supports
        /// `HostMsg::SyncSitePermissions`, `HostMsg::ResetPermission`, and `AnswerPermission.{dismiss,persist}`.
        #[serde(default)]
        permission_sync: bool,
        #[serde(default)]
        preview_credit: bool,
        /// The engine owns `Target.setAutoAttach` / frame exclusion authority (`EngineCdpLedger`).
        #[serde(default)]
        frame_authority: bool,
        #[serde(default)]
        extensions: bool,
        /// The engine exposes `CefCookieManager` list/import/delete/probe/clear & two-way remote cookie mirroring.
        #[serde(default)]
        cookies: bool,
        #[serde(default)]
        agent_files: bool,
        /// The engine enforces the WebAuthn / passkey gate (`TabPasskeyGate`) and emits `PasskeyRefused`.
        #[serde(default)]
        passkeys: bool,
        /// The engine enforces committed-main-frame renderer PID trace isolation and a single engine-wide trace lock.
        #[serde(default)]
        trace_isolation: bool,
        /// The engine runs Copy image / Spellcheck / Languages page-menu commands and fills
        /// `PageContextMenu.{spellcheck, languages, dictionaries}`.
        #[serde(default)]
        menu_extras: bool,
        /// The engine supports `SetVisibleTabs` and `SetTabUnthrottled` tab visibility lifecycle.
        #[serde(default)]
        tab_lifecycle: bool,
    },
    /// Copy image's result: the image under the menu, as PNG (base64), for the app's clipboard.
    CopiedImage { tab_id: u64, png_b64: String },
    /// A tab whose first navigation became the person's download, now placed (or cancelled):
    /// nothing to show (official `chromium.ts:1076-1083`).
    DownloadOnlyTab { tab_id: u64 },
    ProfileCookies { request_id: u64, cookies: Vec<WireCookie> },
    ProfileCookiesImported { request_id: u64, imported: u32, failed: u32 },
    ProfileProbeReady { request_id: u64, written: bool, cache_dir: String },
    ProfileActionDone { request_id: u64, ok: bool },
    /// Requests bytes for a remote workspace or artifact preview from the app host (server
    /// credentials never enter the engine process).
    PreviewFetch {
        req_id: u64,
        tab_id: u64,
        kind: PreviewKindTag,
        path: String,
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range: Option<String>,
    },
    /// A preview page attempted to load a file outside its grant or a secret/hidden path.
    PreviewDenied { tab_id: u64 },
    Refused { tab_id: u64, reason: String },
    /// A controlled (`agent_driven`) `Default`-profile tab was refused because its origin holds an
    /// inherited grant (`clipboard-read`, `geolocation`, or `notifications`). Carries the refused
    /// URL so the host can show the notice bar and offer "Open in new tab" without altering `t.url`.
    ControlledPermissionRefused {
        tab_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
    },
    Dialog { tab_id: u64, dialog: PageDialog },
    DialogsReset { tab_id: u64 },
    PermissionRequest { tab_id: u64, request: PermissionPrompt },
    PermissionDismissed { tab_id: u64, id: u64 },
    AuthRequest { tab_id: u64, challenge: AuthChallenge },
    AuthReset { tab_id: u64 },
    AuthAgentRequired { tab_id: u64, origin: String },
    PopupOpen { tab_id: u64, url: String, background: bool },
    PopupBlocked { tab_id: u64, url: String },
    /// A user-activated non-agent page click on an allow-listed app scheme (`slack:`, `vscode:`,
    /// `mailto:`, etc.) was cancelled in CEF and forwarded to the desktop review bar.
    AppLinkBlocked { tab_id: u64, url: String },
    /// A site asked for a passkey or security key (`navigator.credentials.{get,create}` with
    /// `publicKey`) and the tab's passkey gate refused it (`webauthn-gate.ts`).
    PasskeyRefused {
        tab_id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
        #[serde(default)]
        main_frame: bool,
        kind: String,
    },
    FileChooserBlocked { tab_id: u64 },
    FindResult { tab_id: u64, count: u32, active_match: u32, final_update: bool },
    ContextMenu { tab_id: u64, menu: PageContextMenu },
    PageDiagnostic { tab_id: u64, level: String, event: String, fields: Value },
    TabList { tabs: Vec<TabSnapshot> },
    TabUpdate { tab: TabSnapshot },
    TabClosed { tab_id: u64 },
    InspectState { tab_id: u64, active: bool, picked: Option<PickedElement> },
    CdpResponse { tab_id: u64, request_id: u64, ok: bool, result: Value },
    CdpEvent { tab_id: u64, method: String, params: Value, session_id: Option<String> },
    CpuFrame { tab_id: u64, frame_id: u64, width: u32, height: u32, bgra_b64: String },
    /// Windows: a finished BGRA frame in the named D3D11 shared texture. The app signals the
    /// named shared fence to `frame_id` once it has read it, and the engine reuses the texture
    /// only after that.
    SharedFrame { tab_id: u64, frame_id: u64, width: u32, height: u32, texture: String, release_fence: String },
    /// A download started, moved on or ended (agent-driven tabs only). An app from before it skips the message.
    Download { tab_id: u64, file: DownloadFile },
    /// A human-initiated download (or right-click "Save image as…") awaiting an OS Save dialog path
    /// outside the engine profile and `opencode-page-downloads`.
    DownloadSaveRequest {
        tab_id: u64,
        id: u64,
        name: String,
        mime: String,
        #[serde(default)]
        executable: bool,
    },
    /// Linux: a tab's new linear ARGB8888 dmabufs, one file descriptor each, sent with this line
    /// (`SCM_RIGHTS`). Frames name them by `pool` and index.
    DmabufPool { tab_id: u64, pool: u64, buffers: u32, width: u32, height: u32, stride: u32, offset: u32, modifier: u64 },
    /// Linux: a finished frame in buffer `slot` of `pool`. The app says when it has read it
    /// (`HostMsg::FrameRead`), and the engine writes that buffer again only after.
    DmabufFrame { tab_id: u64, frame_id: u64, pool: u64, slot: u32 },
    ExtensionsState {
        request_id: Option<u64>,
        extensions: Vec<crate::extensions::ExtensionActionSnapshot>,
    },
    ExtensionAction {
        action: crate::extensions::ExtensionActionSnapshot,
    },
    ExtensionPopupRequested {
        ext_id: String,
        url: String,
    },
    ExtensionPopupResized {
        ext_id: String,
        width: u32,
        height: u32,
    },
    ExtensionOpenTab {
        request_id: u64,
        ext_id: String,
        url: String,
        active: bool,
    },
    ExtensionFocusTab {
        tab_id: u64,
    },
    ExtensionFiltersReady,
    ExtensionUninstalled {
        ext_id: String,
        ok: bool,
        error: Option<String>,
    },
    ExtensionReloaded {
        ext_id: String,
        ok: bool,
        error: Option<String>,
    },
    ExtensionIntegrityFailed {
        ext_id: String,
    },
    /// Extension called `chrome.notifications.create`/`update`; app posts via OS notification center.
    ExtensionNotificationShow {
        ext_id: String,
        ext_name: String,
        notif_id: String,
        body: String,
    },
    /// Extension cleared or uninstalled a notification; app removes it from OS notification center.
    ExtensionNotificationClear {
        ext_id: String,
        notif_id: String,
    },
    ExtensionMetrics {
        request_id: u64,
        metrics: crate::extensions::ShimMetrics,
    },
    /// Byte-credit replenishment (or immediate cancellation) for an in-flight `PreviewFetch`.
    PreviewCredit {
        req_id: u64,
        bytes: u32,
        #[serde(default)]
        cancel: bool,
    },
}

/// A file a page downloaded, kept in the engine's private downloads folder for the agent's tools
/// (Desktop's browser keeps them the same way: `gui-plugins/src/browser/files.ts`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DownloadFile {
    /// `file_<n>`, unique in the engine's run.
    pub id: String,
    /// The name the page gave it.
    pub name: String,
    pub mime: String,
    /// Bytes received so far.
    pub bytes: u64,
    pub state: DownloadState,
    /// Where it is on this computer.
    pub path: String,
    /// Why it failed: `too_large` (over 5 MiB, Desktop's limit), `canceled`, or Chromium's
    /// interrupt reason (`network_failed`, `file_no_space`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Source URL captured by the engine in `on_before_download` before any post-download redirect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Pending,
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageDialog {
    pub id: u64,
    pub kind: DialogKind,
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub default_text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogKind {
    Alert,
    Confirm,
    Prompt,
    Leave,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionKind {
    Camera,
    Microphone,
    Location,
    Notifications,
    ClipboardRead,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SitePermissionEntry {
    pub origin: String,
    pub kind: PermissionKind,
    pub allow: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPrompt {
    pub id: u64,
    pub origin: String,
    pub top_origin: String,
    pub kinds: Vec<PermissionKind>,
    #[serde(default)]
    pub is_media: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthChallenge {
    pub id: u64,
    pub origin: String,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub realm: String,
    pub scheme: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionMenuItemKind {
    #[default]
    Normal,
    Checkbox,
    Radio,
    Separator,
}

fn default_menu_enabled() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionMenuItem {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub kind: ExtensionMenuItemKind,
    #[serde(default)]
    pub checked: bool,
    #[serde(default = "default_menu_enabled")]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<ExtensionMenuItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionMenuGroup {
    pub ext_id: String,
    pub title: String,
    pub items: Vec<ExtensionMenuItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageContextMenu {
    pub id: u64,
    pub x: i32,
    pub y: i32,
    #[serde(default)]
    pub page_url: String,
    #[serde(default)]
    pub frame_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub src_url: Option<String>,
    #[serde(default)]
    pub is_image: bool,
    #[serde(default)]
    pub has_image_contents: bool,
    #[serde(default)]
    pub is_editable: bool,
    #[serde(default)]
    pub selection_text: String,
    #[serde(default)]
    pub can_cut: bool,
    #[serde(default)]
    pub can_copy: bool,
    #[serde(default)]
    pub can_paste: bool,
    #[serde(default)]
    pub can_select_all: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub misspelled_word: Option<String>,
    #[serde(default)]
    pub suggestions: Vec<String>,
    /// Editable fields only: whether spellcheck is on, the enabled languages, and the languages
    /// the Languages submenu offers (empty on macOS).
    #[serde(default)]
    pub spellcheck: bool,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub dictionaries: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<ExtensionMenuGroup>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteProxyConfig {
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
}

impl std::fmt::Debug for RemoteProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteProxyConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("realm", &self.realm)
            .finish()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextKind {
    #[default]
    Default,
    /// Workspace preview (`oc-preview://`): distinct per `(workspace, grant)`.
    Preview {
        workspace: String,
        grant: String,
    },
    /// Artifact preview: distinct namespace from workspace previews.
    Artifact {
        artifact_id: String,
    },
    /// Per-tab ephemeral private context (keyed strictly by `tab_id`).
    Private,
    /// Per-remote-server isolated context keyed strictly by canonical server identity.
    RemoteServer {
        #[serde(alias = "partition")]
        server_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proxy: Option<RemoteProxyConfig>,
    },
}

impl ContextKind {
    pub fn is_default(&self) -> bool {
        matches!(self, Self::Default)
    }

    pub fn is_isolated(&self) -> bool {
        !self.is_default()
    }

    /// Returns a wire-safe copy with `RemoteProxyConfig` credentials omitted for `TabSnapshot`.
    pub fn redacted(&self) -> Self {
        match self {
            Self::RemoteServer { server_id, .. } => Self::RemoteServer {
                server_id: server_id.trim().to_owned(),
                proxy: None,
            },
            Self::Preview { workspace, grant } => Self::Preview {
                workspace: workspace.trim().to_owned(),
                grant: grant.trim().to_owned(),
            },
            Self::Artifact { artifact_id } => Self::Artifact {
                artifact_id: artifact_id.trim().to_owned(),
            },
            other => other.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextMenuCommand {
    Back,
    Forward,
    Reload,
    HardReload,
    Cut,
    Copy,
    Paste,
    PastePlain,
    SelectAll,
    SaveImage { url: String },
    ReplaceMisspelling { word: String },
    AddToDictionary { word: String },
    ExtensionItem { ext_id: String, item_id: String },
    CopyImage,
    ToggleSpellcheck,
    ToggleLanguage { value: String },
}

fn default_cookie_path() -> String {
    "/".to_owned()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CookieSameSite {
    #[default]
    Unspecified,
    NoRestriction,
    Lax,
    Strict,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WireCookie {
    /// Leading dot for domain cookies (`.github.com`), bare host for host-only (`github.com`).
    pub host: String,
    pub name: String,
    pub value: String,
    #[serde(default = "default_cookie_path")]
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    /// Unix timestamp in seconds; `None` for session cookies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    #[serde(default)]
    pub same_site: CookieSameSite,
}
