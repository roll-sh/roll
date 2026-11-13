//! Token streams: tokenizers, token filters, char filters and the chain that runs them.
//!
//! The model is Lucene's pull model with one twist: a filter does not own its input. A chain is a
//! tokenizer plus a flat `Vec` of filters, and pulling a token from filter `i` hands it an
//! [`Upstream`] view of filters `0..i` and the tokenizer. That keeps every component a plain
//! `Box<dyn Filter>` with no shared ownership, lets `_analyze`'s explain run the same chain
//! truncated after any filter, and lets the branching filters (`condition`, `multiplexer`) feed
//! their sub-chains from a replay source without `Rc<RefCell<…>>`.
//!
//! Char filters run over the whole text before tokenization (the inputs here are in memory, so
//! Lucene's streaming `Reader` buys nothing) and record offset corrections the way Lucene's
//! `BaseCharFilter` does; the [`Source`] at the bottom of every chain applies them, last char
//! filter first, so token offsets always refer to the original text.

use crate::token::Tok;

/// Anything tokens can be pulled from.
pub(crate) trait TokenStream {
    /// Produces the next token into `t`; false at the end of the stream.
    fn next(&mut self, t: &mut Tok) -> bool;
    /// `TokenStream.end()`: `t` receives the final offset and trailing position increment.
    fn end(&mut self, t: &mut Tok);
}

/// Splits text into tokens. Offsets are UTF-16 offsets into the text it is given; the chain
/// corrects them for char filters. `t` has been cleared before each `next`.
pub(crate) trait Tokenizer: Send {
    fn reset(&mut self, text: &str);
    fn next(&mut self, text: &str, t: &mut Tok) -> bool;
    /// Sets the final offset (usually the UTF-16 length of `text`) and adds positions of any
    /// skipped tokens to `t.pos_inc` (which starts at 0).
    fn end(&mut self, text: &str, t: &mut Tok) {
        let end = crate::unicode::utf16_len(text) as u32;
        t.start = end;
        t.end = end;
    }
}

/// Transforms a token stream.
pub(crate) trait Filter: Send {
    /// Clears per-document state; called before each document.
    fn reset(&mut self) {}
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool;
    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        input.end(t);
    }
}

/// Rewrites text before tokenization.
pub(crate) trait CharFilter: Send {
    /// Writes the filtered `input` to `out` (cleared by the caller) and records offset
    /// corrections in `map` (cleared by the caller). Fails like Lucene's filter fails while
    /// reading (a `pattern_replace` replacement naming a missing group).
    fn filter(
        &mut self,
        input: &str,
        out: &mut String,
        map: &mut OffsetMap,
    ) -> Result<(), crate::error::AnalysisError>;
}

/// Lucene's `BaseCharFilter` offset correction table: at output offset `offs[i]` and beyond,
/// add `diffs[i]` to get the input offset. Offsets are UTF-16 units.
#[derive(Default, Debug, Clone)]
pub(crate) struct OffsetMap {
    offs: Vec<u32>,
    diffs: Vec<i32>,
}

impl OffsetMap {
    pub fn clear(&mut self) {
        self.offs.clear();
        self.diffs.clear();
    }

    /// `addOffCorrectMap(off, cumulativeDiff)`.
    pub fn add(&mut self, off: u32, cumulative_diff: i32) {
        if let Some(&last) = self.offs.last()
            && last == off
        {
            *self.diffs.last_mut().unwrap() = cumulative_diff;
            return;
        }
        self.offs.push(off);
        self.diffs.push(cumulative_diff);
    }

    pub fn last_diff(&self) -> i32 {
        self.diffs.last().copied().unwrap_or(0)
    }

    /// `correct(currentOff)`.
    #[inline]
    pub fn correct(&self, off: u32) -> u32 {
        if self.offs.is_empty() {
            return off;
        }
        let idx = self.offs.partition_point(|&o| o <= off);
        if idx == 0 {
            return off;
        }
        (off as i64 + self.diffs[idx - 1] as i64).max(0) as u32
    }
}

/// Applies the corrections of a char filter chain (the last filter's first).
#[inline]
pub(crate) fn correct(maps: &[OffsetMap], mut off: u32) -> u32 {
    for m in maps.iter().rev() {
        off = m.correct(off);
    }
    off
}

/// The bottom of a chain: the tokenizer over the char-filtered text.
pub(crate) struct Source<'a> {
    pub tokenizer: &'a mut dyn Tokenizer,
    pub text: &'a str,
    pub maps: &'a [OffsetMap],
}

impl TokenStream for Source<'_> {
    #[inline]
    fn next(&mut self, t: &mut Tok) -> bool {
        t.clear();
        if !self.tokenizer.next(self.text, t) {
            return false;
        }
        if !self.maps.is_empty() {
            t.start = correct(self.maps, t.start);
            t.end = correct(self.maps, t.end);
        }
        true
    }

    fn end(&mut self, t: &mut Tok) {
        t.clear_for_end();
        self.tokenizer.end(self.text, t);
        if !self.maps.is_empty() {
            t.start = correct(self.maps, t.start);
            t.end = correct(self.maps, t.end);
        }
    }
}

/// Filters `0..n` over a source, as one stream.
pub(crate) struct Upstream<'a> {
    pub filters: &'a mut [Box<dyn Filter>],
    pub src: &'a mut dyn TokenStream,
}

impl TokenStream for Upstream<'_> {
    #[inline]
    fn next(&mut self, t: &mut Tok) -> bool {
        match self.filters.split_last_mut() {
            None => self.src.next(t),
            Some((last, rest)) => last.next(
                &mut Upstream {
                    filters: rest,
                    src: &mut *self.src,
                },
                t,
            ),
        }
    }

    fn end(&mut self, t: &mut Tok) {
        match self.filters.split_last_mut() {
            None => self.src.end(t),
            Some((last, rest)) => last.end(
                &mut Upstream {
                    filters: rest,
                    src: &mut *self.src,
                },
                t,
            ),
        }
    }
}

/// What `end()` reported.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct End {
    pub final_offset: u32,
    pub pos_inc: u32,
}

/// One instance of an analysis chain. Not shared between threads; analyzers pool them.
pub(crate) struct Chain {
    pub char_filters: Vec<Box<dyn CharFilter>>,
    pub tokenizer: Box<dyn Tokenizer>,
    pub filters: Vec<Box<dyn Filter>>,
    /// Char filter outputs (ping-pong buffers).
    bufs: [String; 2],
    maps: Vec<OffsetMap>,
    pub tok: Tok,
}

impl Chain {
    pub fn new(
        char_filters: Vec<Box<dyn CharFilter>>,
        tokenizer: Box<dyn Tokenizer>,
        filters: Vec<Box<dyn Filter>>,
    ) -> Chain {
        let maps = char_filters.iter().map(|_| OffsetMap::default()).collect();
        Chain {
            char_filters,
            tokenizer,
            filters,
            bufs: [String::new(), String::new()],
            maps,
            tok: Tok::default(),
        }
    }

    /// Runs the char filters over `text`, calling `stage(i, output)` after each (for explain).
    /// Returns the index of the buffer holding the result, or None when there are no char filters.
    fn char_filter(
        &mut self,
        text: &str,
        stage: &mut dyn FnMut(usize, &str),
    ) -> Result<Option<usize>, crate::error::AnalysisError> {
        if self.char_filters.is_empty() {
            return Ok(None);
        }
        let mut cur: Option<usize> = None;
        for (i, cf) in self.char_filters.iter_mut().enumerate() {
            let dst = match cur {
                Some(0) => 1,
                _ => 0,
            };
            let [b0, b1] = &mut self.bufs;
            let (input, out): (&str, &mut String) = match (cur, dst) {
                (None, 0) => (text, b0),
                (Some(1), 0) => (b1.as_str(), b0),
                (Some(0), 1) => (b0.as_str(), b1),
                _ => unreachable!(),
            };
            out.clear();
            self.maps[i].clear();
            cf.filter(input, out, &mut self.maps[i])?;
            stage(i, out);
            cur = Some(dst);
        }
        Ok(cur)
    }

    /// The UTF-16 length of `text` after the char filters.
    pub fn filtered_utf16_len(&mut self, text: &str) -> Result<usize, crate::error::AnalysisError> {
        Ok(match self.char_filter(text, &mut |_, _| {})? {
            None => crate::unicode::utf16_len(text),
            Some(i) => crate::unicode::utf16_len(&self.bufs[i]),
        })
    }

    /// Analyzes `text` through the tokenizer and the first `nfilters` filters, calling `sink` for
    /// every token. `cf_stage` sees each char filter's output.
    pub fn run(
        &mut self,
        text: &str,
        nfilters: usize,
        cf_stage: &mut dyn FnMut(usize, &str),
        sink: &mut dyn FnMut(&Tok),
    ) -> Result<End, crate::error::AnalysisError> {
        let buf = self.char_filter(text, cf_stage)?;
        let Chain {
            tokenizer,
            filters,
            bufs,
            maps,
            tok,
            ..
        } = self;
        let text: &str = match buf {
            None => text,
            Some(i) => &bufs[i],
        };
        tokenizer.reset(text);
        let nfilters = nfilters.min(filters.len());
        for f in filters[..nfilters].iter_mut() {
            f.reset();
        }
        let mut src = Source {
            tokenizer: &mut **tokenizer,
            text,
            maps,
        };
        let mut up = Upstream {
            filters: &mut filters[..nfilters],
            src: &mut src,
        };
        tok.error = None;
        while up.next(tok) {
            sink(tok);
        }
        if let Some(e) = tok.error.take() {
            return Err(*e);
        }
        tok.clear_for_end();
        up.end(tok);
        Ok(End {
            final_offset: tok.end,
            pos_inc: tok.pos_inc,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A tokenizer for tests: splits on ASCII spaces.
    pub(crate) struct Spaces {
        pos: usize,
    }

    impl Spaces {
        pub fn new() -> Spaces {
            Spaces { pos: 0 }
        }
    }

    impl Tokenizer for Spaces {
        fn reset(&mut self, _text: &str) {
            self.pos = 0;
        }
        fn next(&mut self, text: &str, t: &mut Tok) -> bool {
            let b = text.as_bytes();
            while self.pos < b.len() && b[self.pos] == b' ' {
                self.pos += 1;
            }
            if self.pos >= b.len() {
                return false;
            }
            let start = self.pos;
            while self.pos < b.len() && b[self.pos] != b' ' {
                self.pos += 1;
            }
            t.term.clear();
            t.term.push_str(&text[start..self.pos]);
            t.start = start as u32;
            t.end = self.pos as u32;
            true
        }
    }

    struct Upper;
    impl Filter for Upper {
        fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
            if !input.next(t) {
                return false;
            }
            t.term.make_ascii_uppercase();
            true
        }
    }

    struct Dup;
    impl CharFilter for Dup {
        // "ab" -> "aabb": output offset 2k maps to input offset k.
        fn filter(
            &mut self,
            input: &str,
            out: &mut String,
            map: &mut OffsetMap,
        ) -> Result<(), crate::error::AnalysisError> {
            for (i, c) in input.chars().enumerate() {
                out.push(c);
                out.push(c);
                map.add(2 * i as u32 + 1, -(i as i32) - 1);
                map.add(2 * i as u32 + 2, -(i as i32) - 1);
            }
            Ok(())
        }
    }

    #[test]
    fn chain_runs_filters_and_corrects_offsets() {
        let mut chain = Chain::new(
            vec![Box::new(Dup)],
            Box::new(Spaces::new()),
            vec![Box::new(Upper)],
        );
        let mut out = Vec::new();
        let mut stages = Vec::new();
        let end = chain
            .run(
                "ab c",
                1,
                &mut |i, s| stages.push((i, s.to_string())),
                &mut |t| out.push((t.term.clone(), t.start, t.end)),
            )
            .unwrap();
        assert_eq!(stages, vec![(0, "aabb  cc".to_string())]);
        assert_eq!(
            out,
            vec![("AABB".to_string(), 0, 2), ("CC".to_string(), 3, 4)]
        );
        assert_eq!(end.final_offset, 4);
        // Without the filter.
        let mut out = Vec::new();
        chain
            .run("x", 0, &mut |_, _| {}, &mut |t| out.push(t.term.clone()))
            .unwrap();
        assert_eq!(out, vec!["xx"]);
    }

    #[test]
    fn offset_map_matches_base_char_filter() {
        let mut m = OffsetMap::default();
        assert_eq!(m.correct(5), 5);
        m.add(2, 3);
        m.add(4, 1);
        m.add(4, 2);
        assert_eq!(m.correct(1), 1);
        assert_eq!(m.correct(2), 5);
        assert_eq!(m.correct(3), 6);
        assert_eq!(m.correct(4), 6);
        assert_eq!(m.correct(10), 12);
    }
}
