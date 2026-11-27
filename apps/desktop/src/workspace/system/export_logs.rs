//! Export Logs, as Desktop's `writeDebugArchive` (`desktop/src/main/native/logging.ts`): a zip
//! in the Downloads folder named `opencode-debug-<UTC stamp>.zip`, holding `manifest.json`,
//! the last day's files from this app's logs (`desktop/`) and the local server's
//! (`server-1/`), each cut to its last 50 MB, then shown selected in the file manager.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::App;

/// Files older than this stay out (`EXPORT_WINDOW`).
const WINDOW_MS: i64 = 24 * 60 * 60 * 1000;
/// A larger file exports its tail (`MAX_EXPORT_FILE_SIZE`): server logs grow without rotation.
const MAX_FILE: u64 = 50 * 1024 * 1024;

impl App {
    /// Writes the archive off the app's thread and reveals it.
    pub(crate) fn export_logs(&self) {
        let now = *self.now_ms;
        let logs = crate::dirs::logs();
        let Some(downloads) = crate::dirs::downloads() else { return };
        let server_logs = server_log_roots();
        if let Err(e) = std::thread::Builder::new().name("export-logs".into()).spawn(move || {
            match write_archive(now, &downloads, logs.as_deref(), &server_logs) {
                Ok(path) => reveal(&path),
                Err(e) => eprintln!("export logs: {e}"),
            }
        }) {
            eprintln!("export logs: {e}");
        }
    }
}

/// The local server's log folder (`serverLogRoots`): `~/.local/share/opencode/log`.
fn server_log_roots() -> Vec<PathBuf> {
    let home = crate::dirs::home();
    if home.is_empty() {
        return Vec::new();
    }
    vec![Path::new(&home).join(".local").join("share").join("opencode").join("log")]
}

fn write_archive(now: i64, downloads: &Path, logs: Option<&Path>, server_logs: &[PathBuf]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(downloads)?;
    let output = downloads.join(format!("opencode-debug-{}.zip", stamp(now)));
    let mut files = Vec::new();
    if let Some(dir) = logs {
        collect(dir, "desktop", now, &mut files);
    }
    for (i, dir) in server_logs.iter().enumerate() {
        collect(dir, &format!("server-{}", i + 1), now, &mut files);
    }
    let truncated: Vec<&str> = files.iter().filter(|f| f.offset > 0).map(|f| f.name.as_str()).collect();
    let manifest = json!({
        "generated": crate::motion_log::iso(now as f64),
        "version": env!("CARGO_PKG_VERSION"),
        "name": "Shuvcode (Rune)",
        "packaged": !cfg!(debug_assertions),
        "platform": platform(),
        "arch": arch(),
        "userData": crate::dirs::data().map(|d| d.display().to_string()),
        "logs": logs.map(|d| d.display().to_string()),
        "serverLogs": server_logs.iter().map(|d| d.display().to_string()).collect::<Vec<_>>(),
        "truncated": truncated,
    });
    let mut zip = Zip::new(File::create(&output)?, dos_time(now));
    zip.add("manifest.json", serde_json::to_string_pretty(&manifest).unwrap_or_default().as_bytes())?;
    for f in &files {
        let mut data = Vec::new();
        let mut file = File::open(&f.path)?;
        file.seek(SeekFrom::Start(f.offset))?;
        file.read_to_end(&mut data)?;
        zip.add(&f.name, &data)?;
    }
    zip.finish()?;
    Ok(output)
}

struct Entry {
    name: String,
    path: PathBuf,
    offset: u64,
}

/// Every file under `dir` changed in the last day, named `<prefix>/<path in dir>`.
fn collect(dir: &Path, prefix: &str, now: i64, out: &mut Vec<Entry>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        let Ok(meta) = e.metadata() else { continue };
        let name = format!("{prefix}/{}", e.file_name().to_string_lossy());
        if meta.is_dir() {
            collect(&path, &name, now, out);
            continue;
        }
        let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
        if modified < now - WINDOW_MS || name.ends_with(".heapsnapshot") {
            continue;
        }
        out.push(Entry { name, path, offset: meta.len().saturating_sub(MAX_FILE) });
    }
}

/// `20261108T031502`: the UTC time without separators (`stamp()`).
fn stamp(now: i64) -> String {
    let iso = crate::motion_log::iso(now as f64);
    iso[..19].replace(['-', ':'], "")
}

/// The UTC time `now` as a zip entry's DOS time and date.
fn dos_time(now: i64) -> [u16; 2] {
    let iso = crate::motion_log::iso(now as f64);
    let n = |r: std::ops::Range<usize>| iso.get(r).and_then(|s| s.parse::<u16>().ok()).unwrap_or(0);
    let (y, mo, d, h, mi, s) = (n(0..4).max(1980), n(5..7), n(8..10), n(11..13), n(14..16), n(17..19));
    [h << 11 | mi << 5 | (s / 2), (y - 1980) << 9 | mo << 5 | d]
}

/// Node's `process.platform` and `process.arch` names, as Desktop's manifest has them.
fn platform() -> &'static str {
    match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    }
}

fn arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Shows `file` selected in the system's file manager (`shell.showItemInFolder`).
fn reveal(file: &Path) {
    if harness_log::record("reveal", &[("path", &file.to_string_lossy())]) {
        return;
    }
    if crate::headless() || crate::dirs::rooted() {
        eprintln!("export logs: wrote {}", file.display());
        return;
    }
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("explorer.exe").arg(format!("/select,{}", file.display())).spawn();
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("/usr/bin/open").arg("-R").arg(file).spawn();
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(file.parent().unwrap_or(file)).spawn();
    if let Err(e) = spawned {
        eprintln!("export logs: reveal: {e}");
    }
}

/// A zip writer for deflated entries (PKZIP 2.0, no data descriptors, so every unzipper and
/// macOS Archive Utility read it).
struct Zip<W: Write> {
    out: W,
    /// Every entry's DOS time and date.
    time: [u16; 2],
    at: u64,
    central: Vec<u8>,
    count: u16,
}

impl<W: Write> Zip<W> {
    fn new(out: W, time: [u16; 2]) -> Self {
        Zip { out, time, at: 0, central: Vec::new(), count: 0 }
    }

    fn add(&mut self, name: &str, data: &[u8]) -> std::io::Result<()> {
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data)?;
        let packed = enc.finish()?;
        let crc = crc32fast::hash(data);
        let (name, offset) = (name.as_bytes(), u32::try_from(self.at).unwrap_or(u32::MAX));
        let sizes = [u32::try_from(packed.len()).unwrap_or(u32::MAX), u32::try_from(data.len()).unwrap_or(u32::MAX)];
        let time = self.time;
        // Version 20, UTF-8 names (bit 11), deflate.
        let common = |h: &mut Vec<u8>| {
            h.extend(20u16.to_le_bytes());
            h.extend((1u16 << 11).to_le_bytes());
            h.extend(8u16.to_le_bytes());
            h.extend(time[0].to_le_bytes());
            h.extend(time[1].to_le_bytes());
            h.extend(crc.to_le_bytes());
            h.extend(sizes[0].to_le_bytes());
            h.extend(sizes[1].to_le_bytes());
            h.extend((name.len() as u16).to_le_bytes());
            h.extend(0u16.to_le_bytes());
        };
        let mut local = 0x0403_4b50u32.to_le_bytes().to_vec();
        common(&mut local);
        local.extend(name);
        self.out.write_all(&local)?;
        self.out.write_all(&packed)?;
        self.at += (local.len() + packed.len()) as u64;
        let c = &mut self.central;
        c.extend(0x0201_4b50u32.to_le_bytes());
        c.extend(20u16.to_le_bytes());
        common(c);
        // Comment length, disk, internal and external attributes.
        c.extend([0u8; 10]);
        c.extend(offset.to_le_bytes());
        c.extend(name);
        self.count += 1;
        Ok(())
    }

    fn finish(mut self) -> std::io::Result<()> {
        let size = self.central.len() as u32;
        self.out.write_all(&self.central)?;
        let mut end = 0x0605_4b50u32.to_le_bytes().to_vec();
        end.extend([0u8; 4]);
        end.extend(self.count.to_le_bytes());
        end.extend(self.count.to_le_bytes());
        end.extend(size.to_le_bytes());
        end.extend(u32::try_from(self.at).unwrap_or(u32::MAX).to_le_bytes());
        end.extend(0u16.to_le_bytes());
        self.out.write_all(&end)?;
        self.out.flush()
    }
}
