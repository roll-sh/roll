//! OpenSearch's `flat_object` multi-term queries on a dotted sub-field (`catalog.author.given`).
//!
//! OpenSearch stores every leaf of a `flat_object` twice: as its value (`<field>._value`) and as
//! `<path>=<value>` (`<field>._valueAndPath`), and runs wildcard, regexp and fuzzy queries on a
//! sub-field against the second with the pattern prefixed by `<path>=`. roll indexes the same
//! leaves as `flattened` does, `key\0value` terms on the `._keyed` field, so a sub-field query
//! is the pattern's automaton behind the literal `key\0` prefix: [`Keyed`] runs the prefix byte
//! by byte and then hands over to the inner automaton, which keeps term-dictionary seeking
//! (`next_live_byte`) working through the prefix.
//!
//! Elasticsearch's keyed `flattened` fields refuse these queries; only `flat_object` (the
//! opensearch dialect's type) reaches this module.

use crate::engine::fields::FieldInfo;
use search_codec::automaton::{Automaton, DEAD};
use std::sync::Arc;

/// Whether `f` is a dotted sub-field of an OpenSearch `flat_object`.
pub fn is_keyed(f: &FieldInfo) -> bool {
    f.flattened_key.is_some() && f.ty == "flat_object"
}

/// Accepts `prefix` followed by anything `inner` accepts. States `0..prefix.len()` count the
/// prefix bytes read; inner state `s` is `prefix.len() + s`.
pub struct Keyed {
    prefix: Vec<u8>,
    inner: Arc<dyn Automaton>,
}

impl Keyed {
    pub fn new(prefix: Vec<u8>, inner: Arc<dyn Automaton>) -> Keyed {
        Keyed { prefix, inner }
    }

    fn plen(&self) -> u32 {
        self.prefix.len() as u32
    }

    /// The inner state of an outer state past the prefix.
    #[inline]
    fn inner_state(&self, s: u32) -> u32 {
        s - self.plen()
    }

    #[inline]
    fn outer(&self, inner: u32) -> u32 {
        if inner == DEAD { DEAD } else { inner.checked_add(self.plen()).unwrap_or(DEAD) }
    }
}

impl Automaton for Keyed {
    fn start(&self) -> u32 {
        if self.prefix.is_empty() { self.outer(self.inner.start()) } else { 0 }
    }

    fn is_match(&self, s: u32) -> bool {
        s != DEAD && s >= self.plen() && self.inner.is_match(self.inner_state(s))
    }

    fn can_match(&self, s: u32) -> bool {
        s != DEAD && (s < self.plen() || self.inner.can_match(self.inner_state(s)))
    }

    fn will_always_match(&self, s: u32) -> bool {
        s != DEAD && s >= self.plen() && self.inner.will_always_match(self.inner_state(s))
    }

    fn accept(&self, s: u32, b: u8) -> u32 {
        if s == DEAD {
            return DEAD;
        }
        let plen = self.plen();
        if s < plen {
            if self.prefix[s as usize] != b {
                return DEAD;
            }
            return if s + 1 == plen { self.outer(self.inner.start()) } else { s + 1 };
        }
        self.outer(self.inner.accept(self.inner_state(s), b))
    }

    fn next_live_byte(&self, s: u32, from: u8) -> Option<u8> {
        if s == DEAD {
            return None;
        }
        let plen = self.plen();
        if s < plen {
            let want = self.prefix[s as usize];
            return (want >= from).then_some(want);
        }
        self.inner.next_live_byte(self.inner_state(s), from)
    }
}

/// The `key\0` prefix of a keyed sub-field's terms.
pub fn key_prefix(f: &FieldInfo) -> Vec<u8> {
    let key = f.flattened_key.as_deref().unwrap_or("");
    let mut p = Vec::with_capacity(key.len() + 1);
    p.extend_from_slice(key.as_bytes());
    p.push(0);
    p
}

/// Wraps a wildcard or regexp query built over a sub-field's values so it matches the
/// sub-field's `key\0value` terms (OpenSearch searches `<path>=<pattern>`).
pub fn keyed_query(f: &FieldInfo, q: search_index::query::MultiTermQuery) -> search_index::query::MultiTermQuery {
    use search_index::query::{MultiTermQuery, TermsMatcher};
    match q.matcher {
        TermsMatcher::Automaton { aut, desc } => {
            MultiTermQuery::automaton(q.field, Arc::new(Keyed::new(key_prefix(f), aut)), format!("{}={desc}", f.name)).with_rewrite(q.rewrite)
        }
        matcher => MultiTermQuery { matcher, ..q },
    }
}

/// The string whose length `AUTO` fuzziness measures on a sub-field: OpenSearch fuzzes the
/// whole `<path>=<value>` it searches for, not the value alone.
pub fn fuzzy_subject(f: &FieldInfo, value: &str) -> String {
    format!("{}={value}", f.name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use search_codec::automaton::Dfa;

    fn keyed(prefix: &str, pattern: &str) -> Keyed {
        let inner = Dfa::wildcard(pattern, false, search_codec::automaton::dfa::DEFAULT_WORK_LIMIT).unwrap();
        Keyed::new(prefix.as_bytes().to_vec(), Arc::new(inner))
    }

    #[test]
    fn prefix_then_inner() {
        let a = keyed("a.b\0", "ab*");
        assert!(a.matches(b"a.b\0abc8"));
        assert!(a.matches(b"a.b\0ab"));
        assert!(!a.matches(b"a.b\0xab"));
        assert!(!a.matches(b"a.c\0abc"));
        assert!(!a.matches(b"a.b"));
        assert!(!a.matches(b""));
    }

    #[test]
    fn empty_prefix_is_inner() {
        let a = keyed("", "x?z");
        assert!(a.matches(b"xyz"));
        assert!(!a.matches(b"xz"));
    }

    #[test]
    fn seeks_through_the_prefix() {
        let a = keyed("k\0", "b*");
        let s = a.start();
        assert_eq!(a.next_live_byte(s, 0), Some(b'k'));
        assert_eq!(a.next_live_byte(s, b'l'), None);
        let s = a.accept(s, b'k');
        assert_eq!(a.next_live_byte(s, 0), Some(0));
        let s = a.accept(s, 0);
        assert_eq!(a.next_live_byte(s, b'a'), Some(b'b'));
        assert!(!a.can_match(a.accept(s, b'a')));
        assert!(a.will_always_match(a.accept(s, b'b')));
    }
}
