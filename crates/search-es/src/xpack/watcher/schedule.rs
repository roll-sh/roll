//! Watch triggers: the `schedule` of a watch (`interval`, `cron`, `hourly`, `daily`, `weekly`,
//! `monthly`, `yearly`, with an optional `timezone`) and when it next fires.
//!
//! Like Elasticsearch, every calendar schedule is a set of Quartz cron expressions
//! (`CronnableSchedule`), so one evaluator ([`next_cron`]) serves them all. An `interval`
//! schedule fires every interval counted from when the watch was activated
//! (`TickerScheduleTriggerEngine`).

use crate::error::{EsError, EsResult};
use crate::json::Value;
use crate::time::Zone;
use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};

/// A parsed schedule.
#[derive(Debug, Clone, PartialEq)]
pub enum Schedule {
    Interval(i64),
    Cron { exprs: Vec<String>, zone: Zone },
}

fn parse_err(msg: impl Into<String>) -> EsError {
    EsError::new(400, "parse_exception", msg)
}

/// Parses `trigger.schedule` (`WatchParser`, `ScheduleRegistry`): the schedule, or the error
/// Elasticsearch gives for it.
pub fn parse(watch_id: &str, trigger: Option<&Value>) -> EsResult<Schedule> {
    let Some(Value::Obj(t)) = trigger else {
        return Err(parse_err(format!("could not parse watch [{watch_id}]. missing required field [trigger]")));
    };
    let Some((ty, body)) = t.iter().next() else { return Err(parse_err(format!("could not parse trigger for [{watch_id}]"))) };
    if ty != "schedule" {
        return Err(parse_err(format!("could not parse trigger [{ty}] for [{watch_id}]")).caused_by(EsError::illegal_argument(format!("unknown trigger type [{ty}]"))));
    }
    let Value::Obj(s) = body else { return Err(parse_err(format!("could not parse [schedule] trigger for [{watch_id}]"))) };
    let zone = match s.get("timezone") {
        None => Zone::UTC,
        Some(z) => {
            let z = z.to_es_string();
            Zone::parse(&z).map_err(|e| parse_err(format!("could not parse schedule. invalid timezone [{z}]")).caused_by(EsError::java(400, "zone_rules_exception", e)))?
        }
    };
    let Some((kind, spec)) = s.iter().find(|(k, _)| k.as_str() != "timezone") else {
        return Err(parse_err(format!("could not parse [schedule] trigger for [{watch_id}]")).caused_by(parse_err("could not parse schedule. expected a schedule type field, but no fields were found")));
    };
    let wrap = |e: EsError| parse_err(format!("could not parse [schedule] trigger for [{watch_id}]")).caused_by(e);
    let exprs = match kind.as_str() {
        "interval" => return interval(spec).map(Schedule::Interval).map_err(wrap),
        "cron" => {
            let list: Vec<String> = match spec {
                Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
                other => vec![other.to_es_string()],
            };
            for e in &list {
                super::check_cron(e)?;
            }
            list
        }
        "hourly" => hourly(spec).map_err(wrap)?,
        "daily" => daily(spec).map_err(wrap)?,
        "weekly" => weekly(spec).map_err(wrap)?,
        "monthly" => monthly(spec).map_err(wrap)?,
        "yearly" => yearly(spec).map_err(wrap)?,
        other => return Err(parse_err(format!("could not parse schedule. unknown schedule type [{other}]"))),
    };
    Ok(Schedule::Cron { exprs, zone })
}

/// `interval`: a number of seconds, or a time value with unit `s`, `m`, `h`, `d` or `w` (`ms`
/// too), at least a second.
fn interval(v: &Value) -> EsResult<i64> {
    let ms = match v {
        Value::Num(_) => v.as_i64().unwrap_or(0) * 1000,
        _ => {
            let s = v.to_es_string();
            let t = s.trim();
            let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
            let (num, unit) = t.split_at(split);
            let n: i64 = num.parse().map_err(|_| parse_err(format!("could not parse [interval] schedule. unrecognized interval format [{s}]")))?;
            let mult = match unit.to_ascii_lowercase().as_str() {
                "" | "s" => 1000,
                "ms" => 1,
                "m" => 60_000,
                "h" => 3_600_000,
                "d" => 86_400_000,
                "w" => 604_800_000,
                _ => return Err(parse_err(format!("could not parse [interval] schedule. unrecognized interval format [{s}]"))),
            };
            n * mult
        }
    };
    if ms < 1000 {
        return Err(EsError::illegal_argument(format!("interval can't be lower than 1000 ms, but [{ms}ms] was specified")));
    }
    Ok(ms)
}

fn ints(v: Option<&Value>, default: &[i64], what: &str, lo: i64, hi: i64) -> EsResult<Vec<i64>> {
    let list: Vec<&Value> = match v {
        None => return Ok(default.to_vec()),
        Some(Value::Arr(a)) => a.iter().collect(),
        Some(x) => vec![x],
    };
    list.into_iter()
        .map(|x| {
            let n = x.as_i64().or_else(|| x.to_es_string().trim().parse().ok()).ok_or_else(|| parse_err(format!("could not parse {what} value. expected a number")))?;
            if n < lo || n > hi {
                return Err(parse_err(format!("invalid {what} value [{n}]. {what} must be between {lo} and {hi}")));
            }
            Ok(n)
        })
        .collect()
}

fn join(v: &[i64]) -> String {
    v.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
}

fn hourly(v: &Value) -> EsResult<Vec<String>> {
    let minutes = ints(v.get("minute"), &[0], "minute", 0, 59)?;
    Ok(vec![format!("0 {} * * * ?", join(&minutes))])
}

/// The `at` of a daily, weekly, monthly or yearly schedule: `(hours, minutes)` pairs.
fn day_times(v: Option<&Value>) -> EsResult<Vec<(Vec<i64>, Vec<i64>)>> {
    let one = |x: &Value| -> EsResult<(Vec<i64>, Vec<i64>)> {
        match x {
            Value::Obj(_) => Ok((ints(x.get("hour"), &[0], "hour", 0, 23)?, ints(x.get("minute"), &[0], "minute", 0, 59)?)),
            _ => {
                let s = x.to_es_string().to_ascii_lowercase();
                match s.as_str() {
                    "midnight" => Ok((vec![0], vec![0])),
                    "noon" => Ok((vec![12], vec![0])),
                    _ => {
                        let (h, m) = s.split_once(':').ok_or_else(|| parse_err(format!("could not parse time [{s}]. time format must be in the form of hh:mm")))?;
                        let h: i64 = h.parse().map_err(|_| parse_err(format!("could not parse time [{s}]. time format must be in the form of hh:mm")))?;
                        let m: i64 = m.parse().map_err(|_| parse_err(format!("could not parse time [{s}]. time format must be in the form of hh:mm")))?;
                        if !(0..=23).contains(&h) || !(0..=59).contains(&m) {
                            return Err(parse_err(format!("could not parse time [{s}]. invalid time")));
                        }
                        Ok((vec![h], vec![m]))
                    }
                }
            }
        }
    };
    match v {
        None => Ok(vec![(vec![0], vec![0])]),
        Some(Value::Arr(a)) => a.iter().map(one).collect(),
        Some(x) => Ok(vec![one(x)?]),
    }
}

fn daily(v: &Value) -> EsResult<Vec<String>> {
    Ok(day_times(v.get("at"))?.into_iter().map(|(h, m)| format!("0 {} {} * * ?", join(&m), join(&h))).collect())
}

fn list_of(v: &Value) -> Vec<Value> {
    match v {
        Value::Arr(a) => a.clone(),
        other => vec![other.clone()],
    }
}

fn weekly(v: &Value) -> EsResult<Vec<String>> {
    const DAYS: [(&str, &str); 7] = [("sunday", "SUN"), ("monday", "MON"), ("tuesday", "TUE"), ("wednesday", "WED"), ("thursday", "THU"), ("friday", "FRI"), ("saturday", "SAT")];
    let mut out = Vec::new();
    for item in list_of(v) {
        let days: Vec<String> = match item.get("on") {
            None => vec!["MON".into()],
            Some(d) => list_of(d)
                .iter()
                .map(|d| {
                    let s = d.to_es_string().to_ascii_lowercase();
                    DAYS.iter()
                        .find(|(full, short)| s == *full || s == short.to_ascii_lowercase() || d.as_i64().is_some_and(|n| (1..=7).contains(&n) && DAYS[(n % 7) as usize].1 == *short))
                        .map(|(_, short)| short.to_string())
                        .ok_or_else(|| parse_err(format!("could not parse day of the week [{s}]")))
                })
                .collect::<EsResult<_>>()?,
        };
        for (h, m) in day_times(item.get("at"))? {
            out.push(format!("0 {} {} ? * {}", join(&m), join(&h), days.join(",")));
        }
    }
    Ok(out)
}

fn month_days(v: Option<&Value>) -> EsResult<String> {
    let Some(v) = v else { return Ok("1".into()) };
    let mut out = Vec::new();
    for d in list_of(v) {
        let s = d.to_es_string().to_ascii_lowercase();
        if s == "last_day" || s == "-1" {
            out.push("L".to_string());
            continue;
        }
        let n: i64 = s.parse().map_err(|_| parse_err(format!("could not parse day of the month [{s}]")))?;
        if !(1..=31).contains(&n) {
            return Err(parse_err(format!("invalid month day [{n}]")));
        }
        out.push(n.to_string());
    }
    Ok(out.join(","))
}

fn monthly(v: &Value) -> EsResult<Vec<String>> {
    let mut out = Vec::new();
    for item in list_of(v) {
        let days = month_days(item.get("on"))?;
        for (h, m) in day_times(item.get("at"))? {
            out.push(format!("0 {} {} {days} * ?", join(&m), join(&h)));
        }
    }
    Ok(out)
}

fn yearly(v: &Value) -> EsResult<Vec<String>> {
    const MONTHS: [&str; 12] = ["january", "february", "march", "april", "may", "june", "july", "august", "september", "october", "november", "december"];
    let mut out = Vec::new();
    for item in list_of(v) {
        let months: Vec<String> = match item.get("in") {
            None => vec!["1".into()],
            Some(m) => list_of(m)
                .iter()
                .map(|m| {
                    let s = m.to_es_string().to_ascii_lowercase();
                    MONTHS
                        .iter()
                        .position(|n| *n == s || n[..3] == s)
                        .map(|i| (i + 1) as i64)
                        .or_else(|| s.parse::<i64>().ok().filter(|n| (1..=12).contains(n)))
                        .map(|n| n.to_string())
                        .ok_or_else(|| parse_err(format!("could not parse month [{s}]")))
                })
                .collect::<EsResult<_>>()?,
        };
        let days = month_days(item.get("on"))?;
        for (h, m) in day_times(item.get("at"))? {
            out.push(format!("0 {} {} {days} {} ?", join(&m), join(&h), months.join(",")));
        }
    }
    Ok(out)
}

impl Schedule {
    /// The first time this schedule fires strictly after `after` (Unix ms). `start` is when an
    /// interval schedule started counting (the watch's activation).
    pub fn next_after(&self, start: i64, after: i64) -> Option<i64> {
        match self {
            Schedule::Interval(ms) => {
                if after < start {
                    return Some(start + ms);
                }
                Some(start + ((after - start) / ms + 1) * ms)
            }
            Schedule::Cron { exprs, zone } => exprs.iter().filter_map(|e| next_cron(e, zone, after)).min(),
        }
    }
}

// --- Quartz cron -------------------------------------------------------------------------------

/// A day-of-month rule.
#[derive(Debug)]
enum Dom {
    Any,
    Set(u64),
    /// `L` and `L-n`: n days before the month's last.
    Last(u32),
    /// `LW`: the month's last weekday.
    LastWeekday,
    /// `nW`: the weekday nearest day n, within the month.
    NearestWeekday(u32),
}

/// A day-of-week rule (1 = Sunday … 7 = Saturday).
#[derive(Debug)]
enum Dow {
    Any,
    Set(u8),
    /// `nL`: the month's last day n.
    Last(u32),
    /// `n#k`: the month's k-th day n.
    Nth(u32, u32),
}

struct Quartz {
    seconds: u64,
    minutes: u64,
    hours: u32,
    dom: Dom,
    months: u16,
    dow: Dow,
    years: Option<Vec<(i32, i32, i32)>>,
}

const MONTH_NAMES: [&str; 12] = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];
const DAY_NAMES: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];

fn value_of(s: &str, names: &[&str], base: u32) -> Option<u32> {
    let up = s.to_ascii_uppercase();
    names.iter().position(|n| *n == up).map(|i| i as u32 + base).or_else(|| s.parse().ok())
}

/// A field as `(lo, hi, step)` ranges.
fn ranges(text: &str, lo: u32, hi: u32, names: &[&str], base: u32) -> Option<Vec<(u32, u32, u32)>> {
    let mut out = Vec::new();
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0)?),
            None => (part, 1),
        };
        let (a, b) = if range == "*" || range == "?" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (value_of(a, names, base)?, value_of(b, names, base)?)
        } else {
            let a = value_of(range, names, base)?;
            (a, if part.contains('/') { hi } else { a })
        };
        if a < lo || b > hi {
            return None;
        }
        out.push((a, b, step));
    }
    Some(out)
}

fn bits(text: &str, lo: u32, hi: u32, names: &[&str], base: u32) -> Option<u64> {
    let mut bits = 0u64;
    for (a, b, step) in ranges(text, lo, hi, names, base)? {
        // `a > b` wraps around (`FRI-MON`, `22-2`).
        let seq: Vec<u32> = if a <= b { (a..=b).collect() } else { (a..=hi).chain(lo..=b).collect() };
        for v in seq.into_iter().step_by(step as usize) {
            bits |= 1 << v;
        }
    }
    Some(bits)
}

fn parse_quartz(expr: &str) -> Option<Quartz> {
    let f: Vec<&str> = expr.split_whitespace().collect();
    if f.len() < 6 || f.len() > 7 {
        return None;
    }
    let seconds = bits(f[0], 0, 59, &[], 0)?;
    let minutes = bits(f[1], 0, 59, &[], 0)?;
    let hours = bits(f[2], 0, 23, &[], 0)? as u32;
    let d = f[3].to_ascii_uppercase();
    let dom = if d == "?" || d == "*" {
        Dom::Any
    } else if d == "LW" {
        Dom::LastWeekday
    } else if let Some(rest) = d.strip_prefix('L') {
        Dom::Last(rest.strip_prefix('-').map_or(Some(0), |n| n.parse().ok())?)
    } else if let Some(n) = d.strip_suffix('W') {
        Dom::NearestWeekday(n.parse().ok().filter(|n| (1..=31).contains(n))?)
    } else {
        Dom::Set(bits(&d, 1, 31, &[], 0)?)
    };
    let months = bits(f[4], 1, 12, &MONTH_NAMES, 1)? as u16;
    let w = f[5].to_ascii_uppercase();
    let dow = if w == "?" || w == "*" {
        Dow::Any
    } else if let Some((n, k)) = w.split_once('#') {
        Dow::Nth(value_of(n, &DAY_NAMES, 1)?, k.parse().ok().filter(|k| (1..=5).contains(k))?)
    } else if w.len() > 1 && w.ends_with('L') {
        Dow::Last(value_of(&w[..w.len() - 1], &DAY_NAMES, 1)?)
    } else if w == "L" {
        Dow::Set(1 << 7)
    } else {
        Dow::Set(bits(&w, 1, 7, &DAY_NAMES, 1)? as u8)
    };
    let years = match f.get(6) {
        None => None,
        Some(y) => Some(
            ranges(y, 1970, 2199, &[], 0)?
                .into_iter()
                .map(|(a, b, s)| (a as i32, if y.contains('/') && !y.contains('-') { 2199 } else { b as i32 }, s as i32))
                .collect(),
        ),
    };
    Some(Quartz { seconds, minutes, hours, dom, months, dow, years })
}

fn days_in_month(y: i32, m: u32) -> u32 {
    let next = if m == 12 { NaiveDate::from_ymd_opt(y + 1, 1, 1) } else { NaiveDate::from_ymd_opt(y, m + 1, 1) };
    next.and_then(|n| n.pred_opt()).map_or(31, |d| d.day())
}

impl Quartz {
    fn year_ok(&self, y: i32) -> bool {
        self.years.as_ref().is_none_or(|ys| ys.iter().any(|&(a, b, s)| y >= a && y <= b && (y - a) % s == 0))
    }

    fn day_ok(&self, d: NaiveDate) -> bool {
        let last = days_in_month(d.year(), d.month());
        let dow1 = d.weekday().num_days_from_sunday() + 1;
        let dom_ok = match self.dom {
            Dom::Any => true,
            Dom::Set(b) => b & (1 << d.day()) != 0,
            Dom::Last(n) => last.checked_sub(n).is_some_and(|t| t == d.day()),
            Dom::LastWeekday => {
                let mut t = NaiveDate::from_ymd_opt(d.year(), d.month(), last).unwrap_or(d);
                while t.weekday().num_days_from_monday() >= 5 {
                    t = t.pred_opt().unwrap_or(t);
                }
                t == d
            }
            Dom::NearestWeekday(n) => {
                let n = n.min(last);
                let Some(t) = NaiveDate::from_ymd_opt(d.year(), d.month(), n) else { return false };
                let target = match t.weekday().num_days_from_monday() {
                    5 => if n == 1 { t + chrono::Duration::days(2) } else { t - chrono::Duration::days(1) },
                    6 => if n == last { t - chrono::Duration::days(2) } else { t + chrono::Duration::days(1) },
                    _ => t,
                };
                target == d
            }
        };
        let dow_ok = match self.dow {
            Dow::Any => true,
            Dow::Set(b) => b & (1 << dow1) != 0,
            Dow::Last(n) => dow1 == n && d.day() + 7 > last,
            Dow::Nth(n, k) => dow1 == n && (d.day() - 1) / 7 + 1 == k,
        };
        dom_ok && dow_ok
    }
}

/// Whether a Quartz cron expression parses.
pub fn valid_cron(expr: &str) -> bool {
    parse_quartz(expr).is_some()
}

/// The first time strictly after `after` (Unix ms) a Quartz cron expression fires in `zone`.
pub fn next_cron(expr: &str, zone: &Zone, after: i64) -> Option<i64> {
    let q = parse_quartz(expr)?;
    let utc = chrono::DateTime::from_timestamp_millis(after)?.naive_utc();
    let local = utc + chrono::Duration::seconds(zone.offset_at_utc(utc) as i64);
    // The next whole second after `after`.
    let first = local.with_nanosecond(0)? + chrono::Duration::seconds(1);
    let mut day = first.date();
    let end = NaiveDate::from_ymd_opt(2200, 1, 1)?;
    while day < end {
        if !q.year_ok(day.year()) {
            day = NaiveDate::from_ymd_opt(day.year() + 1, 1, 1)?;
            continue;
        }
        if q.months & (1 << day.month()) == 0 {
            day = if day.month() == 12 { NaiveDate::from_ymd_opt(day.year() + 1, 1, 1)? } else { NaiveDate::from_ymd_opt(day.year(), day.month() + 1, 1)? };
            continue;
        }
        if q.day_ok(day) {
            let from = if day == first.date() { (first.hour(), first.minute(), first.second()) } else { (0, 0, 0) };
            for h in from.0..24 {
                if q.hours & (1 << h) == 0 {
                    continue;
                }
                let m0 = if h == from.0 { from.1 } else { 0 };
                for m in m0..60 {
                    if q.minutes & (1 << m) == 0 {
                        continue;
                    }
                    let s0 = if h == from.0 && m == from.1 { from.2 } else { 0 };
                    if let Some(s) = (s0..60).find(|s| q.seconds & (1 << s) != 0) {
                        let at: NaiveDateTime = day.and_hms_opt(h, m, s)?;
                        let ms = (at - chrono::Duration::seconds(zone.offset_for_local(at) as i64)).and_utc().timestamp_millis();
                        if ms > after {
                            return Some(ms);
                        }
                    }
                }
            }
        }
        day = day.succ_opt()?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj;

    fn at(s: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp_millis()
    }

    #[test]
    fn quartz_next_times() {
        let utc = Zone::UTC;
        assert_eq!(next_cron("0 0/5 * * * ?", &utc, at("2026-01-01T00:03:00Z")), Some(at("2026-01-01T00:05:00Z")));
        assert_eq!(next_cron("0 0 12 ? * MON-FRI", &utc, at("2026-01-03T12:00:00Z")), Some(at("2026-01-05T12:00:00Z")));
        assert_eq!(next_cron("0 0 0 1 * ? 2099", &utc, at("2026-01-01T00:00:00Z")), Some(at("2099-01-01T00:00:00Z")));
        assert_eq!(next_cron("0 0 0 L * ?", &utc, at("2026-02-01T00:00:00Z")), Some(at("2026-02-28T00:00:00Z")));
        assert_eq!(next_cron("0 0 0 ? * 6#3", &utc, at("2026-01-01T00:00:00Z")), Some(at("2026-01-16T00:00:00Z")));
        assert_eq!(next_cron("0 0 0 ? * 6L", &utc, at("2026-01-01T00:00:00Z")), Some(at("2026-01-30T00:00:00Z")));
        assert_eq!(next_cron("0 0 0 15W * ?", &utc, at("2026-08-01T00:00:00Z")), Some(at("2026-08-14T00:00:00Z")));
        assert_eq!(next_cron("0 0 0 1 1 ? 2020", &utc, at("2026-01-01T00:00:00Z")), None);
        let berlin = Zone::parse("Europe/Berlin").unwrap();
        assert_eq!(next_cron("0 0 9 * * ?", &berlin, at("2026-07-01T00:00:00Z")), Some(at("2026-07-01T07:00:00Z")));
    }

    #[test]
    fn schedules() {
        let t = |s: Value| parse("w", Some(&obj! { "schedule" => s }));
        assert_eq!(t(obj! { "interval" => "10s" }).unwrap(), Schedule::Interval(10_000));
        assert_eq!(t(obj! { "interval" => 5 }).unwrap(), Schedule::Interval(5_000));
        let Schedule::Cron { exprs, .. } = t(obj! { "hourly" => obj! { "minute" => crate::arr![0, 5] } }).unwrap() else { panic!() };
        assert_eq!(exprs, vec!["0 0,5 * * * ?"]);
        let Schedule::Cron { exprs, .. } = t(obj! { "weekly" => obj! { "on" => "friday", "at" => "noon" } }).unwrap() else { panic!() };
        assert_eq!(exprs, vec!["0 0 12 ? * FRI"]);
        let e = t(obj! { "timezone" => "Pangea/Tethys", "hourly" => obj! {} }).unwrap_err();
        assert_eq!(e.reason(), "could not parse schedule. invalid timezone [Pangea/Tethys]");
        let s = Schedule::Interval(1000);
        assert_eq!(s.next_after(0, 0), Some(1000));
        assert_eq!(s.next_after(0, 2500), Some(3000));
    }
}
