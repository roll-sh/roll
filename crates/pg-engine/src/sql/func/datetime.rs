//! Date/time functions: now() and friends, extract/date_part, date_trunc, make_*, AT TIME ZONE,
//! date_bin, age, justify_*, overlaps (ports of timestamp.c / date.c), and to_char /
//! to_timestamp / to_date / to_number (formatting.c, in `formatting`).

use super::{FnCtx, Registry, float_arg, int_arg, text, text_arg};
use crate::error::{Error, Result};
use crate::types::datetime::{
    self as dt, DATE_NEG_INF, DATE_POS_INF, DTK_CENTURY, DTK_DAY, DTK_DECADE, DTK_DOW, DTK_DOY,
    DTK_EPOCH, DTK_HOUR, DTK_ISODOW, DTK_ISOYEAR, DTK_JULIAN, DTK_MICROSEC, DTK_MILLENNIUM,
    DTK_MILLISEC, DTK_MINUTE, DTK_MONTH, DTK_QUARTER, DTK_SECOND, DTK_TZ, DTK_TZ_HOUR,
    DTK_TZ_MINUTE, DTK_WEEK, DTK_YEAR, POSTGRES_EPOCH_JDATE, RESERV, TS_NEG_INF, TS_POS_INF,
    TimeZone, Tm, TzName, UNITS, UNIX_EPOCH_JDATE, UNIX_EPOCH_OFFSET_SECS, UNKNOWN_FIELD, date2j,
    j2date, j2day, split_ts, timestamp2tm, tm2timestamp, ts_is_finite,
};
use crate::types::numeric::Numeric;
use crate::types::value::{
    Interval, USECS_PER_DAY, USECS_PER_HOUR, USECS_PER_MINUTE, USECS_PER_SEC,
};
use crate::types::{DataType as T, Value};
use crate::{bail, err};
use num_bigint::BigInt;

/// A zone name as AT TIME ZONE / date_trunc / generate_series take it (DecodeTimezoneNameToTz).
pub fn zone(name: &str, c: &FnCtx) -> Result<TimeZone> {
    dt::decode_timezone_name_to_tz(name, &c.exec.fmt().dt_ctx())
}

fn ts_of(v: &Value) -> i64 {
    match v {
        Value::Timestamp(t) | Value::Timestamptz(t) => *t,
        Value::Date(d) => date_to_ts(*d),
        _ => 0,
    }
}

pub fn date_to_ts(d: i32) -> i64 {
    match d {
        DATE_NEG_INF => TS_NEG_INF,
        DATE_POS_INF => TS_POS_INF,
        d => d as i64 * USECS_PER_DAY,
    }
}

/// UTC instant → local wall-clock microseconds in `tz` (timestamptz2timestamp).
pub fn to_local(ts: i64, tz: &TimeZone) -> i64 {
    if !ts_is_finite(ts) {
        return ts;
    }
    ts + tz.offset_at_utc(ts) as i64 * USECS_PER_SEC
}

/// Local wall-clock microseconds in `tz` → UTC instant (timestamp2timestamptz).
pub fn from_local(local: i64, tz: &TimeZone) -> i64 {
    if !ts_is_finite(local) {
        return local;
    }
    local - tz.offset_for_local(local) as i64 * USECS_PER_SEC
}

fn ts_overflow() -> Error {
    err!(DATETIME_FIELD_OVERFLOW, "timestamp out of range")
}

/// downcase_truncate_identifier for unit names.
fn lowunits(u: &str) -> String {
    let mut s: String = u
        .chars()
        .map(|c| if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c })
        .collect();
    if s.len() > 63 {
        let mut n = 63;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
    s
}

// ── current time ──

fn now(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamptz(c.exec.txn_time()))
}
fn statement_timestamp(_: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamptz(c.exec.stmt_time()))
}
fn clock_timestamp(_: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamptz(dt::Clock::system().now))
}
fn timeofday(_: &[Value], c: &FnCtx) -> Result<Value> {
    let ts = dt::Clock::system().now;
    let tz = &c.exec.fmt().tz;
    let Some(t) = tz.localtime(ts) else {
        return Ok(text(String::new()));
    };
    Ok(text(format!(
        "{} {} {:02} {:02}:{:02}:{:02}.{:06} {} {}",
        &dt::DAYS[t.wday as usize][..3],
        dt::MONTHS[(t.mon - 1) as usize],
        t.mday,
        t.hour,
        t.min,
        t.sec,
        ts.rem_euclid(USECS_PER_SEC),
        t.year,
        t.zone
    )))
}
fn current_date(_: &[Value], c: &FnCtx) -> Result<Value> {
    let local = to_local(c.exec.txn_time(), &c.exec.fmt().tz);
    Ok(Value::Date(split_ts(local).0 as i32))
}
fn current_time(a: &[Value], c: &FnCtx) -> Result<Value> {
    let ts = c.exec.txn_time();
    let tz = &c.exec.fmt().tz;
    let off = tz.offset_at_utc(ts);
    let mut us = split_ts(ts + off as i64 * USECS_PER_SEC).1;
    if let Some(p) = a.first() {
        us = dt::round_micros(us, int_arg(p) as i32);
    }
    Ok(Value::Timetz(us, -off))
}
fn localtime(a: &[Value], c: &FnCtx) -> Result<Value> {
    let mut us = split_ts(to_local(c.exec.txn_time(), &c.exec.fmt().tz)).1;
    if let Some(p) = a.first() {
        us = dt::round_micros(us, int_arg(p) as i32);
    }
    Ok(Value::Time(us))
}
fn localtimestamp(a: &[Value], c: &FnCtx) -> Result<Value> {
    let mut ts = to_local(c.exec.txn_time(), &c.exec.fmt().tz);
    if let Some(p) = a.first() {
        ts = dt::round_micros(ts, int_arg(p) as i32);
    }
    Ok(Value::Timestamp(ts))
}
fn current_timestamp_p(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamptz(dt::round_micros(
        c.exec.txn_time(),
        int_arg(&a[0]) as i32,
    )))
}

// ── extract / date_part ──

/// int64_div_fast_to_numeric: `v / 10^scale` with that display scale.
fn num_scaled(v: i128, scale: u32) -> Value {
    Value::numeric(Numeric::from_parts(BigInt::from(v), scale))
}

fn num_int(v: i64) -> Value {
    num_scaled(v as i128, 0)
}

fn not_supported(lowunits: &str, ty: &str) -> Error {
    err!(
        FEATURE_NOT_SUPPORTED,
        "unit \"{lowunits}\" not supported for type {ty}"
    )
}

fn not_recognized(lowunits: &str, ty: &str) -> Error {
    err!(
        INVALID_PARAMETER_VALUE,
        "unit \"{lowunits}\" not recognized for type {ty}"
    )
}

/// DecodeUnits, then DecodeSpecial (as the *_part functions do).
fn part_units(l: &str) -> (i32, i32) {
    let (t, v) = dt::decode_units(l.as_bytes());
    if t == UNKNOWN_FIELD {
        dt::decode_special(l.as_bytes())
    } else {
        (t, v)
    }
}

fn inf_value(neg: bool, numeric: bool) -> Value {
    if numeric {
        Value::numeric(if neg { Numeric::NegInf } else { Numeric::PosInf })
    } else {
        Value::Float8(if neg {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        })
    }
}

/// NonFiniteTimestampTzPart.
fn nonfinite_ts_part(ty: i32, val: i32, l: &str, neg: bool, tyname: &str, numeric: bool) -> Result<Value> {
    if ty != UNITS && ty != RESERV {
        return Err(not_recognized(l, tyname));
    }
    match val {
        DTK_MICROSEC | DTK_MILLISEC | DTK_SECOND | DTK_MINUTE | DTK_HOUR | DTK_DAY | DTK_MONTH
        | DTK_QUARTER | DTK_WEEK | DTK_DOW | DTK_ISODOW | DTK_DOY | DTK_TZ | DTK_TZ_MINUTE
        | DTK_TZ_HOUR => Ok(Value::Null),
        DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM | DTK_JULIAN | DTK_ISOYEAR
        | DTK_EPOCH => Ok(inf_value(neg, numeric)),
        _ => Err(not_supported(l, tyname)),
    }
}

fn year_field(val: i32, y: i32) -> i64 {
    let y = y as i64;
    match val {
        DTK_YEAR => {
            if y > 0 {
                y
            } else {
                y - 1
            }
        }
        DTK_DECADE => {
            if y >= 0 {
                y / 10
            } else {
                -((8 - (y - 1)) / 10)
            }
        }
        DTK_CENTURY => {
            if y > 0 {
                (y + 99) / 100
            } else {
                -((99 - (y - 1)) / 100)
            }
        }
        _ => {
            if y > 0 {
                (y + 999) / 1000
            } else {
                -((999 - (y - 1)) / 1000)
            }
        }
    }
}

/// timestamp_part_common / timestamptz_part_common (`tz` for timestamptz).
fn timestamp_part(
    units: &str,
    ts: i64,
    tz: Option<&TimeZone>,
    numeric: bool,
) -> Result<Value> {
    let tyname = if tz.is_some() {
        "timestamp with time zone"
    } else {
        "timestamp without time zone"
    };
    let l = lowunits(units);
    let (ty, val) = part_units(&l);
    if !ts_is_finite(ts) {
        return nonfinite_ts_part(ty, val, &l, ts == TS_NEG_INF, tyname, numeric);
    }
    let int = |v: i64| -> Value {
        if numeric {
            num_int(v)
        } else {
            Value::Float8(v as f64)
        }
    };
    if ty == UNITS {
        let (tm, fsec) = timestamp2tm(ts, tz).ok_or_else(ts_overflow)?;
        let tzw = -tm.gmtoff;
        let secus = tm.sec as i64 * 1_000_000 + fsec as i64;
        return Ok(match val {
            DTK_TZ if tz.is_some() => int(-tzw),
            DTK_TZ_MINUTE if tz.is_some() => int((-tzw / 60) % 60),
            DTK_TZ_HOUR if tz.is_some() => int(-tzw / 3600),
            DTK_MICROSEC => int(secus),
            DTK_MILLISEC => {
                if numeric {
                    num_scaled(secus as i128, 3)
                } else {
                    Value::Float8(tm.sec as f64 * 1000.0 + fsec as f64 / 1000.0)
                }
            }
            DTK_SECOND => {
                if numeric {
                    num_scaled(secus as i128, 6)
                } else {
                    Value::Float8(tm.sec as f64 + fsec as f64 / 1_000_000.0)
                }
            }
            DTK_MINUTE => int(tm.min as i64),
            DTK_HOUR => int(tm.hour as i64),
            DTK_DAY => int(tm.mday as i64),
            DTK_MONTH => int(tm.mon as i64),
            DTK_QUARTER => int(((tm.mon - 1) / 3 + 1) as i64),
            DTK_WEEK => int(dt::date2isoweek(tm.year, tm.mon, tm.mday) as i64),
            DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM => int(year_field(val, tm.year)),
            DTK_JULIAN => {
                let j = date2j(tm.year, tm.mon, tm.mday);
                let tus = ((tm.hour as i64 * 60 + tm.min as i64) * 60 + tm.sec as i64) * 1_000_000
                    + fsec as i64;
                if numeric {
                    let frac = Numeric::from_i64(tus).div(&Numeric::from_i64(86_400_000_000))?;
                    Value::numeric(Numeric::from_i64(j as i64).add(&frac))
                } else {
                    Value::Float8(
                        j as f64
                            + ((((tm.hour * 60) + tm.min) * 60) as f64
                                + tm.sec as f64
                                + fsec as f64 / 1_000_000.0)
                                / 86400.0,
                    )
                }
            }
            DTK_ISOYEAR => {
                let mut r = dt::date2isoyear(tm.year, tm.mon, tm.mday) as i64;
                if r <= 0 {
                    r -= 1;
                }
                int(r)
            }
            DTK_DOW | DTK_ISODOW => {
                let mut r = j2day(date2j(tm.year, tm.mon, tm.mday)) as i64;
                if val == DTK_ISODOW && r == 0 {
                    r = 7;
                }
                int(r)
            }
            DTK_DOY => {
                int((date2j(tm.year, tm.mon, tm.mday) - date2j(tm.year, 1, 1) + 1) as i64)
            }
            _ => return Err(not_supported(&l, tyname)),
        });
    }
    if ty == RESERV {
        if val == DTK_EPOCH {
            let epoch = -UNIX_EPOCH_OFFSET_SECS * USECS_PER_SEC;
            return Ok(if numeric {
                num_scaled(ts as i128 - epoch as i128, 6)
            } else if ts < i64::MAX + epoch {
                Value::Float8((ts - epoch) as f64 / 1_000_000.0)
            } else {
                Value::Float8((ts as f64 - epoch as f64) / 1_000_000.0)
            });
        }
        return Err(not_supported(&l, tyname));
    }
    Err(not_recognized(&l, tyname))
}

/// extract_date.
fn date_part_extract(units: &str, date: i32) -> Result<Value> {
    let l = lowunits(units);
    let (ty, val) = part_units(&l);
    if !dt::date_is_finite(date) && (ty == UNITS || ty == RESERV) {
        return match val {
            DTK_DAY | DTK_MONTH | DTK_QUARTER | DTK_WEEK | DTK_DOW | DTK_ISODOW | DTK_DOY => {
                Ok(Value::Null)
            }
            DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM | DTK_JULIAN | DTK_ISOYEAR
            | DTK_EPOCH => Ok(inf_value(date == DATE_NEG_INF, true)),
            _ => Err(not_supported(&l, "date")),
        };
    }
    if ty == UNITS {
        let (year, mon, mday) = j2date(date + POSTGRES_EPOCH_JDATE);
        let r: i64 = match val {
            DTK_DAY => mday as i64,
            DTK_MONTH => mon as i64,
            DTK_QUARTER => ((mon - 1) / 3 + 1) as i64,
            DTK_WEEK => dt::date2isoweek(year, mon, mday) as i64,
            DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM => year_field(val, year),
            DTK_JULIAN => (date + POSTGRES_EPOCH_JDATE) as i64,
            DTK_ISOYEAR => {
                let mut r = dt::date2isoyear(year, mon, mday) as i64;
                if r <= 0 {
                    r -= 1;
                }
                r
            }
            DTK_DOW | DTK_ISODOW => {
                let mut r = j2day(date + POSTGRES_EPOCH_JDATE) as i64;
                if val == DTK_ISODOW && r == 0 {
                    r = 7;
                }
                r
            }
            DTK_DOY => (date2j(year, mon, mday) - date2j(year, 1, 1) + 1) as i64,
            _ => return Err(not_supported(&l, "date")),
        };
        return Ok(num_int(r));
    }
    if ty == RESERV {
        if val == DTK_EPOCH {
            return Ok(num_int(
                (date as i64 + POSTGRES_EPOCH_JDATE as i64 - UNIX_EPOCH_JDATE as i64) * 86400,
            ));
        }
        return Err(not_supported(&l, "date"));
    }
    Err(not_recognized(&l, "date"))
}

/// time_part_common / timetz_part_common (`west` for timetz).
fn time_part(units: &str, time: i64, west: Option<i32>, numeric: bool) -> Result<Value> {
    let tyname = if west.is_some() {
        "time with time zone"
    } else {
        "time without time zone"
    };
    let l = lowunits(units);
    let (ty, val) = part_units(&l);
    let int = |v: i64| -> Value {
        if numeric {
            num_int(v)
        } else {
            Value::Float8(v as f64)
        }
    };
    if ty == UNITS {
        let (hour, min, sec, fsec) = dt::dt2time(time);
        let secus = sec as i64 * 1_000_000 + fsec as i64;
        return Ok(match val {
            DTK_TZ if west.is_some() => int(-(west.unwrap_or(0) as i64)),
            DTK_TZ_MINUTE if west.is_some() => int((-(west.unwrap_or(0) as i64) / 60) % 60),
            DTK_TZ_HOUR if west.is_some() => int(-(west.unwrap_or(0) as i64) / 3600),
            DTK_MICROSEC => int(secus),
            DTK_MILLISEC => {
                if numeric {
                    num_scaled(secus as i128, 3)
                } else {
                    Value::Float8(sec as f64 * 1000.0 + fsec as f64 / 1000.0)
                }
            }
            DTK_SECOND => {
                if numeric {
                    num_scaled(secus as i128, 6)
                } else {
                    Value::Float8(sec as f64 + fsec as f64 / 1_000_000.0)
                }
            }
            DTK_MINUTE => int(min as i64),
            DTK_HOUR => int(hour as i64),
            _ => return Err(not_supported(&l, tyname)),
        });
    }
    if ty == RESERV && val == DTK_EPOCH {
        let zone = west.unwrap_or(0) as i64;
        return Ok(if numeric {
            num_scaled(time as i128 + zone as i128 * 1_000_000, 6)
        } else {
            Value::Float8(time as f64 / 1_000_000.0 + zone as f64)
        });
    }
    Err(not_recognized(&l, tyname))
}

/// interval_part_common.
fn interval_part(units: &str, iv: &Interval, numeric: bool) -> Result<Value> {
    let l = lowunits(units);
    let (ty, val) = part_units(&l);
    if iv.is_infinite() {
        if ty != UNITS && ty != RESERV {
            return Err(not_recognized(&l, "interval"));
        }
        return match val {
            DTK_MICROSEC | DTK_MILLISEC | DTK_SECOND | DTK_MINUTE | DTK_WEEK | DTK_MONTH
            | DTK_QUARTER => Ok(Value::Null),
            DTK_HOUR | DTK_DAY | DTK_YEAR | DTK_DECADE | DTK_CENTURY | DTK_MILLENNIUM
            | DTK_EPOCH => Ok(inf_value(iv.is_neg_inf(), numeric)),
            _ => Err(not_supported(&l, "interval")),
        };
    }
    let int = |v: i64| -> Value {
        if numeric {
            num_int(v)
        } else {
            Value::Float8(v as f64)
        }
    };
    if ty == UNITS {
        let tm = dt::interval2itm(iv);
        let secus = tm.sec as i64 * 1_000_000 + tm.usec as i64;
        return Ok(match val {
            DTK_MICROSEC => int(secus),
            DTK_MILLISEC => {
                if numeric {
                    num_scaled(secus as i128, 3)
                } else {
                    Value::Float8(tm.sec as f64 * 1000.0 + tm.usec as f64 / 1000.0)
                }
            }
            DTK_SECOND => {
                if numeric {
                    num_scaled(secus as i128, 6)
                } else {
                    Value::Float8(tm.sec as f64 + tm.usec as f64 / 1_000_000.0)
                }
            }
            DTK_MINUTE => int(tm.min as i64),
            DTK_HOUR => int(tm.hour),
            DTK_DAY => int(tm.mday as i64),
            DTK_WEEK => int((tm.mday / 7) as i64),
            DTK_MONTH => int(tm.mon as i64),
            DTK_QUARTER => int(if iv.months >= 0 {
                (tm.mon / 3 + 1) as i64
            } else {
                -(((-(iv.months as i64) % 12) / 3) + 1)
            }),
            DTK_YEAR => int(tm.year as i64),
            DTK_DECADE => int((tm.year / 10) as i64),
            DTK_CENTURY => int((tm.year / 100) as i64),
            DTK_MILLENNIUM => int((tm.year / 1000) as i64),
            _ => return Err(not_supported(&l, "interval")),
        });
    }
    if ty == RESERV && val == DTK_EPOCH {
        if numeric {
            // ((4 * DAYS_PER_YEAR) * years + (4 * DAYS_PER_MONTH) * months + 4 * days)
            // * (SECS_PER_DAY / 4), in integers.
            let secs_from_day_month: i128 = (1461i128 * (iv.months / 12) as i128
                + 120i128 * (iv.months % 12) as i128
                + 4i128 * iv.days as i128)
                * (86400 / 4);
            return Ok(num_scaled(
                secs_from_day_month * 1_000_000 + iv.micros as i128,
                6,
            ));
        }
        let mut r = iv.micros as f64 / 1_000_000.0;
        r += (365.25 * 86400.0) * (iv.months / 12) as f64;
        r += (30.0 * 86400.0) * (iv.months % 12) as f64;
        r += 86400.0 * iv.days as f64;
        return Ok(Value::Float8(r));
    }
    Err(not_recognized(&l, "interval"))
}

pub fn extract_value(unit_raw: &str, v: &Value, c: &FnCtx) -> Result<Value> {
    part_value(unit_raw, v, c, true)
}

fn part_value(unit: &str, v: &Value, c: &FnCtx, numeric: bool) -> Result<Value> {
    match v {
        Value::Timestamp(ts) => timestamp_part(unit, *ts, None, numeric),
        Value::Timestamptz(ts) => timestamp_part(unit, *ts, Some(&c.exec.fmt().tz), numeric),
        Value::Date(d) => {
            if numeric {
                date_part_extract(unit, *d)
            } else {
                timestamp_part(unit, date_to_ts(*d), None, false)
            }
        }
        Value::Time(us) => time_part(unit, *us, None, numeric),
        Value::Timetz(us, w) => time_part(unit, *us, Some(*w), numeric),
        Value::Interval(iv) => interval_part(unit, iv, numeric),
        Value::Null => Ok(Value::Null),
        _ => Err(err!(DATATYPE_MISMATCH, "cannot extract from this type")),
    }
}

fn extract(a: &[Value], c: &FnCtx) -> Result<Value> {
    part_value(text_arg(&a[0]), &a[1], c, true)
}

fn date_part(a: &[Value], c: &FnCtx) -> Result<Value> {
    part_value(text_arg(&a[0]), &a[1], c, false)
}

// ── date_trunc ──

/// Truncate broken-down time to `val` (the shared switch of timestamp_trunc and
/// timestamptz_trunc_internal). Returns whether the zone must be recomputed.
fn trunc_tm(val: i32, tm: &mut Tm, fsec: &mut i32) -> Option<bool> {
    let mut redotz = false;
    let mut v = val;
    loop {
        match v {
            DTK_WEEK => {
                let woy = dt::date2isoweek(tm.year, tm.mon, tm.mday);
                if woy >= 52 && tm.mon == 1 {
                    tm.year -= 1;
                }
                if woy <= 1 && tm.mon == 12 {
                    tm.year += 1;
                }
                (tm.year, tm.mon, tm.mday) = dt::isoweek2date(woy, tm.year);
                tm.hour = 0;
                tm.min = 0;
                tm.sec = 0;
                *fsec = 0;
                return Some(true);
            }
            DTK_MILLENNIUM => {
                tm.year = if tm.year > 0 {
                    ((tm.year + 999) / 1000) * 1000 - 999
                } else {
                    -((999 - (tm.year - 1)) / 1000) * 1000 + 1
                };
                v = DTK_CENTURY;
            }
            DTK_CENTURY => {
                tm.year = if tm.year > 0 {
                    ((tm.year + 99) / 100) * 100 - 99
                } else {
                    -((99 - (tm.year - 1)) / 100) * 100 + 1
                };
                v = DTK_DECADE;
            }
            DTK_DECADE => {
                if val != DTK_MILLENNIUM && val != DTK_CENTURY {
                    tm.year = if tm.year > 0 {
                        (tm.year / 10) * 10
                    } else {
                        -((8 - (tm.year - 1)) / 10) * 10
                    };
                }
                v = DTK_YEAR;
            }
            DTK_YEAR => {
                tm.mon = 1;
                v = DTK_QUARTER;
            }
            DTK_QUARTER => {
                tm.mon = (3 * ((tm.mon - 1) / 3)) + 1;
                v = DTK_MONTH;
            }
            DTK_MONTH => {
                tm.mday = 1;
                v = DTK_DAY;
            }
            DTK_DAY => {
                tm.hour = 0;
                redotz = true;
                v = DTK_HOUR;
            }
            DTK_HOUR => {
                tm.min = 0;
                v = DTK_MINUTE;
            }
            DTK_MINUTE => {
                tm.sec = 0;
                v = DTK_SECOND;
            }
            DTK_SECOND => {
                *fsec = 0;
                return Some(redotz);
            }
            DTK_MILLISEC => {
                *fsec = (*fsec / 1000) * 1000;
                return Some(redotz);
            }
            DTK_MICROSEC => return Some(redotz),
            _ => return None,
        }
    }
}

const TRUNC_UNITS: [i32; 13] = [
    DTK_WEEK,
    DTK_MILLENNIUM,
    DTK_CENTURY,
    DTK_DECADE,
    DTK_YEAR,
    DTK_QUARTER,
    DTK_MONTH,
    DTK_DAY,
    DTK_HOUR,
    DTK_MINUTE,
    DTK_SECOND,
    DTK_MILLISEC,
    DTK_MICROSEC,
];

/// timestamp_trunc / timestamptz_trunc_internal (`tz` for timestamptz).
fn timestamp_trunc(units: &str, ts: i64, tz: Option<&TimeZone>) -> Result<i64> {
    let tyname = if tz.is_some() {
        "timestamp with time zone"
    } else {
        "timestamp without time zone"
    };
    let l = lowunits(units);
    let (ty, val) = dt::decode_units(l.as_bytes());
    if ty != UNITS {
        return Err(not_recognized(&l, tyname));
    }
    if !ts_is_finite(ts) {
        if TRUNC_UNITS.contains(&val) {
            return Ok(ts);
        }
        return Err(not_supported(&l, tyname));
    }
    let (mut tm, mut fsec) = timestamp2tm(ts, tz).ok_or_else(ts_overflow)?;
    let redotz = trunc_tm(val, &mut tm, &mut fsec).ok_or_else(|| not_supported(&l, tyname))?;
    match tz {
        None => tm2timestamp(&tm, fsec, None).ok_or_else(ts_overflow),
        Some(z) => {
            let off = if redotz {
                dt::determine_tz_offset(&tm, z).0
            } else {
                -tm.gmtoff as i32
            };
            tm2timestamp(&tm, fsec, Some(off)).ok_or_else(ts_overflow)
        }
    }
}

/// interval_trunc.
fn interval_trunc(units: &str, iv: &Interval) -> Result<Interval> {
    let l = lowunits(units);
    let (ty, val) = dt::decode_units(l.as_bytes());
    if ty != UNITS {
        return Err(not_recognized(&l, "interval"));
    }
    let unsupported = || {
        let e = not_supported(&l, "interval");
        if val == DTK_WEEK {
            e.detail("Months usually have fractional weeks.")
        } else {
            e
        }
    };
    if iv.is_infinite() {
        return match val {
            DTK_MILLENNIUM | DTK_CENTURY | DTK_DECADE | DTK_YEAR | DTK_QUARTER | DTK_MONTH
            | DTK_DAY | DTK_HOUR | DTK_MINUTE | DTK_SECOND | DTK_MILLISEC | DTK_MICROSEC => {
                Ok(*iv)
            }
            _ => Err(unsupported()),
        };
    }
    let mut tm = dt::interval2itm(iv);
    let mut v = val;
    loop {
        match v {
            DTK_MILLENNIUM => {
                tm.year = (tm.year / 1000) * 1000;
                v = DTK_CENTURY;
            }
            DTK_CENTURY => {
                tm.year = (tm.year / 100) * 100;
                v = DTK_DECADE;
            }
            DTK_DECADE => {
                tm.year = (tm.year / 10) * 10;
                v = DTK_YEAR;
            }
            DTK_YEAR => {
                tm.mon = 0;
                v = DTK_QUARTER;
            }
            DTK_QUARTER => {
                tm.mon = 3 * (tm.mon / 3);
                v = DTK_MONTH;
            }
            DTK_MONTH => {
                tm.mday = 0;
                v = DTK_DAY;
            }
            DTK_DAY => {
                tm.hour = 0;
                v = DTK_HOUR;
            }
            DTK_HOUR => {
                tm.min = 0;
                v = DTK_MINUTE;
            }
            DTK_MINUTE => {
                tm.sec = 0;
                v = DTK_SECOND;
            }
            DTK_SECOND => {
                tm.usec = 0;
                break;
            }
            DTK_MILLISEC => {
                tm.usec = (tm.usec / 1000) * 1000;
                break;
            }
            DTK_MICROSEC => break,
            _ => return Err(unsupported()),
        }
    }
    dt::itm2interval(&tm).ok_or_else(dt::interval_overflow)
}

fn date_trunc(a: &[Value], c: &FnCtx) -> Result<Value> {
    let units = text_arg(&a[0]);
    match &a[1] {
        Value::Timestamp(ts) => Ok(Value::Timestamp(timestamp_trunc(units, *ts, None)?)),
        Value::Timestamptz(ts) => {
            let tz = match a.get(2) {
                Some(z) => zone(text_arg(z), c)?,
                None => c.exec.fmt().tz.clone(),
            };
            Ok(Value::Timestamptz(timestamp_trunc(units, *ts, Some(&tz))?))
        }
        Value::Interval(iv) => Ok(Value::Interval(interval_trunc(units, iv)?)),
        Value::Date(d) => {
            let tz = c.exec.fmt().tz.clone();
            let ts = from_local(date_to_ts(*d), &tz);
            Ok(Value::Timestamptz(timestamp_trunc(units, ts, Some(&tz))?))
        }
        _ => Ok(Value::Null),
    }
}

/// timestamp_bin / timestamptz_bin.
fn date_bin(a: &[Value], _: &FnCtx) -> Result<Value> {
    let Value::Interval(stride) = &a[0] else {
        return Ok(Value::Null);
    };
    let ts = ts_of(&a[1]);
    let origin = ts_of(&a[2]);
    let wrap = |r: i64| match &a[1] {
        Value::Timestamptz(_) => Value::Timestamptz(r),
        _ => Value::Timestamp(r),
    };
    if !ts_is_finite(ts) {
        return Ok(wrap(ts));
    }
    if !ts_is_finite(origin) {
        bail!(DATETIME_FIELD_OVERFLOW, "origin out of range");
    }
    if stride.is_infinite() {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "timestamps cannot be binned into infinite intervals"
        );
    }
    if stride.months != 0 {
        bail!(
            FEATURE_NOT_SUPPORTED,
            "timestamps cannot be binned into intervals containing months or years"
        );
    }
    let s = (stride.days as i64)
        .checked_mul(USECS_PER_DAY)
        .and_then(|d| d.checked_add(stride.micros))
        .ok_or_else(dt::interval_overflow)?;
    if s <= 0 {
        bail!(DATETIME_FIELD_OVERFLOW, "stride must be greater than zero");
    }
    let diff = ts.checked_sub(origin).ok_or_else(dt::interval_overflow)?;
    let modulo = diff % s;
    let mut r = origin + (diff - modulo);
    if modulo < 0 {
        r = r
            .checked_sub(s)
            .filter(|r| dt::ts_is_valid(*r))
            .ok_or_else(ts_overflow)?;
    }
    Ok(wrap(r))
}

// ── constructors ──

/// C's `%g` (and `%0Ng`) for error messages.
pub fn fmt_g(v: f64, width: usize, zero_pad: bool) -> String {
    let s = if v.is_nan() {
        "nan".to_string()
    } else if v.is_infinite() {
        if v > 0.0 { "inf".into() } else { "-inf".into() }
    } else if v == 0.0 {
        if v.is_sign_negative() { "-0".into() } else { "0".into() }
    } else {
        let p = 6i32;
        let exp = format!("{:.*e}", (p - 1) as usize, v);
        let (mant, e) = exp.split_once('e').unwrap_or((&exp, "0"));
        let x: i32 = e.parse().unwrap_or(0);
        if x < -4 || x >= p {
            let mant = if mant.contains('.') {
                mant.trim_end_matches('0').trim_end_matches('.')
            } else {
                mant
            };
            format!("{mant}e{}{:02}", if x < 0 { '-' } else { '+' }, x.abs())
        } else {
            let f = format!("{:.*}", (p - 1 - x).max(0) as usize, v);
            if f.contains('.') {
                f.trim_end_matches('0').trim_end_matches('.').to_string()
            } else {
                f
            }
        }
    };
    if s.len() >= width {
        return s;
    }
    let pad = width - s.len();
    if zero_pad && !v.is_nan() && !v.is_infinite() {
        if let Some(rest) = s.strip_prefix('-') {
            return format!("-{}{rest}", "0".repeat(pad));
        }
        format!("{}{s}", "0".repeat(pad))
    } else {
        format!("{}{s}", " ".repeat(pad))
    }
}

/// make_date.
fn make_date(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (y, m, d) = (int_arg(&a[0]) as i32, int_arg(&a[1]) as i32, int_arg(&a[2]) as i32);
    let mut tm = Tm {
        year: y,
        mon: m,
        mday: d,
        ..Default::default()
    };
    let mut bc = false;
    if tm.year < 0 {
        bc = true;
        tm.year = tm.year.checked_neg().ok_or_else(|| {
            err!(
                DATETIME_FIELD_OVERFLOW,
                "date field value out of range: {y}-{m:02}-{d:02}"
            )
        })?;
    }
    if dt::validate_date(dt::DTK_DATE_M, false, false, bc, &mut tm).is_err() {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "date field value out of range: {y}-{m:02}-{d:02}"
        );
    }
    if !dt::is_valid_julian(tm.year, tm.mon, tm.mday) {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "date out of range: {y}-{m:02}-{d:02}"
        );
    }
    let date = date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE;
    if !dt::is_valid_date(date) {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "date out of range: {y}-{m:02}-{d:02}"
        );
    }
    Ok(Value::Date(date))
}

/// make_time.
fn make_time(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (h, m, s) = (int_arg(&a[0]) as i32, int_arg(&a[1]) as i32, float_arg(&a[2]));
    if dt::float_time_overflows(h, m, s) {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "time field value out of range: {h}:{m:02}:{}",
            fmt_g(s, 2, true)
        );
    }
    Ok(Value::Time(
        ((h as i64 * 60 + m as i64) * 60) * USECS_PER_SEC
            + (s * USECS_PER_SEC as f64).round_ties_even() as i64,
    ))
}

/// make_timestamp_internal.
fn make_timestamp_internal(a: &[Value]) -> Result<i64> {
    let (year, month, day) = (int_arg(&a[0]) as i32, int_arg(&a[1]) as i32, int_arg(&a[2]) as i32);
    let (hour, min, sec) = (int_arg(&a[3]) as i32, int_arg(&a[4]) as i32, float_arg(&a[5]));
    let mut tm = Tm {
        year,
        mon: month,
        mday: day,
        ..Default::default()
    };
    let mut bc = false;
    if tm.year < 0 {
        bc = true;
        tm.year = tm.year.wrapping_neg();
    }
    if dt::validate_date(dt::DTK_DATE_M, false, false, bc, &mut tm).is_err() {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "date field value out of range: {year}-{month:02}-{day:02}"
        );
    }
    if !dt::is_valid_julian(tm.year, tm.mon, tm.mday) {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "date out of range: {year}-{month:02}-{day:02}"
        );
    }
    let date = (date2j(tm.year, tm.mon, tm.mday) - POSTGRES_EPOCH_JDATE) as i64;
    if dt::float_time_overflows(hour, min, sec) {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "time field value out of range: {hour}:{min:02}:{}",
            fmt_g(sec, 2, true)
        );
    }
    let time = ((hour as i64 * 60 + min as i64) * 60) * USECS_PER_SEC
        + (sec * USECS_PER_SEC as f64).round_ties_even() as i64;
    let out = || {
        err!(
            DATETIME_FIELD_OVERFLOW,
            "timestamp out of range: {year}-{month:02}-{day:02} {hour}:{min:02}:{}",
            fmt_g(sec, 2, true)
        )
    };
    let r = date
        .checked_mul(USECS_PER_DAY)
        .and_then(|d| d.checked_add(time))
        .ok_or_else(out)?;
    if !dt::ts_is_valid(r) {
        return Err(out());
    }
    Ok(r)
}

fn make_timestamp(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamp(make_timestamp_internal(a)?))
}

/// timestamp2timestamptz in a zone.
fn timestamp2timestamptz(ts: i64, tz: &TimeZone) -> Result<i64> {
    if !ts_is_finite(ts) {
        return Ok(ts);
    }
    let (tm, _) = timestamp2tm(ts, None).ok_or_else(ts_overflow)?;
    let (off, _) = dt::determine_tz_offset(&tm, tz);
    let r = ts + off as i64 * USECS_PER_SEC;
    if !dt::ts_is_valid(r) {
        return Err(ts_overflow());
    }
    Ok(r)
}

/// parse_sane_timezone: the zone's offset (seconds west) at local `tm`.
fn parse_sane_timezone(tm: &Tm, zone_name: &str, c: &FnCtx) -> Result<i32> {
    if zone_name.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
        return Err(err!(
            INVALID_PARAMETER_VALUE,
            "invalid input syntax for type numeric time zone: \"{zone_name}\""
        )
        .hint("Numeric time zones must have \"-\" or \"+\" as first character."));
    }
    match dt::decode_timezone(zone_name.as_bytes()) {
        Ok(tz) => Ok(tz),
        Err(dt::DtErr::TzdispOverflow) => Err(err!(
            INVALID_PARAMETER_VALUE,
            "numeric time zone \"{zone_name}\" out of range"
        )),
        Err(dt::DtErr::BadFormat) => {
            match dt::decode_timezone_name(zone_name, &c.exec.fmt().dt_ctx())? {
                TzName::Fixed(off) => Ok(-off),
                TzName::Dynamic(z) => Ok(dt::determine_tz_abbrev_offset(tm, zone_name.as_bytes(), &z).0),
                TzName::Zone(z) => Ok(dt::determine_tz_offset(tm, &z).0),
            }
        }
        Err(_) => Err(err!(
            INVALID_PARAMETER_VALUE,
            "time zone \"{zone_name}\" not recognized"
        )),
    }
}

fn make_timestamptz(a: &[Value], c: &FnCtx) -> Result<Value> {
    let ts = make_timestamp_internal(a)?;
    match a.get(6) {
        None => Ok(Value::Timestamptz(timestamp2timestamptz(
            ts,
            &c.exec.fmt().tz,
        )?)),
        Some(z) => {
            let (tm, _) = timestamp2tm(ts, None).ok_or_else(ts_overflow)?;
            let tz = parse_sane_timezone(&tm, text_arg(z), c)?;
            let r = ts + tz as i64 * USECS_PER_SEC;
            if !dt::ts_is_valid(r) {
                return Err(ts_overflow());
            }
            Ok(Value::Timestamptz(r))
        }
    }
}

/// make_interval.
fn make_interval(a: &[Value], _: &FnCtx) -> Result<Value> {
    let g = |i: usize| a.get(i).map(int_arg).unwrap_or(0) as i32;
    let secs = a.get(6).map(float_arg).unwrap_or(0.0);
    if secs.is_infinite() || secs.is_nan() {
        return Err(dt::interval_overflow());
    }
    let month = g(0)
        .checked_mul(12)
        .and_then(|y| y.checked_add(g(1)))
        .ok_or_else(dt::interval_overflow)?;
    let day = g(2)
        .checked_mul(7)
        .and_then(|w| w.checked_add(g(3)))
        .ok_or_else(dt::interval_overflow)?;
    let mut time = g(4) as i64 * USECS_PER_HOUR + g(5) as i64 * USECS_PER_MINUTE;
    // float8_mul's overflow check, then rint.
    let prod = secs * USECS_PER_SEC as f64;
    if prod.is_infinite() {
        bail!(NUMERIC_VALUE_OUT_OF_RANGE, "value out of range: overflow");
    }
    let s = prod.round_ties_even();
    if s.is_nan() || s < i64::MIN as f64 || s >= -(i64::MIN as f64) {
        return Err(dt::interval_overflow());
    }
    time = time
        .checked_add(s as i64)
        .ok_or_else(dt::interval_overflow)?;
    let r = Interval::new(month, day, time);
    if r.is_infinite() {
        return Err(dt::interval_overflow());
    }
    Ok(Value::Interval(r))
}

/// float8_timestamptz.
fn to_timestamp_epoch(a: &[Value], _: &FnCtx) -> Result<Value> {
    let f = float_arg(&a[0]);
    if f.is_nan() {
        bail!(DATETIME_FIELD_OVERFLOW, "timestamp cannot be NaN");
    }
    if f.is_infinite() {
        return Ok(Value::Timestamptz(if f > 0.0 {
            TS_POS_INF
        } else {
            TS_NEG_INF
        }));
    }
    let lo = 86400.0 * (0.0 - UNIX_EPOCH_JDATE as f64);
    let hi = 86400.0 * (dt::TIMESTAMP_END_JULIAN as f64 - UNIX_EPOCH_JDATE as f64);
    if f < lo || f >= hi {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "timestamp out of range: \"{}\"",
            fmt_g(f, 0, false)
        );
    }
    let us = ((f - UNIX_EPOCH_OFFSET_SECS as f64) * USECS_PER_SEC as f64).round_ties_even() as i64;
    if !dt::ts_is_valid(us) {
        bail!(
            DATETIME_FIELD_OVERFLOW,
            "timestamp out of range: \"{}\"",
            fmt_g(f, 0, false)
        );
    }
    Ok(Value::Timestamptz(us))
}

// ── AT TIME ZONE ──

fn interval_zone(iv: &Interval, c: &FnCtx) -> Result<i32> {
    let shown = || crate::types::text::format_interval(iv, c.exec.fmt());
    if iv.is_infinite() {
        bail!(
            INVALID_PARAMETER_VALUE,
            "interval time zone \"{}\" must be finite",
            shown()
        );
    }
    if iv.months != 0 || iv.days != 0 {
        bail!(
            INVALID_PARAMETER_VALUE,
            "interval time zone \"{}\" must not include months or days",
            shown()
        );
    }
    Ok((iv.micros / USECS_PER_SEC) as i32)
}

/// timestamp_zone: timestamp AT TIME ZONE text → timestamptz.
fn timestamp_zone(zone_name: &str, ts: i64, c: &FnCtx) -> Result<i64> {
    if !ts_is_finite(ts) {
        return Ok(ts);
    }
    let r = match dt::decode_timezone_name(zone_name, &c.exec.fmt().dt_ctx())? {
        TzName::Fixed(val) => ts - val as i64 * USECS_PER_SEC,
        TzName::Dynamic(z) => {
            let (tm, _) = timestamp2tm(ts, None).ok_or_else(ts_overflow)?;
            let tz = -dt::determine_tz_abbrev_offset(&tm, zone_name.as_bytes(), &z).0;
            ts - tz as i64 * USECS_PER_SEC
        }
        TzName::Zone(z) => {
            let (tm, fsec) = timestamp2tm(ts, None).ok_or_else(ts_overflow)?;
            let (tz, _) = dt::determine_tz_offset(&tm, &z);
            tm2timestamp(&tm, fsec, Some(tz)).ok_or_else(ts_overflow)?
        }
    };
    if !dt::ts_is_valid(r) {
        return Err(ts_overflow());
    }
    Ok(r)
}

/// timestamptz_zone: timestamptz AT TIME ZONE text → timestamp.
fn timestamptz_zone(zone_name: &str, ts: i64, c: &FnCtx) -> Result<i64> {
    if !ts_is_finite(ts) {
        return Ok(ts);
    }
    let r = match dt::decode_timezone_name(zone_name, &c.exec.fmt().dt_ctx())? {
        TzName::Fixed(val) => ts + val as i64 * USECS_PER_SEC,
        TzName::Dynamic(z) => {
            let (tz, _) = dt::determine_tz_abbrev_offset_ts(ts, zone_name, &z)?;
            ts - tz as i64 * USECS_PER_SEC
        }
        TzName::Zone(z) => {
            let (tm, fsec) = timestamp2tm(ts, Some(&z)).ok_or_else(ts_overflow)?;
            tm2timestamp(&tm, fsec, None).ok_or_else(ts_overflow)?
        }
    };
    if !dt::ts_is_valid(r) {
        return Err(ts_overflow());
    }
    Ok(r)
}

/// timetz_zone: (time, zone west) in another zone, at the transaction start.
fn timetz_zone(zone_name: &str, t: i64, west: i32, c: &FnCtx) -> Result<Value> {
    let now = c.exec.txn_time();
    let tz = match dt::decode_timezone_name(zone_name, &c.exec.fmt().dt_ctx())? {
        TzName::Fixed(val) => -val,
        TzName::Dynamic(z) => dt::determine_tz_abbrev_offset_ts(now, zone_name, &z)?.0,
        TzName::Zone(z) => {
            let (tm, _) = timestamp2tm(now, Some(&z)).ok_or_else(ts_overflow)?;
            -tm.gmtoff as i32
        }
    };
    Ok(timetz_rotate(t, west, tz))
}

fn timetz_rotate(t: i64, west: i32, tz: i32) -> Value {
    let mut r = t + (west as i64 - tz as i64) * USECS_PER_SEC;
    while r < 0 {
        r += USECS_PER_DAY;
    }
    if r >= USECS_PER_DAY {
        r %= USECS_PER_DAY;
    }
    Value::Timetz(r, tz)
}

/// time_timetz: a time in the session zone (as of today).
fn time_to_timetz(t: i64, c: &FnCtx) -> (i64, i32) {
    let tz = &c.exec.fmt().tz;
    let (mut tm, _) = timestamp2tm(c.exec.txn_time(), Some(tz)).unwrap_or_default();
    let (h, m, s, _) = dt::dt2time(t);
    tm.hour = h;
    tm.min = m;
    tm.sec = s;
    (t, dt::determine_tz_offset(&tm, tz).0)
}

/// timezone(zone, value): AT TIME ZONE.
fn timezone_fn(a: &[Value], c: &FnCtx) -> Result<Value> {
    if let Value::Interval(iv) = &a[0] {
        let tz = interval_zone(iv, c)?;
        return Ok(match &a[1] {
            Value::Timestamptz(ts) => {
                if !ts_is_finite(*ts) {
                    return Ok(Value::Timestamp(*ts));
                }
                let r = ts + tz as i64 * USECS_PER_SEC;
                if !dt::ts_is_valid(r) {
                    return Err(ts_overflow());
                }
                Value::Timestamp(r)
            }
            Value::Timestamp(ts) => {
                if !ts_is_finite(*ts) {
                    return Ok(Value::Timestamptz(*ts));
                }
                let r = ts - tz as i64 * USECS_PER_SEC;
                if !dt::ts_is_valid(r) {
                    return Err(ts_overflow());
                }
                Value::Timestamptz(r)
            }
            Value::Timetz(t, w) => timetz_rotate(*t, *w, -tz),
            Value::Time(t) => {
                let (t, w) = time_to_timetz(*t, c);
                timetz_rotate(t, w, -tz)
            }
            v => v.clone(),
        });
    }
    let name = text_arg(&a[0]);
    Ok(match &a[1] {
        Value::Timestamptz(ts) => Value::Timestamp(timestamptz_zone(name, *ts, c)?),
        Value::Timestamp(ts) => Value::Timestamptz(timestamp_zone(name, *ts, c)?),
        Value::Timetz(t, w) => timetz_zone(name, *t, *w, c)?,
        Value::Time(t) => {
            let (t, w) = time_to_timetz(*t, c);
            timetz_zone(name, t, w, c)?
        }
        v => v.clone(),
    })
}

/// AT LOCAL.
fn timezone_session(a: &[Value], c: &FnCtx) -> Result<Value> {
    let tz = c.exec.fmt().tz.clone();
    Ok(match &a[0] {
        Value::Timestamptz(ts) => Value::Timestamp(to_local(*ts, &tz)),
        Value::Timestamp(ts) => Value::Timestamptz(timestamp2timestamptz(*ts, &tz)?),
        Value::Timetz(t, w) => timetz_zone(&tz.name(), *t, *w, c)?,
        v => v.clone(),
    })
}

// ── date + time constructors ──

/// timestamp(date, time): datetime_timestamp.
fn datetime_timestamp(d: i32, t: i64) -> Result<i64> {
    let mut r = match d {
        DATE_NEG_INF => TS_NEG_INF,
        DATE_POS_INF => TS_POS_INF,
        d => {
            if d >= dt::TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE {
                bail!(DATETIME_FIELD_OVERFLOW, "date out of range for timestamp");
            }
            d as i64 * USECS_PER_DAY
        }
    };
    if ts_is_finite(r) {
        r += t;
        if !dt::ts_is_valid(r) {
            return Err(ts_overflow());
        }
    }
    Ok(r)
}
fn timestamp_date_time(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (Value::Date(d), Value::Time(t)) = (&a[0], &a[1]) else {
        return Ok(Value::Null);
    };
    Ok(Value::Timestamp(datetime_timestamp(*d, *t)?))
}
/// timestamptz(date, time): (date + time)::timestamptz.
fn timestamptz_date_time(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (Value::Date(d), Value::Time(t)) = (&a[0], &a[1]) else {
        return Ok(Value::Null);
    };
    let ts = datetime_timestamp(*d, *t)?;
    Ok(Value::Timestamptz(crate::types::cast::timestamp2timestamptz(
        ts,
        &c.exec.fmt().tz,
    )?))
}
/// timestamptz(date, timetz): datetimetz_timestamptz.
fn timestamptz_date_timetz(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (Value::Date(d), Value::Timetz(t, z)) = (&a[0], &a[1]) else {
        return Ok(Value::Null);
    };
    Ok(Value::Timestamptz(match *d {
        DATE_NEG_INF => TS_NEG_INF,
        DATE_POS_INF => TS_POS_INF,
        d => {
            let range = || err!(DATETIME_FIELD_OVERFLOW, "date out of range for timestamp");
            if d >= dt::TIMESTAMP_END_JULIAN - POSTGRES_EPOCH_JDATE {
                return Err(range());
            }
            let r = d as i64 * USECS_PER_DAY + t + *z as i64 * USECS_PER_SEC;
            if !dt::ts_is_valid(r) {
                return Err(range());
            }
            r
        }
    }))
}

// ── date_add / date_subtract ──

/// timestamptz_pl_interval_at_zone / timestamptz_mi_interval_at_zone (and the session-zone
/// date_add / date_subtract).
fn date_add_sub(a: &[Value], c: &FnCtx, sub: bool) -> Result<Value> {
    let tz = match a.get(2) {
        Some(z) => zone(text_arg(z), c)?,
        None => c.exec.fmt().tz.clone(),
    };
    let span = if sub {
        dt::interval_neg(&iv(&a[1]))?
    } else {
        iv(&a[1])
    };
    Ok(Value::Timestamptz(dt::tstz_add_interval(
        ts_of(&a[0]),
        &span,
        &tz,
    )?))
}
fn date_add(a: &[Value], c: &FnCtx) -> Result<Value> {
    date_add_sub(a, c, false)
}
fn date_subtract(a: &[Value], c: &FnCtx) -> Result<Value> {
    date_add_sub(a, c, true)
}

// ── age / justify / isfinite / overlaps ──

fn age2(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(match (&a[0], &a[1]) {
        (Value::Timestamptz(x), Value::Timestamptz(y)) => {
            dt::age_tz(*x, *y, Some(&c.exec.fmt().tz))?
        }
        (x, y) => dt::age(ts_of(x), ts_of(y))?,
    }))
}
fn age1(a: &[Value], c: &FnCtx) -> Result<Value> {
    let tz = c.exec.fmt().tz.clone();
    let today = split_ts(to_local(c.exec.txn_time(), &tz)).0 * USECS_PER_DAY;
    Ok(Value::Interval(match &a[0] {
        Value::Timestamptz(t) => {
            let midnight = timestamp2timestamptz(today, &tz)?;
            dt::age_tz(midnight, *t, Some(&tz))?
        }
        v => dt::age(today, ts_of(v))?,
    }))
}
fn iv(v: &Value) -> Interval {
    match v {
        Value::Interval(i) => *i,
        _ => Interval::new(0, 0, 0),
    }
}
fn justify_hours(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(dt::justify_hours(iv(&a[0]))?))
}
fn justify_days(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(dt::justify_days(iv(&a[0]))?))
}
fn justify_interval(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Interval(dt::justify_interval(iv(&a[0]))?))
}
fn isfinite(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(match &a[0] {
        Value::Date(d) => *d != DATE_POS_INF && *d != DATE_NEG_INF,
        Value::Timestamp(t) | Value::Timestamptz(t) => ts_is_finite(*t),
        Value::Interval(i) => !i.is_infinite(),
        _ => true,
    }))
}
fn overlaps(a: &[Value], _: &FnCtx) -> Result<Value> {
    // (s1, e1) OVERLAPS (s2, e2); an end may be an interval.
    let key = |v: &Value, start: &Value| -> Option<i128> {
        match v {
            Value::Null => None,
            Value::Interval(i) => {
                let s = key_of(start)?;
                Some(s + i.cmp_key())
            }
            v => key_of(v),
        }
    };
    fn key_of(v: &Value) -> Option<i128> {
        match v {
            Value::Timestamp(t) | Value::Timestamptz(t) | Value::Time(t) => Some(*t as i128),
            Value::Date(d) => Some(*d as i128 * USECS_PER_DAY as i128),
            Value::Timetz(t, w) => Some(*t as i128 + *w as i128 * USECS_PER_SEC as i128),
            _ => None,
        }
    }
    let (mut s1, mut e1) = (key_of(&a[0]), key(&a[1], &a[0]));
    let (mut s2, mut e2) = (key_of(&a[2]), key(&a[3], &a[2]));
    if s1.is_none() {
        s1 = e1;
        e1 = None;
    }
    if let (Some(s), Some(e)) = (s1, e1)
        && s > e
    {
        s1 = Some(e);
        e1 = Some(s);
    }
    if s2.is_none() {
        s2 = e2;
        e2 = None;
    }
    if let (Some(s), Some(e)) = (s2, e2)
        && s > e
    {
        s2 = Some(e);
        e2 = Some(s);
    }
    let (Some(s1), Some(s2)) = (s1, s2) else {
        return Ok(Value::Null);
    };
    if s1 > s2 {
        let Some(e2) = e2 else { return Ok(Value::Null) };
        if s1 < e2 {
            return Ok(Value::Bool(true));
        }
        if e1.is_none() {
            return Ok(Value::Null);
        }
        Ok(Value::Bool(false))
    } else if s1 < s2 {
        let Some(e1) = e1 else { return Ok(Value::Null) };
        if s2 < e1 {
            return Ok(Value::Bool(true));
        }
        if e2.is_none() {
            return Ok(Value::Null);
        }
        Ok(Value::Bool(false))
    } else {
        if e1.is_none() || e2.is_none() {
            return Ok(Value::Null);
        }
        Ok(Value::Bool(true))
    }
}

// ── to_char / to_timestamp (formatting.c) ──

use super::formatting::{self as fmtg, NumArg};

fn to_char_ts(a: &[Value], c: &FnCtx) -> Result<Value> {
    let fmt = text_arg(&a[1]);
    let tz = c.exec.fmt().tz.clone();
    let r = match &a[0] {
        Value::Timestamptz(ts) => fmtg::timestamp_to_char(*ts, fmt, Some(&tz))?,
        Value::Timestamp(ts) => fmtg::timestamp_to_char(*ts, fmt, None)?,
        Value::Date(d) => fmtg::timestamp_to_char(from_local(date_to_ts(*d), &tz), fmt, Some(&tz))?,
        _ => None,
    };
    Ok(r.map(text).unwrap_or(Value::Null))
}

fn to_char_interval(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(fmtg::interval_to_char(&iv(&a[0]), text_arg(&a[1]))?
        .map(text)
        .unwrap_or(Value::Null))
}

fn to_char_num(a: &[Value], _: &FnCtx) -> Result<Value> {
    let v = match &a[0] {
        Value::Numeric(n) => NumArg::Numeric((**n).clone()),
        Value::Float4(f) => NumArg::Float4(*f),
        Value::Float8(f) => NumArg::Float8(*f),
        Value::Int8(i) => NumArg::Int(*i, false),
        v => NumArg::Int(v.as_i64().unwrap_or(0), true),
    };
    Ok(text(fmtg::number_to_char(&v, text_arg(&a[1]))?))
}

fn to_timestamp_fmt(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Timestamptz(fmtg::to_timestamp(
        text_arg(&a[0]),
        text_arg(&a[1]),
        &c.exec.fmt().dt_ctx(),
    )?))
}
fn to_date_fmt(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::Date(fmtg::to_date(
        text_arg(&a[0]),
        text_arg(&a[1]),
        &c.exec.fmt().dt_ctx(),
    )?))
}
fn to_number(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(fmtg::to_number(text_arg(&a[0]), text_arg(&a[1]))?
        .map(Value::numeric)
        .unwrap_or(Value::Null))
}

pub fn register(r: &mut Registry) {
    use T::*;
    let tstz = || Timestamptz(-1);
    let ts = || Timestamp(-1);
    let ivl = || Interval(-1);
    r.func("now", vec![], tstz(), now).stable();
    r.func("transaction_timestamp", vec![], tstz(), now)
        .stable();
    r.func("current_timestamp", vec![], tstz(), now).stable();
    r.func("current_timestamp", vec![Int4], tstz(), current_timestamp_p)
        .stable();
    r.func("statement_timestamp", vec![], tstz(), statement_timestamp)
        .stable();
    r.func("clock_timestamp", vec![], tstz(), clock_timestamp)
        .volatile();
    r.func("timeofday", vec![], Text, timeofday).volatile();
    r.func("current_date", vec![], Date, current_date).stable();
    r.func("current_time", vec![], Timetz(-1), current_time)
        .stable();
    r.func("current_time", vec![Int4], Timetz(-1), current_time)
        .stable();
    r.func("localtime", vec![], Time(-1), localtime).stable();
    r.func("localtime", vec![Int4], Time(-1), localtime)
        .stable();
    r.func("localtimestamp", vec![], ts(), localtimestamp)
        .stable();
    r.func("localtimestamp", vec![Int4], ts(), localtimestamp)
        .stable();
    for t in [ts(), tstz(), Date, Time(-1), Timetz(-1), ivl()] {
        let stable = matches!(t, Timestamptz(_) | Timetz(_));
        let f = r.func("extract", vec![Text, t.clone()], Numeric(-1), extract);
        if stable {
            f.stable();
        }
        if !matches!(t, Date) {
            let f = r.func("date_part", vec![Text, t], Float8, date_part);
            if stable {
                f.stable();
            }
        }
    }
    r.func("date_part", vec![Text, Date], Float8, date_part);
    r.func("date_trunc", vec![Text, ts()], ts(), date_trunc);
    r.func("date_trunc", vec![Text, tstz()], tstz(), date_trunc)
        .stable();
    r.func("date_trunc", vec![Text, tstz(), Text], tstz(), date_trunc);
    r.func("date_trunc", vec![Text, ivl()], ivl(), date_trunc);
    r.func("date_bin", vec![ivl(), ts(), ts()], ts(), date_bin);
    r.func("date_bin", vec![ivl(), tstz(), tstz()], tstz(), date_bin);
    r.func("make_date", vec![Int4, Int4, Int4], Date, make_date)
        .names(&["year", "month", "day"]);
    r.func("make_time", vec![Int4, Int4, Float8], Time(-1), make_time)
        .names(&["hour", "min", "sec"]);
    r.func(
        "make_timestamp",
        vec![Int4, Int4, Int4, Int4, Int4, Float8],
        ts(),
        make_timestamp,
    )
    .names(&["year", "month", "mday", "hour", "min", "sec"]);
    r.func(
        "make_timestamptz",
        vec![Int4, Int4, Int4, Int4, Int4, Float8],
        tstz(),
        make_timestamptz,
    )
    .stable()
    .names(&["year", "month", "mday", "hour", "min", "sec"]);
    r.func(
        "make_timestamptz",
        vec![Int4, Int4, Int4, Int4, Int4, Float8, Text],
        tstz(),
        make_timestamptz,
    )
    .stable()
    .names(&["year", "month", "mday", "hour", "min", "sec", "timezone"]);
    r.func(
        "make_interval",
        vec![Int4, Int4, Int4, Int4, Int4, Int4, Float8],
        ivl(),
        make_interval,
    )
    .min_args(0)
    .names(&["years", "months", "weeks", "days", "hours", "mins", "secs"]);
    r.func("to_timestamp", vec![Float8], tstz(), to_timestamp_epoch);
    r.func("to_timestamp", vec![Text, Text], tstz(), to_timestamp_fmt)
        .stable();
    r.func("to_date", vec![Text, Text], Date, to_date_fmt)
        .stable();
    r.func("to_number", vec![Text, Text], Numeric(-1), to_number)
        .stable();
    r.func("timezone", vec![Text, tstz()], ts(), timezone_fn);
    r.func("timezone", vec![Text, ts()], tstz(), timezone_fn);
    r.func("timezone", vec![ivl(), tstz()], ts(), timezone_fn);
    r.func("timezone", vec![ivl(), ts()], tstz(), timezone_fn);
    r.func("timezone", vec![Text, Timetz(-1)], Timetz(-1), timezone_fn)
        .volatile();
    r.func("timezone", vec![ivl(), Timetz(-1)], Timetz(-1), timezone_fn);
    r.func("timezone", vec![Timetz(-1)], Timetz(-1), timezone_session)
        .volatile();
    r.func("timezone", vec![tstz()], ts(), timezone_session)
        .stable();
    r.func("timezone", vec![ts()], tstz(), timezone_session)
        .stable();
    r.func("timestamp", vec![Date, Time(-1)], ts(), timestamp_date_time);
    r.func("timestamptz", vec![Date, Time(-1)], tstz(), timestamptz_date_time)
        .stable();
    r.func("timestamptz", vec![Date, Timetz(-1)], tstz(), timestamptz_date_timetz);
    r.func("date_add", vec![tstz(), ivl()], tstz(), date_add)
        .stable();
    r.func("date_add", vec![tstz(), ivl(), Text], tstz(), date_add);
    r.func("date_subtract", vec![tstz(), ivl()], tstz(), date_subtract)
        .stable();
    r.func("date_subtract", vec![tstz(), ivl(), Text], tstz(), date_subtract);
    r.func("age", vec![ts(), ts()], ivl(), age2);
    r.func("age", vec![tstz(), tstz()], ivl(), age2).stable();
    r.func("age", vec![ts()], ivl(), age1).stable();
    r.func("age", vec![tstz()], ivl(), age1).stable();
    r.func("justify_hours", vec![ivl()], ivl(), justify_hours);
    r.func("justify_days", vec![ivl()], ivl(), justify_days);
    r.func("justify_interval", vec![ivl()], ivl(), justify_interval);
    for t in [Date, ts(), tstz(), ivl()] {
        r.func("isfinite", vec![t], Bool, isfinite);
    }
    // Each kind of start, with an end or an interval for either period (no date variant: dates
    // become timestamps).
    for start in [tstz(), ts(), Time(-1), Timetz(-1)] {
        let ends: &[(bool, bool)] = if matches!(start, Timetz(_)) {
            &[(false, false)]
        } else {
            &[(false, false), (true, true), (false, true), (true, false)]
        };
        for &(i1, i2) in ends {
            let end = |i: bool| if i { ivl() } else { start.clone() };
            let f = r
                .func(
                    "overlaps",
                    vec![start.clone(), end(i1), start.clone(), end(i2)],
                    Bool,
                    overlaps,
                )
                .non_strict();
            // Postgres's SQL-language variants over timestamptz add intervals in the session zone.
            if matches!(start, Timestamptz(_)) && (i1 || i2) {
                f.stable();
            }
        }
    }
    r.func("to_char", vec![ts(), Text], Text, to_char_ts)
        .stable();
    r.func("to_char", vec![tstz(), Text], Text, to_char_ts)
        .stable();
    r.func("to_char", vec![ivl(), Text], Text, to_char_interval);
    for t in [Int4, Int8, Float4, Float8, Numeric(-1)] {
        r.func("to_char", vec![t, Text], Text, to_char_num).stable();
    }
}
