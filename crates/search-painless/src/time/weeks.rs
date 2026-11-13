//! `java.time.temporal.IsoFields`, `JulianFields` and `WeekFields`: temporal fields that are not
//! `ChronoField`s.
//!
//! Week numbering follows `WeekFields.ComputedDayOfField`: a week definition is a first day of
//! week and a minimal number of days in the first week; the week of a date is computed from the
//! offset of the first week start inside its month or year, and dates in the partial week
//! straddling a year end belong to the neighbouring week-based year. ISO weeks are
//! `WeekFields.of(MONDAY, 4)`. Fields read dates (`LocalDate`, `LocalDateTime`, `ZonedDateTime`,
//! `OffsetDateTime`); quarter fields also accept anything with a month (`YearMonth`).

use super::{dte, is_leap, replace_date, to_local_date, unsupported, Date, TimeVal};
use crate::vm::Thrown;

/// `WeekFields`: first day of week (1 Monday … 7 Sunday) and minimal days in the first week.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WeekDef {
    pub first: u8,
    pub min_days: u8,
}

impl WeekDef {
    pub const ISO: WeekDef = WeekDef { first: 1, min_days: 4 };
    pub const SUNDAY_START: WeekDef = WeekDef { first: 7, min_days: 1 };

    /// `WeekFields.of(Locale)`: the CLDR week data of the locale (`Locale.toString()`).
    pub fn of_locale(locale: &str) -> WeekDef {
        let (first, min_days) = super::locale::week_def(&locale.replace('-', "_"));
        WeekDef { first, min_days }
    }

    pub fn describe(self) -> String {
        format!("WeekFields[{},{}]", super::dow_name(self.first), self.min_days)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WeekKind {
    DayOfWeek,
    WeekOfMonth,
    WeekOfYear,
    WeekOfWeekBasedYear,
    WeekBasedYear,
}

/// A temporal field outside `ChronoField`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum XField {
    DayOfQuarter,
    QuarterOfYear,
    /// `IsoFields.WEEK_OF_WEEK_BASED_YEAR`.
    IsoWeekOfWeekBasedYear,
    /// `IsoFields.WEEK_BASED_YEAR`.
    IsoWeekBasedYear,
    JulianDay,
    ModifiedJulianDay,
    RataDie,
    Week(WeekKind, WeekDef),
}

/// A temporal unit outside `ChronoUnit` (`IsoFields.QUARTER_YEARS`, `WEEK_BASED_YEARS`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum XUnit {
    QuarterYears,
    WeekBasedYears,
}

impl XUnit {
    pub fn name(self) -> &'static str {
        match self {
            XUnit::QuarterYears => "QuarterYears",
            XUnit::WeekBasedYears => "WeekBasedYears",
        }
    }

    /// `addTo`: quarters are three months; week-based years keep the week and day of week.
    pub fn add_to(self, t: &TimeVal, n: i64) -> Result<TimeVal, Thrown> {
        match self {
            XUnit::QuarterYears => t.plus_unit(n.checked_mul(3).ok_or_else(|| super::arith("long overflow"))?, super::Unit::Months),
            XUnit::WeekBasedYears => {
                let d = to_local_date(t).map_err(|_| unsupported(format!("Unsupported unit: {}", self.name())))?;
                let y = iso_week_based_year(d).checked_add(n).ok_or_else(|| super::arith("long overflow"))?;
                XField::IsoWeekBasedYear.adjust(t, y)
            }
        }
    }

    /// `between`: whole quarters / week-based years from `a` to `b`.
    pub fn between(self, a: &TimeVal, b: &TimeVal) -> Result<i64, Thrown> {
        match self {
            XUnit::QuarterYears => Ok(a.until(b, super::Unit::Months)? / 3),
            // the difference of the week-based years, whole or not
            XUnit::WeekBasedYears => Ok(XField::IsoWeekBasedYear.get(b)? - XField::IsoWeekBasedYear.get(a)?),
        }
    }
}

const JULIAN_OFFSET: i64 = 2_440_588;
const MJD_OFFSET: i64 = 40_587;
const RATA_DIE_OFFSET: i64 = 719_163;

fn floor_mod(a: i64, b: i64) -> i64 {
    a.rem_euclid(b)
}

pub(crate) fn localized_dow(d: Date, w: WeekDef) -> i64 {
    floor_mod(d.day_of_week() as i64 - w.first as i64, 7) + 1
}

/// Offset of the first week start in a month or year (zero origin) for `day` falling on
/// localized day of week `dow`.
fn start_of_week_offset(day: i64, dow: i64, w: WeekDef) -> i64 {
    let week_start = floor_mod(day - dow, 7);
    if week_start + 1 > w.min_days as i64 { 7 - week_start } else { -week_start }
}

fn compute_week(offset: i64, day: i64) -> i64 {
    (7 + offset + (day - 1)).div_euclid(7)
}

pub(crate) fn week_of_month(d: Date, w: WeekDef) -> i64 {
    let dom = d.d as i64;
    compute_week(start_of_week_offset(dom, localized_dow(d, w), w), dom)
}

fn week_of_year(d: Date, w: WeekDef) -> i64 {
    let doy = d.day_of_year() as i64;
    compute_week(start_of_week_offset(doy, localized_dow(d, w), w), doy)
}

pub(crate) fn week_based_year(d: Date, w: WeekDef) -> i64 {
    let dow = localized_dow(d, w);
    let doy = d.day_of_year() as i64;
    let offset = start_of_week_offset(doy, dow, w);
    let week = compute_week(offset, doy);
    if week == 0 {
        return d.y as i64 - 1;
    }
    let year_len = d.len_of_year() as i64;
    let new_year_week = compute_week(offset, year_len + w.min_days as i64);
    if week >= new_year_week { d.y as i64 + 1 } else { d.y as i64 }
}

pub(crate) fn week_of_week_based_year(d: Date, w: WeekDef) -> i64 {
    let dow = localized_dow(d, w);
    let doy = d.day_of_year() as i64;
    let offset = start_of_week_offset(doy, dow, w);
    let mut week = compute_week(offset, doy);
    if week == 0 {
        // on or before the first week: count in the previous year
        let prev = Date::from_epoch_day(d.epoch_day() - doy);
        return week_of_week_based_year(prev, w);
    }
    if week > 50 {
        let year_len = d.len_of_year() as i64;
        let new_year_week = compute_week(offset, year_len + w.min_days as i64);
        if week >= new_year_week {
            week = week - new_year_week + 1;
        }
    }
    week
}

/// Epoch day of the first day of week 1 of week-based year `y` (possibly in December of the
/// year before).
pub(crate) fn week1_start(y: i32, w: WeekDef) -> i64 {
    let jan1 = Date { y, m: 1, d: 1 };
    let offset = start_of_week_offset(1, localized_dow(jan1, w), w);
    jan1.epoch_day() - offset
}

pub fn iso_week_based_year(d: Date) -> i64 {
    week_based_year(d, WeekDef::ISO)
}

pub fn iso_week_of_week_based_year(d: Date) -> i64 {
    week_of_week_based_year(d, WeekDef::ISO)
}

/// Weeks in the week-based year `y` of definition `w` (52 or 53).
fn weeks_in_week_based_year(y: i64, w: WeekDef) -> i64 {
    // the week of the last day that still belongs to year `y`
    let mut d = Date { y: y as i32, m: 12, d: 31 };
    while week_based_year(d, w) != y {
        d = Date::from_epoch_day(d.epoch_day() - 7);
    }
    week_of_week_based_year(d, w)
}

fn quarter_len(y: i64, q: i64) -> i64 {
    match q {
        1 => if is_leap(y) { 91 } else { 90 },
        2 => 91,
        _ => 92,
    }
}

fn first_day_of_quarter(d: Date) -> Date {
    Date { y: d.y, m: ((d.m - 1) / 3) * 3 + 1, d: 1 }
}

impl XField {
    pub fn name(self) -> String {
        match self {
            XField::DayOfQuarter => "DayOfQuarter".into(),
            XField::QuarterOfYear => "QuarterOfYear".into(),
            XField::IsoWeekOfWeekBasedYear => "WeekOfWeekBasedYear".into(),
            XField::IsoWeekBasedYear => "WeekBasedYear".into(),
            XField::JulianDay => "JulianDay".into(),
            XField::ModifiedJulianDay => "ModifiedJulianDay".into(),
            XField::RataDie => "RataDie".into(),
            XField::Week(k, w) => {
                let n = match k {
                    WeekKind::DayOfWeek => "DayOfWeek",
                    WeekKind::WeekOfMonth => "WeekOfMonth",
                    WeekKind::WeekOfYear => "WeekOfYear",
                    WeekKind::WeekOfWeekBasedYear => "WeekOfWeekBasedYear",
                    WeekKind::WeekBasedYear => "WeekBasedYear",
                };
                format!("{n}[{}]", w.describe())
            }
        }
    }

    pub fn is_date_based(self) -> bool {
        true
    }

    /// `isSupportedBy`.
    pub fn is_supported_by(self, t: &TimeVal) -> bool {
        match self {
            XField::QuarterOfYear => t.is_supported(super::Field::MonthOfYear),
            _ => to_local_date(t).is_ok(),
        }
    }

    fn date(self, t: &TimeVal) -> Result<Date, Thrown> {
        to_local_date(t).map_err(|_| unsupported(format!("Unsupported field: {}", self.name())))
    }

    /// `getFrom`.
    pub fn get(self, t: &TimeVal) -> Result<i64, Thrown> {
        if self == XField::QuarterOfYear {
            if !t.is_supported(super::Field::MonthOfYear) {
                return Err(unsupported(format!("Unsupported field: {}", self.name())));
            }
            return Ok((t.get_long(super::Field::MonthOfYear)? - 1) / 3 + 1);
        }
        let d = self.date(t)?;
        Ok(match self {
            XField::DayOfQuarter => d.epoch_day() - first_day_of_quarter(d).epoch_day() + 1,
            XField::IsoWeekOfWeekBasedYear => iso_week_of_week_based_year(d),
            XField::IsoWeekBasedYear => iso_week_based_year(d),
            XField::JulianDay => d.epoch_day() + JULIAN_OFFSET,
            XField::ModifiedJulianDay => d.epoch_day() + MJD_OFFSET,
            XField::RataDie => d.epoch_day() + RATA_DIE_OFFSET,
            XField::Week(k, w) => match k {
                WeekKind::DayOfWeek => localized_dow(d, w),
                WeekKind::WeekOfMonth => week_of_month(d, w),
                WeekKind::WeekOfYear => week_of_year(d, w),
                WeekKind::WeekOfWeekBasedYear => week_of_week_based_year(d, w),
                WeekKind::WeekBasedYear => week_based_year(d, w),
            },
            XField::QuarterOfYear => unreachable!(),
        })
    }

    /// `range()`: the outer range of the field.
    pub fn range(self) -> (i64, i64, i64, i64) {
        match self {
            XField::DayOfQuarter => (1, 1, 90, 92),
            XField::QuarterOfYear => (1, 1, 4, 4),
            XField::IsoWeekOfWeekBasedYear => (1, 1, 52, 53),
            XField::IsoWeekBasedYear => (super::MIN_YEAR, super::MIN_YEAR, super::MAX_YEAR, super::MAX_YEAR),
            XField::JulianDay => (-365_243_219_162 + JULIAN_OFFSET, -365_243_219_162 + JULIAN_OFFSET, 365_241_780_471 + JULIAN_OFFSET, 365_241_780_471 + JULIAN_OFFSET),
            XField::ModifiedJulianDay => (-365_243_219_162 + MJD_OFFSET, -365_243_219_162 + MJD_OFFSET, 365_241_780_471 + MJD_OFFSET, 365_241_780_471 + MJD_OFFSET),
            XField::RataDie => (-365_243_219_162 + RATA_DIE_OFFSET, -365_243_219_162 + RATA_DIE_OFFSET, 365_241_780_471 + RATA_DIE_OFFSET, 365_241_780_471 + RATA_DIE_OFFSET),
            XField::Week(k, _) => match k {
                WeekKind::DayOfWeek => (1, 1, 7, 7),
                WeekKind::WeekOfMonth => (0, 1, 4, 6),
                WeekKind::WeekOfYear => (0, 1, 52, 54),
                WeekKind::WeekOfWeekBasedYear => (1, 1, 52, 53),
                WeekKind::WeekBasedYear => (super::MIN_YEAR, super::MIN_YEAR, super::MAX_YEAR, super::MAX_YEAR),
            },
        }
    }

    /// `rangeRefinedBy`: the range for this particular date.
    pub fn range_refined(self, t: &TimeVal) -> Result<(i64, i64, i64, i64), Thrown> {
        let d = self.date(t)?;
        Ok(match self {
            XField::DayOfQuarter => {
                let q = (d.m as i64 - 1) / 3 + 1;
                let n = quarter_len(d.y as i64, q);
                (1, 1, n, n)
            }
            XField::IsoWeekOfWeekBasedYear => {
                let n = weeks_in_week_based_year(iso_week_based_year(d), WeekDef::ISO);
                (1, 1, n, n)
            }
            XField::Week(WeekKind::WeekOfWeekBasedYear, w) => {
                let n = weeks_in_week_based_year(week_based_year(d, w), w);
                (1, 1, n, n)
            }
            // weeks of the month or year spanned by its first and last days
            XField::Week(k @ (WeekKind::WeekOfMonth | WeekKind::WeekOfYear), w) => {
                let (day, len) = if k == WeekKind::WeekOfMonth { (d.d as i64, super::month_len(d.y as i64, d.m) as i64) } else { (d.day_of_year() as i64, d.len_of_year() as i64) };
                let offset = start_of_week_offset(day, localized_dow(d, w), w);
                let (lo, hi) = (compute_week(offset, 1), compute_week(offset, len));
                (lo, lo, hi, hi)
            }
            _ => self.range(),
        })
    }

    fn check(self, v: i64, lo: i64, hi: i64) -> Result<(), Thrown> {
        if v < lo || v > hi {
            return Err(dte(format!("Invalid value for {} (valid values {lo} - {hi}): {v}", self.name())));
        }
        Ok(())
    }

    /// `adjustInto` / `with(field, value)`: the value is checked against the field's outer
    /// range, then the date moves by the difference (so out-of-period values roll over, as
    /// in Java).
    pub fn adjust(self, t: &TimeVal, v: i64) -> Result<TimeVal, Thrown> {
        let (lo, _, _, hi) = self.range();
        if self == XField::QuarterOfYear {
            if !self.is_supported_by(t) {
                return Err(unsupported(format!("Unsupported field: {}", self.name())));
            }
            self.check(v, lo, hi).map_err(|e| super::jt(e, &["temporal.ValueRange.checkValidValue(ValueRange.java:319)", "temporal.IsoFields$Field$2.adjustInto(IsoFields.java:421)"]))?;
            let cur = self.get(t)?;
            return t.plus_unit((v - cur) * 3, super::Unit::Months);
        }
        let d = self.date(t)?;
        self.check(v, lo, hi)?;
        let day = |n: i64| Date::from_epoch_day(d.epoch_day() + n);
        let nd = match self {
            XField::IsoWeekBasedYear | XField::Week(WeekKind::WeekBasedYear, _) => {
                let w = if let XField::Week(_, w) = self { w } else { WeekDef::ISO };
                // keep the week (capped at the target year's last week) and the day of week
                let week = week_of_week_based_year(d, w).min(weeks_in_week_based_year(v, w));
                let dow = localized_dow(d, w);
                let mut probe = Date { y: v as i32, m: 7, d: 1 };
                probe = Date::from_epoch_day(probe.epoch_day() + (dow - localized_dow(probe, w)));
                let cur = week_of_week_based_year(probe, w);
                Date::from_epoch_day(probe.epoch_day() + (week - cur) * 7)
            }
            XField::JulianDay => Date::from_epoch_day(v - JULIAN_OFFSET),
            XField::ModifiedJulianDay => Date::from_epoch_day(v - MJD_OFFSET),
            XField::RataDie => Date::from_epoch_day(v - RATA_DIE_OFFSET),
            // days
            XField::DayOfQuarter | XField::Week(WeekKind::DayOfWeek, _) => day(v - self.get(t)?),
            // weeks
            _ => day((v - self.get(t)?) * 7),
        };
        replace_date(t, nd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u8, dd: u8) -> Date {
        Date { y, m, d: dd }
    }

    #[test]
    fn iso_weeks_at_year_boundaries() {
        // 2020-12-31 is a Thursday: week 53 of 2020; 2021-01-03 (Sunday) still belongs to it
        assert_eq!((iso_week_based_year(d(2020, 12, 31)), iso_week_of_week_based_year(d(2020, 12, 31))), (2020, 53));
        assert_eq!((iso_week_based_year(d(2021, 1, 3)), iso_week_of_week_based_year(d(2021, 1, 3))), (2020, 53));
        assert_eq!((iso_week_based_year(d(2021, 1, 4)), iso_week_of_week_based_year(d(2021, 1, 4))), (2021, 1));
        // 2019-12-30 (Monday) is week 1 of 2020
        assert_eq!((iso_week_based_year(d(2019, 12, 30)), iso_week_of_week_based_year(d(2019, 12, 30))), (2020, 1));
        assert_eq!(weeks_in_week_based_year(2020, WeekDef::ISO), 53);
        assert_eq!(weeks_in_week_based_year(2021, WeekDef::ISO), 52);
    }

    #[test]
    fn sunday_start_weeks() {
        let w = WeekDef::SUNDAY_START;
        // 2021-01-01 is a Friday: week 1 of 2021 (week 1 contains January 1st)
        assert_eq!(week_of_year(d(2021, 1, 1), w), 1);
        assert_eq!(week_of_year(d(2021, 1, 3), w), 2);
        assert_eq!(localized_dow(d(2021, 1, 3), w), 1);
        assert_eq!(week_of_month(d(2021, 1, 31), w), 6);
    }
}
