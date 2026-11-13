//! The desired nodes API (`/_internal/desired_nodes`): update (with `dry_run`), get latest and
//! delete, with Elasticsearch's request parsing, validation and versioning rules.
//!
//! Parsing follows `ConstructingObjectParser`, whose error shapes clients and tests look at: a
//! desired node is built as soon as all five of its arguments (`settings`, `processors`,
//! `processors_range`, `memory`, `storage`) have arrived — a failure then is reported as
//! failing to parse that last field — or else at the end of the object, where missing required
//! fields fail with `Required [x]` and a failing build with "Failed to build [desired_node]
//! after last required field arrived". Every node error is wrapped in
//! `[l:c] [update_desired_nodes_request] failed to parse field [nodes]`, with the locations
//! Jackson would report.

use crate::error::{EsError, EsResult};
use crate::json::locate::{Seg, Target, locate, value_end};
use crate::json::{Map, Value};
use crate::meta::desired_nodes::{DesiredNode, DesiredNodes, Processors, external_id};
use crate::meta::settings::Settings;
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::units::ByteSize;
use std::sync::Arc;

const REQUEST: &str = "update_desired_nodes_request";

/// A processor range: `min` and the optional `max`.
type Range = (Processors, Option<Processors>);
/// An error with the `[line:col]` the parser was at.
type Located = (EsError, (usize, usize));
const NODE: &str = "desired_node";
const RANGE: &str = "processors_range";

/// Positions in the raw body (`None` when the body isn't JSON text).
struct Body<'a> {
    raw: Option<&'a [u8]>,
}

impl Body<'_> {
    fn at(&self, path: &[Seg], target: Target) -> (usize, usize) {
        self.raw.and_then(|r| locate(r, path, target)).unwrap_or((1, 1))
    }

    fn value(&self, path: &[Seg]) -> (usize, usize) {
        self.at(path, Target::Value)
    }

    fn name(&self, path: &[Seg]) -> (usize, usize) {
        self.at(path, Target::Name)
    }

    /// The last byte of the value (an object's closing brace).
    fn end(&self, path: &[Seg]) -> (usize, usize) {
        self.raw.and_then(|r| value_end(r, path)).unwrap_or((1, 1))
    }
}

fn xparse(at: (usize, usize), reason: impl Into<String>) -> EsError {
    EsError::x_content_parse(at.0, at.1, reason)
}

fn token(v: &Value) -> &'static str {
    match v {
        Value::Null => "VALUE_NULL",
        Value::Bool(_) => "VALUE_BOOLEAN",
        Value::Num(_) => "VALUE_NUMBER",
        Value::Str(_) => "VALUE_STRING",
        Value::Arr(_) => "START_ARRAY",
        Value::Obj(_) => "START_OBJECT",
    }
}

fn unsupported(at: (usize, usize), parser: &str, field: &str, v: &Value) -> EsError {
    xparse(at, format!("[{parser}] {field} doesn't support values of type: {}", token(v)))
}

/// Java's `%f`.
fn java_f(d: f64) -> String {
    if d.is_nan() {
        "NaN".into()
    } else if d.is_infinite() {
        if d > 0.0 { "Infinity".into() } else { "-Infinity".into() }
    } else {
        format!("{d:.6}")
    }
}

/// `Processors.fromXContent` for a `DOUBLE` field (numbers or numeric strings).
fn processors(parser: &str, field: &str, v: &Value, at: (usize, usize)) -> EsResult<Processors> {
    let d = match v {
        Value::Num(n) => n.as_f64(),
        Value::Str(s) => match s.as_str() {
            "NaN" => f64::NAN,
            "Infinity" | "+Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            _ => crate::units::parse_java_double(s.trim()).ok_or_else(|| {
                xparse(at, format!("[{parser}] failed to parse field [{field}]")).caused_by(crate::rest::params::nfe(s))
            })?,
        },
        other => return Err(unsupported(at, parser, field, other)),
    };
    if !d.is_finite() || d <= 0.0 {
        return Err(xparse(at, format!("[{parser}] failed to parse field [{field}]"))
            .caused_by(EsError::illegal_argument(format!("Only a positive number of [{field}] are allowed and [{}] was provided", java_f(d)))));
    }
    Ok(Processors::new(d))
}

fn java_double_string(p: Option<Processors>) -> String {
    p.map(|p| crate::json::write::java_double(p.0)).unwrap_or_else(|| "null".into())
}

/// `ProcessorsRange`: `min` required, `max` optional, built once both arrived. Errors carry the
/// location the parser was at.
fn processors_range(body: &Body, path: &[Seg], v: &Map) -> Result<Range, Located> {
    let build = |min: Processors, max: Option<Processors>| -> Result<(Processors, Option<Processors>), EsError> {
        if let Some(mx) = max
            && min.0 > mx.0
        {
            return Err(EsError::new(400, "x_content_parse_exception", "Failed to build [processors_range] after last required field arrived").caused_by(
                EsError::illegal_argument(format!(
                    "min processors must be less than or equal to max processors and it was: min: {} max: {}",
                    java_double_string(Some(min)),
                    java_double_string(max)
                )),
            ));
        }
        Ok((min, max))
    };
    let (mut min, mut max): (Option<Processors>, Option<Option<Processors>>) = (None, None);
    for (k, fv) in v {
        let mut p = path.to_vec();
        p.push(Seg::Key(k));
        let at = body.value(&p);
        match k.as_str() {
            "min" => min = Some(processors(RANGE, "min", fv, at).map_err(|e| (e, at))?),
            "max" => max = Some(Some(processors(RANGE, "max", fv, at).map_err(|e| (e, at))?)),
            other => {
                let name_at = body.name(&p);
                return Err((xparse(name_at, format!("[{RANGE}] unknown field [{other}]")), name_at));
            }
        }
        if let (Some(mn), Some(mx)) = (min, max) {
            return build(mn, mx).map_err(|e| (xparse(at, format!("[{RANGE}] failed to parse field [{k}]")).caused_by(e), at));
        }
    }
    let end = body.end(path);
    let Some(min) = min else { return Err((EsError::illegal_argument("Required [min]"), end)) };
    build(min, max.flatten()).map_err(|e| (e, end))
}

/// The `DesiredNode` constructor's checks.
fn build_node(
    settings: Settings,
    processors: Option<Processors>,
    range: Option<(Processors, Option<Processors>)>,
    memory: ByteSize,
    storage: ByteSize,
) -> Result<DesiredNode, EsError> {
    let fail = |reason: String| {
        EsError::new(400, "x_content_parse_exception", "Failed to build [desired_node] after last required field arrived").caused_by(EsError::illegal_argument(reason))
    };
    if processors.is_none() && range.is_none() {
        return Err(fail("processors or processors_range should be specified and none was specified".into()));
    }
    if processors.is_some() && range.is_some() {
        return Err(fail("processors and processors_range were specified, but only one should be specified".into()));
    }
    let id = external_id(&settings);
    if id.trim().is_empty() {
        return Err(fail("[node.name] or [node.external_id] is missing or empty".into()));
    }
    let roles = DesiredNode::roles_of(&settings).map_err(fail)?;
    Ok(DesiredNode { settings, processors, processors_range: range, memory, storage, external_id: id, roles, actualized: false })
}

/// Parses one entry of `nodes`; errors are the inner exception and the location the outer
/// `failed to parse field [nodes]` reports.
fn parse_node(body: &Body, path: &[Seg], node: &Value) -> Result<DesiredNode, (EsError, (usize, usize))> {
    let Value::Obj(m) = node else {
        let at = body.value(path);
        return Err((xparse(at, format!("[{NODE}] Expected START_OBJECT but was: {}", token(node))), at));
    };
    let mut settings: Option<Settings> = None;
    let mut proc: Option<Option<Processors>> = None;
    let mut range: Option<Option<(Processors, Option<Processors>)>> = None;
    let mut memory: Option<ByteSize> = None;
    let mut storage: Option<ByteSize> = None;
    for (k, v) in m {
        let mut p = path.to_vec();
        p.push(Seg::Key(k));
        let at = body.value(&p);
        let field_err = |e: EsError| (xparse(at, format!("[{NODE}] failed to parse field [{k}]")).caused_by(e), at);
        match k.as_str() {
            "settings" => match v {
                Value::Obj(_) => settings = Some(Settings::from_json(v).map_err(field_err)?),
                other => return Err((unsupported(at, NODE, "settings", other), at)),
            },
            "processors" => proc = Some(Some(processors(NODE, "processors", v, at).map_err(|e| (e, at))?)),
            "processors_range" => match v {
                Value::Null => range = Some(None),
                Value::Obj(r) => {
                    let parsed = processors_range(body, &p, r)
                        .map_err(|(e, loc)| (xparse(loc, format!("[{NODE}] failed to parse field [{k}]")).caused_by(e), loc))?;
                    range = Some(Some(parsed));
                }
                other => return Err((unsupported(at, NODE, "processors_range", other), at)),
            },
            "memory" | "storage" => {
                let Value::Str(s) = v else { return Err((unsupported(at, NODE, k, v), at)) };
                let size = ByteSize::parse(s, k).map_err(field_err)?;
                if k == "memory" {
                    memory = Some(size);
                } else {
                    storage = Some(size);
                }
            }
            other => {
                // Jackson has moved on to the value when the outer parser reports the failure.
                return Err((xparse(body.name(&p), format!("[{NODE}] unknown field [{other}]")), at));
            }
        }
        // All five arguments present: built right away.
        if let (Some(s), Some(pr), Some(r), Some(mem), Some(st)) = (&settings, &proc, &range, &memory, &storage) {
            return build_node(s.clone(), *pr, *r, *mem, *st).map_err(field_err);
        }
    }
    let end = body.end(path);
    let mut missing = Vec::new();
    if settings.is_none() {
        missing.push("settings");
    }
    if memory.is_none() {
        missing.push("memory");
    }
    if storage.is_none() {
        missing.push("storage");
    }
    if !missing.is_empty() {
        return Err((EsError::illegal_argument(format!("Required [{}]", missing.join(", "))), end));
    }
    build_node(settings.unwrap_or_default(), proc.flatten(), range.flatten(), memory.unwrap_or(ByteSize(0)), storage.unwrap_or(ByteSize(0)))
        .map_err(|e| (e, end))
}

fn parse_request(ctx: &Ctx) -> EsResult<Vec<DesiredNode>> {
    let Some(v) = ctx.body()? else { return Err(EsError::parse("request body is required")) };
    let body = Body { raw: ctx.raw_json() };
    let Value::Obj(top) = &v else { return Err(EsError::parse("request body is required")) };
    let mut nodes: Option<Vec<DesiredNode>> = None;
    for (k, val) in top {
        let path = [Seg::Key(k)];
        match k.as_str() {
            "nodes" => {
                let Value::Arr(items) = val else {
                    let at = body.value(&path);
                    return Err(unsupported(at, REQUEST, "nodes", val));
                };
                let mut out = Vec::with_capacity(items.len().min(1024));
                for (i, item) in items.iter().enumerate() {
                    let p = [Seg::Key("nodes"), Seg::Index(i)];
                    match parse_node(&body, &p, item) {
                        Ok(n) => out.push(n),
                        Err((e, at)) => return Err(xparse(at, format!("[{REQUEST}] failed to parse field [nodes]")).caused_by(e)),
                    }
                }
                nodes = Some(out);
            }
            other => return Err(xparse(body.name(&path), format!("[{REQUEST}] unknown field [{other}]"))),
        }
    }
    nodes.ok_or_else(|| EsError::illegal_argument("Required [nodes]"))
}

/// `PUT _internal/desired_nodes/{history_id}/{version}`.
pub async fn update(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let history = ctx.path_param("history_id").unwrap_or_default().to_string();
    let raw_version = ctx.path_param("version").unwrap_or_default().to_string();
    let version = crate::units::parse_java_long(&raw_version).ok_or_else(|| {
        EsError::illegal_argument(format!("Failed to parse long parameter [version] with value [{raw_version}]")).caused_by(crate::rest::params::nfe(&raw_version))
    })?;
    let dry_run = ctx.bool_param("dry_run", false)?;
    let nodes = parse_request(ctx)?;
    let mut errors = Vec::new();
    if history.trim().is_empty() {
        errors.push("historyID should not be empty".to_string());
    }
    if version < 0 {
        errors.push("version must be positive".to_string());
    }
    if !nodes.iter().any(DesiredNode::has_master_role) {
        errors.push("nodes must contain at least one master node".to_string());
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    // `DesiredNodes.create`: external ids are unique (reported in hash-set order).
    let mut seen: Vec<&str> = Vec::new();
    let mut dups: Vec<String> = Vec::new();
    for n in &nodes {
        if seen.contains(&n.external_id.as_str()) {
            if !dups.contains(&n.external_id) {
                dups.push(n.external_id.clone());
            }
        } else {
            seen.push(&n.external_id);
        }
    }
    if !dups.is_empty() {
        let order = crate::json::java_hash_order(&dups, 16);
        let list: Vec<&str> = order.into_iter().map(|i| dups[i].as_str()).collect();
        return Err(EsError::illegal_argument(format!("Some nodes contain the same setting value [{}] for [node.external_id]", list.join(", "))));
    }
    let mut nodes = nodes;
    nodes.sort_by(|a, b| a.external_id.cmp(&b.external_id));
    let member = ctx.cluster.node.name.clone();
    let proposed = DesiredNodes { history_id: history, version, nodes };
    if dry_run {
        // A dry run validates against the current state and changes nothing.
        next_version(ctx.state().metadata.desired_nodes.as_deref(), proposed, &member)?;
        return Reply::ok(obj! { "replaced_existing_history_id" => false, "dry_run" => true });
    }
    let replaced = ctx
        .cluster
        .update(move |st| {
            let latest = st.metadata.desired_nodes.clone();
            let Some(next) = next_version(latest.as_deref(), proposed, &member)? else { return Ok(false) };
            let replaced = latest.as_ref().is_some_and(|l| l.history_id != next.history_id);
            st.metadata.desired_nodes = Some(Arc::new(next));
            Ok(replaced)
        })
        .await?;
    Reply::ok(obj! { "replaced_existing_history_id" => replaced, "dry_run" => false })
}

/// `TransportUpdateDesiredNodesAction.updateDesiredNodes`: the new latest desired nodes, `None`
/// when the request restates the latest ones.
fn next_version(latest: Option<&DesiredNodes>, mut proposed: DesiredNodes, member: &str) -> EsResult<Option<DesiredNodes>> {
    // Statuses carry over from the previous version of the same nodes.
    if let Some(l) = latest {
        for n in &mut proposed.nodes {
            n.actualized = l.nodes.iter().any(|o| o.external_id == n.external_id && o.actualized);
        }
    }
    proposed.actualize(&[member]);
    let Some(l) = latest else { return Ok(Some(proposed)) };
    if l.same_definition(&proposed) {
        return Ok(None);
    }
    if l.history_id == proposed.history_id && l.version == proposed.version {
        return Err(EsError::illegal_argument(format!(
            "Desired nodes with history [{}] and version [{}] already exists with a different definition",
            l.history_id, l.version
        )));
    }
    if l.history_id == proposed.history_id && l.version > proposed.version {
        return Err(EsError::new(
            409,
            "version_conflict_exception",
            format!("version [{}] has been superseded by version [{}] for history [{}]", proposed.version, l.version, l.history_id),
        ));
    }
    Ok(Some(proposed))
}

/// `GET _internal/desired_nodes/_latest`.
pub fn get_latest(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    match &ctx.state().metadata.desired_nodes {
        Some(d) => Reply::ok(d.to_value(false)),
        None => Err(EsError::resource_not_found("Desired nodes not found")),
    }
}

/// `DELETE _internal/desired_nodes`: an empty `text/plain` answer
/// (`plain_text_empty_response`).
pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    if ctx.state().metadata.desired_nodes.is_some() {
        ctx.cluster
            .update(|st| {
                st.metadata.desired_nodes = None;
                Ok(())
            })
            .await?;
    }
    Ok(Reply::Raw(200, "text/plain; charset=UTF-8".into(), bytes::Bytes::new()))
}
