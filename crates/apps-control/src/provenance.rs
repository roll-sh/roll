//! Provenance on roll's own builds (SLSA v1, as GitHub's artifact attestations and Cloud Build's
//! provenance; docs/apps/README.md, "Signed images"): each image roll builds gets an in-toto
//! statement of what it was built from (the source and revision, the base images' digests, the
//! plan, the Dockerfile's hash, the build variables' names, never their values) and how (roll's
//! builder, the deployment, when), with the image's own digest as its subject, signed as a DSSE
//! envelope with a P-256 key roll keeps for the project (sealed; its public key is
//! `GET …/projects/{project}/signing`'s `roll_key`). `GET …/deploys/{dep}/provenance` answers it.
//! A deployment that reused an earlier build's image has that build's.

use crate::{Apps, Error, Result};
use base64::Engine;
use serde_json::{Value, json};
use std::sync::Arc;

/// DSSE's payload type for an in-toto statement.
pub const PAYLOAD_TYPE: &str = "application/vnd.in-toto+json";
/// What roll's builds are, as SLSA's `buildType`.
pub const BUILD_TYPE: &str = "https://roll.sh/apps/build/v1";

/// DSSE's pre-authentication encoding of a payload: what's signed.
pub fn pae(payload_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = format!("DSSEv1 {} {} {} ", payload_type.len(), payload_type, payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out
}

/// A new P-256 key: its PKCS#8 (DER) and its public key in PEM (as cosign writes `cosign.pub`).
pub fn new_key() -> (Vec<u8>, String) {
    use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    let secret = loop {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).expect("system randomness");
        if let Ok(k) = p256::SecretKey::from_slice(&b) {
            break k;
        }
    };
    let der = secret.to_pkcs8_der().expect("a P-256 key encodes").as_bytes().to_vec();
    let public = secret.public_key().to_public_key_pem(LineEnding::LF).expect("a P-256 public key encodes");
    (der, public)
}

/// The statement signed into a DSSE envelope with `key` (PKCS#8 DER).
pub fn sign(statement: &Value, key: &[u8]) -> anyhow::Result<Value> {
    use p256::ecdsa::signature::Signer;
    use p256::pkcs8::DecodePrivateKey;
    let payload = serde_json::to_vec(statement)?;
    let signing = p256::ecdsa::SigningKey::from_pkcs8_der(key).map_err(|e| anyhow::anyhow!("roll's provenance key: {e}"))?;
    let sig: p256::ecdsa::Signature = signing.sign(&pae(PAYLOAD_TYPE, &payload));
    let b64 = base64::engine::general_purpose::STANDARD;
    Ok(json!({ "payloadType": PAYLOAD_TYPE, "payload": b64.encode(&payload), "signatures": [{ "keyid": "", "sig": b64.encode(sig.to_der().as_bytes()) }] }))
}

/// What a build's statement says.
pub struct Built<'a> {
    pub image: &'a str,
    pub digest: &'a str,
    pub deployment: &'a str,
    pub builder: &'a str,
    pub source: Value,
    pub revision: Option<&'a str>,
    pub plan: &'a Value,
    pub dockerfile_sha256: String,
    pub variables: Vec<String>,
    pub started_ms: i64,
    pub finished_ms: i64,
}

/// SLSA v1 provenance for a build.
pub fn statement(b: &Built<'_>) -> Value {
    let time = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)).unwrap_or_default();
    let mut deps = Vec::new();
    if let (Some(rev), Some(uri)) = (b.revision, b.source["repo"].as_str().map(|r| format!("roll-code:{r}")).or_else(|| b.source["url"].as_str().map(|u| format!("git+{u}")))) {
        deps.push(json!({ "uri": uri, "digest": { "gitCommit": rev } }));
    }
    for base in b.plan["bases"].as_array().into_iter().flatten() {
        if let (Some(image), Some(digest)) = (base["image"].as_str(), base["digest"].as_str()) {
            let (algo, hex) = digest.split_once(':').unwrap_or(("sha256", digest));
            deps.push(json!({ "uri": format!("docker://{image}"), "digest": { algo: hex } }));
        }
    }
    let digest = b.digest.strip_prefix("sha256:").unwrap_or(b.digest);
    json!({
        "_type": "https://in-toto.io/Statement/v1",
        "subject": [{ "name": b.image, "digest": { "sha256": digest } }],
        "predicateType": "https://slsa.dev/provenance/v1",
        "predicate": {
            "buildDefinition": {
                "buildType": BUILD_TYPE,
                "externalParameters": { "source": b.source, "revision": b.revision },
                "internalParameters": {
                    "provider": b.plan["provider"],
                    "framework": b.plan["framework"],
                    "dockerfile_sha256": b.dockerfile_sha256,
                    "variables": b.variables,
                },
                "resolvedDependencies": deps,
            },
            "runDetails": {
                "builder": { "id": format!("https://roll.sh/apps/builder/{}", b.builder) },
                "metadata": { "invocationId": b.deployment, "startedOn": time(b.started_ms), "finishedOn": time(b.finished_ms) },
            },
        },
    })
}

impl Apps {
    /// A build's provenance signed and kept with its image (none when the image has no digest
    /// this node can read).
    pub(crate) async fn record_provenance(self: &Arc<Self>, project: &str, b: Built<'_>) -> Result<()> {
        let p = project.to_string();
        let key = self.db(move |s| s.provenance_key(&p)).await?;
        let envelope = sign(&statement(&b), &key.0).map_err(|e| Error::Internal(e.to_string()))?;
        let (image, text) = (b.image.to_string(), envelope.to_string());
        self.db(move |s| s.put_provenance(&image, &text)).await
    }

    /// `GET …/deploys/{dep}/provenance`: its image's signed provenance, a DSSE envelope.
    pub async fn deploy_provenance(self: &Arc<Self>, service: &str, env: &str, deployment: &str) -> Result<Value> {
        let d = self.deployment(service, env, deployment).await?;
        let image = d.image.ok_or_else(|| Error::NotFound(format!("{deployment} has no image")))?;
        let text = self.db(move |s| s.provenance(&image)).await?.ok_or_else(|| Error::NotFound(format!("{deployment}'s image has no provenance: roll didn't build it")))?;
        serde_json::from_str(&text).map_err(|e| Error::Internal(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_statement_is_signed_as_dsse_and_verifies() {
        use p256::ecdsa::signature::Verifier;
        use p256::pkcs8::DecodePublicKey;
        let (key, public) = new_key();
        let plan = json!({ "provider": "node", "bases": [{ "image": "node:22-slim", "digest": "sha256:abc" }] });
        let s = statement(&Built {
            image: "roll-apps-svc_a:dep_1",
            digest: "sha256:0123",
            deployment: "dep_1",
            builder: "docker",
            source: json!({ "kind": "repo", "repo": "acme/site" }),
            revision: Some("a1b2c3"),
            plan: &plan,
            dockerfile_sha256: "ff".into(),
            variables: vec!["NEXT_PUBLIC_API".into()],
            started_ms: 1_700_000_000_000,
            finished_ms: 1_700_000_060_000,
        });
        assert_eq!(s["subject"][0]["digest"]["sha256"], "0123");
        assert_eq!(s["predicate"]["buildDefinition"]["resolvedDependencies"][0]["uri"], "roll-code:acme/site");
        assert_eq!(s["predicate"]["buildDefinition"]["resolvedDependencies"][1]["digest"]["sha256"], "abc");
        let env = sign(&s, &key).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD;
        let payload = b64.decode(env["payload"].as_str().unwrap()).unwrap();
        let sig = p256::ecdsa::Signature::from_der(&b64.decode(env["signatures"][0]["sig"].as_str().unwrap()).unwrap()).unwrap();
        let vk = p256::ecdsa::VerifyingKey::from_public_key_pem(&public).unwrap();
        assert!(vk.verify(&pae(PAYLOAD_TYPE, &payload), &sig).is_ok());
        assert!(vk.verify(&pae(PAYLOAD_TYPE, b"{}"), &sig).is_err(), "only its own payload");
    }
}
