//! Composer attachment encoding, image paste, file helpers, and thumbnail/file chip row (`p254`).

use std::path::Path;

use opencode_kit::model::{Attachment, Message, Transcript};
use rune::{ClipboardContent, El, Live, ScrollPos, SurfaceFit, Tx, col, el, icon, row, text};
use rune_theme_opencode::icons as ic;

use super::ComposerHost;
use crate::focus::FocusExt;
use crate::images::{Fetched, Strip, identity, opens};
use crate::palette::Pal;
use crate::user_parts::{CARD_SUBTITLE, CARD_TITLE};

/// The server's cap on one inline attachment (20 MB).
pub const MAX_BYTES: usize = 20 << 20;

/// The MIME type by extension of media the model reads natively, else `None`.
pub fn native_mime(path: &Path) -> Option<&'static str> {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("pdf") => Some("application/pdf"),
        _ => None,
    }
}

/// Like [`native_mime`], filtered to modalities the selected model reads natively
/// (`attachments.ts:224-231`).
pub fn native_mime_for(path: &Path, input: crate::pickers::AttachmentInput) -> Option<&'static str> {
    match native_mime(path)? {
        "application/pdf" if input.pdf => Some("application/pdf"),
        m if m.starts_with("image/") && input.image => Some(m),
        _ => None,
    }
}

/// `attachmentMime`: native media by extension; otherwise text when the first 4 KB has no NUL
/// and few control bytes, else binary.
pub fn mime_for(path: &Path, head: &[u8]) -> &'static str {
    if let Some(m) = native_mime(path) {
        return m;
    }
    let control = head.iter().filter(|&&b| b < 9 || (b > 13 && b < 32)).count();
    if head.contains(&0) || (!head.is_empty() && control as f32 / head.len() as f32 > 0.3) {
        return "application/octet-stream";
    }
    "text/plain"
}

pub fn name_of(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_owned()
}

/// Reads up to 4 KB from `path` for MIME sniffing.
pub fn head(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut buf = Vec::with_capacity(4096);
    std::fs::File::open(path)
        .and_then(|f| f.take(4096).read_to_end(&mut buf))
        .map_err(|e| format!("{}: {e}", name_of(path)))?;
    Ok(buf)
}

/// Loads a native image/PDF file small enough to travel inline.
pub fn from_path(path: &Path) -> Result<Attachment, String> {
    let name = name_of(path);
    let mime = native_mime(path).ok_or_else(|| format!("{name} doesn't go inline"))?;
    let bytes = std::fs::read(path).map_err(|e| format!("{name}: {e}"))?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("{name} is over 20 MB"));
    }
    Ok(Attachment { mime: mime.into(), name, bytes: bytes.into(), media: None })
}

pub use opencode_kit::prompt::{attachment_inputs as inputs, path_lines};

/// Pastes files or an image from the clipboard into the host's composer attachments.
pub fn paste<S: ComposerHost>(s: &mut S, content: &ClipboardContent, _: &mut Tx) -> bool {
    if !content.files().is_empty() && s.composer_paste_files(content.files()) {
        return true;
    }
    let Some(img) = content.image() else { return false };
    let n = s.composer_attachments().len() + 1;
    s.composer_paste_attachment(Attachment {
        mime: "image/png".into(),
        name: format!("pasted-image-{n}.png"),
        bytes: img.png().into(),
        media: None,
    })
}

/// Deduplicating push helper for a host's `Vec<Attachment>`; returns `true` if inserted.
pub fn push_unique(list: &mut Vec<Attachment>, a: Attachment) -> bool {
    if list.iter().any(|f| f.bytes == a.bytes && f.media == a.media) {
        return false;
    }
    list.push(a);
    true
}

/// Settles local prompt echoes against freshly fetched server messages.
pub fn reconcile(echoes: &mut Vec<(String, String, String)>, t: &mut Transcript, fresh: &[Message]) {
    echoes.retain(|(session, id, body)| {
        if *session != t.session {
            return true;
        }
        if fresh.iter().any(|m| m.id() == id) {
            return false;
        }
        let known = |mid: &str| t.messages.iter().any(|m| m.id() == mid);
        let twin = fresh.iter().any(|m| {
            matches!(m, Message::User { id: mid, text } if mid != id && text.trim() == body && !known(mid))
        });
        if twin {
            t.messages.retain(|m| m.id() != id);
        }
        !twin
    });
}

/// Image and loading footprint (OpenCode 26f2522e60, `packages/app/src/composer/editor/editor.tsx`).
const THUMB: (f32, f32) = (78.0, 52.0);

/// Renders the composer's attachment chips: quote cards, image thumbnails, `160px` file
/// cards, and in-progress uploads, each with a round hover remove button.
pub fn chips<S: ComposerHost>(s: &S, p: &Pal) -> Option<El> {
    let quotes = s.composer_quote_chips(p);
    let files = s.composer_attachments();
    let paths = s.composer_path_attachments();
    let uploading = s.composer_uploading();
    if quotes.is_empty() && files.is_empty() && paths.is_empty() && uploading.is_empty() {
        return None;
    }
    let mut r = row().key("attachments-strip").px(8.0).pt(8.0).pb(4.0).gap(8.0).children(quotes);
    for f in files {
        let id = identity(f);
        let store = s.media_store();
        let (w, body) = match store.image_sized(f, 160.0).filter(|img| img.error().is_none()) {
            Some(img) => {
                let thumb = el()
                    .key("attachment-thumb")
                    .size(THUMB.0, THUMB.1)
                    .radius(6.0)
                    .fill(p.layer2)
                    .pixels_fit(img.pixels(), SurfaceFit::Fill)
                    .child(
                        el()
                            .key("thumb-ring")
                            .absolute(0.0, 0.0)
                            .size(THUMB.0, THUMB.1)
                            .radius(6.0)
                            .box_shadow(Pal::ring_in(p.border)),
                    );
                let held = el().key("held").size(THUMB.0, THUMB.1);
                (THUMB.0, opens(s, thumb, held, Strip::new(format!("composer:{id}"), std::sync::Arc::from([f.clone()]), 0), img, None))
            }
            None if f.is_image() && store.fetched(f) == Some(Fetched::Loading) => (THUMB.0, el().size(THUMB.0, THUMB.1)),
            None => (160.0, card(p, &f.name, type_label(&f.name)).help(f.name.clone()).help_above()),
        };
        let gone = id.clone();
        let remove = remove_button(p, &f.name, w, move |s: &mut S| s.composer_remove_attachment(&gone));
        r = r.child(el().key(format!("chip-{id}")).self_start().hover_group_focus().hover_motion(crate::style::INSTANT, crate::style::INSTANT).child(body).child(remove));
    }
    for f in paths {
        let gone = f.path.clone();
        let remove = remove_button(p, &f.name, 160.0, move |s: &mut S| s.composer_remove_path_attachment(&gone));
        r = r.child(
            el()
                .key(format!("path-{}", f.path))
                .hover_group_focus()
                .hover_motion(crate::style::INSTANT, crate::style::INSTANT)
                // Official wraps composer file cards in `<Tooltip value={file.path} placement="top">` (`editor.tsx:848`).
                .child(card(p, &f.name, type_label(&f.name)).help(f.path.clone()).help_above())
                .child(remove),
        );
    }
    // An upload's card and its cancel (`editor.tsx:880-899`).
    for (id, name, percent) in uploading {
        let gone = id.clone();
        let cancel = remove_button(p, &name, 160.0, move |s: &mut S| s.composer_cancel_upload(&gone)).label_static(opencode_i18n::t!("ui.promptInput.cancelUpload"));
        r = r.child(el().key(format!("uploading-{id}")).hover_group_focus().hover_motion(crate::style::INSTANT, crate::style::INSTANT).child(upload_card(p, &name, percent)).child(cancel));
    }
    // One row that scrolls sideways with no bar (`editor.tsx:758-910`), and a 24 pt bg-base
    // fade on each side that shows while chips hide under it (`attachments.css`).
    let fade = |key: &str, side: &'static std::thread::LocalKey<Live<f32>>, angle: f32| {
        let (left, right) = if angle > 0.0 { (Some(0.0), None) } else { (None, Some(0.0)) };
        side.with(|o| {
            el().key(key.to_owned())
                .pin(left, Some(0.0), right, Some(0.0))
                .w(24.0)
                .linear_gradient(angle, [(0.0, p.bg_base), (1.0, p.bg_base.alpha(0.0))])
                .live_opacity(o)
                .inert()
        })
    };
    let strip = el()
        .key("attachments-scroll")
        .w_full()
        .scroll_x()
        .on_scroll_offset(|at: ScrollPos| {
            FADE_LEFT.with(|o| o.set(if at.offset[0] > 0.1 { 1.0 } else { 0.0 }));
            FADE_RIGHT.with(|o| o.set(if at.max[0] > 0.0 && at.offset[0] < at.max[0] - 1.0 { 1.0 } else { 0.0 }));
        })
        .child(r);
    Some(
        el().key("attachments")
            .w_full()
            // The fades keep to the composer's rounded top corners.
            .clip_corners([10.0, 10.0, 0.0, 0.0])
            .child(strip)
            .child(fade("attachments-fade-left", &FADE_LEFT, std::f32::consts::FRAC_PI_2))
            .child(fade("attachments-fade-right", &FADE_RIGHT, -std::f32::consts::FRAC_PI_2)),
    )
}

thread_local! {
    /// Whether chips hide under the strip's left and right edges: one composer shows at a time.
    static FADE_LEFT: Live<f32> = Live::new(0.0);
    static FADE_RIGHT: Live<f32> = Live::new(0.0);
}

/// The round button on a chip's corner that shows on hover, for a chip `w` wide.
pub fn remove_button<S: 'static>(
    p: &Pal,
    _name: &str,
    w: f32,
    on: impl Fn(&mut S) + 'static,
) -> El {
    el()
        .key("remove")
        .role("button")
        .label_static(opencode_i18n::t!("ui.promptInput.removeAttachment"))
        .absolute(w - 12.0, -4.0)
        .size(16.0, 16.0)
        .radius(8.0)
        .center()
        .focus_default(p)
        .fill(p.icon_muted)
        .outline(1.0, 0.0, p.icon_contrast)
        .group_opacity(if crate::style::hover_none() { 1.0 } else { 0.0 }, 1.0)
        .on_press_tx(move |s: &mut S, tx: &mut Tx| {
            tx.instant();
            on(s);
        })
        .child(icon(&ic::XMARK_SMALL, 12.0).color(p.icon_contrast))
}

/// `AttachmentCard` (`attachment-card.css`): 160 wide, 8 in, its title over its subtitle 6
/// apart (the subtitle's line at least 16 tall), on the hover overlay with a 0.5 hairline in
/// `border-base`.
pub fn card(p: &Pal, title: &str, subtitle: String) -> El {
    col()
        .key("card")
        .w(160.0)
        .padding(8.0)
        .gap(6.0)
        .radius(6.0)
        .fill(p.hover)
        .box_shadow(Pal::ring_in(p.border))
        .child(text(title.to_owned()).font(CARD_TITLE).color(p.text).truncate_fade().w_full())
        .child(row().key("subtitle").w_full().min_h(16.0).center_y().child(text(subtitle).font(CARD_SUBTITLE).color(p.muted).truncate_fade()))
}

/// An upload's card (`editor.tsx:880-890`): its file's name over the progress ring and the
/// percent 4 apart.
pub fn upload_card(p: &Pal, title: &str, percent: u32) -> El {
    let subtitle = row()
        .key("subtitle")
        .w_full()
        .min_h(16.0)
        .gap(4.0)
        .center_y()
        .child(progress_circle(p, percent))
        .child(text(opencode_i18n::t!("ui.promptInput.uploading", percent = percent)).font(CARD_SUBTITLE).color(p.muted).truncate_fade());
    col()
        .key("card")
        .w(160.0)
        .padding(8.0)
        .gap(6.0)
        .radius(6.0)
        .fill(p.hover)
        .box_shadow(Pal::ring_in(p.border))
        .child(text(title.to_owned()).font(CARD_TITLE).color(p.text).truncate_fade().w_full())
        .child(subtitle)
}

/// `ProgressCircle`'s compact look (`progress-circle.css`): a 14 pt ring, 1.5 wide, its track
/// `bg-layer-04` and the part done `icon-base` from twelve o'clock clockwise, which moves on
/// `stroke-dashoffset 0.35s cubic-bezier(0.65, 0, 0.35, 1)`. Each half of the ring is a
/// half-circle turning into a clip of its own, so the arc grows as the halves turn.
pub fn progress_circle(p: &Pal, percent: u32) -> El {
    const SIZE: f32 = 14.0;
    const STROKE: f32 = 1.5;
    let r = SIZE / 2.0 - STROKE / 2.0;
    // Clockwise from twelve o'clock, `from` to `to` turns.
    let arc = |from: f32, to: f32| -> Vec<[f32; 2]> {
        (0..=24).map(|i| {
            let t = std::f32::consts::TAU * (from + (to - from) * i as f32 / 24.0);
            [SIZE / 2.0 + r * t.sin(), SIZE / 2.0 - r * t.cos()]
        }).collect()
    };
    let ring = |from: f32, to: f32, color: rune::Color| rune::Shape::new().view_box(SIZE, SIZE).stroke_with(arc(from, to), false, STROKE, rune::LineCap::Butt, color);
    let done = (percent.min(100) as f32) / 100.0;
    let curve = || rune::Curve::tween(0.35, rune::Easing::cubic(0.65, 0.0, 0.35, 1.0));
    // A half turned `turn` of half a circle about the ring's centre, seen through the half at `x`.
    let half = |key: &'static str, x: f32, from: f32, turn: f32| {
        el().key(key).absolute(x, 0.0).size(SIZE / 2.0, SIZE).clip_soft(0.0).child(
            el().absolute(-x, 0.0).size(SIZE, SIZE).shape(ring(from, from + 0.5, p.icon)).rotate(turn * std::f32::consts::PI).rotate_motion(curve()),
        )
    };
    el()
        .key("progress-circle")
        .size(SIZE, SIZE)
        .shape(ring(0.0, 1.0, p.q.layer4))
        .child(half("progress-right", SIZE / 2.0, 0.5, (done * 2.0).min(1.0)))
        .child(half("progress-left", 0.0, 0.0, (done * 2.0 - 1.0).max(0.0)))
}

/// A file sent as context without a mention (`editor.tsx:802-830`): the card on the base
/// surface (the hover overlay in dark, `attachment-card.css`), its subtitle the file's 12 pt
/// icon, then its type and lines (`TypeScript:12-20`).
pub fn context_card(p: &Pal, title: &str, icon: El, label: String) -> El {
    col()
        .key("card")
        .w(160.0)
        .padding(8.0)
        .gap(6.0)
        .radius(6.0)
        .fill(if p.dark { p.hover } else { p.bg_base })
        .box_shadow(Pal::ring_in(p.border))
        .child(text(title.to_owned()).font(CARD_TITLE).color(p.text).truncate_fade().w_full())
        .child(row().h(16.0).gap(4.0).center_y().child(icon).child(text(label).font(CARD_SUBTITLE).color(p.muted).truncate_fade().shrink().min_w(0.0)))
}

/// Desktop's `typeLabel` (`message-file.ts`): a source file's language by its extension, any
/// other extension in capitals, and File without one (a dotfile has none).
pub fn type_label(name: &str) -> String {
    let base = opencode_kit::path::basename(name);
    let ext = match base.rfind('.') {
        Some(i) if i > 0 => base[i + 1..].to_ascii_lowercase(),
        _ => return opencode_i18n::t!("ui.common.file").into(),
    };
    if ext == "pdf" {
        return "PDF".into();
    }
    LANGUAGES.iter().find(|(_, exts)| exts.contains(&ext.as_str())).map_or_else(|| ext.to_uppercase(), |(lang, _)| (*lang).to_owned())
}

const LANGUAGES: &[(&str, &[&str])] = &[
    ("Assembly", &["asm", "s"]),
    ("Astro", &["astro"]),
    ("Batch File", &["bat", "cmd"]),
    ("C", &["c", "h"]),
    ("C#", &["cs"]),
    ("C++", &["cpp", "cc", "cxx", "hpp", "hh", "hxx"]),
    ("Clojure", &["clj", "cljs", "cljc", "edn"]),
    ("CSS", &["css"]),
    ("CUDA", &["cu", "cuh"]),
    ("Dart", &["dart"]),
    ("Diff", &["diff", "patch"]),
    ("Elixir", &["ex", "exs"]),
    ("Elm", &["elm"]),
    ("Erlang", &["erl", "hrl"]),
    ("F#", &["fs", "fsi", "fsx"]),
    ("Fortran", &["f", "f77", "f90", "f95", "f03", "f08", "for"]),
    ("GDScript", &["gd"]),
    ("Gleam", &["gleam"]),
    ("GLSL", &["glsl", "vert", "frag", "geom", "comp"]),
    ("Go", &["go"]),
    ("GraphQL", &["graphql", "gql"]),
    ("Groovy", &["groovy", "gradle"]),
    ("Haskell", &["hs"]),
    ("HCL", &["hcl"]),
    ("HLSL", &["hlsl"]),
    ("HTML", &["html", "htm"]),
    ("Java", &["java"]),
    ("JavaScript", &["js", "cjs", "mjs"]),
    ("JSON", &["json"]),
    ("JSON Lines", &["jsonl"]),
    ("JSON with Comments", &["jsonc"]),
    ("JSON5", &["json5"]),
    ("JSX", &["jsx"]),
    ("Julia", &["jl"]),
    ("Kotlin", &["kt", "kts"]),
    ("LaTeX", &["tex", "latex"]),
    ("Less", &["less"]),
    ("Lua", &["lua"]),
    ("Markdown", &["md", "markdown"]),
    ("MDX", &["mdx"]),
    ("Nim", &["nim"]),
    ("Nix", &["nix"]),
    ("OCaml", &["ml", "mli"]),
    ("Perl", &["pl", "pm"]),
    ("PHP", &["php"]),
    ("PowerShell", &["ps1", "psm1"]),
    ("Protocol Buffer", &["proto"]),
    ("Python", &["py", "pyi"]),
    ("R", &["r"]),
    ("Ruby", &["rb"]),
    ("Rust", &["rs"]),
    ("Sass", &["sass"]),
    ("Scala", &["scala", "sbt"]),
    ("SCSS", &["scss"]),
    ("Shell", &["sh", "bash", "zsh"]),
    ("Solidity", &["sol"]),
    ("SQL", &["sql"]),
    ("Svelte", &["svelte"]),
    ("SVG", &["svg"]),
    ("Swift", &["swift"]),
    ("Terraform", &["tf", "tfvars"]),
    ("TOML", &["toml"]),
    ("TSX", &["tsx"]),
    ("TypeScript", &["ts", "cts", "mts"]),
    ("Vue", &["vue"]),
    ("WebAssembly", &["wat", "wasm"]),
    ("WGSL", &["wgsl"]),
    ("XML", &["xml"]),
    ("YAML", &["yaml", "yml"]),
    ("Zig", &["zig"]),
];
