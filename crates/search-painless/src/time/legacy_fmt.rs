//! `java.text.DateFormat`, `SimpleDateFormat`, `DateFormatSymbols` and `DateFormat.Field`.
//!
//! A pattern compiles as `SimpleDateFormat.compile` does into literal runs and letter fields
//! (with its errors: `Illegal pattern character 'q'`, `Unterminated quote`); formatting reads
//! the fields of the format's own `GregorianCalendar` (shared with `getCalendar()`, so
//! `setTimeZone` and `setLenient` act through it) and the `DateFormatSymbols` texts, with numbers
//! in the locale's digits. Parsing replays `SimpleDateFormat.subParse`: whitespace before a field
//! is skipped, adjacent numeric fields read exactly their pattern width, texts match the longest
//! symbol case-insensitively, two-digit years land in the century starting 80 years ago, and the
//! parsed fields go through the calendar's lenient or strict resolution, so `Feb 30` is March 2nd
//! unless the format is non-lenient.

use std::cell::RefCell;
use std::rc::Rc;

use super::legacy::{self, cal_of, cal_value, date_value, legacy as legacy_obj, Cal, JTimeZone, LegacyObj};
use crate::types::cls;
use crate::value::{Obj, Value};
use crate::vm::{iae, npe, Throw, Thrown, Vm, VR};

/// The letters `SimpleDateFormat` accepts, in `DateFormatSymbols.patternChars` order.
pub const PATTERN_CHARS: &str = "GyMdkHmsSEDFwWahKzZYuXL";

/// The calendar field each pattern letter reads (-1: none of its own).
const CAL_FIELD: [i32; 23] = [0, 1, 2, 5, 11, 11, 12, 13, 14, 7, 6, 8, 3, 4, 9, 10, 10, 15, 15, 17, 7, 15, 2];
/// The `DateFormat.*_FIELD` id of each pattern letter.
const DF_FIELD: [i32; 23] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 17, 1, 9, 17, 2];

#[derive(Clone, Debug, PartialEq)]
enum Item {
    Literal(String),
    Field { tag: usize, count: usize },
}

/// `DateFormatSymbols`.
#[derive(Clone, Debug, PartialEq)]
pub struct Dfs {
    pub locale: Rc<str>,
    pub eras: Vec<String>,
    pub months: Vec<String>,
    pub short_months: Vec<String>,
    pub weekdays: Vec<String>,
    pub short_weekdays: Vec<String>,
    pub ampm: Vec<String>,
    pub local_chars: String,
    pub zone_strings: Option<Vec<Vec<String>>>,
}

impl Dfs {
    pub fn of(locale: &str) -> Dfs {
        use super::locale as l;
        let mut months: Vec<String> = (1..=12).map(|m| l::month(locale, m, 0).to_string()).collect();
        months.push(String::new());
        let mut short_months: Vec<String> = (1..=12).map(|m| l::month(locale, m, 2).to_string()).collect();
        short_months.push(String::new());
        // index 1 is Sunday; the time module numbers Monday 1 … Sunday 7
        let day = |d: i32, style: u8| l::day(locale, if d == 1 { 7 } else { d as u8 - 1 }, style).to_string();
        let weekdays = std::iter::once(String::new()).chain((1..=7).map(|d| day(d, 0))).collect();
        let short_weekdays = std::iter::once(String::new()).chain((1..=7).map(|d| day(d, 2))).collect();
        Dfs {
            locale: Rc::from(locale),
            eras: vec![l::era(locale, 0, 2).to_string(), l::era(locale, 1, 2).to_string()],
            months,
            short_months,
            weekdays,
            short_weekdays,
            ampm: vec![l::ampm(locale, 0).to_string(), l::ampm(locale, 1).to_string()],
            local_chars: "GyMdkHmsSEDFwWahKzZ".to_string(),
            zone_strings: None,
        }
    }

    /// `getZoneStrings()`: id, long and short standard names, long and short daylight names,
    /// long and short generic names, in the JDK's order. Names come from the locale's zone
    /// names where they exist, else from the `en_US` table.
    pub fn zone_strings(&self) -> Vec<Vec<String>> {
        if let Some(z) = &self.zone_strings {
            return z.clone();
        }
        zone_table()
            .iter()
            .map(|row| {
                let mut r: Vec<String> = row.iter().map(|s| s.to_string()).collect();
                if let Some(n) = super::locale::zone_names(&self.locale, row[0]) {
                    r[1] = n[0].to_string();
                    r[2] = n[2].to_string();
                    r[3] = n[1].to_string();
                    r[4] = n[3].to_string();
                }
                r
            })
            .collect()
    }
}

/// The rows of `zonestrings.tsv`.
fn zone_table() -> &'static Vec<Vec<&'static str>> {
    static T: std::sync::OnceLock<Vec<Vec<&'static str>>> = std::sync::OnceLock::new();
    T.get_or_init(|| include_str!("zonestrings.tsv").lines().filter(|l| !l.starts_with('#') && !l.is_empty()).map(|l| l.split('|').collect()).collect())
}

/// A `SimpleDateFormat`.
#[derive(Clone)]
pub struct DateFmt {
    pub pattern: String,
    items: Vec<Item>,
    pub locale: Rc<str>,
    /// The format's `GregorianCalendar` (a script value, shared with `getCalendar()`).
    pub cal: Value,
    pub sym: Dfs,
    pub century_start: i64,
    century_start_year: i32,
    zero: char,
    minus: char,
    /// `getNumberFormat()` once requested or set.
    pub number_format: Option<Value>,
}

fn pattern_error((msg, inner): (String, &[&str]), outer: &[&str]) -> Thrown {
    let mut t = iae(msg);
    t.frames = inner.iter().chain(outer).map(|s| s.to_string()).collect();
    t
}

/// The frames above `compile` when a constructor compiles the pattern.
const COMPILE_FRAMES: [&str; 2] = ["java.base/java.text.SimpleDateFormat.initialize(SimpleDateFormat.java:662)", "java.base/java.text.SimpleDateFormat.<init>(SimpleDateFormat.java:633)"];

/// `SimpleDateFormat.compile`.
fn compile(pattern: &str) -> Result<Vec<Item>, (String, &'static [&'static str])> {
    compile_inner(pattern).map_err(|m| {
        let frames: &'static [&'static str] = if m == "Unterminated quote" {
            &["java.base/java.text.SimpleDateFormat.compile(SimpleDateFormat.java:870)"]
        } else if m.starts_with("invalid ISO") {
            &["java.base/java.text.SimpleDateFormat.encode(SimpleDateFormat.java:893)", "java.base/java.text.SimpleDateFormat.compile(SimpleDateFormat.java:874)"]
        } else {
            &["java.base/java.text.SimpleDateFormat.compile(SimpleDateFormat.java:854)"]
        };
        (m, frames)
    })
}

fn compile_inner(pattern: &str) -> Result<Vec<Item>, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut items = Vec::new();
    let mut in_quote = false;
    let mut quoted = String::new();
    let mut last: Option<usize> = None;
    let mut count = 0;
    let flush = |items: &mut Vec<Item>, last: &mut Option<usize>, count: &mut usize| -> Result<(), String> {
        if let Some(tag) = last.take() {
            if tag == 21 && *count >= 4 {
                return Err(format!("invalid ISO 8601 format: length={count}"));
            }
            items.push(Item::Field { tag, count: *count });
        }
        *count = 0;
        Ok(())
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            if i + 1 < chars.len() && chars[i + 1] == '\'' {
                i += 2;
                flush(&mut items, &mut last, &mut count)?;
                if in_quote {
                    quoted.push('\'');
                } else {
                    items.push(Item::Literal("'".into()));
                }
                continue;
            }
            if !in_quote {
                flush(&mut items, &mut last, &mut count)?;
                quoted.clear();
                in_quote = true;
            } else {
                items.push(Item::Literal(std::mem::take(&mut quoted)));
                in_quote = false;
            }
            i += 1;
            continue;
        }
        if in_quote {
            quoted.push(c);
            i += 1;
            continue;
        }
        if !c.is_ascii_alphabetic() {
            flush(&mut items, &mut last, &mut count)?;
            if (c as u32) < 128 {
                items.push(Item::Literal(c.to_string()));
                i += 1;
            } else {
                let mut j = i + 1;
                while j < chars.len() && chars[j] != '\'' && !chars[j].is_ascii_alphabetic() {
                    j += 1;
                }
                items.push(Item::Literal(chars[i..j].iter().collect()));
                i = j;
            }
            continue;
        }
        let Some(tag) = PATTERN_CHARS.find(c) else {
            return Err(format!("Illegal pattern character '{c}'"));
        };
        if last.is_none() || last == Some(tag) {
            last = Some(tag);
            count += 1;
        } else {
            flush(&mut items, &mut last, &mut count)?;
            last = Some(tag);
            count = 1;
        }
        i += 1;
    }
    if in_quote {
        return Err("Unterminated quote".into());
    }
    flush(&mut items, &mut last, &mut count)?;
    Ok(items)
}

/// Whether a parse field followed by `tag` must read exactly its width (`shouldObeyCount`).
fn obey_count(tag: usize, count: usize) -> bool {
    match tag {
        2 | 22 => count <= 2,
        1 | 3 | 4 | 5 | 6 | 7 | 8 | 10 | 11 | 12 | 13 | 15 | 16 | 19 | 20 => true,
        _ => false,
    }
}

impl DateFmt {
    pub fn new(pattern: &str, locale: &str, frames: &[&str]) -> Result<DateFmt, Thrown> {
        let items = compile(pattern).map_err(|e| pattern_error(e, frames))?;
        let cal = Cal::now(JTimeZone::gmt(), locale);
        let mut start = cal.clone();
        start.add(legacy::YEAR, -80).ok();
        let century_start = start.time_in_millis().unwrap_or(0);
        let century_start_year = start.get(legacy::YEAR).unwrap_or(1946);
        let sym = crate::text::symbols(locale);
        Ok(DateFmt {
            pattern: pattern.to_string(),
            items,
            locale: Rc::from(locale),
            cal: cal_value(cal),
            sym: Dfs::of(locale),
            century_start,
            century_start_year,
            zero: sym.zero,
            minus: sym.minus,
            number_format: None,
        })
    }

    fn apply_pattern(&mut self, pattern: &str, frames: &[&str]) -> Result<(), Thrown> {
        self.items = compile(pattern).map_err(|e| pattern_error(e, frames))?;
        self.pattern = pattern.to_string();
        Ok(())
    }

    /// Digits in the locale's zero digit, at least `min` and at most `max` of them.
    fn number(&self, out: &mut String, value: i64, min: usize, max: usize) {
        let mut digits = value.unsigned_abs().to_string();
        if digits.len() > max {
            digits = digits[digits.len() - max..].to_string();
        }
        if value < 0 {
            out.push(self.minus);
        }
        for _ in digits.len()..min {
            out.push(self.zero);
        }
        let base = self.zero as u32;
        for d in digits.chars() {
            out.push(char::from_u32(base + (d as u32 - '0' as u32)).unwrap_or(d));
        }
    }

    /// `format(Date)`, recording the span of `want` (a `DateFormat` field id) when asked.
    pub fn format(&self, ms: i64, want: Option<i32>) -> Result<(String, Option<(usize, usize)>), Thrown> {
        if self.cal.is_null() {
            return Err(npe("Cannot invoke \"java.util.Calendar.setTime(java.util.Date)\" because \"this.calendar\" is null").with_frames(&[
                "java.base/java.text.SimpleDateFormat.format(SimpleDateFormat.java:990)",
                "java.base/java.text.SimpleDateFormat.format(SimpleDateFormat.java:983)",
                "java.base/java.text.DateFormat.format(DateFormat.java:353)",
                "java.base/java.text.Format.format(Format.java:165)",
            ]));
        }
        let cal = cal_of(&self.cal)?;
        let mut c = cal.borrow_mut();
        c.set_time_in_millis(ms);
        let f = c.fields;
        let mut out = String::new();
        let mut span = None;
        for item in &self.items {
            match item {
                Item::Literal(s) => out.push_str(s),
                Item::Field { tag, count } => {
                    let begin = crate::java::utf16_len(&out);
                    self.sub_format(&mut out, *tag, *count, &f, &mut c)?;
                    if span.is_none() && want == Some(DF_FIELD[*tag]) {
                        span = Some((begin, crate::java::utf16_len(&out)));
                    }
                }
            }
        }
        Ok((out, span))
    }

    fn sub_format(&self, out: &mut String, tag: usize, count: usize, f: &[i32; legacy::FIELD_COUNT], c: &mut Cal) -> Result<(), Thrown> {
        use legacy::*;
        let max = usize::MAX;
        match tag {
            // G
            0 => out.push_str(self.sym.eras.get(f[ERA] as usize).map(String::as_str).unwrap_or("")),
            // y, Y
            1 | 19 => {
                let v = if tag == 19 { c.week_year()? } else { f[YEAR] as i64 };
                if count != 2 {
                    self.number(out, v, count, max);
                } else {
                    self.number(out, v, 2, 2);
                }
            }
            // M
            2 => {
                let v = f[MONTH];
                let text = match count {
                    0..=2 => None,
                    3 => self.sym.short_months.get(v as usize),
                    _ => self.sym.months.get(v as usize),
                };
                match text {
                    Some(t) => out.push_str(t),
                    None => self.number(out, v as i64 + 1, count, max),
                }
            }
            // L
            22 => {
                let v = f[MONTH];
                let text = match count {
                    0..=2 => None,
                    3 => legacy::field_text(MONTH, v, 0x8001, &self.locale),
                    _ => legacy::field_text(MONTH, v, 0x8002, &self.locale),
                };
                match text {
                    Some(t) => out.push_str(&t),
                    None => self.number(out, v as i64 + 1, count, max),
                }
            }
            // k: 1–24
            4 => {
                let v = if f[HOUR_OF_DAY] == 0 { 24 } else { f[HOUR_OF_DAY] };
                self.number(out, v as i64, count, max);
            }
            // E
            9 => {
                let v = f[DAY_OF_WEEK] as usize;
                let names = if count >= 4 { &self.sym.weekdays } else { &self.sym.short_weekdays };
                out.push_str(names.get(v).map(String::as_str).unwrap_or(""));
            }
            // u: ISO day of week
            20 => {
                let d = f[DAY_OF_WEEK];
                self.number(out, if d == 1 { 7 } else { d as i64 - 1 }, count, max);
            }
            // a
            14 => out.push_str(self.sym.ampm.get(f[AM_PM] as usize).map(String::as_str).unwrap_or("")),
            // h: 1–12
            15 => {
                let v = if f[HOUR] == 0 { 12 } else { f[HOUR] };
                self.number(out, v as i64, count, max);
            }
            // z
            17 => {
                let daylight = f[DST_OFFSET] != 0;
                let name = match &self.sym.zone_strings {
                    Some(zs) => zs.iter().find(|z| z[0] == c.zone.id).map(|z| z[if count < 4 { if daylight { 4 } else { 2 } } else if daylight { 3 } else { 1 }].clone()),
                    None => None,
                };
                out.push_str(&name.unwrap_or_else(|| c.zone.display_name(daylight, count >= 4, &self.locale)));
            }
            // Z: RFC 822 (sign, then hours and minutes in four digits)
            18 => {
                let v = (f[ZONE_OFFSET] + f[DST_OFFSET]) / 60_000;
                let num = (v / 60) * 100 + (v % 60);
                if num < 0 {
                    out.push_str(&format!("-{:04}", -num));
                } else {
                    out.push_str(&format!("+{num:04}"));
                }
            }
            // X: ISO 8601
            21 => {
                let mut v = f[ZONE_OFFSET] + f[DST_OFFSET];
                if v == 0 {
                    out.push('Z');
                } else {
                    v /= 60_000;
                    if v >= 0 {
                        out.push('+');
                    } else {
                        out.push('-');
                        v = -v;
                    }
                    out.push_str(&format!("{:02}", v / 60));
                    if count != 1 {
                        if count == 3 {
                            out.push(':');
                        }
                        out.push_str(&format!("{:02}", v % 60));
                    }
                }
            }
            _ => {
                let field = CAL_FIELD[tag] as usize;
                self.number(out, f[field] as i64, count, max);
            }
        }
        Ok(())
    }

    /// `parse(String, ParsePosition)`: the date and the end index, or the error index.
    pub fn parse(&self, text: &str, start: usize) -> Result<(i64, usize), usize> {
        let t: Vec<u16> = text.encode_utf16().collect();
        let mut pos = start;
        let mut ambiguous = false;
        let mut set: Vec<(usize, i32)> = Vec::new();
        let mut zone_change: Option<JTimeZone> = None;
        for (idx, item) in self.items.iter().enumerate() {
            match item {
                Item::Literal(s) => {
                    for u in s.encode_utf16() {
                        if pos >= t.len() || t[pos] != u {
                            return Err(pos);
                        }
                        pos += 1;
                    }
                }
                Item::Field { tag, count } => {
                    let obey = match self.items.get(idx + 1) {
                        Some(Item::Field { tag: nt, count: nc }) => obey_count(*nt, *nc),
                        _ => false,
                    };
                    pos = self.sub_parse(&t, pos, *tag, *count, obey, &mut ambiguous, &mut set, &mut zone_change)?;
                }
            }
        }
        let cal = cal_of(&self.cal).map_err(|_| pos)?;
        if let Some(z) = zone_change {
            let mut c = cal.borrow_mut();
            c.zone = z;
        }
        let establish = |extra_years: i32| -> Result<i64, ()> {
            let mut c = cal.borrow().clone();
            c.clear();
            for &(f, v) in &set {
                let v = if f == legacy::YEAR { v + extra_years } else { v };
                c.set(f, v);
            }
            c.time_in_millis().map_err(|_| ())
        };
        let mut ms = establish(0).map_err(|_| pos)?;
        if ambiguous && ms < self.century_start {
            ms = establish(100).map_err(|_| pos)?;
        }
        Ok((ms, pos))
    }

    #[allow(clippy::too_many_arguments)]
    fn sub_parse(&self, t: &[u16], start: usize, tag: usize, count: usize, obey: bool, ambiguous: &mut bool, set: &mut Vec<(usize, i32)>, zone: &mut Option<JTimeZone>) -> Result<usize, usize> {
        use legacy::*;
        let mut pos = start;
        loop {
            if pos >= t.len() {
                return Err(start);
            }
            if t[pos] != ' ' as u16 && t[pos] != '\t' as u16 {
                break;
            }
            pos += 1;
        }
        let actual = pos;
        fn put(set: &mut Vec<(usize, i32)>, f: usize, v: i32) {
            set.retain(|(x, _)| *x != f);
            set.push((f, v));
        }
        macro_rules! push {
            ($f:expr, $v:expr) => {
                put(set, $f, $v)
            };
        }
        let numeric_first = matches!(tag, 4 | 15 | 1 | 19) || (matches!(tag, 2 | 22) && count <= 2);
        let mut value = 0i64;
        if numeric_first {
            let limit = if obey { start + count } else { t.len() };
            if limit > t.len() {
                return Err(pos);
            }
            match self.parse_number(&t[..limit], pos) {
                Some((v, end)) => {
                    value = v;
                    pos = end;
                }
                None => return Err(pos),
            }
        }
        match tag {
            0 => match match_string(t, start, &self.sym.eras, false) {
                Some((i, end)) => {
                    push!(ERA, i as i32);
                    Ok(end)
                }
                None => Err(pos),
            },
            1 | 19 => {
                let mut v = value;
                if count <= 2 && pos - actual == 2 && is_digit(t[actual]) && is_digit(t[actual + 1]) {
                    let amb = self.century_start_year as i64 % 100;
                    *ambiguous = v == amb;
                    v += (self.century_start_year as i64 / 100) * 100 + if v < amb { 100 } else { 0 };
                }
                push!(YEAR, v as i32);
                Ok(pos)
            }
            2 | 22 if count <= 2 => {
                push!(MONTH, value as i32 - 1);
                Ok(pos)
            }
            2 => {
                if let Some((i, end)) = match_string(t, start, &self.sym.months, false) {
                    push!(MONTH, i as i32);
                    return Ok(end);
                }
                if let Some((i, end)) = match_string(t, start, &self.sym.short_months, false) {
                    push!(MONTH, i as i32);
                    return Ok(end);
                }
                Err(pos)
            }
            22 => {
                let names: Vec<String> = (0..12).map(|m| field_text(MONTH, m, 0x8002, &self.locale).unwrap_or_default()).collect();
                let short: Vec<String> = (0..12).map(|m| field_text(MONTH, m, 0x8001, &self.locale).unwrap_or_default()).collect();
                let mut all = names;
                all.extend(short);
                match match_string(t, start, &all, false) {
                    Some((i, end)) => {
                        push!(MONTH, (i % 12) as i32);
                        Ok(end)
                    }
                    None => Err(pos),
                }
            }
            4 => {
                let lenient = cal_of(&self.cal).map(|c| c.borrow().lenient).unwrap_or(true);
                if !lenient && !(1..=24).contains(&value) {
                    return Err(pos);
                }
                push!(HOUR_OF_DAY, if value == 24 { 0 } else { value as i32 });
                Ok(pos)
            }
            9 => {
                if let Some((i, end)) = match_string(t, start, &self.sym.weekdays, true) {
                    push!(DAY_OF_WEEK, i as i32);
                    return Ok(end);
                }
                if let Some((i, end)) = match_string(t, start, &self.sym.short_weekdays, true) {
                    push!(DAY_OF_WEEK, i as i32);
                    return Ok(end);
                }
                Err(pos)
            }
            14 => match match_string(t, start, &self.sym.ampm, false) {
                Some((i, end)) => {
                    push!(AM_PM, i as i32);
                    Ok(end)
                }
                None => Err(pos),
            },
            15 => {
                let lenient = cal_of(&self.cal).map(|c| c.borrow().lenient).unwrap_or(true);
                if !lenient && !(1..=12).contains(&value) {
                    return Err(pos);
                }
                push!(HOUR, if value == 12 { 0 } else { value as i32 });
                Ok(pos)
            }
            17 | 18 => {
                let c = t[pos];
                let sign = if c == '+' as u16 {
                    1
                } else if c == '-' as u16 {
                    -1
                } else {
                    0
                };
                if sign == 0 {
                    let gmt: Vec<u16> = "GMT".encode_utf16().collect();
                    if (c == 'G' as u16 || c == 'g' as u16) && t.len() - start >= 3 && eq_ignore_case(&t[start..start + 3], &gmt) {
                        let mut p = start + 3;
                        let s2 = match t.get(p) {
                            Some(&x) if x == '+' as u16 => 1,
                            Some(&x) if x == '-' as u16 => -1,
                            _ => 0,
                        };
                        if s2 == 0 {
                            push!(ZONE_OFFSET, 0);
                            push!(DST_OFFSET, 0);
                            return Ok(p);
                        }
                        p += 1;
                        return match numeric_zone(t, p, s2, 0, true) {
                            Ok((off, end)) => {
                                push!(ZONE_OFFSET, off);
                                push!(DST_OFFSET, 0);
                                Ok(end)
                            }
                            Err(e) => Err(e),
                        };
                    }
                    return match self.zone_string(t, pos) {
                        Some((z, index, same, end)) => {
                            let dst = if index >= 3 { z.dst_savings() } else { 0 };
                            if !(same || (index >= 3 && dst == 0)) {
                                set.retain(|(f, _)| *f != ZONE_OFFSET);
                                set.retain(|(f, _)| *f != DST_OFFSET);
                                set.push((DST_OFFSET, dst));
                            }
                            *zone = Some(z);
                            Ok(end)
                        }
                        None => Err(pos),
                    };
                }
                match numeric_zone(t, pos + 1, sign, 0, false) {
                    Ok((off, end)) => {
                        push!(ZONE_OFFSET, off);
                        push!(DST_OFFSET, 0);
                        Ok(end)
                    }
                    Err(e) => Err(e),
                }
            }
            21 => {
                let c = t[pos];
                if c == 'Z' as u16 {
                    push!(ZONE_OFFSET, 0);
                    push!(DST_OFFSET, 0);
                    return Ok(pos + 1);
                }
                let sign = if c == '+' as u16 {
                    1
                } else if c == '-' as u16 {
                    -1
                } else {
                    return Err(pos + 1);
                };
                match numeric_zone(t, pos + 1, sign, count, count == 3) {
                    Ok((off, end)) => {
                        push!(ZONE_OFFSET, off);
                        push!(DST_OFFSET, 0);
                        Ok(end)
                    }
                    Err(e) => Err(e),
                }
            }
            _ => {
                let limit = if obey { start + count } else { t.len() };
                if limit > t.len() {
                    return Err(pos);
                }
                match self.parse_number(&t[..limit], pos) {
                    Some((v, end)) => {
                        let field = if tag == 20 { DAY_OF_WEEK } else { CAL_FIELD[tag] as usize };
                        let v = if tag == 20 { if v == 7 { 1 } else { v + 1 } } else { v };
                        push!(field, v as i32);
                        Ok(end)
                    }
                    None => Err(pos),
                }
            }
        }
    }

    /// The integer `NumberFormat.parse` reads (an optional minus, then decimal digits).
    fn parse_number(&self, t: &[u16], start: usize) -> Option<(i64, usize)> {
        let mut p = start;
        let neg = p < t.len() && (t[p] == '-' as u16 || t[p] == self.minus as u16);
        if neg {
            p += 1;
        }
        let digits_start = p;
        let mut v: i64 = 0;
        while p < t.len() {
            let d = crate::unicode::digit(t[p] as i32, 10);
            if d < 0 {
                break;
            }
            v = v.saturating_mul(10).saturating_add(d as i64);
            p += 1;
        }
        if p == digits_start {
            return None;
        }
        // `Number.intValue()` of the parsed long
        let v = if neg { -v } else { v };
        Some((v as i32 as i64, p))
    }

    /// A zone name or abbreviation (`subParseZoneString`): the format's zone, then the default
    /// zone, then every zone in table order; the first of a zone's four names that matches.
    /// Returns the zone, whether the name was a daylight one, whether standard and daylight
    /// share the name, and the end.
    fn zone_string(&self, t: &[u16], start: usize) -> Option<(JTimeZone, usize, bool, usize)> {
        let cur = cal_of(&self.cal).ok()?.borrow().zone.clone();
        let rows = self.sym.zone_strings();
        let try_row = |row: &[String]| -> Option<usize> {
            (1..=4).find(|&i| {
                let n: Vec<u16> = row.get(i).map(|s| s.encode_utf16().collect()).unwrap_or_default();
                !n.is_empty() && start + n.len() <= t.len() && eq_ignore_case(&t[start..start + n.len()], &n)
            })
        };
        let by_id = |id: &str| rows.iter().find(|r| r[0] == id).and_then(|r| try_row(r).map(|i| (r, i)));
        let (row, i) = by_id(&cur.id).or_else(|| by_id("GMT")).or_else(|| rows.iter().find_map(|r| try_row(r).map(|i| (r, i))))?;
        let same = i <= 2 && row[i].to_lowercase() == row[i + 2].to_lowercase();
        let len = row[i].encode_utf16().count();
        Some((JTimeZone::of(&row[0]), i, same, start + len))
    }
}

fn is_digit(u: u16) -> bool {
    char::from_u32(u as u32).is_some_and(|c| c.is_ascii_digit())
}

fn eq_ignore_case(a: &[u16], b: &[u16]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x == y || {
                let (cx, cy) = (char::from_u32(*x as u32).unwrap_or('\0'), char::from_u32(*y as u32).unwrap_or('\0'));
                cx.to_uppercase().eq(cy.to_uppercase()) || cx.to_lowercase().eq(cy.to_lowercase())
            }
        })
}

/// `matchString`: the longest entry matching at `start`, case-insensitively (day names skip
/// the empty first entry).
fn match_string(t: &[u16], start: usize, data: &[String], skip_first: bool) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for (i, s) in data.iter().enumerate().skip(skip_first as usize) {
        let n: Vec<u16> = s.encode_utf16().collect();
        if n.len() > best.map_or(0, |b| b.1) && start + n.len() <= t.len() && eq_ignore_case(&t[start..start + n.len()], &n) {
            best = Some((i, n.len()));
        }
    }
    best.map(|(i, l)| (i, start + l))
}

/// `subParseNumericZone`: `hh[[:]mm]`; the offset in milliseconds and the end, or the error
/// index.
fn numeric_zone(t: &[u16], start: usize, sign: i32, count: usize, colon: bool) -> Result<(i32, usize), usize> {
    let mut i = start;
    let digit = |u: Option<&u16>| u.and_then(|&x| char::from_u32(x as u32)).and_then(|c| c.to_digit(10));
    let fail = |i: usize| Err(i.saturating_sub(1).max(start.saturating_sub(1)));
    let Some(h1) = digit(t.get(i)) else { return fail(i + 1) };
    i += 1;
    let mut hours = h1 as i32;
    match digit(t.get(i)) {
        Some(d) => {
            hours = hours * 10 + d as i32;
            i += 1;
        }
        None => {
            if count > 0 || !colon {
                return fail(i + 1);
            }
        }
    }
    if hours > 23 {
        return fail(i);
    }
    let mut minutes = 0;
    if count != 1 {
        if colon {
            if t.get(i) != Some(&(':' as u16)) {
                return fail(i + 1);
            }
            i += 1;
        }
        let Some(m1) = digit(t.get(i)) else { return fail(i + 1) };
        i += 1;
        let Some(m2) = digit(t.get(i)) else { return fail(i + 1) };
        i += 1;
        minutes = (m1 * 10 + m2) as i32;
        if minutes > 59 {
            return fail(i);
        }
    }
    Ok(((minutes + hours * 60) * 60_000 * sign, i))
}

// ------------------------------------------------------------------------------ DateFormat.Field

/// `DateFormat.Field` constants: name, `toString` text, calendar field.
pub const DATE_FIELDS: [(&str, &str, i32); 18] = [
    ("ERA", "era", 0),
    ("YEAR", "year", 1),
    ("MONTH", "month", 2),
    ("DAY_OF_MONTH", "day of month", 5),
    ("HOUR_OF_DAY1", "hour of day 1", -1),
    ("HOUR_OF_DAY0", "hour of day", 11),
    ("MINUTE", "minute", 12),
    ("SECOND", "second", 13),
    ("MILLISECOND", "millisecond", 14),
    ("DAY_OF_WEEK", "day of week", 7),
    ("DAY_OF_YEAR", "day of year", 6),
    ("DAY_OF_WEEK_IN_MONTH", "day of week in month", 8),
    ("WEEK_OF_YEAR", "week of year", 3),
    ("WEEK_OF_MONTH", "week of month", 4),
    ("AM_PM", "am pm", 9),
    ("HOUR1", "hour 1", -1),
    ("HOUR0", "hour", 10),
    ("TIME_ZONE", "time zone", -1),
];

// ------------------------------------------------------------------------------ natives

fn fmt_of(v: &Value) -> Result<&RefCell<DateFmt>, Thrown> {
    match legacy_obj(v) {
        Some(LegacyObj::DateFormat(f)) => Ok(f),
        _ => Err(crate::vm::cce("not a DateFormat")),
    }
}

fn dfs_of(v: &Value) -> Result<&RefCell<Dfs>, Thrown> {
    match legacy_obj(v) {
        Some(LegacyObj::Symbols(s)) => Ok(s),
        _ => Err(npe("Cannot invoke \"java.text.DateFormatSymbols.clone()\" because \"newFormatSymbols\" is null").with_frames(&["java.base/java.text.SimpleDateFormat.setDateFormatSymbols(SimpleDateFormat.java:2442)"])),
    }
}

pub fn fmt_value(f: DateFmt) -> Value {
    Value::obj(Obj::Legacy(Box::new(LegacyObj::DateFormat(RefCell::new(f)))))
}

fn dfs_value(s: Dfs) -> Value {
    Value::obj(Obj::Legacy(Box::new(LegacyObj::Symbols(RefCell::new(s)))))
}

fn locale_of(v: &Value) -> Result<Rc<str>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Locale(l) => Ok(l.clone()),
            _ => Err(crate::vm::cce("not a Locale")),
        },
        _ => Err(npe("Cannot invoke \"java.util.Locale.getUnicodeLocaleType(String)\" because \"locale\" is null")),
    }
}

fn str_of<'v>(v: &'v Value, what: &str) -> Result<&'v str, Thrown> {
    match v {
        Value::Str(s) => Ok(s.as_str()),
        Value::Null => Err(npe(format!("Cannot invoke \"String.length()\" because \"{what}\" is null"))),
        _ => Err(crate::vm::cce("not a String")),
    }
}

fn str_array(items: &[String]) -> Value {
    let items: Vec<Value> = items.iter().map(|s| Value::string(s.clone())).collect();
    Value::obj(Obj::Array(crate::value::JArray::from_data(crate::types::Ty::of(cls::STRING), crate::value::ArrayData::Ref(items))))
}

fn str_array_arg(v: &Value) -> Result<Vec<String>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(a) => Ok(a.data.borrow().to_values().iter().map(|x| x.to_java_string()).collect()),
            _ => Err(crate::vm::cce("not a String[]")),
        },
        _ => Err(npe("Cannot read the array length because \"newArray\" is null")),
    }
}

fn sdf_new0(_: &mut Vm, _: &mut [Value]) -> VR {
    let p = super::locale::localized_pattern(crate::text::DEFAULT_LOCALE, Some(3), Some(3));
    Ok(fmt_value(DateFmt::new(p, crate::text::DEFAULT_LOCALE, &COMPILE_FRAMES)?))
}
fn ctor_npe(one_arg: bool) -> Thrown {
    let mut frames = vec!["java.base/java.text.SimpleDateFormat.<init>(SimpleDateFormat.java:626)"];
    if one_arg {
        frames.push("java.base/java.text.SimpleDateFormat.<init>(SimpleDateFormat.java:608)");
    }
    Throw::new(cls::NULL_POINTER_EXCEPTION, None).with_frames(&frames)
}
fn sdf_new1(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::Str(p) = &a[0] else { return Err(ctor_npe(true)) };
    let mut frames = COMPILE_FRAMES.to_vec();
    frames.push("java.base/java.text.SimpleDateFormat.<init>(SimpleDateFormat.java:608)");
    Ok(fmt_value(DateFmt::new(p.as_str(), crate::text::DEFAULT_LOCALE, &frames)?))
}
fn sdf_new2(_: &mut Vm, a: &mut [Value]) -> VR {
    let (Value::Str(p), Value::Obj(_)) = (&a[0], &a[1]) else { return Err(ctor_npe(false)) };
    let l = locale_of(&a[1])?;
    Ok(fmt_value(DateFmt::new(p.as_str(), &l, &COMPILE_FRAMES)?))
}

/// `DateFormat.get*Instance`: a `SimpleDateFormat` with the locale's localized pattern.
/// `caller` is the frame of the public factory (`DateFormat.get` validates for it).
fn df_instance_at(date: Option<i32>, time: Option<i32>, locale: &str, caller: &str) -> VR {
    let at = |line: u32| vec![format!("java.base/java.text.DateFormat.get(DateFormat.java:{line})"), format!("java.base/java.text.DateFormat.{caller}")];
    if let Some(t) = time
        && !(0..=3).contains(&t)
    {
        let f = at(890);
        return Err(iae(format!("Illegal time style {t}")).with_frames(&f.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    if let Some(d) = date
        && !(0..=3).contains(&d)
    {
        let f = at(897);
        return Err(iae(format!("Illegal date style {d}")).with_frames(&f.iter().map(String::as_str).collect::<Vec<_>>()));
    }
    df_instance(date, time, locale)
}

fn df_locale(v: &Value, caller: &str) -> Result<Rc<str>, Thrown> {
    if v.is_null() {
        return Err(npe("Cannot invoke \"Object.hashCode()\" because \"key\" is null").with_frames(&[
            "java.base/java.util.concurrent.ConcurrentHashMap.get(ConcurrentHashMap.java:951)",
            "java.base/sun.util.locale.provider.LocaleProviderAdapter.getAdapter(LocaleProviderAdapter.java:242)",
            "java.base/java.text.DateFormat.get(DateFormat.java:903)",
            &format!("java.base/java.text.DateFormat.{caller}"),
        ]));
    }
    locale_of(v)
}

fn df_instance(date: Option<i32>, time: Option<i32>, locale: &str) -> VR {
    let p = super::locale::localized_pattern(locale, date.map(|d| d as u8), time.map(|t| t as u8));
    Ok(fmt_value(DateFmt::new(p, locale, &COMPILE_FRAMES)?))
}
fn df_get_instance(_: &mut Vm, _: &mut [Value]) -> VR {
    df_instance(Some(3), Some(3), crate::text::DEFAULT_LOCALE)
}
fn df_date0(_: &mut Vm, _: &mut [Value]) -> VR {
    df_instance(Some(2), None, crate::text::DEFAULT_LOCALE)
}
fn df_date1(_: &mut Vm, a: &mut [Value]) -> VR {
    df_instance_at(Some(crate::lib_lang::int(&a[0])), None, crate::text::DEFAULT_LOCALE, "getDateInstance(DateFormat.java:610)")
}
fn df_date2(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = "getDateInstance(DateFormat.java:624)";
    let style = crate::lib_lang::int(&a[0]);
    if (0..=3).contains(&style) {
        let l = df_locale(&a[1], c)?;
        return df_instance(Some(style), None, &l);
    }
    df_instance_at(Some(style), None, crate::text::DEFAULT_LOCALE, c)
}
fn df_time0(_: &mut Vm, _: &mut [Value]) -> VR {
    df_instance(None, Some(2), crate::text::DEFAULT_LOCALE)
}
fn df_time1(_: &mut Vm, a: &mut [Value]) -> VR {
    df_instance_at(None, Some(crate::lib_lang::int(&a[0])), crate::text::DEFAULT_LOCALE, "getTimeInstance(DateFormat.java:564)")
}
fn df_time2(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = "getTimeInstance(DateFormat.java:578)";
    let style = crate::lib_lang::int(&a[0]);
    if (0..=3).contains(&style) {
        let l = df_locale(&a[1], c)?;
        return df_instance(None, Some(style), &l);
    }
    df_instance_at(None, Some(style), crate::text::DEFAULT_LOCALE, c)
}
fn df_dt0(_: &mut Vm, _: &mut [Value]) -> VR {
    df_instance(Some(2), Some(2), crate::text::DEFAULT_LOCALE)
}
fn df_dt2(_: &mut Vm, a: &mut [Value]) -> VR {
    df_instance_at(Some(crate::lib_lang::int(&a[0])), Some(crate::lib_lang::int(&a[1])), crate::text::DEFAULT_LOCALE, "getDateTimeInstance(DateFormat.java:659)")
}
fn df_dt3(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = "getDateTimeInstance(DateFormat.java:673)";
    let (d, t) = (crate::lib_lang::int(&a[0]), crate::lib_lang::int(&a[1]));
    if (0..=3).contains(&d) && (0..=3).contains(&t) {
        let l = df_locale(&a[2], c)?;
        return df_instance(Some(d), Some(t), &l);
    }
    df_instance_at(Some(d), Some(t), crate::text::DEFAULT_LOCALE, c)
}

fn date_arg(v: &Value) -> Result<i64, Thrown> {
    match v {
        Value::Obj(o) => {
            if let Obj::Time(super::TimeVal::UtilDate(ms)) = &**o {
                return Ok(ms.get());
            }
        }
        Value::Null => {
            return Err(iae("Cannot format given Object as a Date").with_frames(&[
                "java.base/java.text.DateFormat.format(DateFormat.java:358)",
                "java.base/java.text.Format.format(Format.java:165)",
            ]))
        }
        _ => {}
    }
    if let Some(n) = v.as_i64().filter(|_| v.as_f64().is_some()) {
        return Ok(n);
    }
    Err(iae("Cannot format given Object as a Date").with_frames(&[
        "java.base/java.text.DateFormat.format(DateFormat.java:358)",
        "java.base/java.text.Format.format(Format.java:165)",
    ]))
}

/// Formats a `Date` (or a `Number` of milliseconds) with `f`.
pub fn format_with(f: &DateFmt, obj: &Value) -> Result<String, Thrown> {
    let ms = date_arg(obj)?;
    Ok(f.format(ms, None)?.0)
}

/// `Format.format(Object)` on a date format (a `Date` or a `Number` of milliseconds).
pub fn format_object(f: &Value, obj: &Value) -> VR {
    let fm = fmt_of(f)?.borrow();
    let ms = date_arg(obj)?;
    Ok(Value::string(fm.format(ms, None)?.0))
}

/// `Format.format(Object, StringBuffer, FieldPosition)` on a date format.
pub fn format_object3(f: &Value, a: &[Value]) -> VR {
    let fm = fmt_of(f)?.borrow();
    let ms = date_arg(&a[1])?;
    let want = crate::text::field_position_field(&a[3]);
    let (s, span) = fm.format(ms, want)?;
    let Value::Obj(o) = &a[2] else {
        return Err(npe("Cannot invoke \"java.lang.StringBuffer.length()\" because \"this.sb\" is null").with_frames(&[
            "java.base/java.text.StringBufFactory$StringBufferImpl.length(StringBufFactory.java:63)",
            "java.base/java.text.SimpleDateFormat.subFormat(SimpleDateFormat.java:1150)",
            "java.base/java.text.SimpleDateFormat.format(SimpleDateFormat.java:1013)",
            "java.base/java.text.SimpleDateFormat.format(SimpleDateFormat.java:976)",
            "java.base/java.text.DateFormat.format(DateFormat.java:341)",
        ]));
    };
    let Obj::Builder { s: buf, .. } = &**o else { return Err(crate::vm::cce("not a StringBuffer")) };
    let base = crate::java::utf16_len(&buf.borrow()) as i32;
    if let Some((b, e)) = span {
        crate::text::set_field_position(&a[3], base + b as i32, base + e as i32);
    }
    buf.borrow_mut().push_str(&s);
    Ok(a[2].clone())
}

fn parse_exception(text: &str, at: usize, frames: &[&str]) -> Thrown {
    let mut t = Throw::new(cls::TEXT_PARSE_EXCEPTION, Some(format!("Unparseable date: \"{text}\"")));
    t.frames = frames.iter().map(|s| s.to_string()).collect();
    t.error_offset = Some(at as i32);
    t
}

fn df_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = match &a[1] {
        Value::Str(s) => s.as_str().to_string(),
        _ => {
            return Err(npe("Cannot invoke \"String.length()\" because \"text\" is null").with_frames(&[
                "java.base/java.text.SimpleDateFormat.parse(SimpleDateFormat.java:1513)",
                "java.base/java.text.DateFormat.parse(DateFormat.java:425)",
            ]))
        }
    };
    let f = fmt_of(&a[0])?.borrow().clone();
    match f.parse(&text, 0) {
        Ok((ms, end)) if end > 0 || text.is_empty() && f.items.is_empty() => Ok(date_value(ms)),
        Ok(_) => Err(parse_exception(&text, 0, &["java.base/java.text.DateFormat.parse(DateFormat.java:427)"])),
        Err(at) => Err(parse_exception(&text, crate::java::utf16_len(&text).min(at), &["java.base/java.text.DateFormat.parse(DateFormat.java:427)"])),
    }
}

/// `parse(String, ParsePosition)`: null on failure with the position's error index set.
fn df_parse_pos(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = fmt_of(&a[0])?.borrow().clone();
    let start = crate::text::parse_position_index(&a[2]).ok_or_else(|| npe("Cannot read field \"index\" because \"pos\" is null").with_frames(&["java.base/java.text.SimpleDateFormat.parse(SimpleDateFormat.java:1511)"]))?;
    let text = match &a[1] {
        Value::Str(s) => s.as_str().to_string(),
        _ => return Err(npe("Cannot invoke \"String.length()\" because \"text\" is null").with_frames(&["java.base/java.text.SimpleDateFormat.parse(SimpleDateFormat.java:1513)"])),
    };
    match f.parse(&text, start.max(0) as usize) {
        Ok((ms, end)) => {
            crate::text::set_parse_position(&a[2], Some(end as i32), None);
            Ok(date_value(ms))
        }
        Err(at) => {
            crate::text::set_parse_position(&a[2], None, Some(at as i32));
            Ok(Value::Null)
        }
    }
}

/// `Format.parseObject(String)` on a date format.
pub fn parse_object(f: &Value, text: &Value) -> VR {
    let text = str_of(text, "source")?.to_string();
    let f = fmt_of(f)?.borrow().clone();
    match f.parse(&text, 0) {
        Ok((ms, end)) if end > 0 => Ok(date_value(ms)),
        Ok(_) => Err(parse_exception_generic(0)),
        Err(at) => Err(parse_exception_generic(at)),
    }
}

/// `Format.parseObject(String, ParsePosition)` on a date format.
pub fn parse_object2(vm: &mut Vm, a: &mut [Value]) -> VR {
    df_parse_pos(vm, a)
}

fn parse_exception_generic(at: usize) -> Thrown {
    let mut t = Throw::new(cls::TEXT_PARSE_EXCEPTION, Some("Format.parseObject(String) failed".into()));
    t.frames = vec!["java.base/java.text.Format.parseObject(Format.java:262)".into()];
    t.error_offset = Some(at as i32);
    t
}

fn df_calendar(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(fmt_of(&a[0])?.borrow().cal.clone())
}
fn df_set_calendar(_: &mut Vm, a: &mut [Value]) -> VR {
    if !a[1].is_null() {
        cal_of(&a[1])?;
    }
    fmt_of(&a[0])?.borrow_mut().cal = a[1].clone();
    Ok(Value::Null)
}
fn df_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = fmt_of(&a[0])?.borrow();
    let z = cal_of(&f.cal)?.borrow().zone.clone();
    Ok(legacy::tz_value(z))
}
fn df_set_zone(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        // the JDK stores the null zone; only later uses of it fail
        return Ok(Value::Null);
    }
    let z = legacy::tz_of(&a[1])?.borrow().clone();
    let f = fmt_of(&a[0])?.borrow();
    let mut c = cal_of(&f.cal)?.borrow_mut();
    c.zone = z;
    c.are_fields_set = false;
    c.are_all_fields_set = false;
    Ok(Value::Null)
}
fn df_lenient(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = fmt_of(&a[0])?.borrow();
    Ok(Value::Bool(cal_of(&f.cal)?.borrow().lenient))
}
fn df_set_lenient(_: &mut Vm, a: &mut [Value]) -> VR {
    let v = crate::lib_lang::boolean(&a[1]);
    let f = fmt_of(&a[0])?.borrow();
    cal_of(&f.cal)?.borrow_mut().lenient = v;
    Ok(Value::Null)
}
fn df_number_format(vm: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(nf) = fmt_of(&a[0])?.borrow().number_format.clone() {
        return Ok(nf);
    }
    let locale = fmt_of(&a[0])?.borrow().locale.clone();
    let nf = crate::text::integer_instance_for_dates(vm, &locale)?;
    fmt_of(&a[0])?.borrow_mut().number_format = Some(nf.clone());
    Ok(nf)
}
fn df_set_number_format(_: &mut Vm, a: &mut [Value]) -> VR {
    fmt_of(&a[0])?.borrow_mut().number_format = Some(a[1].clone());
    Ok(Value::Null)
}
fn sdf_to_pattern(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(fmt_of(&a[0])?.borrow().pattern.clone()))
}
fn sdf_apply(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::Str(p) = &a[1] else {
        return Err(npe("Cannot invoke \"String.length()\" because \"pattern\" is null").with_frames(&[
            "java.base/java.text.SimpleDateFormat.compile(SimpleDateFormat.java:754)",
            "java.base/java.text.SimpleDateFormat.applyPatternImpl(SimpleDateFormat.java:2402)",
            "java.base/java.text.SimpleDateFormat.applyPattern(SimpleDateFormat.java:2398)",
        ]));
    };
    let p = p.as_str().to_string();
    fmt_of(&a[0])?.borrow_mut().apply_pattern(&p, &["java.base/java.text.SimpleDateFormat.applyPatternImpl(SimpleDateFormat.java:2402)", "java.base/java.text.SimpleDateFormat.applyPattern(SimpleDateFormat.java:2398)"])?;
    Ok(Value::Null)
}
fn sdf_apply_localized(_: &mut Vm, a: &mut [Value]) -> VR {
    let p = str_of(&a[1], "pattern")?.to_string();
    let local = fmt_of(&a[0])?.borrow().sym.local_chars.clone();
    // `translatePattern` rejects letters outside the localized set
    let mut quoted = false;
    for c in p.chars() {
        if c == '\'' {
            quoted = !quoted;
        } else if !quoted && c.is_ascii_alphabetic() && !local.contains(c) {
            return Err(iae(format!("Illegal pattern  character '{c}'")).with_frames(&[
                "java.base/java.text.SimpleDateFormat.translatePattern(SimpleDateFormat.java:2355)",
                "java.base/java.text.SimpleDateFormat.applyLocalizedPattern(SimpleDateFormat.java:2415)",
            ]));
        }
    }
    let p = translate(&p, &local, PATTERN_CHARS);
    fmt_of(&a[0])?.borrow_mut().apply_pattern(&p, &["java.base/java.text.SimpleDateFormat.applyPatternImpl(SimpleDateFormat.java:2402)", "java.base/java.text.SimpleDateFormat.applyLocalizedPattern(SimpleDateFormat.java:2415)"])?;
    Ok(Value::Null)
}
fn sdf_to_localized(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = fmt_of(&a[0])?.borrow();
    Ok(Value::string(translate(&f.pattern, PATTERN_CHARS, &f.sym.local_chars)))
}
/// Maps pattern letters between two letter sets, leaving quoted text alone.
fn translate(p: &str, from: &str, to: &str) -> String {
    let to: Vec<char> = to.chars().collect();
    let mut out = String::new();
    let mut quoted = false;
    for c in p.chars() {
        if c == '\'' {
            quoted = !quoted;
            out.push(c);
            continue;
        }
        if !quoted
            && let Some(i) = from.chars().position(|x| x == c)
            && let Some(&t) = to.get(i)
        {
            out.push(t);
            continue;
        }
        out.push(c);
    }
    out
}
fn sdf_symbols(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(dfs_value(fmt_of(&a[0])?.borrow().sym.clone()))
}
fn sdf_set_symbols(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = dfs_of(&a[1])?.borrow().clone();
    fmt_of(&a[0])?.borrow_mut().sym = s;
    Ok(Value::Null)
}
fn sdf_2digit_start(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(date_value(fmt_of(&a[0])?.borrow().century_start))
}
fn sdf_set_2digit_start(_: &mut Vm, a: &mut [Value]) -> VR {
    let ms = match &a[1] {
        Value::Obj(o) => match &**o {
            Obj::Time(super::TimeVal::UtilDate(ms)) => ms.get(),
            _ => return Err(crate::vm::cce("not a Date")),
        },
        _ => return Err(npe("Cannot invoke \"java.util.Date.getTime()\" because \"startDate\" is null").with_frames(&["java.base/java.text.SimpleDateFormat.set2DigitYearStart(SimpleDateFormat.java:933)"])),
    };
    let f = fmt_of(&a[0])?;
    let mut f = f.borrow_mut();
    let mut c = cal_of(&f.cal)?.borrow().clone();
    c.set_time_in_millis(ms);
    f.century_start = ms;
    f.century_start_year = c.get(legacy::YEAR)?;
    Ok(Value::Null)
}

/// `DateFormat.equals` / `SimpleDateFormat.equals`.
pub fn equals(a: &DateFmt, b: &DateFmt) -> bool {
    if a.pattern != b.pattern || a.sym != b.sym {
        return false;
    }
    match (cal_of(&a.cal), cal_of(&b.cal)) {
        (Ok(x), Ok(y)) => {
            let (x, y) = (x.borrow(), y.borrow());
            x.first_dow == y.first_dow && x.min_days == y.min_days && x.lenient == y.lenient && x.zone.equals(&y.zone)
        }
        _ => false,
    }
}

pub fn clone_fmt(f: &DateFmt) -> DateFmt {
    let mut c = f.clone();
    if let Ok(cal) = cal_of(&f.cal) {
        c.cal = cal_value(cal.borrow().clone());
    }
    c
}

fn df_clone(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(fmt_value(clone_fmt(&fmt_of(&a[0])?.borrow())))
}

fn dfs_new0(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(dfs_value(Dfs::of(crate::text::DEFAULT_LOCALE)))
}
fn dfs_new1(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(dfs_value(Dfs::of(&locale_of(&a[0])?)))
}
fn dfs_clone(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(dfs_value(dfs_of(&a[0])?.borrow().clone()))
}
macro_rules! dfs_array {
    ($get:ident, $set:ident, $field:ident) => {
        fn $get(_: &mut Vm, a: &mut [Value]) -> VR {
            Ok(str_array(&dfs_of(&a[0])?.borrow().$field))
        }
        fn $set(_: &mut Vm, a: &mut [Value]) -> VR {
            let v = str_array_arg(&a[1])?;
            dfs_of(&a[0])?.borrow_mut().$field = v;
            Ok(Value::Null)
        }
    };
}
dfs_array!(dfs_eras, dfs_set_eras, eras);
dfs_array!(dfs_months, dfs_set_months, months);
dfs_array!(dfs_short_months, dfs_set_short_months, short_months);
dfs_array!(dfs_weekdays, dfs_set_weekdays, weekdays);
dfs_array!(dfs_short_weekdays, dfs_set_short_weekdays, short_weekdays);
dfs_array!(dfs_ampm, dfs_set_ampm, ampm);
fn dfs_local_chars(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(dfs_of(&a[0])?.borrow().local_chars.clone()))
}
fn dfs_set_local_chars(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = str_of(&a[1], "newLocalPatternChars")?.to_string();
    dfs_of(&a[0])?.borrow_mut().local_chars = s;
    Ok(Value::Null)
}
fn dfs_zone_strings(_: &mut Vm, a: &mut [Value]) -> VR {
    let rows = dfs_of(&a[0])?.borrow().zone_strings();
    let outer: Vec<Value> = rows.iter().map(|r| str_array(r)).collect();
    let ty = crate::types::Ty { class: cls::STRING, dims: 2 };
    Ok(Value::obj(Obj::Array(crate::value::JArray::from_data(ty, crate::value::ArrayData::Ref(outer)))))
}
fn dfs_set_zone_strings(_: &mut Vm, a: &mut [Value]) -> VR {
    let rows: Vec<Vec<String>> = match &a[1] {
        Value::Obj(o) => match &**o {
            Obj::Array(arr) => arr.data.borrow().to_values().iter().map(str_array_arg).collect::<Result<_, _>>()?,
            _ => return Err(crate::vm::cce("not a String[][]")),
        },
        _ => return Err(npe("Cannot read the array length because \"newZoneStrings\" is null")),
    };
    for r in &rows {
        if r.len() < 5 {
            return Err(Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, None));
        }
    }
    dfs_of(&a[0])?.borrow_mut().zone_strings = Some(rows);
    Ok(Value::Null)
}
fn dfs_hash(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = dfs_of(&a[0])?.borrow();
    let arr_hash = |v: &[String]| v.iter().fold(1i32, |h, x| h.wrapping_mul(31).wrapping_add(crate::java::string_hash(x)));
    let mut h: i32 = 5;
    for v in [&s.eras, &s.months, &s.short_months, &s.weekdays, &s.short_weekdays, &s.ampm] {
        h = h.wrapping_mul(11).wrapping_add(arr_hash(v));
    }
    h = h.wrapping_mul(11).wrapping_add(crate::java::string_hash(&s.local_chars));
    Ok(Value::Int(h))
}

fn field_of(v: &Value) -> Result<u8, Thrown> {
    match legacy_obj(v) {
        Some(LegacyObj::DateField(i)) => Ok(*i),
        _ => Err(crate::vm::cce("not a DateFormat.Field")),
    }
}
fn dff_calendar_field(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(DATE_FIELDS[field_of(&a[0])? as usize].2))
}
fn dff_of_calendar_field(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = crate::lib_lang::int(&a[0]);
    // the first constant with that calendar field (HOUR0 for HOUR, HOUR_OF_DAY0 for HOUR_OF_DAY)
    match DATE_FIELDS.iter().position(|d| d.2 == f && f >= 0) {
        Some(i) => Ok(Value::obj(Obj::Legacy(Box::new(LegacyObj::DateField(i as u8))))),
        None => Err(iae(format!("Unknown Calendar constant {f}")).with_frames(&["java.base/java.text.DateFormat$Field.ofCalendarField(DateFormat.java:974)"])),
    }
}

pub fn register(b: &mut crate::lookup::Builder) {
    use cls::*;
    b.supers(DATE_FORMAT, &[crate::types::cls::FORMAT, OBJECT]);
    b.supers(SIMPLE_DATE_FORMAT, &[DATE_FORMAT, crate::types::cls::FORMAT, OBJECT]);
    b.supers(DATE_FORMAT_SYMBOLS, &[OBJECT]);
    b.supers(DATE_FORMAT_FIELD, &[crate::types::cls::FORMAT_FIELD, OBJECT]);
    for (i, (name, _, _)) in DATE_FIELDS.iter().enumerate() {
        b.sf_data(DATE_FORMAT_FIELD, &format!("DateFormat.Field {name}"), |_, a| {
            let i = crate::lib_lang::int(&a[0]) as u8;
            Ok(Value::obj(Obj::Legacy(Box::new(LegacyObj::DateField(i)))))
        }, i as i64);
    }
    b.m(DATE_FORMAT_FIELD, "int getCalendarField()", dff_calendar_field);
    b.s(DATE_FORMAT_FIELD, "DateFormat.Field ofCalendarField(int)", dff_of_calendar_field);

    for (name, v) in [
        ("AM_PM_FIELD", 14),
        ("DATE_FIELD", 3),
        ("DAY_OF_WEEK_FIELD", 9),
        ("DAY_OF_WEEK_IN_MONTH_FIELD", 11),
        ("DAY_OF_YEAR_FIELD", 10),
        ("DEFAULT", 2),
        ("ERA_FIELD", 0),
        ("FULL", 0),
        ("HOUR_OF_DAY0_FIELD", 5),
        ("HOUR_OF_DAY1_FIELD", 4),
        ("HOUR0_FIELD", 16),
        ("HOUR1_FIELD", 15),
        ("LONG", 1),
        ("MEDIUM", 2),
        ("MILLISECOND_FIELD", 8),
        ("MINUTE_FIELD", 6),
        ("MONTH_FIELD", 2),
        ("SECOND_FIELD", 7),
        ("SHORT", 3),
        ("TIMEZONE_FIELD", 17),
        ("WEEK_OF_MONTH_FIELD", 13),
        ("WEEK_OF_YEAR_FIELD", 12),
        ("YEAR_FIELD", 1),
    ] {
        b.sf_data(DATE_FORMAT, &format!("int {name}"), |_, a| Ok(Value::Int(crate::lib_lang::int(&a[0]))), v);
    }
    b.s(DATE_FORMAT, "Locale[] getAvailableLocales()", crate::text::available_locales);
    b.m(DATE_FORMAT, "Calendar getCalendar()", df_calendar);
    b.s(DATE_FORMAT, "DateFormat getDateInstance()", df_date0);
    b.s(DATE_FORMAT, "DateFormat getDateInstance(int)", df_date1);
    b.s(DATE_FORMAT, "DateFormat getDateInstance(int,Locale)", df_date2);
    b.s(DATE_FORMAT, "DateFormat getDateTimeInstance()", df_dt0);
    b.s(DATE_FORMAT, "DateFormat getDateTimeInstance(int,int)", df_dt2);
    b.s(DATE_FORMAT, "DateFormat getDateTimeInstance(int,int,Locale)", df_dt3);
    b.s(DATE_FORMAT, "DateFormat getInstance()", df_get_instance);
    b.m(DATE_FORMAT, "NumberFormat getNumberFormat()", df_number_format);
    b.s(DATE_FORMAT, "DateFormat getTimeInstance()", df_time0);
    b.s(DATE_FORMAT, "DateFormat getTimeInstance(int)", df_time1);
    b.s(DATE_FORMAT, "DateFormat getTimeInstance(int,Locale)", df_time2);
    b.m(DATE_FORMAT, "TimeZone getTimeZone()", df_zone);
    b.m(DATE_FORMAT, "boolean isLenient()", df_lenient);
    b.m(DATE_FORMAT, "Date parse(String)", df_parse);
    b.m(DATE_FORMAT, "Date parse(String,ParsePosition)", df_parse_pos);
    b.m(DATE_FORMAT, "void setCalendar(Calendar)", df_set_calendar);
    b.m(DATE_FORMAT, "void setLenient(boolean)", df_set_lenient);
    b.m(DATE_FORMAT, "void setNumberFormat(NumberFormat)", df_set_number_format);
    b.m(DATE_FORMAT, "void setTimeZone(TimeZone)", df_set_zone);
    b.m(DATE_FORMAT, "def clone()", df_clone);

    b.c(SIMPLE_DATE_FORMAT, "()", sdf_new0);
    b.c(SIMPLE_DATE_FORMAT, "(String)", sdf_new1);
    b.c(SIMPLE_DATE_FORMAT, "(String,Locale)", sdf_new2);
    b.m(SIMPLE_DATE_FORMAT, "void applyLocalizedPattern(String)", sdf_apply_localized);
    b.m(SIMPLE_DATE_FORMAT, "void applyPattern(String)", sdf_apply);
    b.m(SIMPLE_DATE_FORMAT, "Date get2DigitYearStart()", sdf_2digit_start);
    b.m(SIMPLE_DATE_FORMAT, "DateFormatSymbols getDateFormatSymbols()", sdf_symbols);
    b.m(SIMPLE_DATE_FORMAT, "void setDateFormatSymbols(DateFormatSymbols)", sdf_set_symbols);
    b.m(SIMPLE_DATE_FORMAT, "void set2DigitYearStart(Date)", sdf_set_2digit_start);
    b.m(SIMPLE_DATE_FORMAT, "String toLocalizedPattern()", sdf_to_localized);
    b.m(SIMPLE_DATE_FORMAT, "String toPattern()", sdf_to_pattern);

    b.c(DATE_FORMAT_SYMBOLS, "()", dfs_new0);
    b.c(DATE_FORMAT_SYMBOLS, "(Locale)", dfs_new1);
    b.m(DATE_FORMAT_SYMBOLS, "def clone()", dfs_clone);
    b.m(DATE_FORMAT_SYMBOLS, "String[] getAmPmStrings()", dfs_ampm);
    b.s(DATE_FORMAT_SYMBOLS, "Locale[] getAvailableLocales()", crate::text::available_locales);
    b.m(DATE_FORMAT_SYMBOLS, "String[] getEras()", dfs_eras);
    b.s(DATE_FORMAT_SYMBOLS, "DateFormatSymbols getInstance()", dfs_new0);
    b.s(DATE_FORMAT_SYMBOLS, "DateFormatSymbols getInstance(Locale)", dfs_new1);
    b.m(DATE_FORMAT_SYMBOLS, "String getLocalPatternChars()", dfs_local_chars);
    b.m(DATE_FORMAT_SYMBOLS, "String[] getMonths()", dfs_months);
    b.m(DATE_FORMAT_SYMBOLS, "String[] getShortMonths()", dfs_short_months);
    b.m(DATE_FORMAT_SYMBOLS, "String[] getShortWeekdays()", dfs_short_weekdays);
    b.m(DATE_FORMAT_SYMBOLS, "String[] getWeekdays()", dfs_weekdays);
    b.m(DATE_FORMAT_SYMBOLS, "String[][] getZoneStrings()", dfs_zone_strings);
    b.m(DATE_FORMAT_SYMBOLS, "int hashCode()", dfs_hash);
    b.m(DATE_FORMAT_SYMBOLS, "void setAmPmStrings(String[])", dfs_set_ampm);
    b.m(DATE_FORMAT_SYMBOLS, "void setEras(String[])", dfs_set_eras);
    b.m(DATE_FORMAT_SYMBOLS, "void setLocalPatternChars(String)", dfs_set_local_chars);
    b.m(DATE_FORMAT_SYMBOLS, "void setMonths(String[])", dfs_set_months);
    b.m(DATE_FORMAT_SYMBOLS, "void setShortMonths(String[])", dfs_set_short_months);
    b.m(DATE_FORMAT_SYMBOLS, "void setShortWeekdays(String[])", dfs_set_short_weekdays);
    b.m(DATE_FORMAT_SYMBOLS, "void setWeekdays(String[])", dfs_set_weekdays);
    b.m(DATE_FORMAT_SYMBOLS, "void setZoneStrings(String[][])", dfs_set_zone_strings);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_patterns_like_the_jdk() {
        assert_eq!(compile("yyyy-MM-dd").unwrap().len(), 5);
        assert_eq!(compile("'o''clock' h").unwrap()[0], Item::Literal("o'clock".into()));
        assert_eq!(compile("q").unwrap_err().0, "Illegal pattern character 'q'");
        assert_eq!(compile("'abc").unwrap_err().0, "Unterminated quote");
        assert_eq!(compile("XXXX").unwrap_err().0, "invalid ISO 8601 format: length=4");
    }

    #[test]
    fn numeric_zones() {
        let t: Vec<u16> = "05:30".encode_utf16().collect();
        assert_eq!(numeric_zone(&t, 0, 1, 0, true), Ok((19_800_000, 5)));
        let t: Vec<u16> = "0530".encode_utf16().collect();
        assert_eq!(numeric_zone(&t, 0, -1, 0, false), Ok((-19_800_000, 4)));
    }
}
