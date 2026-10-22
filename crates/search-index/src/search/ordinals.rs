//! Global ordinals (Lucene's `OrdinalMap`): one ordinal space for a sorted / sorted-set doc
//! values field across all segments of a searcher, in term order.
//!
//! Built once per searcher and field (cached on the searcher, so every query and aggregation
//! over the same point-in-time view shares it) by a k-way merge of the segments' term
//! dictionaries: the iterators' current terms are compared in place, nothing is copied but the
//! winning term per global ordinal's first occurrence position. A single segment needs no map
//! (segment ordinals are global). Joins (`has_child` / `has_parent`) and terms aggregations
//! key per-value state by global ordinal.

use search_codec::{DocValuesReader, TermDict};

use crate::search::Searcher;

/// The global ordinal space of one field.
#[derive(Debug)]
pub struct GlobalOrdinals {
    /// Per segment: segment ordinal → global ordinal. Empty for a single segment (identity)
    /// and for segments without the field.
    maps: Vec<Vec<u32>>,
    /// Per global ordinal: the segment index and segment ordinal of its first occurrence.
    first: Vec<(u32, u32)>,
    value_count: u64,
    identity: bool,
}

/// The term dictionary of a sorted or sorted-set doc values field.
pub(crate) fn dict(dv: &DocValuesReader) -> Option<&TermDict> {
    match dv {
        DocValuesReader::Sorted(s) => Some(s.dict()),
        DocValuesReader::SortedSet(s) => Some(s.dict()),
        _ => None,
    }
}

impl GlobalOrdinals {
    /// Builds the map over the searcher's segments; `None` when no segment has sorted values
    /// for the field.
    pub fn build(s: &Searcher, field: u32) -> Option<GlobalOrdinals> {
        let dicts: Vec<Option<&TermDict>> = s.segments().iter().map(|seg| seg.reader.doc_values(field).and_then(dict)).collect();
        if dicts.iter().all(|d| d.is_none()) {
            return None;
        }
        if dicts.len() == 1 {
            let n = dicts[0].map_or(0, |d| d.len());
            return Some(GlobalOrdinals { maps: vec![Vec::new()], first: Vec::new(), value_count: n, identity: true });
        }
        let mut maps: Vec<Vec<u32>> = dicts.iter().map(|d| Vec::with_capacity(d.map_or(0, |d| d.len() as usize))).collect();
        let mut iters: Vec<_> = dicts.iter().map(|d| d.map(|d| d.iter_from(0))).collect();
        // Heap of segment indexes whose iterator holds a term, smallest term on top.
        let mut heap: Vec<usize> = Vec::with_capacity(iters.len());
        for (i, it) in iters.iter_mut().enumerate() {
            if let Some(it) = it
                && it.next().is_some()
            {
                heap.push(i);
            }
        }
        let less = |iters: &[Option<search_codec::docvalues::TermDictIter<'_>>], a: usize, b: usize| -> bool {
            let (x, y) = (iters[a].as_ref().unwrap().term(), iters[b].as_ref().unwrap().term());
            x < y || (x == y && a < b)
        };
        fn sift_down(heap: &mut [usize], mut i: usize, less: &dyn Fn(usize, usize) -> bool) {
            loop {
                let (l, r) = (2 * i + 1, 2 * i + 2);
                let mut m = i;
                if l < heap.len() && less(heap[l], heap[m]) {
                    m = l;
                }
                if r < heap.len() && less(heap[r], heap[m]) {
                    m = r;
                }
                if m == i {
                    return;
                }
                heap.swap(i, m);
                i = m;
            }
        }
        for i in (0..heap.len() / 2).rev() {
            sift_down(&mut heap, i, &|a, b| less(&iters, a, b));
        }
        let mut first: Vec<(u32, u32)> = Vec::new();
        let mut last: Vec<u8> = Vec::new();
        while let Some(&top) = heap.first() {
            let (term_is_new, ord) = {
                let it = iters[top].as_ref().unwrap();
                (first.is_empty() || it.term() != last.as_slice(), it.ord())
            };
            if term_is_new {
                last.clear();
                last.extend_from_slice(iters[top].as_ref().unwrap().term());
                first.push((top as u32, ord as u32));
            }
            maps[top].push((first.len() - 1) as u32);
            if iters[top].as_mut().unwrap().next().is_none() {
                let n = heap.len() - 1;
                heap.swap(0, n);
                heap.pop();
            }
            sift_down(&mut heap, 0, &|a, b| less(&iters, a, b));
        }
        let value_count = first.len() as u64;
        Some(GlobalOrdinals { maps, first, value_count, identity: false })
    }

    /// Number of distinct values across segments.
    pub fn value_count(&self) -> u64 {
        self.value_count
    }

    /// The global ordinal of segment `seg`'s ordinal `ord`.
    #[inline]
    pub fn global(&self, seg: u32, ord: u64) -> u64 {
        if self.identity { ord } else { self.maps[seg as usize][ord as usize] as u64 }
    }

    /// Segment ordinal → global ordinal for one segment (`None`: identity).
    pub fn segment_map(&self, seg: u32) -> Option<&[u32]> {
        if self.identity { None } else { self.maps.get(seg as usize).map(|v| v.as_slice()) }
    }

    /// The value of a global ordinal.
    pub fn lookup(&self, s: &Searcher, field: u32, global: u64, out: &mut Vec<u8>) {
        out.clear();
        let (seg, ord) = if self.identity { (0, global) } else { (self.first[global as usize].0, self.first[global as usize].1 as u64) };
        match s.segments()[seg as usize].reader.doc_values(field) {
            Some(DocValuesReader::Sorted(v)) => v.lookup_ord(ord, out),
            Some(DocValuesReader::SortedSet(v)) => v.lookup_ord(ord, out),
            _ => {}
        }
    }

    /// Approximate heap bytes.
    pub fn ram_bytes(&self) -> usize {
        self.maps.iter().map(|m| m.capacity() * 4).sum::<usize>() + self.first.capacity() * 8 + 64
    }
}
