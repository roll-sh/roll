//! Sequential scans of a modelled heap whose rows aren't stored in heap order (an update
//! moved some, an insert filled free space), returned in heap (TID) order without holding
//! every row: the rows whose TIDs increase with their keys (the longest such run) stream in
//! storage order, and only the others ("displaced") are held, merged in by TID.
//!
//! The range is read at the statement's snapshot, which sees the same rows each time: once
//! for the rows' TIDs (8 bytes a row, dropped once the run is chosen), once more for the
//! displaced rows a TID earlier than a run row before them ("early": a row put in free
//! space) when there are any, then to return them. A displaced row of a later TID (a row
//! an update moved) is held from where the last read meets it until its turn. PostgreSQL
//! reads its heap a page at a time; holding every row to sort them by TID took 13 MB a
//! session in agent 125's 100-session grouping bench.
use bytes::Bytes;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

/// A row's TID as one sortable number.
pub(super) fn tid_rank((b, o): (u32, u16)) -> u64 {
    (b as u64) << 16 | o as u64
}

/// A held row: its TID, key and stored bytes.
pub(super) type Held = (u64, Vec<u8>, Bytes);

/// Which rows (by position in storage order) are outside the longest run whose TIDs
/// increase (`displaced`), and which of those come before a run row stored ahead of them
/// (`early`): two bit sets.
pub(super) fn displaced(tids: &[u64]) -> (Vec<u64>, Vec<u64>) {
    const NONE: u32 = u32::MAX;
    // (Patience sorting: `tails[k]` is the row ending the run of length k + 1 with the
    // smallest last TID; `prev` links each row to the one before it in its run.)
    let mut tails: Vec<u32> = Vec::new();
    let mut prev: Vec<u32> = Vec::with_capacity(tids.len());
    for (i, &t) in tids.iter().enumerate() {
        let j = tails.partition_point(|&x| tids[x as usize] < t);
        prev.push(if j > 0 { tails[j - 1] } else { NONE });
        if j == tails.len() {
            tails.push(i as u32);
        } else {
            tails[j] = i as u32;
        }
    }
    let words = tids.len().div_ceil(64);
    let mut bits = vec![!0u64; words];
    let mut at = tails.last().copied().unwrap_or(NONE);
    while at != NONE {
        bits[at as usize / 64] &= !(1 << (at % 64));
        at = prev[at as usize];
    }
    let mut early = vec![0u64; words];
    let mut last = None;
    for (i, &t) in tids.iter().enumerate() {
        if !is_set(&bits, i) {
            last = Some(t);
        } else if last.is_some_and(|l| t < l) {
            early[i / 64] |= 1 << (i % 64);
        }
    }
    (bits, early)
}

pub(super) fn is_set(bits: &[u64], i: usize) -> bool {
    bits.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
}

/// A heap-order scan's state between its reads.
pub(super) enum Stream {
    /// Reading the rows' TIDs.
    Tids(Vec<u64>),
    /// Returning rows: those of the run as storage gives them (`pos`: the next one's
    /// position; `ready`: the TID of the one at the batch's cursor, not yet returned), each
    /// after the held displaced rows of lesser TIDs: the early ones (read beforehand, by
    /// TID) and the late ones met so far.
    Merge {
        bits: Vec<u64>,
        early_bits: Vec<u64>,
        pos: usize,
        ready: Option<u64>,
        early: VecDeque<Held>,
        late: BinaryHeap<Reverse<Held>>,
    },
}

impl Stream {
    /// The held row of the least TID, when it is below `t` (None: none).
    pub(super) fn pop_below(early: &mut VecDeque<Held>, late: &mut BinaryHeap<Reverse<Held>>, t: Option<u64>) -> Option<Held> {
        let e = early.front().map(|r| r.0);
        let l = late.peek().map(|r| r.0.0);
        let below = |x: u64| t.is_none_or(|t| x < t);
        match (e, l) {
            (Some(e), Some(l)) if l < e => below(l).then(|| late.pop().expect("a row").0),
            (Some(e), _) => below(e).then(|| early.pop_front().expect("a row")),
            (None, Some(l)) => below(l).then(|| late.pop().expect("a row").0),
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(bits: &[u64], n: usize) -> Vec<usize> {
        (0..n).filter(|&i| is_set(bits, i)).collect()
    }

    #[test]
    fn displaced_rows_are_outside_the_longest_run() {
        // An updated row moved to the end (key 1: late), a row put in free space (key 7: early).
        let tids = [10, 900, 11, 12, 13, 14, 15, 2];
        let (bits, early) = displaced(&tids);
        assert_eq!(set(&bits, 8), vec![1, 7]);
        assert_eq!(set(&early, 8), vec![7]);
        // A key range updated: the shorter run is the displaced one, all late.
        let tids = [100, 101, 102, 3, 4, 5, 6, 7];
        let (bits, early) = displaced(&tids);
        assert_eq!(set(&bits, 8), vec![0, 1, 2]);
        assert!(set(&early, 8).is_empty());
        assert!(displaced(&[]).0.is_empty());
        assert_eq!(tid_rank((1, 2)), 65538);
    }
}
