//! Where each session's timeline was being read, so switching back lands on it again, as
//! Desktop keeps a session's detached timeline alive (`session/timeline/cache.ts`).
//!
//! The timeline writes where it last drew its viewport and each row, and which rows showed, at
//! no cost to a scroll (`El::drawn_rect_into`, `El::on_visible_rows`). The place is the first
//! row showing under the viewport's top and how far above that top it starts, so what lands
//! below it while you're away doesn't move what you were reading.
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rune::{El, Live};

/// Each session's last drawn timeline.
#[derive(Default)]
pub struct Recall {
    marks: RefCell<HashMap<String, Rc<Marks>>>,
}

/// One timeline's last drawn frame.
pub struct Marks {
    view: Live<[f32; 4]>,
    rows: RefCell<HashMap<String, Live<[f32; 4]>>>,
    visible: RefCell<Vec<String>>,
}

impl Recall {
    pub fn marks(&self, session: &str) -> Rc<Marks> {
        self.marks.borrow_mut().entry(session.to_owned()).or_insert_with(|| Rc::new(Marks {
            view: Live::new([0.0; 4]),
            rows: RefCell::default(),
            visible: RefCell::default(),
        })).clone()
    }

    /// Row `key` and where its top stood below the viewport's top (negative when above it):
    /// the first row showing on `session`'s last drawn frame.
    pub fn place(&self, session: &str) -> Option<(String, f32)> {
        self.drawn_place(session).map(|(key, row_y, view_y)| (key, row_y - view_y))
    }

    /// The same reading place in window coordinates. When the viewport itself moves, a new
    /// viewport can put the row back at that y, not merely the same distance below its new top.
    pub fn screen_place(&self, session: &str) -> Option<(String, f32)> {
        self.drawn_place(session).map(|(key, row_y, _)| (key, row_y))
    }

    fn drawn_place(&self, session: &str) -> Option<(String, f32, f32)> {
        let marks = self.marks.borrow().get(session)?.clone();
        let [_, top, _, h] = marks.view.get();
        if h <= 0.0 { return None; }
        let rows = marks.rows.borrow();
        marks.visible.borrow().iter().find_map(|key| {
            let [_, y, _, h] = rows.get(key)?.get();
            (h > 0.0 && y + h > top + 0.5).then(|| (key.clone(), y, top))
        })
    }

    pub fn forget(&self, session: &str) {
        self.marks.borrow_mut().remove(session);
    }

    pub fn retain(&self, keep: &dyn Fn(&str) -> bool) {
        self.marks.borrow_mut().retain(|id, _| keep(id));
    }
}

impl Marks {
    pub fn view(&self, el: El) -> El {
        el.drawn_rect_into(&self.view)
    }

    pub fn row(&self, key: &str, el: El) -> El {
        let mut rows = self.rows.borrow_mut();
        let live = rows.entry(key.to_owned()).or_insert_with(|| Live::new([0.0; 4]));
        el.drawn_rect_into(live)
    }

    /// The keys of the rows the last drawn frame showed.
    pub fn visible(&self) -> Vec<String> {
        self.visible.borrow().clone()
    }

    pub fn shown(&self, keys: impl Iterator<Item = String>) {
        let mut visible = self.visible.borrow_mut();
        visible.clear();
        visible.extend(keys);
        // Drop scrolled-off row rect handles in long sessions once the map grows large.
        let mut rows = self.rows.borrow_mut();
        if rows.len() > 256 {
            rows.retain(|k, _| visible.iter().any(|v| v == k));
        }
    }
}
