//! Running a monitor (`MonitorRunnerService`): its inputs, its triggers' conditions, their
//! actions (through notification channels, throttled per alert) and the alerts' new states.
//!
//! - **Query-level**: each search input's response is a result; each trigger's painless
//!   condition sees `ctx.results`, and its alert is created (`ACTIVE`, or `ERROR` when an input
//!   or the condition failed), updated, or `COMPLETED` (moved to the history) when the condition
//!   no longer holds. An `ACKNOWLEDGED` alert stays acknowledged and runs no actions.
//! - **Bucket-level**: each trigger selects the buckets of `parent_bucket_path` for which its
//!   script, given the `buckets_path` values as `params`, holds (`BucketSelectorExtAggregator`);
//!   each selected bucket has its own alert (new, deduped, or completed when it's gone) and its
//!   own actions.
//! - **Document-level**: each query (a query string) runs over the documents written since the
//!   last run, per shard by `_seq_no` (the plugin's `last_run_context`, kept in the job record);
//!   a trigger's expression (`query[id=…]`, `query[name=…]`, `query[tag=…]` with `&&`, `||`,
//!   `!`) picks the documents, each raising an alert.
//!
//! `_execute` runs a stored monitor (its alerts updated and its actions sent unless `dryrun`) or
//! a monitor in the body (nothing kept or sent).

use super::alerts::{self, New};
use super::{JOBS, Stored, alerting};
use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xpack::{enc, internal};
use search_painless::Context;

pub(crate) use super::super::random_uuid;

/// `Instant.toString()` of a millisecond time.
fn iso(ms: i64) -> String {
    crate::time::java_instant_string(ms)
}

/// One run of a monitor.
struct Run<'a> {
    cluster: &'a Cluster,
    /// `None` for a monitor given inline (nothing is kept).
    id: Option<String>,
    version: i64,
    monitor: Map,
    owner: Option<Value>,
    /// Alerts are kept and actions sent.
    live: bool,
    now: i64,
    period_start: i64,
    execution_id: String,
}

fn str_of(e: &EsError) -> String {
    e.reason().to_string()
}

impl Run<'_> {
    fn monitor_arg(&self) -> Value {
        let g = |k: &str| self.monitor.get(k).cloned().unwrap_or(Value::Null);
        obj! { "_id" => self.id.clone().unwrap_or_default(), "_version" => self.version, "name" => g("name"), "enabled" => g("enabled"), "monitor_type" => g("monitor_type") }
    }

    /// The `ctx` of a trigger's condition and its actions' templates.
    fn ctx(&self, trigger: &Map, results: &[Value], error: Option<&str>, extra: Map) -> Value {
        let mut m = obj! {
            "monitor" => self.monitor_arg(),
            "trigger" => obj! { "id" => trigger.get("id").cloned().unwrap_or(Value::Null), "name" => trigger.get("name").cloned().unwrap_or(Value::Null), "severity" => trigger.get("severity").cloned().unwrap_or(Value::Null), "actions" => trigger.get("actions").cloned().unwrap_or(Value::Arr(vec![])) },
            "results" => Value::Arr(results.to_vec()),
            "periodStart" => iso(self.period_start),
            "periodEnd" => iso(self.now),
            "error" => error.map(Value::from).unwrap_or(Value::Null),
        };
        if let Value::Obj(mm) = &mut m {
            mm.extend(extra);
        }
        m
    }

    /// Runs `f` as the monitor's owner (its searches authorized, and filtered, as that user).
    async fn as_owner<F: std::future::Future>(&self, f: F) -> F::Output {
        internal::as_principal(self.owner.as_ref().and_then(internal::principal_of), f).await
    }

    fn refusal(&self) -> Option<String> {
        let p = self.owner.as_ref().and_then(internal::principal_of);
        internal::principal_refusal(self.cluster, p.as_ref()).map(|e| str_of(&e))
    }

    fn user_doc(&self) -> Value {
        super::user_doc(self.owner.as_ref())
    }

    fn new_alert<'s>(&'s self, trigger: &'s Map, state: &'s str, error: Option<String>) -> New<'s> {
        static NULL: Value = Value::Null;
        New {
            monitor_id: self.id.as_deref().unwrap_or_default(),
            monitor_version: self.version,
            monitor_name: self.monitor.get("name").unwrap_or(&NULL),
            execution_id: &self.execution_id,
            user: self.user_doc(),
            trigger_id: trigger.get("id").and_then(Value::as_str).unwrap_or_default(),
            trigger_name: trigger.get("name").unwrap_or(&NULL),
            severity: trigger.get("severity").unwrap_or(&NULL),
            state,
            error,
            finding_ids: Vec::new(),
            related_doc_ids: Vec::new(),
            agg: None,
            now: self.now,
        }
    }

    /// Runs the monitor's search inputs: their responses, or the first error.
    async fn searches(&self) -> (Vec<Value>, Option<String>) {
        let mut results = Vec::new();
        for input in self.monitor.get("inputs").and_then(Value::as_array).into_iter().flatten() {
            let Some(s) = input.get("search") else { continue };
            let indices: Vec<String> = s.get("indices").and_then(Value::as_array).into_iter().flatten().map(Value::to_es_string).collect();
            let target = indices.iter().map(|i| enc(i)).collect::<Vec<_>>().join(",");
            let body = s.get("query").cloned().unwrap_or_else(|| obj! {});
            let call = internal::call(self.cluster, "POST", &format!("/{target}/_search"), Some(&body));
            match self.as_owner(call).await {
                Ok(r) => results.push(super::super::hash_ordered(r)),
                Err(e) => return (results, Some(str_of(&e))),
            }
        }
        (results, None)
    }

    /// Renders an action's templates and sends it (unless the run keeps nothing).
    async fn action(&self, action: &Map, model: &Value) -> Value {
        let render = |key: &str| -> EsResult<Option<String>> {
            let Some(t) = action.get(key) else { return Ok(None) };
            let src = t.get("source").map(Value::to_es_string).unwrap_or_default();
            let tpl = self.cluster.scripts.engine.compile_template(&src, Context::Template, &[]).map_err(crate::scripting::script_error)?;
            tpl.render_json(&crate::scripting::to_serde(&obj! { "ctx" => model.clone() })).map(Some).map_err(crate::scripting::script_error)
        };
        let id = action.get("id").cloned().unwrap_or(Value::Null);
        let name = action.get("name").cloned().unwrap_or(Value::Null);
        let mut output = Map::new();
        let mut error = Value::Null;
        match (render("subject_template"), render("message_template")) {
            (Ok(subject), Ok(message)) => {
                if let Some(s) = &subject {
                    output.insert("subject".into(), s.clone().into());
                }
                let message = message.unwrap_or_default();
                output.insert("message".into(), message.clone().into());
                if self.live
                    && let Some(dest) = action.get("destination_id").and_then(Value::as_str)
                {
                    match super::super::notifications::send(self.cluster, dest, subject.as_deref().unwrap_or_default(), &message).await {
                        Ok(event) => {
                            output.insert("messageId".into(), event.into());
                        }
                        Err(f) => error = Value::from(str_of(&f.error())),
                    }
                }
            }
            (Err(e), _) | (_, Err(e)) => error = Value::from(str_of(&e)),
        }
        obj! { "id" => id, "name" => name, "output" => Value::Obj(output), "throttled" => false, "executionTime" => self.now, "error" => error }
    }

    /// Runs a trigger's actions for one alert (`alert`'s throttles and acknowledgement apply),
    /// updating its `action_execution_results`: the action results by action id.
    async fn actions_for(&self, trigger: &Map, alert: Option<&mut Map>, model: &Value) -> Map {
        let mut out = Map::new();
        let mut scratch = Map::new();
        let alert = alert.unwrap_or(&mut scratch);
        if alert.get("state").and_then(Value::as_str) == Some("ACKNOWLEDGED") {
            return out;
        }
        let mut execs: Vec<Value> = alert.get("action_execution_results").and_then(Value::as_array).cloned().unwrap_or_default();
        for a in trigger.get("actions").and_then(Value::as_array).into_iter().flatten() {
            let Value::Obj(a) = a else { continue };
            let aid = a.get("id").map(Value::to_es_string).unwrap_or_default();
            let pos = execs.iter().position(|e| e.get("action_id").and_then(Value::as_str) == Some(aid.as_str()));
            let throttle_ms = a.get("throttle").map(|t| {
                let unit = t.get("unit").map(Value::to_es_string).unwrap_or_else(|| "MINUTES".into());
                super::unit_ms(&unit).flatten().unwrap_or(60_000).saturating_mul(t.get("value").and_then(Value::as_i64).unwrap_or(0))
            });
            let last = pos.and_then(|p| execs[p].get("last_execution_time")).and_then(Value::as_i64);
            let throttled = a.get("throttle_enabled").and_then(Value::as_bool) == Some(true) && matches!((last, throttle_ms), (Some(l), Some(t)) if l + t > self.now);
            let result = if throttled {
                if let Some(Value::Obj(e)) = pos.map(|p| &mut execs[p]) {
                    let n = e.get("throttled_count").and_then(Value::as_i64).unwrap_or(0) + 1;
                    e.insert("throttled_count".into(), n.into());
                }
                obj! { "id" => aid.clone(), "name" => a.get("name").cloned().unwrap_or(Value::Null), "output" => obj! {}, "throttled" => true, "executionTime" => Value::Null, "error" => Value::Null }
            } else {
                let r = self.action(a, model).await;
                match pos {
                    Some(p) => {
                        if let Value::Obj(e) = &mut execs[p] {
                            e.insert("last_execution_time".into(), self.now.into());
                        }
                    }
                    None => execs.push(obj! { "action_id" => aid.clone(), "last_execution_time" => self.now, "throttled_count" => 0 }),
                }
                r
            };
            out.insert(aid, result);
        }
        alert.insert("action_execution_results".into(), Value::Arr(execs));
        out
    }

    /// Keeps an alert (a run that keeps nothing doesn't).
    async fn keep(&self, id: Option<&str>, alert: Map) -> EsResult<()> {
        if self.live {
            alerts::save(self.cluster, id, alert).await?;
        }
        Ok(())
    }

    async fn complete(&self, a: &alerts::Stored) -> EsResult<()> {
        if !self.live {
            return Ok(());
        }
        let mut alert = a.alert.clone();
        alert.insert("state".into(), "COMPLETED".into());
        alert.insert("end_time".into(), self.now.into());
        alerts::to_history(self.cluster, &a.id, alert, self.now).await
    }

    fn triggers(&self) -> Vec<(String, Map)> {
        self.monitor
            .get("triggers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|t| t.as_object()?.iter().next().and_then(|(k, v)| Some((k.clone(), v.as_object()?.clone()))))
            .collect()
    }

    /// A trigger's painless condition with `ctx` and extra `params`.
    fn condition(&self, trigger: &Map, model: &Value, params: Option<&Map>) -> Result<bool, String> {
        let script = trigger.get("condition").and_then(|c| c.get("script")).cloned().unwrap_or(Value::Null);
        let bound = crate::xpack::watcher::parse::compile(self.cluster, &script, Context::WatcherCondition).map_err(|e| str_of(&e))?;
        let Some(sc) = bound.painless() else { return Err("only painless trigger conditions run".into()) };
        let mut inst = sc.instance();
        match params {
            Some(extra) => {
                let mut p = crate::scripting::from_serde(&bound.params).as_object().cloned().unwrap_or_default();
                p.extend(extra.clone());
                inst.set_params(&crate::scripting::to_serde(&Value::Obj(p)));
            }
            None => inst.set_params(&bound.params),
        }
        inst.set_var("ctx", search_painless::Value::from_json_map(&crate::scripting::to_serde(model)));
        match inst.execute(&mut search_painless::NoHost) {
            Ok(search_painless::Value::Bool(b)) => Ok(b),
            Ok(other) => Err(format!("trigger condition returned [{}], not a boolean", other.to_java_string())),
            Err(e) => Err(str_of(&crate::scripting::script_error(e))),
        }
    }

    // --- query level ------------------------------------------------------------------------

    async fn query_level(&self) -> EsResult<(Value, Map)> {
        let (results, input_error) = match self.refusal() {
            Some(r) => (Vec::new(), Some(r)),
            None => self.searches().await,
        };
        let current = match &self.id {
            Some(id) if self.live => alerts::active(self.cluster, id).await?,
            _ => Vec::new(),
        };
        let mut out = Map::new();
        for (_, t) in self.triggers() {
            let tid = t.get("id").map(Value::to_es_string).unwrap_or_default();
            let prior = current.iter().find(|a| a.trigger_id() == tid);
            let mut extra = Map::new();
            extra.insert("alert".into(), prior.map(alerts::Stored::template).unwrap_or(Value::Null));
            let model = self.ctx(&t, &results, input_error.as_deref(), extra);
            let (triggered, error) = match &input_error {
                Some(e) => (false, Some(e.clone())),
                None => match self.condition(&t, &model, None) {
                    Ok(b) => (b, None),
                    Err(e) => (false, Some(e)),
                },
            };
            let mut action_results = Map::new();
            if triggered || error.is_some() {
                let state = if error.is_some() { "ERROR" } else { "ACTIVE" };
                let (id, mut alert) = match prior {
                    Some(a) => {
                        let mut alert = a.alert.clone();
                        let keep_ack = a.state() == "ACKNOWLEDGED" && error.is_none();
                        alert.insert("state".into(), if keep_ack { "ACKNOWLEDGED" } else { state }.into());
                        alert.insert("last_notification_time".into(), self.now.into());
                        alert.insert("error_message".into(), error.clone().map(Value::from).unwrap_or(Value::Null));
                        (Some(a.id.clone()), alert)
                    }
                    None => (None, alerts::new_alert(self.new_alert(&t, state, error.clone()))),
                };
                if triggered {
                    action_results = self.actions_for(&t, Some(&mut alert), &model).await;
                }
                self.keep(id.as_deref(), alert).await?;
            } else if let Some(a) = prior {
                self.complete(a).await?;
            }
            out.insert(
                tid,
                obj! { "name" => t.get("name").cloned().unwrap_or(Value::Null), "triggered" => triggered, "action_results" => Value::Obj(action_results), "ppl_query_results" => Value::Arr(vec![]), "error" => error.map(Value::from).unwrap_or(Value::Null) },
            );
        }
        Ok((input_results(results, input_error), out))
    }

    // --- bucket level -----------------------------------------------------------------------

    async fn bucket_level(&self) -> EsResult<(Value, Map)> {
        let (mut results, input_error) = match self.refusal() {
            Some(r) => (Vec::new(), Some(r)),
            None => self.searches().await,
        };
        let current = match &self.id {
            Some(id) if self.live => alerts::active(self.cluster, id).await?,
            _ => Vec::new(),
        };
        let mut out = Map::new();
        let mut selections: Vec<(String, Value, Vec<i64>)> = Vec::new();
        for (_, t) in self.triggers() {
            let tid = t.get("id").map(Value::to_es_string).unwrap_or_default();
            let cond = t.get("condition").cloned().unwrap_or(Value::Null);
            let parent = cond.get("parent_bucket_path").map(Value::to_es_string).unwrap_or_default();
            let paths = cond.get("buckets_path").and_then(Value::as_object).cloned().unwrap_or_default();
            let mut error = input_error.clone();
            let buckets: Vec<Value> = results.first().and_then(|r| bucket_list(r, &parent)).unwrap_or_default();
            let mut selected = Vec::new();
            if error.is_none() {
                let model = self.ctx(&t, &results, None, Map::new());
                for (i, b) in buckets.iter().enumerate() {
                    let Some(vars) = bucket_vars(b, &paths) else { continue };
                    match self.condition(&t, &model, Some(&vars)) {
                        Ok(true) => selected.push(i),
                        Ok(false) => {}
                        Err(e) => {
                            error = Some(e);
                            break;
                        }
                    }
                }
            }
            selections.push((tid.clone(), Value::from(parent.clone()), selected.iter().map(|i| *i as i64).collect()));
            let mut agg_buckets = Map::new();
            let mut action_results = Map::new();
            let mine: Vec<&alerts::Stored> = current.iter().filter(|a| a.trigger_id() == tid).collect();
            let mut seen = Vec::new();
            for i in &selected {
                let b = &buckets[*i];
                let keys = bucket_keys(b);
                let hash = keys.iter().map(Value::to_es_string).collect::<Vec<_>>().join("#");
                let content = obj! { "parent_bucket_path" => parent.clone(), "bucket_keys" => Value::Arr(keys.clone()), "bucket" => b.clone() };
                agg_buckets.insert(hash.clone(), obj! { "agg_alert_content" => content.clone() });
                let prior = mine.iter().find(|a| a.bucket_hash().as_deref() == Some(hash.as_str()));
                seen.push(hash.clone());
                let (id, mut alert, new) = match prior {
                    Some(a) => {
                        let mut alert = a.alert.clone();
                        alert.insert("last_notification_time".into(), self.now.into());
                        alert.insert("agg_alert_content".into(), content.clone());
                        (Some(a.id.clone()), alert, false)
                    }
                    None => {
                        let mut n = self.new_alert(&t, "ACTIVE", None);
                        n.agg = Some(content.clone());
                        (None, alerts::new_alert(n), true)
                    }
                };
                let one = alerts::Stored { id: id.clone().unwrap_or_default(), version: 1, alert: alert.clone() };
                let mut extra = Map::new();
                extra.insert("newAlerts".into(), Value::Arr(if new { vec![bucket_template(&one)] } else { vec![] }));
                extra.insert("dedupedAlerts".into(), Value::Arr(if new { vec![] } else { vec![bucket_template(&one)] }));
                extra.insert("completedAlerts".into(), Value::Arr(vec![]));
                let model = self.ctx(&t, &results, None, extra);
                let r = self.actions_for(&t, Some(&mut alert), &model).await;
                if !r.is_empty() {
                    action_results.insert(hash, Value::Obj(r));
                }
                self.keep(id.as_deref(), alert).await?;
            }
            for a in mine {
                if a.bucket_hash().is_some_and(|h| !seen.contains(&h)) {
                    self.complete(a).await?;
                }
            }
            out.insert(tid, obj! { "name" => t.get("name").cloned().unwrap_or(Value::Null), "agg_result_buckets" => Value::Obj(agg_buckets), "action_results" => Value::Obj(action_results), "error" => error.map(Value::from).unwrap_or(Value::Null) });
        }
        // Each trigger's selection shows in the search response, as its pipeline aggregation.
        if let Some(Value::Obj(r)) = results.first_mut()
            && let Some(Value::Obj(aggs)) = r.get_mut("aggregations")
        {
            for (tid, parent, idx) in selections {
                aggs.insert(tid, obj! { "parent_bucket_path" => parent, "bucket_indices" => Value::Arr(idx.into_iter().map(Value::from).collect()) });
            }
            let hashed = super::super::hash_ordered(Value::Obj(std::mem::take(aggs)));
            if let Value::Obj(h) = hashed {
                *aggs = h;
            }
        }
        Ok((input_results(results, input_error), out))
    }

    // --- document level ---------------------------------------------------------------------

    async fn doc_level(&self) -> EsResult<(Value, Map)> {
        let input = self.monitor.get("inputs").and_then(Value::as_array).and_then(|a| a.first()).and_then(|i| i.get("doc_level_input")).cloned().unwrap_or(Value::Null);
        let queries: Vec<Map> = input.get("queries").and_then(Value::as_array).into_iter().flatten().filter_map(|q| q.as_object().cloned()).collect();
        let job = self.id.as_ref().and_then(|id| self.cluster.state().metadata.xpack.get(JOBS, id).cloned());
        let last = job.as_ref().and_then(|j| j.get("run_context")).cloned().unwrap_or_else(|| current_context(self.cluster, &self.monitor));
        let now_ctx = current_context(self.cluster, &self.monitor);
        // query id -> matching docs ("<id>|<index>"), and each doc's matched queries in order.
        let mut by_query: Vec<(String, Vec<String>)> = Vec::new();
        let mut docs: Vec<(String, Vec<usize>)> = Vec::new();
        let mut error = self.refusal();
        if error.is_none() {
            'outer: for (qi, q) in queries.iter().enumerate() {
                let qid = q.get("id").map(Value::to_es_string).unwrap_or_default();
                let text = q.get("query").map(Value::to_es_string).unwrap_or_default();
                let mut hits = Vec::new();
                for (index, shards) in now_ctx.as_object().into_iter().flatten() {
                    for (shard, max) in shards.as_array().into_iter().flatten().enumerate() {
                        let max = max.as_i64().unwrap_or(-1);
                        let from = last.get(index).and_then(|s| s.as_array()).and_then(|s| s.get(shard)).and_then(Value::as_i64).unwrap_or(-1);
                        if max <= from {
                            continue;
                        }
                        let body = obj! {
                            "size" => 10_000, "_source" => false, "sort" => crate::arr!["_doc"],
                            "query" => obj! { "bool" => obj! { "filter" => crate::arr![
                                obj! { "range" => obj! { "_seq_no" => obj! { "gt" => from, "lte" => max } } },
                                obj! { "query_string" => obj! { "query" => text.clone() } },
                            ] } },
                        };
                        let call = internal::call(self.cluster, "POST", &format!("/{}/_search?preference=_shards:{shard}", enc(index)), Some(&body));
                        match self.as_owner(call).await {
                            Ok(r) => {
                                for h in r.pointer(&["hits", "hits"]).and_then(Value::as_array).into_iter().flatten() {
                                    hits.push(format!("{}|{index}", h.get("_id").map(Value::to_es_string).unwrap_or_default()));
                                }
                            }
                            Err(e) => {
                                error = Some(str_of(&e));
                                break 'outer;
                            }
                        }
                    }
                }
                for d in &hits {
                    match docs.iter_mut().find(|(x, _)| x == d) {
                        Some((_, qs)) => qs.push(qi),
                        None => docs.push((d.clone(), vec![qi])),
                    }
                }
                if !hits.is_empty() {
                    by_query.push((qid, hits));
                }
            }
        }
        let results = vec![Value::Obj(by_query.into_iter().map(|(k, v)| (k, Value::Arr(v.into_iter().map(Value::from).collect()))).collect())];
        let mut out = Map::new();
        for (_, t) in self.triggers() {
            let tid = t.get("id").map(Value::to_es_string).unwrap_or_default();
            let source = t.get("condition").and_then(|c| c.get("script")).and_then(|s| s.get("source")).map(Value::to_es_string).unwrap_or_default();
            let mut terror = error.clone();
            let mut triggered = Vec::new();
            if terror.is_none() {
                match Expr::parse(&source) {
                    Ok(expr) => {
                        for (doc, qs) in &docs {
                            let matched: Vec<&Map> = qs.iter().map(|i| &queries[*i]).collect();
                            if expr.eval(&matched) {
                                triggered.push(doc.clone());
                            }
                        }
                    }
                    Err(e) => terror = Some(e),
                }
            }
            let mut action_results = Map::new();
            for doc in &triggered {
                let mut n = self.new_alert(&t, "ACTIVE", None);
                n.finding_ids = vec![Value::from(random_uuid())];
                n.related_doc_ids = vec![Value::from(doc.clone())];
                let mut alert = alerts::new_alert(n);
                let alert_id = random_uuid();
                let one = alerts::Stored { id: alert_id.clone(), version: 1, alert: alert.clone() };
                let mut extra = Map::new();
                extra.insert("alerts".into(), crate::arr![one.template()]);
                let model = self.ctx(&t, &results, None, extra);
                let r = self.actions_for(&t, Some(&mut alert), &model).await;
                if !r.is_empty() {
                    action_results.insert(alert_id.clone(), Value::Obj(r));
                }
                self.keep(Some(&alert_id), alert).await?;
            }
            out.insert(
                tid,
                obj! { "name" => t.get("name").cloned().unwrap_or(Value::Null), "triggeredDocs" => Value::Arr(triggered.into_iter().map(Value::from).collect()), "action_results" => Value::Obj(action_results), "error" => terror.map(Value::from).unwrap_or(Value::Null) },
            );
        }
        if self.live && error.is_none()
            && let Some(id) = &self.id
        {
            let id = id.clone();
            let now = self.now;
            self.cluster
                .update(move |st| {
                    let Some(Value::Obj(mut rec)) = st.metadata.xpack.get(JOBS, &id).cloned() else { return Ok(()) };
                    rec.insert("run_context".into(), now_ctx);
                    crate::xpack::store::put(st, JOBS, &id, Value::Obj(rec), now).map(|_| ())
                })
                .await?;
        }
        Ok((input_results(results, error), out))
    }
}

fn input_results(results: Vec<Value>, error: Option<String>) -> Value {
    obj! { "results" => Value::Arr(results), "ppl_query_results" => Value::Arr(vec![]), "ppl_num_results" => Value::Null, "error" => error.map(Value::from).unwrap_or(Value::Null) }
}

/// A bucket-level alert as a template argument (`bucket_keys` joined by commas).
fn bucket_template(a: &alerts::Stored) -> Value {
    let mut t = a.template();
    if let Value::Obj(m) = &mut t {
        m.shift_remove("agg_alert_content");
        let keys = m.get("bucket_keys").and_then(Value::as_array).map(|k| k.iter().map(Value::to_es_string).collect::<Vec<_>>().join(","));
        m.insert("bucket_keys".into(), keys.map(Value::from).unwrap_or(Value::Null));
    }
    t
}

/// The buckets of `path` (`a>b` for a sub-aggregation of a single-bucket one) in a response.
fn bucket_list(r: &Value, path: &str) -> Option<Vec<Value>> {
    let mut cur = r.get("aggregations")?;
    for (i, seg) in path.split('>').enumerate() {
        if i > 0 {
            cur = cur.get(seg).or_else(|| cur.get("aggregations").and_then(|a| a.get(seg)))?;
        } else {
            cur = cur.get(seg)?;
        }
    }
    match cur.get("buckets")? {
        Value::Arr(a) => Some(a.clone()),
        Value::Obj(m) => Some(m.iter().map(|(k, v)| {
            let mut b = v.as_object().cloned().unwrap_or_default();
            b.entry("key".to_string()).or_insert(Value::from(k.clone()));
            Value::Obj(b)
        }).collect()),
        _ => None,
    }
}

/// A bucket's keys: a composite's key values in order, else its `key_as_string` or `key`.
fn bucket_keys(b: &Value) -> Vec<Value> {
    match b.get("key") {
        Some(Value::Obj(m)) => m.values().map(|v| Value::from(v.to_es_string())).collect(),
        Some(k) => vec![Value::from(b.get("key_as_string").unwrap_or(k).to_es_string())],
        None => Vec::new(),
    }
}

/// The `buckets_path` values of one bucket (`None`: one is missing, skipped by `gap_policy`).
fn bucket_vars(b: &Value, paths: &Map) -> Option<Map> {
    let mut vars = Map::new();
    for (var, path) in paths {
        let path = path.to_es_string();
        let v = match path.as_str() {
            "_count" => b.get("doc_count").cloned(),
            "_key" => b.get("key").cloned(),
            p => {
                let (agg, metric) = p.split_once('.').map_or((p, "value"), |(a, m)| (a, m));
                match b.get(agg) {
                    Some(Value::Obj(m)) => m.get(metric).cloned().or_else(|| m.get("value").cloned()),
                    other => other.cloned(),
                }
            }
        };
        match v {
            Some(v) if !v.is_null() => {
                vars.insert(var.clone(), v);
            }
            _ => return None,
        }
    }
    Some(vars)
}

/// The current maximum sequence number of every shard of a document-level monitor's indices:
/// `{index: [max_seq_no per shard]}`.
pub(crate) fn current_context(cluster: &Cluster, monitor: &Map) -> Value {
    let st = cluster.state();
    let exprs: Vec<String> = monitor
        .get("inputs")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(|i| i.pointer(&["doc_level_input", "indices"]))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(Value::to_es_string)
        .collect();
    let names = crate::meta::resolve::resolve(&st, &exprs, &crate::meta::resolve::IndicesOptions::LENIENT_EXPAND_OPEN, crate::time::now_millis()).unwrap_or_default();
    let mut out = Map::new();
    for name in names {
        let Some(idx) = st.metadata.indices.get(&name) else { continue };
        let seqs: Vec<Value> = match cluster.engine.index(cluster, &st, idx) {
            Ok(e) => (0..idx.number_of_shards()).map(|s| Value::from(e.shard(s).shard.max_seq_no())).collect(),
            Err(_) => continue,
        };
        out.insert(name, Value::Arr(seqs));
    }
    Value::Obj(out)
}

/// A document-level trigger's expression (`TriggerExpressionParser`).
enum Expr {
    Query(String, String),
    Not(Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
}

impl Expr {
    fn parse(src: &str) -> Result<Expr, String> {
        let toks = tokens(src)?;
        let mut pos = 0;
        let e = Self::or(&toks, &mut pos)?;
        if pos != toks.len() {
            return Err(format!("Invalid trigger expression [{src}]"));
        }
        Ok(e)
    }

    fn or(t: &[String], p: &mut usize) -> Result<Expr, String> {
        let mut l = Self::and(t, p)?;
        while t.get(*p).is_some_and(|x| x == "||") {
            *p += 1;
            l = Expr::Or(Box::new(l), Box::new(Self::and(t, p)?));
        }
        Ok(l)
    }

    fn and(t: &[String], p: &mut usize) -> Result<Expr, String> {
        let mut l = Self::not(t, p)?;
        while t.get(*p).is_some_and(|x| x == "&&") {
            *p += 1;
            l = Expr::And(Box::new(l), Box::new(Self::not(t, p)?));
        }
        Ok(l)
    }

    fn not(t: &[String], p: &mut usize) -> Result<Expr, String> {
        match t.get(*p).map(String::as_str) {
            Some("!") => {
                *p += 1;
                Ok(Expr::Not(Box::new(Self::not(t, p)?)))
            }
            Some("(") => {
                *p += 1;
                let e = Self::or(t, p)?;
                if t.get(*p).map(String::as_str) != Some(")") {
                    return Err("Invalid trigger expression: unbalanced parentheses".into());
                }
                *p += 1;
                Ok(e)
            }
            Some(q) if q.starts_with("query[") && q.ends_with(']') => {
                *p += 1;
                let inner = &q["query[".len()..q.len() - 1];
                let (k, v) = inner.split_once('=').ok_or_else(|| format!("Invalid trigger expression term [{q}]"))?;
                Ok(Expr::Query(k.trim().to_string(), v.trim().trim_matches('"').to_string()))
            }
            other => Err(format!("Invalid trigger expression term [{}]", other.unwrap_or(""))),
        }
    }

    fn eval(&self, matched: &[&Map]) -> bool {
        match self {
            Expr::Query(k, v) => matched.iter().any(|q| match k.as_str() {
                "tag" => q.get("tags").and_then(Value::as_array).is_some_and(|t| t.iter().any(|x| x.to_es_string() == *v)),
                field => q.get(field).map(Value::to_es_string).as_deref() == Some(v.as_str()),
            }),
            Expr::Not(e) => !e.eval(matched),
            Expr::And(a, b) => a.eval(matched) && b.eval(matched),
            Expr::Or(a, b) => a.eval(matched) || b.eval(matched),
        }
    }
}

fn tokens(src: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let b: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            c if c.is_whitespace() => i += 1,
            '(' | ')' | '!' => {
                out.push(b[i].to_string());
                i += 1;
            }
            '&' | '|' if b.get(i + 1) == Some(&b[i]) => {
                out.push(format!("{0}{0}", b[i]));
                i += 2;
            }
            _ => {
                let start = i;
                if src[src.char_indices().nth(i).map_or(src.len(), |(o, _)| o)..].starts_with("query[") {
                    while i < b.len() && b[i] != ']' {
                        i += 1;
                    }
                    i += 1;
                } else {
                    while i < b.len() && !b[i].is_whitespace() && !matches!(b[i], '(' | ')') {
                        i += 1;
                    }
                }
                out.push(b[start..i.min(b.len())].iter().collect());
            }
        }
    }
    Ok(out)
}

/// One run of `monitor`: the `_execute` answer (`MonitorRunResult`).
async fn run_monitor(cluster: &Cluster, id: Option<String>, version: i64, monitor: Map, owner: Option<Value>, live: bool, now: i64) -> EsResult<Value> {
    let schedule = monitor.get("schedule").cloned().unwrap_or(Value::Null);
    let execution_id = format!("{}_{}_{}", id.clone().unwrap_or_default(), chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.9f"), random_uuid());
    let run = Run { cluster, id, version, period_start: now - super::period_ms(&schedule, now), monitor, owner, live, now, execution_id };
    let kind = run.monitor.get("monitor_type").and_then(Value::as_str).unwrap_or("query_level_monitor").to_string();
    let (inputs, triggers) = match kind.as_str() {
        "bucket_level_monitor" => run.bucket_level().await?,
        "doc_level_monitor" => run.doc_level().await?,
        _ => run.query_level().await?,
    };
    Ok(obj! {
        "monitor_name" => run.monitor.get("name").cloned().unwrap_or(Value::Null),
        "period_start" => run.period_start, "period_end" => now, "error" => Value::Null,
        "input_results" => inputs, "trigger_results" => Value::Obj(triggers),
    })
}

/// The user a stored monitor runs as.
fn owner_of(cluster: &Cluster, id: &str) -> Option<Value> {
    cluster.state().metadata.xpack.get(JOBS, id).and_then(|j| j.get("user")).cloned()
}

pub async fn execute(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let dryrun = ctx.bool_param("dryrun", false)?;
    let now = ctx.now_ms;
    let out = match ctx.path_param("monitor_id") {
        Some(id) => {
            let Some(Stored { version, monitor, .. }) = super::read(ctx.cluster, id).await? else {
                return Err(alerting(404, "org.opensearch.OpenSearchStatusException", format!("Can't find monitor with id: {id}")));
            };
            run_monitor(ctx.cluster, Some(id.to_string()), version, monitor, owner_of(ctx.cluster, id), !dryrun, now).await?
        }
        None => {
            let body = ctx.body_map()?.unwrap_or_default();
            let user = internal::stored_user(crate::xpack::security::principal(ctx));
            let monitor = super::parse_monitor(&body, None, user.as_ref(), now)?;
            run_monitor(ctx.cluster, None, 1, monitor, user, false, now).await?
        }
    };
    Reply::ok(out)
}

/// A scheduled run of monitor `id`, due at `due`: its alerts kept, its next run set.
pub(crate) async fn scheduled(cluster: &Cluster, id: &str, due: i64, now: i64) {
    let stored = match super::read(cluster, id).await {
        Ok(Some(s)) => s,
        // The monitor's document is gone: so is its schedule.
        Ok(None) => {
            let _ = super::advance(cluster, id, due, None, now).await;
            return;
        }
        Err(_) => return,
    };
    let next = super::job_next(&stored.monitor, now.max(due));
    if run_monitor(cluster, Some(id.to_string()), stored.version, stored.monitor, owner_of(cluster, id), true, now).await.is_ok() {
        let _ = super::advance(cluster, id, due, next, now).await;
    }
}
