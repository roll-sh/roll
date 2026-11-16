//! `roll token …` and `roll mcp …`, the real binary against rolld's gate and `/v1/access` in
//! process (docs/permissions.md): a token made from a preset works as `ROLL_TOKEN` within its
//! rules, a request it may only ask for waits for `roll mcp approve`, and revoking it ends it.

use serde_json::{Value, json};
use std::process::{Command, Output};
use std::sync::Arc;

const OPERATOR: &str = "operator-token-0123456789abcdef-0123";

async fn serve() -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let ws = code_sql::Db::open_or_test(&code_sql::Target::Sqlite(dir.path().join("workspace.db")), "workspace").unwrap();
    let workspace = Arc::new(roll_workspace::Workspace::new(ws).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
    let config = roll_access::Config { locked: true, approval_wait: std::time::Duration::from_secs(20), api_url: api.clone(), device_interval: std::time::Duration::from_secs(1), ..Default::default() };
    let access = roll_access::Access::open(&code_sql::Target::Sqlite(dir.path().join("access.db")), Default::default(), config, Default::default()).unwrap();
    let gate = roll_auth::Gate::new(Some(roll_auth::AdminToken::new(OPERATOR).unwrap()), Vec::new());
    gate.set_authorizer(Arc::new(roll_access::evaluate::Evaluator(access.clone())));
    // A stand-in for Postgres's routes.
    let postgres = axum::Router::new()
        .route("/v1/projects", axum::routing::get(|| async { axum::Json(json!([{ "id": "prj_a", "name": "shop" }])) }))
        .route("/v1/projects/{id}", axum::routing::delete(|| async { axum::http::StatusCode::NO_CONTENT }));
    let router = postgres
        .merge(roll_workspace::router(workspace))
        .merge(roll_access::api::router(access.clone()))
        .layer(axum::middleware::from_fn_with_state(gate, roll_auth::check))
        // Discovery, registration, token and revocation sit in front of the gate, as in rolld.
        .merge(roll_access::oauth::public_router(access));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (api, dir)
}

fn roll(api: &str, token: &str, args: &[&str]) -> Output {
    let mut c = Command::new(env!("CARGO_BIN_EXE_roll"));
    c.env_clear().env("PATH", std::env::var("PATH").unwrap_or_default()).env("ROLL_API_URL", api).env("ROLL_ADMIN_TOKEN", OPERATOR);
    if !token.is_empty() {
        c.env("ROLL_TOKEN", token);
    }
    c.args(args).output().unwrap()
}

fn json_of(o: &Output) -> Value {
    assert!(o.status.success(), "{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_from_the_cli_is_held_to_its_rules() {
    let (api, _dir) = serve().await;
    let made = json_of(&roll(&api, "", &["--json", "token", "create", "agent", "--product", "postgres", "--allow", "read", "--ask", "delete", "--expires", "7d"]));
    let token = made["token"].as_str().unwrap().to_string();
    let id = made["grant"]["id"].as_str().unwrap().to_string();
    // Listed by what it's for: a rule on postgres, but not naming one database.
    let ids = |args: &[&str]| json_of(&roll(&api, "", args)).as_array().unwrap().iter().map(|g| g["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(ids(&["--json", "token", "list", "--product", "postgres"]), vec![id.clone()]);
    assert!(ids(&["--json", "token", "list", "--product", "postgres", "--resource", "prj_a"]).is_empty());

    // ROLL_TOKEN wins over ROLL_ADMIN_TOKEN: the scoped token is what's used.
    let me = json_of(&roll(&api, &token, &["--json", "token", "whoami"]));
    assert_eq!(me["grant"]["id"], id.as_str());
    assert!(roll(&api, &token, &["db", "list"]).status.success());
    let refused = roll(&api, &token, &["token", "list"]);
    assert!(!refused.status.success() && String::from_utf8_lossy(&refused.stderr).contains("can't"), "{refused:?}");

    // An ask waits for `roll mcp approve`, then runs.
    let api2 = api.clone();
    let token2 = token.clone();
    let delete = std::thread::spawn(move || roll(&api2, &token2, &["db", "delete", "prj_a"]));
    let mut approval = None;
    for _ in 0..100 {
        let pending = json_of(&roll(&api, "", &["--json", "mcp", "approvals"]));
        if let Some(a) = pending.as_array().and_then(|l| l.first()) {
            approval = Some(a["id"].as_str().unwrap().to_string());
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(roll(&api, "", &["mcp", "approve", &approval.expect("an approval")]).status.success());
    assert!(delete.join().unwrap().status.success());

    // Tightened to what it used: read as before (a list names no project), delete only on prj_a,
    // still asking.
    std::thread::sleep(std::time::Duration::from_millis(200));
    // The checkup says so first: it's narrower than its rules.
    let check = json_of(&roll(&api, "", &["--json", "token", "checkup"]));
    let mine = check.as_array().unwrap().iter().find(|c| c["grant"]["id"] == id.as_str()).expect("in the checkup");
    assert!(mine["findings"].as_array().unwrap().iter().any(|f| f["kind"] == "narrower"), "{mine}");
    let tight = json_of(&roll(&api, "", &["--json", "token", "tighten", &id, "--apply"]));
    let rules = &tight["rules"];
    assert_eq!(rules, &json!([
        { "product": "postgres", "resources": "*", "actions": { "read": "allow" } },
        { "product": "postgres", "resources": [{ "id": "prj_a" }], "actions": { "delete": "ask" } },
    ]), "{tight}");

    // Revoked: the next command fails.
    assert!(roll(&api, "", &["token", "revoke", &id]).status.success());
    assert!(!roll(&api, &token, &["db", "list"]).status.success());
    let audit = json_of(&roll(&api, "", &["--json", "mcp", "audit", "--token", &id]));
    // And as CSV, for a spreadsheet: a header, then a row an event.
    let csv = String::from_utf8(roll(&api, "", &["mcp", "audit", "--token", &id, "--format", "csv"]).stdout).unwrap();
    assert!(csv.starts_with("at,token,token_name,") && csv.lines().count() > 1, "{csv}");
    assert!(audit.as_array().unwrap().iter().any(|e| e["decision"] == "ask"), "{audit}");
}

/// `roll token client` makes machine clients: one with a secret trades it at the token endpoint
/// for a token held to its rules, one given an openssl PEM signs with its key, and revoking the
/// first ends its token at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_machine_client_from_the_cli_trades_its_secret_for_a_token_held_to_its_rules() {
    const ED25519_PUB: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA5rj3iN7HTU2KOZ5TXM2Zyw8+3iISgidPeqHONeMvZQM=\n-----END PUBLIC KEY-----\n";
    let (api, dir) = serve().await;
    let made = json_of(&roll(&api, "", &["--json", "token", "client", "create", "nightly", "--product", "postgres", "--allow", "read", "--expires", "7d"]));
    let (client_id, secret) = (made["client_id"].as_str().unwrap(), made["client_secret"].as_str().unwrap());
    let form = [("grant_type", "client_credentials"), ("client_id", client_id), ("client_secret", secret)];
    let body = form.iter().map(|(k, v)| format!("{k}={}", urlencoding::encode(v))).collect::<Vec<_>>().join("&");
    let answer: Value = reqwest::Client::new().post(made["token_endpoint"].as_str().unwrap()).header("content-type", "application/x-www-form-urlencoded").body(body).send().await.unwrap().json().await.unwrap();
    let token = answer["access_token"].as_str().unwrap_or_else(|| panic!("{answer}")).to_string();
    assert!(roll(&api, &token, &["db", "list"]).status.success());
    assert!(!roll(&api, &token, &["db", "delete", "prj_a"]).status.success());

    let key = dir.path().join("ci.pub.pem");
    std::fs::write(&key, ED25519_PUB).unwrap();
    let signed = json_of(&roll(&api, "", &["--json", "token", "client", "create", "deploy", "--preset", "read-only", "--public-key", key.to_str().unwrap()]));
    assert!(signed.get("client_secret").is_none(), "{signed}");
    let list = json_of(&roll(&api, "", &["--json", "token", "client", "list"]));
    let by_key: Vec<(&str, bool)> = list.as_array().unwrap().iter().map(|c| (c["client_id"].as_str().unwrap(), c["signs_with_key"] == true)).collect();
    assert_eq!(by_key, vec![(signed["client_id"].as_str().unwrap(), true), (client_id, false)]);

    assert!(roll(&api, "", &["token", "client", "revoke", client_id]).status.success());
    assert!(!roll(&api, &token, &["db", "list"]).status.success());

    // GitHub Actions as the credential: the step to paste, and the one repository and ref trusted.
    let made = roll(&api, "", &["token", "client", "create", "ci", "--preset", "read-only", "--github", "acme/web", "--ref", "refs/heads/main"]);
    let step = String::from_utf8_lossy(&made.stdout);
    assert!(made.status.success() && step.contains("grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer") && step.contains("::add-mask::"), "{step}");
    let list = json_of(&roll(&api, "", &["--json", "token", "client", "list"]));
    assert_eq!(list[0]["trusts"]["subject"], "repo:acme/web:ref:refs/heads/main");
    let words = String::from_utf8_lossy(&roll(&api, "", &["token", "client", "list"]).stdout).to_string();
    assert!(words.contains("trusts token.actions.githubusercontent.com: repo:acme/web:ref:refs/heads/main  (never signed in)"), "{words}");
    assert!(!roll(&api, "", &["token", "client", "create", "x", "--preset", "read-only", "--github", "acme"]).status.success(), "owner/repo");
    let lab = roll(&api, "", &["token", "client", "create", "lab", "--preset", "read-only", "--gitlab", "acme/platform/api", "--ref", "main"]);
    assert!(lab.status.success() && String::from_utf8_lossy(&lab.stdout).contains("id_tokens:"), "{lab:?}");
    let list = json_of(&roll(&api, "", &["--json", "token", "client", "list"]));
    assert_eq!(list[0]["trusts"]["subject"], "project_path:acme/platform/api:ref_type:branch:ref:main");
    // Any CI system's: an issuer, a subject, claims; a shared issuer still names one tenant.
    let bk = roll(&api, "", &["--json", "token", "client", "create", "bk", "--preset", "read-only", "--issuer", "https://agent.buildkite.com", "--subject", "organization:acme:pipeline:deploy:*", "--claim", "build_branch=main"]);
    assert!(bk.status.success(), "{bk:?}");
    let list = json_of(&roll(&api, "", &["--json", "token", "client", "list"]));
    assert_eq!((list[0]["trusts"]["subject"].as_str(), list[0]["trusts"]["claims"]["build_branch"].as_str()), (Some("organization:acme:pipeline:deploy:*"), Some("main")));
    assert!(!roll(&api, "", &["token", "client", "create", "x", "--preset", "read-only", "--issuer", "https://agent.buildkite.com", "--subject", "*"]).status.success(), "a shared issuer names one tenant");
    // A client on a secret moves to CI's own identity, keeping its id (the checkup's fix).
    let moving = json_of(&roll(&api, "", &["--json", "token", "client", "create", "moving", "--preset", "read-only"]));
    let mid = moving["client_id"].as_str().unwrap();
    assert!(roll(&api, "", &["token", "client", "rotate", mid, "--github", "acme/api", "--environment", "production"]).status.success());
    let list = json_of(&roll(&api, "", &["--json", "token", "client", "list"]));
    let moved = list.as_array().unwrap().iter().find(|c| c["client_id"] == mid).unwrap();
    assert_eq!((moved["trusts"]["subject"].as_str(), moved["signs_with_key"].as_bool()), (Some("repo:acme/api:environment:production"), Some(false)), "{moved}");
}

/// `roll login` signs a person's CLI in through the browser: it registers itself, the consent
/// screen's answer comes back to its loopback port, commands with no token set use the login
/// (within what was allowed), and `roll logout` revokes the connection. With no browser here
/// (`--no-browser`, SSH) it signs in with a code entered on roll instead (RFC 8628).
#[tokio::test(flavor = "multi_thread")]
async fn roll_login_signs_the_cli_in_through_the_browser() {
    use std::io::BufRead;
    let (api, _dir) = serve().await;
    let home = tempfile::tempdir().unwrap();
    let bare = |args: &[&str]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_roll"));
        c.env_clear().env("PATH", std::env::var("PATH").unwrap_or_default()).env("ROLL_API_URL", &api).env("XDG_CONFIG_HOME", home.path()).args(args);
        c
    };
    // A browser that opened, as far as the CLI can tell (`$BROWSER`, as gh reads it).
    let mut login = bare(&["login"]).env("BROWSER", "true").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let mut lines = std::io::BufReader::new(login.stderr.take().unwrap()).lines();
    let address = lines.by_ref().map_while(Result::ok).find_map(|l| l.trim().contains("code_challenge=").then(|| l.trim().to_string())).expect("the address to open");
    let asked = url::Url::parse(&address).unwrap();
    let q = |k: &str| asked.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned()).unwrap();

    // The consent screen: the operator lets it read Postgres, nothing else.
    let http = reqwest::Client::new();
    let consent = json!({ "client_id": q("client_id"), "redirect_uri": q("redirect_uri"), "code_challenge": q("code_challenge"), "code_challenge_method": "S256", "state": q("state"),
        "rules": [{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }] });
    let answer: Value = http.post(format!("{api}/v1/access/oauth/authorize")).bearer_auth(OPERATOR).json(&consent).send().await.unwrap().json().await.unwrap();
    // The browser follows the redirect back to the CLI.
    let back = http.get(answer["redirect"].as_str().unwrap()).send().await.unwrap();
    assert!(back.status().is_success());
    let out = tokio::task::spawn_blocking(move || login.wait_with_output()).await.unwrap().unwrap();
    assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("Signed in"), "{out:?}");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(home.path().join("roll/logins.json")).unwrap().permissions().mode() & 0o777, 0o600);

    // No token set: commands use the login, within its rules, and whoami says it's the login's
    // (or, with the operator's token set too, that the login loses to it: gh auth status).
    let me = json_of(&bare(&["--json", "token", "whoami"]).output().unwrap());
    let id = me["grant"]["id"].as_str().unwrap().to_string();
    assert!(me["credential"]["source"].as_str().is_some_and(|s| s.starts_with("roll login")), "{me}");
    let both = json_of(&bare(&["--json", "token", "whoami"]).env("ROLL_ADMIN_TOKEN", OPERATOR).output().unwrap());
    assert_eq!(both["credential"]["source"], "ROLL_ADMIN_TOKEN");
    assert!(both["credential"]["unused"][0].as_str().is_some_and(|s| s.starts_with("roll login")), "{both}");
    assert!(bare(&["db", "list"]).output().unwrap().status.success());
    assert!(!bare(&["token", "list"]).output().unwrap().status.success());
    // An access token at its end is refreshed, and the new pair kept.
    let file = home.path().join("roll/logins.json");
    let mut saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let before = saved[&api]["access_token"].as_str().unwrap().to_string();
    saved[&api]["expires_at"] = json!(0);
    std::fs::write(&file, saved.to_string()).unwrap();
    assert!(bare(&["db", "list"]).output().unwrap().status.success());
    let after: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert!(after[&api]["access_token"].as_str().unwrap() != before && after[&api]["expires_at"].as_i64().unwrap() > 0);

    // Signing out revokes the connection, not just this machine's copy, and says so in its log.
    assert!(bare(&["logout"]).output().unwrap().status.success());
    let g: Value = http.get(format!("{api}/v1/access/tokens/{id}")).bearer_auth(OPERATOR).send().await.unwrap().json().await.unwrap();
    assert!(g["revoked_at"].is_number(), "{g}");
    let mut revoked = false;
    for _ in 0..50 {
        let log: Value = http.get(format!("{api}/v1/access/audit?grant={id}")).bearer_auth(OPERATOR).send().await.unwrap().json().await.unwrap();
        if log.as_array().unwrap().iter().any(|e| e["decision"] == "revoke" && e["args"]["by"] == "its client") {
            revoked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(revoked, "the client's revoke is in the token's log");
    // An address that isn't the server's own (its issuer): no login, so a token is never kept
    // under, and later sent to, an address other than the one that issued it.
    let other = api.replace("127.0.0.1", "localhost");
    let wrong = bare(&["login", "--no-browser"]).env("ROLL_API_URL", &other).stdin(std::process::Stdio::null()).output().unwrap();
    assert!(!wrong.status.success() && String::from_utf8_lossy(&wrong.stderr).contains("says it's"), "{wrong:?}");
    assert!(!bare(&["db", "list"]).output().unwrap().status.success());

    // No browser here: a code to enter on roll, allowed by a person; the CLI's poll takes it.
    let mut login = bare(&["login", "--no-browser"]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let mut lines = std::io::BufReader::new(login.stderr.take().unwrap()).lines();
    let said = lines.by_ref().map_while(Result::ok).find(|l| l.contains("enter the code")).expect("a code to enter");
    let code = said.split("enter the code ").nth(1).unwrap().split('.').next().unwrap().trim().to_string();
    let allow = json!({ "rules": [{ "product": "postgres", "resources": "*", "actions": { "read": "allow" } }] });
    let r = http.post(format!("{api}/v1/access/oauth/devices/{code}")).bearer_auth(OPERATOR).json(&allow).send().await.unwrap();
    assert!(r.status().is_success(), "{:?}", r.text().await);
    let out = tokio::task::spawn_blocking(move || login.wait_with_output()).await.unwrap().unwrap();
    assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("Signed in"), "{out:?}");
    assert!(bare(&["db", "list"]).output().unwrap().status.success());
}
