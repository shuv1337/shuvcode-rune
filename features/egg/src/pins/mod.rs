//! The new-session wordmark as a 3D pin board, Desktop's `new-session/pin-board.tsx` without its
//! DOM: pins tilt toward the pointer and sink under it; a click or drag knocks pins out, they pop
//! toward the viewer, pile up on the composer, then slot back in. In a game the pins are Brick
//! Breaker's bricks. Coordinates are the new-session panel's points; `draw.rs` turns the pins into
//! the quads `gpu.rs` draws.

pub mod draw;
pub mod machine;

use machine::{Board, Event, Pin, PinPhase};

use super::rng::Rng;
use super::sound::Cue;

/// shuv1337 fork: the "shuvcode" wordmark as a 78x14 pin grid: 1 = counter shadow, 2 = "shuv", 3 = "code".
const GRID: [&str; 14] = [
    "..........22......................................................33..........",
    "..........22......................................................33..........",
    "22222222..22222222..22....22..22....22..33333333..33333333..33333333..33333333",
    "22222222..22222222..22....22..22....22..33333333..33333333..33333333..33333333",
    "22........22....22..22....22..22....22..33........33....33..33....33..33....33",
    "22........22....22..22....22..22....22..33........33....33..33....33..33....33",
    "22222222..22111122..22111122..22111122..33111111..33111133..33111133..33333333",
    "22222222..22111122..22111122..22111122..33111111..33111133..33111133..33333333",
    "11111122..22111122..22111122..22111122..33111111..33111133..33111133..33111111",
    "11111122..22111122..22111122..22111122..33111111..33111133..33111133..33111111",
    "22222222..22111122..22222222....2222....33333333..33333333..33333333..33333333",
    "22222222..22111122..22222222....2222....33333333..33333333..33333333..33333333",
    "..............................................................................",
    "..............................................................................",
];

pub const COLS: f64 = 78.0;
pub const ROWS: f64 = 14.0;
pub const REST_TILT: [f64; 2] = [0.42, 0.7];
/// The reveal's span with the `sweep` entrance (the story's static board).
const REVEAL: f64 = 0.75;

/// How the board comes in: columns rising left to right (`sweep`), or a chain reaction from the
/// starter pin in the "e" (`chain`, every time the lodger pops it out).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entrance {
    Sweep,
    Chain,
}

/// The board's footprint and the surfaces pins land on, in panel points.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct View {
    pub w: f64,
    pub h: f64,
    pub x0: f64,
    pub y0: f64,
    pub pitch: f64,
    pub depth: f64,
    pub size: f64,
    pub activity: f64,
    /// The composer's left, right and top edges.
    pub composer: Option<[f64; 3]>,
    /// The back layer's region: the logo while it's intact, the whole panel while pins are loose.
    pub region: Region,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub full: bool,
}

/// Pointer state over the pin board: outside, entering from outside (first move anchors `lx, ly`
/// with no entry velocity), inside with tracked velocity, or re-entering during an active drag.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Pointer {
    #[default]
    Outside,
    Entering,
    Inside { x: f64, y: f64, lx: f64, ly: f64, vx: f64, vy: f64 },
    Reentering { x: f64, y: f64, vx: f64, vy: f64 },
}

#[derive(Clone, Copy, Debug)]
enum PointerEvent {
    Enter,
    Move { x: f64, y: f64 },
    Leave,
    Down { x: f64, y: f64 },
    Steer { x: f64, y: f64 },
    Step { dt: f64 },
}

impl Pointer {
    fn step(&mut self, event: PointerEvent) -> Option<(f64, f64, f64, f64)> {
        match (*self, event) {
            (Self::Outside, PointerEvent::Enter) => {
                *self = Self::Entering;
                None
            }
            (Self::Entering | Self::Reentering { .. }, PointerEvent::Enter) => None,
            (Self::Inside { x, y, vx, vy, .. }, PointerEvent::Enter) => {
                *self = Self::Reentering { x, y, vx, vy };
                None
            }
            (Self::Outside | Self::Entering, PointerEvent::Move { x, y } | PointerEvent::Steer { x, y }) => {
                *self = Self::Inside { x, y, lx: x, ly: y, vx: 0.0, vy: 0.0 };
                None
            }
            (Self::Reentering { vx, vy, .. }, PointerEvent::Move { x, y } | PointerEvent::Steer { x, y }) => {
                *self = Self::Inside { x, y, lx: x, ly: y, vx, vy };
                None
            }
            (Self::Inside { lx, ly, vx, vy, .. }, PointerEvent::Move { x, y } | PointerEvent::Steer { x, y }) => {
                *self = Self::Inside { x, y, lx, ly, vx, vy };
                None
            }
            (_, PointerEvent::Leave) => {
                *self = Self::Outside;
                None
            }
            (Self::Inside { lx, ly, vx, vy, .. }, PointerEvent::Down { x, y }) => {
                *self = Self::Inside { x, y, lx, ly, vx, vy };
                None
            }
            (Self::Reentering { vx, vy, .. }, PointerEvent::Down { x, y }) => {
                *self = Self::Reentering { x, y, vx, vy };
                None
            }
            (Self::Outside | Self::Entering, PointerEvent::Down { .. } | PointerEvent::Step { .. }) => None,
            (Self::Inside { x, y, lx, ly, mut vx, mut vy }, PointerEvent::Step { dt }) => {
                vx += ((x - lx) / dt - vx) * 0.5;
                vy += ((y - ly) / dt - vy) * 0.5;
                *self = Self::Inside { x, y, lx: x, ly: y, vx, vy };
                Some((x, y, vx, vy))
            }
            (Self::Reentering { x, y, mut vx, mut vy }, PointerEvent::Step { .. }) => {
                vx *= 0.5;
                vy *= 0.5;
                *self = Self::Reentering { x, y, vx, vy };
                Some((x, y, vx, vy))
            }
        }
    }
}

/// What a ball hitting the wall did (`PinBoardHandle.collide`).
#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub nx: f64,
    pub ny: f64,
    pub count: usize,
}

pub struct Pins {
    pub pins: Vec<Pin>,
    pub view: View,
    pub board: Board,
    pub entrance: Entrance,
    pub tilt: [f64; 2],
    pointer: Pointer,
    piles_top: Vec<i16>,
    piles_floor: Vec<i16>,
    born: f64,
    still: bool,
    scratch: Vec<usize>,
}

impl Pins {
    /// A new board (`mountPinBoard`), born at `now`; `still` is Reduce Motion: it shows at rest.
    pub fn new(entrance: Entrance, now: f64, still: bool) -> Pins {
        let pins = GRID.iter().enumerate().flat_map(|(r, row)| row.bytes().enumerate().filter(|(_, b)| *b != b'.').map(move |(c, b)| Pin::new(c as u8, r as u8, b - b'0'))).collect();
        Pins {
            pins,
            view: View { pitch: 7.0, depth: 12.0, size: 6.0, ..View::default() },
            board: Board::new(),
            entrance,
            tilt: REST_TILT,
            pointer: Pointer::default(),
            piles_top: Vec::new(),
            piles_floor: Vec::new(),
            born: now - if still { REVEAL * 2.0 } else { 0.0 },
            still,
            scratch: Vec::with_capacity(64),
        }
    }

    /// `layout`: the panel's size, the wordmark box's rect and the composer's left, right and top,
    /// all in panel points. True when something moved.
    pub fn layout(&mut self, w: f64, h: f64, logo: [f64; 4], composer: Option<[f64; 3]>) -> bool {
        let pitch = (logo[2] / COLS).round().max(3.0);
        let x0 = (logo[0] + (logo[2] - pitch * COLS) / 2.0).round();
        let y0 = (logo[1] + (logo[3] - pitch * ROWS) / 2.0).round();
        let v = &mut self.view;
        if v.w == w && v.h == h && v.pitch == pitch && v.x0 == x0 && v.y0 == y0 {
            return false;
        }
        v.w = w;
        v.h = h;
        v.pitch = pitch;
        v.depth = pitch * 1.7;
        v.size = pitch * 0.8;
        v.composer = composer;
        v.x0 = x0;
        v.y0 = y0;
        machine::relayout(&mut self.pins, x0, y0, pitch);
        let buckets = (w / pitch).ceil() as usize + 1;
        self.piles_top = vec![0; buckets];
        self.piles_floor = vec![0; buckets];
        let full = machine::home_count(&self.pins) < self.pins.len();
        self.set_region(full);
        true
    }

    fn set_region(&mut self, full: bool) {
        let v = &mut self.view;
        let margin = (v.pitch * 3.0 + v.depth * 1.5).ceil();
        v.region = if full {
            Region { x: 0.0, y: 0.0, w: v.w.ceil(), h: v.h.ceil(), full }
        } else {
            Region { x: v.x0 - margin, y: v.y0 - margin, w: (v.pitch * COLS + margin * 2.0).ceil(), h: (v.pitch * ROWS + margin * 2.0).ceil(), full }
        };
    }

    pub fn pop_of(p: &Pin, now: f64) -> f64 {
        if p.pop == 0.0 {
            return 0.0;
        }
        let t = now - p.pop_at;
        p.pop * (1.0 - (-t * 22.0).exp()) * (-t * 2.6).exp()
    }

    #[allow(clippy::too_many_arguments)]
    fn knock(&mut self, x: f64, y: f64, radius: f64, power: f64, carry: [f64; 2], now: f64, rng: &mut Rng, cues: &mut Vec<super::sound::Sound>) {
        let mut hits = std::mem::take(&mut self.scratch);
        hits.clear();
        hits.extend(self.pins.iter().enumerate().filter(|(_, p)| (p.x - x).hypot(p.y - y) <= radius).map(|(i, _)| i));
        for &i in &hits {
            if self.pins[i].phase == PinPhase::Rest {
                self.unstack(i, rng);
            }
        }
        for &i in &hits {
            let p = &mut self.pins[i];
            let (dx, dy) = (p.x - x, p.y - y);
            let dist = match dx.hypot(dy) { 0.0 => 1.0, d => d };
            let f = 1.0 - dist / radius;
            let home = p.phase == PinPhase::Home;
            p.phase = PinPhase::Air;
            p.e = 1.0;
            p.vx = (if home { 0.0 } else { p.vx * 0.3 }) + (dx / dist) * power * f + carry[0] + (rng.next() - 0.5) * 60.0;
            p.vy = (if home { 0.0 } else { p.vy.min(0.0) * 0.3 }) + (dy / dist) * power * f + carry[1] - (if home { 220.0 } else { 420.0 }) - 380.0 * f;
            p.spin += (rng.next() - 0.5) * 18.0;
            p.pop = 0.35 + 0.65 * f;
            p.pop_at = now;
        }
        let any = !hits.is_empty();
        self.scratch = hits;
        if !any {
            return;
        }
        self.board = machine::step(self.board, Event::Knocked { now });
        self.set_region(true);
        cues.extend(super::sound::ask(Cue::Knock { x, width: self.view.w }, now));
    }

    /// Hitting a piled pin topples everything stacked above it.
    fn unstack(&mut self, i: usize, rng: &mut Rng) {
        let (top, bucket, level) = (self.pins[i].on_composer, self.pins[i].bucket, self.pins[i].level);
        for o in &mut self.pins {
            if o.phase != PinPhase::Rest || o.on_composer != top || o.bucket != bucket || o.level < level {
                continue;
            }
            o.phase = PinPhase::Air;
            o.vx = (rng.next() - 0.5) * 120.0;
            o.vy = -80.0 - rng.next() * 120.0;
            o.spin += (rng.next() - 0.5) * 6.0;
        }
        let heights = if top { &mut self.piles_top } else { &mut self.piles_floor };
        heights[bucket] = heights[bucket].min(level);
    }

    /// Piled pins sit above the composer's edge by their extrusion depth, so their sides don't
    /// tuck behind it.
    fn composer_surface(&self) -> f64 {
        self.view.composer.map_or(f64::INFINITY, |c| c[2] - self.view.depth * REST_TILT[1])
    }

    fn on_composer(&self, bucket: usize) -> bool {
        let x = (bucket as f64 + 0.5) * self.view.pitch;
        self.view.composer.is_some_and(|c| x > c[0] + 2.0 && x < c[1] - 2.0)
    }

    fn settle(&mut self, i: usize, start: usize, top: bool, rng: &mut Rng) {
        let len = if top { self.piles_top.len() } else { self.piles_floor.len() };
        let height = |s: &Pins, n: usize| if top { s.piles_top[n] } else { s.piles_floor[n] };
        let mut b = start;
        for _ in 0..14 {
            let mut options = [0usize; 2];
            let mut n = 0;
            for o in [b.wrapping_sub(1), b + 1] {
                if o < len && (if top && !self.on_composer(o) { height(self, b) > 0 } else { height(self, o) < height(self, b) }) {
                    options[n] = o;
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            b = options[(rng.next() * n as f64).floor() as usize];
        }
        let pitch = self.view.pitch;
        if top && !self.on_composer(b) {
            let p = &mut self.pins[i];
            p.x = (b as f64 + 0.5) * pitch;
            p.vx = if b < start { -1.0 } else { 1.0 } * 90.0;
            p.vy = 0.0;
            return;
        }
        let surface = if top { self.composer_surface() } else { self.view.h };
        let heights = if top { &mut self.piles_top } else { &mut self.piles_floor };
        let p = &mut self.pins[i];
        p.phase = PinPhase::Rest;
        p.bucket = b;
        p.on_composer = top;
        p.level = heights[b];
        p.x = (b as f64 + 0.5) * pitch;
        p.y = surface - f64::from(heights[b]) * pitch - pitch / 2.0;
        heights[b] += 1;
    }

    fn reassemble(&mut self, now: f64, rng: &mut Rng) {
        for p in &mut self.pins {
            if p.phase != PinPhase::Air && p.phase != PinPhase::Rest {
                continue;
            }
            p.phase = PinPhase::Return;
            p.x0 = p.x;
            p.y0 = p.y;
            p.rot0 = p.rot;
            p.t0 = now + (f64::from(p.c) / COLS) * 0.6 + rng.next() * 0.1;
            p.duration = 0.5 + rng.next() * 0.25;
            p.lift = 30.0 + rng.next() * 70.0;
        }
        self.piles_top.fill(0);
        self.piles_floor.fill(0);
        self.board = machine::step(self.board, Event::Knocked { now });
    }

    /// One frame of motion (`step`), when not still.
    pub fn step(&mut self, now: f64, dt: f64, rng: &mut Rng, cues: &mut Vec<super::sound::Sound>) {
        if self.still {
            return;
        }
        let active = self.pointer.step(PointerEvent::Step { dt });
        let v = self.view;
        let cx = v.x0 + (COLS * v.pitch) / 2.0;
        let cy = v.y0 + (ROWS * v.pitch) / 2.0;
        // In a game the board leans only slightly after the ball, so the bricks stay readable.
        let game = machine::accepts_game_input(self.board);
        let lean = if game { 0.12 } else { 0.6 };
        let (tx, ty) = match active {
            Some((px, py, _, _)) => {
                let aim_x = clamp1(((cx - px) / (v.w * 0.5)) * 1.4);
                let aim_y = clamp1(((cy - py) / (v.h * 0.5)) * 1.4);
                (lerp(REST_TILT[0], aim_x, lean), lerp(REST_TILT[1], aim_y, lean))
            }
            None => (REST_TILT[0], REST_TILT[1]),
        };
        let ease = 1.0 - (-dt * 4.0).exp();
        self.tilt[0] += (tx - self.tilt[0]) * ease;
        self.tilt[1] += (ty - self.tilt[1]) * ease;
        // A held button keeps knocking along the drag path, spaced so it reads as a trail.
        if let Some((px, py, pvx, pvy)) = active
            && machine::drag_knock_due(self.board, px, py, v.pitch)
        {
            self.board = machine::step(self.board, Event::PointerDragged { x: px, y: py });
            self.knock(px, py, 16.0, 260.0, [pvx * 0.25, pvy * 0.25], now, rng, cues);
        }
        let cell = v.pitch;
        let (mut air, mut loose) = (0usize, 0usize);
        let sink = if game { 36.0 } else { 55.0 };
        let (pvx, pvy) = active.map_or((0.0, 0.0), |(_, _, vx, vy)| (vx, vy));
        let mut activity = (tx - self.tilt[0]).abs() + (ty - self.tilt[1]).abs() + (pvx.abs() + pvy.abs()) / 1000.0;
        let closing = matches!(self.board, Board::Closing { .. });
        for i in 0..self.pins.len() {
            let p = &mut self.pins[i];
            if p.phase == PinPhase::Out {
                continue;
            }
            p.e *= (-dt * 1.8).exp();
            if p.pop != 0.0 && now - p.pop_at > 1.5 {
                p.pop = 0.0;
            }
            match p.phase {
                PinPhase::Home => {
                    let dist = active.map_or(f64::INFINITY, |(px, py, _, _)| (p.hx - px).hypot(p.hy - py));
                    let f = if dist < sink { 1.0 - dist / sink } else { 0.0 };
                    let target = f * f * 0.9;
                    p.zv += ((target - p.z) * 260.0 - p.zv * 14.0) * dt;
                    p.z = (p.z + p.zv * dt).clamp(-0.8, 1.0);
                    activity = activity.max((p.zv.abs() * 0.1 + (target - p.z).abs()).max(p.e * 0.05));
                    continue;
                }
                PinPhase::Rest => {
                    loose += 1;
                    p.rot = wrap_quarter(p.rot) * (-dt * 14.0).exp();
                    continue;
                }
                PinPhase::Return => {
                    let k = (now - p.t0) / p.duration;
                    if k < 0.0 {
                        continue;
                    }
                    if k >= 1.0 {
                        // Pins packing into the closing lodger land softer than ones reassembling the wordmark.
                        p.phase = PinPhase::Home;
                        p.x = p.hx;
                        p.y = p.hy;
                        p.vx = 0.0;
                        p.vy = 0.0;
                        p.rot = 0.0;
                        p.spin = 0.0;
                        p.pop = 0.0;
                        p.e = if closing { 0.5 } else { 0.9 };
                        p.z = if closing { -0.2 } else { -0.8 };
                        p.zv = 0.0;
                        cues.extend(super::sound::ask(Cue::Slot { column: f64::from(p.c), columns: COLS, x: p.hx, width: v.w }, now));
                        continue;
                    }
                    let e = 1.0 - (1.0 - k).powi(3);
                    p.x = lerp(p.x0, p.hx, e);
                    p.y = lerp(p.y0, p.hy, e) - (k * std::f64::consts::PI).sin() * p.lift;
                    p.rot = lerp(wrap_quarter(p.rot0), 0.0, e);
                    continue;
                }
                PinPhase::Air | PinPhase::Out => {}
            }
            air += 1;
            loose += 1;
            p.vy += 2400.0 * dt;
            p.x += p.vx * dt;
            p.y += p.vy * dt;
            p.rot += p.spin * dt;
            if p.x < cell {
                p.x = cell;
                p.vx = p.vx.abs() * 0.5;
            }
            if p.x > v.w - cell {
                p.x = v.w - cell;
                p.vx = -p.vx.abs() * 0.5;
            }
            if game {
                if p.y > v.h + cell * 4.0 {
                    p.phase = PinPhase::Out;
                }
                continue;
            }
            if p.vy <= 0.0 || Self::pop_of(p, now) >= 0.1 {
                continue;
            }
            let (x, y, vy) = (p.x, p.y, p.vy);
            let bucket = ((x / cell).floor().max(0.0) as usize).min(self.piles_top.len().saturating_sub(1));
            let top_surface = self.composer_surface() - f64::from(self.piles_top[bucket]) * cell;
            let top = self.on_composer(bucket) && y - vy * dt + cell / 2.0 <= top_surface + 1.0;
            let surface = if top { top_surface } else { v.h - f64::from(self.piles_floor[bucket]) * cell };
            if y + cell / 2.0 < surface {
                continue;
            }
            if vy <= 260.0 {
                self.settle(i, bucket, top, rng);
                continue;
            }
            let p = &mut self.pins[i];
            p.y = surface - cell / 2.0;
            cues.extend(super::sound::ask(Cue::Bounce { x: p.x, width: v.w, speed: p.vy }, now));
            p.vy = -p.vy * 0.34;
            p.vx *= 0.55;
            p.spin = -p.spin * 0.5 + (rng.next() - 0.5) * 4.0;
        }
        self.view.activity = activity;
        if machine::should_reassemble(self.board, air, loose, now) {
            self.reassemble(now, rng);
        }
        if self.view.region.full && machine::home_count(&self.pins) == self.pins.len() {
            self.set_region(false);
        }
    }

    fn reveal_span(&self) -> f64 {
        if self.entrance == Entrance::Sweep { REVEAL } else { 1.3 }
    }

    pub fn resting(p: &Pin) -> bool {
        matches!(p.phase, PinPhase::Home | PinPhase::Out)
    }

    /// Nothing moves: the frame loop can stop (`settled`). Always true when still.
    pub fn settled(&self, now: f64) -> bool {
        if self.still {
            return true;
        }
        if matches!(self.board, Board::Closing { .. }) {
            return machine::close_age(self.board, now) >= 1.2 && machine::home_count(&self.pins) == self.pins.len();
        }
        if now - self.born < self.reveal_span() || machine::dragging(self.board) || self.view.activity > 1e-3 {
            return false;
        }
        self.pins.iter().all(Self::resting)
    }

    pub fn age(&self, now: f64) -> f64 {
        now - self.born
    }

    // Pointer input, in panel points. The board still leans toward the pointer while it packs
    // up, but only the interactive board can be knocked; in a game the ball steers it.

    /// The pointer came in (`onEnter`): its first point counts as where it was, so entering adds
    /// no speed.
    pub fn pointer_entered(&mut self) {
        if machine::accepts_game_input(self.board) || self.still {
            return;
        }
        self.pointer.step(PointerEvent::Enter);
    }

    pub fn pointer_move(&mut self, x: f64, y: f64) {
        if machine::accepts_game_input(self.board) || self.still {
            return;
        }
        self.pointer.step(PointerEvent::Move { x, y });
    }

    pub fn pointer_leave(&mut self) {
        if machine::dragging(self.board) || machine::accepts_game_input(self.board) || self.still {
            return;
        }
        self.pointer.step(PointerEvent::Leave);
    }

    /// A press on the panel, off its controls: knocks pins out. False when the board takes none.
    pub fn pointer_down(&mut self, x: f64, y: f64, now: f64, rng: &mut Rng, cues: &mut Vec<super::sound::Sound>) -> bool {
        if !machine::accepts_pointer(self.board) || self.still {
            return false;
        }
        self.pointer.step(PointerEvent::Down { x, y });
        self.board = machine::step(self.board, Event::PointerDown { x, y });
        self.knock(x, y, 24.0, 560.0, [0.0, 0.0], now, rng, cues);
        true
    }

    pub fn pointer_up(&mut self) {
        self.board = machine::step(self.board, Event::PointerUp);
    }

    pub fn dragging(&self) -> bool {
        machine::dragging(self.board)
    }

    /// The sweep sound the board starts with (`PinSound.sweep` at mount).
    pub fn mount_cue(&self) -> Option<Cue> {
        (!self.still).then(|| Cue::Sweep { duration: if self.entrance == Entrance::Sweep { REVEAL } else { 0.9 }, reverse: false })
    }

    // The Brick Breaker handle (`PinBoardHandle`).

    /// When the ball touches a pin, knocks out every pin within `blast` of the ball's edge, and
    /// returns the average offset from the touched pins to the ball and how many went.
    #[allow(clippy::too_many_arguments)]
    pub fn collide(&mut self, x: f64, y: f64, radius: f64, blast: f64, vx: f64, vy: f64, now: f64, rng: &mut Rng) -> Option<Hit> {
        if !machine::accepts_game_input(self.board) {
            return None;
        }
        let half = self.view.size / 2.0;
        let reach = radius + blast + half;
        let mut hit = Hit { nx: 0.0, ny: 0.0, count: 0 };
        let mut struck = std::mem::take(&mut self.scratch);
        struck.clear();
        for (i, p) in self.pins.iter().enumerate() {
            if p.phase != PinPhase::Home {
                continue;
            }
            let (dx, dy) = (x - p.hx, y - p.hy);
            if dx.abs() > reach || dy.abs() > reach {
                continue;
            }
            let gap = (dx - dx.clamp(-half, half)).hypot(dy - dy.clamp(-half, half));
            if gap > radius + blast {
                continue;
            }
            struck.push(i);
            if gap > radius {
                continue;
            }
            hit.nx += dx;
            hit.ny += dy;
            hit.count += 1;
        }
        if hit.count == 0 {
            self.scratch = struck;
            return None;
        }
        hit.nx /= hit.count as f64;
        hit.ny /= hit.count as f64;
        hit.count = struck.len();
        for &i in &struck {
            let p = &mut self.pins[i];
            let (dx, dy) = (x - p.hx, y - p.hy);
            let dist = match dx.hypot(dy) { 0.0 => 1.0, d => d };
            // Bricks burst away from the ball toward the viewer, then fall off the panel.
            p.phase = PinPhase::Air;
            p.e = 1.0;
            p.vx = vx * 0.35 - (dx / dist) * 140.0 + (rng.next() - 0.5) * 160.0;
            p.vy = vy * 0.35 - (dy / dist) * 140.0 - 180.0 - rng.next() * 180.0;
            p.spin += (rng.next() - 0.5) * 22.0;
            p.pop = 0.45 + rng.next() * 0.4;
            p.pop_at = now;
        }
        self.scratch = struck;
        self.set_region(true);
        Some(hit)
    }

    /// Pins still in their sockets.
    pub fn left(&self) -> usize {
        machine::home_count(&self.pins)
    }

    /// Starts game mode: pins ignore the pointer, follow the ball, and knocked pins fall off the panel.
    pub fn play(&mut self, now: f64, rng: &mut Rng) {
        let next = machine::step(self.board, Event::Play);
        if next == self.board {
            return;
        }
        self.board = next;
        self.pointer.step(PointerEvent::Leave);
        self.piles_top.fill(0);
        self.piles_floor.fill(0);
        // Pins still loose from earlier clicks hurry home so the wall is whole when the ball is served.
        for p in &mut self.pins {
            if p.phase == PinPhase::Home {
                continue;
            }
            p.phase = PinPhase::Return;
            p.x0 = p.x;
            p.y0 = p.y;
            p.rot0 = p.rot;
            p.t0 = now + (f64::from(p.c) / COLS) * 0.2;
            p.duration = 0.35 + rng.next() * 0.1;
            p.lift = 20.0 + rng.next() * 30.0;
        }
    }

    pub fn steer(&mut self, x: f64, y: f64) {
        if !machine::accepts_game_input(self.board) {
            return;
        }
        self.pointer.step(PointerEvent::Steer { x, y });
    }

    /// Ends game mode: lost bricks fly back up from below the panel, sweeping in column by column.
    pub fn restore(&mut self, now: f64, rng: &mut Rng) {
        let next = machine::step(self.board, Event::Restore { now });
        if next == self.board {
            return;
        }
        self.board = next;
        self.pointer.step(PointerEvent::Leave);
        let (pitch, h) = (self.view.pitch, self.view.h);
        for p in &mut self.pins {
            if p.phase == PinPhase::Home {
                continue;
            }
            let out = p.phase == PinPhase::Out;
            p.phase = PinPhase::Return;
            p.x0 = if out { p.hx + (rng.next() - 0.5) * pitch * 6.0 } else { p.x };
            p.y0 = if out { h + pitch * 2.0 } else { p.y };
            p.rot0 = if out { (rng.next() - 0.5) * 6.0 } else { p.rot };
            p.t0 = now + 0.15 + (f64::from(p.c) / COLS) * 0.55 + rng.next() * 0.08;
            p.duration = 0.55 + rng.next() * 0.2;
            p.lift = 20.0 + rng.next() * 50.0;
        }
    }

    /// The lodger packs the board away (`close`): every loose pin flies home as the wave reels in.
    pub fn close(&mut self, now: f64, rng: &mut Rng, cues: &mut Vec<super::sound::Sound>) {
        let next = machine::step(self.board, Event::Close { now });
        if next == self.board {
            return;
        }
        self.board = next;
        self.piles_top.fill(0);
        self.piles_floor.fill(0);
        for p in &mut self.pins {
            if p.phase == PinPhase::Home {
                continue;
            }
            p.phase = PinPhase::Return;
            p.x0 = p.x;
            p.y0 = p.y;
            p.rot0 = p.rot;
            p.t0 = now + (f64::from(p.c) / COLS) * 0.18;
            p.duration = 0.28 + rng.next() * 0.06;
            p.lift = 24.0 + rng.next() * 42.0;
        }
        if !self.still {
            cues.extend(super::sound::ask(Cue::Sweep { duration: 0.95, reverse: true }, now));
        }
    }
}

/// Radial chain reaction from the starter pin in the "e" (column 75, row 3).
pub fn chain_of(p: &Pin, age: f64) -> f64 {
    let dist = (f64::from(p.c) - 75.0).hypot((f64::from(p.r) - 3.0) * 1.4);
    let u = (age - (dist / 76.0) * 0.62) / 0.36;
    if u <= 0.0 {
        return 0.0;
    }
    if u >= 1.0 {
        return 1.0;
    }
    1.0 - (1.0 - u).powi(3)
}

/// The reverse wave reeling pins from the "o" back into the "e" hole as the lodger tucks in.
pub fn close_of(p: &Pin, close_age: f64) -> f64 {
    let dist = (f64::from(p.c) - 75.0).hypot((f64::from(p.r) - 3.0) * 1.4);
    let u = (close_age - (0.32 + ((76.0 - dist) / 76.0) * 0.48)) / 0.32;
    if u <= 0.0 {
        return 1.0;
    }
    if u >= 1.0 {
        return 0.0;
    }
    1.0 - u * u * (3.0 - 2.0 * u)
}

pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

fn clamp1(v: f64) -> f64 {
    v.clamp(-1.0, 1.0)
}

/// JavaScript's `%` keeps the dividend's sign, as Rust's does.
fn wrap_quarter(a: f64) -> f64 {
    let q = std::f64::consts::FRAC_PI_2;
    let m = (((a + q / 2.0) % q) + q) % q;
    m - q / 2.0
}
