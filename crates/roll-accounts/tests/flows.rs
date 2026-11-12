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
}

struct Env {
    router: Router,
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
        .route("/user", get(|| async { Json(json!({ "id": 42, "login": "Ada-L", "name": "Ada Lovelace" })) }))
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
        let router = roll_accounts::router(accounts)
            .route("/v1/projects", get(|| async { Json(json!([])) }).post(|| async { StatusCode::CREATED }))
            .route("/v1/workspace/projects", get(|| async { Json(json!([])) }))
            .layer(axum::middleware::from_fn_with_state(gate, roll_auth::check));
        Env { router, outbox, owners, seen, google_issuer: format!("{fake}/google"), _dir: dir }
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
    // Once linked, the identity signs in without an invite.
    let a = env.oauth(&mut Browser::default(), "github", json!({})).await;
    assert_eq!(location(&a), "https://roll.test/dashboard");

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
    let a = env.call(&mut laptop, Method::DELETE, &format!("/v1/accounts/me/sessions/{phone_session}"), None).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT);
    assert_eq!(env.call(&mut phone, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(env.call(&mut phone, Method::GET, "/v1/accounts/session", None).await.json["status"], "anonymous");

    // Disabling the user ends the rest, on the next request.
    let user = env.call(&mut laptop, Method::GET, "/v1/accounts/me", None).await.json["id"].as_str().unwrap().to_string();
    let a = env.operator(Method::PATCH, &format!("/v1/accounts/users/{user}"), json!({ "disabled": true })).await;
    assert_eq!((a.status, a.json["disabled"].as_bool()), (StatusCode::OK, Some(true)));
    assert_eq!(env.call(&mut laptop, Method::GET, "/v1/workspace/projects", None).await.status, StatusCode::UNAUTHORIZED);
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
