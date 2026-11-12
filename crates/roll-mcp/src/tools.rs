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
    /// fixed body makes it, or anything it could be).
    pub fn actions(&self) -> Vec<Action> {
        let Some(found) = catalog::by_path(self.method, &self.sample_path()) else { return Vec::new() };
        match found.row.act {
            catalog::Act::Body(_) => match self.fixed {
                Some(f) => vec![found.row.action(self.method, &[], Some(&f()))],
                None => vec![Action::Read, Action::Write, Action::Delete],
            },
            catalog::Act::ProductionIf(_) => vec![Action::Write, Action::Deploy],
            _ => vec![found.row.action(self.method, &[], None)],
        }
    }

    /// A route any connection may call (its own identity, its own approvals).
    pub fn open(&self) -> bool {
        catalog::by_path(self.method, &self.sample_path()).is_some_and(|f| matches!(f.row.target, catalog::Tgt::AnyGrant))
    }

    pub fn definition(&self) -> Value {
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
        let actions = self.actions();
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
        let mut raw: Option<Vec<u8>> = None;
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
                At::RawText => raw = Some(as_text(v).into_bytes()),
                At::RawB64 => {
                    use base64::Engine;
                    raw = Some(base64::engine::general_purpose::STANDARD.decode(as_text(v).trim()).map_err(|_| format!("{} isn't base64", p.name))?);
                }
            }
        }
        let q = query.finish();
        let uri = if has_query { format!("{path}?{q}") } else { path };
        let body = match raw {
            Some(b) => Built::Raw(b, "application/octet-stream"),
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
    t("search_list_indices", "List indices", "A cluster's indices.", "search", "GET", "/v1/search/clusters/{cluster}/indices", &[req("cluster", "Cluster id or name")]),
    t("search_query", "Search", "Run an Elasticsearch _search body on an index.", "search", "POST", "/v1/search/clusters/{cluster}/indices/{index}/search", &[req("cluster", "Cluster id or name"), req("index", "Index"), b("query", Ty::Obj, "Query DSL"), b("size", Ty::Int, "Hits"), b("from", Ty::Int, "Offset"), b("sort", Ty::Arr, "Sort"), b("aggs", Ty::Obj, "Aggregations")]),
    t("search_get_document", "Get a document", "A document by id.", "search", "GET", "/v1/search/clusters/{cluster}/indices/{index}/docs/{doc}", &[req("cluster", "Cluster"), req("index", "Index"), req("doc", "Document id")]),
    t("search_index_document", "Index a document", "Create or replace a document.", "search", "PUT", "/v1/search/clusters/{cluster}/indices/{index}/docs/{doc}", &[req("cluster", "Cluster"), req("index", "Index"), req("doc", "Document id"), P { name: "document", ty: Ty::Str, at: At::RawText, required: true, desc: "The document, as JSON text" }]),
    t("search_delete_document", "Delete a document", "Delete a document.", "search", "DELETE", "/v1/search/clusters/{cluster}/indices/{index}/docs/{doc}", &[req("cluster", "Cluster"), req("index", "Index"), req("doc", "Document id")]),
    // ── secrets (roll secrets) ──
    t("secrets_list_environments", "List environments", "A project's secret environments.", "secrets", "GET", "/v1/secrets/projects/{project}/environments", &[SECRET_PROJECT]),
    t("secrets_list", "List secrets", "Secret names and metadata in an environment (never values).", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/secrets", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder, e.g. /api")]),
    t("secrets_reveal", "Reveal a secret", "A secret's value.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/value", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder")]),
    t("secrets_set", "Set a secret", "Create or change a secret's value.", "secrets", "PUT", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}", &[SECRET_PROJECT, ENV, req("name", "Secret name"), breq("value", Ty::Str, "The value"), q("path", Ty::Str, "Folder")]),
    t("secrets_delete", "Delete a secret", "Delete a secret.", "secrets", "DELETE", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder")]),
    // ── apps (roll apps) ──
    t("apps_list_services", "List services", "App services.", "apps", "GET", "/v1/apps/services", &[]),
    t("apps_get_service", "Get a service", "A service with its environments and deploys.", "apps", "GET", "/v1/apps/services/{service}", &[SERVICE]),
    t("apps_deploy", "Deploy", "Deploy a service to an environment (production deploys are \"deploy\" actions).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/deploys", &[SERVICE, ENV, b("ref", Ty::Str, "Revision to deploy")]),
    t("apps_rollback", "Roll back", "Roll an environment back to an earlier deploy.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/rollback", &[SERVICE, ENV, req("deploy", "Deploy id")]),
    t("apps_logs", "Service logs", "Recent logs of a service's environment.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/logs", &[SERVICE, ENV, q("since", Ty::Str, "e.g. 1h"), q("limit", Ty::Int, "Lines")]),
    t("apps_list_domains", "List domains", "An environment's domains.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/domains", &[SERVICE, ENV]),
    t("apps_add_domain", "Add a domain", "Point a domain at an environment.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/domains", &[SERVICE, ENV, breq("host", Ty::Str, "Host name")]),
    t("apps_remove_domain", "Remove a domain", "Remove a domain.", "apps", "DELETE", "/v1/apps/services/{service}/environments/{env}/domains/{host}", &[SERVICE, ENV, req("host", "Host name")]),
    // ── previews (roll previews) ──
    t("previews_list", "List previews", "Preview environments.", "previews", "GET", "/v1/previews", &[q("project", Ty::Str, "Workspace project")]),
    t("previews_get", "Get a preview", "A preview with its branched resources.", "previews", "GET", "/v1/previews/{id}", &[req("id", "Preview id")]),
    t("previews_create", "Create a preview", "A preview environment: every resource of a project branched.", "previews", "POST", "/v1/previews", &[breq("project", Ty::Str, "Workspace project (prj_…)"), b("change", Ty::Str, "roll code change or PR it's for"), b("name", Ty::Str, "Name")]),
    t("previews_delete", "Delete a preview", "Tear a preview down with its branches.", "previews", "DELETE", "/v1/previews/{id}", &[req("id", "Preview id")]),
];

pub fn all() -> &'static [Tool] {
    TOOLS
}

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
    }
}
