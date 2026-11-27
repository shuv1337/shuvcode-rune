//! Finding a project's icon: the server's image, else the app's own icon in its directory.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use crate::pixels::{Buf, lin};

const MAX_BYTES: u64 = 8 << 20;
const MAX_SIDE: u32 = 4096;
/// Directories the repo walk never enters.
const SKIP: &[&str] = &[
    "node_modules", "target", "dist", "out", "DerivedData", "Pods", "vendor", "third_party", "artifacts", "examples",
    "fixtures", "tests", "test", "coverage", "tmp",
];

/// Icon files at fixed places: Tauri, Electron and extension builds, then web icons.
fn fixed() -> Vec<(String, Kind)> {
    let mut v: Vec<(String, Kind)> = [
        "src-tauri/icons/icon.png", "src-tauri/icons/128x128@2x.png", "src-tauri/icons/icon.icns", "build/icon.png",
        "build/icon.icns", "resources/icon.png", "resources/icon.icns", "icons/icon.png", "extension/icons/icon.png",
        "assets/app-icon.png", "assets/icon.png", "icon.png",
    ]
    .iter()
    .map(|f| (f.to_string(), Kind::App))
    .collect();
    for d in ["", "public/", "static/", "site/", "site/public/", "site/static/", "assets/", "app/", "src/app/", "web/public/"] {
        v.push((format!("{d}apple-touch-icon.png"), Kind::Touch));
        for f in ["favicon.png", "icon.png", "icon.jpg"] {
            v.push((format!("{d}{f}"), Kind::Web));
        }
        for f in ["logo.png", "logo.jpg", "logo.jpeg"] {
            v.push((format!("{d}{f}"), Kind::Logo));
        }
    }
    v
}

/// Where an icon came from and when that changed, so an unchanged file isn't decoded again.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Key(pub String, pub SystemTime);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    /// A macOS, Tauri or Electron app icon (`.icns`, `AppIcon.appiconset`, `build/icon.png`).
    App,
    /// An Icon Composer `.icon`, flattened here over its fill.
    Composer,
    Touch,
    Web,
    Logo,
}

#[derive(Debug)]
pub(crate) struct Cand {
    path: PathBuf,
    pub kind: Kind,
    side: u32,
    square: bool,
    depth: usize,
    named: bool,
    /// For a catalog or `.icon`: the image inside it to read.
    pub file: Option<PathBuf>,
}

impl Cand {
    pub(crate) fn rank(&self) -> (bool, bool, Kind, bool, bool, usize, PathBuf) {
        (!self.square, self.side < 64, self.kind, self.side < 128, !self.named, self.depth, self.path.clone())
    }

    pub(crate) fn key(&self) -> Option<Key> {
        let file = self.file.as_ref().unwrap_or(&self.path);
        let mut t = mtime(file)?;
        if self.kind == Kind::Composer {
            t = t.max(mtime(&self.path.join("icon.json"))?);
        }
        Some(Key(file.to_string_lossy().into_owned(), t))
    }
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

pub(crate) fn candidates(root: &Path, name: &str) -> Vec<Cand> {
    let mut out = Vec::new();
    let lname = name.to_lowercase();
    let depth_of = |p: &Path| p.strip_prefix(root).map_or(0, |r| r.components().count());
    for (rel, kind) in fixed() {
        let path = root.join(&rel);
        if let Some(c) = file_cand(&path, kind, depth_of(&path), &lname) {
            out.push(c);
        }
    }
    let mut visited = 0usize;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            visited += 1;
            if visited > 4000 {
                return out;
            }
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with('.') {
                continue;
            }
            let path = e.path();
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if n.ends_with(".appiconset") {
                    out.extend(catalog(&path, depth + 1, &lname));
                } else if n.ends_with(".icon") {
                    out.extend(composer(&path, depth + 1, &lname));
                } else if n.ends_with(".xcassets") || (depth < 3 && !SKIP.contains(&n.as_str())) {
                    stack.push((path, depth + 1));
                }
            } else if n.ends_with(".icns") {
                out.extend(file_cand(&path, Kind::App, depth + 1, &lname));
            }
        }
    }
    out
}

fn named(path: &Path, lname: &str) -> bool {
    !lname.is_empty() && path.to_string_lossy().to_lowercase().rsplit(opencode_kit::path::SEPARATORS).take(3).any(|c| c.contains(lname))
}

fn file_cand(path: &Path, kind: Kind, depth: usize, lname: &str) -> Option<Cand> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() == 0 || meta.len() > MAX_BYTES {
        return None;
    }
    let (w, h) = dims(path)?;
    Some(Cand { path: path.to_owned(), kind, side: w.min(h), square: square(w, h), depth, named: named(path, lname), file: None })
}

fn square(w: u32, h: u32) -> bool {
    w.max(h) as f32 / w.min(h).max(1) as f32 <= 1.15
}

/// An `AppIcon.appiconset`: its smallest image of at least 256 px, else its largest.
fn catalog(dir: &Path, depth: usize, lname: &str) -> Option<Cand> {
    let mut best: Option<(PathBuf, u32, u32)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten().take(128) {
        let p = e.path();
        let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
        if !matches!(ext.as_str(), "png" | "jpg" | "jpeg") || std::fs::metadata(&p).map_or(true, |m| m.len() > MAX_BYTES) {
            continue;
        }
        let Some((w, h)) = dims(&p) else { continue };
        let better = |s: u32, b: u32| if s >= 256 && b >= 256 { s < b } else { s > b };
        if best.as_ref().is_none_or(|b| better(w.min(h), b.1.min(b.2))) {
            best = Some((p, w, h));
        }
    }
    let (file, w, h) = best?;
    Some(Cand { path: dir.to_owned(), kind: Kind::App, side: w.min(h), square: square(w, h), depth, named: named(dir, lname), file: Some(file) })
}

/// An Icon Composer `.icon` whose layers include a PNG or JPEG.
fn composer(dir: &Path, depth: usize, lname: &str) -> Option<Cand> {
    let manifest = read_json(&dir.join("icon.json"))?;
    let has_raster = layers(&manifest).any(|(name, _, _)| {
        let n = name.to_lowercase();
        (n.ends_with(".png") || n.ends_with(".jpg") || n.ends_with(".jpeg")) && dir.join("Assets").join(name).is_file()
    });
    if !has_raster {
        return None;
    }
    Some(Cand { path: dir.to_owned(), kind: Kind::Composer, side: 1024, square: true, depth, named: named(dir, lname), file: None })
}

fn read_json(p: &Path) -> Option<Value> {
    if std::fs::metadata(p).ok()?.len() > 1 << 20 {
        return None;
    }
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

/// A manifest's layers, top first: image name, scale and translation in points.
fn layers(v: &Value) -> impl Iterator<Item = (&str, f32, (f32, f32))> {
    v["groups"].as_array().into_iter().flatten().flat_map(|g| g["layers"].as_array().into_iter().flatten()).filter_map(|l| {
        let name = l["image-name"].as_str()?;
        let pos = &l["position"];
        let scale = pos["scale"].as_f64().unwrap_or(1.0) as f32;
        let t = &pos["translation-in-points"];
        Some((name, scale, (t[0].as_f64().unwrap_or(0.0) as f32, t[1].as_f64().unwrap_or(0.0) as f32)))
    })
}

/// Width and height without decoding: PNG and JPEG headers, or an ICNS's best PNG.
fn dims(path: &Path) -> Option<(u32, u32)> {
    let ext = path.extension().and_then(|x| x.to_str()).unwrap_or("").to_lowercase();
    let d = if ext == "icns" {
        let bytes = std::fs::read(path).ok()?;
        let png = icns_png(&bytes)?;
        png_dims(png)?
    } else {
        image::image_dimensions(path).ok()?
    };
    (d.0 > 0 && d.1 > 0 && d.0 <= MAX_SIDE && d.1 <= MAX_SIDE).then_some(d)
}

pub(crate) fn png_dims(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || &b[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    Some((be(16), be(20)))
}

/// The PNG representation in an ICNS that best fits: the smallest of at least 256 px, else the
/// largest. Legacy (non-PNG) representations are skipped.
pub(crate) fn icns_png(b: &[u8]) -> Option<&[u8]> {
    if b.len() < 8 || &b[..4] != b"icns" {
        return None;
    }
    let mut best: Option<(&[u8], u32)> = None;
    let mut at = 8;
    while at + 8 <= b.len() {
        let len = u32::from_be_bytes([b[at + 4], b[at + 5], b[at + 6], b[at + 7]]) as usize;
        if len < 8 || at + len > b.len() {
            break;
        }
        let data = &b[at + 8..at + len];
        if let Some((w, h)) = png_dims(data) {
            let s = w.min(h);
            let better = |o: u32| if s >= 256 && o >= 256 { s < o } else { s > o };
            if w == h && best.is_none_or(|(_, o)| better(o)) {
                best = Some((data, s));
            }
        }
        at += len;
    }
    best.map(|(d, _)| d)
}

/// The candidate's pixels, and whether to trim transparent margins.
pub(crate) fn decode(c: &Cand) -> Option<(Buf, bool)> {
    if c.kind == Kind::Composer {
        return flatten_composer(&c.path).map(|b| (b, false));
    }
    let file = c.file.as_ref().unwrap_or(&c.path);
    let bytes = std::fs::read(file).ok()?;
    let img = if file.extension().is_some_and(|x| x.eq_ignore_ascii_case("icns")) {
        image::load_from_memory_with_format(icns_png(&bytes)?, image::ImageFormat::Png).ok()?
    } else {
        image::load_from_memory(&bytes).ok()?
    };
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return None;
    }
    Some((Buf::from_rgba8(&img.into_rgba8()), true))
}

/// An Icon Composer icon on a 256 px canvas: its solid or gradient fill, then its raster layers.
fn flatten_composer(dir: &Path) -> Option<Buf> {
    const C: u32 = 256;
    let v = read_json(&dir.join("icon.json"))?;
    let fill = &v["fill"];
    let stops: Vec<[f32; 4]> = match fill["solid"].as_str() {
        Some(s) => composer_color(s).into_iter().collect(),
        None => fill["linear-gradient"].as_array().into_iter().flatten().filter_map(|c| c.as_str().and_then(composer_color)).collect(),
    };
    let stops = if stops.is_empty() { vec![[1.0, 1.0, 1.0, 1.0]] } else { stops };
    let o = &fill["orientation"];
    let pt = |k: &str, d: (f32, f32)| (o[k]["x"].as_f64().map_or(d.0, |x| x as f32), o[k]["y"].as_f64().map_or(d.1, |y| y as f32));
    let (a, b) = (pt("start", (0.5, 0.0)), pt("stop", (0.5, 1.0)));
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = (dx * dx + dy * dy).max(1e-6);
    let mut canvas = Buf::new(C, C);
    for y in 0..C {
        for x in 0..C {
            let (u, v) = ((x as f32 + 0.5) / C as f32, (y as f32 + 0.5) / C as f32);
            let t = (((u - a.0) * dx + (v - a.1) * dy) / len2).clamp(0.0, 1.0);
            canvas.px[(y * C + x) as usize] = gradient(&stops, t);
        }
    }
    let k = C as f32 / 1024.0;
    let mut drew = false;
    let all: Vec<_> = layers(&v).collect();
    for (name, scale, (tx, ty)) in all.into_iter().rev() {
        let file = dir.join("Assets").join(name);
        let lower = name.to_lowercase();
        if !(lower.ends_with(".png") || lower.ends_with(".jpg") || lower.ends_with(".jpeg")) {
            continue;
        }
        if std::fs::metadata(&file).map_or(true, |m| m.len() > MAX_BYTES) {
            continue;
        }
        let Some(img) = std::fs::read(&file).ok().and_then(|b| image::load_from_memory(&b).ok()) else { continue };
        if img.width() > MAX_SIDE || img.height() > MAX_SIDE {
            continue;
        }
        let w = ((img.width() as f32 * scale * k).round() as u32).clamp(1, C * 2);
        let h = ((img.height() as f32 * scale * k).round() as u32).clamp(1, C * 2);
        let layer = Buf::from_rgba8(&img.into_rgba8()).resize(w, h);
        let ox = ((C as f32 - w as f32) / 2.0 + tx * k).round() as i64;
        let oy = ((C as f32 - h as f32) / 2.0 + ty * k).round() as i64;
        canvas.over(&layer, ox, oy);
        drew = true;
    }
    drew.then_some(canvas)
}

/// `gray:g,a`, `srgb:r,g,b,a` or `display-p3:r,g,b,a` (read as sRGB) to linear premultiplied.
fn composer_color(s: &str) -> Option<[f32; 4]> {
    let (space, rest) = s.split_once(':')?;
    let n: Vec<f32> = rest.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    let (r, g, b, a) = match (space, n.as_slice()) {
        ("gray", [g, a]) => (*g, *g, *g, *a),
        ("srgb" | "extended-srgb" | "display-p3", [r, g, b, a]) => (*r, *g, *b, *a),
        _ => return None,
    };
    let a = a.clamp(0.0, 1.0);
    Some([lin(r.clamp(0.0, 1.0)) * a, lin(g.clamp(0.0, 1.0)) * a, lin(b.clamp(0.0, 1.0)) * a, a])
}

fn gradient(stops: &[[f32; 4]], t: f32) -> [f32; 4] {
    if stops.len() == 1 {
        return stops[0];
    }
    let f = t * (stops.len() - 1) as f32;
    let i = (f.floor() as usize).min(stops.len() - 2);
    let u = f - i as f32;
    std::array::from_fn(|c| stops[i][c] + (stops[i + 1][c] - stops[i][c]) * u)
}

/// A server image the app can draw: PNG, JPEG or SVG.
#[derive(Debug, PartialEq)]
pub(crate) enum Data {
    Raster(Vec<u8>),
    Svg(Vec<u8>),
}

/// A `data:` URL's image, base64 or percent-encoded. Other URLs and types are None.
pub(crate) fn data_url(u: &str) -> Option<Data> {
    // Reject oversized data URLs before decoding (matches repo icon MAX_BYTES = 8 MiB).
    if u.len() > (MAX_BYTES as usize) * 2 {
        return None;
    }
    let (head, body) = u.strip_prefix("data:")?.split_once(',')?;
    let mut parts = head.split(';');
    let mime = parts.next()?.trim().to_ascii_lowercase();
    let bytes = if parts.any(|p| p.eq_ignore_ascii_case("base64")) { base64(body)? } else { percent(body) };
    if bytes.len() as u64 > MAX_BYTES {
        return None;
    }
    match mime.as_str() {
        "image/svg+xml" => Some(Data::Svg(bytes)),
        m if RASTER.contains(&m) => Some(Data::Raster(bytes)),
        _ => None,
    }
}

/// The raster types the server's icon finder sends (`packages/core/src/project/icon.ts`: png,
/// jpeg, webp, and `.ico` as `image/x-icon`), and `.ico`'s registered name.
const RASTER: &[&str] = &["image/png", "image/jpeg", "image/jpg", "image/webp", "image/x-icon", "image/vnd.microsoft.icon"];

pub(crate) fn is_data_image(u: &str) -> bool {
    u.strip_prefix("data:").and_then(|r| r.split([';', ',']).next()).is_some_and(|m| m.eq_ignore_ascii_case("image/svg+xml") || RASTER.iter().any(|r| m.eq_ignore_ascii_case(r)))
}

fn percent(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%'
            && let (Some(h), Some(l)) = (b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c)))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}
