//! Live docs: which documents of a segment are not deleted.
//!
//! Segments are immutable; deletes and updates produce a new generation of this small object
//! (`search/seg/<id>.<gen>.liv`). It is a plain bitset (bit set = live) with a header naming the
//! segment and generation, and a CRC32C footer:
//!
//! ```text
//! "RLLV" | version u32 | segment id [16] | generation u64 | max_doc u32 | deleted u32 |
//! words u64 × ceil(max_doc / 64) | crc32c u32
//! ```
//!
//! In memory it is a `Vec<u64>`; readers share it behind an `Arc` and writers copy it on write.

use crate::NO_MORE_DOCS;
use crate::error::{Result, corrupt, invalid};
use crate::io::{In, Out};

const MAGIC: [u8; 4] = *b"RLLV";
const VERSION: u32 = 1;

/// A live-docs bitset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveDocs {
    max_doc: u32,
    deleted: u32,
    words: Vec<u64>,
}

impl LiveDocs {
    /// Every doc live.
    pub fn all_live(max_doc: u32) -> LiveDocs {
        let mut words = vec![u64::MAX; (max_doc as usize).div_ceil(64)];
        if !max_doc.is_multiple_of(64)
            && let Some(last) = words.last_mut()
        {
            *last = (1u64 << (max_doc % 64)) - 1;
        }
        LiveDocs {
            max_doc,
            deleted: 0,
            words,
        }
    }

    pub fn max_doc(&self) -> u32 {
        self.max_doc
    }

    /// Number of deleted docs.
    pub fn num_deleted(&self) -> u32 {
        self.deleted
    }

    pub fn num_live(&self) -> u32 {
        self.max_doc - self.deleted
    }

    #[inline]
    pub fn is_live(&self, doc: u32) -> bool {
        doc < self.max_doc && self.words[(doc >> 6) as usize] & (1 << (doc & 63)) != 0
    }

    /// Deletes `doc`; returns whether it was live.
    pub fn delete(&mut self, doc: u32) -> bool {
        if !self.is_live(doc) {
            return false;
        }
        self.words[(doc >> 6) as usize] &= !(1 << (doc & 63));
        self.deleted += 1;
        true
    }

    /// First live doc `>= from`, or [`NO_MORE_DOCS`].
    pub fn next_live(&self, from: u32) -> u32 {
        if from >= self.max_doc {
            return NO_MORE_DOCS;
        }
        let mut w = (from >> 6) as usize;
        let mut word = self.words[w] & (u64::MAX << (from & 63));
        loop {
            if word != 0 {
                let d = (w as u32) * 64 + word.trailing_zeros();
                return if d < self.max_doc { d } else { NO_MORE_DOCS };
            }
            w += 1;
            if w >= self.words.len() {
                return NO_MORE_DOCS;
            }
            word = self.words[w];
        }
    }

    /// The raw words (bit `d` of word `d / 64` set = live).
    pub fn words(&self) -> &[u64] {
        &self.words
    }

    /// Serializes generation `gen` of the live docs of `segment_id`.
    pub fn encode(&self, segment_id: [u8; 16], generation: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(44 + self.words.len() * 8);
        out.put_bytes(&MAGIC);
        out.put_u32(VERSION);
        out.put_bytes(&segment_id);
        out.put_u64(generation);
        out.put_u32(self.max_doc);
        out.put_u32(self.deleted);
        for w in &self.words {
            out.put_u64(*w);
        }
        let crc = crc32c::crc32c(&out);
        out.put_u32(crc);
        out
    }

    /// Parses live docs, checking they belong to `segment_id`; returns them with their
    /// generation.
    pub fn decode(bytes: &[u8], segment_id: [u8; 16]) -> Result<(LiveDocs, u64)> {
        if bytes.len() < 44 {
            return Err(corrupt("live docs too short"));
        }
        let (body, crc) = bytes.split_at(bytes.len() - 4);
        if crc32c::crc32c(body) != u32::from_le_bytes(crc.try_into().unwrap()) {
            return Err(corrupt("live docs checksum mismatch"));
        }
        let mut r = In::new(body);
        if r.bytes(4)? != MAGIC {
            return Err(corrupt("bad live docs magic"));
        }
        if r.u32()? != VERSION {
            return Err(corrupt("unsupported live docs version"));
        }
        let id: [u8; 16] = r.array()?;
        if id != segment_id {
            return Err(invalid("live docs belong to another segment"));
        }
        let generation = r.u64()?;
        let max_doc = r.u32()?;
        let deleted = r.u32()?;
        let n = (max_doc as usize).div_ceil(64);
        if r.remaining() != n * 8 {
            return Err(corrupt("live docs length mismatch"));
        }
        let mut words = Vec::with_capacity(n);
        for _ in 0..n {
            words.push(r.u64()?);
        }
        let live: u32 = words.iter().map(|w| w.count_ones()).sum();
        if max_doc.checked_sub(live) != Some(deleted) {
            return Err(corrupt("live docs count mismatch"));
        }
        if !max_doc.is_multiple_of(64) && words.last().is_some_and(|w| w >> (max_doc % 64) != 0) {
            return Err(corrupt("live docs bits past max_doc"));
        }
        Ok((
            LiveDocs {
                max_doc,
                deleted,
                words,
            },
            generation,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_iterate_round_trip() {
        for max_doc in [0u32, 1, 63, 64, 65, 1000] {
            let mut l = LiveDocs::all_live(max_doc);
            assert_eq!(l.num_live(), max_doc);
            for d in (0..max_doc).step_by(3) {
                assert!(l.delete(d));
                assert!(!l.delete(d));
            }
            assert!(!l.delete(max_doc));
            let mut live = Vec::new();
            let mut d = l.next_live(0);
            while d != NO_MORE_DOCS {
                live.push(d);
                d = l.next_live(d + 1);
            }
            let want: Vec<u32> = (0..max_doc).filter(|d| d % 3 != 0).collect();
            assert_eq!(live, want);
            let bytes = l.encode([1; 16], 7);
            let (back, g) = LiveDocs::decode(&bytes, [1; 16]).unwrap();
            assert_eq!(back, l);
            assert_eq!(g, 7);
            assert!(LiveDocs::decode(&bytes, [2; 16]).is_err());
            for i in 0..bytes.len() {
                let mut bad = bytes.clone();
                bad[i] ^= 4;
                assert!(LiveDocs::decode(&bad, [1; 16]).is_err());
            }
        }
    }
}
