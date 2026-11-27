//! Files on user messages and tool results, and the user files the app keeps per message.

use super::*;

/// A file on a user message or a tool result. The server inlines its bytes, or, for a client
/// that opts in (see [`crate::client::MEDIA_CAPABILITY`]) on a session with
/// `experimental.media_blob` on, sends a `media:<hash>` ref: then `media` is the hash, `bytes`
/// is empty, and `GET /api/experimental/media/:hash` has the bytes.
#[derive(Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Attachment {
    pub mime: String,
    pub name: String,
    pub bytes: Arc<[u8]>,
    pub media: Option<String>,
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.media {
            Some(hash) => write!(f, "Attachment({}, {}, media:{hash})", self.mime, self.name),
            None => write!(f, "Attachment({}, {}, {} bytes)", self.mime, self.name, self.bytes.len()),
        }
    }
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        self.mime.starts_with("image/")
    }

    /// The server's `[media unavailable: <name> (<mime>)]`, which stands in for a ref whose
    /// stored bytes are gone.
    pub fn unavailable(&self) -> Option<&str> {
        if self.mime != "text/plain" || self.media.is_some() {
            return None;
        }
        std::str::from_utf8(&self.bytes).ok().filter(|t| t.starts_with("[media unavailable: ") && t.ends_with(']'))
    }
}

/// The blob hash in a `media:<hash>` ref (64 lowercase hex digits), the only form the server
/// sends stored media in.
pub fn media_hash(uri: &str) -> Option<&str> {
    uri.strip_prefix("media:").filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

/// Standard base64, padding optional; None on anything else.
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut n = 0u32;
        for (k, &c) in chunk.iter().enumerate() {
            n |= (val(c)? as u32) << (18 - 6 * k);
        }
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        match chunk.len() {
            4 => out.extend_from_slice(&bytes),
            3 => out.extend_from_slice(&bytes[..2]),
            2 => out.push(bytes[0]),
            _ => return None,
        }
    }
    Some(out)
}

/// One file: `{data, mime, name}` (base64), `{uri: "data:…;base64,…", mime, name}` or a
/// `{uri: "media:<hash>", mime, name, bytes}` ref. Other URIs (`file://`) are skipped.
pub fn parse_attachment(f: &Value) -> Option<Attachment> {
    // A part the server read from a path (a file comment's lines, a file sent as context) isn't
    // an attachment the user added (`attached`, `session-ui/src/components/message-file.ts`).
    if f["source"]["uri"].as_str().is_some_and(|u| u.starts_with("file:")) {
        return None;
    }
    if let Some(hash) = f["uri"].as_str().and_then(media_hash) {
        return Some(Attachment { mime: s(&f["mime"]), name: s(&f["name"]), media: Some(hash.to_owned()), ..Default::default() });
    }
    let (mime, bytes) = match (f["data"].as_str(), f["uri"].as_str()) {
        (Some(data), _) => (s(&f["mime"]), base64_decode(data)?),
        (None, Some(uri)) => {
            let (head, data) = uri.strip_prefix("data:")?.split_once(',')?;
            let head_mime = head.split(';').next().unwrap_or("");
            if !head.ends_with(";base64") {
                return None;
            }
            let mime = f["mime"].as_str().unwrap_or(head_mime).to_owned();
            (mime, base64_decode(data)?)
        }
        _ => return None,
    };
    Some(Attachment { mime, name: s(&f["name"]), bytes: bytes.into(), media: None })
}

pub(super) fn parse_attachments(list: &Value) -> Vec<Attachment> {
    list.as_array().into_iter().flatten().filter(|c| c["type"].as_str().is_none_or(|t| t == "file")).filter_map(parse_attachment).collect()
}

/// User messages' files, by message id. `Message::User` has no field for them, so apps that
/// build one as a literal keep compiling; `parse_message` fills this and apps read it.
fn user_file_store() -> &'static Mutex<HashMap<String, Arc<[Attachment]>>> {
    static STORE: OnceLock<Mutex<HashMap<String, Arc<[Attachment]>>>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}

/// Keeps only the user messages `keep` says are still held (`model::retain_user_messages`).
pub(crate) fn retain_user_files(keep: &dyn Fn(&str) -> bool) -> bool {
    let mut store = user_file_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let before = store.len();
    store.retain(|id, _| keep(id));
    store.len() != before
}

/// Moves whenever a prompt's files, comments or path attachments change: what a prompt's row
/// shows beside its words, held outside the transcript. A projection of the timeline that read
/// them keeps while it holds.
pub fn prompt_extras_generation() -> u64 {
    EXTRAS.load(std::sync::atomic::Ordering::Relaxed)
}

static EXTRAS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Records that a prompt's extras changed ([`prompt_extras_generation`]).
pub(crate) fn extras_changed() {
    EXTRAS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The files on user message `id` (empty when it has none).
pub fn user_files(id: &str) -> Arc<[Attachment]> {
    user_file_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(id).cloned().unwrap_or_else(|| Arc::from([]))
}

/// Records the files on user message `id`, as a local echo of a sent prompt has them.
pub fn set_user_files(id: &str, files: Vec<Attachment>) {
    let mut store = user_file_store().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    // A cached page and its refresh describe the same immutable prompt. Preserve the bytes'
    // identity so clients can keep already-decoded images while the refresh is parsed.
    if store.get(id).is_some_and(|old| old.as_ref() == files.as_slice()) { return; }
    if files.is_empty() {
        if store.remove(id).is_some() { extras_changed(); }
    } else {
        store.insert(id.to_owned(), files.into());
        extras_changed();
    }
}
