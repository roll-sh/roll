//! SET / SHOW / RESET, PREPARE / EXECUTE / DEALLOCATE, EXPLAIN, cursors and DISCARD.

use super::cursor::Cursor;
use super::{Outcome, Prepared, QueryResult, Session, TxnStatus, warning};
use crate::catalog::quote_ident;
use crate::error::{Error, Result, code};
use crate::exec::collect;
use crate::sql::binder::ast::{const_str, inner};
use crate::sql::binder::{Binder, Clause, resolve_type_name};
use crate::sql::expr::eval;
use crate::sql::plan::OutCol;
use crate::txn::Isolation;
use crate::types::{DataType, Value};
use parking_lot::Mutex;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

const CURSOR_OPT_SCROLL: i32 = 0x0002;
const CURSOR_OPT_NO_SCROLL: i32 = 0x0004;
const CURSOR_OPT_HOLD: i32 = 0x0020;

impl Session {
    // ── SET / RESET ──

    pub(super) fn set_stmt(&mut self, s: &pb::VariableSetStmt) -> Result<QueryResult> {
        use pb::VariableSetKind as K;
        let kind = K::try_from(s.kind).unwrap_or(K::VarSetValue);
        // An implicit transaction (a multi-statement query, a pipeline) counts: a SET in it
        // is undone if it rolls back.
        let mut in_txn = self.status != TxnStatus::Idle || self.txn.is_some();
        let tag = if matches!(kind, K::VarReset | K::VarResetAll) {
            "RESET"
        } else {
            "SET"
        };
        if s.is_local && !in_txn {
            // Outside any transaction block (an implicit one of several statements counts).
            if !self.multi_stmt && !self.pipelining {
                self.raise_notice(warning(
                    code::NO_ACTIVE_SQL_TRANSACTION,
                    "SET LOCAL can only be used in transaction blocks",
                ));
            }
            // It still holds until the implicit transaction ends.
            self.ensure_txn();
            in_txn = true;
        }
        match kind {
            K::VarSetMulti => {
                match s.name.as_str() {
                    "TRANSACTION" => self.set_transaction(&s.args)?,
                    // ImportSnapshot: one pg_export_snapshot() exported in another session.
                    "TRANSACTION SNAPSHOT" => {
                        let id = s
                            .args
                            .first()
                            .and_then(crate::sql::binder::ast::const_str)
                            .unwrap_or_default();
                        if self.status == TxnStatus::Idle && !self.implicit && !self.multi_stmt && !self.pipelining {
                            self.raise_notice(warning(
                                code::NO_ACTIVE_SQL_TRANSACTION,
                                "SET TRANSACTION can only be used in transaction blocks",
                            ));
                        }
                        if let Some(t) = &self.txn
                            && (t.borrow().in_savepoint()
                                || t.borrow().has_writes()
                                || t.borrow().snapshot_taken()
                                || t.borrow().xid_if_assigned().is_some())
                        {
                            return Err(Error::new(
                                code::ACTIVE_SQL_TRANSACTION,
                                "SET TRANSACTION SNAPSHOT must be called before any query",
                            ));
                        }
                        let iso = match (&self.txn, self.txn_opts) {
                            (Some(t), _) => t.borrow().isolation,
                            (None, Some((iso, _))) => iso,
                            (None, None) => self.default_txn_opts().0,
                        };
                        if !iso.txn_snapshot() {
                            return Err(Error::new(
                                code::FEATURE_NOT_SUPPORTED,
                                "a snapshot-importing transaction must have isolation level SERIALIZABLE or REPEATABLE READ",
                            ));
                        }
                        if !id.chars().all(|ch| ch.is_ascii_digit() || ('A'..='F').contains(&ch) || ch == '-') {
                            return Err(Error::new(
                                code::INVALID_PARAMETER_VALUE,
                                format!("invalid snapshot identifier: \"{id}\""),
                            ));
                        }
                        self.ensure_txn();
                        if let Some(t) = &self.txn {
                            t.borrow_mut().import_snapshot(&id)?;
                        }
                    }
                    "SESSION CHARACTERISTICS" => {
                        let (mut iso, mut ro) = self.default_txn_opts();
                        super::apply_txn_options(&s.args, &mut iso, &mut ro)?;
                        let mut st = self.state.settings.borrow_mut();
                        st.set(
                            "default_transaction_isolation",
                            Some(iso.name()),
                            false,
                            in_txn,
                        )?;
                        st.set(
                            "default_transaction_read_only",
                            Some(if ro { "on" } else { "off" }),
                            false,
                            in_txn,
                        )?;
                    }
                    _ => return Err(crate::error::unsupported(format!("SET {}", s.name))),
                }
                return Ok(QueryResult::command("SET"));
            }
            K::VarResetAll => {
                self.state.settings.borrow_mut().reset_all_in(in_txn);
                return Ok(QueryResult::command("RESET"));
            }
            _ => {}
        }
        let name = s.name.as_str();
        let value: Option<String> = match kind {
            K::VarSetDefault | K::VarReset => None,
            // FROM CURRENT: the value it has (GetConfigOptionByName).
            K::VarSetCurrent => Some(self.state.settings.borrow().show(name)?),
            _ => Some(set_value(name, &s.args)?),
        };
        match name {
            "transaction_isolation" | "transaction_read_only" | "transaction_deferrable" => {
                if value.is_none() {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!("parameter \"{name}\" cannot be reset"),
                    ));
                }
                if let Some(v) = &value {
                    let opt = pb::DefElem {
                        defname: name.into(),
                        arg: Some(Box::new(crate::sql::binder::ast::mk(NodeEnum::String(
                            pb::String { sval: v.clone() },
                        )))),
                        ..Default::default()
                    };
                    self.set_transaction(&[crate::sql::binder::ast::mk(NodeEnum::DefElem(
                        Box::new(opt),
                    ))])?;
                }
                return Ok(QueryResult::command(tag));
            }
            "client_encoding" | "names" => {
                // Any encoding with conversions to and from UTF8 (server.rs converts the
                // session's traffic).
                if let Some(v) = &value
                    && let Some(id) = crate::types::encnames::encoding_id(v)
                    && !crate::types::mbconv::has_conversion(id, crate::types::mbconv::UTF8)
                {
                    return Err(Error::new(
                        code::FEATURE_NOT_SUPPORTED,
                        format!("conversion between {v} and UTF8 is not supported"),
                    ));
                }
                self.state.settings.borrow_mut().set(
                    "client_encoding",
                    value.as_deref(),
                    s.is_local,
                    in_txn,
                )?;
                return Ok(QueryResult::command(tag));
            }
            "role" | "session_authorization" => {
                return self.set_role(name, value, s.is_local, in_txn, tag);
            }
            _ => {}
        }
        // check_temp_buffers: fixed once temporary tables have been used.
        if name.eq_ignore_ascii_case("temp_buffers")
            && self.state.temp_ever.get()
            && let Some(v) = &value
        {
            let st = self.state.settings.borrow();
            let mut probe = st.clone();
            drop(st);
            probe.set(name, Some(v), false, false)?;
            let new = probe.get_raw("temp_buffers").unwrap_or_default();
            if Some(&new) != self.state.settings.borrow().get_raw("temp_buffers").as_ref() {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("invalid value for parameter \"temp_buffers\": {new}"),
                )
                .detail("\"temp_buffers\" cannot be changed after any temporary tables have been accessed in the session."));
            }
        }
        let value = match value {
            Some(v) if name.eq_ignore_ascii_case("default_text_search_config") => {
                let cat = self
                    .txn
                    .as_ref()
                    .map(|t| t.borrow().catalog())
                    .unwrap_or_else(|| self.state.db.catalog());
                Some(crate::catalog::tsearch::check_ts_config_setting(
                    &cat,
                    &v,
                    &self.state.search_path(),
                )?)
            }
            Some(v)
                if name.eq_ignore_ascii_case("default_tablespace")
                    || name.eq_ignore_ascii_case("default_table_access_method") =>
            {
                let cat = self
                    .txn
                    .as_ref()
                    .map(|t| t.borrow().catalog())
                    .unwrap_or_else(|| self.state.db.catalog());
                let canon = name.to_ascii_lowercase();
                let invalid = |detail: String| {
                    Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("invalid value for parameter \"{canon}\": \"{v}\""),
                    )
                    .detail(detail)
                };
                if canon == "default_tablespace" {
                    // check_default_tablespace.
                    if !v.is_empty() && cat.tablespace_oid(&v).is_none() {
                        return Err(invalid(format!("Tablespace \"{v}\" does not exist.")));
                    }
                } else {
                    // check_default_table_access_method.
                    if v.is_empty() {
                        return Err(invalid(format!("\"{canon}\" cannot be empty.")));
                    }
                    if v.len() >= 64 {
                        return Err(invalid(format!(
                            "\"{canon}\" is too long (maximum 63 characters)."
                        )));
                    }
                    match cat.access_method_by_name(&v) {
                        None => {
                            return Err(invalid(format!(
                                "Table access method \"{v}\" does not exist."
                            )));
                        }
                        Some(am) if am.kind != 't' => {
                            return Err(Error::new(
                                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                                format!("access method \"{v}\" is not of type TABLE"),
                            ));
                        }
                        Some(_) => {}
                    }
                }
                Some(v)
            }
            v => v,
        };
        // SET granted on the parameter (GRANT SET ON PARAMETER).
        let granted = {
            let cat = self
                .txn
                .as_ref()
                .map(|t| t.borrow().catalog())
                .unwrap_or_else(|| self.state.db.catalog());
            let lname = name.to_ascii_lowercase();
            cat.parameters
                .iter()
                .find(|p| p.1 == lname)
                .is_some_and(|p| {
                    let role = self.state.current_role(&cat);
                    let db_owner = self.state.db_owner(&cat);
                    cat.obj_mask(crate::catalog::acl::ObjKind::Parameter, p.0, role, db_owner)
                        .0
                        & crate::catalog::acl::SET
                        != 0
                })
        };
        self.state.settings.borrow_mut().set_granted(
            name,
            value.as_deref(),
            s.is_local,
            in_txn,
            granted,
        )?;
        Ok(QueryResult::command(tag))
    }

    /// SET ROLE / SET SESSION AUTHORIZATION (and their RESETs).
    fn set_role(
        &mut self,
        name: &str,
        value: Option<String>,
        local: bool,
        in_txn: bool,
        tag: &str,
    ) -> Result<QueryResult> {
        let cat = self
            .txn
            .as_ref()
            .map(|t| t.borrow().catalog())
            .unwrap_or_else(|| self.state.db.catalog());
        if !self.state.definer.borrow().is_empty() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                format!("cannot set parameter \"{name}\" within security-definer function"),
            ));
        }
        let value = value.filter(|v| !(name == "session_authorization" && v == "default"));
        let target = match value.as_deref() {
            None | Some("none") => None,
            Some(v) => Some(
                cat.role_by_name(v)
                    .ok_or_else(|| {
                        Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            format!("role \"{v}\" does not exist"),
                        )
                    })?
                    .oid,
            ),
        };
        if name == "role" {
            if let Some(r) = target {
                let session = self.state.session_role(&cat);
                if !cat.can_set_role(session, r) {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!(
                            "permission denied to set role \"{}\"",
                            value.as_deref().unwrap_or_default()
                        ),
                    ));
                }
            }
            let v = target.map(|r| cat.roles[&r].name.clone());
            self.state.settings.borrow_mut().set(
                "role",
                Some(v.as_deref().unwrap_or("none")),
                local,
                in_txn,
            )?;
        } else {
            // Only a superuser login may become someone else.
            if let Some(r) = target
                && r != self.state.user_oid
                && !cat.is_superuser(self.state.user_oid)
            {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    "permission denied to set session authorization",
                ));
            }
            let v = target
                .map(|r| cat.roles[&r].name.clone())
                .unwrap_or_else(|| self.state.user.clone());
            let mut st = self.state.settings.borrow_mut();
            st.set("session_authorization", Some(&v), local, in_txn)?;
            st.set("role", Some("none"), local, in_txn)?;
        }
        let me = self.state.current_role(&cat);
        let su = if cat.is_superuser(me) { "on" } else { "off" };
        let mut st = self.state.settings.borrow_mut();
        if st.get_raw("is_superuser").as_deref() != Some(su) {
            st.set_internal("is_superuser", su);
            st.changed.push("is_superuser".into());
        }
        Ok(QueryResult::command(tag))
    }

    fn set_transaction(&mut self, opts: &[pb::Node]) -> Result<()> {
        if self.status == TxnStatus::Idle && !self.implicit && !self.multi_stmt && !self.pipelining {
            self.raise_notice(warning(
                code::NO_ACTIVE_SQL_TRANSACTION,
                "SET TRANSACTION can only be used in transaction blocks",
            ));
            return Ok(());
        }
        super::set_deferrable(&self.state, opts);
        if let Some(t) = &self.txn {
            let (iso, _) = (t.borrow().isolation, ());
            let was_ro = t.borrow().read_only;
            let (mut niso, mut ro) = (iso, was_ro);
            super::apply_txn_options(opts, &mut niso, &mut ro)?;
            if niso != iso {
                return Err(Error::new(
                    code::ACTIVE_SQL_TRANSACTION,
                    "SET TRANSACTION ISOLATION LEVEL must be called before any query",
                ));
            }
            // check_transaction_read_only: no going read-write inside a read-only
            // transaction once it has a snapshot (or from a subtransaction).
            if was_ro && !ro {
                return Err(Error::new(
                    code::ACTIVE_SQL_TRANSACTION,
                    if t.borrow().in_savepoint() {
                        "cannot set transaction read-write mode inside a read-only transaction"
                    } else {
                        "transaction read-write mode must be set before any query"
                    },
                ));
            }
            t.borrow_mut().read_only = ro;
            let _ = self.state.settings.borrow_mut().set(
                "transaction_read_only",
                Some(if ro { "on" } else { "off" }),
                true,
                true,
            );
            return Ok(());
        }
        let (mut iso, mut ro) = self.txn_opts.unwrap_or_else(|| self.default_txn_opts());
        super::apply_txn_options(opts, &mut iso, &mut ro)?;
        self.txn_opts = Some((iso, ro));
        Ok(())
    }

    // ── SHOW ──

    pub(super) fn show_cols(&self, name: &str) -> Vec<OutCol> {
        match name {
            "all" => vec![
                OutCol::new("name", DataType::Text),
                OutCol::new("setting", DataType::Text),
                OutCol::new("description", DataType::Text),
            ],
            _ => vec![OutCol::new(show_name(name), DataType::Text)],
        }
    }

    pub(super) fn show_stmt(&mut self, name: &str) -> Result<QueryResult> {
        let cols = self.show_cols(name);
        let st = self.state.settings.borrow();
        let rows = match name {
            "all" => st
                .all()
                .into_iter()
                .map(|r| {
                    let shown = if r.name == "session_authorization" {
                        self.state.user.clone()
                    } else {
                        r.shown
                    };
                    vec![
                        Value::Text(r.name.into()),
                        Value::Text(shown.into()),
                        Value::Text(r.short_desc.into()),
                    ]
                })
                .collect(),
            "transaction_isolation" => {
                let iso = match &self.txn {
                    Some(t) => t.borrow().isolation,
                    None => self.txn_opts.map(|o| o.0).unwrap_or_else(|| {
                        st.get_raw("default_transaction_isolation")
                            .and_then(|v| Isolation::parse(&v))
                            .unwrap_or(Isolation::ReadCommitted)
                    }),
                };
                vec![vec![Value::Text(iso.name().into())]]
            }
            // (Of a transaction not started yet: BEGIN's options, else the default's.)
            "transaction_read_only" => {
                let ro = match &self.txn {
                    Some(t) => t.borrow().read_only,
                    None => self
                        .txn_opts
                        .map(|o| o.1)
                        .unwrap_or_else(|| st.bool("default_transaction_read_only")),
                };
                vec![vec![Value::Text(if ro { "on" } else { "off" }.into())]]
            }
            "transaction_deferrable" if self.txn.is_none() && self.status == TxnStatus::Idle => {
                vec![vec![Value::Text(st.show("default_transaction_deferrable")?.into())]]
            }
            "session_authorization" => {
                let cat = self.state.db.catalog();
                vec![vec![Value::Text(
                    cat.roles
                        .get(&self.state.session_role(&cat))
                        .map_or_else(|| self.state.user.clone(), |r| r.name.clone())
                        .into(),
                )]]
            }
            n => vec![vec![Value::Text(st.show(n)?.into())]],
        };
        Ok(QueryResult::rows(cols, rows, "SHOW"))
    }

    // ── PREPARE / EXECUTE / DEALLOCATE ──

    pub(super) fn prepare_stmt(&mut self, s: &pb::PrepareStmt, text: &str) -> Result<QueryResult> {
        if self.prepared.contains_key(&s.name) {
            return Err(Error::new(
                code::DUPLICATE_PREPARED_STATEMENT,
                format!("prepared statement \"{}\" already exists", s.name),
            ));
        }
        let cat = self.state.db.catalog();
        let path = self.state.search_path();
        let mut types = Vec::new();
        for a in &s.argtypes {
            let Some(NodeEnum::TypeName(tn)) = inner(a) else {
                continue;
            };
            types.push(resolve_type_name(&cat, &path, tn)?.0);
        }
        let query = s
            .query
            .as_deref()
            .map(|n| crate::sql::binder::ast::select_into_as_ctas(n).unwrap_or_else(|| n.clone()));
        let sql = query
            .as_ref()
            .map(crate::sql::binder::ast::deparse_stmt)
            .transpose()?
            .unwrap_or_default();
        let mut p = Prepared {
            sql,
            offset: 0,
            stmt: query,
            param_types: types,
            bound: Mutex::new(None),
            plans: Default::default(),
        };
        // SELECT ... INTO: its parameters are the query's.
        let select_into = match p.stmt.as_ref().and_then(inner) {
            Some(NodeEnum::CreateTableAsStmt(c)) if c.is_select_into => c.query.as_deref().cloned(),
            _ => None,
        };
        if p.is_query() || select_into.is_some() {
            let is_select_into = select_into.is_some();
            let stmt = select_into.unwrap_or_else(|| p.stmt.clone().unwrap());
            let q = Prepared {
                sql: String::new(),
                offset: 0,
                stmt: Some(stmt.clone()),
                param_types: p.param_types.clone(),
                bound: Mutex::new(None),
                plans: Default::default(),
            };
            self.infer_params = true;
            let b = self.bound(&q, &stmt, &[]);
            self.infer_params = false;
            p.param_types = match b {
                Ok(b) => {
                    // Parse analysis locks the relations the statement names (as it will
                    // run them), until the transaction ends.
                    self.with_ctx(&[], |ctx| {
                        crate::sql::binder::perms::lock_named_relations(&b.perms, ctx)
                    })?;
                    // The result's types are known once it is analyzed (result_types).
                    if !is_select_into && !b.cols.is_empty() {
                        p.plans.lock().cols = Some(b.cols.iter().map(|c| c.ty.clone()).collect());
                    }
                    b.params.clone()
                }
                Err(e) => self.analyzed_params(&stmt, &p.param_types, e)?,
            };
        }
        let p = Arc::new(p);
        self.state.prepared_sql.borrow_mut().insert(s.name.clone(), p.clone());
        self.prepared.insert(s.name.clone(), p);
        self.prepared_meta
            .insert(s.name.clone(), (text.to_string(), Self::now(), true));
        Ok(QueryResult::command("PREPARE"))
    }

    pub(super) fn execute_stmt(&mut self, s: &pb::ExecuteStmt) -> Result<Outcome> {
        let (p, values) = self.prepared_args(s)?;
        // (Errors analyzing the statement anew point at its own text, not EXECUTE's.)
        let strip = |mut e: Error| {
            e.position = None;
            e
        };
        // RevalidateCachedQuery: the result's columns can't change under a client.
        let cols = p.plans.lock().cols.clone();
        if let Some(cols) = cols
            && let Some(stmt) = p.stmt.clone()
        {
            let b = self.bound(&p, &stmt, &values).map_err(strip)?;
            let now: Vec<crate::types::DataType> = b.cols.iter().map(|c| c.ty.clone()).collect();
            if now != cols {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    "cached plan must not change result type",
                )
                .routine("RevalidateCachedQuery"));
            }
        }
        self.execute_with(&p, &values, 0).map_err(strip)
    }

    /// EXECUTE's prepared statement and its argument values.
    pub(super) fn prepared_args(&mut self, s: &pb::ExecuteStmt) -> Result<(Arc<Prepared>, Vec<Value>)> {
        let Some(p) = self.prepared.get(&s.name).cloned() else {
            return Err(Error::new(
                code::INVALID_SQL_STATEMENT_NAME,
                format!("prepared statement \"{}\" does not exist", s.name),
            ));
        };
        if s.params.len() != p.param_types.len() {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                format!(
                    "wrong number of parameters for prepared statement \"{}\"",
                    s.name
                ),
            )
            .detail(format!(
                "Expected {} parameters but got {}.",
                p.param_types.len(),
                s.params.len()
            )));
        }
        let types = p.param_types.clone();
        let values = self.with_ctx(&[], |ctx| {
            let mut b = Binder::new(ctx, Vec::new());
            let mut out = Vec::new();
            for (i, (n, t)) in s.params.iter().zip(&types).enumerate() {
                let e = b.bind_scalar(n, Clause::Other)?;
                let given = e.ty();
                let e = b.coerce_assign(e, t, "parameter").map_err(|err| {
                    if err.code == code::DATATYPE_MISMATCH {
                        crate::sql::binder::ast::with_pos(
                            Error::new(
                                code::DATATYPE_MISMATCH,
                                format!(
                                    "parameter ${} of type {} cannot be coerced to the expected type {}",
                                    i + 1,
                                    given.display(),
                                    t.display()
                                ),
                            )
                            .hint("You will need to rewrite or cast the expression."),
                            crate::sql::binder::ast::location(n),
                        )
                    } else {
                        err
                    }
                })?;
                out.push(eval(&e, &[], ctx)?);
            }
            Ok(out)
        })?;
        Ok((p, values))
    }

    pub(super) fn deallocate_stmt(&mut self, s: &pb::DeallocateStmt) -> Result<QueryResult> {
        if s.isall || s.name.is_empty() {
            self.prepared.retain(|n, _| n.is_empty());
            self.state.prepared_sql.borrow_mut().clear();
            return Ok(QueryResult::command("DEALLOCATE ALL"));
        }
        self.state.prepared_sql.borrow_mut().remove(&s.name);
        if self.prepared.remove(&s.name).is_none() {
            return Err(Error::new(
                code::INVALID_SQL_STATEMENT_NAME,
                format!("prepared statement \"{}\" does not exist", s.name),
            ));
        }
        Ok(QueryResult::command("DEALLOCATE"))
    }

    // ── EXPLAIN ──

    pub(super) fn explain_stmt(
        &mut self,
        s: &pb::ExplainStmt,
        params: &[Value],
    ) -> Result<QueryResult> {
        use crate::sql::explain::{Format, explain, options};
        let mut o = options(&s.options)?;
        let mut q = s.query.as_deref().unwrap().clone();
        let mut prepared = None;
        let mut values = params.to_vec();
        let mut ctas = None;
        let mut ctas_types = Vec::new();
        match inner(&q).cloned() {
            Some(NodeEnum::ExecuteStmt(e)) => {
                let (p, v) = self.prepared_args(&e)?;
                q = p
                    .stmt
                    .clone()
                    .ok_or_else(|| crate::error::unsupported("EXPLAIN of this statement"))?;
                prepared = Some(p);
                values = v;
            }
            Some(NodeEnum::CreateTableAsStmt(c)) => {
                // The target is checked before the query is planned (CreateTableAsRelExists):
                // IF NOT EXISTS over an existing one explains nothing.
                if self.with_ctx(&values, |ctx| crate::session::ddl::ctas_rel_exists(ctx, &c))? {
                    return Ok(QueryResult::rows(
                        vec![OutCol::new("QUERY PLAN", DataType::Text)],
                        Vec::new(),
                        "EXPLAIN",
                    ));
                }
                q = (**c.query.as_ref().unwrap()).clone();
                // CREATE TABLE AS EXECUTE explains the prepared statement's query, and runs
                // as CREATE TABLE AS over it.
                let mut c = c;
                if let Some(NodeEnum::ExecuteStmt(e)) = inner(&q).cloned() {
                    let (p, v) = self.prepared_args(&e)?;
                    q = p
                        .stmt
                        .clone()
                        .ok_or_else(|| crate::error::unsupported("EXPLAIN of this statement"))?;
                    c.query = Some(Box::new(q.clone()));
                    ctas_types = p.param_types.clone();
                    prepared = Some(p);
                    values = v;
                }
                ctas = Some(NodeEnum::CreateTableAsStmt(c));
            }
            Some(NodeEnum::SelectStmt(sel))
                if crate::sql::binder::ast::select_into(&sel).is_some() =>
            {
                let c = crate::sql::binder::ast::select_into_as_ctas(&q)
                    .and_then(|n| n.node)
                    .unwrap();
                if let NodeEnum::CreateTableAsStmt(c) = &c {
                    q = (**c.query.as_ref().unwrap()).clone();
                }
                ctas = Some(c);
            }
            Some(NodeEnum::DeclareCursorStmt(d)) => {
                // CURSOR_OPT_SCROLL
                o.scroll = d.options & 0x0002 != 0;
                q = (*d.query.unwrap()).clone()
            }
            _ => {}
        }
        let p = match &prepared {
            Some(p) => p.clone(),
            None => Arc::new(Prepared {
                sql: String::new(),
                offset: 0,
                stmt: Some(q.clone()),
                param_types: Vec::new(),
                bound: Mutex::new(None),
                plans: Default::default(),
            }),
        };
        if !p.is_query() {
            // ExplainOneUtility: EXPLAIN EXECUTE of a prepared utility statement (a CALL
            // prepared over the protocol, as sqlx's query! macro explains it).
            if prepared.is_some() && ctas.is_none() {
                let (ty, out) = match o.format {
                    Format::Text => (DataType::Text, Value::Text("Utility statements have no plan structure".into())),
                    Format::Json => (DataType::Json, Value::Json("[\n  \"Utility Statement\"\n]".into())),
                    Format::Yaml => (DataType::Text, Value::Text("- \"Utility Statement\"".into())),
                    Format::Xml => (
                        DataType::Xml,
                        Value::Text(
                            "<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n  <Utility-Statement />\n</explain>"
                                .into(),
                        ),
                    ),
                };
                return Ok(QueryResult::rows(vec![OutCol::new("QUERY PLAN", ty)], vec![vec![out]], "EXPLAIN"));
            }
            return Err(crate::error::unsupported("EXPLAIN of this statement"));
        }
        if ctas.is_some() && o.analyze {
            self.ensure_txn();
            if self.txn.as_ref().is_some_and(|t| t.borrow().read_only) {
                return Err(Error::new(
                    code::READ_ONLY_SQL_TRANSACTION,
                    "cannot execute CREATE TABLE AS in a read-only transaction",
                ));
            }
        }
        let start = std::time::Instant::now();
        // EXPLAIN EXECUTE takes the plan EXECUTE would (and counts it); otherwise parameter
        // values make a custom plan, unless a generic one is asked for.
        let (bq, generic) = if prepared.is_some() {
            let (bq, custom) = self.planned(&p, &q, &values)?;
            (bq, !custom)
        } else {
            let generic = o.generic_plan
                || self
                    .state
                    .settings
                    .borrow()
                    .get_raw("plan_cache_mode")
                    .as_deref()
                    == Some("force_generic_plan");
            self.infer_params = o.generic_plan;
            let bq = if !generic && !values.is_empty() && !p.param_types.is_empty() {
                self.bound_custom(&p, &q, &values)
            } else {
                self.bound(&p, &q, &values)
            };
            self.infer_params = false;
            (bq?, generic)
        };
        let shown: Vec<Value> = if generic { Vec::new() } else { values.clone() };
        let planning = start.elapsed().as_secs_f64();
        let out = self.with_ctx(&values, |ctx| {
            bq.check_perms(ctx)?;
            let mut run = None;
            let mut plan = (bq.plan.clone(), bq.hidden_rtes);
            if o.analyze {
                let instr = std::rc::Rc::new(crate::exec::Instr::default());
                *ctx.instr.borrow_mut() = Some(instr.clone());
                let t = std::time::Instant::now();
                let r = match &ctas {
                    // CREATE TABLE AS runs for real; its own binding of the query is the one
                    // the counters belong to.
                    Some(node) if !ctas_types.is_empty() => {
                        crate::session::ddl::execute_with_params(ctx, node, &ctas_types).map(|_| ())
                    }
                    Some(node) => crate::session::ddl::execute(ctx, node, "").map(|_| ()),
                    None => collect(&bq.plan, ctx, None).map(|_| ()),
                };
                *ctx.instr.borrow_mut() = None;
                r?;
                if let Some(ran) = ctx.explained.borrow_mut().take() {
                    plan = ran;
                }
                run = Some((instr, t.elapsed().as_secs_f64()));
            }
            let instr = run.as_ref().map(|(i, _)| &**i);
            Ok(explain(
                &plan.0,
                plan.1,
                &o,
                ctx,
                &shown,
                instr,
                planning,
                run.as_ref().map_or(0.0, |r| r.1),
            ))
        })?;
        let (ty, rows) = match o.format {
            Format::Text => (
                DataType::Text,
                // Expressions print their literals for the session's
                // standard_conforming_strings.
                out.lines()
                    .map(|l| vec![Value::Text(crate::sql::scs::session_literals(l).into())])
                    .collect(),
            ),
            Format::Json => (DataType::Json, vec![vec![Value::Json(out.into())]]),
            Format::Xml => (DataType::Xml, vec![vec![Value::Text(out.into())]]),
            Format::Yaml => (DataType::Text, vec![vec![Value::Text(out.into())]]),
        };
        Ok(QueryResult::rows(
            vec![OutCol::new("QUERY PLAN", ty)],
            rows,
            "EXPLAIN",
        ))
    }

    // ── CALL ──

    pub(super) fn call_stmt(
        &mut self,
        c: &pb::CallStmt,
        params: &[Value],
        types: &[DataType],
    ) -> Result<QueryResult> {
        let atomic = self.atomic();
        self.with_ctx(params, |ctx| {
            let (f, args) = if types.len() == params.len() {
                bind_call_typed(ctx, c, types)?
            } else {
                bind_call(ctx, c, params)?
            };
            // Only the input arguments are evaluated (ExecuteCallStmt).
            let modes: Vec<char> = f.all_params().into_iter().map(|p| p.2).collect();
            let values = args
                .iter()
                .enumerate()
                .map(|(i, e)| match modes.get(i) {
                    Some('o' | 't') => Ok(Value::Null),
                    _ => eval(e, &[], ctx),
                })
                .collect::<Result<Vec<_>>>()?;
            match crate::plpgsql::call_procedure(ctx, &f, values, atomic)? {
                Some(row) => Ok(QueryResult::rows(
                    f.out_cols
                        .iter()
                        .map(|(n, t)| OutCol::new(n.clone(), t.clone()))
                        .collect(),
                    vec![row],
                    "CALL",
                )),
                None => Ok(QueryResult::command("CALL")),
            }
        })
    }

    // ── cursors ──

    pub(super) fn declare_cursor(
        &mut self,
        s: &pb::DeclareCursorStmt,
        params: &[Value],
        text: &str,
    ) -> Result<QueryResult> {
        let hold = s.options & CURSOR_OPT_HOLD != 0;
        let q = s.query.as_deref().unwrap();
        if !matches!(inner(q), Some(NodeEnum::SelectStmt(_))) {
            return Err(Error::new(
                code::INVALID_CURSOR_DEFINITION,
                "DECLARE CURSOR must be a SELECT statement",
            ));
        }
        let p = Prepared {
            sql: String::new(),
            offset: 0,
            stmt: Some(q.clone()),
            param_types: Vec::new(),
            bound: Mutex::new(None),
            plans: Default::default(),
        };
        let bq = self.bound(&p, q, params)?;
        if bq.plan.writes() {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "DECLARE CURSOR must not contain data-modifying statements in WITH",
            ));
        }
        // After parse analysis (PerformCursorOpen).
        if !hold && self.status == TxnStatus::Idle {
            return Err(Error::new(
                code::NO_ACTIVE_SQL_TRANSACTION,
                "DECLARE CURSOR can only be used in transaction blocks",
            ));
        }
        if self.state.cursors.borrow().contains_key(&s.portalname) {
            return Err(Error::new(
                code::DUPLICATE_CURSOR,
                format!("cursor \"{}\" already exists", s.portalname),
            ));
        }
        let keyed = crate::session::cursor::keyed_plan(&bq.plan);
        let scroll = if s.options & (CURSOR_OPT_SCROLL | CURSOR_OPT_NO_SCROLL) == 0 {
            crate::session::cursor::scrollable(&bq.plan)
        } else {
            s.options & CURSOR_OPT_SCROLL != 0
        };
        // The query only starts: it runs as FETCH and MOVE pass over its rows.
        let c = self.with_ctx(params, |ctx| {
            bq.check_perms(ctx)?;
            let plan = match &keyed {
                Some((plan, _)) => Arc::new(plan.clone()),
                None => bq.plan.clone(),
            };
            let lazy = crate::session::cursor::Lazy::new(
                ctx,
                plan,
                std::rc::Rc::new(Vec::new()),
                None,
                ctx.child(&[]).keep(),
            )
            .with_params(params.to_vec())
            .for_cursor();
            Ok(Cursor::new_lazy(
                bq.cols.clone(),
                lazy,
                keyed.map(|k| k.1),
                hold,
                scroll,
                text.to_string(),
                Self::now(),
            ))
        })?;
        let mut c = c;
        c.subxact = self.txn.as_ref().map_or(0, |t| t.borrow().subtxn_depth());
        c.rels = bq.perms.rels.iter().map(|r| r.oid).collect();
        c.row_marks = crate::session::cursor::has_row_marks(&bq.plan);
        self.state
            .cursors
            .borrow_mut()
            .insert(s.portalname.clone(), c);
        Ok(QueryResult::command("DECLARE CURSOR"))
    }

    pub(super) fn fetch_stmt(&mut self, s: &pb::FetchStmt) -> Result<QueryResult> {
        let dir =
            pb::FetchDirection::try_from(s.direction).unwrap_or(pb::FetchDirection::FetchForward);
        let got = self.with_ctx(&[], |ctx| {
            crate::session::cursor::fetch(ctx, &s.portalname, dir, s.how_many, !s.ismove)
        })?;
        let Some((out, cols, n)) = got else {
            if let Some(r) = self.fetch_protocol_portal(s)? {
                return Ok(r);
            }
            return Err(Error::new(
                code::INVALID_CURSOR_NAME,
                format!("cursor \"{}\" does not exist", s.portalname),
            ));
        };
        if s.ismove {
            return Ok(QueryResult::command(format!("MOVE {n}")));
        }
        let tag = format!("FETCH {}", out.len());
        Ok(QueryResult::rows(cols, out, tag))
    }

    pub(super) fn close_cursor(&mut self, s: &pb::ClosePortalStmt) -> Result<QueryResult> {
        if s.portalname.is_empty() {
            self.state.cursors.borrow_mut().clear();
            return Ok(QueryResult::command("CLOSE CURSOR ALL"));
        }
        if self
            .state
            .cursors
            .borrow_mut()
            .remove(&s.portalname)
            .is_none()
        {
            return Err(Error::new(
                code::INVALID_CURSOR_NAME,
                format!("cursor \"{}\" does not exist", s.portalname),
            ));
        }
        Ok(QueryResult::command("CLOSE CURSOR"))
    }

    // ── DISCARD ──

    pub(super) fn discard_stmt(&mut self, s: &pb::DiscardStmt) -> Result<QueryResult> {
        use pb::DiscardMode as M;
        match M::try_from(s.target).unwrap_or(M::DiscardAll) {
            M::DiscardAll => {
                if self.status != TxnStatus::Idle {
                    return Err(Error::new(
                        code::ACTIVE_SQL_TRANSACTION,
                        "DISCARD ALL cannot run inside a transaction block",
                    ));
                }
                // As DiscardAll: back to the login role first (what drops the temporary
                // tables then), then every other setting.
                self.set_role("session_authorization", None, false, false, "RESET")?;
                self.state.settings.borrow_mut().reset_all();
                self.prepared.retain(|n, _| n.is_empty());
                self.state.prepared_sql.borrow_mut().clear();
                self.state.cursors.borrow_mut().clear();
                self.state.listen_actions.borrow_mut().clear();
                self.state.db.notify.unlisten(self.state.pid, None);
                self.state.release_advisory();
                self.state.currval.borrow_mut().clear();
                self.state.lastval.set(None);
                self.state.seq_cache.borrow_mut().clear();
                self.drop_temp_tables()?;
                Ok(QueryResult::command("DISCARD ALL"))
            }
            M::DiscardPlans => {
                for p in self.prepared.values() {
                    *p.bound.lock() = None;
                }
                Ok(QueryResult::command("DISCARD PLANS"))
            }
            M::DiscardSequences => {
                self.state.currval.borrow_mut().clear();
                self.state.lastval.set(None);
                self.state.seq_cache.borrow_mut().clear();
                Ok(QueryResult::command("DISCARD SEQUENCES"))
            }
            M::DiscardTemp => {
                self.drop_temp_tables()?;
                Ok(QueryResult::command("DISCARD TEMP"))
            }
            _ => Ok(QueryResult::command("DISCARD")),
        }
    }

    /// Drop this session's temporary tables.
    pub(crate) fn drop_temp_tables(&mut self) -> Result<()> {
        self.drop_temp_objects(false)
    }

    /// Drop the temporary objects of this session's temporary schema, or (`all`) of every
    /// session's (the last session of a database leaving: the others are gone).
    pub(crate) fn drop_temp_objects(&mut self, all: bool) -> Result<()> {
        let own = self.state.temp_schema();
        let in_scope = |name: &str| {
            if all {
                name.starts_with("pg_temp_")
            } else {
                name == own
            }
        };
        let cat = self.state.db.catalog();
        let names: Vec<String> = cat
            .tables
            .values()
            .filter(|t| t.temporary && in_scope(cat.schema_name(t.schema)))
            .map(|t| {
                format!(
                    "{}.{}",
                    quote_ident(cat.schema_name(t.schema)),
                    quote_ident(&t.name)
                )
            })
            .collect();
        drop(cat);
        if !names.is_empty() {
            let sql = format!("DROP TABLE IF EXISTS {} CASCADE", names.join(", "));
            for p in self.parse(&sql)? {
                self.execute(&p)?;
            }
            self.end_batch()?;
        }
        // Then what else is left in the schema (ResetTempTableNamespace empties it): views
        // and sequences of their own.
        let cat = self.state.db.catalog();
        let qualified = |schema: u32, name: &str| {
            format!("{}.{}", quote_ident(cat.schema_name(schema)), quote_ident(name))
        };
        let mut stmts: Vec<String> = Vec::new();
        for (mat, kw) in [(true, "MATERIALIZED VIEW"), (false, "VIEW")] {
            let views: Vec<String> = cat
                .views
                .values()
                .filter(|v| v.materialized == mat && in_scope(cat.schema_name(v.schema)))
                .map(|v| qualified(v.schema, &v.name))
                .collect();
            if !views.is_empty() {
                stmts.push(format!("DROP {kw} IF EXISTS {} CASCADE", views.join(", ")));
            }
        }
        let seqs: Vec<String> = cat
            .sequences
            .values()
            .filter(|q| in_scope(cat.schema_name(q.schema)))
            .map(|q| qualified(q.schema, &q.name))
            .collect();
        if !seqs.is_empty() {
            stmts.push(format!("DROP SEQUENCE IF EXISTS {} CASCADE", seqs.join(", ")));
        }
        drop(cat);
        for sql in stmts {
            // (A view's drop may have taken a sequence along: no "skipping" notices.)
            let before = self.notices.len();
            let held = self.state.notice_hold.replace(self.state.notice_hold.get() + 1);
            let r = (|| {
                for p in self.parse(&sql)? {
                    self.execute(&p)?;
                }
                self.end_batch()
            })();
            self.state.notice_hold.set(held);
            r?;
            self.notices.truncate(before);
        }
        Ok(())
    }
}

/// The value of `SET name TO ...` as the setting's text.
/// List parameters whose elements are quoted as identifiers (GUC_LIST_QUOTE).
pub(crate) const LIST_QUOTE: &[&str] = &[
    "search_path",
    "temp_tablespaces",
    "local_preload_libraries",
    "session_preload_libraries",
    "shared_preload_libraries",
    "unix_socket_directories",
];

/// A GUC_LIST_QUOTE value's elements (SplitGUCList): comma-separated, each an identifier
/// that may be double-quoted.
pub(crate) fn split_guc_list(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = v.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut item = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        item.push('"');
                        continue;
                    }
                    break;
                }
                item.push(c);
            }
            while chars.peek().is_some_and(|c| *c != ',') {
                chars.next();
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' {
                    break;
                }
                item.push(c);
                chars.next();
            }
            item = item.trim_end().to_string();
        }
        out.push(item);
        if chars.next() != Some(',') {
            break;
        }
    }
    out
}

pub(crate) fn set_value(name: &str, args: &[pb::Node]) -> Result<String> {
    // Only list parameters take several values (GUC_LIST_INPUT).
    const LISTS: &[&str] = &[
        "search_path",
        "temp_tablespaces",
        "datestyle",
        "local_preload_libraries",
        "session_preload_libraries",
        "shared_preload_libraries",
        "log_destination",
        "listen_addresses",
        "unix_socket_directories",
        "debug_io_direct",
        "createrole_self_grant",
        "restrict_nonsystem_relation_kind",
        "oauth_validator_libraries",
        "synchronized_standby_slots",
        "plpgsql.extra_errors",
        "plpgsql.extra_warnings",
    ];
    if args.len() > 1
        && !LISTS.contains(&name.to_ascii_lowercase().as_str())
        && super::settings::is_builtin(name)
    {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("SET {name} takes only one argument"),
        ));
    }
    let mut parts = Vec::new();
    for a in args {
        let v = match inner(a) {
            Some(NodeEnum::AConst(c)) => match &c.val {
                Some(pb::a_const::Val::Sval(s)) => {
                    // GUC_LIST_QUOTE parameters keep each element an identifier.
                    if LIST_QUOTE.contains(&name.to_ascii_lowercase().as_str())
                        && name != "search_path"
                    {
                        quote_ident(&s.sval)
                    } else if name == "search_path" && !s.sval.starts_with('"') {
                        if s.sval == "$user" {
                            "\"$user\"".to_string()
                        } else {
                            quote_ident(&s.sval)
                        }
                    } else {
                        s.sval.clone()
                    }
                }
                Some(pb::a_const::Val::Ival(i)) => i.ival.to_string(),
                Some(pb::a_const::Val::Fval(f)) => f.fval.clone(),
                Some(pb::a_const::Val::Boolval(b)) => {
                    if b.boolval { "on" } else { "off" }.to_string()
                }
                _ => String::new(),
            },
            // SET TIME ZONE INTERVAL '+02:00' HOUR TO MINUTE
            Some(NodeEnum::TypeCast(tc)) => {
                let v = tc.arg.as_deref().and_then(const_str).unwrap_or_default();
                if name == "timezone" {
                    interval_zone(&v)?
                } else {
                    v
                }
            }
            _ => const_str(a).unwrap_or_default(),
        };
        parts.push(v);
    }
    Ok(parts.join(", "))
}

/// `INTERVAL '+02:00'` as a time zone: a fixed offset, written the POSIX way.
fn interval_zone(v: &str) -> Result<String> {
    let t = v.trim();
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let mut it = rest.split(':');
    let h: i32 = it
        .next()
        .and_then(|x| x.trim().parse().ok())
        .ok_or_else(|| {
            Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("invalid interval value for time zone: \"{v}\""),
            )
        })?;
    let m: i32 = it.next().and_then(|x| x.trim().parse().ok()).unwrap_or(0);
    // POSIX offsets have the opposite sign.
    let sign = if neg { "+" } else { "-" };
    Ok(if m == 0 {
        format!("<{}{:02}>{sign}{h}", if neg { "-" } else { "+" }, h)
    } else {
        format!(
            "<{}{:02}{:02}>{sign}{h}:{m:02}",
            if neg { "-" } else { "+" },
            h,
            m
        )
    })
}

/// Column name of SHOW: the parameter's canonical spelling.
fn show_name(name: &str) -> String {
    match name {
        "timezone" => "TimeZone".into(),
        "transaction_isolation" => "transaction_isolation".into(),
        n => super::settings::canonical_name(n),
    }
}

/// Resolve a CALL's procedure and bind its arguments (one per parameter, OUT ones included).
pub(crate) fn bind_call(
    ctx: &crate::exec::ExecCtx,
    c: &pb::CallStmt,
    params: &[Value],
) -> Result<(Arc<crate::catalog::Function>, Vec<crate::sql::expr::Expr>)> {
    bind_call_typed(
        ctx,
        c,
        &params
            .iter()
            .map(crate::types::cast::value_type)
            .collect::<Vec<_>>(),
    )
}

/// `bind_call` with parameters of the given types.
pub(crate) fn bind_call_typed(
    ctx: &crate::exec::ExecCtx,
    c: &pb::CallStmt,
    params: &[DataType],
) -> Result<(Arc<crate::catalog::Function>, Vec<crate::sql::expr::Expr>)> {
    let fc = c
        .funccall
        .as_deref()
        .ok_or_else(|| Error::new(code::SYNTAX_ERROR, "syntax error in CALL"))?;
    let fname = crate::sql::binder::ast::names(&fc.funcname);
    let (schema, name) = match fname.as_slice() {
        [n] => (None, n.as_str()),
        [s, n] => (Some(s.as_str()), n.as_str()),
        _ => {
            return Err(Error::new(
                code::SYNTAX_ERROR,
                format!(
                    "improper qualified name (too many dotted names): {}",
                    fname.join(".")
                ),
            ));
        }
    };
    let mut b = Binder::new(ctx, params.to_vec());
    // The arguments are evaluated when it runs (an output argument's never is).
    b.unplanned = true;
    let r = b
        .resolve_routine(schema, name, &fc.args, fc.func_variadic, fc.location, true)?
        .expect("CALL resolves or fails");
    crate::sql::binder::perms::check(&b.perms, ctx)?;
    Ok(r)
}

/// SET / RESET run inside a function (PL/pgSQL): like the statement, in the current
/// (sub)transaction.
pub(crate) fn routine_set(ctx: &crate::exec::ExecCtx, s: &pb::VariableSetStmt) -> Result<()> {
    use pb::VariableSetKind as K;
    let kind = K::try_from(s.kind).unwrap_or(K::VarSetValue);
    let mut st = ctx.sess.settings.borrow_mut();
    match kind {
        K::VarResetAll => {
            st.reset_all_in(true);
            return Ok(());
        }
        K::VarSetMulti | K::VarSetCurrent => {
            return Err(crate::error::unsupported(format!("SET {} inside a function", s.name)));
        }
        _ => {}
    }
    let name = s.name.as_str();
    if matches!(name, "role" | "session_authorization") {
        return Err(crate::error::unsupported(format!("SET {name} inside a function")));
    }
    let value = match kind {
        K::VarSetDefault | K::VarReset => None,
        _ => Some(set_value(name, &s.args)?),
    };
    let value = match value {
        Some(v) if name.eq_ignore_ascii_case("default_text_search_config") => Some(
            crate::catalog::tsearch::check_ts_config_setting(&ctx.catalog(), &v, &ctx.search_path)?,
        ),
        v => v,
    };
    st.set(name, value.as_deref(), s.is_local, true)
}

impl Session {
    /// LOAD 'library': the engine's modules are built in; loading PL/pgSQL reserves its prefix.
    pub(super) fn load_stmt(&mut self, s: &pb::LoadStmt) -> Result<QueryResult> {
        let lib = s.filename.trim_start_matches("$libdir/");
        let lib = lib.strip_suffix(".so").unwrap_or(lib);
        match lib {
            "plpgsql" => {
                let w = self.state.settings.borrow_mut().reserve_prefix("plpgsql");
                self.notices.extend(w);
            }
            "auto_explain" | "plugins/auto_explain" | "passwordcheck" => {}
            // An extension library the engine implements natively: its parameters.
            l if crate::ext::native::has_library(l) || l == "pg_stat_statements" => {
                let w = self.state.settings.borrow_mut().load_module(l);
                self.notices.extend(w);
            }
            _ => {
                return Err(Error::new(
                    code::UNDEFINED_FILE,
                    format!("could not access file \"{}\": No such file or directory", s.filename),
                ));
            }
        }
        Ok(QueryResult::command("LOAD"))
    }
}
