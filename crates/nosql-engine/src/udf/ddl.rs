//! `CREATE [OR REPLACE] FUNCTION|AGGREGATE [IF NOT EXISTS]` and `DROP FUNCTION|AGGREGATE`.
//!
//! Ports Cassandra's `CreateFunctionStatement`, `CreateAggregateStatement`,
//! `DropFunctionStatement` and `DropAggregateStatement` (Apache-2.0), in their order: the
//! keyspace (`No keyspace has been specified`), authorization, the system-keyspace check, then
//! the statement's own checks. ScyllaDB answers a statement on a system keyspace before
//! authorization and everything else with "User defined functions are disabled" unless they are
//! enabled; with them enabled it runs Lua (and rejects other languages) under its own rules (the
//! "ScyllaDB" section), Cassandra only Java (stored, not run — see compatibility.md).
//!
//! A change rewrites the keyspace's `system_schema.functions` / `aggregates` (and ScyllaDB's
//! `scylla_aggregates`) rows through the core's `ddl::commit`, announces
//! `CREATED|UPDATED|DROPPED FUNCTION|AGGREGATE ks name [types]`, and evicts the prepared
//! statements that use a function of that name (Cassandra's
//! `QueryProcessor.removeInvalidPreparedStatementsForFunction`).

use std::sync::Arc;

use futures::future::BoxFuture;
use nosql_cql::ast::{
    self, CreateAggregate, CreateFunction, DropFunction, RawType, Statement, StatementKind,
};
use nosql_types::{CqlType, Dialect};

use super::function::{self, types_match};
use super::messages as m;
use crate::auth::Resource;
use crate::database::Database;
use crate::error::{Error, Result};
use crate::hooks::{Access, StatementHandler};
use crate::response::{ChangeTarget, ChangeType, QueryParams, Response, SchemaChange};
use crate::schema::ddl::Change;
use crate::schema::{AggregateMeta, FunctionMeta, Keyspace};
use crate::session::SessionState;

/// Runs the function and aggregate DDL.
pub struct Handler;

impl StatementHandler for Handler {
    fn handles(&self, stmt: &StatementKind<'_>) -> bool {
        matches!(
            stmt,
            StatementKind::CreateFunction(_)
                | StatementKind::DropFunction(_)
                | StatementKind::CreateAggregate(_)
                | StatementKind::DropAggregate(_)
        )
    }

    fn execute<'a>(
        &'a self,
        db: &'a Arc<Database>,
        session: &'a mut SessionState,
        stmt: &'a Statement<'static>,
        params: &'a QueryParams,
    ) -> BoxFuture<'a, Result<Response>> {
        Box::pin(async move {
            let current = params.keyspace.clone().or_else(|| session.keyspace.clone());
            let cx = Cx {
                db,
                session,
                d: db.dialect(),
                current: current.as_deref(),
            };
            match &stmt.kind {
                StatementKind::CreateFunction(c) => create_function(&cx, c).await,
                StatementKind::DropFunction(x) => drop(&cx, x, false).await,
                StatementKind::CreateAggregate(c) => create_aggregate(&cx, c).await,
                StatementKind::DropAggregate(x) => drop(&cx, x, true).await,
                other => Err(crate::hooks::unsupported(other)),
            }
        })
    }
}

struct Cx<'a> {
    db: &'a Arc<Database>,
    session: &'a SessionState,
    d: Dialect,
    current: Option<&'a str>,
}

impl Cx<'_> {
    fn keyspace(&self, name: &ast::FunctionName) -> Result<String> {
        match &name.keyspace {
            Some(k) => Ok(k.name().to_string()),
            None => self
                .current
                .map(str::to_string)
                .ok_or_else(crate::messages::no_keyspace),
        }
    }

    fn authorize(&self, access: &Access) -> Result<()> {
        self.db
            .hooks()
            .authorizer
            .authorize(self.db, self.session, access)
    }

    fn create_access(&self, ks: &str, statement: &'static str) -> Result<()> {
        self.authorize(&Access::Schema {
            keyspace: ks,
            table: "",
            statement,
        })
    }

    fn permission(&self, p: ast::Permission, r: Resource) -> Result<()> {
        self.authorize(&Access::Permission {
            permission: p,
            resource: r,
        })
    }

    fn enabled(&self) -> bool {
        self.db.config().settings.user_defined_functions_enabled
    }

    /// The system-keyspace check (ScyllaDB answers it before authorization, Cassandra after).
    fn system(&self, ks: &str) -> Result<()> {
        if crate::schema::ddl::is_system(ks) {
            return Err(m::system_keyspace(self.d, ks));
        }
        Ok(())
    }
}

/// A function's resource (`<function ks.f(int)>`).
pub fn resource(ks: &str, name: &str, types: &[CqlType], d: Dialect) -> Resource {
    Resource::Function {
        keyspace: ks.to_string(),
        name: name.to_string(),
        marshal: types
            .iter()
            .map(|t| nosql_types::marshal_name(t, false, d))
            .collect(),
        cql: types.iter().map(nosql_types::cql_name).collect(),
    }
}

/// `RawType.isFrozen() && !isImplicitlyFrozen()`.
fn explicitly_frozen(t: &RawType) -> bool {
    match t {
        RawType::Frozen(inner) => !matches!(**inner, RawType::Tuple(_) | RawType::Vector(_)),
        _ => false,
    }
}

/// A type as functions keep it: Cassandra's `udfType()` (counters are bigints); ScyllaDB's frozen
/// user types.
fn udf_type(t: CqlType, d: Dialect) -> CqlType {
    match d {
        Dialect::Cassandra if t.is_counter() => CqlType::Bigint,
        Dialect::Scylla => scylla_freeze_udt(t),
        _ => t,
    }
}

fn resolve(ks: &Keyspace, t: &RawType, d: Dialect) -> Result<CqlType> {
    crate::schema::types::resolve(t, &ks.name, &|n| ks.types.get(n).cloned(), d)
        .map(|t| udf_type(t, d))
}

fn type_text(t: &CqlType, d: Dialect) -> String {
    crate::schema::persist::type_text(t, d)
}

/// Resolved types of a user function's stored type texts.
fn meta_types(ks: &Keyspace, texts: &[String]) -> Option<Vec<CqlType>> {
    texts.iter().map(|t| function::parse_type(ks, t)).collect()
}

/// Whether a replacement's return type can read the old one's values
/// (`AbstractType.isCompatibleWith`: the same type, or text over ascii).
fn compatible(new: &CqlType, old: &CqlType) -> bool {
    types_match(std::slice::from_ref(new), std::slice::from_ref(old))
        || (matches!(new, CqlType::Text) && matches!(old, CqlType::Ascii))
}

fn changed(
    d: Dialect,
    created: bool,
    target: ChangeTarget,
    ks: &str,
    name: &str,
    types: Vec<String>,
) -> SchemaChange {
    let _ = d;
    SchemaChange {
        change: if created {
            ChangeType::Created
        } else {
            ChangeType::Updated
        },
        target,
        keyspace: ks.to_string(),
        name: Some(name.to_string()),
        arg_types: Some(types),
    }
}

/// `IF NOT EXISTS` on an existing function or aggregate: nothing changes (VOID, both
/// references).
fn unchanged() -> Response {
    Response::void()
}

async fn commit(cx: &Cx<'_>, ks: Keyspace, event: SchemaChange) -> Result<Response> {
    let schema = cx.db.schema();
    let name = event.name.clone().unwrap_or_default();
    let keyspace = ks.name.clone();
    let r = crate::schema::ddl::commit(
        cx.db,
        &schema,
        Change {
            keyspace: Some(ks),
            events: vec![event],
            ..Default::default()
        },
    )
    .await?;
    super::evict_prepared(cx.db, &keyspace, &name);
    Ok(r)
}

// ---- CREATE FUNCTION -------------------------------------------------------------------------

async fn create_function(cx: &Cx<'_>, c: &CreateFunction<'static>) -> Result<Response> {
    if cx.d.is_scylla() {
        return scylla_create_function(cx, c).await;
    }
    let d = cx.d;
    let ks_name = cx.keyspace(&c.name)?;
    let name = c.name.resolved().to_string();
    let _g = cx.db.ddl.lock().await;
    let schema = cx.db.schema();
    let keyspace = schema.keyspace(&ks_name).cloned();
    // Authorization: ALTER on the function OR REPLACE replaces, else CREATE in its keyspace.
    let existing_types = keyspace.as_ref().and_then(|k| {
        let types = c
            .args
            .iter()
            .map(|(_, t)| resolve(k, t, d).ok())
            .collect::<Option<Vec<_>>>()?;
        let found = k.functions.iter().any(|f| {
            f.name == name
                && meta_types(k, &f.argument_types).is_some_and(|ft| types_match(&ft, &types))
        });
        found.then_some(types)
    });
    match (&existing_types, c.or_replace) {
        (Some(types), true) => {
            cx.permission(ast::Permission::Alter, resource(&ks_name, &name, types, d))?
        }
        _ => cx.create_access(&ks_name, "CREATE FUNCTION")?,
    }
    cx.system(&ks_name)?;
    // The directives are checked first (`apply`).
    if !cx.enabled() && !(c.or_replace && c.if_not_exists) {
        return Err(m::disabled(d));
    }
    if c.or_replace && c.if_not_exists {
        return Err(m::both_directives());
    }
    if c.language != "java" {
        return Err(m::language(d, &c.language));
    }
    if name.contains(['/', '[', ']']) {
        return Err(m::invalid_name("Function", &name));
    }
    let arg_names: Vec<String> = c.args.iter().map(|(n, _)| n.name().to_string()).collect();
    let mut seen = std::collections::HashSet::new();
    if !arg_names.iter().all(|n| seen.insert(n.as_str())) {
        return Err(m::duplicate_arguments(&name, &arg_names));
    }
    if let Some((_, t)) = c.args.iter().find(|(_, t)| explicitly_frozen(t)) {
        return Err(m::frozen("Argument", &t.to_string()));
    }
    if explicitly_frozen(&c.return_type) {
        return Err(m::frozen("Return type", &c.return_type.to_string()));
    }
    let Some(keyspace) = keyspace else {
        return Err(m::keyspace_missing(&ks_name));
    };
    let types = c
        .args
        .iter()
        .map(|(_, t)| resolve(&keyspace, t, d))
        .collect::<Result<Vec<_>>>()?;
    let ret = resolve(&keyspace, &c.return_type, d)?;
    let body = c.body.text.to_string();
    let raw_types: Vec<String> = c.args.iter().map(|(_, t)| t.to_string()).collect();
    let mut created = true;
    if let Some(agg) = keyspace.aggregates.iter().find(|a| {
        a.name == name
            && meta_types(&keyspace, &a.argument_types).is_some_and(|at| types_match(&at, &types))
    }) {
        let _ = agg;
        return Err(m::function_replaces_aggregate(&name));
    }
    let position = keyspace.functions.iter().position(|f| {
        f.name == name
            && meta_types(&keyspace, &f.argument_types).is_some_and(|ft| types_match(&ft, &types))
    });
    if let Some(i) = position {
        if c.if_not_exists {
            return Ok(unchanged());
        }
        if !c.or_replace {
            return Err(m::function_exists(&name));
        }
        let old = &keyspace.functions[i];
        if old.called_on_null_input != c.called_on_null_input {
            return Err(m::null_input_directive(&name, c.called_on_null_input));
        }
        let old_ret = function::parse_type(&keyspace, &old.return_type);
        if let Some(old_ret) = old_ret
            && !compatible(&ret, &old_ret)
        {
            return Err(m::replace_return_type(
                &name,
                &nosql_types::cql_name(&ret),
                &nosql_types::cql_name(&old_ret),
            ));
        }
        created = false;
    }
    let meta = FunctionMeta {
        name: name.clone(),
        argument_types: types.iter().map(|t| type_text(t, d)).collect(),
        argument_names: arg_names,
        body,
        called_on_null_input: c.called_on_null_input,
        language: c.language.to_string(),
        return_type: type_text(&ret, d),
    };
    // Cassandra: replacing with the same definition changes nothing (VOID).
    if let Some(i) = position
        && keyspace.functions[i] == meta
    {
        return Ok(unchanged());
    }
    let mut k = (*keyspace).clone();
    match position {
        Some(i) => k.functions[i] = meta,
        None => k.functions.push(meta),
    }
    let event = changed(
        d,
        created,
        ChangeTarget::Function,
        &ks_name,
        &name,
        raw_types,
    );
    commit(cx, k, event).await
}

// ---- CREATE AGGREGATE ------------------------------------------------------------------------

/// A user function (or aggregate) of `ks` named `name` whose types match.
enum Found<'k> {
    Scalar(&'k FunctionMeta),
    Aggregate,
    None,
}

fn find<'k>(ks: &'k Keyspace, name: &str, types: &[CqlType]) -> Found<'k> {
    if let Some(f) = ks.functions.iter().find(|f| {
        f.name == name
            && meta_types(ks, &f.argument_types).is_some_and(|ft| types_match(&ft, types))
    }) {
        return Found::Scalar(f);
    }
    if ks.aggregates.iter().any(|a| {
        a.name == name
            && meta_types(ks, &a.argument_types).is_some_and(|at| types_match(&at, types))
    }) {
        return Found::Aggregate;
    }
    Found::None
}

/// `name(t1, t2)` as messages show a sub-function: Cassandra qualifies it with the keyspace.
fn shown(d: Dialect, ks: &str, name: &str, types: &[String]) -> String {
    match d {
        Dialect::Scylla => format!("{name}({})", types.join(", ")),
        Dialect::Cassandra => format!("{ks}.{name}({})", types.join(", ")),
    }
}

async fn create_aggregate(cx: &Cx<'_>, c: &CreateAggregate<'static>) -> Result<Response> {
    if cx.d.is_scylla() {
        return scylla_create_aggregate(cx, c).await;
    }
    let d = cx.d;
    let ks_name = cx.keyspace(&c.name)?;
    let name = c.name.resolved().to_string();
    if d.is_scylla() {
        cx.system(&ks_name)?;
    }
    let _g = cx.db.ddl.lock().await;
    let schema = cx.db.schema();
    let keyspace = schema.keyspace(&ks_name).cloned();
    let sfunc = c.state_function.resolved().to_string();
    let ffunc = c.final_function.as_ref().map(|f| f.resolved().to_string());
    let rfunc = c.reduce_function.as_ref().map(|f| f.resolved().to_string());
    // Authorization (`CreateAggregateStatement.authorize`): ALTER on a replaced aggregate, else
    // CREATE in the keyspace; then EXECUTE on the state and final functions, whose argument types
    // are resolved here (a missing keyspace is a configuration error at this point).
    let existing = keyspace.as_ref().is_some_and(|k| {
        c.arg_types
            .iter()
            .map(|t| resolve(k, t, d).ok())
            .collect::<Option<Vec<_>>>()
            .is_some_and(|types| {
                k.aggregates.iter().any(|a| {
                    a.name == name
                        && meta_types(k, &a.argument_types)
                            .is_some_and(|at| types_match(&at, &types))
                })
            })
    });
    if existing && c.or_replace {
        let k = keyspace
            .as_ref()
            .ok_or_else(|| m::keyspace_missing(&ks_name))?;
        let types = c
            .arg_types
            .iter()
            .map(|t| resolve(k, t, d))
            .collect::<Result<Vec<_>>>()?;
        cx.permission(ast::Permission::Alter, resource(&ks_name, &name, &types, d))?;
    } else {
        cx.create_access(&ks_name, "CREATE AGGREGATE")?;
    }
    {
        let Some(k) = keyspace.as_ref() else {
            return Err(m::keyspace_missing_config(&ks_name));
        };
        let stype = resolve(k, &c.state_type, d)?;
        let mut sargs = vec![stype.clone()];
        for t in &c.arg_types {
            sargs.push(resolve(k, t, d)?);
        }
        sub_function_permission(cx, k, &sfunc, &sargs)?;
        if let Some(f) = &ffunc {
            sub_function_permission(cx, k, f, std::slice::from_ref(&stype))?;
        }
    }
    cx.system(&ks_name)?;
    if c.or_replace && c.if_not_exists {
        return Err(m::both_directives());
    }
    if name.contains(['/', '[', ']']) {
        return Err(m::invalid_name("Aggregate", &name));
    }
    if let Some(t) = c.arg_types.iter().find(|t| explicitly_frozen(t)) {
        return Err(m::frozen("Argument", &t.to_string()));
    }
    if explicitly_frozen(&c.state_type) {
        return Err(m::frozen("State type", &c.state_type.to_string()));
    }
    let Some(keyspace) = keyspace else {
        return Err(m::keyspace_missing(&ks_name));
    };
    let types = c
        .arg_types
        .iter()
        .map(|t| resolve(&keyspace, t, d))
        .collect::<Result<Vec<_>>>()?;
    let stype = resolve(&keyspace, &c.state_type, d)?;
    let raw_stype = c.state_type.to_string();
    let mut sraw = vec![raw_stype.clone()];
    sraw.extend(c.arg_types.iter().map(|t| t.to_string()));
    let mut sargs = vec![stype.clone()];
    sargs.extend(types.iter().cloned());
    let state = match find(&keyspace, &sfunc, &sargs) {
        Found::Scalar(f) => f,
        Found::Aggregate => {
            return Err(m::not_scalar("State", &shown(d, &ks_name, &sfunc, &sraw)));
        }
        Found::None => {
            return Err(m::state_function_missing(
                d,
                &shown(d, &ks_name, &sfunc, &sraw),
            ));
        }
    };
    let state_ret = function::parse_type(&keyspace, &state.return_type);
    if state_ret.as_ref() != Some(&stype) {
        return Err(m::state_return_type(&shown(d, &ks_name, &sfunc, &sraw)));
    }
    let mut ret = stype.clone();
    if let Some(f) = &ffunc {
        let fraw = std::slice::from_ref(&raw_stype);
        match find(&keyspace, f, std::slice::from_ref(&stype)) {
            Found::Scalar(meta) => {
                ret = function::parse_type(&keyspace, &meta.return_type)
                    .ok_or_else(|| Error::server(format!("invalid return type of {f}")))?;
            }
            Found::Aggregate => {
                return Err(m::not_scalar("Final", &shown(d, &ks_name, f, fraw)));
            }
            Found::None => {
                return Err(m::final_function_missing(d, &shown(d, &ks_name, f, fraw)));
            }
        }
    }
    // INITCOND: typed as the state (`Terms.asBytes` with a `(dummy)` receiver), validated,
    // stored as its CQL literal.
    let mut initcond: Option<String> = None;
    if let Some(t) = &c.initial_condition {
        let is_null = matches!(t, ast::Term::Null);
        let bytes = initcond_value(cx, &keyspace, t, &stype)?;
        if let Some(b) = &bytes
            && nosql_types::validate(&stype, b, d).is_err()
        {
            return Err(m::initcond_invalid(&nosql_types::cql_name(&stype)));
        }
        let empty = bytes.as_ref().is_none_or(|b| {
            b.is_empty() && !matches!(stype, CqlType::Text | CqlType::Ascii | CqlType::Blob)
        });
        if !is_null && empty {
            return Err(m::initcond_empty());
        }
        initcond = match &bytes {
            Some(b) => Some(
                nosql_types::to_cql_literal(&stype, b, d)
                    .map_err(|e| Error::invalid(e.message.to_string()))?,
            ),
            None => None,
        };
    }
    if !state.called_on_null_input && initcond.is_none() {
        return Err(m::initcond_required(&name, &format!("{ks_name}.{sfunc}")));
    }
    let raw_types: Vec<String> = c.arg_types.iter().map(|t| t.to_string()).collect();
    if keyspace.functions.iter().any(|f| {
        f.name == name
            && meta_types(&keyspace, &f.argument_types).is_some_and(|ft| types_match(&ft, &types))
    }) {
        return Err(m::aggregate_replaces_function(&name));
    }
    let position = keyspace.aggregates.iter().position(|a| {
        a.name == name
            && meta_types(&keyspace, &a.argument_types).is_some_and(|at| types_match(&at, &types))
    });
    let mut created = true;
    if let Some(i) = position {
        if c.if_not_exists {
            return Ok(unchanged());
        }
        if !c.or_replace {
            return Err(m::aggregate_exists(&name));
        }
        if let Some(old) = function::parse_type(&keyspace, &keyspace.aggregates[i].return_type)
            && !compatible(&ret, &old)
        {
            return Err(m::replace_aggregate_return_type(
                &name,
                &nosql_types::cql_name(&ret),
                &nosql_types::cql_name(&old),
            ));
        }
        created = false;
    }
    let meta = AggregateMeta {
        name: name.clone(),
        argument_types: types.iter().map(|t| type_text(t, d)).collect(),
        final_func: ffunc,
        initcond,
        return_type: type_text(&ret, d),
        state_func: sfunc,
        state_type: type_text(&stype, d),
        reduce_func: rfunc,
    };
    // Cassandra: replacing with the same definition changes nothing (VOID).
    if let Some(i) = position
        && keyspace.aggregates[i] == meta
    {
        return Ok(unchanged());
    }
    let mut k = (*keyspace).clone();
    match position {
        Some(i) => k.aggregates[i] = meta,
        None => k.aggregates.push(meta),
    }
    let event = changed(
        d,
        created,
        ChangeTarget::Aggregate,
        &ks_name,
        &name,
        raw_types,
    );
    commit(cx, k, event).await
}

/// EXECUTE on a sub-function of a new aggregate. With an enforcing authorizer, Cassandra fails an
/// assertion when the function doesn't exist (its resource has no applicable permissions).
fn sub_function_permission(cx: &Cx, ks: &Keyspace, name: &str, types: &[CqlType]) -> Result<()> {
    // `ClientState.ensurePermission`: functions of `system` are unrestricted.
    if ks.name == "system" {
        return Ok(());
    }
    let r = resource(&ks.name, name, types, cx.d);
    if crate::auth::authorizer::enforcing(cx.db) && matches!(find(ks, name, types), Found::None) {
        return Err(crate::auth::messages::cassandra_missing_function(
            &r.display(),
        ));
    }
    cx.permission(ast::Permission::Execute, r)
}

fn initcond_value(
    cx: &Cx,
    ks: &Keyspace,
    t: &ast::Term<'static>,
    ty: &CqlType,
) -> Result<Option<bytes::Bytes>> {
    if contains_marker(t) {
        return Err(m::initcond_bind(cx.d));
    }
    let schema = cx.db.schema();
    let mut pcx = crate::prepare::PrepareCtx::new(cx.d, &schema, &ks.name, &cx.db.functions, &[]);
    // `Terms.asBytes`' receiver: Cassandra's `(dummy)`, ScyllaDB's unnamed one.
    let receiver = match cx.d {
        Dialect::Scylla => "",
        Dialect::Cassandra => "(dummy)",
    };
    let term = crate::prepare::terms::prepare(
        t,
        &crate::prepare::Receiver::new(&ks.name, "", receiver, ty.clone()),
        &mut pcx,
    )?;
    term.eval_const(cx.d)
}

fn contains_marker(t: &ast::Term) -> bool {
    use ast::Term as T;
    match t {
        T::Bind(_) => true,
        T::List(v) | T::Set(v) | T::Tuple(v) => v.iter().any(contains_marker),
        T::Map(v) => v
            .iter()
            .any(|(k, x)| contains_marker(k) || contains_marker(x)),
        T::Udt(v) => v.iter().any(|(_, x)| contains_marker(x)),
        T::Function(f) => f.args.iter().any(contains_marker),
        T::Cast(c) => contains_marker(&c.arg),
        T::TypeHint(h) => contains_marker(&h.term),
        T::Binary(b) => contains_marker(&b.left) || contains_marker(&b.right),
        T::Negate(x) => contains_marker(x),
        _ => false,
    }
}

// ---- DROP FUNCTION / AGGREGATE ---------------------------------------------------------------

async fn drop(cx: &Cx<'_>, x: &DropFunction<'static>, aggregate: bool) -> Result<Response> {
    if cx.d.is_scylla() {
        return scylla_drop(cx, x, aggregate).await;
    }
    let d = cx.d;
    let ks_name = cx.keyspace(&x.name)?;
    let name = x.name.resolved().to_string();
    let _g = cx.db.ddl.lock().await;
    let schema = cx.db.schema();
    let keyspace = schema.keyspace(&ks_name).cloned();
    let raw_args: Option<Vec<String>> = x
        .arg_types
        .as_ref()
        .map(|v| v.iter().map(|t| t.to_string()).collect());
    // Authorization: DROP on every overload the statement names.
    if let Some(k) = &keyspace {
        let wanted: Option<Vec<CqlType>> = match &x.arg_types {
            Some(v) => v.iter().map(|t| resolve(k, t, d).ok()).collect(),
            None => None,
        };
        let overloads: Vec<Vec<String>> = if aggregate {
            k.aggregates
                .iter()
                .filter(|a| a.name == name)
                .map(|a| a.argument_types.clone())
                .collect()
        } else {
            k.functions
                .iter()
                .filter(|f| f.name == name)
                .map(|f| f.argument_types.clone())
                .collect()
        };
        for texts in overloads {
            let Some(types) = meta_types(k, &texts) else {
                continue;
            };
            if let Some(w) = &wanted
                && !types_match(&types, w)
            {
                continue;
            }
            if x.arg_types.is_some() && wanted.is_none() {
                continue;
            }
            cx.permission(ast::Permission::Drop, resource(&ks_name, &name, &types, d))?;
        }
    }
    // ScyllaDB answers "disabled" even for a system keyspace (probed); its order past that is
    // Cassandra's here.
    if d.is_scylla() && !cx.enabled() {
        return Err(m::disabled(d));
    }
    cx.system(&ks_name)?;
    let shown = match &raw_args {
        Some(a) => format!("{ks_name}.{name}({})", a.join(", ")),
        None => format!("{ks_name}.{name}"),
    };
    let missing = || {
        if aggregate {
            m::aggregate_missing(&shown)
        } else {
            m::function_missing(&shown)
        }
    };
    let Some(keyspace) = keyspace else {
        if x.if_exists {
            return Ok(Response::void());
        }
        return Err(missing());
    };
    let same_name = keyspace.functions.iter().filter(|f| f.name == name).count()
        + keyspace
            .aggregates
            .iter()
            .filter(|a| a.name == name)
            .count();
    if same_name > 1 && x.arg_types.is_none() {
        return Err(m::ambiguous_drop(
            if aggregate { "AGGREGATE" } else { "FUNCTION" },
            &name,
        ));
    }
    if let Some(t) = x.arg_types.iter().flatten().find(|t| explicitly_frozen(t)) {
        return Err(m::frozen("Argument", &t.to_string()));
    }
    let wanted: Option<Vec<CqlType>> = match &x.arg_types {
        Some(v) => Some(
            v.iter()
                .map(|t| resolve(&keyspace, t, d))
                .collect::<Result<Vec<_>>>()?,
        ),
        None => None,
    };
    let matches = |texts: &[String]| {
        wanted.as_ref().is_none_or(|w| {
            meta_types(&keyspace, texts).is_some_and(|types| types_match(&types, w))
        })
    };
    let mut k = (*keyspace).clone();
    let (types, target) = if aggregate {
        let Some(i) = k
            .aggregates
            .iter()
            .position(|a| a.name == name && matches(&a.argument_types))
        else {
            if x.if_exists {
                return Ok(Response::void());
            }
            return Err(missing());
        };
        let a = k.aggregates.remove(i);
        (a.argument_types, ChangeTarget::Aggregate)
    } else {
        let Some(i) = k
            .functions
            .iter()
            .position(|f| f.name == name && matches(&f.argument_types))
        else {
            if x.if_exists {
                return Ok(Response::void());
            }
            return Err(missing());
        };
        let f = &k.functions[i];
        let ftypes = meta_types(&keyspace, &f.argument_types).unwrap_or_default();
        let users: Vec<String> = keyspace
            .aggregates
            .iter()
            .filter(|a| aggregate_uses(&keyspace, a, &name, &ftypes))
            .map(|a| format!("{ks_name}.{}", a.name))
            .collect();
        if !users.is_empty() {
            return Err(m::used_by_aggregates(&shown, &users));
        }
        let tables: Vec<String> = keyspace
            .tables
            .values()
            // `KeyspaceMetadata.tablesUsingFunction`: tables, not views (whose masks follow them).
            .filter(|t| {
                !t.is_view()
                    && t.masks.iter().any(|(column, mk)| {
                        mk.function_keyspace == ks_name
                            && mk.function_name == name
                            && t.column(column).is_some_and(|c| {
                                mask_types_match(&keyspace, &c.ty, &mk.arg_types, &ftypes)
                            })
                    })
            })
            .map(|t| t.name.clone())
            .collect();
        if !tables.is_empty() {
            return Err(m::used_by_masks(&shown, &tables));
        }
        let f = k.functions.remove(i);
        (f.argument_types, ChangeTarget::Function)
    };
    let event = SchemaChange {
        change: ChangeType::Dropped,
        target,
        keyspace: ks_name.clone(),
        name: Some(name.clone()),
        arg_types: Some(types),
    };
    commit(cx, k, event).await
}

// ---- ScyllaDB ---------------------------------------------------------------------------------
//
// ScyllaDB's own rules (probed on `scylladb/scylla:2026.3.3` with Lua UDFs): duplicate argument
// names, frozen collections, any name and OR REPLACE with another null-input directive or return
// type are fine; a signature taken by a function or aggregate is `The function '…' already
// exists`; aggregates need no INITCOND and don't check the reduce function's return type.

/// How ScyllaDB shows an existing function (`ks.f : (int) -> int`) or aggregate (`ks.a`).
fn scylla_shown(ks: &Keyspace, found: &Found) -> String {
    match found {
        Found::Scalar(f) => {
            let types: Vec<String> = meta_types(ks, &f.argument_types)
                .unwrap_or_default()
                .iter()
                .map(nosql_types::cql_name)
                .collect();
            let ret = function::parse_type(ks, &f.return_type)
                .map(|t| nosql_types::cql_name(&t))
                .unwrap_or_default();
            m::function_shown(&ks.name, &f.name, &types, &ret)
        }
        Found::Aggregate | Found::None => String::new(),
    }
}

/// The aggregate (or function) of `ks` named `name` whose types match, with its display.
fn scylla_find<'k>(ks: &'k Keyspace, name: &str, types: &[CqlType]) -> (Found<'k>, String) {
    let found = find(ks, name, types);
    let shown = match &found {
        Found::Aggregate => format!("{}.{name}", ks.name),
        other => scylla_shown(ks, other),
    };
    (found, shown)
}

/// Resolves types in `ks` (or, when it doesn't exist, without user types).
fn resolve_scylla(ks: Option<&Keyspace>, ks_name: &str, t: &RawType) -> Result<CqlType> {
    match ks {
        Some(k) => resolve(k, t, Dialect::Scylla),
        None => crate::schema::types::resolve(t, ks_name, &|_| None, Dialect::Scylla)
            .map(scylla_freeze_udt),
    }
}

/// ScyllaDB keeps user-type arguments and results frozen (`frozen<u>` in
/// `system_schema.functions`, permissions and signatures).
pub fn scylla_freeze_udt(t: CqlType) -> CqlType {
    match t {
        CqlType::Udt(def, _) => CqlType::Udt(def, true),
        other => other,
    }
}

/// `frozen<udt>` as an argument or return type.
fn frozen_udt(t: &RawType, ty: &CqlType) -> bool {
    explicitly_frozen(t) && matches!(ty, CqlType::Udt(..))
}

async fn scylla_create_function(cx: &Cx<'_>, c: &CreateFunction<'static>) -> Result<Response> {
    let d = cx.d;
    let ks_name = cx.keyspace(&c.name)?;
    let name = c.name.resolved().to_string();
    cx.system(&ks_name)?;
    let _g = cx.db.ddl.lock().await;
    let schema = cx.db.schema();
    let keyspace = schema.keyspace(&ks_name).cloned();
    let existing_types = keyspace.as_ref().and_then(|k| {
        let types = c
            .args
            .iter()
            .map(|(_, t)| resolve(k, t, d).ok())
            .collect::<Option<Vec<_>>>()?;
        matches!(find(k, &name, &types), Found::Scalar(_)).then_some(types)
    });
    // ScyllaDB: replacing an existing function needs ALTER on it (checked first) and CREATE on
    // the keyspace's functions; anything else needs CREATE.
    if let (Some(types), true) = (&existing_types, c.or_replace) {
        cx.permission(ast::Permission::Alter, resource(&ks_name, &name, types, d))?;
    }
    cx.create_access(&ks_name, "CREATE FUNCTION")?;
    if !cx.enabled() {
        return Err(m::disabled(d));
    }
    match c.language.as_ref() {
        "lua" => {}
        "wasm" => {
            super::wasm::compile(&name, &c.body.text).map_err(Error::invalid)?;
        }
        other => return Err(m::language(d, other)),
    }
    let Some(keyspace) = keyspace else {
        return Err(m::keyspace_missing_scylla(&ks_name));
    };
    let mut types = Vec::with_capacity(c.args.len());
    for (_, t) in &c.args {
        let ty = resolve(&keyspace, t, d)?;
        if frozen_udt(t, &ty) {
            return Err(m::frozen_udt());
        }
        types.push(ty);
    }
    let ret = resolve(&keyspace, &c.return_type, d)?;
    if frozen_udt(&c.return_type, &ret) {
        return Err(m::frozen_udt());
    }
    let arg_names: Vec<String> = c.args.iter().map(|(n, _)| n.name().to_string()).collect();
    let body = c.body.text.to_string();
    if c.language.as_ref() == "lua" {
        super::lua::compile(&arg_names, &body).map_err(|e| m::lua_compile(&e))?;
    }
    let raw_types: Vec<String> = c.args.iter().map(|(_, t)| t.to_string()).collect();
    let (found, shown) = scylla_find(&keyspace, &name, &types);
    let position = match found {
        Found::Aggregate if c.or_replace => return Err(m::cannot_replace(&shown, "function")),
        Found::Aggregate | Found::Scalar(_) if c.if_not_exists && !c.or_replace => {
            return Ok(unchanged());
        }
        Found::Aggregate => return Err(m::exists_scylla(&shown)),
        Found::Scalar(_) if !c.or_replace => return Err(m::exists_scylla(&shown)),
        Found::Scalar(_) => keyspace.functions.iter().position(|f| {
            f.name == name
                && meta_types(&keyspace, &f.argument_types)
                    .is_some_and(|ft| types_match(&ft, &types))
        }),
        Found::None => None,
    };
    let meta = FunctionMeta {
        name: name.clone(),
        argument_types: types.iter().map(|t| type_text(t, d)).collect(),
        argument_names: arg_names,
        body,
        called_on_null_input: c.called_on_null_input,
        language: c.language.to_string(),
        return_type: type_text(&ret, d),
    };
    let mut k = (*keyspace).clone();
    match position {
        Some(i) => k.functions[i] = meta,
        None => k.functions.push(meta),
    }
    let event = changed(
        d,
        position.is_none(),
        ChangeTarget::Function,
        &ks_name,
        &name,
        raw_types,
    );
    commit(cx, k, event).await
}

async fn scylla_create_aggregate(cx: &Cx<'_>, c: &CreateAggregate<'static>) -> Result<Response> {
    let d = cx.d;
    let ks_name = cx.keyspace(&c.name)?;
    let name = c.name.resolved().to_string();
    cx.system(&ks_name)?;
    let _g = cx.db.ddl.lock().await;
    let schema = cx.db.schema();
    let keyspace = schema.keyspace(&ks_name).cloned();
    let kref = keyspace.as_deref();
    let existing = kref.is_some_and(|k| {
        c.arg_types
            .iter()
            .map(|t| resolve(k, t, d).ok())
            .collect::<Option<Vec<_>>>()
            .is_some_and(|types| matches!(find(k, &name, &types), Found::Aggregate))
    });
    if existing && c.or_replace {
        let types = c
            .arg_types
            .iter()
            .map(|t| resolve_scylla(kref, &ks_name, t))
            .collect::<Result<Vec<_>>>()?;
        cx.permission(ast::Permission::Alter, resource(&ks_name, &name, &types, d))?;
    }
    cx.create_access(&ks_name, "CREATE AGGREGATE")?;
    if !cx.enabled() {
        return Err(m::disabled(d));
    }
    let types = c
        .arg_types
        .iter()
        .map(|t| resolve_scylla(kref, &ks_name, t))
        .collect::<Result<Vec<_>>>()?;
    let stype = resolve_scylla(kref, &ks_name, &c.state_type)?;
    // EXECUTE on the state, reduce and final functions that exist (a missing one is reported
    // below).
    if let Some(k) = kref {
        let mut sargs = vec![stype.clone()];
        sargs.extend(types.iter().cloned());
        let pair = [stype.clone(), stype.clone()];
        let subs = [
            Some((c.state_function.resolved().to_string(), &sargs[..])),
            c.reduce_function
                .as_ref()
                .map(|f| (f.resolved().to_string(), &pair[..])),
            c.final_function
                .as_ref()
                .map(|f| (f.resolved().to_string(), std::slice::from_ref(&stype))),
        ];
        for (f, ts) in subs.into_iter().flatten() {
            if let Found::Scalar(meta) = find(k, &f, ts)
                && let Some(mt) = meta_types(k, &meta.argument_types)
            {
                cx.permission(ast::Permission::Execute, resource(&ks_name, &f, &mt, d))?;
            }
        }
    }
    let empty = Keyspace::new(
        ks_name.clone(),
        crate::schema::replication::Replication::new("LocalStrategy", &[]),
        crate::schema::KeyspaceKind::User,
    );
    let ks = kref.unwrap_or(&empty);
    let (found, shown) = scylla_find(ks, &name, &types);
    let position = match found {
        Found::Scalar(_) if c.or_replace => return Err(m::cannot_replace(&shown, "aggregate")),
        Found::Aggregate | Found::Scalar(_) if c.if_not_exists && !c.or_replace => {
            return Ok(unchanged());
        }
        Found::Scalar(_) => return Err(m::exists_scylla(&shown)),
        Found::Aggregate if !c.or_replace => return Err(m::exists_scylla(&shown)),
        Found::Aggregate => ks.aggregates.iter().position(|a| {
            a.name == name
                && meta_types(ks, &a.argument_types).is_some_and(|at| types_match(&at, &types))
        }),
        Found::None => None,
    };
    let cql = |ts: &[CqlType]| -> Vec<String> { ts.iter().map(nosql_types::cql_name).collect() };
    let sfunc = c.state_function.resolved().to_string();
    let mut sargs = vec![stype.clone()];
    sargs.extend(types.iter().cloned());
    let Found::Scalar(state) = find(ks, &sfunc, &sargs) else {
        return Err(m::state_function_missing(
            d,
            &format!("{sfunc}({})", cql(&sargs).join(", ")),
        ));
    };
    if function::parse_type(ks, &state.return_type).as_ref() != Some(&stype) {
        return Err(m::state_not_returned(
            &sfunc,
            &nosql_types::cql_name(&stype),
        ));
    }
    let mut ret = stype.clone();
    let ffunc = c.final_function.as_ref().map(|f| f.resolved().to_string());
    if let Some(f) = &ffunc {
        let Found::Scalar(fin) = find(ks, f, std::slice::from_ref(&stype)) else {
            return Err(m::final_function_missing(
                d,
                &format!("{f}({})", nosql_types::cql_name(&stype)),
            ));
        };
        ret = function::parse_type(ks, &fin.return_type)
            .ok_or_else(|| Error::server(format!("invalid return type of {f}")))?;
    }
    let rfunc = c.reduce_function.as_ref().map(|f| f.resolved().to_string());
    if let Some(r) = &rfunc
        && !matches!(
            find(ks, r, &[stype.clone(), stype.clone()]),
            Found::Scalar(_)
        )
    {
        return Err(m::reduce_function_missing(
            r,
            &nosql_types::marshal_name(&stype, false, d),
        ));
    }
    let mut initcond: Option<String> = None;
    if let Some(t) = &c.initial_condition {
        let bytes = initcond_value(cx, ks, t, &stype)?;
        initcond = match &bytes {
            Some(b) => Some(
                nosql_types::to_cql_literal(&stype, b, d)
                    .map_err(|e| Error::invalid(e.message.to_string()))?,
            ),
            None => None,
        };
    }
    let Some(keyspace) = keyspace else {
        return Err(m::keyspace_missing_scylla(&ks_name));
    };
    let meta = AggregateMeta {
        name: name.clone(),
        argument_types: types.iter().map(|t| type_text(t, d)).collect(),
        final_func: ffunc,
        initcond,
        return_type: type_text(&ret, d),
        state_func: sfunc,
        state_type: type_text(&stype, d),
        reduce_func: rfunc,
    };
    let mut k = (*keyspace).clone();
    match position {
        Some(i) => k.aggregates[i] = meta,
        None => k.aggregates.push(meta),
    }
    let raw_types: Vec<String> = c.arg_types.iter().map(|t| t.to_string()).collect();
    let event = changed(
        d,
        position.is_none(),
        ChangeTarget::Aggregate,
        &ks_name,
        &name,
        raw_types,
    );
    commit(cx, k, event).await
}

async fn scylla_drop(cx: &Cx<'_>, x: &DropFunction<'static>, aggregate: bool) -> Result<Response> {
    let d = cx.d;
    let ks_name = cx.keyspace(&x.name)?;
    let name = x.name.resolved().to_string();
    let _g = cx.db.ddl.lock().await;
    let schema = cx.db.schema();
    let keyspace = schema.keyspace(&ks_name).cloned();
    // An overloaded name without argument types fails before authorization.
    if x.arg_types.is_none()
        && cx.enabled()
        && keyspace.as_ref().is_some_and(|k| {
            k.functions.iter().filter(|f| f.name == name).count()
                + k.aggregates.iter().filter(|a| a.name == name).count()
                > 1
        })
    {
        return Err(m::multiple_named(&ks_name, &name));
    }
    // Authorization: DROP on every overload the statement names.
    if let Some(k) = &keyspace {
        let wanted: Option<Vec<CqlType>> = match &x.arg_types {
            Some(v) => v.iter().map(|t| resolve(k, t, d).ok()).collect(),
            None => None,
        };
        let overloads = k
            .functions
            .iter()
            .filter(|f| f.name == name)
            .map(|f| &f.argument_types)
            .chain(
                k.aggregates
                    .iter()
                    .filter(|a| a.name == name)
                    .map(|a| &a.argument_types),
            );
        for texts in overloads {
            let Some(types) = meta_types(k, texts) else {
                continue;
            };
            if x.arg_types.is_some() && wanted.as_ref().is_none_or(|w| !types_match(&types, w)) {
                continue;
            }
            cx.permission(ast::Permission::Drop, resource(&ks_name, &name, &types, d))?;
        }
    }
    if !cx.enabled() {
        return Err(m::disabled(d));
    }
    if !aggregate {
        cx.system(&ks_name)?;
    }
    // Nothing to drop: `No function named …` without argument types, `User function …(types)
    // doesn't exist` with them.
    let raw: Vec<String> = x
        .arg_types
        .iter()
        .flatten()
        .map(|t| t.to_string())
        .collect();
    let none = || {
        if x.if_exists {
            Ok(Response::void())
        } else if x.arg_types.is_some() {
            Err(m::overload_missing(&ks_name, &name, &raw))
        } else {
            Err(m::none_named(&ks_name, &name))
        }
    };
    let Some(keyspace) = keyspace else {
        return none();
    };
    let wanted: Option<Vec<CqlType>> = match &x.arg_types {
        Some(v) => Some(
            v.iter()
                .map(|t| resolve(&keyspace, t, d))
                .collect::<Result<Vec<_>>>()?,
        ),
        None => None,
    };
    let candidates: Vec<Vec<CqlType>> = keyspace
        .functions
        .iter()
        .filter(|f| f.name == name)
        .map(|f| &f.argument_types)
        .chain(
            keyspace
                .aggregates
                .iter()
                .filter(|a| a.name == name)
                .map(|a| &a.argument_types),
        )
        .filter_map(|texts| meta_types(&keyspace, texts))
        .collect();
    if candidates.is_empty() {
        return none();
    }
    let types = match &wanted {
        Some(w) => match candidates.iter().find(|c| types_match(c, w)) {
            Some(t) => t.clone(),
            None => return none(),
        },
        None if candidates.len() > 1 => return Err(m::multiple_named(&ks_name, &name)),
        None => candidates[0].clone(),
    };
    let (found, shown) = scylla_find(&keyspace, &name, &types);
    let mut k = (*keyspace).clone();
    let (arg_types, target) = match (found, aggregate) {
        (Found::Scalar(_), true) => return Err(m::not_user_defined(&shown, "aggregate")),
        (Found::Aggregate, false) => return Err(m::not_user_defined(&shown, "function")),
        (Found::None, _) => return none(),
        (Found::Aggregate, true) => {
            let i = k
                .aggregates
                .iter()
                .position(|a| {
                    a.name == name
                        && meta_types(&keyspace, &a.argument_types)
                            .is_some_and(|at| types_match(&at, &types))
                })
                .ok_or_else(|| m::none_named(&ks_name, &name))?;
            (
                k.aggregates.remove(i).argument_types,
                ChangeTarget::Aggregate,
            )
        }
        (Found::Scalar(_), false) => {
            // The aggregate ScyllaDB names is the first of its (hashed) registry; roll names the
            // last by name.
            if let Some(a) = keyspace
                .aggregates
                .iter()
                .filter(|a| aggregate_uses(&keyspace, a, &name, &types))
                .max_by(|a, b| a.name.cmp(&b.name))
            {
                return Err(m::used_by_aggregate_scylla(
                    &shown,
                    &format!("{ks_name}.{}", a.name),
                ));
            }
            let i = k
                .functions
                .iter()
                .position(|f| {
                    f.name == name
                        && meta_types(&keyspace, &f.argument_types)
                            .is_some_and(|ft| types_match(&ft, &types))
                })
                .ok_or_else(|| m::none_named(&ks_name, &name))?;
            (k.functions.remove(i).argument_types, ChangeTarget::Function)
        }
    };
    let event = SchemaChange {
        change: ChangeType::Dropped,
        target,
        keyspace: ks_name.clone(),
        name: Some(name.clone()),
        arg_types: Some(arg_types),
    };
    commit(cx, k, event).await
}

/// Whether aggregate `a` uses `name(types)` as its state, final or reduce function.
fn aggregate_uses(ks: &Keyspace, a: &AggregateMeta, name: &str, types: &[CqlType]) -> bool {
    let Some(stype) = function::parse_type(ks, &a.state_type) else {
        return false;
    };
    let Some(args) = meta_types(ks, &a.argument_types) else {
        return false;
    };
    let mut sargs = vec![stype.clone()];
    sargs.extend(args);
    (a.state_func == name && types_match(&sargs, types))
        || (a.final_func.as_deref() == Some(name)
            && types_match(std::slice::from_ref(&stype), types))
        || (a.reduce_func.as_deref() == Some(name)
            && types_match(&[stype.clone(), stype.clone()], types))
}

/// Whether a column mask's function signature (the column's type, then the partial arguments'
/// types) is `types`.
fn mask_types_match(
    ks: &Keyspace,
    column: &CqlType,
    partial: &[String],
    types: &[CqlType],
) -> bool {
    let Some(rest) = meta_types(ks, partial) else {
        return false;
    };
    let mut all = vec![column.clone()];
    all.extend(rest);
    types_match(&all, types)
}

/// The functions using user type `ty` of `ks` (`DropTypeStatement`): `ks.f` names.
pub fn functions_using_type(ks: &Keyspace, ty: &str) -> Vec<String> {
    let uses = |texts: &[String], ret: &str| {
        texts
            .iter()
            .chain(std::iter::once(&ret.to_string()))
            .any(|t| function::parse_type(ks, t).is_some_and(|c| c.references_udt(&ks.name, ty)))
    };
    let mut out: Vec<String> = ks
        .functions
        .iter()
        .filter(|f| uses(&f.argument_types, &f.return_type))
        .map(|f| format!("{}.{}", ks.name, f.name))
        .collect();
    out.extend(
        ks.aggregates
            .iter()
            .filter(|a| {
                uses(&a.argument_types, &a.return_type)
                    || function::parse_type(ks, &a.state_type)
                        .is_some_and(|c| c.references_udt(&ks.name, ty))
            })
            .map(|a| format!("{}.{}", ks.name, a.name)),
    );
    out
}
