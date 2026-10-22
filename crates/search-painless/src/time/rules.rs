//! `java.time.zone.ZoneRules` and `ZoneOffsetTransition`.
//!
//! Rules are a zone's offsets over time, read from the tz database through `chrono-tz` (which
//! lists explicit transitions up to 2099). Transitions are found by stepping a day at a time
//! from an instant and bisecting to the second where the wall offset changes: the JDK's
//! transitions are changes of the total offset, so a change of the standard offset that keeps
//! the wall offset is not one. No zone switches its offset twice within a day, so the daily step
//! cannot skip a transition pair.

use super::*;
use crate::lookup::Builder;
use crate::types::cls;
use crate::value::Value;
use crate::vm::{iae, npe, Thrown, Vm, VR};

/// Instants scanned for transitions: the start of the tz data's LMT history to the end of its
/// explicit transitions.
const SCAN_MIN: i64 = -5_364_662_400; // 1800-01-01
const SCAN_MAX: i64 = 4_102_444_800; // 2100-01-01: beyond it, the last rules repeat yearly
const DAY: i64 = 86_400;

/// Whether a zone's rules never change its offset (`ZoneRules.isFixedOffset`).
pub fn is_fixed(z: &Zone) -> bool {
    match z {
        Zone::Offset(_) | Zone::Region(_, None, _) => true,
        Zone::Region(id, Some(_), _) => id.starts_with("Etc/"),
    }
}

/// The first transition strictly after `epoch`: (epoch second, offset before, offset after).
pub fn next_transition(z: &Zone, epoch: i64) -> Option<(i64, i32, i32)> {
    if is_fixed(z) {
        return None;
    }
    let start = epoch.max(SCAN_MIN - DAY);
    // past the explicit data the rules repeat every year: two years cover the next one
    let end = SCAN_MAX.max(start + 2 * 366 * DAY);
    let base = z.offset_at(start);
    let mut lo = start;
    loop {
        let hi = lo + DAY;
        if hi > end + DAY {
            return None;
        }
        if z.offset_at(hi) != base {
            // the offset changes in (lo, hi]: find the first second with the new offset
            let (mut a, mut b) = (lo, hi);
            while b - a > 1 {
                let m = a + (b - a) / 2;
                if z.offset_at(m) == base {
                    a = m;
                } else {
                    b = m;
                }
            }
            return Some((b, base, z.offset_at(b)));
        }
        lo = hi;
    }
}

/// The last transition at or before `epoch` (strictly before when `strict`).
pub fn previous_transition(z: &Zone, epoch: i64, strict: bool) -> Option<(i64, i32, i32)> {
    if is_fixed(z) {
        return None;
    }
    // transitions at or before `limit`
    let limit = if strict { epoch - 1 } else { epoch };
    if limit < SCAN_MIN {
        return None;
    }
    let base = z.offset_at(limit);
    let mut hi = limit;
    loop {
        let lo = hi - DAY;
        if lo < SCAN_MIN - DAY {
            return None;
        }
        if z.offset_at(lo) != base {
            let (mut a, mut b) = (lo, hi);
            while b - a > 1 {
                let m = a + (b - a) / 2;
                if z.offset_at(m) == base {
                    b = m;
                } else {
                    a = m;
                }
            }
            return Some((b, z.offset_at(a), base));
        }
        hi = lo;
    }
}

/// The transition a local date-time falls into (a gap or an overlap), if any.
pub fn transition_at_local(z: &Zone, local_secs: i64) -> Option<(i64, i32, i32)> {
    if is_fixed(z) {
        return None;
    }
    // every transition covering `local` happens within a day of it (offsets are at most 18h)
    let mut from = local_secs - 2 * DAY;
    while let Some(tr) = next_transition(z, from) {
        let (t, before, after) = tr;
        if t > local_secs + 2 * DAY {
            return None;
        }
        let (a, b) = (t + before as i64, t + after as i64);
        let covered = if after > before { a <= local_secs && local_secs < b } else { b <= local_secs && local_secs < a };
        if covered {
            return Some(tr);
        }
        from = t;
    }
    None
}

/// `ZoneRules.equals`: the same offsets over time (links such as `US/Eastern` share their
/// target's rules).
pub fn same_rules(a: &Zone, b: &Zone) -> bool {
    if a == b {
        return true;
    }
    if is_fixed(a) || is_fixed(b) {
        return is_fixed(a) && is_fixed(b) && a.offset_at(0) == b.offset_at(0);
    }
    if a.offset_at(SCAN_MIN) != b.offset_at(SCAN_MIN) {
        return false;
    }
    let (mut x, mut y) = (next_transition(a, SCAN_MIN), next_transition(b, SCAN_MIN));
    loop {
        match (x, y) {
            (None, None) => return true,
            (Some(p), Some(q)) if p == q => {
                if p.0 >= SCAN_MAX {
                    return true;
                }
                x = next_transition(a, p.0);
                y = next_transition(b, q.0);
            }
            _ => return false,
        }
    }
}

/// Standard (non-daylight) offset at an instant.
pub fn standard_offset(z: &Zone, epoch: i64) -> i32 {
    match z {
        Zone::Region(_, Some(tz), _) => tz_offsets(tz, epoch).1,
        _ => z.offset_at(epoch),
    }
}

// ------------------------------------------------------------------------------ natives

fn rules_zone(v: &Value) -> Result<Zone, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            crate::value::Obj::Time(TimeVal::Rules(z)) => Ok(z.clone()),
            _ => Err(crate::vm::cce("not a ZoneRules")),
        },
        _ => Err(npe("rules")),
    }
}

fn transition(v: &Value) -> Result<(i64, i32, i32), Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            crate::value::Obj::Time(TimeVal::Transition { epoch, before, after }) => Ok((*epoch, *before, *after)),
            _ => Err(crate::vm::cce("not a ZoneOffsetTransition")),
        },
        _ => Err(npe("transition")),
    }
}

fn instant_arg(v: &Value) -> Result<(i64, i32), Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            crate::value::Obj::Time(t @ TimeVal::Instant { .. }) => Ok(t.instant().unwrap()),
            _ => Err(crate::vm::cce("not an Instant")),
        },
        _ => Err(npe("Cannot invoke \"java.time.Instant.getEpochSecond()\" because \"instant\" is null")),
    }
}

fn local_arg(v: &Value) -> Result<(Date, Time), Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            crate::value::Obj::Time(TimeVal::LocalDateTime(d, t)) => Ok((*d, *t)),
            _ => Err(crate::vm::cce("not a LocalDateTime")),
        },
        _ => Err(npe("localDateTime")),
    }
}

fn offset_arg(v: &Value) -> Result<i32, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            crate::value::Obj::Time(TimeVal::ZoneId(Zone::Offset(x))) => Ok(*x),
            _ => Err(crate::vm::cce("not a ZoneOffset")),
        },
        _ => Err(npe("offset")),
    }
}

fn offset_v(o: i32) -> Value {
    time_value(TimeVal::ZoneId(Zone::Offset(o)))
}

fn transition_v(t: Option<(i64, i32, i32)>) -> Value {
    match t {
        Some((epoch, before, after)) => time_value(TimeVal::Transition { epoch, before, after }),
        None => Value::Null,
    }
}

fn local_secs(d: Date, t: Time) -> i64 {
    d.epoch_day() * 86_400 + t.second_of_day()
}

fn zid_rules(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Obj(o) => match &**o {
            crate::value::Obj::Time(TimeVal::ZoneId(z)) => Ok(time_value(TimeVal::Rules(z.clone()))),
            _ => Err(crate::vm::cce("not a ZoneId")),
        },
        _ => Err(npe("zone")),
    }
}
fn zr_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(time_value(TimeVal::Rules(Zone::Offset(offset_arg(&a[0])?))))
}
fn zr_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = rules_zone(&a[0])?;
    let (s, _) = instant_arg(&a[1])?;
    Ok(offset_v(z.offset_at(s)))
}
fn zr_standard_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = rules_zone(&a[0])?;
    let (s, _) = instant_arg(&a[1])?;
    Ok(offset_v(standard_offset(&z, s)))
}
fn zr_daylight_savings(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = rules_zone(&a[0])?;
    let (s, _) = instant_arg(&a[1])?;
    Ok(time_value(TimeVal::Duration { secs: (z.offset_at(s) - standard_offset(&z, s)) as i64, nanos: 0 }))
}
fn zr_is_daylight(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = rules_zone(&a[0])?;
    let (s, _) = instant_arg(&a[1])?;
    Ok(Value::Bool(z.offset_at(s) != standard_offset(&z, s)))
}
fn zr_is_fixed(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(is_fixed(&rules_zone(&a[0])?)))
}
fn zr_next(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = rules_zone(&a[0])?;
    let (s, _) = instant_arg(&a[1])?;
    Ok(transition_v(next_transition(&z, s)))
}
fn zr_previous(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = rules_zone(&a[0])?;
    let (s, n) = instant_arg(&a[1])?;
    // an instant past a transition's second includes that transition
    Ok(transition_v(if n > 0 { previous_transition(&z, s, false) } else { previous_transition(&z, s, true) }))
}
fn zr_transition(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = rules_zone(&a[0])?;
    let (d, t) = local_arg(&a[1])?;
    Ok(transition_v(transition_at_local(&z, local_secs(d, t))))
}
fn zr_valid_offsets(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = rules_zone(&a[0])?;
    let (d, t) = local_arg(&a[1])?;
    let offsets: Vec<Value> = z.valid_offsets(d, t).into_iter().map(offset_v).collect();
    Ok(crate::value::JList::array_list(offsets))
}
fn zr_is_valid_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = rules_zone(&a[0])?;
    let (d, t) = local_arg(&a[1])?;
    let o = offset_arg(&a[2])?;
    Ok(Value::Bool(z.valid_offsets(d, t).contains(&o)))
}
fn zr_empty_list(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(crate::value::JList::array_list(Vec::new()))
}

fn tr_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let (d, t) = local_arg(&a[0])?;
    let (before, after) = (offset_arg(&a[1])?, offset_arg(&a[2])?);
    if before == after {
        return Err(iae("Offsets must not be equal").with_frames(&["java.base/java.time.zone.ZoneOffsetTransition.of(ZoneOffsetTransition.java:143)"]));
    }
    if t.nano() != 0 {
        return Err(iae("Nano-of-second must be zero").with_frames(&["java.base/java.time.zone.ZoneOffsetTransition.of(ZoneOffsetTransition.java:146)"]));
    }
    Ok(time_value(TimeVal::Transition { epoch: local_secs(d, t) - before as i64, before, after }))
}
fn tr_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let (e, _, _) = transition(&a[0])?;
    Ok(time_value(TimeVal::Instant { secs: e, nanos: 0 }))
}
fn tr_epoch(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(transition(&a[0])?.0))
}
fn local_of(secs: i64) -> TimeVal {
    let (d, t) = local_of_epoch(secs, 0, 0);
    TimeVal::LocalDateTime(d, t)
}
fn tr_before(_: &mut Vm, a: &mut [Value]) -> VR {
    let (e, b, _) = transition(&a[0])?;
    Ok(time_value(local_of(e + b as i64)))
}
fn tr_after(_: &mut Vm, a: &mut [Value]) -> VR {
    let (e, _, af) = transition(&a[0])?;
    Ok(time_value(local_of(e + af as i64)))
}
fn tr_offset_before(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(offset_v(transition(&a[0])?.1))
}
fn tr_offset_after(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(offset_v(transition(&a[0])?.2))
}
fn tr_duration(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, b, af) = transition(&a[0])?;
    Ok(time_value(TimeVal::Duration { secs: (af - b) as i64, nanos: 0 }))
}
fn tr_is_gap(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, b, af) = transition(&a[0])?;
    Ok(Value::Bool(af > b))
}
fn tr_is_overlap(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, b, af) = transition(&a[0])?;
    Ok(Value::Bool(af < b))
}
fn tr_is_valid_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let (_, b, af) = transition(&a[0])?;
    if af > b {
        return Ok(Value::Bool(false));
    }
    let o = offset_arg(&a[1])?;
    Ok(Value::Bool(o == b || o == af))
}
fn tr_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    let (x, _, _) = transition(&a[0])?;
    let (y, _, _) = transition(&a[1])?;
    Ok(Value::Int(x.cmp(&y) as i32))
}

/// `ZoneOffsetTransition.toString()`.
pub fn transition_string(epoch: i64, before: i32, after: i32) -> String {
    format!(
        "Transition[{} at {}{} to {}]",
        if after > before { "Gap" } else { "Overlap" },
        local_of(epoch + before as i64).to_java_string(),
        offset_id(before),
        offset_id(after)
    )
}

/// `ZoneRules.toString()`: the latest standard offset of the zone's history (read in 2040,
/// where the precomputed Ramadan offsets of Morocco still tell its standard time apart).
pub fn rules_string(z: &Zone) -> String {
    let std = if is_fixed(z) { z.offset_at(0) } else { standard_offset(z, 2_208_988_800) };
    format!("ZoneRules[currentStandardOffset={}]", offset_id(std))
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.supers(ZONE_RULES, &[OBJECT]);
    b.supers(ZONE_OFFSET_TRANSITION, &[OBJECT, COMPARABLE]);
    b.m(ZONE_ID, "ZoneRules getRules()", zid_rules);
    b.s(ZONE_RULES, "ZoneRules of(ZoneOffset)", zr_of);
    b.m(ZONE_RULES, "Duration getDaylightSavings(Instant)", zr_daylight_savings);
    b.m(ZONE_RULES, "ZoneOffset getOffset(Instant)", zr_offset);
    b.m(ZONE_RULES, "ZoneOffset getStandardOffset(Instant)", zr_standard_offset);
    b.m(ZONE_RULES, "ZoneOffsetTransition getTransition(LocalDateTime)", zr_transition);
    b.m(ZONE_RULES, "List getTransitionRules()", zr_empty_list);
    b.m(ZONE_RULES, "List getValidOffsets(LocalDateTime)", zr_valid_offsets);
    b.m(ZONE_RULES, "boolean isDaylightSavings(Instant)", zr_is_daylight);
    b.m(ZONE_RULES, "boolean isFixedOffset()", zr_is_fixed);
    b.m(ZONE_RULES, "boolean isValidOffset(LocalDateTime,ZoneOffset)", zr_is_valid_offset);
    b.m(ZONE_RULES, "ZoneOffsetTransition nextTransition(Instant)", zr_next);
    b.m(ZONE_RULES, "ZoneOffsetTransition previousTransition(Instant)", zr_previous);
    b.s(ZONE_OFFSET_TRANSITION, "ZoneOffsetTransition of(LocalDateTime,ZoneOffset,ZoneOffset)", tr_of);
    b.m(ZONE_OFFSET_TRANSITION, "int compareTo(ZoneOffsetTransition)", tr_compare);
    b.m(ZONE_OFFSET_TRANSITION, "LocalDateTime getDateTimeAfter()", tr_after);
    b.m(ZONE_OFFSET_TRANSITION, "LocalDateTime getDateTimeBefore()", tr_before);
    b.m(ZONE_OFFSET_TRANSITION, "Duration getDuration()", tr_duration);
    b.m(ZONE_OFFSET_TRANSITION, "Instant getInstant()", tr_instant);
    b.m(ZONE_OFFSET_TRANSITION, "ZoneOffset getOffsetAfter()", tr_offset_after);
    b.m(ZONE_OFFSET_TRANSITION, "ZoneOffset getOffsetBefore()", tr_offset_before);
    b.m(ZONE_OFFSET_TRANSITION, "boolean isGap()", tr_is_gap);
    b.m(ZONE_OFFSET_TRANSITION, "boolean isOverlap()", tr_is_overlap);
    b.m(ZONE_OFFSET_TRANSITION, "boolean isValidOffset(ZoneOffset)", tr_is_valid_offset);
    b.m(ZONE_OFFSET_TRANSITION, "long toEpochSecond()", tr_epoch);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paris() -> Zone {
        Zone::Region("Europe/Paris".into(), Some(chrono_tz::Tz::Europe__Paris), 0)
    }

    #[test]
    fn transitions() {
        // captured: nextTransition(EPOCH) = Transition[Gap at 1976-03-28T01:00+01:00 to +02:00]
        let t = next_transition(&paris(), 0).unwrap();
        assert_eq!(transition_string(t.0, t.1, t.2), "Transition[Gap at 1976-03-28T01:00+01:00 to +02:00]");
        let p = previous_transition(&paris(), t.0, true).unwrap();
        assert!(p.0 < t.0);
        assert_eq!(previous_transition(&paris(), t.0, false).unwrap().0, t.0);
        assert!(next_transition(&Zone::Offset(3600), 0).is_none());
        // 2021-03-28T02:30 local is in the spring-forward gap
        let local = Date { y: 2021, m: 3, d: 28 }.epoch_day() * 86_400 + 2 * 3600 + 1800;
        let g = transition_at_local(&paris(), local).unwrap();
        assert_eq!(transition_string(g.0, g.1, g.2), "Transition[Gap at 2021-03-28T02:00+01:00 to +02:00]");
        assert_eq!(rules_string(&paris()), "ZoneRules[currentStandardOffset=+01:00]");
    }
}
