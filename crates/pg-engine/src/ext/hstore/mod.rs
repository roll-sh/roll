//! hstore (`contrib/hstore`): sets of (key, value) text pairs. A value's bytes are
//! PostgreSQL's own varlena data (hstore.h, after the varlena header): a `size_` word (count
//! with the new-version flag), two HEntry words per pair (end positions, the first marked,
//! a null value flagged), then the keys and values; pairs sorted by key length, then bytes,
//! keys unique. So hstore_cmp, hstore_hash and pg_column_size work on the same bytes as
//! PostgreSQL's.

mod build;
mod convert;
mod index;
mod io;
mod ops;
#[cfg(test)]
mod tests;

use super::native::Native;
use super::types::{ExtType, HSTORE};
use std::borrow::Cow;
use std::cmp::Ordering;

const ISFIRST: u32 = 0x8000_0000;
const ISNULL: u32 = 0x4000_0000;
const POSMASK: u32 = 0x3FFF_FFFF;
const NEWVERSION: u32 = 0x8000_0000;
/// HSTORE_MAX_KEY_LEN / HSTORE_MAX_VALUE_LEN.
const MAX_LEN: usize = 0x3FFF_FFFF;

pub(super) const TYPE: ExtType = ExtType {
    kind: HSTORE,
    extension: "hstore",
    name: "hstore",
    input: io::input,
    output: |b, _| io::output(b),
    cmp: hstore_cmp,
    canon: |b| Cow::Borrowed(b),
    key: sort_key,
    to_builtin: None,
    from_builtin: None,
    hash: true,
};

/// A read-only view of an hstore value.
#[derive(Clone, Copy)]
pub(crate) struct Hs<'a> {
    b: &'a [u8],
}

fn word(b: &[u8], i: usize) -> u32 {
    b.get(i * 4..i * 4 + 4)
        .map_or(0, |w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

impl<'a> Hs<'a> {
    pub fn new(b: &'a [u8]) -> Hs<'a> {
        Hs { b }
    }

    pub fn count(&self) -> usize {
        (word(self.b, 0) & 0x0FFF_FFFF) as usize
    }

    fn entry(&self, i: usize) -> u32 {
        word(self.b, 1 + i)
    }

    fn strs(&self) -> &'a [u8] {
        let start = 4 + self.count() * 8;
        self.b.get(start..).unwrap_or(&[])
    }

    fn span(&self, e: usize) -> (usize, usize) {
        let end = (self.entry(e) & POSMASK) as usize;
        let start = if e == 0 { 0 } else { (self.entry(e - 1) & POSMASK) as usize };
        (start, end)
    }

    pub fn key(&self, i: usize) -> &'a [u8] {
        let (s, e) = self.span(2 * i);
        &self.strs()[s..e]
    }

    /// The value of pair `i` (None: NULL).
    pub fn val(&self, i: usize) -> Option<&'a [u8]> {
        if self.entry(2 * i + 1) & ISNULL != 0 {
            return None;
        }
        let (s, e) = self.span(2 * i + 1);
        Some(&self.strs()[s..e])
    }

    /// The length of the strings area (HSE_ENDPOS of the last entry).
    fn strlen(&self) -> usize {
        match self.count() {
            0 => 0,
            n => (self.entry(2 * n - 1) & POSMASK) as usize,
        }
    }

    pub fn pairs(&self) -> impl Iterator<Item = (&'a [u8], Option<&'a [u8]>)> + '_ {
        (0..self.count()).map(|i| (self.key(i), self.val(i)))
    }

    /// `hstoreFindKey`: the index of `key` at or after `*low` (moved past it), or None.
    pub fn find(&self, low: Option<&mut usize>, key: &[u8]) -> Option<usize> {
        let mut lo = low.as_ref().map_or(0, |l| **l);
        let mut hi = self.count();
        let mut found = None;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            let k = self.key(mid);
            match key_order(k, key) {
                Ordering::Equal => {
                    found = Some(mid);
                    break;
                }
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
            }
        }
        if let Some(l) = low {
            *l = match found {
                Some(m) => m + 1,
                None => lo,
            };
        }
        found
    }
}

/// Pairs' key order: shorter first, then bytes.
pub(crate) fn key_order(a: &[u8], b: &[u8]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// `hstorePairs` over pairs already sorted and unique.
pub(crate) fn encode<'p>(pairs: impl IntoIterator<Item = (&'p [u8], Option<&'p [u8]>)>) -> Vec<u8> {
    let pairs: Vec<(&[u8], Option<&[u8]>)> = pairs.into_iter().collect();
    let n = pairs.len();
    let mut out = Vec::with_capacity(4 + n * 8);
    out.extend_from_slice(&((n as u32) | NEWVERSION).to_le_bytes());
    let mut strs = Vec::new();
    for (i, (k, v)) in pairs.iter().enumerate() {
        strs.extend_from_slice(k);
        let mut e = (strs.len() as u32) & POSMASK;
        if i == 0 {
            e |= ISFIRST;
        }
        out.extend_from_slice(&e.to_le_bytes());
        let e = match v {
            Some(v) => {
                strs.extend_from_slice(v);
                (strs.len() as u32) & POSMASK
            }
            None => ((strs.len() as u32) & POSMASK) | ISNULL,
        };
        out.extend_from_slice(&e.to_le_bytes());
    }
    out.extend_from_slice(&strs);
    out
}

/// A pair being collected (`Pairs`): `needfree` only orders duplicates as PostgreSQL's
/// qsort of them does.
#[derive(Clone, Debug)]
pub(crate) struct Pair {
    pub key: Vec<u8>,
    pub val: Option<Vec<u8>>,
    pub needfree: bool,
}

/// `hstoreUniquePairs` then `hstorePairs`: sorted (PostgreSQL's qsort, comparePairs), the
/// first of equal keys kept.
pub(crate) fn from_pairs(mut pairs: Vec<Pair>) -> Vec<u8> {
    if pairs.len() > 1 {
        crate::sort::pg_qsort(&mut pairs, |a, b| {
            key_order(&a.key, &b.key).then_with(|| a.needfree.cmp(&b.needfree))
        });
        pairs.dedup_by(|b, a| a.key == b.key);
    }
    encode(pairs.iter().map(|p| (p.key.as_slice(), p.val.as_deref())))
}

/// `hstore_cmp`.
pub(crate) fn hstore_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (x, y) = (Hs::new(a), Hs::new(b));
    let (c1, c2) = (x.count(), y.count());
    if c1 == 0 || c2 == 0 {
        // (The nonempty one is larger.)
        return (c1 > 0).cmp(&(c2 > 0));
    }
    let (l1, l2) = (x.strlen(), y.strlen());
    let n = l1.min(l2);
    let (s1, s2) = (x.strs(), y.strs());
    s1[..n]
        .cmp(&s2[..n])
        .then(l1.cmp(&l2))
        .then(c1.cmp(&c2))
        .then_with(|| {
            for i in 0..c1 * 2 {
                let (e1, e2) = (x.entry(i), y.entry(i));
                let (p1, p2) = (e1 & POSMASK, e2 & POSMASK);
                let (n1, n2) = (e1 & ISNULL != 0, e2 & ISNULL != 0);
                if p1 != p2 || n1 != n2 {
                    return p1.cmp(&p2).then(n1.cmp(&n2));
                }
            }
            Ordering::Equal
        })
}

/// An order-preserving key for `hstore_cmp`: empty first, then the strings (as memcmp then
/// length), the count, and each entry's position and null flag.
fn sort_key(a: &[u8], out: &mut Vec<u8>) {
    let h = Hs::new(a);
    if h.count() == 0 {
        out.push(0);
        return;
    }
    out.push(1);
    super::types::key_bytes(&h.strs()[..h.strlen()], out);
    out.extend_from_slice(&(h.count() as u32).to_be_bytes());
    for i in 0..h.count() * 2 {
        let e = h.entry(i);
        out.extend_from_slice(&(e & POSMASK).to_be_bytes());
        out.push((e & ISNULL != 0) as u8);
    }
}

/// `hstore_subscript_handler`'s routines (hstore_subs.c): `h[key]` fetches the key's value;
/// assigning sets it (a NULL hstore becoming one with just that pair).
pub(super) const SUBSCRIPT: super::native::NativeSubscript = super::native::NativeSubscript {
    what: "hstore",
    fetch: subscript_fetch,
    assign: subscript_assign,
};

fn subscript_fetch(a: &[crate::types::Value], _: &crate::sql::func::FnCtx) -> crate::Result<crate::types::Value> {
    use crate::types::Value;
    let h = Hs::new(build::hs_bytes(&a[0]));
    Ok(match h.find(None, &build::text_bytes(&a[1])) {
        Some(i) => h.val(i).map_or(Value::Null, build::bytes_text),
        None => Value::Null,
    })
}

fn subscript_assign(a: &[crate::types::Value], _: &crate::sql::func::FnCtx) -> crate::Result<crate::types::Value> {
    if a[1].is_null() {
        return Err(crate::Error::new(
            crate::error::code::NULL_VALUE_NOT_ALLOWED,
            "hstore subscript in assignment must not be null",
        ));
    }
    let key = build::text_bytes(&a[1]);
    io::check_key_len(key.len())?;
    let val = (!a[2].is_null()).then(|| build::text_bytes(&a[2]));
    if let Some(v) = &val {
        io::check_val_len(v.len())?;
    }
    let h = Hs::new(build::hs_bytes(&a[0]));
    let mut out: Vec<(&[u8], Option<&[u8]>)> = Vec::with_capacity(h.count() + 1);
    let mut placed = false;
    for (k, v) in h.pairs() {
        match key_order(k, &key) {
            Ordering::Less => out.push((k, v)),
            o => {
                if !placed {
                    out.push((&key, val.as_deref()));
                    placed = true;
                }
                if o == Ordering::Greater {
                    out.push((k, v));
                }
            }
        }
    }
    if !placed {
        out.push((&key, val.as_deref()));
    }
    Ok(build::hs_value(encode(out)))
}

pub(super) fn symbol(sym: &str) -> Option<Native> {
    build::symbol(sym)
        .or_else(|| ops::symbol(sym))
        .or_else(|| index::symbol(sym))
}
