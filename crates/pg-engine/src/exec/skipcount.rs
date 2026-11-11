//! EXPLAIN ANALYZE's "Index Searches" of a btree skip scan (`IndexBounds::skip`): the
//! `_bt_first` calls PostgreSQL 18 makes, replayed over the index's size model
//! (`btsim::Btree`). The engine reads a skip scan its own way (`scan::Skip`); only the count
//! follows PostgreSQL's.
//!
//! A primitive scan lands where `_bt_first` would and reads leaf entries in order. An entry
//! past the scan's keys advances them to it (`_bt_advance_array_keys`): the skip array
//! takes its leading value, and the bounds on the later columns move to their next range,
//! or past their last one to the next leading value (one more with skip support, else
//! anything above it). An entry then before the keys ends the primitive scan when the
//! page's high key is before them too (a truncated attribute counts as satisfied), unless
//! the scan is past its first page or has advanced the skip array more than
//! NSKIPADVANCES_THRESHOLD times on it: then it goes on to the next page, and starts a new
//! primitive scan there only when that page's high key is still before the keys
//! (`_bt_scanbehind_checkkeys`).

use super::ExecCtx;
use crate::catalog::btsim::Btree;
use crate::catalog::{Index, Table};
use crate::sql::plan::IndexBounds;
use crate::storage::keys::{decode_index_value, encode_index_value, index_prefix, prefix_end};
use crate::types::{DataType, Value};

/// nbtutils.c's NSKIPADVANCES_THRESHOLD.
const NSKIPADVANCES_THRESHOLD: u32 = 3;

/// What a skip scan's keys are over (all keys without the index's storage prefix).
struct SkipModel<'a> {
    /// The skip scan's range on the leading columns.
    main: (&'a [u8], Option<&'a [u8]>),
    /// The length of an entry's leading (skipped) columns' keys (None: not taken apart).
    prefix_len: &'a dyn Fn(&[u8]) -> Option<usize>,
    /// The ranges the later columns' bounds select under a leading value's key.
    targets: &'a mut dyn FnMut(&[u8]) -> Vec<(Vec<u8>, Vec<u8>)>,
    /// The next leading value's key after one (skip support), None without it.
    succ: &'a dyn Fn(&[u8]) -> Option<Vec<u8>>,
    /// The first primitive scan starts at the leading columns' first value with the later
    /// columns' bounds (skip support and a lower bound), else at the range's start.
    seek_first: bool,
}

/// The scan's array keys: the skip array's value and the later bounds' current range.
struct Keys {
    /// None: not set yet (the skip array's -inf).
    v: Option<Vec<u8>>,
    /// The skip array's value is "anything above v" (no skip support).
    next: bool,
    tgt: Vec<(Vec<u8>, Vec<u8>)>,
    ti: usize,
}

impl Keys {
    /// Where entries satisfying the keys begin.
    fn pos(&self) -> Vec<u8> {
        match &self.v {
            None => Vec::new(),
            Some(v) if self.next => prefix_end(v),
            Some(v) => self.tgt.get(self.ti).map_or_else(|| prefix_end(v), |r| r.0.clone()),
        }
    }

    fn matches(&self, k: &[u8]) -> bool {
        !self.next
            && self.v.as_ref().is_some_and(|v| k.starts_with(v))
            && self.tgt.get(self.ti).is_some_and(|(a, b)| k >= a.as_slice() && k < b.as_slice())
    }
}

/// Whether high key `h` (`truncated`: attributes cut off it) is before key position `pos`:
/// compared on the attributes it keeps, a truncated one satisfied.
fn high_before(h: &[u8], truncated: bool, pos: &[u8]) -> bool {
    if truncated { h < &pos[..h.len().min(pos.len())] } else { h < pos }
}

/// The number of `_bt_first` calls a forward skip scan of `bt` makes.
fn searches(bt: &Btree, m: &mut SkipModel) -> u64 {
    let (start, end) = m.main;
    let mut keys = Keys { v: None, next: false, tgt: Vec::new(), ti: 0 };
    let mut land = start.to_vec();
    if m.seek_first
        && let Some(first) = bt.scan_leaf(start).and_then(|b| bt.leaf_view(b)).and_then(|l| (0..l.len()).map(|i| l.key(i)).find(|k| *k >= start).map(<[u8]>::to_vec))
        && let Some(n) = (m.prefix_len)(&first)
    {
        let v = first[..n].to_vec();
        keys.tgt = (m.targets)(&v);
        keys.v = Some(v);
        land = keys.pos();
    }
    let mut searches = 1;
    let mut blk = bt.scan_leaf(&land);
    let mut first_page = true;
    let mut behind = false;
    // (Where entries satisfying the keys begin, kept as the keys change.)
    let mut pos = keys.pos();
    // (Each leaf is read at most once per primitive scan, and primitive scans only move
    // forward: bounded by the model's size.)
    'pages: while let Some(b) = blk {
        let Some(page) = bt.leaf_view(b) else { break };
        if behind && !first_page {
            behind = false;
            if let Some(h) = page.high
                && high_before(h, page.truncated, &pos)
            {
                // (_bt_scanbehind_checkkeys: a new primitive scan after all.)
                land = pos.clone();
                (searches, blk, first_page) = (searches + 1, bt.scan_leaf(&land), true);
                continue;
            }
        }
        let mut advances = 0;
        for i in 0..page.len() {
            let k = page.key(i);
            if k < land.as_slice() {
                continue;
            }
            if end.is_some_and(|e| k >= e) {
                break 'pages;
            }
            if k < pos.as_slice() || keys.matches(k) {
                continue;
            }
            // Past the keys: they advance to this entry (one call, one skip advance).
            let Some(n) = (m.prefix_len)(k) else { break 'pages };
            let p = &k[..n];
            let moved = keys.next || keys.v.as_deref() != Some(p);
            if moved {
                keys.v = Some(p.to_vec());
                keys.next = false;
                keys.tgt = (m.targets)(p);
                keys.ti = 0;
            }
            while keys.tgt.get(keys.ti).is_some_and(|r| k >= r.1.as_slice()) {
                keys.ti += 1;
            }
            if keys.ti >= keys.tgt.len() {
                // Past the leading value's last range: on to the next value.
                let v = keys.v.clone().expect("a skip array value");
                match (m.succ)(&v) {
                    Some(s) => {
                        keys.tgt = (m.targets)(&s);
                        keys.v = Some(s);
                        keys.ti = 0;
                    }
                    None => keys.next = true,
                }
                advances += 1;
            } else if moved {
                advances += 1;
            }
            pos = keys.pos();
            if keys.matches(k) {
                continue;
            }
            if end.is_some_and(|e| pos.as_slice() >= e) {
                break 'pages;
            }
            let Some(h) = page.high else { continue };
            if high_before(h, page.truncated, &pos) {
                if !first_page || advances > NSKIPADVANCES_THRESHOLD {
                    behind = true;
                    break;
                }
                searches += 1;
                land = pos.clone();
                blk = bt.scan_leaf(&land);
                first_page = true;
                continue 'pages;
            }
            if page.truncated {
                behind = true;
            }
        }
        blk = page.right;
        first_page = false;
    }
    searches
}

/// EXPLAIN ANALYZE's "Index Searches" of an index scan over `ranges` of btree `ix` with
/// bounds `b`: for a skip scan of an index the size model holds, the `_bt_first` calls
/// PostgreSQL makes (`searches`; a backward scan is counted as the forward one), else one
/// per range.
pub fn count(t: &Table, ix: &Index, b: &IndexBounds, ranges: &[(Vec<u8>, Vec<u8>)], ctx: &ExecCtx) -> usize {
    let n = ranges.len();
    let Some(skip) = b.skip.as_deref() else { return n };
    if !ctx.instrumented() || ix.method != "btree" {
        return n;
    }
    let pre = index_prefix(t.oid, ix.id);
    let k = skip.cols.len();
    let cols: Vec<(DataType, bool, bool)> =
        (0..k).filter_map(|i| ix.columns.get(i).map(|c| (skip.cols[i].clone(), c.desc, c.nulls_first))).collect();
    if cols.len() != k {
        return n;
    }
    let prefix_len = |key: &[u8]| -> Option<usize> {
        let mut pos = 0;
        for (ty, desc, _) in &cols {
            decode_index_value(key, &mut pos, ty, *desc)?;
        }
        Some(pos)
    };
    let mut failed = false;
    let mut targets = |p: &[u8]| -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut full = pre.clone();
        full.extend_from_slice(p);
        match super::scan::ranges_from(ix, &skip.bounds, full, k, ctx) {
            Ok(rs) => rs.into_iter().map(|(a, z)| (a[pre.len()..].to_vec(), z[pre.len()..].to_vec())).collect(),
            Err(_) => {
                failed = true;
                Vec::new()
            }
        }
    };
    // (PostgreSQL 18's skip support: the next value of one integer-like leading column.)
    let succ = |p: &[u8]| -> Option<Vec<u8>> {
        let [(ty, desc, nulls_first)] = &cols[..] else { return None };
        let mut pos = 0;
        let v = decode_index_value(p, &mut pos, ty, *desc)?;
        let step: i64 = if *desc { -1 } else { 1 };
        let next = match v {
            Value::Int2(x) => Value::Int2(x.checked_add(step as i16)?),
            Value::Int4(x) => Value::Int4(x.checked_add(step as i32)?),
            Value::Int8(x) => Value::Int8(x.checked_add(step)?),
            Value::Date(x) => Value::Date(x.checked_add(step as i32)?),
            Value::Oid(x) => Value::Oid(x.checked_add_signed(step as i32)?),
            Value::Bool(x) if x == *desc => Value::Bool(!x),
            _ => return None,
        };
        let mut out = Vec::new();
        encode_index_value(&mut out, &next, *desc, *nulls_first, None);
        Some(out)
    };
    let supported = matches!(
        &cols[..],
        [(DataType::Int2 | DataType::Int4 | DataType::Int8 | DataType::Date | DataType::Oid | DataType::Bool, ..)]
    );
    let seek_first = supported && b.lower.is_some();
    let mut total = 0u64;
    for (a, z) in ranges {
        // (A range to the index's end ends past its prefix: unbounded here.)
        let Some(a) = a.strip_prefix(pre.as_slice()) else { return n };
        let z = z.strip_prefix(pre.as_slice());
        let mut m = SkipModel { main: (a, z), prefix_len: &prefix_len, targets: &mut targets, succ: &succ, seek_first };
        match super::sizing::with_btree(t.oid, ix.oid, &ctx.sess.db, |bt| searches(bt, &mut m)) {
            Some(c) => total += c,
            None => return n,
        }
    }
    if failed { n } else { total as usize }
}
