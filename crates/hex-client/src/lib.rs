//! Hex's desktop capture API (the one its SDK's `connect()` uses): an app borrows Hex's
//! microphone (the one the user chose in Hex) and model for one recording and gets the whole
//! transcript back from `finish`, with the words so far on the way where Hex offers them
//! (`partials`). Unlike `hex dictation start`, nothing ends it but the app (Smart Turn never
//! stops it on a pause), and nothing is pasted. `transcribe` turns a WAV the app recorded itself
//! into words. Hex writes where it listens to `local-api.json`, and removes the file when it quits.
//!
//! `stream` is the other way round: the app records and pushes the audio, and Hex sends
//! partials, segments, and the final (`stream.rs`). An older Windows Hex offers only that.
//!
//! `dictate` (`session/`) runs a whole dictation on whichever of the two the running Hex offers,
//! and `install.rs` finds, starts, and offers to download Hex.

#[cfg(target_arch = "wasm32")]
extern crate ureq_web as ureq;

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

mod install;
mod installed;
#[cfg(windows)]
mod mic;
mod session;
mod stream;
pub use install::*;
pub use installed::installed;
pub use session::*;
pub use stream::*;

/// Why a capture Hex has already ended can't be finished, from the code Hex answers a
/// heartbeat or finish with: a mic event cleared it, the app's cancel or a lapsed lease ended it,
/// its recording was gone, or the mic never gave any audio. None for a code worth trying again.
pub fn ended(code: &str) -> Option<&'static str> {
    match code {
        "dictation-not-active" | "dictation-not-found" => Some(STOPPED),
        "cancelled" => Some(CANCELLED),
        "capture-discarded" => Some(LOST),
        "microphone-lost" => Some(NO_MIC),
        _ => None,
    }
}

const STOPPED: &str = "Hex stopped the recording";
const CANCELLED: &str = "Hex cancelled the recording";
const LOST: &str = "Hex lost the recording";
const NO_MIC: &str = "The microphone gave no audio";

/// `finish` failed because Hex had already ended the capture, so asking again won't help.
pub fn is_ended(why: &str) -> bool {
    [STOPPED, CANCELLED, LOST, NO_MIC].contains(&why)
}

#[derive(Clone)]
pub struct Hex {
    base: String,
    token: String,
    /// The name Hex shows for who is recording.
    source: String,
    /// What `start` says when this Hex can't record (`OUTDATED` unless the app words it).
    outdated: String,
}

#[derive(Clone)]
pub struct Capture {
    pub id: u64,
    owner: String,
}

/// What the running Hex's local API offers (`/capabilities`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Hex records a capture itself (`/dictations`). Windows Hex before it did says no.
    pub capture: bool,
    /// A capture started with partials sends the words so far (`/dictations/{id}/partials`).
    pub partials: bool,
}

pub const NOT_RUNNING: &str = "Hex isn't running";
pub const BUSY: &str = "Hex is already recording";
pub const OUTDATED: &str = "This Hex can't record and needs an update";

/// Hex's endpoint file: `SEER_HEX` names one (tests, or a Hex elsewhere), else Hex's own in
/// `HEX_APPLICATION_SUPPORT_DIR` or `support` (the app passes Hex's Application Support folder).
pub fn discovery(support: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("SEER_HEX") {
        return p.into();
    }
    let support = std::env::var_os("HEX_APPLICATION_SUPPORT_DIR").map_or_else(|| support.to_owned(), PathBuf::from);
    support.join("local-api.json")
}

impl Hex {
    /// Hex at its usual endpoint file under `support`, recording for `source`.
    pub fn connect(support: &Path, source: &str) -> Result<Hex, String> {
        Self::connect_at(&discovery(support), source)
    }

    pub fn connect_at(file: &Path, source: &str) -> Result<Hex, String> {
        let off = || NOT_RUNNING.to_owned();
        let v: Value = serde_json::from_slice(&std::fs::read(file).map_err(|_| off())?).map_err(|_| off())?;
        let (Some(port), Some(token)) = (v["port"].as_u64(), v["token"].as_str()) else { return Err(off()) };
        Ok(Hex { base: format!("http://127.0.0.1:{port}"), token: token.to_owned(), source: source.to_owned(), outdated: OUTDATED.to_owned() })
    }

    /// The app's own words for a Hex that can't record.
    pub fn outdated_says(mut self, words: &str) -> Hex {
        self.outdated = words.to_owned();
        self
    }

    /// One request; a reply Hex refused comes back as its code.
    fn call(&self, path: &str, owner: Option<&str>, body: Option<Value>, wait: Duration) -> Result<Value, String> {
        let agent: ureq::Agent = net_trust::agent_config_for(&self.base).timeout_global(Some(wait)).http_status_as_error(false).build().into();
        let mut req = agent.post(format!("{}{path}", self.base)).header("Authorization", &format!("Bearer {}", self.token)).header("Content-Type", "application/json");
        if let Some(o) = owner {
            req = req.header("x-hex-dictation-token", o);
        }
        let mut resp = req.send(body.unwrap_or(json!({})).to_string().as_bytes()).map_err(|_| NOT_RUNNING.to_owned())?;
        let status = resp.status().as_u16();
        let v: Value = serde_json::from_str(&resp.body_mut().read_to_string().unwrap_or_default()).unwrap_or(Value::Null);
        if status >= 400 {
            return Err(v["code"].as_str().unwrap_or("refused").to_owned());
        }
        Ok(v)
    }

    /// What this Hex offers; None when it doesn't answer (it quit, or it's too old to say).
    pub fn capabilities(&self) -> Option<Capabilities> {
        let agent: ureq::Agent = net_trust::agent_config_for(&self.base).timeout_global(Some(Duration::from_secs(5))).http_status_as_error(false).build().into();
        let mut resp = agent.get(format!("{}/capabilities", self.base)).header("Authorization", &format!("Bearer {}", self.token)).call().ok()?;
        if resp.status().as_u16() >= 400 {
            return None;
        }
        let v: Value = serde_json::from_str(&resp.body_mut().read_to_string().ok()?).ok()?;
        Some(Capabilities { capture: v["serviceCapture"] == true, partials: v["dictationPartials"] == true })
    }

    /// Starts a capture on Hex's microphone, asking for the words so far (`partials`), which a
    /// Hex without them ignores. `records`: this Hex said it records captures, so one it can't
    /// start yet has its listener off or still loading its model, rather than being too old.
    pub fn start(&self, records: bool) -> Result<Capture, String> {
        let body = json!({ "source": self.source, "partials": true });
        let v = self.call("/dictations", None, Some(body), Duration::from_secs(10)).map_err(|code| match code.as_str() {
            "dictation-busy" => BUSY.to_owned(),
            "service-capture-unavailable" | "service-capture-busy" if records => NO_ENGINE.to_owned(),
            // An older Hex has the API but no capture behind it, or no `/dictations` at all.
            "service-capture-unavailable" | "not_found" => self.outdated.clone(),
            _ if code.contains("running") => code,
            _ => format!("Hex couldn't start: {code}"),
        })?;
        match (v["id"].as_u64(), v["ownerToken"].as_str()) {
            (Some(id), Some(owner)) => Ok(Capture { id, owner: owner.to_owned() }),
            _ => Err("Hex sent an odd reply".into()),
        }
    }

    /// Keeps the capture alive (Hex drops one that misses 10 s of heartbeats). Fails once Hex
    /// has ended it: Escape, or Hex quitting.
    pub fn heartbeat(&self, c: &Capture) -> Result<(), String> {
        self.call(&format!("/dictations/{}/heartbeat", c.id), Some(&c.owner), None, Duration::from_secs(5)).map(|_| ())
    }

    /// Transcribes a kept recording with the first model Hex has ready.
    pub fn transcribe(&self, wav: &Path) -> Result<String, String> {
        let bytes = whole(std::fs::read(wav).map_err(|_| "the recording is gone".to_owned())?);
        let agent: ureq::Agent = net_trust::agent_config_for(&self.base).timeout_global(Some(Duration::from_secs(10))).http_status_as_error(false).build().into();
        let mut resp = agent.get(format!("{}/models", self.base)).header("Authorization", &format!("Bearer {}", self.token)).call().map_err(|_| NOT_RUNNING.to_owned())?;
        let models: Value = serde_json::from_str(&resp.body_mut().read_to_string().unwrap_or_default()).unwrap_or(Value::Null);
        let ready: Vec<&str> = models.as_array().into_iter().flatten().filter(|m| m["installed"] == true && m["verified"] == true).filter_map(|m| m["id"].as_str()).collect();
        let model = ready.iter().find(|id| id.contains("parakeet")).or(ready.first()).ok_or("Hex has no model ready")?;
        let agent: ureq::Agent = net_trust::agent_config_for(&self.base).timeout_global(Some(Duration::from_secs(130))).http_status_as_error(false).build().into();
        let mut resp = agent
            .post(format!("{}/transcriptions?model={model}", self.base))
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", "audio/wav")
            .send(&bytes[..])
            .map_err(|_| NOT_RUNNING.to_owned())?;
        let status = resp.status().as_u16();
        let v: Value = serde_json::from_str(&resp.body_mut().read_to_string().unwrap_or_default()).unwrap_or(Value::Null);
        if status >= 400 {
            return Err(format!("Hex couldn't transcribe: {}", v["code"].as_str().unwrap_or("refused")));
        }
        Ok(v["transcript"].as_str().unwrap_or("").to_owned())
    }

    /// Stops the microphone and waits for the full transcript.
    pub fn finish(&self, c: &Capture) -> Result<String, String> {
        let v = self.call(&format!("/dictations/{}/finish", c.id), Some(&c.owner), None, Duration::from_secs(130)).map_err(|code| ended(&code).map_or_else(|| format!("Hex couldn't finish: {code}"), str::to_owned))?;
        Ok(v["transcript"].as_str().unwrap_or("").to_owned())
    }

    pub fn cancel(&self, c: &Capture) {
        self.call(&format!("/dictations/{}/cancel", c.id), Some(&c.owner), None, Duration::from_secs(5)).ok();
    }

    /// The words so far of a capture started with partials, as (`stable`, which won't change,
    /// `tentative`, which may), until the capture ends. Hex sends them while its model streams
    /// (on a GPU); `finish` has the transcript either way. Read them on their own thread.
    pub fn partials(&self, c: &Capture) -> Result<impl Iterator<Item = (String, String)> + Send + 'static, String> {
        let agent: ureq::Agent = net_trust::agent_config_for(&self.base).timeout_connect(Some(Duration::from_secs(5))).http_status_as_error(false).build().into();
        let resp = agent
            .get(format!("{}/dictations/{}/partials", self.base, c.id))
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("x-hex-dictation-token", &c.owner)
            .call()
            .map_err(|_| NOT_RUNNING.to_owned())?;
        if resp.status().as_u16() >= 400 {
            return Err(format!("partials refused: {}", resp.status().as_u16()));
        }
        let lines = BufReader::new(resp.into_body().into_reader()).lines();
        Ok(lines.map_while(Result::ok).filter_map(|line| {
            let v: Value = serde_json::from_str(line.strip_prefix("data:")?.trim()).ok()?;
            Some((v["stable"].as_str()?.to_owned(), v["tentative"].as_str().unwrap_or("").to_owned()))
        }))
    }

}

/// A kept WAV with its sizes set from its length: one the app stopped writing mid-stream still
/// says it holds no samples.
pub fn whole(mut wav: Vec<u8>) -> Vec<u8> {
    if wav.len() >= 44 && &wav[..4] == b"RIFF" && &wav[36..40] == b"data" {
        let data = (wav.len() - 44) as u32 / 2 * 2;
        wav[4..8].copy_from_slice(&(36 + data).to_le_bytes());
        wav[40..44].copy_from_slice(&data.to_le_bytes());
        wav.truncate(44 + data as usize);
    }
    wav
}

/// A 16-bit mono WAV header for `samples` samples at `rate`.
pub fn wav_header(rate: u32, samples: u32) -> [u8; 44] {
    let data = samples * 2;
    let mut h = [0u8; 44];
    let mut put = |at: usize, b: &[u8]| h[at..at + b.len()].copy_from_slice(b);
    put(0, b"RIFF");
    put(4, &(36 + data).to_le_bytes());
    put(8, b"WAVEfmt ");
    put(16, &16u32.to_le_bytes());
    put(20, &1u16.to_le_bytes());
    put(22, &1u16.to_le_bytes());
    put(24, &rate.to_le_bytes());
    put(28, &(rate * 2).to_le_bytes());
    put(32, &2u16.to_le_bytes());
    put(34, &16u16.to_le_bytes());
    put(36, b"data");
    put(40, &data.to_le_bytes());
    h
}
