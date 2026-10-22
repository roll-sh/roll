//! `java.text` number formatting and normalization, and `java.util.Currency`, for scripts:
//! `NumberFormat`, `DecimalFormat`, `DecimalFormatSymbols`, `ParsePosition`, `FieldPosition`,
//! `NumberFormat.Field`, `Normalizer` and currencies.
//!
//! Locale data (symbols, number/percent/currency patterns for the 1,189 locales of the JDK's
//! CLDR provider, and the 233 currencies) was captured from Elasticsearch 9.5.5's JDK into
//! `locales.tsv`. The default locale is `en_US`, as in Elasticsearch's JVM.

pub mod number;
pub mod message;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;

pub use number::{DecimalFmt, Parsed, Symbols};

use crate::bignum::MathObj;
use crate::lookup::Builder;
use crate::types::{cls, ClassId};
use crate::value::{Obj, Value};
use crate::vm::{npe, Throw, Thrown, Vm, VR};

/// The default `FORMAT` locale of Elasticsearch's JVM.
pub const DEFAULT_LOCALE: &str = "en_US";

// ---------------------------------------------------------------------------------- data

/// A currency (`java.util.Currency`).
#[derive(Debug, PartialEq, Eq)]
pub struct CurrencyInfo {
    pub code: &'static str,
    pub numeric: i32,
    pub digits: i32,
    pub symbol: &'static str,
    pub name: &'static str,
}

struct Data {
    profiles: Vec<Vec<&'static str>>,
    locales: HashMap<&'static str, usize>,
    /// `Locale.toString()` of every available locale, in the JDK's order.
    available: Vec<&'static str>,
    currencies: Vec<CurrencyInfo>,
}

fn data() -> &'static Data {
    static D: OnceLock<Data> = OnceLock::new();
    D.get_or_init(|| {
        let mut d = Data { profiles: Vec::new(), locales: HashMap::new(), available: Vec::new(), currencies: Vec::new() };
        for line in include_str!("locales.tsv").lines() {
            let f: Vec<&'static str> = line.split('\t').collect();
            match f[0] {
                "P" => d.profiles.push(f[1..].to_vec()),
                "L" => {
                    d.locales.insert(f[1], f[3].parse().unwrap_or(0));
                    d.available.push(f[1]);
                }
                "C" => d.currencies.push(CurrencyInfo {
                    code: f[1],
                    numeric: f[2].parse().unwrap_or(0),
                    digits: f[3].parse().unwrap_or(0),
                    symbol: f[4],
                    name: f[5],
                }),
                _ => {}
            }
        }
        d
    })
}

/// `Locale.toString()` of every available locale, in the JDK's order
/// (`Locale.getAvailableLocales()`).
pub fn available_locale_names() -> &'static [&'static str] {
    &data().available
}

/// The data profile of a locale (`Locale.toString()`): exact, then without script and
/// extensions, then language and country, then language, then the root locale.
fn profile(locale: &str) -> &'static [&'static str] {
    let d = data();
    if let Some(&p) = d.locales.get(locale) {
        return &d.profiles[p];
    }
    // a variant the data only lists with extensions (`th_TH_TH_#u-nu-thai`)
    if let Some(k) = d.available.iter().find(|k| k.starts_with(&format!("{locale}_#"))) {
        return &d.profiles[d.locales[k]];
    }
    let mut candidates = Vec::new();
    if let Some(i) = locale.find("_#") {
        candidates.push(locale[..i].to_string());
    }
    let parts: Vec<&str> = locale.split('_').collect();
    if parts.len() > 2 {
        candidates.push(format!("{}_{}", parts[0], parts[1]));
    }
    candidates.push(parts[0].to_string());
    for c in &candidates {
        if let Some(&p) = d.locales.get(c.as_str()) {
            return &d.profiles[p];
        }
    }
    &d.profiles[d.locales.get("").copied().unwrap_or(0)]
}

fn ch(s: &str) -> char {
    s.chars().next().unwrap_or('\0')
}

/// `DecimalFormatSymbols.getInstance(locale)`.
pub fn symbols(locale: &str) -> Symbols {
    let p = profile(locale);
    Symbols {
        locale: Rc::from(locale),
        zero: ch(p[0]),
        grouping: ch(p[1]),
        decimal: ch(p[2]),
        minus: ch(p[3]),
        minus_text: p[4].to_string(),
        percent: ch(p[5]),
        percent_text: p[6].to_string(),
        per_mill: ch(p[7]),
        per_mill_text: p[7].to_string(),
        digit: '#',
        pattern_sep: ';',
        exponent: p[8].to_string(),
        infinity: p[9].to_string(),
        nan: p[10].to_string(),
        currency_symbol: p[11].to_string(),
        intl_currency: p[12].to_string(),
        monetary_decimal: ch(p[13]),
        monetary_grouping: ch(p[14]),
    }
}

/// The styles of `NumberFormat`'s factories.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Style {
    Number,
    Integer,
    Percent,
    Currency,
}

/// `NumberFormat.get*Instance(locale)`.
pub fn number_format(locale: &str, style: Style) -> DecimalFmt {
    let p = profile(locale);
    let pattern = match style {
        Style::Number | Style::Integer => p[15],
        Style::Percent => p[16],
        Style::Currency => p[17],
    };
    let mut f = DecimalFmt::new(symbols(locale), pattern).unwrap_or_else(|_| DecimalFmt::blank(symbols(locale)));
    if style == Style::Integer {
        f.set_max_frac(0);
        f.decimal_always = false;
        f.parse_integer_only = true;
    }
    f
}

pub fn currency(code: &str) -> Option<&'static CurrencyInfo> {
    data().currencies.iter().find(|c| c.code == code)
}

// ---------------------------------------------------------------------------------- objects

/// The `java.text` objects scripts hold.
pub enum TextObj {
    Decimal(Box<RefCell<DecimalFmt>>),
    Symbols(Box<RefCell<Symbols>>),
    ParsePosition { index: Cell<i32>, error: Cell<i32> },
    FieldPosition { field: i32, attribute: Option<u8>, begin: Cell<i32>, end: Cell<i32> },
    /// `NumberFormat.Field` by ordinal of [`FIELDS`].
    NumberField(u8),
    Currency(&'static CurrencyInfo),
    /// `ChoiceFormat`.
    Choice(Box<RefCell<message::Choice>>),
    /// `MessageFormat.Field.ARGUMENT`.
    MessageField,
}

/// `NumberFormat.Field` constants and their names.
const FIELDS: [(&str, &str); 11] = [
    ("INTEGER", "integer"),
    ("FRACTION", "fraction"),
    ("EXPONENT", "exponent"),
    ("DECIMAL_SEPARATOR", "decimal separator"),
    ("SIGN", "sign"),
    ("GROUPING_SEPARATOR", "grouping separator"),
    ("EXPONENT_SYMBOL", "exponent symbol"),
    ("PERCENT", "percent"),
    ("PERMILLE", "per mille"),
    ("CURRENCY", "currency"),
    ("EXPONENT_SIGN", "exponent sign"),
];

impl TextObj {
    pub fn class(&self) -> ClassId {
        match self {
            TextObj::Decimal(_) => cls::DECIMAL_FORMAT,
            TextObj::Symbols(_) => cls::DECIMAL_FORMAT_SYMBOLS,
            TextObj::ParsePosition { .. } => cls::PARSE_POSITION,
            TextObj::FieldPosition { .. } => cls::FIELD_POSITION,
            TextObj::NumberField(_) => cls::NUMBER_FORMAT_FIELD,
            TextObj::Currency(_) => cls::CURRENCY,
            TextObj::Choice(_) => cls::CHOICE_FORMAT,
            TextObj::MessageField => cls::MESSAGE_FORMAT_FIELD,
        }
    }

    pub fn write_string(&self, out: &mut String, identity: impl FnOnce() -> u32) {
        match self {
            TextObj::Decimal(f) => {
                let f = f.borrow();
                let name = crate::lib_util::locale_display_name(&f.sym.locale);
                out.push_str(&format!("DecimalFormat [locale: \"{name}\", pattern: \"{}\"]\n", f.to_pattern(false)));
            }
            TextObj::ParsePosition { index, error } => out.push_str(&format!("java.text.ParsePosition[index={},errorIndex={}]", index.get(), error.get())),
            TextObj::FieldPosition { field, attribute, begin, end } => {
                let attr = attribute.map(|a| format!("java.text.NumberFormat$Field({})", FIELDS[a as usize].1)).unwrap_or_else(|| "null".into());
                out.push_str(&format!("java.text.FieldPosition[field={field},attribute={attr},beginIndex={},endIndex={}]", begin.get(), end.get()));
            }
            TextObj::NumberField(i) => out.push_str(&format!("java.text.NumberFormat$Field({})", FIELDS[*i as usize].1)),
            TextObj::Currency(c) => out.push_str(c.code),
            TextObj::Symbols(_) => out.push_str(&format!("java.text.DecimalFormatSymbols@{:x}", identity())),
            TextObj::Choice(_) => out.push_str(&format!("java.text.ChoiceFormat@{:x}", identity())),
            TextObj::MessageField => out.push_str("java.text.MessageFormat$Field(message argument field)"),
        }
    }

    pub fn equals(&self, other: &Value) -> bool {
        let Some(o) = text(other) else { return false };
        match (self, o) {
            (TextObj::Decimal(a), TextObj::Decimal(b)) => *a.borrow() == *b.borrow(),
            (TextObj::Symbols(a), TextObj::Symbols(b)) => *a.borrow() == *b.borrow(),
            (TextObj::ParsePosition { index: a, error: e }, TextObj::ParsePosition { index: b, error: f }) => a.get() == b.get() && e.get() == f.get(),
            (TextObj::NumberField(a), TextObj::NumberField(b)) => a == b,
            (TextObj::Currency(a), TextObj::Currency(b)) => a.code == b.code,
            (TextObj::Choice(a), TextObj::Choice(b)) => *a.borrow() == *b.borrow(),
            (TextObj::MessageField, TextObj::MessageField) => true,
            _ => false,
        }
    }

    pub fn hash(&self) -> Option<i32> {
        Some(match self {
            TextObj::Decimal(f) => {
                let f = f.borrow();
                // NumberFormat: maximumIntegerDigits * 37 + the serialized maxFractionDigits (3)
                let nf = f.d_max_int().wrapping_mul(37).wrapping_add(3);
                nf.wrapping_mul(37).wrapping_add(crate::java::string_hash(f.positive_prefix()))
            }
            TextObj::Symbols(s) => {
                let s = s.borrow();
                let mut h = s.zero as i32;
                h = h.wrapping_mul(37).wrapping_add(s.grouping as i32);
                h.wrapping_mul(37).wrapping_add(s.decimal as i32)
            }
            TextObj::ParsePosition { index, error } => (error.get() << 16) | index.get(),
            TextObj::Currency(c) => crate::java::string_hash(c.code),
            _ => return None,
        })
    }
}

pub(crate) fn text(v: &Value) -> Option<&TextObj> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Text(t) => Some(t),
            _ => None,
        },
        _ => None,
    }
}

fn text_value(t: TextObj) -> Value {
    Value::obj(Obj::Text(t))
}

fn fmt_value(f: DecimalFmt) -> Value {
    text_value(TextObj::Decimal(Box::new(RefCell::new(f))))
}

fn fmt_of(v: &Value) -> Result<&RefCell<DecimalFmt>, Thrown> {
    match text(v) {
        Some(TextObj::Decimal(f)) => Ok(&**f),
        _ => Err(crate::vm::cce("not a DecimalFormat")),
    }
}

fn sym_of(v: &Value) -> Result<&RefCell<Symbols>, Thrown> {
    match text(v) {
        Some(TextObj::Symbols(s)) => Ok(&**s),
        _ if v.is_null() => Err(npe("Cannot invoke \"java.text.DecimalFormatSymbols.clone()\" because \"newSymbols\" is null")),
        _ => Err(crate::vm::cce("not a DecimalFormatSymbols")),
    }
}

fn int(v: &Value) -> i32 {
    crate::lib_lang::int(v)
}

fn string(v: &Value) -> Result<String, Thrown> {
    match v {
        Value::Str(s) => Ok(s.as_str().to_string()),
        Value::Null => Err(npe("Cannot invoke \"String.length()\" because \"text\" is null")),
        other => Ok(other.to_java_string()),
    }
}

fn locale_arg(v: &Value) -> Result<Rc<str>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Locale(l) => Ok(l.clone()),
            _ => Err(crate::vm::cce("not a Locale")),
        },
        _ => Err(npe("Cannot invoke \"Object.hashCode()\" because \"key\" is null")),
    }
}

fn jdk(mut t: Thrown, frames: &[&str]) -> Thrown {
    t.frames.extend(frames.iter().map(|f| if f.starts_with("sun.") { format!("java.base/{f}") } else { format!("java.base/java.{f}") }));
    t
}

pub(crate) fn pattern_error(msg: String, outer: &str) -> Thrown {
    // the JDK line of each `applyPattern` failure
    let line = if msg.starts_with("Multiple decimal") {
        3896
    } else if msg.starts_with("Malformed exponential") {
        3919
    } else if msg.starts_with("Malformed pattern") {
        3966
    } else if msg.starts_with("Unquoted special") {
        3801
    } else if msg.starts_with("Unexpected '0'") {
        3885
    } else if msg.starts_with("Too many") {
        if msg.contains('\u{2030}') && !msg.contains("%\u{2030}") { 3820 } else { 3812 }
    } else {
        3902
    };
    jdk(crate::vm::iae(msg), &[&format!("text.DecimalFormat.applyPattern(DecimalFormat.java:{line})"), outer])
}

/// `RoundingMode.UNNECESSARY` refusing to round a double.
fn arithmetic(msg: String) -> Thrown {
    jdk(
        Throw::new(cls::ARITHMETIC_EXCEPTION, Some(msg)),
        &[
            "text.DigitList.shouldRoundUp(DigitList.java:543)",
            "text.DigitList.round(DigitList.java:423)",
            "text.DigitList.set(DigitList.java:392)",
            "text.DigitList.set(DigitList.java:334)",
            "text.DecimalFormat.doubleSubformat(DecimalFormat.java:753)",
            "text.DecimalFormat.format(DecimalFormat.java:679)",
            "text.DecimalFormat.format(DecimalFormat.java:639)",
            "text.DecimalFormat.format(DecimalFormat.java:587)",
            "text.Format.format(Format.java:165)",
        ],
    )
}

/// `Format.format(Object)` of a number format.
pub(crate) fn format_object(f: &DecimalFmt, v: &Value) -> Result<String, Thrown> {
    let r = match v {
        Value::Byte(_) | Value::Short(_) | Value::Int(_) | Value::Long(_) => f.format_long(v.as_i64().unwrap_or(0)),
        Value::Float(_) | Value::Double(_) => f.format_double(v.as_f64().unwrap_or(0.0)),
        Value::Obj(o) => match &**o {
            Obj::BigInt(b) => match b.to_i64_exact().filter(|_| b.bit_length() < 64) {
                Some(l) => f.format_long(l),
                None => f.format_big_int(b, false),
            },
            Obj::BigDec(b) => f.format_big_dec(b),
            _ => match v.other_number() {
                Some((d, _)) => f.format_double(d),
                None => return Err(jdk(crate::vm::iae("Cannot format given Object as a Number"), &["text.DecimalFormat.format(DecimalFormat.java:588)", "text.Format.format(Format.java:165)"])),
            },
        },
        _ => return Err(jdk(crate::vm::iae("Cannot format given Object as a Number"), &["text.DecimalFormat.format(DecimalFormat.java:588)", "text.Format.format(Format.java:165)"])),
    };
    r.map_err(arithmetic)
}

fn parsed_value(p: Parsed) -> Value {
    match p {
        Parsed::Long(l) => Value::Long(l),
        Parsed::Double(d) => Value::Double(d),
        Parsed::Big(b) => Value::obj(Obj::BigDec(Rc::new(b))),
    }
}

fn parse_exception(msg: String, offset: i32, frames: &[&str]) -> Thrown {
    let mut t = jdk(Throw::new(cls::TEXT_PARSE_EXCEPTION, Some(msg)), frames);
    t.error_offset = Some(offset);
    t
}

// ---------------------------------------------------------------------------------- natives

fn df_new0(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(fmt_value(number_format(DEFAULT_LOCALE, Style::Number)))
}

fn df_new1(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::Str(p) = &a[0] else {
        return Err(jdk(npe("Cannot invoke \"String.length()\" because \"pattern\" is null"), &["text.DecimalFormat.applyPattern(DecimalFormat.java:3721)", "text.DecimalFormat.<init>(DecimalFormat.java:493)"]));
    };
    DecimalFmt::new(symbols(DEFAULT_LOCALE), p.as_str()).map(fmt_value).map_err(|m| pattern_error(m, "text.DecimalFormat.<init>(DecimalFormat.java:493)"))
}

fn df_new2(_: &mut Vm, a: &mut [Value]) -> VR {
    let sym = sym_of(&a[1])?.borrow().clone();
    let p = string(&a[0])?;
    DecimalFmt::new(sym, &p).map(fmt_value).map_err(|m| pattern_error(m, "text.DecimalFormat.<init>(DecimalFormat.java:523)"))
}

fn df_apply(_: &mut Vm, a: &mut [Value]) -> VR {
    let p = string(&a[1])?;
    fmt_of(&a[0])?.borrow_mut().apply_pattern(&p, false).map_err(|m| pattern_error(m, "text.DecimalFormat.applyPattern(DecimalFormat.java:3662)"))?;
    Ok(Value::Null)
}

fn df_apply_localized(_: &mut Vm, a: &mut [Value]) -> VR {
    let p = string(&a[1])?;
    fmt_of(&a[0])?.borrow_mut().apply_pattern(&p, true).map_err(|m| pattern_error(m, "text.DecimalFormat.applyLocalizedPattern(DecimalFormat.java:3688)"))?;
    Ok(Value::Null)
}

macro_rules! getter {
    ($name:ident, |$f:ident| $e:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let $f = fmt_of(&a[0])?.borrow();
            Ok($e)
        }
    };
}

macro_rules! setter {
    ($name:ident, |$f:ident, $v:ident| $e:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let $v = a[1].clone();
            let mut $f = fmt_of(&a[0])?.borrow_mut();
            $e;
            Ok(Value::Null)
        }
    };
}

getter!(df_grouping_size, |f| Value::Int(f.grouping_size as i32));
getter!(df_multiplier, |f| Value::Int(f.multiplier));
getter!(df_neg_prefix, |f| Value::str(f.negative_prefix()));
getter!(df_neg_suffix, |f| Value::str(f.negative_suffix()));
getter!(df_pos_prefix, |f| Value::str(f.positive_prefix()));
getter!(df_pos_suffix, |f| Value::str(f.positive_suffix()));
getter!(df_decimal_always, |f| Value::Bool(f.decimal_always));
getter!(df_parse_big, |f| Value::Bool(f.parse_big_decimal));
getter!(df_to_pattern, |f| Value::string(f.to_pattern(false)));
getter!(df_to_localized, |f| Value::string(f.to_pattern(true)));
getter!(df_symbols, |f| text_value(TextObj::Symbols(Box::new(RefCell::new(f.sym.clone())))));
getter!(nf_max_frac, |f| Value::Int(f.max_frac()));
getter!(nf_min_frac, |f| Value::Int(f.min_frac()));
getter!(nf_max_int, |f| Value::Int(f.max_int()));
getter!(nf_min_int, |f| Value::Int(f.min_int()));
getter!(nf_grouping_used, |f| Value::Bool(f.grouping_used));
getter!(nf_integer_only, |f| Value::Bool(f.parse_integer_only));
getter!(nf_rounding, |f| Value::obj(Obj::Math(MathObj::Mode(f.rounding))));
getter!(nf_currency, |f| currency(&f.sym.intl_currency).map(|c| text_value(TextObj::Currency(c))).unwrap_or(Value::Null));

setter!(df_set_decimal_always, |f, v| f.decimal_always = matches!(v, Value::Bool(true)));
setter!(df_set_multiplier, |f, v| f.multiplier = int(&v));
setter!(df_set_neg_prefix, |f, v| f.set_negative_prefix(v.to_java_string()));
setter!(df_set_neg_suffix, |f, v| f.set_negative_suffix(v.to_java_string()));
setter!(df_set_pos_prefix, |f, v| f.set_positive_prefix(v.to_java_string()));
setter!(df_set_pos_suffix, |f, v| f.set_positive_suffix(v.to_java_string()));
setter!(df_set_parse_big, |f, v| f.parse_big_decimal = matches!(v, Value::Bool(true)));
setter!(nf_set_max_frac, |f, v| f.set_max_frac(int(&v)));
setter!(nf_set_min_frac, |f, v| f.set_min_frac(int(&v)));
setter!(nf_set_max_int, |f, v| f.set_max_int(int(&v)));
setter!(nf_set_min_int, |f, v| f.set_min_int(int(&v)));
setter!(nf_set_grouping_used, |f, v| f.grouping_used = matches!(v, Value::Bool(true)));
setter!(nf_set_integer_only, |f, v| f.parse_integer_only = matches!(v, Value::Bool(true)));

fn df_set_grouping_size(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = int(&a[1]);
    if !(0..=127).contains(&n) {
        return Err(jdk(crate::vm::iae(format!("newValue is out of valid range. value: {n}")), &["text.DecimalFormat.setGroupingSize(DecimalFormat.java:3057)"]));
    }
    fmt_of(&a[0])?.borrow_mut().grouping_size = n as u8;
    Ok(Value::Null)
}

fn df_set_symbols(_: &mut Vm, a: &mut [Value]) -> VR {
    // a null symbols object is ignored (the JDK swallows the exception)
    if let Ok(s) = sym_of(&a[1]) {
        let s = s.borrow().clone();
        fmt_of(&a[0])?.borrow_mut().set_symbols(s);
    }
    Ok(Value::Null)
}

fn nf_set_rounding(_: &mut Vm, a: &mut [Value]) -> VR {
    let mode = match &a[1] {
        Value::Obj(o) => match &**o {
            Obj::Math(MathObj::Mode(m)) => *m,
            _ => return Err(crate::vm::cce("not a RoundingMode")),
        },
        _ => return Err(jdk(Throw::new(cls::NULL_POINTER_EXCEPTION, None), &["text.DecimalFormat.setRoundingMode(DecimalFormat.java:4197)"])),
    };
    fmt_of(&a[0])?.borrow_mut().rounding = mode;
    Ok(Value::Null)
}

fn nf_set_currency(_: &mut Vm, a: &mut [Value]) -> VR {
    let Some(TextObj::Currency(c)) = text(&a[1]) else { return Err(npe("currency")) };
    let mut f = fmt_of(&a[0])?.borrow_mut();
    let mut s = f.sym.clone();
    s.intl_currency = c.code.to_string();
    s.currency_symbol = currency_symbol(c, &s.locale);
    f.set_symbols(s);
    Ok(Value::Null)
}

fn fmt_format(_: &mut Vm, a: &mut [Value]) -> VR {
    if crate::time::legacy::legacy(&a[0]).is_some() {
        return crate::time::legacy_fmt::format_object(&a[0], &a[1]);
    }
    if let Some(TextObj::Choice(c)) = text(&a[0]) {
        return message::choice_format_object(&c.borrow(), &a[1]);
    }
    let f = fmt_of(&a[0])?.borrow();
    Ok(Value::string(format_object(&f, &a[1])?))
}

/// `format(Object, StringBuffer, FieldPosition)`: appends, and records the requested field's
/// span.
fn fmt_format3(_: &mut Vm, a: &mut [Value]) -> VR {
    if crate::time::legacy::legacy(&a[0]).is_some() {
        return crate::time::legacy_fmt::format_object3(&a[0], a);
    }
    let f = fmt_of(&a[0])?.borrow().clone();
    let s = format_object(&f, &a[1])?;
    let Value::Obj(o) = &a[2] else { return Err(npe("Cannot invoke \"java.lang.StringBuffer.append(String)\" because \"toAppendTo\" is null")) };
    let Obj::Builder { s: buf, .. } = &**o else { return Err(crate::vm::cce("not a StringBuffer")) };
    let base = crate::java::utf16_len(&buf.borrow()) as i32;
    if let Some(TextObj::FieldPosition { field, attribute, begin, end }) = text(&a[3]) {
        let want = attribute.map(|x| x as i32).unwrap_or(*field);
        if let Some((b, e)) = field_span(&f, &s, want) {
            begin.set(base + b);
            end.set(base + e);
        } else {
            begin.set(0);
            end.set(0);
        }
    }
    buf.borrow_mut().push_str(&s);
    Ok(a[2].clone())
}

/// The span (UTF-16 offsets) of `NumberFormat.INTEGER_FIELD` (0) or `FRACTION_FIELD` (1) in a
/// formatted number.
fn field_span(f: &DecimalFmt, s: &str, field: i32) -> Option<(i32, i32)> {
    let chars: Vec<char> = s.chars().collect();
    let is_digit = |c: char| (c as u32).wrapping_sub(f.sym.zero as u32) < 10 || c == f.sym.grouping || c == f.sym.monetary_grouping;
    let start = chars.iter().position(|&c| is_digit(c) || c == f.sym.decimal || c == f.sym.monetary_decimal)?;
    let mut i = start;
    while i < chars.len() && is_digit(chars[i]) {
        i += 1;
    }
    match field {
        0 => Some((start as i32, i as i32)),
        1 => {
            let mut j = i;
            if j < chars.len() && (chars[j] == f.sym.decimal || chars[j] == f.sym.monetary_decimal) {
                j += 1;
                let k = j;
                while j < chars.len() && (chars[j] as u32).wrapping_sub(f.sym.zero as u32) < 10 {
                    j += 1;
                }
                Some((k as i32, j as i32))
            } else {
                Some((i as i32, i as i32))
            }
        }
        _ => None,
    }
}

fn fmt_clone(_: &mut Vm, a: &mut [Value]) -> VR {
    match text(&a[0]) {
        Some(TextObj::Decimal(f)) => Ok(fmt_value(f.borrow().clone())),
        Some(TextObj::Symbols(s)) => Ok(text_value(TextObj::Symbols(Box::new(RefCell::new(s.borrow().clone()))))),
        _ => Ok(a[0].clone()),
    }
}

fn nf_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = fmt_of(&a[0])?.borrow();
    let Value::Str(s) = &a[1] else {
        return Err(jdk(npe("Cannot invoke \"String.regionMatches(int, String, int, int)\" because \"text\" is null"), &["text.DecimalFormat.parse(DecimalFormat.java:2244)", "text.NumberFormat.parse(NumberFormat.java:508)"]));
    };
    match f.parse(s.as_str(), 0) {
        Ok((v, end)) if end > 0 => Ok(parsed_value(v)),
        Ok(_) => Err(parse_exception(format!("Unparseable number: \"{}\"", s.as_str()), 0, &["text.NumberFormat.parse(NumberFormat.java:510)"])),
        Err(e) => Err(parse_exception(format!("Unparseable number: \"{}\"", s.as_str()), e as i32, &["text.NumberFormat.parse(NumberFormat.java:510)"])),
    }
}

/// The field a `FieldPosition` asks for (its `Format.Field` attribute's id when it has one).
pub fn field_position_field(v: &Value) -> Option<i32> {
    match text(v) {
        Some(TextObj::FieldPosition { field, attribute, .. }) => Some(attribute.map(|a| a as i32).unwrap_or(*field)),
        _ => None,
    }
}

pub fn set_field_position(v: &Value, b: i32, e: i32) {
    if let Some(TextObj::FieldPosition { begin, end, .. }) = text(v) {
        begin.set(b);
        end.set(e);
    }
}

pub fn parse_position_index(v: &Value) -> Option<i32> {
    match text(v) {
        Some(TextObj::ParsePosition { index, .. }) => Some(index.get()),
        _ => None,
    }
}

pub fn set_parse_position(v: &Value, i: Option<i32>, e: Option<i32>) {
    if let Some(TextObj::ParsePosition { index, error }) = text(v) {
        if let Some(i) = i {
            index.set(i);
        }
        if let Some(e) = e {
            error.set(e);
        }
    }
}

/// The `NumberFormat` a `SimpleDateFormat` formats numbers with: the locale's integer
/// instance without grouping.
pub fn integer_instance_for_dates(_: &mut Vm, locale: &str) -> VR {
    let mut f = number_format(locale, Style::Integer);
    f.grouping_used = false;
    Ok(fmt_value(f))
}

fn parse_with_position(f: &DecimalFmt, text_v: &Value, pos: &Value) -> VR {
    let Some(TextObj::ParsePosition { index, error }) = text(pos) else {
        return Err(jdk(npe("Cannot read field \"index\" because \"pos\" is null"), &["text.DecimalFormat.parse(DecimalFormat.java:2244)"]));
    };
    let Value::Str(s) = text_v else {
        return Err(jdk(npe("Cannot invoke \"String.regionMatches(int, String, int, int)\" because \"text\" is null"), &["text.DecimalFormat.parse(DecimalFormat.java:2244)"]));
    };
    let start = index.get().max(0) as usize;
    match f.parse(s.as_str(), start) {
        Ok((v, end)) => {
            index.set(end as i32);
            Ok(parsed_value(v))
        }
        Err(e) => {
            error.set(e as i32);
            Ok(Value::Null)
        }
    }
}

fn nf_parse2(_: &mut Vm, a: &mut [Value]) -> VR {
    let f = fmt_of(&a[0])?.borrow().clone();
    parse_with_position(&f, &a[1], &a[2])
}

fn fmt_parse_object(_: &mut Vm, a: &mut [Value]) -> VR {
    if crate::time::legacy::legacy(&a[0]).is_some() {
        return crate::time::legacy_fmt::parse_object(&a[0], &a[1]);
    }
    let f = fmt_of(&a[0])?.borrow();
    let Value::Str(s) = &a[1] else { return Err(npe("Cannot invoke \"String.regionMatches(int, String, int, int)\" because \"text\" is null")) };
    match f.parse(s.as_str(), 0) {
        Ok((v, end)) if end > 0 => Ok(parsed_value(v)),
        Ok(_) => Err(parse_exception("Format.parseObject(String) failed".into(), 0, &["text.Format.parseObject(Format.java:262)"])),
        Err(e) => Err(parse_exception("Format.parseObject(String) failed".into(), e as i32, &["text.Format.parseObject(Format.java:262)"])),
    }
}

fn fmt_parse_object2(vm: &mut Vm, a: &mut [Value]) -> VR {
    if crate::time::legacy::legacy(&a[0]).is_some() {
        return crate::time::legacy_fmt::parse_object2(vm, a);
    }
    nf_parse2(vm, a)
}

fn nf_factory(style: Style, with_locale: bool, a: &[Value]) -> VR {
    let locale = if with_locale {
        locale_arg(&a[0]).map_err(|t| {
            jdk(
                t,
                &[
                    "util.concurrent.ConcurrentHashMap.get(ConcurrentHashMap.java:951)",
                    "sun.util.locale.provider.LocaleProviderAdapter.getAdapter(LocaleProviderAdapter.java:242)",
                    "text.NumberFormat.getInstance(NumberFormat.java:1125)",
                ],
            )
        })?
    } else {
        Rc::from(DEFAULT_LOCALE)
    };
    Ok(fmt_value(number_format(&locale, style)))
}

macro_rules! factory {
    ($n0:ident, $n1:ident, $style:expr) => {
        fn $n0(_: &mut Vm, a: &mut [Value]) -> VR {
            nf_factory($style, false, a)
        }
        fn $n1(_: &mut Vm, a: &mut [Value]) -> VR {
            nf_factory($style, true, a)
        }
    };
}

factory!(nf_instance, nf_instance_l, Style::Number);
factory!(nf_number, nf_number_l, Style::Number);
factory!(nf_integer, nf_integer_l, Style::Integer);
factory!(nf_percent, nf_percent_l, Style::Percent);
factory!(nf_currency_inst, nf_currency_inst_l, Style::Currency);

pub fn available_locales(_: &mut Vm, _: &mut [Value]) -> VR {
    let items: Vec<Value> = data().available.iter().map(|l| Value::obj(Obj::Locale(Rc::from(*l)))).collect();
    Ok(Value::obj(Obj::Array(crate::value::JArray::from_data(crate::types::Ty::of(cls::LOCALE), crate::value::ArrayData::Ref(items)))))
}

fn nf_integer_field(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Int(0))
}
fn nf_fraction_field(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Int(1))
}

// DecimalFormatSymbols

fn sym_value(s: Symbols) -> Value {
    text_value(TextObj::Symbols(Box::new(RefCell::new(s))))
}

fn dfs_new0(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(sym_value(symbols(DEFAULT_LOCALE)))
}

fn dfs_new1(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Null => Err(jdk(npe("locale should not be null"), &["util.Objects.requireNonNull(Objects.java:246)", "text.DecimalFormatSymbols.<init>(DecimalFormatSymbols.java:118)"])),
        v => Ok(sym_value(symbols(&locale_arg(v)?))),
    }
}

macro_rules! sym_get {
    ($name:ident, |$s:ident| $e:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let $s = sym_of(&a[0])?.borrow();
            Ok($e)
        }
    };
}

macro_rules! sym_set {
    ($name:ident, |$s:ident, $v:ident| $e:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let $v = a[1].clone();
            let mut $s = sym_of(&a[0])?.borrow_mut();
            $e;
            Ok(Value::Null)
        }
    };
}

fn char_of(v: &Value) -> char {
    match v {
        Value::Char(c) => char::from_u32(*c as u32).unwrap_or('\u{fffd}'),
        other => other.to_java_string().chars().next().unwrap_or('\0'),
    }
}

sym_get!(dfs_currency_symbol, |s| Value::str(&s.currency_symbol));
sym_get!(dfs_decimal, |s| Value::Char(s.decimal as u16));
sym_get!(dfs_digit, |s| Value::Char(s.digit as u16));
sym_get!(dfs_exponent, |s| Value::str(&s.exponent));
sym_get!(dfs_grouping, |s| Value::Char(s.grouping as u16));
sym_get!(dfs_infinity, |s| Value::str(&s.infinity));
sym_get!(dfs_intl_currency, |s| Value::str(&s.intl_currency));
sym_get!(dfs_minus, |s| Value::Char(s.minus as u16));
sym_get!(dfs_monetary_decimal, |s| Value::Char(s.monetary_decimal as u16));
sym_get!(dfs_nan, |s| Value::str(&s.nan));
sym_get!(dfs_pattern_sep, |s| Value::Char(s.pattern_sep as u16));
sym_get!(dfs_percent, |s| Value::Char(s.percent as u16));
sym_get!(dfs_per_mill, |s| Value::Char(s.per_mill as u16));
sym_get!(dfs_zero, |s| Value::Char(s.zero as u16));
sym_get!(dfs_currency, |s| currency(&s.intl_currency).map(|c| text_value(TextObj::Currency(c))).unwrap_or(Value::Null));

sym_set!(dfs_set_currency_symbol, |s, v| s.currency_symbol = v.to_java_string());
sym_set!(dfs_set_decimal, |s, v| s.decimal = char_of(&v));
sym_set!(dfs_set_digit, |s, v| s.digit = char_of(&v));
sym_set!(dfs_set_exponent, |s, v| s.exponent = v.to_java_string());
sym_set!(dfs_set_grouping, |s, v| s.grouping = char_of(&v));
sym_set!(dfs_set_infinity, |s, v| s.infinity = v.to_java_string());
sym_set!(dfs_set_minus, |s, v| {
    s.minus = char_of(&v);
    s.minus_text = s.minus.to_string();
});
sym_set!(dfs_set_monetary_decimal, |s, v| s.monetary_decimal = char_of(&v));
sym_set!(dfs_set_nan, |s, v| s.nan = v.to_java_string());
sym_set!(dfs_set_pattern_sep, |s, v| s.pattern_sep = char_of(&v));
sym_set!(dfs_set_percent, |s, v| {
    s.percent = char_of(&v);
    s.percent_text = s.percent.to_string();
});
sym_set!(dfs_set_per_mill, |s, v| {
    s.per_mill = char_of(&v);
    s.per_mill_text = s.per_mill.to_string();
});
sym_set!(dfs_set_zero, |s, v| s.zero = char_of(&v));

fn dfs_set_intl_currency(_: &mut Vm, a: &mut [Value]) -> VR {
    let code = a[1].to_java_string();
    let mut s = sym_of(&a[0])?.borrow_mut();
    if let Some(c) = currency(&code) {
        s.currency_symbol = currency_symbol(c, &s.locale);
    }
    s.intl_currency = code;
    Ok(Value::Null)
}

fn dfs_set_currency(_: &mut Vm, a: &mut [Value]) -> VR {
    let Some(TextObj::Currency(c)) = text(&a[1]) else { return Err(npe("Cannot invoke \"java.util.Currency.getCurrencyCode()\" because \"currency\" is null")) };
    let mut s = sym_of(&a[0])?.borrow_mut();
    s.intl_currency = c.code.to_string();
    s.currency_symbol = currency_symbol(c, &s.locale);
    Ok(Value::Null)
}

fn dfs_instance0(vm: &mut Vm, a: &mut [Value]) -> VR {
    dfs_new0(vm, a)
}

fn dfs_instance1(vm: &mut Vm, a: &mut [Value]) -> VR {
    dfs_new1(vm, a)
}

// ParsePosition / FieldPosition

fn pp_new(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(text_value(TextObj::ParsePosition { index: Cell::new(int(&a[0])), error: Cell::new(-1) }))
}

fn pp_cells(v: &Value) -> Result<(&Cell<i32>, &Cell<i32>), Thrown> {
    match text(v) {
        Some(TextObj::ParsePosition { index, error }) => Ok((index, error)),
        _ => Err(crate::vm::cce("not a ParsePosition")),
    }
}

fn pp_index(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(pp_cells(&a[0])?.0.get()))
}
fn pp_error(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(pp_cells(&a[0])?.1.get()))
}
fn pp_set_index(_: &mut Vm, a: &mut [Value]) -> VR {
    pp_cells(&a[0])?.0.set(int(&a[1]));
    Ok(Value::Null)
}
fn pp_set_error(_: &mut Vm, a: &mut [Value]) -> VR {
    pp_cells(&a[0])?.1.set(int(&a[1]));
    Ok(Value::Null)
}

fn fp_new1(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(text_value(TextObj::FieldPosition { field: int(&a[0]), attribute: None, begin: Cell::new(0), end: Cell::new(0) }))
}

fn fp_new2(_: &mut Vm, a: &mut [Value]) -> VR {
    let attribute = match text(&a[0]) {
        Some(TextObj::NumberField(i)) => Some(*i),
        _ => None,
    };
    Ok(text_value(TextObj::FieldPosition { field: int(&a[1]), attribute, begin: Cell::new(0), end: Cell::new(0) }))
}

/// A `FieldPosition`'s field, attribute, begin and end.
type FieldParts<'a> = (i32, Option<u8>, &'a Cell<i32>, &'a Cell<i32>);

fn fp_parts(v: &Value) -> Result<FieldParts<'_>, Thrown> {
    match text(v) {
        Some(TextObj::FieldPosition { field, attribute, begin, end }) => Ok((*field, *attribute, begin, end)),
        _ => Err(crate::vm::cce("not a FieldPosition")),
    }
}

fn fp_begin(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(fp_parts(&a[0])?.2.get()))
}
fn fp_end(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(fp_parts(&a[0])?.3.get()))
}
fn fp_field(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(fp_parts(&a[0])?.0))
}
fn fp_attribute(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(fp_parts(&a[0])?.1.map(|i| text_value(TextObj::NumberField(i))).unwrap_or(Value::Null))
}
fn fp_set_begin(_: &mut Vm, a: &mut [Value]) -> VR {
    fp_parts(&a[0])?.2.set(int(&a[1]));
    Ok(Value::Null)
}
fn fp_set_end(_: &mut Vm, a: &mut [Value]) -> VR {
    fp_parts(&a[0])?.3.set(int(&a[1]));
    Ok(Value::Null)
}

// Normalizer

const FORMS: [&str; 4] = ["NFD", "NFC", "NFKD", "NFKC"];

fn form_value(o: u8) -> Value {
    crate::time::enum_value(cls::NORMALIZER_FORM, o)
}

fn form_of(v: &Value) -> Result<u8, Thrown> {
    crate::time::enum_ordinal(v).ok_or_else(|| npe("Cannot invoke \"java.text.Normalizer$Form.ordinal()\" because \"form\" is null"))
}

fn normalize_str(s: &str, form: u8) -> String {
    use unicode_normalization::UnicodeNormalization;
    match form {
        0 => s.nfd().collect(),
        1 => s.nfc().collect(),
        2 => s.nfkd().collect(),
        _ => s.nfkc().collect(),
    }
}

fn norm_normalize(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        return Err(jdk(npe("Cannot invoke \"java.lang.CharSequence.toString()\" because \"src\" is null"), &["text.Normalizer.normalize(Normalizer.java:161)"]));
    }
    let s = crate::lib_lang::char_seq(&a[0])?;
    Ok(Value::string(normalize_str(&s, form_of(&a[1])?)))
}

fn norm_is_normalized(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        return Err(npe("Cannot invoke \"java.lang.CharSequence.toString()\" because \"src\" is null"));
    }
    let s = crate::lib_lang::char_seq(&a[0])?;
    Ok(Value::Bool(normalize_str(&s, form_of(&a[1])?) == s))
}

macro_rules! form_field {
    ($name:ident, $o:expr) => {
        fn $name(_: &mut Vm, _: &mut [Value]) -> VR {
            Ok(form_value($o))
        }
    };
}
form_field!(form_nfd, 0);
form_field!(form_nfc, 1);
form_field!(form_nfkd, 2);
form_field!(form_nfkc, 3);

fn form_values(_: &mut Vm, _: &mut [Value]) -> VR {
    let items: Vec<Value> = (0..4).map(form_value).collect();
    Ok(Value::obj(Obj::Array(crate::value::JArray::from_data(crate::types::Ty::of(cls::NORMALIZER_FORM), crate::value::ArrayData::Ref(items)))))
}

fn form_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = a[0].to_java_string();
    match FORMS.iter().position(|f| *f == s) {
        Some(i) => Ok(form_value(i as u8)),
        None => Err(jdk(crate::vm::iae(format!("No enum constant java.text.Normalizer.Form.{s}")), &["lang.Enum.valueOf(Enum.java:293)", "text.Normalizer$Form.valueOf(Normalizer.java:123)"])),
    }
}

// NumberFormat.Field

fn number_field(i: u8) -> Value {
    text_value(TextObj::NumberField(i))
}

macro_rules! nf_field {
    ($($name:ident = $i:expr),*) => {
        $(fn $name(_: &mut Vm, _: &mut [Value]) -> VR { Ok(number_field($i)) })*
    };
}
nf_field!(field_integer = 0, field_fraction = 1, field_exponent = 2, field_decimal = 3, field_sign = 4, field_grouping = 5, field_exp_symbol = 6, field_percent = 7, field_permille = 8, field_currency = 9, field_exp_sign = 10);

// Currency

/// `Currency.getSymbol(locale)`: the locale's own symbol for its currency, else the code
/// (`US$` style symbols of other regions are approximated by the default locale's).
fn currency_symbol(c: &CurrencyInfo, locale: &str) -> String {
    let p = profile(locale);
    if p[12] == c.code {
        return p[11].to_string();
    }
    c.symbol.to_string()
}

fn currency_of(v: &Value) -> Result<&'static CurrencyInfo, Thrown> {
    match text(v) {
        Some(TextObj::Currency(c)) => Ok(c),
        _ => Err(crate::vm::cce("not a Currency")),
    }
}

fn cur_code(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::str(currency_of(&a[0])?.code))
}
fn cur_digits(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(currency_of(&a[0])?.digits))
}
fn cur_numeric(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(currency_of(&a[0])?.numeric))
}
fn cur_name(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::str(currency_of(&a[0])?.name))
}
fn cur_symbol(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(currency_symbol(currency_of(&a[0])?, DEFAULT_LOCALE)))
}
fn cur_symbol_l(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = currency_of(&a[0])?;
    let l = locale_arg(&a[1])?;
    Ok(Value::string(currency_symbol(c, &l)))
}
fn cur_instance(_: &mut Vm, a: &mut [Value]) -> VR {
    let code = match &a[0] {
        Value::Str(s) => s.as_str().to_string(),
        Value::Null => {
            return Err(jdk(
                npe("Cannot invoke \"Object.hashCode()\" because \"key\" is null"),
                &["util.concurrent.ConcurrentHashMap.get(ConcurrentHashMap.java:951)", "util.Currency.getInstance(Currency.java:308)", "util.Currency.getInstance(Currency.java:300)"],
            ))
        }
        other => other.to_java_string(),
    };
    match currency(&code) {
        Some(c) => Ok(text_value(TextObj::Currency(c))),
        None => Err(jdk(
            crate::vm::iae(format!("The input currency code: \"{code}\" is not a valid ISO 4217 code")),
            &["util.Currency.getInstance(Currency.java:344)", "util.Currency.getInstance(Currency.java:300)"],
        )),
    }
}
fn cur_available(_: &mut Vm, _: &mut [Value]) -> VR {
    let set = crate::value::JMap::new(cls::HASH_SET);
    for c in &data().currencies {
        set.insert_raw(text_value(TextObj::Currency(c)), Value::Null);
    }
    Ok(Value::Set(Rc::new(set)))
}

// ParseException

fn pe_new(_: &mut Vm, a: &mut [Value]) -> VR {
    let message = if a[0].is_null() { None } else { Some(a[0].to_java_string()) };
    let mut e = crate::value::JException::new(cls::TEXT_PARSE_EXCEPTION, message);
    e.error_offset = Some(int(&a[1]));
    Ok(Value::obj(Obj::Exception(e)))
}

fn pe_offset(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Obj(o) => match &**o {
            Obj::Exception(e) => Ok(Value::Int(e.error_offset.unwrap_or(0))),
            _ => Err(crate::vm::cce("not a ParseException")),
        },
        _ => Err(npe("ParseException")),
    }
}

pub fn register(b: &mut Builder) {
    message::register(b);
    use cls::*;
    b.supers(FORMAT, &[OBJECT]);
    b.supers(NUMBER_FORMAT, &[FORMAT, OBJECT]);
    b.supers(DECIMAL_FORMAT, &[NUMBER_FORMAT, FORMAT, OBJECT]);
    b.supers(DECIMAL_FORMAT_SYMBOLS, &[OBJECT]);
    b.supers(PARSE_POSITION, &[OBJECT]);
    b.supers(FIELD_POSITION, &[OBJECT]);
    b.supers(FORMAT_FIELD, &[OBJECT]);
    b.supers(NUMBER_FORMAT_FIELD, &[FORMAT_FIELD, OBJECT]);
    b.supers(NORMALIZER, &[OBJECT]);
    b.supers(NORMALIZER_FORM, &[ENUM, COMPARABLE, OBJECT]);
    b.supers(CURRENCY, &[OBJECT]);

    b.m(FORMAT, "def clone()", fmt_clone);
    b.m(FORMAT, "String format(Object)", fmt_format);
    b.m(FORMAT, "StringBuffer format(Object,StringBuffer,FieldPosition)", fmt_format3);
    b.m(FORMAT, "Object parseObject(String)", fmt_parse_object);
    b.m(FORMAT, "Object parseObject(String,ParsePosition)", fmt_parse_object2);

    b.sf(NUMBER_FORMAT, "int INTEGER_FIELD", nf_integer_field);
    b.sf(NUMBER_FORMAT, "int FRACTION_FIELD", nf_fraction_field);
    b.s(NUMBER_FORMAT, "Locale[] getAvailableLocales()", available_locales);
    b.m(NUMBER_FORMAT, "Currency getCurrency()", nf_currency);
    b.s(NUMBER_FORMAT, "NumberFormat getCurrencyInstance()", nf_currency_inst);
    b.s(NUMBER_FORMAT, "NumberFormat getCurrencyInstance(Locale)", nf_currency_inst_l);
    b.s(NUMBER_FORMAT, "NumberFormat getInstance()", nf_instance);
    b.s(NUMBER_FORMAT, "NumberFormat getInstance(Locale)", nf_instance_l);
    b.s(NUMBER_FORMAT, "NumberFormat getIntegerInstance()", nf_integer);
    b.s(NUMBER_FORMAT, "NumberFormat getIntegerInstance(Locale)", nf_integer_l);
    b.s(NUMBER_FORMAT, "NumberFormat getNumberInstance()", nf_number);
    b.s(NUMBER_FORMAT, "NumberFormat getNumberInstance(Locale)", nf_number_l);
    b.s(NUMBER_FORMAT, "NumberFormat getPercentInstance()", nf_percent);
    b.s(NUMBER_FORMAT, "NumberFormat getPercentInstance(Locale)", nf_percent_l);
    b.m(NUMBER_FORMAT, "int getMaximumFractionDigits()", nf_max_frac);
    b.m(NUMBER_FORMAT, "int getMaximumIntegerDigits()", nf_max_int);
    b.m(NUMBER_FORMAT, "int getMinimumFractionDigits()", nf_min_frac);
    b.m(NUMBER_FORMAT, "int getMinimumIntegerDigits()", nf_min_int);
    b.m(NUMBER_FORMAT, "RoundingMode getRoundingMode()", nf_rounding);
    b.m(NUMBER_FORMAT, "boolean isGroupingUsed()", nf_grouping_used);
    b.m(NUMBER_FORMAT, "boolean isParseIntegerOnly()", nf_integer_only);
    b.m(NUMBER_FORMAT, "Number parse(String)", nf_parse);
    b.m(NUMBER_FORMAT, "Number parse(String,ParsePosition)", nf_parse2);
    b.m(NUMBER_FORMAT, "void setCurrency(Currency)", nf_set_currency);
    b.m(NUMBER_FORMAT, "void setGroupingUsed(boolean)", nf_set_grouping_used);
    b.m(NUMBER_FORMAT, "void setMaximumFractionDigits(int)", nf_set_max_frac);
    b.m(NUMBER_FORMAT, "void setMaximumIntegerDigits(int)", nf_set_max_int);
    b.m(NUMBER_FORMAT, "void setMinimumFractionDigits(int)", nf_set_min_frac);
    b.m(NUMBER_FORMAT, "void setMinimumIntegerDigits(int)", nf_set_min_int);
    b.m(NUMBER_FORMAT, "void setParseIntegerOnly(boolean)", nf_set_integer_only);
    b.m(NUMBER_FORMAT, "void setRoundingMode(RoundingMode)", nf_set_rounding);

    b.c(DECIMAL_FORMAT, "()", df_new0);
    b.c(DECIMAL_FORMAT, "(String)", df_new1);
    b.c(DECIMAL_FORMAT, "(String,DecimalFormatSymbols)", df_new2);
    b.m(DECIMAL_FORMAT, "void applyLocalizedPattern(String)", df_apply_localized);
    b.m(DECIMAL_FORMAT, "void applyPattern(String)", df_apply);
    b.m(DECIMAL_FORMAT, "DecimalFormatSymbols getDecimalFormatSymbols()", df_symbols);
    b.m(DECIMAL_FORMAT, "int getGroupingSize()", df_grouping_size);
    b.m(DECIMAL_FORMAT, "int getMultiplier()", df_multiplier);
    b.m(DECIMAL_FORMAT, "String getNegativePrefix()", df_neg_prefix);
    b.m(DECIMAL_FORMAT, "String getNegativeSuffix()", df_neg_suffix);
    b.m(DECIMAL_FORMAT, "String getPositivePrefix()", df_pos_prefix);
    b.m(DECIMAL_FORMAT, "String getPositiveSuffix()", df_pos_suffix);
    b.m(DECIMAL_FORMAT, "boolean isDecimalSeparatorAlwaysShown()", df_decimal_always);
    b.m(DECIMAL_FORMAT, "boolean isParseBigDecimal()", df_parse_big);
    b.m(DECIMAL_FORMAT, "void setDecimalFormatSymbols(DecimalFormatSymbols)", df_set_symbols);
    b.m(DECIMAL_FORMAT, "void setDecimalSeparatorAlwaysShown(boolean)", df_set_decimal_always);
    b.m(DECIMAL_FORMAT, "void setGroupingSize(int)", df_set_grouping_size);
    b.m(DECIMAL_FORMAT, "void setMultiplier(int)", df_set_multiplier);
    b.m(DECIMAL_FORMAT, "void setNegativePrefix(String)", df_set_neg_prefix);
    b.m(DECIMAL_FORMAT, "void setNegativeSuffix(String)", df_set_neg_suffix);
    b.m(DECIMAL_FORMAT, "void setPositivePrefix(String)", df_set_pos_prefix);
    b.m(DECIMAL_FORMAT, "void setPositiveSuffix(String)", df_set_pos_suffix);
    b.m(DECIMAL_FORMAT, "void setParseBigDecimal(boolean)", df_set_parse_big);
    b.m(DECIMAL_FORMAT, "String toLocalizedPattern()", df_to_localized);
    b.m(DECIMAL_FORMAT, "String toPattern()", df_to_pattern);

    b.c(DECIMAL_FORMAT_SYMBOLS, "()", dfs_new0);
    b.c(DECIMAL_FORMAT_SYMBOLS, "(Locale)", dfs_new1);
    b.m(DECIMAL_FORMAT_SYMBOLS, "def clone()", fmt_clone);
    b.s(DECIMAL_FORMAT_SYMBOLS, "Locale[] getAvailableLocales()", available_locales);
    b.m(DECIMAL_FORMAT_SYMBOLS, "Currency getCurrency()", dfs_currency);
    b.m(DECIMAL_FORMAT_SYMBOLS, "String getCurrencySymbol()", dfs_currency_symbol);
    b.m(DECIMAL_FORMAT_SYMBOLS, "char getDecimalSeparator()", dfs_decimal);
    b.m(DECIMAL_FORMAT_SYMBOLS, "char getDigit()", dfs_digit);
    b.m(DECIMAL_FORMAT_SYMBOLS, "String getExponentSeparator()", dfs_exponent);
    b.m(DECIMAL_FORMAT_SYMBOLS, "char getGroupingSeparator()", dfs_grouping);
    b.m(DECIMAL_FORMAT_SYMBOLS, "String getInfinity()", dfs_infinity);
    b.s(DECIMAL_FORMAT_SYMBOLS, "DecimalFormatSymbols getInstance()", dfs_instance0);
    b.s(DECIMAL_FORMAT_SYMBOLS, "DecimalFormatSymbols getInstance(Locale)", dfs_instance1);
    b.m(DECIMAL_FORMAT_SYMBOLS, "String getInternationalCurrencySymbol()", dfs_intl_currency);
    b.m(DECIMAL_FORMAT_SYMBOLS, "char getMinusSign()", dfs_minus);
    b.m(DECIMAL_FORMAT_SYMBOLS, "char getMonetaryDecimalSeparator()", dfs_monetary_decimal);
    b.m(DECIMAL_FORMAT_SYMBOLS, "String getNaN()", dfs_nan);
    b.m(DECIMAL_FORMAT_SYMBOLS, "char getPatternSeparator()", dfs_pattern_sep);
    b.m(DECIMAL_FORMAT_SYMBOLS, "char getPercent()", dfs_percent);
    b.m(DECIMAL_FORMAT_SYMBOLS, "char getPerMill()", dfs_per_mill);
    b.m(DECIMAL_FORMAT_SYMBOLS, "char getZeroDigit()", dfs_zero);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setCurrency(Currency)", dfs_set_currency);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setCurrencySymbol(String)", dfs_set_currency_symbol);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setDecimalSeparator(char)", dfs_set_decimal);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setDigit(char)", dfs_set_digit);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setExponentSeparator(String)", dfs_set_exponent);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setGroupingSeparator(char)", dfs_set_grouping);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setInfinity(String)", dfs_set_infinity);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setInternationalCurrencySymbol(String)", dfs_set_intl_currency);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setMinusSign(char)", dfs_set_minus);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setMonetaryDecimalSeparator(char)", dfs_set_monetary_decimal);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setNaN(String)", dfs_set_nan);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setPatternSeparator(char)", dfs_set_pattern_sep);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setPercent(char)", dfs_set_percent);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setPerMill(char)", dfs_set_per_mill);
    b.m(DECIMAL_FORMAT_SYMBOLS, "void setZeroDigit(char)", dfs_set_zero);

    b.c(PARSE_POSITION, "(int)", pp_new);
    b.m(PARSE_POSITION, "int getErrorIndex()", pp_error);
    b.m(PARSE_POSITION, "int getIndex()", pp_index);
    b.m(PARSE_POSITION, "void setErrorIndex(int)", pp_set_error);
    b.m(PARSE_POSITION, "void setIndex(int)", pp_set_index);

    b.c(FIELD_POSITION, "(int)", fp_new1);
    b.c(FIELD_POSITION, "(Format.Field,int)", fp_new2);
    b.m(FIELD_POSITION, "int getBeginIndex()", fp_begin);
    b.m(FIELD_POSITION, "int getEndIndex()", fp_end);
    b.m(FIELD_POSITION, "int getField()", fp_field);
    b.m(FIELD_POSITION, "Format.Field getFieldAttribute()", fp_attribute);
    b.m(FIELD_POSITION, "void setBeginIndex(int)", fp_set_begin);
    b.m(FIELD_POSITION, "void setEndIndex(int)", fp_set_end);

    for (name, f) in [
        ("INTEGER", field_integer as crate::lookup::Native),
        ("FRACTION", field_fraction),
        ("EXPONENT", field_exponent),
        ("DECIMAL_SEPARATOR", field_decimal),
        ("SIGN", field_sign),
        ("GROUPING_SEPARATOR", field_grouping),
        ("EXPONENT_SYMBOL", field_exp_symbol),
        ("PERCENT", field_percent),
        ("PERMILLE", field_permille),
        ("CURRENCY", field_currency),
        ("EXPONENT_SIGN", field_exp_sign),
    ] {
        b.sf(NUMBER_FORMAT_FIELD, &format!("NumberFormat.Field {name}"), f);
    }

    b.s(NORMALIZER, "boolean isNormalized(CharSequence,Normalizer.Form)", norm_is_normalized);
    b.s(NORMALIZER, "String normalize(CharSequence,Normalizer.Form)", norm_normalize);
    b.sf(NORMALIZER_FORM, "Normalizer.Form NFC", form_nfc);
    b.sf(NORMALIZER_FORM, "Normalizer.Form NFD", form_nfd);
    b.sf(NORMALIZER_FORM, "Normalizer.Form NFKC", form_nfkc);
    b.sf(NORMALIZER_FORM, "Normalizer.Form NFKD", form_nfkd);
    b.s(NORMALIZER_FORM, "Normalizer.Form valueOf(String)", form_value_of);
    b.s(NORMALIZER_FORM, "Normalizer.Form[] values()", form_values);

    b.s(CURRENCY, "Set getAvailableCurrencies()", cur_available);
    b.m(CURRENCY, "String getCurrencyCode()", cur_code);
    b.m(CURRENCY, "int getDefaultFractionDigits()", cur_digits);
    b.m(CURRENCY, "String getDisplayName()", cur_name);
    b.m(CURRENCY, "String getDisplayName(Locale)", cur_name);
    b.s(CURRENCY, "Currency getInstance(String)", cur_instance);
    b.m(CURRENCY, "int getNumericCode()", cur_numeric);
    b.m(CURRENCY, "String getSymbol()", cur_symbol);
    b.m(CURRENCY, "String getSymbol(Locale)", cur_symbol_l);

    b.c(TEXT_PARSE_EXCEPTION, "(String,int)", pe_new);
    b.m(TEXT_PARSE_EXCEPTION, "int getErrorOffset()", pe_offset);
}
