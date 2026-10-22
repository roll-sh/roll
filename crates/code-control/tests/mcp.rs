//! The MCP server (`/v1/code/mcp`) driven as a JSON-RPC client: the Streamable HTTP transport
//! (sessions, version negotiation, notifications, batches, Origin, GET/DELETE), every tool end to
//! end, land outcomes (landed, conflict, blocked), and authorization matching the HTTP routes.

mod common;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use common::{App, app};
use serde_json::{Value, json};

const MCP: &str = "/v1/code/mcp";

/// The tools docs/code/mcp.md lists.
const TOOLS: [&str; 22] = [
    "search_code", "read_file", "list_tree", "log", "blame", "get_changeset", "compare", "create_change", "revise_change", "land", "get_change",
    "list_changes", "claim_work", "heartbeat_work", "release_work", "complete_work", "list_work", "create_work", "comment", "review", "queue_status",
    "request_ai_review",
];
/// The ones that only read (`readOnlyHint`).
const READ_ONLY: [&str; 11] = ["search_code", "read_file", "list_tree", "log", "blame", "get_changeset", "compare", "get_change", "list_changes", "list_work", "queue_status"];

struct Client<'a> {
    a: &'a App,
    token: Option<String>,
    session: Option<String>,
    next: u64,
    /// Sent with every request (`X-Roll-Agent`, `X-Roll-User`).
    headers: Vec<(&'static str, String)>,
}

impl<'a> Client<'a> {
    fn new(a: &'a App, token: Option<&str>) -> Self {
        Client { a, token: token.map(str::to_string), session: None, next: 1, headers: Vec::new() }
    }

    async fn post(&self, body: Value, extra: &[(&str, &str)]) -> (StatusCode, HeaderMap, Value) {
        let mut b = Request::builder().method("POST").uri(MCP).header("content-type", "application/json").header("accept", "application/json, text/event-stream");
        if let Some(t) = &self.token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        if let Some(s) = &self.session {
            b = b.header("mcp-session-id", s.as_str());
        }
        for (k, v) in &self.headers {
            b = b.header(*k, v.as_str());
        }
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        let resp = self.a.send(b.body(Body::from(body.to_string())).unwrap()).await;
        let (status, headers) = (resp.status(), resp.headers().clone());
        let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20).await.unwrap();
        (status, headers, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() })
    }

    async fn initialize(&mut self, version: &str) -> Value {
        let (s, h, v) = self
            .post(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": version, "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } } }), &[])
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        self.session = Some(h["mcp-session-id"].to_str().unwrap().to_string());
        let (s, _, body) = self.post(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), &[]).await;
        assert_eq!(s, StatusCode::ACCEPTED);
        assert!(body.is_null());
        v
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let (s, _, v) = self.post(json!({ "jsonrpc": "2.0", "id": self.next, "method": method, "params": params }), &[]).await;
        assert_eq!(s, StatusCode::OK, "{method}: {v}");
        assert_eq!(v["id"], self.next);
        v
    }

    /// A tool call's whole result (`content`, `structuredContent`, `isError`).
    async fn call(&mut self, name: &str, args: Value) -> Value {
        let v = self.rpc("tools/call", json!({ "name": name, "arguments": args })).await;
        assert!(v["error"].is_null(), "{name}: a tool failure is a result, not a JSON-RPC error: {v}");
        v["result"].clone()
    }

    /// A tool's result (`isError` false), structured.
    async fn tool(&mut self, name: &str, args: Value) -> Value {
        let v = self.rpc("tools/call", json!({ "name": name, "arguments": args })).await;
        let r = &v["result"];
        assert_eq!(r["isError"], false, "{name}: {r}");
        assert_eq!(r["content"][0]["type"], "text");
        r["structuredContent"].clone()
    }

    /// A tool's failure: the error text.
    async fn tool_err(&mut self, name: &str, args: Value) -> String {
        let v = self.rpc("tools/call", json!({ "name": name, "arguments": args })).await;
        assert_eq!(v["result"]["isError"], true, "{name}: {v}");
        v["result"]["content"][0]["text"].as_str().unwrap().to_string()
    }
}

#[tokio::test]
async fn transport_sessions_and_protocol() {
    let a = app(None);
    let mut c = Client::new(&a, None);
    // Every request but initialize needs a session.
    let (s, _, v) = c.post(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    c.session = Some("mcp_nope".into());
    let (s, _, _) = c.post(json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }), &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    c.session = None;
    // Version negotiation: a supported version is echoed, an unknown one gets the newest.
    let init = c.initialize("2025-06-18").await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "roll-code");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    assert!(init["result"]["instructions"].as_str().unwrap().contains("claim_work"));
    let mut other = Client::new(&a, None);
    assert_eq!(other.initialize("1999-01-01").await["result"]["protocolVersion"], code_control::mcp::PROTOCOL_VERSIONS[0]);
    // Tools, ping, unknown methods and tools.
    let tools = c.rpc("tools/list", json!({})).await;
    let mut names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    names.sort_unstable();
    let mut documented = TOOLS.to_vec();
    documented.sort_unstable();
    assert_eq!(names, documented, "tools/list is exactly the tools mcp.md documents");
    for t in tools["result"]["tools"].as_array().unwrap() {
        assert_eq!(t["inputSchema"]["additionalProperties"], false, "{}", t["name"]);
        let read_only = READ_ONLY.contains(&t["name"].as_str().unwrap());
        assert_eq!(t["annotations"]["readOnlyHint"], read_only, "{}", t["name"]);
    }
    assert_eq!(c.rpc("ping", json!({})).await["result"], json!({}));
    assert_eq!(c.rpc("nope/method", json!({})).await["error"]["code"], -32601);
    assert_eq!(c.rpc("tools/call", json!({ "name": "rm_rf", "arguments": {} })).await["error"]["code"], -32602);
    assert_eq!(c.rpc("tools/call", json!({ "name": "log", "arguments": "x" })).await["error"]["code"], -32602);
    // Bad JSON and a wrong jsonrpc version.
    let req = Request::builder().method("POST").uri(MCP).header("mcp-session-id", c.session.clone().unwrap()).body(Body::from("{oops")).unwrap();
    let resp = a.send(req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let (_, _, v) = c.post(json!({ "jsonrpc": "1.0", "id": 9, "method": "ping" }), &[]).await;
    assert_eq!(v["error"]["code"], -32600);
    // A batch: answers for requests, nothing for notifications.
    let (s, _, v) = c
        .post(json!([{ "jsonrpc": "2.0", "id": 10, "method": "ping" }, { "jsonrpc": "2.0", "method": "notifications/cancelled" }, { "jsonrpc": "2.0", "id": 11, "method": "ping" }]), &[])
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v.as_array().unwrap().len(), 2);
    // An unsupported protocol version header.
    let (s, _, _) = c.post(json!({ "jsonrpc": "2.0", "id": 12, "method": "ping" }), &[("mcp-protocol-version", "1999-01-01")]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _, _) = c.post(json!({ "jsonrpc": "2.0", "id": 13, "method": "ping" }), &[("mcp-protocol-version", "2025-06-18")]).await;
    assert_eq!(s, StatusCode::OK);
    // Browsers: only roll's own origins.
    let (s, _, _) = c.post(json!({ "jsonrpc": "2.0", "id": 14, "method": "ping" }), &[("origin", "https://evil.example")]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _, _) = c.post(json!({ "jsonrpc": "2.0", "id": 15, "method": "ping" }), &[("origin", "https://roll.sh")]).await;
    assert_eq!(s, StatusCode::OK);
    // No server stream; ending a session.
    let resp = a.send(Request::builder().method("GET").uri(MCP).body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let del = |sid: Option<String>| Request::builder().method("DELETE").uri(MCP).header("mcp-session-id", sid.unwrap_or_default()).body(Body::empty()).unwrap();
    assert_eq!(a.send(del(c.session.clone())).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(a.send(del(c.session.clone())).await.status(), StatusCode::NOT_FOUND);
    let (s, _, _) = c.post(json!({ "jsonrpc": "2.0", "id": 16, "method": "ping" }), &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "an ended session is gone");
}

#[tokio::test]
async fn every_tool_end_to_end() {
    let a = app(None);
    a.repo("acme", "api").await;
    let mut c = Client::new(&a, None);
    c.initialize("2025-11-25").await;
    let repo = "acme/api";

    // Land directly, then read it back every way.
    let landed = c
        .tool("land", json!({ "repo": repo, "message": "first", "files": { "src/lib.rs": { "content": "pub fn a() -> u32 {\n    1\n}\n" }, "README.md": { "content": "# api\n" } } }))
        .await;
    assert_eq!(landed["status"], "landed", "{landed}");
    let first = landed["changeset"].as_str().unwrap().to_string();
    let f = c.tool("read_file", json!({ "repo": repo, "path": "src/lib.rs" })).await;
    assert_eq!(f["content"], "pub fn a() -> u32 {\n    1\n}\n");
    assert_eq!((f["total_lines"].as_u64(), f["truncated"].as_bool()), (Some(3), Some(false)));
    let part = c.tool("read_file", json!({ "repo": repo, "path": "src/lib.rs", "rev": first, "start_line": 2, "end_line": 2 })).await;
    assert_eq!(part["content"], "    1");
    assert!(c.tool_err("read_file", json!({ "repo": repo, "path": "nope.rs" })).await.starts_with("not found"));
    let tree = c.tool("list_tree", json!({ "repo": repo })).await;
    assert!(tree["entries"].as_array().unwrap().iter().any(|e| e["name"] == "src"));
    let sub = c.tool("list_tree", json!({ "repo": repo, "path": "src" })).await;
    assert_eq!(sub["entries"][0]["name"], "lib.rs");
    let log = c.tool("log", json!({ "repo": repo, "limit": 5 })).await;
    assert_eq!(log["items"][0]["id"], first);
    let blame = c.tool("blame", json!({ "repo": repo, "path": "src/lib.rs" })).await;
    assert_eq!(blame["ranges"][0]["changeset"], first);
    assert_eq!(blame["lines"], 3);
    let cs = c.tool("get_changeset", json!({ "repo": repo, "id": first })).await;
    assert_eq!(cs["id"], first);
    let hits = c.tool("search_code", json!({ "repo": repo, "query": "pub fn a" })).await;
    assert_eq!(hits["matches"][0]["path"], "src/lib.rs");

    // A change: create, revise, inspect, compare, comment, review, land.
    let ch = c
        .tool("create_change", json!({ "repo": repo, "title": "Return two", "base": "main", "files": { "src/lib.rs": { "content": "pub fn a() -> u32 {\n    2\n}\n" } } }))
        .await;
    let id = ch["id"].as_str().unwrap().to_string();
    let by_number = format!("{repo}#{}", ch["number"]);
    let revised = c
        .tool("revise_change", json!({ "change": by_number, "base": "main", "message": "two, documented", "files": { "src/lib.rs": { "content": "/// Two.\npub fn a() -> u32 {\n    2\n}\n" } } }))
        .await;
    assert_eq!(revised["revision"], 2);
    let cmp = c.tool("compare", json!({ "repo": repo, "base": "main", "head": revised["head"] })).await;
    assert_eq!(cmp["files"][0]["path"], "src/lib.rs");
    // Searching the change's head finds what only it has.
    let at = c.tool("search_code", json!({ "repo": repo, "query": "/// Two.", "rev": revised["head"] })).await;
    assert_eq!(at["matches"][0]["path"], "src/lib.rs", "{at}");
    let th = c.tool("comment", json!({ "change": id, "path": "src/lib.rs", "line": 1, "body": "Nice doc." })).await;
    let th_id = th["id"].as_str().unwrap().to_string();
    let replied = c.tool("comment", json!({ "thread": th_id, "body": "Thanks!" })).await;
    assert_eq!(replied["comments"].as_array().unwrap().len(), 2);
    let rv = c.tool("review", json!({ "change": id, "verdict": "approve", "body": "LGTM", "comments": [{ "path": "src/lib.rs", "line": 3, "body": "ok" }] })).await;
    assert_eq!(rv["verdict"], "approve");
    let got = c.tool("get_change", json!({ "change": by_number })).await;
    assert_eq!(got["change"]["id"], id);
    assert!(got["review_status"].is_object());
    assert_eq!(got["threads"].as_array().unwrap().len(), 2);
    let list = c.tool("list_changes", json!({ "repo": repo, "state": "open" })).await;
    assert_eq!(list["items"][0]["id"], id);
    let ai = c.tool("request_ai_review", json!({ "change": id, "mode": "standard" })).await;
    assert_eq!(ai["job"]["state"], "queued");
    let landed = c.tool("land", json!({ "change": id })).await;
    assert_eq!(landed["status"], "landed", "{landed}");
    assert_eq!(c.tool("queue_status", json!({ "repo": repo })).await["policy"], "post_merge");

    // A stale base conflicts: an outcome, not an error.
    let conflict = c.tool("land", json!({ "repo": repo, "base": first, "message": "stale", "files": { "src/lib.rs": { "content": "pub fn a() -> u32 {\n    3\n}\n" } } })).await;
    assert_eq!(conflict["status"], "conflict");
    assert_eq!(conflict["paths"][0], "src/lib.rs");
    // Rules that need an approval block a land: also an outcome, with blockers.
    a.ok("PATCH", "/v1/code/repos/acme/api/review-settings", Some(json!({ "required_approvals": 1 }))).await;
    let ch = c.tool("create_change", json!({ "repo": repo, "title": "Needs review", "base": "main", "files": { "x.txt": { "content": "x\n" } } })).await;
    let blocked = c.tool("land", json!({ "change": ch["id"] })).await;
    assert_eq!(blocked["status"], "blocked", "{blocked}");
    assert!(blocked["blockers"].as_array().unwrap().iter().any(|b| b["kind"] == "needs_approval"));

    // Work: nothing to claim, then create, claim, heartbeat, release, claim, complete.
    let none = c.tool("claim_work", json!({ "repo": repo, "agent": "bot-1" })).await;
    assert!(none["work"].is_null());
    let w = c.tool("create_work", json!({ "repo": repo, "title": "Fix the thing", "labels": ["bug"], "priority": 1 })).await;
    let claimed = c.tool("claim_work", json!({ "repo": repo, "agent": "bot-1", "labels": ["bug"], "lease_s": 60 })).await;
    assert_eq!(claimed["work"]["id"], w["id"]);
    let wnum = format!("{repo}#{}", w["number"]);
    assert_eq!(c.tool("heartbeat_work", json!({ "work": wnum, "agent": "bot-1" })).await["claimed_by"], "bot-1");
    assert_eq!(c.tool("release_work", json!({ "work": w["id"], "agent": "bot-1" })).await["state"], "open");
    c.tool("claim_work", json!({ "repo": repo, "agent": "bot-2" })).await;
    assert_eq!(c.tool("complete_work", json!({ "work": w["id"], "agent": "bot-2" })).await["state"], "done");
    let work = c.tool("list_work", json!({ "repo": repo, "state": "done" })).await;
    assert_eq!(work["items"][0]["id"], w["id"]);

    // Argument errors are tool errors.
    assert!(c.tool_err("log", json!({ "repo": "no-slash" })).await.contains("owner/repo"));
    assert!(c.tool_err("get_change", json!({ "change": "acme/api#999" })).await.starts_with("not found"));
    assert!(c.tool_err("land", json!({})).await.contains("land needs"));
    assert!(c.tool_err("comment", json!({ "body": "x" })).await.contains("comment needs"));
}

#[tokio::test]
async fn tools_enforce_the_routes_authorization() {
    let a = common::app(Some("root"));
    for (name, vis) in [("pub", "public"), ("priv", "private")] {
        a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), Some("root")).await;
        let (s, v) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": name, "visibility": vis })), Some("root")).await;
        assert!(s.is_success(), "{v}");
        let (s, v) = a.req("POST", &format!("/v1/code/repos/acme/{name}/land"), Some(json!({ "message": "m", "files": { "a.txt": { "content": "a\n" } } })), Some("root")).await;
        assert!(s.is_success(), "{v}");
    }
    let (_, v) = a.req("POST", "/v1/code/owners", Some(json!({ "name": "eve", "kind": "user" })), Some("root")).await;
    assert!(v.is_object());
    let (_, t) = a.req("POST", "/v1/code/owners/eve/tokens", Some(json!({ "name": "t" })), Some("root")).await;
    let eve = t["token"].as_str().unwrap().to_string();

    // Anonymous: public reads work; private repos don't exist; writes need a token.
    let mut anon = Client::new(&a, None);
    anon.initialize("2025-06-18").await;
    assert_eq!(anon.tool("read_file", json!({ "repo": "acme/pub", "path": "a.txt" })).await["content"], "a\n");
    assert!(anon.tool_err("read_file", json!({ "repo": "acme/priv", "path": "a.txt" })).await.starts_with("not found"));
    assert!(anon.tool_err("create_work", json!({ "repo": "acme/pub", "title": "x" })).await.contains("needs a token"));
    // Another owner's token: reads public, can't write acme's repos.
    let mut e = Client::new(&a, Some(&eve));
    e.initialize("2025-06-18").await;
    assert!(e.tool_err("list_tree", json!({ "repo": "acme/priv" })).await.starts_with("not found"));
    let err = e.tool_err("land", json!({ "repo": "acme/pub", "base": "main", "message": "x", "files": { "b.txt": { "content": "b\n" } } })).await;
    assert!(err.starts_with("not allowed"), "{err}");
    // The admin can.
    let mut root = Client::new(&a, Some("root"));
    root.initialize("2025-06-18").await;
    assert_eq!(root.tool("read_file", json!({ "repo": "acme/priv", "path": "a.txt" })).await["content"], "a\n");
    // A session belongs to the token that started it.
    let mut stolen = Client::new(&a, Some(&eve));
    stolen.session = root.session.clone();
    let (s, _, _) = stolen.post(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }), &[]).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

/// `Accept: text/event-stream` (without `application/json`): each response comes as a `message`
/// event on an event stream; an initialize still answers in JSON (its session id is a header),
/// and notifications alone still get 202.
#[tokio::test]
async fn responses_as_server_sent_events() {
    let a = app(None);
    a.repo("acme", "api").await;
    a.ok("POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "init", "files": { "a.txt": { "content": "hello\n" } } }))).await;
    let mut c = Client::new(&a, None);
    c.initialize("2025-06-18").await;
    let sse = |body: Value| {
        Request::builder()
            .method("POST")
            .uri(MCP)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("mcp-session-id", c.session.clone().unwrap())
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    // One request.
    let resp = a.send(sse(json!({ "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": "read_file", "arguments": { "repo": "acme/api", "path": "a.txt" } } }))).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("text/event-stream"));
    let text = String::from_utf8(axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap().to_vec()).unwrap();
    let events: Vec<Value> = text
        .split("\n\n")
        .filter(|e| e.lines().any(|l| l == "event: message"))
        .map(|e| serde_json::from_str(e.lines().find_map(|l| l.strip_prefix("data: ")).unwrap()).unwrap())
        .collect();
    assert_eq!(events.len(), 1, "{text}");
    assert_eq!(events[0]["id"], 7);
    assert!(events[0]["result"]["content"][0]["text"].as_str().unwrap().contains("hello"), "{}", events[0]);
    // A batch: one event per response.
    let resp = a
        .send(sse(json!([
            { "jsonrpc": "2.0", "id": 1, "method": "ping" },
            { "jsonrpc": "2.0", "method": "notifications/cancelled", "params": {} },
            { "jsonrpc": "2.0", "id": 2, "method": "tools/list" }
        ])))
        .await;
    let text = String::from_utf8(axum::body::to_bytes(resp.into_body(), 4 << 20).await.unwrap().to_vec()).unwrap();
    let ids: Vec<Value> = text
        .split("\n\n")
        .filter_map(|e| e.lines().find_map(|l| l.strip_prefix("data: ")))
        .map(|d| serde_json::from_str::<Value>(d).unwrap()["id"].clone())
        .collect();
    assert_eq!(ids, vec![json!(1), json!(2)]);
    // Only notifications: 202, no stream.
    let resp = a.send(sse(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))).await;
    assert_eq!(resp.status(), StatusCode::ACCEPTED);
    // Without the session, the same 400 as JSON.
    let resp = a
        .send(
            Request::builder()
                .method("POST")
                .uri(MCP)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .body(Body::from(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Initialize asked for as a stream still answers in JSON with the session header.
    let resp = a
        .send(
            Request::builder()
                .method("POST")
                .uri(MCP)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .body(Body::from(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } }).to_string()))
                .unwrap(),
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().contains_key("mcp-session-id"));
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("application/json"));
}

fn init_body(version: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": version, "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } } })
}

fn ping(id: u64) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "ping" })
}

/// The transport details `transport_sessions_and_protocol` doesn't reach: every version, batches
/// that can't be answered, client responses, the other list methods, Origin edge cases, and
/// tools/call without arguments or a name.
#[tokio::test]
async fn protocol_edges() {
    let a = app(None);
    // Every supported version is answered with itself; none asked means the newest.
    for v in code_control::mcp::PROTOCOL_VERSIONS {
        let mut c = Client::new(&a, None);
        assert_eq!(c.initialize(v).await["result"]["protocolVersion"], v);
    }
    let mut c = Client::new(&a, None);
    let (s, h, v) = c.post(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize" }), &[]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["result"]["protocolVersion"], code_control::mcp::PROTOCOL_VERSIONS[0]);
    c.session = Some(h["mcp-session-id"].to_str().unwrap().to_string());

    // initialize goes on its own; an empty batch is invalid.
    let fresh = Client::new(&a, None);
    let (s, h, _) = fresh.post(json!([init_body("2025-06-18"), ping(1)]), &[]).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!h.contains_key("mcp-session-id"));
    let (s, _, v) = c.post(json!([]), &[]).await;
    assert_eq!((s, v["error"]["code"].as_i64()), (StatusCode::BAD_REQUEST, Some(-32600)));
    // A response from the client, or a batch of notifications only: 202 and no body.
    let (s, _, v) = c.post(json!({ "jsonrpc": "2.0", "id": 5, "result": {} }), &[]).await;
    assert_eq!((s, v), (StatusCode::ACCEPTED, Value::Null));
    let (s, _, v) = c
        .post(json!([{ "jsonrpc": "2.0", "method": "notifications/initialized" }, { "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1 } }]), &[])
        .await;
    assert_eq!((s, v), (StatusCode::ACCEPTED, Value::Null));
    // In a batch, a bad entry gets its own error and the rest are answered.
    let (s, _, v) = c.post(json!([{ "jsonrpc": "1.0", "id": 1, "method": "ping" }, ping(2)]), &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((v[0]["id"].as_i64(), v[0]["error"]["code"].as_i64()), (Some(1), Some(-32600)));
    assert_eq!((v[1]["id"].as_i64(), &v[1]["result"]), (Some(2), &json!({})));
    // No resources or prompts, but the methods answer.
    assert_eq!(c.rpc("resources/list", json!({})).await["result"]["resources"], json!([]));
    assert_eq!(c.rpc("prompts/list", json!({})).await["result"]["prompts"], json!([]));
    // tools/call: no arguments means {} (the tool's own error); no name is an unknown tool.
    let v = c.rpc("tools/call", json!({ "name": "queue_status" })).await;
    assert_eq!(v["result"]["isError"], true);
    assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("repo is required"), "{v}");
    assert_eq!(c.rpc("tools/call", json!({})).await["error"]["code"], -32602);
    assert_eq!(c.rpc("tools/call", json!({ "name": "log", "arguments": [1] })).await["error"]["code"], -32602);
    // Another supported protocol version than the session's is fine.
    let (s, _, _) = c.post(ping(3), &[("mcp-protocol-version", "2024-11-05")]).await;
    assert_eq!(s, StatusCode::OK);

    // Origin: the API's own origin is allowed; another scheme, a lookalike host, another port,
    // `null` and garbage are not, and neither is initialize from one.
    let (s, _, _) = c.post(ping(4), &[("origin", "https://api.roll.sh")]).await;
    assert_eq!(s, StatusCode::OK);
    for bad in ["http://roll.sh", "https://roll.sh.evil.example", "https://roll.sh:8443", "null", "not a url"] {
        let (s, _, _) = c.post(ping(5), &[("origin", bad)]).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{bad}");
    }
    let (s, h, _) = fresh.post(init_body("2025-06-18"), &[("origin", "https://evil.example")]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(!h.contains_key("mcp-session-id"));

    // GET says what's allowed; DELETE needs a session id.
    let resp = a.send(Request::builder().method("GET").uri(MCP).body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.headers()["allow"], "POST, DELETE");
    let resp = a.send(Request::builder().method("DELETE").uri(MCP).body(Body::empty()).unwrap()).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

/// A token that was never valid gets 401 on initialize and ends a session it's used on; a
/// session can't be used or ended by another caller (anonymous or another token).
#[tokio::test]
async fn invalid_tokens_and_foreign_sessions() {
    let a = app(Some("root"));
    let bad = Client::new(&a, Some("rlc_never_issued"));
    let (s, h, v) = bad.post(init_body("2025-06-18"), &[]).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "{v}");
    assert!(v["error"]["message"].as_str().unwrap().contains("isn't valid"), "{v}");
    assert!(!h.contains_key("mcp-session-id"));

    // A garbage token on a live session: 401, and the session is gone for its owner too.
    let mut root = Client::new(&a, Some("root"));
    root.initialize("2025-06-18").await;
    let mut thief = Client::new(&a, Some("rlc_never_issued"));
    thief.session = root.session.clone();
    assert_eq!(thief.post(ping(1), &[]).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(root.post(ping(2), &[]).await.0, StatusCode::NOT_FOUND, "the session ended");

    // Without the token the session started with: 404, and the session lives on.
    let mut root = Client::new(&a, Some("root"));
    root.initialize("2025-06-18").await;
    let mut anon = Client::new(&a, None);
    anon.session = root.session.clone();
    assert_eq!(anon.post(ping(3), &[]).await.0, StatusCode::NOT_FOUND);
    assert_eq!(root.post(ping(4), &[]).await.0, StatusCode::OK);
    // DELETE from someone else: 404, still alive; from its token: 204.
    let del = |token: Option<&str>| {
        let mut b = Request::builder().method("DELETE").uri(MCP).header("mcp-session-id", root.session.clone().unwrap());
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(Body::empty()).unwrap()
    };
    assert_eq!(a.send(del(None)).await.status(), StatusCode::NOT_FOUND);
    assert_eq!(root.post(ping(5), &[]).await.0, StatusCode::OK);
    assert_eq!(a.send(del(Some("root"))).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(root.post(ping(6), &[]).await.0, StatusCode::NOT_FOUND);
}

/// `land` on a branch (`ref`, read back by that rev), by number, landing twice (an error, not
/// a conflict), unknown and malformed changes, and on a queue repo: `queued` with its
/// position, the urgent lane, a direct land queued too, a bad lane.
#[tokio::test]
async fn land_outcomes_queued_branches_and_errors() {
    let a = app(None);
    a.repo("acme", "api").await;
    let mut c = Client::new(&a, None);
    c.initialize("2025-11-25").await;
    let repo = "acme/api";
    assert_eq!(c.tool("land", json!({ "repo": repo, "message": "init", "files": { "a.txt": { "content": "a\n" } } })).await["status"], "landed");

    // A branch with a slash in its name.
    let br = c.tool("land", json!({ "repo": repo, "base": "main", "message": "on a branch", "ref": "feature/x", "files": { "b.txt": { "content": "b\n" } } })).await;
    assert_eq!(br["status"], "landed", "{br}");
    assert_eq!(br["ref"], "feature/x", "{br}");
    assert_eq!(c.tool("read_file", json!({ "repo": repo, "path": "b.txt", "rev": "feature/x" })).await["content"], "b\n");
    assert!(c.tool_err("read_file", json!({ "repo": repo, "path": "b.txt" })).await.starts_with("not found"), "not on main");
    let tree = c.tool("list_tree", json!({ "repo": repo, "rev": "feature/x" })).await;
    assert!(tree["entries"].as_array().unwrap().iter().any(|e| e["name"] == "b.txt"), "{tree}");
    assert_eq!(c.tool("log", json!({ "repo": repo, "rev": "feature/x", "limit": 1 })).await["items"][0]["message"], "on a branch");
    assert_eq!(c.tool("blame", json!({ "repo": repo, "path": "b.txt", "rev": "feature/x" })).await["lines"], 1);

    // By number; landing again is an error (409 without paths), not a conflict.
    let ch = c.tool("create_change", json!({ "repo": repo, "title": "c", "base": "main", "files": { "c.txt": { "content": "c\n" } } })).await;
    let num = format!("{repo}#{}", ch["number"]);
    assert_eq!(c.tool("land", json!({ "change": num })).await["status"], "landed");
    let again = c.call("land", json!({ "change": num })).await;
    assert_eq!(again["isError"], true, "{again}");
    assert_eq!(again["structuredContent"]["http_status"], 409, "{again}");
    assert!(again["structuredContent"].get("paths").is_none());
    assert!(c.tool_err("land", json!({ "change": "chg_nope" })).await.starts_with("not found"));
    assert!(c.tool_err("land", json!({ "change": "acme/api#x" })).await.contains("expected owner/repo#number"));
    assert!(c.tool_err("land", json!({ "change": "acme#1" })).await.contains("expected \"owner/repo\""));

    // A queue repo: landing queues.
    a.ok("PATCH", "/v1/code/repos/acme/api", Some(json!({ "land_policy": "queue" }))).await;
    let one = c.tool("create_change", json!({ "repo": repo, "title": "one", "base": "main", "files": { "1.txt": { "content": "1\n" } } })).await;
    let q = c.tool("land", json!({ "change": one["id"] })).await;
    assert_eq!((q["status"].as_str(), q["position"].as_i64()), (Some("queued"), Some(1)), "{q}");
    assert_eq!(q["change"]["queue"]["lane"], "normal");
    let two = c.tool("create_change", json!({ "repo": repo, "title": "two", "base": "main", "files": { "2.txt": { "content": "2\n" } } })).await;
    let q = c.tool("land", json!({ "change": two["id"], "lane": "urgent" })).await;
    assert_eq!((q["status"].as_str(), q["position"].as_i64()), (Some("queued"), Some(1)), "urgent goes first: {q}");
    assert_eq!(q["change"]["queue"]["lane"], "urgent");
    let direct = c.tool("land", json!({ "repo": repo, "base": "main", "message": "direct", "files": { "3.txt": { "content": "3\n" } } })).await;
    assert_eq!((direct["status"].as_str(), direct["position"].as_i64()), (Some("queued"), Some(3)), "{direct}");
    let qs = c.tool("queue_status", json!({ "repo": repo })).await;
    assert_eq!(qs["policy"], "queue");
    assert_eq!(qs["stats"]["depth"], 3, "{qs}");
    assert_eq!(qs["entries"][0]["number"], two["number"]);
    assert_eq!(c.tool("list_changes", json!({ "repo": repo, "state": "queued" })).await["items"].as_array().unwrap().len(), 3);
    let three = c.tool("create_change", json!({ "repo": repo, "title": "three", "base": "main", "files": { "4.txt": { "content": "4\n" } } })).await;
    let bad = c.call("land", json!({ "change": three["id"], "lane": "sideways" })).await;
    assert_eq!((bad["isError"].as_bool(), bad["structuredContent"]["http_status"].as_i64()), (Some(true), Some(400)), "{bad}");
}

/// `read_file` past 2,000 lines comes back in ranges with a pointer to the next one; a binary
/// file comes back as metadata; the text block is the file itself.
#[tokio::test]
async fn read_file_ranges_and_binary_files() {
    let a = app(None);
    a.repo("acme", "api").await;
    let big: String = (1..=4500).map(|i| format!("line {i}\n")).collect();
    a.ok("POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "m", "files": { "big.txt": { "content": big }, "logo.bin": { "content_b64": "AP8AAQ==" }, "small.txt": { "content": "hi\n" } } })))
        .await;
    let mut c = Client::new(&a, None);
    c.initialize("2025-11-25").await;
    let repo = "acme/api";

    let r = c.call("read_file", json!({ "repo": repo, "path": "big.txt" })).await;
    let f = &r["structuredContent"];
    assert_eq!((f["start_line"].as_u64(), f["end_line"].as_u64(), f["total_lines"].as_u64()), (Some(1), Some(2000), Some(4500)));
    assert_eq!((f["truncated"].as_bool(), f["next"].as_str()), (Some(true), Some("more: start_line=2001")));
    assert_eq!(f["content"].as_str().unwrap().lines().count(), 2000);
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("big.txt lines 1-2000 of 4500 (pass start_line=2001 for more)\nline 1\n"), "{}", &text[..80]);
    let f = c.tool("read_file", json!({ "repo": repo, "path": "big.txt", "start_line": 2001 })).await;
    assert_eq!((f["end_line"].as_u64(), f["truncated"].as_bool()), (Some(4000), Some(true)));
    assert!(f["content"].as_str().unwrap().starts_with("line 2001\n"));
    let f = c.tool("read_file", json!({ "repo": repo, "path": "big.txt", "start_line": 4001 })).await;
    assert_eq!((f["end_line"].as_u64(), f["truncated"].as_bool()), (Some(4500), Some(false)));
    assert!(f.get("next").is_none());
    assert!(f["content"].as_str().unwrap().ends_with("line 4500"));
    let f = c.tool("read_file", json!({ "repo": repo, "path": "big.txt", "start_line": 10, "end_line": 12 })).await;
    assert_eq!(f["content"], "line 10\nline 11\nline 12");
    let f = c.tool("read_file", json!({ "repo": repo, "path": "big.txt", "start_line": 9000 })).await;
    assert_eq!((f["content"].as_str(), f["truncated"].as_bool()), (Some(""), Some(false)), "past the end: nothing");

    let r = c.call("read_file", json!({ "repo": repo, "path": "logo.bin" })).await;
    assert_eq!(r["isError"], false);
    let b = &r["structuredContent"];
    assert_eq!((b["binary"].as_bool(), b["size"].as_u64()), (Some(true), Some(4)), "{b}");
    assert!(b["content"].is_null());

    let r = c.call("read_file", json!({ "repo": repo, "path": "small.txt" })).await;
    assert_eq!(r["content"][0]["text"], "hi\n", "the text block is the file content");
    assert_eq!(r["structuredContent"]["binary"], false);
}

/// Each read tool answers exactly what its route does (arrays wrapped as `{items}`), with its
/// arguments mapped onto the route's query; each write tool's effect is what the route then
/// shows. `X-Roll-Agent` and `X-Roll-User` reach the routes too.
#[tokio::test]
async fn tools_map_onto_their_routes() {
    let a = app(None);
    a.repo("acme", "api").await;
    let p = "/v1/code/repos/acme/api";
    let first = a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "first", "files": { "src/lib.rs": { "content": "fn a() {}\nfn b() {}\n" }, "README.md": { "content": "# api\n" } } }))).await;
    let second = a.ok("POST", &format!("{p}/land"), Some(json!({ "message": "second", "files": { "src/lib.rs": { "content": "fn a() {}\nfn b() {}\nfn c() {}\n" } } }))).await;
    let (cs1, cs2) = (first["changeset"].as_str().unwrap().to_string(), second["changeset"].as_str().unwrap().to_string());
    let mut c = Client::new(&a, None);
    c.initialize("2025-11-25").await;
    let repo = "acme/api";

    // Write tools, checked against what the routes then show.
    let w = c.tool("create_work", json!({ "repo": repo, "title": "Add d", "body": "please", "labels": ["feat"], "priority": 2 })).await;
    let wid = w["id"].as_str().unwrap().to_string();
    assert_eq!(a.ok("GET", &format!("/v1/code/work/{wid}"), None).await, w);
    let ch = c
        .tool(
            "create_change",
            json!({ "repo": repo, "title": "Add d", "body": "why", "base": "main", "draft": true, "work_item": format!("{repo}#{}", w["number"]), "files": { "src/lib.rs": { "content": "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n" } } }),
        )
        .await;
    let id = ch["id"].as_str().unwrap().to_string();
    let got = a.ok("GET", &format!("/v1/code/changes/{id}"), None).await;
    assert_eq!((got["title"].as_str(), got["draft"].as_bool(), got["work_item"].as_str()), (Some("Add d"), Some(true), Some(wid.as_str())), "{got}");
    let head = a.ok("GET", &format!("{p}/changesets/{}", got["head"].as_str().unwrap()), None).await;
    assert_eq!(head["message"], "Add d", "message defaults to the title");
    // work_item also takes the item's number, as a number or a string.
    let by_n = c.tool("create_change", json!({ "repo": repo, "title": "also d", "base": "main", "work_item": w["number"], "files": { "d.txt": { "content": "d\n" } } })).await;
    assert_eq!(by_n["work_item"], wid.as_str(), "{by_n}");
    let by_s = c.tool("create_change", json!({ "repo": repo, "title": "d again", "base": "main", "work_item": w["number"].to_string(), "files": { "e.txt": { "content": "e\n" } } })).await;
    assert_eq!(by_s["work_item"], wid.as_str(), "{by_s}");
    let e = c.tool_err("create_change", json!({ "repo": repo, "title": "x", "base": "main", "work_item": "acme/api#99", "files": { "x": { "content": "x" } } })).await;
    assert!(e.starts_with("not found"), "{e}");
    let rv = c.tool("revise_change", json!({ "change": id, "base": got["head"], "files": { "src/lib.rs": { "content": "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n" } }, "message": "and e" })).await;
    assert_eq!(rv["revision"], 2);
    assert_eq!(a.ok("GET", &format!("/v1/code/changes/{id}"), None).await["revision"], 2);
    let th = c.tool("comment", json!({ "change": id, "path": "src/lib.rs", "line": 5, "start_line": 4, "side": "new", "body": "two at once?" })).await;
    let th_id = th["id"].as_str().unwrap().to_string();
    assert_eq!((th["start_line"].as_u64(), th["line"].as_u64(), th["side"].as_str()), (Some(4), Some(5), Some("new")));
    let old = c.tool("comment", json!({ "change": id, "path": "src/lib.rs", "line": 1, "side": "old", "body": "on the old side" })).await;
    assert_eq!(old["side"], "old");
    let replied = c.tool("comment", json!({ "thread": th_id, "body": "yes" })).await;
    assert_eq!(a.ok("GET", &format!("/v1/code/threads/{th_id}"), None).await, replied);
    let review = c.tool("review", json!({ "change": id, "verdict": "request_changes", "body": "not yet" })).await;
    let reviews = a.ok("GET", &format!("/v1/code/changes/{id}/reviews"), None).await;
    assert!(reviews.as_array().unwrap().iter().any(|r| r["id"] == review["id"] && r["verdict"] == "request_changes"), "{reviews}");
    let ai = c.tool("request_ai_review", json!({ "change": id, "mode": "deep", "full": true })).await;
    let jobs = a.ok("GET", &format!("/v1/code/changes/{id}/ai-jobs"), None).await;
    assert!(jobs.as_array().unwrap().iter().any(|j| j["id"] == ai["job"]["id"]), "{ai} {jobs}");

    // A change for a work item puts the item in review, as the route does.
    assert_eq!(a.ok("GET", &format!("/v1/code/work/{wid}"), None).await["state"], "review");

    // Work leases, by number and by id.
    let w = c.tool("create_work", json!({ "repo": repo, "title": "Add f", "labels": ["feat"] })).await;
    let wid = w["id"].as_str().unwrap().to_string();
    let wn = format!("{repo}#{}", w["number"]);
    let claimed = c.tool("claim_work", json!({ "repo": repo, "agent": "bot", "labels": ["feat"], "lease_s": 120 })).await;
    assert_eq!(claimed["work"]["id"], wid.as_str());
    let hb = c.tool("heartbeat_work", json!({ "work": wn, "agent": "bot" })).await;
    assert_eq!(a.ok("GET", &format!("/v1/code/work/{wid}"), None).await["lease_until"], hb["lease_until"]);
    let other = c.call("heartbeat_work", json!({ "work": wid, "agent": "someone-else" })).await;
    assert_eq!(other["isError"], true, "another agent's lease: {other}");
    assert_eq!(c.tool("release_work", json!({ "work": wn, "agent": "bot" })).await["state"], "open");
    c.tool("claim_work", json!({ "repo": repo, "agent": "bot" })).await;
    assert_eq!(c.tool("complete_work", json!({ "work": wn, "agent": "bot" })).await["state"], "done");
    assert_eq!(a.ok("GET", &format!("/v1/code/work/{wid}"), None).await["state"], "done");
    assert!(c.tool("claim_work", json!({ "repo": repo, "agent": "bot", "labels": ["feat"] })).await["work"].is_null(), "nothing left with that label");

    // Read tools: the same JSON as their routes.
    let same = |tool: Value, route: Value| {
        let route = if route.is_array() { json!({ "items": route }) } else { route };
        assert_eq!(tool, route);
    };
    same(
        c.tool("search_code", json!({ "repo": repo, "query": "fn [a-c]", "regex": true, "case_sensitive": true, "path": "src/**", "limit": 2 })).await,
        a.ok("GET", &format!("{p}/search?q=fn+%5Ba-c%5D&regex=true&case=true&path=src%2F**&limit=2"), None).await,
    );
    let at_head = c.tool("search_code", json!({ "repo": repo, "query": "fn e", "rev": rv["head"] })).await;
    assert_eq!(at_head["matches"][0]["path"], "src/lib.rs", "{at_head}");
    assert!(c.tool("search_code", json!({ "repo": repo, "query": "FN A", "case_sensitive": true })).await["matches"].as_array().unwrap().is_empty());
    same(c.tool("list_tree", json!({ "repo": repo })).await, a.ok("GET", &format!("{p}/tree/main/"), None).await);
    same(c.tool("list_tree", json!({ "repo": repo, "path": "src", "rev": cs1 })).await, a.ok("GET", &format!("{p}/tree/{cs1}/src"), None).await);
    let page = c.tool("log", json!({ "repo": repo, "limit": 1 })).await;
    same(page.clone(), a.ok("GET", &format!("{p}/log?limit=1"), None).await);
    assert_eq!(page["items"][0]["id"], cs2.as_str());
    let next = c.tool("log", json!({ "repo": repo, "limit": 1, "cursor": page["cursor"] })).await;
    assert_eq!(next["items"][0]["id"], cs1.as_str(), "the cursor continues: {next}");
    same(c.tool("log", json!({ "repo": repo, "path": "README.md" })).await, a.ok("GET", &format!("{p}/log?path=README.md&limit=30"), None).await);
    same(c.tool("blame", json!({ "repo": repo, "path": "src/lib.rs" })).await, a.ok("GET", &format!("{p}/blame/main/src/lib.rs"), None).await);
    same(c.tool("get_changeset", json!({ "repo": repo, "id": cs2 })).await, a.ok("GET", &format!("{p}/changesets/{cs2}"), None).await);
    same(c.tool("compare", json!({ "repo": repo, "base": cs1, "head": cs2 })).await, a.ok("GET", &format!("{p}/compare/{cs1}...{cs2}"), None).await);
    let gc = c.tool("get_change", json!({ "change": id })).await;
    assert_eq!(gc["change"], a.ok("GET", &format!("/v1/code/changes/{id}"), None).await);
    assert_eq!(gc["review_status"], a.ok("GET", &format!("/v1/code/changes/{id}/review-status"), None).await);
    assert_eq!(gc["threads"], a.ok("GET", &format!("/v1/code/changes/{id}/threads"), None).await);
    same(c.tool("list_changes", json!({ "repo": repo })).await, a.ok("GET", &format!("{p}/changes"), None).await);
    same(c.tool("list_changes", json!({ "repo": repo, "state": "closed" })).await, a.ok("GET", &format!("{p}/changes?state=closed"), None).await);
    same(c.tool("list_work", json!({ "repo": repo, "state": "done", "label": "feat" })).await, a.ok("GET", &format!("{p}/work?state=done&label=feat"), None).await);
    assert!(c.tool("list_work", json!({ "repo": repo, "label": "nope" })).await["items"].as_array().unwrap().is_empty());
    same(c.tool("queue_status", json!({ "repo": repo })).await, a.ok("GET", &format!("{p}/queue"), None).await);
    let raw = a.raw("GET", &format!("{p}/blob/main/src/lib.rs"), None, None).await.1;
    assert_eq!(c.tool("read_file", json!({ "repo": repo, "path": "src/lib.rs" })).await["content"], String::from_utf8(raw).unwrap());

    // The identity headers reach the routes.
    let mut bot = Client::new(&a, None);
    bot.headers.push(("x-roll-agent", "fixer-7".into()));
    bot.initialize("2025-06-18").await;
    let r = bot.tool("review", json!({ "change": id, "verdict": "comment", "body": "hm", "comments": [{ "path": "src/lib.rs", "line": 2, "body": "b?" }] })).await;
    assert_eq!((r["reviewer"]["kind"].as_str(), r["reviewer"]["name"].as_str()), (Some("agent"), Some("fixer-7")), "{r}");
    let t = bot.tool("comment", json!({ "thread": th_id, "body": "from the bot" })).await;
    assert_eq!(t["comments"].as_array().unwrap().last().unwrap()["author"]["name"], "fixer-7");
    let mut ada = Client::new(&a, None);
    ada.headers.push(("x-roll-user", "ada".into()));
    ada.initialize("2025-06-18").await;
    let r = ada.tool("review", json!({ "change": id, "verdict": "approve" })).await;
    assert_eq!((r["reviewer"]["kind"].as_str(), r["reviewer"]["name"].as_str()), (Some("user"), Some("ada")), "{r}");
}

/// Every tool's argument errors: each required argument missing, ill-typed values, and
/// malformed or unknown repos, changes and work items. All are tool results with
/// `isError: true`, never JSON-RPC errors.
#[tokio::test]
async fn tool_argument_errors() {
    let a = app(None);
    a.repo("acme", "api").await;
    let landed = a.ok("POST", "/v1/code/repos/acme/api/land", Some(json!({ "message": "m", "files": { "a.txt": { "content": "a\n" } } }))).await;
    let cs = landed["changeset"].as_str().unwrap().to_string();
    let ch = a.ok("POST", "/v1/code/repos/acme/api/changes", Some(json!({ "title": "t", "base": "main", "message": "t", "files": { "b.txt": { "content": "b\n" } } }))).await;
    let id = ch["id"].as_str().unwrap().to_string();
    let mut c = Client::new(&a, None);
    c.initialize("2025-11-25").await;
    let files = json!({ "x.txt": { "content": "x\n" } });

    // Each tool with valid arguments for everything but its repo.
    let repo_tools: Vec<(&str, Value)> = vec![
        ("search_code", json!({ "query": "a" })),
        ("read_file", json!({ "path": "a.txt" })),
        ("list_tree", json!({})),
        ("log", json!({})),
        ("blame", json!({ "path": "a.txt" })),
        ("get_changeset", json!({ "id": cs })),
        ("compare", json!({ "base": "main", "head": "main" })),
        ("create_change", json!({ "title": "t", "base": "main", "files": files })),
        ("list_changes", json!({})),
        ("claim_work", json!({ "agent": "bot" })),
        ("list_work", json!({})),
        ("create_work", json!({ "title": "t" })),
        ("queue_status", json!({})),
    ];
    for (tool, args) in &repo_tools {
        let with = |repo: Value| {
            let mut v = args.clone();
            v["repo"] = repo;
            v
        };
        assert!(c.tool_err(tool, args.clone()).await.contains("repo is required"), "{tool}");
        assert!(c.tool_err(tool, with(json!(5))).await.contains("repo is required"), "{tool}: not a string");
        assert!(c.tool_err(tool, with(json!("  "))).await.contains("repo is required"), "{tool}: blank");
        for bad in ["acme", "a/b/c", "/api", "acme/"] {
            let e = c.tool_err(tool, with(json!(bad))).await;
            assert!(e.contains("expected \"owner/repo\""), "{tool} {bad}: {e}");
        }
        let e = c.tool_err(tool, with(json!("acme/nope"))).await;
        assert!(e.starts_with("not found"), "{tool}: {e}");
        // `.git` is accepted, as in clone URLs.
        let ok = c.call(tool, with(json!("acme/api.git"))).await;
        assert_eq!(ok["isError"], false, "{tool}: {ok}");
    }
    // A direct land without a usable repo says what land needs.
    for bad in [json!({}), json!({ "repo": "acme" }), json!({ "base": "main", "message": "m", "files": files })] {
        assert!(c.tool_err("land", bad).await.contains("land needs a change, or a repo"));
    }

    // Each tool with valid arguments for everything but its change.
    let change_tools: Vec<(&str, Value, &str)> = vec![
        ("land", json!({}), "land needs a change, or a repo"),
        ("revise_change", json!({ "base": "main", "files": files }), "change is required"),
        ("get_change", json!({}), "change is required"),
        ("review", json!({ "verdict": "comment" }), "change is required"),
        ("request_ai_review", json!({}), "change is required"),
        ("comment", json!({ "path": "b.txt", "line": 1, "body": "x" }), "comment needs a thread to reply in, or a change, path and line"),
    ];
    for (tool, args, missing) in &change_tools {
        let with = |change: &str| {
            let mut v = args.clone();
            v["change"] = json!(change);
            v
        };
        assert!(c.tool_err(tool, args.clone()).await.contains(missing), "{tool}");
        assert!(c.tool_err(tool, with("acme/api#x")).await.contains("expected owner/repo#number"), "{tool}");
        assert!(c.tool_err(tool, with("acme#1")).await.contains("expected \"owner/repo\""), "{tool}");
        for unknown in ["acme/api#999", "chg_nope", "acme/nope#1"] {
            let e = c.tool_err(tool, with(unknown)).await;
            assert!(e.starts_with("not found"), "{tool} {unknown}: {e}");
        }
    }
    for tool in ["heartbeat_work", "complete_work", "release_work"] {
        assert!(c.tool_err(tool, json!({ "agent": "bot" })).await.contains("work is required"), "{tool}");
        assert!(c.tool_err(tool, json!({ "work": "acme/api#x" })).await.contains("expected owner/repo#number"), "{tool}");
        for unknown in ["acme/api#999", "wi_nope"] {
            assert!(c.tool_err(tool, json!({ "work": unknown, "agent": "bot" })).await.starts_with("not found"), "{tool} {unknown}");
        }
    }

    // The other required arguments.
    let r = "acme/api";
    for (tool, args, want) in [
        ("search_code", json!({ "repo": r }), "query is required"),
        ("read_file", json!({ "repo": r }), "path is required"),
        ("blame", json!({ "repo": r }), "path is required"),
        ("get_changeset", json!({ "repo": r }), "id is required"),
        ("compare", json!({ "repo": r, "head": "main" }), "base is required"),
        ("compare", json!({ "repo": r, "base": "main" }), "head is required"),
        ("create_change", json!({ "repo": r, "base": "main", "files": files }), "title is required"),
    ] {
        let e = c.tool_err(tool, args).await;
        assert!(e.contains(want), "{tool}: {e}");
    }
    // Missing or ill-typed arguments the route refuses: its 400 (or 422) and message.
    for (tool, args) in [
        ("create_change", json!({ "repo": r, "title": "t", "base": "main", "files": "x.txt" })),
        ("create_change", json!({ "repo": r, "title": "t", "base": "main", "files": { "x": { "content": 5 } } })),
        ("create_change", json!({ "repo": r, "title": "t", "base": "main", "files": { "x": { "content": "x", "mode": "777" } } })),
        ("revise_change", json!({ "change": id, "base": "main", "files": [] })),
        ("land", json!({ "change": id, "bypass": "yes" })),
        ("search_code", json!({ "repo": r, "query": "(", "regex": true })),
        ("search_code", json!({ "repo": r, "query": "a", "limit": "lots" })),
        ("log", json!({ "repo": r, "limit": "lots" })),
        ("list_changes", json!({ "repo": r, "state": "sideways" })),
        ("list_work", json!({ "repo": r, "state": "sideways" })),
        ("claim_work", json!({ "repo": r })),
        ("claim_work", json!({ "repo": r, "agent": "bot", "labels": "bug" })),
        ("create_work", json!({ "repo": r })),
        ("create_work", json!({ "repo": r, "title": "t", "priority": 9 })),
        ("create_work", json!({ "repo": r, "title": "t", "labels": "bug" })),
        ("comment", json!({ "change": id, "path": "b.txt", "line": 1 })),
        ("comment", json!({ "change": id, "path": "b.txt", "line": "one", "body": "x" })),
        ("comment", json!({ "change": id, "line": 1, "body": "x" })),
        ("comment", json!({ "change": id, "path": "b.txt", "line": 1, "side": "middle", "body": "x" })),
        ("review", json!({ "change": id })),
        ("review", json!({ "change": id, "verdict": "maybe" })),
        ("review", json!({ "change": id, "verdict": "comment", "comments": "x" })),
        ("request_ai_review", json!({ "change": id, "mode": "huge" })),
    ] {
        let res = c.call(tool, args.clone()).await;
        assert_eq!(res["isError"], true, "{tool} {args}: {res}");
        let status = res["structuredContent"]["http_status"].as_u64().unwrap_or(0);
        assert!(status == 400 || status == 422, "{tool} {args}: {res}");
        assert!(!res["content"][0]["text"].as_str().unwrap().is_empty());
    }
    // Unknown things a tool reads: not found.
    for (tool, args) in [
        ("read_file", json!({ "repo": r, "path": "a.txt", "rev": "no-such-branch" })),
        ("list_tree", json!({ "repo": r, "path": "nope" })),
        ("blame", json!({ "repo": r, "path": "nope.txt" })),
        ("get_changeset", json!({ "repo": r, "id": "cs_0000000000000000" })),
        ("compare", json!({ "repo": r, "base": "main", "head": "no-such-branch" })),
        ("log", json!({ "repo": r, "rev": "no-such-branch" })),
        ("comment", json!({ "thread": "th_nope", "body": "x" })),
    ] {
        let e = c.tool_err(tool, args.clone()).await;
        assert!(e.starts_with("not found"), "{tool} {args}: {e}");
    }
}

/// A route: method, path and body.
type Route = (&'static str, String, Option<Value>);

/// Every tool, for anonymous callers and another owner's token, on a public and a private
/// repo: the tool succeeds exactly when its route does, and a refusal carries the route's
/// status and words ("this needs a token", "not allowed", "not found").
#[tokio::test]
async fn every_tool_is_authorized_like_its_route() {
    let a = app(Some("root"));
    let root = Some("root");
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "acme", "kind": "org" })), root).await;
    a.req("POST", "/v1/code/owners", Some(json!({ "name": "eve", "kind": "user" })), root).await;
    let (_, t) = a.req("POST", "/v1/code/owners/eve/tokens", Some(json!({ "name": "t" })), root).await;
    let eve = t["token"].as_str().unwrap().to_string();
    struct Fixture {
        repo: String,
        cs: String,
        change: String,
        work: String,
        thread: String,
    }
    let mut fixtures = Vec::new();
    for (name, vis) in [("pub", "public"), ("priv", "private")] {
        let p = format!("/v1/code/repos/acme/{name}");
        let (s, v) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": name, "visibility": vis })), root).await;
        assert!(s.is_success(), "{v}");
        let (_, l) = a.req("POST", &format!("{p}/land"), Some(json!({ "message": "m", "files": { "a.txt": { "content": "a\n" } } })), root).await;
        let (_, ch) = a.req("POST", &format!("{p}/changes"), Some(json!({ "title": "t", "base": "main", "message": "t", "files": { "a.txt": { "content": "b\n" } } })), root).await;
        let change = ch["id"].as_str().unwrap().to_string();
        let (_, w) = a.req("POST", &format!("{p}/work"), Some(json!({ "title": "w" })), root).await;
        let (_, th) = a.req("POST", &format!("/v1/code/changes/{change}/threads"), Some(json!({ "path": "a.txt", "line": 1, "body": "x" })), root).await;
        let f = Fixture { repo: format!("acme/{name}"), cs: l["changeset"].as_str().unwrap().into(), change, work: w["id"].as_str().unwrap().into(), thread: th["id"].as_str().unwrap().into() };
        fixtures.push((vis, f));
    }

    for (who, token) in [("anonymous", None), ("eve", Some(eve.as_str()))] {
        let mut c = Client::new(&a, token);
        c.initialize("2025-11-25").await;
        for (vis, f) in &fixtures {
            let p = format!("/v1/code/repos/{}", f.repo);
            let (ch, wi, th, cs) = (&f.change, &f.work, &f.thread, &f.cs);
            let files = json!({ "z.txt": { "content": "z\n" } });
            let get = |path: String| -> Route { ("GET", path, None) };
            let post = |path: String, body: Value| -> Route { ("POST", path, Some(body)) };
            // (tool, its arguments, its route, whether it only reads)
            let cases: Vec<(&str, Value, Route, bool)> = vec![
                ("search_code", json!({ "repo": f.repo, "query": "a" }), get(format!("{p}/search?q=a")), true),
                ("read_file", json!({ "repo": f.repo, "path": "a.txt", "rev": "main" }), get(format!("{p}/blob/main/a.txt")), true),
                ("list_tree", json!({ "repo": f.repo, "rev": "main" }), get(format!("{p}/tree/main/")), true),
                ("log", json!({ "repo": f.repo }), get(format!("{p}/log?limit=30")), true),
                ("blame", json!({ "repo": f.repo, "path": "a.txt", "rev": "main" }), get(format!("{p}/blame/main/a.txt")), true),
                ("get_changeset", json!({ "repo": f.repo, "id": cs }), get(format!("{p}/changesets/{cs}")), true),
                ("compare", json!({ "repo": f.repo, "base": cs, "head": "main" }), get(format!("{p}/compare/{cs}...main")), true),
                ("get_change", json!({ "change": ch }), get(format!("/v1/code/changes/{ch}")), true),
                ("list_changes", json!({ "repo": f.repo }), get(format!("{p}/changes")), true),
                ("list_work", json!({ "repo": f.repo }), get(format!("{p}/work")), true),
                ("queue_status", json!({ "repo": f.repo }), get(format!("{p}/queue")), true),
                ("create_change", json!({ "repo": f.repo, "title": "t", "base": "main", "files": files }), post(format!("{p}/changes"), json!({ "title": "t", "message": "t", "base": "main", "files": files })), false),
                ("revise_change", json!({ "change": ch, "base": "main", "files": files }), post(format!("/v1/code/changes/{ch}/revise"), json!({ "base": "main", "message": "m", "files": files })), false),
                ("land", json!({ "change": ch }), post(format!("/v1/code/changes/{ch}/land"), json!({})), false),
                ("land", json!({ "repo": f.repo, "base": "main", "message": "m", "files": files }), post(format!("{p}/land"), json!({ "base": "main", "message": "m", "files": files })), false),
                ("claim_work", json!({ "repo": f.repo, "agent": "bot" }), post(format!("{p}/work/claim"), json!({ "agent": "bot" })), false),
                ("heartbeat_work", json!({ "work": wi, "agent": "bot" }), post(format!("/v1/code/work/{wi}/heartbeat"), json!({ "agent": "bot" })), false),
                ("release_work", json!({ "work": wi, "agent": "bot" }), post(format!("/v1/code/work/{wi}/release"), json!({ "agent": "bot" })), false),
                ("complete_work", json!({ "work": wi, "agent": "bot" }), post(format!("/v1/code/work/{wi}/complete"), json!({ "agent": "bot" })), false),
                ("create_work", json!({ "repo": f.repo, "title": "t" }), post(format!("{p}/work"), json!({ "title": "t" })), false),
                ("comment", json!({ "change": ch, "path": "a.txt", "line": 1, "body": "c" }), post(format!("/v1/code/changes/{ch}/threads"), json!({ "path": "a.txt", "line": 1, "body": "c" })), false),
                ("comment", json!({ "thread": th, "body": "c" }), post(format!("/v1/code/threads/{th}/comments"), json!({ "body": "c" })), false),
                ("review", json!({ "change": ch, "verdict": "approve" }), post(format!("/v1/code/changes/{ch}/reviews"), json!({ "verdict": "approve" })), false),
                ("review", json!({ "change": ch, "verdict": "comment", "body": "c" }), post(format!("/v1/code/changes/{ch}/reviews"), json!({ "verdict": "comment", "body": "c" })), false),
                ("request_ai_review", json!({ "change": ch }), post(format!("/v1/code/changes/{ch}/ai-review"), json!({})), false),
            ];
            let mut seen = std::collections::HashSet::new();
            for (tool, args, (method, path, body), reads) in cases {
                seen.insert(tool);
                let (status, route) = a.req(method, &path, body, token).await;
                let res = c.call(tool, args.clone()).await;
                let ctx = format!("{who} {vis} {tool} {args}: route {status} {route}, tool {res}");
                assert_eq!(res["isError"], !status.is_success(), "{ctx}");
                // What the routes allow: reads of public repos, and on a public repo another
                // owner may comment (not approve, not write). Private repos don't exist.
                let comments = tool == "comment" || (tool == "review" && args["verdict"] == "comment");
                let expected = match (*vis, reads, who) {
                    ("private", ..) => StatusCode::NOT_FOUND,
                    (_, true, _) => StatusCode::OK,
                    (_, false, "anonymous") => StatusCode::UNAUTHORIZED,
                    _ if comments => StatusCode::CREATED,
                    _ => StatusCode::FORBIDDEN,
                };
                assert_eq!(status, expected, "{ctx}");
                if status.is_success() {
                    continue;
                }
                assert_eq!(res["structuredContent"]["http_status"], status.as_u16(), "{ctx}");
                let text = res["content"][0]["text"].as_str().unwrap();
                match status {
                    StatusCode::UNAUTHORIZED => assert!(text.contains("this needs a token"), "{ctx}"),
                    StatusCode::FORBIDDEN => assert!(text.starts_with("not allowed"), "{ctx}"),
                    _ => assert!(text.starts_with("not found"), "{ctx}"),
                }
            }
            assert_eq!(seen.len(), TOOLS.len(), "every tool is checked");
        }
    }
}
