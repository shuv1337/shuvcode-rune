//! Explicit per-tab visibility and frame-throttling state machine for `EngineManager`.
//!
//! Centralizes `was_hidden`, windowless FPS throttling (1 FPS hidden vs display-matched
//! `pane_fps()` awake), late-frame blocking (`ManagedTab::awake`), keyboard focus restore,
//! and Windows D3D11 / Linux dmabuf GPU frame ring release across `SetVisibleTabs`, legacy
//! `FocusTab`, `SetTabUnthrottled`, `open_tab`, and `ExtEffect::FocusTab`.

use std::sync::atomic::Ordering;

use cef::{ImplBrowserHost, PaintElementType};

use super::{EngineManager, ManagedTab};

/// Whether the connected host drives visibility explicitly (`SetVisibleTabs` / `SetTabUnthrottled`)
/// or uses legacy single-active `FocusTab` semantics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum LifecycleMode {
    #[default]
    Legacy,
    Explicit,
}

/// Explicit per-tab rendering mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TabRenderMode {
    #[default]
    Hidden,
    AwakeBackground,
    Visible,
}

impl TabRenderMode {
    pub(crate) fn is_awake(self) -> bool {
        !matches!(self, Self::Hidden)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RenderModeEffect {
    None,
    Wake { visible: bool },
    BecameVisible,
    BecameBackground,
    Sleep,
}

fn step_render_mode(state: &mut TabRenderMode, want: TabRenderMode) -> RenderModeEffect {
    let prev = *state;
    if prev == want {
        return RenderModeEffect::None;
    }
    *state = want;
    match (prev, want) {
        (TabRenderMode::Hidden, TabRenderMode::Visible) => RenderModeEffect::Wake { visible: true },
        (TabRenderMode::Hidden, TabRenderMode::AwakeBackground) => RenderModeEffect::Wake { visible: false },
        (TabRenderMode::AwakeBackground, TabRenderMode::Visible) => RenderModeEffect::BecameVisible,
        (TabRenderMode::Visible, TabRenderMode::AwakeBackground) => RenderModeEffect::BecameBackground,
        (TabRenderMode::Visible | TabRenderMode::AwakeBackground, TabRenderMode::Hidden) => RenderModeEffect::Sleep,
        _ => RenderModeEffect::None,
    }
}

fn apply_render_effect(
    id: u64,
    t: &mut ManagedTab,
    visible: bool,
    focused: bool,
    effect: RenderModeEffect,
) {
    #[cfg(target_os = "linux")]
    {
        t.shown = visible;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = visible;

    match effect {
        RenderModeEffect::None => {}
        RenderModeEffect::Wake { visible } => {
            t.awake.store(true, Ordering::Release);
            let fps = if crate::input_fps::is_boosted(id) {
                crate::input_fps::AGENT_INPUT_FPS.max(crate::input_fps::pane_fps())
            } else {
                crate::input_fps::pane_fps()
            };
            t.host.set_windowless_frame_rate(fps);
            t.host.was_hidden(0);
            if visible && focused {
                t.host.set_focus(1);
            }
            t.host.invalidate(PaintElementType::default());
        }
        RenderModeEffect::BecameVisible => {
            if focused {
                t.host.set_focus(1);
            }
            t.host.invalidate(PaintElementType::default());
        }
        RenderModeEffect::BecameBackground => {
            if !t.is_popup {
                t.host.set_focus(0);
            }
        }
        RenderModeEffect::Sleep => {
            // Gate render handler first so late in-flight Viz frames cannot reallocate GPU
            // slots or repopulate the desktop surface after release.
            t.awake.store(false, Ordering::Release);
            crate::input_fps::clear(id);
            t.host.set_focus(0);
            t.host.was_hidden(1);
            t.host.set_windowless_frame_rate(1);
            #[cfg(windows)]
            if let Some(ring) = &t.frames {
                ring.lock().unwrap().release_slots();
            }
            #[cfg(target_os = "linux")]
            if let Some(ring) = &t.frames {
                let _ = ring.lock().unwrap().step(crate::dmabuf_frames::Event::Released);
            }
        }
    }
}

impl EngineManager {
    pub(super) fn on_hello_lifecycle(&mut self, tab_lifecycle: bool) {
        self.lifecycle_mode = if tab_lifecycle { LifecycleMode::Explicit } else { LifecycleMode::Legacy };
        self.unthrottled_tabs.clear();
        if tab_lifecycle {
            self.visible_tabs.clear();
            self.sync_tab_visibility();
        }
    }

    pub(super) fn on_disconnect_lifecycle(&mut self) {
        self.unthrottled_tabs.clear();
        self.visible_tabs.clear();
        self.sync_tab_visibility();
    }

    pub(super) fn on_tab_closed_lifecycle(&mut self, tab_id: u64) {
        self.visible_tabs.remove(&tab_id);
        self.unthrottled_tabs.remove(&tab_id);
        if self.focused_tab == Some(tab_id) {
            self.focused_tab = None;
        }
        crate::input_fps::clear(tab_id);
    }

    /// Whether any managed tab is currently awake (used on macOS to relax the idle run-loop pump).
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub fn has_awake_tabs(&self) -> bool {
        self.tabs.values().any(|t| t.render_mode.is_awake())
    }

    pub(super) fn handle_set_visible_tabs(&mut self, tab_ids: Vec<u64>) {
        self.visible_tabs = tab_ids.into_iter().collect();
        self.sync_tab_visibility();
    }

    pub(super) fn handle_set_tab_unthrottled(&mut self, tab_id: u64, active: bool) {
        if active {
            self.unthrottled_tabs.insert(tab_id);
        } else {
            self.unthrottled_tabs.remove(&tab_id);
        }
        self.sync_tab_visibility();
    }

    pub(super) fn handle_focus_tab(&mut self, tab_id: u64) {
        let was_visible = self
            .tabs
            .get(&tab_id)
            .is_some_and(|t| t.render_mode == TabRenderMode::Visible);
        self.focused_tab = Some(tab_id);
        if self.lifecycle_mode == LifecycleMode::Legacy {
            self.visible_tabs.clear();
            self.visible_tabs.insert(tab_id);
        }
        for (&id, t) in &self.tabs {
            t.host.set_focus(i32::from(id == tab_id));
        }
        self.sync_tab_visibility();
        // If already visible, `sync_tab_visibility` emitted `None`; keep legacy repaint on FocusTab.
        if was_visible
            && let Some(t) = self.tabs.get(&tab_id)
            && t.render_mode == TabRenderMode::Visible
        {
            t.host.invalidate(PaintElementType::default());
        }
        self.on_tab_focus_extensions(tab_id);
    }

    pub(super) fn handle_ext_focus_tab(&mut self, engine_tab_id: u64) {
        self.focused_tab = Some(engine_tab_id);
        if self.lifecycle_mode == LifecycleMode::Legacy {
            self.visible_tabs.clear();
            self.visible_tabs.insert(engine_tab_id);
        }
        for (&id, t) in &self.tabs {
            t.host.set_focus(i32::from(id == engine_tab_id));
        }
        self.sync_tab_visibility();
    }

    pub(super) fn on_tab_opened(&mut self, tab_id: u64, focused: bool, is_popup: bool) {
        if is_popup {
            self.focused_tab = Some(tab_id);
            if let Some(t) = self.tabs.get(&tab_id) {
                t.host.was_hidden(0);
                t.host.set_focus(1);
            }
        } else if focused {
            self.focused_tab = Some(tab_id);
            if self.lifecycle_mode == LifecycleMode::Legacy {
                self.visible_tabs.clear();
                self.visible_tabs.insert(tab_id);
            }
            for (&id, existing) in &self.tabs {
                if !existing.is_popup {
                    existing.host.set_focus(i32::from(id == tab_id));
                }
            }
        } else if self.lifecycle_mode == LifecycleMode::Legacy {
            self.visible_tabs.remove(&tab_id);
        }
        self.sync_tab_visibility();
    }

    pub(super) fn sync_tab_visibility(&mut self) {
        let legacy = self.lifecycle_mode == LifecycleMode::Legacy;
        let focused_tab = self.focused_tab;
        for (&id, t) in &mut self.tabs {
            let visible = t.is_popup || self.visible_tabs.contains(&id);
            let held = self.unthrottled_tabs.contains(&id)
                || (legacy && t.prompts.agent_driven.load(Ordering::Relaxed));
            let want = if visible {
                TabRenderMode::Visible
            } else if held {
                TabRenderMode::AwakeBackground
            } else {
                TabRenderMode::Hidden
            };
            let effect = step_render_mode(&mut t.render_mode, want);
            apply_render_effect(id, t, visible, focused_tab == Some(id), effect);
        }
    }
}
