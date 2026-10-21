//! COPY ... FROM STDIN / TO STDOUT in text, CSV and binary formats, and FROM / TO a server
//! file when the server allows them (`crate::server_files`).

use super::{Outcome, QueryResult, Session};
use crate::catalog::{Relation, Table};
use crate::error::{Error, Result, code};
use crate::exec::collect;
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
    fn parse(opts: &[pb::Node], is_from: bool) -> Result<CopyFormat> {
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
                    let n = arg.and_then(crate::sql::binder::ast::ival).unwrap_or(0);
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

/// COPY TO STDOUT output: one data message per line.
pub struct CopyOut {
    pub data: Vec<Vec<u8>>,
    pub ncols: usize,
    pub tag: String,
    pub binary: bool,
    /// Notices raised after the rows went out (the query's AFTER triggers, which fire at its
    /// end): they follow the data.
    pub trailing: Vec<Error>,
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
            (Some(rv), None) => {
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
        let (rows, cols, after, file) = self.with_ctx(params, |ctx| {
            let bq = Binder::new(ctx, Vec::new()).bind_statement(&stmt)?;
            bq.check_perms(ctx)?;
            // BeginCopyTo opens the file once the query is planned.
            let file = if s.filename.is_empty() {
                None
            } else {
                Some(create_copy_file(ctx, &s.filename)?)
            };
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
            // The rows go out as they come; AFTER triggers fire at the query's end (their
            // notices follow the data).
            // (One statement around both, so the query's own doesn't run the queue.)
            let mut rows = Vec::new();
            let mut mark = 0;
            ctx.statement(|| {
                crate::exec::trigger::batch(ctx, || {
                    rows = collect(&bq.plan, ctx, None)?;
                    mark = ctx.notices.borrow().len();
                    Ok(())
                })
            })?;
            let after = ctx.notices.borrow().len() - mark;
            Ok((rows, bq.cols, after, file))
        })?;
        let trailing = self.notices.split_off(self.notices.len().saturating_sub(after));
        let fmt = self.state.settings.borrow().format_ctx();
        let mut data = Vec::new();
        if format.header {
            let line: Vec<String> = cols
                .iter()
                .map(|c| {
                    if format.csv {
                        csv_field(&c.name, &format, false, cols.len() == 1)
                    } else {
                        text_escape(&c.name, format.delimiter)
                    }
                })
                .collect();
            data.push(format!("{}\n", line.join(&format.delimiter.to_string())).into_bytes());
        }
        if format.binary {
            return self.copy_out_to(file, CopyOut {
                data: binary_copy_out(&rows, &cols, &fmt)?,
                ncols: cols.len(),
                tag: format!("COPY {}", rows.len()),
                binary: true,
                trailing,
            });
        }
        for r in &rows {
            let mut fields = Vec::with_capacity(r.len());
            for (v, c) in r.iter().zip(&cols) {
                fields.push(match v {
                    Value::Null => format.null.clone(),
                    v => {
                        let s = format_value(v, &c.ty, &fmt);
                        if format.csv {
                            let force =
                                format.force_quote_all || format.force_quote.contains(&c.name);
                            csv_field(&s, &format, force, cols.len() == 1)
                        } else {
                            text_escape(&s, format.delimiter)
                        }
                    }
                });
            }
            data.push(format!("{}\n", fields.join(&format.delimiter.to_string())).into_bytes());
        }
        // The data goes out in the ENCODING option's encoding, else the client's.
        let enc = self.copy_encoding(&format);
        if enc != crate::types::mbconv::UTF8 {
            for d in &mut data {
                *d = crate::types::mbconv::convert(d, crate::types::mbconv::UTF8, enc)?.into_owned();
            }
        }
        self.copy_out_to(file, CopyOut {
            data,
            ncols: cols.len(),
            tag: format!("COPY {}", rows.len()),
            binary: false,
            trailing,
        })
    }

    /// COPY TO's data: to the client, or into the server file `file` (name, handle).
    fn copy_out_to(&mut self, file: Option<(String, std::fs::File)>, out: CopyOut) -> Result<Outcome> {
        use std::io::Write;
        let Some((name, mut f)) = file else {
            return Ok(Outcome::CopyOut(out));
        };
        let mut w = std::io::BufWriter::new(&mut f);
        for d in &out.data {
            w.write_all(d).map_err(|e| {
                crate::server_files::file_error(&e, &format!("could not write to COPY file \"{name}\""))
            })?;
        }
        w.flush().map_err(|e| {
            crate::server_files::file_error(&e, &format!("could not write to COPY file \"{name}\""))
        })?;
        self.notices.extend(out.trailing);
        Ok(Outcome::Done(QueryResult::command(out.tag)))
    }

    /// BeginCopyFrom's file: its contents (relative names are in the server's files
    /// directory).
    fn read_copy_file(&self, name: &str) -> Result<Vec<u8>> {
        let Some(sf) = self.state.cluster.server_files() else {
            return Err(crate::error::internal("COPY from a file without server files"));
        };
        let path = sf.resolve(name)?;
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
        let mut f = std::fs::File::open(&path).map_err(open_error)?;
        let m = f.metadata().map_err(|e| {
            crate::server_files::file_error(&e, &format!("could not stat file \"{name}\""))
        })?;
        if m.is_dir() {
            return Err(Error::new(
                code::WRONG_OBJECT_TYPE,
                format!("\"{name}\" is a directory"),
            ));
        }
        let mut data = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut data).map_err(|e| {
            crate::server_files::file_error(&e, &format!("could not read from COPY file \"{name}\""))
        })?;
        Ok(data)
    }

    /// The client's data for a COPY FROM STDIN arrived.
    pub fn copy_in_finish(&mut self, c: CopyIn, data: &[u8]) -> Result<QueryResult> {
        let r = self.copy_in_rows(&c, data);
        if r.is_err() {
            self.on_error();
        }
        r
    }

    /// The client aborted COPY FROM STDIN.
    pub fn copy_in_fail(&mut self, msg: &str) -> Error {
        self.on_error();
        Error::new(
            code::QUERY_CANCELED,
            format!("COPY from stdin failed: {msg}"),
        )
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
        let data_len = data.len();
        let enc = if c.format.binary {
            crate::types::mbconv::UTF8
        } else {
            self.copy_encoding(&c.format)
        };
        // Data in another encoding is converted line by line, errors naming the line.
        let converted;
        let data = if enc != crate::types::mbconv::UTF8 {
            let mut out = Vec::with_capacity(data.len());
            for (i, line) in data.split_inclusive(|b| *b == b'\n').enumerate() {
                let u = crate::types::mbconv::to_utf8(line, enc).map_err(|e| {
                    e.context(format!("COPY {}, line {}", c.table.name, i + 1))
                })?;
                out.extend_from_slice(u.as_bytes());
            }
            converted = out;
            &converted[..]
        } else {
            data
        };
        let text = if c.format.binary {
            ""
        } else {
            std::str::from_utf8(data).map_err(|_| {
                Error::new(
                    code::CHARACTER_NOT_IN_REPERTOIRE,
                    "invalid byte sequence for encoding \"UTF8\"",
                )
            })?
        };
        let fmt = self.state.settings.borrow().format_ctx();
        let t = c.table.clone();
        let f = &c.format;
        let records = if f.binary {
            Vec::new()
        } else if f.csv {
            split_csv(text, f)?
        } else {
            split_text(text, f, &t.name)?
        };
        let mut rows: Vec<Vec<Expr>> = Vec::with_capacity(records.len());
        let mut lines: Vec<(usize, String)> = Vec::with_capacity(records.len());
        if f.binary {
            for (i, row) in binary_copy_in(data, c, &fmt)?.into_iter().enumerate() {
                rows.push(row);
                lines.push((i + 1, String::new()));
            }
        }
        let mut skipped = 0usize;
        let ctx_line = |line: usize, raw: &str| format!("COPY {}, line {line}: \"{raw}\"", t.name);
        'records: for (lineno, (raw, rec)) in records.into_iter().enumerate() {
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
            if rec.len() > c.ncols {
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
                    Some(s) => match parse_value(&s, ty, &fmt) {
                        Ok(v) => v,
                        Err(_) if f.on_error_ignore => {
                            skipped += 1;
                            if let Some(limit) = f.reject_limit
                                && skipped as i64 > limit
                            {
                                return Err(Error::new(code::INVALID_TEXT_REPRESENTATION, format!("skipped more than REJECT_LIMIT ({limit}) rows due to data type incompatibility"))
                                    .context(format!("COPY {}, line {line}, column {colname}: \"{s}\"", t.name)));
                            }
                            if f.log_verbosity >= 2 {
                                self.notices.push(
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
        let mut ncols = c.ncols;
        let mut target = c.target.clone();
        let mut excluded = 0i64;
        let processed = self.with_ctx(&[], |ctx| {
            let rules = table_rules(&t, ctx)?;
            // WHERE: each row, with the defaults of the columns not copied, checked before it
            // goes in; the rows that pass carry every column.
            if let Some(f) = &c.filter {
                let all: Vec<usize> = t
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, col)| !col.dropped && col.generated.is_none())
                    .map(|(i, _)| i)
                    .collect();
                let mut kept = Vec::with_capacity(rows.len());
                let mut kept_lines = Vec::with_capacity(lines.len());
                for (row, line) in std::mem::take(&mut rows).into_iter().zip(std::mem::take(&mut lines)) {
                    let mut full = vec![Value::Null; t.columns.len()];
                    for (e, &col) in row.iter().zip(&target) {
                        full[col] = match e {
                            Expr::Default(ty) => match &rules.defaults[col] {
                                Some(d) => crate::sql::expr::eval(d, &[], ctx)?,
                                None => {
                                    let _ = ty;
                                    Value::Null
                                }
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
                    if !crate::sql::expr::eval_bool(f, &full, ctx)? {
                        excluded += 1;
                        continue;
                    }
                    kept.push(
                        all.iter()
                            .map(|&i| Expr::Const(full[i].clone(), t.columns[i].ty.clone()))
                            .collect::<Vec<_>>(),
                    );
                    kept_lines.push(line);
                }
                rows = kept;
                lines = kept_lines;
                target = all;
                ncols = target.len();
            }
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
            // A domain's input function checks each value (NULL too) as the line is read.
            if c.filter.is_none() {
                let mut keep = vec![true; rows.len()];
                'rows: for (k, (row, (line, _))) in rows.iter().zip(&lines).enumerate() {
                    for (e, &col) in row.iter().zip(&target) {
                        let (Expr::Const(v, ty), Some(Some(d))) = (e, rules.domain_checks.get(col))
                        else {
                            continue;
                        };
                        if let Err(err) = d.check(v, ctx) {
                            let colname = &t.columns[col].name;
                            let shown = if v.is_null() {
                                "null input".to_string()
                            } else {
                                format!("\"{}\"", format_value(v, ty, &fmt))
                            };
                            // ON_ERROR ignore: the domain's input function failed softly.
                            if f.on_error_ignore {
                                skipped += 1;
                                if let Some(limit) = f.reject_limit
                                    && skipped as i64 > limit
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
            }
            let skipped_notice = (skipped > 0 && f.log_verbosity >= 1).then(|| {
                if skipped == 1 {
                    "1 row was skipped due to data type incompatibility".to_string()
                } else {
                    format!("{skipped} rows were skipped due to data type incompatibility")
                }
            });
            let copy = Some(Arc::new(crate::sql::plan::CopyLines {
                table: t.name.clone(),
                lines,
                multi,
                skipped_notice,
            }));
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
            // DEFAULT markers take the column default.
            for row in &mut rows {
                for (e, &col) in row.iter_mut().zip(&target) {
                    if let Expr::Default(ty) = e {
                        *e = rules.defaults[col]
                            .clone()
                            .unwrap_or_else(|| Expr::null(ty.clone()));
                    }
                }
            }
            let plan = Plan::Insert(Box::new(InsertPlan {
                rules,
                source: Arc::new(Plan::Values { rows, ncols }),
                target,
                on_conflict: None,
                returning: Vec::new(),
                old_new: None,
                overriding_system: true,
                overriding_user: false,
                sys: false,
                route: crate::sql::binder::partition_routing(&t, ctx)?,
                copy,
                alias: None,
            }));
            *ctx.sess.copy_progress.borrow_mut() = Some(super::state::CopyProgress {
                relid: t.oid,
                command: 1,
                // PROGRESS_COPY_TYPE_FILE (with the file's size) or _PIPE.
                kind: if c.file_size.is_some() { 1 } else { 3 },
                bytes_total: c.file_size.unwrap_or(0),
                bytes_processed: data_len as i64,
                tuples_excluded: excluded,
                tuples_skipped: skipped as i64,
                ..Default::default()
            });
            let fed = match &plan {
                Plan::Insert(i) => match &*i.source {
                    Plan::Values { rows, .. } => rows.len(),
                    _ => 0,
                },
                _ => 0,
            };
            let r = collect(&Arc::new(plan), ctx, None);
            *ctx.sess.copy_progress.borrow_mut() = None;
            r?;
            // Through a view's INSTEAD OF trigger every row counts (CopyFrom's processed).
            if ctx.catalog().views.contains_key(&t.oid) {
                return Ok(fed as u64);
            }
            Ok(ctx.processed.get())
        })?;
        Ok(QueryResult::command(format!("COPY {processed}")))
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

/// Split text-format COPY data into records: the raw line and its fields.
fn split_text(data: &str, f: &CopyFormat, table: &str) -> Result<Records> {
    let mut out = Vec::new();
    // (An empty line is a record of one empty field; no data at all is none.)
    if data.is_empty() {
        return Ok(out);
    }
    let data = data.strip_suffix('\n').unwrap_or(data);
    for (lineno, line) in data.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line == "\\." {
            break;
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
    Ok(out)
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
fn binary_copy_out(
    rows: &[Vec<Value>],
    cols: &[crate::sql::plan::OutCol],
    fmt: &crate::types::text::FormatCtx,
) -> Result<Vec<Vec<u8>>> {
    use bytes::BufMut;
    for c in cols {
        if !crate::types::binary::supports_binary(&c.ty) {
            return Err(Error::new(
                code::UNDEFINED_FUNCTION,
                format!(
                    "no binary output function available for type {}",
                    c.ty.display()
                ),
            ));
        }
    }
    let mut data = Vec::with_capacity(rows.len() + 2);
    let mut head = b"PGCOPY\n\xff\r\n\0".to_vec();
    head.extend_from_slice(&0i32.to_be_bytes());
    head.extend_from_slice(&0i32.to_be_bytes());
    data.push(head);
    for r in rows {
        let mut b = bytes::BytesMut::new();
        b.put_i16(r.len() as i16);
        for (v, c) in r.iter().zip(cols) {
            if v.is_null() {
                b.put_i32(-1);
                continue;
            }
            let mut field = bytes::BytesMut::new();
            crate::types::binary::encode(v, &c.ty, fmt, &mut field)?;
            b.put_i32(field.len() as i32);
            b.put_slice(&field);
        }
        data.push(b.to_vec());
    }
    data.push((-1i16).to_be_bytes().to_vec());
    Ok(data)
}

/// COPY FROM in binary format: each tuple's fields through the columns' typreceive.
fn binary_copy_in(
    data: &[u8],
    c: &CopyIn,
    fmt: &crate::types::text::FormatCtx,
) -> Result<Vec<Vec<Expr>>> {
    let t = &c.table;
    let bad = |m: &str| Error::new(code::BAD_COPY_FILE_FORMAT, m.to_string());
    const SIG: &[u8] = b"PGCOPY\n\xff\r\n\0";
    if data.len() < SIG.len() || &data[..SIG.len()] != SIG {
        return Err(bad("COPY file signature not recognized"));
    }
    let mut p = SIG.len();
    let take = |p: &mut usize, n: usize| -> Option<&[u8]> {
        let s = data.get(*p..*p + n)?;
        *p += n;
        Some(s)
    };
    let i32_at = |s: &[u8]| i32::from_be_bytes([s[0], s[1], s[2], s[3]]);
    let flags = take(&mut p, 4)
        .map(i32_at)
        .ok_or_else(|| bad("invalid COPY file header (missing flags)"))?;
    if flags & (1 << 16) != 0 {
        return Err(bad("invalid COPY file header (WITH OIDS)"));
    }
    if (flags & !(1 << 16)) >> 16 != 0 {
        return Err(bad("unrecognized critical flags in COPY file header"));
    }
    let ext = take(&mut p, 4)
        .map(i32_at)
        .ok_or_else(|| bad("invalid COPY file header (missing length)"))?;
    if ext < 0 || take(&mut p, ext as usize).is_none() {
        return Err(bad("invalid COPY file header (wrong length)"));
    }
    let mut out = Vec::new();
    loop {
        let line = out.len() + 1;
        let ctx_line = || format!("COPY {}, line {line}", t.name);
        let Some(n) = take(&mut p, 2).map(|s| i16::from_be_bytes([s[0], s[1]])) else {
            // EOF without a trailer is accepted.
            break;
        };
        if n == -1 {
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
        for &col in &c.target {
            let colname = &t.columns[col].name;
            let ty = &t.columns[col].ty;
            let len = take(&mut p, 4)
                .map(i32_at)
                .ok_or_else(|| bad("unexpected EOF in COPY data").context(ctx_line()))?;
            if len == -1 {
                row.push(Expr::Const(Value::Null, ty.clone()));
                continue;
            }
            if len < 0 {
                return Err(bad("invalid field size").context(ctx_line()));
            }
            let bytes = take(&mut p, len as usize)
                .ok_or_else(|| bad("unexpected EOF in COPY data").context(ctx_line()))?;
            let v = crate::types::binary::decode(bytes, ty, fmt).map_err(|e| {
                e.context(format!("COPY {}, line {line}, column {colname}", t.name))
            })?;
            row.push(Expr::Const(v, ty.clone()));
        }
        out.push(row);
    }
    Ok(out)
}
