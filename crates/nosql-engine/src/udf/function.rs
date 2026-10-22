//! User functions and aggregates as [`Function`]s, built from the schema's metadata
//! ([`FunctionMeta`] / [`AggregateMeta`]) by the [`Provider`] the registry consults.
//!
//! Ports Cassandra's `UDFunction` and `UDAggregate` (Apache-2.0) semantics: user functions are
//! never pre-evaluated (`isPure` is false), `RETURNS NULL ON NULL INPUT` returns null without a
//! call, and an aggregate's state function is skipped for rows with a null argument (and turns a
//! null state into null) when it doesn't accept nulls; the final function sees the state after
//! the last row (the `INITCOND` with no rows). ScyllaDB calls the state function on every row
//! instead (a null argument of a `RETURNS NULL ON NULL INPUT` one makes the state null) and the
//! final function on whatever state is left.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, Weak};

use bytes::Bytes;
use nosql_types::{CqlType, Dialect};
use parking_lot::Mutex;

use super::lua;
use super::messages as m;
use crate::database::Database;
use crate::error::Result;
use crate::functions::{Aggregate, FnCtx, FnName, Function, UserFunctions};
use crate::schema::{AggregateMeta, FunctionMeta, Keyspace};

/// A user-defined scalar function.
pub struct UserFn {
    pub name: FnName,
    pub args: Vec<CqlType>,
    pub arg_names: Vec<String>,
    pub ret: CqlType,
    pub called_on_null: bool,
    pub language: String,
    pub body: String,
    dialect: Dialect,
    /// `user_defined_functions_enabled` when the function was loaded.
    enabled: bool,
    /// The Lua bytecode, compiled on first call.
    code: OnceLock<std::result::Result<Vec<u8>, String>>,
    /// The WASM module, compiled on first call, and its instance.
    wasm: OnceLock<std::result::Result<super::wasm::Compiled, String>>,
    instance: Mutex<Option<super::wasm::Instance>>,
    /// The stored argument types (what owns a WASM instance).
    type_texts: Vec<String>,
}

impl std::fmt::Debug for UserFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({:?}) -> {:?}", self.name, self.args, self.ret)
    }
}

/// Resolves a stored type text in `ks`.
pub fn parse_type(ks: &Keyspace, text: &str) -> Option<CqlType> {
    nosql_types::parse_cql_type(text, &ks.name, &|_, n| ks.types.get(n).cloned()).ok()
}

impl UserFn {
    pub fn from_meta(ks: &Keyspace, f: &FunctionMeta, d: Dialect, enabled: bool) -> Option<UserFn> {
        let args = f
            .argument_types
            .iter()
            .map(|t| parse_type(ks, t))
            .collect::<Option<Vec<_>>>()?;
        Some(UserFn {
            name: FnName {
                keyspace: ks.name.clone(),
                name: f.name.clone(),
            },
            args,
            arg_names: f.argument_names.clone(),
            ret: parse_type(ks, &f.return_type)?,
            called_on_null: f.called_on_null_input,
            language: f.language.clone(),
            body: f.body.clone(),
            dialect: d,
            enabled,
            code: OnceLock::new(),
            wasm: OnceLock::new(),
            instance: Mutex::new(None),
            type_texts: f.argument_types.clone(),
        })
    }

    fn cql_types(&self) -> Vec<String> {
        self.args.iter().map(nosql_types::cql_name).collect()
    }

    /// ScyllaDB's `ks.f : (int, text) -> int`.
    pub fn signature(&self) -> String {
        format!(
            "{}.{} : ({}) -> {}",
            self.name.keyspace,
            self.name.name,
            self.cql_types().join(", "),
            nosql_types::cql_name(&self.ret)
        )
    }

    /// The error of a failed Lua call, as ScyllaDB reports it.
    fn lua_failure(&self, f: lua::Failure) -> crate::error::Error {
        use crate::error::Error;
        match f {
            lua::Failure::Server(m) => Error::server(m),
            lua::Failure::InvalidReturn(b) => {
                let ret = nosql_types::cql_name(&self.ret);
                let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
                Error::server(m::invalid_return(&self.signature(), &hex, &ret))
            }
            other => Error::invalid(other.to_string()),
        }
    }

    /// Runs the body on (possibly null) arguments, whatever the null-input directive.
    pub fn invoke(&self, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        if !self.enabled {
            return Err(m::disabled(self.dialect));
        }
        match self.language.as_str() {
            "lua" if self.dialect.is_scylla() => {
                let code = self
                    .code
                    .get_or_init(|| lua::compile(&self.arg_names, &self.body));
                let code = match code {
                    Ok(c) => c,
                    Err(e) => return Err(m::lua_compile(e)),
                };
                let typed: Vec<(&CqlType, Option<&[u8]>)> = self
                    .args
                    .iter()
                    .zip(
                        args.iter()
                            .map(|a| a.as_deref())
                            .chain(std::iter::repeat(None)),
                    )
                    .collect();
                lua::call(code, &typed, &self.ret).map_err(|f| self.lua_failure(f))
            }
            "wasm" if self.dialect.is_scylla() => {
                let compiled = self
                    .wasm
                    .get_or_init(|| super::wasm::compile(&self.name.name, &self.body));
                let compiled = compiled
                    .as_ref()
                    .map_err(|e| crate::error::Error::invalid(e.clone()))?;
                let typed: Vec<(&CqlType, Option<&[u8]>)> = self
                    .args
                    .iter()
                    .zip(args.iter().map(|a| a.as_deref()))
                    .collect();
                let owner = super::wasm::Owner {
                    keyspace: &self.name.keyspace,
                    name: &self.name.name,
                    argument_types: &self.type_texts,
                };
                super::wasm::call(
                    compiled,
                    &self.instance,
                    &owner,
                    &typed,
                    &self.ret,
                    self.called_on_null,
                )
                .map_err(crate::error::Error::invalid)
            }
            _ => Err(crate::error::Error::function_failure(
                m::JAVA_UNSUPPORTED,
                &self.name.keyspace,
                &self.name.name,
                self.cql_types(),
            )),
        }
    }
}

impl Function for UserFn {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &self.ret
    }
    fn is_deterministic(&self) -> bool {
        false
    }
    fn null_on_null_input(&self) -> bool {
        !self.called_on_null
    }
    fn execute(&self, _ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        if !self.called_on_null && args.iter().any(Option::is_none) {
            return Ok(None);
        }
        self.invoke(args)
    }
}

/// A user-defined aggregate.
#[derive(Debug)]
pub struct UserAgg {
    pub name: FnName,
    pub args: Vec<CqlType>,
    pub ret: CqlType,
    pub state_type: CqlType,
    pub state: Arc<UserFn>,
    pub fin: Option<Arc<UserFn>>,
    /// ScyllaDB's `REDUCEFUNC` (validated and stored; a single node never needs to merge).
    pub reduce: Option<Arc<UserFn>>,
    pub initcond: Option<Bytes>,
}

impl UserAgg {
    pub fn from_meta(
        ks: &Keyspace,
        a: &AggregateMeta,
        d: Dialect,
        enabled: bool,
    ) -> Option<UserAgg> {
        let args = a
            .argument_types
            .iter()
            .map(|t| parse_type(ks, t))
            .collect::<Option<Vec<_>>>()?;
        let state_type = parse_type(ks, &a.state_type)?;
        let find = |name: &str, types: &[CqlType]| -> Option<Arc<UserFn>> {
            let f = ks
                .functions
                .iter()
                .find(|f| f.name == name && types_match_texts(ks, &f.argument_types, types))?;
            UserFn::from_meta(ks, f, d, enabled).map(Arc::new)
        };
        let mut state_args = vec![state_type.clone()];
        state_args.extend(args.iter().cloned());
        let state = find(&a.state_func, &state_args)?;
        let fin = match &a.final_func {
            Some(f) => Some(find(f, std::slice::from_ref(&state_type))?),
            None => None,
        };
        let reduce = match &a.reduce_func {
            Some(f) => find(f, &[state_type.clone(), state_type.clone()]),
            None => None,
        };
        let initcond = match &a.initcond {
            Some(text) => Some(initcond_bytes(&state_type, text, d)?),
            None => None,
        };
        Some(UserAgg {
            name: FnName {
                keyspace: ks.name.clone(),
                name: a.name.clone(),
            },
            args,
            ret: parse_type(ks, &a.return_type)?,
            state_type,
            state,
            fin,
            reduce,
            initcond,
        })
    }
}

/// The stored `INITCOND` (a CQL literal) as bytes of the state type.
pub fn initcond_bytes(ty: &CqlType, text: &str, d: Dialect) -> Option<Bytes> {
    let t = parse_term(text, d)?;
    let schema = Arc::new(crate::schema::Schema::default());
    let reg = crate::functions::Registry::new(d);
    let mut cx = crate::prepare::PrepareCtx::new(d, &schema, "", &reg, &[]);
    let term = crate::prepare::terms::prepare(
        &t,
        &crate::prepare::Receiver::new("", "", "(dummy)", ty.clone()),
        &mut cx,
    )
    .ok()?;
    term.eval_const(d).ok().flatten()
}

/// Parses one CQL term (`Terms.asBytes` re-parses the INITCOND's text the same way).
pub fn parse_term(text: &str, d: Dialect) -> Option<nosql_cql::ast::Term<'static>> {
    use nosql_cql::ast::{InsertValues, IntoStatic, StatementKind};
    let src = format!("INSERT INTO x (a) VALUES ({text})");
    let stmt = crate::session::parse(&src, d).ok()?;
    if !stmt.bind_markers.is_empty() {
        return None;
    }
    match stmt.kind {
        StatementKind::Insert(i) => match i.values {
            InsertValues::Values { mut values, .. } if values.len() == 1 => {
                values.pop().map(IntoStatic::into_static)
            }
            _ => None,
        },
        _ => None,
    }
}

impl Function for UserAgg {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &self.ret
    }
    fn is_aggregate(&self) -> bool {
        true
    }
    fn is_deterministic(&self) -> bool {
        false
    }
    fn new_aggregate(&self) -> Option<Box<dyn Aggregate>> {
        Some(Box::new(State {
            state_fn: self.state.clone(),
            fin: self.fin.clone(),
            value: self.initcond.clone(),
        }))
    }
    fn dependencies(&self) -> Vec<Arc<dyn Function>> {
        let mut v: Vec<Arc<dyn Function>> = vec![self.state.clone()];
        if let Some(f) = &self.fin {
            v.push(f.clone());
        }
        if let Some(f) = &self.reduce {
            v.push(f.clone());
        }
        v
    }
}

/// The running state of a user aggregate over one group.
struct State {
    state_fn: Arc<UserFn>,
    fin: Option<Arc<UserFn>>,
    value: Option<Bytes>,
}

impl Aggregate for State {
    fn add(&mut self, ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<()> {
        let f = &self.state_fn;
        if f.dialect.is_scylla() {
            // ScyllaDB calls the state function on every row: a null argument of a `RETURNS NULL
            // ON NULL INPUT` function makes the state null.
            let mut call = Vec::with_capacity(args.len() + 1);
            call.push(self.value.clone());
            call.extend(args.iter().cloned());
            self.value = f.execute(ctx, &call)?;
            return Ok(());
        }
        if !f.called_on_null {
            // `isCallableWrtNullable`: a null row argument skips the row; a null state stays null
            // (`executeForAggregate`).
            if args.iter().any(Option::is_none) {
                return Ok(());
            }
            if self.value.is_none() {
                return Ok(());
            }
        }
        let mut call = Vec::with_capacity(args.len() + 1);
        call.push(self.value.clone());
        call.extend(args.iter().cloned());
        self.value = f.invoke(&call)?;
        Ok(())
    }

    fn compute(&self, ctx: &FnCtx) -> Result<Option<Bytes>> {
        match &self.fin {
            None => Ok(self.value.clone()),
            Some(f) if f.dialect.is_scylla() => f.execute(ctx, std::slice::from_ref(&self.value)),
            Some(f) => {
                if !f.called_on_null && self.value.is_none() {
                    return Ok(None);
                }
                f.invoke(std::slice::from_ref(&self.value))
            }
        }
    }
}

/// Cassandra's `AbstractFunction.typesMatch`: the frozen CQL names are equal.
pub fn types_match(a: &[CqlType], b: &[CqlType]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| nosql_types::cql_name(&x.freeze()) == nosql_types::cql_name(&y.freeze()))
}

/// [`types_match`] of stored type texts against resolved types.
pub fn types_match_texts(ks: &Keyspace, texts: &[String], types: &[CqlType]) -> bool {
    let Some(parsed) = texts
        .iter()
        .map(|t| parse_type(ks, t))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    types_match(&parsed, types)
}

/// The user functions of the current schema, cached per schema version.
pub struct Provider {
    db: Weak<Database>,
    cache: Mutex<Cache>,
}

#[derive(Default)]
struct Cache {
    version: [u8; 16],
    entries: HashMap<(String, String), Vec<Arc<dyn Function>>>,
}

impl Provider {
    pub fn new(db: &Arc<Database>) -> Provider {
        Provider {
            db: Arc::downgrade(db),
            cache: Mutex::new(Cache::default()),
        }
    }
}

impl UserFunctions for Provider {
    fn get(&self, keyspace: &str, name: &str) -> Vec<Arc<dyn Function>> {
        let Some(db) = self.db.upgrade() else {
            return Vec::new();
        };
        let schema = db.schema();
        let key = (keyspace.to_string(), name.to_string());
        {
            let mut c = self.cache.lock();
            if c.version != schema.version {
                c.version = schema.version;
                c.entries.clear();
            }
            if let Some(v) = c.entries.get(&key) {
                return v.clone();
            }
        }
        let Some(ks) = schema.keyspace(keyspace) else {
            return Vec::new();
        };
        let d = db.dialect();
        let enabled = db.config().settings.user_defined_functions_enabled;
        let mut out: Vec<Arc<dyn Function>> = Vec::new();
        for f in ks.functions.iter().filter(|f| f.name == name) {
            if let Some(u) = UserFn::from_meta(ks, f, d, enabled) {
                out.push(Arc::new(u));
            }
        }
        for a in ks.aggregates.iter().filter(|a| a.name == name) {
            if let Some(u) = UserAgg::from_meta(ks, a, d, enabled) {
                out.push(Arc::new(u));
            }
        }
        let mut c = self.cache.lock();
        if c.version == schema.version {
            c.entries.insert(key, out.clone());
        }
        out
    }
}
