//! What the egg's slots draw this frame (`gpu::Scene`), in window device pixels: the pin layers'
//! regions, the flat wordmark with its notch and shimmer, the lodger, and the paddle.

use rune::Color;
use opencode_ui::palette::Pal;

use super::gpu::{BACK, FRONT, MID, Shape};
use super::lodger::render::Affine;
use super::{Live, SHIMMER, Shimmer};

/// A pin layer's region in panel points, its texture's pixels and scale, and where it lands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerAt {
    pub region: [f64; 2],
    pub pixels: [u32; 2],
    pub scale: f32,
    pub dest: [f32; 4],
}

const RECT: f32 = 0.0;
const RRECT: f32 = 1.0;
const CAPSULE: f32 = 2.0;
const PIN: f32 = 3.0;
const SHIMMER_KIND: f32 = 4.0;
const SHADOW: f32 = 5.0;
const RING: f32 = 6.0;

/// shuv1337 fork: "shuvcode" logo blocks in 234x42 units: x, y, w, h, and whether it's a
/// counter's lower half (`--icon-weak-base`).
const RECTS: [(f64, f64, f64, f64, bool); 38] = [
    (0., 6., 24., 6., false), (0., 12., 6., 12., false), (6., 18., 18., 6., false), (18., 24., 6., 12., false), (0., 30., 18., 6., false), (0., 24., 18., 6., true),
    (30., 0., 6., 36., false), (36., 6., 18., 6., false), (48., 12., 6., 24., false), (36., 18., 12., 18., true),
    (60., 6., 6., 30., false), (78., 6., 6., 30., false), (66., 30., 12., 6., false), (66., 18., 12., 12., true),
    (90., 6., 6., 24., false), (108., 6., 6., 24., false), (96., 30., 12., 6., false), (96., 18., 12., 12., true),
    (120., 6., 24., 6., false), (120., 12., 6., 18., false), (120., 30., 24., 6., false), (126., 18., 18., 12., true),
    (150., 6., 24., 6., false), (150., 12., 6., 24., false), (168., 12., 6., 24., false), (156., 30., 12., 6., false), (156., 18., 12., 12., true),
    (180., 6., 18., 6., false), (198., 0., 6., 36., false), (180., 12., 6., 24., false), (186., 30., 12., 6., false), (186., 18., 12., 12., true),
    (210., 6., 24., 6., false), (210., 12., 6., 24., false), (228., 12., 6., 12., false), (216., 18., 12., 6., false), (216., 30., 18., 6., false), (216., 24., 18., 6., true),
];

/// sRGB-encoded channels, 0 to 1, of a colour.
fn enc(c: Color) -> [f64; 3] {
    [f64::from(rune::color::srgb_encode(c.r)), f64::from(rune::color::srgb_encode(c.g)), f64::from(rune::color::srgb_encode(c.b))]
}

/// CSS `color-mix(in srgb, a t, b)`.
fn mix(a: Color, t: f64, b: Color) -> [f64; 3] {
    let (a, b) = (enc(a), enc(b));
    [a[0] * t + b[0] * (1.0 - t), a[1] * t + b[1] * (1.0 - t), a[2] * t + b[2] * (1.0 - t)]
}

fn premul(c: [f64; 3], a: f64) -> [f32; 4] {
    [(c[0] * a) as f32, (c[1] * a) as f32, (c[2] * a) as f32, a as f32]
}

pub fn scene(scene: &mut super::gpu::Scene, live: &Live, p: &Pal, slots: &[(u32, [f32; 4], f32)], now: f64) {
    let at = |mark: u32| slots.iter().find(|s| s.0 == mark).map(|s| (s.1, f64::from(s.2)));
    scene.back = None;
    scene.front = None;
    scene.paddle.clear();
    scene.mid.clear();
    if let Some(b) = live.board.as_deref() {
        for (n, q) in [&b.back, &b.front].into_iter().enumerate() {
            scene.quads[n].0.clear();
            scene.quads[n].0.extend_from_slice(&q.list);
            scene.quads[n].1 = q.shadows;
        }
    }
    // The panel's origin in device px, as Chromium paints Desktop's: rounded up to whole pixels,
    // with what's inside it at its own fractional offsets from there (measured: at 1.875x the
    // panel at 84.375 paints from 85). Before the egg has read a layout, the slots' own rects.
    let geo = live.geo.get();
    let origin = |rect: [f32; 4], scale: f64| if geo.panel[3] > 0.0 { [(geo.panel[0] * scale).ceil(), (geo.panel[1] * scale).ceil()] } else { [f64::from(rect[0]), f64::from(rect[1])] };
    if let (Some((rect, scale)), Some(b)) = (at(BACK), live.board.as_deref()) {
        let r = b.pins.view.region;
        scene.back = Some(layer(origin(rect, scale), [r.x, r.y, r.w, r.h], scale));
    }
    // The front slot only marks its place in the painter's order; the panel is the back slot's.
    if let (Some(_), Some((rect, scale)), Some(b)) = (at(FRONT), at(BACK), live.board.as_deref())
        && b.front_on
    {
        let v = b.pins.view;
        scene.front = Some(layer(origin(rect, scale), [0.0, 0.0, v.w, v.h], scale));
    }
    if let (Some((rect, scale)), Some(g)) = (at(BACK), live.game.as_deref()) {
        paddle(&mut scene.paddle, g, p, origin(rect, scale), scale, now);
    }
    if let Some((rect, scale)) = at(MID) {
        let stage = live.lodger.stage;
        let rect = if geo.panel[3] > 0.0 {
            let [x, y] = origin(rect, scale);
            [(x + stage.x * scale) as f32, (y + stage.y * scale) as f32, (234.0 * stage.k * scale) as f32, rect[3]]
        } else {
            rect
        };
        mid(&mut scene.mid, live, p, rect, now);
    }
}

fn layer(origin: [f64; 2], r: [f64; 4], scale: f64) -> LayerAt {
    let dpr = scale.min(2.0);
    LayerAt {
        region: [r[0], r[1]],
        pixels: [(r[2] * dpr).round().max(1.0) as u32, (r[3] * dpr).round().max(1.0) as u32],
        scale: dpr as f32,
        dest: [(origin[0] + r[0] * scale) as f32, (origin[1] + r[1] * scale) as f32, (r[2] * scale) as f32, (r[3] * scale) as f32],
    }
}

/// The wordmark's box: the flat letters (while shown), the notch, the shimmer, then the lodger.
fn mid(out: &mut Vec<Shape>, live: &Live, p: &Pal, rect: [f32; 4], now: f64) {
    out.clear();
    let kd = f64::from(rect[2]) / 234.0;
    let stage = Affine([kd, 0.0, 0.0, kd, f64::from(rect[0]), f64::from(rect[1])]);
    let d = &live.lodger.drawn;
    let dark = live.geo.get().dark;
    // While the flat wordmark is at rest the page draws it (iew::wordmark); the egg only while it moves.
    if live.page.flat == super::Flat::Egg {
        let (stem, weak) = super::view::letter_colors(p);
        let (weak_a, stem, weak) = (f64::from(weak.a), enc(stem), enc(weak));
        for &(x, y, w, h, counter) in &RECTS {
            let i = ((x / 30.0) as usize).min(7);
            let (opacity, m) = d.letters[i];
            let a = opacity * 0.16;
            let color = if counter { premul(weak, a * weak_a) } else { premul(stem, a) };
            out.push(axis_rect(stage.then(m), [x, y, w, h], color, RECT, [0.0; 4]));
        }
        let (opacity, m) = d.letters[7];
        if d.notch * opacity > 0.0 {
            out.push(axis_rect(stage.then(m), [224.6, 8.6, 3.8, 3.8], premul(enc(p.bg_base), d.notch * opacity), RECT, [0.0; 4]));
        }
        if let Shimmer::Running { since } = live.shimmer {
            let t = ((now - since) / SHIMMER).clamp(0.0, 1.0);
            let peak = if dark { 0.0672 } else { 0.45 };
            let o = if t < 0.2 { peak * t / 0.2 } else if t <= 0.8 { peak } else { peak * (1.0 - t) / 0.2 };
            let w = 234.0 * kd;
            let x50 = f64::from(rect[0]) + (-0.5 + 2.0 * t) * w;
            let mask = [(x50 - 0.3 * w) as f32, x50 as f32, (x50 + 0.3 * w) as f32, 0.0];
            for &(x, y, ww, h, counter) in &RECTS {
                if !counter {
                    out.push(axis_rect(stage, [x, y, ww, h], premul([1.0; 3], o), SHIMMER_KIND, mask));
                }
            }
        }
    }
    let body = mix(p.icon, 0.22, p.bg_base);
    let side = mix(p.icon, 0.38, p.bg_base);
    let eye = enc(p.icon);
    if d.neck > 0.0 {
        let [x1, y1, x2, y2] = d.neck_line;
        let a = stage.apply(x1, y1);
        let b = stage.apply(x2, y2);
        out.push(capsule(a, b, d.neck_w * kd / 2.0, premul(body, d.neck)));
    }
    if d.pin > 0.0 {
        let m = stage.then(d.pin_at);
        out.push(transformed(m, [-1.9, -1.9, 3.8, 3.8], 0.0, premul(eye, 1.0), PIN, [side[0] as f32, side[1] as f32, side[2] as f32, d.pin as f32]));
    }
    let critter = stage.then(d.critter);
    if d.body > 0.0 {
        let w = 12.0 - 2.0 * d.curl;
        let r = (1.5 + 3.5 * d.curl).min(w / 2.0).min(5.0);
        out.push(transformed(critter, [-w / 2.0, -10.0, w, 10.0], r, premul(body, d.body), RRECT, [0.0; 4]));
    }
    if d.eyes > 0.0 {
        let blink = live.lodger.blink_scale_at(now);
        let eyes = critter.then(d.look).then(Affine::translate(0.0, -6.0)).then(Affine::scale(1.0, blink)).then(Affine::translate(0.0, 6.0));
        for x in [-3.5, 1.5] {
            out.push(transformed(eyes, [x, -7.3, 2.0, 2.6], 0.5, premul(eye, d.eyes), RRECT, [0.0; 4]));
        }
    }
}

/// The paddle: the composer's box shrinking into it and back, `--v2-elevation-raised` under it.
fn paddle(out: &mut Vec<Shape>, g: &super::brick::Game, p: &Pal, origin: [f64; 2], scale: f64, now: f64) {
    out.clear();
    let opacity = g.opacity.value(now);
    if opacity <= 0.0 {
        return;
    }
    let b = g.drawn;
    let surface = enc(opencode_ui::style::composer_fill(p));
    let muted = enc(p.icon_muted);
    let t = g.paint.value(now);
    let fill = [surface[0] + (muted[0] - surface[0]) * t, surface[1] + (muted[1] - surface[1]) * t, surface[2] + (muted[2] - surface[2]) * t];
    let center = [origin[0] + (b.x + b.w / 2.0) * scale, origin[1] + (b.y + b.h / 2.0) * scale];
    let (sin, cos) = g.drawn_tilt.to_radians().sin_cos();
    let half = [b.w / 2.0 * scale, b.h / 2.0 * scale];
    let r = b.r * scale;
    // Box shadows rotate with the box; the first in the list paints on top.
    for sh in p.raised.iter().rev() {
        let color = premul(enc(sh.color), f64::from(sh.color.a) * opacity);
        let (ox, oy) = (f64::from(sh.x) * scale, f64::from(sh.y) * scale);
        let c = [center[0] + ox * cos - oy * sin, center[1] + ox * sin + oy * cos];
        let sp = f64::from(sh.spread) * scale;
        let ring = sh.x == 0.0 && sh.y == 0.0;
        if sh.blur <= 0.0 && ring {
            if sp > 0.0 {
                out.push(box_shape(c, half, r, [cos, sin], color, RING, sp));
            }
        } else if sh.blur <= 0.0 {
            out.push(box_shape(c, [half[0] + sp, half[1] + sp], if r > 0.0 { r + sp } else { 0.0 }, [cos, sin], color, RRECT, 0.0));
        } else {
            let sigma = (f64::from(sh.blur) * scale * 0.5).max(0.5);
            out.push(box_shape(c, [half[0] + sp, half[1] + sp], if r > 0.0 { r + sp } else { 0.0 }, [cos, sin], color, SHADOW, sigma));
        }
    }
    out.push(box_shape(center, half, r, [cos, sin], premul(fill, opacity), RRECT, 0.0));
}

/// An axis-aligned rect of the logo through `m` (a translate and a scale), drawn with Desktop's
/// exact pixel coverage.
fn axis_rect(m: Affine, [x, y, w, h]: [f64; 4], color: [f32; 4], kind: f32, extra: [f32; 4]) -> Shape {
    let a = m.apply(x, y);
    let b = m.apply(x + w, y + h);
    let (x0, x1) = (a[0].min(b[0]), a[0].max(b[0]));
    let (y0, y1) = (a[1].min(b[1]), a[1].max(b[1]));
    let c = [(x0 + x1) / 2.0, (y0 + y1) / 2.0];
    shape([x0 - 1.0, y0 - 1.0, x1 + 1.0, y1 + 1.0], [1.0, 0.0, 0.0, 1.0], c, kind, [(x1 - x0) / 2.0, (y1 - y0) / 2.0, 0.0, 0.0], color, extra)
}

/// The box `[x, y, w, h]` with corner `r`, in a frame `m` maps to device pixels.
fn transformed(m: Affine, [x, y, w, h]: [f64; 4], r: f64, color: [f32; 4], kind: f32, extra: [f32; 4]) -> Shape {
    let [a, b, c, d, e, f] = m.0;
    let det = a * d - b * c;
    let det = if det.abs() < 1e-12 { 1e-12 } else { det };
    // Device to local is the inverse of the linear part.
    let inv = [d / det, -c / det, -b / det, a / det];
    let center = m.apply(x + w / 2.0, y + h / 2.0);
    let corners = [m.apply(x, y), m.apply(x + w, y), m.apply(x, y + h), m.apply(x + w, y + h)];
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    for [cx, cy] in corners {
        x0 = x0.min(cx);
        y0 = y0.min(cy);
        x1 = x1.max(cx);
        y1 = y1.max(cy);
    }
    let _ = (e, f);
    shape([x0 - 2.0, y0 - 2.0, x1 + 2.0, y1 + 2.0], inv, center, kind, [w / 2.0, h / 2.0, r, 0.0], color, extra)
}

fn capsule(a: [f64; 2], b: [f64; 2], hw: f64, color: [f32; 4]) -> Shape {
    let pad = hw + 2.0;
    shape([a[0].min(b[0]) - pad, a[1].min(b[1]) - pad, a[0].max(b[0]) + pad, a[1].max(b[1]) + pad], [1.0, 0.0, 0.0, 1.0], a, CAPSULE, [b[0], b[1], 0.0, hw], color, [0.0; 4])
}

/// A box of `half` extents and corner `r` about `center`, turned by (cos, sin), in device px.
fn box_shape(center: [f64; 2], half: [f64; 2], r: f64, rot: [f64; 2], color: [f32; 4], kind: f32, w: f64) -> Shape {
    let [cos, sin] = rot;
    let reach = half[0].hypot(half[1]) + w * 3.0 + 2.0;
    // Device to local: turn back by the box's angle.
    shape([center[0] - reach, center[1] - reach, center[0] + reach, center[1] + reach], [cos, sin, -sin, cos], center, kind, [half[0], half[1], r, w], color, [0.0; 4])
}

fn shape(bounds: [f64; 4], m: [f64; 4], t: [f64; 2], kind: f32, size: [f64; 4], color: [f32; 4], extra: [f32; 4]) -> Shape {
    let f = |v: f64| v as f32;
    [
        f(bounds[0]), f(bounds[1]), f(bounds[2]), f(bounds[3]),
        f(m[0]), f(m[1]), f(m[2]), f(m[3]),
        f(t[0]), f(t[1]), kind, 0.0,
        f(size[0]), f(size[1]), f(size[2]), f(size[3]),
        color[0], color[1], color[2], color[3],
        extra[0], extra[1], extra[2], extra[3],
    ]
}
