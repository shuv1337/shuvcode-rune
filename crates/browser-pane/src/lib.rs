//! Pure-Rust Rune client for the on-demand `OpenCode Browser Engine` CEF sidecar.
//!
//! Keeps `OpenCode (Rune).app` small (~8 MB) and free of C++/CEF dependencies:
//! - Finds the installed engine, or fetches it from the build's source (`source.rs`) and checks it against
//!   the build's trust rule before anything of it runs: in the background once the app is up
//!   (`BrowserHost::fetch_in_background`, resumable, on a background-priority thread), on first
//!   use, or as an update beside a running one.
//! - Speaks line-delimited JSON over a private Unix domain socket (AF_UNIX on Windows too) for tabs,
//!   input, resize, and CDP.
//! - Receives zero-copy 2x Retina `IOSurface` frames over a Mach port pipe on macOS, and named D3D
//!   shared textures on Windows, into Rune's `NativeSurface`. On Linux the engine copies each GPU
//!   frame into dmabufs it passes over the socket (`fd_lines.rs`), and falls back to BGRA paints.
//!
//! The sidecar needs the `engine` feature. Without it, `BrowserHost` keeps its tab model and mock
//! tabs, and reports `EngineStatus::Unsupported` instead of starting an engine.

pub mod address;
#[cfg(feature = "engine")]
mod background;
mod client;
pub mod cookies;
pub mod device;
#[cfg(feature = "engine")]
mod download;
#[cfg(all(feature = "engine", target_os = "linux"))]
mod dmabuf;
#[cfg(feature = "engine")]
mod engine_log;
pub mod extensions;
#[cfg(all(feature = "engine", target_os = "linux"))]
mod fd_lines;
pub mod find;
mod frame;
pub mod history;
pub mod installer;
mod keys;
#[cfg(all(feature = "engine", target_os = "macos"))]
pub mod mach_pipe;
mod machine;
pub use browser_policy as policy;
#[cfg(feature = "engine")]
mod prefetch;
#[cfg(all(feature = "engine", windows))]
mod private_dir;
pub mod preview;
#[cfg(feature = "engine")]
pub mod profile;
pub mod prompts;
pub mod protocol;
mod scroll_cursor;
#[cfg(feature = "engine")]
mod sidecar;
mod site_zoom;
#[cfg(feature = "engine")]
mod source;
mod surface;
pub mod tools;
#[cfg(feature = "engine")]
mod transport;
#[cfg(feature = "engine")]
mod trust;
#[cfg(feature = "engine")]
mod unpack;

pub use address::{bare_url, host_of, resolve_address};
pub use client::{BrowserHost, BrowserTab, CdpEvent};
pub use extensions::{
    ExtensionLifecycleEvent, ExtensionOpenRequest, FILTER_READY_DEADLINE, FilterGateState,
    OpenExtensionPopup, POPUP_MAX_H, POPUP_MAX_W, POPUP_MIN_H, POPUP_MIN_W, PopupAnchor,
    PopupPlacement, PopupRect, PopupState, StartupExtensionSpec, popup_bounds,
};
pub use find::{FindEvent, FindState, PageDiagnostic};
pub use frame::TabSurface;
pub use installer::{Distrust, ENGINE_DOWNLOAD_MB, ENGINE_REVISION, EngineStatus, Failure, Fault, Net, PauseReason, Version};
pub use policy::{
    Refusal, allowed_destination, check_cdp, destination_origin, file_url_within, frame_guard,
    local_file_path, normalize_url, refusal, subresource_allowed, with_scheme,
};
#[cfg(feature = "engine")]
pub use profile::shared_profile_dir;
pub use cookies::{
    BrowserEngineKind, BrowserProfileInfo, BrowserSource, CookieHost, CookieRead, ImportFailure,
    ImportResult, ProfileEvent, ProfileSite, SiteList, SiteListOrder,
};
pub use protocol::CursorKind;
pub use protocol::{
    AuthChallenge, ContextKind, ContextMenuCommand, CookieSameSite, DialogKind, DownloadFile,
    DownloadSaveRequest, DownloadState, ExtensionActionSnapshot, ExtensionMenuGroup,
    ExtensionMenuItem, ExtensionMenuItemKind, PageContextMenu, PageDialog,
    PermissionKind, PermissionPrompt, PreviewKindTag, PreviewSpec, RemoteProxyConfig, WireCookie,
};
pub use preview::{PreviewFetchRequest, PreviewResponder};
pub use protocol::PickedElement;
pub use prompts::{
    BlockedGuardReason, PermissionDecision, PermissionStore, SiteDecision, SitePermissions,
    SiteSetting, TabDriver,
};
pub use site_zoom::ZoomStore;
