//! Query evaluation (`Query_EvalNode`, the module's `query_eval`): a parsed and expanded
//! query tree to the iterator tree that reads it — term, prefix, fuzzy, wildcard and lex range
//! expansions over the term trie, tag values, numeric and geo ranges, and the combinators.

use super::error::{Code, QErr};
use super::index::Index;
use super::iters::{self, BoxIt, Checker, Empty, IdList, InvIt, IxResult, K_NUMERIC, K_TERM, MASK_ALL, Predicate, Qcx, RFilter, Term};
use super::numtree::NumericFilter;
use super::qast::{Kind, Node};
use super::spec;
use super::trie::{self, Rune};
use crate::util::cstr;
use std::rc::Rc;

/// `QueryEvalCtx`.
pub struct EvalCx<'a> {
    pub ix: &'a Index,
    pub q: &'a Qcx<'a>,
    /// The request's field mask (INFIELDS).
    pub req_mask: u128,
    pub slop: i64,
    pub in_order: bool,
    pub dialect: u32,
    pub max_expansions: usize,
    pub min_prefix: usize,
    pub min_heap: usize,
    pub prioritize: bool,
    pub in_not: bool,
    /// `QUERY_WMAXPREFIXEXPANSIONS` was raised.
    pub expansions_warning: bool,
    /// The doc table's max id when a cursor's iterators were first built: a cursor's later
    /// chunks rebuild them, and the module's wildcard ranges end where they started.
    pub top: Option<u64>,
}

const TOO_LONG: &str = "query string is too long. Maximum allowed length is 1024";

impl<'a> EvalCx<'a> {
    fn index_all(&self) -> bool {
        self.ix.spec.rule.index_all
    }

    fn max_doc(&self) -> u64 {
        self.top.unwrap_or(self.ix.docs.max_doc_id)
    }


    /// `QAST_Iterate`: the root's iterator (empty when it has none).
    pub fn iterate(&mut self, root: &Node) -> Result<BoxIt<'a>, QErr> {
        Ok(self.eval(root, MASK_ALL)?.unwrap_or_else(|| Box::new(Empty)))
    }

    fn child(&mut self, n: &Node, mask: u128) -> Result<BoxIt<'a>, QErr> {
        Ok(self.eval(n, mask)?.unwrap_or_else(|| Box::new(Empty)))
    }

    /// `Redis_OpenReader`: a text term's reader, none when the term has no docs in the fields.
    fn open_term(&self, term: &[u8], mask: u128, weight: f64) -> Option<BoxIt<'a>> {
        let ii = self.ix.terms.get(term)?;
        if ii.unique_docs() == 0 || (self.ix.spec.flags & spec::I_FIELD_FLAGS != 0 && ii.field_mask() & mask == 0) {
            return None;
        }
        let total = self.ix.stats.num_documents;
        let unique = ii.unique_docs() as u64;
        let t = Term { s: term.to_vec(), idf: iters::idf(total, unique), bm25_idf: iters::bm25_idf(total, unique) };
        let res = IxResult { kind: K_TERM, freq: 1, field_mask: MASK_ALL, weight, term: Some(Rc::new(t)), ..Default::default() };
        let stores_fields = ii.flags() & super::invidx::flags::STORE_FIELD_FLAGS != 0;
        let filter = if stores_fields { RFilter::Mask(mask) } else { RFilter::None };
        let wide = ii.flags() & super::invidx::flags::WIDE_SCHEMA != 0;
        Some(Box::new(InvIt::new(ii, res, filter, Some(Checker::Mask(mask, wide)), self.q)))
    }

    /// `TagIndex_OpenReader`.
    fn open_tag(&self, fi: usize, value: &[u8], weight: f64) -> Option<BoxIt<'a>> {
        let tag = self.ix.fields.get(fi)?.tag.as_ref()?;
        let ii = tag.values.get(value)?;
        if ii.unique_docs() == 0 {
            return None;
        }
        let total = self.ix.stats.num_documents;
        let unique = ii.unique_docs() as u64;
        let t = Term { s: value.to_vec(), idf: iters::idf(total, unique), bm25_idf: iters::bm25_idf(total, unique) };
        let res = IxResult { kind: K_TERM, freq: 1, field_mask: MASK_ALL, weight, term: Some(Rc::new(t)), ..Default::default() };
        Some(Box::new(InvIt::new(ii, res, RFilter::None, Some(Checker::Field(fi, Predicate::Default)), self.q)))
    }

    fn union(&self, its: Vec<BoxIt<'a>>, quick: bool, weight: f64) -> BoxIt<'a> {
        iters::union(its, quick, self.min_heap, weight)
    }

    /// The text term expansions a trie walk yields (`runeIterCb`): capped at the max prefix
    /// expansions, with the warning.
    fn expansions(&mut self, mask: u128, walk: impl FnOnce(&mut dyn FnMut(&[Rune]) -> bool)) -> Vec<BoxIt<'a>> {
        let mut its = Vec::new();
        let mut warned = false;
        let max = self.max_expansions;
        let this = &*self;
        walk(&mut |r: &[Rune]| {
            if its.len() >= max {
                warned = true;
                return false;
            }
            let s = trie::runes_to_str(r).unwrap_or_default();
            if let Some(it) = this.open_term(&s, mask, 1.0) {
                its.push(it);
            }
            true
        });
        self.expansions_warning |= warned;
        its
    }

    fn eval(&mut self, n: &Node, and_mask: u128) -> Result<Option<BoxIt<'a>>, QErr> {
        let mask = n.opts.field_mask & and_mask;
        let weight = n.opts.weight;
        Ok(Some(match &n.kind {
            Kind::Null => Box::new(Empty),
            Kind::Wildcard => iters::wildcard(self.ix.existing.as_ref(), self.index_all(), self.max_doc(), weight, self.q),
            Kind::Ids(keys) => {
                let mut ids: Vec<u64> = keys.iter().map(|k| self.ix.docs.get_id(k)).filter(|&d| d != 0).collect();
                ids.sort_unstable();
                ids.dedup();
                Box::new(IdList::new(ids, IxResult { weight: 1.0, kind: iters::K_VIRTUAL, ..Default::default() }))
            }
            Kind::Missing { field } => {
                let Some(fi) = *field else { return Ok(None) };
                let fs = &self.ix.spec.fields[fi];
                let Some(ii) = self.ix.missing.get(&fs.name) else { return Ok(None) };
                let res = IxResult { kind: iters::K_VIRTUAL, freq: 1, field_mask: MASK_ALL, weight: 0.0, ..Default::default() };
                Box::new(InvIt::new(ii, res, RFilter::None, Some(Checker::Field(fi, Predicate::Missing)), self.q))
            }
            Kind::Optional => {
                let c = self.child(&n.children[0], MASK_ALL)?;
                iters::optional(c, self.ix.existing.as_ref(), self.index_all(), self.max_doc(), weight, self.q)
            }
            Kind::Not => {
                let prev = std::mem::replace(&mut self.in_not, true);
                let c = self.child(&n.children[0], MASK_ALL);
                self.in_not = prev;
                iters::not(c?, self.ix.existing.as_ref(), self.index_all(), self.max_doc(), weight, self.q)
            }
            Kind::Phrase { exact } => {
                if n.children.len() == 1 {
                    return self.eval(&n.children[0], mask);
                }
                let (max_slop, in_order) = if *exact {
                    (Some(0), true)
                } else {
                    let slop = if n.opts.max_slop == -1 { self.slop } else { n.opts.max_slop };
                    ((slop >= 0).then_some(slop as u32), self.in_order || n.opts.in_order)
                };
                let mut its = Vec::with_capacity(n.children.len());
                for c in &n.children {
                    its.push(self.child(c, mask)?);
                }
                iters::intersect(its, weight, self.prioritize, max_slop, in_order)
            }
            Kind::Union => {
                let quick = self.in_not || weight == 0.0;
                let mut its = Vec::with_capacity(n.children.len());
                for c in &n.children {
                    its.push(self.child(c, mask)?);
                }
                self.union(its, quick, weight)
            }
            Kind::Numeric(nf) => {
                let Some(fi) = nf.field else { return Ok(None) };
                let Some(tree) = self.ix.fields.get(fi).and_then(|f| f.numeric.as_ref()) else { return Ok(None) };
                let filter = NumericFilter { min: nf.min, max: nf.max, min_inclusive: nf.min_inclusive, max_inclusive: nf.max_inclusive, ..NumericFilter::default() };
                let ranges = tree.find(&filter);
                if ranges.is_empty() {
                    return Ok(None);
                }
                let its = ranges
                    .into_iter()
                    .map(|r| {
                        let contained = filter.value_in_range(r.min_val()) && filter.value_in_range(r.max_val());
                        let f = if contained { RFilter::None } else { RFilter::Numeric(filter) };
                        let res = IxResult { kind: K_NUMERIC, freq: 1, field_mask: MASK_ALL, weight: 1.0, ..Default::default() };
                        Box::new(InvIt::new(r.entries(), res, f, Some(Checker::Field(fi, Predicate::Default)), self.q)) as BoxIt<'a>
                    })
                    .collect();
                self.union(its, true, 1.0)
            }
            Kind::Geo(gf) => {
                gf.validate()?;
                let Some(fi) = gf.field else { return Ok(None) };
                let Some(tree) = self.ix.fields.get(fi).and_then(|f| f.numeric.as_ref()) else { return Ok(None) };
                if gf.radius <= 0.0 || !super::geo::valid(gf.lon, gf.lat) {
                    return Ok(None);
                }
                let factor = match gf.unit {
                    0 => 1000.0,
                    1 => 1.0,
                    2 => 0.3048,
                    _ => 1609.34,
                };
                let radius = gf.radius * factor;
                let mut its: Vec<BoxIt<'a>> = Vec::new();
                for (min, max) in super::geo::calc_ranges(gf.lon, gf.lat, radius) {
                    if min == max {
                        continue;
                    }
                    let filter = NumericFilter::range(min as f64, max as f64);
                    for r in tree.find(&filter) {
                        let res = IxResult { kind: K_NUMERIC, freq: 1, field_mask: MASK_ALL, weight: 1.0, ..Default::default() };
                        its.push(Box::new(InvIt::new(r.entries(), res, RFilter::Geo(gf.lon, gf.lat, radius), Some(Checker::Field(fi, Predicate::Default)), self.q)));
                    }
                }
                if its.is_empty() {
                    return Ok(None);
                }
                self.union(its, true, 1.0)
            }
            Kind::Token(t) => return Ok(self.open_term(&t.s, mask & self.req_mask, weight)),
            Kind::Prefix { tok, prefix, suffix } => {
                if tok.s.len() < self.min_prefix {
                    return Ok(None);
                }
                let kind = if *prefix && *suffix {
                    "INFIX"
                } else if *prefix {
                    "PREFIX"
                } else {
                    "SUFFIX"
                };
                let Some(runes) = trie::str_to_lower_runes(&tok.s) else {
                    return Err(QErr::new(Code::Limit, format!("{kind} {TOO_LONG}")));
                };
                if self.ix.suffix.is_some() && *suffix && !(n.opts.field_mask == MASK_ALL || self.ix.spec.suffix_mask & n.opts.field_mask == n.opts.field_mask) {
                    return Err(QErr::new(Code::Generic, "Contains query on fields without WITHSUFFIXTRIE support"));
                }
                let terms = &self.ix.term_trie;
                let (p, s) = (*prefix, *suffix);
                let its = self.expansions(self.req_mask & n.opts.field_mask, |cb| {
                    terms.iterate_contains(&runes, p, s, &mut |r, _, _| cb(r), None);
                });
                self.union(its, true, weight)
            }
            Kind::WildcardQuery(tok) => {
                let pat = remove_escape(&tok.s);
                let Some(runes) = trie::str_to_lower_runes(&pat) else {
                    return Err(QErr::new(Code::Limit, format!("Wildcard {TOO_LONG}")));
                };
                if self.ix.suffix.is_some() && !(n.opts.field_mask == MASK_ALL || self.ix.spec.suffix_mask & n.opts.field_mask == n.opts.field_mask) {
                    return Err(QErr::new(Code::Generic, "Contains query on fields without WITHSUFFIXTRIE support"));
                }
                let terms = &self.ix.term_trie;
                let its = self.expansions(self.req_mask & n.opts.field_mask, |cb| {
                    terms.iterate_wildcard(&runes, &mut |r, _, _| cb(r), None);
                });
                self.union(its, true, weight)
            }
            Kind::LexRange { begin, include_begin, end, include_end } => {
                let b = begin.as_ref().and_then(|s| trie::str_to_lower_runes(cstr(s)));
                let e = end.as_ref().and_then(|s| trie::str_to_lower_runes(cstr(s)));
                let terms = &self.ix.term_trie;
                let (ib, ie) = (*include_begin, *include_end);
                let its = self.expansions(self.req_mask & n.opts.field_mask, |cb| {
                    terms.iterate_range(b.as_deref(), ib, e.as_deref(), ie, &mut |r, _, _| cb(r));
                });
                self.union(its, true, weight)
            }
            Kind::Fuzzy { tok, max_dist } => {
                let s = cstr(&tok.s);
                let mask = self.req_mask & n.opts.field_mask;
                let Some(mut it) = self.ix.term_trie.iterate_fuzzy(s, *max_dist as i32, super::levenshtein::MatchMode::EditDistance) else { return Ok(None) };
                let mut its = Vec::new();
                let mut has_next;
                loop {
                    let e = it.next_entry();
                    has_next = e.is_some();
                    let Some(e) = e else { break };
                    if its.len() >= self.max_expansions {
                        break;
                    }
                    let term = trie::runes_to_str(e.runes).unwrap_or_default();
                    if let Some(r) = self.open_term(&term, mask, 1.0) {
                        its.push(r);
                    }
                }
                if has_next && its.len() == self.max_expansions {
                    self.expansions_warning = true;
                }
                if self.dialect >= 2 && s.len() <= *max_dist as usize
                    && let Some(r) = self.open_term(b"", mask, 1.0)
                {
                    its.push(r);
                }
                self.union(its, true, weight)
            }
            Kind::Tag { field } => {
                let Some(fi) = *field else { return Ok(None) };
                if self.ix.fields.get(fi).and_then(|f| f.tag.as_ref()).is_none() {
                    return Ok(None);
                }
                if n.children.len() == 1 {
                    return Ok(self.single_tag(fi, &n.children[0], weight));
                }
                let its: Vec<BoxIt<'a>> = n.children.iter().filter_map(|c| self.single_tag(fi, c, weight)).collect();
                let quick = self.in_not || weight == 0.0;
                self.union(its, quick, weight)
            }
            Kind::Vector(_) | Kind::Geometry { .. } => return Err(QErr::new(Code::Generic, "Vector and geometry queries are not supported yet")),
        }))
    }

    /// `query_EvalSingleTagNode`.
    fn single_tag(&mut self, fi: usize, n: &Node, weight: f64) -> Option<BoxIt<'a>> {
        let fs = &self.ix.spec.fields[fi];
        let cs = fs.tag_case_sensitive;
        let idx = self.ix.fields[fi].tag.as_ref()?;
        let open_all = |this: &mut Self, keys: Vec<Vec<u8>>| {
            let mut its = Vec::new();
            let mut more = false;
            for k in keys {
                if its.len() >= this.max_expansions {
                    more = true;
                    break;
                }
                if let Some(it) = this.open_tag(fi, &k, 1.0) {
                    its.push(it);
                }
            }
            if more {
                this.expansions_warning = true;
            }
            its
        };
        match &n.kind {
            Kind::Token(t) => self.open_tag(fi, &tag_lower(&t.s, cs), weight),
            Kind::Phrase { .. } => {
                let parts: Vec<Vec<u8>> = n.children.iter().map(|c| if let Kind::Token(t) = &c.kind { tag_lower(&t.s, cs) } else { Vec::new() }).collect();
                self.open_tag(fi, &parts.join(&b' '), weight)
            }
            Kind::Prefix { tok, prefix, suffix } => {
                let s = tag_lower(&tok.s, cs);
                if s.len() < self.min_prefix {
                    return None;
                }
                let mut keys = Vec::new();
                idx.values.for_each(|k, _| {
                    let ok = match (*prefix, *suffix) {
                        (_, false) => k.starts_with(&s),
                        (true, true) => s.is_empty() || k.windows(s.len()).any(|w| w == &s[..]),
                        (false, true) => k.ends_with(&s),
                    };
                    if ok {
                        keys.push(k.to_vec());
                    }
                    true
                });
                let its = open_all(self, keys);
                Some(self.union(its, true, weight))
            }
            Kind::WildcardQuery(tok) => {
                let pat = remove_escape(&tag_lower(&tok.s, cs));
                let mut keys = Vec::new();
                idx.values.for_each(|k, _| {
                    if wildcard_match(&pat, k) {
                        keys.push(k.to_vec());
                    }
                    true
                });
                let its = open_all(self, keys);
                Some(self.union(its, true, weight))
            }
            Kind::LexRange { begin, include_begin, end, include_end } => {
                let b = begin.as_ref().map(|s| tag_lower(cstr(s), cs));
                let e = end.as_ref().map(|s| tag_lower(cstr(s), cs));
                let mut its = Vec::new();
                let mut keys = Vec::new();
                idx.values.for_each(|k, _| {
                    let lo = b.as_ref().is_none_or(|b| if *include_begin { k >= &b[..] } else { k > &b[..] });
                    let hi = e.as_ref().is_none_or(|e| if *include_end { k <= &e[..] } else { k < &e[..] });
                    if lo && hi {
                        keys.push(k.to_vec());
                    }
                    true
                });
                for k in keys {
                    if let Some(it) = self.open_tag(fi, &k, weight) {
                        its.push(it);
                    }
                }
                Some(self.union(its, true, n.opts.weight))
            }
            _ => None,
        }
    }
}

/// `tag_strtolower`: backslashes before punctuation and spaces dropped, then lowercased
/// unless case-sensitive.
pub fn tag_lower(s: &[u8], case_sensitive: bool) -> Vec<u8> {
    let is_esc = |c: u8| c.is_ascii_punctuation() || matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r');
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' && i + 1 < s.len() && is_esc(s[i + 1]) {
            out.push(s[i + 1]);
            i += 2;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    if case_sensitive {
        return out;
    }
    match std::str::from_utf8(&out) {
        Ok(t) => t.chars().flat_map(char::to_lowercase).collect::<String>().into_bytes(),
        Err(_) => out,
    }
}

/// `Wildcard_RemoveEscape`: every backslash dropped, the next byte kept.
pub fn remove_escape(s: &[u8]) -> Vec<u8> {
    let s = cstr(s);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'\\' {
            i += 1;
            if i >= s.len() {
                break;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

/// `Wildcard_MatchChar`'s full match: `?` any byte, `*` any run.
pub fn wildcard_match(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(tag_lower(b"Hello\\ World\\x", false), b"hello world\\x");
        assert_eq!(remove_escape(b"a\\*b\\"), b"a*b");
        assert!(wildcard_match(b"he*o", b"hello"));
        assert!(wildcard_match(b"h?llo", b"hello"));
        assert!(!wildcard_match(b"h?lo", b"hello"));
        assert!(wildcard_match(b"*", b""));
    }
}
