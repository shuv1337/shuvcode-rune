//! Whether the open session's folder exists, as its server says: OpenCode Desktop's
//! `packages/app/src/workspaces/location-machine.ts`. The app never looks at the disk itself (a
//! remote server's folders aren't this computer's, and a dead network drive would hold the
//! look): it asks `GET /api/location?location[directory]=…` on a worker thread
//! (`App::job`), and only the server's typed `LocationNotFoundError` says the folder is gone. Any
//! other failure is tried again twice and then leaves the composer as it was.
//!
//! The folder is asked about when the session's page shows and each time the server's stream
//! comes back; there is no timer and no file watch, as in Desktop.
//!
//! Beyond Desktop: a folder its server last said was missing (`known_missing`) shows as missing
//! at once when a session there is opened again or the app starts, while the server is asked
//! again, as Desktop already does across a reconnect. So the card doesn't give way to the
//! composer for one request's time, and a launch draws it in its first frame.

use rune::machine::{Attempt, Effects, Seq};

/// `LOCATION_SYNC_RETRIES`.
pub const RETRIES: u8 = 2;
/// `LOCATION_SYNC_RETRY_DELAY`, doubled each retry.
pub const RETRY_DELAY: f64 = 0.5;
/// `LOCATION_SYNC_TIMEOUT`.
pub const TIMEOUT: f64 = 30.0;

/// A folder on a server (`LocationRef`, with the server it is on).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub server: String,
    pub directory: String,
}

/// `LocationSyncState`. `missing` on an attempt: the folder was missing before, so the card
/// stays while the server is asked again.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Idle,
    Syncing { attempt: Attempt, retries: u8, missing: bool, deadline: f64 },
    WaitingRetry { attempt: Attempt, retries: u8, missing: bool, due: f64 },
    Synced,
    Missing,
    Failed,
}

/// The open session's folder and where asking about it stands.
#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub location: Location,
    issued: Seq,
    pub state: State,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// `target.updated`: the page's folder, and whether its server's stream is up.
    Target { location: Location, connected: bool, known_missing: bool, now: f64 },
    /// The server resolved the folder, so it exists (`info.resolved`, `sync.succeeded`).
    Found { attempt: Attempt },
    /// The server's `LocationNotFoundError` (`sync.missing`).
    Missing { attempt: Attempt },
    /// Any other failure (`sync.failed`).
    Failed { attempt: Attempt, now: f64 },
    /// The clock: an attempt's timeout (`sync.timed-out`) or a retry coming due (`retry.due`).
    Tick { now: f64 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Ask the server about `location`; the answer carries `attempt`.
    Ask { attempt: Attempt, location: Location },
    WakeAt(f64),
}

impl Check {
    /// `createLocationSyncState`, for a folder that was or wasn't missing when last asked.
    pub fn new(location: Location, known_missing: bool) -> Check {
        Check { location, issued: Seq::default(), state: if known_missing { State::Missing } else { State::Idle } }
    }

    /// `missingLocation`: whether `current` is this folder and the server says it is gone.
    pub fn missing(&self, current: &Location) -> bool {
        self.location == *current
            && matches!(self.state, State::Missing | State::Syncing { missing: true, .. } | State::WaitingRetry { missing: true, .. })
    }

    /// Whether a timer has run out at `now` (the poll then feeds it the clock).
    pub fn due(&self, now: f64) -> bool {
        match self.state {
            State::Syncing { deadline: at, .. } | State::WaitingRetry { due: at, .. } => now >= at,
            _ => false,
        }
    }
}

/// `transitionLocationSync`.
pub fn step(sync: &mut Check, event: Event) -> Effects<Effect> {
    match (sync.state, event) {
        (_, Event::Target { location, connected, known_missing, now }) => target(sync, location, connected, known_missing, now),
        (State::Syncing { attempt, .. }, Event::Found { attempt: of }) if attempt == of => {
            sync.state = State::Synced;
            Effects::none()
        }
        // A typed not-found answer is final; retrying cannot succeed until the folder changes.
        (State::Syncing { attempt, .. }, Event::Missing { attempt: of }) if attempt == of => {
            sync.state = State::Missing;
            Effects::none()
        }
        // Generic failures and timeouts do not prove the folder is missing.
        (State::Syncing { attempt, retries, missing, .. }, Event::Failed { attempt: of, now }) if attempt == of => retry(sync, retries, missing, now),
        (State::Syncing { retries, missing, deadline, .. }, Event::Tick { now }) if now >= deadline => retry(sync, retries, missing, now),
        (State::WaitingRetry { attempt, retries, missing, due }, Event::Tick { now }) if now >= due => {
            sync.state = State::Syncing { attempt, retries, missing, deadline: now + TIMEOUT };
            Effects::two(Effect::Ask { attempt, location: sync.location.clone() }, Effect::WakeAt(now + TIMEOUT))
        }
        _ => Effects::none(),
    }
}

fn target(sync: &mut Check, location: Location, connected: bool, known_missing: bool, now: f64) -> Effects<Effect> {
    // Another folder starts over: nothing about the previous one carries over.
    if sync.location != location {
        sync.location = location;
        if !connected {
            sync.state = if known_missing { State::Missing } else { State::Idle };
            return Effects::none();
        }
        return ask(sync, known_missing, now);
    }
    if connected {
        return match sync.state {
            State::Idle => ask(sync, false, now),
            // Reconnecting checks the folder again without hiding the card meanwhile.
            State::Missing => ask(sync, true, now),
            _ => Effects::none(),
        };
    }
    sync.state = match sync.state {
        State::Idle | State::Missing => return Effects::none(),
        // Offline proves nothing about the folder, so a missing one stays missing.
        State::Syncing { missing: true, .. } | State::WaitingRetry { missing: true, .. } => State::Missing,
        _ => State::Idle,
    };
    Effects::none()
}

fn ask(sync: &mut Check, missing: bool, now: f64) -> Effects<Effect> {
    let attempt = sync.issued.next();
    sync.state = State::Syncing { attempt, retries: 0, missing, deadline: now + TIMEOUT };
    Effects::two(Effect::Ask { attempt, location: sync.location.clone() }, Effect::WakeAt(now + TIMEOUT))
}

fn retry(sync: &mut Check, retries: u8, missing: bool, now: f64) -> Effects<Effect> {
    if retries < RETRIES {
        let due = now + RETRY_DELAY * f64::from(1u32 << retries);
        sync.state = State::WaitingRetry { attempt: sync.issued.next(), retries: retries + 1, missing, due };
        return Effects::one(Effect::WakeAt(due));
    }
    sync.state = if missing { State::Missing } else { State::Failed };
    Effects::none()
}
