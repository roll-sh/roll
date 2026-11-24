//! Certificate syncs (docs/secrets.md, "Getting certificates where TLS terminates"): a managed
//! `pki` key's certificate, private key and chain, pushed where TLS terminates on issue and on
//! every rotation, replacing the same object each time so what serves it keeps it:
//! - `aws-acm`: `ImportCertificate`, again with the ARN it answered the first time. Signed in as a
//!   project AWS role (`…/aws-roles`), assumed for each push: no AWS key is kept for the sync.
//! - `azure-keyvault-cert`: a Key Vault certificate imported as PEM, a new version each time. Signed
//!   in as a service principal, as `azure-key-vault` is.
//! - `cloudflare-cert`: a zone's custom certificate, made once and `PATCH`ed after.
//!
//! Hosts are AWS's, the vault's and Cloudflare's own, built from names checked as names (a region,
//! a vault, a zone id), never a URL a tenant gives. The private key goes only into the request body
//! over TLS: errors carry the target and the platform's status and error code, never the answer's
//! message (it may quote the request).

use super::platforms::{Http, cf_codes, credential, credential_keys, ident, key_field};
use super::{bad, block_on, target};
use base64::Engine as _;
use roll_secrets::syncs::{Certificate, Push, Resources, SyncField, SyncTarget};
use roll_secrets::{Error, Result};
use serde_json::{Value as Json, json};
use zeroize::Zeroizing;

/// The certificate then its chain, one PEM.
fn full_chain(c: &Certificate) -> Zeroizing<String> {
    let mut s = Zeroizing::new(String::new());
    for part in [c.certificate.expose(), c.chain.expose()] {
        let part = part.trim();
        if !part.is_empty() {
            s.push_str(part);
            s.push('\n');
        }
    }
    s
}

/// What the platform answered as an id: printable, bounded.
fn remote_id(what: &str, v: Option<&str>) -> Result<String> {
    match v {
        Some(id) if !id.is_empty() && id.len() <= 2048 && id.chars().all(|c| c.is_ascii_graphic()) => Ok(id.to_string()),
        _ => Err(Error::Link(format!("{what}: the answer has no id"))),
    }
}

/// An AWS region's name (`us-east-1`).
fn region(r: Option<String>) -> Result<String> {
    let r = r.unwrap_or_default();
    if !ident(&r, 32, "-") || r.chars().any(|c| c.is_ascii_uppercase()) || r.starts_with('-') {
        return Err(bad("region is an AWS region (us-east-1)"));
    }
    Ok(r)
}

// ── aws-acm ──

/// `ImportCertificate` into AWS Certificate Manager. `.1`: one endpoint for every region (tests);
/// `None` is `https://acm.<region>.amazonaws.com`.
pub(super) struct Acm(pub(super) Http, pub(super) Option<String>);

const ACM_TAG: (&str, &str) = ("managed-by", "roll-secrets");

impl Acm {
    /// (region, role)
    fn target(&self, t: &Json) -> Result<(String, String)> {
        let [r, role] = target("aws-acm", t, [("region", true), ("role", true)])?;
        let role = role.unwrap_or_default();
        if !ident(&role, 64, "-_") {
            return Err(bad("role is the name of one of the project's AWS roles (…/aws-roles)"));
        }
        Ok((region(r)?, role))
    }
}

/// The `Authorization` of an AWS JSON 1.1 call signed with a session (SigV4, the session's token
/// signed too).
fn sigv4_session(host: &str, region: &str, service: &str, amz_target: &str, amz_date: &str, body: &[u8], s: &roll_secrets::aws::Session) -> String {
    use sha2::{Digest, Sha256};
    let payload: String = Sha256::digest(body).iter().map(|b| format!("{b:02x}")).collect();
    let signed = "content-type;host;x-amz-date;x-amz-security-token;x-amz-target";
    let token = s.session_token.expose();
    let canonical = format!("POST\n/\n\ncontent-type:application/x-amz-json-1.1\nhost:{host}\nx-amz-date:{amz_date}\nx-amz-security-token:{token}\nx-amz-target:{amz_target}\n\n{signed}\n{payload}");
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let sts = s3_proto::sigv4::string_to_sign(amz_date, &scope, &canonical);
    let sig = s3_proto::sigv4::sign(&s3_proto::sigv4::signing_key(s.secret_access_key.expose(), date, region, service), &sts);
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={sig}", s.access_key_id.expose())
}

impl SyncTarget for Acm {
    fn kind(&self) -> &str {
        "aws-acm"
    }
    fn label(&self) -> &str {
        "AWS Certificate Manager"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "region", label: "Region", required: true, placeholder: "us-east-1" },
            SyncField { name: "role", label: "The project's AWS role (by name)", required: true, placeholder: "certificates" },
        ];
        F
    }
    fn certificate(&self) -> bool {
        true
    }
    fn aws_role(&self, t: &Json) -> Option<String> {
        self.target(t).ok().map(|(_, role)| role)
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, _: &Json, _: &Push<'_>) -> Result<()> {
        Err(bad("aws-acm syncs push a certificate"))
    }
    fn push_certificate(&self, t: &Json, p: &Push<'_>, c: &Certificate, remote: Option<&str>) -> Result<String> {
        let (region, _) = self.target(t)?;
        let session = p.aws.ok_or_else(|| bad("an aws-acm sync signs in as the project's AWS role"))?;
        let base = self.1.clone().unwrap_or_else(|| format!("https://acm.{region}.amazonaws.com"));
        let url = url::Url::parse(&format!("{base}/")).map_err(|e| Error::Internal(e.to_string()))?;
        let host = match url.port() {
            Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
            None => url.host_str().unwrap_or_default().to_string(),
        };
        let what = format!("aws-acm {region}");
        let b64 = |s: &str| base64::engine::general_purpose::STANDARD.encode(s);
        let import = |arn: Option<&str>| {
            let mut body = json!({ "Certificate": b64(c.certificate.expose()), "PrivateKey": b64(c.private_key.expose()) });
            if !c.chain.expose().trim().is_empty() {
                body["CertificateChain"] = json!(b64(c.chain.expose()));
            }
            match arn {
                Some(arn) => body["CertificateArn"] = json!(arn),
                // Tags only on the first import: ACM refuses them on a re-import.
                None => body["Tags"] = json!([{ "Key": ACM_TAG.0, "Value": ACM_TAG.1 }, { "Key": "roll-sync", "Value": p.sync }]),
            }
            let body = Zeroizing::new(serde_json::to_vec(&body).expect("JSON"));
            let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let amz_target = "CertificateManager.ImportCertificate";
            let auth = sigv4_session(&host, &region, "acm", amz_target, &amz_date, &body, session);
            self.0
                .guarded()
                .post(url.clone())
                .header("content-type", "application/x-amz-json-1.1")
                .header("x-amz-date", amz_date)
                .header("x-amz-security-token", session.session_token.expose())
                .header("x-amz-target", amz_target)
                .header("authorization", auth)
                .body(body.to_vec())
        };
        // `__type` without its namespace; never the message.
        let kind = |v: &Json| v["__type"].as_str().unwrap_or_default().rsplit('#').next().unwrap_or_default().to_string();
        block_on(async {
            let (mut status, mut v) = self.0.send(import(remote), &what).await?;
            // Deleted at AWS (nothing uses it then: ACM refuses to delete a certificate in use):
            // import it as a new one.
            if remote.is_some() && kind(&v) == "ResourceNotFoundException" {
                (status, v) = self.0.send(import(None), &what).await?;
            }
            if !status.is_success() {
                return Err(Error::Link(format!("{what}: {status} {}", kind(&v))));
            }
            remote_id(&what, v["CertificateArn"].as_str())
        })
    }
}

// ── azure-keyvault-cert ──

/// Microsoft's sign-in and the vault's host (tests point both at a mock).
#[derive(Clone)]
pub(super) struct Azure {
    login: String,
    /// `None`: `https://<vault>.vault.azure.net`.
    vault: Option<String>,
}

impl Azure {
    pub(super) fn new() -> Azure {
        Azure { login: "https://login.microsoftonline.com".into(), vault: None }
    }
}

/// One Key Vault certificate, imported as PEM (certificate, chain and key): each push is a new
/// version of it. A certificate roll creates carries the tag `managed-by=roll-secrets`; one without
/// it is never written.
pub(super) struct KeyVaultCert(pub(super) Http, pub(super) Azure);

struct KvTarget {
    vault: String,
    name: String,
    tenant: String,
    client_id: String,
    secret_key: String,
}

const AKV_API: &str = "api-version=7.4";

impl KeyVaultCert {
    fn target(&self, t: &Json) -> Result<KvTarget> {
        let fields = [("vault", true), ("certificate", true), ("tenant_id", true), ("client_id", true), ("client_secret_key", true)];
        let [vault, name, tenant, client, secret] = target("azure-keyvault-cert", t, fields)?;
        let vault = vault.unwrap_or_default();
        if !(3..=24).contains(&vault.len()) || !ident(&vault, 24, "-") || !vault.starts_with(|c: char| c.is_ascii_alphabetic()) {
            return Err(bad("vault is a Key Vault's name (3–24 letters, digits and -)"));
        }
        let name = name.unwrap_or_default();
        if !ident(&name, 127, "-") {
            return Err(bad("certificate is a Key Vault certificate's name (letters, digits and -)"));
        }
        let tenant = tenant.unwrap_or_default();
        if !ident(&tenant, 64, "-.") {
            return Err(bad("tenant_id is a Microsoft Entra tenant's id or domain"));
        }
        let client_id = client.unwrap_or_default();
        if !ident(&client_id, 64, "-") {
            return Err(bad("client_id is an app registration's client id"));
        }
        Ok(KvTarget { vault, name, tenant, client_id, secret_key: key_field("azure-keyvault-cert", "client_secret_key", secret)? })
    }
}

impl SyncTarget for KeyVaultCert {
    fn kind(&self) -> &str {
        "azure-keyvault-cert"
    }
    fn label(&self) -> &str {
        "Azure Key Vault certificate"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "vault", label: "Vault name", required: true, placeholder: "shop-prod" },
            SyncField { name: "certificate", label: "Certificate name", required: true, placeholder: "api-tls" },
            SyncField { name: "tenant_id", label: "Tenant id", required: true, placeholder: "contoso.onmicrosoft.com" },
            SyncField { name: "client_id", label: "App registration's client id", required: true, placeholder: "00000000-0000-0000-0000-000000000000" },
            SyncField { name: "client_secret_key", label: "Key holding its client secret", required: true, placeholder: "AZURE_SYNC_CLIENT_SECRET" },
        ];
        F
    }
    fn certificate(&self) -> bool {
        true
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["client_secret_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, _: &Json, _: &Push<'_>) -> Result<()> {
        Err(bad("azure-keyvault-cert syncs push a certificate"))
    }
    fn push_certificate(&self, t: &Json, p: &Push<'_>, c: &Certificate, _: Option<&str>) -> Result<String> {
        let t = self.target(t)?;
        let secret = credential(p, &t.secret_key)?;
        let what = format!("azure-keyvault-cert {}/{}", t.vault, t.name);
        let vault = self.1.vault.clone().unwrap_or_else(|| format!("https://{}.vault.azure.net", t.vault));
        let code_of = |v: &Json| v["error"]["code"].as_str().unwrap_or_default().to_string();
        let mut pem = full_chain(c);
        pem.push_str(c.private_key.expose().trim());
        pem.push('\n');
        let client = self.0.guarded();
        block_on(async {
            let form = Zeroizing::new(format!(
                "grant_type=client_credentials&client_id={}&client_secret={}&scope={}",
                urlencoding::encode(&t.client_id),
                urlencoding::encode(secret),
                urlencoding::encode("https://vault.azure.net/.default")
            ));
            let login = client.post(format!("{}/{}/oauth2/v2.0/token", self.1.login, t.tenant)).header("content-type", "application/x-www-form-urlencoded").body(form.to_string());
            let (status, v) = self.0.send(login, &what).await?;
            let token = match v["access_token"].as_str() {
                Some(tok) if status.is_success() => Zeroizing::new(tok.to_string()),
                _ => return Err(Error::Link(format!("{what}: signing in: {status} {}", v["error"].as_str().unwrap_or_default()))),
            };
            let at = format!("{vault}/certificates/{}", t.name);
            let (status, got) = self.0.send(client.get(format!("{at}?{AKV_API}")).bearer_auth(token.as_str()), &what).await?;
            if status.is_success() && got["tags"]["managed-by"] != "roll-secrets" {
                return Err(Error::Link(format!("{what}: the certificate exists and roll didn't create it (no managed-by=roll-secrets tag): roll never writes to it")));
            }
            if !status.is_success() && status != reqwest::StatusCode::NOT_FOUND {
                return Err(Error::Link(format!("{what}: reading it: {status} {}", code_of(&got))));
            }
            let body = json!({
                "value": pem.as_str(),
                "policy": { "secret_props": { "contentType": "application/x-pem-file" } },
                "tags": { "managed-by": "roll-secrets", "roll-sync": p.sync },
            });
            let body = Zeroizing::new(serde_json::to_vec(&body).expect("JSON"));
            let req = client.post(format!("{at}/import?{AKV_API}")).bearer_auth(token.as_str()).header("content-type", "application/json").body(body.to_vec());
            let (status, v) = self.0.send(req, &what).await?;
            if !status.is_success() {
                return Err(Error::Link(format!("{what}: importing: {status} {}", code_of(&v))));
            }
            remote_id(&what, v["id"].as_str())
        })
    }
}

// ── cloudflare-cert ──

/// A zone's custom certificate (`api.cloudflare.com` only; `.1` is its base, a mock in tests):
/// made the first time, `PATCH`ed after, with the chain as given (`bundle_method: force`).
pub(super) struct CloudflareCert(pub(super) Http, pub(super) String);

impl CloudflareCert {
    /// (zone, token_key)
    fn target(&self, t: &Json) -> Result<(String, String)> {
        let [zone, token_key] = target("cloudflare-cert", t, [("zone", true), ("token_key", true)])?;
        let zone = zone.unwrap_or_default();
        if zone.len() != 32 || !zone.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(bad("zone is a Cloudflare zone's id (32 hex characters)"));
        }
        Ok((zone, key_field("cloudflare-cert", "token_key", token_key)?))
    }
}

impl SyncTarget for CloudflareCert {
    fn kind(&self) -> &str {
        "cloudflare-cert"
    }
    fn label(&self) -> &str {
        "Cloudflare custom certificate"
    }
    fn fields(&self) -> &[SyncField] {
        const F: &[SyncField] = &[
            SyncField { name: "zone", label: "Zone id", required: true, placeholder: "023e105f4ecef8ad9ca31a8372d0c353" },
            SyncField { name: "token_key", label: "Key holding the API token (SSL and Certificates: Edit)", required: true, placeholder: "CLOUDFLARE_API_TOKEN" },
        ];
        F
    }
    fn certificate(&self) -> bool {
        true
    }
    fn credentials(&self, t: &Json) -> Vec<String> {
        credential_keys(t, &["token_key"])
    }
    fn check(&self, _: &str, _: &Resources, t: &Json) -> Result<()> {
        self.target(t).map(|_| ())
    }
    fn push(&self, _: &Json, _: &Push<'_>) -> Result<()> {
        Err(bad("cloudflare-cert syncs push a certificate"))
    }
    fn push_certificate(&self, t: &Json, p: &Push<'_>, c: &Certificate, remote: Option<&str>) -> Result<String> {
        let (zone, token_key) = self.target(t)?;
        let token = credential(p, &token_key)?;
        let what = format!("cloudflare-cert {zone}");
        let base = format!("{}/client/v4/zones/{zone}/custom_certificates", self.1);
        let body = json!({ "certificate": full_chain(c).as_str(), "private_key": c.private_key.expose(), "bundle_method": "force" });
        let client = self.0.client();
        let send = |req: reqwest::RequestBuilder, body: &Json| {
            let body = Zeroizing::new(serde_json::to_vec(body).expect("JSON"));
            req.bearer_auth(token).header("content-type", "application/json").body(body.to_vec())
        };
        block_on(async {
            if let Some(id) = remote.filter(|id| ident(id, 64, "-")) {
                let (status, v) = self.0.send(send(client.patch(format!("{base}/{id}")), &body), &what).await?;
                if status.is_success() {
                    return remote_id(&what, v["result"]["id"].as_str().or(Some(id)));
                }
                // Deleted at Cloudflare: make it again.
                if status != reqwest::StatusCode::NOT_FOUND {
                    return Err(Error::Link(format!("{what}: {status} {}", cf_codes(&v))));
                }
            }
            let mut first = body.clone();
            first["type"] = json!("sni_custom");
            let (status, v) = self.0.send(send(client.post(&base), &first), &what).await?;
            if !status.is_success() {
                return Err(Error::Link(format!("{what}: {status} {}", cf_codes(&v))));
            }
            remote_id(&what, v["result"]["id"].as_str())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, StatusCode, Uri};
    use roll_secrets::Value;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    /// Every request the mock got (method, path and query, body), and how many certificates or
    /// versions it made.
    #[derive(Default)]
    struct Mock {
        seen: Mutex<Vec<(String, String, Json)>>,
        made: Mutex<u32>,
    }

    /// ACM (SigV4 with a session checked), Microsoft's sign-in and a vault, and Cloudflare, on one
    /// address.
    async fn mock() -> (String, Arc<Mock>) {
        async fn any(State(m): State<Arc<Mock>>, method: Method, uri: Uri, headers: HeaderMap, body: axum::body::Bytes) -> (StatusCode, axum::Json<Json>) {
            let pq = uri.path_and_query().map(|p| p.as_str().to_string()).unwrap_or_default();
            let asked: Json = serde_json::from_slice(&body).unwrap_or_else(|_| Json::String(String::from_utf8_lossy(&body).into()));
            m.seen.lock().unwrap().push((method.to_string(), pq.clone(), asked.clone()));
            let mut made = m.made.lock().unwrap();
            if let Some(target) = headers.get("x-amz-target") {
                use sha2::{Digest, Sha256};
                let hs: Vec<(String, String)> = headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_string())).collect();
                let hs: Vec<(&str, &str)> = hs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let req = s3_proto::sigv4::Request { method: method.as_str(), path: uri.path(), query: uri.query().unwrap_or(""), headers: &hs };
                let hash: String = Sha256::digest(&body).iter().map(|b| format!("{b:02x}")).collect();
                let verified = s3_proto::sigv4::authenticate_service(&req, chrono::Utc::now().timestamp(), "acm", &hash, |ak| (ak == "ASIAFAKE").then(|| "fake-session-secret".to_string()));
                if !matches!(verified, Ok(Some(_))) || headers.get("x-amz-security-token").and_then(|t| t.to_str().ok()) != Some("fake-session-token") || target != "CertificateManager.ImportCertificate" {
                    return (StatusCode::FORBIDDEN, axum::Json(json!({ "__type": "InvalidSignatureException" })));
                }
                return match asked["CertificateArn"].as_str() {
                    Some(arn) if arn.ends_with("/gone") => (StatusCode::BAD_REQUEST, axum::Json(json!({ "__type": "com.amazonaws.acm#ResourceNotFoundException" }))),
                    Some(arn) => (StatusCode::OK, axum::Json(json!({ "CertificateArn": arn }))),
                    None => {
                        *made += 1;
                        (StatusCode::OK, axum::Json(json!({ "CertificateArn": format!("arn:aws:acm:us-east-1:123456789012:certificate/fake-{made}") })))
                    }
                };
            }
            if pq == "/contoso.onmicrosoft.com/oauth2/v2.0/token" {
                return (StatusCode::OK, axum::Json(json!({ "access_token": "akv.fake", "token_type": "Bearer" })));
            }
            if pq.starts_with("/certificates/") {
                if headers.get("authorization").and_then(|a| a.to_str().ok()) != Some("Bearer akv.fake") {
                    return (StatusCode::UNAUTHORIZED, axum::Json(json!({ "error": { "code": "Unauthorized" } })));
                }
                return match (method.as_str(), pq.as_str()) {
                    ("GET", "/certificates/theirs-tls?api-version=7.4") => (StatusCode::OK, axum::Json(json!({ "tags": {} }))),
                    ("GET", _) if *made == 0 => (StatusCode::NOT_FOUND, axum::Json(json!({ "error": { "code": "CertificateNotFound" } }))),
                    ("GET", _) => (StatusCode::OK, axum::Json(json!({ "tags": { "managed-by": "roll-secrets" } }))),
                    ("POST", "/certificates/api-tls/import?api-version=7.4") => {
                        *made += 1;
                        (StatusCode::OK, axum::Json(json!({ "id": format!("https://shop-prod.vault.azure.net/certificates/api-tls/v{made}") })))
                    }
                    _ => (StatusCode::NOT_FOUND, axum::Json(json!({ "error": { "code": "NotFound" } }))),
                };
            }
            if let Some(rest) = pq.strip_prefix("/client/v4/zones/023e105f4ecef8ad9ca31a8372d0c353/custom_certificates") {
                if headers.get("authorization").and_then(|a| a.to_str().ok()) != Some("Bearer cf-fake-token") {
                    return (StatusCode::FORBIDDEN, axum::Json(json!({ "success": false, "errors": [{ "code": 10000 }] })));
                }
                return match (method.as_str(), rest) {
                    ("POST", "") => {
                        *made += 1;
                        (StatusCode::OK, axum::Json(json!({ "success": true, "result": { "id": format!("cf-cert-{made}") } })))
                    }
                    ("PATCH", "/cf-cert-1") => (StatusCode::OK, axum::Json(json!({ "success": true, "result": { "id": "cf-cert-1" } }))),
                    _ => (StatusCode::NOT_FOUND, axum::Json(json!({ "success": false, "errors": [{ "code": 1003 }] }))),
                };
            }
            (StatusCode::NOT_FOUND, axum::Json(Json::Null))
        }
        let m: Arc<Mock> = Default::default();
        let app = axum::Router::new().fallback(any).with_state(m.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (base, m)
    }

    /// An obviously fake certificate, key and chain: the `n`th issue.
    fn cert(n: u32) -> Certificate {
        let pem = |label: &str, body: String| Value::new(format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n"));
        Certificate { key: "TLS_CERT".into(), certificate: pem("CERTIFICATE", format!("fake-leaf-{n}")), private_key: pem("PRIVATE KEY", format!("fake-key-{n}")), chain: pem("CERTIFICATE", "fake-ca".into()) }
    }

    /// Check `target`, then push the `n`th certificate with `credentials` and `remote`, on a
    /// blocking thread as the store does.
    async fn push(t: Arc<dyn SyncTarget>, target: Json, n: u32, credentials: &[(&str, &str)], remote: Option<String>) -> Result<String> {
        let credentials: BTreeMap<String, Value> = credentials.iter().map(|(k, v)| (k.to_string(), Value::new(v.to_string()))).collect();
        tokio::task::spawn_blocking(move || {
            t.check("prj_1", &BTreeSet::new(), &target)?;
            let session = roll_secrets::aws::Session { access_key_id: Value::new("ASIAFAKE"), secret_access_key: Value::new("fake-session-secret"), session_token: Value::new("fake-session-token"), expires_at: 0 };
            let aws = t.aws_role(&target).map(|_| &session);
            let values = BTreeMap::new();
            let p = Push { sync: "syn_1", project: "prj_1", resources: &BTreeSet::new(), environment: "production", path: "/", digest: "d", at: 0, values: &values, changed: &[], removed: &[], secret: None, credentials: &credentials, aws };
            t.push_certificate(&target, &p, &cert(n), remote.as_deref())
        })
        .await
        .unwrap()
    }

    fn http() -> Http {
        Http::plain()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn aws_acm_reimports_a_renewed_certificate_to_the_same_arn() {
        let (base, m) = mock().await;
        let acm = || Arc::new(Acm(http(), Some(base.clone())));
        let t = json!({ "region": "us-east-1", "role": "certificates" });
        assert!(acm().check("prj_1", &BTreeSet::new(), &json!({ "region": "https://evil.example", "role": "certificates" })).is_err(), "a region is a name, never a URL");
        let arn = push(acm(), t.clone(), 1, &[], None).await.unwrap();
        assert_eq!(arn, "arn:aws:acm:us-east-1:123456789012:certificate/fake-1");
        assert_eq!(push(acm(), t.clone(), 2, &[], Some(arn.clone())).await.unwrap(), arn, "a renewal keeps the ARN listeners use");
        let seen = m.seen.lock().unwrap().clone();
        let decode = |v: &Json| String::from_utf8(base64::engine::general_purpose::STANDARD.decode(v.as_str().unwrap()).unwrap()).unwrap();
        assert!(seen[0].2["CertificateArn"].is_null() && seen[0].2["Tags"][0]["Value"] == "roll-secrets" && decode(&seen[0].2["Certificate"]).contains("fake-leaf-1"));
        assert!(seen[1].2["CertificateArn"] == arn.as_str() && seen[1].2["Tags"].is_null() && decode(&seen[1].2["Certificate"]).contains("fake-leaf-2"));
        assert!(decode(&seen[1].2["PrivateKey"]).contains("fake-key-2") && decode(&seen[1].2["CertificateChain"]).contains("fake-ca"));
        // Deleted at AWS: imported as a new certificate.
        let gone = "arn:aws:acm:us-east-1:123456789012:certificate/gone".to_string();
        assert_eq!(push(acm(), t, 3, &[], Some(gone)).await.unwrap(), "arn:aws:acm:us-east-1:123456789012:certificate/fake-2");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn azure_keyvault_cert_imports_a_new_version_of_its_own_certificate_on_renewal() {
        let (base, m) = mock().await;
        let kv = || Arc::new(KeyVaultCert(http(), Azure { login: base.clone(), vault: Some(base.clone()) }));
        let t = |name: &str| json!({ "vault": "shop-prod", "certificate": name, "tenant_id": "contoso.onmicrosoft.com", "client_id": "00000000-0000-0000-0000-0000000000aa", "client_secret_key": "AZURE_SYNC_CLIENT_SECRET" });
        let creds = [("AZURE_SYNC_CLIENT_SECRET", "fake-client-secret")];
        let v1 = push(kv(), t("api-tls"), 1, &creds, None).await.unwrap();
        let v2 = push(kv(), t("api-tls"), 2, &creds, Some(v1.clone())).await.unwrap();
        assert_eq!((v1.as_str(), v2.as_str()), ("https://shop-prod.vault.azure.net/certificates/api-tls/v1", "https://shop-prod.vault.azure.net/certificates/api-tls/v2"));
        let imports: Vec<Json> = m.seen.lock().unwrap().iter().filter(|(meth, pq, _)| meth == "POST" && pq.contains("/import")).map(|(_, _, b)| b.clone()).collect();
        assert_eq!(imports.len(), 2);
        let pem = imports[1]["value"].as_str().unwrap();
        assert!(pem.contains("fake-leaf-2") && pem.contains("fake-ca") && pem.contains("fake-key-2") && imports[1]["policy"]["secret_props"]["contentType"] == "application/x-pem-file");
        let e = push(kv(), t("theirs-tls"), 3, &creds, None).await.unwrap_err().to_string();
        assert!(e.contains("roll didn't create it") && !e.contains("fake-key"), "{e}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cloudflare_cert_patches_the_same_custom_certificate_on_renewal() {
        let (base, m) = mock().await;
        let cf = || Arc::new(CloudflareCert(http(), base.clone()));
        let t = json!({ "zone": "023e105f4ecef8ad9ca31a8372d0c353", "token_key": "CLOUDFLARE_API_TOKEN" });
        let creds = [("CLOUDFLARE_API_TOKEN", "cf-fake-token")];
        let id = push(cf(), t.clone(), 1, &creds, None).await.unwrap();
        assert_eq!(id, "cf-cert-1");
        assert_eq!(push(cf(), t.clone(), 2, &creds, Some(id)).await.unwrap(), "cf-cert-1", "a renewal patches the same certificate");
        let seen = m.seen.lock().unwrap().clone();
        let route: Vec<(&str, &str)> = seen.iter().map(|(meth, pq, _)| (meth.as_str(), pq.as_str())).collect();
        let at = "/client/v4/zones/023e105f4ecef8ad9ca31a8372d0c353/custom_certificates";
        assert_eq!(route, [("POST", at), ("PATCH", &*format!("{at}/cf-cert-1"))]);
        assert!(seen[1].2["certificate"].as_str().unwrap().contains("fake-leaf-2") && seen[1].2["private_key"].as_str().unwrap().contains("fake-key-2") && seen[1].2["bundle_method"] == "force");
        // A wrong token: the error names Cloudflare's code, never the key.
        let e = push(cf(), t, 3, &[("CLOUDFLARE_API_TOKEN", "wrong")], None).await.unwrap_err().to_string();
        assert!(e.contains("10000") && !e.contains("fake-key"), "{e}");
    }
}
