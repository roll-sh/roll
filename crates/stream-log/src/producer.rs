//! Idempotent and transactional producer state of one partition, as in Kafka's
//! `ProducerStateManager`: the last few batches of every producer (to answer retries with the
//! original offsets) and the first offset of each open transaction (for the last stable offset).

use crate::batch::{Header, NO_SEQUENCE};
use std::collections::{BTreeMap, HashMap, VecDeque};

/// Retried batches up to this far back are recognised as duplicates (Kafka's
/// `max.in.flight.requests.per.connection` limit for idempotence).
pub const DEDUP_WINDOW: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BatchMeta {
    pub first_seq: i32,
    pub last_seq: i32,
    pub first_offset: i64,
    pub last_offset: i64,
    pub max_timestamp: i64,
}

#[derive(Clone, Debug, Default)]
pub struct ProducerEntry {
    pub epoch: i16,
    pub batches: VecDeque<BatchMeta>,
    /// First offset of the producer's open transaction on this partition.
    pub txn_first_offset: Option<i64>,
    pub coordinator_epoch: i32,
    pub last_timestamp: i64,
}

impl ProducerEntry {
    fn last_seq(&self) -> i32 {
        self.batches.back().map(|b| b.last_seq).unwrap_or(NO_SEQUENCE)
    }
}

/// Outcome of checking a producer's batch before appending it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeqCheck {
    /// Append it.
    Append,
    /// A retry of a batch already in the log at these offsets.
    Duplicate(BatchMeta),
    /// The epoch is older than the producer's current one.
    InvalidEpoch,
    /// A gap or reordering in the sequence numbers.
    OutOfOrder,
    /// The epoch matches but the producer's sequence history is gone (e.g. only a transaction
    /// marker is left); the producer should reset its sequence numbers.
    UnknownProducer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbortedTxn {
    pub producer_id: i64,
    pub first_offset: i64,
    /// Offset of the abort marker.
    pub last_offset: i64,
}

#[derive(Debug, Default)]
pub struct ProducerStates {
    producers: HashMap<i64, ProducerEntry>,
    /// Open transactions by first offset → producer id; the smallest bounds the stable offset.
    ongoing: BTreeMap<i64, i64>,
}

fn next_seq(s: i32) -> i32 {
    if s == i32::MAX { 0 } else { s + 1 }
}

impl ProducerStates {
    pub fn get(&self, pid: i64) -> Option<&ProducerEntry> {
        self.producers.get(&pid)
    }

    pub fn len(&self) -> usize {
        self.producers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.producers.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&i64, &ProducerEntry)> {
        self.producers.iter()
    }

    /// First offset of the earliest open transaction.
    pub fn first_unstable_offset(&self) -> Option<i64> {
        self.ongoing.keys().next().copied()
    }

    /// Open transactions as `(producer id, first offset)`.
    pub fn ongoing(&self) -> impl Iterator<Item = (i64, i64)> + '_ {
        self.ongoing.iter().map(|(o, p)| (*p, *o))
    }

    pub fn check(&self, h: &Header) -> SeqCheck {
        if !h.has_producer() {
            return SeqCheck::Append;
        }
        let Some(e) = self.producers.get(&h.producer_id) else {
            // A producer we have no history for (new, or expired): accept whatever it sends.
            return SeqCheck::Append;
        };
        if h.producer_epoch < e.epoch {
            return SeqCheck::InvalidEpoch;
        }
        if h.is_control() {
            return SeqCheck::Append;
        }
        if h.producer_epoch > e.epoch {
            // A bumped epoch starts its sequences over.
            return if h.base_sequence == 0 { SeqCheck::Append } else { SeqCheck::OutOfOrder };
        }
        let (first, last) = (h.base_sequence, h.last_sequence());
        if let Some(b) = e.batches.iter().find(|b| b.first_seq == first && b.last_seq == last) {
            return SeqCheck::Duplicate(*b);
        }
        let current = e.last_seq();
        if current == NO_SEQUENCE {
            return if first == 0 { SeqCheck::Append } else { SeqCheck::UnknownProducer };
        }
        if first == next_seq(current) { SeqCheck::Append } else { SeqCheck::OutOfOrder }
    }

    /// Record an appended batch (already offset-assigned).
    pub fn update(&mut self, h: &Header) {
        if !h.has_producer() {
            return;
        }
        let e = self.producers.entry(h.producer_id).or_default();
        if h.producer_epoch > e.epoch {
            e.epoch = h.producer_epoch;
            e.batches.clear();
        }
        e.last_timestamp = e.last_timestamp.max(h.max_timestamp);
        if h.is_control() {
            return;
        }
        if h.base_sequence != NO_SEQUENCE {
            e.batches.push_back(BatchMeta {
                first_seq: h.base_sequence,
                last_seq: h.last_sequence(),
                first_offset: h.base_offset,
                last_offset: h.last_offset(),
                max_timestamp: h.max_timestamp,
            });
            while e.batches.len() > DEDUP_WINDOW {
                e.batches.pop_front();
            }
        }
        if h.is_transactional() && e.txn_first_offset.is_none() {
            e.txn_first_offset = Some(h.base_offset);
            self.ongoing.insert(h.base_offset, h.producer_id);
        }
    }

    /// A transaction marker was appended at `marker_offset`. Returns the aborted range, if any.
    pub fn end_txn(&mut self, h: &Header, commit: bool, coordinator_epoch: i32) -> Option<AbortedTxn> {
        let e = self.producers.entry(h.producer_id).or_default();
        if h.producer_epoch > e.epoch {
            e.epoch = h.producer_epoch;
            e.batches.clear();
        }
        e.coordinator_epoch = coordinator_epoch;
        e.last_timestamp = e.last_timestamp.max(h.max_timestamp);
        let first = e.txn_first_offset.take()?;
        self.ongoing.remove(&first);
        (!commit).then_some(AbortedTxn { producer_id: h.producer_id, first_offset: first, last_offset: h.base_offset })
    }

    /// Drop producers idle since before `cutoff_ms` that have no open transaction.
    pub fn expire(&mut self, cutoff_ms: i64) -> usize {
        let before = self.producers.len();
        self.producers.retain(|_, e| e.txn_first_offset.is_some() || e.last_timestamp >= cutoff_ms);
        before - self.producers.len()
    }

    /// Forget all producer state.
    pub fn clear(&mut self) {
        self.producers.clear();
        self.ongoing.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::batch::{BatchOptions, NewRecord, TRANSACTIONAL, assign_offset, build, parse_header};
    use bytes::Bytes;

    fn header(pid: i64, epoch: i16, seq: i32, n: usize, offset: i64, attrs: i16) -> Header {
        let recs: Vec<_> = (0..n).map(|_| NewRecord { value: Some(Bytes::from_static(b"x")), ..Default::default() }).collect();
        let mut b = build(&recs, BatchOptions { producer_id: pid, producer_epoch: epoch, base_sequence: seq, attributes: attrs }, 0).to_vec();
        assign_offset(&mut b, offset, 0);
        parse_header(&b).unwrap()
    }

    #[test]
    fn sequences_and_duplicates() {
        let mut s = ProducerStates::default();
        let a = header(1, 0, 0, 3, 0, 0);
        assert_eq!(s.check(&a), SeqCheck::Append);
        s.update(&a);
        // Retry of the same batch → duplicate with the original offsets.
        assert!(matches!(s.check(&header(1, 0, 0, 3, 99, 0)), SeqCheck::Duplicate(BatchMeta { first_offset: 0, last_offset: 2, .. })));
        assert_eq!(s.check(&header(1, 0, 4, 1, 3, 0)), SeqCheck::OutOfOrder);
        let b = header(1, 0, 3, 2, 3, 0);
        assert_eq!(s.check(&b), SeqCheck::Append);
        s.update(&b);
        assert_eq!(s.check(&header(1, -1, 5, 1, 5, 0)), SeqCheck::InvalidEpoch);
        // New epoch must restart at 0.
        assert_eq!(s.check(&header(1, 1, 5, 1, 5, 0)), SeqCheck::OutOfOrder);
        assert_eq!(s.check(&header(1, 1, 0, 1, 5, 0)), SeqCheck::Append);
        // Unknown producers are accepted at any sequence.
        assert_eq!(s.check(&header(2, 0, 17, 1, 5, 0)), SeqCheck::Append);
    }

    #[test]
    fn dedup_window_is_bounded() {
        let mut s = ProducerStates::default();
        for i in 0..8 {
            let h = header(1, 0, i, 1, i as i64, 0);
            assert_eq!(s.check(&h), SeqCheck::Append);
            s.update(&h);
        }
        assert!(matches!(s.check(&header(1, 0, 7, 1, 0, 0)), SeqCheck::Duplicate(_)));
        assert!(matches!(s.check(&header(1, 0, 3, 1, 0, 0)), SeqCheck::Duplicate(_)));
        // Older than the window: neither a duplicate nor the next sequence.
        assert_eq!(s.check(&header(1, 0, 2, 1, 0, 0)), SeqCheck::OutOfOrder);
    }

    #[test]
    fn transactions_bound_the_stable_offset() {
        let mut s = ProducerStates::default();
        s.update(&header(1, 0, 0, 2, 10, TRANSACTIONAL));
        s.update(&header(2, 0, 0, 1, 12, TRANSACTIONAL));
        s.update(&header(1, 0, 2, 1, 13, TRANSACTIONAL));
        assert_eq!(s.first_unstable_offset(), Some(10));
        let marker = header(1, 0, -1, 1, 14, TRANSACTIONAL | crate::batch::CONTROL);
        assert_eq!(s.end_txn(&marker, false, 0), Some(AbortedTxn { producer_id: 1, first_offset: 10, last_offset: 14 }));
        assert_eq!(s.first_unstable_offset(), Some(12));
        let marker = header(2, 0, -1, 1, 15, TRANSACTIONAL | crate::batch::CONTROL);
        assert_eq!(s.end_txn(&marker, true, 0), None);
        assert_eq!(s.first_unstable_offset(), None);
        // After a marker the epoch's sequence history is kept; the next batch continues it.
        assert_eq!(s.check(&header(1, 0, 3, 1, 16, TRANSACTIONAL)), SeqCheck::Append);
    }

    #[test]
    fn expiry_keeps_open_transactions() {
        let mut s = ProducerStates::default();
        s.update(&header(1, 0, 0, 1, 0, 0));
        s.update(&header(2, 0, 0, 1, 1, TRANSACTIONAL));
        assert_eq!(s.expire(i64::MAX), 1);
        assert!(s.get(2).is_some());
    }
}
