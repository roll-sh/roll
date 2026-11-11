//! Ranges a scan found holding no live row: a queue's head after its rows were deleted, an
//! outbox's processed prefix. Every scan of such a range would step over each deleted key's
//! tombstone again (PostgreSQL's index scans mark such entries LP_DEAD and skip them after).
//! A scan that stepped over many before its first live key records a hint, "[start, live)
//! holds nothing visible at ts", and a later scan at a snapshot at or after `ts` starting in
//! the range starts at `live` instead.
//!
//! A hint must stay true for every snapshot after `ts`. Each commit's span of keys is logged
//! here before its writes go in (`note`, under the store's commit lock; an ingested file's
//! span is every key), and drops the hints it writes into; a hint is recorded only when no
//! commit since the scan's snapshot wrote into its range (the log covers them). Both happen
//! under this structure's lock, so a commit either sees the hint or is seen by it. (Flushes
//! and compactions change no row's visibility.)
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

/// Key bytes a commit's span keeps of its first and last keys.
const CUT: usize = 32;
/// Commits whose spans are kept (scans recording a hint look back this far at most).
const SPANS: usize = 4096;
/// Hints kept (the oldest goes first).
const HINTS: usize = 64;
/// Deleted keys a scan steps over before its first live key for a hint to be worth it.
pub(super) const MIN_DEAD: u32 = 256;

/// A key's first `CUT` bytes.
#[derive(Clone, Copy)]
struct Cut {
    b: [u8; CUT],
    n: u8,
    /// The key was longer.
    cut: bool,
}

impl Cut {
    fn of(k: &[u8]) -> Cut {
        let n = k.len().min(CUT);
        let mut b = [0; CUT];
        b[..n].copy_from_slice(&k[..n]);
        Cut { b, n: n as u8, cut: k.len() > CUT }
    }

    /// Beyond every key.
    const TOP: Cut = Cut { b: [0xFF; CUT], n: CUT as u8, cut: true };

    fn bytes(&self) -> &[u8] {
        &self.b[..self.n as usize]
    }

    /// Whether the key it was cut from may be below `x`.
    fn may_lt(&self, x: &[u8]) -> bool {
        self.bytes() < x
    }

    /// Whether the key it was cut from may be at or above `x`.
    fn may_ge(&self, x: &[u8]) -> bool {
        let p = self.bytes();
        if self.cut { !(p < x && !x.starts_with(p)) } else { p >= x }
    }
}

struct Hint {
    start: Vec<u8>,
    live: Vec<u8>,
    ts: u64,
}

#[derive(Default)]
struct Inner {
    /// The last commits' (ts, first key, last key), oldest first.
    spans: VecDeque<(u64, Cut, Cut)>,
    /// The newest commit whose span left the log.
    dropped: u64,
    hints: Vec<Hint>,
}

#[derive(Default)]
pub(super) struct DeadRanges {
    any: AtomicBool,
    inner: Mutex<Inner>,
}

impl DeadRanges {
    /// A commit at `ts` writing `keys` (all keys: `None`, an ingested file), before its
    /// writes go in, under the commit lock.
    pub(super) fn note(&self, ts: u64, keys: Option<&mut dyn Iterator<Item = &[u8]>>) {
        let (mut lo, mut hi): (Option<&[u8]>, Option<&[u8]>) = (None, None);
        let all = keys.is_none();
        let mut written: Vec<&[u8]> = Vec::new();
        let any = self.any.load(Ordering::Acquire);
        if let Some(keys) = keys {
            for k in keys {
                lo = Some(lo.map_or(k, |l| l.min(k)));
                hi = Some(hi.map_or(k, |h| h.max(k)));
                if any {
                    written.push(k);
                }
            }
        }
        let span = match (lo, hi) {
            _ if all => (Cut::of(&[]), Cut::TOP),
            (Some(l), Some(h)) => (Cut::of(l), Cut::of(h)),
            _ => return,
        };
        let mut g = self.inner.lock();
        if g.spans.len() == SPANS
            && let Some((t, ..)) = g.spans.pop_front()
        {
            g.dropped = t;
        }
        g.spans.push_back((ts, span.0, span.1));
        if any {
            g.hints.retain(|h| !all && !written.iter().any(|k| *k >= h.start.as_slice() && *k < h.live.as_slice()));
            self.any.store(!g.hints.is_empty(), Ordering::Release);
        }
    }

    /// Where a scan at snapshot `ts` starting at `start` may start instead (None: there).
    pub(super) fn skip(&self, start: &[u8], ts: u64) -> Option<Vec<u8>> {
        if !self.any.load(Ordering::Acquire) {
            return None;
        }
        let g = self.inner.lock();
        let mut at: Option<Vec<u8>> = None;
        // (Hints chain: each one a scan recorded from where the last one ended.)
        for _ in 0..HINTS {
            let cur = at.as_deref().unwrap_or(start);
            let Some(h) = g.hints.iter().find(|h| h.ts <= ts && h.start.as_slice() <= cur && cur < h.live.as_slice()) else {
                break;
            };
            at = Some(h.live.clone());
        }
        at
    }

    /// A scan at snapshot `ts` found nothing visible in [start, live): kept as a hint unless
    /// a commit since `ts` wrote into the range (or the log doesn't reach back that far).
    pub(super) fn record(&self, start: &[u8], live: &[u8], ts: u64) {
        if start >= live {
            return;
        }
        let mut g = self.inner.lock();
        if g.dropped > ts || g.spans.iter().rev().take_while(|s| s.0 > ts).any(|(_, lo, hi)| lo.may_lt(live) && hi.may_ge(start)) {
            return;
        }
        if g.hints.len() == HINTS {
            g.hints.remove(0);
        }
        g.hints.push(Hint { start: start.to_vec(), live: live.to_vec(), ts });
        self.any.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_hold_until_a_commit_writes_into_them() {
        let d = DeadRanges::default();
        d.note(5, Some(&mut [&b"z"[..]].into_iter()));
        // Recorded at snapshot 5: nothing since wrote into [b, m).
        d.record(b"b", b"m", 5);
        assert_eq!(d.skip(b"c", 5).as_deref(), Some(&b"m"[..]));
        assert_eq!(d.skip(b"c", 4), None, "an older snapshot");
        assert_eq!(d.skip(b"m", 9), None, "past the range");
        // A commit outside the range keeps it; one inside drops it.
        d.note(6, Some(&mut [&b"q"[..], &b"a"[..]].into_iter()));
        assert!(d.skip(b"b", 6).is_some());
        d.note(7, Some(&mut [&b"f"[..]].into_iter()));
        assert_eq!(d.skip(b"b", 7), None);
        // A scan at snapshot 6 can't record [b, m): commit 7 wrote into it since.
        d.record(b"b", b"m", 6);
        assert_eq!(d.skip(b"b", 7), None);
        d.record(b"b", b"m", 7);
        d.record(b"m", b"p", 7);
        assert_eq!(d.skip(b"b", 8).as_deref(), Some(&b"p"[..]), "hints chain");
        // An ingested file drops them all.
        d.note(8, None);
        assert_eq!(d.skip(b"b", 9), None);
        d.record(b"b", b"m", 7);
        assert_eq!(d.skip(b"b", 9), None, "the ingest is after the scan's snapshot");
    }

    #[test]
    fn cut_keys_compare_conservatively() {
        let long = [7u8; 40];
        let c = Cut::of(&long);
        assert!(c.may_ge(&[7u8; 39]) && c.may_ge(&[7u8; 41]) && !c.may_ge(&[8u8]));
        assert!(c.may_lt(&[7u8; 41]) && !c.may_lt(&[7u8; 32]));
    }
}
