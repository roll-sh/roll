//! The reference captures record values as the Python driver returns them, dumped to JSON with
//! `default=repr` (statements) or `default=str` (catalog). This module renders our values the
//! same way so captures compare as text.

use nosql_types::CqlType;

/// A value as the Python driver returns it.
pub enum Py {
    None,
    Bool(bool),
    Int(String),
    Float(f64),
    Str(String),
    List(Vec<Py>),
    Tuple(Vec<Py>),
    /// An object JSON can't encode, by its `repr()` and `str()`.
    Obj {
        repr: String,
        str: String,
    },
}

impl Py {
    fn obj(repr: String, str: String) -> Py {
        Py::Obj { repr, str }
    }
}

/// How the capture's `json.dump` encoded objects.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dump {
    Repr,
    Str,
}

pub fn repr(v: &Py) -> String {
    match v {
        Py::None => "None".into(),
        Py::Bool(b) => if *b { "True" } else { "False" }.into(),
        Py::Int(s) => s.clone(),
        Py::Float(f) => py_float(*f),
        Py::Str(s) => py_str(s),
        Py::List(items) => format!(
            "[{}]",
            items.iter().map(repr).collect::<Vec<_>>().join(", ")
        ),
        Py::Tuple(items) if items.len() == 1 => format!("({},)", repr(&items[0])),
        Py::Tuple(items) => format!(
            "({})",
            items.iter().map(repr).collect::<Vec<_>>().join(", ")
        ),
        Py::Obj { repr, .. } => repr.clone(),
    }
}

/// `json.dump(..., default=…)` then `json.load`: tuples become lists, objects strings.
pub fn jsonify(v: Py, dump: Dump) -> Py {
    match v {
        Py::List(items) | Py::Tuple(items) => {
            Py::List(items.into_iter().map(|i| jsonify(i, dump)).collect())
        }
        Py::Obj { repr, str } => Py::Str(if dump == Dump::Repr { repr } else { str }),
        other => other,
    }
}

pub fn py_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

pub fn py_bytes(b: &[u8]) -> String {
    let quote = if b.contains(&b'\'') && !b.contains(&b'"') {
        b'"'
    } else {
        b'\''
    };
    let mut out = format!("b{}", quote as char);
    for &c in b {
        match c {
            b'\\' => out.push_str("\\\\"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c as char);
            }
            0x20..=0x7e => out.push(c as char),
            c => out.push_str(&format!("\\x{c:02x}")),
        }
    }
    out.push(quote as char);
    out
}

/// Python's `repr(float)`.
pub fn py_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    if f == 0.0 {
        return if f.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    let sci = format!("{f:e}");
    let (mant, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let sign = if mant.starts_with('-') { "-" } else { "" };
    let digits: String = mant.chars().filter(|c| c.is_ascii_digit()).collect();
    if (-4..16).contains(&exp) {
        let point = exp + 1;
        let s = if point <= 0 {
            format!("0.{}{}", "0".repeat((-point) as usize), digits)
        } else if point as usize >= digits.len() {
            format!("{}{}.0", digits, "0".repeat(point as usize - digits.len()))
        } else {
            format!(
                "{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        };
        format!("{sign}{s}")
    } else {
        let m = if digits.len() > 1 {
            format!("{}.{}", &digits[..1], &digits[1..])
        } else {
            digits.clone()
        };
        format!(
            "{sign}{m}e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    }
}

/// Python's `str(Decimal)`.
pub fn py_decimal(d: &nosql_types::num::Decimal) -> String {
    let neg = d.unscaled.sign() == num_bigint::Sign::Minus;
    let digits = d.unscaled.magnitude().to_string();
    let exp = -(d.scale as i64);
    let left = exp + digits.len() as i64;
    let dot = if exp <= 0 && left > -6 { left } else { 1 };
    let (int, frac) = if dot <= 0 {
        (
            "0".to_string(),
            format!(".{}{}", "0".repeat((-dot) as usize), digits),
        )
    } else if dot as usize >= digits.len() {
        (
            format!("{}{}", digits, "0".repeat(dot as usize - digits.len())),
            String::new(),
        )
    } else {
        (
            digits[..dot as usize].to_string(),
            format!(".{}", &digits[dot as usize..]),
        )
    };
    let e = if left == dot {
        String::new()
    } else {
        format!("E{:+}", left - dot)
    };
    format!("{}{int}{frac}{e}", if neg { "-" } else { "" })
}

fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// A `datetime.datetime` (naive UTC) of milliseconds since the epoch.
pub fn py_datetime(ms: i64) -> Py {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (y, m, d) = civil(days);
    let (h, mi, s, us) = (
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000 * 1000,
    );
    let mut r = format!("datetime.datetime({y}, {m}, {d}, {h}, {mi}");
    if s != 0 || us != 0 {
        r.push_str(&format!(", {s}"));
    }
    if us != 0 {
        r.push_str(&format!(", {us}"));
    }
    r.push(')');
    let mut st = format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}");
    if us != 0 {
        st.push_str(&format!(".{us:06}"));
    }
    Py::obj(r, st)
}

fn be_int(b: &[u8]) -> String {
    nosql_types::num::varint_from_bytes(b).to_string()
}

fn uuid(b: &[u8]) -> Py {
    let u: [u8; 16] = b.try_into().unwrap_or([0; 16]);
    let s = nosql_types::timeuuid::format_uuid(&u);
    Py::obj(format!("UUID('{s}')"), s)
}

/// The driver's deserialisation of a value of type `ty`.
pub fn py_value(ty: &CqlType, v: Option<&[u8]>) -> Py {
    let Some(b) = v else { return Py::None };
    if b.is_empty() && !matches!(ty, CqlType::Ascii | CqlType::Text | CqlType::Blob) {
        return Py::None;
    }
    match ty {
        CqlType::Ascii | CqlType::Text => Py::Str(String::from_utf8_lossy(b).into_owned()),
        CqlType::Blob => Py::obj(py_bytes(b), py_bytes(b)),
        CqlType::Boolean => Py::Bool(b[0] != 0),
        CqlType::Tinyint
        | CqlType::Smallint
        | CqlType::Int
        | CqlType::Bigint
        | CqlType::Counter
        | CqlType::Varint => Py::Int(be_int(b)),
        CqlType::Float => Py::Float(f32::from_be_bytes(b.try_into().unwrap_or([0; 4])) as f64),
        CqlType::Double => Py::Float(f64::from_be_bytes(b.try_into().unwrap_or([0; 8]))),
        CqlType::Decimal => match nosql_types::num::Decimal::from_bytes(b) {
            Some(d) => {
                let s = py_decimal(&d);
                Py::obj(format!("Decimal('{s}')"), s)
            }
            None => Py::Str("<bad decimal>".into()),
        },
        CqlType::Date => {
            let days = u32::from_be_bytes(b.try_into().unwrap_or([0; 4])) as i64 - (1i64 << 31);
            let (y, m, d) = civil(days);
            Py::obj(format!("Date({days})"), format!("{y:04}-{m:02}-{d:02}"))
        }
        CqlType::Time => {
            let n = i64::from_be_bytes(b.try_into().unwrap_or([0; 8]));
            let st = format!(
                "{:02}:{:02}:{:02}.{:09}",
                n / 3_600_000_000_000,
                n / 60_000_000_000 % 60,
                n / 1_000_000_000 % 60,
                n % 1_000_000_000
            );
            Py::obj(format!("Time({n})"), st)
        }
        CqlType::Timestamp => py_datetime(i64::from_be_bytes(b.try_into().unwrap_or([0; 8]))),
        CqlType::Duration => match nosql_types::Duration::decode(b) {
            Some(d) => {
                let s = format!("Duration({}, {}, {})", d.months, d.days, d.nanos);
                Py::obj(s.clone(), s)
            }
            None => Py::Str("<bad duration>".into()),
        },
        CqlType::Inet => Py::Str(match b.len() {
            4 => std::net::Ipv4Addr::from(<[u8; 4]>::try_from(b).unwrap_or([0; 4])).to_string(),
            16 => std::net::Ipv6Addr::from(<[u8; 16]>::try_from(b).unwrap_or([0; 16])).to_string(),
            _ => "<bad inet>".into(),
        }),
        CqlType::Uuid | CqlType::Timeuuid => uuid(b),
        CqlType::List { elem, .. } => Py::List(
            nosql_types::collection_elements(b)
                .unwrap_or_default()
                .into_iter()
                .map(|e| py_value(elem, e))
                .collect(),
        ),
        CqlType::Vector { elem, dim } => {
            let size = (b.len() / (*dim).max(1) as usize).max(1);
            Py::List(b.chunks(size).map(|c| py_value(elem, Some(c))).collect())
        }
        CqlType::Set { elem, .. } => {
            let items: Vec<String> = nosql_types::collection_elements(b)
                .unwrap_or_default()
                .into_iter()
                .map(|e| repr(&py_value(elem, e)))
                .collect();
            let s = format!("SortedSet([{}])", items.join(", "));
            Py::obj(s.clone(), s)
        }
        CqlType::Map { key, value, .. } => {
            let entries: Vec<(String, String)> = nosql_types::map_entries(b)
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| {
                    (
                        repr(&py_value(key, Some(k))),
                        repr(&py_value(value, Some(v))),
                    )
                })
                .collect();
            let r = format!(
                "OrderedMapSerializedKey([{}])",
                entries
                    .iter()
                    .map(|(k, v)| format!("({k}, {v})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let s = format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(k, v)| format!("{k}: {v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Py::obj(r, s)
        }
        CqlType::Tuple(types) => {
            let fields = nosql_types::tuple_fields(b).unwrap_or_default();
            Py::Tuple(
                types
                    .iter()
                    .enumerate()
                    .map(|(i, t)| py_value(t, fields.get(i).copied().flatten()))
                    .collect(),
            )
        }
        CqlType::Udt(def, _) => {
            let fields = nosql_types::tuple_fields(b).unwrap_or_default();
            Py::Tuple(
                def.fields
                    .iter()
                    .enumerate()
                    .map(|(i, (_, t))| py_value(t, fields.get(i).copied().flatten()))
                    .collect(),
            )
        }
        _ => Py::obj(py_bytes(b), py_bytes(b)),
    }
}

/// `cql_parameterized_type()` of the driver's type.
pub fn driver_type(ty: &CqlType) -> String {
    match ty {
        CqlType::Text => "varchar".into(),
        CqlType::List { elem, .. } => format!("list<{}>", driver_type(elem)),
        CqlType::Set { elem, .. } => format!("set<{}>", driver_type(elem)),
        CqlType::Map { key, value, .. } => {
            format!("map<{}, {}>", driver_type(key), driver_type(value))
        }
        CqlType::Tuple(types) => format!(
            "frozen<tuple<{}>>",
            types.iter().map(driver_type).collect::<Vec<_>>().join(", ")
        ),
        CqlType::Udt(def, _) => format!("frozen<{}>", def.name),
        CqlType::Vector { elem, dim } => format!(
            "org.apache.cassandra.db.marshal.VectorType<{}, {dim}>",
            driver_type(elem)
        ),
        other => nosql_types::cql_name(other),
    }
}

pub fn str_list<'a>(items: impl IntoIterator<Item = &'a str>) -> String {
    repr(&Py::List(
        items.into_iter().map(|s| Py::Str(s.to_string())).collect(),
    ))
}

/// Whether the driver fails to decode a value (a null element inside a collection).
pub fn undecodable(ty: &CqlType, v: Option<&[u8]>) -> bool {
    let Some(b) = v else { return false };
    match ty {
        CqlType::List { elem, .. } | CqlType::Set { elem, .. } => {
            nosql_types::collection_elements(b)
                .map(|es| es.iter().any(|e| e.is_none() || undecodable(elem, *e)))
                .unwrap_or(true)
        }
        _ => false,
    }
}

#[test]
fn python_reprs() {
    assert_eq!(py_float(1.5e300), "1.5e+300");
    assert_eq!(py_float(3.25), "3.25");
    assert_eq!(py_float(1.0), "1.0");
    assert_eq!(py_float(1e-5), "1e-05");
    assert_eq!(py_float(0.0001), "0.0001");
    assert_eq!(py_float(1e16), "1e+16");
    assert_eq!(py_float(123456789012345.0), "123456789012345.0");
    assert_eq!(py_str("it's"), "\"it's\"");
    assert_eq!(py_bytes(&[0xca, 0xfe]), "b'\\xca\\xfe'");
    assert_eq!(repr(&py_datetime(0)), "datetime.datetime(1970, 1, 1, 0, 0)");
    assert_eq!(
        repr(&py_datetime(1_709_210_096_789)),
        "datetime.datetime(2024, 2, 29, 12, 34, 56, 789000)"
    );
    let dec = |s: &str| py_decimal(&nosql_types::num::Decimal::parse(s).unwrap());
    assert_eq!(dec("123.4500"), "123.4500");
    assert_eq!(dec("-0.000001"), "-0.000001");
    assert_eq!(dec("0.0000001"), "1E-7");
}
