//! CEF display, load, DevTools observer, and Mach `IOSurface` frame sender.

#[cfg(target_os = "macos")]
use std::ffi::{CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use crate::output::{Output, Paint};
use std::sync::{Arc, Mutex};

use cef::rc::Rc as _;
use cef::{
    AcceleratedPaintInfo, AuthCallback, Browser, Callback, CefString, CefStringList, CursorInfo,
    CursorType, DisplayHandler, Errorcode, Frame, ImplAuthCallback, ImplBrowser, ImplBrowserHost,
    ImplDisplayHandler, ImplFrame, ImplLoadHandler, ImplRenderHandler, ImplRequest,
    ImplRequestHandler, ImplResourceRequestHandler, ImplSelectClientCertificateCallback,
    LoadHandler, PaintElementType, Rect, RenderHandler, Request, RequestHandler,
    ResourceRequestHandler, Response, ReturnValue, ScreenInfo, SelectClientCertificateCallback,
    TerminationStatus, TransitionType, WindowOpenDisposition, WrapDisplayHandler, WrapLoadHandler,
    WrapRenderHandler, WrapRequestHandler, WrapResourceRequestHandler, X509Certificate, sys,
    wrap_display_handler, wrap_load_handler, wrap_render_handler, wrap_request_handler,
    wrap_resource_request_handler,
};

use crate::wire::{CursorKind, EngineMsg, MOD_ALT, MOD_CMD, MOD_CTRL, MOD_SHIFT, TabSnapshot};

#[cfg(target_os = "macos")]
mod mach_tx {
    use super::*;

    type MachPort = u32;
    type KernReturn = i32;

    const MACH_MSGH_BITS_COMPLEX: u32 = 0x8000_0000;
    const MACH_MSG_TYPE_MOVE_SEND: u8 = 17;
    const MACH_MSG_TYPE_COPY_SEND: u32 = 19;
    const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;
    const MACH_SEND_MSG: i32 = 0x0000_0001;
    const TASK_BOOTSTRAP_PORT: i32 = 4;

    unsafe extern "C" {
        fn mach_task_self() -> MachPort;
        fn task_get_special_port(task: MachPort, which: i32, port: *mut MachPort) -> KernReturn;
        fn mach_port_deallocate(task: MachPort, name: MachPort) -> KernReturn;
        fn bootstrap_look_up(bp: MachPort, service_name: *const c_char, sp: *mut MachPort) -> KernReturn;
        fn mach_msg(
            msg: *mut MachMsgHeader,
            option: i32,
            send_size: u32,
            rcv_size: u32,
            rcv_name: MachPort,
            timeout: u32,
            notify: MachPort,
        ) -> KernReturn;
    }

    #[link(name = "IOSurface", kind = "framework")]
    unsafe extern "C" {
        fn IOSurfaceCreateMachPort(buffer: *const c_void) -> MachPort;
    }

    #[repr(C)]
    #[derive(Copy, Clone, Default)]
    struct MachMsgHeader {
        msgh_bits: u32,
        msgh_size: u32,
        msgh_remote_port: MachPort,
        msgh_local_port: MachPort,
        msgh_voucher_port: MachPort,
        msgh_id: i32,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Default)]
    struct MachMsgBody {
        msgh_descriptor_count: u32,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Default)]
    struct MachMsgPortDescriptor {
        name: MachPort,
        pad1: u32,
        pad2: u16,
        disposition: u8,
        type_: u8,
    }

    #[repr(C)]
    #[derive(Copy, Clone, Default)]
    struct FrameSendMsg {
        header: MachMsgHeader,
        body: MachMsgBody,
        port_desc: MachMsgPortDescriptor,
        tab_id: u64,
        frame_id: u64,
        width: u32,
        height: u32,
    }

    pub fn lookup(name: &str) -> u32 {
        let Ok(c_name) = CString::new(name) else { return 0 };
        let mut bp: MachPort = 0;
        if unsafe { task_get_special_port(mach_task_self(), TASK_BOOTSTRAP_PORT, &mut bp) } != 0 {
            return 0;
        }
        let mut port: MachPort = 0;
        if unsafe { bootstrap_look_up(bp, c_name.as_ptr(), &mut port) } == 0 {
            port
        } else {
            0
        }
    }

    pub unsafe fn send_surface(
        remote_port: u32,
        tab_id: u64,
        frame_id: u64,
        width: u32,
        height: u32,
        raw: *const c_void,
    ) -> bool {
        if remote_port == 0 || raw.is_null() {
            return false;
        }
        let surface_port = unsafe { IOSurfaceCreateMachPort(raw) };
        if surface_port == 0 {
            return false;
        }
        let mut msg = FrameSendMsg {
            header: MachMsgHeader {
                msgh_bits: MACH_MSGH_BITS_COMPLEX | MACH_MSG_TYPE_COPY_SEND,
                msgh_size: std::mem::size_of::<FrameSendMsg>() as u32,
                msgh_remote_port: remote_port,
                msgh_local_port: 0,
                msgh_voucher_port: 0,
                msgh_id: 0x4345_4632,
            },
            body: MachMsgBody { msgh_descriptor_count: 1 },
            port_desc: MachMsgPortDescriptor {
                name: surface_port,
                pad1: 0,
                pad2: 0,
                disposition: MACH_MSG_TYPE_MOVE_SEND,
                type_: MACH_MSG_PORT_DESCRIPTOR,
            },
            tab_id,
            frame_id,
            width,
            height,
        };
        let kr = unsafe {
            mach_msg(&mut msg.header, MACH_SEND_MSG, msg.header.msgh_size, 0, 0, 0, 0)
        };
        if kr != 0 {
            unsafe {
                let _ = mach_port_deallocate(mach_task_self(), surface_port);
            }
            false
        } else {
            true
        }
    }
}

#[cfg(target_os = "macos")]
pub use mach_tx::lookup as lookup_mach_port;

/// Windows: the tab's D3D11 frame ring; None when the engine has no D3D11 device (CPU frames).
#[cfg(windows)]
pub type FrameRing = Option<Arc<Mutex<crate::d3d_frames::TabFrames>>>;
/// Linux: the tab's dmabufs; None when frames come as BGRA paints.
#[cfg(target_os = "linux")]
pub type FrameRing = Option<Arc<Mutex<crate::dmabuf_frames::TabFrames>>>;
#[cfg(target_os = "macos")]
pub type FrameRing = ();

/// Linux: where a tab reports frames that can't come the way they do.
#[cfg(target_os = "linux")]
pub type Failures = std::sync::mpsc::Sender<crate::Event>;
#[cfg(not(target_os = "linux"))]
pub type Failures = ();

/// The native cursor CEF hands `OnCursorChange` (unused: the app draws its own cursor).
#[cfg(windows)]
type NativeCursor = sys::HCURSOR;
#[cfg(target_os = "linux")]
type NativeCursor = sys::cef_cursor_handle_t;
#[cfg(target_os = "macos")]
type NativeCursor = *mut u8;

#[derive(Clone, Debug)]
pub struct ViewRectState {
    pub width_pt: f32,
    pub height_pt: f32,
    pub scale: f32,
}

wrap_render_handler! {
    pub struct TabRenderHandler {
        tab_id: u64,
        view_state: Arc<Mutex<ViewRectState>>,
        mach_port: Arc<AtomicU32>,
        out_tx: Output,
        frame_seq: Arc<AtomicU64>,
        awake: Arc<AtomicBool>,
        frames: FrameRing,
        failures: Failures,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let v = self.view_state.lock().unwrap();
            rect.x = 0;
            rect.y = 0;
            rect.width = v.width_pt.round().max(1.0) as i32;
            rect.height = v.height_pt.round().max(1.0) as i32;
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> i32 {
            let Some(info) = info else { return 0 };
            let v = self.view_state.lock().unwrap();
            let r = Rect {
                x: 0,
                y: 0,
                width: v.width_pt.round().max(1.0) as i32,
                height: v.height_pt.round().max(1.0) as i32,
            };
            info.device_scale_factor = v.scale;
            info.depth = 32;
            info.depth_per_component = 8;
            info.is_monochrome = 0;
            info.rect = r.clone();
            info.available_rect = r;
            1
        }

        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            if type_ != PaintElementType::default() || !self.awake.load(Ordering::Acquire) {
                return;
            }
            let Some(info) = info else { return };
            let id = self.frame_seq.fetch_add(1, Ordering::Relaxed) + 1;
            #[cfg(target_os = "macos")]
            {
                let port = self.mach_port.load(Ordering::Relaxed);
                unsafe {
                    let _ = mach_tx::send_surface(
                        port,
                        self.tab_id,
                        id,
                        info.extra.coded_size.width as u32,
                        info.extra.coded_size.height as u32,
                        info.shared_texture_io_surface,
                    );
                }
            }
            #[cfg(windows)]
            if let Some(ring) = &self.frames {
                let handle = windows::Win32::Foundation::HANDLE(info.shared_texture_handle);
                let copied = ring.lock().unwrap().copy(handle, id);
                if let Some(f) = copied {
                    let _ = self.out_tx.send(EngineMsg::SharedFrame {
                        tab_id: self.tab_id,
                        frame_id: id,
                        width: f.width,
                        height: f.height,
                        texture: f.texture,
                        release_fence: f.release_fence,
                    });
                }
            }
            #[cfg(target_os = "linux")]
            if let Some(ring) = &self.frames {
                let effects = ring.lock().unwrap().paint(info, id);
                let host = _browser.and_then(|b| cef::ImplBrowser::host(b));
                let failures = self.failures.clone();
                crate::dmabuf_frames::run(effects, self.tab_id, &self.out_tx, host.as_ref(), &move |f| {
                    let _ = failures.send(crate::Event::FramesFailed(f));
                });
            }
            #[cfg(not(target_os = "linux"))]
            let _ = &self.failures;
            #[cfg(target_os = "linux")]
            let _ = &self.mach_port;
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty: Option<&[Rect]>,
            buffer: *const u8,
            width: i32,
            height: i32,
        ) {
            if type_ != PaintElementType::default()
                || !self.awake.load(Ordering::Acquire)
                || buffer.is_null()
                || width <= 0
                || height <= 0
            {
                return;
            }
            let id = self.frame_seq.fetch_add(1, Ordering::Relaxed) + 1;
            let len = (width as usize) * (height as usize) * 4;
            let slice = unsafe { std::slice::from_raw_parts(buffer, len) };
            // OnPaint owns the full buffer only for this call. A replacement must contain all
            // pixels: the app may never have seen the paints preceding this dirty rectangle.
            self.out_tx.paint(self.tab_id, Paint {
                frame_id: id,
                width: width as u32,
                height: height as u32,
                bgra: slice.to_vec(),
            });
        }
    }
}

fn emit_web_nav(
    events: &std::sync::mpsc::Sender<crate::Event>,
    tab_id: u64,
    frame: Option<&Frame>,
    url: String,
    kind: crate::extensions::web_nav::WebNavKind,
) {
    let Some(f) = frame else { return };
    let is_main = f.is_main() == 1;
    let cef_frame_id = CefString::from(&f.identifier()).to_string();
    let parent_cef_id = (!is_main)
        .then(|| f.parent().map(|p| CefString::from(&p.identifier()).to_string()))
        .flatten();
    let _ = events.send(crate::Event::WebNav(crate::extensions::web_nav::WebNavSignal {
        tab_id,
        cef_frame_id,
        parent_cef_id,
        is_main,
        url,
        kind,
    }));
}

wrap_display_handler! {
    pub struct TabDisplayHandler {
        tab_id: u64,
        preview: Option<Arc<crate::preview::PreviewGrant>>,
        snap: Arc<Mutex<TabSnapshot>>,
        prompts: Arc<crate::prompts::TabPrompts>,
        permissions: Arc<crate::permissions::PermissionTracker>,
        context_scope_id: Option<String>,
        favicon: Arc<crate::favicon::TabFavicon>,
        out_tx: Output,
        events: std::sync::mpsc::Sender<crate::Event>,
    }

    impl DisplayHandler {
        fn on_address_change(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            url: Option<&CefString>,
        ) {
            if let Some(f) = frame.as_deref()
                && f.is_main() != 1
            {
                if let Some(u) = url {
                    emit_web_nav(&self.events, self.tab_id, Some(f), u.to_string(), crate::extensions::web_nav::WebNavKind::AddressChanged);
                }
                return;
            }
            if let Some(url) = url {
                let raw = url.to_string();
                let url = self.preview.as_ref().map_or(raw.clone(), |g| g.sanitize_reported_url(&raw));
                // Guard 2 (commit-side eviction): run BEFORE locking `self.snap` so `stop_load()` /
                // `load_url("about:blank")` can never re-enter `on_loading_state_change` under lock,
                // and `snap.url` never reports the refused granted URL.
                if self.context_scope_id.is_none()
                    && self.preview.is_none()
                    && self.prompts.agent_driven.load(Ordering::Acquire)
                    && self.permissions.has_inherited_default_grant(&url)
                {
                    if let Some(b) = browser.as_deref() {
                        b.stop_load();
                        if let Some(f) = b.main_frame() {
                            f.load_url(Some(&CefString::from("about:blank")));
                        }
                    }
                    let _ = self.out_tx.send(EngineMsg::ControlledPermissionRefused {
                        tab_id: self.tab_id,
                        url: Some(url),
                    });
                    return;
                }
                let mut s = self.snap.lock().unwrap();
                // Chromium's error page commits the failed address; another address is another document.
                if s.url != url {
                    let old_origin = crate::prompts::origin_of(&s.url);
                    let new_origin = crate::prompts::origin_of(&url);
                    let origin_changed = old_origin != new_origin;
                    s.load_error = None;
                    self.favicon.on_navigation(&mut s, origin_changed);
                    self.prompts.clear_auth_on_nav(self.tab_id, &self.out_tx);
                    if origin_changed {
                        self.prompts.clear_permissions_on_origin_change(
                            self.tab_id,
                            new_origin.as_deref(),
                            &self.out_tx,
                        );
                        self.permissions.on_origin_changed(
                            self.context_scope_id.as_deref(),
                            self.tab_id,
                            new_origin.as_deref(),
                        );
                    }
                    if self.context_scope_id.is_some()
                        && let Some(orig) = new_origin.as_deref()
                        && let Some(ctx) = browser.as_ref().and_then(|b| b.host()).and_then(|h| h.request_context())
                    {
                        crate::permissions::block_device_permission_guards_for_origin(&ctx, orig);
                    }
                }
                self.prompts.set_pending_top_url(None);
                s.url = url.clone();
                let _ = self.out_tx.send(EngineMsg::TabUpdate { tab: s.clone() });
                let _ = self.events.send(crate::Event::TabMeta {
                    tab_id: self.tab_id,
                    url: Some(url.clone()),
                    title: None,
                    loading: None,
                });
                emit_web_nav(&self.events, self.tab_id, frame.as_deref(), url, crate::extensions::web_nav::WebNavKind::AddressChanged);
            }
        }

        fn on_favicon_urlchange(
            &self,
            browser: Option<&mut Browser>,
            icon_urls: Option<&mut CefStringList>,
        ) {
            let (Some(browser), Some(list)) = (browser, icon_urls) else { return };
            let candidates = crate::menu::read_cef_string_list(list);
            self.favicon.fetch_candidates(
                browser,
                candidates,
                self.preview.as_ref(),
                self.snap.clone(),
                self.out_tx.clone(),
            );
        }

        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            if let Some(title) = title {
                let mut s = self.snap.lock().unwrap();
                // Chromium's error page names itself by host alone; the failed page keeps its address's name.
                if s.load_error.as_deref().is_some_and(|e| e != "page_crashed") {
                    return;
                }
                let raw = title.to_string();
                // Never leak the internal `oc-preview://<uuid>` origin when a preview document has no <title>.
                let display = if browser_policy::preview::is_preview_scheme(&raw) {
                    s.url.clone()
                } else {
                    raw
                };
                s.title = display.clone();
                let _ = self.out_tx.send(EngineMsg::TabUpdate { tab: s.clone() });
                let _ = self.events.send(crate::Event::TabMeta {
                    tab_id: self.tab_id,
                    url: None,
                    title: Some(display),
                    loading: None,
                });
            }
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: NativeCursor,
            type_: CursorType,
            _custom: Option<&CursorInfo>,
        ) -> i32 {
            use sys::cef_cursor_type_t as C;
            let kind = match *type_.as_ref() {
                C::CT_HAND => CursorKind::Pointer,
                C::CT_IBEAM | C::CT_VERTICALTEXT => CursorKind::Text,
                C::CT_CROSS => CursorKind::Crosshair,
                C::CT_COLUMNRESIZE | C::CT_EASTWESTRESIZE | C::CT_EASTRESIZE | C::CT_WESTRESIZE => {
                    CursorKind::ResizeColumn
                }
                C::CT_ROWRESIZE | C::CT_NORTHSOUTHRESIZE | C::CT_NORTHRESIZE | C::CT_SOUTHRESIZE => {
                    CursorKind::ResizeRow
                }
                C::CT_GRAB => CursorKind::Grab,
                C::CT_GRABBING => CursorKind::Grabbing,
                C::CT_NOTALLOWED => CursorKind::NotAllowed,
                C::CT_MOVE => CursorKind::Move,
                _ => CursorKind::Default,
            };
            let mut s = self.snap.lock().unwrap();
            if s.cursor != kind {
                s.cursor = kind;
                let _ = self.out_tx.send(EngineMsg::TabUpdate { tab: s.clone() });
            }
            1
        }
    }
}

wrap_load_handler! {
    pub struct TabLoadHandler {
        tab_id: u64,
        file_roots: Arc<Mutex<Vec<String>>>,
        preview: Option<Arc<crate::preview::PreviewGrant>>,
        snap: Arc<Mutex<TabSnapshot>>,
        prompts: Arc<crate::prompts::TabPrompts>,
        diagnostics: Arc<crate::diagnostics::TabDiagnostics>,
        deferred_initial: Arc<AtomicBool>,
        out_tx: Output,
        events: std::sync::mpsc::Sender<crate::Event>,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: i32,
            can_go_back: i32,
            can_go_forward: i32,
        ) {
            let loading = is_loading != 0;
            if !loading {
                self.prompts.set_pending_top_url(None);
            }
            let mut s = self.snap.lock().unwrap();
            let waiting_deferred = !loading
                && s.url.eq_ignore_ascii_case("about:blank")
                && self.deferred_initial.load(Ordering::Acquire);
            let effective_loading = loading || waiting_deferred;
            s.loading = effective_loading;
            s.can_go_back = can_go_back != 0;
            s.can_go_forward = can_go_forward != 0;
            let _ = self.out_tx.send(EngineMsg::TabUpdate { tab: s.clone() });
            let _ = self.events.send(crate::Event::TabMeta {
                tab_id: self.tab_id,
                url: None,
                title: None,
                loading: Some(effective_loading),
            });
        }

        fn on_load_start(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _transition_type: TransitionType,
        ) {
            let Some(frame) = frame else { return };
            let url = CefString::from(&frame.url()).to_string();
            emit_web_nav(&self.events, self.tab_id, Some(frame), url.clone(), crate::extensions::web_nav::WebNavKind::Committed);
            if frame.is_main() == 1 {
                self.prompts.clear_all_ext_frame_tracking();
                let mut s = self.snap.lock().unwrap();
                s.document += 1;
                let _ = self.out_tx.send(EngineMsg::TabUpdate { tab: s.clone() });
            } else if crate::extensions::manifest::is_extension_scheme(&url) {
                let fid = CefString::from(&frame.identifier()).to_string();
                self.prompts.mark_pending_ext_subframe(fid);
            }
        }

        // A document from the network (any HTTP status) replaces a failed or crashed page; Chromium's
        // own error page ends with no status.
        fn on_load_end(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, http_status_code: i32) {
            if let Some(f) = frame.as_deref() {
                let url = CefString::from(&f.url()).to_string();
                emit_web_nav(&self.events, self.tab_id, Some(f), url.clone(), crate::extensions::web_nav::WebNavKind::Completed);
                if f.is_main() != 1 {
                    if crate::extensions::manifest::is_extension_scheme(&url) {
                        let fid = CefString::from(&f.identifier()).to_string();
                        self.prompts.clear_pending_ext_subframe(&fid);
                    }
                    return;
                }
            }
            self.prompts.set_pending_top_url(None);
            if http_status_code <= 0 {
                return;
            }
            let mut s = self.snap.lock().unwrap();
            if s.load_error.take().is_some() {
                let _ = self.out_tx.send(EngineMsg::TabUpdate { tab: s.clone() });
            }
        }

        // A main-frame load that failed, as Desktop's did-fail-load: a cancelled one (a newer
        // navigation, a download) keeps the page.
        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            use sys::cef_errorcode_t as E;
            let code = *error_code.as_ref();
            let err_desc = error_text.map(CefString::to_string).unwrap_or_default();
            if code != E::ERR_ABORTED {
                let fail_u = failed_url.map(CefString::to_string).unwrap_or_default();
                emit_web_nav(
                    &self.events,
                    self.tab_id,
                    frame.as_deref(),
                    fail_u,
                    crate::extensions::web_nav::WebNavKind::ErrorOccurred { error: err_desc.clone() },
                );
            }
            if let Some(f) = frame
                && f.is_main() != 1
            {
                if failed_url
                    .map(CefString::to_string)
                    .is_some_and(|u| crate::extensions::manifest::is_extension_scheme(&u))
                {
                    let fid = CefString::from(&f.identifier()).to_string();
                    self.prompts.clear_pending_ext_subframe(&fid);
                }
                return;
            }
            self.prompts.set_pending_top_url(None);
            if code == E::ERR_ABORTED {
                return;
            }
            // Chromium blocks unsafe cross-scheme redirects (`302 -> file://` or `302 -> data:`)
            // and disallowed schemes in the network layer before `on_before_browse`: keep the
            // open page and report the policy refusal, matching Desktop's `will-redirect`.
            // Skip for a preview tab's own canceled `oc-preview://<grant>` URL (e.g. artifact 404).
            let own_preview_fail = self.preview.as_ref().is_some_and(|g| {
                failed_url
                    .map(CefString::to_string)
                    .is_some_and(|u| g.compute_display_url(&u).is_some())
            });
            if !own_preview_fail
                && matches!(code, E::ERR_UNSAFE_REDIRECT | E::ERR_DISALLOWED_URL_SCHEME | E::ERR_UNKNOWN_URL_SCHEME)
            {
                let roots = self.file_roots.lock().unwrap().clone();
                let why = if roots.is_empty() {
                    browser_policy::Refusal::Web
                } else {
                    browser_policy::Refusal::Workspace
                };
                let _ = self.out_tx.send(EngineMsg::Refused {
                    tab_id: self.tab_id,
                    reason: why.code().to_owned(),
                });
                return;
            }
            let mut s = self.snap.lock().unwrap();
            if let Some(raw) = failed_url.map(CefString::to_string).filter(|u| !u.is_empty()) {
                let url = self.preview.as_ref().map_or(raw.clone(), |g| g.sanitize_reported_url(&raw));
                // As Desktop's tab names a failed page: its host and port.
                let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
                s.title = rest.split(['/', '?', '#']).next().unwrap_or(rest).to_owned();
                s.url = url;
            }
            let text = error_text.map(CefString::to_string).unwrap_or_default();
            let err_code = *error_code.as_ref() as i32;
            let url_for_log = s.url.clone();
            let loading_now = s.loading;
            s.load_error = Some(if text.is_empty() { format!("net error {err_code}") } else { text.clone() });
            let _ = self.out_tx.send(EngineMsg::TabUpdate { tab: s.clone() });
            drop(s);
            self.diagnostics.report_load_failed(
                self.tab_id,
                err_code,
                &text,
                &url_for_log,
                loading_now,
                self.prompts.agent_driven.load(std::sync::atomic::Ordering::Relaxed),
                &self.out_tx,
            );
        }
    }
}

wrap_resource_request_handler! {
    pub struct TabResourceRequestHandler {
        file_roots: Arc<Mutex<Vec<String>>>,
        context_ready: Arc<AtomicBool>,
        prompts: Arc<crate::prompts::TabPrompts>,
        is_main_navigation: bool,
        is_subframe_navigation: bool,
        isolated: bool,
    }

    impl ResourceRequestHandler {
        // Sub-resources (`img`, `script`, `link`, `fetch`, `XHR`) may never read `file:` URLs
        // outside the tab's allowed workspace roots (`pane.ts:103-107`), and no non-`about:blank`
        // request may run before an isolated context finishes preference verification (or while suspended).
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            let url = request.map(|r| CefString::from(&r.url()).to_string()).unwrap_or_default();
            if !self.context_ready.load(Ordering::Acquire) {
                return if url.trim().eq_ignore_ascii_case("about:blank") {
                    ReturnValue::CONTINUE
                } else {
                    ReturnValue::CANCEL
                };
            }
            let roots = self.file_roots.lock().unwrap().clone();
            if browser_policy::subresource_allowed(&url, &roots) {
                ReturnValue::CONTINUE
            } else {
                ReturnValue::CANCEL
            }
        }

        fn on_resource_redirect(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _response: Option<&mut Response>,
            new_url: Option<&mut CefString>,
        ) {
            let Some(new_url) = new_url else { return };
            if !self.context_ready.load(Ordering::Acquire) {
                *new_url = CefString::from("");
                return;
            }
            let target = new_url.to_string();
            if crate::extensions::manifest::is_pdf_extension_url(&target) {
                *new_url = CefString::from("");
                return;
            }
            // A redirected navigation lands on a new origin: its device and file-system guards are
            // locked there before it commits, as `on_before_browse` does (READER-7 N9/N10).
            if self.isolated
                && (self.is_main_navigation || self.is_subframe_navigation)
                && let Some(ctx) = browser.as_ref().and_then(|b| b.host()).and_then(|h| h.request_context())
            {
                crate::permissions::block_device_permission_guards_for_origin(&ctx, &target);
            }
            let roots = self.file_roots.lock().unwrap().clone();
            if !browser_policy::subresource_allowed(&target, &roots) {
                *new_url = CefString::from("");
            } else if self.is_main_navigation {
                self.prompts.set_pending_top_url(Some(target));
            } else if self.is_subframe_navigation
                && crate::extensions::manifest::is_extension_scheme(&target)
                && let Some(f) = frame
            {
                // Track strictly sub-frame navigations (`is_navigation && !is_main`), never
                // ordinary `<script>`/`<img>` subresource redirects inside web iframes
                // (`review-rd274.md` Round-4 §4).
                let fid = CefString::from(&f.identifier()).to_string();
                self.prompts.mark_pending_ext_subframe(fid);
            }
        }

        // Never launch external OS protocol handlers (`mailto:`, `ms-calculator:`, ...) from CEF.
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

/// Extension & isolation navigation policy attached to a managed tab's `TabRequestHandler`.
#[derive(Clone)]
pub struct TabExtPolicy {
    pub allowed_ext_ids: Arc<Mutex<Vec<String>>>,
    pub popup_ext_id: Option<String>,
    pub isolated: bool,
}

wrap_request_handler! {
    pub struct TabRequestHandler {
        tab_id: u64,
        file_roots: Arc<Mutex<Vec<String>>>,
        ext_policy: TabExtPolicy,
        preview: Option<Arc<crate::preview::PreviewGrant>>,
        context_ready: Arc<AtomicBool>,
        snap: Arc<Mutex<TabSnapshot>>,
        prompt_shared: crate::prompts::PromptShared,
        prompts: Arc<crate::prompts::TabPrompts>,
        input_guard: Arc<Mutex<crate::input_guard::InputGuard>>,
        emulation_scale: Arc<std::sync::atomic::AtomicU32>,
        next_cdp_id: Arc<std::sync::atomic::AtomicI32>,
        permissions: Arc<crate::permissions::PermissionTracker>,
        context_scope_id: Option<String>,
        diagnostics: Arc<crate::diagnostics::TabDiagnostics>,
        out_tx: Output,
        events: std::sync::mpsc::Sender<crate::Event>,
    }

    impl RequestHandler {
        // Navigation and redirect guard (`chromium.ts:432-446`): main-frame navigations require
        // `about:blank`, credential-free `http:`/`https:`, a `file:` URL inside `file_roots`, or
        // (for non-isolated, non-agent user/popup tabs only) a loaded `chrome-extension://<id>/`
        // page; preview tabs stay strictly confined to their own `oc-preview://<grant_host>/`
        // origin; popup main frames stay strictly on their own `chrome-extension://<id>/` origin;
        // isolated tabs refuse `chrome-extension:` in every frame; sub-frames additionally forbid
        // `file:` URLs outside `file_roots`.
        fn on_before_browse(
            &self,
            browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let (is_main, sub_frame_id) = match frame.as_deref() {
                Some(f) if f.is_main() != 1 => (false, Some(CefString::from(&f.identifier()).to_string())),
                _ => (true, None),
            };
            let (url, referrer) = request
                .map(|r| (CefString::from(&r.url()).to_string(), CefString::from(&r.referrer_url()).to_string()))
                .unwrap_or_default();
            // Fail closed before `on_request_context_initialized` finishes verifying prefs
            // (and while a remote context is suspended): cancel every non-`about:blank` navigation.
            if !self.context_ready.load(Ordering::Acquire) {
                return i32::from(!url.trim().eq_ignore_ascii_case("about:blank"));
            }
            if is_main
                && self.context_scope_id.is_none()
                && self.preview.is_none()
                && self.prompts.agent_driven.load(Ordering::Acquire)
                && self.permissions.has_inherited_default_grant(&url)
            {
                let _ = self.out_tx.send(EngineMsg::ControlledPermissionRefused {
                    tab_id: self.tab_id,
                    url: Some(url),
                });
                return 1;
            }
            if self.context_scope_id.is_some()
                && let Some(ctx) = browser.as_ref().and_then(|b| b.host()).and_then(|h| h.request_context())
            {
                crate::permissions::block_device_permission_guards_for_origin(&ctx, &url);
            }
            if let Some(grant) = &self.preview {
                if is_main {
                    if grant.allows_main_navigation(&url) {
                        self.prompts.set_pending_top_url(Some(url));
                        return 0;
                    }
                    if user_gesture != 0
                        && url.len() <= crate::prompts::MAX_POPUP_URL_BYTES
                        && browser_policy::destination_origin(&url).is_some()
                    {
                        let _ = self.out_tx.send(EngineMsg::PopupBlocked { tab_id: self.tab_id, url });
                    } else {
                        let _ = self.out_tx.send(EngineMsg::Refused {
                            tab_id: self.tab_id,
                            reason: browser_policy::Refusal::Workspace.code().to_owned(),
                        });
                    }
                    return 1;
                }
                return i32::from(!grant.allows_subframe_navigation(&url));
            }
            let is_agent = self.prompts.agent_driven.load(Ordering::Relaxed);
            // Chromium's PDF viewer, as official's Electron shows it: only as the frame Chromium puts
            // in a PDF document (official lets the viewer's own frames through, `chromium.ts:963`).
            // A page can't frame it (none of its pages is web-accessible) and no main frame goes there.
            if crate::extensions::manifest::is_pdf_extension_url(&url) {
                if !is_main {
                    return 0;
                }
                let _ = self.out_tx.send(EngineMsg::Refused {
                    tab_id: self.tab_id,
                    reason: browser_policy::Refusal::Blocked.code().to_owned(),
                });
                return 1;
            }
            // Block extension-origin URLs (`chrome-extension:`, `blob:chrome-extension:`, ...) in
            // EVERY frame (main AND sub-frames) on isolated tabs and agent-driven tabs (`review-rd274.md` §1).
            if (self.ext_policy.isolated || is_agent)
                && crate::extensions::manifest::is_extension_scheme(&url)
            {
                let _ = self.out_tx.send(EngineMsg::Refused {
                    tab_id: self.tab_id,
                    reason: browser_policy::Refusal::Blocked.code().to_owned(),
                });
                return 1;
            }
            // Block non-user extension-initiated navigations on agent-driven tabs (READER-12 C2).
            if is_agent
                && user_gesture == 0
                && !referrer.is_empty()
                && crate::extensions::manifest::is_extension_scheme(&referrer)
            {
                return 1;
            }
            if is_main && let Some(own_id) = &self.ext_policy.popup_ext_id {
                let target = crate::extensions::manifest::extension_id_from_url(&url);
                if !is_agent
                    && target == Some(own_id.as_str())
                    && self.ext_policy.allowed_ext_ids.lock().unwrap().iter().any(|id| id == own_id)
                {
                    self.prompts.set_pending_top_url(Some(url));
                    return 0;
                }
                let _ = self.out_tx.send(EngineMsg::Refused {
                    tab_id: self.tab_id,
                    reason: browser_policy::Refusal::Blocked.code().to_owned(),
                });
                return 1;
            }
            if is_main
                && !is_agent
                && let Some(ext_id) = crate::extensions::manifest::extension_id_from_url(&url)
                && self.ext_policy.allowed_ext_ids.lock().unwrap().iter().any(|id| id == ext_id)
            {
                self.prompts.set_pending_top_url(Some(url));
                return 0;
            }
            let roots = self.file_roots.lock().unwrap().clone();
            if let Some(why) = browser_policy::frame_guard(&url, is_main, &roots) {
                if is_main
                    && user_gesture != 0
                    && !is_agent
                    && !self.ext_policy.isolated
                    && self.ext_policy.popup_ext_id.is_none()
                    && browser_policy::classify_app_link(&url, browser_policy::AppSchemeSource::Page).is_some()
                {
                    let _ = self.out_tx.send(EngineMsg::AppLinkBlocked { tab_id: self.tab_id, url });
                } else {
                    let _ = self.out_tx.send(EngineMsg::Refused {
                        tab_id: self.tab_id,
                        reason: why.code().to_owned(),
                    });
                }
                return 1;
            }
            if is_main {
                self.prompts.set_pending_top_url(Some(url.clone()));
            } else if crate::extensions::manifest::is_extension_scheme(&url)
                && let Some(fid) = sub_frame_id
            {
                self.prompts.mark_pending_ext_subframe(fid);
            }
            emit_web_nav(
                &self.events,
                self.tab_id,
                frame.as_deref(),
                url,
                crate::extensions::web_nav::WebNavKind::BeforeNavigate,
            );
            0
        }

        fn on_open_urlfrom_tab(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            target_url: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            user_gesture: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            if !self.context_ready.load(Ordering::Acquire) {
                return 1;
            }
            let url = target_url.map(CefString::to_string).unwrap_or_default();
            // With the PDF viewer off, Chromium swaps a PDF in a sub-frame for its own placeholder, a
            // `data:` page of about 30 kB, and opens it through here without a gesture; CEF would load
            // it in the main frame, over the page. It stays cancelled, but silently: no top-level
            // page was asked for, so the tab gets no refusal (official refuses only what a main frame
            // navigates to, `chromium.ts:975`, `:991-992`). A `data:` page opened with a gesture is
            // still refused below; one the top frame navigates to, in `on_before_browse`.
            if user_gesture == 0
                && *target_disposition.as_ref() == sys::cef_window_open_disposition_t::CEF_WOD_CURRENT_TAB
                && url.trim_start().get(..5).is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
            {
                return 1;
            }
            if self.preview.is_some() {
                if user_gesture != 0
                    && url.len() <= crate::prompts::MAX_POPUP_URL_BYTES
                    && browser_policy::destination_origin(&url).is_some()
                {
                    let _ = self.out_tx.send(EngineMsg::PopupBlocked { tab_id: self.tab_id, url });
                } else {
                    let _ = self.out_tx.send(EngineMsg::Refused {
                        tab_id: self.tab_id,
                        reason: browser_policy::Refusal::Workspace.code().to_owned(),
                    });
                }
                return 1;
            }
            let is_agent = self.prompts.agent_driven.load(Ordering::Relaxed);
            if crate::extensions::manifest::is_pdf_extension_url(&url) {
                let _ = self.out_tx.send(EngineMsg::Refused {
                    tab_id: self.tab_id,
                    reason: browser_policy::Refusal::Blocked.code().to_owned(),
                });
                return 1;
            }
            if (self.ext_policy.isolated || is_agent)
                && crate::extensions::manifest::is_extension_scheme(&url)
            {
                let _ = self.out_tx.send(EngineMsg::Refused {
                    tab_id: self.tab_id,
                    reason: browser_policy::Refusal::Blocked.code().to_owned(),
                });
                return 1;
            }
            use sys::cef_window_open_disposition_t as D;
            let opens_new_tab = matches!(
                *target_disposition.as_ref(),
                D::CEF_WOD_NEW_BACKGROUND_TAB
                    | D::CEF_WOD_NEW_FOREGROUND_TAB
                    | D::CEF_WOD_NEW_POPUP
                    | D::CEF_WOD_NEW_WINDOW
            );
            if let Some(own_id) = &self.ext_policy.popup_ext_id {
                let target = crate::extensions::manifest::extension_id_from_url(&url);
                if !is_agent
                    && target == Some(own_id.as_str())
                    && self.ext_policy.allowed_ext_ids.lock().unwrap().iter().any(|id| id == own_id)
                {
                    if opens_new_tab {
                        if url.len() > crate::prompts::MAX_POPUP_URL_BYTES
                            || user_gesture == 0
                            || !self.prompts.popup_credit.swap(false, Ordering::AcqRel)
                        {
                            return 1;
                        }
                        let background = *target_disposition.as_ref() == D::CEF_WOD_NEW_BACKGROUND_TAB;
                        let _ = self.out_tx.send(EngineMsg::PopupOpen {
                            tab_id: self.tab_id,
                            url,
                            background,
                        });
                        return 1;
                    }
                    return 0;
                }
                // Links opened in a new tab from a popup (`target="_blank"`) require a user
                // gesture, one-shot `popup_credit`, and `MAX_POPUP_URL_BYTES` (`popup.ts:169-180`;
                // READER-12 #441 Should-Fix #2).
                if opens_new_tab
                    && url.len() <= crate::prompts::MAX_POPUP_URL_BYTES
                    && user_gesture != 0
                    && browser_policy::destination_origin(&url).is_some()
                    && self.prompts.popup_credit.swap(false, Ordering::AcqRel)
                {
                    let background = *target_disposition.as_ref() == D::CEF_WOD_NEW_BACKGROUND_TAB;
                    let _ = self.out_tx.send(EngineMsg::PopupOpen {
                        tab_id: self.tab_id,
                        url,
                        background,
                    });
                    return 1;
                }
                let _ = self.out_tx.send(EngineMsg::Refused {
                    tab_id: self.tab_id,
                    reason: browser_policy::Refusal::Blocked.code().to_owned(),
                });
                return 1;
            }
            if !is_agent
                && let Some(ext_id) = crate::extensions::manifest::extension_id_from_url(&url)
                && self.ext_policy.allowed_ext_ids.lock().unwrap().iter().any(|id| id == ext_id)
            {
                if opens_new_tab {
                    if url.len() > crate::prompts::MAX_POPUP_URL_BYTES
                        || user_gesture == 0
                        || !self.prompts.popup_credit.swap(false, Ordering::AcqRel)
                    {
                        return 1;
                    }
                    let background = *target_disposition.as_ref() == D::CEF_WOD_NEW_BACKGROUND_TAB;
                    let _ = self.out_tx.send(EngineMsg::PopupOpen {
                        tab_id: self.tab_id,
                        url,
                        background,
                    });
                    return 1;
                }
                return 0;
            }
            let roots = self.file_roots.lock().unwrap().clone();
            if url.len() > crate::prompts::MAX_POPUP_URL_BYTES {
                let _ = self.out_tx.send(EngineMsg::Refused {
                    tab_id: self.tab_id,
                    reason: browser_policy::Refusal::Web.code().to_owned(),
                });
                return 1;
            }
            if let Some(why) = browser_policy::frame_guard(&url, true, &roots) {
                if user_gesture != 0
                    && !is_agent
                    && !self.ext_policy.isolated
                    && self.ext_policy.popup_ext_id.is_none()
                    && browser_policy::classify_app_link(&url, browser_policy::AppSchemeSource::Page).is_some()
                {
                    let _ = self.out_tx.send(EngineMsg::AppLinkBlocked { tab_id: self.tab_id, url });
                } else {
                    let _ = self.out_tx.send(EngineMsg::Refused {
                        tab_id: self.tab_id,
                        reason: why.code().to_owned(),
                    });
                }
                return 1;
            }
            if opens_new_tab {
                if user_gesture != 0 && self.prompts.popup_credit.swap(false, Ordering::AcqRel) {
                    let background = *target_disposition.as_ref() == D::CEF_WOD_NEW_BACKGROUND_TAB;
                    let _ = self.out_tx.send(EngineMsg::PopupOpen {
                        tab_id: self.tab_id,
                        url,
                        background,
                    });
                } else {
                    // Reject synthetic modifier-click bypasses (e.g. `new MouseEvent('click', { ctrlKey: true })`)
                    // without a real user gesture by converting to the blocked-popup bar (`review-rd-engine.md`).
                    let _ = self.out_tx.send(EngineMsg::PopupBlocked {
                        tab_id: self.tab_id,
                        url,
                    });
                }
                return 1;
            }
            if matches!(
                *target_disposition.as_ref(),
                D::CEF_WOD_OFF_THE_RECORD | D::CEF_WOD_NEW_PICTURE_IN_PICTURE
            ) {
                return 1;
            }
            0
        }

        fn auth_credentials(
            &self,
            _browser: Option<&mut Browser>,
            origin_url: Option<&CefString>,
            is_proxy: ::std::os::raw::c_int,
            host: Option<&CefString>,
            port: ::std::os::raw::c_int,
            realm: Option<&CefString>,
            scheme: Option<&CefString>,
            callback: Option<&mut AuthCallback>,
        ) -> ::std::os::raw::c_int {
            if self.preview.is_some() {
                if let Some(cb) = callback {
                    cb.cancel();
                }
                return 1;
            }
            crate::prompts::handle_auth_credentials(
                self.tab_id,
                &self.prompt_shared,
                &self.prompts,
                &self.snap,
                &self.out_tx,
                origin_url,
                is_proxy,
                host,
                port,
                realm,
                scheme,
                callback,
            )
        }

        // Client certificate requests are always cancelled so no client cert is ever sent (`#63002`).
        fn on_select_client_certificate(
            &self,
            _browser: Option<&mut Browser>,
            _is_proxy: ::std::os::raw::c_int,
            _host: Option<&CefString>,
            _port: ::std::os::raw::c_int,
            _certificates: Option<&[Option<X509Certificate>]>,
            callback: Option<&mut SelectClientCertificateCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(cb) = callback {
                cb.select(None);
            }
            1
        }

        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            if let Some(grant) = &self.preview {
                return crate::preview::context::handle_resource_request(grant, frame, request, is_navigation, request_initiator);
            }
            let is_main = frame.is_none_or(|f| f.is_main() == 1);
            let is_main_navigation = is_navigation != 0 && is_main;
            let is_subframe_navigation = is_navigation != 0 && !is_main;
            Some(TabResourceRequestHandler::new(
                self.file_roots.clone(),
                self.context_ready.clone(),
                self.prompts.clone(),
                is_main_navigation,
                is_subframe_navigation,
                self.context_scope_id.is_some(),
            ))
        }

        // The page's renderer went away (crashed, killed, out of memory): the tab keeps its address,
        // and a reload loads it in a new renderer, as Desktop's crashed tab does.
        fn on_render_process_terminated(
            &self,
            browser: Option<&mut Browser>,
            status: TerminationStatus,
            error_code: i32,
            error_string: Option<&CefString>,
        ) {
            self.prompts.reset_on_crash(self.tab_id, &self.out_tx);
            if self.prompt_shared.release_trace(self.tab_id)
                && let Some(host) = browser.and_then(|b| b.host())
            {
                let id = self.next_cdp_id.fetch_add(1, Ordering::Relaxed);
                let msg = serde_json::json!({ "id": id, "method": "Tracing.end", "params": {} }).to_string();
                host.send_dev_tools_message(Some(msg.as_bytes()));
            }
            self.emulation_scale.store(1.0f32.to_bits(), Ordering::Release);
            self.input_guard
                .lock()
                .unwrap()
                .reset_all(self.tab_id, &self.out_tx, "The page crashed before input completed.");
            let err_str = error_string.map(CefString::to_string).unwrap_or_default();
            let (url, was_loading) = {
                let mut s = self.snap.lock().unwrap();
                let was_loading = s.loading;
                s.loading = false;
                s.load_error = Some("page_crashed".into());
                let _ = self.out_tx.send(EngineMsg::TabUpdate { tab: s.clone() });
                (s.url.clone(), was_loading)
            };
            self.diagnostics.report_renderer_terminated(
                self.tab_id,
                status,
                error_code,
                &err_str,
                &url,
                was_loading,
                self.prompts.agent_driven.load(std::sync::atomic::Ordering::Relaxed),
                &self.out_tx,
            );
        }
    }
}

pub use crate::cdp_ledger::{CdpPending, PendingClear, TabCdpObserver};

/// An event flag as the `u32` CEF's event structs take (the flag enum is `i32` on Windows).
#[allow(clippy::unnecessary_cast)]
pub fn event_flag(flag: sys::cef_event_flags_t) -> u32 {
    flag.0 as u32
}

pub fn cef_mods(bits: u8) -> u32 {
    let mut flags = 0u32;
    if bits & MOD_SHIFT != 0 {
        flags |= event_flag(sys::cef_event_flags_t::EVENTFLAG_SHIFT_DOWN);
    }
    if bits & MOD_CTRL != 0 {
        flags |= event_flag(sys::cef_event_flags_t::EVENTFLAG_CONTROL_DOWN);
    }
    if bits & MOD_ALT != 0 {
        flags |= event_flag(sys::cef_event_flags_t::EVENTFLAG_ALT_DOWN);
    }
    if bits & MOD_CMD != 0 {
        flags |= event_flag(sys::cef_event_flags_t::EVENTFLAG_COMMAND_DOWN);
    }
    flags
}

/// The wheel delta CEF takes for the app's scroll in points: points elsewhere, but on Windows
/// `WHEEL_DELTA` (120) units that CEF turns into the system's lines (or characters, sideways) of
/// 100/3 px each. Converted so the page scrolls by the points the app reports, as on macOS.
#[cfg(not(windows))]
pub fn wheel_delta(dx: f32, dy: f32) -> (f32, f32) {
    (dx, dy)
}

#[cfg(windows)]
pub fn wheel_delta(dx: f32, dy: f32) -> (f32, f32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETWHEELSCROLLCHARS, SPI_GETWHEELSCROLLLINES, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    let setting = |action| {
        let mut value = 3u32;
        let read = unsafe {
            SystemParametersInfoW(action, 0, Some((&raw mut value).cast()), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0))
        };
        // WHEEL_PAGESCROLL (u32::MAX) pages instead of lines; keep the default then.
        if read.is_ok() && (1..1000).contains(&value) { value as f32 } else { 3.0 }
    };
    let units = |px: f32, action| {
        if px == 0.0 {
            0.0
        } else {
            px * 120.0 / (setting(action) * 100.0 / 3.0)
        }
    };
    (units(dx, SPI_GETWHEELSCROLLCHARS), units(dy, SPI_GETWHEELSCROLLLINES))
}

/// The key's `native_key_code`: a macOS virtual key code, or on Windows the `WM_KEYDOWN`
/// `lParam` with the key's scan code, which gives the page its `KeyboardEvent.code`.
#[cfg(target_os = "macos")]
pub fn native_key_code(_vk: i32, mac: i32) -> i32 {
    mac
}

/// Chromium's Linux keyboard events use XKB keycodes (evdev + 8), including under Ozone.
/// The wire carries logical keys; resolve the US-layout physical key for known keys, and
/// leave the physical code unspecified for text outside that layout.
#[cfg(target_os = "linux")]
pub fn native_key_code(vk: i32, _mac: i32) -> i32 {
    for (row, first) in [(b"1234567890".as_slice(), 10), (b"QWERTYUIOP", 24), (b"ASDFGHJKL", 38), (b"ZXCVBNM", 52)] {
        if let Some(offset) = row.iter().position(|&key| i32::from(key) == vk) {
            return first + offset as i32;
        }
    }
    match vk {
        0x1b => 9, 0xbd => 20, 0xbb => 21, 0x08 => 22, 0x09 => 23,
        0xdb => 34, 0xdd => 35, 0x0d => 36, 0xba => 47, 0xde => 48, 0xc0 => 49,
        0xdc => 51, 0xbc => 59, 0xbe => 60, 0xbf => 61, 0x20 => 65,
        0x24 => 110, 0x26 => 111, 0x21 => 112, 0x25 => 113, 0x27 => 114,
        0x23 => 115, 0x28 => 116, 0x22 => 117, 0x2e => 119,
        0x70..=0x79 => 67 + (vk - 0x70), 0x7a => 95, 0x7b => 96,
        _ => 0,
    }
}

#[cfg(windows)]
pub fn native_key_code(vk: i32, _mac: i32) -> i32 {
    use windows::Win32::UI::Input::KeyboardAndMouse::{MAPVK_VK_TO_VSC_EX, MapVirtualKeyW};
    let scan = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC_EX) };
    // Extended keys (arrows, Home, Delete…) come back as 0xE0nn; lParam bit 24 marks them.
    let extended = u32::from(scan & 0xFF00 == 0xE000) << 24;
    (((scan & 0xFF) << 16) | extended | 1) as i32
}

pub fn ascii_vk(ch: char) -> i32 {
    match ch {
        'a'..='z' | 'A'..='Z' => ch.to_ascii_uppercase() as i32,
        '0'..='9' => ch as i32,
        ' ' => 0x20, '-' | '_' => 0xBD, '=' | '+' => 0xBB, '[' | '{' => 0xDB,
        ']' | '}' => 0xDD, '\\' | '|' => 0xDC, ';' | ':' => 0xBA, '\'' | '"' => 0xDE,
        ',' | '<' => 0xBC, '.' | '>' => 0xBE, '/' | '?' => 0xBF, '`' | '~' => 0xC0,
        _ => ch as i32,
    }
}

pub fn mac_key_code(ch: char) -> i32 {
    match ch.to_ascii_lowercase() {
        'a' => 0, 's' => 1, 'd' => 2, 'f' => 3, 'h' => 4, 'g' => 5, 'z' => 6, 'x' => 7,
        'c' => 8, 'v' => 9, 'b' => 11, 'q' => 12, 'w' => 13, 'e' => 14, 'r' => 15,
        'y' => 16, 't' => 17, '1' | '!' => 18, '2' | '@' => 19, '3' | '#' => 20,
        '4' | '$' => 21, '6' | '^' => 22, '5' | '%' => 23, '=' | '+' => 24, '9' | '(' => 25,
        '7' | '&' => 26, '-' | '_' => 27, '8' | '*' => 28, '0' | ')' => 29, ']' | '}' => 30,
        'o' => 31, 'u' => 32, '[' | '{' => 33, 'i' => 34, 'p' => 35, 'l' => 37, 'j' => 38,
        '\'' | '"' => 39, 'k' => 40, ';' | ':' => 41, '\\' | '|' => 42, ',' | '<' => 43,
        '/' | '?' => 44, 'n' => 45, 'm' => 46, '.' | '>' => 47, ' ' => 49, '`' | '~' => 50,
        _ => 0,
    }
}

pub fn named_codes(name: &str) -> Option<(i32, i32, Option<u16>)> {
    match name {
        "Enter" => Some((0x0D, 36, Some('\r' as u16))),
        "Backspace" => Some((0x08, 51, None)),
        "Delete" => Some((0x2E, 117, None)),
        "Tab" => Some((0x09, 48, None)),
        "Escape" => Some((0x1B, 53, None)),
        "ArrowLeft" => Some((0x25, 123, None)),
        "ArrowRight" => Some((0x27, 124, None)),
        "ArrowDown" => Some((0x28, 125, None)),
        "ArrowUp" => Some((0x26, 126, None)),
        "Home" => Some((0x24, 115, None)),
        "End" => Some((0x23, 119, None)),
        "PageUp" => Some((0x21, 116, None)),
        "PageDown" => Some((0x22, 121, None)),
        // Windows virtual-key codes 0x70..0x7B; macOS key codes (`kVK_F1`..`kVK_F12`).
        "F1" => Some((0x70, 122, None)),
        "F2" => Some((0x71, 120, None)),
        "F3" => Some((0x72, 99, None)),
        "F4" => Some((0x73, 118, None)),
        "F5" => Some((0x74, 96, None)),
        "F6" => Some((0x75, 97, None)),
        "F7" => Some((0x76, 98, None)),
        "F8" => Some((0x77, 100, None)),
        "F9" => Some((0x78, 101, None)),
        "F10" => Some((0x79, 109, None)),
        "F11" => Some((0x7A, 103, None)),
        "F12" => Some((0x7B, 111, None)),
        _ => None,
    }
}
