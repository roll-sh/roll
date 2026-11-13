//! Leaked tokens reported from outside (docs/permissions.md, "Leaked tokens"): GitHub's secret
//! scanning partner program posts every roll token it finds in a public repository, gist or issue
//! to `POST /v1/access/leaks/github`, signed with one of GitHub's ECDSA P-256 keys
//! (`Github-Public-Key-Identifier`, `Github-Public-Key-Signature`: base64 DER over the body). roll
//! checks the signature against GitHub's published keys, revokes each live token at once, and
//! answers which were real roll tokens (`true_positive`) by their SHA-256.

use crate::{Access, checksum_ok, hash};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where GitHub publishes its secret scanning keys.
pub const GITHUB_KEYS: &str = "https://api.github.com/meta/public_keys/secret_scanning";
/// A report's body read at most.
const MAX_REPORT: usize = 1 << 20;
/// Keys are refetched after this, or for an unknown key id at most this often.
const KEYS_FOR: Duration = Duration::from_secs(3600);
const REFETCH_EVERY: Duration = Duration::from_secs(60);
/// A P-256 SubjectPublicKeyInfo is this DER prefix, then the 65-byte uncompressed point.
const P256_SPKI: [u8; 26] = [0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00];

/// Keys by identifier: each one's uncompressed P-256 point.
type Keys = HashMap<String, Vec<u8>>;

/// GitHub's keys, and when they were fetched.
#[derive(Default)]
pub struct GithubKeys(Mutex<Option<(Instant, Keys)>>);

/// A PEM public key's P-256 point.
fn p256_point(pem: &str) -> Option<Vec<u8>> {
    let b64: String = pem.lines().filter(|l| !l.starts_with("-----")).map(str::trim).collect();
    let der = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    (der.len() == P256_SPKI.len() + 65 && der.starts_with(&P256_SPKI)).then(|| der[P256_SPKI.len()..].to_vec())
}

impl Access {
    /// The key GitHub signed with: from the cache, else fetched (at most once a minute).
    async fn github_key(&self, id: &str) -> Option<Vec<u8>> {
        let stale = {
            let keys = self.github_keys.0.lock().unwrap_or_else(|e| e.into_inner());
            match &*keys {
                Some((at, k)) if at.elapsed() < KEYS_FOR && k.contains_key(id) => return k.get(id).cloned(),
                Some((at, _)) => at.elapsed() >= REFETCH_EVERY,
                None => true,
            }
        };
        if !stale {
            return None;
        }
        let fetched = fetch_keys(&self.config.github_keys_url).await;
        let mut keys = self.github_keys.0.lock().unwrap_or_else(|e| e.into_inner());
        match fetched {
            Ok(k) => {
                let found = k.get(id).cloned();
                *keys = Some((Instant::now(), k));
                found
            }
            Err(e) => {
                tracing::warn!(error = %e, "fetching GitHub's secret scanning keys");
                // Don't ask again for a minute; keep what we had.
                let old = keys.take().map(|(_, k)| k).unwrap_or_default();
                *keys = Some((Instant::now() - KEYS_FOR + REFETCH_EVERY, old));
                None
            }
        }
    }
}

async fn fetch_keys(url: &str) -> Result<Keys, String> {
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(Duration::from_secs(5)).build().map_err(|e| e.to_string())?;
    let mut resp = client.get(url).header("user-agent", "roll").header("accept", "application/json").send().await.map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        return Err(format!("answered {}", resp.status()));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| e.without_url().to_string())? {
        body.extend_from_slice(&chunk);
        if body.len() > 256 << 10 {
            return Err("the key list is too big".into());
        }
    }
    let v: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    let keys = v["public_keys"].as_array().ok_or("no public_keys")?;
    Ok(keys
        .iter()
        .filter_map(|k| Some((k["key_identifier"].as_str()?.to_string(), p256_point(k["key"].as_str()?)?)))
        .collect())
}

/// `POST /v1/access/leaks/github`: GitHub's report of roll tokens it found, verified, revoked.
pub async fn github(State(a): State<Arc<Access>>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    let refuse = |status: StatusCode, msg: &str| (status, axum::Json(json!({ "error": msg }))).into_response();
    if body.len() > MAX_REPORT {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "the report is too large");
    }
    let header = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty());
    let (Some(id), Some(sig)) = (header("github-public-key-identifier"), header("github-public-key-signature")) else {
        return refuse(StatusCode::UNAUTHORIZED, "a report is signed by GitHub");
    };
    let Ok(sig) = base64::engine::general_purpose::STANDARD.decode(sig) else { return refuse(StatusCode::UNAUTHORIZED, "the signature isn't base64") };
    let Some(key) = a.github_key(id).await else { return refuse(StatusCode::UNAUTHORIZED, "not one of GitHub's keys") };
    let verifier = ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_ASN1, key);
    if verifier.verify(&body, &sig).is_err() {
        return refuse(StatusCode::UNAUTHORIZED, "the signature doesn't match");
    }
    let Ok(Value::Array(found)) = serde_json::from_slice::<Value>(&body) else { return refuse(StatusCode::BAD_REQUEST, "expected a JSON array") };
    let mut answer = Vec::new();
    for f in found.iter().take(1000) {
        let Some(token) = f["token"].as_str() else { continue };
        let real = checksum_ok(token);
        if real {
            let at = f["url"].as_str().filter(|u| u.len() <= 500).unwrap_or("GitHub");
            let (a2, t, at) = (a.clone(), token.to_string(), format!("{at} (GitHub secret scanning)"));
            if let Ok(Err(e)) = tokio::task::spawn_blocking(move || a2.revoke_leaked(&[t], &at)).await {
                tracing::warn!(error = %e, "revoking a token GitHub found");
            }
        }
        answer.push(json!({ "token_hash": hash(token), "token_type": f["type"], "label": if real { "true_positive" } else { "false_positive" } }));
    }
    axum::Json(Value::Array(answer)).into_response()
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_pem_p256_key_gives_its_point() {
        // GitHub's documented example key shape: a P-256 SubjectPublicKeyInfo.
        let pem = "-----BEGIN PUBLIC KEY-----\nMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEsz9ugWDj5jK5ELBK42ynytbo38gP\nHzZFI03Exwz8Lh/tCfL3YxwMdLjB+bMznsanlhK0RwcGP3IDb34kQDIo3Q==\n-----END PUBLIC KEY-----";
        let point = super::p256_point(pem).unwrap();
        assert_eq!((point.len(), point[0]), (65, 0x04));
        assert!(super::p256_point("-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----").is_none());
    }
}
