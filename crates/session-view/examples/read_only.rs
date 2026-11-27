//! Standalone read-only session timeline app & storybook (`p255`).
//!
//! Run interactively:
//!   cargo run -p session-view --example read_only
//! Capture a headless dark still at zoom 1.6:
//!   cargo run -p session-view --example read_only -- --scene read-only --zoom 1.6 --size 1280x820 --scale 2 --theme dark --snap /tmp/read-only.png

use rune::{App, AppSpec, Cx, Playground};
use rune_knobs::Style;
use session_view::palette::pal;
use session_view::story::{
    DIAGRAM_SRC, Host, READ_ONLY_VARIANTS, SCENES, is_dark, on_key, open_composer_story,
    open_diagram_story,
    open_motion, open_read_only, open_scrollbar, poll, setup, toggle_fold, view,
};
use session_view::KNOB_GROUPS;

fn fresh(real: &Host) -> Host {
    let mut h = Host {
        knobs: real.knobs.clone(),
        clock: real.clock,
        now_ms: real.now_ms,
        base_ms: real.base_ms,
        zoom: real.zoom,
        fold_user: real.fold_user,
        variant: real.variant,
        show_composer: real.show_composer,
        busy: real.busy,
        theme: real.theme.clone(),
        waker: real.waker.clone(),
        ..Host::default()
    };
    if let Some(waker) = h.waker.clone() {
        h.view.setup(waker, true);
    }
    h
}

fn knobs_style(p: &session_view::palette::Pal) -> Style {
    if p.dark {
        return Style::dark();
    }
    Style {
        bg: p.bg_base,
        inset: p.bg_deep,
        raised: p.pressed,
        fg: p.text,
        fg2: p.muted,
        fg3: p.faint,
        fill: p.layer1,
        fill_hover: p.hover,
        fill_selected: p.pressed,
        slider: p.faint.alpha(0.55),
        slider_hover: p.faint.alpha(0.7),
        slider_active: p.muted.alpha(0.8),
        handle: p.text,
        border: p.ring,
        rule: p.text.alpha(0.03),
        rim: p.ring,
        shadow: p.shadow,
        outline: p.text.alpha(0.55),
    }
}

fn side(s: &Host, knobs: &'static [rune_knobs::Group], active: bool, cx: &mut Cx<Host>) -> rune::El {
    let p = pal(is_dark(cx));
    rune_knobs::inline(&s.knobs, knobs, knobs_style(p), |s: &mut Host| &mut s.knobs, active)
}

fn playground() -> Playground<Host> {
    let mut pg = Playground::<Host>::new()
        .scenes(|name| name.contains('/'))
        .fresh(fresh)
        .keep(|stage, real| real.knobs = stage.knobs.clone())
        .trigger("Session view/Read-only", 'r', "Replay", open_read_only)
        .trigger("Session view/Read-only", 'o', "Read more / Show less", |s, tx| toggle_fold(s, tx))
        .trigger("Session view/Read-only", 'u', "Page up", |_, tx| tx.scroll_page(Host::scroll_key(), -1))
        .trigger("Session view/Read-only", 'd', "Page down", |_, tx| tx.scroll_page(Host::scroll_key(), 1))
        .side_keys(
            "Session view/Read-only",
            |s, active, cx| side(s, KNOB_GROUPS, active, cx),
            rune_knobs::side_key(KNOB_GROUPS, |s: &mut Host| &mut s.knobs),
        )
        .trigger("Session view/Motion", 'r', "Replay", open_motion)
        .side_keys(
            "Session view/Motion",
            |s, active, cx| side(s, KNOB_GROUPS, active, cx),
            rune_knobs::side_key(KNOB_GROUPS, |s: &mut Host| &mut s.knobs),
        )
        .trigger("Session view/Scroll bar", 'r', "Replay", open_scrollbar)
        .trigger("Session view/Scroll bar", 'u', "Page up", |_, tx| tx.scroll_page(Host::scroll_key(), -1))
        .trigger("Session view/Scroll bar", 'd', "Page down", |_, tx| tx.scroll_page(Host::scroll_key(), 1))
        .trigger("Session view/Composer", 'r', "Replay", open_composer_story)
        .trigger("Session view/Composer", 'g', "Type two lines", |s, _| {
            s.edit = rune::TextEdit::new("First line\nSecond line");
        })
        .trigger("Session view/Composer", 'b', "Toggle busy", |s, tx| {
            s.busy = !s.busy;
            tx.instant();
        })
        .trigger("Session view/Diagram", 'r', "Replay", open_diagram_story)
        .trigger("Session view/Diagram", 'o', "Open full size", |s, tx| {
            session_view::diagram::open_source(s, DIAGRAM_SRC, tx);
        })
        .trigger("Session view/Diagram", 'c', "Close", |s, tx| session_view::diagram::close(s, tx));
    for &(name, v) in READ_ONLY_VARIANTS {
        pg = pg.variant("Session view/Read-only", name, move |s: &mut Host| s.variant = v);
    }
    pg
}

fn main() {
    let spec = AppSpec {
        scenes: SCENES,
        fonts: rune_theme_opencode::FONTS,
        families: ("Inter", "OpenMono"),
        setup: Some(setup_replay),
        poll: Some(poll),
        on_key: Some(on_key),
        ..AppSpec::new("Session View (Read-only)", (1280.0, 820.0), Host::from_args, view)
    };
    App::new(spec)
        .gpu_backend(rune::GpuBackend::Metal)
        .playground(playground())
        .run();
}

/// A saved API page can be inspected without connecting to any server. Private fixtures stay
/// outside the repo. With refs, `--media-dir` holds the encoded files named by their hash.
fn setup_replay(s: &mut Host, wake: rune::Waker) {
    setup(s, wake);
    let args: Vec<_> = std::env::args().collect();
    let arg = |flag| args.windows(2).find(|p| p[0] == flag).map(|p| p[1].clone());
    if let Some(path) = arg("--messages") {
        match std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
        {
            Ok(page) => {
                *s.talk.borrow_mut() = Some(opencode_kit::model::Transcript {
                    session: session_view::fixture::SESSION_ID.into(),
                    messages: opencode_kit::model::parse_messages(&page),
                });
            }
            Err(e) => eprintln!("session-view read_only: failed to load --messages {path}: {e}"),
        }
    }
    if let Some(dir) = arg("--media-dir") {
        s.view.media.set_fetch(std::sync::Arc::new(move |hash| {
            // Only content hashes are accepted by the attachment parser.
            std::fs::read(std::path::Path::new(&dir).join(hash)).map_err(|e| e.to_string())
        }));
    }
}
