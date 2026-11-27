//! Native client MCP WebSocket worker: dials, polls socket + outbox, and reconnects with backoff.

use std::io::ErrorKind;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use rune::Waker;
use serde_json::Value;
use tungstenite::client::IntoClientRequest;
use tungstenite::http::HeaderValue;
use tungstenite::protocol::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use super::protocol::{handle_frame, outbound_message};
use super::{CONNECT_TIMEOUT, ClientMcpOptions, Conn, McpCall, Outbound, Outbox, Shared, lock, transition};
use crate::client::Server;

/// How long a read waits for the rest of a frame once the socket said it has data.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// Waits out `delay` before the next dial; false when the socket was closed meanwhile.
fn wait_backoff(shared: &Shared, delay: Duration, unsupported: bool, waker: &Waker) -> bool {
    if !transition(shared, |_| Some(Conn::Waiting { unsupported }), waker) {
        return false;
    }
    let st = lock(&shared.state);
    let (st, _) = shared.cv.wait_timeout(st, delay).unwrap_or_else(|e| e.into_inner());
    !st.conn.closed()
}

enum ConnectOutcome {
    Connected(Box<WebSocket<MaybeTlsStream<TcpStream>>>),
    Unsupported,
    Failed,
}

fn dial(server: &Server, url: &str) -> ConnectOutcome {
    let Ok(mut req) = url.into_client_request() else { return ConnectOutcome::Failed };
    if let Some(auth) = &server.auth
        && let Ok(val) = HeaderValue::from_str(auth)
    {
        req.headers_mut().insert("Authorization", val);
    }
    let Some(addr) = resolve_ws_addr(url) else { return ConnectOutcome::Failed };
    let Ok(tcp) = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) else { return ConnectOutcome::Failed };
    tcp.set_nodelay(true).ok();
    tcp.set_read_timeout(Some(CONNECT_TIMEOUT)).ok();
    tcp.set_write_timeout(Some(CONNECT_TIMEOUT)).ok();
    // A `wss://` server's certificate is checked as the operating system checks it (`net_trust`);
    // a `ws://` one (the local server) builds no TLS configuration.
    let tls = url.starts_with("wss:");
    match tungstenite::client_tls_with_config(req, tcp, None, tls.then(net_trust::rustls_client_config).flatten().map(tungstenite::Connector::Rustls)) {
        Ok((ws, _)) => ConnectOutcome::Connected(Box::new(ws)),
        Err(tungstenite::HandshakeError::Failure(tungstenite::Error::Http(resp)))
            if matches!(resp.status().as_u16(), 404 | 405 | 501) =>
        {
            ConnectOutcome::Unsupported
        }
        Err(_) => ConnectOutcome::Failed,
    }
}

fn resolve_ws_addr(url: &str) -> Option<SocketAddr> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?']).next()?;
    let default_port = if scheme == "wss" || scheme == "https" { 443 } else { 80 };
    let host_port = if authority.starts_with('[') {
        if authority.contains("]:") { authority.to_owned() } else { format!("{authority}:{default_port}") }
    } else if authority.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) {
        authority.replace("localhost", "127.0.0.1")
    } else {
        format!("{}:{default_port}", authority.replace("localhost", "127.0.0.1"))
    };
    if let Ok(a) = host_port.parse() {
        return Some(a);
    }
    host_port.to_socket_addrs().ok()?.next()
}

fn tcp_ref(ws: &WebSocket<MaybeTlsStream<TcpStream>>) -> &TcpStream {
    match ws.get_ref() {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::Rustls(s) => s.get_ref(),
        #[allow(unreachable_patterns)]
        _ => unreachable!(),
    }
}

pub(super) fn run_worker(server: Server, options: ClientMcpOptions, calls: Sender<McpCall>, waker: Waker, shared: Arc<Shared>) {
    let url = server.client_mcp_url(&options.directory, &options.name);
    let mut delay = options.reconnect_min;

    loop {
        if !transition(&shared, |_| Some(Conn::Dialing), &waker) {
            return;
        }
        let outcome = dial(&server, &url);

        let mut ws = match outcome {
            ConnectOutcome::Connected(ws) => *ws,
            ConnectOutcome::Unsupported => {
                if !wait_backoff(&shared, options.unsupported_retry, true, &waker) {
                    return;
                }
                continue;
            }
            ConnectOutcome::Failed => {
                if !wait_backoff(&shared, delay, false, &waker) {
                    return;
                }
                delay = delay.saturating_mul(2).min(options.reconnect_max);
                continue;
            }
        };

        // The worker sleeps on the socket and on this poller's nudge: a reply or notice queued by
        // the app wakes it, as data arriving does; nothing else does.
        let Ok(wake) = polling::Poller::new() else {
            send_normal_close(&mut ws);
            if !wait_backoff(&shared, delay, false, &waker) {
                return;
            }
            continue;
        };
        let wake = Arc::new(wake);
        // SAFETY: the socket is removed from the poller (`delete` below) before `ws` closes it.
        #[allow(unsafe_code)]
        let added = unsafe { wake.add(tcp_ref(&ws), polling::Event::readable(0)) };
        if added.is_err() {
            send_normal_close(&mut ws);
            if !wait_backoff(&shared, delay, false, &waker) {
                return;
            }
            continue;
        }
        let (tx, out_rx) = channel::<Outbound>();
        let out = Outbox { tx, wake: wake.clone() };
        let tcp = tcp_ref(&ws).try_clone().ok();
        let open = out.clone();
        if !transition(&shared, move |_| Some(Conn::Open { tcp, out: open, ready: None, list_changed: true }), &waker) {
            wake.delete(tcp_ref(&ws)).ok();
            send_normal_close(&mut ws);
            return;
        }

        let mut initialized = false;
        let disconnected = serve_socket(&mut ws, &options, &calls, &waker, &shared, &out, &out_rx, &mut initialized);
        wake.delete(tcp_ref(&ws)).ok();
        if initialized {
            delay = options.reconnect_min;
        }

        let stopping = matches!(disconnected, LoopExit::ReceiverDropped) || !transition(&shared, |_| Some(Conn::Waiting { unsupported: false }), &waker);
        if stopping {
            lock(&shared.state).conn = Conn::Closed;
            send_normal_close(&mut ws);
            waker();
            return;
        }

        if !wait_backoff(&shared, delay, false, &waker) {
            return;
        }
        delay = delay.saturating_mul(2).min(options.reconnect_max);
    }
}

enum LoopExit {
    SocketClosed,
    ReceiverDropped,
}

#[allow(clippy::too_many_arguments)]
fn serve_socket(
    ws: &mut WebSocket<MaybeTlsStream<TcpStream>>,
    options: &ClientMcpOptions,
    calls: &Sender<McpCall>,
    waker: &Waker,
    shared: &Shared,
    out: &Outbox,
    out_rx: &Receiver<Outbound>,
    initialized: &mut bool,
) -> LoopExit {
    let mut in_flight: usize = 0;
    tcp_ref(ws).set_write_timeout(Some(CONNECT_TIMEOUT)).ok();
    tcp_ref(ws).set_read_timeout(Some(READ_TIMEOUT)).ok();
    let mut events = polling::Events::new();

    loop {
        if lock(&shared.state).conn.closed() || !drain_outbound(ws, out_rx, &mut in_flight) {
            return LoopExit::SocketClosed;
        }

        match ws.read() {
            Ok(Message::Text(text)) => {
                if !handle_frame(&mut |m| send_json(ws, m), text.as_str(), options, calls, waker, shared, out, &mut in_flight, initialized) {
                    return LoopExit::ReceiverDropped;
                }
            }
            Ok(Message::Binary(bytes)) => {
                if let Ok(text) = std::str::from_utf8(&bytes)
                    && !handle_frame(&mut |m| send_json(ws, m), text, options, calls, waker, shared, out, &mut in_flight, initialized)
                {
                    return LoopExit::ReceiverDropped;
                }
            }
            Ok(Message::Ping(_)) => {
                if ws.flush().is_err() {
                    return LoopExit::SocketClosed;
                }
            }
            Ok(Message::Pong(_) | Message::Frame(_)) => {}
            Ok(Message::Close(_)) => return LoopExit::SocketClosed,
            Err(tungstenite::Error::Io(ref err))
                if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) =>
            {
                // Everything read: sleep until the socket has data or the app queued a reply.
                // `modify` re-arms the one-shot interest; a nudge sent meanwhile is kept.
                events.clear();
                if out.wake.modify(tcp_ref(ws), polling::Event::readable(0)).is_err() || out.wake.wait(&mut events, None).is_err() {
                    return LoopExit::SocketClosed;
                }
            }
            Err(_) => return LoopExit::SocketClosed,
        }
    }
}

fn drain_outbound(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>, out_rx: &Receiver<Outbound>, in_flight: &mut usize) -> bool {
    let mut wrote = false;
    loop {
        let Ok(out) = out_rx.try_recv() else { break };
        let msg = outbound_message(out, in_flight);
        if ws.write(Message::Text(msg.to_string().into())).is_err() {
            return false;
        }
        wrote = true;
    }
    !wrote || ws.flush().is_ok()
}

fn send_json(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>, msg: &Value) -> bool {
    ws.send(Message::Text(msg.to_string().into())).is_ok()
}

fn send_normal_close(ws: &mut WebSocket<MaybeTlsStream<TcpStream>>) {
    tcp_ref(ws).set_write_timeout(Some(Duration::from_millis(250))).ok();
    ws.close(Some(CloseFrame { code: CloseCode::Normal, reason: "".into() })).ok();
    ws.flush().ok();
}
