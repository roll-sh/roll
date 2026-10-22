//! The fast vector highlighter (`fvh`): Lucene's `FastVectorHighlighter` as Elasticsearch runs
//! it, for fields indexed with `term_vector: with_positions_offsets`.
//!
//! The pipeline follows Lucene step by step, because its output depends on every step:
//!
//! 1. *Field query* (`FieldQuery` + Elasticsearch's `CustomFieldQuery`): the query is
//!    flattened into terms and phrases with their boosts (must-not clauses skipped,
//!    multi-phrases expanded into every combination up to 16 terms), overlapping phrases are
//!    expanded (`a b c` and `b c d` add `a b c d`), and everything goes into a trie of terms
//!    (`QueryPhraseMap`) whose terminal nodes carry slop, boost and a sequence number (which
//!    picks the tag in `tags_schema: styled`).
//! 2. *Term stack* (`FieldTermStack`): the doc's tokens whose term the query mentions, in
//!    position order; tokens sharing a position form a ring so a phrase can continue through
//!    any of them. Term vectors come from the same analysis as re-analyzing the values, so the
//!    values are re-analyzed here.
//! 3. *Phrase list* (`FieldPhraseList`): the stack is walked through the trie, longest match
//!    first, backing off when a phrase is incomplete or out of slop; overlapping matches merge
//!    (boosts add up). At most `phrase_limit` (256) are collected.
//! 4. *Fragment list* (`SimpleFragListBuilder` / `SingleFragListBuilder`): fragments of
//!    `fragment_size` chars around consecutive matches, with a margin of `fragment_offset` (6)
//!    chars before the first; `number_of_fragments: 0` makes one fragment of everything.
//! 5. *Fragments* (`SimpleFragmentsBuilder` / `ScoreOrderFragmentsBuilder`): values are split
//!    so no fragment crosses a value (`discreteMultiValueHighlighting`, Elasticsearch's
//!    default), ordered by position or by summed boost, and their edges moved to boundaries
//!    (`chars`: the nearest `boundary_chars` within `boundary_max_scan`; `word`/`sentence`:
//!    break iterators over the text before and after).
//!
//! Offsets are UTF-16 code units over the values joined with one separator char, as in Java.

use std::collections::{HashMap, VecDeque};

use search_analysis::Analyzer;

use crate::error::{Error, Result};
use crate::highlight::breaks::{Boundaries, Content};
use crate::highlight::{BoundaryScanner, Encoder, HighlightOptions, HlToken, analyze};
use crate::query::MultiTermQuery;
use crate::search::{QueryRef, QueryVisitor, Searcher, TermMatcher};

/// Lucene's `FieldQuery.MAX_MTQ_TERMS`.
const MAX_MTQ_TERMS: usize = 1024;

/// A flattened query clause.
enum Flat {
    Term { term: Vec<u8>, boost: f32 },
    Phrase { terms: Vec<Vec<u8>>, slop: u32, boost: f32 },
    Multi { matcher: TermMatcher, boost: f32 },
    /// A multi-term query, expanded against the index when a searcher is given.
    MultiQuery { q: MultiTermQuery, boost: f32 },
}

impl Flat {
    fn same(&self, o: &Flat) -> bool {
        match (self, o) {
            (Flat::Term { term: a, boost: x }, Flat::Term { term: b, boost: y }) => a == b && x == y,
            (Flat::Phrase { terms: a, slop: s, boost: x }, Flat::Phrase { terms: b, slop: t, boost: y }) => {
                a == b && s == t && x == y
            }
            _ => false,
        }
    }
}

/// Collects the flat clauses on the highlighted fields (Lucene's `flatten`).
struct Flatten<'a> {
    fields: Option<&'a [u32]>,
    boosts: Vec<f32>,
    out: Vec<Flat>,
}

impl Flatten<'_> {
    fn ok(&self, f: u32) -> bool {
        self.fields.is_none_or(|fs| fs.contains(&f))
    }

    fn boost(&self) -> f32 {
        self.boosts.last().copied().unwrap_or(1.0)
    }

    fn add(&mut self, f: Flat) {
        if !self.out.iter().any(|x| x.same(&f)) {
            self.out.push(f);
        }
    }

    fn add_phrase(&mut self, terms: Vec<Vec<u8>>, slop: u32, boost: f32) {
        if terms.len() == 1 {
            self.add(Flat::Term { term: terms.into_iter().next().unwrap(), boost });
        } else if !terms.is_empty() {
            self.add(Flat::Phrase { terms, slop, boost });
        }
    }
}

impl QueryVisitor for Flatten<'_> {
    fn term(&mut self, field: u32, term: &[u8]) {
        if self.ok(field) {
            let boost = self.boost();
            self.add(Flat::Term { term: term.to_vec(), boost });
        }
    }

    fn multi_term(&mut self, field: u32, matcher: TermMatcher) {
        if self.ok(field) {
            let boost = self.boost();
            self.out.push(Flat::Multi { matcher, boost });
        }
    }

    fn phrase(&mut self, field: u32, slots: &[(u32, Vec<Vec<u8>>)], slop: u32) {
        if !self.ok(field) || slots.is_empty() {
            return;
        }
        if slots.iter().all(|(_, ts)| ts.len() == 1) {
            let boost = self.boost();
            self.add_phrase(slots.iter().map(|(_, ts)| ts[0].clone()).collect(), slop, boost);
            return;
        }
        // Elasticsearch's `convertMultiPhraseQuery`: every combination as a phrase (boost 1),
        // or the bare terms past 16.
        let total: usize = slots.iter().map(|(_, ts)| ts.len()).sum();
        if total > 16 {
            for (_, ts) in slots {
                for t in ts {
                    self.add(Flat::Term { term: t.clone(), boost: 1.0 });
                }
            }
            return;
        }
        let mut idx = vec![0usize; slots.len()];
        loop {
            let terms: Vec<Vec<u8>> = slots.iter().zip(&idx).map(|((_, ts), &i)| ts[i].clone()).collect();
            self.add_phrase(terms, slop, 1.0);
            let mut k = slots.len();
            loop {
                if k == 0 {
                    return;
                }
                k -= 1;
                idx[k] += 1;
                if idx[k] < slots[k].1.len() {
                    break;
                }
                idx[k] = 0;
            }
        }
    }

    fn multi_term_query(&mut self, q: &MultiTermQuery) {
        if self.ok(q.field) {
            let boost = self.boost();
            self.out.push(Flat::MultiQuery { q: q.clone(), boost });
        }
    }

    fn begin_boost(&mut self, boost: f32) {
        let b = self.boost() * boost;
        self.boosts.push(b);
    }

    fn end_boost(&mut self) {
        self.boosts.pop();
    }
}

/// Lucene's `FieldQuery.expand`: overlapping phrases with the same slop add their union.
fn expand(flat: Vec<Flat>) -> Vec<Flat> {
    let mut out: Vec<Flat> = Vec::new();
    let mut rest: VecDeque<Flat> = flat.into();
    while let Some(q) = rest.pop_front() {
        let mut added = Vec::new();
        if let Flat::Phrase { terms: a, slop: sa, .. } = &q {
            for qj in rest.iter() {
                if let Flat::Phrase { terms: b, slop: sb, .. } = qj
                    && sa == sb
                {
                    overlap(&mut added, a, b, *sa);
                    overlap(&mut added, b, a, *sb);
                }
            }
        }
        out.push(q);
        for p in added {
            if !out.iter().any(|x| x.same(&p)) {
                out.push(p);
            }
        }
    }
    out
}

fn overlap(out: &mut Vec<Flat>, src: &[Vec<u8>], dest: &[Vec<u8>], slop: u32) {
    for i in 1..src.len() {
        let ok = (i..src.len()).all(|j| j - i >= dest.len() || src[j] == dest[j - i]);
        if ok && src.len() - i < dest.len() {
            let mut terms = src.to_vec();
            terms.extend(dest[src.len() - i..].iter().cloned());
            // Lucene wraps it in a boost of 1 whatever the original boost.
            let p = Flat::Phrase { terms, slop, boost: 1.0 };
            if !out.iter().any(|x| x.same(&p)) {
                out.push(p);
            }
        }
    }
}

/// A node of the term trie (`QueryPhraseMap`).
#[derive(Default)]
struct Node {
    terminal: bool,
    slop: u32,
    boost: f32,
    seqnum: usize,
    sub: HashMap<Vec<u8>, usize>,
}

struct PhraseMap {
    nodes: Vec<Node>,
    seq: usize,
}

impl PhraseMap {
    fn child(&mut self, n: usize, t: &[u8]) -> usize {
        if let Some(&c) = self.nodes[n].sub.get(t) {
            return c;
        }
        self.nodes.push(Node::default());
        let c = self.nodes.len() - 1;
        self.nodes[n].sub.insert(t.to_vec(), c);
        c
    }

    fn mark(&mut self, n: usize, slop: u32, boost: f32) {
        let s = self.seq;
        self.seq += 1;
        let node = &mut self.nodes[n];
        node.terminal = true;
        node.slop = slop;
        node.boost = boost;
        node.seqnum = s;
    }

    fn get(&self, n: usize, t: &[u8]) -> Option<usize> {
        self.nodes[n].sub.get(t).copied()
    }

    /// `isValidTermOrPhrase`: terminal, and consecutive candidates within the slop.
    fn valid(&self, n: usize, cand: &[usize], infos: &[TermInfo]) -> bool {
        let node = &self.nodes[n];
        if !node.terminal {
            return false;
        }
        let mut pos = infos[cand[0]].position as i64;
        for &c in &cand[1..] {
            let next = infos[c].position as i64;
            if (next - pos - 1).unsigned_abs() > node.slop as u64 {
                return false;
            }
            pos = next;
        }
        true
    }

    fn search(&self, cand: &[usize], infos: &[TermInfo]) -> Option<usize> {
        let mut n = 0;
        for &c in cand {
            n = self.get(n, &infos[c].term)?;
        }
        self.valid(n, cand, infos).then_some(n)
    }
}

struct TermInfo {
    term: Vec<u8>,
    start: i64,
    end: i64,
    position: u32,
    /// Next token at the same position (a ring).
    next: usize,
}

/// A matched term or phrase (`WeightedPhraseInfo`).
#[derive(Clone)]
struct PhraseInfo {
    toffs: Vec<(i64, i64)>,
    boost: f32,
    seqnum: usize,
}

impl PhraseInfo {
    fn new(cand: &[usize], infos: &[TermInfo], boost: f32, seqnum: usize) -> PhraseInfo {
        let first = &infos[cand[0]];
        let mut toffs = vec![(first.start, first.end)];
        let mut pos = first.position;
        for &c in &cand[1..] {
            let ti = &infos[c];
            if ti.position.wrapping_sub(pos) == 1 {
                toffs.last_mut().unwrap().1 = ti.end;
            } else {
                toffs.push((ti.start, ti.end));
            }
            pos = ti.position;
        }
        PhraseInfo { toffs, boost, seqnum }
    }

    fn start(&self) -> i64 {
        self.toffs[0].0
    }

    fn end(&self) -> i64 {
        self.toffs[self.toffs.len() - 1].1
    }

    fn overlaps(&self, o: &PhraseInfo) -> bool {
        let (so, eo, oso, oeo) = (self.start(), self.end(), o.start(), o.end());
        (so <= oso && oso < eo) || (so < oeo && oeo <= eo) || (oso <= so && so < oeo) || (oso < eo && eo <= oeo)
    }
}

/// `FieldPhraseList`: walks the term stack through the trie.
fn phrase_list(map: &PhraseMap, infos: &[TermInfo], mut stack: VecDeque<usize>, limit: usize) -> Vec<PhraseInfo> {
    let mut list: Vec<PhraseInfo> = Vec::new();
    let add = |list: &mut Vec<PhraseInfo>, p: PhraseInfo| {
        for e in list.iter_mut() {
            if e.overlaps(&p) {
                e.boost += p.boost;
                return;
            }
        }
        list.push(p);
    };
    let mut cand: Vec<usize> = Vec::new();
    while !stack.is_empty() && list.len() < limit {
        cand.clear();
        let first = stack.pop_front().unwrap();
        let mut ti = first;
        let mut cur = map.get(0, &infos[ti].term);
        while cur.is_none() && infos[ti].next != first {
            ti = infos[ti].next;
            cur = map.get(0, &infos[ti].term);
        }
        let Some(mut cur) = cur else { continue };
        cand.push(ti);
        loop {
            let popped = stack.pop_front();
            let mut next = None;
            let mut ti = usize::MAX;
            if let Some(first) = popped {
                ti = first;
                next = map.get(cur, &infos[ti].term);
                while next.is_none() && infos[ti].next != first {
                    ti = infos[ti].next;
                    next = map.get(cur, &infos[ti].term);
                }
            }
            match next {
                Some(n) if popped.is_some() => {
                    cand.push(ti);
                    cur = n;
                }
                _ => {
                    if popped.is_some() {
                        stack.push_front(ti);
                    }
                    if map.valid(cur, &cand, infos) {
                        let node = &map.nodes[cur];
                        add(&mut list, PhraseInfo::new(&cand, infos, node.boost, node.seqnum));
                    } else {
                        while cand.len() > 1 {
                            stack.push_front(cand.pop().unwrap());
                            if let Some(n) = map.search(&cand, infos) {
                                let node = &map.nodes[n];
                                add(&mut list, PhraseInfo::new(&cand, infos, node.boost, node.seqnum));
                                break;
                            }
                        }
                    }
                    break;
                }
            }
        }
    }
    list
}

/// A fragment: offsets, the matches inside, summed boost (`WeightedFragInfo`).
#[derive(Clone)]
struct FragInfo {
    start: i64,
    end: i64,
    subs: Vec<PhraseInfo>,
    boost: f32,
}

fn frag(start: i64, end: i64, subs: Vec<PhraseInfo>) -> FragInfo {
    let boost = subs.iter().map(|s| s.boost).sum();
    FragInfo { start, end, subs, boost }
}

/// `SimpleFragListBuilder` (`BaseFragListBuilder.createFieldFragList`).
fn simple_frag_list(phrases: &[PhraseInfo], margin: i64, frag_size: i64) -> Vec<FragInfo> {
    let mut out = Vec::new();
    let accept = |p: &PhraseInfo, match_len: i64| p.toffs.len() <= 1 || match_len <= frag_size;
    let mut i = 0;
    let mut start_offset = 0i64;
    while i < phrases.len() {
        let p = &phrases[i];
        if p.start() < start_offset {
            i += 1;
            continue;
        }
        let mut wpil = Vec::new();
        let cur_start = p.start();
        let mut cur_end = p.end();
        let span_start = (cur_start - margin).max(start_offset);
        let span_end = cur_end.max(span_start + frag_size);
        if accept(p, cur_end - cur_start) {
            wpil.push(p.clone());
        }
        i += 1;
        while i < phrases.len() {
            let q = &phrases[i];
            if q.end() <= span_end {
                cur_end = q.end();
                if accept(q, cur_end - cur_start) {
                    wpil.push(q.clone());
                }
                i += 1;
            } else {
                break;
            }
        }
        if wpil.is_empty() {
            continue;
        }
        let match_len = cur_end - cur_start;
        let new_margin = ((frag_size - match_len) / 2).max(0);
        let span_start = (cur_start - new_margin).max(start_offset);
        let span_end = span_start + match_len.max(frag_size);
        start_offset = span_end;
        out.push(frag(span_start, span_end, wpil));
    }
    out
}

/// Java chars of the values joined with spaces, built lazily (`getFragmentSourceMSO`).
struct Source<'a> {
    values: &'a [Vec<u16>],
    buf: Vec<u16>,
    next: usize,
}

impl Source<'_> {
    fn fill(&mut self, end: i64) {
        while (self.buf.len() as i64) < end && self.next < self.values.len() {
            self.buf.extend_from_slice(&self.values[self.next]);
            self.buf.push(b' ' as u16);
            self.next += 1;
        }
    }
}

/// `BoundaryScanner`.
struct Scanner<'a> {
    kind: BoundaryScanner,
    chars: Vec<u16>,
    max_scan: i64,
    _p: std::marker::PhantomData<&'a ()>,
}

impl Scanner<'_> {
    fn find_start(&self, buf: &[u16], start: i64) -> i64 {
        if start > buf.len() as i64 || start < 1 {
            return start;
        }
        match self.kind {
            BoundaryScanner::Chars => {
                let mut offset = start;
                let mut count = self.max_scan;
                while offset > 0 && count > 0 {
                    if self.chars.contains(&buf[offset as usize - 1]) {
                        return offset;
                    }
                    offset -= 1;
                    count -= 1;
                }
                if offset == 0 { 0 } else { start }
            }
            k => {
                // `BreakIterator.last(); previous()` over the text before `start`.
                let s = String::from_utf16_lossy(&buf[..start as usize]);
                let c = Content::new(&s);
                let b = if k == BoundaryScanner::Word { Boundaries::words(&c) } else { Boundaries::sentences(&c) };
                let len = c.len16();
                b.b.iter().rev().find(|&&x| x < len).map_or(0, |&x| x as i64)
            }
        }
    }

    fn find_end(&self, buf: &[u16], start: i64) -> i64 {
        if start > buf.len() as i64 || start < 0 {
            return start;
        }
        match self.kind {
            BoundaryScanner::Chars => {
                let mut offset = start;
                let mut count = self.max_scan;
                while offset < buf.len() as i64 && count > 0 {
                    if self.chars.contains(&buf[offset as usize]) {
                        return offset;
                    }
                    offset += 1;
                    count -= 1;
                }
                start
            }
            k => {
                // `BreakIterator.next()` over the text from `start`.
                let s = String::from_utf16_lossy(&buf[start as usize..]);
                let c = Content::new(&s);
                let b = if k == BoundaryScanner::Word { Boundaries::words(&c) } else { Boundaries::sentences(&c) };
                b.b.iter().find(|&&x| x > 0).map_or(start, |&x| start + x as i64)
            }
        }
    }
}

fn encode(enc: Encoder, s: &[u16], out: &mut String) {
    enc.encode(&String::from_utf16_lossy(s), out);
}

/// `makeFragment`.
fn make_fragment(src: &mut Source<'_>, sc: &Scanner<'_>, f: &FragInfo, opts: &HighlightOptions) -> String {
    src.fill(f.end);
    let mut blen = src.buf.len() as i64;
    if src.next > 0 {
        // The trailing separator (Elasticsearch's source fields are tokenized).
        blen -= 1;
    }
    let eo = if blen < f.end { blen } else { sc.find_end(&src.buf, f.end) };
    let mso = sc.find_start(&src.buf, f.start);
    let (mso, eo) = (mso.clamp(0, src.buf.len() as i64), eo.clamp(0, src.buf.len() as i64));
    let text = &src.buf[mso as usize..eo.max(mso) as usize];
    let mut out = String::new();
    let mut idx = 0i64;
    let tlen = text.len() as i64;
    for sub in &f.subs {
        let pre = &opts.pre_tags[sub.seqnum % opts.pre_tags.len().max(1)];
        let post = &opts.post_tags[sub.seqnum % opts.post_tags.len().max(1)];
        for &(s, e) in &sub.toffs {
            let (a, b) = ((s - mso).clamp(idx, tlen), (e - mso).clamp(0, tlen));
            let b = b.max(a);
            encode(opts.encoder, &text[idx as usize..a as usize], &mut out);
            out.push_str(pre);
            encode(opts.encoder, &text[a as usize..b as usize], &mut out);
            out.push_str(post);
            idx = b;
        }
    }
    encode(opts.encoder, &text[idx as usize..], &mut out);
    out
}

/// `discreteMultiValueHighlighting`: splits fragments at value boundaries.
fn split_by_value(frags: Vec<FragInfo>, values: &[Vec<u16>]) -> Vec<FragInfo> {
    let mut result = Vec::new();
    'frags: for mut fi in frags {
        let mut field_end = 0i64;
        for v in values {
            if v.is_empty() {
                field_end += 1;
                continue;
            }
            let field_start = field_end;
            field_end += v.len() as i64 + 1;
            if fi.start >= field_start && fi.end >= field_start && fi.start <= field_end && fi.end <= field_end {
                result.push(fi);
                continue 'frags;
            }
            if fi.subs.is_empty() {
                continue 'frags;
            }
            let first = fi.subs[0].toffs[0];
            if fi.start >= field_end || first.0 >= field_end {
                continue;
            }
            let frag_start = if fi.start > field_start && fi.start < field_end { fi.start } else { field_start };
            let frag_end = if fi.end > field_start && fi.end < field_end { fi.end } else { field_end };
            let mut subs = Vec::new();
            let mut boost = 0f32;
            let mut si = 0;
            while si < fi.subs.len() {
                let sub = &mut fi.subs[si];
                let mut kept = Vec::new();
                let mut ti = 0;
                while ti < sub.toffs.len() {
                    let (s, e) = sub.toffs[ti];
                    if s >= field_end {
                        break;
                    }
                    let after = s >= field_start;
                    let before = e < field_end;
                    if after && before {
                        kept.push((s, e));
                        sub.toffs.remove(ti);
                        continue;
                    } else if after {
                        kept.push((s, field_end - 1));
                    } else if before {
                        kept.push((field_start, e));
                        sub.toffs.remove(ti);
                        continue;
                    } else {
                        kept.push((field_start, field_end - 1));
                    }
                    ti += 1;
                }
                if !kept.is_empty() {
                    boost += sub.boost;
                    subs.push(PhraseInfo { toffs: kept, boost: sub.boost, seqnum: sub.seqnum });
                }
                if sub.toffs.is_empty() {
                    fi.subs.remove(si);
                } else {
                    si += 1;
                }
            }
            result.push(FragInfo { start: frag_start, end: frag_end, subs, boost });
        }
    }
    result.sort_by_key(|f| f.start);
    result
}

/// Elasticsearch's `FragmentBuilderHelper.fixWeightedFragInfo` (analyzers whose offsets go
/// backwards).
fn fix_broken(mut f: FragInfo) -> FragInfo {
    if f.subs.is_empty() {
        return f;
    }
    f.subs.sort_by_key(|s| s.toffs[0].0);
    f.start = f.start.min(f.subs[0].toffs[0].0);
    f
}

/// `createFragments`.
fn create_fragments(values: &[Vec<u16>], mut frags: Vec<FragInfo>, max: usize, by_score: bool, sc: &Scanner<'_>, opts: &HighlightOptions) -> Vec<String> {
    if values.is_empty() {
        return Vec::new();
    }
    if values.len() > 1 {
        frags = split_by_value(frags, values);
    }
    if by_score {
        frags.sort_by(|a, b| b.boost.partial_cmp(&a.boost).unwrap_or(std::cmp::Ordering::Equal).then(a.start.cmp(&b.start)));
    }
    let mut src = Source { values, buf: Vec::new(), next: 0 };
    frags
        .into_iter()
        .take(max)
        .map(|f| {
            let f = if opts.fix_broken_analysis { fix_broken(f) } else { f };
            make_fragment(&mut src, sc, &f, opts)
        })
        .collect()
}

/// Highlights `values` with the fast vector highlighter.
pub fn highlight(
    searcher: Option<&Searcher>,
    query: &QueryRef,
    values: &[&str],
    analyzer: Option<&Analyzer>,
    fields: Option<&[u32]>,
    opts: &HighlightOptions,
) -> Result<Vec<String>> {
    if !opts.term_vectors_with_offsets {
        return Err(Error::illegal(format!(
            "the field [{}] should be indexed with term vector with position offsets to be used with fast vector highlighter",
            opts.field_name
        )));
    }
    let whole = opts.number_of_fragments == 0;
    let margin = opts.fragment_offset.map_or(6, |m| m as i64);
    let frag_size = if whole { i32::MAX as i64 } else { opts.fragment_size as i64 };
    let min_frag = (margin * 3).max(1);
    if !whole && frag_size < min_frag {
        return Err(Error::illegal(format!("fragCharSize({frag_size}) is too small. It must be {min_frag} or higher.")));
    }
    // 1. Field query.
    let mut fl = Flatten { fields, boosts: Vec::new(), out: Vec::new() };
    query.visit(&mut fl);
    let tokens: Vec<HlToken> = analyze(values, analyzer, None, opts.tokenized);
    // Multi-term queries expand to their terms in the index (Lucene's top-terms rewrite, in
    // term order), or without a searcher to the doc's matching terms.
    let mut flat = Vec::new();
    let push = |flat: &mut Vec<Flat>, f: Flat| {
        if !flat.iter().any(|x: &Flat| x.same(&f)) {
            flat.push(f);
        }
    };
    for f in fl.out {
        match f {
            Flat::MultiQuery { q, boost } => match searcher {
                Some(s) => {
                    for (term, b, _) in q.top_terms(s, MAX_MTQ_TERMS)? {
                        push(&mut flat, Flat::Term { term, boost: boost * b });
                    }
                }
                None => {
                    let mut ts: Vec<&[u8]> = tokens.iter().map(|t| t.term.as_slice()).filter(|t| q.matches_term(t)).collect();
                    ts.sort_unstable();
                    ts.dedup();
                    for t in ts {
                        push(&mut flat, Flat::Term { term: t.to_vec(), boost });
                    }
                }
            },
            Flat::Multi { matcher, boost } => {
                let mut ts: Vec<&[u8]> = tokens.iter().map(|t| t.term.as_slice()).filter(|t| matcher(t)).collect();
                ts.sort_unstable();
                ts.dedup();
                for t in ts {
                    let f = Flat::Term { term: t.to_vec(), boost };
                    if !flat.iter().any(|x: &Flat| x.same(&f)) {
                        flat.push(f);
                    }
                }
            }
            f => flat.push(f),
        }
    }
    let mut terms: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    for f in &flat {
        match f {
            Flat::Term { term, .. } => {
                terms.insert(term.clone());
            }
            Flat::Phrase { terms: ts, .. } => terms.extend(ts.iter().cloned()),
            Flat::Multi { .. } | Flat::MultiQuery { .. } => {}
        }
    }
    let mut map = PhraseMap { nodes: vec![Node::default()], seq: 0 };
    for f in expand(flat) {
        match f {
            Flat::Term { term, boost } => {
                let n = map.child(0, &term);
                map.mark(n, 0, boost);
            }
            Flat::Phrase { terms, slop, boost } => {
                let mut n = 0;
                for t in &terms {
                    n = map.child(n, t);
                }
                map.mark(n, slop, boost);
            }
            Flat::Multi { .. } | Flat::MultiQuery { .. } => {}
        }
    }
    // 2. Term stack: query terms in position order, then term order.
    let mut sel: Vec<&HlToken> = tokens.iter().filter(|t| terms.contains(&t.term)).collect();
    sel.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.term.cmp(&b.term)));
    let mut infos: Vec<TermInfo> = Vec::with_capacity(sel.len());
    let mut stack = VecDeque::new();
    let mut i = 0;
    while i < sel.len() {
        let mut j = i;
        while j < sel.len() && sel[j].position == sel[i].position {
            j += 1;
        }
        let base = infos.len();
        for (k, t) in sel[i..j].iter().enumerate() {
            let next = if i + k + 1 < j { base + k + 1 } else { base };
            infos.push(TermInfo { term: t.term.clone(), start: t.start as i64, end: t.end as i64, position: t.position, next });
        }
        stack.push_back(base);
        i = j;
    }
    // 3-4. Phrases and fragments.
    let phrases = phrase_list(&map, &infos, stack, opts.phrase_limit);
    let frags = if whole {
        if phrases.is_empty() { Vec::new() } else { vec![frag(0, i32::MAX as i64, phrases)] }
    } else {
        simple_frag_list(&phrases, margin, frag_size)
    };
    // 5. Fragments.
    let vals: Vec<Vec<u16>> = values.iter().map(|v| v.encode_utf16().collect()).collect();
    let chars: Vec<u16> = opts.boundary_chars.encode_utf16().collect();
    let sc = Scanner { kind: opts.boundary_scanner.unwrap_or(BoundaryScanner::Chars), chars, max_scan: opts.boundary_max_scan as i64, _p: std::marker::PhantomData };
    let max = if whole { usize::MAX } else { opts.number_of_fragments };
    let out = create_fragments(&vals, frags, max, opts.order_by_score && !whole, &sc, opts);
    if !out.is_empty() || opts.no_match_size == 0 {
        return Ok(out);
    }
    let nm = vec![frag(0, opts.no_match_size as i64, Vec::new())];
    Ok(create_fragments(&vals, nm, 1, false, &sc, opts))
}
