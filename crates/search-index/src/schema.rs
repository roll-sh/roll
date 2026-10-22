//! The shard's field model: what each field number indexes and how it scores.
//!
//! `search-es` turns mappings into a [`Schema`]; the engine only sees field numbers and their
//! indexing options. Numbers are stable for the life of the shard (segments store them) and
//! the first [`FIRST_USER_FIELD`] are the engine's own metadata fields:
//!
//! | number | field            | indexed as |
//! | ------ | ---------------- | ---------- |
//! | 0      | `_id`            | docs-only terms (Bloom-filtered), stored |
//! | 1      | `_seq_no`        | 1-d long point + numeric doc values |
//! | 2      | `_version`       | numeric doc values |
//! | 3      | `_primary_term`  | numeric doc values, root docs only: it marks which docs are parents of nested blocks |
//!
//! Fields are added as mappings grow ([`Schema::with_field`]); the options of an existing number
//! never change (an analyzer change is a new field in Elasticsearch, too), except its
//! similarity, which scoring reads at search time.

use std::collections::HashMap;
use std::sync::Arc;

use search_codec::{DocValuesType, FieldInfo, IndexOptions, IndexSort, PointsSpec, VectorSpec};

use crate::error::{Error, Result};
use crate::similarity::Similarity;

pub const ID_FIELD: u32 = 0;
pub const SEQ_NO_FIELD: u32 = 1;
pub const VERSION_FIELD: u32 = 2;
pub const PRIMARY_TERM_FIELD: u32 = 3;
/// Field numbers below this are reserved for the engine.
pub const FIRST_USER_FIELD: u32 = 8;

/// Bit set in stored-field numbers holding a field's term vectors (see [`crate::termvectors`]).
pub const TERM_VECTORS_STORED_BIT: u32 = 1 << 31;

/// Term vectors a field keeps per document (`term_vector` mapping option).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermVectorOptions {
    pub positions: bool,
    pub offsets: bool,
    pub payloads: bool,
}

/// One field of the shard.
#[derive(Clone, Debug)]
pub struct FieldSchema {
    pub number: u32,
    pub name: String,
    pub index: IndexOptions,
    pub norms: bool,
    pub payloads: bool,
    pub doc_values: DocValuesType,
    /// Doc values skip index (min/max per block), for fast range filters on doc values.
    pub doc_values_skip_index: bool,
    pub points: Option<PointsSpec>,
    pub vectors: Option<VectorSpec>,
    pub bloom_filter: bool,
    pub suggest: bool,
    pub term_vectors: Option<TermVectorOptions>,
    /// `None`: the schema's default similarity.
    pub similarity: Option<Arc<Similarity>>,
}

impl FieldSchema {
    /// A field that indexes nothing yet.
    pub fn new(number: u32, name: impl Into<String>) -> FieldSchema {
        FieldSchema {
            number,
            name: name.into(),
            index: IndexOptions::None,
            norms: false,
            payloads: false,
            doc_values: DocValuesType::None,
            doc_values_skip_index: false,
            points: None,
            vectors: None,
            bloom_filter: false,
            suggest: false,
            term_vectors: None,
            similarity: None,
        }
    }

    pub fn indexed(mut self, index: IndexOptions, norms: bool) -> Self {
        self.index = index;
        self.norms = norms;
        self
    }
    pub fn with_payloads(mut self) -> Self {
        self.payloads = true;
        self
    }
    pub fn with_doc_values(mut self, dv: DocValuesType) -> Self {
        self.doc_values = dv;
        self
    }
    pub fn with_skip_index(mut self) -> Self {
        self.doc_values_skip_index = true;
        self
    }
    pub fn with_points(mut self, spec: PointsSpec) -> Self {
        self.points = Some(spec);
        self
    }
    pub fn with_vectors(mut self, spec: VectorSpec) -> Self {
        self.vectors = Some(spec);
        self
    }
    pub fn with_bloom_filter(mut self) -> Self {
        self.bloom_filter = true;
        self
    }
    pub fn with_suggest(mut self) -> Self {
        self.suggest = true;
        self
    }
    pub fn with_term_vectors(mut self, tv: TermVectorOptions) -> Self {
        self.term_vectors = Some(tv);
        self
    }
    pub fn with_similarity(mut self, s: Similarity) -> Self {
        self.similarity = Some(Arc::new(s));
        self
    }

    /// The codec's view of the field.
    pub fn field_info(&self) -> FieldInfo {
        let mut f = FieldInfo::new(self.number, self.name.clone());
        f.index = self.index;
        f.norms = self.norms && self.index.is_indexed();
        f.payloads = self.payloads && self.index.has_positions();
        f.doc_values = self.doc_values;
        f.doc_values_skip_index = self.doc_values_skip_index;
        f.points = self.points;
        f.vectors = self.vectors;
        f.bloom_filter = self.bloom_filter;
        f.suggest = self.suggest;
        f
    }

    fn same_indexing(&self, o: &FieldSchema) -> bool {
        self.index == o.index
            && self.norms == o.norms
            && self.payloads == o.payloads
            && self.doc_values == o.doc_values
            && self.points == o.points
            && self.vectors == o.vectors
            && self.suggest == o.suggest
    }
}

/// Every field of a shard plus shard-wide indexing settings.
#[derive(Clone, Debug)]
pub struct Schema {
    fields: Vec<Option<FieldSchema>>,
    names: HashMap<String, u32>,
    /// `index.sort.*`.
    pub index_sort: Option<IndexSort>,
    /// The default similarity (`index.similarity.default`).
    pub similarity: Arc<Similarity>,
}

impl Default for Schema {
    fn default() -> Self {
        Schema::new()
    }
}

impl Schema {
    /// A schema with only the engine's metadata fields.
    pub fn new() -> Schema {
        let mut s = Schema {
            fields: Vec::new(),
            names: HashMap::new(),
            index_sort: None,
            similarity: Arc::new(Similarity::bm25()),
        };
        s.put(FieldSchema::new(ID_FIELD, "_id").indexed(IndexOptions::Docs, false).with_bloom_filter());
        s.put(
            FieldSchema::new(SEQ_NO_FIELD, "_seq_no")
                .with_points(PointsSpec::new(1, 8))
                .with_doc_values(DocValuesType::Numeric)
                .with_skip_index(),
        );
        s.put(FieldSchema::new(VERSION_FIELD, "_version").with_doc_values(DocValuesType::Numeric));
        s.put(FieldSchema::new(PRIMARY_TERM_FIELD, "_primary_term").with_doc_values(DocValuesType::Numeric));
        s
    }

    fn put(&mut self, f: FieldSchema) {
        let n = f.number as usize;
        if self.fields.len() <= n {
            self.fields.resize(n + 1, None);
        }
        if let Some(old) = &self.fields[n] {
            self.names.remove(&old.name);
        }
        self.names.insert(f.name.clone(), f.number);
        self.fields[n] = Some(f);
    }

    /// Adds (or updates the similarity of) a field. Changing how an existing number is
    /// indexed, reusing a name for another number, or a reserved number fails.
    pub fn add_field(&mut self, f: FieldSchema) -> Result<()> {
        if f.number < FIRST_USER_FIELD {
            return Err(Error::illegal(format!("field number {} is reserved", f.number)));
        }
        if f.number >= TERM_VECTORS_STORED_BIT {
            return Err(Error::illegal(format!("field number {} is too large", f.number)));
        }
        if let Some(&n) = self.names.get(&f.name)
            && n != f.number
        {
            return Err(Error::illegal(format!("field [{}] already has number {n}", f.name)));
        }
        if let Some(Some(old)) = self.fields.get(f.number as usize) {
            if old.name != f.name {
                return Err(Error::illegal(format!(
                    "field number {} is already used by [{}]",
                    f.number, old.name
                )));
            }
            if !old.same_indexing(&f) {
                return Err(Error::illegal(format!(
                    "cannot change how field [{}] is indexed",
                    f.name
                )));
            }
        }
        self.put(f);
        Ok(())
    }

    /// Builder form of [`add_field`](Self::add_field).
    pub fn with_field(mut self, f: FieldSchema) -> Result<Schema> {
        self.add_field(f)?;
        Ok(self)
    }

    pub fn with_index_sort(mut self, sort: Option<IndexSort>) -> Schema {
        self.index_sort = sort;
        self
    }

    pub fn with_similarity(mut self, s: Similarity) -> Schema {
        self.similarity = Arc::new(s);
        self
    }

    #[inline]
    pub fn field(&self, number: u32) -> Option<&FieldSchema> {
        self.fields.get(number as usize).and_then(|f| f.as_ref())
    }

    pub fn by_name(&self, name: &str) -> Option<&FieldSchema> {
        self.names.get(name).and_then(|&n| self.field(n))
    }

    /// Name of a field number (`"field#N"` for unknown numbers).
    pub fn name(&self, number: u32) -> String {
        match self.field(number) {
            Some(f) => f.name.clone(),
            None => format!("field#{number}"),
        }
    }

    /// The similarity scoring a field.
    pub fn similarity(&self, number: u32) -> &Similarity {
        match self.field(number).and_then(|f| f.similarity.as_deref()) {
            Some(s) => s,
            None => &self.similarity,
        }
    }

    /// Every field, by number.
    pub fn fields(&self) -> impl Iterator<Item = &FieldSchema> {
        self.fields.iter().flatten()
    }

    /// One past the largest field number.
    pub fn max_number(&self) -> u32 {
        self.fields.len() as u32
    }
}
