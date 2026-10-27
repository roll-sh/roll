//! Cassandra 5's `floor` functions (`TimeFcts.FloorFunction`, `Duration.floorTimestamp` /
//! `floorTime`, Apache-2.0): a timestamp, timeuuid or date rounded down to a multiple of a
//! duration from a starting time (the epoch by default), and a time of day rounded down to a
//! multiple of a sub-day duration. Months follow `GregorianCalendar` in UTC (a month added to the
//! 31st lands on the last day of a shorter month). ScyllaDB has no `floor`.

use bytes::Bytes;
use nosql_types::{CqlType, Duration, temporal};

use super::util::{arg, bytes_i64, i64_of};
use super::{FnCtx, Native, Registry};
use crate::error::{Error, Result};

const MILLIS_PER_DAY: i64 = 86_400_000;
const NANOS_PER_MILLI: i64 = 1_000_000;
const NANOS_PER_DAY: i64 = 86_400 * 1_000_000_000;

pub fn register(r: &mut Registry) {
    if r.dialect().is_scylla() {
        return;
    }
    let ts = CqlType::Timestamp;
    let du = CqlType::Duration;
    r.add(Native::new(
        "floor",
        vec![ts.clone(), du.clone()],
        ts.clone(),
        |c, a| floor(c, a, Src::Timestamp),
    ));
    r.add(Native::new(
        "floor",
        vec![ts.clone(), du.clone(), ts.clone()],
        ts.clone(),
        |c, a| floor(c, a, Src::Timestamp),
    ));
    r.add(Native::new(
        "floor",
        vec![CqlType::Timeuuid, du.clone()],
        ts.clone(),
        |c, a| floor(c, a, Src::Timeuuid),
    ));
    r.add(Native::new(
        "floor",
        vec![CqlType::Timeuuid, du.clone(), ts.clone()],
        ts.clone(),
        |c, a| floor(c, a, Src::Timeuuid),
    ));
    r.add(Native::new(
        "floor",
        vec![CqlType::Date, du.clone()],
        CqlType::Date,
        |c, a| floor(c, a, Src::Date),
    ));
    r.add(Native::new(
        "floor",
        vec![CqlType::Date, du.clone(), CqlType::Date],
        CqlType::Date,
        |c, a| floor(c, a, Src::Date),
    ));
    r.add(Native::new(
        "floor",
        vec![CqlType::Time, du],
        CqlType::Time,
        floor_time,
    ));
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Src {
    Timestamp,
    Timeuuid,
    Date,
}

fn bad() -> Error {
    Error::invalid("Invalid value for the floor function")
}

/// Milliseconds since the epoch of a timestamp, timeuuid or date argument.
fn millis(src: Src, b: &[u8]) -> Result<i64> {
    match src {
        Src::Timestamp => i64_of(b).ok_or_else(bad),
        Src::Timeuuid => <[u8; 16]>::try_from(b)
            .map(|u| nosql_types::timeuuid::unix_timestamp_of(&u))
            .map_err(|_| bad()),
        Src::Date => <[u8; 4]>::try_from(b)
            .map(|d| temporal::date_to_millis(u32::from_be_bytes(d)))
            .map_err(|_| bad()),
    }
}

fn duration(b: &[u8]) -> Result<Duration> {
    Duration::decode(b).ok_or_else(bad)
}

fn floor(ctx: &FnCtx, args: &[Option<Bytes>], src: Src) -> Result<Option<Bytes>> {
    let (Some(t), Some(d)) = (arg(args, 0), arg(args, 1)) else {
        return Ok(None);
    };
    if args.len() == 3 && arg(args, 2).is_none() {
        return Ok(None);
    }
    let time = millis(src, t)?;
    let dur = duration(d)?;
    let start = match arg(args, 2) {
        Some(s) => millis(
            if src == Src::Date {
                Src::Date
            } else {
                Src::Timestamp
            },
            s,
        )?,
        None => 0,
    };
    match src {
        Src::Date if dur.nanos != 0 => {
            return Err(Error::invalid(format!(
                "The floor on date values cannot be computed for the {} duration as precision is below 1 day",
                dur.to_text()
            )));
        }
        _ if dur.nanos % NANOS_PER_MILLI != 0 => {
            return Err(Error::invalid(format!(
                "The floor cannot be computed for the {} duration as precision is below 1 millisecond",
                dur.to_text()
            )));
        }
        _ => {}
    }
    let floor = floor_timestamp(time, &dur, start)?;
    Ok(match src {
        Src::Date => Some(Bytes::copy_from_slice(
            &temporal::millis_to_date(floor, ctx.dialect).to_be_bytes(),
        )),
        _ => bytes_i64(floor),
    })
}

fn duration_millis(d: &Duration) -> i64 {
    (d.days as i64)
        .wrapping_mul(MILLIS_PER_DAY)
        .wrapping_add(d.nanos / NANOS_PER_MILLI)
}

/// `(year, month 0-11)` of a UTC instant.
fn year_month(ms: i64) -> (i64, i64) {
    let (y, m, _) = temporal::civil_from_days(ms.div_euclid(MILLIS_PER_DAY));
    (y, m as i64 - 1)
}

/// `Calendar.add(MONTH, n)` in UTC: the day of month is clamped to the new month's length.
fn add_months(ms: i64, n: i64) -> i64 {
    let days = ms.div_euclid(MILLIS_PER_DAY);
    let within = ms.rem_euclid(MILLIS_PER_DAY);
    let (y, m, d) = temporal::civil_from_days(days);
    let total = y * 12 + (m as i64 - 1) + n;
    let (ny, nm) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    let nd = d.min(temporal::month_length(ny, nm));
    temporal::days_from_civil(ny, nm, nd) * MILLIS_PER_DAY + within
}

/// `Duration.floorTimestamp`.
fn floor_timestamp(time: i64, d: &Duration, start: i64) -> Result<i64> {
    if start > time {
        return Err(Error::invalid(
            "The floor function starting time is greater than the provided time",
        ));
    }
    if d.is_negative() {
        return Err(Error::invalid(
            "Negative durations are not supported by the floor function",
        ));
    }
    if d.months == 0 {
        let step = duration_millis(d);
        if step == 0 {
            return Ok(time);
        }
        return Ok(time - (time - start) % step);
    }
    let (year, month) = year_month(time);
    let (sy, sm) = year_month(start);
    let months_between = (year - sy) * 12 + (month - sm);
    let mut multiplier = months_between / d.months as i64;
    let mut cal = add_months(start, multiplier * d.months as i64);
    if d.days == 0 && d.nanos == 0 {
        return Ok(cal);
    }
    let step = duration_millis(d);
    let mut floor = cal + multiplier * step;
    let mut guard = 0;
    while floor > time {
        multiplier -= 1;
        cal = add_months(cal, -(d.months as i64));
        floor = cal + multiplier * step;
        guard += 1;
        if guard > 1_000_000 {
            break;
        }
    }
    Ok(start.max(floor))
}

/// `floor(time, duration)` (`Duration.floorTime`).
fn floor_time(_: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
    let (Some(t), Some(d)) = (arg(args, 0), arg(args, 1)) else {
        return Ok(None);
    };
    let time = i64_of(t).ok_or_else(bad)?;
    let d = duration(d)?;
    if d.is_negative() {
        return Err(Error::invalid(
            "Negative durations are not supported by the floor function",
        ));
    }
    if d.months != 0 || d.days != 0 || d.nanos > NANOS_PER_DAY {
        return Err(Error::invalid(
            "For time values, the floor can only be computed for durations smaller that a day",
        ));
    }
    if d.nanos == 0 {
        return Ok(bytes_i64(time));
    }
    Ok(bytes_i64(time - time % d.nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dur(months: i32, days: i32, nanos: i64) -> Duration {
        Duration::new(months, days, nanos)
    }

    #[test]
    fn floors() {
        // 2020-05-05 13:47:12.345 UTC.
        let t = 1_588_686_432_345;
        assert_eq!(
            floor_timestamp(t, &dur(0, 0, 3_600_000_000_000), 0).unwrap(),
            1_588_683_600_000
        );
        // Months only: Cassandra returns the start plus whole months between the two months,
        // even past the time (2020-01-31 + 2 months for 2020-03-05).
        let jan31 = temporal::days_from_civil(2020, 1, 31) * MILLIS_PER_DAY;
        let mar5 = temporal::days_from_civil(2020, 3, 5) * MILLIS_PER_DAY;
        assert_eq!(
            floor_timestamp(mar5, &dur(1, 0, 0), jan31).unwrap(),
            temporal::days_from_civil(2020, 3, 31) * MILLIS_PER_DAY
        );
        // `Calendar.add` clamps: one month from 2020-01-31 is 2020-02-29.
        assert_eq!(
            add_months(jan31, 1),
            temporal::days_from_civil(2020, 2, 29) * MILLIS_PER_DAY
        );
        // With days the multiplier backs off until the floor is not after the time.
        assert_eq!(
            floor_timestamp(mar5, &dur(1, 1, 0), jan31).unwrap(),
            temporal::days_from_civil(2020, 3, 1) * MILLIS_PER_DAY
        );
        assert_eq!(
            floor_timestamp(0, &dur(0, 1, 0), 5).unwrap_err().message,
            "The floor function starting time is greater than the provided time"
        );
        assert_eq!(
            floor_timestamp(5, &dur(0, -1, 0), 0).unwrap_err().message,
            "Negative durations are not supported by the floor function"
        );
    }
}
