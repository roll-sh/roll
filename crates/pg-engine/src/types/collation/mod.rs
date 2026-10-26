//! Collations: what a collation does once an expression has derived it (pg_locale.c and its
//! providers). Comparison (`varstr_cmp`), equality and grouping under nondeterministic
//! collations, case mapping (upper, lower, initcap, casefold), and the regex engine's
//! character classification, per provider:
//!
//! - builtin (C, C.UTF-8, PG_UNICODE_FAST): byte order; Postgres's own Unicode case tables;
//! - libc: C and POSIX are ASCII-only; other locales behave as the reference server's musl
//!   does (byte order, its Unicode classes and simple case mappings);
//! - ICU: ICU4C 78.1, the reference server's ICU (see icu.rs).
//!
//! The database default ("default", oid 100) is the database's own locale (dblocale.rs:
//! pg_database's provider and locale, libc C unless initdb or CREATE DATABASE chose
//! another), which the catalog carries (`Catalog::db_locale`).

pub mod case;
pub mod dblocale;
pub mod icu;
#[cfg(feature = "icu4x")]
mod icu4x;
mod libc;
#[rustfmt::skip]
mod tables;

use crate::error::{Error, Result, code};
use crate::types::Value;
use crate::types::regex::Strategy;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub use dblocale::{DbDefault, DbLocale};

pub const DEFAULT_OID: u32 = 100;
pub const C_OID: u32 = 950;
pub const POSIX_OID: u32 = 951;

/// A pg_collation row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CollationDef {
    pub oid: u32,
    pub name: String,
    pub namespace: u32,
    pub owner: u32,
    /// 'd' default, 'c' libc, 'i' ICU, 'b' builtin.
    pub provider: char,
    pub deterministic: bool,
    pub encoding: i32,
    pub collate: Option<String>,
    pub ctype: Option<String>,
    pub locale: Option<String>,
    pub icu_rules: Option<String>,
    pub version: Option<String>,
}

impl CollationDef {
    /// The locale string the provider's version and behavior come from.
    pub fn locale_str(&self) -> &str {
        match self.provider {
            'c' => self.collate.as_deref().unwrap_or(""),
            _ => self.locale.as_deref().unwrap_or(""),
        }
    }
}

/// The collations Postgres 18's initdb made in the reference server, in oid order.
pub fn static_defs() -> &'static [CollationDef] {
    static DEFS: OnceLock<Vec<CollationDef>> = OnceLock::new();
    DEFS.get_or_init(|| {
        let field = |s: &str| crate::catalog::system::copy_field(s);
        include_str!("../../catalog/data/pg_collation.tsv")
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                CollationDef {
                    oid: f[0].parse().unwrap_or(0),
                    name: field(f[1]).unwrap_or_default(),
                    namespace: f[2].parse().unwrap_or(11),
                    owner: f[3].parse().unwrap_or(10),
                    provider: f[4].chars().next().unwrap_or('c'),
                    deterministic: f[5] == "t",
                    encoding: f[6].parse().unwrap_or(-1),
                    collate: field(f[7]),
                    ctype: field(f[8]),
                    locale: field(f[9]),
                    icu_rules: field(f[10]),
                    version: field(f[11]),
                }
            })
            .collect()
    })
}

/// (locale, version) of the imported ICU collations.
#[cfg(feature = "icu4x")]
pub(crate) fn static_icu_locales() -> &'static [(String, String)] {
    static LOCS: OnceLock<Vec<(String, String)>> = OnceLock::new();
    LOCS.get_or_init(|| {
        static_defs()
            .iter()
            .filter(|d| d.provider == 'i')
            .filter_map(|d| Some((d.locale.clone()?, d.version.clone()?)))
            .collect()
    })
}

/// A built-in collation by name.
pub fn static_by_name(name: &str) -> Option<&'static CollationDef> {
    static BY_NAME: OnceLock<HashMap<&'static str, usize>> = OnceLock::new();
    let map = BY_NAME.get_or_init(|| {
        static_defs()
            .iter()
            .enumerate()
            .map(|(i, d)| (d.name.as_str(), i))
            .collect()
    });
    map.get(name).map(|&i| &static_defs()[i])
}

/// A built-in collation by oid.
pub fn static_by_oid(oid: u32) -> Option<&'static CollationDef> {
    static BY_OID: OnceLock<HashMap<u32, usize>> = OnceLock::new();
    let map = BY_OID.get_or_init(|| {
        static_defs()
            .iter()
            .enumerate()
            .map(|(i, d)| (d.oid, i))
            .collect()
    });
    map.get(&oid).map(|&i| &static_defs()[i])
}

/// How a collation classifies characters and maps case.
#[derive(Clone, Debug)]
pub enum Ctype {
    /// ctype C (C, POSIX, builtin C): ASCII only.
    Ascii,
    /// The builtin provider: simple mappings (C.UTF-8) or full ones (PG_UNICODE_FAST).
    Builtin { full: bool },
    /// A libc locale other than C/POSIX (musl).
    Libc,
    /// ICU, under the collation's locale.
    Icu(Arc<icu::Icu>),
}

/// An opened collation (`pg_locale_t`).
#[derive(Clone, Debug)]
pub struct Collation {
    pub oid: u32,
    pub name: String,
    pub provider: char,
    pub deterministic: bool,
    pub ctype: Ctype,
    /// `collate_is_c`: strings sort by their bytes, as the planner knows (a libc locale
    /// other than C or POSIX isn't, though musl's strcoll compares bytes too).
    pub collate_is_c: bool,
    icu: Option<Arc<icu::Icu>>,
}

/// `builtin_validate_locale`'s canonical names.
pub fn builtin_canonical(locale: &str) -> Option<&'static str> {
    match locale {
        "C" => Some("C"),
        "C.UTF-8" | "C.UTF8" => Some("C.UTF-8"),
        "PG_UNICODE_FAST" => Some("PG_UNICODE_FAST"),
        _ => None,
    }
}

fn is_c_locale(l: &str) -> bool {
    l == "C" || l == "POSIX"
}

impl Collation {
    /// `pg_newlocale_from_collation`.
    pub fn open(def: &CollationDef) -> Result<Collation> {
        let mut icu = None;
        let ctype = match def.provider {
            // (pg_collation's own "default" row: libc C, the default locale.)
            'd' => Ctype::Ascii,
            'b' => match def.locale.as_deref().and_then(builtin_canonical) {
                Some("C") => Ctype::Ascii,
                Some("PG_UNICODE_FAST") => Ctype::Builtin { full: true },
                Some(_) => Ctype::Builtin { full: false },
                None => {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!(
                            "invalid locale name \"{}\" for builtin provider",
                            def.locale.as_deref().unwrap_or("")
                        ),
                    ));
                }
            },
            'i' => {
                let opened = Arc::new(icu::open(
                    def.locale.as_deref().unwrap_or(""),
                    def.icu_rules.as_deref(),
                )?);
                icu = Some(opened.clone());
                Ctype::Icu(opened)
            }
            _ => {
                if is_c_locale(def.ctype.as_deref().unwrap_or("C")) {
                    Ctype::Ascii
                } else {
                    Ctype::Libc
                }
            }
        };
        let collate_is_c = match def.provider {
            'c' => is_c_locale(def.collate.as_deref().unwrap_or("C")),
            'i' => false,
            _ => true,
        };
        Ok(Collation {
            oid: def.oid,
            name: def.name.clone(),
            provider: def.provider,
            deterministic: def.deterministic,
            ctype,
            collate_is_c,
            icu,
        })
    }

    /// Whether strings compare as bytes (`collate_is_c`, or a libc locale whose strcoll is
    /// strcmp).
    pub fn bytewise(&self) -> bool {
        self.icu.is_none()
    }

    /// `ctype_is_c`.
    pub fn ctype_is_c(&self) -> bool {
        matches!(self.ctype, Ctype::Ascii)
    }

    /// `pg_strcoll` (no tie-break).
    pub fn strcoll(&self, a: &str, b: &str) -> Ordering {
        match &self.icu {
            None => a.as_bytes().cmp(b.as_bytes()),
            Some(i) => i.compare(a, b),
        }
    }

    /// `varstr_cmp`: strcoll, with deterministic collations breaking ties by bytes.
    pub fn cmp(&self, a: &str, b: &str) -> Ordering {
        if a == b {
            return Ordering::Equal;
        }
        match &self.icu {
            None => a.as_bytes().cmp(b.as_bytes()),
            Some(i) => match i.compare(a, b) {
                Ordering::Equal if self.deterministic => a.as_bytes().cmp(b.as_bytes()),
                o => o,
            },
        }
    }

    /// texteq under the collation.
    pub fn eq(&self, a: &str, b: &str) -> bool {
        if self.deterministic {
            a == b
        } else {
            self.cmp(a, b) == Ordering::Equal
        }
    }

    /// A key equal for strings the collation finds equal (for hashing and grouping under a
    /// nondeterministic collation: its sort key, with the terminating NUL hashtext hashes).
    pub fn key(&self, s: &str) -> Vec<u8> {
        match (&self.icu, self.deterministic) {
            (Some(i), false) => i.sort_key(s),
            _ => s.as_bytes().to_vec(),
        }
    }

    pub fn upper(&self, s: &str) -> String {
        match &self.ctype {
            Ctype::Ascii => s.to_ascii_uppercase(),
            Ctype::Builtin { full } => case::convert(s, case::CaseKind::Upper, *full),
            Ctype::Libc => libc::upper(s),
            Ctype::Icu(i) => i.upper(s),
        }
    }

    pub fn lower(&self, s: &str) -> String {
        match &self.ctype {
            Ctype::Ascii => s.to_ascii_lowercase(),
            Ctype::Builtin { full } => case::convert(s, case::CaseKind::Lower, *full),
            Ctype::Libc => libc::lower(s),
            Ctype::Icu(i) => i.lower(s),
        }
    }

    pub fn casefold(&self, s: &str) -> String {
        match &self.ctype {
            Ctype::Ascii => s.to_ascii_lowercase(),
            Ctype::Builtin { full } => case::convert(s, case::CaseKind::Fold, *full),
            Ctype::Libc => libc::lower(s),
            Ctype::Icu(i) => i.fold(s),
        }
    }

    pub fn initcap(&self, s: &str) -> String {
        match &self.ctype {
            Ctype::Ascii => {
                let mut out = String::with_capacity(s.len());
                let mut prev_alnum = false;
                for c in s.chars() {
                    let c = if prev_alnum {
                        c.to_ascii_lowercase()
                    } else {
                        c.to_ascii_uppercase()
                    };
                    out.push(c);
                    prev_alnum = c.is_ascii_alphanumeric();
                }
                out
            }
            Ctype::Builtin { full } => {
                let posix = !*full;
                case::title(s, *full, |c| {
                    Strategy::Builtin { posix }.isalnum(c)
                })
            }
            Ctype::Libc => libc::title(s),
            Ctype::Icu(i) => i.title(s),
        }
    }

    /// The regex engine's strategy (`pg_set_regex_collation`).
    pub fn regex_strategy(&self) -> Result<Strategy> {
        if !self.deterministic {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "nondeterministic collations are not supported for regular expressions",
            ));
        }
        Ok(match &self.ctype {
            Ctype::Ascii => Strategy::C,
            Ctype::Builtin { full } => Strategy::Builtin { posix: !*full },
            Ctype::Libc => Strategy::Libc,
            Ctype::Icu(_) => Strategy::Icu,
        })
    }
}

/// Character probes and simple case mappings of the libc and ICU strategies, for the regex
/// engine.
pub mod probes {
    pub use super::libc::{
        isalnum as libc_isalnum, isalpha as libc_isalpha, isdigit as libc_isdigit,
        isgraph as libc_isgraph, islower as libc_islower, isprint as libc_isprint,
        ispunct as libc_ispunct, isspace as libc_isspace, isupper as libc_isupper,
        tolower as libc_tolower, toupper as libc_toupper,
    };
    pub use super::icu::probes::{
        isalnum as icu_isalnum, isalpha as icu_isalpha, isdigit as icu_isdigit,
        isgraph as icu_isgraph, islower as icu_islower, isprint as icu_isprint,
        ispunct as icu_ispunct, isspace as icu_isspace, isupper as icu_isupper,
        tolower as icu_tolower, toupper as icu_toupper,
    };
}

/// Opened collations, by definition.
fn cache() -> &'static Mutex<HashMap<String, Arc<Collation>>> {
    static C: OnceLock<Mutex<HashMap<String, Arc<Collation>>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The opened collation of a definition (cached).
pub fn get(def: &CollationDef) -> Result<Arc<Collation>> {
    let key = format!(
        "{}\u{1}{}\u{1}{}\u{1}{:?}\u{1}{:?}\u{1}{:?}\u{1}{:?}",
        def.oid, def.name, def.provider, def.deterministic, def.locale, def.ctype, def.icu_rules
    );
    if let Some(c) = cache().lock().map(|m| m.get(&key).cloned()).ok().flatten() {
        return Ok(c);
    }
    let c = Arc::new(Collation::open(def)?);
    if let Ok(mut m) = cache().lock() {
        if m.len() > 4096 {
            m.clear();
        }
        m.insert(key, c.clone());
    }
    Ok(c)
}

/// libc C, the default of a database whose catalog carries no locale.
fn libc_c() -> &'static Arc<Collation> {
    static D: OnceLock<Arc<Collation>> = OnceLock::new();
    D.get_or_init(|| DbDefault::open(DbLocale::default()).expect("libc C").collation)
}

/// The default collation of the database `cat` is a catalog of (`init_database_collation`).
pub fn default_for(cat: &crate::catalog::Catalog) -> Arc<Collation> {
    match &cat.db_locale {
        Some(d) => d.collation.clone(),
        None => libc_c().clone(),
    }
}

/// The locale of the database `cat` is a catalog of.
pub fn db_locale(cat: &crate::catalog::Catalog) -> DbLocale {
    cat.db_locale.as_ref().map(|d| d.locale.clone()).unwrap_or_default()
}

thread_local! {
    /// The default collation of the database whose statement runs on this thread, for what
    /// runs under DEFAULT_COLLATION_OID with no catalog at hand (text search, jsonpath's
    /// like_regex, calls the binder attached no collation to); set from the catalog by
    /// `binder::note_catalog`.
    static DEFAULT: std::cell::RefCell<Option<Arc<DbDefault>>> = const { std::cell::RefCell::new(None) };
}

/// Make `cat`'s database default the thread's (`default`).
pub fn note_default(cat: &crate::catalog::Catalog) {
    let d = cat.db_locale.as_ref();
    DEFAULT.with(|c| {
        let mut c = c.borrow_mut();
        if c.as_ref().map(Arc::as_ptr) != d.map(Arc::as_ptr) {
            *c = d.cloned();
        }
    });
}

/// The default collation of the database whose statement runs on this thread (libc C when
/// none has run).
pub fn default() -> Arc<Collation> {
    DEFAULT
        .with(|c| c.borrow().as_ref().map(|d| d.collation.clone()))
        .unwrap_or_else(|| libc_c().clone())
}

/// `default()` when it doesn't compare by bytes (an ICU database), for what compares
/// under DEFAULT_COLLATION_OID on its own (jsonb).
pub fn default_unless_bytewise() -> Option<Arc<Collation>> {
    DEFAULT.with(|c| {
        c.borrow()
            .as_ref()
            .filter(|d| !d.collation.bytewise())
            .map(|d| d.collation.clone())
    })
}

/// `database_ctype_is_c` of the database whose statement runs on this thread: text search
/// classifies characters by the database's LC_CTYPE (libc's), whatever its provider.
pub fn database_ctype_is_c() -> bool {
    DEFAULT.with(|c| c.borrow().as_ref().is_none_or(|d| d.locale.ctype_is_c()))
}

/// A collation by (unqualified) name: the catalog's own ones, then the built-in ones;
/// "default" is the database's.
pub fn by_name(cat: &crate::catalog::Catalog, name: &str) -> Option<Arc<Collation>> {
    if name == "default" {
        return Some(default_for(cat));
    }
    def_by_name(cat, name).and_then(|d| get(&d).ok())
}

/// A collation by oid, as `by_name`.
pub fn by_oid(cat: &crate::catalog::Catalog, oid: u32) -> Option<Arc<Collation>> {
    if oid == DEFAULT_OID {
        return Some(default_for(cat));
    }
    def_by_oid(cat, oid).and_then(|d| get(&d).ok())
}

/// A collation's definition by (unqualified) name.
pub fn def_by_name(cat: &crate::catalog::Catalog, name: &str) -> Option<CollationDef> {
    if let Some(d) = cat.collations.values().find(|d| d.name == name) {
        return Some((**d).clone());
    }
    static_by_name(name).cloned()
}

/// A collation's definition by oid.
pub fn def_by_oid(cat: &crate::catalog::Catalog, oid: u32) -> Option<CollationDef> {
    if let Some(d) = cat.collations.get(&oid) {
        return Some((**d).clone());
    }
    static_by_oid(oid).cloned()
}

/// Compare two values under a collation: strings by it, arrays element by element; others
/// as their type does.
pub fn cmp_values(c: &Collation, a: &Value, b: &Value) -> Ordering {
    // A stand-in for a made btree operator class's comparison function.
    if let Some(o) = crate::catalog::partition::comparator_cmp(c, a, b) {
        return o;
    }
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => c.cmp(x, y),
        // Ranges of a collatable subtype: their bounds under it.
        (Value::Range(_), Value::Range(_)) | (Value::Multirange(_), Value::Multirange(_)) => {
            let _g = crate::types::range::under(Some(Arc::new(c.clone())));
            a.cmp_total(b)
        }
        (Value::Array(x), Value::Array(y)) => {
            for (p, q) in x.items.iter().zip(y.items.iter()) {
                let o = match (p.is_null(), q.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    _ => cmp_values(c, p, q),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            x.items.len().cmp(&y.items.len()).then_with(|| {
                x.dims
                    .iter()
                    .map(|d| (d.0, d.1))
                    .cmp(y.dims.iter().map(|d| (d.0, d.1)))
            })
        }
        _ => a.cmp_total(b),
    }
}

/// Equality of two values under a collation.
pub fn eq_values(c: &Collation, a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => c.eq(x, y),
        (Value::Array(x), Value::Array(y)) => {
            x.items.len() == y.items.len()
                && x.dims == y.dims
                && x.items.iter().zip(y.items.iter()).all(|(p, q)| match (p.is_null(), q.is_null()) {
                    (true, true) => true,
                    (false, false) => eq_values(c, p, q),
                    _ => false,
                })
        }
        _ => a.cmp_total(b) == Ordering::Equal,
    }
}

/// A value with its strings replaced by keys equal under a nondeterministic collation (for
/// hashing and grouping).
pub fn key_value(c: &Collation, v: &Value) -> Value {
    if c.deterministic {
        return v.clone();
    }
    match v {
        Value::Text(s) => Value::bytea(c.key(s)),
        Value::Array(a) => {
            let mut a = (**a).clone();
            for x in a.items.iter_mut() {
                *x = key_value(c, x);
            }
            Value::Array(Arc::new(a))
        }
        v => v.clone(),
    }
}

/// `get_collation_actual_version`.
pub fn actual_version(provider: char, locale: &str) -> Result<Option<String>> {
    Ok(match provider {
        'b' => match builtin_canonical(locale) {
            Some(_) => Some("1".into()),
            None => {
                return Err(Error::new(
                    code::WRONG_OBJECT_TYPE,
                    format!("invalid locale name \"{locale}\" for builtin provider"),
                ));
            }
        },
        'i' => Some(icu::version(locale)?),
        // musl reports no version.
        _ => None,
    })
}

/// Functions whose behavior depends on their input collation beyond those the binder already
/// checks for one (comparisons, case mapping, LIKE, regular expressions).
pub fn sensitive(def: &crate::sql::func::FuncDef) -> bool {
    matches!(
        def.name,
        "hashtext" | "hashtextextended" | "string_to_array" | "string_to_table" | "array_sort"
    )
}

/// `def` running under the collation named `name` (an input collation the binder derived),
/// when that behaves differently from the database default; C-like collations take the
/// ASCII variant `c_def`.
pub fn apply(
    def: &Arc<crate::sql::func::FuncDef>,
    name: &str,
    cat: &crate::catalog::Catalog,
    c_def: Option<&crate::sql::func::FuncDef>,
) -> Option<Arc<crate::sql::func::FuncDef>> {
    if matches!(name, "C" | "POSIX" | "ucs_basic") {
        return None;
    }
    let coll = by_name(cat, name)?;
    if coll.ctype_is_c() && coll.bytewise() && coll.deterministic {
        return c_def.map(|d| Arc::new(d.clone()));
    }
    // (A call without a collation runs under the database default, `default()`.)
    if name == "default" && coll.bytewise() {
        return None;
    }
    Some(Arc::new(crate::sql::func::FuncDef {
        coll: Some(coll),
        ..(**def).clone()
    }))
}

impl Collation {
    /// `text_position_next_internal` under a nondeterministic collation: from byte `start`,
    /// the first position where a nonempty substring of `hay` equals `needle`, and that
    /// substring's length (the longest such when `greedy`).
    pub fn find(&self, hay: &str, needle: &str, start: usize, greedy: bool) -> Option<(usize, usize)> {
        let mut h = start;
        while h < hay.len() {
            if !greedy
                && hay.len() - h >= needle.len()
                && hay.is_char_boundary(h + needle.len())
                && self.strcoll(&hay[h..h + needle.len()], needle) == Ordering::Equal
            {
                return Some((h, needle.len()));
            }
            let mut found = None;
            for (off, ch) in hay[h..].char_indices() {
                let end = h + off + ch.len_utf8();
                if self.strcoll(&hay[h..end], needle) == Ordering::Equal {
                    found = Some((h, end - h));
                    if !greedy {
                        break;
                    }
                }
            }
            if found.is_some() {
                return found;
            }
            h += hay[h..].chars().next().map_or(1, |c| c.len_utf8());
        }
        None
    }

    /// The pieces of `s` between matches of `sep` (greedy), for split_part and
    /// string_to_array.
    pub fn split<'a>(&self, s: &'a str, sep: &str) -> Vec<&'a str> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some((at, len)) = self.find(s, sep, from, true) {
            out.push(&s[from..at]);
            from = at + len;
        }
        out.push(&s[from..]);
        out
    }
}

/// The error substring searches give under a nondeterministic collation they can't run with.
pub fn nondeterministic_search_error() -> Error {
    Error::new(
        code::FEATURE_NOT_SUPPORTED,
        "nondeterministic collations are not supported for substring searches",
    )
}

/// Whether a (possibly schema-qualified) collation name resolves.
pub fn exists(cat: &crate::catalog::Catalog, names: &[String]) -> bool {
    match names {
        [name] => def_by_name(cat, name).is_some(),
        [_, ..] => resolve(cat, &[], names).is_some(),
        [] => false,
    }
}

/// `get_collation_oid`: a (possibly schema-qualified) collation name through the search path.
pub fn resolve(
    cat: &crate::catalog::Catalog,
    search_path: &[String],
    names: &[String],
) -> Option<CollationDef> {
    let in_schema = |ns: u32, name: &str| -> Option<CollationDef> {
        if ns == crate::catalog::PG_CATALOG_OID
            && let Some(d) = static_by_name(name)
        {
            return Some(d.clone());
        }
        cat.collations
            .values()
            .find(|d| d.namespace == ns && d.name == name)
            .map(|d| (**d).clone())
    };
    match names {
        [name] => cat
            .effective_path(search_path)
            .into_iter()
            .find_map(|ns| in_schema(ns, name)),
        [.., schema, name] => in_schema(cat.schema_by_name(schema)?.oid, name),
        [] => None,
    }
}

/// The "does not exist" error of a collation name.
pub fn no_such_collation(names: &[String]) -> Error {
    Error::new(
        code::UNDEFINED_OBJECT,
        format!(
            "collation \"{}\" for encoding \"UTF8\" does not exist",
            names.join(".")
        ),
    )
}

/// Per key column of an index, the nondeterministic collation its uniqueness follows (its
/// COLLATE, or the column's); empty when none has one.
pub fn index_nondet(
    cat: &crate::catalog::Catalog,
    t: &crate::catalog::Table,
    ix: &crate::catalog::Index,
) -> Vec<Option<Arc<Collation>>> {
    if cat.collations.values().all(|d| d.deterministic) {
        return Vec::new();
    }
    let v: Vec<Option<Arc<Collation>>> = ix
        .columns
        .iter()
        .map(|c| {
            let name = c.collation.clone().or_else(|| match c.key {
                crate::catalog::IndexKey::Column(i) => {
                    t.columns.get(i).and_then(|col| col.collation.clone())
                }
                crate::catalog::IndexKey::Expr(_) => None,
            })?;
            by_name(cat, &name).filter(|k| !k.deterministic)
        })
        .collect();
    if v.iter().any(Option::is_some) { v } else { Vec::new() }
}

/// Whether two key tuples are equal under per-column collations (NULLs equal only when
/// `nulls_equal`).
pub fn keys_equal(a: &[Value], b: &[Value], colls: &[Option<Arc<Collation>>], nulls_equal: bool) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).enumerate().all(|(i, (x, y))| match (x.is_null(), y.is_null()) {
            (true, true) => nulls_equal,
            (false, false) => match colls.get(i).and_then(Option::as_ref) {
                Some(c) => eq_values(c, x, y),
                None => x.cmp_total(y) == Ordering::Equal,
            },
            _ => false,
        })
}
