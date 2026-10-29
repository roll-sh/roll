//! What a session writes to the server log (postgres.c's statement and duration logging,
//! elog.c's error reporting): log_statement, log_duration, log_min_duration_statement /
//! log_min_duration_sample, errors past log_min_messages with their STATEMENT, and LOG
//! messages to the client too when client_min_messages lets them through.

use super::{Prepared, Session};
use crate::error::{Error, Severity};
use crate::log::{Format, Message, SessionWho, Who};
use crate::sql::binder::ast::inner;
use pg_query::NodeEnum;
use std::sync::Arc;

/// GetCommandLogLevel's levels, in log_statement's order.
#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum LogStmt {
    All,
    Mod,
    Ddl,
}

impl Session {
    /// log_line_prefix's (and csvlog's / jsonlog's) view of this session.
    pub fn log_who(&self) -> Who {
        let st = self.state.settings.borrow();
        let conn = self.state.log_conn.borrow();
        let xid = self.txn.as_ref().and_then(|t| t.borrow().xid_if_assigned()).unwrap_or(0);
        // (pg_locks' virtual transaction ids; a local id of 0 between transactions.)
        let lxid = self.txn.as_ref().map_or(0, |t| t.borrow().id as u32);
        Who {
            pid: self.state.pid,
            backend_type: "client backend",
            start: self.state.backend_start,
            remote: conn.remote.clone(),
            local: conn.local.clone(),
            lines: conn.lines.clone(),
            session: Some(SessionWho {
                user: st.session_authorization().to_string(),
                database: self.state.database.clone(),
                // (Not applied yet while authenticating.)
                application_name: if *self.log_tag.borrow() == "authentication" { String::new() } else { st.show_value("application_name") },
                command_tag: self.log_tag.borrow().clone(),
                vxid: if self.log_exiting.get() { String::new() } else { format!("3/{lxid}") },
                xid,
                query_id: 0,
            }),
        }
    }

    pub fn log_format(&self) -> Format {
        Format::from_settings(&self.state.settings.borrow())
    }

    /// The process title's activity (%i, csvlog's / jsonlog's "ps"): the statement's
    /// command tag while it runs (only kept while the log is written).
    pub fn set_ps(&self, p: &Prepared) {
        if crate::log::active() {
            *self.log_tag.borrow_mut() = p.stmt.as_ref().and_then(|n| inner(n)).map_or_else(|| "???".into(), ps_tag);
        }
    }

    /// `set_ps` for the statement of portal `name` (an Execute message).
    pub fn set_ps_portal(&self, name: &str) {
        if crate::log::active()
            && let Some(p) = self.portals.get(name)
        {
            self.set_ps(&p.stmt);
        }
    }

    /// The title between statements (ReadyForQuery's).
    pub fn set_ps_idle(&self) {
        if crate::log::active() {
            *self.log_tag.borrow_mut() = match self.status() {
                super::TxnStatus::Idle => "idle",
                super::TxnStatus::InBlock => "idle in transaction",
                super::TxnStatus::Failed => "idle in transaction (aborted)",
            }
            .into();
        }
    }

    /// A LOG-level message (statement / duration lines, auto_explain's plans): to the server
    /// log past log_min_messages, to the client past client_min_messages (errhidestmt: no
    /// STATEMENT line).
    pub fn log_message(&mut self, text: &str, detail: Option<&str>) {
        self.log_at(Severity::Log, text, detail)
    }

    /// `log_message` at level `sev`.
    pub fn log_at(&mut self, sev: Severity, text: &str, detail: Option<&str>) {
        let f = self.log_format();
        if crate::log::wanted(&f, sev) {
            let who = self.log_who();
            crate::log::write(&f, &who, &Message { detail, severity: Some(sev), ..Message::log(text) });
        }
        if super::settings::notice_wanted(&self.state.settings.borrow(), sev) {
            let mut e = Error::new(if sev == Severity::Warning { "01000" } else { "00000" }, text.to_string());
            e.severity = sev;
            e.detail = detail.map(str::to_string);
            self.notices.push(e);
        }
    }

    /// An error (or warning) going to the client goes to the server log too, past
    /// log_min_messages, with the query that raised it past log_min_error_statement.
    pub fn log_error(&self, e: &Error, statement: Option<&str>) {
        if !crate::log::active() {
            return;
        }
        let f = self.log_format();
        if crate::log::wanted(&f, e.severity) {
            crate::log::write(&f, &self.log_who(), &Message::error(e, statement));
        }
    }

    /// check_log_statement: whether log_statement covers one of `stmts`.
    pub fn statement_logged(&self, stmts: &[Arc<Prepared>]) -> bool {
        if !self.state.settings.borrow().logs_statements() {
            return false;
        }
        let want = match self.state.settings.borrow().get_str("log_statement") {
            Some("all") => LogStmt::All,
            Some("mod") => LogStmt::Mod,
            Some("ddl") => LogStmt::Ddl,
            _ => return false,
        };
        stmts.iter().filter_map(|p| p.stmt.as_ref()).any(|n| self.log_level(n) >= want)
    }

    fn log_level(&self, n: &pg_query::protobuf::Node) -> LogStmt {
        use NodeEnum as N;
        match inner(n) {
            Some(N::SelectStmt(s)) if s.into_clause.is_some() => LogStmt::Ddl,
            Some(
                N::SelectStmt(_)
                | N::VariableSetStmt(_)
                | N::VariableShowStmt(_)
                | N::TransactionStmt(_)
                | N::DiscardStmt(_)
                | N::LockStmt(_)
                | N::ConstraintsSetStmt(_)
                | N::CheckPointStmt(_)
                | N::ReindexStmt(_)
                | N::CallStmt(_)
                | N::DoStmt(_)
                | N::LoadStmt(_)
                | N::ClusterStmt(_)
                | N::NotifyStmt(_)
                | N::ListenStmt(_)
                | N::UnlistenStmt(_)
                | N::DeclareCursorStmt(_)
                | N::ClosePortalStmt(_)
                | N::FetchStmt(_)
                | N::DeallocateStmt(_)
                | N::VacuumStmt(_),
            ) => LogStmt::All,
            Some(N::InsertStmt(_) | N::UpdateStmt(_) | N::DeleteStmt(_) | N::MergeStmt(_) | N::TruncateStmt(_)) => {
                LogStmt::Mod
            }
            Some(N::CopyStmt(c)) if c.is_from => LogStmt::Mod,
            Some(N::CopyStmt(_)) => LogStmt::All,
            Some(N::ExplainStmt(e)) => {
                let analyze = e.options.iter().any(|o| matches!(inner(o), Some(N::DefElem(d)) if d.defname == "analyze"));
                match (&e.query, analyze) {
                    (Some(q), true) => self.log_level(q),
                    _ => LogStmt::All,
                }
            }
            Some(N::PrepareStmt(p)) => p.query.as_ref().map_or(LogStmt::All, |q| self.log_level(q)),
            Some(N::ExecuteStmt(e)) => self
                .state
                .prepared_sql
                .borrow()
                .get(&e.name)
                .and_then(|p| p.stmt.clone())
                .map_or(LogStmt::All, |q| self.log_level(&q)),
            _ => LogStmt::Ddl,
        }
    }

    /// errdetail_execute: an EXECUTE's prepared statement text.
    pub fn execute_detail(&self, stmts: &[Arc<Prepared>]) -> Option<String> {
        stmts.iter().filter_map(|p| p.stmt.as_ref()).find_map(|n| match inner(n) {
            Some(NodeEnum::ExecuteStmt(e)) => {
                self.state.prepared_sql.borrow().get(&e.name).map(|p| format!("prepare: {}", p.sql))
            }
            _ => None,
        })
    }

    /// check_log_duration: Some((ms, with_statement)) when the statement's duration goes to
    /// the log (with its text when log_min_duration_statement / sampling chose it and
    /// log_statement didn't already log it).
    pub fn duration_logged(&self, elapsed: std::time::Duration, was_logged: bool) -> Option<(String, bool)> {
        let st = self.state.settings.borrow();
        if !st.logs_statements() {
            return None;
        }
        let duration = st.bool("log_duration");
        let min = st.int("log_min_duration_statement");
        let sample = st.int("log_min_duration_sample");
        if !duration && min < 0 && sample < 0 {
            return None;
        }
        let us = elapsed.as_micros() as i64;
        let ms = us / 1000;
        let exceeded = min == 0 || (min > 0 && ms >= min);
        let rate: f64 = st.get_str("log_statement_sample_rate").and_then(|v| v.parse().ok()).unwrap_or(1.0);
        let in_sample = (sample == 0 || (sample > 0 && ms >= sample))
            && rate != 0.0
            && (rate == 1.0 || random_unit() <= rate);
        if !(exceeded || in_sample || duration) {
            return None;
        }
        Some((format!("{}.{:03}", ms, us % 1000), (exceeded || in_sample) && !was_logged))
    }
}

impl Session {
    /// An Execute message's "execute <statement>[/<portal>]: <query>" and its parameters'
    /// DETAIL (errdetail_params, log_parameter_max_length), for the server log.
    pub fn portal_log(&self, portal: &str) -> Option<(String, Option<String>, Arc<Prepared>)> {
        let p = self.portals.get(portal)?;
        let name = self
            .prepared
            .iter()
            .find(|(_, v)| Arc::ptr_eq(v, &p.stmt))
            .map_or("<unnamed>".to_string(), |(k, _)| if k.is_empty() { "<unnamed>".to_string() } else { k.clone() });
        let what = if portal.is_empty() { name } else { format!("{name}/{portal}") };
        let max = self.state.settings.borrow().int("log_parameter_max_length");
        let detail = (!p.params.is_empty() && max != 0).then(|| {
            let ctx = self.format_ctx();
            let items: Vec<String> = p
                .params
                .iter()
                .enumerate()
                .map(|(i, v)| {
                    let ty = p.stmt.param_types.get(i).cloned().unwrap_or(crate::types::DataType::Text);
                    match v {
                        crate::types::Value::Null => format!("${} = NULL", i + 1),
                        v => {
                            let t = crate::types::text::format_value(v, &ty, &ctx);
                            format!("${} = '{}'", i + 1, truncate_param(&t, max).replace('\'', "''"))
                        }
                    }
                })
                .collect();
            format!("Parameters: {}", items.join(", "))
        });
        Some((what, detail, p.stmt.clone()))
    }
}

/// A parameter's text cut to `max` bytes (at a character boundary), with "..." after
/// (log_parameter_max_length; -1: whole).
pub fn truncate_param(t: &str, max: i64) -> String {
    if max < 0 || t.len() as i64 <= max {
        return t.to_string();
    }
    let mut end = max as usize;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &t[..end])
}

/// A random number in [0, 1) (log_statement_sample_rate).
fn random_unit() -> f64 {
    let mut b = [0u8; 8];
    let _ = getrandom::fill(&mut b);
    (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
}

/// CreateCommandTag's tag of `n`, as the process title shows it while the statement runs.
fn ps_tag(n: &NodeEnum) -> String {
    use NodeEnum as N;
    use pg_query::protobuf::TransactionStmtKind as K;
    let t = match n {
        N::SelectStmt(_) => "SELECT",
        N::InsertStmt(_) => "INSERT",
        N::UpdateStmt(_) => "UPDATE",
        N::DeleteStmt(_) => "DELETE",
        N::MergeStmt(_) => "MERGE",
        N::DoStmt(_) => "DO",
        N::CallStmt(_) => "CALL",
        N::ExplainStmt(_) => "EXPLAIN",
        N::CopyStmt(_) => "COPY",
        N::VariableSetStmt(s) if s.kind == pg_query::protobuf::VariableSetKind::VarReset as i32 => "RESET",
        N::VariableSetStmt(_) => "SET",
        N::VariableShowStmt(_) => "SHOW",
        N::PrepareStmt(_) => "PREPARE",
        N::ExecuteStmt(_) => "EXECUTE",
        N::DeallocateStmt(_) => "DEALLOCATE",
        N::DeclareCursorStmt(_) => "DECLARE CURSOR",
        N::FetchStmt(f) if f.ismove => "MOVE",
        N::FetchStmt(_) => "FETCH",
        N::ClosePortalStmt(_) => "CLOSE CURSOR",
        N::LockStmt(_) => "LOCK TABLE",
        N::TruncateStmt(_) => "TRUNCATE TABLE",
        N::VacuumStmt(v) if !v.is_vacuumcmd => "ANALYZE",
        N::VacuumStmt(_) => "VACUUM",
        N::CheckPointStmt(_) => "CHECKPOINT",
        N::ListenStmt(_) => "LISTEN",
        N::UnlistenStmt(_) => "UNLISTEN",
        N::NotifyStmt(_) => "NOTIFY",
        N::LoadStmt(_) => "LOAD",
        N::DiscardStmt(_) => "DISCARD ALL",
        N::TransactionStmt(s) => match K::try_from(s.kind).unwrap_or(K::TransStmtBegin) {
            K::TransStmtBegin | K::TransStmtStart => "BEGIN",
            K::TransStmtCommit => "COMMIT",
            K::TransStmtRollback | K::TransStmtRollbackTo => "ROLLBACK",
            K::TransStmtSavepoint => "SAVEPOINT",
            K::TransStmtRelease => "RELEASE",
            K::TransStmtPrepare => "PREPARE TRANSACTION",
            K::TransStmtCommitPrepared => "COMMIT PREPARED",
            K::TransStmtRollbackPrepared => "ROLLBACK PREPARED",
            _ => "???",
        },
        n => return crate::session::ddl::event_trigger::event_tag(n).unwrap_or_else(|| "???".into()),
    };
    t.into()
}
