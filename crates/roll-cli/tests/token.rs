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
    let config = roll_access::Config { locked: true, approval_wait: std::time::Duration::from_secs(20), ..Default::default() };
    let access = roll_access::Access::open(&code_sql::Target::Sqlite(dir.path().join("access.db")), Default::default(), config, Default::default()).unwrap();
    let gate = roll_auth::Gate::new(Some(roll_auth::AdminToken::new(OPERATOR).unwrap()), Vec::new());
    gate.set_authorizer(Arc::new(roll_access::evaluate::Evaluator(access.clone())));
    // A stand-in for Postgres's routes.
    let postgres = axum::Router::new()
        .route("/v1/projects", axum::routing::get(|| async { axum::Json(json!([{ "id": "prj_a", "name": "shop" }])) }))
        .route("/v1/projects/{id}", axum::routing::delete(|| async { axum::http::StatusCode::NO_CONTENT }));
    let router = postgres
        .merge(roll_workspace::router(workspace))
        .merge(roll_access::api::router(access))
        .layer(axum::middleware::from_fn_with_state(gate, roll_auth::check));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api = format!("http://{}", listener.local_addr().unwrap());
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
    assert!(audit.as_array().unwrap().iter().any(|e| e["decision"] == "ask"), "{audit}");
}
