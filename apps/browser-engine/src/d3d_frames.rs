//! Windows GPU frames: each CEF shared texture is copied into one of a tab's few named D3D11
//! shared textures, which the app opens on its own D3D12 device by name.
//!
//! CEF's texture is only valid during `OnAcceleratedPaint`, so the copy finishes before the
//! callback returns, and so before the app hears of the frame. The app signals the tab's named
//! shared fence to a frame's id once it has read that frame; a texture is written again only
//! after the fence passes the id of the frame it holds. The app shows only the newest frame, so
//! a fence at `n` frees every texture holding a frame up to `n`. With every texture still unread
//! the frame is dropped, and the engine repaints the tab once one frees up.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, HMODULE, LUID, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_FENCE_FLAG_NONE, D3D11_FENCE_FLAG_SHARED, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
    D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11Device1,
    ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence, ID3D11Query,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter, IDXGIFactory4,
    IDXGIResource1,
};
use windows::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
use windows::core::{HSTRING, Interface};

/// Shared texture ring depth per tab: 5 absorbs high-refresh (165-240 Hz) swap-chain + IPC jitter
/// while staying well inside Rune's 16-entry opened-texture cache.
const SLOTS: usize = 5;

enum CopySync {
    /// Zero-spin GPU fence + Win32 auto-reset event (D3D11.4).
    Fence {
        context4: ID3D11DeviceContext4,
        fence: ID3D11Fence,
        event: Owned,
        value: AtomicU64,
    },
    /// Fallback event query when `ID3D11DeviceContext4` is unavailable.
    Query(ID3D11Query),
}

/// The engine's D3D11 device, on the app's GPU. COM calls on it happen on CEF's UI thread only.
pub struct Gpu {
    device: ID3D11Device5,
    context: ID3D11DeviceContext,
    sync: CopySync,
}

// SAFETY: D3D11 devices are free-threaded; the immediate context and `sync` (`context4`,
// `fence`, `event`, and fallback `query`) are used only on CEF's UI thread (`OnAcceleratedPaint`
// and the engine's message loop), never concurrently.
unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

impl Gpu {
    /// A device on the adapter with `luid` (`HighPart`, `LowPart`), else the default adapter.
    pub fn new(luid: Option<(i32, u32)>) -> Option<Arc<Self>> {
        let adapter: Option<IDXGIAdapter> = luid.and_then(|(high, low)| unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory1().ok()?;
            factory.EnumAdapterByLuid(LUID { LowPart: low, HighPart: high }).ok()
        });
        let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        unsafe {
            D3D11CreateDevice(
                adapter.as_ref(),
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .ok()?;
        let device: ID3D11Device5 = device?.cast().ok()?;
        let context = context?;
        let sync = Self::make_fence_sync(&device, &context).or_else(|| {
            let mut query = None;
            unsafe { device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut query)) }
                .ok()?;
            Some(CopySync::Query(query?))
        })?;
        Some(Arc::new(Self { device, context, sync }))
    }

    fn make_fence_sync(device: &ID3D11Device5, context: &ID3D11DeviceContext) -> Option<CopySync> {
        let context4: ID3D11DeviceContext4 = context.cast().ok()?;
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: `device` is live; `fence` receives a newly created COM interface.
        unsafe { device.CreateFence(0, D3D11_FENCE_FLAG_NONE, &mut fence) }.ok()?;
        // SAFETY: creates a private auto-reset Win32 event closed by `Owned`.
        let event = unsafe { CreateEventW(None, false, false, windows::core::PCWSTR::null()) }.ok()?;
        Some(CopySync::Fence {
            context4,
            fence: fence?,
            event: Owned(event),
            value: AtomicU64::new(0),
        })
    }

    /// Waits until the GPU has run everything submitted so far (a copy takes ~0.1 ms). False if
    /// the device was lost or it took over a second.
    fn finish(&self) -> bool {
        match &self.sync {
            CopySync::Fence { context4, fence, event, value } => {
                let next = value.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                // SAFETY: `context4`, `fence`, and `event` are owned by `self` and live on CEF's UI thread.
                unsafe {
                    if context4.Signal(fence, next).is_err() {
                        return false;
                    }
                    self.context.Flush();
                    let completed = fence.GetCompletedValue();
                    // `u64::MAX` signals device removal / TDR (ID3D11Fence::GetCompletedValue).
                    if completed == u64::MAX {
                        return false;
                    }
                    if completed >= next {
                        return true;
                    }
                    // Clear any stale signal left by a previously timed-out wait before re-arming.
                    let _ = ResetEvent(event.0);
                    if fence.SetEventOnCompletion(next, event.0).is_err() {
                        return false;
                    }
                    let deadline = Instant::now() + Duration::from_secs(1);
                    loop {
                        let completed = fence.GetCompletedValue();
                        if completed == u64::MAX {
                            return false;
                        }
                        if completed >= next {
                            return true;
                        }
                        let Some(rem) = deadline.checked_duration_since(Instant::now()) else {
                            return false;
                        };
                        if WaitForSingleObject(event.0, rem.as_millis().clamp(1, 1000) as u32) != WAIT_OBJECT_0 {
                            return false;
                        }
                    }
                }
            }
            CopySync::Query(query) => {
                unsafe {
                    self.context.End(query);
                    self.context.Flush();
                }
                let start = Instant::now();
                loop {
                    let mut done = windows::core::BOOL(0);
                    let polled = unsafe {
                        self.context.GetData(
                            query,
                            Some((&raw mut done).cast()),
                            std::mem::size_of::<windows::core::BOOL>() as u32,
                            0,
                        )
                    };
                    if polled.is_err() || start.elapsed() > Duration::from_secs(1) {
                        return false;
                    }
                    if done.as_bool() {
                        return true;
                    }
                    std::thread::yield_now();
                }
            }
        }
    }
}

/// A frame copied into a shared texture, as the app opens it.
pub struct Copied {
    pub width: u32,
    pub height: u32,
    pub texture: String,
    pub release_fence: String,
}

/// One tab's shared textures and release fence.
pub struct TabFrames {
    gpu: Arc<Gpu>,
    tag: String,
    generation: u64,
    size: (u32, u32),
    slots: Vec<Slot>,
    release: Option<Release>,
    /// A frame was dropped with every texture unread; repaint once one frees up.
    pub dropped: bool,
}

struct Slot {
    texture: ID3D11Texture2D,
    _handle: Owned,
    name: String,
    /// The frame it holds, 0 for none.
    frame: u64,
}

struct Release {
    fence: ID3D11Fence,
    _handle: Owned,
    name: String,
}

struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

// SAFETY: as for `Gpu`: only CEF's UI thread touches these, behind the tab's mutex.
unsafe impl Send for TabFrames {}

impl TabFrames {
    pub fn new(gpu: Arc<Gpu>, tab_id: u64) -> Self {
        Self {
            gpu,
            tag: format!("opencode-browser-{}-{tab_id}", std::process::id()),
            generation: 0,
            size: (0, 0),
            slots: Vec::new(),
            release: None,
            dropped: false,
        }
    }

    /// Copies CEF's shared texture `cef_handle` (valid during this paint only) into a free
    /// texture as frame `frame_id`. None when it couldn't, or every texture is still unread.
    pub fn copy(&mut self, cef_handle: HANDLE, frame_id: u64) -> Option<Copied> {
        let source: ID3D11Texture2D = unsafe { self.gpu.device.cast::<ID3D11Device1>().ok()?.OpenSharedResource1(cef_handle) }.ok()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { source.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM || desc.Width == 0 || desc.Height == 0 {
            return None;
        }
        if self.size != (desc.Width, desc.Height) || self.slots.is_empty() {
            self.resize(desc.Width, desc.Height)?;
        }
        let read = unsafe { self.release()?.fence.GetCompletedValue() };
        let Some(slot) = self.slots.iter_mut().filter(|s| s.frame <= read).min_by_key(|s| s.frame) else {
            self.dropped = true;
            return None;
        };
        unsafe { self.gpu.context.CopyResource(&slot.texture, &source) };
        if !self.gpu.finish() {
            return None;
        }
        slot.frame = frame_id;
        Some(Copied {
            width: desc.Width,
            height: desc.Height,
            texture: slot.name.clone(),
            release_fence: self.release.as_ref()?.name.clone(),
        })
    }

    /// Whether a texture is free for the next frame.
    pub fn has_free(&self) -> bool {
        let Some(release) = &self.release else { return true };
        let read = unsafe { release.fence.GetCompletedValue() };
        self.slots.iter().any(|s| s.frame <= read)
    }

    /// A new app connected: the old one reads nothing more, so every texture is free.
    pub fn forget_reads(&mut self) {
        for slot in &mut self.slots {
            slot.frame = 0;
        }
    }

    /// Drops every shared D3D11 texture while a tab is hidden to release GPU/shared memory.
    /// The next `copy()` allocates a fresh generation so named shared handles never collide.
    pub fn release_slots(&mut self) {
        self.slots.clear();
        self.size = (0, 0);
        self.dropped = false;
    }

    fn resize(&mut self, width: u32, height: u32) -> Option<()> {
        self.slots.clear();
        self.generation += 1;
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
        };
        for i in 0..SLOTS {
            let mut texture = None;
            unsafe { self.gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) }.ok()?;
            let texture: ID3D11Texture2D = texture?;
            let name = format!("{}-{}-{i}", self.tag, self.generation);
            let handle = unsafe {
                texture.cast::<IDXGIResource1>().ok()?.CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                    &HSTRING::from(name.as_str()),
                )
            }
            .ok()?;
            self.slots.push(Slot { texture, _handle: Owned(handle), name, frame: 0 });
        }
        self.size = (width, height);
        Some(())
    }

    fn release(&mut self) -> Option<&Release> {
        if self.release.is_none() {
            let mut fence: Option<ID3D11Fence> = None;
            unsafe { self.gpu.device.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence) }.ok()?;
            let fence = fence?;
            let name = format!("{}-read", self.tag);
            let handle = unsafe { fence.CreateSharedHandle(None, GENERIC_ALL.0, &HSTRING::from(name.as_str())) }.ok()?;
            self.release = Some(Release { fence, _handle: Owned(handle), name });
        }
        self.release.as_ref()
    }
}
