//! Nondeterministic automata over Unicode code points, the common intermediate form of every
//! automaton builder (Levenshtein, wildcard, regexp).
//!
//! Transitions carry inclusive code point ranges; epsilon transitions connect fragments. The NFA
//! is converted to a byte DFA in [`super::dfa`]: each code point range is first expanded into
//! UTF-8 byte-range sequences (surrogates excluded), then determinized over byte classes.

/// Largest Unicode scalar value.
pub const MAX_CP: u32 = 0x10FFFF;

#[derive(Clone, Debug, Default)]
pub(crate) struct NState {
    pub accept: bool,
    pub ranges: Vec<(u32, u32, u32)>,
    /// Raw byte transitions (used to embed already-determinized byte automata).
    pub bytes: Vec<(u8, u8, u32)>,
    pub eps: Vec<u32>,
}

/// A code point NFA under construction.
#[derive(Clone, Debug, Default)]
pub struct Nfa {
    pub(crate) states: Vec<NState>,
    pub(crate) start: u32,
}

impl Nfa {
    pub fn new() -> Nfa {
        Nfa::default()
    }

    pub fn add_state(&mut self) -> u32 {
        self.states.push(NState::default());
        (self.states.len() - 1) as u32
    }

    pub fn set_start(&mut self, s: u32) {
        self.start = s;
    }

    pub fn start(&self) -> u32 {
        self.start
    }

    pub fn set_accept(&mut self, s: u32, accept: bool) {
        self.states[s as usize].accept = accept;
    }

    pub fn is_accept(&self, s: u32) -> bool {
        self.states[s as usize].accept
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    /// Adds `from --[lo, hi]--> to`.
    pub fn add_range(&mut self, from: u32, lo: u32, hi: u32, to: u32) {
        if lo <= hi {
            self.states[from as usize]
                .ranges
                .push((lo, hi.min(MAX_CP), to));
        }
    }

    /// Adds a transition on one code point.
    pub fn add_char(&mut self, from: u32, c: u32, to: u32) {
        self.add_range(from, c, c, to);
    }

    /// Adds a transition on any code point.
    pub fn add_any(&mut self, from: u32, to: u32) {
        self.add_range(from, 0, MAX_CP, to);
    }

    /// Adds a transition on a raw byte range (no UTF-8 expansion).
    pub fn add_byte_range(&mut self, from: u32, lo: u8, hi: u8, to: u32) {
        if lo <= hi {
            self.states[from as usize].bytes.push((lo, hi, to));
        }
    }

    pub fn add_epsilon(&mut self, from: u32, to: u32) {
        self.states[from as usize].eps.push(to);
    }

    /// Number of transitions (ranges plus epsilons).
    pub fn transition_count(&self) -> usize {
        self.states
            .iter()
            .map(|s| s.ranges.len() + s.eps.len())
            .sum()
    }

    /// Simulates the NFA on a string (reference semantics for tests).
    pub fn matches_str(&self, input: &str) -> bool {
        if self.states.is_empty() {
            return false;
        }
        let mut cur = vec![false; self.states.len()];
        let mut stack = vec![self.start];
        self.close(&mut cur, &mut stack);
        for c in input.chars() {
            let c = c as u32;
            let mut next = vec![false; self.states.len()];
            let mut stack = Vec::new();
            for (s, on) in cur.iter().enumerate() {
                if *on {
                    for &(lo, hi, to) in &self.states[s].ranges {
                        if lo <= c && c <= hi {
                            stack.push(to);
                        }
                    }
                }
            }
            self.close(&mut next, &mut stack);
            cur = next;
        }
        cur.iter()
            .enumerate()
            .any(|(s, on)| *on && self.states[s].accept)
    }

    fn close(&self, set: &mut [bool], stack: &mut Vec<u32>) {
        while let Some(s) = stack.pop() {
            if !set[s as usize] {
                set[s as usize] = true;
                stack.extend_from_slice(&self.states[s as usize].eps);
            }
        }
    }
}

/// The characters Lucene's case-insensitive `RegExp` matching accepts for `c` (itself first):
/// `CaseFolding.expand`, i.e. Java's simple upper and lower case mappings plus special variants
/// (`k` also matches KELVIN SIGN, `ß` matches `ẞ`, `σ` matches `ς`, ...). Ranges are not folded.
pub fn regexp_case_variants(c: u32) -> Vec<u32> {
    use super::casefold::{SIMPLE_LOWER, SIMPLE_UPPER, SPECIAL};
    let mut out = vec![c];
    let mut push = |x: u32| {
        if !out.contains(&x) {
            out.push(x);
        }
    };
    if let Ok(i) = SIMPLE_UPPER.binary_search_by_key(&c, |e| e.0) {
        push(SIMPLE_UPPER[i].1);
    }
    if let Ok(i) = SIMPLE_LOWER.binary_search_by_key(&c, |e| e.0) {
        push(SIMPLE_LOWER[i].1);
    }
    let from = SPECIAL.partition_point(|e| e.0 < c);
    for e in SPECIAL[from..].iter().take_while(|e| e.0 == c) {
        push(e.1);
    }
    out
}

/// The characters Elasticsearch's case-insensitive `wildcard`, `prefix` and `term` queries
/// accept for `c` (`AutomatonQueries.toCaseInsensitiveChar`): only code points up to 128 are
/// folded, to their other ASCII case.
pub fn ascii_case_variants(c: u32) -> Vec<u32> {
    match char::from_u32(c) {
        Some(x) if x.is_ascii_lowercase() => vec![c, c - 32],
        Some(x) if x.is_ascii_uppercase() => vec![c, c + 32],
        _ => vec![c],
    }
}
