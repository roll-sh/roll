//! Matching: the literal fast path of `regexec.c` (`tre_match_literal_opt`)
//! and a tag-free port of `tre_tnfa_run_parallel()`.

use super::compile::{LitMode, LitOpt, Nfa};
use super::ctype::{self, is_word};
use super::parse::{
    ASSERT_AT_BOL, ASSERT_AT_BOW, ASSERT_AT_EOL, ASSERT_AT_EOW, ASSERT_AT_WB, ASSERT_AT_WB_NEG,
};

/// `TRE_MAX_STRING`: the parallel matcher looks at no more than this.
const MAX_STRING: usize = i32::MAX as usize;

fn fold(b: u8) -> u8 {
    ctype::to_lower(b as i32) as u8
}

fn eq(hay: &[u8], lit: &[u8], nocase: bool) -> bool {
    if nocase {
        hay.iter().zip(lit).all(|(&h, &l)| fold(h) == l)
    } else {
        hay == lit
    }
}

/// Substring search (KMP), folding the haystack when `nocase` (the literal
/// is already folded).
fn contains(hay: &[u8], lit: &[u8], nocase: bool) -> bool {
    if lit.len() > hay.len() {
        return false;
    }
    let mut fail = vec![0usize; lit.len()];
    let mut k = 0;
    for i in 1..lit.len() {
        while k > 0 && lit[i] != lit[k] {
            k = fail[k - 1];
        }
        if lit[i] == lit[k] {
            k += 1;
        }
        fail[i] = k;
    }
    let mut k = 0;
    for &h in hay {
        let h = if nocase { fold(h) } else { h };
        while k > 0 && h != lit[k] {
            k = fail[k - 1];
        }
        if h == lit[k] {
            k += 1;
            if k == lit.len() {
                return true;
            }
        }
    }
    false
}

/// `tre_match_literal_opt()` with no `eflags`. The anchors are the real
/// ends of the haystack here.
pub(crate) fn literal_match(opt: &LitOpt, hay: &[u8]) -> bool {
    let nocase = opt.nocase;
    opt.literals.iter().any(|lit| match opt.mode {
        LitMode::Contains => contains(hay, lit, nocase),
        LitMode::Prefix => hay.len() >= lit.len() && eq(&hay[..lit.len()], lit, nocase),
        LitMode::Suffix => hay.len() >= lit.len() && eq(&hay[hay.len() - lit.len()..], lit, nocase),
        LitMode::Exact => hay.len() == lit.len() && eq(hay, lit, nocase),
    })
}

/// `CHECK_ASSERTIONS()` without `REG_NOTBOL`/`REG_NOTEOL`/`REG_NEWLINE`:
/// true when an assertion in `a` fails. `next` is 0 both at the end of the
/// string and before a NUL byte, so `$` and the word assertions treat an
/// embedded NUL as the end, exactly as TRE does.
fn fails(a: u32, pos: usize, prev: i32, next: i32) -> bool {
    (a & ASSERT_AT_BOL != 0 && pos > 0)
        || (a & ASSERT_AT_EOL != 0 && next != 0)
        || (a & ASSERT_AT_BOW != 0 && (is_word(prev) || !is_word(next)))
        || (a & ASSERT_AT_EOW != 0 && (!is_word(prev) || is_word(next)))
        || (a & ASSERT_AT_WB != 0 && pos != 0 && next != 0 && is_word(prev) == is_word(next))
        || (a & ASSERT_AT_WB_NEG != 0 && (pos == 0 || next == 0 || is_word(prev) != is_word(next)))
}

/// `tre_tnfa_run_parallel()` with no tags: an unanchored breadth-first
/// simulation that stops at the first time the final state is reached.
/// O(len * transitions), no backtracking.
pub(crate) fn nfa_match(nfa: &Nfa, s: &[u8]) -> bool {
    let len = s.len().min(MAX_STRING);
    let byte = |i: usize| if i < len { s[i] as i32 } else { 0 };
    let fin = nfa.final_state;
    // `reach_pos`: the position at which a state was last added.
    let mut mark = vec![usize::MAX; nfa.states.len()];
    let mut reach: Vec<u32> = Vec::new();
    let mut next: Vec<u32> = Vec::new();
    let mut pos = 0usize;
    let mut prev_c = 0i32;
    let mut next_c = byte(0);

    loop {
        if next.is_empty() && !nfa.initial_has_final {
            // Nothing in flight and no empty match possible: no match can
            // start before the next byte an initial state accepts.
            match (pos..len).find(|&p| nfa.first_bytes[s[p] as usize]) {
                Some(p) => {
                    if p != pos {
                        pos = p;
                        prev_c = s[p - 1] as i32;
                        next_c = s[p] as i32;
                    }
                }
                None => return false,
            }
        }

        for &(t, a) in &nfa.initial {
            if mark[t as usize] == pos || (a != 0 && fails(a, pos, prev_c, next_c)) {
                continue;
            }
            if t == fin {
                return true;
            }
            mark[t as usize] = pos;
            next.push(t);
        }

        if pos >= len {
            return false;
        }
        prev_c = next_c;
        pos += 1;
        next_c = byte(pos);

        std::mem::swap(&mut reach, &mut next);
        next.clear();
        for &st in &reach {
            let st = &nfa.states[st as usize];
            if st.lo > prev_c || st.hi < prev_c {
                continue;
            }
            for &(t, a) in &st.next {
                if mark[t as usize] == pos || (a != 0 && fails(a, pos, prev_c, next_c)) {
                    continue;
                }
                if t == fin {
                    return true;
                }
                mark[t as usize] = pos;
                next.push(t);
            }
        }
    }
}
