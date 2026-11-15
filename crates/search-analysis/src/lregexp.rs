//! Lucene's `RegExp` (the `simple_pattern` and `simple_pattern_split` tokenizers) and the slice
//! of Lucene's automaton library it is built on.
//!
//! The two tokenizers do not only depend on the language a pattern denotes. Lucene 10 builds the
//! automaton with `RegExp.toAutomaton()` and no longer determinizes it, and
//! `SimplePatternTokenizer` rejects one that is not deterministic ("please determinize the
//! incoming automaton first"), while `SimplePatternSplitTokenizer` determinizes it under a work
//! limit. Whether the automaton is deterministic is a property of how each operation builds it
//! (`ab|ac` is not, `a|a` is, because union merges accept states without transitions), so the
//! construction is reproduced operation by operation: the same states, the same transition
//! merging when a state is finished, the same `deterministic` flag (set when a finished state has
//! overlapping ranges, and inherited through `copy`). Even the case where Lucene fails with an
//! `ArrayIndexOutOfBoundsException` (a bounded repetition of the empty language, which leaves an
//! automaton with zero-capacity state arrays) is reproduced.
//!
//! The grammar is Lucene's with its default syntax flags: `|` union, `&` intersection,
//! concatenation, `? * + {n} {n,} {n,m}` repetition, `[...]` classes with ranges and `^`,
//! `\d \D \s \S \w \W`, `.` any character, `"..."` literal strings, `()` grouping (`()` is the
//! empty string), `#` the empty language, `@` any string, `<n-m>` decimal intervals and
//! `<name>` named automata (none exist, so they fail). `~` is a literal (Lucene 10 only
//! complements with a flag Elasticsearch does not set).
//!
//! Matching uses the finished deterministic automaton directly ([`RunAutomaton`]): a binary
//! search over the current state's sorted ranges per character.
//!
//! Unlike Lucene, a pattern cannot exhaust the stack or the heap: the parsed expression is a flat
//! arena evaluated in one pass (`a|a|…` and `a???…` chains are as long as the pattern), group
//! nesting is bounded ([`MAX_NESTING`]), and every automaton operation is bounded by
//! [`MAX_AUTOMATON_SIZE`] before it allocates (`a{100000000}` is a 400 error, not an 8 GB
//! allocation). Elasticsearch dies on such patterns (a `StackOverflowError` or
//! `OutOfMemoryError`), so there is no response to match; both are `illegal_argument_exception`.

use crate::error::{AnalysisError, Result};

const MAX_CP: i32 = 0x10FFFF;

/// Bound on the states plus transitions of any automaton built from a pattern. Lucene has none
/// (`a{100000000}` or a long chain of `a?` exhausts the heap and takes the node down); the bound
/// keeps one pattern's memory at a few tens of MB however large its repetition counts are.
pub(crate) const MAX_AUTOMATON_SIZE: usize = 1_000_000;

/// Bound on the nesting of `(...)` groups. The parser recurses once per level; Elasticsearch
/// overflows its stack (and dies) somewhere under a thousand levels.
pub(crate) const MAX_NESTING: usize = 1000;

fn too_complex() -> AnalysisError {
    illegal(format!(
        "regular expression is too complex: its automaton would exceed {MAX_AUTOMATON_SIZE} states and transitions"
    ))
}

fn check_size(n: usize) -> Result<()> {
    if n > MAX_AUTOMATON_SIZE {
        return Err(too_complex());
    }
    Ok(())
}

fn illegal(msg: impl Into<String>) -> AnalysisError {
    AnalysisError::illegal(msg)
}

// ---------------------------------------------------------------------------------------------
// Automaton

/// Lucene's `Automaton`: states numbered from 0 (the initial state), transitions grouped by
/// source state and, once the state is finished, sorted by (min, max, dest).
#[derive(Clone, Debug)]
pub(crate) struct Automaton {
    /// Per state: (index of its first transition, or -1, and its transition count).
    states: Vec<(i32, i32)>,
    accept: Vec<bool>,
    /// (dest, min, max).
    trans: Vec<(i32, i32, i32)>,
    cur: i32,
    deterministic: bool,
    /// Built by `Builder.finish()` with no states: Lucene's state array then has length 0 and
    /// `RunAutomaton` fails reading state 0.
    zero_capacity: bool,
}

impl Automaton {
    fn new() -> Automaton {
        Automaton {
            states: Vec::new(),
            accept: Vec::new(),
            trans: Vec::new(),
            cur: -1,
            deterministic: true,
            zero_capacity: false,
        }
    }

    pub fn num_states(&self) -> usize {
        self.states.len()
    }

    pub fn num_transitions(&self) -> usize {
        self.trans.len()
    }

    pub fn is_deterministic(&self) -> bool {
        self.deterministic
    }

    /// States plus transitions, the measure [`MAX_AUTOMATON_SIZE`] bounds.
    fn size(&self) -> usize {
        self.states.len() + self.trans.len()
    }

    fn create_state(&mut self) -> i32 {
        self.states.push((-1, 0));
        self.accept.push(false);
        (self.states.len() - 1) as i32
    }

    fn set_accept(&mut self, s: i32, a: bool) {
        self.accept[s as usize] = a;
    }

    fn is_accept(&self, s: i32) -> bool {
        // `BitSet.get` past the end is false.
        self.accept.get(s as usize).copied().unwrap_or(false)
    }

    /// The transitions of a state.
    fn transitions(&self, s: i32) -> &[(i32, i32, i32)] {
        match self.states.get(s as usize) {
            Some(&(off, n)) if off >= 0 => &self.trans[off as usize..(off + n) as usize],
            _ => &[],
        }
    }

    fn add_transition(&mut self, src: i32, dest: i32, min: i32, max: i32) {
        if self.cur != src {
            if self.cur != -1 {
                self.finish_current_state();
            }
            self.cur = src;
            self.states[src as usize].0 = self.trans.len() as i32;
        }
        self.trans.push((dest, min, max));
        self.states[src as usize].1 += 1;
    }

    /// Adds the transitions of `dest` to `src` (and its accept bit).
    fn add_epsilon(&mut self, src: i32, dest: i32) {
        let ts: Vec<(i32, i32, i32)> = self.transitions(dest).to_vec();
        for (d, min, max) in ts {
            self.add_transition(src, d, min, max);
        }
        if self.is_accept(dest) {
            self.set_accept(src, true);
        }
    }

    /// Appends all states and transitions of `other` (renumbered); inherits nondeterminism.
    fn copy(&mut self, other: &Automaton) {
        let state_offset = self.states.len() as i32;
        let trans_offset = self.trans.len() as i32;
        for &(off, n) in &other.states {
            self.states
                .push((if off == -1 { -1 } else { off + trans_offset }, n));
        }
        self.accept.extend_from_slice(&other.accept);
        self.accept.resize(self.states.len(), false);
        self.trans.extend(
            other
                .trans
                .iter()
                .map(|&(d, min, max)| (d + state_offset, min, max)),
        );
        if !other.deterministic {
            self.deterministic = false;
        }
    }

    fn finish_current_state(&mut self) {
        let (off, n) = self.states[self.cur as usize];
        let (off, n) = (off as usize, n as usize);
        let slice = &mut self.trans[off..off + n];
        slice.sort_by_key(|&(d, min, max)| (d, min, max));
        // Merge adjacent or overlapping ranges to the same destination.
        let mut out: Vec<(i32, i32, i32)> = Vec::with_capacity(n);
        for &(d, min, max) in slice.iter() {
            match out.last_mut() {
                Some(last) if last.0 == d && min <= last.2 + 1 => {
                    if max > last.2 {
                        last.2 = max;
                    }
                }
                _ => out.push((d, min, max)),
            }
        }
        out.sort_by_key(|&(d, min, max)| (min, max, d));
        if self.deterministic && out.len() > 1 {
            let mut last_max = out[0].2;
            for &(_, min, max) in &out[1..] {
                if min <= last_max {
                    self.deterministic = false;
                    break;
                }
                last_max = max;
            }
        }
        self.trans.truncate(off);
        self.states[self.cur as usize].1 = out.len() as i32;
        self.trans.extend(out);
    }

    fn finish_state(&mut self) {
        if self.cur != -1 {
            self.finish_current_state();
            self.cur = -1;
        }
    }

    fn accept_count(&self) -> usize {
        self.accept.iter().filter(|a| **a).count()
    }
}

/// Lucene's `Automaton.Builder`: transitions in any order, sorted when finished; no
/// determinism tracking of its own.
struct Builder {
    trans: Vec<(i32, i32, i32, i32)>,
    accept: Vec<bool>,
}

impl Builder {
    fn new() -> Builder {
        Builder {
            trans: Vec::new(),
            accept: Vec::new(),
        }
    }

    fn create_state(&mut self) -> i32 {
        self.accept.push(false);
        (self.accept.len() - 1) as i32
    }

    fn num_states(&self) -> i32 {
        self.accept.len() as i32
    }

    fn set_accept(&mut self, s: i32, a: bool) {
        self.accept[s as usize] = a;
    }

    fn add_transition(&mut self, src: i32, dest: i32, min: i32, max: i32) {
        self.trans.push((src, dest, min, max));
    }

    /// `add_epsilon(src, offset)` where `offset` is the initial state of a copy of `a` made by
    /// [`Builder::copy`]: the same transitions, without scanning every transition added so far
    /// (which makes `a{0,n}` quadratic in Lucene).
    fn add_epsilon_to_copy(&mut self, src: i32, offset: i32, a: &Automaton) {
        for &(d, min, max) in a.transitions(0) {
            self.add_transition(src, offset + d, min, max);
        }
        if a.is_accept(0) {
            self.set_accept(src, true);
        }
    }

    fn copy(&mut self, other: &Automaton) {
        let offset = self.num_states();
        for s in 0..other.num_states() as i32 {
            let n = self.create_state();
            self.set_accept(n, other.is_accept(s));
        }
        for s in 0..other.num_states() as i32 {
            for &(d, min, max) in other.transitions(s) {
                self.add_transition(offset + s, offset + d, min, max);
            }
        }
    }

    fn finish(mut self) -> Automaton {
        let mut a = Automaton::new();
        let n = self.accept.len();
        for s in 0..n {
            a.create_state();
            a.set_accept(s as i32, self.accept[s]);
        }
        self.trans.sort_by_key(|&(s, d, min, max)| (s, min, max, d));
        for &(s, d, min, max) in &self.trans {
            a.add_transition(s, d, min, max);
        }
        a.finish_state();
        a.zero_capacity = n == 0;
        a
    }
}

// ---------------------------------------------------------------------------------------------
// Automata (factories)

fn make_empty() -> Automaton {
    let mut a = Automaton::new();
    a.finish_state();
    a
}

fn make_empty_string() -> Automaton {
    let mut a = Automaton::new();
    a.create_state();
    a.set_accept(0, true);
    a
}

fn make_any_string() -> Automaton {
    let mut a = Automaton::new();
    let s = a.create_state();
    a.set_accept(s, true);
    a.add_transition(s, s, 0, MAX_CP);
    a.finish_state();
    a
}

fn make_char_range(min: i32, max: i32) -> Automaton {
    if min > max {
        return make_empty();
    }
    let mut a = Automaton::new();
    let s1 = a.create_state();
    let s2 = a.create_state();
    a.set_accept(s2, true);
    a.add_transition(s1, s2, min, max);
    a.finish_state();
    a
}

fn make_char_class(starts: &[i32], ends: &[i32]) -> Automaton {
    if starts.is_empty() {
        return make_empty();
    }
    let mut a = Automaton::new();
    let s1 = a.create_state();
    let s2 = a.create_state();
    a.set_accept(s2, true);
    for (&lo, &hi) in starts.iter().zip(ends) {
        a.add_transition(s1, s2, lo, hi);
    }
    a.finish_state();
    a
}

fn make_string(s: &[i32]) -> Automaton {
    let mut a = Automaton::new();
    let mut last = a.create_state();
    for &cp in s {
        let state = a.create_state();
        a.add_transition(last, state, cp, cp);
        last = state;
    }
    a.set_accept(last, true);
    a.finish_state();
    a
}

/// `Automata.makeDecimalInterval`.
fn make_decimal_interval(min: i32, max: i32, digits: i32) -> Result<Automaton> {
    let (x, y) = (min.to_string(), max.to_string());
    if min > max || (digits > 0 && y.len() as i32 > digits) {
        return Err(AnalysisError::other(
            "illegal_argument_exception",
            400,
            "null",
        ));
    }
    let d = if digits > 0 { digits as usize } else { y.len() };
    let x: Vec<u8> = format!("{x:0>d$}").into_bytes();
    let y: Vec<u8> = format!("{y:0>d$}").into_bytes();
    let mut b = Builder::new();
    if digits <= 0 {
        b.create_state();
    }
    let mut initials = Vec::new();
    between(&mut b, &x, &y, 0, &mut initials, digits <= 0);
    let mut a1 = b.finish();
    if digits <= 0 {
        a1.add_transition(0, 0, b'0' as i32, b'0' as i32);
        for p in initials {
            a1.add_epsilon(0, p);
        }
        a1.finish_state();
    }
    Ok(remove_dead_states(a1))
}

fn any_of_right_length(b: &mut Builder, x: &[u8], n: usize) -> i32 {
    let s = b.create_state();
    if x.len() == n {
        b.set_accept(s, true);
    } else {
        let next = any_of_right_length(b, x, n + 1);
        b.add_transition(s, next, '0' as i32, '9' as i32);
    }
    s
}

fn at_least(b: &mut Builder, x: &[u8], n: usize, initials: &mut Vec<i32>, zeros: bool) -> i32 {
    let s = b.create_state();
    if x.len() == n {
        b.set_accept(s, true);
    } else {
        if zeros {
            initials.push(s);
        }
        let c = x[n] as i32;
        let next = at_least(b, x, n + 1, initials, zeros && c == '0' as i32);
        b.add_transition(s, next, c, c);
        if c < '9' as i32 {
            let any = any_of_right_length(b, x, n + 1);
            b.add_transition(s, any, c + 1, '9' as i32);
        }
    }
    s
}

fn at_most(b: &mut Builder, x: &[u8], n: usize) -> i32 {
    let s = b.create_state();
    if x.len() == n {
        b.set_accept(s, true);
    } else {
        let c = x[n] as i32;
        let next = at_most(b, x, n + 1);
        b.add_transition(s, next, c, c);
        if c > '0' as i32 {
            let any = any_of_right_length(b, x, n + 1);
            b.add_transition(s, any, '0' as i32, c - 1);
        }
    }
    s
}

fn between(
    b: &mut Builder,
    x: &[u8],
    y: &[u8],
    n: usize,
    initials: &mut Vec<i32>,
    zeros: bool,
) -> i32 {
    let s = b.create_state();
    if x.len() == n {
        b.set_accept(s, true);
    } else {
        if zeros {
            initials.push(s);
        }
        let (cx, cy) = (x[n] as i32, y[n] as i32);
        if cx == cy {
            let next = between(b, x, y, n + 1, initials, zeros && cx == '0' as i32);
            b.add_transition(s, next, cx, cx);
        } else {
            let lo = at_least(b, x, n + 1, initials, zeros && cx == '0' as i32);
            b.add_transition(s, lo, cx, cx);
            let hi = at_most(b, y, n + 1);
            b.add_transition(s, hi, cy, cy);
            if cx + 1 < cy {
                let any = any_of_right_length(b, x, n + 1);
                b.add_transition(s, any, cx + 1, cy - 1);
            }
        }
    }
    s
}

// ---------------------------------------------------------------------------------------------
// Operations

fn concatenate(list: &[Automaton]) -> Result<Automaton> {
    let mut result = Automaton::new();
    check_size(list.iter().map(Automaton::size).sum())?;
    for a in list {
        if a.num_states() == 0 {
            return Ok(make_empty());
        }
        for _ in 0..a.num_states() {
            result.create_state();
        }
    }
    let mut state_offset = 0i32;
    for (i, a) in list.iter().enumerate() {
        let n = a.num_states() as i32;
        let next_a = list.get(i + 1);
        for s in 0..n {
            for &(d, min, max) in a.transitions(s) {
                result.add_transition(state_offset + s, state_offset + d, min, max);
            }
            if a.is_accept(s) {
                let mut follow = next_a;
                let mut follow_offset = state_offset;
                let mut upto = i + 1;
                loop {
                    match follow {
                        Some(f) => {
                            for &(d, min, max) in f.transitions(0) {
                                result.add_transition(
                                    state_offset + s,
                                    follow_offset + n + d,
                                    min,
                                    max,
                                );
                            }
                            if f.is_accept(0) {
                                follow_offset += f.num_states() as i32;
                                follow = if upto == list.len() - 1 {
                                    None
                                } else {
                                    list.get(upto + 1)
                                };
                                upto += 1;
                            } else {
                                break;
                            }
                        }
                        None => {
                            result.set_accept(state_offset + s, true);
                            break;
                        }
                    }
                }
                // A run of optional parts (`a?a?a?…`) gives every accept state the transitions
                // of all the parts that follow: quadratic, so checked as it grows.
                check_size(result.size())?;
            }
        }
        state_offset += n;
    }
    if result.num_states() == 0 {
        result.create_state();
    }
    result.finish_state();
    Ok(remove_dead_states(result))
}

fn union(list: &[Automaton]) -> Result<Automaton> {
    check_size(list.iter().map(Automaton::size).sum())?;
    let mut result = Automaton::new();
    result.create_state();
    for a in list {
        result.copy(a);
    }
    let mut offset = 1i32;
    for a in list {
        if a.num_states() == 0 {
            continue;
        }
        result.add_epsilon(0, offset);
        offset += a.num_states() as i32;
    }
    result.finish_state();
    Ok(merge_accept_states_with_no_transition(remove_dead_states(result)))
}

fn optional(a: Automaton) -> Automaton {
    if a.is_accept(0) {
        return a;
    }
    let to_initial = (0..a.num_states() as i32).any(|s| a.transitions(s).iter().any(|t| t.0 == 0));
    if !to_initial {
        let mut result = Automaton::new();
        result.copy(&a);
        if result.num_states() == 0 {
            result.create_state();
        }
        result.set_accept(0, true);
        return result;
    }
    let mut result = Automaton::new();
    result.create_state();
    result.set_accept(0, true);
    if a.num_states() > 0 {
        result.copy(&a);
        result.add_epsilon(0, 1);
    }
    result.finish_state();
    result
}

fn repeat(a: Automaton) -> Automaton {
    if a.num_states() == 0 {
        return a;
    }
    if a.is_accept(0) && a.accept_count() == 1 {
        return a;
    }
    let mut b = Builder::new();
    b.create_state();
    b.set_accept(0, true);
    let n = a.num_states();
    let mut map = vec![0i32; n];
    for s in 0..n as i32 {
        if !a.is_accept(s) {
            map[s as usize] = b.create_state();
        } else if a.transitions(s).is_empty() {
            map[s as usize] = 0;
        } else {
            let ns = b.create_state();
            map[s as usize] = ns;
            b.set_accept(ns, true);
        }
    }
    for s in 0..n as i32 {
        let src = map[s as usize];
        for &(d, min, max) in a.transitions(s) {
            b.add_transition(src, map[d as usize], min, max);
        }
    }
    for &(d, min, max) in a.transitions(0) {
        b.add_transition(0, map[d as usize], min, max);
    }
    for s in 0..n as i32 {
        if a.is_accept(s) && map[s as usize] != 0 {
            for &(d, min, max) in a.transitions(0) {
                b.add_transition(map[s as usize], map[d as usize], min, max);
            }
        }
    }
    remove_dead_states(b.finish())
}

fn repeat_min(a: Automaton, count: u32) -> Result<Automaton> {
    if count == 0 {
        return Ok(repeat(a));
    }
    // Checked before cloning: the count comes from the pattern.
    check_size(a.size().saturating_mul(count as usize + 1))?;
    let mut list: Vec<Automaton> = (0..count).map(|_| a.clone()).collect();
    list.push(repeat(a));
    concatenate(&list)
}

fn repeat_min_max(a: Automaton, min: u32, max: u32) -> Result<Automaton> {
    if min > max {
        return Ok(make_empty());
    }
    // Each optional copy adds `a` plus the epsilon transitions from the previous accept states.
    let per_copy = a.size() + a.accept_count() * a.transitions(0).len();
    check_size(per_copy.saturating_mul(max as usize))?;
    let b = if min == 0 {
        make_empty_string()
    } else if min == 1 {
        let mut b = Automaton::new();
        b.copy(&a);
        b
    } else {
        let list: Vec<Automaton> = (0..min).map(|_| a.clone()).collect();
        concatenate(&list)?
    };
    let mut prev_accept: Vec<i32> = (0..b.num_states() as i32)
        .filter(|&s| b.is_accept(s))
        .collect();
    let mut builder = Builder::new();
    builder.copy(&b);
    for _ in min..max {
        let n = builder.num_states();
        builder.copy(&a);
        for &s in &prev_accept {
            builder.add_epsilon_to_copy(s, n, &a);
        }
        prev_accept = (0..a.num_states() as i32)
            .filter(|&s| a.is_accept(s))
            .map(|s| s + n)
            .collect();
    }
    Ok(remove_dead_states(builder.finish()))
}

fn complement(a: Automaton, work_limit: i64) -> Result<Automaton> {
    let mut a = totalize(&determinize(a, work_limit)?);
    for s in 0..a.num_states() {
        a.accept[s] = !a.accept[s];
    }
    Ok(remove_dead_states(a))
}

fn intersection(a1: Automaton, a2: Automaton) -> Result<Automaton> {
    if a1.num_states() == 0 {
        return Ok(a1);
    }
    if a2.num_states() == 0 {
        return Ok(a2);
    }
    let mut c = Automaton::new();
    c.create_state();
    let mut ids: std::collections::HashMap<(i32, i32), i32> = std::collections::HashMap::new();
    let mut work: std::collections::VecDeque<(i32, i32, i32)> = std::collections::VecDeque::new();
    work.push_back((0, 0, 0));
    ids.insert((0, 0), 0);
    while let Some((s1, s2, s)) = work.pop_front() {
        c.set_accept(s, a1.is_accept(s1) && a2.is_accept(s2));
        let (t1, t2) = (a1.transitions(s1), a2.transitions(s2));
        let mut b2 = 0;
        for &(d1, min1, max1) in t1 {
            while b2 < t2.len() && t2[b2].2 < min1 {
                b2 += 1;
            }
            let mut n2 = b2;
            while n2 < t2.len() && max1 >= t2[n2].1 {
                let (d2, min2, max2) = t2[n2];
                if max2 >= min1 {
                    let r = match ids.get(&(d1, d2)) {
                        Some(&r) => r,
                        None => {
                            let r = c.create_state();
                            ids.insert((d1, d2), r);
                            work.push_back((d1, d2, r));
                            r
                        }
                    };
                    c.add_transition(s, r, min1.max(min2), max1.min(max2));
                }
                n2 += 1;
            }
        }
        // The product can have |a1| * |a2| states.
        check_size(c.size())?;
    }
    c.finish_state();
    Ok(remove_dead_states(c))
}

/// `Operations.determinize` (subset construction; the effort is the total size of the state
/// sets visited, bounded by ten times the work limit).
fn determinize(a: Automaton, work_limit: i64) -> Result<Automaton> {
    if a.is_deterministic() || a.num_states() <= 1 {
        return Ok(a);
    }
    let mut b = Builder::new();
    b.create_state();
    b.set_accept(0, a.is_accept(0));
    let mut ids: std::collections::HashMap<Vec<i32>, i32> = std::collections::HashMap::new();
    let mut work: std::collections::VecDeque<(Vec<i32>, i32)> = std::collections::VecDeque::new();
    ids.insert(vec![0], 0);
    work.push_back((vec![0], 0));
    let limit = work_limit.saturating_mul(10);
    let mut effort: i64 = 0;
    let mut points: Vec<i32> = Vec::new();
    while let Some((set, r)) = work.pop_front() {
        effort += set.len() as i64;
        if effort >= limit {
            return Err(AnalysisError::other(
                "too_complex_to_determinize_exception",
                400,
                format!(
                    "Determinizing automaton with {} states and {} transitions would require more than {} effort.",
                    a.num_states(),
                    a.num_transitions(),
                    work_limit
                ),
            ));
        }
        points.clear();
        for &s in &set {
            for &(_, min, max) in a.transitions(s) {
                points.push(min);
                points.push(max + 1);
            }
        }
        if points.is_empty() {
            continue;
        }
        points.sort_unstable();
        points.dedup();
        for w in 0..points.len() {
            let lo = points[w];
            let Some(&next) = points.get(w + 1) else {
                break;
            };
            let hi = next - 1;
            let mut dest: Vec<i32> = Vec::new();
            for &s in &set {
                for &(d, min, max) in a.transitions(s) {
                    if min <= lo && hi <= max {
                        dest.push(d);
                    }
                }
            }
            if dest.is_empty() {
                continue;
            }
            dest.sort_unstable();
            dest.dedup();
            let q = match ids.get(&dest) {
                Some(&q) => q,
                None => {
                    let q = b.create_state();
                    b.set_accept(q, dest.iter().any(|&d| a.is_accept(d)));
                    ids.insert(dest.clone(), q);
                    work.push_back((dest, q));
                    q
                }
            };
            b.add_transition(r, q, lo, hi);
        }
        check_size(b.accept.len() + b.trans.len())?;
    }
    Ok(b.finish())
}

fn totalize(a: &Automaton) -> Automaton {
    let mut result = Automaton::new();
    let n = a.num_states() as i32;
    for s in 0..n {
        result.create_state();
        result.set_accept(s, a.is_accept(s));
    }
    let dead = result.create_state();
    result.add_transition(dead, dead, 0, MAX_CP);
    for s in 0..n {
        let mut maxi = 0;
        for &(d, min, max) in a.transitions(s) {
            result.add_transition(s, d, min, max);
            if min > maxi {
                result.add_transition(s, dead, maxi, min - 1);
            }
            if max + 1 > maxi {
                maxi = max + 1;
            }
        }
        if maxi <= MAX_CP {
            result.add_transition(s, dead, maxi, MAX_CP);
        }
    }
    result.finish_state();
    result
}

/// States reachable from the initial state and from which an accept state is reachable.
fn live_states(a: &Automaton) -> Vec<bool> {
    let n = a.num_states();
    let mut from = vec![false; n];
    if n > 0 {
        let mut stack = vec![0i32];
        from[0] = true;
        while let Some(s) = stack.pop() {
            for &(d, _, _) in a.transitions(s) {
                if !from[d as usize] {
                    from[d as usize] = true;
                    stack.push(d);
                }
            }
        }
    }
    let mut rev: Vec<Vec<i32>> = vec![Vec::new(); n];
    for s in 0..n as i32 {
        for &(d, _, _) in a.transitions(s) {
            rev[d as usize].push(s);
        }
    }
    let mut to = vec![false; n];
    let mut stack: Vec<i32> = (0..n as i32).filter(|&s| a.is_accept(s)).collect();
    for &s in &stack {
        to[s as usize] = true;
    }
    while let Some(s) = stack.pop() {
        for &p in &rev[s as usize] {
            if !to[p as usize] {
                to[p as usize] = true;
                stack.push(p);
            }
        }
    }
    (0..n).map(|s| from[s] && to[s]).collect()
}

fn remove_dead_states(a: Automaton) -> Automaton {
    let live = live_states(&a);
    if live.iter().all(|l| *l) {
        return a;
    }
    let mut map = vec![0i32; a.num_states()];
    let mut result = Automaton::new();
    for (s, &l) in live.iter().enumerate() {
        if l {
            map[s] = result.create_state();
            result.set_accept(map[s], a.is_accept(s as i32));
        }
    }
    for (s, &l) in live.iter().enumerate() {
        if l {
            for &(d, min, max) in a.transitions(s as i32) {
                if live[d as usize] {
                    result.add_transition(map[s], map[d as usize], min, max);
                }
            }
        }
    }
    result.finish_state();
    result
}

fn merge_accept_states_with_no_transition(a: Automaton) -> Automaton {
    let n = a.num_states() as i32;
    let merged: Vec<i32> = (0..n)
        .filter(|&s| a.is_accept(s) && a.transitions(s).is_empty())
        .collect();
    if merged.len() <= 1 {
        return a;
    }
    let remap = |s: i32| -> i32 {
        match merged.binary_search(&s) {
            Ok(_) => merged[0],
            Err(idx) if idx <= 1 => s,
            Err(idx) => s - (idx as i32 - 1),
        }
    };
    let mut result = Automaton::new();
    for s in 0..n {
        let r = remap(s);
        while result.num_states() as i32 <= r {
            result.create_state();
        }
        if a.is_accept(s) {
            result.set_accept(r, true);
        }
    }
    for s in 0..n {
        let src = remap(s);
        for &(d, min, max) in a.transitions(s) {
            result.add_transition(src, remap(d), min, max);
        }
    }
    result.finish_state();
    result
}

// ---------------------------------------------------------------------------------------------
// RegExp

/// A node of the parsed expression. Nodes live in one arena ([`Parser::nodes`]) and refer to
/// their operands by index, and an operand is always created before the node using it, so the
/// arena order is a post-order of the tree: [`to_automaton`] evaluates it in one forward pass,
/// and dropping it is flat. (A tree of boxes would recurse once per level to build, convert and
/// drop it, and `a|a|…|a` or `a????…` chains are as deep as they are long.)
#[derive(Debug)]
enum Exp {
    Union(u32, u32),
    Concat(u32, u32),
    Inter(u32, u32),
    Optional(u32),
    Repeat(u32),
    RepeatMin(u32, u32),
    RepeatMinMax(u32, u32, u32),
    Complement(u32),
    Char(i32),
    CharRange(i32, i32),
    CharClass(Vec<i32>, Vec<i32>),
    AnyChar,
    Empty,
    Str(Vec<i32>),
    AnyString,
    Named(String),
    Interval(i32, i32, i32),
    /// Text merged into another node by [`Parser::make_concat`].
    Merged,
}

fn is_text(e: &Exp) -> bool {
    matches!(e, Exp::Char(_) | Exp::Str(_))
}

fn take_text(e: &mut Exp) -> Vec<i32> {
    match std::mem::replace(e, Exp::Merged) {
        Exp::Char(c) => vec![c],
        Exp::Str(s) => s,
        _ => Vec::new(),
    }
}

fn check_range(from: i32, to: i32) -> Result<()> {
    if from > to {
        return Err(illegal(format!(
            "invalid range: from ({from}) cannot be > to ({to})"
        )));
    }
    Ok(())
}

struct Parser {
    s: Vec<char>,
    /// UTF-16 offset of each character (and of the end), for error positions.
    u16: Vec<usize>,
    pos: usize,
    nodes: Vec<Exp>,
    /// Current `(` nesting.
    depth: usize,
}

type Id = u32;

impl Parser {
    fn push(&mut self, e: Exp) -> Id {
        self.nodes.push(e);
        (self.nodes.len() - 1) as Id
    }

    /// `RegExp.makeConcatenation`: adjacent characters and strings become one string.
    fn make_concat(&mut self, e1: Id, e2: Id) -> Id {
        let (i1, i2) = (e1 as usize, e2 as usize);
        if is_text(&self.nodes[i1]) && is_text(&self.nodes[i2]) {
            let mut s = take_text(&mut self.nodes[i1]);
            s.extend(take_text(&mut self.nodes[i2]));
            self.nodes[i1] = Exp::Str(s);
            return e1;
        }
        if let Exp::Concat(_, b) = self.nodes[i1]
            && is_text(&self.nodes[b as usize])
            && is_text(&self.nodes[i2])
        {
            let mut s = take_text(&mut self.nodes[b as usize]);
            s.extend(take_text(&mut self.nodes[i2]));
            self.nodes[b as usize] = Exp::Str(s);
            return e1;
        }
        if let Exp::Concat(a, _) = self.nodes[i2]
            && is_text(&self.nodes[i1])
            && is_text(&self.nodes[a as usize])
        {
            let mut s = take_text(&mut self.nodes[i1]);
            s.extend(take_text(&mut self.nodes[a as usize]));
            self.nodes[a as usize] = Exp::Str(s);
            return e2;
        }
        self.push(Exp::Concat(e1, e2))
    }

    fn at(&self) -> usize {
        self.u16[self.pos]
    }
    fn more(&self) -> bool {
        self.pos < self.s.len()
    }
    fn peek(&self, chars: &str) -> bool {
        self.more() && chars.contains(self.s[self.pos])
    }
    fn matches(&mut self, c: char) -> bool {
        if self.more() && self.s[self.pos] == c {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn next(&mut self) -> Result<i32> {
        if !self.more() {
            return Err(illegal("unexpected end-of-string"));
        }
        self.pos += 1;
        Ok(self.s[self.pos - 1] as i32)
    }

    fn union(&mut self) -> Result<Id> {
        let mut e = self.inter()?;
        while self.matches('|') {
            let r = self.inter()?;
            e = self.push(Exp::Union(e, r));
        }
        Ok(e)
    }

    fn inter(&mut self) -> Result<Id> {
        let mut e = self.concat()?;
        while self.matches('&') {
            let r = self.concat()?;
            e = self.push(Exp::Inter(e, r));
        }
        Ok(e)
    }

    fn concat(&mut self) -> Result<Id> {
        let mut e = self.repeat()?;
        while self.more() && !self.peek(")|") && !self.peek("&") {
            let r = self.repeat()?;
            e = self.make_concat(e, r);
        }
        Ok(e)
    }

    /// `Integer.parseInt` of a run of ASCII digits (NumberFormatException on overflow).
    fn int(&self, start: usize) -> Result<u32> {
        let s: String = self.s[start..self.pos].iter().collect();
        s.parse::<i32>().map(|v| v as u32).map_err(|_| {
            AnalysisError::other(
                "number_format_exception",
                400,
                format!("For input string: \"{s}\""),
            )
        })
    }

    fn repeat(&mut self) -> Result<Id> {
        let mut e = self.char_class_exp()?;
        while self.peek("?*+{") {
            if self.matches('?') {
                e = self.push(Exp::Optional(e));
            } else if self.matches('*') {
                e = self.push(Exp::Repeat(e));
            } else if self.matches('+') {
                e = self.push(Exp::RepeatMin(e, 1));
            } else if self.matches('{') {
                let start = self.pos;
                while self.peek("0123456789") {
                    self.pos += 1;
                }
                if start == self.pos {
                    return Err(illegal(format!(
                        "integer expected at position {}",
                        self.at()
                    )));
                }
                let n = self.int(start)?;
                let m = if self.matches(',') {
                    let start = self.pos;
                    while self.peek("0123456789") {
                        self.pos += 1;
                    }
                    if start != self.pos {
                        Some(self.int(start)?)
                    } else {
                        None
                    }
                } else {
                    Some(n)
                };
                if !self.matches('}') {
                    return Err(illegal(format!("expected '}}' at position {}", self.at())));
                }
                if let Some(m) = m
                    && n > m
                {
                    return Err(illegal(format!(
                        "invalid repetition range(out of order): {n}..{m}"
                    )));
                }
                e = self.push(match m {
                    None => Exp::RepeatMin(e, n),
                    Some(m) => Exp::RepeatMinMax(e, n, m),
                });
            }
        }
        Ok(e)
    }

    fn char_class_exp(&mut self) -> Result<Id> {
        if self.matches('[') {
            let negate = self.matches('^');
            let class = self.char_classes()?;
            let mut e = self.push(class);
            if negate {
                // Lucene builds `AnyChar` first; neither side can fail, so the order of
                // evaluation does not show.
                let any = self.push(Exp::AnyChar);
                let c = self.push(Exp::Complement(e));
                e = self.push(Exp::Inter(any, c));
            }
            if !self.matches(']') {
                return Err(illegal(format!("expected ']' at position {}", self.at())));
            }
            return Ok(e);
        }
        self.simple()
    }

    fn predefined(&mut self, starts: &mut Vec<i32>, ends: &mut Vec<i32>) -> Result<()> {
        let c = self.s[self.pos];
        let ranges: &[(i32, i32)] = match c {
            '\\' => &[(92, 92)],
            'd' => &[(48, 57)],
            'D' => &[(0, 47), (58, MAX_CP)],
            's' => &[(9, 10), (13, 13), (32, 32)],
            'S' => &[(0, 8), (11, 12), (14, 31), (33, MAX_CP)],
            'w' => &[(48, 57), (65, 90), (95, 95), (97, 122)],
            'W' => &[(0, 47), (58, 64), (91, 94), (96, 96), (123, MAX_CP)],
            _ => {
                self.pos += 1;
                return Err(illegal(format!("invalid character class \\{c}")));
            }
        };
        for &(lo, hi) in ranges {
            starts.push(lo);
            ends.push(hi);
        }
        self.pos += 1;
        Ok(())
    }

    fn predefined_follows(&self) -> bool {
        self.more() && (self.s[self.pos] == '\\' || self.s[self.pos].is_ascii_alphabetic())
    }

    fn char_classes(&mut self) -> Result<Exp> {
        let (mut starts, mut ends) = (Vec::new(), Vec::new());
        loop {
            if self.matches('\\') {
                if self.predefined_follows() {
                    self.predefined(&mut starts, &mut ends)?;
                } else {
                    let c = self.next()?;
                    starts.push(c);
                    ends.push(c);
                }
            } else {
                let c = self.char_exp()?;
                if self.matches('-') {
                    starts.push(c);
                    ends.push(self.char_exp()?);
                } else {
                    starts.push(c);
                    ends.push(c);
                }
            }
            if !(self.more() && !self.peek("]")) {
                break;
            }
        }
        if starts.len() == 1 {
            if starts[0] == ends[0] {
                return Ok(Exp::Char(starts[0]));
            }
            check_range(starts[0], ends[0])?;
            return Ok(Exp::CharRange(starts[0], ends[0]));
        }
        for (&s, &e) in starts.iter().zip(&ends) {
            check_range(s, e)?;
        }
        Ok(Exp::CharClass(starts, ends))
    }

    fn char_exp(&mut self) -> Result<i32> {
        self.matches('\\');
        self.next()
    }

    /// `parseSimpleExp`.
    fn simple(&mut self) -> Result<Id> {
        if self.matches('(') {
            if self.matches(')') {
                return Ok(self.push(Exp::Str(Vec::new())));
            }
            if self.depth >= MAX_NESTING {
                return Err(illegal(format!(
                    "regular expression is nested too deeply (more than {MAX_NESTING} groups) at position {}",
                    self.u16[self.pos - 1]
                )));
            }
            self.depth += 1;
            let e = self.union()?;
            self.depth -= 1;
            if !self.matches(')') {
                return Err(illegal(format!("expected ')' at position {}", self.at())));
            }
            return Ok(e);
        }
        let e = self.simple_exp()?;
        Ok(self.push(e))
    }

    /// The leaves of `parseSimpleExp`.
    fn simple_exp(&mut self) -> Result<Exp> {
        if self.matches('.') {
            return Ok(Exp::AnyChar);
        }
        if self.matches('#') {
            return Ok(Exp::Empty);
        }
        if self.matches('@') {
            return Ok(Exp::AnyString);
        }
        if self.matches('"') {
            let start = self.pos;
            while self.more() && !self.peek("\"") {
                self.pos += 1;
            }
            if !self.matches('"') {
                return Err(illegal(format!("expected '\"' at position {}", self.at())));
            }
            return Ok(Exp::Str(
                self.s[start..self.pos - 1]
                    .iter()
                    .map(|&c| c as i32)
                    .collect(),
            ));
        }
        if self.matches('<') {
            let start = self.pos;
            while self.more() && !self.peek(">") {
                self.pos += 1;
            }
            if !self.matches('>') {
                return Err(illegal(format!("expected '>' at position {}", self.at())));
            }
            let s: String = self.s[start..self.pos - 1].iter().collect();
            let end = self.u16[self.pos - 1];
            return interval(&s, end);
        }
        // `matchPredefinedCharacterClass` consumes the backslash either way.
        if self.matches('\\') && self.predefined_follows() {
            let (mut starts, mut ends) = (Vec::new(), Vec::new());
            self.predefined(&mut starts, &mut ends)?;
            return Ok(Exp::CharClass(starts, ends));
        }
        Ok(Exp::Char(self.char_exp()?))
    }
}

/// `<min-max>` (or a named automaton without `-`).
fn interval(s: &str, pos: usize) -> Result<Exp> {
    let Some(i) = s.find('-') else {
        return Ok(Exp::Named(s.to_string()));
    };
    let syntax = || illegal(format!("interval syntax error at position {pos}"));
    if i == 0 || i == s.len() - 1 || s.rfind('-') != Some(i) {
        return Err(syntax());
    }
    let (smin, smax) = (&s[..i], &s[i + 1..]);
    let mut min = crate::settings::java_parse_int(smin).ok_or_else(syntax)?;
    let mut max = crate::settings::java_parse_int(smax).ok_or_else(syntax)?;
    let digits = if smin.encode_utf16().count() == smax.encode_utf16().count() {
        smin.encode_utf16().count() as i32
    } else {
        0
    };
    if min > max {
        std::mem::swap(&mut min, &mut max);
    }
    Ok(Exp::Interval(min, max, digits))
}

/// A node's result while [`to_automaton`] runs: `RegExp.findLeaves` collects the operands of
/// nested unions (and of nested concatenations) into one list instead of building each level,
/// so a union or concatenation stays a list until a node of another kind uses it.
enum Val {
    Done,
    Auto(Automaton),
    Union(Vec<Automaton>),
    Concat(Vec<Automaton>),
}

impl Val {
    fn finish(self) -> Result<Automaton> {
        match self {
            Val::Auto(a) => Ok(a),
            Val::Union(list) => union(&list),
            Val::Concat(list) => concatenate(&list),
            // Operands are used once, in arena order.
            Val::Done => Err(illegal("regular expression operand used twice")),
        }
    }

    /// Appends this operand to a union (`union` true) or concatenation list.
    fn leaves_into(self, union: bool, out: &mut Vec<Automaton>) -> Result<()> {
        match (self, union) {
            (Val::Union(list), true) | (Val::Concat(list), false) => out.extend(list),
            (v, _) => out.push(v.finish()?),
        }
        Ok(())
    }
}

/// `RegExp.toAutomaton()` of the expression `root`, in one pass over the arena (operands come
/// before their users, left before right, which is the order Lucene's recursion visits them,
/// so the first failing node is the one Lucene reports).
fn to_automaton(nodes: Vec<Exp>, root: Id) -> Result<Automaton> {
    let mut vals: Vec<Val> = Vec::with_capacity(nodes.len());
    for e in nodes {
        let mut take = |i: Id| std::mem::replace(&mut vals[i as usize], Val::Done);
        let v = match e {
            Exp::Union(a, b) | Exp::Concat(a, b) => {
                let is_union = matches!(e, Exp::Union(..));
                // Chains are left-deep (`a|b|c` is `(a|b)|c`): extend the left operand's list
                // in place, or long chains would copy it once per operator.
                let mut list = match (take(a), is_union) {
                    (Val::Union(list), true) | (Val::Concat(list), false) => list,
                    (v, _) => vec![v.finish()?],
                };
                take(b).leaves_into(is_union, &mut list)?;
                if is_union {
                    Val::Union(list)
                } else {
                    Val::Concat(list)
                }
            }
            Exp::Inter(a, b) => {
                let a = take(a).finish()?;
                let b = take(b).finish()?;
                Val::Auto(intersection(a, b)?)
            }
            Exp::Optional(a) => Val::Auto(optional(take(a).finish()?)),
            Exp::Repeat(a) => Val::Auto(repeat(take(a).finish()?)),
            Exp::RepeatMin(a, n) => Val::Auto(repeat_min(take(a).finish()?, n)?),
            Exp::RepeatMinMax(a, n, m) => Val::Auto(repeat_min_max(take(a).finish()?, n, m)?),
            Exp::Complement(a) => Val::Auto(complement(take(a).finish()?, i32::MAX as i64)?),
            Exp::Char(c) => Val::Auto(make_char_range(c, c)),
            Exp::CharRange(lo, hi) => Val::Auto(make_char_range(lo, hi)),
            Exp::CharClass(starts, ends) => Val::Auto(make_char_class(&starts, &ends)),
            Exp::AnyChar => Val::Auto(make_char_range(0, MAX_CP)),
            Exp::Empty => Val::Auto(make_empty()),
            Exp::Str(s) => Val::Auto(make_string(&s)),
            Exp::AnyString => Val::Auto(make_any_string()),
            Exp::Named(name) => return Err(illegal(format!("'{name}' not found"))),
            Exp::Interval(min, max, digits) => Val::Auto(make_decimal_interval(min, max, digits)?),
            Exp::Merged => Val::Done,
        };
        vals.push(v);
    }
    std::mem::replace(&mut vals[root as usize], Val::Done).finish()
}

/// `new RegExp(pattern).toAutomaton()`.
pub(crate) fn parse(pattern: &str) -> Result<Automaton> {
    // The parser recurses per `(` level.
    crate::stack::deep(crate::stack::opens(pattern, b"("), || parse_here(pattern)).unwrap_or_else(
        || {
            Err(AnalysisError::other(
                "illegal_state_exception",
                500,
                "could not start a thread to compile the pattern",
            ))
        },
    )
}

fn parse_here(pattern: &str) -> Result<Automaton> {
    let s: Vec<char> = pattern.chars().collect();
    let mut u16 = Vec::with_capacity(s.len() + 1);
    let mut off = 0;
    for c in &s {
        u16.push(off);
        off += c.len_utf16();
    }
    u16.push(off);
    let mut p = Parser {
        s,
        u16,
        pos: 0,
        nodes: Vec::new(),
        depth: 0,
    };
    let root = if p.s.is_empty() {
        p.push(Exp::Str(Vec::new()))
    } else {
        let e = p.union()?;
        if p.more() {
            return Err(illegal(format!(
                "end-of-string expected at position {}",
                p.at()
            )));
        }
        e
    };
    to_automaton(p.nodes, root)
}

/// The automaton of `simple_pattern` (`split` false) or `simple_pattern_split`, ready to run.
pub(crate) fn compile(pattern: &str, split: bool) -> Result<RunAutomaton> {
    let mut a = parse(pattern)?;
    if split {
        a = determinize(a, 10_000)?;
    } else if !a.is_deterministic() {
        return Err(illegal("please determinize the incoming automaton first"));
    }
    if a.zero_capacity {
        return Err(AnalysisError::other(
            "array_index_out_of_bounds_exception",
            500,
            "Index 0 out of bounds for length 0",
        ));
    }
    Ok(RunAutomaton::new(&a))
}

/// A finished deterministic automaton laid out for matching. States are numbered from
/// [`RunAutomaton::START`]; 0 is the dead state (the conventions of [`crate::dfa::Dfa`]).
#[derive(Debug)]
pub(crate) struct RunAutomaton {
    /// Transitions of state `s` (numbered from 0): `mins/maxs/dests[first[s]..first[s + 1]]`.
    first: Vec<u32>,
    mins: Vec<u32>,
    maxs: Vec<u32>,
    dests: Vec<u32>,
    accept: Vec<bool>,
    /// `ascii[(state - 1) * 128 + c]`: a direct transition table for ASCII input, built when the
    /// automaton is small enough (most patterns are), so the common case is one load per char.
    ascii: Vec<u32>,
}

/// States beyond which the ASCII table is not built (it would take 512 bytes per state).
const ASCII_TABLE_MAX_STATES: usize = 2048;

impl RunAutomaton {
    pub const START: u32 = 1;
    pub const DEAD: u32 = 0;

    fn new(a: &Automaton) -> RunAutomaton {
        let mut r = RunAutomaton {
            first: vec![0],
            mins: Vec::new(),
            maxs: Vec::new(),
            dests: Vec::new(),
            accept: Vec::new(),
            ascii: Vec::new(),
        };
        for s in 0..a.num_states() as i32 {
            for &(d, min, max) in a.transitions(s) {
                r.mins.push(min as u32);
                r.maxs.push(max as u32);
                r.dests.push(d as u32 + 1);
            }
            r.first.push(r.mins.len() as u32);
            r.accept.push(a.is_accept(s));
        }
        if a.num_states() == 0 {
            r.first.push(0);
            r.accept.push(false);
        }
        let n = r.accept.len();
        if n <= ASCII_TABLE_MAX_STATES {
            let mut ascii = vec![Self::DEAD; n * 128];
            for s in 0..n {
                for c in 0..128u8 {
                    ascii[s * 128 + c as usize] = r.step_slow(s as u32 + 1, c as char);
                }
            }
            r.ascii = ascii;
        }
        r
    }

    #[inline]
    pub fn step(&self, state: u32, c: char) -> u32 {
        if state == Self::DEAD {
            return Self::DEAD;
        }
        if (c as u32) < 128 && !self.ascii.is_empty() {
            return self.ascii[(state as usize - 1) * 128 + c as usize];
        }
        self.step_slow(state, c)
    }

    fn step_slow(&self, state: u32, c: char) -> u32 {
        if state == Self::DEAD {
            return Self::DEAD;
        }
        let s = (state - 1) as usize;
        let (lo, hi) = (self.first[s] as usize, self.first[s + 1] as usize);
        let cp = c as u32;
        let mins = &self.mins[lo..hi];
        // The last transition starting at or before `cp`.
        let i = mins.partition_point(|&m| m <= cp);
        if i == 0 || self.maxs[lo + i - 1] < cp {
            return Self::DEAD;
        }
        self.dests[lo + i - 1]
    }

    #[inline]
    pub fn accepts(&self, state: u32) -> bool {
        state != Self::DEAD && self.accept[(state - 1) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lm(p: &str, s: &str) -> Option<usize> {
        let r = compile(p, true).unwrap();
        let mut state = RunAutomaton::START;
        let mut best = None;
        for (i, c) in s.char_indices() {
            state = r.step(state, c);
            if state == RunAutomaton::DEAD {
                break;
            }
            if r.accepts(state) {
                best = Some(i + c.len_utf8());
            }
        }
        best
    }

    #[test]
    fn syntax() {
        assert_eq!(lm("[a-z]+", "abc1"), Some(3));
        assert_eq!(lm("a|bc", "bcd"), Some(2));
        assert_eq!(lm("\"a.b\"", "a.b"), Some(3));
        assert_eq!(lm("\"a.b\"", "axb"), None);
        assert_eq!(lm("a{2,3}", "aaaa"), Some(3));
        assert_eq!(lm("[^ ]+", "ab cd"), Some(2));
        assert_eq!(lm("\\d+", "123a"), Some(3));
        assert_eq!(lm("\\w+", "ab_9-"), Some(4));
        assert_eq!(lm(".", "😀"), Some(4));
        assert_eq!(lm("a@", "abc"), Some(3));
        assert_eq!(lm("#", "a"), None);
        assert_eq!(lm("<1-12>", "07x"), Some(2));
        assert_eq!(lm("<1-12>", "13"), Some(1));
        assert_eq!(lm("<01-12>", "07"), Some(2));
        assert_eq!(lm("<01-12>", "7"), None);
        assert_eq!(lm("<5-300>", "250"), Some(3));
        assert_eq!(lm("<5-300>", "301"), Some(2));
        assert_eq!(lm("[a-c]&[b-d]", "c"), Some(1));
        assert_eq!(lm("[a-c]&[b-d]", "a"), None);
        assert_eq!(lm("~", "~"), Some(1));
        assert!(compile("[a", true).is_err());
        assert!(compile("a{3,2}", true).is_err());
    }

    /// Runs `f` on a thread with a 2 MB stack, the size of a tokio worker's.
    fn on_small_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
        std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    #[test]
    fn deep_and_long_patterns_fit_a_small_stack() {
        on_small_stack(|| {
            let nest = |n: usize| format!("{}a{}", "(".repeat(n), ")".repeat(n));
            assert_eq!(lm(&nest(MAX_NESTING), "ab"), Some(1));
            let err = compile(&nest(MAX_NESTING + 1), false).unwrap_err();
            assert_eq!(err.kind, "illegal_argument_exception");
            assert_eq!(
                err.reason,
                format!(
                    "regular expression is nested too deeply (more than {MAX_NESTING} groups) at position {MAX_NESTING}"
                )
            );
            assert!(compile(&nest(100_000), true).is_err());
            // Chains are as long as the pattern, not nested: unions, concatenations,
            // repetition operators.
            let alt = format!("{}b", "a|".repeat(50_000));
            assert_eq!(lm(&alt, "b"), Some(1));
            assert!(compile(&alt, false).is_ok());
            assert_eq!(lm(&format!("a{}", "?".repeat(50_000)), "aa"), Some(1));
            assert_eq!(lm(&format!("a{}", "*".repeat(50_000)), "aa"), Some(2));
            assert_eq!(lm(&"a.".repeat(20_000), &"ab".repeat(20_001)), Some(40_000));
            assert_eq!(lm(&format!("{}x", "[ab]".repeat(20_000)), &"ab".repeat(10_000)), None);
        });
    }

    #[test]
    fn repetition_counts_are_bounded() {
        let too_complex = |p: &str| {
            let e = compile(p, false).unwrap_err();
            assert_eq!(e.kind, "illegal_argument_exception", "{p}");
            assert!(e.reason.starts_with("regular expression is too complex"), "{p}: {}", e.reason);
        };
        too_complex("a{100000000}");
        too_complex("a{2000000000}");
        too_complex("a{0,2147483647}");
        too_complex("a{100000000,}");
        too_complex("(a{1000}){1000}");
        too_complex("([a-z]{999}&[a-y]{999}){600}");
        // `a?a?…a?` concatenates into a quadratic number of transitions.
        too_complex(&"a?".repeat(5_000));
        // Large but bounded counts still work, in linear time.
        let t = std::time::Instant::now();
        assert_eq!(lm("a{0,200000}", &"a".repeat(10)), Some(10));
        assert_eq!(lm("a{100000}", &"a".repeat(100_001)), Some(100_000));
        assert!(t.elapsed() < std::time::Duration::from_secs(20));
        // Not deterministic, and too much work to determinize, as in Lucene.
        assert_eq!(
            compile(&"a?".repeat(500), true).unwrap_err().kind,
            "too_complex_to_determinize_exception"
        );
    }

    #[test]
    fn determinism_follows_lucene_construction() {
        let det = |p: &str| parse(p).unwrap().is_deterministic();
        assert!(det("a|b"));
        assert!(det("a|a"));
        assert!(!det("ab|ac"));
        assert!(!det("<1-12>"));
        assert!(det("[ab]|c"));
        assert!(det("(ab)*"));
        assert!(det("a*b*"));
        let err = compile("ab|ac", false).unwrap_err();
        assert_eq!(
            err.reason,
            "please determinize the incoming automaton first"
        );
        assert!(compile("ab|ac", true).is_ok());
        assert_eq!(
            compile("(a&b){2}", true).unwrap_err().kind,
            "array_index_out_of_bounds_exception"
        );
        assert!(compile("(a&b){0,3}", false).is_ok());
        assert_eq!(
            compile("<abc>", false).unwrap_err().reason,
            "'abc' not found"
        );
    }
}
