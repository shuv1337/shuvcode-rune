//! Line-delimited JSON wire protocol between `crates/browser-pane` and `opencode-browser-engine`.

use rune::{Cursor, KeyPlatform, Mods, PointerButton};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MOD_SHIFT: u8 = 1 << 0;
pub const MOD_CTRL: u8 = 1 << 1;
pub const MOD_ALT: u8 = 1 << 2;
pub const MOD_CMD: u8 = 1 << 3;

/// The keys held, for the engine: Control, Alt, Shift and Meta (⌘, or the Windows key). `cmd` is
/// the primary key reported beside its own flag (`ctrl` off macOS, `logo` on it), so it adds
/// nothing unless an event names it alone (a script), where it stands for the primary key.
pub fn pack_mods(m: Mods) -> u8 {
    let mac = KeyPlatform::current() == KeyPlatform::Mac;
    let scripted = m.cmd && !m.ctrl && !m.logo;
    let mut bits = 0;
    if m.shift {
        bits |= MOD_SHIFT;
    }
    if m.ctrl || (scripted && !mac) {
        bits |= MOD_CTRL;
    }
    if m.alt {
        bits |= MOD_ALT;
    }
    if m.logo || (scripted && mac) {
        bits |= MOD_CMD;
    }
    bits
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerBtn {
    #[default]
    Primary,
    Secondary,
    Middle,
}

impl From<PointerButton> for PointerBtn {
    fn from(b: PointerButton) -> Self {
        match b {
            PointerButton::Secondary => Self::Secondary,
            PointerButton::Middle => Self::Middle,
            _ => Self::Primary,
        }
    }
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

impl CursorKind {
    pub fn to_rune(self) -> Cursor {
        match self {
            Self::Default => Cursor::Default,
            Self::Pointer => Cursor::Pointer,
            Self::Text => Cursor::Text,
            Self::Crosshair => Cursor::Crosshair,
            Self::ResizeColumn => Cursor::ResizeColumn,
            Self::ResizeRow => Cursor::ResizeRow,
            Self::Grab => Cursor::Grab,
            Self::Grabbing => Cursor::Grabbing,
            Self::NotAllowed => Cursor::NotAllowed,
            Self::Move => Cursor::Move,
        }
    }
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
    /// `page_crashed` when its renderer went away. An engine from before it sends none.
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
    /// Whether this tab is agent-driven (preserved across reconnects).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agent_driven: bool,
    /// Counts main-frame document loads (`on_load_start`); unchanged by same-document URL changes.
    #[serde(default)]
    pub document: u64,
    /// Whether this tab is currently producing audible audio (reserved for Phase 2 hibernation protection).
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

/// An element chosen with the visual element picker (`Overlay.setInspectMode`), ready to insert
/// as an `@eN` reference in the session composer.
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

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMsg {
    Hello {
        mach_service: Option<String>,
        pid: u32,
        /// This app shows and answers page dialogs, permissions, and HTTP auth (`prompts.rs`).
        #[serde(default)]
        prompts: bool,
        /// This app handles extension-initiated tab opens (`EngineMsg::ExtensionOpenTab`).
        #[serde(default)]
        extensions: bool,
        /// This app handles `EngineMsg::DownloadSaveRequest` via a native OS Save dialog.
        #[serde(default)]
        download_save: bool,
        /// This app manages multi-pane tab visibility via `SetVisibleTabs` and `SetTabUnthrottled`.
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        file_roots: Vec<String>,
        #[serde(default, skip_serializing_if = "ContextKind::is_default")]
        context: ContextKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        preview: Option<PreviewSpec>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        agent_driven: bool,
        /// Allows navigating to loaded `chrome-extension://` pages (user tabs & popups only;
        /// never set for agent-driven or isolated tabs).
        #[serde(default, skip_serializing_if = "is_false")]
        allow_extension_pages: bool,
        /// True when this tab is an anchored extension popup (enables content auto-sizing).
        #[serde(default, skip_serializing_if = "is_false")]
        is_popup: bool,
        /// When answering `EngineMsg::ExtensionOpenTab`, correlates this newly opened pane tab.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ext_open_request_id: Option<u64>,
    },
    ClickExtension {
        ext_id: String,
    },
    ListExtensions {
        request_id: u64,
    },
    UninstallExtension {
        ext_id: String,
    },
    ReloadExtension {
        ext_id: String,
    },
    CancelExtensionOpenTab {
        request_id: u64,
    },
    SetTabPolicy {
        tab_id: u64,
        file_roots: Vec<String>,
    },
    CloseTab {
        tab_id: u64,
    },
    FocusTab {
        tab_id: u64,
    },
    /// Exact set of currently visible pane tabs (`vec![]` when panel closed, on Home/Settings, or hidden).
    SetVisibleTabs {
        tab_ids: Vec<u64>,
    },
    /// Explicit agent RPC unthrottle hold, decoupled from security-sticky `agent_driven`.
    SetTabUnthrottled {
        tab_id: u64,
        active: bool,
    },
    Navigate {
        tab_id: u64,
        url: String,
    },
    Back {
        tab_id: u64,
    },
    Forward {
        tab_id: u64,
    },
    Reload {
        tab_id: u64,
        #[serde(default)]
        ignore_cache: bool,
    },
    Stop {
        tab_id: u64,
    },
    SetZoom {
        tab_id: u64,
        factor: f64,
    },
    /// Pages draw in the app's scheme (their `prefers-color-scheme`, scrollbars, blank page), as
    /// Desktop's follow `nativeTheme.themeSource`. An engine from before it ignores the line.
    SetColorScheme {
        dark: bool,
    },
    /// The window's display refresh rate in Hz (clamped by the engine to 60..=240). An engine
    /// from before it ignores the line.
    SetFrameRate {
        fps: u32,
    },
    Resize {
        tab_id: u64,
        width_pt: f32,
        height_pt: f32,
        scale: f32,
    },
    Emulate { tab_id: u64, device: Option<crate::device::Device>, scale: f32 },
    MouseMove {
        tab_id: u64,
        x: f32,
        y: f32,
        mods: u8,
        leave: bool,
    },
    MouseButton {
        tab_id: u64,
        x: f32,
        y: f32,
        button: PointerBtn,
        up: bool,
        clicks: u32,
        mods: u8,
    },
    Wheel {
        tab_id: u64,
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
        mods: u8,
    },
    Key {
        tab_id: u64,
        down: bool,
        text: Option<String>,
        named: Option<String>,
        mods: u8,
    },
    EditCmd {
        tab_id: u64,
        cmd: String,
    },
    Ime {
        tab_id: u64,
        commit: Option<String>,
        preedit: Option<String>,
    },
    SetInspect {
        tab_id: u64,
        enabled: bool,
    },
    HighlightNode {
        tab_id: u64,
        backend_node_id: Option<i64>,
    },
    CdpCall {
        tab_id: u64,
        request_id: u64,
        method: String,
        params: Value,
        session_id: Option<String>,
    },
    SiteCookies {
        tab_id: u64,
        request_id: u64,
    },
    ClearSite {
        tab_id: u64,
        request_id: u64,
    },
    /// Linux: this app's GPU has finished reading `frame_id` (and every frame before it) of
    /// buffer set `pool`.
    FrameRead {
        tab_id: u64,
        pool: u64,
        frame_id: u64,
    },
    /// Linux: this app can't import the engine's dmabufs; it paints BGRA from now on.
    DmabufFailed {
        reason: String,
    },
    AnswerDialog {
        tab_id: u64,
        id: u64,
        accept: bool,
        text: String,
    },
    AnswerPermission {
        tab_id: u64,
        id: u64,
        allow: bool,
        #[serde(default)]
        dismiss: bool,
        #[serde(default)]
        persist: bool,
    },
    ResetPermission {
        origin: String,
        kind: PermissionKind,
    },
    SyncSitePermissions {
        entries: Vec<SitePermissionEntry>,
    },
    AnswerAuth {
        tab_id: u64,
        id: u64,
        username: Option<String>,
        password: Option<String>,
    },
    AnswerDownloadSave {
        tab_id: u64,
        id: u64,
        path: Option<String>,
    },
    SetAgentDriven {
        tab_id: u64,
        agent_driven: bool,
    },
    ClearSiteCache {
        tab_id: u64,
        request_id: u64,
    },
    Find {
        tab_id: u64,
        query: String,
        forward: bool,
        match_case: bool,
        find_next: bool,
        #[serde(default)]
        user_initiated: bool,
    },
    StopFind {
        tab_id: u64,
        clear_selection: bool,
    },
    ContextMenuAction {
        tab_id: u64,
        id: u64,
        action: Option<ContextMenuCommand>,
    },
    SetPartitionProxy {
        #[serde(alias = "partition")]
        server_id: String,
        proxy: Option<RemoteProxyConfig>,
    },
    DropContext {
        context: ContextKind,
    },
    ProfileCookiesList {
        request_id: u64,
        #[serde(default)]
        partition: Option<String>,
    },
    ProfileCookiesSet {
        request_id: u64,
        #[serde(default)]
        partition: Option<String>,
        cookies: Vec<WireCookie>,
    },
    ProfileCookieDelete {
        request_id: u64,
        #[serde(default)]
        partition: Option<String>,
        url: String,
        name: String,
    },
    ProfileProbeCookies {
        request_id: u64,
    },
    /// `sites`: origins the engine checks after the clear (an older engine ignores them).
    ClearProfileData {
        request_id: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sites: Vec<String>,
    },
    SetShareSignIns {
        enabled: bool,
    },
    SyncRemoteCookies {
        request_id: u64,
        partition: String,
        #[serde(default)]
        from_remote: bool,
    },
    TypedCdpCall {
        tab_id: u64,
        request_id: u64,
        method: String,
        params: Value,
        session_id: Option<String>,
        /// The frame an element ref came from; the engine checks it belongs to `session_id`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame_id: Option<String>,
    },
    UploadFiles {
        tab_id: u64,
        request_id: u64,
        backend_node_id: i64,
        files: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame_id: Option<String>,
    },
    DropFiles {
        tab_id: u64,
        request_id: u64,
        x: f64,
        y: f64,
        files: Vec<String>,
    },
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
    PreviewResponseChunk {
        req_id: u64,
        data_b64: String,
    },
    /// Marks EOF (`ok: true`) or failure (`ok: false`) for a pending `EngineMsg::PreviewFetch`.
    PreviewResponseEnd {
        req_id: u64,
        ok: bool,
    },
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
    Ready {
        chromium_major: u32,
        pid: u32,
        /// The engine keeps a page's downloads (`Download`); an engine from before it says nothing.
        #[serde(default)]
        downloads: bool,
        /// The engine enforces URL and workspace `file://` policy in its request handlers.
        #[serde(default)]
        policy: bool,
        /// The engine holds JS dialogs, permissions, and HTTP auth challenges for the app.
        #[serde(default)]
        prompts: bool,
        /// The engine manages pop-ups (`PopupOpen` / `PopupBlocked`) via `LifeSpanHandler`.
        #[serde(default)]
        popups: bool,
        /// The engine supports in-page `FindHandler`.
        #[serde(default)]
        find: bool,
        /// The engine routes page right-click context menus to the app.
        #[serde(default)]
        context_menu: bool,
        /// The engine isolates `Preview`, `Artifact`, `Private`, and `RemoteServer` tabs in dedicated `CefRequestContext`s.
        #[serde(default)]
        partitions: bool,
        /// The engine confines proxied `RemoteServer` partitions via `fixed_servers` + `<-loopback>` + WebRTC/DoH hardening.
        #[serde(default)]
        remote_proxy: bool,
        /// The engine supports `TypedCdpCall` and flattened OOPIF `sessionId` CDP events.
        #[serde(default)]
        agent_tools: bool,
        /// The engine serves isolated `oc-preview://` tabs.
        #[serde(default)]
        preview: bool,
        /// The engine resets Chromium's `HostContentSettingsMap` on one-time/revoked permissions
        /// and supports `HostMsg::ResetPermission` + `AnswerPermission.dismiss`.
        #[serde(default)]
        permission_sync: bool,
        /// The engine emits `EngineMsg::PreviewCredit` for end-to-end preview flow control.
        #[serde(default)]
        preview_credit: bool,
        /// The engine hosts verified MV3 extensions over its private in-process CDP pipe.
        #[serde(default)]
        extensions: bool,
        /// The engine owns `Target.setAutoAttach` and frame exclusion authority (`EngineCdpLedger`).
        #[serde(default)]
        frame_authority: bool,
        /// The engine stages agent files for confined `UploadFiles` and `DropFiles`.
        #[serde(default)]
        agent_files: bool,
        /// The engine exposes `CefCookieManager` list/import/delete/probe/clear & two-way remote cookie mirroring.
        #[serde(default)]
        cookies: bool,
        /// The engine enforces the WebAuthn / passkey gate and emits `PasskeyRefused`.
        #[serde(default)]
        passkeys: bool,
        /// The engine enforces committed-main-frame renderer PID trace isolation and a single engine-wide trace lock.
        #[serde(default)]
        trace_isolation: bool,
        /// The engine supports Copy image, spellcheck toggle, and spellcheck languages in page menus.
        #[serde(default)]
        menu_extras: bool,
        /// The engine supports `SetVisibleTabs` and `SetTabUnthrottled` tab visibility lifecycle.
        #[serde(default)]
        tab_lifecycle: bool,
    },
    ProfileCookies {
        request_id: u64,
        cookies: Vec<WireCookie>,
    },
    ProfileCookiesImported {
        request_id: u64,
        imported: u32,
        failed: u32,
    },
    ProfileProbeReady {
        request_id: u64,
        written: bool,
        cache_dir: String,
    },
    ProfileActionDone {
        request_id: u64,
        ok: bool,
    },
    /// Requests bytes for a remote workspace or artifact preview from the app host.
    PreviewFetch {
        req_id: u64,
        tab_id: u64,
        kind: PreviewKindTag,
        path: String,
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        range: Option<String>,
    },
    /// Byte-credit replenishment (or immediate cancellation) for an in-flight `PreviewFetch`.
    PreviewCredit {
        req_id: u64,
        bytes: u32,
        #[serde(default)]
        cancel: bool,
    },
    /// A preview page attempted to load a file outside its grant or a secret/hidden path.
    PreviewDenied {
        tab_id: u64,
    },
    /// A frame navigation or redirect was refused by URL policy while keeping the current page.
    Refused {
        tab_id: u64,
        reason: String,
    },
    /// Controlled `Default`-profile tab refused due to an inherited site permission; carries the
    /// refused URL for the notice bar and "Open in new tab" without overwriting `BrowserTab.url`.
    ControlledPermissionRefused {
        tab_id: u64,
        #[serde(default)]
        url: Option<String>,
    },
    Dialog {
        tab_id: u64,
        dialog: PageDialog,
    },
    DialogsReset {
        tab_id: u64,
    },
    PermissionRequest {
        tab_id: u64,
        request: PermissionPrompt,
    },
    PermissionDismissed {
        tab_id: u64,
        id: u64,
    },
    AuthRequest {
        tab_id: u64,
        challenge: AuthChallenge,
    },
    AuthReset {
        tab_id: u64,
    },
    AuthAgentRequired {
        tab_id: u64,
        origin: String,
    },
    PopupOpen {
        tab_id: u64,
        url: String,
        background: bool,
    },
    PopupBlocked {
        tab_id: u64,
        url: String,
    },
    /// A user-activated non-agent page click on an allow-listed app scheme was cancelled in CEF
    /// and forwarded to the desktop review bar.
    AppLinkBlocked {
        tab_id: u64,
        url: String,
    },
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
    FileChooserBlocked {
        tab_id: u64,
    },
    /// The person's Copy image: the image under the menu as PNG (base64), for the app's clipboard.
    CopiedImage {
        tab_id: u64,
        png_b64: String,
    },
    /// A tab whose first navigation became the person's download, now placed (or cancelled).
    DownloadOnlyTab {
        tab_id: u64,
    },
    FindResult {
        tab_id: u64,
        count: u32,
        active_match: u32,
        final_update: bool,
    },
    ContextMenu {
        tab_id: u64,
        menu: PageContextMenu,
    },
    PageDiagnostic {
        tab_id: u64,
        level: String,
        event: String,
        fields: Value,
    },
    TabList {
        tabs: Vec<TabSnapshot>,
    },
    TabUpdate {
        tab: TabSnapshot,
    },
    TabClosed {
        tab_id: u64,
    },
    InspectState {
        tab_id: u64,
        active: bool,
        picked: Option<PickedElement>,
    },
    CdpResponse {
        tab_id: u64,
        request_id: u64,
        ok: bool,
        result: Value,
    },
    CdpEvent {
        tab_id: u64,
        method: String,
        params: Value,
        session_id: Option<String>,
    },
    CpuFrame {
        tab_id: u64,
        frame_id: u64,
        width: u32,
        height: u32,
        bgra_b64: String,
    },
    /// Windows: a finished BGRA frame in the named D3D11 shared texture. The app signals the
    /// named shared fence to `frame_id` once it has read it (`rune::native_surface::windows`).
    SharedFrame {
        tab_id: u64,
        frame_id: u64,
        width: u32,
        height: u32,
        texture: String,
        release_fence: String,
    },
    /// A download started, moved on or ended (agent-driven tabs only). An engine from before it sends none.
    Download {
        tab_id: u64,
        file: DownloadFile,
    },
    /// A human-initiated download (or right-click "Save image as…") awaiting a native OS Save
    /// dialog path outside the engine profile and `opencode-page-downloads`.
    DownloadSaveRequest {
        tab_id: u64,
        id: u64,
        name: String,
        mime: String,
        #[serde(default)]
        executable: bool,
    },
    /// Linux: a tab's new linear ARGB8888 dmabufs, their descriptors sent with the line
    /// (`SCM_RIGHTS`, `fd_lines.rs`), one per buffer.
    DmabufPool {
        tab_id: u64,
        pool: u64,
        buffers: u32,
        width: u32,
        height: u32,
        stride: u32,
        offset: u32,
        modifier: u64,
    },
    /// Linux: a finished frame in buffer `slot` of `pool` (`dmabuf.rs`).
    DmabufFrame {
        tab_id: u64,
        frame_id: u64,
        pool: u64,
        slot: u32,
    },
    ExtensionsState {
        request_id: Option<u64>,
        extensions: Vec<ExtensionActionSnapshot>,
    },
    ExtensionAction {
        action: ExtensionActionSnapshot,
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
    ExtensionNotificationShow {
        ext_id: String,
        ext_name: String,
        notif_id: String,
        body: String,
    },
    ExtensionNotificationClear {
        ext_id: String,
        notif_id: String,
    },
}

/// Live toolbar button snapshot for one loaded extension (`apps/browser-engine/src/extensions/mod.rs`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionActionSnapshot {
    pub ext_id: String,
    pub name: String,
    pub version: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub popup: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    #[serde(default)]
    pub badge_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge_color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge_text_color: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// A human download awaiting an OS Save dialog choice in the host app (`EngineMsg::DownloadSaveRequest`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DownloadSaveRequest {
    pub tab_id: u64,
    pub id: u64,
    pub name: String,
    pub mime: String,
    pub executable: bool,
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

impl DialogKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Alert => "alert",
            Self::Confirm => "confirm",
            Self::Prompt => "prompt",
            Self::Leave => "leave",
        }
    }
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

impl PermissionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Camera => "camera",
            Self::Microphone => "microphone",
            Self::Location => "location",
            Self::Notifications => "notifications",
            Self::ClipboardRead => "clipboard-read",
        }
    }
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
    /// Current profile spellcheck toggle state (populated when `is_editable && Ready.menu_extras`).
    #[serde(default)]
    pub spellcheck: bool,
    /// Enabled spellcheck BCP-47 dictionary tags (populated when `is_editable && Ready.menu_extras`).
    #[serde(default)]
    pub languages: Vec<String>,
    /// Offerable spellcheck BCP-47 dictionary tags (empty on macOS, as official).
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

    pub fn is_supported(&self, partitions: bool, remote_proxy: bool) -> bool {
        match self {
            Self::Default => true,
            Self::Preview { .. } | Self::Artifact { .. } | Self::Private => partitions,
            // Remote servers ALWAYS require an authenticated loopback tunnel proxy (never direct).
            Self::RemoteServer { .. } => partitions && remote_proxy,
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
    CopyImage,
    SaveImage { url: String },
    ReplaceMisspelling { word: String },
    AddToDictionary { word: String },
    ToggleSpellcheck,
    ToggleLanguage { value: String },
    ExtensionItem { ext_id: String, item_id: String },
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
