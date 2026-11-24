//! Documents as the engine indexes them.
//!
//! `search-es` maps JSON documents to [`IndexableDoc`]s: per field number, text to analyze (the
//! engine runs the analyzer straight into its term hash — no token vectors in between),
//! pre-analyzed tokens, single terms (keywords), packed points, doc values, vectors and
//! completion entries, plus stored fields (`_source`, `store: true` fields). Nested objects
//! are child documents; the engine indexes a doc's children (recursively, depth first) right
//! before it, as one block, which block-join queries rely on.

use bytes::Bytes;
use search_analysis::Analyzer;
use search_codec::stored::OwnedStoredValue;

/// A document to index.
#[derive(Clone, Debug, Default)]
pub struct IndexableDoc {
    /// The `_id` term (the caller's encoding of the id; Elasticsearch's `Uid.encodeId`).
    /// Ignored on nested children.
    pub id: Bytes,
    pub fields: Vec<IndexableField>,
    pub stored: Vec<StoredField>,
    /// Nested child documents, indexed before this one.
    pub nested: Vec<IndexableDoc>,
}

impl IndexableDoc {
    pub fn new(id: impl Into<Bytes>) -> IndexableDoc {
        IndexableDoc { id: id.into(), ..Default::default() }
    }

    /// What this document holds in memory: its vectors' capacities and every value's bytes
    /// (a `Bytes` that shares another buffer is counted in full, so this errs high). What a
    /// caller charges a breaker for a document it keeps (`_bulk`'s documents prepared ahead).
    pub fn heap_bytes(&self) -> usize {
        use std::mem::size_of;
        let fields: usize = self.fields.iter().map(|f| match f {
            IndexableField::Text { values, .. } => values.capacity() * size_of::<String>() + values.iter().map(String::capacity).sum::<usize>(),
            IndexableField::Tokens { tokens, .. } => tokens.capacity() * size_of::<PreToken>() + tokens.iter().map(|t| t.term.len() + t.payload.as_ref().map_or(0, Bytes::len)).sum::<usize>(),
            IndexableField::Term { term, .. } => term.len(),
            IndexableField::Point { packed, .. } => packed.capacity(),
            IndexableField::DocValue { value, .. } => match value {
                DocValue::Binary(b) | DocValue::Sorted(b) | DocValue::SortedSet(b) => b.len(),
                DocValue::Numeric(_) | DocValue::SortedNumeric(_) => 0,
            },
            IndexableField::Vector { value, .. } => value.len(),
            IndexableField::Suggest { key, payload, .. } => key.len() + payload.len(),
        }).sum();
        let stored: usize = self.stored.iter().map(|s| match &s.value {
            OwnedStoredValue::Str(v) => v.capacity(),
            OwnedStoredValue::Bytes(v) => v.capacity(),
            _ => 0,
        }).sum();
        size_of::<IndexableDoc>() + self.id.len() + self.fields.capacity() * size_of::<IndexableField>() + fields
            + self.stored.capacity() * size_of::<StoredField>() + stored
            + self.nested.iter().map(IndexableDoc::heap_bytes).sum::<usize>()
            + (self.nested.capacity() - self.nested.len()) * size_of::<IndexableDoc>()
    }

    /// Adds analyzed text.
    pub fn text(mut self, field: u32, analyzer: &Analyzer, value: impl Into<String>) -> Self {
        self.fields.push(IndexableField::Text { field, analyzer: analyzer.clone(), values: vec![value.into()] });
        self
    }

    /// Adds a single term.
    /// Adds any field.
    pub fn field(mut self, f: IndexableField) -> Self {
        self.fields.push(f);
        self
    }

    pub fn term(mut self, field: u32, term: impl Into<Bytes>) -> Self {
        self.fields.push(IndexableField::Term { field, term: term.into() });
        self
    }

    pub fn doc_value(mut self, field: u32, value: DocValue) -> Self {
        self.fields.push(IndexableField::DocValue { field, value });
        self
    }

    pub fn point(mut self, field: u32, packed: impl Into<Vec<u8>>) -> Self {
        self.fields.push(IndexableField::Point { field, packed: packed.into() });
        self
    }

    pub fn stored(mut self, field: u32, value: OwnedStoredValue) -> Self {
        self.stored.push(StoredField { field, value });
        self
    }

    pub fn child(mut self, d: IndexableDoc) -> Self {
        self.nested.push(d);
        self
    }

    /// Number of Lucene docs this doc becomes (itself plus nested children, recursively).
    pub fn doc_count(&self) -> u32 {
        1 + self.nested.iter().map(|c| c.doc_count()).sum::<u32>()
    }
}

/// One indexed value of a field.
#[derive(Clone, Debug)]
pub enum IndexableField {
    /// Text analyzed by `analyzer`. Several values of one field form one field instance with
    /// the analyzer's position/offset gaps between them; pass all of a field's values in one
    /// entry (separate entries also work, with the gap applied from the last token).
    Text { field: u32, analyzer: Analyzer, values: Vec<String> },
    /// Pre-analyzed tokens.
    Tokens { field: u32, tokens: Vec<PreToken> },
    /// One un-analyzed term (keyword, numbers' term form, `_field_names`, …).
    Term { field: u32, term: Bytes },
    /// A packed point value (`PointsSpec::packed_len` bytes).
    Point { field: u32, packed: Vec<u8> },
    DocValue { field: u32, value: DocValue },
    /// A raw vector in the field's storage encoding (LE f32, i8 or packed bits).
    Vector { field: u32, value: Bytes },
    /// A completion entry (`completion` fields).
    Suggest { field: u32, key: Bytes, weight: u64, payload: Bytes },
}

impl IndexableField {
    pub fn field(&self) -> u32 {
        match self {
            IndexableField::Text { field, .. }
            | IndexableField::Tokens { field, .. }
            | IndexableField::Term { field, .. }
            | IndexableField::Point { field, .. }
            | IndexableField::DocValue { field, .. }
            | IndexableField::Vector { field, .. }
            | IndexableField::Suggest { field, .. } => *field,
        }
    }
}

/// A token produced outside the engine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreToken {
    pub term: Bytes,
    /// Position increment from the previous token (0 = stacked).
    pub position_increment: u32,
    pub start_offset: u32,
    pub end_offset: u32,
    pub payload: Option<Bytes>,
    /// Custom term frequency (Lucene `TermFrequencyAttribute`; 1 normally). Fields using it
    /// must not index positions.
    pub freq: u32,
}

impl PreToken {
    pub fn new(term: impl Into<Bytes>, position_increment: u32, start_offset: u32, end_offset: u32) -> PreToken {
        PreToken { term: term.into(), position_increment, start_offset, end_offset, payload: None, freq: 1 }
    }
}

/// A doc value. Several values of a sorted-set or sorted-numeric field accumulate; numeric,
/// binary and sorted fields take one value per doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocValue {
    Numeric(i64),
    SortedNumeric(i64),
    Binary(Bytes),
    Sorted(Bytes),
    SortedSet(Bytes),
}

/// A stored field value.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredField {
    pub field: u32,
    pub value: OwnedStoredValue,
}
