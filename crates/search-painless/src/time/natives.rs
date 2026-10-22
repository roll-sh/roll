//! Native methods of the `java.time` classes: thin adapters from the allowlisted signatures to
//! the generic field/unit operations in the parent module.

use std::rc::Rc;
use std::sync::Arc;

use super::weeks::{WeekDef, XField, XUnit};
use super::*;
use crate::value::{JList, JMap};

type N = crate::lookup::Native;

fn this(a: &[Value]) -> Result<TimeVal, Thrown> {
    Ok(tv(&a[0])?.clone())
}

fn l(a: &[Value], i: usize) -> i64 {
    long(&a[i])
}

/// An `IsoFields`/`JulianFields`/`WeekFields` field argument.
fn xfield(v: &Value) -> Option<XField> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Time(TimeVal::XField(f)) => Some(*f),
            _ => None,
        },
        _ => None,
    }
}

/// An `IsoFields` unit argument.
fn xunit(v: &Value) -> Option<XUnit> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Time(TimeVal::XUnit(u)) => Some(*u),
            _ => None,
        },
        _ => None,
    }
}

fn xrange(r4: (i64, i64, i64, i64)) -> VR {
    r(TimeVal::Range { min_s: r4.0, min_l: r4.1, max_s: r4.2, max_l: r4.3 })
}

/// `TemporalAccessor.get`'s check that the field fits an int.
fn x_get_int(x: XField, t: &TimeVal) -> VR {
    let (lo, _, _, hi) = x.range();
    if lo < i32::MIN as i64 || hi > i32::MAX as i64 {
        let e = Throw::new(cls::UNSUPPORTED_TEMPORAL_TYPE_EXCEPTION, Some(format!("Invalid field {} for get() method, use getLong() instead", x.name())));
        let mut frames = vec!["temporal.TemporalAccessor.get(TemporalAccessor.java:220)"];
        if matches!(t, TimeVal::LocalDate(_)) {
            frames.push("LocalDate.get(LocalDate.java:663)");
        }
        return Err(jt(e, &frames));
    }
    Ok(Value::Int(x.get(t)? as i32))
}

fn i(a: &[Value], i: usize) -> i64 {
    int(&a[i]) as i64
}

// ------------------------------------------------------------------------------ generic

fn t_get(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[1]) {
        return x_get_int(x, tv(&a[0])?);
    }
    let f = field_of(&a[1])?;
    let t = tv(&a[0])?;
    let date = matches!(t, TimeVal::LocalDate(_));
    Ok(Value::Int(t.get_int(f).map_err(|e| {
        if date && e.class == cls::UNSUPPORTED_TEMPORAL_TYPE_EXCEPTION {
            let line = if e.message.as_deref().unwrap_or("").starts_with("Invalid field") { 710 } else { 718 };
            jt(e, &[&format!("LocalDate.get0(LocalDate.java:{line})"), "LocalDate.get(LocalDate.java:661)"])
        } else {
            e
        }
    })?))
}
fn t_get_long(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[1]) {
        return Ok(Value::Long(x.get(tv(&a[0])?)?));
    }
    let f = field_of(&a[1])?;
    Ok(Value::Long(tv(&a[0])?.get_long(f)?))
}
fn t_is_supported(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Ok(Value::Bool(false));
    }
    let t = tv(&a[0])?;
    Ok(Value::Bool(match tv(&a[1])? {
        TimeVal::TField(f) => t.is_supported(*f),
        TimeVal::XField(x) => x.is_supported_by(t),
        TimeVal::XUnit(u) => u.add_to(t, 0).is_ok(),
        TimeVal::TUnit(u) => t.plus_unit(0, *u).is_ok() && !matches!(t, TimeVal::Instant { .. } if u.is_date_based() && *u != Unit::Days),
        _ => false,
    }))
}
fn t_range(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[1]) {
        return xrange(x.range_refined(tv(&a[0])?)?);
    }
    let f = field_of(&a[1])?;
    let t = tv(&a[0])?;
    t.get_long(f)?;
    let (lo, hi) = f.range();
    let (max_s, max_l) = match (f, t) {
        (Field::DayOfMonth, _) => {
            let d = to_local_date(t)?;
            let n = month_len(d.y as i64, d.m) as i64;
            (n, n)
        }
        (Field::DayOfYear, _) => {
            let d = to_local_date(t)?;
            let n = d.len_of_year() as i64;
            (n, n)
        }
        _ => (hi, hi),
    };
    r(TimeVal::Range { min_s: lo, min_l: lo, max_s, max_l })
}
fn t_plus_unit(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xunit(&a[2]) {
        return r(x.add_to(tv(&a[0])?, l(a, 1))?);
    }
    let u = unit_of(&a[2])?;
    let t = tv(&a[0])?;
    let is_instant = matches!(t, TimeVal::Instant { .. });
    r(t.plus_unit(l(a, 1), u).map_err(|e| if is_instant && e.class == cls::UNSUPPORTED_TEMPORAL_TYPE_EXCEPTION { jt(e, &["Instant.plus(Instant.java:887)"]) } else { e })?)
}
fn t_minus_unit(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xunit(&a[2]) {
        let n = l(a, 1).checked_neg().ok_or_else(|| crate::vm::Throw::new(cls::ARITHMETIC_EXCEPTION, Some("long overflow".into())))?;
        return r(x.add_to(tv(&a[0])?, n)?);
    }
    let u = unit_of(&a[2])?;
    let n = l(a, 1);
    let t = tv(&a[0])?;
    r(if n == i64::MIN { t.plus_unit(i64::MAX, u)?.plus_unit(1, u)? } else { t.plus_unit(-n, u)? })
}
fn t_plus_amount(_: &mut Vm, a: &mut [Value]) -> VR {
    let amt = arg_tv(&a[1], "amountToAdd")?.clone();
    let t = tv(&a[0])?;
    let instant_period = matches!(t, TimeVal::Instant { .. }) && matches!(amt, TimeVal::Period { .. });
    r(plus_amount(t, &amt, 1).map_err(|e| {
        if instant_period && e.class == cls::UNSUPPORTED_TEMPORAL_TYPE_EXCEPTION {
            jt(e, &["Instant.plus(Instant.java:887)", "Instant.plus(Instant.java:209)", "Period.addTo(Period.java:905)", "Instant.plus(Instant.java:788)"])
        } else {
            e
        }
    })?)
}
fn t_minus_amount(_: &mut Vm, a: &mut [Value]) -> VR {
    let amt = arg_tv(&a[1], "amountToSubtract")?.clone();
    r(plus_amount(tv(&a[0])?, &amt, -1)?)
}
fn t_until(_: &mut Vm, a: &mut [Value]) -> VR {
    let end = arg_tv(&a[1], "endExclusive")?.clone();
    if let Some(x) = xunit(&a[2]) {
        return Ok(Value::Long(x.between(tv(&a[0])?, &end)?));
    }
    let u = unit_of(&a[2])?;
    Ok(Value::Long(tv(&a[0])?.until(&end, u)?))
}
fn t_with_field(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[1]) {
        let t = tv(&a[0])?;
        let date = matches!(t, TimeVal::LocalDate(_));
        // `LocalDate.with(TemporalField, long)` hands other fields to `adjustInto`
        return r(x.adjust(t, l(a, 2)).map_err(|e| if date && !e.frames.is_empty() { jt(e, &["LocalDate.with(LocalDate.java:1070)"]) } else { e })?);
    }
    let f = field_of(&a[1])?;
    let t = tv(&a[0])?;
    let date = matches!(t, TimeVal::LocalDate(_));
    r(t.with_field(f, l(a, 2)).map_err(|e| if date && e.frames.len() == 2 { jt(e, &["LocalDate.with(LocalDate.java:1052)"]) } else { e })?)
}
fn t_with_adjuster(vm: &mut Vm, a: &mut [Value]) -> VR {
    let t = this(a)?;
    let adj = a[1].clone();
    r(adjust(&t, &adj, vm)?)
}
fn t_truncated(_: &mut Vm, a: &mut [Value]) -> VR {
    let u = unit_of(&a[1])?;
    r(tv(&a[0])?.truncated(u)?)
}
fn t_to_epoch_milli(_: &mut Vm, a: &mut [Value]) -> VR {
    let t = tv(&a[0])?;
    let s = t.get_long(Field::InstantSeconds)?;
    let n = t.get_long(Field::NanoOfSecond)?;
    Ok(Value::Long(s.wrapping_mul(1000).wrapping_add(n / 1_000_000)))
}
fn t_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    let o = arg_tv(&a[1], "other")?;
    match tv(&a[0])?.compare(o) {
        Some(c) => Ok(Value::Int(c as i32)),
        None => Err(crate::vm::cce(crate::cast::checkcast_message(
            crate::lookup::java_binary_name(o.class()),
            crate::lookup::java_binary_name(tv(&a[0])?.class()),
        ))),
    }
}
fn t_is_after(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(time_cmp(a)? == std::cmp::Ordering::Greater))
}
fn t_is_before(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(time_cmp(a)? == std::cmp::Ordering::Less))
}
fn t_is_equal(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(time_cmp(a)? == std::cmp::Ordering::Equal))
}
/// Chronological comparison (instants compare by instant only, dates by epoch day).
fn time_cmp(a: &[Value]) -> Result<std::cmp::Ordering, Thrown> {
    let (x, y) = (tv(&a[0])?, arg_tv(&a[1], "other")?);
    if let (Some(p), Some(q)) = (x.instant(), y.instant()) {
        return Ok(p.cmp(&q));
    }
    match (x, y) {
        (TimeVal::LocalDateTime(d1, t1), TimeVal::LocalDateTime(d2, t2)) => Ok((d1, t1).cmp(&(d2, t2))),
        (TimeVal::OffsetTime { t: t1, offset: o1 }, TimeVal::OffsetTime { t: t2, offset: o2 }) => {
            Ok((t1.0 - *o1 as i64 * NANOS_PER_SECOND).cmp(&(t2.0 - *o2 as i64 * NANOS_PER_SECOND)))
        }
        _ => match (to_local_date(x), to_local_date(y)) {
            (Ok(p), Ok(q)) => Ok(p.epoch_day().cmp(&q.epoch_day())),
            _ => x.compare(y).ok_or_else(|| crate::vm::cce("incomparable temporals")),
        },
    }
}
fn t_format(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = format::formatter_of(&a[1])?;
    Ok(Value::string(f.format(tv(&a[0])?)?))
}
fn t_query(vm: &mut Vm, a: &mut [Value]) -> VR {
    let q = a[1].clone();
    crate::func::call(vm, &q, &[a[0].clone()])
}

// ------------------------------------------------------------------------------ accessors

macro_rules! getter_int {
    ($name:ident, $f:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            Ok(Value::Int(tv(&a[0])?.get_long($f)? as i32))
        }
    };
}
getter_int!(g_year, Field::Year);
getter_int!(g_month_value, Field::MonthOfYear);
getter_int!(g_day_of_month, Field::DayOfMonth);
getter_int!(g_day_of_year, Field::DayOfYear);
getter_int!(g_hour, Field::HourOfDay);
getter_int!(g_minute, Field::MinuteOfHour);
getter_int!(g_second, Field::SecondOfMinute);
getter_int!(g_nano, Field::NanoOfSecond);
fn g_month(_: &mut Vm, a: &mut [Value]) -> VR {
    r(TimeVal::Month(tv(&a[0])?.get_long(Field::MonthOfYear)? as u8))
}
fn g_day_of_week(_: &mut Vm, a: &mut [Value]) -> VR {
    r(TimeVal::DayOfWeek(tv(&a[0])?.get_long(Field::DayOfWeek)? as u8))
}
fn g_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    r(TimeVal::ZoneId(Zone::Offset(tv(&a[0])?.get_long(Field::OffsetSeconds)? as i32)))
}
fn g_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    match tv(&a[0])? {
        TimeVal::Zoned { zone, .. } => r(TimeVal::ZoneId(zone.clone())),
        TimeVal::Clock { zone, .. } => r(TimeVal::ZoneId(zone.clone())),
        other => Err(unsupported(format!("Unsupported: getZone on {}", other.to_java_string()))),
    }
}
fn g_chronology(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(iso_chronology())
}

/// `IsoChronology.INSTANCE` (the only chronology of the java.time types scripts get).
pub fn iso_chronology() -> Value {
    time_value(TimeVal::Enum(cls::ISO_CHRONOLOGY, 0))
}
fn chrono_id(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::str("ISO"))
}
fn chrono_calendar_type(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::str("iso8601"))
}
fn chrono_is_leap(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(is_leap(l(a, 1))))
}
fn chrono_date(vm: &mut Vm, a: &mut [Value]) -> VR {
    ld_of(vm, &mut a[1..])
}
fn chrono_date_year_day(vm: &mut Vm, a: &mut [Value]) -> VR {
    ld_of_year_day(vm, &mut a[1..])
}
fn chrono_date_epoch_day(vm: &mut Vm, a: &mut [Value]) -> VR {
    ld_of_epoch_day(vm, &mut a[1..])
}
fn chrono_date_from(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::LOCAL_DATE, arg_tv(&a[1], "temporal")?)?)
}
fn chrono_local_date_time(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::LOCAL_DATE_TIME, arg_tv(&a[1], "temporal")?)?)
}
fn chrono_zoned_date_time(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::ZONED_DATE_TIME, arg_tv(&a[1], "temporal")?)?)
}
fn chrono_zoned_date_time2(vm: &mut Vm, a: &mut [Value]) -> VR {
    zdt_of_instant(vm, &mut a[1..])
}
fn chrono_period(vm: &mut Vm, a: &mut [Value]) -> VR {
    p_of(vm, &mut a[1..])
}
fn chrono_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let id = a[0].to_java_string();
    if id == "ISO" || id == "iso8601" {
        return Ok(iso_chronology());
    }
    Err(dte(format!("Unknown chronology: {id}")))
}
fn chrono_of_locale(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(iso_chronology())
}
fn chrono_from(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(iso_chronology())
}
fn chrono_display_name(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::str("ISO"))
}
fn chrono_compare(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Int(0))
}
fn iso_instance(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(iso_chronology())
}
fn register_chronology(b: &mut Builder) {
    use cls::*;
    for c in [CHRONOLOGY, ISO_CHRONOLOGY] {
        b.m(c, "String getId()", chrono_id);
        b.m(c, "String getCalendarType()", chrono_calendar_type);
        b.m(c, "boolean isLeapYear(long)", chrono_is_leap);
        b.m(c, "String getDisplayName(TextStyle,Locale)", chrono_display_name);
        b.m(c, "int compareTo(Chronology)", chrono_compare);
    }
    b.m(CHRONOLOGY, "ChronoLocalDate date(int,int,int)", chrono_date);
    b.m(CHRONOLOGY, "ChronoLocalDate date(TemporalAccessor)", chrono_date_from);
    b.m(CHRONOLOGY, "ChronoLocalDate dateEpochDay(long)", chrono_date_epoch_day);
    b.m(CHRONOLOGY, "ChronoLocalDate dateYearDay(int,int)", chrono_date_year_day);
    b.m(CHRONOLOGY, "ChronoLocalDateTime localDateTime(TemporalAccessor)", chrono_local_date_time);
    b.m(CHRONOLOGY, "ChronoZonedDateTime zonedDateTime(TemporalAccessor)", chrono_zoned_date_time);
    b.m(CHRONOLOGY, "ChronoZonedDateTime zonedDateTime(Instant,ZoneId)", chrono_zoned_date_time2);
    b.m(CHRONOLOGY, "ChronoPeriod period(int,int,int)", chrono_period);
    b.s(CHRONOLOGY, "Chronology of(String)", chrono_of);
    b.s(CHRONOLOGY, "Chronology ofLocale(Locale)", chrono_of_locale);
    b.s(CHRONOLOGY, "Chronology from(TemporalAccessor)", chrono_from);
    b.sf(ISO_CHRONOLOGY, "IsoChronology INSTANCE", iso_instance);
    b.m(ISO_CHRONOLOGY, "LocalDate date(int,int,int)", chrono_date);
    b.m(ISO_CHRONOLOGY, "LocalDate date(TemporalAccessor)", chrono_date_from);
    b.m(ISO_CHRONOLOGY, "LocalDate dateEpochDay(long)", chrono_date_epoch_day);
    b.m(ISO_CHRONOLOGY, "LocalDate dateYearDay(int,int)", chrono_date_year_day);
    b.m(ISO_CHRONOLOGY, "LocalDateTime localDateTime(TemporalAccessor)", chrono_local_date_time);
    b.m(ISO_CHRONOLOGY, "ZonedDateTime zonedDateTime(TemporalAccessor)", chrono_zoned_date_time);
    b.m(ISO_CHRONOLOGY, "ZonedDateTime zonedDateTime(Instant,ZoneId)", chrono_zoned_date_time2);
    b.m(ISO_CHRONOLOGY, "Period period(int,int,int)", chrono_period);
}
fn to_local_date_v(_: &mut Vm, a: &mut [Value]) -> VR {
    r(TimeVal::LocalDate(to_local_date(tv(&a[0])?)?))
}
fn to_local_time_v(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::LOCAL_TIME, tv(&a[0])?)?)
}
fn to_local_date_time_v(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::LOCAL_DATE_TIME, tv(&a[0])?)?)
}
fn to_offset_date_time_v(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::OFFSET_DATE_TIME, tv(&a[0])?)?)
}
fn to_offset_time_v(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::OFFSET_TIME, tv(&a[0])?)?)
}
fn to_zoned_v(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::ZONED_DATE_TIME, tv(&a[0])?)?)
}
fn to_instant_v(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = tv(&a[0])?.instant().ok_or_else(|| unsupported_field(Field::InstantSeconds))?;
    r(TimeVal::Instant { secs: s, nanos: n })
}
fn to_epoch_second_v(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(tv(&a[0])?.get_long(Field::InstantSeconds)?))
}
fn ldt_to_epoch_second(_: &mut Vm, a: &mut [Value]) -> VR {
    let o = offset_arg(&a[1])?;
    let TimeVal::LocalDateTime(d, t) = tv(&a[0])? else { return Err(unsupported_field(Field::InstantSeconds)) };
    Ok(Value::Long(epoch_of_local(*d, *t, o)))
}
fn ldt_to_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let o = offset_arg(&a[1])?;
    let TimeVal::LocalDateTime(d, t) = tv(&a[0])? else { return Err(unsupported_field(Field::InstantSeconds)) };
    r(TimeVal::Instant { secs: epoch_of_local(*d, *t, o), nanos: t.nano() as i32 })
}
fn to_epoch_day_v(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(to_local_date(tv(&a[0])?)?.epoch_day()))
}
fn is_leap_year_v(_: &mut Vm, a: &mut [Value]) -> VR {
    let y = tv(&a[0])?.get_long(Field::Year)?;
    Ok(Value::Bool(is_leap(y)))
}
fn length_of_month_v(_: &mut Vm, a: &mut [Value]) -> VR {
    let t = tv(&a[0])?;
    let (y, m) = (t.get_long(Field::Year)?, t.get_long(Field::MonthOfYear)?);
    Ok(Value::Int(month_len(y, m as u8) as i32))
}
fn length_of_year_v(_: &mut Vm, a: &mut [Value]) -> VR {
    let y = tv(&a[0])?.get_long(Field::Year)?;
    Ok(Value::Int(if is_leap(y) { 366 } else { 365 }))
}
fn g_era_obj(_: &mut Vm, a: &mut [Value]) -> VR {
    let e = tv(&a[0])?.get_long(Field::Era)?;
    Ok(Value::str(if e == 1 { "CE" } else { "BCE" }))
}

// ------------------------------------------------------------------------------ plus/minus/with shortcuts

macro_rules! plus_fn {
    ($name:ident, $u:expr, $sign:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let n = long(&a[1]);
            let n = if $sign < 0 { n.checked_neg().ok_or_else(|| arith("long overflow"))? } else { n };
            let t = tv(&a[0])?;
            let date = matches!(t, TimeVal::LocalDate(_)) && $u == Unit::Days;
            r(t.plus_unit(n, $u).map_err(|e| {
                if date && e.frames.len() == 2 && e.message.as_deref().is_some_and(|m| m.starts_with("Invalid value for Year ")) {
                    jt(e, &["LocalDate.plusDays(LocalDate.java:1388)"])
                } else if date && e.frames.len() == 2 {
                    let mut f = vec!["LocalDate.ofEpochDay(LocalDate.java:365)", "LocalDate.plusDays(LocalDate.java:1395)"];
                    if $sign < 0 {
                        f.push("LocalDate.minusDays(LocalDate.java:1536)");
                    }
                    jt(e, &f)
                } else {
                    e
                }
            })?)
        }
    };
}
plus_fn!(plus_years, Unit::Years, 1);
plus_fn!(plus_months, Unit::Months, 1);
plus_fn!(plus_weeks, Unit::Weeks, 1);
plus_fn!(plus_days, Unit::Days, 1);
plus_fn!(plus_hours, Unit::Hours, 1);
plus_fn!(plus_minutes, Unit::Minutes, 1);
plus_fn!(plus_seconds, Unit::Seconds, 1);
plus_fn!(plus_millis, Unit::Millis, 1);
plus_fn!(plus_nanos, Unit::Nanos, 1);
plus_fn!(minus_years, Unit::Years, -1);
plus_fn!(minus_months, Unit::Months, -1);
plus_fn!(minus_weeks, Unit::Weeks, -1);
plus_fn!(minus_days, Unit::Days, -1);
plus_fn!(minus_hours, Unit::Hours, -1);
plus_fn!(minus_minutes, Unit::Minutes, -1);
plus_fn!(minus_seconds, Unit::Seconds, -1);
plus_fn!(minus_millis, Unit::Millis, -1);
plus_fn!(minus_nanos, Unit::Nanos, -1);

macro_rules! with_fn {
    ($name:ident, $f:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            r(tv(&a[0])?.with_field($f, long(&a[1]))?)
        }
    };
}
with_fn!(with_year, Field::Year);
with_fn!(with_month, Field::MonthOfYear);
with_fn!(with_day_of_month, Field::DayOfMonth);
with_fn!(with_day_of_year, Field::DayOfYear);
with_fn!(with_hour, Field::HourOfDay);
with_fn!(with_minute, Field::MinuteOfHour);
with_fn!(with_second, Field::SecondOfMinute);
with_fn!(with_nano, Field::NanoOfSecond);

// ------------------------------------------------------------------------------ Instant

fn inst(secs: i64, nanos: i32) -> VR {
    check_instant(secs)?;
    r(TimeVal::Instant { secs, nanos })
}
fn instant_epoch(_: &mut Vm, _: &mut [Value]) -> VR {
    inst(0, 0)
}
fn instant_max(_: &mut Vm, _: &mut [Value]) -> VR {
    inst(31556889864403199, 999_999_999)
}
fn instant_min(_: &mut Vm, _: &mut [Value]) -> VR {
    inst(-31557014167219200, 0)
}
fn instant_of_epoch_second(_: &mut Vm, a: &mut [Value]) -> VR {
    inst(l(a, 0), 0).map_err(|t| jt(t, &["Instant.create(Instant.java:414)", "Instant.ofEpochSecond(Instant.java:308)"]))
}
fn instant_of_epoch_second2(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = plus_secs_nanos(l(a, 0), 0, 0, l(a, 1))?;
    inst(s, n)
}
fn instant_of_epoch_milli(_: &mut Vm, a: &mut [Value]) -> VR {
    let ms = l(a, 0);
    inst(ms.div_euclid(1000), (ms.rem_euclid(1000) * 1_000_000) as i32)
}
fn instant_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = parse_instant(&text_arg(&a[0])?).map_err(|t| jt(parse_frames(t), &["Instant.parse(Instant.java:398)"]))?;
    inst(s, n)
}
fn instant_epoch_second(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(tv(&a[0])?.get_long(Field::InstantSeconds)?))
}
fn instant_at_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = zone_arg(&a[1])?;
    let (s, n) = tv(&a[0])?.instant().unwrap_or((0, 0));
    r(zoned_of_instant(s, n, z))
}
fn instant_at_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let o = offset_arg(&a[1])?;
    let (s, n) = tv(&a[0])?.instant().unwrap_or((0, 0));
    let (d, t) = local_of_epoch(s, n, o);
    r(TimeVal::OffsetDateTime { d, t, offset: o })
}
fn instant_to_epoch_milli(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = tv(&a[0])?.instant().unwrap_or((0, 0));
    let ms = s.checked_mul(1000).and_then(|x| x.checked_add(n as i64 / 1_000_000)).ok_or_else(|| arith("long overflow"))?;
    Ok(Value::Long(ms))
}

// ------------------------------------------------------------------------------ Duration

fn dur(secs: i64, nanos: i32) -> VR {
    r(TimeVal::Duration { secs, nanos })
}
fn dur_of(v: &Value) -> Result<(i64, i32), Thrown> {
    match arg_tv(v, "duration")? {
        TimeVal::Duration { secs, nanos } => Ok((*secs, *nanos)),
        _ => Err(crate::vm::cce("not a Duration")),
    }
}
fn dur_total(d: (i64, i32)) -> i128 {
    d.0 as i128 * NANOS_PER_SECOND as i128 + d.1 as i128
}
fn dur_from_total(t: i128) -> VR {
    let s = t.div_euclid(NANOS_PER_SECOND as i128);
    let n = t.rem_euclid(NANOS_PER_SECOND as i128);
    let s = i64::try_from(s).map_err(|_| arith("long overflow"))?;
    dur(s, n as i32)
}
fn d_zero(_: &mut Vm, _: &mut [Value]) -> VR {
    dur(0, 0)
}
fn d_of_days(_: &mut Vm, a: &mut [Value]) -> VR {
    dur(l(a, 0).checked_mul(86400).ok_or_else(|| arith("long overflow"))?, 0)
}
fn d_of_hours(_: &mut Vm, a: &mut [Value]) -> VR {
    dur(l(a, 0).checked_mul(3600).ok_or_else(|| arith("long overflow"))?, 0)
}
fn d_of_minutes(_: &mut Vm, a: &mut [Value]) -> VR {
    dur(l(a, 0).checked_mul(60).ok_or_else(|| arith("long overflow"))?, 0)
}
fn d_of_seconds(_: &mut Vm, a: &mut [Value]) -> VR {
    dur(l(a, 0), 0)
}
fn d_of_seconds2(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = plus_secs_nanos(l(a, 0), 0, 0, l(a, 1))?;
    dur(s, n)
}
fn d_of_millis(_: &mut Vm, a: &mut [Value]) -> VR {
    let ms = l(a, 0);
    dur(ms.div_euclid(1000), (ms.rem_euclid(1000) * 1_000_000) as i32)
}
fn d_of_nanos(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = l(a, 0);
    dur(n.div_euclid(NANOS_PER_SECOND), n.rem_euclid(NANOS_PER_SECOND) as i32)
}
fn d_of_unit(_: &mut Vm, a: &mut [Value]) -> VR {
    let u = unit_of(&a[1])?;
    if u.is_date_based() && u != Unit::Days {
        return Err(unsupported("Unit must not have an estimated duration".to_string()));
    }
    dur_from_total(l(a, 0) as i128 * u.nanos() as i128)
}
fn d_between(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = arg_tv(&a[0], "startInclusive")?.clone();
    let e = arg_tv(&a[1], "endExclusive")?.clone();
    let n = s.until(&e, Unit::Nanos);
    match n {
        Ok(n) => dur_from_total(n as i128),
        Err(_) => {
            let date = matches!(s, TimeVal::LocalDate(_));
            let secs = s.until(&e, Unit::Seconds).map_err(|t| {
                if date && t.class == cls::UNSUPPORTED_TEMPORAL_TYPE_EXCEPTION {
                    jt(t, &["LocalDate.until(LocalDate.java:1656)", "Duration.between(Duration.java:520)"])
                } else {
                    t
                }
            })?;
            dur(secs, 0)
        }
    }
}
fn d_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = parse_duration(&text_arg(&a[0])?).map_err(|t| jt(t, &["Duration.parse(Duration.java:450)"]))?;
    dur(s, n)
}
fn d_from(_: &mut Vm, a: &mut [Value]) -> VR {
    match arg_tv(&a[0], "amount")? {
        TimeVal::Duration { secs, nanos } => dur(*secs, *nanos),
        TimeVal::Period { y, m, d } => {
            if *y != 0 || *m != 0 {
                return Err(unsupported("Unit must not have an estimated duration".to_string()));
            }
            dur(*d as i64 * 86400, 0)
        }
        _ => Err(crate::vm::cce("not a TemporalAmount")),
    }
}
fn d_get_seconds(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(dur_of(&a[0])?.0))
}
fn d_get_nano(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(dur_of(&a[0])?.1))
}
fn d_is_negative(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(dur_of(&a[0])?.0 < 0))
}
fn d_is_zero(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = dur_of(&a[0])?;
    Ok(Value::Bool(d.0 == 0 && d.1 == 0))
}
fn d_abs(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = dur_of(&a[0])?;
    dur_from_total(dur_total(d).abs())
}
fn d_negated(_: &mut Vm, a: &mut [Value]) -> VR {
    dur_from_total(-dur_total(dur_of(&a[0])?))
}
fn d_plus(_: &mut Vm, a: &mut [Value]) -> VR {
    dur_from_total(dur_total(dur_of(&a[0])?) + dur_total(dur_of(&a[1])?))
}
fn d_minus(_: &mut Vm, a: &mut [Value]) -> VR {
    dur_from_total(dur_total(dur_of(&a[0])?) - dur_total(dur_of(&a[1])?))
}
fn d_plus_unit(_: &mut Vm, a: &mut [Value]) -> VR {
    let u = unit_of(&a[2])?;
    if u.is_date_based() && u != Unit::Days {
        return Err(unsupported("Unit must not have an estimated duration".to_string()));
    }
    dur_from_total(dur_total(dur_of(&a[0])?) + l(a, 1) as i128 * u.nanos() as i128)
}
fn d_minus_unit(_: &mut Vm, a: &mut [Value]) -> VR {
    let u = unit_of(&a[2])?;
    if u.is_date_based() && u != Unit::Days {
        return Err(unsupported("Unit must not have an estimated duration".to_string()));
    }
    dur_from_total(dur_total(dur_of(&a[0])?) - l(a, 1) as i128 * u.nanos() as i128)
}
macro_rules! dur_plus {
    ($name:ident, $mul:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            dur_from_total(dur_total(dur_of(&a[0])?) + l(a, 1) as i128 * $mul as i128)
        }
    };
}
dur_plus!(d_plus_days, 86400 * NANOS_PER_SECOND);
dur_plus!(d_plus_hours, 3600 * NANOS_PER_SECOND);
dur_plus!(d_plus_minutes, 60 * NANOS_PER_SECOND);
fn d_plus_seconds(_: &mut Vm, a: &mut [Value]) -> VR {
    let (secs, nanos) = dur_of(&a[0])?;
    match secs.checked_add(l(a, 1)) {
        Some(s) => dur(s, nanos),
        None => Err(jt(arith("long overflow"), &["java.lang.Math.addExact(Math.java:935)", "Duration.plus(Duration.java:878)", "Duration.plusSeconds(Duration.java:835)"])),
    }
}
dur_plus!(d_plus_millis, 1_000_000);
dur_plus!(d_plus_nanos, 1);
dur_plus!(d_minus_days, -86400 * NANOS_PER_SECOND);
dur_plus!(d_minus_hours, -3600 * NANOS_PER_SECOND);
dur_plus!(d_minus_minutes, -60 * NANOS_PER_SECOND);
dur_plus!(d_minus_seconds, -NANOS_PER_SECOND);
dur_plus!(d_minus_millis, -1_000_000);
dur_plus!(d_minus_nanos, -1);
fn d_multiplied(_: &mut Vm, a: &mut [Value]) -> VR {
    dur_from_total(dur_total(dur_of(&a[0])?).checked_mul(l(a, 1) as i128).ok_or_else(|| arith("Exceeds capacity of Duration"))?)
}
fn d_divided(_: &mut Vm, a: &mut [Value]) -> VR {
    let div = l(a, 1);
    if div == 0 {
        return Err(arith("Cannot divide by zero"));
    }
    dur_from_total(dur_total(dur_of(&a[0])?) / div as i128)
}
fn d_to_days(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(dur_of(&a[0])?.0 / 86400))
}
fn d_to_hours(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(dur_of(&a[0])?.0 / 3600))
}
fn d_to_minutes(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(dur_of(&a[0])?.0 / 60))
}
fn d_to_millis(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = dur_of(&a[0])?;
    let (s, n) = if s < 0 { (s + 1, n as i64 - NANOS_PER_SECOND) } else { (s, n as i64) };
    let ms = s.checked_mul(1000).and_then(|x| x.checked_add(n / 1_000_000)).ok_or_else(|| arith("long overflow"))?;
    Ok(Value::Long(ms))
}
fn d_to_nanos(_: &mut Vm, a: &mut [Value]) -> VR {
    let t = dur_total(dur_of(&a[0])?);
    Ok(Value::Long(i64::try_from(t).map_err(|_| arith("long overflow"))?))
}
fn d_with_seconds(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, n) = dur_of(&a[0])?;
    dur(l(a, 1), n)
}
fn d_with_nanos(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, _) = dur_of(&a[0])?;
    let n = check(Field::NanoOfSecond, i(a, 1))?;
    dur(s, n as i32)
}
fn d_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(dur_total(dur_of(&a[0])?).cmp(&dur_total(dur_of(&a[1])?)) as i32))
}
fn amount_get(_: &mut Vm, a: &mut [Value]) -> VR {
    let u = unit_of(&a[1])?;
    match tv(&a[0])? {
        TimeVal::Duration { secs, nanos } => match u {
            Unit::Seconds => Ok(Value::Long(*secs)),
            Unit::Nanos => Ok(Value::Long(*nanos as i64)),
            _ => Err(unsupported_unit(u)),
        },
        TimeVal::Period { y, m, d } => match u {
            Unit::Years => Ok(Value::Long(*y as i64)),
            Unit::Months => Ok(Value::Long(*m as i64)),
            Unit::Days => Ok(Value::Long(*d as i64)),
            _ => Err(unsupported_unit(u)),
        },
        _ => Err(crate::vm::cce("not a TemporalAmount")),
    }
}
fn amount_units(_: &mut Vm, a: &mut [Value]) -> VR {
    let units = match tv(&a[0])? {
        TimeVal::Duration { .. } => vec![Unit::Seconds, Unit::Nanos],
        _ => vec![Unit::Years, Unit::Months, Unit::Days],
    };
    let mut l = JList::new(cls::IMMUTABLE_LIST, units.into_iter().map(|u| time_value(TimeVal::TUnit(u))).collect());
    l.class = cls::IMMUTABLE_LIST;
    Ok(Value::List(Rc::new(l)))
}
fn amount_add_to(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut b = [a[1].clone(), a[0].clone()];
    t_plus_amount(vm, &mut b)
}
fn amount_subtract_from(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut b = [a[1].clone(), a[0].clone()];
    t_minus_amount(vm, &mut b)
}

// ------------------------------------------------------------------------------ Period

fn per(y: i64, m: i64, d: i64) -> VR {
    let fit = |x: i64| i32::try_from(x).map_err(|_| arith("integer overflow"));
    r(TimeVal::Period { y: fit(y)?, m: fit(m)?, d: fit(d)? })
}
fn per_of(v: &Value) -> Result<(i64, i64, i64), Thrown> {
    match arg_tv(v, "period")? {
        TimeVal::Period { y, m, d } => Ok((*y as i64, *m as i64, *d as i64)),
        _ => Err(crate::vm::cce("not a Period")),
    }
}
fn p_zero(_: &mut Vm, _: &mut [Value]) -> VR {
    per(0, 0, 0)
}
fn p_of(_: &mut Vm, a: &mut [Value]) -> VR {
    per(i(a, 0), i(a, 1), i(a, 2))
}
fn p_of_years(_: &mut Vm, a: &mut [Value]) -> VR {
    per(i(a, 0), 0, 0)
}
fn p_of_months(_: &mut Vm, a: &mut [Value]) -> VR {
    per(0, i(a, 0), 0)
}
fn p_of_weeks(_: &mut Vm, a: &mut [Value]) -> VR {
    per(0, 0, i(a, 0) * 7)
}
fn p_of_days(_: &mut Vm, a: &mut [Value]) -> VR {
    per(0, 0, i(a, 0))
}
fn p_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = parse_period(&text_arg(&a[0])?)?;
    per(y as i64, m as i64, d as i64)
}
fn p_between(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = to_local_date(arg_tv(&a[0], "startDateInclusive")?)?;
    let e = to_local_date(arg_tv(&a[1], "endDateExclusive")?)?;
    let (y, m, d) = period_between(s, e);
    per(y, m, d)
}
/// `LocalDate.until(ChronoLocalDate)`.
fn period_between(s: Date, e: Date) -> (i64, i64, i64) {
    let mut total_months = e.proleptic_month() - s.proleptic_month();
    let mut days = e.d as i64 - s.d as i64;
    if total_months > 0 && days < 0 {
        total_months -= 1;
        let calc = s.plus_months(total_months).unwrap_or(s);
        days = e.epoch_day() - calc.epoch_day();
    } else if total_months < 0 && days > 0 {
        total_months += 1;
        days -= month_len(e.y as i64, e.m) as i64;
    }
    (total_months / 12, total_months % 12, days)
}
fn p_from(_: &mut Vm, a: &mut [Value]) -> VR {
    match arg_tv(&a[0], "amount")? {
        TimeVal::Period { y, m, d } => per(*y as i64, *m as i64, *d as i64),
        TimeVal::Duration { .. } => Err(dte("Unit must be Years, Months or Days, but was Seconds")),
        _ => Err(crate::vm::cce("not a TemporalAmount")),
    }
}
fn p_get_years(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(per_of(&a[0])?.0 as i32))
}
fn p_get_months(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(per_of(&a[0])?.1 as i32))
}
fn p_get_days(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(per_of(&a[0])?.2 as i32))
}
fn p_is_negative(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    Ok(Value::Bool(y < 0 || m < 0 || d < 0))
}
fn p_is_zero(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(per_of(&a[0])? == (0, 0, 0)))
}
fn p_plus(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    let (y2, m2, d2) = per_of(&a[1]).map_err(|_| dte("Chronology mismatch, expected: ISO"))?;
    per(y + y2, m + m2, d + d2)
}
fn p_minus(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    let (y2, m2, d2) = per_of(&a[1]).map_err(|_| dte("Chronology mismatch, expected: ISO"))?;
    per(y - y2, m - m2, d - d2)
}
fn p_plus_years(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    per(y + l(a, 1), m, d)
}
fn p_plus_months(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    per(y, m + l(a, 1), d)
}
fn p_plus_days(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    per(y, m, d + l(a, 1))
}
fn p_minus_years(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    per(y - l(a, 1), m, d)
}
fn p_minus_months(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    per(y, m - l(a, 1), d)
}
fn p_minus_days(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    per(y, m, d - l(a, 1))
}
fn p_multiplied(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    let k = i(a, 1);
    per(y * k, m * k, d * k)
}
fn p_negated(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    per(-y, -m, -d)
}
fn p_normalized(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = per_of(&a[0])?;
    let total = y * 12 + m;
    per(total / 12, total % 12, d)
}
fn p_total_months(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, _) = per_of(&a[0])?;
    Ok(Value::Long(y * 12 + m))
}
fn p_with_days(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, _) = per_of(&a[0])?;
    per(y, m, i(a, 1))
}
fn p_with_months(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, _, d) = per_of(&a[0])?;
    per(y, i(a, 1), d)
}
fn p_with_years(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, m, d) = per_of(&a[0])?;
    per(i(a, 1), m, d)
}

// ------------------------------------------------------------------------------ LocalDate

fn ld(d: Date) -> VR {
    r(TimeVal::LocalDate(d))
}
fn ld_max(_: &mut Vm, _: &mut [Value]) -> VR {
    ld(Date { y: MAX_YEAR as i32, m: 12, d: 31 })
}
fn ld_min(_: &mut Vm, _: &mut [Value]) -> VR {
    ld(Date { y: MIN_YEAR as i32, m: 1, d: 1 })
}
fn ld_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, month_obj) = match &a[1] {
        Value::Obj(_) => (tv(&a[1])?.get_long(Field::MonthOfYear)?, true),
        v => (int(v) as i64, false),
    };
    ld(Date::of(i(a, 0), m, i(a, 2)).map_err(|t| {
        let msg = t.message.clone().unwrap_or_default();
        let line = if msg.starts_with("Invalid value for Year") {
            if month_obj { 311 } else { 292 }
        } else if msg.starts_with("Invalid value for MonthOfYear") {
            293
        } else if msg.starts_with("Invalid value for DayOfMonth") {
            if month_obj { 312 } else { 294 }
        } else if month_obj {
            313
        } else {
            295
        };
        jt(t, &[&format!("LocalDate.of(LocalDate.java:{line})")])
    })?)
}
fn ld_of_year_day(_: &mut Vm, a: &mut [Value]) -> VR {
    ld(Date::of_year_day(i(a, 0), i(a, 1))?)
}
fn ld_of_epoch_day(_: &mut Vm, a: &mut [Value]) -> VR {
    ld(Date::from_epoch_day(check(Field::EpochDay, l(a, 0))?))
}
fn ld_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    ld(parse_local_date(&text_arg(&a[0])?).map_err(|t| jt(parse_frames(t), &["LocalDate.parse(LocalDate.java:455)", "LocalDate.parse(LocalDate.java:440)"]))?)
}
fn ld_parse_fmt(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let f = format::formatter_of(&a[1])?;
    r(f.parse_to(&text, cls::LOCAL_DATE).map_err(|t| jt(parse_frames(t), &["LocalDate.parse(LocalDate.java:455)"]))?)
}
fn ld_at_start_of_day(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = to_local_date(tv(&a[0])?)?;
    r(TimeVal::LocalDateTime(d, Time(0)))
}
fn ld_at_start_of_day_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = to_local_date(tv(&a[0])?)?;
    let z = zone_arg(&a[1])?;
    r(zoned(d, Time(0), z, None))
}
fn ld_at_time(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = to_local_date(tv(&a[0])?)?;
    let t = match arg_tv(&a[1], "time")? {
        TimeVal::LocalTime(t) => *t,
        _ => return Err(crate::vm::cce("not a LocalTime")),
    };
    r(TimeVal::LocalDateTime(d, t))
}
fn ld_at_time2(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = to_local_date(tv(&a[0])?)?;
    r(TimeVal::LocalDateTime(d, Time::of(i(a, 1), i(a, 2), 0, 0)?))
}
fn ld_at_time3(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = to_local_date(tv(&a[0])?)?;
    r(TimeVal::LocalDateTime(d, Time::of(i(a, 1), i(a, 2), i(a, 3), 0)?))
}
fn ld_at_time4(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = to_local_date(tv(&a[0])?)?;
    r(TimeVal::LocalDateTime(d, Time::of(i(a, 1), i(a, 2), i(a, 3), i(a, 4))?))
}
fn ld_until_period(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = to_local_date(tv(&a[0])?)?;
    let e = to_local_date(arg_tv(&a[1], "endDateExclusive")?)?;
    let (y, m, d) = period_between(s, e);
    per(y, m, d)
}

// ------------------------------------------------------------------------------ LocalTime

fn lt(t: Time) -> VR {
    r(TimeVal::LocalTime(t))
}
fn lt_max(_: &mut Vm, _: &mut [Value]) -> VR {
    lt(Time(NANOS_PER_DAY - 1))
}
fn lt_midnight(_: &mut Vm, _: &mut [Value]) -> VR {
    lt(Time(0))
}
fn lt_noon(_: &mut Vm, _: &mut [Value]) -> VR {
    lt(Time(12 * 3600 * NANOS_PER_SECOND))
}
fn lt_of2(_: &mut Vm, a: &mut [Value]) -> VR {
    lt(Time::of(i(a, 0), i(a, 1), 0, 0).map_err(|t| {
        let line = if t.message.as_deref().unwrap_or("").contains("HourOfDay") { 308 } else { 309 };
        jt(t, &[&format!("LocalTime.of(LocalTime.java:{line})")])
    })?)
}
fn lt_of3(_: &mut Vm, a: &mut [Value]) -> VR {
    lt(Time::of(i(a, 0), i(a, 1), i(a, 2), 0)?)
}
fn lt_of4(_: &mut Vm, a: &mut [Value]) -> VR {
    lt(Time::of(i(a, 0), i(a, 1), i(a, 2), i(a, 3))?)
}
fn lt_of_nano_of_day(_: &mut Vm, a: &mut [Value]) -> VR {
    lt(Time(check(Field::NanoOfDay, l(a, 0))?))
}
fn lt_of_second_of_day(_: &mut Vm, a: &mut [Value]) -> VR {
    lt(Time(check(Field::SecondOfDay, l(a, 0))? * NANOS_PER_SECOND))
}
fn lt_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    lt(parse_local_time(&text_arg(&a[0])?)?)
}
fn lt_parse_fmt(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let f = format::formatter_of(&a[1])?;
    r(f.parse_to(&text, cls::LOCAL_TIME)?)
}
fn lt_at_date(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::LocalTime(t) = tv(&a[0])? else { return Err(crate::vm::cce("not a LocalTime")) };
    let d = to_local_date(arg_tv(&a[1], "date")?)?;
    r(TimeVal::LocalDateTime(d, *t))
}
fn lt_at_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::LocalTime(t) = tv(&a[0])? else { return Err(crate::vm::cce("not a LocalTime")) };
    r(TimeVal::OffsetTime { t: *t, offset: offset_arg(&a[1])? })
}
fn lt_to_nano_of_day(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(tv(&a[0])?.get_long(Field::NanoOfDay)?))
}
fn lt_to_second_of_day(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(tv(&a[0])?.get_long(Field::SecondOfDay)? as i32))
}

// ------------------------------------------------------------------------------ LocalDateTime

fn ldt(d: Date, t: Time) -> VR {
    r(TimeVal::LocalDateTime(d, t))
}
fn ldt_min(_: &mut Vm, _: &mut [Value]) -> VR {
    ldt(Date { y: MIN_YEAR as i32, m: 1, d: 1 }, Time(0))
}
fn ldt_max(_: &mut Vm, _: &mut [Value]) -> VR {
    ldt(Date { y: MAX_YEAR as i32, m: 12, d: 31 }, Time(NANOS_PER_DAY - 1))
}
fn ldt_of_dt(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = to_local_date(arg_tv(&a[0], "date")?)?;
    let t = match arg_tv(&a[1], "time")? {
        TimeVal::LocalTime(t) => *t,
        _ => return Err(crate::vm::cce("not a LocalTime")),
    };
    ldt(d, t)
}
fn month_arg(v: &Value) -> Result<i64, Thrown> {
    match v {
        Value::Obj(_) => tv(v)?.get_long(Field::MonthOfYear),
        v => Ok(int(v) as i64),
    }
}
fn ldt_of5(_: &mut Vm, a: &mut [Value]) -> VR {
    ldt(Date::of(i(a, 0), month_arg(&a[1])?, i(a, 2))?, Time::of(i(a, 3), i(a, 4), 0, 0)?)
}
fn ldt_of6(_: &mut Vm, a: &mut [Value]) -> VR {
    ldt(Date::of(i(a, 0), month_arg(&a[1])?, i(a, 2))?, Time::of(i(a, 3), i(a, 4), i(a, 5), 0)?)
}
fn ldt_of7(_: &mut Vm, a: &mut [Value]) -> VR {
    ldt(Date::of(i(a, 0), month_arg(&a[1])?, i(a, 2))?, Time::of(i(a, 3), i(a, 4), i(a, 5), i(a, 6))?)
}
fn ldt_of_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = arg_tv(&a[0], "instant")?.instant().ok_or_else(|| crate::vm::cce("not an Instant"))?;
    let z = zone_arg(&a[1])?;
    let off = z.offset_at(s);
    let (d, t) = local_of_epoch(s, n, off);
    ldt(d, t)
}
fn ldt_of_epoch_second(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = check(Field::NanoOfSecond, i(a, 1))?;
    let o = offset_arg(&a[2])?;
    let (d, t) = local_of_epoch(l(a, 0), n as i32, o);
    ldt(d, t)
}
fn ldt_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let (d, t) = parse_local_date_time(&text_arg(&a[0])?)
        .map_err(|t| jt(parse_frames(t), &["LocalDateTime.parse(LocalDateTime.java:496)", "LocalDateTime.parse(LocalDateTime.java:481)"]))?;
    ldt(d, t)
}
fn ldt_parse_fmt(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let f = format::formatter_of(&a[1])?;
    r(f.parse_to(&text, cls::LOCAL_DATE_TIME).map_err(|t| jt(parse_frames(t), &["LocalDateTime.parse(LocalDateTime.java:496)"]))?)
}
fn ldt_at_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::LocalDateTime(d, t) = tv(&a[0])? else { return Err(crate::vm::cce("not a LocalDateTime")) };
    r(zoned(*d, *t, zone_arg(&a[1])?, None))
}
fn ldt_at_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::LocalDateTime(d, t) = tv(&a[0])? else { return Err(crate::vm::cce("not a LocalDateTime")) };
    r(TimeVal::OffsetDateTime { d: *d, t: *t, offset: offset_arg(&a[1])? })
}

// ------------------------------------------------------------------------------ ZonedDateTime

fn zdt_of_dt(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = to_local_date(arg_tv(&a[0], "date")?)?;
    let t = match arg_tv(&a[1], "time")? {
        TimeVal::LocalTime(t) => *t,
        _ => return Err(crate::vm::cce("not a LocalTime")),
    };
    r(zoned(d, t, zone_arg(&a[2])?, None))
}
fn zdt_of_ldt(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::LocalDateTime(d, t) = arg_tv(&a[0], "localDateTime")? else { return Err(crate::vm::cce("not a LocalDateTime")) };
    r(zoned(*d, *t, zone_arg(&a[1])?, None))
}
fn zdt_of8(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = Date::of(i(a, 0), i(a, 1), i(a, 2))?;
    let t = Time::of(i(a, 3), i(a, 4), i(a, 5), i(a, 6))?;
    r(zoned(d, t, zone_arg(&a[7])?, None))
}
fn zdt_of_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = arg_tv(&a[0], "instant")?.instant().ok_or_else(|| crate::vm::cce("not an Instant"))?;
    r(zoned_of_instant(s, n, zone_arg(&a[1])?))
}
fn zdt_of_instant3(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::LocalDateTime(d, t) = arg_tv(&a[0], "localDateTime")? else { return Err(crate::vm::cce("not a LocalDateTime")) };
    let o = offset_arg(&a[1])?;
    let z = zone_arg(&a[2])?;
    r(zoned_of_instant(epoch_of_local(*d, *t, o), t.nano() as i32, z))
}
fn zdt_of_local(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::LocalDateTime(d, t) = arg_tv(&a[0], "localDateTime")? else { return Err(crate::vm::cce("not a LocalDateTime")) };
    let z = zone_arg(&a[1])?;
    let pref = if a[2].is_null() { None } else { Some(offset_arg(&a[2])?) };
    r(zoned(*d, *t, z, pref))
}
fn zdt_of_strict(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::LocalDateTime(d, t) = arg_tv(&a[0], "localDateTime")? else { return Err(crate::vm::cce("not a LocalDateTime")) };
    let o = offset_arg(&a[1])?;
    let z = zone_arg(&a[2])?;
    let valid = z.valid_offsets(*d, *t);
    if !valid.contains(&o) {
        if valid.is_empty() {
            return Err(dte(format!(
                "LocalDateTime '{}' does not exist in zone '{}' due to a gap in the local time-line, typically caused by daylight savings",
                TimeVal::LocalDateTime(*d, *t).to_java_string(),
                z.id()
            )));
        }
        return Err(dte(format!(
            "ZoneOffset '{}' is not valid for LocalDateTime '{}' in zone '{}'",
            offset_id(o),
            TimeVal::LocalDateTime(*d, *t).to_java_string(),
            z.id()
        )));
    }
    r(TimeVal::Zoned { d: *d, t: *t, offset: o, zone: z })
}
fn zdt_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    r(parse_zoned(&text_arg(&a[0])?).map_err(|t| jt(parse_frames(t), &["ZonedDateTime.parse(ZonedDateTime.java:602)", "ZonedDateTime.parse(ZonedDateTime.java:587)"]))?)
}
fn zdt_parse_fmt(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let f = format::formatter_of(&a[1])?;
    r(f.parse_to(&text, cls::ZONED_DATE_TIME).map_err(|t| jt(parse_frames(t), &["ZonedDateTime.parse(ZonedDateTime.java:602)"]))?)
}
fn zdt_with_zone_same_local(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::Zoned { d, t, offset, .. } = tv(&a[0])? else { return Err(crate::vm::cce("not a ZonedDateTime")) };
    r(zoned(*d, *t, zone_arg(&a[1])?, Some(*offset)))
}
fn zdt_with_zone_same_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = tv(&a[0])?.instant().ok_or_else(|| crate::vm::cce("not a ZonedDateTime"))?;
    r(zoned_of_instant(s, n, zone_arg(&a[1])?))
}
fn zdt_with_fixed_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::Zoned { d, t, offset, .. } = tv(&a[0])? else { return Err(crate::vm::cce("not a ZonedDateTime")) };
    r(TimeVal::Zoned { d: *d, t: *t, offset: *offset, zone: Zone::Offset(*offset) })
}
fn zdt_with_earlier(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::Zoned { d, t, zone, .. } = tv(&a[0])? else { return Err(crate::vm::cce("not a ZonedDateTime")) };
    let offs = zone.valid_offsets(*d, *t);
    let mut out = tv(&a[0])?.clone();
    if offs.len() == 2 {
        out = TimeVal::Zoned { d: *d, t: *t, offset: offs[0], zone: zone.clone() };
    }
    r(out)
}
fn zdt_with_later(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::Zoned { d, t, zone, .. } = tv(&a[0])? else { return Err(crate::vm::cce("not a ZonedDateTime")) };
    let offs = zone.valid_offsets(*d, *t);
    let mut out = tv(&a[0])?.clone();
    if offs.len() == 2 {
        out = TimeVal::Zoned { d: *d, t: *t, offset: offs[1], zone: zone.clone() };
    }
    r(out)
}

// ------------------------------------------------------------------------------ OffsetDateTime / OffsetTime

fn odt_of3(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = to_local_date(arg_tv(&a[0], "date")?)?;
    let TimeVal::LocalTime(t) = arg_tv(&a[1], "time")? else { return Err(crate::vm::cce("not a LocalTime")) };
    r(TimeVal::OffsetDateTime { d, t: *t, offset: offset_arg(&a[2])? })
}
fn odt_of2(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::LocalDateTime(d, t) = arg_tv(&a[0], "dateTime")? else { return Err(crate::vm::cce("not a LocalDateTime")) };
    r(TimeVal::OffsetDateTime { d: *d, t: *t, offset: offset_arg(&a[1])? })
}
fn odt_of8(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = Date::of(i(a, 0), i(a, 1), i(a, 2))?;
    let t = Time::of(i(a, 3), i(a, 4), i(a, 5), i(a, 6))?;
    r(TimeVal::OffsetDateTime { d, t, offset: offset_arg(&a[7])? })
}
fn odt_of_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = arg_tv(&a[0], "instant")?.instant().ok_or_else(|| crate::vm::cce("not an Instant"))?;
    let z = zone_arg(&a[1])?;
    let o = z.offset_at(s);
    let (d, t) = local_of_epoch(s, n, o);
    r(TimeVal::OffsetDateTime { d, t, offset: o })
}
fn odt_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let (d, t, o) = parse_offset_date_time(&text_arg(&a[0])?)?;
    r(TimeVal::OffsetDateTime { d, t, offset: o })
}
fn odt_parse_fmt(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let f = format::formatter_of(&a[1])?;
    r(f.parse_to(&text, cls::OFFSET_DATE_TIME)?)
}
fn odt_at_zone_same_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = tv(&a[0])?.instant().ok_or_else(|| crate::vm::cce("not an OffsetDateTime"))?;
    r(zoned_of_instant(s, n, zone_arg(&a[1])?))
}
fn odt_at_zone_similar_local(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::OffsetDateTime { d, t, offset } = tv(&a[0])? else { return Err(crate::vm::cce("not an OffsetDateTime")) };
    r(zoned(*d, *t, zone_arg(&a[1])?, Some(*offset)))
}
fn odt_with_offset_same_local(_: &mut Vm, a: &mut [Value]) -> VR {
    let o = offset_arg(&a[1])?;
    match tv(&a[0])? {
        TimeVal::OffsetDateTime { d, t, .. } => r(TimeVal::OffsetDateTime { d: *d, t: *t, offset: o }),
        TimeVal::OffsetTime { t, .. } => r(TimeVal::OffsetTime { t: *t, offset: o }),
        _ => Err(crate::vm::cce("not an offset temporal")),
    }
}
fn odt_with_offset_same_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let o = offset_arg(&a[1])?;
    match tv(&a[0])? {
        TimeVal::OffsetDateTime { .. } => {
            let (s, n) = tv(&a[0])?.instant().unwrap();
            let (d, t) = local_of_epoch(s, n, o);
            r(TimeVal::OffsetDateTime { d, t, offset: o })
        }
        TimeVal::OffsetTime { t, offset } => {
            let diff = (o - *offset) as i64 * NANOS_PER_SECOND;
            r(TimeVal::OffsetTime { t: Time((t.0 + diff).rem_euclid(NANOS_PER_DAY)), offset: o })
        }
        _ => Err(crate::vm::cce("not an offset temporal")),
    }
}
fn odt_time_line_order(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(crate::func::native(Some(cls::COMPARATOR), |_, a| {
        let x = tv(&a[0])?.instant().unwrap_or((0, 0));
        let y = tv(&a[1])?.instant().unwrap_or((0, 0));
        Ok(Value::Int(x.cmp(&y) as i32))
    }))
}
fn ot_of2(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::LocalTime(t) = arg_tv(&a[0], "time")? else { return Err(crate::vm::cce("not a LocalTime")) };
    r(TimeVal::OffsetTime { t: *t, offset: offset_arg(&a[1])? })
}
fn ot_of5(_: &mut Vm, a: &mut [Value]) -> VR {
    r(TimeVal::OffsetTime { t: Time::of(i(a, 0), i(a, 1), i(a, 2), i(a, 3))?, offset: offset_arg(&a[4])? })
}
fn ot_of_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = arg_tv(&a[0], "instant")?.instant().ok_or_else(|| crate::vm::cce("not an Instant"))?;
    let z = zone_arg(&a[1])?;
    let o = z.offset_at(s);
    let (_, t) = local_of_epoch(s, n, o);
    r(TimeVal::OffsetTime { t, offset: o })
}
fn ot_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let mut p = iso(&text);
    let t = p.time()?;
    let o = p.offset()?;
    p.end()?;
    r(TimeVal::OffsetTime { t, offset: o })
}
fn ot_parse_fmt(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let f = format::formatter_of(&a[1])?;
    r(f.parse_to(&text, cls::OFFSET_TIME)?)
}
fn odt_min(_: &mut Vm, _: &mut [Value]) -> VR {
    r(TimeVal::OffsetDateTime { d: Date { y: MIN_YEAR as i32, m: 1, d: 1 }, t: Time(0), offset: 18 * 3600 })
}
fn odt_max(_: &mut Vm, _: &mut [Value]) -> VR {
    r(TimeVal::OffsetDateTime { d: Date { y: MAX_YEAR as i32, m: 12, d: 31 }, t: Time(NANOS_PER_DAY - 1), offset: -18 * 3600 })
}
fn ot_min(_: &mut Vm, _: &mut [Value]) -> VR {
    r(TimeVal::OffsetTime { t: Time(0), offset: 18 * 3600 })
}
fn ot_max(_: &mut Vm, _: &mut [Value]) -> VR {
    r(TimeVal::OffsetTime { t: Time(NANOS_PER_DAY - 1), offset: -18 * 3600 })
}

// ------------------------------------------------------------------------------ zones

fn zid_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = match &a[0] {
        Value::Null => return Err(npe("zoneId")),
        v => v.to_java_string(),
    };
    r(TimeVal::ZoneId(zone_of(&s).map_err(|t| if t.frames.is_empty() { t } else { jt(t, &["ZoneId.of(ZoneId.java:411)", "ZoneId.of(ZoneId.java:359)"]) })?))
}
fn zid_of_map(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = a[0].to_java_string();
    let mapped = match &a[1] {
        Value::Map(m) => m.get_str(&s).map(|v| v.to_java_string()).unwrap_or(s),
        _ => s,
    };
    r(TimeVal::ZoneId(zone_of(&mapped)?))
}
fn zid_of_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let prefix = a[0].to_java_string();
    let o = offset_arg(&a[1])?;
    if prefix.is_empty() {
        return r(TimeVal::ZoneId(Zone::Offset(o)));
    }
    if !matches!(prefix.as_str(), "GMT" | "UTC" | "UT") {
        return Err(iae_dte(format!("prefix should be GMT, UTC or UT, is: {prefix}")));
    }
    let id = if o == 0 { prefix.clone() } else { format!("{prefix}{}", offset_id(o)) };
    r(TimeVal::ZoneId(Zone::Region(Arc::from(id.as_str()), None, o)))
}
fn iae_dte(msg: String) -> Thrown {
    Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(msg))
}
fn zid_from(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::ZONE_ID, arg_tv(&a[0], "temporal")?)?)
}
fn zid_get_id(_: &mut Vm, a: &mut [Value]) -> VR {
    match tv(&a[0])? {
        TimeVal::ZoneId(z) => Ok(Value::string(z.id())),
        _ => Err(crate::vm::cce("not a ZoneId")),
    }
}
fn zid_normalized(_: &mut Vm, a: &mut [Value]) -> VR {
    match tv(&a[0])? {
        TimeVal::ZoneId(z) => {
            if z.is_fixed() {
                r(TimeVal::ZoneId(Zone::Offset(z.offset_at(0))))
            } else {
                r(TimeVal::ZoneId(z.clone()))
            }
        }
        _ => Err(crate::vm::cce("not a ZoneId")),
    }
}
fn zid_system_default(_: &mut Vm, _: &mut [Value]) -> VR {
    r(TimeVal::ZoneId(Zone::Region(Arc::from("UTC"), None, 0)))
}
fn zid_available(_: &mut Vm, _: &mut [Value]) -> VR {
    let m = JMap::new(cls::HASH_SET);
    for tz in chrono_tz::TZ_VARIANTS.iter() {
        m.insert_raw(Value::str(tz.name()), Value::Null);
    }
    Ok(Value::Set(Rc::new(m)))
}
fn zid_short_ids(_: &mut Vm, _: &mut [Value]) -> VR {
    let pairs = [
        ("ACT", "Australia/Darwin"),
        ("AET", "Australia/Sydney"),
        ("AGT", "America/Argentina/Buenos_Aires"),
        ("ART", "Africa/Cairo"),
        ("AST", "America/Anchorage"),
        ("BET", "America/Sao_Paulo"),
        ("BST", "Asia/Dhaka"),
        ("CAT", "Africa/Harare"),
        ("CNT", "America/St_Johns"),
        ("CST", "America/Chicago"),
        ("CTT", "Asia/Shanghai"),
        ("EAT", "Africa/Addis_Ababa"),
        ("ECT", "Europe/Paris"),
        ("IET", "America/Indiana/Indianapolis"),
        ("IST", "Asia/Kolkata"),
        ("JST", "Asia/Tokyo"),
        ("MIT", "Pacific/Apia"),
        ("NET", "Asia/Yerevan"),
        ("NST", "Pacific/Auckland"),
        ("PLT", "Asia/Karachi"),
        ("PNT", "America/Phoenix"),
        ("PRT", "America/Puerto_Rico"),
        ("PST", "America/Los_Angeles"),
        ("SST", "Pacific/Guadalcanal"),
        ("VST", "Asia/Ho_Chi_Minh"),
        ("EST", "-05:00"),
        ("MST", "-07:00"),
        ("HST", "-10:00"),
    ];
    let m = JMap::new(cls::UNMODIFIABLE_MAP);
    let tmp = JMap::hash_map();
    for (k, v) in pairs {
        tmp.insert_raw(Value::str(k), Value::str(v));
    }
    for (k, v) in tmp.entries() {
        m.data.borrow_mut().m.insert(crate::value::Key(k), v);
    }
    Ok(Value::Map(Rc::new(m)))
}
fn zid_display_name(_: &mut Vm, a: &mut [Value]) -> VR {
    match tv(&a[0])? {
        TimeVal::ZoneId(z) => Ok(Value::string(z.id())),
        _ => Err(crate::vm::cce("not a ZoneId")),
    }
}
fn zo(o: i32) -> VR {
    r(TimeVal::ZoneId(Zone::Offset(o)))
}
fn zo_utc(_: &mut Vm, _: &mut [Value]) -> VR {
    zo(0)
}
fn zo_max(_: &mut Vm, _: &mut [Value]) -> VR {
    zo(18 * 3600)
}
fn zo_min(_: &mut Vm, _: &mut [Value]) -> VR {
    zo(-18 * 3600)
}
fn zo_of(_: &mut Vm, a: &mut [Value]) -> VR {
    zo(parse_offset(&text_arg(&a[0])?).map_err(|t| if t.frames.is_empty() { t } else { jt(t, &["ZoneOffset.ofHoursMinutesSeconds(ZoneOffset.java:336)", "ZoneOffset.of(ZoneOffset.java:269)"]) })?)
}
fn zo_of_hours(_: &mut Vm, a: &mut [Value]) -> VR {
    zo(offset_hms(i(a, 0) as i32, 0, 0)?)
}
fn zo_of_hm(_: &mut Vm, a: &mut [Value]) -> VR {
    zo(offset_hms(i(a, 0) as i32, i(a, 1) as i32, 0)?)
}
fn zo_of_hms(_: &mut Vm, a: &mut [Value]) -> VR {
    zo(offset_hms(i(a, 0) as i32, i(a, 1) as i32, i(a, 2) as i32)?)
}
fn zo_of_total(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = i(a, 0);
    if s.abs() > 18 * 3600 {
        return Err(dte("Zone offset not in valid range: -18:00 to +18:00"));
    }
    zo(s as i32)
}
fn zo_total(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(offset_arg(&a[0])?))
}
fn zo_from(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::ZONE_OFFSET, arg_tv(&a[0], "temporal")?)?)
}

// ------------------------------------------------------------------------------ enums and small types

fn dow_v(d: u8) -> VR {
    r(TimeVal::DayOfWeek(d))
}
fn month_v(m: u8) -> VR {
    r(TimeVal::Month(m))
}
fn dow_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = i(a, 0);
    if !(1..=7).contains(&d) {
        return Err(dte(format!("Invalid value for DayOfWeek: {d}")));
    }
    dow_v(d as u8)
}
fn dow_from(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::DAY_OF_WEEK, arg_tv(&a[0], "temporal")?)?)
}
fn enum_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(match tv(&a[0])? {
        TimeVal::DayOfWeek(d) | TimeVal::Month(d) => *d as i32,
        TimeVal::Year(y) => *y,
        _ => 0,
    }))
}
fn dow_plus(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::DayOfWeek(d) = tv(&a[0])? else { return Err(crate::vm::cce("not a DayOfWeek")) };
    dow_v(((*d as i64 - 1 + l(a, 1).rem_euclid(7) + 7) % 7 + 1) as u8)
}
fn dow_minus(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::DayOfWeek(d) = tv(&a[0])? else { return Err(crate::vm::cce("not a DayOfWeek")) };
    dow_v(((*d as i64 - 1 - l(a, 1).rem_euclid(7) + 7) % 7 + 1) as u8)
}
fn text_style_ord(v: &Value) -> u8 {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Time(TimeVal::Enum(_, ord)) => *ord,
            _ => 0,
        },
        _ => 0,
    }
}
fn locale_arg(v: &Value) -> Rc<str> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Locale(l) => l.clone(),
            _ => Rc::from("en_US"),
        },
        _ => Rc::from("en_US"),
    }
}
fn dow_display(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::DayOfWeek(d) = tv(&a[0])? else { return Err(crate::vm::cce("not a DayOfWeek")) };
    Ok(Value::string(super::format::locale_text(&locale_arg(&a[2]), super::format::FField::Std(Field::DayOfWeek), *d as i64, text_style_ord(&a[1]))))
}
fn month_display(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::Month(m) = tv(&a[0])? else { return Err(crate::vm::cce("not a Month")) };
    Ok(Value::string(super::format::locale_text(&locale_arg(&a[2]), super::format::FField::Std(Field::MonthOfYear), *m as i64, text_style_ord(&a[1]))))
}
fn dow_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = a[0].to_java_string();
    match (1..=7).find(|d| dow_name(*d) == s) {
        Some(d) => dow_v(d),
        None => Err(iae_dte(format!("No enum constant java.time.DayOfWeek.{s}"))),
    }
}
fn dow_values(_: &mut Vm, _: &mut [Value]) -> VR {
    let items: Vec<Value> = (1..=7).map(|d| time_value(TimeVal::DayOfWeek(d))).collect();
    Ok(crate::dynamic::array_from(crate::types::Ty::array(cls::DAY_OF_WEEK, 1), items))
}
fn month_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = i(a, 0);
    if !(1..=12).contains(&m) {
        return Err(jt(dte(format!("Invalid value for MonthOfYear: {m}")), &["Month.of(Month.java:187)"]));
    }
    month_v(m as u8)
}
fn month_from(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::MONTH, arg_tv(&a[0], "temporal")?)?)
}
fn month_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = a[0].to_java_string();
    match (1..=12).find(|m| month_name(*m) == s) {
        Some(m) => month_v(m),
        None => Err(iae_dte(format!("No enum constant java.time.Month.{s}"))),
    }
}
fn month_values(_: &mut Vm, _: &mut [Value]) -> VR {
    let items: Vec<Value> = (1..=12).map(|m| time_value(TimeVal::Month(m))).collect();
    Ok(crate::dynamic::array_from(crate::types::Ty::array(cls::MONTH, 1), items))
}
fn month_m(a: &[Value]) -> Result<u8, Thrown> {
    match tv(&a[0])? {
        TimeVal::Month(m) => Ok(*m),
        _ => Err(crate::vm::cce("not a Month")),
    }
}
fn month_plus(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = month_m(a)? as i64;
    month_v(((m - 1 + l(a, 1).rem_euclid(12)) % 12 + 1) as u8)
}
fn month_minus(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = month_m(a)? as i64;
    month_v(((m - 1 - l(a, 1).rem_euclid(12) + 12) % 12 + 1) as u8)
}
fn month_length(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = month_m(a)?;
    let leap = crate::lib_lang::boolean(&a[1]);
    Ok(Value::Int(month_len(if leap { 2000 } else { 2001 }, m) as i32))
}
fn month_max_length(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(month_len(2000, month_m(a)?) as i32))
}
fn month_min_length(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(month_len(2001, month_m(a)?) as i32))
}
fn month_first_day_of_year(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = month_m(a)?;
    let leap = crate::lib_lang::boolean(&a[1]);
    let d = Date { y: if leap { 2000 } else { 2001 }, m, d: 1 };
    Ok(Value::Int(d.day_of_year() as i32))
}
fn month_first_of_quarter(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = month_m(a)?;
    month_v(((m - 1) / 3) * 3 + 1)
}

fn year_v(y: i64) -> VR {
    r(TimeVal::Year(check(Field::Year, y)? as i32))
}
fn y_max(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Int(MAX_YEAR as i32))
}
fn y_min(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Int(MIN_YEAR as i32))
}
fn y_of(_: &mut Vm, a: &mut [Value]) -> VR {
    year_v(i(a, 0))
}
fn y_get(a: &[Value]) -> Result<i64, Thrown> {
    match tv(&a[0])? {
        TimeVal::Year(y) => Ok(*y as i64),
        _ => Err(crate::vm::cce("not a Year")),
    }
}
fn y_is_leap(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(is_leap(y_get(a)?)))
}
fn y_is_leap_static(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(is_leap(l(a, 0))))
}
fn y_length(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(if is_leap(y_get(a)?) { 366 } else { 365 }))
}
fn y_at_day(_: &mut Vm, a: &mut [Value]) -> VR {
    ld(Date::of_year_day(y_get(a)?, i(a, 1))?)
}
fn y_at_month(_: &mut Vm, a: &mut [Value]) -> VR {
    let y = y_get(a)?;
    let m = match &a[1] {
        Value::Obj(_) => tv(&a[1])?.get_long(Field::MonthOfYear)?,
        v => int(v) as i64,
    };
    r(TimeVal::YearMonth(y as i32, check(Field::MonthOfYear, m)? as u8))
}
fn y_at_month_day(_: &mut Vm, a: &mut [Value]) -> VR {
    let y = y_get(a)?;
    let TimeVal::MonthDay(m, d) = arg_tv(&a[1], "monthDay")? else { return Err(crate::vm::cce("not a MonthDay")) };
    ld(Date::clamp(y, *m, *d))
}
fn y_is_valid_month_day(_: &mut Vm, a: &mut [Value]) -> VR {
    let y = y_get(a)?;
    let ok = match &a[1] {
        Value::Null => false,
        v => match tv(v)? {
            TimeVal::MonthDay(m, d) => *d <= month_len(y, *m),
            _ => false,
        },
    };
    Ok(Value::Bool(ok))
}
fn y_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let mut p = iso(&text);
    let neg = p.s.first() == Some(&b'-');
    if neg || p.s.first() == Some(&b'+') {
        p.i = 1;
    }
    let y = p.digits(1, 10)?;
    p.end()?;
    year_v(if neg { -y } else { y })
}
fn y_parse_fmt(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let f = format::formatter_of(&a[1])?;
    r(f.parse_to(&text, cls::YEAR)?)
}
fn y_from(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::YEAR, arg_tv(&a[0], "temporal")?)?)
}
fn y_plus_years(_: &mut Vm, a: &mut [Value]) -> VR {
    year_v(y_get(a)? + l(a, 1))
}
fn y_minus_years(_: &mut Vm, a: &mut [Value]) -> VR {
    year_v(y_get(a)? - l(a, 1))
}

fn ym(y: i64, m: i64) -> VR {
    r(TimeVal::YearMonth(check(Field::Year, y)? as i32, check(Field::MonthOfYear, m)? as u8))
}
fn ym_get(a: &[Value]) -> Result<(i64, u8), Thrown> {
    match tv(&a[0])? {
        TimeVal::YearMonth(y, m) => Ok((*y as i64, *m)),
        _ => Err(crate::vm::cce("not a YearMonth")),
    }
}
fn ym_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = match &a[1] {
        Value::Obj(_) => tv(&a[1])?.get_long(Field::MonthOfYear)?,
        v => int(v) as i64,
    };
    ym(i(a, 0), m)
}
fn ym_at_day(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m) = ym_get(a)?;
    ld(Date::of(y, m as i64, i(a, 1))?)
}
fn ym_at_end(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m) = ym_get(a)?;
    ld(Date { y: y as i32, m, d: month_len(y, m) })
}
fn ym_is_valid_day(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m) = ym_get(a)?;
    let d = i(a, 1);
    Ok(Value::Bool(d >= 1 && d <= month_len(y, m) as i64))
}
fn ym_plus_years(_: &mut Vm, a: &mut [Value]) -> VR {
    r(tv(&a[0])?.plus_unit(l(a, 1), Unit::Years)?)
}
fn ym_plus_months(_: &mut Vm, a: &mut [Value]) -> VR {
    r(tv(&a[0])?.plus_unit(l(a, 1), Unit::Months)?)
}
fn ym_minus_years(_: &mut Vm, a: &mut [Value]) -> VR {
    r(tv(&a[0])?.plus_unit(-l(a, 1), Unit::Years)?)
}
fn ym_minus_months(_: &mut Vm, a: &mut [Value]) -> VR {
    r(tv(&a[0])?.plus_unit(-l(a, 1), Unit::Months)?)
}
fn ym_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let mut p = iso(&text);
    let y = p.year()?;
    p.lit(b'-')?;
    let m = p.digits(2, 2)?;
    p.end()?;
    ym(y, m)
}
fn ym_parse_fmt(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let f = format::formatter_of(&a[1])?;
    r(f.parse_to(&text, cls::YEAR_MONTH)?)
}
fn ym_from(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::YEAR_MONTH, arg_tv(&a[0], "temporal")?)?)
}
fn ym_length_of_year(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, _) = ym_get(a)?;
    Ok(Value::Int(if is_leap(y) { 366 } else { 365 }))
}

fn md_get(a: &[Value]) -> Result<(u8, u8), Thrown> {
    match tv(&a[0])? {
        TimeVal::MonthDay(m, d) => Ok((*m, *d)),
        _ => Err(crate::vm::cce("not a MonthDay")),
    }
}
fn md_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let m = check(Field::MonthOfYear, month_arg(&a[0])?)? as u8;
    let d = check(Field::DayOfMonth, i(a, 1))? as u8;
    if d > month_len(2000, m) {
        return Err(dte(format!("Illegal value for DayOfMonth field, value {d} is not valid for month {}", month_name(m))));
    }
    r(TimeVal::MonthDay(m, d))
}
fn md_at_year(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, d) = md_get(a)?;
    let y = check(Field::Year, i(a, 1))?;
    ld(Date::clamp(y, m, d))
}
fn md_is_valid_year(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, d) = md_get(a)?;
    Ok(Value::Bool(!(m == 2 && d == 29 && !is_leap(i(a, 1)))))
}
fn md_with_month(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, d) = md_get(a)?;
    let m = check(Field::MonthOfYear, month_arg(&a[1])?)? as u8;
    r(TimeVal::MonthDay(m, d.min(month_len(2000, m))))
}
fn md_with_day(_: &mut Vm, a: &mut [Value]) -> VR {
    let (m, _) = md_get(a)?;
    let d = check(Field::DayOfMonth, i(a, 1))? as u8;
    if d > month_len(2000, m) {
        return Err(dte(format!("Illegal value for DayOfMonth field, value {d} is not valid for month {}", month_name(m))));
    }
    r(TimeVal::MonthDay(m, d))
}
fn md_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let mut p = iso(&text);
    p.lit(b'-')?;
    p.lit(b'-')?;
    let m = p.digits(2, 2)?;
    p.lit(b'-')?;
    let d = p.digits(2, 2)?;
    p.end()?;
    let m = check(Field::MonthOfYear, m)? as u8;
    if d < 1 || d > month_len(2000, m) as i64 {
        return Err(parse_error(&text, 0, &format!(": Illegal value for DayOfMonth field, value {d} is not valid for month {}", month_name(m))));
    }
    r(TimeVal::MonthDay(m, d as u8))
}
fn md_parse_fmt(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = text_arg(&a[0])?;
    let f = format::formatter_of(&a[1])?;
    r(f.parse_to(&text, cls::MONTH_DAY)?)
}
fn md_from(_: &mut Vm, a: &mut [Value]) -> VR {
    r(convert(cls::MONTH_DAY, arg_tv(&a[0], "temporal")?)?)
}

// ------------------------------------------------------------------------------ ChronoUnit / ChronoField

fn unit_value(u: Unit) -> Value {
    time_value(TimeVal::TUnit(u))
}
fn field_value(f: Field) -> Value {
    time_value(TimeVal::TField(f))
}
fn unit_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = a[0].to_java_string();
    match Unit::ALL.iter().find(|u| u.const_name() == s) {
        Some(u) => Ok(unit_value(*u)),
        None => Err(iae_dte(format!("No enum constant java.time.temporal.ChronoUnit.{s}"))),
    }
}
fn unit_values(_: &mut Vm, _: &mut [Value]) -> VR {
    let items = Unit::ALL.iter().map(|u| unit_value(*u)).collect();
    Ok(crate::dynamic::array_from(crate::types::Ty::array(cls::CHRONO_UNIT, 1), items))
}
fn field_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = a[0].to_java_string();
    match Field::ALL.iter().find(|f| f.const_name() == s) {
        Some(f) => Ok(field_value(*f)),
        None => Err(jt(iae_dte(format!("No enum constant java.time.temporal.ChronoField.{s}")), &["java.lang.Enum.valueOf(Enum.java:293)", "temporal.ChronoField.valueOf(ChronoField.java:102)"])),
    }
}
fn field_values(_: &mut Vm, _: &mut [Value]) -> VR {
    let items = Field::ALL.iter().map(|f| field_value(*f)).collect();
    Ok(crate::dynamic::array_from(crate::types::Ty::array(cls::CHRONO_FIELD, 1), items))
}
fn unit_between(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xunit(&a[0]) {
        let s = arg_tv(&a[1], "temporal1Inclusive")?.clone();
        return Ok(Value::Long(x.between(&s, arg_tv(&a[2], "temporal2Exclusive")?)?));
    }
    let u = unit_of(&a[0])?;
    let s = arg_tv(&a[1], "temporal1Inclusive")?.clone();
    let e = arg_tv(&a[2], "temporal2Exclusive")?.clone();
    let date = matches!(s, TimeVal::LocalDate(_));
    Ok(Value::Long(s.until(&e, u).map_err(|t| {
        if date && t.class == cls::UNSUPPORTED_TEMPORAL_TYPE_EXCEPTION {
            jt(t, &["LocalDate.until(LocalDate.java:1656)", "temporal.ChronoUnit.between(ChronoUnit.java:271)"])
        } else {
            t
        }
    })?))
}
fn unit_add_to(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xunit(&a[0]) {
        return r(x.add_to(arg_tv(&a[1], "temporal")?, l(a, 2))?);
    }
    let u = unit_of(&a[0])?;
    r(arg_tv(&a[1], "temporal")?.plus_unit(l(a, 2), u)?)
}
fn unit_duration(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xunit(&a[0]) {
        // a year is 365.2425 days
        return dur(if x == XUnit::QuarterYears { 31_556_952 / 4 } else { 31_556_952 }, 0);
    }
    let (s, n) = unit_of(&a[0])?.duration();
    dur(s, n as i32)
}
fn unit_is_date_based(_: &mut Vm, a: &mut [Value]) -> VR {
    if xunit(&a[0]).is_some() {
        return Ok(Value::Bool(true));
    }
    Ok(Value::Bool(unit_of(&a[0])?.is_date_based()))
}
fn unit_is_time_based(_: &mut Vm, a: &mut [Value]) -> VR {
    if xunit(&a[0]).is_some() {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(unit_of(&a[0])?.is_time_based()))
}
fn unit_is_estimated(_: &mut Vm, a: &mut [Value]) -> VR {
    if xunit(&a[0]).is_some() {
        return Ok(Value::Bool(true));
    }
    Ok(Value::Bool(unit_of(&a[0])? >= Unit::Days))
}
fn unit_is_supported_by(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xunit(&a[0]) {
        return Ok(Value::Bool(x.add_to(arg_tv(&a[1], "temporal")?, 0).is_ok()));
    }
    let u = unit_of(&a[0])?;
    Ok(Value::Bool(arg_tv(&a[1], "temporal")?.plus_unit(0, u).is_ok()))
}
fn field_check(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_of(&a[0])?;
    Ok(Value::Long(check(f, l(a, 1))?))
}
fn field_check_int(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_of(&a[0])?;
    Ok(Value::Int(check(f, l(a, 1))? as i32))
}
/// Base and range units of the non-`ChronoField` fields.
fn x_units(x: XField) -> (TimeVal, TimeVal) {
    use super::weeks::WeekKind as K;
    let u = |u: Unit| TimeVal::TUnit(u);
    let x_u = |u: XUnit| TimeVal::XUnit(u);
    match x {
        XField::DayOfQuarter => (u(Unit::Days), x_u(XUnit::QuarterYears)),
        XField::QuarterOfYear => (x_u(XUnit::QuarterYears), u(Unit::Years)),
        XField::IsoWeekOfWeekBasedYear => (u(Unit::Weeks), x_u(XUnit::WeekBasedYears)),
        XField::IsoWeekBasedYear => (x_u(XUnit::WeekBasedYears), u(Unit::Forever)),
        XField::JulianDay | XField::ModifiedJulianDay | XField::RataDie => (u(Unit::Days), u(Unit::Forever)),
        XField::Week(K::DayOfWeek, _) => (u(Unit::Days), u(Unit::Weeks)),
        XField::Week(K::WeekOfMonth, _) => (u(Unit::Weeks), u(Unit::Months)),
        XField::Week(K::WeekOfYear, _) => (u(Unit::Weeks), u(Unit::Years)),
        XField::Week(K::WeekOfWeekBasedYear, _) => (u(Unit::Weeks), x_u(XUnit::WeekBasedYears)),
        XField::Week(K::WeekBasedYear, _) => (x_u(XUnit::WeekBasedYears), u(Unit::Forever)),
    }
}
fn field_base_unit(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[0]) {
        return r(x_units(x).0);
    }
    Ok(unit_value(field_of(&a[0])?.base_unit()))
}
fn field_range_unit(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[0]) {
        return r(x_units(x).1);
    }
    Ok(unit_value(field_of(&a[0])?.range_unit()))
}
fn field_is_date_based(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[0]) {
        return Ok(Value::Bool(x.is_date_based()));
    }
    Ok(Value::Bool(field_of(&a[0])?.is_date_based()))
}
fn field_is_time_based(_: &mut Vm, a: &mut [Value]) -> VR {
    if xfield(&a[0]).is_some() {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(field_of(&a[0])?.is_time_based()))
}
fn field_display_name(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[0]) {
        return Ok(Value::string(x.name()));
    }
    Ok(Value::str(field_of(&a[0])?.name()))
}
fn field_get_from(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[0]) {
        return Ok(Value::Long(x.get(arg_tv(&a[1], "temporal")?)?));
    }
    let f = field_of(&a[0])?;
    Ok(Value::Long(arg_tv(&a[1], "temporal")?.get_long(f)?))
}
fn field_is_supported_by(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[0]) {
        return Ok(Value::Bool(x.is_supported_by(arg_tv(&a[1], "temporal")?)));
    }
    let f = field_of(&a[0])?;
    Ok(Value::Bool(arg_tv(&a[1], "temporal")?.is_supported(f)))
}
fn field_adjust_into(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[0]) {
        return r(x.adjust(arg_tv(&a[1], "temporal")?, l(a, 2))?);
    }
    let f = field_of(&a[0])?;
    r(arg_tv(&a[1], "temporal")?.with_field(f, l(a, 2))?)
}
fn field_range(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(x) = xfield(&a[0]) {
        return xrange(x.range());
    }
    let f = field_of(&a[0])?;
    let (lo, hi) = f.range();
    let (min_l, max_s) = match f {
        Field::DayOfMonth => (1, 28),
        Field::DayOfYear => (1, 365),
        Field::AlignedWeekOfMonth => (1, 4),
        Field::YearOfEra => (1, MAX_YEAR),
        _ => (lo, hi),
    };
    r(TimeVal::Range { min_s: lo, min_l, max_s, max_l: hi })
}

// ------------------------------------------------------------------------------ ValueRange

fn vr_get(a: &[Value]) -> Result<(i64, i64, i64, i64), Thrown> {
    match tv(&a[0])? {
        TimeVal::Range { min_s, min_l, max_s, max_l } => Ok((*min_s, *min_l, *max_s, *max_l)),
        _ => Err(crate::vm::cce("not a ValueRange")),
    }
}
fn vr_of2(_: &mut Vm, a: &mut [Value]) -> VR {
    let (lo, hi) = (l(a, 0), l(a, 1));
    if lo > hi {
        return Err(iae_dte("Minimum value must be less than maximum value".into()));
    }
    r(TimeVal::Range { min_s: lo, min_l: lo, max_s: hi, max_l: hi })
}
fn vr_of3(_: &mut Vm, a: &mut [Value]) -> VR {
    let (lo, ms, ml) = (l(a, 0), l(a, 1), l(a, 2));
    if lo > ms {
        return Err(iae_dte("Minimum value must be less than smallest maximum value".into()));
    }
    if ms > ml {
        return Err(iae_dte("Smallest maximum value must be less than largest maximum value".into()));
    }
    r(TimeVal::Range { min_s: lo, min_l: lo, max_s: ms, max_l: ml })
}
fn vr_of4(_: &mut Vm, a: &mut [Value]) -> VR {
    let (a0, a1, a2, a3) = (l(a, 0), l(a, 1), l(a, 2), l(a, 3));
    if a0 > a1 {
        return Err(iae_dte("Smallest minimum value must be less than largest minimum value".into()));
    }
    if a2 > a3 {
        return Err(iae_dte("Smallest maximum value must be less than largest maximum value".into()));
    }
    if a1 > a3 {
        return Err(iae_dte("Minimum value must be less than maximum value".into()));
    }
    r(TimeVal::Range { min_s: a0, min_l: a1, max_s: a2, max_l: a3 })
}
fn vr_min(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(vr_get(a)?.0))
}
fn vr_largest_min(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(vr_get(a)?.1))
}
fn vr_smallest_max(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(vr_get(a)?.2))
}
fn vr_max(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(vr_get(a)?.3))
}
fn vr_is_fixed(_: &mut Vm, a: &mut [Value]) -> VR {
    let (a0, a1, a2, a3) = vr_get(a)?;
    Ok(Value::Bool(a0 == a1 && a2 == a3))
}
fn vr_is_int(_: &mut Vm, a: &mut [Value]) -> VR {
    let (a0, _, _, a3) = vr_get(a)?;
    Ok(Value::Bool(a0 >= i32::MIN as i64 && a3 <= i32::MAX as i64))
}
fn vr_is_valid(_: &mut Vm, a: &mut [Value]) -> VR {
    let (a0, _, _, a3) = vr_get(a)?;
    let v = l(a, 1);
    Ok(Value::Bool(v >= a0 && v <= a3))
}
fn vr_is_valid_int(_: &mut Vm, a: &mut [Value]) -> VR {
    let (a0, _, _, a3) = vr_get(a)?;
    let v = l(a, 1);
    Ok(Value::Bool(a0 >= i32::MIN as i64 && a3 <= i32::MAX as i64 && v >= a0 && v <= a3))
}
fn vr_check(_: &mut Vm, a: &mut [Value]) -> VR {
    let (a0, _, _, a3) = vr_get(a)?;
    let v = l(a, 1);
    if v < a0 || v > a3 {
        let f = field_of(&a[2]).map(|f| f.name()).unwrap_or("value");
        return Err(dte(format!("Invalid value for {f} (valid values {}): {v}", tv(&a[0])?.to_java_string())));
    }
    Ok(Value::Long(v))
}
fn vr_check_int(vm: &mut Vm, a: &mut [Value]) -> VR {
    let v = vr_check(vm, a)?;
    Ok(Value::Int(long(&v) as i32))
}

// ------------------------------------------------------------------------------ TemporalAdjusters

fn adj(a: Adjuster) -> VR {
    r(TimeVal::Adjuster(a))
}
fn dow_arg(v: &Value) -> Result<u8, Thrown> {
    match arg_tv(v, "dayOfWeek")? {
        TimeVal::DayOfWeek(d) => Ok(*d),
        _ => Err(crate::vm::cce("not a DayOfWeek")),
    }
}
fn ta_first_day_of_month(_: &mut Vm, _: &mut [Value]) -> VR {
    adj(Adjuster::FirstDayOfMonth)
}
fn ta_last_day_of_month(_: &mut Vm, _: &mut [Value]) -> VR {
    adj(Adjuster::LastDayOfMonth)
}
fn ta_first_day_of_next_month(_: &mut Vm, _: &mut [Value]) -> VR {
    adj(Adjuster::FirstDayOfNextMonth)
}
fn ta_first_day_of_year(_: &mut Vm, _: &mut [Value]) -> VR {
    adj(Adjuster::FirstDayOfYear)
}
fn ta_last_day_of_year(_: &mut Vm, _: &mut [Value]) -> VR {
    adj(Adjuster::LastDayOfYear)
}
fn ta_first_day_of_next_year(_: &mut Vm, _: &mut [Value]) -> VR {
    adj(Adjuster::FirstDayOfNextYear)
}
fn ta_first_in_month(_: &mut Vm, a: &mut [Value]) -> VR {
    adj(Adjuster::FirstInMonth(dow_arg(&a[0])?))
}
fn ta_last_in_month(_: &mut Vm, a: &mut [Value]) -> VR {
    adj(Adjuster::LastInMonth(dow_arg(&a[0])?))
}
fn ta_dow_in_month(_: &mut Vm, a: &mut [Value]) -> VR {
    adj(Adjuster::DayOfWeekInMonth(int(&a[0]), dow_arg(&a[1])?))
}
fn ta_next(_: &mut Vm, a: &mut [Value]) -> VR {
    adj(Adjuster::Next(dow_arg(&a[0])?))
}
fn ta_next_or_same(_: &mut Vm, a: &mut [Value]) -> VR {
    adj(Adjuster::NextOrSame(dow_arg(&a[0])?))
}
fn ta_previous(_: &mut Vm, a: &mut [Value]) -> VR {
    adj(Adjuster::Previous(dow_arg(&a[0])?))
}
fn ta_previous_or_same(_: &mut Vm, a: &mut [Value]) -> VR {
    adj(Adjuster::PreviousOrSame(dow_arg(&a[0])?))
}
fn ta_of_date_adjuster(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = a[0].clone();
    Ok(crate::func::native(Some(cls::TEMPORAL_ADJUSTER), move |vm, args| {
        let t = tv(args.first().unwrap_or(&Value::Null))?.clone();
        let d = to_local_date(&t)?;
        let r2 = crate::func::call(vm, &f, &[time_value(TimeVal::LocalDate(d))])?;
        let nd = to_local_date(tv(&r2)?)?;
        Ok(time_value(replace_date(&t, nd)?))
    }))
}
fn adjust_into(vm: &mut Vm, a: &mut [Value]) -> VR {
    let target = arg_tv(&a[1], "temporal")?.clone();
    let me = a[0].clone();
    r(adjust(&target, &me, vm)?)
}

// ------------------------------------------------------------------------------ Clock & Date

fn clock_fixed(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = arg_tv(&a[0], "fixedInstant")?.instant().ok_or_else(|| crate::vm::cce("not an Instant"))?;
    r(TimeVal::Clock { fixed: Some((s, n)), zone: zone_arg(&a[1])? })
}
fn now_parts() -> (i64, i32) {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() as i64, d.subsec_nanos() as i32)
}
fn clock_instant_of(t: &TimeVal) -> (i64, i32) {
    match t {
        TimeVal::Clock { fixed: Some(f), .. } => *f,
        _ => now_parts(),
    }
}
fn clock_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = clock_instant_of(tv(&a[0])?);
    r(TimeVal::Instant { secs: s, nanos: n })
}
fn clock_millis(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = clock_instant_of(tv(&a[0])?);
    Ok(Value::Long(s * 1000 + n as i64 / 1_000_000))
}
fn clock_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = arg_tv(&a[0], "baseClock")?.clone();
    let (ds, dn) = dur_of(&a[1])?;
    let TimeVal::Clock { zone, .. } = &c else { return Err(crate::vm::cce("not a Clock")) };
    let (s, n) = clock_instant_of(&c);
    let (s2, n2) = plus_secs_nanos(s, n, ds, dn as i64)?;
    r(TimeVal::Clock { fixed: Some((s2, n2)), zone: zone.clone() })
}
fn clock_tick(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = arg_tv(&a[0], "baseClock")?.clone();
    let tick = dur_total(dur_of(&a[1])?);
    let TimeVal::Clock { zone, .. } = &c else { return Err(crate::vm::cce("not a Clock")) };
    let (s, n) = clock_instant_of(&c);
    let total = s as i128 * NANOS_PER_SECOND as i128 + n as i128;
    let t = if tick > 0 { total - total.rem_euclid(tick) } else { total };
    r(TimeVal::Clock { fixed: Some(((t.div_euclid(NANOS_PER_SECOND as i128)) as i64, t.rem_euclid(NANOS_PER_SECOND as i128) as i32)), zone: zone.clone() })
}

fn util_date(ms: i64) -> VR {
    r(TimeVal::UtilDate(std::cell::Cell::new(ms)))
}
fn ud_ms(a: &[Value], idx: usize) -> Result<i64, Thrown> {
    match arg_tv(&a[idx], "date")? {
        TimeVal::UtilDate(ms) => Ok(ms.get()),
        _ => Err(crate::vm::cce("not a Date")),
    }
}
fn ud_new(_: &mut Vm, _: &mut [Value]) -> VR {
    let (s, n) = now_parts();
    util_date(s * 1000 + n as i64 / 1_000_000)
}
fn ud_new_ms(_: &mut Vm, a: &mut [Value]) -> VR {
    util_date(l(a, 0))
}
fn ud_after(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(ud_ms(a, 0)? > ud_ms(a, 1)?))
}
fn ud_before(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(ud_ms(a, 0)? < ud_ms(a, 1)?))
}
fn ud_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(ud_ms(a, 0)?.cmp(&ud_ms(a, 1)?) as i32))
}
fn ud_get_time(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(ud_ms(a, 0)?))
}
fn ud_clone(_: &mut Vm, a: &mut [Value]) -> VR {
    util_date(ud_ms(a, 0)?)
}
fn ud_from(_: &mut Vm, a: &mut [Value]) -> VR {
    let (s, n) = arg_tv(&a[0], "instant")?.instant().ok_or_else(|| crate::vm::cce("not an Instant"))?;
    let ms = s.checked_mul(1000).and_then(|x| x.checked_add(n as i64 / 1_000_000)).ok_or_else(|| iae_dte("java.lang.ArithmeticException: long overflow".into()))?;
    util_date(ms)
}
fn ud_set_time(_: &mut Vm, a: &mut [Value]) -> VR {
    let ms = crate::lib_lang::long(&a[1]);
    match arg_tv(&a[0], "date")? {
        TimeVal::UtilDate(c) => c.set(ms),
        _ => return Err(crate::vm::cce("not a Date")),
    }
    Ok(Value::Null)
}
fn style_const(c: ClassId, o: u8) -> VR {
    r(TimeVal::Enum(c, o))
}
macro_rules! style {
    ($name:ident, $c:expr, $o:expr) => {
        fn $name(_: &mut Vm, _: &mut [Value]) -> VR {
            style_const($c, $o)
        }
    };
}
style!(ts_full, cls::TEXT_STYLE, 0);
style!(ts_full_sa, cls::TEXT_STYLE, 1);
style!(ts_short, cls::TEXT_STYLE, 2);
style!(ts_short_sa, cls::TEXT_STYLE, 3);
style!(ts_narrow, cls::TEXT_STYLE, 4);
style!(ts_narrow_sa, cls::TEXT_STYLE, 5);
style!(rs_strict, cls::RESOLVER_STYLE, 0);
style!(rs_smart, cls::RESOLVER_STYLE, 1);
style!(rs_lenient, cls::RESOLVER_STYLE, 2);
style!(fs_full, cls::FORMAT_STYLE, 0);
style!(fs_long, cls::FORMAT_STYLE, 1);
style!(fs_medium, cls::FORMAT_STYLE, 2);
style!(fs_short, cls::FORMAT_STYLE, 3);

// values() / valueOf(String) of the java.time.format enums, and TextStyle's helpers

/// Constant names of a simple java.time.format enum, in ordinal order.
fn style_names(c: ClassId) -> &'static [&'static str] {
    match c {
        cls::TEXT_STYLE => &["FULL", "FULL_STANDALONE", "SHORT", "SHORT_STANDALONE", "NARROW", "NARROW_STANDALONE"],
        cls::RESOLVER_STYLE => &["STRICT", "SMART", "LENIENT"],
        cls::FORMAT_STYLE => &["FULL", "LONG", "MEDIUM", "SHORT"],
        _ => &["NORMAL", "ALWAYS", "NEVER", "NOT_NEGATIVE", "EXCEEDS_PAD"],
    }
}
fn style_values(c: ClassId) -> VR {
    let items: Vec<Value> = (0..style_names(c).len()).map(|o| time_value(TimeVal::Enum(c, o as u8))).collect();
    Ok(Value::obj(Obj::Array(crate::value::JArray::from_data(crate::types::Ty::of(c), crate::value::ArrayData::Ref(items)))))
}
fn style_value_of(c: ClassId, v: &Value) -> VR {
    let name = match v {
        Value::Null => return Err(crate::vm::npe("Name is null")),
        v => v.to_java_string(),
    };
    match style_names(c).iter().position(|n| *n == name) {
        Some(o) => style_const(c, o as u8),
        None => Err(crate::vm::iae(format!("No enum constant {}.{name}", crate::lookup::class_name(c)))),
    }
}
macro_rules! style_enum {
    ($values:ident, $value_of:ident, $c:expr) => {
        fn $values(_: &mut Vm, _: &mut [Value]) -> VR {
            style_values($c)
        }
        fn $value_of(_: &mut Vm, a: &mut [Value]) -> VR {
            style_value_of($c, &a[0])
        }
    };
}
style_enum!(ts_values, ts_value_of, cls::TEXT_STYLE);
style_enum!(rs_values, rs_value_of, cls::RESOLVER_STYLE);
style_enum!(fs_values, fs_value_of, cls::FORMAT_STYLE);
style_enum!(ss_values, ss_value_of, cls::SIGN_STYLE);
style!(ss_normal, cls::SIGN_STYLE, 0);
style!(ss_always, cls::SIGN_STYLE, 1);
style!(ss_never, cls::SIGN_STYLE, 2);
style!(ss_not_negative, cls::SIGN_STYLE, 3);
style!(ss_exceeds_pad, cls::SIGN_STYLE, 4);
fn ts_as_normal(_: &mut Vm, a: &mut [Value]) -> VR {
    style_const(cls::TEXT_STYLE, text_style_ord(&a[0]) & !1)
}
fn ts_as_standalone(_: &mut Vm, a: &mut [Value]) -> VR {
    style_const(cls::TEXT_STYLE, text_style_ord(&a[0]) | 1)
}
fn ts_is_standalone(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(text_style_ord(&a[0]) & 1 == 1))
}
fn register_style_enums(b: &mut Builder) {
    use cls::*;
    b.s(TEXT_STYLE, "TextStyle[] values()", ts_values);
    b.s(TEXT_STYLE, "TextStyle valueOf(String)", ts_value_of);
    b.m(TEXT_STYLE, "TextStyle asNormal()", ts_as_normal);
    b.m(TEXT_STYLE, "TextStyle asStandalone()", ts_as_standalone);
    b.m(TEXT_STYLE, "boolean isStandalone()", ts_is_standalone);
    b.s(RESOLVER_STYLE, "ResolverStyle[] values()", rs_values);
    b.s(RESOLVER_STYLE, "ResolverStyle valueOf(String)", rs_value_of);
    b.s(FORMAT_STYLE, "FormatStyle[] values()", fs_values);
    b.s(FORMAT_STYLE, "FormatStyle valueOf(String)", fs_value_of);
    b.s(SIGN_STYLE, "SignStyle[] values()", ss_values);
    b.s(SIGN_STYLE, "SignStyle valueOf(String)", ss_value_of);
    b.sf(SIGN_STYLE, "SignStyle ALWAYS", ss_always);
    b.sf(SIGN_STYLE, "SignStyle EXCEEDS_PAD", ss_exceeds_pad);
    b.sf(SIGN_STYLE, "SignStyle NEVER", ss_never);
    b.sf(SIGN_STYLE, "SignStyle NORMAL", ss_normal);
    b.sf(SIGN_STYLE, "SignStyle NOT_NEGATIVE", ss_not_negative);
}

fn week_def(v: &Value) -> Result<WeekDef, Thrown> {
    match tv(v)? {
        TimeVal::WeekFields(w) => Ok(*w),
        _ => Err(crate::vm::cce("not a WeekFields")),
    }
}
fn wf_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let TimeVal::DayOfWeek(first) = arg_tv(&a[0], "firstDayOfWeek")? else { return Err(crate::vm::cce("not a DayOfWeek")) };
    let min = l(a, 1);
    if !(1..=7).contains(&min) {
        return Err(jt(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some("Minimal number of days is invalid".into())), &["temporal.WeekFields.<init>(WeekFields.java:361)", "temporal.WeekFields.of(WeekFields.java:341)"]));
    }
    r(TimeVal::WeekFields(WeekDef { first: *first, min_days: min as u8 }))
}
fn wf_of_locale(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        return Err(crate::vm::npe("locale"));
    }
    r(TimeVal::WeekFields(WeekDef::of_locale(&locale_arg(&a[0]))))
}
fn wf_field(a: &[Value], k: super::weeks::WeekKind) -> VR {
    r(TimeVal::XField(XField::Week(k, week_def(&a[0])?)))
}
fn wf_day_of_week(_: &mut Vm, a: &mut [Value]) -> VR {
    wf_field(a, super::weeks::WeekKind::DayOfWeek)
}
fn wf_week_of_month(_: &mut Vm, a: &mut [Value]) -> VR {
    wf_field(a, super::weeks::WeekKind::WeekOfMonth)
}
fn wf_week_of_year(_: &mut Vm, a: &mut [Value]) -> VR {
    wf_field(a, super::weeks::WeekKind::WeekOfYear)
}
fn wf_week_of_week_based_year(_: &mut Vm, a: &mut [Value]) -> VR {
    wf_field(a, super::weeks::WeekKind::WeekOfWeekBasedYear)
}
fn wf_week_based_year(_: &mut Vm, a: &mut [Value]) -> VR {
    wf_field(a, super::weeks::WeekKind::WeekBasedYear)
}
fn wf_first_day(_: &mut Vm, a: &mut [Value]) -> VR {
    r(TimeVal::DayOfWeek(week_def(&a[0])?.first))
}
fn wf_min_days(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(week_def(&a[0])?.min_days as i32))
}

fn register_week_fields(b: &mut Builder) {
    use cls::*;
    for c in [ISO_FIELDS, JULIAN_FIELDS, WEEK_FIELDS] {
        b.supers(c, &[OBJECT]);
    }
    b.sf(ISO_FIELDS, "TemporalField DAY_OF_QUARTER", |_, _| r(TimeVal::XField(XField::DayOfQuarter)));
    b.sf(ISO_FIELDS, "TemporalField QUARTER_OF_YEAR", |_, _| r(TimeVal::XField(XField::QuarterOfYear)));
    b.sf(ISO_FIELDS, "TemporalField WEEK_OF_WEEK_BASED_YEAR", |_, _| r(TimeVal::XField(XField::IsoWeekOfWeekBasedYear)));
    b.sf(ISO_FIELDS, "TemporalField WEEK_BASED_YEAR", |_, _| r(TimeVal::XField(XField::IsoWeekBasedYear)));
    b.sf(ISO_FIELDS, "TemporalUnit QUARTER_YEARS", |_, _| r(TimeVal::XUnit(XUnit::QuarterYears)));
    b.sf(ISO_FIELDS, "TemporalUnit WEEK_BASED_YEARS", |_, _| r(TimeVal::XUnit(XUnit::WeekBasedYears)));
    b.sf(JULIAN_FIELDS, "TemporalField JULIAN_DAY", |_, _| r(TimeVal::XField(XField::JulianDay)));
    b.sf(JULIAN_FIELDS, "TemporalField MODIFIED_JULIAN_DAY", |_, _| r(TimeVal::XField(XField::ModifiedJulianDay)));
    b.sf(JULIAN_FIELDS, "TemporalField RATA_DIE", |_, _| r(TimeVal::XField(XField::RataDie)));
    b.sf(WEEK_FIELDS, "WeekFields ISO", |_, _| r(TimeVal::WeekFields(WeekDef::ISO)));
    b.sf(WEEK_FIELDS, "WeekFields SUNDAY_START", |_, _| r(TimeVal::WeekFields(WeekDef::SUNDAY_START)));
    b.sf(WEEK_FIELDS, "TemporalUnit WEEK_BASED_YEARS", |_, _| r(TimeVal::XUnit(XUnit::WeekBasedYears)));
    b.s(WEEK_FIELDS, "WeekFields of(DayOfWeek,int)", wf_of);
    b.s(WEEK_FIELDS, "WeekFields of(Locale)", wf_of_locale);
    b.m(WEEK_FIELDS, "TemporalField dayOfWeek()", wf_day_of_week);
    b.m(WEEK_FIELDS, "TemporalField weekOfMonth()", wf_week_of_month);
    b.m(WEEK_FIELDS, "TemporalField weekOfYear()", wf_week_of_year);
    b.m(WEEK_FIELDS, "TemporalField weekOfWeekBasedYear()", wf_week_of_week_based_year);
    b.m(WEEK_FIELDS, "TemporalField weekBasedYear()", wf_week_based_year);
    b.m(WEEK_FIELDS, "DayOfWeek getFirstDayOfWeek()", wf_first_day);
    b.m(WEEK_FIELDS, "int getMinimalDaysInFirstWeek()", wf_min_days);
}

pub fn register(b: &mut Builder) {
    super::rules::register(b);
    super::legacy::register(b);
    super::legacy_fmt::register(b);
    super::builder::register(b);
    use cls::*;
    register_week_fields(b);
    // hierarchy
    b.interface(TEMPORAL_ACCESSOR, &[]);
    b.interface(TEMPORAL, &[TEMPORAL_ACCESSOR]);
    b.interface(TEMPORAL_ADJUSTER, &[]);
    b.interface(TEMPORAL_AMOUNT, &[]);
    b.interface(TEMPORAL_UNIT, &[]);
    b.interface(TEMPORAL_FIELD, &[]);
    b.interface(TEMPORAL_QUERY, &[]);
    b.interface(CHRONO_LOCAL_DATE, &[TEMPORAL, TEMPORAL_ADJUSTER, COMPARABLE]);
    b.interface(CHRONO_LOCAL_DATE_TIME, &[TEMPORAL, TEMPORAL_ADJUSTER, COMPARABLE]);
    b.interface(CHRONO_ZONED_DATE_TIME, &[TEMPORAL, COMPARABLE]);
    b.interface(CHRONO_PERIOD, &[TEMPORAL_AMOUNT]);
    b.interface(CHRONOLOGY, &[COMPARABLE]);
    b.supers(ISO_CHRONOLOGY, &[OBJECT, CHRONOLOGY]);
    b.supers(INSTANT, &[OBJECT, TEMPORAL, TEMPORAL_ADJUSTER, COMPARABLE]);
    b.supers(DURATION, &[OBJECT, TEMPORAL_AMOUNT, COMPARABLE]);
    b.supers(PERIOD, &[OBJECT, CHRONO_PERIOD]);
    b.supers(LOCAL_DATE, &[OBJECT, CHRONO_LOCAL_DATE]);
    b.supers(LOCAL_TIME, &[OBJECT, TEMPORAL, TEMPORAL_ADJUSTER, COMPARABLE]);
    b.supers(LOCAL_DATE_TIME, &[OBJECT, CHRONO_LOCAL_DATE_TIME]);
    b.supers(ZONED_DATE_TIME, &[OBJECT, CHRONO_ZONED_DATE_TIME]);
    b.supers(OFFSET_DATE_TIME, &[OBJECT, TEMPORAL, TEMPORAL_ADJUSTER, COMPARABLE]);
    b.supers(OFFSET_TIME, &[OBJECT, TEMPORAL, TEMPORAL_ADJUSTER, COMPARABLE]);
    b.supers(ZONE_ID, &[OBJECT]);
    b.supers(ZONE_OFFSET, &[ZONE_ID, TEMPORAL_ACCESSOR, TEMPORAL_ADJUSTER, COMPARABLE]);
    b.supers(ZONE_REGION, &[ZONE_ID]);
    b.supers(DAY_OF_WEEK, &[ENUM, TEMPORAL_ACCESSOR, TEMPORAL_ADJUSTER]);
    b.supers(MONTH, &[ENUM, TEMPORAL_ACCESSOR, TEMPORAL_ADJUSTER]);
    b.supers(YEAR, &[OBJECT, TEMPORAL, TEMPORAL_ADJUSTER, COMPARABLE]);
    b.supers(YEAR_MONTH, &[OBJECT, TEMPORAL, TEMPORAL_ADJUSTER, COMPARABLE]);
    b.supers(MONTH_DAY, &[OBJECT, TEMPORAL_ACCESSOR, TEMPORAL_ADJUSTER, COMPARABLE]);
    b.supers(CHRONO_UNIT, &[ENUM, TEMPORAL_UNIT]);
    b.supers(CHRONO_FIELD, &[ENUM, TEMPORAL_FIELD]);
    b.supers(TEXT_STYLE, &[ENUM]);
    b.supers(RESOLVER_STYLE, &[ENUM]);
    b.supers(FORMAT_STYLE, &[ENUM]);
    b.supers(SIGN_STYLE, &[ENUM]);
    b.supers(VALUE_RANGE, &[OBJECT]);
    b.supers(CLOCK, &[OBJECT]);
    b.supers(TEMPORAL_ADJUSTERS, &[OBJECT]);
    b.supers(TEMPORAL_ADJUSTER_IMPL, &[OBJECT, TEMPORAL_ADJUSTER]);
    b.supers(DATE, &[OBJECT, COMPARABLE]);

    // TemporalAccessor / Temporal
    b.m(TEMPORAL_ACCESSOR, "int get(TemporalField)", t_get);
    b.m(TEMPORAL_ACCESSOR, "long getLong(TemporalField)", t_get_long);
    b.m(TEMPORAL_ACCESSOR, "boolean isSupported(TemporalField)", t_is_supported);
    b.m(TEMPORAL_ACCESSOR, "def query(TemporalQuery)", t_query);
    b.m(TEMPORAL_ACCESSOR, "ValueRange range(TemporalField)", t_range);
    let m = b.m(TEMPORAL_ACCESSOR, "long toEpochMilli()", t_to_epoch_milli);
    b.aug(m, "java.time.temporal.TemporalAccessor.getLong(java.time.temporal.TemporalField)");
    let m = b.m(TEMPORAL_ACCESSOR, "long getMillis()", t_to_epoch_milli);
    b.aug(m, "java.time.temporal.TemporalAccessor.getLong(java.time.temporal.TemporalField)");
    b.m(TEMPORAL, "Temporal minus(long,TemporalUnit)", t_minus_unit);
    b.m(TEMPORAL, "Temporal minus(TemporalAmount)", t_minus_amount);
    b.m(TEMPORAL, "Temporal plus(long,TemporalUnit)", t_plus_unit);
    b.m(TEMPORAL, "Temporal plus(TemporalAmount)", t_plus_amount);
    b.m(TEMPORAL, "long until(Temporal,TemporalUnit)", t_until);
    b.m(TEMPORAL, "Temporal with(TemporalAdjuster)", t_with_adjuster);
    b.m(TEMPORAL, "Temporal with(TemporalField,long)", t_with_field);
    b.m(TEMPORAL_ADJUSTER, "Temporal adjustInto(Temporal)", adjust_into);
    b.m(TEMPORAL_AMOUNT, "Temporal addTo(Temporal)", amount_add_to);
    b.m(TEMPORAL_AMOUNT, "long get(TemporalUnit)", amount_get);
    b.m(TEMPORAL_AMOUNT, "List getUnits()", amount_units);
    b.m(TEMPORAL_AMOUNT, "Temporal subtractFrom(Temporal)", amount_subtract_from);
    b.m(TEMPORAL_UNIT, "Temporal addTo(Temporal,long)", unit_add_to);
    b.m(TEMPORAL_UNIT, "long between(Temporal,Temporal)", unit_between);
    b.m(TEMPORAL_UNIT, "Duration getDuration()", unit_duration);
    b.m(TEMPORAL_UNIT, "boolean isDateBased()", unit_is_date_based);
    b.m(TEMPORAL_UNIT, "boolean isDurationEstimated()", unit_is_estimated);
    b.m(TEMPORAL_UNIT, "boolean isSupportedBy(Temporal)", unit_is_supported_by);
    b.m(TEMPORAL_UNIT, "boolean isTimeBased()", unit_is_time_based);
    b.m(TEMPORAL_FIELD, "Temporal adjustInto(Temporal,long)", field_adjust_into);
    b.m(TEMPORAL_FIELD, "TemporalUnit getBaseUnit()", field_base_unit);
    b.m(TEMPORAL_FIELD, "String getDisplayName(Locale)", field_display_name);
    b.m(TEMPORAL_FIELD, "long getFrom(TemporalAccessor)", field_get_from);
    b.m(TEMPORAL_FIELD, "TemporalUnit getRangeUnit()", field_range_unit);
    b.m(TEMPORAL_FIELD, "boolean isDateBased()", field_is_date_based);
    b.m(TEMPORAL_FIELD, "boolean isSupportedBy(TemporalAccessor)", field_is_supported_by);
    b.m(TEMPORAL_FIELD, "boolean isTimeBased()", field_is_time_based);
    b.m(TEMPORAL_FIELD, "ValueRange range()", field_range);

    // Chrono interfaces
    for (c, cname) in [(CHRONO_LOCAL_DATE, "ChronoLocalDate"), (CHRONO_LOCAL_DATE_TIME, "ChronoLocalDateTime"), (CHRONO_ZONED_DATE_TIME, "ChronoZonedDateTime")] {
        b.m(c, "String format(DateTimeFormatter)", t_format);
        b.m(c, "Chronology getChronology()", g_chronology);
        b.m(c, "boolean isAfter(def)", t_is_after);
        b.m(c, "boolean isBefore(def)", t_is_before);
        b.m(c, "boolean isEqual(def)", t_is_equal);
        b.m(c, &format!("{} minus(TemporalAmount)", cname), t_minus_amount);
        b.m(c, &format!("{} minus(long,TemporalUnit)", cname), t_minus_unit);
        b.m(c, &format!("{} plus(TemporalAmount)", cname), t_plus_amount);
        b.m(c, &format!("{} plus(long,TemporalUnit)", cname), t_plus_unit);
        b.m(c, &format!("{} with(TemporalAdjuster)", cname), t_with_adjuster);
        b.m(c, &format!("{} with(TemporalField,long)", cname), t_with_field);
        b.m(c, "Comparator timeLineOrder()", odt_time_line_order);
        // ChronoLocalDate has no time: only the date-time interfaces declare it
        if c != CHRONO_LOCAL_DATE {
            b.m(c, "LocalTime toLocalTime()", to_local_time_v);
        }
    }
    b.m(CHRONO_LOCAL_DATE, "ChronoLocalDateTime atTime(LocalTime)", ld_at_time);
    b.m(CHRONO_LOCAL_DATE, "int compareTo(ChronoLocalDate)", t_compare);
    b.m(CHRONO_LOCAL_DATE, "def getEra()", g_era_obj);
    b.m(CHRONO_LOCAL_DATE, "boolean isLeapYear()", is_leap_year_v);
    b.m(CHRONO_LOCAL_DATE, "int lengthOfMonth()", length_of_month_v);
    b.m(CHRONO_LOCAL_DATE, "int lengthOfYear()", length_of_year_v);
    b.m(CHRONO_LOCAL_DATE, "long toEpochDay()", to_epoch_day_v);
    b.m(CHRONO_LOCAL_DATE, "ChronoPeriod until(ChronoLocalDate)", ld_until_period);
    b.m(CHRONO_LOCAL_DATE_TIME, "ChronoZonedDateTime atZone(ZoneId)", ldt_at_zone);
    b.m(CHRONO_LOCAL_DATE_TIME, "int compareTo(ChronoLocalDateTime)", t_compare);
    b.m(CHRONO_LOCAL_DATE_TIME, "long toEpochSecond(ZoneOffset)", ldt_to_epoch_second);
    b.m(CHRONO_LOCAL_DATE_TIME, "Instant toInstant(ZoneOffset)", ldt_to_instant);
    b.m(CHRONO_LOCAL_DATE_TIME, "ChronoLocalDate toLocalDate()", to_local_date_v);
    b.m(CHRONO_ZONED_DATE_TIME, "int compareTo(ChronoZonedDateTime)", t_compare);
    b.m(CHRONO_ZONED_DATE_TIME, "ZoneOffset getOffset()", g_offset);
    b.m(CHRONO_ZONED_DATE_TIME, "ZoneId getZone()", g_zone);
    b.m(CHRONO_ZONED_DATE_TIME, "Instant toInstant()", to_instant_v);
    b.m(CHRONO_ZONED_DATE_TIME, "long toEpochSecond()", to_epoch_second_v);
    b.m(CHRONO_ZONED_DATE_TIME, "ChronoLocalDate toLocalDate()", to_local_date_v);
    b.m(CHRONO_ZONED_DATE_TIME, "ChronoLocalDateTime toLocalDateTime()", to_local_date_time_v);
    b.m(CHRONO_ZONED_DATE_TIME, "ChronoZonedDateTime withEarlierOffsetAtOverlap()", zdt_with_earlier);
    b.m(CHRONO_ZONED_DATE_TIME, "ChronoZonedDateTime withLaterOffsetAtOverlap()", zdt_with_later);
    b.m(CHRONO_ZONED_DATE_TIME, "ChronoZonedDateTime withZoneSameLocal(ZoneId)", zdt_with_zone_same_local);
    b.m(CHRONO_ZONED_DATE_TIME, "ChronoZonedDateTime withZoneSameInstant(ZoneId)", zdt_with_zone_same_instant);

    // shared getters for date/time classes
    let common_getters: &[(&str, N)] = &[
        ("int getDayOfMonth()", g_day_of_month),
        ("DayOfWeek getDayOfWeek()", g_day_of_week),
        ("int getDayOfYear()", g_day_of_year),
        ("Month getMonth()", g_month),
        ("int getMonthValue()", g_month_value),
        ("int getYear()", g_year),
    ];
    let time_getters: &[(&str, N)] = &[("int getHour()", g_hour), ("int getMinute()", g_minute), ("int getNano()", g_nano), ("int getSecond()", g_second)];

    // Instant
    b.sf(INSTANT, "Instant EPOCH", instant_epoch);
    b.sf(INSTANT, "Instant MAX", instant_max);
    b.sf(INSTANT, "Instant MIN", instant_min);
    b.m(INSTANT, "OffsetDateTime atOffset(ZoneOffset)", instant_at_offset);
    b.m(INSTANT, "ZonedDateTime atZone(ZoneId)", instant_at_zone);
    b.m(INSTANT, "int compareTo(Instant)", t_compare);
    b.s(INSTANT, "Instant from(TemporalAccessor)", |_, a| r(convert(cls::INSTANT, arg_tv(&a[0], "temporal")?)?));
    b.m(INSTANT, "long getEpochSecond()", instant_epoch_second);
    b.m(INSTANT, "int getNano()", g_nano);
    b.m(INSTANT, "boolean isAfter(Instant)", t_is_after);
    b.m(INSTANT, "boolean isBefore(Instant)", t_is_before);
    b.m(INSTANT, "Instant minus(TemporalAmount)", t_minus_amount);
    b.m(INSTANT, "Instant minus(long,TemporalUnit)", t_minus_unit);
    b.m(INSTANT, "Instant minusMillis(long)", minus_millis);
    b.m(INSTANT, "Instant minusNanos(long)", minus_nanos);
    b.m(INSTANT, "Instant minusSeconds(long)", minus_seconds);
    b.s(INSTANT, "Instant ofEpochSecond(long)", instant_of_epoch_second);
    b.s(INSTANT, "Instant ofEpochSecond(long,long)", instant_of_epoch_second2);
    b.s(INSTANT, "Instant ofEpochMilli(long)", instant_of_epoch_milli);
    b.s(INSTANT, "Instant parse(CharSequence)", instant_parse);
    b.m(INSTANT, "Instant plus(TemporalAmount)", t_plus_amount);
    b.m(INSTANT, "Instant plus(long,TemporalUnit)", t_plus_unit);
    b.m(INSTANT, "Instant plusMillis(long)", plus_millis);
    b.m(INSTANT, "Instant plusNanos(long)", plus_nanos);
    b.m(INSTANT, "Instant plusSeconds(long)", plus_seconds);
    b.m(INSTANT, "long toEpochMilli()", instant_to_epoch_milli);
    b.m(INSTANT, "Instant truncatedTo(TemporalUnit)", t_truncated);
    b.m(INSTANT, "Instant with(TemporalAdjuster)", t_with_adjuster);
    b.m(INSTANT, "Instant with(TemporalField,long)", t_with_field);

    // Duration
    b.sf(DURATION, "Duration ZERO", d_zero);
    b.m(DURATION, "Duration abs()", d_abs);
    b.s(DURATION, "Duration between(Temporal,Temporal)", d_between);
    b.m(DURATION, "int compareTo(Duration)", d_compare);
    b.m(DURATION, "Duration dividedBy(long)", d_divided);
    b.s(DURATION, "Duration from(TemporalAmount)", d_from);
    b.m(DURATION, "int getNano()", d_get_nano);
    b.m(DURATION, "long getSeconds()", d_get_seconds);
    b.m(DURATION, "boolean isNegative()", d_is_negative);
    b.m(DURATION, "boolean isZero()", d_is_zero);
    b.m(DURATION, "Duration minus(Duration)", d_minus);
    b.m(DURATION, "Duration minus(long,TemporalUnit)", d_minus_unit);
    b.m(DURATION, "Duration minusDays(long)", d_minus_days);
    b.m(DURATION, "Duration minusHours(long)", d_minus_hours);
    b.m(DURATION, "Duration minusMinutes(long)", d_minus_minutes);
    b.m(DURATION, "Duration minusSeconds(long)", d_minus_seconds);
    b.m(DURATION, "Duration minusMillis(long)", d_minus_millis);
    b.m(DURATION, "Duration minusNanos(long)", d_minus_nanos);
    b.m(DURATION, "Duration multipliedBy(long)", d_multiplied);
    b.m(DURATION, "Duration negated()", d_negated);
    b.s(DURATION, "Duration of(long,TemporalUnit)", d_of_unit);
    b.s(DURATION, "Duration ofDays(long)", d_of_days);
    b.s(DURATION, "Duration ofHours(long)", d_of_hours);
    b.s(DURATION, "Duration ofMillis(long)", d_of_millis);
    b.s(DURATION, "Duration ofMinutes(long)", d_of_minutes);
    b.s(DURATION, "Duration ofNanos(long)", d_of_nanos);
    b.s(DURATION, "Duration ofSeconds(long)", d_of_seconds);
    b.s(DURATION, "Duration ofSeconds(long,long)", d_of_seconds2);
    b.s(DURATION, "Duration parse(CharSequence)", d_parse);
    b.m(DURATION, "Duration plus(Duration)", d_plus);
    b.m(DURATION, "Duration plus(long,TemporalUnit)", d_plus_unit);
    b.m(DURATION, "Duration plusDays(long)", d_plus_days);
    b.m(DURATION, "Duration plusHours(long)", d_plus_hours);
    b.m(DURATION, "Duration plusMinutes(long)", d_plus_minutes);
    b.m(DURATION, "Duration plusSeconds(long)", d_plus_seconds);
    b.m(DURATION, "Duration plusMillis(long)", d_plus_millis);
    b.m(DURATION, "Duration plusNanos(long)", d_plus_nanos);
    b.m(DURATION, "long toDays()", d_to_days);
    b.m(DURATION, "long toHours()", d_to_hours);
    b.m(DURATION, "long toMinutes()", d_to_minutes);
    b.m(DURATION, "long toMillis()", d_to_millis);
    b.m(DURATION, "long toNanos()", d_to_nanos);
    b.m(DURATION, "Duration withSeconds(long)", d_with_seconds);
    b.m(DURATION, "Duration withNanos(int)", d_with_nanos);

    // Period
    b.sf(PERIOD, "Period ZERO", p_zero);
    b.s(PERIOD, "Period between(LocalDate,LocalDate)", p_between);
    b.s(PERIOD, "Period from(TemporalAmount)", p_from);
    b.m(PERIOD, "IsoChronology getChronology()", g_chronology);
    b.m(PERIOD, "int getDays()", p_get_days);
    b.m(PERIOD, "int getMonths()", p_get_months);
    b.m(PERIOD, "int getYears()", p_get_years);
    b.s(PERIOD, "Period of(int,int,int)", p_of);
    b.s(PERIOD, "Period ofYears(int)", p_of_years);
    b.s(PERIOD, "Period ofMonths(int)", p_of_months);
    b.s(PERIOD, "Period ofWeeks(int)", p_of_weeks);
    b.s(PERIOD, "Period ofDays(int)", p_of_days);
    b.s(PERIOD, "Period parse(CharSequence)", p_parse);
    b.m(PERIOD, "Period plus(TemporalAmount)", p_plus);
    b.m(PERIOD, "Period plusYears(long)", p_plus_years);
    b.m(PERIOD, "Period plusMonths(long)", p_plus_months);
    b.m(PERIOD, "Period plusDays(long)", p_plus_days);
    b.m(PERIOD, "Period minus(TemporalAmount)", p_minus);
    b.m(PERIOD, "Period minusYears(long)", p_minus_years);
    b.m(PERIOD, "Period minusMonths(long)", p_minus_months);
    b.m(PERIOD, "Period minusDays(long)", p_minus_days);
    b.m(PERIOD, "Period multipliedBy(int)", p_multiplied);
    b.m(PERIOD, "Period negated()", p_negated);
    b.m(PERIOD, "Period normalized()", p_normalized);
    b.m(PERIOD, "long toTotalMonths()", p_total_months);
    b.m(PERIOD, "Period withDays(int)", p_with_days);
    b.m(PERIOD, "Period withMonths(int)", p_with_months);
    b.m(PERIOD, "Period withYears(int)", p_with_years);
    b.m(CHRONO_PERIOD, "boolean isNegative()", p_is_negative);
    b.m(CHRONO_PERIOD, "boolean isZero()", p_is_zero);
    b.s(CHRONO_PERIOD, "ChronoPeriod between(ChronoLocalDate,ChronoLocalDate)", p_between);
    b.m(CHRONO_PERIOD, "Chronology getChronology()", g_chronology);
    b.m(CHRONO_PERIOD, "ChronoPeriod minus(TemporalAmount)", p_minus);
    b.m(CHRONO_PERIOD, "ChronoPeriod multipliedBy(int)", p_multiplied);
    b.m(CHRONO_PERIOD, "ChronoPeriod negated()", p_negated);
    b.m(CHRONO_PERIOD, "ChronoPeriod normalized()", p_normalized);
    b.m(CHRONO_PERIOD, "ChronoPeriod plus(TemporalAmount)", p_plus);

    // LocalDate
    b.sf(LOCAL_DATE, "LocalDate MAX", ld_max);
    b.sf(LOCAL_DATE, "LocalDate MIN", ld_min);
    b.m(LOCAL_DATE, "LocalDateTime atStartOfDay()", ld_at_start_of_day);
    b.m(LOCAL_DATE, "ZonedDateTime atStartOfDay(ZoneId)", ld_at_start_of_day_zone);
    b.m(LOCAL_DATE, "LocalDateTime atTime(LocalTime)", ld_at_time);
    b.m(LOCAL_DATE, "LocalDateTime atTime(int,int)", ld_at_time2);
    b.m(LOCAL_DATE, "LocalDateTime atTime(int,int,int)", ld_at_time3);
    b.m(LOCAL_DATE, "LocalDateTime atTime(int,int,int,int)", ld_at_time4);
    b.s(LOCAL_DATE, "LocalDate from(TemporalAccessor)", |_, a| r(convert(cls::LOCAL_DATE, arg_tv(&a[0], "temporal")?).map_err(|t| jt(t, &["LocalDate.from(LocalDate.java:423)"]))?));
    b.m(LOCAL_DATE, "IsoChronology getChronology()", g_chronology);
    for (sig, f) in common_getters {
        b.m(LOCAL_DATE, sig, *f);
        b.m(LOCAL_DATE_TIME, sig, *f);
        b.m(ZONED_DATE_TIME, sig, *f);
        b.m(OFFSET_DATE_TIME, sig, *f);
    }
    for (sig, f) in time_getters {
        b.m(LOCAL_TIME, sig, *f);
        b.m(LOCAL_DATE_TIME, sig, *f);
        b.m(ZONED_DATE_TIME, sig, *f);
        b.m(OFFSET_DATE_TIME, sig, *f);
        b.m(OFFSET_TIME, sig, *f);
    }
    for (c, n) in [(LOCAL_DATE, "LocalDate"), (LOCAL_DATE_TIME, "LocalDateTime"), (ZONED_DATE_TIME, "ZonedDateTime"), (OFFSET_DATE_TIME, "OffsetDateTime")] {
        b.m(c, &format!("{n} minus(TemporalAmount)"), t_minus_amount);
        b.m(c, &format!("{n} minus(long,TemporalUnit)"), t_minus_unit);
        b.m(c, &format!("{n} plus(TemporalAmount)"), t_plus_amount);
        b.m(c, &format!("{n} plus(long,TemporalUnit)"), t_plus_unit);
        b.m(c, &format!("{n} with(TemporalAdjuster)"), t_with_adjuster);
        b.m(c, &format!("{n} with(TemporalField,long)"), t_with_field);
        b.m(c, &format!("{n} minusYears(long)"), minus_years);
        b.m(c, &format!("{n} minusMonths(long)"), minus_months);
        b.m(c, &format!("{n} minusWeeks(long)"), minus_weeks);
        b.m(c, &format!("{n} minusDays(long)"), minus_days);
        b.m(c, &format!("{n} plusYears(long)"), plus_years);
        b.m(c, &format!("{n} plusMonths(long)"), plus_months);
        b.m(c, &format!("{n} plusWeeks(long)"), plus_weeks);
        b.m(c, &format!("{n} plusDays(long)"), plus_days);
        b.m(c, &format!("{n} withDayOfMonth(int)"), with_day_of_month);
        b.m(c, &format!("{n} withDayOfYear(int)"), with_day_of_year);
        b.m(c, &format!("{n} withMonth(int)"), with_month);
        b.m(c, &format!("{n} withYear(int)"), with_year);
    }
    for (c, n) in [(LOCAL_TIME, "LocalTime"), (LOCAL_DATE_TIME, "LocalDateTime"), (ZONED_DATE_TIME, "ZonedDateTime"), (OFFSET_DATE_TIME, "OffsetDateTime"), (OFFSET_TIME, "OffsetTime")] {
        b.m(c, &format!("{n} minusHours(long)"), minus_hours);
        b.m(c, &format!("{n} minusMinutes(long)"), minus_minutes);
        b.m(c, &format!("{n} minusSeconds(long)"), minus_seconds);
        b.m(c, &format!("{n} minusNanos(long)"), minus_nanos);
        b.m(c, &format!("{n} plusHours(long)"), plus_hours);
        b.m(c, &format!("{n} plusMinutes(long)"), plus_minutes);
        b.m(c, &format!("{n} plusSeconds(long)"), plus_seconds);
        b.m(c, &format!("{n} plusNanos(long)"), plus_nanos);
        b.m(c, &format!("{n} withHour(int)"), with_hour);
        b.m(c, &format!("{n} withMinute(int)"), with_minute);
        b.m(c, &format!("{n} withSecond(int)"), with_second);
        // Elasticsearch's allowlist omits LocalDateTime.withNano
        if c != LOCAL_DATE_TIME {
            b.m(c, &format!("{n} withNano(int)"), with_nano);
        }
        b.m(c, &format!("{n} truncatedTo(TemporalUnit)"), t_truncated);
    }
    b.s(LOCAL_DATE, "LocalDate of(int,int,int)", ld_of);
    b.s(LOCAL_DATE, "LocalDate ofYearDay(int,int)", ld_of_year_day);
    b.s(LOCAL_DATE, "LocalDate ofEpochDay(long)", ld_of_epoch_day);
    b.s(LOCAL_DATE, "LocalDate parse(CharSequence)", ld_parse);
    b.s(LOCAL_DATE, "LocalDate parse(CharSequence,DateTimeFormatter)", ld_parse_fmt);
    b.m(LOCAL_DATE, "Period until(ChronoLocalDate)", ld_until_period);

    // LocalTime
    b.sf(LOCAL_TIME, "LocalTime MAX", lt_max);
    b.sf(LOCAL_TIME, "LocalTime MIDNIGHT", lt_midnight);
    b.sf(LOCAL_TIME, "LocalTime MIN", lt_midnight);
    b.sf(LOCAL_TIME, "LocalTime NOON", lt_noon);
    b.m(LOCAL_TIME, "LocalDateTime atDate(LocalDate)", lt_at_date);
    b.m(LOCAL_TIME, "OffsetTime atOffset(ZoneOffset)", lt_at_offset);
    b.m(LOCAL_TIME, "int compareTo(LocalTime)", t_compare);
    b.m(LOCAL_TIME, "String format(DateTimeFormatter)", t_format);
    b.s(LOCAL_TIME, "LocalTime from(TemporalAccessor)", |_, a| r(convert(cls::LOCAL_TIME, arg_tv(&a[0], "temporal")?)?));
    b.m(LOCAL_TIME, "boolean isAfter(LocalTime)", t_is_after);
    b.m(LOCAL_TIME, "boolean isBefore(LocalTime)", t_is_before);
    b.m(LOCAL_TIME, "LocalTime minus(TemporalAmount)", t_minus_amount);
    b.m(LOCAL_TIME, "LocalTime minus(long,TemporalUnit)", t_minus_unit);
    b.s(LOCAL_TIME, "LocalTime of(int,int)", lt_of2);
    b.s(LOCAL_TIME, "LocalTime of(int,int,int)", lt_of3);
    b.s(LOCAL_TIME, "LocalTime of(int,int,int,int)", lt_of4);
    b.s(LOCAL_TIME, "LocalTime ofNanoOfDay(long)", lt_of_nano_of_day);
    b.s(LOCAL_TIME, "LocalTime ofSecondOfDay(long)", lt_of_second_of_day);
    b.s(LOCAL_TIME, "LocalTime parse(CharSequence)", lt_parse);
    b.s(LOCAL_TIME, "LocalTime parse(CharSequence,DateTimeFormatter)", lt_parse_fmt);
    b.m(LOCAL_TIME, "LocalTime plus(TemporalAmount)", t_plus_amount);
    b.m(LOCAL_TIME, "LocalTime plus(long,TemporalUnit)", t_plus_unit);
    b.m(LOCAL_TIME, "long toNanoOfDay()", lt_to_nano_of_day);
    b.m(LOCAL_TIME, "int toSecondOfDay()", lt_to_second_of_day);
    b.m(LOCAL_TIME, "LocalTime with(TemporalAdjuster)", t_with_adjuster);
    b.m(LOCAL_TIME, "LocalTime with(TemporalField,long)", t_with_field);

    // LocalDateTime
    b.sf(LOCAL_DATE_TIME, "LocalDateTime MIN", ldt_min);
    b.sf(LOCAL_DATE_TIME, "LocalDateTime MAX", ldt_max);
    b.m(LOCAL_DATE_TIME, "OffsetDateTime atOffset(ZoneOffset)", ldt_at_offset);
    b.m(LOCAL_DATE_TIME, "ZonedDateTime atZone(ZoneId)", ldt_at_zone);
    b.s(LOCAL_DATE_TIME, "LocalDateTime from(TemporalAccessor)", |_, a| r(convert(cls::LOCAL_DATE_TIME, arg_tv(&a[0], "temporal")?)?));
    b.s(LOCAL_DATE_TIME, "LocalDateTime of(LocalDate,LocalTime)", ldt_of_dt);
    b.s(LOCAL_DATE_TIME, "LocalDateTime of(int,int,int,int,int)", ldt_of5);
    b.s(LOCAL_DATE_TIME, "LocalDateTime of(int,int,int,int,int,int)", ldt_of6);
    b.s(LOCAL_DATE_TIME, "LocalDateTime of(int,int,int,int,int,int,int)", ldt_of7);
    b.s(LOCAL_DATE_TIME, "LocalDateTime ofInstant(Instant,ZoneId)", ldt_of_instant);
    b.s(LOCAL_DATE_TIME, "LocalDateTime ofEpochSecond(long,int,ZoneOffset)", ldt_of_epoch_second);
    b.s(LOCAL_DATE_TIME, "LocalDateTime parse(CharSequence)", ldt_parse);
    b.s(LOCAL_DATE_TIME, "LocalDateTime parse(CharSequence,DateTimeFormatter)", ldt_parse_fmt);
    b.m(LOCAL_DATE_TIME, "LocalDate toLocalDate()", to_local_date_v);

    // ZonedDateTime: Joda-era getters are allowlisted but only explain their replacement
    let m = b.m(ZONED_DATE_TIME, "DayOfWeek getDayOfWeekEnum()", g_day_of_week);
    b.aug(m, "java.time.ZonedDateTime.getDayOfWeek()");
    macro_rules! gone {
        ($($name:literal => $instead:literal,)*) => {$(
            b.m(ZONED_DATE_TIME, concat!("int ", $name, "()"), |_, _| {
                Err(Throw::new(cls::UNSUPPORTED_OPERATION_EXCEPTION, Some(concat!("[", $name, "] is no longer available; use [", $instead, "] instead").to_string())))
            });
        )*};
    }
    gone! {
        "getCenturyOfEra" => "get(ChronoField.YEAR_OF_ERA) / 100",
        "getEra" => "get(ChronoField.ERA)",
        "getHourOfDay" => "getHour()",
        "getMillisOfDay" => "get(ChronoField.MILLI_OF_DAY)",
        "getMillisOfSecond" => "get(ChronoField.MILLI_OF_SECOND)",
        "getMinuteOfDay" => "get(ChronoField.MINUTE_OF_DAY)",
        "getMinuteOfHour" => "getMinute()",
        "getMonthOfYear" => "getMonthValue()",
        "getSecondOfDay" => "get(ChronoField.SECOND_OF_DAY)",
        "getSecondOfMinute" => "getSecond()",
        "getWeekOfWeekyear" => "get(IsoFields.WEEK_OF_WEEK_BASED_YEAR)",
        "getWeekyear" => "get(IsoFields.WEEK_BASED_YEAR)",
        "getYearOfCentury" => "get(ChronoField.YEAR_OF_ERA) % 100",
        "getYearOfEra" => "get(ChronoField.YEAR_OF_ERA)",
    }
    b.m(ZONED_DATE_TIME, "LocalDate toLocalDate()", to_local_date_v);
    b.m(ZONED_DATE_TIME, "LocalDateTime toLocalDateTime()", to_local_date_time_v);
    b.s(ZONED_DATE_TIME, "ZonedDateTime from(TemporalAccessor)", |_, a| r(convert(cls::ZONED_DATE_TIME, arg_tv(&a[0], "temporal")?).map_err(|t| jt(t, &["ZonedDateTime.from(ZonedDateTime.java:570)"]))?));
    b.s(ZONED_DATE_TIME, "ZonedDateTime of(LocalDate,LocalTime,ZoneId)", zdt_of_dt);
    b.s(ZONED_DATE_TIME, "ZonedDateTime of(LocalDateTime,ZoneId)", zdt_of_ldt);
    b.s(ZONED_DATE_TIME, "ZonedDateTime of(int,int,int,int,int,int,int,ZoneId)", zdt_of8);
    b.s(ZONED_DATE_TIME, "ZonedDateTime ofInstant(Instant,ZoneId)", zdt_of_instant);
    b.s(ZONED_DATE_TIME, "ZonedDateTime ofInstant(LocalDateTime,ZoneOffset,ZoneId)", zdt_of_instant3);
    b.s(ZONED_DATE_TIME, "ZonedDateTime ofLocal(LocalDateTime,ZoneId,ZoneOffset)", zdt_of_local);
    b.s(ZONED_DATE_TIME, "ZonedDateTime ofStrict(LocalDateTime,ZoneOffset,ZoneId)", zdt_of_strict);
    b.s(ZONED_DATE_TIME, "ZonedDateTime parse(CharSequence)", zdt_parse);
    b.s(ZONED_DATE_TIME, "ZonedDateTime parse(CharSequence,DateTimeFormatter)", zdt_parse_fmt);
    b.m(ZONED_DATE_TIME, "OffsetDateTime toOffsetDateTime()", to_offset_date_time_v);
    b.m(ZONED_DATE_TIME, "ZonedDateTime withEarlierOffsetAtOverlap()", zdt_with_earlier);
    b.m(ZONED_DATE_TIME, "ZonedDateTime withFixedOffsetZone()", zdt_with_fixed_offset);
    b.m(ZONED_DATE_TIME, "ZonedDateTime withLaterOffsetAtOverlap()", zdt_with_later);
    b.m(ZONED_DATE_TIME, "ZonedDateTime withZoneSameLocal(ZoneId)", zdt_with_zone_same_local);
    b.m(ZONED_DATE_TIME, "ZonedDateTime withZoneSameInstant(ZoneId)", zdt_with_zone_same_instant);

    // OffsetDateTime
    b.sf(OFFSET_DATE_TIME, "OffsetDateTime MAX", odt_max);
    b.sf(OFFSET_DATE_TIME, "OffsetDateTime MIN", odt_min);
    b.m(OFFSET_DATE_TIME, "ZonedDateTime atZoneSameInstant(ZoneId)", odt_at_zone_same_instant);
    b.m(OFFSET_DATE_TIME, "ZonedDateTime atZoneSimilarLocal(ZoneId)", odt_at_zone_similar_local);
    b.m(OFFSET_DATE_TIME, "int compareTo(OffsetDateTime)", t_compare);
    b.m(OFFSET_DATE_TIME, "String format(DateTimeFormatter)", t_format);
    b.s(OFFSET_DATE_TIME, "OffsetDateTime from(TemporalAccessor)", |_, a| r(convert(cls::OFFSET_DATE_TIME, arg_tv(&a[0], "temporal")?)?));
    b.m(OFFSET_DATE_TIME, "ZoneOffset getOffset()", g_offset);
    b.m(OFFSET_DATE_TIME, "boolean isAfter(OffsetDateTime)", t_is_after);
    b.m(OFFSET_DATE_TIME, "boolean isBefore(OffsetDateTime)", t_is_before);
    b.m(OFFSET_DATE_TIME, "boolean isEqual(OffsetDateTime)", t_is_equal);
    b.s(OFFSET_DATE_TIME, "OffsetDateTime of(LocalDate,LocalTime,ZoneOffset)", odt_of3);
    b.s(OFFSET_DATE_TIME, "OffsetDateTime of(LocalDateTime,ZoneOffset)", odt_of2);
    b.s(OFFSET_DATE_TIME, "OffsetDateTime of(int,int,int,int,int,int,int,ZoneOffset)", odt_of8);
    b.s(OFFSET_DATE_TIME, "OffsetDateTime ofInstant(Instant,ZoneId)", odt_of_instant);
    b.s(OFFSET_DATE_TIME, "OffsetDateTime parse(CharSequence)", odt_parse);
    b.s(OFFSET_DATE_TIME, "OffsetDateTime parse(CharSequence,DateTimeFormatter)", odt_parse_fmt);
    b.s(OFFSET_DATE_TIME, "Comparator timeLineOrder()", odt_time_line_order);
    b.m(OFFSET_DATE_TIME, "long toEpochSecond()", to_epoch_second_v);
    b.m(OFFSET_DATE_TIME, "Instant toInstant()", to_instant_v);
    b.m(OFFSET_DATE_TIME, "LocalDate toLocalDate()", to_local_date_v);
    b.m(OFFSET_DATE_TIME, "LocalDateTime toLocalDateTime()", to_local_date_time_v);
    b.m(OFFSET_DATE_TIME, "LocalTime toLocalTime()", to_local_time_v);
    b.m(OFFSET_DATE_TIME, "OffsetTime toOffsetTime()", to_offset_time_v);
    b.m(OFFSET_DATE_TIME, "ZonedDateTime toZonedDateTime()", to_zoned_v);
    b.m(OFFSET_DATE_TIME, "OffsetDateTime withOffsetSameLocal(ZoneOffset)", odt_with_offset_same_local);
    b.m(OFFSET_DATE_TIME, "OffsetDateTime withOffsetSameInstant(ZoneOffset)", odt_with_offset_same_instant);

    // OffsetTime
    b.sf(OFFSET_TIME, "OffsetTime MAX", ot_max);
    b.sf(OFFSET_TIME, "OffsetTime MIN", ot_min);
    b.m(OFFSET_TIME, "int compareTo(OffsetTime)", t_compare);
    b.m(OFFSET_TIME, "String format(DateTimeFormatter)", t_format);
    b.s(OFFSET_TIME, "OffsetTime from(TemporalAccessor)", |_, a| r(convert(cls::OFFSET_TIME, arg_tv(&a[0], "temporal")?)?));
    b.m(OFFSET_TIME, "ZoneOffset getOffset()", g_offset);
    b.m(OFFSET_TIME, "boolean isAfter(OffsetTime)", t_is_after);
    b.m(OFFSET_TIME, "boolean isBefore(OffsetTime)", t_is_before);
    b.m(OFFSET_TIME, "boolean isEqual(OffsetTime)", t_is_equal);
    b.s(OFFSET_TIME, "OffsetTime of(LocalTime,ZoneOffset)", ot_of2);
    b.s(OFFSET_TIME, "OffsetTime of(int,int,int,int,ZoneOffset)", ot_of5);
    b.s(OFFSET_TIME, "OffsetTime ofInstant(Instant,ZoneId)", ot_of_instant);
    b.m(OFFSET_TIME, "OffsetTime plus(TemporalAmount)", t_plus_amount);
    b.m(OFFSET_TIME, "OffsetTime plus(long,TemporalUnit)", t_plus_unit);
    b.m(OFFSET_TIME, "OffsetTime minus(TemporalAmount)", t_minus_amount);
    b.m(OFFSET_TIME, "OffsetTime minus(long,TemporalUnit)", t_minus_unit);
    b.s(OFFSET_TIME, "OffsetTime parse(CharSequence)", ot_parse);
    b.s(OFFSET_TIME, "OffsetTime parse(CharSequence,DateTimeFormatter)", ot_parse_fmt);
    b.m(OFFSET_TIME, "LocalTime toLocalTime()", to_local_time_v);
    b.m(OFFSET_TIME, "OffsetTime with(TemporalAdjuster)", t_with_adjuster);
    b.m(OFFSET_TIME, "OffsetTime with(TemporalField,long)", t_with_field);
    b.m(OFFSET_TIME, "OffsetTime withOffsetSameLocal(ZoneOffset)", odt_with_offset_same_local);
    b.m(OFFSET_TIME, "OffsetTime withOffsetSameInstant(ZoneOffset)", odt_with_offset_same_instant);

    // ZoneId / ZoneOffset
    b.sf(ZONE_ID, "Map SHORT_IDS", zid_short_ids);
    b.s(ZONE_ID, "Set getAvailableZoneIds()", zid_available);
    b.s(ZONE_ID, "ZoneId of(String)", zid_of);
    b.s(ZONE_ID, "ZoneId of(String,Map)", zid_of_map);
    b.s(ZONE_ID, "ZoneId ofOffset(String,ZoneOffset)", zid_of_offset);
    b.s(ZONE_ID, "ZoneId from(TemporalAccessor)", zid_from);
    b.m(ZONE_ID, "String getId()", zid_get_id);
    b.m(ZONE_ID, "String getDisplayName(TextStyle,Locale)", zid_display_name);
    b.m(ZONE_ID, "ZoneId normalized()", zid_normalized);
    b.s(ZONE_ID, "ZoneId systemDefault()", zid_system_default);
    b.sf(ZONE_OFFSET, "ZoneOffset MAX", zo_max);
    b.sf(ZONE_OFFSET, "ZoneOffset MIN", zo_min);
    b.sf(ZONE_OFFSET, "ZoneOffset UTC", zo_utc);
    b.s(ZONE_OFFSET, "ZoneOffset from(TemporalAccessor)", zo_from);
    b.m(ZONE_OFFSET, "int getTotalSeconds()", zo_total);
    b.s(ZONE_OFFSET, "ZoneOffset of(String)", zo_of);
    b.s(ZONE_OFFSET, "ZoneOffset ofHours(int)", zo_of_hours);
    b.s(ZONE_OFFSET, "ZoneOffset ofHoursMinutes(int,int)", zo_of_hm);
    b.s(ZONE_OFFSET, "ZoneOffset ofHoursMinutesSeconds(int,int,int)", zo_of_hms);
    b.s(ZONE_OFFSET, "ZoneOffset ofTotalSeconds(int)", zo_of_total);
    b.m(ZONE_OFFSET, "int compareTo(ZoneOffset)", t_compare);

    // DayOfWeek / Month
    for (n, d) in [("MONDAY", 1u8), ("TUESDAY", 2), ("WEDNESDAY", 3), ("THURSDAY", 4), ("FRIDAY", 5), ("SATURDAY", 6), ("SUNDAY", 7)] {
        let f: N = match d {
            1 => |_, _| dow_v(1),
            2 => |_, _| dow_v(2),
            3 => |_, _| dow_v(3),
            4 => |_, _| dow_v(4),
            5 => |_, _| dow_v(5),
            6 => |_, _| dow_v(6),
            _ => |_, _| dow_v(7),
        };
        b.sf(DAY_OF_WEEK, &format!("DayOfWeek {n}"), f);
    }
    b.s(DAY_OF_WEEK, "DayOfWeek of(int)", dow_of);
    b.s(DAY_OF_WEEK, "DayOfWeek from(TemporalAccessor)", dow_from);
    b.m(DAY_OF_WEEK, "int getValue()", enum_value);
    b.m(DAY_OF_WEEK, "String getDisplayName(TextStyle,Locale)", dow_display);
    b.m(DAY_OF_WEEK, "DayOfWeek minus(long)", dow_minus);
    b.m(DAY_OF_WEEK, "DayOfWeek plus(long)", dow_plus);
    b.s(DAY_OF_WEEK, "DayOfWeek valueOf(String)", dow_value_of);
    b.s(DAY_OF_WEEK, "DayOfWeek[] values()", dow_values);
    macro_rules! month_consts {
        ($($n:literal = $m:literal),*) => { $( b.sf(MONTH, concat!("Month ", $n), |_, _| month_v($m)); )* };
    }
    month_consts!("JANUARY" = 1, "FEBRUARY" = 2, "MARCH" = 3, "APRIL" = 4, "MAY" = 5, "JUNE" = 6, "JULY" = 7, "AUGUST" = 8, "SEPTEMBER" = 9, "OCTOBER" = 10, "NOVEMBER" = 11, "DECEMBER" = 12);
    b.s(MONTH, "Month from(TemporalAccessor)", month_from);
    b.m(MONTH, "int firstDayOfYear(boolean)", month_first_day_of_year);
    b.m(MONTH, "Month firstMonthOfQuarter()", month_first_of_quarter);
    b.m(MONTH, "int getValue()", enum_value);
    b.m(MONTH, "String getDisplayName(TextStyle,Locale)", month_display);
    b.m(MONTH, "int length(boolean)", month_length);
    b.m(MONTH, "int maxLength()", month_max_length);
    b.m(MONTH, "int minLength()", month_min_length);
    b.m(MONTH, "Month minus(long)", month_minus);
    b.s(MONTH, "Month of(int)", month_of);
    b.m(MONTH, "Month plus(long)", month_plus);
    b.s(MONTH, "Month valueOf(String)", month_value_of);
    b.s(MONTH, "Month[] values()", month_values);

    // Year / YearMonth / MonthDay
    b.sf(YEAR, "int MAX_VALUE", y_max);
    b.sf(YEAR, "int MIN_VALUE", y_min);
    b.m(YEAR, "LocalDate atDay(int)", y_at_day);
    b.m(YEAR, "YearMonth atMonth(int)", y_at_month);
    b.m(YEAR, "LocalDate atMonthDay(MonthDay)", y_at_month_day);
    b.m(YEAR, "int compareTo(Year)", t_compare);
    b.m(YEAR, "String format(DateTimeFormatter)", t_format);
    b.s(YEAR, "Year from(TemporalAccessor)", y_from);
    b.m(YEAR, "int getValue()", enum_value);
    b.m(YEAR, "boolean isAfter(Year)", t_is_after);
    // Year.isBefore is not in Elasticsearch's allowlist (isAfter is)
    b.m(YEAR, "boolean isLeap()", y_is_leap);
    b.s(YEAR, "boolean isLeap(long)", y_is_leap_static);
    b.m(YEAR, "boolean isValidMonthDay(MonthDay)", y_is_valid_month_day);
    b.m(YEAR, "int length()", y_length);
    b.m(YEAR, "Year minus(TemporalAmount)", t_minus_amount);
    b.m(YEAR, "Year minus(long,TemporalUnit)", t_minus_unit);
    b.m(YEAR, "Year minusYears(long)", y_minus_years);
    b.s(YEAR, "Year of(int)", y_of);
    b.s(YEAR, "Year parse(CharSequence)", y_parse);
    b.s(YEAR, "Year parse(CharSequence,DateTimeFormatter)", y_parse_fmt);
    b.m(YEAR, "Year plus(TemporalAmount)", t_plus_amount);
    b.m(YEAR, "Year plus(long,TemporalUnit)", t_plus_unit);
    b.m(YEAR, "Year plusYears(long)", y_plus_years);
    b.m(YEAR, "Year with(TemporalAdjuster)", t_with_adjuster);
    b.m(YEAR, "Year with(TemporalField,long)", t_with_field);
    b.m(YEAR_MONTH, "LocalDate atDay(int)", ym_at_day);
    b.m(YEAR_MONTH, "LocalDate atEndOfMonth()", ym_at_end);
    b.m(YEAR_MONTH, "int compareTo(YearMonth)", t_compare);
    b.m(YEAR_MONTH, "String format(DateTimeFormatter)", t_format);
    b.s(YEAR_MONTH, "YearMonth from(TemporalAccessor)", ym_from);
    b.m(YEAR_MONTH, "Month getMonth()", g_month);
    b.m(YEAR_MONTH, "int getMonthValue()", g_month_value);
    b.m(YEAR_MONTH, "int getYear()", g_year);
    b.m(YEAR_MONTH, "boolean isAfter(YearMonth)", t_is_after);
    b.m(YEAR_MONTH, "boolean isBefore(YearMonth)", t_is_before);
    b.m(YEAR_MONTH, "boolean isLeapYear()", is_leap_year_v);
    b.m(YEAR_MONTH, "boolean isValidDay(int)", ym_is_valid_day);
    b.m(YEAR_MONTH, "int lengthOfMonth()", length_of_month_v);
    b.m(YEAR_MONTH, "int lengthOfYear()", ym_length_of_year);
    b.m(YEAR_MONTH, "YearMonth minus(TemporalAmount)", t_minus_amount);
    b.m(YEAR_MONTH, "YearMonth minus(long,TemporalUnit)", t_minus_unit);
    b.m(YEAR_MONTH, "YearMonth minusYears(long)", ym_minus_years);
    b.m(YEAR_MONTH, "YearMonth minusMonths(long)", ym_minus_months);
    b.s(YEAR_MONTH, "YearMonth of(int,int)", ym_of);
    b.s(YEAR_MONTH, "YearMonth parse(CharSequence)", ym_parse);
    b.s(YEAR_MONTH, "YearMonth parse(CharSequence,DateTimeFormatter)", ym_parse_fmt);
    b.m(YEAR_MONTH, "YearMonth plus(TemporalAmount)", t_plus_amount);
    b.m(YEAR_MONTH, "YearMonth plus(long,TemporalUnit)", t_plus_unit);
    b.m(YEAR_MONTH, "YearMonth plusYears(long)", ym_plus_years);
    b.m(YEAR_MONTH, "YearMonth plusMonths(long)", ym_plus_months);
    b.m(YEAR_MONTH, "YearMonth with(TemporalAdjuster)", t_with_adjuster);
    b.m(YEAR_MONTH, "YearMonth with(TemporalField,long)", t_with_field);
    b.m(YEAR_MONTH, "YearMonth withYear(int)", with_year);
    b.m(YEAR_MONTH, "YearMonth withMonth(int)", with_month);
    b.m(MONTH_DAY, "LocalDate atYear(int)", md_at_year);
    b.m(MONTH_DAY, "int compareTo(MonthDay)", t_compare);
    b.m(MONTH_DAY, "String format(DateTimeFormatter)", t_format);
    b.s(MONTH_DAY, "MonthDay from(TemporalAccessor)", md_from);
    b.m(MONTH_DAY, "int getMonthValue()", g_month_value);
    b.m(MONTH_DAY, "Month getMonth()", g_month);
    b.m(MONTH_DAY, "int getDayOfMonth()", g_day_of_month);
    b.m(MONTH_DAY, "boolean isAfter(MonthDay)", t_is_after);
    b.m(MONTH_DAY, "boolean isBefore(MonthDay)", t_is_before);
    b.m(MONTH_DAY, "boolean isValidYear(int)", md_is_valid_year);
    b.s(MONTH_DAY, "MonthDay of(int,int)", md_of);
    b.s(MONTH_DAY, "MonthDay parse(CharSequence)", md_parse);
    b.s(MONTH_DAY, "MonthDay parse(CharSequence,DateTimeFormatter)", md_parse_fmt);
    b.m(MONTH_DAY, "MonthDay with(Month)", md_with_month);
    b.m(MONTH_DAY, "MonthDay withDayOfMonth(int)", md_with_day);
    b.m(MONTH_DAY, "MonthDay withMonth(int)", md_with_month);

    // ChronoUnit / ChronoField
    for u in Unit::ALL {
        let f: N = match u {
            Unit::Nanos => |_, _| Ok(unit_value(Unit::Nanos)),
            Unit::Micros => |_, _| Ok(unit_value(Unit::Micros)),
            Unit::Millis => |_, _| Ok(unit_value(Unit::Millis)),
            Unit::Seconds => |_, _| Ok(unit_value(Unit::Seconds)),
            Unit::Minutes => |_, _| Ok(unit_value(Unit::Minutes)),
            Unit::Hours => |_, _| Ok(unit_value(Unit::Hours)),
            Unit::HalfDays => |_, _| Ok(unit_value(Unit::HalfDays)),
            Unit::Days => |_, _| Ok(unit_value(Unit::Days)),
            Unit::Weeks => |_, _| Ok(unit_value(Unit::Weeks)),
            Unit::Months => |_, _| Ok(unit_value(Unit::Months)),
            Unit::Years => |_, _| Ok(unit_value(Unit::Years)),
            Unit::Decades => |_, _| Ok(unit_value(Unit::Decades)),
            Unit::Centuries => |_, _| Ok(unit_value(Unit::Centuries)),
            Unit::Millennia => |_, _| Ok(unit_value(Unit::Millennia)),
            Unit::Eras => |_, _| Ok(unit_value(Unit::Eras)),
            Unit::Forever => |_, _| Ok(unit_value(Unit::Forever)),
        };
        b.sf(CHRONO_UNIT, &format!("ChronoUnit {}", u.const_name()), f);
    }
    b.s(CHRONO_UNIT, "ChronoUnit valueOf(String)", unit_value_of);
    b.s(CHRONO_UNIT, "ChronoUnit[] values()", unit_values);
    macro_rules! field_consts {
        ($($v:ident),*) => { $( b.sf(CHRONO_FIELD, &format!("ChronoField {}", Field::$v.const_name()), |_, _| Ok(field_value(Field::$v))); )* };
    }
    field_consts!(
        NanoOfSecond,
        NanoOfDay,
        MicroOfSecond,
        MicroOfDay,
        MilliOfSecond,
        MilliOfDay,
        SecondOfMinute,
        SecondOfDay,
        MinuteOfHour,
        MinuteOfDay,
        HourOfAmPm,
        ClockHourOfAmPm,
        HourOfDay,
        ClockHourOfDay,
        AmPmOfDay,
        DayOfWeek,
        AlignedDayOfWeekInMonth,
        AlignedDayOfWeekInYear,
        DayOfMonth,
        DayOfYear,
        EpochDay,
        AlignedWeekOfMonth,
        AlignedWeekOfYear,
        MonthOfYear,
        ProlepticMonth,
        YearOfEra,
        Year,
        Era,
        InstantSeconds,
        OffsetSeconds
    );
    b.m(CHRONO_FIELD, "int checkValidIntValue(long)", field_check_int);
    b.m(CHRONO_FIELD, "long checkValidValue(long)", field_check);
    b.s(CHRONO_FIELD, "ChronoField valueOf(String)", field_value_of);
    b.s(CHRONO_FIELD, "ChronoField[] values()", field_values);

    // ValueRange
    b.m(VALUE_RANGE, "int checkValidIntValue(long,TemporalField)", vr_check_int);
    b.m(VALUE_RANGE, "long checkValidValue(long,TemporalField)", vr_check);
    b.m(VALUE_RANGE, "long getLargestMinimum()", vr_largest_min);
    b.m(VALUE_RANGE, "long getMaximum()", vr_max);
    b.m(VALUE_RANGE, "long getMinimum()", vr_min);
    b.m(VALUE_RANGE, "long getSmallestMaximum()", vr_smallest_max);
    b.m(VALUE_RANGE, "boolean isFixed()", vr_is_fixed);
    b.m(VALUE_RANGE, "boolean isIntValue()", vr_is_int);
    b.m(VALUE_RANGE, "boolean isValidIntValue(long)", vr_is_valid_int);
    b.m(VALUE_RANGE, "boolean isValidValue(long)", vr_is_valid);
    b.s(VALUE_RANGE, "ValueRange of(long,long)", vr_of2);
    b.s(VALUE_RANGE, "ValueRange of(long,long,long)", vr_of3);
    b.s(VALUE_RANGE, "ValueRange of(long,long,long,long)", vr_of4);

    // TemporalAdjusters
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster dayOfWeekInMonth(int,DayOfWeek)", ta_dow_in_month);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster firstDayOfMonth()", ta_first_day_of_month);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster firstDayOfNextMonth()", ta_first_day_of_next_month);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster firstDayOfNextYear()", ta_first_day_of_next_year);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster firstDayOfYear()", ta_first_day_of_year);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster firstInMonth(DayOfWeek)", ta_first_in_month);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster lastDayOfMonth()", ta_last_day_of_month);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster lastDayOfYear()", ta_last_day_of_year);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster lastInMonth(DayOfWeek)", ta_last_in_month);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster next(DayOfWeek)", ta_next);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster nextOrSame(DayOfWeek)", ta_next_or_same);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster ofDateAdjuster(UnaryOperator)", ta_of_date_adjuster);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster previous(DayOfWeek)", ta_previous);
    b.s(TEMPORAL_ADJUSTERS, "TemporalAdjuster previousOrSame(DayOfWeek)", ta_previous_or_same);

    // Clock
    b.s(CLOCK, "Clock fixed(Instant,ZoneId)", clock_fixed);
    let m = b.m(CLOCK, "ZoneId getZone()", g_zone);
    b.nondeterministic(m);
    let m = b.m(CLOCK, "Instant instant()", clock_instant);
    b.nondeterministic(m);
    let m = b.m(CLOCK, "long millis()", clock_millis);
    b.nondeterministic(m);
    let m = b.s(CLOCK, "Clock offset(Clock,Duration)", clock_offset);
    b.nondeterministic(m);
    let m = b.s(CLOCK, "Clock tick(Clock,Duration)", clock_tick);
    b.nondeterministic(m);

    // styles
    b.sf(TEXT_STYLE, "TextStyle FULL", ts_full);
    b.sf(TEXT_STYLE, "TextStyle FULL_STANDALONE", ts_full_sa);
    b.sf(TEXT_STYLE, "TextStyle SHORT", ts_short);
    b.sf(TEXT_STYLE, "TextStyle SHORT_STANDALONE", ts_short_sa);
    b.sf(TEXT_STYLE, "TextStyle NARROW", ts_narrow);
    b.sf(TEXT_STYLE, "TextStyle NARROW_STANDALONE", ts_narrow_sa);
    b.sf(RESOLVER_STYLE, "ResolverStyle STRICT", rs_strict);
    b.sf(RESOLVER_STYLE, "ResolverStyle SMART", rs_smart);
    b.sf(RESOLVER_STYLE, "ResolverStyle LENIENT", rs_lenient);
    b.sf(FORMAT_STYLE, "FormatStyle FULL", fs_full);
    b.sf(FORMAT_STYLE, "FormatStyle LONG", fs_long);
    b.sf(FORMAT_STYLE, "FormatStyle MEDIUM", fs_medium);
    b.sf(FORMAT_STYLE, "FormatStyle SHORT", fs_short);
    register_style_enums(b);
    register_chronology(b);

    // java.util.Date
    let m = b.c(DATE, "()", ud_new);
    b.nondeterministic(m);
    b.c(DATE, "(long)", ud_new_ms);
    b.m(DATE, "boolean after(Date)", ud_after);
    b.m(DATE, "boolean before(Date)", ud_before);
    b.m(DATE, "def clone()", ud_clone);
    b.m(DATE, "int compareTo(Date)", ud_compare);
    b.s(DATE, "Date from(Instant)", ud_from);
    b.m(DATE, "long getTime()", ud_get_time);
    b.m(DATE, "void setTime(long)", ud_set_time);

    format::register(b);
}
