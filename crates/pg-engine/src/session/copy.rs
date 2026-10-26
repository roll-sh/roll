//! COPY ... FROM STDIN / TO STDOUT in text, CSV and binary formats, and FROM / TO a server
//! file when the server allows them (`crate::server_files`).

use super::{Outcome, QueryResult, Session};
use crate::catalog::{Relation, Table};
use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::sql::binder::ast::{const_str, inner, names, with_pos};
use crate::sql::binder::{Binder, table_rules};
use crate::sql::expr::Expr;
use crate::sql::plan::{InsertPlan, Plan};
use crate::types::Value;
use crate::types::text::{format_value, parse_value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

#[derive(Clone, Debug)]
pub struct CopyFormat {
    pub csv: bool,
    pub binary: bool,
    pub delimiter: char,
    pub null: String,
    /// DEFAULT marker: the field takes the column's default.
    pub default: Option<String>,
    pub header: bool,
    /// HEADER MATCH: the header line must name the columns.
    pub header_match: bool,
    pub quote: char,
    pub escape: char,
    pub force_quote: Vec<String>,
    pub force_quote_all: bool,
    pub force_not_null: Vec<String>,
    pub force_not_null_all: bool,
    pub force_null: Vec<String>,
    pub force_null_all: bool,
    /// ON_ERROR ignore: rows with values that don't convert are skipped.
    pub on_error_ignore: bool,
    /// LOG_VERBOSITY: 0 silent, 1 default, 2 verbose.
    pub log_verbosity: u8,
    pub reject_limit: Option<i64>,
    /// FREEZE.
    pub freeze: bool,
    /// ENCODING: the data's encoding (None: the client's).
    pub encoding: Option<i32>,
}

/// A FORCE_* option's columns: `*` or a list.
fn force_columns(d: &pb::DefElem) -> Result<(Vec<String>, bool)> {
    match d.arg.as_deref().and_then(inner) {
        Some(NodeEnum::AStar(_)) => Ok((Vec::new(), true)),
        Some(NodeEnum::List(l)) => Ok((names(&l.items), false)),
        _ => Err(with_pos(
            Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!(
                    "argument to option \"{}\" must be a list of column names",
                    d.defname
                ),
            ),
            d.location,
        )),
    }
}

/// A boolean option value (no argument means true).
fn copy_bool(d: &pb::DefElem) -> Result<bool> {
    let Some(arg) = d.arg.as_deref().and_then(inner) else {
        return Ok(true);
    };
    let s = match arg {
        NodeEnum::Boolean(b) => return Ok(b.boolval),
        NodeEnum::Integer(i) if i.ival == 0 || i.ival == 1 => return Ok(i.ival == 1),
        NodeEnum::String(s) => s.sval.to_ascii_lowercase(),
        _ => String::new(),
    };
    match s.as_str() {
        "true" | "on" => Ok(true),
        "false" | "off" => Ok(false),
        _ => Err(Error::new(
            code::SYNTAX_ERROR,
            format!("{} requires a Boolean value", d.defname),
        )),
    }
}

impl CopyFormat {
    /// ProcessCopyOptions.
    pub(crate) fn parse(opts: &[pb::Node], is_from: bool) -> Result<CopyFormat> {
        let mut f = CopyFormat {
            csv: false,
            binary: false,
            delimiter: '\t',
            null: String::new(),
            default: None,
            header: false,
            header_match: false,
            quote: '"',
            escape: '"',
            force_quote: vec![],
            force_quote_all: false,
            force_not_null: vec![],
            force_not_null_all: false,
            force_null: vec![],
            force_null_all: false,
            on_error_ignore: false,
            log_verbosity: 1,
            reject_limit: None,
            freeze: false,
            encoding: None,
        };
        let (mut delim, mut null, mut quote, mut escape) = (None, None, None, None);
        let mut seen: Vec<&str> = Vec::new();
        let mut freeze = false;
        let mut on_error = false;
        for o in opts {
            let Some(NodeEnum::DefElem(d)) = inner(o) else {
                continue;
            };
            let name = d.defname.as_str();
            let key = match name {
                "force_quote"
                | "force_not_null"
                | "force_null"
                | "format"
                | "freeze"
                | "delimiter"
                | "null"
                | "default"
                | "header"
                | "quote"
                | "escape"
                | "encoding"
                | "on_error"
                | "log_verbosity"
                | "reject_limit"
                | "convert_selectively" => name,
                _ => {
                    return Err(with_pos(
                        Error::new(
                            code::SYNTAX_ERROR,
                            format!("option \"{name}\" not recognized"),
                        ),
                        d.location,
                    ));
                }
            };
            if seen.contains(&key) {
                return Err(with_pos(
                    Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
                    d.location,
                ));
            }
            seen.push(key);
            let arg = d.arg.as_deref();
            let sval = || arg.and_then(const_str).unwrap_or_default();
            match name {
                "format" => match sval().as_str() {
                    "csv" => f.csv = true,
                    "text" => {}
                    "binary" => f.binary = true,
                    other => {
                        return Err(with_pos(
                            Error::new(
                                code::INVALID_PARAMETER_VALUE,
                                format!("COPY format \"{other}\" not recognized"),
                            ),
                            d.location,
                        ));
                    }
                },
                "freeze" => freeze = copy_bool(d)?,
                "delimiter" => delim = Some(sval()),
                "null" => null = Some(sval()),
                "default" => f.default = Some(sval()),
                "header" => {
                    if arg.and_then(inner).is_some_and(|a| matches!(a, NodeEnum::String(s) if s.sval.eq_ignore_ascii_case("match"))) {
                        if !is_from {
                            return Err(Error::new(code::FEATURE_NOT_SUPPORTED, "cannot use \"match\" with HEADER in COPY TO"));
                        }
                        f.header = true;
                        f.header_match = true;
                    } else {
                        f.header = copy_bool(d).map_err(|_| Error::new(code::SYNTAX_ERROR, "header requires a Boolean value or \"match\""))?;
                    }
                }
                "quote" => quote = Some(sval()),
                "escape" => escape = Some(sval()),
                "force_quote" => (f.force_quote, f.force_quote_all) = force_columns(d)?,
                "force_not_null" => (f.force_not_null, f.force_not_null_all) = force_columns(d)?,
                "force_null" => (f.force_null, f.force_null_all) = force_columns(d)?,
                "encoding" => {
                    f.encoding = crate::types::encnames::encoding_id(&sval());
                    if f.encoding.is_none() {
                        return Err(with_pos(
                            Error::new(
                                code::INVALID_PARAMETER_VALUE,
                                "argument to option \"encoding\" must be a valid encoding name",
                            ),
                            d.location,
                        ));
                    }
                }
                "on_error" => {
                    if !is_from {
                        return Err(with_pos(
                            Error::new(
                                code::INVALID_PARAMETER_VALUE,
                                "COPY ON_ERROR cannot be used with COPY TO",
                            ),
                            d.location,
                        ));
                    }
                    match sval().to_ascii_lowercase().as_str() {
                        "stop" => {}
                        "ignore" => f.on_error_ignore = true,
                        other => {
                            return Err(with_pos(
                                Error::new(
                                    code::INVALID_PARAMETER_VALUE,
                                    format!("COPY ON_ERROR \"{other}\" not recognized"),
                                ),
                                d.location,
                            ));
                        }
                    }
                    on_error = true;
                }
                "log_verbosity" => {
                    f.log_verbosity = match sval().to_ascii_lowercase().as_str() {
                        "silent" => 0,
                        "default" => 1,
                        "verbose" => 2,
                        other => {
                            return Err(with_pos(
                                Error::new(
                                    code::INVALID_PARAMETER_VALUE,
                                    format!("COPY LOG_VERBOSITY \"{other}\" not recognized"),
                                ),
                                d.location,
                            ));
                        }
                    }
                }
                "reject_limit" => {
                    // defGetCopyRejectLimitOption: a string (file_fdw's options) is read as
                    // an int8.
                    let n = match arg.and_then(inner) {
                        None => {
                            return Err(Error::new(
                                code::SYNTAX_ERROR,
                                "reject_limit requires a numeric value",
                            ));
                        }
                        Some(NodeEnum::String(s)) => {
                            let ty = crate::types::DataType::Int8;
                            match parse_value(&s.sval, &ty, &Default::default())? {
                                Value::Int8(n) => n,
                                _ => 0,
                            }
                        }
                        Some(_) => arg.and_then(crate::sql::binder::ast::ival).unwrap_or(0),
                    };
                    if n <= 0 {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            format!("REJECT_LIMIT ({n}) must be greater than zero"),
                        ));
                    }
                    f.reject_limit = Some(n);
                }
                _ => {}
            }
        }
        // Incompatible options (checked before defaults are filled in).
        if f.binary && delim.is_some() {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "cannot specify DELIMITER in BINARY mode",
            ));
        }
        if f.binary && null.is_some() {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "cannot specify NULL in BINARY mode",
            ));
        }
        if f.binary && f.default.is_some() {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                "cannot specify DEFAULT in BINARY mode",
            ));
        }
        let delim = delim.unwrap_or_else(|| if f.csv { ",".into() } else { "\t".into() });
        f.null = null.unwrap_or_else(|| if f.csv { String::new() } else { "\\N".into() });
        let unsupported = |m: &str| Err(Error::new(code::FEATURE_NOT_SUPPORTED, m.to_string()));
        let invalid = |m: String| Err(Error::new(code::INVALID_PARAMETER_VALUE, m));
        if delim.len() != 1 {
            return unsupported("COPY delimiter must be a single one-byte character");
        }
        f.delimiter = delim.chars().next().unwrap();
        if matches!(f.delimiter, '\r' | '\n') {
            return invalid("COPY delimiter cannot be newline or carriage return".into());
        }
        if f.null.contains(['\r', '\n']) {
            return invalid(
                "COPY null representation cannot use newline or carriage return".into(),
            );
        }
        if f.default.as_ref().is_some_and(|d| d.contains(['\r', '\n'])) {
            return invalid(
                "COPY default representation cannot use newline or carriage return".into(),
            );
        }
        if !f.csv && "\\.abcdefghijklmnopqrstuvwxyz0123456789".contains(f.delimiter) {
            return invalid(format!("COPY delimiter cannot be \"{delim}\""));
        }
        if f.binary && f.header {
            return unsupported("cannot specify HEADER in BINARY mode");
        }
        if !f.csv && quote.is_some() {
            return unsupported("COPY QUOTE requires CSV mode");
        }
        if f.csv {
            let q = quote.unwrap_or_else(|| "\"".into());
            if q.len() != 1 {
                return unsupported("COPY quote must be a single one-byte character");
            }
            f.quote = q.chars().next().unwrap();
            if f.delimiter == f.quote {
                return invalid("COPY delimiter and quote must be different".into());
            }
        }
        if !f.csv && escape.is_some() {
            return unsupported("COPY ESCAPE requires CSV mode");
        }
        if f.csv {
            let e = escape.unwrap_or_else(|| f.quote.to_string());
            if e.len() != 1 {
                return unsupported("COPY escape must be a single one-byte character");
            }
            f.escape = e.chars().next().unwrap();
        }
        let forced = |list: &Vec<String>, all: bool| all || !list.is_empty();
        if !f.csv && forced(&f.force_quote, f.force_quote_all) {
            return unsupported("COPY FORCE_QUOTE requires CSV mode");
        }
        if forced(&f.force_quote, f.force_quote_all) && is_from {
            return unsupported("COPY FORCE_QUOTE cannot be used with COPY FROM");
        }
        if !f.csv && forced(&f.force_not_null, f.force_not_null_all) {
            return unsupported("COPY FORCE_NOT_NULL requires CSV mode");
        }
        if forced(&f.force_not_null, f.force_not_null_all) && !is_from {
            return invalid("COPY FORCE_NOT_NULL cannot be used with COPY TO".into());
        }
        if !f.csv && forced(&f.force_null, f.force_null_all) {
            return unsupported("COPY FORCE_NULL requires CSV mode");
        }
        if forced(&f.force_null, f.force_null_all) && !is_from {
            return invalid("COPY FORCE_NULL cannot be used with COPY TO".into());
        }
        if f.null.contains(f.delimiter) {
            return invalid(
                "COPY delimiter character must not appear in the NULL specification".into(),
            );
        }
        if f.csv && f.null.contains(f.quote) {
            return invalid("CSV quote character must not appear in the NULL specification".into());
        }
        if freeze && !is_from {
            return invalid("COPY FREEZE cannot be used with COPY TO".into());
        }
        if let Some(d) = &f.default {
            if !is_from {
                return unsupported("COPY DEFAULT cannot be used with COPY TO");
            }
            if d.contains(f.delimiter) {
                return invalid(
                    "COPY delimiter character must not appear in the DEFAULT specification".into(),
                );
            }
            if f.csv && d.contains(f.quote) {
                return invalid(
                    "CSV quote character must not appear in the DEFAULT specification".into(),
                );
            }
            if *d == f.null {
                return unsupported(
                    "NULL specification and DEFAULT specification cannot be the same",
                );
            }
        }
        if f.binary && f.on_error_ignore {
            return unsupported("only ON_ERROR STOP is allowed in BINARY mode");
        }
        if f.reject_limit.is_some() && !on_error {
            return invalid("COPY REJECT_LIMIT requires ON_ERROR to be set to IGNORE".into());
        }
        f.freeze = freeze;
        Ok(f)
    }

    /// FORCE_* columns must be among those copied.
    fn check_forced(&self, t: &Table, target: &[usize]) -> Result<()> {
        for (what, list) in [
            ("FORCE_QUOTE", &self.force_quote),
            ("FORCE_NOT_NULL", &self.force_not_null),
            ("FORCE_NULL", &self.force_null),
        ] {
            for c in list {
                let i = column_for_copy(t, c)?;
                if !target.contains(&i) {
                    return Err(Error::new(
                        code::INVALID_COLUMN_REFERENCE,
                        format!("{what} column \"{c}\" not referenced by COPY"),
                    ));
                }
            }
        }
        Ok(())
    }

    fn forces_not_null(&self, col: &str) -> bool {
        self.force_not_null_all || self.force_not_null.iter().any(|c| c == col)
    }

    fn forces_null(&self, col: &str) -> bool {
        self.force_null_all || self.force_null.iter().any(|c| c == col)
    }
}

/// A COPY FROM STDIN waiting for its data.
pub struct CopyIn {
    pub table: Arc<Table>,
    pub target: Vec<usize>,
    pub format: Box<CopyFormat>,
    pub ncols: usize,
    /// COPY FROM ... WHERE: over the table's row.
    pub filter: Option<Expr>,
    /// The size of the server file the data comes from (None: from the client).
    pub file_size: Option<i64>,
}

/// COPY FROM data read so far, a batch at a time (`Session::copy_in_run`).
#[derive(Default)]
pub struct CopyInParsed {
    rows: Vec<Vec<Expr>>,
    /// Each row's line number and text.
    lines: Vec<(usize, String)>,
    /// Rows ON_ERROR ignore skipped.
    skipped: usize,
    /// Records (lines; binary tuples) read.
    records: usize,
    /// Physical lines read (a CSV record may span several).
    phys_lines: usize,
    /// Bytes taken.
    bytes: usize,
    /// The end-of-copy marker or the binary trailer came: what follows is ignored.
    ended: bool,
    /// Binary: the header was read.
    header: bool,
    /// The target positions of NOT NULL columns, checked as rows are read (ExecConstraints
    /// runs on each row before it is buffered); None until known, empty when that can't
    /// be done early (BEFORE ROW triggers or a WHERE may change or drop the row).
    not_null: Option<Vec<usize>>,
    /// A NOT NULL column COPY doesn't give, with no default: every row fails.
    not_null_missing: bool,
}

/// Where the complete records at the start of COPY data end (0: none yet). A text record
/// ends at a newline, a CSV one at a newline outside quotes.
fn record_boundary(data: &[u8], f: &CopyFormat) -> usize {
    if !f.csv {
        return data.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    }
    let (Ok(q), Ok(esc)) = (u8::try_from(f.quote), u8::try_from(f.escape)) else {
        return 0;
    };
    let (mut in_quotes, mut end, mut i) = (false, 0, 0);
    while i < data.len() {
        let b = data[i];
        if in_quotes {
            // (As split_csv reads them: an escape before a quote or an escape stands for it.)
            if b == esc && data.get(i + 1).is_some_and(|&n| n == q || n == esc) {
                i += 2;
                continue;
            }
            if b == q {
                in_quotes = false;
            }
        } else if b == q {
            in_quotes = true;
        } else if b == b'\n' {
            end = i + 1;
        }
        i += 1;
    }
    end
}

/// COPY TO STDOUT output: one data message per line.
pub struct CopyOut {
    pub data: Vec<Vec<u8>>,
    pub ncols: usize,
    pub tag: String,
    pub binary: bool,
    /// Notices raised after the rows went out (the query's AFTER triggers, which fire at its
    /// end): they follow the data.
    pub trailing: Vec<Error>,
    /// An error making a row's data (its output function's, or the conversion to the
    /// client's encoding): Postgres sends it after CopyOutResponse and the rows before it.
    pub error: Option<Error>,
}

impl Session {
    pub(super) fn copy_stmt(&mut self, s: &pb::CopyStmt, params: &[Value]) -> Result<Outcome> {
        if let Some(NodeEnum::SelectStmt(sel)) = s.query.as_deref().and_then(inner)
            && crate::sql::binder::ast::select_into(sel).is_some()
        {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "COPY (SELECT INTO) is not supported",
            ));
        }
        // Server files are reachable only when the server allows them (and then by the roles
        // Postgres lets in), programs never: the errors are the ones Postgres gives a role
        // without pg_read_server_files / pg_write_server_files / pg_execute_server_program
        // (DoCopy).
        if s.is_program || !s.filename.is_empty() {
            let allowed = !s.is_program
                && self.state.cluster.server_files().is_some()
                && {
                    let cat = {
                        self.ensure_txn();
                        self.txn.as_ref().unwrap().borrow().catalog()
                    };
                    let role = if s.is_from {
                        crate::server_files::PG_READ_SERVER_FILES
                    } else {
                        crate::server_files::PG_WRITE_SERVER_FILES
                    };
                    let me = self.state.current_role(&cat);
                    cat.has_privs_of_role(me, role, self.state.db_owner(&cat))
                };
            let (msg, role, what) = if s.is_program {
                (
                    "permission denied to COPY to or from an external program",
                    "pg_execute_server_program",
                    "COPY to or from an external program",
                )
            } else if s.is_from {
                ("permission denied to COPY from a file", "pg_read_server_files", "COPY from a file")
            } else {
                ("permission denied to COPY to a file", "pg_write_server_files", "COPY to a file")
            };
            if !allowed {
                return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, msg)
                    .detail(format!("Only roles with privileges of the \"{role}\" role may {what}."))
                    .hint("Anyone can COPY to stdout or from stdin. psql's \\copy command also works for anyone."));
            }
        }
        let format = CopyFormat::parse(&s.options, s.is_from)?;
        let cols = names(&s.attlist);
        if s.is_from {
            let rv = s.relation.as_ref().unwrap();
            let cat = {
                self.ensure_txn();
                self.txn.as_ref().unwrap().borrow().catalog()
            };
            let schema = if rv.schemaname.is_empty() {
                None
            } else {
                Some(rv.schemaname.as_str())
            };
            let t = match cat.lookup(schema, &rv.relname, &self.state.search_path())? {
                Some(Relation::Table(t)) => {
                    // CopyFrom refuses FREEZE before it asks a foreign table's wrapper.
                    if format.freeze && (t.partkey.is_some() || t.is_foreign()) {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!(
                                "cannot perform COPY FREEZE on a {} table",
                                if t.is_foreign() { "foreign" } else { "partitioned" }
                            ),
                        ));
                    }
                    // A foreign table's wrapper would take the rows.
                    if let Some(e) = cat.fdw_routine_error(&t) {
                        return Err(e);
                    }
                    if format.freeze
                        && !self.txn.as_ref().unwrap().borrow().storage_new_here(t.oid)
                    {
                        return Err(Error::new(
                            code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                            "cannot perform COPY FREEZE because the table was not created or truncated in the current subtransaction",
                        ));
                    }
                    t
                }
                // A view with an INSTEAD OF INSERT row trigger takes the rows through it.
                Some(Relation::View(v))
                    if !v.materialized
                        && cat.triggers.values().any(|x| {
                            use crate::catalog::trigger_type as tt;
                            x.table == v.oid
                                && x.tgtype & tt::INSTEAD != 0
                                && x.tgtype & tt::INSERT != 0
                                && x.tgtype & tt::ROW != 0
                        }) =>
                {
                    v.as_table()
                }
                Some(r) => {
                    let e = Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("cannot copy to {} \"{}\"", r.kind_name(), rv.relname),
                    );
                    return Err(if matches!(r, Relation::View(_)) {
                        e.hint("To enable copying to a view, provide an INSTEAD OF INSERT trigger.")
                    } else {
                        e
                    });
                }
                None => return Err(crate::catalog::no_such_relation(schema, &rv.relname)),
            };
            let target = target_columns(&t, &cols)?;
            format.check_forced(&t, &target)?;
            let ncols = target.len();
            let perms = crate::sql::binder::perms::Perms {
                rels: vec![crate::sql::binder::perms::RelPerm {
                    oid: t.oid,
                    privs: crate::catalog::acl::INSERT,
                    insert_cols: target.iter().map(|c| *c as i16 + 1).collect(),
                    ..Default::default()
                }],
                funcs: Vec::new(),
                gen_funcs: Vec::new(),
                locks: vec![(t.oid, crate::txn::locks::LockMode::RowExclusive)],
                named_locks: 1,
            };
            let filter = self.with_ctx(params, |ctx| {
                crate::sql::binder::perms::check(&perms, ctx)?;
                if Binder::new(ctx, Vec::new()).rls_enabled(&t)? {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "COPY FROM not supported with row-level security",
                    )
                    .hint("Use INSERT statements instead."));
                }
                let Some(w) = s.where_clause.as_deref() else {
                    return Ok(None);
                };
                let mut b = Binder::new(ctx, Vec::new());
                let e = b.bind_over_table(&t, w, crate::sql::binder::Clause::CopyWhere)?;
                // Generated columns aren't computed yet when the condition is checked.
                let mut gen_col = None;
                e.walk(&mut |x| {
                    if let Expr::Col(i, _) = x
                        && gen_col.is_none()
                        && t.columns.get(*i).is_some_and(|c| c.generated.is_some())
                    {
                        gen_col = Some(*i);
                    }
                });
                let whole = crate::sql::binder::ast::mentions_whole_row(w, &t.name);
                if gen_col.is_none() && whole {
                    gen_col = t.columns.iter().position(|c| c.generated.is_some() && !c.dropped);
                }
                if let Some(i) = gen_col {
                    return Err(Error::new(
                        code::INVALID_COLUMN_REFERENCE,
                        "generated columns are not supported in COPY FROM WHERE conditions",
                    )
                    .detail(format!("Column \"{}\" is a generated column.", t.columns[i].name)));
                }
                Ok(Some(b.coerce_bool(e, "WHERE")?))
            })?;
            let mut cin = CopyIn {
                table: t,
                target,
                format: Box::new(format),
                ncols,
                filter,
                file_size: None,
            };
            if s.filename.is_empty() {
                return Ok(Outcome::CopyIn(cin));
            }
            // BeginCopyFrom: the file is read where the client's data would come.
            let data = self.read_copy_file(&s.filename)?;
            cin.file_size = Some(data.len() as i64);
            return self.copy_in_rows(&cin, &data).map(Outcome::Done);
        }
        // COPY TO: a query.
        let sql = match (&s.relation, &s.query) {
            (_, Some(q)) => {
                // Rules on the statement's relation must leave one query (BeginCopyTo).
                self.with_ctx(params, |ctx| {
                    Binder::new(ctx, Vec::new()).copy_rewritten(q).map(|_| ())
                })?;
                let returning = match inner(q) {
                    Some(NodeEnum::InsertStmt(i)) => {
                        Some(crate::sql::binder::ast::returning(&i.returning_clause))
                    }
                    Some(NodeEnum::UpdateStmt(u)) => {
                        Some(crate::sql::binder::ast::returning(&u.returning_clause))
                    }
                    Some(NodeEnum::DeleteStmt(d)) => {
                        Some(crate::sql::binder::ast::returning(&d.returning_clause))
                    }
                    Some(NodeEnum::MergeStmt(m)) => {
                        Some(crate::sql::binder::ast::returning(&m.returning_clause))
                    }
                    _ => None,
                };
                if returning.is_some_and(|r| r.is_empty()) {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        "COPY query must have a RETURNING clause",
                    ));
                }
                crate::sql::binder::ast::deparse_stmt(q)?
            }
            (Some(rv), None) => 'rel: {
                let cat = {
                    self.ensure_txn();
                    self.txn.as_ref().unwrap().borrow().catalog()
                };
                let schema = if rv.schemaname.is_empty() {
                    None
                } else {
                    Some(rv.schemaname.as_str())
                };
                // A system catalog (pg_dump --schema=pg_catalog copies them all).
                if cat.lookup(schema, &rv.relname, &self.state.search_path())?.is_none()
                    && let Some(st) = crate::catalog::system::SysTable::lookup(
                        schema,
                        &rv.relname,
                        &self.state.search_path(),
                    )
                {
                    if st.is_view() {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!("cannot copy from view \"{}\"", rv.relname),
                        )
                        .hint("Try the COPY (SELECT ...) TO variant."));
                    }
                    let all: Vec<String> = st.columns().iter().map(|c| c.0.clone()).collect();
                    if let Some(c) = cols.iter().find(|c| !all.contains(c)) {
                        return Err(Error::new(
                            code::UNDEFINED_COLUMN,
                            format!("column \"{c}\" of relation \"{}\" does not exist", rv.relname),
                        ));
                    }
                    let list = if cols.is_empty() { &all } else { &cols };
                    let list: Vec<String> = list.iter().map(|c| crate::catalog::quote_ident(c)).collect();
                    break 'rel format!(
                        "SELECT {} FROM pg_catalog.{}",
                        list.join(", "),
                        crate::catalog::quote_ident(st.name())
                    );
                }
                let t = match cat.lookup(schema, &rv.relname, &self.state.search_path())? {
                    Some(Relation::Table(t)) if t.partkey.is_some() || t.is_foreign() => {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!(
                                "cannot copy from {} \"{}\"",
                                if t.is_foreign() {
                                    "foreign table"
                                } else {
                                    "partitioned table"
                                },
                                t.name
                            ),
                        )
                        .hint("Try the COPY (SELECT ...) TO variant."));
                    }
                    Some(Relation::Table(t)) => t,
                    // A materialized view copies out its rows, once it has them.
                    Some(Relation::View(v)) if v.materialized => {
                        if !v.populated {
                            return Err(Error::new(
                                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                                format!(
                                    "cannot copy from unpopulated materialized view \"{}\"",
                                    v.name
                                ),
                            )
                            .hint("Use the REFRESH MATERIALIZED VIEW command."));
                        }
                        match &v.storage {
                            Some(t) => t.clone(),
                            None => return Err(crate::error::internal("materialized view without storage")),
                        }
                    }
                    Some(Relation::View(v)) => {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!("cannot copy from view \"{}\"", v.name),
                        )
                        .hint("Try the COPY (SELECT ...) TO variant."));
                    }
                    Some(r) => {
                        return Err(Error::new(
                            code::WRONG_OBJECT_TYPE,
                            format!("cannot copy from {} \"{}\"", r.kind_name(), rv.relname),
                        )
                        .hint("Try the COPY (SELECT ...) TO variant."));
                    }
                    None => return Err(crate::catalog::no_such_relation(schema, &rv.relname)),
                };
                let target = target_columns(&t, &cols)?;
                format.check_forced(&t, &target)?;
                // DoCopy checks the privileges before row-level security.
                let perms = crate::sql::binder::perms::Perms {
                    rels: vec![crate::sql::binder::perms::RelPerm {
                        oid: t.oid,
                        privs: crate::catalog::acl::SELECT,
                        select_cols: target.iter().map(|c| *c as i16 + 1).collect(),
                        ..Default::default()
                    }],
                    funcs: Vec::new(),
                    gen_funcs: Vec::new(),
                    locks: vec![(t.oid, crate::txn::locks::LockMode::AccessShare)],
                    named_locks: 1,
                };
                self.with_ctx(params, |ctx| crate::sql::binder::perms::check(&perms, ctx))?;
                let rel = if rv.schemaname.is_empty() {
                    crate::catalog::quote_ident(&rv.relname)
                } else {
                    format!(
                        "{}.{}",
                        crate::catalog::quote_ident(&rv.schemaname),
                        crate::catalog::quote_ident(&rv.relname)
                    )
                };
                let list = target
                    .iter()
                    .map(|&c| crate::catalog::quote_ident(&t.columns[c].name))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("SELECT {list} FROM ONLY {rel}")
            }
            _ => return Err(crate::error::internal("COPY without source")),
        };
        // A query as written (errors point into the statement).
        let stmt = match &s.query {
            Some(q) => (**q).clone(),
            None => crate::sql::parse::parse(&sql)?
                .into_iter()
                .next()
                .and_then(|s| s.stmt)
                .map(|b| *b)
                .unwrap(),
        };
        // The data goes to the client as it is made, CopyOutResponse first (DoCopyTo sends it
        // before the executor runs), or into the file; the query's notices interleave with
        // it, its AFTER triggers' (which fire at its end) follow it. Without a sink that
        // takes it (an embedder's session) the data is collected into `Outcome::CopyOut`,
        // the AFTER triggers' notices kept for after it (`trailing`).
        let to_file = !s.filename.is_empty();
        let mut sink = if to_file { None } else { self.row_sink.take().filter(|k| k.takes_copy()) };
        let collecting = sink.is_none() && !to_file;
        let held = self.state.notice_hold.get();
        if collecting {
            self.state.notice_hold.set(held + 1);
        }
        let enc = self.copy_encoding(&format);
        let fmt = self.state.settings.borrow().format_ctx();
        let run = self.with_ctx(params, |ctx| {
            let bq = Binder::new(ctx, Vec::new()).bind_statement(&stmt)?;
            bq.check_perms(ctx)?;
            // BeginCopyTo opens the file once the query is planned.
            let file = if to_file { Some(create_copy_file(ctx, &s.filename)?) } else { None };
            if s.query.is_some() {
                for c in &format.force_quote {
                    if !bq.cols.iter().any(|o| o.name == *c) {
                        return Err(Error::new(
                            code::UNDEFINED_COLUMN,
                            format!("column \"{c}\" does not exist"),
                        ));
                    }
                }
            }
            let mut lines = CopyLines::new(&format, &bq.cols, fmt, enc)?;
            let mut dest = match (file, sink.as_deref_mut()) {
                (Some((name, f)), _) => CopyDest::File(name, std::io::BufWriter::new(f)),
                (None, Some(k)) => {
                    k.copy_begin(format.binary, bq.cols.len())?;
                    CopyDest::Client(k)
                }
                (None, None) => CopyDest::Collect(Vec::new()),
            };
            let (mut n, mut mark) = (0usize, 0);
            let run = lines.header().and_then(|h| h.map_or(Ok(()), |h| dest.put(h))).and_then(|()| {
                // (One statement around both, so the query's own doesn't run the queue.)
                ctx.statement(|| {
                    crate::exec::trigger::batch(ctx, || {
                        crate::exec::run_each(&bq.plan, ctx, |r| {
                            // (A cancel while the data goes out stops it: `sink::Feed::row`.)
                            ctx.check()?;
                            n += 1;
                            dest.put(lines.row(&r)?)
                        })?;
                        if let Some(t) = lines.trailer() {
                            dest.put(t)?;
                        }
                        dest.end()?;
                        mark = ctx.notices.borrow().len();
                        Ok(())
                    })
                })
            });
            let after = if run.is_ok() { ctx.notices.borrow().len() - mark } else { 0 };
            // An error into a file, or to a client the data streams to, is the statement's.
            let failed = match run {
                // (An error into a file, or to a client the data streams to, is the
                // statement's.)
                Err(e) if !collecting => return Err(e),
                r => r.err(),
            };
            Ok((failed, dest.collected(), n, after, bq.cols.len()))
        });
        self.state.notice_hold.set(held);
        self.row_sink = self.row_sink.take().or(sink);
        let (failed, data, n, after, ncols) = run?;
        let tag = format!("COPY {n}");
        if !collecting {
            return Ok(Outcome::Done(QueryResult::command(tag)));
        }
        // An error running the query comes after CopyOutResponse and the rows before it.
        let trailing = self.notices.split_off(self.notices.len().saturating_sub(after));
        Ok(Outcome::CopyOut(CopyOut {
            data,
            ncols,
            tag: if failed.is_some() { String::new() } else { tag },
            binary: format.binary,
            trailing,
            error: failed,
        }))
    }

    /// BeginCopyFrom's file: its contents (relative names are in the server's files
    /// directory).
    fn read_copy_file(&self, name: &str) -> Result<Vec<u8>> {
        let Some(sf) = self.state.cluster.server_files() else {
            return Err(crate::error::internal("COPY from a file without server files"));
        };
        let mut f = open_copy_file(Some(sf), name)?;
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut data).map_err(|e| {
            crate::server_files::file_error(&e, &format!("could not read from COPY file \"{name}\""))
        })?;
        Ok(data)
    }

    /// The data of a COPY FROM STDIN, all of it at hand (an in-process client's).
    pub fn copy_in_finish(&mut self, c: CopyIn, data: &[u8]) -> Result<QueryResult> {
        let mut chunks = data.chunks(COPY_READ);
        self.copy_in_stream(c, &mut || Ok(chunks.next().map(|d| d.to_vec())))
    }

    /// COPY FROM, its data read as the insert takes it, as CopyFrom reads lines as the client
    /// sends them: `read` gives the next piece of the data (None: the end). The rows go in a
    /// batch at a time (CopyFrom's multi-insert buffer: COPY_TUPLES rows or COPY_BYTES of
    /// lines), each batch parsed whole first; a bad line fails the COPY before the rest of
    /// the data has come, and memory holds a batch, not the data.
    pub fn copy_in_stream(
        &mut self,
        c: CopyIn,
        read: &mut dyn FnMut() -> Result<Option<Vec<u8>>>,
    ) -> Result<QueryResult> {
        let r = self.copy_in_run(&c, read);
        if r.is_err() {
            self.on_error();
        }
        self.pgss_copy_done(&r);
        r
    }

    /// The encoding COPY data is in: the ENCODING option's, else the client's.
    fn copy_encoding(&self, f: &CopyFormat) -> i32 {
        f.encoding.unwrap_or_else(|| {
            let e = self.state.settings.borrow().get_raw("client_encoding");
            e.as_deref()
                .and_then(crate::types::encnames::encoding_id)
                .unwrap_or(crate::types::mbconv::UTF8)
        })
    }

    fn copy_in_rows(&mut self, c: &CopyIn, data: &[u8]) -> Result<QueryResult> {
        let mut chunks = data.chunks(COPY_READ);
        self.copy_in_run(c, &mut || Ok(chunks.next().map(|d| d.to_vec())))
    }

    /// COPY FROM's insert, fed a batch of rows at a time from `read`'s data.
    fn copy_in_run(
        &mut self,
        c: &CopyIn,
        read: &mut dyn FnMut() -> Result<Option<Vec<u8>>>,
    ) -> Result<QueryResult> {
        // CopyFrom's CheckValidResultRel, before any row is read: a foreign table's
        // wrapper must take rows.
        if c.table.is_foreign() {
            self.ensure_txn();
            let cat = self.txn.as_ref().unwrap().borrow().catalog();
            if let Some(e) = cat.fdw_modify_error(&c.table, "insert into") {
                return Err(e);
            }
        }
        let t = c.table.clone();
        let f = &c.format;
        let enc = self.copy_encoding(f);
        let processed = self.with_ctx(&[], |ctx| {
            let mut fmt = ctx.sess.settings.borrow().format_ctx();
            // (regclass, regtype, ... input looks names up: the transaction's catalog, the
            // path.)
            fmt.names = Some(Arc::new(crate::catalog::names::CatalogNames::new(
                ctx.txn.borrow().catalog(),
                ctx.sess.search_path(),
            )));
            let mut rd = CopyReader {
                c,
                st: CopyInParsed::default(),
                pending: Vec::new(),
                eof: false,
                fmt: &fmt,
                enc,
                read,
                excluded: 0,
                null_stop: false,
                batch_bytes: 0,
            };
            let rules = table_rules(&t, ctx)?;
            rd.st.not_null = Some(rd.not_null_columns(&rules, ctx));
            let target = rd.target();
            let ncols = target.len();
            // CopyFrom buffers rows unless BEFORE ROW triggers or volatile defaults
            // (other than nextval) need them one at a time.
            let volatile_default = rules.defaults.iter().enumerate().any(|(i, d)| {
                !target.contains(&i)
                    && d.as_ref().is_some_and(|d| {
                        let mut v = false;
                        d.walk(&mut |e| match e {
                            Expr::Call(f, ..) if f.volatility == 'v' && f.name != "nextval" => {
                                v = true
                            }
                            Expr::Subquery(_) => v = true,
                            _ => {}
                        });
                        v
                    })
            });
            let multi = !volatile_default
                && !crate::exec::trigger::any(
                    &rules.triggers,
                    true,
                    true,
                    crate::catalog::trigger_type::INSERT,
                    ctx,
                );
            // The defaults COPY may use are planned when it starts (BeginCopyFrom): those of
            // the columns not copied, and with DEFAULT, every column's.
            for (i, d) in rules.defaults.iter().enumerate() {
                if let Some(d) = d
                    && (f.default.is_some() || !target.contains(&i))
                    && t.columns[i].generated.is_none()
                {
                    crate::sql::binder::check_folds(d, ctx)?;
                }
            }
            let copy = Arc::new(crate::sql::plan::CopyLines {
                table: t.name.as_str().into(),
                multi,
                freeze: f.freeze,
                feed: Default::default(),
            });
            let plan = Arc::new(Plan::Insert(Box::new(InsertPlan {
                rules: rules.clone(),
                source: Arc::new(Plan::Values {
                    rows: Vec::new(),
                    ncols,
                    rti: Default::default(),
                }),
                target,
                on_conflict: None,
                returning: Vec::new(),
                old_new: None,
                overriding_system: true,
                overriding_user: false,
                sys: false,
                route: crate::sql::binder::partition_routing(&t, ctx)?,
                copy: Some(copy.clone()),
                alias: None,
            })));
            *ctx.sess.copy_progress.borrow_mut() = Some(super::state::CopyProgress {
                relid: t.oid,
                command: 1,
                // PROGRESS_COPY_TYPE_FILE (with the file's size) or _PIPE.
                kind: if c.file_size.is_some() { 1 } else { 3 },
                bytes_total: c.file_size.unwrap_or(0),
                ..Default::default()
            });
            let r = ctx.statement(|| {
                let mut node = crate::exec::nodes::open(&plan, ctx)?;
                loop {
                    match node.next(ctx) {
                        Ok(Some(_)) => {}
                        Ok(None) => return Ok(()),
                        Err(e) if e.code == crate::exec::dml::NEED_COPY_ROWS => {
                            rd.next_batch(&rules, &copy, ctx)?
                        }
                        Err(e) => return Err(e),
                    }
                }
            });
            *ctx.sess.copy_progress.borrow_mut() = None;
            r?;
            if rd.null_stop {
                return Err(crate::error::internal(
                    "COPY row with a null in a NOT NULL column went in",
                ));
            }
            // Through a view's INSTEAD OF trigger every row counts (CopyFrom's processed).
            if ctx.catalog().views.contains_key(&t.oid) {
                return Ok(copy.feed.lock().unwrap().fed as u64);
            }
            Ok(ctx.processed.get())
        })?;
        Ok(QueryResult::command(format!("COPY {processed}")))
    }
}

/// COPY FROM's data, read and parsed a batch at a time (`Session::copy_in_run`).
struct CopyReader<'a> {
    c: &'a CopyIn,
    st: CopyInParsed,
    /// Data read and not yet parsed (a record's start).
    pending: Vec<u8>,
    eof: bool,
    fmt: &'a crate::types::text::FormatCtx,
    enc: i32,
    read: &'a mut dyn FnMut() -> Result<Option<Vec<u8>>>,
    /// Rows WHERE left out.
    excluded: i64,
    /// A row a NOT NULL column refuses was read: it is the last to go in, and fails.
    null_stop: bool,
    /// The bytes of the lines in the batch being read.
    batch_bytes: usize,
}

impl CopyReader<'_> {
    /// The columns the rows going in give: the copied ones, or with WHERE every column
    /// (the rows that pass carry them all).
    fn target(&self) -> Vec<usize> {
        if self.c.filter.is_none() {
            return self.c.target.clone();
        }
        self.c
            .table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, col)| !col.dropped && col.generated.is_none())
            .map(|(i, _)| i)
            .collect()
    }

    /// The target positions of NOT NULL columns, checked as rows are read (ExecConstraints
    /// runs on each row before it is buffered); empty when that can't be done early
    /// (BEFORE ROW triggers or a WHERE may change or drop the row).
    fn not_null_columns(&mut self, rules: &crate::sql::plan::TableRules, ctx: &ExecCtx) -> Vec<usize> {
        let (c, t) = (self.c, &self.c.table);
        let early = c.filter.is_none()
            && t.partkey.is_none()
            && !crate::exec::trigger::any(
                &rules.triggers,
                true,
                true,
                crate::catalog::trigger_type::INSERT,
                ctx,
            );
        let mut cols = Vec::new();
        if early {
            for (k, &col) in c.target.iter().enumerate() {
                let tc = &t.columns[col];
                if tc.not_null && tc.domain.is_none() {
                    cols.push(k);
                }
            }
            self.st.not_null_missing = t.columns.iter().enumerate().any(|(i, tc)| {
                tc.not_null
                    && !tc.dropped
                    && !c.target.contains(&i)
                    && tc.default.is_none()
                    && tc.identity.is_none()
                    && tc.generated.is_none()
                    && tc.domain.is_none()
            });
        }
        cols
    }

    /// The first row from `from` on with a NULL a NOT NULL column refuses.
    fn null_violation(&self, from: usize) -> Option<usize> {
        let st = &self.st;
        if st.not_null_missing && st.rows.len() > from {
            return Some(from);
        }
        let cols = st.not_null.as_ref()?;
        (from..st.rows.len()).find(|&r| {
            cols.iter()
                .any(|&k| matches!(st.rows[r].get(k), Some(Expr::Const(Value::Null, _))))
        })
    }

    fn batch_full(&self) -> bool {
        self.st.rows.len() >= crate::exec::sizing::COPY_TUPLES
            || self.batch_bytes >= crate::exec::sizing::COPY_BYTES
    }

    /// Read and parse the next batch of rows into the feed (marking it done at the end of
    /// the data).
    fn next_batch(
        &mut self,
        rules: &crate::sql::plan::TableRules,
        copy: &crate::sql::plan::CopyLines,
        ctx: &ExecCtx,
    ) -> Result<()> {
        while !self.eof && !self.null_stop && !self.batch_full() {
            let before = self.st.rows.len();
            let mut notices = Vec::new();
            match (self.read)()? {
                Some(chunk) => {
                    self.pending.extend_from_slice(&chunk);
                    let took = parse_copy_data(
                        self.c,
                        &mut self.st,
                        &self.pending,
                        false,
                        self.fmt,
                        self.enc,
                        &mut |n| notices.push(n),
                    )?;
                    self.pending.drain(..took);
                }
                None => {
                    let data = std::mem::take(&mut self.pending);
                    parse_copy_data(
                        self.c,
                        &mut self.st,
                        &data,
                        true,
                        self.fmt,
                        self.enc,
                        &mut |n| notices.push(n),
                    )?;
                    self.eof = true;
                }
            }
            for n in notices {
                ctx.notice(n);
            }
            self.batch_bytes += self.st.lines[before..].iter().map(|l| l.1.len()).sum::<usize>();
            // A row a NOT NULL column refuses fails the COPY as it is read: the rows up to it
            // go in, and its own error is the one inserting them raises.
            if let Some(k) = self.null_violation(before) {
                self.st.rows.truncate(k + 1);
                self.st.lines.truncate(k + 1);
                self.null_stop = true;
            }
        }
        let rows = std::mem::take(&mut self.st.rows);
        let lines = std::mem::take(&mut self.st.lines);
        self.batch_bytes = 0;
        let (rows, lines) = self.prepare(rows, lines, rules, ctx)?;
        if let Some(p) = ctx.sess.copy_progress.borrow_mut().as_mut() {
            p.bytes_processed = self.st.bytes as i64;
            p.tuples_excluded = self.excluded;
            p.tuples_skipped = self.st.skipped as i64;
        }
        let mut feed = copy.feed.lock().unwrap();
        feed.fed += rows.len();
        feed.rows
            .extend(rows.into_iter().zip(lines).map(|(r, (line, raw))| (r, line, raw)));
        if self.eof || self.null_stop {
            feed.done = true;
            let skipped = self.st.skipped;
            feed.skipped_notice = (skipped > 0 && self.c.format.log_verbosity >= 1).then(|| {
                if skipped == 1 {
                    "1 row was skipped due to data type incompatibility".to_string()
                } else {
                    format!("{skipped} rows were skipped due to data type incompatibility")
                }
            });
        }
        Ok(())
    }

    /// A batch's rows as they go in: WHERE (each row, with the defaults of the columns not
    /// copied, checked before it goes in), the domains' input checks, DEFAULT markers.
    #[allow(clippy::type_complexity)]
    fn prepare(
        &mut self,
        mut rows: Vec<Vec<Expr>>,
        mut lines: Vec<(usize, String)>,
        rules: &crate::sql::plan::TableRules,
        ctx: &ExecCtx,
    ) -> Result<(Vec<Vec<Expr>>, Vec<(usize, String)>)> {
        let (c, t, fmt) = (self.c, &self.c.table, self.fmt);
        let f = &c.format;
        let target = &c.target;
        if let Some(filter) = &c.filter {
            let all = self.target();
            let mut kept = Vec::with_capacity(rows.len());
            let mut kept_lines = Vec::with_capacity(lines.len());
            for (row, line) in rows.into_iter().zip(lines) {
                let mut full = vec![Value::Null; t.columns.len()];
                for (e, &col) in row.iter().zip(target) {
                    full[col] = match e {
                        Expr::Default(_) => match &rules.defaults[col] {
                            Some(d) => crate::sql::expr::eval(d, &[], ctx)?,
                            None => Value::Null,
                        },
                        e => crate::sql::expr::eval(e, &[], ctx)?,
                    };
                }
                for &col in &all {
                    if !target.contains(&col)
                        && let Some(d) = &rules.defaults[col]
                    {
                        full[col] = crate::sql::expr::eval(d, &[], ctx)?;
                    }
                }
                if !crate::sql::expr::eval_bool(filter, &full, ctx)? {
                    self.excluded += 1;
                    continue;
                }
                kept.push(
                    all.iter()
                        .map(|&i| Expr::Const(full[i].clone(), t.columns[i].ty.clone()))
                        .collect::<Vec<_>>(),
                );
                kept_lines.push(line);
            }
            // (The rows that pass carry every column, their values computed.)
            return Ok((kept, kept_lines));
        }
        // A domain's input function checks each value (NULL too) as the line is read.
        let mut keep = vec![true; rows.len()];
        'rows: for (k, (row, (line, _))) in rows.iter().zip(&lines).enumerate() {
            for (e, &col) in row.iter().zip(target) {
                let (Expr::Const(v, ty), Some(Some(d))) = (e, rules.domain_checks.get(col)) else {
                    continue;
                };
                if let Err(err) = d.check(v, ctx) {
                    let colname = &t.columns[col].name;
                    let shown = if v.is_null() {
                        "null input".to_string()
                    } else {
                        format!("\"{}\"", format_value(v, ty, fmt))
                    };
                    // ON_ERROR ignore: the domain's input function failed softly.
                    if f.on_error_ignore {
                        self.st.skipped += 1;
                        if let Some(limit) = f.reject_limit
                            && self.st.skipped as i64 > limit
                        {
                            return Err(Error::new(code::INVALID_TEXT_REPRESENTATION, format!("skipped more than REJECT_LIMIT ({limit}) rows due to data type incompatibility"))
                                .context(format!("COPY {}, line {line}, column {colname}: {shown}", t.name)));
                        }
                        if f.log_verbosity >= 2 {
                            ctx.notice(
                                super::notice(format!("skipping row due to data type incompatibility at line {line} for column \"{colname}\": {shown}")).context(format!("COPY {}", t.name)),
                            );
                        }
                        keep[k] = false;
                        continue 'rows;
                    }
                    return Err(err.context(format!(
                        "COPY {}, line {line}, column {colname}: {shown}",
                        t.name
                    )));
                }
            }
        }
        if keep.contains(&false) {
            let mut it = keep.iter();
            rows.retain(|_| *it.next().unwrap());
            let mut it = keep.iter();
            lines.retain(|_| *it.next().unwrap());
        }
        // DEFAULT markers take the column default.
        for row in &mut rows {
            for (e, &col) in row.iter_mut().zip(target) {
                if let Expr::Default(ty) = e {
                    *e = rules.defaults[col]
                        .clone()
                        .unwrap_or_else(|| Expr::null(ty.clone()));
                }
            }
        }
        Ok((rows, lines))
    }
}

/// How much of COPY data at hand goes to the parser at a time.
const COPY_READ: usize = 64 << 10;

/// NextCopyFrom over COPY data: the complete records at the start of `data` (every one when
/// `last`), in encoding `enc`, read into `st`; returns the bytes taken. `notice` gets ON_ERROR
/// ignore's verbose notices.
pub(crate) fn parse_copy_data(
    c: &CopyIn,
    st: &mut CopyInParsed,
    data: &[u8],
    last: bool,
    fmt: &crate::types::text::FormatCtx,
    enc: i32,
    notice: &mut dyn FnMut(Error),
) -> Result<usize> {
    // (After the end-of-copy marker or the binary trailer the rest is ignored.)
    if st.ended {
        st.bytes += data.len();
        return Ok(data.len());
    }
    let f = &c.format;
    if f.binary {
        let (rows, took, ended) = binary_copy_in(data, c, fmt, st, last)?;
        for row in rows {
            st.records += 1;
            st.rows.push(row);
            st.lines.push((st.records, String::new()));
        }
        st.ended = ended;
        st.bytes += took;
        return Ok(took);
    }
    let take = if last {
        data.len()
    } else {
        record_boundary(data, f)
    };
    if take == 0 {
        return Ok(0);
    }
    let data = &data[..take];
    st.bytes += take;
    // Data in another encoding is converted line by line, errors naming the line.
    let converted;
    let server = fmt.server_encoding;
    let data = if (enc, server) != (crate::types::mbconv::UTF8, crate::types::mbconv::UTF8) {
        let mut out = Vec::with_capacity(data.len());
        for (i, line) in data.split_inclusive(|b| *b == b'\n').enumerate() {
            let u = crate::types::mbconv::server::from_client(line, enc, server).map_err(|e| {
                e.context(format!("COPY {}, line {}", c.table.name, st.phys_lines + i + 1))
            })?;
            out.extend_from_slice(u.as_bytes());
        }
        converted = out;
        &converted[..]
    } else {
        data
    };
    st.phys_lines += data.iter().filter(|b| **b == b'\n').count();
    let text = std::str::from_utf8(data).map_err(|_| {
        Error::new(
            code::CHARACTER_NOT_IN_REPERTOIRE,
            "invalid byte sequence for encoding \"UTF8\"",
        )
    })?;
    let t = c.table.clone();
    let records = if f.csv {
        split_csv(text, f)?
    } else {
        let (records, ended) = split_text(text, f, &t.name, st.records)?;
        st.ended = ended;
        records
    };
    let first = st.records;
    st.records += records.len();
    st.rows.reserve(records.len());
    st.lines.reserve(records.len());
    let CopyInParsed {
        rows,
        lines,
        skipped,
        ..
    } = st;
    let ctx_line = |line: usize, raw: &str| format!("COPY {}, line {line}: \"{raw}\"", t.name);
    'records: for (lineno, (raw, rec)) in records.into_iter().enumerate() {
        let lineno = first + lineno;
        let line = lineno + 1;
        if lineno == 0 && f.header {
            if f.header_match {
                if rec.len() != c.ncols {
                    return Err(Error::new(
                        code::BAD_COPY_FILE_FORMAT,
                        format!(
                            "wrong number of fields in header line: got {}, expected {}",
                            rec.len(),
                            c.ncols
                        ),
                    )
                    .context(ctx_line(line, &raw)));
                }
                for (n, (field, &col)) in rec.iter().zip(&c.target).enumerate() {
                    let want = &t.columns[col].name;
                    if field.is_null(f) {
                        return Err(Error::new(code::BAD_COPY_FILE_FORMAT, format!("column name mismatch in header line field {}: got null value (\"{}\"), expected \"{want}\"", n + 1, f.null))
                            .context(ctx_line(line, &raw)));
                    }
                    if field.value != *want {
                        return Err(Error::new(code::BAD_COPY_FILE_FORMAT, format!("column name mismatch in header line field {}: got \"{}\", expected \"{want}\"", n + 1, field.value))
                            .context(ctx_line(line, &raw)));
                    }
                }
            }
            continue;
        }
        // (NextCopyFrom: with no columns to fill, a line's fields are ignored.)
        if c.ncols > 0 && rec.len() > c.ncols {
            return Err(Error::new(
                code::BAD_COPY_FILE_FORMAT,
                "extra data after last expected column",
            )
            .context(ctx_line(line, &raw)));
        }
        // (A missing field is found after the ones before it converted: NextCopyFrom.)
        let missing = (rec.len() < c.ncols).then_some(rec.len());
        let mut row = Vec::with_capacity(c.ncols);
        for (field, &col) in rec.into_iter().zip(&c.target) {
            let colname = &t.columns[col].name;
            let ty = &t.columns[col].ty;
            if field.is_default(f) {
                let c = &t.columns[col];
                if c.default.is_none() && c.identity.is_none() && c.domain.is_none() {
                    return Err(Error::new(
                        code::BAD_COPY_FILE_FORMAT,
                        "unexpected default marker in COPY data",
                    )
                    .detail(format!("Column \"{colname}\" has no default value."))
                    .context(ctx_line(line, &raw)));
                }
                row.push(Expr::Default(ty.clone()));
                continue;
            }
            let mut value = if field.is_null(f) {
                None
            } else {
                Some(field.value)
            };
            if f.csv {
                if value.is_none() && f.forces_not_null(colname) {
                    value = Some(f.null.clone());
                } else if value.as_ref().is_some_and(|v| *v == f.null) && f.forces_null(colname)
                {
                    value = None;
                }
            }
            let v = match value {
                None => Value::Null,
                Some(s) => match parse_value(&s, ty, fmt) {
                    Ok(v) => v,
                    Err(_) if f.on_error_ignore => {
                        *skipped += 1;
                        if let Some(limit) = f.reject_limit
                            && *skipped as i64 > limit
                        {
                            return Err(Error::new(code::INVALID_TEXT_REPRESENTATION, format!("skipped more than REJECT_LIMIT ({limit}) rows due to data type incompatibility"))
                                .context(format!("COPY {}, line {line}, column {colname}: \"{s}\"", t.name)));
                        }
                        if f.log_verbosity >= 2 {
                            notice(
                                super::notice(format!("skipping row due to data type incompatibility at line {line} for column \"{colname}\": \"{s}\"")).context(format!("COPY {}", t.name)),
                            );
                        }
                        continue 'records;
                    }
                    Err(e) => {
                        return Err(e.context(format!(
                            "COPY {}, line {line}, column {colname}: \"{s}\"",
                            t.name
                        )));
                    }
                },
            };
            row.push(Expr::Const(v, ty.clone()));
        }
        if let Some(n) = missing {
            let col = &t.columns[c.target[n]].name;
            return Err(Error::new(
                code::BAD_COPY_FILE_FORMAT,
                format!("missing data for column \"{col}\""),
            )
            .context(ctx_line(line, &raw)));
        }
        rows.push(row);
        lines.push((line, raw));
    }
    Ok(take)
}

/// BeginCopyFrom's file, opened: relative names are in the server's files directory (`sf`),
/// errors are PostgreSQL's for a file it can't open (or a directory). Without server files
/// no file can be opened.
pub(crate) fn open_copy_file(
    sf: Option<&crate::server_files::ServerFiles>,
    name: &str,
) -> Result<std::fs::File> {
    let open_error = |e: std::io::Error| {
        let err = crate::server_files::file_error(
            &e,
            &format!("could not open file \"{name}\" for reading"),
        );
        if crate::server_files::is_enoent_or_eacces(&e) {
            err.hint("COPY FROM instructs the PostgreSQL server process to read a file. You may want a client-side facility such as psql's \\copy.")
        } else {
            err
        }
    };
    let Some(sf) = sf else {
        return Err(open_error(std::io::ErrorKind::PermissionDenied.into()));
    };
    let path = sf.resolve(name)?;
    let f = std::fs::File::open(&path).map_err(open_error)?;
    let m = f.metadata().map_err(|e| {
        crate::server_files::file_error(&e, &format!("could not stat file \"{name}\""))
    })?;
    if m.is_dir() {
        return Err(Error::new(
            code::WRONG_OBJECT_TYPE,
            format!("\"{name}\" is a directory"),
        ));
    }
    Ok(f)
}

impl CopyInParsed {
    /// The rows read so far (each the copied columns' values, or `Expr::Default` for a
    /// DEFAULT marker), taken; ON_ERROR ignore's count of rows skipped.
    pub(crate) fn take_rows(&mut self) -> (Vec<Vec<Expr>>, usize) {
        self.lines.clear();
        (std::mem::take(&mut self.rows), self.skipped)
    }
}

/// BeginCopyTo's file: an absolute path (in the server's files directory), created or
/// truncated.
fn create_copy_file(ctx: &crate::exec::ExecCtx, name: &str) -> Result<(String, std::fs::File)> {
    let Some(sf) = ctx.sess.cluster.server_files() else {
        return Err(crate::error::internal("COPY to a file without server files"));
    };
    // Too easy to overwrite a database file otherwise.
    if !name.starts_with('/') {
        return Err(Error::new(
            code::INVALID_NAME,
            "relative path not allowed for COPY to file",
        ));
    }
    let path = sf.resolve(name)?;
    let f = std::fs::File::create(&path).map_err(|e| {
        let err = crate::server_files::file_error(
            &e,
            &format!("could not open file \"{name}\" for writing"),
        );
        if crate::server_files::is_enoent_or_eacces(&e) {
            err.hint("COPY TO instructs the PostgreSQL server process to write a file. You may want a client-side facility such as psql's \\copy.")
        } else {
            err
        }
    })?;
    let m = f.metadata().map_err(|e| {
        crate::server_files::file_error(&e, &format!("could not stat file \"{name}\""))
    })?;
    if m.is_dir() {
        return Err(Error::new(
            code::WRONG_OBJECT_TYPE,
            format!("\"{name}\" is a directory"),
        ));
    }
    Ok((name.to_string(), f))
}

/// One input field: its value, plus the raw text when it can match the
/// NULL or DEFAULT marker (unquoted).
struct InField {
    value: String,
    raw: Option<String>,
}

impl InField {
    fn is_null(&self, f: &CopyFormat) -> bool {
        self.raw.as_deref() == Some(f.null.as_str())
    }

    fn is_default(&self, f: &CopyFormat) -> bool {
        f.default.is_some() && self.raw.as_deref() == f.default.as_deref()
    }
}

type Records = Vec<(String, Vec<InField>)>;

/// Columns a COPY reads or writes; generated columns can't be listed.
fn target_columns(t: &Table, cols: &[String]) -> Result<Vec<usize>> {
    if cols.is_empty() {
        return Ok(t
            .live_columns()
            .filter(|(_, c)| c.generated.is_none())
            .map(|(i, _)| i)
            .collect());
    }
    let mut out = Vec::new();
    for c in cols {
        let i = column_for_copy(t, c)?;
        if out.contains(&i) {
            return Err(Error::new(
                code::DUPLICATE_COLUMN,
                format!("column \"{c}\" specified more than once"),
            ));
        }
        out.push(i);
    }
    Ok(out)
}

/// A column named in a COPY column or FORCE_* list.
fn column_for_copy(t: &Table, c: &str) -> Result<usize> {
    let i = t.column_index(c).ok_or_else(|| {
        Error::new(
            code::UNDEFINED_COLUMN,
            format!("column \"{c}\" of relation \"{}\" does not exist", t.name),
        )
    })?;
    if t.columns[i].generated.is_some() {
        return Err(Error::new(
            code::INVALID_COLUMN_REFERENCE,
            format!("column \"{c}\" is a generated column"),
        )
        .detail("Generated columns cannot be used in COPY."));
    }
    Ok(i)
}

/// Text format escaping.
fn text_escape(s: &str, delim: char) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\u{b}' => out.push_str("\\v"),
            c if c == delim => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// CopyAttributeOutCSV; `single`: the row's only column (where `\.` alone on its line
/// would read as the end-of-data marker).
fn csv_field(s: &str, f: &CopyFormat, force: bool, single: bool) -> String {
    let needs = force
        || s.is_empty() && !f.null.is_empty()
        || s == f.null
        || s.contains(f.delimiter)
        || s.contains(f.quote)
        || s.contains('\n')
        || s.contains('\r')
        || single && s == "\\.";
    if !needs {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push(f.quote);
    for ch in s.chars() {
        if ch == f.quote || ch == f.escape {
            out.push(f.escape);
        }
        out.push(ch);
    }
    out.push(f.quote);
    out
}

/// Split text-format COPY data into records: the raw line and its fields; and whether the
/// end-of-copy marker came. `first`: the lines before it.
fn split_text(data: &str, f: &CopyFormat, table: &str, first: usize) -> Result<(Records, bool)> {
    let mut out = Vec::new();
    // (An empty line is a record of one empty field; no data at all is none.)
    if data.is_empty() {
        return Ok((out, false));
    }
    let data = data.strip_suffix('\n').unwrap_or(data);
    for (lineno, line) in data.split('\n').enumerate() {
        let lineno = first + lineno;
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == "\\." {
            return Ok((out, true));
        }
        let mut fields = Vec::new();
        let mut raw = String::new();
        let mut cur = String::new();
        let mut chars = line.chars().peekable();
        let push = |cur: &mut String, raw: &mut String, fields: &mut Vec<InField>| {
            fields.push(InField {
                value: std::mem::take(cur),
                raw: Some(std::mem::take(raw)),
            });
        };
        while let Some(ch) = chars.next() {
            if ch == f.delimiter {
                push(&mut cur, &mut raw, &mut fields);
                continue;
            }
            if ch == '\\' {
                raw.push('\\');
                let Some(n) = chars.next() else { break };
                raw.push(n);
                match n {
                    // The end-of-copy marker anywhere but alone on its line.
                    '.' => {
                        return Err(Error::new(
                            code::BAD_COPY_FILE_FORMAT,
                            "end-of-copy marker is not alone on its line",
                        )
                        .context(format!("COPY {table}, line {}", lineno + 1)));
                    }
                    'n' => cur.push('\n'),
                    'r' => cur.push('\r'),
                    't' => cur.push('\t'),
                    'b' => cur.push('\u{8}'),
                    'f' => cur.push('\u{c}'),
                    'v' => cur.push('\u{b}'),
                    'x' if chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) => {
                        let mut v = 0u32;
                        let mut k = 0;
                        while k < 2 && chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                            let c = chars.next().unwrap();
                            raw.push(c);
                            v = v * 16 + c.to_digit(16).unwrap();
                            k += 1;
                        }
                        cur.push(char::from_u32(v).unwrap_or('?'));
                    }
                    '0'..='7' => {
                        let mut v = n.to_digit(8).unwrap();
                        let mut k = 1;
                        while k < 3 && chars.peek().is_some_and(|c| ('0'..='7').contains(c)) {
                            let c = chars.next().unwrap();
                            raw.push(c);
                            v = v * 8 + c.to_digit(8).unwrap();
                            k += 1;
                        }
                        cur.push(char::from_u32(v).unwrap_or('?'));
                    }
                    c => cur.push(c),
                }
                continue;
            }
            raw.push(ch);
            cur.push(ch);
        }
        push(&mut cur, &mut raw, &mut fields);
        out.push((line.to_string(), fields));
    }
    Ok((out, false))
}

/// Split CSV data (quoted fields may span lines).
fn split_csv(data: &str, f: &CopyFormat) -> Result<Records> {
    let mut out = Vec::new();
    let mut fields: Vec<InField> = Vec::new();
    let mut cur = String::new();
    let mut line = String::new();
    let mut quoted = false;
    let mut in_quotes = false;
    let mut chars = data.chars().peekable();
    let finish_field = |cur: &mut String, quoted: &mut bool, fields: &mut Vec<InField>| {
        let value = std::mem::take(cur);
        let raw = if *quoted { None } else { Some(value.clone()) };
        fields.push(InField { value, raw });
        *quoted = false;
    };
    while let Some(ch) = chars.next() {
        if in_quotes {
            line.push(ch);
            // An escape before a quote or another escape stands for that character
            // (CopyReadAttributesCSV).
            if ch == f.escape && chars.peek().is_some_and(|&n| n == f.quote || n == f.escape) {
                let q = chars.next().unwrap();
                line.push(q);
                cur.push(q);
            } else if ch == f.quote {
                if f.escape == f.quote && chars.peek() == Some(&f.quote) {
                    line.push(chars.next().unwrap());
                    cur.push(f.quote);
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(ch);
            }
            continue;
        }
        // (CSV data has no end-of-copy marker: `\.` is a value.)
        match ch {
            c if c == f.quote => {
                line.push(c);
                in_quotes = true;
                quoted = true;
            }
            c if c == f.delimiter => {
                line.push(c);
                finish_field(&mut cur, &mut quoted, &mut fields);
            }
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => {
                finish_field(&mut cur, &mut quoted, &mut fields);
                out.push((std::mem::take(&mut line), std::mem::take(&mut fields)));
            }
            c => {
                line.push(c);
                cur.push(c);
            }
        }
    }
    if in_quotes {
        return Err(Error::new(
            code::BAD_COPY_FILE_FORMAT,
            "unterminated CSV quoted field",
        ));
    }
    if !cur.is_empty() || !fields.is_empty() || quoted {
        finish_field(&mut cur, &mut quoted, &mut fields);
        out.push((line, fields));
    }
    Ok(out)
}

/// COPY TO in binary format: the signature, flags and extension area, then each row as a
/// field count and length-prefixed fields (typsend), then the trailer.
/// COPY TO's data, a CopyData message (a line) at a time: the header, each row's, the binary
/// trailer; text in the target encoding (CopyToTextLikeOneRow, CopySendEndOfRow).
struct CopyLines<'a> {
    format: &'a CopyFormat,
    cols: &'a [crate::sql::plan::OutCol],
    fmt: crate::types::text::FormatCtx,
    enc: i32,
    /// The binary signature, not sent yet: it goes out with the first row (or the trailer).
    signature: Option<Vec<u8>>,
}

impl<'a> CopyLines<'a> {
    fn new(
        format: &'a CopyFormat,
        cols: &'a [crate::sql::plan::OutCol],
        fmt: crate::types::text::FormatCtx,
        enc: i32,
    ) -> Result<Self> {
        let mut signature = None;
        if format.binary {
            for c in cols {
                if !crate::types::binary::supports_binary(&c.ty) {
                    return Err(Error::new(
                        code::UNDEFINED_FUNCTION,
                        format!("no binary output function available for type {}", c.ty.display()),
                    ));
                }
            }
            let mut head = b"PGCOPY\n\xff\r\n\0".to_vec();
            head.extend_from_slice(&0i32.to_be_bytes());
            head.extend_from_slice(&0i32.to_be_bytes());
            signature = Some(head);
        }
        Ok(CopyLines { format, cols, fmt, enc, signature })
    }

    /// Text in the target encoding.
    fn encoded(&self, line: String) -> Result<Vec<u8>> {
        use crate::types::mbconv;
        if (self.enc, self.fmt.server_encoding) == (mbconv::UTF8, mbconv::UTF8) {
            return Ok(line.into_bytes());
        }
        Ok(mbconv::server::to_client(&line, self.enc, self.fmt.server_encoding)?.into_owned())
    }

    /// The HEADER line (text and CSV).
    fn header(&self) -> Result<Option<Vec<u8>>> {
        let f = self.format;
        if !f.header || f.binary {
            return Ok(None);
        }
        let single = self.cols.len() == 1;
        let line: Vec<String> = self
            .cols
            .iter()
            .map(|c| if f.csv { csv_field(&c.name, f, false, single) } else { text_escape(&c.name, f.delimiter) })
            .collect();
        self.encoded(format!("{}\n", line.join(&f.delimiter.to_string()))).map(Some)
    }

    fn row(&mut self, r: &[Value]) -> Result<Vec<u8>> {
        use bytes::BufMut;
        let f = self.format;
        if f.binary {
            let mut b = bytes::BytesMut::new();
            if let Some(h) = self.signature.take() {
                b.put_slice(&h);
            }
            b.put_i16(r.len() as i16);
            for (v, c) in r.iter().zip(self.cols) {
                if v.is_null() {
                    b.put_i32(-1);
                    continue;
                }
                let at = b.len();
                b.put_i32(0);
                crate::types::binary::encode(v, &c.ty, &self.fmt, &mut b)?;
                let n = (b.len() - at - 4) as i32;
                b[at..at + 4].copy_from_slice(&n.to_be_bytes());
            }
            return Ok(b.to_vec());
        }
        let single = self.cols.len() == 1;
        let mut line = String::new();
        for (i, (v, c)) in r.iter().zip(self.cols).enumerate() {
            if i > 0 {
                line.push(f.delimiter);
            }
            match v {
                Value::Null => line.push_str(&f.null),
                v => {
                    let s = format_value(v, &c.ty, &self.fmt);
                    if f.csv {
                        let force = f.force_quote_all || f.force_quote.contains(&c.name);
                        line.push_str(&csv_field(&s, f, force, single));
                    } else {
                        line.push_str(&text_escape(&s, f.delimiter));
                    }
                }
            }
        }
        line.push('\n');
        self.encoded(line)
    }

    /// The binary trailer (with the signature, when no row carried it).
    fn trailer(&mut self) -> Option<Vec<u8>> {
        if !self.format.binary {
            return None;
        }
        let mut tail = self.signature.take().unwrap_or_default();
        tail.extend_from_slice(&(-1i16).to_be_bytes());
        Some(tail)
    }
}

/// Where COPY TO's data goes: the client, as it is made; a server file; or a list.
enum CopyDest<'a> {
    Client(&'a mut dyn super::RowSink),
    File(String, std::io::BufWriter<std::fs::File>),
    Collect(Vec<Vec<u8>>),
}

impl CopyDest<'_> {
    fn put(&mut self, d: Vec<u8>) -> Result<()> {
        use std::io::Write;
        match self {
            CopyDest::Client(k) => k.copy_data(&d),
            CopyDest::File(name, w) => w.write_all(&d).map_err(|e| {
                crate::server_files::file_error(&e, &format!("could not write to COPY file \"{name}\""))
            }),
            CopyDest::Collect(v) => {
                v.push(d);
                Ok(())
            }
        }
    }

    /// The data is complete: CopyDone, or the file flushed.
    fn end(&mut self) -> Result<()> {
        use std::io::Write;
        match self {
            CopyDest::Client(k) => k.copy_end(),
            CopyDest::File(name, w) => w.flush().map_err(|e| {
                crate::server_files::file_error(&e, &format!("could not write to COPY file \"{name}\""))
            }),
            CopyDest::Collect(_) => Ok(()),
        }
    }

    fn collected(self) -> Vec<Vec<u8>> {
        match self {
            CopyDest::Collect(v) => v,
            _ => Vec::new(),
        }
    }
}

/// COPY FROM in binary format: each tuple's fields through the columns' typreceive. Reads
/// the header (once) and the complete tuples at the start of `data` (when not `last`, an
/// incomplete one waits for more data); returns the rows, the bytes taken and whether the
/// trailer came.
fn binary_copy_in(
    data: &[u8],
    c: &CopyIn,
    fmt: &crate::types::text::FormatCtx,
    st: &mut CopyInParsed,
    last: bool,
) -> Result<(Vec<Vec<Expr>>, usize, bool)> {
    let t = &c.table;
    let bad = |m: &str| Error::new(code::BAD_COPY_FILE_FORMAT, m.to_string());
    let take = |p: &mut usize, n: usize| -> Option<&[u8]> {
        let s = data.get(*p..*p + n)?;
        *p += n;
        Some(s)
    };
    let i32_at = |s: &[u8]| i32::from_be_bytes([s[0], s[1], s[2], s[3]]);
    let mut p = 0;
    if !st.header {
        const SIG: &[u8] = b"PGCOPY\n\xff\r\n\0";
        // (Each part of the header is checked once it is all there.)
        let have = |n: usize| last || data.len() >= n;
        if !have(SIG.len()) {
            return Ok((Vec::new(), 0, false));
        }
        if data.len() < SIG.len() || &data[..SIG.len()] != SIG {
            return Err(bad("COPY file signature not recognized"));
        }
        p = SIG.len();
        if !have(p + 4) {
            return Ok((Vec::new(), 0, false));
        }
        let flags = take(&mut p, 4)
            .map(i32_at)
            .ok_or_else(|| bad("invalid COPY file header (missing flags)"))?;
        if flags & (1 << 16) != 0 {
            return Err(bad("invalid COPY file header (WITH OIDS)"));
        }
        if (flags & !(1 << 16)) >> 16 != 0 {
            return Err(bad("unrecognized critical flags in COPY file header"));
        }
        if !have(p + 4) {
            return Ok((Vec::new(), 0, false));
        }
        let ext = take(&mut p, 4)
            .map(i32_at)
            .ok_or_else(|| bad("invalid COPY file header (missing length)"))?;
        if ext >= 0 && !have(p + ext as usize) {
            return Ok((Vec::new(), 0, false));
        }
        if ext < 0 || take(&mut p, ext as usize).is_none() {
            return Err(bad("invalid COPY file header (wrong length)"));
        }
        st.header = true;
    }
    let mut out = Vec::new();
    let mut ended = false;
    let mut done = p;
    loop {
        let line = st.records + out.len() + 1;
        let ctx_line = || format!("COPY {}, line {line}", t.name);
        let Some(n) = take(&mut p, 2).map(|s| i16::from_be_bytes([s[0], s[1]])) else {
            // EOF without a trailer is accepted.
            if last {
                done = data.len();
            }
            break;
        };
        if n == -1 {
            ended = true;
            done = data.len();
            break;
        }
        if n as usize != c.ncols {
            return Err(bad(&format!(
                "row field count is {n}, expected {}",
                c.ncols
            ))
            .context(ctx_line()));
        }
        let mut row = Vec::with_capacity(c.ncols);
        let mut incomplete = false;
        for &col in &c.target {
            let colname = &t.columns[col].name;
            let ty = &t.columns[col].ty;
            let Some(len) = take(&mut p, 4).map(i32_at) else {
                if !last {
                    incomplete = true;
                    break;
                }
                return Err(bad("unexpected EOF in COPY data").context(ctx_line()));
            };
            if len == -1 {
                row.push(Expr::Const(Value::Null, ty.clone()));
                continue;
            }
            if len < 0 {
                return Err(bad("invalid field size").context(ctx_line()));
            }
            let Some(bytes) = take(&mut p, len as usize) else {
                if !last {
                    incomplete = true;
                    break;
                }
                return Err(bad("unexpected EOF in COPY data").context(ctx_line()));
            };
            // (A field is received as it is read, CopyReadBinaryAttribute: its error comes
            // before the rest of the tuple has.)
            let v = crate::types::binary::decode(bytes, ty, fmt).map_err(|e| {
                e.context(format!("COPY {}, line {line}, column {colname}", t.name))
            })?;
            row.push(Expr::Const(v, ty.clone()));
        }
        if incomplete {
            break;
        }
        out.push(row);
        done = p;
    }
    Ok((out, done, ended))
}
