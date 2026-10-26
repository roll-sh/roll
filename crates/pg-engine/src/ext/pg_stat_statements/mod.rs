//! pg_stat_statements (`contrib/pg_stat_statements`): planning and execution statistics of
//! the statements a cluster runs, grouped by query id (`jumble`), with normalized texts
//! (`normalize`). Collected only when the library is preloaded (the engine's
//! `shared_preload_libraries`); the statistics are the engine's own measures: times are
//! real, block counts are zero (the engine has no shared buffers), WAL records and bytes
//! are those of the rows a statement wrote.

pub mod jumble;
pub mod normalize;
pub mod store;

use super::native::Native;
use crate::Result;
use crate::error::{Error, code};
use crate::session::state::SessionState;
use crate::sql::func::{FnCtx, SrfIter};
use crate::types::{DataType, Value};
use std::sync::Arc;
pub use store::{Key, Kind, Store, Usage};

/// What a session keeps for pg_stat_statements: the nesting level (statements run by
/// others: functions, EXPLAIN's query), and the prepared statements' query ids and texts.
#[derive(Default)]
pub struct SessionStats {
    pub level: u32,
    pub prepared: std::collections::HashMap<String, (i64, Arc<str>)>,
    /// The rows a utility command processed that its tag doesn't show (REFRESH
    /// MATERIALIZED VIEW's).
    pub utility_rows: Option<u64>,
}

fn not_loaded() -> Error {
    Error::new(
        code::OBJECT_NOT_IN_PREREQUISITE_STATE,
        "pg_stat_statements must be loaded via \"shared_preload_libraries\"",
    )
}

fn setting(s: &SessionState, name: &str) -> Option<String> {
    s.settings.borrow().get_raw(name)
}

/// `pgss_enabled(level)`: track = all, or top and at the top level.
pub fn enabled(s: &SessionState, level: u32) -> bool {
    match setting(s, "pg_stat_statements.track").as_deref() {
        Some("all") => true,
        Some("none") => false,
        _ => level == 0,
    }
}

pub fn track_utility(s: &SessionState) -> bool {
    setting(s, "pg_stat_statements.track_utility").as_deref() != Some("off")
}

pub fn track_planning(s: &SessionState) -> bool {
    setting(s, "pg_stat_statements.track_planning").as_deref() == Some("on")
}

/// A statement as pg_stat_statements sees it: the string it came in (`source`, which the
/// node's locations count in) and the part of it that is the statement.
pub struct Stmt<'a> {
    pub source: &'a str,
    /// Where the statement starts in `source` (-1: unknown, the whole string).
    pub location: i32,
    /// Its length (0: the rest of the string).
    pub len: i32,
}

/// `CleanQuerytext`: the statement's text without surrounding blanks, and where it starts.
pub fn clean(st: &Stmt) -> (String, i32) {
    let (mut loc, mut len) = if st.location >= 0 {
        let rest = st.source.len() as i32 - st.location;
        (st.location, if st.len <= 0 { rest } else { st.len.min(rest) })
    } else {
        (0, st.source.len() as i32)
    };
    let b = st.source.as_bytes();
    let space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c');
    while len > 0 && space(b[loc as usize]) {
        loc += 1;
        len -= 1;
    }
    while len > 0 && space(b[(loc + len - 1) as usize]) {
        len -= 1;
    }
    let text = st.source.get(loc as usize..(loc + len) as usize).unwrap_or("").to_string();
    (text, loc)
}

/// The key of an entry for this session's current user and database at `level`.
pub fn key(s: &SessionState, queryid: i64, level: u32) -> Key {
    let cat = s.db.catalog();
    Key {
        userid: s.current_role(&cat),
        dbid: s.database_oid(),
        queryid,
        toplevel: level == 0,
    }
}

/// Store `kind` for the statement with query id `id`. None: parse analysis's entry of a
/// statement with constants, its text normalized by `j` (an entry first made when the
/// statement is planned or run keeps its text as written, as PostgreSQL's does).
pub fn store(s: &SessionState, st: &Stmt, id: i64, j: &jumble::Jumble, level: u32, kind: Option<Kind>, usage: Usage) {
    let Some(store) = s.cluster.statements() else { return };
    let now = crate::types::datetime::Clock::system().now;
    let k = key(s, id, level);
    store.store(
        k,
        || {
            let (text, loc) = clean(st);
            if kind.is_none() && !j.locs.is_empty() {
                normalize::normalize(&text, loc, j)
            } else {
                text
            }
        },
        kind,
        usage,
        now,
    );
}

/// `jumble` for a session: relations resolve through its catalog and search path; `vars`
/// are a function's parameters and variables.
pub fn jumble_vars(s: &SessionState, node: &pg_query::NodeEnum, vars: &[(String, i32)]) -> (i64, jumble::Jumble) {
    let cat = s.db.catalog();
    let path = s.search_path();
    let columns = |schema: Option<&str>, name: &str| -> Option<Vec<(String, String)>> {
        match cat.lookup(schema, name, &path).ok()?? {
            crate::catalog::Relation::Table(t) => {
                Some(t.live_columns().map(|(_, c)| (c.name.clone(), c.ty.typname())).collect())
            }
            _ => None,
        }
    };
    // CALL's procedure: the user procedure of that name and number of parameters, when one.
    let procs = |schema: Option<&str>, name: &str, n: usize| -> Option<Vec<String>> {
        let ns = match schema {
            Some(sc) => Some(cat.schema_by_name(sc)?.oid),
            None => None,
        };
        let mut found = cat.functions.values().filter(|f| {
            f.procedure && f.name == name && ns.is_none_or(|o| o == f.schema) && f.args.len() + f.modes.iter().filter(|m| **m == 'o').count() == n
        });
        let f = found.next()?;
        if found.next().is_some() {
            return None;
        }
        let (mut ins, mut outs) = (f.args.iter(), f.out_cols.iter());
        let modes = if f.modes.is_empty() { vec!['i'; f.args.len()] } else { f.modes.clone() };
        modes
            .iter()
            .map(|m| match m {
                'o' => outs.next().map(|(_, t)| t.typname()),
                _ => ins.next().map(|(_, t)| t.typname()),
            })
            .collect()
    };
    jumble::jumble(node, &columns, &procs, vars)
}

pub fn jumble_in(s: &SessionState, node: &pg_query::NodeEnum) -> (i64, jumble::Jumble) {
    jumble_vars(s, node, &[])
}

/// A statement a function ran (`text`, one statement), counted one level down from what
/// called it when track = all: its rows and time.
pub fn nested(s: &SessionState, text: &str, vars: &[(String, i32)], rows: u64, took: std::time::Duration) {
    if s.cluster.statements().is_none() {
        return;
    }
    let level = s.pgss.borrow().level;
    if !enabled(s, level) {
        return;
    }
    let Ok(parsed) = pg_query::parse(text, 0) else { return };
    let Some(node) = parsed.protobuf.stmts.first().and_then(|r| r.stmt.as_ref()).and_then(|n| n.node.as_ref()) else {
        return;
    };
    let utility = !jumble::is_query(node);
    let (id, j) = jumble_vars(s, node, vars);
    let st = Stmt { source: text, location: 0, len: 0 };
    if !j.locs.is_empty() {
        store(s, &st, id, &j, level, None, Usage::default());
    }
    if utility && !track_utility(s) {
        return;
    }
    let usage = Usage { millis: took.as_secs_f64() * 1000.0, rows: if utility { 0 } else { rows }, ..Default::default() };
    store(s, &st, id, &j, level, Some(Kind::Exec), usage);
}

/// `pgssVersion`: which API a pg_stat_statements function's columns are (its number of
/// output columns tells 1.0 from 1.1).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Api {
    V1_0,
    V1_1,
    V1_2,
    V1_3,
    V1_8,
    V1_9,
    V1_10,
    V1_11,
    V1_12,
}

/// The rows of `pg_stat_statements_internal`.
fn rows(c: &FnCtx, api: Api, showtext: bool) -> Result<SrfIter> {
    let sess = c.exec.sess;
    let store = sess.cluster.statements().ok_or_else(not_loaded)?;
    let ncols = match c.ret {
        DataType::Row(r) => r.fields.len(),
        _ => 0,
    };
    let expected = match api {
        Api::V1_0 | Api::V1_1 => [14, 18].contains(&ncols),
        Api::V1_2 => ncols == 19,
        Api::V1_3 => ncols == 23,
        Api::V1_8 => ncols == 32,
        Api::V1_9 => ncols == 33,
        Api::V1_10 => ncols == 43,
        Api::V1_11 => ncols == 49,
        Api::V1_12 => ncols == 52,
    };
    if !expected {
        return Err(crate::error::internal("incorrect number of output arguments"));
    }
    let api = if api == Api::V1_0 && ncols == 18 { Api::V1_1 } else { api };
    let cat = c.exec.catalog();
    let me = c.exec.current_user();
    let allowed = cat.has_privs_of_role(me, 3375, c.exec.db_owner());
    let i8 = Value::Int8;
    let f8 = Value::Float8;
    let mut out = Vec::new();
    for (k, e) in store.entries() {
        let t = &e.counters;
        if t.sticky() {
            continue;
        }
        let mut v = vec![Value::Oid(k.userid), Value::Oid(k.dbid)];
        if api >= Api::V1_9 {
            v.push(Value::Bool(k.toplevel));
        }
        if allowed || k.userid == me {
            if api >= Api::V1_2 {
                v.push(i8(k.queryid));
            }
            v.push(if showtext { Value::Text(e.query.clone()) } else { Value::Null });
        } else {
            if api >= Api::V1_2 {
                v.push(Value::Null);
            }
            v.push(if showtext { Value::Text(Arc::from("<insufficient privilege>")) } else { Value::Null });
        }
        for kind in 0..2 {
            if kind == 1 || api >= Api::V1_8 {
                v.push(i8(t.calls[kind]));
                v.push(f8(t.total_time[kind]));
            }
            if (kind == 1 && api >= Api::V1_3) || api >= Api::V1_8 {
                v.push(f8(t.min_time[kind]));
                v.push(f8(t.max_time[kind]));
                v.push(f8(t.mean_time[kind]));
                let sd = if t.calls[kind] > 1 { (t.sum_var_time[kind] / t.calls[kind] as f64).sqrt() } else { 0.0 };
                v.push(f8(sd));
            }
        }
        v.push(i8(t.rows));
        // Blocks: those added to the tables written, dirtied and written (the engine has
        // no buffer pool: nothing is hit or read).
        let (sw, lw) = (t.shared_blks_written, t.local_blks_written);
        let v11 = api >= Api::V1_1;
        let blocks: Vec<i64> = if v11 { vec![0, 0, sw, sw, 0, 0, lw, lw, 0, 0] } else { vec![0, 0, sw, 0, 0, lw, 0, 0] };
        v.extend(blocks.into_iter().map(i8));
        let times = [api >= Api::V1_1, api >= Api::V1_11, api >= Api::V1_10].iter().filter(|x| **x).count() * 2;
        v.extend(std::iter::repeat_n(f8(0.0), times));
        if api >= Api::V1_8 {
            v.push(i8(t.wal_records));
            v.push(i8(t.wal_fpi));
            v.push(Value::Numeric(Arc::new(crate::types::Numeric::from_i64(t.wal_bytes as i64))));
        }
        if api >= Api::V1_12 {
            v.push(i8(0));
        }
        if api >= Api::V1_10 {
            for _ in 0..4 {
                v.push(i8(0));
                v.push(f8(0.0));
            }
        }
        if api >= Api::V1_11 {
            v.push(i8(0));
            v.push(f8(0.0));
        }
        if api >= Api::V1_12 {
            v.push(i8(t.parallel_workers_to_launch));
            v.push(i8(t.parallel_workers_launched));
        }
        if api >= Api::V1_11 {
            v.push(Value::Timestamptz(e.stats_since));
            v.push(Value::Timestamptz(e.minmax_stats_since));
        }
        out.push(Ok(v));
    }
    Ok(Box::new(out.into_iter()))
}

fn showtext(a: &[Value]) -> bool {
    a.first().is_none_or(|v| *v != Value::Bool(false))
}

fn reset(a: &[Value], c: &FnCtx, minmax: bool) -> Result<i64> {
    let store = c.exec.sess.cluster.statements().ok_or_else(not_loaded)?;
    let oid = |i: usize| a.get(i).and_then(Value::as_i64).unwrap_or(0) as u32;
    let queryid = a.get(2).and_then(Value::as_i64).unwrap_or(0);
    let only = minmax && a.get(3) == Some(&Value::Bool(true));
    Ok(store.reset(oid(0), oid(1), queryid, only, crate::types::datetime::Clock::system().now))
}

pub(in crate::ext) fn symbol(sym: &str) -> Option<Native> {
    macro_rules! srf {
        ($api:expr) => {
            Native::set(|a, c| rows(c, $api, showtext(a)))
        };
    }
    Some(match sym {
        "pg_stat_statements" => Native::set(|_, c| rows(c, Api::V1_0, true)),
        "pg_stat_statements_1_2" => srf!(Api::V1_2),
        "pg_stat_statements_1_3" => srf!(Api::V1_3),
        "pg_stat_statements_1_8" => srf!(Api::V1_8),
        "pg_stat_statements_1_9" => srf!(Api::V1_9),
        "pg_stat_statements_1_10" => srf!(Api::V1_10),
        "pg_stat_statements_1_11" => srf!(Api::V1_11),
        "pg_stat_statements_1_12" => srf!(Api::V1_12),
        "pg_stat_statements_reset" => Native::scalar(|a, c| reset(a, c, false).map(|_| Value::Text(Arc::from("")))),
        "pg_stat_statements_reset_1_7" => Native::scalar(|a, c| reset(a, c, false).map(|_| Value::Text(Arc::from("")))),
        "pg_stat_statements_reset_1_11" => Native::scalar(|a, c| reset(a, c, true).map(Value::Timestamptz)),
        "pg_stat_statements_info" => Native::scalar(|_, c| {
            let store = c.exec.sess.cluster.statements().ok_or_else(not_loaded)?;
            let (dealloc, reset) = store.info();
            Ok(Value::Record(Arc::new(vec![Value::Int8(dealloc), Value::Timestamptz(reset)])))
        }),
        _ => return None,
    })
}
