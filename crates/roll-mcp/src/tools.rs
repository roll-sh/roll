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
    /// A body field the route clears with `null`: sent as `null` only when the call's `clear` names
    /// it (a `null` argument is left out, as any other: some clients fill every unused field with
    /// one). Left out, the route keeps what it has.
    BodyOrNull,
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
    /// It removes or replaces only what can be put back (a resource off a canvas, a deleted
    /// cluster brought back): never destructive, though its route is a `DELETE` or its name says so.
    Undoable,
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
    /// Fields of its answer that anyone outside can write (a site's form submissions): withheld in
    /// lockdown wherever they sit, as an outside record's text is (lockdown::withhold_fields).
    pub outside: &'static [&'static str],
    /// What it writes goes on a public page (a status page's incidents): the credential guard
    /// refuses what it would anywhere people read (`roll_auth::credentials`, `published`).
    pub published: bool,
    /// It makes roll reach outside it, to an address the caller chose (a task's URL, a webhook, a log
    /// drain, a Git URL or image, a vendor's API): MCP's `openWorldHint`, so a client can say so.
    pub open_world: bool,
    /// Sends no body when no body field is given, for a route that reads a missing body one way
    /// and `{}` another (a transit key's rotate: no body is roll's own new material).
    pub bodiless: bool,
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

/// What a path segment keeps encoded: everything but letters, digits and `-`, `_`, `~` (RFC 3986's
/// unreserved characters, as browsers and roll's CLI send them). An id's `_` sent as `%5F` reached
/// products that route by the raw path (apps' owner routing) as a different key: its service was
/// never "owned", so live actions answered 503. A `.` stays encoded, so `..` is never a dot-segment.
pub const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'~');

pub fn seg(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, SEGMENT).to_string()
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
    /// Which area of its product's toolset this tool is in (`?toolsets=apps.observe`).
    pub fn area(&self) -> Area {
        match self.product {
            "apps" => apps_area(self.method, self.path),
            "secrets" => secrets_area(self.method, self.path),
            _ => Area::Every,
        }
    }

    pub fn open(&self) -> bool {
        PREPARED.get(self.name).map_or_else(|| self.find_open(), |p| p.open)
    }

    /// Whether a person's role decides it too, on top of their token's rules: one resource of a
    /// member product, as roll-access's gate asks a person's token `within` their session (a list,
    /// a create or the product as a whole goes on the token's rules alone).
    pub fn within_session(&self) -> bool {
        PREPARED.get(self.name).map_or_else(|| self.find_within(), |p| p.within)
    }

    /// Its MCP definition, as `tools/list` shows it.
    pub fn definition(&self) -> Value {
        self.definition_ref().clone()
    }

    /// Its definition, worked out once: read it without copying (filtering, names).
    pub fn definition_ref(&self) -> &'static Value {
        &PREPARED[self.name].definition
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

    fn find_within(&self) -> bool {
        roll_auth::roles::MEMBER_PRODUCTS.contains(&self.product)
            && catalog::by_path(self.method, &self.sample_path()).is_some_and(|f| !matches!(f.row.target, catalog::Tgt::AnyGrant | catalog::Tgt::List { .. } | catalog::Tgt::Product | catalog::Tgt::Create { .. }))
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
        let clearable: Vec<&str> = self.params.iter().filter(|p| p.at == At::BodyOrNull).map(|p| p.name).collect();
        if !clearable.is_empty() {
            props.insert(
                "clear".into(),
                json!({ "type": "array", "items": { "type": "string", "enum": clearable }, "description": "Fields to set to none (each says what none is); a field left out, or null, stays as it is" }),
            );
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
        // A `DELETE` removes something whatever the catalogue calls its action (a drain, a lease,
        // a link are writes or admin there), unless the tool says it can be put back.
        let destructive = self.hint == Hint::Destructive
            || actions.iter().any(|a| matches!(a, Action::Delete | Action::Deploy))
            || (self.method == "DELETE" && self.hint != Hint::Undoable);
        let idempotent = read_only || self.method == "PUT" || self.method == "DELETE";
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": { "type": "object", "properties": props, "required": required, "additionalProperties": false },
            "annotations": { "readOnlyHint": read_only, "destructiveHint": destructive, "idempotentHint": idempotent, "openWorldHint": self.open_world },
            "_meta": { "roll/product": self.product, "roll/route": format!("{} {}", self.method, self.path), "roll/actions": actions },
        })
    }

    /// The request this call makes.
    pub fn request(&self, args: &Value) -> Result<Request, String> {
        let obj = args.as_object().cloned().unwrap_or_default();
        let clearable = self.params.iter().any(|p| p.at == At::BodyOrNull);
        for k in obj.keys() {
            if k != "approval" && !(k == "clear" && clearable) && !self.params.iter().any(|p| p.name == k) {
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
                At::Body | At::BodyOrNull => {
                    body[p.name] = v.clone();
                    body_used = true;
                }
                // A yes/no header is `1` when yes and left out when no (apps' `X-Roll-Override`
                // takes exactly `1`: `true` would be no override at all).
                At::Header(h) if p.ty == Ty::Bool => {
                    if v.as_bool() == Some(true) {
                        headers.push((h, "1".into()));
                    }
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
        // `clear`: the fields to set to none, sent as null. Only by name: a client that fills every
        // field it doesn't use with null changes nothing.
        if let Some(names) = obj.get("clear").filter(|v| !v.is_null()) {
            let names = names.as_array().ok_or_else(|| format!("{}: clear is a list of field names", self.name))?;
            for n in names {
                let n = n.as_str().unwrap_or_default();
                if !self.params.iter().any(|p| p.name == n && p.at == At::BodyOrNull) {
                    return Err(format!("{}: {n:?} can't be cleared", self.name));
                }
                if obj.get(n).is_some_and(|v| !v.is_null()) {
                    return Err(format!("{}: {n} is both set and cleared", self.name));
                }
                body[n] = Value::Null;
                body_used = true;
            }
        }
        let body = match raw {
            Some((b, ct)) => Built::Raw(b, ct),
            None if body_used || (!self.bodiless && matches!(self.method, "POST" | "PUT" | "PATCH")) => Built::Json(body),
            None => Built::None,
        };
        Ok(Request { uri, headers, body })
    }
}

/// The areas a product's toolset splits into, as GitHub's server splits its own: a client with a
/// tool limit (VS Code 128, Cursor about 40) takes the area it works in: apps (101 tools) and secrets
/// (94) near VS Code's with the access tools, far past Cursor's.
pub const AREAS: &[(&str, &[&str])] = &[("apps", &["observe", "deploy", "configure", "project"]), ("secrets", &["values", "security", "integrations"])];

/// Where a tool sits in its product's areas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Area {
    /// The product has no areas, or this is a lookup every area needs (a service, an environment).
    Every,
    One(&'static str),
    /// A route the table below doesn't place: a test fails until it does.
    Unplaced,
}

/// A secrets tool's area, from its route: the values themselves (keys, folders, environments and
/// what moves them), who reaches them and what's wrong (audit, findings, shares), or what roll
/// keeps for outside systems (transit, leases, clouds, CAs, syncs).
fn secrets_area(method: &str, path: &str) -> Area {
    let Some(rest) = path.strip_prefix("/v1/secrets") else { return Area::Unplaced };
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty() && !s.starts_with('{')).collect();
    let read = method == "GET";
    let area = match parts.as_slice() {
        // A project and its environments: every area starts there.
        ["projects"] | ["projects", "environments"] if read => return Area::Every,
        ["findings", ..] | ["deleted"] => "security",
        ["search", ..] => "values",
        ["projects", "environments", s, ..] => match *s {
            "secrets" | "changes" | "diff" | "folders" | "import" | "imports" | "exports" | "order" | "promote" | "rollback" | "requests" | "value-requests" | "values" => "values",
            "reach" | "insights" | "folder-policies" => "security",
            _ => return Area::Unplaced,
        },
        ["projects", "environments"] | ["projects"] => "values",
        ["projects", s, ..] => match *s {
            "grid" | "compare" | "required" | "shared" | "suggestions" | "links" | "value-requests" => "values",
            "audit" | "health" | "leaks" | "shares" | "rotations" | "backup" | "unstored" => "security",
            "transit" | "leases" | "cas" | "ssh-cas" | "atlas-projects" | "aws-roles" | "aws-users" | "cloud-accounts" | "directories" | "gateways" | "github-apps"
            | "kubernetes" | "outside-databases" | "rotators" | "saas" | "snowflake-accounts" | "syncs" => "integrations",
            "placeholders" => "security",
            _ => return Area::Unplaced,
        },
        _ => return Area::Unplaced,
    };
    Area::One(area)
}

/// An apps tool's area, from its route: what an agent does with it (watch, ship, set up, run the
/// project).
fn apps_area(method: &str, path: &str) -> Area {
    let Some(rest) = path.strip_prefix("/v1/apps") else { return Area::Unplaced };
    let parts: Vec<&str> = rest.split('/').filter(|s| !s.is_empty() && !s.starts_with('{')).collect();
    let read = method == "GET";
    let area = match parts.as_slice() {
        // Finding a service and its environments: every area starts there.
        [] | ["services"] | ["services", "environments"] if read => return Area::Every,
        ["projects", "services"] if read => return Area::Every,
        ["services"] | ["services", "environments"] | ["projects", "services"] => "configure",
        ["services", "environments", s, ..] => match *s {
            "analytics" | "app-metrics" | "errors" | "logs" | "metrics" | "routes" | "slo" | "traces" | "usage" | "vitals" | "push" => "observe",
            "deploys" | "promote" | "redeploy" | "rollback" | "pending" | "diff" | "jobs" | "runs" | "restart" | "sleep" | "wake" | "instances" | "cache" | "sandboxes" => "deploy",
            "private-network" => "configure",
            "variables" | "domains" | "forms" | "volume" | "config" => "configure",
            _ => return Area::Unplaced,
        },
        ["services", "activity"] | ["events"] => "observe",
        ["services", "links"] | ["services", "restore"] | ["import"] | ["plan"] => "configure",
        ["projects", "drains" | "notifications" | "limits" | "usage" | "status-page" | "preview-domain" | "export", ..] => "project",
        _ => return Area::Unplaced,
    };
    Area::One(area)
}

/// A workspace project's icon, as the dashboard shows it.
const ICON: &str = "{kind: monogram, text?: 1-3 characters} or {kind: emoji, emoji} or {kind: image, url: a data:image/png, jpeg or webp URL, at most 200 KiB}, with color?: slate, violet, purple, pink, rose, teal, cyan or green";
const fn req(name: &'static str, desc: &'static str) -> P {
    P { name, ty: Ty::Str, at: At::Path, required: true, desc }
}
/// A path parameter the route parses as a number (a change's seq, a version).
const fn req_int(name: &'static str, desc: &'static str) -> P {
    P { name, ty: Ty::Int, at: At::Path, required: true, desc }
}
const fn q(name: &'static str, ty: Ty, desc: &'static str) -> P {
    P { name, ty, at: At::Query, required: false, desc }
}
const fn b(name: &'static str, ty: Ty, desc: &'static str) -> P {
    P { name, ty, at: At::Body, required: false, desc }
}
/// A body field a call can clear by naming it in `clear` (`At::BodyOrNull`).
const fn bnull(name: &'static str, ty: Ty, desc: &'static str) -> P {
    P { name, ty, at: At::BodyOrNull, required: false, desc }
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
/// Past an environment's deploy freeze: an admin's call (roll-access makes the request `admin` and
/// binds the header into its approval), audited by roll apps as `freeze.overridden`.
const OVERRIDE: P = P { name: "override", ty: Ty::Bool, at: At::Header("x-roll-override"), required: false, desc: "Go past the environment's deploy freeze: an admin's call (a token that may only deploy can't), audited" };
const IN_PROJECT: P = P { name: "workspace_project", ty: Ty::Str, at: At::Header("x-roll-project"), required: false, desc: "Workspace project (prj_…) to put the new resource in" };

const fn t(name: &'static str, title: &'static str, description: &'static str, product: &'static str, method: &'static str, path: &'static str, params: &'static [P]) -> Tool {
    Tool { name, title, description, product, method, path, params, fixed: None, wrap: None, hint: Hint::Auto, until: None, outside: &[], published: false, open_world: false, bodiless: false }
}

/// A value request takes one answer and lasts a day unless asked otherwise: the link is a way to
/// write those keys (the API's own default is a week).
fn value_request_defaults() -> Value {
    json!({ "ttl_minutes": 1440, "answers": 1 })
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

/// A preview that's done changing (roll previews' `ready`, `sleeping`, `failed`, `deleting`), or
/// waits on a person (`awaiting_approval`: a fork's change runs only after `approve-run`); `queued`,
/// `creating` and `updating` aren't.
fn preview_settled(v: &Value) -> bool {
    matches!(v["state"].as_str(), Some("ready" | "sleeping" | "failed" | "deleting" | "awaiting_approval"))
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
    /// It removes or replaces only what can be put back ([`Hint::Undoable`]).
    const fn undoable(mut self) -> Tool {
        self.hint = Hint::Undoable;
        self
    }
    const fn until(mut self, f: fn(&Value) -> bool) -> Tool {
        self.until = Some(f);
        self
    }
    const fn published(mut self) -> Tool {
        self.published = true;
        self
    }
    /// No body unless a body field is given ([`Tool::bodiless`]).
    const fn bodiless(mut self) -> Tool {
        self.bodiless = true;
        self
    }

    const fn open_world(mut self) -> Tool {
        self.open_world = true;
        self
    }
    const fn outside(mut self, fields: &'static [&'static str]) -> Tool {
        self.outside = fields;
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

/// Every client session on the database now, blocked ones first (Neon's and PlanetScale's live
/// connections): what it runs, waits on and is blocked by, and how long its transaction has been
/// open. Read-only (the read-only role has `pg_read_all_stats`).
fn activity() -> Value {
    json!({ "read_only": true, "sql": r#"SELECT pid, usename AS "user", application_name AS app, client_addr::text AS client, state,
  wait_event_type || coalesce(':' || wait_event, '') AS waiting_on, pg_blocking_pids(pid) AS blocked_by,
  round(extract(epoch FROM now() - xact_start)::numeric, 1) AS transaction_s, round(extract(epoch FROM now() - query_start)::numeric, 1) AS query_s,
  left(query, 300) AS query
FROM pg_stat_activity
WHERE datname = current_database() AND pid <> pg_backend_pid() AND backend_type = 'client backend'
ORDER BY cardinality(pg_blocking_pids(pid)) DESC, xact_start NULLS LAST"# })
}

/// A database's health in one read (Neon's `inspect_database`): each check's value, `ok` or
/// `warn`, and what it means.
fn health_checks() -> Value {
    json!({ "read_only": true, "sql": r#"SELECT name AS "check", value, status, detail FROM (
  SELECT 1 AS n, 'cache hit rate' AS name, coalesce(round(100.0 * sum(blks_hit) / nullif(sum(blks_hit) + sum(blks_read), 0), 1)::text || '%', 'no reads yet') AS value,
    CASE WHEN sum(blks_hit) + sum(blks_read) < 10000 OR 100.0 * sum(blks_hit) / (sum(blks_hit) + sum(blks_read)) >= 99 THEN 'ok' ELSE 'warn' END AS status,
    'reads served from memory (judged from 10,000 reads on); under 99% the working set doesn''t fit (a bigger compute, or fewer scans: postgres_slow_queries)' AS detail
  FROM pg_stat_database WHERE datname = current_database()
  UNION ALL
  SELECT 2, 'index hit rate', coalesce(round(100.0 * sum(idx_blks_hit) / nullif(sum(idx_blks_hit) + sum(idx_blks_read), 0), 1)::text || '%', 'no index reads yet'),
    CASE WHEN coalesce(sum(idx_blks_hit) + sum(idx_blks_read), 0) < 10000 OR 100.0 * sum(idx_blks_hit) / (sum(idx_blks_hit) + sum(idx_blks_read)) >= 99 THEN 'ok' ELSE 'warn' END,
    'index reads served from memory'
  FROM pg_statio_user_indexes
  UNION ALL
  SELECT 3, 'dead rows', coalesce(string_agg(relname || ' ' || n_dead_tup || ' (' || round(100.0 * n_dead_tup / nullif(n_live_tup + n_dead_tup, 0)) || '%)', ', ' ORDER BY n_dead_tup DESC), 'none'),
    CASE WHEN count(*) = 0 THEN 'ok' ELSE 'warn' END,
    'tables with 10,000 or more dead rows and over 20% of them: bloat until vacuum reclaims it'
  FROM (SELECT relname, n_dead_tup, n_live_tup FROM pg_stat_user_tables WHERE n_dead_tup >= 10000 AND n_dead_tup > 0.2 * (n_live_tup + n_dead_tup) ORDER BY n_dead_tup DESC LIMIT 5) d
  UNION ALL
  SELECT 4, 'oldest open transaction', coalesce(round(max(extract(epoch FROM now() - xact_start)))::text || ' s', 'none'),
    CASE WHEN coalesce(max(extract(epoch FROM now() - xact_start)), 0) < 300 THEN 'ok' ELSE 'warn' END,
    'a transaction open for minutes holds locks and keeps vacuum from cleaning up (postgres_activity shows whose)'
  FROM pg_stat_activity WHERE datname = current_database() AND xact_start IS NOT NULL AND pid <> pg_backend_pid()
  UNION ALL
  SELECT 5, 'idle in transaction', count(*)::text, CASE WHEN count(*) = 0 THEN 'ok' ELSE 'warn' END,
    'sessions that began a transaction and went quiet: they hold their locks'
  FROM pg_stat_activity WHERE datname = current_database() AND state LIKE 'idle in transaction%'
  UNION ALL
  SELECT 6, 'connections', count(*)::text || ' of ' || current_setting('max_connections'),
    CASE WHEN count(*) < 0.8 * current_setting('max_connections')::int THEN 'ok' ELSE 'warn' END,
    'client connections against the limit (a pooler, or fewer idle connections)'
  FROM pg_stat_activity WHERE backend_type = 'client backend'
  UNION ALL
  SELECT 7, 'transaction id age', age(datfrozenxid)::text, CASE WHEN age(datfrozenxid) < 500000000 THEN 'ok' ELSE 'warn' END,
    'past about 2 billion Postgres stops taking writes; vacuum freezes old rows'
  FROM pg_database WHERE datname = current_database()
  UNION ALL
  SELECT 8, 'database size', pg_size_pretty(pg_database_size(current_database())), 'ok', ''
) checks ORDER BY n"# })
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
    t("workspace_create_project", "Create a project", "A new workspace project to group resources.", "workspace", "POST", "/v1/workspace/projects", &[breq("name", Ty::Str, "1-48 of a-z, 0-9 and -"), b("description", Ty::Str, "At most 280 characters"), b("icon", Ty::Obj, ICON)]),
    t("workspace_update_project", "Update a project", "Rename a project, or change its description or icon.", "workspace", "PATCH", "/v1/workspace/projects/{id}", &[WS, b("name", Ty::Str, "New name"), b("description", Ty::Str, "New description"), bnull("icon", Ty::Obj, ICON)]),
    t("workspace_delete_project", "Delete a project", "Delete a workspace project. Its resources stay. A project that holds secrets needs a secrets admin's confirmation in the dashboard.", "workspace", "DELETE", "/v1/workspace/projects/{id}", &[WS]),
    t("workspace_list_roles", "List member roles", "The roles a project member can have (viewer, developer, admin, owner), the rules each one grants, the products those rules cover, and which environments a developer reaches when none are named.", "workspace", "GET", "/v1/workspace/roles", &[]),
    t(
        "workspace_place_resource",
        "Place a resource",
        "Put a resource on a project's canvas (or move it), at x, y.",
        "workspace",
        "PUT",
        "/v1/workspace/projects/{id}/resources/{kind}/{ref}",
        &[WS, req("kind", "postgres, redis, stream, bucket, nosql or repo"), req("ref", "The resource's id (owner/name for a repo)"), breq("x", Ty::Num, "Canvas x"), breq("y", Ty::Num, "Canvas y")],
    ),
    t("workspace_remove_resource", "Remove a resource from a project", "Take a resource off a project's canvas (the resource itself stays).", "workspace", "DELETE", "/v1/workspace/projects/{id}/resources/{kind}/{ref}", &[WS, req("kind", "Resource kind"), req("ref", "The resource's id")]).undoable(),
    // Members (docs/members.md): accepting an invite is a person's own, so it's no tool.
    t("workspace_list_members", "List a project's members", "Who is in a workspace project: handle, role (viewer, developer, admin, owner), a developer's environments, who added them and when. Emails only for the project's admins.", "workspace", "GET", "/v1/workspace/projects/{id}/members", &[WS]),
    t(
        "workspace_invite_member",
        "Invite a member",
        "Invite an email address to a project with a role (never more than the caller's own). The answer has the invite link once; the person accepts it by signing in as that address.",
        "workspace",
        "POST",
        "/v1/workspace/projects/{id}/invites",
        &[WS, breq("email", Ty::Str, "Their email address"), breq("role", Ty::Str, "viewer, developer, admin or owner"), b("environments", Ty::Arr, "A developer's environments, where the role applies in full (default: every environment that isn't protected)")],
    ),
    t(
        "workspace_set_member_role",
        "Change a member's role",
        "Change a member's role or a developer's environments. Both the old and the new role must be within the caller's own; the last owner can't be demoted.",
        "workspace",
        "PATCH",
        "/v1/workspace/projects/{id}/members/{user}",
        &[WS, req("user", "The member (usr_…)"), b("role", Ty::Str, "viewer, developer, admin or owner"), b("environments", Ty::Arr, "A developer's environments (null: the default)")],
    )
    .destructive(),
    t("workspace_remove_member", "Remove a member", "Take a person out of a project: their session and tokens stop reaching it at once, and their tokens confined to it are revoked. The last owner can't be removed.", "workspace", "DELETE", "/v1/workspace/projects/{id}/members/{user}", &[WS, req("user", "The member (usr_…)")]).destructive(),
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
    t("postgres_create_replica", "Add a read replica", "A read-only endpoint on a branch that follows its primary, for reads (analytics, a read-heavy service) without touching the primary. Its connection string is a credential (shown only to a caller that may reveal).", "postgres", "POST", "/v1/projects/{project}/branches/{branch}/replicas", &[PROJECT, BRANCH]),
    t("postgres_restore_branch", "Restore a branch", "Rewind a branch in place to a past point (the current state is kept as a branch).", "postgres", "POST", "/v1/projects/{project}/branches/{branch}/restore", &[PROJECT, BRANCH, b("at", Ty::Str, "Point in time (RFC 3339)"), b("lsn", Ty::Str, "Or an LSN")]),
    t("postgres_reset_branch", "Reset a branch", "Put a branch back to its parent's current state, in place: the same endpoint and connection string, its own changes gone (schema_only: then empty every table).", "postgres", "POST", "/v1/projects/{project}/branches/{branch}/reset", &[PROJECT, BRANCH, b("schema_only", Ty::Bool, "Keep only the structure")]),
    t("postgres_get_endpoint", "Get an endpoint", "An endpoint's compute: state (running, suspended), size limits and suspend timeout (connection strings need reveal).", "postgres", "GET", "/v1/endpoints/{endpoint}", &[ENDPOINT]),
    t("postgres_update_endpoint", "Resize an endpoint", "An endpoint's autoscaling limits and how long it idles before suspending.", "postgres", "PATCH", "/v1/endpoints/{endpoint}", &[ENDPOINT, b("min_cu", Ty::Num, "Smallest size (compute units)"), b("max_cu", Ty::Num, "Largest size"), b("suspend_timeout_s", Ty::Int, "Idle seconds before suspending")]),
    t("postgres_query", "Query Postgres (read-only)", "Run read-only SQL on an endpoint: it runs in a read-only transaction, so writes fail. Returns columns and rows.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT, breq("sql", Ty::Str, "SQL to run")]).fixed(read_only),
    t("postgres_execute", "Execute SQL", "Run SQL that may change data or schema (INSERT, UPDATE, DDL) on an endpoint. Without admin on the endpoint it runs as a role that owns the database's objects but isn't the superuser: making roles, untrusted extensions, COPY … PROGRAM and the server's files need admin, and dropping a table, column or schema needs delete.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT, breq("sql", Ty::Str, "SQL to run")]).fixed(writes).destructive(),
    t("postgres_describe_schema", "Describe the schema", "Every table's columns and types on an endpoint (read-only).", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(describe_schema),
    t("postgres_explain", "Explain a query", "A statement's plan as JSON (EXPLAIN, FORMAT JSON), without running it: which indexes it uses, its estimated cost and rows. For real timings, postgres_query with EXPLAIN ANALYZE.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT, breq("sql", Ty::Str, "The statement")]).fixed(read_only).wraps(explain),
    t("postgres_activity", "What's running and blocking queries", "Every client session on an endpoint now: user, application (`roll tok_…` or `roll usr_…`: SQL that token or person sent through roll), state, what it waits on, which sessions block it (blocked_by), how long its transaction and query have run, and its query (300 characters). Blocked sessions first. Start here when queries hang or time out.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(activity),
    t("postgres_health_checks", "Database health checks", "A database's health in one read, each check ok or warn with what it means: cache and index hit rates, tables bloated with dead rows, the oldest open transaction, sessions idle in a transaction, connections against the limit, transaction ID age, and size.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(health_checks),
    t("postgres_slow_queries", "Slowest queries", "The 20 slowest statements by mean time, with calls, rows and cache hit rate (pg_stat_statements; if it's missing, CREATE EXTENSION pg_stat_statements once with postgres_execute, which takes admin on the endpoint).", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(slow_queries),
    t("postgres_advisors", "Database advice", "What's worth fixing: unused indexes, foreign keys without an index, tables without a primary key, large tables read by sequential scans.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(advisors),
    t("postgres_list_extensions", "Extensions", "Extensions installed and available, with versions.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(extensions),
    t("postgres_typescript_types", "TypeScript types", "TypeScript interfaces for every table on an endpoint (enums as unions, arrays, nullable columns as | null): to write typed code against the schema.", "postgres", "POST", "/v1/endpoints/{endpoint}/query", &[ENDPOINT]).fixed(typescript_types),
    t("postgres_start_endpoint", "Start (wake) a database's compute", "Wake a Postgres endpoint's compute now, before a client connects (one connecting wakes it too).", "postgres", "POST", "/v1/endpoints/{endpoint}/start", &[ENDPOINT]),
    t("postgres_suspend_endpoint", "Suspend an endpoint", "Suspend an endpoint's compute.", "postgres", "POST", "/v1/endpoints/{endpoint}/suspend", &[ENDPOINT]),
    t("postgres_update_project", "Change a Postgres project", "How long a project keeps history for point-in-time restores and branches.", "postgres", "PATCH", "/v1/projects/{project}", &[PROJECT, breq("history_retention_s", Ty::Int, "Seconds of history to keep")]),
    t("postgres_backup_branch", "Back up a branch", "Take a base backup of a branch now (restores and new branches start from the newest).", "postgres", "POST", "/v1/projects/{project}/branches/{branch}/backup", &[PROJECT, BRANCH]),
    t("postgres_delete_endpoint", "Delete an endpoint", "Delete an endpoint (a read replica, or a branch's compute; its data stays on the branch).", "postgres", "DELETE", "/v1/endpoints/{endpoint}", &[ENDPOINT]),
    t("postgres_usage", "Postgres usage", "Compute and storage use of a project, with cost estimates.", "postgres", "GET", "/v1/projects/{project}/usage", &[PROJECT, DAYS]),
    // ── Redis ──
    t("redis_list_databases", "List Redis databases", "Serverless Redis databases.", "redis", "GET", "/v1/redis/databases", &[]),
    t("redis_get_database", "Get a Redis database", "A Redis database's settings and connection (credentials need reveal).", "redis", "GET", "/v1/redis/databases/{database}", &[REDIS]),
    t("redis_overview", "roll Redis here", "What roll Redis offers here: its version, region, defaults, endpoints (RESP, HTTP, TLS), storage and pricing.", "redis", "GET", "/v1/redis", &[]),
    t("redis_create_replica", "Add a read replica", "A read replica of a Redis database: its own address that follows the primary, for reads. Its password is a credential (shown only to a caller that may reveal).", "redis", "POST", "/v1/redis/databases/{database}/replicas", &[REDIS]),
    t("redis_delete_replica", "Remove a read replica", "Remove one of a database's read replicas: its address stops answering.", "redis", "DELETE", "/v1/redis/replicas/{replica}", &[req("replica", "Replica id")]),
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
        "List objects (a bucket's files)",
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
    t("storage_restore_object", "Restore a deleted object", "Bring an object back from the trash (key and deleted_at from storage_list_trash).", "storage", "POST", "/v1/storage/buckets/{bucket}/trash/restore", &[BUCKET, breq("key", Ty::Str, "Object key"), breq("deleted_at", Ty::Int, "When it was deleted, as listed")]).undoable(),
    t("storage_usage", "Storage usage", "A bucket's usage.", "storage", "GET", "/v1/storage/buckets/{bucket}/usage", &[BUCKET, DAYS]),
    t("storage_usage_all", "Storage usage, every bucket", "Storage and requests across every bucket.", "storage", "GET", "/v1/storage/usage", &[DAYS]),
    t("storage_list_accounts", "List storage accounts", "Storage's accounts (AWS-style, 12-digit ids): name, alias, and which is the default (creating one is the operator's).", "storage", "GET", "/v1/storage/accounts", &[]),
    t("storage_overview", "roll storage here", "What roll storage offers here: the S3 endpoint and region, and the account's ids.", "storage", "GET", "/v1/storage", &[]),
    t("storage_get_account", "Get a storage account", "One storage account: its name, email, ids (12-digit and canonical) and whether it's the default.", "storage", "GET", "/v1/storage/accounts/{id}", &[req("id", "Account id (12 digits)")]),
    t("storage_list_keys", "List S3 access keys", "An account's access keys: id, name, permission, the buckets it's limited to, the IAM user it belongs to and when it was last used (never a secret).", "storage", "GET", "/v1/storage/keys", &[q("account", Ty::Str, "Account id (12 digits; the default account when left out)")]),
    t("storage_revoke_key", "Revoke an S3 access key", "Revoke an S3 access key at once (an unused or too-wide one, from storage_list_keys). Making one answers its secret, so it has no tool.", "storage", "DELETE", "/v1/storage/keys/{id}", &[req("id", "Access key id"), q("account", Ty::Str, "Account id (12 digits; the default account when left out)")]).destructive(),
    t("storage_list_roles", "List IAM roles", "An account's IAM roles: ARN, trust, permissions policy and session length. Use it when an assumed role is refused.", "storage", "GET", "/v1/storage/roles", &[q("account", Ty::Str, "Account id (12 digits; the default account when left out)")]),
    t("storage_get_role", "Get an IAM role", "One IAM role: its ARN, trust, permissions policy and session length.", "storage", "GET", "/v1/storage/roles/{name}", &[req("name", "Role name"), q("account", Ty::Str, "Account id (12 digits; the default account when left out)")]),
    t("storage_revoke_role_sessions", "Revoke a role's sessions", "Deny every session issued for a role until now (AWS's AWSRevokeOlderSessions): leaked temporary credentials stop working at once; new sessions still can be made.", "storage", "POST", "/v1/storage/roles/{name}/revoke", &[req("name", "Role name"), q("account", Ty::Str, "Account id (12 digits; the default account when left out)")]).destructive(),
    t("storage_list_replication_targets", "List replication targets", "Buckets outside roll that replication copies to: endpoint, region, access key id, buckets (never the secret; adding one is admin).", "storage", "GET", "/v1/storage/replication-targets", &[]),
    t("storage_get_replication_target", "Get a replication target", "One replication target outside roll: its endpoint, bucket, region and access key id (never its secret).", "storage", "GET", "/v1/storage/replication-targets/{name}", &[req("name", "Target name")]),
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
    t("streams_overview", "roll streams here", "What roll streams offers here: the Kafka bootstrap address, the SASL mechanisms and security protocol, the default stream, storage and pricing.", "streams", "GET", "/v1/kafka", &[]),
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
    t("streams_list_groups", "List consumer groups and their lag", "Consumer groups with their lag.", "streams", "GET", "/v1/streams/{stream}/groups", &[STREAM]),
    t("streams_reset_group", "Reset a consumer group", "Move a group's offsets (earliest, latest, an offset or timestamp:<ms>).", "streams", "POST", "/v1/streams/{stream}/groups/{group}/reset", &[STREAM, req("group", "Group id"), breq("topic", Ty::Str, "Topic"), breq("to", Ty::Str, "Where to")]),
    t("streams_get_topic", "Get a topic", "A topic's partitions, their offsets and its settings (retention, compaction).", "streams", "GET", "/v1/streams/{stream}/topics/{topic}", &[STREAM, TOPIC]),
    t("streams_get_group", "Get a consumer group", "A group's members and its committed offset and lag on each partition.", "streams", "GET", "/v1/streams/{stream}/groups/{group}", &[STREAM, req("group", "Group id")]),
    t("streams_branch", "Branch a stream", "A new stream with this one's topics, records (shared, not copied) and committed offsets; its first key's secret comes back once (reveal).", "streams", "POST", "/v1/streams/{stream}/branches", &[STREAM, breq("name", Ty::Str, "Branch name")]),
    t("streams_usage", "Stream usage", "A stream's usage and throughput.", "streams", "GET", "/v1/streams/{stream}/usage", &[STREAM, q("hours", Ty::Int, "Hours of history")]),
    t("streams_list_keys", "List a stream's keys", "The SASL users a stream's Kafka clients sign in as: username, label and when made (never a password; a new key is a reveal, in the dashboard or roll stream).", "streams", "GET", "/v1/streams/{stream}/keys", &[STREAM]),
    t("streams_revoke_key", "Revoke a stream key", "Revoke one of a stream's SASL users at once: its Kafka clients can't sign in again. Making one answers a password, so it has no tool.", "streams", "DELETE", "/v1/streams/{stream}/keys/{username}", &[STREAM, req("username", "The key's username")]).destructive(),
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
    t("nosql_overview", "roll NoSQL here", "What roll NoSQL offers here: the dialects (ScyllaDB, Cassandra) and their versions, and the CQL and other endpoints.", "nosql", "GET", "/v1/nosql", &[]),
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
            bnull("expires_at", Ty::Int, "Unix seconds it's deleted at (clear it: never)"),
        ],
    ),
    t("nosql_activity", "NoSQL activity", "What a database is doing now: connected clients, slow queries, warnings and metrics.", "nosql", "GET", "/v1/nosql/databases/{database}/activity", &[NOSQL]),
    t("nosql_list_imports", "List imports into a database", "A database's imports from another cluster (cql://) or a backup (s3://, file://): state, keyspaces, rows, tables done, errors, the schedule and the next run (the source's password hidden).", "nosql", "GET", "/v1/nosql/databases/{database}/imports", &[NOSQL]),
    // ── queues ──
    t("queues_overview", "Queues overview", "The broker's totals and listeners.", "queues", "GET", "/v1/queues/overview", &[]),
    t("queues_list_vhosts", "List virtual hosts", "RabbitMQ-compatible virtual hosts.", "queues", "GET", "/v1/queues/vhosts", &[]),
    t("queues_create_vhost", "Create a virtual host", "A new virtual host.", "queues", "POST", "/v1/queues/vhosts", &[breq("name", Ty::Str, "Name"), b("description", Ty::Str, "Description"), IN_PROJECT]),
    t("queues_delete_vhost", "Delete a virtual host", "Delete a virtual host and everything in it.", "queues", "DELETE", "/v1/queues/vhosts/{vhost}", &[VHOST]),
    t("queues_suspend_vhost", "Suspend a virtual host", "Scale a virtual host to zero now: its queues keep their messages, and a client connecting wakes it.", "queues", "POST", "/v1/queues/vhosts/{vhost}/suspend", &[VHOST]),
    t("queues_wake_vhost", "Wake a virtual host", "Start a suspended virtual host now, before a client connects.", "queues", "POST", "/v1/queues/vhosts/{vhost}/wake", &[VHOST]),
    t("queues_list_queues", "List queues", "A virtual host's queues with message counts.", "queues", "GET", "/v1/queues/vhosts/{vhost}/queues", &[VHOST]),
    t("queues_declare_queue", "Declare a queue", "Create a queue.", "queues", "POST", "/v1/queues/vhosts/{vhost}/queues", &[VHOST, breq("name", Ty::Str, "Queue name"), b("type", Ty::Str, "classic, quorum or stream"), b("durable", Ty::Bool, "Durable")]),
    t("queues_delete_queue", "Delete a queue", "Delete a queue and its messages.", "queues", "DELETE", "/v1/queues/vhosts/{vhost}/queues/{queue}", &[VHOST, QUEUE]),
    t("queues_purge_queue", "Purge a queue", "Drop every message in a queue.", "queues", "POST", "/v1/queues/vhosts/{vhost}/queues/{queue}/purge", &[VHOST, QUEUE]),
    t("queues_get_queue", "Get a queue", "One queue: its messages (ready and unacknowledged), consumers and their prefetch, arguments, and the dead-letter queues it feeds. Start here when a queue is backing up and messages are piling up.", "queues", "GET", "/v1/queues/vhosts/{vhost}/queues/{queue}", &[VHOST, QUEUE]),
    t("queues_queue_rates", "One queue's rates", "A queue's publish, deliver and acknowledge rates and depth over the last seconds (5-300, default 300), as points.", "queues", "GET", "/v1/queues/vhosts/{vhost}/queues/{queue}/rates", &[VHOST, QUEUE, q("seconds", Ty::Int, "How far back, 5-300 (default 300)")]),
    t("queues_list_exchanges", "List exchanges", "A virtual host's exchanges, their types and arguments.", "queues", "GET", "/v1/queues/vhosts/{vhost}/exchanges", &[VHOST]),
    t("queues_list_bindings", "List bindings", "A virtual host's bindings: which exchange routes to which queue by which key. Why a message went nowhere.", "queues", "GET", "/v1/queues/vhosts/{vhost}/bindings", &[VHOST]),
    t("queues_declare_exchange", "Declare an exchange", "Make an exchange in a virtual host (idempotent: the same declaration again is a no-op, a different one a 409; amq.* names are reserved). Bind queues to it with queues_bind: a fanout or topic exchange bound to several queues is SNS-style fan-out.", "queues", "POST", "/v1/queues/vhosts/{vhost}/exchanges", &[VHOST, breq("name", Ty::Str, "Exchange name"), breq("type", Ty::Str, "direct, fanout, topic, headers or x-delayed-message"), b("durable", Ty::Bool, "Survive a restart (default true)"), b("auto_delete", Ty::Bool, "Go once nothing is bound to it"), b("internal", Ty::Bool, "Only other exchanges publish to it"), b("arguments", Ty::Obj, "e.g. alternate-exchange, x-delayed-type")]),
    t("queues_delete_exchange", "Delete an exchange", "Delete an exchange and its bindings (the amq.* ones can't be).", "queues", "DELETE", "/v1/queues/vhosts/{vhost}/exchanges/{exchange}", &[VHOST, req("exchange", "Exchange name")]),
    t("queues_bind", "Bind a queue to an exchange", "Route an exchange's messages to a queue (or another exchange) by a binding key: a no-op if the binding exists. queues_list_bindings shows them.", "queues", "POST", "/v1/queues/vhosts/{vhost}/bindings", &[VHOST, breq("source", Ty::Str, "The exchange messages come from"), breq("destination", Ty::Str, "The queue (or exchange) they go to"), b("destination_type", Ty::Str, "queue (default) or exchange"), b("routing_key", Ty::Str, "The binding key, e.g. order.# for a topic exchange"), b("arguments", Ty::Obj, "e.g. x-match for a headers exchange")]),
    t("queues_unbind", "Remove a binding", "Stop routing an exchange's messages to a queue by that key (the same fields as queues_bind; no error if there was none).", "queues", "DELETE", "/v1/queues/vhosts/{vhost}/bindings", &[VHOST, breq("source", Ty::Str, "The exchange messages come from"), breq("destination", Ty::Str, "The queue (or exchange) they go to"), b("destination_type", Ty::Str, "queue (default) or exchange"), b("routing_key", Ty::Str, "The binding key, e.g. order.# for a topic exchange"), b("arguments", Ty::Obj, "e.g. x-match for a headers exchange")]),
    t("queues_replay", "Replay a queue's messages", "Move up to count messages (default 1000) out of a queue, usually a dead-letter queue, and publish each again: to where it was dead-lettered from (its x-death), or to target_exchange with target_routing_key. Answers how many went and how many were skipped.", "queues", "POST", "/v1/queues/vhosts/{vhost}/queues/{queue}/replay", &[VHOST, req("queue", "Queue to replay from"), b("count", Ty::Int, "At most this many (1-100000, default 1000)"), b("target_exchange", Ty::Str, "Publish here instead of where each came from"), b("target_routing_key", Ty::Str, "With target_exchange")]),
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
    ).open_world(),
    t("tasks_list_messages", "List task messages", "Messages by state, queue or topic.", "tasks", "GET", "/v1/tasks/messages", &[q("state", Ty::Str, "pending, delivered, failed…"), q("queue", Ty::Str, "Queue"), q("limit", Ty::Int, "At most")]),
    t("tasks_get_message", "Get a task message", "A message and its delivery attempts.", "tasks", "GET", "/v1/tasks/messages/{id}", &[req("id", "Message id (msg_…)")]),
    t("tasks_cancel_message", "Cancel a task message", "Cancel a message (purge=true deletes it).", "tasks", "DELETE", "/v1/tasks/messages/{id}", &[req("id", "Message id"), q("purge", Ty::Bool, "Delete it for good")]),
    t("tasks_list_schedules", "List schedules", "Cron schedules.", "tasks", "GET", "/v1/tasks/schedules", &[]),
    t("tasks_create_schedule", "Create a schedule", "A cron schedule that publishes a task.", "tasks", "POST", "/v1/tasks/schedules", &[breq("cron", Ty::Str, "Cron expression"), b("tz", Ty::Str, "Time zone"), breq("publish", Ty::Obj, "What to publish (as tasks_publish)")]).open_world(),
    t("tasks_delete_schedule", "Delete a schedule", "Delete a schedule.", "tasks", "DELETE", "/v1/tasks/schedules/{id}", &[req("id", "Schedule id")]),
    t("tasks_list_runs", "List function runs", "Durable function runs.", "tasks", "GET", "/v1/tasks/runs", &[q("function", Ty::Str, "Function"), q("status", Ty::Str, "Status"), q("limit", Ty::Int, "At most")]),
    t("tasks_get_run", "Get a function run", "A run with its steps.", "tasks", "GET", "/v1/tasks/runs/{id}", &[req("id", "Run id (run_…)")]),
    t("tasks_invoke", "Run a function now", "Start a durable function run (invoke it).", "tasks", "POST", "/v1/tasks/functions/{id}/invoke", &[req("id", "Function id"), b("data", Ty::Obj, "Input"), b("wait", Ty::Bool, "Wait for the result")]).open_world(),
    t("tasks_retry_message", "Retry a message", "Deliver a failed or dead-lettered message again now.", "tasks", "POST", "/v1/tasks/messages/{id}/retry", &[req("id", "Message id")]).open_world(),
    t("tasks_retry_dead_letters", "Retry dead letters", "Deliver dead-lettered messages again: these ids, a queue's, or all.", "tasks", "POST", "/v1/tasks/dlq/retry", &[b("ids", Ty::Arr, "Message ids"), b("queue", Ty::Str, "Only this queue's"), b("all", Ty::Bool, "Every dead letter")]).open_world(),
    t("tasks_cancel_run", "Cancel a run", "Stop a durable function run (its finished steps stay done).", "tasks", "POST", "/v1/tasks/runs/{id}/cancel", &[req("id", "Run id")]),
    t("tasks_rerun", "Rerun a run", "Run a function again with the same input as a past run.", "tasks", "POST", "/v1/tasks/runs/{id}/rerun", &[req("id", "Run id")]).open_world(),
    t("tasks_list_functions", "List functions", "Durable functions: their triggers and URLs.", "tasks", "GET", "/v1/tasks/functions", &[]),
    t("tasks_list_apps", "List synced apps", "The apps whose functions tasks runs: each one's URL, functions, SDK and when it last synced. Use it when a function is missing or stale.", "tasks", "GET", "/v1/tasks/apps", &[]),
    t("tasks_delete_app", "Remove a synced app", "Stop syncing an app whose functions tasks runs (tasks_list_apps): its functions are no longer run from it.", "tasks", "DELETE", "/v1/tasks/apps/{id}", &[req("id", "App id")]),
    t("tasks_list_api_keys", "List tasks API keys", "Tasks' API keys: name, prefix, when made and last used (never the key itself).", "tasks", "GET", "/v1/tasks/api-keys", &[]),
    t("tasks_delete_api_key", "Revoke an API key", "Revoke one of tasks' API keys at once: clients using it are refused. Making one answers its secret, so it has no tool.", "tasks", "DELETE", "/v1/tasks/api-keys/{id}", &[req("id", "API key id")]),
    t("tasks_list_queues", "List task queues", "Queues with their parallelism, rate and backlog.", "tasks", "GET", "/v1/tasks/queues", &[]),
    t("tasks_stats", "Tasks stats", "Nodes, queue stats, dead letters, schedules and functions.", "tasks", "GET", "/v1/tasks/stats", &[]),
    t("tasks_list_dead_letters", "List dead letters", "Messages that kept failing until they ran out of retries, newest first.", "tasks", "GET", "/v1/tasks/dlq", &[q("queue", Ty::Str, "Only this queue's"), q("topic", Ty::Str, "Only this topic's"), q("cursor", Ty::Str, "From a page's cursor"), q("limit", Ty::Int, "At most")]),
    t("tasks_delete_dead_letter", "Delete a dead letter", "Drop a dead-lettered message for good.", "tasks", "DELETE", "/v1/tasks/dlq/{id}", &[req("id", "Message id (msg_…)")]),
    t("tasks_get_queue", "Get a task queue", "A queue's parallelism, rate, retries, backlog and whether it's paused.", "tasks", "GET", "/v1/tasks/queues/{name}", &[req("name", "Queue")]),
    t(
        "tasks_configure_queue",
        "Create or configure a task queue",
        "Make a queue, or change its limits: what's left out stays as it is.",
        "tasks",
        "PUT",
        "/v1/tasks/queues/{name}",
        &[req("name", "Queue"), b("parallelism", Ty::Int, "Deliveries at once (0: no limit)"), bnull("rate", Ty::Any, "e.g. \"10/s\" or {\"limit\": 10, \"period\": \"1m\"}; clear it for no limit"), b("retries", Ty::Int, "Retries per message"), b("timeout", Ty::Str, "Per delivery, e.g. 30s"), b("partitions", Ty::Int, "Ordered partitions")],
    ),
    t("tasks_pause_queue", "Pause a task queue", "Hold a queue's deliveries; messages keep arriving.", "tasks", "POST", "/v1/tasks/queues/{name}/pause", &[req("name", "Queue")]),
    t("tasks_resume_queue", "Resume a task queue", "Deliver a paused queue's messages again.", "tasks", "POST", "/v1/tasks/queues/{name}/resume", &[req("name", "Queue")]),
    t("tasks_list_topics", "List topics", "Topics: a message published to one goes to each of its endpoints.", "tasks", "GET", "/v1/tasks/topics", &[]),
    t("tasks_get_topic", "Get a topic", "A topic and its endpoints.", "tasks", "GET", "/v1/tasks/topics/{name}", &[req("name", "Topic")]),
    t("tasks_set_topic", "Create or replace a topic", "A topic with exactly these endpoints.", "tasks", "PUT", "/v1/tasks/topics/{name}", &[req("name", "Topic"), breq("endpoints", Ty::Arr, "URLs, or {\"url\", \"name\"} objects")]).open_world(),
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
    t("tasks_send_event", "Send an event", "An event that starts the functions it triggers (an id makes it idempotent).", "tasks", "POST", "/v1/tasks/events", &[breq("name", Ty::Str, "Event name, e.g. user.signup"), b("data", Ty::Any, "Its data"), b("id", Ty::Str, "Idempotency id")]).open_world(),
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
    ).open_world(),
    t("tasks_pause_schedule", "Pause a schedule", "Stop a schedule firing until it's resumed.", "tasks", "POST", "/v1/tasks/schedules/{id}/pause", &[req("id", "Schedule id")]),
    t("tasks_resume_schedule", "Resume a schedule", "Fire a paused schedule again.", "tasks", "POST", "/v1/tasks/schedules/{id}/resume", &[req("id", "Schedule id")]),
    t("tasks_trigger_schedule", "Fire a schedule now", "Publish what a schedule publishes once, now.", "tasks", "POST", "/v1/tasks/schedules/{id}/trigger", &[req("id", "Schedule id")]).open_world(),
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
    t("search_overview", "roll search here", "What roll search offers here: versions, the endpoint, defaults, the history window and pricing.", "search", "GET", "/v1/search", &[]),
    t("search_unlink_remote", "Unlink a remote cluster", "Stop a cluster searching another (alias:index): the remote's settings and credentials go, and a cross-cluster key roll made for it is revoked. Linking one stays the cluster admin's (no tool).", "search", "DELETE", "/v1/search/clusters/{cluster}/remotes/{alias}", &[req("cluster", "Cluster id or name"), req("alias", "The remote's alias")]).destructive(),
    t("search_revoke_key", "Revoke a cluster API key", "Revoke one of a cluster's API keys at once (404 once it's gone; on a deleted cluster the revoke is applied by its restore). Making one answers a credential, so it has no tool.", "search", "DELETE", "/v1/search/clusters/{cluster}/keys/{key}", &[req("cluster", "Cluster id or name"), req("key", "The API key's id")]).destructive(),
    t("search_list_browser_keys", "List a cluster's browser keys", "The keys a cluster gave web pages for search from the browser that still work, newest first: name, the indices or search application each may read, its filter, rate limit and when it expires (never the key). Making one answers a credential, so it has no tool.", "search", "GET", "/v1/search/clusters/{cluster}/browser-keys", &[req("cluster", "Cluster id or name")]),
    t("search_revoke_browser_key", "Revoke a browser key", "Revoke one of a cluster's browser keys at once: pages shipping it stop getting answers (on a deleted cluster the revoke is applied by its restore).", "search", "DELETE", "/v1/search/clusters/{cluster}/browser-keys/{key}", &[req("cluster", "Cluster id or name"), req("key", "Browser key id (from search_list_browser_keys)")]),
    t("search_list_inference_secrets", "Inference endpoints keyed by a roll secret", "A cluster's inference endpoints whose API key comes from a roll secret, with the project and key each follows (a rotated value reaches the endpoint within seconds; never the value), and the projects whose secrets it could use.", "search", "GET", "/v1/search/clusters/{cluster}/inference/secrets", &[req("cluster", "Cluster id or name")]),
    t("search_branch_cluster", "Branch a search cluster", "A copy-on-write branch of a cluster, now or at a time within its history.", "search", "POST", "/v1/search/clusters/{cluster}/branches", &[req("cluster", "Cluster id or name"), breq("name", Ty::Str, "Branch name"), b("at", Ty::Int, "Unix seconds")]),
    t("search_update_cluster", "Update a search cluster", "Change a cluster's settings: idle_timeout_s (0: always loaded), max_in_flight, max_body_mb at once; flavor or destructive_requires_name put it to sleep to reload once no request runs; ip_allowlist replaces the networks its endpoint answers (up to 64 CIDRs, each a string or {cidr, description}; [] answers everyone; this API isn't filtered by it).", "search", "PATCH", "/v1/search/clusters/{cluster}", &[req("cluster", "Cluster id or name"), b("idle_timeout_s", Ty::Int, "Seconds idle before it sleeps (0: never)"), b("max_in_flight", Ty::Int, "Requests at once"), b("max_body_mb", Ty::Int, "Largest request body"), b("destructive_requires_name", Ty::Bool, "Refuse wildcard deletes"), b("flavor", Ty::Str, "elasticsearch or opensearch"), b("ip_allowlist", Ty::Arr, "CIDRs its endpoint answers (strings or {cidr, description})")]),
    t("search_delete_cluster", "Delete a search cluster", "Delete a cluster and its indices (its branches keep working). It's kept for the deployment's restore window (7 days by default) and search_restore_cluster brings it back until then.", "search", "DELETE", "/v1/search/clusters/{cluster}", &[req("cluster", "Cluster id or name")]),
    t("search_restore_cluster", "Restore a deleted search cluster", "Bring a deleted cluster back as it was (data, snapshots, settings, password, API keys) within its restore window, by its id (search_list_clusters with deleted=true). Its name, or name if that's taken.", "search", "POST", "/v1/search/clusters/{cluster}/restore", &[req("cluster", "The deleted cluster's id"), b("name", Ty::Str, "Another name, if its own is taken")]).undoable(),
    t("search_cluster_metrics", "Search cluster metrics", "A cluster's request rate, latency (p50, p95, p99), and 2xx/4xx/5xx/429 answers by kind of request, over range 1h (default), 24h or 14d.", "search", "GET", "/v1/search/clusters/{cluster}/metrics", &[req("cluster", "Cluster id or name"), q("range", Ty::Str, "1h, 24h or 14d")]),
    t("search_slowlog", "Search slow log", "A cluster's slowest recent requests: what ran, on which index, and how long it took.", "search", "GET", "/v1/search/clusters/{cluster}/slowlog", &[req("cluster", "Cluster id or name")]),
    t("search_audit", "Search audit log", "A cluster's security audit log, newest first: failed logins, access denials and changes to users, roles, role mappings and API keys (granted access and successful logins too when the cluster records them), in Elasticsearch's or OpenSearch's event names (authentication_failed, access_denied, security_config_change, …). Never a secret or a request body.", "search", "GET", "/v1/search/clusters/{cluster}/audit", &[req("cluster", "Cluster id or name"), q("from", Ty::Int, "From (Unix ms, inclusive)"), q("to", Ty::Int, "To (Unix ms, exclusive)"), q("event", Ty::Str, "Event names or categories, comma-separated"), q("size", Ty::Int, "Entries (default 100)")]),
    t("search_analytics", "Search analytics", "What a cluster's users search for, over range 24h (default), 7d or 30d: top queries, queries that found nothing, and click-through, for one index or all. Only indices with index.roll.analytics.enabled record it.", "search", "GET", "/v1/search/clusters/{cluster}/analytics", &[req("cluster", "Cluster id or name"), q("index", Ty::Str, "One index (default: all)"), q("range", Ty::Str, "24h, 7d or 30d")]).outside(&["query"]),
    t("search_suggestions", "Query suggestions", "An index's query suggestions built from its search analytics: its settings (index.roll.suggestions.*), its last build (when, how many, any error) and its heaviest suggestions.", "search", "GET", "/v1/search/clusters/{cluster}/indices/{index}/suggestions", &[req("cluster", "Cluster id or name"), req("index", "Index whose analytics the suggestions come from")]).outside(&["query"]),
    t("search_rebuild_suggestions", "Rebuild query suggestions", "Rebuild an index's suggestions index from its search analytics now (it is rebuilt on its schedule anyway); the alias moves to the new build at once. Needs index.roll.analytics.enabled and index.roll.suggestions.enabled.", "search", "POST", "/v1/search/clusters/{cluster}/indices/{index}/suggestions/rebuild", &[req("cluster", "Cluster id or name"), req("index", "Index whose analytics the suggestions come from")]),
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
    t("search_put_index_settings", "Change index settings", "Change an index's dynamic settings (Elasticsearch's PUT /<index>/_settings): refresh_interval, number_of_replicas, analysis on a closed index. A lifecycle policy (index.lifecycle.*, OpenSearch's index_state_management) can delete the index, so it needs delete; anything else needs write.", "search", "PUT", "/v1/search/clusters/{cluster}/indices/{index}/settings", &[req("cluster", "Cluster"), req("index", "Index"), breq("index", Ty::Obj, "Settings under index, e.g. {\"refresh_interval\": \"30s\"}")]),
    t("search_explain", "Explain a document's score", "Why a document matches a query and how its score adds up (Elasticsearch's POST /<index>/_explain/<id>).", "search", "POST", "/v1/search/clusters/{cluster}/indices/{index}/explain/{doc}", &[req("cluster", "Cluster"), req("index", "Index"), req("doc", "Document id"), breq("query", Ty::Obj, "Query DSL")]),
    t("search_list_synonyms", "List synonyms sets", "A cluster's synonyms sets and how many rules each has (Elasticsearch's GET /_synonyms); an analyzer names one by synonyms_set.", "search", "GET", "/v1/search/clusters/{cluster}/synonyms", &[req("cluster", "Cluster")]),
    t("search_get_synonyms", "Get a synonyms set", "A synonyms set's rules (Elasticsearch's GET /_synonyms/<set>).", "search", "GET", "/v1/search/clusters/{cluster}/synonyms/{set}", &[req("cluster", "Cluster"), req("set", "Synonyms set")]),
    t("search_put_synonyms", "Replace a synonyms set", "Create a synonyms set or replace all its rules (Elasticsearch's PUT /_synonyms/<set>); analyzers using it reload.", "search", "PUT", "/v1/search/clusters/{cluster}/synonyms/{set}", &[req("cluster", "Cluster"), req("set", "Synonyms set"), breq("synonyms_set", Ty::Arr, "Rules: [{\"id\": \"r1\", \"synonyms\": \"tv, television\"}] (Solr format, => for one-way)")]).destructive(),
    t("search_delete_synonyms", "Delete a synonyms set", "Delete a synonyms set (refused while an index's analyzer uses it).", "search", "DELETE", "/v1/search/clusters/{cluster}/synonyms/{set}", &[req("cluster", "Cluster"), req("set", "Synonyms set")]),
    t("search_get_synonym_rule", "Get a synonym rule", "One rule of a synonyms set.", "search", "GET", "/v1/search/clusters/{cluster}/synonyms/{set}/{rule}", &[req("cluster", "Cluster"), req("set", "Synonyms set"), req("rule", "Rule id")]),
    t("search_put_synonym_rule", "Set a synonym rule", "Add or change one rule of a synonyms set (Elasticsearch's PUT /_synonyms/<set>/<rule>).", "search", "PUT", "/v1/search/clusters/{cluster}/synonyms/{set}/{rule}", &[req("cluster", "Cluster"), req("set", "Synonyms set"), req("rule", "Rule id"), breq("synonyms", Ty::Str, "The rule, e.g. \"tv, television\" or \"laptop => notebook\"")]),
    t("search_delete_synonym_rule", "Delete a synonym rule", "Delete one rule of a synonyms set.", "search", "DELETE", "/v1/search/clusters/{cluster}/synonyms/{set}/{rule}", &[req("cluster", "Cluster"), req("set", "Synonyms set"), req("rule", "Rule id")]),
    t("search_list_query_rulesets", "List query rulesets", "A cluster's query rulesets (Elasticsearch's GET /_query_rules): pinned and excluded results for matching queries, used by a rule query or retriever.", "search", "GET", "/v1/search/clusters/{cluster}/query_rules", &[req("cluster", "Cluster")]),
    t("search_get_query_ruleset", "Get a query ruleset", "A query ruleset's rules (Elasticsearch's GET /_query_rules/<ruleset>).", "search", "GET", "/v1/search/clusters/{cluster}/query_rules/{ruleset}", &[req("cluster", "Cluster"), req("ruleset", "Ruleset")]),
    t("search_put_query_ruleset", "Replace a query ruleset", "Create a query ruleset or replace all its rules (Elasticsearch's PUT /_query_rules/<ruleset>).", "search", "PUT", "/v1/search/clusters/{cluster}/query_rules/{ruleset}", &[req("cluster", "Cluster"), req("ruleset", "Ruleset"), breq("rules", Ty::Arr, "Rules: [{rule_id, type: pinned|exclude, criteria: [{type, metadata, values}], actions: {ids|docs}}]")]).destructive(),
    t("search_delete_query_ruleset", "Delete a query ruleset", "Delete a query ruleset.", "search", "DELETE", "/v1/search/clusters/{cluster}/query_rules/{ruleset}", &[req("cluster", "Cluster"), req("ruleset", "Ruleset")]),
    t("search_test_query_ruleset", "Test a query ruleset", "Which of a ruleset's rules match these criteria (Elasticsearch's POST /_query_rules/<ruleset>/_test): check a rule before relying on it.", "search", "POST", "/v1/search/clusters/{cluster}/query_rules/{ruleset}/test", &[req("cluster", "Cluster"), req("ruleset", "Ruleset"), breq("match_criteria", Ty::Obj, "Criteria values, e.g. {\"user_query\": \"pugs\"}")]),
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
    t("secrets_project", "A project's secrets at a glance", "A project's environments (with their kind, base, protection and expiry), how many are previews', and how long old versions are kept.", "secrets", "GET", "/v1/secrets/projects/{project}", &[SECRET_PROJECT]),
    t("secrets_get_environment", "Get an environment", "One environment's settings: slug, name, kind (standard or ephemeral), base, labels, protection, approvals, expiry and max age.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}", &[SECRET_PROJECT, ENV]),
    t("secrets_ensure_environment", "Make or update an environment", "Make an environment if it isn't there, or check one that is matches (a mismatch is a 409, and changing a kind or base means deleting it first: a person's): standard, or ephemeral with a base it overlays and an expiry (a preview's or a CI run's). Protection and approvers are a person's (secrets' policy), not set here.", "secrets", "PUT", "/v1/secrets/projects/{project}/environments/{env}", &[SECRET_PROJECT, ENV, b("name", Ty::Str, "Display name"), b("kind", Ty::Str, "standard (default) or ephemeral"), b("base", Ty::Str, "Ephemeral: the environment it overlays"), b("labels", Ty::Obj, "Labels, {key: value}"), b("expires_at", Ty::Int, "Ephemeral: unix ms it's deleted at"), b("ttl_minutes", Ty::Int, "Ephemeral: or minutes from now"), b("trusted", Ty::Bool, "Ephemeral: may read a protected base (admin on it)")]),
    t("secrets_update_environment", "Rename or set an environment's expiry", "Change an environment's slug, name, expiry or max age (how old a value may get before it's flagged). A field left out stays.", "secrets", "PATCH", "/v1/secrets/projects/{project}/environments/{env}", &[SECRET_PROJECT, ENV, b("slug", Ty::Str, "New slug"), b("name", Ty::Str, "New display name"), bnull("expires_at", Ty::Int, "Unix ms it's deleted at (clear it: never)"), bnull("max_age_days", Ty::Int, "Days a value may get old (clear it: no limit)")]),
    t("secrets_delete_environment", "Delete an environment", "Delete an environment and every secret in it. A standard one is kept for the recovery window (an admin can recover it); a protected one asks a person to confirm, and where its policy asks for approvals the delete is a change request.", "secrets", "DELETE", "/v1/secrets/projects/{project}/environments/{env}", &[SECRET_PROJECT, ENV]),
    t("secrets_delete_ephemeral_environments", "Delete ephemeral environments by label", "Delete every ephemeral environment with a label (a finished CI run's, a closed preview's): standard ones are never touched.", "secrets", "DELETE", "/v1/secrets/projects/{project}/environments", &[SECRET_PROJECT, qreq("label", "key=value")]),
    t("secrets_order_environments", "Order environments", "The order environments are shown and promoted in.", "secrets", "PUT", "/v1/secrets/projects/{project}/environments/order", &[SECRET_PROJECT, breq("order", Ty::Arr, "Every environment's slug, in order")]),
    t("secrets_create_folder", "Make a folder", "A folder in an environment (/api, /api/stripe) to group keys.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/folders", &[SECRET_PROJECT, ENV, breq("path", Ty::Str, "Folder path, e.g. /api")]),
    t("secrets_delete_folder", "Delete a folder", "Delete a folder and every key in it (a change in the history).", "secrets", "DELETE", "/v1/secrets/projects/{project}/environments/{env}/folders", &[SECRET_PROJECT, ENV, qreq("path", "Folder path")]),
    t("secrets_set_imports", "Set an environment's imports", "Which environments (or other projects' shared folders) a folder of this environment reads keys from, replacing the list: [{environment, path, project?}]. Each needs read on what it imports.", "secrets", "PUT", "/v1/secrets/projects/{project}/environments/{env}/imports", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder"), P { name: "imports", ty: Ty::Arr, at: At::RawJson, required: true, desc: "[{environment, path, project?}]" }]),
    t("secrets_set_required", "Set the keys every environment needs", "The keys every standard environment of a project must define, replacing the list: [{key, path, comment, type?, pattern?, owner?, password?}] (password: how a generated value is made).", "secrets", "PUT", "/v1/secrets/projects/{project}/required", &[SECRET_PROJECT, P { name: "required", ty: Ty::Arr, at: At::RawJson, required: true, desc: "[{key, path, comment, type?, pattern?, owner?, password?}]" }]),
    t("secrets_apply_changes", "Change several keys at once", "Set and delete several keys of a folder in one change (one entry in the history): ops [{op: set, key, value?, comment?, tags?, link?, if_version?} | {op: delete, key, if_version?}]. if_seq refuses it when the environment changed since. A protected environment refuses it (409 approval_required): propose it there with secrets_propose.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/changes", &[SECRET_PROJECT, ENV, b("path", Ty::Str, "Folder"), b("message", Ty::Str, "Why"), breq("ops", Ty::Arr, "The set and delete operations"), b("if_seq", Ty::Int, "Only if the environment's last change is this")]).destructive(),
    t("secrets_revoke_share", "Revoke a share link", "Revoke one of a project's one-off share links at once (secrets_list_shares lists them): it opens nothing after.", "secrets", "DELETE", "/v1/secrets/projects/{project}/shares/{id}", &[SECRET_PROJECT, req("id", "Share id")]),
    t("secrets_create_transit_key", "Make a transit key", "A key roll keeps to encrypt and sign for an app (secrets_transit_encrypt, secrets_transit_sign and the like), whose material never leaves roll. Bringing your own material is a person's (no tool).", "secrets", "POST", "/v1/secrets/projects/{project}/transit", &[SECRET_PROJECT, breq("name", Ty::Str, "Key name")]),
    t("secrets_rotate_transit_key", "Rotate a transit key", "A new version of a transit key, made by roll: new ciphertexts use it, old ones still decrypt until min_decrypt_version moves past them.", "secrets", "POST", "/v1/secrets/projects/{project}/transit/{name}/rotate", &[SECRET_PROJECT, req("name", "Key name")]).undoable().bodiless(),
    t("secrets_update_transit_key", "Set a transit key's oldest version", "Stop older versions of a transit key from decrypting: min_decrypt_version is the oldest that still does (ciphertexts made with older ones stop opening).", "secrets", "PATCH", "/v1/secrets/projects/{project}/transit/{name}", &[SECRET_PROJECT, req("name", "Key name"), breq("min_decrypt_version", Ty::Int, "The oldest version that still decrypts")]).destructive(),
    t("secrets_backup_schedule", "A project's backup schedule", "Where and how often a project's values are saved as a sealed copy (to a recipient key, in one of its buckets): bucket, every_hours, keep, next and last run, last error. Setting or stopping it is a person's.", "secrets", "GET", "/v1/secrets/projects/{project}/backup/schedule", &[SECRET_PROJECT]),
    t("secrets_list", "List secrets", "Secret names and metadata in an environment (never values).", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/secrets", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder, e.g. /api")]),
    t("secrets_get", "Get a secret's metadata", "One secret's metadata in an environment: its type (shared or a person's own), version, link, tags, comment, where it comes from (its own, imported, a base), what it references and who changed it last (never its value: secrets_reveal reads that).", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder"), q("type", Ty::Str, "shared (default) or personal")]),
    t("secrets_grid", "Every key in every environment", "A project's keys across all its environments at once: for each key whether it's the same, different or missing between them (by fingerprint, never values), and whether it's required. Is it set everywhere, and does production differ from staging?", "secrets", "GET", "/v1/secrets/projects/{project}/grid", &[SECRET_PROJECT, q("path", Ty::Str, "Folder")]),
    t("secrets_imports", "What an environment imports", "The environments (and other projects' shared folders) an environment's folder reads keys from: where its values come from besides its own.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/imports", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder")]),
    t("secrets_exports", "Who imports a folder", "The other projects a folder of this environment is shared with, and who shared it when: where its values go besides here.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/exports", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder")]),
    t("secrets_shared", "Folders shared with a project", "The folders other projects share with this one, which its environments can import.", "secrets", "GET", "/v1/secrets/projects/{project}/shared", &[SECRET_PROJECT]),
    t("secrets_list_shares", "A project's share links", "Its one-off share links, newest first: note, views left, who made it and when, when it expires and whether it was opened (never the link or its value). Open ones are values out of roll until they expire.", "secrets", "GET", "/v1/secrets/projects/{project}/shares", &[SECRET_PROJECT]),
    t("secrets_reveal", "Read a secret's value", "Read a secret's value. A key tagged sensitive is never returned to anyone (403): only running services get it, so don't retry or look for it another way.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/value", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder"), q("resolve", Ty::Bool, "false: the value as stored, a reference as written")]),
    t("secrets_set", "Set a secret", "Create or change a secret's value. Give if_version (the version you read) so a change someone made meanwhile isn't overwritten: then it's a 409 stale, and you read it again.", "secrets", "PUT", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}", &[SECRET_PROJECT, ENV, req("name", "Secret name"), breq("value", Ty::Str, "The value"), q("path", Ty::Str, "Folder"), b("if_version", Ty::Int, "Only if it's still this version (0: only if it doesn't exist)")]),
    t("secrets_import", "Import a .env file", "Write a .env file's keys (or JSON: {\"KEY\": \"value\"} or [{key, value, comment?}]) into an environment as one change: only new keys unless overwrite, and replace makes the folder match the file (its other own keys are deleted). On a protected environment it opens a change request instead (request: true asks for one anywhere).", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/import", &[SECRET_PROJECT, ENV, b("dotenv", Ty::Str, "A .env file's text"), b("json", Ty::Obj, "Or the keys as JSON"), b("path", Ty::Str, "Folder (default /)"), b("overwrite", Ty::Bool, "Replace keys that exist"), b("replace", Ty::Bool, "Also delete the folder's keys the file lacks"), b("message", Ty::Str, "Why, for the change's history"), b("request", Ty::Bool, "Open a change request instead of writing"), b("if_seq", Ty::Int, "Only if the environment's last change is still this one")]).destructive(),
    t("secrets_rollback", "Roll back an environment", "Put an environment's values back as they were at a change (to: its seq, from secrets_changes) or a time (at: Unix ms), as one new change: what's there now is replaced. On a protected environment it opens a change request.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/rollback", &[SECRET_PROJECT, ENV, b("to", Ty::Int, "The change (seq) to go back to"), b("at", Ty::Int, "Or a time, Unix ms"), b("message", Ty::Str, "Why")]).destructive(),
    t("secrets_create_environment", "Add an environment", "A new environment in a project (staging, a preview's): slug, optional name, base (an environment it inherits from), labels, and for an ephemeral one ttl_minutes. Making one is the project admin's.", "secrets", "POST", "/v1/secrets/projects/{project}/environments", &[SECRET_PROJECT, breq("slug", Ty::Str, "Short name (staging, pr-42)"), b("name", Ty::Str, "Display name"), b("kind", Ty::Str, "standard or ephemeral"), b("base", Ty::Str, "Environment it inherits from"), b("labels", Ty::Obj, "Labels, {key: value}"), b("ttl_minutes", Ty::Int, "An ephemeral one ends after this")]),
    t("secrets_link_targets", "List what keys can link to", "The project's resources a key can be linked to (a database, a bucket, a stream…) and each one's fields, for secrets_link.", "secrets", "GET", "/v1/secrets/projects/{project}/links", &[SECRET_PROJECT]),
    t("secrets_link", "Link a key to a resource", "Make a key read its value from a project resource instead of holding one: link {kind, ref, field, mode} from secrets_link_targets (mode: live reads it on every resolve, managed issues and stores one, dynamic issues one per resolve and revokes it when its lease ends). link: null unlinks.", "secrets", "PUT", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}", &[SECRET_PROJECT, ENV, req("name", "Secret name"), breq("link", Ty::Obj, "{kind, ref, field, mode, options?}, or null"), q("path", Ty::Str, "Folder"), b("if_version", Ty::Int, "Only if it's still this version")]),
    t("secrets_delete", "Delete a secret", "Delete a secret (if_version: only if it's still the version you read).", "secrets", "DELETE", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder"), q("if_version", Ty::Int, "Only if it's still this version"), q("type", Ty::Str, "personal: your own override instead of the shared value")]),
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
    t("secrets_get_change", "Get a secrets change", "One change of an environment (seq, from secrets_changes): who, when, why, and what it did to each key (fingerprints, never values); state: true adds every key as it was after it.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/changes/{seq}", &[SECRET_PROJECT, ENV, req_int("seq", "The change's seq"), q("state", Ty::Bool, "Every key as it was after it")]),
    t("secrets_insights", "Secret insights", "Each key in a folder: its version, who changed it and when, its last read and reads in 30 days, and whether it's stale (unchanged past the environment's max age) or expiring. Which keys nobody uses, which to rotate.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/insights", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder")]),
    t("secrets_verify_audit", "Is a project's secrets audit log intact", "Walk a project's secrets audit chain and say whether it holds, or was tampered with: the rows walked, how far roll's signed digests vouch for them, and the first break (edited, missing, rewritten, signature, truncated, cut, unchained, head, stale or gap) with where. Use it after an incident, or before trusting the log's history.", "secrets", "GET", "/v1/secrets/projects/{project}/audit/verify", &[SECRET_PROJECT, q("since", Ty::Int, "From the last digest before this time (unix ms; the chain's start when left out)")]),
    t("secrets_audit", "Secrets audit log: who did what, when", "Who read or changed what in a project's secrets, newest first (its audit log): each entry's actor, action, environment, key, when and from where (never a value). Filter by env, key, actor or action (e.g. secret.read); before pages back. Answers \"who read this secret\" after an incident.", "secrets", "GET", "/v1/secrets/projects/{project}/audit", &[SECRET_PROJECT, q("env", Ty::Str, "Only this environment"), q("key", Ty::Str, "Only this key"), q("actor", Ty::Str, "Only this actor (a person's handle, a token or an identity)"), q("action", Ty::Str, "Only this action, e.g. secret.read, secret.set"), q("since", Ty::Int, "From this time (unix ms)"), q("before", Ty::Int, "Older than this time (unix ms): the next page"), q("limit", Ty::Int, "At most this many"), q("digests", Ty::Bool, "false leaves out the signed digest rows")]),
    t("secrets_required", "Required keys", "The keys every standard environment of a project must define (and their types or patterns, when set): what an environment is missing before it runs.", "secrets", "GET", "/v1/secrets/projects/{project}/required", &[SECRET_PROJECT]),
    t("secrets_folders", "List folders", "An environment's folders under a path.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/folders", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Under this folder (default /)")]),
    t("secrets_suggestions", "Suggested links", "Links roll suggests for a project's resources (a database, a bucket…): which keys each would fill and where, for secrets_link.", "secrets", "GET", "/v1/secrets/projects/{project}/suggestions", &[SECRET_PROJECT]),
    t("secrets_versions", "A secret's versions", "A secret's history, newest first (fingerprints, who and when; never values).", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/versions", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder"), q("type", Ty::Str, "personal: your own override instead of the shared value")]),
    t("secrets_compare", "Compare environments", "Which keys differ between two environments (missing, or a different value by fingerprint; never values).", "secrets", "GET", "/v1/secrets/projects/{project}/compare", &[SECRET_PROJECT, qreq("from", "Environment"), qreq("to", "Environment"), q("path", Ty::Str, "Folder")]),
    t("secrets_rotate", "Rotate a secret", "Make a new value for a secret and switch to it: a generated one, or a managed link's (a database user's password, a cloud key) made where it lives.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/secrets/{name}/rotate", &[SECRET_PROJECT, ENV, req("name", "Secret name"), q("path", Ty::Str, "Folder")]).open_world().destructive(),
    t("secrets_promote", "Promote secrets", "Copy an environment's secrets into this one (all, or some keys). Into a protected environment it opens a change request for a person instead.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/promote", &[SECRET_PROJECT, ENV, qreq("from", "The environment to copy from"), b("keys", Ty::Arr, "Only these keys"), b("path", Ty::Str, "Folder"), b("message", Ty::Str, "Why")]),
    t("secrets_list_requests", "Change requests", "An environment's change requests (proposed changes waiting on review).", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/requests", &[SECRET_PROJECT, ENV, q("state", Ty::Str, "open, applied or closed")]),
    t("secrets_get_request", "Get a change request", "One change request on a protected environment: what it changes (keys, fingerprints, never values), who asked, its reviews and whether its approvals are met.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/requests/{id}", &[SECRET_PROJECT, ENV, req("id", "Request id")]),
    t("secrets_apply_request", "Apply an approved change request", "Apply a change request whose approvals are met, as one change. Applying one that isn't approved (bypass) is a person's: not here.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/requests/{id}/apply", &[SECRET_PROJECT, ENV, req("id", "Request id")]),
    t("secrets_close_request", "Close a change request", "Withdraw a change request without applying it.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/requests/{id}/close", &[SECRET_PROJECT, ENV, req("id", "Request id")]),
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
        &[SECRET_PROJECT, ENV, req("name", "Secret name"), req_int("v", "Version number"), q("path", Ty::Str, "Folder"), q("type", Ty::Str, "personal: your own override instead of the shared value")],
    ),
    t("secrets_request_value", "Ask a person for an API key or other secret value", "When you need a value you don't have (a vendor's API key, a webhook secret), don't ask for it in the chat: this answers a link where the person pastes it, sealed in their browser and written straight into the folder, so it never passes through the conversation. The link is set when roll knows the dashboard's address; the token (rlq_…) always comes back, and the page is <dashboard>/request/<token>. Give the link or token only to the person it's for, in a message to them: whoever opens it first can send values for these keys, and roll refuses to write either where others read (a repo, an issue, a comment). By default it takes one answer and expires in a day. An answer lands as a change by request:<id> on behalf of whoever asked (a change request in a protected environment), so it can be seen and rolled back. Follow it with secrets_list_value_requests.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/value-requests", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder, e.g. /api (the root when left out)"), breq("keys", Ty::Arr, "1-20 key names to ask for"), b("note", Ty::Str, "What it's for, shown on the page (never the project or who asked)"), b("ttl_minutes", Ty::Int, "Minutes until it expires (5-43200; default 1440, a day)"), b("answers", Ty::Int, "How many times it may be answered (1-10; default 1)")]).fixed(value_request_defaults),
    t("secrets_list_value_requests", "List secret value requests", "A project's value requests, newest first: keys, folder, whether each is open, answered, expired or cancelled, and what an answer became (a change or a change request). Never a value.", "secrets", "GET", "/v1/secrets/projects/{project}/value-requests", &[SECRET_PROJECT]),
    t("secrets_cancel_value_request", "Cancel a secret value request", "Close a value request so its link stops working (whoever asked, or an admin of the folder).", "secrets", "DELETE", "/v1/secrets/projects/{project}/value-requests/{id}", &[SECRET_PROJECT, req("id", "Value request id")]),
    t("secrets_renew_lease", "Renew a lease", "Push a dynamic credential's expiry out, as a long-running consumer does instead of restarting.", "secrets", "POST", "/v1/secrets/projects/{project}/leases/{id}/renew", &[SECRET_PROJECT, req("id", "Lease id"), b("ttl_minutes", Ty::Int, "Minutes from now")]),
    t("secrets_reach", "Who reaches a secret", "Which tokens and identities reach an environment's folder or key, whether code nobody vetted can, where it flows (syncs, shares, imports) and who read it in 30 days: names only. Who has to rotate what after a leak (the project admin's).", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/reach", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder"), q("key", Ty::Str, "One key")]),
    t("secrets_folder_policies", "Folder approval policies", "An environment's approval policies per folder: who must approve a change there.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/folder-policies", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder")]),
    t("secrets_list_rotators", "Rotators", "A project's rotators: which secrets roll rotates (a database password, an API key), how often and from what.", "secrets", "GET", "/v1/secrets/projects/{project}/rotators", &[SECRET_PROJECT]),
    t("secrets_list_syncs", "Syncs", "Where a project's environments are pushed (GitHub, Vercel, AWS, Kubernetes…) and how each last went.", "secrets", "GET", "/v1/secrets/projects/{project}/syncs", &[SECRET_PROJECT]),
    t("secrets_diff", "Compare a local .env", "Compare a local .env (dotenv) or JSON with an environment by fingerprint: names added, changed, the same, missing; never a value.", "secrets", "POST", "/v1/secrets/projects/{project}/environments/{env}/diff", &[SECRET_PROJECT, ENV, b("dotenv", Ty::Str, "The file's text"), b("json", Ty::Obj, "{NAME: value}"), b("path", Ty::Str, "Folder")]),
    t("secrets_list_rotations", "Rotations", "A project's secret rotations: what rotated, when, and what's due.", "secrets", "GET", "/v1/secrets/projects/{project}/rotations", &[req("project", "Workspace project")]),
    t("secrets_ca_certificates", "A CA's certificates", "The certificates a project's CA has issued (public, with their expiry; never a key).", "secrets", "GET", "/v1/secrets/projects/{project}/cas/{ca}/certificates", &[req("project", "Workspace project"), req("ca", "CA id")]),
    t("secrets_get_values", "Reveal an environment's values", "All of an environment's values (a folder's, with path) at once, resolved (references and imports), as one reveal instead of one per key; what can't be resolved is named, never guessed. dynamic=false leaves dynamic keys out instead of minting fresh credentials. Needs reveal.", "secrets", "GET", "/v1/secrets/projects/{project}/environments/{env}/values", &[SECRET_PROJECT, ENV, q("path", Ty::Str, "Folder"), q("dynamic", Ty::Bool, "false: leave dynamic keys out (named in dynamic)"), q("seq", Ty::Int, "As stored after this change (from secrets_changes): what the folder had then; audited as an export"), q("at", Ty::Int, "As stored at this time (unix seconds), as seq does"), q("strict", Ty::Bool, "Any problem is a 409 naming the keys, instead of values with problems"), q("resolve", Ty::Bool, "false: values as stored, references as written")]),
    t("secrets_list_placeholders", "List placeholders", "The placeholders a project's host-bound keys were handed out as (a sandbox or an agent uses the key without seeing it: the value is swapped in only on the way to the key's hosts), newest first: key, environment, consumer, who issued it, when it expires or was ended. Never the placeholder or the value.", "secrets", "GET", "/v1/secrets/projects/{project}/placeholders", &[SECRET_PROJECT]),
    t("secrets_list_recoverable", "List deleted projects you can recover", "Workspace projects deleted lately whose secrets are kept for a recovery window, as far as you may recover them: who deleted each, when, and when it goes for good (soonest first). Recovering one is a person's, in the dashboard or roll secrets recover.", "secrets", "GET", "/v1/secrets/deleted", &[]),
    t("secrets_end_placeholder", "End a placeholder", "End one placeholder now: its value is no longer swapped in for anyone holding it (409 when it already ended).", "secrets", "DELETE", "/v1/secrets/projects/{project}/placeholders/{id}", &[SECRET_PROJECT, req("id", "Placeholder id (from secrets_list_placeholders)")]),
    t("secrets_end_consumer_placeholders", "End a consumer's placeholders", "End every live placeholder one consumer (a sandbox, a preview, an agent's run) holds that you may end, at once; answers how many ended.", "secrets", "DELETE", "/v1/secrets/projects/{project}/placeholders", &[SECRET_PROJECT, qreq("consumer", "The consumer, as secrets_list_placeholders names it")]),
    t("secrets_list_leases", "List leases", "A project's live leases on dynamic credentials (a database user, a cloud key made for a reader): who holds each, for what, and when it ends. Never a credential.", "secrets", "GET", "/v1/secrets/projects/{project}/leases", &[SECRET_PROJECT]),
    t("secrets_end_lease", "End a lease", "End a dynamic credential's lease now (the credential is revoked where it lives): yours once you're done with it, or anyone's with more than reveal.", "secrets", "DELETE", "/v1/secrets/projects/{project}/leases/{id}", &[SECRET_PROJECT, req("id", "Lease id")]),
    t("secrets_credential_check", "Is a key found in code still live?", "A credential found in code that the project doesn't hold (by its fingerprint, from roll code's unstored notice or roll secrets scan): its kind, whether it was live when last checked (live, invalid or unknown), the account it belongs to and the rotator that could replace it. Never its value; 404 when it hasn't been checked.", "secrets", "GET", "/v1/secrets/projects/{project}/unstored/{fingerprint}", &[SECRET_PROJECT, req("fingerprint", "The credential's fingerprint")]),
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
    ).open_world(),
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
        bnull("routes", Ty::Arr, "[{path, service}] (at most 20): a path (a prefix ends in *) another service of the project answers, by name; clear it for none"),
        bnull("egress", Ty::Any, "{mode: roll|open, hosts?}: roll lets its code reach roll and only these hosts (at most 20, *. for subdomains), from the next deploy; clear it for open"),
        b("vitals", Ty::Bool, "Measure its pages' Web Vitals in visitors' browsers (read them with apps_vitals)"),
        bnull("config", Ty::Obj, "Runtime config its instances read at ROLL_CONFIG_URL (edge functions: context.config) without a deploy: at most 200 keys, 64 KiB; the first one needs a deploy to give them the address. Clear it for none"),
        bnull("freeze", Ty::Obj, "A deploy freeze: {until? (unix ms), windows?: [{from: \"fri 17:00\", to}], timezone?, reason?}, on until cleared when it has neither until nor windows. Setting or clearing one is an admin's call (clearing it would get past it)"),
        b("traces", Ty::Bool, "Send its OpenTelemetry traces and metrics to roll (OTEL_EXPORTER_OTLP_* from the next deploy)"),
        b("redeploy_on_secrets", Ty::Bool, "Redeploy (the same image, new values) when the project's roll secrets change"),
        b("pages", Ty::Obj, "{error?, maintenance?}: its own HTML for roll's error and maintenance pages (a page set to null puts roll's back)"),
        bnull("headers", Ty::Any, "Response header rules: [{path?, set: {name: value}, replace?}] (at most 20); clear it for none"),
        b("paused", Ty::Bool, "Pause it (no deploys on push, nothing running) or resume it"),
        // A rollback holds deploys (apps #26): false lets pushes and hooks deploy again.
        b("held", Ty::Bool, "false: resume deploys a rollback held (held_after shows since when)"),
        b("hostname", Ty::Str, "Its new name on the roll domain (<hostname>.<domain>); the old one stops answering"),
        b("report", Ty::Bool, "false: its deploys post no commit status and no GitHub Deployment (something else reports them)"),
        b("scheduled", Ty::Bool, "false: its schedules (a cron's, run.schedules) don't run; deploys and runs still go"),
        bnull("data", Ty::Str, "\"branch\": data of its own, a branch of each Postgres and Redis database it links in place of the databases (admin; not production or a preview); clear it to go back, deleting the branches"),
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
            OVERRIDE,
            IDEMPOTENCY,
        ],
    ).open_world(),
    t("apps_list_deploys", "List deploys", "An environment's deploys, newest first, 100 at a time (before: the next page).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/deploys", &[SERVICE, ENV, q("before", Ty::Int, "The next page: the last one's created_at (unix ms)")]),
    t("apps_deploy_logs", "Deploy logs", "A deploy's build and release log.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/logs", &[SERVICE, ENV, req("deploy", "Deploy id"), q("tail", Ty::Int, "Last lines")]),
    t(
        "apps_promote",
        "Promote",
        "Deploy the image another environment is running into this one, with this environment's variables and no build (production is a \"deploy\" action).",
        "apps",
        "POST",
        "/v1/apps/services/{service}/environments/{env}/promote",
        &[SERVICE, ENV, breq("from", Ty::Str, "The environment to take it from"), b("message", Ty::Str, "Why"), b("expect", Ty::Str, "The deployment id you expect live in from (409 if it moved: promote what you checked)"), OVERRIDE],
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
    t("apps_redeploy", "Redeploy", "Deploy what's running again with the current settings and variables (production is a \"deploy\" action).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/redeploy", &[SERVICE, ENV, b("message", Ty::Str, "Why"), b("closed", Ty::Bool, "Refuse (409) unless the environment builds closed"), OVERRIDE]),
    t(
        "apps_run_job",
        "Run a job",
        "Run a one-off command in an environment's image with its variables (a migration, a script); read its output with apps_job_logs. Production is a \"deploy\" action.",
        "apps",
        "POST",
        "/v1/apps/services/{service}/environments/{env}/jobs",
        &[SERVICE, ENV, breq("command", Ty::Str, "The command line"), b("timeout_s", Ty::Int, "Seconds before it's stopped"), b("tasks", Ty::Int, "Containers to run it in, 1-100 (each gets ROLL_TASK_INDEX and ROLL_TASK_COUNT)"), b("parallelism", Ty::Int, "Tasks at a time, 1-10"), b("retries", Ty::Int, "Tries again for a failed or timed-out task, 0-5"), b("cpu", Ty::Num, "Each container's vCPUs, 0.25-8 (default: the service's)"), b("memory_mb", Ty::Int, "Each container's memory in MiB, 256-16384 (default: the service's)")],
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
    t(
        "apps_set_private_network",
        "Connect a private network",
        "Reach hosts in your own network (Cloud Run's VPC egress, Fly's WireGuard, a Tailscale tailnet): a WireGuard config, or a Tailscale auth key (kind: tailscale; login_server for Headscale), kept in roll secrets as this environment's secret (secret: roll://ENV/KEY, read as you at each use; PostUp, DNS, Table and full tunnels are refused) and routes, each a name the app connects to (db.acme.corp) and a host:port in your network (a MagicDNS name on a tailnet); a peer answers each name on the project network. Each route has its own port; a closed environment reaches these and still nothing else (admin).",
        "apps",
        "PUT",
        "/v1/apps/services/{service}/environments/{env}/private-network",
        &[SERVICE, ENV, breq("secret", Ty::Str, "roll://ENV/KEY: the WireGuard config ([Interface] with Address, [Peer]s)"), breq("routes", Ty::Arr, "[{name: db.acme.corp, target: 10.0.3.4:5432}], 1-20, each port its own"), b("kind", Ty::Str, "wireguard (the default) or tailscale"), b("login_server", Ty::Str, "With tailscale: the control server's URL when it isn't Tailscale's (Headscale)")],
    )
    .open_world(),
    t("apps_get_private_network", "Get the private network", "An environment's private network: its secret's reference (never the config), routes, and whether its peer runs (running, failed: why, waiting for a deploy).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/private-network", &[SERVICE, ENV]),
    t("apps_delete_private_network", "Disconnect the private network", "Remove an environment's private network: its peer and routes stop now (admin).", "apps", "DELETE", "/v1/apps/services/{service}/environments/{env}/private-network", &[SERVICE, ENV]).destructive(),
    t(
        "apps_create_sandbox",
        "Start a sandbox",
        "A short-lived container to run commands in (Vercel Sandbox, Fly Machines): the environment's live image or a public one, up for ttl_s (60-86400, default 600), at most 10 per environment. By default it gets none of the environment's variables or files and a network of its own (the internet, nothing of the project); variables: true gives it the live deployment's, network: \"environment\" the environment's network. A closed environment's are always on its network. Run commands with apps_sandbox_exec; it's removed at its end or with apps_delete_sandbox (admin).",
        "apps",
        "POST",
        "/v1/apps/services/{service}/environments/{env}/sandboxes",
        &[SERVICE, ENV, b("image", Ty::Str, "A public image instead of the live one (python:3.13-slim)"), b("ttl_s", Ty::Int, "Seconds it stays up, 60-86400"), b("cpu", Ty::Num, "vCPUs, 0.25-4 (default 0.5)"), b("memory_mb", Ty::Int, "MiB, 256-8192 (default 512)"), b("variables", Ty::Bool, "The live deployment's variables and files"), b("network", Ty::Str, "none (its own: the internet) or environment")],
    ).open_world(),
    t("apps_list_sandboxes", "List sandboxes", "An environment's sandboxes: image, size, whether they have its variables, when each ends.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/sandboxes", &[SERVICE, ENV]),
    t("apps_get_sandbox", "Get a sandbox", "One sandbox: image, size, network, when it ends.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/sandboxes/{sandbox}", &[SERVICE, ENV, req("sandbox", "Sandbox id (sbx_…)")]),
    t(
        "apps_sandbox_exec",
        "Run a command in a sandbox",
        "sh -c <command> in a sandbox: its exit code, stdout and stderr (each up to 1 MiB, truncated says when more was dropped); timed_out when it ran past timeout_s and was killed. Files are written and read with commands too (cat, a heredoc). Admin.",
        "apps",
        "POST",
        "/v1/apps/services/{service}/environments/{env}/sandboxes/{sandbox}/exec",
        &[SERVICE, ENV, req("sandbox", "Sandbox id (sbx_…)"), breq("command", Ty::Str, "A shell command line"), b("timeout_s", Ty::Int, "Seconds before it's killed, 1-600 (default 60)")],
    ).open_world(),
    t("apps_delete_sandbox", "Delete a sandbox", "Stop and remove a sandbox now (admin).", "apps", "DELETE", "/v1/apps/services/{service}/environments/{env}/sandboxes/{sandbox}", &[SERVICE, ENV, req("sandbox", "Sandbox id (sbx_…)")]).destructive(),
    t("apps_cancel_job", "Cancel a job", "Stop a running job (production is a \"deploy\" action).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/jobs/{job}/cancel", &[SERVICE, ENV, req("job", "Job id")]),
    t("apps_events", "App events", "What happened to services (deploys going live or failing, crash loops, failed cron runs and jobs, memory, error and latency alerts, and audit.* changes to settings, variables, domains and links), in order after a cursor.", "apps", "GET", "/v1/apps/events", &[q("after", Ty::Int, "The last answer's next"), q("limit", Ty::Int, "At most this many")]).outside(&["fields"]),
    t("apps_list_variables", "List variables", "An environment's variables, values masked. A sealed variable is never readable again by anyone (only the running service gets it): set a new value or delete it, never look for the old one.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/variables", &[SERVICE, ENV]),
    t(
        "apps_set_variables",
        "Set variables",
        "Set or unset an environment's variables, or copy some from another environment (copy_from with copy: their values, never shown; needs reveal on that environment too); seal some, keep some as files, or keep some out of builds. A value roll://ENV[/folder…]/KEY is read from roll secrets at each deploy. Production is a \"deploy\" action.",
        "apps",
        "PATCH",
        "/v1/apps/services/{service}/environments/{env}/variables",
        &[SERVICE, ENV, b("set", Ty::Obj, "{NAME: value}"), b("unset", Ty::Arr, "Names to remove"), b("copy_from", Ty::Str, "Another environment to copy values from"), b("copy", Ty::Arr, "Names to copy from copy_from"), b("seal", Ty::Arr, "Names to seal (set here or already set): their values are never shown again, not even to reveal; nothing unseals"), b("files", Ty::Obj, "{NAME: absolute path, or null to put it back}: kept as a read-only file at that path in its instances from the next deploy, not in their environment (at most 20; not under /proc, /sys or /dev)"), b("runtime_only", Ty::Obj, "{NAME: true}: only its instances get it, never a build's steps, from the next deploy (false: builds get it again)")],
    ),
    t("apps_diff_environments", "Compare environments", "How an environment differs from another (Railway's environment sync): variables missing, extra or with another value (names only; which differ needs reveal on both), and settings with the body that makes each match. Read on both environments.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/diff", &[SERVICE, ENV, qreq("from", "The environment to compare with")]),
    t("apps_metrics", "Service metrics", "CPU, memory and requests of an environment, and each instance's own last hour (by_instance).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/metrics", &[SERVICE, ENV, q("minutes", Ty::Int, "Window")]),
    t("apps_plan", "Plan a service", "How roll would build and run a repo (provider, framework, install, build and start commands, port), before making a service: a dry run.", "apps", "POST", "/v1/apps/plan", &[breq("repo", Ty::Str, "owner/name"), b("revision", Ty::Str, "A branch or commit"), b("root", Ty::Str, "The app's folder in the repo")]),
    t("apps_import", "Check a project file", "What a roll.toml, a docker-compose file or another platform's file (Render, Fly, Railway, Heroku) would make, without making anything: the apps with their source, branch, run settings, links and variables, the databases, notes on what isn't carried over, and the file as roll.toml (check it before roll up --all). Give one: a file's text, a roll code repo's root roll.toml, or a public Git repository's.", "apps", "POST", "/v1/apps/import", &[b("roll_toml", Ty::Str, "A roll.toml's text"), b("compose", Ty::Str, "A docker-compose.yml's text"), b("render", Ty::Str, "A Render Blueprint's render.yaml text"), b("fly", Ty::Str, "A fly.toml's text"), b("railway", Ty::Str, "A railway.json or railway.toml's text"), b("heroku", Ty::Str, "A Heroku app.json's text"), b("procfile", Ty::Str, "With heroku: its Procfile's text"), b("repo", Ty::Str, "A roll code repo (owner/name): its root roll.toml"), b("revision", Ty::Str, "With repo: a branch or commit (default: its default branch's head)"), b("git", Ty::Str, "A public Git repository's https URL: its root roll.toml"), b("branch", Ty::Str, "With git: a branch (default: its default branch)")]).open_world(),
    t("apps_export_project", "Export a project file", "A project as a roll.toml: its services (source, branch, run settings that differ from defaults) and the postgres and redis databases they link, variables by name only, never a value. Write it to the repo, check it with apps_import, deploy it with roll up --all.", "apps", "POST", "/v1/apps/projects/{project}/export", &[req("project", "Workspace project"), b("databases", Ty::Obj, "Names for the linked databases, as {\"postgres:<ref>\": \"db\"}")]),
    t("apps_get_environment", "Get an environment", "One environment: its URL, state (running, asleep), live deploy, branch and size; build_closed (its builds get no secret value) and base_updated (base images that moved since its build).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}", &[SERVICE, ENV]),
    t("apps_delete_environment", "Delete an environment", "Delete an environment with its deploys, instances and domains. Irreversible.", "apps", "DELETE", "/v1/apps/services/{service}/environments/{env}", &[SERVICE, ENV]),
    t("apps_link", "Link a resource", "Give a service a roll resource's connection variables (DATABASE_URL and the like, with prefix if given): kind is postgres, redis, bucket, stream, queue, nosql or search (a search cluster: a data-only key of the link's own, narrowed with indices and access: a SaaS's tenants' indices, a frontend's read-only key, a log shipper's append-only one; changing them makes a new key and revokes the old). Needs reveal on the resource; the next deploy picks it up.", "apps", "PUT", "/v1/apps/services/{service}/links/{kind}/{ref}", &[SERVICE, req("kind", "postgres, redis, bucket, stream, queue, nosql or search"), req("ref", "The resource's id or name"), b("prefix", Ty::Str, "Variable name prefix"), b("indices", Ty::Arr, "Search only: the key's index names or patterns (tenant-*)"), b("access", Ty::Str, "Search only: read, or append (create documents on exact names); none: read and write documents")]),
    t("apps_unlink", "Unlink a resource", "Stop giving a service a linked resource's variables (from the next deploy).", "apps", "DELETE", "/v1/apps/services/{service}/links/{kind}/{ref}", &[SERVICE, req("kind", "Link kind"), req("ref", "The resource's id or name")]),
    t("apps_verify_domain", "Check a domain", "Check a custom domain's DNS now and answer its state (pending, verified) and the record to add.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/domains/{host}/verify", &[SERVICE, ENV, req("host", "The domain")]),
    t("apps_list_backups", "Volume backups", "An environment's volume backups, newest first (7 kept).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/volume/backups", &[SERVICE, ENV]),
    t("apps_backup_volume", "Back up a volume", "Back up an environment's volume now.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/volume/backups", &[SERVICE, ENV]),
    t("apps_restore_backup", "Restore a volume backup", "Put an environment's volume back as it was in a backup, replacing its files (what's there now is backed up first); the environment sleeps meanwhile. With from, the backup is another environment's (seeding staging from production; needs reveal there too).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/volume/backups/{backup}/restore", &[SERVICE, ENV, req("backup", "Backup id"), b("from", Ty::Str, "The environment whose backup it is (default: this one)")]),
    t("apps_activity", "Service activity", "Who changed what on a service, newest first (never a value); with all, its whole timeline: deploys, failures, crash loops, alerts, domains, spend.", "apps", "GET", "/v1/apps/services/{service}/activity", &[SERVICE, q("all", Ty::Bool, "The whole timeline, not only changes")]).outside(&["fields"]),
    t("apps_get_deploy", "Get a deploy", "One deploy: its status, commit, image and when it went live.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}", &[SERVICE, ENV, req("deploy", "Deploy id")]),
    t(
        "apps_wait_deploy",
        "Wait for a deploy to finish",
        "A deploy once it's live or failed, with how far each step got, or built and waiting at ready for people to approve it where the environment asks for approvals (approved_by and the count it needs: a person approves it in roll, never a token). A client that runs MCP tasks gets a task that waits as long as it takes; otherwise this answers the status now (call again to keep waiting).",
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
    t("apps_push_status", "Stream push status", "An environment's run.push rules as they stand: each one's lag (records not yet delivered), delivered and dead-lettered counts, the last delivery, and failing_since and last_error when it's failing, or why nothing is delivered (off). Why a pushed stream is behind.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/push", &[SERVICE, ENV]),
    t("apps_set_error_state", "Resolve, mute or reopen an error", "Triage an error group (its fingerprint from apps_errors): resolved goes quiet until it's seen again, which reopens it as a regression naming the deploy it came back in; muted stays quiet; open reopens it.", "apps", "PATCH", "/v1/apps/services/{service}/environments/{env}/errors/{fingerprint}", &[SERVICE, ENV, req("fingerprint", "The error group's fingerprint"), breq("state", Ty::Str, "open, resolved or muted")]),
    t("apps_rotate_config", "Rotate the runtime config's address", "Give an environment's runtime config a new read token (a leaked ROLL_CONFIG_URL): the old address reads until a deploy made after the rotation is live, so redeploy to finish it. 409 while the last rotation's old address still reads.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/config/rotate", &[SERVICE, ENV]).destructive(),
    t("apps_traces", "Traces of slow requests", "An environment's recent traces (traces on), newest first, at most 100: each one's root span, service, duration, span count and whether any span failed; min_ms keeps the slow ones. apps_get_trace has one's spans.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/traces", &[SERVICE, ENV, q("minutes", Ty::Int, "How far back (default 60)"), q("min_ms", Ty::Int, "Only traces at least this slow")]),
    t("apps_get_trace", "Get a trace", "One trace by its id (from apps_traces): its spans in start order, from the last week.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/traces/{trace}", &[SERVICE, ENV, req("trace", "Trace id")]),
    t("apps_app_metrics", "App metrics", "What the app's OpenTelemetry SDK sent as metrics (traces on) over the last minutes (default 60): each series (a name and its attributes, the busiest 50) as at most 60 points.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/app-metrics", &[SERVICE, ENV, q("minutes", Ty::Int, "How far back (default 60)")]),
    t("apps_analytics", "Web analytics", "An environment's page views and visitors from its access log (no script, no cookie) over the last minutes (default 1440): top pages, referring sites, countries and a series.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/analytics", &[SERVICE, ENV, q("minutes", Ty::Int, "How far back (default 1440)")]),
    t("apps_list_cron_runs", "Cron runs", "A cron service's runs, newest first.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/runs", &[SERVICE, ENV]),
    t("apps_get_cron_run", "Get a cron run", "One cron run: its status, exit code, start and end.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/runs/{run}", &[SERVICE, ENV, req("run", "Run id")]),
    t("apps_cron_run_logs", "A cron run's logs", "A cron run's output (its lines in the environment's log).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/runs/{run}/logs", &[SERVICE, ENV, req("run", "Run id"), q("tail", Ty::Int, "Last lines")]),
    t("apps_cancel_cron_run", "Cancel a cron run", "Stop a running cron run (production is a \"deploy\" action).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/runs/{run}/cancel", &[SERVICE, ENV, req("run", "Run id")]),
    t("apps_run_cron", "Run a cron now", "Run a cron service's job now (in production this is a deploy action).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/runs", &[SERVICE, ENV]),
    t("apps_limits", "Spend limits", "A project's apps spend limits and how much of them is used.", "apps", "GET", "/v1/apps/projects/{project}/limits", &[req("project", "Workspace project")]),
    t("apps_project_usage", "A project's apps cost by service", "What a project's apps used and cost in a month (YYYY-MM, this one by default) by service and environment at roll's prices, most costly first: CPU and memory time, requests, egress and build time. Each service's previews are one row, deleted services one; this month's total is projected to its end at the pace so far. Which service costs the most starts here.", "apps", "GET", "/v1/apps/projects/{project}/usage", &[req("project", "Workspace project"), q("month", Ty::Str, "YYYY-MM (this month by default; not one to come)")]),
    t("apps_set_limits", "Set spend limits", "A project's monthly apps spend: alert_usd notifies, hard_usd pauses the whole project once reached (null: none). An admin's call.", "apps", "PUT", "/v1/apps/projects/{project}/limits", &[req("project", "Workspace project"), bnull("alert_usd", Ty::Num, "Notify past this many dollars a month (left out or cleared: no alert; both are set each time)"), bnull("hard_usd", Ty::Num, "Pause the project past this (left out or cleared: no cap; both are set each time)")]),
    t("apps_overview", "roll apps here", "What roll apps runs here: its domain, runtime, prices, and every service with its environments' state.", "apps", "GET", "/v1/apps", &[]),
    t("apps_list_project_services", "List a project's services", "The services in one workspace project.", "apps", "GET", "/v1/apps/projects/{project}/services", &[req("project", "Workspace project")]),
    t("apps_list_notifications", "List notifications", "Where a project's deploy, crash and preview events are sent (webhook, Slack, Discord, email; the URL's host only).", "apps", "GET", "/v1/apps/projects/{project}/notifications", &[req("project", "Workspace project")]),
    t(
        "apps_add_notification",
        "Notify Slack, Discord, email or a webhook",
        "Notify a webhook, Slack, Discord, the project's owner by email, PagerDuty or Opsgenie of a project's events (a URL or key is stored sealed and never shown again).",
        "apps",
        "POST",
        "/v1/apps/projects/{project}/notifications",
        &[req("project", "Workspace project"), b("kind", Ty::Str, "webhook (default), slack, discord, email (to the project's owner), pagerduty or opsgenie"), b("url", Ty::Str, "Where to send, for a webhook, Slack or Discord (email, pagerduty and opsgenie take none)"), b("routing_key", Ty::Str, "pagerduty: its integration's routing key"), b("api_key", Ty::Str, "opsgenie: its API key"), b("region", Ty::Str, "pagerduty or opsgenie: us (default) or eu"), b("events", Ty::Arr, "Which events (default: deploys and crashes)"), b("environments", Ty::Arr, "Only these environments"), b("services", Ty::Arr, "Only these services"), b("previews", Ty::Bool, "Previews' events too")],
    ).open_world(),
    t("apps_update_notification", "Change a notification", "Change which events, environments and services a notification gets: given ones replace.", "apps", "PATCH", "/v1/apps/projects/{project}/notifications/{nid}", &[req("project", "Workspace project"), req("nid", "Notification id"), b("events", Ty::Arr, "Events"), b("environments", Ty::Arr, "Environments"), b("services", Ty::Arr, "Services"), b("previews", Ty::Bool, "Previews' events too")]),
    t("apps_delete_notification", "Delete a notification", "Stop sending a project's events there.", "apps", "DELETE", "/v1/apps/projects/{project}/notifications/{nid}", &[req("project", "Workspace project"), req("nid", "Notification id")]),
    t("apps_test_notification", "Test a notification", "Send a test event now (not retried): whether it arrived.", "apps", "POST", "/v1/apps/projects/{project}/notifications/{nid}/test", &[req("project", "Workspace project"), req("nid", "Notification id")]).open_world(),
    t("apps_notification_deliveries", "Notification deliveries", "A notification's last 50 deliveries, newest first: what was sent and whether it arrived.", "apps", "GET", "/v1/apps/projects/{project}/notifications/{nid}/deliveries", &[req("project", "Workspace project"), req("nid", "Notification id")]),
    t("apps_list_drains", "List log drains", "Where a project's logs are sent (the URL's host only).", "apps", "GET", "/v1/apps/projects/{project}/drains", &[req("project", "Workspace project")]),
    t(
        "apps_add_drain",
        "Send or archive logs (add a log drain)",
        "Send every matching service's logs out of roll: to an HTTPS endpoint, signed (its signing secret comes back once), an OTLP collector, syslog over TLS, or archived to an S3 bucket (any S3-compatible endpoint, or with bucket one of the project's own roll storage buckets, nothing to paste).",
        "apps",
        "POST",
        "/v1/apps/projects/{project}/drains",
        &[req("project", "Workspace project"), b("url", Ty::Str, "https URL on a public host (not with bucket)"), b("bucket", Ty::Str, "A roll storage bucket on the project's canvas instead of a url: logs archived there with a key minted for the drain (no headers or format)"), b("prefix", Ty::Str, "With bucket: where in it"), b("sources", Ty::Arr, "runtime (default), http, build, audit (who changed what), traces, metrics"), b("format", Ty::Str, "ndjson (default, signed batches), otlp (an OTLP/HTTP collector's /v1/logs, traces and metrics beside it), syslog (a syslog+tls:// URL, no headers) or s3 (url https://<endpoint>/<bucket>[/<prefix>], headers aws-access-key-id, aws-secret-access-key and optionally aws-region, nothing else)"), b("environments", Ty::Arr, "Only these environments"), b("services", Ty::Arr, "Only these services"), b("previews", Ty::Bool, "Previews' logs too"), b("headers", Ty::Obj, "Headers sent with each batch")],
    ).open_world(),
    t("apps_update_drain", "Change a log drain", "Change what a drain sends or its headers: given ones replace (a new URL is a new drain).", "apps", "PATCH", "/v1/apps/projects/{project}/drains/{did}", &[req("project", "Workspace project"), req("did", "Drain id"), b("sources", Ty::Arr, "runtime (default), http, build, audit (who changed what), traces, metrics"), b("environments", Ty::Arr, "Environments"), b("services", Ty::Arr, "Services"), b("previews", Ty::Bool, "Previews' logs too"), b("headers", Ty::Obj, "Headers (all of them)")]).open_world(),
    t("apps_delete_drain", "Delete a log drain", "Stop sending a project's logs there.", "apps", "DELETE", "/v1/apps/projects/{project}/drains/{did}", &[req("project", "Workspace project"), req("did", "Drain id")]),
    t("apps_test_drain", "Test a log drain", "Send one signed test line now (not retried): whether it arrived.", "apps", "POST", "/v1/apps/projects/{project}/drains/{did}/test", &[req("project", "Workspace project"), req("did", "Drain id")]).open_world(),
    t("apps_pending", "What the next deploy changes", "Before deploying: which variables (names, never values), run settings and project secrets the next deploy changes against what's live.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/pending", &[SERVICE, ENV]),
    t("apps_deploy_commits", "A deploy's commits", "The commits a deploy brings over the one live before it, newest first (at most 50).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/commits", &[SERVICE, ENV, req("deploy", "Deploy id")]),
    t("apps_deploy_vulnerabilities", "A deploy's vulnerabilities", "Known vulnerabilities in a deploy's image, most severe first, then those with a fix: id (CVE-…, GHSA-…), severity, package, installed and fixed versions, and where (the OS's packages or a lockfile), at most 2,000, with the scan's counts. Use it before promoting a deploy, or to find what to upgrade.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/vulnerabilities", &[SERVICE, ENV, req("deploy", "Deploy id"), q("severity", Ty::Str, "Only this severity or worse: critical, high, medium or low")]),
    t("apps_explain_deploy", "Why did a deploy fail", "Ask why a failed deploy failed: the model of the owner of the service's roll code repo reads its error, plan, steps and the newest lines of its build log and output (masked) and answers what went wrong and what to change. The answer is kept with the deploy and answered again without a model call; again: true asks anew (at most once a minute). 409 when it didn't fail.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/explain", &[SERVICE, ENV, req("deploy", "Deploy id"), q("again", Ty::Bool, "Ask the model anew instead of the kept answer")]).open_world(),
    t("apps_deploy_sbom", "A deploy's software bill of materials", "A deploy's image as a CycloneDX SBOM (JSON) from the same scan as its vulnerabilities: every package and version it ships. 404 when it wasn't scanned or was pruned.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/sbom", &[SERVICE, ENV, req("deploy", "Deploy id")]),
    t("apps_status_page", "The project's public status page", "A project's public status page: its title, its public URL and what it shows (which services' checks and heartbeats, names and up or down only). 404 without one; making one is a person's choice, in the dashboard.", "apps", "GET", "/v1/apps/projects/{project}/status-page", &[req("project", "Workspace project")]),
    t("apps_remove_status_page", "Take down the status page", "Take a project's public status page down: its address answers nothing after, and its incidents go with it. Making or changing one stays a person's (it makes the project's service names public; admin, no tool).", "apps", "DELETE", "/v1/apps/projects/{project}/status-page", &[req("project", "Workspace project")]).destructive(),
    t("apps_list_incidents", "Status page incidents", "The last 90 days' incidents on a project's status page and the open ones, newest first: title, state, whether roll opened it, and its updates.", "apps", "GET", "/v1/apps/projects/{project}/status-page/incidents", &[req("project", "Workspace project")]),
    t("apps_create_incident", "Post an incident on the status page", "Post an incident on a project's public status page: anyone can read its title and text, so write for the public and never put a credential, an internal address or a customer's data in it (roll refuses credentials). States: investigating (default), identified, monitoring, resolved.", "apps", "POST", "/v1/apps/projects/{project}/status-page/incidents", &[req("project", "Workspace project"), breq("title", Ty::Str, "1-120 characters, public"), breq("text", Ty::Str, "1-2000 characters, public"), b("state", Ty::Str, "investigating, identified, monitoring or resolved")]).published(),
    t("apps_update_incident", "Update an incident on the status page", "Add a public update to a status page incident (resolved closes it). Anyone can read it.", "apps", "POST", "/v1/apps/projects/{project}/status-page/incidents/{incident}/updates", &[req("project", "Workspace project"), req("incident", "Incident id"), breq("state", Ty::Str, "investigating, identified, monitoring or resolved"), breq("text", Ty::Str, "1-2000 characters, public")]).published(),
    t("apps_environment_slo", "SLO and error budget", "An environment's objective against its window: per indicator (availability, latency) the total, the failures, the share that met it and the error budget left (1 untouched, below 0 spent), and whether it's burning now.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/slo", &[SERVICE, ENV]),
    t("apps_list_forms", "List a site's forms", "A static site's forms with their submission counts and the last one's time (never what people sent; apps_form_submissions reads that).", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/forms", &[SERVICE, ENV]),
    t("apps_form_submissions", "Read form submissions", "What people sent through a site's form, newest first: each submission's id, time and fields. People's own data: every read is audited.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/forms/{form}/submissions", &[SERVICE, ENV, req("form", "Form name"), q("limit", Ty::Int, "At most (1-200, default 50)"), q("before", Ty::Str, "A submission's id: the page after it")]).outside(&["fields"]),
    t("apps_delete_form_submission", "Delete a form submission", "Delete one submission for good (someone asked to be forgotten, or spam). Audited.", "apps", "DELETE", "/v1/apps/services/{service}/environments/{env}/forms/{form}/submissions/{submission}", &[SERVICE, ENV, req("form", "Form name"), req("submission", "Submission id")]),
    t("apps_rollback", "Roll back a deployment", "Roll an environment back to an earlier deploy, with the variables it went live with (keep_variables: true keeps today's).", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/deploys/{deploy}/rollback", &[SERVICE, ENV, req("deploy", "Deploy id"), b("keep_variables", Ty::Bool, "Keep the variables as they are now"), OVERRIDE]),
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
        q("level", Ty::Str, "Only lines at this level or worse (debug, info, warn, error, fatal); a line that says none isn't kept"),
        q("where", Ty::Str, "Only lines whose JSON or logfmt fields say all these terms: level:error user_id:42, key:*, -key:value, a.b:x (at most 10)"),
        q("by", Ty::Str, "With http: only answers roll gave itself, for this reason (or any)"),
    ]),
    t("apps_list_domains", "List domains", "An environment's domains.", "apps", "GET", "/v1/apps/services/{service}/environments/{env}/domains", &[SERVICE, ENV]),
    t("apps_add_domain", "Add a domain", "Point a domain at an environment.", "apps", "POST", "/v1/apps/services/{service}/environments/{env}/domains", &[SERVICE, ENV, breq("hostname", Ty::Str, "Host name")]),
    t("apps_remove_domain", "Remove a domain", "Remove a domain.", "apps", "DELETE", "/v1/apps/services/{service}/environments/{env}/domains/{host}", &[SERVICE, ENV, req("host", "Host name")]),
    t("apps_update_domain", "Change a domain", "Redirect a domain to another hostname of the same environment (redirect_to; null serves the app again; no loops or chains), or serve another of the app's ports on it (port; null the main one). A field left out stays.", "apps", "PATCH", "/v1/apps/services/{service}/environments/{env}/domains/{host}", &[SERVICE, ENV, req("host", "Host name"), bnull("redirect_to", Ty::Str, "Another hostname of this environment to redirect to (clear it: serve the app)"), bnull("port", Ty::Int, "Another of the app's ports (clear it: the main one)")]),
    t("apps_preview_domain", "A project's preview domain", "The wildcard a project's preview environments answer under (each by its preview_host), with its DNS state and TLS; 404 when it has none.", "apps", "GET", "/v1/apps/projects/{project}/preview-domain", &[req("project", "Workspace project")]),
    t("apps_set_preview_domain", "Set a project's preview domain", "Give a project's preview environments a wildcard of its own (*.preview.example.com; a new one replaces it): answers the DNS record to add, then apps_verify_preview_domain.", "apps", "POST", "/v1/apps/projects/{project}/preview-domain", &[req("project", "Workspace project"), breq("hostname", Ty::Str, "A wildcard, *.preview.example.com")]),
    t("apps_verify_preview_domain", "Check a preview domain", "Check a project's preview domain's DNS now: active once it checks out, else pending with what DNS answered.", "apps", "POST", "/v1/apps/projects/{project}/preview-domain/verify", &[req("project", "Workspace project")]),
    t("apps_remove_preview_domain", "Remove a preview domain", "Remove a project's preview domain: its names answer nothing after.", "apps", "DELETE", "/v1/apps/projects/{project}/preview-domain", &[req("project", "Workspace project")]),
    // ── previews (roll previews) ──
    t("previews_list", "List previews", "Preview environments with their counts and states (checks, visual review, pages); previews_get has one preview's issues, broken links and diffs.", "previews", "GET", "/v1/previews", &[q("project", Ty::Str, "Workspace project"), q("state", Ty::Str, "Only previews in this state (ready, sleeping, failed, …)")]),
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
        "previews_wait",
        "Wait for a preview to be ready",
        "A preview once it's ready (its url and share_url), failed (its error), sleeping, going away, or waiting on a person (awaiting_approval: a fork's change runs only after a writer's approve-run). A client that runs MCP tasks gets a task that waits as long as it takes; otherwise this answers the state now (call again to keep waiting).",
        "previews",
        "GET",
        "/v1/previews/{id}",
        &[req("id", "Preview id (from previews_create or previews_list)")],
    )
    .until(preview_settled),
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
    t("previews_traces", "Preview traces: slow or failing requests", "A preview's recent traces (5-360 minutes back, default 60), newest first, at most 100: each one's root span, service, duration, span count and whether it failed. previews_get_trace has one trace's spans. Names and attributes are masked as its logs are.", "previews", "GET", "/v1/previews/{id}/traces", &[req("id", "Preview id"), q("minutes", Ty::Int, "How far back, 5-360 (default 60)")]),
    t("previews_routes", "Preview route times against main", "Each route's p50 and p95 in a preview (from its traces, 5-360 minutes back, default 60) against the same route on main's preview, the slower ones first: `slower` when both sides have 5 traces and the preview's p50 is half again main's and 100 ms more. A slower route's `grew` names the span whose time grew most against main's (span, ms, main_ms): where to look. Which request got slower before it merges. Names are masked as its traces are.", "previews", "GET", "/v1/previews/{id}/routes", &[req("id", "Preview id"), q("minutes", Ty::Int, "How far back, 5-360 (default 60)")]),
    t("previews_get_trace", "A preview trace's spans", "One trace's spans, oldest first: where a slow or failing request spent its time across the preview's services (names and attributes masked).", "previews", "GET", "/v1/previews/{id}/traces/{trace}", &[req("id", "Preview id"), req("trace", "Trace id (from previews_traces)")]),
    t("previews_mask_suggestions", "Unmasked personal data", "Columns in a project's databases and indices that look like people's data (emails, names, phone numbers…) and aren't masked in its previews yet, with the masking rule each would take and why; only resources this token can read.", "previews", "GET", "/v1/previews/projects/{project}/mask-suggestions", &[req("project", "Workspace project")]),
    t("previews_metrics", "Preview metrics", "Each of a preview's services' CPU and memory now: which one is slow or about to run out.", "previews", "GET", "/v1/previews/{id}/metrics", &[req("id", "Preview id")]),
    t("previews_logs", "Preview logs", "A preview's deploy and service logs.", "previews", "GET", "/v1/previews/{id}/logs", &[req("id", "Preview id"), q("service", Ty::Str, "One service's logs (default: the first)"), q("lines", Ty::Int, "Last lines")]),
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
    t("previews_list_mail", "A preview's mail (the emails it sent)", "The mail a preview's services sent, caught and never delivered: who to, subject, when (for checking a sign-up or receipt flow). It can hold production's data: reveal.", "previews", "GET", "/v1/previews/{id}/mail", &[req("id", "Preview id")]),
    t("previews_read_mail", "Read a preview's mail", "One caught message as text.", "previews", "GET", "/v1/previews/{id}/mail/{mail}", &[req("id", "Preview id"), req("mail", "Message id from previews_list_mail")]),
    t("previews_list_notes", "A preview's notes", "Notes people left on a preview's pages through its feedback button and nobody has resolved yet: page, element, what they said, who, when (the latest 500).", "previews", "GET", "/v1/previews/{id}/notes", &[req("id", "Preview id")]),
    t("previews_resolve_note", "Resolve a preview note", "Mark a note left on a preview page's feedback button as dealt with, so the page stops listing it (after fixing what it asked).", "previews", "POST", "/v1/previews/{id}/notes/{note}/resolve", &[req("id", "Preview id"), req("note", "Note id from previews_list_notes")]),
    t("previews_screenshot", "Screenshot a preview", "A preview's page as an image the agent can look at; diff against the base or the previous deploy, another page, or the phone view.", "previews", "GET", "/v1/previews/{id}/screenshot", &[req("id", "Preview id"), q("diff", Ty::Str, "base or previous: highlight what changed"), q("page", Ty::Str, "Path of the page (default /)"), q("view", Ty::Str, "phone for a phone-sized view")]),
    t("previews_share", "Share a preview", "A link for someone outside the team to open a preview for some hours (its token is in this answer only; anyone with the link gets in).", "previews", "POST", "/v1/previews/{id}/shares", &[req("id", "Preview id"), breq("hours", Ty::Int, "How long the link works")]),
    t("previews_revoke_share", "Revoke a share link", "Stop a preview's share link at once (who it's shared with: previews_list_shares).", "previews", "DELETE", "/v1/previews/{id}/shares/{share}", &[req("id", "Preview id"), req("share", "Share id")]),
    t("previews_settings", "Preview settings", "A project's preview rules: which branches get one, how long they live, private or not, budgets (previews_set_rules changes the operational ones; an admin's).", "previews", "GET", "/v1/previews/projects/{project}/settings", &[req("project", "Workspace project")]),
    t(
        "previews_set_rules",
        "Change a project's preview rules",
        "Change how a project's previews are made and kept (a field left out stays): which changes get one (branches, drafts, manual, paths, ignore_paths, skip_authors), how each resource is branched (resources, data), how long they live and sleep, their budgets, Who can open them (access), production secrets, egress, variables, masking, the visual review and what reaches GitHub (its screenshots, thresholds and hidden parts decide that review), and the seed, release and teardown jobs are a person's, in the dashboard: not here.",
        "previews",
        "PUT",
        "/v1/previews/projects/{project}/settings",
        &[
            req("project", "Workspace project"),
            b("enabled", Ty::Bool, "Previews on for the project"),
            b("branches", Ty::Arr, "Globs on a change's branch that get one ([\"*\"])"),
            b("drafts", Ty::Bool, "Previews for draft changes too"),
            b("manual", Ty::Bool, "Only changes whose title asks ([preview])"),
            b("paths", Ty::Arr, "What a change must touch (gitignore-style; [] is anything)"),
            b("ignore_paths", Ty::Arr, "Touching only these doesn't count"),
            b("skip_authors", Ty::Arr, "Globs on who opened it (\"*[bot]\"): no preview unless asked"),
            b("resources", Ty::Obj, "\"<kind>:<ref>\" → branch, schema or skip"),
            b("data", Ty::Str, "The default: branch (with data) or schema"),
            b("reset_every_hours", Ty::Int, "Reset to fresh data every so often (0: never)"),
            b("sleep_after_s", Ty::Int, "Sleep after this long idle"),
            b("ttl_hours", Ty::Int, "Delete after this long"),
            b("max_previews", Ty::Int, "At most this many at once"),
            b("max_bytes", Ty::Int, "At most this much branched data"),
            b("max_usd_month", Ty::Num, "New previews wait once the month's cost reaches it (0: no cap)"),
            b("feedback", Ty::Bool, "A feedback button on its pages for people who came in through roll"),
            b("warm", Ty::Bool, "Wake the services as soon as a new head is deployed"),
            b("crons", Ty::Bool, "Scheduled work (roll apps' crons, scheduled commands and calls) runs in previews too; off by default: deployed, schedule off"),
        ],
    ),
    t("previews_visitors", "Who opened a preview", "The last 8 people signed in to roll who opened a preview (team access), by handle, latest first. Share links and the CI bypass aren't people and aren't listed.", "previews", "GET", "/v1/previews/{id}/visitors", &[req("id", "Preview id")]),
    t("previews_list_hooks", "A project's deploy hooks", "The hooks that start a preview from outside (CI): name, branches, when made and last fired; never their secrets.", "previews", "GET", "/v1/previews/projects/{project}/hooks", &[req("project", "Workspace project")]),
    t("previews_delete_hook", "Delete a deploy hook", "Delete one of a project's preview deploy hooks: its URL stops working.", "previews", "DELETE", "/v1/previews/projects/{project}/hooks/{hook}", &[req("project", "Workspace project"), req("hook", "Hook id")]).destructive(),
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
    within: bool,
    definition: Value,
}

static PREPARED: std::sync::LazyLock<std::collections::HashMap<&'static str, Prepared>> = std::sync::LazyLock::new(|| {
    TOOLS.iter().map(|t| (t.name, Prepared { actions: t.find_actions(), open: t.find_open(), within: t.find_within(), definition: t.build_definition() })).collect()
});

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tool calls a route the permission catalogue classifies, tool names are unique and
    /// fit every client (`^[a-zA-Z0-9_-]{1,64}$`), path parameters are all declared, and the hints
    /// follow the catalogue.
    /// The top-level field names of a contract's body column: each `{ a?, b: string, c?: {…} }`
    /// outside parentheses (a body can be one shape `or` another; prose in parentheses is skipped).
    fn body_fields(body: &str) -> Vec<String> {
        let (mut out, mut parens, mut braces) = (Vec::new(), 0usize, 0usize);
        for (i, c) in body.char_indices() {
            match c {
                '(' if braces == 0 => parens += 1,
                ')' if braces == 0 => parens = parens.saturating_sub(1),
                '{' => {
                    if braces == 0 && parens == 0 {
                        for f in shape_fields(&body[i..]) {
                            if !out.contains(&f) {
                                out.push(f);
                            }
                        }
                    }
                    braces += 1;
                }
                '}' => braces = braces.saturating_sub(1),
                _ => {}
            }
        }
        out
    }

    /// The top-level field names of the shape `body` starts with.
    fn shape_fields(body: &str) -> Vec<String> {
        let (mut depth, mut segs, mut cur) = (0usize, Vec::new(), String::new());
        for c in body[1..].chars() {
            match c {
                '{' | '[' | '(' => depth += 1,
                '}' | ']' | ')' if depth == 0 => break,
                '}' | ']' | ')' => depth -= 1,
                ',' if depth == 0 => {
                    segs.push(std::mem::take(&mut cur));
                    continue;
                }
                _ => {}
            }
            if depth == 0 {
                cur.push(c);
            }
        }
        segs.push(cur);
        segs.iter()
            .filter_map(|seg| {
                let seg = seg.trim();
                let name: String = seg.chars().take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_').collect();
                let rest = seg[name.len()..].trim_start_matches('?').trim_start();
                (!name.is_empty() && name.starts_with(|c: char| c.is_ascii_lowercase()) && (rest.is_empty() || rest.starts_with(':'))).then_some(name)
            })
            .collect()
    }

    /// A tool takes every body field and query parameter its route's contract lists, or
    /// no-field.txt says why not: one a product adds later (S3 log drains, a promotion's
    /// `expect`, a log filter) can't go unmentioned.
    #[test]
    fn tools_take_their_routes_body_fields() {
        const CONTRACTS: &[(&str, &str)] = &[
            (include_str!("../../../docs/apps/http-api.md"), ""),
            (include_str!("../../../docs/workspace.md"), ""),
            (include_str!("../../../docs/previews.md"), ""),
            (include_str!("../../../docs/queues/http-api.md"), ""),
            (include_str!("../../../docs/secrets.md"), "/v1/secrets/projects/{project}"),
        ];
        let norm = |p: &str| p.split('?').next().unwrap_or("").split('/').map(|s| if s.starts_with('{') { "{}" } else { s }).collect::<Vec<_>>().join("/").trim_end_matches('/').to_string();
        let listed: Vec<(String, String, String)> = include_str!("no-field.txt")
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                Some((w.next()?.to_string(), norm(w.next()?), w.next()?.to_string()))
            })
            .collect();
        let (mut missing, mut taken) = (Vec::new(), std::collections::HashSet::new());
        for (doc, prefix) in CONTRACTS {
            for line in doc.lines() {
                let Some(rest) = line.strip_prefix("| `") else { continue };
                let Some((route, rest)) = rest.split_once('`') else { continue };
                let Some((method, path)) = route.split_once(' ') else { continue };
                if !matches!(method, "GET" | "POST" | "PUT" | "PATCH" | "DELETE") || !path.starts_with('/') {
                    continue;
                }
                let path = if path.starts_with("/v1") { path.to_string() } else { format!("{prefix}{path}") };
                let queries: Vec<String> = path.split_once('?').map(|(_, q)| q.split('&').filter_map(|kv| kv.split('=').next()).filter(|k| !k.is_empty()).map(str::to_string).collect()).unwrap_or_default();
                // The body column: up to the next `|` that isn't escaped.
                let rest = rest.trim_start().trim_start_matches('|');
                let mut body = String::new();
                let mut chars = rest.chars();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => body.extend(chars.next()),
                        '|' => break,
                        c => body.push(c),
                    }
                }
                let tools: Vec<&Tool> = all().iter().filter(|t| t.method == method && norm(t.path) == norm(&path)).collect();
                for t in tools.iter().filter(|t| !t.params.iter().any(|p| p.at == At::RawJson)) {
                    let body = if matches!(method, "POST" | "PUT" | "PATCH") { body_fields(&body) } else { Vec::new() };
                    for field in body.into_iter().chain(queries.iter().cloned()) {
                        if t.params.iter().any(|p| p.name == field) {
                            taken.insert((method.to_string(), norm(&path), field));
                        } else if !listed.iter().any(|(m, r, f)| m == method && *r == norm(&path) && *f == field) {
                            missing.push(format!("{} ({method} {path}): {field}", t.name));
                        }
                    }
                }
            }
        }
        assert!(missing.is_empty(), "body fields and query parameters a route's contract lists that its tool doesn't take: add each to the tool in tools.rs, or `METHOD route field  why` to crates/roll-mcp/src/no-field.txt:\n{}", missing.join("\n"));
        let stale: Vec<String> = listed.iter().filter(|l| taken.contains(l)).map(|(m, r, f)| format!("{m} {r} {f}")).collect();
        assert!(stale.is_empty(), "no-field.txt lines for fields a tool takes now: remove them:\n{}", stale.join("\n"));
        assert_eq!(body_fields("{ from, message?, expect?: string, files?: {[name]: path | null} }"), ["from", "message", "expect", "files"]);
        assert_eq!(body_fields("{ url, headers? } or { bucket, prefix?, … } (prose `{ x }`)"), ["url", "headers", "bucket", "prefix"]);
    }

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
        // A tool that another's text names ("from secrets_list_placeholders") is one roll has
        // (`access_denied` is an audit event; roll code's tools are another crate's).
        for t in all() {
            let params: Vec<&str> = t.params.iter().map(|p| p.desc).collect();
            let text = format!("{} {} {}", t.title, t.description, params.join(" "));
            for word in text.split(|c: char| !(c.is_ascii_lowercase() || c == '_')) {
                let Some((product, _)) = word.split_once('_') else { continue };
                if roll_auth::scope::PRODUCTS.contains(&product) && !matches!(product, "code" | "agents") && word != "access_denied" {
                    assert!(names.contains(word), "{} names {word}, which isn't a tool", t.name);
                }
            }
        }
        let def = |n: &str| all().iter().find(|t| t.name == n).unwrap().definition();
        assert_eq!(def("postgres_query")["annotations"]["readOnlyHint"], true);
        assert_eq!(def("storage_presign")["annotations"]["readOnlyHint"], false, "a presigned PUT writes");
        assert_eq!(def("secrets_reveal")["annotations"]["readOnlyHint"], true);
        assert_eq!(def("postgres_execute")["annotations"]["destructiveHint"], true);
        assert_eq!(def("postgres_delete_project")["annotations"]["destructiveHint"], true);
        assert_eq!(def("postgres_create_branch")["annotations"]["readOnlyHint"], false);
        assert_eq!(def("apps_deploy")["annotations"]["destructiveHint"], true);
        // Taking a status page down deletes its incidents too: a write the catalogue calls write.
        assert_eq!(def("apps_remove_status_page")["annotations"]["destructiveHint"], true);
        // Every apps tool has an area (or is a lookup every area lists), and each area is a fraction of the whole.
        for (product, areas) in AREAS {
            let tools: Vec<&Tool> = all().iter().filter(|t| t.product == *product).collect();
            let unplaced: Vec<&str> = tools.iter().filter(|t| t.area() == Area::Unplaced).map(|t| t.name).collect();
            assert!(unplaced.is_empty(), "{product} tools with no area (add their route to {product}_area): {unplaced:?}");
            for area in *areas {
                let n = tools.iter().filter(|t| matches!(t.area(), Area::Every) || t.area() == Area::One(area)).count();
                assert!(n > 5 && n < 50, "{product}.{area}: {n} tools");
            }
        }
        assert_eq!(all().iter().find(|t| t.name == "secrets_transit_encrypt").unwrap().area(), Area::One("integrations"));
        assert_eq!(all().iter().find(|t| t.name == "apps_logs").unwrap().area(), Area::One("observe"));
        assert_eq!(all().iter().find(|t| t.name == "apps_list_services").unwrap().area(), Area::Every);
        // Every catalogued route of a product with tools here is a tool's, or in no-tool.txt with
        // why (`no tool yet` for a gap): a new route can't go without a decision, as
        // prefix-reviewed.txt does for the catalogue. roll code's and agents' have their own map.
        let norm = |p: &str| p.split('/').map(|s| if s.starts_with('{') { "{}" } else { s }).collect::<Vec<_>>().join("/").trim_end_matches('/').to_string();
        let products: std::collections::BTreeSet<&str> = all().iter().map(|t| t.product).collect();
        let covered = |method: &str, route: &str| all().iter().any(|t| (method == "*" || t.method == method) && norm(t.path) == norm(route));
        let listed: Vec<(&str, &str)> = include_str!("no-tool.txt")
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                Some((w.next()?, w.next()?))
            })
            .collect();
        let product_of = |method: &str, route: &str| {
            let sample = route.split('/').map(|s| if s.starts_with('{') { "x" } else { s }).collect::<Vec<_>>().join("/");
            catalog::by_path(if method == "*" { "GET" } else { method }, &sample).map(|f| f.row.product)
        };
        let mut routes: Vec<(&str, &str)> = catalog::rows().iter().filter(|r| !r.route.ends_with('/') && products.contains(r.product)).map(|r| (r.method, r.route)).collect();
        routes.extend(catalog::prefix_reviewed_routes().filter(|(m, r)| product_of(m, r).is_some_and(|p| products.contains(p))));
        let is_listed = |m: &str, r: &str| listed.iter().any(|(lm, lr)| *lm == m && norm(lr) == norm(r));
        let undecided: Vec<String> = routes.iter().filter(|(m, r)| !covered(m, r) && !is_listed(m, r)).map(|(m, r)| format!("{m} {r}")).collect();
        assert!(undecided.is_empty(), "catalogued routes with no MCP tool and no reason: map each in tools.rs, or add `METHOD route  why` to crates/roll-mcp/src/no-tool.txt:\n{}", undecided.join("\n"));
        // A line may name a route an open PR is about to add (declared ahead, so its gate passes);
        // one whose route has a tool now is wrong.
        // A `*` line speaks for the methods with no tool, so a tool on its path doesn't make it stale.
        let stale: Vec<String> = listed.iter().filter(|(m, r)| *m != "*" && covered(m, r)).map(|(m, r)| format!("{m} {r}")).collect();
        assert!(stale.is_empty(), "no-tool.txt lines for routes that have a tool now: remove them:\n{}", stale.join("\n"));
        // A tool that removes, revokes or replaces something says so (clients ask a person before a
        // destructive call), unless it's marked undoable: the review that found five revoke tools
        // unmarked is why this is a test, not a habit.
        let words = ["delete", "remove", "revoke", "purge", "unlink", "restore", "reset", "rotate", "truncate", "drop", "wipe", "cut_off", "end_lease"];
        let unmarked: Vec<&str> = all()
            .iter()
            .filter(|t| (t.method == "DELETE" || words.iter().any(|w| t.name.contains(w))) && def(t.name)["annotations"]["destructiveHint"] != true && t.hint != Hint::Undoable)
            .map(|t| t.name)
            .collect();
        assert!(unmarked.is_empty(), "removes or replaces something but isn't destructive (mark it .destructive() or .undoable()): {unmarked:?}");
        assert_eq!(def("workspace_remove_resource")["annotations"]["destructiveHint"], false);
        // A project's preview rules from an agent are only the operational ones: who may open the
        // previews, production secrets, egress, variables, masking, the visual review's gate and the
        // jobs (which run with the preview's secrets) stay a person's, so the closed schema refuses them.
        let rules = all().iter().find(|t| t.name == "previews_set_rules").unwrap();
        for field in ["access", "secrets", "egress", "egress_hosts", "egress_forks", "env", "env_for", "masking", "visual_review", "github", "seed", "release", "teardown"] {
            assert!(rules.request(&json!({ "project": "prj_a", field: "x" })).is_err(), "previews_set_rules takes {field}");
        }
        assert!(rules.request(&json!({ "project": "prj_a", "ttl_hours": 24 })).is_ok());
        // Ensuring an environment never deletes keys (the product's `replace` drops the folder's
        // keys it isn't given), and a batch that can delete keys says so.
        assert!(all().iter().find(|t| t.name == "secrets_ensure_environment").unwrap().request(&json!({ "project": "prj_a", "env": "staging", "replace": true })).is_err());
        assert_eq!(def("secrets_apply_changes")["annotations"]["destructiveHint"], true);
        // Waiting on a preview stops where it settles or needs a person, never while it's still being made.
        let settled = all().iter().find(|t| t.name == "previews_wait").unwrap().until.unwrap();
        assert!(["ready", "failed", "awaiting_approval"].iter().all(|s| settled(&json!({ "state": s }))) && !settled(&json!({ "state": "creating" })));
        // Revoking a credential can't be undone: clients that ask before destructive calls ask.
        for n in ["storage_revoke_key", "streams_revoke_key", "search_revoke_key", "search_unlink_remote", "previews_delete_hook"] {
            assert_eq!(def(n)["annotations"]["destructiveHint"], true, "{n}");
        }
        // A tool that has roll reach an address the caller chose says so (MCP's openWorldHint); the rest don't.
        for n in ["tasks_publish", "apps_add_drain", "apps_deploy", "secrets_rotate"] {
            assert_eq!(def(n)["annotations"]["openWorldHint"], true, "{n}");
        }
        for n in ["storage_list_buckets", "postgres_execute", "apps_delete_drain", "secrets_set"] {
            assert_eq!(def(n)["annotations"]["openWorldHint"], false, "{n}");
        }
        let r = all().iter().find(|t| t.name == "storage_write_object").unwrap().request(&json!({ "bucket": "a b", "key": "x/y.txt", "content": "hi" })).unwrap();
        assert_eq!(r.uri, "/v1/storage/buckets/a%20b/upload?key=x%2Fy.txt");
        assert!(matches!(r.body, Built::Raw(ref b, _) if b == b"hi"));
        // Ids go as they are (`svc_…`): a product that routes by the raw path sees the id it made.
        let r = all().iter().find(|t| t.name == "apps_restart").unwrap().request(&json!({ "service": "svc_a-b~c", "env": ".." })).unwrap();
        assert_eq!(r.uri, "/v1/apps/services/svc_a-b~c/environments/%2E%2E/restart");
        assert!(all()[0].request(&json!({ "nope": 1 })).is_err());
        // Past a freeze: the header apps reads (exactly `1`), never a body field; no override, no header.
        let deploy = all().iter().find(|t| t.name == "apps_deploy").unwrap();
        let r = deploy.request(&json!({ "service": "svc_a", "env": "production", "override": true })).unwrap();
        assert!(r.headers.contains(&("x-roll-override", "1".into())) && matches!(r.body, Built::Json(ref b) if b.get("override").is_none()), "{:?}", r.headers);
        assert!(!deploy.request(&json!({ "service": "svc_a", "env": "production", "override": false })).unwrap().headers.iter().any(|(h, _)| *h == "x-roll-override"));
        // Explain plans the agent's statement in a read-only transaction, and runs nothing.
        let r = all().iter().find(|t| t.name == "postgres_explain").unwrap().request(&json!({ "endpoint": "ep_1", "sql": "select * from users;" })).unwrap();
        assert!(matches!(r.body, Built::Json(ref b) if b["sql"] == "EXPLAIN (FORMAT JSON) select * from users" && b["read_only"] == true));
        for n in ["postgres_explain", "postgres_slow_queries", "postgres_advisors", "postgres_list_extensions"] {
            assert_eq!(def(n)["annotations"]["readOnlyHint"], true, "{n}");
        }
        // A value request's link writes those keys: one answer and a day unless the agent asks for more.
        let ask = all().iter().find(|t| t.name == "secrets_request_value").unwrap();
        let r = ask.request(&json!({ "project": "prj_a", "env": "production", "path": "/api", "keys": ["STRIPE_KEY"] })).unwrap();
        assert_eq!(r.uri, "/v1/secrets/projects/prj_a/environments/production/value-requests?path=%2Fapi");
        assert!(matches!(r.body, Built::Json(ref b) if b["ttl_minutes"] == 1440 && b["answers"] == 1 && b["keys"][0] == "STRIPE_KEY"));
        let r = ask.request(&json!({ "project": "prj_a", "env": "production", "keys": ["A"], "answers": 3 })).unwrap();
        assert!(matches!(r.body, Built::Json(ref b) if b["answers"] == 3 && b["ttl_minutes"] == 1440));
        // Skipping a protected environment's approvals is a person's: an agent applies only what's approved.
        assert!(all().iter().find(|t| t.name == "secrets_apply_request").unwrap().request(&json!({ "project": "prj_a", "env": "production", "id": "req_a", "bypass": true })).is_err());
        // A field is cleared only when `clear` names it: a null is left out (a client that fills
        // unused fields with null changes nothing), and only a field that can be is.
        let domain = all().iter().find(|t| t.name == "apps_update_domain").unwrap();
        assert_eq!(def("apps_update_domain")["inputSchema"]["properties"]["clear"]["items"]["enum"], json!(["redirect_to", "port"]));
        let body = |args: Value| match domain.request(&args).unwrap().body { Built::Json(b) => b, _ => Value::Null };
        let at = json!({ "service": "svc_a", "env": "production", "host": "a.example.com" });
        let with = |k: &str, v: Value| { let mut a = at.clone(); a[k] = v; a };
        assert_eq!(body(json!({ "service": "svc_a", "env": "production", "host": "a.example.com", "port": 8080, "redirect_to": null })), json!({ "port": 8080 }));
        assert_eq!(body(with("clear", json!(["redirect_to"]))), json!({ "redirect_to": null }));
        assert!(domain.request(&with("clear", json!(["host"]))).is_err());
        let drain = all().iter().find(|t| t.name == "apps_update_drain").unwrap();
        assert!(drain.request(&json!({ "project": "prj_a", "did": "d", "clear": ["headers"] })).is_err(), "a drain's headers are never cleared by a null");
        // What a site's visitors write (a form's fields, and the field names a form.submitted event
        // carries) is withheld in lockdown wherever an answer holds it.
        for n in ["apps_form_submissions", "apps_events", "apps_activity"] {
            assert!(all().iter().find(|t| t.name == n).unwrap().outside.contains(&"fields"), "{n}");
        }
        // What a cluster's users typed into a search box (top queries, queries that found nothing).
        assert!(all().iter().find(|t| t.name == "search_analytics").unwrap().outside.contains(&"query"));
        // What goes on a public page is held to the credential guard as anywhere people read.
        for n in ["apps_create_incident", "apps_update_incident"] {
            assert!(all().iter().find(|t| t.name == n).unwrap().published, "{n}");
        }
        // docs/mcp.md lists every tool.
        let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/mcp.md")).unwrap();
        let missing: Vec<_> = all().iter().filter(|t| !doc.contains(&format!("| `{}` |", t.name))).map(|t| t.name).collect();
        assert!(missing.is_empty(), "docs/mcp.md doesn't list {missing:?}");
        // A tool a description or a workflow tells the agent to call is one roll has: a renamed
        // tool would otherwise send agents after a name that answers "unknown tool".
        // roll code's are another crate's: the ones roll-mcp names are in prompts::FRONTED (rolld's
        // tests hold each to one roll code fronts).
        let names: Vec<String> = all().iter().map(|t| t.name.to_string()).chain([crate::search::FIND, crate::search::RUN_READ, crate::search::RUN].map(String::from)).chain(crate::prompts::FRONTED.iter().map(|n| n.to_string())).collect();
        let products = ["access", "workspace", "postgres", "redis", "storage", "streams", "tasks", "queues", "nosql", "search", "secrets", "apps", "previews", "roll", "code", "agents"];
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
