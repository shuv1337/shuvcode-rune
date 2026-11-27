//! Settings › About (`settings/about/about.tsx`): the version (pressing it copies it), the
//! licence, the wordmark, the credits with the count of other contributors, where it was first
//! published and illustrated, the site, the trademark, the typefaces, the third-party notices,
//! the tagline, the MIT and globe marks and the copyright over Anomaly's brush mark, centred in
//! a 560 pt column.

pub mod notices;

use std::cell::Cell;

use opencode_i18n::t;
use rune::{Cursor, El, Icon, Link, LinkPaint, TextStyle, Tx, col, el, icon, row, text};
use opencode_ui::focus::FocusExt;
use opencode_ui::palette::Pal;

pub use self::notices::{NoticesState, close as close_notices, dialog as notices_dialog};
use crate::host::SettingsHost;

/// `.settings-about-content`: 13 on 20 at 440, -0.04 tracking.
const BODY: TextStyle = TextStyle::new(13.0, 440.0).lh(20.0).tracking(-0.04);

/// `FALLBACK_OTHER_CONTRIBUTORS`, until GitHub answers.
pub const FALLBACK_OTHERS: u32 = 935;

/// The 16 credited writers on Settings › About (hosts subtract `WRITERS_COUNT` from total contributors).
pub const WRITERS_COUNT: u32 = WRITERS.len() as u32;

const WRITERS: [&str; 16] = [
    "thdxr", "adamdotdev", "rekram1-node", "kitlangton", "iamdavidhill", "jayair", "fwang", "brendonovich", "nexxeln", "hona", "kommander", "jlongster", "vimtor", "r44vcorp", "simonklee", "arvsrn",
];
const ILLUSTRATORS: [&str; 4] = ["usrnk1", "ludvigrask_", "arvsrn", "iamdavidhill"];

static LEGAL: Icon = Icon { name: "about-legal", view: 91.0, body: include_str!("../../assets/legal.svg.body") };
static BRUSH: Icon = Icon { name: "about-anomaly-brush", view: 110.0, body: include_str!("../../assets/anomaly-brush.svg.body") };

fn copy_version(host: &mut dyn SettingsHost, value: &str, tx: &mut Tx) {
    tx.copy(value.to_owned());
    host.mark_copied("settings-about-version", tx);
}

fn profile(name: &str) -> String {
    match name {
        "r44vcorp" => "https://github.com/R44VC0RP".to_owned(),
        "ludvigrask_" => "https://x.com/ludvigrask_".to_owned(),
        _ => format!("https://github.com/{name}"),
    }
}

fn open_notices(host: &mut dyn SettingsHost, tx: &mut Tx) {
    notices::open(host, tx);
}

fn credit_line<H: SettingsHost + 'static>(p: &Pal, key: &str, words: String, entries: &[(String, String)]) -> El {
    let hover = LinkPaint::new().color(p.text).underline(p.text);
    let mut el = text(words.clone()).key(key.to_owned()).font(BODY).color(p.faint).wrap().align(rune::Align::Center);
    let mut cursor = 0usize;
    for (label, url) in entries {
        if let Some(rel) = words[cursor..].find(label.as_str()) {
            let start = cursor + rel;
            let end = start + label.len();
            el = el.link(Link::new(start..end, url.clone()).role("link").label(label.clone()).hover(hover));
            cursor = end;
        }
    }
    el.on_link(|s: &mut H, url: &str, tx: &mut Tx| crate::open_external(s, url, tx))
}

pub fn view<H: SettingsHost + 'static>(s: &H, p: &Pal) -> El {
    let info = s.about_info();
    let version = if info.version.is_empty() { crate::build_version().to_owned() } else { info.version };
    let label = t!("settings.about.version", version = version);
    let copied = s.is_copied("settings-about-version");
    let copy = label.clone();
    let version_button = row()
        .key("settings-about-version")
        .role("button")
        .label(if copied { t!("common.copied") } else { t!("ui.message.copy") })
        .help(if copied { t!("common.copied") } else { t!("ui.message.copy") })
        .help_above()
        .help_open(copied)
        .h(24.0)
        .my(-2.0)
        .px(6.0)
        .mx(-6.0)
        .radius(4.0)
        .center_y()
        .cursor(Cursor::Pointer)
        .hover_group_focus()
        .hover(p.layer2)
        // The ring follows the hover fill, which reaches past the label (`settings.css`, opencode#65215).
        .focus_default(p)
        .on_press_tx(move |s: &mut H, tx: &mut Tx| copy_version(s, &copy, tx))
        .child(text(label).font(BODY).color(p.muted).group_color(p.text));
    let link_hover = LinkPaint::new().color(p.text).underline(p.text);
    let site_words = t!("settings.about.website");
    let site = text(site_words)
        .key("settings-about-site")
        .dir(rune::Direction::Ltr)
        .font(BODY)
        .color(p.faint)
        .link(Link::new(0..site_words.len(), "https://opencode.ai").role("link").label(site_words).hover(link_hover))
        .on_link(|s: &mut H, url: &str, tx: &mut Tx| crate::open_external(s, url, tx));
    let notices_words = t!("settings.about.notices.title");
    let notices = text(notices_words)
        .key("settings-about-notices")
        .font(BODY)
        .color(p.muted)
        .link(Link::new(0..notices_words.len(), "notices").role("button").label(notices_words).hover(link_hover))
        .on_link(|s: &mut H, _: &str, tx: &mut Tx| open_notices(s, tx));
    build_about_page::<H>(s, p, info.other_contributors, version_button, site, notices)
}

fn build_about_page<H: SettingsHost + 'static>(s: &H, p: &Pal, others: u32, version_button: El, site: El, notices: El) -> El {
    let line = |key: &str, words: &str, faint: bool| {
        text(words.to_owned())
            .key(key.to_owned())
            .font(BODY)
            .color(if faint { p.faint } else { p.muted })
            .wrap()
            .align(rune::Align::Center)
    };
    let others_label = opencode_i18n::plural!("settings.about.otherContributor", others);
    let mut writers: Vec<String> = WRITERS.iter().map(|w| (*w).to_owned()).collect();
    writers.push(others_label.clone());
    let mut writer_links: Vec<(String, String)> = WRITERS.iter().map(|w| ((*w).to_owned(), profile(w))).collect();
    writer_links.push((others_label, "https://github.com/anomalyco/opencode/graphs/contributors".to_owned()));
    let illustrator_links: Vec<(String, String)> = ILLUSTRATORS.iter().map(|w| ((*w).to_owned(), profile(w))).collect();
    let ink = p.muted;
    let (stem, weak) = (ink.alpha(ink.a * 0.2), ink.alpha(ink.a * 0.2 * 0.3));
    let wordmark = wordmark(&s.settings_pages().wordmark.letters, 178.0 / 234.0, stem, weak).key("settings-about-wordmark");
    let clipped = |key: &str, glyph: &'static Icon, w: f32, h: f32| el().key(key.to_owned()).size(w, h).clip_soft(0.0).child(icon(glyph, w));
    let content = col()
        .key("settings-about-content")
        .w_full()
        .max_w(560.0)
        .min_h(462.0)
        .pt(80.0)
        .pb(80.0)
        .gap(24.0)
        .items_center()
        .child(col().key("settings-about-intro").items_center().gap(14.0).child(version_button).child(line("license", t!("settings.about.license"), false)))
        .child(wordmark)
        .child(
            col()
                .key("settings-about-credits")
                .w_full()
                .items_center()
                .gap(16.0)
                .child(credit_line::<H>(p, "writers", t!("settings.about.writtenByNames", names = opencode_i18n::list(&writers)), &writer_links))
                .child(credit_line::<H>(p, "illustrators", t!("settings.about.illustratedByNames", names = opencode_i18n::list(&ILLUSTRATORS)), &illustrator_links)),
        )
        .child(
            col()
                .w_full()
                .items_center()
                .gap(16.0)
                .child(line("published", t!("settings.about.firstPublished"), false))
                .child(line("illustrated", t!("settings.about.firstIllustrated"), false)),
        )
        .child(site)
        .child(
            col()
                .key("settings-about-details")
                .w_full()
                .items_center()
                .gap(16.0)
                .child(line("description", t!("settings.about.description"), false))
                .child(line("trademark", t!("settings.about.trademark"), false))
                .child(line("typeset", t!("settings.about.typeset"), false))
                .child(notices),
        )
        .child(line("tagline", t!("settings.about.tagline"), false))
        .child(clipped("settings-about-legal", &LEGAL, 109.2, 19.2))
        .child(
            col()
                .key("settings-about-copyright")
                .w_full()
                .items_center()
                .gap(12.0)
                .child(line("copyright", t!("settings.about.copyright"), false))
                .child(clipped("settings-about-anomaly-brush", &BRUSH, 126.72, 48.384)),
        );
    col().key("settings-about").w_full().px(24.0).items_center().child(content)
}

/// shuv1337 fork: the wordmark's letters, as `AnimatedWordmark` spells it: s h u v c o d e.
const TARGET: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
/// The letters a scramble draws from: s, h, u, v, c, o, d, e.
const CHOICES: u8 = 8;
/// Each letter's rects in its 24 by 42 cell (`settings-about-letter-*`), `true` for its shadow.
type LetterRect = (f32, f32, f32, f32, bool);

const LETTERS: [&[LetterRect]; 8] = [
    &[(0., 6., 24., 6., false), (0., 12., 6., 12., false), (6., 18., 18., 6., false), (18., 24., 6., 12., false), (0., 30., 18., 6., false), (0., 24., 18., 6., true)],
    &[(0., 0., 6., 36., false), (6., 6., 18., 6., false), (18., 12., 6., 24., false), (6., 18., 12., 18., true)],
    &[(0., 6., 6., 30., false), (18., 6., 6., 30., false), (6., 30., 12., 6., false), (6., 18., 12., 12., true)],
    &[(0., 6., 6., 24., false), (18., 6., 6., 24., false), (6., 30., 12., 6., false), (6., 18., 12., 12., true)],
    &[(0., 6., 24., 6., false), (0., 12., 6., 18., false), (0., 30., 24., 6., false), (6., 18., 18., 12., true)],
    &[(0., 6., 24., 6., false), (0., 12., 6., 24., false), (18., 12., 6., 24., false), (6., 30., 12., 6., false), (6., 18., 12., 12., true)],
    &[(0., 6., 18., 6., false), (18., 0., 6., 36., false), (0., 12., 6., 24., false), (6., 30., 12., 6., false), (6., 18., 12., 12., true)],
    &[(0., 6., 24., 6., false), (0., 12., 6., 24., false), (18., 12., 6., 12., false), (6., 18., 12., 6., false), (6., 30., 18., 6., false), (6., 24., 18., 6., true)],
];
/// A scramble's step (`tick * 75` ms).
const TICK: f64 = 0.075;

/// The wordmark's letters while the page shows (`AnimatedWordmark`'s `active`): each opening
/// starts every letter on a random one, which changes each 75 ms tick until its own settle tick,
/// 6 to 13, then stays right. Under Reduce Motion it is spelled at once.
#[derive(Clone, Copy)]
pub struct Wordmark {
    pub letters: [u8; 8],
    scramble: Scramble,
    clock: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Scramble {
    Still,
    Pending { since: f64 },
    Running { start: f64, settles: [u8; 8], drawn: u8 },
}

enum Event {
    Active(bool),
    Poll { now: f64, reduced: bool },
}

#[derive(Default)]
struct Effects {
    drew: bool,
    wake: Option<f64>,
}

impl Default for Wordmark {
    fn default() -> Self {
        Wordmark { letters: TARGET, scramble: Scramble::Still, clock: 0.0 }
    }
}

impl Wordmark {
    pub fn activate(&mut self, active: bool) {
        self.step(Event::Active(active), |_| 0.0);
    }

    /// Formats the current 8 letters as a string (for diagnostic traces).
    pub fn letters_string(&self) -> String {
        const NAMES: [char; 8] = ['s', 'h', 'u', 'v', 'c', 'o', 'd', 'e'];
        self.letters.iter().map(|&l| NAMES[usize::from(l) % NAMES.len()]).collect()
    }

    fn step(&mut self, event: Event, mut next_unit: impl FnMut(u64) -> f64) -> Effects {
        match (self.scramble, event) {
            (_, Event::Active(active)) => {
                self.scramble = if active { Scramble::Pending { since: self.clock } } else { Scramble::Still };
                self.letters = TARGET;
                Effects::default()
            }
            (Scramble::Still, Event::Poll { .. }) => Effects::default(),
            (_, Event::Poll { reduced: true, .. }) => {
                self.scramble = Scramble::Still;
                let drew = self.letters != TARGET;
                self.letters = TARGET;
                Effects { drew, wake: None }
            }
            (Scramble::Pending { since }, Event::Poll { now, .. }) => {
                let seed = now.to_bits();
                let starts: [u8; 8] = std::array::from_fn(|_| pick(&mut next_unit, seed, CHOICES));
                let settles: [u8; 8] = std::array::from_fn(|_| 6 + pick(&mut next_unit, seed, 8));
                self.letters = starts;
                self.scramble = Scramble::Running { start: since, settles, drawn: 0 };
                let fx = self.step(Event::Poll { now, reduced: false }, next_unit);
                Effects { drew: true, wake: fx.wake }
            }
            (Scramble::Running { start, settles, drawn }, Event::Poll { now, .. }) => {
                let tick = ((now - start + 1e-9) / TICK).floor().clamp(0.0, 255.0) as u8;
                let last = settles.iter().copied().max().unwrap_or(0);
                let tick = tick.min(last);
                let seed = now.to_bits();
                for at in drawn + 1..=tick {
                    for i in 0..8 {
                        self.letters[i] = if at >= settles[i] { TARGET[i] } else { pick(&mut next_unit, seed, CHOICES) };
                    }
                }
                let drew = tick > drawn;
                if tick >= last {
                    self.scramble = Scramble::Still;
                    return Effects { drew, wake: None };
                }
                self.scramble = Scramble::Running { start, settles, drawn: tick.max(drawn) };
                Effects { drew, wake: Some(start + f64::from(tick + 1) * TICK) }
            }
        }
    }
}

thread_local! {
    static LOCAL_RNG: Cell<u64> = const { Cell::new(0) };
}

fn local_unit(seed: u64) -> f64 {
    LOCAL_RNG.with(|cell| {
        let mut s = cell.get();
        if s == 0 {
            s = seed | 1;
        }
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        cell.set(s);
        (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64
    })
}

fn pick(next_unit: &mut impl FnMut(u64) -> f64, seed: u64, below: u8) -> u8 {
    (next_unit(seed).clamp(0.0, 0.999_999_999_999) * f64::from(below)).floor() as u8
}

/// Runs the scramble on the app's clock: an opening's first letters, then each tick's.
pub fn poll(s: &mut dyn SettingsHost, now: f64, tx: &mut Tx) -> bool {
    let reduced = s.reduced_motion();
    let mut w = s.settings_pages().wordmark;
    if w.scramble == Scramble::Still {
        s.settings_pages_mut().wordmark.clock = now;
        return false;
    }
    let fx = w.step(Event::Poll { now, reduced }, |seed| s.about_random_unit(seed).unwrap_or_else(|| local_unit(seed)));
    w.clock = now;
    let letters = w.letters_string();
    s.settings_pages_mut().wordmark = w;
    s.on_wordmark_tick(now, &letters);
    if fx.drew {
        tx.instant();
    }
    if let Some(due) = fx.wake {
        tx.wake_after(std::time::Duration::from_secs_f64((due - now).max(0.001)));
    }
    fx.drew
}

/// The wordmark spelling `letters`, `k` points a unit of its 234 by 42 grid, each letter 30 on.
/// Matches `.settings-about-wordmark { width: 178px; height: 32px; }` (avoiding `42 * 178/234 = 31.95`
/// flooring by 1 px at 1.5x scale).
fn wordmark(letters: &[u8; 8], k: f32, stem: rune::Color, weak: rune::Color) -> El {
    let mut logo = el().size((234.0 * k).round(), (42.0 * k).round()).dir(rune::Direction::Ltr).pixel_snap(false);
    for (i, &letter) in letters.iter().enumerate() {
        for &(x, y, w, h, shadow) in LETTERS[usize::from(letter) % LETTERS.len()] {
            logo = logo.child(
                el().key(format!("l{i}-{x}-{y}-{w}-{h}"))
                    .absolute((x + i as f32 * 30.0) * k, y * k)
                    .size(w * k, h * k)
                    .fill(if shadow { weak } else { stem }),
            );
        }
    }
    logo
}
