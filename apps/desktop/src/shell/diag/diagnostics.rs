//! Always-on frame diagnostics for live windowed runs, through Rune's `rune-perf`: one writer
//! thread keeps `perf.json` (a two-second snapshot), capped logs and incident files in
//! `<data dir>/rune-opencode/perf/`. The UI thread only sends a fixed-size record per frame
//! report; it never formats, writes or asks for a frame, so an idle window still draws nothing.
//!
//! Timing is Rune's own (`FrameEncoded::update_ms`, `encode_ms`), so no compositor is attached
//! and the window keeps skipping unchanged timer frames. Input latency runs from the newest input
//! the window dispatched since the last drawn frame (`rune::last_input`) to this frame's encode:
//! a lower bound when several inputs land between frames.

#[cfg(target_arch = "wasm32")]
mod web {
    use rune::{FrameEncoded, Runtime};

    use crate::App;

    /// Frame diagnostics are native-only (`rune-perf` writes to local log files); on `wasm32`
    /// this handle is a zero-sized no-op.
    #[derive(Clone, Default)]
    pub struct Diagnostics;

    impl Diagnostics {
        pub fn drawn(&self, _: &Runtime<App>) {}
        pub fn encoded(&self, _: &FrameEncoded) {}
    }
}

#[cfg(target_arch = "wasm32")]
pub use web::Diagnostics;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::{cell::RefCell, rc::Rc};

    use rune::time::Instant;
    use rune::{FrameEncoded, FrameOutcome, Runtime};
    use rune_perf::{Config, Frame, Outcome, Recorder};

    use crate::App;

    /// Shared by the window's encoded and drawn hooks, and by `main` to shut the writer down.
    #[derive(Clone, Default)]
    pub struct Diagnostics(Rc<RefCell<State>>);

    #[derive(Default)]
    enum State {
        #[default]
        Unstarted,
        /// Recording could not start (unavailable or failed), or has been shut down.
        Inactive,
        Recording {
            recorder: Recorder,
            last_drawn: Option<Instant>,
        },
    }

    enum Event {
        Drawn { live: bool, headless: bool },
        Started(Option<Recorder>),
        Encoded {
            seq: u64,
            outcome: FrameOutcome,
            at: Instant,
            input: Option<Instant>,
            update_ms: Option<f64>,
            encode_ms: Option<f64>,
            allocated_bytes: Option<u64>,
        },
        Shutdown,
    }

    enum Effect {
        Start,
        Record(Frame),
        Shutdown(Recorder),
    }

    fn step(state: &mut State, event: Event) -> Option<Effect> {
        match (state, event) {
            (State::Unstarted, Event::Drawn { live: true, headless: false }) => Some(Effect::Start),
            (state @ State::Unstarted, Event::Started(recorder)) => {
                *state = match recorder {
                    Some(recorder) => State::Recording { recorder, last_drawn: None },
                    None => State::Inactive,
                };
                None
            }
            (
                State::Recording { last_drawn, .. },
                Event::Encoded { seq, outcome, at, input, update_ms, encode_ms, allocated_bytes },
            ) => {
                let outcome = match outcome {
                    FrameOutcome::Drawn => Outcome::Drawn,
                    FrameOutcome::Unchanged => Outcome::Unchanged,
                    _ => Outcome::Skipped,
                };
                let mut input_ms = None;
                if outcome == Outcome::Drawn {
                    input_ms = input
                        .filter(|t| last_drawn.is_none_or(|d| *t > d))
                        .and_then(|t| at.checked_duration_since(t))
                        .map(|d| d.as_secs_f64() * 1000.0);
                    *last_drawn = Some(at);
                }
                Some(Effect::Record(
                    Frame::new(0, seq, outcome)
                        .update_ms(update_ms)
                        .render_ms(encode_ms)
                        .input_to_frame_ms(input_ms)
                        .allocated_bytes(allocated_bytes),
                ))
            }
            (state, Event::Shutdown) => match std::mem::replace(state, State::Inactive) {
                State::Recording { recorder, .. } => Some(Effect::Shutdown(recorder)),
                State::Unstarted | State::Inactive => None,
            },
            _ => None,
        }
    }

    impl Diagnostics {
        /// `App::on_frame_drawn`: starts recording on the first drawn frame of a live session, never
        /// for a fixture, a test or a headless run.
        pub fn drawn(&self, rt: &Runtime<App>) {
            let should_start = {
                let mut s = self.0.borrow_mut();
                matches!(step(&mut s, Event::Drawn { live: rt.state.live(), headless: crate::headless() }), Some(Effect::Start))
            };
            if should_start {
                let recorder = start();
                step(&mut self.0.borrow_mut(), Event::Started(recorder));
            }
        }

        /// `App::on_frame_encoded`.
        pub fn encoded(&self, f: &FrameEncoded) {
            self.record(f.seq(), f.outcome(), f.at(), f.update_ms(), f.encode_ms());
        }

        fn record(&self, seq: u64, outcome: FrameOutcome, at: Instant, update_ms: Option<f64>, encode_ms: Option<f64>) {
            let mut s = self.0.borrow_mut();
            if !matches!(*s, State::Recording { .. }) {
                return;
            }
            let event = Event::Encoded {
                seq,
                outcome,
                at,
                input: rune::last_input(),
                update_ms,
                encode_ms,
                allocated_bytes: rune_perf::take_allocated_bytes(),
            };
            if let Some(Effect::Record(frame)) = step(&mut s, event)
                && let State::Recording { recorder, .. } = &*s
            {
                recorder.record(frame);
            }
        }

        /// After `run`: drains what's queued. The window is gone, so nothing waits on it.
        pub fn shutdown(&self) {
            let effect = step(&mut self.0.borrow_mut(), Event::Shutdown);
            if let Some(Effect::Shutdown(r)) = effect {
                r.shutdown().ok();
            }
        }
    }

    fn start() -> Option<Recorder> {
        let dir = crate::dirs::logs()?.join("perf");
        let config = Config::new(dir, "Shuvcode (Rune)").build(env!("CARGO_PKG_VERSION"));
        match Recorder::start(config) {
            Ok(r) => {
                // The first call only sets this thread's baseline.
                rune_perf::take_allocated_bytes();
                Some(r)
            }
            Err(e) => {
                eprintln!("opencode-rune: frame diagnostics unavailable: {e}");
                None
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::Diagnostics;
