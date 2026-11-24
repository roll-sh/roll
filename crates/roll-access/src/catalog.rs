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
    /// The product answers only the indices the grant reaches (`roll_auth::scope::Resource::filters_index`).
    pub filters_index: bool,
    /// Other resources whose credentials the request hands to this one (apps' links put a
    /// database's address and password in a service's variables): it also needs `reveal` on each.
    pub lends: Option<Lend>,
    /// A request header that makes it `admin` when it's `1` (apps' `X-Roll-Override`: going past
    /// a deploy freeze the team set is a person's call, never a deploy token's).
    pub admin_if: Option<&'static str>,
    /// A stronger action the product may use within this route when the grant allows it (Postgres
    /// SQL as the database's superuser only for a grant that may `admin` the endpoint): the gate
    /// asks it quietly, never refusing for it, and says yes with `roll_auth::Capable`.
    pub capable: &'static [Action],
    /// A JSON body field whose presence makes it `admin` (apps' `freeze`: setting or lifting a
    /// freeze is the same call as going past one).
    pub admin_if_field: Option<&'static [&'static str]>,
}

/// Where a request names other resources it hands to this one ([`Row::lends`]).
#[derive(Clone, Copy, Debug)]
pub enum Lend {
    /// Two path parameters, the kind and the ref, of a link: `reveal` on it.
    Path(&'static str, &'static str),
    /// What the JSON body names, each with the action it needs there.
    Each(fn(&Value) -> Vec<Lent>),
}

/// Another resource a request hands to this one, as an apps link kind names it, and the action
/// that needs there.
#[derive(Clone, Debug, PartialEq)]
pub struct Lent {
    pub kind: String,
    pub r#ref: String,
    pub action: Action,
}

/// Apps' `links: [{kind, ref}]`: each linked resource's credentials go to the service (reveal).
/// An entry that isn't `{kind, ref}` names a kind roll doesn't know, so it's refused.
fn apps_links(b: &Value) -> Vec<Lent> {
    b.get("links").and_then(Value::as_array).into_iter().flatten().map(|l| Lent { kind: l["kind"].as_str().unwrap_or("").into(), r#ref: l["ref"].as_str().unwrap_or("").into(), action: Action::Reveal }).collect()
}

/// A worker's `run.scale.stream`: apps reads that stream group's lag for it (read on the stream).
fn apps_scale(b: &Value) -> Vec<Lent> {
    b.pointer("/run/scale/stream").and_then(Value::as_str).map(|s| Lent { kind: "stream".into(), r#ref: s.into(), action: Action::Read }).into_iter().collect()
}

/// A web service's `run.push`: apps reads each stream for it as a consumer group (read on it), and
/// appends a record its app keeps refusing to a dead-letter topic there (write, for a rule with
/// one).
fn apps_push(b: &Value) -> Vec<Lent> {
    let rules = b.pointer("/run/push").and_then(Value::as_array).into_iter().flatten();
    rules
        .filter_map(|r| {
            let s = r.get("stream").and_then(Value::as_str)?;
            let action = if r.get("dead_letter").is_some_and(|d| !d.is_null()) { Action::Write } else { Action::Read };
            Some(Lent { kind: "stream".into(), r#ref: s.into(), action })
        })
        .collect()
}

/// A drain into a roll storage bucket (`bucket`): apps mints a key that writes there for it.
fn apps_drain_bucket(b: &Value) -> Vec<Lent> {
    b.get("bucket").and_then(Value::as_str).map(|r| Lent { kind: "bucket".into(), r#ref: r.into(), action: Action::Write }).into_iter().collect()
}

/// The roll code repos the body names, which apps reads for the caller (`repo` for a plan or an
/// import, `source.repo` for a service built from one, `build.submodule_repos` its private
/// submodules come from): read on each, so a token can't learn a repo's stack, head or roll.toml,
/// or build and run its code, without being allowed to read it.
fn apps_repo(b: &Value) -> Vec<Lent> {
    let from_source = (b.pointer("/source/kind").and_then(Value::as_str) == Some("repo")).then(|| b.pointer("/source/repo").and_then(Value::as_str)).flatten();
    let subs = b.pointer("/build/submodule_repos").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str);
    b.get("repo").and_then(Value::as_str).or(from_source).into_iter().chain(subs).map(|r| Lent { kind: "repo".into(), r#ref: r.into(), action: Action::Read }).collect()
}

/// A new service: its links, its scaling and pushed streams and the repo it's built from.
fn apps_create_lends(b: &Value) -> Vec<Lent> {
    let mut out = apps_links(b);
    out.extend(apps_scale(b));
    out.extend(apps_push(b));
    out.extend(apps_repo(b));
    out
}

/// A service's settings: its scaling and pushed streams and, when its source changes, the repo.
fn apps_patch_lends(b: &Value) -> Vec<Lent> {
    let mut out = apps_scale(b);
    out.extend(apps_push(b));
    out.extend(apps_repo(b));
    out
}

/// A search cluster made as a branch of `parent` (a body field, unlike `…/{id}/branches`): it
/// copies the parent's indices, so it needs what branching there does, `write` on the parent.
fn search_parent(b: &Value) -> Vec<Lent> {
    b.get("parent").and_then(Value::as_str).map(|p| Lent { kind: "search".into(), r#ref: p.into(), action: Action::Write }).into_iter().collect()
}

/// A search cluster linked as another's remote (`cluster`): roll issues a cross-cluster API key
/// of it, so the link needs `admin` on it too, as making its keys does.
fn search_remote(b: &Value) -> Vec<Lent> {
    b.get("cluster").and_then(Value::as_str).map(|p| Lent { kind: "search".into(), r#ref: p.into(), action: Action::Admin }).into_iter().collect()
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
        "search" => ("search", "cluster"),
        // A tasks tenant: an API key of the link's own and its signing keys.
        "tasks" => ("tasks", "tasks"),
        // Another service's URLs: no credential, but held to the same `reveal` as the rest.
        "service" => ("apps", "service"),
        // Not a link: a repo apps reads (`apps_repo`).
        "repo" => ("code", "repo"),
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
        self.admin_if_field.is_some() || matches!(self.act, Act::Body(_) | Act::Raw(_)) || matches!(self.copies_from, Some(CopyFrom::Body(_) | CopyFrom::Each(_))) || matches!(self.lends, Some(Lend::Each(_)))
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
    Row { method, route, product, act, target, env: EnvFrom::None, path_query: None, reveals: &[], copies_from: None, index: None, lends: None, filters_itself: false, filters_index: false, admin_if: None, admin_if_field: None, capable: &[] }
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
    const fn capable(mut self, a: &'static [Action]) -> Row {
        self.capable = a;
        self
    }
    const fn admin_if(mut self, header: &'static str) -> Row {
        self.admin_if = Some(header);
        self
    }
    const fn admin_if_field(mut self, fields: &'static [&'static str]) -> Row {
        self.admin_if_field = Some(fields);
        self
    }

    /// The action with the request's headers: `admin` when [`Row::admin_if`]'s is `1`.
    pub fn action_with(&self, method: &str, params: &[(String, String)], body: Option<&Value>, headers: &axum::http::HeaderMap) -> Action {
        match (self.admin_if, self.admin_if_field) {
            (Some(h), _) if headers.get(h).is_some_and(|v| v == "1") => Action::Admin,
            (_, Some(fs)) if fs.iter().any(|f| body.and_then(|b| b.get(f)).is_some()) => Action::Admin,
            _ => self.action(method, params, body),
        }
    }
    /// A route that keeps only what the caller's `Grant` allows in each answer (secrets' health
    /// and leaks): a rule limited to some environments or folders matches it too.
    const fn filters_itself(mut self) -> Row {
        self.filters_itself = true;
        self
    }
    /// A list that keeps only the indices the caller's `Grant` reaches (secrets' encryption keys):
    /// a rule limited to some indices matches it too.
    const fn filters_index(mut self) -> Row {
        self.filters_index = true;
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
    ("/v1/access/oauth/devices/{code}", "only a person signs a device in, as on the consent screen"),
    ("/v1/access/notify", "where approvals go is the operator's choice"),
    ("/v1/access/policy", "the rules for every token are the operator's"),
    ("/v1/access/enterprise", "which identity providers roll trusts is the operator's"),
    ("/v1/access/enterprise/idps", "which identity providers roll trusts is the operator's"),
    ("/v1/access/tokens/revoke", "an incident's cut-off of many tokens at once is the operator's"),
    ("/v1/access/enterprise/apps", "which apps an identity provider may sign people in to is the operator's"),
    ("/v1/access/enterprise/apps/{id}", "which apps an identity provider may sign people in to is the operator's"),
    ("/v1/access/enterprise/apps/{id}/rotate", "an enterprise app's credential is the operator's"),
    ("/v1/access/clients", "a token makes no machine clients: the operator, or a person signed in, makes their own"),
    ("/v1/access/clients/{id}", "a token revokes no machine clients: the operator, or a person signed in, revokes their own"),
    ("/v1/access/clients/{id}/rotate", "a token rotates no machine client's credential: the operator, or a person signed in, rotates their own"),
    ("/v1/access/audit/sinks", "where the audit log is sent is the operator's choice"),
    ("/v1/access/audit/sinks/{id}", "where the audit log is sent is the operator's choice"),
    ("/v1/access/audit/sinks/{id}/retry", "where the audit log is sent is the operator's choice"),
    ("/v1/secrets/issuer/rotate", "roll's OIDC issuer key signs for every project: rotating it is the operator's"),
    ("/v1/secrets/audit/key/rotate", "the audit log's digest key signs every project's digests: rotating or pulling it is the operator's"),
    ("/v1/previews/{id}/visit", "a browser session in a team-access preview is a signed-in person's (rolld's gate sees their session), never a token's"),
    ("/v1/apps/services/{id}/environments/{env}/visit", "a browser session in a sign-in-gated environment is a signed-in person's (rolld's gate sees their session), never a token's"),
    ("/v1/apps/services/{id}/github", "roll's GitHub App installations span tenants: connecting a service to one (its tokens fetch any repository the installation sees) is the operator's, as connecting a repo is roll code's admin token's"),
];

/// What a person's session does for itself, past the member rules (docs/members.md): making a
/// project, which they then own. Tokens are decided by its row as ever.
pub const PERSON_OWN: &[(&str, &str)] = &[("POST", "/v1/workspace/projects")];

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
    let forwarded = FORWARDS.iter().any(|(r, _, _)| *r == route);
    if !route.contains("{*") && !is_nest(route) && !nested && !forwarded {
        return None;
    }
    by_path(method, path)
}

/// A workspace membership change (`PUT`/`DELETE …/projects/{id}/resources/{kind}/{ref}`): the
/// product and id of the resource being moved, which the evaluator also checks.
pub fn placement(row: &Row, params: &[(String, String)]) -> Option<(&'static str, String)> {
    if !matches!(row.route, "/v1/workspace/projects/{id}/resources/{kind}/{ref}" | "/v1/workspace/projects/{id}/homes/{kind}/{ref}") {
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
/// Signed hops to an owner or leader: `(route, the header carrying the client's path, the product
/// whose route it carries)`. A hop is decided by the route it carries, and only its own product's.
pub const FORWARDS: &[(&str, &str, &str)] = &[("/internal/storage/control", "x-roll-storage-uri", "storage"), ("/internal/tasks", "x-roll-tasks-uri", "tasks")];

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
    if FORWARDS.iter().any(|(r, _, _)| *r == route) {
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

/// Whether `method route` is decided by a prefix row: no row of the route's own names that method
/// (a route with its own `GET` row can still have a `PUT` a prefix decides), and a prefix row
/// covers it.
pub fn method_by_prefix(method: &str, route: &str) -> bool {
    let shape = shape(route);
    let own = ROWS.iter().any(|r| self::shape(r.route) == shape && (r.method == "*" || r.method == method));
    !own && !OPERATOR_ONLY.iter().any(|(r, _)| self::shape(r) == shape) && ROWS.iter().any(|r| r.route.ends_with('/') && shape.starts_with(&self::shape(r.route)))
}

/// Whether `method route`, decided by a prefix row, was looked at (`prefix-reviewed.txt`, one
/// `METHOD route` a line).
pub fn prefix_reviewed(method: &str, route: &str) -> bool {
    include_str!("prefix-reviewed.txt").lines().map(str::trim).any(|l| l.split_once(' ').is_some_and(|(m, r)| m == method && r == route))
}

/// Every route a prefix row decides that was looked at, as `(method, route)`.
pub fn prefix_reviewed_routes() -> impl Iterator<Item = (&'static str, &'static str)> {
    include_str!("prefix-reviewed.txt").lines().map(str::trim).filter_map(|l| l.split_once(' '))
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
        Act::ProductionIf(p) => format!("deploy when {{{p}}} is production, else write{}", r.admin_if.map(|h| format!("; admin with {h}: 1")).unwrap_or_default()),
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

/// A NoSQL database's settings: `admin` when they loosen what keeps its data safe, a masking
/// policy (applied at read time for every role: dropping it shows a reader the raw values) or a
/// read-only flag turned off; the rest (name, idle timeout, durability, history, expiry) `write`.
fn nosql_patch_action(b: &Value) -> Action {
    if b.get("masking").is_some() || b.get("read_only") == Some(&Value::Bool(false)) {
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
        "FLUSHDB" | "FLUSHALL" | "SWAPDB" | "EVAL" | "EVALSHA" | "EVAL_RO" | "EVALSHA_RO" | "FCALL" | "FCALL_RO" | "FUNCTION" | "SCRIPT" => Action::Delete,
        // The server's, not the data's: Redis's own @dangerous category. MONITOR and SYNC stream
        // everyone's commands and data, CLIENT sees and ends other connections, SLOWLOG shows other
        // commands' arguments.
        "ACL" | "CONFIG" | "AUTH" | "HELLO" | "MIGRATE" | "MODULE" | "DEBUG" | "SHUTDOWN" | "REPLICAOF" | "SLAVEOF" | "MONITOR" | "SYNC" | "PSYNC" | "REPLCONF"
        | "CLIENT" | "SLOWLOG" | "LATENCY" | "CLUSTER" | "FAILOVER" | "ROLE" | "BGSAVE" | "SAVE" | "BGREWRITEAOF" | "LASTSAVE" | "RESTORE" | "RESTORE-ASKING"
        | "PFDEBUG" | "PFSELFTEST" => Action::Admin,
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

/// Writing an index's settings: `delete` when the body names a lifecycle (ILM's
/// `index.lifecycle.*`, OpenSearch ISM's `…index_state_management…`), whose policy can delete the
/// index, which `write` can't; or when it isn't JSON (it fails closed); else `write`. Every key is
/// read, duplicates included, in every form a settings body takes (`{"index": {"lifecycle": …}}`,
/// `"index.lifecycle.name"`, under `settings`).
fn index_settings_action(b: &[u8]) -> Action {
    let mut keys = Vec::new();
    let mut de = serde_json::Deserializer::from_slice(b);
    let read = serde::de::DeserializeSeed::deserialize(KeyPaths { prefix: String::new(), out: &mut keys }, &mut de).and_then(|()| de.end());
    let lifecycle = |k: &String| {
        let k = k.to_ascii_lowercase();
        k.contains("lifecycle") || k.contains("index_state_management")
    };
    if read.is_err() || keys.iter().any(lifecycle) { Action::Delete } else { Action::Write }
}

/// Collects every key of a JSON document as a dotted path, as it's parsed: a duplicate key is
/// seen as often as it appears (a `Value` would keep only the last).
struct KeyPaths<'a> {
    prefix: String,
    out: &'a mut Vec<String>,
}

impl<'de> serde::de::DeserializeSeed<'de> for KeyPaths<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for KeyPaths<'_> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("JSON")
    }
    fn visit_map<M: serde::de::MapAccess<'de>>(self, mut m: M) -> Result<(), M::Error> {
        let out = self.out;
        while let Some(k) = m.next_key::<String>()? {
            let path = if self.prefix.is_empty() { k } else { format!("{}.{k}", self.prefix) };
            out.push(path.clone());
            m.next_value_seed(KeyPaths { prefix: path, out: &mut *out })?;
        }
        Ok(())
    }
    fn visit_seq<S: serde::de::SeqAccess<'de>>(self, mut s: S) -> Result<(), S::Error> {
        let out = self.out;
        while s.next_element_seed(KeyPaths { prefix: self.prefix.clone(), out: &mut *out })?.is_some() {}
        Ok(())
    }
    fn visit_bool<E>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E>(self) -> Result<(), E> {
        Ok(())
    }
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

/// A service's Git login: `deploy` when it names a project secret (`secret`): the service's
/// fetches then send that production secret to its source's host, which only someone who may
/// change what production runs gets to decide; `write` for a password the caller gives itself.
fn apps_git_login_action(b: &Value) -> Action {
    if b.get("secret").is_some() { Action::Deploy } else { Action::Write }
}

/// A new apps notification: a webhook (the default) answers its signing secret once, so `reveal`;
/// a Slack, Discord or email one (the project's owner) answers none, so `write`.
fn apps_notification_action(b: &Value) -> Action {
    match b["kind"].as_str() {
        // Nothing comes back from these (an on-call key was the caller's to begin with).
        Some("slack" | "discord" | "teams" | "googlechat" | "email" | "pagerduty" | "opsgenie") => Action::Write,
        _ => Action::Reveal,
    }
}

/// A new apps service: `deploy` unless its source is an upload (which deploys only with `roll up`).
fn apps_create_action(b: &Value) -> Action {
    if b["source"]["kind"] == "upload" { Action::Write } else { Action::Deploy }
}

/// A service's run settings that change only how much of it runs and how it's watched: its size,
/// scale, health checks, timers, alerts, rollout and restarts. Every other run key decides what
/// production runs, when, or who reaches it (start, pre-deploy, schedule, schedules and their
/// timezone and retries, kind, port, volume, public, tcp, a worker's `health_command`, which runs
/// in its instances, and `rollback_on_crash`, which has roll
/// put an older deployment live by itself), and so does any key apps adds later until it's listed
/// here: a new setting is never a quiet way into production.
const APPS_WRITE_RUN: [&str; 27] = [
    "min_instances",
    "max_instances",
    "concurrency",
    "queue_s",
    "latency_target_ms",
    "cpu",
    "memory_mb",
    "health_path",
    "health_timeout_s",
    "health_every_s",
    "health_failures",
    "suspend_after_s",
    "sleep_after_s",
    "alerts",
    "slo",
    "metrics",
    "schedule_scale",
    "log_metrics",
    "rollout",
    "scale",
    "drain_s",
    "cpu_target",
    "memory_target",
    "restart",
    "restart_tries",
    "affinity",
    "timeout_s",
];

/// A service's settings: `deploy` when they change what runs or who reaches it (`source`, `build`,
/// any run key not in [`APPS_WRITE_RUN`]); `write` for the rest of `run`.
fn apps_patch_action(b: &Value) -> Action {
    let runs = b.get("source").is_some() || b.get("build").is_some() || b["run"].as_object().is_some_and(|r| r.keys().any(|k| !APPS_WRITE_RUN.contains(&k.as_str())));
    if runs { Action::Deploy } else { Action::Write }
}

/// A new roll code repo: `admin` when it connects a GitHub repository (any installation's
/// repositories could be imported that way: listing installations is admin too), else `write`.
fn code_repo_action(b: &Value) -> Action {
    if b.get("source").is_some_and(Value::is_object) { Action::Admin } else { Action::Write }
}

/// Landing: `deploy`, and `admin` with `bypass: true` (it lands over the repo's required reviews,
/// the merge queue and a secret-leak blocker; roll code allows that to its admin only).
fn code_land_action(b: &Value) -> Action {
    if b.get("bypass") == Some(&Value::Bool(true)) { Action::Admin } else { Action::Deploy }
}

/// A stream's settings: `delete` when they change its retention (a shorter one drops records at
/// once), else `write`.
fn stream_settings_action(b: &Value) -> Action {
    if b["settings"].get("retention_ms").is_some() { Action::Delete } else { Action::Write }
}

/// A topic's change: `delete` when its configs change what's kept (`retention.*`,
/// `cleanup.policy`, `delete.retention.ms`: records go at once), else `write` (partitions only grow).
fn topic_change_action(b: &Value) -> Action {
    let drops = b["configs"].as_object().is_some_and(|c| c.keys().any(|k| k.starts_with("retention.") || k == "cleanup.policy" || k == "delete.retention.ms"));
    if drops { Action::Delete } else { Action::Write }
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
/// Tasks: the headers a message, schedule or run forwards to its destination
/// (`Roll-Forward-Authorization: Bearer …`, or `headers` in the body), stored as given.
const TASKS_SECRETS: &[&str] = &["headers"];
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
    // Deciding a person's pending token is an admin's (the handler refuses any token).
    row("POST", "/v1/access/tokens/{id}/approve", "access", ADMIN, Tgt::Product),
    row("POST", "/v1/access/tokens/{id}/deny", "access", ADMIN, Tgt::Product),
    row("GET", "/v1/access/tokens/{id}/tighten", "access", R, Tgt::Product),
    row("GET", "/v1/access/tokens/{id}/networks", "access", R, Tgt::Product),
    // What an agent with the token is offered over MCP (roll-mcp reads the token as the caller first).
    row("GET", "/v1/access/tokens/{id}/tools", "access", R, Tgt::Product),
    // How many tools each product has and their context cost: counts only, for the connect panel.
    row("GET", "/v1/mcp/toolsets", "access", R, Tgt::AnyGrant),
    row("GET", "/v1/access/checkup", "access", R, Tgt::Product),
    row("POST", "/v1/access/tokens/{id}/tighten", "access", ADMIN, Tgt::Product),
    row("DELETE", "/v1/access/tokens/{id}/temporary/{approval}", "access", ADMIN, Tgt::Product),
    // A token asks for more for itself (a person decides): any grant may ask, as it may ask whoami.
    row("POST", "/v1/access/requests", "access", R, Tgt::AnyGrant),
    row("GET", "/v1/access/approvals", "access", R, Tgt::Product),
    // A connection's own approvals' decisions (the handler answers no one else's).
    row("GET", "/v1/access/approvals/decided", "access", R, Tgt::AnyGrant),
    // A grant waits on its own approvals (the handler checks it's its own, or access read).
    row("GET", "/v1/access/approvals/{id}", "access", R, Tgt::AnyGrant),
    row("GET", "/v1/access/audit", "access", R, Tgt::Product),
    row("GET", "/v1/access/activity", "access", R, Tgt::Product),
    row("GET", "/v1/access/alerts", "access", R, Tgt::Product),
    // Any connection may tell the operator what was hard (it changes nothing; capped an hour).
    row("POST", "/v1/access/feedback", "access", R, Tgt::AnyGrant),
    row("GET", "/v1/access/oauth/clients", "access", R, Tgt::Product),
    row("GET", "/v1/access/oauth/clients/{id}", "access", R, Tgt::Product),
    row("GET", "/v1/access/oauth/step-ups/{id}", "access", R, Tgt::Product),
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
    // Members (docs/members.md): seeing who's in a project is a read; managing them is the
    // project's admin (and a role is given only when the caller's rules cover it). Leaving is any
    // member's own. A home changes which project stands for a resource: admin on the project, and
    // admin outright on the resource ([`placement`]).
    row("GET", "/v1/workspace/projects/{id}/members", "workspace", R, one(&["id"], "project")),
    // The project's events feed (member, group role and invite changes), for whoever sees its
    // members; an invite's address only for its admins. roll MCP's `workspace.event`.
    row("GET", "/v1/workspace/projects/{id}/events", "workspace", R, one(&["id"], "project")),
    row("PATCH", "/v1/workspace/projects/{id}/members/{user}", "workspace", ADMIN, one(&["id"], "project")),
    row("DELETE", "/v1/workspace/projects/{id}/members/{user}", "workspace", ADMIN, one(&["id"], "project")),
    row("DELETE", "/v1/workspace/projects/{id}/members/me", "workspace", R, one(&["id"], "project")),
    row("GET", "/v1/workspace/projects/{id}/invites", "workspace", ADMIN, one(&["id"], "project")),
    row("POST", "/v1/workspace/projects/{id}/invites", "workspace", ADMIN, one(&["id"], "project")),
    row("DELETE", "/v1/workspace/projects/{id}/invites/{invite}", "workspace", ADMIN, one(&["id"], "project")),
    row("PUT", "/v1/workspace/projects/{id}/homes/{kind}/{ref}", "workspace", ADMIN, one(&["id"], "project")),
    // Groups' roles (docs/members-v1-1.md): seeing which groups hold a role is a read; giving or
    // taking one away is the project's admin (covered by the caller's own rules, and only by one
    // who is in the group, D11). The groups themselves (`/v1/workspace/groups…`) are the
    // operator's: no row.
    row("GET", "/v1/workspace/projects/{id}/groups", "workspace", R, one(&["id"], "project")),
    row("PUT", "/v1/workspace/projects/{id}/groups/{group}", "workspace", ADMIN, one(&["id"], "project")),
    row("DELETE", "/v1/workspace/projects/{id}/groups/{group}", "workspace", ADMIN, one(&["id"], "project")),
    // The built-in roles and their rules: any grant.
    row("GET", "/v1/workspace/roles", "workspace", R, Tgt::AnyGrant),
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
    // Decoy roles (roll secrets' Postgres decoys): the proxy refuses their logins; no password.
    row("POST", "/v1/projects/{id}/decoy-roles", "postgres", W, one(PG, "postgres-project")),
    row("DELETE", "/v1/projects/{id}/decoy-roles/{role}", "postgres", W, one(PG, "postgres-project")),
    row("GET", "/v1/projects/{id}/usage", "postgres", R, one(PG, "postgres-project")),
    row("GET", "/v1/endpoints/{id}", "postgres", R, one(PG, "endpoint")).reveals(PG_SECRETS),
    row("PATCH", "/v1/endpoints/{id}", "postgres", W, one(PG, "endpoint")),
    row("DELETE", "/v1/endpoints/{id}", "postgres", D, one(PG, "endpoint")),
    row("POST", "/v1/endpoints/{id}/start", "postgres", W, one(PG, "endpoint")),
    row("POST", "/v1/endpoints/{id}/suspend", "postgres", W, one(PG, "endpoint")),
    // SQL that isn't read-only runs as a role that owns the database's objects but is never the
    // superuser (pg-control's `SqlAs::Writer`), unless the grant may `admin` the endpoint; it
    // drops objects only if the grant may `delete` there.
    row("POST", "/v1/endpoints/{id}/query", "postgres", Act::Body(sql_action), one(PG, "endpoint")).capable(&[Action::Admin, Action::Delete]),
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
    row("PATCH", "/v1/streams/{s}", "streams", Act::Body(stream_settings_action), one(&["s"], "stream")),
    row("DELETE", "/v1/streams/{s}", "streams", D, one(&["s"], "stream")),
    // A copy-on-write branch of the stream (roll previews): its answer has the branch's own key.
    row("POST", "/v1/streams/{s}/branches", "streams", W, one(&["s"], "stream")).reveals(STREAM_SECRETS),
    row("GET", "/v1/streams/{s}/keys", "streams", R, one(&["s"], "stream")),
    row("POST", "/v1/streams/{s}/keys", "streams", REVEAL, one(&["s"], "stream")),
    row("DELETE", "/v1/streams/{s}/keys/{username}", "streams", ADMIN, one(&["s"], "stream")),
    row("GET", "/v1/streams/{s}/topics", "streams", R, one(&["s"], "stream")),
    row("POST", "/v1/streams/{s}/topics", "streams", W, one(&["s"], "stream")),
    row("GET", "/v1/streams/{s}/topics/{t}", "streams", R, one(&["s"], "stream")),
    row("PATCH", "/v1/streams/{s}/topics/{t}", "streams", Act::Body(topic_change_action), one(&["s"], "stream")),
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
    row("GET", "/v1/tasks/messages", "tasks", R, Tgt::Product).reveals(TASKS_SECRETS),
    row("POST", "/v1/tasks/messages/cancel", "tasks", D, Tgt::Product),
    row("GET", "/v1/tasks/messages/{id}", "tasks", R, Tgt::Product).reveals(TASKS_SECRETS),
    row("DELETE", "/v1/tasks/messages/{id}", "tasks", D, Tgt::Product),
    row("POST", "/v1/tasks/messages/{id}/retry", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/dlq", "tasks", R, Tgt::Product).reveals(TASKS_SECRETS),
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
    row("GET", "/v1/tasks/schedules", "tasks", R, list_of("tasks-schedule", "id")).reveals(TASKS_SECRETS),
    row("POST", "/v1/tasks/schedules", "tasks", W, Tgt::Product),
    row("GET", "/v1/tasks/schedules/{id}", "tasks", R, one(&["id"], "tasks-schedule")).reveals(TASKS_SECRETS),
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
    row("GET", "/v1/tasks/runs", "tasks", R, Tgt::Product).reveals(TASKS_SECRETS),
    row("GET", "/v1/tasks/runs/{id}", "tasks", R, Tgt::Product).reveals(TASKS_SECRETS),
    row("GET", "/v1/tasks/runs/{id}/wait", "tasks", R, Tgt::Product).reveals(TASKS_SECRETS),
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
    row("PATCH", "/v1/nosql/databases/{id}", "nosql", Act::Body(nosql_patch_action), one(PG, "nosql-database")).reveals(NOSQL_SECRETS),
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
    row("POST", "/v1/search/clusters", "search", W, create("search", "id")).reveals(SEARCH_SECRETS).lends(Lend::Each(search_parent)),
    row("GET", "/v1/search/clusters/{id}", "search", R, one(PG, "cluster")).reveals(SEARCH_SECRETS),
    row("PATCH", "/v1/search/clusters/{id}", "search", ADMIN, one(PG, "cluster")).reveals(SEARCH_SECRETS),
    row("DELETE", "/v1/search/clusters/{id}", "search", D, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/branches", "search", W, one(PG, "cluster")).reveals(SEARCH_SECRETS),
    row("POST", "/v1/search/clusters/{id}/password", "search", REVEAL, one(PG, "cluster")),
    // A deleted cluster back (its id still resolves while it is kept): a write, like a create.
    row("POST", "/v1/search/clusters/{id}/restore", "search", W, one(PG, "cluster")),
    // Any request as the cluster's owner. One that answers or sets a credential (decided by
    // search on the API the cluster routes it to) needs reveal too: the gate says whether the
    // grant has it (`capable`), without reading the body.
    row("POST", "/v1/search/clusters/{id}/request", "search", ADMIN, one(PG, "cluster")).capable(&[Action::Reveal]),
    // An API key: its secret is in the answer.
    row("POST", "/v1/search/clusters/{id}/keys", "search", REVEAL, one(PG, "cluster")),
    row("DELETE", "/v1/search/clusters/{id}/keys/{key}", "search", ADMIN, one(PG, "cluster")),
    // Keys a web page ships (read-only, rate limited): the new key's secret is in the answer;
    // the list has none.
    row("GET", "/v1/search/clusters/{id}/browser-keys", "search", R, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/browser-keys", "search", REVEAL, one(PG, "cluster")),
    row("DELETE", "/v1/search/clusters/{id}/browser-keys/{key}", "search", ADMIN, one(PG, "cluster")),
    // A snapshot repository, the only way its credentials can be roll secrets references: it
    // changes the cluster as the console does; roll secrets checks `reveal` on each reference
    // itself (search_control::secret_refs).
    row("PUT", "/v1/search/clusters/{id}/repositories/{name}", "search", ADMIN, one(PG, "cluster")),
    // A migration's listing reads another Elasticsearch or OpenSearch through roll with
    // credentials the caller gives (never kept) and changes nothing on roll: a write. The
    // migration itself adds to the cluster's reindex whitelist (an admin's setting), creates
    // indices with the remote's settings and aliases and starts reindex tasks as the cluster's
    // own user (what the console does, an admin's): admin.
    row("POST", "/v1/search/clusters/{id}/migrate/inspect", "search", W, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/migrate", "search", ADMIN, one(PG, "cluster")),
    row("GET", "/v1/search/clusters/{id}/remotes", "search", R, one(PG, "cluster")),
    row("PUT", "/v1/search/clusters/{id}/remotes/{alias}", "search", ADMIN, one(PG, "cluster")).lends(Lend::Each(search_remote)),
    row("DELETE", "/v1/search/clusters/{id}/remotes/{alias}", "search", ADMIN, one(PG, "cluster")),
    // Inference endpoints keyed by a project's secret: making one changes the cluster as the
    // console does; roll secrets checks `reveal` on the secret itself, as the caller.
    row("GET", "/v1/search/clusters/{id}/inference/secrets", "search", R, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/inference/secrets", "search", ADMIN, one(PG, "cluster")),
    // Request metrics and the slow logs (queries' and documents' sources, cut): reads of the
    // cluster's data.
    row("GET", "/v1/search/clusters/{id}/metrics", "search", R, one(PG, "cluster")),
    row("GET", "/v1/search/clusters/{id}/slowlog", "search", R, one(PG, "cluster")),
    // The security audit log: a read, not a reveal (entries never hold a credential).
    row("GET", "/v1/search/clusters/{id}/audit", "search", R, one(PG, "cluster")),
    // Search analytics: the indices' query text, aggregated; a read of the cluster's data.
    row("GET", "/v1/search/clusters/{id}/analytics", "search", R, one(PG, "cluster")),
    row("*", "/v1/search/clusters/{id}/", "search", M, one(PG, "cluster")),
    // An index's routes are about those indices (`{index}`: a name, or on reads an expression),
    // so a rule can grant some of a cluster's indices. The console (`request`) and the index list
    // stay the cluster's: they reach every index.
    row("*", "/v1/search/clusters/{id}/indices/{index}", "search", M, one(PG, "cluster")).index("index"),
    row("*", "/v1/search/clusters/{id}/indices/{index}/", "search", M, one(PG, "cluster")).index("index"),
    row("POST", "/v1/search/clusters/{id}/indices/{index}/search", "search", R, one(PG, "cluster")).index("index"),
    // Analyzing text with an index's analyzers reads the index.
    row("POST", "/v1/search/clusters/{id}/indices/{index}/analyze", "search", R, one(PG, "cluster")).index("index"),
    row("POST", "/v1/search/clusters/{id}/indices/{index}/delete_by_query", "search", D, one(PG, "cluster")).index("index"),
    // A bulk can delete as well as index: its action lines decide.
    row("POST", "/v1/search/clusters/{id}/indices/{index}/bulk", "search", Act::Raw(bulk_action), one(PG, "cluster")).index("index"),
    // Query suggestions built from the index's search analytics: reading them (aggregated query
    // text, like the analytics) and rebuilding its suggestions index now (a write of the cluster).
    row("GET", "/v1/search/clusters/{id}/indices/{index}/suggestions", "search", R, one(PG, "cluster")).index("index"),
    row("POST", "/v1/search/clusters/{id}/indices/{index}/suggestions/rebuild", "search", W, one(PG, "cluster")).index("index"),
    // Relevance tuning: an index's settings (the engine refuses private, internal and managed
    // ones to every caller) change how it searches, as a mapping does, so they're write, except a
    // lifecycle policy, which can delete the index (`index_settings_action`); `_explain` reads one
    // document's score.
    row("PUT", "/v1/search/clusters/{id}/indices/{index}/settings", "search", Act::Raw(index_settings_action), one(PG, "cluster")).index("index"),
    row("POST", "/v1/search/clusters/{id}/indices/{index}/explain/{doc}", "search", R, one(PG, "cluster")).index("index"),
    // Synonyms sets and query rulesets are the cluster's (any index's analyzer or query may name
    // one): reading them is read; putting or deleting a set, a rule or a ruleset changes search
    // behaviour, never a credential, so it's write; testing a ruleset reads it.
    row("GET", "/v1/search/clusters/{id}/synonyms", "search", R, one(PG, "cluster")),
    row("GET", "/v1/search/clusters/{id}/synonyms/{set}", "search", R, one(PG, "cluster")),
    row("PUT", "/v1/search/clusters/{id}/synonyms/{set}", "search", W, one(PG, "cluster")),
    row("DELETE", "/v1/search/clusters/{id}/synonyms/{set}", "search", W, one(PG, "cluster")),
    row("GET", "/v1/search/clusters/{id}/synonyms/{set}/{rule}", "search", R, one(PG, "cluster")),
    row("PUT", "/v1/search/clusters/{id}/synonyms/{set}/{rule}", "search", W, one(PG, "cluster")),
    row("DELETE", "/v1/search/clusters/{id}/synonyms/{set}/{rule}", "search", W, one(PG, "cluster")),
    row("GET", "/v1/search/clusters/{id}/query_rules", "search", R, one(PG, "cluster")),
    row("GET", "/v1/search/clusters/{id}/query_rules/{ruleset}", "search", R, one(PG, "cluster")),
    row("PUT", "/v1/search/clusters/{id}/query_rules/{ruleset}", "search", W, one(PG, "cluster")),
    row("DELETE", "/v1/search/clusters/{id}/query_rules/{ruleset}", "search", W, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/query_rules/{ruleset}/test", "search", R, one(PG, "cluster")),
    // The web crawler's sites (verified for the cluster's owner) and its URL test. Starting,
    // checking or dropping a verification changes the limits the owner's crawls get, so it's
    // write; the token it answers is meant to be published (DNS or the site), not a credential.
    // `crawl-test` fetches one URL and writes nothing: read.
    row("GET", "/v1/search/clusters/{id}/site-verifications", "search", R, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/site-verifications", "search", W, one(PG, "cluster")),
    row("DELETE", "/v1/search/clusters/{id}/site-verifications/{site}", "search", W, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/site-verifications/{site}/check", "search", W, one(PG, "cluster")),
    row("POST", "/v1/search/clusters/{id}/crawl-test", "search", R, one(PG, "cluster")),
    // ── secrets (feature/roll-secrets, docs/secrets.md) ──
    // The caller and its grant's project (`roll run`), and health: any grant.
    row("GET", "/v1/secrets/whoami", "secrets", R, Tgt::AnyGrant),
    row("GET", "/v1/secrets/health", "secrets", R, Tgt::AnyGrant),
    // Search across projects: secrets keeps each project's results only where the token may read
    // (`Grant::allows`, one check per project inside the request).
    row("GET", "/v1/secrets/search", "secrets", R, Tgt::AnyGrant),
    row("GET", "/v1/secrets/findings", "secrets", R, Tgt::AnyGrant),
    // Deleted projects in their recovery window: secrets keeps those the caller may recover (an
    // admin of their secrets, asked of the grant per project, or the person whose project it was).
    row("GET", "/v1/secrets/deleted", "secrets", R, Tgt::AnyGrant),
    // Recovering one is its secrets admin's, as deleting it was.
    row("POST", "/v1/secrets/projects/{project}/recover", "secrets", ADMIN, one(&["project"], "project")),
    // A deleted environment's recovery is its admin's (docs/secrets.md, "When a secret, an
    // environment or the project goes").
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/recover", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("GET", "/v1/secrets/projects/{project}/", "secrets", R, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("GET", "/v1/secrets/projects/{project}", "secrets", R, one(&["project"], "project")),
    // How long a project keeps old values (history_days): purging is the project admin's.
    row("PATCH", "/v1/secrets/projects/{project}", "secrets", ADMIN, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/", "secrets", W, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("PUT", "/v1/secrets/projects/{project}/", "secrets", W, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("DELETE", "/v1/secrets/projects/{project}/", "secrets", D, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("PATCH", "/v1/secrets/projects/{project}/environments/{env}", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("PUT", "/v1/secrets/projects/{project}/environments/order", "secrets", ADMIN, one(&["project"], "project")),
    // A decoy key (docs/secrets.md, "Decoys"): an admin's, and it answers the key, never its value.
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/decoys", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
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
    // `filters_itself`: secrets asks the caller's `Grant` for each finding, so a token limited to
    // some environments or folders gets theirs and nothing else.
    row("GET", "/v1/secrets/projects/{project}/health", "secrets", R, one(&["project"], "project")).filters_itself(),
    // Which candidates are this project's stored values (`roll secrets scan`): read on the
    // project. Secrets keeps only matches in folders the caller may reveal, so it's no oracle.
    row("POST", "/v1/secrets/projects/{project}/leaks", "secrets", R, one(&["project"], "project")).filters_itself(),
    // Validity checks (docs/secrets.md, "Is it live?"): asking a credential's provider whether a
    // credential found in code works is a write (it calls out and is audited, rate-limited per
    // project); a kept answer by fingerprint (never the value) is a read.
    row("POST", "/v1/secrets/projects/{project}/validity", "secrets", W, one(&["project"], "project")),
    row("GET", "/v1/secrets/projects/{project}/unstored/{fingerprint}", "secrets", R, one(&["project"], "project")),
    // Rotators: URLs roll calls to issue and revoke values that live outside roll. Listing shows
    // names and hosts; adding one answers its signing secret once; both changes are admin.
    row("GET", "/v1/secrets/projects/{project}/rotations", "secrets", R, one(&["project"], "project")),
    // A customer-managed key wrapping the project's data key: seeing, setting and removing it are
    // the project admin's (it decides whether roll can read the project at all).
    row("*", "/v1/secrets/projects/{project}/kms", "secrets", ADMIN, one(&["project"], "project")),
    // AWS roles a project registers for dynamic credentials: listing is a read; adding one (roll
    // assumes it to check) and deleting one are admin.
    row("GET", "/v1/secrets/projects/{project}/aws-roles", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/aws-roles", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/aws-roles/{id}", "secrets", ADMIN, one(&["project"], "project")),
    row("PATCH", "/v1/secrets/projects/{project}/aws-roles/{id}", "secrets", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/secrets/projects/{project}/aws-users", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/aws-users", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/aws-users/{id}", "secrets", ADMIN, one(&["project"], "project")),
    row("PATCH", "/v1/secrets/projects/{project}/aws-users/{id}", "secrets", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/secrets/projects/{project}/rotators", "secrets", R, one(&["project"], "project")),
    // A signing secret signs calls that mint vendor keys: shown only to who may also reveal.
    row("POST", "/v1/secrets/projects/{project}/rotators", "secrets", ADMIN, one(&["project"], "project")).reveals(&["secret"]),
    row("DELETE", "/v1/secrets/projects/{project}/rotators/{id}", "secrets", ADMIN, one(&["project"], "project")),
    row("PATCH", "/v1/secrets/projects/{project}/rotators/{id}", "secrets", ADMIN, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/rotators/{id}/rotate-secret", "secrets", ADMIN, one(&["project"], "project")).reveals(&["secret"]),
    // SaaS rotators: vendor credentials roll issues and revokes keys with. Listing shows vendors
    // and targets; the credential is never answered, so adding one reveals nothing.
    row("GET", "/v1/secrets/projects/{project}/saas", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/saas", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/saas/{id}", "secrets", ADMIN, one(&["project"], "project")),
    // Lists only the keys the caller's grant reaches (`Grants::reaches_key`, each key's name as
    // its index), so a rule limited to some keys sees those.
    row("GET", "/v1/secrets/projects/{project}/transit", "secrets", R, one(&["project"], "project")).filters_index(),
    row("POST", "/v1/secrets/projects/{project}/transit", "secrets", ADMIN, one(&["project"], "project")),
    // Each key's routes name it (`.index("name")`): a rule can grant one key
    // (`{"id": "prj_x", "index": "payments"}`); the list and making a key stay the project's.
    row("PATCH", "/v1/secrets/projects/{project}/transit/{name}", "secrets", ADMIN, one(&["project"], "project")).index("name"),
    row("DELETE", "/v1/secrets/projects/{project}/transit/{name}", "secrets", ADMIN, one(&["project"], "project")).index("name"),
    row("POST", "/v1/secrets/projects/{project}/transit/{name}/rotate", "secrets", ADMIN, one(&["project"], "project")).index("name"),
    row("POST", "/v1/secrets/projects/{project}/transit/{name}/encrypt", "secrets", W, one(&["project"], "project")).index("name"),
    row("POST", "/v1/secrets/projects/{project}/transit/{name}/rewrap", "secrets", W, one(&["project"], "project")).index("name"),
    row("POST", "/v1/secrets/projects/{project}/transit/{name}/decrypt", "secrets", REVEAL, one(&["project"], "project")).index("name"),
    row("POST", "/v1/secrets/projects/{project}/transit/{name}/datakey", "secrets", REVEAL, one(&["project"], "project")).index("name"),
    row("POST", "/v1/secrets/projects/{project}/transit/{name}/sign", "secrets", W, one(&["project"], "project")).index("name"),
    row("POST", "/v1/secrets/projects/{project}/transit/{name}/verify", "secrets", R, one(&["project"], "project")).index("name"),
    row("POST", "/v1/secrets/projects/{project}/transit/{name}/hmac", "secrets", W, one(&["project"], "project")).index("name"),
    row("POST", "/v1/secrets/projects/{project}/transit/{name}/hmac/verify", "secrets", W, one(&["project"], "project")).index("name"),
    row("GET", "/v1/secrets/projects/{project}/transit/{name}/public", "secrets", R, one(&["project"], "project")).index("name"),
    row("GET", "/v1/secrets/projects/{project}/cas", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/cas", "secrets", ADMIN, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/cas/{id}/sign", "secrets", W, one(&["project"], "project")),
    row("GET", "/v1/secrets/projects/{project}/cas/{id}/certificates", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/cas/{id}/revoke", "secrets", W, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/cas/{id}", "secrets", ADMIN, one(&["project"], "project")),
    // ACME on a CA: its directory, pre-authorization and External Account Bindings. A binding's
    // HMAC key is answered once, to who may also reveal; making, revoking and pre-authorizing are
    // the project admin's (an account acts with its binding's names).
    row("GET", "/v1/secrets/projects/{project}/cas/{id}/acme", "secrets", R, one(&["project"], "project")),
    row("PATCH", "/v1/secrets/projects/{project}/cas/{id}/acme", "secrets", ADMIN, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/cas/{id}/acme/eab", "secrets", ADMIN, one(&["project"], "project")).reveals(&["hmac_key"]),
    row("DELETE", "/v1/secrets/projects/{project}/cas/{id}/acme/eab/{eab}", "secrets", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/secrets/projects/{project}/ssh-cas", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/ssh-cas", "secrets", ADMIN, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/ssh-cas/{id}/sign-host", "secrets", ADMIN, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/ssh-cas/{id}/sign-user", "secrets", REVEAL, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/ssh-cas/{id}", "secrets", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/secrets/projects/{project}/cloud-accounts", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/cloud-accounts", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/cloud-accounts/{id}", "secrets", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/secrets/projects/{project}/github-apps", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/github-apps", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/github-apps/{id}", "secrets", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/secrets/projects/{project}/outside-databases", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/outside-databases", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/outside-databases/{id}", "secrets", ADMIN, one(&["project"], "project")),
    // LDAP and Active Directory: listing never shows the bind password; registering takes one.
    row("GET", "/v1/secrets/projects/{project}/directories", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/directories", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/directories/{id}", "secrets", ADMIN, one(&["project"], "project")),
    // Snowflake accounts: listing never shows the admin's private key; registering takes one.
    row("GET", "/v1/secrets/projects/{project}/snowflake-accounts", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/snowflake-accounts", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/snowflake-accounts/{id}", "secrets", ADMIN, one(&["project"], "project")),
    // MongoDB Atlas projects: listing shows the service account's client id, never its secret;
    // registering takes one.
    row("GET", "/v1/secrets/projects/{project}/atlas-projects", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/atlas-projects", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/atlas-projects/{id}", "secrets", ADMIN, one(&["project"], "project")),
    // Gateways into private networks (docs/secrets.md, "Gateways"): records are the project's;
    // a gateway's own link is write on the gateway by its id, which only its token's rule names
    // (a project-wide rule doesn't reach it).
    row("GET", "/v1/secrets/projects/{project}/gateways", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/gateways", "secrets", ADMIN, one(&["project"], "project")).reveals(&["token"]),
    row("DELETE", "/v1/secrets/projects/{project}/gateways/{id}", "secrets", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/secrets/gateways/{id}/connect", "secrets", W, one(&["id"], "gateway")),
    row("PATCH", "/v1/secrets/projects/{project}/outside-databases/{id}", "secrets", ADMIN, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/outside-databases/{id}/rotate-admin", "secrets", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/secrets/projects/{project}/kubernetes", "secrets", R, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/kubernetes", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/kubernetes/{id}", "secrets", ADMIN, one(&["project"], "project")),
    // A backup seals every environment's values to a key the caller chooses; a restore loads a
    // whole project. Both are admin in secrets too.
    row("POST", "/v1/secrets/projects/{project}/backup", "secrets", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/secrets/projects/{project}/backup/schedule", "secrets", ADMIN, one(&["project"], "project")),
    row("PUT", "/v1/secrets/projects/{project}/backup/schedule", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/backup/schedule", "secrets", ADMIN, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/restore", "secrets", ADMIN, one(&["project"], "project")),
    row("PUT", "/v1/secrets/projects/{project}/environments/{env}/policy", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/folder-policies", "secrets", R, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("PUT", "/v1/secrets/projects/{project}/environments/{env}/folder-policies", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/secrets/projects/{project}/environments/{env}/folder-policies", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/requests/{id}/review", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/values", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    // A watcher's read of what changed since the values it holds: values, so reveal.
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/values/delta", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/value", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    // A TOTP key's current code is a credential: reveal, never the prefix's read.
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/totp", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/versions/{v}/value", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    // What hands out more than the request itself: syncs (they push values
    // elsewhere), audit sinks and required keys are admin; a share link of a value reveals it.
    row("*", "/v1/secrets/projects/{project}/syncs", "secrets", ADMIN, one(&["project"], "project")).reveals(&["signing_secret"]),
    row("GET", "/v1/secrets/projects/{project}/syncs", "secrets", R, one(&["project"], "project")),
    row("*", "/v1/secrets/projects/{project}/syncs/", "secrets", ADMIN, one(&["project"], "project")),
    // The events feed (docs/secrets.md, "Events"): names, kinds and counts, never a value. Secrets
    // asks the caller's `Grant` for each event's environment (and for `environment=`'s), so a token
    // limited to some environments reads only theirs.
    row("GET", "/v1/secrets/projects/{project}/events", "secrets", R, one(&["project"], "project")).filters_itself(),
    // Checking the audit log's chain answers where it breaks, never what a row says.
    row("GET", "/v1/secrets/projects/{project}/audit/verify", "secrets", R, one(&["project"], "project")),
    row("*", "/v1/secrets/projects/{project}/audit/sinks", "secrets", ADMIN, one(&["project"], "project")).reveals(&["secret"]),
    row("*", "/v1/secrets/projects/{project}/audit/sinks/", "secrets", ADMIN, one(&["project"], "project")),
    row("PUT", "/v1/secrets/projects/{project}/required", "secrets", ADMIN, one(&["project"], "project")),
    // Identities mint tokens, as `tokens` does: seeing or changing them is the project admin's.
    row("*", "/v1/secrets/projects/{project}/identities", "secrets", ADMIN, one(&["project"], "project")),
    row("*", "/v1/secrets/projects/{project}/identities/", "secrets", ADMIN, one(&["project"], "project")),
    // Pinning an issuer's keys decides which tokens log in as the identity: the identity's admin.
    row("PUT", "/v1/secrets/projects/{project}/identities/{id}/jwks", "secrets", ADMIN, one(&["project"], "project")),
    // Renewing a lease extends a credential whoever could read it holds.
    row("POST", "/v1/secrets/projects/{project}/leases/{id}/renew", "secrets", REVEAL, one(&["project"], "project")),
    // Ending a credential only takes access away: whoever could reveal it may.
    row("DELETE", "/v1/secrets/projects/{project}/leases/{id}", "secrets", REVEAL, one(&["project"], "project")),
    // Exporting an environment's folder shares it with another project.
    row("PUT", "/v1/secrets/projects/{project}/environments/{env}/exports", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")),
    // Who and what can read a folder: the project's credentials, as the tokens list (a project admin's).
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/reach", "secrets", ADMIN, one(&["project"], "project")),
    // An incident's cut-off revokes the project's credentials that reach a folder, and its freeze
    // locks readers out until an admin lifts it: both the project admin's, as reach is.
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/reach/cut-off", "secrets", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/environments/{env}/reach/cut-off", "secrets", ADMIN, one(&["project"], "project")),
    row("POST", "/v1/secrets/projects/{project}/shares", "secrets", REVEAL, one(&["project"], "project")),
    // A secret request asks someone outside roll for a folder's values: its answer is written as
    // the asker, so asking is a write there; the list shows names only; cancelling is the asker's
    // (or the folder admin's: the product checks).
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/value-requests", "secrets", W, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("GET", "/v1/secrets/projects/{project}/value-requests", "secrets", R, one(&["project"], "project")),
    row("DELETE", "/v1/secrets/projects/{project}/value-requests/{id}", "secrets", W, one(&["project"], "project")),
    // Binding a key to hosts decides where its value goes through a placeholder (docs/secrets.md,
    // "Using a key without seeing it"): the folder's admin, as purge is.
    row("PUT", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/hosts", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("DELETE", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/hosts", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    // Purging a secret's history can't be undone.
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/purge", "secrets", ADMIN, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    // A change request's current and proposed values, for its approvers.
    row("GET", "/v1/secrets/projects/{project}/environments/{env}/requests/{id}/values", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/resolve", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    // Comparing a local copy with a folder's values (by fingerprint, names only) tests a guess:
    // reveal there, as reading the values is.
    row("POST", "/v1/secrets/projects/{project}/environments/{env}/diff", "secrets", REVEAL, one(&["project"], "project")).env(EnvFrom::Param("env")).path_query("path"),
    // ── apps (feature/roll-apps, docs/apps/http-api.md) ──
    row("GET", "/v1/apps/services", "apps", R, list("id")),
    // A service from a repo or an image deploys production at once; an upload one waits for `roll up`.
    // Its `links` hand each linked resource's credentials to the service: reveal on each too.
    row("POST", "/v1/apps/services", "apps", Act::Body(apps_create_action), Tgt::Product).env(EnvFrom::Fixed("production")).lends(Lend::Each(apps_create_lends)),
    row("POST", "/v1/apps/projects/{project}/services", "apps", Act::Body(apps_create_action), Tgt::Create { project: Some("project"), place: None, id_field: "id" })
        .env(EnvFrom::Fixed("production"))
        .lends(Lend::Each(apps_create_lends)),
    row("GET", "/v1/apps/events", "apps", R, Tgt::Product),
    // A project's notifications: the URL is stored sealed and never answered; a webhook's signing
    // secret comes back once, so making one is reveal (Slack and Discord ones are write).
    // A project's spend limits: at the hard limit the whole project pauses, so setting one is admin.
    row("GET", "/v1/apps/projects/{project}/limits", "apps", R, one(&["project"], "project")),
    row("GET", "/v1/apps/projects/{project}/usage", "apps", R, one(&["project"], "project")),
    row("PUT", "/v1/apps/projects/{project}/limits", "apps", ADMIN, one(&["project"], "project")),
    // Which images may run (signed ones): a supply-chain policy, an admin's call; reading it is read.
    row("GET", "/v1/apps/projects/{project}/signing", "apps", R, one(&["project"], "project")),
    row("PUT", "/v1/apps/projects/{project}/signing", "apps", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/apps/projects/{project}/signing", "apps", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/apps/projects/{project}/notifications", "apps", R, one(&["project"], "project")),
    row("POST", "/v1/apps/projects/{project}/notifications", "apps", Act::Body(apps_notification_action), one(&["project"], "project")),
    row("DELETE", "/v1/apps/projects/{project}/notifications/{nid}", "apps", W, one(&["project"], "project")),
    // An edit can send a webhook more (events, environments, services): reveal, as a drain's edit is.
    row("PATCH", "/v1/apps/projects/{project}/notifications/{nid}", "apps", REVEAL, one(&["project"], "project")),
    row("POST", "/v1/apps/projects/{project}/notifications/{nid}/test", "apps", W, one(&["project"], "project")),
    row("GET", "/v1/apps/projects/{project}/notifications/{nid}/deliveries", "apps", R, one(&["project"], "project")),
    // A project's log drains: every service's output POSTed out of roll, signed. Adding one sends
    // runtime output elsewhere and its signing secret comes back once: reveal. The URL is never
    // answered (only its host).
    row("GET", "/v1/apps/projects/{project}/drains", "apps", R, one(&["project"], "project")),
    // The project's preview domain: which names its preview environments answer under.
    // A public page of the project's checks and heartbeats (names and up or down only).
    row("GET", "/v1/apps/projects/{project}/status-page", "apps", R, one(&["project"], "project")),
    // Publishing makes the project's service names public: a person's call (MCP's review).
    row("PUT", "/v1/apps/projects/{project}/status-page", "apps", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/apps/projects/{project}/status-page", "apps", W, one(&["project"], "project")),
    row("GET", "/v1/apps/projects/{project}/status-page/incidents", "apps", R, one(&["project"], "project")),
    row("POST", "/v1/apps/projects/{project}/status-page/incidents", "apps", W, one(&["project"], "project")),
    row("POST", "/v1/apps/projects/{project}/status-page/incidents/{iid}/updates", "apps", W, one(&["project"], "project")),
    row("GET", "/v1/apps/projects/{project}/preview-domain", "apps", R, one(&["project"], "project")),
    row("POST", "/v1/apps/projects/{project}/preview-domain", "apps", W, one(&["project"], "project")),
    row("DELETE", "/v1/apps/projects/{project}/preview-domain", "apps", D, one(&["project"], "project")),
    row("POST", "/v1/apps/projects/{project}/preview-domain/verify", "apps", W, one(&["project"], "project")),
    // Its services as roll.toml: settings and variables' names, never a value.
    row("POST", "/v1/apps/projects/{project}/export", "apps", R, one(&["project"], "project")),
    row("POST", "/v1/apps/projects/{project}/drains", "apps", REVEAL, one(&["project"], "project")).lends(Lend::Each(apps_drain_bucket)),
    row("DELETE", "/v1/apps/projects/{project}/drains/{did}", "apps", W, one(&["project"], "project")),
    // An edit can send more out (another source, every environment): reveal, as making one is.
    row("PATCH", "/v1/apps/projects/{project}/drains/{did}", "apps", REVEAL, one(&["project"], "project")),
    row("POST", "/v1/apps/projects/{project}/drains/{did}/test", "apps", W, one(&["project"], "project")),
    // A private registry's login (never answered back), and unlinking a database: settings.
    row("PUT", "/v1/apps/services/{id}/registry", "apps", W, one(PG, "service")),
    row("DELETE", "/v1/apps/services/{id}/registry", "apps", W, one(PG, "service")),
    // A private Git repository's login (never answered back), as a registry's.
    row("PUT", "/v1/apps/services/{id}/git-login", "apps", Act::Body(apps_git_login_action), one(PG, "service")),
    row("DELETE", "/v1/apps/services/{id}/git-login", "apps", W, one(PG, "service")),
    // A link puts the linked resource's address and password in the service's variables, where
    // its code reads them: reveal on that resource too.
    row("PUT", "/v1/apps/services/{id}/links/{kind}/{ref}", "apps", W, one(PG, "service")).lends(Lend::Path("kind", "ref")),
    row("DELETE", "/v1/apps/services/{id}/links/{kind}/{ref}", "apps", W, one(PG, "service")),
    row("*", "/v1/apps/services/{id}", "apps", M, one(PG, "service")),
    // A worker scaled on a stream group's lag has apps read that stream for it, and a web service's
    // pushes consume one (and dead-letter into it): read there too, write with a dead letter.
    row("PATCH", "/v1/apps/services/{id}", "apps", Act::Body(apps_patch_action), one(PG, "service")).lends(Lend::Each(apps_patch_lends)),
    row("*", "/v1/apps/services/{id}/", "apps", M, one(PG, "service")),
    // A restore puts a deleted service's deployments live again, production's too: a deploy.
    row("POST", "/v1/apps/services/{id}/restore", "apps", DEPLOY, one(PG, "service")).env(EnvFrom::Fixed("production")),
    // Moving a service to another project: admin on it (and, asked inside, on it there too).
    row("POST", "/v1/apps/services/{id}/move", "apps", ADMIN, one(PG, "service")),
    // `variables_from` copies another environment's values into this one: reveal there too.
    // A new heartbeat's ping URL (its token) is in the PUT's answer, once: a credential.
    // Setting or lifting a deploy freeze is admin (lifting one would get past it).
    row("*", "/v1/apps/services/{id}/environments/{env}", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")).admin_if_field(&["freeze", "approvals", "closed_by", "data"]).copies_from(CopyFrom::Body("variables_from")).reveals(&["heartbeat_urls"]),
    row("GET", "/v1/apps/services/{id}/environments/{env}", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}", "apps", D, one(PG, "service")).env(EnvFrom::Param("env")),
    row("GET", "/v1/apps/services/{id}/environments/{env}/", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    // A person's deploy past a freeze (`X-Roll-Override: 1`) is admin; otherwise as any deploy.
    row("POST", "/v1/apps/services/{id}/environments/{env}/deploys", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")).admin_if("x-roll-override"),
    row("POST", "/v1/apps/services/{id}/environments/{env}/redeploy", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")).admin_if("x-roll-override"),
    row("POST", "/v1/apps/services/{id}/environments/{env}/promote", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")).admin_if("x-roll-override"),
    // "Why did it fail?": reads a deploy's logs and keeps the repo owner's model's answer (metered
    // on their usage), in any environment: write, never production's deploy.
    row("POST", "/v1/apps/services/{id}/environments/{env}/deploys/{dep}/explain", "apps", W, one(PG, "service")).env(EnvFrom::Param("env")),
    row("POST", "/v1/apps/services/{id}/environments/{env}/deploys/{dep}/approve", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("POST", "/v1/apps/services/{id}/environments/{env}/deploys/{dep}/rollback", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")).admin_if("x-roll-override"),
    // An error group resolved, muted or open again: the team's triage, a write in any environment.
    row("PATCH", "/v1/apps/services/{id}/environments/{env}/errors/{fingerprint}", "apps", W, one(PG, "service")).env(EnvFrom::Param("env")),
    row("POST", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("PUT", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("PATCH", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}/", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}/domains/{host}", "apps", D, one(PG, "service")).env(EnvFrom::Param("env")),
    // A volume's backups: making one changes nothing the app sees (write, production too);
    // restoring one replaces the volume's files (delete, as restoring over a branch is).
    row("POST", "/v1/apps/services/{id}/environments/{env}/volume/backups", "apps", W, one(PG, "service")).env(EnvFrom::Param("env")),
    // `from` restores another environment's backup here (its files are that environment's data):
    // reveal there too.
    row("POST", "/v1/apps/services/{id}/environments/{env}/volume/backups/{bid}/restore", "apps", D, one(PG, "service")).env(EnvFrom::Param("env")).copies_from(CopyFrom::Body("from")),
    // A shell runs any command where the service runs, with its variables (`env` prints them all):
    // admin in every environment, as an agent's shell is: its WebSocket, and all of a session
    // (opening, keys, screen, resizing, closing: what may close an admin's shell is admin too).
    row("GET", "/v1/apps/services/{id}/environments/{env}/shell", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    // A tunnel reaches the service's port past its allow list, access token and maintenance
    // (a private service's admin page, say): admin, as a shell is.
    row("GET", "/v1/apps/services/{id}/environments/{env}/tunnel", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    // Files in an instance (`roll apps cp`): read, anything the app keeps (its secrets too);
    // written, anything it runs next: admin, as a shell is.
    row("GET", "/v1/apps/services/{id}/environments/{env}/files", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("PUT", "/v1/apps/services/{id}/environments/{env}/files", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
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
    // A plan or an import of a roll code repo reads it: read there too.
    row("POST", "/v1/apps/plan", "apps", R, Tgt::AnyGrant).lends(Lend::Each(apps_repo)),
    row("POST", "/v1/apps/quote", "apps", R, Tgt::AnyGrant),
    row("POST", "/v1/apps/import", "apps", R, Tgt::AnyGrant).lends(Lend::Each(apps_repo)),
    row("GET", "/v1/apps/projects/{project}/services", "apps", R, one(&["project"], "project")),
    row("GET", "/v1/apps/services/{id}/environments/{env}/variables/values", "apps", REVEAL, one(PG, "service")).env(EnvFrom::Param("env")),
    // A static site's forms: what people sent through them is theirs to see only with reveal.
    row("GET", "/v1/apps/services/{id}/environments/{env}/forms", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    row("GET", "/v1/apps/services/{id}/environments/{env}/forms/{form}/submissions", "apps", REVEAL, one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}/forms/{form}/submissions/{sub}", "apps", D, one(PG, "service")).env(EnvFrom::Param("env")),
    // `copy_from` copies another environment's values into this one: reveal there too.
    row("PATCH", "/v1/apps/services/{id}/environments/{env}/variables", "apps", Act::ProductionIf("env"), one(PG, "service")).env(EnvFrom::Param("env")).copies_from(CopyFrom::Body("copy_from")),
    // What the next deploy changes: names of variables and settings, never a value.
    row("GET", "/v1/apps/services/{id}/environments/{env}/pending", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    // A job is a command run with the environment's variables: its output can be them.
    row("GET", "/v1/apps/services/{id}/environments/{env}/jobs/{job}/logs", "apps", REVEAL, one(PG, "service")).env(EnvFrom::Param("env")),
    // A private network reaches into the customer's own (its WireGuard config read as whoever
    // set it): admin to set or remove, as a shell is; its routes and reference are read.
    row("GET", "/v1/apps/services/{id}/environments/{env}/private-network", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    row("PUT", "/v1/apps/services/{id}/environments/{env}/private-network", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}/private-network", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    // A sandbox runs any command, with the environment's variables when asked, and reads back
    // any file: admin, as a shell is (seeing which there are is read).
    row("GET", "/v1/apps/services/{id}/environments/{env}/sandboxes", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    row("POST", "/v1/apps/services/{id}/environments/{env}/sandboxes", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("GET", "/v1/apps/services/{id}/environments/{env}/sandboxes/{sbx}", "apps", R, one(PG, "service")).env(EnvFrom::Param("env")),
    row("DELETE", "/v1/apps/services/{id}/environments/{env}/sandboxes/{sbx}", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("POST", "/v1/apps/services/{id}/environments/{env}/sandboxes/{sbx}/exec", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("GET", "/v1/apps/services/{id}/environments/{env}/sandboxes/{sbx}/files", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    row("PUT", "/v1/apps/services/{id}/environments/{env}/sandboxes/{sbx}/files", "apps", ADMIN, one(PG, "service")).env(EnvFrom::Param("env")),
    // ── previews (feature/roll-previews, docs/previews.md) ──
    // A preview's link carries its access token (`share_url`, also each service's and earlier
    // ones'): the link is a credential, as its QR code is.
    row("GET", "/v1/previews", "previews", R, list("id")).reveals(&["share_url"]),
    row("POST", "/v1/previews", "previews", W, Tgt::Create { project: None, place: None, id_field: "id" }).env(EnvFrom::Fixed("preview")),
    // Each event is its project's: a member, or a token held to some projects, follows theirs
    // (a deleted preview's event too: its project still resolves).
    row("GET", "/v1/previews/events", "previews", R, Tgt::List { items: Some("events"), id_field: "project", kind: Some("project") }),
    row("*", "/v1/previews/projects/{project}/", "previews", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/previews/projects/{project}/", "previews", R, one(&["project"], "project")),
    // A project's preview settings carry the variable values every preview's services get
    // (`env`, and per service `env_for`): API keys among them. A preview's own are reveal.
    row("GET", "/v1/previews/projects/{project}/settings", "previews", R, one(&["project"], "project")).reveals(&["env", "env_for"]),
    // A project's CI bypass secret for private previews: made (and shown once) is reveal.
    row("POST", "/v1/previews/projects/{project}/bypass", "previews", REVEAL, one(&["project"], "project")),
    row("GET", "/v1/previews/projects/{project}/bypass", "previews", ADMIN, one(&["project"], "project")),
    row("DELETE", "/v1/previews/projects/{project}/bypass", "previews", ADMIN, one(&["project"], "project")),
    // A deploy hook's answer is its secret URL (shown once), as the bypass's is.
    row("POST", "/v1/previews/projects/{project}/hooks", "previews", REVEAL, one(&["project"], "project")),
    row("DELETE", "/v1/previews/projects/{project}/hooks/{hook}", "previews", ADMIN, one(&["project"], "project")),
    row("GET", "/v1/previews/{id}", "previews", R, one(PG, "preview")).env(EnvFrom::Fixed("preview")).reveals(&["share_url"]),
    row("DELETE", "/v1/previews/{id}", "previews", D, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("*", "/v1/previews/{id}/", "previews", M, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("POST", "/v1/previews/{id}/reset", "previews", D, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // A share link lets someone outside the team in, its token only in the answer: reveal, as the
    // bypass secret is. Listing (no tokens) is read and revoking one is delete, by the prefix.
    row("POST", "/v1/previews/{id}/shares", "previews", REVEAL, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // A preview's own variables come back with their values: reveal (setting them is write, by
    // the prefix).
    row("GET", "/v1/previews/{id}/env", "previews", REVEAL, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // The share link with its access token, as a QR code for a phone (previews-next): the token in
    // another encoding, so reveal as the link itself is.
    row("GET", "/v1/previews/{id}/qr.png", "previews", REVEAL, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // Each data branch's connection as the preview's services get it (the branches' own
    // credentials, never a parent's; previews-next): reveal, as the preview's env is.
    row("GET", "/v1/previews/{id}/connect", "previews", REVEAL, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // A shell in a preview's running service runs anything with its variables and branches, as
    // roll apps' shells do: admin, every route of it (what closes an admin's shell is admin).
    row("POST", "/v1/previews/{id}/shell/sessions", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("GET", "/v1/previews/{id}/shell/sessions/{sh}/screen", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("POST", "/v1/previews/{id}/shell/sessions/{sh}/keys", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("POST", "/v1/previews/{id}/shell/sessions/{sh}/resize", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("DELETE", "/v1/previews/{id}/shell/sessions/{sh}", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // Files in a preview's service (`roll preview cp`): read, anything it keeps (its variables
    // too); written, anything it runs next: admin, as the shell is.
    row("GET", "/v1/previews/{id}/files", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // A port of a preview's service on a laptop (`roll preview forward`): any port it listens on,
    // past its access: admin, as roll apps' tunnel is.
    row("GET", "/v1/previews/{id}/forward", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // A laptop's process standing in for a preview's service (`roll preview swap`): it gets the
    // preview's requests, its visitors' included: admin, as the shell is.
    row("GET", "/v1/previews/{id}/swap", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // A page of a preview's service read through roll (roll MCP's `previews_fetch`): what its
    // visitors see, which can be copies of production's people (masked or not): reveal, as its
    // mail is.
    row("GET", "/v1/previews/{id}/fetch", "previews", REVEAL, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // A look at a preview's data branch (its tables, a table's first rows): copies of
    // production's (masked or not): reveal.
    row("GET", "/v1/previews/{id}/data/{resource}", "previews", REVEAL, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("PUT", "/v1/previews/{id}/files", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // The mail a preview's services sent, caught and never delivered (previews-next): it can hold
    // copies of production's people, so reveal, as the preview's env is.
    row("GET", "/v1/previews/{id}/mail", "previews", REVEAL, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("GET", "/v1/previews/{id}/mail/{mail}", "previews", REVEAL, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // Accepting a visual change is a person's call (with `visual_review = "required"` previews
    // refuses tokens anyway): admin, so a write token can't wave its own diff through.
    row("POST", "/v1/previews/{id}/visual/accept", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    row("POST", "/v1/previews/{id}/regressions/accept", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // Letting a fork's pull request run with the project's data (a Git URL service's preview,
    // roll code's `approve-run`): a person's call, as accepting a visual change is.
    row("POST", "/v1/previews/{id}/approve", "previews", ADMIN, one(PG, "preview")).env(EnvFrom::Fixed("preview")),
    // ── roll code ──
    row("GET", "/v1/code", "code", R, Tgt::AnyGrant),
    row("GET", "/v1/code/health", "code", R, Tgt::AnyGrant),
    row("GET", "/v1/code/pricing", "code", R, Tgt::AnyGrant),
    row("GET", "/v1/code/names/{name}", "code", R, Tgt::Product),
    row("GET", "/v1/code/names/{owner}/{repo}", "code", R, one(REPO, "repo")),
    row("GET", "/v1/code/repos", "code", R, list("full_name")),
    // Changes and work items across the caller's repos (the `search` tool): each result is its
    // repo's, so a token narrowed to some repos keeps only theirs.
    row("GET", "/v1/code/search", "code", R, Tgt::List { items: Some("results"), id_field: "repo", kind: None }),
    row("GET", "/v1/code/documents/{id}", "code", R, one(PG, "document")),
    row("POST", "/v1/code/repos", "code", Act::Body(code_repo_action), create("repo", "full_name")),
    row("GET", "/v1/code/repos/{owner}/{repo}", "code", R, one(REPO, "repo")),
    row("PATCH", "/v1/code/repos/{owner}/{repo}", "code", ADMIN, one(REPO, "repo")),
    row("DELETE", "/v1/code/repos/{owner}/{repo}", "code", D, one(REPO, "repo")),
    row("*", "/v1/code/repos/{owner}/{repo}/", "code", M, one(REPO, "repo")),
    // Where a repo's committed secrets are (key, environment, path, line; never the value): it
    // tells a reader which line holds production's key, so it's the repo's managers' (roll code
    // checks manage too): admin.
    row("*", "/v1/code/repos/{owner}/{repo}/leaks", "code", ADMIN, one(REPO, "repo")),
    row("POST", "/v1/code/repos/{owner}/{repo}/land", "code", Act::Body(code_land_action), one(REPO, "repo")),
    row("POST", "/v1/code/repos/{owner}/{repo}/queue/pause", "code", ADMIN, one(REPO, "repo")),
    row("POST", "/v1/code/repos/{owner}/{repo}/queue/resume", "code", ADMIN, one(REPO, "repo")),
    row("*", "/v1/code/repos/{owner}/{repo}/webhooks", "code", ADMIN, one(REPO, "repo")),
    row("*", "/v1/code/repos/{owner}/{repo}/webhooks/", "code", ADMIN, one(REPO, "repo")),
    row("PATCH", "/v1/code/repos/{owner}/{repo}/reviewer", "code", ADMIN, one(REPO, "repo")),
    row("PATCH", "/v1/code/repos/{owner}/{repo}/review-settings", "code", ADMIN, one(REPO, "repo")),
    row("*", "/v1/code/changes/{id}", "code", M, one(PG, "change")),
    row("*", "/v1/code/changes/{id}/", "code", M, one(PG, "change")),
    row("POST", "/v1/code/changes/{id}/land", "code", Act::Body(code_land_action), one(PG, "change")),
    row("POST", "/v1/code/changes/{id}/enqueue", "code", DEPLOY, one(PG, "change")),
    row("PATCH", "/v1/code/changes/{id}/enqueue", "code", DEPLOY, one(PG, "change")),
    // Running a fork's code with the repo's data is the repo's owner's call.
    row("POST", "/v1/code/changes/{id}/approve-run", "code", ADMIN, one(PG, "change")),
    // "Store in roll": a change's credential into roll secrets. A writer's (roll secrets then asks
    // the same grant for `write` in the folder); the target lists the project's environments, so
    // it's a writer's too.
    row("GET", "/v1/code/changes/{id}/unstored/{fingerprint}", "code", W, one(PG, "change")),
    row("POST", "/v1/code/changes/{id}/unstored/{fingerprint}/store", "code", W, one(PG, "change")),
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
    // Adding a machine mints its long-lived machine token, as approving a login does.
    row("POST", "/v1/code/agents/{owner}/machines", "agents", ADMIN, one(&["owner"], "owner")),
    // A share is an anonymous link to a copy of the thread (prompts, code, tool output): how its
    // content leaves the owner, as previews' and secrets' shares are.
    row("POST", "/v1/code/agents/{owner}/threads/{thread}/shares", "agents", REVEAL, one(&["owner"], "owner")),
    // The share list answers each share's id and link, and the id alone opens the copy: reveal.
    row("GET", "/v1/code/agents/{owner}/shares", "agents", REVEAL, one(&["owner"], "owner")),
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
        for r in ["values", "secrets/{name}/value", "secrets/{name}/totp", "secrets/{name}/versions/{v}/value", "requests/{id}/values"] {
            let route = format!("/v1/secrets/projects/{{project}}/environments/{{env}}/{r}");
            let f = by_path("GET", &route.replace('{', "x").replace('}', "")).unwrap();
            assert_eq!(f.row.action("GET", &[], None), Action::Reveal, "{route}");
        }
        // Comparing with values (a guess tested) is reveal too, not the POST prefix's write.
        let f = by_path("POST", "/v1/secrets/projects/x/environments/x/diff").unwrap();
        assert_eq!(f.row.action("POST", &[], None), Action::Reveal);
        assert_eq!(apps_create_action(&serde_json::json!({ "source": { "kind": "repo", "repo": "acme/site" } })), Action::Deploy);
        assert_eq!(apps_create_action(&serde_json::json!({ "source": { "kind": "upload" } })), Action::Write);
        assert_eq!(apps_patch_action(&serde_json::json!({ "run": { "public": false } })), Action::Deploy);
        assert_eq!(apps_patch_action(&serde_json::json!({ "run": { "min_instances": 2, "memory_mb": 512 } })), Action::Write);
        assert_eq!(apps_patch_action(&serde_json::json!({ "run": { "pre_deploy": "./migrate" } })), Action::Deploy);
        assert_eq!(apps_patch_action(&serde_json::json!({ "run": { "schedules": [{ "name": "nightly", "schedule": "@daily", "command": "./export" }] } })), Action::Deploy);
        // A port on the internet is who reaches it, as public is; a key apps adds later is deploy until listed.
        assert_eq!(apps_patch_action(&serde_json::json!({ "run": { "tcp": [5432] } })), Action::Deploy);
        assert_eq!(apps_patch_action(&serde_json::json!({ "run": { "rollback_on_crash": true } })), Action::Deploy);
        assert_eq!(apps_patch_action(&serde_json::json!({ "run": { "a_setting_from_next_week": true } })), Action::Deploy);
        assert_eq!(apps_patch_action(&serde_json::json!({ "run": { "memory_mb": 1024, "max_instances": 4, "alerts": { "memory_pct": 90 } } })), Action::Write);
        assert_eq!(apps_patch_action(&serde_json::json!({ "source": { "kind": "image", "image": "x" } })), Action::Deploy);
        assert_eq!(bulk_action(b"{\"index\":{\"_id\":\"1\"}}\n{\"title\":\"a\"}\n"), Action::Write);
        assert_eq!(bulk_action(b"{\"index\":{}}\n{\"t\":1}\n{\"delete\":{\"_id\":\"2\"}}\n"), Action::Delete);
        assert_eq!(bulk_action(b"{\"update\":{\"_id\":\"3\"}}\n{\"script\":{\"source\":\"ctx.op = 'delete'\"}}\n"), Action::Delete);
        assert_eq!(index_settings_action(br#"{"index.roll.suggestions.enabled": true, "index": {"number_of_replicas": 0}}"#), Action::Write);
        let lifecycles: [&[u8]; 7] = [
            br#"{"index.lifecycle.name": "30-days-default"}"#,
            br#"{"settings": {"index": {"lifecycle": {"name": "x"}}}}"#,
            br#"{"lifecycle.origination_date": 0}"#,
            br#"{"index.plugins.index_state_management.policy_id": "p"}"#,
            br#"{"index.refresh_interval": "1s", "index.refresh_interval": "1s", "index.lifecycle.indexing_complete": true}"#,
            br#"{"index.lifecycle.name": "x", "index.lifecycle.name": null}"#,
            b"not json",
        ];
        for lifecycle in lifecycles {
            assert_eq!(index_settings_action(lifecycle), Action::Delete, "{}", String::from_utf8_lossy(lifecycle));
        }
        assert_eq!(apps_notification_action(&serde_json::json!({ "url": "https://example.com/h" })), Action::Reveal);
        assert_eq!(apps_git_login_action(&serde_json::json!({ "username": "x", "secret": "GIT_TOKEN" })), Action::Deploy, "a project secret sent to the source's host");
        assert_eq!(apps_git_login_action(&serde_json::json!({ "username": "x", "password": "p" })), Action::Write);
        assert_eq!(apps_notification_action(&serde_json::json!({ "kind": "slack", "url": "https://hooks.slack.com/x" })), Action::Write);
        let dep = find("POST", "/v1/apps/services/{id}/environments/{env}/deploys", "x").unwrap();
        assert_eq!(dep.row.action("POST", &[("env".into(), "production".into())], None), Action::Deploy);
        // Past a deploy freeze: a person's call (admin), never a deploy token's.
        let mut over = axum::http::HeaderMap::new();
        over.insert("x-roll-override", axum::http::HeaderValue::from_static("1"));
        for route in ["deploys", "redeploy", "promote", "deploys/{dep}/rollback"] {
            let f = find("POST", &format!("/v1/apps/services/{{id}}/environments/{{env}}/{route}"), "x").unwrap();
            assert_eq!(f.row.action_with("POST", &[("env".into(), "staging".into())], None, &over), Action::Admin, "{route}");
        }
        // Lifting a freeze would get past it, and so would turning approvals off: admin too, any
        // environment.
        let env = find("PUT", "/v1/apps/services/{id}/environments/{env}", "x").unwrap();
        let none = axum::http::HeaderMap::new();
        assert_eq!(env.row.action_with("PUT", &[("env".into(), "staging".into())], Some(&serde_json::json!({ "freeze": null })), &none), Action::Admin);
        assert_eq!(env.row.action_with("PUT", &[("env".into(), "production".into())], Some(&serde_json::json!({ "approvals": 0 })), &none), Action::Admin);
        // Naming whose placeholders a closed environment uses is the operator's call, never write's.
        assert_eq!(env.row.action_with("PUT", &[("env".into(), "staging".into())], Some(&serde_json::json!({ "closed_by": null })), &none), Action::Admin);
        assert_eq!(env.row.action_with("PUT", &[("env".into(), "staging".into())], Some(&serde_json::json!({ "vitals": true })), &none), Action::Write);
        assert_eq!(dep.row.action("POST", &[("env".into(), "staging".into())], None), Action::Write);
        assert_eq!(sql_action(&serde_json::json!({ "sql": "select 1", "read_only": true })), Action::Read);
        assert_eq!(sql_action(&serde_json::json!({ "sql": "select 1" })), Action::Write);
        // Without `admin`, pg-control runs it as the database's owner role, never the superuser.
        assert_eq!(by_path("POST", "/v1/endpoints/ep_x/query").map(|f| f.row.capable), Some(&[Action::Admin, Action::Delete][..]));
        // A repo apps reads for the caller needs read on it: a plan's, an import's, a source's.
        let repo = |r: &str| vec![Lent { kind: "repo".into(), r#ref: r.into(), action: Action::Read }];
        assert_eq!(apps_repo(&serde_json::json!({ "repo": "acme/web", "revision": "main" })), repo("acme/web"));
        assert_eq!(apps_repo(&serde_json::json!({ "build": { "submodule_repos": ["acme/lib"] } })), repo("acme/lib"));
        assert_eq!(apps_repo(&serde_json::json!({ "source": { "kind": "repo", "repo": "acme/api" } })), repo("acme/api"));
        assert!(apps_repo(&serde_json::json!({ "source": { "kind": "image", "image": "nginx" } })).is_empty());
        assert_eq!(link_target("repo"), Some(("code", "repo")));
        // Loosening a NoSQL database's masking or read-only flag is admin; its other settings write.
        // Credentials in read answers are hidden from tokens that may not reveal them.
        let reveals = |m: &str, p: &str| by_path(m, p).unwrap().row.reveals;
        assert!(reveals("GET", "/v1/previews/projects/prj_1/settings").contains(&"env"));
        assert!(reveals("GET", "/v1/tasks/messages/msg_1").contains(&"headers") && reveals("GET", "/v1/tasks/schedules").contains(&"headers"));
        assert!(reveals("PUT", "/v1/apps/services/svc_1/environments/production").contains(&"heartbeat_urls"), "a heartbeat's ping URL is shown once, to who may reveal");
        assert_eq!(by_path("GET", "/v1/code/agents/acme/shares").unwrap().row.action("GET", &[], None), Action::Reveal);
        // Connecting a GitHub repository, landing over the rules, and dropping a stream's
        // records aren't the plain write or deploy their routes otherwise are.
        assert_eq!(code_repo_action(&serde_json::json!({ "name": "web", "source": { "github": { "repo": "a/b" } } })), Action::Admin);
        assert_eq!(code_repo_action(&serde_json::json!({ "name": "web", "source": "native" })), Action::Write);
        assert_eq!(code_land_action(&serde_json::json!({ "bypass": true })), Action::Admin);
        assert_eq!(code_land_action(&serde_json::json!({})), Action::Deploy);
        assert_eq!(stream_settings_action(&serde_json::json!({ "settings": { "retention_ms": 1 } })), Action::Delete);
        assert_eq!(stream_settings_action(&serde_json::json!({ "settings": { "max_partitions": 12 } })), Action::Write);
        assert_eq!(topic_change_action(&serde_json::json!({ "configs": { "retention.ms": "1" } })), Action::Delete);
        assert_eq!(topic_change_action(&serde_json::json!({ "partitions": 6, "configs": { "max.message.bytes": "2000000" } })), Action::Write);
        assert_eq!(nosql_patch_action(&serde_json::json!({ "masking": {} })), Action::Admin);
        assert_eq!(nosql_patch_action(&serde_json::json!({ "read_only": false })), Action::Admin);
        assert_eq!(nosql_patch_action(&serde_json::json!({ "read_only": true, "idle_timeout": 300 })), Action::Write);
        assert_eq!(redis_action(&serde_json::json!({ "command": ["get", "k"] })), Action::Read);
        assert_eq!(redis_action(&serde_json::json!({ "command": ["FLUSHALL"] })), Action::Delete);
        assert_eq!(redis_action(&serde_json::json!({ "command": ["set", "k", "v"] })), Action::Write);
        // Reading everyone's traffic or the whole dataset is the server's, not a write.
        assert_eq!(redis_action(&serde_json::json!({ "command": ["MONITOR"] })), Action::Admin);
        assert_eq!(redis_action(&serde_json::json!({ "command": "client kill 10.0.0.1:5000" })), Action::Admin);
        assert_eq!(redis_action(&serde_json::json!({ "command": ["SWAPDB", "0", "1"] })), Action::Delete);
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
        assert_eq!(action("GET", "/v1/previews/{id}/env", "/v1/previews/pv_1/env"), Some(Action::Reveal), "a preview's variables have their values");
        assert_eq!(action("GET", "/v1/previews/{id}/qr.png", "/v1/previews/pv_1/qr.png"), Some(Action::Reveal), "a QR code is the share link's token");
        assert_eq!(action("POST", "/v1/previews/{id}/visual/accept", "/v1/previews/pv_1/visual/accept"), Some(Action::Admin));
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
