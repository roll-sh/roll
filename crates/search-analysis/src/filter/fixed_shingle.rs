//! Lucene's `FixedShingleFilter` (over its `GraphTokenFilter` base) and the analyzer wrappers
//! Elasticsearch builds from it for `index_phrases` and `search_as_you_type`.
//!
//! Unlike `shingle`, a fixed shingle filter emits shingles of exactly one size and never
//! unigrams, and it walks the token *graph*: when the input has stacked tokens (synonyms), every
//! route through the graph from a base token yields its own shingle. `GraphTokenFilter` keeps
//! the tokens read ahead of the base token in a window; a route is a list of window indices,
//! advanced the way Lucene advances it (the next route changes the deepest token that has a
//! sibling stacked after it). Tokens in the window are recycled through a pool, so steady-state
//! shingling does not allocate.
//!
//! `TrailingShingle` is `SearchAsYouTypeAnalyzer.TrailingShingleTokenFilter`: it reports extra
//! trailing positions at the end of the stream, so the fixed shingle filter also emits the
//! shorter shingles at the end of the text (padded with empty fillers) for prefix indexing.

use std::collections::VecDeque;

use crate::error::AnalysisError;
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;

/// `GraphTokenFilter.MAX_GRAPH_STACK_SIZE`.
const MAX_GRAPH_STACK_SIZE: usize = 1000;
/// `GraphTokenFilter.MAX_TOKEN_CACHE_SIZE`.
const MAX_TOKEN_CACHE_SIZE: usize = 100;
/// `FixedShingleFilter.MAX_SHINGLE_SIZE`.
pub(crate) const MAX_SHINGLE_SIZE: usize = 4;

/// `FixedShingleFilter(input, size, separator, filler)`.
pub(crate) struct FixedShingle {
    size: usize,
    separator: &'static str,
    filler: &'static str,
    /// Tokens read from the input; `window[0]` has absolute index `first`.
    window: VecDeque<Tok>,
    pool: Vec<Tok>,
    first: usize,
    /// Absolute index of the base token.
    base: Option<usize>,
    /// The current route (absolute indices); only `..=depth` is meaningful.
    graph: Vec<usize>,
    depth: usize,
    pos: usize,
    /// The input's trailing position increment once it has ended (`-1` before).
    trailing: i64,
    final_offset: u32,
    stack_size: usize,
    buffer: String,
    end_tok: Tok,
    error: Option<Box<AnalysisError>>,
}

impl FixedShingle {
    pub fn new(size: usize, separator: &'static str, filler: &'static str) -> FixedShingle {
        FixedShingle {
            size,
            separator,
            filler,
            window: VecDeque::new(),
            pool: Vec::new(),
            first: 0,
            base: None,
            graph: Vec::new(),
            depth: 0,
            pos: 0,
            trailing: -1,
            final_offset: 0,
            stack_size: 0,
            buffer: String::new(),
            end_tok: Tok::default(),
            error: None,
        }
    }

    fn tok(&self, i: usize) -> &Tok {
        &self.window[i - self.first]
    }

    /// `nextTokenInStream`: the token read after `i` (reading it from the input if needed).
    fn next_in_stream(&mut self, input: &mut dyn TokenStream, i: Option<usize>) -> Option<usize> {
        let next = i.map_or(self.first, |i| i + 1);
        if next < self.first + self.window.len() {
            return Some(next);
        }
        if self.trailing != -1 || self.error.is_some() {
            return None;
        }
        let mut t = self.pool.pop().unwrap_or_default();
        if !input.next(&mut t) {
            if let Some(e) = t.error.take() {
                self.error = Some(e);
                self.pool.push(t);
                return None;
            }
            self.pool.push(t);
            self.end_tok.clear_for_end();
            input.end(&mut self.end_tok);
            self.trailing = self.end_tok.pos_inc as i64;
            self.final_offset = self.end_tok.end;
            return None;
        }
        self.window.push_back(t);
        if self.window.len() > MAX_TOKEN_CACHE_SIZE {
            self.error = Some(Box::new(AnalysisError::other(
                "illegal_state_exception",
                500,
                format!("Too many cached tokens (> {MAX_TOKEN_CACHE_SIZE})"),
            )));
            return None;
        }
        Some(self.first + self.window.len() - 1)
    }

    /// `nextTokenInGraph`: the token a position length after `i`.
    fn next_in_graph(&mut self, input: &mut dyn TokenStream, i: usize) -> Option<usize> {
        let mut remaining = self.tok(i).pos_len as i64;
        let mut cur = i;
        loop {
            cur = self.next_in_stream(input, Some(cur))?;
            remaining -= self.tok(cur).pos_inc as i64;
            if remaining <= 0 {
                return Some(cur);
            }
        }
    }

    /// `lastInStack`: no token stacked on `i` follows it.
    fn last_in_stack(&mut self, input: &mut dyn TokenStream, i: usize) -> bool {
        match self.next_in_stream(input, Some(i)) {
            None => true,
            Some(n) => self.tok(n).pos_inc != 0,
        }
    }

    /// `incrementBaseToken`.
    fn increment_base(&mut self, input: &mut dyn TokenStream) -> Option<usize> {
        self.stack_size = 0;
        self.depth = 0;
        self.pos = 0;
        let base = self.next_in_stream(input, self.base);
        self.base = base;
        let b = base?;
        self.graph.clear();
        self.graph.push(b);
        // Tokens before the base are unreachable now: recycle them.
        while self.first < b {
            if let Some(t) = self.window.pop_front() {
                self.pool.push(t);
            }
            self.first += 1;
        }
        Some(b)
    }

    /// `incrementGraphToken`: the next token on the current route.
    fn increment_graph_token(&mut self, input: &mut dyn TokenStream) -> Option<usize> {
        if self.pos < self.depth {
            self.pos += 1;
            return Some(self.graph[self.pos]);
        }
        let t = self.next_in_graph(input, self.graph[self.depth])?;
        self.depth += 1;
        self.pos += 1;
        self.graph.truncate(self.depth);
        self.graph.push(t);
        Some(t)
    }

    /// `incrementGraph`: back to the base token on the next route; false when there is none.
    fn increment_graph(&mut self, input: &mut dyn TokenStream) -> bool {
        if self.base.is_none() {
            return false;
        }
        self.pos = 0;
        for i in (1..=self.depth).rev() {
            if !self.last_in_stack(input, self.graph[i]) {
                let Some(n) = self.next_in_stream(input, Some(self.graph[i])) else {
                    return false;
                };
                self.graph[i] = n;
                // Lucene advances the deeper entries too (reading ahead as it does) before
                // cutting the route back to `i`.
                for j in i + 1..self.depth {
                    if let Some(n) = self.next_in_graph(input, self.graph[j]) {
                        self.graph[j] = n;
                    }
                }
                self.stack_size += 1;
                if self.stack_size > MAX_GRAPH_STACK_SIZE + 1 {
                    self.error = Some(Box::new(AnalysisError::other(
                        "illegal_state_exception",
                        500,
                        format!("Too many graph paths (> {MAX_GRAPH_STACK_SIZE})"),
                    )));
                    return false;
                }
                self.depth = i;
                return true;
            }
        }
        false
    }

    fn push_filler(&mut self) {
        self.buffer.push_str(self.separator);
        self.buffer.push_str(self.filler);
    }
}

impl Filter for FixedShingle {
    fn reset(&mut self) {
        while let Some(t) = self.window.pop_front() {
            self.pool.push(t);
        }
        self.first = 0;
        self.base = None;
        self.graph.clear();
        self.depth = 0;
        self.pos = 0;
        self.trailing = -1;
        self.final_offset = 0;
        self.stack_size = 0;
        self.error = None;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        let (start, mut end, mut shingle_pos_inc);
        'outer: loop {
            // A new base position carries its increment; another route from the same base is
            // stacked. (Lucene 10.5's version: a base that yields no shingle loses its
            // increment.)
            let base = if self.increment_graph(input) {
                shingle_pos_inc = 0;
                self.graph[0]
            } else {
                let Some(b) = self.increment_base(input) else {
                    if let Some(e) = self.error.take() {
                        t.error = Some(e);
                    }
                    return false;
                };
                shingle_pos_inc = self.tok(b).pos_inc;
                b
            };
            if let Some(e) = self.error.take() {
                t.error = Some(e);
                return false;
            }
            let bt = &self.window[base - self.first];
            let s = bt.start;
            end = bt.end;
            self.buffer.clear();
            self.buffer.push_str(&self.window[base - self.first].term);
            let mut i = 1;
            while i < self.size {
                let Some(g) = self.increment_graph_token(input) else {
                    if let Some(e) = self.error.take() {
                        t.error = Some(e);
                        return false;
                    }
                    // The end of the stream: pad with fillers if trailing positions allow a
                    // whole shingle, else try another route.
                    let trailing = self.trailing.max(0) as usize;
                    if i + trailing < self.size {
                        continue 'outer;
                    }
                    while i < self.size {
                        self.push_filler();
                        i += 1;
                    }
                    break;
                };
                let mut pos_inc = self.tok(g).pos_inc as usize;
                if pos_inc > 1 {
                    if i + pos_inc > self.size {
                        while i < self.size {
                            self.push_filler();
                            i += 1;
                        }
                        break;
                    }
                    while pos_inc > 1 {
                        self.push_filler();
                        pos_inc -= 1;
                        i += 1;
                    }
                }
                self.buffer.push_str(self.separator);
                let gt = &self.window[g - self.first];
                self.buffer.push_str(&gt.term);
                end = gt.end;
                i += 1;
            }
            start = s;
            break;
        }
        t.clear();
        t.start = start;
        t.end = end;
        t.pos_inc = shingle_pos_inc;
        t.term.push_str(&self.buffer);
        t.kind = "shingle";
        true
    }

    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        // `GraphTokenFilter.end`.
        if self.trailing == -1 {
            input.end(t);
            self.trailing = t.pos_inc as i64;
            self.final_offset = t.end;
        } else {
            t.clear_for_end();
            t.pos_inc = self.trailing as u32;
            t.start = self.final_offset;
            t.end = self.final_offset;
        }
    }
}

/// `SearchAsYouTypeAnalyzer.TrailingShingleTokenFilter`: the stream ends with `extra`
/// positions.
pub(crate) struct TrailingShingle {
    pub extra: u32,
}

impl Filter for TrailingShingle {
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        input.next(t)
    }

    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        input.end(t);
        t.pos_inc = self.extra;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::run_filters;

    fn shingles(text: &str, size: usize) -> Vec<(String, u32, u32, u32)> {
        run_filters(vec![Box::new(FixedShingle::new(size, " ", "_"))], text)
    }

    #[test]
    fn fixed_size_only() {
        assert_eq!(
            shingles("please divide this sentence", 2),
            vec![
                ("please divide".into(), 1, 0, 13),
                ("divide this".into(), 1, 7, 18),
                ("this sentence".into(), 1, 14, 27),
            ]
        );
        assert_eq!(shingles("one", 2), vec![]);
        assert_eq!(
            shingles("a b c d", 3),
            vec![("a b c".into(), 1, 0, 5), ("b c d".into(), 1, 2, 7)]
        );
    }
}
