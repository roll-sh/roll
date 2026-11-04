//! Key ranges nobody reads any more: flushes and compactions leave their entries out.
//!
//! The change stream (`repl::log`) names them: what every replication slot confirmed is never
//! read again, so instead of a tombstone per record (written after reading each one back) or a
//! range deletion (`rdel`, which every read checks), its owner moves a floor here and the
//! entries below it go the next time a flush or compaction writes past them. Reads don't look
//! at these ranges: an owner never reads in a range it discarded, at any snapshot, so whether
//! an entry is still there makes no difference. Kept in memory only: after a restart the owner
//! names its floors again (what's on disk meanwhile is only read by nobody). A file whose
//! every key is discarded leaves without being read (`Store::collect_ranges`).
use parking_lot::RwLock;
use std::sync::Arc;

/// The ranges, each [start, end), sorted by start and not overlapping.
#[derive(Default)]
pub struct Discard {
    ranges: RwLock<Arc<Vec<(Vec<u8>, Vec<u8>)>>>,
}

impl Discard {
    /// Everything from `start` to `end` is discarded (a range of this `start` only grows).
    pub fn raise(&self, start: &[u8], end: Vec<u8>) {
        if end.as_slice() <= start {
            return;
        }
        let mut g = self.ranges.write();
        let mut l = g.as_ref().clone();
        match l.iter_mut().find(|r| r.0 == start) {
            Some(r) if r.1 >= end => return,
            Some(r) => r.1 = end,
            None => l.push((start.to_vec(), end)),
        }
        l.sort_by(|a, b| a.0.cmp(&b.0));
        // (Two owners' ranges don't meet; a range grown into the next one takes it.)
        let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(l.len());
        for r in l {
            match out.last_mut() {
                Some(p) if r.0 <= p.1 => {
                    if r.1 > p.1 {
                        p.1 = r.1;
                    }
                }
                _ => out.push(r),
            }
        }
        *g = Arc::new(out);
    }

    /// Whether a discarded range begins under key prefix `p`: its owner's keyspace, which
    /// isn't kept as a hot range (`hot`: its chains would keep what's discarded).
    pub fn claims(&self, p: &[u8]) -> bool {
        self.ranges.read().iter().any(|(s, _)| s.starts_with(p))
    }

    /// A filter for one pass over keys in ascending order.
    pub fn filter(&self) -> Filter {
        Filter { ranges: self.ranges.read().clone(), at: 0 }
    }

    pub fn ranges(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.ranges.read().as_ref().clone()
    }
}

pub struct Filter {
    ranges: Arc<Vec<(Vec<u8>, Vec<u8>)>>,
    at: usize,
}

impl Filter {
    /// Whether user key `uk` is discarded. Keys come in ascending order (a key out of order
    /// may be kept, never dropped wrongly: the filter only moves past ranges).
    pub fn drops(&mut self, uk: &[u8]) -> bool {
        while let Some((s, e)) = self.ranges.get(self.at) {
            if uk < e.as_slice() {
                return uk >= s.as_slice();
            }
            self.at += 1;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floors_grow_and_filter_in_order() {
        let d = Discard::default();
        d.raise(b"c", b"e".to_vec());
        d.raise(b"a", b"b".to_vec());
        d.raise(b"c", b"d".to_vec()); // (lower: ignored)
        assert_eq!(d.ranges(), vec![(b"a".to_vec(), b"b".to_vec()), (b"c".to_vec(), b"e".to_vec())]);
        let mut f = d.filter();
        let got: Vec<bool> = [&b"0"[..], b"a", b"ab", b"b", b"c", b"dz", b"e", b"f"].iter().map(|k| f.drops(k)).collect();
        assert_eq!(got, [false, true, true, false, true, true, false, false]);
        d.raise(b"a", b"cc".to_vec());
        assert_eq!(d.ranges(), vec![(b"a".to_vec(), b"e".to_vec())]);
    }
}
