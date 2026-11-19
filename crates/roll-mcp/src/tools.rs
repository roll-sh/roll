//! Every tool roll's MCP server has of its own (roll code's are fronted from `code_control::mcp`).
//! A tool is one route: its method and path, and where each argument goes (the path, the query,
//! the JSON body, a header, or the raw body). What a tool may do is what its route is in the
//! permission catalogue (`roll_access::catalog`), and its annotations follow from that.

use roll_access::catalog;
use roll_auth::scope::Action;
use serde_json::{Map, Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum At {
    /// `{name}` in the path, one percent-encoded segment.
    Path,
    Query,
    Body,
    Header(&'static str),
    /// The request's whole body, as text.
    RawText,
    /// The request's whole body, JSON text sent as `application/json` (a search document).
    RawJson,
    /// The request's whole body, NDJSON text sent as `application/x-ndjson` (a search bulk).
    RawNdjson,
    /// The request's whole body, from base64.
    RawB64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    Str,
    Int,
    Bool,
    Obj,
    Arr,
    Num,
    /// More than one shape (`true`, `false` or an object): no `type` in the schema.
    Any,
}

#[derive(Clone, Copy, Debug)]
pub struct P {
    pub name: &'static str,
    pub ty: Ty,
    pub at: At,
    pub required: bool,
    pub desc: &'static str,
}

/// Beyond what the catalogue says: arbitrary statements that can drop data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hint {
    Auto,
    Destructive,
}

#[derive(Clone, Copy, Debug)]
pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub product: &'static str,
    pub method: &'static str,
    pub path: &'static str,
    pub params: &'static [P],
    /// Fields always in the JSON body (`read_only: true` for read-only SQL).
    pub fixed: Option<fn() -> Value>,
    /// The `sql` argument as the route gets it (`EXPLAIN (FORMAT JSON) <sql>`).
    pub wrap: Option<fn(&str) -> String>,
    pub hint: Hint,
    /// For a tool that waits for something (a run, a decision) up to a request's limit: whether
    /// its answer says the wait is over. As an MCP task, an answer that isn't is asked again, so
    /// the task waits as long as it takes (tasks::run).
    pub until: Option<fn(&Value) -> bool>,
}

pub enum Built {
    None,
    Json(Value),
    Raw(Vec<u8>, &'static str),
}

pub struct Request {
    pub uri: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Built,
}

fn seg(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl Tool {
    /// A path with every `{param}` filled with a sample: what the catalogue classifies.
    fn sample_path(&self) -> String {
        self.path.split('/').map(|s| if s.starts_with('{') { "x" } else { s }).collect::<Vec<_>>().join("/")
    }

    /// The actions this tool's route can be, from the catalogue (a body-decided route: what its
    /// fixed body makes it, or anything it could be). Worked out once ([`PREPARED`]).
    pub fn actions(&self) -> Vec<Action> {
        PREPARED.get(self.name).map_or_else(|| self.find_actions(), |p| p.actions.clone())
    }

    /// Whether any grant may call it (an `AnyGrant` route).
    pub fn open(&self) -> bool {
        PREPARED.get(self.name).map_or_else(|| self.find_open(), |p| p.open)
    }

    /// Its MCP definition, as `tools/list` shows it.
    pub fn definition(&self) -> Value {
        PREPARED.get(self.name).map_or_else(|| self.build_definition(), |p| p.definition.clone())
    }

    fn find_actions(&self) -> Vec<Action> {
        let Some(found) = catalog::by_path(self.method, &self.sample_path()) else { return Vec::new() };
        match found.row.act {
            catalog::Act::Body(_) => match self.fixed {
                Some(f) => vec![found.row.action(self.method, &[], Some(&f()))],
                None => vec![Action::Read, Action::Write, Action::Delete],
            },
            catalog::Act::ProductionIf(_) => vec![Action::Write, Action::Deploy],
            catalog::Act::Raw(_) => vec![Action::Write, Action::Delete],
            _ => vec![found.row.action(self.method, &[], None)],
        }
    }

    /// A route any connection may call (its own identity, its own approvals).
    fn find_open(&self) -> bool {
        catalog::by_path(self.method, &self.sample_path()).is_some_and(|f| matches!(f.row.target, catalog::Tgt::AnyGrant))
    }

    fn build_definition(&self) -> Value {
        let mut props = Map::new();
        let mut required = Vec::new();
        for p in self.params {
            let ty = match p.ty {
                Ty::Str => "string",
                Ty::Int => "integer",
                Ty::Bool => "boolean",
                Ty::Obj => "object",
                Ty::Arr => "array",
                Ty::Num => "number",
                Ty::Any => "",
            };
            let schema = if ty.is_empty() { json!({ "description": p.desc }) } else { json!({ "type": ty, "description": p.desc }) };
            props.insert(p.name.into(), schema);
            if p.required {
                required.push(p.name);
            }
        }
        props.insert(
            "approval".into(),
            // Every tool takes it: kept short (roll://guide and the server's instructions explain it).
            json!({ "type": "string", "description": "apr_… from an approval_pending answer: waits, then runs" }),
        );
        let actions = self.find_actions();
        // Read-only changes nothing and makes nothing: a reveal that's a POST (a presigned PUT URL,
        // a share link) makes something someone can use.
        let read_only = !actions.is_empty() && (actions.iter().all(|a| *a == Action::Read) || (self.method == "GET" && actions.iter().all(|a| matches!(a, Action::Read | Action::Reveal))));
        let destructive = self.hint == Hint::Destructive || actions.iter().any(|a| matches!(a, Action::Delete | Action::Deploy));
        let idempotent = read_only || self.method == "PUT" || self.method == "DELETE";
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": { "type": "object", "properties": props, "required": required, "additionalProperties": false },
            "annotations": { "readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": idempotent, "openWorldHint": false },
            "_meta": { "roll/product": self.product, "roll/route": format!("{} {}", self.method, self.path), "roll/actions": actions },
        })
    }

    /// The request this call makes.
    pub fn request(&self, args: &Value) -> Result<Request, String> {
        let obj = args.as_object().cloned().unwrap_or_default();
        for k in obj.keys() {
            if k != "approval" && !self.params.iter().any(|p| p.name == k) {
                return Err(format!("{}: unknown argument {k:?}", self.name));
            }
        }
        let mut path = self.path.to_string();
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        let mut has_query = false;
        let mut body = self.fixed.map(|f| f()).unwrap_or_else(|| json!({}));
        let mut body_used = self.fixed.is_some();
        let mut raw: Option<(Vec<u8>, &'static str)> = None;
        let mut headers = Vec::new();
        for p in self.params {
            let Some(v) = obj.get(p.name).filter(|v| !v.is_null()) else {
                if p.required {
                    return Err(format!("{} is required", p.name));
                }
                continue;
            };
            match p.at {
                At::Path => {
                    let s = as_text(v);
                    if s.is_empty() {
                        return Err(format!("{} can't be empty", p.name));
                    }
                    path = path.replace(&format!("{{{}}}", p.name), &seg(&s));
                }
                At::Query => {
                    query.append_pair(p.name, &as_text(v));
                    has_query = true;
                }
                At::Body => {
                    body[p.name] = v.clone();
                    body_used = true;
                }
                At::Header(h) => headers.push((h, as_text(v))),
                At::RawText => raw = Some((as_text(v).into_bytes(), "application/octet-stream")),
                At::RawJson => raw = Some((as_text(v).into_bytes(), "application/json")),
                At::RawNdjson => {
                    // Every line ends with a newline (Elasticsearch's bulk wants the last one too).
                    let mut t = as_text(v);
                    if !t.ends_with('\n') {
                        t.push('\n');
                    }
                    raw = Some((t.into_bytes(), "application/x-ndjson"));
                }
                At::RawB64 => {
                    use base64::Engine;
                    raw = Some((base64::engine::general_purpose::STANDARD.decode(as_text(v).trim()).map_err(|_| format!("{} isn't base64", p.name))?, "application/octet-stream"));
                }
            }
        }
        if let (Some(w), Some(sql)) = (self.wrap, body.get("sql").and_then(Value::as_str)) {
            body["sql"] = json!(w(sql));
        }
        let q = query.finish();
        let uri = if has_query { format!("{path}?{q}") } else { path };
        let body = match raw {
            Some((b, ct)) => Built::Raw(b, ct),
            None if body_used || matches!(self.method, "POST" | "PUT" | "PATCH") => Built::Json(body),
            None => Built::None,
        };
        Ok(Request { uri, headers, body })
    }
}

const fn req(name: &'static str, desc: &'static str) -> P {
    P { name, ty: Ty::Str, at: At::Path, required: true, desc }
}
const fn q(name: &'static str, ty: Ty, desc: &'static str) -> P {
    P { name, ty, at: At::Query, required: false, desc }
}
const fn b(name: &'static str, ty: Ty, desc: &'static str) -> P {
    P { name, ty, at: At::Body, required: false, desc }
}
const fn breq(name: &'static str, ty: Ty, desc: &'static str) -> P {
    P { name, ty, at: At::Body, required: true, desc }
}
const fn qreq(name: &'static str, desc: &'static str) -> P {
    P { name, ty: Ty::Str, at: At::Query, required: true, desc }
}
/// The workspace project a new resource goes in (`X-Roll-Project`): needed by project-scoped tokens.
/// A key a retried call sends again, so a route that honours it (apps' deploys) runs it once.
const IDEMPOTENCY: P = P { name: "idempotency_key", ty: Ty::Str, at: At::Header("idempotency-key"), required: false, desc: "Any unique string: a call retried with the same key runs once" };
const IN_PROJECT: P = P { name: "workspace_project", ty: Ty::Str, at: At::Header("x-roll-project"), required: false, desc: "Workspace project (prj_…) to put the new resource in" };

const fn t(name: &'static str, title: &'static str, description: &'static str, product: &'static str, method: &'static str, path: &'static str, params: &'static [P]) -> Tool {
    Tool { name, title, description, product, method, path, params, fixed: None, wrap: None, hint: Hint::Auto, until: None }
}

/// A tasks run that's over (`completed`, `failed` or `canceled`).
fn run_over(v: &Value) -> bool {
    matches!(v["status"].as_str(), Some("completed" | "failed" | "canceled"))
}

/// A deploy that's done one way or another (roll apps' final states); `queued`, `building` and
/// `deploying` aren't.
fn deploy_over(v: &Value) -> bool {
    matches!(v["status"].as_str(), Some("active" | "ready" | "failed" | "canceled" | "superseded" | "removed" | "inactive"))
}

/// A job that ended (roll apps' `succeeded`, `failed`, `skipped`, `timed_out`, `canceled`):
/// anything but `running`.
fn job_over(v: &Value) -> bool {
    v["status"].as_str().is_some_and(|s| s != "running")
}

/// An approval a person decided (or that expired): anything but `pending`.
fn decided(v: &Value) -> bool {
    v["state"].as_str().is_some_and(|s| s != "pending")
}

impl Tool {
    const fn wraps(mut self, f: fn(&str) -> String) -> Tool {
        self.wrap = Some(f);
        self
    }
    const fn fixed(mut self, f: fn() -> Value) -> Tool {
        self.fixed = Some(f);
        self
    }
    const fn destructive(mut self) -> Tool {
        self.hint = Hint::Destructive;
        self
    }
    const fn until(mut self, f: fn(&Value) -> bool) -> Tool {
        self.until = Some(f);
        self
    }
}

fn read_only() -> Value {
    json!({ "read_only": true })
}

/// SQL that may write (the route's default without `read_only`).
fn writes() -> Value {
    json!({})
}

fn describe_schema() -> Value {
    json!({
        "read_only": true,
        "sql": "SELECT table_schema, table_name, column_name, data_type, is_nullable, column_default FROM information_schema.columns \
WHERE table_schema NOT IN ('pg_catalog', 'information_schema') ORDER BY table_schema, table_name, ordinal_position",
    })
}

/// `EXPLAIN (FORMAT JSON)` of a statement: its plan, without running it.
fn explain(sql: &str) -> String {
    format!("EXPLAIN (FORMAT JSON) {}", sql.trim().trim_end_matches(';'))
}

/// Things worth fixing, as Supabase's advisors find them: indexes nothing uses, foreign keys no
/// index serves, tables without a primary key, and large tables read mostly by sequential scans.
fn advisors() -> Value {
    json!({ "read_only": true, "sql": "SELECT kind, \"table\", detail FROM ( \
  SELECT 'unused_index' AS kind, s.schemaname || '.' || s.relname AS \"table\", s.indexrelname || ' (' || pg_size_pretty(pg_relation_size(s.indexrelid)) || ', never used since stats were reset)' AS detail \
  FROM pg_stat_user_indexes s JOIN pg_index i ON i.indexrelid = s.indexrelid \
  WHERE s.idx_scan = 0 AND NOT i.indisunique AND NOT i.indisprimary \
  UNION ALL \
  SELECT 'unindexed_foreign_key', c.conrelid::regclass::text, c.conname || ' on (' || (SELECT string_agg(a.attname, ', ' ORDER BY a.attnum) FROM pg_attribute a WHERE a.attrelid = c.conrelid AND a.attnum = ANY (c.conkey)) || '): joins and cascading deletes scan the table' \
  FROM pg_constraint c \
  WHERE c.contype = 'f' AND NOT EXISTS (SELECT 1 FROM pg_index i WHERE i.indrelid = c.conrelid AND (i.indkey::int2[])[0:cardinality(c.conkey) - 1] @> c.conkey AND (i.indkey::int2[])[0:cardinality(c.conkey) - 1] <@ c.conkey) \
  UNION ALL \
  SELECT 'no_primary_key', t.schemaname || '.' || t.relname, 'rows can''t be told apart for updates, replication or tools' \
  FROM pg_stat_user_tables t \
  WHERE NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conrelid = t.relid AND c.contype = 'p') \
  UNION ALL \
  SELECT 'sequential_scans', t.schemaname || '.' || t.relname, t.seq_scan || ' sequential scans against ' || coalesce(t.idx_scan, 0) || ' index scans over ~' || t.n_live_tup || ' rows: a query may be missing an index' \
  FROM pg_stat_user_tables t \
  WHERE t.n_live_tup > 10000 AND t.seq_scan > 10 * coalesce(t.idx_scan, 0) \
) a ORDER BY kind, \"table\"" })
}

/// The slowest statements by mean time (`pg_stat_statements`, which every compute preloads; the
/// database needs `CREATE EXTENSION pg_stat_statements` once).
fn slow_queries() -> Value {
    json!({ "read_only": true, "sql": "SELECT left(query, 500) AS query, calls, round(mean_exec_time::numeric, 2) AS mean_ms, round(total_exec_time::numeric, 2) AS total_ms, rows, round((100.0 * shared_blks_hit / nullif(shared_blks_hit + shared_blks_read, 0))::numeric, 1) AS cache_hit_pct \
FROM pg_stat_statements WHERE dbid = (SELECT oid FROM pg_database WHERE datname = current_database()) AND query NOT LIKE '%pg_stat_statements%' \
ORDER BY mean_exec_time DESC LIMIT 20" })
}

/// TypeScript interfaces for every table, written by the query itself (Supabase's generated
/// types): one per table, enums as unions of their labels, arrays, nullable columns `| null`.
fn typescript_types() -> Value {
    json!({ "read_only": true, "sql": "SELECT coalesce(string_agg(def, E'\\n\\n' ORDER BY schema_name, table_name), '// no tables') AS typescript FROM ( \
  SELECT c.table_schema AS schema_name, c.table_name, \
    'export interface ' || CASE WHEN c.table_schema = 'public' THEN '' ELSE replace(initcap(replace(c.table_schema, '_', ' ')), ' ', '') END || replace(initcap(replace(c.table_name, '_', ' ')), ' ', '') || E' {\\n' || \
    string_agg('  ' || CASE WHEN c.column_name ~ '^[A-Za-z_][A-Za-z0-9_]*$' THEN c.column_name ELSE '\"' || replace(c.column_name, '\"', '\\\"') || '\"' END || ': ' || \
      (CASE \
        WHEN c.data_type = 'ARRAY' THEN (CASE WHEN substr(c.udt_name, 2) IN ('int2', 'int4', 'int8', 'float4', 'float8', 'numeric', 'oid') THEN 'number' WHEN substr(c.udt_name, 2) = 'bool' THEN 'boolean' WHEN substr(c.udt_name, 2) IN ('json', 'jsonb') THEN 'unknown' ELSE 'string' END) || '[]' \
        WHEN c.udt_name IN ('int2', 'int4', 'int8', 'float4', 'float8', 'numeric', 'oid', 'money') THEN 'number' \
        WHEN c.udt_name = 'bool' THEN 'boolean' \
        WHEN c.udt_name IN ('json', 'jsonb') THEN 'unknown' \
        WHEN c.data_type = 'USER-DEFINED' AND EXISTS (SELECT 1 FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.typname = c.udt_name AND n.nspname = c.udt_schema AND t.typtype = 'e') \
          THEN (SELECT string_agg(quote_literal(e.enumlabel), ' | ' ORDER BY e.enumsortorder) FROM pg_enum e JOIN pg_type t ON t.oid = e.enumtypid JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.typname = c.udt_name AND n.nspname = c.udt_schema) \
        ELSE 'string' \
      END) || CASE WHEN c.is_nullable = 'YES' THEN ' | null' ELSE '' END || ';', E'\\n' ORDER BY c.ordinal_position) || E'\\n}' AS def \
  FROM information_schema.columns c JOIN information_schema.tables t ON t.table_schema = c.table_schema AND t.table_name = c.table_name AND t.table_type = 'BASE TABLE' \
  WHERE c.table_schema NOT IN ('pg_catalog', 'information_schema') GROUP BY c.table_schema, c.table_name \
) d" })
}

fn extensions() -> Value {
    json!({ "read_only": true, "sql": "SELECT a.name, a.default_version, e.extversion AS installed_version, a.comment FROM pg_available_extensions a LEFT JOIN pg_extension e ON e.extname = a.name ORDER BY e.extname IS NULL, a.name" })
}

const PROJECT: P = req("project", "Postgres project id (prj_…)");
const ENDPOINT: P = req("endpoint", "Postgres endpoint id (ep_…); a project's primary endpoint is in postgres_get_project");
const BRANCH: P = req("branch", "Branch id (br_…) or name");
const REDIS: P = req("database", "Redis database id (rdb_…) or name");
const NOSQL: P = req("database", "NoSQL database id or name");
const BUCKET: P = req("bucket", "Bucket name");
const STREAM: P = req("stream", "Stream id (st_…) or name");
const TOPIC: P = req("topic", "Topic name");
const VHOST: P = req("vhost", "Virtual host name");
const QUEUE: P = req("queue", "Queue name");
const WS: P = req("id", "Workspace project id (prj_…)");
const DAYS: P = q("days", Ty::Int, "Days of history (default 30)");
const SECRET_PROJECT: P = req("project", "Workspace project id (prj_…)");
const ENV: P = req("env", "Environment name (development, staging, production, …)");
const SERVICE: P = req("service", "App service id");

static TOOLS: &[Tool] = &[
    // ── access ──
    t("access_whoami", "Who am I", "The connection's identity: the operator, or the token or OAuth connection and the rules it has.", "access", "GET", "/v1/access", &[]),
    t("access_wait_approval", "Wait for an approval", "Wait (up to 55 s) for a person to decide an approval this connection asked for; then call the original tool again with approval.", "access", "GET", "/v1/access/approvals/{approval_id}", &[req("approval_id", "The approval id (apr_…)"), q("wait", Ty::Int, "Seconds to wait (at most 55; default 55)")]).until(decided),
    t(
        "access_check",
        "Check before acting",
        "Whether this connection may make a request (allow, ask or deny), and as which action on what, without making it.",
        "access",
        "POST",
        "/v1/access/check",
        &[breq("method", Ty::Str, "HTTP method, e.g. DELETE"), breq("path", Ty::Str, "The API path, e.g. /v1/projects/prj_x"), b("body", Ty::Obj, "The JSON body, for SQL, Redis or CQL")],
    ),
    t(
        "access_request_access",
        "Ask for access for a while",
        "Ask a person to let this connection do one more thing for a while, saying why: one action on one product (one resource, environment or folder if named) for 15m, 1h, 8h or 1d. Answers the pending approval; wait for it with access_wait_approval, then go on.",
        "access",
        "POST",
        "/v1/access/requests",
        &[
            breq("product", Ty::Str, "The product, e.g. postgres, apps, secrets"),
            breq("action", Ty::Str, "read, write, delete, reveal, deploy or admin"),
            b("resource", Ty::Str, "One resource's id (every resource of the product when left out)"),
            b("environment", Ty::Str, "Only in this environment"),
            b("path", Ty::Str, "Only under this folder"),
            breq("for", Ty::Str, "15m, 1h, 8h or 1d"),
            breq("reason", Ty::Str, "Why: the person deciding reads it"),
        ],
    ),
    t("access_list_approvals", "List approvals", "Approvals, newest first: pending ones wait for the operator.", "access", "GET", "/v1/access/approvals", &[q("state", Ty::Str, "pending, approved, denied, used")]),
    t("access_audit_log", "Audit log", "Every request made with a token and every MCP tool call, newest first.", "access", "GET", "/v1/access/audit", &[q("grant", Ty::Str, "Only this token (tok_…)"), q("product", Ty::Str, "Only this product"), q("decision", Ty::Str, "allow, deny or ask"), q("limit", Ty::Int, "At most (default 100)")]),
    t("access_checkup", "Checkup: which tokens need attention", "The live tokens that need attention, most serious first, each with its findings and fix: everything (delete, reveal or admin on every resource without asking: tighten), never_expires (give it an expiry), unused (nothing in 30 days: revoke), narrower (its requests needed less than its rules: tighten), roams (five or more networks and no address limit). A person's checkup is of their own tokens; fixing one is theirs or the operator's, on the token's page or with roll token.", "access", "GET", "/v1/access/checkup", &[]),
    t("access_list_tokens", "List tokens", "Scoped tokens and OAuth connections you may see: name, rules, expiry, address limits, who made it and when it was last used (never a secret). A person sees their own.", "access", "GET", "/v1/access/tokens", &[q("product", Ty::Str, "Only tokens made for this product"), q("resource", Ty::Str, "Only tokens made for this resource (with product)"), q("kind", Ty::Str, "workload: CI and cluster logins (the latest 500) instead")]),
    t("access_get_token", "Get a token", "One token's rules, expiry, address limits, approvals for a while and last use (never its secret).", "access", "GET", "/v1/access/tokens/{id}", &[req("id", "Token id (tok_…)")]),
    t("access_tighten_token", "Propose narrower rules for a token", "A token's rules narrowed to what its requests used in the last days (never wider): the proposal, what it went by and the rules it's from. Proposing changes nothing; applying it is the operator's (the token's page, or roll token tighten --apply).", "access", "GET", "/v1/access/tokens/{id}/tighten", &[req("id", "Token id (tok_…)"), q("days", Ty::Int, "Days of activity to go by (1-90; default 30)")]),
    t("access_token_networks", "Where a token was used from", "The networks (an IPv4 /24 or IPv6 /48) a token was used from, with the first and last time: a leaked token shows up somewhere new.", "access", "GET", "/v1/access/tokens/{id}/networks", &[req("id", "Token id (tok_…)")]),
    // ── workspace ──
    t("workspace_list_projects", "List projects", "Workspace projects, each with the resources on its canvas (kind, ref, position).", "workspace", "GET", "/v1/workspace/projects", &[]),
    t("workspace_get_project", "Get a project", "A workspace project and its resources.", "workspace", "GET", "/v1/workspace/projects/{id}", &[WS]),
    t("workspace_create_project", "Create a project", "A new workspace project to group resources.", "workspace", "POST", "/v1/workspace/projects", &[breq("name", Ty::Str, "1-48 of a-z, 0-9 and -"), b("description", Ty::Str, "At most 280 characters")]),
    t("workspace_update_project", "Update a project", "Rename a project or change its description.", "workspace", "PATCH", "/v1/workspace/projects/{id}", &[WS, b("name", Ty::Str, "New name"), b("description", Ty::Str, "New description")]),
    t("workspace_delete_project", "Delete a project", "Delete a workspace project. Its resources stay; only the grouping goes.", "workspace", "DELETE", "/v1/workspace/projects/{id}", &[WS]),
    t(
        "workspace_place_resource",
        "Place a resource",
        "Put a resource on a project's canvas (or move it), at x, y.",
        "workspace",
        "PUT",
        "/v1/workspace/projects/{id}/resources/{kind}/{ref}",
        &[WS, req("kind", "postgres, redis, stream, bucket, nosql or repo"), req("ref", "The resource's id (owner/name for a repo)"), breq("x", Ty::Num, "Canvas x"), breq("y", Ty::Num, "Canvas y")],
    ),
    t("workspace_remove_resource", "Remove a resource from a project", "Take a resource off a project's canvas (the resource itself stays).", "workspace", "DELETE", "/v1/workspace/projects/{id}/resources/{kind}/{ref}", &[WS, req("kind", "Resource kind"), req("ref", "The resource's id")]),
    // ── Postgres ──
    t("postgres_list_projects", "List Postgres projects", "Postgres projects with their branches and endpoints.", "postgres", "GET", "/v1/projects", &[]),
    t("postgres_get_project", "Get a Postgres project", "A Postgres project with its branches and endpoints (connection strings need reveal).", "postgres", "GET", "/v1/projects/{project}", &[PROJECT]),
    t("postgres_create_project", "Create a Postgres project", "A new serverless Postgres database (a project with a main branch and endpoint).", "postgres", "POST", "/v1/projects", &[breq("name", Ty::Str, "Project name"), b("pg_version", Ty::Int, "Postgres major version"), IN_PROJECT]),
    t("postgres_delete_project", "Delete a Postgres project", "Delete a project with all its branches and data. Irreversible.", "postgres", "DELETE", "/v1/projects/{project}", &[PROJECT]),
    t(
        "postgres_create_branch",
        "Create a branch",
        "A copy-on-write branch of a project, from its parent now or at a point in time.",
        "postgres",
        "POST",
        "/v1/projects/{project}/branches",
        &[PROJECT, breq("name", Ty::Str, "Branch name"), b("parent", Ty::Str, "Parent branch (default main)"), b("at", Ty::Str, "Point in time (RFC 3339) to branch from")],
    ),
    t("postgres_delete_branch", "Delete a branch", "Delete a branch and its compute.", "postgres", "DELETE", "/v1/projects/{project}/branches/{branch}", &[PROJECT, BRANCH]),
    t("postgres_branch_history", "Branch history", "The range a branch can be restored to or branched from.", "postgres", "GET", "/v1/projects/{project}/branches/{branch}/history", &[PROJECT, BRANCH]),
    t("postgres_restore_branch", "Restore a branch", "Rewind a branch in place to a past point (the current state is kept as a branch).", "postgres", "POST", "/v1/projects/{project}/branches/{branch}/restore", &[PROJECT, BRANCH, b("at", Ty::Str, "Point in time (RFC 3339)"), b("lsn", Ty::Str, "Or an LSN")]),
    t("postgres_reset_branch", "Reset a branch", "Put a branch back to its parent's current state, in place: the same endpoint and connection string, its own changes gone (schema_only: then empty every table).", "postgres", "POST", "/v1/projects/{project}/branches/{branch}/reset", &[PROJECT, BRANCH, b("schema_only", Ty::Bool, "Keep only the structure")]),
    t("postgres_get_endpoint", "Get an endpoint", "An endpoint's compute: state (running, suspended), size limits and suspend timeout (connection strings need reveal).", "postgres", "GET", "/v1/endpoints/{endpoint}", &[ENDPOINT]),
    t("postgres_update_endpoint", "Resize an endpoint", "An endpoint's autoscaling limits and how long it idles before suspending.", "postgres", "PATCH", "/v1/endpoints/{endpoint}", &[ENDPOINT, b("min_cu", Ty::Num, "Smallest size (compute units)"), b("max_cu", Ty::Num, "Largest size"), b("suspend_timeout_s", Ty::Int, "Idle seconds before suspending")]),
    t("postgres_query", "Query Postgres (read-only)", "Run read-only SQL on an endpoint: it runs in a read-only transaction, so writes fail. Returns columns and rows.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT, breq("sql", Ty::Str, "SQL to run")]).fixed(read_only),
    t("postgres_execute", "Execute SQL", "Run SQL that may change data or schema (INSERT, UPDATE, DDL) on an endpoint.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT, breq("sql", Ty::Str, "SQL to run")]).fixed(writes).destructive(),
    t("postgres_describe_schema", "Describe the schema", "Every table's columns and types on an endpoint (read-only).", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(describe_schema),
    t("postgres_explain", "Explain a query", "A statement's plan as JSON (EXPLAIN, FORMAT JSON), without running it: which indexes it uses, its estimated cost and rows. For real timings, postgres_query with EXPLAIN ANALYZE.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT, breq("sql", Ty::Str, "The statement")]).fixed(read_only).wraps(explain),
    t("postgres_slow_queries", "Slowest queries", "The 20 slowest statements by mean time, with calls, rows and cache hit rate (pg_stat_statements; if it's missing, CREATE EXTENSION pg_stat_statements once with postgres_execute).", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(slow_queries),
    t("postgres_advisors", "Database advice", "What's worth fixing: unused indexes, foreign keys without an index, tables without a primary key, large tables read by sequential scans.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(advisors),
    t("postgres_list_extensions", "Extensions", "Extensions installed and available, with versions.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(extensions),
    t("postgres_typescript_types", "TypeScript types", "TypeScript interfaces for every table on an endpoint (enums as unions, arrays, nullable columns as | null): to write typed code against the schema.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(typescript_types),
    t("postgres_start_endpoint", "Start an endpoint", "Wake an endpoint's compute.", "postgres", "POST", "/v1/endpoints/{endpoint}/start", &[ENDPOINT]),
    t("postgres_suspend_endpoint", "Suspend an endpoint", "Suspend an endpoint's compute.", "postgres", "POST", "/v1/endpoints/{endpoint}/suspend", &[ENDPOINT]),
    t("postgres_update_project", "Change a Postgres project", "How long a project keeps history for point-in-time restores and branches.", "postgres", "PATCH", "/v1/projects/{project}", &[PROJECT, breq("history_retention_s", Ty::Int, "Seconds of history to keep")]),
    t("postgres_backup_branch", "Back up a branch", "Take a base backup of a branch now (restores and new branches start from the newest).", "postgres", "POST", "/v1/projects/{project}/branches/{branch}/backup", &[PROJECT, BRANCH]),
    t("postgres_delete_endpoint", "Delete an endpoint", "Delete an endpoint (a read replica, or a branch's compute; its data stays on the branch).", "postgres", "DELETE", "/v1/endpoints/{endpoint}", &[ENDPOINT]),
    t("postgres_usage", "Postgres usage", "Compute and storage use of a project, with cost estimates.", "postgres", "GET", "/v1/projects/{project}/usage", &[PROJECT, DAYS]),
    // ── Redis ──
    t("redis_list_databases", "List Redis databases", "Serverless Redis databases.", "redis", "GET", "/v1/redis/databases", &[]),
    t("redis_get_database", "Get a Redis database", "A Redis database's settings and connection (credentials need reveal).", "redis", "GET", "/v1/redis/databases/{database}", &[REDIS]),
    t("redis_create_database", "Create a Redis database", "A new serverless Redis database.", "redis", "POST", "/v1/redis/databases", &[breq("name", Ty::Str, "Database name"), b("max_memory_mb", Ty::Int, "Memory limit"), b("eviction_policy", Ty::Str, "Eviction policy"), IN_PROJECT]),
    t("redis_delete_database", "Delete a Redis database", "Delete a database and its data. Irreversible.", "redis", "DELETE", "/v1/redis/databases/{database}", &[REDIS]),
    t("redis_create_branch", "Branch a Redis database", "A fork of a database's data, now or at a point in time.", "redis", "POST", "/v1/redis/databases/{database}/branches", &[REDIS, breq("name", Ty::Str, "Branch name"), b("at", Ty::Int, "Unix seconds")]),
    t("redis_command", "Run a Redis command", "Run one command on a database, e.g. [\"GET\", \"key\"]. Read-only commands are reads; FLUSHDB and FLUSHALL delete.", "redis", "POST", "/v1/redis/databases/{database}/command", &[REDIS, breq("command", Ty::Arr, "The command and its arguments")]),
    t("redis_restore", "Restore a Redis database", "Rewind a database in place to a point in time (its state before is kept as an archive).", "redis", "POST", "/v1/redis/databases/{database}/restore", &[REDIS, breq("at", Ty::Int, "Unix seconds")]),
    t("redis_usage", "Redis usage", "A database's usage report.", "redis", "GET", "/v1/redis/databases/{database}/usage", &[REDIS, DAYS]),
    t(
        "redis_update_database",
        "Change a Redis database",
        "Change a database's name, memory limit, eviction policy, durability, idle timeout or history: what's left out stays.",
        "redis",
        "PATCH",
        "/v1/redis/databases/{database}",
        &[REDIS, b("name", Ty::Str, "Name"), b("max_memory_mb", Ty::Int, "Memory limit in MB"), b("eviction_policy", Ty::Str, "e.g. allkeys-lru, noeviction"), b("durability", Ty::Str, "How writes are kept"), b("idle_timeout_s", Ty::Int, "Seconds idle before it sleeps"), b("history_retention_s", Ty::Int, "Seconds of history for restores")],
    ),
    t("redis_list_replicas", "List Redis replicas", "A database's read replicas and their lag.", "redis", "GET", "/v1/redis/databases/{database}/replicas", &[REDIS]),
    // ── storage ──
    t("storage_list_buckets", "List buckets", "S3-compatible buckets.", "storage", "GET", "/v1/storage/buckets", &[]),
    t("storage_get_bucket", "Get a bucket", "A bucket's settings.", "storage", "GET", "/v1/storage/buckets/{bucket}", &[BUCKET]),
    t("storage_create_bucket", "Create a bucket", "A new bucket.", "storage", "POST", "/v1/storage/buckets", &[breq("name", Ty::Str, "Bucket name"), b("versioning", Ty::Bool, "Keep object versions"), IN_PROJECT]),
    t("storage_delete_bucket", "Delete a bucket", "Delete an empty bucket (force deletes every object too, irreversibly).", "storage", "DELETE", "/v1/storage/buckets/{bucket}", &[BUCKET, q("force", Ty::Bool, "Delete every object and the trash too")]),
    t(
        "storage_list_objects",
        "List objects",
        "Objects in a bucket, by prefix; with delimiter \"/\" it lists one level like a folder.",
        "storage",
        "GET",
        "/v1/storage/buckets/{bucket}/objects",
        &[BUCKET, q("prefix", Ty::Str, "Key prefix"), q("delimiter", Ty::Str, "e.g. /"), q("cursor", Ty::Str, "From the last page"), q("limit", Ty::Int, "At most")],
    ),
    t("storage_head_object", "Object metadata", "An object's size, type, ETag and metadata.", "storage", "GET", "/v1/storage/buckets/{bucket}/object", &[BUCKET, qreq("key", "Object key"), q("version_id", Ty::Str, "A version")]),
    t("storage_read_object", "Read an object", "An object's content: text as is, anything else as base64.", "storage", "GET", "/v1/storage/buckets/{bucket}/download", &[BUCKET, qreq("key", "Object key"), q("version_id", Ty::Str, "A version")]),
    t(
        "storage_write_object",
        "Upload an object to a bucket",
        "Upload an object (replacing the key): content as text, or content_b64.",
        "storage",
        "PUT",
        "/v1/storage/buckets/{bucket}/upload",
        &[BUCKET, qreq("key", "Object key"), P { name: "content", ty: Ty::Str, at: At::RawText, required: false, desc: "Text content" }, P { name: "content_b64", ty: Ty::Str, at: At::RawB64, required: false, desc: "Binary content, base64" }],
    ),
    t("storage_delete_object", "Delete an object", "Delete an object (to the trash; with version_id, that version for good).", "storage", "DELETE", "/v1/storage/buckets/{bucket}/object", &[BUCKET, qreq("key", "Object key"), q("version_id", Ty::Str, "A version")]),
    t("storage_presign", "Presign a URL", "A URL anyone can use to get (or put) an object until it expires.", "storage", "POST", "/v1/storage/buckets/{bucket}/presign", &[BUCKET, breq("key", Ty::Str, "Object key"), b("method", Ty::Str, "GET or PUT"), b("expires_s", Ty::Int, "Seconds valid")]),
    t("storage_branch_bucket", "Branch a bucket", "A copy-on-write branch of a bucket (objects shared, not copied).", "storage", "POST", "/v1/storage/buckets/{bucket}/branches", &[BUCKET, breq("name", Ty::Str, "Branch bucket name")]),
    t("storage_list_trash", "Deleted objects", "Objects in a bucket's trash, with when each was deleted (they can be restored until it's emptied).", "storage", "GET", "/v1/storage/buckets/{bucket}/trash", &[BUCKET, q("prefix", Ty::Str, "Key prefix"), q("limit", Ty::Int, "At most (default 200)"), q("cursor", Ty::Str, "From a previous page")]),
    t("storage_restore_object", "Restore a deleted object", "Bring an object back from the trash (key and deleted_at from storage_list_trash).", "storage", "POST", "/v1/storage/buckets/{bucket}/trash/restore", &[BUCKET, breq("key", Ty::Str, "Object key"), breq("deleted_at", Ty::Int, "When it was deleted, as listed")]),
    t("storage_usage", "Storage usage", "A bucket's usage.", "storage", "GET", "/v1/storage/buckets/{bucket}/usage", &[BUCKET, DAYS]),
    t("storage_usage_all", "Storage usage, every bucket", "Storage and requests across every bucket.", "storage", "GET", "/v1/storage/usage", &[DAYS]),
    t("storage_list_accounts", "List storage accounts", "Storage's accounts (AWS-style, 12-digit ids): name, alias, and which is the default (creating one is the operator's).", "storage", "GET", "/v1/storage/accounts", &[]),
    t("storage_list_keys", "List S3 access keys", "An account's access keys: id, name, permission, the buckets it's limited to, the IAM user it belongs to and when it was last used (never a secret).", "storage", "GET", "/v1/storage/keys", &[q("account", Ty::Str, "Account id (12 digits; the default account when left out)")]),
    t("storage_list_roles", "List IAM roles", "An account's IAM roles: ARN, trust, permissions policy and session length. Use it when an assumed role is refused.", "storage", "GET", "/v1/storage/roles", &[q("account", Ty::Str, "Account id (12 digits; the default account when left out)")]),
    t("storage_get_role", "Get an IAM role", "One IAM role: its ARN, trust, permissions policy and session length.", "storage", "GET", "/v1/storage/roles/{name}", &[req("name", "Role name"), q("account", Ty::Str, "Account id (12 digits; the default account when left out)")]),
    t("storage_list_replication_targets", "List replication targets", "Buckets outside roll that replication copies to: endpoint, region, access key id, buckets (never the secret; adding one is admin).", "storage", "GET", "/v1/storage/replication-targets", &[]),
    t(
        "storage_update_bucket",
        "Change a bucket",
        "Change a bucket's versioning, lifecycle rules, CORS, trash days, tags or whether it's public: what's left out stays.",
        "storage",
        "PATCH",
        "/v1/storage/buckets/{bucket}",
        &[
            BUCKET,
            b("versioning", Ty::Str, "Enabled or Suspended"),
            b("public", Ty::Bool, "Anyone may read its objects"),
            b("recover_days", Ty::Int, "Days deleted objects stay in the trash"),
            b("lifecycle", Ty::Arr, "Lifecycle rules (replace the bucket's)"),
            b("cors", Ty::Arr, "CORS rules (replace the bucket's)"),
            b("tags", Ty::Obj, "Tags (replace the bucket's)"),
        ],
    )
    .destructive(),
    t("storage_delete_objects", "Delete objects", "Delete these keys, or every object under a prefix (to the trash).", "storage", "POST", "/v1/storage/buckets/{bucket}/delete", &[BUCKET, b("keys", Ty::Arr, "Keys"), b("prefix", Ty::Str, "Or everything under this prefix")]).destructive(),
    // ── streams ──
    t("streams_list", "List streams", "Kafka-compatible streams.", "streams", "GET", "/v1/streams", &[]),
    t("streams_get", "Get a stream", "A stream's bootstrap address, topics and throughput.", "streams", "GET", "/v1/streams/{stream}", &[STREAM]),
    t("streams_create", "Create a stream", "A new stream (its first API key needs reveal to see).", "streams", "POST", "/v1/streams", &[breq("name", Ty::Str, "Stream name"), b("settings", Ty::Obj, "Settings"), IN_PROJECT]),
    t("streams_delete", "Delete a stream", "Delete a stream with its topics and data. Irreversible.", "streams", "DELETE", "/v1/streams/{stream}", &[STREAM]),
    t("streams_list_topics", "List topics", "A stream's topics.", "streams", "GET", "/v1/streams/{stream}/topics", &[STREAM]),
    t("streams_create_topic", "Create a topic", "A new topic.", "streams", "POST", "/v1/streams/{stream}/topics", &[STREAM, breq("name", Ty::Str, "Topic name"), b("partitions", Ty::Int, "Partitions"), b("configs", Ty::Obj, "Topic configs")]),
    t("streams_delete_topic", "Delete a topic", "Delete a topic and its records.", "streams", "DELETE", "/v1/streams/{stream}/topics/{topic}", &[STREAM, TOPIC]),
    t("streams_produce", "Produce records", "Append records to a topic: [{key?, value, headers?, partition?}].", "streams", "POST", "/v1/streams/{stream}/topics/{topic}/records", &[STREAM, TOPIC, breq("records", Ty::Arr, "Records")]),
    t(
        "streams_read",
        "Read records",
        "Records from a topic, from an offset or a time.",
        "streams",
        "GET",
        "/v1/streams/{stream}/topics/{topic}/records",
        &[STREAM, TOPIC, q("offset", Ty::Str, "earliest, latest or a number"), q("timestamp", Ty::Int, "Unix ms"), q("limit", Ty::Int, "At most"), q("partitions", Ty::Str, "Comma-separated partitions")],
    ),
    t("streams_list_groups", "List consumer groups", "Consumer groups with their lag.", "streams", "GET", "/v1/streams/{stream}/groups", &[STREAM]),
    t("streams_reset_group", "Reset a consumer group", "Move a group's offsets (earliest, latest, an offset or timestamp:<ms>).", "streams", "POST", "/v1/streams/{stream}/groups/{group}/reset", &[STREAM, req("group", "Group id"), breq("topic", Ty::Str, "Topic"), breq("to", Ty::Str, "Where to")]),
    t("streams_get_topic", "Get a topic", "A topic's partitions, their offsets and its settings (retention, compaction).", "streams", "GET", "/v1/streams/{stream}/topics/{topic}", &[STREAM, TOPIC]),
    t("streams_get_group", "Get a consumer group", "A group's members and its committed offset and lag on each partition.", "streams", "GET", "/v1/streams/{stream}/groups/{group}", &[STREAM, req("group", "Group id")]),
    t("streams_branch", "Branch a stream", "A new stream with this one's topics, records (shared, not copied) and committed offsets; its first key's secret comes back once (reveal).", "streams", "POST", "/v1/streams/{stream}/branches", &[STREAM, breq("name", Ty::Str, "Branch name")]),
    t("streams_usage", "Stream usage", "A stream's usage and throughput.", "streams", "GET", "/v1/streams/{stream}/usage", &[STREAM, q("hours", Ty::Int, "Hours of history")]),
    t("streams_list_keys", "List a stream's keys", "The SASL users a stream's Kafka clients sign in as: username, label and when made (never a password; a new key is a reveal, in the dashboard or roll stream).", "streams", "GET", "/v1/streams/{stream}/keys", &[STREAM]),
    t("streams_update", "Change a stream's settings", "Change some of a stream's settings (as streams_get shows them: retention, throughput limits…); the rest stay.", "streams", "PATCH", "/v1/streams/{stream}", &[STREAM, breq("settings", Ty::Obj, "The settings to change")]),
    t(
        "streams_update_topic",
        "Change a topic",
        "Grow a topic's partitions or change its configs, online.",
        "streams",
        "PATCH",
        "/v1/streams/{stream}/topics/{topic}",
        &[STREAM, TOPIC, b("partitions", Ty::Int, "Grow to this many (partitions never shrink)"), b("configs", Ty::Obj, "Kafka configs, e.g. {\"retention.ms\": \"86400000\"}; null removes an override")],
    ),
    t("streams_delete_group", "Delete a consumer group", "Delete a group and its committed offsets (refused while it has members).", "streams", "DELETE", "/v1/streams/{stream}/groups/{group}", &[STREAM, req("group", "Consumer group")]).destructive(),
    // ── NoSQL ──
    t("nosql_list_databases", "List NoSQL databases", "ScyllaDB/Cassandra-compatible databases.", "nosql", "GET", "/v1/nosql/databases", &[]),
    t("nosql_get_database", "Get a NoSQL database", "A database's settings and endpoints (credentials need reveal).", "nosql", "GET", "/v1/nosql/databases/{database}", &[NOSQL]),
    t("nosql_create_database", "Create a NoSQL database", "A new database.", "nosql", "POST", "/v1/nosql/databases", &[breq("name", Ty::Str, "Database name"), b("dialect", Ty::Str, "scylla or cassandra"), IN_PROJECT]),
    t("nosql_delete_database", "Delete a NoSQL database", "Delete a database and its data. Irreversible.", "nosql", "DELETE", "/v1/nosql/databases/{database}", &[NOSQL]),
    t("nosql_create_branch", "Branch a NoSQL database", "A branch database, now or at a point in time.", "nosql", "POST", "/v1/nosql/databases/{database}/branches", &[NOSQL, breq("name", Ty::Str, "Branch name"), b("at", Ty::Str, "Point in time"), b("schema_only", Ty::Bool, "Schema without data")]),
    t("nosql_cql", "Run CQL", "Run a CQL statement. SELECT reads; DROP and TRUNCATE delete; the rest write.", "nosql", "POST", "/v1/nosql/databases/{database}/cql", &[NOSQL, breq("query", Ty::Str, "CQL"), b("keyspace", Ty::Str, "Keyspace"), b("page_state", Ty::Str, "From the last page")]),
    t("nosql_schema", "NoSQL schema", "Keyspaces, tables and their columns.", "nosql", "GET", "/v1/nosql/databases/{database}/schema", &[NOSQL]),
    t("nosql_usage", "NoSQL usage", "A database's usage report.", "nosql", "GET", "/v1/nosql/databases/{database}/usage", &[NOSQL, DAYS]),
    t("nosql_reset", "Reset a NoSQL branch", "Put a branch back to its parent's state (now, or at a time), in place: the same endpoint and credentials.", "nosql", "POST", "/v1/nosql/databases/{database}/reset", &[NOSQL, b("at", Ty::Int, "Unix seconds")]),
    t("nosql_restore", "Restore a NoSQL database", "Rewind a database in place to a point in time (its state before is archived).", "nosql", "POST", "/v1/nosql/databases/{database}/restore", &[NOSQL, breq("at", Ty::Int, "Unix seconds")]),
    t("nosql_diff", "Diff a NoSQL branch", "What a branch changed against its parent: tables and rows added, changed and removed.", "nosql", "GET", "/v1/nosql/databases/{database}/diff", &[NOSQL]),
    t(
        "nosql_update_database",
        "Change a NoSQL database",
        "Change a database's name, idle timeout, durability, history, expiry, read-only flag or masking policy: what's left out stays. Loosening masking or read-only needs admin.",
        "nosql",
        "PATCH",
        "/v1/nosql/databases/{database}",
        &[
            NOSQL,
            b("name", Ty::Str, "Name"),
            b("idle_timeout", Ty::Int, "Seconds idle before it sleeps"),
            b("durability", Ty::Str, "How writes are kept"),
            b("history_retention", Ty::Int, "Seconds of history for restores"),
            b("read_only", Ty::Bool, "Refuse writes (a safe branch to debug on)"),
            b("masking", Ty::Obj, "Masking policy, replaced whole: {\"ks.table.column\": \"mask_inner(1, 1)\"}"),
            b("expires_at", Ty::Int, "Unix seconds it's deleted at (null: never)"),
        ],
    ),
    t("nosql_activity", "NoSQL activity", "What a database is doing now: connected clients, slow queries, warnings and metrics.", "nosql", "GET", "/v1/nosql/databases/{database}/activity", &[NOSQL]),
    t("nosql_list_imports", "List imports into a database", "A database's imports from another cluster (cql://) or a backup (s3://, file://): state, keyspaces, rows, tables done, errors, the schedule and the next run (the source's password hidden).", "nosql", "GET", "/v1/nosql/databases/{database}/imports", &[NOSQL]),
    // ── queues ──
    t("queues_overview", "Queues overview", "The broker's totals and listeners.", "queues", "GET", "/v1/queues/overview", &[]),
    t("queues_list_vhosts", "List virtual hosts", "RabbitMQ-compatible virtual hosts.", "queues", "GET", "/v1/queues/vhosts", &[]),
    t("queues_create_vhost", "Create a virtual host", "A new virtual host.", "queues", "POST", "/v1/queues/vhosts", &[breq("name", Ty::Str, "Name"), b("description", Ty::Str, "Description"), IN_PROJECT]),
    t("queues_delete_vhost", "Delete a virtual host", "Delete a virtual host and everything in it.", "queues", "DELETE", "/v1/queues/vhosts/{vhost}", &[VHOST]),
    t("queues_list_queues", "List queues", "A virtual host's queues with message counts.", "queues", "GET", "/v1/queues/vhosts/{vhost}/queues", &[VHOST]),
    t("queues_declare_queue", "Declare a queue", "Create a queue.", "queues", "POST", "/v1/queues/vhosts/{vhost}/queues", &[VHOST, breq("name", Ty::Str, "Queue name"), b("type", Ty::Str, "classic, quorum or stream"), b("durable", Ty::Bool, "Durable")]),
    t("queues_delete_queue", "Delete a queue", "Delete a queue and its messages.", "queues", "DELETE", "/v1/queues/vhosts/{vhost}/queues/{queue}", &[VHOST, QUEUE]),
    t("queues_purge_queue", "Purge a queue", "Drop every message in a queue.", "queues", "POST", "/v1/queues/vhosts/{vhost}/queues/{queue}/purge", &[VHOST, QUEUE]),
    t("queues_get_queue", "Get a queue", "One queue: its messages (ready and unacknowledged), consumers and their prefetch, arguments, and the dead-letter queues it feeds. Start here when messages pile up.", "queues", "GET", "/v1/queues/vhosts/{vhost}/queues/{queue}", &[VHOST, QUEUE]),
    t("queues_queue_rates", "One queue's rates", "A queue's publish, deliver and acknowledge rates and depth over the last seconds (5-300, default 300), as points.", "queues", "GET", "/v1/queues/vhosts/{vhost}/queues/{queue}/rates", &[VHOST, QUEUE, q("seconds", Ty::Int, "How far back, 5-300 (default 300)")]),
    t("queues_list_exchanges", "List exchanges", "A virtual host's exchanges, their types and arguments.", "queues", "GET", "/v1/queues/vhosts/{vhost}/exchanges", &[VHOST]),
    t("queues_list_bindings", "List bindings", "A virtual host's bindings: which exchange routes to which queue by which key. Why a message went nowhere.", "queues", "GET", "/v1/queues/vhosts/{vhost}/bindings", &[VHOST]),
    t("queues_peek", "Peek messages", "Messages in a queue, without consuming them.", "queues", "GET", "/v1/queues/vhosts/{vhost}/queues/{queue}/messages", &[VHOST, QUEUE, q("count", Ty::Int, "At most 500")]),
    t("queues_publish", "Publish a message", "Publish to an exchange with a routing key.", "queues", "POST", "/v1/queues/vhosts/{vhost}/publish", &[VHOST, breq("exchange", Ty::Str, "Exchange (\"\" for the default)"), breq("routing_key", Ty::Str, "Routing key"), breq("body", Ty::Str, "Message body")]),
    t("queues_list_connections", "List connections", "Open client connections: who, from where, and their channels.", "queues", "GET", "/v1/queues/connections", &[]),
    t("queues_close_connection", "Close a connection", "Close a client's connection (it may reconnect).", "queues", "DELETE", "/v1/queues/connections/{id}", &[req("id", "Connection id or name")]).destructive(),
    t("queues_list_channels", "List channels", "Open channels with their prefetch and unacknowledged messages.", "queues", "GET", "/v1/queues/channels", &[]),
    t("queues_list_policies", "List policies", "Policies (message TTL, dead-lettering, length limits) and the queues they match.", "queues", "GET", "/v1/queues/policies", &[]),
    t("queues_list_users", "List queue users", "The broker's users: tags, each vhost's configure, write and read permissions, and limits (never a password). Use it when a client is refused (ACCESS_REFUSED).", "queues", "GET", "/v1/queues/users", &[]),
    t(
        "queues_set_policy",
        "Set a policy",
        "Create or replace a policy: a pattern of queue or exchange names and what applies to them.",
        "queues",
        "PUT",
        "/v1/queues/policies/{vhost}/{name}",
        &[VHOST, req("name", "Policy name"), breq("pattern", Ty::Str, "Regex of names it applies to"), breq("definition", Ty::Obj, "e.g. {\"message-ttl\": 60000, \"dead-letter-exchange\": \"dlx\"}"), b("apply_to", Ty::Str, "queues, exchanges or all"), b("priority", Ty::Int, "Higher wins")],
    ),
    t("queues_delete_policy", "Delete a policy", "Delete a policy.", "queues", "DELETE", "/v1/queues/policies/{vhost}/{name}", &[VHOST, req("name", "Policy name")]),
    t("queues_rates", "Message rates", "Publish, deliver and ack rates over the last minutes.", "queues", "GET", "/v1/queues/rates", &[q("seconds", Ty::Int, "How far back, 5–3600 (300)")]),
    t("queues_usage", "Queues usage", "Messages and storage by day.", "queues", "GET", "/v1/queues/usage", &[q("days", Ty::Int, "Days back (30)"), q("vhost", Ty::Str, "Only this vhost")]),
    // ── tasks ──
    t(
        "tasks_publish",
        "Publish a task",
        "Deliver an HTTP request to a URL, topic or queue, now or later, with retries.",
        "tasks",
        "POST",
        "/v1/tasks/publish",
        &[b("url", Ty::Str, "Destination URL"), b("topic", Ty::Str, "Or a topic"), b("queue", Ty::Str, "Or a queue"), b("body", Ty::Str, "Body"), b("headers", Ty::Obj, "Headers"), b("delay", Ty::Str, "e.g. 10s, 5m"), b("retries", Ty::Int, "Retries")],
    ),
    t("tasks_list_messages", "List task messages", "Messages by state, queue or topic.", "tasks", "GET", "/v1/tasks/messages", &[q("state", Ty::Str, "pending, delivered, failed…"), q("queue", Ty::Str, "Queue"), q("limit", Ty::Int, "At most")]),
    t("tasks_get_message", "Get a task message", "A message and its delivery attempts.", "tasks", "GET", "/v1/tasks/messages/{id}", &[req("id", "Message id (msg_…)")]),
    t("tasks_cancel_message", "Cancel a task message", "Cancel a message (purge=true deletes it).", "tasks", "DELETE", "/v1/tasks/messages/{id}", &[req("id", "Message id"), q("purge", Ty::Bool, "Delete it for good")]),
    t("tasks_list_schedules", "List schedules", "Cron schedules.", "tasks", "GET", "/v1/tasks/schedules", &[]),
    t("tasks_create_schedule", "Create a schedule", "A cron schedule that publishes a task.", "tasks", "POST", "/v1/tasks/schedules", &[breq("cron", Ty::Str, "Cron expression"), b("tz", Ty::Str, "Time zone"), breq("publish", Ty::Obj, "What to publish (as tasks_publish)")]),
    t("tasks_delete_schedule", "Delete a schedule", "Delete a schedule.", "tasks", "DELETE", "/v1/tasks/schedules/{id}", &[req("id", "Schedule id")]),
    t("tasks_list_runs", "List function runs", "Durable function runs.", "tasks", "GET", "/v1/tasks/runs", &[q("function", Ty::Str, "Function"), q("status", Ty::Str, "Status"), q("limit", Ty::Int, "At most")]),
    t("tasks_get_run", "Get a function run", "A run with its steps.", "tasks", "GET", "/v1/tasks/runs/{id}", &[req("id", "Run id (run_…)")]),
    t("tasks_invoke", "Invoke a function", "Start a durable function run.", "tasks", "POST", "/v1/tasks/functions/{id}/invoke", &[req("id", "Function id"), b("data", Ty::Obj, "Input"), b("wait", Ty::Bool, "Wait for the result")]),
    t("tasks_retry_message", "Retry a message", "Deliver a failed or dead-lettered message again now.", "tasks", "POST", "/v1/tasks/messages/{id}/retry", &[req("id", "Message id")]),
    t("tasks_retry_dead_letters", "Retry dead letters", "Deliver dead-lettered messages again: these ids, a queue's, or all.", "tasks", "POST", "/v1/tasks/dlq/retry", &[b("ids", Ty::Arr, "Message ids"), b("queue", Ty::Str, "Only this queue's"), b("all", Ty::Bool, "Every dead letter")]),
    t("tasks_cancel_run", "Cancel a run", "Stop a durable function run (its finished steps stay done).", "tasks", "POST", "/v1/tasks/runs/{id}/cancel", &[req("id", "Run id")]),
    t("tasks_rerun", "Rerun a run", "Run a function again with the same input as a past run.", "tasks", "POST", "/v1/tasks/runs/{id}/rerun", &[req("id", "Run id")]),
    t("tasks_list_functions", "List functions", "Durable functions: their triggers and URLs.", "tasks", "GET", "/v1/tasks/functions", &[]),
    t("tasks_list_apps", "List synced apps", "The apps whose functions tasks runs: each one's URL, functions, SDK and when it last synced. Use it when a function is missing or stale.", "tasks", "GET", "/v1/tasks/apps", &[]),
    t("tasks_list_api_keys", "List tasks API keys", "Tasks' API keys: name, prefix, when made and last used (never the key itself).", "tasks", "GET", "/v1/tasks/api-keys", &[]),
    t("tasks_list_queues", "List task queues", "Queues with their parallelism, rate and backlog.", "tasks", "GET", "/v1/tasks/queues", &[]),
    t("tasks_stats", "Tasks stats", "Nodes, queue stats, dead letters, schedules and functions.", "tasks", "GET", "/v1/tasks/stats", &[]),
    t("tasks_list_dead_letters", "List dead letters", "Messages that ran out of retries, newest first.", "tasks", "GET", "/v1/tasks/dlq", &[q("queue", Ty::Str, "Only this queue's"), q("topic", Ty::Str, "Only this topic's"), q("cursor", Ty::Str, "From a page's cursor"), q("limit", Ty::Int, "At most")]),
    t("tasks_delete_dead_letter", "Delete a dead letter", "Drop a dead-lettered message for good.", "tasks", "DELETE", "/v1/tasks/dlq/{id}", &[req("id", "Message id (msg_…)")]),
    t("tasks_get_queue", "Get a task queue", "A queue's parallelism, rate, retries, backlog and whether it's paused.", "tasks", "GET", "/v1/tasks/queues/{name}", &[req("name", "Queue")]),
    t(
        "tasks_configure_queue",
        "Create or configure a task queue",
        "Make a queue, or change its limits: what's left out stays as it is.",
        "tasks",
        "PUT",
        "/v1/tasks/queues/{name}",
        &[req("name", "Queue"), b("parallelism", Ty::Int, "Deliveries at once (0: no limit)"), b("rate", Ty::Any, "e.g. \"10/s\" or {\"limit\": 10, \"period\": \"1m\"}; null for no limit"), b("retries", Ty::Int, "Retries per message"), b("timeout", Ty::Str, "Per delivery, e.g. 30s"), b("partitions", Ty::Int, "Ordered partitions")],
    ),
    t("tasks_pause_queue", "Pause a task queue", "Hold a queue's deliveries; messages keep arriving.", "tasks", "POST", "/v1/tasks/queues/{name}/pause", &[req("name", "Queue")]),
    t("tasks_resume_queue", "Resume a task queue", "Deliver a paused queue's messages again.", "tasks", "POST", "/v1/tasks/queues/{name}/resume", &[req("name", "Queue")]),
    t("tasks_list_topics", "List topics", "Topics: a message published to one goes to each of its endpoints.", "tasks", "GET", "/v1/tasks/topics", &[]),
    t("tasks_get_topic", "Get a topic", "A topic and its endpoints.", "tasks", "GET", "/v1/tasks/topics/{name}", &[req("name", "Topic")]),
    t("tasks_set_topic", "Create or replace a topic", "A topic with exactly these endpoints.", "tasks", "PUT", "/v1/tasks/topics/{name}", &[req("name", "Topic"), breq("endpoints", Ty::Arr, "URLs, or {\"url\", \"name\"} objects")]),
    t("tasks_delete_topic", "Delete a topic", "Delete a topic; its pending messages stay.", "tasks", "DELETE", "/v1/tasks/topics/{name}", &[req("name", "Topic")]),
    t("tasks_delete_queue", "Delete a task queue", "Delete a queue. One with unfinished messages is refused unless force=true, which cancels them.", "tasks", "DELETE", "/v1/tasks/queues/{name}", &[req("name", "Queue"), q("force", Ty::Bool, "Cancel its unfinished messages too")]),
    t(
        "tasks_cancel_messages",
        "Cancel task messages",
        "Cancel many messages at once: these ids, or every one a queue, URL, topic, label or schedule names (all=true for every message). Answers how many were canceled.",
        "tasks",
        "POST",
        "/v1/tasks/messages/cancel",
        &[b("ids", Ty::Arr, "Message ids"), b("queue", Ty::Str, "A queue's"), b("url", Ty::Str, "Those to this URL"), b("topic", Ty::Str, "A topic's"), b("label", Ty::Str, "Those with this label"), b("schedule_id", Ty::Str, "A schedule's"), b("all", Ty::Bool, "Every message")],
    ),
    t("tasks_send_event", "Send an event", "An event that starts the functions it triggers (an id makes it idempotent).", "tasks", "POST", "/v1/tasks/events", &[breq("name", Ty::Str, "Event name, e.g. user.signup"), b("data", Ty::Any, "Its data"), b("id", Ty::Str, "Idempotency id")]),
    t("tasks_list_events", "List events", "Events received, newest first.", "tasks", "GET", "/v1/tasks/events", &[q("name", Ty::Str, "Only this event"), q("cursor", Ty::Str, "From a page's cursor"), q("limit", Ty::Int, "At most (50)")]),
    t("tasks_get_event", "Get an event", "An event and the runs it started.", "tasks", "GET", "/v1/tasks/events/{id}", &[req("id", "Event id")]),
    t("tasks_get_function", "Get a function", "A durable function: its triggers, concurrency, retries and whether it's paused.", "tasks", "GET", "/v1/tasks/functions/{id}", &[req("id", "Function id")]),
    t("tasks_pause_function", "Pause a function", "Start no new runs of a function (running ones carry on).", "tasks", "POST", "/v1/tasks/functions/{id}/pause", &[req("id", "Function id")]),
    t("tasks_resume_function", "Resume a function", "Start runs of a paused function again.", "tasks", "POST", "/v1/tasks/functions/{id}/resume", &[req("id", "Function id")]),
    t("tasks_cancel_function_runs", "Cancel a function's runs", "Stop every running run of a function, or those an expression matches.", "tasks", "POST", "/v1/tasks/functions/{id}/cancel", &[req("id", "Function id"), b("if", Ty::Str, "Only runs this matches, e.g. event.data.user == \"u1\"")]),
    t("tasks_wait_run", "Wait for a run", "A run once it finishes, or as it is when the timeout passes.", "tasks", "GET", "/v1/tasks/runs/{id}/wait", &[req("id", "Run id (run_…)"), q("timeout", Ty::Str, "e.g. 30s (the default), at most a few minutes")]).until(run_over),
    t("tasks_get_schedule", "Get a schedule", "A schedule, its next run and what it publishes.", "tasks", "GET", "/v1/tasks/schedules/{id}", &[req("id", "Schedule id")]),
    t(
        "tasks_update_schedule",
        "Change a schedule",
        "Change a schedule's cron, time zone, what it publishes or its label: what's left out stays.",
        "tasks",
        "PATCH",
        "/v1/tasks/schedules/{id}",
        &[req("id", "Schedule id"), b("cron", Ty::Str, "Cron expression"), b("tz", Ty::Str, "Time zone"), b("publish", Ty::Obj, "What to publish (as tasks_publish)"), b("label", Ty::Str, "Label")],
    ),
    t("tasks_pause_schedule", "Pause a schedule", "Stop a schedule firing until it's resumed.", "tasks", "POST", "/v1/tasks/schedules/{id}/pause", &[req("id", "Schedule id")]),
    t("tasks_resume_schedule", "Resume a schedule", "Fire a paused schedule again.", "tasks", "POST", "/v1/tasks/schedules/{id}/resume", &[req("id", "Schedule id")]),
    t("tasks_trigger_schedule", "Fire a schedule now", "Publish what a schedule publishes once, now.", "tasks", "POST", "/v1/tasks/schedules/{id}/trigger", &[req("id", "Schedule id")]),
    t("tasks_usage", "Tasks usage", "Messages, deliveries and runs by day, and what they cost.", "tasks", "GET", "/v1/tasks/usage", &[q("days", Ty::Int, "Days back (30)")]),
    // ── search (roll search) ──
    t("search_list_clusters", "List search clusters", "Elasticsearch-compatible search clusters; deleted=true lists those deleted recently instead (each with deleted_at, purge_at and its name), which search_restore_cluster brings back.", "search", "GET", "/v1/search/clusters", &[q("deleted", Ty::Bool, "Recently deleted clusters instead")]),
    t("search_get_cluster", "Get a search cluster", "A cluster's settings, state (loaded or asleep), indices, documents, size, URL and branches.", "search", "GET", "/v1/search/clusters/{cluster}", &[req("cluster", "Cluster id or name")]),
    t(
        "search_create_cluster",
        "Create a search cluster",
        "A new Elasticsearch-compatible cluster (or a branch of one with parent, at a time with at). Its password comes back once, to tokens that may reveal.",
        "search",
        "POST",
        "/v1/search/clusters",
        &[breq("name", Ty::Str, "Name"), b("flavor", Ty::Str, "elasticsearch or opensearch"), b("parent", Ty::Str, "Branch this cluster"), b("at", Ty::Int, "Unix seconds to branch at"), IN_PROJECT],
    ),
    t("search_list_branches", "List a cluster's branches", "A search cluster's copy-on-write branches: names, when each was taken, and their state.", "search", "GET", "/v1/search/clusters/{cluster}/branches", &[req("cluster", "Cluster id or name")]),
    t("search_list_remotes", "List a cluster's remotes", "The clusters this one searches across (cross-cluster search, alias:index): each remote's alias, cluster, the cross-cluster key's id and whether it still connects (never the key). Linking one is the cluster admin's.", "search", "GET", "/v1/search/clusters/{cluster}/remotes", &[req("cluster", "Cluster id or name")]),
    t("search_list_inference_secrets", "Inference endpoints keyed by a roll secret", "A cluster's inference endpoints whose API key comes from a roll secret, with the project and key each follows (a rotated value reaches the endpoint within seconds; never the value), and the projects whose secrets it could use.", "search", "GET", "/v1/search/clusters/{cluster}/inference/secrets", &[req("cluster", "Cluster id or name")]),
    t("search_branch_cluster", "Branch a search cluster", "A copy-on-write branch of a cluster, now or at a time within its history.", "search", "POST", "/v1/search/clusters/{cluster}/branches", &[req("cluster", "Cluster id or name"), breq("name", Ty::Str, "Branch name"), b("at", Ty::Int, "Unix seconds")]),
    t("search_update_cluster", "Update a search cluster", "Change a cluster's settings: idle_timeout_s (0: always loaded), max_in_flight, max_body_mb at once; flavor or destructive_requires_name put it to sleep to reload once no request runs; ip_allowlist replaces the networks its endpoint answers (up to 64 CIDRs, each a string or {cidr, description}; [] answers everyone; this API isn't filtered by it).", "search", "PATCH", "/v1/search/clusters/{cluster}", &[req("cluster", "Cluster id or name"), b("idle_timeout_s", Ty::Int, "Seconds idle before it sleeps (0: never)"), b("max_in_flight", Ty::Int, "Requests at once"), b("max_body_mb", Ty::Int, "Largest request body"), b("destructive_requires_name", Ty::Bool, "Refuse wildcard deletes"), b("flavor", Ty::Str, "elasticsearch or opensearch"), b("ip_allowlist", Ty::Arr, "CIDRs its endpoint answers (strings or {cidr, description})")]),
    t("search_delete_cluster", "Delete a search cluster", "Delete a cluster and its indices (its branches keep working). It's kept for the deployment's restore window (7 days by default) and search_restore_cluster brings it back until then.", "search", "DELETE", "/v1/search/clusters/{cluster}", &[req("cluster", "Cluster id or name")]),
    t("search_restore_cluster", "Restore a deleted search cluster", "Bring a deleted cluster back as it was (data, snapshots, settings, password, API keys) within its restore window, by its id (search_list_clusters with deleted=true). Its name, or name if that's taken.", "search", "POST", "/v1/search/clusters/{cluster}/restore", &[req("cluster", "The deleted cluster's id"), b("name", Ty::Str, "Another name, if its own is taken")]),
    t("search_cluster_metrics", "Search cluster metrics", "A cluster's request rate, latency (p50, p95, p99), and 2xx/4xx/5xx/429 answers by kind of request, over range 1h (default), 24h or 14d.", "search", "GET", "/v1/search/clusters/{cluster}/metrics", &[req("cluster", "Cluster id or name"), q("range", Ty::Str, "1h, 24h or 14d")]),
    t("search_slowlog", "Search slow log", "A cluster's slowest recent requests: what ran, on which index, and how long it took.", "search", "GET", "/v1/search/clusters/{cluster}/slowlog", &[req("cluster", "Cluster id or name")]),
    t("search_audit", "Search audit log", "A cluster's security audit log, newest first: failed logins, access denials and changes to users, roles, role mappings and API keys (granted access and successful logins too when the cluster records them), in Elasticsearch's or OpenSearch's event names (authentication_failed, access_denied, security_config_change, …). Never a secret or a request body.", "search", "GET", "/v1/search/clusters/{cluster}/audit", &[req("cluster", "Cluster id or name"), q("from", Ty::Int, "From (Unix ms, inclusive)"), q("to", Ty::Int, "To (Unix ms, exclusive)"), q("event", Ty::Str, "Event names or categories, comma-separated"), q("size", Ty::Int, "Entries (default 100)")]),
    t("search_analytics", "Search analytics", "What a cluster's users search for, over range 24h (default), 7d or 30d: top queries, queries that found nothing, and click-through, for one index or all. Only indices with index.roll.analytics.enabled record it.", "search", "GET", "/v1/search/clusters/{cluster}/analytics", &[req("cluster", "Cluster id or name"), q("index", Ty::Str, "One index (default: all)"), q("range", Ty::Str, "24h, 7d or 30d")]),
    t("search_usage", "Search usage", "A cluster's metering and cost, against the same workload on Elastic Cloud and Amazon OpenSearch.", "search", "GET", "/v1/search/clusters/{cluster}/usage", &[req("cluster", "Cluster id or name"), q("days", Ty::Int, "Days back (default 30)")]),
    t("search_list_indices", "List indices", "A cluster's indices.", "search", "GET", "/v1/search/clusters/{cluster}/indices", &[req("cluster", "Cluster id or name")]),
    t("search_get_index", "Get an index", "An index's mappings, settings and aliases.", "search", "GET", "/v1/search/clusters/{cluster}/indices/{index}", &[req("cluster", "Cluster"), req("index", "Index")]),
    t(
        "search_create_index",
        "Create an index",
        "Create an index with its mappings and settings (Elasticsearch's PUT /<index>).",
        "search",
        "PUT",
        "/v1/search/clusters/{cluster}/indices/{index}",
        &[req("cluster", "Cluster"), req("index", "Index"), b("mappings", Ty::Obj, "Mappings"), b("settings", Ty::Obj, "Settings"), b("aliases", Ty::Obj, "Aliases")],
    ),
    t("search_delete_index", "Delete an index", "Delete an index and its documents.", "search", "DELETE", "/v1/search/clusters/{cluster}/indices/{index}", &[req("cluster", "Cluster"), req("index", "Index")]),
    t("search_put_mapping", "Update a mapping", "Add fields to an index's mapping (Elasticsearch's PUT /<index>/_mapping).", "search", "PUT", "/v1/search/clusters/{cluster}/indices/{index}/mapping", &[req("cluster", "Cluster"), req("index", "Index"), breq("properties", Ty::Obj, "Field mappings")]),
    t("search_get_mapping", "Get a mapping", "An index's field mappings (Elasticsearch's GET /<index>/_mapping).", "search", "GET", "/v1/search/clusters/{cluster}/indices/{index}/mapping", &[req("cluster", "Cluster"), req("index", "Index")]),
    t("search_index_settings", "Index settings", "An index's settings (Elasticsearch's GET /<index>/_settings).", "search", "GET", "/v1/search/clusters/{cluster}/indices/{index}/settings", &[req("cluster", "Cluster"), req("index", "Index")]),
    t("search_refresh", "Refresh an index", "Make what was just indexed searchable now (Elasticsearch's POST /<index>/_refresh).", "search", "POST", "/v1/search/clusters/{cluster}/indices/{index}/refresh", &[req("cluster", "Cluster"), req("index", "Index")]),
    t("search_analyze", "Analyze text", "The tokens an index's analyzer makes of some text (Elasticsearch's POST /<index>/_analyze): how a field will match.", "search", "POST", "/v1/search/clusters/{cluster}/indices/{index}/analyze", &[req("cluster", "Cluster"), req("index", "Index"), breq("text", Ty::Str, "Text to analyze"), b("analyzer", Ty::Str, "An analyzer by name"), b("field", Ty::Str, "Analyze as this field does")]),
    t("search_add_document", "Add a document", "Index a document under a new id.", "search", "POST", "/v1/search/clusters/{cluster}/indices/{index}/docs", &[req("cluster", "Cluster"), req("index", "Index"), P { name: "document", ty: Ty::Str, at: At::RawJson, required: true, desc: "The document, as JSON text" }]),
    t(
        "search_bulk",
        "Bulk index",
        "Index, update or delete many documents at once (Elasticsearch's _bulk, NDJSON). Deleting is a \"delete\" action.",
        "search",
        "POST",
        "/v1/search/clusters/{cluster}/indices/{index}/bulk",
        &[req("cluster", "Cluster"), req("index", "Index"), P { name: "ndjson", ty: Ty::Str, at: At::RawNdjson, required: true, desc: "Action and document lines" }],
    ),
    t("search_delete_by_query", "Delete by query", "Delete every document of an index that matches a query.", "search", "POST", "/v1/search/clusters/{cluster}/indices/{index}/delete_by_query", &[req("cluster", "Cluster"), req("index", "Index"), breq("query", Ty::Obj, "Query DSL")]),
    t("search_query", "Search", "Run an Elasticsearch _search body on an index.", "search", "POST", "/v1/search/clusters/{cluster}/indices/{index}/search", &[req("cluster", "Cluster id or name"), req("index", "Index"), b("query", Ty::Obj, "Query DSL"), b("size", Ty::Int, "Hits"), b("from", Ty::Int, "Offset"), b("sort", Ty::Arr, "Sort"), b("aggs", Ty::Obj, "Aggregations")]),
    t("search_get_document", "Get a document", "A document by id.", "search", "GET", "/v1/search/clusters/{cluster}/indices/{index}/docs/{doc}", &[req("cluster", "Cluster"), req("index", "Index"), req("doc", "Document id")]),
    t("search_index_document", "Index a document", "Create or replace a document.", "search", "PUT", "/v1/search/clusters/{cluster}/indices/{index}/docs/{doc}", &[req("cluster", "Cluster"), req("index", "Index"), req("doc", "Document id"), P { name: "document", ty: Ty::Str, at: At::RawJson, required: true, desc: "The document, as JSON text" }]),
    t("search_delete_document", "Delete a document", "Delete a document.", "search", "DELETE", "/v1/search/clusters/{cluster}/indices/{index}/docs/{doc}", &[req("cluster", "Cluster"), req("index", "Index"), req("doc", "Document id")]),
    // ── secrets (roll secrets) ──
    t("secrets_list_environments", "List environments", "A project's secret environments.", "secrets", "GET", "/v1/secrets/projects/{project}/environments", &[SECRET_PROJECT]),
    t("secrets_list", "List secrets", "Secret names and metadata in an environment (never values).", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/secrets", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder, e.g. /api")]),
    t("secrets_reveal", "Read a secret's value", "Read a secret's value. A key tagged sensitive is never returned to anyone (403): only running services get it, so don't retry or look for it another way.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/value", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder")]),
    t("secrets_set", "Set a secret", "Create or change a secret's value. Give if_version (the version you read) so a change someone made meanwhile isn't overwritten: then it's a 409 stale, and you read it again.", "secrets", "PUT", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}", &[SECRET_PROJECT, ENV, req("name", "Secret name"), breq("value", Ty::Str, "The value"), q("path", Ty::Str, "Folder"), b("if_version", Ty::Int, "Only if it's still this version (0: only if it doesn't exist)")]),
    t("secrets_import", "Import a .env file", "Write a .env file's keys (or JSON: {\"KEY\": \"value\"} or [{key, value, comment?}]) into an environment as one change: only new keys unless overwrite, and replace makes the folder match the file (its other own keys are deleted). On a protected environment it opens a change request instead (request: true asks for one anywhere).", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/import", &[SECRET_PROJECT, ENV, b("dotenv", Ty::Str, "A .env file's text"), b("json", Ty::Obj, "Or the keys as JSON"), b("path", Ty::Str, "Folder (default /)"), b("overwrite", Ty::Bool, "Replace keys that exist"), b("replace", Ty::Bool, "Also delete the folder's keys the file lacks"), b("message", Ty::Str, "Why, for the change's history"), b("request", Ty::Bool, "Open a change request instead of writing"), b("if_seq", Ty::Int, "Only if the environment's last change is still this one")]).destructive(),
    t("secrets_rollback", "Roll back an environment", "Put an environment's values back as they were at a change (to: its seq, from secrets_changes) or a time (at: Unix ms), as one new change: what's there now is replaced. On a protected environment it opens a change request.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/rollback", &[SECRET_PROJECT, ENV, b("to", Ty::Int, "The change (seq) to go back to"), b("at", Ty::Int, "Or a time, Unix ms"), b("message", Ty::Str, "Why")]).destructive(),
    t("secrets_create_environment", "Add an environment", "A new environment in a project (staging, a preview's): slug, optional name, base (an environment it inherits from), labels, and for an ephemeral one ttl_minutes. Making one is the project admin's.", "secrets", "POST", "/v1/secrets/projects/{project}/environments", &[SECRET_PROJECT, breq("slug", Ty::Str, "Short name (staging, pr-42)"), b("name", Ty::Str, "Display name"), b("kind", Ty::Str, "standard or ephemeral"), b("base", Ty::Str, "Environment it inherits from"), b("labels", Ty::Obj, "Labels, {key: value}"), b("ttl_minutes", Ty::Int, "An ephemeral one ends after this")]),
    t("secrets_link_targets", "List what keys can link to", "The project's resources a key can be linked to (a database, a bucket, a stream…) and each one's fields, for secrets_link.", "secrets", "GET", "/v1/secrets/projects/{project}/links", &[SECRET_PROJECT]),
    t("secrets_link", "Link a key to a resource", "Make a key read its value from a project resource instead of holding one: link {kind, ref, field, mode} from secrets_link_targets (mode: live reads it on every resolve, managed issues and stores one, dynamic issues one per resolve and revokes it when its lease ends). link: null unlinks.", "secrets", "PUT", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}", &[SECRET_PROJECT, ENV, req("name", "Secret name"), breq("link", Ty::Obj, "{kind, ref, field, mode, options?}, or null"), q("path", Ty::Str, "Folder"), b("if_version", Ty::Int, "Only if it's still this version")]),
    t("secrets_delete", "Delete a secret", "Delete a secret (if_version: only if it's still the version you read).", "secrets", "DELETE", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder"), q("if_version", Ty::Int, "Only if it's still this version")]),
    t("secrets_list_aws_roles", "List AWS roles", "A project's registered AWS roles, which dynamic AWS keys are made from (registering one is the project admin's, in the dashboard).", "secrets", "GET", "/v1/secrets/projects/{project}/aws-roles", &[req("project", "Workspace project")]),
    t("secrets_list_outside_databases", "List outside databases", "Servers roll doesn't run whose users a project's secrets rotate: Postgres, MySQL, SQL Server, ClickHouse, RabbitMQ, Redis or MongoDB (name, engine, host, users; never the admin URL; adding one is the project admin's, in the dashboard).", "secrets", "GET", "/v1/secrets/projects/{project}/outside-databases", &[req("project", "Workspace project")]),
    t("secrets_list_aws_users", "List AWS users", "A project's AWS IAM users whose access keys roll rotates (name, role, user; never a key; adding one is the project admin's, in the dashboard).", "secrets", "GET", "/v1/secrets/projects/{project}/aws-users", &[req("project", "Workspace project")]),
    t("secrets_list_cloud_accounts", "List cloud accounts", "A project's GCP and Azure accounts that dynamic credentials come from (no secrets; adding one is the project admin's, in the dashboard).", "secrets", "GET", "/v1/secrets/projects/{project}/cloud-accounts", &[req("project", "Workspace project")]),
    t("secrets_list_cas", "List certificate authorities", "A project's own certificate authorities: their public certificates, domains and limits (never a CA's key; adding one is the project admin's, in the dashboard).", "secrets", "GET", "/v1/secrets/projects/{project}/cas", &[req("project", "Workspace project")]),
    t("secrets_list_ssh_cas", "List SSH certificate authorities", "A project's SSH CAs: their public keys, principals and limits (never a private key; adding one is the project admin's, in the dashboard).", "secrets", "GET", "/v1/secrets/projects/{project}/ssh-cas", &[req("project", "Workspace project")]),
    t("secrets_totp", "Get a one-time code", "The current one-time code (TOTP) from a secret that holds an authenticator seed, and how long it lasts: a reveal, asked for like reading a value. The seed itself is never returned.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/totp", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder")]),
    t("secrets_list_saas", "List SaaS rotators", "A project's rotators for third-party API keys: each one's vendor (SendGrid, Twilio, Okta, Entra, Auth0) and the account or app it rotates for (never a credential; adding one is the project admin's).", "secrets", "GET", "/v1/secrets/projects/{project}/saas", &[SECRET_PROJECT]),
    t("secrets_list_kubernetes", "List Kubernetes clusters", "The Kubernetes clusters a project issues service-account tokens for: names, API servers, and whether calls trust the cluster's own CA (never a token or the CA; adding one is the project admin's).", "secrets", "GET", "/v1/secrets/projects/{project}/kubernetes", &[SECRET_PROJECT]),
    t("secrets_list_github_apps", "List GitHub Apps", "A project's GitHub Apps that short-lived installation tokens come from: app and installation ids, repositories and permissions (never the private key; adding one is the project admin's, in the dashboard).", "secrets", "GET", "/v1/secrets/projects/{project}/github-apps", &[req("project", "Workspace project")]),
    t("secrets_list_directories", "List LDAP and Active Directory servers", "A project's directories whose account passwords roll rotates and where dynamic users are made: URL, flavor, bind DN, managed accounts, groups and the gateway it's reached through (never the bind password; adding one is the project admin's, in the dashboard).", "secrets", "GET", "/v1/secrets/projects/{project}/directories", &[SECRET_PROJECT]),
    t("secrets_list_snowflake_accounts", "List Snowflake accounts", "A project's Snowflake accounts that rotated keys and dynamic users come from: account, host, admin user and role, the admin key's public fingerprint, the managed user and roles (never the key; adding one is the project admin's).", "secrets", "GET", "/v1/secrets/projects/{project}/snowflake-accounts", &[SECRET_PROJECT]),
    t("secrets_list_atlas_projects", "List MongoDB Atlas projects", "A project's MongoDB Atlas projects that rotated passwords and dynamic database users come from: the Atlas group id, the service account's client id, managed users and roles (never the client secret; adding one is the project admin's).", "secrets", "GET", "/v1/secrets/projects/{project}/atlas-projects", &[SECRET_PROJECT]),
    t("secrets_list_gateways", "Gateways: is a private network reachable", "A project's gateways into private networks, each with its link's status: online, the node holding it, its version, the allow-list it reported, when it connected and was last seen. Use it when a directory or outside database behind one fails (never a token; making one is the project admin's).", "secrets", "GET", "/v1/secrets/projects/{project}/gateways", &[SECRET_PROJECT]),
    t("secrets_ca_acme", "A CA's ACME server", "A certificate authority's ACME directory URL, whether names are pre-authorized, the challenges it checks, and its external account bindings (label, names, expiry, last use, accounts; never an HMAC key; making one is the project admin's).", "secrets", "GET", "/v1/secrets/projects/{project}/cas/{id}/acme", &[SECRET_PROJECT, req("id", "Certificate authority id")]),
    t("secrets_changes", "Secret changes", "What changed in an environment, newest first: who, when, which keys (fingerprints, never values).", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/changes", &[SECRET_PROJECT, ENV, q("before", Ty::Int, "Changes before this sequence number"), q("limit", Ty::Int, "How many")]),
    t("secrets_get_change", "Get a secrets change", "One change of an environment (seq, from secrets_changes): who, when, why, and what it did to each key (fingerprints, never values); state: true adds every key as it was after it.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/changes/{seq}", &[SECRET_PROJECT, ENV, req("seq", "The change's seq"), q("state", Ty::Bool, "Every key as it was after it")]),
    t("secrets_insights", "Secret insights", "Each key in a folder: its version, who changed it and when, its last read and reads in 30 days, and whether it's stale (unchanged past the environment's max age) or expiring. Which keys nobody uses, which to rotate.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/insights", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder")]),
    t("secrets_required", "Required keys", "The keys every standard environment of a project must define (and their types or patterns, when set): what an environment is missing before it runs.", "secrets", "GET", "/v1/secrets/projects/{project}/required", &[SECRET_PROJECT]),
    t("secrets_folders", "List folders", "An environment's folders under a path.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/folders", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Under this folder (default /)")]),
    t("secrets_suggestions", "Suggested links", "Links roll suggests for a project's resources (a database, a bucket…): which keys each would fill and where, for secrets_link.", "secrets", "GET", "/v1/secrets/projects/{project}/suggestions", &[SECRET_PROJECT]),
    t("secrets_versions", "A secret's versions", "A secret's history, newest first (fingerprints, who and when; never values).", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/versions", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder")]),
    t("secrets_compare", "Compare environments", "Which keys differ between two environments (missing, or a different value by fingerprint; never values).", "secrets", "GET", "/v1/secrets/projects/{project}/compare", &[SECRET_PROJECT, qreq("from", "Environment"), qreq("to", "Environment"), q("path", Ty::Str, "Folder")]),
    t("secrets_rotate", "Rotate a secret", "Make a new value for a secret and switch to it: a generated one, or a managed link's (a database user's password, a cloud key) made where it lives.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/rotate", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder")]),
    t("secrets_promote", "Promote secrets", "Copy an environment's secrets into this one (all, or some keys). Into a protected environment it opens a change request for a person instead.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/promote", &[SECRET_PROJECT, ENV, qreq("from", "The environment to copy from"), b("keys", Ty::Arr, "Only these keys"), b("path", Ty::Str, "Folder"), b("message", Ty::Str, "Why")]),
    t("secrets_list_requests", "Change requests", "An environment's change requests (proposed changes waiting on review).", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/requests", &[SECRET_PROJECT, ENV, q("state", Ty::Str, "open, applied or closed")]),
    t("secrets_propose", "Propose a change", "Propose changes to an environment for a person to review and apply: ops [{op: set, key, value?, if_version?} or {op: delete, key, if_version?}]; if_version keeps each op from applying over a change made meanwhile. The way to change a protected environment.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/requests", &[SECRET_PROJECT, ENV, breq("ops", Ty::Arr, "[{op: set|delete, key, value?, comment?, if_version?}]"), b("message", Ty::Str, "Why"), b("path", Ty::Str, "Folder")]),
    t("secrets_transit_encrypt", "Encrypt with a key", "Encrypt with one of a project's encryption keys; the key never leaves roll. plaintext is base64. Returns ciphertext (roll:v<n>:…) to store.", "secrets", "POST", "/v1/secrets/projects/{project}/transit/{name}/encrypt", &[SECRET_PROJECT, req("name", "Encryption key name"), breq("plaintext", Ty::Str, "Base64"), b("context", Ty::Str, "Base64 context, if the key derives per context")]),
    t("secrets_transit_decrypt", "Decrypt with a key", "Decrypt a ciphertext (roll:v<n>:…, or vault:v<n>:… for a key brought from Vault). Returns plaintext as base64. Audited.", "secrets", "POST", "/v1/secrets/projects/{project}/transit/{name}/decrypt", &[SECRET_PROJECT, req("name", "Encryption key name"), breq("ciphertext", Ty::Str, "roll:v<n>:…"), b("context", Ty::Str, "Base64 context")]),
    t("secrets_transit_sign", "Sign with a key", "An Ed25519 signature over input's raw bytes, by a project's signing key. input is base64. Returns signature (roll:v<n>:…).", "secrets", "POST", "/v1/secrets/projects/{project}/transit/{name}/sign", &[SECRET_PROJECT, req("name", "Signing key name"), breq("input", Ty::Str, "Base64")]),
    t("secrets_transit_verify", "Verify a signature", "Whether signature (roll:v<n>:…) is the key's over input (base64): valid true or false; a retired version is false.", "secrets", "POST", "/v1/secrets/projects/{project}/transit/{name}/verify", &[SECRET_PROJECT, req("name", "Signing key name"), breq("input", Ty::Str, "Base64"), breq("signature", Ty::Str, "roll:v<n>:…")]),
    t("secrets_transit_public_key", "A signing key's public key", "Each version's public key (raw base64, PEM and JWK), for checking signatures outside roll.", "secrets", "GET", "/v1/secrets/projects/{project}/transit/{name}/public", &[SECRET_PROJECT, req("name", "Signing key name")]),
    t("secrets_list_transit_keys", "List encryption keys", "A project's encryption and signing keys: names, kinds, versions and dates (never key material).", "secrets", "GET", "/v1/secrets/projects/{project}/transit", &[SECRET_PROJECT]),
    t("secrets_transit_rewrap", "Rewrap a ciphertext", "Re-encrypt a ciphertext under the key's newest version, without the plaintext leaving roll (after a rotation).", "secrets", "POST", "/v1/secrets/projects/{project}/transit/{name}/rewrap", &[SECRET_PROJECT, req("name", "Encryption key name"), breq("ciphertext", Ty::Str, "roll:v<n>:…"), b("context", Ty::Str, "Base64, as given when encrypting")]),
    t("secrets_transit_hmac", "HMAC with a key", "An HMAC-SHA256 of input by a project's key (input is base64).", "secrets", "POST", "/v1/secrets/projects/{project}/transit/{name}/hmac", &[SECRET_PROJECT, req("name", "Key name"), breq("input", Ty::Str, "Base64")]),
    t("secrets_transit_hmac_verify", "Check an HMAC", "Whether an HMAC of input is the key's.", "secrets", "POST", "/v1/secrets/projects/{project}/transit/{name}/hmac/verify", &[SECRET_PROJECT, req("name", "Key name"), breq("input", Ty::Str, "Base64"), breq("hmac", Ty::Str, "The HMAC to check")]),
    t("secrets_transit_datakey", "Make a data key", "A fresh data key for encrypting locally: its plaintext and the same key encrypted by the project's key (store that one). Audited.", "secrets", "POST", "/v1/secrets/projects/{project}/transit/{name}/datakey", &[SECRET_PROJECT, req("name", "Encryption key name"), b("context", Ty::Str, "Base64")]),
    t(
        "secrets_version_value",
        "Reveal an old version",
        "One earlier version's value of a secret (secrets_versions lists them), to compare or roll back.",
        "secrets",
        "GET",
        "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/versions/{v}/value",
        &[SECRET_PROJECT, ENV, req("name", "Secret name"), req("v", "Version number"), q("path", Ty::Str, "Folder")],
    ),
    t("secrets_renew_lease", "Renew a lease", "Push a dynamic credential's expiry out, as a long-running consumer does instead of restarting.", "secrets", "POST", "/v1/secrets/projects/{project}/leases/{id}/renew", &[SECRET_PROJECT, req("id", "Lease id"), b("ttl_minutes", Ty::Int, "Minutes from now")]),
    t("secrets_reach", "Who reaches a secret", "Which tokens and identities reach an environment's folder or key, whether code nobody vetted can, where it flows (syncs, shares, imports) and who read it in 30 days: names only. Who has to rotate what after a leak (the project admin's).", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/reach", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder"), q("key", Ty::Str, "One key")]),
    t("secrets_folder_policies", "Folder approval policies", "An environment's approval policies per folder: who must approve a change there.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/folder-policies", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder")]),
    t("secrets_list_rotators", "Rotators", "A project's rotators: which secrets roll rotates (a database password, an API key), how often and from what.", "secrets", "GET", "/v1/secrets/projects/{project}/rotators", &[SECRET_PROJECT]),
    t("secrets_list_syncs", "Syncs", "Where a project's environments are pushed (GitHub, Vercel, AWS, Kubernetes…) and how each last went.", "secrets", "GET", "/v1/secrets/projects/{project}/syncs", &[SECRET_PROJECT]),
    t("secrets_diff", "Compare a local .env", "Compare a local .env (dotenv) or JSON with an environment by fingerprint: names added, changed, the same, missing; never a value.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/diff", &[SECRET_PROJECT, ENV, b("dotenv", Ty::Str, "The file's text"), b("json", Ty::Obj, "{NAME: value}"), b("path", Ty::Str, "Folder")]),
    t("secrets_list_rotations", "Rotations", "A project's secret rotations: what rotated, when, and what's due.", "secrets", "GET", "/v1/secrets/projects/{project}/rotations", &[req("project", "Workspace project")]),
    t("secrets_ca_certificates", "A CA's certificates", "The certificates a project's CA has issued (public, with their expiry; never a key).", "secrets", "GET", "/v1/secrets/projects/{project}/cas/{ca}/certificates", &[req("project", "Workspace project"), req("ca", "CA id")]),
    t("secrets_get_values", "Reveal an environment's values", "All of an environment's values (a folder's, with path) at once, resolved (references and imports), as one reveal instead of one per key; what can't be resolved is named, never guessed. dynamic=false leaves dynamic keys out instead of minting fresh credentials. Needs reveal.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/values", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder"), q("dynamic", Ty::Bool, "false: leave dynamic keys out (named in dynamic)")]),
    t("secrets_list_leases", "List leases", "A project's live leases on dynamic credentials (a database user, a cloud key made for a reader): who holds each, for what, and when it ends. Never a credential.", "secrets", "GET", "/v1/secrets/projects/{project}/leases", &[SECRET_PROJECT]),
    t("secrets_end_lease", "End a lease", "End a dynamic credential's lease now (the credential is revoked where it lives): yours once you're done with it, or anyone's with more than reveal.", "secrets", "DELETE", "/v1/secrets/projects/{project}/leases/{id}", &[SECRET_PROJECT, req("id", "Lease id")]),
    t("secrets_health", "Secrets health", "What needs fixing in a project's secrets, by name (never values), in the environments you may read: expired, missing, exposed, weak, expiring, stale or unread ones, rotations or revocations failing, certificates expiring, and values marked public.", "secrets", "GET", "/v1/secrets/projects/{project}/health", &[
        SECRET_PROJECT,
        q("code", Ty::Bool, "Also keys no repository on the project's canvas mentions (needs code read on every one)"),
    ]),
    t("secrets_findings", "Secrets to fix", "Every project's secrets health findings you may read, leaving out projects with none.", "secrets", "GET", "/v1/secrets/findings", &[]),
    t("secrets_find", "Find a key", "Where a key is set (project, environment, folder) across the projects you may read: before rotating it.", "secrets", "GET", "/v1/secrets/search", &[qreq("key", "The key, e.g. STRIPE_KEY")]),
    t("secrets_scan", "Scan for committed secrets", "Which of these values are a project's stored secrets (by key, environment and path, never the value): check a diff before committing it. Only values you may reveal are confirmed.", "secrets", "POST", "/v1/secrets/projects/{project}/leaks", &[
        SECRET_PROJECT,
        breq("candidates", Ty::Arr, "Strings from the diff to check (at most 20,000)"),
    ]),
    // ── apps (roll apps) ──
    t("apps_list_services", "List services", "App services; with deleted, the ones deleted in the last 48 hours, which apps_restore_service brings back until purge_at.", "apps", "GET", "/v1/apps/services", &[q("project", Ty::Str, "Only this workspace project's"), q("deleted", Ty::Bool, "The deleted services kept for a restore")]),
    t("apps_restore_service", "Restore a deleted service", "Bring back a service deleted in the last 48 hours, as it was: environments, volumes, domains, links, name, and its live deployments running again (a \"deploy\" action).", "apps", "POST", "/v1/apps/services/{service}/restore", &[SERVICE]),
    t(
        "apps_create_service",
        "Create a service",
        "A new service in a workspace project, from a roll code repo ({kind: \"repo\", repo: \"owner/name\", root?}), a public Git URL ({kind: \"git\", url: \"https://github.com/owner/name\", root?}, with branch? to follow), an image ({kind: \"image\", image}) or an upload ({kind: \"upload\"}, deployed with roll up). A repo, Git or image service deploys to production at once (a \"deploy\" action).",
        "apps",
        "POST",
        "/v1/apps/projects/{project}/services",
        &[req("project", "Workspace project"), breq("name", Ty::Str, "Service name"), breq("source", Ty::Obj, "Where the code comes from"), b("branch", Ty::Str, "A repo source's branch for production (default: the repo's default)"), b("variables", Ty::Obj, "{NAME: value}, set before the first deploy"), b("run", Ty::Obj, "Run settings: start, port, scale, size"), b("links", Ty::Arr, "[{kind: postgres|redis|bucket|stream|queue|nosql, ref, prefix?}]: connection variables, before the first deploy (reveal on each)")],
    ),
    t("apps_list_environments", "List environments", "A service's environments.", "apps", "GET", "/v1/apps/services/{service}/environments", &[SERVICE]),
    t("apps_get_service", "Get a service", "A service with its environments' summaries (apps_list_deploys lists an environment's deploys).", "apps", "GET", "/v1/apps/services/{service}", &[SERVICE]),
    t("apps_set_environment", "Set up an environment", "Create an environment or change one: the branch it deploys, its own run settings, following an image tag, waiting for CI, and who gets in (maintenance, allowed addresses, a rate limit, firewall rules). Production changes are \"deploy\" actions; copying another environment's variables needs reveal there.", "apps", "PUT", "/v1/apps/services/{service}/environments/{env}", &[
        SERVICE,
        ENV,
        b("branch", Ty::Str, "The git branch it deploys"),
        b("ephemeral", Ty::Bool, "A preview environment"),
        b("run", Ty::Obj, "Its own run settings (min/max instances, cpu, memory, concurrency, suspend, sleep)"),
        b("follow_tag", Ty::Bool, "An image service: redeploy when its tag points at a new digest"),
        b("wait_for_ci", Ty::Bool, "Deploy a commit only once its checks pass"),
        b("variables_from", Ty::Str, "Creating it: start with this environment's variables"),
        // Who gets in (apps-next #26): in production each change is deploy.
        b("maintenance", Ty::Any, "true, {message} or false: answer a maintenance page instead of the app"),
        b("allow_ips", Ty::Arr, "Only these addresses get in: [cidr or {cidr, description}]"),
        b("rate_limit", Ty::Obj, "{per_minute, burst?}: requests a client address may make"),
        b("firewall", Ty::Arr, "[{action: allow|deny|limit|challenge, path?, methods?, user_agent?, per_minute?, burst?}], first match wins; limit holds a client to per_minute (1-100000) on what it matches; challenge lets in a browser that passed a proof of work this hour (GET only)"),
        b("routes", Ty::Arr, "[{path, service}] (at most 20): a path (a prefix ends in *) another service of the project answers, by name, or null for none"),
        b("egress", Ty::Any, "{mode: roll|open, hosts?}: roll lets its code reach roll and only these hosts (at most 20, *. for subdomains), from the next deploy; null is open"),
        b("vitals", Ty::Bool, "Measure its pages' Web Vitals in visitors' browsers (read them with apps_vitals)"),
        b("traces", Ty::Bool, "Send its OpenTelemetry traces and metrics to roll (OTEL_EXPORTER_OTLP_* from the next deploy)"),
        b("redeploy_on_secrets", Ty::Bool, "Redeploy (the same image, new values) when the project's roll secrets change"),
        b("pages", Ty::Obj, "{error?, maintenance?}: its own HTML for roll's error and maintenance pages (null puts roll's back)"),
        b("headers", Ty::Any, "Response header rules: [{path?, set: {name: value}, replace?}] (at most 20), or null for none"),
        b("paused", Ty::Bool, "Pause it (no deploys on push, nothing running) or resume it"),
        // A rollback holds deploys (apps #26): false lets pushes and hooks deploy again.
        b("held", Ty::Bool, "false: resume deploys a rollback held (held_after shows since when)"),
        b("hostname", Ty::Str, "Its new name on the roll domain (<hostname>.<domain>); the old one stops answering"),
        b("report", Ty::Bool, "false: its deploys post no commit status and no GitHub Deployment (something else reports them)"),
    ]),
    t("apps_update_service", "Update a service", "Change a service's name, source, build or run settings (fields given are changed; run takes alerts {memory_pct, error_pct, p95_ms} for metric alerts).", "apps", "PATCH", "/v1/apps/services/{service}", &[
        SERVICE,
        b("name", Ty::Str, "New name"),
        b("source", Ty::Obj, "Where it's built from (repo or image), as apps_get_service shows it"),
        b("build", Ty::Obj, "Build settings to change"),
        b("run", Ty::Obj, "Run settings to change (instances, memory, start command, h2c, alerts {memory_pct, error_pct, p95_ms}; schedules [{name, schedule (cron or @daily), command, timezone?, timeout_s?}] runs commands as jobs of the live deployment, replacing the list; health checks health_path, health_every_s, health_failures, or a worker's health_command; rollout {steps, step_s, error_pct, manual?} for gradual rollouts (manual waits for apps_rollout); rebuild_on_base rebuilds the same revision when a base image moves; only size, scale, health checks, timers, alerts, rollout and restarts are write; any other run setting needs deploy, as source and build do; a worker's scale {stream, group, per_instance} needs read on that stream)"),
    ]),
    t(
        "apps_deploy",
        "Deploy",
        "Deploy a service to an environment: a git revision of its repo, or an image (production deploys are \"deploy\" actions).",
        "apps",
        "POST",
        "/v1/apps/services/{service}/environments/{env}/deploys",
        &[
            SERVICE,
            ENV,
            b("revision", Ty::Str, "Git commit or branch of the service's repo"),
            b("image", Ty::Str, "Or an image"),
            b("message", Ty::Str, "Why"),
            b("no_traffic", Ty::Bool, "Build and health-check it, then stop at status ready without sending it traffic"),
            b("no_cache", Ty::Bool, "Build from scratch: no cached layer, no reused build"),
            b("closed", Ty::Bool, "Refuse (409) unless the environment builds closed (its builds get no secret value)"),
            IDEMPOTENCY,
        ],
    ),
    t("apps_list_deploys", "List deploys", "An environment's deploys, newest first.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/deploys", &[SERVICE, ENV]),
    t("apps_deploy_logs", "Deploy logs", "A deploy's build and release log.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/logs", &[SERVICE, ENV, req("deploy", "Deploy id"), q("tail", Ty::Int, "Last lines")]),
    t(
        "apps_promote",
        "Promote",
        "Deploy the image another environment is running into this one, with this environment's variables and no build (production is a \"deploy\" action).",
        "apps",
        "POST",
        "/v1/apps/services/{service}/environments/{env}/promote",
        &[SERVICE, ENV, breq("from", Ty::Str, "The environment to take it from"), b("message", Ty::Str, "Why")],
    ),
    t("apps_cancel_deploy", "Cancel a deploy", "Stop a deploy that hasn't gone live.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/cancel", &[SERVICE, ENV, req("deploy", "Deploy id")]),
    t(
        "apps_rollout",
        "Move a manual rollout",
        "A gradual rollout set to manual waits after each step that passed (the environment's canary says waiting): advance to its next step, all to send it every request, or abort to put the old deployment back. A \"deploy\" action.",
        "apps",
        "POST",
        "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/rollout",
        &[SERVICE, ENV, req("deploy", "Deploy id (the canary's deployment)"), breq("action", Ty::Str, "advance, all or abort")],
    ),
    t("apps_sleep", "Sleep", "Stop an environment's instances until a request or apps_wake wakes them.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/sleep", &[SERVICE, ENV]),
    t("apps_wake", "Wake", "Start a sleeping environment's instances.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/wake", &[SERVICE, ENV]),
    t("apps_usage", "Service usage", "An environment's usage and cost per day.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/usage", &[SERVICE, ENV, q("days", Ty::Int, "Days back")]),
    t("apps_restart", "Restart a service", "Restart an environment's instances.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/restart", &[SERVICE, ENV]),
    t(
        "apps_purge_cache",
        "Purge cached answers",
        "Drop answers roll's cache keeps for a web service (on-demand revalidation): some paths (a trailing * a prefix), answers carrying some cache tags, or neither for everything. Answers how many went. Production is a \"deploy\" action.",
        "apps",
        "POST",
        "/v1/apps/services/{service}/environments/{env}/cache/purge",
        &[SERVICE, ENV, b("paths", Ty::Arr, "1-100 absolute paths (any query; a trailing * is a prefix)"), b("tags", Ty::Arr, "1-100 cache tags its answers carried (any case)")],
    ),
    t("apps_restart_instance", "Restart one instance", "Replace one instance of a web service (a fresh one answers before this one drains; if it doesn't start, this one keeps serving). Instance names are in apps_get_environment. Production is a \"deploy\" action.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/instances/{name}/restart", &[SERVICE, ENV, req("name", "Instance id (instances[].id in apps_get_environment)")]),
    t("apps_redeploy", "Redeploy", "Deploy what's running again with the current settings and variables (production is a \"deploy\" action).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/redeploy", &[SERVICE, ENV, b("message", Ty::Str, "Why"), b("closed", Ty::Bool, "Refuse (409) unless the environment builds closed")]),
    t(
        "apps_run_job",
        "Run a job",
        "Run a one-off command in an environment's image with its variables (a migration, a script); read its output with apps_job_logs. Production is a \"deploy\" action.",
        "apps",
        "POST",
        "/v1/apps/services/{service}/environments/{env}/jobs",
        &[SERVICE, ENV, breq("command", Ty::Str, "The command line"), b("timeout_s", Ty::Int, "Seconds before it's stopped")],
    ),
    t("apps_job_logs", "Job logs", "A job's output (it ends with the job).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/jobs/{job}/logs", &[SERVICE, ENV, req("job", "Job id"), q("tail", Ty::Int, "Last lines")]),
    t("apps_list_jobs", "List jobs", "An environment's one-off jobs, newest first: command, status, exit code, when.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/jobs", &[SERVICE, ENV]),
    t("apps_get_job", "Get a job", "One job: its status, exit code and timing; poll it until it ends, then read apps_job_logs.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/jobs/{job}", &[SERVICE, ENV, req("job", "Job id")]),
    t(
        "apps_wait_job",
        "Wait for a job to finish",
        "A job once it ends (succeeded, failed, skipped, timed out or canceled), with its exit code. A client that runs MCP tasks gets a task that waits as long as it takes; otherwise this answers the status now (call again to keep waiting).",
        "apps",
        "GET",
        "/v1/apps/services/{service}/environments/{env}/jobs/{job}",
        &[SERVICE, ENV, req("job", "Job id")],
    )
    .until(job_over),
    t("apps_cancel_job", "Cancel a job", "Stop a running job (production is a \"deploy\" action).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/jobs/{job}/cancel", &[SERVICE, ENV, req("job", "Job id")]),
    t("apps_events", "App events", "What happened to services (deploys going live or failing, crash loops, failed cron runs and jobs, memory, error and latency alerts, and audit.* changes to settings, variables, domains and links), in order after a cursor.", "apps", "GET", "/v1/apps/events", &[q("after", Ty::Int, "The last answer's next"), q("limit", Ty::Int, "At most this many")]),
    t("apps_list_variables", "List variables", "An environment's variables, values masked. A sealed variable is never readable again by anyone (only the running service gets it): set a new value or delete it, never look for the old one.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/variables", &[SERVICE, ENV]),
    t(
        "apps_set_variables",
        "Set variables",
        "Set or unset an environment's variables, or copy some from another environment (copy_from with copy: their values, never shown; needs reveal on that environment too). Production is a \"deploy\" action.",
        "apps",
        "PATCH",
        "/v1/apps/services/{service}/environments/{env}/variables",
        &[SERVICE, ENV, b("set", Ty::Obj, "{NAME: value}"), b("unset", Ty::Arr, "Names to remove"), b("copy_from", Ty::Str, "Another environment to copy values from"), b("copy", Ty::Arr, "Names to copy from copy_from")],
    ),
    t("apps_diff_environments", "Compare environments", "How an environment differs from another (Railway's environment sync): variables missing, extra or with another value (names only; which differ needs reveal on both), and settings with the body that makes each match. Read on both environments.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/diff", &[SERVICE, ENV, qreq("from", "The environment to compare with")]),
    t("apps_metrics", "Service metrics", "CPU, memory and requests of an environment, and each instance's own last hour (by_instance).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/metrics", &[SERVICE, ENV, q("minutes", Ty::Int, "Window")]),
    t("apps_plan", "Plan a service", "How roll would build and run a repo (provider, framework, install, build and start commands, port), before making a service: a dry run.", "apps", "POST", "/v1/apps/plan", &[breq("repo", Ty::Str, "owner/name"), b("revision", Ty::Str, "A branch or commit"), b("root", Ty::Str, "The app's folder in the repo")]),
    t("apps_import", "Check a project file", "What a roll.toml or a docker-compose file would make, without making anything: the apps with their source, branch, run settings, links and variables, the databases, notes, and the file as roll.toml (check it before roll up --all).", "apps", "POST", "/v1/apps/import", &[b("roll_toml", Ty::Str, "A roll.toml's text"), b("compose", Ty::Str, "A docker-compose.yml's text")]),
    t("apps_export_project", "Export a project file", "A project as a roll.toml: its services (source, branch, run settings that differ from defaults) and the postgres and redis databases they link, variables by name only, never a value. Write it to the repo, check it with apps_import, deploy it with roll up --all.", "apps", "POST", "/v1/apps/projects/{project}/export", &[req("project", "Workspace project"), b("databases", Ty::Obj, "Names for the linked databases, as {\"postgres:<ref>\": \"db\"}")]),
    t("apps_get_environment", "Get an environment", "One environment: its URL, state (running, asleep), live deploy, branch and size; build_closed (its builds get no secret value) and base_updated (base images that moved since its build).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}", &[SERVICE, ENV]),
    t("apps_delete_environment", "Delete an environment", "Delete an environment with its deploys, instances and domains. Irreversible.", "apps", "DELETE", "/v1/apps/services/{service}/environments/{env}", &[SERVICE, ENV]),
    t("apps_link", "Link a resource", "Give a service a roll resource's connection variables (DATABASE_URL and the like, with prefix if given): kind is postgres, redis, bucket, stream, queue, nosql or search (a search cluster: a data-only key of the link's own). Needs reveal on the resource; the next deploy picks it up.", "apps", "PUT", "/v1/apps/services/{service}/links/{kind}/{ref}", &[SERVICE, req("kind", "postgres, redis, bucket, stream, queue, nosql or search"), req("ref", "The resource's id or name"), b("prefix", Ty::Str, "Variable name prefix")]),
    t("apps_unlink", "Unlink a resource", "Stop giving a service a linked resource's variables (from the next deploy).", "apps", "DELETE", "/v1/apps/services/{service}/links/{kind}/{ref}", &[SERVICE, req("kind", "Link kind"), req("ref", "The resource's id or name")]),
    t("apps_verify_domain", "Check a domain", "Check a custom domain's DNS now and answer its state (pending, verified) and the record to add.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/domains/{host}/verify", &[SERVICE, ENV, req("host", "The domain")]),
    t("apps_list_backups", "Volume backups", "An environment's volume backups, newest first (7 kept).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/volume/backups", &[SERVICE, ENV]),
    t("apps_backup_volume", "Back up a volume", "Back up an environment's volume now.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/volume/backups", &[SERVICE, ENV]),
    t("apps_restore_backup", "Restore a volume backup", "Put an environment's volume back as it was in a backup, replacing its files; the environment sleeps meanwhile.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/volume/backups/{backup}/restore", &[SERVICE, ENV, req("backup", "Backup id")]),
    t("apps_activity", "Service activity", "Who changed what on a service, newest first (never a value); with all, its whole timeline: deploys, failures, crash loops, alerts, domains, spend.", "apps", "GET", "/v1/apps/services/{service}/activity", &[SERVICE, q("all", Ty::Bool, "The whole timeline, not only changes")]),
    t("apps_get_deploy", "Get a deploy", "One deploy: its status, commit, image and when it went live.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}", &[SERVICE, ENV, req("deploy", "Deploy id")]),
    t(
        "apps_wait_deploy",
        "Wait for a deploy to finish",
        "A deploy once it's live or failed, with how far each step got. A client that runs MCP tasks gets a task that waits as long as it takes; otherwise this answers the status now (call again to keep waiting).",
        "apps",
        "GET",
        "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}",
        &[SERVICE, ENV, req("deploy", "Deploy id (from apps_deploy, apps_redeploy or apps_promote)")],
    )
    .until(deploy_over),
    t("apps_list_links", "List links", "The databases, buckets, streams and other services a service is linked to.", "apps", "GET", "/v1/apps/services/{service}/links", &[SERVICE]),
    t("apps_vitals", "Web Vitals", "What visitors' browsers measured on an environment's pages (with vitals on): LCP, INP, CLS, FCP and TTFB p75s rated good, needs-improvement or poor, overall and for the 20 routes with the most page loads.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/vitals", &[SERVICE, ENV, q("minutes", Ty::Int, "How far back, 60-10080 (default 1440)")]),
    t("apps_route_stats", "Route stats", "An environment's routes from its access log over the last minutes (5-60, default 60): requests, 5xx and 4xx errors, p50/p95/max latency per route (ids folded as :id), the busiest, most failing and slowest 50 of each. Start here when a service is slow or failing.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/routes", &[SERVICE, ENV, q("minutes", Ty::Int, "How far back, 5-60 (default 60)")]),
    t("apps_errors", "Errors: why a service is failing", "An environment's errors over the last minutes (5-10080, default 1440), grouped (the same error once, with its count, first and last seen and a sample), the most frequent first, at most 50. Start here when a service is failing or erroring.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/errors", &[SERVICE, ENV, q("minutes", Ty::Int, "How far back, 5-10080 (default 1440)")]),
    t("apps_traces", "Traces of slow requests", "An environment's recent traces (traces on), newest first, at most 100: each one's root span, service, duration, span count and whether any span failed; min_ms keeps the slow ones. apps_get_trace has one's spans.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/traces", &[SERVICE, ENV, q("minutes", Ty::Int, "How far back (default 60)"), q("min_ms", Ty::Int, "Only traces at least this slow")]),
    t("apps_get_trace", "Get a trace", "One trace by its id (from apps_traces): its spans in start order, from the last week.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/traces/{trace}", &[SERVICE, ENV, req("trace", "Trace id")]),
    t("apps_app_metrics", "App metrics", "What the app's OpenTelemetry SDK sent as metrics (traces on) over the last minutes (default 60): each series (a name and its attributes, the busiest 50) as at most 60 points.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/app-metrics", &[SERVICE, ENV, q("minutes", Ty::Int, "How far back (default 60)")]),
    t("apps_analytics", "Web analytics", "An environment's page views and visitors from its access log (no script, no cookie) over the last minutes (default 1440): top pages, referring sites, countries and a series.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/analytics", &[SERVICE, ENV, q("minutes", Ty::Int, "How far back (default 1440)")]),
    t("apps_list_cron_runs", "Cron runs", "A cron service's runs, newest first.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/runs", &[SERVICE, ENV]),
    t("apps_get_cron_run", "Get a cron run", "One cron run: its status, exit code, start and end.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/runs/{run}", &[SERVICE, ENV, req("run", "Run id")]),
    t("apps_cron_run_logs", "A cron run's logs", "A cron run's output (its lines in the environment's log).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/runs/{run}/logs", &[SERVICE, ENV, req("run", "Run id")]),
    t("apps_cancel_cron_run", "Cancel a cron run", "Stop a running cron run (production is a \"deploy\" action).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/runs/{run}/cancel", &[SERVICE, ENV, req("run", "Run id")]),
    t("apps_run_cron", "Run a cron now", "Run a cron service's job now (in production this is a deploy action).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/runs", &[SERVICE, ENV]),
    t("apps_limits", "Spend limits", "A project's apps spend limits and how much of them is used.", "apps", "GET", "/v1/apps/projects/{project}/limits", &[req("project", "Workspace project")]),
    t("apps_list_project_services", "List a project's services", "The services in one workspace project.", "apps", "GET", "/v1/apps/projects/{project}/services", &[req("project", "Workspace project")]),
    t("apps_list_notifications", "List notifications", "Where a project's deploy, crash and preview events are sent (webhook, Slack, Discord, email; the URL's host only).", "apps", "GET", "/v1/apps/projects/{project}/notifications", &[req("project", "Workspace project")]),
    t(
        "apps_add_notification",
        "Notify Slack, Discord, email or a webhook",
        "Notify a webhook, Slack, Discord or an email address of a project's events (the URL is stored sealed and never shown again).",
        "apps",
        "POST",
        "/v1/apps/projects/{project}/notifications",
        &[req("project", "Workspace project"), b("kind", Ty::Str, "webhook (default), slack, discord or email"), breq("url", Ty::Str, "Where to send (an address for email)"), b("events", Ty::Arr, "Which events (default: deploys and crashes)"), b("environments", Ty::Arr, "Only these environments"), b("services", Ty::Arr, "Only these services"), b("previews", Ty::Bool, "Previews' events too")],
    ),
    t("apps_update_notification", "Change a notification", "Change which events, environments and services a notification gets: given ones replace.", "apps", "PATCH", "/v1/apps/projects/{project}/notifications/{nid}", &[req("project", "Workspace project"), req("nid", "Notification id"), b("events", Ty::Arr, "Events"), b("environments", Ty::Arr, "Environments"), b("services", Ty::Arr, "Services"), b("previews", Ty::Bool, "Previews' events too")]),
    t("apps_delete_notification", "Delete a notification", "Stop sending a project's events there.", "apps", "DELETE", "/v1/apps/projects/{project}/notifications/{nid}", &[req("project", "Workspace project"), req("nid", "Notification id")]),
    t("apps_test_notification", "Test a notification", "Send a test event now (not retried): whether it arrived.", "apps", "POST", "/v1/apps/projects/{project}/notifications/{nid}/test", &[req("project", "Workspace project"), req("nid", "Notification id")]),
    t("apps_notification_deliveries", "Notification deliveries", "A notification's last 50 deliveries, newest first: what was sent and whether it arrived.", "apps", "GET", "/v1/apps/projects/{project}/notifications/{nid}/deliveries", &[req("project", "Workspace project"), req("nid", "Notification id")]),
    t("apps_list_drains", "List log drains", "Where a project's logs are sent (the URL's host only).", "apps", "GET", "/v1/apps/projects/{project}/drains", &[req("project", "Workspace project")]),
    t(
        "apps_add_drain",
        "Add a log drain",
        "Send every matching service's logs out of roll to an HTTPS endpoint, signed (its signing secret comes back once).",
        "apps",
        "POST",
        "/v1/apps/projects/{project}/drains",
        &[req("project", "Workspace project"), breq("url", Ty::Str, "https URL on a public host"), b("sources", Ty::Arr, "runtime (default), http, build"), b("environments", Ty::Arr, "Only these environments"), b("services", Ty::Arr, "Only these services"), b("previews", Ty::Bool, "Previews' logs too"), b("headers", Ty::Obj, "Headers sent with each batch")],
    ),
    t("apps_update_drain", "Change a log drain", "Change what a drain sends or its headers: given ones replace (a new URL is a new drain).", "apps", "PATCH", "/v1/apps/projects/{project}/drains/{did}", &[req("project", "Workspace project"), req("did", "Drain id"), b("sources", Ty::Arr, "Sources"), b("environments", Ty::Arr, "Environments"), b("services", Ty::Arr, "Services"), b("previews", Ty::Bool, "Previews' logs too"), b("headers", Ty::Obj, "Headers (all of them)")]),
    t("apps_delete_drain", "Delete a log drain", "Stop sending a project's logs there.", "apps", "DELETE", "/v1/apps/projects/{project}/drains/{did}", &[req("project", "Workspace project"), req("did", "Drain id")]),
    t("apps_test_drain", "Test a log drain", "Send one signed test line now (not retried): whether it arrived.", "apps", "POST", "/v1/apps/projects/{project}/drains/{did}/test", &[req("project", "Workspace project"), req("did", "Drain id")]),
    t("apps_pending", "What the next deploy changes", "Before deploying: which variables (names, never values), run settings and project secrets the next deploy changes against what's live.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/pending", &[SERVICE, ENV]),
    t("apps_deploy_commits", "A deploy's commits", "The commits a deploy brings over the one live before it, newest first (at most 50).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/commits", &[SERVICE, ENV, req("deploy", "Deploy id")]),
    t("apps_rollback", "Roll back a deployment", "Roll an environment back to an earlier deploy, with the variables it went live with (keep_variables: true keeps today's).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/rollback", &[SERVICE, ENV, req("deploy", "Deploy id"), b("keep_variables", Ty::Bool, "Keep the variables as they are now")]),
    t("apps_rollback_previous", "Roll back to the last deploy", "Put back the deploy that was live before this one, with its variables (keep_variables: true keeps today's).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/rollback", &[SERVICE, ENV, b("keep_variables", Ty::Bool, "Keep the variables as they are now")]),
    t("apps_logs", "Service logs", "Recent logs of a service's environment, or with http its requests (method, path, status, ms): the quickest way to find failing or slow ones.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/logs", &[
        SERVICE,
        ENV,
        q("since", Ty::Int, "From this time (unix ms)"),
        q("tail", Ty::Int, "Last lines (at most 2000)"),
        q("q", Ty::Str, "Search the last 7 days"),
        q("http", Ty::Bool, "The proxy's access log (one line per request) instead of the runtime's output"),
        q("status", Ty::Str, "With http: only these statuses (500, 5xx, 400-499)"),
        q("min_ms", Ty::Int, "With http: only requests that took at least this long"),
        q("before", Ty::Int, "Older lines: before this time (unix ms)"),
        q("instance", Ty::Str, "Only this instance's"),
        q("deployment", Ty::Str, "Only this deploy's"),
        q("stream", Ty::Str, "stdout, stderr, build or system"),
    ]),
    t("apps_list_domains", "List domains", "An environment's domains.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/domains", &[SERVICE, ENV]),
    t("apps_add_domain", "Add a domain", "Point a domain at an environment.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/domains", &[SERVICE, ENV, breq("hostname", Ty::Str, "Host name")]),
    t("apps_remove_domain", "Remove a domain", "Remove a domain.", "apps", "DELETE", "/v1/apps/services/{service}/environments/{env}/domains/{host}", &[SERVICE, ENV, req("host", "Host name")]),
    // ── previews (roll previews) ──
    t("previews_list", "List previews", "Preview environments with their counts and states (checks, visual review, pages); previews_get has one preview's issues, broken links and diffs.", "previews", "GET", "/v1/previews", &[q("project", Ty::Str, "Workspace project")]),
    t(
        "previews_get",
        "Get a preview",
        "A preview with its branched resources, and `schema_changes`: the database schema's +/- lines against what it was branched from (review a migration here).",
        "previews",
        "GET",
        "/v1/previews/{id}",
        &[req("id", "Preview id")],
    ),
    t(
        "previews_create",
        "Create a preview",
        "A preview environment for a change or a branch: the project's resources branched, its services deployed.",
        "previews",
        "POST",
        "/v1/previews",
        &[breq("project", Ty::Str, "Workspace project (prj_…)"), breq("repo", Ty::Str, "Repo in the project, owner/name"), b("branch", Ty::Str, "Branch"), b("change", Ty::Str, "Or a change (chg_…)")],
    ),
    t("previews_refresh", "Refresh a preview", "Redeploy a preview now.", "previews", "POST", "/v1/previews/{id}/refresh", &[req("id", "Preview id")]),
    t("previews_errors", "Preview errors", "A preview's errors over the last minutes (5-1440, default 60), grouped by service and fingerprint, with counts, first and last seen and a sample (masked as its logs are). Start here when a preview fails.", "previews", "GET", "/v1/previews/{id}/errors", &[req("id", "Preview id"), q("minutes", Ty::Int, "How far back, 5-1440 (default 60)")]),
    t("previews_mask_suggestions", "Unmasked personal data", "Columns in a project's databases and indices that look like people's data (emails, names, phone numbers…) and aren't masked in its previews yet, with the masking rule each would take and why; only resources this token can read.", "previews", "GET", "/v1/previews/projects/{project}/mask-suggestions", &[req("project", "Workspace project")]),
    t("previews_metrics", "Preview metrics", "Each of a preview's services' CPU and memory now: which one is slow or about to run out.", "previews", "GET", "/v1/previews/{id}/metrics", &[req("id", "Preview id")]),
    t("previews_logs", "Preview logs", "A preview's deploy and service logs.", "previews", "GET", "/v1/previews/{id}/logs", &[req("id", "Preview id")]),
    t("previews_requests", "A preview's requests", "A preview's latest 200 requests through roll, newest first: service, method, path (never its query), status and time. Start here to find the request that failed.", "previews", "GET", "/v1/previews/{id}/requests", &[req("id", "Preview id")]),
    t(
        "previews_reset",
        "Reset a preview",
        "Branch a preview's resources again from their parents, dropping what changed in it (all of them, or some).",
        "previews",
        "POST",
        "/v1/previews/{id}/reset",
        &[req("id", "Preview id"), b("resources", Ty::Arr, "Only these, as \"<kind>:<parent>\"")],
    ),
    t("previews_keep", "Keep a preview", "Keep a preview past its time to live, for some hours (0 lets it go; a landed or closed change still ends it).", "previews", "POST", "/v1/previews/{id}/keep", &[req("id", "Preview id"), breq("hours", Ty::Int, "0 to 720")]),
    t("previews_activity", "Preview activity", "What happened to a preview, newest first.", "previews", "GET", "/v1/previews/{id}/activity", &[req("id", "Preview id")]),
    t("previews_sleep", "Sleep a preview", "Stop a preview's services until it's woken (its data stays).", "previews", "POST", "/v1/previews/{id}/sleep", &[req("id", "Preview id")]),
    t("previews_wake", "Wake a preview", "Start a sleeping preview's services again.", "previews", "POST", "/v1/previews/{id}/wake", &[req("id", "Preview id")]),
    t("previews_usage", "Previews usage", "What a project's previews used and cost this month, against its budget.", "previews", "GET", "/v1/previews/projects/{project}/usage", &[req("project", "Workspace project")]),
    t("previews_list_shares", "List a preview's share links", "Who a preview is shared with outside the team, and until when (never the links' tokens).", "previews", "GET", "/v1/previews/{id}/shares", &[req("id", "Preview id")]),
    t("previews_env", "A preview's variables", "A preview's own variables, with their values (reveal).", "previews", "GET", "/v1/previews/{id}/env", &[req("id", "Preview id")]),
    t("previews_connect", "A preview's data connections", "Each data branch's connection as the preview's services get it (DATABASE_URL, REDIS_URL, a branch bucket's keys): the branches' own credentials, never production's. For querying what a preview sees.", "previews", "GET", "/v1/previews/{id}/connect", &[req("id", "Preview id")]),
    t("previews_list_mail", "A preview's mail", "The mail a preview's services sent, caught and never delivered: who to, subject, when (for checking a sign-up or receipt flow). It can hold production's data: reveal.", "previews", "GET", "/v1/previews/{id}/mail", &[req("id", "Preview id")]),
    t("previews_read_mail", "Read a preview's mail", "One caught message as text.", "previews", "GET", "/v1/previews/{id}/mail/{mail}", &[req("id", "Preview id"), req("mail", "Message id from previews_list_mail")]),
    t("previews_list_notes", "A preview's notes", "Notes people left on a preview's pages through its feedback button and nobody has resolved yet: page, element, what they said, who, when (the latest 500).", "previews", "GET", "/v1/previews/{id}/notes", &[req("id", "Preview id")]),
    t("previews_resolve_note", "Resolve a preview note", "Mark a note left on a preview page's feedback button as dealt with, so the page stops listing it (after fixing what it asked).", "previews", "POST", "/v1/previews/{id}/notes/{note}/resolve", &[req("id", "Preview id"), req("note", "Note id from previews_list_notes")]),
    t("previews_screenshot", "Screenshot a preview", "A preview's page as an image the agent can look at; diff against the base or the previous deploy, another page, or the phone view.", "previews", "GET", "/v1/previews/{id}/screenshot", &[req("id", "Preview id"), q("diff", Ty::Str, "base or previous: highlight what changed"), q("page", Ty::Str, "Path of the page (default /)"), q("view", Ty::Str, "phone for a phone-sized view")]),
    t("previews_share", "Share a preview", "A link for someone outside the team to open a preview for some hours (its token is in this answer only; anyone with the link gets in).", "previews", "POST", "/v1/previews/{id}/shares", &[req("id", "Preview id"), breq("hours", Ty::Int, "How long the link works")]),
    t("previews_revoke_share", "Revoke a share link", "Stop a preview's share link at once (who it's shared with: previews_list_shares).", "previews", "DELETE", "/v1/previews/{id}/shares/{share}", &[req("id", "Preview id"), req("share", "Share id")]),
    t("previews_settings", "Preview settings", "A project's preview rules: which branches get one, how long they live, private or not, budgets (changing them is the project admin's).", "previews", "GET", "/v1/previews/projects/{project}/settings", &[req("project", "Workspace project")]),
    t("previews_delete", "Delete a preview", "Tear a preview down with its branches.", "previews", "DELETE", "/v1/previews/{id}", &[req("id", "Preview id")]),
];

pub fn all() -> &'static [Tool] {
    TOOLS
}

/// What each tool's route is, worked out once: matching a path against every catalogue row, for
/// every tool, on every `tools/list`, cost ~43 ms a call with every product on.
struct Prepared {
    actions: Vec<Action>,
    open: bool,
    definition: Value,
}

static PREPARED: std::sync::LazyLock<std::collections::HashMap<&'static str, Prepared>> = std::sync::LazyLock::new(|| {
    TOOLS.iter().map(|t| (t.name, Prepared { actions: t.find_actions(), open: t.find_open(), definition: t.build_definition() })).collect()
});

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool calls a route the permission catalogue classifies, tool names are unique and
    /// fit every client (`^[a-zA-Z0-9_-]{1,64}$`), path parameters are all declared, and the hints
    /// follow the catalogue.
    #[test]
    fn tools_map_onto_classified_routes() {
        let mut names = std::collections::HashSet::new();
        for t in all() {
            assert!(names.insert(t.name), "{} twice", t.name);
            assert!(t.name.len() <= 64 && t.name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-'), "{}", t.name);
            assert!(!t.actions().is_empty(), "{} {} {} isn't in the catalogue", t.name, t.method, t.path);
            for seg in t.path.split('/').filter(|s| s.starts_with('{')) {
                let name = &seg[1..seg.len() - 1];
                assert!(t.params.iter().any(|p| p.name == name && p.at == At::Path), "{}: {{{name}}} isn't a path parameter", t.name);
            }
            assert!(t.name.starts_with(t.product), "{} isn't named after {}", t.name, t.product);
        }
        let def = |n: &str| all().iter().find(|t| t.name == n).unwrap().definition();
        assert_eq!(def("postgres_query")["annotations"]["readOnlyHint"], true);
        assert_eq!(def("storage_presign")["annotations"]["readOnlyHint"], false, "a presigned PUT writes");
        assert_eq!(def("secrets_reveal")["annotations"]["readOnlyHint"], true);
        assert_eq!(def("postgres_execute")["annotations"]["destructiveHint"], true);
        assert_eq!(def("postgres_delete_project")["annotations"]["destructiveHint"], true);
        assert_eq!(def("postgres_create_branch")["annotations"]["readOnlyHint"], false);
        assert_eq!(def("apps_deploy")["annotations"]["destructiveHint"], true);
        let r = all().iter().find(|t| t.name == "storage_write_object").unwrap().request(&json!({ "bucket": "a b", "key": "x/y.txt", "content": "hi" })).unwrap();
        assert_eq!(r.uri, "/v1/storage/buckets/a%20b/upload?key=x%2Fy.txt");
        assert!(matches!(r.body, Built::Raw(ref b, _) if b == b"hi"));
        assert!(all()[0].request(&json!({ "nope": 1 })).is_err());
        // Explain plans the agent's statement in a read-only transaction, and runs nothing.
        let r = all().iter().find(|t| t.name == "postgres_explain").unwrap().request(&json!({ "endpoint": "ep_1", "sql": "select * from users;" })).unwrap();
        assert!(matches!(r.body, Built::Json(ref b) if b["sql"] == "EXPLAIN (FORMAT JSON) select * from users" && b["read_only"] == true));
        for n in ["postgres_explain", "postgres_slow_queries", "postgres_advisors", "postgres_list_extensions"] {
            assert_eq!(def(n)["annotations"]["readOnlyHint"], true, "{n}");
        }
        // docs/mcp.md lists every tool.
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/mcp.md")).unwrap();
        let missing: Vec<_> = all().iter().filter(|t| !doc.contains(&format!("| `{}` |", t.name))).map(|t| t.name).collect();
        assert!(missing.is_empty(), "docs/mcp.md doesn't list {missing:?}");
        // A tool a description or a workflow tells the agent to call is one roll has: a renamed
        // tool would otherwise send agents after a name that answers "unknown tool".
        let names: Vec<String> = all().iter().map(|t| t.name.to_string()).chain([crate::search::FIND, crate::search::RUN_READ, crate::search::RUN].map(String::from)).collect();
        let products = ["access", "workspace", "postgres", "redis", "storage", "streams", "tasks", "queues", "nosql", "search", "secrets", "apps", "previews", "roll"];
        // Words that look like a tool and aren't one: Elasticsearch's audit event names.
        let not_tools = ["access_denied"];
        let mut texts: Vec<(String, String)> = all().iter().map(|t| (t.name.to_string(), std::iter::once(t.title).chain([t.description]).chain(t.params.iter().map(|p| p.desc)).collect::<Vec<_>>().join(" "))).collect();
        texts.extend(crate::prompts::workflows(&names).into_iter().map(|w| (format!("prompt {}", w.name), w.text)));
        let mut dangling = Vec::new();
        for (whose, text) in &texts {
            for w in text.split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')) {
                let looks = w.split_once('_').is_some_and(|(p, rest)| products.contains(&p) && !rest.is_empty() && !rest.starts_with('_'));
                if looks && !names.iter().any(|n| n == w) && !not_tools.contains(&w) {
                    dangling.push(format!("{whose}: {w}"));
                }
            }
        }
        dangling.dedup();
        assert!(dangling.is_empty(), "these name tools roll doesn't have: {dangling:?}");
    }
}
