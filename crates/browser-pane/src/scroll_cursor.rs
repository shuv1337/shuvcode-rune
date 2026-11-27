//! Explicit state machine deferring cursor-only `TabUpdate` commits while a tab is actively
//! wheel-scrolling. Scrolling over link-heavy pages causes Chromium to emit `on_cursor_change`
//! dozens of times per second under a stationary pointer; applying each immediately forces a
//! full-window UI rebuild on the desktop UI thread. Deferring cursor-only updates until wheel
//! scrolling settles keeps scrolling at display refresh rate while applying the exact cursor as
//! soon as scroll stops.

use std::time::Duration;

use rune::time::Instant;

use crate::protocol::CursorKind;

/// How long after the last wheel tick cursor-only `TabUpdate` commits wait before applying.
const SCROLL_CURSOR_SETTLE: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum ScrollCursor {
    #[default]
    Idle,
    /// Wheel scrolling is active until `until`; no cursor change has been deferred yet.
    Scrolling { until: Instant },
    /// Wheel scrolling is active until `until`, holding `pending` to apply once scrolling settles.
    Deferred { until: Instant, pending: CursorKind },
}

pub(crate) enum ScrollCursorEvent {
    Wheel { now: Instant },
    CursorChanged { cursor: CursorKind, now: Instant },
    /// Another tab field already requires a view commit; fold any pending cursor into it.
    CoalescedCommit,
    Tick { now: Instant },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScrollCursorEffect {
    None,
    Apply(CursorKind),
}

impl ScrollCursor {
    pub(crate) fn step(&mut self, event: ScrollCursorEvent) -> ScrollCursorEffect {
        match (*self, event) {
            (Self::Idle | Self::Scrolling { .. }, ScrollCursorEvent::Wheel { now }) => {
                *self = Self::Scrolling { until: now + SCROLL_CURSOR_SETTLE };
                ScrollCursorEffect::None
            }
            (Self::Deferred { pending, .. }, ScrollCursorEvent::Wheel { now }) => {
                *self = Self::Deferred { until: now + SCROLL_CURSOR_SETTLE, pending };
                ScrollCursorEffect::None
            }
            (Self::Idle, ScrollCursorEvent::CursorChanged { cursor, .. }) => {
                ScrollCursorEffect::Apply(cursor)
            }
            (
                Self::Scrolling { until } | Self::Deferred { until, .. },
                ScrollCursorEvent::CursorChanged { cursor, now },
            ) => {
                if now >= until {
                    *self = Self::Idle;
                    ScrollCursorEffect::Apply(cursor)
                } else {
                    *self = Self::Deferred { until, pending: cursor };
                    ScrollCursorEffect::None
                }
            }
            (Self::Scrolling { until } | Self::Deferred { until, .. }, ScrollCursorEvent::CoalescedCommit) => {
                *self = Self::Scrolling { until };
                ScrollCursorEffect::None
            }
            (Self::Idle, ScrollCursorEvent::CoalescedCommit | ScrollCursorEvent::Tick { .. }) => {
                ScrollCursorEffect::None
            }
            (Self::Scrolling { until }, ScrollCursorEvent::Tick { now }) => {
                if now >= until {
                    *self = Self::Idle;
                }
                ScrollCursorEffect::None
            }
            (Self::Deferred { until, pending }, ScrollCursorEvent::Tick { now }) => {
                if now >= until {
                    *self = Self::Idle;
                    ScrollCursorEffect::Apply(pending)
                } else {
                    ScrollCursorEffect::None
                }
            }
        }
    }

    /// Next deadline at which a deferred cursor must be applied (only while `Deferred`).
    pub(crate) fn wake_at(&self) -> Option<Instant> {
        match *self {
            Self::Deferred { until, .. } => Some(until),
            Self::Idle | Self::Scrolling { .. } => None,
        }
    }
}
