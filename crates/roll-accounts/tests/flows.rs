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

    // Carol signs in; deactivated by the provider (Entra's string), her session ends at once.
    let mut b = Browser::default();
    env.email_sign_in(&mut b, "carol@acme.test", None).await;
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::OK);
    let off = scim(Method::PATCH, format!("/Users/{carol_id}"), Some(json!({ "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": [{ "op": "Replace", "path": "active", "value": "False" }] })), token.clone()).await;
    assert_eq!((off.status, off.json["active"].as_bool()), (StatusCode::OK, Some(false)), "{}", off.json);
    assert_eq!(env.call(&mut b, Method::GET, "/v1/accounts/me", None).await.status, StatusCode::UNAUTHORIZED, "her session ended");
    let on = scim(Method::PATCH, format!("/Users/{carol_id}"), Some(json!({ "schemas": ["urn:ietf:params:scim:api:messages:2.0:PatchOp"], "Operations": [{ "op": "replace", "value": { "active": true } }] })), token.clone()).await;
    assert_eq!(on.json["active"], true);
    assert_eq!(scim(Method::DELETE, format!("/Users/{carol_id}"), None, token.clone()).await.status, StatusCode::NO_CONTENT);
    assert_eq!(scim(Method::GET, format!("/Users/{carol_id}"), None, token.clone()).await.json["active"], false, "DELETE deactivates");

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
