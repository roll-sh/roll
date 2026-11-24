//! `gcs` and `azure` repositories against their emulators in Docker (`fsouza/fake-gcs-server`,
//! Azurite; `scripts/search-repo-emulators.sh` starts them): each test runs only when its
//! emulator is named (`ROLL_SEARCH_GCS_TEST_ENDPOINT`, `ROLL_SEARCH_AZURE_TEST_ENDPOINT`) and
//! says so when it isn't. No test calls Google or Azure.
//!
//! roll reaches each emulator through a [`Recorder`], a proxy that keeps every request's line
//! and headers and can fail the parts of uploads, so a test sees what went over the wire: the
//! resumable chunks or staged blocks of a large object, the bearer token a service account got,
//! a cancelled upload. A GCS service account's tokens come from a local token endpoint that
//! checks the JWT's signature, issuer, audience and scope.

use super::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Azurite's well-known development account.
const AZURITE_ACCOUNT: &str = "devstoreaccount1";
const AZURITE_KEY: &str = "Eby8vdM02xNOcqFlqUwJPLlmEtlCDXJ1OUzFT50uSRZ6IFsuFq2UVErCz4I6tq/K1SZFPTOtr/KBHBeksoGMGw==";

/// A request a [`Recorder`] passed on.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// A part of an object written in parts: a GCS resumable chunk (not the last) or an Azure
    /// staged block.
    fn is_part(&self) -> bool {
        self.method == "PUT" && (self.header("content-range").is_some_and(|r| r.ends_with("/*")) || (self.uri.contains("comp=block") && !self.uri.contains("comp=blocklist")))
    }

    fn is_last_chunk(&self) -> bool {
        self.method == "PUT" && self.header("content-range").is_some_and(|r| r.starts_with("bytes ") && !r.ends_with("/*"))
    }
}

struct RecorderInner {
    upstream: String,
    me: String,
    seen: Mutex<Vec<Seen>>,
    fail_parts: AtomicBool,
    http: reqwest::Client,
}

/// A proxy in front of an emulator.
#[derive(Clone)]
struct Recorder(Arc<RecorderInner>);

impl Recorder {
    /// A proxy for `upstream` (`scheme://host:port`): its own `scheme://host:port`.
    async fn start(upstream: &str) -> (String, Recorder) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let me = format!("http://{}", listener.local_addr().unwrap());
        let up = reqwest::Url::parse(upstream).unwrap();
        let upstream = format!("{}://{}", up.scheme(), up.host_str().unwrap()) + &up.port().map(|p| format!(":{p}")).unwrap_or_default();
        let r = Recorder(Arc::new(RecorderInner { upstream, me: me.clone(), seen: Mutex::new(Vec::new()), fail_parts: AtomicBool::new(false), http: reqwest::Client::new() }));
        let app = axum::Router::new().fallback(forward).with_state(r.clone());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (me, r)
    }

    fn seen(&self) -> Vec<Seen> {
        self.0.seen.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.0.seen.lock().unwrap().clear();
    }
}

async fn forward(axum::extract::State(r): axum::extract::State<Recorder>, req: axum::extract::Request) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (parts, body) = req.into_parts();
    let uri = parts.uri.path_and_query().map(|p| p.to_string()).unwrap_or_default();
    let headers: Vec<(String, String)> = parts.headers.iter().map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or_default().to_string())).collect();
    let seen = Seen { method: parts.method.to_string(), uri: uri.clone(), headers };
    let fail = r.0.fail_parts.load(Ordering::Relaxed) && seen.is_part();
    r.0.seen.lock().unwrap().push(seen);
    let body = axum::body::to_bytes(body, 1 << 30).await.unwrap();
    if fail {
        return (axum::http::StatusCode::SERVICE_UNAVAILABLE, "failing parts").into_response();
    }
    let mut req = r.0.http.request(parts.method, format!("{}{uri}", r.0.upstream));
    for (k, v) in parts.headers.iter() {
        if k != "host" && k != "content-length" {
            req = req.header(k, v);
        }
    }
    let resp = req.body(body).send().await.unwrap();
    let mut out = axum::http::Response::builder().status(resp.status().as_u16());
    for (k, v) in resp.headers() {
        if matches!(k.as_str(), "transfer-encoding" | "connection" | "content-length") {
            continue;
        }
        // Sessions the emulator hands out are reached through the proxy.
        let v = match k.as_str() {
            "location" => v.to_str().unwrap().replace(&r.0.upstream, &r.0.me),
            _ => v.to_str().unwrap_or_default().to_string(),
        };
        out = out.header(k, v);
    }
    out.body(axum::body::Body::from(resp.bytes().await.unwrap())).unwrap()
}

/// A local OAuth token endpoint for one service account: checks each JWT and counts them.
struct TokenServer {
    uri: String,
    issued: AtomicUsize,
    /// Answer 400 to everyone.
    refuse: AtomicBool,
}

/// A fresh service account (its key file's JSON and its private key's PEM) and the token
/// endpoint that knows it.
async fn service_account() -> (String, String, Arc<TokenServer>) {
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    let key = rsa::RsaPrivateKey::new(&mut rand::thread_rng(), 2048).unwrap();
    let private = key.to_pkcs8_pem(LineEnding::LF).unwrap().to_string();
    let public = key.to_public_key().to_public_key_pem(LineEnding::LF).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/token", listener.local_addr().unwrap());
    let server = Arc::new(TokenServer { uri: uri.clone(), issued: AtomicUsize::new(0), refuse: AtomicBool::new(false) });
    let s = server.clone();
    let app = axum::Router::new().route(
        "/token",
        axum::routing::post(move |body: String| {
            let (s, public) = (s.clone(), public.clone());
            async move {
                use axum::response::IntoResponse;
                if s.refuse.load(Ordering::Relaxed) {
                    return (axum::http::StatusCode::BAD_REQUEST, axum::Json(json!({"error": "invalid_grant", "error_description": "Invalid JWT Signature."}))).into_response();
                }
                let form: std::collections::HashMap<&str, &str> = body.split('&').filter_map(|kv| kv.split_once('=')).collect();
                assert_eq!(form.get("grant_type").copied(), Some("urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer"));
                let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
                v.set_audience(&[s.uri.as_str()]);
                v.set_issuer(&["roll-test@test-project.iam.gserviceaccount.com"]);
                let claims = jsonwebtoken::decode::<Value>(form["assertion"], &jsonwebtoken::DecodingKey::from_rsa_pem(public.as_bytes()).unwrap(), &v).expect("a JWT signed with the account's key");
                assert_eq!(claims.claims["scope"], "https://www.googleapis.com/auth/devstorage.full_control");
                assert_eq!(claims.header.kid.as_deref(), Some("key-1"));
                let n = s.issued.fetch_add(1, Ordering::Relaxed) + 1;
                axum::Json(json!({"access_token": format!("ya29.roll-test-token-{n}"), "expires_in": 3599, "token_type": "Bearer"})).into_response()
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, app).await });
    let file = json!({
        "type": "service_account", "project_id": "test-project", "private_key_id": "key-1", "private_key": private,
        "client_email": "roll-test@test-project.iam.gserviceaccount.com", "client_id": "1", "token_uri": uri,
    });
    (file.to_string(), private, server)
}

/// Every log line the test binary writes, from the first call on.
fn captured_logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: std::sync::OnceLock<Arc<Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        struct W(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for W {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let b = buf.clone();
        let _ = tracing_subscriber::fmt().with_env_filter("debug").with_ansi(false).with_writer(move || W(b.clone())).try_init();
        buf
    })
    .clone()
}

/// A repository type's emulator, through a [`Recorder`].
struct Cloud {
    ty: &'static str,
    recorder: Recorder,
    /// roll's own client for it, for what a test reads or plants directly.
    store: search_es::blob::Store,
    /// Settings every repository of the test has (credentials, endpoint, the bucket).
    settings: Value,
    /// `base_path` of this test's repositories start with it.
    base: String,
    /// What must never show: keys, tokens.
    secrets: Vec<String>,
}

impl Cloud {
    async fn gcs(test: &str) -> Option<(Cloud, Arc<TokenServer>)> {
        let Some(emulator) = std::env::var("ROLL_SEARCH_GCS_TEST_ENDPOINT").ok().filter(|e| !e.is_empty()) else {
            eprintln!("ROLL_SEARCH_GCS_TEST_ENDPOINT isn't set (scripts/search-repo-emulators.sh): {test} not run");
            return None;
        };
        let (endpoint, recorder) = Recorder::start(&emulator).await;
        let bucket = "roll-backups";
        let r = reqwest::Client::new().post(format!("{emulator}/storage/v1/b?project=test-project")).json(&json!({"name": bucket})).send().await.unwrap();
        assert!(r.status().is_success() || r.status().as_u16() == 409, "creating the bucket: {}", r.status());
        let (file, private, tokens) = service_account().await;
        let store = search_es::blob::Store::Gcs(search_es::gcs::Client::new(reqwest::Url::parse(&emulator).unwrap(), bucket, None, None, &search_es::egress::Egress::of(|_| None, true).unwrap()).unwrap());
        // A line from the middle of the key: the PEM's armour isn't secret.
        let key_line = private.lines().nth(10).unwrap().to_string();
        let base = format!("{test}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let settings = json!({"bucket": bucket, "endpoint": endpoint, "credentials_file": file});
        Some((Cloud { ty: "gcs", recorder, store, settings, base, secrets: vec![key_line, "ya29.roll-test-token".into()] }, tokens))
    }

    async fn azure(test: &str) -> Option<Cloud> {
        let Some(emulator) = std::env::var("ROLL_SEARCH_AZURE_TEST_ENDPOINT").ok().filter(|e| !e.is_empty()) else {
            eprintln!("ROLL_SEARCH_AZURE_TEST_ENDPOINT isn't set (scripts/search-repo-emulators.sh): {test} not run");
            return None;
        };
        let (proxy, recorder) = Recorder::start(&emulator).await;
        // The account's path (`/devstoreaccount1`) stays in the endpoint.
        let endpoint = format!("{proxy}{}", reqwest::Url::parse(&emulator).unwrap().path().trim_end_matches('/'));
        let container = "roll-backups";
        let key = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, AZURITE_KEY).unwrap();
        let client = search_es::azure::Client::new(AZURITE_ACCOUNT, search_es::azure::Auth::Key(key), reqwest::Url::parse(&emulator).unwrap(), None, search_es::azure::LocationMode::PrimaryOnly, container, &search_es::egress::Egress::of(|_| None, true).unwrap()).unwrap();
        client.create_container().await.unwrap();
        let base = format!("{test}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let settings = json!({"container": container, "endpoint": endpoint, "account": AZURITE_ACCOUNT, "key": AZURITE_KEY});
        Some(Cloud { ty: "azure", recorder, store: search_es::blob::Store::Azure(client), settings, base, secrets: vec![AZURITE_KEY.into(), AZURITE_KEY[10..40].into()] })
    }

    /// A repository of this type under `<base>-<sub>`, with `extra` settings.
    fn repo(&self, sub: &str, extra: Value) -> Value {
        let mut settings = self.settings.clone();
        settings["base_path"] = json!(format!("{}-{sub}", self.base));
        for (k, v) in extra.as_object().unwrap() {
            settings[k] = v.clone();
        }
        json!({"type": self.ty, "settings": settings})
    }

    async fn objects(&self, sub: &str, under: &str) -> Vec<(String, u64)> {
        self.store.list(&format!("{}-{sub}/roll/{under}", self.base)).await.unwrap()
    }

    /// Fails when any of the secrets shows in `what`.
    fn hidden(&self, label: &str, what: &str) {
        for s in &self.secrets {
            assert!(!what.contains(s.as_str()), "{label} shows a credential");
        }
    }
}

/// The flow both types share: their settings validated as Elasticsearch validates them, a
/// snapshot whose large objects go up in parts and restore whole, a failed part abandoning its
/// object, throttles as an upper bound, `_analyze` against the emulator, `_cleanup`, and no
/// credential in any answer or log line.
async fn repository_flow(c: &Cloud, invalid: &[(Value, &str)]) -> Env {
    let logs = captured_logs();
    let e = env_with(|c| {
        c.node_settings.push((search_control::S3_ALLOW_PRIVATE.into(), "true".into()));
        c.sealer = search_control::Sealer::from_secret("0123456789abcdef0123456789abcdef");
    })
    .await;
    let (u, p) = creds(&e.post("/v1/search/clusters", json!({"name": c.ty})).await);
    for (settings, reason) in invalid {
        let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/bad?verify=false", Some(json!({"type": c.ty, "settings": settings}))).await;
        assert_eq!((s, v["error"]["caused_by"]["reason"].as_str()), (500, Some(*reason)), "{settings}: {v}");
    }

    // Registered and verified; the credentials never come back.
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/backup", Some(c.repo("main", json!({"chunk_size": "12mb"})))).await;
    assert_eq!(s, 200, "{v}");
    for path in ["/_snapshot/backup", "/_snapshot/_all", "/_cluster/state/metadata"] {
        let (s, v) = e.es(&u, &p, reqwest::Method::GET, path, None).await;
        assert_eq!(s, 200, "{v}");
        c.hidden(path, &v.to_string());
    }

    // ~18 MiB gzipped: chunks of 12 MiB, each in 8 MiB parts.
    index_docs(&e, &u, &p, "docs", &random_docs(24, 1 << 20, 7)).await;
    c.recorder.clear();
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/backup/one?wait_for_completion=true", Some(json!({"indices": "docs"}))).await;
    assert_eq!((s, v["snapshot"]["state"].as_str()), (200, Some("SUCCESS")), "{v}");
    let chunks: Vec<(String, u64)> = c.objects("main", "indices/").await.into_iter().filter(|(k, _)| k.ends_with(".gz")).collect();
    assert!(chunks.len() >= 2 && chunks.iter().any(|(_, n)| *n == 12 << 20) && chunks.iter().all(|(_, n)| *n <= 12 << 20), "{chunks:?}");
    let seen = c.recorder.seen();
    let parts = seen.iter().filter(|r| r.is_part()).count();
    assert!(parts >= 1, "no object went up in parts");
    if c.ty == "gcs" {
        assert!(seen.iter().any(Seen::is_last_chunk), "no resumable upload was completed by its last chunk");
    } else {
        assert!(seen.iter().any(|r| r.method == "PUT" && r.uri.contains("comp=blocklist")), "no block list was committed");
    }
    let (s, v) = e.es(&u, &p, reqwest::Method::POST, "/_snapshot/backup/one/_restore?wait_for_completion=true", Some(json!({"rename_pattern": "docs", "rename_replacement": "back"}))).await;
    assert_eq!(s, 200, "{v}");
    let (_, v) = e.es(&u, &p, reqwest::Method::GET, "/back/_count", None).await;
    assert_eq!(v["count"], 24, "{v}");
    let (_, from) = e.es(&u, &p, reqwest::Method::GET, "/docs/_doc/23", None).await;
    let (_, to) = e.es(&u, &p, reqwest::Method::GET, "/back/_doc/23", None).await;
    assert_eq!(from["_source"], to["_source"]);

    // A part the store fails abandons its object (a GCS session is cancelled), and the failed
    // snapshot leaves nothing behind.
    let before: Vec<String> = c.objects("main", "").await.into_iter().map(|(k, _)| k).collect();
    index_docs(&e, &u, &p, "more", &random_docs(12, 1 << 20, 9)).await;
    c.recorder.clear();
    c.recorder.0.fail_parts.store(true, Ordering::Relaxed);
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/backup/two?wait_for_completion=true", Some(json!({"indices": "more"}))).await;
    c.recorder.0.fail_parts.store(false, Ordering::Relaxed);
    assert!(s >= 500 || v["snapshot"]["state"] != "SUCCESS", "{v}");
    c.hidden("a failed snapshot's answer", &v.to_string());
    if c.ty == "gcs" {
        assert!(c.recorder.seen().iter().any(|r| r.method == "DELETE" && r.uri.contains("upload_id")), "the failed resumable upload wasn't cancelled");
    }
    let after: Vec<String> = c.objects("main", "").await.into_iter().map(|(k, _)| k).collect();
    assert_eq!(after, before, "the failed snapshot left objects behind");

    // Throttles bound transfers from above (a slow host only makes them slower).
    index_docs(&e, &u, &p, "small", &random_docs(4, 1 << 20, 3)).await;
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/slow", Some(c.repo("slow", json!({"max_snapshot_bytes_per_sec": "2mb", "max_restore_bytes_per_sec": "2mb"})))).await;
    assert_eq!(s, 200, "{v}");
    let rate = (2u64 << 20) as f64;
    let burst = (2u64 << 20) as f64;
    let started = std::time::Instant::now();
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/slow/s?wait_for_completion=true", Some(json!({"indices": "small"}))).await;
    let took = started.elapsed().as_secs_f64();
    assert_eq!((s, v["snapshot"]["state"].as_str()), (200, Some("SUCCESS")), "{v}");
    let (_, stats) = e.es(&u, &p, reqwest::Method::GET, "/_nodes/stats/repositories", None).await;
    let uploaded = stats["nodes"].as_object().unwrap().values().next().unwrap()["repositories"]["slow"]["uploaded_size_in_bytes"].as_f64().unwrap();
    eprintln!("{} snapshot: {uploaded:.0} bytes in {took:.2}s ({:.2} of the rate)", c.ty, uploaded / took / rate);
    assert!(uploaded > 0.0 && uploaded <= rate * took + burst, "{uploaded} bytes in {took}s is faster than {rate} B/s");
    let started = std::time::Instant::now();
    let (s, v) = e.es(&u, &p, reqwest::Method::POST, "/_snapshot/slow/s/_restore?wait_for_completion=true", Some(json!({"rename_pattern": "small", "rename_replacement": "small2"}))).await;
    let took = started.elapsed().as_secs_f64();
    assert_eq!(s, 200, "{v}");
    eprintln!("{} restore: {uploaded:.0} bytes in {took:.2}s ({:.2} of the rate)", c.ty, uploaded / took / rate);
    assert!(uploaded <= rate * took + burst, "the restore read {uploaded} bytes in {took}s, faster than {rate} B/s");

    // Repository analysis against the emulator: blobs past a part, overwrites, aborts, early
    // reads; nothing left behind.
    let path = "/_snapshot/backup/_analyze?blob_count=10&max_blob_size=10mb&max_total_data_size=40mb&seed=42&detailed=true&rare_action_probability=0.5&timeout=120s";
    let (s, v) = e.es(&u, &p, reqwest::Method::POST, path, None).await;
    assert_eq!(s, 200, "{v}");
    assert_eq!((v["blob_count"].as_i64(), &v["issues_detected"]), (Some(10), &json!([])), "{v}");
    assert!(v["details"].as_array().unwrap().iter().any(|d| d["blob"]["overwritten"] == true || d["blob"]["read_early"] == true), "no rare action ran: {v}");
    assert!(c.objects("main", "temp-analysis-").await.is_empty(), "the analysis left blobs behind");

    // `_cleanup` deletes what no snapshot references.
    let stray = format!("{}-main/roll/indices/stray/0/0123456789abcdef0123456789abcdef-1.0.gz", c.base);
    c.store.put(&stray, bytes::Bytes::from_static(b"0123456789")).await.unwrap();
    let (s, v) = e.es(&u, &p, reqwest::Method::POST, "/_snapshot/backup/_cleanup", None).await;
    assert_eq!((s, v["results"]["deleted_blobs"].as_i64()), (200, Some(1)), "{v}");
    assert!(c.objects("main", "indices/stray/").await.is_empty());

    // Nothing logged carries a credential.
    let logged = String::from_utf8_lossy(&logs.lock().unwrap()).to_string();
    assert!(!logged.is_empty(), "no log line was captured");
    c.hidden("a log line", &logged);
    e
}

/// A `gcs` repository on fake-gcs-server: Elasticsearch's validation, a service account's
/// token (fetched once, then cached) on every request, resumable uploads, and the shared flow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_gcs_repository_keeps_snapshots_in_a_bucket() {
    let Some((c, tokens)) = Cloud::gcs("gcs").await else { return };
    let invalid = [
        (json!({}), "[bad] Setting [bucket] is empty for repository"),
        (json!({"chunk_size": "0", "compress": "x"}), "[bad] Setting [bucket] is empty for repository"),
        (json!({"bucket": "b", "compress": "x", "max_snapshot_bytes_per_sec": "x"}), "Failed to parse value [x] as only [true] or [false] are allowed."),
        (json!({"bucket": "b", "max_snapshot_bytes_per_sec": "x", "chunk_size": "0"}), "failed to parse setting [max_snapshot_bytes_per_sec] with value [x] as a size in bytes: unit is missing or unrecognized"),
        (json!({"bucket": "b", "chunk_size": "0"}), "failed to parse value [0] for setting [chunk_size], must be >= [1b]"),
        (json!({"bucket": "b", "chunk_size": "6tb"}), "failed to parse value [6tb] for setting [chunk_size], must be <= [5tb]"),
        (json!({"bucket": "roll-backups", "readonly": "y"}), "Failed to parse value [y] as only [true] or [false] are allowed."),
        (json!({"bucket": "roll-backups", "credentials_file": {"type": "service_account"}}), "[credentials_file] must be given as a string"),
    ];
    let e = repository_flow(&c, &invalid).await;
    assert_eq!(tokens.issued.load(Ordering::Relaxed), 1, "the service account's token wasn't cached");
    let seen = c.recorder.seen();
    assert!(!seen.is_empty() && seen.iter().all(|r| r.header("authorization") == Some("Bearer ya29.roll-test-token-1")), "a request went without the account's token");

    // A token endpoint that refuses the account fails the repository's verification, saying
    // why without the key.
    let (file, _, refusing) = service_account().await;
    refusing.refuse.store(true, Ordering::Relaxed);
    let (u, p) = creds(&e.post("/v1/search/clusters", json!({"name": "refused"})).await);
    let mut repo = c.repo("refused", json!({}));
    repo["settings"]["credentials_file"] = json!(file);
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/refused", Some(repo)).await;
    assert_eq!((s, v["error"]["type"].as_str()), (500, Some("repository_verification_exception")), "{v}");
    assert!(v.to_string().contains("Invalid JWT Signature."), "{v}");
    c.hidden("a refused verification", &v.to_string());
    let private: Value = serde_json::from_str(&file).unwrap();
    assert!(!v.to_string().contains(private["private_key"].as_str().unwrap().lines().nth(10).unwrap()), "{v}");
}

/// An `azure` repository on Azurite: Elasticsearch's validation and client errors, Shared Key
/// signing, staged blocks, the shared flow, and a SAS token instead of the key.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_azure_repository_keeps_snapshots_in_a_container() {
    let Some(mut c) = Cloud::azure("azure").await else { return };
    let invalid = [
        (json!({"compress": "x", "chunk_size": "0"}), "Failed to parse value [x] as only [true] or [false] are allowed."),
        (json!({"max_snapshot_bytes_per_sec": "x", "chunk_size": "0"}), "failed to parse setting [max_snapshot_bytes_per_sec] with value [x] as a size in bytes: unit is missing or unrecognized"),
        (json!({"location_mode": "bogus", "chunk_size": "0"}), "failed to parse value [0] for setting [chunk_size], must be >= [1b]"),
        (json!({"chunk_size": "6tb"}), "failed to parse value [6tb] for setting [chunk_size], must be <= [2684354550000b]"),
        (json!({"location_mode": "bogus"}), "No enum constant org.elasticsearch.repositories.azure.LocationMode.BOGUS"),
        (json!({"key": {"value": "x"}}), "[key] must be given as a string"),
    ];
    let e = repository_flow(&c, &invalid).await;
    assert!(c.recorder.seen().iter().all(|r| r.header("authorization").is_some_and(|a| a.starts_with("SharedKey devstoreaccount1:"))), "a request went unsigned");
    let (u, p) = creds(&e.post("/v1/search/clusters", json!({"name": "clients"})).await);

    // A client Elasticsearch couldn't make: no account, a key and a SAS token at once.
    let mut no_account = c.repo("x", json!({}));
    no_account["settings"].as_object_mut().unwrap().remove("account");
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/nobody", Some(no_account)).await;
    assert_eq!((s, v["error"]["type"].as_str(), v["error"]["caused_by"]["type"].as_str()), (500, Some("repository_verification_exception"), Some("settings_exception")), "{v}");
    assert_eq!(v["error"]["caused_by"]["reason"], "Unable to find client with name [default]", "{v}");
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/both", Some(c.repo("x", json!({"sas_token": "sv=2019-02-02&sig=abc"})))).await;
    assert_eq!((s, v["error"]["caused_by"]["reason"].as_str()), (500, Some("Both a secret as well as a shared access token were set for account [devstoreaccount1]")), "{v}");
    // A wrong key: Azure refuses it, and the answer doesn't show it.
    let wrong = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, [7u8; 64]);
    c.secrets.push(wrong.clone());
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/wrong", Some(c.repo("x", json!({"key": wrong})))).await;
    assert_eq!((s, v["error"]["type"].as_str()), (500, Some("repository_verification_exception")), "{v}");
    c.hidden("a refused key's answer", &v.to_string());

    // A SAS token instead of the key (an account SAS, as the portal makes them).
    let sas = account_sas();
    c.secrets.push(sas.rsplit("sig=").next().unwrap().to_string());
    let mut with_sas = c.repo("sas", json!({"sas_token": sas}));
    with_sas["settings"].as_object_mut().unwrap().remove("key");
    c.recorder.clear();
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/sas", Some(with_sas)).await;
    assert_eq!(s, 200, "{v}");
    index_docs(&e, &u, &p, "tiny", &random_docs(3, 4 << 10, 5)).await;
    let (s, v) = e.es(&u, &p, reqwest::Method::PUT, "/_snapshot/sas/t?wait_for_completion=true", Some(json!({"indices": "tiny"}))).await;
    assert_eq!((s, v["snapshot"]["state"].as_str()), (200, Some("SUCCESS")), "{v}");
    let (s, v) = e.es(&u, &p, reqwest::Method::POST, "/_snapshot/sas/t/_restore?wait_for_completion=true", Some(json!({"rename_pattern": "tiny", "rename_replacement": "tiny2"}))).await;
    assert_eq!(s, 200, "{v}");
    assert!(c.recorder.seen().iter().all(|r| r.header("authorization").is_none() && r.uri.contains("sig=")), "a request went without the SAS token");
    for path in ["/_snapshot/sas", "/_cluster/state/metadata"] {
        let (_, v) = e.es(&u, &p, reqwest::Method::GET, path, None).await;
        c.hidden(path, &v.to_string());
    }
    c.hidden("a log line", &String::from_utf8_lossy(&captured_logs().lock().unwrap()));
}

/// An account SAS for Azurite's account (blob service, every resource type, read to create),
/// signed as Azure's documentation spells it for version 2019-02-02.
fn account_sas() -> String {
    use hmac::Mac;
    let (version, expiry) = ("2019-02-02", "2099-01-01T00:00:00Z");
    let to_sign = format!("{AZURITE_ACCOUNT}\nrwdlac\nb\nsco\n\n{expiry}\n\nhttps,http\n{version}\n");
    let key = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, AZURITE_KEY).unwrap();
    let mut m = hmac::Hmac::<sha2::Sha256>::new_from_slice(&key).unwrap();
    m.update(to_sign.as_bytes());
    let sig = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, m.finalize().into_bytes());
    format!("sv={version}&ss=b&srt=sco&sp=rwdlac&se={}&spr=https,http&sig={}", urlencoding::encode(expiry), urlencoding::encode(&sig))
}
