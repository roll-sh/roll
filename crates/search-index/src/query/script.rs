//! The `script` query (Elasticsearch's `ScriptQuery`): docs for which a filter script returns
//! true, with a constant score.
//!
//! The script runs in the two-phase confirmation step over a match-all approximation whose
//! cost is reported above any real iterator's, so conjunctions always let the other clauses
//! lead and run the script only on their candidates. Script failures stop the search and
//! surface through the searcher (as the deferred error of score scripts).

use std::any::Any;
use std::sync::Arc;

use search_codec::NO_MORE_DOCS;

use crate::error::Result;
use crate::query::basic::explain_const;
use crate::schema::Schema;
use crate::search::{Explanation, Query, Scorable, ScoreMode, Scorer, Searcher, SegmentContext, Weight, defer_error};

/// A filter script (Painless in `search-es`).
pub trait FilterScript: Send + Sync {
    /// An instance bound to a segment.
    fn leaf(&self, seg: &SegmentContext) -> Result<Box<dyn LeafFilterScript>>;
    /// The script's `toString` (`Script{type=inline, lang='painless', idOrCode='…', …}`).
    fn describe(&self) -> String;
}

/// A filter script bound to a segment.
pub trait LeafFilterScript: Send {
    fn matches(&mut self, doc: u32) -> Result<bool>;
}

/// Docs the script accepts.
#[derive(Clone)]
pub struct ScriptQuery {
    pub script: Arc<dyn FilterScript>,
}

impl std::fmt::Debug for ScriptQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ScriptQuery({})", self.script.describe())
    }
}

/// Cost of the approximation: above `NO_MORE_DOCS`, so it never leads a conjunction.
const EXPENSIVE: u64 = NO_MORE_DOCS as u64 + 100;

struct ScriptWeight {
    script: Arc<dyn FilterScript>,
    score: f32,
    desc: String,
}

struct ScriptScorer {
    leaf: Box<dyn LeafFilterScript>,
    max_doc: u32,
    doc: u32,
    score: f32,
}

impl Scorable for ScriptScorer {
    fn score(&mut self) -> f32 {
        self.score
    }
}

impl Scorer for ScriptScorer {
    fn doc(&self) -> u32 {
        self.doc
    }
    fn next(&mut self) -> u32 {
        loop {
            let d = self.approx_next();
            if d == NO_MORE_DOCS || self.matches() {
                return self.doc;
            }
        }
    }
    fn advance(&mut self, t: u32) -> u32 {
        let d = self.approx_advance(t);
        if d == NO_MORE_DOCS || self.matches() {
            return self.doc;
        }
        self.next()
    }
    fn cost(&self) -> u64 {
        EXPENSIVE
    }
    fn has_two_phase(&self) -> bool {
        true
    }
    fn approx_next(&mut self) -> u32 {
        let t = if self.doc == u32::MAX { 0 } else { self.doc.saturating_add(1) };
        self.approx_advance(t)
    }
    fn approx_advance(&mut self, t: u32) -> u32 {
        if self.doc != u32::MAX && t <= self.doc {
            return self.doc;
        }
        self.doc = if t < self.max_doc { t } else { NO_MORE_DOCS };
        self.doc
    }
    fn matches(&mut self) -> bool {
        match self.leaf.matches(self.doc) {
            Ok(m) => m,
            Err(e) => {
                defer_error(e);
                self.doc = NO_MORE_DOCS;
                false
            }
        }
    }
    fn match_cost(&self) -> f32 {
        1000.0
    }
    fn max_score(&mut self, _upto: u32) -> f32 {
        self.score
    }
}

impl Weight for ScriptWeight {
    fn scorer(&self, seg: &SegmentContext) -> Result<Option<Box<dyn Scorer>>> {
        let leaf = self.script.leaf(seg)?;
        Ok(Some(Box::new(ScriptScorer { leaf, max_doc: seg.reader.max_doc(), doc: u32::MAX, score: self.score })))
    }
    fn explain(&self, seg: &SegmentContext, doc: u32) -> Result<Explanation> {
        explain_const(self, seg, doc, &self.desc, self.score)
    }
}

impl Query for ScriptQuery {
    fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>> {
        let score = if mode.needs_scores() { boost } else { 0.0 };
        Ok(Box::new(ScriptWeight { script: self.script.clone(), score, desc: self.lucene_string(s.schema()) }))
    }

    fn lucene_string(&self, _schema: &Schema) -> String {
        format!("ScriptQuery({})", self.script.describe())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
