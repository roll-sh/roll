//! A value's Datum as Postgres builds it in memory (what `outDatum` prints in
//! `nodeToString`, and what a Const in a stored tree holds): fixed-length types as their raw
//! bytes, varlena types with a 4-byte header as their input functions build them (arrays,
//! ranges, jsonb, tsvector, geometric and composite types in their own on-disk layouts; the
//! varlenas inside a range or a composite take a 1-byte header where they can, as
//! `datum_write` / `heap_fill_tuple` pack them).

use super::datum::{self, Layout};
use super::geo::{Geo, Point};
use super::range::Range;
use super::{DataType, Value};

/// `typlen`, `typbyval` and `typalign` (in bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub len: i16,
    pub byval: bool,
    pub align: usize,
}

/// How `ty`'s values are laid out (None: not modelled).
pub fn meta(ty: &DataType) -> Option<Meta> {
    use DataType as T;
    let align = crate::catalog::relsize::type_layout(ty).1;
    let m = |len: i16, byval: bool| Some(Meta { len, byval, align });
    match ty {
        T::Unknown | T::Cstring => Some(Meta {
            len: -2,
            byval: false,
            align: 1,
        }),
        T::Point | T::Lseg | T::GBox | T::Line | T::Circle => m(ty.typlen(), false),
        T::Path | T::Polygon => m(-1, false),
        T::Jsonb | T::TsVector | T::TsQuery | T::Int2Vector | T::OidVector => m(-1, false),
        T::Array(e) => meta(e).and(m(-1, false)),
        T::Range(rt) | T::Multirange(rt) => {
            let sub = meta(&rt.subtype)?;
            Some(Meta {
                len: -1,
                byval: false,
                align: if sub.align == 8 { 8 } else { 4 },
            })
        }
        T::Row(r) if r.oid != 0 => {
            r.fields.iter().try_for_each(|(_, t)| meta(t).map(|_| ()))?;
            Some(Meta {
                len: -1,
                byval: false,
                align: 8,
            })
        }
        _ => {
            let Layout { len, byval } = datum::layout(ty)?;
            m(len, byval)
        }
    }
}

/// A varlena's bytes with a 4-byte header.
pub fn long_varlena(body: &[u8]) -> Vec<u8> {
    let mut out = (((body.len() + 4) as u32) << 2).to_le_bytes().to_vec();
    out.extend_from_slice(body);
    out
}

/// A varlena's bytes with a 1-byte header where it fits (`VARATT_CAN_MAKE_SHORT`).
fn packed_varlena(body: &[u8]) -> (Vec<u8>, bool) {
    if body.len() < 0x7f {
        let mut out = vec![(((body.len() + 1) << 1) | 1) as u8];
        out.extend_from_slice(body);
        (out, true)
    } else {
        (long_varlena(body), false)
    }
}

fn align_to(buf: &mut Vec<u8>, base: usize, align: usize) {
    let at = base + buf.len();
    let pad = (align - at % align) % align;
    buf.resize(buf.len() + pad, 0);
}

/// The bytes `outDatum` prints for a non-null `v` of type `ty`: a by-value type's `typlen`
/// bytes, a fixed-length one's bytes, a varlena's with its header, a C string's with its
/// terminating NUL. `enum_oid` gives an enum label's pg_enum oid.
pub fn datum_bytes(v: &Value, ty: &DataType, enum_oid: &dyn Fn(u32, &str) -> u32) -> Option<Vec<u8>> {
    let m = meta(ty)?;
    if m.len == -2 {
        let Value::Text(s) = v else { return None };
        let mut b = s.as_bytes().to_vec();
        b.push(0);
        return Some(b);
    }
    let body = body(v, ty, enum_oid)?;
    if m.len == -1 {
        return Some(long_varlena(&body));
    }
    (body.len() == m.len as usize).then_some(body)
}

/// A value's bytes after any varlena header.
fn body(v: &Value, ty: &DataType, enum_oid: &dyn Fn(u32, &str) -> u32) -> Option<Vec<u8>> {
    use DataType as T;
    Some(match (ty, v) {
        (T::Jsonb, Value::Jsonb(j)) => crate::catalog::jsonbin::jsonb_bytes(&j.to_string()),
        (T::Jsonb, Value::Json(s)) => crate::catalog::jsonbin::jsonb_bytes(s),
        (T::Array(e), Value::Array(a)) => array(a, e, enum_oid)?,
        (T::OidVector, Value::Array(a)) => array(a, &T::Oid, enum_oid)?,
        (T::Int2Vector, Value::Array(a)) => array(a, &T::Int2, enum_oid)?,
        (T::Range(rt), Value::Range(r)) => {
            let mut out = rt.oid.to_le_bytes().to_vec();
            let (data, flags) = range_data(r, &rt.subtype, enum_oid)?;
            out.extend(data);
            out.push(flags);
            out
        }
        (T::Multirange(rt), Value::Multirange(rs)) => multirange(rt, rs, enum_oid)?,
        (_, Value::Geo(g)) => geo(g),
        (T::TsVector, Value::TsVector(t)) => tsvector(t),
        (T::TsQuery, Value::TsQuery(q)) => tsquery(q),
        (T::Row(r), Value::Record(vals)) if r.oid != 0 => composite(r, vals, enum_oid)?,
        (T::Enum(oid, ..), Value::Text(label)) => enum_oid(*oid, label).to_le_bytes().to_vec(),
        _ => {
            datum::layout(ty)?;
            // (`encode`'s text form for shapes it doesn't know is no Datum.)
            let ok = match (ty, v) {
                (T::Text | T::Varchar(_) | T::Bpchar(_) | T::Name | T::Json | T::Xml, Value::Text(_) | Value::Json(_)) => true,
                (T::Bytea, Value::Bytea(_)) | (T::Numeric(_), Value::Numeric(_)) => true,
                (T::Bit(_) | T::Varbit(_), Value::Bits(..)) | (T::Inet | T::Cidr, Value::Inet(..)) => true,
                (_, Value::Text(_) | Value::Json(_) | Value::Record(_) | Value::Array(_)) => false,
                _ => true,
            };
            if !ok {
                return None;
            }
            let mut b = datum::encode(v, ty, enum_oid);
            if let (T::Bpchar(m), Value::Text(s)) = (ty, v)
                && *m > 4
            {
                // bpchar's input pads to the length.
                let want = (*m - 4) as usize;
                b.resize(b.len() + want.saturating_sub(s.chars().count()), b' ');
            }
            b
        }
    })
}

/// An element, bound or attribute: its bytes, laid out from offset `base + buf.len()` of
/// the enclosing datum. `pack`: varlenas take a 1-byte header where they fit (unaligned).
fn put(
    buf: &mut Vec<u8>,
    base: usize,
    v: &Value,
    ty: &DataType,
    pack: bool,
    enum_oid: &dyn Fn(u32, &str) -> u32,
) -> Option<()> {
    let m = meta(ty)?;
    let b = body(v, ty, enum_oid)?;
    if m.len == -1 {
        if pack {
            let (bytes, short) = packed_varlena(&b);
            if !short {
                align_to(buf, base, m.align);
            }
            buf.extend(bytes);
        } else {
            align_to(buf, base, m.align);
            buf.extend(long_varlena(&b));
        }
    } else {
        if m.len < 0 || b.len() != m.len as usize {
            return None;
        }
        align_to(buf, base, m.align);
        buf.extend(b);
    }
    Some(())
}

/// ArrayType after its header: ndim, data offset (0 without nulls), element type,
/// dimensions, lower bounds, the null bitmap, then the elements from a MAXALIGNed offset.
fn array(
    a: &crate::types::array::ArrayValue,
    elem: &DataType,
    enum_oid: &dyn Fn(u32, &str) -> u32,
) -> Option<Vec<u8>> {
    let ndim = a.dims.len();
    let has_null = a.items.iter().any(Value::is_null);
    let bitmap = if has_null { a.items.len().div_ceil(8) } else { 0 };
    let overhead = (16 + 8 * ndim + bitmap).div_ceil(8) * 8;
    let mut out = Vec::new();
    out.extend_from_slice(&(ndim as i32).to_le_bytes());
    out.extend_from_slice(&(if has_null { overhead as i32 } else { 0 }).to_le_bytes());
    out.extend_from_slice(&elem.oid().to_le_bytes());
    for (n, _) in &a.dims {
        out.extend_from_slice(&(*n as i32).to_le_bytes());
    }
    for (_, lb) in &a.dims {
        out.extend_from_slice(&lb.to_le_bytes());
    }
    if has_null {
        let mut bits = vec![0u8; bitmap];
        for (i, x) in a.items.iter().enumerate() {
            if !x.is_null() {
                bits[i / 8] |= 1 << (i % 8);
            }
        }
        out.extend(bits);
    }
    if ndim == 0 {
        return Some(out);
    }
    out.resize(overhead - 4, 0);
    for x in a.items.iter().filter(|x| !x.is_null()) {
        put(&mut out, 4, x, elem, false, enum_oid)?;
    }
    // (construct_md_array aligns the size after each element, the last one's too.)
    align_to(&mut out, 4, meta(elem)?.align);
    Some(out)
}

/// `range_serialize`'s bound data (from offset 8 of the range) and flags.
fn range_data(
    r: &Range,
    sub: &DataType,
    enum_oid: &dyn Fn(u32, &str) -> u32,
) -> Option<(Vec<u8>, u8)> {
    if r.empty {
        return Some((Vec::new(), 0x01));
    }
    let mut flags = 0u8;
    let mut data = Vec::new();
    match &r.lower {
        Some(v) => {
            if r.lower_inc {
                flags |= 0x02;
            }
            put(&mut data, 8, v, sub, true, enum_oid)?;
        }
        None => flags |= 0x08,
    }
    match &r.upper {
        Some(v) => {
            if r.upper_inc {
                flags |= 0x04;
            }
            put(&mut data, 8, v, sub, true, enum_oid)?;
        }
        None => flags |= 0x10,
    }
    Some((data, flags))
}

/// `multirange_serialize` after the header: the type, the range count, an item per range
/// after the first (every fourth an offset, the rest lengths), the ranges' flags, then their
/// bound data, aligned.
fn multirange(
    rt: &crate::types::range::RangeType,
    rs: &[Range],
    enum_oid: &dyn Fn(u32, &str) -> u32,
) -> Option<Vec<u8>> {
    let align = meta(&DataType::Range(std::sync::Arc::new(rt.clone())))?.align;
    let n = rs.len();
    let mut out = rt.multi_oid.to_le_bytes().to_vec();
    out.extend_from_slice(&(n as u32).to_le_bytes());
    let items_at = out.len();
    out.resize(out.len() + 4 * n.saturating_sub(1), 0);
    let mut parts = Vec::with_capacity(n);
    for r in rs {
        let (data, flags) = range_data(r, &rt.subtype, enum_oid)?;
        out.push(flags);
        parts.push(data);
    }
    align_to(&mut out, 4, align);
    let begin = out.len();
    let mut prev = 0usize;
    for (i, data) in parts.into_iter().enumerate() {
        let at = out.len() - begin;
        if i > 0 {
            let item = if i % 4 != 0 {
                (at - prev) as u32
            } else {
                at as u32 | 0x8000_0000
            };
            out[items_at + 4 * (i - 1)..items_at + 4 * i].copy_from_slice(&item.to_le_bytes());
            prev = at;
        }
        let len = data.len();
        out.extend(data);
        out.resize(out.len() + (align - len % align) % align, 0);
    }
    Some(out)
}

fn geo(g: &Geo) -> Vec<u8> {
    let mut out = Vec::new();
    let f = |out: &mut Vec<u8>, x: f64| out.extend_from_slice(&x.to_bits().to_le_bytes());
    let p = |out: &mut Vec<u8>, pt: &Point| {
        f(out, pt.x);
        f(out, pt.y);
    };
    match g {
        Geo::Point(pt) => p(&mut out, pt),
        Geo::Lseg(l) => {
            p(&mut out, &l.p[0]);
            p(&mut out, &l.p[1]);
        }
        Geo::Box(b) => {
            p(&mut out, &b.high);
            p(&mut out, &b.low);
        }
        Geo::Line(l) => {
            f(&mut out, l.a);
            f(&mut out, l.b);
            f(&mut out, l.c);
        }
        Geo::Circle(c) => {
            p(&mut out, &c.center);
            f(&mut out, c.radius);
        }
        Geo::Path(path) => {
            out.extend_from_slice(&(path.p.len() as i32).to_le_bytes());
            out.extend_from_slice(&(path.closed as i32).to_le_bytes());
            out.extend_from_slice(&0i32.to_le_bytes());
            for pt in &path.p {
                p(&mut out, pt);
            }
        }
        Geo::Polygon(poly) => {
            out.extend_from_slice(&(poly.p.len() as i32).to_le_bytes());
            p(&mut out, &poly.boundbox.high);
            p(&mut out, &poly.boundbox.low);
            for pt in &poly.p {
                p(&mut out, pt);
            }
        }
    }
    out
}

/// TSVectorData after its header: the count, a WordEntry per lexeme (haspos, length,
/// offset), then each lexeme with its positions after a 2-byte alignment.
fn tsvector(t: &crate::types::tsearch::TsVector) -> Vec<u8> {
    let mut entries = Vec::with_capacity(t.lexemes.len());
    let mut data: Vec<u8> = Vec::new();
    for l in &t.lexemes {
        let pos = data.len() as u32;
        data.extend_from_slice(l.word.as_bytes());
        let haspos = !l.pos.is_empty();
        if haspos {
            if data.len() % 2 == 1 {
                data.push(0);
            }
            data.extend_from_slice(&(l.pos.len() as u16).to_le_bytes());
            for p in &l.pos {
                data.extend_from_slice(&p.to_le_bytes());
            }
        }
        entries.push(haspos as u32 | ((l.word.len() as u32 & 0x7ff) << 1) | (pos << 12));
    }
    let mut out = (t.lexemes.len() as i32).to_le_bytes().to_vec();
    for e in entries {
        out.extend_from_slice(&e.to_le_bytes());
    }
    out.extend(data);
    out
}

/// TSQueryData after its header: the count, the items in polish notation (12 bytes each),
/// then the operands' strings, each NUL-terminated, in the order the query reads them.
fn tsquery(q: &crate::types::tsearch::TsQuery) -> Vec<u8> {
    use crate::types::tsearch::{OP_NOT, QueryItem};
    // Operands' offsets into the strings, assigned left to right.
    let mut order = Vec::new();
    fn visit(items: &[QueryItem], i: usize, order: &mut Vec<usize>, depth: usize) {
        if depth > items.len() {
            return;
        }
        match items.get(i) {
            Some(QueryItem::Val(_)) => order.push(i),
            Some(QueryItem::Opr { oper, left, .. }) => {
                if *oper != OP_NOT {
                    visit(items, i + *left as usize, order, depth + 1);
                }
                visit(items, i + 1, order, depth + 1);
            }
            _ => {}
        }
    }
    visit(&q.items, 0, &mut order, 0);
    let mut dist = vec![0u32; q.items.len()];
    let mut strings = Vec::new();
    for i in order {
        if let QueryItem::Val(o) = &q.items[i] {
            dist[i] = strings.len() as u32;
            strings.extend_from_slice(o.word.as_bytes());
            strings.push(0);
        }
    }
    let mut out = (q.items.len() as i32).to_le_bytes().to_vec();
    for (i, it) in q.items.iter().enumerate() {
        let mut b = [0u8; 12];
        match it {
            QueryItem::Val(o) => {
                b[0] = 1;
                b[1] = o.weight;
                b[2] = o.prefix as u8;
                b[4..8].copy_from_slice(&o.valcrc.to_le_bytes());
                let packed = (o.word.len() as u32 & 0xfff) | (dist[i] << 12);
                b[8..12].copy_from_slice(&packed.to_le_bytes());
            }
            QueryItem::Opr {
                oper,
                distance,
                left,
            } => {
                b[0] = 2;
                b[1] = *oper;
                b[2..4].copy_from_slice(&distance.to_le_bytes());
                b[4..8].copy_from_slice(&left.to_le_bytes());
            }
            QueryItem::Stop => {}
        }
        out.extend_from_slice(&b);
    }
    out.extend(strings);
    out
}

/// A composite value as `heap_form_tuple` builds it (HeapTupleHeader with the type's oid,
/// then the attributes, varlenas packed), after the length word.
fn composite(
    r: &crate::types::RowType,
    vals: &[Value],
    enum_oid: &dyn Fn(u32, &str) -> u32,
) -> Option<Vec<u8>> {
    let natts = r.fields.len();
    if vals.len() != natts {
        return None;
    }
    let has_null = vals.iter().any(Value::is_null);
    let varwidth = r
        .fields
        .iter()
        .zip(vals)
        .any(|((_, t), v)| !v.is_null() && meta(t).is_some_and(|m| m.len < 0));
    let hoff = (23 + if has_null { natts.div_ceil(8) } else { 0 }).div_ceil(8) * 8;
    let mut out = Vec::new();
    out.extend_from_slice(&(-1i32).to_le_bytes());
    out.extend_from_slice(&r.oid.to_le_bytes());
    out.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0, 0]);
    out.extend_from_slice(&(natts as u16).to_le_bytes());
    let infomask = has_null as u16 | if varwidth { 0x0002 } else { 0 };
    out.extend_from_slice(&infomask.to_le_bytes());
    out.push(hoff as u8);
    if has_null {
        let mut bits = vec![0u8; natts.div_ceil(8)];
        for (i, v) in vals.iter().enumerate() {
            if !v.is_null() {
                bits[i / 8] |= 1 << (i % 8);
            }
        }
        out.extend(bits);
    }
    out.resize(hoff - 4, 0);
    for ((_, t), v) in r.fields.iter().zip(vals) {
        if !v.is_null() {
            put(&mut out, 4, v, t, true, enum_oid)?;
        }
    }
    Some(out)
}
