//! contrib/file_fdw (REL_18_6 `file_fdw.c`): foreign tables over a server file, read with
//! COPY FROM's parser. The files are the server's files of `--allow-server-files`
//! (`server_files.rs`): resolved and confined as COPY FROM's are, and unreadable (as a file
//! the server process may not open) when the server has none. The `program` option is
//! accepted where PostgreSQL accepts it, but running the program is refused, as COPY ...
//! PROGRAM is.

use super::native::{FdwValidate, Native};
use crate::catalog::{Catalog, Table};
use crate::error::{Error, Result, code};
use crate::exec::{BoxNode, ExecCtx, Node, Row};
use crate::session::copy::{CopyFormat, CopyIn, CopyInParsed, open_copy_file, parse_copy_data};
use crate::sql::binder::ast::mk;
use crate::sql::expr::{Expr, eval, eval_bool};
use crate::sql::plan::ScanPlan;
use crate::types::Value;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

/// pg_foreign_table, pg_attribute: the catalogs a validator's options belong to.
const FOREIGN_TABLE: u32 = 3118;
const ATTRIBUTE: u32 = 1249;
/// ERRCODE_FDW_INVALID_OPTION_NAME, ERRCODE_FDW_DYNAMIC_PARAMETER_VALUE_NEEDED.
const FDW_INVALID_OPTION_NAME: &str = "HV00D";
const FDW_DYNAMIC_PARAMETER_VALUE_NEEDED: &str = "HV002";
const BLCKSZ: u64 = 8192;

/// valid_options: each option and the catalog it may be given in.
const OPTIONS: &[(&str, u32)] = &[
    ("filename", FOREIGN_TABLE),
    ("program", FOREIGN_TABLE),
    ("format", FOREIGN_TABLE),
    ("header", FOREIGN_TABLE),
    ("delimiter", FOREIGN_TABLE),
    ("quote", FOREIGN_TABLE),
    ("escape", FOREIGN_TABLE),
    ("null", FOREIGN_TABLE),
    ("default", FOREIGN_TABLE),
    ("encoding", FOREIGN_TABLE),
    ("on_error", FOREIGN_TABLE),
    ("log_verbosity", FOREIGN_TABLE),
    ("reject_limit", FOREIGN_TABLE),
    ("force_not_null", ATTRIBUTE),
    ("force_null", ATTRIBUTE),
];

pub(super) fn symbol(sym: &str) -> Option<Native> {
    match sym {
        // (Its result is a pointer to the wrapper's routines: no value SQL can show.)
        "file_fdw_handler" => Some(Native::scalar(|_, _| {
            Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot display a value of type fdw_handler",
            ))
        })),
        "file_fdw_validator" => Some(Native::scalar(|args, c| {
            let options: Vec<String> = match &args[0] {
                Value::Array(a) => a
                    .items
                    .iter()
                    .filter_map(|v| match v {
                        Value::Text(s) => Some(s.to_string()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            let catalog = match args[1] {
                Value::Oid(o) => o,
                _ => 0,
            };
            let cat = c.exec.catalog();
            let (me, dbo) = (c.exec.current_user(), c.exec.db_owner());
            validate(&FdwValidate {
                options: &options,
                catalog,
                cat: &cat,
                superuser: cat.is_superuser(me),
                has_privs: &|role| cat.has_privs_of_role(me, role, dbo),
                notice: &|e| c.exec.notice(e),
            })?;
            Ok(Value::Null)
        })),
        _ => None,
    }
}

/// untransformRelOptions: `name=value` as a DefElem with a string argument (none without
/// `=`).
fn def_elem(option: &str) -> pb::DefElem {
    let (name, value) = match option.split_once('=') {
        Some((n, v)) => (n, Some(v)),
        None => (option, None),
    };
    pb::DefElem {
        defname: name.into(),
        arg: value.map(|v| Box::new(mk(NodeEnum::String(pb::String { sval: v.into() })))),
        location: -1,
        ..Default::default()
    }
}

/// defGetBoolean of a string option.
fn def_bool(d: &pb::DefElem) -> Result<bool> {
    let Some(NodeEnum::String(s)) = d.arg.as_deref().and_then(|a| a.node.as_ref()) else {
        return Ok(true);
    };
    match s.sval.to_ascii_lowercase().as_str() {
        "true" | "on" => Ok(true),
        "false" | "off" => Ok(false),
        _ => Err(Error::new(
            code::SYNTAX_ERROR,
            format!("{} requires a Boolean value", d.defname),
        )),
    }
}

fn redundant() -> Error {
    Error::new(code::SYNTAX_ERROR, "conflicting or redundant options")
}

/// file_fdw_validator.
pub fn validate(v: &FdwValidate) -> Result<()> {
    let (mut filename, mut force_not_null, mut force_null) = (false, false, false);
    let mut other = Vec::new();
    for o in v.options {
        let d = def_elem(o);
        let name = d.defname.as_str();
        if !OPTIONS.iter().any(|(n, c)| *n == name && *c == v.catalog) {
            let valid: Vec<&str> = OPTIONS.iter().filter(|(_, c)| *c == v.catalog).map(|(n, _)| *n).collect();
            let e = Error::new(FDW_INVALID_OPTION_NAME, format!("invalid option \"{name}\""));
            return Err(if valid.is_empty() {
                e.hint("There are no valid options in this context.")
            } else {
                match crate::session::ddl::closest_match(name, valid) {
                    Some(m) => e.hint(format!("Perhaps you meant the option \"{m}\".")),
                    None => e,
                }
            });
        }
        match name {
            "filename" | "program" => {
                if filename {
                    return Err(redundant());
                }
                let (role, role_name) = if name == "filename" {
                    (crate::server_files::PG_READ_SERVER_FILES, "pg_read_server_files")
                } else {
                    (crate::server_files::PG_EXECUTE_SERVER_PROGRAM, "pg_execute_server_program")
                };
                if !v.superuser && !(v.has_privs)(role) {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied to set the \"{name}\" option of a file_fdw foreign table"),
                    )
                    .detail(format!("Only roles with privileges of the \"{role_name}\" role may set this option.")));
                }
                filename = true;
            }
            "force_not_null" | "force_null" => {
                let seen = if name == "force_null" { &mut force_null } else { &mut force_not_null };
                if *seen {
                    return Err(redundant().hint(format!("Option \"{name}\" supplied more than once for a column.")));
                }
                *seen = true;
                def_bool(&d)?;
            }
            _ => other.push(mk(NodeEnum::DefElem(Box::new(d)))),
        }
    }
    CopyFormat::parse(&other, true)?;
    if v.catalog == FOREIGN_TABLE && !filename {
        return Err(Error::new(
            FDW_DYNAMIC_PARAMETER_VALUE_NEEDED,
            "either filename or program is required for file_fdw foreign tables",
        ));
    }
    Ok(())
}

/// Whether `t` is a foreign table of a file_fdw wrapper.
pub fn is_file_table(cat: &Catalog, t: &Table) -> bool {
    t.is_foreign()
        && cat.functions.get(&cat.fdw_handler_of(t)).is_some_and(|h| {
            h.body == "file_fdw_handler"
                && h.probin.as_deref().is_some_and(|b| super::native::library(b) == "file_fdw")
        })
}

/// fileGetOptions: the file (or program) and the COPY options of foreign table `t`: the
/// wrapper's, the server's and the table's options, then the columns' FORCE_NOT_NULL and
/// FORCE_NULL as lists.
struct FileOptions {
    filename: String,
    program: bool,
    copy: Vec<pb::Node>,
}

fn file_options(cat: &Catalog, t: &Table) -> Result<FileOptions> {
    let f = t.foreign.as_ref().ok_or_else(|| crate::error::internal("not a foreign table"))?;
    let srv = cat.servers.get(&f.server);
    let fdw = srv.and_then(|s| cat.fdws.get(&s.fdw));
    let all = [fdw.and_then(|w| w.options.as_ref()), srv.and_then(|s| s.options.as_ref()), f.options.as_ref()];
    let mut file = None;
    let mut copy = Vec::new();
    for o in all.into_iter().flatten().flatten() {
        let d = def_elem(o);
        if file.is_none() && matches!(d.defname.as_str(), "filename" | "program") {
            let value = o.split_once('=').map_or("", |(_, v)| v).to_string();
            file = Some((value, d.defname == "program"));
            continue;
        }
        copy.push(mk(NodeEnum::DefElem(Box::new(d))));
    }
    // get_file_fdw_attribute_options.
    let (mut fnn, mut fnull) = (Vec::new(), Vec::new());
    for (i, c) in t.live_columns() {
        for o in f.column_options(i).into_iter().flatten() {
            let d = def_elem(o);
            let list = match d.defname.as_str() {
                "force_not_null" => &mut fnn,
                "force_null" => &mut fnull,
                _ => continue,
            };
            if def_bool(&d)? {
                list.push(mk(NodeEnum::String(pb::String { sval: c.name.clone() })));
            }
        }
    }
    for (name, cols) in [("force_not_null", fnn), ("force_null", fnull)] {
        if !cols.is_empty() {
            copy.push(mk(NodeEnum::DefElem(Box::new(pb::DefElem {
                defname: name.into(),
                arg: Some(Box::new(mk(NodeEnum::List(pb::List { items: cols })))),
                location: -1,
                ..Default::default()
            }))));
        }
    }
    let Some((filename, program)) = file else {
        return Err(crate::error::internal(
            "either filename or program is required for file_fdw foreign tables",
        ));
    };
    Ok(FileOptions { filename, program, copy })
}

/// The file's size, when it can be had (stat on the server's files; never a program's).
fn file_size(o: &FileOptions, ctx: &crate::exec::ExecCtx) -> Option<u64> {
    if o.program {
        return None;
    }
    let path = ctx.sess.cluster.server_files()?.resolve(&o.filename).ok()?;
    std::fs::metadata(path).ok().map(|m| m.len())
}

/// estimate_size: the pages of the file and the tuples in it, by pg_class's density once
/// analyzed, else by the row's width (`width`, the columns the scan reads).
pub fn estimate(t: &Table, width: u32, ctx: &crate::exec::ExecCtx) -> Option<crate::sql::costs::RelSize> {
    let cat = ctx.catalog();
    if !is_file_table(&cat, t) {
        return None;
    }
    let size = file_options(&cat, t)
        .ok()
        .and_then(|o| file_size(&o, ctx))
        .unwrap_or(10 * BLCKSZ);
    let pages = size.div_ceil(BLCKSZ).max(1) as f64;
    let tuples = match cat.relstats.get(&t.oid) {
        Some(s) if s.tuples >= 0.0 && s.pages > 0 => s.tuples / s.pages as f64 * pages,
        _ => size as f64 / (width.next_multiple_of(8) + 24) as f64,
    };
    Some(crate::sql::costs::RelSize {
        pages,
        tuples: crate::sql::costs::clamp_rows(tuples),
    })
}

/// fileExplainForeignScan: "Foreign File" (or "Foreign Program") and the file's size.
pub fn explain_info(t: &Table, ctx: &crate::exec::ExecCtx) -> Option<(&'static str, String, Option<u64>)> {
    let cat = ctx.catalog();
    if !is_file_table(&cat, t) {
        return None;
    }
    let o = file_options(&cat, t).ok()?;
    let size = file_size(&o, ctx);
    Some((
        if o.program { "Foreign Program" } else { "Foreign File" },
        o.filename,
        size,
    ))
}

/// BeginCopyFrom for foreign table `t`: its options checked by COPY's rules, its columns
/// (all but generated ones) and the file opened. A program is refused, as COPY ... PROGRAM
/// is.
fn begin(t: &Arc<Table>, ctx: &ExecCtx) -> Result<FileReader> {
    let cat = ctx.catalog();
    let o = file_options(&cat, t)?;
    let format = CopyFormat::parse(&o.copy, true)?;
    let target: Vec<usize> = t
        .live_columns()
        .filter(|(_, c)| c.generated.is_none())
        .map(|(i, _)| i)
        .collect();
    if o.program {
        return Err(crate::server_files::file_error(
            &std::io::ErrorKind::PermissionDenied.into(),
            &format!("could not execute command \"{}\"", o.filename),
        ));
    }
    let file = open_copy_file(ctx.sess.cluster.server_files(), &o.filename)?;
    let enc = format.encoding.unwrap_or_else(|| {
        let e = ctx.sess.settings.borrow().get_raw("client_encoding");
        e.as_deref()
            .and_then(crate::types::encnames::encoding_id)
            .unwrap_or(crate::types::mbconv::UTF8)
    });
    Ok(FileReader {
        copy: CopyIn {
            table: t.clone(),
            ncols: target.len(),
            target,
            format: Box::new(format),
            filter: None,
            file_size: None,
        },
        name: o.filename,
        file: Some(file),
        buf: Vec::new(),
        st: CopyInParsed::default(),
        rows: Vec::new().into_iter(),
        enc,
        skipped: 0,
    })
}

/// A file read by COPY FROM's parser, a chunk at a time.
struct FileReader {
    copy: CopyIn,
    name: String,
    /// None at the end of the file.
    file: Option<std::fs::File>,
    /// Data read and not yet parsed (an incomplete record).
    buf: Vec<u8>,
    st: CopyInParsed,
    rows: std::vec::IntoIter<Vec<Expr>>,
    enc: i32,
    /// Rows ON_ERROR ignore skipped.
    skipped: usize,
}

impl FileReader {
    /// The next record (the copied columns' values, `Expr::Default` for a DEFAULT marker).
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Vec<Expr>>> {
        loop {
            if let Some(r) = self.rows.next() {
                return Ok(Some(r));
            }
            let Some(file) = &mut self.file else {
                return Ok(None);
            };
            ctx.check()?;
            let mut chunk = vec![0u8; 64 * 1024];
            let n = std::io::Read::read(file, &mut chunk).map_err(|e| {
                crate::server_files::file_error(&e, &format!("could not read from COPY file \"{}\"", self.name))
            })?;
            if n == 0 {
                self.file = None;
            }
            self.buf.extend_from_slice(&chunk[..n]);
            let took = parse_copy_data(&self.copy, &mut self.st, &self.buf, self.file.is_none(), ctx.fmt(), self.enc, &mut |e| ctx.notice(e))?;
            self.buf.drain(..took);
            let (rows, skipped) = self.st.take_rows();
            self.skipped = skipped;
            self.rows = rows.into_iter();
        }
    }

    /// A record as the table's row (columns COPY doesn't fill are NULL), its DEFAULT
    /// markers' columns given their defaults.
    fn row(&self, rec: Vec<Expr>, defaults: &[Option<Expr>], ctx: &ExecCtx) -> Result<Row> {
        let t = &self.copy.table;
        let mut row = vec![Value::Null; t.columns.len()];
        for (e, &col) in rec.into_iter().zip(&self.copy.target) {
            row[col] = match e {
                Expr::Const(v, _) => v,
                Expr::Default(_) => match &defaults[col] {
                    Some(d) => eval(d, &[], ctx)?,
                    None => Value::Null,
                },
                e => eval(&e, &[], ctx)?,
            };
        }
        Ok(row)
    }

    /// fileEndForeignScan's NOTICE of the rows ON_ERROR ignore skipped.
    fn end(&self, ctx: &ExecCtx) {
        let n = self.skipped;
        if n > 0 && self.copy.format.log_verbosity >= 1 {
            let rows = if n == 1 { "row was" } else { "rows were" };
            ctx.notice(crate::session::notice(format!(
                "{n} {rows} skipped due to data type incompatibility"
            )));
        }
    }
}

/// file_acquire_sample_rows: every row of the file (ANALYZE's sample is taken from them).
pub fn read_all(t: &Table, ctx: &ExecCtx) -> Result<Vec<Row>> {
    let t = Arc::new(t.clone());
    let mut reader = begin(&t, ctx)?;
    let defaults = crate::sql::binder::table_rules(&t, ctx)?.defaults.clone();
    let mut out = Vec::new();
    while let Some(rec) = reader.next(ctx)? {
        out.push(reader.row(rec, &defaults, ctx)?);
    }
    Ok(out)
}

/// fileBeginForeignScan / fileIterateForeignScan: the file's rows, then the scan's filter.
pub fn open(s: &ScanPlan, need: Option<&[bool]>, ctx: &ExecCtx) -> Result<BoxNode> {
    let t = s.table.clone();
    // A foreign scan's tuples are virtual: of the system columns only ctid (invalid) and
    // tableoid can be read. (A mask asking for everything, as an inheritance tree's members
    // get, says nothing about which.)
    let sys = t.columns.len() + usize::from(s.with_key);
    if s.sys
        && need.is_some_and(|n| !n.iter().all(|b| *b) && (1..5).any(|i| n.get(sys + i) == Some(&true)))
    {
        return Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            "cannot retrieve a system column in this context",
        ));
    }
    let reader = begin(&t, ctx)?;
    let defaults = crate::sql::binder::table_rules(&t, ctx)?.defaults.clone();
    Ok(Box::new(FileScan {
        reader,
        defaults,
        filter: s.filter.clone(),
        with_key: s.with_key,
        sys: s.sys,
        key: s as *const ScanPlan as usize,
        n: 0,
        ended: false,
    }))
}

struct FileScan {
    reader: FileReader,
    defaults: Vec<Option<Expr>>,
    filter: Option<Expr>,
    with_key: bool,
    sys: bool,
    /// The scan's plan address (instrumentation's key).
    key: usize,
    /// Rows read.
    n: u64,
    ended: bool,
}

impl Node for FileScan {
    fn next(&mut self, ctx: &ExecCtx) -> Result<Option<Row>> {
        loop {
            let Some(rec) = self.reader.next(ctx)? else {
                if !self.ended {
                    self.ended = true;
                    self.reader.end(ctx);
                }
                return Ok(None);
            };
            let mut row = self.reader.row(rec, &self.defaults, ctx)?;
            self.n += 1;
            if self.with_key {
                row.push(Value::Bytea(Arc::from(&self.n.to_be_bytes()[..])));
            }
            if self.sys {
                let oid = self.reader.copy.table.oid;
                row.extend([Value::Tid(u32::MAX, 0), Value::Oid(0), Value::Oid(0), Value::Oid(0), Value::Oid(0), Value::Oid(oid)]);
            }
            if let Some(f) = &self.filter
                && !eval_bool(f, &row, ctx)?
            {
                ctx.filtered(self.key);
                continue;
            }
            return Ok(Some(row));
        }
    }
}
