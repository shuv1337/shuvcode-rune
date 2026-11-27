//! The egg's elements on the new-session page (`apps/desktop/src/shell/view/draft.rs`): the wordmark with the lodger's
//! grip, the slots the egg draws in, the panel's pointer surface, the game's surface and HUD, and
//! what the egg does to the composer. At rest it's the plain wordmark and two invisible hit areas
//! in the "e"; the rest exists only while the egg is out.

use rune::{Anim, Curve, Cursor, El, Easing, PointerEvent, Transition, Tx, el, row, text};
use opencode_ui::palette::Pal;

use super::lodger::{HOLE, Stage};
use super::wordmark::{self, Grip, State};
use super::{EggHost, Geo, gpu};

/// The drop hint: the composer gives a little under the carried lodger (`wordmark.css:31-37`).
pub const SINK: Easing = Easing::cubic(0.34, 1.56, 0.64, 1.0);

struct WordmarkParts {
    boxed: El,
    geo: Geo,
    stage: Stage,
    near: Option<(El, [f64; 2])>,
    grip: Option<(El, [f64; 2])>,
    lifted: bool,
}

fn wordmark_parts(s: &dyn EggHost, p: &Pal, width: f32, top: Option<f32>, dark: bool) -> WordmarkParts {
    let geo = rest_geo(width, top, dark);
    s.egg().rest.set(geo);
    let stage = geo.stage();
    let live = s.egg().live.as_deref();
    if let Some(live) = live {
        live.geo.set(read_geo(geo));
    }
    let [bx, by, bw, bh] = geo.logo;
    let k = stage.k as f32;
    let at = [(stage.x - bx) as f32, (stage.y - by) as f32];
    // The egg's parts come and go at once: their motion is the egg's own.
    let mut boxed = el().key("wordmark-box").w(bw as f32).h(bh as f32).instant_changes();
    let (stem, weak) = letter_colors(p);
    let (stem, weak) = (stem.alpha(stem.a * 0.16), weak.alpha(weak.a * 0.16));
    let still = logo_at(k, stem, weak).key("wordmark").absolute(at[0], at[1]);
    // While the egg draws the letters (or the board stands in for them), the page's own hide at
    // once, in the frame the egg's show: never both, never a fade between.
    boxed = boxed.child(match live {
        None => still,
        Some(l) => still.opacity(if l.page.flat == super::Flat::Page { 1.0 } else { 0.0 }).opacity_motion(Anim::INSTANT),
    });
    if live.is_some() {
        boxed = boxed.child(el().key("egg-mid").transition(Transition::none()).absolute(at[0], at[1]).size(234.0 * k, 42.0 * k).effect(gpu::MID).a11y_hidden());
    }
    let state = live.map_or(State::Hidden, |l| l.state);
    // The eyes peek when the pointer comes within 24 units of the hole: a box around that reach,
    // until the egg is out and the panel takes the pointer.
    let near = live.is_none_or(|l| l.state == State::Hidden && l.board.is_none()).then(|| {
        let zone = [HOLE[0] - 24.0, HOLE[1] - 24.0];
        let el = el()
            .key("egg-near")
            .absolute(at[0] + zone[0] as f32 * k, at[1] + zone[1] as f32 * k)
            .size(48.0 * k, 48.0 * k)
            .a11y_hidden();
        (el, zone)
    });
    let grip = wordmark::grip(state);
    let grip = (grip != Grip::None).then(|| {
        let (gx, gy, gw, gh) = if grip == Grip::Hidden { (213.0, 8.0, 18.0, 14.0) } else { (234.0, 18.0, 24.0, 24.0) };
        let el = el()
            .key("egg-grip")
            .absolute(at[0] + gx as f32 * k, at[1] + gy as f32 * k)
            .size(gw as f32 * k, gh as f32 * k)
            // The keyboard's way in, where the pointer's is: in the tab order (Desktop's egg is
            // the pointer's alone), Enter or Space plays it as a hand would.
            .role("button")
            .label(opencode_i18n::t!("rune.egg.play"))
            .focusable(true)
            // A pull with the pointer leaves the composer focused, as Desktop's grip (its
            // `pointerdown` prevents the default) doesn't take focus.
            .preserve_focus_on_press(true)
            // Desktop's grip is inside the panel that hears the pointer (`wordmark.tsx:1177-1179`):
            // over it the eyes still follow, and the pointer hasn't left. Here it's a layer over
            // the "e"'s reach and the panel's surface: hover passes through it to them, and it
            // sends on the moves it takes (`wordmark`).
            .hover_passes_below()
            // No `.cursor`: the arrow stays over the grip and through a pull, by the owner's word
            // (25 Nov). Desktop shows a hand (`cursor-grab active:cursor-grabbing`, `wordmark.tsx:295`).
            // `touch-none` (`wordmark.tsx:295`): a finger pulls the lodger from where it lands.
            .touch_drags_at_once();
        (el, [gx, gy])
    });
    let lifted = live.is_some_and(|l| wordmark::lifted(l.state));
    WordmarkParts { boxed, geo, stage, near, grip, lifted }
}

fn wordmark_row(boxed: El, lifted: bool) -> El {
    row().key("wordmark-row").w_full().justify_center().paint_order(if lifted { 1 } else { 0 }).child(boxed)
}

/// The new-session wordmark (`view.tsx:105-113`): its box spans 80% of up to 720 pt at the logo's
/// 234:42; the logo inside is 78x14 whole-point pitches, `max(3, round(box / 78))`, its origin
/// rounded in the panel (`fitStage`). `width` and `height` are the panel's, from the last layout;
/// `top` is the box's top in the panel.
pub fn wordmark<H: EggHost + 'static>(s: &H, p: &Pal, width: f32, top: Option<f32>, dark: bool) -> El {
    let WordmarkParts { mut boxed, geo, stage, near, grip, lifted } = wordmark_parts(s, p, width, top, dark);
    if let Some((near, zone)) = near {
        boxed = boxed.child(
            near.on_pointer_move_local(move |s: &mut H, e: PointerEvent, tx: &mut Tx| {
                super::pointer_move_at(s, geo, stage.x + zone[0] * stage.k + f64::from(e.x), stage.y + zone[1] * stage.k + f64::from(e.y), tx);
            })
            .on_hover_change(|s: &mut H, inside, tx| if !inside { super::pointer_leave(s, tx) }),
        );
    }
    if let Some((grip, [gx, gy])) = grip {
        let at = move |x: f32, y: f32| (stage.x + gx * stage.k + f64::from(x), stage.y + gy * stage.k + f64::from(y));
        boxed = boxed.child(
            grip.on_key(|s: &mut H, ev, tx| super::grip_key(s, ev, tx))
                // A pointer over the grip is over the "e"'s reach (or the panel) too: the eyes
                // keep looking at it.
                .on_pointer_move_local(move |s: &mut H, e: PointerEvent, tx: &mut Tx| {
                    let (x, y) = at(e.x, e.y);
                    super::pointer_move_at(s, geo, x, y, tx);
                })
                .on_drag(move |s: &mut H, d, tx| {
                    let (x, y) = at(d.x, d.y);
                    match d.phase {
                        rune::Phase::Down if tx.button() == rune::PointerButton::Primary => super::grab_at(s, geo, x, y, tx),
                        rune::Phase::Down => {}
                        rune::Phase::Move => super::pointer_move_at(s, geo, x, y, tx),
                        rune::Phase::Up => super::release(s, tx),
                    }
                }),
        );
    }
    wordmark_row(boxed, lifted)
}

/// The logo's blocks at `k` points a unit (234 x 42 units): letters in `stem`, the counters'
/// lower halves in `weak`.
pub fn logo_at(k: f32, stem: rune::Color, weak: rune::Color) -> El {
    // shuv1337 fork: "shuvcode" wordmark (matching packages/tui/src/shuv-logo.ts in shuv1337/shuvcode).
    // x, y, w, h in the logo's 234x42 units; `true` for a counter's lower half.
    const RECTS: [(f32, f32, f32, f32, bool); 38] = [
        // s
        (0., 6., 24., 6., false), (0., 12., 6., 12., false), (6., 18., 18., 6., false), (18., 24., 6., 12., false), (0., 30., 18., 6., false), (0., 24., 18., 6., true),
        // h
        (30., 0., 6., 36., false), (36., 6., 18., 6., false), (48., 12., 6., 24., false), (36., 18., 12., 18., true),
        // u
        (60., 6., 6., 30., false), (78., 6., 6., 30., false), (66., 30., 12., 6., false), (66., 18., 12., 12., true),
        // v
        (90., 6., 6., 24., false), (108., 6., 6., 24., false), (96., 30., 12., 6., false), (96., 18., 12., 12., true),
        // c
        (120., 6., 24., 6., false), (120., 12., 6., 18., false), (120., 30., 24., 6., false), (126., 18., 18., 12., true),
        // o
        (150., 6., 24., 6., false), (150., 12., 6., 24., false), (168., 12., 6., 24., false), (156., 30., 12., 6., false), (156., 18., 12., 12., true),
        // d
        (180., 6., 18., 6., false), (198., 0., 6., 36., false), (180., 12., 6., 24., false), (186., 30., 12., 6., false), (186., 18., 12., 12., true),
        // e
        (210., 6., 24., 6., false), (210., 12., 6., 24., false), (228., 12., 6., 12., false), (216., 18., 12., 6., false), (216., 30., 18., 6., false), (216., 24., 18., 6., true),
    ];
    // A picture, as Desktop's SVG and canvas are: right to left doesn't mirror it.
    let mut logo = el().size(78.0 * 3.0 * k, 14.0 * 3.0 * k).dir(rune::Direction::Ltr);
    for (x, y, w, h, counter) in RECTS {
        logo = logo.child(el().absolute(x * k, y * k).size(w * k, h * k).fill(if counter { weak } else { stem }));
    }
    logo
}

/// The panel's geometry from arithmetic on its size, as the draft page lays it out: what the egg
/// starts from before a layout of its own has been read.
fn rest_geo(width: f32, top: Option<f32>, dark: bool) -> Geo {
    let width = f64::from(if width > 0.0 { width } else { 1280.0 });
    let span = 0.8 * (width - 48.0).min(720.0);
    let content = (width - 48.0).min(880.0);
    let x = 24.0 + (width - 48.0 - content) / 2.0 + (content - span) / 2.0;
    let y = f64::from(top.unwrap_or(0.0));
    Geo { panel: [0.0, 0.0, width, 0.0], logo: [x, y, span, span * 42.0 / 234.0], composer: None, wrap: None, radius: f64::from(opencode_ui::style::COMPOSER_RADIUS), dark }
}

/// The geometry the last layout put on screen, read while the egg is out (each read re-runs
/// the view when it moves).
fn read_geo(rest: Geo) -> Geo {
    let Some(panel) = rune::memo::layout("draft") else { return rest };
    let rel = |r: [f32; 4]| [f64::from(r[0] - panel[0]), f64::from(r[1] - panel[1]), f64::from(r[2]), f64::from(r[3])];
    let logo = rune::memo::layout("wordmark-box").map_or(rest.logo, rel);
    let composer = rune::memo::layout("composer").map(rel);
    let wrap = rune::memo::layout("draft-composer").map(rel);
    Geo { panel: [f64::from(panel[0]), f64::from(panel[1]), f64::from(panel[2]), f64::from(panel[3])], logo, composer, wrap, ..rest }
}

fn under_parts(s: &dyn EggHost) -> Option<(Option<El>, El)> {
    let live = s.egg().live.as_deref()?;
    let slot = (live.board.is_some() || live.game.is_some()).then(|| el().key("egg-back").transition(Transition::none()).pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0)).effect(gpu::BACK).a11y_hidden());
    let surface = el()
        .key("egg-panel")
        .transition(Transition::none())
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .a11y_hidden()
        // A tap knocks at once, and a finger held on the board is never a long press.
        .touch_drags_at_once();
    Some((slot, surface))
}

/// The slot under everything on the panel (the pins behind the composer, the paddle), and the
/// panel's pointer surface: the board leans toward the pointer everywhere on the panel, and a
/// press anywhere off its controls knocks pins out.
pub fn under<H: EggHost + 'static>(s: &H) -> [Option<El>; 2] {
    let Some((slot, surface)) = under_parts(s) else { return [None, None] };
    let surface = surface
        .on_pointer_move_local(|s: &mut H, e: PointerEvent, tx: &mut Tx| super::pointer_move(s, f64::from(e.x), f64::from(e.y), tx))
        .on_hover_change(|s: &mut H, inside, tx| if !inside { super::pointer_leave(s, tx) } else { super::pointer_entered(s, tx) })
        .on_drag(|s: &mut H, d, tx| match d.phase {
            rune::Phase::Down if tx.button() == rune::PointerButton::Primary => super::panel_press(s, f64::from(d.x), f64::from(d.y), tx),
            rune::Phase::Up => super::panel_release(s),
            _ => {}
        });
    [slot, Some(surface)]
}

/// The pins popped toward the viewer, between the composer's tray and the composer (`FRONT`).
pub fn front(s: &dyn EggHost) -> Option<El> {
    let live = s.egg().live.as_deref()?;
    live.board.as_ref()?;
    Some(el().key("egg-front").transition(Transition::none()).absolute(0.0, 0.0).size(1.0, 1.0).effect(gpu::FRONT).a11y_hidden())
}

/// The composer while the egg is out: it sinks a little under the carried lodger, and fades out
/// for the game (160 ms ease-out), when it takes no input.
pub fn composer_wrap(s: &dyn EggHost, wrap: El) -> El {
    let Some(live) = s.egg().live.as_deref() else { return wrap };
    let over = wordmark::drop_over(live.state);
    let sink = Anim::new(Curve::tween(0.26, SINK));
    let wrap = wrap.offset(0.0, if over { 3.0 } else { 0.0 }).scale(if over { 0.985 } else { 1.0 }).offset_motion(sink).scale_motion(sink);
    if live.page.composer_hidden { wrap.inert() } else { wrap }
}

/// One of the composer wrap's parts (the composer, its tray) fading for the game.
pub fn composer_part(s: &dyn EggHost, part: El) -> El {
    let Some(live) = s.egg().live.as_deref() else { return part };
    part.opacity(if live.page.composer_hidden { 0.0 } else { 1.0 }).opacity_motion(Anim::new(Curve::tween(0.16, Easing::EASE_OUT)))
}

fn over_parts(s: &dyn EggHost, p: &Pal) -> Option<(El, El)> {
    let live = s.egg().live.as_deref()?;
    let g = live.game.as_deref()?;
    // Tailwind's `transition-*` with `duration-300`: its default curve.
    let css = || Anim::new(Curve::tween(0.3, Easing::cubic(0.4, 0.0, 0.2, 1.0)));
    let pills = (0..super::brick::machine::LIVES - 1).map(|i| {
        let left = i < g.spares;
        el().key(format!("egg-spare-{i}")).size(20.0, 6.0).radius(3.0).fill(p.icon_faint)
            .opacity(if left { 1.0 } else { 0.0 }).offset(0.0, if left { 0.0 } else { 4.0 })
            .opacity_motion(css()).offset_motion(css())
    });
    let hud = el()
        .key("egg-hud")
        .transition(Transition::none())
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        .a11y_hidden()
        .opacity(if g.hud { 1.0 } else { 0.0 })
        .opacity_motion(css())
        .child(el().key("egg-wall-l").pin(Some((g.l - 1.0) as f32), Some(0.0), None, Some(0.0)).w(1.0).fill(p.border))
        .child(el().key("egg-wall-r").pin(Some(g.r as f32), Some(0.0), None, Some(0.0)).w(1.0).fill(p.border))
        .child(text(g.score_text()).key("egg-score").font(SCORE).color(p.faint).pin(None, Some(12.0), Some(16.0), None))
        .child(row().key("egg-spares").gap(6.0).pin(Some(16.0), None, None, Some(16.0)).children(pills));
    let surface = el()
        .key("egg-game")
        .transition(Transition::none())
        .pin(Some(0.0), Some(0.0), Some(0.0), Some(0.0))
        // What a reader says as the game takes focus: the keys it plays with.
        .role("group")
        .label(opencode_i18n::t!("rune.egg.help"))
        .cursor(if live.page.cursor_hidden { Cursor::Hidden } else { Cursor::Default })
        // It holds the keyboard while the game plays, as Desktop's window-wide key listener does.
        .focusable(true)
        // A finger steers the paddle from where it lands, and holding it still keeps steering.
        .touch_drags_at_once();
    Some((hud, surface))
}

/// Brick Breaker's surface over the panel (it takes every press, as Desktop's capture-phase
/// listener does) and its HUD (`brick-breaker.tsx:56-86`).
pub fn over<H: EggHost + 'static>(s: &H, p: &Pal) -> Vec<El> {
    let Some((hud, surface)) = over_parts(s, p) else { return Vec::new() };
    let surface = surface
        .on_key(|s: &mut H, ev, tx| super::game_key(s, ev, true, tx))
        .on_key_up(|s: &mut H, ev, tx| super::game_key(s, ev, false, tx))
        .on_physical_input(|s: &mut H, ev, _| {
            if matches!(ev, rune::PhysicalInput::FocusLost) {
                super::blur(s);
            }
            false
        })
        .on_pointer_move_local(|s: &mut H, e: PointerEvent, tx: &mut Tx| super::pointer_move(s, f64::from(e.x), f64::from(e.y), tx))
        .on_drag(|s: &mut H, d, tx| match d.phase {
            rune::Phase::Down if tx.button() == rune::PointerButton::Primary => super::panel_press(s, f64::from(d.x), f64::from(d.y), tx),
            rune::Phase::Move => super::pointer_move(s, f64::from(d.x), f64::from(d.y), tx),
            _ => {}
        });
    vec![hud, surface]
}

/// The score (`font-mono text-[11px] leading-[16px] tabular-nums`).
const SCORE: rune::TextStyle = rune::TextStyle::new(11.0, 400.0).mono().lh(16.0).features("\"tnum\"");

/// The flat wordmark's ink (`wordmark.tsx:240-242`, `wordmark.css:44-46`): Desktop's v1
/// `--icon-base` for the letters and `--icon-weak-base` for the counters' lower halves (in
/// light, the letters' at 30%); a chosen theme's nearest tokens.
pub fn letter_colors(p: &Pal) -> (rune::Color, rune::Color) {
    let hex = |h: &str| rune::color::hex(h).unwrap_or(rune::Color::BLACK);
    match (p.theme, p.dark) {
        (0, true) => (hex("#7e7e7e"), hex("#343434")),
        (0, false) => (hex("#8f8f8f"), hex("#8f8f8f").alpha(0.3)),
        (_, _) => (p.standard.icon, p.standard.icon.alpha(0.3)),
    }
}
