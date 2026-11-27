//! One prompt's way to the server, as an explicit machine. Desktop's counterparts:
//! `composer/machine.ts` (`handing-off`, then `failSubmission`/`restoreSubmission` in
//! `composer/submit.ts:595-614`), `session/handoff-machine.ts` (the shown prompt waits for the
//! server's admission echo) and `composer/prompt-status-machine.ts` (`admitted` / `failed`). The
//! durable outbox, the retries and the receipt check are Rune's own (`send_failed.rs`): Desktop
//! puts a failed prompt back in the composer, where Rune keeps it in the outbox and sends it again.
//!
//! A journaled prompt's row is in `opencode.sqlite` (`outbox.rs`), and its write only ever leaves
//! from the storage thread once the row that says `Sending` is durable: a prompt is never on the
//! wire without a row that makes the next launch check for it, so none is lost or sent twice.

use opencode_kit::client::{Prompt, Write};
use rune::machine::Effects;

use super::send_reason::NO_RECEIPT;

/// How long a prompt sends before the UI says the server hasn't answered yet.
pub const SLOW_SEND: f64 = 5.0;

/// Action queued while waiting for a delivery receipt check.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq, Hash)]
pub enum AfterCheck {
    Edit,
    Delete,
}

/// Slow-send timer state while a prompt is in [`Phase::Sending`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum SlowSend {
    /// Entered `Sending` without a timestamp yet; the first [`Event::Tick`] anchors it.
    #[default]
    Unstamped,
    /// Sending since `since`, not yet past [`SLOW_SEND`].
    Waiting { since: f64 },
    /// Past [`SLOW_SEND`]: the server has not answered yet.
    Slow,
}

/// Which server write an outbox entry performs.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Operation {
    /// `MovingHome` in a profile saved before the "Session location unavailable" card: a reply to a
    /// subagent that moved its session to its project's checkout by itself when its folder was
    /// gone. It is a plain send now, and the card asks where the session goes on.
    #[default]
    #[serde(alias = "MovingHome")]
    Send,
    /// A queued follow-up submitted with Alt+Enter (`#63109`): once admitted to the server's inbox,
    /// moves ahead of existing queued prompts (or immediately after any still-queued `after` IDs
    /// submitted with Alt+Enter even more recently before this entry drained).
    SendFirst {
        #[serde(default)]
        after: Vec<String>,
    },
    Start(String),
    Worktree {
        project: String,
        from: String,
        branch: Option<String>,
    },
    Revert,
}

impl Operation {
    pub fn creates(&self) -> bool {
        matches!(self, Self::Start(_) | Self::Worktree { .. })
    }

    /// Marks a plain send or subagent reply to move to the front of the queue once admitted.
    pub fn into_first(self) -> Self {
        match self {
            Self::Send => Self::SendFirst { after: Vec::new() },
            other => other,
        }
    }

    /// Drops any queue-first intent (e.g. when promoted directly to a steer before send).
    pub fn into_plain(self) -> Self {
        match self {
            Self::SendFirst { .. } => Self::Send,
            other => other,
        }
    }

    /// If this entry carries queue-first intent, returns the IDs it should remain ordered behind.
    pub fn first_after(&self) -> Option<&[String]> {
        match self {
            Self::SendFirst { after, .. } => Some(after),
            _ => None,
        }
    }

    /// Mutable access to the queue-first `after` anchor IDs.
    pub fn first_after_mut(&mut self) -> Option<&mut Vec<String>> {
        match self {
            Self::SendFirst { after, .. } => Some(after),
            _ => None,
        }
    }

    pub fn write(&self, session: String, prompt: Prompt) -> Write {
        match self {
            Self::Send | Self::SendFirst { .. } => Write::Send { session, prompt },
            Self::Start(directory) => Write::StartAs { session, directory: directory.clone(), prompt },
            Self::Worktree { project, from, branch } => {
                Write::StartInNewWorktree { session, project: project.clone(), from: from.clone(), branch: branch.clone(), prompt }
            }
            Self::Revert => Write::SendFromRevert { session, prompt },
        }
    }
}

/// Where a prompt is on its way. Only `Sending` and `Unconfirmed` can hold an Edit or Delete the
/// user asked for while the server's answer was still out (`then`); every other phase acts on
/// one at once.
#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Saved, not sent: the outbox's next pump sends it. `why` says what it waits for (empty:
    /// only its turn).
    Waiting { why: String },
    /// On its way through the writer of window `by` (`windows::peer::token`; 0 is a window of
    /// its own, a fixture or story), as hand-off number `handoff`: its row commits, then the
    /// storage thread gives the write to the writer (`NotSaved` when the commit failed and the
    /// write never went, `NotSent` when the writer was gone).
    Sending { by: u64, then: Option<AfterCheck>, handoff: u32, slow: SlowSend },
    /// Its reply was lost: a receipt check decides whether the server has it. `why` is what the
    /// last check or failure said.
    Unconfirmed { why: String, then: Option<AfterCheck> },
    /// The server refused it, or a check found it never arrived: the user retries, edits or
    /// deletes it. `restored` when its words went back into the composer, so Enter on them as
    /// they are is Retry.
    Failed { why: String, restored: bool },
    /// The server has it.
    Accepted,
    /// Kept, not to be sent yet: a new session's first prompt while its worktree is made or its
    /// folder's MCP servers settle (`new_session`). Its row is durable from Enter, so a kill
    /// loses nothing; read back at launch it's Waiting, and its write makes what's missing.
    Preparing,
}

/// How a failed write ended, as far as its error proves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// A 4xx other than 408, 425 or 429: the server refused this very prompt.
    Rejected,
    /// It never reached the server (connection refused before a start began, say).
    Unsent,
    /// Anything else: the server may have it.
    Lost,
}

/// What a receipt check found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    /// In the inbox, or delivered as a message.
    Arrived,
    /// The session exists and has neither.
    Missing,
    /// The session itself isn't there (404).
    NoSession,
    /// The server couldn't answer; `why` is its error in words.
    Unknown(&'static str),
}

pub enum Event {
    /// The outbox's pump hands it to window `by`'s writer at `now`.
    Send { by: u64, now: f64 },
    /// Clock tick at `now`: anchors an unstamped `Sending` phase or marks it slow past [`SLOW_SEND`].
    Tick { now: f64 },
    /// Hand-off `handoff`'s writer couldn't take it (the window's link is gone).
    NotSent { why: &'static str, handoff: u32 },
    /// Its row couldn't be committed: hand-off `handoff`'s write never went, or (`handoff` none)
    /// the row that holds it waiting was never saved. Either way nothing keeps it across a
    /// launch, so its words go back to the composer.
    NotSaved { handoff: Option<u32> },
    WriteFailed { why: String, verdict: Verdict },
    /// The server has it: its admission echo, a page or inbox listing with its id, the write's
    /// own reply, or a receipt.
    Admitted,
    Checked(Found),
    /// Window `by` closed (or left the server) with its write still out.
    Released { by: u64 },
    /// Read back from the journal at launch: a write out when the app quit may have landed.
    Reopened,
    /// Retry now, from the bubble or its menu.
    Retry,
    /// Edit in composer, or Delete, from its menu.
    Act(AfterCheck),
    /// Its Edit waits for the user to come back to its session.
    Parked,
    /// Its words went back into an empty composer.
    Restored,
    /// A Preparing prompt's session phase began: it may go now.
    Prepared,
}

#[derive(Debug, PartialEq)]
pub enum Effect {
    /// Write its row (journaled prompts only).
    Save,
    /// Ask the server for receipts of every unconfirmed prompt.
    Check,
    /// Back off before the outbox pumps again.
    Defer,
    /// Show it as not sent, back in the timeline (out of the inbox).
    Outbox,
    /// Show it as failed in the timeline (`App::fail_send`).
    ShowFailed,
    /// Put its words back where the user will see them (`App::restore_rejection`).
    Restore,
    /// Carry out an Edit or Delete the user asked for.
    Run(AfterCheck),
    /// Accepted: reset the backoff; `late` when an Edit or Delete waited on it, which the server
    /// beat ("Message already sent").
    Accepted { late: bool },
    /// A `Sending` prompt crossed [`SLOW_SEND`] and its status footer should repaint.
    Slow,
}

/// Owned machine state for a standalone headless send entry.
#[derive(Clone, Debug, PartialEq)]
pub struct SendState {
    pub phase: Phase,
    pub journaled: bool,
    pub operation: Operation,
    pub handoffs: u32,
}

impl SendState {
    pub fn step(&mut self, event: Event) -> Effects<Effect> {
        step_fields(&mut self.phase, &mut self.operation, &mut self.handoffs, self.journaled, event)
    }
}

/// Steps an owned [`SendState`] machine.
pub fn step(s: &mut SendState, event: Event) -> Effects<Effect> {
    s.step(event)
}

/// Steps the send machine directly over a caller's own fields without copying.
pub fn step_fields(phase: &mut Phase, operation: &mut Operation, handoffs: &mut u32, journaled: bool, event: Event) -> Effects<Effect> {
    match (&mut *phase, event) {
        (Phase::Accepted, _) => Effects::none(),
        (Phase::Preparing, Event::Prepared) => {
            *phase = Phase::Waiting { why: String::new() };
            Effects::one(Effect::Save)
        }
        (Phase::Preparing, _) => Effects::none(),
        (_, Event::Admitted) => {
            let late = matches!(&*phase, Phase::Sending { then: Some(_), .. } | Phase::Unconfirmed { then: Some(_), .. });
            *phase = Phase::Accepted;
            Effects::two(Effect::Save, Effect::Accepted { late })
        }
        // The pump commits its row, and the storage thread hands the write over once it's durable.
        (Phase::Waiting { .. }, Event::Send { by, now }) => {
            *handoffs += 1;
            *phase = Phase::Sending { by, then: None, handoff: *handoffs, slow: SlowSend::Waiting { since: now } };
            Effects::none()
        }
        (Phase::Sending { slow, .. }, Event::Tick { now }) => match *slow {
            SlowSend::Unstamped => {
                *slow = SlowSend::Waiting { since: now };
                Effects::none()
            }
            SlowSend::Waiting { since } if now + 1e-9 >= since + SLOW_SEND => {
                *slow = SlowSend::Slow;
                Effects::one(Effect::Slow)
            }
            SlowSend::Waiting { .. } | SlowSend::Slow => Effects::none(),
        },
        (Phase::Sending { then, handoff, .. }, Event::NotSent { why, handoff: h }) if *handoff == h => {
            let then = then.take();
            // Its row still says Sending: a launch before the next hand-off checks for it first.
            *phase = Phase::Waiting { why: why.into() };
            then.map_or(Effects::none(), |action| Effects::one(Effect::Run(action)))
        }
        (Phase::Sending { then, handoff, .. }, Event::NotSaved { handoff: Some(h) }) if *handoff == h => {
            let then = then.take();
            reject(phase, journaled, "Couldn't save message".into(), then)
        }
        (Phase::Waiting { .. }, Event::NotSaved { handoff: None }) => reject(phase, journaled, "Couldn't save message".into(), None),
        (Phase::Sending { then, .. }, Event::WriteFailed { why, verdict }) => {
            let then = then.take();
            failed(phase, journaled, why, verdict, then)
        }
        (Phase::Sending { by, then, .. }, Event::Released { by: gone }) if *by == gone => {
            *phase = Phase::Unconfirmed { why: String::new(), then: then.take() };
            Effects::two(Effect::Save, Effect::Check)
        }
        (Phase::Sending { then, .. }, Event::Reopened) => {
            *phase = Phase::Unconfirmed { why: String::new(), then: then.take() };
            Effects::none()
        }
        (Phase::Unconfirmed { then, .. }, Event::Checked(found)) => {
            let then = then.take();
            checked(phase, operation, journaled, found, then)
        }
        (Phase::Failed { .. }, Event::Retry) => {
            *phase = Phase::Waiting { why: String::new() };
            Effects::one(Effect::Save)
        }
        (Phase::Waiting { .. } | Phase::Failed { .. }, Event::Act(action)) => Effects::one(Effect::Run(action)),
        (Phase::Sending { then, .. }, Event::Act(action)) => {
            *then = Some(action);
            Effects::one(Effect::Save)
        }
        (Phase::Unconfirmed { then, .. }, Event::Act(action)) => {
            *then = Some(action);
            Effects::two(Effect::Save, Effect::Check)
        }
        (Phase::Waiting { .. } | Phase::Failed { .. }, Event::Parked) => {
            *phase = Phase::Failed { why: "Ready to edit".into(), restored: false };
            Effects::one(Effect::Save)
        }
        (Phase::Failed { restored, .. }, Event::Restored) if journaled => {
            *restored = true;
            Effects::none()
        }
        _ => Effects::none(),
    }
}

/// A write that failed. A local prompt (no journal) fails at once when it surely never arrived
/// and is checked otherwise; a journaled one fails only when the server refused it, else waits
/// for the connection or a receipt.
fn failed(phase: &mut Phase, journaled: bool, why: String, verdict: Verdict, then: Option<AfterCheck>) -> Effects<Effect> {
    if verdict == Verdict::Rejected || (!journaled && verdict == Verdict::Unsent) {
        return reject(phase, journaled, why, then);
    }
    if !journaled {
        *phase = Phase::Unconfirmed { why, then };
        return Effects::one(Effect::Check);
    }
    // Not sent: an Edit or Delete waiting on it goes ahead. Maybe sent: it waits for the receipt.
    let mut effects = Effects::one(Effect::Outbox);
    match (verdict, then) {
        (Verdict::Unsent, Some(action)) => {
            *phase = Phase::Waiting { why };
            effects.push(Effect::Run(action));
        }
        (Verdict::Unsent, None) => *phase = Phase::Waiting { why },
        (_, then) => *phase = Phase::Unconfirmed { why, then },
    }
    effects.push(Effect::Defer);
    effects.push(Effect::Save);
    effects
}

/// Failed for good. A journaled prompt's words go back to the composer (or its Edit or Delete
/// runs); a local one keeps its bubble with Retry and Edit.
fn reject(phase: &mut Phase, journaled: bool, why: String, then: Option<AfterCheck>) -> Effects<Effect> {
    *phase = Phase::Failed { why, restored: false };
    if !journaled {
        return Effects::one(Effect::ShowFailed);
    }
    Effects::Many(vec![Effect::ShowFailed, then.map_or(Effect::Restore, Effect::Run), Effect::Save])
}

fn checked(phase: &mut Phase, operation: &mut Operation, journaled: bool, found: Found, then: Option<AfterCheck>) -> Effects<Effect> {
    match found {
        Found::Arrived => {
            *phase = Phase::Accepted;
            Effects::two(Effect::Save, Effect::Accepted { late: then.is_some() })
        }
        Found::Missing if !journaled => reject(phase, journaled, String::new(), then),
        // A revert commit isn't idempotent: never replay that destructive step.
        Found::Missing if matches!(operation, Operation::Revert) => {
            *operation = Operation::Send;
            reject(phase, journaled, "Check the rewind before resending".into(), then)
        }
        Found::Missing => {
            // The session exists, so a create went through before its reply was lost.
            if operation.creates() {
                *operation = Operation::Send;
            }
            waiting(phase, then)
        }
        Found::NoSession if matches!(operation, Operation::Start(_)) => waiting(phase, then),
        Found::NoSession if journaled => {
            let why = if operation.creates() { "Couldn't confirm the new worktree" } else { "Session not found" };
            reject(phase, journaled, why.into(), then)
        }
        Found::NoSession => {
            *phase = Phase::Unconfirmed { why: String::new(), then };
            Effects::none()
        }
        // No receipt isn't a missing receipt: keep it unconfirmed, and never send it here.
        Found::Unknown(why) => {
            // A receipt read that proved nothing keeps the reason the write already gave (a captive
            // portal named there, say), rather than a vaguer one.
            let why = match &*phase {
                Phase::Unconfirmed { why: was, .. } | Phase::Waiting { why: was } if why == NO_RECEIPT && !was.is_empty() => was.clone(),
                _ => why.to_owned(),
            };
            let changed = !matches!(&*phase, Phase::Unconfirmed { why: was, .. } if *was == why);
            *phase = Phase::Unconfirmed { why: if journaled { why } else { String::new() }, then };
            if changed && journaled { Effects::one(Effect::Save) } else { Effects::none() }
        }
    }
}

fn waiting(phase: &mut Phase, then: Option<AfterCheck>) -> Effects<Effect> {
    *phase = Phase::Waiting { why: String::new() };
    match then {
        Some(action) => Effects::two(Effect::Save, Effect::Run(action)),
        None => Effects::one(Effect::Save),
    }
}
