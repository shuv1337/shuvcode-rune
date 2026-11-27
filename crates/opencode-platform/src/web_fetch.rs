//! A plain web GET for pictures a reply shows from the web (a markdown image's URL, a link's
//! favicon), as Desktop's renderer loads an `<img>`: no credentials, a short timeout, at most
//! 8 MB, strictly globally routable public destinations (blocking loopback, RFC 1918 LAN,
//! CGNAT, link-local/cloud IMDS `169.254.169.254`, ULA, private DNS suffixes, redirects to
//! private addresses, and DNS rebinding), and only a successful reply counts.
//!
//! As the browser's cache does for Desktop, a picture read once is kept for the process (up to
//! 256 of them and 32 MB), so the timeline's sources chip and the Sources panel, each with its own
//! image store, read a favicon off the network once; a second reader of a URL still being read
//! waits for that read rather than starting its own. One agent serves every read, so its TLS
//! setup and connections are shared.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

#[cfg(target_arch = "wasm32")]
use ureq_web as ureq;

const MAX_ENTRIES: usize = 256;
const MAX_BYTES: usize = 32 << 20;
const MAX_REDIRECTS: usize = 5;

#[derive(Default)]
struct Cache {
    done: HashMap<String, Arc<[u8]>>,
    /// Oldest first, for eviction.
    order: VecDeque<String>,
    bytes: usize,
    /// URLs being read now.
    reading: Vec<String>,
}

static CACHE: OnceLock<(Mutex<Cache>, Condvar)> = OnceLock::new();

pub fn get(url: &str) -> Result<Vec<u8>, String> {
    if !browser_policy::is_public_http_url(url) {
        return Err(format!("GET {url}: refused non-public destination"));
    }
    let (lock, landed) = CACHE.get_or_init(Default::default);
    let mut cache = lock.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(bytes) = cache.done.get(url) {
            return Ok(bytes.to_vec());
        }
        if !cache.reading.iter().any(|u| u == url) {
            break;
        }
        cache = landed.wait(cache).unwrap_or_else(|e| e.into_inner());
    }
    cache.reading.push(url.to_owned());
    drop(cache);
    let read = fetch(url);
    let mut cache = lock.lock().unwrap_or_else(|e| e.into_inner());
    cache.reading.retain(|u| u != url);
    if let Ok(bytes) = &read {
        keep(&mut cache, url, bytes);
    }
    landed.notify_all();
    read
}

fn keep(cache: &mut Cache, url: &str, bytes: &[u8]) {
    if bytes.len() > MAX_BYTES {
        return;
    }
    while cache.done.len() >= MAX_ENTRIES || cache.bytes + bytes.len() > MAX_BYTES {
        let Some(old) = cache.order.pop_front() else { break };
        cache.bytes -= cache.done.remove(&old).map_or(0, |b| b.len());
    }
    cache.bytes += bytes.len();
    cache.done.insert(url.to_owned(), bytes.into());
    cache.order.push_back(url.to_owned());
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
struct PublicOnlyResolver(ureq::unversioned::resolver::DefaultResolver);

#[cfg(not(target_arch = "wasm32"))]
impl ureq::unversioned::resolver::Resolver for PublicOnlyResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ureq::unversioned::resolver::ResolvedSocketAddrs, ureq::Error> {
        let addrs = self.0.resolve(uri, config, timeout)?;
        if addrs.is_empty() || addrs.iter().any(|a| !browser_policy::is_public_ip(a.ip())) {
            return Err(ureq::Error::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "refused non-public IP resolution",
            )));
        }
        Ok(addrs)
    }
}

fn resolve_redirect(current: &str, location: &str) -> Option<String> {
    let loc = location.trim();
    if loc.is_empty() || loc.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    if loc.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("http://"))
        || loc.get(..8).is_some_and(|s| s.eq_ignore_ascii_case("https://"))
    {
        return Some(loc.to_owned());
    }
    // Reject non-HTTP(S) schemes (e.g. `file:`, `ftp:`, `javascript:`, `gopher:`) rather than
    // concatenating them onto `current` as a relative path.
    if let Some(colon) = loc.find(':')
        && loc[..colon].bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        && loc.as_bytes()[0].is_ascii_alphabetic()
        && !loc[..colon].contains(['/', '?', '#'])
    {
        return None;
    }
    let origin = browser_policy::destination_origin(current)?;
    let (scheme, _) = origin.split_once("://")?;
    if loc.starts_with("//") {
        return Some(format!("{scheme}:{loc}"));
    }
    if loc.starts_with('/') {
        return Some(format!("{origin}{loc}"));
    }
    let without_qf = current.split(['?', '#']).next().unwrap_or(current);
    let base_dir = match without_qf.rfind('/') {
        Some(idx) if idx >= origin.len() => &without_qf[..=idx],
        _ => return Some(format!("{origin}/{loc}")),
    };
    Some(format!("{base_dir}{loc}"))
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    let agent = AGENT.get_or_init(|| {
        let config = net_trust::agent_config()
            .timeout_global(Some(Duration::from_secs(15)))
            .http_status_as_error(false)
            .max_redirects(0)
            .proxy(None)
            .build();
        #[cfg(not(target_arch = "wasm32"))]
        {
            ureq::Agent::with_parts(
                config,
                ureq::unversioned::transport::DefaultConnector::default(),
                PublicOnlyResolver(ureq::unversioned::resolver::DefaultResolver::default()),
            )
        }
        #[cfg(target_arch = "wasm32")]
        {
            config.into()
        }
    });

    let mut current = url.to_owned();
    for hop in 0..=MAX_REDIRECTS {
        if !browser_policy::is_public_http_url(&current) {
            return Err(format!("GET {current}: refused non-public destination"));
        }
        let mut resp = agent.get(&current).call().map_err(|e| format!("GET {current}: {e}"))?;
        let status = resp.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            if hop == MAX_REDIRECTS {
                return Err(format!("GET {current}: too many redirects"));
            }
            let Some(loc) = resp.headers().get("location").and_then(|v| v.to_str().ok()) else {
                return Err(format!("GET {current}: redirect missing Location"));
            };
            let Some(next) = resolve_redirect(&current, loc) else {
                return Err(format!("GET {current}: invalid redirect Location"));
            };
            current = next;
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(format!("GET {current}: http status: {status}"));
        }
        return resp
            .body_mut()
            .with_config()
            .limit(8 << 20)
            .read_to_vec()
            .map_err(|e| format!("GET {current}: {e}"));
    }
    Err(format!("GET {url}: too many redirects"))
}
