//! `java.time.format.DateTimeFormatter`: pattern compilation (`ofPattern`), the predefined ISO
//! formatters, formatting and parsing with Java's resolution rules and error messages.
//!
//! A formatter is a list of printer-parsers like `DateTimeFormatterBuilder` builds (numeric
//! values with width and sign rules, reduced two-digit years, fractions, texts, offsets, zone
//! ids, literals and optional sections). Parsing fills a field map that is then resolved the
//! way `java.time.format.Parsed` does it — `YearOfEra` needs an era under `STRICT`, `SMART`
//! clamps an out-of-range day-of-month, `HourOfAmPm` combines with `AmPmOfDay` — and the result
//! converts to whatever type the caller asked for.

use std::rc::Rc;

use super::*;
use super::weeks::WeekDef;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sign {
    Normal,
    Always,
    Never,
    NotNegative,
    ExceedsPad,
}

impl Sign {
    fn name(self) -> &'static str {
        match self {
            Sign::Normal => "NORMAL",
            Sign::Always => "ALWAYS",
            Sign::Never => "NEVER",
            Sign::NotNegative => "NOT_NEGATIVE",
            Sign::ExceedsPad => "EXCEEDS_PAD",
        }
    }
}

/// Numeric fields a formatter can print, beyond [`Field`]: ISO and localized week fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FField {
    Std(Field),
    QuarterOfYear,
    WeekBasedYear,
    WeekOfWeekBasedYear,
    LocalizedDayOfWeek,
    WeekOfMonth,
    /// `IsoFields.WEEK_BASED_YEAR` / `WEEK_OF_WEEK_BASED_YEAR` (`ISO_WEEK_DATE`).
    IsoWeekBasedYear,
    IsoWeekOfWeekBasedYear,
}

impl FField {
    fn name(self) -> String {
        match self {
            FField::Std(f) => f.name().to_string(),
            FField::QuarterOfYear => "QuarterOfYear".into(),
            FField::WeekBasedYear => "Localized(WeekBasedYear,4,10,EXCEEDS_PAD)".into(),
            FField::WeekOfWeekBasedYear => "WeekOfWeekBasedYear".into(),
            FField::LocalizedDayOfWeek => "DayOfWeek".into(),
            FField::WeekOfMonth => "WeekOfMonth".into(),
            FField::IsoWeekBasedYear => "WeekBasedYear".into(),
            FField::IsoWeekOfWeekBasedYear => "WeekOfWeekBasedYear".into(),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) enum Item {
    Lit(String),
    Value { f: FField, min: u8, max: u8, sign: Sign },
    /// `appendValueReduced`: `max_width` digits at most, `base` a year (`date_base`: the
    /// pattern letters' base date `base-01-01`, which is how it prints).
    Reduced { f: Field, width: u8, max_width: u8, base: i64, date_base: bool },
    Fraction { f: Field, min: u8, max: u8, point: bool },
    Text { f: FField, style: u8 },
    Offset { pattern: &'static str, none: String },
    LocalizedOffset { full: bool },
    ZoneId,
    ZoneText { full: bool },
    Instant,
    Optional(Vec<Item>),
    CaseInsensitive,
    CaseSensitive,
    Pad { width: usize, ch: char, item: Box<Item> },
    /// `appendInstant(fractionalDigits)`: -1 prints as many digits as needed, 0–9 exactly that
    /// many (`Instant` is `appendInstant()`, digits in groups of three).
    InstantDigits(i8),
    /// `parseDefaulting(field, value)`: the value of a field the text did not set.
    Default { f: FField, v: i64 },
    /// A formatter appended as a whole (`DateTimeFormatterBuilder.append`): prints in
    /// parentheses in `toString`.
    Group(Vec<Item>),
    /// `appendChronologyId()` / `appendChronologyText(style)`: always the ISO chronology.
    Chrono { text: bool },
    /// `parseLenient()` / `parseStrict()`: number parsing mode (no effect on output).
    ParseMode { strict: bool },
    /// `appendLocalized(dateStyle, timeStyle)` (`FormatStyle` ordinals): the pattern comes from
    /// the formatter's locale when it formats or parses.
    Localized { date: Option<u8>, time: Option<u8> },
    /// `appendDayPeriodText`: the locale's day period ("in the morning", "noon") by the
    /// minute of the day (`TextStyle` ordinal).
    DayPeriod { style: u8 },
}

const FORMAT_STYLES: [&str; 4] = ["FULL", "LONG", "MEDIUM", "SHORT"];

/// The items of a localized style in `locale` (compiled once per locale and style).
type LocalizedCache = std::collections::HashMap<(String, Option<u8>, Option<u8>), Rc<Vec<Item>>>;

fn localized_items(locale: &str, date: Option<u8>, time: Option<u8>) -> Rc<Vec<Item>> {
    thread_local! {
        static CACHE: std::cell::RefCell<LocalizedCache> = std::cell::RefCell::new(std::collections::HashMap::new());
    }
    CACHE.with(|c| {
        let key = (locale.to_string(), date, time);
        if let Some(v) = c.borrow().get(&key) {
            return v.clone();
        }
        let pattern = super::locale::localized_pattern(locale, date, time);
        let items = Rc::new(compile_pattern(pattern).unwrap_or_default());
        c.borrow_mut().insert(key, items.clone());
        items
    })
}

/// A compiled `DateTimeFormatter`.
#[derive(Debug, Clone)]
pub struct Formatter {
    items: Vec<Item>,
    /// `ResolverStyle` ordinal: 0 STRICT, 1 SMART, 2 LENIENT.
    resolver: u8,
    zone: Option<Zone>,
    locale: Rc<str>,
    /// Name of a predefined constant (`ISO_LOCAL_DATE`) for `toString`.
    name: Option<&'static str>,
}

pub fn formatter_value(f: Formatter) -> Value {
    Value::obj(Obj::Formatter(Rc::new(f)))
}

pub fn formatter_of(v: &Value) -> Result<Rc<Formatter>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Formatter(f) => Ok(f.clone()),
            _ => Err(crate::vm::cce("not a DateTimeFormatter")),
        },
        _ => Err(npe("formatter")),
    }
}

fn too_many(c: char) -> Thrown {
    jt(
        Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(format!("Too many pattern letters: {c}"))),
        &["format.DateTimeFormatterBuilder.parseField(DateTimeFormatterBuilder.java:2158)", "format.DateTimeFormatterBuilder.parsePattern(DateTimeFormatterBuilder.java:1942)"],
    )
}

fn pattern_error(msg: String) -> Thrown {
    Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(msg))
}

/// `DateTimeFormatterBuilder.appendPattern`.
pub(super) fn compile_pattern(p: &str) -> Result<Vec<Item>, Thrown> {
    let mut stack: Vec<Vec<Item>> = vec![Vec::new()];
    compile_pattern_into(p, &mut stack)?;
    while stack.len() > 1 {
        let inner = stack.pop().unwrap();
        stack.last_mut().unwrap().push(Item::Optional(inner));
    }
    Ok(stack.pop().unwrap())
}

/// `appendPattern` into a builder's open sections: `[` opens one that may stay open, `]`
/// closes the innermost open one (even one opened by an earlier call).
pub(super) fn compile_pattern_into(p: &str, stack: &mut Vec<Vec<Item>>) -> Result<(), Thrown> {
    let chars: Vec<char> = p.chars().collect();
    let mut i = 0;
    let mut pad_next: Option<usize> = None;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_alphabetic() {
            let start = i;
            while i < chars.len() && chars[i] == c {
                i += 1;
            }
            let mut count = i - start;
            if c == 'p' {
                let mut pad = 0;
                if i < chars.len() && chars[i].is_ascii_alphabetic() {
                    pad = count;
                    let c2 = chars[i];
                    let s2 = i;
                    while i < chars.len() && chars[i] == c2 {
                        i += 1;
                    }
                    count = i - s2;
                    let item = letter(c2, count)?;
                    stack.last_mut().unwrap().push(Item::Pad { width: pad, ch: ' ', item: Box::new(item) });
                    continue;
                }
                if pad == 0 {
                    pad_next = Some(count);
                }
                continue;
            }
            let item = letter(c, count)?;
            let item = match pad_next.take() {
                Some(w) => Item::Pad { width: w, ch: ' ', item: Box::new(item) },
                None => item,
            };
            stack.last_mut().unwrap().push(item);
            continue;
        }
        match c {
            '\'' => {
                let start = i;
                i += 1;
                let mut lit = String::new();
                loop {
                    if i >= chars.len() {
                        return Err(pattern_error(format!("Pattern ends with an incomplete string literal: {p}")));
                    }
                    if chars[i] == '\'' {
                        if i + 1 < chars.len() && chars[i + 1] == '\'' {
                            lit.push('\'');
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    lit.push(chars[i]);
                    i += 1;
                }
                if i == start + 1 {
                    lit.push('\'');
                }
                i += 1;
                stack.last_mut().unwrap().push(Item::Lit(lit));
            }
            '[' => {
                stack.push(Vec::new());
                i += 1;
            }
            ']' => {
                if stack.len() == 1 {
                    return Err(jt(pattern_error("Pattern invalid as it contains ] without previous [".into()), &["format.DateTimeFormatterBuilder.parsePattern(DateTimeFormatterBuilder.java:2051)"]));
                }
                let inner = stack.pop().unwrap();
                stack.last_mut().unwrap().push(Item::Optional(inner));
                i += 1;
            }
            '{' | '}' | '#' => {
                return Err(jt(pattern_error(format!("Pattern includes reserved character: '{c}'")), &["format.DateTimeFormatterBuilder.parsePattern(DateTimeFormatterBuilder.java:2056)"]))
            }
            _ => {
                stack.last_mut().unwrap().push(Item::Lit(c.to_string()));
                i += 1;
            }
        }
    }
    Ok(())
}

fn letter(c: char, count: usize) -> Result<Item, Thrown> {
    use Field as F;
    let std = FField::Std;
    let value = |f: Field, count: usize| -> Result<Item, Thrown> {
        match count {
            1 => Ok(Item::Value { f: std(f), min: 1, max: 19, sign: Sign::Normal }),
            2 => Ok(Item::Value { f: std(f), min: 2, max: 2, sign: Sign::NotNegative }),
            _ => Err(too_many(c)),
        }
    };
    Ok(match c {
        'G' => match count {
            1..=3 => Item::Text { f: std(F::Era), style: 2 },
            4 => Item::Text { f: std(F::Era), style: 0 },
            5 => Item::Text { f: std(F::Era), style: 4 },
            _ => return Err(too_many(c)),
        },
        'y' | 'u' => {
            let f = if c == 'y' { F::YearOfEra } else { F::Year };
            match count {
                2 => Item::Reduced { f, width: 2, max_width: 2, base: 2000, date_base: true },
                1 | 3 => Item::Value { f: std(f), min: count as u8, max: 19, sign: Sign::Normal },
                _ => Item::Value { f: std(f), min: count.min(19) as u8, max: 19, sign: Sign::ExceedsPad },
            }
        }
        'Y' => match count {
            2 => Item::Value { f: FField::WeekBasedYear, min: 2, max: 2, sign: Sign::NotNegative },
            _ => Item::Value { f: FField::WeekBasedYear, min: count as u8, max: 19, sign: if count < 4 { Sign::Normal } else { Sign::ExceedsPad } },
        },
        'M' | 'L' => match count {
            1 => Item::Value { f: std(F::MonthOfYear), min: 1, max: 19, sign: Sign::Normal },
            2 => Item::Value { f: std(F::MonthOfYear), min: 2, max: 2, sign: Sign::NotNegative },
            3 => Item::Text { f: std(F::MonthOfYear), style: if c == 'L' { 3 } else { 2 } },
            4 => Item::Text { f: std(F::MonthOfYear), style: if c == 'L' { 1 } else { 0 } },
            5 => Item::Text { f: std(F::MonthOfYear), style: 4 },
            _ => return Err(too_many(c)),
        },
        'Q' | 'q' => match count {
            1 => Item::Value { f: FField::QuarterOfYear, min: 1, max: 19, sign: Sign::Normal },
            2 => Item::Value { f: FField::QuarterOfYear, min: 2, max: 2, sign: Sign::NotNegative },
            3 => Item::Text { f: FField::QuarterOfYear, style: if c == 'q' { 3 } else { 2 } },
            4 => Item::Text { f: FField::QuarterOfYear, style: if c == 'q' { 1 } else { 0 } },
            5 => Item::Text { f: FField::QuarterOfYear, style: if c == 'q' { 5 } else { 4 } },
            _ => return Err(too_many(c)),
        },
        'w' => match count {
            1 => Item::Value { f: FField::WeekOfWeekBasedYear, min: 1, max: 19, sign: Sign::Normal },
            2 => Item::Value { f: FField::WeekOfWeekBasedYear, min: 2, max: 2, sign: Sign::NotNegative },
            _ => return Err(too_many(c)),
        },
        'W' => match count {
            1 => Item::Value { f: FField::WeekOfMonth, min: 1, max: 19, sign: Sign::Normal },
            _ => return Err(too_many(c)),
        },
        'E' => match count {
            1..=3 => Item::Text { f: std(F::DayOfWeek), style: 2 },
            4 => Item::Text { f: std(F::DayOfWeek), style: 0 },
            5 => Item::Text { f: std(F::DayOfWeek), style: 4 },
            _ => return Err(too_many(c)),
        },
        'e' | 'c' => match count {
            1 => Item::Value { f: FField::LocalizedDayOfWeek, min: 1, max: 19, sign: Sign::Normal },
            2 if c == 'e' => Item::Value { f: FField::LocalizedDayOfWeek, min: 2, max: 2, sign: Sign::NotNegative },
            3 => Item::Text { f: std(F::DayOfWeek), style: if c == 'c' { 3 } else { 2 } },
            4 => Item::Text { f: std(F::DayOfWeek), style: if c == 'c' { 1 } else { 0 } },
            5 => Item::Text { f: std(F::DayOfWeek), style: 4 },
            _ => return Err(too_many(c)),
        },
        'F' => match count {
            1 => Item::Value { f: std(F::AlignedWeekOfMonth), min: 1, max: 19, sign: Sign::Normal },
            _ => return Err(too_many(c)),
        },
        'a' => match count {
            1 => Item::Text { f: std(F::AmPmOfDay), style: 2 },
            _ => return Err(too_many(c)),
        },
        'B' => match count {
            1 => Item::DayPeriod { style: 2 },
            4 => Item::DayPeriod { style: 0 },
            5 => Item::DayPeriod { style: 4 },
            _ => {
                let mut t = pattern_error(format!("Wrong number of pattern letters: {c}"));
                t.frames.push("java.base/java.time.format.DateTimeFormatterBuilder.parsePattern(DateTimeFormatterBuilder.java:2017)".into());
                return Err(t);
            }
        },
        'd' => value(F::DayOfMonth, count)?,
        'D' => match count {
            1 => Item::Value { f: std(F::DayOfYear), min: 1, max: 19, sign: Sign::Normal },
            2 => Item::Value { f: std(F::DayOfYear), min: 2, max: 3, sign: Sign::NotNegative },
            3 => Item::Value { f: std(F::DayOfYear), min: 3, max: 3, sign: Sign::NotNegative },
            _ => return Err(too_many(c)),
        },
        'h' => value(F::ClockHourOfAmPm, count)?,
        'K' => value(F::HourOfAmPm, count)?,
        'k' => value(F::ClockHourOfDay, count)?,
        'H' => value(F::HourOfDay, count)?,
        'm' => value(F::MinuteOfHour, count)?,
        's' => value(F::SecondOfMinute, count)?,
        'S' => Item::Fraction { f: F::NanoOfSecond, min: count as u8, max: count as u8, point: false },
        'A' => Item::Value { f: std(F::MilliOfDay), min: count as u8, max: 19, sign: Sign::NotNegative },
        'n' => Item::Value { f: std(F::NanoOfSecond), min: count as u8, max: 19, sign: Sign::NotNegative },
        'N' => Item::Value { f: std(F::NanoOfDay), min: count as u8, max: 19, sign: Sign::NotNegative },
        'V' => {
            if count != 2 {
                return Err(pattern_error(format!("Pattern letter count must be 2: {c}")));
            }
            Item::ZoneId
        }
        'v' => match count {
            1 | 4 => Item::ZoneText { full: count == 4 },
            _ => return Err(pattern_error(format!("Wrong number of pattern letters: {c}"))),
        },
        'z' => match count {
            1..=3 => Item::ZoneText { full: false },
            4 => Item::ZoneText { full: true },
            _ => return Err(too_many(c)),
        },
        'O' => match count {
            1 => Item::LocalizedOffset { full: false },
            4 => Item::LocalizedOffset { full: true },
            _ => return Err(pattern_error(format!("Pattern letter count must be 1 or 4: {c}"))),
        },
        'X' => match count {
            1 => Item::Offset { pattern: "+HHmm", none: "Z".into() },
            2 => Item::Offset { pattern: "+HHMM", none: "Z".into() },
            3 => Item::Offset { pattern: "+HH:MM", none: "Z".into() },
            4 => Item::Offset { pattern: "+HHMMss", none: "Z".into() },
            5 => Item::Offset { pattern: "+HH:MM:ss", none: "Z".into() },
            _ => return Err(too_many(c)),
        },
        'x' => match count {
            1 => Item::Offset { pattern: "+HHmm", none: "+00".into() },
            2 => Item::Offset { pattern: "+HHMM", none: "+0000".into() },
            3 => Item::Offset { pattern: "+HH:MM", none: "+00:00".into() },
            4 => Item::Offset { pattern: "+HHMMss", none: "+0000".into() },
            5 => Item::Offset { pattern: "+HH:MM:ss", none: "+00:00".into() },
            _ => return Err(too_many(c)),
        },
        'Z' => match count {
            1..=3 => Item::Offset { pattern: "+HHMM", none: "+0000".into() },
            4 => Item::LocalizedOffset { full: true },
            5 => Item::Offset { pattern: "+HH:MM:ss", none: "Z".into() },
            _ => return Err(too_many(c)),
        },
        _ => return Err(pattern_error(format!("Unknown pattern letter: {c}"))),
    })
}

impl Formatter {
    pub fn of_pattern(p: &str, locale: &str) -> Result<Formatter, Thrown> {
        Ok(Formatter { items: compile_pattern(p)?, resolver: 1, zone: None, locale: Rc::from(locale), name: None })
    }

    /// `ofLocalizedDate` / `ofLocalizedTime` / `ofLocalizedDateTime`: SMART resolving, the
    /// default locale until `withLocale`.
    pub fn localized(date: Option<u8>, time: Option<u8>) -> Formatter {
        Formatter { items: vec![Item::Localized { date, time }], resolver: 1, zone: None, locale: Rc::from(crate::text::DEFAULT_LOCALE), name: None }
    }

    /// `DateTimeFormatterBuilder.toFormatter(locale)`: SMART resolving, no chronology.
    pub(super) fn from_items(items: Vec<Item>, locale: &str) -> Formatter {
        Formatter { items, resolver: 1, zone: None, locale: Rc::from(locale), name: None }
    }

    /// The items of an existing formatter (`DateTimeFormatterBuilder.append`).
    pub(super) fn items(&self) -> &[Item] {
        &self.items
    }

    fn iso(name: &'static str, items: Vec<Item>) -> Formatter {
        Formatter { items, resolver: 0, zone: None, locale: Rc::from("en_US"), name: Some(name) }
    }

    /// How `DateTimePrintContext` shows the temporal in errors: a formatter with the ISO
    /// chronology (the predefined and localized ones) wraps a temporal without a chronology
    /// (`LocalTime`), and the wrapper's `toString` names the chronology.
    fn print_context(&self, t: &TimeVal) -> String {
        let iso = self.name.is_some() || self.items.iter().any(|i| matches!(i, Item::Localized { .. }));
        if iso && matches!(t, TimeVal::LocalTime(_)) {
            format!("{} with chronology ISO", t.to_java_string())
        } else {
            t.to_java_string()
        }
    }

    /// `toString()`.
    pub fn describe(&self) -> String {
        let mut s = String::new();
        describe_items(&self.items, &mut s);
        s
    }

    // ---------------------------------------------------------------- formatting

    pub fn format(&self, t: &TimeVal) -> Result<String, Thrown> {
        let t = match (&self.zone, t) {
            (Some(z), TimeVal::Instant { secs, nanos }) => zoned_of_instant(*secs, *nanos, z.clone()),
            (Some(z), TimeVal::Zoned { .. }) | (Some(z), TimeVal::OffsetDateTime { .. }) => {
                let (s, n) = t.instant().unwrap();
                zoned_of_instant(s, n, z.clone())
            }
            _ => t.clone(),
        };
        let mut out = String::new();
        self.print_items(&self.items, &t, &mut out, false)?;
        Ok(out)
    }

    fn print_items(&self, items: &[Item], t: &TimeVal, out: &mut String, optional: bool) -> Result<bool, Thrown> {
        let mark = out.len();
        for it in items {
            match self.print(it, t, out) {
                Ok(true) => {}
                Ok(false) | Err(_) if optional => {
                    out.truncate(mark);
                    return Ok(false);
                }
                Ok(false) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(true)
    }

    fn print(&self, it: &Item, t: &TimeVal, out: &mut String) -> Result<bool, Thrown> {
        match it {
            Item::Lit(s) => out.push_str(s),
            Item::Localized { date, time } => {
                let items = localized_items(&self.locale, *date, *time);
                return self.print_items(&items, t, out, false);
            }
            Item::DayPeriod { style } => {
                let hod = t.get_long(Field::HourOfDay)?.rem_euclid(24);
                let moh = t.get_long(Field::MinuteOfHour)?.rem_euclid(60);
                out.push_str(super::locale::day_period(&self.locale, (hod * 60 + moh) as u32, *style));
            }
            Item::CaseInsensitive | Item::CaseSensitive => {}
            Item::Value { f, min, max, sign } => {
                let v = get_ffield(t, *f, WeekDef::of_locale(&self.locale))?;
                print_value(*f, v, *min, *max, *sign, out)?;
            }
            Item::Reduced { f, width, max_width, base, .. } => {
                // `ReducedPrinterParser.getValue`: values in [base, base + 10^width) print their
                // last `width` digits, others their last `max_width` digits
                let v = t.get_long(*f)?;
                let small = 10i64.pow(*width as u32);
                let r = if v >= *base && v < base + small { v.abs() % small } else { v.abs() % 10i64.pow(*max_width as u32) };
                out.push_str(&format!("{r:0w$}", w = *width as usize));
            }
            Item::Fraction { f, min, max, point } => {
                let v = t.get_long(*f)?;
                let mut digits = format!("{v:09}");
                digits.truncate(*max as usize);
                while digits.len() > *min as usize && digits.ends_with('0') {
                    digits.pop();
                }
                if !digits.is_empty() {
                    if *point {
                        out.push('.');
                    }
                    out.push_str(&digits);
                }
            }
            Item::Text { f, style } => {
                let v = get_ffield(t, *f, WeekDef::of_locale(&self.locale))?;
                out.push_str(&locale_text(&self.locale, *f, v, *style));
            }
            Item::Offset { pattern, none } => {
                let o = t.get_long(Field::OffsetSeconds)? as i32;
                out.push_str(&format_offset(o, pattern, none));
            }
            Item::LocalizedOffset { full } => {
                let o = t.get_long(Field::OffsetSeconds)? as i32;
                out.push_str(&localized_offset(o, *full));
            }
            Item::ZoneId => match t {
                TimeVal::Zoned { zone, .. } => out.push_str(&zone.id()),
                TimeVal::OffsetDateTime { offset, .. } | TimeVal::OffsetTime { offset, .. } => out.push_str(&offset_id(*offset)),
                TimeVal::ZoneId(z) => out.push_str(&z.id()),
                _ => return Err(dte(format!("Unable to extract ZoneId from temporal {}", self.print_context(t)))),
            },
            Item::ZoneText { full } => match t {
                TimeVal::Zoned { zone, offset, .. } => out.push_str(&zone_text(zone, *offset, *full, t.instant().map_or(0, |i| i.0), &self.locale)),
                _ => return Err(dte(format!("Unable to extract ZoneId from temporal {}", self.print_context(t)))),
            },
            Item::Instant => {
                let (s, n) = t.instant().ok_or_else(|| unsupported_field(Field::InstantSeconds))?;
                out.push_str(&TimeVal::Instant { secs: s, nanos: n }.to_java_string());
            }
            Item::Optional(inner) => {
                self.print_items(inner, t, out, true)?;
            }
            Item::Pad { width, ch, item } => {
                let mut tmp = String::new();
                self.print(item, t, &mut tmp)?;
                let n = tmp.chars().count();
                if n > *width {
                    return Err(dte(format!("Cannot print as output of {n} characters exceeds pad width of {width}")));
                }
                for _ in n..*width {
                    out.push(*ch);
                }
                out.push_str(&tmp);
            }
            Item::InstantDigits(digits) => {
                let (s, n) = t.instant().ok_or_else(|| unsupported_field(Field::InstantSeconds))?;
                let full = TimeVal::Instant { secs: s, nanos: 0 }.to_java_string();
                let base = full.trim_end_matches('Z');
                out.push_str(base);
                match *digits {
                    -1 => {
                        if n != 0 {
                            let f = format!(".{n:09}");
                            out.push_str(f.trim_end_matches('0'));
                        }
                    }
                    d if d > 0 => {
                        let f = format!("{n:09}");
                        out.push('.');
                        out.push_str(&f[..d as usize]);
                    }
                    _ => {}
                }
                out.push('Z');
            }
            Item::Default { .. } | Item::ParseMode { .. } => {}
            Item::Group(inner) => {
                return self.print_items(inner, t, out, false);
            }
            Item::Chrono { .. } => out.push_str("ISO"),
        }
        Ok(true)
    }

    // ---------------------------------------------------------------- parsing

    /// Parses into a resolved temporal of the requested class.
    pub fn parse_to(&self, text: &str, target: ClassId) -> Result<TimeVal, Thrown> {
        let parsed = self.parse_resolved(text)?;
        let v = parsed.to_value();
        convert_desc(target, &v, &parsed.describe()).map_err(|e| {
            Throw::new(cls::DATE_TIME_PARSE_EXCEPTION, Some(format!("Text '{text}' could not be parsed: {}", e.message.clone().unwrap_or_default()))).with_cause(&e)
        })
    }

    pub fn parse_resolved(&self, text: &str) -> Result<Parsed, Thrown> {
        let chars: Vec<char> = text.chars().collect();
        let mut st = PState { fields: Vec::new(), offset: None, zone: None, insensitive: false, instant: None };
        let pos = match self.parse_items(&self.items, &chars, 0, &mut st) {
            Some(p) => p,
            None => {
                let at = self.error_index(&chars);
                return Err(parse_error(text, at, ""));
            }
        };
        if pos < chars.len() {
            return Err(Throw::new(cls::DATE_TIME_PARSE_EXCEPTION, Some(format!("Text '{text}' could not be parsed, unparsed text found at index {pos}"))));
        }
        self.resolve(st, text)
    }

    fn error_index(&self, chars: &[char]) -> usize {
        // the position of the first item that failed: replay items one by one
        let mut st = PState { fields: Vec::new(), offset: None, zone: None, insensitive: false, instant: None };
        let mut pos = 0;
        for it in &self.items {
            match self.parse_item(it, chars, pos, &mut st) {
                Some(p) => pos = p,
                None => return pos,
            }
        }
        pos
    }

    fn parse_items(&self, items: &[Item], s: &[char], mut pos: usize, st: &mut PState) -> Option<usize> {
        for it in items {
            pos = self.parse_item(it, s, pos, st)?;
        }
        Some(pos)
    }

    fn parse_item(&self, it: &Item, s: &[char], pos: usize, st: &mut PState) -> Option<usize> {
        match it {
            Item::Localized { date, time } => {
                let items = localized_items(&self.locale, *date, *time);
                self.parse_items(&items, s, pos, st)
            }
            // the longest day period name of the style; the period itself is not resolved
            Item::DayPeriod { style } => {
                let mut best = 0;
                for name in super::locale::day_period_names(&self.locale, *style) {
                    let n = name.chars().count();
                    if n > best && pos + n <= s.len() && (if st.insensitive { chars_eq_ignore_case(&s[pos..pos + n], name) } else { s[pos..pos + n].iter().copied().eq(name.chars()) }) {
                        best = n;
                    }
                }
                if best == 0 { None } else { Some(pos + best) }
            }
            Item::Lit(l) => {
                let mut p = pos;
                for c in l.chars() {
                    let got = *s.get(p)?;
                    let ok = if st.insensitive { got.eq_ignore_ascii_case(&c) } else { got == c };
                    if !ok {
                        return None;
                    }
                    p += 1;
                }
                Some(p)
            }
            Item::CaseInsensitive => {
                st.insensitive = true;
                Some(pos)
            }
            Item::CaseSensitive => {
                st.insensitive = false;
                Some(pos)
            }
            Item::Value { f, min, max, sign } => {
                let mut p = pos;
                let mut neg = false;
                match s.get(p) {
                    Some('+') => {
                        if *sign == Sign::Never || *sign == Sign::NotNegative && *min == *max {
                            return None;
                        }
                        p += 1;
                    }
                    Some('-') => {
                        if matches!(sign, Sign::Never | Sign::NotNegative) {
                            return None;
                        }
                        neg = true;
                        p += 1;
                    }
                    _ => {
                        if *sign == Sign::Always {
                            return None;
                        }
                    }
                }
                // adjacent value parsing: a variable-width value followed by a fixed-width
                // one leaves room for it (yyyyMMdd)
                let start = p;
                let maxw = (*max as usize).min(s.len().saturating_sub(start));
                let mut end = start;
                while end < start + maxw && s[end].is_ascii_digit() {
                    end += 1;
                }
                if end - start < *min as usize {
                    return None;
                }
                let digits: String = s[start..end].iter().collect();
                let v: i64 = digits.parse().ok()?;
                let v = if neg { -v } else { v };
                st.set(*f, v);
                Some(end)
            }
            Item::Reduced { f, width, base, .. } => {
                let w = *width as usize;
                if pos + w > s.len() || !s[pos..pos + w].iter().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                let v: i64 = s[pos..pos + w].iter().collect::<String>().parse().ok()?;
                let range = 10i64.pow(*width as u32);
                let base_mod = base % range;
                let mut val = base - base_mod + v;
                if v < base_mod {
                    val += range;
                }
                st.set(FField::Std(*f), val);
                Some(pos + w)
            }
            Item::Fraction { f, min, max, point } => {
                let mut p = pos;
                if *point {
                    if s.get(p) != Some(&'.') {
                        return if *min == 0 { Some(pos) } else { None };
                    }
                    p += 1;
                }
                let start = p;
                while p < s.len() && p - start < *max as usize && s[p].is_ascii_digit() {
                    p += 1;
                }
                if p - start < *min as usize {
                    return None;
                }
                let digits: String = s[start..p].iter().collect();
                let v = if digits.is_empty() { 0 } else { format!("{digits:0<9}").parse::<i64>().ok()? };
                st.set(FField::Std(*f), v);
                Some(p)
            }
            Item::Text { f, style } => {
                let (count, vals): (i64, Vec<(i64, String)>) = text_candidates(&self.locale, *f, *style);
                let _ = count;
                let mut best: Option<(usize, i64)> = None;
                for (v, name) in vals {
                    let n = name.chars().count();
                    if pos + n <= s.len() {
                        let seg = &s[pos..pos + n];
                        let ok = if st.insensitive { chars_eq_ignore_case(seg, &name) } else { seg.iter().copied().eq(name.chars()) };
                        if ok && best.is_none_or(|(bn, _)| n > bn) {
                            best = Some((n, v));
                        }
                    }
                }
                let (n, v) = best?;
                st.set(*f, v);
                Some(pos + n)
            }
            Item::Offset { pattern, none } => {
                let nlen = none.chars().count();
                if pos + nlen <= s.len() && s[pos..pos + nlen].iter().collect::<String>() == *none {
                    st.offset = Some(0);
                    return Some(pos + nlen);
                }
                let (o, p) = parse_offset_text(s, pos, pattern)?;
                st.offset = Some(o);
                Some(p)
            }
            Item::LocalizedOffset { .. } => {
                let pre: String = s.get(pos..pos + 3)?.iter().collect();
                if pre != "GMT" {
                    return None;
                }
                let p = pos + 3;
                if !matches!(s.get(p), Some('+') | Some('-')) {
                    st.offset = Some(0);
                    return Some(p);
                }
                let (o, e) = parse_offset_text(s, p, "+HH:MM:ss").or_else(|| parse_offset_text(s, p, "+H"))?;
                st.offset = Some(o);
                Some(e)
            }
            Item::ZoneId | Item::ZoneText { .. } => {
                let start = pos;
                let mut p = pos;
                while p < s.len() && (s[p].is_ascii_alphanumeric() || matches!(s[p], '/' | '_' | '+' | '-' | ':' | '~' | '.')) {
                    p += 1;
                }
                let mut end = p;
                while end > start {
                    let id: String = s[start..end].iter().collect();
                    if let Ok(z) = zone_of(&id) {
                        st.zone = Some(z);
                        return Some(end);
                    }
                    end -= 1;
                }
                None
            }
            Item::Instant => {
                let rest: String = s[pos..].iter().collect();
                // ISO_INSTANT: up to the zone designator
                let end = rest.find(['Z', 'z']).map(|i| i + 1).unwrap_or(rest.len());
                let (secs, nanos) = parse_instant(&rest[..end]).ok()?;
                st.instant = Some((secs, nanos));
                Some(pos + rest[..end].chars().count())
            }
            Item::Optional(inner) => {
                let mut copy = st.clone();
                match self.parse_items(inner, s, pos, &mut copy) {
                    Some(p) => {
                        *st = copy;
                        Some(p)
                    }
                    None => Some(pos),
                }
            }
            Item::Pad { width, ch, item } => {
                let mut p = pos;
                while p < s.len() && p < pos + width && s[p] == *ch {
                    p += 1;
                }
                self.parse_item(item, s, p, st)
            }
            Item::InstantDigits(_) => self.parse_item(&Item::Instant, s, pos, st),
            Item::Default { f, v } => {
                if !st.fields.iter().any(|(k, _)| k == f) {
                    st.set(*f, *v);
                }
                Some(pos)
            }
            Item::ParseMode { .. } => Some(pos),
            Item::Group(inner) => self.parse_items(inner, s, pos, st),
            Item::Chrono { .. } => {
                let end = pos + 3;
                if end <= s.len() && s[pos..end].iter().collect::<String>() == "ISO" { Some(end) } else { None }
            }
        }
    }

    fn resolve(&self, st: PState, text: &str) -> Result<Parsed, Thrown> {
        let err = |e: Thrown| Throw::new(cls::DATE_TIME_PARSE_EXCEPTION, Some(format!("Text '{text}' could not be parsed: {}", e.message.clone().unwrap_or_default()))).with_cause(&e);
        let mut fields = st.fields.clone();
        let take = |fields: &mut Vec<(FField, i64)>, f: Field| -> Option<i64> {
            let i = fields.iter().position(|(k, _)| *k == FField::Std(f))?;
            Some(fields.remove(i).1)
        };
        let get = |fields: &Vec<(FField, i64)>, f: Field| fields.iter().find(|(k, _)| *k == FField::Std(f)).map(|(_, v)| *v);
        // validate ranges
        for (f, v) in &fields {
            if let FField::Std(f) = f
                && self.resolver != 2 {
                    check(*f, *v).map_err(err)?;
                }
        }
        // year
        if let Some(yoe) = get(&fields, Field::YearOfEra) {
            let era = get(&fields, Field::Era);
            if era.is_some() || self.resolver != 0 || get(&fields, Field::Year).is_some() {
                take(&mut fields, Field::YearOfEra);
                let era = take(&mut fields, Field::Era).unwrap_or(1);
                let y = if era == 1 { yoe } else { 1 - yoe };
                if let Some(existing) = get(&fields, Field::Year) {
                    if existing != y {
                        return Err(err(dte(format!("Conflict found: Year {existing} differs from Year {y} derived from YearOfEra"))));
                    }
                } else {
                    fields.push((FField::Std(Field::Year), y));
                }
            }
        }
        let mut date: Option<Date> = None;
        if let Some(y) = get(&fields, Field::Year) {
            if let (Some(m), Some(d)) = (get(&fields, Field::MonthOfYear), get(&fields, Field::DayOfMonth)) {
                let dd = if self.resolver == 1 && d <= 31 && d > month_len(y, m as u8) as i64 {
                    // SMART: clamp February 29-31 and day 31 of short months
                    if m == 2 || d == 31 {
                        month_len(y, m as u8) as i64
                    } else {
                        d
                    }
                } else {
                    d
                };
                date = Some(Date::of(y, m, dd).map_err(err)?);
                take(&mut fields, Field::Year);
                take(&mut fields, Field::MonthOfYear);
                take(&mut fields, Field::DayOfMonth);
            } else if let Some(doy) = get(&fields, Field::DayOfYear) {
                date = Some(Date::of_year_day(y, doy).map_err(err)?);
                take(&mut fields, Field::Year);
                take(&mut fields, Field::DayOfYear);
            }
        }
        if date.is_none()
            && let Some(ed) = take(&mut fields, Field::EpochDay) {
                date = Some(Date::from_epoch_day(ed));
            }
        if date.is_none() {
            let wby = fields.iter().position(|(k, _)| *k == FField::IsoWeekBasedYear);
            let wow = fields.iter().position(|(k, _)| *k == FField::IsoWeekOfWeekBasedYear);
            if let (Some(a), Some(b)) = (wby, wow) {
                let (y, w) = (fields[a].1, fields[b].1);
                let dow = get(&fields, Field::DayOfWeek).unwrap_or(1);
                let jan4 = Date { y: y as i32, m: 1, d: 4 };
                let start = jan4.epoch_day() - (jan4.day_of_week() as i64 - 1);
                date = Some(Date::from_epoch_day(start + (w - 1) * 7 + (dow - 1)));
                fields.retain(|(k, _)| !matches!(k, FField::IsoWeekBasedYear | FField::IsoWeekOfWeekBasedYear) && *k != FField::Std(Field::DayOfWeek));
            }
        }
        if date.is_none() {
            // localized week date
            let wby = fields.iter().position(|(k, _)| *k == FField::WeekBasedYear);
            let wow = fields.iter().position(|(k, _)| *k == FField::WeekOfWeekBasedYear);
            if let (Some(a), Some(b)) = (wby, wow) {
                let y = fields[a].1;
                let w = fields[b].1;
                // localized day of week: 1 = the locale's first day of the week
                let wd = WeekDef::of_locale(&self.locale);
                let ldow = fields
                    .iter()
                    .find(|(k, _)| *k == FField::LocalizedDayOfWeek)
                    .map(|(_, v)| *v)
                    .or_else(|| get(&fields, Field::DayOfWeek).map(|d| (d - wd.first as i64).rem_euclid(7) + 1))
                    .unwrap_or(1);
                let start = super::weeks::week1_start(y as i32, wd);
                date = Some(Date::from_epoch_day(start + (w - 1) * 7 + (ldow - 1)));
                fields.retain(|(k, _)| !matches!(k, FField::WeekBasedYear | FField::WeekOfWeekBasedYear | FField::LocalizedDayOfWeek) && *k != FField::Std(Field::DayOfWeek));
            }
        }
        if let (Some(d), Some(dow)) = (date, get(&fields, Field::DayOfWeek)) {
            if d.day_of_week() as i64 != dow {
                return Err(err(dte(format!("Conflict found: Field DayOfWeek {} differs from DayOfWeek {dow} derived from {}", d.day_of_week(), format_date(d)))));
            }
            take(&mut fields, Field::DayOfWeek);
        }
        // time
        if let (Some(h), Some(ap)) = (get(&fields, Field::ClockHourOfAmPm), get(&fields, Field::AmPmOfDay)) {
            take(&mut fields, Field::ClockHourOfAmPm);
            take(&mut fields, Field::AmPmOfDay);
            fields.push((FField::Std(Field::HourOfDay), (if h == 12 { 0 } else { h }) + ap * 12));
        }
        if let (Some(h), Some(ap)) = (get(&fields, Field::HourOfAmPm), get(&fields, Field::AmPmOfDay)) {
            take(&mut fields, Field::HourOfAmPm);
            take(&mut fields, Field::AmPmOfDay);
            fields.push((FField::Std(Field::HourOfDay), h + ap * 12));
        }
        if let Some(h) = take(&mut fields, Field::ClockHourOfDay) {
            fields.push((FField::Std(Field::HourOfDay), if h == 24 { 0 } else { h }));
        }
        if let Some(md) = take(&mut fields, Field::MilliOfDay) {
            fields.push((FField::Std(Field::NanoOfDay), md * 1_000_000));
        }
        let mut time: Option<Time> = None;
        let mut excess_days = 0;
        if let Some(nod) = take(&mut fields, Field::NanoOfDay) {
            time = Some(Time(nod));
        } else if let Some(h) = get(&fields, Field::HourOfDay) {
            let m = get(&fields, Field::MinuteOfHour);
            let s = get(&fields, Field::SecondOfMinute);
            let n = get(&fields, Field::NanoOfSecond)
                .or_else(|| get(&fields, Field::MicroOfSecond).map(|x| x * 1000))
                .or_else(|| get(&fields, Field::MilliOfSecond).map(|x| x * 1_000_000));
            if m.is_some() || (s.is_none() && n.is_none()) {
                let (h, m, s, n) = (h, m.unwrap_or(0), s.unwrap_or(0), n.unwrap_or(0));
                if h == 24 && m == 0 && s == 0 && n == 0 && self.resolver != 0 {
                    excess_days = 1;
                    time = Some(Time(0));
                } else {
                    time = Some(Time::of(h, m, s, n).map_err(err)?);
                }
                for f in [Field::HourOfDay, Field::MinuteOfHour, Field::SecondOfMinute, Field::NanoOfSecond, Field::MicroOfSecond, Field::MilliOfSecond] {
                    take(&mut fields, f);
                }
            }
        }
        if excess_days != 0
            && let Some(d) = date {
                date = Some(d.plus_days(excess_days).map_err(err)?);
            }
        let mut zone = st.zone.clone().or_else(|| self.zone.clone());
        if let (Some(o), None) = (st.offset, &zone) {
            let _ = o;
        }
        if let Some((secs, nanos)) = st.instant {
            let z = zone.clone().unwrap_or(Zone::Offset(0));
            let zd = zoned_of_instant(secs, nanos, z);
            if let TimeVal::Zoned { d, t, offset, zone: zz } = zd {
                date = Some(d);
                time = Some(t);
                return Ok(Parsed { date, time, offset: Some(offset), zone: Some(zz), fields, instant: Some((secs, nanos)) });
            }
        }
        if zone.is_none() && st.offset.is_some() {
            zone = None;
        }
        Ok(Parsed { date, time, offset: st.offset, zone, fields, instant: None })
    }
}

#[derive(Clone)]
struct PState {
    fields: Vec<(FField, i64)>,
    offset: Option<i32>,
    zone: Option<Zone>,
    insensitive: bool,
    instant: Option<(i64, i32)>,
}

impl PState {
    fn set(&mut self, f: FField, v: i64) {
        match self.fields.iter_mut().find(|(k, _)| *k == f) {
            Some(slot) => slot.1 = v,
            None => self.fields.push((f, v)),
        }
    }
}

/// The result of `DateTimeFormatter.parse` (`java.time.format.Parsed`).
#[derive(Clone, Debug)]
pub struct Parsed {
    pub date: Option<Date>,
    pub time: Option<Time>,
    pub offset: Option<i32>,
    pub zone: Option<Zone>,
    fields: Vec<(FField, i64)>,
    instant: Option<(i64, i32)>,
}

impl Parsed {
    /// The most specific temporal the parsed fields form.
    pub fn to_value(&self) -> TimeVal {
        match (self.date, self.time, self.offset, &self.zone) {
            (Some(d), Some(t), _, Some(z)) => {
                if let Some((s, n)) = self.instant {
                    zoned_of_instant(s, n, z.clone())
                } else {
                    zoned(d, t, z.clone(), self.offset)
                }
            }
            (Some(d), Some(t), Some(o), None) => TimeVal::OffsetDateTime { d, t, offset: o },
            (Some(d), Some(t), None, None) => TimeVal::LocalDateTime(d, t),
            (Some(d), None, _, _) => TimeVal::LocalDate(d),
            (None, Some(t), Some(o), _) => TimeVal::OffsetTime { t, offset: o },
            (None, Some(t), None, _) => TimeVal::LocalTime(t),
            (None, None, Some(o), _) => TimeVal::ZoneId(Zone::Offset(o)),
            _ => {
                // partial fields: a year-month, month-day or year
                let get = |f: Field| self.fields.iter().find(|(k, _)| *k == FField::Std(f)).map(|(_, v)| *v);
                match (get(Field::Year), get(Field::MonthOfYear), get(Field::DayOfMonth)) {
                    (Some(y), Some(m), None) => TimeVal::YearMonth(y as i32, m as u8),
                    (None, Some(m), Some(d)) => TimeVal::MonthDay(m as u8, d as u8),
                    (Some(y), None, None) => TimeVal::Year(y as i32),
                    _ => TimeVal::Enum(cls::OBJECT, 0),
                }
            }
        }
    }

    /// `Parsed.getLong`: unresolved fields first, then the resolved date, time and zone.
    pub fn get_long(&self, f: Field) -> Result<i64, Thrown> {
        if let Some((_, v)) = self.fields.iter().find(|(k, _)| *k == FField::Std(f)) {
            return Ok(*v);
        }
        if let Some(d) = self.date
            && let Some(v) = date_field(d, f) {
                return Ok(v);
            }
        if let Some(t) = self.time
            && let Some(v) = time_field(t, f) {
                return Ok(v);
            }
        if f == Field::OffsetSeconds
            && let Some(o) = self.offset {
                return Ok(o as i64);
            }
        if f == Field::InstantSeconds
            && let Some((s, _)) = self.instant {
                return Ok(s);
            }
        Err(Throw::new(cls::UNSUPPORTED_TEMPORAL_TYPE_EXCEPTION, Some(format!("Unsupported field: {}", f.name()))))
    }

    /// `Parsed.toString()`: unresolved fields, chronology, and what was resolved.
    pub fn describe(&self) -> String {
        let mut s = String::from("{");
        for (i, (f, v)) in self.fields.iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            s.push_str(&format!("{}={v}", f.name()));
        }
        s.push_str("},ISO");
        if let Some(z) = &self.zone {
            s.push(',');
            s.push_str(&z.id());
        } else if let Some(o) = self.offset {
            s.push(',');
            s.push_str(&offset_id(o));
        }
        if self.date.is_some() || self.time.is_some() {
            s.push_str(" resolved to ");
            match (self.date, self.time) {
                (Some(d), Some(t)) => s.push_str(&format!("{}T{}", format_date(d), format_time(t))),
                (Some(d), None) => s.push_str(&format_date(d)),
                (None, Some(t)) => s.push_str(&format_time(t)),
                _ => {}
            }
        }
        s.push_str(" of type java.time.format.Parsed");
        s
    }
}

/// A printed field's value; localized week fields follow `w`, the formatter locale's weeks.
fn get_ffield(t: &TimeVal, f: FField, w: WeekDef) -> Result<i64, Thrown> {
    match f {
        FField::Std(f) => t.get_long(f),
        FField::QuarterOfYear => Ok((t.get_long(Field::MonthOfYear)? - 1) / 3 + 1),
        FField::WeekBasedYear | FField::WeekOfWeekBasedYear => {
            let d = to_local_date(t).map_err(|_| unsupported(format!("Unsupported field: {}", if f == FField::WeekBasedYear { "WeekBasedYear" } else { "WeekOfWeekBasedYear" })))?;
            Ok(if f == FField::WeekBasedYear { super::weeks::week_based_year(d, w) } else { super::weeks::week_of_week_based_year(d, w) })
        }
        FField::IsoWeekBasedYear | FField::IsoWeekOfWeekBasedYear => {
            let d = to_local_date(t).map_err(|_| unsupported(format!("Unsupported field: {}", f.name())))?;
            let (y, w) = iso_week_of(d);
            Ok(if f == FField::IsoWeekBasedYear { y } else { w })
        }
        FField::LocalizedDayOfWeek => Ok((t.get_long(Field::DayOfWeek)? - w.first as i64).rem_euclid(7) + 1),
        FField::WeekOfMonth => Ok(super::weeks::week_of_month(to_local_date(t)?, w)),
    }
}

/// ISO week-based year and week.
pub fn iso_week_of(d: Date) -> (i64, i64) {
    let ed = d.epoch_day();
    let dow = d.day_of_week() as i64;
    // Thursday of this week decides the week-based year
    let thursday = Date::from_epoch_day(ed - dow + 4);
    let y = thursday.y as i64;
    let jan4 = Date { y: y as i32, m: 1, d: 4 };
    let week1_monday = jan4.epoch_day() - (jan4.day_of_week() as i64 - 1);
    (y, (ed - week1_monday) / 7 + 1)
}

fn print_value(f: FField, v: i64, min: u8, max: u8, sign: Sign, out: &mut String) -> Result<(), Thrown> {
    let abs = v.unsigned_abs().to_string();
    if abs.len() > max as usize {
        return Err(dte(format!("Field {} cannot be printed as the value {v} exceeds the maximum print width of {max}", f.name())));
    }
    if v < 0 {
        match sign {
            Sign::Never | Sign::NotNegative => {
                return Err(dte(format!("Field {} cannot be printed as the value {v} cannot be negative according to the SignStyle", f.name())));
            }
            _ => out.push('-'),
        }
    } else {
        match sign {
            Sign::Always => out.push('+'),
            Sign::ExceedsPad if min < 19 && abs.len() > min as usize => out.push('+'),
            _ => {}
        }
    }
    for _ in abs.len()..min as usize {
        out.push('0');
    }
    out.push_str(&abs);
    Ok(())
}

/// Text of a field in a locale (`DateTimeTextProvider` over the JDK's CLDR data).
pub(crate) fn locale_text(locale: &str, f: FField, v: i64, style: u8) -> String {
    text_of(locale, f, v, style)
}

fn text_of(locale: &str, f: FField, v: i64, style: u8) -> String {
    use super::locale as l;
    match f {
        FField::Std(Field::MonthOfYear) => l::month(locale, v as u8, style).into(),
        FField::Std(Field::DayOfWeek) => l::day(locale, v as u8, style).into(),
        FField::Std(Field::AmPmOfDay) => l::ampm(locale, v as u8).into(),
        FField::Std(Field::Era) => l::era(locale, v as u8, style).into(),
        FField::QuarterOfYear => l::quarter(locale, v as u8, style).into(),
        _ => v.to_string(),
    }
}

fn text_candidates(locale: &str, f: FField, style: u8) -> (i64, Vec<(i64, String)>) {
    let range: Vec<i64> = match f {
        FField::Std(Field::MonthOfYear) => (1..=12).collect(),
        FField::Std(Field::DayOfWeek) => (1..=7).collect(),
        FField::Std(Field::AmPmOfDay) | FField::Std(Field::Era) => (0..=1).collect(),
        FField::QuarterOfYear => (1..=4).collect(),
        _ => Vec::new(),
    };
    let n = range.len() as i64;
    let mut out: Vec<(i64, String)> = range.iter().map(|v| (*v, text_of(locale, f, *v, style))).collect();
    // parsing accepts the other styles too (SMART/LENIENT)
    for alt in [0u8, 2u8] {
        if alt != style {
            for v in &range {
                out.push((*v, text_of(locale, f, *v, alt)));
            }
        }
    }
    (n, out)
}

/// `DateTimeParseContext.subSequenceEquals` without case sensitivity: chars match when equal,
/// or equal after upper-casing, or after lower-casing.
fn chars_eq_ignore_case(a: &[char], b: &str) -> bool {
    let mut it = b.chars();
    for &x in a {
        let Some(y) = it.next() else { return false };
        if x == y {
            continue;
        }
        let (ux, uy) = (x.to_uppercase().next(), y.to_uppercase().next());
        if ux == uy {
            continue;
        }
        let (lx, ly) = (ux.and_then(|c| c.to_lowercase().next()), uy.and_then(|c| c.to_lowercase().next()));
        if lx != ly {
            return false;
        }
    }
    it.next().is_none()
}

/// `OffsetIdPrinterParser.format`.
/// `OffsetIdPrinterParser`'s patterns, in the JDK's order (the index decides the layout).
pub(super) const OFFSET_PATTERNS: [&str; 22] = [
    "+HH", "+HHmm", "+HH:mm", "+HHMM", "+HH:MM", "+HHMMss", "+HH:MM:ss", "+HHMMSS", "+HH:MM:SS", "+HHmmss", "+HH:mm:ss",
    "+H", "+Hmm", "+H:mm", "+HMM", "+H:MM", "+HMMss", "+H:MM:ss", "+HMMSS", "+H:MM:SS", "+Hmmss", "+H:mm:ss",
];

/// `OffsetIdPrinterParser.format`: optional (`mm`, `ss`) parts print only when non-zero, and
/// a zero offset prints the no-offset text.
fn format_offset(total: i32, pattern: &str, none: &str) -> String {
    if total == 0 {
        return none.to_string();
    }
    let index = OFFSET_PATTERNS.iter().position(|p| *p == pattern).unwrap_or(4);
    let ty = index % 11;
    let padded = index < 11;
    let colon = OFFSET_PATTERNS[ty].contains(':');
    let abs = total.unsigned_abs();
    let (h, m, sec) = (abs / 3600, (abs / 60) % 60, abs % 60);
    let mut out = String::from(if total < 0 { "-" } else { "+" });
    if padded || h >= 10 {
        out.push_str(&format!("{h:02}"));
    } else {
        out.push_str(&h.to_string());
    }
    let mut output = h;
    if (3..=8).contains(&ty) || (ty >= 9 && sec > 0) || (ty >= 1 && m > 0) {
        if colon {
            out.push(':');
        }
        out.push_str(&format!("{m:02}"));
        output += m;
        if ty == 7 || ty == 8 || (ty >= 5 && sec > 0) {
            if colon {
                out.push(':');
            }
            out.push_str(&format!("{sec:02}"));
            output += sec;
        }
    }
    if output == 0 {
        return none.to_string();
    }
    out
}

fn parse_offset_text(s: &[char], pos: usize, pattern: &str) -> Option<(i32, usize)> {
    let neg = match s.get(pos)? {
        '+' => false,
        '-' => true,
        _ => return None,
    };
    let mut p = pos + 1;
    let two = |p: usize| -> Option<i32> {
        let a = s.get(p)?.to_digit(10)?;
        let b = s.get(p + 1)?.to_digit(10)?;
        Some((a * 10 + b) as i32)
    };
    let h = if pattern == "+H" {
        let a = s.get(p)?.to_digit(10)? as i32;
        p += 1;
        match s.get(p).and_then(|c| c.to_digit(10)) {
            Some(b) => {
                p += 1;
                a * 10 + b as i32
            }
            None => a,
        }
    } else {
        let h = two(p)?;
        p += 2;
        h
    };
    let colon = pattern.contains(':');
    let mut m = 0;
    let mut sec = 0;
    let try_part = |p: &mut usize| -> Option<i32> {
        let mut q = *p;
        if colon {
            if s.get(q) != Some(&':') {
                return None;
            }
            q += 1;
        }
        let v = two(q)?;
        *p = q + 2;
        Some(v)
    };
    if pattern.len() > 3 {
        match try_part(&mut p) {
            Some(v) => {
                m = v;
                if (pattern.contains("ss") || pattern.contains("SS"))
                    && let Some(v) = try_part(&mut p) {
                        sec = v;
                    }
            }
            None => {
                if !pattern.ends_with("mm") {
                    return None;
                }
            }
        }
    }
    let sign = if neg { -1 } else { 1 };
    let total = offset_hms(sign * h, sign * m, sign * sec).ok()?;
    Some((total, p))
}

fn localized_offset(total: i32, full: bool) -> String {
    if total == 0 {
        return "GMT".into();
    }
    let abs = total.unsigned_abs();
    let (h, m, s) = (abs / 3600, (abs / 60) % 60, abs % 60);
    let sign = if total < 0 { '-' } else { '+' };
    if full {
        let mut out = format!("GMT{sign}{h:02}:{m:02}");
        if s != 0 {
            out.push_str(&format!(":{s:02}"));
        }
        out
    } else {
        let mut out = format!("GMT{sign}{h}");
        if m != 0 || s != 0 {
            out.push_str(&format!(":{m:02}"));
            if s != 0 {
                out.push_str(&format!(":{s:02}"));
            }
        }
        out
    }
}

/// The short display name of a zone at an instant (`%tZ`, pattern `z`).
pub fn zone_short_name(z: &Zone, offset: i32, epoch_secs: i64) -> String {
    zone_text(z, offset, false, epoch_secs, crate::text::DEFAULT_LOCALE)
}

/// Whether daylight saving time is in effect in `tz` at `epoch_secs` (`TimeZone.inDaylightTime`).
fn in_daylight(tz: &chrono_tz::Tz, epoch_secs: i64) -> bool {
    let (total, std) = super::tz_offsets(tz, epoch_secs);
    total != std
}

/// `ZoneTextPrinterParser`: the zone's `TimeZone` display name in `locale` (long for `zzzz`,
/// short for `z`), standard or daylight by the instant; fixed offsets print their id.
fn zone_text(z: &Zone, offset: i32, full: bool, epoch_secs: i64, locale: &str) -> String {
    match z {
        Zone::Offset(_) => offset_id(offset),
        Zone::Region(id, tz, _) => {
            let dst = tz.as_ref().is_some_and(|tz| in_daylight(tz, epoch_secs));
            match super::locale::zone_names(locale, id) {
                Some(n) => n[if full { 0 } else { 2 } + dst as usize].to_string(),
                None => id.to_string(),
            }
        }
    }
}

fn describe_items(items: &[Item], s: &mut String) {
    for it in items {
        match it {
            Item::Localized { date, time } => {
                let name = |x: &Option<u8>| x.map(|i| FORMAT_STYLES[i as usize & 3]).unwrap_or("");
                s.push_str(&format!("Localized({},{})", name(date), name(time)));
            }
            Item::DayPeriod { style } => {
                let st = ["FULL", "FULL_STANDALONE", "SHORT", "SHORT_STANDALONE", "NARROW", "NARROW_STANDALONE"][*style as usize];
                s.push_str(&format!("DayPeriod({st})"));
            }
            Item::Lit(l) => s.push_str(&format!("'{}'", l.replace('\'', "''"))),
            // `WeekBasedFieldPrinterParser.toString`: the localized week fields print the
            // pattern letter's count, not the value parser they become
            Item::Value { f: f @ (FField::WeekOfWeekBasedYear | FField::WeekOfMonth | FField::LocalizedDayOfWeek), min, .. } => {
                s.push_str(&format!("Localized({},{min})", f.name()));
            }
            Item::Value { f: FField::WeekBasedYear, min, sign, .. } => match min {
                1 => s.push_str("Localized(WeekBasedYear)"),
                2 => s.push_str("Localized(ReducedValue(WeekBasedYear,2,2,2000-01-01))"),
                _ => s.push_str(&format!("Localized(WeekBasedYear,{min},19,{})", sign.name())),
            },
            Item::Value { f, min, max, sign } => {
                if *min == 1 && *max == 19 && *sign == Sign::Normal {
                    s.push_str(&format!("Value({})", f.name()));
                } else if min == max && *sign == Sign::NotNegative {
                    s.push_str(&format!("Value({},{min})", f.name()));
                } else {
                    s.push_str(&format!("Value({},{min},{max},{})", f.name(), sign.name()));
                }
            }
            Item::Reduced { f, width, max_width, base, date_base } => {
                if *date_base {
                    s.push_str(&format!("ReducedValue({},{width},{max_width},{base}-01-01)", f.name()))
                } else {
                    s.push_str(&format!("ReducedValue({},{width},{max_width},{base})", f.name()))
                }
            }
            Item::Fraction { f, min, max, point } => s.push_str(&format!("Fraction({},{min},{max}{})", f.name(), if *point { ",DecimalPoint" } else { "" })),
            Item::Text { f, style } => {
                let st = ["FULL", "FULL_STANDALONE", "SHORT", "SHORT_STANDALONE", "NARROW", "NARROW_STANDALONE"][*style as usize];
                if *style == 0 {
                    s.push_str(&format!("Text({})", f.name()));
                } else {
                    s.push_str(&format!("Text({},{st})", f.name()));
                }
            }
            Item::Offset { pattern, none } => s.push_str(&format!("Offset({pattern},'{}')", none.replace('\'', "''"))),
            Item::LocalizedOffset { full } => s.push_str(&format!("LocalizedOffset({})", if *full { "FULL" } else { "SHORT" })),
            Item::ZoneId => s.push_str("ZoneId()"),
            Item::ZoneText { full } => s.push_str(&format!("ZoneText({})", if *full { "FULL" } else { "SHORT" })),
            Item::Instant => s.push_str("ParseCaseSensitive(false)(Value(Year,4,10,EXCEEDS_PAD)'-'Value(MonthOfYear,2)'-'Value(DayOfMonth,2))'T'(Value(HourOfDay,2)':'Value(MinuteOfHour,2)[':'Value(SecondOfMinute,2)[Fraction(NanoOfSecond,0,9,DecimalPoint)]])Offset(+HH:MM:ss,'Z')"),
            Item::Optional(inner) => {
                s.push('[');
                describe_items(inner, s);
                s.push(']');
            }
            Item::CaseInsensitive => s.push_str("ParseCaseSensitive(false)"),
            Item::CaseSensitive => s.push_str("ParseCaseSensitive(true)"),
            Item::Pad { width, ch, item } => {
                s.push_str("Pad(");
                describe_items(std::slice::from_ref(item), s);
                if *ch == ' ' {
                    s.push_str(&format!(",{width})"));
                } else {
                    s.push_str(&format!(",{width},'{ch}')"));
                }
            }
            Item::InstantDigits(_) => s.push_str("Instant()"),
            Item::Group(inner) => {
                s.push('(');
                describe_items(inner, s);
                s.push(')');
            }
            Item::Chrono { text } => s.push_str(if *text { "java.time.format.DateTimeFormatterBuilder$ChronoPrinterParser@5e9f23b4" } else { "java.time.format.DateTimeFormatterBuilder$ChronoPrinterParser@42bfe1c6" }),
            Item::Default { .. } => s.push_str("java.time.format.DateTimeFormatterBuilder$DefaultValueParser@1b6d3586"),
            Item::ParseMode { strict } => s.push_str(&format!("ParseStrict({strict})")),
        }
    }
}

// ------------------------------------------------------------------------------ predefined

fn v(f: Field, min: u8, max: u8, sign: Sign) -> Item {
    Item::Value { f: FField::Std(f), min, max, sign }
}

fn iso_local_date_items() -> Vec<Item> {
    vec![v(Field::Year, 4, 10, Sign::ExceedsPad), Item::Lit("-".into()), v(Field::MonthOfYear, 2, 2, Sign::NotNegative), Item::Lit("-".into()), v(Field::DayOfMonth, 2, 2, Sign::NotNegative)]
}

fn iso_local_time_items() -> Vec<Item> {
    vec![
        v(Field::HourOfDay, 2, 2, Sign::NotNegative),
        Item::Lit(":".into()),
        v(Field::MinuteOfHour, 2, 2, Sign::NotNegative),
        Item::Optional(vec![
            Item::Lit(":".into()),
            v(Field::SecondOfMinute, 2, 2, Sign::NotNegative),
            Item::Optional(vec![Item::Fraction { f: Field::NanoOfSecond, min: 0, max: 9, point: true }]),
        ]),
    ]
}

fn iso_local_date_time_items() -> Vec<Item> {
    let mut items = vec![Item::CaseInsensitive];
    items.extend(iso_local_date_items());
    items.push(Item::Lit("T".into()));
    items.extend(iso_local_time_items());
    items
}

fn offset_item() -> Item {
    Item::Offset { pattern: "+HH:MM:ss", none: "Z".into() }
}

pub fn predefined(name: &str) -> Option<Formatter> {
    let f = match name {
        "ISO_LOCAL_DATE" => Formatter::iso("ISO_LOCAL_DATE", iso_local_date_items()),
        "ISO_LOCAL_TIME" => Formatter::iso("ISO_LOCAL_TIME", iso_local_time_items()),
        "ISO_LOCAL_DATE_TIME" => Formatter::iso("ISO_LOCAL_DATE_TIME", iso_local_date_time_items()),
        "ISO_OFFSET_DATE" => {
            let mut i = vec![Item::CaseInsensitive];
            i.extend(iso_local_date_items());
            i.push(offset_item());
            Formatter::iso("ISO_OFFSET_DATE", i)
        }
        "ISO_OFFSET_TIME" => {
            let mut i = vec![Item::CaseInsensitive];
            i.extend(iso_local_time_items());
            i.push(offset_item());
            Formatter::iso("ISO_OFFSET_TIME", i)
        }
        "ISO_OFFSET_DATE_TIME" => {
            let mut i = iso_local_date_time_items();
            i.push(offset_item());
            Formatter::iso("ISO_OFFSET_DATE_TIME", i)
        }
        "ISO_ZONED_DATE_TIME" => {
            let mut i = iso_local_date_time_items();
            i.push(offset_item());
            i.push(Item::Optional(vec![Item::Lit("[".into()), Item::CaseSensitive, Item::ZoneId, Item::Lit("]".into())]));
            Formatter::iso("ISO_ZONED_DATE_TIME", i)
        }
        "ISO_DATE_TIME" => {
            let mut i = iso_local_date_time_items();
            i.push(Item::Optional(vec![offset_item(), Item::Optional(vec![Item::Lit("[".into()), Item::CaseSensitive, Item::ZoneId, Item::Lit("]".into())])]));
            Formatter::iso("ISO_DATE_TIME", i)
        }
        "ISO_DATE" => {
            let mut i = vec![Item::CaseInsensitive];
            i.extend(iso_local_date_items());
            i.push(Item::Optional(vec![offset_item()]));
            Formatter::iso("ISO_DATE", i)
        }
        "ISO_TIME" => {
            let mut i = vec![Item::CaseInsensitive];
            i.extend(iso_local_time_items());
            i.push(Item::Optional(vec![offset_item()]));
            Formatter::iso("ISO_TIME", i)
        }
        "ISO_INSTANT" => Formatter::iso("ISO_INSTANT", vec![Item::CaseInsensitive, Item::Instant]),
        "ISO_ORDINAL_DATE" => Formatter::iso(
            "ISO_ORDINAL_DATE",
            vec![
                Item::CaseInsensitive,
                v(Field::Year, 4, 10, Sign::ExceedsPad),
                Item::Lit("-".into()),
                v(Field::DayOfYear, 3, 3, Sign::NotNegative),
                Item::Optional(vec![offset_item()]),
            ],
        ),
        "ISO_WEEK_DATE" => Formatter::iso(
            "ISO_WEEK_DATE",
            vec![
                Item::CaseInsensitive,
                Item::Value { f: FField::IsoWeekBasedYear, min: 4, max: 10, sign: Sign::ExceedsPad },
                Item::Lit("-W".into()),
                Item::Value { f: FField::IsoWeekOfWeekBasedYear, min: 2, max: 2, sign: Sign::NotNegative },
                Item::Lit("-".into()),
                v(Field::DayOfWeek, 1, 1, Sign::NotNegative),
                Item::Optional(vec![offset_item()]),
            ],
        ),
        "BASIC_ISO_DATE" => Formatter::iso(
            "BASIC_ISO_DATE",
            vec![
                Item::CaseInsensitive,
                v(Field::Year, 4, 4, Sign::NotNegative),
                v(Field::MonthOfYear, 2, 2, Sign::NotNegative),
                v(Field::DayOfMonth, 2, 2, Sign::NotNegative),
                Item::Optional(vec![Item::Offset { pattern: "+HHMMss", none: "Z".into() }]),
            ],
        ),
        "RFC_1123_DATE_TIME" => Formatter::iso(
            "RFC_1123_DATE_TIME",
            vec![
                Item::CaseInsensitive,
                Item::Optional(vec![Item::Text { f: FField::Std(Field::DayOfWeek), style: 2 }, Item::Lit(", ".into())]),
                v(Field::DayOfMonth, 1, 2, Sign::NotNegative),
                Item::Lit(" ".into()),
                Item::Text { f: FField::Std(Field::MonthOfYear), style: 2 },
                Item::Lit(" ".into()),
                v(Field::Year, 4, 4, Sign::NotNegative),
                Item::Lit(" ".into()),
                v(Field::HourOfDay, 2, 2, Sign::NotNegative),
                Item::Lit(":".into()),
                v(Field::MinuteOfHour, 2, 2, Sign::NotNegative),
                Item::Optional(vec![Item::Lit(":".into()), v(Field::SecondOfMinute, 2, 2, Sign::NotNegative)]),
                Item::Lit(" ".into()),
                Item::Offset { pattern: "+HHMM", none: "GMT".into() },
            ],
        ),
        _ => return None,
    };
    Some(f)
}

// ------------------------------------------------------------------------------ natives

fn fmt_const(name: &'static str) -> VR {
    Ok(formatter_value(predefined(name).unwrap()))
}
macro_rules! consts {
    ($($f:ident = $n:literal),*) => { $( fn $f(_: &mut Vm, _: &mut [Value]) -> VR { fmt_const($n) } )* };
}
consts!(
    c_basic_iso_date = "BASIC_ISO_DATE",
    c_iso_date = "ISO_DATE",
    c_iso_date_time = "ISO_DATE_TIME",
    c_iso_instant = "ISO_INSTANT",
    c_iso_local_date = "ISO_LOCAL_DATE",
    c_iso_local_date_time = "ISO_LOCAL_DATE_TIME",
    c_iso_local_time = "ISO_LOCAL_TIME",
    c_iso_offset_date = "ISO_OFFSET_DATE",
    c_iso_offset_date_time = "ISO_OFFSET_DATE_TIME",
    c_iso_offset_time = "ISO_OFFSET_TIME",
    c_iso_ordinal_date = "ISO_ORDINAL_DATE",
    c_iso_time = "ISO_TIME",
    c_iso_week_date = "ISO_WEEK_DATE",
    c_iso_zoned_date_time = "ISO_ZONED_DATE_TIME",
    c_rfc_1123 = "RFC_1123_DATE_TIME"
);

fn f_of_pattern(_: &mut Vm, a: &mut [Value]) -> VR {
    let p = match &a[0] {
        Value::Null => return Err(npe("pattern")),
        v => v.to_java_string(),
    };
    Ok(formatter_value(Formatter::of_pattern(&p, "en_US").map_err(|t| {
        if t.frames.is_empty() {
            t
        } else {
            jt(t, &["format.DateTimeFormatterBuilder.appendPattern(DateTimeFormatterBuilder.java:1910)", "format.DateTimeFormatter.ofPattern(DateTimeFormatter.java:569)"])
        }
    })?))
}
/// A `FormatStyle` argument's ordinal; null fails `Objects.requireNonNull(x, what)` at
/// `method`'s line of DateTimeFormatter.java.
fn format_style(v: &Value, what: &str, method: &str, line: u32) -> Result<u8, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Time(TimeVal::Enum(c, ord)) if *c == cls::FORMAT_STYLE => Ok(*ord),
            _ => Err(crate::vm::cce("not a FormatStyle")),
        },
        _ => {
            let mut t = npe(what);
            t.frames = vec![
                "java.base/java.util.Objects.requireNonNull(Objects.java:246)".into(),
                format!("java.base/java.time.format.DateTimeFormatter.{method}(DateTimeFormatter.java:{line})"),
            ];
            Err(t)
        }
    }
}
fn f_of_localized_date(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(formatter_value(Formatter::localized(Some(format_style(&a[0], "dateStyle", "ofLocalizedDate", 620)?), None)))
}
fn f_of_localized_time(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(formatter_value(Formatter::localized(None, Some(format_style(&a[0], "timeStyle", "ofLocalizedTime", 651)?))))
}
fn f_of_localized_date_time(_: &mut Vm, a: &mut [Value]) -> VR {
    let st = format_style(&a[0], "dateTimeStyle", "ofLocalizedDateTime", 682)?;
    Ok(formatter_value(Formatter::localized(Some(st), Some(st))))
}
fn f_of_localized_date_time2(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = format_style(&a[0], "dateStyle", "ofLocalizedDateTime", 714)?;
    let t = format_style(&a[1], "timeStyle", "ofLocalizedDateTime", 715)?;
    Ok(formatter_value(Formatter::localized(Some(d), Some(t))))
}
fn f_of_pattern_locale(_: &mut Vm, a: &mut [Value]) -> VR {
    let p = a[0].to_java_string();
    let loc = a[1].to_java_string();
    Ok(formatter_value(Formatter::of_pattern(&p, &loc)?))
}
fn f_format(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = formatter_of(&a[0])?;
    let t = arg_tv(&a[1], "temporal")?;
    let time = matches!(t, TimeVal::LocalTime(_));
    let iso = f.name.is_some();
    Ok(Value::string(f.format(t).map_err(|e| {
        if time && e.class == cls::UNSUPPORTED_TEMPORAL_TYPE_EXCEPTION {
            let mut fr = vec!["LocalTime.get0(LocalTime.java:712)", "LocalTime.getLong(LocalTime.java:690)"];
            if iso {
                fr.push("format.DateTimePrintContext$1.getLong(DateTimePrintContext.java:282)");
            }
            fr.extend([
                "format.DateTimePrintContext.getValue(DateTimePrintContext.java:373)",
                "format.DateTimeFormatterBuilder$NumberPrinterParser.format(DateTimeFormatterBuilder.java:2920)",
                "format.DateTimeFormatterBuilder$CompositePrinterParser.format(DateTimeFormatterBuilder.java:2549)",
                "format.DateTimeFormatter.formatTo(DateTimeFormatter.java:1928)",
                "format.DateTimeFormatter.format(DateTimeFormatter.java:1902)",
            ]);
            jt(e, &fr)
        } else {
            e
        }
    })?))
}
fn f_format_to(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = formatter_of(&a[0])?;
    let s = f.format(arg_tv(&a[1], "temporal")?)?;
    crate::dynamic::call_named(vm, &a[2], "append", vec![Value::string(s)])?;
    Ok(Value::Null)
}
fn f_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = formatter_of(&a[0])?;
    let text = text_arg(&a[1])?;
    let p = f.parse_resolved(&text)?;
    Ok(time_value(TimeVal::Parsed(Rc::new(p))))
}
fn f_parse_query(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = formatter_of(&a[0])?;
    let text = text_arg(&a[1])?;
    let p = f.parse_resolved(&text)?;
    let v = time_value(p.to_value());
    let q = a[2].clone();
    crate::func::call(vm, &q, &[v])
}
fn f_get_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = formatter_of(&a[0])?;
    Ok(match &f.zone {
        Some(z) => time_value(TimeVal::ZoneId(z.clone())),
        None => Value::Null,
    })
}
fn f_get_resolver(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = formatter_of(&a[0])?;
    Ok(time_value(TimeVal::Enum(cls::RESOLVER_STYLE, f.resolver)))
}
fn f_get_locale(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = formatter_of(&a[0])?;
    Ok(Value::obj(Obj::Locale(f.locale.clone())))
}
fn f_with_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    let mut f = (*formatter_of(&a[0])?).clone();
    f.zone = if a[1].is_null() { None } else { Some(zone_arg(&a[1])?) };
    Ok(formatter_value(f))
}
fn f_with_resolver(_: &mut Vm, a: &mut [Value]) -> VR {
    let mut f = (*formatter_of(&a[0])?).clone();
    f.resolver = match arg_tv(&a[1], "resolverStyle")? {
        TimeVal::Enum(_, o) => *o,
        _ => 1,
    };
    Ok(formatter_value(f))
}
fn f_with_locale(_: &mut Vm, a: &mut [Value]) -> VR {
    let mut f = (*formatter_of(&a[0])?).clone();
    if let Value::Obj(o) = &a[1]
        && let Obj::Locale(l) = &**o {
            f.locale = l.clone();
        }
    Ok(formatter_value(f))
}
fn f_identity(_: &mut Vm, a: &mut [Value]) -> VR {
    let _ = formatter_of(&a[0])?;
    Ok(a[0].clone())
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.supers(DATE_TIME_FORMATTER, &[OBJECT]);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter BASIC_ISO_DATE", c_basic_iso_date);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_DATE", c_iso_date);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_DATE_TIME", c_iso_date_time);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_INSTANT", c_iso_instant);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_LOCAL_DATE", c_iso_local_date);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_LOCAL_DATE_TIME", c_iso_local_date_time);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_LOCAL_TIME", c_iso_local_time);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_OFFSET_DATE", c_iso_offset_date);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_OFFSET_DATE_TIME", c_iso_offset_date_time);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_OFFSET_TIME", c_iso_offset_time);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_ORDINAL_DATE", c_iso_ordinal_date);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_TIME", c_iso_time);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_WEEK_DATE", c_iso_week_date);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter ISO_ZONED_DATE_TIME", c_iso_zoned_date_time);
    b.sf(DATE_TIME_FORMATTER, "DateTimeFormatter RFC_1123_DATE_TIME", c_rfc_1123);
    b.m(DATE_TIME_FORMATTER, "String format(TemporalAccessor)", f_format);
    b.m(DATE_TIME_FORMATTER, "void formatTo(TemporalAccessor,Appendable)", f_format_to);
    b.m(DATE_TIME_FORMATTER, "Locale getLocale()", f_get_locale);
    b.m(DATE_TIME_FORMATTER, "ResolverStyle getResolverStyle()", f_get_resolver);
    b.m(DATE_TIME_FORMATTER, "ZoneId getZone()", f_get_zone);
    b.s(DATE_TIME_FORMATTER, "DateTimeFormatter ofPattern(String)", f_of_pattern);
    b.s(DATE_TIME_FORMATTER, "DateTimeFormatter ofPattern(String,Locale)", f_of_pattern_locale);
    b.s(DATE_TIME_FORMATTER, "DateTimeFormatter ofLocalizedDate(FormatStyle)", f_of_localized_date);
    b.s(DATE_TIME_FORMATTER, "DateTimeFormatter ofLocalizedTime(FormatStyle)", f_of_localized_time);
    b.s(DATE_TIME_FORMATTER, "DateTimeFormatter ofLocalizedDateTime(FormatStyle)", f_of_localized_date_time);
    b.s(DATE_TIME_FORMATTER, "DateTimeFormatter ofLocalizedDateTime(FormatStyle,FormatStyle)", f_of_localized_date_time2);
    b.m(DATE_TIME_FORMATTER, "TemporalAccessor parse(CharSequence)", f_parse);
    b.m(DATE_TIME_FORMATTER, "def parse(CharSequence,TemporalQuery)", f_parse_query);
    b.m(DATE_TIME_FORMATTER, "DateTimeFormatter withLocale(Locale)", f_with_locale);
    b.m(DATE_TIME_FORMATTER, "DateTimeFormatter withResolverStyle(ResolverStyle)", f_with_resolver);
    b.m(DATE_TIME_FORMATTER, "DateTimeFormatter withZone(ZoneId)", f_with_zone);
    b.m(DATE_TIME_FORMATTER, "DateTimeFormatter withChronology(Chronology)", f_identity);
}

impl Formatter {
    /// `toString()` as Java prints the formatter.
    pub fn java_string(&self) -> String {
        let _ = self.name;
        self.describe()
    }
}
