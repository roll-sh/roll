//! `java.text.MessageFormat.format(String, Object[])` and `java.text.ChoiceFormat`.
//!
//! A message pattern is literal text with `{index[,type[,style]]}` arguments, `'…'` quoting and
//! `''` for an apostrophe, parsed in one pass the way `MessageFormat.applyPattern` does (a quote
//! left open quotes the rest of the pattern; stray `}` are literal). Arguments format as
//! `MessageFormat.subformat` formats them: by the argument's declared type (`number`, `date`,
//! `time`, `choice`) with the default locale's formats, else numbers through
//! `NumberFormat.getInstance()`, dates through the short date-time format, anything else with
//! `toString()`; a choice result holding `{` is itself a message over the same arguments.
//! Missing arguments print as `{n}`.

use std::cell::RefCell;

use super::{number_format, symbols, DecimalFmt, Style, DEFAULT_LOCALE};
use crate::time::legacy_fmt::DateFmt;
use crate::types::cls;
use crate::value::{Obj, Value};
use crate::vm::{iae, Throw, Thrown};

/// `ChoiceFormat`: ascending limits and the text chosen at or above each.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub limits: Vec<f64>,
    pub formats: Vec<String>,
}

/// `ChoiceFormat.nextDouble(d, positive)`: the adjacent double.
pub fn next_double(d: f64, positive: bool) -> f64 {
    if d.is_nan() {
        return d;
    }
    if d == 0.0 {
        let smallest = f64::from_bits(1);
        return if positive { smallest } else { -smallest };
    }
    let bits = d.to_bits();
    let up = (d > 0.0) == positive;
    if d.is_infinite() {
        return if (d > 0.0) == positive { d } else if d > 0.0 { f64::MAX } else { f64::MIN };
    }
    f64::from_bits(if up { bits + 1 } else { bits - 1 })
}

const CHOICE_CTOR: &str = "java.base/java.text.ChoiceFormat.<init>(ChoiceFormat.java:433)";

impl Choice {
    /// `ChoiceFormat.applyPattern`: `limit#text` or `limit<text` intervals separated by `|`.
    pub fn parse(pattern: &str, outer: &[&str]) -> Result<Choice, Thrown> {
        let mut seg = [String::new(), String::new()];
        let mut part = 0;
        let mut start = 0.0f64;
        let mut old = f64::NAN;
        let mut in_quote = false;
        let mut c = Choice { limits: Vec::new(), formats: Vec::new() };
        let chars: Vec<char> = pattern.chars().collect();
        let frames = |inner: &str| -> Vec<String> { std::iter::once(inner.to_string()).chain(outer.iter().map(|s| s.to_string())).collect() };
        let mut i = 0;
        while i < chars.len() {
            let ch = chars[i];
            if ch == '\'' {
                if i + 1 < chars.len() && chars[i + 1] == '\'' {
                    seg[part].push(ch);
                    i += 1;
                } else {
                    in_quote = !in_quote;
                }
            } else if in_quote {
                seg[part].push(ch);
            } else if ch == '<' || ch == '#' || ch == '\u{2264}' {
                if seg[0].is_empty() {
                    let f = frames("java.base/java.text.ChoiceFormat.applyPatternImpl(ChoiceFormat.java:301)");
                    return Err(iae("Each interval must contain a number before a format").with_frames(&f.iter().map(String::as_str).collect::<Vec<_>>()));
                }
                start = match seg[0].as_str() {
                    "\u{221E}" => f64::INFINITY,
                    "-\u{221E}" => f64::NEG_INFINITY,
                    s => match crate::java::parse_double(s) {
                        Some(d) => d,
                        None => {
                            let mut t = crate::lib_lang::float_failure(&Value::str(s), false);
                            t.frames.push("java.base/java.text.ChoiceFormat.stringToNum(ChoiceFormat.java:355)".into());
                            t.frames.push("java.base/java.text.ChoiceFormat.applyPatternImpl(ChoiceFormat.java:304)".into());
                            t.frames.extend(outer.iter().map(|s| s.to_string()));
                            return Err(t);
                        }
                    },
                };
                if ch == '<' && start.is_finite() {
                    start = next_double(start, true);
                }
                if start <= old {
                    let f = frames("java.base/java.text.ChoiceFormat.applyPatternImpl(ChoiceFormat.java:309)");
                    return Err(iae("Incorrect order of intervals, must be in ascending order").with_frames(&f.iter().map(String::as_str).collect::<Vec<_>>()));
                }
                seg[0].clear();
                part = 1;
            } else if ch == '|' {
                c.limits.push(start);
                c.formats.push(std::mem::take(&mut seg[1]));
                old = start;
                part = 0;
            } else {
                seg[part].push(ch);
            }
            i += 1;
        }
        if part == 1 {
            c.limits.push(start);
            c.formats.push(std::mem::take(&mut seg[1]));
        }
        Ok(c)
    }

    /// `ChoiceFormat.format(double)`.
    pub fn format(&self, x: f64) -> Result<String, Thrown> {
        let mut i = 0;
        while i < self.limits.len() {
            // `!(x >= limit)`: also stops at NaN
            if x.partial_cmp(&self.limits[i]).is_none_or(|o| o == std::cmp::Ordering::Less) {
                break;
            }
            i += 1;
        }
        let i = i.saturating_sub(1);
        match self.formats.get(i) {
            Some(s) => Ok(s.clone()),
            None => {
                let mut t = Throw::new(cls::ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index {i} out of bounds for length {}", self.formats.len())));
                t.frames = vec![
                    "java.base/java.text.ChoiceFormat.format(ChoiceFormat.java:557)".into(),
                    "java.base/java.text.ChoiceFormat.format(ChoiceFormat.java:523)".into(),
                    "java.base/java.text.NumberFormat.format(NumberFormat.java:331)".into(),
                    "java.base/java.text.Format.format(Format.java:165)".into(),
                ];
                Err(t)
            }
        }
    }

    /// `ChoiceFormat.toPattern()`.
    pub fn to_pattern(&self) -> String {
        let mut out = String::new();
        for (i, (&limit, text)) in self.limits.iter().zip(&self.formats).enumerate() {
            if i != 0 {
                out.push('|');
            }
            let less = next_double(limit, false);
            let le = libm::remainder(limit, 1.0).abs();
            let lt = libm::remainder(less, 1.0).abs();
            if le < lt {
                out.push_str(&num(limit));
                out.push('#');
            } else {
                if limit == f64::INFINITY {
                    out.push('\u{221E}');
                } else if limit == f64::NEG_INFINITY {
                    out.push_str("-\u{221E}");
                } else {
                    out.push_str(&num(less));
                }
                out.push('<');
            }
            let quote = text.contains(['<', '#', '\u{2264}', '|']);
            if quote {
                out.push('\'');
            }
            for c in text.chars() {
                out.push(c);
                if c == '\'' {
                    out.push(c);
                }
            }
            if quote {
                out.push('\'');
            }
        }
        out
    }
}

/// A double as string concatenation prints it (`∞` for infinities, as `ChoiceFormat` writes).
fn num(d: f64) -> String {
    if d == f64::INFINITY {
        "\u{221E}".into()
    } else if d == f64::NEG_INFINITY {
        "-\u{221E}".into()
    } else {
        crate::java::fmt_double(d)
    }
}

enum Sub {
    None,
    Number(Box<DecimalFmt>),
    Date(Box<DateFmt>),
    Choice(Choice),
}

struct Arg {
    /// Offset in the unquoted text where the argument goes.
    offset: usize,
    index: usize,
    sub: Sub,
}

const APPLY: [&str; 2] = ["java.base/java.text.MessageFormat.<init>(MessageFormat.java:516)", "java.base/java.text.MessageFormat.format(MessageFormat.java:1058)"];

fn frames(inner: &[&str]) -> Vec<String> {
    inner.iter().chain(APPLY.iter()).map(|s| s.to_string()).collect()
}

fn with(t: Thrown, inner: &[&str]) -> Thrown {
    let f = frames(inner);
    t.with_frames(&f.iter().map(String::as_str).collect::<Vec<_>>())
}

/// `MessageFormat.applyPattern`: the unquoted text and its arguments.
fn compile(pattern: &str) -> Result<(String, Vec<Arg>), Thrown> {
    let mut text = String::new();
    let mut seg: [String; 3] = Default::default();
    let mut part = 0usize; // 0 raw, 1 index, 2 type, 3 modifier
    let mut in_quote = false;
    let mut braces = 0;
    let mut args = Vec::new();
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        if part == 0 {
            if ch == '\'' {
                if i + 1 < chars.len() && chars[i + 1] == '\'' {
                    text.push(ch);
                    i += 1;
                } else {
                    in_quote = !in_quote;
                }
            } else if ch == '{' && !in_quote {
                part = 1;
            } else {
                text.push(ch);
            }
        } else if in_quote {
            seg[part - 1].push(ch);
            if ch == '\'' {
                in_quote = false;
            }
        } else {
            match ch {
                ',' if part < 3 => part += 1,
                '{' => {
                    braces += 1;
                    seg[part - 1].push(ch);
                }
                '}' if braces == 0 => {
                    args.push(make_arg(text.chars().map(|c| c.len_utf16()).sum(), &seg)?);
                    seg = Default::default();
                    part = 0;
                }
                '}' => {
                    braces -= 1;
                    seg[part - 1].push(ch);
                }
                ' ' if part == 2 && seg[1].is_empty() => {}
                '\'' => {
                    in_quote = true;
                    seg[part - 1].push(ch);
                }
                _ => seg[part - 1].push(ch),
            }
        }
        i += 1;
    }
    if braces == 0 && part != 0 {
        return Err(with(iae("Unmatched braces in the pattern."), &["java.base/java.text.MessageFormat.applyPatternImpl(MessageFormat.java:689)"]));
    }
    Ok((text, args))
}

fn make_arg(offset: usize, seg: &[String; 3]) -> Result<Arg, Thrown> {
    let set_from = "java.base/java.text.MessageFormat.setFormatFromPattern(MessageFormat.java:1679)";
    let apply = "java.base/java.text.MessageFormat.applyPatternImpl(MessageFormat.java:660)";
    let index: i64 = match seg[0].parse::<i32>() {
        Ok(n) if !seg[0].starts_with('+') => n as i64,
        _ => {
            let cause = Throw::new(cls::NUMBER_FORMAT_EXCEPTION, Some(format!("For input string: \"{}\"", seg[0])));
            let t = iae(format!("can't parse argument number: {}", seg[0])).with_cause(&cause);
            return Err(with(t, &["java.base/java.text.MessageFormat.setFormatFromPattern(MessageFormat.java:1644)", apply]));
        }
    };
    if index < 0 {
        return Err(with(iae(format!("negative argument number: {index}")), &["java.base/java.text.MessageFormat.setFormatFromPattern(MessageFormat.java:1648)", apply]));
    }
    let ty = seg[1].trim().to_lowercase();
    let style = &seg[2];
    let style_kw = style.trim().to_lowercase();
    let sub_frames = ["java.base/java.text.MessageFormat.formatFromSubformatPattern(MessageFormat.java:1842)", "java.base/java.text.MessageFormat.formatFromPattern(MessageFormat.java:1730)", set_from, apply];
    let sub = match ty.as_str() {
        "" => Sub::None,
        "number" => Sub::Number(Box::new(match style_kw.as_str() {
            "" => number_format(DEFAULT_LOCALE, Style::Number),
            "currency" => number_format(DEFAULT_LOCALE, Style::Currency),
            "percent" => number_format(DEFAULT_LOCALE, Style::Percent),
            "integer" => number_format(DEFAULT_LOCALE, Style::Integer),
            _ => DecimalFmt::new(symbols(DEFAULT_LOCALE), style).map_err(|m| {
                let t = super::pattern_error(m, "text.DecimalFormat.<init>(DecimalFormat.java:523)");
                let mut f: Vec<String> = t.frames.clone();
                f.extend(frames(&sub_frames));
                let mut t = t;
                t.frames = f;
                t
            })?,
        })),
        "date" | "time" => {
            let style_no = match style_kw.as_str() {
                "" => Some(2),
                "short" => Some(3),
                "medium" => Some(2),
                "long" => Some(1),
                "full" => Some(0),
                _ => None,
            };
            let pattern = match style_no {
                Some(n) if ty == "date" => crate::time::locale::localized_pattern(DEFAULT_LOCALE, Some(n), None).to_string(),
                Some(n) => crate::time::locale::localized_pattern(DEFAULT_LOCALE, None, Some(n)).to_string(),
                None => style.clone(),
            };
            Sub::Date(Box::new(DateFmt::new(&pattern, DEFAULT_LOCALE, &frames(&sub_frames).iter().map(String::as_str).collect::<Vec<_>>())?))
        }
        "choice" => Sub::Choice(Choice::parse(style, &frames(&sub_frames).iter().map(String::as_str).collect::<Vec<_>>())?),
        _ => {
            return Err(with(iae(format!("unknown format type: {}", seg[1])), &["java.base/java.text.MessageFormat.formatFromPattern(MessageFormat.java:1713)", set_from, apply]));
        }
    };
    Ok(Arg { offset, index: index as usize, sub })
}

/// The frames of a sub-format failure inside `MessageFormat.format(String, Object[])`.
fn sub_failure(mut t: Thrown, line: u32) -> Thrown {
    for f in [
        format!("java.base/java.text.MessageFormat.subformat(MessageFormat.java:{line})"),
        "java.base/java.text.MessageFormat.format(MessageFormat.java:1093)".into(),
        "java.base/java.text.Format.format(Format.java:165)".into(),
        "java.base/java.text.MessageFormat.format(MessageFormat.java:1059)".into(),
    ] {
        t.frames.push(f);
    }
    t
}

fn is_number(v: &Value) -> bool {
    matches!(v, Value::Byte(_) | Value::Short(_) | Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_)) || v.other_number().is_some()
}

fn number_of(v: &Value) -> f64 {
    v.as_f64().or_else(|| v.other_number().map(|(d, _)| d)).unwrap_or(0.0)
}

/// `MessageFormat.format(String, Object[])`.
pub fn format_message(pattern: &str, args: Option<&[Value]>) -> Result<String, Thrown> {
    let (text, compiled) = compile(pattern)?;
    run(&text, &compiled, args)
}

fn run(text: &str, compiled: &[Arg], args: Option<&[Value]>) -> Result<String, Thrown> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut out = String::new();
    let mut last = 0;
    for a in compiled {
        out.push_str(&String::from_utf16_lossy(&units[last..a.offset]));
        last = a.offset;
        let Some(obj) = args.and_then(|v| v.get(a.index)) else {
            out.push_str(&format!("{{{}}}", a.index));
            continue;
        };
        if obj.is_null() {
            out.push_str("null");
            continue;
        }
        match &a.sub {
            Sub::Choice(c) => {
                if !is_number(obj) {
                    let t = Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some("Cannot format given Object as a Number".into()))
                        .with_frames(&["java.base/java.text.NumberFormat.format(NumberFormat.java:335)", "java.base/java.text.Format.format(Format.java:165)"]);
                    return Err(sub_failure(t, 1521));
                }
                let s = c.format(number_of(obj)).map_err(|t| sub_failure(t, 1521))?;
                if s.contains('{') {
                    let (t2, c2) = compile(&s)?;
                    out.push_str(&run(&t2, &c2, args)?);
                } else {
                    out.push_str(&s);
                }
            }
            Sub::Number(f) => out.push_str(&super::format_object(f, obj).map_err(|t| sub_failure(t, 1581))?),
            Sub::Date(f) => {
                let s = crate::time::legacy_fmt::format_with(f, obj).map_err(|t| sub_failure(t, 1581))?;
                out.push_str(&s);
            }
            Sub::None => {
                if is_number(obj) {
                    out.push_str(&super::format_object(&number_format(DEFAULT_LOCALE, Style::Number), obj).map_err(|t| sub_failure(t, 1581))?);
                } else if matches!(obj, Value::Obj(o) if matches!(&**o, Obj::Time(crate::time::TimeVal::UtilDate(_)))) {
                    let f = DateFmt::new(crate::time::locale::localized_pattern(DEFAULT_LOCALE, Some(3), Some(3)), DEFAULT_LOCALE, &[])?;
                    out.push_str(&crate::time::legacy_fmt::format_with(&f, obj)?);
                } else {
                    out.push_str(&obj.to_java_string());
                }
            }
        }
    }
    out.push_str(&String::from_utf16_lossy(&units[last..]));
    Ok(out)
}

// ---------------------------------------------------------------------------------- natives

use crate::vm::{Vm, VR};

fn mf_format(_: &mut Vm, a: &mut [Value]) -> VR {
    let pattern = match &a[0] {
        Value::Str(s) => s.as_str().to_string(),
        _ => {
            return Err(crate::vm::npe("Cannot invoke \"String.length()\" because \"pattern\" is null").with_frames(&[
                "java.base/java.text.MessageFormat.applyPatternImpl(MessageFormat.java:604)",
                "java.base/java.text.MessageFormat.<init>(MessageFormat.java:516)",
                "java.base/java.text.MessageFormat.format(MessageFormat.java:1058)",
            ]))
        }
    };
    let args = match &a[1] {
        Value::Obj(o) => match &**o {
            Obj::Array(arr) => Some(arr.data.borrow().to_values()),
            _ => None,
        },
        _ => None,
    };
    Ok(Value::string(format_message(&pattern, args.as_deref())?))
}

pub fn choice_value(c: Choice) -> Value {
    Value::obj(Obj::Text(super::TextObj::Choice(Box::new(RefCell::new(c)))))
}

pub fn choice_of(v: &Value) -> Result<&RefCell<Choice>, Thrown> {
    match super::text(v) {
        Some(super::TextObj::Choice(c)) => Ok(c),
        _ => Err(crate::vm::cce("not a ChoiceFormat")),
    }
}

fn arrays(limits: &Value, formats: &Value) -> Result<Choice, Thrown> {
    let get = |v: &Value| -> Result<Vec<Value>, Thrown> {
        match v {
            Value::Obj(o) => match &**o {
                Obj::Array(a) => Ok(a.data.borrow().to_values()),
                _ => Err(crate::vm::cce("not an array")),
            },
            _ => Err(crate::vm::npe("Cannot read the array length because \"limits\" is null")),
        }
    };
    let l: Vec<f64> = get(limits)?.iter().map(|v| v.as_f64().unwrap_or(0.0)).collect();
    let f: Vec<String> = get(formats)?.iter().map(|v| v.to_java_string()).collect();
    if l.len() != f.len() {
        return Err(iae("Array and limit arrays must be of the same length."));
    }
    Ok(Choice { limits: l, formats: f })
}

fn cf_new_pattern(_: &mut Vm, a: &mut [Value]) -> VR {
    let p = crate::lib_lang::string(&a[0], "newPattern")?.as_str().to_string();
    Ok(choice_value(Choice::parse(&p, &[CHOICE_CTOR])?))
}
fn cf_new_arrays(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(choice_value(arrays(&a[0], &a[1])?))
}
fn cf_apply(_: &mut Vm, a: &mut [Value]) -> VR {
    let p = crate::lib_lang::string(&a[1], "newPattern")?.as_str().to_string();
    let c = Choice::parse(&p, &["java.base/java.text.ChoiceFormat.applyPattern(ChoiceFormat.java:275)"])?;
    *choice_of(&a[0])?.borrow_mut() = c;
    Ok(Value::Null)
}
fn cf_set_choices(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = arrays(&a[1], &a[2])?;
    *choice_of(&a[0])?.borrow_mut() = c;
    Ok(Value::Null)
}
fn cf_to_pattern(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(choice_of(&a[0])?.borrow().to_pattern()))
}
fn cf_limits(_: &mut Vm, a: &mut [Value]) -> VR {
    let l = choice_of(&a[0])?.borrow().limits.clone();
    Ok(Value::obj(Obj::Array(crate::value::JArray::from_data(crate::types::Ty::of(cls::DOUBLE), crate::value::ArrayData::Double(l)))))
}
fn cf_formats(_: &mut Vm, a: &mut [Value]) -> VR {
    let f: Vec<Value> = choice_of(&a[0])?.borrow().formats.iter().map(|s| Value::string(s.clone())).collect();
    Ok(Value::obj(Obj::Array(crate::value::JArray::from_data(crate::types::Ty::of(cls::STRING), crate::value::ArrayData::Ref(f)))))
}
fn cf_next(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(next_double(a[0].as_f64().unwrap_or(0.0), true)))
}
fn cf_next2(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(next_double(a[0].as_f64().unwrap_or(0.0), crate::lib_lang::boolean(&a[1]))))
}
fn cf_previous(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(next_double(a[0].as_f64().unwrap_or(0.0), false)))
}

/// `Format.format(Object)` on a `ChoiceFormat`.
pub fn choice_format_object(c: &Choice, v: &Value) -> VR {
    if !is_number(v) {
        return Err(iae("Cannot format given Object as a Number").with_frames(&["java.base/java.text.NumberFormat.format(NumberFormat.java:335)", "java.base/java.text.Format.format(Format.java:165)"]));
    }
    Ok(Value::string(c.format(number_of(v))?))
}

pub fn register(b: &mut crate::lookup::Builder) {
    use cls::*;
    b.supers(MESSAGE_FORMAT, &[FORMAT, OBJECT]);
    b.supers(CHOICE_FORMAT, &[NUMBER_FORMAT, FORMAT, OBJECT]);
    b.supers(MESSAGE_FORMAT_FIELD, &[FORMAT_FIELD, OBJECT]);
    b.sf(MESSAGE_FORMAT_FIELD, "MessageFormat.Field ARGUMENT", |_, _| Ok(Value::obj(Obj::Text(super::TextObj::MessageField))));
    b.s(MESSAGE_FORMAT, "String format(String,Object[])", mf_format);
    b.c(CHOICE_FORMAT, "(String)", cf_new_pattern);
    b.c(CHOICE_FORMAT, "(double[],String[])", cf_new_arrays);
    b.m(CHOICE_FORMAT, "void applyPattern(String)", cf_apply);
    b.m(CHOICE_FORMAT, "def[] getFormats()", cf_formats);
    b.m(CHOICE_FORMAT, "double[] getLimits()", cf_limits);
    b.s(CHOICE_FORMAT, "double nextDouble(double)", cf_next);
    b.s(CHOICE_FORMAT, "double nextDouble(double,boolean)", cf_next2);
    b.s(CHOICE_FORMAT, "double previousDouble(double)", cf_previous);
    b.m(CHOICE_FORMAT, "void setChoices(double[],String[])", cf_set_choices);
    b.m(CHOICE_FORMAT, "String toPattern()", cf_to_pattern);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages() {
        let n = |v: i32| Value::Int(v);
        assert_eq!(format_message("{0} x {1}", Some(&[n(1), Value::str("a")])).unwrap(), "1 x a");
        assert_eq!(format_message("It's {0}", Some(&[Value::str("x")])).unwrap(), "Its {0}");
        assert_eq!(format_message("{0} {5}", Some(&[n(1)])).unwrap(), "1 {5}");
        assert_eq!(format_message("{0,choice,0#none|1#one|1<{0} many}", Some(&[n(7)])).unwrap(), "7 many");
        assert!(format_message("{0", Some(&[n(1)])).is_err());
    }

    #[test]
    fn choices() {
        let c = Choice::parse("0#a|1<b|\u{221E}#c", &[]).unwrap();
        assert_eq!(c.to_pattern(), "0.0#a|1.0<b|\u{221E}<c");
        assert_eq!(c.format(f64::NAN).unwrap(), "a");
        assert_eq!(next_double(1.0, true), 1.0000000000000002);
    }
}
