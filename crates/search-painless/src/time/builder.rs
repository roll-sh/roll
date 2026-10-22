//! `java.time.format.DateTimeFormatterBuilder`: building a formatter item by item.
//!
//! The builder holds the same items `DateTimeFormatter.ofPattern` compiles (see
//! [`super::format`]) on a stack whose top is the innermost open `optionalStart()`; a pending
//! `padNext` decorates the next appended item. `toFormatter()` closes open optional sections,
//! as the JDK does, and gives a SMART-resolving formatter in the default (or given) locale.
//! Argument validation follows the JDK's messages.

use std::cell::RefCell;
use std::rc::Rc;

use super::format::{compile_pattern_into, formatter_of, formatter_value, FField, Formatter, Item, Sign, OFFSET_PATTERNS};
use super::*;
use crate::lookup::Builder;
use crate::value::{Obj, Value};
use crate::vm::{iae, npe, Thrown, Vm, VR};

#[derive(Debug, Default)]
pub struct FmtBuilder {
    stack: Vec<Vec<Item>>,
    pad: Option<(usize, char)>,
}

impl FmtBuilder {
    fn new() -> FmtBuilder {
        FmtBuilder { stack: vec![Vec::new()], pad: None }
    }

    fn push(&mut self, item: Item) {
        let item = match self.pad.take() {
            Some((width, ch)) => Item::Pad { width, ch, item: Box::new(item) },
            None => item,
        };
        self.stack.last_mut().unwrap().push(item);
    }

    /// The items with every open optional section closed.
    fn items(&self) -> Vec<Item> {
        let mut stack = self.stack.clone();
        while stack.len() > 1 {
            let inner = stack.pop().unwrap();
            stack.last_mut().unwrap().push(Item::Optional(inner));
        }
        stack.pop().unwrap_or_default()
    }
}

/// An `IllegalArgumentException` from `DateTimeFormatterBuilder` methods (`frames` relative to
/// `java.time.format.`, `DateTimeFormatterBuilder.` implied when absent).
fn builder_iae(msg: String, frames: &[&str]) -> Thrown {
    let f: Vec<String> = frames
        .iter()
        .map(|f| if f.starts_with("DateTimeFormatterBuilder") { format!("java.base/java.time.format.{f}") } else { format!("java.base/java.time.format.DateTimeFormatterBuilder.{f}") })
        .collect();
    let refs: Vec<&str> = f.iter().map(|s| s.as_str()).collect();
    iae(msg).with_frames(&refs)
}

fn builder_of(v: &Value) -> Result<Rc<RefCell<FmtBuilder>>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Time(TimeVal::FmtBuilder(b)) => Ok(b.clone()),
            _ => Err(crate::vm::cce("not a DateTimeFormatterBuilder")),
        },
        _ => Err(npe("builder")),
    }
}

fn with(a: &[Value], f: impl FnOnce(&mut FmtBuilder) -> Result<(), Thrown>) -> VR {
    let b = builder_of(&a[0])?;
    f(&mut b.borrow_mut())?;
    Ok(a[0].clone())
}

/// A `TemporalField` argument as a formatter field.
fn field_arg(v: &Value) -> Result<FField, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Time(TimeVal::TField(f)) => Ok(FField::Std(*f)),
            Obj::Time(TimeVal::XField(x)) => match x {
                weeks::XField::QuarterOfYear => Ok(FField::QuarterOfYear),
                weeks::XField::IsoWeekBasedYear => Ok(FField::IsoWeekBasedYear),
                weeks::XField::IsoWeekOfWeekBasedYear => Ok(FField::IsoWeekOfWeekBasedYear),
                _ => Err(crate::vm::uoe()),
            },
            _ => Err(crate::vm::cce("not a TemporalField")),
        },
        _ => Err(npe("field")),
    }
}

fn std_field(v: &Value) -> Result<Field, Thrown> {
    match field_arg(v)? {
        FField::Std(f) => Ok(f),
        _ => Err(crate::vm::uoe()),
    }
}

fn enum_arg(v: &Value, what: &str) -> Result<u8, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Time(TimeVal::Enum(_, ord)) => Ok(*ord),
            _ => Err(crate::vm::cce("not an enum")),
        },
        _ => Err(npe(what)),
    }
}

fn sign_of(ord: u8) -> Sign {
    match ord {
        1 => Sign::Always,
        2 => Sign::Never,
        3 => Sign::NotNegative,
        4 => Sign::ExceedsPad,
        _ => Sign::Normal,
    }
}

fn int(v: &Value) -> i32 {
    crate::lib_lang::int(v)
}

fn str_arg(v: &Value, what: &str) -> Result<String, Thrown> {
    match v {
        Value::Null => Err(npe(what)),
        v => Ok(v.to_java_string()),
    }
}

fn b_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(time_value(TimeVal::FmtBuilder(Rc::new(RefCell::new(FmtBuilder::new())))))
}

fn b_append(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = formatter_of(&a[1])?;
    with(a, |b| {
        b.push(Item::Group(f.items().to_vec()));
        Ok(())
    })
}

fn b_append_optional(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = formatter_of(&a[1])?;
    with(a, |b| {
        b.push(Item::Optional(f.items().to_vec()));
        Ok(())
    })
}

fn b_append_pattern(_: &mut Vm, a: &mut [Value]) -> VR {
    let p = str_arg(&a[1], "pattern")?;
    with(a, |b| compile_pattern_into(&p, &mut b.stack).map_err(|e| jt(e, &["format.DateTimeFormatterBuilder.appendPattern(DateTimeFormatterBuilder.java:1910)"])))
}

fn b_append_literal(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = str_arg(&a[1], "literal")?;
    with(a, |b| {
        if !s.is_empty() {
            b.push(Item::Lit(s));
        }
        Ok(())
    })
}

fn b_append_value1(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_arg(&a[1])?;
    with(a, |b| {
        b.push(Item::Value { f, min: 1, max: 19, sign: Sign::Normal });
        Ok(())
    })
}

fn append_fixed_width(a: &[Value], f: FField, w: i32) -> VR {
    if !(1..=19).contains(&w) {
        return Err(builder_iae(format!("The width must be from 1 to 19 inclusive but was {w}"), &["appendValue(DateTimeFormatterBuilder.java:502)"]));
    }
    with(a, |b| {
        b.push(Item::Value { f, min: w as u8, max: w as u8, sign: Sign::NotNegative });
        Ok(())
    })
}

fn b_append_value2(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_arg(&a[1])?;
    append_fixed_width(a, f, int(&a[2]))
}

fn b_append_value4(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_arg(&a[1])?;
    let (min, max) = (int(&a[2]), int(&a[3]));
    let sign = sign_of(enum_arg(&a[4], "signStyle")?);
    if min == max && sign == Sign::NotNegative {
        return append_fixed_width(a, f, max);
    }
    if !(1..=19).contains(&min) {
        return Err(builder_iae(format!("The minimum width must be from 1 to 19 inclusive but was {min}"), &["appendValue(DateTimeFormatterBuilder.java:548)"]));
    }
    if !(1..=19).contains(&max) {
        return Err(builder_iae(format!("The maximum width must be from 1 to 19 inclusive but was {max}"), &["appendValue(DateTimeFormatterBuilder.java:551)"]));
    }
    if max < min {
        return Err(builder_iae(format!("The maximum width must exceed or equal the minimum width but {max} < {min}"), &["appendValue(DateTimeFormatterBuilder.java:554)"]));
    }
    with(a, |b| {
        b.push(Item::Value { f, min: min as u8, max: max as u8, sign });
        Ok(())
    })
}

fn b_append_value_reduced(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = std_field(&a[1])?;
    let (width, max_width, base) = (int(&a[2]), int(&a[3]), int(&a[4]));
    let fail = |msg: String, line: u32| {
        builder_iae(msg, &[&format!("DateTimeFormatterBuilder$ReducedPrinterParser.<init>(DateTimeFormatterBuilder.java:{line})"), "appendValueReduced(DateTimeFormatterBuilder.java:604)"])
    };
    if !(1..=10).contains(&width) {
        return Err(fail(format!("The minWidth must be from 1 to 10 inclusive but was {width}"), 3146));
    }
    if !(1..=10).contains(&max_width) {
        return Err(fail(format!("The maxWidth must be from 1 to 10 inclusive but was {width}"), 3149));
    }
    if max_width < width {
        return Err(fail(format!("Maximum width must exceed or equal the minimum width but {max_width} < {width}"), 3152));
    }
    with(a, |b| {
        b.push(Item::Reduced { f, width: width as u8, max_width: max_width as u8, base: base as i64, date_base: false });
        Ok(())
    })
}

fn b_append_fraction(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = std_field(&a[1])?;
    let (min, max) = (int(&a[2]), int(&a[3]));
    let point = crate::lib_lang::boolean(&a[4]);
    // JDK 27 parses NANO_OF_SECOND with NanosPrinterParser, other fields with
    // FractionPrinterParser; both validate in their constructors
    let nanos = f == Field::NanoOfSecond;
    let (class, lines) = if nanos { ("NanosPrinterParser", [3298, 3301, 3304]) } else { ("FractionPrinterParser", [3495, 3498, 3501]) };
    let caller = match (nanos, min == max && !point) {
        (true, true) => 753,
        (true, false) => 755,
        (false, true) => 760,
        (false, false) => 762,
    };
    let fail = |msg: String, line: u32| {
        builder_iae(msg, &[&format!("DateTimeFormatterBuilder${class}.<init>(DateTimeFormatterBuilder.java:{line})"), &format!("appendFraction(DateTimeFormatterBuilder.java:{caller})")])
    };
    if !nanos && matches!(f, Field::DayOfMonth | Field::DayOfYear | Field::AlignedWeekOfMonth | Field::YearOfEra) {
        return Err(fail(format!("Field must have a fixed set of values: {}", f.name()), 3492));
    }
    if !(0..=9).contains(&min) {
        return Err(fail(format!("Minimum width must be from 0 to 9 inclusive but was {min}"), lines[0]));
    }
    if !(1..=9).contains(&max) {
        return Err(fail(format!("Maximum width must be from 1 to 9 inclusive but was {max}"), lines[1]));
    }
    if max < min {
        return Err(fail(format!("Maximum width must exceed or equal the minimum width but {max} < {min}"), lines[2]));
    }
    with(a, |b| {
        b.push(Item::Fraction { f, min: min as u8, max: max as u8, point });
        Ok(())
    })
}

fn b_append_text1(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_arg(&a[1])?;
    with(a, |b| {
        b.push(Item::Text { f, style: 0 });
        Ok(())
    })
}

fn b_append_text2(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_arg(&a[1])?;
    let style = enum_arg(&a[2], "textStyle")?;
    with(a, |b| {
        b.push(Item::Text { f, style });
        Ok(())
    })
}

fn b_append_instant0(_: &mut Vm, a: &mut [Value]) -> VR {
    with(a, |b| {
        b.push(Item::Instant);
        Ok(())
    })
}

fn b_append_instant1(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = int(&a[1]);
    if !(-1..=9).contains(&d) {
        return Err(builder_iae(format!("The fractional digits must be from -1 to 9 inclusive but was {d}"), &["appendInstant(DateTimeFormatterBuilder.java:950)"]));
    }
    with(a, |b| {
        b.push(Item::InstantDigits(d as i8));
        Ok(())
    })
}

fn b_append_localized(_: &mut Vm, a: &mut [Value]) -> VR {
    let date = if a[1].is_null() { None } else { Some(enum_arg(&a[1], "dateStyle")?) };
    let time = if a[2].is_null() { None } else { Some(enum_arg(&a[2], "timeStyle")?) };
    if date.is_none() && time.is_none() {
        return Err(builder_iae("Either the date or time style must be non-null".into(), &["appendLocalized(DateTimeFormatterBuilder.java:1467)"]));
    }
    with(a, |b| {
        b.push(Item::Localized { date, time });
        Ok(())
    })
}

fn b_append_localized_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let style = enum_arg(&a[1], "style")?;
    if style != 0 && style != 2 {
        return Err(iae("Style must be either full or short"));
    }
    with(a, |b| {
        b.push(Item::LocalizedOffset { full: style == 0 });
        Ok(())
    })
}

fn b_append_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    let pattern = str_arg(&a[1], "pattern")?;
    let none = str_arg(&a[2], "noOffsetText")?;
    let Some(p) = OFFSET_PATTERNS.iter().find(|p| **p == pattern) else {
        return Err(builder_iae(
            format!("Invalid zone offset pattern: {pattern}"),
            &[
                "DateTimeFormatterBuilder$OffsetIdPrinterParser.checkPattern(DateTimeFormatterBuilder.java:3980)",
                "DateTimeFormatterBuilder$OffsetIdPrinterParser.<init>(DateTimeFormatterBuilder.java:3969)",
                "appendOffset(DateTimeFormatterBuilder.java:1042)",
            ],
        ));
    };
    with(a, |b| {
        b.push(Item::Offset { pattern: p, none });
        Ok(())
    })
}

fn b_append_offset_id(_: &mut Vm, a: &mut [Value]) -> VR {
    with(a, |b| {
        b.push(Item::Offset { pattern: "+HH:MM:ss", none: "Z".into() });
        Ok(())
    })
}

fn b_append_zone_id(_: &mut Vm, a: &mut [Value]) -> VR {
    with(a, |b| {
        b.push(Item::ZoneId);
        Ok(())
    })
}

fn b_append_zone_text(_: &mut Vm, a: &mut [Value]) -> VR {
    let style = enum_arg(&a[1], "textStyle")?;
    with(a, |b| {
        b.push(Item::ZoneText { full: style < 2 });
        Ok(())
    })
}

fn b_append_chronology_id(_: &mut Vm, a: &mut [Value]) -> VR {
    with(a, |b| {
        b.push(Item::Chrono { text: false });
        Ok(())
    })
}

fn b_append_chronology_text(_: &mut Vm, a: &mut [Value]) -> VR {
    enum_arg(&a[1], "textStyle")?;
    with(a, |b| {
        b.push(Item::Chrono { text: true });
        Ok(())
    })
}

fn b_optional_start(_: &mut Vm, a: &mut [Value]) -> VR {
    with(a, |b| {
        b.stack.push(Vec::new());
        Ok(())
    })
}

fn b_optional_end(_: &mut Vm, a: &mut [Value]) -> VR {
    with(a, |b| {
        if b.stack.len() < 2 {
            return Err(crate::vm::Throw::new(cls::ILLEGAL_STATE_EXCEPTION, Some("Cannot call optionalEnd() as there was no previous call to optionalStart()".into()))
                .with_frames(&["java.base/java.time.format.DateTimeFormatterBuilder.optionalEnd(DateTimeFormatterBuilder.java:2347)"]));
        }
        let inner = b.stack.pop().unwrap();
        if !inner.is_empty() {
            b.push(Item::Optional(inner));
        }
        Ok(())
    })
}

fn pad_next(a: &[Value], width: i32, ch: char, via_one_arg: bool) -> VR {
    if width < 1 {
        let mut f = vec!["padNext(DateTimeFormatterBuilder.java:2285)"];
        if via_one_arg {
            f.push("padNext(DateTimeFormatterBuilder.java:2259)");
        }
        return Err(builder_iae(format!("The pad width must be at least one but was {width}"), &f));
    }
    with(a, |b| {
        b.pad = Some((width as usize, ch));
        Ok(())
    })
}

fn b_pad_next1(_: &mut Vm, a: &mut [Value]) -> VR {
    pad_next(a, int(&a[1]), ' ', true)
}

fn b_pad_next2(_: &mut Vm, a: &mut [Value]) -> VR {
    let ch = char::from_u32(crate::lib_lang::int(&a[2]) as u32).unwrap_or(' ');
    pad_next(a, int(&a[1]), ch, false)
}

fn b_case_insensitive(_: &mut Vm, a: &mut [Value]) -> VR {
    with(a, |b| {
        b.stack.last_mut().unwrap().push(Item::CaseInsensitive);
        Ok(())
    })
}

fn b_case_sensitive(_: &mut Vm, a: &mut [Value]) -> VR {
    with(a, |b| {
        b.stack.last_mut().unwrap().push(Item::CaseSensitive);
        Ok(())
    })
}

fn b_parse_lenient(_: &mut Vm, a: &mut [Value]) -> VR {
    with(a, |b| {
        b.stack.last_mut().unwrap().push(Item::ParseMode { strict: false });
        Ok(())
    })
}

fn b_parse_strict(_: &mut Vm, a: &mut [Value]) -> VR {
    with(a, |b| {
        b.stack.last_mut().unwrap().push(Item::ParseMode { strict: true });
        Ok(())
    })
}

fn b_parse_defaulting(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = field_arg(&a[1])?;
    let v = crate::lib_lang::long(&a[2]);
    with(a, |b| {
        b.stack.last_mut().unwrap().push(Item::Default { f, v });
        Ok(())
    })
}

fn b_to_formatter(_: &mut Vm, a: &mut [Value]) -> VR {
    let b = builder_of(&a[0])?;
    let items = b.borrow().items();
    Ok(formatter_value(Formatter::from_items(items, crate::text::DEFAULT_LOCALE)))
}

fn b_to_formatter_locale(_: &mut Vm, a: &mut [Value]) -> VR {
    let b = builder_of(&a[0])?;
    let locale = match &a[1] {
        Value::Obj(o) => match &**o {
            Obj::Locale(l) => l.to_string(),
            _ => return Err(crate::vm::cce("not a Locale")),
        },
        _ => return Err(npe("locale")),
    };
    let items = b.borrow().items();
    Ok(formatter_value(Formatter::from_items(items, &locale)))
}

fn b_localized_pattern(_: &mut Vm, a: &mut [Value]) -> VR {
    let date = if a[0].is_null() { None } else { Some(enum_arg(&a[0], "dateStyle")?) };
    let time = if a[1].is_null() { None } else { Some(enum_arg(&a[1], "timeStyle")?) };
    if date.is_none() && time.is_none() {
        return Err(builder_iae("Either dateStyle or timeStyle must be non-null".into(), &["getLocalizedDateTimePattern(DateTimeFormatterBuilder.java:223)"]));
    }
    let locale = match &a[3] {
        Value::Obj(o) => match &**o {
            Obj::Locale(l) => l.to_string(),
            _ => return Err(crate::vm::cce("not a Locale")),
        },
        _ => return Err(npe("locale")),
    };
    Ok(Value::str(locale::localized_pattern(&locale, date, time)))
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.supers(DATE_TIME_FORMATTER_BUILDER, &[OBJECT]);
    let c = DATE_TIME_FORMATTER_BUILDER;
    b.c(c, "()", b_new);
    let n = "DateTimeFormatterBuilder";
    b.m(c, &format!("{n} append(DateTimeFormatter)"), b_append);
    b.m(c, &format!("{n} appendChronologyId()"), b_append_chronology_id);
    b.m(c, &format!("{n} appendChronologyText(TextStyle)"), b_append_chronology_text);
    b.m(c, &format!("{n} appendFraction(TemporalField,int,int,boolean)"), b_append_fraction);
    b.m(c, &format!("{n} appendInstant()"), b_append_instant0);
    b.m(c, &format!("{n} appendInstant(int)"), b_append_instant1);
    b.m(c, &format!("{n} appendLiteral(String)"), b_append_literal);
    b.m(c, &format!("{n} appendLocalized(FormatStyle,FormatStyle)"), b_append_localized);
    b.m(c, &format!("{n} appendLocalizedOffset(TextStyle)"), b_append_localized_offset);
    b.m(c, &format!("{n} appendOffset(String,String)"), b_append_offset);
    b.m(c, &format!("{n} appendOffsetId()"), b_append_offset_id);
    b.m(c, &format!("{n} appendOptional(DateTimeFormatter)"), b_append_optional);
    b.m(c, &format!("{n} appendPattern(String)"), b_append_pattern);
    b.m(c, &format!("{n} appendText(TemporalField)"), b_append_text1);
    b.m(c, &format!("{n} appendText(TemporalField,TextStyle)"), b_append_text2);
    b.m(c, &format!("{n} appendValue(TemporalField)"), b_append_value1);
    b.m(c, &format!("{n} appendValue(TemporalField,int)"), b_append_value2);
    b.m(c, &format!("{n} appendValue(TemporalField,int,int,SignStyle)"), b_append_value4);
    b.m(c, &format!("{n} appendValueReduced(TemporalField,int,int,int)"), b_append_value_reduced);
    b.m(c, &format!("{n} appendZoneId()"), b_append_zone_id);
    b.m(c, &format!("{n} appendZoneOrOffsetId()"), b_append_zone_id);
    b.m(c, &format!("{n} appendZoneRegionId()"), b_append_zone_id);
    b.m(c, &format!("{n} appendZoneText(TextStyle)"), b_append_zone_text);
    b.m(c, &format!("{n} appendZoneText(TextStyle,Set)"), b_append_zone_text);
    b.s(c, "String getLocalizedDateTimePattern(FormatStyle,FormatStyle,Chronology,Locale)", b_localized_pattern);
    b.m(c, &format!("{n} optionalEnd()"), b_optional_end);
    b.m(c, &format!("{n} optionalStart()"), b_optional_start);
    b.m(c, &format!("{n} padNext(int)"), b_pad_next1);
    b.m(c, &format!("{n} padNext(int,char)"), b_pad_next2);
    b.m(c, &format!("{n} parseCaseInsensitive()"), b_case_insensitive);
    b.m(c, &format!("{n} parseCaseSensitive()"), b_case_sensitive);
    b.m(c, &format!("{n} parseDefaulting(TemporalField,long)"), b_parse_defaulting);
    b.m(c, &format!("{n} parseLenient()"), b_parse_lenient);
    b.m(c, &format!("{n} parseStrict()"), b_parse_strict);
    b.m(c, "DateTimeFormatter toFormatter()", b_to_formatter);
    b.m(c, "DateTimeFormatter toFormatter(Locale)", b_to_formatter_locale);
}
