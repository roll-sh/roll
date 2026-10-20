//! The regex executor: a port of regexec.c and rege_dfa.c (lazy DFAs over the compacted NFAs,
//! with backtracking dissection of the subexpression tree for captures and back-references).
//!
//! Positions are indexes into the chr array; -1 plays the part of a NULL pointer (it compares
//! below every real position, as NULL does in the C code's pointer comparisons).

use super::comp::{Compiled, NOSR, SHORTER, latype_is_ahead, latype_is_pos};
use super::nfa::{CNFA_NOPROGRESS, COLORLESS, Cnfa, HASLACONS, MATCHALL, RAINBOW};
use super::*;

const NULLP: isize = -1;

// sset flags
const STARTER: i32 = 0o1;
const POSTSTATE: i32 = 0o2;
const LOCKED: i32 = 0o4;
const NOPROGRESS: i32 = 0o10;

const UBITS: usize = 32;
/// The size of the preallocated `smalldfa` (rege_dfa.c).
const FEWSTATES: usize = 20;
const FEWCOLORS: usize = 15;

#[derive(Clone, Copy, Debug)]
struct Arcp {
    ss: i32,
    co: Color,
}

const NOARCP: Arcp = Arcp { ss: -1, co: 0 };

#[derive(Clone, Copy, Debug)]
struct Sset {
    hash: u32,
    flags: i32,
    ins: Arcp,
    lastseen: isize,
}

/// A lazy DFA (`struct dfa`).
struct Dfa<'a> {
    nssets: usize,
    nssused: usize,
    nstates: usize,
    ncolors: usize,
    wordsper: usize,
    ssets: Vec<Sset>,
    statesarea: Vec<u32>,
    work: usize,
    outsarea: Vec<i32>,
    incarea: Vec<Arcp>,
    cnfa: &'a Cnfa,
    lastpost: isize,
    lastnopr: isize,
    search: usize,
    backno: i32,
    backmin: i32,
    backmax: i32,
}

/// `newdfa`.
fn newdfa(cnfa: &Cnfa) -> R<Box<Dfa<'_>>> {
    let nstates = cnfa.nstates as usize;
    let nss = nstates * 2;
    let ncolors = cnfa.ncolors as usize;
    let wordsper = nstates.div_ceil(UBITS);
    if wordsper >= i32::MAX as usize / (nss + 1) || ncolors >= i32::MAX as usize / nss {
        return Err(REG_ETOOBIG);
    }
    // A DFA beyond the preallocated small one is MALLOCed piecewise, in this order, with
    // Postgres's (64-bit) struct sizes: sset 56 bytes, unsigned 4, sset pointer 8, arcp 16.
    if nss > FEWSTATES || ncolors > FEWCOLORS {
        palloc(nss * 56)?;
        palloc((nss + 1) * wordsper * 4)?;
        palloc(nss * ncolors * 8)?;
        palloc(nss * ncolors * 16)?;
    }
    Ok(Box::new(Dfa {
        nssets: nss,
        nssused: 0,
        nstates,
        ncolors,
        wordsper,
        ssets: vec![
            Sset {
                hash: 0,
                flags: 0,
                ins: NOARCP,
                lastseen: NULLP,
            };
            nss
        ],
        statesarea: vec![0; (nss + 1) * wordsper],
        work: nss * wordsper,
        outsarea: vec![-1; nss * ncolors],
        incarea: vec![NOARCP; nss * ncolors],
        cnfa,
        lastpost: NULLP,
        lastnopr: NULLP,
        search: 0,
        backno: -1,
        backmin: 0,
        backmax: 0,
    }))
}

impl Dfa<'_> {
    fn isbset(&self, base: usize, i: usize) -> bool {
        self.statesarea[base + i / UBITS] & (1u32 << (i % UBITS)) != 0
    }
    fn bset(&mut self, base: usize, i: usize) {
        self.statesarea[base + i / UBITS] |= 1u32 << (i % UBITS);
    }
    fn states(&self, ss: usize) -> usize {
        ss * self.wordsper
    }
    fn hash(&self, base: usize) -> u32 {
        let mut h = 0;
        for i in 0..self.wordsper {
            h ^= self.statesarea[base + i];
        }
        h
    }
    fn outs(&self, ss: usize, co: usize) -> isize {
        self.outsarea[ss * self.ncolors + co] as isize
    }
}

/// The executor's state (regexec.c's `struct vars`).
struct Exec<'a> {
    g: &'a Compiled,
    data: &'a [Chr],
    eflags: i32,
    nmatch: usize,
    pmatch: Vec<(isize, isize)>,
    search_start: isize,
    stop: isize,
    err: i32,
    subdfas: Vec<Option<Box<Dfa<'a>>>>,
    ladfas: Vec<Option<Box<Dfa<'a>>>>,
    lblastcss: Vec<isize>,
    lblastcp: Vec<isize>,
}

/// `pg_regexec`: a match of `re` in `data` starting the search at `search_start`. Returns the
/// match's `nmatch` subexpression locations (-1 where one didn't take part), None for no match.
pub fn regexec(
    g: &Compiled,
    data: &[Chr],
    search_start: usize,
    nmatch: usize,
) -> R<Option<Vec<(isize, isize)>>> {
    if search_start > data.len() {
        return Ok(None);
    }
    if g.info & REG_UIMPOSSIBLE != 0 {
        return Ok(None);
    }
    let backref = g.info & REG_UBACKREF != 0;
    let mut v = Exec {
        g,
        data,
        eflags: 0,
        nmatch: 0,
        pmatch: Vec::new(),
        search_start: search_start as isize,
        stop: data.len() as isize,
        err: 0,
        subdfas: (0..g.ntree.max(0)).map(|_| None).collect(),
        ladfas: (0..g.lacons.len()).map(|_| None).collect(),
        lblastcss: vec![NULLP; g.lacons.len()],
        lblastcp: vec![NULLP; g.lacons.len()],
    };
    if backref && nmatch <= g.nsub {
        v.nmatch = g.nsub + 1;
    } else {
        v.nmatch = nmatch.min(g.nsub + 1);
    }
    v.pmatch = vec![(-1, -1); v.nmatch.max(1)];
    let st = if backref { v.cfind() } else { v.find() };
    match st {
        REG_OKAY => {
            let mut out: Vec<(isize, isize)> = vec![(-1, -1); nmatch];
            let n = nmatch.min(v.nmatch);
            out[..n].copy_from_slice(&v.pmatch[..n]);
            if g.cflags & REG_NOSUB != 0 {
                for m in out.iter_mut().skip(1) {
                    *m = (-1, -1);
                }
            }
            Ok(Some(out))
        }
        REG_NOMATCH => Ok(None),
        e => Err(e),
    }
}

impl<'a> Exec<'a> {
    fn iserr(&self) -> bool {
        self.err != 0
    }
    fn seterr(&mut self, e: i32) {
        if self.err == 0 {
            self.err = e;
        }
    }
    fn getcolor(&self, cp: isize) -> Color {
        self.g.cm.getcolor(self.data[cp as usize])
    }
    fn compare(&self, a: isize, b: isize, len: usize) -> bool {
        let x = &self.data[a as usize..a as usize + len];
        let y = &self.data[b as usize..b as usize + len];
        if self.g.cflags & REG_ICASE != 0 {
            let s = self.g.cm.strategy;
            x.iter().zip(y).all(|(&p, &q)| p == q || s.tolower(p) == s.tolower(q))
        } else {
            x == y
        }
    }

    /// Run `f` on the DFA of tree node `t` (`getsubdfa`), creating it on first use.
    fn with_subdfa<T>(&mut self, t: usize, f: impl FnOnce(&mut Self, &mut Dfa<'a>) -> T) -> Option<T> {
        let g: &'a Compiled = self.g;
        let node = &g.tree[t];
        let id = node.id as usize;
        let mut d = match self.subdfas[id].take() {
            Some(d) => d,
            None => match newdfa(&node.cnfa) {
                Ok(mut d) => {
                    if node.op == b'b' {
                        d.backno = node.backno;
                        d.backmin = node.min;
                        d.backmax = node.max;
                    }
                    d
                }
                Err(e) => {
                    self.seterr(e);
                    return None;
                }
            },
        };
        let r = f(self, &mut d);
        self.subdfas[id] = Some(d);
        Some(r)
    }

    fn sub_longest(&mut self, t: usize, start: isize, stop: isize) -> isize {
        self.with_subdfa(t, |v, d| v.longest(d, start, stop).0)
            .unwrap_or(NULLP)
    }

    fn sub_shortest(&mut self, t: usize, start: isize, min: isize, max: isize) -> isize {
        self.with_subdfa(t, |v, d| v.shortest(d, start, min, max, false).0)
            .unwrap_or(NULLP)
    }

    /// `find`: a match for the main NFA (no back-references).
    fn find(&mut self) -> i32 {
        let g = self.g;
        let shorter = g.tree[g.root].flags & SHORTER != 0;
        let mut s = match newdfa(&g.search) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let (close, cold, _) = self.shortest(&mut s, self.search_start, self.search_start, self.stop, true);
        drop(s);
        if self.iserr() {
            return self.err;
        }
        if close == NULLP {
            return REG_NOMATCH;
        }
        if self.nmatch == 0 {
            return REG_OKAY;
        }
        let open = cold;
        let mut d = match newdfa(&g.tree[g.root].cnfa) {
            Ok(d) => d,
            Err(e) => return e,
        };
        let mut begin = open;
        let mut end = NULLP;
        while begin <= close {
            end = if shorter {
                self.shortest(&mut d, begin, begin, self.stop, false).0
            } else {
                self.longest(&mut d, begin, self.stop).0
            };
            if self.iserr() {
                return self.err;
            }
            if end != NULLP {
                break;
            }
            begin += 1;
        }
        drop(d);
        if end == NULLP {
            return REG_NOMATCH;
        }
        self.pmatch[0] = (begin, end);
        if self.nmatch == 1 {
            return REG_OKAY;
        }
        self.cdissect(g.root, begin, end)
    }

    /// `cfind`: a match for the main NFA (with complications).
    fn cfind(&mut self) -> i32 {
        let g = self.g;
        let mut s = match newdfa(&g.search) {
            Ok(s) => s,
            Err(e) => return e,
        };
        let mut d = match newdfa(&g.tree[g.root].cnfa) {
            Ok(d) => d,
            Err(e) => return e,
        };
        let ret = self.cfindloop(&mut d, &mut s);
        if self.iserr() {
            return self.err;
        }
        ret
    }

    /// `cfindloop`.
    fn cfindloop(&mut self, d: &mut Dfa<'a>, s: &mut Dfa<'a>) -> i32 {
        let g = self.g;
        let shorter = g.tree[g.root].flags & SHORTER != 0;
        let mut close = self.search_start;
        loop {
            let (c, cold, _) = self.shortest(s, close, close, self.stop, true);
            close = c;
            if self.iserr() {
                return self.err;
            }
            if close == NULLP {
                break;
            }
            let open = cold;
            let mut begin = open;
            while begin <= close {
                let mut estart = begin;
                let mut estop = self.stop;
                loop {
                    let end = if shorter {
                        self.shortest(d, begin, estart, estop, false).0
                    } else {
                        self.longest(d, begin, estop).0
                    };
                    if self.iserr() {
                        return self.err;
                    }
                    if end == NULLP {
                        break;
                    }
                    let er = self.cdissect(g.root, begin, end);
                    if er == REG_OKAY {
                        if self.nmatch > 0 {
                            self.pmatch[0] = (begin, end);
                        }
                        return REG_OKAY;
                    }
                    if er != REG_NOMATCH {
                        self.seterr(er);
                        return er;
                    }
                    if shorter {
                        if end == estop {
                            break;
                        }
                        estart = end + 1;
                    } else {
                        if end == begin {
                            break;
                        }
                        estop = end - 1;
                    }
                }
                begin += 1;
            }
            close += 1;
            if close >= self.stop {
                break;
            }
        }
        REG_NOMATCH
    }

    /// `zaptreesubs`.
    fn zaptreesubs(&mut self, t: usize) {
        let n = self.g.tree[t].capno;
        if n > 0 && (n as usize) < self.nmatch {
            self.pmatch[n as usize] = (-1, -1);
        }
        let mut t2 = self.g.tree[t].child;
        while t2 != NOSR {
            self.zaptreesubs(t2);
            t2 = self.g.tree[t2].sibling;
        }
    }

    /// `subset`.
    fn subset(&mut self, t: usize, begin: isize, end: isize) {
        let n = self.g.tree[t].capno as usize;
        if n >= self.nmatch {
            return;
        }
        self.pmatch[n] = (begin, end);
    }

    /// `cdissect`: check back-references and find subexpression matches.
    fn cdissect(&mut self, t: usize, begin: isize, end: isize) -> i32 {
        let _f = super::Frame::enter(super::frame::CDISSECT);
        if let Err(e) = super::interrupt() {
            return e;
        }
        if too_deep() {
            return REG_ETOOBIG;
        }
        let g: &'a Compiled = self.g;
        let node = &g.tree[t];
        let er = match node.op {
            b'=' => REG_OKAY,
            b'b' => self.cbrdissect(t, begin, end),
            b'.' => {
                if g.tree[node.child].flags & SHORTER != 0 {
                    self.crevcondissect(t, begin, end)
                } else {
                    self.ccondissect(t, begin, end)
                }
            }
            b'|' => self.caltdissect(t, begin, end),
            b'*' => {
                if g.tree[node.child].flags & SHORTER != 0 {
                    self.creviterdissect(t, begin, end)
                } else {
                    self.citerdissect(t, begin, end)
                }
            }
            b'(' => self.cdissect(node.child, begin, end),
            _ => REG_ASSERT,
        };
        if g.tree[t].capno > 0 && er == REG_OKAY {
            self.subset(t, begin, end);
        }
        er
    }

    /// `ccondissect`: concatenation, longest-first.
    fn ccondissect(&mut self, t: usize, begin: isize, end: isize) -> i32 {
        let left = self.g.tree[t].child;
        let right = self.g.tree[left].sibling;
        let mut mid = self.sub_longest(left, begin, end);
        if self.iserr() {
            return self.err;
        }
        if mid == NULLP {
            return REG_NOMATCH;
        }
        loop {
            if self.sub_longest(right, mid, end) == end {
                let mut er = self.cdissect(left, begin, mid);
                if er == REG_OKAY {
                    er = self.cdissect(right, mid, end);
                    if er == REG_OKAY {
                        return REG_OKAY;
                    }
                    self.zaptreesubs(left);
                }
                if er != REG_NOMATCH {
                    return er;
                }
            }
            if self.iserr() {
                return self.err;
            }
            if mid == begin {
                return REG_NOMATCH;
            }
            mid = self.sub_longest(left, begin, mid - 1);
            if self.iserr() {
                return self.err;
            }
            if mid == NULLP {
                return REG_NOMATCH;
            }
        }
    }

    /// `crevcondissect`: concatenation, shortest-first.
    fn crevcondissect(&mut self, t: usize, begin: isize, end: isize) -> i32 {
        let left = self.g.tree[t].child;
        let right = self.g.tree[left].sibling;
        let mut mid = self.sub_shortest(left, begin, begin, end);
        if self.iserr() {
            return self.err;
        }
        if mid == NULLP {
            return REG_NOMATCH;
        }
        loop {
            if self.sub_longest(right, mid, end) == end {
                let mut er = self.cdissect(left, begin, mid);
                if er == REG_OKAY {
                    er = self.cdissect(right, mid, end);
                    if er == REG_OKAY {
                        return REG_OKAY;
                    }
                    self.zaptreesubs(left);
                }
                if er != REG_NOMATCH {
                    return er;
                }
            }
            if self.iserr() {
                return self.err;
            }
            if mid == end {
                return REG_NOMATCH;
            }
            mid = self.sub_shortest(left, begin, mid + 1, end);
            if self.iserr() {
                return self.err;
            }
            if mid == NULLP {
                return REG_NOMATCH;
            }
        }
    }

    /// `cbrdissect`: a back-reference.
    fn cbrdissect(&mut self, t: usize, begin: isize, end: isize) -> i32 {
        let node = &self.g.tree[t];
        let n = node.backno as usize;
        let (min, max) = (node.min, node.max);
        let (so, eo) = self.pmatch[n];
        if so == -1 {
            return REG_NOMATCH;
        }
        let brlen = (eo - so) as usize;
        if brlen == 0 {
            if begin == end && min <= max {
                return REG_OKAY;
            }
            return REG_NOMATCH;
        }
        if begin == end {
            if min == 0 {
                return REG_OKAY;
            }
            return REG_NOMATCH;
        }
        let tlen = (end - begin) as usize;
        if !tlen.is_multiple_of(brlen) {
            return REG_NOMATCH;
        }
        let mut numreps = tlen / brlen;
        if numreps < min as usize || (numreps > max as usize && max != DUPINF) {
            return REG_NOMATCH;
        }
        let mut p = begin;
        while numreps > 0 {
            if !self.compare(so, p, brlen) {
                return REG_NOMATCH;
            }
            p += brlen as isize;
            numreps -= 1;
        }
        REG_OKAY
    }

    /// `caltdissect`: alternation.
    fn caltdissect(&mut self, t: usize, begin: isize, end: isize) -> i32 {
        let mut t = self.g.tree[t].child;
        while t != NOSR {
            if self.sub_longest(t, begin, end) == end {
                let er = self.cdissect(t, begin, end);
                if er != REG_NOMATCH {
                    return er;
                }
            }
            if self.iserr() {
                return self.err;
            }
            t = self.g.tree[t].sibling;
        }
        REG_NOMATCH
    }

    /// `citerdissect`: iteration, longest-first.
    fn citerdissect(&mut self, t: usize, begin: isize, end: isize) -> i32 {
        let node = &self.g.tree[t];
        let child = node.child;
        let (tmin, tmax) = (node.min, node.max);
        let min_matches: isize = if tmin <= 0 { 1 } else { tmin as isize };
        let mut max_matches: isize = end - begin;
        if max_matches > tmax as isize && tmax != DUPINF {
            max_matches = tmax as isize;
        }
        if max_matches < min_matches {
            max_matches = min_matches;
        }
        let mut endpts = vec![NULLP; max_matches as usize + 1];
        endpts[0] = begin;

        let mut nverified: isize = 0;
        let mut k: isize = 1;
        let mut limit = end;
        while k > 0 {
            let ku = k as usize;
            endpts[ku] = self.sub_longest(child, endpts[ku - 1], limit);
            if self.iserr() {
                return self.err;
            }
            if endpts[ku] == NULLP {
                k -= 1;
            } else {
                if nverified >= k {
                    nverified = k - 1;
                }
                if endpts[ku] != end {
                    if k >= max_matches {
                        k -= 1;
                    } else if endpts[ku] == endpts[ku - 1]
                        && (k >= min_matches || min_matches - k < end - endpts[ku])
                    {
                        // reject zero-length match unless necessary to achieve min
                    } else {
                        k += 1;
                        limit = end;
                        continue;
                    }
                } else if k < min_matches {
                    // not enough matches yet
                } else {
                    let mut i = nverified + 1;
                    while i <= k {
                        self.zaptreesubs(child);
                        let er = self.cdissect(child, endpts[i as usize - 1], endpts[i as usize]);
                        if er == REG_OKAY {
                            nverified = i;
                            i += 1;
                            continue;
                        }
                        if er == REG_NOMATCH {
                            break;
                        }
                        return er;
                    }
                    if i > k {
                        return REG_OKAY;
                    }
                    k = i;
                }
            }
            // backtrack: consider shorter versions of the k'th sub-match
            while k > 0 {
                let prev_end = endpts[k as usize - 1];
                if endpts[k as usize] > prev_end {
                    limit = endpts[k as usize] - 1;
                    if limit > prev_end || (k < min_matches && min_matches - k >= end - prev_end) {
                        break;
                    }
                }
                k -= 1;
            }
        }
        if tmin == 0 && begin == end {
            return REG_OKAY;
        }
        REG_NOMATCH
    }

    /// `creviterdissect`: iteration, shortest-first.
    fn creviterdissect(&mut self, t: usize, begin: isize, end: isize) -> i32 {
        let node = &self.g.tree[t];
        let child = node.child;
        let (tmin, tmax) = (node.min, node.max);
        let mut min_matches: isize = tmin as isize;
        if min_matches <= 0 {
            if begin == end {
                return REG_OKAY;
            }
            min_matches = 1;
        }
        let mut max_matches: isize = end - begin;
        if max_matches > tmax as isize && tmax != DUPINF {
            max_matches = tmax as isize;
        }
        if max_matches < min_matches {
            max_matches = min_matches;
        }
        let mut endpts = vec![NULLP; max_matches as usize + 1];
        endpts[0] = begin;

        let mut nverified: isize = 0;
        let mut k: isize = 1;
        let mut limit = begin;
        while k > 0 {
            let ku = k as usize;
            if limit == endpts[ku - 1]
                && limit != end
                && (k >= min_matches || min_matches - k < end - limit)
            {
                limit += 1;
            }
            if k >= max_matches {
                limit = end;
            }
            endpts[ku] = self.sub_shortest(child, endpts[ku - 1], limit, end);
            if self.iserr() {
                return self.err;
            }
            if endpts[ku] == NULLP {
                k -= 1;
            } else {
                if nverified >= k {
                    nverified = k - 1;
                }
                if endpts[ku] != end {
                    if k >= max_matches {
                        k -= 1;
                    } else {
                        k += 1;
                        limit = endpts[k as usize - 1];
                        continue;
                    }
                } else if k < min_matches {
                    // not enough matches yet
                } else {
                    let mut i = nverified + 1;
                    while i <= k {
                        self.zaptreesubs(child);
                        let er = self.cdissect(child, endpts[i as usize - 1], endpts[i as usize]);
                        if er == REG_OKAY {
                            nverified = i;
                            i += 1;
                            continue;
                        }
                        if er == REG_NOMATCH {
                            break;
                        }
                        return er;
                    }
                    if i > k {
                        return REG_OKAY;
                    }
                    k = i;
                }
            }
            // backtrack: consider longer versions of the k'th sub-match
            while k > 0 {
                if endpts[k as usize] < end {
                    limit = endpts[k as usize] + 1;
                    break;
                }
                k -= 1;
            }
        }
        REG_NOMATCH
    }

    // ── rege_dfa.c ──

    /// `longest`: the longest match from `start` ending at or before `stop`; with whether the
    /// scan hit the end of the string.
    fn longest(&mut self, d: &mut Dfa<'a>, start: isize, stop: isize) -> (isize, bool) {
        let _f = super::Frame::enter(super::frame::LONGEST);
        let mut hitstop = false;
        let realstop = if stop == self.stop { stop } else { stop + 1 };

        if d.backno >= 0 && self.pmatch[d.backno as usize].0 >= 0 {
            let cp = self.dfa_backref(d, start, start, stop, false);
            if cp == self.stop && stop == self.stop {
                hitstop = true;
            }
            return (cp, hitstop);
        }

        if d.cnfa.flags & MATCHALL != 0 {
            let nchr = stop - start;
            let maxmatchall = d.cnfa.maxmatchall as isize;
            if nchr < d.cnfa.minmatchall as isize {
                return (NULLP, false);
            }
            if maxmatchall == DUPINF as isize {
                if stop == self.stop {
                    hitstop = true;
                }
            } else {
                if stop == self.stop && nchr <= maxmatchall + 1 {
                    hitstop = true;
                }
                if nchr > maxmatchall {
                    return (start + maxmatchall, hitstop);
                }
            }
            return (stop, hitstop);
        }

        let mut css = self.initialize(d, start);
        if css < 0 {
            return (NULLP, hitstop);
        }
        let mut cp = start;
        let co = if cp == 0 {
            d.cnfa.bos[if self.eflags & REG_NOTBOL != 0 { 0 } else { 1 }]
        } else {
            self.getcolor(cp - 1)
        };
        css = self.miss(d, css as usize, co, cp, start);
        if css < 0 {
            return (NULLP, hitstop);
        }
        d.ssets[css as usize].lastseen = cp;

        while cp < realstop {
            let co = self.getcolor(cp);
            let mut ss = d.outs(css as usize, co as usize);
            if ss < 0 {
                ss = self.miss(d, css as usize, co, cp + 1, start);
                if ss < 0 {
                    break;
                }
            }
            cp += 1;
            d.ssets[ss as usize].lastseen = cp;
            css = ss;
        }
        if self.iserr() {
            return (NULLP, hitstop);
        }

        if cp == self.stop && stop == self.stop {
            hitstop = true;
            let co = d.cnfa.eos[if self.eflags & REG_NOTEOL != 0 { 0 } else { 1 }];
            let ss = self.miss(d, css as usize, co, cp, start);
            if self.iserr() {
                return (NULLP, hitstop);
            }
            if ss >= 0 && d.ssets[ss as usize].flags & POSTSTATE != 0 {
                return (cp, hitstop);
            } else if ss >= 0 {
                d.ssets[ss as usize].lastseen = cp;
            }
        }

        let mut post = d.lastpost;
        for i in 0..d.nssused {
            let ss = &d.ssets[i];
            if ss.flags & POSTSTATE != 0
                && post != ss.lastseen
                && (post == NULLP || post < ss.lastseen)
            {
                post = ss.lastseen;
            }
        }
        if post != NULLP {
            return (post - 1, hitstop);
        }
        (NULLP, hitstop)
    }

    /// `shortest`: the shortest match from `start` ending between `min` and `max`; with the
    /// coldstart point (when `want_cold`) and whether the end of the string was hit.
    fn shortest(
        &mut self,
        d: &mut Dfa<'a>,
        start: isize,
        min: isize,
        max: isize,
        want_cold: bool,
    ) -> (isize, isize, bool) {
        let _f = super::Frame::enter(super::frame::SHORTEST);
        let mut min = min;
        let mut cold = NULLP;
        let mut hitstop = false;
        let realmin = if min == self.stop { min } else { min + 1 };
        let realmax = if max == self.stop { max } else { max + 1 };

        if d.backno >= 0 && self.pmatch[d.backno as usize].0 >= 0 {
            let cp = self.dfa_backref(d, start, min, max, true);
            if cp != NULLP && want_cold {
                cold = start;
            }
            return (cp, cold, false);
        }

        if d.cnfa.flags & MATCHALL != 0 {
            let nchr = min - start;
            if d.cnfa.maxmatchall != DUPINF && nchr > d.cnfa.maxmatchall as isize {
                return (NULLP, cold, false);
            }
            if max - start < d.cnfa.minmatchall as isize {
                return (NULLP, cold, false);
            }
            if nchr < d.cnfa.minmatchall as isize {
                min = start + d.cnfa.minmatchall as isize;
            }
            if want_cold {
                cold = start;
            }
            return (min, cold, false);
        }

        let mut css = self.initialize(d, start);
        if css < 0 {
            return (NULLP, cold, false);
        }
        let mut cp = start;
        let co = if cp == 0 {
            d.cnfa.bos[if self.eflags & REG_NOTBOL != 0 { 0 } else { 1 }]
        } else {
            self.getcolor(cp - 1)
        };
        css = self.miss(d, css as usize, co, cp, start);
        if css < 0 {
            return (NULLP, cold, false);
        }
        d.ssets[css as usize].lastseen = cp;
        let mut ss = css;

        while cp < realmax {
            let co = self.getcolor(cp);
            ss = d.outs(css as usize, co as usize);
            if ss < 0 {
                ss = self.miss(d, css as usize, co, cp + 1, start);
                if ss < 0 {
                    break;
                }
            }
            cp += 1;
            d.ssets[ss as usize].lastseen = cp;
            css = ss;
            if d.ssets[ss as usize].flags & POSTSTATE != 0 && cp >= realmin {
                break;
            }
        }
        if ss < 0 {
            return (NULLP, cold, false);
        }
        if want_cold {
            cold = self.lastcold(d);
        }
        if d.ssets[ss as usize].flags & POSTSTATE != 0 && cp > min {
            cp -= 1;
        } else if cp == self.stop && max == self.stop {
            let co = d.cnfa.eos[if self.eflags & REG_NOTEOL != 0 { 0 } else { 1 }];
            ss = self.miss(d, css as usize, co, cp, start);
            if ss < 0 || d.ssets[ss as usize].flags & POSTSTATE == 0 {
                hitstop = true;
            }
        }
        if ss < 0 || d.ssets[ss as usize].flags & POSTSTATE == 0 {
            return (NULLP, cold, hitstop);
        }
        (cp, cold, hitstop)
    }

    /// `matchuntil`: whether a match of the search-style DFA ends at `probe`, incrementally.
    fn matchuntil(&mut self, d: &mut Dfa<'a>, probe: isize, lastcss: &mut isize, lastcp: &mut isize) -> bool {
        let mut cp = *lastcp;
        let mut css = *lastcss;
        if d.cnfa.flags & MATCHALL != 0 {
            return probe >= d.cnfa.minmatchall as isize;
        }
        if cp == NULLP || cp > probe {
            cp = 0;
            css = self.initialize(d, cp);
            if css < 0 {
                return false;
            }
            let co = d.cnfa.bos[if self.eflags & REG_NOTBOL != 0 { 0 } else { 1 }];
            css = self.miss(d, css as usize, co, cp, 0);
            if css < 0 {
                return false;
            }
            d.ssets[css as usize].lastseen = cp;
        } else if css < 0 {
            return false;
        }
        let mut ss = css;
        while cp < probe {
            let co = self.getcolor(cp);
            ss = d.outs(css as usize, co as usize);
            if ss < 0 {
                ss = self.miss(d, css as usize, co, cp + 1, 0);
                if ss < 0 {
                    break;
                }
            }
            cp += 1;
            d.ssets[ss as usize].lastseen = cp;
            css = ss;
        }
        *lastcss = ss;
        *lastcp = cp;
        if ss < 0 {
            return false;
        }
        if cp < self.stop {
            let co = self.getcolor(cp);
            ss = d.outs(css as usize, co as usize);
            if ss < 0 {
                ss = self.miss(d, css as usize, co, cp + 1, 0);
            }
        } else {
            let co = d.cnfa.eos[if self.eflags & REG_NOTEOL != 0 { 0 } else { 1 }];
            ss = self.miss(d, css as usize, co, cp, 0);
        }
        ss >= 0 && d.ssets[ss as usize].flags & POSTSTATE != 0
    }

    /// `dfa_backref`: the best match length for a known back-referenced string.
    fn dfa_backref(&mut self, d: &Dfa<'a>, start: isize, min: isize, max: isize, shortest: bool) -> isize {
        let n = d.backno as usize;
        let (backmin, backmax) = (d.backmin, d.backmax);
        let (so, eo) = self.pmatch[n];
        if so == -1 {
            return NULLP;
        }
        let brlen = eo - so;
        if brlen == 0 {
            if min == start && backmin <= backmax {
                return start;
            }
            return NULLP;
        }
        let mut minreps = if min <= start { 0 } else { (min - start - 1) / brlen + 1 };
        let mut maxreps = (max - start) / brlen;
        if minreps < backmin as isize {
            minreps = backmin as isize;
        }
        if backmax != DUPINF && maxreps > backmax as isize {
            maxreps = backmax as isize;
        }
        if maxreps < minreps {
            return NULLP;
        }
        if shortest && minreps == 0 {
            return start;
        }
        let mut p = start;
        let mut numreps = 0;
        while numreps < maxreps {
            if !self.compare(so, p, brlen as usize) {
                break;
            }
            p += brlen;
            numreps += 1;
            if shortest && numreps >= minreps {
                break;
            }
        }
        if numreps >= minreps {
            return p;
        }
        NULLP
    }

    /// `lastcold`: the last point at which no progress had been made.
    fn lastcold(&self, d: &Dfa<'a>) -> isize {
        let mut nopr = d.lastnopr;
        if nopr == NULLP {
            nopr = 0;
        }
        for i in 0..d.nssused {
            let ss = &d.ssets[i];
            if ss.flags & NOPROGRESS != 0 && nopr < ss.lastseen {
                nopr = ss.lastseen;
            }
        }
        nopr
    }

    /// `initialize`: the starting state set.
    fn initialize(&mut self, d: &mut Dfa<'a>, start: isize) -> isize {
        let ss = if d.nssused > 0 && d.ssets[0].flags & STARTER != 0 {
            0
        } else {
            let ss = self.getvacant(d, start, start);
            if ss < 0 {
                return NULLP;
            }
            let ss = ss as usize;
            let base = d.states(ss);
            for i in 0..d.wordsper {
                d.statesarea[base + i] = 0;
            }
            let pre = d.cnfa.pre as usize;
            d.bset(base, pre);
            d.ssets[ss].hash = d.hash(base);
            d.ssets[ss].flags = STARTER | LOCKED | NOPROGRESS;
            ss
        };
        for i in 0..d.nssused {
            d.ssets[i].lastseen = NULLP;
        }
        d.ssets[ss].lastseen = start;
        d.lastpost = NULLP;
        d.lastnopr = NULLP;
        ss as isize
    }

    /// `miss`: a state-set cache miss; returns the next state set, or -1.
    fn miss(&mut self, d: &mut Dfa<'a>, css: usize, co: Color, cp: isize, start: isize) -> isize {
        let existing = d.outs(css, co as usize);
        if existing >= 0 {
            return existing;
        }
        let _f = super::Frame::enter(super::frame::MISS);
        if let Err(e) = super::interrupt() {
            self.seterr(e);
            return -1;
        }
        let cnfa = d.cnfa;
        let work = d.work;
        for i in 0..d.wordsper {
            d.statesarea[work + i] = 0;
        }
        let ispseudo = self.g.cm.is_pseudo(co);
        let mut ispost = false;
        let mut noprogress = true;
        let mut gotstate = false;
        let cbase = d.states(css);
        for i in 0..d.nstates {
            if !d.isbset(cbase, i) {
                continue;
            }
            let mut k = cnfa.states[i];
            loop {
                let (aco, to) = cnfa.arcs[k];
                if aco == COLORLESS {
                    break;
                }
                if aco == co || (aco == RAINBOW && !ispseudo) {
                    d.bset(work, to as usize);
                    gotstate = true;
                    if to == cnfa.post {
                        ispost = true;
                    }
                    if cnfa.stflags[to as usize] & CNFA_NOPROGRESS == 0 {
                        noprogress = false;
                    }
                }
                k += 1;
            }
        }
        if !gotstate {
            return NULLP;
        }
        let mut dolacons = cnfa.flags & HASLACONS != 0;
        let mut sawlacons = false;
        while dolacons {
            dolacons = false;
            for i in 0..d.nstates {
                if !d.isbset(work, i) {
                    continue;
                }
                let mut k = cnfa.states[i];
                loop {
                    let (aco, to) = cnfa.arcs[k];
                    k += 1;
                    if aco == COLORLESS {
                        break;
                    }
                    if (aco as i32) < cnfa.ncolors {
                        continue;
                    }
                    if d.isbset(work, to as usize) {
                        continue;
                    }
                    sawlacons = true;
                    if !self.lacon(cnfa, cp, aco) {
                        if self.iserr() {
                            return NULLP;
                        }
                        continue;
                    }
                    if self.iserr() {
                        return NULLP;
                    }
                    d.bset(work, to as usize);
                    dolacons = true;
                    if to == cnfa.post {
                        ispost = true;
                    }
                    if cnfa.stflags[to as usize] & CNFA_NOPROGRESS == 0 {
                        noprogress = false;
                    }
                }
            }
        }
        let h = d.hash(work);

        let mut found = NULLP;
        for i in 0..d.nssused {
            let p = &d.ssets[i];
            if p.hash == h {
                let pb = d.states(i);
                if d.wordsper == 1
                    || d.statesarea[pb..pb + d.wordsper] == d.statesarea[work..work + d.wordsper]
                {
                    found = i as isize;
                    break;
                }
            }
        }
        let p = if found >= 0 {
            found as usize
        } else {
            let p = self.getvacant(d, cp, start);
            if p < 0 {
                return NULLP;
            }
            let p = p as usize;
            let pb = d.states(p);
            for i in 0..d.wordsper {
                d.statesarea[pb + i] = d.statesarea[work + i];
            }
            d.ssets[p].hash = h;
            d.ssets[p].flags = if ispost { POSTSTATE } else { 0 };
            if noprogress {
                d.ssets[p].flags |= NOPROGRESS;
            }
            p
        };
        if !sawlacons {
            let idx = css * d.ncolors + co as usize;
            d.outsarea[idx] = p as i32;
            d.incarea[idx] = d.ssets[p].ins;
            d.ssets[p].ins = Arcp {
                ss: css as i32,
                co,
            };
        }
        p as isize
    }

    /// `lacon`: whether lookaround constraint `co` (a LACON "color") is satisfied at `cp`.
    fn lacon(&mut self, pcnfa: &Cnfa, cp: isize, co: Color) -> bool {
        if too_deep() {
            self.seterr(REG_ETOOBIG);
            return false;
        }
        let n = (co as i32 - pcnfa.ncolors) as usize;
        let g = self.g;
        let sub = &g.lacons[n];
        let mut d = match self.ladfas[n].take() {
            Some(d) => d,
            None => match newdfa(&sub.cnfa) {
                Ok(d) => d,
                Err(e) => {
                    self.seterr(e);
                    return false;
                }
            },
        };
        let satisfied = if latype_is_ahead(sub.latype) {
            let end = self.shortest(&mut d, cp, cp, self.stop, false).0;
            if latype_is_pos(sub.latype) { end != NULLP } else { end == NULLP }
        } else {
            let (mut lcss, mut lcp) = (self.lblastcss[n], self.lblastcp[n]);
            let m = self.matchuntil(&mut d, cp, &mut lcss, &mut lcp);
            self.lblastcss[n] = lcss;
            self.lblastcp[n] = lcp;
            if latype_is_pos(sub.latype) { m } else { !m }
        };
        self.ladfas[n] = Some(d);
        satisfied
    }

    /// `getvacant`: a vacant state set, unlinked from the cache's arcs.
    fn getvacant(&mut self, d: &mut Dfa<'a>, cp: isize, start: isize) -> isize {
        let ss = self.pickss(d, cp, start);
        if ss < 0 {
            return NULLP;
        }
        let ss = ss as usize;
        let nc = d.ncolors;
        // clear out its inarcs, including self-referential ones
        let mut ap = d.ssets[ss].ins;
        while ap.ss >= 0 {
            let p = ap.ss as usize;
            let co = ap.co as usize;
            d.outsarea[p * nc + co] = -1;
            ap = d.incarea[p * nc + co];
            d.incarea[p * nc + co].ss = -1;
        }
        d.ssets[ss].ins.ss = -1;
        // take it off the inarc chains of the ssets reached by its outarcs
        for i in 0..nc {
            let p = d.outsarea[ss * nc + i];
            if p < 0 {
                continue;
            }
            let p = p as usize;
            let pins = d.ssets[p].ins;
            if pins.ss == ss as i32 && pins.co as usize == i {
                d.ssets[p].ins = d.incarea[ss * nc + i];
            } else {
                let mut lastap = NOARCP;
                let mut ap = pins;
                while ap.ss >= 0 && !(ap.ss == ss as i32 && ap.co as usize == i) {
                    lastap = ap;
                    ap = d.incarea[ap.ss as usize * nc + ap.co as usize];
                }
                if lastap.ss >= 0 {
                    d.incarea[lastap.ss as usize * nc + lastap.co as usize] = d.incarea[ss * nc + i];
                }
            }
            d.outsarea[ss * nc + i] = -1;
            d.incarea[ss * nc + i].ss = -1;
        }
        let s = d.ssets[ss];
        if s.flags & POSTSTATE != 0
            && s.lastseen != d.lastpost
            && (d.lastpost == NULLP || d.lastpost < s.lastseen)
        {
            d.lastpost = s.lastseen;
        }
        if s.flags & NOPROGRESS != 0
            && s.lastseen != d.lastnopr
            && (d.lastnopr == NULLP || d.lastnopr < s.lastseen)
        {
            d.lastnopr = s.lastseen;
        }
        ss as isize
    }

    /// `pickss`: the next state set to use.
    fn pickss(&mut self, d: &mut Dfa<'a>, cp: isize, start: isize) -> isize {
        if d.nssused < d.nssets {
            let i = d.nssused;
            d.nssused += 1;
            d.ssets[i] = Sset {
                hash: 0,
                flags: 0,
                ins: Arcp { ss: -1, co: 0 },
                lastseen: NULLP,
            };
            let nc = d.ncolors;
            for k in 0..nc {
                d.outsarea[i * nc + k] = -1;
                d.incarea[i * nc + k].ss = -1;
            }
            return i as isize;
        }
        let third = (d.nssets * 2 / 3) as isize;
        let ancient = if cp - start > third { cp - third } else { start };
        for i in d.search..d.nssets {
            let ss = &d.ssets[i];
            if (ss.lastseen == NULLP || ss.lastseen < ancient) && ss.flags & LOCKED == 0 {
                d.search = i + 1;
                return i as isize;
            }
        }
        for i in 0..d.search.min(d.nssets) {
            let ss = &d.ssets[i];
            if (ss.lastseen == NULLP || ss.lastseen < ancient) && ss.flags & LOCKED == 0 {
                d.search = i + 1;
                return i as isize;
            }
        }
        self.seterr(REG_ASSERT);
        NULLP
    }
}
