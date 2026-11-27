//! The one-line Rune element that shows a tab's frames and forwards its input.

use rune::{Align, Drag, El, KeyEvent, MagnifyEvent, Phase, PointerButton, PointerEvent, WheelEvent, el};

use crate::client::BrowserHost;
use crate::protocol::{HostMsg, PointerBtn, pack_mods};

impl BrowserHost {
    pub fn set_device(&mut self, tab_id: u64, device: Option<crate::device::Device>) {
        let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) else { return };
        if tab.device == device { return; }
        tab.device = device.clone();
        let scale = device.as_ref().map_or(1.0, |d| d.fit([tab.viewport_pt.0, tab.viewport_pt.1]));
        self.send(HostMsg::Emulate { tab_id, device, scale });
    }

    /// The page's surface element. `scale` is the window's backing scale and `view_zoom` how many
    /// window points one layout point of this element covers (the app's own zoom). The page gets
    /// a viewport in window points, so it renders at its own zoom, not the app's. Frames draw
    /// one texel per device pixel from the top-left corner, so while the element resizes the
    /// last frame holds still, with the parent's fill in any gap, until one at the new size lands.
    /// `pressed` runs as any button goes down on the page, so the app can note that the keyboard
    /// is in it now (the page's own focus never reaches the app).
    pub fn surface_el<S: 'static>(
        &self,
        key: impl Into<String>,
        tab_id: u64,
        scale: f32,
        view_zoom: f32,
        access: fn(&mut S) -> &mut BrowserHost,
        pressed: fn(&mut S, u64),
    ) -> El {
        let z = view_zoom.max(0.01);
        let Some(tab) = self.tab(tab_id) else {
            return el().key(key.into());
        };
        let page = el().key(key.into())
            .grow(1.0)
            .min_w(0.0)
            .min_h(0.0)
            .w_full()
            .focusable(true)
            .cursor(tab.cursor.to_rune())
            // The engine takes the primary, middle and secondary buttons (CEF has no others): a
            // Back or Forward press would reach the page as a primary click on what's under it.
            .pointer_buttons([PointerButton::Middle]);
        crate::frame::draw(page, &tab.surface)
            .surface_anchor(Align::Start, Align::Start)
            .surface_unscaled(true)
            .on_layout_tx(move |s: &mut S, [x, y, w, h], tx| {
                let width_pt = (w * z).max(1.0);
                let height_pt = (h * z).max(1.0);
                let sc = scale.max(1.0);
                let host = access(s);
                host.last_viewport = ((width_pt, height_pt), scale.max(1.0));
                let connected = host.is_connected();
                let mut should_send = false;
                if let Some(t) = host.tabs.iter_mut().find(|t| t.id == tab_id) {
                    t.surface_origin = (x, y);
                    t.view_zoom = z;
                    t.viewport_pt = (width_pt, height_pt);
                    t.scale = sc;
                    let changed = t.sent_resize.is_none_or(|(pw, ph, ps)| {
                        (pw - width_pt).abs() > 0.5
                            || (ph - height_pt).abs() > 0.5
                            || (ps - sc).abs() > 0.01
                    });
                    if connected && changed {
                        t.sent_resize = Some((width_pt, height_pt, sc));
                        should_send = true;
                    }
                }
                if should_send {
                    host.send(HostMsg::Resize { tab_id, width_pt, height_pt, scale: sc });
                    if let Some(device) = host.tab(tab_id).and_then(|t| t.device.clone()) {
                        // CEF receives ordinary surface-local input; the CDP metrics scale
                        // maps the composited page to that surface's window points.
                        let scale = device.fit([width_pt, height_pt]);
                        host.send(HostMsg::Emulate { tab_id, device: Some(device), scale });
                    }
                }
                tx.live_only();
            })
            .on_pointer_move_local(move |s: &mut S, p: PointerEvent, tx| {
                let host = access(s);
                if host.tab(tab_id).is_some_and(|t| t.prompts.has_blocking_modal()) {
                    return;
                }
                host.send(HostMsg::MouseMove {
                    tab_id,
                    x: p.x * z,
                    y: p.y * z,
                    mods: pack_mods(tx.mods()),
                    leave: false,
                });
                tx.live_only();
            })
            .on_hover_change(move |s: &mut S, inside: bool, tx| {
                if !inside {
                    access(s).send(HostMsg::MouseMove {
                        tab_id,
                        x: -1.0,
                        y: -1.0,
                        mods: pack_mods(tx.mods()),
                        leave: true,
                    });
                }
                tx.live_only();
            })
            .on_drag(move |s: &mut S, d: &mut Drag, tx| {
                if access(s).tab(tab_id).is_some_and(|t| t.prompts.has_blocking_modal()) {
                    return;
                }
                let mods = pack_mods(tx.mods());
                let btn = PointerBtn::from(tx.button());
                if d.phase == Phase::Down {
                    pressed(s, tab_id);
                }
                let host = access(s);
                if d.phase == Phase::Down
                    && let Some(t) = host.tabs.iter_mut().find(|t| t.id == tab_id)
                {
                    t.prompts.record_document_input();
                }
                match d.phase {
                    Phase::Down => host.send(HostMsg::MouseButton {
                        tab_id,
                        x: d.x * z,
                        y: d.y * z,
                        button: btn,
                        up: false,
                        clicks: d.clicks.clamp(1, 3),
                        mods,
                    }),
                    Phase::Move => {
                        host.send(HostMsg::MouseMove {
                            tab_id,
                            x: d.x * z,
                            y: d.y * z,
                            mods,
                            leave: false,
                        });
                        tx.live_only();
                    }
                    Phase::Up => {
                        host.send(HostMsg::MouseButton {
                            tab_id,
                            x: d.x * z,
                            y: d.y * z,
                            button: btn,
                            up: true,
                            clicks: d.clicks.clamp(1, 3),
                            mods,
                        });
                        tx.live_only();
                    }
                }
            })
            .on_secondary_press_local(move |s: &mut S, p: PointerEvent, tx| {
                if access(s).tab(tab_id).is_some_and(|t| t.prompts.has_blocking_modal()) {
                    return;
                }
                let mods = pack_mods(tx.mods());
                pressed(s, tab_id);
                let host = access(s);
                if let Some(t) = host.tabs.iter_mut().find(|t| t.id == tab_id) {
                    t.prompts.record_document_input();
                }
                for up in [false, true] {
                    host.send(HostMsg::MouseButton {
                        tab_id,
                        x: p.x * z,
                        y: p.y * z,
                        button: PointerBtn::Secondary,
                        up,
                        clicks: 1,
                        mods,
                    });
                }
            })
            .on_wheel(move |s: &mut S, w: WheelEvent, tx| {
                let host = access(s);
                if host.tab(tab_id).is_some_and(|t| t.prompts.has_blocking_modal()) {
                    return false;
                }
                let zoom_mod = if cfg!(target_os = "macos") { w.mods.cmd && !w.mods.ctrl } else { w.mods.ctrl && !w.mods.cmd };
                if zoom_mod && !w.mods.alt && w.dy.abs() > 0.0 {
                    host.step_zoom(tab_id, if w.dy > 0.0 { 1 } else { -1 });
                    return true;
                }
                host.note_wheel_scroll(tab_id);
                host.send(HostMsg::Wheel {
                    tab_id,
                    x: w.x * z,
                    y: w.y * z,
                    dx: w.dx,
                    dy: w.dy,
                    mods: pack_mods(w.mods),
                });
                tx.live_only();
                true
            })
            // Ctrl + wheel (and off macOS a pinch, which arrives as one) steps the page's zoom
            // presets (`chromium.ts:289` `zoom-changed`), leaving the window's own pinch zoom alone.
            // A macOS pinch gesture is no `zoom-changed` in Electron, so it goes on to the window.
            .on_magnify(move |s: &mut S, m: MagnifyEvent, _tx| {
                if cfg!(target_os = "macos") && !m.wheel {
                    return false;
                }
                let host = access(s);
                if host.tab(tab_id).is_some_and(|t| t.prompts.has_blocking_modal()) {
                    return false;
                }
                if m.delta.abs() > 0.0 {
                    host.step_zoom(tab_id, if m.delta > 0.0 { 1 } else { -1 });
                }
                true
            })
            .on_key(move |s: &mut S, ev: &KeyEvent, tx| {
                let host = access(s);
                match host.forward_key(tab_id, ev, true) {
                    crate::client::KeyForward::Ignored => false,
                    crate::client::KeyForward::Forwarded => {
                        if let Some(t) = host.tabs.iter_mut().find(|t| t.id == tab_id) {
                            t.prompts.record_document_input();
                        }
                        tx.live_only();
                        true
                    }
                    crate::client::KeyForward::StateChanged => {
                        if let Some(t) = host.tabs.iter_mut().find(|t| t.id == tab_id) {
                            t.prompts.record_document_input();
                        }
                        true
                    }
                }
            })
            .on_key_up(move |s: &mut S, ev: &KeyEvent, tx| {
                match access(s).forward_key(tab_id, ev, false) {
                    crate::client::KeyForward::Ignored => false,
                    crate::client::KeyForward::Forwarded => {
                        tx.live_only();
                        true
                    }
                    crate::client::KeyForward::StateChanged => true,
                }
            })
    }
}
