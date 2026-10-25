//! Run-time parameters (GUCs): `SET`, `SHOW`, `current_setting`, `set_config`, `SET LOCAL`.
//!
//! Every parameter Postgres 18 has is described by `gucs.tsv` (its `pg_settings` row: type,
//! unit, context, range, enum values, defaults, flags). Values are kept as strings in the
//! parameter's base unit; SET inside a transaction goes on a per-parameter stack by
//! (sub)transaction nest level, undone or merged at savepoint and transaction end the way
//! `AtEOXact_GUC` does.

use crate::error::{Error, Result, code};
use crate::types::datetime::TimeZone;
use crate::types::text::{FormatCtx, IntervalStyle};
use crate::{bail, err};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// The unit an integer or real parameter's value is kept in.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Unit {
    None,
    Bytes,
    Kb,
    Mb,
    /// 8kB blocks (BLCKSZ and XLOG_BLCKSZ).
    Blocks,
    Ms,
    S,
    Min,
}

impl Unit {
    fn parse(s: &str) -> Unit {
        match s {
            "B" => Unit::Bytes,
            "kB" => Unit::Kb,
            "MB" => Unit::Mb,
            "8kB" => Unit::Blocks,
            "ms" => Unit::Ms,
            "s" => Unit::S,
            "min" => Unit::Min,
            _ => Unit::None,
        }
    }

    fn name(self) -> Option<&'static str> {
        Some(match self {
            Unit::None => return None,
            Unit::Bytes => "B",
            Unit::Kb => "kB",
            Unit::Mb => "MB",
            Unit::Blocks => "8kB",
            Unit::Ms => "ms",
            Unit::S => "s",
            Unit::Min => "min",
        })
    }

    fn is_memory(self) -> bool {
        matches!(self, Unit::Bytes | Unit::Kb | Unit::Mb | Unit::Blocks)
    }

    /// `memory_unit_conversion_table` / `time_unit_conversion_table` for this base unit:
    /// (unit, multiplier to the base unit), largest first.
    fn table(self) -> Vec<(&'static str, f64)> {
        let mem = |base: f64| {
            vec![
                ("TB", 1024.0 * 1024.0 * 1024.0 * 1024.0 / base),
                ("GB", 1024.0 * 1024.0 * 1024.0 / base),
                ("MB", 1024.0 * 1024.0 / base),
                ("kB", 1024.0 / base),
                ("B", 1.0 / base),
            ]
        };
        let time = |base: f64| {
            vec![
                ("d", 86_400_000.0 / base),
                ("h", 3_600_000.0 / base),
                ("min", 60_000.0 / base),
                ("s", 1000.0 / base),
                ("ms", 1.0 / base),
                ("us", 0.001 / base),
            ]
        };
        match self {
            Unit::None => Vec::new(),
            Unit::Bytes => mem(1.0),
            Unit::Kb => mem(1024.0),
            Unit::Mb => mem(1024.0 * 1024.0),
            Unit::Blocks => mem(8192.0),
            Unit::Ms => time(1.0),
            Unit::S => time(1000.0),
            Unit::Min => time(60_000.0),
        }
    }

    fn hint(self) -> &'static str {
        if self.is_memory() {
            "Valid units for this parameter are \"B\", \"kB\", \"MB\", \"GB\", and \"TB\"."
        } else {
            "Valid units for this parameter are \"us\", \"ms\", \"s\", \"min\", \"h\", and \"d\"."
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Str,
    Bool,
    Int(Unit),
    Real(Unit),
    Enum(&'static [&'static str]),
}

/// `GucContext`: when a parameter may change.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Context {
    Internal,
    Postmaster,
    Sighup,
    SuBackend,
    Backend,
    Superuser,
    User,
}

impl Context {
    fn parse(s: &str) -> Context {
        match s {
            "internal" => Context::Internal,
            "postmaster" => Context::Postmaster,
            "sighup" => Context::Sighup,
            "superuser-backend" => Context::SuBackend,
            "backend" => Context::Backend,
            "superuser" => Context::Superuser,
            _ => Context::User,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Context::Internal => "internal",
            Context::Postmaster => "postmaster",
            Context::Sighup => "sighup",
            Context::SuBackend => "superuser-backend",
            Context::Backend => "backend",
            Context::Superuser => "superuser",
            Context::User => "user",
        }
    }
}

struct Def {
    name: &'static str,
    /// The value a session starts with (reset_val of an initdb'd server).
    default: &'static str,
    boot: &'static str,
    kind: Kind,
    context: Context,
    category: &'static str,
    desc: &'static str,
    extra_desc: &'static str,
    /// Range of an integer or real parameter, and how pg_settings shows it.
    min: f64,
    max: f64,
    min_s: &'static str,
    max_s: &'static str,
    /// pg_settings_get_flags.
    flags: &'static [&'static str],
    /// pg_settings.source when the session hasn't changed it.
    source: &'static str,
    /// An extension library's parameter (`ext_gucs.tsv`): the library's prefix. A session
    /// has it once it has loaded the library.
    module: Option<&'static str>,
}

impl Def {
    fn flag(&self, f: &str) -> bool {
        self.flags.contains(&f)
    }
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

/// FxHash over a name's bytes: parameter names are short, so this beats SipHash by a lot on
/// the lookups every statement makes.
#[derive(Clone, Copy, Default)]
struct NameHasher(u64);

impl std::hash::Hasher for NameHasher {
    fn write(&mut self, bytes: &[u8]) {
        let mut add = |w: u64| {
            self.0 = (self.0.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95);
        };
        let (words, rest) = bytes.as_chunks::<8>();
        for w in words {
            add(u64::from_le_bytes(*w));
        }
        if !rest.is_empty() {
            let mut w = [0u8; 8];
            w[..rest.len()].copy_from_slice(rest);
            add(u64::from_le_bytes(w) ^ ((rest.len() as u64) << 59));
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

type NameMap<V> = HashMap<String, V, std::hash::BuildHasherDefault<NameHasher>>;

/// Longer than every built-in parameter's name (`find` folds case into a buffer this big).
const MAX_NAME: usize = 64;

struct Table {
    defs: Vec<Def>,
    /// Each parameter by its lower-case and its canonical name (and `names`, which is
    /// client_encoding).
    by_name: HashMap<&'static str, usize, std::hash::BuildHasherDefault<NameHasher>>,
}

fn table() -> &'static Table {
    static T: OnceLock<Table> = OnceLock::new();
    T.get_or_init(|| {
        let mut defs = Vec::new();
        let builtin = include_str!("gucs.tsv").lines().map(|l| (l, false));
        let modules = include_str!("ext_gucs.tsv").lines().map(|l| (l, true));
        for (line, module) in builtin.chain(modules) {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let f: Vec<String> = line.split('\t').map(unescape).collect();
            let s = |i: usize| leak(f.get(i).cloned().unwrap_or_default());
            let unit = Unit::parse(&f[2]);
            let kind = match f[1].as_str() {
                "bool" => Kind::Bool,
                "integer" => Kind::Int(unit),
                "real" => Kind::Real(unit),
                "enum" => {
                    let vals: Vec<&'static str> =
                        f[9].split(',').map(|v| leak(v.to_string())).collect();
                    Kind::Enum(Box::leak(vals.into_boxed_slice()))
                }
                _ => Kind::Str,
            };
            let flags: Vec<&'static str> = f[12]
                .split(',')
                .filter(|v| !v.is_empty())
                .map(|v| leak(v.to_string()))
                .collect();
            defs.push(Def {
                name: s(0),
                kind,
                context: Context::parse(&f[3]),
                category: s(4),
                desc: s(5),
                extra_desc: s(6),
                min: f[7].parse().unwrap_or(f64::MIN),
                max: f[8].parse().unwrap_or(f64::MAX),
                min_s: s(7),
                max_s: s(8),
                boot: s(10),
                default: s(11),
                flags: Box::leak(flags.into_boxed_slice()),
                source: s(13),
                module: module.then(|| leak(f[0].split('.').next().unwrap_or("").to_string())),
            });
        }
        // (By name, as pg_settings lists them: the libraries' among the built-ins.)
        defs.sort_by_key(|d| d.name.to_ascii_lowercase());
        let mut by_name: HashMap<&'static str, usize, _> = HashMap::default();
        for (i, d) in defs.iter().enumerate() {
            debug_assert!(d.name.len() <= MAX_NAME);
            by_name.insert(leak(d.name.to_ascii_lowercase()), i);
            by_name.insert(d.name, i);
        }
        // `SET NAMES`: a name Postgres maps to another.
        if let Some(&i) = by_name.get("client_encoding") {
            by_name.insert("names", i);
        }
        Table { defs, by_name }
    })
}

/// The built-in parameter `name` (any case): not an extension library's.
fn find(name: &str) -> Option<&'static Def> {
    find_any(name).filter(|d| d.module.is_none())
}

/// The prefix library `lib` reserves when it defines parameters (`ext_gucs.tsv`).
fn find_module_prefix(lib: &str) -> Option<&'static str> {
    table().defs.iter().filter_map(|d| d.module).find(|m| *m == lib)
}

/// The parameter `name` (any case), an extension library's too, without allocating.
fn find_any(name: &str) -> Option<&'static Def> {
    let t = table();
    if let Some(&i) = t.by_name.get(name) {
        return Some(&t.defs[i]);
    }
    let b = name.as_bytes();
    if b.len() > MAX_NAME || !b.iter().any(u8::is_ascii_uppercase) {
        return None;
    }
    let mut buf = [0u8; MAX_NAME];
    for (o, c) in buf.iter_mut().zip(b) {
        *o = c.to_ascii_lowercase();
    }
    let folded = std::str::from_utf8(&buf[..b.len()]).ok()?;
    t.by_name.get(folded).map(|&i| &t.defs[i])
}

/// Whether `name` is a built-in parameter (not a custom `prefix.name` one).
pub fn is_builtin(name: &str) -> bool {
    find(name).is_some()
}

/// Parameters reported to the client in ParameterStatus when they change.
pub const REPORTED: &[&str] = &[
    "application_name",
    "client_encoding",
    "DateStyle",
    "default_transaction_read_only",
    "in_hot_standby",
    "integer_datetimes",
    "IntervalStyle",
    "is_superuser",
    "scram_iterations",
    "search_path",
    "server_encoding",
    "server_version",
    "session_authorization",
    "standard_conforming_strings",
    "TimeZone",
];

/// What a SET at some nest level did (`GucStackState`).
#[derive(Clone, Copy, Debug, PartialEq)]
enum StackState {
    /// A function's SET clause: restored when the function ends.
    Save,
    Set,
    Local,
    SetLocal,
}

/// A parameter's value saved when it was first changed at a nest level (`GucStack`).
#[derive(Clone, Debug)]
struct Stacked {
    level: usize,
    state: StackState,
    /// The value before this level changed it (None: not set in the session).
    prior: Option<String>,
    /// SET LOCAL after SET at the same level: the value SET gave.
    masked: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// Values set in the session, by canonical name (a custom parameter's in lower case).
    values: NameMap<String>,
    /// Per-role / per-database defaults applied at login (what RESET returns to).
    defaults: NameMap<String>,
    /// Saved values by nest level, per parameter (only while a transaction is open).
    stack: NameMap<Vec<Stacked>>,
    /// The current (sub)transaction nest level: 1 in a transaction, +1 per savepoint.
    level: usize,
    /// Prefixes reserved by loaded modules (`MarkGUCPrefixReserved`).
    reserved: Vec<&'static str>,
    /// Names whose value changed since the last report (for ParameterStatus).
    pub changed: Vec<String>,
    /// What every statement derives from the settings, built on first use and dropped when
    /// a value changes (see `touched`).
    derived: DerivedCache,
    /// The values `start_transaction` replaced (of `TXN_START`'s parameters), and its nest
    /// level: the GUC stack entries its SET LOCALs make, until something needs them there
    /// (`stack_txn_start`).
    txn_start: Option<(usize, [Option<String>; 2])>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            values: NameMap::default(),
            defaults: NameMap::default(),
            stack: NameMap::default(),
            level: 1,
            reserved: Vec::new(),
            changed: Vec::new(),
            derived: DerivedCache::default(),
            txn_start: None,
        }
    }
}

/// Settings every statement reads, computed once per change of the settings (a session
/// that SETs nothing builds them once).
#[derive(Clone)]
struct Derived {
    /// Unique in the process for this set of values: what was derived from other values has
    /// another (caches of what the settings shape compare it).
    generation: u64,
    /// The values themselves (`values_key`).
    values_key: Arc<ValuesKey>,
    /// `format_ctx` (without a clock or names).
    fmt: FormatCtx,
    lex: crate::sql::scs::LexOpts,
    /// `statement_timeout` and `max_stack_depth` (ms, kB).
    statement_timeout: i64,
    max_stack_depth: i64,
    /// `search_path` split into schema names (None: `$user`).
    search_path: Vec<Option<String>>,
    /// `role` and `session_authorization`.
    role: String,
    session_authorization: String,
    /// `session_search_path`'s last answer.
    path_memo: PathMemo,
    /// A new transaction's characteristics (`default_transaction_isolation`, `…_read_only`),
    /// and `lock_timeout` (ms).
    default_txn: (crate::txn::Isolation, bool),
    lock_timeout: i64,
}

/// A session's search path (`session_search_path`) for a `$user` and backend id: what every
/// statement looks names up in, worked out once per change of the settings, user or backend.
#[derive(Default)]
struct PathMemo(parking_lot::Mutex<Option<(String, i32, Vec<String>)>>);

impl Clone for PathMemo {
    fn clone(&self) -> Self {
        PathMemo::default()
    }
}

#[derive(Clone, Default)]
struct DerivedCache(OnceLock<Box<Derived>>);

/// The settings' values set in a session or by its role / database defaults, by canonical
/// name, sorted (`Settings::values_key`); the rest have their built-in defaults.
#[derive(Debug, PartialEq, Eq)]
pub struct ValuesKey(Vec<(String, String)>);

impl std::fmt::Debug for DerivedCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DerivedCache")
    }
}

/// Parameters every transaction sets and resets (`txn_settings`) that nothing in `Derived`
/// reads: changing them keeps the cache.
const NOT_DERIVED: &[&str] = &[
    "transaction_isolation",
    "transaction_read_only",
    "transaction_deferrable",
];

/// What a transaction's start sets (`start_transaction`).
const TXN_START: [&str; 2] = ["transaction_isolation", "transaction_read_only"];

pub fn canonical_name(name: &str) -> String {
    match find(name) {
        Some(d) => d.name.to_string(),
        None => name.to_ascii_lowercase(),
    }
}

/// `parse_bool_with_len`.
fn parse_bool(v: &str) -> Option<bool> {
    let l = v.to_ascii_lowercase();
    let prefix_of = |word: &str, min: usize| l.len() >= min && word.starts_with(l.as_str());
    match l.chars().next()? {
        't' if prefix_of("true", 1) => Some(true),
        'f' if prefix_of("false", 1) => Some(false),
        'y' if prefix_of("yes", 1) => Some(true),
        'n' if prefix_of("no", 1) => Some(false),
        'o' if prefix_of("on", 2) => Some(true),
        'o' if prefix_of("off", 2) => Some(false),
        '1' if l == "1" => Some(true),
        '0' if l == "0" => Some(false),
        _ => None,
    }
}

fn parse_bool_setting(name: &str, v: &str) -> Result<bool> {
    parse_bool(v.trim()).ok_or_else(|| {
        err!(
            INVALID_PARAMETER_VALUE,
            "parameter \"{name}\" requires a Boolean value"
        )
    })
}

/// C's `strtod` on a prefix of `s`: the number and the rest, or None if none.
fn strtod(s: &str) -> Option<(f64, &str)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let rest = &s[i..];
    for word in ["infinity", "inf", "nan"] {
        if rest.len() >= word.len() && rest[..word.len()].eq_ignore_ascii_case(word) {
            let v: f64 = match word {
                "nan" => f64::NAN,
                _ => f64::INFINITY,
            };
            let v = if b[start] == b'-' { -v } else { v };
            return Some((v, &rest[word.len()..]));
        }
    }
    let digits_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == digits_start || (i == digits_start + 1 && b[digits_start] == b'.') {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            i = j;
        }
    }
    s[start..i].parse::<f64>().ok().map(|v| (v, &s[i..]))
}

/// C's `strtol(s, &end, 0)`: decimal, `0x` hex or `0` octal. Err(()) when it overflows.
fn strtol(s: &str) -> Option<(std::result::Result<i64, ()>, &str)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let neg = i < b.len() && b[i] == b'-';
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let (radix, mut j) = if b.len() > i + 2
        && b[i] == b'0'
        && (b[i + 1] | 0x20) == b'x'
        && b[i + 2].is_ascii_hexdigit()
    {
        (16, i + 2)
    } else if i < b.len() && b[i] == b'0' {
        (8, i)
    } else {
        (10, i)
    };
    let start = j;
    let mut v: i128 = 0;
    let mut overflow = false;
    while j < b.len() && (b[j] as char).is_digit(radix) {
        v = v * radix as i128 + (b[j] as char).to_digit(radix).unwrap() as i128;
        if v > i64::MAX as i128 + 1 {
            overflow = true;
            v = i64::MAX as i128 + 1;
        }
        j += 1;
    }
    if j == start {
        return None;
    }
    let v = if neg { -v } else { v };
    let r = if overflow || v > i64::MAX as i128 || v < i64::MIN as i128 {
        Err(())
    } else {
        Ok(v as i64)
    };
    Some((r, &s[j..]))
}

/// `convert_to_base_unit`: `value` with `unit` (and only trailing spaces after it) in `base`.
fn convert_to_base_unit(value: f64, unit: &str, base: Unit) -> Option<f64> {
    let unit = unit.trim_end_matches(|c: char| c.is_ascii_whitespace());
    if unit.contains(|c: char| c.is_ascii_whitespace()) {
        return None;
    }
    let table = base.table();
    let i = table.iter().position(|(u, _)| *u == unit)?;
    let mut v = value * table[i].1;
    // A fractional value rounds to a multiple of the next smaller unit, if there is one.
    if let Some((_, next)) = table.get(i + 1) {
        v = (v / next).round_ties_even() * next;
    }
    Some(v)
}

/// `parse_int(..., GUC_UNIT_KB, ...)` for an option given in kilobytes (VACUUM's
/// BUFFER_USAGE_LIMIT): Ok(value) or Err(hint).
pub(crate) fn parse_int_kb(v: &str) -> std::result::Result<i64, Option<&'static str>> {
    parse_int(v, Unit::Kb)
}

/// `parse_int`: Ok(value) or Err(hint).
fn parse_int(v: &str, unit: Unit) -> std::result::Result<i64, Option<&'static str>> {
    let (mut val, mut rest) = match strtol(v) {
        Some((Ok(n), rest)) => (n as f64, rest),
        Some((Err(()), _)) => match strtod(v) {
            Some((f, rest)) => (f, rest),
            None => return Err(None),
        },
        None => (0.0, v),
    };
    if rest.starts_with(['.', 'e', 'E']) || strtol(v).is_none() {
        match strtod(v) {
            Some((f, r)) => {
                val = f;
                rest = r;
            }
            None => return Err(None),
        }
    }
    if val.is_nan() {
        return Err(None);
    }
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if !rest.is_empty() {
        if unit == Unit::None {
            return Err(None);
        }
        val = convert_to_base_unit(val, rest, unit).ok_or(Some(unit.hint()))?;
    }
    let val = val.round_ties_even();
    if val > i32::MAX as f64 || val < i32::MIN as f64 {
        return Err(Some("Value exceeds integer range."));
    }
    Ok(val as i64)
}

/// `parse_real`.
fn parse_real(v: &str, unit: Unit) -> std::result::Result<f64, Option<&'static str>> {
    let Some((mut val, rest)) = strtod(v) else {
        return Err(None);
    };
    if val.is_nan() {
        return Err(None);
    }
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
    if !rest.is_empty() {
        if unit == Unit::None {
            return Err(None);
        }
        val = convert_to_base_unit(val, rest, unit).ok_or(Some(unit.hint()))?;
    }
    Ok(val)
}

/// C's `%g`.
pub fn format_g(v: f64) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0" } else { "0" }.into();
    }
    if v.is_nan() {
        return "nan".into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-inf" } else { "inf" }.into();
    }
    let sci = format!("{v:.5e}");
    let (mant, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let strip = |s: &str| {
        if s.contains('.') {
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            s.to_string()
        }
    };
    if !(-4..6).contains(&exp) {
        format!(
            "{}e{}{:02}",
            strip(mant),
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    } else {
        strip(&format!("{v:.*}", (5 - exp).max(0) as usize))
    }
}

/// `convert_int_from_base_unit` + `_ShowOption` for an integer with a unit.
fn show_int(v: i64, unit: Unit) -> String {
    if v <= 0 || unit == Unit::None {
        return v.to_string();
    }
    for (u, m) in unit.table() {
        if m <= 1.0 || v % (m as i64) == 0 {
            return format!("{}{u}", (v as f64 / m).round() as i64);
        }
    }
    v.to_string()
}

/// `convert_real_from_base_unit` + `_ShowOption` for a real.
fn show_real(v: f64, unit: Unit) -> String {
    if v <= 0.0 || unit == Unit::None {
        return format_g(v);
    }
    let mut out = (v, "");
    for (u, m) in unit.table() {
        let x = v / m;
        out = (x, u);
        if x > 0.0 && ((x.round() / x) - 1.0).abs() <= 1e-8 {
            break;
        }
    }
    format!("{}{}", format_g(out.0), out.1)
}

/// `valid_custom_variable_name`: two or more simple identifiers separated by dots.
fn valid_custom_name(name: &str) -> bool {
    let (mut saw_sep, mut start) = (false, true);
    for c in name.chars() {
        if c == '.' {
            if start {
                return false;
            }
            saw_sep = true;
            start = true;
        } else if c.is_ascii_alphabetic() || c == '_' || !c.is_ascii() {
            start = false;
        } else if !start && (c.is_ascii_digit() || c == '$') {
        } else {
            return false;
        }
    }
    !start && saw_sep
}

/// DateStyle: a format and/or an order; whichever is left out keeps `current`'s.
fn normalize_datestyle(raw: &str, current: &str) -> Result<String> {
    let mut cur = current.split(',').map(str::trim);
    let (mut style, mut order) = (
        cur.next().unwrap_or("ISO").to_string(),
        cur.next().unwrap_or("MDY").to_string(),
    );
    let (mut have_style, mut have_order) = (false, false);
    for part in raw.split(',').map(|p| p.trim().to_ascii_lowercase()) {
        let conflict = |what: &str| {
            err!(
                INVALID_PARAMETER_VALUE,
                "invalid value for parameter \"DateStyle\": \"{raw}\""
            )
            .detail(format!("Conflicting \"{what}\" specifications."))
        };
        // check_datestyle: some key words match by prefix ("PostgreSQL", "European").
        let (s, o) = match part.as_str() {
            "iso" => (Some("ISO"), None),
            "sql" => (Some("SQL"), None),
            p if p.starts_with("postgres") => (Some("Postgres"), None),
            "german" => (Some("German"), None),
            "ymd" => (None, Some("YMD")),
            p if p == "dmy" || p.starts_with("euro") => (None, Some("DMY")),
            p if p == "mdy" || p == "us" || p.starts_with("noneuro") => (None, Some("MDY")),
            "default" => (Some("ISO"), Some("MDY")),
            "" => continue,
            _ => {
                return Err(err!(
                    INVALID_PARAMETER_VALUE,
                    "invalid value for parameter \"DateStyle\": \"{raw}\""
                )
                .detail(format!("Unrecognized key word: \"{part}\".")));
            }
        };
        if let Some(s) = s {
            if have_style && style != s {
                return Err(conflict("DateStyle"));
            }
            style = s.into();
            have_style = true;
            // German implies DMY unless an order is given.
            if s == "German" && !have_order {
                order = "DMY".into();
            }
        }
        if let Some(o) = o {
            if have_order && order != o {
                return Err(conflict("DateStyle"));
            }
            order = o.into();
            have_order = true;
        }
    }
    Ok(format!("{style}, {order}"))
}

/// Validate and normalise a value for a known parameter.
/// `plpgsql_extra_checks_check_hook`: `all`, `none`, or a list of the checks.
fn plpgsql_extra_checks(name: &str, raw: &str) -> Result<()> {
    if raw.eq_ignore_ascii_case("all") || raw.eq_ignore_ascii_case("none") {
        return Ok(());
    }
    let invalid = |detail: String| {
        Error::new(
            code::INVALID_PARAMETER_VALUE,
            format!("invalid value for parameter \"{name}\": \"{raw}\""),
        )
        .detail(detail)
    };
    let Some(list) = split_identifiers(raw) else {
        return Err(invalid("List syntax is invalid.".into()));
    };
    for tok in list {
        let t = tok.to_ascii_lowercase();
        match t.as_str() {
            "shadowed_variables" | "too_many_rows" | "strict_multi_assignment" => {}
            "all" | "none" => {
                return Err(invalid(format!(
                    "Key word \"{tok}\" cannot be combined with other key words."
                )));
            }
            _ => return Err(invalid(format!("Unrecognized key word: \"{tok}\"."))),
        }
    }
    Ok(())
}

/// `check_restrict_nonsystem_relation_kind`: which kinds of non-system relations a value
/// restricts, (views, foreign tables); the DETAIL of an invalid one.
pub fn restrict_kinds(raw: &str) -> std::result::Result<(bool, bool), String> {
    if raw.trim().is_empty() {
        return Ok((false, false));
    }
    let Some(list) = split_identifiers(raw) else {
        return Err("List syntax is invalid.".into());
    };
    let mut kinds = (false, false);
    for tok in list {
        if tok.eq_ignore_ascii_case("view") {
            kinds.0 = true;
        } else if tok.eq_ignore_ascii_case("foreign-table") {
            kinds.1 = true;
        } else {
            return Err(format!("Unrecognized key word: \"{tok}\"."));
        }
    }
    Ok(kinds)
}

/// The kinds of non-system relations `restrict_nonsystem_relation_kind` keeps queries from
/// reading: (views, foreign tables).
pub fn restricted_kinds(st: &Settings) -> (bool, bool) {
    st.get_str("restrict_nonsystem_relation_kind")
        .and_then(|v| restrict_kinds(v).ok())
        .unwrap_or_default()
}

/// `SplitIdentifierString` on commas: the identifiers (downcased unless quoted), or `None`
/// when the list is malformed.
fn split_identifiers(raw: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut it = raw.chars().peekable();
    loop {
        while it.peek().is_some_and(|c| c.is_whitespace()) {
            it.next();
        }
        let mut cur = String::new();
        if it.peek() == Some(&'"') {
            it.next();
            loop {
                match it.next()? {
                    '"' if it.peek() == Some(&'"') => {
                        it.next();
                        cur.push('"');
                    }
                    '"' => break,
                    c => cur.push(c),
                }
            }
        } else {
            while let Some(&c) = it.peek() {
                if c == ',' || c.is_whitespace() {
                    break;
                }
                cur.extend(c.to_lowercase());
                it.next();
            }
            if cur.is_empty() {
                return None;
            }
        }
        while it.peek().is_some_and(|c| c.is_whitespace()) {
            it.next();
        }
        out.push(cur);
        match it.next() {
            None => return Some(out),
            Some(',') => {}
            Some(_) => return None,
        }
    }
}


fn invalid_value(name: &str, raw: &str) -> Error {
    err!(
        INVALID_PARAMETER_VALUE,
        "invalid value for parameter \"{name}\": \"{raw}\""
    )
}

/// The unit as range errors print it: (" ", "ms") or ("", "").
fn unit_words(unit: Unit) -> (&'static str, &'static str) {
    match unit.name() {
        Some(u) => (" ", u),
        None => ("", ""),
    }
}

/// Validate and normalise a value for a known parameter (`parse_and_validate_value` and the
/// check hooks the engine has).
fn normalize(def: &Def, raw: &str) -> Result<String> {
    let name = def.name;
    Ok(match def.kind {
        Kind::Str => match name {
            "TimeZone" | "log_timezone" => {
                // check_timezone (its errors carry DETAILs for SET TimeZone).
                crate::types::datetime::check_timezone(raw, IntervalStyle::Postgres)
                    .map_err(|e| {
                        if name == "TimeZone" {
                            e
                        } else {
                            invalid_value(name, raw)
                        }
                    })?
                    .name()
            }
            "DateStyle" => normalize_datestyle(raw, def.default)?,
            // check_client_encoding: any encoding (by any of its names) shown by its canonical
            // name; the session converts to and from it (server.rs).
            "client_encoding" => {
                use crate::types::{encnames, mbconv};
                let Some(id) = encnames::encoding_id(raw.trim()) else {
                    return Err(invalid_value(name, raw));
                };
                if !mbconv::has_conversion(id, mbconv::UTF8) {
                    return Err(invalid_value(name, raw));
                }
                encnames::encoding_name(id as i64)
                    .ok_or_else(|| invalid_value(name, raw))?
                    .to_string()
            }
            "plpgsql.extra_errors" | "plpgsql.extra_warnings" => {
                plpgsql_extra_checks(name, raw)?;
                raw.to_string()
            }
            "restrict_nonsystem_relation_kind" => {
                restrict_kinds(raw).map_err(|d| invalid_value(name, raw).detail(d))?;
                raw.to_string()
            }
            _ => raw.to_string(),
        },
        Kind::Bool => {
            let b = parse_bool_setting(name, raw)?;
            if name == "default_with_oids" && b {
                bail!(
                    FEATURE_NOT_SUPPORTED,
                    "tables declared WITH OIDS are not supported"
                );
            }
            if b { "on" } else { "off" }.into()
        }
        Kind::Int(unit) => {
            let n = parse_int(raw, unit).map_err(|hint| {
                let e = invalid_value(name, raw);
                match hint {
                    Some(h) => e.hint(h),
                    None => e,
                }
            })?;
            if (n as f64) < def.min || (n as f64) > def.max {
                let (sp, u) = unit_words(unit);
                bail!(
                    INVALID_PARAMETER_VALUE,
                    "{n}{sp}{u} is outside the valid range for parameter \"{name}\" ({}{sp}{u} .. {}{sp}{u})",
                    def.min as i64,
                    def.max as i64
                );
            }
            n.to_string()
        }
        Kind::Real(unit) => {
            let f = parse_real(raw, unit).map_err(|hint| {
                let e = invalid_value(name, raw);
                match hint {
                    Some(h) => e.hint(h),
                    None => e,
                }
            })?;
            if f < def.min || f > def.max {
                let (sp, u) = unit_words(unit);
                bail!(
                    INVALID_PARAMETER_VALUE,
                    "{}{sp}{u} is outside the valid range for parameter \"{name}\" ({}{sp}{u} .. {}{sp}{u})",
                    format_g(f),
                    format_g(def.min),
                    format_g(def.max)
                );
            }
            f.to_string()
        }
        Kind::Enum(options) => {
            let l = raw.trim().to_ascii_lowercase();
            if let Some(o) = options.iter().find(|o| o.eq_ignore_ascii_case(&l)) {
                return Ok(o.to_string());
            }
            // Hidden spellings: booleans for on/off style enums, "debug" for debug2.
            if options.contains(&"on")
                && options.contains(&"off")
                && let Some(b) = parse_bool(&l).filter(|_| {
                    matches!(l.as_str(), "true" | "false" | "yes" | "no" | "1" | "0")
                })
            {
                return Ok(if b { "on" } else { "off" }.into());
            }
            if l == "debug" && options.contains(&"debug2") {
                return Ok("debug2".into());
            }
            return Err(invalid_value(name, raw)
                .hint(format!("Available values: {}.", options.join(", "))));
        }
    })
}

/// Whether the context of `d` lets it change now (`set_config_with_handle`'s context
/// checks). `startup`: from the connection's startup packet.
fn check_context(d: &Def, superuser: bool, granted: bool, startup: bool) -> Result<()> {
    let cant = |msg: String| Error::new(code::CANT_CHANGE_RUNTIME_PARAM, msg);
    match d.context {
        Context::Internal => Err(cant(format!(
            "parameter \"{}\" cannot be changed",
            d.name
        ))),
        Context::Postmaster => Err(cant(format!(
            "parameter \"{}\" cannot be changed without restarting the server",
            d.name
        ))),
        Context::Sighup => Err(cant(format!(
            "parameter \"{}\" cannot be changed now",
            d.name
        ))),
        Context::SuBackend if startup && !superuser && !granted => Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("permission denied to set parameter \"{}\"", d.name),
        )),
        Context::SuBackend | Context::Backend if !startup => Err(cant(format!(
            "parameter \"{}\" cannot be set after connection start",
            d.name
        ))),
        Context::Superuser if !superuser && !granted => Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            format!("permission denied to set parameter \"{}\"", d.name),
        )),
        _ => Ok(()),
    }
}

/// A value `SET name = value` would accept, normalised (ALTER ROLE / DATABASE … SET).
pub fn check_setting(name: &str, value: &str) -> Result<String> {
    match find(name) {
        Some(d) => {
            check_context(d, true, true, false)?;
            if d.name == "DateStyle" {
                return normalize_datestyle(value, d.default);
            }
            normalize(d, value)
        }
        None if name.contains('.') => {
            check_custom_name(name, &[])?;
            Ok(value.to_string())
        }
        None => bail!(
            UNDEFINED_OBJECT,
            "unrecognized configuration parameter \"{name}\""
        ),
    }
}

/// A custom (placeholder) parameter's name must be valid and outside reserved prefixes.
fn check_custom_name(name: &str, reserved: &[&str]) -> Result<()> {
    if !valid_custom_name(name) {
        return Err(Error::new(
            code::INVALID_NAME,
            format!("invalid configuration parameter name \"{name}\""),
        )
        .detail("Custom parameter names must be two or more simple identifiers separated by dots."));
    }
    let prefix = name.split('.').next().unwrap_or("");
    if reserved.iter().any(|r| r.eq_ignore_ascii_case(prefix)) {
        return Err(Error::new(
            code::INVALID_NAME,
            format!("invalid configuration parameter name \"{name}\""),
        )
        .detail(format!("\"{prefix}\" is a reserved prefix.")));
    }
    Ok(())
}

/// pg_settings_get_flags(name): the parameter's flags, or None when there is no such one.
pub fn flags(name: &str) -> Option<&'static [&'static str]> {
    find(name).map(|d| d.flags)
}

/// One row of pg_settings.
pub struct SettingRow {
    pub name: String,
    /// The value in its base unit (pg_settings.setting).
    pub setting: String,
    /// SHOW's form of the value.
    pub shown: String,
    pub unit: Option<&'static str>,
    pub category: &'static str,
    pub short_desc: &'static str,
    pub extra_desc: Option<&'static str>,
    pub context: &'static str,
    pub vartype: &'static str,
    pub source: String,
    pub min_val: Option<&'static str>,
    pub max_val: Option<&'static str>,
    pub enumvals: Option<&'static [&'static str]>,
    pub boot_val: String,
    pub reset_val: String,
}

impl Settings {
    pub fn new() -> Settings {
        Settings::default()
    }

    /// The value of parameter `name` changed (or may have): drop what was derived from the
    /// settings unless it can't depend on it.
    fn touched(&mut self, name: &str) {
        if !NOT_DERIVED.contains(&name) {
            self.derived = DerivedCache::default();
        }
    }

    /// Changes whenever a value that anything derived reads changes (see `touched`): equal
    /// generations mean equal settings.
    pub fn generation(&self) -> u64 {
        self.derived().generation
    }

    /// The values every setting has now (but those `touched` leaves out), which caches
    /// shared by sessions are keyed by: equal keys mean equal settings, as equal generations
    /// do within a session.
    pub fn values_key(&self) -> Arc<ValuesKey> {
        self.derived().values_key.clone()
    }

    fn derived(&self) -> &Derived {
        self.derived.0.get_or_init(|| {
            static GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            let generation = GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut fmt = self.build_format_ctx();
            fmt.settings = generation;
            let mut values: Vec<(String, String)> = self
                .values
                .iter()
                .chain(self.defaults.iter().filter(|(k, _)| !self.values.contains_key(*k)))
                .filter(|(k, _)| !NOT_DERIVED.contains(&k.as_str()))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            values.sort();
            Box::new(Derived {
                generation,
                values_key: Arc::new(ValuesKey(values)),
                fmt,
                lex: crate::sql::scs::LexOpts::from_settings(
                    self.bool("standard_conforming_strings"),
                    self.bool("escape_string_warning"),
                    self.get_str("backslash_quote").unwrap_or_default(),
                    self.get_str("client_encoding").unwrap_or_default(),
                ),
                statement_timeout: self.int("statement_timeout"),
                max_stack_depth: self.int("max_stack_depth"),
                search_path: search_path_items(self.get_str("search_path").unwrap_or_default()),
                role: self.get_str("role").unwrap_or_default().to_string(),
                session_authorization: self
                    .get_str("session_authorization")
                    .unwrap_or_default()
                    .to_string(),
                path_memo: PathMemo::default(),
                default_txn: (
                    self.get_str("default_transaction_isolation")
                        .and_then(crate::txn::Isolation::parse)
                        .unwrap_or(crate::txn::Isolation::ReadCommitted),
                    self.bool("default_transaction_read_only"),
                ),
                lock_timeout: self.int("lock_timeout"),
            })
        })
    }

    /// A login-time default (ALTER ROLE / DATABASE ... SET): validated like SET; RESET returns to it.
    pub fn set_default(&mut self, name: &str, value: &str) -> Result<()> {
        self.set_full(name, Some(value), false, false, true, true)?;
        let canon = canonical_name(name);
        if let Some(v) = self.values.remove(&canon) {
            self.defaults.insert(canon.clone(), v);
        }
        self.touched(&canon);
        Ok(())
    }

    /// A setting from the connection's startup packet (`options`, or a plain parameter).
    pub fn set_startup(&mut self, name: &str, value: &str) -> Result<()> {
        self.set_full(name, Some(value), false, false, false, true)
    }

    /// Validate `name = value` as SET would, without applying it.
    pub fn check(&self, name: &str, value: &str) -> Result<()> {
        self.clone().set(name, Some(value), false, false)
    }

    /// The value SET gave a parameter in this session, if any (for `restore_raw`).
    pub fn session_value(&self, name: &str) -> Option<String> {
        self.values.get(&canonical_name(name)).cloned()
    }

    /// Put back a value saved with `session_value` (a function's SET clauses on exit).
    pub fn restore_raw(&mut self, name: &str, prev: Option<String>) {
        let canon = canonical_name(name);
        self.touched(&canon);
        match prev {
            Some(v) => {
                self.values.insert(canon, v);
            }
            None => {
                self.values.remove(&canon);
            }
        }
    }

    /// Set a value the server owns (session_authorization, is_superuser), bypassing checks.
    pub fn set_internal(&mut self, name: &str, value: &str) {
        let canon = canonical_name(name);
        self.touched(&canon);
        self.values.insert(canon, value.to_string());
    }

    /// A module reserves its prefix (`MarkGUCPrefixReserved`): placeholders under it go, with
    /// a WARNING each.
    pub fn reserve_prefix(&mut self, prefix: &'static str) -> Vec<Error> {
        if self.reserved.contains(&prefix) {
            return Vec::new();
        }
        self.reserved.push(prefix);
        let mut gone: Vec<String> = self
            .values
            .keys()
            .chain(self.defaults.keys())
            .filter(|n| {
                self.def(n).is_none()
                    && n.split('.').next().is_some_and(|p| p.eq_ignore_ascii_case(prefix))
            })
            .cloned()
            .collect();
        gone.sort();
        gone.dedup();
        // The library's own parameters take the placeholders' values (define_custom_variable:
        // an invalid one is a WARNING, and the parameter keeps its default).
        let mut warnings = Vec::new();
        let mut defined: Vec<(String, &'static Def)> = self
            .values
            .keys()
            .chain(self.defaults.keys())
            .filter_map(|n| Some((n.clone(), self.def(n).filter(|d| d.module.is_some_and(|m| m.eq_ignore_ascii_case(prefix)))?)))
            .collect();
        defined.sort_by(|a, b| a.0.cmp(&b.0));
        defined.dedup_by(|a, b| a.0 == b.0);
        for (n, d) in defined {
            for map in [&mut self.values, &mut self.defaults] {
                let Some(v) = map.remove(&n) else { continue };
                match normalize(d, &v) {
                    Ok(v) => {
                        map.insert(d.name.to_string(), v);
                    }
                    Err(mut e) => {
                        e.severity = crate::error::Severity::Warning;
                        warnings.push(e);
                    }
                }
            }
        }
        if !warnings.is_empty() {
            self.derived = DerivedCache::default();
        }
        if !gone.is_empty() {
            self.derived = DerivedCache::default();
        }
        gone.into_iter()
            .map(|n| {
                self.values.remove(&n);
                self.defaults.remove(&n);
                self.stack.remove(&n);
                let mut e = Error::new(
                    code::INVALID_NAME,
                    format!("invalid configuration parameter name \"{n}\", removing it"),
                )
                .detail(format!("\"{prefix}\" is now a reserved prefix."));
                e.severity = crate::error::Severity::Warning;
                e
            })
            .chain(warnings)
            .collect()
    }

    /// The parameter `name` as this session has it: a built-in one, or one of an extension
    /// library it has loaded.
    fn def(&self, name: &str) -> Option<&'static Def> {
        find_any(name).filter(|d| d.module.is_none_or(|m| self.reserved.iter().any(|r| r.eq_ignore_ascii_case(m))))
    }

    /// An extension library is loaded (its `_PG_init`): its parameters are defined and its
    /// prefix reserved. Returns the WARNINGs that raises.
    pub fn load_module(&mut self, lib: &str) -> Vec<Error> {
        match find_module_prefix(lib) {
            Some(prefix) => self.reserve_prefix(prefix),
            None => Vec::new(),
        }
    }

    /// Current raw (normalised, unit-less) value.
    pub fn get_raw(&self, name: &str) -> Option<String> {
        self.get_str(name).map(str::to_string)
    }

    /// `get_raw`, borrowed (no allocation for a built-in parameter or a lower-case name).
    pub fn get_str(&self, name: &str) -> Option<&str> {
        let get = |canon: &str| {
            self.values
                .get(canon)
                .or_else(|| self.defaults.get(canon))
                .map(String::as_str)
        };
        match self.def(name) {
            Some(d) => Some(get(d.name).unwrap_or(d.default)),
            None if name.bytes().any(|c| c.is_ascii_uppercase()) => {
                get(&name.to_ascii_lowercase())
            }
            None => get(name),
        }
    }

    /// SHOW's form of a known parameter's raw value.
    fn shown(d: &Def, raw: &str) -> String {
        match d.kind {
            Kind::Int(u) => show_int(raw.parse().unwrap_or(0), u),
            Kind::Real(u) => show_real(raw.parse().unwrap_or(0.0), u),
            _ => raw.to_string(),
        }
    }

    /// `SHOW name` / current_setting(name).
    pub fn show(&self, name: &str) -> Result<String> {
        match self.def(name) {
            Some(d) if d.name == "seed" => Ok("unavailable".into()),
            Some(d) => Ok(Self::shown(d, self.get_str(d.name).unwrap_or_default())),
            None => match self
                .values
                .get(&name.to_ascii_lowercase())
                .or_else(|| self.defaults.get(&name.to_ascii_lowercase()))
            {
                Some(v) => Ok(v.clone()),
                None => Err(err!(
                    UNDEFINED_OBJECT,
                    "unrecognized configuration parameter \"{name}\""
                )),
            },
        }
    }

    pub fn exists(&self, name: &str) -> bool {
        self.def(name).is_some()
            || self.values.contains_key(&name.to_ascii_lowercase())
            || self.defaults.contains_key(&name.to_ascii_lowercase())
    }

    /// SET name = value. `local` reverts at transaction end; `in_txn` reverts on rollback.
    pub fn set(
        &mut self,
        name: &str,
        value: Option<&str>,
        local: bool,
        in_txn: bool,
    ) -> Result<()> {
        self.set_granted(name, value, local, in_txn, false)
    }

    /// SET, where `granted` says the role holds SET on the parameter (pg_parameter_acl), so a
    /// superuser-only one may be set too.
    pub fn set_granted(
        &mut self,
        name: &str,
        value: Option<&str>,
        local: bool,
        in_txn: bool,
        granted: bool,
    ) -> Result<()> {
        self.set_full(name, value, local, in_txn, granted, false)
    }

    fn set_full(
        &mut self,
        name: &str,
        value: Option<&str>,
        local: bool,
        in_txn: bool,
        granted: bool,
        startup: bool,
    ) -> Result<()> {
        let def = self.def(name);
        let canon = match def {
            Some(d) => d.name.to_string(),
            None => name.to_ascii_lowercase(),
        };
        let new = match def {
            Some(d) => {
                let superuser = self.get_str("is_superuser") == Some("on");
                check_context(d, superuser, granted, startup)?;
                if value.is_none() && d.flag("NO_RESET") {
                    bail!(
                        FEATURE_NOT_SUPPORTED,
                        "parameter \"{}\" cannot be reset",
                        d.name
                    );
                }
                match value {
                    Some(v) if d.name == "DateStyle" => Some(normalize_datestyle(
                        v,
                        self.get_str("DateStyle").unwrap_or(d.default),
                    )?),
                    Some(v) => Some(normalize(d, v)?),
                    None => None,
                }
            }
            None => {
                if !name.contains('.') {
                    bail!(
                        UNDEFINED_OBJECT,
                        "unrecognized configuration parameter \"{name}\""
                    );
                }
                check_custom_name(name, &self.reserved)?;
                Some(value.unwrap_or("").to_string())
            }
        };
        if in_txn {
            self.push_old(
                &canon,
                if local {
                    StackState::Local
                } else {
                    StackState::Set
                },
            );
        }
        self.touched(&canon);
        match new {
            Some(v) => {
                self.values.insert(canon.clone(), v);
            }
            None => {
                // RESET / SET TO DEFAULT
                if def.is_some() || self.defaults.contains_key(&canon) {
                    self.values.remove(&canon);
                } else {
                    self.values.insert(canon.clone(), String::new());
                }
            }
        }
        if REPORTED.contains(&canon.as_str()) {
            self.changed.push(canon);
        }
        Ok(())
    }

    /// A transaction starts as `isolation` (`Isolation::name`) and `read_only`: SET LOCAL
    /// transaction_isolation and transaction_read_only. Unless they have stack entries
    /// already, without making them yet (each transaction does this; few need them).
    pub fn start_transaction(&mut self, isolation: &'static str, read_only: bool) {
        let values = [isolation, if read_only { "on" } else { "off" }];
        if self.txn_start.is_some() || TXN_START.iter().any(|n| self.stack.contains_key(*n)) {
            for (name, v) in TXN_START.iter().zip(values) {
                let _ = self.set(name, Some(v), true, true);
            }
            return;
        }
        // (What `set` does with these: user-context parameters, values in canonical form,
        // neither reported nor read by `Derived`.)
        let mut priors = [None, None];
        for ((name, v), prior) in TXN_START.iter().zip(values).zip(&mut priors) {
            *prior = match self.values.get_mut(*name) {
                Some(slot) => Some(std::mem::replace(slot, v.to_string())),
                None => {
                    self.values.insert(name.to_string(), v.to_string());
                    None
                }
            };
        }
        self.txn_start = Some((self.level, priors));
    }

    /// Make the stack entries `start_transaction` left out (before the stack is used).
    fn stack_txn_start(&mut self) {
        let Some((level, priors)) = self.txn_start.take() else {
            return;
        };
        for (name, prior) in TXN_START.iter().zip(priors) {
            self.stack.insert(
                name.to_string(),
                vec![Stacked {
                    level,
                    state: StackState::Local,
                    prior,
                    masked: None,
                }],
            );
        }
    }

    /// `push_old_value`: remember the value before the first change at this nest level.
    fn push_old(&mut self, name: &str, action: StackState) {
        self.stack_txn_start();
        let level = self.level;
        if let Some(top) = self.stack.get_mut(name).and_then(|s| s.last_mut())
            && top.level >= level
        {
            match action {
                // SET overrides any prior action at the same level.
                StackState::Set => {
                    top.state = StackState::Set;
                    top.masked = None;
                }
                // SET followed by SET LOCAL: remember SET's value.
                StackState::Local if top.state == StackState::Set => {
                    top.masked = self.values.get(name).cloned();
                    top.state = StackState::SetLocal;
                }
                _ => {}
            }
            return;
        }
        let entry = Stacked {
            level,
            state: action,
            prior: self.values.get(name).cloned(),
            masked: None,
        };
        match self.stack.get_mut(name) {
            Some(stack) => stack.push(entry),
            None => {
                self.stack.insert(name.to_string(), vec![entry]);
            }
        }
    }

    /// `AtEOXact_GUC`: the (sub)transactions at `nest_level` and deeper end.
    fn at_eoxact(&mut self, commit: bool, nest_level: usize) {
        self.stack_txn_start();
        if self.stack.is_empty() {
            return;
        }
        for (name, mut stack) in std::mem::take(&mut self.stack) {
            // The value to go back to: that of the outermost entry ended (popped last).
            let mut restore: Option<Option<String>> = None;
            while let Some(top) = stack.last()
                && top.level >= nest_level
            {
                let n = stack.len();
                let prev_level = (n >= 2).then(|| stack[n - 2].level);
                let (state, level) = (top.state, top.level);
                if !commit || state == StackState::Save {
                    restore = Some(stack.pop().unwrap().prior);
                } else if level == 1 {
                    let top = stack.pop().unwrap();
                    match state {
                        StackState::SetLocal => restore = Some(top.masked),
                        StackState::Set => {}
                        StackState::Local | StackState::Save => restore = Some(top.prior),
                    }
                } else if prev_level.is_none_or(|p| p < level - 1) {
                    // Nothing saved at the parent level: this entry becomes the parent's.
                    stack.last_mut().unwrap().level -= 1;
                } else {
                    // Merge into the parent level's entry.
                    let top = stack.pop().unwrap();
                    let prev = stack.last_mut().unwrap();
                    match top.state {
                        StackState::Save => {}
                        StackState::Set => {
                            prev.state = StackState::Set;
                            prev.masked = None;
                        }
                        StackState::Local => {
                            if prev.state == StackState::Set {
                                prev.masked = top.prior;
                                prev.state = StackState::SetLocal;
                            }
                        }
                        StackState::SetLocal => {
                            prev.masked = top.masked;
                            prev.state = StackState::SetLocal;
                        }
                    }
                }
            }
            if !stack.is_empty() {
                self.stack.insert(name.clone(), stack);
            }
            let Some(v) = restore else {
                continue;
            };
            self.touched(&name);
            let reported = REPORTED.contains(&name.as_str());
            let before = if reported {
                self.values.get(&name).cloned()
            } else {
                None
            };
            match v {
                Some(v) => match self.values.get_mut(&name) {
                    Some(slot) => *slot = v,
                    None => {
                        self.values.insert(name.clone(), v);
                    }
                },
                // A placeholder (custom) parameter stays once made, at its empty reset
                // value.
                None if self.def(&name).is_none() && !self.defaults.contains_key(&name) => {
                    self.values.insert(name.clone(), String::new());
                }
                None => {
                    self.values.remove(&name);
                }
            }
            if reported && before.as_ref() != self.values.get(&name) {
                self.changed.push(name);
            }
        }
    }

    /// `NewGUCNestLevel`: a function with SET clauses starts; returns its level.
    pub fn push_nest(&mut self) -> usize {
        self.level += 1;
        self.level
    }

    /// A function's SET clause (GUC_ACTION_SAVE at the level `push_nest` gave).
    pub fn set_saved(&mut self, name: &str, value: &str) -> Result<()> {
        let canon = canonical_name(name);
        let mut probe = self.clone();
        probe.stack.clear();
        probe.set(name, Some(value), false, false)?;
        let v = probe.values.remove(&canon);
        self.push_old(&canon, StackState::Save);
        self.touched(&canon);
        match v {
            Some(v) => {
                self.values.insert(canon.clone(), v);
            }
            None => {
                self.values.remove(&canon);
            }
        }
        if REPORTED.contains(&canon.as_str()) {
            self.changed.push(canon);
        }
        Ok(())
    }

    /// RestrictSearchPath: a security-restricted operation (index build, CLUSTER, REFRESH, …)
    /// sees `search_path` as `pg_catalog, pg_temp`, at a nest level of its own that
    /// `pop_restricted` ends.
    pub fn restrict_search_path(&mut self) -> usize {
        let level = self.push_nest();
        self.push_old("search_path", StackState::Save);
        self.touched("search_path");
        self.values
            .insert("search_path".into(), "pg_catalog, pg_temp".into());
        level
    }

    /// The security-restricted operation that `restrict_search_path` began at `level`
    /// ends: every change made at that level goes, what a function it ran SET included
    /// (`AtEOXact_GUC(false, level)`).
    pub fn pop_restricted(&mut self, level: usize) {
        self.subxact_leave(level, false);
    }

    /// The function that pushed `level` ends: its SET clauses' values go; what it SET
    /// itself joins the enclosing level (`AtEOXact_GUC(true, level)`).
    pub fn pop_nest(&mut self, level: usize) {
        self.at_eoxact(true, level);
        self.level = level - 1;
    }

    /// An exception block's subtransaction begins inside a routine; returns its level.
    pub fn subxact_enter(&mut self) -> usize {
        self.push_nest()
    }

    /// That subtransaction ends (committed, or rolled back).
    pub fn subxact_leave(&mut self, level: usize, commit: bool) {
        self.at_eoxact(commit, level);
        self.level = level - 1;
    }

    /// A savepoint (subtransaction) began; `depth`: open savepoints now.
    pub fn subxact_begin(&mut self, depth: usize) {
        self.level = depth + 1;
    }

    /// Savepoints released (their changes join the enclosing level); `depth`: those left.
    pub fn subxact_commit(&mut self, depth: usize) {
        self.at_eoxact(true, depth + 2);
        self.level = depth + 1;
    }

    /// Rolled back to a savepoint (or out of one); `depth`: open savepoints now.
    pub fn subxact_abort(&mut self, depth: usize) {
        self.at_eoxact(false, depth + 1);
        self.level = depth + 1;
    }

    /// RESET ALL: every parameter that RESET ALL covers goes back to its default.
    pub fn reset_all(&mut self) {
        self.reset_all_in(false);
    }

    pub fn reset_all_in(&mut self, in_txn: bool) {
        let mut names: Vec<String> = self
            .values
            .keys()
            .filter(|n| self.def(n).is_none_or(|d| !d.flag("NO_RESET_ALL")))
            .cloned()
            .collect();
        names.sort();
        if !names.is_empty() {
            self.derived = DerivedCache::default();
        }
        for n in names {
            if in_txn {
                self.push_old(&n, StackState::Set);
            }
            if REPORTED.contains(&n.as_str()) {
                self.changed.push(n.clone());
            }
            if self.def(&n).is_some() || self.defaults.contains_key(&n) {
                self.values.remove(&n);
            } else {
                // A placeholder stays, empty.
                self.values.insert(n, String::new());
            }
        }
    }

    /// Transaction end: undo SET LOCAL, and on abort undo every SET made in the transaction.
    pub fn end_transaction(&mut self, committed: bool) {
        if self.stack.is_empty()
            && let Some((_, priors)) = self.txn_start.take()
        {
            // Just `start_transaction`'s SET LOCALs: what `at_eoxact` does with their entries.
            for (name, prior) in TXN_START.iter().zip(priors) {
                match prior {
                    Some(v) => {
                        if let Some(slot) = self.values.get_mut(*name) {
                            *slot = v;
                        } else {
                            self.values.insert(name.to_string(), v);
                        }
                    }
                    None => {
                        self.values.remove(*name);
                    }
                }
            }
        } else {
            self.at_eoxact(committed, 1);
        }
        self.level = 1;
    }

    pub fn bool(&self, name: &str) -> bool {
        self.get_str(name).is_some_and(|v| v == "on")
    }

    pub fn int(&self, name: &str) -> i64 {
        self.get_str(name).and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    /// `int("statement_timeout")`.
    pub fn statement_timeout(&self) -> i64 {
        self.derived().statement_timeout
    }

    /// `int("max_stack_depth")`.
    pub fn max_stack_depth(&self) -> i64 {
        self.derived().max_stack_depth
    }

    /// A new transaction's isolation level and read-only status, by default.
    pub fn default_txn(&self) -> (crate::txn::Isolation, bool) {
        self.derived().default_txn
    }

    /// `int("lock_timeout")`.
    pub fn lock_timeout(&self) -> i64 {
        self.derived().lock_timeout
    }

    /// How the scanner reads string literals under these settings.
    pub fn lex_opts(&self) -> crate::sql::scs::LexOpts {
        self.derived().lex
    }

    pub fn search_path(&self, user: &str) -> Vec<String> {
        self.derived()
            .search_path
            .iter()
            .map(|s| s.as_deref().unwrap_or(user).to_string())
            .collect()
    }

    /// The search path of backend `pid`'s session: `search_path` with `$user` as `user`, and
    /// `pg_temp` as the session's temporary schema (`temp`; first when not listed).
    pub fn session_search_path(
        &self,
        user: &str,
        pid: i32,
        temp: impl FnOnce() -> String,
    ) -> Vec<String> {
        let d = self.derived();
        let mut memo = d.path_memo.0.lock();
        if let Some((u, p, path)) = &*memo
            && u == user
            && *p == pid
        {
            return path.clone();
        }
        let mut path = self.search_path(user);
        let temp = temp();
        match path.iter().position(|s| s == "pg_temp") {
            Some(i) => path[i] = temp,
            None => path.insert(0, temp),
        }
        *memo = Some((user.to_string(), pid, path.clone()));
        path
    }

    /// `get_str("role")`, or "".
    pub fn role(&self) -> &str {
        &self.derived().role
    }

    /// `get_str("session_authorization")`, or "".
    pub fn session_authorization(&self) -> &str {
        &self.derived().session_authorization
    }

    pub fn timezone(&self) -> TimeZone {
        self.derived().fmt.tz.clone()
    }

    /// How values are rendered as text under these settings (no clock or names).
    pub fn format_ctx(&self) -> FormatCtx {
        self.derived().fmt.clone()
    }

    fn build_format_ctx(&self) -> FormatCtx {
        FormatCtx {
            tz: self
                .get_str("TimeZone")
                .and_then(TimeZone::parse)
                .unwrap_or_default(),
            interval_style: match self.get_str("IntervalStyle") {
                Some("postgres_verbose") => IntervalStyle::PostgresVerbose,
                Some("sql_standard") => IntervalStyle::SqlStandard,
                Some("iso_8601") => IntervalStyle::Iso8601,
                _ => IntervalStyle::Postgres,
            },
            date_style: crate::types::datetime::DateStyle::parse(
                self.get_str("DateStyle").unwrap_or_default(),
            ),
            extra_float_digits: self.int("extra_float_digits") as i32,
            bytea_hex: self.get_str("bytea_output") != Some("escape"),
            xml_document: self.get_str("xmloption") == Some("document"),
            xml_binary_hex: self.get_str("xmlbinary") == Some("hex"),
            tz_abbrevs: match self.get_str("timezone_abbreviations") {
                Some("Australia") => "Australia",
                Some("India") => "India",
                _ => "Default",
            },
            client_encoding: self
                .get_str("client_encoding")
                .and_then(crate::types::encnames::encoding_id)
                .unwrap_or(crate::types::mbconv::UTF8),
            ..FormatCtx::default()
        }
    }

    /// Rows for pg_settings / SHOW ALL (those not hidden from them), by name.
    pub fn all(&self) -> Vec<SettingRow> {
        table()
            .defs
            .iter()
            .filter(|d| !d.flag("NO_SHOW_ALL") && self.def(d.name).is_some())
            .map(|d| {
                let raw = self.get_raw(d.name).unwrap_or_default();
                let (unit, vartype) = match d.kind {
                    Kind::Bool => (None, "bool"),
                    Kind::Int(u) => (u.name(), "integer"),
                    Kind::Real(u) => (u.name(), "real"),
                    Kind::Enum(_) => (None, "enum"),
                    Kind::Str => (None, "string"),
                };
                let setting = match d.kind {
                    Kind::Real(_) => format_g(raw.parse().unwrap_or(0.0)),
                    _ => raw.clone(),
                };
                let ranged = matches!(d.kind, Kind::Int(_) | Kind::Real(_));
                let reset = self
                    .defaults
                    .get(d.name)
                    .cloned()
                    .unwrap_or_else(|| d.default.to_string());
                let reset = match d.kind {
                    Kind::Real(_) => format_g(reset.parse().unwrap_or(0.0)),
                    _ => reset,
                };
                let source = if self.values.contains_key(d.name) {
                    "session"
                } else if self.defaults.contains_key(d.name) {
                    "user"
                } else {
                    d.source
                };
                SettingRow {
                    name: d.name.to_string(),
                    shown: Self::shown(d, &raw),
                    setting,
                    unit,
                    category: d.category,
                    short_desc: d.desc,
                    extra_desc: (!d.extra_desc.is_empty()).then_some(d.extra_desc),
                    context: d.context.name(),
                    vartype,
                    source: source.to_string(),
                    min_val: ranged.then_some(d.min_s),
                    max_val: ranged.then_some(d.max_s),
                    enumvals: match d.kind {
                        Kind::Enum(v) => Some(v),
                        _ => None,
                    },
                    boot_val: d.boot.to_string(),
                    reset_val: reset,
                }
            })
            .collect()
    }

    pub fn show_value(&self, name: &str) -> String {
        self.show(name).unwrap_or_default()
    }
}

/// Whether a message of this severity reaches the client under `client_min_messages`
/// (INFO always does).
pub fn notice_wanted(st: &Settings, sev: crate::error::Severity) -> bool {
    use crate::error::Severity as S;
    let rank = match sev {
        S::Info => return true,
        S::Debug => 4,
        S::Log => 5,
        S::Notice => 6,
        S::Warning => 7,
        S::Error | S::Fatal => 8,
    };
    let min_rank = match st.get_str("client_min_messages") {
        Some("debug5") => 0,
        Some("debug4") => 1,
        Some("debug3") => 2,
        Some("debug2") => 3,
        Some("debug1" | "debug") => 4,
        Some("log") => 5,
        Some("warning") => 7,
        Some("error") => 8,
        _ => 6,
    };
    rank >= min_rank
}

/// Split a search_path value into schema names, expanding `$user`.
pub fn parse_search_path(v: &str, user: &str) -> Vec<String> {
    search_path_items(v)
        .into_iter()
        .map(|s| s.unwrap_or_else(|| user.to_string()))
        .collect()
}

/// A search_path value's schema names (None: `$user`).
fn search_path_items(v: &str) -> Vec<Option<String>> {
    // SplitIdentifierString: commas outside double quotes separate the names; a quoted one
    // keeps its case (and its commas), "" in it standing for ".
    let mut out = Vec::new();
    let mut chars = v.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut name = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            while let Some(c) = chars.next() {
                if c == '"' {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        name.push('"');
                    } else {
                        break;
                    }
                } else {
                    name.push(c);
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' || c.is_whitespace() {
                    break;
                }
                name.push(c.to_ascii_lowercase());
                chars.next();
            }
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek() == Some(&',') {
            chars.next();
        }
        if !name.is_empty() {
            out.push((name != "$user").then_some(name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units() {
        let mut s = Settings::new();
        s.set("statement_timeout", Some("5000"), false, false)
            .unwrap();
        assert_eq!(s.show("statement_timeout").unwrap(), "5s");
        s.set("statement_timeout", Some("1min"), false, false)
            .unwrap();
        assert_eq!(s.show("statement_timeout").unwrap(), "1min");
        s.set("work_mem", Some("64MB"), false, false).unwrap();
        assert_eq!(s.show("work_mem").unwrap(), "64MB");
        assert_eq!(s.show("work_mem").unwrap(), "64MB");
        assert!(s.set("nope", Some("1"), false, false).is_err());
        s.set("my.custom", Some("x"), false, false).unwrap();
        assert_eq!(s.show("my.custom").unwrap(), "x");
    }

    #[test]
    fn local_and_rollback() {
        let mut s = Settings::new();
        s.set("application_name", Some("a"), false, false).unwrap();
        s.set("application_name", Some("b"), true, true).unwrap();
        assert_eq!(s.show("application_name").unwrap(), "b");
        s.end_transaction(true);
        assert_eq!(s.show("application_name").unwrap(), "a");
        s.set("application_name", Some("c"), false, true).unwrap();
        s.end_transaction(false);
        assert_eq!(s.show("application_name").unwrap(), "a");
    }
}
