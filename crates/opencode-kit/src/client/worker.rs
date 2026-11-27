//! The connection's threads: the fetch worker answering `Request`s, the writer running
//! `Write`s and the event stream, each waking the window when something lands.

use std::ops::ControlFlow;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::PoisonError;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use rune::Waker;
use rune::machine::Effects;

use super::{Incoming, Request, Server, Write, encode_component, list};
use crate::model;
use crate::wire_error::{WireError, WireKind};

/// Initial retry delay after a drop or first failure, doubled up to [`BACKOFF_MAX_MS`].
pub(super) const BACKOFF_MIN_MS: u64 = 250;
pub(super) const BACKOFF_MAX_MS: u64 = 3_000;

/// Lifecycle of one `/api/event` stream worker.
///
/// Compared with the official app's `packages/client/src/solid/connection.ts`:
/// - Matches `defaultIdleTimeout = 45_000` (`EVENTS_IDLE`), gating readiness on the first
///   `server.connected` event, and distinguishing initial open (`Incoming::Opened`) from
///   reconnects (`Incoming::Reconnected`).
/// - Deliberately differs in backoff (`250 ms` doubling to `3_000 ms`, reset on `server.connected`
///   rather than official's flat `1_000 ms` reset after 1 s connected), native reach-watcher
///   parking (`PARKED = 30 s` while TCP refuses), and error-wake deduplication (`last_error`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum StreamState {
    /// First attempt of a fresh stream, awaiting HTTP headers and `server.connected`.
    /// Handshake emits `Incoming::Opened`.
    Initial,
    /// Subsequent attempt (after a failure, drop, or `streams::verify` takeover), awaiting HTTP
    /// headers and `server.connected`. Handshake emits `Incoming::Reconnected`.
    Reconnecting {
        /// Delay (ms) to wait if this attempt fails before opening.
        backoff_ms: u64,
        /// Failure last reported to the window; an identical consecutive failure skips `wake()`.
        last_error: Option<WireError>,
    },
    /// `server.connected` received; forwarding events to the inbox.
    Open,
    /// Waiting between failed or dropped attempts before opening the next connection.
    Waiting {
        /// Backoff (ms) carried into the next `Reconnecting` attempt (`(wait_ms * 2).min(3000)`).
        next_backoff_ms: u64,
        last_error: WireError,
    },
    /// Terminated (inbox closed or superseded by a newer epoch in `streams::verify`).
    Closed,
}

pub(super) enum StreamEvent {
    /// A non-heartbeat SSE `data:` frame parsed from the stream; `connected` is true iff its
    /// `type` is `"server.connected"`.
    Frame { connected: bool },
    /// The connection attempt failed, timed out, returned an unexpected response, rejected
    /// handshake, or ended (EOF).
    Failed(WireError),
    /// The retry sleep elapsed or was interrupted early by `wake_reconnects()`.
    WaitElapsed,
    /// The inbox closed or a newer `streams::verify` epoch took over.
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Handshake {
    Opened,
    Reconnected,
}

pub(super) enum StreamEffect {
    /// Forward `Incoming::Opened` or `Incoming::Reconnected` to `inbox`.
    DeliverHandshake(Handshake),
    /// Forward the current SSE frame to `inbox`.
    DeliverFrame,
    /// First event was not `server.connected`; close this response and feed `StreamEvent::Failed`.
    RejectHandshake(WireError),
    /// Report `Incoming::StreamDown(error)` to `inbox`, waking the window iff `wake`.
    ReportDown { error: WireError, wake: bool },
    /// Register the server with the reach watcher (no-op on `wasm32`).
    WatchReach,
    /// Wait `backoff_ms` (or `PARKED` on native when `server.refusing()`), interruptible by
    /// `wake_reconnects()`, then feed `StreamEvent::WaitElapsed`.
    Wait { backoff_ms: u64 },
}

pub(super) fn stream_step(state: &mut StreamState, event: StreamEvent) -> Effects<StreamEffect> {
    match (std::mem::replace(state, StreamState::Closed), event) {
        (_, StreamEvent::Closed) | (StreamState::Closed, _) => Effects::none(),
        (StreamState::Initial, StreamEvent::Frame { connected: true }) => {
            *state = StreamState::Open;
            Effects::two(StreamEffect::DeliverHandshake(Handshake::Opened), StreamEffect::DeliverFrame)
        }
        (StreamState::Initial, StreamEvent::Frame { connected: false }) => {
            *state = StreamState::Initial;
            Effects::one(StreamEffect::RejectHandshake(handshake_error()))
        }
        (StreamState::Reconnecting { .. }, StreamEvent::Frame { connected: true }) => {
            *state = StreamState::Open;
            Effects::two(StreamEffect::DeliverHandshake(Handshake::Reconnected), StreamEffect::DeliverFrame)
        }
        (StreamState::Reconnecting { backoff_ms, last_error }, StreamEvent::Frame { connected: false }) => {
            *state = StreamState::Reconnecting { backoff_ms, last_error };
            Effects::one(StreamEffect::RejectHandshake(handshake_error()))
        }
        (StreamState::Open, StreamEvent::Frame { .. }) => {
            *state = StreamState::Open;
            Effects::one(StreamEffect::DeliverFrame)
        }
        (StreamState::Initial | StreamState::Open, StreamEvent::Failed(error)) => {
            let backoff_ms = BACKOFF_MIN_MS;
            let next_backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
            *state = StreamState::Waiting { next_backoff_ms, last_error: error.clone() };
            let mut effects = Effects::two(StreamEffect::ReportDown { error, wake: true }, StreamEffect::WatchReach);
            effects.push(StreamEffect::Wait { backoff_ms });
            effects
        }
        (StreamState::Reconnecting { backoff_ms, last_error }, StreamEvent::Failed(error)) => {
            let wake = last_error.as_ref() != Some(&error);
            let next_backoff_ms = (backoff_ms * 2).min(BACKOFF_MAX_MS);
            *state = StreamState::Waiting { next_backoff_ms, last_error: error.clone() };
            let mut effects = Effects::two(StreamEffect::ReportDown { error, wake }, StreamEffect::WatchReach);
            effects.push(StreamEffect::Wait { backoff_ms });
            effects
        }
        (StreamState::Waiting { next_backoff_ms, last_error }, StreamEvent::WaitElapsed) => {
            *state = StreamState::Reconnecting { backoff_ms: next_backoff_ms, last_error: Some(last_error) };
            Effects::none()
        }
        (prev, _) => {
            *state = prev;
            Effects::none()
        }
    }
}

fn handshake_error() -> WireError {
    WireError::new(
        "GET",
        "/api/event",
        WireKind::NotOpenCode {
            status: 200,
            what: "an event stream that didn't start with server.connected".into(),
        },
    )
}

/// How long the event stream may stay silent before it's dropped and reopened, as Desktop's
/// `defaultIdleTimeout`. The server writes a heartbeat every 15 s. A connection that died while
/// the Mac slept just goes quiet, and without a limit its read blocks forever: no reconnect, so
/// no resync, and sessions that finished meanwhile stay running.
const EVENTS_IDLE: Duration = Duration::from_secs(45);

#[cfg(not(target_arch = "wasm32"))]
static RETRY: (std::sync::Mutex<u64>, std::sync::Condvar) = (std::sync::Mutex::new(0),std::sync::Condvar::new());
#[cfg(target_arch = "wasm32")]
pub(super) use super::web::{read_events, wake_reconnects};
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn wake_reconnects() {
    *RETRY.0.lock().unwrap_or_else(PoisonError::into_inner) += 1;
    RETRY.1.notify_all();
}

/// Starts the worker, writer and event threads (the first loads already happened). Returns the
/// request and write senders and the inbox.
pub fn connect(server: Server, wake: Waker) -> (Sender<Request>, Sender<Write>, Receiver<Incoming>) {
    connect_with_idle(server, wake, EVENTS_IDLE)
}

/// [`connect`], dropping and reopening the event stream once it has been silent for `idle`
/// (heartbeats included) instead of [`EVENTS_IDLE`]: a slow or dead link (a tunnel that still
/// accepts but never answers) is noticed sooner.
pub fn connect_with_idle(server: Server, wake: Waker, idle: Duration) -> (Sender<Request>, Sender<Write>, Receiver<Incoming>) {
    let (inbox_tx, inbox) = channel();
    let (req_tx, write_tx) = spawn_workers(&server, &inbox_tx, &wake);
    #[cfg(not(target_arch = "wasm32"))]
    if let Err(e) = std::thread::Builder::new()
        .name("opencode-events".into())
        .spawn(move || read_events(&server, idle, &inbox_tx, &wake))
    {
        eprintln!("opencode-kit: couldn't spawn opencode-events thread: {e}");
    }
    #[cfg(target_arch = "wasm32")]
    read_events(&server, idle, &inbox_tx, &wake);
    (req_tx, write_tx, inbox)
}

/// The event stream, reopened with backoff after each drop, until `inbox` closes (or, after a
/// network change or a wake, until a fresh thread takes it over: `streams::verify`).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn read_events(server: &Server, idle: Duration, inbox_tx: &Sender<Incoming>, wake: &Waker) {
    let stream = super::streams::Stream::new(server.clone(), idle, inbox_tx.clone(), wake.clone());
    run_stream(&stream, stream.epoch(), false);
}

/// How long a stream whose server refuses connections waits before trying on its own: the
/// reach watcher ([`super::reach`]) wakes it the moment the server accepts again, so this is only
/// a backstop.
#[cfg(not(target_arch = "wasm32"))]
const PARKED: Duration = Duration::from_secs(30);

/// Reads `stream` as `epoch` for as long as that's the thread that speaks for it; `resumed` when
/// it takes over from an earlier read (its first open is a reconnect, and resyncs).
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn run_stream(stream: &super::streams::Stream, epoch: u64, resumed: bool) {
    let (server, inbox_tx, wake) = (&stream.server, &stream.inbox, &stream.wake);
    let mut state = if resumed {
        StreamState::Reconnecting { backoff_ms: BACKOFF_MIN_MS, last_error: None }
    } else {
        StreamState::Initial
    };
    while !matches!(state, StreamState::Closed) {
        let generation = *RETRY.0.lock().unwrap_or_else(PoisonError::into_inner);
        let result = server.events(stream, epoch, &mut state);
        if matches!(state, StreamState::Closed) {
            return;
        }
        if !stream.live(epoch) {
            stream_step(&mut state, StreamEvent::Closed);
            return;
        }
        let Err(e) = result else {
            stream_step(&mut state, StreamEvent::Closed);
            return;
        };
        for effect in stream_step(&mut state, StreamEvent::Failed(e)) {
            match effect {
                StreamEffect::ReportDown { error, wake: news } => {
                    // Sent either way: a closed inbox is how this thread learns its window went.
                    if inbox_tx.send(Incoming::StreamDown(error)).is_err() {
                        stream_step(&mut state, StreamEvent::Closed);
                        return;
                    }
                    if news {
                        wake();
                    }
                }
                StreamEffect::WatchReach => server.watch_reach(None),
                StreamEffect::Wait { backoff_ms } => {
                    let wait = if server.refusing() { PARKED } else { Duration::from_millis(backoff_ms) };
                    let _wait = RETRY
                        .1
                        .wait_timeout_while(RETRY.0.lock().unwrap_or_else(PoisonError::into_inner), wait, |g| *g == generation)
                        .unwrap_or_else(PoisonError::into_inner);
                    stream_step(&mut state, StreamEvent::WaitElapsed);
                }
                StreamEffect::DeliverHandshake(_) | StreamEffect::DeliverFrame | StreamEffect::RejectHandshake(_) => {}
            }
        }
    }
}
/// Fail-open new-session prompt gate on the owning `Server`'s write thread (`<= 1 s` hard ceiling).
///
/// Invariant: no prompt is ever held longer than `PROMPT_GATE_TIMEOUT_MS` (1 s), dropped,
/// duplicated, reordered, or sent to another server regardless of window close, tab switch,
/// server switch, app quit, or panic. Dropping or sending on `release` wakes `recv_timeout`
/// immediately and posts the prompt straight onto this worker's own `Server`.
#[cfg(not(target_arch = "wasm32"))]
fn wait_after_create(inbox_tx: &Sender<Incoming>, wake: &Waker, info: &crate::model::SessionInfo) {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    if inbox_tx
        .send(Incoming::PromptGate {
            session: info.id.clone(),
            directory: info.directory.clone(),
            release: tx,
        })
        .is_ok()
    {
        wake();
        rx.recv_timeout(Duration::from_millis(crate::browser_rpc::PROMPT_GATE_TIMEOUT_MS)).ok();
    }
}

#[cfg(target_arch = "wasm32")]
fn wait_after_create(_inbox_tx: &Sender<Incoming>, _wake: &Waker, _info: &crate::model::SessionInfo) {}

/// Whether a held write may go: its gate's answer (`false` when the gate is gone), or at once.
fn released(gate: &mut Option<std::sync::mpsc::Receiver<bool>>) -> bool {
    gate.take().is_none_or(|g| g.recv().unwrap_or(false))
}

/// The fetch and write threads, answering into `inbox_tx`.
pub(super) fn spawn_workers(server: &Server, inbox_tx: &Sender<Incoming>, wake: &Waker) -> (Sender<Request>, Sender<Write>) {
    let (req_tx, req_rx) = channel::<Request>();
    let (write_tx, write_rx) = channel::<Write>();

    {
        let server = server.clone();
        let inbox_tx = inbox_tx.clone();
        let wake = wake.clone();
        let handle = move |w| {
                    #[cfg(not(target_arch = "wasm32"))]
                    let (mut gate, w) = match w {
                        Write::Gated { gate, write } => (Some(gate), *write),
                        w => (None, w),
                    };
                    #[cfg(target_arch = "wasm32")]
                    let mut gate: Option<std::sync::mpsc::Receiver<bool>> = None;
                    // Every gated write but a plain send waits for its row first: they change the server
                    // before they post the prompt (a session made, a revert committed).
                    if !matches!(w, Write::Send { .. }) && !released(&mut gate) {
                        return ControlFlow::Continue(());
                    }
                    let (ticket, w) = match w {
                        Write::Tracked { ticket, write } => (Some(ticket), *write),
                        w => (None, w),
                    };
                    let mut forked = None;
                    let prompt_id = match &w {
                        Write::Send { session, prompt }
                        | Write::SendFromRevert { session, prompt }
                        | Write::StartAs { session, prompt, .. }
                        | Write::StartInNewWorktree { session, prompt, .. } => {
                            prompt.id.clone().map(|id| (session.clone(), id))
                        }
                        _ => None,
                    };
                    // A dock's reply: its request, so the dock hears it was taken.
                    let request_id = match &w {
                        Write::Permission { request, .. } => Some(request.clone()),
                        Write::Form { form, .. } | Write::CancelForm { form, .. } => Some(form.clone()),
                        _ => None,
                    };
                    // A rename, delete, archive or restore: its answer goes back to the machine that
                    // sent it (`client.session.write`), failures included.
                    let session_write = match &w {
                        Write::Rename { session, title } => Some(serde_json::json!({ "kind": "rename", "sessionID": session, "title": title })),
                        Write::Delete(session) => Some(serde_json::json!({ "kind": "delete", "sessionID": session })),
                        Write::Archive(session) => Some(serde_json::json!({ "kind": "archive", "sessionID": session })),
                        Write::Unarchive(session) => Some(serde_json::json!({ "kind": "unarchive", "sessionID": session })),
                        _ => None,
                    };
                    let result = match w {
                        Write::Prompt { session, text } => server.send_prompt(&session, &text),
                        Write::Permission { session, request, decision } => server.reply_permission(&session, &request, &decision),
                        Write::Form { session, form, answer } => server.reply_form(&session, &form, answer),
                        Write::CancelForm { session, form } => server.cancel_form(&session, &form),
                        Write::Interrupt(session) => server.stop(&session).map(|interrupted| {
                            if let Some(interrupted) = interrupted {
                                inbox_tx.send(Incoming::InterruptAcknowledged { session, interrupted }).ok();
                            }
                        }),
                        Write::Create { directory, text } => server.create_session(&directory).and_then(|info| {
                            wait_after_create(&inbox_tx, &wake, &info);
                            server.send_prompt(&info.id, &text)?;
                            inbox_tx.send(Incoming::Created(info)).ok();
                            Ok(())
                        }),
                        Write::Send { session, prompt } => {
                            // A send makes its request while its row commits, and posts it once it has.
                            let request = server.prepare(&session, &prompt);
                            if !released(&mut gate) {
                                return ControlFlow::Continue(());
                            }
                            server.send_prepared(&session, &prompt, request).map(|_| ())
                        }
                        Write::Start { directory, prompt } => server.create_session(&directory).and_then(|mut info| {
                            wait_after_create(&inbox_tx, &wake, &info);
                            server.send(&info.id, &prompt)?;
                            // The reply to the create came before the switches.
                            if let Some(agent) = &prompt.agent {
                                info.agent.clone_from(agent);
                            }
                            if let Some(m) = &prompt.model {
                                info.model = m.clone();
                            }
                            inbox_tx.send(Incoming::Created(info)).ok();
                            Ok(())
                        }),
                        Write::StartAs { session, directory, prompt } => server
                            .start_as(&session, &directory, &prompt, |info| wait_after_create(&inbox_tx, &wake, info))
                            .map(|info| {
                                inbox_tx.send(Incoming::Created(info)).ok();
                            }),
                        Write::StartInNewWorktree { session, project, from, branch, prompt } => {
                            let made = || {
                                inbox_tx.send(Incoming::WorktreeCreated { project: project.clone() }).ok();
                            };
                            server
                                .start_in_new_worktree(
                                    &session,
                                    &project,
                                    &from,
                                    branch.as_deref(),
                                    &prompt,
                                    made,
                                    |info| wait_after_create(&inbox_tx, &wake, info),
                                )
                                .map(|info| {
                                    inbox_tx.send(Incoming::Created(info)).ok();
                                })
                        }
                        Write::Cancel { session, item } => server.cancel_inbox(&session, &item),
                        Write::Deliver { session, item, delivery } => server.set_delivery(&session, &item, delivery),
                        Write::Shell { session, id, command } => server.shell(&session, id.as_deref(), &command),
                        Write::Rename { session, title } => server.rename_session(&session, &title),
                        Write::Delete(session) => server.delete_session(&session),
                        Write::Archive(session) => server.archive(&session),
                        Write::Unarchive(session) => server.unarchive(&session),
                        Write::View { session, idle } => server.view(&session, idle),
                        Write::Metadata { session, metadata } => server.set_metadata(&session, metadata),
                        Write::Background { session, quiet } => server.background(&session).or_else(|e| if quiet { Ok(()) } else { Err(e) }),
                        Write::Compact(session) => server.compact(&session),
                        // `command.config.reload.done` and `.failed` (`composer/commands.tsx:75-82`).
                        Write::Reload => server.write("POST", "/api/location/reload", None)
                            .map(|_| { inbox_tx.send(Incoming::Status("Configuration reloaded".into())).ok(); }),
                        Write::OpenProject { directory, start_session } => server.open_project(&directory).and_then(|(location, projects)| {
                            let session = if start_session { Some(server.create_session(&location.directory)?) } else { None };
                            inbox_tx.send(Incoming::ProjectOpened { location, projects, session }).ok();
                            Ok(())
                        }),
                        Write::RevertStage { session, message, settle } => {
                            let res = if settle { server.revert_to(&session, &message) } else { server.revert_stage(&session, &message) };
                            res.map(|_| ())
                        }
                        Write::RevertClear(session) => server.revert_clear(&session),
                        Write::RevertCommit(session) => server.revert_commit(&session),
                        Write::TakeBack { session, message } => server.take_back(&session, &message),
                        Write::SendFromRevert { session, prompt } => server.send_from_revert(&session, &prompt).map(|_| ()),
                        Write::Fork { session, before } => server.fork_session(&session, before.as_deref()).map(|session| forked = Some(session)),
                        Write::Tracked { .. } => Err("a tracked write can't track another".into()),
                        #[cfg(not(target_arch = "wasm32"))]
                        Write::Gated { .. } => Err("a gated write can't hold another".into()),
                    };
                    if let Some(ticket) = ticket {
                        if inbox_tx.send(Incoming::Settled { ticket, result: result.map(|()| forked) }).is_err() {
                            return ControlFlow::Break(());
                        }
                        wake();
                        return ControlFlow::Continue(());
                    }
                    if result.is_ok() && let Some(id) = &request_id {
                        inbox_tx.send(Incoming::RequestReplied(id.clone())).ok();
                    }
                    if result.is_ok() && let Some((session,id)) = &prompt_id {
                        inbox_tx.send(Incoming::PromptAccepted { session: session.clone(), id: id.clone() }).ok();
                    }
                    if let Some(mut data) = session_write {
                        data["ok"] = serde_json::json!(result.is_ok());
                        if inbox_tx.send(Incoming::SessionWrite { data, error: result.err() }).is_err() {
                            return ControlFlow::Break(());
                        }
                        wake();
                        return ControlFlow::Continue(());
                    }
                    if let Err(e) = result {
                        let told = match prompt_id {
                            Some((session, id)) => inbox_tx.send(Incoming::SendFailed { session, id, error: e }),
                            None => inbox_tx.send(Incoming::Failed(e)),
                        };
                        if told.is_err() {
                            return ControlFlow::Break(());
                        }
                    }
                    wake();
                    ControlFlow::Continue(())
        };
        // Like packages/app/src/composer/submit.ts's inTurn: order one session's writes,
        // not unrelated sessions. Queues borrow workers only while they have work; a slow
        // HTTP request or durability gate holds its own lane, never the dispatcher.
        let mut lanes = std::collections::HashMap::new();
        if let Err(e) = rune::task::serve("opencode-write", write_rx, move |w| {
            let key = w.session().map(str::to_owned);
            let lane = lanes.entry(key).or_insert_with(|| rune::task::queue("opencode-write-session", handle.clone()));
            if let Err(error) = lane.send(w) {
                // No worker available: retain the write and its receipt, even if this
                // dispatcher must temporarily do the blocking work itself.
                return handle(error.0);
            }
            ControlFlow::Continue(())
        }) {
            eprintln!("opencode-kit: couldn't spawn opencode-write thread: {e}");
        }
    }

    {
        let server = server.clone();
        let inbox_tx = inbox_tx.clone();
        let wake = wake.clone();
        let send = move |m: Incoming| {
            let ok = inbox_tx.send(m).is_ok();
            wake();
            ok
        };
        let sessions = |server: &Server| server.get("/api/session?limit=120").map(|v| model::parse_sessions(&v));
        if let Err(e) = rune::task::serve("opencode-fetch", req_rx, move |r| {
                    let ok = match r {
                        Request::Session(id) => match server.read(&format!("/api/session/{id}/message?limit=80&order=desc")) {
                            Ok(v) => send(Incoming::Messages { session: id, messages: model::parse_messages(&v) }),
                            Err(e) => send(Incoming::Failed(e)),
                        },
                        Request::Sessions => match sessions(&server) {
                            Ok(list) => send(Incoming::Sessions(list)),
                            Err(e) => send(Incoming::Failed(e)),
                        },
                        Request::Projects => match server.projects() {
                            Ok(list) => send(Incoming::Projects(list)),
                            Err(e) => send(Incoming::Failed(e)),
                        },
                        Request::DefaultDirectory => match server.get("/api/location").ok().as_ref().and_then(model::parse_location) {
                            Some(location) => send(Incoming::DefaultDirectory(location.directory)),
                            None => true,
                        },
                        Request::Flags { directory } => {
                            let flags = server.flags(directory.as_deref()).unwrap_or_default();
                            send(Incoming::Flags { directory, flags })
                        }
                        Request::Info(id) => match server.get(&format!("/api/session/{id}")) {
                            Ok(v) => send(Incoming::Info(model::parse_session(v.get("data").unwrap_or(&v)))),
                            Err(e) => send(Incoming::Failed(e)),
                        },
                        // A miss (deleted, another server's) says so quietly, not as a status.
                        Request::Lookup(id) => match server.get(&format!("/api/session/{id}")) {
                            Ok(v) => send(Incoming::Info(model::parse_session(v.get("data").unwrap_or(&v)))),
                            Err(_) => send(Incoming::LookupMissing(id)),
                        },
                        Request::Active => match server.get("/api/session/active") {
                            Ok(v) => {
                                let map = v.get("data").unwrap_or(&v);
                                send(Incoming::Active(map.as_object().into_iter().flatten().map(|(k, _)| k.clone()).collect()))
                            }
                            Err(e) => send(Incoming::ActiveFailed(e)),
                        },
                        Request::Pending { session, epoch } => {
                            let permissions = server.pending_permissions(&session);
                            let forms = server.pending_forms(&session);
                            send(Incoming::Pending { session, epoch, permissions, forms })
                        }
                        Request::Worktrees { project, root } => match server.get(&format!("/api/worktree?projectID={}", encode_component(&project))) {
                            Ok(v) => send(Incoming::Worktrees { project, directories: model::parse_worktrees(&v, &root), managed: model::parse_managed_worktrees(&v, &root) }),
                            Err(e) => send(Incoming::Failed(e)),
                        },
                        Request::Branches { directory, search } => {
                            let result = server.branches(&directory, &search);
                            send(Incoming::Branches { directory, search, result })
                        }
                        Request::Inbox(id) => match server.read(&format!("/api/session/{id}/inbox")) {
                            Ok(v) => send(Incoming::Inbox { session: id, items: list(&v) }),
                            Err(e) => send(Incoming::Failed(e)),
                        },
                        // As Desktop's family sync, a failed read leaves the family as it was.
                        Request::Children(id) => match server.children(&id) {
                            Ok(sessions) => send(Incoming::Children { parent: id, sessions }),
                            Err(_) => true,
                        },
                    };
                    if !ok {
                        return ControlFlow::Break(());
                    }
                    // The channel blocks when idle. User-requested reads should drain without
                    // a timer between them: opening a session queues several reads together.
                    ControlFlow::Continue(())
            })
        {
            eprintln!("opencode-kit: couldn't spawn opencode-fetch thread: {e}");
        }
    }

    (req_tx, write_tx)
}
