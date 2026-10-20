//! What the statistics and monitoring views are built from: pg_stat_get_activity, the
//! cumulative statistics counters (nothing is counted, so they read as a fresh server's),
//! server files and shared memory (none to show), wait events and time zone abbreviations;
//! and information_schema's helper functions.

use super::{FnCtx, FnImpl, Registry, SrfIter, text, text_arg};
use crate::catalog::system::builtin_proc_sigs;
use crate::error::Result;
use crate::types::{DataType, Value};
use std::sync::Arc;

/// information_schema's functions: callable only qualified, or with it on the search path.
pub const INFORMATION_SCHEMA_FUNCS: &[&str] = &[
    "_pg_expandarray",
    "_pg_index_position",
    "_pg_truetypid",
    "_pg_truetypmod",
    "_pg_char_max_length",
    "_pg_char_octet_length",
    "_pg_numeric_precision",
    "_pg_numeric_precision_radix",
    "_pg_numeric_scale",
    "_pg_datetime_precision",
    "_pg_interval_type",
];

/// pg_read_all_stats.
const READ_ALL_STATS: u32 = 3375;

/// The server's auxiliary processes as pg_stat_activity lists them (after the client
/// backends): pid, backend_type, wait event, and whether it runs as the bootstrap superuser.
pub(crate) const AUX_PROCESSES: [(i32, &str, &str, bool); 8] = [
    (34, "autovacuum launcher", "AutovacuumMain", false),
    (35, "logical replication launcher", "LogicalLauncherMain", true),
    (27, "io worker", "IoWorkerMain", false),
    (28, "io worker", "IoWorkerMain", false),
    (29, "io worker", "IoWorkerMain", false),
    (30, "checkpointer", "CheckpointerMain", false),
    (31, "background writer", "BgwriterMain", false),
    (33, "walwriter", "WalWriterMain", false),
];

/// Is `pid` one of the auxiliary processes?
pub(crate) fn is_aux_process(pid: i64) -> bool {
    AUX_PROCESSES.iter().any(|a| a.0 as i64 == pid)
}

/// Is `pid` one of the launchers, which (unlike the other auxiliary processes) are in the
/// process array, so pg_signal_backend finds them (as a superuser's)?
pub(crate) fn is_launcher_process(pid: i64) -> bool {
    AUX_PROCESSES
        .iter()
        .any(|a| a.0 as i64 == pid && a.1.ends_with("launcher"))
}

fn ty(o: u32) -> DataType {
    DataType::from_oid(o).unwrap_or(DataType::Unknown)
}

/// A counter's value before anything was counted: 0, or NULL for what isn't a number
/// (times of last events, reset times, names).
fn zero(t: &DataType, c: &FnCtx) -> Value {
    match t {
        DataType::Int2
        | DataType::Int4
        | DataType::Int8
        | DataType::Float4
        | DataType::Float8
        | DataType::Numeric(_) => {
            crate::types::text::parse_value("0", t, c.exec.fmt()).unwrap_or(Value::Null)
        }
        _ => Value::Null,
    }
}

fn counter(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(zero(c.ret, c))
}

/// A statistic of something that has no entry (functions are only counted when tracked).
fn no_stat(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Null)
}

fn no_rows(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    Ok(Box::new(std::iter::empty()))
}

fn rows(v: Vec<Vec<Value>>) -> Result<SrfIter> {
    Ok(Box::new(v.into_iter().map(Ok)))
}

/// One row of zeros of function `oid`'s OUT columns, with column `col` the reset time
/// of `target`.
fn reset_row(oid: u32, col: usize, target: &str, c: &FnCtx) -> Vec<Value> {
    let sig = builtin_proc_sigs()
        .iter()
        .find(|s| s.oid == oid)
        .expect("built-in function");
    let mut row: Vec<Value> = sig.out.iter().map(|(_, t)| zero(&ty(*t), c)).collect();
    row[col] = crate::exec::pgstat::reset_time(c.exec, target)
        .map_or(Value::Null, Value::Timestamptz);
    row
}

fn archiver(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    rows(vec![reset_row(3195, 6, "archiver", c)])
}
fn wal(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let mut row = reset_row(1136, 4, "wal", c);
    let (records, bytes) = crate::exec::pgstat::with_shared(c.exec, |s| (s.wal_records, s.wal_bytes));
    row[0] = Value::Int8(records);
    row[2] = crate::types::text::parse_value(&bytes.to_string(), &DataType::Numeric(-1), c.exec.fmt())
        .unwrap_or(Value::Null);
    rows(vec![row])
}
fn recovery_prefetch(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    rows(vec![reset_row(6248, 0, "recovery_prefetch", c)])
}
/// pg_stat_get_replication_slot(name): no slot has statistics here, so the named slot's
/// are all zeros (as for a slot whose stats were never reported).
fn replication_slot(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let sig = builtin_proc_sigs()
        .iter()
        .find(|s| s.oid == 6169)
        .expect("built-in function");
    let mut row: Vec<Value> = sig.out.iter().map(|(_, t)| zero(&ty(*t), c)).collect();
    let name: String = text_arg(&a[0]).chars().take(63).collect();
    row[0] = text(name);
    Ok(Box::new(std::iter::once(Ok(row))))
}
/// pg_stat_get_backend_wal(pid): this backend's WAL (no row for any other).
fn backend_wal(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    if a.first().and_then(|v| v.as_i64()) != Some(c.exec.sess.pid as i64) {
        return rows(Vec::new());
    }
    let sig = builtin_proc_sigs()
        .iter()
        .find(|s| s.oid == 6313)
        .expect("built-in function");
    let mut row: Vec<Value> = sig.out.iter().map(|(_, t)| zero(&ty(*t), c)).collect();
    let (records, bytes) = c.exec.sess.pgstat.borrow().wal;
    row[0] = Value::Int8(records);
    row[2] = crate::types::text::parse_value(&bytes.to_string(), &DataType::Numeric(-1), c.exec.fmt())
        .unwrap_or(Value::Null);
    row[4] = Value::Null;
    rows(vec![row])
}
/// No WAL receiver: one row of NULLs (the view keeps rows with a pid).
fn wal_receiver(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(vec![vec![Value::Null; 15]])
}

fn slru(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let z = zero(&DataType::Int8, c);
    rows(
        super::admin::SLRUS
            .iter()
            .map(|n| {
                let reset = crate::exec::pgstat::reset_time(c.exec, &format!("slru:{n}"))
                    .map_or(Value::Null, Value::Timestamptz);
                [vec![text(*n)], vec![z.clone(); 7], vec![reset]].concat()
            })
            .collect(),
    )
}

/// pg_stat_io: a row per backend type, object and context Postgres tracks, with 0 for what
/// it counts there and NULL for what it doesn't.
fn io(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let counts = crate::exec::pgstat::with_shared(c.exec, |s| s.io.clone());
    let reset = crate::exec::pgstat::reset_time(c.exec, "io");
    rows(io_rows(&counts, reset, None, c))
}

/// pg_stat_io's rows over these counts (of one backend's, with its pid first).
fn io_rows(
    counts: &std::collections::HashMap<crate::exec::pgstat::IoKey, i64>,
    reset: Option<i64>,
    pid: Option<i32>,
    c: &FnCtx,
) -> Vec<Vec<Value>> {
    let types = [
        20, 1700, 701, 20, 1700, 701, 20, 701, 20, 1700, 701, 20, 20, 20, 20, 701,
    ];
    // The operation each count column is, and the bytes columns after them.
    let ops: [Option<&str>; 16] = [
        Some("reads"),
        None,
        None,
        Some("writes"),
        None,
        None,
        Some("writebacks"),
        None,
        Some("extends"),
        None,
        None,
        Some("hits"),
        Some("evictions"),
        Some("reuses"),
        Some("fsyncs"),
        None,
    ];
    include_str!("data/stat_io.tsv")
        .lines()
        .filter(|l| pid.is_none() || l.starts_with("client backend\t"))
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let mut row = vec![text(f[0]), text(f[1]), text(f[2])];
            let count = |op: &str| {
                counts
                    .iter()
                    .filter(|((b, o, x, k), _)| b == f[0] && o == f[1] && x == f[2] && *k == op)
                    .map(|(_, n)| *n)
                    .sum::<i64>()
            };
            for (i, (t, has)) in types.iter().zip(&f[3..]).enumerate() {
                if *has != "t" {
                    row.push(Value::Null);
                    continue;
                }
                let n = match ops[i] {
                    Some(op) => count(op),
                    // read_bytes, write_bytes, extend_bytes: the blocks', 8kB each.
                    None if matches!(i, 1 | 4 | 9) => count(ops[i - 1].unwrap()) * 8192,
                    None => 0,
                };
                row.push(if n == 0 {
                    zero(&ty(*t), c)
                } else {
                    crate::types::text::parse_value(&n.to_string(), &ty(*t), c.exec.fmt())
                        .unwrap_or(Value::Null)
                });
            }
            row.push(reset.map_or(Value::Null, Value::Timestamptz));
            row
        })
        .collect()
}

/// pg_stat_get_backend_io(pid): this backend's I/O (no rows for any other).
fn backend_io(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let pid = a.first().and_then(|v| v.as_i64());
    if pid != Some(c.exec.sess.pid as i64) {
        return rows(Vec::new());
    }
    let counts = c.exec.sess.pgstat.borrow().io.clone();
    rows(io_rows(&counts, None, Some(c.exec.sess.pid), c))
}

/// pg_stat_get_activity(pid): the backends (all for NULL), with what only their own role,
/// its members and pg_read_all_stats may see hidden from others.
fn activity(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    rows(activity_rows(a.first().and_then(|v| v.as_i64()), c))
}

/// pg_stat_get_activity's rows (of backend `want`, or all).
fn activity_rows(want: Option<i64>, c: &FnCtx) -> Vec<Vec<Value>> {
    let cat = c.exec.catalog();
    let me = c.exec.current_user();
    let dbo = c.exec.db_owner();
    let dbs = c.exec.sess.cluster.databases();
    let all_stats = cat.has_privs_of_role(me, READ_ALL_STATS, dbo);
    let mut out = Vec::new();
    for b in c.exec.sess.cluster.activity() {
        if want.is_some_and(|p| p != b.pid as i64) {
            continue;
        }
        let role = cat.role_by_name(&b.user).map(|r| r.oid);
        let mut row = vec![Value::Null; 31];
        row[0] = dbs
            .iter()
            .find(|d| d.1 == b.database)
            .map_or(Value::Null, |d| Value::Oid(d.0));
        row[1] = Value::Int4(b.pid);
        row[2] = role.map_or(Value::Null, Value::Oid);
        row[3] = text(b.application_name.as_str());
        if all_stats || role.is_some_and(|r| cat.has_privs_of_role(me, r, dbo)) {
            row[4] = text(b.state.as_str());
            row[5] = text(b.query.as_str());
            row[8] = b.xact_start.map_or(Value::Null, Value::Timestamptz);
            row[9] = b.query_start.map_or(Value::Null, Value::Timestamptz);
            row[10] = Value::Timestamptz(b.backend_start);
            row[11] = b.query_start.map_or(Value::Null, Value::Timestamptz);
            match &b.client_addr {
                Some(s) => {
                    row[12] = crate::types::text::parse_inet(s, false).unwrap_or(Value::Null);
                    row[14] = Value::Int4(0);
                }
                None => row[14] = Value::Int4(-1),
            }
            row[17] = text("client backend");
            // query_id: this session's, when compute_query_id computes it and the query is
            // reported (a hash of the query text stands in for Postgres's jumble).
            if b.pid == c.exec.sess.pid {
                let settings = c.exec.sess.settings.borrow();
                let on = |n: &str, v: &[&str]| {
                    settings.show(n).is_ok_and(|s| v.contains(&s.as_str()))
                };
                if on("compute_query_id", &["on", "regress"]) && on("track_activities", &["on"]) {
                    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                    for byte in b.query.bytes() {
                        h = (h ^ byte as u64).wrapping_mul(0x0100_0000_01b3);
                    }
                    row[30] = Value::Int8(h as i64);
                }
            }
            row[18] = Value::Bool(false);
            row[25] = Value::Bool(false);
            row[27] = Value::Bool(false);
            row[28] = Value::Bool(false);
        } else {
            row[5] = text("<insufficient privilege>");
        }
        out.push(row);
    }
    // The auxiliary processes: no database, client or query.
    let started = c.exec.sess.cluster.start_time();
    for (k, (pid, kind, event, superuser)) in AUX_PROCESSES.iter().enumerate() {
        if want.is_some_and(|p| p != *pid as i64) {
            continue;
        }
        let role = superuser.then_some(10u32);
        let mut row = vec![Value::Null; 31];
        row[1] = Value::Int4(*pid);
        row[2] = role.map_or(Value::Null, Value::Oid);
        row[3] = text("");
        if all_stats || role.is_some_and(|r| cat.has_privs_of_role(me, r, dbo)) || cat.is_superuser(me) {
            row[5] = text("");
            row[6] = text("Activity");
            row[7] = text(*event);
            row[10] = Value::Timestamptz(started + 1000 * (k as i64 + 1));
            row[17] = text(*kind);
            row[18] = Value::Bool(false);
            row[25] = Value::Bool(false);
            row[27] = Value::Bool(false);
            row[28] = Value::Bool(false);
        } else {
            row[5] = text("<insufficient privilege>");
        }
        out.push(row);
    }
    out
}

/// Column `col` of pg_stat_get_activity for backend `a[0]` (pg_stat_get_backend_idset's
/// numbering); text columns hidden from the caller read "<insufficient privilege>".
fn backend_col(a: &[Value], c: &FnCtx, col: usize) -> Result<Value> {
    let Some(i) = a.first().and_then(|v| v.as_i64()) else {
        return Ok(Value::Null);
    };
    let all = activity_rows(None, c);
    let Some(row) = all.iter().find(|r| r[1] == Value::Int4(i as i32)) else {
        return Ok(if col == 5 {
            text("<backend information not available>")
        } else {
            Value::Null
        });
    };
    // (No command reported yet, as an auxiliary process has none.)
    if col == 5 && matches!(&row[col], Value::Text(t) if t.is_empty()) {
        return Ok(text("<command string not enabled>"));
    }
    Ok(row[col].clone())
}

macro_rules! backend_fns {
    ($($name:ident = $col:expr),* $(,)?) => {
        $(fn $name(a: &[Value], c: &FnCtx) -> Result<Value> {
            backend_col(a, c, $col)
        })*
    };
}

backend_fns!(
    backend_dbid = 0,
    backend_pid = 1,
    backend_userid = 2,
    backend_activity = 5,
    backend_wait_event_type = 6,
    backend_wait_event = 7,
    backend_xact_start = 8,
    backend_activity_start = 9,
    backend_start = 10,
    backend_client_addr = 12,
    backend_client_port = 14
);

/// pg_stat_get_backend_idset: the backends' ids, here their pids (the number a temporary
/// schema is named by, pg_temp_N).
fn backend_idset(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    rows(
        activity_rows(None, c)
            .into_iter()
            .map(|r| vec![r[1].clone()])
            .collect(),
    )
}

fn wait_events(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(
        include_str!("data/wait_events.tsv")
            .lines()
            .map(|l| l.split('\t').map(text).collect())
            .collect(),
    )
}

fn pg_config(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(
        include_str!("data/pg_config.tsv")
            .lines()
            .map(|l| l.split('\t').map(text).collect())
            .collect(),
    )
}

fn abbrev_row(f: &[&str], c: &FnCtx) -> Vec<Value> {
    let off = crate::types::text::parse_value(f[1], &DataType::Interval(-1), c.exec.fmt())
        .unwrap_or(Value::Null);
    vec![text(f[0]), off, Value::Bool(f[2] == "t")]
}

/// The abbreviations the timezone_abbreviations file (Default) defines.
fn tz_abbrevs(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    rows(
        include_str!("data/tz_abbrevs.tsv")
            .lines()
            .map(|l| abbrev_row(&l.split('\t').collect::<Vec<_>>(), c))
            .collect(),
    )
}

/// pg_timezone_abbrevs_zone: the abbreviations the session's time zone has ever used (its
/// tzfile's, in order), each with its meaning at the transaction's start (or nearest to it).
/// (The pg_timezone_abbrevs view leaves out the abbreviations file's entries for these.)
fn tz_abbrevs_zone(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let fmt = c.exec.fmt();
    let tz = &fmt.tz.0;
    // Transaction start, in Unix seconds.
    let now = c.exec.txn_time().div_euclid(1_000_000) + 946_684_800;
    let mut out = Vec::new();
    for abbrev in crate::types::tz::zone_abbrevs(tz) {
        if abbrev.is_empty() || abbrev.starts_with(['+', '-']) {
            continue;
        }
        let Some((off, dst)) = crate::types::tz::pg_interpret_timezone_abbrev(&abbrev, now, tz)
        else {
            continue;
        };
        out.push(vec![
            text(abbrev),
            Value::Interval(crate::types::value::Interval::new(0, 0, off * 1_000_000)),
            Value::Bool(dst),
        ]);
    }
    rows(out)
}

/// The extensions that can be installed, at their one version.
fn extension_versions(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    rows(
        crate::session::ddl::EXTENSIONS
            .iter()
            .map(|(n, v, comment)| {
                let pl = *n == "plpgsql";
                vec![
                    text(*n),
                    text(*v),
                    Value::Bool(true),
                    Value::Bool(pl),
                    Value::Bool(!pl),
                    if pl {
                        Value::Text(Arc::from("pg_catalog"))
                    } else {
                        Value::Null
                    },
                    Value::Null,
                    text(*comment),
                ]
            })
            .collect(),
    )
}

/// pg_hba_file_rules: the one rule the server applies, to every connection: SCRAM when the
/// session authenticated with a password, trust otherwise.
fn hba_file_rules(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let file = c.exec.sess.settings.borrow().get_raw("hba_file").unwrap_or_default();
    let scram = c
        .exec
        .sess
        .system_user
        .as_deref()
        .is_some_and(|u| u.starts_with("scram-sha-256:"));
    let all = || {
        Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(
            DataType::Text,
            vec![text("all")],
        )))
    };
    rows(vec![vec![
        Value::Int4(1),
        text(file),
        Value::Int4(1),
        text("host"),
        all(),
        all(),
        text("all"),
        Value::Null,
        text(if scram { "scram-sha-256" } else { "trust" }),
        Value::Null,
        Value::Null,
    ]])
}

/// pg_get_backend_memory_contexts: the backend's memory context tree as Postgres lays it out
/// (the long-lived contexts, and a tuplesort's under each portal still sorting), sizes as an
/// idle backend's.
fn memory_contexts(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    // (name, ident, type, parent index, total bytes, blocks, free bytes, free chunks)
    let mut ctxs: Vec<(&str, Option<String>, &str, Option<usize>, i64, i64, i64, i64)> = vec![
        ("TopMemoryContext", None, "AllocSet", None, 99136, 5, 6896, 13),
        ("TopTransactionContext", None, "AllocSet", Some(0), 8192, 1, 7752, 0),
        ("MessageContext", None, "AllocSet", Some(0), 16384, 2, 6016, 2),
        ("CacheMemoryContext", None, "AllocSet", Some(0), 524288, 7, 189784, 0),
        ("index info", Some("pg_class_oid_index".into()), "AllocSet", Some(3), 2048, 2, 704, 2),
        ("index info", Some("pg_type_oid_index".into()), "AllocSet", Some(3), 2048, 2, 704, 2),
        ("PortalContext", None, "AllocSet", Some(0), 1024, 1, 600, 0),
    ];
    for (name, cur) in c.exec.sess.cursors.borrow().iter() {
        if !cur.lazy.as_ref().is_some_and(|l| l.sorting()) {
            continue;
        }
        let portal = ctxs.len();
        ctxs.push(("PortalContext", Some(name.clone()), "AllocSet", Some(0), 1024, 1, 592, 0));
        ctxs.push(("ExecutorState", None, "AllocSet", Some(portal), 49272, 4, 15128, 3));
        let main = ctxs.len();
        ctxs.push(("TupleSort main", None, "AllocSet", Some(main - 1), 32832, 2, 15848, 0));
        ctxs.push(("TupleSort sort", None, "AllocSet", Some(main), 8192, 1, 7952, 0));
        ctxs.push(("Caller tuples", None, "Bump", Some(main + 1), 16384, 2, 5544, 0));
    }
    // Paths: the context ids (1-based positions) from the top down.
    let mut paths: Vec<Vec<Value>> = Vec::with_capacity(ctxs.len());
    for (i, x) in ctxs.iter().enumerate() {
        let mut p = x.3.map(|parent| paths[parent].clone()).unwrap_or_default();
        p.push(Value::Int4(i as i32 + 1));
        paths.push(p);
    }
    rows(
        ctxs.into_iter()
            .zip(paths)
            .map(|((name, ident, ty, _, total, blocks, free, chunks), path)| {
                vec![
                    text(name),
                    ident.map_or(Value::Null, text),
                    text(ty),
                    Value::Int4(path.len() as i32),
                    Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(
                        DataType::Int4,
                        path,
                    ))),
                    Value::Int8(total),
                    Value::Int8(blocks),
                    Value::Int8(free),
                    Value::Int8(chunks),
                    Value::Int8(total - free),
                ]
            })
            .collect(),
    )
}

fn getdatabaseencoding(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Text(Arc::from("UTF8")))
}

/// nameconcatoid: `name_oid`, with the name cut so it all fits in a name.
fn nameconcatoid(a: &[Value], _: &FnCtx) -> Result<Value> {
    let suffix = format!("_{}", a[1].as_i64().unwrap_or(0) as u32);
    let mut name = text_arg(&a[0]).to_string();
    while name.len() + suffix.len() > 63 {
        name.pop();
    }
    Ok(Value::Text(Arc::from(format!("{name}{suffix}"))))
}

/// pg_mcv_list_items: an MCV list's items, values as text.
fn mcv_list_items(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let Value::Bytea(b) = &a[0] else {
        return rows(Vec::new());
    };
    let cat = c.exec.catalog();
    let Some((types, items)) = crate::catalog::system::mcv_items(&cat, b) else {
        return rows(Vec::new());
    };
    let fmt = c.exec.fmt();
    let array = |elem: DataType, items: Vec<Value>| {
        Value::Array(Arc::new(crate::types::array::ArrayValue::from_vec(
            elem, items,
        )))
    };
    rows(
        items
            .iter()
            .enumerate()
            .map(|(i, it)| {
                let values = it
                    .values
                    .iter()
                    .zip(&types)
                    .zip(&it.nulls)
                    .map(|((v, ty), null)| {
                        if *null {
                            Value::Null
                        } else {
                            text(crate::types::text::format_value(v, ty, fmt))
                        }
                    })
                    .collect();
                vec![
                    Value::Int4(i as i32),
                    array(DataType::Text, values),
                    array(
                        DataType::Bool,
                        it.nulls.iter().map(|n| Value::Bool(*n)).collect(),
                    ),
                    Value::Float8(it.frequency),
                    Value::Float8(it.base_frequency),
                ]
            })
            .collect(),
    )
}

/// pg_options_to_table: `name=value` options as rows.
fn options_to_table(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    let Value::Array(arr) = &a[0] else {
        return rows(Vec::new());
    };
    rows(
        arr.items
            .iter()
            .filter_map(|v| match v {
                Value::Null => None,
                v => {
                    let s = text_arg(v);
                    Some(match s.split_once('=') {
                        Some((k, v)) => vec![text(k), text(v)],
                        None => vec![text(s), Value::Null],
                    })
                }
            })
            .collect(),
    )
}

// ── information_schema's helpers ──

fn int_arg(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

fn int(v: Option<i64>) -> Value {
    v.map_or(Value::Null, |n| Value::Int4(n as i32))
}

fn char_max_length(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (t, m) = (int_arg(&a[0]), int_arg(&a[1]));
    Ok(int(match t {
        _ if m == -1 => None,
        1042 | 1043 => Some(m - 4),
        1560 | 1562 => Some(m),
        _ => None,
    }))
}

fn char_octet_length(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (t, m) = (int_arg(&a[0]), int_arg(&a[1]));
    if !matches!(t, 25 | 1042 | 1043) {
        return Ok(Value::Null);
    }
    if m == -1 {
        return Ok(Value::Int4(1 << 30));
    }
    // UTF8: up to 4 bytes a character.
    Ok(match char_max_length(a, c)? {
        Value::Int4(n) => Value::Int4(n * 4),
        v => v,
    })
}

fn numeric_precision(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (t, m) = (int_arg(&a[0]), int_arg(&a[1]));
    Ok(int(match t {
        21 => Some(16),
        23 => Some(32),
        20 => Some(64),
        1700 if m == -1 => None,
        1700 => Some(((m - 4) >> 16) & 65535),
        700 => Some(24),
        701 => Some(53),
        _ => None,
    }))
}

fn numeric_precision_radix(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(int(match int_arg(&a[0]) {
        21 | 23 | 20 | 700 | 701 => Some(2),
        1700 => Some(10),
        _ => None,
    }))
}

fn numeric_scale(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (t, m) = (int_arg(&a[0]), int_arg(&a[1]));
    Ok(int(match t {
        21 | 23 | 20 => Some(0),
        1700 if m == -1 => None,
        1700 => Some((m - 4) & 65535),
        _ => None,
    }))
}

fn datetime_precision(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (t, m) = (int_arg(&a[0]), int_arg(&a[1]));
    Ok(int(match t {
        1082 => Some(0),
        1083 | 1114 | 1184 | 1266 => Some(if m < 0 { 6 } else { m }),
        1186 => Some(if m < 0 || m & 65535 == 65535 {
            6
        } else {
            m & 65535
        }),
        _ => None,
    }))
}

/// The fields of an interval type (`DAY TO SECOND`), from format_type.
fn interval_type(a: &[Value], c: &FnCtx) -> Result<Value> {
    if int_arg(&a[0]) != 1186 {
        return Ok(Value::Null);
    }
    let shown = crate::sql::func::system::format_type_oid(
        1186,
        int_arg(&a[1]) as i32,
        &c.exec.catalog(),
        &c.exec.search_path,
    )
    .unwrap_or_default();
    // SIMILAR 'interval[()0-9]* #"%#"': what follows the type name and a space.
    let rest = shown.strip_prefix("interval").unwrap_or("");
    let rest = rest.trim_start_matches(|ch: char| ch == '(' || ch == ')' || ch.is_ascii_digit());
    Ok(match rest.strip_prefix(' ') {
        Some(fields) => Value::Text(Arc::from(fields.to_uppercase())),
        None => Value::Null,
    })
}

/// The position of column `attnum` in index `indexrelid`'s key.
fn index_position(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (ix, att) = (int_arg(&a[0]) as u32, int_arg(&a[1]));
    let cat = c.exec.catalog();
    let Some(i) = cat.indexes.get(&ix) else {
        return Ok(Value::Null);
    };
    let cols = crate::catalog::system::index_cols(i);
    Ok(int(cols
        .iter()
        .position(|x| *x == att)
        .map(|p| p as i64 + 1)))
}

/// _pg_expandarray: each element with its position.
fn expandarray(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    let items: Vec<Value> = match &a[0] {
        Value::Array(arr) => arr.items.clone(),
        _ => Vec::new(),
    };
    rows(
        items
            .into_iter()
            .enumerate()
            .map(|(i, v)| vec![v, Value::Int4(i as i32 + 1)])
            .collect(),
    )
}

pub fn register(r: &mut Registry) {
    // Built from what the backends report.
    r.from_pg(2022, None, Some(activity));
    r.from_pg(1936, None, Some(backend_idset));
    r.from_pg(3195, None, Some(archiver));
    r.from_pg(1136, None, Some(wal));
    r.from_pg(6248, None, Some(recovery_prefetch));
    r.from_pg(6169, None, Some(replication_slot));
    r.from_pg(6313, None, Some(backend_wal));
    r.from_pg(3317, None, Some(wal_receiver));
    r.from_pg(2306, None, Some(slru));
    r.from_pg(6214, None, Some(io));
    r.from_pg(6386, None, Some(backend_io));
    r.from_pg(3099, None, Some(no_rows));
    r.from_pg(3318, None, Some(super::system::pg_stat_get_progress_info));
    r.from_pg(6118, None, Some(no_rows));
    let backend: [(&str, FnImpl); 11] = [
        ("dbid", backend_dbid),
        ("pid", backend_pid),
        ("userid", backend_userid),
        ("activity", backend_activity),
        ("wait_event_type", backend_wait_event_type),
        ("wait_event", backend_wait_event),
        ("xact_start", backend_xact_start),
        ("activity_start", backend_activity_start),
        ("start", backend_start),
        ("client_addr", backend_client_addr),
        ("client_port", backend_client_port),
    ];
    for s in builtin_proc_sigs() {
        if let Some(f) = s
            .name
            .strip_prefix("pg_stat_get_backend_")
            .and_then(|n| backend.iter().find(|b| b.0 == n))
        {
            r.from_pg(s.oid, Some(f.1), None);
        }
    }
    // Every other statistics counter.
    for s in builtin_proc_sigs() {
        if s.name.starts_with("pg_stat_get_")
            && !s.retset
            && s.out.is_empty()
            && !r.funcs.get(s.name).is_some_and(|v| {
                v.iter()
                    .any(|d| d.args.iter().map(|t| t.oid()).eq(s.args.iter().copied()))
            })
        {
            let untracked = [
                "pg_stat_get_function_",
                "pg_stat_get_xact_function_",
                "pg_stat_get_db_checksum_",
            ]
            .iter()
            .any(|p| s.name.starts_with(p));
            r.from_pg(s.oid, Some(if untracked { no_stat } else { counter }), None);
        }
    }
    // Server files, shared memory, replication and two-phase state: nothing to show.
    for oid in [6399, 5052, 4100, 3329, 6250, 1065, 6014, 3781] {
        r.from_pg(oid, None, Some(no_rows));
    }
    r.from_pg(3400, None, Some(pg_config));
    r.from_pg(2282, None, Some(memory_contexts));
    r.from_pg(3401, None, Some(hba_file_rules));
    r.from_pg(6318, None, Some(wait_events));
    r.from_pg(6401, None, Some(tz_abbrevs_zone));
    r.from_pg(2599, None, Some(tz_abbrevs));
    r.from_pg(3083, None, Some(extension_versions));
    r.from_pg(1039, Some(getdatabaseencoding), None);
    r.from_pg(266, Some(nameconcatoid), None);
    r.from_pg(2289, None, Some(options_to_table));
    r.from_pg(3427, None, Some(mcv_list_items));
    // information_schema.
    r.from_pg(13328, None, Some(expandarray));
    r.from_pg(13329, Some(index_position), None);
    r.from_pg(13332, Some(char_max_length), None);
    r.from_pg(13333, Some(char_octet_length), None);
    r.from_pg(13335, Some(numeric_precision), None);
    r.from_pg(13336, Some(numeric_precision_radix), None);
    r.from_pg(13337, Some(numeric_scale), None);
    r.from_pg(13338, Some(datetime_precision), None);
    r.from_pg(13339, Some(interval_type), None);
}
