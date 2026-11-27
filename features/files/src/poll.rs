//! Non-generic frame polling, background read landing, and keyboard dispatch for the Files panel.

use std::collections::BTreeSet;

use opencode_ui::keymap::Bind;
use rune::{Key, KeyEvent, TextEdit, Tx};

use crate::{FilesHost, FilesKeyMatch, code_paths, content, image, links, markdown, ops, trace, video, watch};

/// Landing a positive path check changes a visible reply's plain code to a link.
pub fn poll_reply_links(host: &dyn FilesHost, tx: &mut Tx) -> bool {
    if !host.files_untracked().replies.land(host.active_session_id()) {
        return false;
    }
    host.touch_files();
    tx.instant();
    true
}

/// Lands the folder listings, file loads and finds the panel asked for.
pub fn poll_files(host: &mut dyn FilesHost, tx: &mut Tx) -> bool {
    let mut changed = false;
    if host.files().sessions.values().any(|ws| ws.paths.is_building()) {
        for ws in host.files_mut().sessions.values_mut() {
            changed |= ws.paths.poll();
        }
    }
    if let Some((session_id, rel_dir, entries)) = host.files().dir_fetch.take() {
        tx.instant();
        let pane = host.files_mut();
        let waker = pane.waker.clone();
        let ws = pane.sessions.entry(session_id).or_default();
        ws.listed_dirs.insert(rel_dir);
        let mut added = false;
        for (path, is_dir) in entries {
            if is_dir {
                ws.known_dirs.insert(path);
            } else {
                added |= ws.known_files.insert(path);
            }
        }
        if added {
            ws.reindex(waker.as_ref());
        }
        ws.tree_load.step(crate::TreeLoadEvent::Loaded);
        ws.rebuild_tree();
        changed = true;
    }
    // A folder listed again after one of its open files went: what's gone leaves the tree.
    while let Ok((session_id, rel_dir, entries)) = host.files().relists.1.try_recv() {
        tx.instant();
        let pane = host.files_mut();
        let waker = pane.waker.clone();
        let Some(ws) = pane.sessions.get_mut(&session_id) else { continue };
        let parent = |p: &str| p.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned();
        let listed: BTreeSet<&str> = entries.iter().map(|(p, _)| p.as_str()).collect();
        let before = ws.known_files.len();
        ws.known_files.retain(|f| parent(f) != rel_dir || listed.contains(f.as_str()));
        ws.known_dirs.retain(|d| parent(d) != rel_dir || listed.contains(d.as_str()));
        let mut moved = ws.known_files.len() != before;
        for (path, is_dir) in entries {
            if is_dir {
                ws.known_dirs.insert(path);
            } else {
                moved |= ws.known_files.insert(path);
            }
        }
        if moved {
            ws.reindex(waker.as_ref());
        }
        ws.rebuild_tree();
        changed = true;
    }
    while let Some(landed) = host.files().reads.take() {
        changed |= land_read(host, landed, tx);
    }
    if let Some((session_id, query, found)) = host.files().find_fetch.take()
        && let Some(ws) = host.files_mut().sessions.get_mut(&session_id)
        && ws.search.answered(&query, found)
    {
        tx.instant();
        changed = true;
    }
    // A picture's size is known once it decodes: its stage lays out again.
    if host.files().sessions.values().any(|ws| ws.images.values().any(image::FileImage::lands)) {
        for ws in host.files_mut().sessions.values_mut() {
            changed |= ws.images.values_mut().fold(false, |any, i| i.landed() | any);
        }
    }
    changed |= video::poll(host, tx);
    // A session that comes on screen reads what changed while it was off.
    let session = host.active_session_id().map(str::to_owned);
    if session != host.files().shown_session {
        host.files_mut().shown_session = session.clone();
        if let Some(sid) = session {
            watch::shown(host, &sid);
        }
    }
    // A file tab that comes to show arms its watch again (Desktop's tab `focus` syncs it).
    let shown = host.active_file_tab().and_then(|(sid, path)| Some((sid, path?)));
    // It counts as shown once its watch could be asked (its session's folder may come later).
    if shown != host.files().shown {
        let armed = shown
            .as_ref()
            .is_none_or(|(sid, path)| watch::watch(host, sid, path) || !host.is_live());
        if armed {
            host.files_mut().shown = shown;
        }
    }
    // Sync active file's live find query if open.
    if let Some((sid, Some(path))) = host.active_file_tab()
        && host
            .files()
            .sessions
            .get(&sid)
            .and_then(|ws| ws.docs.get(&path))
            .is_some_and(|doc| doc.find_open())
    {
        let sync_doc = host
            .files()
            .sessions
            .get(&sid)
            .and_then(|ws| {
                let doc = ws.docs.get(&path)?;
                Some(
                    (!ws.html.contains_key(&path)
                        && !ws.tables.contains_key(&path)
                        && ws.diagram.current(doc).is_none())
                        || ws.md.get(&path).is_some_and(|v| v.mode.is_forced_source()),
                )
            })
            .unwrap_or(false);
        if sync_doc
            && let Some(doc) = host
                .files_mut()
                .sessions
                .get_mut(&sid)
                .and_then(|ws| ws.docs.get_mut(&path))
        {
            changed |= doc.sync_find_query();
        }
        changed |= markdown::sync_rendered_find(host, &sid, &path, tx);
        let announcement = host.files().sessions.get(&sid).and_then(|ws| {
            let doc = ws.docs.get(&path)?;
            if tx.assistive_technology() && !doc.find_input.text.is_empty() {
                let (at, count) = ws.file_matches(&path, doc);
                Some((sid.clone(), path.clone(), at, count))
            } else {
                None
            }
        });
        if let Some(result) = announcement {
            if host.files().spoken_match.as_ref() != Some(&result) {
                let (at, count) = (result.2, result.3);
                let words = if count == 0 {
                    opencode_i18n::t!("session.search.noResults").to_owned()
                } else {
                    opencode_i18n::t!("rune.a11y.find.match", at = at + 1, count = count)
                };
                tx.announce(words, rune::LiveRegion::Polite);
                host.files_mut().spoken_match = Some(result);
            }
        } else if host.files().spoken_match.is_some() {
            host.files_mut().spoken_match = None;
        }
    } else if host.files().spoken_match.is_some() {
        host.files_mut().spoken_match = None;
    }
    changed
}

/// A read that came back (`content`): shown in place of what showed, keeping where the view is,
/// unless a newer read superseded it. A refresh that fails keeps the text that shows, with no
/// error, and reads again on the next ask (Desktop's `stale`); a failure or a missing file that
/// a later change already outdated isn't shown at all: the next read is on its way.
fn land_read(host: &mut dyn FilesHost, landed: content::Landed, tx: &mut Tx) -> bool {
    let (sid, path) = (landed.sid.clone(), landed.path.clone());
    let again = match host.files_mut().reads.land(&landed) {
        content::ReadEffect::Show => None,
        content::ReadEffect::ShowThenStart { attempt } => Some(attempt),
        _ => return false,
    };
    if let Some(attempt) = again {
        ops::start_read(host, &sid, &path, attempt);
    }
    let body = landed.body;
    let shows_content = host
        .files()
        .sessions
        .get(&sid)
        .and_then(|w| w.docs.get(&path))
        .is_some_and(|d| d.content().has_content());
    if !body.content() && (again.is_some() || (shows_content && matches!(body, content::Body::Failed(_)))) {
        if again.is_none() {
            host.files_mut().reads.outdate(&sid, &path);
        }
        trace::kept(
            &sid,
            &path,
            landed.attempt,
            if again.is_some() {
                "a newer read follows"
            } else {
                "the refresh failed; the text stays, stale"
            },
        );
        return false;
    }
    tx.instant();
    let swap = rune::time::Instant::now();
    let failed_msg = match &body {
        content::Body::Failed(wire) => {
            let what = opencode_i18n::t!("rune.file.readFailed", path = opencode_kit::path::basename(&path)).to_owned();
            let words = host.format_wire_error(&what, wire);
            Some((what, words))
        }
        _ => None,
    };
    let link = {
        let pane = host.files_mut();
        let wrap = pane.wrap;
        let (picture, body) = match body {
            content::Body::Image(bytes) => {
                let len = bytes.len();
                (Some(pane.picture(bytes, false)), content::Body::Binary(len))
            }
            content::Body::Svg(text, bytes) => (Some(pane.picture(bytes, true)), content::Body::Text(text)),
            other => (None, other),
        };
        let ws = pane.sessions.entry(sid.clone()).or_default();
        match picture {
            // A picture read again keeps its zoom and its decoded pixels while the new ones decode.
            Some(mut p) => {
                if let Some(old) = ws.images.remove(&path) {
                    p.carry_from(old);
                }
                ws.images.insert(path.clone(), p);
            }
            None => {
                ws.images.remove(&path);
            }
        }
        ws.md.remove(&path);
        if let Some(previous) = ws.tables.remove(&path) {
            ws.table_find.forget(&previous);
        }
        if let Some(previous) = ws.html.remove(&path) {
            ws.html_find.forget(&previous);
        }
        let doc = ws.docs.entry(path.clone()).or_insert_with(|| wrap.doc(&path, ""));
        match body {
            content::Body::Text(text) | content::Body::Svg(text, _) => doc.refresh(text),
            content::Body::Html(text, document) => {
                doc.refresh(text);
                ws.html.insert(path.clone(), document);
            }
            content::Body::Table(text, document) => {
                doc.refresh(text);
                ws.tables.insert(path.clone(), document);
            }
            content::Body::Binary(sz) => doc.set_binary(sz),
            content::Body::Image(bytes) => doc.set_binary(bytes.len()),
            content::Body::Missing => doc.set_missing(),
            content::Body::Failed(_) => {
                if let Some((what, words)) = failed_msg {
                    doc.set_failed(what, words);
                }
            }
        }
        if let Some((start, end)) = ws.pending_jump.remove(&path) {
            let scroll_key = format!("file-code-{sid}-{path}-scroll");
            doc.scroll_to_lines(&scroll_key, start, end, tx);
        }
        ws.link_opens.remove(&path)
    };
    if let Some(link) = link {
        links::finish_file_link(host, &sid, &path, link, tx);
    }
    code_paths::check_code_paths(host, &sid, &path);
    trace::landed(
        &sid,
        &path,
        landed.attempt,
        landed.times,
        swap,
        host.files().sessions.get(&sid).and_then(|w| w.docs.get(&path)),
    );
    true
}

/// Keys for the open file while a Files tab shows: its find, selection and copy.
pub fn key_files(host: &mut dyn FilesHost, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let Some((sid, active_path)) = host.active_file_tab() else {
        return false;
    };

    if host.files().filter.focused {
        let q = host.files().filter.text.clone();
        if ev.key == Key::Escape {
            tx.instant();
            let pane = host.files_mut();
            if !q.is_empty() {
                pane.filter = TextEdit::new("");
                pane.filter.focused = true;
            } else {
                pane.filter.focused = false;
            }
            return true;
        }
        let filtered = ops::filtered_sidebar_files(host, &sid);
        if !q.trim().is_empty() && !filtered.is_empty() {
            match &ev.key {
                Key::Down | Key::Up => {
                    tx.instant();
                    let cur = host
                        .files()
                        .sessions
                        .get(&sid)
                        .and_then(|w| w.highlighted_filter_path.as_deref())
                        .and_then(|p| filtered.iter().position(|x| x == p));
                    let delta: isize = if ev.key == Key::Down { 1 } else { -1 };
                    let next_idx = match cur {
                        Some(i) => (i as isize + delta).clamp(0, filtered.len() as isize - 1) as usize,
                        None => 0,
                    };
                    if let Some(ws) = host.files_mut().sessions.get_mut(&sid) {
                        ws.highlighted_filter_path = Some(filtered[next_idx].clone());
                    }
                    return true;
                }
                Key::Enter => {
                    ops::submit_files_filter(host, tx);
                    return true;
                }
                _ => {}
            }
        }
        return false;
    }

    if host.is_composer_focused() || !host.is_side_focused() {
        return false;
    }

    if let Some(path) = active_path {
        let scroll_key = format!("file-code-{sid}-{path}-scroll");
        let rendered_find_enter = host
            .files()
            .sessions
            .get(&sid)
            .filter(|ws| markdown::shows_rendered(ws, &path))
            .and_then(|ws| ws.docs.get(&path))
            .map(|doc| {
                doc.find_open()
                    && doc.find_input.focused
                    && !doc.find_input.is_composing()
                    && ev.key == Key::Enter
            });
        if let Some(find_enter) = rendered_find_enter {
            // The rendered text keeps its own selection and matches.
            if find_enter {
                markdown::file_find_step(host, &sid, &path, !ev.mods.shift, tx);
                return true;
            }
            if Bind::CopySelection.matches_tx(ev, tx) {
                return false;
            }
        }
        if let Some(ws) = host.files_mut().sessions.get_mut(&sid)
            && let Some(doc) = ws.docs.get_mut(&path)
        {
            return doc.handle_key(&scroll_key, ev, tx, &FilesKeyMatch);
        }
    }
    false
}
