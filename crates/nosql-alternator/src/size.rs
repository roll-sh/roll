//! Item sizes and consumed capacity, as Alternator computes them.
//!
//! An item's size is the sum, over its attributes, of the name's bytes and the stored value's
//! bytes: the CQL serialisation of a key column, or an `:attrs` blob without its type tag (so a
//! string is its UTF-8 bytes, a binary its raw bytes, a boolean 1, a number its CQL `decimal`
//! encoding, and anything else its compact JSON). ScyllaDB enforces no 400 KB item limit.
//!
//! - Reads cost `ceil(size / 4096)` units, halved unless `ConsistentRead`; a missing item costs
//!   as much as a 1-byte one.
//! - Writes cost `max(1, ceil(size / 1024))` units.

use crate::json::Json;
use crate::storage::{encode_key, encode_value};
use crate::value::Item;

/// Bytes of one stored attribute value.
pub fn value_size(v: &Json, is_key: bool) -> usize {
    if is_key && let Some(b) = encode_key(v) {
        return b.len();
    }
    encode_value(v).len().saturating_sub(1)
}

/// Size of an item. Key attributes are measured as key columns when `keys` names them.
pub fn item_size_with_keys(item: &Item, keys: &[&str]) -> usize {
    item.iter()
        .map(|(k, v)| k.len() + value_size(v, keys.contains(&k.as_str())))
        .sum()
}

/// Size of an item, every attribute measured as an `:attrs` value (key columns of type S and
/// B have the same size either way; N keys too, as both use the decimal encoding).
pub fn item_size(item: &Item) -> usize {
    item_size_with_keys(item, &[])
}

/// Read capacity units for an item of `size` bytes (`None`: missing item).
pub fn read_units(size: Option<usize>, consistent: bool) -> f64 {
    let units = size.unwrap_or(1).max(1).div_ceil(4096) as f64;
    if consistent { units } else { units * 0.5 }
}

/// Write capacity units for an item of `size` bytes.
pub fn write_units(size: usize) -> u64 {
    size.div_ceil(1024).max(1) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn sizes() {
        let item: Item = parse(br#"{"p":{"S":"abc"},"n":{"N":"1.50"},"b":{"BOOL":true},"x":{"B":"AQID"},"l":{"L":[]}}"#)
            .unwrap()
            .as_object()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        // p:1+3, n:1+(4+2), b:1+1, x:1+3, l:1+len({"L":[]})=1+8
        assert_eq!(item_size(&item), 4 + 7 + 2 + 4 + 9);
        assert_eq!(read_units(Some(4096), true), 1.0);
        assert_eq!(read_units(Some(4097), false), 1.0);
        assert_eq!(read_units(None, false), 0.5);
        assert_eq!(write_units(0), 1);
        assert_eq!(write_units(1025), 2);
    }
}
