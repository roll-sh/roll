//! `ngram` and `edge_ngram` token filters (Lucene's `NGramTokenFilter` and
//! `EdgeNGramTokenFilter`).
//!
//! Grams are counted in code points. Every gram keeps the attributes of its token (offsets, type,
//! keyword flag: Lucene restores the captured state); the first gram of a token carries the
//! token's position increment and the others are stacked on it. The input token is kept in a
//! reused [`Tok`] and grams are cut from it by byte ranges, so nothing is allocated per gram.

use std::sync::Arc;

use crate::component::{FilterFactory, SynonymRole, filter_with};
use crate::error::{AnalysisError, Result};
use crate::registry::Ctx;
use crate::settings::Settings;
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;

/// `NGramTokenFilter` / `EdgeNGramTokenFilter`.
pub(crate) struct NGram {
    min: usize,
    max: usize,
    edge: bool,
    preserve: bool,
    /// The current input token (None: fetch the next one).
    active: bool,
    cur: Tok,
    /// Byte offset of each code point of `cur.term`, plus the end.
    bounds: Vec<usize>,
    pos: usize,
    gram: usize,
    pos_inc: u32,
}

impl NGram {
    pub fn new(min: usize, max: usize, edge: bool, preserve: bool) -> NGram {
        NGram {
            min,
            max,
            edge,
            preserve,
            active: false,
            cur: Tok::default(),
            bounds: Vec::new(),
            pos: 0,
            gram: 0,
            pos_inc: 0,
        }
    }

    fn count(&self) -> usize {
        self.bounds.len() - 1
    }

    fn emit(&mut self, t: &mut Tok, from: usize, to: usize) {
        t.clone_from(&self.cur);
        t.term.clear();
        t.term
            .push_str(&self.cur.term[self.bounds[from]..self.bounds[to]]);
    }
}

impl Filter for NGram {
    fn reset(&mut self) {
        self.active = false;
        self.pos_inc = 0;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        loop {
            if !self.active {
                if !input.next(t) {
                    return false;
                }
                self.bounds.clear();
                self.bounds.extend(t.term.char_indices().map(|(i, _)| i));
                self.bounds.push(t.term.len());
                self.pos_inc += t.pos_inc;
                if self.preserve && self.count() < self.min {
                    // Shorter than min_gram but kept.
                    t.pos_inc = self.pos_inc;
                    self.pos_inc = 0;
                    return true;
                }
                self.cur.clone_from(t);
                self.active = true;
                self.pos = 0;
                self.gram = self.min;
            }
            let n = self.count();
            if self.edge {
                if self.gram <= n {
                    if self.gram <= self.max {
                        let g = self.gram;
                        self.emit(t, 0, g);
                        t.pos_inc = self.pos_inc;
                        self.pos_inc = 0;
                        self.gram += 1;
                        return true;
                    } else if self.preserve {
                        t.clone_from(&self.cur);
                        t.pos_inc = 0;
                        self.active = false;
                        return true;
                    }
                }
            } else {
                if self.gram > self.max || self.pos + self.gram > n {
                    self.pos += 1;
                    self.gram = self.min;
                }
                if self.pos + self.gram <= n {
                    let (from, to) = (self.pos, self.pos + self.gram);
                    self.emit(t, from, to);
                    t.pos_inc = self.pos_inc;
                    self.pos_inc = 0;
                    self.gram += 1;
                    return true;
                } else if self.preserve && n > self.max {
                    t.clone_from(&self.cur);
                    t.pos_inc = 0;
                    self.active = false;
                    return true;
                }
            }
            self.active = false;
        }
    }

    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        input.end(t);
        t.pos_inc = self.pos_inc;
    }
}

/// Lucene's constructor checks, which Elasticsearch hits when it builds the analyzer.
fn check(min: i32, max: i32) -> Result<()> {
    if min < 1 {
        return Err(AnalysisError::illegal("minGram must be greater than zero"));
    }
    if min > max {
        return Err(AnalysisError::illegal(
            "minGram must not be greater than maxGram",
        ));
    }
    Ok(())
}

struct NGramFactory {
    min: i32,
    max: i32,
    edge: bool,
    back: bool,
    preserve: bool,
}

impl FilterFactory for NGramFactory {
    fn create(&self) -> Box<dyn Filter> {
        let (min, max) = (self.min.max(1) as usize, self.max.max(1) as usize);
        let ng = NGram::new(min, max, self.edge, self.preserve);
        if self.back {
            Box::new(Reversed {
                inner: ng,
                scratch: String::new(),
            })
        } else {
            Box::new(ng)
        }
    }
    fn check(&self) -> Result<()> {
        check(self.min, self.max)
    }
    fn synonym_role(&self) -> SynonymRole {
        SynonymRole::Forbidden
    }
}

/// `side: back`: the edge n-grams of the reversed term, reversed back.
struct Reversed {
    inner: NGram,
    scratch: String,
}

impl Filter for Reversed {
    fn reset(&mut self) {
        self.inner.reset();
    }
    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        let mut rev = ReverseInput {
            input,
            scratch: &mut self.scratch,
        };
        if !self.inner.next(&mut rev, t) {
            return false;
        }
        super::basic::reverse(t, &mut self.scratch);
        true
    }
    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        self.inner.end(input, t);
    }
}

struct ReverseInput<'a> {
    input: &'a mut dyn TokenStream,
    scratch: &'a mut String,
}

impl TokenStream for ReverseInput<'_> {
    fn next(&mut self, t: &mut Tok) -> bool {
        if !self.input.next(t) {
            return false;
        }
        super::basic::reverse(t, self.scratch);
        true
    }
    fn end(&mut self, t: &mut Tok) {
        self.input.end(t);
    }
}

/// `ngram` (`NGramTokenFilterFactory`).
pub(crate) fn ngram_factory(ctx: &Ctx, s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let min = s.get_int("min_gram", 1)?;
    let max = s.get_int("max_gram", 2)?;
    let diff = max - min;
    if diff > ctx.opts.max_ngram_diff {
        return Err(AnalysisError::illegal(format!(
            "The difference between max_gram and min_gram in NGram Tokenizer must be less than or equal to: [{}] but was [{diff}]. This limit can be set by changing the [index.max_ngram_diff] index level setting.",
            ctx.opts.max_ngram_diff
        )));
    }
    let preserve = s.get_bool("preserve_original", false)?;
    Ok(Arc::new(NGramFactory {
        min,
        max,
        edge: false,
        back: false,
        preserve,
    }))
}

/// `edge_ngram` (`EdgeNGramTokenFilterFactory`).
pub(crate) fn edge_ngram_factory(s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let min = s.get_int("min_gram", 1)?;
    let max = s.get_int("max_gram", 2)?;
    let back = match s.get("side").as_deref() {
        None | Some("front") => false,
        Some("back") => true,
        Some(other) => return Err(AnalysisError::illegal(format!("invalid side: {other}"))),
    };
    let preserve = s.get_bool("preserve_original", false)?;
    Ok(Arc::new(NGramFactory {
        min,
        max,
        edge: true,
        back,
        preserve,
    }))
}

/// The pre-configured `ngram` (1–2, no originals) and `edge_ngram` (1–1).
pub(crate) fn prebuilt(edge: bool) -> Arc<dyn FilterFactory> {
    if edge {
        Arc::new(NGramFactory {
            min: 1,
            max: 1,
            edge: true,
            back: false,
            preserve: false,
        })
    } else {
        filter_with(false, 0, SynonymRole::Forbidden, || {
            Box::new(NGram::new(1, 2, false, false))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::run_filters;

    #[test]
    fn grams_and_positions() {
        let out = run_filters(vec![Box::new(NGram::new(1, 2, false, false))], "abc d");
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(
            terms,
            vec![("a", 1), ("ab", 0), ("b", 0), ("bc", 0), ("c", 0), ("d", 1)]
        );
        let out = run_filters(vec![Box::new(NGram::new(2, 3, true, true))], "a abcd");
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(terms, vec![("a", 1), ("ab", 1), ("abc", 0), ("abcd", 0)]);
        // Offsets are the token's.
        assert!(out.iter().skip(1).all(|t| (t.2, t.3) == (2, 6)));
        let out = run_filters(vec![Box::new(NGram::new(3, 3, false, true))], "ab abcd");
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(terms, vec![("ab", 1), ("abc", 1), ("bcd", 0), ("abcd", 0)]);
        let out = run_filters(vec![Box::new(NGram::new(1, 1, false, false))], "😀x");
        assert_eq!(
            out.iter().map(|t| t.0.as_str()).collect::<Vec<_>>(),
            vec!["😀", "x"]
        );
    }
}
