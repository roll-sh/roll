//! Cassandra's tombstone warning: a read command that iterated more than
//! `tombstone_warn_threshold` tombstones (1000) warns the client with
//! `Read <live> live rows and <n> tombstone cells for query <CQL>; token <t> (see tombstone_warn_threshold)`.
//! ScyllaDB has no such warning (its `query_tombstone_page_limit` ends pages instead).
//!
//! Ported from Cassandra 5.0:
//!
//! - `ReadCommand.withMetricsRecording` (`MetricRecording`): what is counted (dead cells, rows
//!   whose primary key liveness is deleted and that have no dead cell, range tombstone bounds;
//!   live rows including live static rows), the threshold (`>`), the text (`%1.512s` cuts the
//!   query at 512 UTF-16 units) and the token of the last partition the command opened. Local
//!   system keyspaces (`system`, `system_schema`) don't count (`respectTombstoneThresholds`).
//! - the command as CQL, `AbstractReadQuery.toCQLString`: `ColumnFilter.toCQLString` (`*` for a
//!   wildcard, otherwise the queried non-key columns, statics first, by name, with collection
//!   subselections), `SinglePartitionReadCommand` / `PartitionRangeReadCommand
//!   .appendCQLWhereClause`, `DecoratedKey.toCQLString`, `DataRange.toCQLString`,
//!   `Slices.ArrayBackedSlices.toCQLString` (its approximations included: a multi-column slice
//!   prints one relation per column), `ClusteringIndexNamesFilter.toCQLString`,
//!   `AbstractClusteringIndexFilter.appendOrderByToCQLString`, `RowFilter.toCQLString` and
//!   `DataLimits.toString`, always ending in ` ALLOW FILTERING`.
//!
//! Which commands a SELECT becomes follows the coordinator: one per partition of an `IN` / `=`
//! query (`SinglePartitionReadCommand.Group`; paged, each with the rows left in the page,
//! `MultiPartitionPager`), one per page of a scan (`PartitionRangeQueryPager`: a later page starts
//! at `token(pk) >= token(<last key>)`), and the internal pages of aggregates (`LIMIT <page
//! size>`, resuming after the last row) and GROUP BY (`GROUP LIMIT`, 10000 rows unpaged).
//!
//! Counting needs the exact position where a command stopped: storage reads ahead a chunk, so
//! the read records a [`ReadTrace`] (live rows and tombstones after every row of the current
//! chunk) and a command closes at the mark of the last row it consumed.
//!
//! Known differences: an unpaged `IN` query whose `LIMIT` is reached before the last partition
//! doesn't read (or warn for) the remaining partitions, which Cassandra reads concurrently, so
//! its warnings also come in completion order there; a later GROUP BY page that starts in
//! another partition of an `IN` query, GROUP BY with `PER PARTITION LIMIT` over a scan, and a
//! scan whose static-column filter rejects a partition count approximately; reads through a
//! secondary index don't warn.

use std::fmt::Write as _;
use std::sync::Arc;

use bytes::Bytes;
use nosql_storage::read::{ClusteringFilter, ReadMetrics, ReadTrace};
use nosql_storage::sstable::cursor::KeyBound;
use nosql_storage::{PartitionKey, Slice};
use nosql_types::{CqlType, Dialect};

use super::select::SelectPlan;
use crate::prepare::restrictions::{InValues, Pred};
use crate::prepare::selection::Sel;
use crate::prepare::terms::{Bound, EvalCtx, Term};
use crate::schema::Table;
use crate::storage;

/// `Integer.MAX_VALUE`, `DataLimits.NO_LIMIT`.
const NO_LIMIT: u64 = i32::MAX as u64;

/// One expression of the row filter (`RowFilter.Expression`).
#[derive(Clone, Debug)]
struct Expr {
    column: usize,
    op: &'static str,
    /// The value, for `RowFilter.without` (`None`: never matches a slice bound).
    value: Option<Bytes>,
    text: String,
}

/// How each command's `DataLimits` print.
#[derive(Clone, Debug)]
pub(crate) enum LimitMode {
    /// `CQLLimits` of an unpaged read: the user's limit.
    Fixed {
        rows: Option<u64>,
        per_partition: Option<u64>,
    },
    /// A paged read: the rows the command may return (the page's room), then the partition
    /// limit.
    Paged { per_partition: Option<u64> },
    /// An unpaged aggregate: limits that print as nothing (`SELECT … WHERE …  ALLOW FILTERING`).
    Empty,
    /// GROUP BY (`CQLGroupByLimits`): groups, groups per partition, rows per (sub)page.
    Group {
        groups: Option<u64>,
        per_partition: Option<u64>,
        rows: u64,
    },
}

impl LimitMode {
    /// `None`: `DataLimits.NONE` (nothing is appended).
    fn text(&self, room: u64, groups_done: Option<u64>) -> Option<String> {
        let rows = |rows: Option<u64>, ppl: Option<u64>| -> Option<String> {
            match (rows, ppl) {
                (None, None) => None,
                (Some(r), None) => Some(format!("LIMIT {r}")),
                (None, Some(p)) => Some(format!("PER PARTITION LIMIT {p}")),
                (Some(r), Some(p)) => Some(format!("LIMIT {r} PER PARTITION LIMIT {p}")),
            }
        };
        match self {
            LimitMode::Fixed {
                rows: r,
                per_partition,
            } => rows(*r, *per_partition),
            LimitMode::Paged { per_partition } => rows(Some(room), *per_partition),
            LimitMode::Empty => Some(String::new()),
            LimitMode::Group {
                groups,
                per_partition,
                rows,
            } => {
                let groups = match groups_done {
                    // A later sub-page: the groups left (`CQLGroupByPagingLimits`).
                    Some(done) => Some(groups.unwrap_or(NO_LIMIT).saturating_sub(done)),
                    None => *groups,
                };
                let mut s = String::new();
                if let Some(g) = groups {
                    let _ = write!(s, "GROUP LIMIT {g} ");
                }
                if let Some(p) = per_partition {
                    let _ = write!(s, "GROUP PER PARTITION LIMIT {p} ");
                }
                let _ = write!(s, "LIMIT {rows}");
                Some(s)
            }
        }
    }
}

/// Where a command reads.
#[derive(Clone, Debug)]
pub(crate) enum Place {
    Key(PartitionKey),
    Range { start: KeyBound, end: KeyBound },
}

/// One read command.
#[derive(Clone, Debug)]
pub(crate) struct Command {
    pub place: Place,
    pub clustering: ClusteringFilter,
    pub reversed: bool,
    /// The rows it may return (paged limits print it).
    pub room: u64,
    /// Groups completed before a GROUP BY sub-page (`None` for the first).
    pub groups_done: Option<u64>,
}

/// The parts of the statement every command shares.
pub(crate) struct Query {
    table: Arc<Table>,
    /// `SELECT <columns> FROM <ks>.<table>`.
    head: String,
    filter: Vec<Expr>,
    pub limits: LimitMode,
    threshold: u64,
    /// Whether storage may stop at the page's room (no row filter and nothing the engine
    /// drops): then the read stops exactly where Cassandra's limits do.
    pub storage_limited: bool,
}

impl Query {
    /// The query of `plan`, or `None` when it can't warn (ScyllaDB, generated tables, local
    /// system keyspaces, index reads).
    pub(crate) fn new(
        db_threshold: u64,
        plan: &SelectPlan,
        cx: &EvalCtx<'_>,
        limits: LimitMode,
    ) -> Option<Query> {
        let t = &plan.table;
        if cx.dialect.is_scylla()
            || t.kind != crate::schema::TableKind::Regular
            || matches!(t.keyspace.as_str(), "system" | "system_schema")
            || plan.index.is_some()
        {
            return None;
        }
        let mut head = String::from("SELECT ");
        let cols = columns(plan, cx);
        head.push_str(&cols);
        let _ = write!(head, " FROM {}.{}", ident(&t.keyspace), ident(&t.name));
        let filter = row_filter(plan, cx);
        let storage_limited = filter.is_empty() && t.dropped.is_empty();
        Some(Query {
            table: t.clone(),
            head,
            filter,
            limits,
            threshold: db_threshold,
            storage_limited,
        })
    }

    /// `ReadCommand.toCQLString`.
    fn render(&self, c: &Command) -> String {
        let t = &*self.table;
        let mut sb = self.head.clone();
        match &c.place {
            Place::Key(k) => {
                sb.push_str(" WHERE ");
                let vals = storage::key_components(t, &k.key);
                let parts: Vec<String> = t
                    .partition_key
                    .iter()
                    .zip(vals.iter())
                    .map(|(&ci, v)| {
                        let col = &t.columns[ci];
                        format!("{} = {}", ident(&col.name), literal(&col.ty, v.as_deref()))
                    })
                    .collect();
                sb.push_str(&parts.join(" AND "));
                let f = self.clustering_string(c);
                if !f.is_empty() {
                    if !selects_all(&c.clustering) || !self.filter.is_empty() {
                        sb.push_str(" AND ");
                    }
                    sb.push_str(&f);
                }
            }
            Place::Range { start, end } => {
                let f = self.range_string(c, start, end);
                if !f.is_empty() {
                    sb.push_str(" WHERE ");
                    sb.push_str(&f);
                }
            }
        }
        if let Some(l) = self.limits.text(c.room, c.groups_done) {
            sb.push(' ');
            sb.push_str(&l);
        }
        sb.push_str(" ALLOW FILTERING");
        sb
    }

    /// `DataRange.toCQLString`.
    fn range_string(&self, c: &Command, start: &KeyBound, end: &KeyBound) -> String {
        let unbounded = matches!(start, KeyBound::Unbounded) && matches!(end, KeyBound::Unbounded);
        if unbounded && (selects_all(&c.clustering) || self.table.clustering.is_empty()) {
            return filter_string(&self.filter.iter().collect::<Vec<_>>());
        }
        let t = &*self.table;
        let cols: Vec<String> = t
            .partition_key
            .iter()
            .map(|&ci| ident(&t.columns[ci].name))
            .collect();
        let token = format!("token({})", cols.join(", "));
        let key = |k: &PartitionKey| -> String {
            let vals = storage::key_components(t, &k.key);
            let parts: Vec<String> = t
                .partition_key
                .iter()
                .zip(vals.iter())
                .map(|(&ci, v)| literal(&t.columns[ci].ty, v.as_deref()))
                .collect();
            format!("token({})", parts.join(", "))
        };
        let mut parts = Vec::new();
        match start {
            KeyBound::Unbounded => {}
            KeyBound::Token { token: v, after } => {
                parts.push(format!("{token} {} {v}", if *after { ">" } else { ">=" }))
            }
            KeyBound::Key { key: k, inclusive } => parts.push(format!(
                "{token} {} {}",
                if *inclusive { ">=" } else { ">" },
                key(k)
            )),
        }
        match end {
            KeyBound::Unbounded => {}
            KeyBound::Token { token: v, after } => {
                parts.push(format!("{token} {} {v}", if *after { "<=" } else { "<" }))
            }
            KeyBound::Key { key: k, inclusive } => parts.push(format!(
                "{token} {} {}",
                if *inclusive { "<=" } else { "<" },
                key(k)
            )),
        }
        let mut sb = parts.join(" AND ");
        let f = self.clustering_string(c);
        if !f.is_empty() {
            if !sb.is_empty() {
                sb.push_str(" AND ");
            }
            sb.push_str(&f);
        }
        sb
    }

    /// `ClusteringIndexFilter.toCQLString`.
    fn clustering_string(&self, c: &Command) -> String {
        let t = &*self.table;
        let mut filter: Vec<&Expr> = self.filter.iter().collect();
        let mut sb = String::new();
        match &c.clustering {
            ClusteringFilter::Names(names) => {
                if t.clustering.is_empty() || names.is_empty() {
                    return filter_string(&filter);
                }
                let single_column = t.clustering.len() == 1;
                let names_cols: Vec<String> = t
                    .clustering
                    .iter()
                    .map(|&ci| ident(&t.columns[ci].name))
                    .collect();
                let mut names = names.clone();
                names.sort_by(|a, b| {
                    nosql_storage::compare::compare_clustering(&*t.comparator, a, b)
                });
                names.dedup_by(|a, b| {
                    nosql_storage::compare::compare_clustering(&*t.comparator, a, b).is_eq()
                });
                let joined = names_cols.join(", ");
                if single_column {
                    sb.push_str(&joined);
                } else {
                    let _ = write!(sb, "({joined})");
                }
                sb.push_str(if names.len() == 1 { " = " } else { " IN (" });
                for (i, n) in names.iter().enumerate() {
                    if i > 0 {
                        sb.push_str(", ");
                    }
                    let vals: Vec<String> = t
                        .clustering
                        .iter()
                        .enumerate()
                        .map(|(pos, &ci)| literal(&t.columns[ci].ty, n.get(pos).as_deref()))
                        .collect();
                    if single_column {
                        sb.push_str(&vals.join(", "));
                    } else {
                        let _ = write!(sb, "({})", vals.join(", "));
                    }
                    for (pos, &ci) in t.clustering.iter().enumerate() {
                        without(&mut filter, ci, "=", n.get(pos).as_deref());
                    }
                }
                if names.len() != 1 {
                    sb.push(')');
                }
                if !filter.is_empty() {
                    sb.push_str(" AND ");
                    sb.push_str(&filter_string(&filter));
                }
                self.order_by(c, &mut sb);
                sb
            }
            ClusteringFilter::Slices(slices) => {
                let mut slices = slices.clone();
                slices.sort_by(|a, b| {
                    nosql_storage::compare::compare_bounds(&*t.comparator, &a.start, &b.start)
                });
                if slices.is_empty() {
                    // `SelectNoSlices`.
                } else if slices.len() == 1 && is_all(&slices[0]) {
                    sb.push_str(&filter_string(&filter));
                } else {
                    self.slices_string(&slices, &mut filter, &mut sb);
                }
                self.order_by(c, &mut sb);
                sb
            }
        }
    }

    /// `Slices.ArrayBackedSlices.toCQLString`.
    fn slices_string(&self, slices: &[Slice], filter: &mut Vec<&Expr>, sb: &mut String) {
        let t = &*self.table;
        let mut need_and = false;
        for (i, &ci) in t.clustering.iter().enumerate() {
            let comps: Vec<Component> = slices.iter().filter_map(|s| Component::of(i, s)).collect();
            let Some(first) = comps.first() else { break };
            let col = &t.columns[ci];
            let name = ident(&col.name);
            if first.is_eq() {
                if need_and {
                    sb.push_str(" AND ");
                }
                need_and = true;
                sb.push_str(&name);
                let mut values: Vec<&Option<Bytes>> = Vec::new();
                for c in &comps {
                    if !values.contains(&&c.start) {
                        values.push(&c.start);
                    }
                }
                if values.len() == 1 {
                    let _ = write!(sb, " = {}", literal(&col.ty, first.start.as_deref()));
                    without(filter, ci, "=", first.start.as_deref());
                } else {
                    sb.push_str(" IN (");
                    for (j, v) in values.iter().enumerate() {
                        if j > 0 {
                            sb.push_str(", ");
                        }
                        sb.push_str(&literal(&col.ty, v.as_deref()));
                        without(filter, ci, "=", v.as_deref());
                    }
                    sb.push(')');
                }
            } else {
                if let Some(v) = &first.start {
                    if need_and {
                        sb.push_str(" AND ");
                    }
                    need_and = true;
                    let op = match (col.reversed, first.start_inclusive) {
                        (true, true) => "<=",
                        (true, false) => "<",
                        (false, true) => ">=",
                        (false, false) => ">",
                    };
                    let _ = write!(sb, "{name} {op} {}", literal(&col.ty, Some(v)));
                    without(filter, ci, op, Some(v));
                }
                if let Some(v) = &first.end {
                    if need_and {
                        sb.push_str(" AND ");
                    }
                    need_and = true;
                    let op = match (col.reversed, first.end_inclusive) {
                        (true, true) => ">=",
                        (true, false) => ">",
                        (false, true) => "<=",
                        (false, false) => "<",
                    };
                    let _ = write!(sb, "{name} {op} {}", literal(&col.ty, Some(v)));
                    without(filter, ci, op, Some(v));
                }
            }
        }
        if !filter.is_empty() {
            if need_and {
                sb.push_str(" AND ");
            }
            sb.push_str(&filter_string(filter));
        }
    }

    /// `AbstractClusteringIndexFilter.appendOrderByToCQLString`.
    fn order_by(&self, c: &Command, sb: &mut String) {
        if !c.reversed {
            return;
        }
        let t = &*self.table;
        sb.push_str(" ORDER BY ");
        let cols: Vec<String> = t
            .clustering_columns()
            .map(|col| {
                format!(
                    "{} {}",
                    ident(&col.name),
                    if col.reversed { "ASC" } else { "DESC" }
                )
            })
            .collect();
        sb.push_str(&cols.join(", "));
    }
}

/// `ComponentOfSlice`: one clustering column's part of a slice.
struct Component {
    start_inclusive: bool,
    start: Option<Bytes>,
    end_inclusive: bool,
    end: Option<Bytes>,
}

impl Component {
    fn of(i: usize, s: &Slice) -> Option<Component> {
        let (sl, el) = (s.start.prefix.len(), s.end.prefix.len());
        if i >= sl && i >= el {
            return None;
        }
        let mut c = Component {
            start_inclusive: true,
            start: None,
            end_inclusive: true,
            end: None,
        };
        if i < sl {
            c.start_inclusive = !s.start.after;
            c.start = s.start.prefix.get(i);
        }
        if i < el {
            c.end_inclusive = s.end.after;
            c.end = s.end.prefix.get(i);
        }
        Some(c)
    }

    fn is_eq(&self) -> bool {
        self.start == self.end
    }
}

fn is_all(s: &Slice) -> bool {
    s.start.prefix.is_empty() && s.end.prefix.is_empty()
}

/// `ClusteringIndexFilter.selectsAllPartition` (a names filter never does).
fn selects_all(f: &ClusteringFilter) -> bool {
    matches!(f, ClusteringFilter::Slices(s) if s.len() == 1 && is_all(&s[0]))
}

/// `RowFilter.without`: drops the expression a clustering relation already prints.
fn without(filter: &mut Vec<&Expr>, column: usize, op: &str, value: Option<&[u8]>) {
    filter.retain(|e| {
        !(e.column == column && e.op == op && e.value.as_deref() == value && value.is_some())
    });
}

fn filter_string(filter: &[&Expr]) -> String {
    filter
        .iter()
        .map(|e| e.text.as_str())
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// `ColumnIdentifier.maybeQuote`.
fn ident(name: &str) -> String {
    crate::describe::builder::quote(name, Dialect::Cassandra)
}

/// `AbstractType.toCQLString`.
fn literal(ty: &CqlType, v: Option<&[u8]>) -> String {
    match v {
        None => "null".into(),
        Some(b) => nosql_types::to_cql_literal(ty, b, Dialect::Cassandra).unwrap_or_else(|_| {
            let mut s = String::from("0x");
            for x in b {
                let _ = write!(s, "{x:02x}");
            }
            s
        }),
    }
}

fn eval(t: &Term, cx: &EvalCtx<'_>) -> Option<Bytes> {
    match t.eval(cx) {
        Ok(Bound::Val(b)) => Some(b),
        _ => None,
    }
}

fn eval_list(v: &InValues, cx: &EvalCtx<'_>) -> Vec<Option<Bytes>> {
    match v {
        InValues::Terms(ts) => ts.iter().map(|t| eval(t, cx)).collect(),
        InValues::Marker(t) => match eval(t, cx) {
            Some(b) => nosql_types::collection_elements(&b)
                .map(|es| {
                    es.into_iter()
                        .map(|e| e.map(Bytes::copy_from_slice))
                        .collect()
                })
                .unwrap_or_default(),
            None => Vec::new(),
        },
    }
}

/// The element type `CONTAINS` compares with, and the key type of a map.
fn contained(ty: &CqlType) -> (CqlType, CqlType) {
    match ty {
        CqlType::Set { elem, .. } | CqlType::List { elem, .. } => {
            ((**elem).clone(), (**elem).clone())
        }
        CqlType::Map { key, value, .. } => ((**value).clone(), (**key).clone()),
        other => (other.clone(), other.clone()),
    }
}

/// `StatementRestrictions.getRowFilter`: partition key columns (when filtered), clustering
/// columns (when filtered), then the other columns by name; per column `=` / `IN`, slice bounds
/// (start, end), `CONTAINS`, `CONTAINS KEY`, map entries.
fn row_filter(plan: &SelectPlan, cx: &EvalCtx<'_>) -> Vec<Expr> {
    let t = &*plan.table;
    let r = &plan.restrictions;
    let mut out = Vec::new();
    if r.pk_filtered {
        for (pos, preds) in r.pk.iter().enumerate() {
            column_exprs(t, t.partition_key[pos], preds, cx, &mut out);
        }
    }
    if r.ck_filtered {
        for (pos, preds) in r.ck.iter().enumerate() {
            column_exprs(t, t.clustering[pos], preds, cx, &mut out);
        }
    }
    let mut others: Vec<&(usize, Vec<Pred>)> = r.others.iter().collect();
    others.sort_by(|a, b| {
        t.columns[a.0]
            .name
            .as_bytes()
            .cmp(t.columns[b.0].name.as_bytes())
    });
    for (ci, preds) in others {
        column_exprs(t, *ci, preds, cx, &mut out);
    }
    out
}

fn column_exprs(t: &Table, ci: usize, preds: &[Pred], cx: &EvalCtx<'_>, out: &mut Vec<Expr>) {
    let col = &t.columns[ci];
    let name = ident(&col.name);
    let rank = |p: &Pred| match p {
        Pred::Eq(_) | Pred::In(_) | Pred::NotIn(_) | Pred::Like(_) => 0,
        Pred::Slice { lower: true, .. } => 1,
        Pred::Slice { lower: false, .. } => 2,
        Pred::Contains(_) => 3,
        Pred::ContainsKey(_) => 4,
        Pred::Entry(..) => 5,
    };
    let mut preds: Vec<&Pred> = preds.iter().collect();
    preds.sort_by_key(|p| rank(p));
    let (elem, key_ty) = contained(&col.ty);
    for p in preds {
        let e = match p {
            Pred::Eq(v) => {
                let v = eval(v, cx);
                Expr {
                    column: ci,
                    op: "=",
                    text: format!("{name} = {}", literal(&col.ty, v.as_deref())),
                    value: v,
                }
            }
            Pred::In(vs) | Pred::NotIn(vs) => {
                let op = if matches!(p, Pred::In(_)) {
                    "IN"
                } else {
                    "NOT IN"
                };
                let vals: Vec<String> = eval_list(vs, cx)
                    .iter()
                    .map(|v| literal(&col.ty, v.as_deref()))
                    .collect();
                Expr {
                    column: ci,
                    op,
                    text: format!("{name} {op} [{}]", vals.join(", ")),
                    value: None,
                }
            }
            Pred::Slice {
                lower,
                inclusive,
                value,
            } => {
                let op = match (lower, inclusive) {
                    (true, true) => ">=",
                    (true, false) => ">",
                    (false, true) => "<=",
                    (false, false) => "<",
                };
                let v = eval(value, cx);
                Expr {
                    column: ci,
                    op,
                    text: format!("{name} {op} {}", literal(&col.ty, v.as_deref())),
                    value: v,
                }
            }
            Pred::Contains(v) => {
                let v = eval(v, cx);
                Expr {
                    column: ci,
                    op: "CONTAINS",
                    text: format!("{name} CONTAINS {}", literal(&elem, v.as_deref())),
                    value: None,
                }
            }
            Pred::ContainsKey(v) => {
                let v = eval(v, cx);
                Expr {
                    column: ci,
                    op: "CONTAINS KEY",
                    text: format!("{name} CONTAINS KEY {}", literal(&key_ty, v.as_deref())),
                    value: None,
                }
            }
            Pred::Entry(k, v) => {
                let (k, v) = (eval(k, cx), eval(v, cx));
                Expr {
                    column: ci,
                    op: "[]=",
                    text: format!(
                        "{name}[{}] = {}",
                        literal(&key_ty, k.as_deref()),
                        literal(&elem, v.as_deref())
                    ),
                    value: None,
                }
            }
            Pred::Like(v) => {
                let v = eval(v, cx);
                Expr {
                    column: ci,
                    op: "LIKE",
                    text: format!("{name} LIKE {}", literal(&col.ty, v.as_deref())),
                    value: None,
                }
            }
        };
        out.push(e);
    }
}

/// A queried column and its subselections (`None`: the whole column).
type Queried = Vec<(usize, Option<Vec<String>>)>;

/// `ColumnFilter.toCQLString`.
fn columns(plan: &SelectPlan, cx: &EvalCtx<'_>) -> String {
    let t = &*plan.table;
    let sel = &plan.selection;
    let wildcard = sel.wildcard;
    // `Selection.wildcardWithGroupByOrMaskedColumns` lists the columns.
    if wildcard && plan.group_by.is_none() && t.masks.is_empty() {
        return "*".into();
    }
    let mut q: Queried = Vec::new();
    let mut add = |q: &mut Queried, ci: usize, sub: Option<String>| {
        if t.columns[ci].is_primary_key() {
            return;
        }
        match q.iter_mut().find(|(c, _)| *c == ci) {
            Some((_, subs)) => match (subs.as_mut(), sub) {
                (Some(list), Some(s)) => {
                    if !list.contains(&s) {
                        list.push(s);
                    }
                }
                (_, None) => *subs = None,
                (None, Some(_)) => {}
            },
            None => q.push((ci, sub.map(|s| vec![s]))),
        }
    };
    if wildcard {
        for ci in 0..t.columns.len() {
            add(&mut q, ci, None);
        }
    } else {
        for s in &sel.sels {
            sel_columns(t, s, cx, &mut q, &mut add);
        }
    }
    for (ci, _) in &plan.restrictions.others {
        add(&mut q, *ci, None);
    }
    if q.is_empty() {
        return "*".into();
    }
    q.sort_by(|a, b| {
        let (x, y) = (&t.columns[a.0], &t.columns[b.0]);
        (!x.is_static())
            .cmp(&!y.is_static())
            .then_with(|| x.name.as_bytes().cmp(y.name.as_bytes()))
    });
    let mut parts = Vec::new();
    for (ci, subs) in q {
        let name = ident(&t.columns[ci].name);
        match subs {
            None => parts.push(name),
            Some(list) => parts.extend(list.into_iter().map(|s| format!("{name}{s}"))),
        }
    }
    parts.join(", ")
}

/// The columns a selector reads (`Selector.addFetchedColumns`): element and slice selections of
/// a non-frozen collection column fetch only that part.
fn sel_columns(
    t: &Table,
    s: &Sel,
    cx: &EvalCtx<'_>,
    q: &mut Queried,
    add: &mut dyn FnMut(&mut Queried, usize, Option<String>),
) {
    let multi_cell = |ci: usize| {
        matches!(
            t.columns[ci].ty,
            CqlType::Map { frozen: false, .. } | CqlType::Set { frozen: false, .. }
        )
    };
    match s {
        Sel::Column(c)
        | Sel::WriteTime { col: c, .. }
        | Sel::Ttl(c)
        | Sel::ElemMeta { col: c, .. } => add(q, *c, None),
        Sel::Element { arg, key, .. } if matches!(**arg, Sel::Column(c) if multi_cell(c)) => {
            let Sel::Column(c) = **arg else { return };
            let (_, key_ty) = contained(&t.columns[c].ty);
            let k = eval(key, cx);
            add(q, c, Some(format!("[{}]", literal(&key_ty, k.as_deref()))));
        }
        Sel::Slice { arg, from, to, .. } if matches!(**arg, Sel::Column(c) if multi_cell(c)) => {
            let Sel::Column(c) = **arg else { return };
            let (_, key_ty) = contained(&t.columns[c].ty);
            let bound = |b: &Option<Term>| {
                b.as_ref()
                    .and_then(|t| eval(t, cx))
                    .map(|v| literal(&key_ty, Some(&v)))
                    .unwrap_or_default()
            };
            add(q, c, Some(format!("[{}:{}]", bound(from), bound(to))));
        }
        Sel::Call { args, .. } | Sel::Agg { args, .. } => {
            for a in args {
                sel_columns(t, a, cx, q, add);
            }
        }
        Sel::Field { arg, .. } | Sel::Element { arg, .. } | Sel::Slice { arg, .. } => {
            sel_columns(t, arg, cx, q, add)
        }
        Sel::Term(_) => {}
    }
}

/// The commands of one execution and the warnings they raised.
pub(crate) struct Tracker {
    pub query: Query,
    open: Option<Open>,
    /// A command stopped the statement (GROUP BY's group limit): no more commands start.
    halted: bool,
    /// Counts at a row taken earlier ([`Tracker::remember`]), with its read.
    remembered: Option<(Arc<ReadMetrics>, (u64, u64, u64))>,
    pub warnings: Vec<String>,
}

struct Open {
    metrics: Arc<ReadMetrics>,
    command: Command,
    /// Counts when it started (live rows, live statics, tombstones).
    base: (u64, u64, u64),
}

impl Tracker {
    pub(crate) fn new(query: Query) -> Self {
        Tracker {
            query,
            open: None,
            halted: false,
            remembered: None,
            warnings: Vec::new(),
        }
    }

    /// A command starts reading `metrics`' read (before it is polled).
    pub(crate) fn start(&mut self, metrics: &Arc<ReadMetrics>, command: Command) {
        self.end();
        if self.halted {
            return;
        }
        metrics.trace_rows();
        self.open = Some(Open {
            metrics: metrics.clone(),
            command,
            base: (0, 0, 0),
        });
    }

    /// The open command's read ended (or was dropped): it counted everything it read.
    pub(crate) fn end(&mut self) {
        let Some(o) = self.open.take() else { return };
        let trace = o.metrics.trace.lock().clone();
        let counts = (
            o.metrics.live_rows(),
            trace.live_statics,
            o.metrics.tombstones(),
        );
        self.close(o, counts, &trace);
    }

    /// The open command stopped after row `row` of the chunk it last returned.
    pub(crate) fn stop_at(&mut self, row: usize) {
        let Some(o) = self.open.take() else { return };
        let trace = o.metrics.trace.lock().clone();
        match trace.marks.get(row).copied() {
            Some(m) => self.close(o, m, &trace),
            None => {
                self.open = Some(o);
                self.end();
            }
        }
    }

    /// GROUP BY's group limit: the statement reads nothing more after row `row`.
    pub(crate) fn halt_at(&mut self, row: usize) {
        self.stop_at(row);
        self.halted = true;
    }

    /// An internal page ends after row `row`; the next one continues the same read.
    pub(crate) fn split_at(&mut self, row: usize, next: Command) {
        let Some(o) = self.open.take() else { return };
        let trace = o.metrics.trace.lock().clone();
        let Some(m) = trace.marks.get(row).copied() else {
            self.open = Some(o);
            return;
        };
        let metrics = o.metrics.clone();
        self.close(o, m, &trace);
        self.open = Some(Open {
            metrics,
            command: next,
            base: m,
        });
    }

    /// The open command stopped before the partition its read opened last (a group or page
    /// completed with the partition before it).
    pub(crate) fn stop_at_partition_start(&mut self) {
        let Some(o) = self.open.take() else { return };
        let mut trace = o.metrics.trace.lock().clone();
        let counts = trace.opened;
        trace.last_key = trace.previous_key.take();
        self.close(o, counts, &trace);
    }

    /// The open command skipped the rest of the partition it opened last once its static row
    /// was read (the row filter's static columns don't match: `RowFilter.CQLFilter`).
    pub(crate) fn stop_after_static(&mut self) {
        let Some(o) = self.open.take() else { return };
        let trace = o.metrics.trace.lock().clone();
        self.close(o, trace.after_static, &trace);
    }

    /// [`Self::stop_at_partition_start`], and nothing more is read.
    pub(crate) fn halt_at_partition_start(&mut self) {
        self.stop_at_partition_start();
        self.halted = true;
    }

    /// Keeps the counts after row `row` of the last chunk, where [`Self::restart`] begins.
    pub(crate) fn remember(&mut self, row: usize) {
        let Some(o) = &self.open else { return };
        let m = o.metrics.trace.lock().marks.get(row).copied();
        if let Some(m) = m {
            self.remembered = Some((o.metrics.clone(), m));
        }
    }

    /// A later page of a computed result (GROUP BY): what was read before it belonged to
    /// earlier pages, and its command starts at the remembered row (or at the start of the
    /// open read when that row was in another one).
    pub(crate) fn restart(&mut self, command: Command) {
        self.warnings.clear();
        let Some(o) = self.open.take() else { return };
        let base = match &self.remembered {
            Some((m, counts)) if Arc::ptr_eq(m, &o.metrics) => *counts,
            _ => (0, 0, 0),
        };
        self.open = Some(Open {
            metrics: o.metrics,
            command,
            base,
        });
    }

    /// The row remembered last was read by the open command.
    pub(crate) fn remembered_here(&self) -> bool {
        matches!((&self.remembered, &self.open), (Some((m, _)), Some(o)) if Arc::ptr_eq(m, &o.metrics))
    }

    /// The open command (internal paging continues from it).
    pub(crate) fn command(&self) -> Option<&Command> {
        self.open.as_ref().map(|o| &o.command)
    }

    /// Closes every command; the warnings in order.
    pub(crate) fn finish(&mut self) -> Vec<String> {
        self.end();
        std::mem::take(&mut self.warnings)
    }

    fn close(&mut self, o: Open, (live, statics, tombstones): (u64, u64, u64), trace: &ReadTrace) {
        let tombstones = tombstones.saturating_sub(o.base.2);
        if tombstones <= self.query.threshold {
            return;
        }
        let mut live = live.saturating_sub(o.base.0);
        // Static rows are always fetched, and a live one counts.
        live += statics.saturating_sub(o.base.1);
        let token = match (&trace.last_key, &o.command.place) {
            (Some(k), _) | (None, Place::Key(k)) => k.token.to_string(),
            (None, Place::Range { .. }) => return,
        };
        let cql = truncate_utf16(&self.query.render(&o.command), 512);
        self.warnings.push(format!(
            "Read {live} live rows and {tombstones} tombstone cells for query {cql}; token {token} (see tombstone_warn_threshold)"
        ));
    }
}

/// Java's `%.<n>s`: at most `n` UTF-16 code units.
fn truncate_utf16(s: &str, n: usize) -> String {
    let mut units = 0;
    for (i, ch) in s.char_indices() {
        units += ch.len_utf16();
        if units > n {
            return s[..i].to_owned();
        }
    }
    s.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_print_like_data_limits() {
        let fixed = |rows, per_partition| {
            LimitMode::Fixed {
                rows,
                per_partition,
            }
            .text(0, None)
        };
        assert_eq!(fixed(None, None), None);
        assert_eq!(fixed(Some(3), None).as_deref(), Some("LIMIT 3"));
        assert_eq!(
            fixed(None, Some(1)).as_deref(),
            Some("PER PARTITION LIMIT 1")
        );
        assert_eq!(
            fixed(Some(100), Some(1)).as_deref(),
            Some("LIMIT 100 PER PARTITION LIMIT 1")
        );
        assert_eq!(
            LimitMode::Paged {
                per_partition: None
            }
            .text(95, None)
            .as_deref(),
            Some("LIMIT 95")
        );
        assert_eq!(LimitMode::Empty.text(5, None).as_deref(), Some(""));
        let g = LimitMode::Group {
            groups: None,
            per_partition: None,
            rows: 10000,
        };
        assert_eq!(g.text(0, None).as_deref(), Some("LIMIT 10000"));
        assert_eq!(
            g.text(0, Some(9999)).as_deref(),
            Some("GROUP LIMIT 2147473648 LIMIT 10000")
        );
        let g = LimitMode::Group {
            groups: Some(3),
            per_partition: Some(2),
            rows: 10000,
        };
        assert_eq!(
            g.text(0, None).as_deref(),
            Some("GROUP LIMIT 3 GROUP PER PARTITION LIMIT 2 LIMIT 10000")
        );
    }

    #[test]
    fn queries_are_cut_at_512_utf16_units() {
        assert_eq!(truncate_utf16("abc", 2), "ab");
        assert_eq!(truncate_utf16("abc", 3), "abc");
        assert_eq!(truncate_utf16("a\u{1F600}b", 2), "a");
        assert_eq!(truncate_utf16("a\u{1F600}b", 3), "a\u{1F600}");
    }
}
