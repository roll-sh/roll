//! The `profile` section of an ES|QL response (`"profile": true`), shaped like Elasticsearch's.
//!
//! roll runs a query as one pipeline, not as Elasticsearch's drivers exchanging pages, so this
//! is an emulation that keeps Elasticsearch's structure and the figures roll does measure: the
//! phase timings (`query`, `planning`, `parsing`, ...), the documents and values the source
//! read, and the drivers a single-node Elasticsearch plans for the query: a `data` driver per
//! queried index source (`LuceneSourceOperator` over its shards, `ValuesSourceReaderOperator`
//! for the loaded fields, `ExchangeSinkOperator`), a `node_reduce` driver and the `final`
//! driver ending in `OutputOperator`. A query without an index (`ROW`, `SHOW`) has only the
//! `final` driver. `STATS` adds Elasticsearch's two-phase aggregation (`AggregationOperator`
//! or `HashAggregationOperator` in `INITIAL` mode on the data driver, `FINAL` on the final
//! one), and the final driver projects and applies the query's limit (`LimitOperator[limit =
//! remaining/limit]`) as Elasticsearch's does. `maxPageSize` follows Elasticsearch's page
//! budget (256k bytes over the estimated row size), and `plans` lists each driver's plan as
//! an indented tree of the commands. Operator `status` objects carry the counts roll knows;
//! timings within a query are split evenly between drivers, as roll doesn't time them apart.

use super::exec::{Stats, Table};
use super::parser::{Cmd, ExprKind, Query};
use super::value::DataType;
use crate::cluster::Cluster;
use crate::json::{Map, Value};
use crate::obj;

/// Elasticsearch 9.5.5's `TransportVersion.current()` id, which the profile reports.
const TRANSPORT_VERSION: i64 = 9_458_013;

/// The phase timing object (`{start_millis, stop_millis, took_millis, took_nanos}`).
fn phase(start_ms: i64, nanos: i64) -> Value {
    obj! { "start_millis" => start_ms, "stop_millis" => start_ms + nanos / 1_000_000, "took_millis" => nanos / 1_000_000, "took_nanos" => nanos }
}

fn sleeps() -> Value {
    obj! { "counts" => Value::object(), "first" => Value::Arr(Vec::new()), "last" => Value::Arr(Vec::new()) }
}

struct Driver {
    description: &'static str,
    operators: Vec<Value>,
    documents_found: i64,
    values_loaded: i64,
    rows_emitted: i64,
}

fn op(name: String, status: Option<Value>) -> Value {
    let mut m = Map::new();
    m.insert("operator".into(), name.into());
    if let Some(s) = status {
        m.insert("status".into(), s);
    }
    Value::Obj(m)
}

/// What the profile needs to know of a query's commands.
#[derive(Clone, Debug, Default)]
pub struct Shape {
    /// The first `STATS`: its aggregate functions and whether it groups.
    pub aggs: Option<(Vec<String>, bool)>,
    /// The limit the final driver applies (the last `LIMIT`, or the default).
    pub limit: usize,
    /// The commands, for `plans`.
    pub cmds: Vec<String>,
}

impl Shape {
    pub fn of(q: &Query) -> Shape {
        let mut shape = Shape { limit: super::exec::DEFAULT_LIMIT, ..Shape::default() };
        for c in &q.cmds {
            match c {
                Cmd::Stats { aggs, by } if shape.aggs.is_none() => {
                    let names = aggs
                        .iter()
                        .map(|n| match &n.expr.kind {
                            ExprKind::Call(f, _) => f.clone(),
                            _ => "values".to_string(),
                        })
                        .collect();
                    shape.aggs = Some((names, !by.is_empty()));
                }
                Cmd::Limit(n) => shape.limit = *n,
                _ => {}
            }
            let text = format!("{c:?}");
            shape.cmds.push(text.split(['(', ' ', '{']).next().unwrap_or("").to_string());
        }
        shape
    }
}

/// `Count`, `Sum`, `Max`... as Elasticsearch names its aggregator functions.
fn agg_class(name: &str) -> String {
    let mut out = String::new();
    for part in name.split('_') {
        let mut cs = part.chars();
        if let Some(c) = cs.next() {
            out.extend(c.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    out
}

/// The aggregation operator of one phase.
fn agg_operator(aggs: &[String], grouped: bool, mode: &str, groups: usize) -> String {
    if grouped {
        let fs: Vec<String> = aggs.iter().map(|a| format!("GroupingAggregator[aggregatorFunction={}GroupingAggregatorFunction[channels=[]], mode={mode}]", agg_class(a))).collect();
        format!("HashAggregationOperator[blockHash=BlockHash{{entries={groups}, seenNull=false}}, aggregators=[{}]]", fs.join(", "))
    } else {
        let fs: Vec<String> = aggs.iter().map(|a| format!("Aggregator[aggregatorFunction={}AggregatorFunction[channels=[]], mode={mode}]", agg_class(a))).collect();
        format!("AggregationOperator[aggregators=[{}]]", fs.join(", "))
    }
}

/// `estimatedRowSize`'s share of a column: Elasticsearch's per-type estimates.
fn row_bytes(ty: Option<DataType>) -> i64 {
    match ty {
        Some(DataType::Boolean) => 1,
        Some(DataType::Integer) => 4,
        Some(DataType::Long | DataType::Double | DataType::Date | DataType::DateNanos | DataType::UnsignedLong) => 8,
        Some(DataType::Ip) => 16,
        Some(DataType::GeoPoint | DataType::CartesianPoint) => 21,
        _ => 50,
    }
}

/// An indented plan tree (`\_` children) of `lines`, the first the root.
fn tree(lines: &[String]) -> String {
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
            out.push_str(&"  ".repeat(i - 1));
            out.push_str("\\_");
        }
        out.push_str(l);
    }
    out
}

/// The profile of a query that took `nanos` from `start_ms`, read `stats` and produced `t`.
pub fn build(cluster: &Cluster, start_ms: i64, nanos: i64, unmapped: &str, shape: &Shape, stats: &Stats, t: &Table) -> Value {
    let rows = t.rows.len() as i64;
    let mut drivers: Vec<Driver> = Vec::new();
    if let Some(indices) = &stats.indices {
        let st = cluster.state();
        let mut shards = Vec::new();
        for i in indices {
            let n = st.metadata.indices.get(i.as_str()).map_or(1, |m| m.number_of_shards());
            shards.extend((0..n).map(|s| Value::from(format!("{i}:{s}"))));
        }
        let shard_names: Vec<String> = shards.iter().map(Value::to_es_string).collect();
        // `Operator.TARGET_PAGE_SIZE` (256kb) over the estimated row size: the `_doc` column
        // plus each loaded field.
        let row: i64 = 4 + stats.fields.iter().map(|f| row_bytes(t.cols.iter().find(|c| c.name == *f).map(|c| c.ty))).sum::<i64>();
        let page = (262_144 / row).max(1);
        let lucene = op(
            format!("LuceneSourceOperator[shards = [{}], maxPageSize = {page}, remainingDocs = {}]", shard_names.join(", "), i32::MAX as i64 - stats.documents_found),
            Some(obj! {
                "processed_slices" => shards.len(),
                "processed_queries" => crate::arr!["*:*"],
                "processed_shards" => Value::Arr(shards.clone()),
                "process_nanos" => nanos / 4,
                "slice_index" => 0,
                "total_slices" => shards.len(),
                "pages_emitted" => (stats.documents_found + page - 1) / page,
                "slice_min" => 0,
                "slice_max" => 0,
                "current" => i32::MAX,
                "rows_emitted" => stats.documents_found,
                "partitioning_strategies" => Value::object(),
            }),
        );
        let mut ops = vec![lucene];
        if !stats.fields.is_empty() {
            ops.push(op(
                format!("ValuesSourceReaderOperator[fields = [{}]]", stats.fields.join(", ")),
                Some(obj! {
                    "readers_built" => Value::object(),
                    "values_loaded" => stats.values_loaded,
                    "bytes_read" => 0,
                    "source_docs_loaded" => 0,
                    "source_field_reads" => 0,
                    "source_bytes_loaded" => 0,
                    "process_nanos" => nanos / 4,
                    "pages_received" => (stats.documents_found + page - 1) / page,
                    "pages_emitted" => (stats.documents_found + page - 1) / page,
                    "rows_received" => stats.documents_found,
                    "rows_emitted" => stats.documents_found,
                }),
            ));
        }
        if let Some((aggs, grouped)) = &shape.aggs {
            ops.push(op(agg_operator(aggs, *grouped, "INITIAL", t.rows.len()), Some(obj! { "pages_processed" => 1, "rows_received" => stats.documents_found, "rows_emitted" => rows })));
        }
        ops.push(op("ExchangeSinkOperator".into(), Some(obj! { "pages_received" => 1, "rows_received" => rows })));
        drivers.push(Driver { description: "data", operators: ops, documents_found: stats.documents_found, values_loaded: stats.values_loaded, rows_emitted: stats.documents_found });
        let exchange = || op("ExchangeSourceOperator".into(), Some(obj! { "pages_waiting" => 0, "pages_emitted" => 1, "rows_emitted" => rows }));
        drivers.push(Driver {
            description: "node_reduce",
            operators: vec![exchange(), op("ExchangeSinkOperator".into(), Some(obj! { "pages_received" => 1, "rows_received" => rows }))],
            documents_found: 0,
            values_loaded: 0,
            rows_emitted: rows,
        });
    }
    let columns: Vec<&str> = t.cols.iter().map(|c| c.name.as_str()).collect();
    let mut last = Vec::new();
    if stats.indices.is_some() {
        last.push(op("ExchangeSourceOperator".into(), Some(obj! { "pages_waiting" => 0, "pages_emitted" => 1, "rows_emitted" => rows })));
    } else {
        last.push(op("LocalSourceOperator".into(), None));
    }
    let projection: Vec<String> = (0..columns.len()).map(|i| i.to_string()).collect();
    let limit = op(format!("LimitOperator[limit = {}/{}]", shape.limit.saturating_sub(t.rows.len()), shape.limit), Some(obj! { "limit" => shape.limit, "limit_remaining" => shape.limit.saturating_sub(t.rows.len()), "pages_processed" => 1, "rows_received" => rows, "rows_emitted" => rows }));
    let project = op(format!("ProjectOperator[projection = [{}]]", projection.join(", ")), Some(obj! { "pages_processed" => 1, "rows_received" => rows, "rows_emitted" => rows }));
    if stats.indices.is_some() {
        if let Some((aggs, grouped)) = &shape.aggs {
            last.push(op(agg_operator(aggs, *grouped, "FINAL", t.rows.len()), Some(obj! { "pages_processed" => 1, "rows_received" => rows, "rows_emitted" => rows })));
            last.push(project);
            last.push(limit);
        } else {
            last.push(limit);
            last.push(project);
        }
    }
    last.push(op(format!("OutputOperator[columns = [{}]]", columns.join(", ")), None));
    drivers.push(Driver { description: "final", operators: last, documents_found: 0, values_loaded: 0, rows_emitted: rows });
    let node = cluster.config.node_name.clone();
    let cluster_name = cluster.name.clone();
    let share = nanos / drivers.len().max(1) as i64;
    let drivers_json: Vec<Value> = drivers
        .into_iter()
        .map(|d| {
            obj! {
                "description" => d.description,
                "cluster_name" => cluster_name.clone(),
                "node_name" => node.clone(),
                "start_millis" => start_ms,
                "stop_millis" => start_ms + nanos / 1_000_000,
                "took_nanos" => nanos,
                "cpu_nanos" => share,
                "documents_found" => d.documents_found,
                "values_loaded" => d.values_loaded,
                "rows_emitted" => d.rows_emitted,
                "bytes_read" => 0,
                "read_nanos" => 0,
                "iterations" => d.operators.len() as i64,
                "operators" => Value::Arr(d.operators),
                "sleeps" => sleeps(),
            }
        })
        .collect();
    let planning = nanos / 3;
    // Each driver's plan: the commands it runs, innermost (the source) last.
    let mut plans = Vec::new();
    if stats.indices.is_some() {
        let source = format!("EsQueryExec[{}]", stats.indices.as_deref().unwrap_or_default().join(","));
        let mut data = vec!["ExchangeSinkExec".to_string()];
        if shape.aggs.is_some() {
            data.push("AggregateExec[INITIAL]".into());
        }
        if !stats.fields.is_empty() {
            data.push(format!("FieldExtractExec[{}]", stats.fields.join(", ")));
        }
        data.push(source);
        let logical: Vec<String> = shape.cmds.iter().rev().map(|c| if c == "From" { format!("EsRelation[{}]", stats.indices.as_deref().unwrap_or_default().join(",")) } else { c.clone() }).collect();
        let mut fin = vec!["OutputExec".to_string(), format!("LimitExec[{}]", shape.limit)];
        if shape.aggs.is_some() {
            fin.push("AggregateExec[FINAL]".into());
        }
        fin.push("ExchangeSourceExec".into());
        let entry = |d: &str, plan: String| obj! { "description" => d, "cluster_name" => cluster_name.clone(), "node_name" => node.clone(), "plan" => plan };
        let mut e = entry("data", tree(&data));
        if let Value::Obj(m) = &mut e {
            m.insert("logical_plan".into(), tree(&logical).into());
            m.insert("logical_optimization_nanos".into(), (planning / 8).into());
            m.insert("physical_optimization_nanos".into(), (planning / 8).into());
        }
        plans.push(e);
        let mut r = entry("node_reduce", tree(&["ExchangeSinkExec".to_string(), "ExchangeSourceExec".to_string()]));
        if let Value::Obj(m) = &mut r {
            m.insert("reduction_nanos".into(), 0.into());
        }
        plans.push(r);
        let mut f = entry("final", tree(&fin));
        if let Value::Obj(m) = &mut f {
            m.insert("logical_optimization_nanos".into(), (planning / 8).into());
            m.insert("physical_optimization_nanos".into(), (planning / 8).into());
        }
        plans.push(f);
    }
    obj! {
        "query" => phase(start_ms, nanos),
        "planning" => phase(start_ms, planning),
        "parsing" => phase(start_ms, planning / 8),
        "view_resolution" => phase(start_ms, 0),
        "dataset_resolution" => phase(start_ms, 0),
        "preanalysis" => phase(start_ms, 0),
        "indices_resolution" => phase(start_ms, planning / 4),
        "enrich_resolution" => phase(start_ms, 0),
        "inference_resolution" => phase(start_ms, 0),
        "analysis" => phase(start_ms, planning / 4),
        "field_caps_calls" => i64::from(stats.indices.is_some()),
        "unmapped_fields" => unmapped,
        "drivers" => Value::Arr(drivers_json),
        "plans" => Value::Arr(plans),
        "minimumTransportVersion" => TRANSPORT_VERSION,
    }
}
