//! Server events in two steps. `apply_shared` changes the server's copy of the session data
//! (`windows::peer`), once per event whichever window polls first, and says what it did
//! ([`Done`]); `react` then runs in every window of the server, the first included, for what
//! each shows and does itself: its view, plugins, notifications and sounds (each claimed
//! once per event, [`Claim`]), its own sends and stops.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use opencode_kit::client::{Incoming, Request};
use opencode_kit::model::Transcript;
use rune::Tx;
use serde_json::Value;

use crate::{App, Route, Source, pickers, servers};

pub mod incoming;
pub mod stream;

/// The latest uninterrupted run of stream writes to one transcript. Bulk loads, clears,
/// other transcript writers and server swaps need no bookkeeping: the cell and both ends of
/// the version interval must match before a reader can narrow its comparison to this id.
#[derive(Default)]
pub(crate) struct TranscriptEdits {
    cell: u32,
    from: u64,
    to: u64,
    session: String,
    /// The first message the run's writes may have changed (`touched`): a streamed reply's
    /// writes change only the message they name, so the messages above it hold.
    low: usize,
}

impl TranscriptEdits {
    fn get_mut<'a>(&mut self, map: &'a mut rune::Tracked<HashMap<String, Transcript>>, id: &str) -> Option<&'a mut Transcript> {
        let cell = rune::Tracked::id(map);
        let before = rune::Tracked::version(map);
        let rows = &mut **map;
        if self.cell != cell || self.to != before || self.session != id {
            self.cell = cell;
            self.from = before;
            self.session.clear();
            self.session.push_str(id);
            self.low = usize::MAX;
        }
        self.to = before + 1;
        rows.get_mut(id)
    }

    /// The write just made changed messages from `at` on, as far as anyone can tell.
    fn touched(&mut self, at: usize) {
        self.low = self.low.min(at);
    }

    pub(crate) fn only(&self, cell: u32, from: u64, to: u64) -> Option<&str> {
        (cell == self.cell && self.from <= from && from < to && to == self.to).then_some(&self.session)
    }

    /// Session `session`'s first message that may have changed between versions `from` and `to`
    /// of the transcripts (`cell`), when only this run of stream writes came between (past its
    /// end when the run wrote another session); `None` when anything else wrote them.
    pub(crate) fn changed_from(&self, cell: u32, from: u64, to: u64, session: &str) -> Option<usize> {
        self.only(cell, from, to).map(|s| if s == session { self.low } else { usize::MAX })
    }

    pub(crate) fn from(&self) -> u64 {
        self.from
    }
}

/// The first message stream event `ty` (`d`) may have changed in transcript `t`, which held
/// `len` messages before it. A step's own events (`Transcript::apply`: started, its text,
/// reasoning, tools, retry, end) change only the step they name, found from the end as `apply`
/// finds it; a prompt adds only itself; anything else (compaction, revert, notices) may have
/// changed any message.
fn touched_at(t: &Transcript, ty: &str, d: &serde_json::Value, len: usize) -> usize {
    let step = ty.starts_with("session.tool.")
        || matches!(ty, "session.text.delta" | "session.reasoning.delta" | "session.text.started" | "session.reasoning.started"
            | "session.text.ended" | "session.reasoning.ended" | "session.step.started" | "session.step.first-token"
            | "session.step.streamed" | "session.step.ended" | "session.step.failed" | "session.retry.scheduled");
    if step && let Some(mid) = d["assistantMessageID"].as_str() {
        return t.messages.iter().rposition(|m| m.id() == mid).map_or(0, |i| i.min(len));
    }
    if matches!(ty, "session.prompted" | "session.user.message") && t.messages.len() == len + 1 {
        return len;
    }
    0
}

impl App {
    /// Session `session`'s first message that may have changed since version `since` of its
    /// family's transcripts, when only stream writes to it came between (`TranscriptEdits`).
    pub(crate) fn transcript_changed_from(&self, session: &str, since: u64) -> Option<usize> {
        let (cell, now) = (rune::Tracked::id(&self.transcripts), rune::Tracked::version(&self.transcripts));
        self.transcript_edits.changed_from(cell, since, now, session)
    }

    /// Version of `session`'s family in `self.transcripts`: unchanged across an uninterrupted
    /// stream run on an unrelated session (`TranscriptEdits`), and jumping to the map's version
    /// whenever anything else writes `self.transcripts`.
    pub(crate) fn family_transcript_version(&self, session: &str) -> (u32, u64) {
        let cell = rune::Tracked::id(&self.transcripts);
        let now = rune::Tracked::version(&self.transcripts);
        let root = self.root_of(session);
        let v = match self.transcript_edits.only(cell, self.transcript_edits.from(), now) {
            Some(edited) if self.root_of(edited) != root => self.transcript_edits.from(),
            _ => now,
        };
        (cell, v)
    }
}

/// A stream message after `apply_shared`, for each window's `react`.
pub enum Heard {
    Event(StreamEvent),
    /// Anything else the stream says: it reconnected, or a line about the stream.
    Message(Box<Incoming>),
}

/// Where this poll got the open timeline's geometry. A page wins over stream events in
/// either order: growth opts in a whole stack, not just the row that the stream changed.
#[derive(Default)]
pub(crate) enum TailChange {
    #[default]
    None,
    /// A live delivery. `from` is the delivered row when it left the tray instead of a
    /// Pending row: it grows into the room the tray gives up.
    Live { route: Route, from: Option<String> },
    Page,
}

pub(crate) static TAIL_CHANGE: opencode_kernel::Slot<TailChange> = opencode_kernel::Slot::new();

/// Each session's assistant steps that were open when its event stream dropped: their text grows
/// again only from whole text (`Transcript::apply_caught`, the catching-up barrier). Stored in
/// `App::kernel.slots`.
#[derive(Default)]
pub(crate) struct StreamGaps(pub(crate) HashMap<String, HashSet<String>>);

pub(crate) static STREAM_GAPS: opencode_kernel::Slot<StreamGaps> = opencode_kernel::Slot::new();

pub struct StreamEvent {
    pub ty: String,
    pub data: Value,
    pub done: Done,
    /// The notification and the sound the event calls for, which one window posts or plays.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub notice: Cell<Claim>,
    pub sound: Cell<Claim>,
}

/// Whether a window already took what an event calls for once (a notification, a sound). Each
/// window with the session's tab decides; the first to decide yes takes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Claim {
    #[default]
    Open,
    Taken,
}

impl Claim {
    /// `want`: this window would post it. The next state, and whether this window does.
    pub fn step(self, want: bool) -> (Claim, bool) {
        match (self, want) {
            (Claim::Open, true) => (Claim::Taken, true),
            (state, _) => (state, false),
        }
    }
}

impl StreamEvent {
    /// Whether this window takes `claim` for the event: it would, and no window did yet.
    pub fn take(claim: &Cell<Claim>, want: bool) -> bool {
        let (next, take) = claim.get().step(want);
        claim.set(next);
        take
    }
}

/// What `apply_shared` changed.
#[derive(Default)]
pub struct Done {
    /// Running sessions, idle and busy marks, requests (`apply_status`).
    pub status: bool,
    pub inbox: bool,
    /// The delivered item occupied the queue, not a Pending row in the transcript.
    pub queued_delivery: bool,
    /// A revert's mark moved.
    pub revert: bool,
    /// The session's agent or model, archive or metadata.
    pub info: bool,
    /// Selection confirmed by this event, before later events in the batch change it.
    pub selected: Option<Box<pickers::Selection>>,
    /// The session's transcript.
    pub transcript: bool,
    pub renamed: bool,
    pub project: bool,
}

impl App {
    /// A stream message into the server's copy, once (`windows::peer::heard`).
    pub(crate) fn apply_shared(&mut self, m: Incoming) -> Heard {
        match m {
            Incoming::Event { ty, data } => {
                let done = self.apply_event_shared(&ty, &data);
                Heard::Event(StreamEvent { ty, data, done, notice: Cell::default(), sound: Cell::default() })
            }
            Incoming::Opened => {
                crate::reconnect::request_active(self, false);
                Heard::Message(Box::new(Incoming::Opened))
            }
            Incoming::Reconnected => {
                self.resync_shared();
                Heard::Message(Box::new(Incoming::Reconnected))
            }
            other => Heard::Message(Box::new(other)),
        }
    }

    /// An event this window alone hears (e.g. fixture playback): both steps here.
    pub(crate) fn apply_event(&mut self, ty: &str, data: &Value) -> bool {
        let Heard::Event(e) = self.apply_shared(Incoming::Event { ty: ty.to_owned(), data: data.clone() }) else { return false };
        self.react(&e)
    }

    /// Only live deliveries opt in, never a history page, replay on open, or reconnect read.
    pub(crate) fn live_event(&mut self, ty: String, data: Value) -> bool {
        let Heard::Event(e) = self.apply_shared(Incoming::Event { ty, data }) else { return false };
        self.react_live(&e)
    }

    pub(crate) fn react_live(&mut self, e: &StreamEvent) -> bool {
        let ty = e.ty.as_str();
        if !matches!(self.kernel.slots.untracked(&TAIL_CHANGE), TailChange::Page)
            && let Some(id) = e.data["sessionID"].as_str()
            && self.showing_here(id)
        {
            // A queued delivery leaves the tray for the transcript: its row takes the tray's room.
            let from = if self.settings.smooth_timeline && e.done.queued_delivery {
                e.data["inboxID"].as_str().map(|id| format!("u-{id}"))
            } else {
                None
            };
            let live = (e.done.transcript || e.done.inbox)
                && (ty.starts_with("session.text.") || ty.starts_with("session.reasoning.") || ty.starts_with("session.tool.")
                    || matches!(ty, "session.synthetic" | "session.user.message" | "session.prompted" | "session.inbox.delivered")
                    || (ty == "session.inbox.enqueued" && self.waiting_finish(id, &e.data)));
            let is_none = !matches!(self.kernel.slots.untracked(&TAIL_CHANGE), TailChange::Live { .. } | TailChange::Page);
            if from.is_some() || (live && is_none) {
                let route = (*self.route).clone();
                *self.kernel.slots.get_mut(&TAIL_CHANGE) = TailChange::Live { route, from };
            }
        }
        self.react(e)
    }

    fn showing_here(&self, sid: &str) -> bool {
        matches!(&*self.route, Route::Session { server, id } if id == sid && *server == servers::runtime::current())
    }

    fn waiting_finish(&self, sid: &str, data: &Value) -> bool {
        use session_view::TimelineHost as _;
        use session_view::detail::{Placement, notice_placement};
        let Some(id) = data["inboxID"].as_str() else { return false };
        self.session_revert(sid).is_none()
            && self.inboxes.get(sid).is_some_and(|i| i.waiting_notices().any(|n| n.id == id
                && matches!(n.kind, opencode_kit::model::NoticeKind::Finished(_) | opencode_kit::model::NoticeKind::Job { script: Some(_), .. })
                && notice_placement(n, &self.timeline_detail()) != Placement::Hidden))
    }

    pub(crate) fn tail_page(&mut self, sid: &str) {
        if self.showing_here(sid) { *self.kernel.slots.get_mut(&TAIL_CHANGE) = TailChange::Page; }
    }

    pub(crate) fn finish_tail_change(&mut self, tx: &mut Tx) {
        let TailChange::Live { route, from } = std::mem::take(self.kernel.slots.get_mut(&TAIL_CHANGE)) else { return };
        let Route::Session { id, .. } = &route else { return };
        let has_gap = self.kernel.slots.untracked(&STREAM_GAPS).0.contains_key(id);
        if *self.route != route || !self.timeline_fx.ends.at_end(id) || self.timeline_fx.ends.due(id) || has_gap { return; }
        let stack = format!("scroll-{id}/timeline");
        if !session_view::TimelineHost::smooth_timeline(self) {
            // The shipped live-row growth and reordering stay on in either presentation.
            tx.grow_visible_tail(stack);
            return;
        }
        if let Some(from) = from { tx.grow_visible_tail_from(stack.clone(), from, "dock/queue-room"); }
        tx.grow_visible_tail_with_ink(stack);
    }

    fn apply_event_shared(&mut self, ty: &str, data: &Value) -> Done {
        let sid = data["sessionID"].as_str().unwrap_or("").to_owned();
        let now = *self.now_ms;
        let mut done = Done { status: self.apply_status(ty, data, &sid), ..Done::default() };
        if ty.starts_with("session.inbox.") && !sid.is_empty() {
            done.queued_delivery = ty == "session.inbox.delivered" && data["inboxID"].as_str().is_some_and(|id|
                self.inboxes.get(&sid).is_some_and(|i| i.queued().any(|item| item.id == id)));
            done.inbox = self.apply_inbox(ty, data, &sid);
        }
        if ty.starts_with("session.revert.") && !sid.is_empty() {
            done.revert = self.apply_revert(ty, data, &sid);
        }
        done.selected = self.apply_selected(ty, data, &sid);
        done.info = done.selected.is_some() | self.apply_archive(ty, data, &sid);
        if ty == "session.usage.updated" {
            done.info |= self.usage_updated(&sid, data["cost"].as_f64());
        }
        let len = self.transcripts.get(&sid).map_or(0, |t| t.messages.len());
        if let Some(t) = self.transcript_edits.get_mut(&mut self.transcripts, &sid) {
            let (changed, reread) = if self.kernel.slots.untracked(&STREAM_GAPS).0.contains_key(&sid) {
                let gaps = self.kernel.slots.get_mut(&STREAM_GAPS);
                let Some(gapped) = gaps.0.get_mut(&sid) else { unreachable!("checked contains_key") };
                let res = t.apply_caught(ty, data, now, gapped);
                if gapped.is_empty() {
                    gaps.0.remove(&sid);
                }
                res
            } else {
                (t.apply(ty, data, now), false)
            };
            done.transcript = changed;
            if reread {
                self.reload_session(&sid);
            }
        }
        if done.transcript {
            let at = self.transcripts.get(&sid).map_or(0, |t| touched_at(t, ty, data, len));
            self.transcript_edits.touched(at);
        }
        // A user's message can reach the transcript before the separate inbox delivery.
        // Once the echo landed, the steer is that row, not another copy below Working.
        if done.transcript && matches!(ty, "session.user.message" | "session.prompted")
            && let Some(id) = data["messageID"].as_str()
            && self.inboxes.get(&sid).is_some_and(|i| i.steering().any(|item| item.id == id))
            && let Some(item) = self.inboxes.get_mut(&sid).and_then(|i| i.remove(id))
        {
            if !item.files.is_empty() { opencode_kit::model::set_user_files(id, item.files); }
            done.inbox = true;
        }
        if ty == "session.renamed" && let Some(title) = data["title"].as_str()
            && let Some(session) = self.sessions.iter_mut().find(|s| s.id == sid) {
            session.title = title.to_owned();
            done.renamed = true;
        }
        // The session runs in another folder now, and maybe another project (`session.moved` in
        // `packages/client/src/solid/data.ts`): its header, its panels and its location card follow.
        if ty == "session.moved" && let Some(directory) = data["location"]["directory"].as_str()
            && let Some(session) = self.sessions.iter_mut().find(|s| s.id == sid) {
            if session.directory != directory {
                session.directory = directory.to_owned();
                done.info = true;
            }
            if let Some(project) = data["projectID"].as_str().filter(|p| !p.is_empty() && *p != session.project) {
                session.project = project.to_owned();
                done.info = true;
            }
        }
        // A desktop attached to the session's browser, or the last one let it go (`data.ts`).
        if ty == "session.permissions"
            && let Some(session) = self.sessions.iter_mut().find(|s| s.id == sid) {
            session.browser = opencode_kit::model::BrowserGrant::of(&data["permissions"]);
        }
        if crate::recent_page::reloads_on(ty, data) {
            self.schedule_recent_reload();
        }
        if ty == "project.updated"
            && let Some(updated) = opencode_kit::model::parse_project(data)
        {
            match self.projects.iter_mut().find(|p| p.id == updated.id && updated.id != "global") {
                Some(p) => *p = updated,
                None => self.projects.push(updated),
            }
            self.sort_projects();
            done.project = true;
        }
        done
    }

    /// Running sessions, read state and pending requests, in the server's copy.
    fn apply_status(&mut self, ty: &str, data: &Value, sid: &str) -> bool {
        let id = |v: &Value| v.as_str().unwrap_or("").to_owned();
        match ty {
            "session.execution.started" => {
                self.stops.active_sync.note_execution(sid, true);
                if let Some(missing) = self.missing_ancestor(sid) {
                    self.fetch_info(&missing);
                }
                self.stops.forget(sid);
                crate::rewind::take_back::forget(self, sid);
                // The run's start, as the server stamps `time.busy`: the inbox's Working age.
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.busy = Some(data["eventCreated"].as_i64().unwrap_or(*self.now_ms));
                }
                self.running.insert(sid.to_owned())
            }
            "session.execution.succeeded" | "session.execution.failed" | "session.execution.interrupted" => {
                self.stops.active_sync.note_execution(sid, false);
                let was = self.running.remove(sid);
                crate::stop::confirmed(self, sid);
                crate::rewind::take_back::run_ended(self, sid);
                if ty == "session.execution.interrupted" && data["reason"] == "shutdown" {
                    return was;
                }
                if let Some(s) = self.sessions.iter_mut().find(|s| s.id == sid) {
                    s.idle = Some(data["eventCreated"].as_i64().unwrap_or(*self.now_ms));
                }
                // The server's own idle mark, like Desktop's re-read after a run ends.
                if let Source::Live { requests: Some(r), .. } = &*self.source {
                    r.send(Request::Info(sid.to_owned())).ok();
                }
                true
            }
            "session.viewed" => {
                let idle = data["idle"].as_i64();
                self.sessions.iter_mut().find(|s| s.id == sid).is_some_and(|s| {
                    s.viewed = s.viewed.max(idle);
                    true
                })
            }
            "permission.asked" | "form.created" => {
                let parsed = if ty == "form.created" { opencode_kit::model::parse_form(data) } else { opencode_kit::model::parse_permission(data) };
                let Some(p) = parsed else { return false };
                self.request_reads.note_sse(p.id());
                self.add_pending(p)
            }
            "permission.replied" | "form.replied" | "form.cancelled" => {
                let rid = if ty == "permission.replied" { id(&data["requestID"]) } else { id(&data["id"]) };
                let had = self.requests.iter().any(|(_, r)| *r == rid) || self.pending.iter().any(|p| p.id() == rid);
                self.pending.retain(|p| p.id() != rid);
                self.requests.retain(|(_, r)| *r != rid);
                self.request_reads.forget_sse(&rid);
                crate::notifications::answered(self, &rid);
                had
            }
            _ => false,
        }
    }

    /// `session.revert.staged` hides the transcript from its boundary on, `cleared` shows it again,
    /// and `committed` ends the revert and drops the steers waiting behind it. The transcript's
    /// own rows go in `Transcript::apply`.
    fn apply_revert(&mut self, ty: &str, data: &Value, sid: &str) -> bool {
        // The stream's truth: the window that asked keeps its own mark over a stale echo
        // (`revert_landed`).
        let Some(next) = crate::rewind::reported(ty, data) else {
            return false;
        };
        let mut changed = false;
        if let Some(info) = self.sessions.iter_mut().find(|i| i.id == sid) {
            changed |= std::mem::replace(&mut info.revert, next) != info.revert;
        }
        if ty == "session.revert.committed"
            && let Some(to) = data["to"].as_str()
            && let Some(inbox) = self.inboxes.get_mut(sid)
        {
            let before = inbox.items.len();
            inbox.items.retain(|i| i.id.as_str() < to);
            changed |= inbox.items.len() != before;
        }
        changed
    }

    /// Whether `sid`'s transcript is on screen: the open session's family (its page, agent
    /// rows and subagent panels) is the only place transcripts draw.
    fn shows_transcript(&self, sid: &str) -> bool {
        matches!(&*self.route, Route::Session { id: open, .. } if self.root_of(open) == self.root_of(sid))
    }

    /// An event the server's copy took, in this window: what it shows and does. True when its
    /// view changed.
    pub(crate) fn react(&mut self, e: &StreamEvent) -> bool {
        let (ty, data, done) = (e.ty.as_str(), &e.data, &e.done);
        let sid = data["sessionID"].as_str().unwrap_or("").to_owned();
        if matches!(*self.source, Source::Fixture) { self.fixture_run.served.record(ty, data); }
        crate::plugin_host::server_event(self, ty, data);
        let mut changed = self.note_agent(ty, data, &sid);
        changed |= done.status | self.status_landed(ty, data);
        changed |= self.note_review_event(ty, data);
        if ty == "filesystem.changed" {
            crate::files::watch::changed(self, data);
        }
        if ty.starts_with("pty.") || ty.starts_with("persistent-pty.") {
            changed |= self.apply_pty_event(ty, data);
        }
        crate::notifications::event(self, e, &sid);
        crate::sounds::event(self, e, &sid);
        crate::speech::event(self, ty, data, &sid);
        if ty.starts_with("session.inbox.") && !sid.is_empty() {
            changed |= done.inbox | self.inbox_landed(ty, data, &sid);
            crate::background::admitted(self, ty, data, &sid);
        }
        if ty.starts_with("shell.") {
            changed |= crate::subagents::shells::event(self, ty, data);
        }
        if opencode_kit::model::is_job_event(ty) {
            changed |= crate::subagents::jobs::event(self, ty, data);
        }
        if ty.starts_with("session.revert.") && !sid.is_empty() {
            changed |= done.revert | self.revert_landed(ty, data, &sid);
        }
        if let Some(selected) = &done.selected {
            self.selected_landed(&sid, selected);
        }
        changed |= done.info;
        // What the tab saved of its session (its folder) follows the move.
        if ty == "session.moved" && done.info { self.note_tab_titles(); }
        if matches!(ty, "session.archived" | "session.unarchived") { self.session_search.invalidate(); }
        if ty == "session.text.delta" && self.shows_transcript(&sid) {
            crate::telemetry::streamed(&self.window.id, &sid, data);
        }
        // A transcript shows only in its own family's page, so another session's stream
        // updates its state without rebuilding the view.
        changed |= done.transcript && self.shows_transcript(&sid);
        if ty == "session.step.failed" {
            changed |= crate::free_limit::on_step_failed(self, data);
            if matches!(&*self.route, Route::Session { id, .. } if id == &sid) {
                changed |= crate::connect::on_step_failed(self, data);
            }
        }
        if ty.starts_with("session.") && !sid.is_empty() {
            // A step started in the open session may follow a prompt we haven't seen.
            if ty == "session.step.started" && self.route == Route::here(sid.clone()) {
                self.reload_session(&sid);
            }
        }
        if ty == "worktree.updated" && let Some(project) = data["projectID"].as_str() {
            self.worktree_updated(project);
        }
        if ty == "plugin.updated" {
            let loc = data["eventLocation"]["directory"].as_str();
            crate::settings::pages::extensions::on_plugin_updated(self);
            crate::settings::pages::project_extensions::on_updated(self, loc);
            #[cfg(not(target_arch = "wasm32"))]
            {
                let active_server = crate::servers::runtime::active(self).clone();
                crate::surfaces::browser::rpc::on_plugin_updated(self, &active_server);
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        if ty == opencode_kit::browser_rpc::CONTROL_EVENT {
            let active_server = crate::servers::runtime::active(self).clone();
            crate::surfaces::browser::rpc::on_control_event(self, &active_server, data);
            changed = true;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if matches!(ty, "session.created" | "session.moved" | "session.deleted") && !sid.is_empty() {
            let active_server = crate::servers::runtime::active(self).clone();
            crate::surfaces::browser::rpc::on_session_lifecycle(self, &active_server, ty, &sid);
        }
        if let Some(list) = match ty {
            "provider.updated" => Some(crate::connect::PickerList::Providers),
            "model.updated" => Some(crate::connect::PickerList::Models),
            "agent.updated" => Some(crate::connect::PickerList::Agents),
            "config.updated" => Some(crate::connect::PickerList::Config),
            _ => None,
        } {
            // Only react to global updates or updates for the active/default location, and
            // coalesce bursts per single resource (`packages/app/src/runtime/server/sync.tsx:163`,
            // `packages/client/src/solid/data.ts:346-356,1460-1528`).
            let loc = data["eventLocation"]["directory"].as_str();
            let active_dir = match &*self.route {
                Route::Session { id, .. } => self.session(id).map(|i| i.directory.as_str()),
                Route::Draft(_) => Some(self.draft_dir()),
                Route::Home | Route::Automations => None,
            };
            if loc.is_none() || loc == crate::start_location::server_directory(self) || loc == active_dir {
                let now = *self.clock;
                crate::connect::picker_reads::schedule(self, list, now);
            }
        }
        if ty == "config.updated" {
            let loc = data["eventLocation"]["directory"].as_str();
            crate::settings::pages::extensions::on_config_updated(self);
            crate::settings::pages::project_extensions::on_updated(self, loc);
        }
        if done.project || done.renamed {
            self.session_search.invalidate();
            changed = true;
        }
        changed
    }

    /// After `apply_status`, in this window: its sidebar's request times, its question dock and
    /// replies on their way. A focused window reports the open session's new idle mark viewed
    /// (`session_viewed::poll`).
    fn status_landed(&mut self, ty: &str, data: &Value) -> bool {
        match ty {
            "vcs.branch.updated" => {
                let Some(dir) = data["eventLocation"]["directory"].as_str() else { return false };
                let branch = data["branch"].as_str().map(str::to_owned);
                let server = crate::servers::runtime::active(self).clone();
                self.sidebar_inbox.branches.updated(&server, dir, branch.clone());
                crate::view::location_card::branch_updated(self, &server, dir, branch.clone());
                self.note_review_branch_updated(dir, branch);
                true
            }
            "permission.asked" | "form.created" => {
                let id = if data["form"].is_object() { &data["form"]["id"] } else { &data["id"] };
                let at = data["eventCreated"].as_i64().unwrap_or(*self.now_ms);
                if let Some(id) = id.as_str() { self.sidebar_inbox.asked(id, at); }
                false
            }
            "permission.replied" | "form.replied" | "form.cancelled" => {
                let rid = if ty == "permission.replied" { &data["requestID"] } else { &data["id"] };
                let rid = rid.as_str().unwrap_or("");
                self.kernel.slots.get_mut(&crate::question::QUESTION).forget(rid);
                self.kernel.slots.get_mut(&crate::permission::PERMISSION_SENT).0.remove(rid);
                false
            }
            _ => false,
        }
    }

    /// After `apply_revert`, in this window: a revert it asked for keeps its own mark over a
    /// stale event's.
    fn revert_landed(&mut self, ty: &str, data: &Value, sid: &str) -> bool {
        let mut changed = false;
        if let Some(next) = crate::rewind::landed(self, ty, data, sid)
            && let Some(info) = self.sessions.iter_mut().find(|i| i.id == sid)
            && info.revert != next
        {
            info.revert = next;
            crate::windows::peer::touch(self);
            changed = true;
        }
        changed
    }
}

impl App {
    /// Archives and metadata changed from other clients.
    fn apply_archive(&mut self, ty: &str, data: &Value, sid: &str) -> bool {
        if !matches!(ty, "session.archived" | "session.unarchived" | "session.metadata.updated") {
            return false;
        }
        if ty == "session.unarchived" && self.session(sid).is_none() {
            // Archived before this window read its sessions: it comes back from the inbox's archived list.
            self.unarchive_session_locally(sid);
            return self.session(sid).is_some();
        }
        let Some(i) = self.sessions.iter_mut().find(|i| i.id == sid) else { return false };
        match ty {
            "session.archived" => {
                i.archived = Some(data["eventCreated"].as_i64().unwrap_or(*self.now_ms));
                let info = i.clone();
                self.inbox_file(info);
            }
            "session.unarchived" => i.archived = None,
            "session.metadata.updated" => i.metadata = data["metadata"].clone(),
            _ => return false,
        }
        true
    }
}
