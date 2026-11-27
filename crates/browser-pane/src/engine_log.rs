#![cfg(not(target_arch = "wasm32"))]
//! Bounded diagnostic logs for the browser engine sidecar, kept beside Chromium's `debug.log`
//! inside the engine profile directory:
//! - `engine-host.log` (rotated to `engine-host.prev.log` at 256 KiB): host-side lifecycle,
//!   startup timing, timeouts, exit codes, and fault transitions.
//! - `engine-stderr.log` (rotated to `engine-stderr.prev.log` on each spawn, capped at 256 KiB
//!   per run): drained by a host thread from a pipe so sandboxed CEF subprocesses never inherit a
//!   writable file handle and cannot exhaust disk space; also keeps a 1 KiB in-memory tail ring
//!   for immediate fault annotations without disk reads or Windows file-sharing races.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, Stdio};
use std::sync::{Arc, Mutex, PoisonError};

use rune::time::{SystemTime, UNIX_EPOCH};

use crate::installer::is_reparse_or_symlink;

const HOST_LOG: &str = "engine-host.log";
const HOST_LOG_PREV: &str = "engine-host.prev.log";
const STDERR_LOG: &str = "engine-stderr.log";
const STDERR_LOG_PREV: &str = "engine-stderr.prev.log";
const MAX_LOG_BYTES: u64 = 256 * 1024;
const STDERR_TAIL_BYTES: usize = 1024;

static LOG_LOCK: Mutex<()> = Mutex::new(());

/// Bounded in-memory ring holding the last `STDERR_TAIL_BYTES` bytes of one spawned engine's stderr.
#[derive(Clone, Default)]
pub(crate) struct StderrTail(Arc<Mutex<VecDeque<u8>>>);

impl StderrTail {
    fn push(&self, chunk: &[u8]) {
        let mut ring = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let tail = if chunk.len() >= STDERR_TAIL_BYTES {
            ring.clear();
            &chunk[chunk.len() - STDERR_TAIL_BYTES..]
        } else {
            let overflow = (ring.len() + chunk.len()).saturating_sub(STDERR_TAIL_BYTES);
            ring.drain(..overflow);
            chunk
        };
        ring.extend(tail);
    }

    /// Returns the buffered stderr tail as a compact single line (no disk I/O).
    pub(crate) fn snapshot(&self) -> Option<String> {
        let bytes: Vec<u8> = {
            let ring = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            if ring.is_empty() {
                return None;
            }
            ring.iter().copied().collect()
        };
        let text = String::from_utf8_lossy(&bytes);
        let compact = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" | ");
        (!compact.is_empty()).then_some(compact)
    }
}

fn ensure_private_dir(dir: &Path) -> bool {
    if is_reparse_or_symlink(dir) {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).is_ok()
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).is_ok()
    }
}

fn open_safe_log(path: &Path, append: bool) -> Option<File> {
    if is_reparse_or_symlink(path) {
        return None;
    }
    let mut opts = OpenOptions::new();
    opts.create(true).write(true);
    if append {
        opts.append(true);
    } else {
        opts.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path).ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

fn rotate_if_plain(src: &Path, dst: &Path) {
    if !is_reparse_or_symlink(src) && !is_reparse_or_symlink(dst) && src.is_file() {
        std::fs::rename(src, dst).ok();
    }
}

/// Appends one timestamped line to `<profile>/engine-host.log`, rotating at `MAX_LOG_BYTES`.
/// Never panics and ignores I/O failures so logging can never disrupt engine startup.
pub(crate) fn log(profile: Option<&Path>, message: &str) {
    let Some(dir) = profile else { return };
    let _guard = LOG_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    if !ensure_private_dir(dir) {
        return;
    }
    let path = dir.join(HOST_LOG);
    if !is_reparse_or_symlink(&path) && std::fs::symlink_metadata(&path).is_ok_and(|m| m.is_file() && m.len() >= MAX_LOG_BYTES) {
        rotate_if_plain(&path, &dir.join(HOST_LOG_PREV));
    }
    let Some(mut file) = open_safe_log(&path, true) else {
        return;
    };
    // Wall-clock timestamp for correlating host events with CEF's `debug.log` (native-only module).
    #[allow(clippy::disallowed_methods)]
    let since = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    writeln!(
        file,
        "{}.{:03} [host_pid={}] {message}",
        since.as_secs(),
        since.subsec_millis(),
        std::process::id(),
    )
    .ok();
}

/// Returns `Stdio::inherit()` when `OPENCODE_BROWSER_ENGINE_TRACE` is set, else `Stdio::piped()`
/// so the host drains stderr into a bounded file + in-memory ring without leaking a file handle
/// into sandboxed Chromium subprocesses.
pub(crate) fn stderr_stdio() -> Stdio {
    if std::env::var_os("OPENCODE_BROWSER_ENGINE_TRACE").is_some() {
        Stdio::inherit()
    } else {
        Stdio::piped()
    }
}

/// Spawns a host thread to drain `stderr` to EOF, keeping a 1 KiB in-memory tail and writing up
/// to `MAX_LOG_BYTES` to `<profile>/engine-stderr.log` (rotating any prior log first).
pub(crate) fn capture_stderr(mut stderr: ChildStderr, profile: PathBuf) -> StderrTail {
    let tail = StderrTail::default();
    let writer_tail = tail.clone();
    std::thread::Builder::new()
        .name("opencode-browser-stderr".into())
        .spawn(move || {
            let mut file = {
                let _guard = LOG_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
                if ensure_private_dir(&profile) {
                    let current = profile.join(STDERR_LOG);
                    rotate_if_plain(&current, &profile.join(STDERR_LOG_PREV));
                    open_safe_log(&current, false)
                } else {
                    None
                }
            };
            let mut written: u64 = 0;
            let mut buf = [0u8; 4096];
            loop {
                match stderr.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        writer_tail.push(chunk);
                        if let Some(f) = file.as_mut() {
                            let left = MAX_LOG_BYTES.saturating_sub(written) as usize;
                            if left > 0 {
                                let take = left.min(chunk.len());
                                if f.write_all(&chunk[..take]).is_ok() {
                                    written += take as u64;
                                } else {
                                    file = None;
                                }
                            } else {
                                // Stop writing past the cap, but keep draining the pipe to EOF
                                // so the engine can never block on stderr writes.
                                file = None;
                            }
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .ok();
    tail
}
