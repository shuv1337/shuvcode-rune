//! Recovering a session whose folder is gone: OpenCode Desktop's
//! `packages/app/src/session/composer/location-missing-machine.ts`. `Picking`: the directory
//! picker is open. `Moving`: the worktree or move request runs. `Moved`: the server moved the
//! session and the card waits for the session's new folder to take it away. `restore_focus`: a
//! cancelled or failed move returns focus to Choose directory once the dialog over it closes.

use rune::machine::{Attempt, Effects, Seq};

/// `LOCATION_MOVE_SETTLE_TIMEOUT_MS`: a move the server accepted waits this long for the
/// session's new folder to take the card away.
pub const MOVE_SETTLE_TIMEOUT: f64 = 5.0;

/// `LocationMissingSource`: which button the running move came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Worktree,
    Directory,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    Idle { restore_focus: bool },
    Picking { attempt: Attempt },
    Moving { attempt: Attempt, source: Source },
    Moved { attempt: Attempt, source: Source, due: f64 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Card {
    issued: Seq,
    pub state: State,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    ChooseRequested,
    PickerSettled { attempt: Attempt, directory: Option<String>, dialog_open: bool },
    /// A row of the worktree dialog. Without `directory`, the move first creates a worktree
    /// from the project's checkout.
    MoveWorktree { directory: Option<String> },
    MoveSucceeded { attempt: Attempt, now: f64 },
    MoveFailed { attempt: Attempt, error: String, dialog_open: bool },
    /// `dialog.changed`: whether a dialog is open over the card.
    DialogChanged { open: bool },
    /// The clock: the moved session's folder not arriving (`move.settle-timed-out`).
    Tick { now: f64 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    PickerOpen { attempt: Attempt },
    /// Without `directory`, the move first creates a worktree from the project's checkout.
    MoveRun { attempt: Attempt, directory: Option<String> },
    WakeAt(f64),
    FocusChoose,
    ToastMoveFailed { error: String },
}

/// `createLocationMissingState`.
impl Default for Card {
    fn default() -> Card {
        Card { issued: Seq::default(), state: State::Idle { restore_focus: false } }
    }
}

impl Card {
    /// One move at a time: the desktop's native picker leaves the window usable while it is open.
    pub fn disabled(&self) -> bool {
        !matches!(self.state, State::Idle { .. })
    }

    /// The button that shows the running move.
    pub fn pending(&self) -> Option<Source> {
        match self.state {
            State::Moving { source, .. } | State::Moved { source, .. } => Some(source),
            _ => None,
        }
    }

    /// Whether a timer has run out at `now` (the poll then feeds it the clock).
    pub fn due(&self, now: f64) -> bool {
        matches!(self.state, State::Moved { due, .. } if now >= due)
    }
}

/// `transitionLocationMissing`.
pub fn step(card: &mut Card, event: Event) -> Effects<Effect> {
    match (card.state, event) {
        (State::Idle { .. }, Event::ChooseRequested) => {
            let attempt = card.issued.next();
            card.state = State::Picking { attempt };
            Effects::one(Effect::PickerOpen { attempt })
        }
        (State::Picking { attempt }, Event::PickerSettled { attempt: of, directory, dialog_open }) if attempt == of => match directory {
            Some(directory) => run(card, Source::Directory, Some(directory)),
            None => cancelled(card, dialog_open),
        },
        (State::Idle { .. }, Event::MoveWorktree { directory }) => run(card, Source::Worktree, directory),
        (State::Moving { attempt, source }, Event::MoveSucceeded { attempt: of, now }) if attempt == of => {
            let due = now + MOVE_SETTLE_TIMEOUT;
            card.state = State::Moved { attempt, source, due };
            Effects::one(Effect::WakeAt(due))
        }
        (State::Moving { attempt, .. }, Event::MoveFailed { attempt: of, error, dialog_open }) if attempt == of => {
            let mut effects = Effects::one(Effect::ToastMoveFailed { error });
            effects.extend(cancelled(card, dialog_open));
            effects
        }
        (State::Moved { due, .. }, Event::Tick { now }) if now >= due => {
            card.state = State::Idle { restore_focus: false };
            Effects::none()
        }
        (State::Idle { restore_focus: true }, Event::DialogChanged { open: false }) => {
            card.state = State::Idle { restore_focus: false };
            Effects::one(Effect::FocusChoose)
        }
        _ => Effects::none(),
    }
}

fn run(card: &mut Card, source: Source, directory: Option<String>) -> Effects<Effect> {
    let attempt = card.issued.next();
    card.state = State::Moving { attempt, source };
    Effects::one(Effect::MoveRun { attempt, directory })
}

/// Focus returns to Choose directory now, or once the dialog over it closes.
fn cancelled(card: &mut Card, dialog_open: bool) -> Effects<Effect> {
    card.state = State::Idle { restore_focus: dialog_open };
    if dialog_open { Effects::none() } else { Effects::one(Effect::FocusChoose) }
}
