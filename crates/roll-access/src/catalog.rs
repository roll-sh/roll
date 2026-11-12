//! Every control-plane route as one action on one product (docs/permissions.md, "Actions"), and
//! how to find the resource it's about. The gate matches a request's route pattern and method
//! here; a route with no row is the operator's alone (403 for every grant).
//!
//! A row names an exact route pattern, or a prefix ending in `/` that covers every pattern under
//! it (the longest prefix wins; an exact row beats any prefix). `crates/rolld/tests/auth.rs`
//! checks that every route rolld serves is classified or listed in [`OPERATOR_ONLY`].

use roll_auth::scope::Action;
use serde::Serialize;
use serde_json::Value;

/// How the action is decided.
#[derive(Clone, Copy, Debug)]
pub enum Act {
    Is(Action),
    /// GET and HEAD read, DELETE deletes, the rest write.
    ByMethod,
    /// `deploy` when the path parameter is `production`, else `write`.
    ProductionIf(&'static str),
    /// Decided from the JSON body.
    Body(fn(&Value) -> Action),
}

/// What the request is about.
#[derive(Clone, Copy, Debug)]
pub enum Tgt {
    /// One resource, named by these path parameters joined with `/` (`owner`, `repo`), of `kind`
    /// (a [`crate::Resolver`] may map it to the resource rules name, e.g. a branch to its project).
    Param(&'static [&'static str], &'static str),
    /// A list of the product's resources: items are filtered by `id_field` (in `items`, or the
    /// answer itself when it's an array) for rules that name resources.
    List { items: Option<&'static str>, id_field: &'static str },
    /// A new resource. With a project-scoped rule the project comes from the path parameter
    /// `project`, or the `X-Roll-Project` header; the new resource (`id_field` of the answer)
    /// is then put in it, when `place` names its workspace kind.
    Create { project: Option<&'static str>, place: Option<&'static str>, id_field: &'static str },
    /// The product as a whole.
    Product,
    /// Any live grant (its own info, the MCP endpoint whose tools each make their own requests).
    AnyGrant,
}

/// Where the resource's environment comes from, besides its product's resolver.
#[derive(Clone, Copy, Debug)]
pub enum EnvFrom {
    None,
    Param(&'static str),
    Fixed(&'static str),
}

#[derive(Clone, Copy, Debug)]
pub struct Row {
    /// `GET`, `POST`, … or `*` for any.
    pub method: &'static str,
    pub route: &'static str,
    pub product: &'static str,
    pub act: Act,
    pub target: Tgt,
    pub env: EnvFrom,
    /// The query parameter carrying a path inside the resource (secrets' folders): `/` without it.
    pub path_query: Option<&'static str>,
    /// Fields of the answer that are credentials: replaced unless the grant may `reveal`.
    pub reveals: &'static [&'static str],
}

impl Row {
    pub fn needs_body(&self) -> bool {
        matches!(self.act, Act::Body(_))
    }

    pub fn action(&self, method: &str, params: &[(String, String)], body: Option<&Value>) -> Action {
        match self.act {
            Act::Is(a) => a,
            Act::ByMethod => match method {
                "GET" | "HEAD" | "OPTIONS" => Action::Read,
                "DELETE" => Action::Delete,
                _ => Action::Write,
            },
            Act::ProductionIf(p) => {
                if params.iter().any(|(k, v)| k == p && v == "production") { Action::Deploy } else { Action::Write }
            }
            Act::Body(f) => f(body.unwrap_or(&Value::Null)),
        }
    }
}

const fn row(method: &'static str, route: &'static str, product: &'static str, act: Act, target: Tgt) -> Row {
    Row { method, route, product, act, target, env: EnvFrom::None, path_query: None, reveals: &[] }
}

const R: Act = Act::Is(Action::Read);
const W: Act = Act::Is(Action::Write);
const D: Act = Act::Is(Action::Delete);
const REVEAL: Act = Act::Is(Action::Reveal);
const DEPLOY: Act = Act::Is(Action::Deploy);
const ADMIN: Act = Act::Is(Action::Admin);
const M: Act = Act::ByMethod;

const fn one(params: &'static [&'static str], kind: &'static str) -> Tgt {
    Tgt::Param(params, kind)
}
const fn list(id_field: &'static str) -> Tgt {
    Tgt::List { items: None, id_field }
}
const fn create(place: &'static str, id_field: &'static str) -> Tgt {
    Tgt::Create { project: None, place: Some(place), id_field }
}

impl Row {
    const fn env(mut self, e: EnvFrom) -> Row {
        self.env = e;
        self
    }
    const fn reveals(mut self, f: &'static [&'static str]) -> Row {
        self.reveals = f;
        self
    }
    const fn path_query(mut self, q: &'static str) -> Row {
        self.path_query = Some(q);
        self
    }
}

/// The workspace kind of a product's resources (docs/workspace.md), for project selectors.
pub fn workspace_kind(product: &str) -> Option<&'static str> {
    Some(match product {
        "postgres" => "postgres",
        "redis" => "redis",
        "streams" => "stream",
        "storage" => "bucket",
        "nosql" => "nosql",
        "code" => "repo",
        "queues" => "queue",
        "tasks" => "tasks",
        "search" => "search",
        "apps" => "service",
        _ => return None,
    })
}

/// Routes only the operator may call, on purpose (no grant ever, whatever its rules): they hand
/// out power the evaluator can't narrow.
pub const OPERATOR_ONLY: &[(&str, &str)] = &[
    ("/v1/code/mcp", "roll code's own MCP server calls roll code's routes without the gate: grants use /v1/mcp"),
    ("/v1/access/approvals/{id}/approve", "a grant never approves a request"),
    ("/v1/access/approvals/{id}/deny", "a grant never decides a request"),
    ("/v1/access/oauth/authorize", "the consent screen is the operator's"),
];

pub fn rows() -> &'static [Row] {
    ROWS
}

/// A row and the path parameters it found (when it matched the real path, not the route).
pub struct Found {
    pub row: &'static Row,
    /// Set when the row was matched against the request's path (under a catch-all route); else
    /// the router's own parameters apply.
    pub params: Option<Vec<(String, String)>>,
}

fn method_rank(r: &Row, method: &str) -> Option<u8> {
    if r.method == method || (method == "HEAD" && r.method == "GET") {
        Some(2)
    } else if r.method == "*" {
        Some(1)
    } else {
        None
    }
}

/// The row for a request. An exact route first (a row naming the method beats `*`), else the
/// longest prefix. A route that is a catch-all (`/v1/tasks/{*rest}`, storage's nest) is the
/// product's own router in disguise: its rows are matched against the request's path.
pub fn find(method: &str, route: &str, path: &str) -> Option<Found> {
    if OPERATOR_ONLY.iter().any(|(r, _)| *r == route) {
        return None;
    }
    let exact = ROWS.iter().filter(|r| r.route == route).filter_map(|r| method_rank(r, method).map(|m| (m, r))).max_by_key(|(m, _)| *m);
    if let Some((_, row)) = exact {
        return Some(Found { row, params: None });
    }
    let prefix = ROWS
        .iter()
        .filter(|r| r.route.ends_with('/') && route.starts_with(r.route))
        .filter_map(|r| method_rank(r, method).map(|m| ((r.route.len(), m), r)))
        .max_by_key(|(k, _)| *k);
    if let Some((_, row)) = prefix {
        return Some(Found { row, params: None });
    }
    // A nested service (storage) has no matched route at all: its path decides.
    let nested = route == "unmatched" && NESTS.iter().any(|n| path == *n || path.starts_with(&format!("{n}/")));
    if !route.contains("{*") && !is_nest(route) && !nested {
        return None;
    }
    by_path(method, path)
}

/// Products mounted with `nest_service`: axum gives their requests no matched route.
pub const NESTS: &[&str] = &["/v1/storage"];

/// The row for a request by its path alone (what a catch-all hides, or a tool's route).
pub fn by_path(method: &str, path: &str) -> Option<Found> {
    let path = path.split('?').next().unwrap_or(path);
    if OPERATOR_ONLY.iter().any(|(r, _)| match_pattern(r, path).is_some()) {
        return None;
    }
    let exact = ROWS
        .iter()
        .filter(|r| !r.route.ends_with('/'))
        .filter_map(|r| {
            let (score, params) = match_pattern(r.route, path)?;
            Some(((score, method_rank(r, method)?), r, params))
        })
        .max_by_key(|(k, ..)| *k);
    if let Some((_, row, params)) = exact {
        return Some(Found { row, params: Some(params) });
    }
    ROWS.iter()
        .filter(|r| r.route.ends_with('/'))
        .filter_map(|r| {
            let params = match_prefix(r.route, path)?;
            Some(((r.route.len(), method_rank(r, method)?), r, params))
        })
        .max_by_key(|(k, ..)| *k)
        .map(|(_, row, params)| Found { row, params: Some(params) })
}

/// How a route pattern is classified for grants, whatever the method: its product, `by path` (a
/// catch-all whose real routes have rows), `operator only` (on purpose), or `None`: no row, so no
/// grant can call it. rolld logs it for every route at startup; `rolld/tests/auth.rs` reads it.
pub fn classified(route: &str) -> Option<&'static str> {
    if OPERATOR_ONLY.iter().any(|(r, _)| *r == route) {
        return Some("operator only");
    }
    if let Some(r) = ROWS.iter().find(|r| r.route == route) {
        return Some(r.product);
    }
    if let Some(r) = ROWS.iter().filter(|r| r.route.ends_with('/') && route.starts_with(r.route)).max_by_key(|r| r.route.len()) {
        return Some(r.product);
    }
    if route.contains("{*") || is_nest(route) {
        let base = route.split("{*").next().unwrap_or(route);
        if ROWS.iter().any(|r| r.route.starts_with(base) && r.route != base) {
            return Some("by path");
        }
    }
    None
}

/// A route listed for a nest (`/v1/storage`, `/v1/storage/`): its rows are matched by path.
fn is_nest(route: &str) -> bool {
    NESTS.iter().any(|n| route == *n || route.starts_with(&format!("{n}/")))
}

fn decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// `pattern` against a path: the parameters and how many static segments matched.
fn match_pattern(pattern: &str, path: &str) -> Option<(usize, Vec<(String, String)>)> {
    let pat: Vec<&str> = pattern.trim_start_matches('/').split('/').collect();
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    let mut params = Vec::new();
    let mut statics = 0;
    for (i, p) in pat.iter().enumerate() {
        if let Some(name) = p.strip_prefix("{*").and_then(|p| p.strip_suffix('}')) {
            if i >= segs.len() {
                return None;
            }
            params.push((name.to_string(), decode(&segs[i..].join("/"))));
            return Some((statics, params));
        }
        let s = segs.get(i)?;
        match p.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
            Some(name) if !s.is_empty() => params.push((name.to_string(), decode(s))),
            Some(_) => return None,
            None if p == s => statics += 1,
            None => return None,
        }
    }
    (pat.len() == segs.len()).then_some((statics, params))
}

/// A prefix row (`/a/{x}/`) against a path with something after it.
fn match_prefix(prefix: &str, path: &str) -> Option<Vec<(String, String)>> {
    let base = prefix.trim_end_matches('/');
    let n = base.trim_start_matches('/').split('/').count();
    let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if segs.len() <= n {
        return None;
    }
    let head = format!("/{}", segs[..n].join("/"));
    match_pattern(base, &head).map(|(_, p)| p)
}

/// A row as the catalogue route (and the dashboard) shows it.
#[derive(Serialize)]
pub struct RowView {
    pub method: &'static str,
    pub route: &'static str,
    pub product: &'static str,
    pub action: String,
    pub target: &'static str,
}

pub fn view(r: &Row) -> RowView {
    let action = match r.act {
        Act::Is(a) => a.as_str().to_string(),
        Act::ByMethod => "by method".into(),
        Act::ProductionIf(p) => format!("deploy when {{{p}}} is production, else write"),
        Act::Body(_) => "from the body".into(),
    };
    let target = match r.target {
        Tgt::Param(..) => "one",
        Tgt::List { .. } => "list",
        Tgt::Create { .. } => "create",
        Tgt::Product => "product",
        Tgt::AnyGrant => "any grant",
    };
    RowView { method: r.method, route: r.route, product: r.product, action, target }
}

// ── body-decided actions ──

/// Postgres SQL: `read_only: true` reads (and runs in a read-only transaction); the rest writes.
fn sql_action(b: &Value) -> Action {
    if b.get("read_only").and_then(Value::as_bool) == Some(true) { Action::Read } else { Action::Write }
}

/// Redis: commands that only read are `read`, flushes `delete`, the rest `write`.
fn redis_action(b: &Value) -> Action {
    // `{"command": ["SET", "k", "v"]}` (the route's shape), or one string.
    let cmd = match b.get("command") {
        Some(Value::Array(a)) => a.first().and_then(Value::as_str).unwrap_or(""),
        Some(Value::String(s)) => s.as_str(),
        _ => "",
    };
    let first = cmd.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
    match first.as_str() {
        "FLUSHDB" | "FLUSHALL" => Action::Delete,
        "GET" | "MGET" | "STRLEN" | "GETRANGE" | "EXISTS" | "TYPE" | "TTL" | "PTTL" | "EXPIRETIME" | "KEYS" | "SCAN" | "DBSIZE" | "HGET" | "HMGET" | "HGETALL"
        | "HKEYS" | "HVALS" | "HLEN" | "HEXISTS" | "HSCAN" | "HSTRLEN" | "LRANGE" | "LLEN" | "LINDEX" | "LPOS" | "SMEMBERS" | "SISMEMBER" | "SMISMEMBER"
        | "SCARD" | "SSCAN" | "SRANDMEMBER" | "SINTER" | "SUNION" | "SDIFF" | "ZRANGE" | "ZRANGEBYSCORE" | "ZREVRANGE" | "ZREVRANGEBYSCORE" | "ZSCORE"
        | "ZMSCORE" | "ZCARD" | "ZCOUNT" | "ZRANK" | "ZREVRANK" | "ZSCAN" | "XRANGE" | "XREVRANGE" | "XLEN" | "XINFO" | "PING" | "ECHO" | "INFO" | "MEMORY"
        | "OBJECT" | "RANDOMKEY" | "BITCOUNT" | "GETBIT" | "BITPOS" | "PFCOUNT" | "GEOPOS" | "GEODIST" | "GEOSEARCH" | "JSON.GET" | "JSON.TYPE" | "TIME" => {
            Action::Read
        }
        _ => Action::Write,
    }
}

/// CQL: `SELECT` (and `DESCRIBE`, `USE`) read, `DROP` and `TRUNCATE` delete, the rest write. A
/// body with several statements is the strongest of them.
fn cql_action(b: &Value) -> Action {
    let text = b.get("query").or_else(|| b.get("cql")).or_else(|| b.get("statement")).and_then(Value::as_str).unwrap_or("");
    let mut out = Action::Read;
    let mut any = false;
    for stmt in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        any = true;
        let first = stmt.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
        let a = match first.as_str() {
            "SELECT" | "DESCRIBE" | "DESC" | "USE" | "LIST" => Action::Read,
            "DROP" | "TRUNCATE" => Action::Delete,
            _ => Action::Write,
        };
        out = out.max(a);
    }
    if any { out } else { Action::Write }
}

/// Agents' machine commands: answering an agent's approval is `admin` (it would let an agent
/// approve itself), publishing with `land` ships to the default branch (`deploy`), the rest write.
fn agent_command_action(b: &Value) -> Action {
    match b.get("type").and_then(Value::as_str) {
        Some("request.respond") => Action::Admin,
        Some("thread.publish") if b.get("land").and_then(Value::as_bool) == Some(true) => Action::Deploy,
        _ => Action::Write,
    }
}

/// Storage's IAM JSON API: reads and simulations are `read`, everything else `admin`.
fn iam_action(b: &Value) -> Action {
    let a = b.get("Action").and_then(Value::as_str).unwrap_or("");
    if ["Get", "List", "Simulate", "Generate"].iter().any(|p| a.starts_with(p)) { Action::Read } else { Action::Admin }
}

// Credentials in answers, per product: replaced unless the grant may reveal.
const PG_SECRETS: &[&str] = &["connection_uri", "password"];
const REDIS_SECRETS: &[&str] = &["redis_url", "rest_token", "password"];
const NOSQL_SECRETS: &[&str] = &["password", "http_token", "secret_access_key"];
const STREAM_SECRETS: &[&str] = &["connect", "secret", "password"];
const STORAGE_SECRETS: &[&str] = &["webhook_secret", "secret_access_key", "session_token", "anon_key", "service_key"];
const QUEUE_SECRETS: &[&str] = &["password", "urls", "password_hash"];
const SEARCH_SECRETS: &[&str] = &["password"];

const PG: &[&str] = &["id"];
const REPO: &[&str] = &["owner", "repo"];

static ROWS: &[Row] = &[
    // ── access (this crate) ──
    row("GET", "/v1/access", "access", R, Tgt::AnyGrant),
    row("GET", "/v1/access/catalog", "access", R, Tgt::AnyGrant),
    row("GET", "/v1/access/tokens", "access", R, Tgt::Product),
    row("POST", "/v1/access/tokens", "access", ADMIN, Tgt::Product),
    row("GET", "/v1/access/tokens/{id}", "access", R, Tgt::Product),
    row("PATCH", "/v1/access/tokens/{id}", "access", ADMIN, Tgt::Product),
    row("DELETE", "/v1/access/tokens/{id}", "access", ADMIN, Tgt::Product),
    row("GET", "/v1/access/approvals", "access", R, Tgt::Product),
    // A grant waits on its own approvals (the handler checks it's its own, or access read).
    row("GET", "/v1/access/approvals/{id}", "access", R, Tgt::AnyGrant),
    row("GET", "/v1/access/audit", "access", R, Tgt::Product),
    row("GET", "/v1/access/oauth/clients", "access", R, Tgt::Product),
    row("GET", "/v1/access/oauth/clients/{id}", "access", R, Tgt::Product),
    // The MCP server: every tool call is its own request through the gate.
    row("*", "/v1/mcp", "access", R, Tgt::AnyGrant),
    // rolld's own.
    row("GET", "/v1/info", "access", R, Tgt::AnyGrant),
    // ── workspace projects ──
    row("GET", "/v1/workspace/projects", "workspace", R, list("id")),
    row("POST", "/v1/workspace/projects", "workspace", W, Tgt::Product),
    row("GET", "/v1/workspace/projects/{id}", "workspace", R, one(&["id"], "project")),
    row("PATCH", "/v1/workspace/projects/{id}", "workspace", W, one(&["id"], "project")),
    row("DELETE", "/v1/workspace/projects/{id}", "workspace", D, one(&["id"], "project")),
    row("PUT", "/v1/workspace/projects/{id}/resources/{kind}/{ref}", "workspace", W, one(&["id"], "project")),
    row("DELETE", "/v1/workspace/projects/{id}/resources/{kind}/{ref}", "workspace", W, one(&["id"], "project")),
    // ── Postgres ──
    row("GET", "/v1/health", "postgres", R, Tgt::AnyGrant),
    row("GET", "/v1/projects", "postgres", R, list("id")).reveals(PG_SECRETS),
    row("POST", "/v1/projects", "postgres", W, create("postgres", "id")).reveals(PG_SECRETS),
    row("GET", "/v1/projects/{id}", "postgres", R, one(PG, "project")).reveals(PG_SECRETS),
    row("PATCH", "/v1/projects/{id}", "postgres", W, one(PG, "project")).reveals(PG_SECRETS),
    row("DELETE", "/v1/projects/{id}", "postgres", D, one(PG, "project")),
    row("POST", "/v1/projects/{id}/branches", "postgres", W, one(PG, "project")).reveals(PG_SECRETS),
    row("DELETE", "/v1/projects/{id}/branches/{branch}", "postgres", D, one(PG, "project")),
    row("GET", "/v1/projects/{id}/branches/{branch}/history", "postgres", R, one(PG, "project")),
    // Rewinds the branch in place (the old state is kept as a branch, but clients see it gone).
    row("POST", "/v1/projects/{id}/branches/{branch}/restore", "postgres", D, one(PG, "project")),
    row("POST", "/v1/projects/{id}/branches/{branch}/backup", "postgres", W, one(PG, "project")),
    row("POST", "/v1/projects/{id}/branches/{branch}/evict", "postgres", W, one(PG, "project")),
    row("POST", "/v1/projects/{id}/branches/{branch}/replicas", "postgres", W, one(PG, "project")).reveals(PG_SECRETS),
    row("GET", "/v1/projects/{id}/usage", "postgres", R, one(PG, "project")),
    row("GET", "/v1/endpoints/{id}", "postgres", R, one(PG, "endpoint")).reveals(PG_SECRETS),
    row("PATCH", "/v1/endpoints/{id}", "postgres", W, one(PG, "endpoint")),
    row("DELETE", "/v1/endpoints/{id}", "postgres", D, one(PG, "endpoint")),
    row("POST", "/v1/endpoints/{id}/start", "postgres", W, one(PG, "endpoint")),
    row("POST", "/v1/endpoints/{id}/suspend", "postgres", W, one(PG, "endpoint")),
    row("POST", "/v1/endpoints/{id}/query", "postgres", Act::Body(sql_action), one(PG, "endpoint")),
    // ── Redis ──
    row("GET", "/v1/redis", "redis", R, Tgt::AnyGrant),
    row("GET", "/v1/redis/databases", "redis", R, list("id")).reveals(REDIS_SECRETS),
    row("POST", "/v1/redis/databases", "redis", W, create("redis", "id")).reveals(REDIS_SECRETS),
    row("GET", "/v1/redis/databases/{id}", "redis", R, one(PG, "database")).reveals(REDIS_SECRETS),
    row("PATCH", "/v1/redis/databases/{id}", "redis", W, one(PG, "database")).reveals(REDIS_SECRETS),
    row("DELETE", "/v1/redis/databases/{id}", "redis", D, one(PG, "database")),
    row("POST", "/v1/redis/databases/{id}/branches", "redis", W, one(PG, "database")).reveals(REDIS_SECRETS),
    row("POST", "/v1/redis/databases/{id}/restore", "redis", D, one(PG, "database")),
    row("POST", "/v1/redis/databases/{id}/password", "redis", REVEAL, one(PG, "database")),
    row("GET", "/v1/redis/databases/{id}/replicas", "redis", R, one(PG, "database")).reveals(REDIS_SECRETS),
    row("POST", "/v1/redis/databases/{id}/replicas", "redis", W, one(PG, "database")).reveals(REDIS_SECRETS),
    row("GET", "/v1/redis/databases/{id}/usage", "redis", R, one(PG, "database")),
    row("POST", "/v1/redis/databases/{id}/unload", "redis", W, one(PG, "database")),
    row("POST", "/v1/redis/databases/{id}/command", "redis", Act::Body(redis_action), one(PG, "database")),
    row("DELETE", "/v1/redis/replicas/{id}", "redis", D, one(PG, "replica")),
    row("POST", "/v1/redis/replicas/{id}/unload", "redis", W, one(PG, "replica")),
    // ── streams ──
    row("GET", "/v1/kafka", "streams", R, Tgt::AnyGrant),
    row("GET", "/v1/streams", "streams", R, list("id")),
    row("POST", "/v1/streams", "streams", W, create("stream", "id")).reveals(STREAM_SECRETS),
    row("GET", "/v1/streams/{s}", "streams", R, one(&["s"], "stream")),
    row("PATCH", "/v1/streams/{s}", "streams", W, one(&["s"], "stream")),
    row("DELETE", "/v1/streams/{s}", "streams", D, one(&["s"], "stream")),
    row("GET", "/v1/streams/{s}/keys", "streams", R, one(&["s"], "stream")),
    row("POST", "/v1/streams/{s}/keys", "streams", REVEAL, one(&["s"], "stream")),
    row("DELETE", "/v1/streams/{s}/keys/{username}", "streams", ADMIN, one(&["s"], "stream")),
    row("GET", "/v1/streams/{s}/topics", "streams", R, one(&["s"], "stream")),
    row("POST", "/v1/streams/{s}/topics", "streams", W, one(&["s"], "stream")),
    row("GET", "/v1/streams/{s}/topics/{t}", "streams", R, one(&["s"], "stream")),
    row("PATCH", "/v1/streams/{s}/topics/{t}", "streams", W, one(&["s"], "stream")),
    row("DELETE", "/v1/streams/{s}/topics/{t}", "streams", D, one(&["s"], "stream")),
    row("GET", "/v1/streams/{s}/groups", "streams", R, one(&["s"], "stream")),
    row("GET", "/v1/streams/{s}/groups/{g}", "streams", R, one(&["s"], "stream")),
    row("DELETE", "/v1/streams/{s}/groups/{g}", "streams", D, one(&["s"], "stream")),
    row("POST", "/v1/streams/{s}/groups/{g}/reset", "streams", D, one(&["s"], "stream")),
    row("GET", "/v1/streams/{s}/usage", "streams", R, one(&["s"], "stream")),
    row("POST", "/v1/streams/{s}/topics/{t}/records", "streams", W, one(&["s"], "stream")),
    row("GET", "/v1/streams/{s}/topics/{t}/records", "streams", R, one(&["s"], "stream")),
    row("GET", "/v1/streams/{s}/topics/{t}/partitions/{p}/records", "streams", R, one(&["s"], "stream")),
    row("GET", "/v1/streams/{s}/topics/{t}/tail", "streams", R, one(&["s"], "stream")),
    row("POST", "/v1/streams/{s}/groups/{g}/commit", "streams", W, one(&["s"], "stream")),
    row("DELETE", "/v1/streams/{s}/groups/{g}/consumers/{c}", "streams", W, one(&["s"], "stream")),
    // ── tasks (rolld's catch-all: classified by the real path) ──
    row("GET", "/v1/tasks/health", "tasks", R, Tgt::AnyGrant),
    row("POST", "/v1/tasks/publish", "tasks", W, Tgt::Product),
    row("POST", "/v1/tasks/publish/{*destination}", "tasks", W, Tgt::Product),
    row("POST", "/v1/tasks/batch", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/messages", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/messages/cancel", "tasks", D, Tgt::Product),
    row("GET", "/v1/tasks/messages/{id}", "tasks", R, Tgt::Product),
    row("DELETE", "/v1/tasks/messages/{id}", "tasks", D, Tgt::Product),
    row("POST", "/v1/tasks/messages/{id}/retry", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/dlq", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/dlq/retry", "tasks", W, Tgt::Product),
    row("DELETE", "/v1/tasks/dlq/{id}", "tasks", D, Tgt::Product),
    row("GET", "/v1/tasks/queues", "tasks", R, Tgt::Product),
    row("GET", "/v1/tasks/queues/{name}", "tasks", R, Tgt::Product),
    row("PUT", "/v1/tasks/queues/{name}", "tasks", W, Tgt::Product),
    row("DELETE", "/v1/tasks/queues/{name}", "tasks", D, Tgt::Product),
    row("POST", "/v1/tasks/queues/{name}/", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/topics", "tasks", R, Tgt::Product),
    row("GET", "/v1/tasks/topics/{name}", "tasks", R, Tgt::Product),
    row("PUT", "/v1/tasks/topics/{name}", "tasks", W, Tgt::Product),
    row("DELETE", "/v1/tasks/topics/{name}", "tasks", D, Tgt::Product),
    row("POST", "/v1/tasks/topics/{name}/endpoints", "tasks", W, Tgt::Product),
    row("DELETE", "/v1/tasks/topics/{name}/endpoints", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/schedules", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/schedules", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/schedules/{id}", "tasks", R, Tgt::Product),
    row("PATCH", "/v1/tasks/schedules/{id}", "tasks", W, Tgt::Product),
    row("DELETE", "/v1/tasks/schedules/{id}", "tasks", D, Tgt::Product),
    row("POST", "/v1/tasks/schedules/{id}/", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/events", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/events", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/events/{id}", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/e/{*name}", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/apps", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/apps", "tasks", W, Tgt::Product),
    row("DELETE", "/v1/tasks/apps/{id}", "tasks", D, Tgt::Product),
    row("GET", "/v1/tasks/functions", "tasks", R, Tgt::Product),
    row("GET", "/v1/tasks/functions/{id}", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/functions/{id}/", "tasks", W, Tgt::Product),
    row("POST", "/v1/tasks/functions/{id}/cancel", "tasks", D, Tgt::Product),
    row("GET", "/v1/tasks/runs", "tasks", R, Tgt::Product),
    row("GET", "/v1/tasks/runs/{id}", "tasks", R, Tgt::Product),
    row("GET", "/v1/tasks/runs/{id}/wait", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/runs/{id}/cancel", "tasks", D, Tgt::Product),
    row("POST", "/v1/tasks/runs/{id}/rerun", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/keys", "tasks", REVEAL, Tgt::Product),
    row("POST", "/v1/tasks/keys/rotate", "tasks", REVEAL, Tgt::Product),
    row("GET", "/v1/tasks/api-keys", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/api-keys", "tasks", REVEAL, Tgt::Product),
    row("DELETE", "/v1/tasks/api-keys/{id}", "tasks", ADMIN, Tgt::Product),
    row("GET", "/v1/tasks/usage", "tasks", R, Tgt::Product),
    row("GET", "/v1/tasks/stats", "tasks", R, Tgt::Product),
    // ── queues ──
    row("GET", "/v1/queues/health", "queues", R, Tgt::AnyGrant),
    row("GET", "/v1/queues/overview", "queues", R, Tgt::Product),
    row("GET", "/v1/queues/rates", "queues", R, Tgt::Product),
    row("GET", "/v1/queues/usage", "queues", R, Tgt::Product),
    row("GET", "/v1/queues/vhosts", "queues", R, list("name")),
    row("POST", "/v1/queues/vhosts", "queues", W, create("queue", "name")).reveals(QUEUE_SECRETS),
    row("DELETE", "/v1/queues/vhosts/{vhost}", "queues", D, one(&["vhost"], "vhost")),
    row("POST", "/v1/queues/vhosts/{vhost}/credentials", "queues", REVEAL, one(&["vhost"], "vhost")),
    row("POST", "/v1/queues/vhosts/{vhost}/suspend", "queues", W, one(&["vhost"], "vhost")),
    row("POST", "/v1/queues/vhosts/{vhost}/wake", "queues", W, one(&["vhost"], "vhost")),
    row("*", "/v1/queues/vhosts/{vhost}/", "queues", M, one(&["vhost"], "vhost")),
    row("POST", "/v1/queues/vhosts/{vhost}/queues/{queue}/purge", "queues", D, one(&["vhost"], "vhost")),
    row("GET", "/v1/queues/connections", "queues", R, Tgt::Product),
    row("DELETE", "/v1/queues/connections/{id}", "queues", ADMIN, Tgt::Product),
    row("GET", "/v1/queues/channels", "queues", R, Tgt::Product),
    row("GET", "/v1/queues/policies", "queues", R, Tgt::Product),
    row("PUT", "/v1/queues/policies/{vhost}/{name}", "queues", ADMIN, one(&["vhost"], "vhost")),
    row("DELETE", "/v1/queues/policies/{vhost}/{name}", "queues", ADMIN, one(&["vhost"], "vhost")),
    row("GET", "/v1/queues/users", "queues", R, Tgt::Product),
    row("PUT", "/v1/queues/users/{name}", "queues", ADMIN, Tgt::Product),
    row("DELETE", "/v1/queues/users/{name}", "queues", ADMIN, Tgt::Product),
    row("PUT", "/v1/queues/permissions/{vhost}/{user}", "queues", ADMIN, one(&["vhost"], "vhost")),
    row("DELETE", "/v1/queues/permissions/{vhost}/{user}", "queues", ADMIN, one(&["vhost"], "vhost")),
    row("GET", "/v1/queues/definitions", "queues", REVEAL, Tgt::Product),
    row("POST", "/v1/queues/definitions", "queues", ADMIN, Tgt::Product),
    row("*", "/v1/queues/migrate", "queues", ADMIN, Tgt::Product),
    // ── storage (rolld's catch-all: classified by the real path) ──
    row("GET", "/v1/storage", "storage", R, Tgt::Product),
    row("GET", "/v1/storage/usage", "storage", R, Tgt::Product),
    row("*", "/v1/storage/accounts", "storage", ADMIN, Tgt::Product).reveals(STORAGE_SECRETS),
    row("GET", "/v1/storage/accounts", "storage", R, Tgt::Product),
    row("*", "/v1/storage/accounts/{id}", "storage", ADMIN, Tgt::Product),
    row("GET", "/v1/storage/accounts/{id}", "storage", R, Tgt::Product),
    row("GET", "/v1/storage/buckets", "storage", R, list("name")).reveals(STORAGE_SECRETS),
    row("POST", "/v1/storage/buckets", "storage", W, create("bucket", "name")).reveals(STORAGE_SECRETS),
    row("GET", "/v1/storage/buckets/{name}", "storage", R, one(&["name"], "bucket")).reveals(STORAGE_SECRETS),
    row("PATCH", "/v1/storage/buckets/{name}", "storage", ADMIN, one(&["name"], "bucket")).reveals(STORAGE_SECRETS),
    row("DELETE", "/v1/storage/buckets/{name}", "storage", D, one(&["name"], "bucket")),
    row("POST", "/v1/storage/buckets/{name}/branches", "storage", W, one(&["name"], "bucket")).reveals(STORAGE_SECRETS),
    row("GET", "/v1/storage/buckets/{name}/", "storage", R, one(&["name"], "bucket")),
    row("DELETE", "/v1/storage/buckets/{name}/object", "storage", D, one(&["name"], "bucket")),
    row("POST", "/v1/storage/buckets/{name}/delete", "storage", D, one(&["name"], "bucket")),
    row("PUT", "/v1/storage/buckets/{name}/upload", "storage", W, one(&["name"], "bucket")),
    row("POST", "/v1/storage/buckets/{name}/presign", "storage", REVEAL, one(&["name"], "bucket")),
    row("POST", "/v1/storage/buckets/{name}/url", "storage", REVEAL, one(&["name"], "bucket")),
    row("POST", "/v1/storage/buckets/{name}/trash/restore", "storage", W, one(&["name"], "bucket")),
    row("POST", "/v1/storage/buckets/{name}/rules", "storage", ADMIN, one(&["name"], "bucket")),
    row("GET", "/v1/storage/keys", "storage", R, Tgt::Product),
    row("POST", "/v1/storage/keys", "storage", REVEAL, Tgt::Product),
    row("DELETE", "/v1/storage/keys/{id}", "storage", ADMIN, Tgt::Product),
    row("GET", "/v1/storage/roles", "storage", R, Tgt::Product),
    row("*", "/v1/storage/roles", "storage", ADMIN, Tgt::Product),
    row("GET", "/v1/storage/roles/{name}", "storage", R, Tgt::Product),
    row("*", "/v1/storage/roles/{name}", "storage", ADMIN, Tgt::Product),
    row("POST", "/v1/storage/roles/{name}/revoke", "storage", ADMIN, Tgt::Product),
    row("POST", "/v1/storage/roles/{name}/sessions", "storage", REVEAL, Tgt::Product),
    row("POST", "/v1/storage/session-keys/rotate", "storage", ADMIN, Tgt::Product),
    row("GET", "/v1/storage/replication-targets", "storage", R, Tgt::Product),
    row("*", "/v1/storage/replication-targets", "storage", ADMIN, Tgt::Product),
    row("GET", "/v1/storage/replication-targets/{name}", "storage", R, Tgt::Product),
    row("*", "/v1/storage/replication-targets/{name}", "storage", ADMIN, Tgt::Product),
    row("GET", "/v1/storage/cdn", "storage", R, Tgt::Product),
    row("PATCH", "/v1/storage/cdn", "storage", ADMIN, Tgt::Product),
    row("GET", "/v1/storage/cdn/worker", "storage", ADMIN, Tgt::Product),
    row("POST", "/v1/storage/iam", "storage", Act::Body(iam_action), Tgt::Product),
    row("GET", "/v1/storage/api", "storage", REVEAL, Tgt::Product),
    row("POST", "/v1/storage/api/keys/rotate", "storage", REVEAL, Tgt::Product),
    row("PUT", "/v1/storage/api/auth", "storage", ADMIN, Tgt::Product),
    // ── NoSQL ──
    row("GET", "/v1/nosql", "nosql", R, Tgt::AnyGrant),
    row("GET", "/v1/nosql/databases", "nosql", R, list("id")).reveals(NOSQL_SECRETS),
    row("POST", "/v1/nosql/databases", "nosql", W, create("nosql", "id")).reveals(NOSQL_SECRETS),
    row("GET", "/v1/nosql/databases/{id}", "nosql", R, one(PG, "nosql-database")).reveals(NOSQL_SECRETS),
    row("PATCH", "/v1/nosql/databases/{id}", "nosql", W, one(PG, "nosql-database")).reveals(NOSQL_SECRETS),
    row("DELETE", "/v1/nosql/databases/{id}", "nosql", D, one(PG, "nosql-database")),
    row("POST", "/v1/nosql/databases/{id}/branches", "nosql", W, one(PG, "nosql-database")).reveals(NOSQL_SECRETS),
    row("POST", "/v1/nosql/databases/{id}/reset", "nosql", D, one(PG, "nosql-database")).reveals(NOSQL_SECRETS),
    row("POST", "/v1/nosql/databases/{id}/restore", "nosql", D, one(PG, "nosql-database")),
    row("GET", "/v1/nosql/databases/{id}/diff", "nosql", R, one(PG, "nosql-database")),
    row("GET", "/v1/nosql/databases/{id}/imports", "nosql", R, one(PG, "nosql-database")),
    row("POST", "/v1/nosql/databases/{id}/imports", "nosql", D, one(PG, "nosql-database")),
    row("POST", "/v1/nosql/databases/{id}/password", "nosql", REVEAL, one(PG, "nosql-database")),
    row("POST", "/v1/nosql/databases/{id}/cql", "nosql", Act::Body(cql_action), one(PG, "nosql-database")),
    row("GET", "/v1/nosql/databases/{id}/schema", "nosql", R, one(PG, "nosql-database")),
    row("GET", "/v1/nosql/databases/{id}/usage", "nosql", R, one(PG, "nosql-database")),
    row("GET", "/v1/nosql/databases/{id}/alternator", "nosql", REVEAL, one(PG, "nosql-database")),
    row("GET", "/v1/nosql/databases/{id}/activity", "nosql", R, one(PG, "nosql-database")),
    row("POST", "/v1/nosql/databases/{id}/flush", "nosql", W, one(PG, "nosql-database")),
    row("POST", "/v1/nosql/databases/{id}/compact", "nosql", W, one(PG, "nosql-database")),
    row("POST", "/v1/nosql/databases/{id}/unload", "nosql", W, one(PG, "nosql-database")),
    // ── search (feature/roll-search) ──
    row("GET", "/v1/search", "search", R, Tgt::AnyGrant),
    row("GET", "/v1/search/clusters", "search", R, list("id")).reveals(SEARCH_SECRETS),
    row("POST", "/v1/search/clusters", "search", W, create("search", "id")).reveals(SEARCH_SECRETS),
    row("GET", "/v1/search/clusters/{id}", "search", R, one(PG, "cluster")).reveals(SEARCH_SECRETS),
    row("PATCH", "/v1/search/clusters/{id}", "search", ADMIN, one(PG, "cluster")).reveals(SEARCH_SECRETS),
    row("DELETE", "/v1/search/clusters/{id}", "search", D, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/branches", "search", W, one(PG, "cluster")).reveals(SEARCH_SECRETS),
    row("POST", "/v1/search/clusters/{id}/password", "search", REVEAL, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/request", "search", ADMIN, one(PG, "cluster")),
    row("*", "/v1/search/clusters/{id}/", "search", M, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/indices/{index}/search", "search", R, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/indices/{index}/delete_by_query", "search", D, one(PG, "cluster")),
    // ── secrets (feature/roll-secrets, docs/secrets.md) ──
    row("GET", "/v1/secrets/projects/{project}/", "secrets", R, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("GET", "/v1/secrets/projects/{project}", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/", "secrets", W, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("PUT", "/v1/secrets/projects/{project}/", "secrets", W, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("DELETE", "/v1/secrets/projects/{project}/", "secrets", D, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("PATCH", "/v1/secrets/projects/{project}/environments/{env}", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("PUT", "/v1/secrets/projects/{project}/environments/order", "secrets", ADMIN, one(&["project"], "project")),
    row("PUT", "/v1/secrets/projects/{project}/environments/{env}/policy", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/requests/{id}/review", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/values", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/value", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/versions/{v}/value", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/resolve", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    // ── apps (feature/roll-apps, docs/apps/http-api.md) ──
    row("GET", "/v1/apps/services", "apps", R, list("id")),
    row("POST", "/v1/apps/services", "apps", W, Tgt::Product),
    row("POST", "/v1/apps/projects/{project}/services", "apps", W, Tgt::Create { project: Some("project"), place: None, id_field: "id" }),
    row("*", "/v1/apps/services/{id}", "apps", M, one(PG, "service")),
    row("*", "/v1/apps/services/{id}/", "apps", M, one(PG, "service")),
    row("*", "/v1/apps/services/{id}/environments/{env}", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("GET", "/v1/apps/services/{id}/environments/{env}", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}", "apps", D, one(PG, "service")).env(EnvFrom::Param("env")),
    row("GET", "/v1/apps/services/{id}/environments/{env}/", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    row("POST", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("PUT", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("PATCH", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}/domains/{host}", "apps", D, one(PG, "service")).env(EnvFrom::Param("env")),
    row("GET", "/v1/apps/services/{id}/environments/{env}/variables/values", "apps", REVEAL, one(PG, "service")).env(EnvFrom::Param("env")),
    // ── previews (feature/roll-previews, docs/previews.md) ──
    row("GET", "/v1/previews", "previews", R, list("id")),
    row("POST", "/v1/previews", "previews", W, Tgt::Create { project: None, place: None, id_field: "id" }).env(EnvFrom::Fixed("preview")),
    row("*", "/v1/previews/projects/{project}/", "previews", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/previews/projects/{project}/", "previews", R, one(&["project"], "project")),
    row("GET", "/v1/previews/{id}", "previews", R, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("DELETE", "/v1/previews/{id}", "previews", D, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("*", "/v1/previews/{id}/", "previews", M, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("POST", "/v1/previews/{id}/reset", "previews", D, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // ── roll code ──
    row("GET", "/v1/code", "code", R, Tgt::AnyGrant),
    row("GET", "/v1/code/health", "code", R, Tgt::AnyGrant),
    row("GET", "/v1/code/pricing", "code", R, Tgt::AnyGrant),
    row("GET", "/v1/code/names/{name}", "code", R, Tgt::Product),
    row("GET", "/v1/code/names/{owner}/{repo}", "code", R, one(REPO, "repo")),
    row("GET", "/v1/code/repos", "code", R, list("full_name")),
    row("POST", "/v1/code/repos", "code", W, create("repo", "full_name")),
    row("GET", "/v1/code/repos/{owner}/{repo}", "code", R, one(REPO, "repo")),
    row("PATCH", "/v1/code/repos/{owner}/{repo}", "code", ADMIN, one(REPO, "repo")),
    row("DELETE", "/v1/code/repos/{owner}/{repo}", "code", D, one(REPO, "repo")),
    row("*", "/v1/code/repos/{owner}/{repo}/", "code", M, one(REPO, "repo")),
    row("POST", "/v1/code/repos/{owner}/{repo}/land", "code", DEPLOY, one(REPO, "repo")),
    row("POST", "/v1/code/repos/{owner}/{repo}/queue/pause", "code", ADMIN, one(REPO, "repo")),
    row("POST", "/v1/code/repos/{owner}/{repo}/queue/resume", "code", ADMIN, one(REPO, "repo")),
    row("*", "/v1/code/repos/{owner}/{repo}/webhooks", "code", ADMIN, one(REPO, "repo")),
    row("*", "/v1/code/repos/{owner}/{repo}/webhooks/", "code", ADMIN, one(REPO, "repo")),
    row("PATCH", "/v1/code/repos/{owner}/{repo}/reviewer", "code", ADMIN, one(REPO, "repo")),
    row("PATCH", "/v1/code/repos/{owner}/{repo}/review-settings", "code", ADMIN, one(REPO, "repo")),
    row("*", "/v1/code/changes/{id}", "code", M, one(PG, "change")),
    row("*", "/v1/code/changes/{id}/", "code", M, one(PG, "change")),
    row("POST", "/v1/code/changes/{id}/land", "code", DEPLOY, one(PG, "change")),
    row("POST", "/v1/code/changes/{id}/enqueue", "code", DEPLOY, one(PG, "change")),
    row("PATCH", "/v1/code/changes/{id}/enqueue", "code", DEPLOY, one(PG, "change")),
    // Running a fork's code with the repo's data is the repo's owner's call.
    row("POST", "/v1/code/changes/{id}/approve-run", "code", ADMIN, one(PG, "change")),
    row("*", "/v1/code/threads/{id}", "code", M, one(PG, "thread")),
    row("*", "/v1/code/threads/{id}/", "code", M, one(PG, "thread")),
    row("*", "/v1/code/work/{id}", "code", M, one(PG, "work")),
    row("*", "/v1/code/work/{id}/", "code", M, one(PG, "work")),
    row("GET", "/v1/code/owners", "code", R, Tgt::Product),
    row("POST", "/v1/code/owners", "code", ADMIN, Tgt::Product),
    row("GET", "/v1/code/owners/{owner}", "code", R, Tgt::Product),
    row("PATCH", "/v1/code/owners/{owner}", "code", ADMIN, Tgt::Product),
    row("*", "/v1/code/owners/{owner}/", "code", ADMIN, Tgt::Product),
    row("GET", "/v1/code/owners/{owner}/usage", "code", R, Tgt::Product),
    row("*", "/v1/code/review-apps/{id}", "code", ADMIN, Tgt::Product),
    row("*", "/v1/code/review-apps/{id}/", "code", ADMIN, Tgt::Product),
    row("GET", "/v1/code/github", "code", R, Tgt::Product),
    row("GET", "/v1/code/github/installations", "code", ADMIN, Tgt::Product),
    // ── agents (roll code agents, under /v1/code/agents/{owner}) ──
    row("*", "/v1/code/agents/{owner}/", "agents", M, one(&["owner"], "owner")),
    row("POST", "/v1/code/agents/{owner}/machines/{machine}/commands", "agents", Act::Body(agent_command_action), one(&["owner"], "owner")),
    row("POST", "/v1/code/agents/{owner}/login/approve", "agents", ADMIN, one(&["owner"], "owner")),
    row("POST", "/v1/code/agents/{owner}/login/deny", "agents", ADMIN, one(&["owner"], "owner")),
    row("*", "/v1/code/agents/{owner}/endpoints/{id}", "agents", ADMIN, one(&["owner"], "owner")),
    row("*", "/v1/code/agents/{owner}/gateway", "agents", ADMIN, one(&["owner"], "owner")),
    row("GET", "/v1/code/agents/{owner}/gateway", "agents", R, one(&["owner"], "owner")),
    row("*", "/v1/code/agents/{owner}/gateway/", "agents", ADMIN, one(&["owner"], "owner")),
    row("*", "/v1/code/agents/{owner}/integrations/", "agents", ADMIN, one(&["owner"], "owner")),
    row("*", "/v1/code/agents/{owner}/linear", "agents", ADMIN, one(&["owner"], "owner")),
    row("GET", "/v1/code/agents/{owner}/linear", "agents", R, one(&["owner"], "owner")),
    row("*", "/v1/code/agents/{owner}/linear/", "agents", ADMIN, one(&["owner"], "owner")),
    row("PUT", "/v1/code/agents/{owner}/settings", "agents", ADMIN, one(&["owner"], "owner")),
    row("*", "/v1/code/agents/{owner}/automations/{id}/secrets/{name}", "agents", ADMIN, one(&["owner"], "owner")),
    row("POST", "/v1/code/agents/{owner}/automations/{id}/webhook/secret", "agents", REVEAL, one(&["owner"], "owner")),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Exact rows beat prefixes, the longest prefix wins, and a body decides SQL, Redis and CQL.
    #[test]
    fn rows_and_bodies() {
        let action = |m: &str, route: &str, path: &str| find(m, route, path).map(|f| f.row.action(m, &f.params.unwrap_or_default(), None));
        assert_eq!(action("GET", "/v1/workspace/projects", "/v1/workspace/projects"), Some(Action::Read));
        assert!(find("POST", "/v1/code/mcp", "/v1/code/mcp").is_none());
        // A specific method beats "*"; the longest prefix wins.
        assert_eq!(action("GET", "/v1/storage/accounts", "/v1/storage/accounts"), Some(Action::Read));
        assert_eq!(action("POST", "/v1/code/changes/{id}/land", "/v1/code/changes/chg_1/land"), Some(Action::Deploy));
        assert_eq!(action("GET", "/v1/code/repos/{owner}/{repo}/blob/{rev}/{*path}", "/v1/code/repos/a/b/blob/main/x.rs"), Some(Action::Read));
        // Catch-alls: the real path decides, with its parameters.
        let f = find("POST", "/v1/tasks/{*rest}", "/v1/tasks/queues/jobs/pause").unwrap();
        assert_eq!((f.row.route, f.row.action("POST", &[], None)), ("/v1/tasks/queues/{name}/", Action::Write));
        let f = find("DELETE", "/v1/storage/{*__private__axum_nest_tail_param}", "/v1/storage/buckets/assets/object").unwrap();
        assert_eq!(f.params.unwrap(), vec![("name".to_string(), "assets".to_string())]);
        assert!(find("GET", "/v1/tasks/{*rest}", "/v1/tasks/nothing/here/at/all").is_none());
        // Static segments beat parameters; production decides deploy.
        let env = find("PUT", "/v1/secrets/projects/{project}/environments/order", "x").unwrap();
        assert_eq!(env.row.action("PUT", &[], None), Action::Admin);
        let dep = find("POST", "/v1/apps/services/{id}/environments/{env}/deploys", "x").unwrap();
        assert_eq!(dep.row.action("POST", &[("env".into(), "production".into())], None), Action::Deploy);
        assert_eq!(dep.row.action("POST", &[("env".into(), "staging".into())], None), Action::Write);
        assert_eq!(sql_action(&serde_json::json!({ "sql": "select 1", "read_only": true })), Action::Read);
        assert_eq!(sql_action(&serde_json::json!({ "sql": "select 1" })), Action::Write);
        assert_eq!(redis_action(&serde_json::json!({ "command": ["get", "k"] })), Action::Read);
        assert_eq!(redis_action(&serde_json::json!({ "command": ["FLUSHALL"] })), Action::Delete);
        assert_eq!(redis_action(&serde_json::json!({ "command": ["set", "k", "v"] })), Action::Write);
        assert_eq!(cql_action(&serde_json::json!({ "query": "SELECT * FROM t; DROP TABLE t" })), Action::Delete);
        assert_eq!(cql_action(&serde_json::json!({ "query": "" })), Action::Write);
        let mut routes: Vec<_> = ROWS.iter().map(|r| (r.method, r.route)).collect();
        let n = routes.len();
        routes.sort();
        routes.dedup();
        assert_eq!(routes.len(), n, "a route and method has two rows");
    }
}
