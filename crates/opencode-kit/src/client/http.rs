//! Every request a [`Server`] makes, and what counts as OpenCode's answer to it. A redirect is
//! never followed (OpenCode's API never sends one, so it's a captive portal or a proxy talking),
//! an error status is read rather than thrown (OpenCode's explanation is in its body), and an
//! answer only counts when it has the type OpenCode gives it: JSON for its API, a download for a
//! file's bytes. Anything else is a [`WireError`] that says what answered instead.

use std::{io::Read, time::Duration};

use rune::time::Instant;
use serde_json::Value;

use super::Server;
use crate::wire_error::{WireError, WireKind, explanation, is_json, transport};

/// A response as it came, its body read.
pub(crate) struct Answer {
    pub status: u16,
    pub content_type: String,
    pub location: Option<String>,
    pub body: Vec<u8>,
    /// When its headers were in.
    pub first_byte: Instant,
}

/// Inline reads are bounded *before* JSON decoding, including chunked and decompressed bodies.
fn inline_limit(path: &str) -> u64 {
    let route = path.split('?').next().unwrap_or(path);
    if route.starts_with("/api/fs/read/") { 16 << 20 }
    else if route.starts_with("/api/session/") && route.split('/').nth(4) == Some("message") { 32 << 20 }
    else { 64 << 20 }
}

/// How long an idle kept-alive connection is reused: under the server's own keep-alive (5 s), so
/// a reused connection is never one the server has already closed.
const IDLE_REUSE: Duration = Duration::from_secs(4);

/// Whether `base` names this machine, where no proxy belongs: a `HTTP_PROXY` set for the web
/// would otherwise send the local server's (and a tunnel's) requests away.
pub(crate) fn loopback(base: &str) -> bool {
    let host = base.split("://").nth(1).unwrap_or("").split('/').next().unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, p)| if p.parse::<u16>().is_ok() { h } else { host });
    matches!(host, "127.0.0.1" | "localhost" | "[::1]") || host.starts_with("127.")
}

/// The agent a server's requests share (connections kept alive between them). A plain `http://`
/// base (the local server) reads no certificate store (`net_trust::agent_config_for`).
pub(crate) fn agent_for(base: &str) -> ureq::Agent {
    let config = net_trust::agent_config_for(base).http_status_as_error(false).max_redirects(0).max_idle_age(IDLE_REUSE);
    let config = if loopback(base) { config.proxy(None) } else { config };
    config.build().into()
}

impl Server {
    /// The agent every request of this server shares, made on the first one: on a worker's
    /// thread, so an `https` server's first use reads the certificate store there, not where the
    /// server was named.
    pub(crate) fn agent(&self) -> &ureq::Agent {
        self.agent.get_or_init(|| agent_for(&self.base))
    }

    /// One request: `body` sent as JSON when there is one, `timeout` for the whole exchange.
    pub(crate) fn exchange(&self, method: &'static str, path: &str, body: Option<&[u8]>, timeout: Option<Duration>, headers: &[(&str, &str)]) -> Result<Answer, WireError> {
        self.exchange_with_limit(method, path, body, timeout, headers, inline_limit(path))
    }

    pub(crate) fn exchange_with_limit(&self, method: &'static str, path: &str, body: Option<&[u8]>, timeout: Option<Duration>, headers: &[(&str, &str)], limit: u64) -> Result<Answer, WireError> {
        let url = format!("{}{path}", self.base);
        let fail = |kind| WireError::new(method, path, kind);
        let a = self.agent();
        let sent = match (method, body) {
            ("GET", _) => self.dress(a.get(url), headers, timeout).call(),
            ("DELETE", None) => self.dress(a.delete(url), headers, timeout).call(),
            ("DELETE", Some(b)) => self.dress(a.delete(url).force_send_body(), headers, timeout).header("Content-Type", "application/json").send(b),
            ("PATCH", b) => self.dress(a.patch(url), headers, timeout).header("Content-Type", "application/json").send(b.unwrap_or(&b"{}"[..])),
            ("PUT", b) => self.dress(a.put(url), headers, timeout).header("Content-Type", "application/json").send(b.unwrap_or(&b"{}"[..])),
            (_, b) => self.dress(a.post(url), headers, timeout).header("Content-Type", "application/json").send(b.unwrap_or(&b"{}"[..])),
        };
        let mut resp = sent.map_err(|e| fail(transport(e)))?;
        let first_byte = Instant::now();
        let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_owned);
        let (status, content_type, location) = (resp.status().as_u16(), header("content-type").unwrap_or_default(), header("location"));
        // An error's body is only its explanation: a little of it is enough.
        let length = header("content-length").and_then(|s| s.parse::<u64>().ok());
        let body = if status >= 300 {
            let mut bytes = Vec::new();
            // The status remains authoritative even if its explanation is oversized or cut short.
            resp.body_mut().as_reader().take(64 << 10).read_to_end(&mut bytes).ok();
            bytes
        } else if limit == 32 << 20 && is_json(&content_type) {
            // An indefinite page may be a single enormous string. Four readers buffering to
            // the page ceiling used hundreds of MiB before any one of them refused the page.
            let declared_large = length.is_some_and(|n| n > limit);
            let probe = match length {
                Some(n) if n <= limit && header("content-encoding").is_none() => limit,
                // Unknown-length legacy responses grow only to the inline tool budget plus a
                // small allowance for the page envelope before inspecting the in-flight token.
                None => (2 << 20) + (64 << 10),
                Some(n) if n <= limit => (2 << 20) + (64 << 10),
                _ => 8 << 10,
            };
            let mut reader = resp.body_mut().as_reader();
            let mut bytes = Vec::new();
            #[cfg(not(target_arch="wasm32"))]
            let read = |e| fail(crate::wire_error::read_failure(e));
            #[cfg(target_arch="wasm32")]
            let read = |e:std::io::Error| fail(WireKind::Dropped(e.to_string()));
            (&mut reader).take(probe).read_to_end(&mut bytes).map_err(read)?;
            if bytes.len() as u64 == probe && let Some(kind) = super::output_head::partial(&bytes)
                && (declared_large || matches!(kind, WireKind::PartialOutput {at_least, ..} if at_least > 2 << 20)) {
                return Err(fail(kind));
            }
            if declared_large {return Err(fail(WireKind::TooLarge {limit,length}));}
            if bytes.len() as u64 == probe {
                reader.take(limit+1-probe).read_to_end(&mut bytes).map_err(read)?;
                if bytes.len() as u64 > limit {return Err(fail(super::output_head::partial(&bytes).unwrap_or(WireKind::TooLarge{limit,length:None})));}
            }
            bytes
        } else {
            if length.is_some_and(|n| n > limit) {
                return Err(fail(WireKind::TooLarge { limit, length }));
            }
            // ureq enforces the limit on decoded bytes, not just the advertised transfer length.
            resp.body_mut().with_config().limit(limit).read_to_vec().map_err(|e| fail(transport(e)))?
        };
        Ok(Answer { status, content_type, location, body, first_byte })
    }

    pub(crate) fn dress<B>(&self, mut req: ureq::RequestBuilder<B>, headers: &[(&str, &str)], timeout: Option<Duration>) -> ureq::RequestBuilder<B> {
        if let Some(a) = &self.auth {
            req = req.header("Authorization", a);
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        req.config().timeout_global(timeout).build()
    }
}

impl Answer {
    /// A redirect or an error status as the error it is; `None` for a 2xx.
    fn refusal(&self, method: &'static str, path: &str) -> Option<WireError> {
        let kind = match self.status {
            200..=299 => return None,
            300..=399 => WireKind::Redirected { status: self.status, location: self.location.clone() },
            400.. => {
                let (tag, message) = explanation(&self.content_type, &self.body);
                WireKind::Status { status: self.status, tag, message }
            }
            _ => WireKind::NotOpenCode { status: self.status, what: "an informational answer".into() },
        };
        Some(WireError::new(method, path, kind))
    }

    /// OpenCode's JSON: a 2xx with a JSON body, or `Value::Null` for its empty answers (`204`).
    pub fn json(self, method: &'static str, path: &str) -> Result<(Value, usize, Instant), WireError> {
        if let Some(e) = self.refusal(method, path) {
            return Err(e);
        }
        let parsing = Instant::now();
        let json = is_json(&self.content_type);
        if self.body.is_empty() && (self.status == 204 || json) {
            return Ok((Value::Null, 0, parsing));
        }
        if !json {
            return Err(WireError::new(method, path, WireKind::NotOpenCode { status: self.status, what: kind_of(&self.content_type, self.body.is_empty()) }));
        }
        let value = serde_json::from_slice(&self.body)
            .map_err(|_| WireError::new(method, path, WireKind::NotOpenCode { status: self.status, what: "JSON that doesn't parse".into() }))?;
        Ok((value, self.body.len(), parsing))
    }

    /// A file's bytes as OpenCode serves them (`/api/fs/read`, `/api/experimental/media`). OpenCode
    /// never serves raw bytes as a web page (it sends HTML as `text/plain`), so `text/html` or no
    /// type at all is someone else's page.
    pub fn bytes(self, method: &'static str, path: &str) -> Result<Vec<u8>, WireError> {
        if let Some(e) = self.refusal(method, path) {
            return Err(e);
        }
        let t = self.content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        if t.is_empty() || t == "text/html" {
            return Err(WireError::new(method, path, WireKind::NotOpenCode { status: self.status, what: kind_of(&self.content_type, self.body.is_empty()) }));
        }
        Ok(self.body)
    }
}

/// What an answer that isn't OpenCode's was, in words.
pub(crate) fn kind_of(content_type: &str, empty: bool) -> String {
    match content_type.split(';').next().map(str::trim).unwrap_or("") {
        "" if empty => "an empty answer".to_owned(),
        "" => "an answer with no content type".to_owned(),
        t if t.eq_ignore_ascii_case("text/html") => "a web page (text/html)".to_owned(),
        t => format!("an answer of type {t}"),
    }
}
