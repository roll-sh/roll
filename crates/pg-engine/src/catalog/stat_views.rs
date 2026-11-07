//! pg_stat_all_tables, pg_stat_user_tables and pg_stat_sys_tables generated in one pass:
//! each relation's statistics entry read once, its indexes' from pg_index's rows. Running
//! their SQL definitions called 25 functions a row through a grouped join of pg_class and
//! pg_index: 190-540 ms at 10,000 tables against PostgreSQL's 31-49.

use super::system::{SysTable, rows, rows_shared};
use crate::error::Result;
use crate::exec::ExecCtx;
use crate::exec::pgstat::{self, TabEntry};
use crate::exec::Row;
use crate::types::Value;
use std::collections::HashMap;

fn oid_of(v: &Value) -> u32 {
    match v {
        Value::Oid(o) => *o,
        v => v.as_i64().unwrap_or(0) as u32,
    }
}

fn oid_at(r: &[Value], i: usize) -> u32 {
    r.get(i).map_or(0, oid_of)
}

fn ts(t: Option<i64>) -> Value {
    t.map_or(Value::Null, Value::Timestamptz)
}

/// Whether `table` is one of the three (generated here, not by their SQL definitions).
pub fn generated_here(table: SysTable) -> bool {
    matches!(table, SysTable::PgStatAllTables | SysTable::PgStatUserTables | SysTable::PgStatSysTables)
}

/// The view's rows, in relation oid order (the definition's grouping sorts by c.oid): of the
/// relations named `name` in the schema named `schema` alone when given (`WHERE relname =
/// ...`, what a monitoring query of one table reads; PostgreSQL finds it through pg_class's
/// name index), every relation's otherwise.
/// (pg_class's rows are read where they are kept, the columns needed only: making each row
/// whole was most of the time.)
pub fn stat_tables(table: SysTable, ctx: &ExecCtx, schema: Option<&str>, name: Option<&str>) -> Result<Vec<Row>> {
    let class = rows_shared(SysTable::PgClass, ctx)?;
    let names: HashMap<u32, Value> = (rows(SysTable::PgNamespace, ctx)?.into_iter())
        .map(|r| (oid_at(&r, 0), r.get(1).cloned().unwrap_or(Value::Null)))
        .collect();
    let in_schema = |nsp: u32| schema.is_none_or(|s| names.get(&nsp).and_then(|v| v.as_str()) == Some(s));
    let mut rels: Vec<(u32, u32, Value)> = (class.places())
        .filter(|(_, ch, i)| ch.with_value(*i, 17, |v| matches!(v, Value::Char(b'r' | b't' | b'm' | b'p'))))
        .filter(|(_, ch, i)| name.is_none_or(|n| ch.with_value(*i, 1, |v| v.as_str() == Some(n))))
        .map(|(_, ch, i)| (ch.with_value(i, 0, oid_of), ch.with_value(i, 2, oid_of), ch.value(i, 1)))
        .filter(|r| in_schema(r.1))
        .collect();
    rels.sort_by_key(|r| r.0);
    let wanted: Option<std::collections::HashSet<u32>> =
        (schema.is_some() || name.is_some()).then(|| rels.iter().map(|r| r.0).collect());
    let want = |rel: u32| wanted.as_ref().is_none_or(|w| w.contains(&rel));
    // (pg_index's (indexrelid, indrelid) pairs from where its rows come from: the built-in
    // ones, the catalog's indexes, TOAST tables' indexes. Generating its rows made the
    // expressions and vectors of every index.)
    let mut indexes: HashMap<u32, Vec<u32>> = HashMap::new();
    for r in super::static_rows::rows(SysTable::PgIndex).iter() {
        let (ix, rel) = (super::static_rows::oid_at(r, 0), super::static_rows::oid_at(r, 1));
        if want(rel) {
            indexes.entry(rel).or_default().push(ix);
        }
    }
    let cat = ctx.catalog();
    for ix in cat.indexes.values().filter(|ix| want(ix.table)) {
        indexes.entry(ix.table).or_default().push(ix.oid);
    }
    for t in cat.toasted().filter(|t| want(t.toast)) {
        indexes.entry(t.toast).or_default().push(t.toast + 1);
    }
    drop(cat);
    let mut out = Vec::with_capacity(rels.len());
    for (oid, nsp, relname) in rels {
        let schema = names.get(&nsp).cloned().unwrap_or(Value::Null);
        // (pg_stat_user_tables leaves out pg_catalog, information_schema and pg_toast*;
        // pg_stat_sys_tables keeps only those.)
        let sys = match &schema {
            Value::Null => false,
            v => v.as_str().is_some_and(|s| s == "pg_catalog" || s == "information_schema" || s.starts_with("pg_toast")),
        };
        let keep = match table {
            SysTable::PgStatUserTables => !sys && !schema.is_null(),
            SysTable::PgStatSysTables => sys,
            _ => true,
        };
        if !keep {
            continue;
        }
        let e = pgstat::fetch_table(ctx, oid);
        let c = |f: fn(&TabEntry) -> i64| Value::Int8(e.as_ref().map_or(0, f));
        let t = |f: fn(&TabEntry) -> Option<i64>| e.as_ref().map_or(Value::Null, |e| ts(f(e)));
        let fl = |f: fn(&TabEntry) -> f64| Value::Float8(e.as_ref().map_or(0.0, f));
        // The indexes' sums (NULL without an index: the LEFT JOIN's NULL indexrelid).
        let (idx_scan, last_idx_scan, idx_fetch) = match indexes.get(&oid) {
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
        out.push(vec![
            Value::Oid(oid),
            schema,
            relname,
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
        ]);
    }
    Ok(out)
}

/// Whether `table` is one of pg_stat_all_indexes, pg_stat_user_indexes, pg_stat_sys_indexes.
pub fn indexes_here(table: SysTable) -> bool {
    matches!(table, SysTable::PgStatAllIndexes | SysTable::PgStatUserIndexes | SysTable::PgStatSysIndexes)
}

/// pg_stat_*_indexes' rows: each index of a table, TOAST table or materialized view (not a
/// partitioned table's: the definition's relkind list), by (relid, indexrelid); of the tables
/// named `name` in schema `schema` alone when given. (Their definition joined pg_class twice
/// with pg_index, every index's name through the second: 470 ms at 10,000 tables against 20.)
pub fn stat_indexes(table: SysTable, ctx: &ExecCtx, schema: Option<&str>, name: Option<&str>) -> Result<Vec<Row>> {
    let class = rows_shared(SysTable::PgClass, ctx)?;
    let names: HashMap<u32, Value> = (rows(SysTable::PgNamespace, ctx)?.into_iter())
        .map(|r| (oid_at(&r, 0), r.get(1).cloned().unwrap_or(Value::Null)))
        .collect();
    // The tables (and TOAST tables, materialized views) shown, by oid: (namespace, name).
    let mut tabs: HashMap<u32, (u32, Value)> = HashMap::new();
    for (_, ch, i) in class.places() {
        let shown = ch.with_value(i, 17, |v| matches!(v, Value::Char(b'r' | b't' | b'm')))
            && name.is_none_or(|n| ch.with_value(i, 1, |v| v.as_str() == Some(n)));
        if shown {
            tabs.insert(ch.with_value(i, 0, oid_of), (ch.with_value(i, 2, oid_of), ch.value(i, 1)));
        }
    }
    let mut pairs: Vec<(u32, u32)> = (super::static_rows::rows(SysTable::PgIndex).iter())
        .map(|r| (super::static_rows::oid_at(r, 1), super::static_rows::oid_at(r, 0)))
        .filter(|(t, _)| tabs.contains_key(t))
        .collect();
    {
        let cat = ctx.catalog();
        pairs.extend(cat.indexes.values().filter(|ix| tabs.contains_key(&ix.table)).map(|ix| (ix.table, ix.oid)));
        pairs.extend(cat.toasted().filter(|t| tabs.contains_key(&t.toast)).map(|t| (t.toast, t.toast + 1)));
    }
    pairs.sort_unstable();
    // Their indexes' names (an index pg_class doesn't show isn't joined).
    let wanted: std::collections::HashSet<u32> = pairs.iter().map(|p| p.1).collect();
    let mut ixnames: HashMap<u32, Value> = HashMap::new();
    for (_, ch, i) in class.places() {
        let o = ch.with_value(i, 0, oid_of);
        if wanted.contains(&o) {
            ixnames.insert(o, ch.value(i, 1));
        }
    }
    let mut out = Vec::new();
    for (t, ix) in pairs {
        let (Some((nsp, relname)), Some(ixname)) = (tabs.get(&t), ixnames.get(&ix)) else {
            continue;
        };
        let schema_v = names.get(nsp).cloned().unwrap_or(Value::Null);
        if schema.is_some_and(|s| schema_v.as_str() != Some(s)) {
            continue;
        }
        let sys = schema_v
            .as_str()
            .is_some_and(|s| s == "pg_catalog" || s == "information_schema" || s.starts_with("pg_toast"));
        let keep = match table {
            SysTable::PgStatUserIndexes => !sys && !schema_v.is_null(),
            SysTable::PgStatSysIndexes => sys,
            _ => true,
        };
        if !keep {
            continue;
        }
        let e = pgstat::fetch_table(ctx, ix);
        let c = |f: fn(&TabEntry) -> i64| Value::Int8(e.as_ref().map_or(0, f));
        out.push(vec![
            Value::Oid(t),
            Value::Oid(ix),
            schema_v,
            relname.clone(),
            ixname.clone(),
            c(|e| e.numscans),
            e.as_ref().map_or(Value::Null, |e| ts(e.lastscan)),
            c(|e| e.tuples_returned),
            c(|e| e.tuples_fetched),
        ]);
    }
    Ok(out)
}
