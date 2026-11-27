//! Hex's live transcription streams (`/streams`, Hex's `docs/streams.md`): the app records the
//! microphone itself and pushes 16 kHz mono samples; Hex transcribes them on the model its
//! dictation has loaded and sends partial text while it listens, one segment per window (cut at
//! pauses), and the final text. Hex's own dictation always goes first, so a stream's text can lag
//! while the user dictates. One stream runs at a time, and Hex ends one that gets neither audio
//! nor a heartbeat for 10 s.

use std::io::{BufRead, BufReader, Read};
use std::time::Duration;

use serde_json::{Value, json};

use crate::{Hex, NOT_RUNNING};

/// The rate every stream's samples are in.
pub const STREAM_RATE: u32 = 16_000;

pub const STREAM_BUSY: &str = "Hex is already transcribing a stream";
pub const NO_MODEL: &str = "Hex has no dictation model loaded";
pub const NO_ENGINE: &str = "Hex's transcription engine isn't running";
/// Hex's code for audio it can't take yet: it has 120 s it hasn't transcribed. Push it again later.
pub const BACKLOG_FULL: &str = "stream-backlog-full";

/// Hex's largest audio request is 256 KiB, so longer pushes go in pieces.
const MAX_PUSH_SAMPLES: usize = 256 * 1024 / 4;

#[derive(Clone, Debug)]
pub struct Stream {
    pub id: u64,
    owner: String,
    /// The model and language Hex transcribes with: dictation's.
    pub model: String,
    pub language: String,
    /// Hex sends `Partial`s: its model streams. Otherwise only segments and the final.
    pub partials: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub segment: u64,
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Final {
    pub text: String,
    pub segments: Vec<Segment>,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    /// The window being heard: `stable` won't change, `tentative` may. Replaces the last partial.
    Partial {
        segment: u64,
        stable: String,
        tentative: String,
    },
    /// A finished window.
    Segment(Segment),
    /// Every segment; the stream has ended.
    Final(Final),
    /// Hex ended the stream: `lease-expired`, `model-changed`, `transcription-failed`, `hex-quitting`.
    Error {
        code: String,
        message: String,
    },
    Cancelled,
}

impl StreamEvent {
    fn parse(v: &Value) -> Option<StreamEvent> {
        let text = |k: &str| v[k].as_str().unwrap_or("").to_owned();
        Some(match v["type"].as_str()? {
            "partial" => StreamEvent::Partial {
                segment: v["segment"].as_u64()?,
                stable: text("stable"),
                tentative: text("tentative"),
            },
            "segment" => StreamEvent::Segment(segment(v)?),
            "final" => StreamEvent::Final(final_of(v)?),
            "error" => StreamEvent::Error {
                code: text("code"),
                message: text("message"),
            },
            "cancelled" => StreamEvent::Cancelled,
            _ => return None,
        })
    }

    fn ends(&self) -> bool {
        matches!(
            self,
            StreamEvent::Final(_) | StreamEvent::Error { .. } | StreamEvent::Cancelled
        )
    }
}

fn segment(v: &Value) -> Option<Segment> {
    Some(Segment {
        segment: v["segment"].as_u64()?,
        start_ms: v["startMs"].as_u64()?,
        end_ms: v["endMs"].as_u64()?,
        text: v["text"].as_str()?.to_owned(),
    })
}

fn final_of(v: &Value) -> Option<Final> {
    let segments = v["segments"]
        .as_array()?
        .iter()
        .map(segment)
        .collect::<Option<_>>()?;
    Some(Final {
        text: v["text"].as_str()?.to_owned(),
        segments,
        duration_ms: v["durationMs"].as_u64()?,
    })
}

/// A stream's events in order, from its start, until it ends or the connection drops. Hex keeps
/// one reader per stream; a newer one ends this.
pub struct StreamEvents {
    lines: std::io::Lines<BufReader<Box<dyn Read + Send>>>,
    done: bool,
}

impl Iterator for StreamEvents {
    type Item = StreamEvent;

    fn next(&mut self) -> Option<StreamEvent> {
        while !self.done {
            let Some(Ok(line)) = self.lines.next() else {
                break;
            };
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let Some(event) = serde_json::from_str::<Value>(data.trim())
                .ok()
                .as_ref()
                .and_then(StreamEvent::parse)
            else {
                continue;
            };
            self.done = event.ends();
            return Some(event);
        }
        self.done = true;
        None
    }
}

impl Hex {
    /// One `/streams` request; a reply Hex refused comes back as its code.
    fn stream_call(
        &self,
        path: &str,
        s: Option<&Stream>,
        kind: &str,
        body: &[u8],
        wait: Duration,
    ) -> Result<Value, String> {
        let agent: ureq::Agent = net_trust::agent_config_for(&self.base)
            .timeout_global(Some(wait))
            .http_status_as_error(false)
            .build()
            .into();
        let mut req = agent
            .post(format!("{}{path}", self.base))
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", kind);
        if let Some(s) = s {
            req = req.header("x-hex-stream-token", &s.owner);
        }
        let mut resp = req.send(body).map_err(|_| NOT_RUNNING.to_owned())?;
        let status = resp.status().as_u16();
        let v: Value = serde_json::from_str(&resp.body_mut().read_to_string().unwrap_or_default())
            .unwrap_or(Value::Null);
        if status >= 400 {
            return Err(v["code"].as_str().unwrap_or("refused").to_owned());
        }
        Ok(v)
    }

    /// Starts a stream of f32 samples at 16 kHz on whatever model Hex's dictation holds.
    pub fn stream(&self) -> Result<Stream, String> {
        let body = json!({ "sampleFormat": "f32le", "sampleRate": STREAM_RATE }).to_string();
        let v = self
            .stream_call(
                "/streams",
                None,
                "application/json",
                body.as_bytes(),
                Duration::from_secs(10),
            )
            .map_err(|code| match code.as_str() {
                "stream-busy" => STREAM_BUSY.to_owned(),
                "model-not-loaded" => NO_MODEL.to_owned(),
                "engine-unavailable" => NO_ENGINE.to_owned(),
                // A Hex from before streams.
                "not_found" => self.outdated.clone(),
                _ if code.contains("running") => code,
                _ => format!("Hex couldn't start a stream: {code}"),
            })?;
        let text = |k: &str| v[k].as_str().unwrap_or("").to_owned();
        match (v["id"].as_u64(), v["ownerToken"].as_str()) {
            (Some(id), Some(owner)) => Ok(Stream {
                id,
                owner: owner.to_owned(),
                model: text("model"),
                language: text("language"),
                partials: v["partials"] == true,
            }),
            _ => Err("Hex sent an odd reply".into()),
        }
    }

    /// Sends recorded samples (mono, 16 kHz, -1 to 1) and keeps the stream alive. Fails with
    /// Hex's code: [`BACKLOG_FULL`] means push the same samples again shortly.
    pub fn push(&self, s: &Stream, samples: &[f32]) -> Result<(), String> {
        for piece in samples.chunks(MAX_PUSH_SAMPLES) {
            let bytes: Vec<u8> = piece.iter().flat_map(|f| f.to_le_bytes()).collect();
            self.stream_call(
                &format!("/streams/{}/audio", s.id),
                Some(s),
                "application/octet-stream",
                &bytes,
                Duration::from_secs(10),
            )?;
        }
        Ok(())
    }

    /// Keeps a stream alive while nothing is pushed (Hex ends one after 10 s without either).
    pub fn stream_heartbeat(&self, s: &Stream) -> Result<(), String> {
        self.stream_call(
            &format!("/streams/{}/heartbeat", s.id),
            Some(s),
            "application/json",
            b"",
            Duration::from_secs(5),
        )
        .map(|_| ())
    }

    /// The stream's events, from its start until it ends. Read them on their own thread.
    pub fn stream_events(&self, s: &Stream) -> Result<StreamEvents, String> {
        let agent: ureq::Agent = net_trust::agent_config_for(&self.base)
            .timeout_connect(Some(Duration::from_secs(5)))
            .http_status_as_error(false)
            .build()
            .into();
        let resp = agent
            .get(format!("{}/streams/{}/events", self.base, s.id))
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("x-hex-stream-token", &s.owner)
            .call()
            .map_err(|_| NOT_RUNNING.to_owned())?;
        if resp.status().as_u16() >= 400 {
            return Err(format!("events refused: {}", resp.status().as_u16()));
        }
        let reader: Box<dyn Read + Send> = Box::new(resp.into_body().into_reader());
        Ok(StreamEvents {
            lines: BufReader::new(reader).lines(),
            done: false,
        })
    }

    /// Transcribes what's left and waits for the whole text. Hex's code on failure, such as
    /// `stream-finish-timeout` (still going: finish again) or the code the stream ended with.
    pub fn finish_stream(&self, s: &Stream) -> Result<Final, String> {
        let v = self.stream_call(
            &format!("/streams/{}/finish", s.id),
            Some(s),
            "application/json",
            b"",
            Duration::from_secs(130),
        )?;
        final_of(&v).ok_or_else(|| "Hex sent an odd reply".to_owned())
    }

    pub fn cancel_stream(&self, s: &Stream) {
        self.stream_call(
            &format!("/streams/{}/cancel", s.id),
            Some(s),
            "application/json",
            b"",
            Duration::from_secs(5),
        )
        .ok();
    }
}

