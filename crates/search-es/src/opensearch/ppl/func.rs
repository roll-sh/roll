//! PPL's scalar functions: one [`Def`] per name (its result type from the argument types, and
//! its evaluation), looked up once when an expression is compiled.
//!
//! Semantics follow the plugin's Calcite implementations: nulls propagate unless the function
//! is about nulls (`isnull`, `ifnull`, `coalesce`), string arguments of math functions read as
//! doubles, and the date functions take timestamps, dates or timestamp texts. The relevance
//! functions (`match`, `match_phrase`, ...) evaluate on the row as the plugin's in-memory
//! fallback does: tokens compared case-insensitively.

use super::value::{self, Ty, Val};
use crate::error::{EsError, EsResult};
use crate::json::Value;
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};

pub struct Def {
    pub name: &'static str,
    pub ret: fn(&[Ty]) -> Result<Ty, String>,
    pub eval: fn(&[Val], &[Ty]) -> EsResult<Val>,
}

impl std::fmt::Debug for Def {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name)
    }
}

macro_rules! def {
    ($name:literal, $ret:expr, $eval:expr) => {
        Def { name: $name, ret: $ret, eval: $eval }
    };
}

fn t_bool(_: &[Ty]) -> Result<Ty, String> {
    Ok(Ty::Bool)
}
fn t_str(_: &[Ty]) -> Result<Ty, String> {
    Ok(Ty::Str)
}
fn t_int(_: &[Ty]) -> Result<Ty, String> {
    Ok(Ty::Int)
}
fn t_long(_: &[Ty]) -> Result<Ty, String> {
    Ok(Ty::Long)
}
fn t_double(_: &[Ty]) -> Result<Ty, String> {
    Ok(Ty::Double)
}
fn t_ts(_: &[Ty]) -> Result<Ty, String> {
    Ok(Ty::Timestamp)
}
fn t_date(_: &[Ty]) -> Result<Ty, String> {
    Ok(Ty::Date)
}
fn t_time(_: &[Ty]) -> Result<Ty, String> {
    Ok(Ty::Time)
}
fn t_first(a: &[Ty]) -> Result<Ty, String> {
    Ok(a.first().copied().unwrap_or(Ty::Undefined))
}
/// The common type of every argument (`coalesce`, `ifnull`).
fn t_common(a: &[Ty]) -> Result<Ty, String> {
    Ok(a.iter().copied().filter(|t| *t != Ty::Undefined).reduce(|x, y| if x == y { x } else if x.is_numeric() && y.is_numeric() { Ty::widen(x, y) } else { Ty::Str }).unwrap_or(Ty::Undefined))
}
/// `if(cond, a, b)`: the branches' common type.
fn t_if(a: &[Ty]) -> Result<Ty, String> {
    t_common(a.get(1..).unwrap_or(&[]))
}
/// Math functions of a number: integers stay integral (`abs`), else double.
fn t_num_keep(a: &[Ty]) -> Result<Ty, String> {
    Ok(match a.first() {
        Some(t) if t.is_numeric() => *t,
        _ => Ty::Double,
    })
}
fn t_ceil(a: &[Ty]) -> Result<Ty, String> {
    Ok(match a.first() {
        Some(t) if t.is_integral() => *t,
        Some(Ty::Str) => Ty::Double,
        _ => Ty::Long,
    })
}
fn t_round(a: &[Ty]) -> Result<Ty, String> {
    Ok(match a.first() {
        Some(t) if t.is_integral() => Ty::Long,
        Some(Ty::Float) => Ty::Float,
        _ => Ty::Double,
    })
}
fn t_mod(a: &[Ty]) -> Result<Ty, String> {
    let (x, y) = (a.first().copied().unwrap_or(Ty::Double), a.get(1).copied().unwrap_or(Ty::Double));
    Ok(if x.is_integral() && y.is_integral() { Ty::widen(x, y) } else { Ty::Double })
}

fn null_if_any(args: &[Val]) -> bool {
    args.iter().any(Val::is_null)
}

fn f64_of(v: &Val) -> Option<f64> {
    v.as_f64()
}

fn s_of(v: &Val, t: Ty) -> Option<String> {
    v.to_text(t)
}

fn finite(f: f64) -> Val {
    if f.is_finite() { Val::Float(f) } else { Val::Null }
}

fn math1(args: &[Val], f: impl Fn(f64) -> f64) -> EsResult<Val> {
    Ok(match args.first().and_then(f64_of) {
        Some(x) => finite(f(x)),
        None => Val::Null,
    })
}

// ------------------------------------------------------------------------------ table

static DEFS: &[Def] = &[
    // Conditions and nulls.
    def!("isnull", t_bool, |a, _| Ok(Val::Bool(a.first().is_none_or(Val::is_null)))),
    def!("isnotnull", t_bool, |a, _| Ok(Val::Bool(a.first().is_some_and(|v| !v.is_null())))),
    def!("ispresent", t_bool, |a, _| Ok(Val::Bool(a.first().is_some_and(|v| !v.is_null())))),
    def!("isempty", t_bool, |a, t| Ok(Val::Bool(a.first().is_none_or(|v| v.is_null() || s_of(v, t[0]).is_some_and(|s| s.is_empty()))))),
    def!("isblank", t_bool, |a, t| Ok(Val::Bool(a.first().is_none_or(|v| v.is_null() || s_of(v, t[0]).is_some_and(|s| s.trim().is_empty()))))),
    def!("ifnull", t_common, |a, _| Ok(if a[0].is_null() { a.get(1).cloned().unwrap_or_default() } else { a[0].clone() })),
    def!("nullif", t_first, |a, _| Ok(if super::exec::compare(super::parser::Op::Eq, &a[0], &a[1]).as_bool() == Some(true) { Val::Null } else { a[0].clone() })),
    def!("coalesce", t_common, |a, _| Ok(a.iter().find(|v| !v.is_null()).cloned().unwrap_or_default())),
    def!("if", t_if, |a, _| Ok(if a[0].as_bool() == Some(true) { a[1].clone() } else { a.get(2).cloned().unwrap_or_default() })),
    def!("like", t_bool, |a, t| like(a, t, true)),
    def!("ilike", t_bool, |a, t| like(a, t, true)),
    def!("regexp", t_bool, |a, t| regexp(a, t)),
    def!("regex_match", t_bool, |a, t| regexp(a, t)),
    def!("contains", t_bool, |a, t| Ok(match (s_of(&a[0], t[0]), a.get(1).and_then(|v| s_of(v, Ty::Str))) {
        (Some(x), Some(y)) => Val::Bool(x.contains(&y)),
        _ => Val::Null,
    })),
    // Relevance (evaluated on the row).
    def!("match", t_bool, |a, t| relevance(a, t, false)),
    def!("match_phrase", t_bool, |a, t| relevance(a, t, true)),
    def!("match_bool_prefix", t_bool, |a, t| relevance(a, t, false)),
    def!("match_phrase_prefix", t_bool, |a, t| relevance(a, t, true)),
    // Math.
    def!("abs", t_num_keep, |a, _| Ok(match &a[0] {
        Val::Int(i) => Val::Int(i.wrapping_abs()),
        v => v.as_f64().map_or(Val::Null, |f| Val::Float(f.abs())),
    })),
    def!("ceil", t_ceil, |a, t| ceil_floor(a, t, f64::ceil)),
    def!("ceiling", t_ceil, |a, t| ceil_floor(a, t, f64::ceil)),
    def!("floor", t_ceil, |a, t| ceil_floor(a, t, f64::floor)),
    def!("round", t_round, |a, t| round(a, t)),
    def!("sqrt", t_double, |a, _| math1(a, |x| if x < 0.0 { f64::NAN } else { x.sqrt() })),
    def!("cbrt", t_double, |a, _| math1(a, f64::cbrt)),
    def!("exp", t_double, |a, _| math1(a, f64::exp)),
    def!("ln", t_double, |a, _| math1(a, |x| if x <= 0.0 { f64::NAN } else { x.ln() })),
    def!("log2", t_double, |a, _| math1(a, |x| if x <= 0.0 { f64::NAN } else { x.log2() })),
    def!("log10", t_double, |a, _| math1(a, |x| if x <= 0.0 { f64::NAN } else { x.log10() })),
    def!("log", t_double, |a, _| {
        if a.len() == 2 {
            return Ok(match (a[0].as_f64(), a[1].as_f64()) {
                (Some(b), Some(x)) if b > 0.0 && x > 0.0 => finite(x.ln() / b.ln()),
                _ => Val::Null,
            });
        }
        math1(a, |x| if x <= 0.0 { f64::NAN } else { x.ln() })
    }),
    def!("pow", t_double, pow),
    def!("power", t_double, pow),
    def!("sign", t_int, |a, _| Ok(a[0].as_f64().map_or(Val::Null, |x| Val::Int(if x > 0.0 { 1 } else if x < 0.0 { -1 } else { 0 })))),
    def!("mod", t_mod, modulus),
    def!("modulus", t_mod, modulus),
    def!("rand", t_double, |_, _| Ok(Val::Float(rand::random::<f64>()))),
    def!("pi", t_double, |_, _| Ok(Val::Float(std::f64::consts::PI))),
    def!("e", t_double, |_, _| Ok(Val::Float(std::f64::consts::E))),
    def!("sin", t_double, |a, _| math1(a, f64::sin)),
    def!("cos", t_double, |a, _| math1(a, f64::cos)),
    def!("tan", t_double, |a, _| math1(a, f64::tan)),
    // Strings.
    def!("concat", t_str, |a, t| {
        if null_if_any(a) {
            return Ok(Val::Null);
        }
        Ok(Val::Str(a.iter().zip(t).filter_map(|(v, t)| s_of(v, *t)).collect()))
    }),
    def!("concat_ws", t_str, |a, t| {
        let Some(sep) = s_of(&a[0], t[0]) else { return Ok(Val::Null) };
        Ok(Val::Str(a[1..].iter().zip(&t[1..]).filter_map(|(v, t)| s_of(v, *t)).collect::<Vec<_>>().join(&sep)))
    }),
    def!("length", t_int, |a, t| Ok(s_of(&a[0], t[0]).map_or(Val::Null, |s| Val::Int(s.chars().count() as i64)))),
    def!("len", t_int, |a, t| Ok(s_of(&a[0], t[0]).map_or(Val::Null, |s| Val::Int(s.chars().count() as i64)))),
    def!("lower", t_str, |a, t| Ok(s_of(&a[0], t[0]).map_or(Val::Null, |s| Val::Str(s.to_lowercase())))),
    def!("upper", t_str, |a, t| Ok(s_of(&a[0], t[0]).map_or(Val::Null, |s| Val::Str(s.to_uppercase())))),
    def!("trim", t_str, |a, t| Ok(s_of(&a[0], t[0]).map_or(Val::Null, |s| Val::Str(s.trim_matches(' ').into())))),
    def!("ltrim", t_str, |a, t| Ok(s_of(&a[0], t[0]).map_or(Val::Null, |s| Val::Str(s.trim_start_matches(' ').into())))),
    def!("rtrim", t_str, |a, t| Ok(s_of(&a[0], t[0]).map_or(Val::Null, |s| Val::Str(s.trim_end_matches(' ').into())))),
    def!("reverse", t_str, |a, t| Ok(s_of(&a[0], t[0]).map_or(Val::Null, |s| Val::Str(s.chars().rev().collect())))),
    def!("substring", t_str, substring),
    def!("substr", t_str, substring),
    def!("left", t_str, |a, t| Ok(match (s_of(&a[0], t[0]), a[1].as_i64()) {
        (Some(s), Some(n)) => Val::Str(s.chars().take(n.max(0) as usize).collect()),
        _ => Val::Null,
    })),
    def!("right", t_str, |a, t| Ok(match (s_of(&a[0], t[0]), a[1].as_i64()) {
        (Some(s), Some(n)) => {
            let c: Vec<char> = s.chars().collect();
            Val::Str(c[c.len().saturating_sub(n.max(0) as usize)..].iter().collect())
        }
        _ => Val::Null,
    })),
    def!("replace", t_str, |a, t| Ok(match (s_of(&a[0], t[0]), s_of(&a[1], t[1]), s_of(&a[2], t[2])) {
        (Some(s), Some(from), Some(to)) => Val::Str(if from.is_empty() { s } else { s.replace(&from, &to) }),
        _ => Val::Null,
    })),
    def!("locate", t_int, |a, t| locate(a, t)),
    def!("position", t_int, |a, t| locate(a, t)),
    def!("ascii", t_int, |a, t| Ok(s_of(&a[0], t[0]).and_then(|s| s.chars().next()).map_or(Val::Null, |c| Val::Int(c as i64)))),
    def!("tostring", t_str, |a, t| Ok(s_of(&a[0], t[0]).map_or(Val::Null, Val::Str))),
    def!("tonumber", t_double, |a, _| Ok(a[0].as_f64().map_or(Val::Null, Val::Float))),
    def!("typeof", t_str, |_, t| Ok(Val::Str(t[0].upper().into()))),
    def!("cidrmatch", t_bool, |a, t| Ok(match (s_of(&a[0], t[0]), s_of(&a[1], t[1])) {
        (Some(ip), Some(cidr)) => match ip.parse::<std::net::IpAddr>() {
            Ok(ip) => Val::Bool(crate::xpack::esql::value::cidr_contains(&cidr, ip)),
            Err(_) => Val::Null,
        },
        _ => Val::Null,
    })),
    // JSON.
    def!("json_extract", t_str, json_extract),
    def!("json_set", t_str, |a, t| json_edit(a, t, true)),
    def!("json_delete", t_str, |a, t| json_edit(a, t, false)),
    def!("json_valid", t_bool, |a, t| Ok(s_of(&a[0], t[0]).map_or(Val::Null, |s| Val::Bool(crate::json::parse(s.as_bytes()).is_ok_and(|v| v.is_some()))))),
    def!("json", t_str, |a, t| Ok(s_of(&a[0], t[0]).and_then(|s| crate::json::parse(s.as_bytes()).ok().flatten()).map_or(Val::Null, |v| Val::Str(v.to_json())))),
    def!("json_keys", t_str, |a, t| Ok(match s_of(&a[0], t[0]).and_then(|s| crate::json::parse(s.as_bytes()).ok().flatten()) {
        Some(Value::Obj(m)) => Val::Str(Value::Arr(m.keys().map(|k| Value::Str(k.clone())).collect()).to_json()),
        _ => Val::Null,
    })),
    def!("json_array_length", t_int, |a, t| Ok(match s_of(&a[0], t[0]).and_then(|s| crate::json::parse(s.as_bytes()).ok().flatten()) {
        Some(Value::Arr(x)) => Val::Int(x.len() as i64),
        _ => Val::Null,
    })),
    // Dates and times.
    def!("now", t_ts, |_, _| Ok(Val::Ts(chrono::Utc::now().naive_utc()))),
    def!("current_timestamp", t_ts, |_, _| Ok(Val::Ts(chrono::Utc::now().naive_utc()))),
    def!("sysdate", t_ts, |_, _| Ok(Val::Ts(chrono::Utc::now().naive_utc()))),
    def!("curdate", t_date, |_, _| Ok(Val::Date(chrono::Utc::now().date_naive()))),
    def!("current_date", t_date, |_, _| Ok(Val::Date(chrono::Utc::now().date_naive()))),
    def!("timestamp", t_ts, |a, _| Ok(ts_of(&a[0]).map_or(Val::Null, Val::Ts))),
    def!("date", t_date, |a, _| Ok(ts_of(&a[0]).map_or(Val::Null, |t| Val::Date(t.date())))),
    def!("time", t_time, |a, _| Ok(match &a[0] {
        Val::Time(t) => Val::Time(*t),
        Val::Str(s) => NaiveTime::parse_from_str(s, "%H:%M:%S%.f").ok().map(Val::Time).or_else(|| value::parse_ts(s).map(|t| Val::Time(t.time()))).unwrap_or(Val::Null),
        v => ts_of(v).map_or(Val::Null, |t| Val::Time(t.time())),
    })),
    def!("hour", t_int, |a, _| part(a, |t| t.hour() as i64)),
    def!("hour_of_day", t_int, |a, _| part(a, |t| t.hour() as i64)),
    def!("minute", t_int, |a, _| part(a, |t| t.minute() as i64)),
    def!("minute_of_hour", t_int, |a, _| part(a, |t| t.minute() as i64)),
    def!("second", t_int, |a, _| part(a, |t| t.second() as i64)),
    def!("second_of_minute", t_int, |a, _| part(a, |t| t.second() as i64)),
    def!("day", t_int, |a, _| part(a, |t| t.day() as i64)),
    def!("dayofmonth", t_int, |a, _| part(a, |t| t.day() as i64)),
    def!("day_of_month", t_int, |a, _| part(a, |t| t.day() as i64)),
    def!("dayofweek", t_int, |a, _| part(a, |t| t.weekday().number_from_sunday() as i64)),
    def!("day_of_week", t_int, |a, _| part(a, |t| t.weekday().number_from_sunday() as i64)),
    def!("dayofyear", t_int, |a, _| part(a, |t| t.ordinal() as i64)),
    def!("day_of_year", t_int, |a, _| part(a, |t| t.ordinal() as i64)),
    def!("month", t_int, |a, _| part(a, |t| t.month() as i64)),
    def!("month_of_year", t_int, |a, _| part(a, |t| t.month() as i64)),
    def!("quarter", t_int, |a, _| part(a, |t| t.month0() as i64 / 3 + 1)),
    def!("year", t_int, |a, _| part(a, |t| t.year() as i64)),
    def!("week", t_int, |a, _| part(a, |t| t.iso_week().week() as i64)),
    def!("unix_timestamp", t_double, |a, _| Ok(match a.first() {
        None => Val::Float(chrono::Utc::now().timestamp() as f64),
        Some(v) => ts_of(v).map_or(Val::Null, |t| Val::Float(value::ts_millis(&t) as f64 / 1000.0)),
    })),
    def!("from_unixtime", t_ts, |a, _| Ok(a[0].as_f64().and_then(|s| value::ts_from_millis((s * 1000.0) as i64)).map_or(Val::Null, Val::Ts))),
    def!("date_format", t_str, |a, t| Ok(match (ts_of(&a[0]), s_of(&a[1], t[1])) {
        (Some(ts), Some(f)) => Val::Str(format_date(&ts, &f)),
        _ => Val::Null,
    })),
    def!("str_to_date", t_ts, |a, t| Ok(match (s_of(&a[0], t[0]), s_of(&a[1], t[1])) {
        (Some(s), Some(f)) => parse_with(&s, &mysql_to_chrono(&f)).map_or(Val::Null, Val::Ts),
        _ => Val::Null,
    })),
    def!("datediff", t_long, |a, _| Ok(match (ts_of(&a[0]), ts_of(&a[1])) {
        (Some(x), Some(y)) => Val::Int((x.date() - y.date()).num_days()),
        _ => Val::Null,
    })),
    def!("timestampdiff", t_long, |a, _| Ok(match (a[0].as_str(), ts_of(&a[1]), ts_of(&a[2])) {
        (Some(unit), Some(x), Some(y)) => diff(unit, x, y),
        _ => Val::Null,
    })),
    def!("timestampadd", t_ts, |a, _| Ok(match (a[0].as_str(), a[1].as_i64(), ts_of(&a[2])) {
        (Some(unit), Some(n), Some(t)) => add_unit(t, unit, n).map_or(Val::Null, Val::Ts),
        _ => Val::Null,
    })),
    def!("date_add", t_ts, |a, _| Ok(match (ts_of(&a[0]), a[1].as_i64()) {
        (Some(t), Some(ms)) => value::ts_from_millis(value::ts_millis(&t) + ms).map_or(Val::Null, Val::Ts),
        _ => Val::Null,
    })),
    def!("date_sub", t_ts, |a, _| Ok(match (ts_of(&a[0]), a[1].as_i64()) {
        (Some(t), Some(ms)) => value::ts_from_millis(value::ts_millis(&t) - ms).map_or(Val::Null, Val::Ts),
        _ => Val::Null,
    })),
    def!("adddate", t_ts, |a, _| Ok(match (ts_of(&a[0]), a[1].as_i64()) {
        (Some(t), Some(d)) => value::ts_from_millis(value::ts_millis(&t) + d * 86_400_000).map_or(Val::Null, Val::Ts),
        _ => Val::Null,
    })),
    def!("subdate", t_ts, |a, _| Ok(match (ts_of(&a[0]), a[1].as_i64()) {
        (Some(t), Some(d)) => value::ts_from_millis(value::ts_millis(&t) - d * 86_400_000).map_or(Val::Null, Val::Ts),
        _ => Val::Null,
    })),
    def!("extract", t_long, |a, _| Ok(match (a[0].as_str(), ts_of(&a[1])) {
        (Some(unit), Some(t)) => Val::Int(match unit {
            "YEAR" => t.year() as i64,
            "MONTH" => t.month() as i64,
            "DAY" => t.day() as i64,
            "HOUR" => t.hour() as i64,
            "MINUTE" => t.minute() as i64,
            "SECOND" => t.second() as i64,
            "QUARTER" => t.month0() as i64 / 3 + 1,
            "WEEK" => t.iso_week().week() as i64,
            _ => return Ok(Val::Null),
        }),
        _ => Val::Null,
    })),
    // `INTERVAL n unit` as milliseconds.
    def!("__interval", t_long, |a, _| Ok(match (a[0].as_i64(), a[1].as_str()) {
        (Some(n), Some(u)) => Val::Int(n * unit_ms(u)),
        _ => Val::Null,
    })),
];

pub fn lookup(name: &str) -> Option<&'static Def> {
    DEFS.iter().find(|d| d.name == name)
}

fn unit_ms(u: &str) -> i64 {
    match u {
        "MICROSECOND" => 0,
        "MILLISECOND" => 1,
        "SECOND" => 1000,
        "MINUTE" => 60_000,
        "HOUR" => 3_600_000,
        "DAY" => 86_400_000,
        "WEEK" => 7 * 86_400_000,
        "MONTH" => 30 * 86_400_000,
        "QUARTER" => 91 * 86_400_000,
        "YEAR" => 365 * 86_400_000,
        _ => 0,
    }
}

// ------------------------------------------------------------------------------ helpers

fn pow(a: &[Val], _: &[Ty]) -> EsResult<Val> {
    Ok(match (a[0].as_f64(), a[1].as_f64()) {
        (Some(x), Some(y)) => finite(x.powf(y)),
        _ => Val::Null,
    })
}

fn modulus(a: &[Val], _: &[Ty]) -> EsResult<Val> {
    Ok(match (&a[0], &a[1]) {
        (Val::Int(x), Val::Int(y)) => {
            if *y == 0 {
                Val::Null
            } else {
                Val::Int(x.wrapping_rem(*y))
            }
        }
        (x, y) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) if y != 0.0 => {
                // Decimal remainder (`mod(3.1, 2)` is `1.1`, not the binary 1.1000000000000001).
                let scale = 10f64.powi(decimals(x).max(decimals(y)) as i32);
                finite(((x * scale).round() % (y * scale).round()) / scale)
            }
            _ => Val::Null,
        },
    })
}

/// The decimal places of a double's shortest representation.
fn decimals(x: f64) -> usize {
    let s = crate::json::write::java_double(x);
    if s.contains('E') {
        return 0;
    }
    s.split_once('.').map_or(0, |(_, f)| f.trim_end_matches('0').len())
}

fn ceil_floor(a: &[Val], t: &[Ty], f: fn(f64) -> f64) -> EsResult<Val> {
    Ok(match &a[0] {
        Val::Int(i) => Val::Int(*i),
        v => match v.as_f64() {
            Some(x) if t[0] == Ty::Str => finite(f(x)),
            Some(x) => {
                let r = f(x);
                if r.abs() < 9.2e18 { Val::Int(r as i64) } else { Val::Float(r) }
            }
            None => Val::Null,
        },
    })
}

fn round(a: &[Val], t: &[Ty]) -> EsResult<Val> {
    let d = a.get(1).and_then(Val::as_i64).unwrap_or(0);
    Ok(match &a[0] {
        Val::Int(i) if d >= 0 => Val::Int(*i),
        Val::Int(i) => {
            let p = 10i64.pow((-d) as u32);
            Val::Int(((*i as f64 / p as f64).round() as i64) * p)
        }
        v => match v.as_f64() {
            Some(x) => {
                let p = 10f64.powi(d as i32);
                // HALF_UP on the decimal value.
                let r = (x * p).abs();
                let r = if (r - r.floor() - 0.5).abs() < 1e-9 { r.floor() + 1.0 } else { r.round() };
                let r = r.copysign(x) / p;
                if t[0] == Ty::Float { Val::Float(r as f32 as f64) } else { finite(r) }
            }
            None => Val::Null,
        },
    })
}

fn substring(a: &[Val], t: &[Ty]) -> EsResult<Val> {
    let Some(s) = s_of(&a[0], t[0]) else { return Ok(Val::Null) };
    let Some(start) = a.get(1).and_then(Val::as_i64) else { return Ok(Val::Null) };
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len() as i64;
    let from = if start > 0 { start - 1 } else if start < 0 { n + start } else { return Ok(Val::Str(String::new())) };
    if from < 0 || from >= n {
        return Ok(Val::Str(String::new()));
    }
    let len = a.get(2).and_then(Val::as_i64).unwrap_or(n);
    let to = (from + len.max(0)).min(n);
    Ok(Val::Str(chars[from as usize..to as usize].iter().collect()))
}

fn locate(a: &[Val], t: &[Ty]) -> EsResult<Val> {
    let (Some(sub), Some(s)) = (s_of(&a[0], t[0]), s_of(&a[1], t[1])) else { return Ok(Val::Null) };
    let from = a.get(2).and_then(Val::as_i64).unwrap_or(1).max(1) as usize - 1;
    let chars: Vec<char> = s.chars().collect();
    if from > chars.len() {
        return Ok(Val::Int(0));
    }
    let hay: String = chars[from..].iter().collect();
    Ok(Val::Int(hay.find(&sub).map_or(0, |b| hay[..b].chars().count() + from + 1) as i64))
}

fn like(a: &[Val], t: &[Ty], insensitive: bool) -> EsResult<Val> {
    let (Some(s), Some(p)) = (s_of(&a[0], t[0]), s_of(&a[1], t[1])) else { return Ok(Val::Null) };
    let insensitive = a.get(2).and_then(Val::as_bool).unwrap_or(insensitive);
    let mut re = String::from("(?s)^");
    if insensitive {
        re.insert_str(0, "(?i)");
    }
    let mut it = p.chars();
    while let Some(c) = it.next() {
        match c {
            '%' => re.push_str(".*"),
            '_' => re.push('.'),
            '\\' => {
                if let Some(n) = it.next() {
                    re.push_str(&regex::escape(&n.to_string()));
                }
            }
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    Ok(regex::Regex::new(&re).map_or(Val::Null, |r| Val::Bool(r.is_match(&s))))
}

fn regexp(a: &[Val], t: &[Ty]) -> EsResult<Val> {
    let (Some(s), Some(p)) = (s_of(&a[0], t[0]), s_of(&a[1], t[1])) else { return Ok(Val::Null) };
    regex::Regex::new(&p).map(|r| Val::Bool(r.is_match(&s))).map_err(|e| super::semantic(format!("Invalid regular expression: {e}")))
}

fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_lowercase).collect()
}

fn relevance(a: &[Val], t: &[Ty], phrase: bool) -> EsResult<Val> {
    let Some(q) = a.get(1).and_then(|v| s_of(v, Ty::Str)) else { return Ok(Val::Null) };
    let values: Vec<String> = match &a[0] {
        Val::Arr(x) => x.iter().filter_map(|v| s_of(v, t[0])).collect(),
        v => s_of(v, t[0]).into_iter().collect(),
    };
    let qt = tokens(&q);
    Ok(Val::Bool(values.iter().any(|v| {
        let vt = tokens(v);
        if phrase {
            qt.is_empty() || vt.windows(qt.len()).any(|w| w == qt.as_slice())
        } else {
            qt.iter().any(|q| vt.contains(q))
        }
    })))
}

/// A timestamp from a cell: timestamps, dates, texts, epoch millis.
pub fn ts_of(v: &Val) -> Option<NaiveDateTime> {
    match v {
        Val::Ts(t) => Some(*t),
        Val::Date(d) => d.and_hms_opt(0, 0, 0),
        Val::Time(t) => Some(chrono::Utc::now().date_naive().and_time(*t)),
        Val::Str(s) => value::parse_ts(s),
        Val::Int(ms) => value::ts_from_millis(*ms),
        _ => None,
    }
}

fn part(a: &[Val], f: impl Fn(&NaiveDateTime) -> i64) -> EsResult<Val> {
    Ok(match &a[0] {
        Val::Time(t) => {
            let d = NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch").and_time(*t);
            Val::Int(f(&d))
        }
        v => ts_of(v).map_or(Val::Null, |t| Val::Int(f(&t))),
    })
}

fn diff(unit: &str, x: NaiveDateTime, y: NaiveDateTime) -> Val {
    let ms = value::ts_millis(&y) - value::ts_millis(&x);
    let months = || (y.year() as i64 - x.year() as i64) * 12 + y.month() as i64 - x.month() as i64 - i64::from((y.day(), y.time()) < (x.day(), x.time()));
    Val::Int(match unit {
        "MICROSECOND" => ms * 1000,
        "MILLISECOND" => ms,
        "SECOND" => ms / 1000,
        "MINUTE" => ms / 60_000,
        "HOUR" => ms / 3_600_000,
        "DAY" => ms / 86_400_000,
        "WEEK" => ms / (7 * 86_400_000),
        "MONTH" => months(),
        "QUARTER" => months() / 3,
        "YEAR" => months() / 12,
        _ => return Val::Null,
    })
}

fn add_unit(t: NaiveDateTime, unit: &str, n: i64) -> Option<NaiveDateTime> {
    match unit {
        "MONTH" | "QUARTER" | "YEAR" => {
            let m = n * match unit {
                "MONTH" => 1,
                "QUARTER" => 3,
                _ => 12,
            };
            if m >= 0 { t.checked_add_months(chrono::Months::new(m as u32)) } else { t.checked_sub_months(chrono::Months::new((-m) as u32)) }
        }
        u => value::ts_from_millis(value::ts_millis(&t) + n * unit_ms(u)),
    }
}

/// `DATE_FORMAT`: MySQL `%` specifiers, or a Java pattern when there are none.
fn format_date(t: &NaiveDateTime, f: &str) -> String {
    if f.contains('%') {
        return t.format(&mysql_to_chrono(f)).to_string();
    }
    let mut out = String::new();
    let chars: Vec<char> = f.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let mut n = 1;
        while i + n < chars.len() && chars[i + n] == c {
            n += 1;
        }
        let piece = match (c, n) {
            ('y', 2) => format!("{:02}", t.year() % 100),
            ('y', _) => format!("{:04}", t.year()),
            ('M', 1) => t.month().to_string(),
            ('M', 2) => format!("{:02}", t.month()),
            ('M', 3) => t.format("%b").to_string(),
            ('M', _) => t.format("%B").to_string(),
            ('d', 1) => t.day().to_string(),
            ('d', _) => format!("{:02}", t.day()),
            ('H', 1) => t.hour().to_string(),
            ('H', _) => format!("{:02}", t.hour()),
            ('m', 1) => t.minute().to_string(),
            ('m', _) => format!("{:02}", t.minute()),
            ('s', 1) => t.second().to_string(),
            ('s', _) => format!("{:02}", t.second()),
            ('S', k) => format!("{:09}", t.nanosecond())[..k.min(9)].to_string(),
            (c, k) => std::iter::repeat_n(c, k).collect(),
        };
        out.push_str(&piece);
        i += n;
    }
    out
}

fn mysql_to_chrono(f: &str) -> String {
    let mut out = String::new();
    let mut it = f.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('i') => out.push_str("%M"),
            Some('s') | Some('S') => out.push_str("%S"),
            Some('H') | Some('k') => out.push_str("%H"),
            Some('h') | Some('I') | Some('l') => out.push_str("%I"),
            Some('M') => out.push_str("%B"),
            Some('b') => out.push_str("%b"),
            Some('c') => out.push_str("%-m"),
            Some('e') => out.push_str("%-d"),
            Some('f') => out.push_str("%6f"),
            Some('p') => out.push_str("%p"),
            Some('W') => out.push_str("%A"),
            Some('a') => out.push_str("%a"),
            Some('T') => out.push_str("%H:%M:%S"),
            Some('r') => out.push_str("%I:%M:%S %p"),
            Some('j') => out.push_str("%j"),
            Some(o) => {
                out.push('%');
                out.push(o);
            }
            None => out.push('%'),
        }
    }
    out
}

fn parse_with(s: &str, f: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(s, f).ok().or_else(|| NaiveDate::parse_from_str(s, f).ok().and_then(|d| d.and_hms_opt(0, 0, 0)))
}

fn json_extract(a: &[Val], t: &[Ty]) -> EsResult<Val> {
    let Some(doc) = s_of(&a[0], t[0]).and_then(|s| crate::json::parse(s.as_bytes()).ok().flatten()) else { return Ok(Val::Null) };
    let get = |p: &Val| -> Value { s_of(p, Ty::Str).and_then(|p| json_path(&doc, &p).cloned()).unwrap_or(Value::Null) };
    if a.len() > 2 {
        let all: Vec<Value> = a[1..].iter().map(get).collect();
        return Ok(Val::Str(Value::Arr(all).to_json()));
    }
    Ok(match get(&a[1]) {
        Value::Null => Val::Null,
        Value::Str(s) => Val::Str(s),
        other => Val::Str(other.to_json()),
    })
}

fn path_parts(p: &str) -> Vec<String> {
    let p = p.strip_prefix('$').unwrap_or(p);
    let p = p.strip_prefix('.').unwrap_or(p);
    let mut out = Vec::new();
    for part in p.split('.') {
        if part.is_empty() {
            continue;
        }
        match part.split_once('[') {
            Some((name, rest)) => {
                if !name.is_empty() {
                    out.push(name.to_string());
                }
                for idx in rest.split('[') {
                    out.push(format!("[{}", idx.trim_end_matches(']')));
                }
            }
            None => out.push(part.to_string()),
        }
    }
    out
}

fn json_path<'v>(doc: &'v Value, p: &str) -> Option<&'v Value> {
    let mut cur = doc;
    for part in path_parts(p) {
        cur = match part.strip_prefix('[') {
            Some(i) => cur.as_array()?.get(i.parse::<usize>().ok()?)?,
            None => cur.get(&part)?,
        };
    }
    Some(cur)
}

fn json_edit(a: &[Val], t: &[Ty], set: bool) -> EsResult<Val> {
    let Some(mut doc) = s_of(&a[0], t[0]).and_then(|s| crate::json::parse(s.as_bytes()).ok().flatten()) else { return Ok(Val::Null) };
    let mut i = 1;
    while i < a.len() {
        let Some(path) = s_of(&a[i], Ty::Str) else { break };
        let parts = path_parts(&path);
        if set {
            let v = match a.get(i + 1) {
                Some(Val::Str(s)) => Value::Str(s.clone()),
                Some(v) => v.to_json(t.get(i + 1).copied().unwrap_or(Ty::Undefined)),
                None => Value::Null,
            };
            set_path(&mut doc, &parts, v);
            i += 2;
        } else {
            delete_path(&mut doc, &parts);
            i += 1;
        }
    }
    Ok(Val::Str(doc.to_json()))
}

fn set_path(doc: &mut Value, parts: &[String], v: Value) {
    let Some((last, init)) = parts.split_last() else { return };
    let mut cur = doc;
    for p in init {
        let Some(next) = (match p.strip_prefix('[') {
            Some(i) => i.parse::<usize>().ok().and_then(|i| match cur { Value::Arr(a) => a.get_mut(i), _ => None }),
            None => cur.as_object_mut().and_then(|o| o.get_mut(p)),
        }) else {
            return;
        };
        cur = next;
    }
    match last.strip_prefix('[') {
        Some(i) => {
            if let (Value::Arr(arr), Ok(i)) = (cur, i.parse::<usize>())
                && i < arr.len()
            {
                arr[i] = v;
            }
        }
        None => {
            if let Some(o) = cur.as_object_mut()
                && o.contains_key(last)
            {
                o.insert(last.clone(), v);
            }
        }
    }
}

fn delete_path(doc: &mut Value, parts: &[String]) {
    let Some((last, init)) = parts.split_last() else { return };
    let mut cur = doc;
    for p in init {
        let Some(next) = (match p.strip_prefix('[') {
            Some(i) => i.parse::<usize>().ok().and_then(|i| match cur { Value::Arr(a) => a.get_mut(i), _ => None }),
            None => cur.as_object_mut().and_then(|o| o.get_mut(p)),
        }) else {
            return;
        };
        cur = next;
    }
    match last.strip_prefix('[') {
        Some(i) => {
            if let (Value::Arr(arr), Ok(i)) = (cur, i.parse::<usize>())
                && i < arr.len()
            {
                arr.remove(i);
            }
        }
        None => {
            if let Some(o) = cur.as_object_mut() {
                o.shift_remove(last);
            }
        }
    }
}

/// `CAST(x AS type)`.
pub fn cast(v: Val, from: Ty, to: Ty) -> Val {
    if v.is_null() {
        return Val::Null;
    }
    match to {
        Ty::Str => v.to_text(from).map_or(Val::Null, Val::Str),
        Ty::Int | Ty::Long | Ty::Short | Ty::Byte => match v {
            Val::Int(i) => Val::Int(i),
            Val::Float(f) => Val::Int(f.trunc() as i64),
            Val::Bool(b) => Val::Int(i64::from(b)),
            Val::Str(s) => s.trim().parse::<i64>().map(Val::Int).or_else(|_| s.trim().parse::<f64>().map(|f| Val::Int(f.trunc() as i64))).unwrap_or(Val::Null),
            _ => Val::Null,
        },
        Ty::Double | Ty::Float => match v {
            Val::Bool(b) => Val::Float(f64::from(u8::from(b))),
            v => v.as_f64().map_or(Val::Null, Val::Float),
        },
        Ty::Bool => match v {
            Val::Bool(b) => Val::Bool(b),
            Val::Int(i) => Val::Bool(i != 0),
            Val::Float(f) => Val::Bool(f != 0.0),
            Val::Str(s) => match s.to_ascii_lowercase().as_str() {
                "true" => Val::Bool(true),
                "false" => Val::Bool(false),
                _ => Val::Null,
            },
            _ => Val::Null,
        },
        Ty::Timestamp => ts_of(&v).map_or(Val::Null, Val::Ts),
        Ty::Date => ts_of(&v).map_or(Val::Null, |t| Val::Date(t.date())),
        Ty::Time => ts_of(&v).map_or(Val::Null, |t| Val::Time(t.time())),
        _ => v,
    }
}

#[allow(dead_code)]
fn unsupported(name: &str) -> EsError {
    super::semantic(format!("Function [{name}] is not supported"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_modulus() {
        assert_eq!(modulus(&[Val::Float(3.1), Val::Int(2)], &[Ty::Double, Ty::Int]).unwrap().as_f64(), Some(1.1));
    }

    #[test]
    fn json_paths() {
        let r = json_edit(&[Val::Str(r#"{"name":"alice","scores":[90,85,92]}"#.into()), Val::Str("$.name".into()), Val::Str("modified_alice".into())], &[Ty::Str, Ty::Str, Ty::Str], true).unwrap();
        assert_eq!(r.as_str(), Some(r#"{"name":"modified_alice","scores":[90,85,92]}"#));
        let r = json_extract(&[Val::Str(r#"{"name":"John"}"#.into()), Val::Str("name".into()), Val::Str("age".into())], &[Ty::Str, Ty::Str, Ty::Str]).unwrap();
        assert_eq!(r.as_str(), Some(r#"["John",null]"#));
    }

    #[test]
    fn java_date_patterns() {
        let t = value::parse_ts("2024-05-01 07:08:09").unwrap();
        assert_eq!(format_date(&t, "yyyy-MM-dd"), "2024-05-01");
        assert_eq!(format_date(&t, "%Y-%m-%d %H:%i:%S"), "2024-05-01 07:08:09");
    }
}
