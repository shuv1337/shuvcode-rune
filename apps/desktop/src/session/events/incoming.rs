//! `poll`'s server messages: the replies to this window's own reads and writes (its inbox), and
//! its server's stream events, which the server's copy took once and each window reacts to
//! (`windows::peer`, `events`).

use opencode_kit::client::{Incoming, Ticket, TicketKind};
use opencode_kit::model::{Pending, SessionInfo, Transcript};
use rune::Tx;

use crate::{App, Route, attachments, fork, permission, reconnect, status_toast, stop, windows};

/// Takes what came in since the last poll. A delivered prompt has no entrance: no snapshot of
/// the old transcript or queues is needed to render the new state.
pub(crate) fn poll(s: &mut App, tx: &mut Tx) -> bool {
    let mut changed = false;
    let replies: Vec<Incoming> = s.inbox.as_ref().map(|inbox| inbox.try_iter().collect()).unwrap_or_default();
    if !replies.is_empty() {
        // A reply only this window heard changes the data its server's windows share.
        windows::peer::touch(s);
    }
    for m in replies {
        changed |= reply(s, m, tx);
    }
    let heard = windows::peer::heard(s);
    for h in heard {
        let is_status = matches!(
            &*h,
            crate::events::Heard::Message(m)
                if matches!(**m, Incoming::Status(_) | Incoming::Failed(_) | Incoming::ActiveFailed(_) | Incoming::StreamDown(_) | Incoming::Unanswered(_))
        );
        if !is_status {
            changed |= reconnect::heard(s);
        }
        changed |= match &*h {
            crate::events::Heard::Event(e) => {
                if crate::motion_log::wanted() { s.diag.motion_events.note(&e.ty); }
                // A session deleted while its tab is open: the tab says it's gone (`session_state`).
                let gone = e.ty == "session.deleted" && e.data["sessionID"].as_str().is_some_and(|id| s.route == Route::here(id.to_owned()));
                if e.ty == "session.deleted" && let Some(id) = e.data["sessionID"].as_str() {
                    s.close_browser_tabs_for_session(id);
                    crate::session_menu::unread::set(s, &crate::servers::runtime::current(), id, false);
                    crate::session_menu::preferences::forget(s, &crate::servers::runtime::current(), id);
                    crate::pickers::saved::forget(s, id);
                }
                if gone && let Some(id) = e.data["sessionID"].as_str() { crate::session_state::missing(s, id, tx); }
                s.react_live(e) | gone
            }
            crate::events::Heard::Message(m) => message(s, (**m).clone(), tx),
        };
    }
    s.poll_recent_reload(tx);
    changed |= s.settle_picks();
    changed
}

/// The stream's own news: it came back, or a line about it.
fn message(s: &mut App, m: Incoming, tx: &mut Tx) -> bool {
    match m {
        Incoming::Reconnected => {
            tx.instant();
            crate::connect::picker_reads::refresh(s);
            s.windows_forgotten();
            crate::session_state::reconnected(s, tx);
            crate::location_missing::reconnected(s, tx);
            let active_server = crate::servers::runtime::active(s).clone();
            s.sidebar_inbox.branches.reconnected(&active_server);
            reconnect::back(s);
            // Terminals that failed under the old connection attach again (`server.connected`).
            s.kernel.slots.get_mut(&crate::terminal::TERMINAL).connections += 1;
            s.pump_outbox();
            crate::session_viewed::connected(s);
            s.stream_back();
            s.check_sends();
            s.resync_view();
            crate::settings::pages::extensions::on_server_reconnected(s, &active_server);
            #[cfg(not(target_arch = "wasm32"))]
            crate::surfaces::browser::rpc::on_sse_connected(s, &active_server);
            // A restarted server forgets its file watches (`server.connected` in Desktop), and a
            // change made meanwhile went unreported.
            crate::files::watch::connected(s);
            crate::files::watch::reread(s);
        }
        // The first open: the link was never down, but the health poll can stand down now.
        Incoming::Opened => {
            reconnect::back(s);
            crate::start_location::ask(s);
            crate::files::watch::connected(s);
            #[cfg(not(target_arch = "wasm32"))]
            {
                let active_server = crate::servers::runtime::active(s).clone();
                crate::surfaces::browser::rpc::on_sse_connected(s, &active_server);
            }
        }
        Incoming::Status(e) => status_toast::from_server(s, e),
        Incoming::Failed(e) => failed(s, e, tx),
        Incoming::ActiveFailed(e) => reconnect::active_failed(s, &e, tx),
        Incoming::StreamDown(e) => {
            #[cfg(not(target_arch = "wasm32"))]
            {
                let active_server = crate::servers::runtime::active(s).clone();
                crate::surfaces::browser::rpc::on_sse_disconnected(s, &active_server);
            }
            reconnect::stream_down(s, e, tx);
        }
        Incoming::Unanswered(e) => reconnect::unanswered(s, e, tx),
        _ => {}
    }
    true
}

/// A read or write that failed: the link hears it, a reply's dock takes its own, and the rest
/// is the toast for a failed write (a read stays in the debug bar: the app retries those).
fn failed(s: &mut App, e: opencode_kit::wire_error::WireError, tx: &mut Tx) {
    reconnect::call_failed(s, &e, tx);
    if permission::failed(s, &e) {
        return;
    }
    status_toast::from_wire(s, e);
}

/// A tracked write's outcome, for the machine that asked under the attempt it asked with.
fn settled(s: &mut App, ticket: Ticket, result: Result<Option<SessionInfo>, opencode_kit::wire_error::WireError>, tx: &mut Tx) {
    let (session, attempt) = (ticket.session.as_str(), ticket.attempt);
    match ticket.kind {
        TicketKind::Stop => {
            if let Err(e) = result {
                stop::failed(s, session, attempt, &e);
            }
        }
        TicketKind::Revert => s.revert_settled(session, attempt, result.map(|_| ())),
        TicketKind::Fork => fork::settled(s, attempt, result, tx),
        TicketKind::Inbox => s.queue_settled(session, attempt, result.map(|_| ()).map_err(Some)),
        TicketKind::View => crate::session_viewed::settled(s, session, attempt, result),
        TicketKind::TakeBack => crate::rewind::take_back::settled(s, session, attempt, result.is_ok()),
    }
}

/// One reply to this window's own reads and writes.
fn reply(s: &mut App, m: Incoming, tx: &mut Tx) -> bool {
    if !matches!(m, Incoming::Status(_) | Incoming::Failed(_) | Incoming::ActiveFailed(_) | Incoming::StreamDown(_) | Incoming::Unanswered(_) | Incoming::SendFailed { .. }) {
        reconnect::heard(s);
    }
    match m {
        Incoming::Sessions(list) => {
            s.session_search.invalidate();
            s.sessions_listed(list);
            s.sort_projects();
            crate::rewind::read_sessions(s);
        }
        Incoming::Info(mut info) => {
            if rune::Tracked::untracked(&s.recent_page).would_record_info(&info) {
                s.recent_page.record_info(&info);
            }
            let recent_changed = rune::Tracked::untracked(&s.recent_page).would_observe(&info);
            if recent_changed {
                s.recent_page.observe(&info);
            }
            let parent = info.parent.clone();
            let root = s.root_of(&info.id).to_owned();
            let tab_before = s.tab_status(&root);
            let (changed, visible_meta) = match s.sessions.iter().position(|x| x.id == info.id) {
                Some(idx) => {
                    let old = &s.sessions[idx];
                    info.viewed = info.viewed.max(old.viewed);
                    let meta = old.title != info.title
                        || old.directory != info.directory
                        || old.parent != info.parent
                        || old.project != info.project
                        || old.revert != info.revert
                        || old.outcome != info.outcome
                        || old.cost.to_bits() != info.cost.to_bits()
                        || old.model != info.model
                        || old.agent != info.agent;
                    let any = meta
                        || old.updated != info.updated
                        || old.idle != info.idle
                        || old.viewed != info.viewed
                        || old.busy != info.busy
                        || old.created != info.created
                        || old.archived != info.archived
                        || old.metadata != info.metadata;
                    if any {
                        s.sessions[idx] = info.clone();
                    }
                    (any, meta)
                }
                None => {
                    s.sessions.push(info.clone());
                    (true, true)
                }
            };
            if changed {
                s.session_search.invalidate();
            }
            if let Some(missing) = parent.and_then(|p| s.missing_ancestor(&p)) {
                s.fetch_info(&missing);
            }
            crate::rewind::read_sessions(s);
            if visible_meta {
                s.note_tab_titles();
            }
            let tab_after = s.tab_status(&root);
            let shown_here = matches!(&*s.route, crate::Route::Home)
                || s.tab_layout() == crate::layout_state::TabLayout::Inbox
                || s.kernel.slots.get(&crate::switcher::SWITCHER).is_open()
                || matches!(&*s.route, crate::Route::Session { id, .. } if s.root_of(id) == root);
            return recent_changed || visible_meta || tab_before != tab_after || (changed && shown_here);
        }
        Incoming::Children { sessions, .. } => s.children_listed(sessions),
        Incoming::Messages { session, messages } => {
            s.tail_page(&session);
            s.judge_sends_by_page(&session, &messages);
            s.check_sends();
            let cold = s.launch.cold_transcript.as_ref().is_some_and(|c| c.id == session);
            if cold {
                s.launch.cold_transcript = None;
            }
            let first = !s.transcripts.contains_key(&session);
            let t = s.transcripts.entry(session.clone()).or_insert_with(|| Transcript { session: session.clone(), messages: vec![] });
            attachments::reconcile(&mut s.composer_ext.attach.echoes, t, &messages);
            let delivered: Vec<String> = messages.iter().map(|m| m.id().to_owned()).collect();
            match s.kernel.slots.get_mut(&crate::events::STREAM_GAPS).0.get_mut(&session) {
                Some(gapped) => t.merge_caught(messages, gapped),
                None => t.merge(messages),
            }
            reconnect::page_in(s, &session);
            s.settle_inbox(&session, &delivered);
            if (first || cold) && s.route == Route::here(session.clone()) {
                // Guarantee 8: the first load of the open session lands, nothing grows in. One
                // read up from its end (a place kept from the last run) stays where it is: landing
                // the end's follow would carry it there.
                session_view::jump::settle(s, tx);
            }
        }
        Incoming::LookupMissing(_) => return false,
        Incoming::Inbox { session, items } => s.inbox_listed(&session, &items),
        Incoming::Worktrees { project, directories, managed } => s.worktrees_listed(project, directories, managed),
        Incoming::Branches { directory, search, result } => return s.branches_listed(&directory, &search, result.ok()),
        Incoming::InterruptAcknowledged { session, interrupted } => {
            if !interrupted { stop::confirmed(s, &session); }
            crate::rewind::take_back::acknowledged(s, &session, interrupted);
        }
        Incoming::WorktreeCreated { project } => s.worktree_updated(&project),
        Incoming::RequestReplied(id) => s.reply_answered(&id, Ok(())),
        Incoming::PromptAccepted { session, id } => {
            let admitted = s.prompt_accepted(&session, &id) | s.admitted_send(&id);
            s.pump_outbox();
            return admitted;
        }
        Incoming::Event { ty, data } => {
            if crate::motion_log::wanted() { s.diag.motion_events.note(&ty); }
            return s.live_event(ty, data);
        }
        Incoming::Status(e) => status_toast::from_server(s, e),
        Incoming::Failed(e) => failed(s, e, tx),
        Incoming::ActiveFailed(e) => {
            reconnect::active_failed(s, &e, tx);
            return false;
        }
        Incoming::StreamDown(e) => reconnect::stream_down(s, e, tx),
        Incoming::Unanswered(e) => reconnect::unanswered(s, e, tx),
        Incoming::SessionWrite { data, error } => {
            s.session_write_replied(&data, error, tx);
            return true;
        }
        Incoming::SendFailed { session, id, error } => {
            if s.unsent.journaled(&id) {
                s.send_failed(&id, &error);
            } else if !s.start_failed(&session, &id, &error, tx) && !s.send_failed(&id, &error) {
                status_toast::from_wire(s, error);
            }
        }
        Incoming::Pending { session, epoch, permissions, forms } => {
            if s.request_reads.is_stale(&session, epoch) {
                return false;
            }
            let is_not_found = |e: &opencode_kit::wire_error::WireError| {
                matches!(e.kind, opencode_kit::wire_error::WireKind::Status { status: 404, .. })
            };
            let perm_failed = matches!(&permissions, Err(e) if !is_not_found(e));
            let form_failed = matches!(&forms, Err(e) if !is_not_found(e));
            let any_failed = perm_failed || form_failed;
            s.settle_pending(&session, epoch, any_failed, tx);
            let inbox = !any_failed && !s.sidebar_inbox.settled() && s.sidebar_inbox.heard_pending(&session);
            let mut changed = inbox;
            // Sync permissions and forms independently: a transient HTTP error on either endpoint
            // never prunes cached items, and a delayed GET never deletes a request that arrived
            // over SSE after `epoch` was stamped (`data.ts:2132,2182`).
            if let Ok(items) = permissions {
                changed |= apply_pending_kind(s, &session, epoch, false, items);
            }
            if let Ok(items) = forms {
                changed |= apply_pending_kind(s, &session, epoch, true, items);
            }
            return changed;
        }
        // Only the stream sends this in the app (`windows::peer::heard`); a test's or the fuzz
        // link's inbox gets the same resync.
        Incoming::Reconnected => {
            crate::session::meta::flags::reset_server(s);
            s.resync_shared();
            return message(s, Incoming::Reconnected, tx);
        }
        Incoming::Opened => {
            reconnect::request_active(s, false);
            return message(s, Incoming::Opened, tx);
        }
        Incoming::Active(ids) => {
            tx.instant();
            // Overlay execution SSE events received while `Request::Active` was in flight so live
            // transitions supersede the snapshot (`packages/client/src/solid/data.ts:783-807`).
            let deltas = s.stops.active_sync.landed();
            let mut merged: std::collections::HashSet<String> = ids.into_iter().collect();
            for (sid, started) in deltas {
                if started {
                    merged.insert(sid);
                } else {
                    merged.remove(&sid);
                }
            }
            let reconciled: Vec<String> = merged.iter().cloned().collect();
            s.stops.reconcile(&reconciled);
            reconnect::active(s, &reconciled);
            // A stop the server hasn't confirmed yet stays stopped, whichever window pressed it.
            *s.running = merged.into_iter().filter(|id| !s.stops.pending(id)).collect();
        }
        Incoming::Projects(projects) => {
            s.session_search.invalidate();
            tx.instant();
            *s.projects = projects;
            s.sort_projects();
            s.list_unlisted_worktrees();
        }
        Incoming::DefaultDirectory(directory) => crate::start_location::answered(s, directory),
        Incoming::Flags { directory, flags } => crate::session::meta::flags::answered(s, directory, flags),
        Incoming::ProjectOpened { location, projects, session } => {
            s.apply_project_opened(location, projects, session, tx);
        }
        Incoming::Settled { ticket, result } => settled(s, ticket, result, tx),
        #[cfg(not(target_arch = "wasm32"))]
        Incoming::PromptGate { session, directory, release } => {
            crate::surfaces::browser::rpc::hold_start_gate(s, &session, &directory, release);
            return false;
        }
        Incoming::Created(info) if s.queue.starts.has(&info.id) => {
            s.started(info, tx);
        }
        Incoming::Created(info) => {
            let id = info.id.clone();
            // A command sent from a draft started it: the draft's tab becomes its tab.
            if let Some(draft) = s.promote_draft(&id, &info.directory) {
                s.composer_ext.pickers.promote_draft(&draft, &id);
                crate::pickers::saved::save(s, &draft);
                crate::pickers::saved::save(s, &id);
            }
            if s.session(&id).is_none() {
                s.sessions.insert(0, info);
            }
            *s.diag.status = s.live_status();
            s.open(&id, tx);
        }
    }
    true
}

/// Applies one successful `/permission` (`is_form = false`) or `/form` (`is_form = true`) snapshot
/// for `session` at `epoch`: prunes only entries of that kind absent from `items` and NOT received
/// over SSE after `epoch` was stamped, then merges `items`.
fn apply_pending_kind(s: &mut App, session: &str, epoch: u64, is_form: bool, items: Vec<Pending>) -> bool {
    let matches_kind = |p: &Pending| matches!(p, Pending::Form { .. }) == is_form;
    let to_drop: Vec<String> = s
        .requests
        .iter()
        .filter(|(sid, id)| {
            sid == session
                && !items.iter().any(|i| i.id() == id)
                && !s.request_reads.arrived_after(id, epoch)
                && match s.pending.iter().find(|p| p.id() == id) {
                    Some(p) => matches_kind(p),
                    None => (!id.starts_with("per")) == is_form,
                }
        })
        .map(|(_, id)| id.clone())
        .collect();
    let mut changed = false;
    let drop_pending = s.pending.iter().any(|p| {
        p.session() == session
            && matches_kind(p)
            && !items.iter().any(|i| i.id() == p.id())
            && !s.request_reads.arrived_after(p.id(), epoch)
    });
    if drop_pending {
        let reads = &s.request_reads;
        s.pending.retain(|p| {
            p.session() != session
                || !matches_kind(p)
                || items.iter().any(|i| i.id() == p.id())
                || reads.arrived_after(p.id(), epoch)
        });
        changed = true;
    }
    if !to_drop.is_empty() {
        s.requests.retain(|(sid, id)| sid != session || !to_drop.iter().any(|d| d == id));
        for id in &to_drop {
            s.request_reads.forget_sse(id);
        }
        changed = true;
    }
    for p in items {
        changed |= s.add_pending(p);
    }
    changed
}
