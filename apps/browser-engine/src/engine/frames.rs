//! Frame-pacing and Linux dmabuf recovery helpers for `EngineManager` (moved verbatim from
//! `engine.rs` to keep `engine.rs` below the 1,500-line guardrail).

#[cfg(target_os = "linux")]
use std::sync::atomic::Ordering;

use cef::ImplBrowserHost;
#[cfg(windows)]
use cef::PaintElementType;

use super::EngineManager;

impl EngineManager {
    /// Updates the display-matched off-screen frame rate on all live non-boosted tabs.
    pub(super) fn set_frame_rate(&self, fps: u32) {
        let Some(clamped) = crate::input_fps::set_pane_fps(fps) else { return };
        for (&tab_id, t) in &self.tabs {
            if t.render_mode.is_awake() && !crate::input_fps::is_boosted(tab_id) {
                t.host.set_windowless_frame_rate(clamped);
            }
        }
    }

    /// Whether a tab dropped a frame and waits for the app to free a texture (`poll_frames`).
    pub fn waits_for_texture(&self) -> bool {
        #[cfg(windows)]
        return self.tabs.values().any(|t| t.frames.as_ref().is_some_and(|ring| ring.lock().unwrap().dropped));
        // Linux: the app's read repaints the tab (`HostMsg::FrameRead`); nothing waits on a timer.
        #[cfg(not(windows))]
        false
    }

    /// Windows: repaints each tab that dropped a frame while the app had every one unread,
    /// once it has read one.
    pub fn poll_frames(&mut self) {
        crate::input_fps::relax(std::time::Instant::now());
        #[cfg(windows)]
        for t in self.tabs.values() {
            let Some(ring) = &t.frames else { continue };
            let mut ring = ring.lock().unwrap();
            if ring.dropped && ring.has_free() {
                ring.dropped = false;
                t.host.invalidate(PaintElementType::default());
            }
        }
    }

    /// Linux: runs tab `tab_id`'s frame effects.
    #[cfg(target_os = "linux")]
    pub(super) fn run_frames(&self, tab_id: u64, effects: Vec<crate::dmabuf_frames::Effect>) {
        let host = self.tabs.get(&tab_id).map(|t| &t.host);
        let events = self.events.clone();
        crate::dmabuf_frames::run(effects, tab_id, &self.out_tx, host, &move |f| {
            let _ = events.send(crate::Event::FramesFailed(f));
        });
    }

    /// Linux: frames can't come the way they do; the engine's transport moves on.
    #[cfg(target_os = "linux")]
    pub fn frames_failed(&mut self, failure: crate::dmabuf_frames::Failure) {
        use crate::dmabuf_frames::{Event, TransportEffect};
        match self.transport.step(failure) {
            Some(TransportEffect::ReadBackTabs) => {
                for (&tab_id, t) in &self.tabs {
                    let Some(ring) = &t.frames else { continue };
                    self.run_frames(tab_id, ring.lock().unwrap().step(Event::ReadBack));
                }
            }
            Some(TransportEffect::ReopenTabs) => {
                let ids: Vec<u64> = self.tabs.keys().copied().collect();
                for tab_id in ids {
                    let Some(tab) = self.tabs.remove(&tab_id) else { continue };
                    self.release_preview(tab_id, tab.preview.as_ref());
                    let was_agent = tab.prompts.agent_driven.load(Ordering::Acquire);
                    self.permissions.on_tab_closed(tab_id);
                    tab.download_alive.store(false, Ordering::Release);
                    tab.host.close_browser(1);
                    self.downloads.close_tab(tab_id, tab.download_gen);
                    if !tab.isolated && !tab.is_popup
                        && let Some(ext) = &mut self.extensions
                    {
                        ext.on_tab_closed(tab_id);
                    }
                    self.release_tab_context(tab_id);
                    let url = tab.snap.lock().unwrap().url.clone();
                    let v = tab.view_state.lock().unwrap().clone();
                    let roots = tab.file_roots.lock().unwrap().clone();
                    let allow_ext = tab.allow_extension_pages && !was_agent;
                    let ctx = tab.context.clone();
                    self.open_tab(
                        tab_id,
                        &url,
                        v.width_pt,
                        v.height_pt,
                        v.scale,
                        tab.shown,
                        roots,
                        ctx,
                        tab.preview_spec,
                        was_agent,
                        tab.isolated,
                        allow_ext,
                        tab.is_popup,
                        None,
                    );
                    if !self.tabs.contains_key(&tab_id) {
                        self.release_tab_context(tab_id);
                    }
                }
            }
            None => {}
        }
    }
}
