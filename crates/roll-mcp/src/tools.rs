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
    pub hint: Hint,
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
            };
            props.insert(p.name.into(), json!({ "type": ty, "description": p.desc }));
            if p.required {
                required.push(p.name);
            }
        }
        props.insert(
            "approval".into(),
            json!({ "type": "string", "description": "An approval id (apr_…) from an earlier call that needed the user's approval: waits for it, and runs once it's approved" }),
        );
        let actions = self.find_actions();
        let read_only = !actions.is_empty() && actions.iter().all(|a| matches!(a, Action::Read | Action::Reveal));
        let destructive = self.hint == Hint::Destructive || actions.iter().any(|a| matches!(a, Action::Delete | Action::Deploy));
        let idempotent = read_only || self.method == "PUT" || self.method == "DELETE";
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": { "type": "object", "properties": props, "required": required, "additionalProperties": false },
            "annotations": { "title": self.title, "readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": idempotent, "openWorldHint": false },
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
    Tool { name, title, description, product, method, path, params, fixed: None, hint: Hint::Auto }
}

impl Tool {
    const fn fixed(mut self, f: fn() -> Value) -> Tool {
        self.fixed = Some(f);
        self
    }
    const fn destructive(mut self) -> Tool {
        self.hint = Hint::Destructive;
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
    t("access_wait_approval", "Wait for an approval", "Wait (up to 55 s) for a person to decide an approval this connection asked for; then call the original tool again with approval.", "access", "GET", "/v1/access/approvals/{approval_id}", &[req("approval_id", "The approval id (apr_…)"), q("wait", Ty::Int, "Seconds to wait (at most 55; default 55)")]),
    t(
        "access_check",
        "Check before acting",
        "Whether this connection may make a request (allow, ask or deny), and as which action on what, without making it.",
        "access",
        "POST",
        "/v1/access/check",
        &[breq("method", Ty::Str, "HTTP method, e.g. DELETE"), breq("path", Ty::Str, "The API path, e.g. /v1/projects/prj_x"), b("body", Ty::Obj, "The JSON body, for SQL, Redis or CQL")],
    ),
    t("access_list_approvals", "List approvals", "Approvals, newest first: pending ones wait for the operator.", "access", "GET", "/v1/access/approvals", &[q("state", Ty::Str, "pending, approved, denied, used")]),
    t("access_audit_log", "Audit log", "Every request made with a token and every MCP tool call, newest first.", "access", "GET", "/v1/access/audit", &[q("grant", Ty::Str, "Only this token (tok_…)"), q("product", Ty::Str, "Only this product"), q("decision", Ty::Str, "allow, deny or ask"), q("limit", Ty::Int, "At most (default 100)")]),
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
    t("postgres_query", "Query Postgres (read-only)", "Run read-only SQL on an endpoint: it runs in a read-only transaction, so writes fail. Returns columns and rows.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT, breq("sql", Ty::Str, "SQL to run")]).fixed(read_only),
    t("postgres_execute", "Execute SQL", "Run SQL that may change data or schema (INSERT, UPDATE, DDL) on an endpoint.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT, breq("sql", Ty::Str, "SQL to run")]).fixed(writes).destructive(),
    t("postgres_describe_schema", "Describe the schema", "Every table's columns and types on an endpoint (read-only).", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(describe_schema),
    t("postgres_start_endpoint", "Start an endpoint", "Wake an endpoint's compute.", "postgres", "POST", "/v1/endpoints/{endpoint}/start", &[ENDPOINT]),
    t("postgres_suspend_endpoint", "Suspend an endpoint", "Suspend an endpoint's compute.", "postgres", "POST", "/v1/endpoints/{endpoint}/suspend", &[ENDPOINT]),
    t("postgres_usage", "Postgres usage", "Compute and storage use of a project, with cost estimates.", "postgres", "GET", "/v1/projects/{project}/usage", &[PROJECT, DAYS]),
    // ── Redis ──
    t("redis_list_databases", "List Redis databases", "Serverless Redis databases.", "redis", "GET", "/v1/redis/databases", &[]),
    t("redis_get_database", "Get a Redis database", "A Redis database's settings and connection (credentials need reveal).", "redis", "GET", "/v1/redis/databases/{database}", &[REDIS]),
    t("redis_create_database", "Create a Redis database", "A new serverless Redis database.", "redis", "POST", "/v1/redis/databases", &[breq("name", Ty::Str, "Database name"), b("max_memory_mb", Ty::Int, "Memory limit"), b("eviction_policy", Ty::Str, "Eviction policy"), IN_PROJECT]),
    t("redis_delete_database", "Delete a Redis database", "Delete a database and its data. Irreversible.", "redis", "DELETE", "/v1/redis/databases/{database}", &[REDIS]),
    t("redis_create_branch", "Branch a Redis database", "A fork of a database's data, now or at a point in time.", "redis", "POST", "/v1/redis/databases/{database}/branches", &[REDIS, breq("name", Ty::Str, "Branch name"), b("at", Ty::Int, "Unix seconds")]),
    t("redis_command", "Run a Redis command", "Run one command on a database, e.g. [\"GET\", \"key\"]. Read-only commands are reads; FLUSHDB and FLUSHALL delete.", "redis", "POST", "/v1/redis/databases/{database}/command", &[REDIS, breq("command", Ty::Arr, "The command and its arguments")]),
    t("redis_usage", "Redis usage", "A database's usage report.", "redis", "GET", "/v1/redis/databases/{database}/usage", &[REDIS, DAYS]),
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
        "Write an object",
        "Upload an object (replacing the key): content as text, or content_b64.",
        "storage",
        "PUT",
        "/v1/storage/buckets/{bucket}/upload",
        &[BUCKET, qreq("key", "Object key"), P { name: "content", ty: Ty::Str, at: At::RawText, required: false, desc: "Text content" }, P { name: "content_b64", ty: Ty::Str, at: At::RawB64, required: false, desc: "Binary content, base64" }],
    ),
    t("storage_delete_object", "Delete an object", "Delete an object (to the trash; with version_id, that version for good).", "storage", "DELETE", "/v1/storage/buckets/{bucket}/object", &[BUCKET, qreq("key", "Object key"), q("version_id", Ty::Str, "A version")]),
    t("storage_presign", "Presign a URL", "A URL anyone can use to get (or put) an object until it expires.", "storage", "POST", "/v1/storage/buckets/{bucket}/presign", &[BUCKET, breq("key", Ty::Str, "Object key"), b("method", Ty::Str, "GET or PUT"), b("expires_s", Ty::Int, "Seconds valid")]),
    t("storage_usage", "Storage usage", "A bucket's usage.", "storage", "GET", "/v1/storage/buckets/{bucket}/usage", &[BUCKET, DAYS]),
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
    t("streams_usage", "Stream usage", "A stream's usage and throughput.", "streams", "GET", "/v1/streams/{stream}/usage", &[STREAM, q("hours", Ty::Int, "Hours of history")]),
    // ── NoSQL ──
    t("nosql_list_databases", "List NoSQL databases", "ScyllaDB/Cassandra-compatible databases.", "nosql", "GET", "/v1/nosql/databases", &[]),
    t("nosql_get_database", "Get a NoSQL database", "A database's settings and endpoints (credentials need reveal).", "nosql", "GET", "/v1/nosql/databases/{database}", &[NOSQL]),
    t("nosql_create_database", "Create a NoSQL database", "A new database.", "nosql", "POST", "/v1/nosql/databases", &[breq("name", Ty::Str, "Database name"), b("dialect", Ty::Str, "scylla or cassandra"), IN_PROJECT]),
    t("nosql_delete_database", "Delete a NoSQL database", "Delete a database and its data. Irreversible.", "nosql", "DELETE", "/v1/nosql/databases/{database}", &[NOSQL]),
    t("nosql_create_branch", "Branch a NoSQL database", "A branch database, now or at a point in time.", "nosql", "POST", "/v1/nosql/databases/{database}/branches", &[NOSQL, breq("name", Ty::Str, "Branch name"), b("at", Ty::Str, "Point in time"), b("schema_only", Ty::Bool, "Schema without data")]),
    t("nosql_cql", "Run CQL", "Run a CQL statement. SELECT reads; DROP and TRUNCATE delete; the rest write.", "nosql", "POST", "/v1/nosql/databases/{database}/cql", &[NOSQL, breq("query", Ty::Str, "CQL"), b("keyspace", Ty::Str, "Keyspace"), b("page_state", Ty::Str, "From the last page")]),
    t("nosql_schema", "NoSQL schema", "Keyspaces, tables and their columns.", "nosql", "GET", "/v1/nosql/databases/{database}/schema", &[NOSQL]),
    t("nosql_usage", "NoSQL usage", "A database's usage report.", "nosql", "GET", "/v1/nosql/databases/{database}/usage", &[NOSQL, DAYS]),
    // ── queues ──
    t("queues_overview", "Queues overview", "The broker's totals and listeners.", "queues", "GET", "/v1/queues/overview", &[]),
    t("queues_list_vhosts", "List virtual hosts", "RabbitMQ-compatible virtual hosts.", "queues", "GET", "/v1/queues/vhosts", &[]),
    t("queues_create_vhost", "Create a virtual host", "A new virtual host.", "queues", "POST", "/v1/queues/vhosts", &[breq("name", Ty::Str, "Name"), b("description", Ty::Str, "Description"), IN_PROJECT]),
    t("queues_delete_vhost", "Delete a virtual host", "Delete a virtual host and everything in it.", "queues", "DELETE", "/v1/queues/vhosts/{vhost}", &[VHOST]),
    t("queues_list_queues", "List queues", "A virtual host's queues with message counts.", "queues", "GET", "/v1/queues/vhosts/{vhost}/queues", &[VHOST]),
    t("queues_declare_queue", "Declare a queue", "Create a queue.", "queues", "POST", "/v1/queues/vhosts/{vhost}/queues", &[VHOST, breq("name", Ty::Str, "Queue name"), b("type", Ty::Str, "classic, quorum or stream"), b("durable", Ty::Bool, "Durable")]),
    t("queues_delete_queue", "Delete a queue", "Delete a queue and its messages.", "queues", "DELETE", "/v1/queues/vhosts/{vhost}/queues/{queue}", &[VHOST, QUEUE]),
    t("queues_purge_queue", "Purge a queue", "Drop every message in a queue.", "queues", "POST", "/v1/queues/vhosts/{vhost}/queues/{queue}/purge", &[VHOST, QUEUE]),
    t("queues_peek", "Peek messages", "Messages in a queue, without consuming them.", "queues", "GET", "/v1/queues/vhosts/{vhost}/queues/{queue}/messages", &[VHOST, QUEUE, q("count", Ty::Int, "At most 500")]),
    t("queues_publish", "Publish a message", "Publish to an exchange with a routing key.", "queues", "POST", "/v1/queues/vhosts/{vhost}/publish", &[VHOST, breq("exchange", Ty::Str, "Exchange (\"\" for the default)"), breq("routing_key", Ty::Str, "Routing key"), breq("body", Ty::Str, "Message body")]),
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
    t("tasks_stats", "Tasks stats", "Nodes, queue stats, dead letters, schedules and functions.", "tasks", "GET", "/v1/tasks/stats", &[]),
    // ── search (roll search) ──
    t("search_list_clusters", "List search clusters", "Elasticsearch-compatible search clusters.", "search", "GET", "/v1/search/clusters", &[]),
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
    t("search_branch_cluster", "Branch a search cluster", "A copy-on-write branch of a cluster, now or at a time within its history.", "search", "POST", "/v1/search/clusters/{cluster}/branches", &[req("cluster", "Cluster id or name"), breq("name", Ty::Str, "Branch name"), b("at", Ty::Int, "Unix seconds")]),
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
    t("secrets_reveal", "Reveal a secret", "A secret's value.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/value", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder")]),
    t("secrets_set", "Set a secret", "Create or change a secret's value.", "secrets", "PUT", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}", &[SECRET_PROJECT, ENV, req("name", "Secret name"), breq("value", Ty::Str, "The value"), q("path", Ty::Str, "Folder")]),
    t("secrets_delete", "Delete a secret", "Delete a secret.", "secrets", "DELETE", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder")]),
    t("secrets_health", "Secrets health", "What needs fixing in a project's secrets: expired, missing, exposed, weak, expiring, stale or unread ones, by name (never values), in the environments you may read.", "secrets", "GET", "/v1/secrets/projects/{project}/health", &[
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
    t("apps_list_services", "List services", "App services.", "apps", "GET", "/v1/apps/services", &[]),
    t(
        "apps_create_service",
        "Create a service",
        "A new service in a workspace project, from a roll code repo ({kind: \"repo\", repo: \"owner/name\", root?}), an image ({kind: \"image\", image}) or an upload ({kind: \"upload\"}, deployed with roll up). A repo or image service deploys to production at once (a \"deploy\" action).",
        "apps",
        "POST",
        "/v1/apps/projects/{project}/services",
        &[req("project", "Workspace project"), breq("name", Ty::Str, "Service name"), breq("source", Ty::Obj, "Where the code comes from"), b("variables", Ty::Obj, "{NAME: value}, set before the first deploy")],
    ),
    t("apps_list_environments", "List environments", "A service's environments.", "apps", "GET", "/v1/apps/services/{service}/environments", &[SERVICE]),
    t("apps_get_service", "Get a service", "A service with its environments' summaries (apps_list_deploys lists an environment's deploys).", "apps", "GET", "/v1/apps/services/{service}", &[SERVICE]),
    t("apps_set_environment", "Set up an environment", "Create an environment or change one: the branch it deploys, its own run settings, following an image tag, waiting for CI. Production changes are \"deploy\" actions; copying another environment's variables needs reveal there.", "apps", "PUT", "/v1/apps/services/{service}/environments/{env}", &[
        SERVICE,
        ENV,
        b("branch", Ty::Str, "The git branch it deploys"),
        b("ephemeral", Ty::Bool, "A preview environment"),
        b("run", Ty::Obj, "Its own run settings (min/max instances, cpu, memory, concurrency, suspend, sleep)"),
        b("follow_tag", Ty::Bool, "An image service: redeploy when its tag points at a new digest"),
        b("wait_for_ci", Ty::Bool, "Deploy a commit only once its checks pass"),
        b("variables_from", Ty::Str, "Creating it: start with this environment's variables"),
    ]),
    t("apps_update_service", "Update a service", "Change a service's name, source, build or run settings (fields given are changed; run takes alerts {memory_pct, error_pct, p95_ms} for metric alerts).", "apps", "PATCH", "/v1/apps/services/{service}", &[
        SERVICE,
        b("name", Ty::Str, "New name"),
        b("source", Ty::Obj, "Where it's built from (repo or image), as apps_get_service shows it"),
        b("build", Ty::Obj, "Build settings to change"),
        b("run", Ty::Obj, "Run settings to change (instances, memory, start command, h2c, alerts {memory_pct, error_pct, p95_ms}; changing start, pre_deploy, schedule, kind, port, volume or public needs deploy, as source and build do; a worker's scale {stream, group, per_instance} needs read on that stream)"),
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
    t("apps_sleep", "Sleep", "Stop an environment's instances until a request or apps_wake wakes them.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/sleep", &[SERVICE, ENV]),
    t("apps_wake", "Wake", "Start a sleeping environment's instances.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/wake", &[SERVICE, ENV]),
    t("apps_usage", "Service usage", "An environment's usage and cost per day.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/usage", &[SERVICE, ENV, q("days", Ty::Int, "Days back")]),
    t("apps_restart", "Restart", "Restart an environment's instances.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/restart", &[SERVICE, ENV]),
    t("apps_redeploy", "Redeploy", "Deploy what's running again with the current settings and variables (production is a \"deploy\" action).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/redeploy", &[SERVICE, ENV, b("message", Ty::Str, "Why")]),
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
    t("apps_events", "App events", "What happened to services (deploys going live or failing, crash loops, failed cron runs and jobs, memory, error and latency alerts, and audit.* changes to settings, variables, domains and links), in order after a cursor.", "apps", "GET", "/v1/apps/events", &[q("after", Ty::Int, "The last answer's next"), q("limit", Ty::Int, "At most this many")]),
    t("apps_list_variables", "List variables", "An environment's variables, values masked.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/variables", &[SERVICE, ENV]),
    t(
        "apps_set_variables",
        "Set variables",
        "Set or unset an environment's variables (production is a \"deploy\" action).",
        "apps",
        "PATCH",
        "/v1/apps/services/{service}/environments/{env}/variables",
        &[SERVICE, ENV, b("set", Ty::Obj, "{NAME: value}"), b("unset", Ty::Arr, "Names to remove")],
    ),
    t("apps_metrics", "Service metrics", "CPU, memory and requests of an environment.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/metrics", &[SERVICE, ENV, q("minutes", Ty::Int, "Window")]),
    t("apps_rollback", "Roll back", "Roll an environment back to an earlier deploy, with the variables it went live with (keep_variables: true keeps today's).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/rollback", &[SERVICE, ENV, req("deploy", "Deploy id"), b("keep_variables", Ty::Bool, "Keep the variables as they are now")]),
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
    ]),
    t("apps_list_domains", "List domains", "An environment's domains.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/domains", &[SERVICE, ENV]),
    t("apps_add_domain", "Add a domain", "Point a domain at an environment.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/domains", &[SERVICE, ENV, breq("hostname", Ty::Str, "Host name")]),
    t("apps_remove_domain", "Remove a domain", "Remove a domain.", "apps", "DELETE", "/v1/apps/services/{service}/environments/{env}/domains/{host}", &[SERVICE, ENV, req("host", "Host name")]),
    // ── previews (roll previews) ──
    t("previews_list", "List previews", "Preview environments.", "previews", "GET", "/v1/previews", &[q("project", Ty::Str, "Workspace project")]),
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
    t("previews_logs", "Preview logs", "A preview's deploy and service logs.", "previews", "GET", "/v1/previews/{id}/logs", &[req("id", "Preview id")]),
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
    t("previews_share", "Share a preview", "A link for someone outside the team to open a preview for some hours (its token is in this answer only; anyone with the link gets in).", "previews", "POST", "/v1/previews/{id}/shares", &[req("id", "Preview id"), breq("hours", Ty::Int, "How long the link works")]),
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
        assert_eq!(def("postgres_execute")["annotations"]["destructiveHint"], true);
        assert_eq!(def("postgres_delete_project")["annotations"]["destructiveHint"], true);
        assert_eq!(def("postgres_create_branch")["annotations"]["readOnlyHint"], false);
        assert_eq!(def("apps_deploy")["annotations"]["destructiveHint"], true);
        let r = all().iter().find(|t| t.name == "storage_write_object").unwrap().request(&json!({ "bucket": "a b", "key": "x/y.txt", "content": "hi" })).unwrap();
        assert_eq!(r.uri, "/v1/storage/buckets/a%20b/upload?key=x%2Fy.txt");
        assert!(matches!(r.body, Built::Raw(ref b, _) if b == b"hi"));
        assert!(all()[0].request(&json!({ "nope": 1 })).is_err());
        // docs/mcp.md lists every tool.
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/mcp.md")).unwrap();
        let missing: Vec<_> = all().iter().filter(|t| !doc.contains(&format!("| `{}` |", t.name))).map(|t| t.name).collect();
        assert!(missing.is_empty(), "docs/mcp.md doesn't list {missing:?}");
    }
}
