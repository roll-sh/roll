//! `shingle` and `common_grams` (Lucene's `ShingleFilter`, `CommonGramsFilter` and
//! `CommonGramsQueryFilter`).
//!
//! The shingle filter is a faithful port of Lucene's window algorithm, including its quirks that
//! show up in output: filler tokens for position gaps (capped at `max_shingle_size - 1`, with
//! the gap token's attributes and an empty offset at its start), trailing fillers from the final
//! position increment, shingles copying every attribute of their first token, position lengths
//! that depend on `output_unigrams`, and `output_unigrams_if_no_shingles`. Window entries are
//! recycled, so steady-state shingling allocates nothing.

use std::collections::VecDeque;
use std::sync::Arc;

use super::WordSet;
use crate::component::{FilterFactory, SynonymRole};
use crate::error::{AnalysisError, Result};
use crate::registry::{Ctx, parse_words};
use crate::settings::Settings;
use crate::stream::{Filter, TokenStream};
use crate::token::Tok;

#[derive(Clone, Debug)]
pub(crate) struct ShingleConfig {
    pub min: usize,
    pub max: usize,
    pub output_unigrams: bool,
    pub output_unigrams_if_no_shingles: bool,
    pub separator: String,
    pub filler: String,
    pub token_type: &'static str,
}

impl Default for ShingleConfig {
    fn default() -> Self {
        ShingleConfig {
            min: 2,
            max: 2,
            output_unigrams: true,
            output_unigrams_if_no_shingles: false,
            separator: " ".into(),
            filler: "_".into(),
            token_type: "shingle",
        }
    }
}

struct Win {
    tok: Tok,
    filler: bool,
}

/// Lucene's `CircularSequence` of gram sizes.
#[derive(Clone, Copy)]
struct Circ {
    value: usize,
    prev: usize,
    min_value: usize,
}

pub(crate) struct Shingle {
    cfg: Arc<ShingleConfig>,
    window: VecDeque<Win>,
    spare: Vec<Win>,
    gram: Circ,
    builder: String,
    fillers_to_insert: usize,
    /// The real token that follows pending fillers.
    next_input: Tok,
    /// The trailing fillers' source only carries a term and offsets.
    next_input_partial: bool,
    is_next_input: bool,
    is_output_here: bool,
    no_shingle_output: bool,
    exhausted: bool,
    end_state: Tok,
}

impl Shingle {
    pub fn new(cfg: Arc<ShingleConfig>) -> Shingle {
        let min_value = if cfg.output_unigrams { 1 } else { cfg.min };
        Shingle {
            cfg,
            window: VecDeque::new(),
            spare: Vec::new(),
            gram: Circ {
                value: min_value,
                prev: min_value,
                min_value,
            },
            builder: String::new(),
            fillers_to_insert: 0,
            next_input: Tok::default(),
            next_input_partial: false,
            is_next_input: false,
            is_output_here: false,
            no_shingle_output: true,
            exhausted: false,
            end_state: Tok::default(),
        }
    }

    fn advance(&mut self) {
        let g = &mut self.gram;
        g.prev = g.value;
        if g.value == 1 {
            g.value = self.cfg.min;
        } else if g.value == self.cfg.max {
            g.value = g.min_value;
            g.prev = g.min_value;
        } else {
            g.value += 1;
        }
    }

    fn reset_gram(&mut self) {
        self.gram.value = self.gram.min_value;
        self.gram.prev = self.gram.min_value;
    }

    fn filler_from(&mut self, target: &mut Win) {
        if self.next_input_partial {
            // Only term and offsets are copied onto the recycled entry.
            target.tok.start = self.next_input.start;
            target.tok.end = self.next_input.end;
        } else {
            target.tok.clone_from(&self.next_input);
        }
        target.tok.end = target.tok.start;
        target.tok.term.clear();
        target.tok.term.push_str(&self.cfg.filler);
        target.filler = true;
        self.fillers_to_insert -= 1;
    }

    /// `getNextToken`: fills `target` with the next window entry; false at the end.
    fn next_token(
        &mut self,
        input: &mut dyn TokenStream,
        target: &mut Win,
        scratch: &mut Tok,
    ) -> bool {
        if self.fillers_to_insert > 0 {
            self.filler_from(target);
            return true;
        }
        if self.is_next_input {
            target.tok.clone_from(&self.next_input);
            self.is_next_input = false;
            target.filler = false;
            return true;
        }
        if self.exhausted {
            return false;
        }
        if input.next(scratch) {
            target.tok.clone_from(scratch);
            if scratch.pos_inc > 1 {
                self.fillers_to_insert = (scratch.pos_inc as usize - 1).min(self.cfg.max - 1);
                self.next_input.clone_from(scratch);
                self.next_input_partial = false;
                self.is_next_input = true;
                target.tok.end = target.tok.start;
                target.tok.term.clear();
                target.tok.term.push_str(&self.cfg.filler);
                target.filler = true;
                self.fillers_to_insert -= 1;
            } else {
                target.filler = false;
            }
            true
        } else {
            self.exhausted = true;
            scratch.clear_for_end();
            input.end(scratch);
            self.end_state.clone_from(scratch);
            self.fillers_to_insert = (scratch.pos_inc as usize).min(self.cfg.max - 1);
            if self.fillers_to_insert > 0 {
                self.next_input.clear();
                self.next_input.start = scratch.end;
                self.next_input.end = scratch.end;
                self.next_input_partial = true;
                return self.next_token(input, target, scratch);
            }
            false
        }
    }

    fn shift(&mut self, input: &mut dyn TokenStream, scratch: &mut Tok) {
        let mut first = self.window.pop_front();
        while self.window.len() < self.cfg.max {
            if let Some(mut f) = first.take() {
                if self.next_token(input, &mut f, scratch) {
                    self.window.push_back(f);
                } else {
                    self.spare.push(f);
                    break;
                }
            } else {
                let mut w = self.spare.pop().unwrap_or(Win {
                    tok: Tok::default(),
                    filler: false,
                });
                // A fresh entry (Lucene clones the attributes it is filled from).
                w.tok.clear();
                if self.next_token(input, &mut w, scratch) {
                    self.window.push_back(w);
                } else {
                    self.spare.push(w);
                    break;
                }
            }
        }
        if self.cfg.output_unigrams_if_no_shingles
            && self.no_shingle_output
            && self.gram.min_value > 1
            && self.window.len() < self.cfg.min
        {
            self.gram.min_value = 1;
        }
        self.reset_gram();
        self.is_output_here = false;
    }
}

impl Filter for Shingle {
    fn reset(&mut self) {
        while let Some(w) = self.window.pop_front() {
            self.spare.push(w);
        }
        self.gram.min_value = if self.cfg.output_unigrams {
            1
        } else {
            self.cfg.min
        };
        self.reset_gram();
        self.is_next_input = false;
        self.fillers_to_insert = 0;
        self.is_output_here = false;
        self.no_shingle_output = true;
        self.exhausted = false;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        let mut built = 0usize;
        if self.gram.value == self.gram.min_value || self.window.len() < self.gram.value {
            self.shift(input, t);
            self.builder.clear();
        } else {
            built = self.gram.prev;
        }
        if self.window.len() < self.gram.value {
            return false;
        }
        let mut all_filler = true;
        let mut last = 0usize;
        let mut i = 0;
        let mut gram_num = 1;
        // The gram size can advance inside the loop (a window starting with fillers).
        while i < self.window.len() && built < self.gram.value {
            last = i;
            if built < gram_num {
                if built > 0 {
                    self.builder.push_str(&self.cfg.separator);
                }
                self.builder.push_str(&self.window[i].tok.term);
                built += 1;
            }
            if all_filler && self.window[i].filler {
                if gram_num == self.gram.value {
                    self.advance();
                }
            } else {
                all_filler = false;
            }
            i += 1;
            gram_num += 1;
        }
        if all_filler || built != self.gram.value {
            return false;
        }
        let end = self.window[last].tok.end;
        t.clone_from(&self.window[0].tok);
        t.pos_inc = if self.is_output_here { 0 } else { 1 };
        t.term.clear();
        t.term.push_str(&self.builder);
        if self.gram.value > 1 {
            t.kind = self.cfg.token_type;
            self.no_shingle_output = false;
        }
        t.end = end;
        t.pos_len = if self.cfg.output_unigrams {
            built as u32
        } else {
            (built as i64 - self.cfg.min as i64 + 1).max(1) as u32
        };
        self.is_output_here = true;
        self.advance();
        true
    }

    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        if self.exhausted {
            t.clone_from(&self.end_state);
        } else {
            input.end(t);
        }
    }
}

/// `CommonGramsFilter`: bigrams of a common word with its neighbours (type `gram`, position
/// length 2), stacked on the first word.
pub(crate) struct CommonGrams {
    words: Arc<WordSet>,
    buffer: String,
    last_start: u32,
    last_was_common: bool,
    saved: Option<()>,
    saved_tok: Tok,
    scratch: String,
}

impl CommonGrams {
    pub fn new(words: Arc<WordSet>) -> CommonGrams {
        CommonGrams {
            words,
            buffer: String::new(),
            last_start: 0,
            last_was_common: false,
            saved: None,
            saved_tok: Tok::default(),
            scratch: String::new(),
        }
    }

    fn is_common(&mut self, t: &Tok) -> bool {
        self.words.contains(&t.term, &mut self.scratch)
    }

    fn save_term(&mut self, t: &Tok) {
        self.buffer.clear();
        self.buffer.push_str(&t.term);
        self.buffer.push('_');
        self.last_start = t.start;
        self.last_was_common = self.is_common(t);
    }
}

impl Filter for CommonGrams {
    fn reset(&mut self) {
        self.last_was_common = false;
        self.saved = None;
        self.buffer.clear();
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        if self.saved.take().is_some() {
            t.clone_from(&self.saved_tok);
            self.save_term(t);
            return true;
        }
        if !input.next(t) {
            return false;
        }
        if self.last_was_common || (self.is_common(t) && !self.buffer.is_empty()) {
            self.saved_tok.clone_from(t);
            self.saved = Some(());
            self.buffer.push_str(&t.term);
            let end = t.end;
            t.clear();
            std::mem::swap(&mut t.term, &mut self.buffer);
            self.buffer.clear();
            t.pos_inc = 0;
            t.pos_len = 2;
            t.start = self.last_start;
            t.end = end;
            t.kind = "gram";
            return true;
        }
        self.save_term(t);
        true
    }
}

/// `CommonGramsQueryFilter`: keeps the grams and drops unigrams covered by them.
pub(crate) struct CommonGramsQuery {
    inner: CommonGrams,
    previous: Tok,
    has_previous: bool,
    /// The type of the token returned last (Lucene's `previousType`).
    previous_type: &'static str,
    exhausted: bool,
}

impl CommonGramsQuery {
    pub fn new(inner: CommonGrams) -> Self {
        CommonGramsQuery {
            inner,
            previous: Tok::default(),
            has_previous: false,
            previous_type: "",
            exhausted: false,
        }
    }

    fn fix_gram(t: &mut Tok) {
        if t.kind == "gram" {
            t.pos_inc = 1;
            t.pos_len = 1;
        }
    }
}

impl Filter for CommonGramsQuery {
    fn reset(&mut self) {
        self.inner.reset();
        self.has_previous = false;
        self.previous_type = "";
        self.exhausted = false;
    }

    fn next(&mut self, input: &mut dyn TokenStream, t: &mut Tok) -> bool {
        while !self.exhausted && self.inner.next(input, t) {
            if self.has_previous && t.kind != "gram" {
                std::mem::swap(&mut self.previous, t);
                self.previous_type = t.kind;
                Self::fix_gram(t);
                return true;
            }
            self.previous.clone_from(t);
            self.has_previous = true;
        }
        self.exhausted = true;
        if !self.has_previous || self.previous_type == "gram" {
            return false;
        }
        t.clone_from(&self.previous);
        self.has_previous = false;
        Self::fix_gram(t);
        true
    }

    fn end(&mut self, input: &mut dyn TokenStream, t: &mut Tok) {
        self.inner.end(input, t);
    }
}

struct ShingleFactory {
    cfg: Arc<ShingleConfig>,
}

impl FilterFactory for ShingleFactory {
    fn create(&self) -> Box<dyn Filter> {
        Box::new(Shingle::new(self.cfg.clone()))
    }
    fn check(&self) -> Result<()> {
        if self.cfg.max < 2 {
            return Err(AnalysisError::illegal("Max shingle size must be >= 2"));
        }
        if self.cfg.min < 2 {
            return Err(AnalysisError::illegal("Min shingle size must be >= 2"));
        }
        if self.cfg.min > self.cfg.max {
            return Err(AnalysisError::illegal(
                "Min shingle size must be <= max shingle size",
            ));
        }
        Ok(())
    }
    fn synonym_role(&self) -> SynonymRole {
        SynonymRole::Forbidden
    }
}

/// `shingle` (`ShingleTokenFilterFactory`).
pub(crate) fn shingle_factory(ctx: &Ctx, s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let max = s.get_int("max_shingle_size", 2)?;
    let min = s.get_int("min_shingle_size", 2)?;
    let output_unigrams = s.get_bool("output_unigrams", true)?;
    let diff = max - min + output_unigrams as i32;
    if diff > ctx.opts.max_shingle_diff {
        return Err(AnalysisError::illegal(format!(
            "In Shingle TokenFilter the difference between max_shingle_size and min_shingle_size (and +1 if outputting unigrams) must be less than or equal to: [{}] but was [{diff}]. This limit can be set by changing the [index.max_shingle_diff] index level setting.",
            ctx.opts.max_shingle_diff
        )));
    }
    let output_unigrams_if_no_shingles = s.get_bool("output_unigrams_if_no_shingles", false)?;
    let separator = s.get("token_separator").unwrap_or_else(|| " ".into());
    let filler = s.get("filler_token").unwrap_or_else(|| "_".into());
    Ok(Arc::new(ShingleFactory {
        cfg: Arc::new(ShingleConfig {
            min: min.max(0) as usize,
            max: max.max(0) as usize,
            output_unigrams,
            output_unigrams_if_no_shingles,
            separator,
            filler,
            token_type: "shingle",
        }),
    }))
}

pub(crate) fn shingle_prebuilt() -> Arc<dyn FilterFactory> {
    Arc::new(ShingleFactory {
        cfg: Arc::new(ShingleConfig::default()),
    })
}

struct CommonGramsFactory {
    words: Arc<WordSet>,
    query: bool,
}

impl FilterFactory for CommonGramsFactory {
    fn create(&self) -> Box<dyn Filter> {
        let cg = CommonGrams::new(self.words.clone());
        if self.query {
            Box::new(CommonGramsQuery::new(cg))
        } else {
            Box::new(cg)
        }
    }
    fn synonym_role(&self) -> SynonymRole {
        SynonymRole::Forbidden
    }
}

/// `common_grams` (`CommonGramsTokenFilterFactory`).
pub(crate) fn common_grams_factory(s: &Settings) -> Result<Arc<dyn FilterFactory>> {
    let ic = s.get_bool("ignore_case", false)?;
    let query = s.get_bool("query_mode", false)?;
    let Some(words) = parse_words(s, "common_words", None, true, ic)? else {
        return Err(AnalysisError::illegal(
            "missing or empty [common_words] or [common_words_path] configuration for common_grams token filter",
        ));
    };
    Ok(Arc::new(CommonGramsFactory {
        words: Arc::new(words),
        query,
    }))
}

pub(crate) fn common_grams_prebuilt() -> Arc<dyn FilterFactory> {
    Arc::new(CommonGramsFactory {
        words: Arc::new(WordSet::default()),
        query: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::tests::run_filters;

    fn shingles(cfg: ShingleConfig, text: &str) -> Vec<(String, u32)> {
        run_filters(vec![Box::new(Shingle::new(Arc::new(cfg)))], text)
            .into_iter()
            .map(|t| (t.0, t.1))
            .collect()
    }

    #[test]
    fn shingles_with_unigrams() {
        let out = shingles(ShingleConfig::default(), "a b c");
        assert_eq!(
            out,
            vec![
                ("a".into(), 1),
                ("a b".into(), 0),
                ("b".into(), 1),
                ("b c".into(), 0),
                ("c".into(), 1)
            ]
        );
        let cfg = ShingleConfig {
            min: 2,
            max: 3,
            output_unigrams: false,
            ..Default::default()
        };
        let out = shingles(cfg, "a b c d");
        let terms: Vec<_> = out.iter().map(|t| t.0.as_str()).collect();
        assert_eq!(terms, vec!["a b", "a b c", "b c", "b c d", "c d"]);
        let cfg = ShingleConfig {
            output_unigrams: false,
            output_unigrams_if_no_shingles: true,
            ..Default::default()
        };
        assert_eq!(shingles(cfg, "single"), vec![("single".into(), 1)]);
    }

    #[test]
    fn common_grams() {
        let words = Arc::new(WordSet::new(["the", "of"], false));
        let out = run_filters(
            vec![Box::new(CommonGrams::new(words.clone()))],
            "the quick of fox",
        );
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(
            terms,
            vec![
                ("the", 1),
                ("the_quick", 0),
                ("quick", 1),
                ("quick_of", 0),
                ("of", 1),
                ("of_fox", 0),
                ("fox", 1)
            ]
        );
        let out = run_filters(
            vec![Box::new(CommonGramsQuery::new(CommonGrams::new(words)))],
            "the quick of fox",
        );
        let terms: Vec<_> = out.iter().map(|t| (t.0.as_str(), t.1)).collect();
        assert_eq!(
            terms,
            vec![("the_quick", 1), ("quick_of", 1), ("of_fox", 1)]
        );
    }
}
