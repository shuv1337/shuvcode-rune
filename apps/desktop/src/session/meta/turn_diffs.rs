//! Changed files' diffs (Desktop's `turn-diffs.ts`): each finished run's diff, read once from
//! `GET /api/session/:id/diff?from=<the run's first prompt>` when its row first asks, kept for
//! the window's life and read again only on Retry.

use std::cell::RefCell;
use std::collections::HashMap;

use serde_json::Value;
use session_view::turn_diff::State as View;

use crate::App;

enum State {
    Loading,
    Ready(Vec<Value>),
    Failed,
}

/// By session and first prompt.
#[derive(Default)]
pub struct TurnDiffs {
    states: HashMap<(String, String), State>,
    /// Asked for while drawing, read on the next poll.
    wanted: RefCell<Vec<(String, String)>>,
    waker: Option<rune::Waker>,
}

impl TurnDiffs {
    pub fn view(&self, session: &str, from: &str) -> View<'_> {
        match self.states.get(&(session.to_owned(), from.to_owned())) {
            Some(State::Ready(d)) => View::Ready(d),
            Some(State::Failed) => View::Failed,
            Some(State::Loading) => View::Loading,
            None => {
                self.wanted.borrow_mut().push((session.to_owned(), from.to_owned()));
                if let Some(w) = &self.waker {
                    w();
                }
                View::Loading
            }
        }
    }

    pub fn set_waker(&mut self, waker: rune::Waker) {
        self.waker = Some(waker);
    }

    pub fn retry(&mut self, session: &str, from: &str) {
        self.states.remove(&(session.to_owned(), from.to_owned()));
    }

    pub fn retain(&mut self, keep: &dyn Fn(&str) -> bool) {
        self.states.retain(|(session, _), _| keep(session));
    }

    pub fn forget(&mut self, session: &str) {
        self.states.retain(|(s, _), _| s != session);
    }
}

impl App {
    /// Starts reading the diffs drawing asked for; true when one landed or failed at once.
    pub fn poll_turn_diffs(&mut self) -> bool {
        let wanted = std::mem::take(&mut *self.turn_diffs.wanted.borrow_mut());
        let mut changed = false;
        for key in wanted {
            if self.turn_diffs.states.contains_key(&key) {
                continue;
            }
            if !matches!(*self.source, crate::Source::Live { .. }) {
                self.turn_diffs.states.insert(key, State::Failed);
                changed = true;
                continue;
            }
            self.turn_diffs.states.insert(key.clone(), State::Loading);
            let path = format!("/api/session/{}/diff?from={}", crate::switcher::encode(&key.0), crate::switcher::encode(&key.1));
            self.job(move |server| {
                let state = match server.get(&path) {
                    Ok(v) => match v["data"].as_array().or(v.as_array()) {
                        Some(list) => State::Ready(list.clone()),
                        None => State::Failed,
                    },
                    Err(_) => State::Failed,
                };
                Box::new(move |s: &mut App, _| {
                    s.turn_diffs.states.insert(key, state);
                })
            });
        }
        changed
    }
}
