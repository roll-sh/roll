//! Index sorting: the doc order of `index.sort.*`.
//!
//! [`sort_doc_map`] reads the sort fields' doc values from a source and returns the permutation
//! that orders docs by the sort keys. Elasticsearch semantics: numeric keys compare as signed
//! longs (doubles and floats are stored as sortable bits, so this is their numeric order),
//! keyword keys compare by ordinal (term order), multi-valued docs use their min or max value,
//! and `missing: _first`/`_last` refer to the final order regardless of `reverse`. Ties keep the
//! original doc order (stable sort), so flushing the same docs twice gives the same segment.

use std::cmp::Ordering;

use crate::error::{Result, invalid};
use crate::info::{IndexSort, SortMissing, SortSelector};
use crate::source::{DocMap, DocValuesSource, SegmentSource};

/// Sort keys of one field: a value per doc plus a bitset of docs that have one (8 bytes and a
/// bit per doc, rather than 16 for `Option<i64>`).
struct Keys {
    values: Vec<i64>,
    present: Vec<u64>,
}

impl Keys {
    #[inline]
    fn get(&self, doc: u32) -> Option<i64> {
        let d = doc as usize;
        (self.present[d >> 6] >> (d & 63) & 1 != 0).then(|| self.values[d])
    }

    fn set(&mut self, doc: u32, v: Option<i64>) {
        let d = doc as usize;
        if d < self.values.len()
            && let Some(v) = v
        {
            self.values[d] = v;
            self.present[d >> 6] |= 1 << (d & 63);
        }
    }
}

fn keys(src: &dyn SegmentSource, field: u32, selector: SortSelector) -> Result<Keys> {
    let n = src.max_doc() as usize;
    let mut k = Keys { values: vec![0; n], present: vec![0; n.div_ceil(64)] };
    let pick = |vs: &mut dyn Iterator<Item = i64>| match selector {
        SortSelector::Min => vs.min(),
        SortSelector::Max => vs.max(),
    };
    match src.doc_values(field)? {
        None => {}
        Some(DocValuesSource::Numeric(it)) => {
            for (d, v) in it {
                k.set(d, Some(v));
            }
        }
        Some(DocValuesSource::SortedNumeric(mut s)) => {
            while let Some((d, vs)) = s.next_doc() {
                k.set(d, pick(&mut vs.iter().copied()));
            }
        }
        Some(DocValuesSource::Sorted(mut s)) => {
            while let Some((d, o)) = s.next_doc() {
                k.set(d, Some(o as i64));
            }
        }
        Some(DocValuesSource::SortedSet(mut s)) => {
            while let Some((d, os)) = s.next_doc() {
                k.set(d, pick(&mut os.iter().map(|&o| o as i64)));
            }
        }
        Some(DocValuesSource::Binary(_)) => {
            return Err(invalid(format!(
                "cannot sort the index on binary doc values field [{field}]"
            )));
        }
    }
    Ok(k)
}

/// The doc permutation that sorts `src` by `sort`.
pub fn sort_doc_map(src: &dyn SegmentSource, sort: &IndexSort) -> Result<DocMap> {
    let max_doc = src.max_doc();
    let all: Vec<Keys> = sort
        .fields
        .iter()
        .map(|f| keys(src, f.field, f.selector))
        .collect::<Result<_>>()?;
    let mut order: Vec<u32> = (0..max_doc).collect();
    order.sort_by(|&a, &b| {
        for (f, k) in sort.fields.iter().zip(&all) {
            let o = match (k.get(a), k.get(b)) {
                (Some(x), Some(y)) => {
                    let o = x.cmp(&y);
                    if f.reverse { o.reverse() } else { o }
                }
                (None, None) => Ordering::Equal,
                (None, Some(_)) => {
                    if f.missing == SortMissing::First {
                        Ordering::Less
                    } else {
                        Ordering::Greater
                    }
                }
                (Some(_), None) => {
                    if f.missing == SortMissing::First {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    }
                }
            };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    });
    DocMap::from_new_to_old(order, max_doc)
}
