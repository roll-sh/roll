//! `POST _cluster/reroute`: allocation commands against roll's routing table.
//!
//! A roll cluster reports one node holding every primary (started) while replicas stay
//! unassigned, so commands can't change anything; but they are parsed and executed exactly as
//! Elasticsearch's `AllocationCommands` do, so clients get the same validation errors (with the
//! same `[line:col]` locations, which follow Jackson's token positions) and `?explain=true`
//! returns the decisions Elasticsearch gives on a single node: a primary can't be cancelled
//! without `allow_primary`, moving to the node holding the shard is refused by `same_shard`, and
//! so on.

use crate::error::{EsError, EsResult};
use crate::json::locate::{Seg, Target, locate, value_end};
use crate::json::{Map, Value};
use crate::meta::IndexMeta;
use crate::obj;
use crate::rest::{Ctx, Reply};

/// One parsed allocation command.
#[derive(Debug)]
enum Command {
    Cancel { index: String, shard: i64, node: String, allow_primary: bool },
    Move { index: String, shard: i64, from_node: String, to_node: String },
    Allocate { kind: AllocateKind, index: String, shard: i64, node: String, accept_data_loss: bool },
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum AllocateKind {
    Replica,
    EmptyPrimary,
    StalePrimary,
}

impl AllocateKind {
    fn name(self) -> &'static str {
        match self {
            AllocateKind::Replica => "allocate_replica",
            AllocateKind::EmptyPrimary => "allocate_empty_primary",
            AllocateKind::StalePrimary => "allocate_stale_primary",
        }
    }
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::Cancel { .. } => "cancel",
            Command::Move { .. } => "move",
            Command::Allocate { kind, .. } => kind.name(),
        }
    }

    fn parameters(&self) -> Value {
        match self {
            Command::Cancel { index, shard, node, allow_primary } => {
                obj! { "index" => index.clone(), "shard" => *shard, "node" => node.clone(), "allow_primary" => *allow_primary }
            }
            Command::Move { index, shard, from_node, to_node } => {
                obj! { "index" => index.clone(), "shard" => *shard, "from_node" => from_node.clone(), "to_node" => to_node.clone() }
            }
            Command::Allocate { kind: AllocateKind::Replica, index, shard, node, .. } => {
                obj! { "index" => index.clone(), "shard" => *shard, "node" => node.clone() }
            }
            Command::Allocate { index, shard, node, accept_data_loss, .. } => {
                obj! { "index" => index.clone(), "shard" => *shard, "node" => node.clone(), "accept_data_loss" => *accept_data_loss }
            }
        }
    }
}

/// `XContentParser.Token` names (one `VALUE_NUMBER` for every number, unlike Jackson).
fn token_name(v: &Value) -> &'static str {
    match v {
        Value::Num(_) => "VALUE_NUMBER",
        other => other.kind_name(),
    }
}

/// Builds the `x_content_parse_exception` that wraps every command parse failure.
fn wrap(at: (usize, usize), cause: EsError) -> EsError {
    EsError::x_content_parse(at.0, at.1, "[cluster_reroute] failed to parse field [commands]").caused_by(cause)
}

/// Elasticsearch's `Booleans.parseBoolean` on a JSON value.
fn parse_bool(v: &Value) -> Result<bool, EsError> {
    match v {
        Value::Bool(b) => Ok(*b),
        Value::Str(s) if s == "true" => Ok(true),
        Value::Str(s) if s == "false" => Ok(false),
        other => Err(EsError::illegal_argument(format!("Failed to parse value [{}] as only [true] or [false] are allowed.", other.to_es_string()))),
    }
}

/// `parser.intValue()`: numbers (truncated) and numeric strings.
fn parse_int(v: &Value) -> Result<i64, EsError> {
    match v {
        Value::Num(n) => Ok(n.as_i64().unwrap_or(n.as_f64() as i64)),
        Value::Str(s) => s
            .trim()
            .parse::<i64>()
            .or_else(|_| s.trim().parse::<f64>().map(|f| f as i64))
            .map_err(|_| EsError::java(400, "number_format_exception", format!("For input string: \"{s}\""))),
        other => Err(EsError::java(400, "illegal_state_exception", format!("Can't get text on a {} at 1:1", token_name(other)))),
    }
}

/// Parses the `commands` array (`AllocationCommands.fromXContent`).
fn parse_commands(raw: &[u8], body: &Map) -> EsResult<Vec<Command>> {
    let mut out = Vec::new();
    for (key, v) in body {
        if key != "commands" {
            let (l, c) = locate(raw, &[Seg::Key(key)], Target::Name).unwrap_or((1, 2));
            return Err(EsError::x_content_parse(l, c, format!("[cluster_reroute] unknown field [{key}]")));
        }
        let at = locate(raw, &[Seg::Key("commands")], Target::Value).unwrap_or((1, 1));
        let list = match v {
            Value::Arr(list) => list,
            Value::Obj(_) => {
                return Err(wrap(at, EsError::parse("expected either field name [commands], or start array, got [START_OBJECT] instead")));
            }
            Value::Null => continue,
            other => {
                return Err(EsError::x_content_parse(at.0, at.1, format!("[cluster_reroute] commands doesn't support values of type: {}", token_name(other))));
            }
        };
        for (i, item) in list.iter().enumerate() {
            let item_path = [Seg::Key("commands"), Seg::Index(i)];
            let Value::Obj(cmd) = item else {
                let at = locate(raw, &item_path, Target::Value).unwrap_or((1, 1));
                return Err(wrap(at, EsError::parse(format!("allocation command is malformed, got [{}] instead", token_name(item)))));
            };
            let mut entries = cmd.iter();
            let Some((name, params)) = entries.next() else {
                let at = value_end(raw, &item_path).unwrap_or((1, 1));
                return Err(wrap(at, EsError::parse("allocation command is malformed, done parsing a command, but didn't get END_OBJECT, got [END_OBJECT] instead")));
            };
            let mut path = item_path.to_vec();
            path.push(Seg::Key(name));
            let parsed = parse_command(raw, &path, name, params)?;
            if let Some((second, sv)) = entries.next() {
                let mut p = item_path.to_vec();
                p.push(Seg::Key(second));
                let at = locate(raw, &p, Target::Name).unwrap_or((1, 1));
                let token = if matches!(sv, Value::Obj(_)) { "START_OBJECT" } else { "FIELD_NAME" };
                return Err(wrap(at, EsError::parse(format!("allocation command is malformed, done parsing a command, but didn't get END_OBJECT, got [{token}] instead"))));
            }
            out.push(parsed);
        }
    }
    Ok(out)
}

fn parse_command(raw: &[u8], path: &[Seg], name: &str, params: &Value) -> EsResult<Command> {
    let value_at = |key: &str| {
        let mut p = path.to_vec();
        p.push(Seg::Key(key));
        locate(raw, &p, Target::Value).unwrap_or((1, 1))
    };
    let name_at = |key: &str| {
        let mut p = path.to_vec();
        p.push(Seg::Key(key));
        locate(raw, &p, Target::Name).unwrap_or((1, 1))
    };
    let end_at = || value_end(raw, path).unwrap_or((1, 1));
    let kind = match name {
        "cancel" | "move" => None,
        "allocate_replica" => Some(AllocateKind::Replica),
        "allocate_empty_primary" => Some(AllocateKind::EmptyPrimary),
        "allocate_stale_primary" => Some(AllocateKind::StalePrimary),
        other => {
            let at = locate(raw, path, Target::Value).unwrap_or((1, 1));
            return Err(wrap(at, EsError::new(400, "named_object_not_found_exception", format!("[{}:{}] unknown field [{other}]", at.0, at.1))));
        }
    };
    let Value::Obj(fields) = params else {
        // The hand-written parsers read the next token: an empty array's END_ARRAY, or the
        // first element.
        let at = value_end(raw, path).unwrap_or((1, 1));
        let token = match params {
            Value::Arr(a) if a.is_empty() => "END_ARRAY",
            Value::Arr(a) => token_name(&a[0]),
            _ => "VALUE",
        };
        return Err(wrap(at, EsError::parse(format!("[{name}] command does not support complex json tokens [{token}]"))));
    };
    if let Some(kind) = kind {
        // `ObjectParser`: located errors, then the builder's validation.
        let (mut index, mut shard, mut node, mut accept) = (None, -1i64, None, false);
        for (k, v) in fields {
            let r: EsResult<()> = (|| {
                match k.as_str() {
                    "index" => index = Some(v.to_es_string()),
                    "shard" => shard = parse_int(v)?,
                    "node" => node = Some(v.to_es_string()),
                    "accept_data_loss" if kind != AllocateKind::Replica => accept = parse_bool(v)?,
                    _ => {
                        let at = name_at(k);
                        return Err(EsError::x_content_parse(at.0, at.1, format!("[{name}] unknown field [{k}]")));
                    }
                }
                Ok(())
            })();
            if let Err(e) = r {
                let at = value_at(k);
                if e.kind == "x_content_parse_exception" {
                    return Err(wrap(at, e));
                }
                let inner = EsError::x_content_parse(at.0, at.1, format!("[{name}] failed to parse field [{k}]")).caused_by(e);
                return Err(wrap(at, inner));
            }
        }
        let invalid = if index.is_none() {
            Some("Argument [index] must be defined")
        } else if shard < 0 {
            Some("Argument [shard] must be defined and non-negative")
        } else if node.is_none() {
            Some("Argument [node] must be defined")
        } else {
            None
        };
        if let Some(msg) = invalid {
            return Err(wrap(end_at(), EsError::illegal_argument(msg)));
        }
        return Ok(Command::Allocate { kind, index: index.unwrap_or_default(), shard, node: node.unwrap_or_default(), accept_data_loss: accept });
    }
    // `CancelAllocationCommand` / `MoveAllocationCommand`: hand-written, unlocated errors.
    let (mut index, mut shard, mut node, mut to_node, mut allow_primary) = (None, -1i64, None, None, false);
    for (k, v) in fields {
        let r: EsResult<()> = (|| {
            if matches!(v, Value::Obj(_) | Value::Arr(_)) {
                let token = if matches!(v, Value::Obj(_)) { "START_OBJECT" } else { "START_ARRAY" };
                return Err(EsError::parse(format!("[{name}] command does not support complex json tokens [{token}]")));
            }
            match (name, k.as_str()) {
                (_, "index") => index = Some(v.to_es_string()),
                (_, "shard") => shard = parse_int(v)?,
                ("cancel", "node") => node = Some(v.to_es_string()),
                ("cancel", "allow_primary" | "allowPrimary") => allow_primary = parse_bool(v)?,
                ("move", "from_node" | "fromNode") => node = Some(v.to_es_string()),
                ("move", "to_node" | "toNode") => to_node = Some(v.to_es_string()),
                _ => return Err(EsError::parse(format!("[{name}] command does not support field [{k}]"))),
            }
            Ok(())
        })();
        if let Err(e) = r {
            return Err(wrap(value_at(k), e));
        }
    }
    let missing = if index.is_none() {
        Some("index")
    } else if shard == -1 {
        Some("shard")
    } else if node.is_none() {
        Some(if name == "move" { "from_node" } else { "node" })
    } else if name == "move" && to_node.is_none() {
        Some("to_node")
    } else {
        None
    };
    if let Some(p) = missing {
        return Err(wrap(end_at(), EsError::parse(format!("[{name}] command missing the {p} parameter"))));
    }
    let (index, node) = (index.unwrap_or_default(), node.unwrap_or_default());
    Ok(if name == "move" {
        Command::Move { index, shard, from_node: node, to_node: to_node.unwrap_or_default() }
    } else {
        Command::Cancel { index, shard, node, allow_primary }
    })
}

/// `DiscoveryNodes.resolveNode`: exactly one node must match.
fn resolve_node(ctx: &Ctx, node: &str) -> EsResult<()> {
    if super::nodes::selects_node(ctx, &[node.to_string()]) {
        Ok(())
    } else {
        Err(EsError::illegal_argument(format!("failed to resolve [{node}], no matching nodes")))
    }
}

fn decision(decider: &str, yes: bool, explanation: impl Into<String>) -> Value {
    obj! { "decider" => decider.to_string(), "decision" => if yes { "YES" } else { "NO" }, "explanation" => explanation.into() }
}

/// `ShardRouting.toString()` of the started primary.
fn primary_routing(ctx: &Ctx, idx: &IndexMeta, shard: u32) -> String {
    format!(
        "[{}][{shard}], node[{}], [P], s[STARTED], a[id={}], failed_attempts[0]",
        idx.name,
        ctx.cluster.node.id,
        super::cluster::allocation_id(idx, shard)
    )
}

/// Every allocation decider's decision for placing a copy of `idx` on the node, as
/// Elasticsearch's default distribution lists them in explain mode.
fn allocation_deciders(ctx: &Ctx, idx: &IndexMeta, shard: u32, relocating_primary: bool) -> Vec<Value> {
    let m = super::nodes::NodeMetrics::sample(ctx.cluster);
    let version = crate::version::ES_VERSION;
    let index_version = crate::version::INDEX_VERSION_STRING;
    let (index_version_text, node_version_text, after_primary) = if relocating_primary {
        (
            format!("can relocate primary shard from a node with index version [{index_version}] to a node with equal-or-newer index version [{index_version}]"),
            format!("can relocate primary shard from a node with version [{version}] to a node with equal-or-newer version [{version}]"),
            "shard is primary and can be allocated",
        )
    } else {
        (
            format!("can allocate replica shard to a node with index version [{index_version}] since this is equal-or-newer than the primary index version [{index_version}]"),
            format!("can allocate replica shard to a node with version [{version}] since this is equal-or-newer than the primary version [{version}]"),
            "primary shard for this replica is already active",
        )
    };
    let same_shard = if relocating_primary {
        format!("this shard is already allocated to this node [{}]", primary_routing(ctx, idx, shard))
    } else {
        format!("a copy of this shard is already allocated to this node [{}]", primary_routing(ctx, idx, shard))
    };
    let used = (m.disk_total - m.disk_avail).max(0);
    let used_pct = if m.disk_total > 0 { 100.0 * used as f64 / m.disk_total as f64 } else { 0.0 };
    let free = crate::units::ByteSize(m.disk_avail).human();
    let tiers = idx.settings.get("index.routing.allocation.include._tier_preference").unwrap_or("data_content").to_string();
    vec![
        decision("max_retry", true, "shard has no previous failures"),
        decision("replica_after_primary_active", true, after_primary),
        decision("enable", true, "explicitly ignoring any disabling of allocation due to manual allocation commands via the reroute API"),
        decision("index_version", true, index_version_text),
        decision("node_version", true, node_version_text),
        decision("snapshot_in_progress", true, "no snapshots are currently running"),
        decision("restore_in_progress", true, "ignored as shard is not being recovered from a snapshot"),
        decision("node_shutdown", true, "no nodes are shutting down"),
        decision("write_load", true, "Decider is disabled"),
        decision("node_replacement", true, "there are no ongoing node replacements"),
        decision("filter", true, "node passes include/exclude/require filters"),
        decision("same_shard", false, same_shard),
        decision(
            "disk_threshold",
            true,
            format!("enough disk for shard on node, free: [{free}], used: [{used_pct:.1}%], shard size: [0b], free after allocating shard: [{free}]"),
        ),
        decision("throttling", true, "below shard recovery limit of outgoing: [0 < 2] incoming: [0 < 2]"),
        decision("shards_limit", true, "total shard limits are disabled: [index: -1, cluster: -1] <= 0"),
        decision(
            "awareness",
            true,
            "allocation awareness is not enabled, set cluster setting [cluster.routing.allocation.awareness.attributes] to enable it",
        ),
        decision("index_balance", true, "Decider is disabled."),
        decision("data_tier", true, format!("index has a preference for tiers [{tiers}] and node has tier [data]")),
        decision("ccr_primary_follower", true, "shard is not a follower and is not under the purview of this decider"),
        decision("searchable_snapshots", true, "decider only applicable for indices backed by searchable snapshots"),
        decision("searchable_snapshot_repository_exists", true, "this decider only applies to indices backed by searchable snapshots"),
        decision("searchable_snapshots_enable", true, "decider only applicable for indices backed by searchable snapshots"),
        decision("dedicated_frozen_node", true, "this node's data roles are not exactly [data_frozen] so it is not a dedicated frozen node"),
        decision("archive", true, "decider only applicable for indices backed by archive functionality"),
    ]
}

/// Runs one command; `Ok(decisions)` with `explain`, an error otherwise when it's rejected.
fn execute(ctx: &Ctx, cmd: &Command, explain: bool) -> EsResult<Vec<Value>> {
    let st = ctx.state();
    let node_str = super::nodes::discovery_node_string(ctx.cluster);
    let reject = |decider: String, reason: String, throw: String| -> EsResult<Vec<Value>> {
        if explain { Ok(vec![decision(&decider, false, reason)]) } else { Err(EsError::illegal_argument(throw)) }
    };
    match cmd {
        Command::Cancel { index, shard, node, allow_primary } => {
            resolve_node(ctx, node)?;
            let Some(idx) = st.metadata.indices.get(index) else {
                return Err(EsError::index_not_found_bare(index));
            };
            if *shard < 0 || *shard >= idx.number_of_shards() as i64 {
                return reject(
                    "cancel_allocation_command".into(),
                    format!("can't cancel [{index}][{shard}], failed to find it on node {node_str}"),
                    format!("[cancel_allocation] can't cancel [{index}][{shard}], failed to find it on node {node_str}"),
                );
            }
            if !allow_primary {
                return reject(
                    "cancel_allocation_command".into(),
                    format!("can't cancel [{index}][{shard}] on node {node_str}, shard is primary and started"),
                    format!("[cancel_allocation] can't cancel [{index}][{shard}] on node {node_str}, shard is primary and started"),
                );
            }
            Ok(vec![decision("cancel_allocation_command", true, format!("shard [{index}][{shard}] on node {node_str} can be cancelled"))])
        }
        Command::Move { index, shard, from_node, to_node } => {
            resolve_node(ctx, from_node)?;
            resolve_node(ctx, to_node)?;
            let found = st.metadata.indices.get(index).filter(|i| *shard >= 0 && *shard < i.number_of_shards() as i64 && !i.is_closed());
            let Some(idx) = found else {
                return reject(
                    "move_allocation_command".into(),
                    format!("shard [{index}][{shard}] not found"),
                    format!("[move_allocation] can't move [{index}][{shard}], failed to find it on node {node_str}"),
                );
            };
            let decisions = allocation_deciders(ctx, idx, *shard as u32, true);
            if explain {
                return Ok(decisions);
            }
            Err(EsError::illegal_argument(format!(
                "[move_allocation] can't move [{index}][{shard}], from {node_str}, to {node_str}, since its not allowed, reason: {}",
                decision_string(&decisions)
            )))
        }
        Command::Allocate { kind, index, shard, node, accept_data_loss: _ } => {
            let name = kind.name();
            let decider = format!("{name} (allocation command)");
            if let Err(e) = resolve_node(ctx, node) {
                let reason = e.reason.clone().unwrap_or_default();
                return if explain { Ok(vec![decision(&decider, false, reason)]) } else { Err(e) };
            }
            let Some(idx) = st.metadata.indices.get(index) else {
                let e = EsError::index_not_found_bare(index);
                return if explain { Ok(vec![decision(&decider, false, format!("no such index [{index}]"))]) } else { Err(e) };
            };
            if *shard >= idx.number_of_shards() as i64 {
                let e = EsError::new(404, "shard_not_found_exception", "no such shard")
                    .with_meta("index_uuid", idx.uuid.clone())
                    .with_meta("shard", shard.to_string())
                    .with_meta("index", idx.name.clone());
                return if explain { Ok(vec![decision(&decider, false, "no such shard")]) } else { Err(e) };
            }
            match kind {
                AllocateKind::Replica => {
                    if idx.number_of_replicas() == 0 {
                        return reject(
                            decider,
                            format!("all copies of [{index}][{shard}] are already assigned. Use the move allocation command instead"),
                            format!("[{name}] all copies of [{index}][{shard}] are already assigned. Use the move allocation command instead"),
                        );
                    }
                    let decisions = allocation_deciders(ctx, idx, *shard as u32, false);
                    if explain {
                        return Ok(decisions);
                    }
                    Err(EsError::illegal_argument(format!(
                        "[{name}] allocation of [{index}][{shard}] on node {node_str} is not allowed, reason: {}",
                        decision_string(&decisions)
                    )))
                }
                _ => reject(
                    decider,
                    format!("primary [{index}][{shard}] is already assigned"),
                    format!("[{name}] primary [{index}][{shard}] is already assigned"),
                ),
            }
        }
    }
}

/// `Decision.Multi.toString()`: the decisions that said NO (in explain mode, all of them).
fn decision_string(decisions: &[Value]) -> String {
    let parts: Vec<String> = decisions
        .iter()
        .map(|d| {
            format!(
                "[{}({}): {}]",
                d.get("decision").and_then(Value::as_str).unwrap_or(""),
                d.get("decider").and_then(Value::as_str).unwrap_or(""),
                d.get("explanation").and_then(Value::as_str).unwrap_or("")
            )
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

/// `POST _cluster/reroute`.
pub fn reroute(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let explain = ctx.bool_param("explain", false)?;
    ctx.bool_param("dry_run", false)?;
    ctx.bool_param("retry_failed", false)?;
    let os = ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch;
    if ctx.has_query("metric") && !os {
        ctx.warn("the [?metric] query parameter to the [POST /_cluster/reroute] API has no effect; its use will be forbidden in a future version");
    }
    let body = ctx.body_map()?.unwrap_or_default();
    let raw = ctx.raw_json().unwrap_or(b"");
    let commands = parse_commands(raw, &body)?;
    let mut explanations = Vec::with_capacity(commands.len());
    for cmd in &commands {
        let decisions = execute(ctx, cmd, explain)?;
        explanations.push(obj! { "command" => cmd.name(), "parameters" => cmd.parameters(), "decisions" => Value::Arr(decisions) });
    }
    let mut out = obj! { "acknowledged" => true };
    if os {
        // OpenSearch answers with the resulting cluster state (without metadata unless asked).
        let metrics: Vec<String> = ctx.list_param("metric").unwrap_or_else(|| {
            ["version", "master_node", "cluster_manager_node", "blocks", "nodes", "routing_table", "routing_nodes"].iter().map(|s| s.to_string()).collect()
        });
        let state = super::cluster::state_value(ctx, &metrics, false)?;
        if let Some(o) = out.as_object_mut() {
            o.insert("state".into(), Value::Obj(state));
        }
    }
    if explain && let Some(o) = out.as_object_mut() {
        o.insert("explanations".into(), Value::Arr(explanations));
    }
    Reply::ok(out)
}
