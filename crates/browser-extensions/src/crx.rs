//! Chrome Web Store / Edge Add-ons URL parsing, HTTPS redirect allow-list download, CRX3 protobuf
//! & RSA/ECDSA-P256 signature verification, canonical SPKI validation, and local P-256 keygen.
//!
//! Mirrors `packages/gui-plugins/src/browser/profile/{extensions,install}.ts` and Chromium
//! `components/crx_file/{crx3.proto,crx_verifier.cc}`.

use sha2::{Digest, Sha256};

use crate::StoreKind;

/// Maximum CRX3 header size (1 MiB, matching `extensions.ts:265`).
#[cfg(not(target_arch = "wasm32"))]
const MAX_CRX_HEADER_BYTES: usize = 1024 * 1024;
/// Maximum protobuf fields parsed per message (matching `extensions.ts:322`).
#[cfg(not(target_arch = "wasm32"))]
const MAX_PROTO_FIELDS: usize = 32;
/// Maximum asymmetric proofs checked per CRX3 header (matching `extensions.ts:276`).
#[cfg(not(target_arch = "wasm32"))]
const MAX_PROOFS: usize = 8;
#[cfg(not(target_arch = "wasm32"))]
const MAX_KEY_BYTES: usize = 4096;
#[cfg(not(target_arch = "wasm32"))]
const MAX_SIG_BYTES: usize = 2048;

/// Maximum downloadable CRX3 size (128 MiB, matching `install.ts:7`).
#[cfg(not(target_arch = "wasm32"))]
pub const MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;
#[cfg(not(target_arch = "wasm32"))]
const DOWNLOAD_TIMEOUT_SECS: u64 = 90;
#[cfg(not(target_arch = "wasm32"))]
const MAX_REDIRECT_HOPS: usize = 6;

/// Verified store listing parsed from a bare 32-char ID or Chrome Web Store / Edge Add-ons URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreTarget {
    pub id: String,
    pub store: StoreKind,
}

/// Verified CRX3 package payload and the developer SPKI public key that derived `expected_id`.
#[derive(Clone, Debug)]
pub struct VerifiedCrx<'a> {
    pub zip: &'a [u8],
    pub key: Vec<u8>,
}

/// Constant-time byte slice equality (used for SHA-256 digests and single-use review tokens).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Whether `id` is a valid 32-character Chromium extension ID (`a..p`).
pub fn is_valid_ext_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| (b'a'..=b'p').contains(&b))
}

/// Derives the 32-character Chromium extension ID (`a..p`) from an SPKI DER public key
/// (`extensions.ts:219-228`).
pub fn extension_id(spki_der: &[u8]) -> String {
    let digest = Sha256::digest(spki_der);
    encode_id(&digest[..16])
}

/// Encodes a 16-byte CRX3 `crx_id` into Chromium's 32-char `a..p` alphabet.
pub fn encode_id(bytes_16: &[u8]) -> String {
    let mut out = String::with_capacity(bytes_16.len() * 2);
    for &b in bytes_16 {
        out.push((b'a' + (b >> 4)) as char);
        out.push((b'a' + (b & 0x0f)) as char);
    }
    out
}

/// Parses a bare 32-char extension ID or HTTPS Chrome Web Store / Edge Add-ons URL
/// (`extensions.ts:231-246`). Refuses non-HTTPS, credentials, custom ports, control/whitespace,
/// dot-dot segments, empty slashes, and other hosts.
pub fn store_target(input: &str) -> Option<StoreTarget> {
    let value = input.trim();
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    if is_valid_ext_id(value) {
        return Some(StoreTarget { id: value.to_owned(), store: StoreKind::Chrome });
    }
    let (host, path) = parse_https_no_cred_no_port(value)?;
    let trimmed_path = path.strip_suffix('/').unwrap_or(&path);
    if trimmed_path.contains("//")
        || trimmed_path.split('/').any(|seg| seg == ".." || seg == ".")
    {
        return None;
    }
    let id = trimmed_path.rsplit('/').next()?;
    if !is_valid_ext_id(id) {
        return None;
    }
    if host == "microsoftedge.microsoft.com" && path.starts_with("/addons/detail/") {
        return Some(StoreTarget { id: id.to_owned(), store: StoreKind::Edge });
    }
    if (host == "chromewebstore.google.com" && path.starts_with("/detail/"))
        || (host == "chrome.google.com" && path.starts_with("/webstore/detail/"))
    {
        return Some(StoreTarget { id: id.to_owned(), store: StoreKind::Chrome });
    }
    None
}

/// Builds the CRX3 download URL for `target` and `chrome_version` (`extensions.ts:249-256`).
/// Returns `None` if `target.id` is not a valid 32-char extension ID.
pub fn store_download_url(target: &StoreTarget, chrome_version: &str) -> Option<String> {
    if !is_valid_ext_id(&target.id) {
        return None;
    }
    let query = format!("id%3D{}%26installsource%3Dondemand%26uc", target.id);
    Some(match target.store {
        StoreKind::Edge => {
            format!("https://edge.microsoft.com/extensionwebstorebase/v1/crx?response=redirect&x={query}")
        }
        StoreKind::Chrome => {
            let ver = if chrome_version.bytes().all(|b| b.is_ascii_digit() || b == b'.')
                && !chrome_version.is_empty()
                && chrome_version.len() <= 32
            {
                chrome_version
            } else {
                "156.0.0.0"
            };
            format!(
                "https://clients2.google.com/service/update2/crx?response=redirect&prodversion={ver}&acceptformat=crx3&x={query}"
            )
        }
    })
}

/// Enforces the store redirect host allow-list (`install.ts:67-85`), upgrading Microsoft's HTTP
/// `msedgeextensions.[fb].` delivery hosts to HTTPS `msedgeextensions.s[fb].`.
/// On native builds, cross-validates the resolved URL with `ureq::http::Uri` so there is zero
/// URL parser differential between allow-list checking and the HTTP client.
pub fn store_redirect(store: StoreKind, location: &str) -> Option<String> {
    let trimmed = location.trim();
    if trimmed.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let (expected_host, candidate) = match store {
        StoreKind::Chrome => {
            let (host, _) = parse_https_no_cred_no_port(trimmed)?;
            if host != "clients2.google.com" && host != "clients2.googleusercontent.com" {
                return None;
            }
            (host, trimmed.to_owned())
        }
        StoreKind::Edge => {
            if let Some(rest) = strip_prefix_ci(trimmed, "http://") {
                let (authority, tail) = split_authority(rest);
                if authority.contains('@')
                    || authority.contains(':')
                    || authority.contains('\\')
                {
                    return None;
                }
                let host = authority.to_ascii_lowercase();
                if !is_ms_http_delivery_host(&host) {
                    return None;
                }
                let upgraded = host.replacen("msedgeextensions.", "msedgeextensions.s", 1);
                (upgraded.clone(), format!("https://{upgraded}{tail}"))
            } else {
                let (host, _) = parse_https_no_cred_no_port(trimmed)?;
                if host != "edge.microsoft.com" && !is_ms_https_delivery_host(&host) {
                    return None;
                }
                (host, trimmed.to_owned())
            }
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let uri: ureq::http::Uri = candidate.parse().ok()?;
        if uri.scheme_str() != Some("https") || uri.port().is_some() {
            return None;
        }
        let auth = uri.authority()?.as_str();
        if auth.contains('@') || auth.contains(':') || !auth.eq_ignore_ascii_case(&expected_host) {
            return None;
        }
    }
    #[cfg(target_arch = "wasm32")]
    let _ = expected_host;
    Some(candidate)
}

fn is_ms_http_delivery_host(host: &str) -> bool {
    matches!(
        host,
        "msedgeextensions.f.dl.delivery.mp.microsoft.com"
            | "msedgeextensions.b.dl.delivery.mp.microsoft.com"
            | "msedgeextensions.f.tlu.dl.delivery.mp.microsoft.com"
            | "msedgeextensions.b.tlu.dl.delivery.mp.microsoft.com"
    )
}

fn is_ms_https_delivery_host(host: &str) -> bool {
    matches!(
        host,
        "msedgeextensions.sf.dl.delivery.mp.microsoft.com"
            | "msedgeextensions.sb.dl.delivery.mp.microsoft.com"
            | "msedgeextensions.sf.tlu.dl.delivery.mp.microsoft.com"
            | "msedgeextensions.sb.tlu.dl.delivery.mp.microsoft.com"
    )
}

fn parse_https_no_cred_no_port(url: &str) -> Option<(String, String)> {
    let rest = strip_prefix_ci(url, "https://")?;
    let (authority, tail) = split_authority(rest);
    if authority.is_empty()
        || authority.contains('@')
        || authority.contains(':')
        || authority.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return None;
    }
    let path = tail.split(['?', '#']).next().unwrap_or("");
    Some((authority.to_ascii_lowercase(), path.to_owned()))
}

fn strip_prefix_ci<'a>(input: &'a str, prefix: &str) -> Option<&'a str> {
    if input.len() >= prefix.len() && input.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes()) {
        Some(&input[prefix.len()..])
    } else {
        None
    }
}

fn split_authority(rest: &str) -> (&str, &str) {
    match rest.find(['/', '?', '#']) {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, ""),
    }
}

/// Downloads a CRX3 package over HTTPS following only allow-listed store redirects
/// (`<= 6` hops, `<= 128 MiB`, one shared 90 s total deadline across all hops, HTTPS-only,
/// OS certificate store via `PlatformVerifier`, zero credentials).
#[cfg(not(target_arch = "wasm32"))]
pub fn download_crx(target: &StoreTarget, chrome_version: &str) -> Option<Vec<u8>> {
    use std::io::Read as _;
    use std::time::Duration;
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::Instant;

    let mut url = store_download_url(target, chrome_version)?;
    let deadline = Instant::now() + Duration::from_secs(DOWNLOAD_TIMEOUT_SECS);
    let agent: ureq::Agent = net_trust::agent_config()
        .https_only(true)
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();

    for _ in 0..MAX_REDIRECT_HOPS {
        let remaining = deadline.checked_duration_since(Instant::now())?;
        if remaining.is_zero() {
            return None;
        }
        let resp = agent
            .get(&url)
            .config()
            .timeout_global(Some(remaining))
            .build()
            .call()
            .ok()?;
        let status = resp.status().as_u16();
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let loc = resp.headers().get("location")?.to_str().ok()?;
            url = store_redirect(target.store, loc)?;
            continue;
        }
        if status != 200 {
            return None;
        }
        if let Some(cl) = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            && cl > MAX_DOWNLOAD_BYTES
        {
            return None;
        }
        let mut buf = Vec::new();
        let reader = resp.into_body().into_reader();
        reader.take(MAX_DOWNLOAD_BYTES + 1).read_to_end(&mut buf).ok()?;
        if buf.len() as u64 > MAX_DOWNLOAD_BYTES {
            return None;
        }
        return Some(buf);
    }
    None
}

/// Chrome Web Store CRX3 ECDSA P-256 publisher key SPKI SHA-256 (`"ecdsa_2017_public"`),
/// defined in Chromium `components/crx_file/crx_verifier.cc` (`kPublisherKeyHash`).
#[cfg(not(target_arch = "wasm32"))]
const CHROME_WEB_STORE_PUBLISHER_KEY_SHA256: [u8; 32] = [
    0x61, 0xf7, 0xf2, 0xa6, 0xbf, 0xcf, 0x74, 0xcd, 0x0b, 0xc1, 0xfe, 0x24, 0x97, 0xcc, 0x9b, 0x04,
    0x25, 0x4c, 0x65, 0x8f, 0x79, 0xf2, 0x14, 0x53, 0x92, 0x86, 0x7e, 0xa8, 0x36, 0x63, 0x67, 0xcf,
];

/// Microsoft Edge Add-ons CRX3 ECDSA P-256 publisher key SPKI SHA-256 (verified across live
/// Edge Add-ons store CRX3 packages).
#[cfg(not(target_arch = "wasm32"))]
const EDGE_ADDONS_PUBLISHER_KEY_SHA256: [u8; 32] = [
    0x67, 0x5b, 0xd8, 0xed, 0xdd, 0x38, 0x50, 0x20, 0x17, 0x7c, 0xcf, 0xed, 0xa2, 0x51, 0x03, 0x89,
    0x57, 0x99, 0xfe, 0x41, 0xec, 0xa5, 0xc9, 0x4e, 0x61, 0xdc, 0x13, 0xdf, 0x35, 0x9d, 0xa1, 0xdc,
];

#[cfg(not(target_arch = "wasm32"))]
fn pinned_publisher_key_sha256(store: StoreKind) -> &'static [u8; 32] {
    match store {
        StoreKind::Chrome => &CHROME_WEB_STORE_PUBLISHER_KEY_SHA256,
        StoreKind::Edge => &EDGE_ADDONS_PUBLISHER_KEY_SHA256,
    }
}

/// Parses and cryptographically verifies a CRX3 package against `expected_id` without trusting
/// legacy RSA-1024 keys (`store = None`).
#[cfg(not(target_arch = "wasm32"))]
pub fn parse_crx<'a>(bytes: &'a [u8], expected_id: &str) -> Option<VerifiedCrx<'a>> {
    parse_crx_for_store(bytes, expected_id, None)
}

/// Parses and cryptographically verifies a CRX3 package against `expected_id`
/// (`extensions.ts:259-304`, Chromium `components/crx_file/crx_verifier.cc`).
///
/// Every RSA-SHA256 (field 2) and ECDSA-P256-SHA256 (field 3) proof in the header is verified over
/// `"CRX3 SignedData\0" || LE32(signed_header_data.len()) || signed_header_data || zip`.
/// Keys must be strictly canonical DER SPKI. Returns `Some(VerifiedCrx)` only when every proof
/// verifies AND at least one verified key derives `expected_id` matching `signed_header_data.crx_id`.
///
/// Legacy pre-2013 store extensions (e.g. LastPass) may carry an RSA-1024 developer proof;
/// RSA-1024 is accepted ONLY when `store` is `Some` AND the CRX3 header also carries a verified
/// ECDSA P-256 proof matching that store's pinned publisher key SPKI SHA-256. Any RSA-1024 CRX3
/// lacking the pinned store publisher proof (including attacker-added throwaway P-256/RSA-2048
/// proofs or cross-store proofs) is refused.
#[cfg(not(target_arch = "wasm32"))]
pub fn parse_crx_for_store<'a>(
    bytes: &'a [u8],
    expected_id: &str,
    store: Option<StoreKind>,
) -> Option<VerifiedCrx<'a>> {
    use ring::signature::{
        ECDSA_P256_SHA256_ASN1, RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY,
        RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey,
    };

    if !is_valid_ext_id(expected_id)
        || bytes.len() < 12
        || bytes.len() as u64 > MAX_DOWNLOAD_BYTES
        || &bytes[0..4] != b"Cr24"
    {
        return None;
    }
    let version = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    if version != 3 {
        return None;
    }
    let header_len = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    if header_len > MAX_CRX_HEADER_BYTES || 12usize.checked_add(header_len)? >= bytes.len() {
        return None;
    }
    let header = &bytes[12..12 + header_len];
    let zip = &bytes[12 + header_len..];

    let fields = protobuf_fields(header)?;
    let signed_data = single_field(&fields, 10000)?;
    let signed_fields = protobuf_fields(signed_data)?;
    let crx_id = single_field(&signed_fields, 1)?;
    if crx_id.len() != 16 || encode_id(crx_id) != expected_id {
        return None;
    }

    let proofs: Vec<&(u32, &[u8])> = fields.iter().filter(|(n, _)| *n == 2 || *n == 3).collect();
    if proofs.is_empty() || proofs.len() > MAX_PROOFS {
        return None;
    }

    // Build exact CRX3 SignedData payload once.
    let signed_cap = 20usize.checked_add(signed_data.len())?.checked_add(zip.len())?;
    let mut signed_msg = Vec::with_capacity(signed_cap);
    signed_msg.extend_from_slice(b"CRX3 SignedData\0");
    signed_msg.extend_from_slice(&(signed_data.len() as u32).to_le_bytes());
    signed_msg.extend_from_slice(signed_data);
    signed_msg.extend_from_slice(zip);

    let pinned_pub = store.map(pinned_publisher_key_sha256);
    let mut developer_key: Option<Vec<u8>> = None;
    let mut used_legacy_rsa1024 = false;
    let mut has_pinned_publisher_proof = false;
    for &(field_num, proof_bytes) in &proofs {
        let pfields = protobuf_fields(proof_bytes)?;
        let key_spki = single_field(&pfields, 1)?;
        let sig = single_field(&pfields, 2)?;
        if key_spki.is_empty() || key_spki.len() > MAX_KEY_BYTES || sig.is_empty() || sig.len() > MAX_SIG_BYTES {
            return None;
        }
        let ok = if *field_num == 2 {
            let rsa_pkcs1 = parse_canonical_rsa_spki(key_spki)?;
            if UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, rsa_pkcs1)
                .verify(&signed_msg, sig)
                .is_ok()
            {
                true
            } else if pinned_pub.is_some()
                && UnparsedPublicKey::new(&RSA_PKCS1_1024_8192_SHA256_FOR_LEGACY_USE_ONLY, rsa_pkcs1)
                    .verify(&signed_msg, sig)
                    .is_ok()
            {
                used_legacy_rsa1024 = true;
                true
            } else {
                false
            }
        } else {
            let ec_point = parse_canonical_p256_spki(key_spki)?;
            let valid = UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, ec_point)
                .verify(&signed_msg, sig)
                .is_ok();
            if valid
                && let Some(expected_hash) = pinned_pub
                && constant_time_eq(Sha256::digest(key_spki).as_slice(), expected_hash)
            {
                has_pinned_publisher_proof = true;
            }
            valid
        };
        if !ok {
            return None;
        }
        if extension_id(key_spki) == expected_id {
            developer_key = Some(key_spki.to_vec());
        }
    }

    if used_legacy_rsa1024 && !has_pinned_publisher_proof {
        return None;
    }
    developer_key.map(|key| VerifiedCrx { zip, key })
}

/// Generates a fresh private ECDSA P-256 keypair and returns its canonical SPKI DER public key
/// (`profile/index.ts:573-575`) so local folders and browser-imported copies receive a unique
/// profile-private extension ID that can never impersonate a store extension.
#[cfg(not(target_arch = "wasm32"))]
pub fn generate_local_spki_key() -> Option<Vec<u8>> {
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _};

    let rng = SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).ok()?;
    let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).ok()?;
    let pub_point = pair.public_key().as_ref();
    if pub_point.len() != 65 || pub_point[0] != 0x04 {
        return None;
    }
    let mut spki = Vec::with_capacity(P256_SPKI_PREFIX.len() + pub_point.len());
    spki.extend_from_slice(&P256_SPKI_PREFIX);
    spki.extend_from_slice(pub_point);
    Some(spki)
}

/// Generates a 128-bit cryptographic random hex token for single-use review approval.
#[cfg(not(target_arch = "wasm32"))]
pub fn random_token() -> Option<String> {
    use ring::rand::{SecureRandom as _, SystemRandom};

    let mut bytes = [0u8; 16];
    SystemRandom::new().fill(&mut bytes).ok()?;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(32);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    Some(out)
}

// Canonical DER prefix for SubjectPublicKeyInfo(id-ecPublicKey, prime256v1, 65-byte uncompressed point):
// 30 59 (SEQUENCE 89)
//   30 13 (SEQUENCE 19)
//     06 07 2a 86 48 ce 3d 02 01 (OID 1.2.840.10045.2.1 id-ecPublicKey)
//     06 08 2a 86 48 ce 3d 03 01 07 (OID 1.2.840.10045.3.1.7 prime256v1)
//   03 42 00 (BIT STRING 66 bytes, 0 unused bits)
#[cfg(not(target_arch = "wasm32"))]
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08,
    0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

// Canonical DER AlgorithmIdentifier for rsaEncryption (OID 1.2.840.113549.1.1.1 + NULL):
#[cfg(not(target_arch = "wasm32"))]
const RSA_ALGO_ID: [u8; 15] = [
    0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01, 0x05, 0x00,
];

#[cfg(not(target_arch = "wasm32"))]
fn parse_canonical_p256_spki(spki: &[u8]) -> Option<&[u8]> {
    if spki.len() == P256_SPKI_PREFIX.len() + 65
        && spki.starts_with(&P256_SPKI_PREFIX)
        && spki[P256_SPKI_PREFIX.len()] == 0x04
    {
        Some(&spki[P256_SPKI_PREFIX.len()..])
    } else {
        None
    }
}

/// Validates canonical DER `SubjectPublicKeyInfo` for `rsaEncryption` and returns the inner
/// `RSAPublicKey` DER sequence expected by `ring::signature::RSA_PKCS1_2048_8192_SHA256`.
#[cfg(not(target_arch = "wasm32"))]
fn parse_canonical_rsa_spki(spki: &[u8]) -> Option<&[u8]> {
    let (body, rem) = der_read_tlv(spki, 0x30)?;
    if !rem.is_empty() || !body.starts_with(&RSA_ALGO_ID) {
        return None;
    }
    let after_algo = &body[RSA_ALGO_ID.len()..];
    let (bit_string, rem2) = der_read_tlv(after_algo, 0x03)?;
    if !rem2.is_empty() || bit_string.is_empty() || bit_string[0] != 0x00 {
        return None;
    }
    let rsa_der = &bit_string[1..];
    // Validate inner `RSAPublicKey ::= SEQUENCE { modulus INTEGER, publicExponent INTEGER }` is
    // strictly canonical DER (minimal lengths, positive minimal integers, no trailing bytes).
    let (seq_body, rsa_rem) = der_read_tlv(rsa_der, 0x30)?;
    if !rsa_rem.is_empty() {
        return None;
    }
    let (modulus, after_n) = der_read_tlv(seq_body, 0x02)?;
    let (exponent, after_e) = der_read_tlv(after_n, 0x02)?;
    if !after_e.is_empty() || !is_canonical_positive_der_int(modulus) || !is_canonical_positive_der_int(exponent) {
        return None;
    }
    Some(rsa_der)
}

#[cfg(not(target_arch = "wasm32"))]
fn is_canonical_positive_der_int(bytes: &[u8]) -> bool {
    if bytes.is_empty() || (bytes[0] & 0x80) != 0 {
        return false;
    }
    if bytes.len() > 1 && bytes[0] == 0x00 && (bytes[1] & 0x80) == 0 {
        return false;
    }
    !(bytes.len() == 1 && bytes[0] == 0)
}

/// Reads one canonical DER TLV with expected `tag`, enforcing minimal length encoding.
#[cfg(not(target_arch = "wasm32"))]
fn der_read_tlv(input: &[u8], expected_tag: u8) -> Option<(&[u8], &[u8])> {
    if input.len() < 2 || input[0] != expected_tag {
        return None;
    }
    let b1 = input[1];
    let (len, hdr_len) = if b1 < 0x80 {
        (b1 as usize, 2)
    } else {
        let num_bytes = (b1 & 0x7f) as usize;
        if num_bytes == 0 || num_bytes > 2 || input.len() < 2 + num_bytes {
            return None;
        }
        // Canonical DER forbids leading zero length byte and non-minimal long form (< 128).
        if input[2] == 0 {
            return None;
        }
        let mut v = 0usize;
        for &b in &input[2..2 + num_bytes] {
            v = (v << 8) | (b as usize);
        }
        if v < 0x80 {
            return None;
        }
        (v, 2 + num_bytes)
    };
    let end = hdr_len.checked_add(len)?;
    if end > input.len() {
        return None;
    }
    Some((&input[hdr_len..end], &input[end..]))
}

#[cfg(not(target_arch = "wasm32"))]
fn single_field<'a>(fields: &[(u32, &'a [u8])], number: u32) -> Option<&'a [u8]> {
    let mut found: Option<&'a [u8]> = None;
    for &(n, val) in fields {
        if n == number {
            if found.is_some() {
                return None;
            }
            found = Some(val);
        }
    }
    found
}

/// Bounded length-delimited protobuf decoder (`extensions.ts:315-347`).
#[cfg(not(target_arch = "wasm32"))]
fn protobuf_fields(data: &[u8]) -> Option<Vec<(u32, &[u8])>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < data.len() {
        if out.len() >= MAX_PROTO_FIELDS {
            return None;
        }
        let tag = read_varint(data, &mut off)?;
        if tag < 8 || tag > u64::from(u32::MAX) || (tag & 0x7) != 2 {
            return None;
        }
        let len_u64 = read_varint(data, &mut off)?;
        let len = usize::try_from(len_u64).ok()?;
        let end = off.checked_add(len)?;
        if end > data.len() {
            return None;
        }
        out.push(((tag >> 3) as u32, &data[off..end]));
        off = end;
    }
    Some(out)
}

#[cfg(not(target_arch = "wasm32"))]
fn read_varint(data: &[u8], off: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    while *off < data.len() && shift <= 56 {
        let byte = data[*off];
        *off += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if (byte & 0x80) == 0 {
            return Some(value);
        }
        shift += 7;
    }
    None
}
