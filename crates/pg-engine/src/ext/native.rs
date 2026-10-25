//! The native code behind the extensions' `LANGUAGE C` functions, by library and symbol
//! (what `dlopen` / `dlsym` find in PostgreSQL).

use crate::sql::func::{FnCtx, FnImpl, SrfImpl};
use crate::types::Value;

/// A C function's implementation: a scalar one, or a set-returning one.
#[derive(Clone, Copy)]
pub struct Native {
    pub imp: FnImpl,
    pub srf: Option<SrfImpl>,
}

fn not_scalar(_: &[Value], _: &FnCtx) -> crate::Result<Value> {
    Err(crate::error::internal("set-valued function called in context that cannot accept a set"))
}

impl Native {
    pub const fn scalar(imp: FnImpl) -> Native {
        Native { imp, srf: None }
    }

    pub const fn set(srf: SrfImpl) -> Native {
        Native {
            imp: not_scalar,
            srf: Some(srf),
        }
    }
}

/// The library a function's `probin` names: `$libdir/pgcrypto` (or a path, with or without
/// the `.so` suffix) is `pgcrypto`.
pub fn library(probin: &str) -> &str {
    let base = probin.rsplit('/').next().unwrap_or(probin);
    base.strip_suffix(".so").unwrap_or(base)
}

/// Whether the engine has native code for library `lib` (so a symbol it lacks is an error,
/// as `dlsym` failing is).
pub fn has_library(lib: &str) -> bool {
    matches!(lib, "_int" | "amcheck" | "btree_gin" | "btree_gist" | "citext" | "cube" | "earthdistance" | "fuzzystrmatch" | "hstore" | "isn" | "lo" | "ltree" | "pgcrypto"
            | "pg_buffercache" | "pg_stat_statements" | "pg_trgm" | "seg" | "tablefunc" | "unaccent" | "uuid-ossp")
}

/// A made operator class's support function `number` (for its input type), when it is a C
/// function: (probin, symbol).
pub fn opclass_support(cat: &crate::catalog::Catalog, opclass: u32, number: i16) -> Option<(String, String)> {
    let oc = cat.opclasses.get(&opclass)?;
    let m = cat
        .opmembers
        .values()
        .find(|m| m.func && m.family == oc.family && m.number == number && m.left == oc.intype)?;
    let f = cat.functions.get(&m.object)?;
    Some((f.probin.clone()?, f.body.clone()))
}

/// The options support function's number of an index access method (`amoptsprocnum`).
fn options_procnum(method: &str) -> Option<i16> {
    Some(match method {
        "btree" => 5,
        "hash" => 3,
        "gist" => 10,
        "gin" => 7,
        "spgist" => 6,
        "brin" => 5,
        _ => return None,
    })
}

/// What a made operator class's native options support function accepts.
pub fn user_opclass_options(
    cat: &crate::catalog::Catalog,
    opclass: u32,
    method: &str,
) -> Option<Vec<(&'static str, i64, i64, i64, i64)>> {
    let (bin, sym) = opclass_support(cat, opclass, options_procnum(method)?)?;
    opclass_options(&bin, &sym)
}

/// A new key of an index column of made operator class `opclass`, checked as its GiST
/// compress function checks it.
pub fn check_key_for(cat: &crate::catalog::Catalog, opclass: u32, key: &Value, opts: &[String]) -> crate::Result<()> {
    if cat.opclasses.get(&opclass).is_none_or(|c| c.method != 783) {
        return Ok(());
    }
    match opclass_support(cat, opclass, 3) {
        Some((bin, sym)) => check_index_key(&bin, &sym, key, opts),
        None => Ok(()),
    }
}

/// The options an operator class's options support function (`symbol` in `probin`)
/// accepts: (name, default, min, max) integer options.
pub fn opclass_options(probin: &str, symbol: &str) -> Option<Vec<(&'static str, i64, i64, i64, i64)>> {
    match library(probin) {
        "_int" => super::intarray::options(symbol),
        "ltree" => super::ltree::options(symbol),
        "pg_trgm" if symbol == "gtrgm_options" => Some(vec![("siglen", 12, 1, 2024, 1)]),
        // (SIGLEN_MAX: GISTMaxIndexKeySize.)
        "hstore" if symbol == "ghstore_options" => Some(vec![("siglen", 16, 1, 2024, 1)]),
        _ => None,
    }
}

/// What an index whose operator class's compress function is `symbol` in `probin` checks of
/// a new key (with the column's operator class options, `name=value`).
pub fn check_index_key(probin: &str, symbol: &str, key: &Value, opts: &[String]) -> crate::Result<()> {
    match library(probin) {
        "_int" => super::intarray::check_key(symbol, key, opts),
        _ => Ok(()),
    }
}

/// The implementation of `symbol` in the library `probin` names.
pub fn lookup(probin: &str, symbol: &str) -> Option<Native> {
    match library(probin) {
        "_int" => super::intarray::symbol(symbol),
        "amcheck" => super::amcheck::symbol(symbol),
        "pg_buffercache" => super::pg_buffercache::symbol(symbol),
        "btree_gin" => super::btree::btree_gin(symbol),
        "btree_gist" => super::btree::btree_gist(symbol),
        "ltree" => super::ltree::symbol(symbol),
        "citext" => super::citext::symbol(symbol),
        "hstore" => super::hstore::symbol(symbol),
        "fuzzystrmatch" => super::fuzzystrmatch::symbol(symbol),
        "isn" => super::isn::symbol(symbol),
        "lo" => super::lo::symbol(symbol),
        "tablefunc" => super::tablefunc::symbol(symbol),
        "unaccent" => super::unaccent::symbol(symbol),
        "cube" => super::cube::symbol(symbol),
        "earthdistance" => super::cube::earth_symbol(symbol),
        "seg" => super::seg::symbol(symbol),
        "pgcrypto" => super::pgcrypto::symbol(symbol),
        "pg_trgm" => super::pg_trgm::symbol(symbol),
        "pg_stat_statements" => super::pg_stat_statements::symbol(symbol),
        "uuid-ossp" => super::uuid_ossp::symbol(symbol),
        _ => None,
    }
}

/// A type's subscripting handler of native code (`SubscriptRoutines`): one text subscript,
/// fetching and assigning text values.
pub struct NativeSubscript {
    /// The type's name in its messages ("hstore allows only one subscript").
    pub what: &'static str,
    /// (value, key) to the element; strict.
    pub fetch: FnImpl,
    /// (value or NULL, key, new element or NULL) to the new value.
    pub assign: FnImpl,
}

/// The subscripting handler `symbol` of the library `probin` names.
pub fn subscript(probin: &str, symbol: &str) -> Option<NativeSubscript> {
    match (library(probin), symbol) {
        ("hstore", "hstore_subscript_handler") => Some(super::hstore::SUBSCRIPT),
        _ => None,
    }
}
/// What a C trigger function's `TriggerData` holds.
pub struct TriggerData<'a> {
    /// The trigger's name.
    pub name: &'a str,
    pub before: bool,
    /// FOR EACH ROW.
    pub row: bool,
    pub update: bool,
    pub delete: bool,
    pub table: &'a crate::catalog::Table,
    /// tgargs.
    pub args: &'a [String],
    pub old: Option<crate::exec::Row>,
    pub new: Option<crate::exec::Row>,
    /// The columns the UPDATE assigns (tg_updatedcols).
    pub updated_cols: &'a [usize],
}

/// A C trigger function: the row to go on with (BEFORE ROW), as the trigger returns it.
pub type TriggerImpl = fn(TriggerData, &crate::exec::ExecCtx) -> crate::Result<Option<crate::exec::Row>>;

/// The trigger function `symbol` in the library `probin` names.
pub fn trigger(probin: &str, symbol: &str) -> Option<TriggerImpl> {
    match library(probin) {
        "lo" => super::lo::trigger(symbol),
        _ => None,
    }
}

/// A dictionary of a text search template whose init function is `init` in library `lib`.
pub fn ts_dict(
    lib: &str,
    init: &str,
    options: &[crate::types::tsearch::config::DictOption],
) -> crate::Result<Option<Box<dyn crate::types::tsearch::config::ExtDict>>> {
    match (lib, init) {
        ("unaccent", "unaccent_init") => super::unaccent::init(options).map(Some),
        _ => Ok(None),
    }
}
