//! The task management APIs (`GET _tasks`, `GET _tasks/<id>`, `POST _tasks[/<id>]/_cancel`) over
//! the cluster's [`crate::cluster::TaskRegistry`].
//!
//! Like Elasticsearch, listing tasks is itself a task (`cluster:monitor/tasks/lists`) with a
//! node-level child (`[n]`), both carrying the request's `X-Opaque-Id`; long-running operations
//! register theirs. Task ids are `<node id>:<number>`; ids of other nodes, or of tasks no longer
//! running, are answered as Elasticsearch answers for tasks that left no stored result.

use crate::cluster::TaskInfo;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xcontent::filter::glob_match;

/// Headers a task keeps from its request.
fn task_headers(ctx: &Ctx) -> Vec<(String, String)> {
    ctx.request.header("x-opaque-id").map(|v| vec![("X-Opaque-Id".to_string(), v.to_string())]).unwrap_or_default()
}

fn task_value(ctx: &Ctx, t: &TaskInfo, detailed: bool) -> Value {
    task_value_at(ctx, t, detailed, t.started.elapsed().as_nanos() as i64)
}

fn task_value_at(ctx: &Ctx, t: &TaskInfo, detailed: bool, running_nanos: i64) -> Value {
    let node = &ctx.cluster.node;
    let mut m = Map::new();
    m.insert("node".into(), Value::from(node.id.clone()));
    m.insert("id".into(), Value::from(t.id));
    m.insert("type".into(), "transport".into());
    m.insert("action".into(), Value::from(t.action.to_string()));
    // A running by-query task reports its progress (`BulkByScrollTask.Status`).
    if detailed && let Some(live) = &t.live {
        m.insert("status".into(), live.status_value());
    }
    if detailed {
        m.insert("description".into(), Value::from(t.description_in(ctx.cluster)));
    }
    m.insert("start_time_in_millis".into(), Value::from(t.start_ms));
    m.insert("running_time_in_nanos".into(), Value::from(running_nanos));
    m.insert("cancellable".into(), t.cancellable.into());
    // Elasticsearch reports `cancelled` only for cancellable tasks; OpenSearch always.
    if t.cancellable || ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch {
        m.insert("cancelled".into(), t.cancelled.load(std::sync::atomic::Ordering::Relaxed).into());
    }
    if let Some(p) = t.parent {
        m.insert("parent_task_id".into(), Value::from(format!("{}:{p}", node.id)));
    }
    let mut h = Map::new();
    for (k, v) in &t.headers {
        h.insert(k.clone(), Value::from(v.clone()));
    }
    m.insert("headers".into(), Value::Obj(h));
    // OpenSearch adds per-task resource usage to detailed listings (in its HashMap order). roll
    // doesn't account CPU or memory per task, so the counters are zero.
    if detailed && ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch {
        let usage = || obj! { "cpu_time_in_nanos" => 0, "memory_in_bytes" => 0 };
        m.insert(
            "resource_stats".into(),
            obj! {
                "average" => usage(), "total" => usage(), "min" => usage(), "max" => usage(),
                "thread_info" => obj! { "thread_executions" => 0, "active_threads" => 0 },
            },
        );
    }
    Value::Obj(m)
}

/// `TaskId.fromString`: `<node>:<number>`.
fn parse_task_id(s: &str) -> EsResult<(String, u64)> {
    let malformed = || EsError::illegal_argument(format!("malformed task id {s}"));
    let (node, n) = s.split_once(':').ok_or_else(malformed)?;
    if node.is_empty() {
        return Err(malformed());
    }
    let n = n.parse::<u64>().map_err(|_| malformed().caused_by(EsError::new(400, "number_format_exception", format!("For input string: \"{n}\""))))?;
    Ok((node.to_string(), n))
}

fn tasks_index_missing() -> EsError {
    EsError::index_not_found(".tasks")
}

/// `GET _tasks`.
pub async fn list(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let group_by = ctx.query("group_by").unwrap_or("nodes");
    if !matches!(group_by, "nodes" | "parents" | "none") {
        return Err(EsError::illegal_argument(format!("[group_by] must be one of [nodes], [parents] or [none] but was [{group_by}]")));
    }
    let detailed = ctx.bool_param("detailed", false)?;
    let wait = ctx.bool_param("wait_for_completion", false)?;
    let timeout = ctx.time_param("timeout", crate::units::TimeValue::seconds(30))?;
    let actions: Vec<String> = ctx.list_param("actions").unwrap_or_default();
    // `wait_for_completion`: until no matching task (other than task listings) runs.
    if wait {
        let tasks = &ctx.cluster.tasks;
        let busy = || tasks.list().iter().any(|t| !t.action.starts_with("cluster:monitor/tasks/lists") && (actions.is_empty() || actions.iter().any(|a| glob_match(a, &t.action))));
        tasks.wait_until(std::time::Duration::from_millis(timeout.as_millis().max(0) as u64), || !busy()).await;
    }
    let nodes: Vec<String> = ctx.list_param("nodes").unwrap_or_default();
    let parent = ctx.query("parent_task_id").filter(|p| !p.is_empty() && *p != "unset");
    let headers = task_headers(ctx);
    // Listing tasks is a task, with a child on the node.
    let registry = &ctx.cluster.tasks;
    let me = registry.register("cluster:monitor/tasks/lists", "", None, headers.clone());
    let _child = registry.register("cluster:monitor/tasks/lists[n]", "", Some(me.id), headers);
    let node_selected = nodes.is_empty() || super::nodes::selects_node(ctx, &nodes);
    let node_id = ctx.cluster.node.id.clone();
    let tasks: Vec<TaskInfo> = registry
        .list()
        .into_iter()
        .filter(|_| node_selected)
        .filter(|t| actions.is_empty() || actions.iter().any(|a| glob_match(a, &t.action)))
        .filter(|t| parent.is_none_or(|p| t.parent.is_some_and(|pid| format!("{node_id}:{pid}") == p)))
        .collect();
    let body = match group_by {
        "none" => obj! { "tasks" => Value::Arr(tasks.iter().map(|t| task_value(ctx, t, detailed)).collect()) },
        "parents" => {
            let mut out = Map::new();
            for t in tasks.iter().filter(|t| t.parent.is_none_or(|p| !tasks.iter().any(|x| x.id == p))) {
                let mut v = task_value(ctx, t, detailed);
                let children: Vec<Value> = tasks.iter().filter(|c| c.parent == Some(t.id)).map(|c| task_value(ctx, c, detailed)).collect();
                if !children.is_empty()
                    && let Some(m) = v.as_object_mut()
                {
                    m.insert("children".into(), Value::Arr(children));
                }
                out.insert(format!("{node_id}:{}", t.id), v);
            }
            obj! { "tasks" => Value::Obj(out) }
        }
        _ => by_node(ctx, &tasks, detailed),
    };
    let mut v = body;
    if ctx.human {
        crate::units::humanize(&mut v);
    }
    Reply::ok(v)
}

/// Tasks grouped by node (`ListTasksResponse` with `group_by=nodes`): `{"nodes": {..}}`.
pub fn by_node(ctx: &Ctx, tasks: &[TaskInfo], detailed: bool) -> Value {
    let node_id = ctx.cluster.node.id.clone();
    let mut nodes_out = Map::new();
    if !tasks.is_empty() {
        let node = &ctx.cluster.node;
        let m = super::nodes::NodeMetrics::sample(ctx.cluster);
        let mut attrs = Map::new();
        for (k, v) in super::nodes::node_attributes(ctx.cluster, &m) {
            attrs.insert(k, v.into());
        }
        let mut tm = Map::new();
        for t in tasks {
            tm.insert(format!("{node_id}:{}", t.id), task_value(ctx, t, detailed));
        }
        nodes_out.insert(
            node_id,
            obj! {
                "name" => node.name.clone(),
                "transport_address" => node.transport_address.clone(),
                "host" => node.host.clone(),
                "ip" => node.transport_address.clone(),
                "roles" => super::nodes::roles(ctx.cluster.dialect).to_vec(),
                "attributes" => Value::Obj(attrs),
                "tasks" => Value::Obj(crate::json::java_hash_map(tm, 16)),
            },
        );
    }
    obj! { "nodes" => Value::Obj(nodes_out) }
}

/// A node-level failure as the task APIs report it (`failed_node_exception`).
pub fn node_failure(node: &str, reason: EsError) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), "failed_node_exception".into());
    m.insert("reason".into(), Value::from(format!("Failed node [{node}]")));
    m.insert("node_id".into(), Value::from(node.to_string()));
    m.insert("caused_by".into(), reason.to_value(false));
    obj! { "node_failures" => Value::Arr(vec![Value::Obj(m)]), "nodes" => obj! {} }
}

/// `GET _tasks/<task_id>`.
pub async fn get(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let raw = ctx.path_param("task_id").unwrap_or_default();
    let (node, n) = parse_task_id(raw)?;
    let wait = ctx.bool_param("wait_for_completion", false)?;
    let timeout = ctx.time_param("timeout", crate::units::TimeValue::seconds(30))?;
    if wait && node == ctx.cluster.node.id && ctx.cluster.tasks.get(n).is_some() {
        let tasks = &ctx.cluster.tasks;
        if !tasks.wait_until(std::time::Duration::from_millis(timeout.as_millis().max(0) as u64), || tasks.get(n).is_none()).await {
            return Err(EsError::new(500, "elasticsearch_timeout_exception", format!("Timed out waiting for completion of task [{raw}]")));
        }
    }
    if node != ctx.cluster.node.id {
        return Err(EsError::resource_not_found(format!(
            "task [{raw}] belongs to the node [{node}] which isn't part of the cluster and there is no record of the task"
        ))
        .caused_by(EsError::resource_not_found(format!("task [{raw}] isn't running and hasn't stored its results")).caused_by(tasks_index_missing())));
    }
    if let Some(done) = ctx.cluster.tasks.stored(n) {
        return Reply::ok(result_value(ctx, &done));
    }
    // Results of earlier lives of this cluster stay in the `.tasks` index.
    if let Some(v) = persisted_result(ctx, raw)? {
        return Reply::ok(v);
    }
    match ctx.cluster.tasks.get(n) {
        Some(t) => Reply::ok(obj! { "completed" => false, "task" => task_value(ctx, &t, true) }),
        None => Err(EsError::resource_not_found(format!("task [{raw}] isn't running and hasn't stored its results")).caused_by(tasks_index_missing())),
    }
}

/// `POST _tasks[/<task_id>]/_cancel`.
pub fn cancel(ctx: &Ctx) -> EsResult<Reply> {
    let actions: Vec<String> = ctx.list_param("actions").unwrap_or_default();
    let nodes: Vec<String> = ctx.list_param("nodes").unwrap_or_default();
    let node_id = ctx.cluster.node.id.clone();
    // A task that can't be found fails its node (the one named by the id).
    let failure = node_failure;
    if let Some(raw) = ctx.path_param("task_id") {
        let (node, n) = parse_task_id(raw)?;
        let task = if node == node_id { ctx.cluster.tasks.get(n) } else { None };
        return match task {
            Some(t) if !t.cancellable => Reply::ok(failure(&node, EsError::illegal_argument(format!("task [{raw}] doesn't support cancellation")))),
            Some(t) => {
                t.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                // The cancelled task as it now stands (`CancelTasksResponse`).
                Reply::ok(by_node(ctx, &[t], false))
            }
            None => Reply::ok(failure(&node, EsError::resource_not_found(format!("task [{raw}] is not found")))),
        };
    }
    // Without filters the cancel request matches (and cancels) itself.
    let me = ctx.cluster.tasks.register("cluster:admin/tasks/cancel", "", None, task_headers(ctx));
    let node_selected = nodes.is_empty() || super::nodes::selects_node(ctx, &nodes);
    let matched: Vec<TaskInfo> =
        ctx.cluster.tasks.list().into_iter().filter(|_| node_selected).filter(|t| actions.is_empty() || actions.iter().any(|a| glob_match(a, &t.action))).collect();
    if matched.iter().any(|t| t.id == me.id) {
        return Reply::ok(failure(&node_id, EsError::new(400, "task_cancelled_exception", "task cancelled [by user request]")));
    }
    // Only cancellable tasks are cancelled and listed.
    let matched: Vec<TaskInfo> = matched.into_iter().filter(|t| t.cancellable).collect();
    for t in &matched {
        t.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Reply::ok(by_node(ctx, &matched, false))
}

/// A finished task as `GET _tasks/<id>` (and the `.tasks` index) shows it (`TaskResult`).
fn result_value(ctx: &Ctx, done: &crate::cluster::StoredTask) -> Value {
    let mut task = task_value_at(ctx, &done.info, true, done.running_nanos);
    // A finished by-query task keeps its last `BulkByScrollTask.Status` (the response's
    // counters) in its task info: its live status, else the response's counters.
    if done.info.live.is_some() {
        return {
            let mut out = obj! { "completed" => true, "task" => task };
            if let Some(o) = out.as_object_mut() {
                match &done.outcome {
                    Ok(v) => o.insert("response".into(), v.clone()),
                    Err(e) => o.insert("error".into(), e.to_value(false)),
                };
            }
            out
        };
    }
    if (done.info.action.ends_with("byquery") || done.info.action == "indices:data/write/reindex")
        && let (Ok(Value::Obj(resp)), Value::Obj(t)) = (&done.outcome, &mut task)
    {
        let mut status = Map::new();
        for k in ["slice_id", "total", "updated", "created", "deleted", "batches", "version_conflicts", "noops", "retries", "throttled_millis", "requests_per_second", "canceled", "throttled_until_millis", "slices"] {
            if let Some(v) = resp.get(k) {
                status.insert(k.into(), v.clone());
            }
        }
        let at = t.get_index_of("description").unwrap_or(t.len());
        t.shift_insert(at, "status".into(), Value::Obj(status));
    }
    let mut out = obj! { "completed" => true, "task" => task };
    if let Some(o) = out.as_object_mut() {
        match &done.outcome {
            Ok(v) => o.insert("response".into(), v.clone()),
            Err(e) => o.insert("error".into(), e.to_value(false)),
        };
    }
    out
}

/// The `.tasks` mapping (`TaskResultsService`): strict, with the opaque parts disabled.
fn tasks_mapping() -> Map {
    let off = || obj! { "type" => "object", "enabled" => false };
    let kw = || obj! { "type" => "keyword" };
    let long = || obj! { "type" => "long" };
    let boolean = || obj! { "type" => "boolean" };
    let task = obj! {
        "properties" => obj! {
            "action" => kw(),
            "cancellable" => boolean(),
            "cancelled" => boolean(),
            "description" => obj! { "type" => "text" },
            "headers" => off(),
            "id" => long(),
            "node" => kw(),
            "parent_task_id" => kw(),
            "running_time_in_nanos" => long(),
            "start_time_in_millis" => long(),
            "status" => off(),
            "type" => kw(),
        }
    };
    match obj! {
        "dynamic" => "strict",
        "properties" => obj! { "completed" => boolean(), "error" => off(), "response" => off(), "task" => task },
    } {
        Value::Obj(m) => m,
        _ => Map::new(),
    }
}

/// Stores a finished task's result in the `.tasks` system index, as
/// `TaskResultsService.storeResult` does (creating the index on first use): its documents
/// outlive this process's memory of the task.
pub async fn persist_result(ctx: &Ctx<'_>, id: u64) -> EsResult<()> {
    let Some(done) = ctx.cluster.tasks.stored(id) else { return Ok(()) };
    let doc = result_value(ctx, &done);
    let cluster = ctx.cluster;
    let now = ctx.now_ms;
    if !cluster.state().metadata.indices.contains_key(".tasks") {
        cluster
            .update(move |st| {
                if st.metadata.indices.contains_key(".tasks") {
                    return Ok(());
                }
                let mut req = super::indices::CreateRequest::default();
                req.settings.set("index.number_of_shards", "1");
                req.settings.set("index.auto_expand_replicas", "0-1");
                req.mappings = Some(tasks_mapping());
                super::indices::create_index(st, ".tasks", ".tasks", req, now).map(|_| ())
            })
            .await?;
    }
    let Some(idx) = cluster.state().metadata.indices.get(".tasks").cloned() else { return Ok(()) };
    let doc_id = format!("{}:{id}", cluster.node.id);
    let source = bytes::Bytes::from(doc.to_json());
    let p = crate::write::prepare(cluster, idx, &doc_id, None, &source, &doc, None).await?;
    crate::write::apply_index(p, &crate::write::WriteArgs::default(), false, &doc_id)?;
    Ok(())
}

/// A task result stored in the `.tasks` index by an earlier life of the cluster.
fn persisted_result(ctx: &Ctx, raw: &str) -> EsResult<Option<Value>> {
    let st = ctx.state();
    let Some(idx) = st.metadata.indices.get(".tasks").cloned() else { return Ok(None) };
    let args = crate::api::document::GetArgs { realtime: true, ..Default::default() };
    let Some(got) = crate::api::document::get_one(ctx, &idx, raw, &args)? else { return Ok(None) };
    match crate::fetch::source_bytes(&got.doc.fields) {
        Some(b) => Ok(Some(crate::fetch::parse_source(b)?)),
        None => Ok(None),
    }
}
