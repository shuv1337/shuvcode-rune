//! Authenticated, redirect-forbidding, abortable streaming reader for remote workspace
//! (`/api/fs/read/*`) and artifact (`/api/experimental/artifacts/<id>/view/*`) browser previews
//! (`#63002`: `remote-preview.ts`, `artifact-preview.ts`).
//!
//! Security & resource-control invariants (`notes/review-rd281.md` PR 2 Blocker #3 & 2nd read #3):
//! - Uses a dedicated per-request `ureq::Agent` (`max_redirects(0)`, `max_idle_connections(0)`,
//!   `http_status_as_error(false)`, loopback proxy bypass, resolve 5 s, connect 10 s, header 15 s,
//!   per-read 20 s, global 120 s) registered in the tab's [`PreviewAbort`] handle so closing a tab
//!   or signing out immediately shuts down every connected socket (`ureq::Agent::shutdown`) AND
//!   wakes any worker waiting in DNS resolution (`AbortablePreviewResolver`).
//! - When a tab closes during a slow OS `getaddrinfo` call, the worker wakes in `< 1 ms` and
//!   returns to `PreviewFetchPool` immediately (releasing its worker, tab, and server slots); the
//!   detached OS lookup finishes in the background under a separate bounded semaphore
//!   (`MAX_DNS_THREADS = 16`).
//! - Sends `Accept-Encoding: identity` and ONLY `self.auth` (owning session's server credentials).
//! - Streams in 64 KiB chunks, caps bytes at `min(max_bytes, content_length)`, and requires exact
//!   `Content-Length` completion when declared.

use std::io::Read as _;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs as _};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ureq::config::{Config, IpFamily};
use ureq::http::Uri;
use ureq::unversioned::resolver::{ArrayVec, DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{DefaultConnector, NextTimeout};

use super::{Server, http};

/// Header metadata observed from a preview `HEAD` or `GET` request.
#[derive(Clone, Debug)]
pub struct PreviewHttpHead {
    pub status: u16,
    pub content_type: String,
    pub content_length: Option<u64>,
    pub content_range: Option<String>,
}

const PREVIEW_RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
const PREVIEW_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const PREVIEW_HEADER_TIMEOUT: Duration = Duration::from_secs(15);
const PREVIEW_PER_READ_TIMEOUT: Duration = Duration::from_secs(20);
const PREVIEW_GLOBAL_TIMEOUT: Duration = Duration::from_secs(120);
const PREVIEW_CHUNK_BYTES: usize = 64 * 1024;
/// Separate global & per-host caps on detached OS `getaddrinfo` helper threads so repeated
/// open/close against a stalling DNS hostname can neither starve `PreviewFetchPool`, exhaust
/// DNS slots for other hostnames, nor leak unbounded OS threads.
const MAX_DNS_THREADS_GLOBAL: usize = 16;
const MAX_DNS_THREADS_PER_HOST: usize = 4;

#[derive(Default)]
struct DnsThreads {
    total: usize,
    by_host: std::collections::HashMap<String, usize>,
}

fn dns_threads() -> &'static Mutex<DnsThreads> {
    static STATE: std::sync::OnceLock<Mutex<DnsThreads>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| Mutex::new(DnsThreads::default()))
}

struct DnsThreadPermit {
    host: String,
}

impl Drop for DnsThreadPermit {
    fn drop(&mut self) {
        let mut g = dns_threads().lock().unwrap_or_else(PoisonError::into_inner);
        g.total = g.total.saturating_sub(1);
        if let Some(n) = g.by_host.get_mut(&self.host) {
            if *n <= 1 {
                g.by_host.remove(&self.host);
            } else {
                *n -= 1;
            }
        }
    }
}

fn try_acquire_dns_thread(host: &str) -> Option<DnsThreadPermit> {
    let key = host.to_ascii_lowercase();
    let mut g = dns_threads().lock().unwrap_or_else(PoisonError::into_inner);
    let host_n = g.by_host.get(&key).copied().unwrap_or(0);
    if g.total >= MAX_DNS_THREADS_GLOBAL || host_n >= MAX_DNS_THREADS_PER_HOST {
        return None;
    }
    g.total += 1;
    *g.by_host.entry(key.clone()).or_default() += 1;
    Some(DnsThreadPermit { host: key })
}

#[derive(Default)]
struct DnsWaitState {
    done: Option<std::io::Result<Vec<SocketAddr>>>,
    aborted: bool,
}

type DnsSlot = Arc<(Mutex<DnsWaitState>, Condvar)>;

#[derive(Default)]
struct AbortState {
    aborted: bool,
    agents: Vec<(u64, ureq::Agent)>,
    dns_slots: Vec<(u64, DnsSlot)>,
}

/// Shared cancellation handle owned by a preview tab binding: calling [`PreviewAbort::abort`]
/// marks the tab aborted, immediately wakes any worker blocked in DNS resolution, and immediately
/// shuts down every in-flight HTTP socket via `ureq::Agent::shutdown`.
#[derive(Clone, Default)]
pub struct PreviewAbort {
    inner: Arc<Mutex<AbortState>>,
}

static NEXT_ABORT_TOKEN: AtomicU64 = AtomicU64::new(1);

impl PreviewAbort {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_aborted(&self) -> bool {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).aborted
    }

    pub fn abort(&self) {
        let (agents, dns_slots): (Vec<ureq::Agent>, Vec<DnsSlot>) = {
            let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            g.aborted = true;
            let agents = g.agents.drain(..).map(|(_, a)| a).collect();
            let dns = g.dns_slots.drain(..).map(|(_, s)| s).collect();
            (agents, dns)
        };
        for slot in dns_slots {
            let mut st = slot.0.lock().unwrap_or_else(PoisonError::into_inner);
            st.aborted = true;
            drop(st);
            slot.1.notify_all();
        }
        for agent in agents {
            agent.shutdown();
        }
    }

    fn register_agent(&self, agent: &ureq::Agent) -> Option<AgentLease> {
        let token = NEXT_ABORT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if g.aborted {
            drop(g);
            agent.clone().shutdown();
            return None;
        }
        g.agents.push((token, agent.clone()));
        Some(AgentLease { inner: Arc::clone(&self.inner), token })
    }

    fn register_dns(&self, slot: &DnsSlot) -> Option<DnsSlotLease> {
        let token = NEXT_ABORT_TOKEN.fetch_add(1, Ordering::Relaxed);
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if g.aborted {
            return None;
        }
        g.dns_slots.push((token, Arc::clone(slot)));
        Some(DnsSlotLease { inner: Arc::clone(&self.inner), token })
    }
}

struct AgentLease {
    inner: Arc<Mutex<AbortState>>,
    token: u64,
}

impl Drop for AgentLease {
    fn drop(&mut self) {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(pos) = g.agents.iter().position(|(t, _)| *t == self.token) {
            let (_, agent) = g.agents.swap_remove(pos);
            drop(g);
            agent.shutdown();
        }
    }
}

struct DnsSlotLease {
    inner: Arc<Mutex<AbortState>>,
    token: u64,
}

impl Drop for DnsSlotLease {
    fn drop(&mut self) {
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(pos) = g.dns_slots.iter().position(|(t, _)| *t == self.token) {
            g.dns_slots.swap_remove(pos);
        }
    }
}

struct AbortablePreviewResolver {
    abort: PreviewAbort,
}

impl std::fmt::Debug for AbortablePreviewResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AbortablePreviewResolver")
    }
}

impl Resolver for AbortablePreviewResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        if self.abort.is_aborted() {
            return Err(ureq::Error::HostNotFound);
        }
        let Some(scheme) = uri.scheme() else {
            return Err(ureq::Error::HostNotFound);
        };
        let Some(authority) = uri.authority() else {
            return Err(ureq::Error::HostNotFound);
        };
        let Some(addr_str) = DefaultResolver::host_and_port(scheme, authority) else {
            return Err(ureq::Error::HostNotFound);
        };
        let raw_host = authority.host().trim_start_matches('[').trim_end_matches(']');
        let port = authority
            .port_u16()
            .or_else(|| match scheme.as_str() {
                "http" => Some(80),
                "https" => Some(443),
                _ => None,
            })
            .ok_or(ureq::Error::HostNotFound)?;

        // Literal IP fast path (e.g. 127.0.0.1, ::1): zero DNS, zero helper thread.
        if let Ok(ip) = raw_host.parse::<IpAddr>() {
            return filter_addrs(vec![SocketAddr::new(ip, port)], config.ip_family());
        }

        let Some(dns_permit) = try_acquire_dns_thread(raw_host) else {
            return Err(ureq::Error::Timeout(timeout.reason));
        };
        let slot: DnsSlot = Arc::new((Mutex::new(DnsWaitState::default()), Condvar::new()));
        let Some(_lease) = self.abort.register_dns(&slot) else {
            return Err(ureq::Error::HostNotFound);
        };

        let slot_bg = Arc::clone(&slot);
        let spawn = std::thread::Builder::new()
            .name("preview-dns".into())
            .spawn(move || {
                let _permit = dns_permit;
                let res = addr_str.to_socket_addrs().map(|iter| iter.collect::<Vec<_>>());
                let mut st = slot_bg.0.lock().unwrap_or_else(PoisonError::into_inner);
                st.done = Some(res);
                drop(st);
                slot_bg.1.notify_all();
            });
        if spawn.is_err() {
            return Err(ureq::Error::HostNotFound);
        }

        let wait_limit = if timeout.after.is_not_happening() {
            PREVIEW_RESOLVE_TIMEOUT
        } else {
            (*timeout.after).min(PREVIEW_RESOLVE_TIMEOUT)
        };
        let deadline = Instant::now() + wait_limit;
        let mut st = slot.0.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if st.aborted || self.abort.is_aborted() {
                return Err(ureq::Error::HostNotFound);
            }
            if let Some(res) = st.done.take() {
                return match res {
                    Ok(addrs) => filter_addrs(addrs, config.ip_family()),
                    Err(err) => Err(ureq::Error::Io(err)),
                };
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(ureq::Error::Timeout(timeout.reason));
            }
            let (next, _) = slot
                .1
                .wait_timeout(st, deadline.saturating_duration_since(now))
                .unwrap_or_else(PoisonError::into_inner);
            st = next;
        }
    }
}

fn filter_addrs(addrs: Vec<SocketAddr>, family: IpFamily) -> Result<ResolvedSocketAddrs, ureq::Error> {
    let mut result = ArrayVec::from_fn(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
    for addr in addrs
        .into_iter()
        .filter(|a| match family {
            IpFamily::Any => true,
            IpFamily::Ipv4Only => a.is_ipv4(),
            IpFamily::Ipv6Only => a.is_ipv6(),
        })
        .take(16)
    {
        result.push(addr);
    }
    if result.is_empty() {
        Err(ureq::Error::HostNotFound)
    } else {
        Ok(result)
    }
}

impl Server {
    fn preview_agent(&self, abort: &PreviewAbort) -> ureq::Agent {
        let cfg = net_trust::agent_config_for(&self.base)
            .http_status_as_error(false)
            .max_redirects(0)
            .max_idle_connections(0)
            .timeout_resolve(Some(PREVIEW_RESOLVE_TIMEOUT))
            .timeout_connect(Some(PREVIEW_CONNECT_TIMEOUT))
            .timeout_recv_response(Some(PREVIEW_HEADER_TIMEOUT))
            .timeout_per_read(Some(PREVIEW_PER_READ_TIMEOUT))
            .timeout_global(Some(PREVIEW_GLOBAL_TIMEOUT));
        let built = if http::loopback(&self.base) {
            cfg.proxy(None).build()
        } else {
            cfg.build()
        };
        ureq::Agent::with_parts(
            built,
            DefaultConnector::default(),
            AbortablePreviewResolver { abort: abort.clone() },
        )
    }

    /// Streams a `GET` or `HEAD` preview response from `path` with optional `Range` header,
    /// calling `on_head` first; if `on_head` returns `true` and `method == "GET"`, streams 64 KiB
    /// chunks into `on_chunk` up to `min(max_bytes, content_length)`. Immediately aborts on
    /// `abort.abort()` (including mid-DNS), resolve/connect/header/per-read/global timeout.
    /// Returns `true` only on clean completion.
    pub fn preview_stream(
        &self,
        method: &str,
        path: &str,
        range: Option<&str>,
        max_bytes: u64,
        abort: &PreviewAbort,
        on_head: impl FnOnce(&PreviewHttpHead) -> bool,
        mut on_chunk: impl FnMut(&[u8]) -> bool,
    ) -> bool {
        if abort.is_aborted() {
            return false;
        }
        let agent = self.preview_agent(abort);
        let Some(_lease) = abort.register_agent(&agent) else {
            return false;
        };
        let url = format!("{}{path}", self.base);
        let is_head = method.eq_ignore_ascii_case("HEAD");
        let mut resp = if is_head {
            let mut req = agent.head(&url).header("Accept-Encoding", "identity");
            if let Some(auth) = &self.auth {
                req = req.header("Authorization", auth);
            }
            if let Some(r) = range {
                req = req.header("Range", r);
            }
            let Ok(r) = req.call() else {
                return false;
            };
            r
        } else {
            let mut req = agent.get(&url).header("Accept-Encoding", "identity");
            if let Some(auth) = &self.auth {
                req = req.header("Authorization", auth);
            }
            if let Some(r) = range {
                req = req.header("Range", r);
            }
            let Ok(r) = req.call() else {
                return false;
            };
            r
        };

        if abort.is_aborted() {
            return false;
        }
        let head = extract_head(&resp);
        if !on_head(&head) {
            return false;
        }
        if is_head {
            return true;
        }

        let stream_cap = head.content_length.map_or(max_bytes, |cl| cl.min(max_bytes));
        let mut reader = resp.body_mut().as_reader();
        let mut buf = vec![0u8; PREVIEW_CHUNK_BYTES];
        let mut total = 0u64;
        loop {
            if abort.is_aborted() {
                return false;
            }
            match reader.read(&mut buf) {
                Ok(0) => return !abort.is_aborted() && head.content_length.is_none_or(|cl| total == cl),
                Ok(n) => {
                    total = total.saturating_add(n as u64);
                    if total > stream_cap {
                        return false;
                    }
                    if !on_chunk(&buf[..n]) {
                        return false;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
    }
}

fn extract_head(resp: &ureq::http::Response<ureq::Body>) -> PreviewHttpHead {
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let content_length = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            let t = s.trim();
            (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
                .then(|| t.parse::<u64>().ok())
                .flatten()
        });
    let content_range = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    PreviewHttpHead {
        status,
        content_type,
        content_length,
        content_range,
    }
}
