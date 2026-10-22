//! Code point DFAs: the first stage of every automaton build.
//!
//! Automata are built like Lucene builds them: the code point NFA is determinized over code point
//! intervals first (where Levenshtein, wildcard and regexp automata are small: a handful of
//! intervals per state), and only the resulting DFA is expanded into UTF-8 byte transitions and
//! determinized again over bytes ([`super::dfa`]), which is cheap because the input is already
//! deterministic per code point. Determinizing the byte-expanded NFA directly would multiply
//! every set of NFA states by the UTF-8 continuation states of `.`/`*`/edit transitions.
//!
//! Work is bounded like Lucene's `Operations.determinize`: each DFA state processed costs the
//! number of NFA states in it that a character leads to (Lucene's automata have no epsilon
//! transitions, so our join and fork states do not count), and reaching `10 × limit` fails with
//! [`TooComplex`]. Intersection and complement (regexp `&` and `~`) also run here, on code point
//! DFAs, as in Lucene.

use std::collections::HashMap;

use super::FxBuild;
use super::dfa::TooComplex;
use super::nfa::{MAX_CP, Nfa};

/// A DFA over code points. State 0 is the start; an empty language has no states.
#[derive(Clone, Debug, Default)]
pub(crate) struct CpDfa {
    pub(crate) accept: Vec<bool>,
    /// Per state: sorted, disjoint, inclusive code point ranges and their targets.
    pub(crate) trans: Vec<Vec<(u32, u32, u32)>>,
}

impl CpDfa {
    /// Subset construction over code point intervals under Lucene's work limit. The NFA must not
    /// carry raw byte transitions.
    pub(crate) fn determinize(nfa: &Nfa, work_limit: u32) -> Result<CpDfa, TooComplex> {
        if nfa.states.is_empty() {
            return Ok(CpDfa::default());
        }
        debug_assert!(nfa.states.iter().all(|s| s.bytes.is_empty()));
        let n = nfa.states.len();
        let mut entered = vec![false; n];
        entered[nfa.start as usize] = true;
        for st in &nfa.states {
            for &(_, _, to) in &st.ranges {
                entered[to as usize] = true;
            }
        }
        let too_complex = || TooComplex {
            states: n,
            transitions: nfa.states.iter().map(|s| s.ranges.len()).sum(),
            limit: work_limit,
        };
        let effort_limit = work_limit as u64 * 10;
        let mut effort = 0u64;

        let mut mark = vec![0u32; n];
        let mut stamp = 1u32;
        let mut ids: HashMap<Vec<u32>, u32, FxBuild> = HashMap::default();
        let mut sets: Vec<Vec<u32>> = Vec::new();
        let mut out = CpDfa::default();

        let mut start = vec![nfa.start];
        close(nfa, &mut start, &mut mark, stamp);
        stamp += 1;
        ids.insert(start.clone(), 0);
        sets.push(start);

        let mut ranges: Vec<(u32, u32, u32)> = Vec::new();
        let mut points: Vec<u32> = Vec::new();
        let mut targets: Vec<u32> = Vec::new();
        let mut next = 0usize;
        while next < sets.len() {
            let set = std::mem::take(&mut sets[next]);
            effort += set.iter().filter(|&&s| entered[s as usize]).count() as u64;
            if effort >= effort_limit {
                return Err(too_complex());
            }
            out.accept.push(set.iter().any(|&s| nfa.states[s as usize].accept));
            ranges.clear();
            points.clear();
            for &s in &set {
                for &r in &nfa.states[s as usize].ranges {
                    ranges.push(r);
                    points.push(r.0);
                    points.push(r.1.saturating_add(1));
                }
            }
            points.sort_unstable();
            points.dedup();
            let mut row: Vec<(u32, u32, u32)> = Vec::new();
            for w in points.windows(2) {
                let (lo, hi) = (w[0], w[1] - 1);
                targets.clear();
                for &(a, b, to) in &ranges {
                    if a <= lo && hi <= b && mark[to as usize] != stamp {
                        mark[to as usize] = stamp;
                        targets.push(to);
                    }
                }
                stamp += 1;
                if targets.is_empty() {
                    continue;
                }
                close(nfa, &mut targets, &mut mark, stamp);
                stamp += 1;
                let id = match ids.get(&targets) {
                    Some(&id) => id,
                    None => {
                        let id = sets.len() as u32;
                        ids.insert(targets.clone(), id);
                        sets.push(targets.clone());
                        id
                    }
                };
                match row.last_mut() {
                    Some(last) if last.2 == id && last.1 + 1 == lo => last.1 = hi,
                    _ => row.push((lo, hi, id)),
                }
            }
            out.trans.push(row);
            sets[next] = set;
            next += 1;
        }
        out.remove_dead();
        Ok(out)
    }

    /// Drops states that cannot reach acceptance (and every transition to them); an automaton
    /// whose start is dead becomes the empty one.
    fn remove_dead(&mut self) {
        let n = self.accept.len();
        let mut rev: Vec<Vec<u32>> = vec![Vec::new(); n];
        for (s, row) in self.trans.iter().enumerate() {
            for &(_, _, t) in row {
                rev[t as usize].push(s as u32);
            }
        }
        let mut live = self.accept.clone();
        let mut stack: Vec<u32> = (0..n as u32).filter(|&s| live[s as usize]).collect();
        while let Some(s) = stack.pop() {
            for &p in &rev[s as usize] {
                if !std::mem::replace(&mut live[p as usize], true) {
                    stack.push(p);
                }
            }
        }
        if n == 0 || !live[0] {
            *self = CpDfa::default();
            return;
        }
        if live.iter().all(|&l| l) {
            return;
        }
        // Renumber the live states (keeping 0 first).
        let mut map = vec![u32::MAX; n];
        let mut k = 0u32;
        for (s, &l) in live.iter().enumerate() {
            if l {
                map[s] = k;
                k += 1;
            }
        }
        let mut out = CpDfa::default();
        for s in 0..n {
            if live[s] {
                out.accept.push(self.accept[s]);
                out.trans.push(
                    self.trans[s]
                        .iter()
                        .filter(|r| live[r.2 as usize])
                        .map(|&(a, b, t)| (a, b, map[t as usize]))
                        .collect(),
                );
            }
        }
        *self = out;
    }

    pub(crate) fn is_empty_language(&self) -> bool {
        self.accept.is_empty()
    }

    /// The product automaton (Lucene's `Operations.intersection`).
    pub(crate) fn intersection(&self, other: &CpDfa) -> CpDfa {
        if self.is_empty_language() || other.is_empty_language() {
            return CpDfa::default();
        }
        let mut ids: HashMap<(u32, u32), u32, FxBuild> = HashMap::default();
        let mut pairs = vec![(0u32, 0u32)];
        ids.insert((0, 0), 0);
        let mut out = CpDfa::default();
        let mut i = 0;
        while i < pairs.len() {
            let (x, y) = pairs[i];
            out.accept.push(self.accept[x as usize] && other.accept[y as usize]);
            let (ra, rb) = (&self.trans[x as usize], &other.trans[y as usize]);
            let mut row = Vec::new();
            let (mut p, mut q) = (0, 0);
            while p < ra.len() && q < rb.len() {
                let (a, b) = (ra[p], rb[q]);
                let (lo, hi) = (a.0.max(b.0), a.1.min(b.1));
                if lo <= hi {
                    let id = *ids.entry((a.2, b.2)).or_insert_with(|| {
                        pairs.push((a.2, b.2));
                        (pairs.len() - 1) as u32
                    });
                    row.push((lo, hi, id));
                }
                if a.1 < b.1 {
                    p += 1;
                } else {
                    q += 1;
                }
            }
            out.trans.push(row);
            i += 1;
        }
        out.remove_dead();
        out
    }

    /// Every code point string this automaton rejects (Lucene's `Operations.complement`).
    pub(crate) fn complement(&self) -> CpDfa {
        let n = self.accept.len();
        let sink = n as u32;
        let mut out = CpDfa::default();
        for s in 0..n {
            let mut row = Vec::new();
            let mut next = 0u32;
            for &(a, b, t) in &self.trans[s] {
                if a > next {
                    row.push((next, a - 1, sink));
                }
                row.push((a, b, t));
                next = b.saturating_add(1);
            }
            if next <= MAX_CP {
                row.push((next, MAX_CP, sink));
            }
            out.trans.push(row);
            out.accept.push(!self.accept[s]);
        }
        // The sink (also the start when this automaton is empty).
        out.trans.push(vec![(0, MAX_CP, sink)]);
        out.accept.push(true);
        if n == 0 {
            out = CpDfa { accept: vec![true], trans: vec![vec![(0, MAX_CP, 0)]] };
        }
        out.remove_dead();
        out
    }

    /// Copies this automaton into `nfa`; accepting states get an epsilon to `end`. Returns the
    /// copy's start state, or `None` for the empty language.
    pub(crate) fn embed(&self, nfa: &mut Nfa, end: u32) -> Option<u32> {
        if self.is_empty_language() {
            return None;
        }
        let base = nfa.len() as u32;
        for _ in 0..self.accept.len() {
            nfa.add_state();
        }
        for (s, row) in self.trans.iter().enumerate() {
            let from = base + s as u32;
            if self.accept[s] {
                nfa.add_epsilon(from, end);
            }
            for &(a, b, t) in row {
                nfa.add_range(from, a, b, base + t);
            }
        }
        Some(base)
    }

    /// States from which every string of valid code points is accepted (the greatest fixpoint
    /// of accepting states whose ranges cover every scalar value and lead to such states).
    pub(crate) fn universal(&self) -> Vec<bool> {
        let n = self.accept.len();
        let covers_all = |row: &[(u32, u32, u32)]| {
            // Surrogates never occur in valid UTF-8, so they need no transition.
            let mut next = 0u32;
            for &(a, b, _) in row {
                if a > next && !(next >= 0xD800 && a <= 0xE000) {
                    return false;
                }
                next = next.max(b.saturating_add(1));
                if (0xD800..0xE000).contains(&next) {
                    next = 0xE000;
                }
            }
            next > MAX_CP
        };
        let mut good: Vec<bool> = (0..n).map(|s| self.accept[s] && covers_all(&self.trans[s])).collect();
        loop {
            let mut changed = false;
            for s in 0..n {
                if good[s] && self.trans[s].iter().any(|r| !good[r.2 as usize]) {
                    good[s] = false;
                    changed = true;
                }
            }
            if !changed {
                return good;
            }
        }
    }
}

/// Epsilon closure of `set` (sorted on return); `mark` holds `stamp` for members.
fn close(nfa: &Nfa, set: &mut Vec<u32>, mark: &mut [u32], stamp: u32) {
    for &s in set.iter() {
        mark[s as usize] = stamp;
    }
    let mut i = 0;
    while i < set.len() {
        let s = set[i] as usize;
        for &e in &nfa.states[s].eps {
            if mark[e as usize] != stamp {
                mark[e as usize] = stamp;
                set.push(e);
            }
        }
        i += 1;
    }
    set.sort_unstable();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn run(d: &CpDfa, s: &str) -> bool {
        if d.is_empty_language() {
            return false;
        }
        let mut q = 0u32;
        for c in s.chars() {
            let c = c as u32;
            match d.trans[q as usize].iter().find(|r| r.0 <= c && c <= r.1) {
                Some(r) => q = r.2,
                None => return false,
            }
        }
        d.accept[q as usize]
    }

    fn random_nfa(rng: &mut StdRng) -> Nfa {
        let mut nfa = Nfa::new();
        let n = rng.gen_range(1..8);
        for _ in 0..n {
            nfa.add_state();
        }
        for s in 0..n as u32 {
            for _ in 0..rng.gen_range(0..4) {
                let to = rng.gen_range(0..n as u32);
                match rng.gen_range(0..4) {
                    0 => nfa.add_epsilon(s, to),
                    1 => nfa.add_any(s, to),
                    2 => {
                        let a = rng.gen_range(b'a' as u32..=b'd' as u32);
                        nfa.add_range(s, a, a + rng.gen_range(0..2), to)
                    }
                    _ => nfa.add_char(s, ['a', 'é', '日'][rng.gen_range(0..3)] as u32, to),
                }
            }
            if rng.gen_bool(0.3) {
                nfa.set_accept(s, true);
            }
        }
        nfa.set_start(0);
        nfa
    }

    #[test]
    fn determinize_intersect_complement_agree_with_nfa_simulation() {
        let mut rng = StdRng::seed_from_u64(3);
        let words: Vec<String> = (0..400)
            .map(|_| (0..rng.gen_range(0..5)).map(|_| ['a', 'b', 'c', 'd', 'é', '日', 'x'][rng.gen_range(0..7)]).collect())
            .collect();
        for _ in 0..300 {
            let (n1, n2) = (random_nfa(&mut rng), random_nfa(&mut rng));
            let (d1, d2) = (CpDfa::determinize(&n1, 1 << 20).unwrap(), CpDfa::determinize(&n2, 1 << 20).unwrap());
            let (both, not1) = (d1.intersection(&d2), d1.complement());
            for w in &words {
                let (a, b) = (n1.matches_str(w), n2.matches_str(w));
                assert_eq!(run(&d1, w), a, "{w}");
                assert_eq!(run(&both, w), a && b, "{w}");
                assert_eq!(run(&not1, w), !a, "{w}");
            }
            // Ranges stay sorted and disjoint.
            for row in &d1.trans {
                assert!(row.windows(2).all(|w| w[0].1 < w[1].0));
            }
        }
        assert!(CpDfa::default().complement().accept[0]);
    }
}
