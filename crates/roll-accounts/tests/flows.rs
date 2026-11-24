//! `/v1/accounts` behind rolld's real gate (`roll_auth::check`), on SQLite in a tempdir, with
//! mail in a file outbox and fake GitHub and Google servers (docs/accounts.md). One test per
//! flow: an invite to a session by email code, each OAuth provider, TOTP, revocation, operator
//! routes, CSRF.

use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::{Method, Request, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use http_body_util::BodyExt;
use roll_accounts::{Accounts, Config, MailConfig, OAuthApp, Owners, Provider, ProviderConfig, Secret};
use roll_auth::{Access, AdminToken, Gate, Origins, Rule};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tower::ServiceExt;

const OPERATOR: &str = "test-operator-token-0123456789abcdef";
const WEB: &str = "https://roll.test";
const API: &str = "http://api.roll.test";

/// roll code's owners, in memory: names taken, and the ones made.
#[derive(Default)]
struct FakeOwners(Mutex<Vec<String>>);

impl Owners for FakeOwners {
    fn available(&self, handle: &str) -> Result<bool, String> {
        Ok(handle != "admin" && !self.0.lock().unwrap().iter().any(|h| h == handle))
    }

    fn create(&self, handle: &str, _name: &str) -> Result<Option<String>, String> {
        self.0.lock().unwrap().push(handle.to_string());
        Ok(Some(format!("own_{handle}")))
    }
}

/// What the fake providers saw: the PKCE challenge and nonce of each authorize URL, by state.
#[derive(Default)]
struct Seen {
    challenge: HashMap<String, String>,
    nonce: HashMap<String, String>,
    /// The fake GitHub's login for its user (`Ada-L` when not set): renamed by a test.
    github_login: Option<String>,
}

struct Env {
    router: Router,
    accounts: Arc<Accounts>,
    outbox: std::path::PathBuf,
    owners: Arc<FakeOwners>,
    seen: Arc<Mutex<Seen>>,
    google_issuer: String,
    _dir: tempfile::TempDir,
}

/// A browser: its cookies, and the CSRF token its last session answer carried.
#[derive(Default, Clone)]
struct Browser {
    cookies: HashMap<String, String>,
    csrf: Option<String>,
    /// Its User-Agent, if it sends one.
    user_agent: Option<String>,
    /// Where it calls from (the gate's client address).
    ip: Option<String>,
}

struct Answer {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    json: Value,
}

async fn fake_providers(seen: Arc<Mutex<Seen>>) -> String {
    #[derive(Clone)]
    struct S {
        seen: Arc<Mutex<Seen>>,
        base: Arc<Mutex<String>>,
    }
    // Both token endpoints: the code is `<state>`, and the verifier must match its challenge.
    async fn token(State(s): State<S>, Form(f): Form<HashMap<String, String>>) -> Json<Value> {
        let state = f.get("code").cloned().unwrap_or_default();
        let challenge = s.seen.lock().unwrap().challenge.get(&state).cloned().unwrap_or_default();
        let verifier = f.get("code_verifier").cloned().unwrap_or_default();
        let sent = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        if sent != challenge || f.get("client_secret").map(String::as_str) != Some("shh") {
            return Json(json!({ "error": "bad_verification_code" }));
        }
        let nonce = s.seen.lock().unwrap().nonce.get(&state).cloned().unwrap_or_default();
        let claims = json!({
            "iss": format!("{}/google", s.base.lock().unwrap()), "aud": "google-client", "exp": 4_000_000_000i64, "nonce": nonce,
            "sub": "g-7", "email": "grace@example.com", "email_verified": true, "name": "Grace",
        });
        let b64 = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        Json(json!({ "access_token": "gho_fake", "id_token": format!("{}.{}.sig", b64(&json!({ "alg": "RS256" })), b64(&claims)) }))
    }
    let base = Arc::new(Mutex::new(String::new()));
    let app = Router::new()
        .route("/token", post(token))
        .route("/user", get(|State(s): State<S>| async move { Json(json!({ "id": 42, "login": s.seen.lock().unwrap().github_login.clone().unwrap_or_else(|| "Ada-L".into()), "name": "Ada Lovelace" })) }))
        .route("/user/orgs", get(|| async { Json(json!([{ "login": "Roll-sh" }, { "login": "acme" }])) }))
        .route(
            "/user/emails",
            get(|| async { Json(json!([{ "email": "ada@old.example", "primary": false, "verified": true }, { "email": "ada@example.com", "primary": true, "verified": true }])) }),
        )
        .with_state(S { seen, base: base.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    *base.lock().unwrap() = url.clone();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    url
}

impl Env {
    async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let fake = fake_providers(seen.clone()).await;
        let app = |id: &str| OAuthApp { client_id: id.into(), client_secret: Secret::new("shh") };
        let provider = |p, id: &str, api: String| ProviderConfig { authorize_url: format!("{fake}/authorize"), token_url: format!("{fake}/token"), api_url: api, ..ProviderConfig::new(p, app(id)) };
        let outbox = dir.path().join("outbox");
        let config = Config {
            api_url: API.into(),
            web_url: WEB.into(),
            mail: MailConfig::Outbox(outbox.clone()),
            github: Some(provider(Provider::Github, "github-client", fake.clone())),
            google: Some(provider(Provider::Google, "google-client", format!("{fake}/google"))),
            secret_key: Some(Secret::new("0123456789abcdef0123456789abcdef")),
            open: false,
            // The fake SSO provider is on 127.0.0.1.
            sso_allow_private: true,
        };
        let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("accounts.db")), "accounts").unwrap();
        let owners = Arc::new(FakeOwners::default());
        let accounts = Arc::new(Accounts::new(db, config, Some(owners.clone())).await.unwrap());
        // As rolld composes it: products behind one gate that resolves the session cookie.
        let rules = vec![
            Rule { route: "/v1/accounts", access: Access::Product("accounts") },
            Rule { route: "/v1/workspace", access: Access::User("projects") },
        ];
        let gate = Gate::with_sessions(Some(AdminToken::new(OPERATOR).unwrap()), rules, accounts.sessions(), Origins::parse(WEB).unwrap(), API);
        // The client's address, as rolld's trusted proxies would read it (here: a test header).
        gate.set_client_ip(Arc::new(|req| req.headers().get("x-test-ip").and_then(|v| v.to_str().ok()).and_then(|v| v.parse().ok())));
        let router = roll_accounts::router(accounts.clone())
            .route("/v1/projects", get(|| async { Json(json!([])) }).post(|| async { StatusCode::CREATED }))
            .route("/v1/workspace/projects", get(|| async { Json(json!([])) }))
            .layer(axum::middleware::from_fn_with_state(gate, roll_auth::check));
        Env { router, accounts, outbox, owners, seen, google_issuer: format!("{fake}/google"), _dir: dir }
    }

    /// One request from `b` (its cookies, and its CSRF token on writes), keeping what it sets.
    async fn call(&self, b: &mut Browser, method: Method, path: &str, body: Option<Value>) -> Answer {
        let mut req = Request::builder().method(method.clone()).uri(path).header("content-type", "application/json").header("origin", WEB);
        if !b.cookies.is_empty() {
            req = req.header(header::COOKIE, b.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; "));
        }
        if let Some(c) = b.csrf.as_ref().filter(|_| method != Method::GET) {
            req = req.header("x-roll-csrf", c);
        }
        if let Some(ua) = &b.user_agent {
            req = req.header(header::USER_AGENT, ua);
        }
        if let Some(ip) = &b.ip {
            req = req.header("x-test-ip", ip);
        }
        let a = self.send(req.body(body.map_or(Body::empty(), |v| Body::from(v.to_string()))).unwrap()).await;
        for c in a.headers.get_all(header::SET_COOKIE) {
            let (kv, _) = c.to_str().unwrap().split_once(';').unwrap();
            let (k, v) = kv.split_once('=').unwrap();
            if v.is_empty() {
                b.cookies.remove(k);
            } else {
                b.cookies.insert(k.into(), v.into());
            }
        }
        if let Some(c) = a.json.get("csrf").and_then(Value::as_str) {
            b.csrf = Some(c.into());
        }
        a
    }

    async fn send(&self, req: Request<Body>) -> Answer {
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let (status, headers) = (resp.status(), resp.headers().clone());
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        Answer { status, headers, json: serde_json::from_slice(&bytes).unwrap_or(Value::Null) }
    }

    async fn operator(&self, method: Method, path: &str, body: Value) -> Answer {
        let req = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {OPERATOR}")).header("content-type", "application/json");
        self.send(req.body(Body::from(body.to_string())).unwrap()).await
    }

    async fn invite(&self, email: &str) -> String {
        let a = self.operator(Method::POST, "/v1/accounts/invites", json!({ "email": email })).await;
        assert_eq!(a.status, StatusCode::CREATED, "{}", a.json);
        assert!(a.json["url"].as_str().unwrap().starts_with(&format!("{WEB}/signin?invite=rli_")));
        a.json["token"].as_str().unwrap().to_string()
    }

    /// The code in the newest sign-in mail to `to`, once there are more than `already`.
    async fn code_for(&self, to: &str, already: usize) -> String {
        for _ in 0..400 {
            if count_codes(&self.outbox, to) > already {
                return newest_code(&self.outbox, to);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("no sign-in mail to {to}");
    }

    /// Sign `b` in by email code (with `invite` for a first sign-in); the answer.
    async fn email_sign_in(&self, b: &mut Browser, email: &str, invite: Option<&str>) -> Answer {
        let sent = count_codes(&self.outbox, email);
        let a = self.call(b, Method::POST, "/v1/accounts/email/start", Some(json!({ "email": email, "invite": invite }))).await;
        assert_eq!(a.status, StatusCode::ACCEPTED, "{}", a.json);
        let code = self.code_for(email, sent).await;
        let challenge = a.json["challenge"].as_str().unwrap().to_string();
        self.call(b, Method::POST, "/v1/accounts/email/verify", Some(json!({ "challenge": challenge, "code": code }))).await
    }

    /// Run an OAuth sign-in through the fake provider: start, "consent", callback.
    async fn oauth(&self, b: &mut Browser, provider: &str, body: Value) -> Answer {
        let a = self.call(b, Method::POST, &format!("/v1/accounts/oauth/{provider}/start"), Some(body)).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.json);
        let url = url::Url::parse(a.json["url"].as_str().unwrap()).unwrap();
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!((q["code_challenge_method"].as_str(), q["redirect_uri"].clone()), ("S256", format!("{API}/v1/accounts/oauth/{provider}/callback")));
        let state = q["state"].clone();
        {
            let mut seen = self.seen.lock().unwrap();
            seen.challenge.insert(state.clone(), q["code_challenge"].clone());
            seen.nonce.insert(state.clone(), q.get("nonce").cloned().unwrap_or_default());
        }
        self.call(b, Method::GET, &format!("/v1/accounts/oauth/{provider}/callback?code={state}&state={state}"), None).await
    }
}

fn read_codes(outbox: &std::path::Path, to: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(outbox)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                .map(|e| (e.file_name().to_string_lossy().into_owned(), serde_json::from_slice::<Value>(&std::fs::read(e.path()).unwrap()).unwrap()))
                .filter(|(_, m)| m["to"] == to && m["subject"].as_str().unwrap().contains("sign-in code"))
                .map(|(name, m)| (name, m["subject"].as_str().unwrap()[..6].to_string()))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn count_codes(outbox: &std::path::Path, to: &str) -> usize {
    read_codes(outbox, to).len()
}

fn newest_code(outbox: &std::path::Path, to: &str) -> String {
    read_codes(outbox, to).pop().unwrap().1
}

fn location(a: &Answer) -> &str {
    a.headers.get(header::LOCATION).unwrap().to_str().unwrap()
}

#[tokio::test]
async fn an_invite_becomes_a_session_by_email_code() {
    let env = Env::new().await;
    let token = env.invite("Ada@Example.com").await;
    let mut b = Browser::default();
    // Nobody signs in without an account or an invite, and the answer doesn't say so.
    let a = env.call(&mut b, Method::POST, "/v1/accounts/email/start", Some(json!({ "email": "eve@example.com" }))).await;
    assert_eq!(a.status, StatusCode::ACCEPTED);
    let a = env.call(&mut b, Method::POST, "/v1/accounts/email/verify", Some(json!({ "challenge": a.json["challenge"], "code": "000000" }))).await;
    assert_eq!((a.status, a.json["code"].as_str()), (StatusCode::BAD_REQUEST, Some("bad_code")));

    let a = env.call(&mut b, Method::POST, "/v1/accounts/invites/peek", Some(json!({ "token": token }))).await;
    assert_eq!(a.json["email"], "ada@example.com");
    // Resent (a lost mail): a fresh link, and the old one stops, a sign-in begun with it too.
    let sent = count_codes(&env.outbox, "ada@example.com");
    let begun = env.call(&mut Browser::default(), Method::POST, "/v1/accounts/email/start", Some(json!({ "email": "ada@example.com", "invite": token }))).await.json["challenge"].as_str().unwrap().to_string();
    let early = env.code_for("ada@example.com", sent).await;
    let list = env.operator(Method::GET, "/v1/accounts/invites", json!({})).await.json;
    let id = list.as_array().unwrap().iter().find(|i| i["email"] == "ada@example.com").unwrap()["id"].as_str().unwrap().to_string();
    let again = env.operator(Method::POST, &format!("/v1/accounts/invites/{id}/resend"), json!({})).await;
    assert_eq!(again.status, StatusCode::OK, "{}", again.json);
    let old = token;
    let token = again.json["token"].as_str().unwrap().to_string();
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/invites/peek", Some(json!({ "token": old }))).await.status, StatusCode::NOT_FOUND);
    let late = env.call(&mut Browser::default(), Method::POST, "/v1/accounts/email/verify", Some(json!({ "challenge": begun, "code": early }))).await;
    assert_eq!(late.json["code"], "bad_code", "a sign-in begun with the old link: {}", late.json);
    let a = env.email_sign_in(&mut b, "ada@example.com", Some(&token)).await;
    assert_eq!((a.status, a.json["status"].as_str()), (StatusCode::OK, Some("signed_in")), "{}", a.json);
    assert_eq!((a.json["user"]["handle"].as_str(), a.json["user"]["admin"].as_bool()), (Some("ada"), Some(false)));
    let set = a.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
    assert!(set.starts_with("roll_session=rls_") && set.contains("HttpOnly") && set.contains("SameSite=Lax"), "{set}");
    assert_eq!(*env.owners.0.lock().unwrap(), vec!["ada".to_string()], "a roll code owner on first sign-in");

    let a = env.call(&mut b, Method::GET, "/v1/accounts/session", None).await;
    assert_eq!((a.json["status"].as_str(), a.json["user"]["email"].as_str()), (Some("signed_in"), Some("ada@example.com")));
    // The invite is used up.
    let a = env.call(&mut Browser::default(), Method::POST, "/v1/accounts/invites/peek", Some(json!({ "token": token }))).await;
    assert_eq!(a.status, StatusCode::NOT_FOUND);
    // Signing in again needs no invite, and a code works once.
    let mut b2 = Browser::default();
    assert_eq!(env.email_sign_in(&mut b2, "ada@example.com", None).await.json["status"], "signed_in");
}

#[tokio::test]
async fn github_and_google_sign_in() {
    let env = Env::new().await;
    // GitHub: the invite's email is the account's primary verified one; the login is the handle.
    let invite = env.invite("ada@example.com").await;
    let mut b = Browser::default();
    let a = env.oauth(&mut b, "github", json!({ "invite": invite, "return_to": "/dashboard/code" })).await;
    assert_eq!((a.status, location(&a)), (StatusCode::SEE_OTHER, "https://roll.test/dashboard/code"));
    let me = env.call(&mut b, Method::GET, "/v1/accounts/me", None).await;
    assert_eq!((me.json["handle"].as_str(), me.json["identities"][0]["login"].as_str()), (Some("ada-l"), Some("Ada-L")), "{}", me.json);
    // Once linked, the identity signs in without an invite; a login renamed on GitHub since follows.
    env.seen.lock().unwrap().github_login = Some("Ada-New".into());
    let mut b = Browser::default();
    let a = env.oauth(&mut b, "github", json!({})).await;
    assert_eq!(location(&a), "https://roll.test/dashboard");
    let me = env.call(&mut b, Method::GET, "/v1/accounts/me", None).await;
    assert_eq!(me.json["identities"][0]["login"].as_str(), Some("Ada-New"), "{}", me.json);

    // No invite, no account: refused.
    let a = env.oauth(&mut Browser::default(), "google", json!({})).await;
    assert_eq!(location(&a), "https://roll.test/signin?error=not_invited");

    // Google: an ID token from the token endpoint, its nonce and audience checked.
    let invite = env.invite("grace@example.com").await;
    let mut g = Browser::default();
    let a = env.oauth(&mut g, "google", json!({ "invite": invite })).await;
    assert_eq!(location(&a), "https://roll.test/dashboard", "issuer {}", env.google_issuer);
    let me = env.call(&mut g, Method::GET, "/v1/accounts/me", None).await;
    assert_eq!((me.json["email"].as_str(), me.json["name"].as_str()), (Some("grace@example.com"), Some("Grace")));

    // A callback in another browser (without the cookie that binds it): refused.
    let mut other = Browser::default();
    let started = env.call(&mut other, Method::POST, "/v1/accounts/oauth/github/start", Some(json!({}))).await;
    let state: String = url::Url::parse(started.json["url"].as_str().unwrap()).unwrap().query_pairs().find(|(k, _)| k == "state").unwrap().1.into_owned();
    let a = env.call(&mut Browser::default(), Method::GET, &format!("/v1/accounts/oauth/github/callback?code={state}&state={state}"), None).await;
    assert_eq!(location(&a), "https://roll.test/signin?error=state");
}

/// A visitor's sign-in (roll apps' `access.sso`): straight to the provider (GitHub asking for
/// organisations when told), back through the callback to the product's gate with the verified
/// email and organisations, and on to where the gate says, with no account or session of roll's;
/// a refusal is roll's own page, in the gate's words.
#[tokio::test]
async fn a_visitor_signs_in_for_a_product_without_an_account() {
    struct Gate(Mutex<Vec<(String, String, Vec<String>)>>);
    #[async_trait::async_trait]
    impl roll_accounts::VisitGate for Gate {
        async fn admit(&self, request: &str, who: &roll_accounts::Visitor) -> Result<String, String> {
            self.0.lock().unwrap().push((request.to_string(), who.email.clone(), who.orgs.clone()));
            match request {
                "ok-req" => Ok("https://wiki.apps.test/__roll/visit?ticket=t".into()),
                _ => Err(format!("{} isn't let in <here>", who.email)),
            }
        }
    }
    let env = Env::new().await;
    let gate = Arc::new(Gate(Mutex::new(Vec::new())));
    env.accounts.set_visit_gate(gate.clone());
    assert_eq!(env.accounts.visit_url("github", "ok-req", true).as_deref(), Some(format!("{API}/v1/accounts/oauth/github/visit?r=ok%2Dreq&orgs=1").as_str()));
    let visit = |r: &'static str| {
        let env = &env;
        async move {
            let mut b = Browser::default();
            let a = env.call(&mut b, Method::GET, &format!("/v1/accounts/oauth/github/visit?r={r}&orgs=1"), None).await;
            assert_eq!(a.status, StatusCode::SEE_OTHER);
            let url = url::Url::parse(location(&a)).unwrap();
            let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
            assert_eq!(q["scope"], "read:user user:email read:org");
            let state = q["state"].clone();
            env.seen.lock().unwrap().challenge.insert(state.clone(), q["code_challenge"].clone());
            let done = env.call(&mut b, Method::GET, &format!("/v1/accounts/oauth/github/callback?code={state}&state={state}"), None).await;
            (done, b)
        }
    };
    let (a, b) = visit("ok-req").await;
    assert_eq!((a.status, location(&a)), (StatusCode::SEE_OTHER, "https://wiki.apps.test/__roll/visit?ticket=t"));
    assert!(b.cookies.is_empty(), "no session of roll's, and the sign-in's own cookie gone: {:?}", b.cookies);
    assert_eq!(gate.0.lock().unwrap()[0], ("ok-req".into(), "ada@example.com".into(), vec!["roll-sh".into(), "acme".into()]));
    let (a, _) = visit("no-req").await;
    assert_eq!((a.status, a.headers.get(header::LOCATION)), (StatusCode::FORBIDDEN, None), "roll's page, not the dashboard's sign-in");
}

#[tokio::test]
async fn totp_enrolment_then_a_sign_in_needs_it() {
    let env = Env::new().await;
    let invite = env.invite("ada@example.com").await;
    let mut b = Browser::default();
    env.email_sign_in(&mut b, "ada@example.com", Some(&invite)).await;
    let a = env.call(&mut b, Method::POST, "/v1/accounts/me/totp", None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert!(a.json["otpauth_url"].as_str().unwrap().starts_with("otpauth://totp/") && a.json["qr_png"].as_str().unwrap().starts_with("data:image/png;base64,"));
    let seed = roll_auth::totp::base32_decode(a.json["secret"].as_str().unwrap()).unwrap();
    let step = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 / roll_auth::totp::STEP_S;
    let code = |s: i64| format!("{:06}", roll_auth::totp::totp(&seed, s));
    let a = env.call(&mut b, Method::POST, "/v1/accounts/me/totp/confirm", Some(json!({ "code": code(step) }))).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    let recovery: Vec<String> = serde_json::from_value(a.json["recovery_codes"].clone()).unwrap();
    assert_eq!(recovery.len(), 10);

    // The next sign-in owes the second factor: the session reaches nothing else yet.
    let mut b = Browser::default();
    let a = env.email_sign_in(&mut b, "ada@example.com", None).await;
    assert_eq!(a.json, json!({ "status": "mfa_required", "methods": ["totp", "recovery"], "csrf": a.json["csrf"] }));
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(env.call(&mut b, Method::GET, "/v1/workspace/projects", None).await.status, StatusCode::UNAUTHORIZED);
    // A code is used once: the one that confirmed the app is spent.
    let a = env.call(&mut b, Method::POST, "/v1/accounts/signin/totp", Some(json!({ "code": code(step) }))).await;
    assert_eq!(a.json["code"], "bad_code");
    let a = env.call(&mut b, Method::POST, "/v1/accounts/signin/totp", Some(json!({ "code": code(step + 1) }))).await;
    assert_eq!((a.status, a.json["status"].as_str()), (StatusCode::OK, Some("signed_in")), "{}", a.json);
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.json["totp"], true);

    // A recovery code works once, in place of the app.
    let mut c = Browser::default();
    env.email_sign_in(&mut c, "ada@example.com", None).await;
    let a = env.call(&mut c, Method::POST, "/v1/accounts/signin/recovery", Some(json!({ "code": recovery[0].to_uppercase() }))).await;
    assert_eq!(a.json["status"], "signed_in", "{}", a.json);
    let mut d = Browser::default();
    env.email_sign_in(&mut d, "ada@example.com", None).await;
    let a = env.call(&mut d, Method::POST, "/v1/accounts/signin/recovery", Some(json!({ "code": recovery[0] }))).await;
    assert_eq!(a.json["code"], "bad_code");
}

#[tokio::test]
async fn revocation_is_immediate() {
    let env = Env::new().await;
    let invite = env.invite("ada@example.com").await;
    let (mut laptop, mut phone) = (Browser::default(), Browser::default());
    env.email_sign_in(&mut laptop, "ada@example.com", Some(&invite)).await;
    let a = env.email_sign_in(&mut phone, "ada@example.com", None).await;
    let phone_session = a.json["session"]["id"].as_str().unwrap().to_string();

    let a = env.call(&mut laptop, Method::GET, "/v1/accounts/me/sessions", None).await;
    assert_eq!(a.json.as_array().unwrap().len(), 2);
    let laptop_session = a.json.as_array().unwrap().iter().find(|s| s["current"] == true).unwrap()["id"].as_str().unwrap().to_string();
    let user = env.call(&mut laptop, Method::GET, "/v1/accounts/me", None).await.json["id"].as_str().unwrap().to_string();
    // What a team-access preview's session asks by the session's id (roll previews' visitors).
    assert!(env.accounts.session_live(&user, &phone_session).unwrap());
    let a = env.call(&mut laptop, Method::DELETE, &format!("/v1/accounts/me/sessions/{phone_session}"), None).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT);
    assert_eq!(env.call(&mut phone, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(env.call(&mut phone, Method::GET, "/v1/accounts/session", None).await.json["status"], "anonymous");
    assert!(!env.accounts.session_live(&user, &phone_session).unwrap());

    // "Sign out of all other sessions": every one but this browser's.
    let mut tablet = Browser::default();
    env.email_sign_in(&mut tablet, "ada@example.com", None).await;
    let a = env.call(&mut laptop, Method::DELETE, "/v1/accounts/me/sessions", None).await;
    assert_eq!((a.status, a.json["ended"].as_u64()), (StatusCode::OK, Some(1)), "{}", a.json);
    assert_eq!(env.call(&mut tablet, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(env.call(&mut laptop, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::OK);

    // Disabling the user ends the rest, on the next request.
    assert!(env.accounts.session_live(&user, &laptop_session).unwrap());
    let a = env.operator(Method::PATCH, &format!("/v1/accounts/users/{user}"), json!({ "disabled": true })).await;
    assert_eq!((a.status, a.json["disabled"].as_bool()), (StatusCode::OK, Some(true)));
    assert_eq!(env.call(&mut laptop, Method::GET, "/v1/workspace/projects", None).await.status, StatusCode::UNAUTHORIZED);
    assert!(!env.accounts.session_live(&user, &laptop_session).unwrap());
}

#[tokio::test]
async fn a_user_who_isnt_an_admin_gets_403_on_operator_routes() {
    let env = Env::new().await;
    let invite = env.invite("ada@example.com").await;
    let mut b = Browser::default();
    env.email_sign_in(&mut b, "ada@example.com", Some(&invite)).await;
    for (m, path) in [(Method::GET, "/v1/projects"), (Method::POST, "/v1/projects"), (Method::GET, "/v1/accounts/users"), (Method::POST, "/v1/accounts/invites")] {
        let a = env.call(&mut b, m.clone(), path, Some(json!({ "email": "x@example.com" }))).await;
        assert_eq!(a.status, StatusCode::FORBIDDEN, "{m} {path}: {}", a.json);
        assert!(a.json["error"].is_string() && ["operator_only", "admin_only"].contains(&a.json["code"].as_str().unwrap()), "{}", a.json);
    }
    assert_eq!(env.call(&mut b, Method::GET, "/v1/workspace/projects", None).await.status, StatusCode::OK, "projects are for every user");
    // Without a session: the usual 401.
    assert_eq!(env.call(&mut Browser::default(), Method::GET, "/v1/projects", None).await.status, StatusCode::UNAUTHORIZED);

    // An admin acts as the operator.
    let id = env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.json["id"].as_str().unwrap().to_string();
    env.operator(Method::PATCH, &format!("/v1/accounts/users/{id}"), json!({ "admin": true })).await;
    assert_eq!(env.call(&mut b, Method::POST, "/v1/projects", None).await.status, StatusCode::CREATED);
}

#[tokio::test]
async fn csrf_is_refused() {
    let env = Env::new().await;
    let invite = env.invite("ada@example.com").await;
    let mut b = Browser::default();
    env.email_sign_in(&mut b, "ada@example.com", Some(&invite)).await;
    let csrf = b.csrf.take().unwrap();
    let a = env.call(&mut b, Method::PATCH, "/v1/accounts/me", Some(json!({ "name": "Ada" }))).await;
    assert_eq!((a.status, a.json["code"].as_str()), (StatusCode::FORBIDDEN, Some("csrf")));
    // The right header from another origin: refused too.
    let cookie = b.cookies.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ");
    let req = Request::builder().method(Method::PATCH).uri("/v1/accounts/me").header("cookie", &cookie).header("x-roll-csrf", &csrf).header("origin", "https://evil.example");
    let a = env.send(req.body(Body::from(json!({ "name": "Ada" }).to_string())).unwrap()).await;
    assert_eq!((a.status, a.json["code"].as_str()), (StatusCode::FORBIDDEN, Some("csrf")));
    b.csrf = Some(csrf);
    let a = env.call(&mut b, Method::PATCH, "/v1/accounts/me", Some(json!({ "name": "Ada" }))).await;
    assert_eq!((a.status, a.json["name"].as_str()), (StatusCode::OK, Some("Ada")));
    // A request carrying a token is judged by it: it doesn't end the cookie's session.
    let req = Request::builder().uri("/v1/accounts/session").header("cookie", &cookie).header("authorization", "Bearer rlc_someone_elses");
    assert!(env.send(req.body(Body::empty()).unwrap()).await.headers.get(header::SET_COOKIE).is_none());

    // Login CSRF: another site's form posts the attacker's own code (any text can be the body)
    // to sign the visitor in to the attacker's account. Only JSON is taken from a browser.
    let sent = count_codes(&env.outbox, "ada@example.com");
    let start = env.call(&mut Browser::default(), Method::POST, "/v1/accounts/email/start", Some(json!({ "email": "ada@example.com" }))).await;
    let code = env.code_for("ada@example.com", sent).await;
    let body = format!("{{\"challenge\":\"{}\",\"code\":\"{code}\",\"pad\":\"=\"}}", start.json["challenge"].as_str().unwrap());
    let req = Request::builder().method(Method::POST).uri("/v1/accounts/email/verify").header("content-type", "text/plain").header("origin", "https://evil.example");
    let a = env.send(req.body(Body::from(body.clone())).unwrap()).await;
    assert_eq!(a.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert!(a.headers.get(header::SET_COOKIE).is_none());
    let req = Request::builder().method(Method::POST).uri("/v1/accounts/email/verify").header("content-type", "application/json");
    assert_eq!(env.send(req.body(Body::from(body)).unwrap()).await.status, StatusCode::OK, "the same code as JSON");
}

/// A company's OpenID Connect provider, faked: discovery, its keys, and a token endpoint that
/// signs an ES256 ID token for the code (a state), with the nonce its authorize URL carried and
/// whatever `next` says about the person.
#[derive(Clone, Default)]
struct Idp {
    base: Arc<Mutex<String>>,
    /// The nonce and PKCE challenge of each authorize URL, by state.
    seen: Arc<Mutex<HashMap<String, (String, String)>>>,
    /// Claims for the next ID token (email, email_verified, sub, nonce…).
    next: Arc<Mutex<Value>>,
    key: Arc<Vec<u8>>,
}

async fn fake_idp() -> Idp {
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
    let idp = Idp { key: Arc::new(pkcs8.as_ref().to_vec()), ..Idp::default() };
    let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
    let point = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &idp.key, &rng).unwrap().public_key().as_ref().to_vec();
    let jwks = json!({ "keys": [{ "kty": "EC", "crv": "P-256", "kid": "k1", "alg": "ES256", "x": b64(&point[1..33]), "y": b64(&point[33..]) }] });
    async fn token(State(idp): State<Idp>, Form(f): Form<HashMap<String, String>>) -> Json<Value> {
        use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
        let state = f.get("code").cloned().unwrap_or_default();
        let (nonce, challenge) = idp.seen.lock().unwrap().get(&state).cloned().unwrap_or_default();
        let sent = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(f.get("code_verifier").cloned().unwrap_or_default().as_bytes()));
        if sent != challenge || f.get("client_secret").map(String::as_str) != Some("s3cret") || f.get("client_id").map(String::as_str) != Some("roll-sso") {
            return Json(json!({ "error": "invalid_grant" }));
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
        let mut claims = json!({ "iss": idp.base.lock().unwrap().clone(), "aud": "roll-sso", "exp": now + 300, "iat": now, "nonce": nonce, "email_verified": true });
        for (k, v) in idp.next.lock().unwrap().as_object().cloned().unwrap_or_default() {
            claims[k] = v;
        }
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let input = format!("{}.{}", b64(json!({ "alg": "ES256", "kid": "k1", "typ": "JWT" }).to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let rng = ring::rand::SystemRandom::new();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &idp.key, &rng).unwrap();
        let sig = key.sign(&rng, input.as_bytes()).unwrap();
        Json(json!({ "access_token": "at", "token_type": "Bearer", "id_token": format!("{input}.{}", b64(sig.as_ref())) }))
    }
    let base = idp.base.clone();
    let app = Router::new()
        .route(
            "/.well-known/openid-configuration",
            get(move || {
                let b = base.lock().unwrap().clone();
                async move { Json(json!({ "issuer": b, "authorization_endpoint": format!("{b}/authorize"), "token_endpoint": format!("{b}/token"), "jwks_uri": format!("{b}/jwks") })) }
            }),
        )
        .route("/jwks", get(move || async move { Json(jwks) }))
        .route("/token", post(token))
        .with_state(idp.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    *idp.base.lock().unwrap() = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    idp
}

impl Env {
    /// Sign `b` in through SSO as `email`, the provider saying `claims` about them: start,
    /// the provider's answer, callback. The callback's answer (a redirect).
    async fn sso(&self, idp: &Idp, b: &mut Browser, email: &str, claims: Value) -> Answer {
        let a = self.call(b, Method::POST, "/v1/accounts/sso/start", Some(json!({ "email": email }))).await;
        assert_eq!(a.status, StatusCode::OK, "{}", a.json);
        let url = url::Url::parse(a.json["url"].as_str().unwrap()).unwrap();
        let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
        assert_eq!((q["client_id"].as_str(), q["login_hint"].as_str(), q["code_challenge_method"].as_str()), ("roll-sso", email, "S256"));
        let state = q["state"].clone();
        idp.seen.lock().unwrap().insert(state.clone(), (q["nonce"].clone(), q["code_challenge"].clone()));
        *idp.next.lock().unwrap() = claims;
        let callback = url::Url::parse(&q["redirect_uri"]).unwrap();
        self.call(b, Method::GET, &format!("{}?code={state}&state={state}", callback.path()), None).await
    }
}

/// TXT records from a table.
#[derive(Default)]
struct Txt(Mutex<HashMap<String, Vec<String>>>);

impl roll_accounts::sso::TxtLookup for Txt {
    fn txt<'a>(&'a self, host: &'a str) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<String>> + Send + 'a>> {
        let v = self.0.lock().unwrap().get(host).cloned().unwrap_or_default();
        Box::pin(async move { v })
    }
}

/// Enterprise SSO (docs/accounts.md, "SSO"): a connection's domains count once a TXT record
/// proves them; the provider's ID token (signature, issuer, audience, nonce, a verified email in
/// those domains) signs the person in, linking their account (an admin's only from their own
/// session; never over another subject's link); new people need an invite unless it provisions
/// them; with `enforce`, the domain signs in only that way and its other sessions end.
#[tokio::test]
async fn sso_signs_people_in_by_their_domain() {
    let env = Env::new().await;
    let idp = fake_idp().await;
    let issuer = idp.base.lock().unwrap().clone();
    let txt = Arc::new(Txt::default());
    env.accounts.set_txt_lookup(txt.clone());
    // Ada (a person) and Root (an admin) have accounts already, by email code.
    let mut ada = Browser::default();
    env.email_sign_in(&mut ada, "ada@acme.test", Some(&env.invite("ada@acme.test").await)).await;
    let root_invite = env.operator(Method::POST, "/v1/accounts/invites", json!({ "email": "root@acme.test", "admin": true })).await.json["token"].as_str().unwrap().to_string();
    let mut root = Browser::default();
    env.email_sign_in(&mut root, "root@acme.test", Some(&root_invite)).await;

    assert_eq!(env.operator(Method::POST, "/v1/accounts/sso", json!({ "issuer": issuer, "client_id": "x", "client_secret": "y", "domains": ["gmail.com"] })).await.json["code"], "public_domain");
    let conn = env.operator(Method::POST, "/v1/accounts/sso", json!({ "name": "Acme Okta", "issuer": issuer, "client_id": "roll-sso", "client_secret": "s3cret", "domains": ["Acme.test"] })).await;
    assert_eq!(conn.status, StatusCode::CREATED, "{}", conn.json);
    assert!(conn.json.get("client_secret").is_none() && conn.json["redirect_uri"].as_str().unwrap().ends_with("/callback"), "{}", conn.json);
    let id = conn.json["id"].as_str().unwrap().to_string();
    assert_eq!(env.operator(Method::POST, "/v1/accounts/sso", json!({ "issuer": issuer, "client_id": "x", "client_secret": "y", "domains": ["acme.test"] })).await.status, StatusCode::CONFLICT, "a domain has one connection");
    // Claimed, not proven: nothing routes to it until the TXT record is there.
    let start = |email: &str| {
        let (env, email) = (&env, email.to_string());
        async move { env.call(&mut Browser::default(), Method::POST, "/v1/accounts/sso/start", Some(json!({ "email": email }))).await.json["code"].clone() }
    };
    assert_eq!(start("ada@acme.test").await, "no_sso");
    assert_eq!(env.operator(Method::POST, &format!("/v1/accounts/sso/{id}/verify"), json!({})).await.json["verified"], json!([]));
    txt.0.lock().unwrap().insert("_roll-sso.acme.test".into(), vec![conn.json["dns"]["value"].as_str().unwrap().to_string()]);
    assert_eq!(env.operator(Method::POST, &format!("/v1/accounts/sso/{id}/verify"), json!({})).await.json["verified"], json!(["acme.test"]));
    assert_eq!(start("eve@elsewhere.test").await, "no_sso");

    // Ada through the provider: her account, linked.
    let mut b = Browser::default();
    let a = env.sso(&idp, &mut b, "ada@acme.test", json!({ "sub": "00u-ada", "email": "ada@acme.test" })).await;
    assert_eq!(location(&a), "https://roll.test/dashboard");
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.json["email"], "ada@acme.test");
    let refused = |claims: Value| {
        let (env, idp) = (&env, &idp);
        async move { location(&env.sso(idp, &mut Browser::default(), "ada@acme.test", claims).await).to_string() }
    };
    // What the provider doesn't vouch for: an unverified email, another domain, another sign-in's
    // nonce; and another subject with Ada's address (reassigned there) doesn't take her account.
    assert!(refused(json!({ "sub": "00u-ada", "email": "ada@acme.test", "email_verified": false })).await.ends_with("error=unverified_email"));
    assert!(refused(json!({ "sub": "00u-x", "email": "mallory@evil.test" })).await.ends_with("error=domain"));
    assert!(refused(json!({ "sub": "00u-ada", "email": "ada@acme.test", "nonce": "another" })).await.ends_with("error=provider"));
    assert!(refused(json!({ "sub": "00u-new-ada", "email": "ada@acme.test" })).await.ends_with("error=linked_elsewhere"));
    // An admin's account isn't linked by its email: Root links from their own session first.
    assert!(location(&env.sso(&idp, &mut Browser::default(), "root@acme.test", json!({ "sub": "00u-root", "email": "root@acme.test" })).await).ends_with("error=link_required"));
    let a = env.call(&mut root, Method::POST, "/v1/accounts/sso/start", Some(json!({ "intent": "link" }))).await;
    let q: HashMap<String, String> = url::Url::parse(a.json["url"].as_str().unwrap()).unwrap().query_pairs().into_owned().collect();
    idp.seen.lock().unwrap().insert(q["state"].clone(), (q["nonce"].clone(), q["code_challenge"].clone()));
    *idp.next.lock().unwrap() = json!({ "sub": "00u-root", "email": "root@acme.test" });
    let linked = env.call(&mut root, Method::GET, &format!("/v1/accounts/sso/{id}/callback?code={0}&state={0}", q["state"]), None).await;
    assert_eq!(location(&linked), "https://roll.test/dashboard/account");
    assert_eq!(location(&env.sso(&idp, &mut Browser::default(), "root@acme.test", json!({ "sub": "00u-root", "email": "root@acme.test" })).await), "https://roll.test/dashboard");
    // No account: an invite, or a connection that provisions (never an admin).
    assert!(refused(json!({ "sub": "00u-bob", "email": "bob@acme.test" })).await.ends_with("error=not_invited"));
    assert_eq!(env.operator(Method::PATCH, &format!("/v1/accounts/sso/{id}"), json!({ "provision": true })).await.status, StatusCode::OK);
    let mut bob = Browser::default();
    assert_eq!(location(&env.sso(&idp, &mut bob, "bob@acme.test", json!({ "sub": "00u-bob", "email": "bob@acme.test", "name": "Bob" })).await), "https://roll.test/dashboard");
    let me = env.call(&mut bob, Method::GET, "/v1/accounts/me", None).await;
    assert_eq!((me.json["email"].as_str(), me.json["admin"].as_bool()), (Some("bob@acme.test"), Some(false)), "{}", me.json);

    // Enforced: the domain signs in only through the provider. Ada's email-code session ends;
    // her SSO session stays; an email code is refused at once (account or not); the operator's
    // token still works.
    assert_eq!(env.operator(Method::PATCH, &format!("/v1/accounts/sso/{id}"), json!({ "enforce": true })).await.status, StatusCode::OK);
    assert_eq!(env.call(&mut ada, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::UNAUTHORIZED, "the email-code session ended");
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::OK, "the SSO session stays");
    // Her company's sign-in decides who she is: she can't move her address off the domain or
    // delete her account (her admin removes her).
    // (The session answer carries the CSRF token, as the dashboard reads it after a redirect.)
    let ada_handle = env.call(&mut b, Method::GET, "/v1/accounts/session", None).await.json["user"]["handle"].as_str().unwrap().to_string();
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/me/email", Some(json!({ "email": "ada@elsewhere.test" }))).await.json["code"], "sso_required");
    assert_eq!(env.call(&mut b, Method::DELETE, "/v1/accounts/me", Some(json!({ "confirm": ada_handle }))).await.json["code"], "sso_required");
    for who in ["ada@acme.test", "nobody@acme.test"] {
        let a = env.call(&mut Browser::default(), Method::POST, "/v1/accounts/email/start", Some(json!({ "email": who }))).await;
        assert_eq!((a.status, a.json["code"].as_str()), (StatusCode::FORBIDDEN, Some("sso_required")), "{}", a.json);
    }
    assert_eq!(env.operator(Method::GET, "/v1/accounts/sso", json!({})).await.status, StatusCode::OK);
    assert_eq!(env.call(&mut Browser::default(), Method::GET, "/v1/accounts", None).await.json["providers"]["sso"], true);

    // Removed: the domain is free again, and its people sign in other ways.
    assert_eq!(env.operator(Method::DELETE, &format!("/v1/accounts/sso/{id}"), json!({})).await.status, StatusCode::NO_CONTENT);
    let a = env.call(&mut Browser::default(), Method::POST, "/v1/accounts/email/start", Some(json!({ "email": "ada@acme.test" }))).await;
    assert_eq!(a.status, StatusCode::ACCEPTED);
}

/// SCIM (docs/accounts.md, "SCIM"): a connection's token lets its provider create people in its
/// verified domains (never admins, never elsewhere), find them by userName, and deactivate them,
/// which ends their sessions at once; a new token or a removed connection stops the old one.
#[tokio::test]
async fn scim_provisions_and_deactivates_people() {
    let env = Env::new().await;
    let idp = fake_idp().await;
    let issuer = idp.base.lock().unwrap().clone();
    let txt = Arc::new(Txt::default());
    env.accounts.set_txt_lookup(txt.clone());
    let root_invite = env.operator(Method::POST, "/v1/accounts/invites", json!({ "email": "root@acme.test", "admin": true })).await.json["token"].as_str().unwrap().to_string();
    let root = env.email_sign_in(&mut Browser::default(), "root@acme.test", Some(&root_invite)).await.json["user"]["id"].as_str().unwrap().to_string();
    let conn = env.operator(Method::POST, "/v1/accounts/sso", json!({ "issuer": issuer, "client_id": "roll-sso", "client_secret": "s3cret", "domains": ["acme.test"] })).await.json;
    let id = conn["id"].as_str().unwrap().to_string();
    txt.0.lock().unwrap().insert("_roll-sso.acme.test".into(), vec![conn["dns"]["value"].as_str().unwrap().to_string()]);
    env.operator(Method::POST, &format!("/v1/accounts/sso/{id}/verify"), json!({})).await;
    let made = env.operator(Method::POST, &format!("/v1/accounts/sso/{id}/scim"), json!({})).await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.json);
    let token = made.json["token"].as_str().unwrap().to_string();
    assert!(made.json["base_url"].as_str().unwrap().ends_with("/v1/accounts/scim/v2"));

    let scim = |method: Method, path: String, body: Option<Value>, token: String| {
        let env = &env;
        async move {
            let req = Request::builder().method(method).uri(format!("/v1/accounts/scim/v2{path}")).header("authorization", format!("Bearer {token}")).header("content-type", "application/scim+json");
            env.send(req.body(body.map_or(Body::empty(), |b| Body::from(b.to_string()))).unwrap()).await
        }
    };
    assert_eq!(scim(Method::GET, "/ServiceProviderConfig".into(), None, token.clone()).await.status, StatusCode::OK);
    assert_eq!(scim(Method::GET, "/Users".into(), None, "rlp_wrong".into()).await.status, StatusCode::UNAUTHORIZED);

    // Carol, made by the provider: not an admin, active, found by her userName.
    let carol = scim(Method::POST, "/Users".into(), Some(json!({ "schemas": ["urn:ietf:params:scim:schemas:core:2.0:User"], "userName": "carol@acme.test", "name": { "givenName": "Carol", "familyName": "Shaw" }, "externalId": "00u-carol", "active": true })), token.clone()).await;
    assert_eq!(carol.status, StatusCode::CREATED, "{}", carol.json);
    assert_eq!(carol.headers.get(header::CONTENT_TYPE).unwrap(), "application/scim+json");
    let carol_id = carol.json["id"].as_str().unwrap().to_string();
    assert_eq!((carol.json["active"].as_bool(), carol.json["displayName"].as_str(), carol.json["externalId"].as_str()), (Some(true), Some("Carol Shaw"), Some("00u-carol")));
    assert_eq!(scim(Method::POST, "/Users".into(), Some(json!({ "userName": "carol@acme.test" })), token.clone()).await.json["scimType"], "uniqueness");
    assert_eq!(scim(Method::POST, "/Users".into(), Some(json!({ "userName": "eve@elsewhere.test" })), token.clone()).await.status, StatusCode::BAD_REQUEST, "outside its domains");
    let found = scim(Method::GET, "/Users?filter=userName%20eq%20%22carol@acme.test%22".into(), None, token.clone()).await;
    assert_eq!((found.json["totalResults"].as_u64(), found.json["Resources"][0]["id"].as_str()), (Some(1), Some(carol_id.as_str())), "{}", found.json);
    // The admin is roll's to manage: invisible to SCIM.
    assert_eq!(scim(Method::GET, format!("/Users/{root}"), None, token.clone()).await.status, StatusCode::NOT_FOUND);
    assert_eq!(scim(Method::GET, "/Users".into(), None, token.clone()).await.json["totalResults"], 1, "Carol only");

    // Other products hear of each deactivation from the outbox (docs/accounts.md, "Offboarding":
    // rolld ends her leases, shares and sandboxes); a reactivation gives nothing back, so it's silent.
    let offboarded = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let heard = offboarded.clone();
    env.accounts.on_offboarded(Box::new(move |user| {
        heard.lock().unwrap().push(user.to_string());
        Ok(())
    }));

    // Carol signs in; deactivated by the provider (Entra's string), her session ends at once.
    let mut b = Browser::default();
    env.email_sign_in(&mut b, "carol@acme.test", None).await;
    let carol_handle = env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.json["handle"].as_str().unwrap().to_string();
    // The provider manages her: she can't move her account to another address or delete it.
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/me/email", Some(json!({ "email": "carol@elsewhere.test" }))).await.json["code"], "managed");
    assert_eq!(env.call(&mut b, Method::DELETE, "/v1/accounts/me", Some(json!({ "confirm": carol_handle }))).await.json["code"], "managed");
    let off = scim(Method::PATCH, format!("/Users/{carol_id}"), Some(json!({ "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": [{ "op": "Replace", "path": "active", "value": "False" }] })), token.clone()).await;
    assert_eq!((off.status, off.json["active"].as_bool()), (StatusCode::OK, Some(false)), "{}", off.json);
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::UNAUTHORIZED, "her session ended");
    let on = scim(Method::PATCH, format!("/Users/{carol_id}"), Some(json!({ "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": [{ "op": "replace", "value": { "active": true } }] })), token.clone()).await;
    assert_eq!(on.json["active"], true);
    assert_eq!(scim(Method::DELETE, format!("/Users/{carol_id}"), None, token.clone()).await.status, StatusCode::NO_CONTENT);
    assert_eq!(scim(Method::GET, format!("/Users/{carol_id}"), None, token.clone()).await.json["active"], false, "DELETE deactivates");
    assert_eq!(*offboarded.lock().unwrap(), [carol_id.as_str(), carol_id.as_str()], "the PATCH and the DELETE, not the reactivation");

    // Entra's userName is often a UPN, not the address: roll's address is the primary email,
    // and the UPN finds the person.
    let dave = scim(Method::POST, "/Users".into(), Some(json!({ "userName": "d.ruiz@acme.onmicrosoft.example", "emails": [{ "value": "dave@acme.test", "primary": true, "type": "work" }], "displayName": "Dave Ruiz" })), token.clone()).await;
    assert_eq!((dave.status, dave.json["emails"][0]["value"].as_str()), (StatusCode::CREATED, Some("dave@acme.test")), "{}", dave.json);
    assert!(dave.headers.get(header::LOCATION).is_some(), "201 says where");
    let dave_id = dave.json["id"].as_str().unwrap().to_string();
    let by_upn = scim(Method::GET, "/Users?filter=userName%20eq%20%22d.ruiz@acme.onmicrosoft.example%22".into(), None, token.clone()).await;
    assert_eq!(by_upn.json["Resources"][0]["id"].as_str(), Some(dave_id.as_str()), "{}", by_upn.json);
    // A deactivation roll can't read is refused, never silently nothing; a remove of what roll
    // keeps nothing of goes through with the rest.
    let patch = |ops: Value| json!({ "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": ops });
    assert_eq!(scim(Method::PATCH, format!("/Users/{dave_id}"), Some(patch(json!([{ "op": "replace", "path": "active", "value": "no" }]))), token.clone()).await.status, StatusCode::BAD_REQUEST);
    let mixed = scim(Method::PATCH, format!("/Users/{dave_id}"), Some(patch(json!([{ "op": "Remove", "path": "title" }, { "op": "Replace", "path": "active", "value": "False" }]))), token.clone()).await;
    assert_eq!((mixed.status, mixed.json["active"].as_bool()), (StatusCode::OK, Some(false)), "{}", mixed.json);
    // An admin's disable is roll's: SCIM can't lift it.
    let ruth = scim(Method::POST, "/Users".into(), Some(json!({ "userName": "ruth@acme.test" })), token.clone()).await.json["id"].as_str().unwrap().to_string();
    assert_eq!(env.operator(Method::PATCH, &format!("/v1/accounts/users/{ruth}"), json!({ "disabled": true })).await.status, StatusCode::OK);
    assert_eq!(scim(Method::PATCH, format!("/Users/{ruth}"), Some(patch(json!([{ "op": "replace", "path": "active", "value": true }]))), token.clone()).await.status, StatusCode::FORBIDDEN);
    assert_eq!(scim(Method::GET, format!("/Users/{ruth}"), None, token.clone()).await.json["active"], false);
    // What roll's SCIM doesn't serve is a SCIM 404 to the token (a 401 would read as a bad token).
    assert_eq!(scim(Method::GET, "/Groups".into(), None, token.clone()).await.status, StatusCode::NOT_FOUND);
    assert_eq!(scim(Method::GET, "/Groups".into(), None, "rlp_wrong".into()).await.status, StatusCode::UNAUTHORIZED);

    // A new token stops the old one; removing the connection stops SCIM.
    let token2 = env.operator(Method::POST, &format!("/v1/accounts/sso/{id}/scim"), json!({})).await.json["token"].as_str().unwrap().to_string();
    assert_eq!(scim(Method::GET, "/Users".into(), None, token.clone()).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(scim(Method::GET, "/Users".into(), None, token2.clone()).await.status, StatusCode::OK);

    // A leaked token (scanners find it by its checksum) stops at once, and the connection says
    // where it was found until a new one is made.
    assert!(roll_auth::credentials::checksum_ok(&token2), "a SCIM token is checksummed: {}", &token2[..4]);
    let revoked = env.accounts.revoke_leaked_scim(&token2, "found in acme/app (README.md)");
    assert!(revoked.is_some_and(|w| w.contains("SCIM token")));
    assert_eq!(env.accounts.revoke_leaked_scim(&token2, "again"), None, "only a live token is revoked");
    assert_eq!(scim(Method::GET, "/Users".into(), None, token2.clone()).await.status, StatusCode::UNAUTHORIZED);
    let listed = env.operator(Method::GET, "/v1/accounts/sso", json!({})).await.json;
    let c = listed.as_array().unwrap().iter().find(|c| c["id"] == id.as_str()).unwrap().clone();
    assert_eq!((c["scim"].clone(), c["scim_leaked"].clone()), (json!(false), json!("found in acme/app (README.md)")));
    let token3 = env.operator(Method::POST, &format!("/v1/accounts/sso/{id}/scim"), json!({})).await.json["token"].as_str().unwrap().to_string();
    assert_eq!(scim(Method::GET, "/Users".into(), None, token3).await.status, StatusCode::OK);
    let listed = env.operator(Method::GET, "/v1/accounts/sso", json!({})).await.json;
    assert_eq!(listed.as_array().unwrap().iter().find(|c| c["id"] == id.as_str()).unwrap()["scim_leaked"], Value::Null);
    env.operator(Method::DELETE, &format!("/v1/accounts/sso/{id}"), json!({})).await;
    assert_eq!(scim(Method::GET, "/Users".into(), None, token2).await.status, StatusCode::UNAUTHORIZED);
}

/// The workspace as rolld hands it to SCIM (`rolld::workspace::Directory`): each change one write.
struct Groups(Arc<roll_workspace::Workspace>);

fn dir_err(e: roll_workspace::Error) -> roll_accounts::scim_groups::DirError {
    use roll_accounts::scim_groups::DirError;
    match e {
        roll_workspace::Error::NotFound(m) => DirError::NotFound(m),
        roll_workspace::Error::Conflict(m) => DirError::Conflict(m),
        roll_workspace::Error::TooMany(m) | roll_workspace::Error::Limit(m) => DirError::TooMany(m),
        e => DirError::Internal(e.to_string()),
    }
}

impl roll_accounts::scim_groups::Directory for Groups {
    fn list(&self, conn: &str, filter: Option<(&str, &str)>, offset: i64, count: i64) -> Result<(i64, Vec<roll_accounts::scim_groups::DirGroup>), roll_accounts::scim_groups::DirError> {
        let (n, page) = self.0.scim_groups(conn, filter, offset, count).map_err(dir_err)?;
        Ok((n, page.into_iter().map(|g| roll_accounts::scim_groups::DirGroup { id: g.id, name: g.name, external_id: g.external_id, members: vec![], created_at: g.created_at }).collect()))
    }
    fn get(&self, conn: &str, id: &str, members: bool) -> Result<roll_accounts::scim_groups::DirGroup, roll_accounts::scim_groups::DirError> {
        let (g, m) = self.0.scim_group(conn, id, members).map_err(dir_err)?;
        Ok(roll_accounts::scim_groups::DirGroup { id: g.id, name: g.name, external_id: g.external_id, members: m.into_iter().map(|m| (m.user, m.handle)).collect(), created_at: g.created_at })
    }
    fn create(&self, conn: &str, name: &str, external_id: Option<&str>, members: &[String]) -> Result<(roll_accounts::scim_groups::DirGroup, roll_accounts::scim_groups::Counts), roll_accounts::scim_groups::DirError> {
        let (g, c) = self.0.scim_create(conn, name, external_id, members).map_err(dir_err)?;
        Ok((roll_accounts::scim_groups::DirGroup { id: g.id, name: g.name, external_id: g.external_id, members: vec![], created_at: g.created_at }, roll_accounts::scim_groups::Counts { added: c.added, removed: c.removed, skipped: c.skipped }))
    }
    fn patch(&self, conn: &str, id: &str, ops: &[roll_accounts::scim_groups::GroupOp]) -> Result<roll_accounts::scim_groups::Counts, roll_accounts::scim_groups::DirError> {
        use roll_accounts::scim_groups::GroupOp as A;
        use roll_workspace::GroupOp as W;
        let ops: Vec<W> = ops.iter().map(|o| match o { A::Add(l) => W::AddMembers(l.clone()), A::Remove(l) => W::RemoveMembers(l.clone()), A::Replace(l) => W::ReplaceMembers(l.clone()), A::Rename(n) => W::Rename(n.clone()), A::ExternalId(x) => W::ExternalId(x.clone()) }).collect();
        let c = self.0.scim_patch(conn, id, &ops).map_err(dir_err)?;
        Ok(roll_accounts::scim_groups::Counts { added: c.added, removed: c.removed, skipped: c.skipped })
    }
    fn delete(&self, conn: &str, id: &str) -> Result<(), roll_accounts::scim_groups::DirError> {
        self.0.scim_delete(conn, id).map_err(dir_err)
    }
    fn forget_connection(&self, conn: &str) -> Result<(), roll_accounts::scim_groups::DirError> {
        self.0.forget_connection_groups(conn).map(|_| ()).map_err(dir_err)
    }
}

/// docs/members-v1-1.md, "SCIM Groups": off until an admin turns a connection's groups on; then the
/// provider's group is one roll group whose members it keeps. Adding someone gives them the
/// group's project role at once and taking them out (Entra's path form, a PUT, a DELETE) takes it;
/// people the connection doesn't reach are skipped, and the bounds hold.
#[tokio::test]
async fn scim_groups_give_and_take_a_projects_role() {
    let env = Env::new().await;
    let idp = fake_idp().await;
    let issuer = idp.base.lock().unwrap().clone();
    let txt = Arc::new(Txt::default());
    env.accounts.set_txt_lookup(txt.clone());
    let root_invite = env.operator(Method::POST, "/v1/accounts/invites", json!({ "email": "root@acme.test", "admin": true })).await.json["token"].as_str().unwrap().to_string();
    let root = env.email_sign_in(&mut Browser::default(), "root@acme.test", Some(&root_invite)).await.json["user"]["id"].as_str().unwrap().to_string();
    let conn = env.operator(Method::POST, "/v1/accounts/sso", json!({ "issuer": issuer, "client_id": "roll-sso", "client_secret": "s3cret", "domains": ["acme.test"] })).await.json;
    let id = conn["id"].as_str().unwrap().to_string();
    txt.0.lock().unwrap().insert("_roll-sso.acme.test".into(), vec![conn["dns"]["value"].as_str().unwrap().to_string()]);
    env.operator(Method::POST, &format!("/v1/accounts/sso/{id}/verify"), json!({})).await;
    let token = env.operator(Method::POST, &format!("/v1/accounts/sso/{id}/scim"), json!({})).await.json["token"].as_str().unwrap().to_string();
    let dir = tempfile::tempdir().unwrap();
    let ws = Arc::new(roll_workspace::Workspace::new(code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap()).unwrap());
    env.accounts.set_directory(Arc::new(Groups(ws.clone())));
    let project = ws.create("", "shop", "", None).unwrap().id;
    let scim = |method: Method, path: String, body: Option<String>| {
        let (env, token) = (&env, token.clone());
        async move {
            let req = Request::builder().method(method).uri(format!("/v1/accounts/scim/v2{path}")).header("authorization", format!("Bearer {token}")).header("content-type", "application/scim+json");
            env.send(req.body(body.map_or(Body::empty(), Body::from)).unwrap()).await
        }
    };
    let patch = |ops: Value| Some(json!({ "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": ops }).to_string());
    let mut people = vec![];
    for who in ["carol", "dave"] {
        people.push(scim(Method::POST, "/Users".into(), Some(json!({ "userName": format!("{who}@acme.test") }).to_string())).await.json["id"].as_str().unwrap().to_string());
    }
    let (carol, dave) = (people[0].clone(), people[1].clone());

    // Off by default: a SCIM 404, and the config describes users only.
    let eng = json!({ "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"], "displayName": "eng", "externalId": "okta-eng", "members": [{ "value": carol }, { "value": root }, { "value": "usr_nobody" }] }).to_string();
    assert_eq!(scim(Method::POST, "/Groups".into(), Some(eng.clone())).await.status, StatusCode::NOT_FOUND);
    assert_eq!(scim(Method::GET, "/ResourceTypes".into(), None).await.json["totalResults"], 1);
    assert_eq!(env.operator(Method::PATCH, &format!("/v1/accounts/sso/{id}"), json!({ "groups": true })).await.json["groups"], true);
    assert_eq!(scim(Method::GET, "/ResourceTypes".into(), None).await.json["totalResults"], 2);

    // Made with carol; the admin and the unknown id are skipped, never an error.
    let made = scim(Method::POST, "/Groups".into(), Some(eng.clone())).await;
    assert_eq!(made.status, StatusCode::CREATED, "{}", made.json);
    let group = made.json["id"].as_str().unwrap().to_string();
    assert_eq!(made.json["members"].as_array().unwrap().iter().map(|m| m["value"].as_str().unwrap()).collect::<Vec<_>>(), vec![carol.as_str()]);
    assert_eq!(scim(Method::POST, "/Groups".into(), Some(eng)).await.json["scimType"], "uniqueness");
    let found = scim(Method::GET, "/Groups?filter=displayName%20eq%20%22eng%22&excludedAttributes=members".into(), None).await;
    assert_eq!((found.json["totalResults"].as_i64(), found.json["Resources"][0]["id"].as_str()), (Some(1), Some(group.as_str())), "{}", found.json);
    ws.set_group_role(&roll_workspace::Actor::operator(), &project, &group, "developer", None).unwrap();
    assert!(ws.reaches(&project, &carol).unwrap() && !ws.reaches(&project, &dave).unwrap());

    // Okta adds dave; Entra removes carol by its path form: each takes effect at once.
    let r = scim(Method::PATCH, format!("/Groups/{group}"), patch(json!([{ "op": "add", "path": "members", "value": [{ "value": dave }] }]))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(ws.reaches(&project, &dave).unwrap());
    assert_eq!(scim(Method::PATCH, format!("/Groups/{group}"), patch(json!([{ "op": "Remove", "path": format!("members[value eq \"{carol}\"]") }]))).await.status, StatusCode::OK);
    assert!(!ws.reaches(&project, &carol).unwrap(), "removed from the group, she's out of the project");
    // A PUT replaces the members: carol back, dave out.
    let put = json!({ "schemas": ["urn:ietf:params:scim:schemas:core:2.0:Group"], "displayName": "eng", "members": [{ "value": carol }] }).to_string();
    assert_eq!(scim(Method::PUT, format!("/Groups/{group}"), Some(put)).await.status, StatusCode::OK);
    assert!(ws.reaches(&project, &carol).unwrap() && !ws.reaches(&project, &dave).unwrap());

    // Bounds: a body past 1 MiB isn't read; too many members or operations change nothing.
    let huge = format!("{{\"pad\": \"{}\"}}", "x".repeat(roll_accounts::scim_groups::BODY_MAX));
    assert_eq!(scim(Method::PATCH, format!("/Groups/{group}"), Some(huge)).await.status, StatusCode::PAYLOAD_TOO_LARGE);
    let many: Vec<Value> = (0..=roll_accounts::scim_groups::MEMBERS_PER_REQUEST).map(|i| json!({ "value": format!("usr_{i}") })).collect();
    assert_eq!(scim(Method::PATCH, format!("/Groups/{group}"), patch(json!([{ "op": "add", "path": "members", "value": many }]))).await.json["scimType"], "tooMany");
    let ops: Vec<Value> = (0..=roll_accounts::scim_groups::OPS_MAX).map(|_| json!({ "op": "remove", "path": "members", "value": [{ "value": carol }] })).collect();
    assert_eq!(scim(Method::PATCH, format!("/Groups/{group}"), patch(json!(ops))).await.json["scimType"], "tooMany");
    assert!(ws.reaches(&project, &carol).unwrap());

    // Deleted by the provider: its people lose its role, and it's gone.
    assert_eq!(scim(Method::DELETE, format!("/Groups/{group}"), None).await.status, StatusCode::NO_CONTENT);
    assert!(!ws.reaches(&project, &carol).unwrap());
    assert_eq!(scim(Method::GET, format!("/Groups/{group}"), None).await.status, StatusCode::NOT_FOUND);
}

/// Account events, as rolld's audit log gets them (docs/accounts.md, "Audit").
#[derive(Default)]
struct Events(Mutex<Vec<roll_accounts::audit::AccountEvent>>);

impl roll_accounts::audit::Audit for Events {
    fn record(&self, e: roll_accounts::audit::AccountEvent) {
        self.0.lock().unwrap().push(e);
    }
    fn client_ip(&self, _req: &Request<Body>) -> Option<std::net::IpAddr> {
        Some("203.0.113.7".parse().unwrap())
    }
    // As rolld answers it: the events about the person, newest first.
    fn activity(&self, user: &str, _before: Option<i64>, limit: usize) -> Vec<roll_accounts::audit::Activity> {
        let all = self.0.lock().unwrap();
        all.iter()
            .enumerate()
            .rev()
            .filter(|(_, e)| e.target.as_deref() == Some(user))
            .take(limit)
            .map(|(i, e)| roll_accounts::audit::Activity { at: i as i64, action: e.action.clone(), actor: e.actor.clone(), ok: e.ok, status: e.status, ip: e.ip.map(|i| i.to_string()), user_agent: e.user_agent.clone(), detail: e.detail.clone() })
            .collect()
    }
}

/// Who signed in and how, who failed to, and every change to accounts go to the audit log, with
/// the client's address and the actor; never a code or a token.
#[tokio::test]
async fn account_events_are_audited() {
    let env = Env::new().await;
    let events = Arc::new(Events::default());
    env.accounts.set_audit(events.clone());
    let invite = env.invite("ada@example.com").await;
    let mut adab = Browser::default();
    let ada = env.email_sign_in(&mut adab, "ada@example.com", Some(&invite)).await.json["user"]["id"].as_str().unwrap().to_string();
    // A wrong code for her account: a refused sign-in, by nobody known, in her log.
    let mut b = Browser::default();
    let started = env.call(&mut b, Method::POST, "/v1/accounts/email/start", Some(json!({ "email": "ada@example.com" }))).await;
    // The code is made after the answer (its timing says nothing): wait for its mail.
    for _ in 0..200 {
        if read_codes(&env.outbox, "ada@example.com").len() >= 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let wrong = env.call(&mut b, Method::POST, "/v1/accounts/email/verify", Some(json!({ "challenge": started.json["challenge"], "code": "000000" }))).await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
    // Her own security log: her account made, her sign-in, the refused code, and her own change,
    // newest first.
    assert_eq!(env.call(&mut adab, Method::PATCH, "/v1/accounts/me", Some(json!({ "name": "Ada L." }))).await.status, StatusCode::OK);
    let log = env.call(&mut adab, Method::GET, "/v1/accounts/me/activity", None).await;
    let rows = log.json["events"].as_array().unwrap();
    let actions: Vec<(&str, bool)> = rows.iter().map(|e| (e["action"].as_str().unwrap(), e["ok"].as_bool().unwrap())).collect();
    assert_eq!(actions, [("PATCH /v1/accounts/me", true), ("sign_in", false), ("sign_in", true), ("user.create", true)], "{}", log.json);
    assert_eq!((rows[1]["actor"].as_str(), rows[1]["detail"]["refused"].as_str()), (Some("anonymous"), Some("bad_code")));
    assert_eq!((rows[2]["detail"]["method"].as_str(), rows[2]["ip"].as_str()), (Some("email"), Some("203.0.113.7")));
    assert_eq!(env.operator(Method::PATCH, &format!("/v1/accounts/users/{ada}"), json!({ "disabled": true })).await.status, StatusCode::OK);

    let got = events.0.lock().unwrap().clone();
    let find = |f: &dyn Fn(&roll_accounts::audit::AccountEvent) -> bool| got.iter().find(|e| f(e)).cloned().unwrap_or_else(|| panic!("{got:#?}"));
    // The invite: who, from where, the address it's for and the invite it made.
    let made = find(&|e| e.action == "POST /v1/accounts/invites");
    assert_eq!((made.actor.as_str(), made.ok, made.ip.map(|i| i.to_string())), ("operator", true, Some("203.0.113.7".into())));
    assert_eq!(made.detail["set"]["email"], "ada@example.com");
    assert!(made.target.as_deref().is_some_and(|t| t.starts_with("inv_")), "{made:?}");
    // Its use made an account, then signed her in.
    let created = find(&|e| e.action == "user.create");
    assert_eq!((created.target.as_deref(), created.detail["via"].as_str()), (Some(ada.as_str()), Some("invite")));
    let signed = find(&|e| e.action == "sign_in" && e.ok);
    assert_eq!((signed.actor.as_str(), signed.detail["method"].as_str()), (ada.as_str(), Some("email")));
    let refused = find(&|e| e.action == "sign_in" && !e.ok);
    assert_eq!((refused.actor.as_str(), refused.target.as_deref(), refused.detail["refused"].as_str(), refused.status), ("anonymous", Some(ada.as_str()), Some("bad_code"), 400));
    // The disable says what it set.
    let off = find(&|e| e.action == "PATCH /v1/accounts/users/{id}");
    assert_eq!((off.actor.as_str(), off.target.as_deref(), off.detail["set"]["disabled"].as_bool()), ("operator", Some(ada.as_str()), Some(true)));
    let all = serde_json::to_string(&got).unwrap();
    assert!(!all.contains(&invite) && !all.contains(&newest_code(&env.outbox, "ada@example.com")), "never a token or a code");
}

/// A passkey on a device, in software: a P-256 key, attestation `none`, its own sign count.
struct Authenticator {
    key: ring::signature::EcdsaKeyPair,
    credential: Vec<u8>,
    count: u32,
    /// Flags besides user presence: `UV` (0x04), `BS` (0x10, synced).
    flags: u8,
    /// Its model (AAGUID).
    aaguid: [u8; 16],
}

impl Authenticator {
    fn new(flags: u8) -> Authenticator {
        use ring::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair};
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng).unwrap();
        Authenticator { key, credential: (0..32).map(|i| i as u8 ^ 0x5a).collect(), count: 0, flags, aaguid: [0; 16] }
    }
    fn b64(b: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
    }
    /// Its public key as COSE: {1: 2, 3: -7, -1: 1, -2: x, -3: y}.
    fn cose(&self) -> Vec<u8> {
        use ring::signature::KeyPair;
        let p = self.key.public_key().as_ref();
        let mut c = vec![0xa5, 0x01, 0x02, 0x03, 0x26, 0x20, 0x01, 0x21, 0x58, 0x20];
        c.extend_from_slice(&p[1..33]);
        c.extend_from_slice(&[0x22, 0x58, 0x20]);
        c.extend_from_slice(&p[33..65]);
        c
    }
    fn auth_data(&self, rp: &str, new: bool) -> Vec<u8> {
        let mut d = Sha256::digest(rp.as_bytes()).to_vec();
        d.push(0x01 | self.flags | if new { 0x40 } else { 0 });
        d.extend_from_slice(&self.count.to_be_bytes());
        if new {
            d.extend_from_slice(&self.aaguid);
            d.extend_from_slice(&(self.credential.len() as u16).to_be_bytes());
            d.extend_from_slice(&self.credential);
            d.extend_from_slice(&self.cose());
        }
        d
    }
    fn client(kind: &str, options: &Value, origin: &str) -> Vec<u8> {
        json!({ "type": kind, "challenge": options["publicKey"]["challenge"], "origin": origin, "crossOrigin": false }).to_string().into_bytes()
    }
    /// `navigator.credentials.create`, as the browser answers it.
    fn create(&mut self, options: &Value) -> Value {
        let rp = options["publicKey"]["rp"]["id"].as_str().unwrap().to_string();
        let data = self.auth_data(&rp, true);
        let mut att = vec![0xa3, 0x63, b'f', b'm', b't', 0x64, b'n', b'o', b'n', b'e', 0x67];
        att.extend_from_slice(b"attStmt");
        att.extend_from_slice(&[0xa0, 0x68]);
        att.extend_from_slice(b"authData");
        att.extend_from_slice(&[0x58, data.len() as u8]);
        att.extend_from_slice(&data);
        let id = Self::b64(&self.credential);
        json!({ "id": id, "rawId": id, "type": "public-key", "response": { "clientDataJSON": Self::b64(&Self::client("webauthn.create", options, WEB)), "attestationObject": Self::b64(&att) } })
    }
    /// `navigator.credentials.create` with a `packed` attestation signed by `signer`, whose
    /// certificate is `cert` (a hardware key proving its model).
    fn create_packed(&mut self, options: &Value, signer: &ring::signature::EcdsaKeyPair, cert: &[u8]) -> Value {
        let rp = options["publicKey"]["rp"]["id"].as_str().unwrap().to_string();
        let data = self.auth_data(&rp, true);
        let client = Self::client("webauthn.create", options, WEB);
        let mut msg = data.clone();
        msg.extend_from_slice(&Sha256::digest(&client));
        let sig = signer.sign(&ring::rand::SystemRandom::new(), &msg).unwrap();
        let bytes = |b: &[u8]| {
            let mut v = if b.len() < 24 { vec![0x40 | b.len() as u8] } else if b.len() < 256 { vec![0x58, b.len() as u8] } else { let mut h = vec![0x59]; h.extend_from_slice(&(b.len() as u16).to_be_bytes()); h };
            v.extend_from_slice(b);
            v
        };
        let text = |t: &str| {
            let mut v = vec![0x60 | t.len() as u8];
            v.extend_from_slice(t.as_bytes());
            v
        };
        let mut att = vec![0xa3];
        att.extend(text("fmt"));
        att.extend(text("packed"));
        att.extend(text("attStmt"));
        att.push(0xa3);
        att.extend(text("alg"));
        att.push(0x26);
        att.extend(text("sig"));
        att.extend(bytes(sig.as_ref()));
        att.extend(text("x5c"));
        att.push(0x81);
        att.extend(bytes(cert));
        att.extend(text("authData"));
        att.extend(bytes(&data));
        let id = Self::b64(&self.credential);
        json!({ "id": id, "rawId": id, "type": "public-key", "response": { "clientDataJSON": Self::b64(&client), "attestationObject": Self::b64(&att) } })
    }

    /// `navigator.credentials.get` at `origin`: the count goes up, the data is signed.
    fn get(&mut self, options: &Value, origin: &str) -> Value {
        self.count += 1;
        let data = self.auth_data(options["publicKey"]["rpId"].as_str().unwrap(), false);
        let client = Self::client("webauthn.get", options, origin);
        let mut msg = data.clone();
        msg.extend_from_slice(&Sha256::digest(&client));
        let sig = self.key.sign(&ring::rand::SystemRandom::new(), &msg).unwrap();
        let id = Self::b64(&self.credential);
        json!({ "id": id, "rawId": id, "type": "public-key", "response": { "clientDataJSON": Self::b64(&client), "authenticatorData": Self::b64(&data), "signature": Self::b64(sig.as_ref()) } })
    }
}

/// Passkeys (docs/accounts.md, "Passkeys"): made on the account page, they sign a person in on
/// their own when the authenticator verified them, or give the second factor a sign-in owes. A
/// challenge is used once and only for the sign-in it was made for; another site's, an unverified
/// or a copied (counter going back) passkey is refused.
#[tokio::test]
async fn passkeys_sign_people_in_on_their_own_or_as_a_second_factor() {
    let env = Env::new().await;
    let events = Arc::new(Events::default());
    env.accounts.set_audit(events.clone());
    let invite = env.invite("ada@example.com").await;
    let mut b = Browser::default();
    env.email_sign_in(&mut b, "ada@example.com", Some(&invite)).await;
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts", None).await.json["providers"]["passkey"], true);

    // Made on the account page: for roll's dashboard, discoverable, no attestation.
    let opts = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys/options", None).await.json;
    assert_eq!((opts["publicKey"]["rp"]["id"].as_str(), opts["publicKey"]["attestation"].as_str(), opts["publicKey"]["authenticatorSelection"]["residentKey"].as_str()), (Some("roll.test"), Some("none"), Some("required")), "{opts}");
    let mut phone = Authenticator::new(0x04 | 0x10);
    let made = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys", Some(json!({ "id": opts["id"], "name": "Ada's phone", "credential": phone.create(&opts) }))).await;
    assert_eq!((made.status, made.json["synced"].as_bool()), (StatusCode::CREATED, Some(true)), "{}", made.json);
    let again = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys", Some(json!({ "id": opts["id"], "name": "again", "credential": phone.create(&opts) }))).await;
    assert_eq!((again.status, again.json["code"].as_str()), (StatusCode::BAD_REQUEST, Some("passkey")), "a challenge is used once");
    let pk = made.json["id"].as_str().unwrap().to_string();

    // On its own: any passkey the browser holds for roll, verified by the authenticator.
    let mut c = Browser::default();
    let o = env.call(&mut c, Method::POST, "/v1/accounts/signin/passkey/options", None).await.json;
    assert_eq!((o["publicKey"]["userVerification"].as_str(), o["publicKey"]["allowCredentials"].as_array().map(Vec::len)), (Some("required"), Some(0)), "{o}");
    let a = env.call(&mut c, Method::POST, "/v1/accounts/signin/passkey", Some(json!({ "id": o["id"], "credential": phone.get(&o, WEB) }))).await;
    assert_eq!((a.status, a.json["status"].as_str(), a.json["user"]["email"].as_str()), (StatusCode::OK, Some("signed_in"), Some("ada@example.com")), "{}", a.json);
    assert_eq!(env.call(&mut c, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::OK);
    // Her sessions say how each signed in: this one with the passkey, the first by email code.
    let sessions = env.call(&mut c, Method::GET, "/v1/accounts/me/sessions", None).await.json;
    let how: Vec<(Value, Value, Value)> = sessions.as_array().unwrap().iter().map(|s| (s["current"].clone(), s["method"].clone(), s["passkey"].clone())).collect();
    assert_eq!(how, [(json!(true), json!("passkey"), json!(true)), (json!(false), json!("email"), json!(false))], "{sessions}");
    // Refused: another site's origin, an authenticator that didn't verify the person, a copied key.
    let refused = |browser: &mut Browser, credential: Value, id: Value| {
        let env = &env;
        let mut b = std::mem::take(browser);
        async move { (env.call(&mut b, Method::POST, "/v1/accounts/signin/passkey", Some(json!({ "id": id, "credential": credential }))).await, b) }
    };
    let mut d = Browser::default();
    let o = env.call(&mut d, Method::POST, "/v1/accounts/signin/passkey/options", None).await.json;
    let (a, mut d) = refused(&mut d, phone.get(&o, "https://evil.test"), o["id"].clone()).await;
    assert_eq!((a.status, a.json["code"].as_str()), (StatusCode::BAD_REQUEST, Some("passkey")), "another site: {}", a.json);
    let o = env.call(&mut d, Method::POST, "/v1/accounts/signin/passkey/options", None).await.json;
    phone.flags = 0x10;
    let (a, mut d) = refused(&mut d, phone.get(&o, WEB), o["id"].clone()).await;
    assert!(a.json["error"].as_str().is_some_and(|e| e.contains("didn't verify you")), "no user verification: {}", a.json);
    phone.flags = 0x04 | 0x10;
    let o = env.call(&mut d, Method::POST, "/v1/accounts/signin/passkey/options", None).await.json;
    phone.count = 0;
    let (a, _) = refused(&mut d, phone.get(&o, WEB), o["id"].clone()).await;
    assert!(a.json["error"].as_str().is_some_and(|e| e.contains("copied")), "a counter going back: {}", a.json);
    phone.count = 100;

    // As a second factor: with TOTP on, an email sign-in owes one, and the passkey gives it.
    let t = env.call(&mut b, Method::POST, "/v1/accounts/me/totp", None).await.json;
    let seed = roll_auth::totp::base32_decode(t["secret"].as_str().unwrap()).unwrap();
    let step = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 / roll_auth::totp::STEP_S;
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/me/totp/confirm", Some(json!({ "code": format!("{:06}", roll_auth::totp::totp(&seed, step)) }))).await.status, StatusCode::OK);
    let mut e = Browser::default();
    let a = env.email_sign_in(&mut e, "ada@example.com", None).await;
    assert_eq!(a.json["methods"], json!(["totp", "recovery", "passkey"]), "{}", a.json);
    let o = env.call(&mut e, Method::POST, "/v1/accounts/signin/passkey/options", None).await.json;
    assert_eq!((o["publicKey"]["userVerification"].as_str(), o["publicKey"]["allowCredentials"][0]["id"].as_str()), (Some("preferred"), Some(Authenticator::b64(&phone.credential).as_str())), "{o}");
    // That challenge is this owing sign-in's: it never signs anyone in on its own elsewhere.
    let mut f = Browser::default();
    let (a, _) = refused(&mut f, phone.get(&o, WEB), o["id"].clone()).await;
    assert!(a.json["error"].as_str().is_some_and(|e| e.contains("another sign-in")), "{}", a.json);
    let o = env.call(&mut e, Method::POST, "/v1/accounts/signin/passkey/options", None).await.json;
    phone.flags = 0x10; // a second factor needs presence, not verification
    let a = env.call(&mut e, Method::POST, "/v1/accounts/signin/passkey", Some(json!({ "id": o["id"], "credential": phone.get(&o, WEB) }))).await;
    assert_eq!(a.json["status"], "signed_in", "{}", a.json);
    phone.flags = 0x04 | 0x10;
    // On its own it's both factors, TOTP on or not.
    let mut g = Browser::default();
    let o = env.call(&mut g, Method::POST, "/v1/accounts/signin/passkey/options", None).await.json;
    assert_eq!(env.call(&mut g, Method::POST, "/v1/accounts/signin/passkey", Some(json!({ "id": o["id"], "credential": phone.get(&o, WEB) }))).await.json["status"], "signed_in");

    // Renamed, listed, removed: a removed passkey signs nobody in.
    assert_eq!(env.call(&mut b, Method::PATCH, &format!("/v1/accounts/me/passkeys/{pk}"), Some(json!({ "name": "Phone" }))).await.json["name"], "Phone");
    let listed = env.call(&mut b, Method::GET, "/v1/accounts/me/passkeys", None).await.json;
    assert_eq!((listed["passkeys"][0]["name"].as_str(), listed["passkeys"][0]["last_used_at"].is_i64()), (Some("Phone"), true), "{listed}");
    assert!(listed.get("public_key").is_none() && !listed.to_string().contains("credential"), "never the key: {listed}");
    assert_eq!(env.call(&mut b, Method::DELETE, &format!("/v1/accounts/me/passkeys/{pk}"), None).await.status, StatusCode::NO_CONTENT);
    let mut h = Browser::default();
    let o = env.call(&mut h, Method::POST, "/v1/accounts/signin/passkey/options", None).await.json;
    let (a, _) = refused(&mut h, phone.get(&o, WEB), o["id"].clone()).await;
    assert!(a.json["error"].as_str().is_some_and(|e| e.contains("doesn't know")), "{}", a.json);

    // The audit log: passkey sign-ins and refusals by method, the passkey's add and removal; never
    // the ceremonies' options, never a credential.
    let got = events.0.lock().unwrap().clone();
    let ada = listed_owner(&got);
    assert!(got.iter().any(|e| e.action == "sign_in" && e.ok && e.detail["method"] == "passkey" && e.target.as_deref() == Some(ada.as_str())), "{got:#?}");
    assert!(got.iter().any(|e| e.action == "sign_in" && !e.ok && e.detail["method"] == "passkey" && e.actor == "anonymous" && e.target.as_deref() == Some(ada.as_str())), "a refusal is the owner's");
    assert!(got.iter().any(|e| e.action == "POST /v1/accounts/me/passkeys") && got.iter().any(|e| e.action == "DELETE /v1/accounts/me/passkeys/{id}"));
    assert!(!got.iter().any(|e| e.action.contains("options")), "no row for asking for a challenge");
    assert!(!serde_json::to_string(&got).unwrap().contains("clientDataJSON"));
}

/// Ada's id, from the account event that made her.
fn listed_owner(got: &[roll_accounts::audit::AccountEvent]) -> String {
    got.iter().find(|e| e.action == "POST /v1/accounts/me/passkeys").and_then(|e| e.target.clone()).unwrap()
}

/// Sudo mode (docs/accounts.md): changing how an account is reached, or who's an admin, needs a
/// proof from the last two hours (here 1.5 s): a sign-in, or a confirmation by email code (or the
/// authenticator app, or a passkey). Another person's code proves nothing; the operator's token
/// has no session and is exempt.
#[tokio::test]
async fn sudo_mode_asks_for_a_recent_proof() {
    let env = Env::new().await;
    let events = Arc::new(Events::default());
    env.accounts.set_audit(events.clone());
    env.accounts.set_sudo_window(1500);
    let invite = env.operator(Method::POST, "/v1/accounts/invites", json!({ "email": "root@example.com", "admin": true })).await.json["token"].as_str().unwrap().to_string();
    let mut b = Browser::default();
    env.email_sign_in(&mut b, "root@example.com", Some(&invite)).await;
    let bob_invite = env.invite("bob@example.com").await;
    let mut bb = Browser::default();
    let bob = env.email_sign_in(&mut bb, "bob@example.com", Some(&bob_invite)).await.json["user"]["id"].as_str().unwrap().to_string();

    // Just signed in: fresh.
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys/options", None).await.status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let stale = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys/options", None).await;
    assert_eq!((stale.status, stale.json["code"].as_str()), (StatusCode::FORBIDDEN, Some("reauth_required")), "{}", stale.json);
    assert_eq!(env.call(&mut b, Method::PATCH, &format!("/v1/accounts/users/{bob}"), Some(json!({ "admin": true }))).await.json["code"], "reauth_required");
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/me/totp", None).await.json["code"], "reauth_required");
    // Not everything: a name change, inviting someone who isn't an admin.
    assert_eq!(env.call(&mut b, Method::PATCH, "/v1/accounts/me", Some(json!({ "name": "Root" }))).await.status, StatusCode::OK);
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/invites", Some(json!({ "email": "carol@example.com" }))).await.status, StatusCode::CREATED);
    // The operator's token has no session to prove.
    assert_eq!(env.operator(Method::PATCH, &format!("/v1/accounts/users/{bob}"), json!({ "admin": false })).await.status, StatusCode::OK);

    let info = env.call(&mut b, Method::GET, "/v1/accounts/me/reauth", None).await.json;
    assert_eq!(info["methods"], json!(["email"]), "{info}");
    // Someone else's email code proves nothing.
    let theirs = env.call(&mut bb, Method::POST, "/v1/accounts/email/start", Some(json!({ "email": "bob@example.com" }))).await.json["challenge"].as_str().unwrap().to_string();
    let wait_codes = |to: &'static str, n: usize| {
        let outbox = env.outbox.clone();
        async move {
            for _ in 0..200 {
                if read_codes(&outbox, to).len() >= n {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    };
    wait_codes("bob@example.com", 2).await;
    let wrong = env.call(&mut b, Method::POST, "/v1/accounts/me/reauth", Some(json!({ "challenge": theirs, "code": newest_code(&env.outbox, "bob@example.com") }))).await;
    assert_eq!((wrong.status, wrong.json["code"].as_str()), (StatusCode::BAD_REQUEST, Some("bad_code")), "{}", wrong.json);
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/me/reauth", Some(json!({ "code": "123456" }))).await.json["code"], "bad_code", "no app to check");
    // Their own code: fresh again.
    let ch = env.call(&mut b, Method::POST, "/v1/accounts/me/reauth/email", None).await;
    assert_eq!(ch.status, StatusCode::ACCEPTED);
    wait_codes("root@example.com", 2).await;
    let ok = env.call(&mut b, Method::POST, "/v1/accounts/me/reauth", Some(json!({ "challenge": ch.json["challenge"], "code": newest_code(&env.outbox, "root@example.com") }))).await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.json);
    assert!(ok.json["fresh_until"].as_i64().unwrap() > 0);
    assert_eq!(env.call(&mut b, Method::PATCH, &format!("/v1/accounts/users/{bob}"), Some(json!({ "admin": true }))).await.status, StatusCode::OK);

    let got = events.0.lock().unwrap().clone();
    assert_eq!(got.iter().filter(|e| e.action == "reauth").map(|e| (e.ok, e.detail["method"].as_str().unwrap().to_string())).collect::<Vec<_>>(), [(false, "email".into()), (false, "totp".into()), (true, "email".into())]);
}

/// The mails "a new sign-in to your roll account" sent to `to`.
fn new_sign_in_mails(outbox: &std::path::Path, to: &str) -> Vec<String> {
    let mut out: Vec<(String, String)> = std::fs::read_dir(outbox)
        .map(|d| d.filter_map(|e| e.ok()).map(|e| (e.file_name().to_string_lossy().into_owned(), serde_json::from_slice::<Value>(&std::fs::read(e.path()).unwrap()).unwrap())).filter(|(_, m)| m["to"] == to && m["subject"] == "A new sign-in to your roll account").map(|(n, m)| (n, m["text"].as_str().unwrap().to_string())).collect())
        .unwrap_or_default();
    out.sort();
    out.into_iter().map(|(_, t)| t).collect()
}

/// A whole sign-in from a browser and network a person hasn't signed in from before mails them
/// (docs/accounts.md, "New sign-ins"); their first sign-in, and the same place again, don't.
#[tokio::test]
async fn a_sign_in_from_somewhere_new_mails_the_person() {
    let env = Env::new().await;
    env.accounts.set_audit(Arc::new(Events::default()));
    let mac = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_5) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36";
    let windows = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:133.0) Gecko/20100101 Firefox/133.0";
    let invite = env.invite("ada@example.com").await;
    let mut b = Browser { user_agent: Some(mac.into()), ..Default::default() };
    env.email_sign_in(&mut b, "ada@example.com", Some(&invite)).await;
    let mut again = Browser { user_agent: Some(mac.into()), ..Default::default() };
    assert_eq!(env.email_sign_in(&mut again, "ada@example.com", None).await.json["status"], "signed_in");
    let mut elsewhere = Browser { user_agent: Some(windows.into()), ..Default::default() };
    assert_eq!(env.email_sign_in(&mut elsewhere, "ada@example.com", None).await.json["status"], "signed_in");
    let mut twice = Browser { user_agent: Some(windows.into()), ..Default::default() };
    env.email_sign_in(&mut twice, "ada@example.com", None).await;
    // The mail goes out off the request: wait for it, then a moment for any wrong second one.
    for _ in 0..100 {
        if !new_sign_in_mails(&env.outbox, "ada@example.com").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mails = new_sign_in_mails(&env.outbox, "ada@example.com");
    assert_eq!(mails.len(), 1, "one mail, for the one new place: {mails:?}");
    assert!(mails[0].contains("Firefox on Windows, from 203.0.113.7") && mails[0].contains(&format!("{WEB}/dashboard/account#activity")), "{}", mails[0]);
}

/// Second-factor recovery (docs/accounts.md, "Recovery"): someone who lost their authenticator app
/// and recovery codes asks after their email code, admins are mailed, and an admin's approval
/// takes their TOTP away: their next sign-in is whole and they set it up again. Asked twice it's
/// one request; decided once; each step in their security log.
#[tokio::test]
async fn an_admin_resets_a_lost_second_factor() {
    let env = Env::new().await;
    let events = Arc::new(Events::default());
    env.accounts.set_audit(events.clone());
    let root_invite = env.operator(Method::POST, "/v1/accounts/invites", json!({ "email": "root@example.com", "admin": true })).await.json["token"].as_str().unwrap().to_string();
    let mut root = Browser::default();
    let root_id = env.email_sign_in(&mut root, "root@example.com", Some(&root_invite)).await.json["user"]["id"].as_str().unwrap().to_string();
    let bob_invite = env.invite("bob@example.com").await;
    let mut b = Browser::default();
    let bob = env.email_sign_in(&mut b, "bob@example.com", Some(&bob_invite)).await.json["user"]["id"].as_str().unwrap().to_string();
    let t = env.call(&mut b, Method::POST, "/v1/accounts/me/totp", None).await.json;
    let seed = roll_auth::totp::base32_decode(t["secret"].as_str().unwrap()).unwrap();
    let step = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 / roll_auth::totp::STEP_S;
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/me/totp/confirm", Some(json!({ "code": format!("{:06}", roll_auth::totp::totp(&seed, step)) }))).await.status, StatusCode::OK);

    // Bob lost his phone and his codes: his email code gets him as far as the second factor.
    let mut lost = Browser::default();
    assert_eq!(env.email_sign_in(&mut lost, "bob@example.com", None).await.json["status"], "mfa_required");
    let ask = json!({ "reason": "Lost my phone and my codes. Call me on the desk phone." });
    assert_eq!(env.call(&mut Browser::default(), Method::POST, "/v1/accounts/signin/reset-request", Some(ask.clone())).await.status, StatusCode::UNAUTHORIZED, "a first factor first");
    let r = env.call(&mut lost, Method::POST, "/v1/accounts/signin/reset-request", Some(ask.clone())).await;
    assert_eq!((r.status, r.json["state"].as_str()), (StatusCode::ACCEPTED, Some("pending")), "{}", r.json);
    let id = r.json["id"].as_str().unwrap().to_string();
    assert_eq!(env.call(&mut lost, Method::POST, "/v1/accounts/signin/reset-request", Some(ask)).await.json["id"], id.as_str(), "one pending request");
    // Admins are told; Bob can't see or decide requests.
    let mut told = false;
    for _ in 0..100 {
        told = std::fs::read_dir(&env.outbox).unwrap().filter_map(|e| e.ok()).any(|e| {
            let m: Value = serde_json::from_slice(&std::fs::read(e.path()).unwrap()).unwrap();
            m["to"] == "root@example.com" && m["subject"] == "bob asks for a second-factor reset" && m["text"].as_str().unwrap().contains("desk phone")
        });
        if told {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(told, "the admin was mailed the request and its reason");
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/reset-requests", None).await.status, StatusCode::FORBIDDEN);
    assert_eq!(env.call(&mut b, Method::POST, &format!("/v1/accounts/reset-requests/{id}/approve"), None).await.status, StatusCode::FORBIDDEN);
    let listed = env.call(&mut root, Method::GET, "/v1/accounts/reset-requests", None).await.json;
    assert_eq!((listed[0]["id"].as_str(), listed[0]["handle"].as_str(), listed[0]["state"].as_str()), (Some(id.as_str()), Some("bob"), Some("pending")), "{listed}");

    // Approved: his TOTP is gone, the sign-in waiting for it ends, the next one is whole.
    let ok = env.call(&mut root, Method::POST, &format!("/v1/accounts/reset-requests/{id}/approve"), None).await;
    assert_eq!((ok.status, ok.json["state"].as_str(), ok.json["decided_by"].as_str()), (StatusCode::OK, Some("approved"), Some(root_id.as_str())), "{}", ok.json);
    assert_eq!(env.call(&mut root, Method::POST, &format!("/v1/accounts/reset-requests/{id}/deny"), None).await.status, StatusCode::CONFLICT, "decided once");
    assert_eq!(env.call(&mut lost, Method::GET, "/v1/accounts/session", None).await.json["status"], "anonymous");
    let mut again = Browser::default();
    let whole = env.email_sign_in(&mut again, "bob@example.com", None).await.json;
    assert_eq!((whole["status"].as_str(), whole["user"]["totp"].as_bool()), (Some("signed_in"), Some(false)), "{whole}");

    let got = events.0.lock().unwrap().clone();
    let steps: Vec<(String, String, Option<String>)> = got.iter().filter(|e| e.action.starts_with("recovery.")).map(|e| (e.action.clone(), e.actor.clone(), e.target.clone())).collect();
    assert_eq!(steps, [("recovery.request".to_string(), bob.clone(), Some(bob.clone())), ("recovery.approve".to_string(), root_id.clone(), Some(bob.clone()))]);
}

/// A person's own data (docs/accounts.md, "Your data"): one JSON download of their account,
/// sessions, passkeys, security log and more, behind sudo mode, with no secret or hash in it.
#[tokio::test]
async fn a_person_exports_their_own_data() {
    let env = Env::new().await;
    env.accounts.set_audit(Arc::new(Events::default()));
    let invite = env.invite("ada@example.com").await;
    let mut b = Browser::default();
    env.email_sign_in(&mut b, "ada@example.com", Some(&invite)).await;
    let t = env.call(&mut b, Method::POST, "/v1/accounts/me/totp", None).await.json;
    let seed = t["secret"].as_str().unwrap().to_string();
    let a = env.call(&mut b, Method::POST, "/v1/accounts/me/export", None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.headers.get(header::CONTENT_DISPOSITION).and_then(|v| v.to_str().ok()), Some("attachment; filename=\"roll-account-ada.json\""));
    let d = &a.json;
    assert_eq!(d["account"]["email"], "ada@example.com");
    assert_eq!(d["sessions"].as_array().map(Vec::len), Some(1), "{d}");
    assert!(d["security_log"].as_array().unwrap().iter().any(|e| e["action"] == "sign_in"), "{d}");
    assert!(d["passkeys"].is_array() && d["tokens"].is_array() && d["sign_in_places"].is_array());
    let all = d.to_string();
    assert!(!all.contains(&seed) && !all.contains("token_hash") && !all.contains("code_hash") && !all.contains(&invite), "no secret in an export");
    // Sudo mode: an old session can't take it.
    env.accounts.set_sudo_window(1);
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/me/export", None).await.json["code"], "reauth_required");
}

/// A maker's root and an attestation certificate it signed for model `aaguid` (OU "Authenticator
/// Attestation", FIDO's AAGUID extension), with the certificate's signing key: the root's PEM, the
/// certificate's DER, the key.
fn attestation_chain(maker: &str, aaguid: [u8; 16]) -> (String, Vec<u8>, ring::signature::EcdsaKeyPair) {
    use rcgen::{BasicConstraints, CertificateParams, CustomExtension, DnType, IsCa, Issuer, KeyPair};
    let root_key = KeyPair::generate().unwrap();
    let mut root = CertificateParams::new(Vec::<String>::new()).unwrap();
    root.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    root.distinguished_name.push(DnType::CommonName, format!("{maker} Attestation Root"));
    let root_cert = root.self_signed(&root_key).unwrap();
    let issuer = Issuer::from_params(&root, &root_key);
    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf = CertificateParams::new(Vec::<String>::new()).unwrap();
    leaf.is_ca = IsCa::ExplicitNoCa;
    leaf.distinguished_name.push(DnType::OrganizationName, maker);
    leaf.distinguished_name.push(DnType::OrganizationalUnitName, "Authenticator Attestation");
    leaf.distinguished_name.push(DnType::CommonName, format!("{maker} Key"));
    let mut ext = vec![0x04, 16];
    ext.extend_from_slice(&aaguid);
    leaf.custom_extensions.push(CustomExtension::from_oid_content(&[1, 3, 6, 1, 4, 1, 45724, 1, 1, 4], ext));
    let leaf_cert = leaf.signed_by(&leaf_key, &issuer).unwrap();
    let signer = ring::signature::EcdsaKeyPair::from_pkcs8(&ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING, &leaf_key.serialize_der(), &ring::rand::SystemRandom::new()).unwrap();
    (root_cert.pem(), leaf_cert.der().to_vec(), signer)
}

/// Hardware keys only (docs/accounts.md, "Passkeys"): with the policy on, registration asks for
/// attestation and keeps only a listed model whose `packed` statement chains to its maker's root;
/// no attestation, another model, or another maker's chain is refused, and an older passkey that
/// proved nothing stops signing in. Off, nothing changes.
#[tokio::test]
async fn hardware_keys_only_takes_verified_listed_models() {
    let env = Env::new().await;
    let root_invite = env.operator(Method::POST, "/v1/accounts/invites", json!({ "email": "root@example.com", "admin": true })).await.json["token"].as_str().unwrap().to_string();
    let mut b = Browser::default();
    env.email_sign_in(&mut b, "root@example.com", Some(&root_invite)).await;
    // Off: any passkey, no attestation asked.
    let o = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys/options", None).await.json;
    assert_eq!(o["publicKey"]["attestation"], "none");
    let mut phone = Authenticator::new(0x04);
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys", Some(json!({ "id": o["id"], "name": "phone", "credential": phone.create(&o) }))).await.status, StatusCode::CREATED);

    // On: one model, with its maker's root.
    let yk: [u8; 16] = *b"yubikey-5-model!";
    let (root_pem, cert, signer) = attestation_chain("Yubico", yk);
    let model_id = roll_accounts::attestation::uuid(&yk);
    let bad = env.call(&mut b, Method::PUT, "/v1/accounts/passkey-policy", Some(json!({ "require": true, "models": [] }))).await;
    assert_eq!((bad.status, bad.json["code"].as_str()), (StatusCode::BAD_REQUEST, Some("policy")), "at least one model");
    let set = env.call(&mut b, Method::PUT, "/v1/accounts/passkey-policy", Some(json!({ "require": true, "models": [{ "aaguid": model_id, "name": "YubiKey 5", "roots": [root_pem] }] }))).await;
    assert_eq!(set.status, StatusCode::OK, "{}", set.json);

    // A listed model, attested by its maker: kept, with its model's name.
    let o = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys/options", None).await.json;
    assert_eq!(o["publicKey"]["attestation"], "direct");
    let mut key = Authenticator::new(0x04);
    key.credential = vec![7; 32];
    key.aaguid = yk;
    let made = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys", Some(json!({ "id": o["id"], "name": "my key", "credential": key.create_packed(&o, &signer, &cert) }))).await;
    assert_eq!((made.status, made.json["model"].as_str()), (StatusCode::CREATED, Some("YubiKey 5")), "{}", made.json);
    // Refused: no attestation, an unlisted model, a chain to another maker's root.
    let mut none = Authenticator::new(0x04);
    none.credential = vec![8; 32];
    none.aaguid = yk;
    let o = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys/options", None).await.json;
    let a = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys", Some(json!({ "id": o["id"], "name": "x", "credential": none.create(&o) }))).await;
    assert!(a.json["error"].as_str().is_some_and(|e| e.contains("doesn't prove its model")), "no attestation: {}", a.json);
    let mut other = Authenticator::new(0x04);
    other.credential = vec![9; 32];
    other.aaguid = *b"another-model-xx";
    let (_, other_cert, other_signer) = attestation_chain("Yubico", other.aaguid);
    let o = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys/options", None).await.json;
    let a = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys", Some(json!({ "id": o["id"], "name": "x", "credential": other.create_packed(&o, &other_signer, &other_cert) }))).await;
    assert!(a.json["error"].as_str().is_some_and(|e| e.contains("isn't one")), "unlisted model: {}", a.json);
    let mut fake = Authenticator::new(0x04);
    fake.credential = vec![10; 32];
    fake.aaguid = yk;
    let (_, fake_cert, fake_signer) = attestation_chain("Counterfeit", yk);
    let o = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys/options", None).await.json;
    let a = env.call(&mut b, Method::POST, "/v1/accounts/me/passkeys", Some(json!({ "id": o["id"], "name": "x", "credential": fake.create_packed(&o, &fake_signer, &fake_cert) }))).await;
    assert!(a.json["error"].as_str().is_some_and(|e| e.contains("isn't from this model's maker")), "another maker's chain: {}", a.json);

    // Signing in: the attested key works; the phone's passkey, which proved nothing, doesn't.
    let mut c = Browser::default();
    let o = env.call(&mut c, Method::POST, "/v1/accounts/signin/passkey/options", None).await.json;
    assert_eq!(env.call(&mut c, Method::POST, "/v1/accounts/signin/passkey", Some(json!({ "id": o["id"], "credential": key.get(&o, WEB) }))).await.json["status"], "signed_in");
    let mut d = Browser::default();
    let o = env.call(&mut d, Method::POST, "/v1/accounts/signin/passkey/options", None).await.json;
    let a = env.call(&mut d, Method::POST, "/v1/accounts/signin/passkey", Some(json!({ "id": o["id"], "credential": phone.get(&o, WEB) }))).await;
    assert!(a.json["error"].as_str().is_some_and(|e| e.contains("isn't on one")), "{}", a.json);
}

/// An SSO connection's session length (docs/accounts.md, "SSO"): what it signs in lasts at most
/// that long, a shorter one applies at once to its running sessions, and a sign-in another way
/// keeps roll's 30 days.
#[tokio::test]
async fn an_sso_connection_caps_its_sessions() {
    let env = Env::new().await;
    let idp = fake_idp().await;
    let issuer = idp.base.lock().unwrap().clone();
    let txt = Arc::new(Txt::default());
    env.accounts.set_txt_lookup(txt.clone());
    let mut by_code = Browser::default();
    env.email_sign_in(&mut by_code, "ada@acme.test", Some(&env.invite("ada@acme.test").await)).await;
    let conn = env.operator(Method::POST, "/v1/accounts/sso", json!({ "name": "Acme Okta", "issuer": issuer, "client_id": "roll-sso", "client_secret": "s3cret", "domains": ["acme.test"] })).await.json;
    let id = conn["id"].as_str().unwrap().to_string();
    txt.0.lock().unwrap().insert("_roll-sso.acme.test".into(), vec![conn["dns"]["value"].as_str().unwrap().to_string()]);
    env.operator(Method::POST, &format!("/v1/accounts/sso/{id}/verify"), json!({})).await;
    assert_eq!(env.operator(Method::PATCH, &format!("/v1/accounts/sso/{id}"), json!({ "session_hours": 721 })).await.status, StatusCode::BAD_REQUEST);
    let set = env.operator(Method::PATCH, &format!("/v1/accounts/sso/{id}"), json!({ "session_hours": 12 })).await;
    assert_eq!(set.json["session_hours"], 12, "{}", set.json);

    let hours = |b: &mut Browser| {
        let env = &env;
        let mut b = b.clone();
        async move {
            let s = env.call(&mut b, Method::GET, "/v1/accounts/session", None).await.json;
            (s["session"]["expires_at"].as_i64().unwrap() - s["session"]["created_at"].as_i64().unwrap()) / 3_600_000
        }
    };
    let mut by_sso = Browser::default();
    env.sso(&idp, &mut by_sso, "ada@acme.test", json!({ "sub": "00u-ada", "email": "ada@acme.test" })).await;
    assert_eq!(hours(&mut by_sso).await, 12, "an SSO session lasts the connection's length");
    assert_eq!(hours(&mut by_code).await, 30 * 24, "a sign-in another way keeps roll's 30 days");
    // Shorter: its running sessions shorten now; the email-code one doesn't.
    assert_eq!(env.operator(Method::PATCH, &format!("/v1/accounts/sso/{id}"), json!({ "session_hours": 1 })).await.status, StatusCode::OK);
    assert_eq!(hours(&mut by_sso).await, 1);
    assert_eq!(hours(&mut by_code).await, 30 * 24);
}

/// OIDC Back-Channel Logout (docs/accounts.md, "SSO"): the provider posts a signed logout token
/// and the sessions it names end: by the provider's session (`sid`), else the person (`sub`).
/// A replayed, unsigned, nonce-bearing or event-less token is refused; other sign-ins stay.
#[tokio::test]
async fn an_sso_providers_logout_ends_its_sessions() {
    let env = Env::new().await;
    let idp = fake_idp().await;
    let issuer = idp.base.lock().unwrap().clone();
    let txt = Arc::new(Txt::default());
    env.accounts.set_txt_lookup(txt.clone());
    let mut by_code = Browser::default();
    env.email_sign_in(&mut by_code, "ada@acme.test", Some(&env.invite("ada@acme.test").await)).await;
    let conn = env.operator(Method::POST, "/v1/accounts/sso", json!({ "name": "Acme Okta", "issuer": issuer, "client_id": "roll-sso", "client_secret": "s3cret", "domains": ["acme.test"] })).await.json;
    let id = conn["id"].as_str().unwrap().to_string();
    assert!(conn["backchannel_logout_uri"].as_str().unwrap().ends_with(&format!("/v1/accounts/sso/{id}/backchannel-logout")), "{conn}");
    txt.0.lock().unwrap().insert("_roll-sso.acme.test".into(), vec![conn["dns"]["value"].as_str().unwrap().to_string()]);
    env.operator(Method::POST, &format!("/v1/accounts/sso/{id}/verify"), json!({})).await;
    let mut laptop = Browser::default();
    env.sso(&idp, &mut laptop, "ada@acme.test", json!({ "sub": "00u-ada", "email": "ada@acme.test", "sid": "okta-sid-1" })).await;
    let mut phone = Browser::default();
    env.sso(&idp, &mut phone, "ada@acme.test", json!({ "sub": "00u-ada", "email": "ada@acme.test", "sid": "okta-sid-2" })).await;

    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let sign = |claims: Value, key: &[u8]| {
        use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair};
        let b64 = |b: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b);
        let input = format!("{}.{}", b64(json!({ "alg": "ES256", "kid": "k1", "typ": "logout+jwt" }).to_string().as_bytes()), b64(claims.to_string().as_bytes()));
        let k = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, key, &ring::rand::SystemRandom::new()).unwrap();
        format!("{input}.{}", b64(k.sign(&ring::rand::SystemRandom::new(), input.as_bytes()).unwrap().as_ref()))
    };
    let token = |extra: Value| {
        let mut c = json!({ "iss": issuer, "aud": "roll-sso", "iat": now, "exp": now + 120, "jti": format!("j-{}", rand_hex()), "events": { "http://schemas.openid.net/event/backchannel-logout": {} } });
        for (k, v) in extra.as_object().unwrap() {
            if v.is_null() {
                c.as_object_mut().unwrap().remove(k);
            } else {
                c[k] = v.clone();
            }
        }
        c
    };
    let post = |t: String| {
        let (env, id) = (&env, id.clone());
        async move {
            let req = Request::builder().method(Method::POST).uri(format!("/v1/accounts/sso/{id}/backchannel-logout")).header("content-type", "application/x-www-form-urlencoded");
            env.send(req.body(Body::from(format!("logout_token={t}"))).unwrap()).await
        }
    };
    let status = |b: &Browser| {
        let env = &env;
        let mut b = b.clone();
        async move { env.call(&mut b, Method::GET, "/v1/accounts/session", None).await.json["status"].as_str().unwrap().to_string() }
    };
    // Its session, by sid: the laptop's SSO session ends; the phone's and the email code's stay.
    let first = sign(token(json!({ "sid": "okta-sid-1" })), &idp.key);
    let a = post(first.clone()).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.json);
    assert_eq!(a.headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()), Some("no-store"));
    assert_eq!((status(&laptop).await, status(&phone).await, status(&by_code).await), ("anonymous".into(), "signed_in".into(), "signed_in".into()));
    // Refused: a replay, a nonce, no logout event, another key's signature.
    assert_eq!(post(first).await.status, StatusCode::BAD_REQUEST, "used once");
    assert_eq!(post(sign(token(json!({ "sub": "00u-ada", "nonce": "n" })), &idp.key)).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(post(sign(token(json!({ "sub": "00u-ada", "events": null })), &idp.key)).await.status, StatusCode::BAD_REQUEST);
    let other = ring::signature::EcdsaKeyPair::generate_pkcs8(&ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING, &ring::rand::SystemRandom::new()).unwrap();
    assert_eq!(post(sign(token(json!({ "sub": "00u-ada" })), other.as_ref())).await.status, StatusCode::BAD_REQUEST);
    assert_eq!(status(&phone).await, "signed_in");
    // The person, by sub: every session this connection signed in for them ends; the code's stays.
    assert_eq!(post(sign(token(json!({ "sub": "00u-ada" })), &idp.key)).await.status, StatusCode::OK);
    assert_eq!((status(&phone).await, status(&by_code).await), ("anonymous".into(), "signed_in".into()));
}

fn rand_hex() -> String {
    let mut b = [0u8; 8];
    getrandom::fill(&mut b).unwrap();
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A second factor for everyone (docs/accounts.md): while it's on, a person without one reaches
/// only their account, where they set one up; then everything again.
#[tokio::test]
async fn a_company_can_require_a_second_factor() {
    let env = Env::new().await;
    let invite = env.invite("ada@example.com").await;
    let mut b = Browser::default();
    env.email_sign_in(&mut b, "ada@example.com", Some(&invite)).await;
    assert_eq!(env.call(&mut b, Method::GET, "/v1/workspace/projects", None).await.status, StatusCode::OK);
    let set = env.operator(Method::PUT, "/v1/accounts/policy", json!({ "require_second_factor": true })).await;
    assert_eq!((set.status, set.json["require_second_factor"].as_bool()), (StatusCode::OK, Some(true)), "{}", set.json);

    let refused = env.call(&mut b, Method::GET, "/v1/workspace/projects", None).await;
    assert_eq!((refused.status, refused.json["code"].as_str()), (StatusCode::FORBIDDEN, Some("second_factor_required")), "{}", refused.json);
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/session", None).await.json["second_factor_required"], true);
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::OK, "her account, to set one up");
    // She sets up an authenticator app: everything again.
    let t = env.call(&mut b, Method::POST, "/v1/accounts/me/totp", None).await.json;
    let seed = roll_auth::totp::base32_decode(t["secret"].as_str().unwrap()).unwrap();
    let step = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64 / roll_auth::totp::STEP_S;
    assert_eq!(env.call(&mut b, Method::POST, "/v1/accounts/me/totp/confirm", Some(json!({ "code": format!("{:06}", roll_auth::totp::totp(&seed, step)) }))).await.status, StatusCode::OK);
    assert_eq!(env.call(&mut b, Method::GET, "/v1/workspace/projects", None).await.status, StatusCode::OK);
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/session", None).await.json["second_factor_required"], false);
    // Off: nobody's held to it.
    let invite = env.invite("bob@example.com").await;
    let mut bob = Browser::default();
    env.email_sign_in(&mut bob, "bob@example.com", Some(&invite)).await;
    // The people list says who has a second factor.
    let people = env.operator(Method::GET, "/v1/accounts/users", json!({})).await.json;
    let factors = |email: &str| people.as_array().unwrap().iter().find(|u| u["email"] == email).map(|u| (u["totp"].as_bool().unwrap(), u["passkeys"].as_u64().unwrap())).unwrap();
    assert_eq!((factors("ada@example.com"), factors("bob@example.com")), ((true, 0), (false, 0)), "{people}");
    // And when each last used roll (signing in counts).
    assert!(people.as_array().unwrap().iter().all(|u| u["last_active_at"].as_i64().is_some_and(|t| t > 0)), "{people}");
    // A token's use counts too (rolld's gate notes it): Bob, idle for months, is active again.
    let bob_id = people.as_array().unwrap().iter().find(|u| u["email"] == "bob@example.com").unwrap()["id"].as_str().unwrap().to_string();
    let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(env._dir.path().join("accounts.db")), "accounts").unwrap();
    db.write(|c| c.execute("UPDATE users SET last_active_at = 1 WHERE id = ?", [bob_id.as_str()])).unwrap();
    env.accounts.note_active(&bob_id).unwrap();
    let again = env.operator(Method::GET, "/v1/accounts/users", json!({})).await.json;
    assert!(again.as_array().unwrap().iter().any(|u| u["id"] == bob_id.as_str() && u["last_active_at"].as_i64().is_some_and(|t| t > 1)), "{again}");
    assert_eq!(env.call(&mut bob, Method::GET, "/v1/workspace/projects", None).await.status, StatusCode::FORBIDDEN);
    env.operator(Method::PUT, "/v1/accounts/policy", json!({ "require_second_factor": false })).await;
    assert_eq!(env.call(&mut bob, Method::GET, "/v1/workspace/projects", None).await.status, StatusCode::OK);
}

/// Allowed networks (docs/accounts.md): a person's session is used only from the company's
/// networks; saving a list that leaves the saving admin's own address out is refused; the
/// operator's token is never held to it.
#[tokio::test]
async fn sessions_are_used_only_from_allowed_networks() {
    let env = Env::new().await;
    let root_invite = env.operator(Method::POST, "/v1/accounts/invites", json!({ "email": "root@example.com", "admin": true })).await.json["token"].as_str().unwrap().to_string();
    let mut office = Browser { ip: Some("203.0.113.20".into()), ..Default::default() };
    env.email_sign_in(&mut office, "root@example.com", Some(&root_invite)).await;
    let mut cafe = Browser { ip: Some("192.0.2.50".into()), ..Default::default() };
    env.email_sign_in(&mut cafe, "root@example.com", None).await;
    assert_eq!(env.call(&mut cafe, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::OK, "off: anywhere");

    let put = |b: &mut Browser, body: Value| {
        let env = &env;
        let mut b = b.clone();
        async move { env.call(&mut b, Method::PUT, "/v1/accounts/network-policy", Some(body)).await }
    };
    assert_eq!(put(&mut office, json!({ "on": true, "networks": [] })).await.json["code"], "networks");
    assert_eq!(put(&mut office, json!({ "on": true, "networks": ["198.51.100.0/24"] })).await.json["code"], "lockout", "the admin's own address left out");
    let on = put(&mut office, json!({ "on": true, "networks": ["203.0.113.7/24"] })).await;
    assert_eq!((on.status, on.json["networks"].clone()), (StatusCode::OK, json!(["203.0.113.0/24"])), "{}", on.json);

    assert_eq!(env.call(&mut office, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::OK);
    let refused = env.call(&mut cafe, Method::GET, "/v1/accounts/me", None).await;
    assert_eq!((refused.status, refused.json["code"].as_str()), (StatusCode::FORBIDDEN, Some("network")), "{}", refused.json);
    assert_eq!(env.operator(Method::GET, "/v1/accounts/network-policy", json!({})).await.status, StatusCode::OK, "the operator's token isn't held to it");
    // Off again, from the office: the café works again.
    assert_eq!(put(&mut office, json!({ "on": false, "networks": ["203.0.113.0/24"] })).await.status, StatusCode::OK);
    assert_eq!(env.call(&mut cafe, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::OK);
}

/// An idle timeout and a longest session (docs/accounts.md): a session unused longer than the
/// company's minutes ends, and so does one older than its hours, however much it's used;
/// out-of-range values are refused.
#[tokio::test]
async fn a_company_sets_an_idle_timeout() {
    let env = Env::new().await;
    let invite = env.invite("ada@example.com").await;
    let mut b = Browser::default();
    env.email_sign_in(&mut b, "ada@example.com", Some(&invite)).await;
    assert_eq!(env.operator(Method::PUT, "/v1/accounts/policy", json!({ "idle_minutes": 5 })).await.json["code"], "policy");
    let set = env.operator(Method::PUT, "/v1/accounts/policy", json!({ "idle_minutes": 30 })).await;
    assert_eq!(set.json["idle_minutes"], 30, "{}", set.json);
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::OK, "used just now");
    // Unused for 31 minutes (as the database's clock sees it): gone.
    let db = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(env._dir.path().join("accounts.db")), "accounts").unwrap();
    db.write(|c| c.execute("UPDATE sessions SET seen_at = seen_at - 31 * 60000", ())).unwrap();
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::UNAUTHORIZED);

    // The longest session (Okta's maximum session lifetime): 8 hours after its sign-in, however
    // much it's used.
    assert_eq!(env.operator(Method::PUT, "/v1/accounts/policy", json!({ "max_session_hours": 721 })).await.json["code"], "policy");
    assert_eq!(env.operator(Method::PUT, "/v1/accounts/policy", json!({ "idle_minutes": 30, "max_session_hours": 8 })).await.json["max_session_hours"], 8);
    let mut c = Browser::default();
    env.email_sign_in(&mut c, "ada@example.com", None).await;
    db.write(|c| c.execute("UPDATE sessions SET created_at = created_at - 7 * 3600000", ())).unwrap();
    assert_eq!(env.call(&mut c, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::OK, "7 hours in, in use");
    // The session list says when it ends: 8 hours after its sign-in, not roll's 30 days.
    let listed = env.call(&mut c, Method::GET, "/v1/accounts/me/sessions", None).await.json;
    let s = &listed.as_array().unwrap()[0];
    assert_eq!(s["expires_at"].as_i64().unwrap() - s["created_at"].as_i64().unwrap(), 8 * 3600000, "{listed}");
    db.write(|c| c.execute("UPDATE sessions SET created_at = created_at - 3600000", ())).unwrap();
    assert_eq!(env.call(&mut c, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::UNAUTHORIZED, "8 hours in");
}

/// Phishing-resistant sign-in (docs/accounts.md): on for admins, an admin's session that hasn't
/// used a passkey reaches only their account, and confirming it's them with one (or signing in
/// with one) opens the rest; people who aren't admins are untouched until it's everyone's. An
/// admin can't turn it on from a session that hasn't used one (it would lock them out).
#[tokio::test]
async fn a_company_requires_phishing_resistant_sign_in() {
    let env = Env::new().await;
    let root_invite = env.operator(Method::POST, "/v1/accounts/invites", json!({ "email": "root@example.com", "admin": true })).await.json["token"].as_str().unwrap().to_string();
    let mut root = Browser::default();
    env.email_sign_in(&mut root, "root@example.com", Some(&root_invite)).await;
    let mut ada = Browser::default();
    env.email_sign_in(&mut ada, "ada@example.com", Some(&env.invite("ada@example.com").await)).await;
    let admins = json!({ "phishing_resistant": "admins" });

    assert_eq!(env.call(&mut root, Method::PUT, "/v1/accounts/policy", Some(admins.clone())).await.status, StatusCode::CONFLICT, "not from an email-code session");
    // The admin adds a passkey and confirms it's them with it.
    let opts = env.call(&mut root, Method::POST, "/v1/accounts/me/passkeys/options", None).await.json;
    let mut key = Authenticator::new(0x04 | 0x10);
    assert_eq!(env.call(&mut root, Method::POST, "/v1/accounts/me/passkeys", Some(json!({ "id": opts["id"], "name": "Root's key", "credential": key.create(&opts) }))).await.status, StatusCode::CREATED);
    let o = env.call(&mut root, Method::POST, "/v1/accounts/me/reauth/passkey/options", None).await.json;
    let confirmed = env.call(&mut root, Method::POST, "/v1/accounts/me/reauth", Some(json!({ "id": o["id"], "credential": key.get(&o, WEB) }))).await;
    assert_eq!(confirmed.status, StatusCode::OK, "{}", confirmed.json);
    let set = env.call(&mut root, Method::PUT, "/v1/accounts/policy", Some(admins)).await;
    assert_eq!((set.status, set.json["phishing_resistant"].as_str()), (StatusCode::OK, Some("admins")), "{}", set.json);
    for b in [&mut root, &mut ada] {
        assert_eq!(env.call(b, Method::GET, "/v1/workspace/projects", None).await.status, StatusCode::OK);
    }

    // The admin by email code elsewhere: only their account, until a passkey is used.
    let mut laptop = Browser::default();
    env.email_sign_in(&mut laptop, "root@example.com", None).await;
    let refused = env.call(&mut laptop, Method::GET, "/v1/workspace/projects", None).await;
    assert_eq!((refused.status, refused.json["code"].as_str()), (StatusCode::FORBIDDEN, Some("passkey_required")), "{}", refused.json);
    assert_eq!(env.call(&mut laptop, Method::GET, "/v1/accounts/session", None).await.json["passkey_required"], true);
    let mut signed = Browser::default();
    let o = env.call(&mut signed, Method::POST, "/v1/accounts/signin/passkey/options", None).await.json;
    assert_eq!(env.call(&mut signed, Method::POST, "/v1/accounts/signin/passkey", Some(json!({ "id": o["id"], "credential": key.get(&o, WEB) }))).await.status, StatusCode::OK);
    assert_eq!(env.call(&mut signed, Method::GET, "/v1/workspace/projects", None).await.status, StatusCode::OK, "a passkey sign-in");

    // Everyone's (the operator has no session to check): Ada too.
    assert_eq!(env.operator(Method::PUT, "/v1/accounts/policy", json!({ "phishing_resistant": "everyone" })).await.status, StatusCode::OK);
    assert_eq!(env.call(&mut ada, Method::GET, "/v1/workspace/projects", None).await.json["code"], "passkey_required");
}

/// Mails to `to` whose subject has `about`, oldest first: (subject, text).
async fn mails_about(outbox: &std::path::Path, to: &str, about: &str, n: usize) -> Vec<(String, String)> {
    for _ in 0..400 {
        let mut out: Vec<(String, String, String)> = std::fs::read_dir(outbox)
            .map(|d| d.filter_map(|e| e.ok()).filter(|e| !e.file_name().to_string_lossy().starts_with('.')).map(|e| (e.file_name().to_string_lossy().into_owned(), serde_json::from_slice::<Value>(&std::fs::read(e.path()).unwrap()).unwrap())).filter(|(_, m)| m["to"] == to && m["subject"].as_str().unwrap().contains(about)).map(|(f, m)| (f, m["subject"].as_str().unwrap().to_string(), m["text"].as_str().unwrap().to_string())).collect())
            .unwrap_or_default();
        if out.len() >= n {
            out.sort();
            return out.into_iter().map(|(_, s, t)| (s, t)).collect();
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("no mail to {to} about {about:?}");
}

async fn change(env: &Env, b: &mut Browser, email: &str) -> Answer {
    env.call(b, Method::POST, "/v1/accounts/me/email", Some(json!({ "email": email }))).await
}

async fn confirm(env: &Env, b: &mut Browser, challenge: &str, code: &str) -> Answer {
    env.call(b, Method::POST, "/v1/accounts/me/email/confirm", Some(json!({ "challenge": challenge, "code": code }))).await
}

/// A person moves their account to another address (docs/accounts.md, "Your address"): from a
/// fresh session, with the code sent there; the old address is told, and signs in no more.
#[tokio::test]
async fn a_person_moves_their_account_to_another_address() {
    let env = Env::new().await;
    let mut ada = Browser::default();
    let id = env.email_sign_in(&mut ada, "ada@example.com", Some(&env.invite("ada@example.com").await)).await.json["user"]["id"].as_str().unwrap().to_string();
    let mut bob = Browser::default();
    env.email_sign_in(&mut bob, "bob@example.com", Some(&env.invite("bob@example.com").await)).await;

    // A stolen cookie alone can't move it.
    env.accounts.set_sudo_window(0);
    assert_eq!(change(&env, &mut ada, "ada@new.test").await.json["code"], "reauth_required");
    env.accounts.set_sudo_window(2 * 60 * 60 * 1000);

    let started = change(&env, &mut ada, "Ada@New.test").await;
    assert_eq!(started.status, StatusCode::ACCEPTED, "{}", started.json);
    let challenge = started.json["challenge"].as_str().unwrap().to_string();
    let code = mails_about(&env.outbox, "ada@new.test", "is your code to move", 1).await[0].0[..6].to_string();
    // Only Ada's session confirms Ada's change, and only with its code.
    assert_eq!(confirm(&env, &mut bob, &challenge, &code).await.json["code"], "bad_code");
    assert_eq!(confirm(&env, &mut ada, &challenge, "000000").await.json["code"], "bad_code");
    let moved = confirm(&env, &mut ada, &challenge, &code).await;
    assert_eq!((moved.status, moved.json["email"].as_str()), (StatusCode::OK, Some("ada@new.test")), "{}", moved.json);
    assert_eq!(confirm(&env, &mut ada, &challenge, &code).await.json["code"], "bad_code", "single use");
    let told = mails_about(&env.outbox, "ada@example.com", "moved to another address", 1).await;
    assert!(told[0].1.contains("ada@new.test"), "{}", told[0].1);

    // The new address signs Ada in; the old one gets no code.
    let old_codes = count_codes(&env.outbox, "ada@example.com");
    let a = env.call(&mut Browser::default(), Method::POST, "/v1/accounts/email/start", Some(json!({ "email": "ada@example.com" }))).await;
    assert_eq!(a.status, StatusCode::ACCEPTED);
    let again = env.email_sign_in(&mut Browser::default(), "ada@new.test", None).await;
    assert_eq!(again.json["user"]["id"].as_str(), Some(id.as_str()));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(count_codes(&env.outbox, "ada@example.com"), old_codes);

    // Bob's address: the same answer, a note to Bob instead of a code, and nothing to confirm.
    let taken = change(&env, &mut ada, "bob@example.com").await;
    assert_eq!(taken.status, StatusCode::ACCEPTED);
    assert!(mails_about(&env.outbox, "bob@example.com", "Someone tried to move", 1).await[0].1.contains("Nothing changed"));
    let guess = env.call(&mut ada, Method::POST, "/v1/accounts/me/email/confirm", Some(json!({ "challenge": taken.json["challenge"], "code": "123456" }))).await;
    assert_eq!(guess.json["code"], "bad_code");
    assert_eq!(change(&env, &mut ada, "ada@new.test").await.status, StatusCode::BAD_REQUEST, "already hers");
}

async fn delete(env: &Env, b: &mut Browser, confirm: &str) -> Answer {
    env.call(b, Method::DELETE, "/v1/accounts/me", Some(json!({ "confirm": confirm }))).await
}

/// A person deletes their own account (docs/accounts.md, "Deleting your account"): from a fresh
/// session, typing their handle; every way in ends at once, the address can be invited again,
/// and nothing (an admin included) brings it back. The last admin can't leave.
#[tokio::test]
async fn a_person_deletes_their_own_account() {
    let env = Env::new().await;
    let root_invite = env.operator(Method::POST, "/v1/accounts/invites", json!({ "email": "root@example.com", "admin": true })).await.json["token"].as_str().unwrap().to_string();
    let mut root = Browser::default();
    env.email_sign_in(&mut root, "root@example.com", Some(&root_invite)).await;
    let mut ada = Browser::default();
    let signed = env.email_sign_in(&mut ada, "ada@example.com", Some(&env.invite("ada@example.com").await)).await.json;
    let (id, handle) = (signed["user"]["id"].as_str().unwrap().to_string(), signed["user"]["handle"].as_str().unwrap().to_string());

    // Other products hear of a deletion from its outbox, until each has acted (docs/members.md:
    // the workspace removes their memberships). The first try fails here; the next run delivers.
    let heard = Arc::new(std::sync::Mutex::new((0u32, Vec::<String>::new())));
    let h = heard.clone();
    env.accounts.on_deleted(Box::new(move |user| {
        let mut h = h.lock().unwrap();
        h.0 += 1;
        if h.0 == 1 {
            return Err("the workspace is busy".into());
        }
        h.1.push(user.to_string());
        Ok(())
    }));
    env.accounts.set_sudo_window(0);
    assert_eq!(delete(&env, &mut ada, &handle).await.json["code"], "reauth_required", "a stolen cookie alone can't");
    env.accounts.set_sudo_window(2 * 60 * 60 * 1000);
    assert_eq!(delete(&env, &mut ada, "not-me").await.json["code"], "confirm");
    assert_eq!(delete(&env, &mut root, "root").await.status, StatusCode::CONFLICT, "roll's last admin stays");

    let gone = delete(&env, &mut ada, &handle).await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT, "{}", gone.json);
    assert!(gone.headers.get(header::SET_COOKIE).unwrap().to_str().unwrap().contains("Max-Age=0"));
    assert_eq!(env.call(&mut ada, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::UNAUTHORIZED);
    assert!(env.accounts.person(&id).unwrap().is_none(), "its tokens act as nobody");
    assert!(heard.lock().unwrap().1.is_empty(), "the first try failed, and it's kept");
    env.accounts.run_deletions();
    assert_eq!(heard.lock().unwrap().1, [id.as_str()], "delivered once, on the next run");
    env.accounts.run_deletions();
    assert_eq!(heard.lock().unwrap().1.len(), 1, "and never again");
    assert!(mails_about(&env.outbox, "ada@example.com", "was deleted", 1).await[0].1.contains(&handle));
    // Gone from the admin's list, and no admin turns it back on.
    let people = env.call(&mut root, Method::GET, "/v1/accounts/users", None).await.json;
    assert!(!people.to_string().contains(&id), "{people}");
    assert_eq!(env.call(&mut root, Method::PATCH, &format!("/v1/accounts/users/{id}"), Some(json!({ "disabled": false }))).await.status, StatusCode::NOT_FOUND);

    // The address signs nothing in; invited again, it's a new account.
    let codes = count_codes(&env.outbox, "ada@example.com");
    env.call(&mut Browser::default(), Method::POST, "/v1/accounts/email/start", Some(json!({ "email": "ada@example.com" }))).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(count_codes(&env.outbox, "ada@example.com"), codes);
    let again = env.email_sign_in(&mut Browser::default(), "ada@example.com", Some(&env.invite("ada@example.com").await)).await.json;
    assert_ne!(again["user"]["id"].as_str(), Some(id.as_str()), "{again}");
}

/// An admin reads a person's security log and sessions from People (Okta's per-user System Log),
/// and ends one session (a lost laptop) while the others stay; nobody else can.
#[tokio::test]
async fn an_admin_reads_a_persons_log_and_ends_one_session() {
    let env = Env::new().await;
    env.accounts.set_audit(Arc::new(Events::default()));
    let root_invite = env.operator(Method::POST, "/v1/accounts/invites", json!({ "email": "root@example.com", "admin": true })).await.json["token"].as_str().unwrap().to_string();
    let mut root = Browser::default();
    env.email_sign_in(&mut root, "root@example.com", Some(&root_invite)).await;
    let mut laptop = Browser::default();
    let ada = env.email_sign_in(&mut laptop, "ada@example.com", Some(&env.invite("ada@example.com").await)).await.json["user"]["id"].as_str().unwrap().to_string();
    let mut phone = Browser::default();
    env.email_sign_in(&mut phone, "ada@example.com", None).await;
    let mut bob = Browser::default();
    env.email_sign_in(&mut bob, "bob@example.com", Some(&env.invite("bob@example.com").await)).await;

    assert_eq!(env.call(&mut bob, Method::GET, &format!("/v1/accounts/users/{ada}/activity"), None).await.status, StatusCode::FORBIDDEN, "not an admin");
    let log = env.call(&mut root, Method::GET, &format!("/v1/accounts/users/{ada}/activity"), None).await.json;
    assert_eq!(log["events"].as_array().unwrap().iter().filter(|e| e["action"] == "sign_in" && e["ok"] == true).count(), 2, "{log}");
    let sessions = env.call(&mut root, Method::GET, &format!("/v1/accounts/users/{ada}/sessions"), None).await.json;
    let list = sessions.as_array().unwrap();
    assert!(list.len() == 2 && list.iter().all(|s| s["current"] == false), "{sessions}");
    // Her laptop's session (the older one) ends; her phone's stays.
    let oldest = list.iter().min_by_key(|s| s["created_at"].as_i64()).unwrap()["id"].as_str().unwrap().to_string();
    assert_eq!(env.call(&mut root, Method::DELETE, &format!("/v1/accounts/users/{ada}/sessions/{oldest}"), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(env.call(&mut laptop, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(env.call(&mut phone, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::OK);
    assert_eq!(env.call(&mut root, Method::GET, "/v1/accounts/users/usr_nobody/sessions", None).await.status, StatusCode::NOT_FOUND);
}

/// Admin alerts (docs/accounts.md): making someone an admin or changing how people sign in mails
/// every other admin, naming who did it; the admin who did it isn't mailed.
#[tokio::test]
async fn other_admins_are_told_of_security_changes() {
    let env = Env::new().await;
    let admin_invite = |email: &'static str| {
        let env = &env;
        async move { env.operator(Method::POST, "/v1/accounts/invites", json!({ "email": email, "admin": true })).await.json["token"].as_str().unwrap().to_string() }
    };
    let mut root = Browser::default();
    env.email_sign_in(&mut root, "root@example.com", Some(&admin_invite("root@example.com").await)).await;
    env.email_sign_in(&mut Browser::default(), "ruth@example.com", Some(&admin_invite("ruth@example.com").await)).await;
    let ada = env.email_sign_in(&mut Browser::default(), "ada@example.com", Some(&env.invite("ada@example.com").await)).await.json["user"]["id"].as_str().unwrap().to_string();

    assert_eq!(env.call(&mut root, Method::PATCH, &format!("/v1/accounts/users/{ada}"), Some(json!({ "admin": true }))).await.status, StatusCode::OK);
    let told = mails_about(&env.outbox, "ruth@example.com", "was made an admin", 1).await;
    assert!(told[0].0.starts_with("roll security: ") && told[0].1.contains("by root (root@example.com)"), "{told:?}");
    assert_eq!(env.operator(Method::PUT, "/v1/accounts/policy", json!({ "require_second_factor": true })).await.status, StatusCode::OK);
    let policy = mails_about(&env.outbox, "root@example.com", "sign-in policy changed", 1).await;
    assert!(policy[0].1.contains("a second factor for everyone on") && policy[0].1.contains("by the operator's token"), "{policy:?}");
    // Whoever made a change isn't told of it.
    let mine = |to: &str| {
        std::fs::read_dir(&env.outbox).unwrap().filter_map(|e| e.ok()).map(|e| serde_json::from_slice::<Value>(&std::fs::read(e.path()).unwrap()).unwrap()).filter(|m| m["to"] == to && m["subject"].as_str().unwrap().contains("was made an admin")).count()
    };
    assert_eq!(mine("root@example.com"), 0);
}
