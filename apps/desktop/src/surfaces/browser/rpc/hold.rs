//! Explicit per-tab agent RPC unthrottle hold state machine with 30 s cooldown
//! (matching official `packages/gui-plugins/src/browser/agent-rendering.ts`).
//!
//! Decouples background rendering unthrottle from security-sticky `agent_driven` so idle
//! agent tabs return to 1 FPS + `was_hidden(1)` + released GPU textures once work finishes.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use opencode_kit::servers::ServerKey;
use rune::time::Instant;

use super::session::SessionAttachment;
use crate::App;

/// Official agent rendering cooldown (`packages/gui-plugins/src/browser/agent-rendering.ts`).
pub const COOLDOWN: Duration = Duration::from_secs(30);

/// Explicit hold lifecycle for one browser tab driven by agent RPC.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HoldState {
    #[default]
    Idle,
    Active,
    Cooldown { until: Instant },
}

/// Input event driving [`step`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldEvent {
    /// `ongoing`: active command, trace/CPU/heap recording, or active agent tab on a busy session.
    /// `touched`: one-shot synchronous command touched the tab on this tick.
    Sync { ongoing: bool, touched: bool, now: Instant },
}

/// Pure effects emitted by [`step`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HoldEffects {
    pub set_unthrottled: Option<bool>,
    pub wake_at: Option<Instant>,
}

/// Pure transition function for [`HoldState`].
pub fn step(state: &mut HoldState, event: HoldEvent) -> HoldEffects {
    let HoldEvent::Sync { ongoing, touched, now } = event;
    match (*state, ongoing, touched) {
        (HoldState::Idle, true, _) => {
            *state = HoldState::Active;
            HoldEffects { set_unthrottled: Some(true), wake_at: None }
        }
        (HoldState::Active, true, _) => HoldEffects::default(),
        (HoldState::Cooldown { .. }, true, _) => {
            *state = HoldState::Active;
            HoldEffects::default()
        }
        // One-shot synchronous command finished on this tick: enter cooldown immediately.
        (HoldState::Idle, false, true) => {
            let until = now + COOLDOWN;
            *state = HoldState::Cooldown { until };
            HoldEffects { set_unthrottled: Some(true), wake_at: Some(until) }
        }
        (HoldState::Active | HoldState::Cooldown { .. }, false, true) => {
            let until = now + COOLDOWN;
            *state = HoldState::Cooldown { until };
            HoldEffects { set_unthrottled: None, wake_at: Some(until) }
        }
        (HoldState::Idle, false, false) => HoldEffects::default(),
        (HoldState::Active, false, false) => {
            let until = now + COOLDOWN;
            *state = HoldState::Cooldown { until };
            HoldEffects { set_unthrottled: None, wake_at: Some(until) }
        }
        (HoldState::Cooldown { until }, false, false) if now >= until => {
            *state = HoldState::Idle;
            HoldEffects { set_unthrottled: Some(false), wake_at: None }
        }
        (HoldState::Cooldown { until }, false, false) => {
            HoldEffects { set_unthrottled: None, wake_at: Some(until) }
        }
    }
}

/// Synchronously unthrottles `host_id` before dispatching an RPC command or tab open so the
/// engine wakes the tab ahead of CDP calls, and records a one-shot touch for cooldown tracking.
pub(super) fn acquire_now(att: &mut SessionAttachment, app: &mut App, host_id: u64) {
    app.workspace_panes.browser.host.set_tab_unthrottled(host_id, true);
    att.hold_touched.push(host_id);
}

/// Tracks per-tab [`HoldState`] entries across all attached RPC sessions.
#[derive(Default)]
pub(super) struct RpcHoldManager {
    states: BTreeMap<u64, HoldState>,
}

impl RpcHoldManager {
    /// Recomputes active RPC holds across all sessions, steps each tab's [`HoldState`],
    /// forwards transitions to `BrowserHost::set_tab_unthrottled`, and returns the earliest
    /// cooldown deadline to schedule on `tx.wake_after`.
    pub fn sync(
        &mut self,
        app: &mut App,
        sessions: &mut BTreeMap<(ServerKey, String), SessionAttachment>,
        now: Instant,
    ) -> Option<Instant> {
        let mut ongoing_tabs: HashSet<u64> = HashSet::new();
        let mut touched_tabs: HashSet<u64> = HashSet::new();
        for att in sessions.values_mut() {
            for hid in att.hold_touched.drain(..) {
                if app.workspace_panes.browser.host.tab(hid).is_some() {
                    touched_tabs.insert(hid);
                }
            }
            for cmd in &att.active_commands {
                ongoing_tabs.insert(cmd.target_host_tab());
            }
            if let Some(rec) = &att.trace_recording
                && rec.auto_stopped.is_none()
            {
                ongoing_tabs.insert(rec.host_id);
            }
            for (&hid, rec) in &att.cpu_started {
                if rec.auto_stopped.is_none() {
                    ongoing_tabs.insert(hid);
                }
            }
            for &hid in att.heap_chunks.keys() {
                ongoing_tabs.insert(hid);
            }
            // While a session turn is busy, hold only its active `agent_tab_id` awake (not
            // every historical `agent_driven` tab opened earlier in the session).
            if app.session_busy(&att.session_id)
                && let Some(wire_id) = att.agent_tab_id.as_deref()
                && let Some(&hid) = att.wire_to_host.get(wire_id)
            {
                ongoing_tabs.insert(hid);
            }
        }

        // Drop state for closed tabs; `BrowserHost::close_tab` / `TabClosed` already cleaned up.
        self.states
            .retain(|&hid, _| app.workspace_panes.browser.host.tab(hid).is_some());
        ongoing_tabs.retain(|&hid| app.workspace_panes.browser.host.tab(hid).is_some());
        touched_tabs.retain(|&hid| app.workspace_panes.browser.host.tab(hid).is_some());

        let mut all_tabs: Vec<u64> = self.states.keys().copied().collect();
        for &hid in ongoing_tabs.iter().chain(touched_tabs.iter()) {
            if !self.states.contains_key(&hid) && !all_tabs.contains(&hid) {
                all_tabs.push(hid);
            }
        }

        let mut earliest_wake: Option<Instant> = None;
        for hid in all_tabs {
            let ongoing = ongoing_tabs.contains(&hid);
            let touched = touched_tabs.contains(&hid);
            let state = self.states.entry(hid).or_default();
            let eff = step(state, HoldEvent::Sync { ongoing, touched, now });
            if let Some(unthrottled) = eff.set_unthrottled {
                app.workspace_panes.browser.host.set_tab_unthrottled(hid, unthrottled);
            }
            if let Some(w) = eff.wake_at {
                earliest_wake = Some(earliest_wake.map_or(w, |cur| cur.min(w)));
            }
            if *state == HoldState::Idle {
                self.states.remove(&hid);
            }
        }
        earliest_wake
    }
}
