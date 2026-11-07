//! The statistics views generated in one pass: pg_stat_{all,user,sys}_{tables,indexes} and
//! pg_statio_{all,user,sys}_{tables,indexes,sequences}. Each relation's statistics entry is
//! read once, its indexes' from where pg_index's rows come from, and pg_class's relations from
//! the relation maps (`system::class_keys`), of the relations a filter names alone. Running
//! their SQL definitions called 25 functions a row through a grouped join of pg_class and
//! pg_index: 190-540 ms at 10,000 tables against PostgreSQL's 31-49.

use super::system::{ClassKeep, ClassKey, ClassSel, SysTable, class_keys, rows, rows_shared};
use crate::error::Result;
use crate::exec::ExecCtx;
use crate::exec::Row;
use crate::exec::pgstat::{self, TabEntry};
use crate::types::Value;
use crate::exec::fasthash::{HashMap, HashSet};

fn oid_of(v: &Value) -> u32 {
    match v {
        Value::Oid(o) => *o,
        v => v.as_i64().unwrap_or(0) as u32,
    }
}

fn ts(t: Option<i64>) -> Value {
    t.map_or(Value::Null, Value::Timestamptz)
}

/// What a query's filter fixes of a view's rows (`exec/nodes.rs` `is_named_rows`): the
/// rows of other relations aren't made.
#[derive(Default, Clone, Debug)]
pub struct StatSel {
    pub schema: Option<String>,
    pub name: Option<String>,
    pub relid: Option<u32>,
    pub indexrelid: Option<u32>,
    /// The columns the query reads (None: all): the others are NULL, not made (`count(*)`
    /// read every relation's statistics entry and its indexes').
    pub reads: Option<Vec<bool>>,
}

impl StatSel {
    /// Whether any of columns `cols` is read (a column past the mask's end isn't: `exec::need`).
    fn reads_any(&self, cols: std::ops::Range<usize>) -> bool {
        self.reads.as_ref().is_none_or(|r| cols.clone().any(|c| r.get(c).copied().unwrap_or(false)))
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Shape {
    Tables,
    Indexes,
    IoTables,
    IoIndexes,
    IoSequences,
}

/// Which relations' rows: all, those outside pg_catalog, information_schema and pg_toast*
/// (the user views), or those inside (the sys views).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Part {
    All,
    User,
    Sys,
}

fn shape(table: SysTable) -> Option<(Shape, Part)> {
    use SysTable::*;
    Some(match table {
        PgStatAllTables => (Shape::Tables, Part::All),
        PgStatUserTables => (Shape::Tables, Part::User),
        PgStatSysTables => (Shape::Tables, Part::Sys),
        PgStatAllIndexes => (Shape::Indexes, Part::All),
        PgStatUserIndexes => (Shape::Indexes, Part::User),
        PgStatSysIndexes => (Shape::Indexes, Part::Sys),
        PgStatioAllTables => (Shape::IoTables, Part::All),
        PgStatioUserTables => (Shape::IoTables, Part::User),
        PgStatioSysTables => (Shape::IoTables, Part::Sys),
        PgStatioAllIndexes => (Shape::IoIndexes, Part::All),
        PgStatioUserIndexes => (Shape::IoIndexes, Part::User),
        PgStatioSysIndexes => (Shape::IoIndexes, Part::Sys),
        PgStatioAllSequences => (Shape::IoSequences, Part::All),
        PgStatioUserSequences => (Shape::IoSequences, Part::User),
        PgStatioSysSequences => (Shape::IoSequences, Part::Sys),
        _ => return None,
    })
}

/// Whether `table` is one of the views generated here (not by their SQL definitions).
pub fn generated_here(table: SysTable) -> bool {
    shape(table).is_some()
}

/// The view's `schemaname` and `relname` columns.
pub fn name_columns(table: SysTable) -> Option<(usize, usize)> {
    Some(match shape(table)?.0 {
        Shape::Indexes | Shape::IoIndexes => (2, 3),
        _ => (1, 2),
    })
}

/// The view's `relid` column and its `indexrelid` column when it has one.
pub fn oid_columns(table: SysTable) -> Option<(usize, Option<usize>)> {
    Some(match shape(table)?.0 {
        Shape::Indexes | Shape::IoIndexes => (0, Some(1)),
        _ => (0, None),
    })
}

fn part_keeps(part: Part, schema: &Value) -> bool {
    let sys = (schema.as_str())
        .is_some_and(|s| s == "pg_catalog" || s == "information_schema" || s.starts_with("pg_toast"));
    match part {
        Part::All => true,
        Part::User => !sys && !schema.is_null(),
        Part::Sys => sys,
    }
}

/// pg_class's keys of the relations `pick` selects that `keep` keeps (from its kept rows when
/// the relation maps can't give them, `class_keys`).
fn keys(ctx: &ExecCtx, pick: ClassSel, keep: ClassKeep) -> Result<Vec<ClassKey>> {
    if let Some(k) = class_keys(ctx, pick, keep) {
        return Ok(k);
    }
    let class = rows_shared(SysTable::PgClass, ctx)?;
    let kind = |v: &Value| if let Value::Char(k) = v { *k } else { 0 };
    Ok((class.places())
        .map(|(_, ch, i)| ClassKey {
            oid: ch.with_value(i, 0, oid_of),
            schema: ch.with_value(i, 2, oid_of),
            kind: ch.with_value(i, 17, kind),
            name: ch.value(i, 1),
            toast: ch.with_value(i, 13, oid_of),
        })
        .filter(|k| match pick {
            ClassSel::All => true,
            ClassSel::Oid(o) => k.oid == o,
            ClassSel::Named(n) => k.name.as_str() == Some(n),
        })
        .filter(|k| (keep.kinds.is_empty() || keep.kinds.contains(&k.kind)) && keep.schema.is_none_or(|f| f(k.schema)))
        .collect())
}

/// The table an index belongs to, as pg_index's `indrelid` has it.
fn index_owner(ctx: &ExecCtx, ix: u32) -> Option<u32> {
    let cat = ctx.catalog();
    if let Some(i) = cat.indexes.get(&ix) {
        return Some(i.table);
    }
    if let Some((t, true)) = cat.toast_owner(ix) {
        return Some(t.toast);
    }
    (super::static_rows::rows(SysTable::PgIndex).iter())
        .find(|r| super::static_rows::oid_at(r, 0) == ix)
        .map(|r| super::static_rows::oid_at(r, 1))
}

/// The indexes of the tables `of` (each table's, by index oid), from where pg_index's rows
/// come from: the built-in ones, each table's own list (`Table::indexes`, its primary key's),
/// TOAST tables' indexes. (Generating pg_index's rows made the expressions and vectors of
/// every index; iterating the catalog's indexes read every index entry.) `scan`: from every
/// index entry instead (what debug builds check the lists against).
fn indexes_of(ctx: &ExecCtx, of: &HashSet<u32>, scan: bool) -> HashMap<u32, Vec<u32>> {
    let mut out: HashMap<u32, Vec<u32>> = HashMap::default();
    for r in super::static_rows::rows(SysTable::PgIndex).iter() {
        let (ix, rel) = (super::static_rows::oid_at(r, 0), super::static_rows::oid_at(r, 1));
        if of.contains(&rel) {
            out.entry(rel).or_default().push(ix);
        }
    }
    let cat = ctx.catalog();
    if !scan {
        for &rel in of {
            if let Some(t) = cat.index_table(rel) {
                let mut own: Vec<u32> = t.indexes.iter().chain(t.pkey_index.iter()).copied().collect();
                own.sort_unstable();
                own.dedup();
                out.entry(rel).or_default().extend(own);
            }
            if let Some((t, false)) = cat.toast_owner(rel) {
                out.entry(rel).or_default().push(t.toast + 1);
            }
        }
        out.retain(|_, v| !v.is_empty());
    } else {
        for ix in cat.indexes.values().filter(|ix| of.contains(&ix.table)) {
            out.entry(ix.table).or_default().push(ix.oid);
        }
        for t in cat.toasted().filter(|t| of.contains(&t.toast)) {
            out.entry(t.toast).or_default().push(t.toast + 1);
        }
    }
    for v in out.values_mut() {
        v.sort_unstable();
    }
    #[cfg(debug_assertions)]
    if !scan {
        drop(cat);
        let all = indexes_of(ctx, of, true);
        debug_assert!(all == out, "the indexes of {} tables", of.len());
    }
    out
}

/// The view's row count for planning (`sql::explain::sys_tuples`), from the relation maps'
/// sizes and Postgres's own relations, without making its rows: after each DDL statement the
/// parallel-plan check of a query reading a view made every row of it to count them (17 ms at
/// 10,000 tables). An estimate, as a view's is for Postgres's planner; TOAST tables aren't
/// counted.
pub fn row_estimate(table: SysTable, ctx: &ExecCtx) -> Option<f64> {
    let (shape, part) = shape(table)?;
    let kinds: &[u8] = match shape {
        Shape::Tables => b"rtmp",
        Shape::IoSequences => b"S",
        Shape::Indexes | Shape::IoIndexes => b"i",
        Shape::IoTables => b"rtm",
    };
    let builtin = (super::static_rows::rows(SysTable::PgClass).iter())
        .filter(|r| matches!(r[17], Value::Char(k) if kinds.contains(&k)))
        .count();
    let cat = ctx.catalog();
    let user = match shape {
        Shape::IoSequences => cat.sequences.len(),
        Shape::Indexes | Shape::IoIndexes => cat.indexes.len(),
        _ => cat.tables.len() + cat.views.len(),
    };
    let n = match part {
        Part::All => user + builtin,
        Part::User => user,
        Part::Sys => builtin,
    };
    Some(n.max(1) as f64)
}

/// The view's rows, in relation oid order (pg_stat_*_tables' grouping sorts by c.oid; the
/// indexes views by (relid, indexrelid)); of the relations `sel` fixes alone, and of the
/// columns it reads (`StatSel::reads`: the others NULL).
pub fn rows_of(table: SysTable, ctx: &ExecCtx, sel: &StatSel) -> Result<Vec<Row>> {
    let rows = rows_made(table, ctx, sel)?;
    // (The relations' columns, which no statistics change between the two reads.)
    #[cfg(debug_assertions)]
    if sel.reads.is_some() {
        let n = if matches!(shape(table), Some((Shape::Indexes | Shape::IoIndexes, _))) { 5 } else { 3 };
        let all = rows_made(table, ctx, &StatSel { reads: None, ..sel.clone() })?;
        let heads = |rs: &[Row]| rs.iter().map(|r| r[..n].to_vec()).collect::<Vec<_>>();
        debug_assert!(heads(&all) == heads(&rows), "{}'s rows of the columns read", table.name());
    }
    Ok(rows)
}

fn rows_made(table: SysTable, ctx: &ExecCtx, sel: &StatSel) -> Result<Vec<Row>> {
    let (shape, part) = shape(table).expect("a statistics view generated here");
    let indexes = matches!(shape, Shape::Indexes | Shape::IoIndexes);
    // An index named by its oid: its table's rows, of that index.
    let relid = match (sel.relid, sel.indexrelid.filter(|_| indexes)) {
        (r, None) => r,
        (Some(r), Some(ix)) if index_owner(ctx, ix) != Some(r) => return Ok(Vec::new()),
        (_, Some(ix)) => match index_owner(ctx, ix) {
            Some(r) => Some(r),
            None => return Ok(Vec::new()),
        },
    };
    let pick = match (relid, sel.name.as_deref()) {
        (Some(o), _) => ClassSel::Oid(o),
        (None, Some(n)) => ClassSel::Named(n),
        (None, None) => ClassSel::All,
    };
    let narrow = !matches!(pick, ClassSel::All);
    let kinds: &[u8] = match shape {
        Shape::Tables => b"rtmp",
        Shape::IoSequences => b"S",
        _ => b"rtm",
    };
    // Each namespace's name and whether the view shows its relations (decided once: per
    // relation it cloned the name, 20% of a whole view's generation).
    let names: HashMap<u32, (Value, bool)> = (rows(SysTable::PgNamespace, ctx)?.into_iter())
        .map(|r| {
            let s = r.get(1).cloned().unwrap_or(Value::Null);
            let shown = part_keeps(part, &s) && sel.schema.as_deref().is_none_or(|w| s.as_str() == Some(w));
            (oid_of(&r[0]), (s, shown))
        })
        .collect();
    let schema_of = |nsp: u32| names.get(&nsp).map_or(Value::Null, |n| n.0.clone());
    // (A relation of a namespace that doesn't exist: its schemaname is NULL.)
    let null_shown = part_keeps(part, &Value::Null) && sel.schema.is_none();
    let shown = |nsp: u32| names.get(&nsp).map_or(null_shown, |n| n.1);
    let keep = ClassKeep { kinds, schema: Some(&shown) };
    let all = keys(ctx, pick, keep)?;
    let mut rels: Vec<&ClassKey> = (all.iter())
        .filter(|k| kinds.contains(&k.kind) && relid.is_none_or(|o| k.oid == o))
        .filter(|k| sel.name.as_deref().is_none_or(|n| k.name.as_str() == Some(n)))
        .collect();
    rels.sort_by_key(|k| k.oid);
    // (A TOAST table's indexes too, for pg_statio_*_tables' tidx columns.)
    let mut of: HashSet<u32> = rels.iter().map(|k| k.oid).collect();
    if shape == Shape::IoTables {
        of.extend(rels.iter().map(|k| k.toast).filter(|&t| t != 0));
    }
    // Whether any statistics column is read (`count(*)` reads none: no entry is fetched).
    let width = table.columns().len();
    let first = if indexes { 5 } else { 3 };
    let stats = sel.reads_any(first..width);
    let unread = || std::iter::repeat_n(Value::Null, width - first);
    let ixs = match shape {
        Shape::IoSequences => HashMap::default(),
        _ if !indexes && !stats => HashMap::default(),
        _ => indexes_of(ctx, &of, false),
    };
    let fetch = |oid: u32| pgstat::fetch_table(ctx, oid);
    let blocks = |e: &Option<TabEntry>| e.as_ref().map_or((0, 0), |e| (e.blocks_fetched - e.blocks_hit, e.blocks_hit));
    let mut out = Vec::with_capacity(rels.len());
    if indexes {
        // Their indexes' names (an index pg_class doesn't show isn't joined).
        let mut ixnames: HashMap<u32, Value> = HashMap::default();
        let wanted = |ix: u32| sel.indexrelid.is_none_or(|w| w == ix);
        if narrow {
            for ix in rels.iter().flat_map(|k| ixs.get(&k.oid).into_iter().flatten()).copied().filter(|&i| wanted(i)) {
                if let Some(k) = keys(ctx, ClassSel::Oid(ix), ClassKeep::ALL)?.into_iter().next() {
                    ixnames.insert(ix, k.name);
                }
            }
        } else {
            // (An index is in its table's schema: the schemas shown are its table's.)
            let listed: HashSet<u32> = ixs.values().flatten().copied().collect();
            let ix_keep = ClassKeep { kinds: b"i", schema: Some(&shown) };
            for k in keys(ctx, ClassSel::All, ix_keep)?.into_iter().filter(|k| listed.contains(&k.oid)) {
                ixnames.insert(k.oid, k.name);
            }
        }
        for k in &rels {
            let schema = schema_of(k.schema);
            if !part_keeps(part, &schema) {
                continue;
            }
            for &ix in ixs.get(&k.oid).into_iter().flatten().filter(|&&i| wanted(i)) {
                let Some(ixname) = ixnames.get(&ix) else { continue };
                let head = [Value::Oid(k.oid), Value::Oid(ix), schema.clone(), k.name.clone(), ixname.clone()];
                if !stats {
                    out.push(head.into_iter().chain(unread()).collect());
                    continue;
                }
                let e = fetch(ix);
                let c = |f: fn(&TabEntry) -> i64| Value::Int8(e.as_ref().map_or(0, f));
                out.push(match shape {
                    Shape::Indexes => {
                        let last = e.as_ref().map_or(Value::Null, |e| ts(e.lastscan));
                        let tail = [c(|e| e.numscans), last, c(|e| e.tuples_returned), c(|e| e.tuples_fetched)];
                        head.into_iter().chain(tail).collect()
                    }
                    _ => {
                        let (read, hit) = blocks(&e);
                        head.into_iter().chain([Value::Int8(read), Value::Int8(hit)]).collect()
                    }
                });
            }
        }
        return Ok(out);
    }
    // The sums over a relation's indexes (NULL without one: the LATERAL's sum of no rows).
    let idx_blocks = |rel: u32| match ixs.get(&rel) {
        None => (Value::Null, Value::Null),
        Some(ix) => {
            let (r, h) = ix.iter().map(|&i| blocks(&fetch(i))).fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
            (Value::Int8(r), Value::Int8(h))
        }
    };
    for k in rels {
        let schema = schema_of(k.schema);
        if !part_keeps(part, &schema) {
            continue;
        }
        let head = [Value::Oid(k.oid), schema, k.name.clone()];
        if !stats {
            out.push(head.into_iter().chain(unread()).collect());
            continue;
        }
        let e = fetch(k.oid);
        match shape {
            Shape::Tables => out.push(head.into_iter().chain(stat_tables_tail(ctx, &e, ixs.get(&k.oid))).collect()),
            Shape::IoSequences => {
                let (read, hit) = blocks(&e);
                out.push(head.into_iter().chain([Value::Int8(read), Value::Int8(hit)]).collect());
            }
            _ => {
                let (read, hit) = blocks(&e);
                let (ir, ih) = idx_blocks(k.oid);
                let ((tr, th), (xr, xh)) = match k.toast {
                    0 => ((Value::Null, Value::Null), (Value::Null, Value::Null)),
                    t => {
                        let (r, h) = blocks(&fetch(t));
                        ((Value::Int8(r), Value::Int8(h)), idx_blocks(t))
                    }
                };
                out.push(head.into_iter().chain([Value::Int8(read), Value::Int8(hit), ir, ih, tr, th, xr, xh]).collect());
            }
        }
    }
    Ok(out)
}

/// pg_stat_*_tables' columns after relid, schemaname and relname.
fn stat_tables_tail(ctx: &ExecCtx, e: &Option<TabEntry>, ix: Option<&Vec<u32>>) -> Vec<Value> {
    let c = |f: fn(&TabEntry) -> i64| Value::Int8(e.as_ref().map_or(0, f));
    let t = |f: fn(&TabEntry) -> Option<i64>| e.as_ref().map_or(Value::Null, |e| ts(f(e)));
    let fl = |f: fn(&TabEntry) -> f64| Value::Float8(e.as_ref().map_or(0.0, f));
    // The indexes' sums (NULL without an index: the LEFT JOIN's NULL indexrelid).
    let (idx_scan, last_idx_scan, idx_fetch) = match ix {
        None => (Value::Null, Value::Null, Value::Null),
        Some(ix) => {
            let (mut scans, mut fetched, mut last) = (0i64, 0i64, None::<i64>);
            for i in ix {
                if let Some(x) = pgstat::fetch_table(ctx, *i) {
                    scans += x.numscans;
                    fetched += x.tuples_fetched;
                    last = last.max(x.lastscan);
                }
            }
            let own = e.as_ref().map_or(0, |e| e.tuples_fetched);
            (Value::Int8(scans), ts(last), Value::Int8(fetched + own))
        }
    };
    vec![
        c(|e| e.numscans),
        t(|e| e.lastscan),
        c(|e| e.tuples_returned),
        idx_scan,
        last_idx_scan,
        idx_fetch,
        c(|e| e.tuples_inserted),
        c(|e| e.tuples_updated),
        c(|e| e.tuples_deleted),
        c(|e| e.tuples_hot_updated),
        c(|e| e.tuples_newpage_updated),
        c(|e| e.live_tuples),
        c(|e| e.dead_tuples),
        c(|e| e.mod_since_analyze),
        c(|e| e.ins_since_vacuum),
        t(|e| e.last_vacuum_time),
        t(|e| e.last_autovacuum_time),
        t(|e| e.last_analyze_time),
        t(|e| e.last_autoanalyze_time),
        c(|e| e.vacuum_count),
        c(|e| e.autovacuum_count),
        c(|e| e.analyze_count),
        c(|e| e.autoanalyze_count),
        fl(|e| e.total_vacuum_time),
        fl(|e| e.total_autovacuum_time),
        fl(|e| e.total_analyze_time),
        fl(|e| e.total_autoanalyze_time),
    ]
}
