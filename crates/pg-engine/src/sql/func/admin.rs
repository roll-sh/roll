//! Server administration functions (Postgres's genfile.c, misc.c, pgstatfuncs.c resets):
//! configuration reload, log files, statistics resets and server file access. There is no
//! server log or data directory to reach here: log and directory listings are empty, and
//! reading server files is refused (as COPY to or from a file is) unless the server allows
//! them in a directory of its own (`crate::server_files`).

use super::{FnCtx, Registry, SrfIter, text_arg};
use crate::error::{Error, Result, code};
use crate::session::warning;
use crate::types::{DataType, Value};

fn arg_types(a: &[Value], c: &FnCtx) -> Vec<DataType> {
    (0..a.len()).map(|i| c.arg_type(i)).collect()
}

fn restore_relation_stats(a: &[Value], c: &FnCtx) -> Result<Value> {
    crate::session::ddl::stats_import::restore_relation_stats(c.exec, a, &arg_types(a, c))
        .map(Value::Bool)
}

fn restore_attribute_stats(a: &[Value], c: &FnCtx) -> Result<Value> {
    crate::session::ddl::stats_import::restore_attribute_stats(c.exec, a, &arg_types(a, c))
        .map(Value::Bool)
}

fn clear_relation_stats(a: &[Value], c: &FnCtx) -> Result<Value> {
    crate::session::ddl::stats_import::clear_relation_stats(c.exec, &a[0], &a[1])?;
    Ok(Value::Null)
}

fn clear_attribute_stats(a: &[Value], c: &FnCtx) -> Result<Value> {
    crate::session::ddl::stats_import::clear_attribute_stats(c.exec, a)?;
    Ok(Value::Null)
}

fn pg_reload_conf(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(true))
}

fn pg_rotate_logfile(_: &[Value], c: &FnCtx) -> Result<Value> {
    c.exec.notice(warning(
        code::WARNING,
        "rotation not possible because log collection not active",
    ));
    Ok(Value::Bool(false))
}

fn pg_current_logfile(a: &[Value], _: &FnCtx) -> Result<Value> {
    if let Some(v) = a.first().filter(|v| !v.is_null())
        && !matches!(text_arg(v), "stderr" | "csvlog" | "jsonlog")
    {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("log format \"{}\" is not supported", text_arg(v)),
        )
        .hint("The supported log formats are \"stderr\", \"csvlog\", and \"jsonlog\"."));
    }
    Ok(Value::Null)
}

fn pg_log_backend_memory_contexts(a: &[Value], c: &FnCtx) -> Result<Value> {
    let pid = a[0].as_i64().unwrap_or(0) as i32;
    if c.exec.sess.cluster.activity().iter().any(|b| b.pid == pid)
        || super::monitor::is_aux_process(pid as i64)
    {
        return Ok(Value::Bool(true));
    }
    c.exec.notice(warning(
        code::WARNING,
        format!("PID {pid} is not a PostgreSQL server process"),
    ));
    Ok(Value::Bool(false))
}

/// Statistics are not collected, so resetting them changes nothing.
fn stat_reset(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Null)
}

fn pg_stat_reset_shared(a: &[Value], c: &FnCtx) -> Result<Value> {
    const ALL: [&str; 7] = [
        "archiver",
        "bgwriter",
        "checkpointer",
        "io",
        "recovery_prefetch",
        "slru",
        "wal",
    ];
    if let Some(v) = a.first().filter(|v| !v.is_null()) {
        let t = text_arg(v);
        if !ALL.contains(&t) {
            return Err(Error::new(code::INVALID_PARAMETER_VALUE, format!("unrecognized reset target: \"{t}\""))
                .hint("Target must be \"archiver\", \"bgwriter\", \"checkpointer\", \"io\", \"recovery_prefetch\", \"slru\", or \"wal\"."));
        }
        reset_target(t, c);
    } else {
        for t in ALL {
            reset_target(t, c);
        }
    }
    Ok(Value::Null)
}

/// The SLRUs pg_stat_slru lists.
pub(crate) const SLRUS: [&str; 8] = [
    "commit_timestamp",
    "multixact_member",
    "multixact_offset",
    "notify",
    "serializable",
    "subtransaction",
    "transaction",
    "other",
];

fn reset_target(t: &str, c: &FnCtx) {
    if t == "slru" {
        for s in SLRUS {
            crate::exec::pgstat::reset_shared(c.exec, &format!("slru:{s}"));
        }
    } else {
        crate::exec::pgstat::reset_shared(c.exec, t);
        // (The bgwriter's and checkpointer's reset together.)
        if t == "bgwriter" {
            crate::exec::pgstat::reset_shared(c.exec, "checkpointer");
        }
    }
}

/// pg_stat_reset_slru(target): one SLRU (an unknown name counts as "other"), or all.
fn pg_stat_reset_slru(a: &[Value], c: &FnCtx) -> Result<Value> {
    match a.first().filter(|v| !v.is_null()) {
        Some(v) => {
            let t = text_arg(v);
            let name = if SLRUS.contains(&t) { t } else { "other" };
            crate::exec::pgstat::reset_shared(c.exec, &format!("slru:{name}"));
        }
        None => reset_target("slru", c),
    }
    Ok(Value::Null)
}

fn pg_stat_reset(_: &[Value], c: &FnCtx) -> Result<Value> {
    crate::exec::pgstat::reset_db(c.exec);
    Ok(Value::Null)
}

fn pg_stat_reset_single_table_counters(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = match &a[0] {
        Value::Oid(o) => *o,
        v => v.as_i64().unwrap_or(0) as u32,
    };
    let shared = crate::exec::pgstat::is_shared(oid);
    crate::exec::pgstat::reset_table(c.exec, oid, shared);
    Ok(Value::Null)
}

fn pg_stat_reset_single_function_counters(a: &[Value], c: &FnCtx) -> Result<Value> {
    let oid = match &a[0] {
        Value::Oid(o) => *o,
        v => v.as_i64().unwrap_or(0) as u32,
    };
    crate::exec::pgstat::reset_function(c.exec, oid);
    Ok(Value::Null)
}

fn no_slot(name: &str) -> Error {
    Error::new(
        code::UNDEFINED_OBJECT,
        format!("replication slot \"{name}\" does not exist"),
    )
}

fn pg_stat_reset_replication_slot(a: &[Value], _: &FnCtx) -> Result<Value> {
    match a.first().filter(|v| !v.is_null()) {
        Some(v) => Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("replication slot \"{}\" does not exist", text_arg(v)),
        )),
        None => Ok(Value::Null),
    }
}

fn pg_stat_have_stats(a: &[Value], c: &FnCtx) -> Result<Value> {
    let kind = text_arg(&a[0]);
    let (db, obj) = (
        a[1].as_i64().unwrap_or(0) as u32,
        a[2].as_i64().unwrap_or(0),
    );
    Ok(Value::Bool(match kind {
        // Fixed-numbered statistics always exist.
        "archiver" | "bgwriter" | "checkpointer" | "io" | "slru" | "wal" => true,
        "database" => obj == 0 && (db == 0 || db == c.exec.sess.database_oid()),
        // (Every relation and function has an entry from its creation to its drop: those
        // in the committed catalog or this transaction's.)
        "relation" | "function" if db == c.exec.sess.database_oid() => {
            let o = obj as u32;
            let has = |cat: &crate::catalog::Catalog| {
                if kind == "function" {
                    cat.functions.contains_key(&o)
                } else {
                    cat.tables.contains_key(&o)
                        || cat.indexes.contains_key(&o)
                        || cat.views.get(&o).is_some_and(|v| v.materialized)
                }
            };
            has(&c.exec.sess.db.catalog()) || has(&c.exec.catalog())
        }
        "relation" | "function" | "replslot" | "subscription" | "backend" => false,
        _ => {
            return Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("invalid statistics kind: \"{kind}\""),
            ));
        }
    }))
}

/// Server files aren't reachable unless the server allows them (`--allow-server-files`): the
/// error is the one Postgres gives a role without EXECUTE on the function (granted only to
/// the bootstrap superuser).
fn server_files(what: &str) -> Error {
    Error::new(
        code::INSUFFICIENT_PRIVILEGE,
        format!("permission denied for function {what}"),
    )
}

/// convert_and_check_filename: the name as messages show it (canonicalized) and where it
/// is, inside the server's files directory.
fn checked_file(c: &FnCtx, what: &str, name: &Value) -> Result<(String, std::path::PathBuf)> {
    let Some(sf) = c.exec.sess.cluster.server_files() else {
        return Err(server_files(what));
    };
    let name = text_arg(name);
    Ok((crate::server_files::canonicalize_path(name), sf.resolve(name)?))
}

/// pg_read_file_common's argument check, which comes before the file is opened.
fn read_length_check(a: &[Value]) -> Result<()> {
    if a.get(2).and_then(Value::as_i64).is_some_and(|n| n < 0) {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "requested length cannot be negative",
        ));
    }
    Ok(())
}

/// read_binary_file: `len` bytes from `offset` (from the end if negative), all of the rest
/// without `len`; None for a missing file with `missing_ok`.
fn read_server_file(a: &[Value], c: &FnCtx, what: &str) -> Result<Option<Vec<u8>>> {
    use std::io::{Read, Seek, SeekFrom};
    read_length_check(a)?;
    let (name, path) = checked_file(c, what, &a[0])?;
    let (offset, len, missing_ok) = match a.len() {
        2 => (0, None, a[1].as_bool() == Some(true)),
        3 | 4 => (
            a[1].as_i64().unwrap_or(0),
            a[2].as_i64(),
            a.get(3).and_then(Value::as_bool) == Some(true),
        ),
        _ => (0, None, false),
    };
    // MaxAllocSize - VARHDRSZ.
    const MAX_LEN: i64 = 0x3fff_ffff - 4;
    if len.is_some_and(|n| n > MAX_LEN) {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "requested length too large",
        ));
    }
    let mut f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if missing_ok && crate::server_files::is_enoent(&e) => return Ok(None),
        Err(e) => {
            return Err(crate::server_files::file_error(
                &e,
                &format!("could not open file \"{name}\" for reading"),
            ));
        }
    };
    let pos = if offset >= 0 {
        SeekFrom::Start(offset as u64)
    } else {
        SeekFrom::End(offset)
    };
    f.seek(pos).map_err(|e| {
        crate::server_files::file_error(&e, &format!("could not seek in file \"{name}\""))
    })?;
    let mut buf = Vec::new();
    let read = match len {
        Some(n) => f.take(n as u64).read_to_end(&mut buf),
        None => f.take(MAX_LEN as u64 + 1).read_to_end(&mut buf),
    };
    read.map_err(|e| crate::server_files::file_error(&e, &format!("could not read file \"{name}\"")))?;
    if len.is_none() && buf.len() as i64 > MAX_LEN {
        return Err(Error::new(code::PROGRAM_LIMIT_EXCEEDED, "file length too large"));
    }
    Ok(Some(buf))
}

fn pg_read_file(a: &[Value], c: &FnCtx) -> Result<Value> {
    let Some(b) = read_server_file(a, c, "pg_read_file")? else {
        return Ok(Value::Null);
    };
    // read_text_file: the contents must be valid in the database encoding.
    crate::types::mbconv::verify(crate::types::mbconv::UTF8, &b)?;
    match String::from_utf8(b) {
        Ok(s) => Ok(Value::text(s)),
        Err(_) => Err(crate::error::internal("invalid UTF-8 after verification")),
    }
}

fn pg_read_binary_file(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(match read_server_file(a, c, "pg_read_binary_file")? {
        Some(b) => Value::bytea(b),
        None => Value::Null,
    })
}

/// time_t_to_timestamptz.
fn time_t(secs: i64) -> Value {
    Value::Timestamptz((secs - 946_684_800) * 1_000_000)
}

fn pg_stat_file(a: &[Value], c: &FnCtx) -> Result<Value> {
    use std::os::unix::fs::MetadataExt;
    let missing_ok = a.get(1).and_then(Value::as_bool) == Some(true);
    let (name, path) = checked_file(c, "pg_stat_file", &a[0])?;
    let m = match std::fs::metadata(&path) {
        Ok(m) => m,
        Err(e) if missing_ok && crate::server_files::is_enoent(&e) => return Ok(Value::Null),
        Err(e) => {
            return Err(crate::server_files::file_error(
                &e,
                &format!("could not stat file \"{name}\""),
            ));
        }
    };
    // Unix has the status change time, no creation time.
    Ok(Value::Record(std::sync::Arc::new(vec![
        Value::Int8(m.size() as i64),
        time_t(m.atime()),
        time_t(m.mtime()),
        time_t(m.ctime()),
        Value::Null,
        Value::Bool(m.is_dir()),
    ])))
}

fn pg_ls_dir(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let (name, path) = checked_file(c, "pg_ls_dir", &a[0])?;
    let missing_ok = a.get(1).and_then(Value::as_bool) == Some(true);
    let include_dot_dirs = a.get(2).and_then(Value::as_bool) == Some(true);
    let dir_error = |e: std::io::Error| {
        crate::server_files::file_error(&e, &format!("could not open directory \"{name}\""))
    };
    let entries = match std::fs::read_dir(&path) {
        Ok(d) => d,
        Err(e) if missing_ok && crate::server_files::is_enoent(&e) => {
            return Ok(Box::new(std::iter::empty()));
        }
        Err(e) => return Err(dir_error(e)),
    };
    // readdir's "." and ".." (std leaves them out).
    let mut out = Vec::new();
    if include_dot_dirs {
        out.push(Ok(vec![Value::text(".")]));
        out.push(Ok(vec![Value::text("..")]));
    }
    for e in entries {
        let e = e.map_err(|e| {
            crate::server_files::file_error(&e, &format!("could not read directory \"{name}\""))
        })?;
        out.push(Ok(vec![Value::text(e.file_name().to_string_lossy().into_owned())]));
    }
    Ok(Box::new(out.into_iter()))
}

/// pg_ls_logdir & co.: no files.
fn ls_empty(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    Ok(Box::new(std::iter::empty()))
}

/// WAL geometry: the segment size (wal_segment_size) and the one timeline there is.
const WAL_SEGMENT_SIZE: u64 = 16 * 1024 * 1024;
const SEGMENTS_PER_ID: u64 = 0x1_0000_0000 / WAL_SEGMENT_SIZE;

/// XLogFileName: timeline, then the segment as two 32-bit halves.
fn wal_file_name(tli: u32, segno: u64) -> String {
    format!(
        "{tli:08X}{:08X}{:08X}",
        segno / SEGMENTS_PER_ID,
        segno % SEGMENTS_PER_ID
    )
}

fn lsn_arg(v: &Value) -> u64 {
    match v {
        Value::PgLsn(l) => *l,
        v => v.as_i64().unwrap_or(0) as u64,
    }
}

/// pg_ls_waldir: the segment the server writes now.
fn pg_ls_waldir(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let now = Value::Timestamptz(c.exec.stmt_time());
    Ok(Box::new(std::iter::once(Ok(vec![
        Value::text(wal_file_name(1, 1)),
        Value::Int8(WAL_SEGMENT_SIZE as i64),
        now,
    ]))))
}

/// pg_walfile_name(lsn): the segment holding the byte before `lsn` (XLByteToPrevSeg).
fn pg_walfile_name(a: &[Value], _: &FnCtx) -> Result<Value> {
    let lsn = lsn_arg(&a[0]);
    Ok(Value::text(wal_file_name(1, lsn.saturating_sub(1) / WAL_SEGMENT_SIZE)))
}

/// pg_walfile_name_offset(lsn): the segment holding `lsn`, and its offset there.
fn pg_walfile_name_offset(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    let lsn = lsn_arg(&a[0]);
    Ok(Box::new(std::iter::once(Ok(vec![
        Value::text(wal_file_name(1, lsn / WAL_SEGMENT_SIZE)),
        Value::Int4((lsn % WAL_SEGMENT_SIZE) as i32),
    ]))))
}

/// pg_replication_origin_create: the name's checks (reserved names, MAX_RONAME_LEN);
/// replication origins themselves aren't kept.
fn pg_replication_origin_create(a: &[Value], _: &FnCtx) -> Result<Value> {
    let name = text_arg(&a[0]);
    let reserved = ["any", "none"].iter().any(|r| name.eq_ignore_ascii_case(r));
    if reserved || name.starts_with("pg_") {
        return Err(Error::new(
            code::RESERVED_NAME,
            format!("replication origin name \"{name}\" is reserved"),
        )
        .detail("Origin names \"any\", \"none\", and names starting with \"pg_\" are reserved."));
    }
    if name.len() > 512 {
        return Err(Error::new(code::PROGRAM_LIMIT_EXCEEDED, "replication origin name is too long")
            .detail("Replication origin names must be no longer than 512 bytes."));
    }
    Err(Error::new(
        code::FEATURE_NOT_SUPPORTED,
        "replication origins are not supported",
    ))
}

/// pg_split_walfile_name(name): its segment number and timeline.
fn pg_split_walfile_name(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    let name = text_arg(&a[0]);
    let bad = || {
        Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("invalid WAL file name \"{name}\""),
        )
    };
    let fname = name.rsplit('/').next().unwrap_or(name);
    if fname.len() != 24 || !fname.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    let part = |i: usize| u64::from_str_radix(&fname[i..i + 8], 16).map_err(|_| bad());
    let (tli, log, seg) = (part(0)?, part(8)?, part(16)?);
    if seg >= SEGMENTS_PER_ID {
        return Err(bad());
    }
    let segno = log * SEGMENTS_PER_ID + seg;
    Ok(Box::new(std::iter::once(Ok(vec![
        Value::numeric(crate::types::numeric::Numeric::from_i64(segno as i64)),
        Value::Int8(tli as i64),
    ]))))
}

fn one_row(row: Vec<Value>) -> Result<SrfIter> {
    Ok(Box::new(std::iter::once(Ok(row))))
}

/// pg_control_*: the control file's fields, for this server.
fn pg_control_system(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    one_row(vec![
        Value::Int4(1800),
        Value::Int4(202506291),
        Value::Int8(7_000_000_000_000_000_001),
        Value::Timestamptz(c.exec.stmt_time()),
    ])
}

fn pg_control_checkpoint(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    one_row(vec![
        Value::PgLsn(WAL_SEGMENT_SIZE + 0x28),
        Value::PgLsn(WAL_SEGMENT_SIZE + 0x28),
        Value::text(wal_file_name(1, 1)),
        Value::Int4(1),
        Value::Int4(1),
        Value::Bool(true),
        Value::text("0:1000"),
        Value::Oid(16384),
        Value::Oid(1),
        Value::Oid(0),
        Value::Oid(3),
        Value::Oid(1),
        Value::Oid(0),
        Value::Oid(1),
        Value::Oid(1),
        Value::Oid(0),
        Value::Oid(0),
        Value::Timestamptz(c.exec.stmt_time()),
    ])
}

fn pg_control_init(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    one_row(vec![
        Value::Int4(8),
        Value::Int4(8192),
        Value::Int4(131072),
        Value::Int4(8192),
        Value::Int4(WAL_SEGMENT_SIZE as i32),
        Value::Int4(64),
        Value::Int4(32),
        Value::Int4(1996),
        Value::Int4(2048),
        Value::Bool(true),
        Value::Int4(1),
        Value::Bool(true),
    ])
}

fn pg_control_recovery(_: &[Value], _: &FnCtx) -> Result<SrfIter> {
    one_row(vec![
        Value::PgLsn(0),
        Value::Int4(0),
        Value::PgLsn(0),
        Value::PgLsn(0),
        Value::Bool(false),
    ])
}

/// gist_translate_cmptype_common(cmptype): the GiST strategy number of a CompareType.
fn gist_translate_cmptype_common(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Int2(match a[0].as_i64().unwrap_or(0) {
        1 => 20,
        2 => 21,
        3 => 18,
        4 => 23,
        5 => 22,
        7 => 3,
        8 => 8,
        _ => 0,
    }))
}

/// pg_timezone_names(): the rows of the view of the same name.
fn pg_timezone_names(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let rows = crate::catalog::system::rows(crate::catalog::system::SysTable::PgTimezoneNames, c.exec)?;
    Ok(Box::new(rows.into_iter().map(Ok)))
}

/// pg_tablespace_databases(spc): the databases with objects there (every database has
/// some in pg_default).
fn pg_tablespace_databases(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    if a[0].as_i64() != Some(1663) {
        return Ok(Box::new(std::iter::empty()));
    }
    let rows = crate::catalog::system::rows(crate::catalog::system::SysTable::PgDatabase, c.exec)?;
    Ok(Box::new(
        rows.into_iter()
            .filter_map(|r| r.first().cloned())
            .map(|o| Ok(vec![o]))
            .collect::<Vec<_>>()
            .into_iter(),
    ))
}

/// pg_show_all_settings(): the rows of pg_settings (the view over it).
fn pg_show_all_settings(_: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let rows = crate::catalog::system::rows(crate::catalog::system::SysTable::PgSettings, c.exec)?;
    Ok(Box::new(rows.into_iter().map(Ok)))
}

/// pg_column_toast_chunk_id(value): the chunk_id of the value's out-of-line copy, when it
/// was read from a table storing it in its TOAST table.
fn pg_column_toast_chunk_id(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(crate::exec::sizing::toast_chunk_id(&a[0], c.exec).map_or(Value::Null, Value::Oid))
}

/// The rows of a TOAST table (`pg_toast.pg_toast_<table>`, read directly): its table's
/// out-of-line values, chunked.
pub(crate) fn toast_table_rows(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let oid = match &a[0] {
        Value::Oid(o) => *o,
        v => v.as_i64().unwrap_or(0) as u32,
    };
    let cat = c.exec.catalog();
    let Some((t, _)) = cat.toast_owner(oid) else {
        return Ok(Box::new(std::iter::empty()));
    };
    let rows = crate::exec::sizing::toast_chunks(t, c.exec)?;
    Ok(Box::new(rows.into_iter().map(|(id, seq, data)| {
        Ok(vec![
            Value::Oid(id),
            Value::Int4(seq),
            Value::Bytea(data.into()),
        ])
    })))
}

fn pg_ls_tmpdir_in(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    let oid = a[0].as_i64().unwrap_or(0) as u32;
    if !matches!(oid, 1663 | 1664) {
        return Err(Error::new(
            code::UNDEFINED_OBJECT,
            format!("tablespace with OID {oid} does not exist"),
        ));
    }
    Ok(Box::new(std::iter::empty()))
}

fn pg_ls_replslotdir(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    Err(no_slot(text_arg(&a[0])))
}

pub fn register(r: &mut Registry) {
    use DataType::*;
    r.func("pg_reload_conf", vec![], Bool, pg_reload_conf)
        .volatile();
    r.func("pg_rotate_logfile", vec![], Bool, pg_rotate_logfile)
        .volatile();
    r.func("pg_current_logfile", vec![], Text, pg_current_logfile)
        .volatile()
        .non_strict();
    r.func("pg_current_logfile", vec![Text], Text, pg_current_logfile)
        .volatile()
        .non_strict();
    r.func(
        "pg_log_backend_memory_contexts",
        vec![Int4],
        Bool,
        pg_log_backend_memory_contexts,
    )
    .volatile();
    r.func("pg_stat_reset", vec![], Void, pg_stat_reset)
        .volatile()
        .non_strict();
    // Statistics import (relation_stats.c, attribute_stats.c).
    r.func(
        "pg_restore_relation_stats",
        vec![DataType::array_of(Any)],
        Bool,
        restore_relation_stats,
    )
    .variadic()
    .volatile()
    .non_strict()
    .names(&["kwargs"]);
    r.func(
        "pg_restore_attribute_stats",
        vec![DataType::array_of(Any)],
        Bool,
        restore_attribute_stats,
    )
    .variadic()
    .volatile()
    .non_strict()
    .names(&["kwargs"]);
    r.func(
        "pg_clear_relation_stats",
        vec![Text, Text],
        Void,
        clear_relation_stats,
    )
    .volatile()
    .non_strict()
    .names(&["schemaname", "relname"]);
    r.func(
        "pg_clear_attribute_stats",
        vec![Text, Text, Text, Bool],
        Void,
        clear_attribute_stats,
    )
    .volatile()
    .non_strict()
    .names(&["schemaname", "relname", "attname", "inherited"]);
    // Built without libnuma.
    r.func("pg_numa_available", vec![], DataType::Bool, |_, _| Ok(Value::Bool(false)));
    r.func("pg_trigger_depth", vec![], Int4, |_, _| {
        Ok(Value::Int4(crate::plpgsql::trigger_depth()))
    })
    .volatile();
    // Statistics are flushed when each transaction ends: nothing waits longer.
    r.func("pg_stat_force_next_flush", vec![], Void, stat_reset)
        .volatile()
        .non_strict();
    r.func(
        "pg_stat_reset_shared",
        vec![Text],
        Void,
        pg_stat_reset_shared,
    )
    .volatile()
    .non_strict()
    .names(&["target"])
    .min_args(0)
    .defaults(vec![Value::Null]);
    r.func("pg_stat_reset_slru", vec![Text], Void, pg_stat_reset_slru)
        .volatile()
        .non_strict()
        .names(&["target"])
        .min_args(0)
        .defaults(vec![Value::Null]);
    r.func(
        "pg_stat_reset_single_table_counters",
        vec![Oid],
        Void,
        pg_stat_reset_single_table_counters,
    )
    .volatile();
    r.func(
        "pg_stat_reset_single_function_counters",
        vec![Oid],
        Void,
        pg_stat_reset_single_function_counters,
    )
    .volatile();
    r.func(
        "pg_stat_reset_replication_slot",
        vec![Text],
        Void,
        pg_stat_reset_replication_slot,
    )
    .volatile()
    .non_strict();
    r.func(
        "pg_stat_reset_subscription_stats",
        vec![Oid],
        Void,
        super::pubsub::reset_subscription_stats,
    )
    .volatile()
    .non_strict();
    r.func("pg_stat_reset_backend_stats", vec![Int4], Void, |a, c| {
        if a[0].as_i64() == Some(c.exec.sess.pid as i64) {
            crate::exec::pgstat::reset_backend(c.exec);
        }
        Ok(Value::Null)
    })
        .volatile();
    r.func(
        "pg_stat_have_stats",
        vec![Text, Oid, Int8],
        Bool,
        pg_stat_have_stats,
    )
    .volatile();
    r.func("pg_read_file", vec![Text], Text, pg_read_file)
        .volatile();
    r.func("pg_read_file", vec![Text, Bool], Text, pg_read_file)
        .volatile();
    r.func("pg_read_file", vec![Text, Int8, Int8], Text, pg_read_file)
        .volatile();
    r.func(
        "pg_read_file",
        vec![Text, Int8, Int8, Bool],
        Text,
        pg_read_file,
    )
    .volatile();
    r.func(
        "pg_read_binary_file",
        vec![Text],
        Bytea,
        pg_read_binary_file,
    )
    .volatile();
    r.func(
        "pg_read_binary_file",
        vec![Text, Bool],
        Bytea,
        pg_read_binary_file,
    )
    .volatile();
    r.func(
        "pg_read_binary_file",
        vec![Text, Int8, Int8],
        Bytea,
        pg_read_binary_file,
    )
    .volatile();
    r.func(
        "pg_read_binary_file",
        vec![Text, Int8, Int8, Bool],
        Bytea,
        pg_read_binary_file,
    )
    .volatile();
    let stat_cols = || {
        vec![
            ("size", Int8),
            ("access", Timestamptz(-1)),
            ("modification", Timestamptz(-1)),
            ("change", Timestamptz(-1)),
            ("creation", Timestamptz(-1)),
            ("isdir", Bool),
        ]
    };
    let stat_ret = || {
        DataType::row(
            stat_cols()
                .into_iter()
                .map(|(n, t)| (n.to_string(), t))
                .collect(),
        )
    };
    r.func("pg_stat_file", vec![Text], stat_ret(), pg_stat_file)
        .volatile()
        .names(&["filename"]);
    r.func("pg_stat_file", vec![Text, Bool], stat_ret(), pg_stat_file)
        .volatile()
        .names(&["filename", "missing_ok"]);
    r.srf("pg_ls_dir", vec![Text], Text, pg_ls_dir).volatile();
    r.srf("pg_ls_dir", vec![Text, Bool, Bool], Text, pg_ls_dir)
        .volatile();
    let ls_cols = || {
        vec![
            ("name", Text),
            ("size", Int8),
            ("modification", Timestamptz(-1)),
        ]
    };
    r.srf_cols("pg_ls_waldir", vec![], ls_cols(), pg_ls_waldir).volatile();
    r.from_pg(2851, Some(pg_walfile_name), None);
    r.from_pg(2850, None, Some(pg_walfile_name_offset));
    r.from_pg(6213, None, Some(pg_split_walfile_name));
    r.from_pg(6003, Some(pg_replication_origin_create), None);
    r.from_pg(3441, None, Some(pg_control_system));
    r.from_pg(3442, None, Some(pg_control_checkpoint));
    r.from_pg(3443, None, Some(pg_control_recovery));
    r.from_pg(3444, None, Some(pg_control_init));
    r.from_pg(6347, Some(gist_translate_cmptype_common), None);
    r.from_pg(6316, Some(pg_column_toast_chunk_id), None);
    r.from_pg(2856, None, Some(pg_timezone_names));
    r.from_pg(2084, None, Some(pg_show_all_settings));
    r.from_pg(2556, None, Some(pg_tablespace_databases));
    for name in [
        "pg_ls_logdir",
        "pg_ls_tmpdir",
        "pg_ls_archive_statusdir",
        "pg_ls_summariesdir",
        "pg_ls_logicalsnapdir",
        "pg_ls_logicalmapdir",
    ] {
        r.srf_cols(name, vec![], ls_cols(), ls_empty).volatile();
    }
    r.srf_cols("pg_ls_tmpdir", vec![Oid], ls_cols(), pg_ls_tmpdir_in)
        .volatile()
        .names(&["tablespace"]);
    r.srf_cols(
        "pg_ls_replslotdir",
        vec![Text],
        ls_cols(),
        pg_ls_replslotdir,
    )
    .volatile()
    .names(&["slot_name"]);
}
