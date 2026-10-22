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
    m.insert("action".into(), Value::from(t.action.clone()));
    if detailed {
        m.insert("description".into(), Value::from(t.description.clone()));
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
pub fn list(ctx: &Ctx) -> EsResult<Reply> {
    let group_by = ctx.query("group_by").unwrap_or("nodes");
    if !matches!(group_by, "nodes" | "parents" | "none") {
        return Err(EsError::illegal_argument(format!("[group_by] must be one of [nodes], [parents] or [none] but was [{group_by}]")));
    }
    let detailed = ctx.bool_param("detailed", false)?;
    ctx.bool_param("wait_for_completion", false)?;
    ctx.time_param("timeout", crate::units::TimeValue::seconds(30))?;
    let actions: Vec<String> = ctx.list_param("actions").unwrap_or_default();
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
        _ => {
            let mut nodes_out = Map::new();
            if !tasks.is_empty() {
                let node = &ctx.cluster.node;
                let m = super::nodes::NodeMetrics::sample(ctx.cluster);
                let mut attrs = Map::new();
                for (k, v) in super::nodes::node_attributes(ctx.cluster, &m) {
                    attrs.insert(k, v.into());
                }
                let mut tm = Map::new();
                for t in &tasks {
                    tm.insert(format!("{node_id}:{}", t.id), task_value(ctx, t, detailed));
                }
                nodes_out.insert(
                    node_id.clone(),
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
    };
    let mut v = body;
    if ctx.human {
        crate::units::humanize(&mut v);
    }
    Reply::ok(v)
}

/// `GET _tasks/<task_id>`.
pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    let raw = ctx.path_param("task_id").unwrap_or_default();
    let (node, n) = parse_task_id(raw)?;
    ctx.bool_param("wait_for_completion", false)?;
    ctx.time_param("timeout", crate::units::TimeValue::seconds(30))?;
    if node != ctx.cluster.node.id {
        return Err(EsError::resource_not_found(format!(
            "task [{raw}] belongs to the node [{node}] which isn't part of the cluster and there is no record of the task"
        ))
        .caused_by(EsError::resource_not_found(format!("task [{raw}] isn't running and hasn't stored its results")).caused_by(tasks_index_missing())));
    }
    if let Some(done) = ctx.cluster.tasks.stored(n) {
        let mut out = obj! { "completed" => true, "task" => task_value_at(ctx, &done.info, true, done.running_nanos) };
        if let Some(o) = out.as_object_mut() {
            match &done.outcome {
                Ok(v) => o.insert("response".into(), v.clone()),
                Err(e) => o.insert("error".into(), e.to_value(false)),
            };
        }
        return Reply::ok(out);
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
    let failure = |node: &str, reason: EsError| {
        let mut m = Map::new();
        m.insert("type".into(), "failed_node_exception".into());
        m.insert("reason".into(), Value::from(format!("Failed node [{node}]")));
        m.insert("node_id".into(), Value::from(node.to_string()));
        m.insert("caused_by".into(), reason.to_value(false));
        obj! { "node_failures" => Value::Arr(vec![Value::Obj(m)]), "nodes" => obj! {} }
    };
    if let Some(raw) = ctx.path_param("task_id") {
        let (node, n) = parse_task_id(raw)?;
        let task = if node == node_id { ctx.cluster.tasks.get(n) } else { None };
        return match task {
            Some(t) => {
                t.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                Reply::ok(obj! { "nodes" => obj! {} })
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
    for t in &matched {
        t.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    Reply::ok(obj! { "nodes" => obj! {} })
}
