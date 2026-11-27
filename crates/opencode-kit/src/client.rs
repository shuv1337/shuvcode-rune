//! The OpenCode server connection: GETs on a worker thread, writes (prompts, replies, stop, new
//! session) on another, `/api/event` on a third, all waking the window when data lands.
//!
//! The password comes from `OPENCODE_SERVER_PASSWORD`, the local OpenCode `service.json`,
//! or a connect link (`Server::with_password`, pairing.rs), and only ever goes into the
//! Authorization header.

#[cfg(not(target_arch = "wasm32"))]
use std::io::{BufRead, BufReader, Read};
#[cfg(not(target_arch = "wasm32"))]
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use rune::time::Instant;

#[cfg(target_arch = "wasm32")]
use rune::Waker;
use serde_json::Value;

use crate::model::{self, Delivery, Pending, SessionInfo};
use crate::wire_error::{WireError, WireKind};

pub mod browser_rpc;
mod downloads;
mod http;
mod hub;
mod ids;
mod jobs;
pub mod mcp;
mod media;
mod mentions;
#[cfg(not(target_arch = "wasm32"))]
pub use media::TempMediaFile;
mod output_head;
mod plugins;
#[cfg(not(target_arch = "wasm32"))]
mod preview;
#[cfg(not(target_arch = "wasm32"))]
pub use preview::{PreviewAbort, PreviewHttpHead};
#[cfg(not(target_arch = "wasm32"))]
mod reach;
#[cfg(not(target_arch = "wasm32"))]
mod streams;
pub mod rewrite;
mod upload;
#[cfg(target_arch = "wasm32")]
pub mod web;
mod wire;
mod worker;

pub use downloads::{export_filename, save_download, set_download_dir};
pub use hub::{EventHub, HUB_AT, connect_workers, heard_at};
pub use ids::{message_id, session_id};
pub use jobs::{JobEndpoint, is_job_endpoint_unsupported};
pub use plugins::{PluginEndpoint, is_plugin_endpoint_unsupported};
#[cfg(not(target_arch = "wasm32"))]
pub use reach::reach_back;
#[cfg(not(target_arch = "wasm32"))]
pub use streams::verify as verify_streams;

/// In a browser the page's own connection is the stream: nothing to verify.
#[cfg(target_arch = "wasm32")]
pub fn verify_streams() {}

/// In a browser no address is watched: the page's own requests find a server back.
#[cfg(target_arch = "wasm32")]
pub fn reach_back(_seen: &mut u64) -> Vec<String> {
    Vec::new()
}

#[cfg(target_arch = "wasm32")]
impl Server {
    pub fn watch_reach(&self, _wake: Option<Waker>) {}

    pub fn refusing(&self) -> bool {
        false
    }
}
pub use upload::UploadError;
pub use wire::{Incoming, Request, Ticket, TicketKind, Write};
pub use worker::{connect, connect_with_idle};

// shuv1337 fork: Shuvcode's default service port is 0x1337 (4919).
pub const DEFAULT_SERVER: &str = "http://127.0.0.1:4919";

/// The server to use when no `--server` was passed: `OPENCODE_SERVER_URL` when set (a Mac that
/// reaches another Mac's server through a tunnel sets it with `launchctl setenv`, so opening the
/// app from Spotlight or a launcher still finds it), else [`DEFAULT_SERVER`].
pub fn default_server() -> String {
    // Public standalone client API; opencode-kit has no app config.rs layer.
    #[allow(clippy::disallowed_methods)]
    server_or_default(std::env::var("OPENCODE_SERVER_URL").ok())
}

fn server_or_default(env: Option<String>) -> String {
    env.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()).unwrap_or_else(|| DEFAULT_SERVER.to_owned())
}

/// Sent as `X-OpenCode-Capabilities` on message and inbox reads by a [`Server`] built with
/// [`Server::with_media_refs`]: the server may answer with `media:<hash>` refs (see
/// [`model::Attachment`]) instead of inline bytes. Servers without it ignore the header.
pub const MEDIA_CAPABILITY: &str = "experimental-media-uri";

pub use crate::prompt::{AgentInput, FileInput, Inline, Mention, PathAttachment, Prompt, SkillInput};

/// `encodeURIComponent`, for a query value.
pub(crate) fn encode_component(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// `data:<mime>;base64,<bytes>`.
pub fn data_url(mime: &str, bytes: &[u8]) -> String {
    format!("data:{mime};base64,{}", base64(bytes))
}

fn list(v: &Value) -> Vec<Value> {
    v["data"].as_array().or_else(|| v.as_array()).cloned().unwrap_or_default()
}

/// When a read's response started (headers in), its body's size, and when JSON parsing began.
#[derive(Clone, Copy, Debug)]
pub struct ReadTiming {
    pub first_byte: Instant,
    pub bytes: usize,
    pub parsing: Instant,
}

/// What a prompt's or command's POST must get back to count as taken ([`Server::write_bytes`]).
#[derive(Clone, Copy)]
pub(crate) enum Expect<'a> {
    /// A 2xx JSON `{ data: { id } }`, the id the prompt carried when it carried one.
    Prompt(Option<&'a str>),
    /// A `204`, or a 2xx JSON.
    Command,
}

/// A prompt's request, made before it's sent ([`Server::prepare`]).
pub enum Prepared {
    Prompt(Vec<u8>),
    Command(Vec<u8>),
}

#[derive(Clone)]
pub struct Server {
    pub base: String,
    pub(crate) auth: Option<String>,
    pub(crate) media_refs: bool,
    /// Its files aren't this machine's even on a loopback address (a WSL distro's server, an SSH
    /// tunnel), as Desktop treats a contributed server.
    pub(crate) remote_files: bool,
    /// Every request's connections, kept alive between them ([`http::agent_for`]).
    pub(crate) agent: ureq::Agent,
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server").field("base", &self.base).finish_non_exhaustive()
    }
}

fn base64(input: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(input)
}

/// Appends `input`'s standard base64 to `out`, encoded in place.
pub(crate) fn base64_into(input: &[u8], out: &mut Vec<u8>) {
    use base64::Engine;
    let start = out.len();
    out.resize(start + input.len().div_ceil(3) * 4, 0);
    let written = base64::engine::general_purpose::STANDARD.encode_slice(input, &mut out[start..]).unwrap_or(0);
    out.truncate(start + written);
}

impl Server {
    /// Interrupt only sleeping stream retries; healthy streams stay connected.
    pub fn wake_reconnects() { worker::wake_reconnects(); }
    pub fn new(base: &str) -> Server {
        let clean = base.trim_end_matches('/').to_owned();
        // Public standalone client API; opencode-kit has no app config.rs layer.
        #[allow(clippy::disallowed_methods)]
        let password = std::env::var("OPENCODE_SERVER_PASSWORD")
            .ok()
            .or_else(|| crate::local_service::password_for_base(&clean));
        let auth = password
            .map(|p| p.trim().to_owned())
            .filter(|p| !p.is_empty())
            .map(|p| format!("Basic {}", base64(format!("opencode:{p}").as_bytes())));
        Server { agent: http::agent_for(&clean), base: clean, auth, media_refs: false, remote_files: false }
    }

    /// `base` with no password (not even the environment's), for a caller that brings its own.
    pub fn anonymous(base: &str) -> Server {
        let clean = base.trim_end_matches('/').to_owned();
        Server { agent: http::agent_for(&clean), base: clean, auth: None, media_refs: false, remote_files: false }
    }

    /// Uses `password` (a token from a connect link, say) in place of the one `new` found.
    pub fn with_password(mut self, password: &str) -> Server {
        self.auth = Some(format!("Basic {}", base64(format!("opencode:{}", password.trim()).as_bytes())));
        self
    }

    /// The `Authorization` header value this connection sends, if any.
    pub fn auth_header(&self) -> Option<&str> {
        self.auth.as_deref()
    }

    /// Marks its files as another machine's though it answers on a loopback address.
    pub fn with_remote_files(mut self) -> Server {
        self.remote_files = true;
        self
    }

    /// Opts `connect`'s message and inbox reads into `media:` refs ([`MEDIA_CAPABILITY`]). Off by
    /// default. A media-capable client can fetch refs with `Server::media`, but should not
    /// enable this on a connection shared by background tasks: some servers initialize the
    /// session's location to resolve its media configuration, recording project activity.
    pub fn with_media_refs(mut self) -> Server {
        self.media_refs = true;
        self
    }

    /// A read `connect` makes: with the capability header when opted in.
    pub fn read(&self, path: &str) -> Result<Value, WireError> {
        self.get_with(path, self.media_refs.then_some(MEDIA_CAPABILITY))
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn addr(&self) -> Option<SocketAddr> {
        let (scheme, rest) = self.base.split_once("://")?;
        let host = rest.split('/').next()?.replace("localhost", "127.0.0.1");
        if let Ok(a) = host.parse() {
            return Some(a);
        }
        // A name, as a remote server behind a tunnel has.
        let port = if scheme == "https" { 443 } else { 80 };
        let host = if host.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) { host } else { format!("{host}:{port}") };
        std::net::ToSocketAddrs::to_socket_addrs(&host).ok()?.next()
    }

    /// A quick TCP check, so a missing server falls back to the fixture without waiting.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn reachable(&self) -> bool {
        self.addr().is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_millis(250)).is_ok())
    }

    /// The server shares this machine's filesystem, so a local path means the same file there.
    pub fn is_local(&self) -> bool {
        if self.remote_files {
            return false;
        }
        let host = self.base.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or("");
        let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
        matches!(host, "127.0.0.1" | "localhost" | "[::1]")
    }

    pub fn get(&self, path: &str) -> Result<Value, WireError> {
        self.get_with(path, None)
    }

    /// `get`, waiting out a server that answers 503 while it starts: `GET /api/skill` waits for
    /// the server's plugins and refuses with `ServiceUnavailableError` until they've loaded. It
    /// tries again after 0.25, 0.5, 1, 2 and 4 s, so off the app's thread only.
    pub fn get_ready(&self, path: &str) -> Result<Value, WireError> {
        self.ready(|| self.get_json(path, None, Duration::from_secs(15)).map(|(v, _)| v))
    }

    /// `f`, again while it's refused with 503.
    fn ready<T>(&self, f: impl Fn() -> Result<T, WireError>) -> Result<T, WireError> {
        let mut wait = Duration::from_millis(250);
        for _ in 0..5 {
            match f() {
                Err(e) if e.status() == Some(503) => rune::task::sleep(wait),
                done => return done,
            }
            wait *= 2;
        }
        f()
    }

    fn get_with(&self, path: &str, capabilities: Option<&str>) -> Result<Value, WireError> {
        self.get_timed(path, capabilities).map(|(v, _)| v)
    }

    /// `get_text`, waiting out a server that answers 503 while it starts, as [`Self::get_ready`]
    /// does: off the app's thread only.
    pub fn get_text_ready(&self, path: &str) -> Result<String, WireError> {
        self.ready(|| self.json_text(path))
    }

    /// `GET path`'s JSON as the server wrote it, unparsed (its keys in the server's order).
    pub fn get_text(&self, path: &str) -> Result<String, WireError> {
        self.json_text(path)
    }

    fn json_text(&self, path: &str) -> Result<String, WireError> {
        let answer = self.exchange("GET", path, None, Some(Duration::from_secs(15)), &[])?;
        // Checked as JSON for whom it came from; handed back as the text it was.
        let text = String::from_utf8_lossy(&answer.body).into_owned();
        answer.json("GET", path).map(|_| text)
    }

    /// `read`, with when its response started, its body size and when parsing started.
    pub fn read_timed(&self, path: &str) -> Result<(Value, ReadTiming), WireError> {
        self.get_timed(path, self.media_refs.then_some(MEDIA_CAPABILITY))
    }

    fn get_timed(&self, path: &str, capabilities: Option<&str>) -> Result<(Value, ReadTiming), WireError> {
        self.get_json(path, capabilities, Duration::from_secs(15))
    }

    /// A GET of OpenCode's JSON within `timeout`.
    pub(crate) fn get_json(&self, path: &str, capabilities: Option<&str>, timeout: Duration) -> Result<(Value, ReadTiming), WireError> {
        let caps;
        let headers: &[(&str, &str)] = match capabilities {
            Some(c) => {
                caps = [("X-OpenCode-Capabilities", c)];
                &caps
            }
            None => &[],
        };
        let answer = self.exchange("GET", path, None, Some(timeout), headers)?;
        let first_byte = answer.first_byte;
        let (value, bytes, parsing) = answer.json("GET", path)?;
        Ok((value, ReadTiming { first_byte, bytes, parsing }))
    }

    /// The bytes behind a `media:<hash>` ref (`GET /api/experimental/media/:hash`). They never
    /// change, so callers cache them by hash; a 404 means nothing references them anymore.
    pub fn media(&self, hash: &str) -> Result<Vec<u8>, WireError> {
        self.get_bytes(&format!("/api/experimental/media/{hash}"))
    }

    /// Raw bytes from `path` (such as `/api/fs/read/<path>?location[directory]=…`).
    pub fn get_bytes(&self, path: &str) -> Result<Vec<u8>, WireError> {
        self.exchange("GET", path, None, Some(Duration::from_secs(60)), &[])?.bytes("GET", path)
    }

    /// A write: `method` with an optional JSON body; OpenCode's empty (204) reply is `Value::Null`.
    pub fn write(&self, method: &'static str, path: &str, body: Option<Value>) -> Result<Value, WireError> {
        self.write_within(method, path, body, Duration::from_secs(15))
    }

    /// A prompt's or command's POST (`body` already made, `Prompt::body_bytes`), taken only on an
    /// answer that proves OpenCode took it: OpenCode's JSON (`http::Answer::json`) of the shape
    /// `expect` names. Anything else is an error, so the prompt's outbox row stays.
    pub(crate) fn write_bytes(&self, path: &str, body: Vec<u8>, expect: Expect) -> Result<Value, WireError> {
        rune::marks::mark("net.sending");
        let answer = self.exchange("POST", path, Some(&body), Some(Duration::from_secs(15)), &[])?;
        let status = answer.status;
        let (v, _, _) = answer.json("POST", path)?;
        let what = match (expect, v["data"]["id"].as_str()) {
            (Expect::Command, _) => return Ok(v),
            (Expect::Prompt(Some(want)), Some(got)) if got != want => format!("JSON naming message {got}, not {want}"),
            (Expect::Prompt(_), Some(_)) => return Ok(v),
            (Expect::Prompt(_), None) => "JSON without the message's id (data.id)".into(),
        };
        Err(crate::wire_error::WireError::new("POST", path, crate::wire_error::WireKind::NotOpenCode { status, what }))
    }

    /// `write`, waiting up to `timeout` for the reply.
    pub(crate) fn write_within(&self, method: &'static str, path: &str, body: Option<Value>, timeout: Duration) -> Result<Value, WireError> {
        self.write_raw(method, path, body.map(|b| b.to_string().into_bytes()), timeout)
    }

    /// A write that counts only when OpenCode answers it: never a redirect followed to a sign-in
    /// page, never a 200 that isn't OpenCode's JSON (`http::Answer::json`).
    fn write_raw(&self, method: &'static str, path: &str, body: Option<Vec<u8>>, timeout: Duration) -> Result<Value, WireError> {
        let answer = self.exchange(method, path, body.as_deref(), Some(timeout), &[])?;
        Ok(answer.json(method, path)?.0)
    }

    /// `POST /session/:id/prompt`. Tests point this at a local mock, never the live server.
    pub fn send_prompt(&self, session: &str, text: &str) -> Result<(), WireError> {
        self.write("POST", &format!("/api/session/{session}/prompt"), Some(serde_json::json!({ "text": text }))).map(|_| ())
    }

    /// `POST /session/:id/prompt` with an id and files; returns the id the server gave the
    /// message (`data.id`), which is `prompt.id` when that was set.
    pub fn send(&self, session: &str, prompt: &Prompt) -> Result<Option<String>, WireError> {
        let request = self.prepare(session, prompt);
        self.send_prepared(session, prompt, request)
    }

    /// What [`Server::send`] posts, made before anything is sent: a command's body, or the
    /// prompt's with the files its comments mention (reads only, no change on the server).
    pub fn prepare(&self, session: &str, prompt: &Prompt) -> Prepared {
        rune::marks::mark("net.prompt");
        let request = match prompt.command {
            Some(_) => Prepared::Command(prompt.command_body().to_string().into_bytes()),
            None => Prepared::Prompt(self.with_mentions(None, session, prompt).body_bytes()),
        };
        rune::marks::mark("net.body");
        request
    }

    /// Sends what [`Server::prepare`] made: the agent and model switches, then the prompt.
    pub fn send_prepared(&self, session: &str, prompt: &Prompt, request: Prepared) -> Result<Option<String>, WireError> {
        self.apply_choice(session, prompt)?;
        match request {
            Prepared::Command(body) => self.write_bytes(&format!("/api/session/{session}/command"), body, Expect::Command).map(|_| None),
            Prepared::Prompt(body) => {
                let v = self.write_bytes(&format!("/api/session/{session}/prompt"), body, Expect::Prompt(prompt.id.as_deref()))?;
                rune::marks::mark("net.answered");
                Ok(v["data"]["id"].as_str().map(str::to_owned))
            }
        }
    }
    /// Desktop's `applySelection`: a prompt that runs now (a steer, or any prompt to an idle
    /// session) switches the session's agent, then its model, before it's admitted. A queued one
    /// mustn't reconfigure the turn it waits behind, so it only records its choice.
    fn apply_choice(&self, session: &str, prompt: &Prompt) -> Result<(), WireError> {
        if prompt.delivery == Some(Delivery::Queue) {
            return Ok(());
        }
        if let Some(agent) = &prompt.agent {
            self.switch_agent(session, agent)?;
        }
        if let Some(m) = &prompt.model {
            self.switch_model(session, m)?;
        }
        Ok(())
    }

    /// `session.switchAgent`: `{"agent": …}`.
    pub fn switch_agent(&self, session: &str, agent: &str) -> Result<(), WireError> {
        self.write("POST", &format!("/api/session/{session}/agent"), Some(serde_json::json!({ "agent": agent }))).map(|_| ())
    }

    /// `session.switchModel`: `{"model": {id, providerID, variant?}}`, no variant for the model's
    /// default.
    pub fn switch_model(&self, session: &str, m: &model::ModelRef) -> Result<(), WireError> {
        let mut v = serde_json::json!({ "id": m.id, "providerID": m.provider });
        if !m.variant.is_empty() {
            v["variant"] = m.variant.clone().into();
        }
        self.write("POST", &format!("/api/session/{session}/model"), Some(serde_json::json!({ "model": v }))).map(|_| ())
    }

    /// Runs `command` in the session's directory; the server reports it as `shell.*` events.
    pub fn shell(&self, session: &str, id: Option<&str>, command: &str) -> Result<(), WireError> {
        let mut body = serde_json::json!({ "command": command });
        if let Some(id) = id {
            body["id"] = id.into();
        }
        self.write("POST", &format!("/api/session/{session}/shell"), Some(body)).map(|_| ())
    }

    /// `decision`: `once`, `always` or `reject`.
    pub fn reply_permission(&self, session: &str, request: &str, decision: &str) -> Result<(), WireError> {
        let path = format!("/api/session/{session}/permission/{request}/reply");
        self.write("POST", &path, Some(serde_json::json!({ "decision": decision }))).map(|_| ())
    }

    /// `answer` maps each field key to a string, number, bool or list of strings.
    pub fn reply_form(&self, session: &str, form: &str, answer: Value) -> Result<(), WireError> {
        let path = format!("/api/session/{session}/form/{form}/reply");
        self.write("POST", &path, Some(serde_json::json!({ "answer": answer }))).map(|_| ())
    }

    pub fn cancel_form(&self, session: &str, form: &str) -> Result<(), WireError> {
        self.write("DELETE", &format!("/api/session/{session}/form/{form}"), None).map(|_| ())
    }

    /// Stops the session's current run.
    pub fn interrupt(&self, session: &str) -> Result<(), WireError> {
        self.write("POST", &format!("/api/session/{session}/interrupt"), None).map(|_| ())
    }

    /// Like `interrupt`, preserving whether a run was interrupted. `false` means idle or
    /// already stopping; `None` is an older server that doesn't return an acknowledgement.
    pub fn interrupt_ack(&self, session: &str) -> Result<Option<bool>, WireError> {
        self.write("POST", &format!("/api/session/{session}/interrupt"), None).map(|v| v["interrupted"].as_bool())
    }

    /// Stop, as Desktop's composer asks it (`session/composer/adapter.ts:56-60`): `interrupt_ack`
    /// with `resume=true`, so the server goes on with the steers already waiting (and manual
    /// compactions and moves) while queued prompts stay parked.
    pub fn stop(&self, session: &str) -> Result<Option<bool>, WireError> {
        self.write("POST", &format!("/api/session/{session}/interrupt?resume=true"), None).map(|v| v["interrupted"].as_bool())
    }

    /// `session.background`: moves the session's running shells and subagents that block its run
    /// to the background, so the run goes on to its next step. The server moves each job once
    /// and ignores the call when nothing blocks.
    pub fn background(&self, session: &str) -> Result<(), WireError> {
        self.write("POST", &format!("/api/session/{session}/background"), None).map(|_| ())
    }

    /// `session.compact`: asks the server to summarize the session now. It admits the compaction
    /// to the session's inbox, steering a running turn as Desktop's does.
    pub fn compact(&self, session: &str) -> Result<(), WireError> {
        self.write("POST", &format!("/api/session/{session}/compact"), Some(serde_json::json!({}))).map(|_| ())
    }

    pub fn create_session(&self, directory: &str) -> Result<SessionInfo, WireError> {
        self.create(serde_json::json!({ "location": { "directory": directory } }))
    }

    /// `create_session` under `id`, running `agent` and `model` from the start (Desktop's
    /// `session.create` from the new-session page), so its first prompt needs no switches.
    pub fn create_session_as(&self, id: &str, directory: &str, agent: Option<&str>, model: Option<&model::ModelRef>) -> Result<SessionInfo, WireError> {
        let mut body = serde_json::json!({ "id": id, "location": { "directory": directory } });
        if let Some(agent) = agent {
            body["agent"] = agent.into();
        }
        if let Some(m) = model {
            body["model"] = serde_json::json!({ "id": m.id, "providerID": m.provider });
            if !m.variant.is_empty() {
                body["model"]["variant"] = m.variant.clone().into();
            }
        }
        self.create(body)
    }

    fn create(&self, body: Value) -> Result<SessionInfo, WireError> {
        let v = self.write("POST", "/api/session", Some(body))?;
        let info = model::parse_session(v.get("data").unwrap_or(&v));
        if info.id.is_empty() { Err(WireError::new("POST", "/api/session", WireKind::Shape("no session in the reply".into()))) } else { Ok(info) }
    }

    /// Create and admit the first prompt, calling `after_create` between `POST /api/session` and
    /// `POST /prompt` (used by the fail-open `<= 1 s` new-session browser-attach gate).
    pub fn start_as(&self, id: &str, directory: &str, prompt: &Prompt, after_create: impl FnOnce(&SessionInfo)) -> Result<SessionInfo, WireError> {
        let created = self.create_session_as(id, directory, prompt.agent.as_deref(), prompt.model.as_ref());
        // The create already chose the agent and model, so no switches before the prompt.
        let sent = created.as_ref().map_err(WireError::clone).and_then(|info| {
            after_create(info);
            let prompt = self.with_mentions(Some(directory), id, prompt);
            self.write_bytes(&format!("/api/session/{id}/prompt"), prompt.body_bytes(), Expect::Prompt(prompt.id.as_deref()))
        });
        match sent {
            Ok(_) => created,
            Err(e) => {
                // A lost reply may be the only missing part of a successful first turn.
                // Deleting the session then destroys both the message and its receipt.
                if created.is_ok() && matches!(e.kind, WireKind::Status { status, .. } if (400..500).contains(&status) && ![408, 409, 425, 429].contains(&status)) {
                    self.delete_session(id).ok();
                }
                Err(e)
            }
        }
    }

    /// `worktree.create`: a new worktree of `project` from the checkout `from`, on `branch` or
    /// the checkout's current one; returns its directory. The server runs the project's setup
    /// script before it answers (often a dependency install), so this waits up to ten minutes.
    pub fn create_worktree(&self, project: &str, from: &str, branch: Option<&str>) -> Result<String, WireError> {
        let mut body = serde_json::json!({ "projectID": project, "from": from });
        if let Some(b) = branch {
            body["branch"] = b.into();
        }
        let v = self.write_within("POST", "/api/worktree", Some(body), Duration::from_secs(600))?;
        v.get("data").unwrap_or(&v)["directory"].as_str().map(str::to_owned).ok_or_else(|| WireError::new("POST", "/api/worktree", WireKind::Shape("no directory in the reply".into())))
    }

    /// `worktree.remove`, forced: deletes the worktree `directory` of `project` and every file in
    /// it, waiting up to two minutes (Desktop's `WORKSPACE_REMOVE_TIMEOUT_MS`).
    pub fn remove_worktree(&self, project: &str, directory: &str) -> Result<(), WireError> {
        let body = serde_json::json!({ "projectID": project, "directory": directory, "force": true });
        self.write_within("DELETE", "/api/worktree", Some(body), Duration::from_secs(120)).map(|_| ())
    }

    /// `StartInNewWorktree`: the worktree, then `start_as` in it.
    #[allow(clippy::too_many_arguments)]
    pub fn start_in_new_worktree(&self, id: &str, project: &str, from: &str, branch: Option<&str>, prompt: &Prompt, made: impl FnOnce(), after_create: impl FnOnce(&SessionInfo)) -> Result<SessionInfo, WireError> {
        let directory = self.create_worktree(project, from, branch)?;
        made();
        self.start_as(id, &directory, prompt, after_create)
    }

    /// `vcs.branch.list`: up to 50 branches at `directory` whose names hold `search`.
    pub fn branches(&self, directory: &str, search: &str) -> Result<Vec<String>, WireError> {
        let path = format!("/api/vcs/branch?location%5Bdirectory%5D={}&search={}&limit=50", encode_component(directory), encode_component(search));
        let v = self.get(&path)?;
        Ok(list(&v).iter().filter_map(|b| b.as_str().map(str::to_owned)).collect())
    }

    /// Resolves `directory` (`GET /api/location?location[directory]=…`), registering its project
    /// on the server as Desktop's `projects.add` does.
    pub fn resolve_location(&self, directory: &str) -> Result<model::LocationInfo, WireError> {
        let path = format!("/api/location?location%5Bdirectory%5D={}", encode_component(directory));
        let v = self.get(&path)?;
        model::parse_location(&v).ok_or_else(|| WireError::new("GET", &path, WireKind::Shape("no location in the reply".into())))
    }

    /// Lists the server's non-archived projects (`GET /api/project`).
    pub fn projects(&self) -> Result<Vec<model::Project>, WireError> {
        self.get("/api/project").map(|v| model::parse_projects(&v))
    }



    /// Every direct subagent session of `session`, newest first, in pages of 200 (Desktop's
    /// `listChildren`).
    pub fn children(&self, session: &str) -> Result<Vec<model::SessionInfo>, WireError> {
        const PAGE: usize = 200;
        let mut out = Vec::new();
        let mut path = format!("/api/session?parentID={}&order=desc&limit={PAGE}", encode_component(session));
        loop {
            let page = self.get(&path)?;
            let sessions = model::parse_sessions(&page);
            let full = sessions.len() >= PAGE;
            out.extend(sessions);
            match page["cursor"]["next"].as_str() {
                Some(next) if full => path = format!("/api/session?cursor={}&limit={PAGE}", encode_component(next)),
                _ => return Ok(out),
            }
        }
    }

    /// Registers `directory` as a project on the server and returns its resolved location and the
    /// refreshed project list.
    pub fn open_project(&self, directory: &str) -> Result<(model::LocationInfo, Vec<model::Project>), WireError> {
        let location = self.resolve_location(directory)?;
        let projects = self.projects()?;
        Ok((location, projects))
    }

    /// `session.update` with a title: the body is `{"title": …}` and nothing else.
    pub fn rename_session(&self, session: &str, title: &str) -> Result<(), WireError> {
        self.write("PATCH", &format!("/api/session/{session}"), Some(serde_json::json!({ "title": title }))).map(|_| ())
    }

    pub fn delete_session(&self, session: &str) -> Result<(), WireError> {
        self.write("DELETE", &format!("/api/session/{session}"), None).map(|_| ())
    }

    /// What Desktop's Export saves: `{ info, messages }`, the messages oldest first in pages of
    /// 200, with the file name it gives it.
    pub fn export_session(&self, session: &str) -> Result<(String, String), WireError> {
        let get = |path: &str| self.get_json(path, None, Duration::from_secs(60)).map(|(v, _)| v);
        let v = get(&format!("/api/session/{session}"))?;
        let info = v.get("data").unwrap_or(&v).clone();
        let mut messages = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            // A cursor carries its order; the server refuses both together.
            let path = match &cursor {
                Some(c) => format!("/api/session/{session}/message?limit=200&cursor={}", encode_component(c)),
                None => format!("/api/session/{session}/message?limit=200&order=asc"),
            };
            let mut page = get(&path)?;
            let Some(rows) = page["data"].as_array_mut() else { return Err(WireError::new("GET", &path, WireKind::Shape("missing message list".into()))); };
            messages.append(rows);
            cursor = page["cursor"]["next"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        let name = export_filename(&info, session);
        let json = serde_json::to_string_pretty(&serde_json::json!({ "info": info, "messages": messages })).map_err(|e| WireError::new("GET", &format!("/api/session/{session}/message"), WireKind::Other(e.to_string())))?;
        Ok((name, json))
    }

    pub fn cancel_inbox(&self, session: &str, item: &str) -> Result<(), WireError> {
        self.write("DELETE", &format!("/api/session/{session}/inbox/{item}"), None).map(|_| ())
    }

    pub fn set_delivery(&self, session: &str, item: &str, delivery: Delivery) -> Result<(), WireError> {
        let body = serde_json::json!({ "delivery": delivery.as_str() });
        self.write("PATCH", &format!("/api/session/{session}/inbox/{item}"), Some(body)).map(|_| ())
    }

    /// Open permission requests for `session`, propagating errors so callers never prune on failure.
    pub fn pending_permissions(&self, session: &str) -> Result<Vec<Pending>, WireError> {
        let v = self.get(&format!("/api/session/{session}/permission"))?;
        Ok(list(&v).iter().filter_map(model::parse_permission).collect())
    }

    /// Open structured forms for `session`, propagating errors so callers never prune on failure.
    pub fn pending_forms(&self, session: &str) -> Result<Vec<Pending>, WireError> {
        let v = self.get(&format!("/api/session/{session}/form"))?;
        Ok(list(&v).iter().filter_map(model::parse_form).collect())
    }

    /// The session's open permission requests and forms.
    pub fn pending(&self, session: &str) -> Vec<Pending> {
        let mut out = self.pending_permissions(session).unwrap_or_default();
        out.extend(self.pending_forms(session).unwrap_or_default());
        out
    }

    /// Reads `stream` as `epoch` until it ends or goes silent for `stream.idle`; drives `state`
    /// through its `server.connected` handshake and frames. Only once `server.connected` arrives
    /// does it enter `Open`, so a stream a captive portal answers (a redirect, a web page) is a
    /// failure, not a reconnect. A stream replaced after a network change or a wake
    /// (`streams::verify`) ends quietly, sending nothing more; `stream.heard()` is told of every
    /// line, heartbeats included.
    #[cfg(not(target_arch = "wasm32"))]
    fn events(&self, stream: &streams::Stream, epoch: u64, state: &mut worker::StreamState) -> Result<(), WireError> {
        use crate::wire_error::{read_failure, transport};
        use worker::{Handshake, StreamEffect, StreamEvent, stream_step};
        let down = |kind: WireKind| WireError::new("GET", "/api/event", kind);
        let config = ureq::Agent::config_builder().timeout_per_read(Some(stream.idle)).http_status_as_error(false).max_redirects(0);
        let agent: ureq::Agent = if http::loopback(&self.base) { config.proxy(None) } else { config }.build().into();
        let mut req = agent.get(format!("{}/api/event", self.base)).header("Accept", "text/event-stream");
        if let Some(a) = &self.auth {
            req = req.header("Authorization", a);
        }
        let mut resp = req.call().map_err(|e| down(transport(e)))?;
        let content_type = resp.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
        if resp.status().as_u16() != 200 {
            let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_owned);
            let body = resp.body_mut().with_config().limit(64 << 10).read_to_vec().unwrap_or_default();
            let answer = http::Answer { status: resp.status().as_u16(), content_type, location, body, first_byte: Instant::now() };
            return Err(answer.json("GET", "/api/event").err().unwrap_or_else(|| down(WireKind::Shape("an empty answer".into()))));
        }
        if !content_type.trim_start().to_ascii_lowercase().starts_with("text/event-stream") {
            return Err(down(WireKind::NotOpenCode { status: 200, what: http::kind_of(&content_type, false) }));
        }
        let mut reader = BufReader::new(resp.into_body().into_reader());
        let mut line = String::new();
        loop {
            if !stream.live(epoch) {
                stream_step(state, StreamEvent::Closed);
                return Ok(());
            }
            line.clear();
            let limit = 32 << 20;
            let read = (&mut reader).take(limit + 1).read_line(&mut line).map_err(|e| down(read_failure(e)))?;
            if read == 0 { break; }
            if read as u64 > limit { return Err(down(WireKind::TooLarge { limit, length: None })); }
            stream.heard();
            let Some(event) = event_line(line.trim_end_matches(['\r', '\n'])) else { continue };
            let connected = matches!(&event, Incoming::Event { ty, .. } if ty == "server.connected");
            let mut pending_frame = Some(event);
            let mut delivered = false;
            for effect in stream_step(state, StreamEvent::Frame { connected }) {
                match effect {
                    StreamEffect::DeliverHandshake(h) => {
                        let msg = match h {
                            Handshake::Opened => Incoming::Opened,
                            Handshake::Reconnected => Incoming::Reconnected,
                        };
                        if stream.inbox.send(msg).is_err() {
                            stream_step(state, StreamEvent::Closed);
                            return Ok(());
                        }
                        delivered = true;
                    }
                    StreamEffect::DeliverFrame => {
                        if let Some(msg) = pending_frame.take() {
                            if stream.inbox.send(msg).is_err() {
                                stream_step(state, StreamEvent::Closed);
                                return Ok(());
                            }
                            delivered = true;
                        }
                    }
                    StreamEffect::RejectHandshake(err) => return Err(err),
                    _ => {}
                }
            }
            if delivered {
                (stream.wake)();
            }
        }
        Err(down(WireKind::Dropped("the stream ended".into())))
    }
}

/// One line of the event stream as the app hears it: a `data:` line's event, or nothing for a
/// heartbeat, a comment or a line it can't read.
fn event_line(line: &str) -> Option<Incoming> {
    let data = line.strip_prefix("data:")?;
    let v = serde_json::from_str::<Value>(data.trim()).ok()?;
    let ty = v["type"].as_str().unwrap_or("").to_owned();
    if ty.is_empty() || ty == "server.heartbeat" {
        return None;
    }
    let mut data = v["data"].clone();
    // A compaction message, and a notice's, takes its id from the event that started it.
    if (ty.starts_with("session.compaction.") || crate::model::is_notice_event(&ty))
        && let Some(id) = v["id"].as_str()
    {
        data["eventID"] = id.into();
    }
    // When a request was asked, for the inbox's waiting age; and a branch change's folder.
    if ty.starts_with("session.execution.") || matches!(ty.as_str(), "permission.asked" | "form.created" | "session.archived") {
        data["eventCreated"] = v["created"].clone();
    }
    if matches!(ty.as_str(), "vcs.branch.updated" | "filesystem.changed" | "plugin.updated" | "config.updated") {
        data["eventLocation"] = v["location"].clone();
    }
    Some(Incoming::Event { ty, data })
}
