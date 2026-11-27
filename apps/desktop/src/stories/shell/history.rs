//! The storybook's Navigation › Back and forward story: a session, another, Home, the first again
//! and its subagent, then ⌘[ three times and ⌘] once. The strip on top is the history.

use crate::stories::fixture_ids::{MAIN, CHILD, THIRD as OTHER};
use rune::{El, Play, Step, col, row, text};

use crate::palette::Pal;
use crate::{App, Route};

pub const KEYS: &[crate::stories::Trigger] = &[crate::stories::replay(open)];


fn open(s: &mut App, tx: &mut Play<App>) {
    s.finish_stream();
    let steps: [(f64, Step<App>); 8] = [
        (0.8, |s, tx| s.open(OTHER, tx)),
        (1.6, |s, tx| {
            *s.route = Route::Home;
            tx.instant();
        }),
        (2.4, |s, tx| s.open(MAIN, tx)),
        (3.2, |s, tx| s.open(CHILD, tx)),
        (4.4, |s, tx| {
            s.navigate(false, tx);
        }),
        (5.2, |s, tx| {
            s.navigate(false, tx);
        }),
        (6.0, |s, tx| {
            s.navigate(false, tx);
        }),
        (6.8, |s, tx| {
            s.navigate(true, tx);
        }),
    ];
    for (at, run) in steps {
        tx.after(at, run);
    }
}

pub fn view(s: &App, p: &Pal) -> El {
    let strip = s.nav.places().into_iter().enumerate().fold(row().gap(6.0).px(16.0).py(12.0), |strip, (i, place)| {
        let here = i == s.nav.index();
        let name = match &place.route {
            Route::Home => "Home".to_owned(),
            Route::Automations => "Automations".to_owned(),
            Route::Draft(_) => "New session".to_owned(),
            Route::Session { id, .. } => s.session(id).map_or(id.clone(), |x| x.title.chars().take(18).collect()),
        };
        let chip = col().px(8.0).py(4.0).radius(6.0).fill(if here { p.layer3 } else { p.layer1 });
        strip.child(chip.child(text(name).font(crate::view::UI).color(if here { p.text } else { p.muted })))
    });
    let page = match &*s.route {
        Route::Session { id, .. } => crate::view::session_page(s, p, id),
        _ => col().h_full().child(text("Home").font(crate::view::UI).color(p.muted).px(16.0)),
    };
    col().key("history-stage").w_full().h_full().child(strip).child(page)
}

rune_register::register!(crate::stories::STORY_LIST: crate::stories::Story = [
    crate::stories::Story { path: "Navigation/Back and forward", scene: |s| crate::stories::stage(s, crate::stories::history_view), knobs: &[], keys: KEYS, variants: &[] },
]);
