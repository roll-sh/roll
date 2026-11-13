//! ES|QL usage telemetry (`GET _xpack/usage` `esql`): how many queries ran (by client, and
//! how many failed), which commands (`features`) and functions they used, settings they set and
//! how long they took, counted like Elasticsearch's `PlanTelemetry`: each command and function
//! once per query that uses it, a view instead of `subquery` when the union came from a view.
//!
//! Counters live with the loaded cluster (Elasticsearch keeps them per node, in memory) and
//! are written into the static usage document's zeroed `esql` section.

use super::parser::{Cmd, Expr, ExprKind, Query};
use crate::json::{Map, Value};
use parking_lot::Mutex;
use std::collections::{BTreeSet, HashMap};

#[derive(Default)]
pub struct Telemetry {
    inner: Mutex<Counters>,
}

#[derive(Default)]
struct Counters {
    features: HashMap<&'static str, i64>,
    functions: HashMap<String, i64>,
    settings: HashMap<&'static str, i64>,
    /// `[rest, kibana]` × `[total, failed]`.
    queries: [[i64; 2]; 2],
    took: HashMap<&'static str, i64>,
}

/// The commands and functions one query uses.
#[derive(Default)]
pub struct Used {
    features: BTreeSet<&'static str>,
    functions: BTreeSet<String>,
    settings: BTreeSet<&'static str>,
}

impl Used {
    pub fn of(q: &Query, st: &crate::meta::ClusterState) -> Used {
        let mut u = Used::default();
        // A `PROMQL` command counts as Elasticsearch's plan of it: `promql`, `ts`, a filter and
        // an evaluation, and the functions it calls (not its grouping).
        let promql = super::promql::find(&q.src);
        let skip = match promql {
            Some(_) => {
                let n = super::promql::desugared_len(&q.cmds);
                u.features.extend(["promql", "ts", "where", "eval"]);
                for c in &q.cmds[..n] {
                    match c {
                        Cmd::Stats { aggs, .. } => aggs.iter().for_each(|a| u.expr(&a.expr)),
                        Cmd::Eval(l) => l.iter().for_each(|a| u.expr(&a.expr)),
                        _ => {}
                    }
                }
                n
            }
            None => 0,
        };
        u.cmds(&q.cmds[skip..]);
        // A `FROM` naming a view reads it as a subquery, counted as `view`.
        if let Some(Cmd::From { sources, .. }) = q.cmds.first()
            && st.metadata.xpack.iter(super::VIEWS).any(|(v, _)| sources.iter().any(|s| crate::xcontent::filter::glob_match(s, v)))
        {
            u.features.insert("view");
        }
        let ts_stats = matches!(q.cmds.first(), Some(Cmd::Ts { .. })) && q.cmds.iter().any(|c| matches!(c, Cmd::Stats { .. }));
        if !ts_stats && !q.cmds.iter().any(|c| matches!(c, Cmd::Limit(_))) {
            // The implicit limit is a `LIMIT` of the analyzed plan.
            u.features.insert("limit");
        }
        let s = &q.settings;
        // `PROMQL` reads unknown fields as null whatever the query sets.
        let set_unmapped = promql.is_none_or(|(start, _)| q.src[..start].to_ascii_lowercase().contains("unmapped_fields"));
        if s.unmapped != super::parser::Unmapped::Default && set_unmapped {
            u.settings.insert("unmapped_fields");
        }
        if s.time_zone.is_some() {
            u.settings.insert("time_zone");
        }
        if s.column_metadata {
            u.settings.insert("column_metadata");
        }
        u
    }

    fn cmds(&mut self, cmds: &[Cmd]) {
        for c in cmds {
            let f = match c {
                Cmd::From { .. } => "from",
                Cmd::Ts { .. } => "ts",
                Cmd::Row(_) => "row",
                Cmd::Show(_) => "show",
                Cmd::Where(_) => "where",
                Cmd::Eval(_) => "eval",
                Cmd::Stats { .. } => "stats",
                Cmd::Sort(_) => "sort",
                Cmd::Limit(_) => "limit",
                Cmd::Keep(_) => "keep",
                Cmd::Drop(_) => "drop",
                Cmd::Rename(_) => "rename",
                Cmd::MvExpand(..) => "mv_expand",
                Cmd::Dissect { .. } => "dissect",
                Cmd::Grok { .. } => "grok",
                Cmd::Enrich { .. } => "enrich",
                Cmd::InlineStats { .. } => "inline_stats",
                Cmd::Sample(_) => "sample",
                Cmd::LookupJoin { cond: Some(_), .. } => "lookup_join_on_expression",
                Cmd::LookupJoin { .. } => "lookup_join",
                Cmd::Fork { branches, .. } => {
                    for b in branches {
                        self.cmds(b);
                    }
                    "fork"
                }
                Cmd::Union { branches, .. } => {
                    let view = branches.iter().any(|b| b.view.is_some());
                    for b in branches {
                        self.cmds(&b.cmds);
                    }
                    if view { "view" } else { "subquery" }
                }
            };
            self.features.insert(f);
            self.exprs(c);
        }
    }

    fn exprs(&mut self, c: &Cmd) {
        let mut visit = |e: &Expr| self.expr(e);
        match c {
            Cmd::Where(e) => visit(e),
            Cmd::Eval(l) | Cmd::Row(l) => l.iter().for_each(|n| visit(&n.expr)),
            Cmd::Stats { aggs, by } | Cmd::InlineStats { aggs, by } => aggs.iter().chain(by).for_each(|n| visit(&n.expr)),
            Cmd::Sort(keys) => keys.iter().for_each(|k| visit(&k.expr)),
            Cmd::Dissect { field, .. } | Cmd::Grok { field, .. } => visit(field),
            Cmd::LookupJoin { cond: Some(e), .. } => visit(e),
            _ => {}
        }
    }

    fn expr(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Call(name, args) => {
                if !name.starts_with('$') {
                    self.functions.insert(name.to_ascii_lowercase());
                }
                args.iter().for_each(|a| self.expr(a));
            }
            ExprKind::Neg(x) | ExprKind::Not(x) | ExprKind::IsNull(x, _) | ExprKind::Like(x, ..) | ExprKind::Cast(x, _) => self.expr(x),
            ExprKind::Bin(_, a, b) => {
                self.expr(a);
                self.expr(b);
            }
            ExprKind::In(x, list, _) => {
                self.expr(x);
                list.iter().for_each(|a| self.expr(a));
            }
            _ => {}
        }
    }
}

/// Elasticsearch's `took` buckets.
fn took_bucket(ms: i64) -> &'static str {
    match ms {
        ..10 => "lt_10ms",
        10..100 => "lt_100ms",
        100..1000 => "lt_1s",
        1000..10_000 => "lt_10s",
        10_000..60_000 => "lt_1m",
        60_000..600_000 => "lt_10m",
        600_000..3_600_000 => "lt_1h",
        3_600_000..36_000_000 => "lt_10h",
        36_000_000..86_400_000 => "lt_1d",
        _ => "gt_1d",
    }
}

impl Telemetry {
    /// Counts a finished query (`used`: `None` when it failed before it was planned).
    pub fn record(&self, kibana: bool, used: Option<&Used>, failed: bool, took_ms: i64) {
        let mut c = self.inner.lock();
        let client = usize::from(kibana);
        c.queries[client][0] += 1;
        if failed {
            c.queries[client][1] += 1;
        }
        if let Some(u) = used.filter(|_| !failed) {
            for f in &u.features {
                *c.features.entry(f).or_default() += 1;
            }
            for f in &u.functions {
                *c.functions.entry(f.clone()).or_default() += 1;
            }
            for s in &u.settings {
                *c.settings.entry(s).or_default() += 1;
            }
            *c.took.entry(took_bucket(took_ms)).or_default() += 1;
        }
    }

    /// Writes the counters into the usage document's `esql` section.
    pub fn fill(&self, esql: &mut Map) {
        let c = self.inner.lock();
        let bump = |m: Option<&mut Value>, k: &str, n: i64| {
            if let Some(Value::Obj(m)) = m
                && let Some(v) = m.get_mut(k)
            {
                *v = Value::from(v.as_i64().unwrap_or(0) + n);
            }
        };
        for (k, n) in &c.features {
            bump(esql.get_mut("features"), k, *n);
        }
        for (k, n) in &c.functions {
            bump(esql.get_mut("functions"), k, *n);
        }
        for (k, n) in &c.settings {
            bump(esql.get_mut("settings"), k, *n);
        }
        for (k, n) in &c.took {
            bump(esql.get_mut("took"), k, *n);
        }
        if let Some(Value::Obj(q)) = esql.get_mut("queries") {
            for (name, i) in [("rest", Some(0)), ("kibana", Some(1)), ("_all", None)] {
                let (total, failed) = match i {
                    Some(i) => (c.queries[i][0], c.queries[i][1]),
                    None => (c.queries[0][0] + c.queries[1][0], c.queries[0][1] + c.queries[1][1]),
                };
                if let Some(Value::Obj(m)) = q.get_mut(name) {
                    m.insert("total".into(), total.into());
                    m.insert("failed".into(), failed.into());
                }
            }
        }
    }
}
