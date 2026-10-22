//! Counter cell values: Cassandra's counter contexts (`org.apache.cassandra.db.context.
//! CounterContext`), and their conversion to roll's counter model.
//!
//! ```text
//! be16 n  (be16 index)*n  (counter-id[16] be64 clock be64 count)*
//! ```
//!
//! The header lists the global shards (index + `Short.MIN_VALUE`) and legacy local shards
//! (index); the rest are legacy remote shards. A counter's value is the sum of its shards'
//! counts; replicas' copies of a cell merge per counter id (`CounterContext.merge`: a global
//! shard beats others and the higher clock wins, two local shards add up).
//!
//! roll has one writer per database, so a counter is one `(clock, total)` pair: the merged
//! context's total, with the sum of its clocks (which grows with every update, so the newest
//! state wins any later reconciliation).

use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShardKind {
    Global,
    Local,
    Remote,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shard {
    pub id: [u8; 16],
    pub clock: i64,
    pub count: i64,
    pub kind: ShardKind,
}

/// A parsed context, shards in counter id order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Context {
    pub shards: Vec<Shard>,
}

const STEP: usize = 32;

impl Context {
    pub fn parse(v: &[u8]) -> Result<Context> {
        if v.len() < 2 {
            bail!("corrupt counter context: {} bytes", v.len());
        }
        let n = i16::from_be_bytes([v[0], v[1]]).unsigned_abs() as usize;
        let header = 2 + 2 * n;
        if v.len() < header || !(v.len() - header).is_multiple_of(STEP) {
            bail!("corrupt counter context: {} bytes with {n} header entries", v.len());
        }
        let count = (v.len() - header) / STEP;
        if n > count {
            bail!("corrupt counter context: {n} header entries for {count} shards");
        }
        let mut shards = Vec::with_capacity(count);
        let mut h = 0;
        for i in 0..count {
            let kind = if h < n {
                let e = i16::from_be_bytes([v[2 + 2 * h], v[3 + 2 * h]]) as i32;
                if e == i as i32 + i16::MIN as i32 {
                    h += 1;
                    ShardKind::Global
                } else if e == i as i32 {
                    h += 1;
                    ShardKind::Local
                } else {
                    ShardKind::Remote
                }
            } else {
                ShardKind::Remote
            };
            let at = header + i * STEP;
            let id: [u8; 16] = v[at..at + 16].try_into().expect("16");
            let clock = i64::from_be_bytes(v[at + 16..at + 24].try_into().expect("8"));
            let count = i64::from_be_bytes(v[at + 24..at + 32].try_into().expect("8"));
            shards.push(Shard { id, clock, count, kind });
        }
        if !shards.windows(2).all(|w| w[0].id < w[1].id) {
            bail!("corrupt counter context: shards out of order");
        }
        Ok(Context { shards })
    }

    /// `CounterContext.total`.
    pub fn total(&self) -> i64 {
        self.shards.iter().fold(0i64, |t, s| t.wrapping_add(s.count))
    }

    /// The clock roll stores: the sum of the shards' clocks (at least 1).
    pub fn clock(&self) -> i64 {
        self.shards.iter().fold(0i64, |t, s| t.saturating_add(s.clock.max(0))).max(1)
    }

    /// `CounterContext.merge`.
    pub fn merge(&self, other: &Context) -> Context {
        let (a, b) = (&self.shards, &other.shards);
        let (mut i, mut j) = (0, 0);
        let mut out = Vec::with_capacity(a.len() + b.len());
        while i < a.len() && j < b.len() {
            match a[i].id.cmp(&b[j].id) {
                std::cmp::Ordering::Less => {
                    out.push(a[i]);
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    out.push(b[j]);
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    out.push(merge_shard(&a[i], &b[j]));
                    i += 1;
                    j += 1;
                }
            }
        }
        out.extend_from_slice(&a[i..]);
        out.extend_from_slice(&b[j..]);
        Context { shards: out }
    }
}

/// `CounterContext.compare` for one counter id, and the merged shard.
fn merge_shard(l: &Shard, r: &Shard) -> Shard {
    use ShardKind::*;
    let higher = |l: &Shard, r: &Shard| if l.clock != r.clock { if l.clock > r.clock { *l } else { *r } } else if l.count > r.count { *l } else { *r };
    match (l.kind, r.kind) {
        (Global, Global) => higher(l, r),
        (Global, _) => *l,
        (_, Global) => *r,
        (Local, Local) => Shard { id: l.id, clock: l.clock.wrapping_add(r.clock), count: l.count.wrapping_add(r.count), kind: Local },
        (Local, _) => *l,
        (_, Local) => *r,
        _ => {
            if l.clock == r.clock {
                return if l.count > r.count { *l } else { *r };
            }
            // Remote shards: a negative clock is a deleted counter (pre-2.1 semantics).
            let left_wins = (l.clock >= 0 && r.clock > 0 && l.clock >= r.clock) || (l.clock < 0 && (r.clock > 0 || l.clock < r.clock));
            if left_wins { *l } else { *r }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn encode(shards: &[Shard]) -> Vec<u8> {
        let header: Vec<i16> = shards
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match s.kind {
                ShardKind::Global => Some(i as i16 + i16::MIN),
                ShardKind::Local => Some(i as i16),
                ShardKind::Remote => None,
            })
            .collect();
        let mut v = (header.len() as i16).to_be_bytes().to_vec();
        for h in header {
            v.extend_from_slice(&h.to_be_bytes());
        }
        for s in shards {
            v.extend_from_slice(&s.id);
            v.extend_from_slice(&s.clock.to_be_bytes());
            v.extend_from_slice(&s.count.to_be_bytes());
        }
        v
    }

    pub fn g(id: u8, clock: i64, count: i64) -> Shard {
        Shard { id: [id; 16], clock, count, kind: ShardKind::Global }
    }

    #[test]
    fn parse_total_and_merge() {
        let a = Context::parse(&encode(&[g(1, 5, 10), g(2, 3, 4)])).unwrap();
        assert_eq!((a.total(), a.clock()), (14, 8));
        let b = Context::parse(&encode(&[g(2, 4, 6)])).unwrap();
        // Per counter id the higher clock wins: 10 + 6.
        assert_eq!(a.merge(&b).total(), 16);
        assert_eq!(b.merge(&a).total(), 16);
        let l1 = Shard { kind: ShardKind::Local, ..g(3, 1, 2) };
        let l2 = Shard { kind: ShardKind::Local, ..g(3, 2, 5) };
        let m = Context { shards: vec![l1] }.merge(&Context { shards: vec![l2] });
        assert_eq!(m.total(), 7, "local shards add up");
        let r = Shard { kind: ShardKind::Remote, ..g(3, 9, 100) };
        assert_eq!(Context { shards: vec![r] }.merge(&Context { shards: vec![g(3, 1, 1)] }).total(), 1, "global beats remote");
        let parsed = Context::parse(&encode(&[l1, r])).unwrap_err();
        assert!(parsed.to_string().contains("order"), "same id twice");
    }

    #[test]
    fn corrupt_contexts() {
        assert!(Context::parse(&[]).is_err());
        assert!(Context::parse(&[0, 1, 0, 0]).is_err());
        let mut v = encode(&[g(1, 1, 1)]);
        v.pop();
        assert!(Context::parse(&v).is_err());
        let mut v = encode(&[g(1, 1, 1)]);
        v[1] = 5;
        assert!(Context::parse(&v).is_err());
        assert_eq!(Context::parse(&[0, 0]).unwrap().total(), 0);
    }
}
