//! The part of checking and downloading every platform shares: read the channel's manifest,
//! decide (`protocol::decide`), and stream the chosen archive to disk, hashing it as it lands
//! and reporting its progress. Runs on the updater's worker thread.
//!
//! Every request names itself `User-Agent: opencode/<channel>/<version>` and nothing else about
//! the install (no query string, no install id), and the manifest is revalidated with
//! `If-None-Match` and the last `ETag` (`release:docs/protocol.md`, "URLs").

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use sha2::{Digest as _, Sha256};

use super::feed;
use super::protocol::{self, Decision, Target};

/// The facts a check needs.
#[derive(Clone, Debug)]
pub struct Facts {
    /// The running build's version.
    pub version: String,
    /// The feed channel.
    pub channel: String,
    /// The feed's base URL.
    pub service: String,
    /// The folder downloads and the staged release live in.
    pub updates: PathBuf,
}

/// What the last check chose to install.
#[derive(Clone, Debug)]
pub struct Found {
    pub version: String,
    pub target: Target,
}

pub struct Engine {
    pub facts: Facts,
    rollout_bucket: u8,
    found: Mutex<Option<Found>>,
    cache: Mutex<ManifestCache>,
}

/// The last manifest the service sent and its `ETag`, sent back as `If-None-Match` so an
/// unchanged manifest comes back as `304 Not Modified`.
#[derive(Default)]
struct ManifestCache {
    url: String,
    etag: Option<String>,
    body: Option<String>,
}

const MANIFEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(3600);

fn manifest_timeout() -> Duration {
    #[cfg(debug_assertions)]
    if super::build_channel() == "local"
        && let Some(ms) = feed::env("OPENCODE_RUNE_MANIFEST_TIMEOUT_MS").and_then(|s| s.parse::<u64>().ok())
    {
        return Duration::from_millis(ms.max(50));
    }
    MANIFEST_TIMEOUT
}

impl Engine {
    pub fn new(facts: Facts) -> Self {
        let rollout_bucket = load_or_create_rollout_bucket();
        Engine { facts, rollout_bucket, found: Mutex::new(None), cache: Mutex::new(ManifestCache::default()) }
    }

    /// `<app>/<channel>/<version>`: the app, the channel this build was published on and its
    /// version, e.g. `opencode/dev/0.0.0-dev-171`.
    fn user_agent(&self) -> String {
        format!("{}/{}/{}", protocol::APP_NAME, self.facts.channel, self.facts.version)
    }

    /// The manifest's text: fresh, or the cached one when the service answers 304.
    fn fetch_manifest(&self, url: &str) -> Result<String, String> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if cache.url != url {
            *cache = ManifestCache { url: url.to_owned(), ..ManifestCache::default() };
        }
        let mut resp = Self::call(manifest_timeout(), feed::requires_https(url), |agent| {
            let mut request = agent.get(url).header("User-Agent", &self.user_agent()).header("Accept", "application/json");
            if let (Some(etag), Some(_)) = (&cache.etag, &cache.body) {
                request = request.header("If-None-Match", etag);
            }
            request.call()
        })
        .map_err(|e| e.to_string())?;
        match resp.status().as_u16() {
            304 => cache.body.clone().ok_or_else(|| "HTTP 304 for a manifest never received".to_owned()),
            200..300 => {
                let etag = resp.headers().get("etag").and_then(|v| v.to_str().ok()).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty() && !v.chars().any(char::is_control));
                let text = resp.body_mut().with_config().limit(MAX_MANIFEST_BYTES).read_to_string().map_err(|e| format!("could not read manifest: {e}"))?;
                cache.etag = etag;
                cache.body = Some(text.clone());
                Ok(text)
            }
            status => Err(format!("HTTP {status}")),
        }
    }

    /// `send` through an agent with the operating system's certificate check; when that check
    /// refuses the server (or the handshake fails), once more with the root list compiled into the
    /// app, the one every build before `net_trust` checked with alone. An update check must never
    /// fail where the build before it passed: a build that can't verify its feed could never fetch
    /// the build that repairs it.
    fn call<T>(timeout: Duration, https_only: bool, send: impl Fn(ureq::Agent) -> Result<T, ureq::Error>) -> Result<T, ureq::Error> {
        match send(Self::agent(timeout, https_only, false)) {
            Err(e) if net_trust::is_certificate_refusal(&e) => {
                eprintln!("updater: the system's certificate check refused the update server ({e}); trying the built-in root list");
                send(Self::agent(timeout, https_only, true))
            }
            other => other,
        }
    }

    fn agent(timeout: Duration, https_only: bool, builtin: bool) -> ureq::Agent {
        (if builtin { net_trust::builtin_agent_config() } else { net_trust::agent_config() })
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .https_only(https_only)
            .build()
            .into()
    }

    /// The release to install, or `None` when a valid manifest for this platform names no newer
    /// build to install. Any missing, unreachable or malformed manifest, or one without an entry
    /// for `target_name`, fails the check instead of claiming the install is up to date.
    pub fn check(&self) -> Result<Option<String>, String> {
        Ok(self.check_target(feed::target_name())?.map(|f| f.version))
    }

    /// [`Engine::check`] for release target `target_name`.
    pub fn check_target(&self, target_name: &str) -> Result<Option<Found>, String> {
        let url = protocol::manifest_url(&self.facts.service, &self.facts.channel);
        let fail = |why: &str| -> String {
            eprintln!("updater: check failed ({url}): {why}");
            opencode_i18n::t!("rune.updater.checkFailed").into()
        };
        if !feed::allowed_update_url(&url) {
            return Err(fail("refusing insecure manifest URL"));
        }
        let text = self.fetch_manifest(&url).map_err(|e| fail(&e))?;
        let manifest = protocol::parse_manifest(&text, &url, target_name).map_err(|e| fail(&e))?;
        let installed = read_installed_sha(&self.facts.version);
        let decision = protocol::decide(&manifest, &self.facts.channel, &self.facts.version, installed.as_deref(), self.rollout_bucket).map_err(|e| fail(&e))?;
        let found = match decision {
            Decision::Install { version, target } => Some(Found { version, target }),
            Decision::NoTarget => return Err(fail(&format!("manifest for {} has no target {target_name}", manifest.release.version))),
            Decision::UpToDate | Decision::RolloutDeferred { .. } => None,
        };
        if let Ok(mut slot) = self.found.lock() {
            slot.clone_from(&found);
        }
        Ok(found)
    }

    pub fn found(&self) -> Option<Found> {
        self.found.lock().ok()?.clone()
    }

    /// Downloads the last check's archive to `archive`, checking its size and SHA-256 against
    /// the manifest's; `progress` hears the bytes so far and the total.
    pub fn download(&self, archive: &Path, progress: &dyn Fn(u64, u64)) -> Result<Found, String> {
        let found = self.found().ok_or_else(|| "No update to download".to_owned())?;
        let result = self.fetch(&found.target, archive, progress);
        if result.is_err() {
            std::fs::remove_file(archive).ok();
        }
        result.map(|()| found)
    }

    fn fetch(&self, target: &Target, archive: &Path, progress: &dyn Fn(u64, u64)) -> Result<(), String> {
        if !feed::allowed_update_url(&target.url) {
            return Err("refusing insecure download URL".into());
        }
        let mut resp = Self::call(DOWNLOAD_TIMEOUT, feed::requires_https(&target.url), |agent| agent.get(&target.url).header("User-Agent", &self.user_agent()).call())
            .map_err(|e| format!("download failed: {e}"))?;
        let code = resp.status().as_u16();
        if !(200..300).contains(&code) {
            return Err(format!("download returned HTTP {code}"));
        }
        let mut file = std::fs::File::create(archive).map_err(|e| format!("could not write archive: {e}"))?;
        let mut reader = resp.body_mut().as_reader();
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        let mut written: u64 = 0;
        progress(0, target.size);
        loop {
            let n = reader.read(&mut buf).map_err(|e| format!("download interrupted: {e}"))?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > target.size {
                return Err("downloaded archive exceeded manifest size".into());
            }
            hasher.update(&buf[..n]);
            file.write_all(&buf[..n]).map_err(|e| format!("disk write failed: {e}"))?;
            progress(written, target.size);
        }
        file.sync_all().map_err(|e| format!("sync failed: {e}"))?;
        if written != target.size {
            return Err(format!("downloaded {written} bytes, expected {}", target.size));
        }
        let digest: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if digest != target.sha256 {
            return Err("update archive SHA-256 mismatch".into());
        }
        Ok(())
    }
}

/// The updater's records, in the app's state (Desktop's updater plugin namespace): `ready` (the
/// staged version), `rune.rolloutBucket` (this install's 0-99 rollout seed) and
/// `rune.installedRelease` (`<version> <sha256>` of the release last staged). Read and written in
/// memory; the storage lands them behind.
pub const NAMESPACE: &str = "plugin.updater";

#[allow(clippy::disallowed_methods)]
fn load_or_create_rollout_bucket() -> u8 {
    if let Some(bucket) = opencode_platform::storage::read_json(NAMESPACE, "rune.rolloutBucket").and_then(|v| v.as_u64()).and_then(|b| u8::try_from(b).ok()).filter(|b| *b < 100) {
        return bucket;
    }
    let nanos = rune::time::SystemTime::now().duration_since(rune::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    let bucket = hasher.finalize()[0] % 100;
    opencode_platform::storage::write_json(NAMESPACE, "rune.rolloutBucket", &serde_json::json!(bucket));
    bucket
}

/// The SHA-256 of the archive the running build was installed from, when an update installed it:
/// the release last staged, if that's the version running now.
pub fn read_installed_sha(running_version: &str) -> Option<String> {
    let raw = opencode_platform::storage::read_json(NAMESPACE, "rune.installedRelease")?;
    let (ver, sha) = raw.as_str()?.split_once(' ')?;
    (ver == running_version && sha.len() == 64).then(|| sha.to_owned())
}

/// Records the verified archive only after its bundle has reached the install path.
pub fn record_installed(version: &str, sha256: &str) {
    opencode_platform::storage::write_json(NAMESPACE, "rune.installedRelease", &serde_json::json!(format!("{version} {sha256}")));
}
