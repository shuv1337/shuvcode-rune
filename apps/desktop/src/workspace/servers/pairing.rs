//! Server addresses and pairing links (Desktop's `servers/connect/pairing.ts`,
//! `normalizeServerUrl`): pure URL/link parsing lives in `opencode_runtime::pairing`, while this
//! module handles HTTP redemption of a link's single-use code for the server token.

use std::time::Duration;

use serde_json::Value;

pub use opencode_runtime::pairing::*;

/// A redeemed pairing: the address it reached and the token that signs in there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    pub url: String,
    pub password: String,
}

/// What redeeming a link came to.
pub enum Redeemed {
    Paired(Pairing),
    /// A server answered that the code is spent.
    Expired,
    /// No address answered.
    Unreachable,
}

/// `redeemPairingLink`: tries every address at once, 5 s each. The code is single-use, so at most
/// one succeeds; the first address (the one the user chose in Pairing) wins when it answers within
/// a second of another, as a local address stops working away from its network. Blocks: run it
/// off the app's thread. A page has no threads: it tries the addresses one after another, the
/// chosen one first.
pub fn redeem(link: &Link) -> Redeemed {
    #[cfg(target_arch = "wasm32")]
    return redeem_in_turn(link);
    #[cfg(not(target_arch = "wasm32"))]
    redeem_at_once(link)
}

#[cfg(target_arch = "wasm32")]
fn redeem_in_turn(link: &Link) -> Redeemed {
    let mut expired = false;
    for url in &link.urls {
        match connect(url, &link.code) {
            Ok(token) => return Redeemed::Paired(Pairing { url: url.clone(), password: token }),
            Err(true) => expired = true,
            Err(false) => {}
        }
    }
    if expired { Redeemed::Expired } else { Redeemed::Unreachable }
}

#[cfg(not(target_arch = "wasm32"))]
fn redeem_at_once(link: &Link) -> Redeemed {
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Result<String, bool>)>();
    for (i, url) in link.urls.iter().enumerate() {
        let (tx, url, code) = (tx.clone(), url.clone(), link.code.clone());
        std::thread::spawn(move || {
            tx.send((i, connect(&url, &code))).ok();
        });
    }
    drop(tx);
    let mut paired: Option<(usize, String)> = None;
    let mut expired = false;
    let mut first_answered: Option<bool> = None;
    let mut deadline: Option<rune::time::Instant> = None;
    loop {
        let next = match deadline {
            Some(at) => rx.recv_timeout(at.saturating_duration_since(rune::time::Instant::now())).ok(),
            None => rx.recv().ok(),
        };
        let Some((i, result)) = next else { break };
        if i == 0 {
            first_answered = Some(matches!(result, Ok(_) | Err(true)));
        }
        match result {
            Ok(token) if paired.is_none() => {
                paired = Some((i, token));
                if i == 0 {
                    break;
                }
                deadline = Some(rune::time::Instant::now() + Duration::from_secs(1));
            }
            Err(true) => expired = true,
            _ => {}
        }
        if paired.is_some() && first_answered.is_some() {
            break;
        }
    }
    let Some((i, token)) = paired else { return if expired { Redeemed::Expired } else { Redeemed::Unreachable } };
    // The token works on every address: keep the first one when it answers.
    if i != 0 && first_answered == Some(true) {
        let first = Pairing { url: link.urls[0].clone(), password: token.clone() };
        if crate::servers::check(&first.url, Some(&first.password), Duration::from_secs(5)) == crate::servers::Health::Healthy {
            return Redeemed::Paired(first);
        }
    }
    Redeemed::Paired(Pairing { url: link.urls[i].clone(), password: token })
}

/// `GET /auth/connect/<code>` on `url`: the session token, or whether the server refused it (401).
fn connect(url: &str, code: &str) -> Result<String, bool> {
    let agent: ureq::Agent = net_trust::agent_config().timeout_global(Some(Duration::from_secs(5))).http_status_as_error(false).build().into();
    let mut resp = agent.get(format!("{url}/auth/connect/{code}")).call().map_err(|_| false)?;
    if resp.status().as_u16() == 401 {
        return Err(true);
    }
    if !resp.status().is_success() {
        return Err(false);
    }
    let v: Value = resp.body_mut().read_to_string().ok().and_then(|t| serde_json::from_str(&t).ok()).ok_or(false)?;
    v["token"].as_str().map(str::to_owned).ok_or(false)
}
