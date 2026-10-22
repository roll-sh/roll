//! Range field values (`integer_range`, `long_range`, `float_range`, `double_range`,
//! `date_range`, `ip_range`): bounds made inclusive the way `RangeFieldMapper` does (exclusive
//! integers step by one, floats by one ulp, IPs by one address), packed as Lucene's
//! `*Range` points (all mins, then all maxes) and encoded as `BinaryRangeUtil` doc values.

use super::fields::{FieldInfo, Kind};
use crate::error::{EsError, EsResult};
use crate::json::Value;
use search_codec::points::encode;

/// One bound of a range.
#[derive(Clone, Debug, PartialEq)]
pub struct Bound {
    /// The bound as one dimension of the range's point.
    pub packed: Vec<u8>,
    pub value: BoundValue,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BoundValue {
    Long(i64),
    Double(f64),
    Float(f32),
    Ip([u8; 16]),
}

fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Num(n) => Some(n.as_f64()),
        Value::Str(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn ip_step(mut b: [u8; 16], up: bool) -> [u8; 16] {
    for i in (0..16).rev() {
        if up {
            let (v, carry) = b[i].overflowing_add(1);
            b[i] = v;
            if !carry {
                break;
            }
        } else {
            let (v, borrow) = b[i].overflowing_sub(1);
            b[i] = v;
            if !borrow {
                break;
            }
        }
    }
    b
}

/// CIDR `addr/bits` to its first and last address.
pub fn cidr(s: &str) -> EsResult<([u8; 16], [u8; 16])> {
    let (addr, bits) = s.split_once('/').ok_or_else(|| EsError::illegal_argument(format!("Expected [ip/prefix] but was [{s}]")))?;
    let b = crate::document::values::parse_ip(addr).ok_or_else(|| EsError::illegal_argument(format!("'{addr}' is not an IP string literal.")))?;
    let bits: u32 = bits.parse().map_err(|_| EsError::number_format(bits))?;
    let v4 = !addr.contains(':');
    let total = if v4 { bits + 96 } else { bits };
    let mut lo = b;
    let mut hi = b;
    for i in 0..128u32 {
        if i >= total {
            let byte = (i / 8) as usize;
            let bit = 7 - (i % 8);
            lo[byte] &= !(1 << bit);
            hi[byte] |= 1 << bit;
        }
    }
    Ok((lo, hi))
}

/// The inclusive bounds of a range value.
pub fn bounds(f: &FieldInfo, k: Kind, from: &Value, to: &Value, include_from: bool, include_to: bool) -> EsResult<(Bound, Bound)> {
    let bad = |v: &Value| EsError::illegal_argument(format!("failed to parse range bound [{}]", v.to_es_string()));
    Ok(match k {
        Kind::IntegerRange => {
            // A missing bound is the type's extreme, still stepped when exclusive (`Range`).
            let lo = if from.is_null() { i32::MIN as i64 } else { num(from).ok_or_else(|| bad(from))? as i64 } + if include_from { 0 } else { 1 };
            let hi = if to.is_null() { i32::MAX as i64 } else { num(to).ok_or_else(|| bad(to))? as i64 } - if include_to { 0 } else { 1 };
            let (lo, hi) = (lo.clamp(i32::MIN as i64, i32::MAX as i64), hi.clamp(i32::MIN as i64, i32::MAX as i64));
            (
                Bound { packed: encode::i32_bytes(lo as i32).to_vec(), value: BoundValue::Long(lo) },
                Bound { packed: encode::i32_bytes(hi as i32).to_vec(), value: BoundValue::Long(hi) },
            )
        }
        Kind::LongRange | Kind::DateRange => {
            let parse = |v: &Value, up: bool| -> EsResult<i64> {
                if k == Kind::DateRange {
                    // Parsed (and rounded) by the document parser already.
                    if let Value::Num(crate::json::Number::Int(i)) = v {
                        return Ok(*i);
                    }
                    let text = match v {
                        Value::Str(s) => s.clone(),
                        other => other.to_es_string(),
                    };
                    let fmt = f.date_format.clone().ok_or_else(|| bad(v))?;
                    let i = fmt.parse_detailed(&text, None, up).map_err(|e| e.to_es())?;
                    Ok(i.millis())
                } else {
                    match v {
                        Value::Num(crate::json::Number::Int(i)) => Ok(*i),
                        _ => num(v).map(|x| x as i64).ok_or_else(|| bad(v)),
                    }
                }
            };
            let lo = if from.is_null() { i64::MIN } else { parse(from, false)? }.saturating_add(if include_from { 0 } else { 1 });
            let hi = if to.is_null() { i64::MAX } else { parse(to, true)? }.saturating_sub(if include_to { 0 } else { 1 });
            (Bound { packed: encode::i64_bytes(lo).to_vec(), value: BoundValue::Long(lo) }, Bound { packed: encode::i64_bytes(hi).to_vec(), value: BoundValue::Long(hi) })
        }
        Kind::FloatRange => {
            let lo = if from.is_null() { f32::NEG_INFINITY } else { num(from).ok_or_else(|| bad(from))? as f32 };
            let hi = if to.is_null() { f32::INFINITY } else { num(to).ok_or_else(|| bad(to))? as f32 };
            let lo = if include_from { lo } else { lo.next_up() };
            let hi = if include_to { hi } else { hi.next_down() };
            (Bound { packed: encode::f32_bytes(lo).to_vec(), value: BoundValue::Float(lo) }, Bound { packed: encode::f32_bytes(hi).to_vec(), value: BoundValue::Float(hi) })
        }
        Kind::DoubleRange => {
            let lo = if from.is_null() { f64::NEG_INFINITY } else { num(from).ok_or_else(|| bad(from))? };
            let hi = if to.is_null() { f64::INFINITY } else { num(to).ok_or_else(|| bad(to))? };
            let lo = if include_from { lo } else { lo.next_up() };
            let hi = if include_to { hi } else { hi.next_down() };
            (Bound { packed: encode::f64_bytes(lo).to_vec(), value: BoundValue::Double(lo) }, Bound { packed: encode::f64_bytes(hi).to_vec(), value: BoundValue::Double(hi) })
        }
        Kind::IpRange => {
            if let (Value::Str(a), Value::Str(b)) = (from, to)
                && a == b
                && a.contains('/')
            {
                let (lo, hi) = cidr(a)?;
                return Ok((Bound { packed: lo.to_vec(), value: BoundValue::Ip(lo) }, Bound { packed: hi.to_vec(), value: BoundValue::Ip(hi) }));
            }
            let ip = |v: &Value| -> EsResult<[u8; 16]> {
                let s = v.as_str().map(str::to_string).unwrap_or_else(|| v.to_es_string());
                crate::document::values::parse_ip(&s).ok_or_else(|| EsError::illegal_argument(format!("'{s}' is not an IP string literal.")))
            };
            let lo = if from.is_null() { [0u8; 16] } else { ip(from)? };
            let hi = if to.is_null() { [0xffu8; 16] } else { ip(to)? };
            let lo = if include_from { lo } else { ip_step(lo, true) };
            let hi = if include_to { hi } else { ip_step(hi, false) };
            (Bound { packed: lo.to_vec(), value: BoundValue::Ip(lo) }, Bound { packed: hi.to_vec(), value: BoundValue::Ip(hi) })
        }
        _ => return Err(EsError::illegal_argument(format!("field [{}] is not a range field", f.name))),
    })
}

/// The `BinaryRangeUtil` doc value of a doc's ranges.
pub fn encode_dv(k: Kind, all: &[(Bound, Bound)]) -> Vec<u8> {
    match k {
        Kind::IntegerRange | Kind::LongRange | Kind::DateRange => {
            let v: Vec<(i64, i64)> = all
                .iter()
                .map(|(a, b)| match (a.value, b.value) {
                    (BoundValue::Long(x), BoundValue::Long(y)) => (x, y),
                    _ => (0, 0),
                })
                .collect();
            search_aggs::range_field::encode_long_ranges(&v)
        }
        Kind::FloatRange => {
            let v: Vec<(f32, f32)> = all
                .iter()
                .map(|(a, b)| match (a.value, b.value) {
                    (BoundValue::Float(x), BoundValue::Float(y)) => (x, y),
                    _ => (0.0, 0.0),
                })
                .collect();
            search_aggs::range_field::encode_float_ranges(&v)
        }
        Kind::DoubleRange => {
            let v: Vec<(f64, f64)> = all
                .iter()
                .map(|(a, b)| match (a.value, b.value) {
                    (BoundValue::Double(x), BoundValue::Double(y)) => (x, y),
                    _ => (0.0, 0.0),
                })
                .collect();
            search_aggs::range_field::encode_double_ranges(&v)
        }
        Kind::IpRange => {
            let v: Vec<([u8; 16], [u8; 16])> = all
                .iter()
                .map(|(a, b)| match (a.value, b.value) {
                    (BoundValue::Ip(x), BoundValue::Ip(y)) => (x, y),
                    _ => ([0; 16], [0; 16]),
                })
                .collect();
            search_aggs::range_field::encode_ip_ranges(&v)
        }
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_bounds() {
        let (lo, hi) = cidr("192.168.0.0/16").unwrap();
        assert_eq!(&lo[12..], &[192, 168, 0, 0]);
        assert_eq!(&hi[12..], &[192, 168, 255, 255]);
        assert_eq!(ip_step([0xff; 16], true), [0; 16]);
        let mut one = [0u8; 16];
        one[15] = 1;
        assert_eq!(ip_step(one, false), [0; 16]);
    }
}
