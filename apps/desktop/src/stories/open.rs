//! The actual session page and opening path, with deterministic page and image delivery.
use std::sync::{Arc, mpsc::Sender};
use opencode_kit::model::{self, Message, Transcript};
use rune::{El, Play, col, text};
use serde_json::json;
use crate::{App, Route, palette::Pal, session_pages::{Key, Page, PageResult, Pages}};
use crate::stories::{Trigger, replay};

const TARGET: &str = "ses_open_story";
const MEDIA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

pub struct Demo {
    page: Arc<Page>,
    send: Sender<PageResult>,
    png: Vec<u8>,
    at: usize,
    // The "Switch at once" variant: a faithful replay of the old route switch.
    before: bool,
}

/// The storybook's Session opening stories, in order.
pub const PATHS: [&str; 4] = ["Session opening/Cold · ready at 100 ms", "Session opening/Prefetched · ready now", "Session opening/Slow · 150 ms fallback", "Session opening/Subagent · ready now"];
/// Each story's triggers: R sets it up and clicks 0.75 s in, O clicks now.
pub const KEYS: [&[Trigger]; 4] = [&[replay(open_at::<0>), CLICK], &[replay(open_at::<1>), CLICK], &[replay(open_at::<2>), CLICK], &[replay(open_at::<3>), CLICK]];
/// How the click opens the session: waiting for its content, or the old route switch at once.
pub const VARIANTS: &[(&str, usize)] = &[("Wait for the content", 0), ("Switch at once", 1)];
const CLICK: Trigger = Trigger { key: 'o', label: "Open now", run: click };

pub fn setup(s: &mut App, at: usize) {
    let prefetched = matches!(at, 1 | 3);
    let png = picture();
    let mut messages: Vec<_> = (0..24).map(|i| Message::User { id:format!("story_note_{i:02}"), text:format!("Layout review {i}: keep the chart aligned with the session header.") }).collect();
    messages.extend(model::parse_messages(&json!({"data":[
        {"id":"story_image", "type":"user", "text":"Here's the revised dashboard. Does the chart have enough room?", "files":[{"mime":"image/png", "name":"dashboard.png", "uri":format!("media:{MEDIA}")}]},
        {"id":"story_answer", "type":"assistant", "time":{"created":100,"completed":101}, "tokens":{"input":42000}, "content":[{"type":"text","text":"Yes. The chart now fills the available width, and the controls stay aligned with the header.\n\nThe final image size and scroll position should be ready on the first frame."}]}
    ]})));
    let page = Arc::new(Page { key:Key { id:TARGET.into(), updated:0 }, messages, cursor:None, stubs:vec![], asked:0, history:false, read_of: Default::default(), images:vec![], bytes:0, read:Default::default() });
    let (pages, send) = Pages::fixture(prefetched.then(|| page.clone()));
    s.pages.set(pages);
    s.transcripts.remove(TARGET);
    *s.opening = None;
    s.composer_ext.attach = Default::default();
    s.composer_ext.attach.fixture_media(MEDIA, prefetched.then(|| png.clone()));
    *s.route = Route::here("ses_open_previous".into());
    s.transcripts.insert("ses_open_previous".into(), Transcript { session:"ses_open_previous".into(), messages:vec![Message::User { id:"story_previous".into(), text:"Keep this session visible while the dashboard review gets ready.".into() }] });
    if !s.sessions.iter().any(|i| i.id == TARGET) {
        let mut info = s.sessions.first().cloned().unwrap_or_default();
        info.id = TARGET.into(); info.title = "Dashboard review".into(); info.parent = None;
        s.sessions.push(info.clone());
        info.id = "ses_open_previous".into(); info.title = "Current session".into();
        s.sessions.push(info);
    }
    if let Some(info) = s.sessions.iter_mut().find(|i| i.id == TARGET) {
        info.parent = (at == 3).then(|| "ses_open_previous".into());
    }
    s.composer.focused = false;
    *s.fixture_run.open_demo = Some(Demo { page, send, png, at, before:false });
}

/// A Session opening story's setup: the click lands 0.75 s in.
fn open_at<const AT: usize>(s: &mut App, tx: &mut Play<App>) { open(s, AT, tx) }

fn open(s: &mut App, at: usize, tx: &mut Play<App>) {
    setup(s, at);
    if let Some(d) = s.fixture_run.open_demo.as_mut() {
        d.before = s.fixture_run.variant == 1;
    }
    tx.after(0.75, click);
}

fn click(s: &mut App, tx: &mut Play<App>) {
    tx.clear();
    let Some(d) = s.fixture_run.open_demo.as_ref() else { return };
    let (at, before) = (d.at, d.before);
    if before { s.open_now(TARGET, tx); s.open_page(TARGET, tx); } else { s.open(TARGET, tx); }
    if !matches!(at, 1 | 3) {
        tx.after(if at == 2 { 0.25 } else { 0.05 }, deliver);
        tx.after(if at == 2 { 0.4 } else { 0.1 }, pixels);
    }
}

/// The page arriving, then its picture: the O trigger's later steps.
pub(crate) fn arrive(s: &mut App, picture: bool) {
    let Some(d) = s.fixture_run.open_demo.as_ref() else { return };
    if picture {
        s.composer_ext.attach.fixture_media(MEDIA, Some(d.png.clone()));
    } else {
        d.send.send(Ok(d.page.clone())).ok();
    }
}

fn deliver(s: &mut App, _: &mut Play<App>) {
    // A second click's page finds the first's receiver gone.
    arrive(s, false);
}
fn pixels(s: &mut App, _: &mut Play<App>) {
    arrive(s, true);
}

pub fn view(s: &App, p: &Pal) -> El {
    let Some(d) = s.fixture_run.open_demo.as_ref() else { return col() };
    let Route::Session { id, .. } = &*s.route else { return col() };
    let label = if d.before { "Immediate transcript" } else { "Type while the transcript loads" };
    col().key("opening-stage").w_full().h_full()
        .child(text(label).font(crate::view::UI).color(p.muted).px(16.0).py(16.0))
        .child(crate::view::session_page(s, p, id))
}

fn picture() -> Vec<u8> {
    let image = image::RgbImage::from_fn(640, 360, |x,y| {
        let bar = x > 45 && x < 600 && x % 70 < 38 && y > 310 - (x / 70) * 25 && y < 310;
        image::Rgb(if bar { [92, 171, 190] } else if y % 60 == 10 { [58, 65, 72] } else { [30, 35, 42] })
    });
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).ok();
    out.into_inner()
}

/// One of the Session opening stories.
const fn story(at: usize) -> crate::stories::Story {
    crate::stories::Story { path: PATHS[at], scene: |s| crate::stories::stage(s, crate::stories::opening_view), knobs: &[], keys: KEYS[at], variants: VARIANTS }
}

rune_register::register!(crate::stories::STORY_LIST: crate::stories::Story = [story(0), story(1), story(2), story(3)]);
