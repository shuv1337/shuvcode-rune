//! Discovers the local OpenCode background service when `OPENCODE_SERVER_PASSWORD` is unset, where
//! OpenCode's own clients look (`@opencode/client` `service-probe.ts`, Desktop's
//! `background-service.ts`): the service's registration `<state>/opencode/service.json` (`url`,
//! `password`), with `<state>` = `XDG_STATE_HOME`, else `<home>/.local/state` on every platform
//! and `<home>` = `USERPROFILE` on Windows; `service-local.json` there for Desktop's local channel
//! (`OPENCODE_DESKTOP_SERVER_CHANNEL=local`). The CLI's own config (`hostname`, `port`,
//! `password` in `OPENCODE_CONFIG_DIR`, else `<config>/opencode/service.json`) comes after, for a
//! service that hasn't registered.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde_json::Value;

#[cfg(not(target_arch = "wasm32"))]
use crate::client::DEFAULT_SERVER;
use crate::client::Server;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalService {
    pub base: String,
    pub port: u16,
    pub password: Option<String>,
}

/// Parses a `service.json` written by `opencode serve --service`: the registration (`url`) or the
/// CLI's config (`hostname`, `port`).
pub fn parse_service_json(text: &str) -> Option<LocalService> {
    let v: Value = serde_json::from_str(text).ok()?;
    let (raw_host, port) = match v.get("url").and_then(Value::as_str) {
        Some(url) => {
            let authority = url.trim().strip_prefix("http://")?.split('/').next()?;
            let (host, port) = authority.rsplit_once(':')?;
            (host, port.parse::<u64>().ok()?)
        }
        None => (v.get("hostname").and_then(Value::as_str).unwrap_or("127.0.0.1"), v.get("port")?.as_u64()?),
    };
    let port = u16::try_from(port).ok().filter(|p| *p > 0)?;
    // shuv1337 fork: our shuvcode service binds the machine's Tailscale address, not loopback, so
    // any host its own `service.json` names is accepted (the file is the user's, mode 0600).
    let host = match raw_host.trim() {
        "" | "0.0.0.0" | "localhost" | "127.0.0.1" => "127.0.0.1",
        "::1" | "[::1]" => "[::1]",
        h if !h.contains(['/', '@', ' ']) => h,
        _ => return None,
    };
    let password = v
        .get("password")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    Some(LocalService { base: format!("http://{host}:{port}"), port, password })
}

/// The files to try, in order, from an environment lookup (see the module docs).
fn candidate_paths(var: impl Fn(&str) -> Option<OsString>) -> Vec<PathBuf> {
    let set = |k: &str| var(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    let home = rune::dirs::home_from(&var);
    let xdg = |k: &str, fallback: &[&str]| set(k).or_else(|| Some(fallback.iter().fold(home.clone()?, |p, part| p.join(part))));
    let local = var("OPENCODE_DESKTOP_SERVER_CHANNEL").is_some_and(|c| c == "local");
    let registration = if local { "service-local.json" } else { "service.json" };
    // shuv1337 fork: our local shuvcode service registers under `shuvcode/`, ahead of OpenCode's.
    [
        xdg("XDG_STATE_HOME", &[".local", "state"]).map(|d| d.join("shuvcode").join(registration)),
        xdg("XDG_STATE_HOME", &[".local", "state"]).map(|d| d.join("opencode").join(registration)),
        set("OPENCODE_CONFIG_DIR").map(|d| d.join("service.json")),
        xdg("XDG_CONFIG_HOME", &[".config"]).map(|d| d.join("shuvcode").join("service.json")),
        xdg("XDG_CONFIG_HOME", &[".config"]).map(|d| d.join("opencode").join("service.json")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn read_local_service_from(paths: &[PathBuf]) -> Option<LocalService> {
    paths.iter().find_map(|p| read_one(p))
}

fn read_one(path: &Path) -> Option<LocalService> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_service_json(&text)
}

/// Reads the first valid `service.json` where OpenCode's clients look.
pub fn read_local_service() -> Option<LocalService> {
    read_local_service_from(&candidate_paths(|k| std::env::var_os(k)))
}

fn local_port(base: &str) -> Option<u16> {
    let (scheme, rest) = base.trim_end_matches('/').split_once("://")?;
    if scheme != "http" {
        return None;
    }
    let host_port = rest.split('/').next()?;
    let (host, port) = host_port.rsplit_once(':')?;
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
        return None;
    }
    port.parse::<u16>().ok()
}

/// Returns the `service.json` password when `base` targets the same local port.
pub fn password_for_base(base: &str) -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    if let Some(password) = page_password(base) {
        return Some(password);
    }
    let svc = read_local_service()?;
    // shuv1337 fork: the registered service may be on a non-loopback (Tailscale) address.
    if svc.base == base.trim_end_matches('/') {
        return svc.password;
    }
    let port = local_port(base)?;
    (svc.port == port).then_some(svc.password).flatten()
}

/// The passwords a page was handed for its servers (`?password=` in its address), by base: a
/// browser has no `service.json` to read them from.
#[cfg(target_arch = "wasm32")]
static PAGE_PASSWORDS: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// Remembers `password` for the server at `base`, for every [`Server::new`] of it from now on.
#[cfg(target_arch = "wasm32")]
pub fn remember_page_password(base: &str, password: &str) {
    let base = base.trim_end_matches('/').to_owned();
    let mut all = PAGE_PASSWORDS.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|(b, _)| *b != base);
    all.push((base, password.to_owned()));
}

#[cfg(target_arch = "wasm32")]
fn page_password(base: &str) -> Option<String> {
    let base = base.trim_end_matches('/');
    PAGE_PASSWORDS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(b, _)| b == base).map(|(_, p)| p.clone())
}

/// Whether `server`, which accepts connections, answers as OpenCode's server.
#[cfg(not(target_arch = "wasm32"))]
fn answers(server: &Server) -> bool {
    server.get_json("/api/info", None, std::time::Duration::from_millis(500)).is_ok()
}

#[cfg(not(target_arch = "wasm32"))]
fn candidate_for_service(svc: &LocalService, media_refs: bool) -> Server {
    let mut candidate = Server::anonymous(&svc.base);
    if let Some(pw) = &svc.password {
        candidate = candidate.with_password(pw);
    }
    if media_refs {
        candidate = candidate.with_media_refs();
    }
    candidate
}

/// The server and whether it accepts connections (none when it wasn't tried). `registered_up`:
/// what a caller that has just tried the registered service's address found, so it isn't tried again.
#[cfg(not(target_arch = "wasm32"))]
fn discover_with_service(default: &Server, explicit_env_url: bool, svc: Option<LocalService>, registered_up: Option<bool>) -> (Server, Option<bool>) {
    if explicit_env_url {
        return (default.clone(), None);
    }
    // Official Desktop resolves its own server from `<state>/opencode/service.json`
    // (`packages/desktop/src/main/service/background-service.ts:48-86,130-153`,
    // `packages/client/src/service-probe.ts:39-90`), never preferring an unauthenticated or 401
    // server on `:4096` ahead of a registered service that answers with its credentials.
    let reg = svc.as_ref().map(|s| candidate_for_service(s, default.media_refs));
    // Each address is tried once. Where nothing listens a try costs its whole 250 ms on Windows
    // (`Server::reachable`), and each address was tried twice here and once more by the caller:
    // a launch with no server running learned so after three of them (790 ms).
    let reg_up = reg.as_ref().is_some_and(|candidate| registered_up.unwrap_or_else(|| candidate.reachable()));
    if let Some(candidate) = &reg
        && reg_up
        && answers(candidate)
    {
        return (candidate.clone(), Some(true));
    }
    let mut fallback = Server::new(DEFAULT_SERVER);
    if default.media_refs {
        fallback = fallback.with_media_refs();
    }
    let fallback_up = fallback.reachable();
    if fallback_up && answers(&fallback) {
        return (fallback, Some(true));
    }
    match reg {
        Some(candidate) if reg_up => (candidate, Some(true)),
        _ if fallback_up => (fallback, Some(true)),
        Some(candidate) => (candidate, Some(false)),
        None => (fallback, Some(false)),
    }
}

/// The background service `service.json` registers, as a server to talk to, with nothing asked
/// of it: none when `OPENCODE_SERVER_URL` names the server, or no registration is there. A
/// launch reads from it at once; whether it is OpenCode's is what its answers say, and one
/// that fails them is looked for again the long way ([`discover_local_server`]).
#[cfg(not(target_arch = "wasm32"))]
pub fn registered(default: &Server) -> Option<Server> {
    // Public standalone discovery API; opencode-kit has no app config.rs layer.
    #[allow(clippy::disallowed_methods)]
    let explicit_env_url = std::env::var("OPENCODE_SERVER_URL").ok().is_some_and(|v| !v.trim().is_empty());
    if explicit_env_url {
        return None;
    }
    read_local_service().map(|svc| candidate_for_service(&svc, default.media_refs))
}

/// Resolves the app's own local OpenCode server when `--server` was not passed: prefers a
/// registered background service in `service.json` when it answers `/api/info`, falls back to
/// `http://127.0.0.1:4096` (with `OPENCODE_SERVER_PASSWORD` if set) only when no registration
/// answers, and re-reads `service.json` even if an earlier probe moved to another port.
pub fn discover_local_server(default: &Server) -> Server {
    discover_local_reach(default, None).0
}

/// [`discover_local_server`], with whether the server it names accepts connections when finding
/// it tried that (the caller needn't try again). `registered_up`: what a caller that has just
/// tried the registered service's address found ([`registered`]).
pub fn discover_local_reach(default: &Server, registered_up: Option<bool>) -> (Server, Option<bool>) {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = registered_up;
        return (default.clone(), None);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Public standalone discovery API; opencode-kit has no app config.rs layer.
        #[allow(clippy::disallowed_methods)]
        let explicit_env_url = std::env::var("OPENCODE_SERVER_URL").ok().is_some_and(|v| !v.trim().is_empty());
        discover_with_service(default, explicit_env_url, read_local_service(), registered_up)
    }
}

