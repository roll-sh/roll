//! The highlighter result processor (`RPHighlighter`) and its fragmenter: HIGHLIGHT and
//! SUMMARIZE over a result's text fields. The query terms' positions (the result's offsets,
//! merged in position order) are mapped to bytes through the document's byte offsets, grouped
//! into scored fragments, and either tagged in the whole field or cut into the best fragments
//! with some context around them.

use super::areq::{MODE_HIGHLIGHT, MODE_SYNOPSIS, ReturnedField};
use super::doctable::ByteOffsets;
use super::invidx::OffsetsIter;
use super::iters::{IxResult, K_INTERSECT, K_TERM, K_UNION, Term};
use crate::util::cstr;

const EOF: u32 = u32::MAX;
/// `FragmentList_Init(&frags, 8, 6)`: the max token distance within a fragment and the
/// estimated word size.
const MAX_DISTANCE: u32 = 8;
const EST_WORD_SIZE: usize = 6;
/// `FRAGMENTIZE_TOKLEN_EXACT` (Chinese): a match is as long as its query term.
pub const TOKLEN_EXACT: u32 = 0x01;

/// `ToksepMap_g`.
pub fn is_toksep(c: u8) -> bool {
    matches!(
        c,
        b' ' | b'\t'
            | b','
            | b'.'
            | b'/'
            | b'('
            | b')'
            | b'{'
            | b'}'
            | b'['
            | b']'
            | b':'
            | b';'
            | b'~'
            | b'!'
            | b'@'
            | b'#'
            | b'$'
            | b'%'
            | b'^'
            | b'&'
            | b'*'
            | b'-'
            | b'='
            | b'+'
            | b'|'
            | b'\''
            | b'`'
            | b'"'
            | b'<'
            | b'>'
            | b'?'
    )
}

/// musl's `isspace`.
fn is_space(c: u8) -> bool {
    c == b' ' || (b'\t'..=b'\r').contains(&c)
}

// ---------------------------------------------------------------------------------------------
// Term offsets with their query terms (`RSIndexResult_IterateOffsets`)

enum TermOffsets<'a> {
    Empty,
    Term(OffsetsIter<'a>, &'a Term),
    /// `_RSAggregateOffsetIterator`: each child's next offset and term.
    Merge { children: Vec<TermOffsets<'a>>, offsets: Vec<u32>, terms: Vec<Option<&'a Term>> },
}

impl<'a> TermOffsets<'a> {
    fn of(r: &'a IxResult) -> Self {
        match r.kind {
            K_TERM => match &r.term {
                Some(t) => TermOffsets::Term(super::invidx::offsets(&r.offsets), t),
                None => TermOffsets::Empty,
            },
            K_UNION | K_INTERSECT if r.children.len() == 1 => Self::of(&r.children[0]),
            K_UNION | K_INTERSECT => {
                let mut children: Vec<TermOffsets<'a>> = r.children.iter().map(Self::of).collect();
                let mut terms = vec![None; children.len()];
                let offsets = children.iter_mut().zip(terms.iter_mut()).map(|(c, t)| c.next(t)).collect();
                TermOffsets::Merge { children, offsets, terms }
            }
            _ => TermOffsets::Empty,
        }
    }

    /// The next position (`EOF` at the end, leaving `t` as it was).
    fn next(&mut self, t: &mut Option<&'a Term>) -> u32 {
        match self {
            TermOffsets::Empty => EOF,
            TermOffsets::Term(it, term) => match it.next() {
                Some(p) => {
                    *t = Some(term);
                    p
                }
                None => EOF,
            },
            TermOffsets::Merge { children, offsets, terms } => {
                let mut min_idx = None;
                let mut min = EOF;
                for (i, &o) in offsets.iter().enumerate() {
                    if o < min {
                        min_idx = Some(i);
                        min = o;
                    }
                }
                if let Some(i) = min_idx {
                    *t = terms[i];
                    offsets[i] = children[i].next(&mut terms[i]);
                }
                min
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Byte offsets (`RSByteOffsetIterator`)

struct ByteIter<'a> {
    offs: &'a [u32],
    idx: usize,
    cur_pos: u32,
    end_pos: u32,
}

impl<'a> ByteIter<'a> {
    /// `RSByteOffset_Iterate`: positioned before the field's first token.
    fn new(bo: &'a ByteOffsets, ft_id: usize) -> Option<Self> {
        let &(_, first, last) = bo.fields.iter().find(|f| f.0 as u16 == ft_id as u16)?;
        let mut it = ByteIter { offs: &bo.offsets, idx: 0, cur_pos: 1, end_pos: last };
        while it.cur_pos < first && it.idx < it.offs.len() {
            it.idx += 1;
            it.cur_pos += 1;
        }
        it.cur_pos -= 1;
        Some(it)
    }

    fn next(&mut self) -> u32 {
        if self.idx >= self.offs.len() {
            return EOF;
        }
        self.cur_pos = self.cur_pos.wrapping_add(1);
        if self.cur_pos > self.end_pos {
            return EOF;
        }
        let v = self.offs[self.idx];
        self.idx += 1;
        v
    }
}

/// `FragmentTerm`.
struct FragTerm {
    tok_pos: u32,
    byte_pos: u32,
    term_id: u32,
    len: u32,
    score: f32,
}

/// `FragmentTermIterator`.
struct FragIter<'a> {
    bytes: ByteIter<'a>,
    offs: TermOffsets<'a>,
    cur: Option<&'a Term>,
    cur_tok_pos: u32,
    cur_byte: u32,
}

impl<'a> FragIter<'a> {
    fn new(mut bytes: ByteIter<'a>, mut offs: TermOffsets<'a>) -> Self {
        let cur_byte = bytes.next();
        let mut cur = None;
        let mut cur_tok_pos;
        loop {
            cur_tok_pos = offs.next(&mut cur);
            if bytes.cur_pos <= cur_tok_pos {
                break;
            }
        }
        FragIter { bytes, offs, cur, cur_tok_pos, cur_byte }
    }

    /// `None` at the end; `Some(None)` for a token without a match.
    fn next(&mut self) -> Option<Option<FragTerm>> {
        let term = self.cur?;
        if self.cur_byte == EOF || self.cur_tok_pos == EOF {
            return None;
        }
        if self.bytes.cur_pos < self.cur_tok_pos {
            self.cur_byte = self.bytes.next();
            return Some(None);
        }
        let ft = FragTerm { score: term.idf as f32, term_id: term.id, len: term.s.len() as u32, tok_pos: self.cur_tok_pos, byte_pos: self.cur_byte };
        let next = self.offs.next(&mut self.cur);
        if next != self.cur_tok_pos {
            self.cur_byte = self.bytes.next();
        }
        self.cur_tok_pos = next;
        Some(Some(ft))
    }
}

// ---------------------------------------------------------------------------------------------
// Fragments (`FragmentList`)

/// `TermLoc`.
struct Loc {
    offset: u32,
    len: u16,
    term_id: u16,
}

/// `Fragment`.
struct Frag {
    buf: usize,
    len: u32,
    last_match_pos: u32,
    total_tokens: u32,
    num_matches: u32,
    pos: usize,
    score: f32,
    locs: Vec<Loc>,
}

struct FragList<'d> {
    doc: &'d [u8],
    frags: Vec<Frag>,
    toks_since_last: u32,
}

impl<'d> FragList<'d> {
    /// `FragmentList_AddMatchingTerm`.
    fn add_match(&mut self, term_id: u32, tok_pos: u32, tok: usize, tok_len: usize, score: f32) {
        let new = match self.frags.last() {
            Some(f) => tok_pos.wrapping_sub(f.last_match_pos) > MAX_DISTANCE,
            None => true,
        };
        if new {
            let pos = self.frags.len();
            self.frags.push(Frag { buf: tok, len: 0, last_match_pos: 0, total_tokens: 0, num_matches: 0, pos, score: 0.0, locs: Vec::new() });
            self.toks_since_last = 0;
        }
        let toks = self.toks_since_last;
        let f = self.frags.last_mut().expect("a fragment");
        if !f.locs.iter().any(|l| l.term_id as u32 == term_id) {
            f.score += score;
        }
        f.len = ((tok - f.buf) + tok_len) as u32;
        f.last_match_pos = tok_pos;
        f.num_matches = f.num_matches.wrapping_add(1);
        f.total_tokens = f.total_tokens.wrapping_add(toks.wrapping_add(1));
        self.toks_since_last = 0;
        f.locs.push(Loc { term_id: term_id as u16, offset: (tok - f.buf) as u32, len: tok_len as u16 });
    }

    /// `FragmentList_FragmentizeIter`.
    fn fragmentize(&mut self, it: &mut FragIter<'_>, options: u32) {
        let doc = self.doc;
        let mut last_tok_pos: Option<u32> = None;
        let mut last_byte_end = 0usize;
        while let Some(t) = it.next() {
            let Some(t) = t else {
                self.toks_since_last = self.toks_since_last.wrapping_add(1);
                continue;
            };
            if last_tok_pos == Some(t.tok_pos) {
                continue;
            }
            let byte_pos = t.byte_pos as usize;
            if byte_pos < last_byte_end {
                continue;
            }
            let start = byte_pos.min(doc.len());
            let len = if options & TOKLEN_EXACT != 0 { t.len as usize } else { doc[start..].iter().take_while(|&&c| !is_toksep(c)).count() };
            self.add_match(t.term_id, t.tok_pos, start, len, t.score);
            last_tok_pos = Some(t.tok_pos);
            last_byte_end = byte_pos + len;
        }
    }

    fn slice(&self, from: usize, len: usize) -> &'d [u8] {
        let from = from.min(self.doc.len());
        &self.doc[from..(from + len).min(self.doc.len())]
    }

    /// `Fragment_WriteIovs`.
    fn write_frag(&self, f: &Frag, tags: (&[u8], &[u8]), out: &mut Vec<u8>, preamble: &mut usize) {
        for l in &f.locs {
            let start = f.buf + l.offset as usize;
            if start > *preamble {
                out.extend_from_slice(self.slice(*preamble, start - *preamble));
            }
            out.extend_from_slice(tags.0);
            out.extend_from_slice(self.slice(start, l.len as usize));
            out.extend_from_slice(tags.1);
            *preamble = start + l.len as usize;
        }
    }

    /// `FragmentList_HighlightWholeDocS`.
    fn highlight_whole(&self, tags: (&[u8], &[u8])) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.doc.len());
        if self.frags.is_empty() {
            out.extend_from_slice(self.doc);
            return out;
        }
        let mut preamble = 0;
        for f in &self.frags {
            self.write_frag(f, tags, &mut out, &mut preamble);
        }
        if preamble < self.doc.len() {
            out.extend_from_slice(&self.doc[preamble..]);
        }
        out
    }

    /// `FragmentList_FindContext`: the context before and after a fragment, as byte ranges.
    fn context(&self, f: &Frag, before_limit: Option<usize>, after_limit: Option<usize>, context: usize) -> ((usize, usize), (usize, usize)) {
        let doc = self.doc;
        let mut lb = before_limit.unwrap_or(0) as i128;
        let mut la = after_limit.map_or(doc.len() as i128 - 1, |a| a as i128);
        let inside = f.total_tokens.wrapping_sub(f.num_matches) as usize;
        if context <= inside {
            return ((f.buf, 0), (f.buf + f.len as usize, 0));
        }
        let cs = (((context - inside) / 2) * EST_WORD_SIZE) as i128;
        let buf = f.buf as i128;
        let end = buf + f.len as i128;
        lb = lb.max(buf - cs);
        la = la.min(end + cs);
        let at = |i: i128| doc.get(i as usize).copied().unwrap_or(0);
        while lb < buf && !is_toksep(at(lb)) {
            lb += 1;
        }
        while lb < buf && is_toksep(at(lb)) {
            lb += 1;
        }
        while la > end && !is_toksep(at(la)) {
            la -= 1;
        }
        while la > end && is_toksep(at(la)) {
            la -= 1;
        }
        let after_len = (la - end + 1).max(0) as usize;
        ((lb as usize, (buf - lb) as usize), (end as usize, after_len))
    }

    /// `FragmentList_HighlightFragments` in `HIGHLIGHT_ORDER_SCOREPOS`: the best fragments in
    /// document order, each with its context.
    fn highlight_fragments(&self, tags: (&[u8], &[u8]), context: usize, n: usize) -> Vec<Vec<u8>> {
        let n = n.min(self.frags.len());
        let mut order: Vec<usize> = (0..self.frags.len()).collect();
        order.sort_by(|&a, &b| {
            let (x, y) = (self.frags[a].score, self.frags[b].score);
            if x == y {
                a.cmp(&b)
            } else if x > y {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        });
        order.truncate(n);
        order.sort_by_key(|&i| self.frags[i].pos);
        let mut out = Vec::with_capacity(n);
        for (ii, &fi) in order.iter().enumerate() {
            let f = &self.frags[fi];
            let before_limit = (ii > 0).then(|| {
                let p = &self.frags[order[ii - 1]];
                p.buf + p.len as usize
            });
            let after_limit = (ii + 1 < n).then(|| self.frags[order[ii + 1]].buf);
            let ((bs, bl), (as_, al)) = self.context(f, before_limit, after_limit, context);
            let mut v = Vec::new();
            v.extend_from_slice(self.slice(bs, bl));
            let mut preamble = f.buf;
            self.write_frag(f, tags, &mut v, &mut preamble);
            v.extend_from_slice(self.slice(as_, al));
            out.push(v);
        }
        out
    }
}

/// `stripDuplicateSpaces`: every whitespace run becomes one space.
fn strip_spaces(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut last_space = false;
    for &c in s {
        if is_space(c) {
            if !last_space {
                last_space = true;
                out.push(b' ');
            }
        } else {
            last_space = false;
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Per-field settings and the processor

/// A field's combined settings (`normalizeSettings`): no tags when highlighting was not set.
#[derive(Clone, Debug)]
pub struct Settings {
    pub mode: u8,
    tags: Option<(Vec<u8>, Vec<u8>)>,
    context_len: u32,
    num_frags: u16,
    separator: Option<Vec<u8>>,
}

fn tags_of(f: &ReturnedField) -> Option<(Vec<u8>, Vec<u8>)> {
    if f.mode & MODE_HIGHLIGHT == 0 {
        return None;
    }
    let open = f.open_tag.as_deref().map_or(&b"<b>"[..], cstr).to_vec();
    let close = f.close_tag.as_deref().map_or(&b"</b>"[..], cstr).to_vec();
    Some((open, close))
}

fn summarize_of(f: &ReturnedField) -> (u32, u16, Option<Vec<u8>>) {
    if f.mode & MODE_SYNOPSIS == 0 {
        return (0, 0, None);
    }
    (f.context_len, f.num_frags as u16, Some(f.separator.as_deref().map_or(&b"... "[..], cstr).to_vec()))
}

/// `normalizeSettings`: a field's own settings over the defaults (`None`: the defaults).
pub fn normalize(src: Option<&ReturnedField>, def: &ReturnedField) -> Settings {
    let Some(s) = src else {
        let (context_len, num_frags, separator) = summarize_of(def);
        return Settings { mode: def.mode, tags: tags_of(def), context_len, num_frags, separator };
    };
    let tags = if def.mode & MODE_HIGHLIGHT != 0 && s.mode & MODE_HIGHLIGHT == 0 {
        tags_of(def)
    } else if s.mode != 0 {
        tags_of(s)
    } else {
        None
    };
    let (context_len, num_frags, separator) = if def.mode & MODE_SYNOPSIS != 0 && s.mode & MODE_SYNOPSIS == 0 { summarize_of(def) } else { summarize_of(s) };
    Settings { mode: def.mode | s.mode, tags, context_len, num_frags, separator }
}

/// `trimField`: without fragments, a synopsis is the field's head, cut back to a separator.
fn trim_field(st: &Settings, doc: &[u8]) -> Vec<u8> {
    let head = EST_WORD_SIZE * st.context_len as usize * st.num_frags as usize + EST_WORD_SIZE;
    let head = head.min(doc.len());
    let mut buf = strip_spaces(&doc[..head]);
    while buf.len() > 1 {
        if is_toksep(buf[buf.len() - 1]) {
            break;
        }
        buf.pop();
    }
    while buf.last().is_some_and(|&c| is_space(c)) {
        buf.pop();
    }
    buf
}

/// `summarizeField`: the field's new value, `None` to keep it as it is.
pub fn summarize_field(st: &Settings, ft_id: Option<usize>, doc: &[u8], ir: &IxResult, bo: Option<&ByteOffsets>, options: u32) -> Option<Vec<u8>> {
    let mut frags = FragList { doc, frags: Vec::new(), toks_since_last: 0 };
    let fragmented = match (bo, ft_id) {
        (Some(bo), Some(id)) => match ByteIter::new(bo, id) {
            Some(bytes) => {
                let mut it = FragIter::new(bytes, TermOffsets::of(ir));
                frags.fragmentize(&mut it, options);
                !frags.frags.is_empty()
            }
            None => false,
        },
        _ => false,
    };
    if !fragmented {
        if st.mode == MODE_SYNOPSIS {
            return Some(trim_field(st, doc));
        }
        return None;
    }
    let empty: &[u8] = b"";
    let tags = st.tags.as_ref().map_or((empty, empty), |(o, c)| (&o[..], &c[..]));
    if st.mode == MODE_HIGHLIGHT {
        let mut out = frags.highlight_whole(tags);
        // read back as a C string
        let n = cstr(&out).len();
        out.truncate(n);
        return Some(out);
    }
    let sep = st.separator.as_deref().unwrap_or(b"");
    let mut out = Vec::new();
    for f in frags.highlight_fragments(tags, st.context_len as usize, st.num_frags as usize) {
        out.extend_from_slice(&strip_spaces(&f));
        out.extend_from_slice(sep);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn term_result(s: &str, id: u32, positions: &[u32]) -> IxResult {
        let mut w = super::super::invidx::OffsetsWriter::new();
        for &p in positions {
            w.write(p);
        }
        IxResult { kind: K_TERM, term: Some(Rc::new(Term { s: s.as_bytes().to_vec(), idf: 1.0, bm25_idf: 1.0, id })), offsets: w.bytes().to_vec(), ..Default::default() }
    }

    fn offsets_of(doc: &str) -> ByteOffsets {
        let mut offs = Vec::new();
        let b = doc.as_bytes();
        let mut i = 0;
        while i < b.len() {
            while i < b.len() && b[i] == b' ' {
                i += 1;
            }
            if i < b.len() {
                offs.push(i as u32);
            }
            while i < b.len() && b[i] != b' ' {
                i += 1;
            }
        }
        ByteOffsets { fields: vec![(0, 1, offs.len() as u32)], offsets: offs }
    }

    fn hl() -> ReturnedField {
        ReturnedField { mode: MODE_HIGHLIGHT, ..Default::default() }
    }

    #[test]
    fn highlights_whole_field() {
        let doc = "the quick brown fox";
        let bo = offsets_of(doc);
        let ir = term_result("quick", 0, &[2]);
        let st = normalize(None, &hl());
        let out = summarize_field(&st, Some(0), doc.as_bytes(), &ir, Some(&bo), 0).unwrap();
        assert_eq!(out, b"the <b>quick</b> brown fox");
    }

    #[test]
    fn summarizes_and_trims() {
        let doc = "a b c d e f g h i j k l m n o p q r s t u v w x y z";
        let bo = offsets_of(doc);
        let ir = term_result("m", 0, &[13]);
        let def = ReturnedField { mode: MODE_SYNOPSIS, context_len: 4, num_frags: 3, ..Default::default() };
        let st = normalize(None, &def);
        let out = summarize_field(&st, Some(0), doc.as_bytes(), &ir, Some(&bo), 0).unwrap();
        assert_eq!(out, b"h i j k l m n o p q r s... ");
        // no matches: the head, cut back to a separator
        let none = term_result("zz", 0, &[]);
        let out = summarize_field(&st, Some(0), doc.as_bytes(), &none, Some(&bo), 0).unwrap();
        assert_eq!(out, &doc.as_bytes()[..doc.len() - 2]);
    }

    #[test]
    fn merges_children() {
        let doc = "one two three two one";
        let bo = offsets_of(doc);
        let a = term_result("one", 0, &[1, 5]);
        let b = term_result("two", 1, &[2, 4]);
        let ir = IxResult { kind: K_UNION, children: vec![a, b], ..Default::default() };
        let st = normalize(None, &hl());
        let out = summarize_field(&st, Some(0), doc.as_bytes(), &ir, Some(&bo), 0).unwrap();
        assert_eq!(out, b"<b>one</b> <b>two</b> three <b>two</b> <b>one</b>");
    }
}
