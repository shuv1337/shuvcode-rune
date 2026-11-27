//! Connect links from `opencode pair`: `<server>/auth/connect/<code>`, where the code is
//! single-use and short-lived. Redeeming one asks the server for JSON and gets back a session
//! token, which is then the server's password (`Server::with_password`).

use std::time::Duration;

use serde_json::Value;

/// A pasted connect link split into the server's address and its code.
#[derive(Debug, PartialEq)]
pub struct Link {
    pub server: String,
    pub code: String,
}

/// Reads a connect link, or None when `value` isn't one. The server is the link's origin.
pub fn parse(value: &str) -> Option<Link> {
    let value = value.trim();
    let (scheme, rest) = value.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (host, path) = rest.split_once('/')?;
    if host.is_empty() || host.contains('@') {
        return None;
    }
    let code = path.strip_prefix("auth/connect/")?.trim_end_matches('/');
    if code.is_empty() || !code.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    Some(Link { server: format!("{scheme}://{host}"), code: code.to_owned() })
}

/// Trades the link's code for a session token. The code works once: a second try, or one after
/// it expires, is refused.
pub fn redeem(link: &Link) -> Result<String, String> {
    let agent: ureq::Agent = net_trust::agent_config().timeout_global(Some(Duration::from_secs(10))).http_status_as_error(false).build().into();
    let mut resp = agent
        .get(format!("{}/auth/connect/{}", link.server, link.code))
        .header("Accept", "application/json")
        .call()
        .map_err(|e| format!("Can't reach {}: {e}", link.server))?;
    match resp.status().as_u16() {
        200 => {}
        401 => return Err("That link has expired or was already used. Make a new one with `opencode pair`.".into()),
        n => return Err(format!("{} answered {n}", link.server)),
    }
    let text = resp.body_mut().read_to_string().map_err(|e| format!("Can't read {}'s answer: {e}", link.server))?;
    let body: Value = serde_json::from_str(&text).map_err(|e| format!("{} sent something unexpected: {e}", link.server))?;
    body["token"].as_str().filter(|t| !t.is_empty()).map(str::to_owned).ok_or_else(|| format!("{} sent no token", link.server))
}

