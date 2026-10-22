//! Index sorting that keeps nested blocks together.
//!
//! The codec's `sort_doc_map` orders single docs. With nested documents a parent must stay
//! right after its children (block joins rely on it), so — like Lucene's parent-field index
//! sort — whole blocks are ordered by their root doc's sort key, children keeping their order
//! inside the block. Keys follow Elasticsearch: numeric keys compare as longs, keyword keys by
//! ordinal, multi-valued docs use min or max, `missing` refers to the final order, and ties keep
//! the original order.

use std::cmp::Ordering;

use search_codec::source::{DocMap, DocValuesSource, SegmentSource};
use search_codec::{IndexSort, Result as CResult, SortMissing, SortSelector, sort_doc_map};

use crate::util::bitset::FixedBitSet;

fn keys(src: &dyn SegmentSource, field: u32, sel: SortSelector) -> CResult<Vec<Option<i64>>> {
    let mut k = vec![None; src.max_doc() as usize];
    let pick = |vs: &mut dyn Iterator<Item = i64>| match sel {
        SortSelector::Min => vs.min(),
        SortSelector::Max => vs.max(),
    };
    let mut set = |d: u32, v: Option<i64>| {
        if let Some(slot) = k.get_mut(d as usize) {
            *slot = v;
        }
    };
    match src.doc_values(field)? {
        None => {}
        Some(DocValuesSource::Numeric(it)) => {
            for (d, v) in it {
                set(d, Some(v));
            }
        }
        Some(DocValuesSource::SortedNumeric(mut s)) => {
            while let Some((d, vs)) = s.next_doc() {
                set(d, pick(&mut vs.iter().copied()));
            }
        }
        Some(DocValuesSource::Sorted(mut s)) => {
            while let Some((d, o)) = s.next_doc() {
                set(d, Some(o as i64));
            }
        }
        Some(DocValuesSource::SortedSet(mut s)) => {
            while let Some((d, os)) = s.next_doc() {
                set(d, pick(&mut os.iter().map(|&o| o as i64)));
            }
        }
        Some(DocValuesSource::Binary(_)) => {
            return Err(search_codec::Error::InvalidArgument(format!(
                "cannot sort the index on binary doc values field [{field}]"
            )));
        }
    }
    Ok(k)
}

/// The doc map sorting `src` by `sort`; with `roots`, blocks (children before their root)
/// move as a whole, keyed by the root.
pub fn block_sort_map(src: &dyn SegmentSource, sort: &IndexSort, roots: Option<&FixedBitSet>) -> CResult<DocMap> {
    let Some(roots) = roots else {
        return sort_doc_map(src, sort);
    };
    let max_doc = src.max_doc();
    let all: Vec<Vec<Option<i64>>> = sort.fields.iter().map(|f| keys(src, f.field, f.selector)).collect::<CResult<_>>()?;
    // Blocks: (first doc, root doc); docs after the last root form a block keyed by its last doc.
    let mut blocks: Vec<(u32, u32)> = Vec::new();
    let mut start = 0u32;
    for d in 0..max_doc {
        if roots.get(d) {
            blocks.push((start, d));
            start = d + 1;
        }
    }
    if start < max_doc {
        blocks.push((start, max_doc - 1));
    }
    blocks.sort_by(|a, b| {
        for (f, k) in sort.fields.iter().zip(&all) {
            let (x, y) = (k[a.1 as usize], k[b.1 as usize]);
            let o = match (x, y) {
                (Some(x), Some(y)) => {
                    let o = x.cmp(&y);
                    if f.reverse { o.reverse() } else { o }
                }
                (None, None) => Ordering::Equal,
                (None, Some(_)) => {
                    if f.missing == SortMissing::First { Ordering::Less } else { Ordering::Greater }
                }
                (Some(_), None) => {
                    if f.missing == SortMissing::First { Ordering::Greater } else { Ordering::Less }
                }
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    });
    let mut order = Vec::with_capacity(max_doc as usize);
    for (s, e) in blocks {
        order.extend(s..=e);
    }
    DocMap::from_new_to_old(order, max_doc)
}
