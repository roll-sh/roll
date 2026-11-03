//! KNN index scans read nearest-first (`search::Near`, round 22 agent 139): the candidates
//! come in ascending order of a lower bound on their distance; each is read and its
//! distance computed by the ORDER BY operator itself, and a row is returned once no
//! candidate left can come before it (IndexNextWithReorder's queue: every bound here is
//! inexact, as circle and polygon distances are in PostgreSQL). Rows at equal distances
//! come in the order they were read (data key order within a batch): PostgreSQL's order
//! among them follows its trees' shape.

use super::ExecCtx;
use crate::catalog::Table;
use crate::error::Result;
use crate::sql::expr::Expr;
use crate::types::Value;
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashSet};
use std::sync::Arc;

/// A distance as the ORDER BY sorts it: numbers ascending (float NaN after every other),
/// null last.
enum Dist {
    I(i64),
    F(f64),
    Null,
}

impl Dist {
    fn of(v: &Value) -> Option<Dist> {
        Some(match v {
            Value::Int2(x) => Dist::I(*x as i64),
            Value::Int4(x) => Dist::I(*x as i64),
            Value::Int8(x) => Dist::I(*x),
            Value::Float4(x) => Dist::F(*x as f64),
            Value::Float8(x) => Dist::F(*x),
            // (btree_gist's timestamp and timestamptz distances: intervals, ordered by their
            // span in microseconds as interval_cmp orders them, the grid's unit for those keys.)
            Value::Interval(i) if i.is_infinite() => Dist::F(if i.is_pos_inf() { f64::INFINITY } else { f64::NEG_INFINITY }),
            Value::Interval(i) => match i64::try_from(i.cmp_key()) {
                Ok(x) => Dist::I(x),
                Err(_) => Dist::F(i.cmp_key() as f64),
            },
            Value::Null => Dist::Null,
            _ => return None,
        })
    }

    /// Whether it is at most a bound: nothing with a distance of at least the bound comes
    /// before it. A NaN or a null only once what is left is null (bound infinity: the null
    /// keys'). (Rounding to f64 is monotone; past 2^53 an integer equal as f64 may be more.)
    fn within(&self, b: f64) -> bool {
        match self {
            Dist::I(i) if i.unsigned_abs() < 1 << 53 => (*i as f64) <= b,
            Dist::I(i) => (*i as f64) < b,
            Dist::F(f) if f.is_nan() => b == f64::INFINITY,
            Dist::F(f) => *f <= b,
            Dist::Null => b == f64::INFINITY,
        }
    }

    fn order(&self, o: &Dist) -> Ordering {
        let f = |a: f64, b: f64| match (a.is_nan(), b.is_nan()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            _ => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
        };
        match (self, o) {
            (Dist::Null, Dist::Null) => Ordering::Equal,
            (Dist::Null, _) => Ordering::Greater,
            (_, Dist::Null) => Ordering::Less,
            (Dist::I(a), Dist::I(b)) => a.cmp(b),
            (Dist::I(a), Dist::F(b)) => f(*a as f64, *b),
            (Dist::F(a), Dist::I(b)) => f(*a, *b as f64),
            (Dist::F(a), Dist::F(b)) => f(*a, *b),
        }
    }
}

/// A row read, by its distance (the queue's least first).
struct Ready {
    d: Dist,
    key: Vec<u8>,
    row: bytes::Bytes,
}

impl Ord for Ready {
    fn cmp(&self, o: &Ready) -> Ordering {
        o.d.order(&self.d).then_with(|| o.key.cmp(&self.key))
    }
}
impl PartialOrd for Ready {
    fn partial_cmp(&self, o: &Ready) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl PartialEq for Ready {
    fn eq(&self, o: &Ready) -> bool {
        self.cmp(o) == Ordering::Equal
    }
}
impl Eq for Ready {}

/// A KNN scan's rows (data key, stored row) in the order of its ORDER BY distance.
pub struct NearestRows {
    near: super::search::Near,
    table: Arc<Table>,
    /// The distance, over the table's row, and the columns it reads.
    dist: Expr,
    need: Option<Vec<bool>>,
    /// Rows read already (a row may have entries for several versions), and those read
    /// and not returned yet.
    seen: HashSet<Vec<u8>>,
    ready: BinaryHeap<Ready>,
    batch: usize,
}

impl NearestRows {
    pub fn new(near: super::search::Near, table: Arc<Table>, dist: Expr) -> NearestRows {
        let need = super::need::of([&dist]);
        NearestRows { near, table, dist, need, seen: HashSet::new(), ready: BinaryHeap::new(), batch: 4 }
    }

    pub fn next(&mut self, ctx: &ExecCtx) -> Result<Option<(Vec<u8>, bytes::Bytes)>> {
        loop {
            let bound = self.near.bound();
            if let Some(top) = self.ready.peek()
                && bound.is_none_or(|b| top.d.within(b))
            {
                let r = self.ready.pop().expect("peeked");
                return Ok(Some((r.key, r.row)));
            }
            if bound.is_none() {
                return Ok(None);
            }
            // (Candidates that may come before the nearest row read so far, a few at a time.)
            let mut keys = Vec::new();
            while keys.len() < self.batch {
                if let (Some(top), Some(b)) = (self.ready.peek(), self.near.bound())
                    && top.d.within(b)
                {
                    break;
                }
                let Some(k) = self.near.next_key() else { break };
                if self.seen.insert(k.clone()) {
                    keys.push(k);
                }
            }
            self.batch = (self.batch * 2).min(64);
            let rows = super::scan::fetch_rows(&keys, ctx)?;
            for (key, row) in keys.into_iter().zip(rows) {
                let Some(row) = row else { continue };
                let vals = match &self.need {
                    Some(n) => super::scan::decode_table_row_need(&self.table, &row, n)?,
                    None => super::scan::decode_table_row(&self.table, &row)?,
                };
                let v = crate::sql::expr::eval(&self.dist, &vals, ctx)?;
                let Some(d) = Dist::of(&v) else {
                    return Err(crate::error::internal("a KNN distance that isn't a number"));
                };
                self.ready.push(Ready { d, key, row });
            }
        }
    }
}
