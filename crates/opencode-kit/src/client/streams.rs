//! Every event stream this process reads, so a network change or a wake from sleep can check
//! them all at once ([`verify`]). After either, no stream that looks up can be trusted: a
//! connection that died while the machine slept, or whose route went with the old network, just
//! goes quiet, and its read would wait out the idle limit (25 s) before anyone knew. So each is
//! suspect: its server is asked once, on a fresh connection with a 1 s bound, whether it answers,
//! and the stream is replaced unless it has spoken since. A server that doesn't answer is said to
//! be unreachable at once (`Incoming::Unanswered`, typed), and its new stream parks on the reach
//! watcher. Nothing polls: it runs when the OS says the network changed or the machine woke.
//!
//! A replaced stream's thread can't be stopped mid-read, so it's left to end on its own: the
//! stream it reads no longer speaks for its server ([`Stream::live`]), sends nothing more, and
//! ends at its next line or its idle limit.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rune::Waker;
use rune::time::Instant;

use super::{Incoming, Server, http};
use crate::wire_error::{WireError, transport};

/// How long a verifying request may take: past it, the server counts as not answering.
const CHECK: Duration = Duration::from_secs(1);

/// Ownership and wire activity of a tracked event stream:
/// - `Silent { epoch }`: no line has been read yet; thread `epoch` speaks for the stream.
/// - `Active { epoch, last_heard }`: at least one line (including heartbeats) arrived at
///   `last_heard`; thread `epoch` speaks for the stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Track {
    Silent { epoch: u64 },
    Active { epoch: u64, last_heard: Instant },
}

enum TrackEvent {
    LineRead { at: Instant },
    VerifyChecked { started_at: Instant },
}

enum TrackEffect {
    Keep,
    Replace { epoch: u64 },
}

impl Track {
    fn epoch(self) -> u64 {
        match self {
            Self::Silent { epoch } | Self::Active { epoch, .. } => epoch,
        }
    }

    fn step(&mut self, event: TrackEvent) -> TrackEffect {
        match (*self, event) {
            (Self::Silent { epoch } | Self::Active { epoch, .. }, TrackEvent::LineRead { at }) => {
                *self = Self::Active { epoch, last_heard: at };
                TrackEffect::Keep
            }
            (Self::Active { last_heard, .. }, TrackEvent::VerifyChecked { started_at }) if last_heard > started_at => {
                TrackEffect::Keep
            }
            (Self::Silent { epoch }, TrackEvent::VerifyChecked { .. }) => {
                let next = epoch + 1;
                *self = Self::Silent { epoch: next };
                TrackEffect::Replace { epoch: next }
            }
            (Self::Active { epoch, last_heard }, TrackEvent::VerifyChecked { .. }) => {
                let next = epoch + 1;
                *self = Self::Active { epoch: next, last_heard };
                TrackEffect::Replace { epoch: next }
            }
        }
    }
}

/// One server's event stream for one inbox (a window, or a hub of several).
pub(super) struct Stream {
    pub server: Server,
    pub idle: Duration,
    pub inbox: Sender<Incoming>,
    pub wake: Waker,
    /// Owning epoch and last wire activity, stepped through [`Track::step`].
    track: Mutex<Track>,
}

static STREAMS: Mutex<Vec<Weak<Stream>>> = Mutex::new(Vec::new());

impl Stream {
    /// A stream to read, known to [`verify`] for as long as a thread reads it.
    pub fn new(server: Server, idle: Duration, inbox: Sender<Incoming>, wake: Waker) -> Arc<Stream> {
        let s = Arc::new(Stream { server, idle, inbox, wake, track: Mutex::new(Track::Silent { epoch: 0 }) });
        let mut all = STREAMS.lock().unwrap_or_else(|e| e.into_inner());
        all.retain(|w| w.strong_count() > 0);
        all.push(Arc::downgrade(&s));
        s
    }

    pub fn epoch(&self) -> u64 {
        self.track.lock().unwrap_or_else(|e| e.into_inner()).epoch()
    }

    /// Whether the thread reading as `epoch` still speaks for the stream.
    pub fn live(&self, epoch: u64) -> bool {
        self.epoch() == epoch
    }

    pub fn heard(&self) {
        self.track.lock().unwrap_or_else(|e| e.into_inner()).step(TrackEvent::LineRead { at: Instant::now() });
    }
}

/// After a network change or a wake: each stream's server is asked once whether it answers, and
/// each stream that hasn't spoken since is replaced by a fresh one (one whose server didn't
/// answer says so first). Returns at once; the checks run on their own threads.
pub fn verify() {
    let at = Instant::now();
    let all: Vec<Arc<Stream>> = STREAMS.lock().unwrap_or_else(|e| e.into_inner()).iter().filter_map(Weak::upgrade).collect();
    for s in all {
        std::thread::Builder::new().name("opencode-verify".into()).spawn(move || check(&s, at)).ok();
    }
}

fn check(s: &Arc<Stream>, at: Instant) {
    let answer = answers(&s.server);
    let TrackEffect::Replace { epoch } = s.track.lock().unwrap_or_else(|e| e.into_inner()).step(TrackEvent::VerifyChecked { started_at: at }) else {
        return;
    };
    if let Err(e) = answer {
        // Said before the new stream's own try, which may take its time to fail.
        if s.inbox.send(Incoming::Unanswered(e)).is_err() {
            return;
        }
        (s.wake)();
    }
    spawn_replacement(s, epoch);
}

/// Whether `server` answers at all, on a connection of its own (a pooled one may be the dead
/// one): any HTTP answer counts, a refusal or silence past [`CHECK`] doesn't.
fn answers(server: &Server) -> Result<(), WireError> {
    let config = net_trust::agent_config_for(&server.base).timeout_global(Some(CHECK)).http_status_as_error(false).max_redirects(0).max_idle_connections(0);
    let agent: ureq::Agent = if http::loopback(&server.base) { config.proxy(None) } else { config }.build().into();
    let mut req = agent.get(format!("{}/api/info", server.base));
    if let Some(a) = &server.auth {
        req = req.header("Authorization", a);
    }
    req.call().map(drop).map_err(|e| WireError::new("GET", "/api/info", transport(e)))
}

/// A new thread speaks for the stream from now; the old one ends quietly.
fn spawn_replacement(s: &Arc<Stream>, epoch: u64) {
    let s = s.clone();
    std::thread::Builder::new().name("opencode-events".into()).spawn(move || super::worker::run_stream(&s, epoch, true)).ok();
}
