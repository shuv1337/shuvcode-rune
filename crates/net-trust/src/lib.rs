//! The certificate check of every HTTPS request and `wss://` socket the app makes: webpki
//! against the union of the roots compiled into the app (Mozilla's, `webpki-root-certs`) and the
//! roots in the operating system's store (`rustls-native-certs`: Windows' ROOT store, macOS'
//! keychain trust settings, the distribution's bundle on Linux).
//!
//! A network that inspects TLS with its own root installed in the system store (a company's)
//! then works, as it does in Desktop's Chromium, where the compiled-in list alone refused it as an
//! unknown issuer. Nothing the compiled-in list accepts is refused (the union contains it), and
//! no revocation is fetched online (as before), so no first connection waits on a revocation
//! server that a network drops.
//!
//! The store is read once per process, on the first agent or socket configured for `https`:
//! never for an agent of a plain `http://` base ([`agent_config_for`]), so the local server's
//! agents at launch read nothing.
//!
//! Every ureq agent starts from [`agent_config`] or [`agent_config_for`]
//! (`scripts/guardrails/net-trust.mjs` fails a `ureq::Agent::config_builder()` or a default agent
//! anywhere else), and a WebSocket's TLS takes [`rustls_client_config`].

#![forbid(unsafe_code)]

#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, OnceLock};

/// A ureq agent's configuration that checks certificates against the compiled-in roots and the
/// operating system's; the caller adds its timeouts and the rest. Its first call in the process
/// reads the operating system's store: for an agent that may only ever talk to a plain `http://`
/// base, use [`agent_config_for`].
#[cfg(not(target_arch = "wasm32"))]
pub fn agent_config() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    ureq::Agent::config_builder().tls_config(tls_config())
}

/// [`agent_config`] for an agent whose requests all go to `base`: a plain `http://` base (the
/// local server) has no TLS, so its configuration reads no certificate store.
#[cfg(not(target_arch = "wasm32"))]
pub fn agent_config_for(base: &str) -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    if base.starts_with("http://") { ureq::Agent::config_builder() } else { agent_config() }
}

/// A page's requests: the browser checks their certificates itself.
#[cfg(target_arch = "wasm32")]
pub fn agent_config() -> ureq_web::Config {
    ureq_web::Agent::config_builder()
}

/// A page's requests: the browser checks their certificates itself.
#[cfg(target_arch = "wasm32")]
pub fn agent_config_for(_base: &str) -> ureq_web::Config {
    ureq_web::Agent::config_builder()
}

/// A ureq agent's configuration with the compiled-in roots alone: what every request checked
/// with before this crate. Only the updater's fallback takes it ([`is_certificate_refusal`]): a
/// build whose update check failed where the build before it passed could never fetch the build
/// that repairs it.
#[cfg(not(target_arch = "wasm32"))]
pub fn builtin_agent_config() -> ureq::config::ConfigBuilder<ureq::typestate::AgentScope> {
    ureq::Agent::config_builder().tls_config(ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::WebPki).build())
}

/// A page's requests: the browser checks their certificates itself.
#[cfg(target_arch = "wasm32")]
pub fn builtin_agent_config() -> ureq_web::Config {
    ureq_web::Agent::config_builder()
}

/// Whether a request failed in its TLS handshake (the certificate check refused the server, or
/// the handshake itself failed), rather than on the network or in HTTP.
#[cfg(not(target_arch = "wasm32"))]
pub fn is_certificate_refusal(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::Rustls(_) => true,
        // ureq's rustls transport reports the handshake as the stream's error, the rustls error inside.
        ureq::Error::Io(io) => io.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()),
        _ => false,
    }
}

/// A page's requests have no TLS of their own.
#[cfg(target_arch = "wasm32")]
pub fn is_certificate_refusal(_: &ureq_web::Error) -> bool {
    false
}

/// ureq's TLS settings: the compiled-in roots and the operating system's.
#[cfg(not(target_arch = "wasm32"))]
pub fn tls_config() -> ureq::tls::TlsConfig {
    ureq::tls::TlsConfig::builder().root_certs(ureq::tls::RootCerts::Specific(roots())).build()
}

/// A rustls client configuration with the same roots, for a WebSocket's own TLS
/// (`tungstenite::Connector::Rustls`). Made once.
#[cfg(not(target_arch = "wasm32"))]
pub fn rustls_client_config() -> Option<Arc<rustls::ClientConfig>> {
    static CONFIG: OnceLock<Option<Arc<rustls::ClientConfig>>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut store = rustls::RootCertStore::empty();
            store.add_parsable_certificates(roots().iter().map(|c| rustls::pki_types::CertificateDer::from(c.der()).into_owned()));
            Some(Arc::new(rustls::ClientConfig::builder().with_root_certificates(store).with_no_client_auth()))
        })
        .clone()
}

/// The compiled-in roots, then the operating system store's: read once per process. A store that
/// can't be read (or a Linux system without a bundle) leaves the compiled-in roots, as before.
#[cfg(not(target_arch = "wasm32"))]
fn roots() -> Arc<Vec<ureq::tls::Certificate<'static>>> {
    static ROOTS: OnceLock<Arc<Vec<ureq::tls::Certificate<'static>>>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let builtin = webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned());
            let started = std::time::Instant::now();
            let system = rustls_native_certs::load_native_certs().certs;
            eprintln!("net-trust: read {} roots from the system store in {} us", system.len(), started.elapsed().as_micros());
            Arc::new(builtin.chain(system.iter().map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned())).collect())
        })
        .clone()
}
