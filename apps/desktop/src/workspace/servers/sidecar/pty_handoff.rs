//! Persistent terminal handoff across background service replacements
//! (`packages/client/src/pty-handoff.ts`). Best-effort: any failure falls back to replacing the
//! service without a handoff.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::service::{Info, auth_header, normalize_origin};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Source {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    pid: u32,
    url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Handoff {
    directory: String,
    #[serde(rename = "instanceID")]
    instance_id: String,
    ticket: String,
    expires_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sidecar {
    source: Source,
    handoff: Option<Handoff>,
    expires_at: i64,
}

fn handoff_path(file: &Path) -> PathBuf {
    let mut name = file.as_os_str().to_os_string();
    name.push(".pty-handoff");
    PathBuf::from(name)
}

fn same_source(source: &Source, info: &Info) -> bool {
    source.id == info.id && source.pid == info.pid && source.url == info.url
}

fn read(file: &Path) -> Option<Sidecar> {
    let text = std::fs::read_to_string(handoff_path(file)).ok()?;
    serde_json::from_str::<Sidecar>(&text).ok()
}

fn now_ms() -> i64 {
    opencode_platform::wall_ms()
}

fn post_pty(info: &Info, path: &str, timeout: Duration) -> Result<Value, String> {
    let base = normalize_origin(&info.url).unwrap_or_else(|| info.url.trim_end_matches('/').to_owned());
    let agent: ureq::Agent = net_trust::agent_config_for(&base)
        .timeout_global(Some(timeout))
        .http_status_as_error(true)
        .max_redirects(0)
        .proxy(None)
        .build()
        .into();
    let mut req = agent.post(format!("{base}{path}")).header("Content-Type", "application/json");
    if let Some(auth) = auth_header(info.password.as_deref()) {
        req = req.header("Authorization", &auth);
    }
    let mut resp = req.send(&b"{}"[..]).map_err(|e| e.to_string())?;
    let bytes = resp.body_mut().with_config().limit(1 << 20).read_to_vec().map_err(|e| e.to_string())?;
    if bytes.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn parse_handoff(body: &Value) -> Result<Option<Handoff>, String> {
    let Some(obj) = body.as_object() else {
        return Err("Invalid persistent terminal handoff response".into());
    };
    let Some(val) = obj.get("handoff") else {
        return Err("Invalid persistent terminal handoff response".into());
    };
    if val.is_null() {
        return Ok(None);
    }
    let handoff: Handoff = serde_json::from_value(val.clone()).map_err(|_| "Invalid persistent terminal handoff".to_owned())?;
    if handoff.expires_at <= now_ms() {
        return Err("Expired persistent terminal handoff".into());
    }
    Ok(Some(handoff))
}

fn publish(file: &Path, info: &Info, handoff: Option<Handoff>) {
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).ok();
    let tag: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let mut tmp_name = file.as_os_str().to_os_string();
    tmp_name.push(format!(".pty-handoff.{tag}.tmp"));
    let tmp = PathBuf::from(tmp_name);
    let expires_at = handoff.as_ref().map_or_else(|| now_ms() + 30_000, |h| h.expires_at);
    let sidecar = Sidecar {
        source: Source { id: info.id.clone(), pid: info.pid, url: info.url.clone() },
        handoff,
        expires_at,
    };
    let Ok(bytes) = serde_json::to_vec(&sidecar) else { return };
    if let Some(parent) = tmp.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let wrote = opts.open(&tmp).and_then(|mut f| std::io::Write::write_all(&mut f, &bytes)).is_ok();
    if wrote && std::fs::rename(&tmp, handoff_path(file)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// `PtyHandoff.prepare` (`packages/client/src/pty-handoff.ts:19-46`).
pub(super) fn prepare(file: &Path, info: &Info, timeout: Duration) {
    if let Some(existing) = read(file)
        && existing.expires_at > now_ms()
        && same_source(&existing.source, info)
    {
        return;
    }
    let handoff = match post_pty(info, "/api/experimental/persistent-pty/handoff", timeout).and_then(|v| parse_handoff(&v)) {
        Ok(h) => Some(h),
        Err(_) => {
            if let Some(concurrent) = read(file)
                && concurrent.expires_at > now_ms()
                && same_source(&concurrent.source, info)
            {
                return;
            }
            let _ = post_pty(info, "/api/experimental/persistent-pty/shutdown", timeout);
            Some(None)
        }
    };
    if let Some(handoff) = handoff {
        publish(file, info, handoff);
    }
}

/// `PtyHandoff.environment` (`packages/client/src/pty-handoff.ts:74-84`): adds
/// `OPENCODE_PTY_HANDOFF` to `base_env` when a valid handoff ticket matches the replaced owner.
pub(super) fn environment(file: &Path, base_env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut env = base_env.clone();
    let Some(record) = read(file) else {
        env.remove("OPENCODE_PTY_HANDOFF");
        return env;
    };
    let current = std::fs::read_to_string(file).ok().and_then(|t| serde_json::from_str::<Info>(&t).ok());
    let valid = record.expires_at > now_ms() && current.as_ref().is_none_or(|c| same_source(&record.source, c));
    if valid && let Some(handoff) = &record.handoff && let Ok(json) = serde_json::to_string(handoff) {
        env.insert("OPENCODE_PTY_HANDOFF".into(), json);
    } else {
        env.remove("OPENCODE_PTY_HANDOFF");
    }
    env
}

/// `PtyHandoff.complete` (`packages/client/src/pty-handoff.ts:86-89`).
pub(super) fn complete(file: &Path, info: &Info) {
    if let Some(record) = read(file)
        && !same_source(&record.source, info)
    {
        clear(file);
    }
}

/// `PtyHandoff.clear` (`packages/client/src/pty-handoff.ts:91-93`).
pub(super) fn clear(file: &Path) {
    let _ = std::fs::remove_file(handoff_path(file));
}
