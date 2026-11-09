//! The owner's model gateway on the relay (`src/agents/gateway.rs`, docs/code/gateway.md): the
//! plan's routes, joining a machine (the host mints a client key sealed to the member, and the
//! relay passes that box on unchanged in the member's `endpoint.set`), leaving (the endpoint
//! removed, the key revoked), a failed join taking its key back, the host joining itself on
//! loopback, a new sandbox's `hello` joining it, and nothing the relay keeps holding a box. The
//! daemons are test daemons speaking the real protocol over a real socket.

mod common;

use agent_proto::{
    Capabilities, Command, DaemonFrame, GatewayClient, IssuedClient, KeyChange, MachineId, MachineInfo, ProviderId, ProviderInfo, RelayFrame, Reply, SealedSecret,
};
use axum::http::StatusCode;
use common::daemon::{Daemon, dial, info};
use common::{App, app};
use serde_json::{Value, json};
use std::sync::Arc;

const ADMIN: &str = "admin-secret";

struct World {
    a: App,
    url: String,
    owner: String,
    agent: String,
}

async fn world() -> World {
    let a = app(Some(ADMIN));
    let (s, v) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), Some(ADMIN)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let mint = |owner: &'static str, name: &'static str| {
        let a = &a;
        async move {
            let (s, t) = a.req("POST", &format!("/v1/code/owners/{owner}/tokens"), Some(json!({ "name": name })), Some(ADMIN)).await;
            assert_eq!(s, StatusCode::CREATED);
            t["token"].as_str().unwrap().to_string()
        }
    };
    let owner = mint("acme", "me").await;
    let agent = mint("acme", "agent:bot").await;
    let url = a.serve().await;
    World { a, url, owner, agent }
}

impl World {
    async fn req(&self, method: &str, path: &str, body: Option<Value>, who: &str) -> (StatusCode, Value) {
        self.a.req(method, path, body, Some(who)).await
    }

    async fn machine(&self, name: &str) -> (String, String) {
        let (s, v) = self.req("POST", "/v1/code/agents/acme/machines", Some(json!({ "name": name })), &self.owner).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        (v["machine"]["id"].as_str().unwrap().to_string(), v["token"].as_str().unwrap().to_string())
    }
}

/// A seal key's shape (32 bytes, base64url), different per `n`.
fn seal_key(n: u8) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([n; 32])
}

/// A daemon's hello with a seal key and a Claude and a Codex.
fn hello(n: u8) -> MachineInfo {
    let provider = |id: ProviderId, name: &str| ProviderInfo { installed: true, ..ProviderInfo::new(id, name.into(), Capabilities::default()) };
    MachineInfo { seal_key: Some(seal_key(n)), providers: vec![provider(ProviderId::claude(), "Claude Code"), provider(ProviderId::codex(), "Codex")], ..info() }
}

/// Connects a daemon whose hello is `info`.
async fn connect(url: &str, token: &str, info: MachineInfo) -> Daemon {
    let mut d = dial(url, token).await.unwrap();
    d.send(&DaemonFrame::Hello { protocol: agent_proto::PROTOCOL, info }).await;
    assert!(matches!(d.recv().await, Some(RelayFrame::Welcome { .. })));
    d
}

/// The next command the daemon gets: (id, command).
async fn command(d: &mut Daemon) -> (String, Command) {
    match d.recv().await {
        Some(RelayFrame::Command { id, command, .. }) => (id, *command),
        other => panic!("expected a command, got {other:?}"),
    }
}

/// A box as the host would seal it: opaque to the relay.
fn sealed_box(tag: &str) -> SealedSecret {
    SealedSecret { v: 1, epk: "E".repeat(43), nonce: "N".repeat(16), ct: format!("{tag}{}", "C".repeat(48)) }
}

fn plan(host: &str, url: &str) -> Value {
    json!({ "host": host, "urls": [{ "label": "Tailscale", "url": url }], "fallback": true, "acknowledged_at": 1_700_000_000_000u64 })
}

/// Every file under `dir` that holds `needle`.
fn files_holding(dir: &std::path::Path, needle: &str) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if std::fs::read(&path).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes())) {
                found.push(path);
            }
        }
    }
    found
}

/// Joining: the host gets `gateway.client.issue` with the member's seal key, and the member gets
/// `endpoint.set` for `roll-gateway` with the host's box exactly as the host sent it; the plan
/// records the client. The host can't change while machines use it, nor can the plan go. Leaving
/// removes the member's endpoint and revokes its key. A join whose member doesn't take the key
/// revokes it again and records why. The host joins itself on loopback. No box is kept anywhere.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joining_passes_the_sealed_box_through_and_leaving_revokes_it() {
    let w = Arc::new(world().await);
    let base = "/v1/code/agents/acme/gateway";
    let (host, host_token) = w.machine("studio").await;
    let (member, member_token) = w.machine("laptop").await;
    let (offline, _) = w.machine("offline").await;
    let mut h = connect(&w.url, &host_token, hello(1)).await;
    let mut m = connect(&w.url, &member_token, hello(2)).await;

    // The plan: the notice acknowledged first, the host one of the owner's machines, an http(s)
    // URL, and members only the relay's (members sent with it are ignored).
    let mut unread = plan(&host, "http://gw.tailnet:8790");
    unread["acknowledged_at"] = json!(0);
    let (s, v) = w.req("PUT", base, Some(unread), &w.owner).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert!(v["error"].as_str().unwrap().contains("notice"), "{v}");
    assert_eq!(w.req("PUT", base, Some(plan("mch_nope", "http://gw.tailnet:8790")), &w.owner).await.0, StatusCode::NOT_FOUND);
    assert_eq!(w.req("PUT", base, Some(plan(&host, "ftp://gw")), &w.owner).await.0, StatusCode::BAD_REQUEST);
    let mut with_members = plan(&host, "http://gw.tailnet:8790/");
    with_members["members"] = json!([{ "machine": "mch_sneaky", "url": "http://x", "joined_at": 1 }]);
    let (s, v) = w.req("PUT", base, Some(with_members), &w.owner).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["urls"][0]["url"].as_str(), v["members"].as_array()), (Some("http://gw.tailnet:8790"), None), "{v}");
    assert_eq!(w.req("GET", base, None, &w.owner).await, (StatusCode::OK, v));

    // Refused before anything goes out: an agent, a machine that isn't the owner's, a URL that
    // isn't the plan's.
    let join = |machine: &str, url: Option<&str>| json!({ "machine": machine, "url": url });
    assert_eq!(w.req("POST", &format!("{base}/members"), Some(join(&member, None)), &w.agent).await.0, StatusCode::FORBIDDEN);
    assert_eq!(w.req("POST", &format!("{base}/members"), Some(join("mch_nope", None)), &w.owner).await.0, StatusCode::NOT_FOUND);
    let loopback = format!("http://{}:8790", std::net::Ipv4Addr::LOCALHOST);
    assert_eq!(w.req("POST", &format!("{base}/members"), Some(join(&member, Some(&loopback))), &w.owner).await.0, StatusCode::BAD_REQUEST);

    let pending = {
        let (w, body) = (w.clone(), join(&member, None));
        tokio::spawn(async move { w.req("POST", "/v1/code/agents/acme/gateway/members", Some(body), &w.owner).await })
    };
    let (id, c) = command(&mut h).await;
    let Command::GatewayClientIssue { name, machine, seal_key: to } = c else { panic!("not gateway.client.issue: {c:?}") };
    assert_eq!((name.as_str(), machine, to), ("laptop", Some(MachineId(member.clone())), seal_key(2)));
    let issued = IssuedClient { client: GatewayClient { id: "gwc_0000000000000001".into(), name, machine: Some(MachineId(member.clone())), created_at: 1 }, sealed: sealed_box("ONE") };
    h.send(&DaemonFrame::Reply { id, result: Reply::GatewayClient(issued.clone()) }).await;
    let (id, c) = command(&mut m).await;
    let Command::EndpointSet { endpoint, key } = c else { panic!("not endpoint.set: {c:?}") };
    assert_eq!((endpoint.id.as_str(), endpoint.name.as_str(), endpoint.kind), ("roll-gateway", "roll gateway on studio", agent_proto::EndpointKind::Gateway));
    assert_eq!((endpoint.base_url.as_str(), endpoint.fallback), ("http://gw.tailnet:8790", true));
    assert_eq!(endpoint.providers, vec![ProviderId::claude(), ProviderId::codex()]);
    assert_eq!(key, KeyChange::Set { sealed: issued.sealed.clone() }, "the box passes unchanged");
    m.send(&DaemonFrame::Reply { id, result: Reply::Ok }).await;
    let (s, p) = pending.await.unwrap();
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!((p["members"][0]["machine"].as_str(), p["members"][0]["client"].as_str()), (Some(member.as_str()), Some("gwc_0000000000000001")), "{p}");
    assert!(p["members"][0]["error"].is_null());

    // While it's in use: no other host, and the plan stays.
    let (s, v) = w.req("PUT", base, Some(plan(&member, "http://gw.tailnet:8790")), &w.owner).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v["error"].as_str().unwrap().contains("remove the machines first"));
    assert_eq!(w.req("DELETE", base, None, &w.owner).await.0, StatusCode::CONFLICT);
    // The same host with other settings is fine, and keeps the members.
    let mut changed = plan(&host, "http://gw.tailnet:8790");
    changed["new_sandboxes"] = json!(true);
    let (s, v) = w.req("PUT", base, Some(changed), &w.owner).await;
    assert_eq!((s, v["members"].as_array().map(Vec::len)), (StatusCode::OK, Some(1)), "{v}");

    // A member that doesn't take the key: the host revokes it again, and the plan says why.
    let pending = {
        let (w, body) = (w.clone(), join(&offline, None));
        tokio::spawn(async move { w.req("POST", "/v1/code/agents/acme/gateway/members", Some(body), &w.owner).await })
    };
    let (s, v) = pending.await.unwrap();
    assert_eq!(s, StatusCode::CONFLICT, "no seal key yet: {v}");
    let mut o = connect(&w.url, &w.machine("flaky").await.1, hello(3)).await;
    let flaky = w.a.req("GET", "/v1/code/agents/acme/machines", None, Some(&w.owner)).await.1.as_array().unwrap().iter().find(|x| x["name"] == "flaky").unwrap()["id"].as_str().unwrap().to_string();
    let pending = {
        let (w, body) = (w.clone(), join(&flaky, None));
        tokio::spawn(async move { w.req("POST", "/v1/code/agents/acme/gateway/members", Some(body), &w.owner).await })
    };
    let (id, _) = command(&mut h).await;
    let second = IssuedClient { client: GatewayClient { id: "gwc_0000000000000002".into(), name: "flaky".into(), machine: Some(MachineId(flaky.clone())), created_at: 2 }, sealed: sealed_box("TWO") };
    h.send(&DaemonFrame::Reply { id, result: Reply::GatewayClient(second) }).await;
    let (id, _) = command(&mut o).await;
    o.send(&DaemonFrame::Reply { id, result: agent_proto::Reply::Error(agent_proto::ReplyError { code: agent_proto::ErrorCode::Invalid, message: "no provider claude on this machine".into() }) }).await;
    let (id, c) = command(&mut h).await;
    assert_eq!(c, Command::GatewayClientRevoke { client: "gwc_0000000000000002".into() }, "taken back");
    h.send(&DaemonFrame::Reply { id, result: Reply::Ok }).await;
    let (s, v) = pending.await.unwrap();
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (_, p) = w.req("GET", base, None, &w.owner).await;
    let failed = p["members"].as_array().unwrap().iter().find(|x| x["machine"] == flaky.as_str()).unwrap().clone();
    assert!(failed["error"].as_str().unwrap().contains("no provider claude"), "{failed}");
    assert!(failed["client"].is_null());

    // The host joins itself through loopback.
    let pending = {
        let (w, body) = (w.clone(), join(&host, Some(&loopback)));
        tokio::spawn(async move { w.req("POST", "/v1/code/agents/acme/gateway/members", Some(body), &w.owner).await })
    };
    let (id, _) = command(&mut h).await;
    let third = IssuedClient { client: GatewayClient { id: "gwc_0000000000000003".into(), name: "studio".into(), machine: Some(MachineId(host.clone())), created_at: 3 }, sealed: sealed_box("THREE") };
    h.send(&DaemonFrame::Reply { id, result: Reply::GatewayClient(third) }).await;
    let (id, c) = command(&mut h).await;
    let Command::EndpointSet { endpoint, .. } = c else { panic!("not endpoint.set: {c:?}") };
    assert_eq!(endpoint.base_url, loopback);
    h.send(&DaemonFrame::Reply { id, result: Reply::Ok }).await;
    assert_eq!(pending.await.unwrap().0, StatusCode::OK);

    // Leaving: the member forgets the endpoint, the host revokes the key.
    let pending = {
        let (w, path) = (w.clone(), format!("{base}/members/{member}"));
        tokio::spawn(async move { w.req("DELETE", &path, None, &w.owner).await })
    };
    let (id, c) = command(&mut m).await;
    assert_eq!(c, Command::EndpointRemove { endpoint: "roll-gateway".into() });
    m.send(&DaemonFrame::Reply { id, result: Reply::Ok }).await;
    let (id, c) = command(&mut h).await;
    assert_eq!(c, Command::GatewayClientRevoke { client: "gwc_0000000000000001".into() });
    h.send(&DaemonFrame::Reply { id, result: Reply::Ok }).await;
    let (s, p) = pending.await.unwrap();
    assert_eq!(s, StatusCode::OK, "{p}");
    assert!(!p["members"].as_array().unwrap().iter().any(|x| x["machine"] == member.as_str()), "{p}");
    assert_eq!(w.req("DELETE", &format!("{base}/members/{member}"), None, &w.owner).await.0, StatusCode::NOT_FOUND);

    // Nothing the relay keeps holds a box (commands are never stored).
    for tag in ["ONE", "TWO", "THREE"] {
        let ct = format!("{tag}{}", "C".repeat(48));
        assert_eq!(files_holding(w.a.dir.path(), &ct), Vec::<std::path::PathBuf>::new(), "{tag}");
    }
}

/// With `new_sandboxes`, a sandbox's first `hello` joins it on its own (the relay sends as `roll`);
/// a machine that isn't a sandbox is left alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_sandbox_joins_the_gateway_on_its_hello() {
    let w = world().await;
    let (host, host_token) = w.machine("studio").await;
    let (laptop, laptop_token) = w.machine("laptop").await;
    let (sandbox, sandbox_token) = w.machine("sandbox-1").await;
    let info = json!({ "backend": "docker", "size": "small", "status": "running", "hourly_cents": 0 });
    let n = w.a.code.db.sql().execute("UPDATE agent_machines SET sandbox = ? WHERE id = ?", code_sql::params![info.to_string(), sandbox]).unwrap();
    assert_eq!(n, 1);
    // A sandbox roll hosts elsewhere (agent37) has no route to the gateway.
    let (hosted, hosted_token) = w.machine("build-box").await;
    let info = json!({ "backend": "agent37", "size": "small", "status": "running", "hourly_cents": 3 });
    w.a.code.db.sql().execute("UPDATE agent_machines SET sandbox = ? WHERE id = ?", code_sql::params![info.to_string(), hosted]).unwrap();
    let mut p = plan(&host, "http://gw.tailnet:8790");
    p["new_sandboxes"] = json!(true);
    assert_eq!(w.req("PUT", "/v1/code/agents/acme/gateway", Some(p), &w.owner).await.0, StatusCode::OK);
    let mut h = connect(&w.url, &host_token, hello(1)).await;

    // Not a sandbox, or a hosted one: nothing.
    let _l = connect(&w.url, &laptop_token, hello(2)).await;
    let _hosted = connect(&w.url, &hosted_token, hello(4)).await;
    // A sandbox: joined.
    let mut s = connect(&w.url, &sandbox_token, hello(3)).await;
    let (id, c) = command(&mut h).await;
    let Command::GatewayClientIssue { machine, seal_key: to, .. } = c else { panic!("not gateway.client.issue: {c:?}") };
    assert_eq!((machine, to), (Some(MachineId(sandbox.clone())), seal_key(3)), "the sandbox, not {laptop} or {hosted}");
    let issued = IssuedClient { client: GatewayClient { id: "gwc_00000000000000aa".into(), name: "sandbox-1".into(), machine: Some(MachineId(sandbox.clone())), created_at: 1 }, sealed: sealed_box("SBX") };
    h.send(&DaemonFrame::Reply { id, result: Reply::GatewayClient(issued.clone()) }).await;
    let (id, c) = command(&mut s).await;
    assert!(matches!(&c, Command::EndpointSet { key, .. } if *key == KeyChange::Set { sealed: issued.sealed.clone() }), "{c:?}");
    s.send(&DaemonFrame::Reply { id, result: Reply::Ok }).await;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (_, p) = w.req("GET", "/v1/code/agents/acme/gateway", None, &w.owner).await;
        if p["members"][0]["client"] == "gwc_00000000000000aa" {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "{p}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // Its next hello doesn't join it again.
    drop(s);
    let _s = connect(&w.url, &sandbox_token, hello(3)).await;
    assert!(tokio::time::timeout(std::time::Duration::from_millis(300), h.recv()).await.is_err(), "joined once");
    // Joining the hosted sandbox by hand is refused with why.
    let (s, v) = w.req("POST", "/v1/code/agents/acme/gateway/members", Some(json!({ "machine": hosted })), &w.owner).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert!(v.to_string().contains("can't reach your computers"), "{v}");
}

/// `gateway.*` commands through `…/commands`: the relay checks their shape before they reach the
/// host (settings that would listen everywhere, a key that isn't a sealed box, a seal key that
/// isn't one) and agents can't send them; a sealed upstream key reaches the host as it was sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_commands_are_checked_before_they_reach_the_host() {
    let w = Arc::new(world().await);
    let (host, host_token) = w.machine("studio").await;
    let mut h = connect(&w.url, &host_token, hello(1)).await;
    let path = format!("/v1/code/agents/acme/machines/{host}/commands");
    let cmd = |id: &str, c: Value| json!({ "id": id, "command": c });
    let everywhere = json!({ "type": "gateway.configure", "settings": { "enabled": true, "listen": [std::net::Ipv4Addr::UNSPECIFIED.to_string()] } });
    let plaintext = json!({ "type": "gateway.key", "upstream": "anthropic", "key": { "type": "set", "sealed": { "v": 1, "epk": "x", "nonce": "y", "ct": "sk-ant-plaintext" } } });
    let bad_seal = json!({ "type": "gateway.client.issue", "name": "laptop", "seal_key": "not-a-key" });
    for (i, c) in [everywhere, plaintext, bad_seal].into_iter().enumerate() {
        let (s, v) = w.req("POST", &path, Some(cmd(&format!("bad{i}"), c.clone())), &w.owner).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{c}: {v}");
        assert!(!v.to_string().contains("sk-ant-plaintext"), "{v}");
    }
    let sealed = sealed_box("UPSTREAM");
    let key = json!({ "type": "gateway.key", "upstream": "anthropic", "key": { "type": "set", "sealed": sealed } });
    assert_eq!(w.req("POST", &path, Some(cmd("k0", key.clone())), &w.agent).await.0, StatusCode::FORBIDDEN);
    let pending = {
        let (w, path, body) = (w.clone(), path.clone(), cmd("k1", key));
        tokio::spawn(async move { w.req("POST", &path, Some(body), &w.owner).await })
    };
    let (id, c) = command(&mut h).await;
    assert_eq!(id, "k1", "nothing refused went out");
    assert_eq!(c, Command::GatewayKey { upstream: "anthropic".into(), key: KeyChange::Set { sealed } });
    h.send(&DaemonFrame::Reply { id, result: Reply::Ok }).await;
    assert_eq!(pending.await.unwrap().0, StatusCode::OK);
    assert_eq!(files_holding(w.a.dir.path(), &format!("UPSTREAM{}", "C".repeat(48))), Vec::<std::path::PathBuf>::new());
}
