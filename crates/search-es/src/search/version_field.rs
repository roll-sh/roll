//! Queries on `version` fields (x-pack's `VersionStringFieldType`).
//!
//! The field's terms hold [`search_aggs::version_string`]'s precedence-ordered encoding, so term
//! and range queries just encode their values. Patterns are written against the text users see,
//! though: `prefix`, `wildcard`, `regexp` and `fuzzy` match the *decoded* terms. Elasticsearch
//! brute-forces regexp and fuzzy (decoding every term) and builds a wildcard automaton that
//! tolerates the marker bytes; [`Decoded`] does both in one way that still intersects with the
//! term dictionary: it runs the pattern's automaton over the encoded bytes while skipping the
//! markers, so dead prefixes are pruned exactly as for a keyword field.

use std::sync::Arc;

use search_aggs::version_string::{NO_PRERELEASE_SEPARATOR, NUMERIC_MARKER, PRERELEASE_SEPARATOR};
use search_codec::automaton::{Automaton, DEAD, Dfa};
use search_index::query::{MultiTermQuery, RewriteMethod, TermsMatcher};

use crate::engine::fields::FieldInfo;
use crate::error::{EsError, EsResult};

/// The encoded term of a version value (term and range queries, `search_after`, `missing`).
pub fn encode(s: &str) -> EsResult<Vec<u8>> {
    search_aggs::version_string::encode(s).map_err(|e| EsError::java(400, e.kind, e.reason))
}

/// The text of an encoded term.
pub fn decode(b: &[u8]) -> String {
    search_aggs::version_string::decode(b)
}

/// Runs `inner` over the decoded form of encoded version terms. A state is `inner_state << 1`,
/// plus 1 right after a numeric marker (whose length byte comes next and is skipped).
pub struct Decoded {
    inner: Arc<dyn Automaton>,
}

impl Decoded {
    pub fn new(inner: Arc<dyn Automaton>) -> Decoded {
        Decoded { inner }
    }

    fn wrap(inner: u32, skip: bool) -> u32 {
        if inner == DEAD || inner >= DEAD >> 1 { DEAD } else { (inner << 1) | skip as u32 }
    }
}

impl Automaton for Decoded {
    fn start(&self) -> u32 {
        Self::wrap(self.inner.start(), false)
    }

    fn is_match(&self, s: u32) -> bool {
        s != DEAD && s & 1 == 0 && self.inner.is_match(s >> 1)
    }

    fn can_match(&self, s: u32) -> bool {
        s != DEAD && self.inner.can_match(s >> 1)
    }

    fn will_always_match(&self, s: u32) -> bool {
        s != DEAD && self.inner.will_always_match(s >> 1)
    }

    fn accept(&self, s: u32, b: u8) -> u32 {
        if s == DEAD {
            return DEAD;
        }
        let inner = s >> 1;
        if s & 1 == 1 {
            return Self::wrap(inner, false);
        }
        match b {
            NUMERIC_MARKER => Self::wrap(inner, true),
            PRERELEASE_SEPARATOR | NO_PRERELEASE_SEPARATOR => s,
            _ => Self::wrap(self.inner.accept(inner, b), false),
        }
    }

    fn next_live_byte(&self, s: u32, from: u8) -> Option<u8> {
        if !self.can_match(s) {
            return None;
        }
        // Marker bytes (and the length byte after a numeric marker) keep the state alive.
        if s & 1 == 1 {
            return Some(from);
        }
        if from == 0 && self.inner.can_match(self.inner.accept(s >> 1, 0)) {
            return Some(0);
        }
        if from <= NO_PRERELEASE_SEPARATOR {
            return Some(from.max(NUMERIC_MARKER));
        }
        self.inner.next_live_byte(s >> 1, from)
    }
}

/// A multi-term query whose automaton matches decoded terms.
fn decoded(field: u32, aut: Arc<dyn Automaton>, desc: String) -> MultiTermQuery {
    MultiTermQuery::automaton(field, Arc::new(Decoded::new(aut)), desc)
}

/// `VersionFieldWildcardQuery`: `*` and `?` are the only special characters (a backslash is a
/// literal, there is no escaping).
pub fn wildcard(f: &FieldInfo, pattern: &str, case_insensitive: bool) -> EsResult<MultiTermQuery> {
    let escaped = pattern.replace('\\', "\\\\");
    let aut = Dfa::wildcard(&escaped, case_insensitive, search_codec::automaton::dfa::DEFAULT_WORK_LIMIT)
        .map_err(|e| EsError::new(400, "too_complex_to_determinize_exception", e.to_string()))?;
    Ok(decoded(f.number, Arc::new(aut), pattern.to_string()))
}

/// `prefixQuery`: the wildcard `value*` (so `*` and `?` in the value are wildcards too).
pub fn prefix(f: &FieldInfo, value: &str, case_insensitive: bool) -> EsResult<MultiTermQuery> {
    wildcard(f, &format!("{value}*"), case_insensitive)
}

/// `regexpQuery`: a regexp query built as for a keyword field, matched against decoded terms.
pub fn regexp(q: MultiTermQuery) -> MultiTermQuery {
    match q.matcher {
        TermsMatcher::Automaton { aut, desc } => MultiTermQuery::automaton(q.field, Arc::new(Decoded::new(aut)), desc).with_rewrite(q.rewrite),
        _ => q,
    }
}

/// `fuzzyQuery`: Lucene's Levenshtein automaton over decoded terms. Elasticsearch's version
/// enumerates them through a plain filter, so every matching term scores with boost 1 (no
/// edit-distance boost), still under `top_terms_blended_freqs_{max_expansions}`.
pub fn fuzzy(f: &FieldInfo, value: &str, max_edits: u32, prefix_len: usize, max_expansions: usize, transpositions: bool) -> MultiTermQuery {
    let edits = max_edits.min(2);
    let aut = Dfa::levenshtein(value, edits, prefix_len, transpositions);
    decoded(f.number, Arc::new(aut), format!("{value}~{edits}")).with_rewrite(RewriteMethod::TopTermsBlendedFreqs(max_expansions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(s: &str) -> Vec<u8> {
        encode(s).unwrap()
    }

    fn wildcard_matches(pattern: &str, value: &str) -> bool {
        let aut = Dfa::wildcard(&pattern.replace('\\', "\\\\"), false, search_codec::automaton::dfa::DEFAULT_WORK_LIMIT).unwrap();
        Decoded::new(Arc::new(aut)).matches(&enc(value))
    }

    #[test]
    fn patterns_match_decoded_terms() {
        assert!(wildcard_matches("1.1*", "1.1.12"));
        assert!(wildcard_matches("1.1*", "1.10.0-rc.1+b2"));
        assert!(!wildcard_matches("1.1*", "2.0.0-beta"));
        assert!(wildcard_matches("1.*.12", "1.1.12"));
        assert!(wildcard_matches("*-beta", "2.0.0-beta"));
        assert!(wildcard_matches("2.0.0-?eta", "2.0.0-beta"));
        assert!(wildcard_matches("*", ""));
        assert!(wildcard_matches("", ""));
        assert!(wildcard_matches("v1", "v1"));
        assert!(!wildcard_matches("\\1*", "1.1.12"));
        assert!(wildcard_matches("1.2.3-alpha.1?", "1.2.3-alpha.10"));
    }

    #[test]
    fn levenshtein_over_decoded_terms() {
        let d = Decoded::new(Arc::new(Dfa::levenshtein("1.1.13", 1, 0, true)));
        assert!(d.matches(&enc("1.1.12")));
        assert!(d.matches(&enc("1.1.13")));
        assert!(!d.matches(&enc("1.10.12")));
        assert!(!d.matches(&enc("2.0.0-beta")));
    }

    /// The live-byte jump must agree with stepping byte by byte.
    #[test]
    fn next_live_byte_is_consistent() {
        let aut = Dfa::wildcard("1.1*", false, search_codec::automaton::dfa::DEFAULT_WORK_LIMIT).unwrap();
        let d = Decoded::new(Arc::new(aut));
        let mut states = vec![d.start()];
        for b in enc("1.10") {
            let s = *states.last().unwrap();
            states.push(d.accept(s, b));
        }
        for &s in &states {
            for from in [0u8, 1, 3, 4, b'0', b'1', b'2', 0x81, 0xff] {
                let want = (from..=255).find(|&b| d.can_match(d.accept(s, b)));
                assert_eq!(d.next_live_byte(s, from), want, "state {s} from {from}");
            }
        }
    }
}
