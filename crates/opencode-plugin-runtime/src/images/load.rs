//! One image's load, as an explicit machine (Desktop's `<img>` behind `site-icon.tsx`'s
//! `loading | loaded | failed`): its bytes are fetched on the store's worker, decoded by Rune's
//! image cache on its own threads, and then it has pixels or it failed. Where the bytes come
//! from is a [`Source`]; reading one runs on the worker, never on the UI thread.

use std::path::PathBuf;
use std::sync::Arc;

use rune::Image;

/// Where an image is.
pub enum Load {
    /// Its bytes are on their way from the worker.
    Fetching,
    /// Rune's image cache decodes them.
    Decoding { image: Image, bytes: Arc<[u8]> },
    Loaded { image: Image, bytes: Arc<[u8]>, size: [u32; 2] },
    Failed(String),
}

pub enum LoadEvent {
    /// The worker's answer, the bytes already handed to the image cache.
    Fetched(Result<(Image, Arc<[u8]>), String>),
    /// The image cache settled the decode (`Image::is_ready`).
    Decoded,
}

/// What a transition asks of the store.
#[derive(Debug, PartialEq, Eq)]
pub enum LoadEffect {
    /// What `Images:State` answers moved: the renders that read it run again.
    Changed,
}

impl Load {
    pub fn step(self, event: LoadEvent) -> (Load, Option<LoadEffect>) {
        match (self, event) {
            (Load::Fetching, LoadEvent::Fetched(Ok((image, bytes)))) => (Load::Decoding { image, bytes }, None),
            (Load::Fetching, LoadEvent::Fetched(Err(why))) => (Load::Failed(why), Some(LoadEffect::Changed)),
            (Load::Decoding { image, bytes }, LoadEvent::Decoded) => {
                let next = match (image.error(), image.size()) {
                    (Some(e), _) => Load::Failed(e.to_string()),
                    // An image with no pixels failed, as Desktop checks `naturalWidth > 0`.
                    (None, Some(size @ [w, h])) if w > 0 && h > 0 => Load::Loaded { image, bytes, size },
                    (None, _) => Load::Failed("the image has no pixels".into()),
                };
                (next, Some(LoadEffect::Changed))
            }
            (state, _) => (state, None),
        }
    }

    /// `Images:State`'s status.
    pub fn status(&self) -> &'static str {
        match self {
            Load::Fetching | Load::Decoding { .. } => "loading",
            Load::Loaded { .. } => "loaded",
            Load::Failed(_) => "failed",
        }
    }

    pub fn image(&self) -> Option<&Image> {
        match self {
            Load::Decoding { image, .. } | Load::Loaded { image, .. } => Some(image),
            Load::Fetching | Load::Failed(_) => None,
        }
    }

    pub fn bytes(&self) -> Option<&Arc<[u8]>> {
        match self {
            Load::Decoding { bytes, .. } | Load::Loaded { bytes, .. } => Some(bytes),
            Load::Fetching | Load::Failed(_) => None,
        }
    }
}

/// Where an image's bytes are read from (`Images:Load`'s `src`).
pub enum Source {
    /// The web, as a browser loads an `<img>`.
    Web(String),
    /// A path on the window's server, with its sign-in.
    Server { server: opencode_kit::client::Server, path: String },
    /// A `data:` URL.
    Data(String),
    /// A file of a plugin's folder.
    File(PathBuf),
    /// A text file of a built-in plugin's embedded copy (an SVG).
    Embedded(&'static str),
}

impl Source {
    /// Reads the bytes (on the store's worker).
    pub fn read(self) -> Result<Vec<u8>, String> {
        match self {
            Source::Web(url) => opencode_platform::web_fetch::get(&url),
            Source::Server { server, path } => server.get_bytes(&path).map_err(|e| e.to_string()),
            Source::Data(url) => data_url(&url),
            Source::File(path) => std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display())),
            Source::Embedded(text) => Ok(text.as_bytes().to_vec()),
        }
    }
}

/// Maximum `data:` URL length decoded for a plugin image (16 MiB encoded).
const MAX_DATA_URL_BYTES: usize = 16 << 20;

/// A `data:[<type>][;base64],<data>` URL's bytes.
fn data_url(url: &str) -> Result<Vec<u8>, String> {
    if url.len() > MAX_DATA_URL_BYTES {
        return Err("data URL exceeds 16 MiB limit".into());
    }
    use base64::Engine as _;
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    // As browsers read them: padding optional, whitespace ignored.
    const LENIENT: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    let rest = url.get(..5).filter(|s| s.eq_ignore_ascii_case("data:")).map(|_| &url[5..]).ok_or("not a data URL")?;
    let (meta, payload) = rest.split_once(',').ok_or("a data URL needs a comma")?;
    if meta.rsplit(';').next().is_some_and(|p| p.trim().eq_ignore_ascii_case("base64")) {
        let clean: Vec<u8> = percent_decode(payload).into_iter().filter(|c| !c.is_ascii_whitespace()).collect();
        return LENIENT.decode(clean).map_err(|e| format!("data URL: {e}"));
    }
    Ok(percent_decode(payload))
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).copied().and_then(hex), b.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

/// The MIME type of encoded image bytes, for the viewer's file (a drag out, Copy image).
pub fn mime(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [0xFF, 0xD8, ..] => "image/jpeg",
        [b'G', b'I', b'F', ..] => "image/gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        [b'B', b'M', ..] => "image/bmp",
        [0, 0, 1, 0, ..] => "image/x-icon",
        [_, _, _, _, b'f', b't', b'y', b'p', b'a', b'v', b'i', b'f', ..] => "image/avif",
        _ => "image/svg+xml",
    }
}
