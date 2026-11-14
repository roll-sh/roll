//! Collectors: what a search keeps from the matches.
//!
//! - [`top_docs`]: top hits by score with Elasticsearch's `track_total_hits` semantics, and
//!   plain counting.
//! - [`top_field`]: top hits by sort keys (multi-key, missing, mode, nested, search_after,
//!   early termination on the index sort).
//! - [`compose`]: several collectors in one pass, `terminate_after`, `min_score`, and the
//!   managers + merges of concurrent segment search.
//! - [`collapse`]: field collapsing (top groups by their best hit).
//! - [`rescore`]: Elasticsearch's query rescorer over the top window.

pub mod collapse;
pub mod compose;
pub mod rescore;
pub mod top_docs;
pub mod top_field;

pub use top_docs::{ScoreDoc, TopDocs, TopScoreCollector, TotalHitCountCollector, TotalRelation};
pub use collapse::{CollapseCollector, CollapseField, CollapseHit, CollapseTopDocs};
pub use compose::{CountManager, MinScore, MultiCollector, PostFilter, TerminateAfter, TopFieldManager, TopScoreManager};
pub use rescore::{QueryRescorer, RescoreMode};
pub use top_field::{
    FieldDoc, FieldSort, Missing, NestedSort, SortKey, SortMode, SortType, SortValue, TopFieldCollector, TopFieldDocs, compare_field_docs,
    GeoDistanceSort, GeoDistanceType, LeafSortScript, ScriptSort, SortScript,
};
