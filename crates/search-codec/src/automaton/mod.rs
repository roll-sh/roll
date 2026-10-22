//! Byte-level automata for term dictionary intersection.
//!
//! Prefix, wildcard, regexp and fuzzy queries all reduce to "every term accepted by a DFA over
//! UTF-8 bytes". [`Automaton`] is that interface: states are `u32`s, so it is object safe and a
//! query can hold a `Box<dyn Automaton>` regardless of how the automaton was built. Term
//! dictionaries run it over each term incrementally (only the bytes past the prefix shared with
//! the previous term) and, when a prefix leads to a dead state, ask
//! [`next_live_byte`](Automaton::next_live_byte) for the next string that could still match
//! and seek straight to it, so runs of non-matching terms are skipped rather than visited.
//!
//! Builders:
//! - [`Prefix`] and [`AlwaysMatch`] are trivial.
//! - [`Dfa`] is a dense, byte-class-compressed DFA produced by subset construction from an NFA
//!   over Unicode code points ([`nfa`]), so every automaton that counts characters (fuzzy edits,
//!   `?` in wildcards, `.` in regexps) does so on code points like Lucene, while matching bytes.
//! - [`Dfa::levenshtein`] builds Lucene's Levenshtein automata (with optional transpositions,
//!   i.e. optimal-string-alignment distance) after an exact prefix.
//! - [`Dfa::wildcard`] implements Lucene's `WildcardQuery` syntax.
//! - [`regexp`] parses Lucene's `RegExp` syntax (with its flags) into the same NFA.

mod casefold;
mod cp;
pub mod dfa;
pub mod nfa;
pub mod regexp;

pub use dfa::{Dfa, TooComplex};

/// A fast non-cryptographic hasher (FxHash) for the sets of states automaton construction keys
/// its maps by; inputs are our own state numbers, never untrusted.
#[derive(Clone, Copy, Default)]
pub(crate) struct FxHasher(u64);

const FX_SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FxHasher {
    #[inline]
    fn add(&mut self, v: u64) {
        self.0 = (self.0.rotate_left(5) ^ v).wrapping_mul(FX_SEED);
    }
}

impl std::hash::Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let mut chunks = bytes.chunks_exact(8);
        for c in &mut chunks {
            self.add(u64::from_le_bytes(c.try_into().unwrap()));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut b = [0u8; 8];
            b[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(b));
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

pub(crate) type FxBuild = std::hash::BuildHasherDefault<FxHasher>;

/// The state that rejects everything.
pub const DEAD: u32 = u32::MAX;

/// A deterministic automaton over bytes.
pub trait Automaton: Send + Sync {
    /// The initial state.
    fn start(&self) -> u32;
    /// Whether `state` accepts (the bytes consumed so far form a match).
    fn is_match(&self, state: u32) -> bool;
    /// Whether some continuation from `state` can still match; `false` prunes the branch.
    fn can_match(&self, state: u32) -> bool {
        state != DEAD
    }
    /// Whether every continuation from `state` matches (lets intersection stop checking).
    fn will_always_match(&self, _state: u32) -> bool {
        false
    }
    /// The state after reading `byte`.
    fn accept(&self, state: u32, byte: u8) -> u32;

    /// The smallest byte `>= from` after which a match is still possible from `state`. Term
    /// dictionaries use it to jump over every term that cannot match.
    fn next_live_byte(&self, state: u32, from: u8) -> Option<u8> {
        (from..=255).find(|&b| self.can_match(self.accept(state, b)))
    }

    /// Runs the automaton over a whole input.
    fn matches(&self, input: &[u8]) -> bool {
        let mut s = self.start();
        for &b in input {
            if !self.can_match(s) {
                return false;
            }
            s = self.accept(s, b);
        }
        self.is_match(s)
    }
}

impl<A: Automaton + ?Sized> Automaton for &A {
    fn start(&self) -> u32 {
        (**self).start()
    }
    fn is_match(&self, s: u32) -> bool {
        (**self).is_match(s)
    }
    fn can_match(&self, s: u32) -> bool {
        (**self).can_match(s)
    }
    fn will_always_match(&self, s: u32) -> bool {
        (**self).will_always_match(s)
    }
    fn accept(&self, s: u32, b: u8) -> u32 {
        (**self).accept(s, b)
    }
    fn next_live_byte(&self, s: u32, from: u8) -> Option<u8> {
        (**self).next_live_byte(s, from)
    }
}

impl<A: Automaton + ?Sized> Automaton for Box<A> {
    fn start(&self) -> u32 {
        (**self).start()
    }
    fn is_match(&self, s: u32) -> bool {
        (**self).is_match(s)
    }
    fn can_match(&self, s: u32) -> bool {
        (**self).can_match(s)
    }
    fn will_always_match(&self, s: u32) -> bool {
        (**self).will_always_match(s)
    }
    fn accept(&self, s: u32, b: u8) -> u32 {
        (**self).accept(s, b)
    }
    fn next_live_byte(&self, s: u32, from: u8) -> Option<u8> {
        (**self).next_live_byte(s, from)
    }
}

/// Accepts everything.
#[derive(Clone, Copy, Debug, Default)]
pub struct AlwaysMatch;

impl Automaton for AlwaysMatch {
    fn start(&self) -> u32 {
        0
    }
    fn is_match(&self, _: u32) -> bool {
        true
    }
    fn will_always_match(&self, _: u32) -> bool {
        true
    }
    fn accept(&self, _: u32, _: u8) -> u32 {
        0
    }
    fn next_live_byte(&self, _: u32, from: u8) -> Option<u8> {
        Some(from)
    }
}

/// Accepts every input starting with a byte prefix. State `i` means `i` prefix bytes matched.
#[derive(Clone, Debug)]
pub struct Prefix {
    prefix: Vec<u8>,
}

impl Prefix {
    pub fn new(prefix: impl Into<Vec<u8>>) -> Prefix {
        Prefix {
            prefix: prefix.into(),
        }
    }
}

impl Automaton for Prefix {
    fn start(&self) -> u32 {
        0
    }
    fn is_match(&self, s: u32) -> bool {
        s as usize == self.prefix.len()
    }
    fn will_always_match(&self, s: u32) -> bool {
        s as usize == self.prefix.len()
    }
    fn accept(&self, s: u32, b: u8) -> u32 {
        if s == DEAD {
            return DEAD;
        }
        let i = s as usize;
        if i == self.prefix.len() {
            s
        } else if self.prefix[i] == b {
            s + 1
        } else {
            DEAD
        }
    }
    fn next_live_byte(&self, s: u32, from: u8) -> Option<u8> {
        match self.prefix.get(s as usize) {
            _ if s == DEAD => None,
            None => Some(from),
            Some(&b) => (b >= from).then_some(b),
        }
    }
}

/// The smallest string greater than every string starting with `term[..=k]` from which the
/// automaton may still reach a match: the next live byte after `term[d]` at the deepest
/// depth `d <= k` that has one. `states[d]` is the state after `term[..d]`.
pub(crate) fn next_candidate(aut: &dyn Automaton, states: &[u32], term: &[u8], k: usize, out: &mut Vec<u8>) -> bool {
    let mut d = k.min(states.len().saturating_sub(1)).min(term.len().saturating_sub(1));
    loop {
        if let Some(&byte) = term.get(d)
            && byte < 255
            && let Some(c) = aut.next_live_byte(states[d], byte + 1)
        {
            out.clear();
            out.extend_from_slice(&term[..d]);
            out.push(c);
            return true;
        }
        if d == 0 {
            return false;
        }
        d -= 1;
    }
}

/// The bytes every accepted string starts with (`foo` for `foo*bar`), up to 256.
pub(crate) fn literal_prefix(aut: &dyn Automaton) -> Vec<u8> {
    let mut out = Vec::new();
    let mut s = aut.start();
    while out.len() < 256 && !aut.is_match(s) && !aut.will_always_match(s) {
        let Some(c) = aut.next_live_byte(s, 0) else { break };
        if c < 255 && aut.next_live_byte(s, c + 1).is_some() {
            break;
        }
        out.push(c);
        s = aut.accept(s, c);
    }
    out
}

/// Optimal-string-alignment (restricted Damerau-) Levenshtein distance over code points, or
/// plain Levenshtein without transpositions. Used to score fuzzy matches and in tests.
pub fn edit_distance(a: &str, b: &str, transpositions: bool) -> u32 {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let w = m + 1;
    let mut d = vec![0u32; (n + 1) * w];
    for i in 0..=n {
        d[i * w] = i as u32;
    }
    for (j, v) in d.iter_mut().enumerate().take(m + 1) {
        *v = j as u32;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = (a[i - 1] != b[j - 1]) as u32;
            let mut v = (d[(i - 1) * w + j] + 1)
                .min(d[i * w + j - 1] + 1)
                .min(d[(i - 1) * w + j - 1] + cost);
            if transpositions && i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[(i - 2) * w + j - 2] + 1);
            }
            d[i * w + j] = v;
        }
    }
    d[n * w + m]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_and_always() {
        let p = Prefix::new("ab");
        assert!(p.matches(b"ab"));
        assert!(p.matches(b"abc"));
        assert!(!p.matches(b"a"));
        assert!(!p.matches(b"ba"));
        assert!(Prefix::new("").matches(b""));
        assert!(AlwaysMatch.matches(b"anything"));
    }

    #[test]
    fn distances() {
        assert_eq!(edit_distance("kitten", "sitting", false), 3);
        assert_eq!(edit_distance("ab", "ba", false), 2);
        assert_eq!(edit_distance("ab", "ba", true), 1);
        assert_eq!(edit_distance("ca", "abc", true), 3);
        assert_eq!(edit_distance("", "日本", true), 2);
    }
}
