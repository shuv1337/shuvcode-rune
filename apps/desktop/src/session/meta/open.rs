use opencode_kit::client::Request;
use rune::Tx;

use crate::pickers::PickerHost as _;
use crate::tabs::Tab;
use crate::{App, Route, Source, flag};

impl App {
    /// Opens a session: lands at the bottom on the first frame with nothing animating in. It
    /// leaves Settings, as any page the sidebar opens does.
    pub fn open(&mut self, id: &str, tx: &mut Tx) {
        self.close_settings();
        if self.can_reveal_outside()
            && let Some(dir) = self.session(id).map(|s| s.directory.clone())
        {
            // Lazily pin ONLY this routed session's workspace root once process-wide before its
            // messages render or any tool runs (zero boot syscalls for inactive listed projects).
            let _ = self.pinned_local_root(&dir);
        }
        if self.opening.as_ref().is_some_and(|o| o.id == id) { return; }
        let held = self.pages.has_pool() && self.route != Route::here(id.to_owned());
        if self.route != Route::here(id.to_owned()) { crate::telemetry::open_begin(id, held); }
        if held {
            self.begin_open(id, tx);
        } else {
            *self.opening = None;
            self.open_now(id, tx);
            self.open_page(id, tx);
            // Its place comes back now if its row is here, else when a page brings it.
            self.land_place(id, tx);
        }
    }

    /// Opens a session as a tab behind the page that shows (Desktop's `open` with `background`,
    /// `home/sessions/controller.tsx:567-586`): the page stays, its messages aren't read yet and
    /// its project isn't touched.
    pub fn open_in_background(&mut self, id: &str) {
        let root = self.root_of(id).to_owned();
        if self.tabs.iter().any(|t| t.id == root) {
            return;
        }
        let (title, directory) = self.session(&root).map(|s| (s.title.clone(), s.directory.clone())).unwrap_or_default();
        self.tabs.push(Tab::session(root, title, directory));
        self.save_tabs();
    }

    /// Commits the input destination immediately; the transcript can still be loading.
    pub(crate) fn open_now(&mut self, id: &str, tx: &mut Tx) {
        // What the session's screen draws from plugins is there in its first frame.
        if *self.route != Route::here(id.to_owned()) {
            crate::plugin_host::dormant::wake_for_route(self, id);
        }
        crate::telemetry::committed(id, false);
        self.close_picker(false);
        self.close_message_menu();
        self.close_fork_dialog(false);
        crate::subagents::shells::set_open(self, false);
        *self.kernel.slots.get_mut(&crate::todos::TODO_OPEN) = false;
        if self.route != Route::here(id.to_owned()) {
            // Read scrolled up, it comes back where it was read; otherwise on its end.
            self.timeline_fx.ends.reopen(id);
        }
        if *self.route != Route::here(id.to_owned()) {
            crate::shell::memory_reclaim::on_tab_switched(self);
        }
        *self.route = Route::here(id.to_owned());
        crate::narrow::follow_route(self, tx);
        crate::session_state::opened(self, id, tx);
        // Its own draft is in the composer on the first frame.
        let _ = self.show_draft();
        let root = self.root_of(id).to_owned();
        // As in Desktop, you can type straight away, handing focus on from whatever control held it
        // when the session opened (including a titlebar button or side panel via a tab shortcut,
        // `session/composer/focus.tsx` #64418/#64504). A subagent's page waits for a click into its
        // composer, so its keys (E in the inbox, Esc) stay with the page, and a story's keys stay
        // with the storybook.
        if !crate::auto_focus_composer(tx) {
            self.composer.focused = false;
        } else if root == id && self.fixture_run.stage.is_none() && !self.dock_shown() {
            self.search.focused = false;
            self.kernel.slots.get_mut(&crate::find::FIND).input.focused = false;
            self.kernel.slots.get_mut(&crate::panels::SIDE).focus.blur();
            crate::plugin_host::blur_fields(self);
            self.composer.focused = true;
            tx.focus("prompt", tx.focus_visible());
        }
        // Reading it, and reporting it read, waits for a focused window (`session_viewed::poll`).
        crate::notifications::seen(self, &root);
        crate::windows::peer::touch(self);
        if !self.tabs.iter().any(|t| t.id == root) {
            let (title, directory) = self.session(&root).map(|s| (s.title.clone(), s.directory.clone())).unwrap_or_default();
            self.tabs.push(Tab::session(root.clone(), title, directory));
            self.save_tabs();
        }
        if let Some(pid) = self.session(&root).and_then(|s| self.project_for_session(s)).map(|p| p.id.clone()) {
            self.touch_project(&pid);
        }
        crate::plugin_host::native::diff::prepare_route(self, id);
        session_view::jump::settle(self, tx);
        match &*self.source {
            // A session still starting has nothing to read yet (`new_session.rs`).
            Source::Live { .. } if self.queue.starts.has(id) => {}
            Source::Live { requests: Some(r), .. } => {
                if !self.pages.has_pool() { r.send(Request::Session(id.to_owned())).ok(); }
                r.send(Request::Inbox(id.to_owned())).ok();
                self.list_children(id);
                self.window_opened(id);
                self.read_open_requests(id);
                if let Some(dir) = self.session(id).map(|i| i.directory.clone()) {
                    crate::subagents::shells::list(self, &dir);
                    self.prefetch_review_vcs(&dir);
                }
                crate::subagents::jobs::list(self, id);
            }
            Source::Live { .. } => {}
            Source::Fixture => self.start_stream_if_due(id),
        }
    }

    /// The event stream came back: re-reads what a gap may have made stale in the server's copy
    /// (what's running, the session list and projects, pending requests), like the first load,
    /// once for all its windows (`App::apply_shared`).
    pub(crate) fn resync_shared(&mut self) {
        self.refresh_recent();
        crate::reconnect::request_active(self, true);
        let Source::Live { requests: Some(r), .. } = &*self.source else { return };
        r.send(Request::Sessions).ok();
        r.send(Request::Projects).ok();
        r.send(Request::DefaultDirectory).ok();
        // Requests asked while the stream was down never arrive as events: read again what shows
        // them, as Desktop's store invalidates its syncs on a disconnect.
        self.forget_request_reads();
        let mut asked: Vec<String> = self
            .pending
            .iter()
            .map(|p| p.session().to_owned())
            .chain(self.requests.iter().map(|(s, _)| s.clone()))
            .collect();
        asked.sort();
        asked.dedup();
        for id in asked {
            self.read_requests(&id);
        }
    }

    /// The event stream came back, in each window: the open session's inbox, requests, shells
    /// and latest page, and its subagents' pages.
    pub(crate) fn resync_view(&mut self) {
        crate::subagents::shells::forget_lists(self);
        crate::subagents::jobs::reset(self);
        let dir = match &*self.route { Route::Session { id, .. } => self.session(id).map(|i| i.directory.clone()), _ => None };
        if let Some(dir) = dir {
            crate::subagents::shells::list(self, &dir);
        }
        if let Route::Session { id, .. } = (*self.route).clone() {
            crate::subagents::jobs::list(self, &id);
        }
        let Source::Live { requests: Some(r), .. } = &*self.source else { return };
        if let Route::Session { id, .. } = (*self.route).clone() {
            r.send(Request::Inbox(id.clone())).ok();
            self.list_children(&id);
            self.read_open_requests(&id);
            self.reload_session(&id);
        }
        crate::reconnect::catch_up(self);
    }

    pub(crate) fn start_stream_if_due(&mut self, id: &str) {
        let stream = &self.fixture_run.fixture["stream"];
        if stream["session"].as_str() != Some(id) || stream["played"].as_bool() == Some(true) || flag("--no-stream") {
            return;
        }
        let at = *self.clock;
        for e in stream["events"].as_array().into_iter().flatten() {
            let t = at + e["t"].as_f64().unwrap_or(0.0);
            self.fixture_run.playback.push_back((t, e["type"].as_str().unwrap_or("").to_owned(), e["data"].clone()));
        }
        self.fixture_run.fixture["stream"]["played"] = true.into();
        self.wake_at_playback();
    }

    /// Applies the fixture's streaming turn at once (the finished session).
    pub(crate) fn finish_stream(&mut self) {
        let events = self.fixture_run.fixture["stream"]["events"].as_array().cloned().unwrap_or_default();
        for e in events {
            self.apply_event(e["type"].as_str().unwrap_or(""), &e["data"]);
        }
        self.fixture_run.fixture["stream"]["played"] = true.into();
    }

    /// Windowed runs sleep between frames; a timer wakes the window for each event.
    pub(crate) fn wake_at_playback(&self) {
        let Some(w) = (*self.waker).clone() else { return };
        let times: Vec<f64> = self.fixture_run.playback.iter().map(|e| e.0 - *self.clock).collect();
        wake_each(w, times.into_iter(), rune::time::Instant::now());
    }
}

/// Wakes `w` at each of `times` (seconds after `start`), one timer after another: a thread that
/// sleeps natively, a timer on the web.
fn wake_each(w: rune::Waker, mut times: std::vec::IntoIter<f64>, start: rune::time::Instant) {
    let Some(t) = times.next() else { return };
    let delay = std::time::Duration::from_secs_f64(t.max(0.0)).saturating_sub(start.elapsed());
    rune::task::spawn_after("fixture-playback", delay, move || {
        w();
        wake_each(w, times, start);
    })
    .ok();
}
