//! Ticket-authenticated binary WebSocket upgrade client for remote browser streaming
//! (`packages/gui-plugins/src/browser/tunnel-client.ts`, opencode#66217).
//!
//! Strictly derives the `ws://` / `wss://` upgrade URL from the configured `Server::base`
//! (preserving any reverse-proxy base path), verifies `ticket.path == "/api/experimental/browser/tunnel"`,
//! passes the single-use ticket exclusively in `x-opencode-browser-ticket`, disables
//! `perMessageDeflate` and redirects, enforces verified TLS, and classifies auth/TLS/blocked/
//! protocol/transport errors explicitly without downgrading.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::protocol::WebSocketConfig;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Error as WsError, HandshakeError, WebSocket};

use super::super::stream_wire::MAX_WS_FRAME_BYTES;
use super::super::types::{
    BROWSER_TUNNEL_PATH, LocationRef, STREAM_PROTOCOL_VERSION, TunnelTicket, TunnelUpgradeError,
    TunnelUpgradeFailureKind,
};
use crate::client::{Server, encode_component};

pub const TUNNEL_TICKET_HEADER: &str = "x-opencode-browser-ticket";
pub const TUNNEL_SESSION_HEADER: &str = "x-opencode-browser-session";
pub const TUNNEL_CONNECTION_HEADER: &str = "x-opencode-browser-connection";
pub const TUNNEL_PROTOCOL_HEADER: &str = "x-opencode-browser-protocol";
pub const PREWARM_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

/// Derives the binary WebSocket upgrade URL solely from `server_url`, preserving any reverse-proxy
/// base path prefix, and strictly validates `reply_path` against `BROWSER_TUNNEL_PATH`
/// (`packages/gui-plugins/src/browser/tunnel-client.ts:41-86`).
pub fn resolve_browser_tunnel_url(
    server_url: &str,
    location: &LocationRef,
    reply_path: Option<&str>,
) -> Result<String, TunnelUpgradeError> {
    if let Some(p) = reply_path
        && p != BROWSER_TUNNEL_PATH
    {
        let clipped: String = p.chars().take(128).collect();
        return Err(TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Protocol,
            format!("Refused unexpected browser tunnel upgrade path: {clipped}"),
        ));
    }

    let trimmed = server_url.trim();
    let Some((scheme_raw, rest)) = trimmed.split_once("://") else {
        return Err(TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Protocol,
            "Unsupported server URL protocol for browser tunnel: invalid",
        ));
    };
    let scheme = scheme_raw.to_ascii_lowercase();
    let ws_scheme = match scheme.as_str() {
        "http" => "ws",
        "https" => "wss",
        other => {
            return Err(TunnelUpgradeError::new(
                TunnelUpgradeFailureKind::Protocol,
                format!("Unsupported server URL protocol for browser tunnel: {other}:"),
            ));
        }
    };

    let without_frag = rest.split('#').next().unwrap_or(rest);
    let without_query = without_frag.split('?').next().unwrap_or(without_frag);
    let (authority, raw_path) = match without_query.find('/') {
        Some(idx) => (&without_query[..idx], &without_query[idx..]),
        None => (without_query, ""),
    };
    if authority.is_empty() || authority.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Protocol,
            "Configured browser tunnel server URL has an invalid authority.",
        ));
    }
    if authority.contains('@') {
        return Err(TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Protocol,
            "Configured browser tunnel server URL must not contain credentials.",
        ));
    }
    let prefix = raw_path.trim_end_matches('/');
    if prefix.contains("..") || prefix.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Protocol,
            "Derived browser tunnel upgrade URL did not match configured server authority.",
        ));
    }
    let expected_pathname = format!("{prefix}{BROWSER_TUNNEL_PATH}");
    let mut url = format!(
        "{ws_scheme}://{authority}{expected_pathname}?location%5Bdirectory%5D={}",
        encode_component(&location.directory),
    );
    if let Some(ws) = &location.workspace_id
        && !ws.is_empty()
    {
        url.push_str("&location%5Bworkspace%5D=");
        url.push_str(&encode_component(ws));
    }
    Ok(url)
}

/// Opens one authenticated binary WebSocket using a single-use ticket in
/// `x-opencode-browser-ticket`. Disables `perMessageDeflate` and redirects, enforces verified
/// TLS, and returns explicit non-downgrading [`TunnelUpgradeError`]s on failure
/// (`packages/gui-plugins/src/browser/tunnel-client.ts:92-194`).
pub fn open_authenticated_websocket(
    server: &Server,
    location: &LocationRef,
    session_id: &str,
    connection_id: &str,
    ticket: &TunnelTicket,
) -> Result<WebSocket<MaybeTlsStream<TcpStream>>, TunnelUpgradeError> {
    if ticket.protocol != STREAM_PROTOCOL_VERSION {
        return Err(TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Protocol,
            format!("Unsupported browser stream tunnel protocol: {}.", ticket.protocol),
        ));
    }
    let ws_url = resolve_browser_tunnel_url(&server.base, location, Some(&ticket.path))?;
    let mut req = ws_url.as_str().into_client_request().map_err(|e| {
        TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Protocol,
            format!("Invalid browser tunnel WebSocket request: {e}"),
        )
    })?;

    // Forward configured Authorization (for reverse proxies / authenticated servers), never cookies.
    if let Some(auth) = server.auth_header()
        && let Ok(val) = HeaderValue::from_str(auth)
    {
        req.headers_mut().insert("Authorization", val);
    }
    insert_header(req.headers_mut(), TUNNEL_TICKET_HEADER, &ticket.ticket)?;
    insert_header(req.headers_mut(), TUNNEL_SESSION_HEADER, session_id)?;
    insert_header(req.headers_mut(), TUNNEL_CONNECTION_HEADER, connection_id)?;
    insert_header(
        req.headers_mut(),
        TUNNEL_PROTOCOL_HEADER,
        &ticket.protocol.to_string(),
    )?;

    let addrs = resolve_ws_addrs(&ws_url);
    if addrs.is_empty() {
        return Err(TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Transport,
            "Could not resolve browser stream tunnel server address.",
        ));
    }
    let mut last_err = None;
    let mut connected = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, PREWARM_WAIT_TIMEOUT) {
            Ok(s) => {
                connected = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let Some(tcp) = connected else {
        let msg = last_err.map_or_else(|| "connect failed".into(), |e| e.to_string());
        return Err(TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Transport,
            format!("Browser stream tunnel connection failed: {msg}"),
        ));
    };
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(PREWARM_WAIT_TIMEOUT)).ok();
    tcp.set_write_timeout(Some(PREWARM_WAIT_TIMEOUT)).ok();

    let mut cfg = WebSocketConfig::default();
    cfg.max_message_size = Some(MAX_WS_FRAME_BYTES);
    cfg.max_frame_size = Some(MAX_WS_FRAME_BYTES);
    cfg.read_buffer_size = 64 * 1024;
    cfg.write_buffer_size = 64 * 1024;

    let tls = ws_url.starts_with("wss:");
    match tungstenite::client_tls_with_config(req, tcp, Some(cfg), tls.then(net_trust::rustls_client_config).flatten().map(tungstenite::Connector::Rustls)) {
        Ok((ws, _resp)) => Ok(ws),
        Err(HandshakeError::Failure(err)) => Err(classify_ws_handshake_error(err)),
        Err(HandshakeError::Interrupted(_)) => Err(TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Transport,
            "Browser stream tunnel handshake was interrupted.",
        )),
    }
}

fn insert_header(
    headers: &mut tungstenite::http::HeaderMap,
    name: &'static str,
    value: &str,
) -> Result<(), TunnelUpgradeError> {
    let val = HeaderValue::from_str(value).map_err(|e| {
        TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Protocol,
            format!("Invalid header {name} for browser stream tunnel: {e}"),
        )
    })?;
    headers.insert(name, val);
    Ok(())
}

fn resolve_ws_addrs(url: &str) -> Vec<SocketAddr> {
    let Some((scheme, rest)) = url.split_once("://") else { return Vec::new() };
    let Some(authority) = rest.split(['/', '?']).next() else { return Vec::new() };
    let default_port = if scheme == "wss" || scheme == "https" { 443 } else { 80 };
    let host_port = if authority.starts_with('[') {
        if authority.contains("]:") {
            authority.to_owned()
        } else {
            format!("{authority}:{default_port}")
        }
    } else if authority.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        authority.to_owned()
    } else {
        format!("{authority}:{default_port}")
    };
    if let Ok(a) = host_port.parse::<SocketAddr>() {
        return vec![a];
    }
    let mut out = Vec::new();
    if let Ok(iter) = host_port.to_socket_addrs() {
        for a in iter {
            if !out.contains(&a) {
                out.push(a);
            }
        }
    }
    // Sort IPv4 before IPv6 for fast localhost loopback connect while preserving IPv6 fallback.
    out.sort_by_key(|a| u8::from(a.is_ipv6()));
    out
}

fn classify_ws_handshake_error(err: WsError) -> TunnelUpgradeError {
    match err {
        WsError::Http(resp) => {
            let status = resp.status().as_u16();
            if status == 401 || status == 403 {
                TunnelUpgradeError::with_status(
                    TunnelUpgradeFailureKind::Auth,
                    format!(
                        "Browser stream tunnel upgrade was rejected (HTTP {status}). Check server credentials and attachment state."
                    ),
                    status,
                )
            } else {
                TunnelUpgradeError::with_status(
                    TunnelUpgradeFailureKind::Blocked,
                    format!(
                        "Browser stream tunnel WebSocket upgrade was blocked (HTTP {status}). Ensure the server or reverse proxy permits WebSocket upgrades."
                    ),
                    status,
                )
            }
        }
        WsError::Tls(e) => TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Tls,
            format!("Browser stream tunnel TLS verification failed ({e}). Check the server certificate."),
        ),
        WsError::Protocol(e) => TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Protocol,
            format!("Browser stream tunnel protocol error during handshake: {e}"),
        ),
        WsError::Capacity(e) => TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Protocol,
            format!("Browser stream tunnel frame capacity exceeded: {e}"),
        ),
        WsError::Utf8(e) => TunnelUpgradeError::new(
            TunnelUpgradeFailureKind::Protocol,
            format!("Browser stream tunnel invalid UTF-8 during handshake: {e}"),
        ),
        other => {
            let msg = other.to_string();
            let upper = msg.to_ascii_uppercase();
            if upper.contains("CERT")
                || upper.contains("TLS")
                || upper.contains("SSL")
                || upper.contains("HOSTNAME")
                || upper.contains("SELF_SIGNED")
                || upper.contains("UNABLE_TO_VERIFY")
            {
                TunnelUpgradeError::new(
                    TunnelUpgradeFailureKind::Tls,
                    format!("Browser stream tunnel TLS verification failed ({msg}). Check the server certificate."),
                )
            } else {
                TunnelUpgradeError::new(
                    TunnelUpgradeFailureKind::Transport,
                    format!("Browser stream tunnel connection failed: {msg}"),
                )
            }
        }
    }
}
