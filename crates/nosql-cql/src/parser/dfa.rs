//! Prediction tables. The walk itself (`Parser::predict`) is a port of ANTLR 3's
//! `org.antlr.runtime.DFA.predict`.
//!
//! Cassandra's grammar has 25 decisions that ANTLR compiles to tables (the rest are inline
//! `switch`es, which the parser writes out by hand). Running the same tables makes the parser
//! choose the same alternatives, look ahead as far, and fail at the same token as Cassandra's,
//! which is what its error messages (position and snippet) depend on.

use std::sync::OnceLock;

/// A decision's tables, packed as ANTLR packs them: (count, value) pairs of `u16`, where
/// `0xFFFF` is -1.
pub(crate) struct Dfa {
    /// The decision number in the generated parser (for debugging).
    #[allow(dead_code)]
    pub decision: u16,
    pub eot: &'static [u16],
    pub eof: &'static [u16],
    pub min: &'static [u16],
    pub max: &'static [u16],
    pub accept: &'static [u16],
    pub special: &'static [u16],
    pub transition: &'static [&'static [u16]],
    /// Special states: (special index, synpred number, state if it holds, state otherwise).
    pub specials: &'static [(u16, u16, u16, u16)],
    pub cache: OnceLock<Tables>,
}

pub(crate) struct Tables {
    pub eot: Vec<i32>,
    pub eof: Vec<i32>,
    pub min: Vec<u32>,
    pub max: Vec<u32>,
    pub accept: Vec<i32>,
    pub special: Vec<i32>,
    pub transition: Vec<Vec<i32>>,
}

fn unpack(packed: &[u16]) -> Vec<i32> {
    let mut out = Vec::new();
    for pair in packed.chunks_exact(2) {
        let v = if pair[1] == u16::MAX { -1 } else { pair[1] as i32 };
        out.extend(std::iter::repeat_n(v, pair[0] as usize));
    }
    out
}

fn unpack_chars(packed: &[u16]) -> Vec<u32> {
    let mut out = Vec::new();
    for pair in packed.chunks_exact(2) {
        out.extend(std::iter::repeat_n(pair[1] as u32, pair[0] as usize));
    }
    out
}

impl Dfa {
    pub fn tables(&self) -> &Tables {
        self.cache.get_or_init(|| Tables {
            eot: unpack(self.eot),
            eof: unpack(self.eof),
            min: unpack_chars(self.min),
            max: unpack_chars(self.max),
            accept: unpack(self.accept),
            special: unpack(self.special),
            transition: self.transition.iter().map(|t| unpack(t)).collect(),
        })
    }

    pub fn special(&self, index: i32) -> Option<(u16, u16, u16)> {
        self.specials.iter().find(|(i, ..)| *i as i32 == index).map(|&(_, p, y, n)| (p, y, n))
    }
}

/// The symbol a DFA reads for end of input: Java's `(char) Token.EOF`.
pub(crate) const EOF_SYMBOL: u32 = 0xFFFF;
