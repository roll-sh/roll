//! Value display, as cqlsh shows what the Python driver decoded.
//!
//! Ported from Apache Cassandra's `pylib/cqlshlib/formatting.py` and `displaying.py`
//! (Apache-2.0), with the driver's decoding folded in (`cassandra.cqltypes`, bundled with
//! Cassandra under the same licence): sets and map keys sorted by Python's ordering, nested
//! nulls `None` (`null` in UDT fields), empty values of non-string types blank, timestamps
//! through float seconds. The width cqlsh pads with is kept separately from the text, bugs
//! included (empty collections count as zero columns wide).

use std::cmp::Ordering;
use std::sync::Arc;

use nosql_proto::ColType;
use nosql_types::{CqlType, UdtDef};
use unicode_width::UnicodeWidthStr;

use super::pyfmt::{self, Zone};

/// Which cqlsh is being reproduced: Cassandra's 6.2 or the one in the ScyllaDB image (6.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flavor {
    Cassandra,
    Scylla,
}

pub const RED: &str = "\x1b[0;1;31m";
pub const GREEN: &str = "\x1b[0;1;32m";
pub const YELLOW: &str = "\x1b[0;1;33m";
pub const BLUE: &str = "\x1b[0;1;34m";
pub const CYAN: &str = "\x1b[0;1;36m";
pub const WHITE: &str = "\x1b[0;1;37m";
pub const MAGENTA: &str = "\x1b[0;1;35m";
pub const DARK_MAGENTA: &str = "\x1b[0;35m";
pub const RESET: &str = "\x1b[0m";

/// `displaying.FormattedValue`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fv {
    pub text: String,
    pub colored: String,
    /// What cqlsh pads by; can disagree with the text (see the module docs).
    pub width: isize,
}

impl Fv {
    pub fn plain(text: impl Into<String>) -> Fv {
        let text = text.into();
        let width = text.chars().count() as isize;
        Fv { colored: text.clone(), text, width }
    }

    fn pad(&self, width: usize) -> String {
        let w = width as isize;
        if w > self.width { " ".repeat((w - self.width) as usize) } else { String::new() }
    }

    pub fn ljust(&self, width: usize, color: bool) -> String {
        format!("{}{}", if color { &self.colored } else { &self.text }, self.pad(width))
    }

    pub fn rjust(&self, width: usize, color: bool) -> String {
        format!("{}{}", self.pad(width), if color { &self.colored } else { &self.text })
    }
}

/// Display settings (`cqlshrc [ui]`).
#[derive(Clone, Debug)]
pub struct Opts {
    pub flavor: Flavor,
    pub color: bool,
    pub float_precision: usize,
    pub double_precision: usize,
    pub zone: Zone,
    pub nullval: String,
    /// COPY TO: no colours or widths, timestamps to the millisecond.
    pub copy: bool,
    pub decimal_sep: Option<String>,
    pub thousands_sep: Option<String>,
    pub bool_styles: Option<(String, String)>,
}

impl Opts {
    pub fn new(flavor: Flavor) -> Opts {
        Opts {
            flavor,
            color: false,
            float_precision: 5,
            double_precision: 5,
            zone: Zone::Utc,
            nullval: "null".into(),
            copy: false,
            decimal_sep: None,
            thousands_sep: None,
            bool_styles: None,
        }
    }
}

/// The resolved type of a result column.
pub fn cql_type(t: &ColType) -> CqlType {
    use ColType as C;
    match t {
        C::Custom(c) => match nosql_types::parse_marshal(c) {
            Ok((ty, _)) => ty,
            Err(_) => CqlType::Custom(Arc::from(c.as_str())),
        },
        C::Ascii => CqlType::Ascii,
        C::Bigint => CqlType::Bigint,
        C::Blob => CqlType::Blob,
        C::Boolean => CqlType::Boolean,
        C::Counter => CqlType::Counter,
        C::Decimal => CqlType::Decimal,
        C::Double => CqlType::Double,
        C::Float => CqlType::Float,
        C::Int => CqlType::Int,
        C::Timestamp => CqlType::Timestamp,
        C::Uuid => CqlType::Uuid,
        C::Varchar => CqlType::Text,
        C::Varint => CqlType::Varint,
        C::Timeuuid => CqlType::Timeuuid,
        C::Inet => CqlType::Inet,
        C::Date => CqlType::Date,
        C::Time => CqlType::Time,
        C::Smallint => CqlType::Smallint,
        C::Tinyint => CqlType::Tinyint,
        C::Duration => CqlType::Duration,
        C::List(e) => CqlType::list(cql_type(e), true),
        C::Set(e) => CqlType::set(cql_type(e), true),
        C::Map(k, v) => CqlType::map(cql_type(k), cql_type(v), true),
        C::Udt { keyspace, name, fields } => CqlType::Udt(
            Arc::new(UdtDef::new(keyspace.clone(), name.clone(), fields.iter().map(|(n, t)| (n.clone(), cql_type(t))).collect())),
            true,
        ),
        C::Tuple(ts) => CqlType::Tuple(ts.iter().map(cql_type).collect()),
    }
}

fn colorme(text: String, key: &str, o: &Opts) -> Fv {
    let width = text.chars().count() as isize;
    let colored = if o.color { format!("{}{text}{RESET}", value_color(key)) } else { text.clone() };
    Fv { text, colored, width }
}

fn value_color(key: &str) -> &'static str {
    match key {
        "error" => RED,
        "blob" => DARK_MAGENTA,
        "collection" => BLUE,
        "text" | "default" => YELLOW,
        _ => GREEN,
    }
}

/// Escapes text as `format_value_text` does: backslashes doubled, control characters as
/// Python escapes, quotes doubled and wrapped when `quote`.
pub fn escape_text(s: &str, quote: bool) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    if quote {
        out.push('\'');
    }
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' if quote => out.push_str("''"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{a0}' => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    if quote {
        out.push('\'');
    }
    out
}

fn text_fv(s: &str, quote: bool, o: &Opts) -> Fv {
    let text = escape_text(s, quote);
    let mut width = UnicodeWidthStr::width(text.as_str()) as isize;
    let colored = if o.color {
        // `color_text`: escapes in the blob colour, doubled backslashes shown single (and
        // counted single).
        width -= text.matches("\\\\").count() as isize;
        format!("{YELLOW}{}{RESET}", color_escapes(&text))
    } else {
        text.clone()
    };
    Fv { text, colored, width }
}

/// `bits_to_turn_red_re` substitution of `color_text`.
fn color_escapes(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let hex = |from: usize, n: usize| from + n <= c.len() && c[from..from + n].iter().all(|x| x.is_ascii_hexdigit());
    while i < c.len() {
        if c[i] != '\\' || i + 1 >= c.len() {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let n = match c[i + 1] {
            'u' if hex(i + 2, 4) => 6,
            'x' if hex(i + 2, 2) => 4,
            'U' if hex(i + 2, 8) => 10,
            'u' | 'x' | 'U' => 0,
            _ => 2,
        };
        if n == 0 {
            out.push(c[i]);
            i += 1;
            continue;
        }
        let m: String = c[i..i + n].iter().collect();
        if m == "\\\\" {
            out.push('\\');
        } else {
            out.push_str(&format!("{DARK_MAGENTA}{m}{YELLOW}"));
        }
        i += n;
    }
    out
}

/// The colour key of a type's formatter.
fn color_key(ty: &CqlType) -> &'static str {
    use CqlType as T;
    match ty {
        T::Ascii | T::Text => "text",
        T::Blob | T::Custom(_) | T::Empty => "blob",
        T::List { .. } | T::Set { .. } | T::Map { .. } | T::Tuple(_) | T::Udt(..) | T::Vector { .. } => "collection",
        _ => "int",
    }
}

/// cqlsh's `format_floating_point_type`.
pub fn float_text(x: f64, precision: usize, o: &Opts) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if let Some(sep) = &o.thousands_sep {
        let ip = x.trunc();
        let dp = (x - ip).abs();
        let mut s = thousands(&format!("{:.0}", ip.abs()), sep);
        if ip < 0.0 || (ip == 0.0 && x.is_sign_negative() && x != 0.0) {
            s.insert(0, '-');
        }
        let d = format!("{:.*}", precision, dp);
        let d = d.get(2..).unwrap_or("").trim_end_matches('0');
        if !d.is_empty() {
            s.push_str(o.decimal_sep.as_deref().unwrap_or("."));
            s.push_str(d);
        }
        return s;
    }
    let mut p = precision as i64;
    if x.abs() > f64::EPSILON {
        let exponent = x.abs().log10() as i64;
        if -4 <= exponent && exponent < p {
            p += exponent + 1;
        }
    }
    let s = pyfmt::percent_g(x, p.max(0) as usize);
    match &o.decimal_sep {
        Some(sep) => s.replace('.', sep),
        None => s,
    }
}

fn thousands(digits: &str, sep: &str) -> String {
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push_str(sep);
        }
        out.push(c);
    }
    out
}

/// cqlsh's `duration_as_str`; ScyllaDB's cqlsh divides with `/`, printing floats.
pub fn duration_text(months: i64, days: i64, nanos: i64, flavor: Flavor) -> String {
    let mut b = String::new();
    if months < 0 || days < 0 || nanos < 0 {
        b.push('-');
    }
    let scylla = flavor == Flavor::Scylla;
    let append = |b: &mut String, dividend: i64, divisor: i64, unit: &str| -> i64 {
        if dividend == 0 || dividend < divisor {
            return dividend;
        }
        if scylla {
            b.push_str(&pyfmt::float_repr(dividend as f64 / divisor as f64));
        } else {
            b.push_str(&(dividend / divisor).to_string());
        }
        b.push_str(unit);
        dividend % divisor
    };
    let rem = append(&mut b, months.abs(), 12, "y");
    append(&mut b, rem, 1, "mo");
    append(&mut b, days.abs(), 1, "d");
    if nanos != 0 {
        let mut r = append(&mut b, nanos.abs(), 3_600_000_000_000, "h");
        r = append(&mut b, r, 60_000_000_000, "m");
        r = append(&mut b, r, 1_000_000_000, "s");
        r = append(&mut b, r, 1_000_000, "ms");
        r = append(&mut b, r, 1000, "us");
        append(&mut b, r, 1, "ns");
    }
    b
}

fn be<const N: usize>(b: &[u8]) -> Option<[u8; N]> {
    b.try_into().ok()
}

/// Python's ordering of decoded values, for sets and map keys.
fn py_cmp(ty: &CqlType, a: &[u8], b: &[u8]) -> Ordering {
    use CqlType as T;
    match ty {
        T::Uuid | T::Timeuuid | T::Blob | T::Ascii | T::Text | T::Custom(_) => a.cmp(b),
        T::Inet => {
            let f = |x: &[u8]| nosql_types::inet::format_inet(x, nosql_types::Dialect::Scylla).unwrap_or_default();
            f(a).cmp(&f(b))
        }
        T::Tuple(ts) => {
            let (fa, fb) = (nosql_types::tuple_fields(a).unwrap_or_default(), nosql_types::tuple_fields(b).unwrap_or_default());
            for (i, t) in ts.iter().enumerate() {
                let o = match (fa.get(i).copied().flatten(), fb.get(i).copied().flatten()) {
                    (Some(x), Some(y)) => py_cmp(t, x, y),
                    (x, y) => x.is_some().cmp(&y.is_some()),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            Ordering::Equal
        }
        T::Udt(def, _) => {
            let tt = T::Tuple(def.fields.iter().map(|(_, t)| t.clone()).collect());
            py_cmp(&tt, a, b)
        }
        T::List { elem, .. } | T::Set { elem, .. } => {
            let (ea, eb) = (nosql_types::collection_elements(a).unwrap_or_default(), nosql_types::collection_elements(b).unwrap_or_default());
            for (x, y) in ea.iter().zip(eb.iter()) {
                let o = match (x, y) {
                    (Some(x), Some(y)) => py_cmp(elem, x, y),
                    (x, y) => x.is_some().cmp(&y.is_some()),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
            ea.len().cmp(&eb.len())
        }
        _ => nosql_types::compare(ty, a, b),
    }
}

/// Formats one value (`None` is null) of type `ty`.
pub fn format(ty: &CqlType, v: Option<&[u8]>, o: &Opts) -> Fv {
    match v {
        None => colorme(o.nullval.clone(), "error", o),
        Some(b) => value(ty, b, false, o),
    }
}

fn nested(ty: &CqlType, v: Option<&[u8]>, o: &Opts) -> Fv {
    match v {
        // `format_value(None)` with the subtype's formatter: `str(None)`.
        None => colorme("None".into(), color_key(ty), o),
        Some(b) => value(ty, b, true, o),
    }
}

fn join(open: &str, close: &str, subs: &[Fv], o: &Opts) -> Fv {
    let text = format!("{open}{}{close}", subs.iter().map(|s| s.text.as_str()).collect::<Vec<_>>().join(", "));
    let width = 2 * subs.len() as isize + subs.iter().map(|s| s.width).sum::<isize>();
    let colored = if o.color {
        let sep = format!("{BLUE}, {RESET}");
        format!(
            "{BLUE}{open}{RESET}{}{BLUE}{close}{RESET}",
            subs.iter().map(|s| s.colored.as_str()).collect::<Vec<_>>().join(&sep)
        )
    } else {
        text.clone()
    };
    Fv { text, colored, width }
}

fn pairs(subs: &[(Fv, Fv)], o: &Opts) -> Fv {
    let text = format!("{{{}}}", subs.iter().map(|(k, v)| format!("{}: {}", k.text, v.text)).collect::<Vec<_>>().join(", "));
    let width = 4 * subs.len() as isize + subs.iter().map(|(k, v)| k.width + v.width).sum::<isize>();
    let colored = if o.color {
        let parts: Vec<String> = subs.iter().map(|(k, v)| format!("{}{BLUE}: {RESET}{}", k.colored, v.colored)).collect();
        format!("{BLUE}{{{RESET}{}{BLUE}}}{RESET}", parts.join(&format!("{BLUE}, {RESET}")))
    } else {
        text.clone()
    };
    Fv { text, colored, width }
}

fn bad(b: &[u8], o: &Opts) -> Fv {
    colorme(format!("0x{}", hex(b)), "error", o)
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn value(ty: &CqlType, b: &[u8], quote: bool, o: &Opts) -> Fv {
    use CqlType as T;
    // The driver returns EMPTY for empty values of every type but strings and blobs.
    if b.is_empty() && !matches!(ty, T::Ascii | T::Text | T::Blob | T::Custom(_)) {
        return colorme(String::new(), "text", o);
    }
    match ty {
        T::Ascii | T::Text => text_fv(&String::from_utf8_lossy(b), quote, o),
        T::Blob | T::Custom(_) | T::Empty => colorme(format!("0x{}", hex(b)), "blob", o),
        T::Boolean => match b {
            [x] => {
                let t = *x != 0;
                let s = match &o.bool_styles {
                    Some((y, n)) => if t { y.clone() } else { n.clone() },
                    None => if t { "True" } else { "False" }.to_string(),
                };
                colorme(s, "boolean", o)
            }
            _ => bad(b, o),
        },
        T::Tinyint => be::<1>(b).map(|a| int_fv((a[0] as i8).to_string(), o)).unwrap_or_else(|| bad(b, o)),
        T::Smallint => be::<2>(b).map(|a| int_fv(i16::from_be_bytes(a).to_string(), o)).unwrap_or_else(|| bad(b, o)),
        T::Int => be::<4>(b).map(|a| int_fv(i32::from_be_bytes(a).to_string(), o)).unwrap_or_else(|| bad(b, o)),
        T::Bigint | T::Counter => be::<8>(b).map(|a| int_fv(i64::from_be_bytes(a).to_string(), o)).unwrap_or_else(|| bad(b, o)),
        T::Varint => int_fv(nosql_types::num::varint_from_bytes(b).to_string(), o),
        T::Float => match be::<4>(b) {
            Some(a) => colorme(float_text(f32::from_be_bytes(a) as f64, o.float_precision, o), "float", o),
            None => bad(b, o),
        },
        T::Double => match be::<8>(b) {
            Some(a) => colorme(float_text(f64::from_be_bytes(a), o.double_precision, o), "float", o),
            None => bad(b, o),
        },
        T::Decimal => match nosql_types::Decimal::from_bytes(b) {
            Some(d) => {
                let s = d.to_java_string();
                let s = if o.decimal_sep.is_some() || o.thousands_sep.is_some() {
                    match s.parse::<f64>() {
                        Ok(x) => float_text(x, o.float_precision, o),
                        Err(_) => s,
                    }
                } else {
                    s
                };
                colorme(s, "decimal", o)
            }
            None => bad(b, o),
        },
        T::Timestamp => match be::<8>(b) {
            Some(a) => {
                let s = pyfmt::cqlsh_timestamp(i64::from_be_bytes(a), o.zone, o.copy);
                colorme(if quote { format!("'{s}'") } else { s }, "timestamp", o)
            }
            None => bad(b, o),
        },
        T::Date => match be::<4>(b) {
            Some(a) => colorme(nosql_types::temporal::format_date_cqlsh(u32::from_be_bytes(a)), "date", o),
            None => bad(b, o),
        },
        T::Time => match be::<8>(b) {
            Some(a) => colorme(nosql_types::temporal::format_time(i64::from_be_bytes(a)), "time", o),
            None => bad(b, o),
        },
        T::Uuid | T::Timeuuid => match be::<16>(b) {
            Some(a) => colorme(nosql_types::timeuuid::format_uuid(&a), "uuid", o),
            None => bad(b, o),
        },
        T::Inet => match nosql_types::inet::format_inet(b, nosql_types::Dialect::Scylla) {
            Some(s) => colorme(if quote { format!("'{s}'") } else { s }, "inet", o),
            None => bad(b, o),
        },
        T::Duration => match nosql_types::Duration::decode(b) {
            Some(d) => colorme(duration_text(d.months as i64, d.days as i64, d.nanos, o.flavor), "duration", o),
            None => bad(b, o),
        },
        T::List { elem, .. } => match nosql_types::collection_elements(b) {
            Ok(es) => join("[", "]", &es.iter().map(|e| nested(elem, *e, o)).collect::<Vec<_>>(), o),
            Err(_) => bad(b, o),
        },
        T::Set { elem, .. } => match nosql_types::collection_elements(b) {
            Ok(mut es) => {
                es.sort_by(|x, y| match (x, y) {
                    (Some(x), Some(y)) => py_cmp(elem, x, y),
                    (x, y) => x.is_some().cmp(&y.is_some()),
                });
                join("{", "}", &es.iter().map(|e| nested(elem, *e, o)).collect::<Vec<_>>(), o)
            }
            Err(_) => bad(b, o),
        },
        T::Map { key, value: vt, .. } => match nosql_types::map_entries(b) {
            Ok(mut es) => {
                es.sort_by(|x, y| py_cmp(key, x.0, y.0).then_with(|| py_cmp(vt, x.1, y.1)));
                pairs(&es.iter().map(|(k, v)| (nested(key, Some(k), o), nested(vt, Some(v), o))).collect::<Vec<_>>(), o)
            }
            Err(_) => bad(b, o),
        },
        T::Tuple(ts) => match nosql_types::tuple_fields(b) {
            Ok(fs) => {
                let subs: Vec<Fv> = ts.iter().enumerate().map(|(i, t)| nested(t, fs.get(i).copied().flatten(), o)).collect();
                join("(", ")", &subs, o)
            }
            Err(_) => bad(b, o),
        },
        T::Udt(def, _) => match nosql_types::tuple_fields(b) {
            Ok(fs) => {
                let subs: Vec<(Fv, Fv)> = def
                    .fields
                    .iter()
                    .enumerate()
                    .map(|(i, (n, t))| {
                        let v = match fs.get(i).copied().flatten() {
                            Some(v) => value(t, v, true, o),
                            None => colorme(o.nullval.clone(), "error", o),
                        };
                        (text_fv(n, false, o), v)
                    })
                    .collect();
                pairs(&subs, o)
            }
            Err(_) => bad(b, o),
        },
        T::Vector { elem, dim } if o.flavor == Flavor::Scylla && *dim != 2 => match nosql_types::vector_elements(elem, *dim, b) {
            // ScyllaDB's cqlsh prints the driver's list as Python's `str(list)`.
            Ok(es) => colorme(format!("[{}]", es.iter().map(|e| py_value(elem, e)).collect::<Vec<_>>().join(", ")), "collection", o),
            Err(_) => bad(b, o),
        },
        T::Vector { elem, dim } => match nosql_types::vector_elements(elem, *dim, b) {
            Ok(es) => join("[", "]", &es.iter().map(|e| nested(elem, Some(e), o)).collect::<Vec<_>>(), o),
            Err(_) => bad(b, o),
        },
    }
}

/// ScyllaDB's cqlsh cannot format vectors (it reads `vector<float, 3>` as two subtypes): the
/// decoding error it reports after the rows, if this value has one.
pub fn format_error(ty: &CqlType, v: Option<&[u8]>, o: &Opts) -> Option<String> {
    match (ty, v) {
        (CqlType::Vector { elem, dim }, Some(b)) if o.flavor == Flavor::Scylla && *dim != 2 => Some(format!(
            "Failed to format value {} : Unexpected number of subtypes {dim} - [{}, {dim}]",
            value(ty, b, false, o).text,
            nosql_types::cql_name(elem)
        )),
        _ => None,
    }
}

/// Python's `repr` of a decoded scalar.
fn py_value(ty: &CqlType, b: &[u8]) -> String {
    use CqlType as T;
    match ty {
        T::Float => be::<4>(b).map(|a| pyfmt::float_repr(f32::from_be_bytes(a) as f64)).unwrap_or_default(),
        T::Double => be::<8>(b).map(|a| pyfmt::float_repr(f64::from_be_bytes(a))).unwrap_or_default(),
        T::Ascii | T::Text => super::session::py_repr(&String::from_utf8_lossy(b)),
        T::Boolean => if b.first().is_some_and(|x| *x != 0) { "True" } else { "False" }.into(),
        _ => value(ty, b, false, &Opts::new(Flavor::Scylla)).text,
    }
}

fn int_fv(s: String, o: &Opts) -> Fv {
    let s = match &o.thousands_sep {
        Some(sep) => {
            let (neg, d) = s.strip_prefix('-').map(|d| (true, d)).unwrap_or((false, &s));
            format!("{}{}", if neg { "-" } else { "" }, thousands(d, sep))
        }
        None => s,
    };
    colorme(s, "int", o)
}

/// A column name (`myformat_colname`, without the key-role colours).
pub fn colname(name: &str, o: &Opts) -> Fv {
    colname_as(name, "", o)
}

/// A column name coloured by its role in the table (`partition_key`, `clustering`, `static`).
pub fn colname_as(name: &str, kind: &str, o: &Opts) -> Fv {
    let mut f = text_fv(name, false, o);
    if o.color {
        let c = match kind {
            "partition_key" => RED,
            "clustering" => CYAN,
            "static" => WHITE,
            _ => MAGENTA,
        };
        f.colored = format!("{c}{}{RESET}", f.text);
    }
    f
}

/// A string shown by type (`format_value` of a `str`): trace rows and the like.
pub fn text_value(s: &str, o: &Opts) -> Fv {
    text_fv(s, false, o)
}

/// An integer shown by type.
pub fn int_value(n: i64, o: &Opts) -> Fv {
    int_fv(n.to_string(), o)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosql_types::{Value, encode};

    fn f(ty: &CqlType, v: Value) -> Fv {
        let o = Opts::new(Flavor::Cassandra);
        format(ty, Some(&encode(&v, ty)), &o)
    }

    #[test]
    fn scalars() {
        use CqlType as T;
        assert_eq!(f(&T::Double, Value::Double(std::f64::consts::PI)).text, "3.14159");
        assert_eq!(f(&T::Double, Value::Double(123456789.123)).text, "1.2346e+08");
        assert_eq!(f(&T::Float, Value::Float(1.1)).text, "1.1");
        assert_eq!(f(&T::Float, Value::Float(-0.0)).text, "-0");
        assert_eq!(f(&T::Double, Value::Double(1e300)).text, "1e+300");
        assert_eq!(f(&T::Double, Value::Double(99999.5)).text, "99999.5");
        assert_eq!(f(&T::Float, Value::Float(f32::NAN)).text, "NaN");
        assert_eq!(f(&T::Double, Value::Double(f64::NEG_INFINITY)).text, "-Infinity");
        assert_eq!(f(&T::Boolean, Value::Boolean(true)).text, "True");
        assert_eq!(f(&T::Text, Value::Text("a\\b\tc'".into())).text, "a\\\\b\\tc'");
        assert_eq!(f(&T::Text, Value::Text("héllo".into())).width, 5);
        assert_eq!(f(&T::Text, Value::Text("日本".into())).width, 4);
        assert_eq!(f(&T::Blob, Value::Blob(bytes::Bytes::new())).text, "0x");
        let o = Opts::new(Flavor::Cassandra);
        assert_eq!(format(&T::Int, Some(&[]), &o).text, "");
        assert_eq!(format(&T::Int, None, &o).text, "null");
        assert_eq!(format(&T::Int, Some(&[1]), &o).text, "0x01");
    }

    #[test]
    fn durations() {
        let n = 4 * 3_600_000_000_000 + 5 * 60_000_000_000 + 6_007_008_009;
        assert_eq!(duration_text(14, 3, n, Flavor::Cassandra), "1y2mo3d4h5m6s7ms8us9ns");
        assert_eq!(
            duration_text(14, 3, n, Flavor::Scylla),
            "1.1666666666666667y2.0mo3.0d4.085001946669166h5.10011680015m6.007008009s7.008009ms8.009us9.0ns"
        );
        assert_eq!(duration_text(0, -1, -2 * 3_600_000_000_000, Flavor::Cassandra), "-1d2h");
        assert_eq!(duration_text(3, 0, 0, Flavor::Scylla), "3.0mo");
    }

    #[test]
    fn collections() {
        use CqlType as T;
        let lt = T::list(T::Int, true);
        let l = f(&lt, Value::List(vec![]));
        assert_eq!((l.text.as_str(), l.width), ("[]", 0));
        assert_eq!(f(&lt, Value::List(vec![Value::Int(1), Value::Int(2)])).text, "[1, 2]");
        let st = T::set(T::Timeuuid, true);
        let a = nosql_types::timeuuid::parse_uuid("d2177dd0-eaa2-11de-a572-001b779c76e3").unwrap();
        let b = nosql_types::timeuuid::parse_uuid("50554d6e-29bb-11e5-b345-feff819cdc9f").unwrap();
        let packed = nosql_types::pack_collection([&a[..], &b[..]]);
        let o = Opts::new(Flavor::Cassandra);
        assert_eq!(
            format(&st, Some(&packed), &o).text,
            "{50554d6e-29bb-11e5-b345-feff819cdc9f, d2177dd0-eaa2-11de-a572-001b779c76e3}"
        );
        let mt = T::map(T::Text, T::Int, true);
        let m = f(&mt, Value::Map(vec![(Value::Text("x".into()), Value::Int(1)), (Value::Text("a".into()), Value::Int(2))]));
        assert_eq!((m.text.as_str(), m.width), ("{'a': 2, 'x': 1}", 16));
        let tt = T::Tuple(vec![T::Int, T::Text, T::Boolean]);
        assert_eq!(f(&tt, Value::Tuple(vec![Value::Null, Value::Text("it's".into()), Value::Null])).text, "(None, 'it''s', None)");
        let ut = T::Udt(
            Arc::new(UdtDef::new("k", "a", vec![("street".into(), T::Text), ("Zip".into(), T::Int), ("tags".into(), T::set(T::Text, false))])),
            true,
        );
        assert_eq!(
            f(&ut, Value::Udt(vec![Value::Text("Main".into()), Value::Null, Value::Set(vec![Value::Text("x".into())])])).text,
            "{street: 'Main', Zip: null, tags: {'x'}}"
        );
        let vt = T::vector(T::Float, 3);
        assert_eq!(f(&vt, Value::Vector(vec![Value::Float(1.0), Value::Float(2.5), Value::Float(-3.25)])).text, "[1, 2.5, -3.25]");
        let tst = T::list(T::Timestamp, true);
        assert_eq!(f(&tst, Value::List(vec![Value::Timestamp(0)])).text, "['1970-01-01 00:00:00.000000+0000']");
    }

    #[test]
    fn separators_and_colors() {
        let mut o = Opts::new(Flavor::Cassandra);
        o.thousands_sep = Some(",".into());
        assert_eq!(float_text(1234567.25, 5, &o), "1,234,567.25");
        assert_eq!(int_fv("-1234567".into(), &o).text, "-1,234,567");
        let mut o = Opts::new(Flavor::Cassandra);
        o.decimal_sep = Some(",".into());
        assert_eq!(float_text(1.5, 5, &o), "1,5");
        o.color = true;
        let v = format(&CqlType::Int, None, &o);
        assert_eq!(v.colored, format!("{RED}null{RESET}"));
        assert_eq!(v.rjust(6, true), format!("  {RED}null{RESET}"));
        assert_eq!(v.ljust(6, false), "null  ");
    }
}
