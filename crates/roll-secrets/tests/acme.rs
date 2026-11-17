//! ACME on a project's CA (docs/secrets.md, "ACME"), driven by a real ACME client (instant-acme)
//! in process: its HTTP goes straight to the router, and challenges are answered by a fake web
//! server, a fake DNS zone and fake `acme-tls/1` servers. Nothing here reaches the internet. On SQLite by default;
//! `ROLL_CODE_TEST_DATABASE=postgres://…` runs it on Postgres.

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use instant_acme::{Account, AuthorizationStatus, BodyWrapper, BytesResponse, ChallengeType, ExternalAccountKey, HttpClient, Identifier, NewAccount, NewOrder, OrderStatus, RetryPolicy, RevocationRequest};
use roll_secrets::acme::AlpnFailure;
use roll_secrets::{Keyring, Options, Secrets};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const API: &str = "https://roll.example";

/// The internet as far as challenges go: web pages by URL, TXT records by name, and the digest
/// each name's `acme-tls/1` certificate carries.
#[derive(Default)]
struct World {
    pages: Mutex<HashMap<String, String>>,
    txt: Mutex<HashMap<String, Vec<String>>>,
    alpn: Mutex<HashMap<String, Vec<u8>>>,
}

impl roll_secrets::acme::Checks for World {
    fn txt(&self, name: &str) -> Result<Vec<String>, String> {
        self.txt.lock().unwrap().get(name).cloned().ok_or_else(|| "NXDOMAIN".into())
    }
    fn get(&self, url: &str, limit: usize) -> Result<Vec<u8>, String> {
        let page = self.pages.lock().unwrap().get(url).cloned().ok_or("404 Not Found")?;
        Ok(page.into_bytes().into_iter().take(limit).collect())
    }
    fn tls_alpn(&self, name: &str, digest: &[u8; 32]) -> Result<(), AlpnFailure> {
        match self.alpn.lock().unwrap().get(name) {
            None => Err(AlpnFailure::Tls("connection refused".into())),
            Some(d) if d[..] == digest[..] => Ok(()),
            Some(_) => Err(AlpnFailure::Incorrect("the acmeIdentifier isn't the key authorization's digest".into())),
        }
    }
}

/// Every POST sent: its path and body.
type Posts = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

/// instant-acme's HTTP, answered by the router; every POST is kept, to replay one.
#[derive(Clone)]
struct InProcess {
    router: Router,
    posts: Posts,
}

impl HttpClient for InProcess {
    fn request(&self, req: Request<BodyWrapper<bytes::Bytes>>) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>> {
        let (router, posts) = (self.router.clone(), self.posts.clone());
        Box::pin(async move {
            let (parts, body) = req.into_parts();
            let body = body.collect().await.map(|b| b.to_bytes()).unwrap_or_default();
            let path = parts.uri.path_and_query().map(|p| p.to_string()).unwrap_or_default();
            if parts.method == Method::POST {
                posts.lock().unwrap().push((path.clone(), body.to_vec()));
            }
            let mut req = Request::builder().method(parts.method).uri(path);
            for (k, v) in &parts.headers {
                req = req.header(k, v);
            }
            let resp = router.oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
            Ok(BytesResponse::from(resp))
        })
    }
}

struct Setup {
    router: Router,
    client: InProcess,
    world: Arc<World>,
    db: code_sql::Db,
    project: String,
    _dir: tempfile::TempDir,
}

impl Setup {
    async fn new() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let ws_db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
        let ws = Arc::new(roll_workspace::Workspace::new(ws_db).unwrap());
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("secrets.db")), "secrets").unwrap();
        let opts = Options { keyring: Ok(Keyring::new(&"k".repeat(32), None).unwrap()), key_source: roll_secrets::KeySource::Env, links: Default::default(), syncs: Default::default(), open: true, trust_forwarded_for: false, web_url: None, deliver: None, grants: None, people: None, kms: None, sts: None, outside: None, clouds: None, workloads: None };
        let secrets = Arc::new(Secrets::new(db.clone(), ws.clone(), opts).unwrap());
        secrets.set_public_api(API.into());
        let world = Arc::new(World::default());
        secrets.set_acme_checks(world.clone());
        let router = roll_workspace::router(ws).merge(roll_secrets::router(secrets));
        let client = InProcess { router: router.clone(), posts: Default::default() };
        let mut s = Setup { router, client, world, db, project: String::new(), _dir: dir };
        s.project = s.call(Method::POST, "/v1/workspace/projects", Some(json!({ "name": "shop" }))).await.1["id"].as_str().unwrap().to_string();
        s
    }

    /// The operator, on the project's routes (`/cas…`) or any other (`/v1/…`).
    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let path = if path.starts_with("/v1/") { path.to_string() } else { format!("/v1/secrets/projects/{}{path}", self.project) };
        let req = Request::builder().method(method).uri(path).header("content-type", "application/json").body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap();
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into())))
    }

    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.call(method.clone(), path, body).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    /// An ACME account made with a binding's key (or none).
    async fn account(&self, directory: &str, eab: Option<&Value>) -> Result<Account, instant_acme::Error> {
        let key = eab.map(|k| {
            use base64::Engine;
            ExternalAccountKey::new(k["key_id"].as_str().unwrap().into(), &base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(k["hmac_key"].as_str().unwrap()).unwrap())
        });
        let new = NewAccount { contact: &["mailto:ops@svc.internal"], terms_of_service_agreed: true, only_return_existing: false };
        Account::builder_with_http(Box::new(self.client.clone())).create(&new, directory.into(), key.as_ref()).await.map(|(a, _)| a)
    }
}

/// The problem type an ACME error carries.
fn problem(e: instant_acme::Error) -> String {
    match e {
        instant_acme::Error::Api(p) => p.r#type.unwrap_or_default().trim_start_matches("urn:ietf:params:acme:error:").to_string(),
        e => panic!("not a problem document: {e}"),
    }
}

fn dns(name: &str) -> Identifier {
    Identifier::Dns(name.into())
}

#[tokio::test]
async fn an_acme_client_gets_certificates_from_a_projects_ca_only_with_a_live_binding() {
    let s = Setup::new().await;
    let ca = s.ok(Method::POST, "/cas", Some(json!({ "name": "internal", "domains": ["svc.internal"] }))).await;
    let id = ca["id"].as_str().unwrap().to_string();
    let settings = s.ok(Method::GET, &format!("/cas/{id}/acme"), None).await;
    let directory = settings["directory"].as_str().unwrap().to_string();
    assert_eq!(directory, format!("{API}/v1/secrets/acme/{id}/directory"));
    assert_eq!(s.ok(Method::GET, &directory[API.len()..], None).await["meta"]["externalAccountRequired"], true);

    // No account without a binding.
    assert_eq!(problem(s.account(&directory, None).await.err().unwrap()), "externalAccountRequired");
    let eab = s.ok(Method::POST, &format!("/cas/{id}/acme/eab"), Some(json!({ "label": "cluster-a" }))).await;
    let account = s.account(&directory, Some(&eab)).await.unwrap();

    // Names outside the CA's policy are refused.
    assert_eq!(problem(account.new_order(&NewOrder::new(&[dns("api.example.com")])).await.err().unwrap()), "rejectedIdentifier");

    // http-01 for a name, dns-01 for a wildcard, tls-alpn-01 for another name, through the fake
    // web server, zone and acme-tls/1 server.
    let mut order = account.new_order(&NewOrder::new(&[dns("api.svc.internal"), dns("*.web.svc.internal"), dns("edge.svc.internal")])).await.unwrap();
    let mut edge_authz = String::new();
    let mut authzs = order.authorizations();
    while let Some(a) = authzs.next().await {
        let mut a = a.unwrap();
        assert_eq!(a.status, AuthorizationStatus::Pending);
        let name = a.identifier().to_string();
        let kind = match name.as_str() {
            "*.web.svc.internal" => ChallengeType::Dns01,
            "edge.svc.internal" => ChallengeType::TlsAlpn01,
            _ => ChallengeType::Http01,
        };
        if kind == ChallengeType::TlsAlpn01 {
            edge_authz = a.url().to_string();
        }
        let mut ch = a.challenge(kind.clone()).unwrap();
        match kind {
            ChallengeType::Dns01 => drop(s.world.txt.lock().unwrap().insert("_acme-challenge.web.svc.internal".into(), vec!["unrelated".into(), ch.key_authorization().dns_value()])),
            ChallengeType::TlsAlpn01 => drop(s.world.alpn.lock().unwrap().insert("edge.svc.internal".into(), ch.key_authorization().digest().as_ref().to_vec())),
            _ => drop(s.world.pages.lock().unwrap().insert(format!("http://api.svc.internal/.well-known/acme-challenge/{}", ch.token), ch.key_authorization().as_str().to_string())),
        }
        ch.set_ready().await.unwrap();
    }
    let fast = RetryPolicy::new().initial_delay(std::time::Duration::from_millis(10));
    assert_eq!(order.poll_ready(&fast).await.unwrap(), OrderStatus::Ready);
    // Replaying a request (its nonce) is refused, whatever it says.
    let (path, body) = s.client.posts.lock().unwrap().last().cloned().unwrap();
    let replay = Request::post(path).header("content-type", "application/jose+json").body(Body::from(body)).unwrap();
    let resp = s.router.clone().oneshot(replay).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(resp.headers().contains_key("replay-nonce"));
    let replayed: Value = serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(replayed["type"], "urn:ietf:params:acme:error:badNonce", "{replayed}");

    // Finalized to a certificate for exactly those names, chaining to the CA, on its list.
    order.finalize().await.unwrap();
    let chain = order.poll_certificate(&fast).await.unwrap();
    let pems: Vec<_> = x509_parser::pem::Pem::iter_from_buffer(chain.as_bytes()).map(|p| p.unwrap()).collect();
    let leaf = pems[0].parse_x509().unwrap();
    let ca_cert = x509_parser::pem::parse_x509_pem(ca["certificate"].as_str().unwrap().as_bytes()).unwrap().1;
    let ca_cert = ca_cert.parse_x509().unwrap();
    leaf.verify_signature(Some(ca_cert.public_key())).expect("the certificate chains to the CA");
    assert_eq!(pems[1].contents, x509_parser::pem::parse_x509_pem(ca["certificate"].as_str().unwrap().as_bytes()).unwrap().1.contents);
    let serial: String = leaf.raw_serial().iter().map(|b| format!("{b:02x}")).collect();
    let listed = s.ok(Method::GET, &format!("/cas/{id}/certificates"), None).await;
    let row = listed.as_array().unwrap().iter().find(|c| c["serial"] == serial.as_str()).expect("listed");
    assert_eq!((row["source"].as_str(), row["names"].clone()), (Some("acme"), json!(["*.web.svc.internal", "api.svc.internal", "edge.svc.internal"])));
    assert_eq!(row["issued_by"], format!("acme:{}", eab["key_id"].as_str().unwrap()));

    // revoke-cert by the ordering account puts it on the CA's CRL.
    let der = tokio_rustls::rustls::pki_types::CertificateDer::from(pems[0].contents.clone());
    account.revoke(&RevocationRequest { certificate: &der, reason: None }).await.unwrap();
    let crl = s.router.clone().oneshot(Request::get(format!("/v1/secrets/crl/{id}")).body(Body::empty()).unwrap()).await.unwrap();
    let crl = crl.into_body().collect().await.unwrap().to_bytes();
    let (_, crl) = x509_parser::parse_x509_crl(&crl).unwrap();
    assert!(crl.iter_revoked_certificates().any(|r| r.raw_serial() == leaf.raw_serial()), "the CRL lists the revoked certificate");
    let audit = s.ok(Method::GET, "/audit", None).await;
    let actions: Vec<&str> = audit.as_array().unwrap().iter().filter_map(|e| e["action"].as_str()).collect();
    assert!(actions.contains(&"ca.acme.issue") && actions.contains(&"ca.revoke"), "{actions:?}");

    // A name the account proved is ready again at once: the order lists the same authorization.
    let mut again = account.new_order(&NewOrder::new(&[dns("edge.svc.internal")])).await.unwrap();
    assert_eq!(again.state().status, OrderStatus::Ready);
    let reused = {
        let mut authzs = again.authorizations();
        let a = authzs.next().await.unwrap().unwrap();
        (a.url().to_string(), a.status)
    };
    assert_eq!(reused, (edge_authz, AuthorizationStatus::Valid));
    // A binding narrowed since (no route narrows one yet; the row stands for it) stops it at finalize.
    let narrowed = s.db.write(|c| c.execute("UPDATE acme_bindings SET names = ? WHERE id = ?", [r#"["api.svc.internal"]"#, eab["key_id"].as_str().unwrap()]));
    assert_eq!(narrowed.unwrap(), 1);
    assert_eq!(problem(again.finalize().await.err().unwrap()), "unauthorized");

    // Pre-authorized names: a binding's names are valid at once, nothing else is.
    s.ok(Method::PATCH, &format!("/cas/{id}/acme"), Some(json!({ "pre_authorize": true }))).await;
    let narrow = s.ok(Method::POST, &format!("/cas/{id}/acme/eab"), Some(json!({ "label": "db", "names": ["db.svc.internal"], "days": 30 }))).await;
    let mut db_account = s.account(&directory, Some(&narrow)).await.unwrap();
    // The account rolls its key (key-change): the new one signs from now on.
    let _credentials = db_account.update_key().await.unwrap();
    assert_eq!(problem(db_account.new_order(&NewOrder::new(&[dns("api.svc.internal")])).await.err().unwrap()), "rejectedIdentifier");
    let mut order = db_account.new_order(&NewOrder::new(&[dns("primary.db.svc.internal")])).await.unwrap();
    assert_eq!(order.state().status, OrderStatus::Ready);
    order.finalize().await.unwrap();
    assert!(order.poll_certificate(&fast).await.unwrap().contains("BEGIN CERTIFICATE"));

    // Revoking a binding stops its account at once.
    let (status, _) = s.call(Method::DELETE, &format!("/cas/{id}/acme/eab/{}", eab["key_id"].as_str().unwrap()), None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(problem(account.new_order(&NewOrder::new(&[dns("api.svc.internal")])).await.err().unwrap()), "unauthorized");
    let listed = s.ok(Method::GET, &format!("/cas/{id}/acme"), None).await;
    assert!(listed["bindings"].as_array().unwrap().iter().any(|b| b["id"] == eab["key_id"] && b["revoked_at"].is_i64() && b["accounts"] == 1));
}
