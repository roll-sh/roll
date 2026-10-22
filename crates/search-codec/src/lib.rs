//! roll search's segment format: everything one immutable segment stores, in one file.
//!
//! A segment is written once by [`write_segment`] from a [`SegmentSource`] (the indexing buffer
//! or a [`MergeSource`]) and read by [`SegmentReader`] over a [`Slice`] (a memory map or owned
//! bytes) without copying sections. The modules, bottom up:
//!
//! - [`slice`](mod@slice), [`io`], [`bitpack`] (SIMD bit unpacking, FOR/PFOR), [`packed`] (doc values
//!   encodings), [`docset`] (sparse doc sets with O(1) rank): building blocks.
//! - [`container`]: the file layout (header, aligned sections, table of contents, CRC32C).
//! - [`info`]: segment and field metadata.
//! - [`terms`] + [`postings`]: block term dictionary (front-coded blocks of 32 terms with their
//!   metadata, a cache-resident index of block keys, optional [`bloom`] filter); doc/freq/
//!   position/offset/payload streams with inline multi-level skip data and impacts for
//!   block-max WAND/MaxScore.
//! - [`automaton`]: the [`Automaton`] trait and builders (prefix, wildcard, Lucene regexp,
//!   Levenshtein) used to intersect the term dictionary.
//! - [`norms`] (Lucene `SmallFloat` encoding), [`docvalues`] (with skip indexes), [`points`]
//!   (BKD trees), [`stored`] (zstd chunks with a trained dictionary), [`vectors`] (HNSW built
//!   on several threads, int8/int4 and binary quantization, SIMD distances).
//! - [`suggest`]: weighted completion entries (Elasticsearch's `completion` field) with
//!   best-first prefix/automaton completion.
//! - [`livedocs`]: per-generation deletion bitsets.
//! - [`source`], [`writer`], [`reader`], [`merge`], [`sort`], [`bp`]: writing, reading, merging,
//!   doc maps, index sorting and BP doc reordering; [`spill`]: bounded-memory section buffers.
//!
//! Design notes live in each module's documentation; see also `docs/search/architecture.md` and
//! `docs/search/contracts.md`.

pub mod automaton;
pub mod bitpack;
pub mod bloom;
pub mod bp;
pub mod container;
pub mod docset;
pub mod docvalues;
pub mod error;
pub mod info;
pub mod io;
pub mod livedocs;
pub mod merge;
pub mod norms;
pub mod packed;
pub mod points;
pub mod postings;
pub mod reader;
pub mod slice;
pub mod sort;
pub mod source;
pub mod spill;
pub mod stored;
pub mod suggest;
pub mod terms;
pub mod vectors;
pub mod writer;

pub use bloom::BloomFilter;
pub use bp::{BpOptions, bp_reorder};
pub use automaton::{AlwaysMatch, Automaton, Dfa, Prefix};
pub use docset::{DocSet, DocSetCursor};
pub use docvalues::{
    BinaryDocValues, DocValuesSkipper, NumericDocValues, SortedDocValues, SortedNumericDocValues,
    SortedSetDocValues, TermDict,
};
pub use error::{Error, Result};
pub use info::{
    DocValuesType, FieldInfo, HnswParams, IndexOptions, IndexSort, PointsSpec, SegmentInfo,
    SortField, SortKind, SortMissing, SortSelector, VectorElement, VectorQuantization,
    VectorSimilarity, VectorSpec,
};
pub use livedocs::LiveDocs;
pub use merge::{MergeInput, MergeSource};
pub use norms::{NormsReader, byte4_to_int, int_to_byte4};
pub use points::{PointVisitor, PointsReader, Relation};
pub use postings::{Impact, PostingsCursor, PostingsFlags, TermState};
pub use reader::{DocValuesReader, SegmentReader};
pub use slice::Slice;
pub use sort::sort_doc_map;
pub use spill::SpillOptions;
pub use source::{
    BytesSource, DocMap, DocValuesSource, NormsIter, PostingsSource, RemappedSource, SegmentSource,
    SortedNumericSource, SortedSetSource, SortedSource, TermsSource,
};
pub use stored::{StoredDocWriter, StoredReader, StoredValue, StoredVisitor, VisitStatus};
pub use suggest::{Completion, Completions, SuggestEntry, SuggestRange, SuggestReader, SuggestSource};
pub use terms::{TermsIter, TermsReader, TermsStats};
pub use vectors::{KnnHit, KnnResult, PreparedQuery, VectorQuery, VectorsReader};
pub use writer::{WriteOptions, write_segment, write_segment_with};

/// Sentinel returned by iterators past their last doc (Lucene's `NO_MORE_DOCS`).
pub const NO_MORE_DOCS: u32 = i32::MAX as u32;
/// Largest number of docs in a segment (doc ids stay below [`NO_MORE_DOCS`]).
pub const MAX_DOCS: u32 = i32::MAX as u32 - 128;
