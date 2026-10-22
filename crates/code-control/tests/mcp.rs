//! The MCP server (`/v1/code/mcp`) driven as a JSON-RPC client: the Streamable HTTP transport
//! (sessions, version negotiation, notifications, batches, Origin, GET/DELETE), every tool end to
//! end, land outcomes (landed, conflict, blocked), and authorization matching the HTTP routes.

mod common;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use common::{App, app};
use serde_json::{Value, json};

const MCP: &str = "/v1/code/mcp";

struct Client<'a> {
    a: &'a App,
    token: Option<String>,
    session: Option<String>,
    next: u64,
}

impl<'a> Client<'a> {
    fn new(a: &'a App, token: Option<&str>) -> Self {
        Client { a, token: token.map(str::to_string), session: None, next: 1 }
    }

    async fn post(&self, body: Value, extra: &[(&str, &str)]) -> (StatusCode, HeaderMap, Value) {
        let mut b = Request::builder().method("POST").uri(MCP).header("content-type", "application/json").header("accept", "application/json, text/event-stream");
        if let Some(t) = &self.token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        if let Some(s) = &self.session {
            b = b.header("mcp-session-id", s.as_str());
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
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for n in ["search_code", "read_file", "list_tree", "log", "blame", "get_changeset", "compare", "create_change", "revise_change", "land", "get_change", "list_changes", "claim_work", "heartbeat_work", "complete_work", "list_work", "create_work", "comment", "review", "queue_status", "request_ai_review"] {
        assert!(names.contains(&n), "missing tool {n}");
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

/// The auth matrix through MCP tools: every caller × read and write, public and private, as the
/// routes answer them; and a bad token gets no session. Open mode lets any token in.
#[tokio::test]
async fn tools_follow_the_whole_auth_matrix() {
    let a = common::app(Some("root"));
    let mut tokens = Vec::new();
    for owner in ["acme", "other"] {
        a.req("POST", "/v1/code/owners", Some(json!({ "name": owner, "kind": "org" })), Some("root")).await;
        let (_, t) = a.req("POST", &format!("/v1/code/owners/{owner}/tokens"), Some(json!({ "name": "t" })), Some("root")).await;
        tokens.push(t["token"].as_str().unwrap().to_string());
    }
    for (name, vis) in [("pub", "public"), ("priv", "private")] {
        let (s, v) = a.req("POST", "/v1/code/repos", Some(json!({ "owner": "acme", "name": name, "visibility": vis })), Some("root")).await;
        assert!(s.is_success(), "{v}");
        let (s, v) = a.req("POST", &format!("/v1/code/repos/acme/{name}/land"), Some(json!({ "message": "m", "files": { "a.txt": { "content": "a\n" } } })), Some("root")).await;
        assert!(s.is_success(), "{v}");
    }
    // None: the tool works; Some(text): it fails, saying that.
    type Outcomes<'a> = [Option<&'a str>; 4];
    let rows: [(&str, Option<&str>, Outcomes); 4] = [
        ("admin", Some("root"), [None; 4]),
        ("owner", Some(tokens[0].as_str()), [None; 4]),
        ("another owner", Some(tokens[1].as_str()), [None, Some("not found"), Some("not allowed"), Some("not found")]),
        ("anonymous", None, [None, Some("not found"), Some("this needs a token"), Some("not found")]),
    ];
    for (who, token, want) in rows {
        let mut c = Client::new(&a, token);
        c.initialize("2025-06-18").await;
        for (i, (tool, repo)) in [("read_file", "acme/pub"), ("read_file", "acme/priv"), ("create_work", "acme/pub"), ("create_work", "acme/priv")].into_iter().enumerate() {
            let args = if tool == "read_file" { json!({ "repo": repo, "path": "a.txt" }) } else { json!({ "repo": repo, "title": format!("by {who}") }) };
            match want[i] {
                None if tool == "read_file" => assert_eq!(c.tool(tool, args).await["content"], "a\n", "{who} {tool} {repo}"),
                None => assert_eq!(c.tool(tool, args).await["title"], format!("by {who}"), "{who} {tool} {repo}"),
                Some(msg) => {
                    let err = c.tool_err(tool, args).await;
                    assert!(err.contains(msg), "{who} {tool} {repo}: {err}");
                }
            }
        }
    }
    // Unknown and garbage tokens: no session (401), so no tool runs.
    let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } });
    for token in ["rlc_0000", "hunter2"] {
        let (s, _, v) = Client::new(&a, Some(token)).post(init.clone(), &[]).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{token}: {v}");
    }
    // DELETE needs the session header.
    assert_eq!(a.send(Request::builder().method("DELETE").uri(MCP).body(Body::empty()).unwrap()).await.status(), StatusCode::BAD_REQUEST);
    // Open mode: every caller is the admin, whatever token it sends.
    let open = app(None);
    open.repo("acme", "x").await;
    let mut c = Client::new(&open, Some("hunter2"));
    c.initialize("2025-06-18").await;
    assert_eq!(c.tool("create_work", json!({ "repo": "acme/x", "title": "t" })).await["title"], "t");
}
