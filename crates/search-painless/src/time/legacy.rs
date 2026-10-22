//! The pre-`java.time` date API scripts still use: `java.util.TimeZone` (the JDK's
//! `sun.util.calendar.ZoneInfo` and `SimpleTimeZone`), `Calendar` / `GregorianCalendar` and
//! `Calendar.Builder`. `java.text.DateFormat` and friends live in [`super::legacy_fmt`].
//!
//! Zones come from `timezones.tsv`, the JDK's own list of `TimeZone` ids with each zone's raw
//! offset, DST savings, last rule and `ZoneId`; offsets at an instant are read from the
//! `ZoneId`'s tz rules (split into standard and daylight time as the JDK compiles them), so a
//! `TimeZone` and its `toZoneId()` always agree.
//!
//! A calendar keeps the JDK's state machine: the time in milliseconds, the 17 field values
//! with a "stamp" per field recording when it was set, and the flags telling which side is
//! current. `set` only records a field; `get` and `getTimeInMillis` recompute lazily, resolving
//! conflicting fields by the most recently set combination (`Calendar.selectFields`), so
//! lenient arithmetic (`set(MONTH, 13)`, February 30th) and non-lenient validation errors
//! (`MONTH: 1 -> 2`) come out as in Java. Dates before the Gregorian cutover (October 15,
//! 1582 by default) are Julian, as in `GregorianCalendar`; days are counted as epoch days.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

use super::{rules, Zone, TimeVal};
use crate::types::cls;
use crate::value::{Obj, Value};
use crate::vm::{iae, Throw, Thrown, Vm, VR};

pub const MS_DAY: i64 = 86_400_000;
const MS_HOUR: i64 = 3_600_000;

// ------------------------------------------------------------------------------ zone table

/// A row of `timezones.tsv`.
#[derive(Debug)]
pub struct ZoneRow {
    pub id: &'static str,
    pub raw: i32,
    pub dst: i32,
    pub use_daylight: bool,
    pub transitions: u32,
    pub observes: bool,
    pub zone_id: &'static str,
    pub last_rule: Option<[i32; 13]>,
}

struct ZoneTable {
    rows: Vec<ZoneRow>,
    by_id: HashMap<&'static str, usize>,
}

fn table() -> &'static ZoneTable {
    static T: OnceLock<ZoneTable> = OnceLock::new();
    T.get_or_init(|| {
        let mut rows = Vec::new();
        for line in include_str!("timezones.tsv").lines() {
            if line.starts_with('#') || line.is_empty() {
                continue;
            }
            let f: Vec<&'static str> = line.split('\t').collect();
            let last_rule = if f[7] == "-" {
                None
            } else {
                let v: Vec<i32> = f[7].split(',').map(|x| x.parse().unwrap_or(0)).collect();
                let mut a = [0; 13];
                a.copy_from_slice(&v[..13]);
                Some(a)
            };
            rows.push(ZoneRow {
                id: f[0],
                raw: f[1].parse().unwrap_or(0),
                dst: f[2].parse().unwrap_or(0),
                use_daylight: f[3] == "1",
                transitions: f[4].parse().unwrap_or(0),
                observes: f[5] == "1",
                zone_id: f[6],
                last_rule,
            });
        }
        let by_id = rows.iter().enumerate().map(|(i, r)| (r.id, i)).collect();
        ZoneTable { rows, by_id }
    })
}

/// `TimeZone.getAvailableIDs()`, in the JDK's order.
pub fn available_ids() -> impl Iterator<Item = &'static ZoneRow> {
    table().rows.iter()
}

// ------------------------------------------------------------------------------ SimpleTimeZone rules

const DOM_MODE: i32 = 1;
const DOW_IN_MONTH_MODE: i32 = 2;
const DOW_GE_DOM_MODE: i32 = 3;
const DOW_LE_DOM_MODE: i32 = 4;
pub const WALL_TIME: i32 = 0;
pub const STANDARD_TIME: i32 = 1;
pub const UTC_TIME: i32 = 2;

const MONTH_LEN: [i32; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// The daylight saving rule of a `SimpleTimeZone` (its fields as `toString` prints them).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SimpleRule {
    pub start_year: i32,
    pub start_mode: i32,
    pub start_month: i32,
    pub start_day: i32,
    pub start_dow: i32,
    pub start_time: i32,
    pub start_time_mode: i32,
    pub end_mode: i32,
    pub end_month: i32,
    pub end_day: i32,
    pub end_dow: i32,
    pub end_time: i32,
    pub end_time_mode: i32,
    pub dst_savings: i32,
    pub use_daylight: bool,
}

impl SimpleRule {
    fn from_row(r: &[i32; 13], dst: i32) -> SimpleRule {
        SimpleRule {
            start_year: r[0],
            start_mode: r[1],
            start_month: r[2],
            start_day: r[3],
            start_dow: r[4],
            start_time: r[5],
            start_time_mode: r[6],
            end_mode: r[7],
            end_month: r[8],
            end_day: r[9],
            end_dow: r[10],
            end_time: r[11],
            end_time_mode: r[12],
            dst_savings: dst,
            use_daylight: true,
        }
    }

    /// `SimpleTimeZone.decodeStartRule`: the constructor/`setStartRule` encoding of the mode in
    /// the signs of the day and day-of-week arguments, validated with the JDK's messages.
    fn decode_start(&mut self) -> Result<(), Thrown> {
        self.use_daylight = self.start_day != 0 && self.end_day != 0;
        if self.start_day != 0 {
            if !(0..=11).contains(&self.start_month) {
                return Err(iae(format!("Illegal start month {}", self.start_month)));
            }
            if self.start_time < 0 || self.start_time as i64 > MS_DAY {
                return Err(iae(format!("Illegal start time {}", self.start_time)));
            }
            if self.start_dow == 0 {
                self.start_mode = DOM_MODE;
            } else {
                if self.start_dow > 0 {
                    self.start_mode = DOW_IN_MONTH_MODE;
                } else {
                    self.start_dow = -self.start_dow;
                    if self.start_day > 0 {
                        self.start_mode = DOW_GE_DOM_MODE;
                    } else {
                        self.start_day = -self.start_day;
                        self.start_mode = DOW_LE_DOM_MODE;
                    }
                }
                if self.start_dow > 7 {
                    return Err(iae(format!("Illegal start day of week {}", self.start_dow)));
                }
            }
            if self.start_mode == DOW_IN_MONTH_MODE {
                if !(-5..=5).contains(&self.start_day) {
                    return Err(iae(format!("Illegal start day of week in month {}", self.start_day)));
                }
            } else if self.start_day < 1 || self.start_day > MONTH_LEN[self.start_month as usize] {
                return Err(iae(format!("Illegal start day {}", self.start_day)));
            }
        }
        Ok(())
    }

    fn decode_end(&mut self) -> Result<(), Thrown> {
        self.use_daylight = self.start_day != 0 && self.end_day != 0;
        if self.end_day != 0 {
            if !(0..=11).contains(&self.end_month) {
                return Err(iae(format!("Illegal end month {}", self.end_month)));
            }
            if self.end_time < 0 || self.end_time as i64 > MS_DAY {
                return Err(iae(format!("Illegal end time {}", self.end_time)));
            }
            if self.end_dow == 0 {
                self.end_mode = DOM_MODE;
            } else {
                if self.end_dow > 0 {
                    self.end_mode = DOW_IN_MONTH_MODE;
                } else {
                    self.end_dow = -self.end_dow;
                    if self.end_day > 0 {
                        self.end_mode = DOW_GE_DOM_MODE;
                    } else {
                        self.end_day = -self.end_day;
                        self.end_mode = DOW_LE_DOM_MODE;
                    }
                }
                if self.end_dow > 7 {
                    return Err(iae(format!("Illegal end day of week {}", self.end_dow)));
                }
            }
            if self.end_mode == DOW_IN_MONTH_MODE {
                if !(-5..=5).contains(&self.end_day) {
                    return Err(iae(format!("Illegal end day of week in month {}", self.end_day)));
                }
            } else if self.end_day < 1 || self.end_day > MONTH_LEN[self.end_month as usize] {
                return Err(iae(format!("Illegal end day {}", self.end_day)));
            }
        }
        Ok(())
    }

    /// Epoch day of a rule's transition date in `year` (Gregorian).
    fn rule_day(year: i64, month: i32, day: i32, dow: i32, mode: i32) -> i64 {
        let m = month as i64 + 1;
        match mode {
            DOM_MODE => gregorian_ed(year, m, day as i64),
            DOW_IN_MONTH_MODE => {
                if day > 0 {
                    let first = gregorian_ed(year, m, 1);
                    on_or_after(first, dow) + 7 * (day as i64 - 1)
                } else {
                    let last = gregorian_ed(year, m, 1) + month_len_g(year, m) as i64 - 1;
                    on_or_before(last, dow) + 7 * (day as i64 + 1)
                }
            }
            DOW_GE_DOM_MODE => on_or_after(gregorian_ed(year, m, day as i64), dow),
            _ => on_or_before(gregorian_ed(year, m, day as i64), dow),
        }
    }

    /// Whether the rule has daylight time at a UTC instant for a zone of standard offset `raw`.
    fn in_dst(&self, raw: i32, utc_ms: i64) -> bool {
        if !self.use_daylight {
            return false;
        }
        let std_local = utc_ms + raw as i64;
        let year = gregorian_from_ed(std_local.div_euclid(MS_DAY)).0;
        if year < self.start_year as i64 {
            return false;
        }
        let to_utc = |ed: i64, time: i32, mode: i32, in_dst: bool| -> i64 {
            let local = ed * MS_DAY + time as i64;
            match mode {
                UTC_TIME => local,
                STANDARD_TIME => local - raw as i64,
                _ => local - raw as i64 - if in_dst { self.dst_savings as i64 } else { 0 },
            }
        };
        let start = to_utc(Self::rule_day(year, self.start_month, self.start_day, self.start_dow, self.start_mode), self.start_time, self.start_time_mode, false);
        let end = to_utc(Self::rule_day(year, self.end_month, self.end_day, self.end_dow, self.end_mode), self.end_time, self.end_time_mode, true);
        if start <= end {
            start <= utc_ms && utc_ms < end
        } else {
            // southern hemisphere: daylight time spans the new year
            !(end <= utc_ms && utc_ms < start)
        }
    }

    fn write_fields(&self, out: &mut String) {
        out.push_str(&format!(
            "startYear={},startMode={},startMonth={},startDay={},startDayOfWeek={},startTime={},startTimeMode={},endMode={},endMonth={},endDay={},endDayOfWeek={},endTime={},endTimeMode={}",
            self.start_year,
            self.start_mode,
            self.start_month,
            self.start_day,
            self.start_dow,
            self.start_time,
            self.start_time_mode,
            self.end_mode,
            self.end_month,
            self.end_day,
            self.end_dow,
            self.end_time,
            self.end_time_mode
        ));
    }
}

// ------------------------------------------------------------------------------ TimeZone

#[derive(Clone, Debug)]
pub enum TzKind {
    /// A `ZoneInfo` with tz rules (the `ZoneId` its `toZoneId()` returns).
    Region { zone: Zone, row: &'static ZoneRow },
    /// A `ZoneInfo` without tz data here: its raw offset and, if it has one, its last rule.
    Table { row: &'static ZoneRow, rule: Option<SimpleRule> },
    /// A custom `GMT+hh:mm` `ZoneInfo`: a fixed offset.
    Custom,
    /// A `java.util.SimpleTimeZone`.
    Simple(SimpleRule),
}

/// A `TimeZone`. `raw` is the raw offset in milliseconds (`setRawOffset` moves it; a
/// `ZoneInfo` then shifts all its offsets by the difference, as the JDK does).
#[derive(Clone, Debug)]
pub struct JTimeZone {
    pub id: String,
    pub raw: i32,
    pub kind: TzKind,
}

impl JTimeZone {
    /// The JDK's `GMT` zone.
    pub fn gmt() -> JTimeZone {
        JTimeZone::known("GMT").expect("GMT is in the table")
    }

    fn known(id: &str) -> Option<JTimeZone> {
        let t = table();
        let row = &t.rows[*t.by_id.get(id)?];
        let kind = match super::zone_of(row.zone_id) {
            Ok(zone) => TzKind::Region { zone, row },
            Err(_) => TzKind::Table { row, rule: row.last_rule.map(|r| SimpleRule::from_row(&r, row.dst)) },
        };
        Some(JTimeZone { id: row.id.to_string(), raw: row.raw, kind })
    }

    /// `TimeZone.getTimeZone(String)`: a known id, a custom `GMT±hh[:mm]` id, else GMT.
    pub fn of(id: &str) -> JTimeZone {
        if let Some(z) = JTimeZone::known(id) {
            return z;
        }
        if let Some(z) = parse_custom(id) {
            return z;
        }
        JTimeZone::gmt()
    }

    /// The `TimeZone` of a `ZoneId` (`GregorianCalendar.from`, `TimeZone.getTimeZone(ZoneId)`).
    pub fn of_zone(zone: &Zone) -> JTimeZone {
        match zone {
            Zone::Offset(o) => {
                if *o == 0 {
                    return JTimeZone::of("UTC");
                }
                JTimeZone::of(&format!("GMT{}", custom_suffix(*o as i64 * 1000)))
            }
            Zone::Region(id, ..) => {
                let id = id.to_string();
                if let Some(z) = JTimeZone::known(&id) {
                    return z;
                }
                // `UTC+05:00` and friends: the JDK maps them to `GMT+05:00`
                if let Some(rest) = id.strip_prefix("UTC").or_else(|| id.strip_prefix("UT")) {
                    return JTimeZone::of(&format!("GMT{rest}"));
                }
                JTimeZone::of(&id)
            }
        }
    }

    pub fn is_zone_info(&self) -> bool {
        !matches!(self.kind, TzKind::Simple(_))
    }

    fn row(&self) -> Option<&'static ZoneRow> {
        match &self.kind {
            TzKind::Region { row, .. } | TzKind::Table { row, .. } => Some(row),
            _ => None,
        }
    }

    /// The difference `setRawOffset` introduced on a `ZoneInfo`.
    fn raw_diff(&self) -> i32 {
        match self.row() {
            Some(r) => self.raw - r.raw,
            None => 0,
        }
    }

    /// Raw (standard) and daylight offsets in milliseconds at a UTC instant.
    pub fn offsets(&self, utc_ms: i64) -> (i32, i32) {
        match &self.kind {
            TzKind::Region { zone, .. } => {
                let s = utc_ms.div_euclid(1000);
                let total = zone.offset_at(s) * 1000;
                let std = rules::standard_offset(zone, s) * 1000;
                (std + self.raw_diff(), total - std)
            }
            TzKind::Table { rule, .. } => {
                let dst = match rule {
                    Some(r) if r.in_dst(self.raw, utc_ms) => r.dst_savings,
                    _ => 0,
                };
                (self.raw, dst)
            }
            TzKind::Custom => (self.raw, 0),
            TzKind::Simple(r) => (self.raw, if r.in_dst(self.raw, utc_ms) { r.dst_savings } else { 0 }),
        }
    }

    pub fn offset(&self, utc_ms: i64) -> i32 {
        let (r, d) = self.offsets(utc_ms);
        r + d
    }

    /// Offsets for a wall-clock time (`ZoneInfo.getOffsetsByWall`): the last transition whose
    /// wall time (its instant plus the offset it introduces) is not after the given one, so a
    /// time in a gap or an overlap takes standard time.
    pub fn offsets_by_wall(&self, wall_ms: i64) -> (i32, i32) {
        match &self.kind {
            TzKind::Region { zone, .. } => {
                if rules::is_fixed(zone) {
                    return self.offsets(wall_ms - self.raw as i64);
                }
                let wall = wall_ms.div_euclid(1000);
                // no offset is beyond ±18 hours: start safely before any transition in reach
                let mut probe = wall - 18 * 3600 - 1;
                loop {
                    match rules::next_transition(zone, probe) {
                        Some((t, _, after)) if t + after as i64 <= wall => probe = t,
                        _ => break,
                    }
                }
                let at = if probe == wall - 18 * 3600 - 1 { wall - 18 * 3600 - 1 } else { probe };
                self.offsets(at * 1000)
            }
            _ => {
                let (raw, _) = self.offsets(wall_ms - self.raw as i64);
                let guess = wall_ms - raw as i64;
                // standard time unless the instant before the wall time is in daylight time
                let (r1, d1) = self.offsets(guess);
                if d1 == 0 {
                    return (r1, 0);
                }
                let (r2, d2) = self.offsets(guess - d1 as i64);
                (r2, d2)
            }
        }
    }

    pub fn dst_savings(&self) -> i32 {
        match &self.kind {
            TzKind::Simple(r) => r.dst_savings,
            _ => self.row().map(|r| r.dst).unwrap_or(0),
        }
    }

    pub fn use_daylight(&self) -> bool {
        match &self.kind {
            TzKind::Simple(r) => r.use_daylight,
            _ => self.row().is_some_and(|r| r.use_daylight),
        }
    }

    pub fn observes(&self) -> bool {
        match &self.kind {
            TzKind::Simple(r) => r.use_daylight,
            _ => self.row().is_some_and(|r| r.observes),
        }
    }

    /// The `ZoneId` of `toZoneId()`.
    pub fn zone_id(&self) -> Zone {
        self.to_zone_id().unwrap_or(Zone::Offset(self.raw / 1000))
    }

    /// `TimeZone.toZoneId()`: `ZoneId.of(id, SHORT_IDS)` for zones the table does not map.
    pub fn to_zone_id(&self) -> Result<Zone, Thrown> {
        match &self.kind {
            TzKind::Region { zone, .. } => Ok(zone.clone()),
            TzKind::Table { row, .. } => Ok(super::zone_of(row.zone_id).unwrap_or(Zone::Offset(row.raw / 1000))),
            TzKind::Custom | TzKind::Simple(_) => super::zone_of(&self.id).map_err(|mut t| {
                for f in ["time.ZoneId.of(ZoneId.java:411)", "time.ZoneId.of(ZoneId.java:359)", "time.ZoneId.of(ZoneId.java:315)", "util.TimeZone.toZoneId0(TimeZone.java:596)", "util.TimeZone.toZoneId(TimeZone.java:582)"] {
                    t.frames.push(format!("java.base/java.{f}"));
                }
                t
            }),
        }
    }

    /// `TimeZone.getDisplayName(daylight, style, locale)`; `long` is `TimeZone.LONG`.
    pub fn display_name(&self, daylight: bool, long: bool, locale: &str) -> String {
        let lookup = |id: &str| super::locale::zone_names(locale, id);
        let names = lookup(&self.id).or_else(|| self.row().and_then(|r| lookup(r.zone_id)));
        if self.is_zone_info()
            && let Some(n) = names
        {
            let i = match (long, daylight) {
                (true, false) => 0,
                (true, true) => 1,
                (false, false) => 2,
                (false, true) => 3,
            };
            return n[i].to_string();
        }
        if let Some(rest) = self.id.strip_prefix("GMT")
            && (rest.starts_with('+') || rest.starts_with('-'))
        {
            return self.id.clone();
        }
        let mut offset = self.raw as i64;
        if daylight {
            offset += self.dst_savings() as i64;
        }
        format!("GMT{}", custom_suffix(offset))
    }

    pub fn write_string(&self, out: &mut String) {
        match &self.kind {
            TzKind::Simple(r) => {
                out.push_str(&format!("java.util.SimpleTimeZone[id={},offset={},dstSavings={},useDaylight={},", self.id, self.raw, r.dst_savings, r.use_daylight));
                r.write_fields(out);
                out.push(']');
            }
            _ => {
                let (dst, use_d, tr) = match self.row() {
                    Some(r) => (r.dst, r.use_daylight, r.transitions),
                    None => (0, false, 0),
                };
                out.push_str(&format!("sun.util.calendar.ZoneInfo[id=\"{}\",offset={},dstSavings={},useDaylight={},transitions={},lastRule=", self.id, self.raw, dst, use_d, tr));
                match self.row().and_then(|r| r.last_rule.map(|l| (r, l))) {
                    Some((r, l)) => {
                        let rule = SimpleRule::from_row(&l, r.dst);
                        out.push_str(&format!("java.util.SimpleTimeZone[id={},offset={},dstSavings={},useDaylight=true,", self.id, self.raw, r.dst));
                        rule.write_fields(out);
                        out.push(']');
                    }
                    None => out.push_str("null"),
                }
                out.push(']');
            }
        }
    }

    /// `hasSameRules`: same raw offset and daylight behaviour.
    pub fn same_rules(&self, o: &JTimeZone) -> bool {
        if self.raw != o.raw || self.use_daylight() != o.use_daylight() {
            return false;
        }
        match (&self.kind, &o.kind) {
            (TzKind::Region { zone: a, .. }, TzKind::Region { zone: b, .. }) => rules::same_rules(a, b),
            (TzKind::Simple(a), TzKind::Simple(b)) => !a.use_daylight || a == b,
            _ => !self.use_daylight(),
        }
    }

    pub fn equals(&self, o: &JTimeZone) -> bool {
        match (&self.kind, &o.kind) {
            (TzKind::Simple(a), TzKind::Simple(b)) => self.id == o.id && self.raw == o.raw && a == b,
            (TzKind::Simple(_), _) | (_, TzKind::Simple(_)) => false,
            _ => self.id == o.id && self.raw == o.raw && self.same_rules(o),
        }
    }

    pub fn java_hash(&self) -> i32 {
        match &self.kind {
            TzKind::Simple(r) => {
                r.start_month ^ r.start_day ^ r.start_dow ^ r.start_time ^ r.end_month ^ r.end_day ^ r.end_dow ^ r.end_time ^ self.raw
            }
            _ => self.raw ^ crate::java::string_hash(&self.id),
        }
    }
}

/// `±hh:mm` of a custom id.
fn custom_suffix(offset_ms: i64) -> String {
    let mut m = offset_ms / 60_000;
    let sign = if m < 0 {
        m = -m;
        '-'
    } else {
        '+'
    };
    format!("{sign}{:02}:{:02}", m / 60, m % 60)
}

/// `TimeZone.parseCustomTimeZone`: `GMT` then a sign and `h`, `hh`, `hhmm` or `hh:mm`.
fn parse_custom(id: &str) -> Option<JTimeZone> {
    let rest = id.strip_prefix("GMT")?;
    if rest.len() < 2 {
        return None;
    }
    let mut chars = rest.chars();
    let negative = match chars.next()? {
        '-' => true,
        '+' => false,
        _ => return None,
    };
    let (mut hours, mut num, mut delims, mut len) = (0i64, 0i64, 0, 0);
    for c in chars {
        if c == ':' {
            if delims > 0 || len > 2 {
                return None;
            }
            hours = num;
            delims += 1;
            num = 0;
            len = 0;
            continue;
        }
        let d = c.to_digit(10)? as i64;
        num = num * 10 + d;
        len += 1;
        if num > 100_000 {
            return None;
        }
    }
    if delims == 0 {
        if len <= 2 {
            hours = num;
            num = 0;
        } else {
            hours = num / 100;
            num %= 100;
        }
    } else if len != 2 {
        return None;
    }
    if hours > 23 || num > 59 {
        return None;
    }
    let offset = (hours * 60 + num) * 60_000;
    if offset == 0 {
        let mut z = JTimeZone::gmt();
        z.id = if negative { "GMT-00:00".into() } else { "GMT+00:00".into() };
        return Some(z);
    }
    let off = if negative { -offset } else { offset };
    Some(JTimeZone { id: format!("GMT{}", custom_suffix(off)), raw: off as i32, kind: TzKind::Custom })
}

// ------------------------------------------------------------------------------ day arithmetic

/// Epoch day of a proleptic Gregorian date (month and day may be out of range by a little).
pub fn gregorian_ed(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468 + d - 1
}

/// Proleptic Gregorian (year, month 1–12, day) of an epoch day.
pub fn gregorian_from_ed(ed: i64) -> (i64, i64, i64) {
    let z = ed + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

/// Epoch day of a Julian date (normalized year: 0 is 1 BC).
pub fn julian_ed(y: i64, m: i64, d: i64) -> i64 {
    let a = (14 - m).div_euclid(12);
    let yy = y + 4800 - a;
    let mm = m + 12 * a - 3;
    d + (153 * mm + 2).div_euclid(5) + 365 * yy + yy.div_euclid(4) - 32_083 - 2_440_588
}

pub fn julian_from_ed(ed: i64) -> (i64, i64, i64) {
    let c = ed + 2_440_588 + 32_082;
    let dd = (4 * c + 3).div_euclid(1461);
    let e = c - (1461 * dd).div_euclid(4);
    let m = (5 * e + 2).div_euclid(153);
    let day = e - (153 * m + 2).div_euclid(5) + 1;
    let month = m + 3 - 12 * (m / 10);
    let year = dd - 4800 + m / 10;
    (year, month, day)
}

pub fn greg_leap(y: i64) -> bool {
    y.rem_euclid(4) == 0 && (y.rem_euclid(100) != 0 || y.rem_euclid(400) == 0)
}

fn julian_leap(y: i64) -> bool {
    y.rem_euclid(4) == 0
}

fn month_len_g(y: i64, m: i64) -> i32 {
    match m {
        2 => {
            if greg_leap(y) {
                29
            } else {
                28
            }
        }
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `Calendar.DAY_OF_WEEK` (Sunday = 1) of an epoch day.
pub fn dow(ed: i64) -> i32 {
    ((ed + 4).rem_euclid(7) + 1) as i32
}

fn on_or_before(ed: i64, d: i32) -> i64 {
    ed - (dow(ed) - d).rem_euclid(7) as i64
}

fn on_or_after(ed: i64, d: i32) -> i64 {
    on_or_before(ed + 6, d)
}

// ------------------------------------------------------------------------------ Calendar

pub const ERA: usize = 0;
pub const YEAR: usize = 1;
pub const MONTH: usize = 2;
pub const WEEK_OF_YEAR: usize = 3;
pub const WEEK_OF_MONTH: usize = 4;
pub const DAY_OF_MONTH: usize = 5;
pub const DAY_OF_YEAR: usize = 6;
pub const DAY_OF_WEEK: usize = 7;
pub const DAY_OF_WEEK_IN_MONTH: usize = 8;
pub const AM_PM: usize = 9;
pub const HOUR: usize = 10;
pub const HOUR_OF_DAY: usize = 11;
pub const MINUTE: usize = 12;
pub const SECOND: usize = 13;
pub const MILLISECOND: usize = 14;
pub const ZONE_OFFSET: usize = 15;
pub const DST_OFFSET: usize = 16;
pub const FIELD_COUNT: usize = 17;
const ALL_FIELDS: u32 = (1 << FIELD_COUNT) - 1;

pub const FIELD_NAMES: [&str; FIELD_COUNT] = [
    "ERA",
    "YEAR",
    "MONTH",
    "WEEK_OF_YEAR",
    "WEEK_OF_MONTH",
    "DAY_OF_MONTH",
    "DAY_OF_YEAR",
    "DAY_OF_WEEK",
    "DAY_OF_WEEK_IN_MONTH",
    "AM_PM",
    "HOUR",
    "HOUR_OF_DAY",
    "MINUTE",
    "SECOND",
    "MILLISECOND",
    "ZONE_OFFSET",
    "DST_OFFSET",
];

const MIN_VALUES: [i32; FIELD_COUNT] = [0, 1, 0, 1, 0, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, -13 * 3_600_000, 0];
const LEAST_MAX_VALUES: [i32; FIELD_COUNT] = [1, 292_269_054, 11, 50, 3, 28, 355, 7, 3, 1, 11, 23, 59, 59, 999, 14 * 3_600_000, 1_200_000];
const MAX_VALUES: [i32; FIELD_COUNT] = [1, 292_278_994, 11, 53, 6, 31, 366, 7, 6, 1, 11, 23, 59, 59, 999, 14 * 3_600_000, 7_200_000];

const UNSET: i32 = 0;
const COMPUTED: i32 = 1;
const MINIMUM_USER_STAMP: i32 = 2;

/// The default Gregorian cutover: October 15, 1582 (Gregorian) at midnight UTC.
pub const DEFAULT_CUTOVER: i64 = -12_219_292_800_000;

/// A `GregorianCalendar`.
#[derive(Clone, Debug)]
pub struct Cal {
    pub time: i64,
    pub is_time_set: bool,
    pub fields: [i32; FIELD_COUNT],
    pub stamp: [i32; FIELD_COUNT],
    pub are_fields_set: bool,
    pub are_all_fields_set: bool,
    pub lenient: bool,
    pub zone: JTimeZone,
    pub first_dow: i32,
    pub min_days: i32,
    next_stamp: i32,
    /// The cutover instant and the derived epoch day and years.
    pub cutover: i64,
    cut_ed: i64,
    cut_year: i64,
    cut_year_julian: i64,
    /// Whether the last date computation used the Julian calendar (`calsys == jcal`).
    julian: bool,
    pub locale: Rc<str>,
}

/// Calendar dates of an epoch day in the hybrid calendar: (normalized year, month 1–12, day,
/// julian).
struct CDate {
    y: i64,
    m: i64,
    d: i64,
    julian: bool,
}

impl Cal {
    /// A calendar with no field set and no time (the state the JDK's internal constructor
    /// leaves; public constructors then set the time or fields).
    pub fn new(zone: JTimeZone, locale: &str) -> Cal {
        let (first, min) = week_data(locale);
        let mut c = Cal {
            time: 0,
            is_time_set: false,
            fields: [0; FIELD_COUNT],
            stamp: [UNSET; FIELD_COUNT],
            are_fields_set: false,
            are_all_fields_set: false,
            lenient: true,
            zone,
            first_dow: first,
            min_days: min,
            next_stamp: MINIMUM_USER_STAMP,
            cutover: DEFAULT_CUTOVER,
            cut_ed: 0,
            cut_year: 0,
            cut_year_julian: 0,
            julian: false,
            locale: Rc::from(locale),
        };
        c.set_cutover(DEFAULT_CUTOVER);
        c
    }

    pub fn now(zone: JTimeZone, locale: &str) -> Cal {
        let mut c = Cal::new(zone, locale);
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        c.set_time_in_millis(d.as_millis() as i64);
        c
    }

    /// `setGregorianChange`.
    pub fn set_cutover(&mut self, ms: i64) {
        self.cutover = ms;
        // the day of the cutover instant; `Long.MAX_VALUE` makes a pure Julian calendar
        let ed = ms.div_euclid(MS_DAY) + if ms == i64::MAX { 1 } else { 0 };
        self.cut_ed = ed;
        self.cut_year = gregorian_from_ed(ed).0;
        self.cut_year_julian = julian_from_ed(ed - 1).0;
        self.are_fields_set = false;
        self.are_all_fields_set = false;
    }

    fn is_cutover_year(&self, y: i64) -> bool {
        y == self.cut_year || y == self.cut_year_julian
    }

    fn cdate(&self, ed: i64) -> CDate {
        if ed >= self.cut_ed {
            let (y, m, d) = gregorian_from_ed(ed);
            CDate { y, m, d, julian: false }
        } else {
            let (y, m, d) = julian_from_ed(ed);
            CDate { y, m, d, julian: true }
        }
    }

    fn fixed(&self, julian: bool, y: i64, m: i64, d: i64) -> i64 {
        if julian {
            julian_ed(y, m, d)
        } else {
            gregorian_ed(y, m, d)
        }
    }

    /// `GregorianCalendar.isLeapYear` (the hybrid rule).
    pub fn is_leap(&self, y: i64) -> bool {
        if y.rem_euclid(4) != 0 {
            return false;
        }
        if y > self.cut_year {
            return greg_leap(y);
        }
        if y < self.cut_year_julian {
            return true;
        }
        let gregorian = if self.cut_year == self.cut_year_julian { self.cdate(self.cut_ed).m < 3 } else { y == self.cut_year };
        if gregorian {
            greg_leap(y)
        } else {
            true
        }
    }

    fn month_length(&self, month0: i32, y: i64) -> i32 {
        const ML: [i32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        if month0 == 1 && self.is_leap(y) {
            29
        } else {
            ML[month0.rem_euclid(12) as usize]
        }
    }

    fn internal_era(&self) -> i32 {
        if self.stamp[ERA] != UNSET {
            self.fields[ERA]
        } else {
            1
        }
    }

    pub fn is_set(&self, f: usize) -> bool {
        self.stamp[f] != UNSET
    }

    fn is_externally_set(&self, f: usize) -> bool {
        self.stamp[f] >= MINIMUM_USER_STAMP
    }

    fn set_state_fields(&self) -> u32 {
        (0..FIELD_COUNT).filter(|&i| self.stamp[i] != UNSET).fold(0, |m, i| m | 1 << i)
    }

    fn is_partially_normalized(&self) -> bool {
        self.are_fields_set && !self.are_all_fields_set
    }

    fn set_fields_computed(&mut self, mask: u32) {
        if mask == ALL_FIELDS {
            self.stamp = [COMPUTED; FIELD_COUNT];
            self.are_fields_set = true;
            self.are_all_fields_set = true;
        } else {
            for i in 0..FIELD_COUNT {
                if mask & (1 << i) != 0 {
                    self.stamp[i] = COMPUTED;
                } else if self.are_all_fields_set && self.stamp[i] == UNSET {
                    self.are_all_fields_set = false;
                }
            }
        }
    }

    fn set_fields_normalized(&mut self, mask: u32) {
        if mask != ALL_FIELDS {
            for i in 0..FIELD_COUNT {
                if mask & (1 << i) == 0 {
                    self.stamp[i] = UNSET;
                    self.fields[i] = 0;
                }
            }
        }
        self.are_fields_set = true;
        self.are_all_fields_set = false;
    }

    /// `Calendar.set(field, value)`.
    pub fn set(&mut self, f: usize, v: i32) {
        if self.is_partially_normalized() {
            self.compute_fields_all();
        }
        self.fields[f] = v;
        self.is_time_set = false;
        self.are_fields_set = false;
        self.stamp[f] = self.next_stamp;
        self.next_stamp += 1;
        if self.next_stamp == i32::MAX {
            self.adjust_stamp();
        }
    }

    fn adjust_stamp(&mut self) {
        let mut max = MINIMUM_USER_STAMP;
        let mut new = MINIMUM_USER_STAMP;
        loop {
            let mut min = i32::MAX;
            for &s in &self.stamp {
                if s >= new && min > s {
                    min = s;
                }
                if max < s {
                    max = s;
                }
            }
            if max != min && min == i32::MAX {
                break;
            }
            for s in self.stamp.iter_mut() {
                if *s == min {
                    *s = new;
                }
            }
            new += 1;
            if min == max {
                break;
            }
        }
        self.next_stamp = new;
    }

    pub fn clear(&mut self) {
        self.fields = [0; FIELD_COUNT];
        self.stamp = [UNSET; FIELD_COUNT];
        self.are_fields_set = false;
        self.are_all_fields_set = false;
        self.is_time_set = false;
    }

    pub fn clear_field(&mut self, f: usize) {
        self.fields[f] = 0;
        self.stamp[f] = UNSET;
        self.are_fields_set = false;
        self.are_all_fields_set = false;
        self.is_time_set = false;
    }

    pub fn set_time_in_millis(&mut self, ms: i64) {
        if self.time == ms && self.is_time_set && self.are_fields_set && self.are_all_fields_set && self.zone.is_zone_info() {
            return;
        }
        self.time = ms;
        self.is_time_set = true;
        self.are_fields_set = false;
        self.compute_fields_all();
        self.are_fields_set = true;
        self.are_all_fields_set = true;
    }

    pub fn time_in_millis(&mut self) -> Result<i64, Thrown> {
        if !self.is_time_set {
            self.update_time().map_err(|e| frame(e, "Calendar.getTimeInMillis(Calendar.java:1810)"))?;
        }
        Ok(self.time)
    }

    fn update_time(&mut self) -> Result<(), Thrown> {
        self.compute_time().map_err(|e| frame(e, "Calendar.updateTime(Calendar.java:3423)"))?;
        self.is_time_set = true;
        Ok(())
    }

    /// `Calendar.complete()`.
    pub fn complete(&mut self) -> Result<(), Thrown> {
        if !self.is_time_set {
            self.update_time().map_err(|e| frame(e, "Calendar.complete(Calendar.java:2289)"))?;
        }
        if !self.are_fields_set || !self.are_all_fields_set {
            self.compute_fields_all();
            self.are_fields_set = true;
            self.are_all_fields_set = true;
        }
        Ok(())
    }

    pub fn get(&mut self, f: usize) -> Result<i32, Thrown> {
        self.complete().map_err(|e| frame(e, "Calendar.get(Calendar.java:1854)"))?;
        Ok(self.fields[f])
    }

    /// The time for comparisons (`Calendar.getMillisOf`): a calendar whose time is stale is
    /// read through a lenient copy, so invalid fields do not fail the comparison.
    pub fn millis_of(&self) -> i64 {
        if self.is_time_set {
            return self.time;
        }
        let mut c = self.clone();
        c.lenient = true;
        c.time_in_millis().unwrap_or(0)
    }

    /// `GregorianCalendar.computeFields()`.
    fn compute_fields_all(&mut self) {
        let mask = if self.is_partially_normalized() {
            let mut mask = self.set_state_fields();
            let field_mask = !mask & ALL_FIELDS;
            if field_mask != 0 {
                mask |= self.compute_fields(field_mask, mask & ((1 << ZONE_OFFSET) | (1 << DST_OFFSET)));
            }
            mask
        } else {
            self.compute_fields(ALL_FIELDS, 0);
            ALL_FIELDS
        };
        self.set_fields_computed(mask);
    }

    fn week_number(&self, fixed_day1: i64, fixed: i64) -> i32 {
        let mut first = on_or_before(fixed_day1 + 6, self.first_dow);
        let ndays = first - fixed_day1;
        if ndays >= self.min_days as i64 {
            first -= 7;
        }
        let n = fixed - first;
        (n.div_euclid(7) + 1) as i32
    }

    /// January 1 of the year of `c` (the cutover date when it does not exist).
    fn jan1(&self, c: &CDate, fixed: i64) -> i64 {
        if c.y == self.cut_year || c.y == self.cut_year_julian {
            if self.cut_year != self.cut_year_julian && fixed >= self.cut_ed {
                return self.cut_ed;
            }
            return julian_ed(c.y, 1, 1);
        }
        self.fixed(c.julian, c.y, 1, 1)
    }

    fn month1(&self, c: &CDate, fixed: i64) -> i64 {
        let cut = self.cdate(self.cut_ed);
        if cut.m == 1 && cut.d == 1 {
            return fixed - c.d + 1;
        }
        if c.m == cut.m && (c.y == self.cut_year || c.y == self.cut_year_julian) {
            let last_julian = julian_from_ed(self.cut_ed - 1);
            if self.cut_year == self.cut_year_julian && cut.m == last_julian.1 {
                return julian_ed(c.y, c.m, 1);
            }
            return self.cut_ed;
        }
        fixed - c.d + 1
    }

    /// `GregorianCalendar.computeFields(fieldMask, tzMask)`: fields from `time`.
    fn compute_fields(&mut self, field_mask: u32, tz_mask: u32) -> u32 {
        let (mut raw, mut dst) = (0, 0);
        if tz_mask != (1 << ZONE_OFFSET) | (1 << DST_OFFSET) {
            (raw, dst) = self.zone.offsets(self.time);
        }
        if tz_mask != 0 {
            if tz_mask & (1 << ZONE_OFFSET) != 0 {
                raw = self.fields[ZONE_OFFSET];
            }
            if tz_mask & (1 << DST_OFFSET) != 0 {
                dst = self.fields[DST_OFFSET];
            }
        }
        let local = self.time as i128 + raw as i128 + dst as i128;
        let fixed = local.div_euclid(MS_DAY as i128) as i64;
        let tod = local.rem_euclid(MS_DAY as i128) as i64;
        let c = self.cdate(fixed);
        self.julian = c.julian;
        let (era, year) = if c.y <= 0 { (0, 1 - c.y) } else { (1, c.y) };
        self.fields[ERA] = era;
        self.fields[YEAR] = year as i32;
        let mut mask = field_mask | (1 << ERA) | (1 << YEAR);
        let ymd_mask = (1 << MONTH) | (1 << DAY_OF_MONTH) | (1 << DAY_OF_WEEK);
        if field_mask & ymd_mask != 0 {
            self.fields[MONTH] = c.m as i32 - 1;
            self.fields[DAY_OF_MONTH] = c.d as i32;
            self.fields[DAY_OF_WEEK] = dow(fixed);
            mask |= ymd_mask;
        }
        let time_mask = (1 << HOUR_OF_DAY) | (1 << AM_PM) | (1 << HOUR) | (1 << MINUTE) | (1 << SECOND) | (1 << MILLISECOND);
        if field_mask & time_mask != 0 {
            let hours = (tod / MS_HOUR) as i32;
            self.fields[HOUR_OF_DAY] = hours;
            self.fields[AM_PM] = hours / 12;
            self.fields[HOUR] = hours % 12;
            let r = tod % MS_HOUR;
            self.fields[MINUTE] = (r / 60_000) as i32;
            self.fields[SECOND] = ((r % 60_000) / 1000) as i32;
            self.fields[MILLISECOND] = (r % 1000) as i32;
            mask |= time_mask;
        }
        if field_mask & ((1 << ZONE_OFFSET) | (1 << DST_OFFSET)) != 0 {
            self.fields[ZONE_OFFSET] = raw;
            self.fields[DST_OFFSET] = dst;
            mask |= (1 << ZONE_OFFSET) | (1 << DST_OFFSET);
        }
        let week_mask = (1 << DAY_OF_YEAR) | (1 << WEEK_OF_YEAR) | (1 << WEEK_OF_MONTH) | (1 << DAY_OF_WEEK_IN_MONTH);
        if field_mask & week_mask != 0 {
            let y = c.y;
            let mut jan1 = self.fixed(c.julian, y, 1, 1);
            let mut doy = fixed - jan1 + 1;
            let mut month1 = fixed - c.d + 1;
            let mut rel_dom = c.d - 1;
            let cut_year = if c.julian { self.cut_year_julian } else { self.cut_year };
            if y == cut_year {
                if self.cut_year_julian <= self.cut_year {
                    jan1 = self.jan1(&c, fixed);
                    if fixed >= self.cut_ed {
                        month1 = self.month1(&c, fixed);
                    }
                }
                doy = fixed - jan1 + 1;
                rel_dom = fixed - month1;
            }
            self.fields[DAY_OF_YEAR] = doy as i32;
            self.fields[DAY_OF_WEEK_IN_MONTH] = (rel_dom / 7 + 1) as i32;
            let mut woy = self.week_number(jan1, fixed);
            if woy == 0 {
                let dec31 = jan1 - 1;
                let prev = self.cdate(dec31);
                let prev_jan1 = if prev.y == self.cut_year || prev.y == self.cut_year_julian { self.jan1(&prev, dec31) } else { self.fixed(prev.julian, prev.y, 1, 1) };
                woy = self.week_number(prev_jan1, dec31);
            } else if y > self.cut_year || y < self.cut_year_julian - 1 {
                if woy >= 52 {
                    let next_jan1 = jan1 + if (c.julian && julian_leap(y)) || (!c.julian && greg_leap(y)) { 366 } else { 365 };
                    let next_first = on_or_before(next_jan1 + 6, self.first_dow);
                    let ndays = next_first - next_jan1;
                    if ndays >= self.min_days as i64 && fixed >= next_first - 7 {
                        woy = 1;
                    }
                }
            } else {
                // around the cutover: the next year's January 1st may be Julian, Gregorian or
                // missing (then the cutover date starts the year)
                let mut next_year = y + 1;
                if next_year == self.cut_year_julian + 1 && next_year < self.cut_year {
                    next_year = self.cut_year;
                }
                let next_jan1 = if next_year > self.cut_year || self.cut_year_julian == self.cut_year || next_year == self.cut_year_julian {
                    let julian = if next_year == self.cut_year { self.cut_ed > gregorian_ed(next_year, 1, 1) } else { next_year < self.cut_year_julian };
                    self.fixed(julian, next_year, 1, 1)
                } else {
                    self.cut_ed
                };
                let next_first = on_or_before(next_jan1 + 6, self.first_dow);
                let ndays = next_first - next_jan1;
                if ndays >= self.min_days as i64 && fixed >= next_first - 7 {
                    woy = 1;
                }
            }
            self.fields[WEEK_OF_YEAR] = woy;
            self.fields[WEEK_OF_MONTH] = self.week_number(month1, fixed);
            mask |= week_mask;
        }
        mask
    }

    /// `Calendar.selectFields()`: which fields decide the date and the time of day.
    fn select_fields(&self) -> u32 {
        let st = &self.stamp;
        let agg = |a: i32, b: i32| if a == UNSET || b == UNSET { UNSET } else { a.max(b) };
        let mut mask = 1 << YEAR;
        if st[ERA] != UNSET {
            mask |= 1 << ERA;
        }
        let dow_s = st[DAY_OF_WEEK];
        let month_s = st[MONTH];
        let mut dom_s = st[DAY_OF_MONTH];
        let mut wom_s = agg(st[WEEK_OF_MONTH], dow_s);
        let mut dowim_s = agg(st[DAY_OF_WEEK_IN_MONTH], dow_s);
        let doy_s = st[DAY_OF_YEAR];
        let mut woy_s = agg(st[WEEK_OF_YEAR], dow_s);
        let mut best = dom_s.max(wom_s).max(dowim_s).max(doy_s).max(woy_s);
        if best == UNSET {
            wom_s = st[WEEK_OF_MONTH];
            dowim_s = st[DAY_OF_WEEK_IN_MONTH].max(dow_s);
            woy_s = st[WEEK_OF_YEAR];
            best = wom_s.max(dowim_s).max(woy_s);
            if best == UNSET {
                best = month_s;
                dom_s = month_s;
            }
        }
        if best == dom_s || (best == wom_s && st[WEEK_OF_MONTH] >= st[WEEK_OF_YEAR]) || (best == dowim_s && st[DAY_OF_WEEK_IN_MONTH] >= st[WEEK_OF_YEAR]) {
            mask |= 1 << MONTH;
            if best == dom_s {
                mask |= 1 << DAY_OF_MONTH;
            } else {
                if dow_s != UNSET {
                    mask |= 1 << DAY_OF_WEEK;
                }
                if wom_s == dowim_s {
                    if st[WEEK_OF_MONTH] >= st[DAY_OF_WEEK_IN_MONTH] {
                        mask |= 1 << WEEK_OF_MONTH;
                    } else {
                        mask |= 1 << DAY_OF_WEEK_IN_MONTH;
                    }
                } else if best == wom_s {
                    mask |= 1 << WEEK_OF_MONTH;
                } else if st[DAY_OF_WEEK_IN_MONTH] != UNSET {
                    mask |= 1 << DAY_OF_WEEK_IN_MONTH;
                }
            }
        } else if best == doy_s {
            mask |= 1 << DAY_OF_YEAR;
        } else {
            if dow_s != UNSET {
                mask |= 1 << DAY_OF_WEEK;
            }
            mask |= 1 << WEEK_OF_YEAR;
        }
        let hod_s = st[HOUR_OF_DAY];
        let hour_s = agg(st[HOUR], st[AM_PM]);
        let mut best = hour_s.max(hod_s);
        if best == UNSET {
            best = st[HOUR].max(st[AM_PM]);
        }
        if best != UNSET {
            if best == hod_s {
                mask |= 1 << HOUR_OF_DAY;
            } else {
                mask |= 1 << HOUR;
                if st[AM_PM] != UNSET {
                    mask |= 1 << AM_PM;
                }
            }
        }
        for f in [MINUTE, SECOND, MILLISECOND] {
            if st[f] != UNSET {
                mask |= 1 << f;
            }
        }
        if st[ZONE_OFFSET] >= MINIMUM_USER_STAMP {
            mask |= 1 << ZONE_OFFSET;
        }
        if st[DST_OFFSET] >= MINIMUM_USER_STAMP {
            mask |= 1 << DST_OFFSET;
        }
        mask
    }

    /// `GregorianCalendar.getFixedDate(cal, year, fieldMask)`: the day the selected date
    /// fields designate, in the Julian or Gregorian calendar.
    fn fixed_date(&self, julian: bool, mut year: i64, mask: u32) -> i64 {
        let set = |f: usize| mask & (1 << f) != 0;
        let mut month = 0i64;
        if set(MONTH) {
            month = self.fields[MONTH] as i64;
            if month > 11 {
                year += month / 12;
                month %= 12;
            } else if month < 0 {
                year += month.div_euclid(12);
                month = month.rem_euclid(12);
            }
        }
        let mut fixed = self.fixed(julian, year, month + 1, 1);
        if set(MONTH) {
            if set(DAY_OF_MONTH) {
                if self.is_set(DAY_OF_MONTH) {
                    fixed += self.fields[DAY_OF_MONTH] as i64 - 1;
                }
            } else if set(WEEK_OF_MONTH) {
                let mut first = on_or_before(fixed + 6, self.first_dow);
                if first - fixed >= self.min_days as i64 {
                    first -= 7;
                }
                if set(DAY_OF_WEEK) {
                    first = on_or_before(first + 6, self.fields[DAY_OF_WEEK]);
                }
                fixed = first + 7 * (self.fields[WEEK_OF_MONTH] as i64 - 1);
            } else {
                let dow_v = if set(DAY_OF_WEEK) { self.fields[DAY_OF_WEEK] } else { self.first_dow };
                let dowim = if set(DAY_OF_WEEK_IN_MONTH) { self.fields[DAY_OF_WEEK_IN_MONTH] as i64 } else { 1 };
                if dowim >= 0 {
                    fixed = on_or_before(fixed + 7 * dowim - 1, dow_v);
                } else {
                    let last = self.month_length(month as i32, year) as i64 + 7 * (dowim + 1);
                    fixed = on_or_before(fixed + last - 1, dow_v);
                }
            }
        } else {
            if year == self.cut_year && !julian && fixed < self.cut_ed && self.cut_year != self.cut_year_julian {
                fixed = self.cut_ed;
            }
            if set(DAY_OF_YEAR) {
                fixed += self.fields[DAY_OF_YEAR] as i64 - 1;
            } else {
                let mut first = on_or_before(fixed + 6, self.first_dow);
                if first - fixed >= self.min_days as i64 {
                    first -= 7;
                }
                if set(DAY_OF_WEEK) {
                    let d = self.fields[DAY_OF_WEEK];
                    if d != self.first_dow {
                        first = on_or_before(first + 6, d);
                    }
                }
                fixed = first + 7 * (self.fields[WEEK_OF_YEAR] as i64 - 1);
            }
        }
        fixed
    }

    /// `GregorianCalendar.computeTime()`.
    fn compute_time(&mut self) -> Result<(), Thrown> {
        let mut original = [0; FIELD_COUNT];
        if !self.lenient {
            for f in 0..FIELD_COUNT {
                let v = self.fields[f];
                if self.is_externally_set(f) && (v < MIN_VALUES[f] || v > MAX_VALUES[f]) {
                    return Err(cal_iae(FIELD_NAMES[f].to_string(), &[COMPUTE_TIME_RANGE]));
                }
                original[f] = v;
            }
        }
        let mut mask = self.select_fields();
        let mut year = if self.is_set(YEAR) { self.fields[YEAR] as i64 } else { 1970 };
        let era = self.internal_era();
        if era == 0 {
            year = 1 - year;
        } else if era != 1 {
            return Err(cal_iae("Invalid era".to_string(), &[COMPUTE_TIME_ERA]));
        }
        if year <= 0 && !self.is_set(ERA) {
            mask |= 1 << ERA;
            self.set_fields_computed(1 << ERA);
        }
        let set = |f: usize| mask & (1 << f) != 0;
        let mut tod: i64 = if set(HOUR_OF_DAY) {
            self.fields[HOUR_OF_DAY] as i64
        } else {
            self.fields[HOUR] as i64 + if set(AM_PM) { 12 * self.fields[AM_PM] as i64 } else { 0 }
        };
        tod = tod * 60 + self.fields[MINUTE] as i64;
        tod = tod * 60 + self.fields[SECOND] as i64;
        tod = tod * 1000 + self.fields[MILLISECOND] as i64;
        let mut fixed = tod.div_euclid(MS_DAY);
        tod = tod.rem_euclid(MS_DAY);
        let (gfd, jfd);
        let chosen: i64;
        'calc: {
            if year > self.cut_year && year > self.cut_year_julian {
                gfd = fixed + self.fixed_date(false, year, mask);
                if gfd >= self.cut_ed {
                    chosen = gfd;
                    break 'calc;
                }
                jfd = fixed + self.fixed_date(true, year, mask);
            } else if year < self.cut_year && year < self.cut_year_julian {
                jfd = fixed + self.fixed_date(true, year, mask);
                if jfd < self.cut_ed {
                    chosen = jfd;
                    break 'calc;
                }
                gfd = jfd;
            } else {
                jfd = fixed + self.fixed_date(true, year, mask);
                gfd = fixed + self.fixed_date(false, year, mask);
            }
            if set(DAY_OF_YEAR) || set(WEEK_OF_YEAR) {
                if self.cut_year == self.cut_year_julian {
                    chosen = jfd;
                    break 'calc;
                } else if year == self.cut_year {
                    chosen = gfd;
                    break 'calc;
                }
            }
            if gfd >= self.cut_ed {
                if jfd >= self.cut_ed {
                    chosen = gfd;
                } else {
                    chosen = if self.julian { jfd } else { gfd };
                }
            } else if jfd < self.cut_ed {
                chosen = jfd;
            } else {
                if !self.lenient {
                    return Err(cal_iae("the specified date doesn't exist".to_string(), &[COMPUTE_TIME_GAP]));
                }
                chosen = jfd;
            }
        }
        fixed = chosen;
        let millis = fixed as i128 * MS_DAY as i128 + tod as i128;
        let tz_mask = mask & ((1 << ZONE_OFFSET) | (1 << DST_OFFSET));
        let (mut raw, mut dst) = (0, 0);
        if tz_mask != (1 << ZONE_OFFSET) | (1 << DST_OFFSET) {
            let wall = millis.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
            (raw, dst) = if self.zone.is_zone_info() {
                self.zone.offsets_by_wall(wall)
            } else {
                let gmt = if set(ZONE_OFFSET) { self.fields[ZONE_OFFSET] } else { self.zone.raw };
                self.zone.offsets(wall - gmt as i64)
            };
        }
        if tz_mask & (1 << ZONE_OFFSET) != 0 {
            raw = self.fields[ZONE_OFFSET];
        }
        if tz_mask & (1 << DST_OFFSET) != 0 {
            dst = self.fields[DST_OFFSET];
        }
        let utc = millis - raw as i128 - dst as i128;
        self.time = utc.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
        let state = self.set_state_fields();
        let computed = self.compute_fields(mask | state, tz_mask);
        if !self.lenient {
            for f in 0..FIELD_COUNT {
                if !self.is_externally_set(f) {
                    continue;
                }
                if original[f] != self.fields[f] {
                    let s = format!("{} -> {}", original[f], self.fields[f]);
                    self.fields = original;
                    return Err(cal_iae(format!("{}: {s}", FIELD_NAMES[f]), &[COMPUTE_TIME_CHECK]));
                }
            }
        }
        self.set_fields_normalized(computed);
        Ok(())
    }

    /// The epoch day of the current fields (`getCurrentFixedDate`).
    fn current_fixed(&self) -> i64 {
        let local = self.time as i128 + self.fields[ZONE_OFFSET] as i128 + self.fields[DST_OFFSET] as i128;
        local.div_euclid(MS_DAY as i128) as i64
    }

    fn normalized_year(&self) -> i64 {
        let y = self.fields[YEAR] as i64;
        if self.internal_era() == 0 {
            1 - y
        } else {
            y
        }
    }

    fn pin_day_of_month(&mut self) -> Result<(), Thrown> {
        let y = self.normalized_year();
        let len = if !self.is_cutover_year(y) {
            self.month_length(self.fields[MONTH], y)
        } else {
            let mut gc = self.clone();
            gc.lenient = true;
            gc.complete()?;
            gc.actual_maximum(DAY_OF_MONTH)?
        };
        if self.fields[DAY_OF_MONTH] > len {
            self.set(DAY_OF_MONTH, len);
        }
        Ok(())
    }

    fn set_year_with_era(&mut self, y: i64, ce: bool) {
        if y > 0 {
            self.set(YEAR, y as i32);
        } else {
            self.set(YEAR, (1 - y) as i32);
            self.set(ERA, if ce { 0 } else { 1 });
        }
    }

    /// `GregorianCalendar.add(field, amount)`.
    pub fn add(&mut self, f: usize, amount: i32) -> Result<(), Thrown> {
        if amount == 0 {
            return Ok(());
        }
        if f >= ZONE_OFFSET {
            return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, None).with_frames(&["java.base/java.util.GregorianCalendar.add(GregorianCalendar.java:932)"]));
        }
        self.complete().map_err(|e| frame(e, "GregorianCalendar.add(GregorianCalendar.java:936)"))?;
        match f {
            YEAR => {
                let y = self.fields[YEAR] as i64;
                if self.internal_era() == 1 {
                    self.set_year_with_era(y + amount as i64, true);
                } else {
                    self.set_year_with_era(y - amount as i64, false);
                }
                self.pin_day_of_month()?;
            }
            MONTH => {
                let month = self.fields[MONTH] as i64 + amount as i64;
                let y = self.fields[YEAR] as i64;
                let ya = if month >= 0 { month / 12 } else { (month + 1) / 12 - 1 };
                if ya != 0 {
                    if self.internal_era() == 1 {
                        self.set_year_with_era(y + ya, true);
                    } else {
                        self.set_year_with_era(y - ya, false);
                    }
                }
                self.set(MONTH, month.rem_euclid(12) as i32);
                self.pin_day_of_month()?;
            }
            ERA => {
                let era = (self.fields[ERA] + amount).clamp(0, 1);
                self.set(ERA, era);
            }
            _ => {
                let mut delta = amount as i64;
                let mut tod: i64 = 0;
                match f {
                    HOUR | HOUR_OF_DAY => delta *= MS_HOUR,
                    MINUTE => delta *= 60_000,
                    SECOND => delta *= 1000,
                    WEEK_OF_YEAR | WEEK_OF_MONTH | DAY_OF_WEEK_IN_MONTH => delta *= 7,
                    AM_PM => {
                        delta = amount as i64 / 2;
                        tod = 12 * (amount as i64 % 2);
                    }
                    _ => {}
                }
                if f >= HOUR {
                    self.set_time_in_millis(self.time.wrapping_add(delta));
                    return Ok(());
                }
                let mut fd = self.current_fixed();
                tod += self.fields[HOUR_OF_DAY] as i64;
                tod = tod * 60 + self.fields[MINUTE] as i64;
                tod = tod * 60 + self.fields[SECOND] as i64;
                tod = tod * 1000 + self.fields[MILLISECOND] as i64;
                if tod >= MS_DAY {
                    fd += 1;
                    tod -= MS_DAY;
                } else if tod < 0 {
                    fd -= 1;
                    tod += MS_DAY;
                }
                fd += delta;
                let mut zo = self.fields[ZONE_OFFSET] as i64 + self.fields[DST_OFFSET] as i64;
                self.set_time_in_millis(fd * MS_DAY + tod - zo);
                zo -= self.fields[ZONE_OFFSET] as i64 + self.fields[DST_OFFSET] as i64;
                if zo != 0 {
                    self.set_time_in_millis(self.time + zo);
                    if self.current_fixed() != fd {
                        self.set_time_in_millis(self.time - zo);
                    }
                }
            }
        }
        Ok(())
    }

    /// `GregorianCalendar.roll(field, amount)` (outside the cutover year).
    pub fn roll(&mut self, f: usize, mut amount: i32) -> Result<(), Thrown> {
        if amount == 0 {
            return Ok(());
        }
        if f >= ZONE_OFFSET {
            return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, None).with_frames(&["java.base/java.util.GregorianCalendar.roll(GregorianCalendar.java:1180)"]));
        }
        self.complete().map_err(|e| frame(e, "GregorianCalendar.roll(GregorianCalendar.java:1184)"))?;
        let mut min = MIN_VALUES[f];
        let mut max = MAX_VALUES[f];
        let y = self.normalized_year();
        match f {
            HOUR | HOUR_OF_DAY => {
                let unit = max + 1;
                let h = self.fields[f];
                let nh = (h + amount).rem_euclid(unit);
                let dom = self.fields[DAY_OF_MONTH];
                self.time += MS_HOUR * (nh - h) as i64;
                let mut probe = self.clone();
                probe.compute_fields(ALL_FIELDS, 0);
                if probe.fields[DAY_OF_MONTH] != dom {
                    // keep the date: the same wall hour on the original day
                    let mut c = self.clone();
                    c.lenient = true;
                    c.clear();
                    c.set(ERA, self.fields[ERA]);
                    c.set(YEAR, self.fields[YEAR]);
                    c.set(MONTH, self.fields[MONTH]);
                    c.set(DAY_OF_MONTH, dom);
                    c.set(HOUR_OF_DAY, probe.fields[HOUR_OF_DAY] + if f == HOUR { 12 } else { 0 });
                    c.set(MINUTE, self.fields[MINUTE]);
                    c.set(SECOND, self.fields[SECOND]);
                    c.set(MILLISECOND, self.fields[MILLISECOND]);
                    self.time = c.time_in_millis()?;
                    probe = self.clone();
                    probe.compute_fields(ALL_FIELDS, 0);
                }
                let hod = probe.fields[HOUR_OF_DAY];
                self.fields[f] = hod % unit;
                if f == HOUR {
                    self.fields[HOUR_OF_DAY] = hod;
                } else {
                    self.fields[AM_PM] = hod / 12;
                    self.fields[HOUR] = hod % 12;
                }
                self.fields[ZONE_OFFSET] = probe.fields[ZONE_OFFSET];
                self.fields[DST_OFFSET] = probe.fields[DST_OFFSET];
                return Ok(());
            }
            MONTH => {
                let mon = (self.fields[MONTH] + amount).rem_euclid(12);
                self.set(MONTH, mon);
                let len = self.month_length(mon, y);
                if self.fields[DAY_OF_MONTH] > len {
                    self.set(DAY_OF_MONTH, len);
                }
                return Ok(());
            }
            WEEK_OF_YEAR => {
                max = self.actual_maximum(WEEK_OF_YEAR)?;
                let d = self.fields[DAY_OF_WEEK];
                self.set(DAY_OF_WEEK, d);
                let mut woy = self.fields[WEEK_OF_YEAR];
                let value = woy + amount;
                let week_year = self.week_year()?;
                if week_year == y {
                    if value > min && value < max {
                        self.set(WEEK_OF_YEAR, value);
                        return Ok(());
                    }
                    let fd = self.current_fixed();
                    let day1 = fd - 7 * (woy - min) as i64;
                    if self.cdate(day1).y != y {
                        min += 1;
                    }
                    let fd2 = fd + 7 * (max - self.fields[WEEK_OF_YEAR]) as i64;
                    if self.cdate(fd2).y != y {
                        max -= 1;
                    }
                } else if week_year > y {
                    if amount < 0 {
                        amount += 1;
                    }
                    woy = max;
                } else {
                    if amount > 0 {
                        amount -= woy - max;
                    }
                    woy = min;
                }
                let v = rolled(woy, amount, min, max);
                self.set(WEEK_OF_YEAR, v);
                return Ok(());
            }
            WEEK_OF_MONTH => {
                let mut d = self.fields[DAY_OF_WEEK] - self.first_dow;
                if d < 0 {
                    d += 7;
                }
                let fd = self.current_fixed();
                let month1 = fd - self.fields[DAY_OF_MONTH] as i64 + 1;
                let mlen = self.month_length(self.fields[MONTH], y) as i64;
                let mut first = on_or_before(month1 + 6, self.first_dow);
                if first - month1 >= self.min_days as i64 {
                    first -= 7;
                }
                let max = self.actual_maximum(WEEK_OF_MONTH)?;
                let value = rolled(self.fields[WEEK_OF_MONTH], amount, 1, max) - 1;
                let mut nfd = first + value as i64 * 7 + d as i64;
                if nfd < month1 {
                    nfd = month1;
                } else if nfd >= month1 + mlen {
                    nfd = month1 + mlen - 1;
                }
                self.set(DAY_OF_MONTH, (nfd - month1 + 1) as i32);
                return Ok(());
            }
            DAY_OF_MONTH => max = self.month_length(self.fields[MONTH], y),
            DAY_OF_YEAR => max = self.actual_maximum(DAY_OF_YEAR)?,
            DAY_OF_WEEK => {
                let woy = self.fields[WEEK_OF_YEAR];
                if !self.is_cutover_year(y) && woy > 1 && woy < 52 {
                    self.set(WEEK_OF_YEAR, woy);
                    max = 7;
                } else {
                    amount %= 7;
                    if amount == 0 {
                        return Ok(());
                    }
                    let mut fd = self.current_fixed();
                    let first = on_or_before(fd, self.first_dow);
                    fd += amount as i64;
                    if fd < first {
                        fd += 7;
                    } else if fd >= first + 7 {
                        fd -= 7;
                    }
                    let c = self.cdate(fd);
                    self.set(ERA, if c.y <= 0 { 0 } else { 1 });
                    self.set(YEAR, if c.y <= 0 { (1 - c.y) as i32 } else { c.y as i32 });
                    self.set(MONTH, c.m as i32 - 1);
                    self.set(DAY_OF_MONTH, c.d as i32);
                    return Ok(());
                }
            }
            DAY_OF_WEEK_IN_MONTH => {
                let dom = self.fields[DAY_OF_MONTH];
                let mlen = self.month_length(self.fields[MONTH], y);
                let last = mlen % 7;
                max = mlen / 7;
                if (dom - 1) % 7 < last {
                    max += 1;
                }
                let d = self.fields[DAY_OF_WEEK];
                self.set(DAY_OF_WEEK, d);
            }
            _ => {}
        }
        let v = rolled(self.fields[f], amount, min, max);
        self.set(f, v);
        Ok(())
    }

    /// `Calendar.getWeekYear()`.
    pub fn week_year(&mut self) -> Result<i64, Thrown> {
        let mut y = self.get(YEAR).map_err(|e| frame(e, "GregorianCalendar.getWeekYear(GregorianCalendar.java:2061)"))? as i64;
        if self.internal_era() == 0 {
            y = 1 - y;
        }
        let woy = self.fields[WEEK_OF_YEAR];
        if self.fields[MONTH] == 0 {
            if woy >= 52 {
                y -= 1;
            }
        } else if woy == 1 {
            y += 1;
        }
        Ok(y)
    }

    /// `GregorianCalendar.getActualMaximum(field)` (cutover years approximated by their
    /// calendar system).
    pub fn actual_maximum(&mut self, f: usize) -> Result<i32, Thrown> {
        if matches!(f, ERA | DAY_OF_WEEK | HOUR | AM_PM | HOUR_OF_DAY | MINUTE | SECOND | MILLISECOND | ZONE_OFFSET | DST_OFFSET) {
            return Ok(MAX_VALUES[f]);
        }
        let mut gc = self.clone();
        gc.lenient = true;
        gc.complete()?;
        let y = gc.normalized_year();
        let fd = gc.current_fixed();
        let c = gc.cdate(fd);
        Ok(match f {
            MONTH => 11,
            DAY_OF_MONTH => gc.month_length(c.m as i32 - 1, c.y),
            DAY_OF_YEAR => {
                if gc.is_cutover_year(y) {
                    let jan1 = gc.jan1(&c, fd);
                    let next = gc.fixed(false, y + 1, 1, 1);
                    (next - jan1) as i32
                } else if c.julian {
                    if julian_leap(y) {
                        366
                    } else {
                        365
                    }
                } else if greg_leap(y) {
                    366
                } else {
                    365
                }
            }
            WEEK_OF_YEAR if gc.is_cutover_year(y) => {
                let max_doy = gc.actual_maximum(DAY_OF_YEAR)?;
                let mut g = gc.clone();
                g.set(DAY_OF_YEAR, max_doy);
                let mut v = g.get(WEEK_OF_YEAR)?;
                if g.fields[YEAR] as i64 != g.week_year()? {
                    g.set(DAY_OF_YEAR, max_doy - 7);
                    v = g.get(WEEK_OF_YEAR)?;
                }
                v
            }
            WEEK_OF_MONTH if gc.is_cutover_year(y) => {
                let mut g = gc.clone();
                let (yy, mm) = (g.fields[YEAR], g.fields[MONTH]);
                let mut v;
                loop {
                    v = g.get(WEEK_OF_MONTH)?;
                    g.add(WEEK_OF_MONTH, 1)?;
                    if g.get(YEAR)? != yy || g.get(MONTH)? != mm {
                        break;
                    }
                }
                v
            }
            DAY_OF_WEEK_IN_MONTH if gc.is_cutover_year(y) => {
                let mut g = gc.clone();
                let mut ndays = g.actual_month_length();
                let min = g.actual_minimum(DAY_OF_MONTH)?;
                g.set(DAY_OF_MONTH, min);
                let dow1 = g.get(DAY_OF_WEEK)?;
                let mut x = dow(fd) - dow1;
                if x < 0 {
                    x += 7;
                }
                ndays -= x;
                (ndays + 6) / 7
            }
            WEEK_OF_YEAR => {
                let jan1 = gc.fixed(c.julian, c.y, 1, 1);
                let mut d = dow(jan1) - gc.first_dow;
                if d < 0 {
                    d += 7;
                }
                let magic = d + gc.min_days - 1;
                let leap = if c.julian { julian_leap(c.y) } else { greg_leap(c.y) };
                if magic == 6 || (leap && (magic == 5 || magic == 12)) {
                    53
                } else {
                    52
                }
            }
            WEEK_OF_MONTH => {
                let first = gc.fixed(c.julian, c.y, c.m, 1);
                let mut d = dow(first) - gc.first_dow;
                if d < 0 {
                    d += 7;
                }
                let mut mlen = gc.month_length(c.m as i32 - 1, c.y);
                let ndays = 7 - d;
                let mut v = 3;
                if ndays >= gc.min_days {
                    v += 1;
                }
                mlen -= ndays + 21;
                if mlen > 0 {
                    v += 1;
                    if mlen > 7 {
                        v += 1;
                    }
                }
                v
            }
            DAY_OF_WEEK_IN_MONTH => {
                let first = gc.fixed(c.julian, c.y, c.m, 1);
                let mut ndays = gc.month_length(c.m as i32 - 1, c.y);
                let mut x = dow(fd) - dow(first);
                if x < 0 {
                    x += 7;
                }
                ndays -= x;
                (ndays + 6) / 7
            }
            YEAR => {
                // the year of the largest representable instant, one less when this date's
                // position in its year lies beyond it
                let offset_in_year = |cal: &mut Cal| -> Result<i64, Thrown> {
                    let doy = cal.get(DAY_OF_YEAR)? as i64 - 1;
                    Ok((((doy * 24 + cal.fields[HOUR_OF_DAY] as i64) * 60 + cal.fields[MINUTE] as i64) * 60 + cal.fields[SECOND] as i64) * 1000 + cal.fields[MILLISECOND] as i64
                        - cal.fields[ZONE_OFFSET] as i64)
                };
                let current = offset_in_year(&mut gc)?;
                if gc.internal_era() == 1 {
                    let mut m = gc.clone();
                    m.set_time_in_millis(i64::MAX);
                    let mut v = m.fields[YEAR];
                    if current > offset_in_year(&mut m)? {
                        v -= 1;
                    }
                    v
                } else {
                    let mut m = gc.clone();
                    m.set_time_in_millis(i64::MIN);
                    let mut v = m.fields[YEAR];
                    if current < offset_in_year(&mut m)? {
                        v -= 1;
                    }
                    v
                }
            }
            _ => MAX_VALUES[f],
        })
    }

    /// `actualMonthLength()`: the days the current month really has (fewer in the cutover
    /// month).
    fn actual_month_length(&self) -> i32 {
        let fd = self.current_fixed();
        let c = self.cdate(fd);
        if !self.is_cutover_year(c.y) {
            return self.month_length(c.m as i32 - 1, c.y);
        }
        let month1 = self.month1(&c, fd);
        let len = if c.julian {
            if c.m == 2 && julian_leap(c.y) {
                29
            } else {
                self.month_length(c.m as i32 - 1, 2001)
            }
        } else {
            month_len_g(c.y, c.m)
        } as i64;
        let next1 = month1 + len;
        if next1 < self.cut_ed {
            return len as i32;
        }
        let nc = self.cdate(next1);
        (self.month1(&nc, next1) - month1) as i32
    }

    pub fn actual_minimum(&mut self, f: usize) -> Result<i32, Thrown> {
        if f == DAY_OF_MONTH {
            let mut gc = self.clone();
            gc.lenient = true;
            gc.complete()?;
            let fd = gc.current_fixed();
            let c = gc.cdate(fd);
            if gc.is_cutover_year(c.y) {
                let m1 = gc.month1(&c, fd);
                return Ok(gc.cdate(m1).d as i32);
            }
        }
        Ok(MIN_VALUES[f])
    }

    /// `Calendar.toString()`.
    pub fn write_string(&self, out: &mut String) {
        out.push_str("java.util.GregorianCalendar[time=");
        if self.is_time_set {
            out.push_str(&self.time.to_string());
        } else {
            out.push('?');
        }
        out.push_str(&format!(",areFieldsSet={},areAllFieldsSet={},lenient={},zone=", self.are_fields_set, self.are_all_fields_set, self.lenient));
        self.zone.write_string(out);
        out.push_str(&format!(",firstDayOfWeek={},minimalDaysInFirstWeek={}", self.first_dow, self.min_days));
        for (i, name) in FIELD_NAMES.iter().enumerate() {
            out.push(',');
            out.push_str(name);
            out.push('=');
            if self.is_set(i) {
                out.push_str(&self.fields[i].to_string());
            } else {
                out.push('?');
            }
        }
        out.push(']');
    }

    /// `GregorianCalendar.setWeekDate`.
    pub fn set_week_date(&mut self, week_year: i32, woy: i32, d: i32) -> Result<(), Thrown> {
        if !(1..=7).contains(&d) {
            return Err(iae(format!("invalid dayOfWeek: {d}")).with_frames(&["java.base/java.util.GregorianCalendar.setWeekDate(GregorianCalendar.java:2183)"]));
        }
        let mut gc = self.clone();
        gc.lenient = true;
        let era = gc.get(ERA)?;
        gc.clear();
        gc.zone = JTimeZone::gmt();
        gc.set(ERA, era);
        gc.set(YEAR, week_year);
        gc.set(WEEK_OF_YEAR, 1);
        gc.set(DAY_OF_WEEK, self.first_dow);
        let mut days = d - self.first_dow;
        if days < 0 {
            days += 7;
        }
        days += 7 * (woy - 1);
        if days != 0 {
            gc.add(DAY_OF_YEAR, days)?;
        } else {
            gc.complete()?;
        }
        if !self.lenient && (gc.week_year()? != week_year as i64 || gc.fields[WEEK_OF_YEAR] != woy || gc.fields[DAY_OF_WEEK] != d) {
            return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, None));
        }
        self.set(ERA, gc.fields[ERA]);
        self.set(YEAR, gc.fields[YEAR]);
        self.set(MONTH, gc.fields[MONTH]);
        self.set(DAY_OF_MONTH, gc.fields[DAY_OF_MONTH]);
        self.fields[WEEK_OF_YEAR] = woy;
        self.complete()
    }

    /// `setFirstDayOfWeek` / `setMinimalDaysInFirstWeek`: computed week fields follow.
    pub fn invalidate_week_fields(&mut self) {
        if self.stamp[WEEK_OF_MONTH] != COMPUTED && self.stamp[WEEK_OF_YEAR] != COMPUTED {
            return;
        }
        let mut c = self.clone();
        c.lenient = true;
        c.clear_field(WEEK_OF_MONTH);
        c.clear_field(WEEK_OF_YEAR);
        if self.stamp[WEEK_OF_MONTH] == COMPUTED
            && let Ok(v) = c.get(WEEK_OF_MONTH)
        {
            self.fields[WEEK_OF_MONTH] = v;
        }
        if self.stamp[WEEK_OF_YEAR] == COMPUTED
            && let Ok(v) = c.get(WEEK_OF_YEAR)
        {
            self.fields[WEEK_OF_YEAR] = v;
        }
    }
}

fn rolled(value: i32, amount: i32, min: i32, max: i32) -> i32 {
    let range = max - min + 1;
    let amount = amount % range;
    let n = value + amount;
    if n > max {
        n - range
    } else if n < min {
        n + range
    } else {
        n
    }
}

const COMPUTE_TIME_RANGE: &str = "java.base/java.util.GregorianCalendar.computeTime(GregorianCalendar.java:2628)";
const COMPUTE_TIME_ERA: &str = "java.base/java.util.GregorianCalendar.computeTime(GregorianCalendar.java:2652)";
const COMPUTE_TIME_GAP: &str = "java.base/java.util.GregorianCalendar.computeTime(GregorianCalendar.java:2747)";
const COMPUTE_TIME_CHECK: &str = "java.base/java.util.GregorianCalendar.computeTime(GregorianCalendar.java:2809)";

fn cal_iae(msg: String, frames: &[&str]) -> Thrown {
    iae(msg).with_frames(frames)
}

/// Appends the frame of a `java.base/java.util` caller to an exception raised below it.
fn frame(mut t: Thrown, f: &str) -> Thrown {
    t.frames.push(format!("java.base/java.util.{f}"));
    t
}

/// The week definition `Calendar.getInstance(locale)` uses: Sunday-based (1) with
/// `java.util.Calendar`'s numbering, from the locale's region.
pub fn week_data(locale: &str) -> (i32, i32) {
    let (first, min) = super::locale::week_def(locale);
    // `WeekFields` numbers Monday 1 … Sunday 7; `Calendar` Sunday 1 … Saturday 7
    ((first as i32 % 7) + 1, min as i32)
}

// ------------------------------------------------------------------------------ script objects

/// `java.util` legacy date objects as script values.
pub enum LegacyObj {
    TimeZone(RefCell<JTimeZone>),
    Calendar(RefCell<Cal>),
    CalendarBuilder(RefCell<CalBuilder>),
    DateFormat(RefCell<super::legacy_fmt::DateFmt>),
    Symbols(RefCell<super::legacy_fmt::Dfs>),
    /// `DateFormat.Field` by index of [`super::legacy_fmt::DATE_FIELDS`].
    DateField(u8),
}

impl LegacyObj {
    pub fn class(&self) -> crate::types::ClassId {
        match self {
            LegacyObj::TimeZone(z) => {
                if z.borrow().is_zone_info() {
                    cls::ZONE_INFO
                } else {
                    cls::SIMPLE_TIME_ZONE
                }
            }
            LegacyObj::Calendar(_) => cls::GREGORIAN_CALENDAR,
            LegacyObj::CalendarBuilder(_) => cls::CALENDAR_BUILDER,
            LegacyObj::DateFormat(_) => cls::SIMPLE_DATE_FORMAT,
            LegacyObj::Symbols(_) => cls::DATE_FORMAT_SYMBOLS,
            LegacyObj::DateField(_) => cls::DATE_FORMAT_FIELD,
        }
    }

    pub fn write_string(&self, out: &mut String) -> bool {
        match self {
            LegacyObj::TimeZone(z) => z.borrow().write_string(out),
            LegacyObj::Calendar(c) => c.borrow().write_string(out),
            LegacyObj::DateField(i) => out.push_str(&format!("java.text.DateFormat$Field({})", super::legacy_fmt::DATE_FIELDS[*i as usize].1)),
            LegacyObj::DateFormat(f) => {
                let f = f.borrow();
                out.push_str(&format!("SimpleDateFormat [locale: \"{}\", pattern: \"{}\"]\n", crate::lib_util::locale_display_name(&f.locale), f.pattern));
            }
            LegacyObj::CalendarBuilder(_) | LegacyObj::Symbols(_) => return false,
        }
        true
    }

    pub fn equals(&self, o: &LegacyObj) -> Option<bool> {
        Some(match (self, o) {
            (LegacyObj::TimeZone(a), LegacyObj::TimeZone(b)) => a.borrow().equals(&b.borrow()),
            (LegacyObj::DateFormat(a), LegacyObj::DateFormat(b)) => super::legacy_fmt::equals(&a.borrow(), &b.borrow()),
            (LegacyObj::Symbols(a), LegacyObj::Symbols(b)) => *a.borrow() == *b.borrow(),
            (LegacyObj::DateField(a), LegacyObj::DateField(b)) => a == b,
            (LegacyObj::Calendar(a), LegacyObj::Calendar(b)) => {
                if std::ptr::eq(a, b) {
                    return Some(true);
                }
                let (x, y) = (a.borrow(), b.borrow());
                let (tx, ty) = (x.millis_of(), y.millis_of());
                tx == ty && x.lenient == y.lenient && x.first_dow == y.first_dow && x.min_days == y.min_days && x.zone.equals(&y.zone) && x.cutover == y.cutover
            }
            _ => return None,
        })
    }

    pub fn java_hash(&self) -> Option<i32> {
        match self {
            LegacyObj::TimeZone(z) => Some(z.borrow().java_hash()),
            LegacyObj::DateFormat(f) => Some(crate::java::string_hash(&f.borrow().pattern)),
            LegacyObj::Calendar(c) => {
                let c = c.borrow();
                let t = c.millis_of();
                let other = (c.lenient as i32) | (c.first_dow << 1) | (c.min_days << 4) | (c.zone.java_hash() << 7);
                let cut_fixed = c.cut_ed + 719_163;
                Some((t as i32) ^ ((t >> 32) as i32) ^ other ^ cut_fixed as i32)
            }
            _ => None,
        }
    }

    pub fn compare(&self, o: &LegacyObj) -> Option<std::cmp::Ordering> {
        match (self, o) {
            (LegacyObj::Calendar(a), LegacyObj::Calendar(b)) => Some(a.borrow().millis_of().cmp(&b.borrow().millis_of())),
            _ => None,
        }
    }
}

pub fn legacy(v: &Value) -> Option<&LegacyObj> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Legacy(l) => Some(l),
            _ => None,
        },
        _ => None,
    }
}

pub fn tz_value(z: JTimeZone) -> Value {
    Value::obj(Obj::Legacy(Box::new(LegacyObj::TimeZone(RefCell::new(z)))))
}

pub fn cal_value(c: Cal) -> Value {
    Value::obj(Obj::Legacy(Box::new(LegacyObj::Calendar(RefCell::new(c)))))
}

pub fn tz_of(v: &Value) -> Result<&RefCell<JTimeZone>, Thrown> {
    match legacy(v) {
        Some(LegacyObj::TimeZone(z)) => Ok(z),
        _ if v.is_null() => Err(crate::vm::npe("Cannot invoke \"java.util.TimeZone.getID()\" because \"zone\" is null")),
        _ => Err(crate::vm::cce("not a TimeZone")),
    }
}

pub fn cal_of(v: &Value) -> Result<&RefCell<Cal>, Thrown> {
    match legacy(v) {
        Some(LegacyObj::Calendar(c)) => Ok(c),
        _ => Err(crate::vm::cce("not a Calendar")),
    }
}

// ------------------------------------------------------------------------------ Calendar.Builder

/// `Calendar.Builder`: fields in stamp order, or an instant.
#[derive(Clone, Default)]
pub struct CalBuilder {
    /// (field, value) in the order they were set; field 17 is the week year.
    fields: Vec<(usize, i32)>,
    instant: Option<i64>,
    lenient: Option<bool>,
    calendar_type: Option<String>,
    locale: Option<Rc<str>>,
    zone: Option<JTimeZone>,
    week: Option<(i32, i32)>,
}

const WEEK_YEAR: usize = FIELD_COUNT;

impl CalBuilder {
    fn field_set(&mut self, f: usize, v: i32) -> Result<(), Thrown> {
        if self.instant.is_some() {
            return Err(Throw::new(cls::ILLEGAL_STATE_EXCEPTION, Some("instant has been set".into())));
        }
        self.fields.retain(|(x, _)| *x != f);
        self.fields.push((f, v));
        Ok(())
    }

    fn build(&self) -> Result<Cal, Thrown> {
        let locale: Rc<str> = self.locale.clone().unwrap_or_else(|| Rc::from(crate::text::DEFAULT_LOCALE));
        let zone = self.zone.clone().unwrap_or_else(JTimeZone::gmt);
        let ty = self.calendar_type.clone().unwrap_or_else(|| "gregory".into());
        let mut cal = Cal::new(zone, &locale);
        match ty.as_str() {
            "gregory" | "gregorian" => {}
            "iso8601" => {
                cal.set_cutover(i64::MIN);
                cal.first_dow = 2;
                cal.min_days = 4;
            }
            other => return Err(iae(format!("unknown calendar type: {other}"))),
        }
        cal.lenient = self.lenient.unwrap_or(true);
        if let Some((first, min)) = self.week {
            cal.first_dow = first;
            cal.min_days = min;
        } else if ty == "iso8601" {
            cal.first_dow = 2;
            cal.min_days = 4;
        }
        if let Some(i) = self.instant {
            cal.set_time_in_millis(i);
            cal.complete()?;
            return Ok(cal);
        }
        if !self.fields.is_empty() {
            let year = self.fields.iter().find(|(f, _)| *f == YEAR).map(|x| x.1);
            let week_year = self.fields.iter().find(|(f, _)| *f == WEEK_YEAR).map(|x| x.1);
            let week_date = matches!((week_year, year), (Some(w), y) if y.is_none_or(|y| w > y));
            for &(f, v) in &self.fields {
                if f < FIELD_COUNT {
                    cal.set(f, v);
                }
            }
            if week_date {
                let woy = self.fields.iter().find(|(f, _)| *f == WEEK_OF_YEAR).map(|x| x.1).unwrap_or(1);
                let d = self.fields.iter().find(|(f, _)| *f == DAY_OF_WEEK).map(|x| x.1).unwrap_or(cal.first_dow);
                cal.set_week_date(week_year.unwrap_or(1970), woy, d)?;
            }
            cal.complete()?;
        }
        Ok(cal)
    }
}

// ------------------------------------------------------------------------------ natives

fn int(v: &Value) -> i32 {
    crate::lib_lang::int(v)
}

fn locale_str(v: &Value) -> Result<Rc<str>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Locale(l) => Ok(l.clone()),
            _ => Err(crate::vm::cce("not a Locale")),
        },
        _ => Err(crate::vm::npe("Cannot invoke \"java.util.Locale.getUnicodeLocaleType(String)\" because \"aLocale\" is null")),
    }
}

fn date_ms(v: &Value) -> Result<i64, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Time(TimeVal::UtilDate(ms)) => Ok(ms.get()),
            _ => Err(crate::vm::cce("not a Date")),
        },
        _ => Err(crate::vm::npe("Cannot invoke \"java.util.Date.getTime()\" because \"date\" is null")),
    }
}

pub fn date_value(ms: i64) -> Value {
    Value::obj(Obj::Time(TimeVal::UtilDate(std::cell::Cell::new(ms))))
}

/// Field index argument: out of range reads fail as the JDK's array access does.
fn field_arg(v: &Value, frames: &[&str]) -> Result<usize, Thrown> {
    let f = int(v);
    if !(0..FIELD_COUNT as i32).contains(&f) {
        let mut t = Throw::new(cls::ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index {f} out of bounds for length {FIELD_COUNT}")));
        t.frames = frames.iter().map(|s| s.to_string()).collect();
        return Err(t);
    }
    Ok(f as usize)
}

fn tz_get_time_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    let id = crate::lib_lang::string(&a[0], "ID")?;
    Ok(tz_value(JTimeZone::of(id.as_str())))
}
fn tz_default(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(tz_value(JTimeZone::gmt()))
}
fn tz_available(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(str_array(available_ids().map(|r| r.id.to_string())))
}
fn tz_available_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let raw = int(&a[0]);
    let mut ids: Vec<String> = available_ids().filter(|r| r.raw == raw).map(|r| r.id.to_string()).collect();
    ids.sort();
    Ok(str_array(ids.into_iter()))
}
fn str_array(items: impl Iterator<Item = String>) -> Value {
    let items: Vec<Value> = items.map(Value::string).collect();
    Value::obj(Obj::Array(crate::value::JArray::from_data(crate::types::Ty::of(cls::STRING), crate::value::ArrayData::Ref(items))))
}
fn tz_id(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(tz_of(&a[0])?.borrow().id.clone()))
}
fn tz_raw(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(tz_of(&a[0])?.borrow().raw))
}
fn tz_set_raw(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = int(&a[1]);
    tz_of(&a[0])?.borrow_mut().raw = v;
    Ok(Value::Null)
}
fn tz_dst(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(tz_of(&a[0])?.borrow().dst_savings()))
}
fn tz_use_daylight(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(tz_of(&a[0])?.borrow().use_daylight()))
}
fn tz_observes(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(tz_of(&a[0])?.borrow().observes()))
}
fn tz_offset_at(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(tz_of(&a[0])?.borrow().offset(crate::lib_lang::long(&a[1]))))
}
/// `getOffset(era, year, month, day, dayOfWeek, millis)`: the offset at that standard local
/// time.
fn tz_offset6(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = tz_of(&a[0])?.borrow().clone();
    let (era, year, month, day, d, millis) = (int(&a[1]), int(&a[2]), int(&a[3]), int(&a[4]), int(&a[5]), int(&a[6]));
    if (era != 0 && era != 1) || !(0..=11).contains(&month) || !(1..=7).contains(&d) || millis < 0 || millis as i64 >= MS_DAY {
        return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, None).with_frames(&["java.base/sun.util.calendar.ZoneInfo.getOffset(ZoneInfo.java:383)"]));
    }
    let y = if era == 0 { 1 - year as i64 } else { year as i64 };
    let mut cal = Cal::new(JTimeZone::gmt(), crate::text::DEFAULT_LOCALE);
    let ed = if gregorian_ed(y, month as i64 + 1, day as i64) >= cal.cut_ed { gregorian_ed(y, month as i64 + 1, day as i64) } else { julian_ed(y, month as i64 + 1, day as i64) };
    cal.set_time_in_millis(ed * MS_DAY + millis as i64);
    let local = ed * MS_DAY + millis as i64;
    Ok(Value::Int(z.offset(local - z.raw as i64)))
}
fn tz_in_daylight(_: &mut Vm, a: &mut [Value]) -> VR {
    let ms = date_ms(&a[1])?;
    Ok(Value::Bool(tz_of(&a[0])?.borrow().offsets(ms).1 != 0))
}
fn tz_to_zone_id(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(super::time_value(TimeVal::ZoneId(tz_of(&a[0])?.borrow().to_zone_id()?)))
}
fn tz_name_style(style: i32) -> Result<bool, Thrown> {
    match style {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(iae(format!("Illegal style: {style}")).with_frames(&["java.base/java.util.TimeZone.getDisplayName(TimeZone.java:427)", "java.base/java.util.TimeZone.getDisplayName(TimeZone.java:388)"])),
    }
}
fn tz_display(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(tz_of(&a[0])?.borrow().display_name(false, true, crate::text::DEFAULT_LOCALE)))
}
fn tz_display_l(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = locale_str(&a[1])?;
    Ok(Value::string(tz_of(&a[0])?.borrow().display_name(false, true, &l)))
}
fn tz_display2(_: &mut Vm, a: &mut [Value]) -> VR {
    let long = tz_name_style(int(&a[2]))?;
    Ok(Value::string(tz_of(&a[0])?.borrow().display_name(crate::lib_lang::boolean(&a[1]), long, crate::text::DEFAULT_LOCALE)))
}
fn tz_display3(_: &mut Vm, a: &mut [Value]) -> VR {
    let long = tz_name_style(int(&a[2]))?;
    let l = locale_str(&a[3])?;
    Ok(Value::string(tz_of(&a[0])?.borrow().display_name(crate::lib_lang::boolean(&a[1]), long, &l)))
}
fn tz_same_rules(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = tz_of(&a[0])?.borrow().clone();
    if a[1].is_null() {
        return Ok(Value::Bool(false));
    }
    let o = tz_of(&a[1])?.borrow().clone();
    Ok(Value::Bool(z.same_rules(&o)))
}
fn tz_clone(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(tz_value(tz_of(&a[0])?.borrow().clone()))
}

fn stz_new(_: &mut Vm, a: &mut [Value]) -> VR {
    let raw = int(&a[0]);
    let id = crate::lib_lang::string(&a[1], "ID")?.as_str().to_string();
    let mut r = SimpleRule { dst_savings: 3_600_000, ..SimpleRule::default() };
    let n = a.len();
    if n >= 10 {
        r.start_month = int(&a[2]);
        r.start_day = int(&a[3]);
        r.start_dow = int(&a[4]);
        r.start_time = int(&a[5]);
        let mut i = 6;
        if n == 13 {
            r.start_time_mode = int(&a[6]);
            i = 7;
        }
        r.end_month = int(&a[i]);
        r.end_day = int(&a[i + 1]);
        r.end_dow = int(&a[i + 2]);
        r.end_time = int(&a[i + 3]);
        i += 4;
        if n == 13 {
            r.end_time_mode = int(&a[i]);
            i += 1;
        }
        if n >= 11 {
            r.dst_savings = int(&a[i]);
        }
        if r.dst_savings <= 0 {
            return Err(iae(format!("Illegal daylight saving value: {}", r.dst_savings)));
        }
        // the shorter constructors delegate to the 13-argument one, which decodes the rules
        let outer: &[&str] = match n {
            10 => &["java.base/java.util.SimpleTimeZone.<init>(SimpleTimeZone.java:353)", "java.base/java.util.SimpleTimeZone.<init>(SimpleTimeZone.java:220)"],
            11 => &["java.base/java.util.SimpleTimeZone.<init>(SimpleTimeZone.java:353)", "java.base/java.util.SimpleTimeZone.<init>(SimpleTimeZone.java:271)"],
            _ => &["java.base/java.util.SimpleTimeZone.<init>(SimpleTimeZone.java:353)"],
        };
        let wrap = |mut t: Thrown, decode: &str| {
            let mut frames = vec![decode.to_string(), "java.base/java.util.SimpleTimeZone.decodeRules(SimpleTimeZone.java:1357)".to_string()];
            frames.extend(outer.iter().map(|s| s.to_string()));
            t.frames = frames;
            t
        };
        r.decode_start().map_err(|t| wrap(t, "java.base/java.util.SimpleTimeZone.decodeStartRule(SimpleTimeZone.java:1389)"))?;
        r.decode_end().map_err(|t| wrap(t, "java.base/java.util.SimpleTimeZone.decodeEndRule(SimpleTimeZone.java:1436)"))?;
    }
    Ok(tz_value(JTimeZone { id, raw, kind: TzKind::Simple(r) }))
}
fn stz_rule(z: &RefCell<JTimeZone>, f: impl FnOnce(&mut SimpleRule) -> Result<(), Thrown>) -> VR {
    let mut z = z.borrow_mut();
    match &mut z.kind {
        TzKind::Simple(r) => f(r)?,
        _ => return Err(crate::vm::cce("not a SimpleTimeZone")),
    }
    Ok(Value::Null)
}
fn stz_set_dst(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = int(&a[1]);
    if v <= 0 {
        return Err(iae(format!("Illegal daylight saving value: {v}")));
    }
    stz_rule(tz_of(&a[0])?, |r| {
        r.dst_savings = v;
        Ok(())
    })
}
fn stz_start_year(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = int(&a[1]);
    stz_rule(tz_of(&a[0])?, |r| {
        r.start_year = v;
        Ok(())
    })
}
/// `setStartRule` / `setEndRule` with 3, 4 or 5 arguments.
fn stz_rule_args(a: &[Value], start: bool) -> VR {
    let (month, mut day, mut d, time) = (int(&a[1]), int(&a[2]), if a.len() > 4 { int(&a[3]) } else { 0 }, if a.len() > 4 { int(&a[4]) } else { int(&a[3]) });
    if a.len() == 4 {
        // (month, dayOfMonth, time): an exact date
        d = 0;
    }
    if a.len() == 6 {
        // (month, dayOfMonth, dayOfWeek, time, after)
        let after = crate::lib_lang::boolean(&a[5]);
        if after {
            d = -d.abs();
        } else {
            d = -d.abs();
            day = -day.abs();
        }
    }
    stz_rule(tz_of(&a[0])?, |r| {
        if start {
            r.start_month = month;
            r.start_day = day;
            r.start_dow = d;
            r.start_time = time;
            r.start_time_mode = WALL_TIME;
            r.decode_start()
        } else {
            r.end_month = month;
            r.end_day = day;
            r.end_dow = d;
            r.end_time = time;
            r.end_time_mode = WALL_TIME;
            r.decode_end()
        }
    })
}
fn stz_set_start(_: &mut Vm, a: &mut [Value]) -> VR {
    stz_rule_args(a, true)
}
fn stz_set_end(_: &mut Vm, a: &mut [Value]) -> VR {
    stz_rule_args(a, false)
}

// Calendar

fn default_cal(zone: JTimeZone, locale: &str) -> Cal {
    Cal::now(zone, locale)
}
fn cal_get_instance(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(cal_value(default_cal(JTimeZone::gmt(), crate::text::DEFAULT_LOCALE)))
}
fn cal_get_instance_z(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = tz_of(&a[0])?.borrow().clone();
    Ok(cal_value(default_cal(z, crate::text::DEFAULT_LOCALE)))
}
fn cal_get_instance_zl(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = tz_of(&a[0])?.borrow().clone();
    let l = locale_str(&a[1])?;
    Ok(cal_value(default_cal(z, &l)))
}
fn gc_new_ymd(_: &mut Vm, a: &mut [Value]) -> VR {
    let mut c = Cal::new(JTimeZone::gmt(), crate::text::DEFAULT_LOCALE);
    let get = |i: usize| a.get(i).map(int).unwrap_or(0);
    let hod = get(3);
    c.set(YEAR, get(0));
    c.set(MONTH, get(1));
    c.set(DAY_OF_MONTH, get(2));
    if (12..=23).contains(&hod) {
        c.fields[AM_PM] = 1;
        c.fields[HOUR] = hod - 12;
    } else {
        c.fields[HOUR] = hod;
    }
    c.set_fields_computed((1 << HOUR) | (1 << AM_PM));
    c.set(HOUR_OF_DAY, hod);
    c.set(MINUTE, get(4));
    c.set(SECOND, get(5));
    c.fields[MILLISECOND] = 0;
    Ok(cal_value(c))
}
fn gc_new_z(_: &mut Vm, a: &mut [Value]) -> VR {
    cal_get_instance_z_impl(a)
}
fn cal_get_instance_z_impl(a: &[Value]) -> VR {
    let z = tz_of(&a[0])?.borrow().clone();
    let l = if a.len() > 1 { locale_str(&a[1])? } else { Rc::from(crate::text::DEFAULT_LOCALE) };
    Ok(cal_value(default_cal(z, &l)))
}

fn cal_get(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = cal_of(&a[0])?;
    let mut c = c.borrow_mut();
    c.complete().map_err(|e| frame(e, "Calendar.get(Calendar.java:1854)"))?;
    let f = field_arg(&a[1], &["java.base/java.util.Calendar.internalGet(Calendar.java:1868)", "java.base/java.util.Calendar.get(Calendar.java:1855)"])?;
    Ok(Value::Int(c.fields[f]))
}
fn cal_set(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_arg(&a[1], &["java.base/java.util.Calendar.internalSet(Calendar.java:1883)", "java.base/java.util.Calendar.set(Calendar.java:1904)"])?;
    let v = int(&a[2]);
    cal_of(&a[0])?.borrow_mut().set(f, v);
    Ok(Value::Null)
}
fn cal_set_ymd(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = cal_of(&a[0])?;
    let mut c = c.borrow_mut();
    let fields = [YEAR, MONTH, DAY_OF_MONTH, HOUR_OF_DAY, MINUTE, SECOND];
    for (i, v) in a[1..].iter().enumerate() {
        c.set(fields[i], int(v));
    }
    Ok(Value::Null)
}
fn cal_add(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = int(&a[1]);
    let amount = int(&a[2]);
    let c = cal_of(&a[0])?;
    if !(0..FIELD_COUNT as i32).contains(&f) {
        return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, None).with_frames(&["java.base/java.util.GregorianCalendar.add(GregorianCalendar.java:932)"]));
    }
    c.borrow_mut().add(f as usize, amount)?;
    Ok(Value::Null)
}
fn cal_roll(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = int(&a[1]);
    let amount = int(&a[2]);
    let c = cal_of(&a[0])?;
    if !(0..FIELD_COUNT as i32).contains(&f) {
        return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, None).with_frames(&["java.base/java.util.GregorianCalendar.roll(GregorianCalendar.java:1180)"]));
    }
    c.borrow_mut().roll(f as usize, amount)?;
    Ok(Value::Null)
}
fn cal_clear(_: &mut Vm, a: &mut [Value]) -> VR {
    cal_of(&a[0])?.borrow_mut().clear();
    Ok(Value::Null)
}
fn cal_clear_field(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_arg(&a[1], &["java.base/java.util.Calendar.clear(Calendar.java:2037)"])?;
    cal_of(&a[0])?.borrow_mut().clear_field(f);
    Ok(Value::Null)
}
fn cal_is_set(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_arg(&a[1], &["java.base/java.util.Calendar.isSet(Calendar.java:2056)"])?;
    Ok(Value::Bool(cal_of(&a[0])?.borrow().is_set(f)))
}
fn cal_get_time(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(date_value(cal_of(&a[0])?.borrow_mut().time_in_millis().map_err(|e| frame(e, "Calendar.getTime(Calendar.java:1781)"))?))
}
fn cal_set_time(_: &mut Vm, a: &mut [Value]) -> VR {
    let ms = date_ms(&a[1])?;
    cal_of(&a[0])?.borrow_mut().set_time_in_millis(ms);
    Ok(Value::Null)
}
fn cal_millis(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(cal_of(&a[0])?.borrow_mut().time_in_millis()?))
}
fn cal_set_millis(_: &mut Vm, a: &mut [Value]) -> VR {
    let ms = crate::lib_lang::long(&a[1]);
    cal_of(&a[0])?.borrow_mut().set_time_in_millis(ms);
    Ok(Value::Null)
}
fn cal_to_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let ms = cal_of(&a[0])?.borrow_mut().time_in_millis().map_err(|e| frame(e, "Calendar.toInstant(Calendar.java:3638)"))?;
    Ok(super::time_value(TimeVal::Instant { secs: ms.div_euclid(1000), nanos: (ms.rem_euclid(1000) * 1_000_000) as i32 }))
}
fn cal_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(tz_value(cal_of(&a[0])?.borrow().zone.clone()))
}
fn cal_set_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = tz_of(&a[1])?.borrow().clone();
    let c = cal_of(&a[0])?;
    let mut c = c.borrow_mut();
    c.zone = z;
    c.are_fields_set = false;
    c.are_all_fields_set = false;
    Ok(Value::Null)
}
fn cal_lenient(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(cal_of(&a[0])?.borrow().lenient))
}
fn cal_set_lenient(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = crate::lib_lang::boolean(&a[1]);
    cal_of(&a[0])?.borrow_mut().lenient = v;
    Ok(Value::Null)
}
fn cal_first_dow(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(cal_of(&a[0])?.borrow().first_dow))
}
fn cal_set_first_dow(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = int(&a[1]);
    let c = cal_of(&a[0])?;
    let mut c = c.borrow_mut();
    if c.first_dow != v {
        c.first_dow = v;
        c.invalidate_week_fields();
    }
    Ok(Value::Null)
}
fn cal_min_days(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(cal_of(&a[0])?.borrow().min_days))
}
fn cal_set_min_days(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = int(&a[1]);
    let c = cal_of(&a[0])?;
    let mut c = c.borrow_mut();
    if c.min_days != v {
        c.min_days = v;
        c.invalidate_week_fields();
    }
    Ok(Value::Null)
}
fn cal_limit(a: &[Value], which: u8) -> VR {
    let frames: &[&str] = match which {
        0 => &["java.base/java.util.GregorianCalendar.getMinimum(GregorianCalendar.java:1556)"],
        1 => &["java.base/java.util.GregorianCalendar.getGreatestMinimum(GregorianCalendar.java:1576)"],
        2 => &["java.base/java.util.GregorianCalendar.getLeastMaximum(GregorianCalendar.java:1636)"],
        3 => &["java.base/java.util.GregorianCalendar.getMaximum(GregorianCalendar.java:1597)"],
        _ => &[],
    };
    let raw = int(&a[1]);
    if which >= 4 && !(0..FIELD_COUNT as i32).contains(&raw) {
        let line = if which == 4 { "getActualMinimum(GregorianCalendar.java:1694)" } else { "getActualMaximum(GregorianCalendar.java:1962)" };
        return Err(Throw::new(cls::ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Array index out of range: {raw}"))).with_frames(&[&format!("java.base/java.util.GregorianCalendar.{line}")]));
    }
    let f = field_arg(&a[1], frames)?;
    let c = cal_of(&a[0])?;
    Ok(Value::Int(match which {
        0 => MIN_VALUES[f],
        1 => MIN_VALUES[f],
        2 => LEAST_MAX_VALUES[f],
        3 => MAX_VALUES[f],
        4 => c.borrow_mut().actual_minimum(f)?,
        _ => c.borrow_mut().actual_maximum(f)?,
    }))
}
fn cal_minimum(_: &mut Vm, a: &mut [Value]) -> VR {
    cal_limit(a, 0)
}
fn cal_greatest_min(_: &mut Vm, a: &mut [Value]) -> VR {
    cal_limit(a, 1)
}
fn cal_least_max(_: &mut Vm, a: &mut [Value]) -> VR {
    cal_limit(a, 2)
}
fn cal_maximum(_: &mut Vm, a: &mut [Value]) -> VR {
    cal_limit(a, 3)
}
fn cal_actual_min(_: &mut Vm, a: &mut [Value]) -> VR {
    cal_limit(a, 4)
}
fn cal_actual_max(_: &mut Vm, a: &mut [Value]) -> VR {
    cal_limit(a, 5)
}
fn cal_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    let x = cal_of(&a[0])?.borrow().millis_of();
    if a[1].is_null() {
        return Err(crate::vm::npe("Cannot read field \"isTimeSet\" because \"calendar\" is null").with_frames(&[
            "java.base/java.util.Calendar.getMillisOf(Calendar.java:3564)",
            "java.base/java.util.Calendar.compareTo(Calendar.java:3291)",
        ]));
    }
    let y = cal_of(&a[1])?.borrow().millis_of();
    Ok(Value::Int(x.cmp(&y) as i32))
}
fn cal_before(_: &mut Vm, a: &mut [Value]) -> VR {
    let x = cal_of(&a[0])?.borrow().millis_of();
    match legacy(&a[1]) {
        Some(LegacyObj::Calendar(o)) => Ok(Value::Bool(x < o.borrow().millis_of())),
        _ => Ok(Value::Bool(false)),
    }
}
fn cal_after(_: &mut Vm, a: &mut [Value]) -> VR {
    let x = cal_of(&a[0])?.borrow().millis_of();
    match legacy(&a[1]) {
        Some(LegacyObj::Calendar(o)) => Ok(Value::Bool(x > o.borrow().millis_of())),
        _ => Ok(Value::Bool(false)),
    }
}
fn cal_clone(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(cal_value(cal_of(&a[0])?.borrow().clone()))
}
fn cal_type(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::str("gregory"))
}
fn cal_types(_: &mut Vm, _: &mut [Value]) -> VR {
    let m = crate::value::JMap::new(cls::HASH_SET);
    for t in ["gregory", "buddhist", "japanese"] {
        m.insert_raw(Value::str(t), Value::Null);
    }
    Ok(Value::Set(Rc::new(m)))
}
fn cal_week_year(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(cal_of(&a[0])?.borrow_mut().week_year()? as i32))
}
fn cal_weeks_in_week_year(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = cal_of(&a[0])?;
    let mut gc = c.borrow().clone();
    gc.complete()?;
    let wy = gc.week_year()?;
    if wy == gc.normalized_year() {
        return Ok(Value::Int(gc.actual_maximum(WEEK_OF_YEAR)?));
    }
    let d = gc.fields[DAY_OF_WEEK];
    gc.set_week_date(wy as i32, 2, d)?;
    Ok(Value::Int(gc.actual_maximum(WEEK_OF_YEAR)?))
}
fn cal_set_week_date(_: &mut Vm, a: &mut [Value]) -> VR {
    let (wy, woy, d) = (int(&a[1]), int(&a[2]), int(&a[3]));
    cal_of(&a[0])?.borrow_mut().set_week_date(wy, woy, d)?;
    Ok(Value::Null)
}
fn cal_true(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Bool(true))
}

/// Calendar styles: `SHORT_FORMAT` 1, `LONG_FORMAT` 2, `NARROW_FORMAT` 4, standalone forms
/// with `0x8000` added. Returns (base, standalone).
fn style_of(style: i32) -> Option<(i32, bool)> {
    let standalone = style & 0x8000 != 0;
    let base = style & !0x8000;
    matches!(base, 1 | 2 | 4).then_some((base, standalone))
}

/// `Calendar.getDisplayName` text of a field value, or `None` when the field has none.
pub fn field_text(field: usize, value: i32, style: i32, locale: &str) -> Option<String> {
    let (base, standalone) = style_of(style)?;
    // the time module's styles: FULL 0, SHORT 2, NARROW 4, standalone + 1
    let ts = match base {
        2 => 0,
        1 => 2,
        _ => 4,
    } + standalone as u8;
    Some(match field {
        ERA => super::locale::era(locale, value.clamp(0, 1) as u8, ts & !1).to_string(),
        MONTH => {
            if !(0..=11).contains(&value) {
                return None;
            }
            super::locale::month(locale, value as u8 + 1, ts).to_string()
        }
        DAY_OF_WEEK => {
            if !(1..=7).contains(&value) {
                return None;
            }
            // Monday 1 … Sunday 7
            let d = if value == 1 { 7 } else { value - 1 };
            super::locale::day(locale, d as u8, ts).to_string()
        }
        AM_PM => super::locale::ampm(locale, value.clamp(0, 1) as u8).to_string(),
        _ => return None,
    })
}

fn cal_display_name(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = int(&a[1]);
    let style = int(&a[2]);
    let l = locale_str(&a[3])?;
    let c = cal_of(&a[0])?;
    if style_of(style).is_none() || !(0..FIELD_COUNT as i32).contains(&f) {
        return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, None).with_frames(&[
            "java.base/java.util.Calendar.checkDisplayNameParams(Calendar.java:2254)",
            "java.base/java.util.Calendar.getDisplayName(Calendar.java:2110)",
        ]));
    }
    let f = f as usize;
    let mut c = c.borrow_mut();
    c.complete().map_err(|e| frame(frame(e, "Calendar.get(Calendar.java:1854)"), "Calendar.getDisplayName(Calendar.java:2116)"))?;
    let v = c.fields[f];
    // the format styles read `DateFormatSymbols`, whose eras are the abbreviated ones
    let style = if f == ERA && matches!(style, 1 | 2) { 1 } else { style };
    Ok(field_text(f, v, style, &l).map(Value::string).unwrap_or(Value::Null))
}
fn cal_display_names(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = int(&a[1]);
    let style = int(&a[2]);
    let l = locale_str(&a[3])?;
    let c = cal_of(&a[0])?;
    if (style != 0 && style_of(style).is_none()) || !(0..FIELD_COUNT as i32).contains(&f) {
        return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, None).with_frames(&[
            "java.base/java.util.Calendar.checkDisplayNameParams(Calendar.java:2254)",
            "java.base/java.util.Calendar.getDisplayNames(Calendar.java:2205)",
        ]));
    }
    c.borrow_mut().complete()?;
    let f = f as usize;
    let range: Vec<i32> = match f {
        ERA | AM_PM => vec![0, 1],
        MONTH => (0..12).collect(),
        DAY_OF_WEEK => (1..=7).collect(),
        _ => return Ok(Value::Null),
    };
    let styles: Vec<i32> = match (style, f) {
        (0, ERA) => vec![2, 1, 4],
        (0, _) => vec![2, 1, 0x8002, 0x8001],
        _ => vec![style],
    };
    let mut names: Vec<(String, i32)> = Vec::new();
    for s in styles {
        for &v in &range {
            if let Some(n) = field_text(f, v, s, &l)
                && !names.iter().any(|(x, _)| *x == n)
            {
                names.push((n, v));
            }
        }
    }
    // `SHORT`/`LONG` read `DateFormatSymbols` into a `HashMap`; other styles come from the
    // locale provider as a `TreeMap` ordered longest name first, then alphabetically
    let hashed = matches!(style, 1 | 2);
    if !hashed {
        names.sort_by(|(a, _), (b, _)| crate::java::utf16_len(b).cmp(&crate::java::utf16_len(a)).then_with(|| crate::java::compare_strings(a, b).cmp(&0)));
    }
    let m = crate::value::JMap::new(if hashed { cls::HASH_MAP } else { cls::LINKED_HASH_MAP });
    for (n, v) in names {
        m.insert_raw(Value::string(n), Value::Int(v));
    }
    Ok(Value::Map(Rc::new(m)))
}
fn cal_available_locales(vm: &mut Vm, a: &mut [Value]) -> VR {
    crate::text::available_locales(vm, a)
}

fn gc_is_leap(_: &mut Vm, a: &mut [Value]) -> VR {
    let y = int(&a[1]) as i64;
    Ok(Value::Bool(cal_of(&a[0])?.borrow().is_leap(y)))
}
fn gc_change(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(date_value(cal_of(&a[0])?.borrow().cutover))
}
fn gc_set_change(_: &mut Vm, a: &mut [Value]) -> VR {
    let ms = date_ms(&a[1])?;
    let c = cal_of(&a[0])?;
    let mut c = c.borrow_mut();
    c.complete()?;
    c.set_cutover(ms);
    let t = c.time;
    c.is_time_set = false;
    c.set_time_in_millis(t);
    Ok(Value::Null)
}
fn gc_to_zoned(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = cal_of(&a[0])?;
    let mut c = c.borrow_mut();
    let ms = c.time_in_millis().map_err(|e| frame(e, "GregorianCalendar.toZonedDateTime(GregorianCalendar.java:3289)"))?;
    let zone = c.zone.zone_id();
    let secs = ms.div_euclid(1000);
    let offset = zone.offset_at(secs);
    let (d, t) = super::local_of_epoch(secs, (ms.rem_euclid(1000) * 1_000_000) as i32, offset);
    Ok(super::time_value(TimeVal::Zoned { d, t, offset, zone }))
}
fn gc_from(_: &mut Vm, a: &mut [Value]) -> VR {
    let (inst, zone) = match a[0].clone() {
        Value::Obj(o) => match &*o {
            Obj::Time(t @ TimeVal::Zoned { zone, .. }) => (t.instant(), zone.clone()),
            _ => return Err(crate::vm::cce("not a ZonedDateTime")),
        },
        _ => return Err(crate::vm::npe("Cannot invoke \"java.time.ZonedDateTime.getZone()\" because \"zdt\" is null")),
    };
    let (secs, nanos) = inst.unwrap_or((0, 0));
    let ms = secs.checked_mul(1000).and_then(|x| x.checked_add(nanos as i64 / 1_000_000)).ok_or_else(|| iae("java.lang.ArithmeticException: long overflow"))?;
    let mut c = Cal::new(JTimeZone::of_zone(&zone), crate::text::DEFAULT_LOCALE);
    c.set_cutover(i64::MIN);
    c.first_dow = 2;
    c.min_days = 4;
    c.set_time_in_millis(ms);
    Ok(cal_value(c))
}

fn builder_of(v: &Value) -> Result<&RefCell<CalBuilder>, Thrown> {
    match legacy(v) {
        Some(LegacyObj::CalendarBuilder(b)) => Ok(b),
        _ => Err(crate::vm::cce("not a Calendar.Builder")),
    }
}
fn cb_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::Legacy(Box::new(LegacyObj::CalendarBuilder(RefCell::new(CalBuilder::default()))))))
}
fn cb_with(a: &[Value], f: impl FnOnce(&mut CalBuilder) -> Result<(), Thrown>) -> VR {
    f(&mut builder_of(&a[0])?.borrow_mut())?;
    Ok(a[0].clone())
}
fn cb_set(_: &mut Vm, a: &mut [Value]) -> VR {
    let (f, v) = (int(&a[1]), int(&a[2]));
    cb_with(a, |b| {
        if !(0..FIELD_COUNT as i32).contains(&f) {
            return Err(iae("field is invalid").with_frames(&["java.base/java.util.Calendar$Builder.set(Calendar.java:1168)"]));
        }
        b.field_set(f as usize, v).map_err(|e| e.with_frames(&["java.base/java.util.Calendar$Builder.set(Calendar.java:1173)"]))
    })
}
fn cb_set_date(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, m, d) = (int(&a[1]), int(&a[2]), int(&a[3]));
    cb_with(a, |b| {
        let f = |e: Thrown| e.with_frames(&["java.base/java.util.Calendar$Builder.setFields(Calendar.java:1208)", "java.base/java.util.Calendar$Builder.setDate(Calendar.java:1241)"]);
        b.field_set(YEAR, y).map_err(f)?;
        b.field_set(MONTH, m).map_err(f)?;
        b.field_set(DAY_OF_MONTH, d).map_err(f)
    })
}
fn cb_set_time_of_day(_: &mut Vm, a: &mut [Value]) -> VR {
    let vals: Vec<i32> = a[1..].iter().map(int).collect();
    cb_with(a, |b| {
        b.field_set(HOUR_OF_DAY, vals[0])?;
        b.field_set(MINUTE, vals[1])?;
        b.field_set(SECOND, vals[2])?;
        b.field_set(MILLISECOND, vals.get(3).copied().unwrap_or(0))
    })
}
fn cb_set_week_date(_: &mut Vm, a: &mut [Value]) -> VR {
    let (y, w, d) = (int(&a[1]), int(&a[2]), int(&a[3]));
    cb_with(a, |b| {
        b.fields.retain(|(x, _)| *x != WEEK_YEAR);
        b.fields.push((WEEK_YEAR, y));
        b.field_set(WEEK_OF_YEAR, w)?;
        b.field_set(DAY_OF_WEEK, d)
    })
}
fn cb_set_fields(_: &mut Vm, a: &mut [Value]) -> VR {
    let vals: Vec<i32> = match &a[1] {
        Value::Obj(o) => match &**o {
            Obj::Array(arr) => arr.data.borrow().to_values().iter().map(int).collect(),
            _ => return Err(crate::vm::cce("not an int[]")),
        },
        _ => return Err(crate::vm::npe("Cannot read the array length because \"fieldValuePairs\" is null")),
    };
    cb_with(a, |b| {
        if !vals.len().is_multiple_of(2) {
            return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, None).with_frames(&["java.base/java.util.Calendar$Builder.setFields(Calendar.java:1205)"]));
        }
        for p in vals.chunks(2) {
            if p[0] < 0 || p[0] as usize >= FIELD_COUNT {
                return Err(iae(format!("field is invalid. field: {}", p[0])));
            }
            b.field_set(p[0] as usize, p[1])?;
        }
        Ok(())
    })
}
fn cb_set_instant(_: &mut Vm, a: &mut [Value]) -> VR {
    let ms = crate::lib_lang::long(&a[1]);
    cb_with(a, |b| {
        if !b.fields.is_empty() {
            return Err(Throw::new(cls::ILLEGAL_STATE_EXCEPTION, None).with_frames(&["java.base/java.util.Calendar$Builder.setInstant(Calendar.java:1122)"]));
        }
        b.instant = Some(ms);
        Ok(())
    })
}
fn cb_set_lenient(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = crate::lib_lang::boolean(&a[1]);
    cb_with(a, |b| {
        b.lenient = Some(v);
        Ok(())
    })
}
fn cb_set_type(_: &mut Vm, a: &mut [Value]) -> VR {
    let t = crate::lib_lang::string(&a[1], "type")?.as_str().to_string();
    cb_with(a, |b| {
        if t == "gregorian" {
            b.calendar_type = Some("gregory".into());
        } else if matches!(t.as_str(), "gregory" | "iso8601" | "buddhist" | "japanese") {
            b.calendar_type = Some(t);
        } else {
            return Err(iae(format!("unknown calendar type: {t}")).with_frames(&["java.base/java.util.Calendar$Builder.setCalendarType(Calendar.java:1367)"]));
        }
        Ok(())
    })
}
fn cb_set_locale(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = locale_str(&a[1])?;
    cb_with(a, |b| {
        b.locale = Some(l);
        Ok(())
    })
}
fn cb_set_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    let z = tz_of(&a[1])?.borrow().clone();
    cb_with(a, |b| {
        b.zone = Some(z);
        Ok(())
    })
}
fn cb_set_week(_: &mut Vm, a: &mut [Value]) -> VR {
    let (f, m) = (int(&a[1]), int(&a[2]));
    cb_with(a, |b| {
        if !(1..=7).contains(&f) || !(1..=7).contains(&m) {
            return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, None));
        }
        b.week = Some((f, m));
        Ok(())
    })
}
fn cb_build(_: &mut Vm, a: &mut [Value]) -> VR {
    let b = builder_of(&a[0])?.borrow().clone();
    Ok(cal_value(b.build()?))
}

macro_rules! int_consts {
    ($b:expr, $class:expr, [$($name:literal = $v:expr),* $(,)?]) => {
        $( $b.sf($class, concat!("int ", $name), |_, _| Ok(Value::Int($v))); )*
    };
}

pub fn register(b: &mut crate::lookup::Builder) {
    use cls::*;
    b.supers(TIME_ZONE, &[OBJECT]);
    b.supers(ZONE_INFO, &[TIME_ZONE]);
    b.supers(SIMPLE_TIME_ZONE, &[TIME_ZONE]);
    b.supers(CALENDAR, &[OBJECT, COMPARABLE]);
    b.supers(GREGORIAN_CALENDAR, &[CALENDAR]);
    b.supers(CALENDAR_BUILDER, &[OBJECT]);

    int_consts!(b, TIME_ZONE, ["LONG" = 1, "SHORT" = 0]);
    b.m(TIME_ZONE, "def clone()", tz_clone);
    b.s(TIME_ZONE, "String[] getAvailableIDs()", tz_available);
    b.s(TIME_ZONE, "String[] getAvailableIDs(int)", tz_available_offset);
    b.s(TIME_ZONE, "TimeZone getDefault()", tz_default);
    b.m(TIME_ZONE, "String getDisplayName()", tz_display);
    b.m(TIME_ZONE, "String getDisplayName(boolean,int)", tz_display2);
    b.m(TIME_ZONE, "String getDisplayName(boolean,int,Locale)", tz_display3);
    b.m(TIME_ZONE, "String getDisplayName(Locale)", tz_display_l);
    b.m(TIME_ZONE, "int getDSTSavings()", tz_dst);
    b.m(TIME_ZONE, "String getID()", tz_id);
    b.m(TIME_ZONE, "int getOffset(int,int,int,int,int,int)", tz_offset6);
    b.m(TIME_ZONE, "int getOffset(long)", tz_offset_at);
    b.m(TIME_ZONE, "int getRawOffset()", tz_raw);
    b.s(TIME_ZONE, "TimeZone getTimeZone(String)", tz_get_time_zone);
    b.m(TIME_ZONE, "boolean hasSameRules(TimeZone)", tz_same_rules);
    b.m(TIME_ZONE, "boolean inDaylightTime(Date)", tz_in_daylight);
    b.m(TIME_ZONE, "boolean observesDaylightTime()", tz_observes);
    b.m(TIME_ZONE, "void setRawOffset(int)", tz_set_raw);
    b.m(TIME_ZONE, "ZoneId toZoneId()", tz_to_zone_id);
    b.m(TIME_ZONE, "boolean useDaylightTime()", tz_use_daylight);

    int_consts!(b, SIMPLE_TIME_ZONE, ["STANDARD_TIME" = 1, "UTC_TIME" = 2, "WALL_TIME" = 0]);
    b.c(SIMPLE_TIME_ZONE, "(int,String)", stz_new);
    b.c(SIMPLE_TIME_ZONE, "(int,String,int,int,int,int,int,int,int,int)", stz_new);
    b.c(SIMPLE_TIME_ZONE, "(int,String,int,int,int,int,int,int,int,int,int)", stz_new);
    b.c(SIMPLE_TIME_ZONE, "(int,String,int,int,int,int,int,int,int,int,int,int,int)", stz_new);
    b.m(SIMPLE_TIME_ZONE, "int getDSTSavings()", tz_dst);
    b.m(SIMPLE_TIME_ZONE, "void setDSTSavings(int)", stz_set_dst);
    b.m(SIMPLE_TIME_ZONE, "void setEndRule(int,int,int)", stz_set_end);
    b.m(SIMPLE_TIME_ZONE, "void setEndRule(int,int,int,int)", stz_set_end);
    b.m(SIMPLE_TIME_ZONE, "void setEndRule(int,int,int,int,boolean)", stz_set_end);
    b.m(SIMPLE_TIME_ZONE, "void setStartRule(int,int,int)", stz_set_start);
    b.m(SIMPLE_TIME_ZONE, "void setStartRule(int,int,int,int)", stz_set_start);
    b.m(SIMPLE_TIME_ZONE, "void setStartRule(int,int,int,int,boolean)", stz_set_start);
    b.m(SIMPLE_TIME_ZONE, "void setStartYear(int)", stz_start_year);

    int_consts!(b, CALENDAR, [
        "ALL_STYLES" = 0, "AM" = 0, "AM_PM" = 9, "APRIL" = 3, "AUGUST" = 7, "DATE" = 5, "DAY_OF_MONTH" = 5,
        "DAY_OF_WEEK" = 7, "DAY_OF_WEEK_IN_MONTH" = 8, "DAY_OF_YEAR" = 6, "DECEMBER" = 11, "DST_OFFSET" = 16,
        "ERA" = 0, "FEBRUARY" = 1, "FIELD_COUNT" = 17, "FRIDAY" = 6, "HOUR" = 10, "HOUR_OF_DAY" = 11,
        "JANUARY" = 0, "JULY" = 6, "JUNE" = 5, "LONG" = 2, "LONG_FORMAT" = 2, "LONG_STANDALONE" = 32770,
        "MARCH" = 2, "MAY" = 4, "MILLISECOND" = 14, "MINUTE" = 12, "MONDAY" = 2, "MONTH" = 2,
        "NARROW_FORMAT" = 4, "NARROW_STANDALONE" = 32772, "NOVEMBER" = 10, "OCTOBER" = 9, "PM" = 1,
        "SATURDAY" = 7, "SECOND" = 13, "SEPTEMBER" = 8, "SHORT" = 1, "SHORT_FORMAT" = 1,
        "SHORT_STANDALONE" = 32769, "SUNDAY" = 1, "THURSDAY" = 5, "TUESDAY" = 3, "UNDECIMBER" = 12,
        "WEDNESDAY" = 4, "WEEK_OF_MONTH" = 4, "WEEK_OF_YEAR" = 3, "YEAR" = 1, "ZONE_OFFSET" = 15,
    ]);
    b.m(CALENDAR, "void add(int,int)", cal_add);
    b.m(CALENDAR, "boolean after(Object)", cal_after);
    b.m(CALENDAR, "boolean before(Object)", cal_before);
    b.m(CALENDAR, "void clear()", cal_clear);
    b.m(CALENDAR, "void clear(int)", cal_clear_field);
    b.m(CALENDAR, "def clone()", cal_clone);
    b.m(CALENDAR, "int compareTo(Calendar)", cal_compare);
    b.m(CALENDAR, "int get(int)", cal_get);
    b.m(CALENDAR, "int getActualMaximum(int)", cal_actual_max);
    b.m(CALENDAR, "int getActualMinimum(int)", cal_actual_min);
    b.s(CALENDAR, "Set getAvailableCalendarTypes()", cal_types);
    b.s(CALENDAR, "Locale[] getAvailableLocales()", cal_available_locales);
    b.m(CALENDAR, "String getCalendarType()", cal_type);
    b.m(CALENDAR, "String getDisplayName(int,int,Locale)", cal_display_name);
    b.m(CALENDAR, "Map getDisplayNames(int,int,Locale)", cal_display_names);
    b.m(CALENDAR, "int getFirstDayOfWeek()", cal_first_dow);
    b.m(CALENDAR, "int getGreatestMinimum(int)", cal_greatest_min);
    let m = b.s(CALENDAR, "Calendar getInstance()", cal_get_instance);
    b.nondeterministic(m);
    let m = b.s(CALENDAR, "Calendar getInstance(TimeZone)", cal_get_instance_z);
    b.nondeterministic(m);
    let m = b.s(CALENDAR, "Calendar getInstance(TimeZone,Locale)", cal_get_instance_zl);
    b.nondeterministic(m);
    b.m(CALENDAR, "int getLeastMaximum(int)", cal_least_max);
    b.m(CALENDAR, "int getMaximum(int)", cal_maximum);
    b.m(CALENDAR, "int getMinimalDaysInFirstWeek()", cal_min_days);
    b.m(CALENDAR, "int getMinimum(int)", cal_minimum);
    b.m(CALENDAR, "Date getTime()", cal_get_time);
    b.m(CALENDAR, "long getTimeInMillis()", cal_millis);
    b.m(CALENDAR, "TimeZone getTimeZone()", cal_zone);
    b.m(CALENDAR, "int getWeeksInWeekYear()", cal_weeks_in_week_year);
    b.m(CALENDAR, "int getWeekYear()", cal_week_year);
    b.m(CALENDAR, "boolean isLenient()", cal_lenient);
    b.m(CALENDAR, "boolean isSet(int)", cal_is_set);
    b.m(CALENDAR, "boolean isWeekDateSupported()", cal_true);
    b.m(CALENDAR, "void roll(int,int)", cal_roll);
    b.m(CALENDAR, "void set(int,int)", cal_set);
    b.m(CALENDAR, "void set(int,int,int)", cal_set_ymd);
    b.m(CALENDAR, "void set(int,int,int,int,int)", cal_set_ymd);
    b.m(CALENDAR, "void set(int,int,int,int,int,int)", cal_set_ymd);
    b.m(CALENDAR, "void setFirstDayOfWeek(int)", cal_set_first_dow);
    b.m(CALENDAR, "void setLenient(boolean)", cal_set_lenient);
    b.m(CALENDAR, "void setMinimalDaysInFirstWeek(int)", cal_set_min_days);
    b.m(CALENDAR, "void setTime(Date)", cal_set_time);
    b.m(CALENDAR, "void setTimeInMillis(long)", cal_set_millis);
    b.m(CALENDAR, "void setTimeZone(TimeZone)", cal_set_zone);
    b.m(CALENDAR, "void setWeekDate(int,int,int)", cal_set_week_date);
    b.m(CALENDAR, "Instant toInstant()", cal_to_instant);

    int_consts!(b, GREGORIAN_CALENDAR, ["AD" = 1, "BC" = 0]);
    let m = b.c(GREGORIAN_CALENDAR, "()", gc_new_now);
    b.nondeterministic(m);
    b.c(GREGORIAN_CALENDAR, "(int,int,int)", gc_new_ymd);
    b.c(GREGORIAN_CALENDAR, "(int,int,int,int,int)", gc_new_ymd);
    b.c(GREGORIAN_CALENDAR, "(int,int,int,int,int,int)", gc_new_ymd);
    let m = b.c(GREGORIAN_CALENDAR, "(TimeZone)", gc_new_z);
    b.nondeterministic(m);
    let m = b.c(GREGORIAN_CALENDAR, "(TimeZone,Locale)", gc_new_z);
    b.nondeterministic(m);
    b.s(GREGORIAN_CALENDAR, "GregorianCalendar from(ZonedDateTime)", gc_from);
    b.m(GREGORIAN_CALENDAR, "Date getGregorianChange()", gc_change);
    b.m(GREGORIAN_CALENDAR, "boolean isLeapYear(int)", gc_is_leap);
    b.m(GREGORIAN_CALENDAR, "void setGregorianChange(Date)", gc_set_change);
    b.m(GREGORIAN_CALENDAR, "ZonedDateTime toZonedDateTime()", gc_to_zoned);

    b.c(CALENDAR_BUILDER, "()", cb_new);
    b.m(CALENDAR_BUILDER, "Calendar build()", cb_build);
    b.m(CALENDAR_BUILDER, "Calendar.Builder set(int,int)", cb_set);
    b.m(CALENDAR_BUILDER, "Calendar.Builder setCalendarType(String)", cb_set_type);
    b.m(CALENDAR_BUILDER, "Calendar.Builder setDate(int,int,int)", cb_set_date);
    b.m(CALENDAR_BUILDER, "Calendar.Builder setFields(int[])", cb_set_fields);
    b.m(CALENDAR_BUILDER, "Calendar.Builder setInstant(long)", cb_set_instant);
    b.m(CALENDAR_BUILDER, "Calendar.Builder setLenient(boolean)", cb_set_lenient);
    b.m(CALENDAR_BUILDER, "Calendar.Builder setLocale(Locale)", cb_set_locale);
    b.m(CALENDAR_BUILDER, "Calendar.Builder setTimeOfDay(int,int,int)", cb_set_time_of_day);
    b.m(CALENDAR_BUILDER, "Calendar.Builder setTimeOfDay(int,int,int,int)", cb_set_time_of_day);
    b.m(CALENDAR_BUILDER, "Calendar.Builder setTimeZone(TimeZone)", cb_set_zone);
    b.m(CALENDAR_BUILDER, "Calendar.Builder setWeekDate(int,int,int)", cb_set_week_date);
    b.m(CALENDAR_BUILDER, "Calendar.Builder setWeekDefinition(int,int)", cb_set_week);
}

fn gc_new_now(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(cal_value(default_cal(JTimeZone::gmt(), crate::text::DEFAULT_LOCALE)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn julian_and_gregorian_days_round_trip() {
        for ed in [-200_000i64, -141_428, -141_427, -1, 0, 1, 18_000, 3_000_000] {
            let (y, m, d) = gregorian_from_ed(ed);
            assert_eq!(gregorian_ed(y, m, d), ed);
            let (y, m, d) = julian_from_ed(ed);
            assert_eq!(julian_ed(y, m, d), ed);
        }
        // the day before the cutover is October 4, 1582 (Julian)
        assert_eq!(julian_from_ed(-141_428), (1582, 10, 4));
        assert_eq!(gregorian_from_ed(-141_427), (1582, 10, 15));
        assert_eq!(dow(0), 5);
    }

    #[test]
    fn custom_ids() {
        assert_eq!(JTimeZone::of("GMT+5").id, "GMT+05:00");
        assert_eq!(JTimeZone::of("GMT+0530").raw, 19_800_000);
        assert_eq!(JTimeZone::of("GMT-00:00").id, "GMT-00:00");
        assert_eq!(JTimeZone::of("UTC+5").id, "GMT");
        assert_eq!(JTimeZone::of("GMT+24").id, "GMT");
    }

    #[test]
    fn fields_from_time() {
        let mut c = Cal::new(JTimeZone::of("UTC"), "en_US");
        c.set_time_in_millis(1_600_000_000_000);
        let f = c.fields;
        assert_eq!((f[YEAR], f[MONTH], f[DAY_OF_MONTH], f[HOUR_OF_DAY], f[DAY_OF_WEEK], f[DAY_OF_YEAR], f[WEEK_OF_YEAR], f[WEEK_OF_MONTH]), (2020, 8, 13, 12, 1, 257, 38, 3));
    }

    #[test]
    fn lenient_resolution() {
        let mut c = Cal::new(JTimeZone::of("UTC"), "en_US");
        c.clear();
        c.set(YEAR, 2020);
        c.set(MONTH, 1);
        c.set(DAY_OF_MONTH, 31);
        assert_eq!(c.time_in_millis().unwrap(), 1_583_107_200_000);
        // the Gregorian gap: October 10, 1582 is read as Julian
        c.clear();
        c.set(YEAR, 1582);
        c.set(MONTH, 9);
        c.set(DAY_OF_MONTH, 10);
        assert_eq!(c.time_in_millis().unwrap(), -12_218_860_800_000);
    }
}
