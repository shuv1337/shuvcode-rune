//! Prompts sent while a session is busy, as OpenCode Desktop shows them. Enter steers: the
//! prompt shows at the end of the timeline, muted and marked Pending, until the server delivers
//! it at the turn's next step boundary. ⌘⏎ queues: the prompt waits in a panel above the
//! composer until the turn ends, where it can be steered (Send once the turn has stopped),
//! taken back into the composer, or removed.

use std::collections::HashMap;

use opencode_kit::client::{AgentInput, FileInput, PathAttachment, Prompt, SkillInput, Write};
use opencode_kit::model::{self, Attachment, Delivery, Inbox, InboxChange, InboxItem, Message, PromptComment, Transcript};
use opencode_kit::wire_error::WireError;
use rune::machine::Attempt;
use rune::{TextEdit, Tx};
use opencode_ui::prompt_history::Entry;

pub mod drag;
pub mod edit;
pub mod machine;
pub mod run;
pub mod view;

pub use edit::{base_rows, composer_draft, confirm_edit, edit_queued, editing, key, leave_edits, rows};
pub use run::{QueueJob, QueueReply, RewriteTask, Run, Runs, apply_reply, execute_rewrite};
pub use view::{alternate_hint, panel, pending_label};

use crate::send_failed::{self, SendFailedHost, Sent, State as SendState};
use machine::{Effect, Event, Operation, Stage};

/// Composer mention wire encoding needed when confirming a queued prompt edit.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditWire {
    pub text: String,
    pub display: String,
    pub agents: Vec<AgentInput>,
    pub skills: Vec<SkillInput>,
    pub files: Vec<FileInput>,
}

/// Object-safe host boundary for the session prompt queue strip, edit-in-composer workflow,
/// drag-to-reorder, and inbox SSE/listing reconciliation.
///
/// What became of a write offered quietly (`QueueHost::write_quiet`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Offered {
    /// Handed to the server's writer.
    Sent,
    /// The window has no writer for it.
    NoWriter,
    /// The build may not send.
    ReadOnly,
}

/// Every state transition, timer poll, key handler, and inbox event handler operates on
/// `&dyn QueueHost` / `&mut dyn QueueHost`. Generic `H: QueueHost + 'static` bounds appear
/// strictly on `rune::El` view wrappers in [`view`].
pub trait QueueHost: opencode_kernel::AppEnv + opencode_kernel::AppFeedback {
    /// Per-session server inbox items and waiting notices.
    fn inboxes(&self) -> &HashMap<String, Inbox>;
    /// Mutable per-session inbox map.
    fn inboxes_mut(&mut self) -> &mut HashMap<String, Inbox>;

    /// Loaded session transcripts (used to settle delivered inbox items and notices).
    fn transcripts(&self) -> &HashMap<String, Transcript>;
    /// Mutable session transcripts.
    fn transcripts_mut(&mut self) -> &mut HashMap<String, Transcript>;

    /// Per-session queue operation state machines.
    fn queue_ops(&self) -> &HashMap<String, machine::Queue>;
    /// Mutable per-session queue operation state machines.
    fn queue_ops_mut(&mut self) -> &mut HashMap<String, machine::Queue>;

    /// Active pointer/keyboard drag state for queue row reordering.
    fn queue_drag(&self) -> &drag::Drag;
    /// Mutable drag state.
    fn queue_drag_mut(&mut self) -> &mut drag::Drag;

    /// In-flight queue rewrite chains by `(session, attempt)`.
    fn queue_runs(&self) -> &run::Runs;
    /// Mutable in-flight rewrite registry.
    fn queue_runs_mut(&mut self) -> &mut run::Runs;

    /// Whether the app is running against an offline fixture (mutates locally without a server).
    fn is_fixture(&self) -> bool {
        false
    }
    /// Whether this window may mutate live server state (live + `allow_send` + no storybook stage).
    fn can_write(&self) -> bool;
    /// Whether this live window is read-only (`--read-only` or unattended).
    fn is_read_only(&self) -> bool {
        false
    }
    /// Sends a server write; returns `false` when the write could not be queued.
    fn write(&mut self, write: Write) -> bool;
    /// [`Self::write`] without the host's own toast when the write can't go: the queue machine
    /// says so itself, once.
    fn write_quiet(&mut self, write: Write) -> Offered {
        if self.write(write) { Offered::Sent } else { Offered::NoWriter }
    }
    /// Builds the write used to send `prompt` to `session` (`Send`).
    fn send_write(&self, session: &str, prompt: Prompt) -> Write {
        Write::Send { session: session.to_owned(), prompt }
    }

    /// Active composer text editor.
    fn composer(&self) -> &TextEdit;
    /// Mutable active composer text editor.
    fn composer_mut(&mut self) -> &mut TextEdit;
    /// Composer file attachments currently staged.
    fn composer_files(&self) -> &[Attachment];
    /// Takes all staged composer file attachments.
    fn take_composer_files(&mut self) -> Vec<Attachment>;
    /// Composer path attachments currently staged.
    fn composer_paths(&self) -> &[PathAttachment];
    /// Restores path attachments back onto the composer (e.g. when admission fails).
    fn extend_composer_paths(&mut self, paths: Vec<PathAttachment>);
    /// Appends file and path attachments onto the composer (e.g. undo / failed edit merge).
    fn extend_composer_attachments(&mut self, files: Vec<Attachment>, paths: Vec<PathAttachment>);
    /// Replaces staged composer file and path attachments (e.g. edit load/restore).
    fn set_composer_attachments(&mut self, files: Vec<Attachment>, paths: Vec<PathAttachment>);
    /// Whether the composer is currently in shell mode.
    fn composer_shell(&self) -> bool {
        false
    }
    /// Synchronizes the composer's shell mode when loading or restoring an edit stash.
    fn sync_composer_shell(&mut self, shell: bool) {
        let _ = shell;
    }
    /// Takes all staged quote comments from the composer on admission.
    fn take_composer_comments(&mut self) -> Vec<PromptComment> {
        Vec::new()
    }
    /// Clears pending code/diff line comments for `session` on admission.
    fn clear_line_comments(&mut self, session: &str) {
        let _ = session;
    }
    /// Merges text and attachments into the saved/parked draft of a non-shown `session`.
    fn merge_parked_draft(&mut self, session: &str, text: String, files: Vec<Attachment>, paths: Vec<PathAttachment>) {
        let _ = (session, text, files, paths);
    }
    /// Encodes current composer mentions into wire fields for confirming an edited queued prompt.
    fn edit_wire(&self) -> EditWire;

    /// Whether `session` currently has a staged revert boundary.
    fn has_staged_revert(&self, session: &str) -> bool {
        let _ = session;
        false
    }
    /// Whether a blocking dock (question or permission) currently covers the composer.
    fn dock_shown(&self) -> bool {
        false
    }

    /// Notified when a steer prompt or steered edit is admitted for `session`.
    fn on_steered(&mut self, session: &str, id: &str) {
        let _ = (session, id);
    }
    /// Notified when an own user prompt is enqueued via SSE (for recent-session recency).
    fn on_prompt_accepted(&mut self, session: &str, id: &str) -> bool {
        let _ = (session, id);
        false
    }
    /// Notified after `session`'s inbox syncs (e.g. rewind state invalidation).
    fn on_inbox_changed(&mut self, session: &str) {
        let _ = session;
    }
    /// Marks shared server peer data dirty so sibling windows repaint.
    fn on_peer_touch(&mut self) {}
    /// Offers an undo toast for a removed queued prompt.
    fn offer_undo_removed(&mut self, session: &str, text: String, files: Vec<Attachment>) {
        let _ = (session, text, files);
    }
    /// Shows a wire-error failure toast.
    fn toast_failed_wire(&mut self, title: &str, error: &WireError);
    /// Schedules an off-thread rewrite job or next-poll deferred failure reply.
    fn spawn_queue_job(&mut self, job: QueueJob);
    /// Submits the composer with alternate delivery (triggered by [`alternate_hint`]).
    fn submit_alternate(&mut self, tx: &mut Tx) {
        let _ = tx;
    }

    /// Sibling durable-outbox host interface when the host also participates in `send_failed`.
    fn send_failed(&self) -> Option<&dyn SendFailedHost> {
        None
    }
    /// Mutable sibling durable-outbox host interface.
    fn send_failed_mut(&mut self) -> Option<&mut dyn SendFailedHost> {
        None
    }
}

/// The open session's undelivered prompts.
pub fn inbox_for<'a>(host: &'a dyn QueueHost, session: &str) -> Option<&'a Inbox> {
    host.inboxes().get(session).filter(|i| !i.items.is_empty() || i.waiting_notices().next().is_some())
}

/// Admits a prompt sent while the session is busy: shown at once, as Desktop does, and
/// replaced by the server's copy when `session.inbox.enqueued` echoes it. A steer shows at
/// once where it rests, flying nowhere, and the queue panel rises from behind the composer.
pub fn admit(host: &mut dyn QueueHost, session: &str, prompt: Prompt, delivery: Delivery) {
    admit_with(host, session, prompt, delivery, false);
}

/// Like [`admit`], with `first = true` for `Alt+Enter` queue-first (`#63109`): queues the prompt
/// at the front of the durable outbox and moves it ahead of existing queued prompts once admitted.
pub fn admit_with(host: &mut dyn QueueHost, session: &str, mut prompt: Prompt, delivery: Delivery, first: bool) {
    prompt.delivery = Some(delivery);
    let id = prompt.id.clone().unwrap_or_default();
    let files = host.composer_files().to_vec();
    let item = InboxItem {
        id: id.clone(),
        text: prompt.display_text.clone().unwrap_or_else(|| prompt.text.clone()),
        delivery,
        files: files.clone(),
    };
    let sent = Sent::new(session, &id, Entry::of(host.composer()), files, prompt.attachments.clone(), true).with_prompt(&prompt);
    let write = host.send_write(session, prompt);
    let journaled = host.can_write() && host.send_failed().is_some();
    if host.can_write() {
        let paths = sent.paths.clone();
        let ok = match host.send_failed_mut() {
            Some(sf) if first => send_failed::enqueue_prompt_first(sf, sent, write),
            Some(sf) => send_failed::enqueue_prompt(sf, sent, write),
            None => inbox_write(host, write),
        };
        if !ok {
            host.extend_composer_paths(paths);
            return;
        }
    } else {
        if !inbox_write(host, write) {
            return;
        }
        if let Some(sf) = host.send_failed_mut() {
            sf.unsent_mut().note(sent);
        }
    }
    let taken_files = host.take_composer_files();
    model::set_user_files(&id, taken_files);
    let taken_comments = host.take_composer_comments();
    model::set_user_comments(&id, taken_comments);
    host.clear_line_comments(session);
    let waiting = host.send_failed().is_some_and(|sf| sf.unsent().state(&id) == Some(SendState::Waiting));
    if waiting {
        if let Some(sf) = host.send_failed_mut() {
            send_failed::show_outbox(sf);
        }
    } else {
        host.inboxes_mut().entry(session.to_owned()).or_default().upsert(item);
    }
    host.composer_mut().clear();
    if delivery == Delivery::Steer {
        host.on_steered(session, &id);
    } else if first && !journaled {
        // Fixtures and non-journaled hosts admit synchronously, so move to front at once.
        first_queued(host, session, &id, Vec::new(), false);
    }
}

/// Moves an admitted queued prompt ahead of existing queued prompts (`Alt+Enter`, `#63109`),
/// keeping it behind any still-queued `after` anchors from earlier queue-first prompts in the
/// same offline drain.
pub fn first_queued(host: &mut dyn QueueHost, session: &str, id: &str, after: Vec<String>, force: bool) {
    let reverted = host.has_staged_revert(session);
    let now = host.clock();
    step_queue(host, session, Event::FirstRequested { id: id.to_owned(), after, reverted, force, now });
}

/// Steers the first queued prompt when the normal-mode composer is empty (`Mod+Enter`, `#63111`).
/// Only acts while the queue is idle so a second press never skips a busy first row.
pub fn steer_first(host: &mut dyn QueueHost, session: &str) {
    if host.queue_ops().get(session).is_some_and(|q| !matches!(q.state, machine::State::Idle)) {
        return;
    }
    let Some(first) = edit::rows(host, session).into_iter().next().filter(|r| !r.pending) else {
        return;
    };
    let promoted_waiting = host
        .send_failed_mut()
        .is_some_and(|sf| send_failed::steer_waiting_outbox(sf, session, &first.id));
    if promoted_waiting {
        host.on_steered(session, &first.id);
        host.on_peer_touch();
        return;
    }
    let now = host.clock();
    step_queue(host, session, Event::SteerFirstRequested { now });
}

/// Steers a queued prompt (sends it now when the turn has stopped).
pub fn steer_queued(host: &mut dyn QueueHost, session: &str, id: &str) {
    queue_op(host, session, Operation::Steer { id: id.to_owned() });
}

/// Moves a pending steer back to the queue (its menu's Move to queue).
pub fn queue_pending(host: &mut dyn QueueHost, session: &str, id: &str) {
    queue_op(host, session, Operation::Requeue { id: id.to_owned() });
}

/// Removes a queued prompt.
pub fn withdraw(host: &mut dyn QueueHost, session: &str, id: &str) {
    let queued = host.inboxes().get(session).is_some_and(|i| i.queued().any(|i| i.id == id));
    let id = id.to_owned();
    queue_op(host, session, if queued { Operation::Remove { id } } else { Operation::Delete { id } });
}

/// Undo on a removed queued prompt's toast: its words and files come back into `session`'s
/// composer after anything typed there (its saved draft when another page shows), ready to
/// queue again. Re-queued by itself it would go with today's agent and model, not its own.
pub fn unwithdraw(host: &mut dyn QueueHost, session: &str, text: String, files: Vec<Attachment>) {
    let _ = edit::apply_draft_effect(host, session, Effect::Merge { text, files, paths: Vec::new() });
}

/// Withdraws a queued prompt into the composer, after anything already typed there, once the
/// server lets it go; until then its row shows it pending.
pub fn undo_queued(host: &mut dyn QueueHost, session: &str, id: &str) {
    let Some(item) = host.inboxes().get(session).and_then(|i| i.items.iter().find(|i| i.id == id)).cloned() else { return };
    // An item listed by the server has its files only on the item.
    let files = model::user_files(&item.id);
    let files = if files.is_empty() { item.files } else { files.to_vec() };
    let (shell, now) = (host.composer_shell(), host.clock());
    step_queue(host, session, Event::UndoRequested { id: item.id, text: item.text, files, shell, now });
}

/// A row dropped in a new place: the queue is rewritten in `ids`' order (`reorder.requested`).
pub fn reorder_queued(host: &mut dyn QueueHost, session: &str, ids: Vec<String>) {
    let reverted = host.has_staged_revert(session);
    let (current, now) = (edit::rows(host, session), host.clock());
    step_queue(host, session, Event::ReorderRequested { ids, reverted, rows: current, now });
}

/// Fixture runs act locally, as if the server agreed.
fn inbox_write(host: &mut dyn QueueHost, w: Write) -> bool {
    host.is_fixture() || host.write(w)
}

/// Asks `session`'s queue machine for `operation`.
fn queue_op(host: &mut dyn QueueHost, session: &str, operation: Operation) {
    let now = host.clock();
    step_queue(host, session, Event::Requested { operation, now });
}

/// Steps `session`'s queue state machine and applies every resulting effect.
pub fn step_queue(host: &mut dyn QueueHost, session: &str, event: Event) {
    if let Event::Moved { moved, .. } = &event
        && let Some(sf) = host.send_failed_mut()
    {
        send_failed::remap_first_after(sf, session, moved);
    }
    let was_busy = host.queue_ops().get(session).is_some_and(machine::Queue::busy);
    let current = edit::rows(host, session);
    let base = edit::base_rows(host, session);
    let effects = host.queue_ops_mut().entry(session.to_owned()).or_default().step(event, &current, &base);
    for effect in effects {
        let Some(effect) = edit::apply_draft_effect(host, session, effect) else { continue };
        match effect {
            Effect::Run { attempt, operation } => run::run_queue_op(host, session, attempt, operation),
            Effect::Abort { attempt } => host.queue_runs().abort(session, attempt),
            Effect::Failed(Some(e)) => host.toast_failed_wire(opencode_i18n::t!("rune.queue.changeFailed"), &e),
            // Never sent: read-only keeps our read-only words; no writer says not connected.
            Effect::Failed(None) if host.is_read_only() => host.toast_fail(
                opencode_i18n::t!("rune.send.nothingSent").into(),
                opencode_i18n::t!("rune.jobs.readOnly").into(),
            ),
            Effect::Failed(None) => host.toast_fail(
                opencode_i18n::t!("rune.queue.changeFailed").into(),
                opencode_i18n::t!("rune.send.disconnected").into(),
            ),
            Effect::Reverted => host.toast_say(opencode_i18n::t!("session.queue.reverted").into(), String::new()),
            Effect::UndoShell => host.toast_say(opencode_i18n::t!("session.queue.undoShell").into(), String::new()),
            Effect::Load(_) | Effect::Restore { .. } | Effect::Merge { .. } => {}
        }
    }
    let now_busy = host.queue_ops().get(session).is_some_and(machine::Queue::busy);
    if was_busy
        && !now_busy
        && let Some(sf) = host.send_failed_mut()
    {
        send_failed::pump_outbox(sf);
    }
}

/// `Incoming::Settled` for a queue change of `session`. A change the server took shows at
/// once, as its event will say; one it refused says why (none when it was never sent).
pub fn queue_settled(host: &mut dyn QueueHost, session: &str, attempt: u32, result: Result<(), Option<WireError>>) {
    let (attempt, now) = (Attempt(attempt), host.clock());
    let Some((a, active)) = host.queue_ops().get(session).and_then(|q| q.running()).map(|(a, o)| (a, o.clone())) else { return };
    if a != attempt {
        return;
    }
    let why = result.err();
    if why.is_none() {
        let mut removed_for_undo = None;
        {
            let inbox = host.inboxes_mut().entry(session.to_owned()).or_default();
            match &active {
                Operation::Steer { id } => {
                    inbox.set_delivery(id, Delivery::Steer);
                }
                Operation::Requeue { id } => {
                    inbox.set_delivery(id, Delivery::Queue);
                }
                Operation::Remove { id } | Operation::Undo { id, .. } | Operation::Delete { id } => {
                    let item = inbox.items.iter().find(|i| i.id == *id).cloned();
                    inbox.remove(id);
                    // Removed (not taken back into the composer, which keeps it anyway): offered back.
                    if let (Some(item), false) = (item, matches!(active, Operation::Undo { .. })) {
                        let files = model::user_files(&item.id);
                        let files = if files.is_empty() { item.files } else { files.to_vec() };
                        removed_for_undo = Some((item.text, files));
                    }
                }
                Operation::Edit(_) | Operation::First { .. } | Operation::Reorder { .. } => {}
            }
        }
        if let Some((text, files)) = removed_for_undo {
            host.offer_undo_removed(session, text, files);
        }
        if let Operation::Undo { id, .. } = &active {
            model::set_user_files(id, Vec::new());
        }
        if let Operation::Steer { id } = &active {
            host.on_steered(session, id);
        }
        host.on_peer_touch();
        return step_queue(host, session, Event::Succeeded { attempt, now });
    }
    step_queue(host, session, Event::Failed { attempt, hidden: Vec::new(), why: why.flatten(), now });
}

/// Each frame: confirmed rows' hiding expires and a change past its bound ends as it got.
pub fn poll_timers(host: &mut dyn QueueHost, tx: &mut Tx) {
    let now = host.clock();
    let due: Vec<String> = host
        .queue_ops()
        .iter()
        .filter(|(_, q)| q.due().is_some_and(|d| now + 1e-9 >= d))
        .map(|(s, _)| s.clone())
        .collect();
    for session in due {
        let (stage, hidden) = match host.queue_ops().get(&session).and_then(|q| q.running()) {
            // An undo's cancel goes out at once, so it waits for its answer (`committed`).
            Some((_, Operation::Undo { .. })) => (Stage::Committed, Vec::new()),
            Some((attempt, _)) => host.queue_runs().progress(&session, attempt),
            None => (Stage::Running, Vec::new()),
        };
        step_queue(host, &session, Event::Tick { now, stage, hidden });
    }
    if let Some(next) = host.queue_ops().values().filter_map(machine::Queue::due).reduce(f64::min) {
        tx.wake_after(std::time::Duration::from_secs_f64((next - now).max(0.0)));
    }
}

/// `session.inbox.*` in the server's copy (`App::apply_shared`): a delivered prompt joins
/// the end of the transcript, where the server puts it.
pub fn apply_inbox(host: &mut dyn QueueHost, ty: &str, data: &serde_json::Value, sid: &str) -> bool {
    // SSE can admit our prompt before its write reply. Only our sends get the
    // composer's recency; compactions and other clients' prompts also arrive here.
    let accepted = if ty == "session.inbox.enqueued"
        && data["item"]["type"] == "user"
        && let Some(id) = data["inboxID"].as_str()
        && (host.send_failed().is_some_and(|sf| sf.unsent().state(id).is_some()) || host.is_fixture())
    {
        host.on_prompt_accepted(sid, id)
    } else {
        false
    };
    let change = host.inboxes_mut().entry(sid.to_owned()).or_default().apply(ty, data);
    finish_inbox(host, sid, change) || accepted
}

fn finish_inbox(host: &mut dyn QueueHost, sid: &str, change: InboxChange) -> bool {
    // The server admits every prompt to its inbox, a send to an idle session too. That one
    // already shows as its prompt, which is where it stays, so it never waits as a steer.
    let seen: Vec<String> = match (host.inboxes().get(sid), host.transcripts().get(sid)) {
        (Some(inbox), Some(t)) => inbox
            .items
            .iter()
            .map(|i| i.id.as_str())
            .chain(inbox.waiting_notices().map(|n| n.id.as_str()))
            .filter(|id| t.messages.iter().any(|m| m.id() == *id))
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    let delivered = {
        let Some(inbox) = host.inboxes_mut().get_mut(sid) else { return false };
        if !seen.is_empty() {
            inbox.items.retain(|i| !seen.iter().any(|id| id == &i.id));
            inbox.settle_notices(|id| seen.iter().any(|s| s == id));
        }
        inbox.take_notice_effects()
    };
    // A background shell or subagent's notice the server delivered keeps its row as a message.
    let notices = !delivered.is_empty();
    for model::NoticeEffect::ToTranscript(n) in delivered {
        if let Some(t) = host.transcripts_mut().get_mut(sid)
            && !t.messages.iter().any(|m| m.id() == n.id)
        {
            t.messages.push(Message::Notice(n));
        }
    }
    match change {
        InboxChange::None => notices,
        InboxChange::Changed => true,
        InboxChange::Delivered(item) => {
            if !item.files.is_empty() {
                model::set_user_files(&item.id, item.files);
            }
            if let Some(t) = host.transcripts_mut().get_mut(sid)
                && !t.messages.iter().any(|m| m.id() == item.id)
            {
                t.messages.push(Message::User { id: item.id, text: item.text });
            }
            true
        }
    }
}

/// After `apply_inbox`, in each window: its own sends the event admits or delivers.
pub fn inbox_landed(host: &mut dyn QueueHost, ty: &str, data: &serde_json::Value, sid: &str) -> bool {
    let id = data["inboxID"].as_str().unwrap_or("");
    let admitted = match (ty, host.send_failed_mut()) {
        ("session.inbox.enqueued", Some(sf)) => send_failed::admitted_send(sf, id),
        ("session.inbox.delivered", Some(sf)) => send_failed::accept_send(sf, id),
        _ => false,
    };
    sync_queue(host, sid);
    admitted
}

/// Replaces `sid`'s inbox with a freshly listed server snapshot and reconciles pending sends.
pub fn inbox_listed(host: &mut dyn QueueHost, sid: &str, rows: &[serde_json::Value]) {
    let dropped: Vec<InboxItem> = {
        let inbox = host.inboxes_mut().entry(sid.to_owned()).or_default();
        let previous = std::mem::take(&mut inbox.items);
        inbox.replace(rows);
        previous.into_iter().filter(|o| !inbox.items.iter().any(|i| i.id == o.id)).collect()
    };
    let kept = host.send_failed().map(|sf| sf.unsent().keep_in_inbox(sid, dropped)).unwrap_or_default();
    if !kept.is_empty() {
        let inbox = host.inboxes_mut().entry(sid.to_owned()).or_default();
        for item in kept {
            if !inbox.items.iter().any(|i| i.id == item.id) {
                inbox.upsert(item);
            }
        }
    }
    finish_inbox(host, sid, InboxChange::Changed);
    host.on_peer_touch();
    let ids: Vec<_> = rows.iter().filter_map(|i| i["id"].as_str().map(str::to_owned)).collect();
    if let Some(sf) = host.send_failed_mut() {
        send_failed::judge_sends_by_inbox(sf, sid, &ids);
        send_failed::check_sends(sf);
    }
    sync_queue(host, sid);
}

fn sync_queue(host: &mut dyn QueueHost, sid: &str) {
    // The queue hears the server's queued prompts whenever they change (`inbox.synced`).
    if let Some(queue) = host.queue_ops().get(sid) {
        let ids = host.inboxes().get(sid).map(|i| i.queued().map(|i| i.id.clone()).collect()).unwrap_or_default();
        let delivered = queue.doubted().is_some_and(|d| host.transcripts().get(sid).is_some_and(|t| t.messages.iter().any(|m| m.id() == d)));
        step_queue(host, sid, Event::Synced { ids, delivered });
    }
    host.on_inbox_changed(sid);
}

/// A fetched page that already has a prompt means its delivery event was missed.
pub fn settle_inbox(host: &mut dyn QueueHost, session: &str, fetched: &[String]) {
    if let Some(inbox) = host.inboxes_mut().get_mut(session) {
        inbox.items.retain(|i| !fetched.contains(&i.id));
        inbox.settle_notices(|id| fetched.iter().any(|f| f == id));
        inbox.take_notice_effects();
    }
}
