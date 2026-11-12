//! A connection's session: its keyspace and identity, and the request entry points (QUERY,
//! PREPARE, EXECUTE, BATCH).
//!
//! A QUERY is parsed, prepared and executed like an unprepared Cassandra statement; PREPARE
//! caches the prepared plan per database under `MD5(keyspace + query)`, so a statement prepared on
//! one connection executes on every other (and after reconnects).

use std::sync::Arc;

use bytes::Bytes;
use nosql_cql::ast::IntoStatic;

use crate::database::Database;
use crate::error::{Error, ErrorCode, Result};
use crate::messages as msg;
use crate::response::{
    Batch, ClientInfo, Param, Prepared as PreparedResult, QueryParams, Response,
};
use crate::statements::{self, Prepared};
use crate::trace;

/// Per-connection state visible to statements and hooks.
#[derive(Clone, Debug, Default)]
pub struct SessionState {
    /// The current keyspace (`USE`).
    pub keyspace: Option<String>,
    /// The authenticated role (`None`: anonymous).
    pub user: Option<String>,
    pub client: ClientInfo,
}

/// The outcome of an authentication step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthStep {
    /// More SASL rounds (`AUTH_CHALLENGE` with this token).
    Challenge(Bytes),
    /// Done (`AUTH_SUCCESS` with this token).
    Success(Option<Bytes>),
}

/// One connection to a database.
pub struct Session {
    db: Arc<Database>,
    pub state: SessionState,
    /// Trace the requests (the frame's TRACING flag).
    tracing: bool,
}

impl Session {
    pub(crate) fn new(db: Arc<Database>, client: ClientInfo) -> Self {
        Session {
            db,
            state: SessionState {
                client,
                ..Default::default()
            },
            tracing: false,
        }
    }

    /// A session continuing a connection's `state` (keyspace, role, client) — what a server
    /// that runs each request on its own session creates per request. Costs one `Arc` clone
    /// besides `state` (see `Database::session`).
    pub(crate) fn with_state(db: Arc<Database>, state: SessionState) -> Self {
        Session {
            db,
            state,
            tracing: false,
        }
    }

    /// Remembers the warnings of a request's outcome (`Database::recent_warnings`).
    fn note_warnings(&self, r: &Result<Response>, statement: impl FnOnce() -> String) {
        let warnings = match r {
            Ok(resp) => &resp.warnings,
            Err(e) => &e.warnings,
        };
        self.db
            .record_warnings(warnings, self.state.user.as_deref(), statement);
    }

    /// Traces the following requests into `system_traces` (the frame's TRACING flag); their
    /// responses carry the tracing session id (`Response::tracing_id`,
    /// `Prepared::tracing_id`). See `crate::trace`.
    pub fn set_tracing(&mut self, on: bool) {
        self.tracing = on;
    }

    pub fn database(&self) -> &Arc<Database> {
        &self.db
    }

    pub fn keyspace(&self) -> Option<&str> {
        self.state.keyspace.as_deref()
    }

    /// Whether the client must authenticate before issuing requests.
    pub fn needs_auth(&self) -> bool {
        crate::auth::requires_authentication(&self.db.cfg) && self.state.user.is_none()
    }

    /// One SASL step (`AUTH_RESPONSE` token): PLAIN against the database's roles with
    /// `PasswordAuthenticator` (see `crate::auth::authenticate`); `AllowAllAuthenticator`
    /// accepts anything as the anonymous user.
    pub async fn authenticate(&mut self, sasl: &[u8]) -> Result<AuthStep> {
        let r = crate::auth::authenticate(&self.db, &mut self.state, sasl).await;
        if crate::auth::requires_authentication(&self.db.cfg) && !matches!(r, Ok(AuthStep::Challenge(_))) {
            let user = match &r {
                Ok(_) => self.state.user.clone(),
                Err(_) => crate::audit::plain_user(sasl),
            };
            crate::audit::login(&self.db, &self.state, user.as_deref(), r.is_err()).await;
        }
        r
    }

    /// QUERY.
    pub async fn query(&mut self, text: &str, p: QueryParams) -> Result<Response> {
        let request = trace::Request::Query { text, params: &p };
        let mut t = trace::begin(&self.db, &self.state, self.tracing, request);
        let cl = p.consistency;
        let mut prepared = None;
        let r = self.query_traced(text, p, t.as_mut(), &mut prepared).await;
        let r = trace::finish(&self.db, t, r).await;
        self.note_warnings(&r, || text.to_string());
        match prepared {
            Some(p) => self.audit(&p, text, cl, &r).await,
            None => crate::audit::unprepared(&self.db, &self.state, text, cl, &r).await,
        }
        r
    }

    /// Audits a statement that ran (`crate::audit`).
    async fn audit(
        &self,
        p: &Prepared,
        text: &str,
        cl: crate::error::Consistency,
        r: &Result<Response>,
    ) {
        let ks = p.keyspace.as_deref();
        crate::audit::statement(&self.db, &self.state, &p.stmt, ks, text, cl, r).await;
    }

    async fn query_traced(
        &mut self,
        text: &str,
        p: QueryParams,
        mut t: Option<&mut trace::Trace>,
        out: &mut Option<Arc<Prepared>>,
    ) -> Result<Response> {
        if let Some(t) = t.as_deref_mut() {
            t.parsing(text);
        }
        let prepared = self.prepare_text(text, p.keyspace.as_deref(), false)?;
        *out = Some(prepared.clone());
        if let Some(t) = t.as_deref_mut() {
            t.prepared(false);
            t.executing(&self.db, &prepared);
            t.resuming(&prepared, p.paging_state.is_some());
        }
        check_values(&self.db, &prepared, &p)?;
        let values = order_values(&prepared, &p)?;
        let p = QueryParams { values, ..p };
        let r = statements::execute(&self.db, &mut self.state, &prepared, &p).await;
        if let Some(t) = t {
            t.executed(&self.db, &r);
        }
        r
    }

    /// PREPARE.
    pub async fn prepare(&mut self, text: &str, keyspace: Option<&str>) -> Result<PreparedResult> {
        let mut t = trace::begin(
            &self.db,
            &self.state,
            self.tracing,
            trace::Request::Prepare { text },
        );
        if let Some(t) = t.as_mut() {
            t.parsing(text);
        }
        let r = self.prepare_text(text, keyspace, true);
        if let (Some(t), Ok(_)) = (t.as_mut(), &r) {
            t.prepared(false);
        }
        let tracing_id = trace::end(&self.db, t, trace::response_size(None)).await;
        let prepared = r.map_err(|mut e| {
            e.tracing_id = tracing_id;
            e
        })?;
        let result = prepared.result.clone();
        let result_metadata_id = result
            .as_ref()
            .map(|r| r.id())
            .unwrap_or_else(|| crate::response::ResultMetadata::default().id());
        let out = PreparedResult {
            id: Bytes::copy_from_slice(&prepared.id),
            bind: prepared.bind.clone(),
            pk_indexes: prepared.pk_indexes.clone(),
            result,
            result_metadata_id,
            is_lwt: prepared.is_lwt,
            tracing_id,
        };
        self.db.cache_prepared(prepared);
        Ok(out)
    }

    /// EXECUTE.
    pub async fn execute(&mut self, id: &[u8], p: QueryParams) -> Result<Response> {
        let Some(prepared) = self.db.get_prepared(id) else {
            return Err(msg::unprepared(self.db.dialect(), id));
        };
        if !prepared.is_valid(&self.db.schema()) {
            return Err(msg::unprepared(self.db.dialect(), id));
        }
        let request = trace::Request::Execute {
            prepared: &prepared,
            params: &p,
        };
        let mut t = trace::begin(&self.db, &self.state, self.tracing, request);
        if let Some(t) = t.as_mut() {
            t.prepared(true);
            t.executing(&self.db, &prepared);
            t.resuming(&prepared, p.paging_state.is_some());
        }
        let cl = p.consistency;
        let r = async {
            check_values(&self.db, &prepared, &p)?;
            let values = order_values(&prepared, &p)?;
            let p = QueryParams { values, ..p };
            statements::execute(&self.db, &mut self.state, &prepared, &p).await
        }
        .await;
        if let Some(t) = t.as_mut() {
            t.executed(&self.db, &r);
        }
        let r = trace::finish(&self.db, t, r).await;
        self.note_warnings(&r, || prepared.text.clone());
        self.audit(&prepared, &prepared.text, cl, &r).await;
        r
    }

    /// BATCH.
    pub async fn batch(&mut self, b: Batch) -> Result<Response> {
        let mut t = trace::begin(
            &self.db,
            &self.state,
            self.tracing,
            trace::Request::Batch { batch: &b },
        );
        if let Some(t) = t.as_mut() {
            t.prepared(false);
            t.batch(&self.db, &b);
        }
        // What a warning names: the first statement (one allocation at most).
        let count = b.statements.len();
        let first = b.statements.first().map(|s| match s {
            crate::response::BatchStatement::Query { text, .. } => Ok(text.clone()),
            crate::response::BatchStatement::Prepared { id, .. } => Err(id.clone()),
        });
        let audited = crate::audit::batch_statements(&self.db, &self.state, &b);
        let cl = b.params.consistency;
        let r = statements::batch::execute_protocol_batch(&self.db, &mut self.state, b).await;
        for (stmt, ks, text) in &audited {
            crate::audit::statement(&self.db, &self.state, stmt, ks.as_deref(), text, cl, &r).await;
        }
        if let Some(t) = t.as_mut() {
            t.executed(&self.db, &r);
        }
        let r = trace::finish(&self.db, t, r).await;
        self.note_warnings(&r, || {
            let first = match first {
                Some(Ok(text)) => text,
                Some(Err(id)) => self
                    .db
                    .get_prepared(&id)
                    .map(|p| p.text.clone())
                    .unwrap_or_default(),
                None => String::new(),
            };
            format!("BATCH of {count}: {first}")
        });
        r
    }

    /// Parses and prepares `text` (statements that change schema are prepared lazily).
    pub(crate) fn prepare_text(
        &self,
        text: &str,
        keyspace: Option<&str>,
        for_prepare: bool,
    ) -> Result<Arc<Prepared>> {
        let d = self.db.dialect();
        let ks = keyspace
            .map(str::to_string)
            .or_else(|| self.state.keyspace.clone());
        if for_prepare {
            let id = statements::statement_id(text, ks.as_deref());
            if let Some(p) = self.db.get_prepared(&id)
                && p.is_valid(&self.db.schema())
                && p.text == text
            {
                return Ok(p);
            }
        }
        let stmt = parse(text, d)?;
        let prepared = statements::prepare(&self.db, &self.db.schema(), stmt, text, ks.as_deref())?;
        Ok(Arc::new(prepared))
    }
}

/// Parses one statement with the dialect's grammar and errors.
pub fn parse(text: &str, d: nosql_types::Dialect) -> Result<nosql_cql::Statement<'static>> {
    let cd = match d {
        nosql_types::Dialect::Scylla => nosql_cql::Dialect::Scylla,
        nosql_types::Dialect::Cassandra => nosql_cql::Dialect::Cassandra,
    };
    nosql_cql::parse_statement(text, cd)
        .map(IntoStatic::into_static)
        .map_err(|e| {
            let code = match e.code {
                nosql_cql::ErrorCode::Syntax => ErrorCode::Syntax,
                nosql_cql::ErrorCode::Invalid => ErrorCode::Invalid,
            };
            Error::new(code, e.message)
        })
}

fn check_values(db: &Database, p: &Prepared, params: &QueryParams) -> Result<()> {
    if params.values.len() != p.bind.len() {
        return Err(msg::invalid_bind_count(
            db.dialect(),
            p.bind.len(),
            params.values.len(),
        ));
    }
    Ok(())
}

/// Reorders named values (`names_for_values`) into marker order.
fn order_values(p: &Prepared, params: &QueryParams) -> Result<Vec<Param>> {
    let Some(names) = &params.names else {
        return Ok(params.values.clone());
    };
    let mut out = vec![Param::Unset; p.bind.len()];
    for (i, spec) in p.bind.iter().enumerate() {
        let pos = names
            .iter()
            .position(|n| n == &spec.name)
            .ok_or_else(|| Error::invalid(format!("No value for bind variable {}", spec.name)))?;
        out[i] = params.values.get(pos).cloned().unwrap_or(Param::Null);
    }
    Ok(out)
}
