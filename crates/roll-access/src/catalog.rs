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
    /// Decided from the raw body (NDJSON: search's bulk).
    Raw(fn(&[u8]) -> Action),
    /// A git push: from the ref updates at the start of its body (the evaluator reads them):
    /// `deploy` onto the default branch, `delete` for a ref deleted, else `write`.
    GitPush,
}

/// What the request is about.
#[derive(Clone, Copy, Debug)]
pub enum Tgt {
    /// One resource, named by these path parameters joined with `/` (`owner`, `repo`), of `kind`
    /// (a [`crate::Resolver`] may map it to the resource rules name, e.g. a branch to its project).
    Param(&'static [&'static str], &'static str),
    /// A list of the product's resources: items are filtered by `id_field` (in `items`, or the
    /// answer itself when it's an array) for rules that name resources.
    List { items: Option<&'static str>, id_field: &'static str, kind: Option<&'static str> },
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
    /// Where the request names another environment of the same resource whose values it copies
    /// (apps' `variables_from`, secrets' promote `?from=`): it also needs `reveal` there.
    pub copies_from: Option<CopyFrom>,
    /// The path parameter naming indices of the resource (search's `{index}`): rules can grant
    /// some indices of it (`{"id": "cl_1", "index": "logs-*"}`).
    pub index: Option<&'static str>,
    /// The product answers only what the caller's grant reaches per environment and path (secrets'
    /// health and leak scan): rules limited to some environments or paths match it too.
    pub filters_itself: bool,
    /// Other resources whose credentials the request hands to this one (apps' links put a
    /// database's address and password in a service's variables): it also needs `reveal` on each.
    pub lends: Option<Lend>,
}

/// Where a request names the resources it links ([`Row::lends`]), as `(kind, ref)`.
#[derive(Clone, Copy, Debug)]
pub enum Lend {
    /// Two path parameters: the kind and the ref.
    Path(&'static str, &'static str),
    /// A top-level JSON array of `{kind, ref}`.
    Body(&'static str),
}

/// What an apps link's kind is, as a rule names it: its product and the resource kind its ids are.
pub fn link_target(kind: &str) -> Option<(&'static str, &'static str)> {
    Some(match kind {
        "postgres" => ("postgres", "postgres-project"),
        "redis" => ("redis", "database"),
        "bucket" => ("storage", "bucket"),
        "stream" => ("streams", "stream"),
        "queue" => ("queues", "vhost"),
        "nosql" => ("nosql", "nosql-database"),
        _ => return None,
    })
}

/// Where a copying request names the environment it copies from.
#[derive(Clone, Copy, Debug)]
pub enum CopyFrom {
    /// A top-level string field of the JSON body.
    Body(&'static str),
    /// A query parameter.
    Query(&'static str),
    /// Each `(environment, path)` the JSON body lists (`None`: the request's own path).
    Each(fn(&Value) -> Sources),
}

/// Environments (and paths inside them, `None`: the request's own) a request copies from.
pub type Sources = Vec<(String, Option<String>)>;

impl Row {
    pub fn needs_body(&self) -> bool {
        matches!(self.act, Act::Body(_) | Act::Raw(_)) || matches!(self.copies_from, Some(CopyFrom::Body(_) | CopyFrom::Each(_))) || matches!(self.lends, Some(Lend::Body(_)))
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
            // Without the bytes (a dry run), the strongest it can be; the evaluator reads them.
            Act::Raw(_) => Action::Delete,
            // The strongest it can be; the evaluator reads its commands (`git_push`).
            Act::GitPush => Action::Deploy,
        }
    }
}

const fn row(method: &'static str, route: &'static str, product: &'static str, act: Act, target: Tgt) -> Row {
    Row { method, route, product, act, target, env: EnvFrom::None, path_query: None, reveals: &[], copies_from: None, index: None, lends: None, filters_itself: false }
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
    Tgt::List { items: None, id_field, kind: None }
}
/// A list whose items are resources of `kind` (tasks' queues, topics, …).
const fn list_of(kind: &'static str, id_field: &'static str) -> Tgt {
    Tgt::List { items: None, id_field, kind: Some(kind) }
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
    /// A route that keeps only what the caller's `Grant` allows in each answer (secrets' health
    /// and leaks): a rule limited to some environments or folders matches it too.
    const fn filters_itself(mut self) -> Row {
        self.filters_itself = true;
        self
    }
    const fn lends(mut self, l: Lend) -> Row {
        self.lends = Some(l);
        self
    }
    const fn index(mut self, param: &'static str) -> Row {
        self.index = Some(param);
        self
    }
    const fn copies_from(mut self, from: CopyFrom) -> Row {
        self.copies_from = Some(from);
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
    ("/v1/access/notify", "where approvals go is the operator's choice"),
    ("/v1/access/policy", "the rules for every token are the operator's"),
    ("/v1/access/audit/sinks", "where the audit log is sent is the operator's choice"),
    ("/v1/access/audit/sinks/{id}", "where the audit log is sent is the operator's choice"),
    ("/v1/access/audit/sinks/{id}/retry", "where the audit log is sent is the operator's choice"),
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
/// The catalogue by shape, built once: every request looks its route up here.
struct Index {
    exact: std::collections::HashMap<String, Vec<&'static Row>>,
    prefixes: Vec<(String, &'static Row)>,
    operator_only: std::collections::HashSet<String>,
}

static INDEX: std::sync::LazyLock<Index> = std::sync::LazyLock::new(|| {
    let mut exact: std::collections::HashMap<String, Vec<&'static Row>> = std::collections::HashMap::new();
    let mut prefixes = Vec::new();
    for r in ROWS {
        if r.route.ends_with('/') {
            prefixes.push((shape(r.route), r));
        } else {
            exact.entry(shape(r.route)).or_default().push(r);
        }
    }
    Index { exact, prefixes, operator_only: OPERATOR_ONLY.iter().map(|(r, _)| shape(r)).collect() }
});

pub fn find(method: &str, route: &str, path: &str) -> Option<Found> {
    // Patterns are compared by shape: a product naming a parameter `{key}` where its row says
    // `{name}` still matches its row (a rename must never change an action).
    let shape = shape(route);
    if INDEX.operator_only.contains(&shape) {
        return None;
    }
    let exact = INDEX.exact.get(&shape).into_iter().flatten().filter_map(|r| method_rank(r, method).map(|m| (m, *r))).max_by_key(|(m, _)| *m);
    if let Some((_, row)) = exact {
        return Some(Found { row, params: None });
    }
    let prefix = INDEX
        .prefixes
        .iter()
        .filter(|(s, _)| shape.starts_with(s.as_str()))
        .filter_map(|(_, r)| method_rank(r, method).map(|m| ((r.route.len(), m), *r)))
        .max_by_key(|(k, _)| *k);
    if let Some((_, row)) = prefix {
        return Some(Found { row, params: None });
    }
    // A nested service (storage) has no matched route at all: its path decides.
    let nested = route == "unmatched" && NESTS.iter().any(|n| path == *n || path.starts_with(&format!("{n}/")));
    let forwarded = FORWARDS.iter().any(|(r, _)| *r == route);
    if !route.contains("{*") && !is_nest(route) && !nested && !forwarded {
        return None;
    }
    by_path(method, path)
}

/// A workspace membership change (`PUT`/`DELETE …/projects/{id}/resources/{kind}/{ref}`): the
/// product and id of the resource being moved, which the evaluator also checks.
pub fn placement(row: &Row, params: &[(String, String)]) -> Option<(&'static str, String)> {
    if row.route != "/v1/workspace/projects/{id}/resources/{kind}/{ref}" {
        return None;
    }
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let product = match get("kind")?.as_str() {
        "postgres" => "postgres",
        "redis" => "redis",
        "stream" => "streams",
        "bucket" => "storage",
        "nosql" => "nosql",
        "repo" => "code",
        "queue" => "queues",
        "tasks" => "tasks",
        "search" => "search",
        "service" => "apps",
        // An unknown kind: the workspace refuses it anyway; check it as the workspace's own.
        _ => "workspace",
    };
    Some((product, get("ref")?))
}

/// Signed hops a node forwards to a product's owner or leader after its own gate (storage's
/// metadata owner, the tasks cell's leader), and the header with the client's original path and
/// query. The route only takes signed requests (`roll_cluster::require_signed`, after the gate);
/// the gate here decides the original request again, on this node.
pub const FORWARDS: &[(&str, &str)] = &[("/internal/storage/control", "x-roll-storage-uri"), ("/internal/tasks", "x-roll-tasks-uri")];

/// A route pattern with its parameters' names dropped (`/a/{id}/{*rest}` → `/a/{}/{*}`).
pub fn shape(route: &str) -> String {
    route
        .split('/')
        .map(|seg| match seg.strip_prefix('{') {
            Some(rest) if rest.starts_with('*') => "{*}",
            Some(_) => "{}",
            None => seg,
        })
        .collect::<Vec<_>>()
        .join("/")
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
    if OPERATOR_ONLY.iter().any(|(r, _)| shape(r) == shape(route)) {
        return Some("operator only");
    }
    let shape = shape(route);
    if let Some(r) = ROWS.iter().find(|r| self::shape(r.route) == shape) {
        return Some(r.product);
    }
    if let Some(r) = ROWS.iter().filter(|r| r.route.ends_with('/') && shape.starts_with(&self::shape(r.route))).max_by_key(|r| r.route.len()) {
        return Some(r.product);
    }
    if FORWARDS.iter().any(|(r, _)| *r == route) {
        return Some("by path");
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
        Act::Body(_) | Act::Raw(_) => "from the body".into(),
        Act::GitPush => "deploy onto the default branch, delete for a deleted ref, else write".into(),
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

/// Statement text without comments (`--`, `//` and `/* */`), so a comment can't hide what runs.
fn uncommented(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            out.push(c);
            if c == q {
                quote = None;
            }
            continue;
        }
        match (c, chars.peek()) {
            ('\'', _) | ('"', _) => {
                quote = Some(c);
                out.push(c);
            }
            ('-', Some('-')) | ('/', Some('/')) => {
                for n in chars.by_ref() {
                    if n == '\n' {
                        break;
                    }
                }
                out.push(' ');
            }
            ('/', Some('*')) => {
                chars.next();
                let mut prev = ' ';
                for n in chars.by_ref() {
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Statements that make or change database credentials and privileges: `admin`, whatever else
/// the body does (a login made in SQL would outlive the token and skip `reveal`).
fn grants_access(text: &str) -> bool {
    let t = uncommented(text).to_ascii_lowercase();
    let words: Vec<&str> = t.split(|c: char| !c.is_ascii_alphanumeric() && c != '_').filter(|w| !w.is_empty()).collect();
    words.windows(2).any(|w| matches!(w[0], "create" | "alter" | "drop") && matches!(w[1], "role" | "user"))
        || words.iter().any(|w| matches!(*w, "grant" | "revoke" | "superuser"))
}

/// Postgres SQL: `read_only: true` reads (Postgres enforces it: the SQL runs as a role that can
/// only read); anything else writes, and SQL that touches roles or privileges is `admin`.
fn sql_action(b: &Value) -> Action {
    let sql = b.get("sql").and_then(Value::as_str).unwrap_or("");
    if b.get("read_only").and_then(Value::as_bool) == Some(true) {
        Action::Read
    } else if grants_access(sql) {
        Action::Admin
    } else {
        Action::Write
    }
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
        // A script can run anything, flushes included: the strongest a command can be.
        "FLUSHDB" | "FLUSHALL" | "EVAL" | "EVALSHA" | "EVAL_RO" | "EVALSHA_RO" | "FCALL" | "FCALL_RO" | "FUNCTION" | "SCRIPT" => Action::Delete,
        "ACL" | "CONFIG" | "AUTH" | "HELLO" | "MIGRATE" | "MODULE" | "DEBUG" | "SHUTDOWN" | "REPLICAOF" | "SLAVEOF" => Action::Admin,
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
    let raw = b.get("query").or_else(|| b.get("cql")).or_else(|| b.get("statement")).and_then(Value::as_str).unwrap_or("");
    if grants_access(raw) {
        return Action::Admin;
    }
    let text = uncommented(raw);
    let mut out = Action::Read;
    let mut any = false;
    for stmt in text.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        any = true;
        let first = stmt.split_whitespace().next().unwrap_or("").to_ascii_uppercase();
        let a = match first.as_str() {
            "SELECT" | "DESCRIBE" | "DESC" | "USE" => Action::Read,
            "LIST" => Action::Admin,
            "DROP" | "TRUNCATE" => Action::Delete,
            _ => Action::Write,
        };
        out = out.max(a);
    }
    if any { out } else { Action::Write }
}

/// Agents' machine commands, by what they can do on the machine (`policy::agent_may`'s tiers):
/// reading a thread's diff or files is `read`; working threads is `write`; publishing with `land`
/// ships (`deploy`); deleting a thread `delete`; and a shell, writing files, rolling back, raising a
/// thread's runtime or interaction mode, answering an agent's approval (it would approve itself),
/// endpoints, the gateway, projects and the machine's settings are `admin`. Unknown types too.
/// An Elasticsearch bulk (NDJSON): `delete` when an action line deletes or an update runs a script
/// (`ctx.op = 'delete'` deletes), or when a line isn't JSON (it fails closed); else `write`.
fn bulk_action(b: &[u8]) -> Action {
    for line in b.split(|c| *c == b'\n').filter(|l| !l.iter().all(u8::is_ascii_whitespace)) {
        let Ok(v) = serde_json::from_slice::<Value>(line) else { return Action::Delete };
        let Some(o) = v.as_object() else { return Action::Delete };
        if o.contains_key("delete") || o.contains_key("script") {
            return Action::Delete;
        }
    }
    Action::Write
}

/// Secrets' imports (`[{project?, environment, path}]`): importing another project's folder is
/// admin on the importing folder (that project's admin exports it too); in-project ones are write,
/// plus reveal on each source ([`secrets_import_sources`]).
fn secrets_imports_action(b: &Value) -> Action {
    let list = b.as_array().map(Vec::as_slice).unwrap_or_default();
    if list.iter().any(|i| i.get("project").is_some_and(|p| !p.is_null())) { Action::Admin } else { Action::Write }
}

/// The in-project folders an imports body reads from: every reader of the importing folder will
/// read them.
fn secrets_import_sources(b: &Value) -> Sources {
    let list = b.as_array().map(Vec::as_slice).unwrap_or_default();
    list.iter()
        .filter(|i| i.get("project").is_none_or(Value::is_null))
        .filter_map(|i| Some((i.get("environment")?.as_str()?.to_string(), i.get("path").and_then(Value::as_str).map(str::to_string))))
        .collect()
}

/// A new apps notification: a webhook (the default) answers its signing secret once, so `reveal`;
/// a Slack or Discord one answers none, so `write`.
fn apps_notification_action(b: &Value) -> Action {
    match b["kind"].as_str() {
        Some("slack" | "discord") => Action::Write,
        _ => Action::Reveal,
    }
}

/// A new apps service: `deploy` unless its source is an upload (which deploys only with `roll up`).
fn apps_create_action(b: &Value) -> Action {
    if b["source"]["kind"] == "upload" { Action::Write } else { Action::Deploy }
}

fn agent_command_action(b: &Value) -> Action {
    let kind = b.get("type").and_then(Value::as_str).unwrap_or("");
    let raises = b.get("runtime_mode").is_some_and(|m| !m.is_null()) || b.get("interaction_mode").is_some_and(|m| !m.is_null());
    match kind {
        "diff.get" | "git.status" | "fs.read" | "terminal.scrollback" => Action::Read,
        "fs.list" if b.get("project").is_some_and(|p| !p.is_null()) => Action::Read,
        "thread.publish" if b.get("land").and_then(Value::as_bool) == Some(true) => Action::Deploy,
        "thread.delete" => Action::Delete,
        "thread.create" if b.get("runtime_mode").and_then(Value::as_str) == Some("full_access") => Action::Admin,
        "thread.configure" if raises => Action::Admin,
        "thread.create" | "thread.configure" | "thread.fork" | "thread.archive" | "thread.unarchive" | "thread.settle" | "thread.unsettle" | "thread.snooze"
        | "thread.unsnooze" | "thread.visit" | "thread.auto_settle" | "thread.publish" | "message.send" | "message.unqueue" | "message.edit" | "message.reorder"
        | "message.promote" | "run.interrupt" => Action::Write,
        _ => Action::Admin,
    }
}

/// Storage's IAM JSON API: reads and simulations are `read`, everything else `admin`.
fn iam_action(b: &Value) -> Action {
    let a = b.get("Action").and_then(Value::as_str).unwrap_or("");
    if ["Get", "List", "Simulate", "Generate"].iter().any(|p| a.starts_with(p)) { Action::Read } else { Action::Admin }
}

// Credentials in answers, per product: replaced unless the grant may reveal.
const PG_SECRETS: &[&str] = &["connection_uri", "app_connection_uri", "password"];
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
    row("POST", "/v1/access/check", "access", R, Tgt::AnyGrant),
    row("GET", "/v1/access/tokens", "access", R, Tgt::Product),
    row("POST", "/v1/access/tokens", "access", ADMIN, Tgt::Product),
    row("GET", "/v1/access/tokens/{id}", "access", R, Tgt::Product),
    row("PATCH", "/v1/access/tokens/{id}", "access", ADMIN, Tgt::Product),
    row("DELETE", "/v1/access/tokens/{id}", "access", ADMIN, Tgt::Product),
    row("POST", "/v1/access/tokens/{id}/rotate", "access", ADMIN, Tgt::Product),
    row("GET", "/v1/access/tokens/{id}/tighten", "access", R, Tgt::Product),
    row("GET", "/v1/access/tokens/{id}/networks", "access", R, Tgt::Product),
    row("GET", "/v1/access/checkup", "access", R, Tgt::Product),
    row("POST", "/v1/access/tokens/{id}/tighten", "access", ADMIN, Tgt::Product),
    row("DELETE", "/v1/access/tokens/{id}/temporary/{approval}", "access", ADMIN, Tgt::Product),
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
    row("GET", "/v1/projects/{id}", "postgres", R, one(PG, "postgres-project")).reveals(PG_SECRETS),
    row("PATCH", "/v1/projects/{id}", "postgres", W, one(PG, "postgres-project")).reveals(PG_SECRETS),
    row("DELETE", "/v1/projects/{id}", "postgres", D, one(PG, "postgres-project")),
    row("POST", "/v1/projects/{id}/branches", "postgres", W, one(PG, "postgres-project")).reveals(PG_SECRETS),
    row("DELETE", "/v1/projects/{id}/branches/{branch}", "postgres", D, one(PG, "postgres-project")),
    // A branch's own app role (feature/roll-previews): its answer has the role's password.
    row("POST", "/v1/projects/{id}/branches/{branch}/app-role", "postgres", REVEAL, one(PG, "postgres-project")),
    row("GET", "/v1/projects/{id}/branches/{branch}/history", "postgres", R, one(PG, "postgres-project")),
    // Rewinds the branch in place (the old state is kept as a branch, but clients see it gone).
    row("POST", "/v1/projects/{id}/branches/{branch}/restore", "postgres", D, one(PG, "postgres-project")),
    // Re-branches from the parent in place (roll previews): the branch's own writes are gone.
    row("POST", "/v1/projects/{id}/branches/{branch}/reset", "postgres", D, one(PG, "postgres-project")),
    row("POST", "/v1/projects/{id}/branches/{branch}/backup", "postgres", W, one(PG, "postgres-project")),
    row("POST", "/v1/projects/{id}/branches/{branch}/evict", "postgres", W, one(PG, "postgres-project")),
    row("POST", "/v1/projects/{id}/branches/{branch}/replicas", "postgres", W, one(PG, "postgres-project")).reveals(PG_SECRETS),
    row("GET", "/v1/projects/{id}/usage", "postgres", R, one(PG, "postgres-project")),
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
    row("GET", "/v1/tasks/queues", "tasks", R, list_of("tasks-queue", "name")),
    row("GET", "/v1/tasks/queues/{name}", "tasks", R, one(&["name"], "tasks-queue")),
    row("PUT", "/v1/tasks/queues/{name}", "tasks", W, one(&["name"], "tasks-queue")),
    row("DELETE", "/v1/tasks/queues/{name}", "tasks", D, one(&["name"], "tasks-queue")),
    row("POST", "/v1/tasks/queues/{name}/", "tasks", W, one(&["name"], "tasks-queue")),
    row("GET", "/v1/tasks/topics", "tasks", R, list_of("tasks-topic", "name")),
    row("GET", "/v1/tasks/topics/{name}", "tasks", R, one(&["name"], "tasks-topic")),
    row("PUT", "/v1/tasks/topics/{name}", "tasks", W, one(&["name"], "tasks-topic")),
    row("DELETE", "/v1/tasks/topics/{name}", "tasks", D, one(&["name"], "tasks-topic")),
    row("POST", "/v1/tasks/topics/{name}/endpoints", "tasks", W, one(&["name"], "tasks-topic")),
    row("DELETE", "/v1/tasks/topics/{name}/endpoints", "tasks", W, one(&["name"], "tasks-topic")),
    row("GET", "/v1/tasks/schedules", "tasks", R, list_of("tasks-schedule", "id")),
    row("POST", "/v1/tasks/schedules", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/schedules/{id}", "tasks", R, one(&["id"], "tasks-schedule")),
    row("PATCH", "/v1/tasks/schedules/{id}", "tasks", W, one(&["id"], "tasks-schedule")),
    row("DELETE", "/v1/tasks/schedules/{id}", "tasks", D, one(&["id"], "tasks-schedule")),
    row("POST", "/v1/tasks/schedules/{id}/", "tasks", W, one(&["id"], "tasks-schedule")),
    row("GET", "/v1/tasks/events", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/events", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/events/{id}", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/e/{*name}", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/apps", "tasks", R, Tgt::Product),
    row("POST", "/v1/tasks/apps", "tasks", W, Tgt::Product),
    row("DELETE", "/v1/tasks/apps/{id}", "tasks", D, Tgt::Product),
    row("GET", "/v1/tasks/functions", "tasks", R, list_of("tasks-function", "id")),
    row("GET", "/v1/tasks/functions/{id}", "tasks", R, one(&["id"], "tasks-function")),
    row("POST", "/v1/tasks/functions/{id}/", "tasks", W, one(&["id"], "tasks-function")),
    row("POST", "/v1/tasks/functions/{id}/cancel", "tasks", D, one(&["id"], "tasks-function")),
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
    // An API key: its secret is in the answer.
    row("POST", "/v1/search/clusters/{id}/keys", "search", REVEAL, one(PG, "cluster")),
    row("DELETE", "/v1/search/clusters/{id}/keys/{key}", "search", ADMIN, one(PG, "cluster")),
    row("*", "/v1/search/clusters/{id}/", "search", M, one(PG, "cluster")),
    // An index's routes are about those indices (`{index}`: a name, or on reads an expression),
    // so a rule can grant some of a cluster's indices. The console (`request`) and the index list
    // stay the cluster's: they reach every index.
    row("*", "/v1/search/clusters/{id}/indices/{index}", "search", M, one(PG, "cluster")).index("index"),
    row("*", "/v1/search/clusters/{id}/indices/{index}/", "search", M, one(PG, "cluster")).index("index"),
    row("POST", "/v1/search/clusters/{id}/indices/{index}/search", "search", R, one(PG, "cluster")).index("index"),
    row("POST", "/v1/search/clusters/{id}/indices/{index}/delete_by_query", "search", D, one(PG, "cluster")).index("index"),
    // A bulk can delete as well as index: its action lines decide.
    row("POST", "/v1/search/clusters/{id}/indices/{index}/bulk", "search", Act::Raw(bulk_action), one(PG, "cluster")).index("index"),
    // ── secrets (feature/roll-secrets, docs/secrets.md) ──
    // The caller and its grant's project (`roll run`), and health: any grant.
    row("GET", "/v1/secrets/whoami", "secrets", R, Tgt::AnyGrant),
    row("GET", "/v1/secrets/health", "secrets", R, Tgt::AnyGrant),
    // Search across projects: secrets keeps each project's results only where the token may read
    // (`Grant::allows`, one check per project inside the request).
    // Across projects: until secrets filters by the caller's `Grant` (its swap makes these
    // `AnyGrant`), it would answer any token as the operator, so only a rule for all of secrets.
    row("GET", "/v1/secrets/search", "secrets", R, Tgt::AnyGrant),
    row("GET", "/v1/secrets/findings", "secrets", R, Tgt::AnyGrant),
    row("GET", "/v1/secrets/projects/{project}/", "secrets", R, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("GET", "/v1/secrets/projects/{project}", "secrets", R, one(&["project"], "project")),
    // How long a project keeps old values (history_days): purging is the project admin's.
    row("PATCH", "/v1/secrets/projects/{project}", "secrets", ADMIN, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/", "secrets", W, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("PUT", "/v1/secrets/projects/{project}/", "secrets", W, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("DELETE", "/v1/secrets/projects/{project}/", "secrets", D, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("PATCH", "/v1/secrets/projects/{project}/environments/{env}", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("PUT", "/v1/secrets/projects/{project}/environments/order", "secrets", ADMIN, one(&["project"], "project")),
    // Imports: write, reveal on each in-project source, admin for another project's folder.
    row("PUT", "/v1/secrets/projects/{project}/environments/{env}/imports", "secrets", Act::Body(secrets_imports_action), one(&["project"], "project"))
        .env(EnvFrom::Param("env"))
        .path_query("path")
        .copies_from(CopyFrom::Each(secrets_import_sources)),
    // Promote copies the selected keys' values from `?from=` into `{env}`: reveal there too.
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/promote", "secrets", W, one(&["project"], "project"))
        .env(EnvFrom::Param("env"))
        .path_query("path")
        .copies_from(CopyFrom::Query("from")),
    // Findings across every environment (expired, missing, exposed, weak, …), names only: read on
    // the project. Secrets keeps only what the caller may read (weak ones: may reveal).
    // Not `filters_itself` yet: secrets checks each finding against the caller's `Grant` only once
    // it reads it (the swap on wip/secrets-on-mcp); until then an environment-limited token
    // would get every environment's findings. The swap marks both.
    row("GET", "/v1/secrets/projects/{project}/health", "secrets", R, one(&["project"], "project")).filters_itself(),
    // Which candidates are this project's stored values (`roll secrets scan`): read on the
    // project. Secrets keeps only matches in folders the caller may reveal, so it's no oracle.
    row("POST", "/v1/secrets/projects/{project}/leaks", "secrets", R, one(&["project"], "project")).filters_itself(),
    // Rotators: URLs roll calls to issue and revoke values that live outside roll. Listing shows
    // names and hosts; adding one answers its signing secret once; both changes are admin.
    row("GET", "/v1/secrets/projects/{project}/rotators", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/rotators", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/rotators/{id}", "secrets", ADMIN, one(&["project"], "project")),
    // A backup seals every environment's values to a key the caller chooses; a restore loads a
    // whole project. Both are admin in secrets too.
    row("POST", "/v1/secrets/projects/{project}/backup", "secrets", ADMIN, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/restore", "secrets", ADMIN, one(&["project"], "project")),
    row("PUT", "/v1/secrets/projects/{project}/environments/{env}/policy", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/requests/{id}/review", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/values", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/value", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/versions/{v}/value", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    // What hands out more than the request itself: secrets' own tokens, syncs (they push values
    // elsewhere), audit sinks and required keys are admin; a share link of a value reveals it.
    row("*", "/v1/secrets/projects/{project}/tokens", "secrets", ADMIN, one(&["project"], "project")),
    row("*", "/v1/secrets/projects/{project}/tokens/", "secrets", ADMIN, one(&["project"], "project")),
    row("*", "/v1/secrets/projects/{project}/syncs", "secrets", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/secrets/projects/{project}/syncs", "secrets", R, one(&["project"], "project")),
    row("*", "/v1/secrets/projects/{project}/syncs/", "secrets", ADMIN, one(&["project"], "project")),
    row("*", "/v1/secrets/projects/{project}/audit/sinks", "secrets", ADMIN, one(&["project"], "project")),
    row("*", "/v1/secrets/projects/{project}/audit/sinks/", "secrets", ADMIN, one(&["project"], "project")),
    row("PUT", "/v1/secrets/projects/{project}/required", "secrets", ADMIN, one(&["project"], "project")),
    // Identities mint tokens, as `tokens` does: seeing or changing them is the project admin's.
    row("*", "/v1/secrets/projects/{project}/identities", "secrets", ADMIN, one(&["project"], "project")),
    row("*", "/v1/secrets/projects/{project}/identities/", "secrets", ADMIN, one(&["project"], "project")),
    // Renewing a lease extends a credential whoever could read it holds.
    row("POST", "/v1/secrets/projects/{project}/leases/{id}/renew", "secrets", REVEAL, one(&["project"], "project")),
    // Exporting an environment's folder shares it with another project.
    row("PUT", "/v1/secrets/projects/{project}/environments/{env}/exports", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("POST", "/v1/secrets/projects/{project}/shares", "secrets", REVEAL, one(&["project"], "project")),
    // Purging a secret's history can't be undone.
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/purge", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    // A change request's current and proposed values, for its approvers.
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/requests/{id}/values", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/resolve", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    // ── apps (feature/roll-apps, docs/apps/http-api.md) ──
    row("GET", "/v1/apps/services", "apps", R, list("id")),
    // A service from a repo or an image deploys production at once; an upload one waits for `roll up`.
    // Its `links` hand each linked resource's credentials to the service: reveal on each too.
    row("POST", "/v1/apps/services", "apps", Act::Body(apps_create_action), Tgt::Product).env(EnvFrom::Fixed("production")).lends(Lend::Body("links")),
    row("POST", "/v1/apps/projects/{project}/services", "apps", Act::Body(apps_create_action), Tgt::Create { project: Some("project"), place: None, id_field: "id" })
        .env(EnvFrom::Fixed("production"))
        .lends(Lend::Body("links")),
    row("GET", "/v1/apps/events", "apps", R, Tgt::Product),
    // A project's notifications: the URL is stored sealed and never answered; a webhook's signing
    // secret comes back once, so making one is reveal (Slack and Discord ones are write).
    // A project's spend limits: at the hard limit the whole project pauses, so setting one is admin.
    row("GET", "/v1/apps/projects/{project}/limits", "apps", R, one(&["project"], "project")),
    row("PUT", "/v1/apps/projects/{project}/limits", "apps", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/apps/projects/{project}/notifications", "apps", R, one(&["project"], "project")),
    row("POST", "/v1/apps/projects/{project}/notifications", "apps", Act::Body(apps_notification_action), one(&["project"], "project")),
    row("DELETE", "/v1/apps/projects/{project}/notifications/{nid}", "apps", W, one(&["project"], "project")),
    row("POST", "/v1/apps/projects/{project}/notifications/{nid}/test", "apps", W, one(&["project"], "project")),
    row("GET", "/v1/apps/projects/{project}/notifications/{nid}/deliveries", "apps", R, one(&["project"], "project")),
    // A project's log drains: every service's output POSTed out of roll, signed. Adding one sends
    // runtime output elsewhere and its signing secret comes back once: reveal. The URL is never
    // answered (only its host).
    row("GET", "/v1/apps/projects/{project}/drains", "apps", R, one(&["project"], "project")),
    row("POST", "/v1/apps/projects/{project}/drains", "apps", REVEAL, one(&["project"], "project")),
    row("DELETE", "/v1/apps/projects/{project}/drains/{did}", "apps", W, one(&["project"], "project")),
    // A private registry's login (never answered back), and unlinking a database: settings.
    row("PUT", "/v1/apps/services/{id}/registry", "apps", W, one(PG, "service")),
    row("DELETE", "/v1/apps/services/{id}/registry", "apps", W, one(PG, "service")),
    // A link puts the linked resource's address and password in the service's variables, where
    // its code reads them: reveal on that resource too.
    row("PUT", "/v1/apps/services/{id}/links/{kind}/{ref}", "apps", W, one(PG, "service")).lends(Lend::Path("kind", "ref")),
    row("DELETE", "/v1/apps/services/{id}/links/{kind}/{ref}", "apps", W, one(PG, "service")),
    row("*", "/v1/apps/services/{id}", "apps", M, one(PG, "service")),
    row("*", "/v1/apps/services/{id}/", "apps", M, one(PG, "service")),
    // `variables_from` copies another environment's values into this one: reveal there too.
    row("*", "/v1/apps/services/{id}/environments/{env}", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")).copies_from(CopyFrom::Body("variables_from")),
    row("GET", "/v1/apps/services/{id}/environments/{env}", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}", "apps", D, one(PG, "service")).env(EnvFrom::Param("env")),
    row("GET", "/v1/apps/services/{id}/environments/{env}/", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    row("POST", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("PUT", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("PATCH", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}/domains/{host}", "apps", D, one(PG, "service")).env(EnvFrom::Param("env")),
    // A volume's backups: making one changes nothing the app sees (write, production too);
    // restoring one replaces the volume's files (delete, as restoring over a branch is).
    row("POST", "/v1/apps/services/{id}/environments/{env}/volume/backups", "apps", W, one(PG, "service")).env(EnvFrom::Param("env")),
    row("POST", "/v1/apps/services/{id}/environments/{env}/volume/backups/{bid}/restore", "apps", D, one(PG, "service")).env(EnvFrom::Param("env")),
    // A shell runs any command where the service runs, with its variables (`env` prints them all):
    // admin in every environment, as an agent's shell is: its WebSocket, and all of a session
    // (opening, keys, screen, resizing, closing: what may close an admin's shell is admin too).
    row("GET", "/v1/apps/services/{id}/environments/{env}/shell", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("POST", "/v1/apps/services/{id}/environments/{env}/shell/sessions", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("POST", "/v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/keys", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("GET", "/v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/screen", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("*", "/v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}/", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}/shell/sessions/{sh}", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    // A deploy hook is a credential that deploys, outside roll's tokens (no expiry, rules or
    // audit here; its URL is the secret, shown once): making, rotating or removing one is admin,
    // as an API key is. The hook's own URL isn't behind the gate.
    row("POST", "/v1/apps/services/{id}/environments/{env}/hook", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}/hook", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    // Waking starts compute someone pays for (and a private environment's, only its holders
    // could start): write, as waking a queues vhost is.
    row("POST", "/v1/apps/services/{id}/environments/{env}/wake", "apps", W, one(PG, "service")).env(EnvFrom::Param("env")),
    // Checking a domain changes nothing a person sees.
    row("POST", "/v1/apps/services/{id}/environments/{env}/domains/{host}/verify", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    row("GET", "/v1/apps", "apps", R, Tgt::Product),
    row("POST", "/v1/apps/plan", "apps", R, Tgt::AnyGrant),
    row("GET", "/v1/apps/projects/{project}/services", "apps", R, one(&["project"], "project")),
    row("GET", "/v1/apps/services/{id}/environments/{env}/variables/values", "apps", REVEAL, one(PG, "service")).env(EnvFrom::Param("env")),
    // A job is a command run with the environment's variables: its output can be them.
    row("GET", "/v1/apps/services/{id}/environments/{env}/jobs/{job}/logs", "apps", REVEAL, one(PG, "service")).env(EnvFrom::Param("env")),
    // ── previews (feature/roll-previews, docs/previews.md) ──
    row("GET", "/v1/previews", "previews", R, list("id")),
    row("POST", "/v1/previews", "previews", W, Tgt::Create { project: None, place: None, id_field: "id" }).env(EnvFrom::Fixed("preview")),
    row("*", "/v1/previews/projects/{project}/", "previews", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/previews/projects/{project}/", "previews", R, one(&["project"], "project")),
    // A project's CI bypass secret for private previews: made (and shown once) is reveal.
    row("POST", "/v1/previews/projects/{project}/bypass", "previews", REVEAL, one(&["project"], "project")),
    row("GET", "/v1/previews/projects/{project}/bypass", "previews", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/previews/projects/{project}/bypass", "previews", ADMIN, one(&["project"], "project")),
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
    // git smart HTTP with a scoped token (Basic: any user name, the token as password).
    row("GET", "/git/{owner}/{repo}/info/refs", "code", R, one(REPO, "git-repo")),
    row("POST", "/git/{owner}/{repo}/git-upload-pack", "code", R, one(REPO, "git-repo")),
    row("POST", "/git/{owner}/{repo}/git-receive-pack", "code", Act::GitPush, one(REPO, "git-repo")),
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

/// One ref update in a push, and the action it takes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RefUpdate {
    #[serde(rename = "ref")]
    pub name: String,
    pub old: String,
    pub new: String,
    pub action: Action,
}

/// A push's commands (`old new ref` pkt-lines before the first flush), as far as they've arrived.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Push {
    /// Not all there yet.
    More,
    /// Not commands this parser reads: the push needs everything it could do ([`UNREAD_PUSH`]).
    Bad,
    Commands(Vec<RefUpdate>),
}

/// What a push whose commands weren't read might do; it needs every one of them.
pub const UNREAD_PUSH: [Action; 3] = [Action::Write, Action::Delete, Action::Deploy];

/// A push's ref updates and what each one is: `deploy` when it moves `default_branch`, directly or
/// through `refs/land/<branch>` (which lands onto the branch; unknown default: any branch),
/// `delete` when it deletes a ref, else `write`. A push needs every action its updates take:
/// actions don't imply each other.
pub fn git_push(prefix: &[u8], default_branch: Option<&str>) -> Push {
    let mut rest = prefix;
    let mut out = Vec::new();
    loop {
        let Some(head) = rest.get(..4) else { return Push::More };
        let Some(len) = std::str::from_utf8(head).ok().and_then(|h| usize::from_str_radix(h, 16).ok()) else { return Push::Bad };
        if len == 0 {
            return if out.is_empty() { Push::Bad } else { Push::Commands(out) };
        }
        if len < 4 {
            return Push::Bad;
        }
        if rest.len() < len {
            return Push::More;
        }
        let line = &rest[4..len];
        rest = &rest[len..];
        let line = line.split(|b| *b == 0).next().unwrap_or(line);
        let Ok(text) = std::str::from_utf8(line) else { return Push::Bad };
        let text = text.trim_end_matches('\n');
        // A shallow client says which commits it has the history of before its commands.
        if out.is_empty() && text.strip_prefix("shallow ").is_some_and(is_oid) {
            continue;
        }
        let mut parts = text.split(' ');
        let (Some(old), Some(new), Some(name), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return Push::Bad };
        if !is_oid(old) || !is_oid(new) || name.is_empty() {
            return Push::Bad;
        }
        let onto = name.strip_prefix("refs/heads/").or_else(|| name.strip_prefix("refs/land/"));
        let ships = match (onto, default_branch) {
            (Some(b), Some(d)) => b == d,
            (Some(_), None) => true,
            (None, _) => false,
        };
        let action = if ships {
            Action::Deploy
        } else if new.bytes().all(|c| c == b'0') {
            Action::Delete
        } else {
            Action::Write
        };
        out.push(RefUpdate { name: name.to_string(), old: old.to_string(), new: new.to_string(), action });
    }
}

/// A git object id: 40 (SHA-1) or 64 (SHA-256) lowercase hex digits.
fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

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
        // A branch's app role answers its password: reveal, never the branch prefix's write.
        let f = find("POST", "/v1/projects/{id}/branches/{branch}/app-role", "x").unwrap();
        assert_eq!(f.row.action("POST", &[], None), Action::Reveal);
        // A whole project's values out (a backup) or in (a restore) is admin, never the prefix's write.
        for r in ["backup", "restore"] {
            let f = find("POST", &format!("/v1/secrets/projects/{{project}}/{r}"), "x").unwrap();
            assert_eq!(f.row.action("POST", &[], None), Action::Admin, "{r}");
        }
        // A product naming a parameter differently from its row still gets its row (secrets'
        // router says `{key}`, the row `{name}`): a value read is never the prefix's read.
        let f = find("GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{key}/value", "x").unwrap();
        assert_eq!(f.row.action("GET", &[], None), Action::Reveal);
        // Every route that answers secret values is reveal.
        for r in ["values", "secrets/{name}/value", "secrets/{name}/versions/{v}/value", "requests/{id}/values"] {
            let route = format!("/v1/secrets/projects/{{project}}/environments/{{env}}/{r}");
            let f = by_path("GET", &route.replace('{', "x").replace('}', "")).unwrap();
            assert_eq!(f.row.action("GET", &[], None), Action::Reveal, "{route}");
        }
        assert_eq!(apps_create_action(&serde_json::json!({ "source": { "kind": "repo", "repo": "acme/site" } })), Action::Deploy);
        assert_eq!(apps_create_action(&serde_json::json!({ "source": { "kind": "upload" } })), Action::Write);
        assert_eq!(bulk_action(b"{\"index\":{\"_id\":\"1\"}}\n{\"title\":\"a\"}\n"), Action::Write);
        assert_eq!(bulk_action(b"{\"index\":{}}\n{\"t\":1}\n{\"delete\":{\"_id\":\"2\"}}\n"), Action::Delete);
        assert_eq!(bulk_action(b"{\"update\":{\"_id\":\"3\"}}\n{\"script\":{\"source\":\"ctx.op = 'delete'\"}}\n"), Action::Delete);
        assert_eq!(apps_notification_action(&serde_json::json!({ "url": "https://example.com/h" })), Action::Reveal);
        assert_eq!(apps_notification_action(&serde_json::json!({ "kind": "slack", "url": "https://hooks.slack.com/x" })), Action::Write);
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
        // Comments can't hide what runs; scripts and credentials are the strongest.
        assert_eq!(cql_action(&serde_json::json!({ "query": "/* x */ DROP TABLE t" })), Action::Delete);
        assert_eq!(cql_action(&serde_json::json!({ "query": "-- hi\nTRUNCATE t" })), Action::Delete);
        assert_eq!(cql_action(&serde_json::json!({ "query": "SELECT '--' FROM t" })), Action::Read);
        assert_eq!(cql_action(&serde_json::json!({ "query": "CREATE ROLE x WITH SUPERUSER = true AND LOGIN = true" })), Action::Admin);
        assert_eq!(redis_action(&serde_json::json!({ "command": ["EVAL", "return redis.call('FLUSHALL')", "0"] })), Action::Delete);
        assert_eq!(sql_action(&serde_json::json!({ "sql": "CREATE ROLE x LOGIN SUPERUSER PASSWORD 'p'" })), Action::Admin);
        assert_eq!(sql_action(&serde_json::json!({ "sql": "insert into grants_log values (1)" })), Action::Write);
        assert_eq!(agent_command_action(&serde_json::json!({ "type": "terminal.open" })), Action::Admin);
        assert_eq!(agent_command_action(&serde_json::json!({ "type": "thread.configure", "runtime_mode": "full_access" })), Action::Admin);
        assert_eq!(agent_command_action(&serde_json::json!({ "type": "message.send" })), Action::Write);
        assert_eq!(agent_command_action(&serde_json::json!({ "type": "something.new" })), Action::Admin);
        // A push is read from its commands: the default branch ships, a deleted ref deletes.
        let pkt = |s: &str| format!("{:04x}{s}", s.len() + 4);
        let (z, a) = ("0".repeat(40), "a".repeat(40));
        let push = |cmds: &[String]| format!("{}0000PACK…", cmds.join("")).into_bytes();
        let actions = |p: Push| match p {
            Push::Commands(c) => c.into_iter().map(|u| u.action).collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        assert_eq!(actions(git_push(&push(&[pkt(&format!("{z} {a} refs/heads/feature\0report-status\n"))]), Some("main"))), [Action::Write]);
        assert_eq!(actions(git_push(&push(&[pkt(&format!("{z} {a} refs/heads/feature\0caps\n")), pkt(&format!("{z} {a} refs/heads/main\n"))]), Some("main"))), [Action::Write, Action::Deploy]);
        assert_eq!(actions(git_push(&push(&[pkt(&format!("{a} {z} refs/tags/v1\0caps\n")), pkt(&format!("{z} {a} refs/heads/x\n"))]), Some("main"))), [Action::Delete, Action::Write]);
        // refs/land/<branch> lands onto the branch.
        assert_eq!(actions(git_push(&push(&[pkt(&format!("{z} {a} refs/land/main\0caps\n"))]), Some("main"))), [Action::Deploy]);
        assert_eq!(actions(git_push(&push(&[pkt(&format!("{z} {a} refs/land/feature\0caps\n"))]), None)), [Action::Deploy]);
        assert_eq!(git_push(b"00", Some("main")), Push::More);
        assert_eq!(git_push(b"0000", Some("main")), Push::Bad);
        assert_eq!(git_push(pkt(&format!("{z} {a}")).as_bytes(), Some("main")), Push::Bad);
        assert_eq!(action("POST", "/v1/previews/projects/{p}/bypass", "/v1/previews/projects/prj_a/bypass"), Some(Action::Reveal));
        // An apps shell runs any command with the environment's variables: admin, outside production
        // too, all of a session (closing one included). Waking an environment starts compute: write.
        for (m, p) in [("POST", "/v1/apps/services/{s}/environments/{e}/shell/sessions/{sh}/resize"), ("DELETE", "/v1/apps/services/{s}/environments/{e}/shell/sessions/{sh}")] {
            assert_eq!(action(m, p, "x"), Some(Action::Admin), "{m} {p}");
        }
        assert_eq!(action("POST", "/v1/apps/services/{s}/environments/{e}/wake", "x"), Some(Action::Write));
        assert_eq!(action("POST", "/v1/apps/services/{s}/environments/{e}/shell/sessions/{sh}/keys", "/v1/apps/services/s/environments/staging/shell/sessions/x/keys"), Some(Action::Admin));
        // Secrets' identities mint tokens; renewing a lease extends a credential.
        assert_eq!(action("GET", "/v1/secrets/projects/{p}/identities", "x"), Some(Action::Admin));
        assert_eq!(action("POST", "/v1/secrets/projects/{p}/leases/{l}/renew", "x"), Some(Action::Reveal));
        let f = find("POST", "/git/{*path}", "/git/acme/site.git/git-receive-pack").unwrap();
        assert_eq!(f.row.route, "/git/{owner}/{repo}/git-receive-pack");
        let mut routes: Vec<_> = ROWS.iter().map(|r| (r.method, r.route)).collect();
        let n = routes.len();
        routes.sort();
        routes.dedup();
        assert_eq!(routes.len(), n, "a route and method has two rows");
    }
}
