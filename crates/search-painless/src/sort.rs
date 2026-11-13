//! `java.util.TimSort` / `ComparableTimSort`: the stable sort behind `List.sort`,
//! `Collections.sort`, `Arrays.sort(Object[])` and sorted streams.
//!
//! Scripts can observe the exact sequence of comparisons — a comparator with side effects, the
//! first pair of mutually incomparable elements (which decides the `ClassCastException` text),
//! the "Comparison method violates its general contract!" check — so this follows the JDK's
//! algorithm step for step: runs found by `countRunAndMakeAscending`, binary insertion sort for
//! short runs, the merge-collapse invariants, galloping with the adaptive `minGallop`. A failing
//! comparison raised by library code (not inside a script lambda) gets the JDK frames of the
//! comparison site and of every enclosing sort method, innermost first.

use std::cmp::Ordering;

use crate::value::Value;
use crate::vm::{iae, Thrown};

const MIN_MERGE: usize = 32;
const MIN_GALLOP: usize = 7;

/// Which of the JDK's two TimSorts runs: natural ordering (`ComparableTimSort`, line numbers
/// of its `compareTo` sites) or a comparator (`TimSort`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Natural,
    Comparator,
}

/// Source lines of the call sites in JDK 27's `ComparableTimSort.java` / `TimSort.java`.
struct Lines {
    sort_count_mini: u32,
    sort_binary_mini: u32,
    sort_count: u32,
    sort_binary: u32,
    sort_collapse: u32,
    sort_force: u32,
    binary_cmp: u32,
    count_first: u32,
    count_desc: u32,
    count_asc: u32,
    collapse_merge: u32,
    force_merge: u32,
    at_gallop_right: u32,
    at_gallop_left: u32,
    at_lo: u32,
    at_hi: u32,
    gl_hint: u32,
    gl_right: u32,
    gl_left: u32,
    gl_bin: u32,
    gr_hint: u32,
    gr_left: u32,
    gr_right: u32,
    gr_bin: u32,
    lo_cmp: u32,
    lo_gallop_right: u32,
    lo_gallop_left: u32,
    lo_contract: u32,
    hi_cmp: u32,
    hi_gallop_right: u32,
    hi_gallop_left: u32,
    hi_contract: u32,
}

const NATURAL: Lines = Lines {
    sort_count_mini: 188,
    sort_binary_mini: 189,
    sort_count: 202,
    sort_binary: 207,
    sort_collapse: 213,
    sort_force: 222,
    binary_cmp: 262,
    count_first: 320,
    count_desc: 321,
    count_asc: 325,
    collapse_merge: 413,
    force_merge: 426,
    at_gallop_right: 466,
    at_gallop_left: 477,
    at_lo: 485,
    at_hi: 487,
    gl_hint: 513,
    gl_right: 516,
    gl_left: 531,
    gl_bin: 556,
    gr_hint: 583,
    gr_left: 586,
    gr_right: 602,
    gr_bin: 626,
    lo_cmp: 688,
    lo_gallop_right: 710,
    lo_gallop_left: 723,
    lo_contract: 749,
    hi_cmp: 808,
    hi_gallop_right: 830,
    hi_gallop_left: 843,
    hi_contract: 871,
};

const COMPARATOR: Lines = Lines {
    sort_count_mini: 220,
    sort_binary_mini: 221,
    sort_count: 234,
    sort_binary: 239,
    sort_collapse: 245,
    sort_force: 254,
    binary_cmp: 296,
    count_first: 355,
    count_desc: 356,
    count_asc: 360,
    collapse_merge: 448,
    force_merge: 461,
    at_gallop_right: 500,
    at_gallop_left: 511,
    at_lo: 518,
    at_hi: 520,
    gl_hint: 546,
    gl_right: 549,
    gl_left: 564,
    gl_bin: 589,
    gr_hint: 617,
    gr_left: 620,
    gr_right: 636,
    gr_bin: 660,
    lo_cmp: 721,
    lo_gallop_right: 743,
    lo_gallop_left: 756,
    lo_contract: 782,
    hi_cmp: 841,
    hi_gallop_right: 863,
    hi_gallop_left: 876,
    hi_contract: 904,
};

struct Sorter<'c> {
    a: Vec<Value>,
    cmp: &'c mut dyn FnMut(&Value, &Value) -> Result<Ordering, Thrown>,
    flavor: Flavor,
    lines: &'static Lines,
    min_gallop: usize,
    runs: Vec<(usize, usize)>,
    /// Enclosing JDK methods and the line each is executing, outermost first.
    chain: Vec<(&'static str, u32)>,
}

impl Sorter<'_> {
    fn class(&self) -> &'static str {
        match self.flavor {
            Flavor::Natural => "ComparableTimSort",
            Flavor::Comparator => "TimSort",
        }
    }

    fn frames(&self, method: &str, line: u32) -> Vec<String> {
        let c = self.class();
        let mut f = vec![format!("java.base/java.util.{c}.{method}({c}.java:{line})")];
        for (m, l) in self.chain.iter().rev() {
            f.push(format!("java.base/java.util.{c}.{m}({c}.java:{l})"));
        }
        f
    }

    /// One comparison at `method`/`line`; errors raised by library code get the sort's frames.
    fn c(&mut self, x: &Value, y: &Value, method: &str, line: u32) -> Result<Ordering, Thrown> {
        if self.flavor == Flavor::Natural && x.is_null() {
            // the helpful NPE names the receiver expression of the `compareTo` call
            let what = match method {
                "countRunAndMakeAscending" => "a[runHi]",
                "binarySort" => "pivot",
                "mergeLo" => "a[cursor2]",
                "mergeHi" => "tmp[cursor2]",
                _ => "key",
            };
            let mut e = crate::vm::npe(format!("Cannot invoke \"java.lang.Comparable.compareTo(Object)\" because \"{what}\" is null"));
            e.frames = self.frames(method, line);
            return Err(e);
        }
        match (self.cmp)(x, y) {
            Ok(o) => Ok(o),
            Err(mut e) => {
                if e.offset.is_none() {
                    let f = self.frames(method, line);
                    e.frames.extend(f);
                }
                Err(e)
            }
        }
    }

    fn contract_violation(&self, method: &str, line: u32) -> Thrown {
        let mut e = iae("Comparison method violates its general contract!");
        e.frames = self.frames(method, line);
        e
    }

    fn enter(&mut self, method: &'static str, line: u32) {
        self.chain.push((method, line));
    }

    fn leave(&mut self) {
        self.chain.pop();
    }

    fn sort(&mut self) -> Result<(), Thrown> {
        let hi = self.a.len();
        if hi < 2 {
            return Ok(());
        }
        let l = self.lines;
        if hi < MIN_MERGE {
            self.enter("sort", l.sort_count_mini);
            let init = self.count_run(0, hi)?;
            self.leave();
            self.enter("sort", l.sort_binary_mini);
            self.binary_sort(0, hi, init)?;
            self.leave();
            return Ok(());
        }
        let min_run = min_run_length(hi);
        let mut lo = 0;
        let mut remaining = hi;
        while remaining != 0 {
            self.enter("sort", l.sort_count);
            let mut run = self.count_run(lo, hi)?;
            self.leave();
            if run < min_run {
                let force = remaining.min(min_run);
                self.enter("sort", l.sort_binary);
                self.binary_sort(lo, lo + force, lo + run)?;
                self.leave();
                run = force;
            }
            self.runs.push((lo, run));
            self.enter("sort", l.sort_collapse);
            self.merge_collapse()?;
            self.leave();
            lo += run;
            remaining -= run;
        }
        self.enter("sort", l.sort_force);
        self.merge_force_collapse()?;
        self.leave();
        Ok(())
    }

    fn binary_sort(&mut self, lo: usize, hi: usize, start: usize) -> Result<(), Thrown> {
        let line = self.lines.binary_cmp;
        let mut start = if start == lo { start + 1 } else { start };
        while start < hi {
            let pivot = self.a[start].clone();
            let (mut left, mut right) = (lo, start);
            while left < right {
                let mid = (left + right) >> 1;
                let m = self.a[mid].clone();
                if self.c(&pivot, &m, "binarySort", line)? == Ordering::Less {
                    right = mid;
                } else {
                    left = mid + 1;
                }
            }
            let v = self.a.remove(start);
            self.a.insert(left, v);
            start += 1;
        }
        Ok(())
    }

    fn count_run(&mut self, lo: usize, hi: usize) -> Result<usize, Thrown> {
        let l = self.lines;
        let mut run_hi = lo + 1;
        if run_hi == hi {
            return Ok(1);
        }
        let (x, y) = (self.a[run_hi].clone(), self.a[lo].clone());
        run_hi += 1;
        if self.c(&x, &y, "countRunAndMakeAscending", l.count_first)? == Ordering::Less {
            while run_hi < hi {
                let (x, y) = (self.a[run_hi].clone(), self.a[run_hi - 1].clone());
                if self.c(&x, &y, "countRunAndMakeAscending", l.count_desc)? != Ordering::Less {
                    break;
                }
                run_hi += 1;
            }
            self.a[lo..run_hi].reverse();
        } else {
            while run_hi < hi {
                let (x, y) = (self.a[run_hi].clone(), self.a[run_hi - 1].clone());
                if self.c(&x, &y, "countRunAndMakeAscending", l.count_asc)? == Ordering::Less {
                    break;
                }
                run_hi += 1;
            }
        }
        Ok(run_hi - lo)
    }

    fn merge_collapse(&mut self) -> Result<(), Thrown> {
        while self.runs.len() > 1 {
            let mut n = self.runs.len() - 2;
            let len = |s: &Self, i: usize| s.runs[i].1;
            if (n > 0 && len(self, n - 1) <= len(self, n) + len(self, n + 1)) || (n > 1 && len(self, n - 2) <= len(self, n) + len(self, n - 1)) {
                if len(self, n - 1) < len(self, n + 1) {
                    n -= 1;
                }
            } else if len(self, n) > len(self, n + 1) {
                break;
            }
            self.enter("mergeCollapse", self.lines.collapse_merge);
            self.merge_at(n)?;
            self.leave();
        }
        Ok(())
    }

    fn merge_force_collapse(&mut self) -> Result<(), Thrown> {
        while self.runs.len() > 1 {
            let mut n = self.runs.len() - 2;
            if n > 0 && self.runs[n - 1].1 < self.runs[n + 1].1 {
                n -= 1;
            }
            self.enter("mergeForceCollapse", self.lines.force_merge);
            self.merge_at(n)?;
            self.leave();
        }
        Ok(())
    }

    fn merge_at(&mut self, i: usize) -> Result<(), Thrown> {
        let l = self.lines;
        let (mut base1, mut len1) = self.runs[i];
        let (base2, mut len2) = self.runs[i + 1];
        self.runs[i] = (base1, len1 + len2);
        self.runs.remove(i + 1);
        // where does the first element of run 2 go in run 1?
        let key = self.a[base2].clone();
        self.enter("mergeAt", l.at_gallop_right);
        let k = self.gallop_right(&key, Src::A, base1, len1, 0)?;
        self.leave();
        base1 += k;
        len1 -= k;
        if len1 == 0 {
            return Ok(());
        }
        // where does the last element of run 1 go in run 2?
        let key = self.a[base1 + len1 - 1].clone();
        self.enter("mergeAt", l.at_gallop_left);
        len2 = self.gallop_left(&key, Src::A, base2, len2, len2 - 1)?;
        self.leave();
        if len2 == 0 {
            return Ok(());
        }
        if len1 <= len2 {
            self.enter("mergeAt", l.at_lo);
            self.merge_lo(base1, len1, base2, len2)?;
        } else {
            self.enter("mergeAt", l.at_hi);
            self.merge_hi(base1, len1, base2, len2)?;
        }
        self.leave();
        Ok(())
    }

    fn get(&self, src: Src, tmp: &[Value], i: usize) -> Value {
        match src {
            Src::A => self.a[i].clone(),
            Src::Tmp => tmp[i].clone(),
        }
    }

    fn gallop_left(&mut self, key: &Value, src: Src, base: usize, len: usize, hint: usize) -> Result<usize, Thrown> {
        self.gallop_left_in(key, src, &[], base, len, hint)
    }

    fn gallop_right(&mut self, key: &Value, src: Src, base: usize, len: usize, hint: usize) -> Result<usize, Thrown> {
        self.gallop_right_in(key, src, &[], base, len, hint)
    }

    /// `gallopLeft`: the leftmost position to insert `key` into the sorted range.
    fn gallop_left_in(&mut self, key: &Value, src: Src, tmp: &[Value], base: usize, len: usize, hint: usize) -> Result<usize, Thrown> {
        let l = self.lines;
        let (mut last_ofs, mut ofs): (i64, i64);
        let hint_v = self.get(src, tmp, base + hint);
        if self.c(key, &hint_v, "gallopLeft", l.gl_hint)? == Ordering::Greater {
            let max_ofs = (len - hint) as i64;
            last_ofs = 0;
            ofs = 1;
            while ofs < max_ofs {
                let v = self.get(src, tmp, base + hint + ofs as usize);
                if self.c(key, &v, "gallopLeft", l.gl_right)? != Ordering::Greater {
                    break;
                }
                last_ofs = ofs;
                ofs = (ofs << 1) + 1;
            }
            ofs = ofs.min(max_ofs);
            last_ofs += hint as i64;
            ofs += hint as i64;
        } else {
            let max_ofs = (hint + 1) as i64;
            last_ofs = 0;
            ofs = 1;
            while ofs < max_ofs {
                let v = self.get(src, tmp, base + hint - ofs as usize);
                if self.c(key, &v, "gallopLeft", l.gl_left)? == Ordering::Greater {
                    break;
                }
                last_ofs = ofs;
                ofs = (ofs << 1) + 1;
            }
            ofs = ofs.min(max_ofs);
            let t = last_ofs;
            last_ofs = hint as i64 - ofs;
            ofs = hint as i64 - t;
        }
        last_ofs += 1;
        while last_ofs < ofs {
            let m = last_ofs + ((ofs - last_ofs) >> 1);
            let v = self.get(src, tmp, base + m as usize);
            if self.c(key, &v, "gallopLeft", l.gl_bin)? == Ordering::Greater {
                last_ofs = m + 1;
            } else {
                ofs = m;
            }
        }
        Ok(ofs as usize)
    }

    /// `gallopRight`: the rightmost position to insert `key` into the sorted range.
    fn gallop_right_in(&mut self, key: &Value, src: Src, tmp: &[Value], base: usize, len: usize, hint: usize) -> Result<usize, Thrown> {
        let l = self.lines;
        let (mut last_ofs, mut ofs): (i64, i64);
        let hint_v = self.get(src, tmp, base + hint);
        if self.c(key, &hint_v, "gallopRight", l.gr_hint)? == Ordering::Less {
            let max_ofs = (hint + 1) as i64;
            last_ofs = 0;
            ofs = 1;
            while ofs < max_ofs {
                let v = self.get(src, tmp, base + hint - ofs as usize);
                if self.c(key, &v, "gallopRight", l.gr_left)? != Ordering::Less {
                    break;
                }
                last_ofs = ofs;
                ofs = (ofs << 1) + 1;
            }
            ofs = ofs.min(max_ofs);
            let t = last_ofs;
            last_ofs = hint as i64 - ofs;
            ofs = hint as i64 - t;
        } else {
            let max_ofs = (len - hint) as i64;
            last_ofs = 0;
            ofs = 1;
            while ofs < max_ofs {
                let v = self.get(src, tmp, base + hint + ofs as usize);
                if self.c(key, &v, "gallopRight", l.gr_right)? == Ordering::Less {
                    break;
                }
                last_ofs = ofs;
                ofs = (ofs << 1) + 1;
            }
            ofs = ofs.min(max_ofs);
            last_ofs += hint as i64;
            ofs += hint as i64;
        }
        last_ofs += 1;
        while last_ofs < ofs {
            let m = last_ofs + ((ofs - last_ofs) >> 1);
            let v = self.get(src, tmp, base + m as usize);
            if self.c(key, &v, "gallopRight", l.gr_bin)? == Ordering::Less {
                ofs = m;
            } else {
                last_ofs = m + 1;
            }
        }
        Ok(ofs as usize)
    }

    /// `mergeLo`: merges two adjacent runs with the first (shorter) one copied out.
    fn merge_lo(&mut self, base1: usize, len1: usize, base2: usize, len2: usize) -> Result<(), Thrown> {
        let l = self.lines;
        let tmp: Vec<Value> = self.a[base1..base1 + len1].to_vec();
        let (mut len1, mut len2) = (len1, len2);
        let (mut c1, mut c2, mut dest) = (0usize, base2, base1);
        self.a[dest] = self.a[c2].clone();
        dest += 1;
        c2 += 1;
        len2 -= 1;
        if len2 == 0 {
            self.a[dest..dest + len1].clone_from_slice(&tmp[c1..c1 + len1]);
            return Ok(());
        }
        if len1 == 1 {
            for k in 0..len2 {
                self.a[dest + k] = self.a[c2 + k].clone();
            }
            self.a[dest + len2] = tmp[c1].clone();
            return Ok(());
        }
        let mut min_gallop = self.min_gallop as i64;
        'outer: loop {
            let (mut count1, mut count2) = (0usize, 0usize);
            loop {
                let (x, y) = (self.a[c2].clone(), tmp[c1].clone());
                if self.c(&x, &y, "mergeLo", l.lo_cmp)? == Ordering::Less {
                    self.a[dest] = x;
                    dest += 1;
                    c2 += 1;
                    count2 += 1;
                    count1 = 0;
                    len2 -= 1;
                    if len2 == 0 {
                        break 'outer;
                    }
                } else {
                    self.a[dest] = y;
                    dest += 1;
                    c1 += 1;
                    count1 += 1;
                    count2 = 0;
                    len1 -= 1;
                    if len1 == 1 {
                        break 'outer;
                    }
                }
                if (count1 | count2) as i64 >= min_gallop {
                    break;
                }
            }
            loop {
                let key = self.a[c2].clone();
                self.enter("mergeLo", l.lo_gallop_right);
                count1 = self.gallop_right_in(&key, Src::Tmp, &tmp, c1, len1, 0)?;
                self.leave();
                if count1 != 0 {
                    self.a[dest..dest + count1].clone_from_slice(&tmp[c1..c1 + count1]);
                    dest += count1;
                    c1 += count1;
                    len1 -= count1;
                    if len1 <= 1 {
                        break 'outer;
                    }
                }
                self.a[dest] = self.a[c2].clone();
                dest += 1;
                c2 += 1;
                len2 -= 1;
                if len2 == 0 {
                    break 'outer;
                }
                let key = tmp[c1].clone();
                self.enter("mergeLo", l.lo_gallop_left);
                count2 = self.gallop_left(&key, Src::A, c2, len2, 0)?;
                self.leave();
                if count2 != 0 {
                    for k in 0..count2 {
                        self.a[dest + k] = self.a[c2 + k].clone();
                    }
                    dest += count2;
                    c2 += count2;
                    len2 -= count2;
                    if len2 == 0 {
                        break 'outer;
                    }
                }
                self.a[dest] = tmp[c1].clone();
                dest += 1;
                c1 += 1;
                len1 -= 1;
                if len1 == 1 {
                    break 'outer;
                }
                min_gallop -= 1;
                if !(count1 >= MIN_GALLOP || count2 >= MIN_GALLOP) {
                    break;
                }
            }
            if min_gallop < 0 {
                min_gallop = 0;
            }
            min_gallop += 2;
        }
        self.min_gallop = min_gallop.max(1) as usize;
        if len1 == 1 {
            for k in 0..len2 {
                self.a[dest + k] = self.a[c2 + k].clone();
            }
            self.a[dest + len2] = tmp[c1].clone();
        } else if len1 == 0 {
            return Err(self.contract_violation("mergeLo", l.lo_contract));
        } else {
            self.a[dest..dest + len1].clone_from_slice(&tmp[c1..c1 + len1]);
        }
        Ok(())
    }

    /// `mergeHi`: merges two adjacent runs with the second (shorter) one copied out, from the
    /// end.
    fn merge_hi(&mut self, base1: usize, len1: usize, base2: usize, len2: usize) -> Result<(), Thrown> {
        let l = self.lines;
        let tmp: Vec<Value> = self.a[base2..base2 + len2].to_vec();
        let (mut len1, mut len2) = (len1 as i64, len2 as i64);
        let mut c1 = (base1 + len1 as usize) as i64 - 1;
        let mut c2 = len2 - 1;
        let mut dest = (base2 + len2 as usize) as i64 - 1;
        self.a[dest as usize] = self.a[c1 as usize].clone();
        dest -= 1;
        c1 -= 1;
        len1 -= 1;
        if len1 == 0 {
            let start = (dest - (len2 - 1)) as usize;
            self.a[start..start + len2 as usize].clone_from_slice(&tmp[..len2 as usize]);
            return Ok(());
        }
        if len2 == 1 {
            dest -= len1;
            c1 -= len1;
            for k in (0..len1 as usize).rev() {
                self.a[(dest + 1) as usize + k] = self.a[(c1 + 1) as usize + k].clone();
            }
            self.a[dest as usize] = tmp[c2 as usize].clone();
            return Ok(());
        }
        let mut min_gallop = self.min_gallop as i64;
        'outer: loop {
            let (mut count1, mut count2) = (0i64, 0i64);
            loop {
                let (x, y) = (tmp[c2 as usize].clone(), self.a[c1 as usize].clone());
                if self.c(&x, &y, "mergeHi", l.hi_cmp)? == Ordering::Less {
                    self.a[dest as usize] = y;
                    dest -= 1;
                    c1 -= 1;
                    count1 += 1;
                    count2 = 0;
                    len1 -= 1;
                    if len1 == 0 {
                        break 'outer;
                    }
                } else {
                    self.a[dest as usize] = x;
                    dest -= 1;
                    c2 -= 1;
                    count2 += 1;
                    count1 = 0;
                    len2 -= 1;
                    if len2 == 1 {
                        break 'outer;
                    }
                }
                if (count1 | count2) >= min_gallop {
                    break;
                }
            }
            loop {
                let key = tmp[c2 as usize].clone();
                self.enter("mergeHi", l.hi_gallop_right);
                count1 = len1 - self.gallop_right(&key, Src::A, base1, len1 as usize, len1 as usize - 1)? as i64;
                self.leave();
                if count1 != 0 {
                    dest -= count1;
                    c1 -= count1;
                    len1 -= count1;
                    for k in (0..count1 as usize).rev() {
                        self.a[(dest + 1) as usize + k] = self.a[(c1 + 1) as usize + k].clone();
                    }
                    if len1 == 0 {
                        break 'outer;
                    }
                }
                self.a[dest as usize] = tmp[c2 as usize].clone();
                dest -= 1;
                c2 -= 1;
                len2 -= 1;
                if len2 == 1 {
                    break 'outer;
                }
                let key = self.a[c1 as usize].clone();
                self.enter("mergeHi", l.hi_gallop_left);
                count2 = len2 - self.gallop_left_in(&key, Src::Tmp, &tmp, 0, len2 as usize, len2 as usize - 1)? as i64;
                self.leave();
                if count2 != 0 {
                    dest -= count2;
                    c2 -= count2;
                    len2 -= count2;
                    for k in 0..count2 as usize {
                        self.a[(dest + 1) as usize + k] = tmp[(c2 + 1) as usize + k].clone();
                    }
                    if len2 <= 1 {
                        break 'outer;
                    }
                }
                self.a[dest as usize] = self.a[c1 as usize].clone();
                dest -= 1;
                c1 -= 1;
                len1 -= 1;
                if len1 == 0 {
                    break 'outer;
                }
                min_gallop -= 1;
                if !(count1 >= MIN_GALLOP as i64 || count2 >= MIN_GALLOP as i64) {
                    break;
                }
            }
            if min_gallop < 0 {
                min_gallop = 0;
            }
            min_gallop += 2;
        }
        self.min_gallop = min_gallop.max(1) as usize;
        if len2 == 1 {
            dest -= len1;
            c1 -= len1;
            for k in (0..len1 as usize).rev() {
                self.a[(dest + 1) as usize + k] = self.a[(c1 + 1) as usize + k].clone();
            }
            self.a[dest as usize] = tmp[c2 as usize].clone();
        } else if len2 == 0 {
            return Err(self.contract_violation("mergeHi", l.hi_contract));
        } else {
            let start = (dest - (len2 - 1)) as usize;
            self.a[start..start + len2 as usize].clone_from_slice(&tmp[..len2 as usize]);
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum Src {
    A,
    Tmp,
}

fn min_run_length(mut n: usize) -> usize {
    let mut r = 0;
    while n >= MIN_MERGE {
        r |= n & 1;
        n >>= 1;
    }
    n + r
}

/// Sorts `items` in place the way `TimSort.sort` (with a comparator) or `ComparableTimSort.sort`
/// (natural ordering) does. On failure `items` is left as the JDK would leave its array:
/// partially sorted.
pub fn timsort(items: &mut Vec<Value>, flavor: Flavor, cmp: &mut dyn FnMut(&Value, &Value) -> Result<Ordering, Thrown>) -> Result<(), Thrown> {
    let lines = match flavor {
        Flavor::Natural => &NATURAL,
        Flavor::Comparator => &COMPARATOR,
    };
    let mut s = Sorter { a: std::mem::take(items), cmp, flavor, lines, min_gallop: MIN_GALLOP, runs: Vec::new(), chain: Vec::new() };
    let r = s.sort();
    *items = s.a;
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(v: &[i32]) -> (Vec<i32>, usize) {
        let mut items: Vec<Value> = v.iter().map(|&x| Value::Int(x)).collect();
        let mut n = 0;
        timsort(&mut items, Flavor::Natural, &mut |a, b| {
            n += 1;
            Ok(a.as_i64().cmp(&b.as_i64()))
        })
        .unwrap();
        (items.iter().map(|v| v.as_i64().unwrap() as i32).collect(), n)
    }

    #[test]
    fn sorts_like_java() {
        // pseudo-random data across the mini and full paths
        let mut seed = 42u64;
        for n in [0usize, 1, 2, 5, 31, 32, 33, 64, 100, 257, 1000, 5000] {
            let v: Vec<i32> = (0..n)
                .map(|_| {
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    ((seed >> 33) % 100) as i32
                })
                .collect();
            let mut want = v.clone();
            want.sort();
            assert_eq!(sorted(&v).0, want, "n = {n}");
        }
        // already sorted input costs n - 1 comparisons
        let v: Vec<i32> = (0..100).collect();
        assert_eq!(sorted(&v).1, 99);
    }

    #[test]
    fn stable() {
        let mut items: Vec<Value> = (0..200).map(|i| Value::Int((i % 7) * 1000 + i)).collect();
        timsort(&mut items, Flavor::Comparator, &mut |a, b| Ok((a.as_i64().unwrap() / 1000).cmp(&(b.as_i64().unwrap() / 1000)))).unwrap();
        let keys: Vec<i64> = items.iter().map(|v| v.as_i64().unwrap()).collect();
        for w in keys.windows(2) {
            assert!(w[0] / 1000 < w[1] / 1000 || (w[0] / 1000 == w[1] / 1000 && w[0] < w[1]));
        }
    }

    #[test]
    fn failure_frames() {
        let mut items = vec![Value::Int(3), Value::str("a")];
        let e = timsort(&mut items, Flavor::Natural, &mut |a, b| crate::func::natural_compare(a, b)).unwrap_err();
        assert_eq!(
            e.frames,
            vec![
                "java.base/java.lang.String.compareTo(String.java:173)",
                "java.base/java.util.ComparableTimSort.countRunAndMakeAscending(ComparableTimSort.java:320)",
                "java.base/java.util.ComparableTimSort.sort(ComparableTimSort.java:188)",
            ]
        );
    }
}
