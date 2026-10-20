//! Extended statistics values (pg_ndistinct, pg_dependencies, pg_mcv_list) in Postgres's
//! serialized forms (mvdistinct.c, dependencies.c, mcv.c), and their text output.

use super::datum::{self, Layout};
use super::{DataType, Value};
use std::cmp::Ordering;

const NDISTINCT_MAGIC: u32 = 0xA352BFA4;
const DEPS_MAGIC: u32 = 0xB4549A2C;
const MCV_MAGIC: u32 = 0xE1A651C2;

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn i16(&mut self) -> Option<i16> {
        Some(i16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn f64(&mut self) -> Option<f64> {
        Some(f64::from_bits(u64::from_le_bytes(
            self.take(8)?.try_into().ok()?,
        )))
    }
}

/// statext_ndistinct_serialize: (attnums, ndistinct) per combination.
pub fn ndistinct_bytes(items: &[(Vec<i16>, f64)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(NDISTINCT_MAGIC.to_le_bytes());
    out.extend(1u32.to_le_bytes());
    out.extend((items.len() as u32).to_le_bytes());
    for (attrs, nd) in items {
        out.extend(nd.to_le_bytes());
        out.extend((attrs.len() as i32).to_le_bytes());
        for a in attrs {
            out.extend(a.to_le_bytes());
        }
    }
    out
}

pub(crate) fn ndistinct_items(b: &[u8]) -> Option<Vec<(Vec<i16>, f64)>> {
    let mut r = Reader(b);
    if r.u32()? != NDISTINCT_MAGIC {
        return None;
    }
    r.u32()?;
    let n = r.u32()?;
    (0..n)
        .map(|_| {
            let nd = r.f64()?;
            let k = r.i32()?;
            Some(((0..k).map(|_| r.i16()).collect::<Option<Vec<_>>>()?, nd))
        })
        .collect()
}

/// pg_ndistinct_out: `{"1, 2": 4, ...}`.
pub fn format_ndistinct(b: &[u8]) -> String {
    let items = ndistinct_items(b).unwrap_or_default();
    let body: Vec<String> = items
        .iter()
        .map(|(a, nd)| {
            format!(
                "\"{}\": {}",
                a.iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                *nd as i32
            )
        })
        .collect();
    format!("{{{}}}", body.join(", "))
}

/// statext_dependencies_serialize: (attnums with the implied one last, degree).
pub fn dependencies_bytes(deps: &[(Vec<i16>, f64)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(DEPS_MAGIC.to_le_bytes());
    out.extend(1u32.to_le_bytes());
    out.extend((deps.len() as u32).to_le_bytes());
    for (attrs, degree) in deps {
        out.extend(degree.to_le_bytes());
        out.extend((attrs.len() as i16).to_le_bytes());
        for a in attrs {
            out.extend(a.to_le_bytes());
        }
    }
    out
}

/// statext_dependencies_deserialize: (attnums with the implied one last, degree).
pub(crate) fn dependency_items(b: &[u8]) -> Option<Vec<(Vec<i16>, f64)>> {
    let mut r = Reader(b);
    if r.u32()? != DEPS_MAGIC {
        return None;
    }
    r.u32()?;
    let n = r.u32()?;
    (0..n)
        .map(|_| {
            let degree = r.f64()?;
            let k = r.i16()?;
            Some(((0..k).map(|_| r.i16()).collect::<Option<Vec<_>>>()?, degree))
        })
        .collect()
}

/// pg_dependencies_out: `{"1 => 2": 1.000000, ...}`.
pub fn format_dependencies(b: &[u8]) -> String {
    let mut r = Reader(b);
    let mut parts = Vec::new();
    let mut read = |r: &mut Reader| -> Option<()> {
        if r.u32()? != DEPS_MAGIC {
            return None;
        }
        r.u32()?;
        for _ in 0..r.u32()? {
            let degree = r.f64()?;
            let k = r.i16()?;
            let attrs: Vec<i16> = (0..k).map(|_| r.i16()).collect::<Option<_>>()?;
            let (implied, given) = attrs.split_last()?;
            let given: Vec<String> = given.iter().map(|a| a.to_string()).collect();
            parts.push(format!(
                "\"{} => {implied}\": {degree:.6}",
                given.join(", ")
            ));
        }
        Some(())
    };
    read(&mut r);
    format!("{{{}}}", parts.join(", "))
}

/// One MCV list item.
#[derive(Clone, Debug)]
pub struct McvItem {
    pub values: Vec<Value>,
    pub nulls: Vec<bool>,
    pub frequency: f64,
    pub base_frequency: f64,
}

/// A dimension of an MCV list: the type (its oid as pg_type has it, and how to handle it).
pub struct McvDim<'a> {
    pub type_oid: u32,
    pub ty: DataType,
    pub cmp: &'a dyn Fn(&Value, &Value) -> Ordering,
}

/// statext_mcv_serialize: each dimension's distinct non-NULL values (sorted), then the items
/// as indexes into them.
pub fn mcv_bytes(
    items: &[McvItem],
    dims: &[McvDim],
    enum_oid: &dyn Fn(u32, &str) -> u32,
) -> Vec<u8> {
    let ndims = dims.len();
    let mut out = Vec::new();
    out.extend(MCV_MAGIC.to_le_bytes());
    out.extend(1u32.to_le_bytes());
    out.extend((items.len() as u32).to_le_bytes());
    out.extend((ndims as i16).to_le_bytes());
    for d in dims {
        out.extend(d.type_oid.to_le_bytes());
    }
    let mut distinct: Vec<Vec<Value>> = Vec::new();
    let mut data: Vec<Vec<u8>> = Vec::new();
    let mut info: Vec<u8> = Vec::new();
    for (dim, d) in dims.iter().enumerate() {
        let mut vals: Vec<Value> = items
            .iter()
            .filter(|it| !it.nulls[dim])
            .map(|it| it.values[dim].clone())
            .collect();
        crate::sort::pg_qsort(&mut vals, |a, b| (d.cmp)(a, b));
        vals.dedup_by(|a, b| (d.cmp)(a, b) == Ordering::Equal);
        let layout = datum::layout(&d.ty);
        let (typlen, byval) = layout.map_or((-1, false), |l| (l.len, l.byval));
        let (mut nbytes, mut aligned) = (0usize, 0usize);
        let mut bytes = Vec::new();
        for v in &vals {
            let raw = datum::encode(v, &d.ty, enum_oid);
            match layout {
                Some(Layout { byval: true, len }) => {
                    nbytes += len as usize;
                    bytes.extend(&raw[..len as usize]);
                }
                Some(Layout { len, .. }) if len > 0 => {
                    nbytes += len as usize;
                    aligned += maxalign(len as usize);
                    bytes.extend(&raw[..len as usize]);
                }
                _ => {
                    nbytes += 4 + raw.len();
                    aligned += maxalign(4 + raw.len());
                    bytes.extend((raw.len() as u32).to_le_bytes());
                    bytes.extend(&raw);
                }
            }
        }
        // DimensionInfo: nvalues, nbytes, nbytes_aligned, typlen, typbyval (and padding).
        let nvalues = if vals.is_empty() { 0 } else { vals.len() };
        let (nbytes, aligned) = if vals.is_empty() {
            (0, 0)
        } else {
            (nbytes, if byval { 0 } else { aligned })
        };
        info.extend((nvalues as i32).to_le_bytes());
        info.extend((nbytes as i32).to_le_bytes());
        info.extend((aligned as i32).to_le_bytes());
        info.extend((typlen as i32).to_le_bytes());
        info.extend([byval as u8, 0, 0, 0]);
        distinct.push(vals);
        data.push(bytes);
    }
    out.extend(info);
    for d in data {
        out.extend(d);
    }
    for it in items {
        out.extend(it.nulls.iter().map(|n| *n as u8));
        out.extend(it.frequency.to_le_bytes());
        out.extend(it.base_frequency.to_le_bytes());
        for (dim, d) in dims.iter().enumerate() {
            let index = if it.nulls[dim] {
                0
            } else {
                distinct[dim]
                    .iter()
                    .position(|v| (d.cmp)(v, &it.values[dim]) == Ordering::Equal)
                    .unwrap_or(0)
            };
            out.extend((index as u16).to_le_bytes());
        }
    }
    out
}

fn maxalign(n: usize) -> usize {
    n.div_ceil(8) * 8
}

/// statext_mcv_deserialize: the dimensions' type oids and the items, values read as the types
/// `type_of` gives for those oids.
pub fn mcv_items(
    b: &[u8],
    type_of: &dyn Fn(u32) -> Option<DataType>,
    enum_label: &dyn Fn(u32, u32) -> Option<String>,
) -> Option<(Vec<u32>, Vec<McvItem>)> {
    let mut r = Reader(b);
    if r.u32()? != MCV_MAGIC {
        return None;
    }
    r.u32()?;
    let nitems = r.u32()? as usize;
    let ndims = r.i16()? as usize;
    let types: Vec<u32> = (0..ndims).map(|_| r.u32()).collect::<Option<_>>()?;
    let mut infos = Vec::new();
    for _ in 0..ndims {
        let nvalues = r.i32()? as usize;
        r.take(12)?;
        r.take(4)?;
        infos.push(nvalues);
    }
    let mut values: Vec<Vec<Value>> = Vec::new();
    for (dim, nvalues) in infos.iter().enumerate() {
        let ty = type_of(types[dim]).unwrap_or(DataType::Text);
        let layout = datum::layout(&ty);
        let mut vals = Vec::with_capacity(*nvalues);
        for _ in 0..*nvalues {
            let raw = match layout {
                Some(Layout { len, .. }) if len > 0 => r.take(len as usize)?,
                _ => {
                    let n = r.u32()? as usize;
                    r.take(n)?
                }
            };
            vals.push(datum::decode(raw, &ty, enum_label)?);
        }
        values.push(vals);
    }
    let mut items = Vec::with_capacity(nitems);
    for _ in 0..nitems {
        let nulls: Vec<bool> = r.take(ndims)?.iter().map(|b| *b != 0).collect();
        let frequency = r.f64()?;
        let base_frequency = r.f64()?;
        let mut vals = Vec::with_capacity(ndims);
        for (dim, null) in nulls.iter().enumerate() {
            let index = r.u16()? as usize;
            vals.push(if *null {
                Value::Null
            } else {
                values[dim].get(index)?.clone()
            });
        }
        items.push(McvItem {
            values: vals,
            nulls,
            frequency,
            base_frequency,
        });
    }
    Some((types, items))
}
