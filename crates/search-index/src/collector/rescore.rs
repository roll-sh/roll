//! Elasticsearch's query rescorer: re-scores the top `window_size` hits of a score-sorted
//! search with a second query and combines both scores.
//!
//! A rescored hit's score is `mode(first * query_weight, second * rescore_query_weight)` when
//! the rescore query matches it and `first * query_weight` otherwise; hits beyond the window
//! are scaled by `query_weight` too, and all hits are sorted again (score desc, doc asc), as
//! Elasticsearch does. Several rescorers apply one after the other.

use crate::collector::top_docs::{ScoreDoc, TopDocs};
use crate::error::Result;
use crate::search::{Explanation, QueryRef, ScoreMode, Searcher};

/// `score_mode` of a rescorer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RescoreMode {
    /// `total` (the default): the sum.
    #[default]
    Total,
    Multiply,
    Avg,
    Max,
    Min,
}

impl RescoreMode {
    pub fn parse(s: &str) -> Option<RescoreMode> {
        Some(match s {
            "total" => RescoreMode::Total,
            "multiply" => RescoreMode::Multiply,
            "avg" => RescoreMode::Avg,
            "max" => RescoreMode::Max,
            "min" => RescoreMode::Min,
            _ => return None,
        })
    }

    pub fn combine(self, a: f32, b: f32) -> f32 {
        match self {
            RescoreMode::Total => a + b,
            RescoreMode::Multiply => a * b,
            RescoreMode::Avg => (a + b) / 2.0,
            RescoreMode::Max => a.max(b),
            RescoreMode::Min => a.min(b),
        }
    }

    /// The word explanations use (`<name> of:`).
    pub fn name(self) -> &'static str {
        match self {
            RescoreMode::Total => "sum",
            RescoreMode::Multiply => "product",
            RescoreMode::Avg => "avg",
            RescoreMode::Max => "max",
            RescoreMode::Min => "min",
        }
    }
}

/// One `rescore` entry with a `query`.
#[derive(Clone, Debug)]
pub struct QueryRescorer {
    pub query: QueryRef,
    /// `window_size` (10).
    pub window_size: usize,
    /// `query_weight` (1).
    pub query_weight: f32,
    /// `rescore_query_weight` (1).
    pub rescore_query_weight: f32,
    pub mode: RescoreMode,
}

impl QueryRescorer {
    pub fn new(query: QueryRef) -> QueryRescorer {
        QueryRescorer { query, window_size: 10, query_weight: 1.0, rescore_query_weight: 1.0, mode: RescoreMode::Total }
    }

    /// Rescores the first `window_size` hits of `top` (sorted by score).
    pub fn rescore(&self, s: &Searcher, mut top: TopDocs) -> Result<TopDocs> {
        let window = self.window_size.min(top.hits.len());
        if window == 0 {
            return Ok(top);
        }
        let rest = top.hits.split_off(window);
        let mut hits = std::mem::take(&mut top.hits);
        let second = self.second_pass_scores(s, &hits)?;
        for (h, sec) in hits.iter_mut().zip(second) {
            let first = h.score * self.query_weight;
            h.score = match sec {
                Some(v) => self.mode.combine(first, v * self.rescore_query_weight),
                None => first,
            };
        }
        // Hits beyond the window count as rescored hits the second query didn't match, and
        // everything is sorted again (as Elasticsearch does).
        hits.extend(rest.into_iter().map(|h| ScoreDoc { doc: h.doc, score: h.score * self.query_weight }));
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.doc.cmp(&b.doc)));
        top.hits = hits;
        Ok(top)
    }

    /// The rescore query's score of each hit (`None`: no match), in the hits' order.
    fn second_pass_scores(&self, s: &Searcher, hits: &[ScoreDoc]) -> Result<Vec<Option<f32>>> {
        let w = s.create_weight(&self.query, ScoreMode::Complete)?;
        let mut by_doc: Vec<(u32, usize)> = hits.iter().enumerate().map(|(i, h)| (h.doc, i)).collect();
        by_doc.sort_unstable();
        let mut out = vec![None; hits.len()];
        let mut i = 0;
        while i < by_doc.len() {
            let Some((seg, _)) = s.segment_of(by_doc[i].0) else {
                i += 1;
                continue;
            };
            let end = seg.base + seg.reader.max_doc();
            let mut j = i;
            while j < by_doc.len() && by_doc[j].0 < end {
                j += 1;
            }
            if let Some(mut sc) = w.scorer(seg)? {
                for &(doc, idx) in &by_doc[i..j] {
                    let local = doc - seg.base;
                    let cur = sc.doc();
                    let d = if cur != u32::MAX && cur >= local { cur } else { sc.advance(local) };
                    if d == local {
                        out[idx] = Some(sc.score());
                    }
                }
            }
            i = j;
        }
        if let Some(e) = crate::search::take_deferred_error() {
            return Err(e);
        }
        Ok(out)
    }

    /// Explains a rescored hit, given the first-pass explanation; `rescored`: whether the hit
    /// was inside the window.
    pub fn explain(&self, s: &Searcher, doc: u32, first: Explanation, rescored: bool) -> Result<Explanation> {
        let prim = if first.is_match() {
            let v = first.value() * self.query_weight;
            Explanation::matched(v, "product of:", vec![first, Explanation::leaf(self.query_weight, "primaryWeight")])
        } else {
            Explanation::no_match("First pass did not match", vec![first])
        };
        if rescored {
            let e = s.explain(&self.query, doc)?;
            if e.is_match() {
                let w = self.rescore_query_weight;
                let sec_value = e.value() * w;
                let sec = Explanation::matched(sec_value, "product of:", vec![e, Explanation::leaf(w, "secondaryWeight")]);
                let v = self.mode.combine(prim.value(), sec_value);
                return Ok(Explanation::matched(v, format!("{} of:", self.mode.name()), vec![prim, sec]));
            }
        }
        Ok(prim)
    }
}
