//! pgvector against the reference build: a differential fixture (`VECTOR_FIXTURE`, made
//! by running every case on PostgreSQL 18.6 + pgvector 0.8.7 with extra_float_digits = 3),
//! and the orders the index keys must keep.

use super::common::{f32_to_half, fmt_f32, fmt_f64, half_to_f32, strtof};
use super::{bitvec, halfvec, sparsevec, vector};
use crate::Result;
use crate::ext::types::{self, HALFVEC, SPARSEVEC, VECTOR};
use crate::types::array::ArrayValue;
use crate::types::{DataType, Numeric, Value};
use std::cmp::Ordering;
use std::sync::Arc;

/// The C function `sym` (no call context).
fn plain(sym: &str) -> Option<fn(&[Value]) -> Result<Value>> {
    vector::plain(sym)
        .or_else(|| halfvec::plain(sym))
        .or_else(|| sparsevec::plain(sym))
        .or_else(|| bitvec::plain(sym))
}

/// An array from its text: one or two dimensions.
fn array(elem: DataType, s: &str, item: impl Fn(&str) -> Value) -> Value {
    let inner = &s[1..s.len() - 1];
    let one = |t: &str| -> Vec<Value> {
        if t.is_empty() {
            return Vec::new();
        }
        t.split(',').map(|x| if x == "NULL" { Value::Null } else { item(x) }).collect()
    };
    if let Some(rows) = inner.strip_prefix('{').and_then(|r| r.strip_suffix('}')) {
        let rows: Vec<Vec<Value>> = rows.split("},{").map(one).collect();
        let dims = vec![(rows.len(), 1), (rows[0].len(), 1)];
        let items = rows.into_iter().flatten().collect();
        return Value::Array(Arc::new(ArrayValue { elem, dims, items }));
    }
    Value::Array(Arc::new(ArrayValue::from_vec(elem, one(inner))))
}

/// The fixture's escapes (`\\ \n \t \r \v \f`).
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match it.next() {
            Some('n') => '\n',
            Some('t') => '\t',
            Some('r') => '\r',
            Some('v') => '\x0b',
            Some('f') => '\x0c',
            Some(c) => c,
            None => '\\',
        });
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\t', "\\t")
        .replace('\r', "\\r")
        .replace('\x0b', "\\v")
        .replace('\x0c', "\\f")
}

/// A fixture argument: `kind:text`.
fn arg(s: &str) -> Value {
    let (kind, t) = s.split_once(':').unwrap();
    let ext = |k: u16| types::input(k, t).unwrap_or_else(|e| panic!("bad fixture value {s}: {}", e.message));
    match kind {
        "v" => ext(VECTOR),
        "h" => ext(HALFVEC),
        "s" => ext(SPARSEVEC),
        "i" => Value::Int4(t.parse().unwrap()),
        "f" => Value::Float8(t.parse().unwrap()),
        "t" => Value::Text(unescape(t).into()),
        "b" => {
            let mut bytes = vec![0u8; t.len().div_ceil(8)];
            for (i, c) in t.bytes().enumerate() {
                if c == b'1' {
                    bytes[i / 8] |= 0x80 >> (i % 8);
                }
            }
            Value::Bits(t.len() as u32, bytes.into())
        }
        "f8a" => array(DataType::Float8, t, |x| Value::Float8(x.parse().unwrap())),
        "f4a" => array(DataType::Float4, t, |x| Value::Float4(x.parse().unwrap())),
        "i4a" => array(DataType::Int4, t, |x| Value::Int4(x.parse().unwrap())),
        "na" => array(DataType::Numeric(-1), t, |x| Value::Numeric(Arc::new(Numeric::parse_pg(x).unwrap()))),
        "ta" => array(DataType::Text, t, |x| Value::Text(x.into())),
        _ => panic!("unknown fixture argument kind {kind}"),
    }
}

/// A result as `::text` shows it under extra_float_digits = 3 (floats in their shortest
/// form, here this module's: the engine's float4out / float8out differ for powers of two).
fn show(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Float8(f) => fmt_f64(*f),
        Value::Float4(f) => fmt_f32(*f),
        Value::Int4(i) => i.to_string(),
        Value::Bool(b) => if *b { "true" } else { "false" }.into(),
        Value::Text(t) => t.to_string(),
        Value::Ext(k, b) => types::output_plain(*k, b),
        Value::Array(a) => format!("{{{}}}", a.items.iter().map(show).collect::<Vec<_>>().join(",")),
        Value::Bits(n, b) => (0..*n as usize).map(|i| if b[i / 8] & (0x80 >> (i % 8)) != 0 { '1' } else { '0' }).collect(),
        Value::Bytea(b) => format!("\\x{}", b.iter().map(|c| format!("{c:02x}")).collect::<String>()),
        v => format!("{v:?}"),
    }
}

/// The C function `sym` over `args`, or a composition: `@acos`, and the aggregates
/// `@vector_avg` / `@vector_sum` / `@halfvec_avg` / `@halfvec_sum` over their inputs (the
/// state functions in turn, then the final function).
fn call(sym: &str, args: &[Value]) -> Result<Value> {
    let f = |s: &str| plain(s).unwrap_or_else(|| panic!("no native {s}"));
    let (step, fin) = match sym {
        "@acos" => return Ok(Value::Float8(super::dist::acos(args[0].as_f64().unwrap()))),
        "@fmt4" => return Ok(Value::Text(fmt_f32(args[0].as_f64().unwrap() as f32).into())),
        "@fmt8" => return Ok(Value::Text(fmt_f64(args[0].as_f64().unwrap()).into())),
        "@vector_avg" => ("vector_accum", Some("vector_avg")),
        "@halfvec_avg" => ("halfvec_accum", Some("halfvec_avg")),
        "@vector_sum" => ("vector_add", None),
        "@halfvec_sum" => ("halfvec_add", None),
        _ => return f(sym)(args),
    };
    let mut state = match fin {
        Some(_) => array(DataType::Float8, "{0}", |x| Value::Float8(x.parse().unwrap())),
        None => args[0].clone(),
    };
    for v in &args[usize::from(fin.is_none())..] {
        state = f(step)(&[state, v.clone()])?;
    }
    match fin {
        Some(fin) => f(fin)(&[state]),
        None => Ok(state),
    }
}

fn run(sym: &str, args: &[Value]) -> String {
    escape(&match call(sym, args) {
        Ok(v) => show(&v),
        Err(e) => {
            let mut s = format!("ERROR {} {}", e.code, e.message);
            if let Some(d) = e.detail {
                s.push_str(" | ");
                s.push_str(&d);
            }
            s
        }
    })
}

/// Each fixture line: the C symbol, its arguments (`kind:text`), and the reference's
/// result, tab-separated. Every result must be identical; where the symbol is a distance,
/// norm or `l2_normalize`, the index access methods' entry points (`dist`) must give it
/// too. `VECTOR_FIXTURE` lists the fixtures, `:`-separated.
#[test]
#[ignore]
fn reference_fixture() {
    let paths = std::env::var("VECTOR_FIXTURE").expect("VECTOR_FIXTURE: the fixtures' paths");
    let (mut n, mut bad) = (0, Vec::new());
    let mut per = std::collections::BTreeMap::<String, (usize, usize)>::new();
    for path in paths.split(':') {
        let text = std::fs::read_to_string(path).unwrap();
        for line in text.lines().filter(|l| !l.is_empty()) {
            let f: Vec<&str> = line.split('\t').collect();
            let (sym, want) = (f[0], f[f.len() - 1]);
            let args: Vec<Value> = f[1..f.len() - 1].iter().map(|a| arg(a)).collect();
            let mut got = vec![run(sym, &args)];
            if !want.starts_with("ERROR") {
                if let Some(d) = super::dist::distance(sym) {
                    got.push(show(&Value::Float8(d(&args[0], &args[1]))));
                }
                if let Some(d) = super::dist::norm(sym) {
                    got.push(show(&Value::Float8(d(&args[0]))));
                }
                if matches!(sym, "l2_normalize" | "halfvec_l2_normalize" | "sparsevec_l2_normalize") {
                    got.push(show(&super::dist::l2_normalize(&args[0])));
                }
            }
            n += 1;
            let e = per.entry(sym.to_string()).or_default();
            e.0 += 1;
            if let Some(g) = got.iter().find(|g| *g != want) {
                e.1 += 1;
                bad.push(format!("{line}\n    got  {g}"));
            }
        }
    }
    for (sym, (cases, failed)) in &per {
        eprintln!("{sym}: {cases} cases, {failed} different");
    }
    for b in bad.iter().take(400) {
        eprintln!("{b}");
    }
    assert!(bad.is_empty(), "{} of {n} cases differ from the reference", bad.len());
}

// ── input ──

#[test]
fn strtof_prefixes() {
    let t = |s: &str| strtof(s.as_bytes()).map(|f| (f.value, f.len, f.erange));
    assert_eq!(t("1.5,"), Some((1.5, 3, false)));
    assert_eq!(t("-.5e1]"), Some((-5.0, 5, false)));
    assert_eq!(t("5.e"), Some((5.0, 2, false)));
    assert_eq!(t("1e+x"), Some((1.0, 1, false)));
    assert_eq!(t("infinity"), Some((f32::INFINITY, 8, false)));
    assert_eq!(t("-infin"), Some((f32::NEG_INFINITY, 4, false)));
    assert_eq!(t("in"), None);
    assert_eq!(t("."), None);
    assert_eq!(t("0x"), Some((0.0, 1, false)));
    assert_eq!(t("0x1.8p1"), Some((3.0, 7, false)));
    assert_eq!(t("1e39"), Some((f32::INFINITY, 4, true)));
    assert_eq!(t("1e-50"), Some((0.0, 5, true)));
    assert_eq!(t("0e-50"), Some((0.0, 5, false)));
    assert!(t("nan(x_1)").is_some_and(|f| f.0.is_nan() && f.1 == 8));
    assert!(t("nan(x").is_some_and(|f| f.0.is_nan() && f.1 == 3));
}

#[test]
fn halves() {
    for h in 0..=u16::MAX {
        let f = half_to_f32(h);
        if !f.is_nan() {
            assert_eq!(f32_to_half(f), h, "{h:#x}");
        }
    }
    assert_eq!(f32_to_half(65520.0), 0x7c00);
    assert_eq!(f32_to_half(65519.99), 0x7bff);
    assert_eq!(f32_to_half(2.980_232_2e-8), 0);
}

// ── keys ──

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// Small values, zeros of both signs and repeats, so that ties are common.
    fn elem(&mut self) -> f32 {
        [0.0, -0.0, 1.0, -1.0, 2.5, -2.5, 1e-3][self.below(7) as usize]
    }
}

fn check_keys(kind: u16, values: &[Vec<u8>]) {
    let t = types::get(kind).unwrap();
    let key = |b: &[u8]| {
        let mut k = Vec::new();
        (t.key)(b, &mut k);
        k
    };
    for a in values {
        for b in values {
            let c = (t.cmp)(a, b);
            assert_eq!(key(a).cmp(&key(b)), c, "{} vs {}", (t.output)(a, &Default::default()), (t.output)(b, &Default::default()));
            assert_eq!((t.canon)(a) == (t.canon)(b), c == Ordering::Equal);
        }
    }
}

#[test]
fn keys_order_as_cmp() {
    let mut r = Rng(0x2545_f491_4f6c_dd1d);
    let dense: Vec<Vec<f32>> = (0..120).map(|_| (0..1 + r.below(4)).map(|_| r.elem()).collect()).collect();
    let text = |v: &Vec<f32>| format!("[{}]", v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","));
    let input = |k: u16, s: &str| (types::get(k).unwrap().input)(s).unwrap();
    check_keys(VECTOR, &dense.iter().map(|v| input(VECTOR, &text(v))).collect::<Vec<_>>());
    check_keys(HALFVEC, &dense.iter().map(|v| input(HALFVEC, &text(v))).collect::<Vec<_>>());
    let sparse: Vec<Vec<u8>> = dense
        .iter()
        .map(|v| {
            let dim = v.len() + r.below(3) as usize;
            let elems: Vec<String> =
                v.iter().enumerate().filter(|e| *e.1 != 0.0).map(|(i, x)| format!("{}:{x}", i + 1)).collect();
            input(SPARSEVEC, &format!("{{{}}}/{dim}", elems.join(",")))
        })
        .collect();
    check_keys(SPARSEVEC, &sparse);
}

// ── binary I/O ──

#[test]
fn binary_round_trip() {
    for (k, lit, dim) in [(VECTOR, "[1,-0,2.5]", 3), (HALFVEC, "[1,-0,2.5]", 3), (SPARSEVEC, "{1:1,3:-2}/5", 5)] {
        let b = (types::get(k).unwrap().input)(lit).unwrap();
        let mut out = bytes::BytesMut::new();
        super::send(k, &b, &mut out).unwrap();
        assert_eq!(super::recv(k, &out, -1).unwrap(), b);
        assert_eq!(super::recv(k, &out, dim).unwrap(), b);
        let e = super::recv(k, &out, 4).unwrap_err();
        assert_eq!(e.message, format!("expected 4 dimensions, not {dim}"));
        let e = super::recv(k, &out[..out.len() - 1], -1).unwrap_err();
        assert_eq!((e.code, e.message.as_str()), ("08P01", "insufficient data left in message"));
    }
    // vector_send: int16 dimensions, int16 unused, big-endian floats.
    let mut out = bytes::BytesMut::new();
    super::send(VECTOR, &(types::get(VECTOR).unwrap().input)("[1,2]").unwrap(), &mut out).unwrap();
    assert_eq!(&out[..], &[0, 2, 0, 0, 0x3f, 0x80, 0, 0, 0x40, 0, 0, 0]);
}

#[test]
fn binary_errors() {
    let err = |k: u16, b: &[u8]| super::recv(k, b, -1).unwrap_err().message;
    let f = |v: f32| v.to_be_bytes();
    assert_eq!(err(VECTOR, &[0, 0, 0, 0]), "vector must have at least 1 dimension");
    assert_eq!(err(VECTOR, &[0xff, 0xff, 0, 0]), "vector must have at least 1 dimension");
    assert_eq!(err(VECTOR, &[0, 1, 0, 1]), "expected unused to be 0, not 1");
    assert_eq!(err(VECTOR, &[&[0, 1, 0, 0][..], &f(f32::NAN)].concat()), "NaN not allowed in vector");
    assert_eq!(err(HALFVEC, &[0, 1, 0, 0, 0x7c, 0]), "infinite value not allowed in halfvec");
    let sparse = |dim: i32, nnz: i32, unused: i32, idx: &[i32], vals: &[f32]| {
        let mut b = Vec::new();
        for x in [dim, nnz, unused].iter().chain(idx) {
            b.extend_from_slice(&x.to_be_bytes());
        }
        for v in vals {
            b.extend_from_slice(&v.to_be_bytes());
        }
        b
    };
    assert_eq!(err(SPARSEVEC, &sparse(3, 1, 0, &[0], &[0.0])), "binary representation of sparsevec cannot contain zero values");
    assert_eq!(err(SPARSEVEC, &sparse(3, 2, 0, &[1, 0], &[1.0, 1.0])), "sparsevec indices must be in ascending order");
    assert_eq!(err(SPARSEVEC, &sparse(3, 2, 0, &[1, 1], &[1.0, 1.0])), "sparsevec indices must not contain duplicates");
    assert_eq!(err(SPARSEVEC, &sparse(3, 1, 0, &[3], &[1.0])), "sparsevec index out of bounds");
    assert_eq!(err(SPARSEVEC, &sparse(3, 4, 0, &[], &[])), "sparsevec cannot have more elements than dimensions");
    assert_eq!(err(SPARSEVEC, &sparse(3, -1, 0, &[], &[])), "sparsevec cannot have negative number of elements");
    assert_eq!(err(SPARSEVEC, &sparse(3, 0, 7, &[], &[])), "expected unused to be 0, not 7");
}

/// Nanoseconds per call of the distances over 1536 dimensions (run with --release).
#[test]
#[ignore]
fn kernel_speed() {
    let mut r = Rng(7);
    let mut v = || -> Value {
        let x: Vec<String> = (0..1536).map(|_| format!("{}", (r.below(2000) as f32 - 1000.0) / 997.0)).collect();
        types::input(VECTOR, &format!("[{}]", x.join(","))).unwrap()
    };
    let (a, b) = (v(), v());
    let h = |x: &Value| plain("vector_to_halfvec").unwrap()(&[x.clone(), Value::Int4(-1), Value::Int4(1)]).unwrap();
    let (ha, hb) = (h(&a), h(&b));
    for (sym, x, y) in [
        ("vector_l2_squared_distance", &a, &b),
        ("vector_negative_inner_product", &a, &b),
        ("cosine_distance", &a, &b),
        ("l1_distance", &a, &b),
        ("halfvec_l2_squared_distance", &ha, &hb),
        ("halfvec_cosine_distance", &ha, &hb),
    ] {
        let d = super::dist::distance(sym).unwrap();
        let n = 200_000;
        let t = std::time::Instant::now();
        let mut s = 0.0;
        for _ in 0..n {
            s += d(std::hint::black_box(x), std::hint::black_box(y));
        }
        eprintln!("{sym}: {:.0} ns ({s})", t.elapsed().as_nanos() as f64 / n as f64);
    }
    let t = std::time::Instant::now();
    let out = types::output_plain(VECTOR, super::common::bytes(&a));
    eprintln!("vector_out: {:?} for {} bytes", t.elapsed(), out.len());
}
