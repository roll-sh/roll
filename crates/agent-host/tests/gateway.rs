//! roll's model gateway in the daemon (`src/gateway.rs`, `orchestrator/gateway.rs`,
//! docs/code/gateway.md): hosting it (configure, upstream keys sealed to this daemon, client keys
//! sealed to the machine that uses them, revoke, remove, `hello`, a restart) against a mock
//! Anthropic upstream on loopback, and the member side (the fallback health check and the
//! attribution headers) on fake providers.

mod common;

use agent_host::provider::fake::FakeProvider;
use agent_host::{Daemon, Outbound};
use agent_proto::*;
use common::*;
use serde_json::{Value, json};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const UPSTREAM_KEY: &str = "sk-ant-gateway-upstream-5b1e";

/// What the mock upstream was sent: the key headers and any `x-roll-*` header.
#[derive(Default)]
struct Seen {
    keys: Vec<String>,
    roll_headers: Vec<String>,
}

/// An Anthropic stand-in: `POST /v1/messages` answers a Messages response with usage when the
/// key is [`UPSTREAM_KEY`] (401 otherwise); `GET /v1/models` lists one model.
async fn mock_anthropic() -> (String, Arc<Mutex<Seen>>) {
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    let seen = Arc::new(Mutex::new(Seen::default()));
    let note = |seen: &Arc<Mutex<Seen>>, headers: &HeaderMap| -> bool {
        let key = headers.get("x-api-key").or_else(|| headers.get("authorization")).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let mut s = seen.lock().unwrap();
        s.roll_headers.extend(headers.keys().filter(|k| k.as_str().starts_with("x-roll-")).map(|k| k.to_string()));
        let ok = key == UPSTREAM_KEY || key == format!("Bearer {UPSTREAM_KEY}");
        s.keys.push(key);
        ok
    };
    let (a, b) = (seen.clone(), seen.clone());
    let app = axum::Router::new()
        .route(
            "/v1/messages",
            axum::routing::post(move |headers: HeaderMap, body: axum::Json<Value>| {
                let seen = a.clone();
                async move {
                    if !note(&seen, &headers) {
                        return (StatusCode::UNAUTHORIZED, axum::Json(json!({ "type": "error", "error": { "type": "authentication_error", "message": "bad key" } }))).into_response();
                    }
                    axum::Json(json!({
                        "id": "msg_1", "type": "message", "role": "assistant", "model": body.0["model"],
                        "content": [{ "type": "text", "text": "ok" }], "stop_reason": "end_turn", "stop_sequence": null,
                        "usage": { "input_tokens": 12, "output_tokens": 3 }
                    }))
                    .into_response()
                }
            }),
        )
        .route(
            "/v1/models",
            axum::routing::get(move |headers: HeaderMap| {
                let seen = b.clone();
                async move {
                    if !note(&seen, &headers) {
                        return StatusCode::UNAUTHORIZED.into_response();
                    }
                    axum::Json(json!({ "data": [{ "type": "model", "id": "claude-haiku-4-5", "display_name": "Claude Haiku 4.5" }], "has_more": false })).into_response()
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

/// A loopback port nothing listens on right now.
async fn free_port() -> u16 {
    let l = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await.unwrap();
    l.local_addr().unwrap().port()
}

fn settings(upstream: &str, port: u16) -> GatewaySettings {
    GatewaySettings {
        enabled: true,
        port,
        upstreams: vec![UpstreamSpec {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            kind: UpstreamKind::AnthropicCompatible,
            base_url: Some(upstream.into()),
            region: None,
            project: None,
            models: vec!["claude-*".into()],
            responses: false,
            headers: vec![],
            allow_private: true,
            key_ref: None,
            disabled: false,
        }],
        ..GatewaySettings::default()
    }
}

async fn gateway(host: &Arc<agent_host::Host>) -> Option<GatewayInfo> {
    host.info().await.settings.and_then(|s| s.gateway).map(|g| *g)
}

/// Waits up to 10 s for the hosted gateway to satisfy `f`.
async fn wait_gateway(host: &Arc<agent_host::Host>, what: &str, f: impl Fn(&GatewayInfo) -> bool) -> GatewayInfo {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let g = gateway(host).await;
        if let Some(g) = g.as_ref().filter(|g| f(g)) {
            return g.clone();
        }
        assert!(tokio::time::Instant::now() < deadline, "never {what}: {g:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A Messages request through the gateway with `key`: (status, body).
async fn ask(port: u16, key: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .post(format!("http://{}/v1/messages", SocketAddr::from((Ipv4Addr::LOCALHOST, port))))
        .header("anthropic-version", "2023-06-01")
        .bearer_auth(key)
        .json(&json!({ "model": "claude-haiku-4-5", "max_tokens": 8, "messages": [{ "role": "user", "content": "hi" }] }));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let r = req.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap_or(Value::Null))
}

fn issue(seal_key: &str, machine: &str) -> Command {
    Command::GatewayClientIssue { name: format!("{machine}'s key"), machine: Some(MachineId(machine.into())), seal_key: seal_key.into() }
}

/// Hosting: configured, it listens on a free loopback port and says so in `hello`; an upstream's
/// key sealed to this daemon (and only for that upstream) reaches the upstream; a client key is
/// answered sealed to the member's seal key as its `roll-gateway` endpoint key, opens there, and
/// works until revoked; usage is counted by the attribution headers, which never go upstream;
/// everything survives a restart; `gateway.remove` wipes it. No key is ever in `hello`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_hosts_the_gateway_with_sealed_keys() {
    let (upstream, seen) = mock_anthropic().await;
    let home = tempfile::tempdir().unwrap();
    let host = open(home.path(), vec![FakeProvider::new("claude")]).await;
    let mut events = host.subscribe();
    let seal_key = host.info().await.seal_key.unwrap();
    assert!(gateway(&host).await.is_none(), "never set up here");
    assert!(matches!(run(&host, Command::GatewayStatus {}).await, Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })));

    let port = free_port().await;
    let bad = GatewaySettings { listen: vec![Ipv4Addr::UNSPECIFIED.to_string()], ..settings(&upstream, port) };
    assert!(matches!(run(&host, Command::GatewayConfigure { settings: Box::new(bad) }).await, Reply::Error(ReplyError { code: ErrorCode::Invalid, .. })));
    assert_eq!(run(&host, Command::GatewayConfigure { settings: Box::new(settings(&upstream, port)) }).await, Reply::Ok);
    let g = wait_gateway(&host, "listened", |g| matches!(g.run, GatewayRun::Running { .. })).await;
    assert_eq!(g.keys, vec![UpstreamKey { upstream: "anthropic".into(), key: KeyState::None }]);
    let health = reqwest::get(format!("http://{}/health", SocketAddr::from((Ipv4Addr::LOCALHOST, port)))).await.unwrap();
    assert_eq!(health.status(), 200);

    // A box for an endpoint (or for another upstream) never opens as an upstream's key.
    let wrong = agent_host::endpoints::seal(&seal_key, "anthropic", UPSTREAM_KEY).unwrap();
    let r = run(&host, Command::GatewayKey { upstream: "anthropic".into(), key: KeyChange::Set { sealed: wrong } }).await;
    assert!(matches!(&r, Reply::Error(ReplyError { code: ErrorCode::Invalid, message }) if message.contains("sealed to this machine")), "{r:?}");
    let sealed = agent_host::endpoints::seal_with(&seal_key, &agent_host::gateway::upstream_key_context("anthropic"), UPSTREAM_KEY).unwrap();
    let r = run(&host, Command::GatewayKey { upstream: "nope".into(), key: KeyChange::Set { sealed: sealed.clone() } }).await;
    assert!(matches!(r, Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })), "{r:?}");
    assert_eq!(run(&host, Command::GatewayKey { upstream: "anthropic".into(), key: KeyChange::Set { sealed } }).await, Reply::Ok);
    let g = gateway(&host).await.unwrap();
    assert_eq!(g.keys[0].key, KeyState::Sealed);

    // A client key for a member: sealed to its seal key, for its `roll-gateway` endpoint.
    let member = agent_host::endpoints::SealKey::generate();
    let r = run(&host, issue("not-a-seal-key", "mch_member")).await;
    assert!(matches!(r, Reply::Error(ReplyError { code: ErrorCode::Invalid, .. })), "{r:?}");
    let Reply::GatewayClient(issued) = run(&host, issue(&member.public(), "mch_member")).await else { panic!("no client") };
    assert!(issued.client.id.starts_with("gwc_") && issued.client.machine == Some(MachineId("mch_member".into())));
    let client_key = member.open(gateway_limits::ENDPOINT_ID, &issued.sealed).expect("the member opens it as its endpoint key");
    assert!(client_key.expose().starts_with("rgk_"));
    assert!(member.open("anthropic", &issued.sealed).is_err(), "only as the gateway endpoint's key");
    let g = gateway(&host).await.unwrap();
    assert_eq!(g.clients.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(), vec![issued.client.id.as_str()]);

    // Through the gateway: the upstream gets its key and never the attribution headers.
    let (status, body) = ask(port, client_key.expose(), &[("x-roll-thread", "thr_0123456789abcdef"), ("x-roll-project", "prj_0123456789abcdef")]).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["content"][0]["text"], "ok");
    {
        let s = seen.lock().unwrap();
        assert!(s.keys.iter().any(|k| k.contains(UPSTREAM_KEY)), "the upstream got its key");
        assert!(!s.keys.iter().any(|k| k.contains(client_key.expose())), "never the client's");
        assert!(s.roll_headers.is_empty(), "{:?}", s.roll_headers);
    }
    let usage = loop {
        let Reply::GatewayUsage(report) = run(&host, Command::GatewayUsage { from: 0, to: u64::MAX / 2, by: UsageBy::Thread, bucket: UsageBucket::Day }).await else { panic!("no usage") };
        if !report.rows.is_empty() {
            break report;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(usage.rows[0].key, "thr_0123456789abcdef");
    assert_eq!((usage.rows[0].input_tokens, usage.rows[0].output_tokens), (12, 3));
    let Reply::GatewayLog { entries } = run(&host, Command::GatewayLog { after: None, before: None, limit: 0, errors: false }).await else { panic!("no log") };
    assert_eq!(entries.len(), 1);
    assert_eq!((entries[0].machine.as_ref().map(|m| m.0.as_str()), entries[0].status), (Some("mch_member"), 200));
    let Reply::GatewayStatus(status) = run(&host, Command::GatewayStatus {}).await else { panic!("no status") };
    assert!(matches!(status.run, GatewayRun::Running { .. }));
    let Reply::GatewayTested { checks } = run(&host, Command::GatewayTest { upstream: None }).await else { panic!("no test") };
    assert!(checks.len() == 1 && checks[0].ok, "{checks:?}");

    // Kept across a restart: settings, the sealed key and the client.
    // A clean stop (SIGTERM) stops the gateway too.
    host.shutdown().await;
    drop(host);
    let host = open(home.path(), vec![FakeProvider::new("claude")]).await;
    wait_gateway(&host, "listened again", |g| matches!(g.run, GatewayRun::Running { .. }) && g.keys[0].key == KeyState::Sealed).await;
    assert_eq!(ask(port, client_key.expose(), &[]).await.0, 200);

    // Revoked: refused at once.
    assert_eq!(run(&host, Command::GatewayClientRevoke { client: issued.client.id.clone() }).await, Reply::Ok);
    assert_eq!(ask(port, client_key.expose(), &[]).await.0, 401);
    assert!(matches!(run(&host, Command::GatewayClientRevoke { client: issued.client.id.clone() }).await, Reply::Error(ReplyError { code: ErrorCode::NotFound, .. })));
    assert!(gateway(&host).await.unwrap().clients.is_empty());

    // Removed: stopped, forgotten, its directory gone.
    assert_eq!(run(&host, Command::GatewayRemove {}).await, Reply::Ok);
    assert!(gateway(&host).await.is_none());
    assert!(!home.path().join("gateway").exists());
    assert!(reqwest::get(format!("http://{}/health", SocketAddr::from((Ipv4Addr::LOCALHOST, port)))).await.is_err(), "it stopped listening");
    assert_eq!(run(&host, Command::GatewayRemove {}).await, Reply::Ok, "removing nothing is done");

    // Nothing the daemon sent held a key, and agent.db holds the upstream key only while hosted.
    while let Ok(out) = events.try_recv() {
        let text = match out {
            Outbound::Info(i) => serde_json::to_string(&i).unwrap(),
            Outbound::Entity(e) => serde_json::to_string(&e).unwrap(),
            Outbound::Delta(d) => serde_json::to_string(&d).unwrap(),
        };
        assert!(!text.contains(UPSTREAM_KEY) && !text.contains(client_key.expose()), "sent a key: {text}");
    }
    let db = std::fs::read(home.path().join("agent.db")).unwrap();
    assert!(!db.windows(client_key.expose().len()).any(|w| w == client_key.expose().as_bytes()), "the client key is never stored");
}

/// `hello` follows the run state: a port taken by something else is `failed` with why, and a
/// gateway turned off is `stopped`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hello_carries_the_gateways_run_state() {
    let (upstream, _) = mock_anthropic().await;
    let home = tempfile::tempdir().unwrap();
    let host = open(home.path(), vec![FakeProvider::new("claude")]).await;
    let taken = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await.unwrap();
    let port = taken.local_addr().unwrap().port();
    assert_eq!(run(&host, Command::GatewayConfigure { settings: Box::new(settings(&upstream, port)) }).await, Reply::Ok);
    wait_gateway(&host, "failed to listen", |g| matches!(&g.run, GatewayRun::Failed { error, .. } if !error.is_empty())).await;
    drop(taken);
    let off = GatewaySettings { enabled: false, ..settings(&upstream, port) };
    assert_eq!(run(&host, Command::GatewayConfigure { settings: Box::new(off) }).await, Reply::Ok);
    wait_gateway(&host, "stopped", |g| g.run == GatewayRun::Stopped).await;
    let on = settings(&upstream, free_port().await);
    assert_eq!(run(&host, Command::GatewayConfigure { settings: Box::new(on) }).await, Reply::Ok);
    wait_gateway(&host, "listened", |g| matches!(g.run, GatewayRun::Running { .. })).await;
}

/// A stand-in gateway for members: `/health` answers.
async fn mock_health() -> String {
    let app = axum::Router::new().route("/health", axum::routing::get(|| async { axum::Json(json!({ "ok": true })) }));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0))).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn member_endpoint(base: &str, fallback: bool) -> Command {
    let spec = EndpointSpec {
        id: gateway_limits::ENDPOINT_ID.into(),
        name: "roll gateway on studio".into(),
        kind: EndpointKind::Gateway,
        base_url: base.into(),
        providers: vec![ProviderId::claude()],
        url_env: None,
        key_env: None,
        key_ref: None,
        fallback,
    };
    Command::EndpointSet { endpoint: spec, key: KeyChange::Keep }
}

fn env_of(session: &agent_host::provider::fake::FakeSession) -> Vec<(String, String)> {
    session.config.env.clone()
}

fn var<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
    env.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

/// The member side: a provider whose endpoint is a gateway gets per-thread attribution headers;
/// with `fallback`, a gateway that doesn't answer `/health` leaves the provider on its own login
/// (and its status says so) until it answers again; without it the variables stay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn members_fall_back_when_the_gateway_is_down_and_name_their_threads() {
    let claude = FakeProvider::new("claude");
    let w = World::new(vec![claude.clone()]).await;
    let dead = format!("http://{}", SocketAddr::from((Ipv4Addr::LOCALHOST, free_port().await)));

    // Down, with fallback: the process starts without the gateway, and the status says why.
    assert_eq!(w.run(member_endpoint(&dead, true)).await, Reply::Ok);
    let t1 = w.thread("claude", Some("one"), false).await;
    let env = env_of(&claude.session(0).await);
    assert_eq!(var(&env, "ANTHROPIC_BASE_URL"), None, "{env:?}");
    assert_eq!(var(&env, "ANTHROPIC_CUSTOM_HEADERS"), None);
    let detail = |info: &MachineInfo| info.providers.iter().find(|p| p.id == ProviderId::claude()).and_then(|p| p.detail.clone());
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while detail(&w.host.info().await).as_deref() != Some("roll gateway offline: using its own login") {
        assert!(tokio::time::Instant::now() < deadline, "{:?}", detail(&w.host.info().await));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    w.wait_thread(&t1, |t| t.status == ThreadStatus::Idle).await;

    // Up: the variables, and headers naming the thread and project.
    let up = mock_health().await;
    assert_eq!(w.run(member_endpoint(&up, true)).await, Reply::Ok);
    let t2 = w.thread("claude", Some("two"), false).await;
    let env = env_of(&claude.session(1).await);
    assert_eq!(var(&env, "ANTHROPIC_BASE_URL"), Some(up.as_str()), "{env:?}");
    assert_eq!(var(&env, "ANTHROPIC_CUSTOM_HEADERS"), Some(format!("x-roll-thread: {t2}\nx-roll-project: {}", w.project).as_str()));
    assert_eq!(var(&env, "ROLL_GATEWAY_THREAD"), Some(t2.as_str()));
    assert_eq!(detail(&w.host.info().await).as_deref(), Some("via roll gateway on studio"));
    w.wait_thread(&t2, |t| t.status == ThreadStatus::Idle).await;

    // Down without fallback: requests fail, but the variables stay (no silent switch).
    assert_eq!(w.run(member_endpoint(&dead, false)).await, Reply::Ok);
    w.thread("claude", Some("three"), false).await;
    let env = env_of(&claude.session(2).await);
    assert_eq!(var(&env, "ANTHROPIC_BASE_URL"), Some(dead.as_str()));
    assert!(var(&env, "ANTHROPIC_CUSTOM_HEADERS").is_some());
}

/// Codex names its thread through `env_http_headers`, read from the session's variables.
#[test]
fn codex_reads_the_attribution_headers_from_its_session() {
    let spec = EndpointSpec {
        id: gateway_limits::ENDPOINT_ID.into(),
        name: "roll gateway on studio".into(),
        kind: EndpointKind::Gateway,
        base_url: "http://gw:8790".into(),
        providers: vec![],
        url_env: None,
        key_env: None,
        key_ref: None,
        fallback: false,
    };
    let used = agent_host::endpoints::codex_use(&spec, Some("rgk_x")).unwrap();
    assert!(used.args.contains(&r#"model_providers.roll_roll_gateway.env_http_headers={"x-roll-thread"="ROLL_GATEWAY_THREAD","x-roll-project"="ROLL_GATEWAY_PROJECT"}"#.to_string()), "{:?}", used.args);
    assert!(used.args.contains(&r#"model_providers.roll_roll_gateway.base_url="http://gw:8790/v1""#.to_string()));
    assert!(!used.args.iter().any(|a| a.contains("rgk_x")), "the key is never an argument");
}
