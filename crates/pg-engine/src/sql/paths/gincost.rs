//! gincostestimate (selfuncs.c): a GIN index scan's cost from the metapage's statistics
//! and the entries its conditions search for (each operator class's extractQuery).

use super::{AmCost, CPU_INDEX_TUPLE_COST, IClause, IndexInfo, Kind, PAGE_CPU_MULTIPLIER, Planner, Rel, implied_by, qual_exprs};
use crate::sql::costs::{self, CPU_OPERATOR_COST};
use crate::sql::expr::Expr;
use crate::types::{DataType, Value};

/// GIN_SEARCH_MODE_*: which items a query's entries leave out.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Default,
    IncludeEmpty,
    All,
}

/// What extractQuery gives for one query value: no match possible, or its entries (how
/// many, how many of them partial matches) and search mode.
enum Pattern {
    NoMatch,
    Entries { n: f64, partial: f64, mode: Mode },
}

/// The operator classes whose extractQuery is modelled here.
#[derive(Clone, Copy)]
enum Class {
    Array,
    TsVector,
    Jsonb,
    JsonbPath,
}

/// GinQualCounts.
#[derive(Default)]
struct Counts {
    full: Vec<bool>,
    normal: Vec<bool>,
    partial: f64,
    exact: f64,
    search: f64,
}

/// The operator class of index column `col` (None: one extractQuery isn't modelled for).
fn class_of(ii: &IndexInfo, col: usize) -> Option<Class> {
    let opclass = ii.ix.columns.get(col).map_or(0, |c| c.opclass);
    let name = crate::catalog::system::opclass_name(opclass).map(|(n, _)| n);
    match (ii.keys.get(col)?.ty(), name) {
        (DataType::Array(_), None | Some("array_ops")) => Some(Class::Array),
        (DataType::TsVector, None | Some("tsvector_ops")) => Some(Class::TsVector),
        (DataType::Jsonb, None | Some("jsonb_ops")) => Some(Class::Jsonb),
        (DataType::Jsonb, Some("jsonb_path_ops")) => Some(Class::JsonbPath),
        _ => None,
    }
}

/// gincost_pattern's call of the class's extractQuery for operator `op` and query value
/// `v` (not null). None: an operator or value not modelled.
fn pattern(class: Class, op: &str, v: &Value) -> Option<Pattern> {
    let entries = |n: usize, partial: usize, mode: Mode| {
        Some(if n == 0 && mode == Mode::Default {
            Pattern::NoMatch
        } else {
            Pattern::Entries { n: n as f64, partial: partial as f64, mode }
        })
    };
    match (class, v) {
        // ginqueryarrayextract: every element, NULLs too.
        (Class::Array, Value::Array(a)) => {
            let n = a.items.len();
            let mode = match op {
                "&&" => Mode::Default,
                "@>" if n > 0 => Mode::Default,
                "@>" => Mode::All,
                "<@" => Mode::IncludeEmpty,
                "=" if n > 0 => Mode::Default,
                "=" => Mode::IncludeEmpty,
                _ => return None,
            };
            entries(n, 0, mode)
        }
        // gin_extract_tsquery: the operands, prefix ones partial; a full scan when no
        // positive match is required (`!a`).
        (Class::TsVector, Value::TsQuery(q)) if matches!(op, "@@" | "@@@") => {
            use crate::types::tsearch::QueryItem;
            if q.items.is_empty() {
                return entries(0, 0, Mode::Default);
            }
            let vals = q.items.iter().filter_map(|i| if let QueryItem::Val(o) = i { Some(o) } else { None });
            let (n, partial) = vals.fold((0, 0), |(n, p), o| (n + 1, p + o.prefix as usize));
            let mode = if crate::types::tsearch::exec::requires_match(&q.items, 0) { Mode::Default } else { Mode::All };
            entries(n, partial, mode)
        }
        // gin_extract_jsonb_query.
        (Class::Jsonb, Value::Jsonb(j)) if op == "@>" => {
            let n = jsonb_entries(j);
            entries(n, 0, if n == 0 { Mode::All } else { Mode::Default })
        }
        (Class::Jsonb, Value::Text(_)) if op == "?" => entries(1, 0, Mode::Default),
        (Class::Jsonb, Value::Array(a)) if matches!(op, "?|" | "?&") => {
            let n = a.items.iter().filter(|x| !x.is_null()).count();
            entries(n, 0, if n == 0 && op == "?&" { Mode::All } else { Mode::Default })
        }
        // gin_extract_jsonb_query_path.
        (Class::JsonbPath, Value::Jsonb(j)) if op == "@>" => {
            let n = jsonb_path_entries(j);
            entries(n, 0, if n == 0 { Mode::All } else { Mode::Default })
        }
        // extract_jsp_query: no entries is a full scan.
        (Class::Jsonb | Class::JsonbPath, Value::Text(p)) if matches!(op, "@?" | "@@") => {
            let n = crate::exec::search::jsonpath_entries(matches!(class, Class::JsonbPath), op == "@?", p)?;
            entries(n, 0, if n == 0 { Mode::All } else { Mode::Default })
        }
        _ => None,
    }
}

/// gin_extract_jsonb's entries: every key, array element and value (a raw scalar's one).
fn jsonb_entries(j: &crate::types::json::Jsonb) -> usize {
    use crate::types::json::Jsonb as J;
    fn walk(j: &J) -> usize {
        match j {
            J::Object(pairs) => pairs.iter().map(|(_, v)| 1 + walk(v)).sum(),
            J::Array(items) => items.iter().map(walk).sum(),
            _ => 1,
        }
    }
    match j {
        J::Object(p) if p.is_empty() => 0,
        J::Array(a) if a.is_empty() => 0,
        j => walk(j),
    }
}

/// gin_extract_jsonb_path's entries: one per scalar (a raw scalar's one).
fn jsonb_path_entries(j: &crate::types::json::Jsonb) -> usize {
    use crate::types::json::Jsonb as J;
    match j {
        J::Object(pairs) => pairs.iter().map(|(_, v)| jsonb_path_entries(v)).sum(),
        J::Array(items) => items.iter().map(jsonb_path_entries).sum(),
        _ => 1,
    }
}

/// gincost_opexpr's operand reduced to a constant as estimate_expression_value reduces it
/// (casts of constants and calls of built-in immutable or stable functions of constants
/// evaluated): Some(None) for NULL, None for a value known only when run.
fn operand(e: &Expr, ctx: &crate::exec::ExecCtx) -> Option<Option<Value>> {
    match costs::strip(e) {
        Expr::Const(v, _) if v.is_null() => Some(None),
        Expr::Const(v, _) => Some(Some(v.clone())),
        Expr::Cast(x, from, to, explicit) => match operand(x, ctx)? {
            None => Some(None),
            Some(v) => crate::types::cast::cast_value(v, from, to, ctx.fmt(), *explicit).ok().map(|v| (!v.is_null()).then_some(v)),
        },
        // (An error leaves it to the scan, which raises it.)
        e @ Expr::Call(def, args, _)
            if def.oid > 0
                && def.oid < crate::catalog::FIRST_USER_OID
                && def.volatility != 'v'
                && def.srf.is_none()
                && args.iter().all(|a| operand(a, ctx).is_some()) =>
        {
            crate::sql::expr::eval(e, &[], ctx).ok().map(|v| (!v.is_null()).then_some(v))
        }
        _ => None,
    }
}

impl Planner<'_, '_> {
    /// gincostestimate. An operator class or condition whose extractQuery isn't modelled
    /// keeps genericcostestimate's cost.
    pub(super) fn gincost(&self, rel: &Rel, ii: &IndexInfo, quals: &[IClause], loop_count: f64) -> AmCost {
        let generic = || self.generic(rel, ii, quals, 0.0, 0.0, loop_count, 0).0;
        let ncols = ii.keys.len();
        let mut counts = Counts { full: vec![false; ncols], normal: vec![false; ncols], ..Default::default() };
        let mut possible = true;
        for q in quals {
            let (Kind::Op(op), Some(class)) = (&q.kind, class_of(ii, q.col)) else {
                return generic();
            };
            let Some(v) = &q.value else {
                return generic();
            };
            match operand(v, self.ctx) {
                // (Unknown until run: one ordinary search entry.)
                None => {
                    counts.exact += 1.0;
                    counts.search += 1.0;
                    continue;
                }
                Some(None) => {
                    possible = false;
                    break;
                }
                Some(Some(v)) => match pattern(class, op, &v) {
                    None => return generic(),
                    Some(Pattern::NoMatch) => {
                        possible = false;
                        break;
                    }
                    Some(Pattern::Entries { n, partial, mode }) => {
                        // (A partial match is taken as 100 entries.)
                        counts.partial += 100.0 * partial;
                        counts.exact += n - partial;
                        counts.search += n;
                        match mode {
                            Mode::Default => counts.normal[q.col] = true,
                            Mode::IncludeEmpty => {
                                counts.normal[q.col] = true;
                                counts.exact += 1.0;
                                counts.search += 1.0;
                            }
                            Mode::All => counts.full[q.col] = true,
                        }
                    }
                },
            }
        }
        let zero = AmCost { startup: 0.0, total: 0.0, sel: 0.0, correlation: 0.0, pages: 0.0, disabled: None };
        let stats = ii.gin.unwrap_or_default();
        let mut num_pages = ii.pages;
        let num_tuples = ii.tuples;
        let num_pending = if (stats.pending_pages as f64) < num_pages { stats.pending_pages as f64 } else { 0.0 };
        let total_pages = stats.total_pages as f64;
        let (num_entry_pages, num_data_pages, mut num_entries);
        if num_pages > 0.0 && total_pages <= num_pages && total_pages > num_pages / 4.0 && stats.entry_pages > 0 && stats.entries > 0 {
            let scale = num_pages / total_pages;
            let e = (stats.entry_pages as f64 * scale).ceil().min(num_pages - num_pending);
            num_data_pages = (stats.data_pages as f64 * scale).ceil().min(num_pages - num_pending - e);
            num_entry_pages = e;
            num_entries = (stats.entries as f64 * scale).ceil();
        } else {
            num_pages = num_pages.max(10.0);
            num_entry_pages = ((num_pages - num_pending) * 0.90).floor();
            num_data_pages = num_pages - num_pending - num_entry_pages;
            num_entries = (num_entry_pages * 100.0).floor();
        }
        if num_entries < 1.0 {
            num_entries = 1.0;
        }
        // add_predicate_to_index_quals, then the fraction of the table's rows visited.
        let mut selq: Vec<Expr> = Vec::new();
        let qexprs = qual_exprs(quals);
        for p in &ii.pred {
            if !implied_by(std::slice::from_ref(p), &qexprs) {
                selq.push(p.clone());
            }
        }
        selq.extend(qexprs);
        let sel = self.sel(rel, &selq);
        if !possible {
            return zero;
        }
        let full = (0..ncols).any(|i| counts.full[i] && !counts.normal[i]);
        if full || quals.is_empty() {
            counts.partial = 0.0;
            counts.exact = num_entries;
            counts.search = num_entries;
        }
        // (No ScalarArrayOpExpr reaches a GIN index here: arrayScans stays 1.)
        let outer_scans = loop_count;
        let page_cpu = PAGE_CPU_MULTIPLIER * CPU_OPERATOR_COST;
        let mut entry_fetched = num_pending;
        entry_fetched += (counts.search * num_entry_pages.powf(0.15).round_ties_even()).ceil();
        let partial_scale = (counts.partial / num_entries).min(1.0);
        entry_fetched += (num_entry_pages * partial_scale).ceil();
        let mut data_fetched = (num_data_pages * partial_scale).ceil();
        let mut startup = 0.0;
        let mut total = 0.0;
        if num_entries > 1.0 {
            let descent = (num_entries.ln() / 2f64.ln()).ceil() * CPU_OPERATOR_COST;
            startup += descent * counts.search;
            total += descent * counts.search;
        }
        // (In selfuncs.c's order of operations, as its costs come out.)
        startup += entry_fetched * PAGE_CPU_MULTIPLIER * CPU_OPERATOR_COST;
        total += entry_fetched * PAGE_CPU_MULTIPLIER * CPU_OPERATOR_COST;
        startup += page_cpu * data_fetched;
        if outer_scans > 1.0 {
            entry_fetched = self.pages_fetched(entry_fetched * outer_scans, num_entry_pages.floor(), num_entry_pages, rel.size.pages) / outer_scans;
            data_fetched = self.pages_fetched(data_fetched * outer_scans, num_data_pages.floor(), num_data_pages, rel.size.pages) / outer_scans;
        }
        let rpc = self.g.random_page_cost;
        startup += (entry_fetched + data_fetched) * rpc;
        data_fetched = (num_data_pages * counts.exact / num_entries).ceil();
        let by_sel = (sel * (num_tuples / (8192 / 3) as f64)).ceil();
        if by_sel > data_fetched {
            data_fetched = by_sel;
        }
        startup += page_cpu * counts.search;
        total += data_fetched * PAGE_CPU_MULTIPLIER * CPU_OPERATOR_COST;
        if outer_scans > 1.0 {
            data_fetched = self.pages_fetched(data_fetched * outer_scans, num_data_pages.floor(), num_data_pages, rel.size.pages) / outer_scans;
        }
        total += startup + data_fetched * rpc;
        let arg_cost = self.other_operands_cost(rel, quals);
        startup += arg_cost;
        total += arg_cost;
        total += counts.search * (CPU_OPERATOR_COST * quals.len() as f64);
        total += num_tuples * sel * CPU_INDEX_TUPLE_COST;
        AmCost { startup, total, sel, correlation: 0.0, pages: data_fetched, disabled: None }
    }
}
