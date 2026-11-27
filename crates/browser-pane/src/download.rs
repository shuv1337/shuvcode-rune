//! The engine archive as one stream of bytes, read once: first whatever an earlier try left in
//! the partial file, then the rest from the source (an HTTP `Range` from where it ended), each
//! new byte written to the partial file as it passes, so a cut-short download resumes. The
//! reader hashes everything it hands on, so the SHA-256 covers the whole archive however many
//! tries it took, and the archive unpacks while it downloads.
//!
//! A connection that goes silent (no byte and no error: a network that dropped it without
//! telling, a computer back from sleep) would hold a read for ever, since the HTTP library only
//! limits a whole body's time. So the source is read on a thread of its own (`Watched`) and the
//! stream waits for its bytes with a clock: after `STALL` without one the try ends as
//! `Cut::Stalled`, the partial file keeps what came, and the next try goes on from there.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, sync_channel};
use std::time::Duration;
use rune::time::Instant;

use sha2::{Digest, Sha256};

use crate::installer::{Fault, Net, err_write};
use crate::source;

/// How long the source may send nothing before its connection counts as gone silent.
const STALL: Duration = Duration::from_secs(25);
/// The same for a download nobody waited for when a page begins to wait for it: a silence this
/// long already is reason enough to connect again at once, rather than have the page watch a
/// bar that doesn't move for the rest of `STALL`.
const JOIN_STALL: Duration = Duration::from_secs(3);
/// How often a wait for the source's next bytes asks the pace again (a page may wait now).
const STALL_SLICE: Duration = Duration::from_millis(250);
/// The longest one connection carries the archive. Past it the try ends and the next goes on
/// from the partial file; it is also when a reader left behind on a silent connection ends.
const BODY_LIMIT: Duration = Duration::from_secs(60 * 60);

/// How fast the next bytes may come, asked before each read from the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pace {
    /// A page waits: as fast as they come.
    Full,
    /// Nobody waits: as fast as they come, quietly.
    Quiet,
    /// At most this many bytes a second.
    Limit(u64),
    /// Stop now and keep the partial file for later.
    Stop,
}

/// Why the stream ended before the archive did.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Cut {
    /// `pace` said stop; the partial file keeps what came.
    Stopped,
    /// The source sent nothing for `STALL` (or `JOIN_STALL`, when a page began to wait): its
    /// connection is dropped. The partial file keeps what came.
    Stalled,
    /// The network or the disk failed. The bytes so far stay.
    Failed(Fault),
    /// The bytes don't check out (size, hash, archive, trust rule). The partial file goes, so
    /// the next try starts over.
    Rejected(Fault),
}

/// HTTP statuses that mean someone refused the download (a proxy, a filter, the server), not
/// that it broke.
pub(crate) fn blocked(code: u16) -> bool {
    matches!(code, 401 | 403 | 407 | 451)
}

/// The agent every engine request goes through: the system's certificate store (so a network
/// that inspects TLS with its own root, as companies' do, still works) and the proxy the
/// environment names.
pub(crate) fn agent(timeout: Duration) -> ureq::Agent {
    net_trust::agent_config()
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(BODY_LIMIT))
        .build()
        .new_agent()
}

pub(crate) struct Body<'a> {
    partial: File,
    /// Bytes of the partial file not yet handed on.
    replay: u64,
    source: Option<Source>,
    /// The pace when the source's last bytes came (before any, at the first read): a page that
    /// waits now and didn't then joined a download that may have gone silent before it came.
    heard_at: Option<Pace>,
    hasher: Sha256,
    /// Bytes handed on so far (the replayed ones too).
    have: u64,
    /// Bytes the partial file held when it opened: progress never shows less.
    resumed_at: u64,
    total: u64,
    progress: &'a mut dyn FnMut(u64, u64),
    pace: &'a mut dyn FnMut() -> Pace,
    window: (Instant, u64, Pace),
    last_notify: u64,
    cut: Option<Cut>,
}

impl<'a> Body<'a> {
    /// Opens `url` (`total` bytes, as the index says) after what `partial` already holds.
    /// `progress` hears `(downloaded, total)` every 256 KB of new bytes.
    pub(crate) fn open(
        url: &str,
        partial: &Path,
        total: u64,
        progress: &'a mut dyn FnMut(u64, u64),
        pace: &'a mut dyn FnMut() -> Pace,
        low: &Arc<AtomicBool>,
    ) -> Result<Self, Fault> {
        if let Some(dir) = partial.parent() {
            std::fs::create_dir_all(dir).map_err(|e| err_write(&e))?;
        }
        let mut file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(partial).map_err(|e| err_write(&e))?;
        // Locked while open, so two apps sharing the engine folder never write it at once.
        file.try_lock().map_err(|_| Fault::Busy)?;
        let mut have = file.metadata().map_or(0, |m| m.len());
        if have > total {
            file.set_len(0).map_err(|e| err_write(&e))?;
            have = 0;
        }
        let source = if have == total {
            None
        } else {
            match open_source(url, have, low)? {
                Opened::From(reader) => Some(reader),
                Opened::Whole(reader) => {
                    file.set_len(0).map_err(|e| err_write(&e))?;
                    have = 0;
                    Some(reader)
                }
            }
        };
        file.seek(SeekFrom::Start(0)).map_err(|e| err_write(&e))?;
        progress(have, total);
        Ok(Self {
            partial: file,
            replay: have,
            source,
            heard_at: None,
            hasher: Sha256::new(),
            have: 0,
            resumed_at: have,
            total,
            progress,
            pace,
            window: (Instant::now(), 0, Pace::Full),
            last_notify: have,
            cut: None,
        })
    }

    /// Bytes an earlier try left in the partial file.
    pub(crate) fn resumed_at(&self) -> u64 {
        self.resumed_at
    }

    /// Why the stream stopped early, once a read failed.
    pub(crate) fn cut(&self) -> Option<&Cut> {
        self.cut.as_ref()
    }

    /// Reads whatever the consumer left (an archive's zero padding), checks the size, and
    /// returns the SHA-256 (lowercase hex).
    pub(crate) fn finish(mut self) -> Result<String, Cut> {
        let mut rest = [0u8; 64 * 1024];
        loop {
            match self.read(&mut rest) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => return Err(self.cut.take().unwrap_or_else(|| Cut::Failed(self.interrupted()))),
            }
        }
        self.partial.flush().ok();
        if self.have != self.total {
            return Err(Cut::Rejected(Fault::Corrupt));
        }
        (self.progress)(self.have, self.total);
        Ok(format!("{:x}", self.hasher.finalize()))
    }

    /// The connection ended part-way: what the partial file keeps for the next try.
    fn interrupted(&self) -> Fault {
        Fault::Interrupted { have: self.have.max(self.resumed_at), total: self.total }
    }

    fn fail(&mut self, cut: Cut) -> std::io::Error {
        self.partial.flush().ok();
        self.cut = Some(cut);
        std::io::Error::other("engine download cut short")
    }

    fn read_source(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let limit = match (self.pace)() {
            Pace::Stop => return Err(self.fail(Cut::Stopped)),
            p => p,
        };
        if limit != self.window.2 {
            self.window = (Instant::now(), 0, limit);
        }
        if let Pace::Limit(rate) = limit {
            // Hold the window's average at `rate`: sleep until its bytes are due.
            let due = Duration::from_secs_f64(self.window.1 as f64 / rate.max(1) as f64);
            if let Some(wait) = due.checked_sub(self.window.0.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        let chunk = match limit {
            Pace::Limit(rate) => buf.len().min((rate / 8).max(4096) as usize),
            _ => buf.len(),
        };
        let heard_at = *self.heard_at.get_or_insert(limit);
        let n = loop {
            let next = match self.source.as_mut() {
                None => return Ok(0),
                Some(Source::File(file)) => file.read(&mut buf[..chunk]).map_or(Next::Failed, Next::Bytes),
                Some(Source::Network(watched)) => watched.next(&mut buf[..chunk], STALL_SLICE),
            };
            match next {
                Next::Bytes(n) => break n,
                Next::Failed => {
                    let cut = Cut::Failed(self.interrupted());
                    return Err(self.fail(cut));
                }
                // Nothing came for a slice: stop if the pace says so, and give the connection up
                // once it has been silent too long.
                Next::Silent(silent) => {
                    let now = (self.pace)();
                    if now == Pace::Stop {
                        return Err(self.fail(Cut::Stopped));
                    }
                    let joined = now == Pace::Full && heard_at != Pace::Full;
                    if silent >= STALL || (joined && silent >= JOIN_STALL) {
                        return Err(self.fail(Cut::Stalled));
                    }
                }
            }
        };
        if n == 0 {
            return Ok(0);
        }
        self.heard_at = Some(limit);
        if let Err(e) = self.partial.write_all(&buf[..n]) {
            let msg = err_write(&e);
            return Err(self.fail(Cut::Failed(msg)));
        }
        self.window.1 += n as u64;
        Ok(n)
    }
}

impl Read for Body<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cut.is_some() || buf.is_empty() {
            return Err(std::io::Error::other("engine download cut short"));
        }
        let n = if self.replay > 0 {
            let want = buf.len().min(self.replay.min(usize::MAX as u64) as usize);
            match self.partial.read(&mut buf[..want]) {
                Ok(0) => return Err(self.fail(Cut::Rejected(Fault::Corrupt))),
                Err(e) => return Err(self.fail(Cut::Failed(err_write(&e)))),
                Ok(n) => {
                    self.replay -= n as u64;
                    n
                }
            }
        } else {
            self.read_source(buf)?
        };
        if self.have + n as u64 > self.total {
            return Err(self.fail(Cut::Rejected(Fault::Corrupt)));
        }
        self.hasher.update(&buf[..n]);
        self.have += n as u64;
        if self.have >= self.last_notify + 256 * 1024 {
            self.last_notify = self.have;
            (self.progress)(self.have.max(self.resumed_at), self.total);
        }
        Ok(n)
    }
}

/// Where the archive's bytes come from.
enum Source {
    /// A `file:` source (a development build's package on this disk).
    File(File),
    /// The network, read on a thread of its own with a clock on its silence.
    Network(Watched),
}

/// What a wait for the source's next bytes brought.
enum Next {
    /// This many bytes are in the buffer (0: the source ended).
    Bytes(usize),
    /// Nothing for the slice; the source has been silent this long.
    Silent(Duration),
    /// The connection failed, or its reader is gone.
    Failed,
}

/// The network's bytes, read by a thread of its own and handed over a short queue, so the
/// stream can wait for them with a clock (and a slow consumer still holds the sender back, as a
/// direct read would). Dropped while its thread waits on a silent connection, the thread ends at
/// that connection's error or at `BODY_LIMIT`, whichever comes first, and its bytes go nowhere.
struct Watched {
    chunks: Receiver<std::io::Result<Vec<u8>>>,
    /// Bytes that came and aren't handed on yet.
    held: Vec<u8>,
    at: usize,
    /// How long nothing has come, in waits that timed out since the last bytes.
    silent: Duration,
    /// The source ended: every read after it says so again (the reader's thread is gone).
    ended: bool,
}

impl Watched {
    /// Reads `reader` on a thread that keeps the download job's priority (`low`).
    fn spawn(mut reader: Box<dyn Read + Send>, low: &Arc<AtomicBool>) -> std::io::Result<Self> {
        let (tx, chunks) = sync_channel(4);
        let low = low.clone();
        std::thread::Builder::new().name("opencode-browser-download".into()).spawn(move || {
            let mut background = false;
            loop {
                let want = low.load(Ordering::Relaxed);
                if want != background {
                    background = want;
                    crate::background::set_background_priority(want);
                }
                let mut chunk = vec![0u8; 64 * 1024];
                let more = match reader.read(&mut chunk) {
                    Ok(n) => {
                        chunk.truncate(n);
                        tx.send(Ok(chunk)).is_ok() && n > 0
                    }
                    Err(err) => {
                        tx.send(Err(err)).ok();
                        false
                    }
                };
                if !more {
                    break;
                }
            }
        })?;
        Ok(Self { chunks, held: Vec::new(), at: 0, silent: Duration::ZERO, ended: false })
    }

    /// The next bytes into `buf`, waiting at most `wait` for them.
    fn next(&mut self, buf: &mut [u8], wait: Duration) -> Next {
        if self.ended {
            return Next::Bytes(0);
        }
        if self.at == self.held.len() {
            match self.chunks.recv_timeout(wait) {
                Ok(Ok(chunk)) => {
                    self.ended = chunk.is_empty();
                    self.held = chunk;
                    self.at = 0;
                    self.silent = Duration::ZERO;
                }
                Ok(Err(_)) | Err(RecvTimeoutError::Disconnected) => return Next::Failed,
                Err(RecvTimeoutError::Timeout) => {
                    self.silent += wait;
                    return Next::Silent(self.silent);
                }
            }
        }
        let n = buf.len().min(self.held.len() - self.at);
        buf[..n].copy_from_slice(&self.held[self.at..self.at + n]);
        self.at += n;
        Next::Bytes(n)
    }
}

enum Opened {
    /// The source answers from the requested offset.
    From(Source),
    /// The source sends the whole archive again.
    Whole(Source),
}

fn open_source(url: &str, from: u64, low: &Arc<AtomicBool>) -> Result<Opened, Fault> {
    if let Some(path) = source::file_path(url) {
        let mut file = File::open(path).map_err(|_| Fault::NotPublished)?;
        file.seek(SeekFrom::Start(from)).map_err(|e| err_write(&e))?;
        return Ok(Opened::From(Source::File(file)));
    }
    let agent = agent(Duration::from_secs(30));
    let mut req = agent.get(url);
    if from > 0 {
        req = req.header("Range", format!("bytes={from}-"));
    }
    match req.call() {
        Ok(resp) => {
            let resumed = from > 0 && resp.status().as_u16() == 206;
            // `bytes <start>-<end>/<total>`: the rest must start where the partial file ends.
            let start = resp.headers().get("content-range").and_then(|v| v.to_str().ok()).and_then(|r| r.strip_prefix("bytes ")?.split('-').next()?.parse::<u64>().ok());
            if resumed && start != Some(from) {
                return Err(Fault::Unexpected { host: Fault::host(url) });
            }
            let reader = Watched::spawn(Box::new(resp.into_body().into_reader()), low).map_err(|e| Fault::Write(format!("no thread to download on: {e}")))?;
            let reader = Source::Network(reader);
            Ok(if resumed || from == 0 { Opened::From(reader) } else { Opened::Whole(reader) })
        }
        // The server lost track of the partial file: start over.
        Err(ureq::Error::StatusCode(416)) if from > 0 => open_source(url, 0, low).map(|o| match o {
            Opened::From(r) | Opened::Whole(r) => Opened::Whole(r),
        }),
        Err(e) => Err(request_fault(url, e)),
    }
}

/// A request's failure: refused (`blocked`), an error status, or why the server was out of reach.
pub(crate) fn request_fault(url: &str, err: ureq::Error) -> Fault {
    // The transport's own words, for the log a user can send.
    eprintln!("browser engine: {url}: {err}");
    let host = Fault::host(url);
    let why = match err {
        ureq::Error::StatusCode(status) if blocked(status) => return Fault::Blocked { host, status },
        ureq::Error::StatusCode(status) => return Fault::Server { host, status },
        ureq::Error::Io(e) => return io_fault(url, &e),
        ureq::Error::HostNotFound => Net::NotFound,
        ureq::Error::ConnectionFailed => Net::Refused,
        ureq::Error::Timeout(_) => Net::TimedOut,
        ureq::Error::Tls(_) | ureq::Error::Pem(_) | ureq::Error::Rustls(_) => Net::Secure,
        ureq::Error::ConnectProxyFailed(_) | ureq::Error::InvalidProxyUrl => Net::Proxy,
        _ => Net::Other,
    };
    Fault::Unreachable { host, why }
}

/// A socket's failure before any of the archive came.
pub(crate) fn io_fault(url: &str, err: &std::io::Error) -> Fault {
    use std::io::ErrorKind;
    // WSAHOST_NOT_FOUND, WSATRY_AGAIN, WSANO_DATA: the name didn't resolve.
    if cfg!(windows) && matches!(err.raw_os_error(), Some(11001 | 11002 | 11004)) {
        return Fault::Unreachable { host: Fault::host(url), why: Net::NotFound };
    }
    let why = match err.kind() {
        ErrorKind::ConnectionRefused => Net::Refused,
        ErrorKind::TimedOut => Net::TimedOut,
        ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable | ErrorKind::NetworkDown => Net::NotFound,
        _ => Net::Other,
    };
    Fault::Unreachable { host: Fault::host(url), why }
}
