//! Term vectors: per-document inverted fields (`term_vector` mapping option), for the
//! `_termvectors` API, the fast vector highlighter and more-like-this.
//!
//! The segment format has no term vector section; a doc's term vectors are serialized into
//! its stored fields under the field number with [`TERM_VECTORS_STORED_BIT`] set, so they
//! travel with the stored document through refreshes, merges and reordering for free and
//! cost nothing for fields that don't ask for them. Encoding (all varints):
//!
//! ```text
//! flags: u8 (1 positions, 2 offsets, 4 payloads)   num_terms
//! per term, in byte order: shared prefix len, suffix len, suffix, freq,
//!   per occurrence: [position delta] [start delta, length] [payload len, payload]
//! ```

use search_codec::stored::{StoredValue, StoredVisitor, VisitStatus};

use crate::buffer::TvOcc;
use crate::buffer::termhash::TermHash;
use crate::error::{Error, Result};
use crate::schema::{TERM_VECTORS_STORED_BIT, TermVectorOptions};

fn put_vu32(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_vu32(b: &[u8], p: &mut usize) -> Result<u32> {
    let mut v = 0u32;
    let mut shift = 0;
    loop {
        let x = *b.get(*p).ok_or_else(|| Error::illegal_state("truncated term vectors"))?;
        *p += 1;
        if shift >= 32 {
            return Err(Error::illegal_state("bad term vector varint"));
        }
        v |= ((x & 0x7f) as u32) << shift;
        if x < 0x80 {
            return Ok(v);
        }
        shift += 7;
    }
}

/// Encodes one doc's occurrences of one field.
pub(crate) fn encode(
    out: &mut Vec<u8>,
    tv: TermVectorOptions,
    terms: &TermHash,
    occ: &mut [TvOcc],
    payloads: &[u8],
) -> Result<()> {
    occ.sort_by(|a, b| terms.get(a.term).cmp(terms.get(b.term)).then(a.pos.cmp(&b.pos)));
    let flags = tv.positions as u8 | (tv.offsets as u8) << 1 | (tv.payloads as u8) << 2;
    out.push(flags);
    let mut n = 0u32;
    let mut i = 0;
    while i < occ.len() {
        let t = occ[i].term;
        let mut j = i;
        while j < occ.len() && occ[j].term == t {
            j += 1;
        }
        n += 1;
        i = j;
    }
    put_vu32(out, n);
    let mut prev: &[u8] = &[];
    let mut i = 0;
    while i < occ.len() {
        let t = occ[i].term;
        let mut j = i;
        while j < occ.len() && occ[j].term == t {
            j += 1;
        }
        let term = terms.get(t);
        let shared = prev.iter().zip(term).take_while(|(a, b)| a == b).count();
        put_vu32(out, shared as u32);
        put_vu32(out, (term.len() - shared) as u32);
        out.extend_from_slice(&term[shared..]);
        put_vu32(out, (j - i) as u32);
        let (mut lp, mut ls) = (0u32, 0u32);
        for o in &occ[i..j] {
            if tv.positions {
                put_vu32(out, o.pos.wrapping_sub(lp));
                lp = o.pos;
            }
            if tv.offsets {
                put_vu32(out, o.start.wrapping_sub(ls));
                put_vu32(out, o.end.saturating_sub(o.start));
                ls = o.start;
            }
            if tv.payloads {
                let p = &payloads[o.pay_start as usize..(o.pay_start + o.pay_len) as usize];
                put_vu32(out, p.len() as u32);
                out.extend_from_slice(p);
            }
        }
        prev = term;
        i = j;
    }
    Ok(())
}

/// One occurrence of a term in a doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TvToken {
    pub position: Option<u32>,
    pub start_offset: Option<u32>,
    pub end_offset: Option<u32>,
    pub payload: Option<Vec<u8>>,
}

/// One term of a doc's field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TvTerm {
    pub term: Vec<u8>,
    pub freq: u32,
    pub tokens: Vec<TvToken>,
}

/// A field's term vector of one doc.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TermVector {
    pub positions: bool,
    pub offsets: bool,
    pub payloads: bool,
    /// In term byte order.
    pub terms: Vec<TvTerm>,
}

/// Decodes `encode`'s output.
pub fn decode(b: &[u8]) -> Result<TermVector> {
    let mut p = 0;
    let flags = *b.first().ok_or_else(|| Error::illegal_state("empty term vectors"))?;
    p += 1;
    let (pos, off, pay) = (flags & 1 != 0, flags & 2 != 0, flags & 4 != 0);
    let n = get_vu32(b, &mut p)?;
    let mut tv = TermVector { positions: pos, offsets: off, payloads: pay, terms: Vec::new() };
    let mut prev: Vec<u8> = Vec::new();
    for _ in 0..n {
        let shared = get_vu32(b, &mut p)? as usize;
        let suffix = get_vu32(b, &mut p)? as usize;
        if shared > prev.len() || p + suffix > b.len() {
            return Err(Error::illegal_state("corrupt term vectors"));
        }
        let mut term = prev[..shared].to_vec();
        term.extend_from_slice(&b[p..p + suffix]);
        p += suffix;
        let freq = get_vu32(b, &mut p)?;
        let mut tokens = Vec::with_capacity((freq as usize).min(b.len()));
        let (mut lp, mut ls) = (0u32, 0u32);
        for _ in 0..freq {
            let mut t = TvToken { position: None, start_offset: None, end_offset: None, payload: None };
            if pos {
                lp = lp.wrapping_add(get_vu32(b, &mut p)?);
                t.position = Some(lp);
            }
            if off {
                ls = ls.wrapping_add(get_vu32(b, &mut p)?);
                let len = get_vu32(b, &mut p)?;
                t.start_offset = Some(ls);
                t.end_offset = Some(ls.wrapping_add(len));
            }
            if pay {
                let len = get_vu32(b, &mut p)? as usize;
                if p + len > b.len() {
                    return Err(Error::illegal_state("corrupt term vectors"));
                }
                t.payload = Some(b[p..p + len].to_vec());
                p += len;
            }
            tokens.push(t);
        }
        prev = term.clone();
        tv.terms.push(TvTerm { term, freq, tokens });
    }
    Ok(tv)
}

/// Collects the stored term vector blobs of a doc: `(field, bytes)`.
#[derive(Default)]
pub struct TvCollector {
    /// Only these fields (None: all).
    pub fields: Option<Vec<u32>>,
    pub found: Vec<(u32, Vec<u8>)>,
}

impl StoredVisitor for TvCollector {
    fn needs_field(&mut self, field: u32) -> VisitStatus {
        if field & TERM_VECTORS_STORED_BIT == 0 {
            return VisitStatus::No;
        }
        let f = field & !TERM_VECTORS_STORED_BIT;
        match &self.fields {
            Some(fs) if !fs.contains(&f) => VisitStatus::No,
            _ => VisitStatus::Yes,
        }
    }

    fn value(&mut self, field: u32, value: StoredValue<'_>) {
        if let StoredValue::Bytes(b) = value {
            self.found.push((field & !TERM_VECTORS_STORED_BIT, b.to_vec()));
        }
    }
}

/// A field's term vector of one doc with optional statistics (the `_termvectors` API).
#[derive(Clone, Debug, PartialEq)]
pub struct FieldTermVector {
    pub field: u32,
    pub vector: TermVector,
    /// `field_statistics`: doc count, sum of doc freqs, sum of total term freqs.
    pub field_stats: Option<crate::similarity::CollectionStats>,
    /// `term_statistics`: per term of `vector` (same order): doc freq and total term freq.
    pub term_stats: Vec<crate::similarity::TermStats>,
}

/// The term vectors among a doc's stored fields (`(field, value)` as stored, e.g. from a
/// realtime get).
pub fn from_stored(stored: &[(u32, search_codec::stored::OwnedStoredValue)], fields: Option<&[u32]>) -> Result<Vec<(u32, TermVector)>> {
    let mut out = Vec::new();
    for (f, v) in stored {
        if f & TERM_VECTORS_STORED_BIT == 0 {
            continue;
        }
        let field = f & !TERM_VECTORS_STORED_BIT;
        if fields.is_some_and(|fs| !fs.contains(&field)) {
            continue;
        }
        if let search_codec::stored::OwnedStoredValue::Bytes(b) = v {
            out.push((field, decode(b)?));
        }
    }
    Ok(out)
}

/// Whether a stored field number carries term vectors (hide it from `_source`-like output).
pub fn is_term_vector_field(stored_field: u32) -> bool {
    stored_field & TERM_VECTORS_STORED_BIT != 0
}

impl crate::search::Searcher {
    /// The stored term vectors of global doc `doc` (`fields`: only these), with statistics
    /// from this searcher's view when asked.
    pub fn term_vectors(&self, doc: u32, fields: Option<&[u32]>, term_statistics: bool, field_statistics: bool) -> Result<Vec<FieldTermVector>> {
        let (seg, local) = self.segment_of(doc).ok_or_else(|| Error::illegal(format!("doc {doc} out of range")))?;
        let mut c = TvCollector { fields: fields.map(|f| f.to_vec()), found: Vec::new() };
        seg.ensure_stored()?;
        seg.reader.stored().visit(local, &mut c)?;
        let mut out = Vec::with_capacity(c.found.len());
        for (field, bytes) in c.found {
            let vector = decode(&bytes)?;
            out.push(self.with_stats(field, vector, term_statistics, field_statistics));
        }
        out.sort_by_key(|f| f.field);
        Ok(out)
    }

    /// Adds statistics to a term vector (also for vectors computed on the fly by the caller,
    /// e.g. of artificial documents or fields without stored vectors).
    pub fn with_stats(&self, field: u32, vector: TermVector, term_statistics: bool, field_statistics: bool) -> FieldTermVector {
        let term_stats = if term_statistics { vector.terms.iter().map(|t| self.term_stats(field, &t.term)).collect() } else { Vec::new() };
        let field_stats = if field_statistics { self.collection_stats(field) } else { None };
        FieldTermVector { field, vector, field_stats, term_stats }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut h = TermHash::new();
        let a = h.add(b"brown").0;
        let b = h.add(b"brow").0;
        let c = h.add(b"zebra").0;
        let payloads = b"xyz".to_vec();
        let mut occ = vec![
            TvOcc { term: c, pos: 5, start: 30, end: 35, pay_start: 0, pay_len: 0 },
            TvOcc { term: a, pos: 1, start: 4, end: 9, pay_start: 0, pay_len: 2 },
            TvOcc { term: b, pos: 2, start: 10, end: 14, pay_start: 2, pay_len: 1 },
            TvOcc { term: a, pos: 7, start: 40, end: 45, pay_start: 0, pay_len: 0 },
        ];
        let opts = TermVectorOptions { positions: true, offsets: true, payloads: true };
        let mut out = Vec::new();
        encode(&mut out, opts, &h, &mut occ, &payloads).unwrap();
        let tv = decode(&out).unwrap();
        let terms: Vec<&[u8]> = tv.terms.iter().map(|t| &t.term[..]).collect();
        assert_eq!(terms, vec![&b"brow"[..], b"brown", b"zebra"]);
        assert_eq!(tv.terms[1].freq, 2);
        assert_eq!(tv.terms[1].tokens[1].position, Some(7));
        assert_eq!(tv.terms[1].tokens[1].start_offset, Some(40));
        assert_eq!(tv.terms[1].tokens[0].payload.as_deref(), Some(&b"xy"[..]));
        assert_eq!(tv.terms[0].tokens[0].payload.as_deref(), Some(&b"z"[..]));
        assert!(decode(&out[..out.len() - 2]).is_err());
    }
}
