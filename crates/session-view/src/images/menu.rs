//! Right-clicking in the timeline: an image's items (Copy Image, Save Image As…, Open in
//! Preview), which a host puts before a message's own (`TimelineHost::context_menu`).

use std::path::Path;

use rune::{ClipboardContent, ClipboardImage, El, FileDialogOptions, PointerPress, PopupItem, PopupMenu, Tx};

use super::Dragged;
use super::drag::save_file_name;
use crate::TimelineHost;

pub const COPY: &str = "image-copy";
pub const SAVE: &str = "image-save";
pub const OPEN: &str = "image-open";

/// What a right-click landed on: message `message` in `session`, an image, or an image in a
/// message (`image` and `message` both set).
#[derive(Clone, Debug, Default)]
pub struct MenuAt {
    pub session: Option<String>,
    pub message: Option<String>,
    pub image: Option<Dragged>,
}

/// The image items, in order, as `(id, title)`.
pub fn items() -> [(&'static str, &'static str); 3] {
    let open = if cfg!(target_arch = "wasm32") {
        opencode_i18n::t!("rune.image.open")
    } else if cfg!(target_os = "macos") {
        opencode_i18n::t!("rune.image.revealFinder")
    } else if cfg!(windows) {
        opencode_i18n::t!("rune.image.revealExplorer")
    } else {
        opencode_i18n::t!("rune.image.revealFolder")
    };
    [(COPY, opencode_i18n::t!("rune.image.copy")), (SAVE, opencode_i18n::t!("rune.image.save")), (OPEN, open)]
}

/// `e` opens the timeline's menu for `at` on a right-click, in the open session unless `at`
/// names one.
pub fn on<S: TimelineHost>(e: El, at: MenuAt) -> El {
    e.on_secondary_press(move |s: &mut S, press: PointerPress, tx: &mut Tx| {
        let mut at = at.clone();
        if at.session.is_none() {
            at.session = s.active_session_id().map(str::to_owned);
        }
        s.context_menu(at, press, tx)
    })
}

/// `e`, an image showing `d`, opens its menu on a right-click, with message `message`'s items
/// after its own when it sits in one.
pub fn on_image<S: TimelineHost>(e: El, d: Dragged, message: Option<&str>) -> El {
    on::<S>(e, MenuAt { session: None, message: message.map(str::to_owned), image: Some(d) })
}

/// `e`, message `message`'s face, opens its menu on a right-click.
pub fn on_message<S: TimelineHost>(e: El, session: &str, message: &str) -> El {
    let session = (!session.is_empty()).then(|| session.to_owned());
    on::<S>(e, MenuAt { session, message: Some(message.to_owned()), image: None })
}

/// A read-only host's menu: the image's items, at the pointer; nothing for a message alone.
pub fn open_default<S: TimelineHost>(_s: &mut S, at: MenuAt, press: PointerPress, tx: &mut Tx) {
    let Some(d) = at.image else { return };
    let menu = items().into_iter().fold(PopupMenu::new(), |m, (id, title)| m.item(PopupItem::new(id, title)));
    tx.popup_menu(menu, press, move |s: &mut S, picked, tx| {
        if let Some(id) = picked {
            run(s, &d, id, tx);
        }
    });
}

/// Runs image item `id` on `d`. False when `id` isn't an image item.
pub fn run<S: TimelineHost>(s: &mut S, d: &Dragged, id: &str, tx: &mut Tx) -> bool {
    match id {
        COPY => {
            if let Some(content) = clipboard(s, d) {
                tx.copy_rich(content);
            }
        }
        SAVE => {
            let Some((bytes, hint)) = s.media_store().image_bytes(d) else { return true };
            let options = FileDialogOptions { title: opencode_i18n::t!("rune.image.save").trim_end_matches('\u{2026}').to_owned(), file_name: Some(save_file_name(&hint, &bytes)), ..Default::default() };
            tx.save_file(options, move |_: &mut S, answer, _| {
                if let Ok(Some(path)) = answer
                    && let Err(e) = std::fs::write(&path, &bytes)
                {
                    eprintln!("save image: {e}");
                }
            });
        }
        OPEN => {
            #[cfg(target_arch = "wasm32")]
            if let Some((bytes, _)) = s.media_store().image_bytes(d)
                && let Some(mime) = safe_raster_mime(&bytes)
            {
                web::open_image_blob(&bytes, mime);
            }
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(path) = s.media_store().image_file(d) {
                s.open_file(path);
            }
        }
        _ => return false,
    }
    true
}

/// What Copy Image puts on the clipboard: the picture as PNG (macOS adds TIFF), and its file,
/// so Messages, Slack and Preview paste the image and Finder pastes the file.
pub fn clipboard<S: TimelineHost>(s: &S, d: &Dragged) -> Option<ClipboardContent> {
    let store = s.media_store();
    let (bytes, _) = store.image_bytes(d)?;
    let png = if bytes.starts_with(b"\x89PNG") { bytes.to_vec() } else { to_png(&bytes)? };
    let content = ClipboardContent::new("").with_image(ClipboardImage::from_png(png)?);
    Some(match store.image_file(d) {
        Some(path) => content.with_files([path]),
        None => content,
    })
}

fn to_png(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// Safely reveals `path` in the file manager instead of executing it with the OS default handler.
/// Never invokes bare `explorer <path>` or `/usr/bin/open <path>`.
pub fn open_with_system(path: &Path) {
    let _ = reveal_with_system(path);
}

/// Shows `path` selected in the file manager through `browser_policy::reveal_in_file_manager`
/// (`SHOpenFolderAndSelectItems` on Windows, `/usr/bin/open -R --` on macOS, `xdg-open` parent
/// on Linux) after full local fixed-drive and no-reparse-point verification. Never touches UNC,
/// mapped drives, `subst`-to-UNC, symlinks/junctions, or autofs mounts.
pub fn reveal_with_system(path: &Path) -> bool {
    if rune::is_headless() {
        return false;
    }
    match harness_log::reveal(&path.to_string_lossy()) {
        Ok(_) => true,
        Err(e) => {
            eprintln!("reveal {}: {e}", path.display());
            false
        }
    }
}

/// Sniffs `bytes` against an explicit allow-list of non-scriptable raster image MIME types so
/// the `wasm32` web build never opens attacker-controlled `image/svg+xml` or active HTML in a
/// same-origin `blob:` tab.
#[cfg(target_arch = "wasm32")]
fn safe_raster_mime(bytes: &[u8]) -> Option<&'static str> {
    match super::drag::sniff_image_ext(bytes)? {
        "png" => Some("image/png"),
        "jpg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "ico" => Some("image/x-icon"),
        "avif" => Some("image/avif"),
        _ => None,
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use wasm_bindgen::prelude::wasm_bindgen;

    #[wasm_bindgen(inline_js = "export function openImageBlob(bytes, mime) {\n  const url = URL.createObjectURL(new Blob([bytes], { type: mime }));\n  window.open(url, '_blank', 'noopener,noreferrer');\n  setTimeout(() => URL.revokeObjectURL(url), 60000);\n}\n")]
    extern "C" {
        #[wasm_bindgen(js_name = openImageBlob)]
        pub fn open_image_blob(bytes: &[u8], mime: &str);
    }
}

