//! Dynamic data masking: Cassandra 5's column masks and the platform's masking policy.
//!
//! **Column masks** (`MASKED WITH fn(args)` in `CREATE TABLE` / `ALTER TABLE`, Cassandra's
//! `ColumnMask`, Apache-2.0) are a partial application of a masking function (or a UDF of the
//! table's keyspace) whose first argument is the column's value. They are stored in
//! [`crate::schema::Table::masks`] and `system_schema.column_masks` (argument types, values as
//! `AbstractType.getString` prints them ([`get_string`]), nulls in a parallel list), created only
//! with `dynamic_data_masking_enabled`, and applied at read time to roles without `UNMASK` on the
//! table read while the setting is on (`SimpleSelector`: the masked value feeds functions,
//! aggregates, `token()` and JSON; `WRITETIME` / `TTL`, the row filter, grouping and post-query
//! ordering see the real values). Restricting a masked column needs `UNMASK` or `SELECT_MASKED`
//! (`SelectStatement.authorize`, columns listed in `TableMetadata.columns()` hash order). The
//! `[applied]` rows of conditional statements show real values to every role.
//!
//! A materialized view's columns carry their base columns' masks ([`view_masks`], [`sync_views`])
//! with their own `column_masks` rows; reading a view needs `UNMASK` on the view (or its keyspace),
//! not on the base. Like Cassandra's `SchemaKeyspace`, dropping or renaming a column or dropping a
//! table or view leaves its `column_masks` rows behind ([`carry_stale`]). ScyllaDB has no masking:
//! its parser rejects `MASKED`, `UNMASK` and `SELECT_MASKED`.
//!
//! **The platform policy** ([`crate::config::DbConfig::masking`], [`Database::set_masking`])
//! masks `(keyspace, table, column)` for every role, superusers included, in both dialects and
//! whatever the setting: preview branches of production data are anonymised this way. It reuses
//! the masking functions (Cassandra's, available in both dialects here). Where a column has both,
//! the platform's mask applies. A policy entry that doesn't fit its column (`mask_inner` on an
//! `int`) masks with `mask_default` instead, so a mistake never shows real data. A table's entries
//! also cover the columns of its materialized views, index views and CDC log
//! (`cdc$deleted_elements_<c>` with `c`'s entry, which falls back to `mask_default`).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use bytes::Bytes;
use nosql_cql::ast;
use nosql_types::{CqlType, Dialect};
use parking_lot::RwLock;

use crate::config::MaskCall;
use crate::database::Database;
use crate::error::{Error, Result};
use crate::functions::{FnCtx, Function, Typed};
use crate::prepare::{PrepareCtx, Receiver, terms};
use crate::schema::{ColumnMask, Schema, Table};
use crate::session::SessionState;

/// `ColumnMask.DISABLED_ERROR_MESSAGE`.
pub const DISABLED: &str = "Cannot mask columns because dynamic data masking is not enabled. You can enable it with the dynamic_data_masking_enabled property on cassandra.yaml";

/// The platform policy: `(keyspace, table, column)` → call.
pub type Policy = BTreeMap<(String, String, String), MaskCall>;

/// The live platform policy of a database.
#[derive(Default)]
pub struct State {
    policy: RwLock<Arc<Policy>>,
    /// Cassandra's functions, for the policy's masking functions in any dialect.
    functions: std::sync::OnceLock<crate::functions::Registry>,
}

impl State {
    pub fn new(policy: Policy) -> State {
        State {
            policy: RwLock::new(Arc::new(policy)),
            functions: std::sync::OnceLock::new(),
        }
    }

    fn functions(&self) -> &crate::functions::Registry {
        self.functions
            .get_or_init(|| crate::functions::Registry::new(Dialect::Cassandra))
    }
}

impl Database {
    /// The platform masking policy in force.
    pub fn masking(&self) -> Arc<Policy> {
        self.masking.policy.read().clone()
    }

    /// Replaces the platform masking policy (applies to the next reads).
    pub fn set_masking(&self, policy: Policy) {
        *self.masking.policy.write() = Arc::new(policy);
    }
}

/// `ColumnMask.ensureEnabled`.
pub fn ensure_enabled(db: &Database) -> Result<()> {
    if db.settings().dynamic_data_masking_enabled {
        Ok(())
    } else {
        Err(Error::invalid(DISABLED))
    }
}

/// A resolved mask: the function and its partial arguments.
#[derive(Debug, Clone)]
pub struct Masker {
    f: Arc<dyn Function>,
    partial: Vec<Option<Bytes>>,
}

impl Masker {
    /// `ColumnMask.Masker.mask`.
    pub fn mask(&self, ctx: &FnCtx, value: Option<Bytes>) -> Result<Option<Bytes>> {
        let mut args = Vec::with_capacity(self.partial.len() + 1);
        args.push(value);
        args.extend(self.partial.iter().cloned());
        if self.f.null_on_null_input() && args.iter().any(Option::is_none) {
            return Ok(None);
        }
        self.f.execute(ctx, &args)
    }
}

/// The masks of one read, by table column index.
#[derive(Debug, Default)]
pub struct ReadMasks {
    pub columns: HashMap<usize, Masker>,
}

impl ReadMasks {
    pub fn get(&self, column: usize) -> Option<&Masker> {
        self.columns.get(&column)
    }
}

/// `FunctionName.toString` + `ColumnMask.Raw.toString`: the mask as written, for messages.
fn raw_text(m: &ast::Masking) -> String {
    let name = match &m.function {
        Some(f) => match &f.keyspace {
            Some(k) => format!("{}.{}", k.name(), f.resolved()),
            None => f.resolved().to_string(),
        },
        None => "system.mask_default".into(),
    };
    let args: Vec<String> = m.args.iter().map(|a| a.to_string()).collect();
    format!("{name}({})", args.join(", "))
}

fn schema_resolver<'a>(
    schema: &'a Schema,
) -> impl Fn(&str, &str) -> Option<Arc<nosql_types::UdtDef>> + 'a {
    move |ks: &str, name: &str| schema.keyspace(ks).and_then(|k| k.types.get(name).cloned())
}

/// Resolves a mask written in DDL for column `column` of type `ty` of `keyspace.table`
/// (`ColumnMask.Raw.prepare`), with Cassandra's messages.
pub fn prepare(
    db: &Database,
    schema: &Schema,
    keyspace: &str,
    table: &str,
    column: &str,
    ty: &CqlType,
    m: &ast::Masking,
) -> Result<(ColumnMask, Masker)> {
    let shown = raw_text(m);
    let (fks, fname) = match &m.function {
        Some(f) => (
            f.keyspace.as_ref().map(|k| k.name().to_string()),
            f.resolved().to_ascii_lowercase(),
        ),
        None => (Some("system".into()), "mask_default".into()),
    };
    let masker = resolve(db, db.functions(), schema, keyspace, table, ty, fks.as_deref(), &fname, &m.args)?
        .ok_or_else(|| {
            Error::invalid(format!(
                "Unable to find masking function for {column}, no declared function matches the signature {shown}"
            ))
        })?;
    let f = &masker.f;
    if f.is_aggregate() {
        return Err(Error::invalid(format!(
            "Aggregate function {shown} cannot be used for masking table columns"
        )));
    }
    let native = f.name().keyspace == "system";
    if native && !f.name().name.starts_with(crate::functions::masking::PREFIX) {
        return Err(Error::invalid(format!(
            "Not-masking function {shown} cannot be used for masking table columns"
        )));
    }
    if !native && f.name().keyspace != keyspace {
        return Err(Error::invalid(format!(
            "Masking function {shown} doesn't belong to the same keyspace as the table {keyspace}.{table}"
        )));
    }
    let ret = nosql_types::cql_name(f.return_type());
    let want = nosql_types::cql_name(ty);
    if ret != want {
        return Err(Error::invalid(format!(
            "Masking function {shown} return type is {ret}. This is different to the type of the masked column {column} of type {want}. Masking functions can only be attached to table columns if they return the same data type as the masked column."
        )));
    }
    let d = db.dialect();
    let types: Vec<&CqlType> = f.arg_types().iter().skip(1).collect();
    let mask = ColumnMask {
        function_keyspace: f.name().keyspace.clone(),
        function_name: f.name().name.clone(),
        arg_types: types.iter().map(|t| nosql_types::cql_name(t)).collect(),
        arg_values: masker
            .partial
            .iter()
            .zip(&types)
            .map(|(v, t)| v.as_ref().map(|b| get_string(t, b, d)))
            .collect(),
    };
    Ok((mask, masker))
}

/// Resolves `fname(column, args…)` and evaluates the arguments (constants only).
#[allow(clippy::too_many_arguments)]
fn resolve(
    db: &Database,
    registry: &crate::functions::Registry,
    schema: &Schema,
    keyspace: &str,
    table: &str,
    ty: &CqlType,
    fks: Option<&str>,
    fname: &str,
    args: &[ast::Term],
) -> Result<Option<Masker>> {
    let d = db.dialect();
    let schema = Arc::new(schema.clone());
    let cx = PrepareCtx::new(d, &schema, keyspace, db.functions(), &[]);
    let uf = db.user_functions.read().clone();
    let f = {
        let first = Typed {
            ty: ty.clone(),
            text: nosql_types::marshal_name(ty, false, d),
        };
        let raw: Vec<terms::RawArg> = args
            .iter()
            .map(|a| terms::RawArg { term: a, cx: &cx })
            .collect();
        let mut refs: Vec<&dyn crate::functions::ArgInfo> = vec![&first];
        refs.extend(raw.iter().map(|a| a as &dyn crate::functions::ArgInfo));
        let user = uf.as_deref().map(|u| (u, keyspace));
        registry.resolve(fks, fname, &refs, Some(ty), user)?
    };
    let Some(f) = f else {
        return Ok(None);
    };
    let mut cx = cx;
    let mut partial = Vec::with_capacity(args.len());
    for (i, a) in args.iter().enumerate() {
        let aty = f.arg_types().get(i + 1).cloned().unwrap_or(CqlType::Blob);
        let r = Receiver::new(keyspace, table, format!("arg{}({})", i + 1, f.name()), aty);
        match terms::prepare(a, &r, &mut cx)? {
            terms::Term::Const(v) => partial.push(v),
            _ => {
                return Err(Error::invalid(format!(
                    "Masking function arguments must be constants: {a}"
                )));
            }
        }
    }
    Ok(Some(Masker { f, partial }))
}

/// The masker of a stored mask (`SchemaKeyspace.createColumnFromRow`).
pub fn stored(
    db: &Database,
    schema: &Arc<Schema>,
    t: &Table,
    ty: &CqlType,
    m: &ColumnMask,
) -> Result<Masker> {
    let d = db.dialect();
    let resolver = schema_resolver(schema);
    let mut types = vec![ty.clone()];
    for s in &m.arg_types {
        types.push(
            nosql_types::parse_cql_type(s, &t.keyspace, &resolver).map_err(|e| {
                Error::server(format!("column mask of {}.{}: {e}", t.keyspace, t.name))
            })?,
        );
    }
    let typed: Vec<Typed> = types
        .iter()
        .map(|t| Typed {
            ty: t.clone(),
            text: nosql_types::cql_name(t),
        })
        .collect();
    let refs: Vec<&dyn crate::functions::ArgInfo> = typed.iter().map(|a| a as _).collect();
    let uf = db.user_functions.read().clone();
    let user = uf.as_deref().map(|u| (u, t.keyspace.as_str()));
    let f = db
        .functions()
        .resolve(
            Some(&m.function_keyspace),
            &m.function_name,
            &refs,
            Some(ty),
            user,
        )?
        .ok_or_else(|| {
            Error::server(format!(
                "Unable to find masking function {}.{} for column {}.{}",
                m.function_keyspace, m.function_name, t.keyspace, t.name
            ))
        })?;
    let mut partial = Vec::with_capacity(m.arg_values.len());
    for (v, aty) in m.arg_values.iter().zip(types.iter().skip(1)) {
        partial.push(match v {
            None => None,
            Some(s) => Some(from_get_string(aty, s, d).map_err(|e| Error::server(e.message))?),
        });
    }
    Ok(Masker { f, partial })
}

/// `ColumnMask.toString`: `system.mask_inner(1, 1)` (DESCRIBE).
pub fn describe(m: &ColumnMask, k: &crate::schema::Keyspace, d: Dialect) -> String {
    let resolver = |_: &str, name: &str| k.types.get(name).cloned();
    let args: Vec<String> = m
        .arg_types
        .iter()
        .zip(&m.arg_values)
        .map(|(t, v)| match v {
            None => "null".to_string(),
            Some(s) => nosql_types::parse_cql_type(t, &k.name, &resolver)
                .ok()
                .and_then(|ty| {
                    let b = from_get_string(&ty, s, d).ok()?;
                    nosql_types::to_cql_literal(&ty, &b, d).ok()
                })
                .unwrap_or_else(|| s.clone()),
        })
        .collect();
    format!(
        "{}.{}({})",
        m.function_keyspace,
        m.function_name,
        args.join(", ")
    )
}

/// Parses a platform call (`mask_inner(1, 1)`, `system.mask_hash('MD5')`, `mask_default`,
/// `DEFAULT`).
pub fn parse_call(text: &str) -> std::result::Result<MaskCall, String> {
    let text = text.trim();
    let text = if text.contains('(') || text.eq_ignore_ascii_case("default") {
        text.to_string()
    } else {
        format!("{text}()")
    };
    let m = parse_masking(&text)?;
    let function = match &m.function {
        None => "mask_default".to_string(),
        Some(f) => {
            if let Some(k) = &f.keyspace
                && k.name() != "system"
            {
                return Err(format!(
                    "{text}: only the system masking functions can be used"
                ));
            }
            f.resolved().to_ascii_lowercase()
        }
    };
    if !MASKING_FUNCTIONS.contains(&function.as_str()) {
        return Err(format!(
            "{function} is not a masking function (one of {})",
            MASKING_FUNCTIONS.join(", ")
        ));
    }
    let mut args = Vec::with_capacity(m.args.len());
    for a in &m.args {
        if !matches!(a, ast::Term::Literal(_) | ast::Term::Null) {
            return Err(format!("{text}: arguments must be literals"));
        }
        args.push(a.to_string());
    }
    Ok(MaskCall { function, args })
}

/// The masking functions a platform call may use.
pub const MASKING_FUNCTIONS: &[&str] = &[
    "mask_null",
    "mask_default",
    "mask_replace",
    "mask_inner",
    "mask_outer",
    "mask_hash",
];

fn parse_masking(text: &str) -> std::result::Result<ast::Masking<'static>, String> {
    use nosql_cql::ast::{AlterTableAction, StatementKind};
    let stmt = format!("ALTER TABLE t ALTER c MASKED WITH {text}");
    let parsed = nosql_cql::parse(&stmt, nosql_cql::Dialect::Cassandra)
        .map_err(|e| format!("{text}: {e}"))?;
    let Some(s) = parsed.into_iter().next() else {
        return Err(format!("{text}: not a function call"));
    };
    use nosql_cql::ast::IntoStatic;
    match s.kind.into_static() {
        StatementKind::AlterTable(a) => match a.action {
            AlterTableAction::Mask { mask: Some(m), .. } => Ok(m),
            _ => Err(format!("{text}: not a function call")),
        },
        _ => Err(format!("{text}: not a function call")),
    }
}

/// The masker of a platform call for a column of type `ty` (falling back to `mask_default` when
/// the call doesn't fit the type).
fn platform(
    db: &Database,
    schema: &Arc<Schema>,
    t: &Table,
    ty: &CqlType,
    call: &MaskCall,
) -> Option<Masker> {
    let text = call.to_string();
    let fit = parse_masking(&text).ok().and_then(|m| {
        resolve(
            db,
            db.masking.functions(),
            schema,
            &t.keyspace,
            &t.name,
            ty,
            Some("system"),
            &call.function,
            &m.args,
        )
        .ok()
        .flatten()
        .filter(|mk| {
            mk.f.return_type() == ty
                || nosql_types::cql_name(mk.f.return_type()) == nosql_types::cql_name(ty)
        })
    });
    fit.or_else(|| {
        tracing::warn!(
            keyspace = %t.keyspace, table = %t.name, call = %text,
            "masking policy entry doesn't fit its column; masking with mask_default"
        );
        resolve(
            db,
            db.masking.functions(),
            schema,
            &t.keyspace,
            &t.name,
            ty,
            Some("system"),
            "mask_default",
            &[],
        )
        .ok()
        .flatten()
    })
}

/// Whether a read of `t` by `session` sees column masks (Cassandra: the setting is on and the
/// role has no `UNMASK`).
pub fn column_masks_apply(db: &Database, session: &SessionState, t: &Table) -> bool {
    !t.masks.is_empty()
        && !db.dialect().is_scylla()
        && db.settings().dynamic_data_masking_enabled
        && !crate::auth::can_unmask(db, session, &t.keyspace, &t.name)
}

/// The tables whose platform policy entries cover `t`: itself, and the base table of a view (or
/// index view) or CDC log, whose columns carry the base's values under the same names.
fn policy_tables(t: &Table) -> Vec<&str> {
    let mut out = vec![t.name.as_str()];
    if let Some(v) = &t.view {
        out.push(v.base_table_name.as_str());
    } else if let Some(base) = t.name.strip_suffix(crate::cdc::SUFFIX) {
        out.push(base);
    }
    out
}

/// The platform call for column `column` of `t` ([`policy_tables`]; a CDC log's
/// `cdc$deleted_elements_<c>` follows `c`).
fn policy_call<'p>(policy: &'p Policy, t: &Table, column: &str) -> Option<&'p MaskCall> {
    let mut names = vec![column];
    if let Some(c) = column.strip_prefix("cdc$deleted_elements_") {
        names.push(c);
    }
    policy_tables(t).into_iter().find_map(|table| {
        names
            .iter()
            .find_map(|c| policy.get(&(t.keyspace.clone(), table.to_string(), c.to_string())))
    })
}

/// The masks a read of `t` applies (`None`: no column is masked for it).
pub fn read_masks(
    db: &Database,
    session: &SessionState,
    t: &Table,
) -> Result<Option<Arc<ReadMasks>>> {
    let policy = db.masking();
    let columns_apply = column_masks_apply(db, session, t);
    let has_platform = policy_tables(t).into_iter().any(|name| {
        policy
            .range((t.keyspace.clone(), name.to_string(), String::new())..)
            .next()
            .is_some_and(|((k, n, _), _)| *k == t.keyspace && n == name)
    });
    if !columns_apply && !has_platform {
        return Ok(None);
    }
    let schema = db.schema();
    let mut out = ReadMasks::default();
    for (i, c) in t.columns.iter().enumerate() {
        if has_platform && let Some(call) = policy_call(&policy, t, &c.name) {
            if let Some(m) = platform(db, &schema, t, &c.ty, call) {
                out.columns.insert(i, m);
            }
            continue;
        }
        if columns_apply && let Some(m) = t.masks.get(&c.name) {
            out.columns.insert(i, stored(db, &schema, t, &c.ty, m)?);
        }
    }
    Ok((!out.columns.is_empty()).then(|| Arc::new(out)))
}

/// Applies the platform policy to the rows of a conditional statement's result (the `[applied]`
/// row shows the values read). Cassandra's column masks don't apply there (verified on the
/// reference: `ModificationStatement.buildCasResultSet` returns the real values to any role).
pub fn mask_conditional_rows(db: &Database, t: &Table, rows: &mut crate::response::Rows) {
    let policy = db.masking();
    if !policy_tables(t).into_iter().any(|name| {
        policy
            .range((t.keyspace.clone(), name.to_string(), String::new())..)
            .next()
            .is_some_and(|((k, n, _), _)| *k == t.keyspace && n == name)
    }) {
        return;
    }
    let schema = db.schema();
    let ctx = FnCtx {
        dialect: db.dialect(),
        protocol_version: 4,
    };
    let maskers: Vec<Option<Masker>> = rows
        .metadata
        .columns
        .iter()
        .map(|spec| {
            let c = t.column(&spec.name)?;
            let call = policy_call(&policy, t, &c.name)?;
            platform(db, &schema, t, &c.ty, call)
        })
        .collect();
    for row in &mut rows.rows {
        for (v, m) in row.iter_mut().zip(&maskers) {
            if let Some(m) = m {
                // A masking function that fails hides the value.
                *v = m.mask(&ctx, v.take()).unwrap_or(None);
            }
        }
    }
}

/// `SelectStatement.authorize`'s check: restricting masked columns needs `UNMASK` or
/// `SELECT_MASKED` (only while column masks apply).
pub fn check_restrictions(
    db: &Database,
    session: &SessionState,
    t: &Table,
    restricted: &[usize],
) -> Result<()> {
    if t.masks.is_empty() || db.dialect().is_scylla() || !db.settings().dynamic_data_masking_enabled
    {
        return Ok(());
    }
    if crate::auth::can_unmask(db, session, &t.keyspace, &t.name)
        || crate::auth::can_select_masked(db, session, &t.keyspace, &t.name)
    {
        return Ok(());
    }
    let masked: Vec<&str> = java_column_order(t)
        .into_iter()
        .map(|i| (i, &t.columns[i]))
        .filter(|(i, c)| t.masks.contains_key(&c.name) && restricted.contains(i))
        .map(|(_, c)| c.name.as_str())
        .collect();
    if masked.is_empty() {
        return Ok(());
    }
    Err(Error::unauthorized(format!(
        "User {} has no UNMASK nor SELECT_MASKED permission on table {}.{}, cannot query masked columns [{}]",
        session.user.as_deref().unwrap_or("anonymous"),
        t.keyspace,
        t.name,
        masked.join(", ")
    )))
}

/// `AbstractType.getString`: how `column_masks` stores an argument value. Blobs, collections
/// and vectors are bare hex (`BytesType`); tuples and UDTs join their fields with `:` (`\:` and
/// `\@` escape, `@` is null: `TupleType.getString`); other types print as their serializer's
/// `toString` (what `CAST(x AS text)` returns).
pub fn get_string(ty: &CqlType, b: &[u8], d: Dialect) -> String {
    match ty {
        CqlType::Blob
        | CqlType::Custom(_)
        | CqlType::Empty
        | CqlType::List { .. }
        | CqlType::Set { .. }
        | CqlType::Map { .. }
        | CqlType::Vector { .. } => b.iter().map(|x| format!("{x:02x}")).collect(),
        CqlType::Tuple(ts) => tuple_string(ts.iter(), b, d),
        CqlType::Udt(u, _) => tuple_string(u.fields.iter().map(|(_, t)| t), b, d),
        _ => nosql_types::to_text(ty, b, d)
            .ok()
            .flatten()
            .unwrap_or_default(),
    }
}

fn tuple_string<'t>(types: impl Iterator<Item = &'t CqlType>, b: &[u8], d: Dialect) -> String {
    let mut out = String::new();
    let mut rest = b;
    for (i, ty) in types.enumerate() {
        if rest.is_empty() {
            break;
        }
        if i > 0 {
            out.push(':');
        }
        let Some((len, tail)) = rest.split_first_chunk::<4>() else {
            break;
        };
        let len = i32::from_be_bytes(*len);
        if len < 0 {
            out.push('@');
            rest = tail;
            continue;
        }
        let n = (len as usize).min(tail.len());
        let field = get_string(ty, &tail[..n], d);
        out.push_str(&field.replace(':', "\\:").replace('@', "\\@"));
        rest = &tail[n..];
    }
    out
}

/// `AbstractType.fromString` of [`get_string`]'s forms (`ByteBufferUtil.hexToBytes`,
/// `TupleType.fromString` with `AbstractCompositeType.split`).
pub fn from_get_string(
    ty: &CqlType,
    s: &str,
    d: Dialect,
) -> std::result::Result<Bytes, nosql_types::MarshalError> {
    let bad = || nosql_types::MarshalError::new(format!("cannot parse '{s}' as hex bytes"));
    let fields = |types: Vec<&CqlType>| {
        let parts = split_unescaped(s);
        if parts.len() > types.len() {
            return Err(nosql_types::MarshalError::new(format!(
                "Invalid tuple literal: too many elements. Type {} expects {} but got {}",
                nosql_types::cql_name(ty),
                types.len(),
                parts.len()
            )));
        }
        let mut out = Vec::new();
        for (p, t) in parts.iter().zip(types) {
            if p == "@" {
                out.extend_from_slice(&(-1i32).to_be_bytes());
                continue;
            }
            let v = from_get_string(t, &p.replace("\\:", ":").replace("\\@", "@"), d)?;
            out.extend_from_slice(&(v.len() as i32).to_be_bytes());
            out.extend_from_slice(&v);
        }
        Ok(Bytes::from(out))
    };
    match ty {
        CqlType::Blob
        | CqlType::Custom(_)
        | CqlType::Empty
        | CqlType::List { .. }
        | CqlType::Set { .. }
        | CqlType::Map { .. }
        | CqlType::Vector { .. } => {
            let padded = if s.len() % 2 == 1 {
                format!("0{s}")
            } else {
                s.to_string()
            };
            (0..padded.len() / 2)
                .map(|i| {
                    padded
                        .get(2 * i..2 * i + 2)
                        .and_then(|h| u8::from_str_radix(h, 16).ok())
                        .ok_or_else(bad)
                })
                .collect::<std::result::Result<Vec<u8>, _>>()
                .map(Bytes::from)
        }
        CqlType::Tuple(ts) => fields(ts.iter().collect()),
        CqlType::Udt(u, _) => fields(u.fields.iter().map(|(_, t)| t).collect()),
        _ => nosql_types::from_string(ty, s, d),
    }
}

/// `AbstractCompositeType.split`: on every `:` not escaped by an odd number of backslashes.
fn split_unescaped(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut backslashes = 0;
    for ch in s.chars() {
        if ch == ':' && backslashes % 2 == 0 {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(ch);
        }
        backslashes = if ch == '\\' { backslashes + 1 } else { 0 };
    }
    out.push(cur);
    out
}

/// Keeps Cassandra's `column_masks` leftovers when `old` becomes `new`: `SchemaKeyspace` writes
/// (or deletes) a column's row whenever the column is written, but never deletes it when the
/// column is dropped or renamed or its table (or view) is dropped. Rows of columns that exist
/// again are theirs (written with the column), so leftovers only name columns that are gone.
pub fn carry_stale(old: Option<&crate::schema::Keyspace>, new: &mut crate::schema::Keyspace) {
    let exists = |ks: &crate::schema::Keyspace, t: &str, c: &str| {
        ks.tables.get(t).is_some_and(|t| t.column(c).is_some())
    };
    if let Some(old) = old {
        for (tname, t) in &old.tables {
            for (c, m) in &t.masks {
                if !exists(new, tname, c) {
                    new.stale_masks
                        .insert((tname.clone(), c.clone()), m.clone());
                }
            }
        }
    }
    let gone: Vec<(String, String)> = new
        .stale_masks
        .keys()
        .filter(|(t, c)| exists(new, t, c))
        .cloned()
        .collect();
    for k in gone {
        new.stale_masks.remove(&k);
    }
}

/// The masks of a materialized view: its columns keep their base columns' masks
/// (`CreateViewStatement`; `AlterTableStatement.MaskColumn` and `AddColumns` update the views).
pub fn view_masks(base: &Table, view: &Table) -> BTreeMap<String, ColumnMask> {
    view.columns
        .iter()
        .filter_map(|c| base.masks.get(&c.name).map(|m| (c.name.clone(), m.clone())))
        .collect()
}

/// After `base` changed in `ks`: its materialized views (not index views) take its masks again,
/// as new table versions so prepared reads of them are re-prepared.
pub fn sync_views(ks: &mut crate::schema::Keyspace, base: &Table) {
    let index_views: Vec<String> = base
        .indexes
        .iter()
        .map(|i| format!("{}_index", i.name))
        .collect();
    let views: Vec<Arc<Table>> = crate::views::views_of(ks, base)
        .filter(|v| !index_views.contains(&v.name))
        .cloned()
        .collect();
    for v in views {
        let masks = view_masks(base, &v);
        if masks != v.masks {
            let mut t = (*v).clone();
            t.masks = masks;
            t.version = crate::uuid::timeuuid();
            ks.tables.insert(t.name.clone(), Arc::new(t));
        }
    }
}

/// `TableMetadata.columns()` order, which `SelectStatement.authorize` lists masked columns in:
/// a Java `HashMap<ByteBuffer, ColumnMetadata>` (default capacity, grown at 0.75 load) iterated
/// by bucket (`ByteBuffer.hashCode` spread by `HashMap.hash`), ties in insertion order
/// (`CreateTableStatement.builder`: partition key, clustering, then the rest by name).
fn java_column_order(t: &Table) -> Vec<usize> {
    let mut cap = 16usize;
    while t.columns.len() > cap / 4 * 3 {
        cap *= 2;
    }
    let bucket = |name: &str| {
        let mut h: i32 = 1;
        for b in name.as_bytes().iter().rev() {
            h = h.wrapping_mul(31).wrapping_add(i32::from(*b as i8));
        }
        let h = h as u32;
        ((h ^ (h >> 16)) as usize) & (cap - 1)
    };
    let mut order: Vec<usize> = t.partition_key.clone();
    order.extend(t.clustering.iter().copied());
    let mut rest: Vec<usize> = (0..t.columns.len())
        .filter(|i| !order.contains(i))
        .collect();
    rest.sort_by(|a, b| {
        t.columns[*a]
            .name
            .as_bytes()
            .cmp(t.columns[*b].name.as_bytes())
    });
    order.extend(rest);
    order.sort_by_key(|i| bucket(&t.columns[*i].name));
    order
}
