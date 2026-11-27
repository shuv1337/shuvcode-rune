//! Off-screen frame rate pacing for browser tabs: matches the window's display refresh rate
//! (60..=240 Hz, queried from the OS primary display at startup and updated via
//! `HostMsg::SetFrameRate`) so scrolling runs at native high-refresh rates (120/144/165/240 Hz)
//! instead of a hardcoded 60 FPS cap.
//!
//! While an agent's `Input.*` calls arrive, the tab runs at least at [`AGENT_INPUT_FPS`], and
//! returns to the display's pane rate [`IDLE_AFTER`] after the last one.
//! State lives on the engine's main thread, where every `TypedCdpCall` and `poll_frames` runs.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

use cef::{BrowserHost, ImplBrowserHost};

pub const MIN_PANE_FPS: i32 = 60;
pub const MAX_PANE_FPS: i32 = 240;
pub const AGENT_INPUT_FPS: i32 = 240;
const IDLE_AFTER: Duration = Duration::from_secs(1);

/// Current display-matched pane FPS (0 = uninitialized, seeded lazily from the OS display).
static PANE_FPS: AtomicI32 = AtomicI32::new(0);

thread_local! {
    static BOOSTED: RefCell<HashMap<u64, (Instant, BrowserHost)>> = RefCell::new(HashMap::new());
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn query_default_fps() -> i32 {
    use windows::Win32::Graphics::Gdi::{DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW};
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: plain Win32 query filling our owned `DEVMODEW` with its size initialized.
    let ok = unsafe { EnumDisplaySettingsW(windows::core::PCWSTR::null(), ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool();
    if ok && mode.dmDisplayFrequency >= MIN_PANE_FPS as u32 {
        mode.dmDisplayFrequency.clamp(MIN_PANE_FPS as u32, MAX_PANE_FPS as u32) as i32
    } else {
        MIN_PANE_FPS
    }
}

#[cfg(not(windows))]
fn query_default_fps() -> i32 {
    MIN_PANE_FPS
}

/// Target windowless frame rate for normal browsing tabs (matches the display refresh rate).
pub fn pane_fps() -> i32 {
    let cur = PANE_FPS.load(Ordering::Relaxed);
    if cur != 0 {
        return cur;
    }
    let detected = query_default_fps();
    match PANE_FPS.compare_exchange(0, detected, Ordering::Relaxed, Ordering::Relaxed) {
        Ok(_) => detected,
        Err(existing) => existing,
    }
}

/// Updates the pane frame rate from the host app's window display refresh rate. Returns
/// `Some(fps)` when the clamped target changed.
pub fn set_pane_fps(fps: u32) -> Option<i32> {
    let clamped = fps.clamp(MIN_PANE_FPS as u32, MAX_PANE_FPS as u32) as i32;
    let prev = PANE_FPS.swap(clamped, Ordering::Relaxed);
    (prev != clamped).then_some(clamped)
}

/// Whether tab `tab_id` is currently boosted for agent input.
pub fn is_boosted(tab_id: u64) -> bool {
    BOOSTED.with(|b| b.borrow().contains_key(&tab_id))
}

/// An agent's input call for tab `tab_id` arrived.
pub fn boost(tab_id: u64, host: &BrowserHost) {
    let target = AGENT_INPUT_FPS.max(pane_fps());
    BOOSTED.with(|b| {
        let mut b = b.borrow_mut();
        if !b.contains_key(&tab_id) {
            host.set_windowless_frame_rate(target);
        }
        b.insert(tab_id, (Instant::now() + IDLE_AFTER, host.clone()));
    });
}

/// Drops any pending input-FPS boost for a hidden or closed tab so `relax()` never
/// restores `pane_fps()` on a throttled hidden tab.
pub fn clear(tab_id: u64) {
    BOOSTED.with(|b| {
        b.borrow_mut().remove(&tab_id);
    });
}

/// Returns each tab whose agent input stopped to the pane's display-matched rate.
pub fn relax(now: Instant) {
    let target = pane_fps();
    BOOSTED.with(|b| {
        b.borrow_mut().retain(|_, (until, host)| {
            if *until > now {
                return true;
            }
            host.set_windowless_frame_rate(target);
            false
        });
    });
}
