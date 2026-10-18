//! An index's data (the rest of `IndexSpec`): the document table, the term dictionary and its
//! inverted indexes, the per-field tag, numeric/geo, vector and geometry indexes, the
//! missing/existing documents indexes, statistics and indexing errors; and the indexing
//! pipeline (`AddDocumentCtx`, `Document_AddToIndexes`, `Indexer_Process`) that fills them.

use super::doctable::{ByteOffsets, DocTable, SortVal};
use super::error::{Code, QErr};
use super::invidx::{self, InvertedIndex, OffsetsWriter, Record, flags as iflags};
use super::spec::{self, DocType, Field, Spec};
use super::tokenize::{self, StopWords, Tokenizer};
use super::triemap::TrieMap;
use bytes::Bytes;
use std::collections::HashMap;

/// `TrieType_MemUsage` per term: a node, its parent's pointer and key rune, and two runes.
const TERM_TRIE_ENTRY: usize = 43;
/// Tags are truncated to 4096 bytes (`MAX_TAG_LEN`).
const MAX_TAG_LEN: usize = 0x1000;
/// `STEM_TOKEN_FACTOR`.
const STEM_FACTOR: f64 = 0.2;

/// `CLOCK_MONOTONIC_RAW` as (seconds, nanoseconds).
pub fn monotonic_raw() -> (i64, i64) {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid timespec to write into.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
    (ts.tv_sec, ts.tv_nsec)
}

/// `IndexError`: failures, the last one's message (with and without user data), key and time.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IndexError {
    pub count: u64,
    pub last: Option<(String, String)>,
    pub key: Option<Bytes>,
    pub at: (i64, i64),
    pub oom: bool,
}

impl IndexError {
    /// `IndexError_AddQueryError`.
    pub fn add(&mut self, e: &QErr, key: &[u8]) {
        self.last = Some((e.displayable(false), e.displayable(true)));
        self.key = Some(Bytes::copy_from_slice(key));
        self.count += 1;
        self.at = monotonic_raw();
    }
}

/// `IndexStats` (and its `ScoringIndexStats`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub num_documents: u64,
    pub num_terms: u64,
    pub total_docs_len: u64,
    pub num_records: i64,
    pub inverted_size: i64,
    pub offset_vecs_size: u64,
    pub offset_vec_records: u64,
    pub terms_size: u64,
    pub total_blocks: i64,
    /// Wall-clock nanoseconds spent indexing.
    pub total_index_time: u64,
}

/// `suffixData`: the term a suffix completes (if it is one) and every term it is a suffix of.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Suffix {
    pub term: Option<Bytes>,
    pub terms: Vec<Bytes>,
}

/// `addSuffixTrieMap`.
pub fn add_suffix(trie: &mut TrieMap<Suffix>, s: &[u8]) {
    if trie.get(s).is_some_and(|d| d.term.is_some()) {
        return;
    }
    let term = Bytes::copy_from_slice(s);
    match trie.get_mut(s) {
        Some(d) => {
            d.term = Some(term.clone());
            d.terms.push(term.clone());
        }
        None => {
            trie.insert(s, Suffix { term: Some(term.clone()), terms: vec![term.clone()] });
        }
    }
    for j in 1..s.len() {
        match trie.get_mut(&s[j..]) {
            Some(d) => d.terms.push(term.clone()),
            None => {
                trie.insert(&s[j..], Suffix { term: None, terms: vec![term.clone()] });
            }
        }
    }
}

/// A TAG field's index (`TagIndex`): a posting list per value.
#[derive(Clone, Debug, Default)]
pub struct TagIndex {
    pub values: TrieMap<InvertedIndex>,
    pub suffix: Option<TrieMap<Suffix>>,
}

impl TagIndex {
    /// `TagIndex_GetOverhead`.
    pub fn overhead(&self) -> usize {
        self.values.mem_usage() + self.suffix.as_ref().map_or(0, |s| s.mem_usage())
    }
}

/// A field's indexes and its indexing errors.
#[derive(Debug, Default)]
pub struct FieldIndex {
    pub tag: Option<TagIndex>,
    pub numeric: Option<super::numtree::NumericRangeTree>,
    /// Vectors by document id (until the vector index proper).
    pub vectors: Option<HashMap<u64, Vec<Vec<u8>>>>,
    /// Geometries (WKT) by document id.
    pub geometry: Option<HashMap<u64, Bytes>>,
    pub error: IndexError,
}

/// A value read from a document (`DocumentField`'s union).
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// A string (hash fields, JSON strings).
    Str(Bytes),
    Num(f64),
    Null,
    /// A JSON array of strings (or their string forms).
    Array(Vec<Bytes>),
    /// A JSON array of numbers (numeric fields).
    NumArray(Vec<f64>),
    /// Vectors from a JSON array: the concatenated blobs, each of `size` bytes.
    Blobs { data: Vec<u8>, size: usize, count: usize },
}

impl Value {
    /// `DocumentField_GetValueCStr`.
    fn cstr(&self) -> &[u8] {
        match self {
            Value::Str(s) => s,
            Value::Array(a) => a.first().map_or(&[][..], |s| s),
            _ => &[],
        }
    }
}

#[derive(Clone, Debug)]
pub struct DocField {
    /// The schema field's name (`docFieldName`).
    pub name: Bytes,
    pub value: Value,
    /// The multi-value's sortable form (JSON arrays).
    pub multi_sort: Option<SortVal>,
}

/// `Document`.
#[derive(Clone, Debug)]
pub struct Document {
    pub key: Bytes,
    pub fields: Vec<DocField>,
    pub language: usize,
    pub score: f64,
    pub payload: Option<Bytes>,
    pub doc_type: DocType,
    pub expire_at: i64,
    pub field_expirations: Vec<(usize, i64)>,
}

impl Document {
    pub fn new(key: &[u8], doc_type: DocType) -> Self {
        Document {
            key: Bytes::copy_from_slice(key),
            fields: Vec::new(),
            language: spec::ENGLISH,
            score: 1.0,
            payload: None,
            doc_type,
            expire_at: 0,
            field_expirations: Vec::new(),
        }
    }
}

/// Indexing settings read from the configuration when a document is added.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub min_stem_len: usize,
    pub min_phonetic_len: usize,
    pub multi_text_slop: u32,
    pub raw_doc_ids: bool,
    pub compress_floats: bool,
    pub numeric_max_depth: usize,
}

impl Settings {
    pub fn from(cfg: &crate::config::Config) -> Self {
        Settings {
            min_stem_len: cfg.num("search-min-stem-len") as usize,
            min_phonetic_len: cfg.num("search-min-phonetic-term-len") as usize,
            multi_text_slop: cfg.num("search-multi-text-slop") as u32,
            raw_doc_ids: cfg.num("search-raw-docid-encoding") != 0,
            compress_floats: cfg.num("search-_numeric-compress") != 0,
            numeric_max_depth: cfg.num("search-_numeric-ranges-parents") as usize,
        }
    }
}

/// A forward index entry (`ForwardIndexEntry`).
struct FwEntry {
    term: Vec<u8>,
    field_mask: u128,
    freq: u32,
    offsets: Option<OffsetsWriter>,
}

/// `ForwardIndex`: a document's terms in first-seen order.
#[derive(Default)]
struct Forward {
    entries: Vec<FwEntry>,
    by_term: HashMap<Vec<u8>, usize>,
    max_freq: u32,
    total_freq: u32,
}

impl Forward {
    /// `ForwardIndex_HandleToken`.
    #[allow(clippy::too_many_arguments)]
    fn handle(&mut self, term: &[u8], pos: u32, weight: f64, field: usize, stem: bool, raw: bool, offsets: bool) {
        let i = match self.by_term.get(term) {
            Some(&i) => i,
            None => {
                self.entries.push(FwEntry { term: term.to_vec(), field_mask: 0, freq: 0, offsets: offsets.then(OffsetsWriter::new) });
                self.by_term.insert(term.to_vec(), self.entries.len() - 1);
                self.entries.len() - 1
            }
        };
        let e = &mut self.entries[i];
        e.field_mask |= 1u128 << field;
        let mut score = weight as f32;
        if stem {
            score = (score as f64 * STEM_FACTOR) as f32;
        }
        let add = (score as u32).max(1);
        e.freq += add;
        self.max_freq = self.max_freq.max(e.freq);
        if raw {
            self.total_freq += add;
        }
        if let Some(w) = &mut e.offsets {
            w.write(pos);
        }
    }
}

/// Per-field indexing data (`FieldIndexerData`).
#[derive(Clone, Debug, Default)]
struct FieldData {
    is_null: bool,
    numeric: f64,
    multi: Option<Vec<f64>>,
    tags: Option<Vec<Option<Vec<u8>>>>,
    vectors: Vec<Vec<u8>>,
    geometry: Option<Bytes>,
}

const IX_TYPES: [u8; 6] = [spec::T_TEXT, spec::T_NUMERIC, spec::T_GEO, spec::T_TAG, spec::T_VECTOR, spec::T_GEOMETRY];

/// An index: its schema and its data.
#[derive(Debug)]
pub struct Index {
    pub spec: Spec,
    /// `specId`, unique for the process.
    pub id: u64,
    pub docs: DocTable,
    /// `keysDict`: a term's inverted index.
    pub terms: HashMap<Bytes, InvertedIndex>,
    /// The term trie (`sp->terms`).
    pub term_trie: super::trie::Trie,
    /// The text suffix trie (`sp->suffix`), when a text field has WITHSUFFIXTRIE.
    pub suffix: Option<TrieMap<Suffix>>,
    pub fields: Vec<FieldIndex>,
    /// `missingFieldDict`: documents missing an INDEXMISSING field, by field name.
    pub missing: HashMap<Bytes, InvertedIndex>,
    /// `existingDocs` (INDEXALL).
    pub existing: Option<InvertedIndex>,
    pub stopwords: StopWords,
    pub stats: Stats,
    pub error: IndexError,
    /// `counter`: FT.INFO's `number_of_uses`.
    pub counter: u64,
    /// Dialects queries used (bit per dialect).
    pub used_dialects: u8,
    /// `monitorDocumentExpiration` / `monitorFieldExpiration`.
    pub monitor_expiration: bool,
    pub monitor_field_expiration: bool,
    /// `aliases`: the index's aliases, in the order they were added (removed by swapping).
    pub aliases: Vec<Bytes>,
    /// `smap`: synonym terms → their groups (`~id`), created by the first FT.SYNUPDATE.
    pub synonyms: Option<super::rsdict::RsDict<Vec<Bytes>>>,
}

impl Index {
    pub fn new(spec: Spec, id: u64, cfg: &crate::config::Config) -> Self {
        let stopwords = match &spec.stopwords {
            Some(w) => StopWords::from_words(w.iter().map(|b| b.as_ref())),
            None => StopWords::default_list(),
        };
        let fields = spec.fields.iter().map(|_| FieldIndex::default()).collect();
        let suffix = (spec.flags & spec::I_SUFFIX_TRIE != 0).then(TrieMap::new);
        Index {
            id,
            docs: DocTable::new(cfg.num("search-max-doctablesize") as u64),
            terms: HashMap::new(),
            term_trie: super::trie::Trie::new(super::trie::SortMode::Lex),
            suffix,
            fields,
            missing: HashMap::new(),
            existing: None,
            stopwords,
            stats: Stats::default(),
            error: IndexError::default(),
            counter: 0,
            used_dialects: 0,
            monitor_expiration: true,
            monitor_field_expiration: true,
            aliases: Vec::new(),
            synonyms: None,
            spec,
        }
    }

    pub fn name(&self) -> &Bytes {
        &self.spec.name
    }

    /// After FT.ALTER added fields: their (empty) indexes, and the suffix trie a new
    /// WITHSUFFIXTRIE text field needs (`IndexSpec_EnsureSuffixForField`).
    pub fn fields_added(&mut self) {
        while self.fields.len() < self.spec.fields.len() {
            self.fields.push(FieldIndex::default());
        }
        if self.spec.flags & spec::I_SUFFIX_TRIE != 0 && self.suffix.is_none() {
            self.suffix = Some(TrieMap::new());
        }
    }

    /// `IndexSpec_collect_text_overhead`.
    pub fn text_overhead(&self) -> usize {
        self.term_trie.size() * TERM_TRIE_ENTRY
    }

    /// `IndexSpec_collect_tags_overhead`.
    pub fn tags_overhead(&self) -> usize {
        self.fields.iter().filter_map(|f| f.tag.as_ref()).map(|t| t.overhead()).sum()
    }

    /// `IndexSpec_collect_numeric_overhead`.
    pub fn numeric_overhead(&self) -> usize {
        self.spec
            .fields
            .iter()
            .zip(&self.fields)
            .filter(|(fs, f)| fs.is(spec::T_NUMERIC | spec::T_GEO) && f.numeric.is_some())
            .count()
            * super::numtree::NumericRangeTree::base_size()
    }

    /// `IndexSpec_TotalMemUsage`.
    pub fn total_mem(&self, vectors: usize) -> i64 {
        (self.docs.memsize + self.docs.sortables_size + self.docs.dim.mem_usage() + self.text_overhead() + self.tags_overhead() + self.numeric_overhead()) as i64
            + self.stats.inverted_size
            + self.stats.offset_vecs_size as i64
            + self.stats.terms_size as i64
            + vectors as i64
    }

    fn add_inverted(&mut self, size: usize) {
        self.stats.inverted_size += size as i64;
    }

    /// `IndexSpec_AddTerm`.
    fn add_term(&mut self, term: &[u8]) {
        if self.term_trie.insert(term, 1.0, true, None, 1) == super::trie::TRIE_OK_NEW {
            self.stats.num_terms += 1;
            self.stats.terms_size += term.len() as u64;
        }
    }

    /// `IndexSpec_DeleteDoc`: the document leaves the table; its postings stay until GC.
    pub fn delete_doc(&mut self, key: &[u8]) -> bool {
        let Some(d) = self.docs.pop(key) else { return false };
        self.stats.total_docs_len -= d.doc_len as u64;
        self.stats.num_documents -= 1;
        self.remove_vectors(d.id);
        true
    }

    fn remove_vectors(&mut self, id: u64) {
        for f in &mut self.fields {
            if let Some(v) = &mut f.vectors {
                v.remove(&id);
            }
            if let Some(g) = &mut f.geometry {
                g.remove(&id);
            }
        }
    }

    fn record_error(&mut self, field: Option<usize>, e: &QErr, key: &[u8]) {
        self.error.add(e, key);
        if let Some(i) = field {
            self.fields[i].error.add(e, key);
        }
    }

    /// A document failed to load (`IndexSpec_UpdateDoc`'s error path): the error is recorded and
    /// any previous version deleted.
    pub fn load_failed(&mut self, e: &QErr, key: &[u8]) {
        self.error.add(e, key);
        self.delete_doc(key);
    }

    /// `AddDocumentCtx_Submit(DOCUMENT_ADD_REPLACE)` for a loaded document.
    pub fn add_document(&mut self, mut doc: Document, set: &Settings) {
        let started = std::time::Instant::now();
        self.add_document_inner(&mut doc, set);
        self.stats.total_index_time += started.elapsed().as_nanos() as u64;
    }

    fn add_document_inner(&mut self, doc: &mut Document, set: &Settings) {
        let is_hash = doc.doc_type == DocType::Hash;
        // `AddDocumentCtx_SetDocument`.
        let mut fspecs: Vec<Option<usize>> = Vec::with_capacity(doc.fields.len());
        let mut index_as: Vec<u8> = Vec::with_capacity(doc.fields.len());
        let mut has_sortables = false;
        let mut has_text = false;
        let mut has_other = false;
        let mut text_fields = 0;
        let mut seen = vec![false; self.spec.fields.len()];
        for f in &doc.fields {
            let fs = self.spec.fields.iter().position(|s| s.name == f.name);
            let fs = fs.filter(|_| !(is_hash && f.value == Value::Null));
            fspecs.push(fs);
            let Some(i) = fs else {
                index_as.push(0);
                continue;
            };
            if seen[i] {
                // `QUERY_ERROR_CODE_DUP_FIELD`: the document isn't added.
                return;
            }
            seen[i] = true;
            let fs = &self.spec.fields[i];
            if fs.sortable() {
                has_sortables = true;
            }
            index_as.push(fs.types);
            if fs.indexable() {
                if fs.types & spec::T_TEXT != 0 {
                    text_fields += 1;
                    has_text = true;
                }
                if fs.types != spec::T_TEXT {
                    has_other = true;
                }
            }
        }
        let mut sv: Vec<SortVal> = if has_sortables { vec![SortVal::Null; self.spec.num_sortables] } else { Vec::new() };
        let empty = sv.is_empty() && !has_text && !has_other;
        let mut byte_offsets = (text_fields > 0 && self.spec.flags & spec::I_BYTE_OFFSETS != 0).then(ByteOffsets::default);

        // `Document_AddToIndexes`: the preprocessors.
        let stemmer = tokenize::stemmer_for(doc.language);
        let mut fw = Forward::default();
        let offsets = self.spec.flags & spec::I_TERM_OFFSETS != 0;
        let mut fdatas: Vec<FieldData> = vec![FieldData::default(); doc.fields.len()];
        {
            let mut tok = Tokenizer::with(&self.stopwords, stemmer, set.min_stem_len, set.min_phonetic_len);
            let mut total_tokens = 0u32;
            for (i, f) in doc.fields.iter_mut().enumerate() {
                let Some(fi) = fspecs[i] else { continue };
                let fs = &self.spec.fields[fi];
                for &t in &IX_TYPES {
                    if index_as[i] & t == 0 {
                        continue;
                    }
                    let rc = match t {
                        spec::T_TEXT => fulltext(fs, f, &mut sv, &mut tok, &mut fw, offsets, set, &mut total_tokens, &mut byte_offsets, self.synonyms.as_ref()),
                        spec::T_NUMERIC => numeric(fs, f, &mut fdatas[i], &mut sv),
                        spec::T_GEO => geo(fs, f, &mut fdatas[i], &mut sv),
                        spec::T_TAG => tag(fs, f, &mut fdatas[i], &mut sv),
                        spec::T_VECTOR => vector(fs, f, &mut fdatas[i]),
                        _ => geometry(f, &mut fdatas[i]),
                    };
                    if let Err(e) = rc {
                        self.record_error(Some(fi), &e, &doc.key);
                        self.delete_doc(&doc.key);
                        return;
                    }
                    break;
                }
            }
        }

        // `Indexer_Process`: nothing indexable and some sortables: the document is left as it was.
        if !has_text && !has_other && !empty {
            return;
        }

        // `doAssignIds` / `makeDocumentId`: the old version goes, the new one gets the next id.
        if let Some(old) = self.docs.pop(&doc.key) {
            self.stats.num_documents -= 1;
            self.stats.total_docs_len -= old.doc_len as u64;
            self.remove_vectors(old.id);
        }
        let id = self.docs.put(&doc.key, doc.score, 0, doc.payload.as_deref(), doc.doc_type);
        let has_expiration = doc.expire_at != 0 || !doc.field_expirations.is_empty();
        if let Some(d) = self.docs.get_mut(id) {
            d.max_freq = fw.max_freq;
            d.doc_len = fw.total_freq;
            if has_expiration {
                d.expire_at = doc.expire_at;
                d.field_expirations = std::mem::take(&mut doc.field_expirations);
            }
            if let Some(b) = byte_offsets.take() {
                d.byte_offsets = Some(b);
                d.flags |= super::doctable::DOC_HAS_OFFSET_VECTOR;
            }
        }
        self.stats.num_documents += 1;
        self.stats.total_docs_len += fw.total_freq as u64;
        if !sv.is_empty() {
            self.docs.set_sort_vector(id, sv);
        }

        // `indexText`.
        for e in &fw.entries {
            let is_new = !self.terms.contains_key(e.term.as_slice());
            if is_new {
                let (ii, size) = invidx::new_inverted_index(self.spec.flags & iflags::STORAGE_MASK, set.raw_doc_ids, set.compress_floats)
                    .expect("term index flags");
                self.add_inverted(size);
                self.terms.insert(Bytes::copy_from_slice(&e.term), ii);
            }
            let ii = self.terms.get_mut(e.term.as_slice()).unwrap();
            let bytes = e.offsets.as_ref().map_or(&[][..], |w| w.bytes());
            let r = ii.add_record(&Record::term(id, e.freq, e.field_mask, bytes));
            self.stats.inverted_size += r.mem_growth as i64;
            self.stats.total_blocks += r.blocks_added as i64;
            self.stats.num_records += 1;
            if let Some(w) = &e.offsets {
                self.stats.offset_vecs_size += w.len() as u64;
                self.stats.offset_vec_records += w.count() as u64;
            }
            if is_new && !e.term.is_empty() {
                self.add_term(&e.term);
            }
            if self.spec.suffix_mask & e.field_mask != 0
                && !e.term.is_empty()
                && ![tokenize::STEM_PREFIX, tokenize::PHONETIC_PREFIX, tokenize::SYNONYM_PREFIX].contains(&e.term[0])
                && let Some(s) = &mut self.suffix
            {
                add_suffix(s, crate::util::cstr(&e.term));
            }
        }

        // `bulkIndexFields`.
        if has_other {
            for (i, f) in doc.fields.iter().enumerate() {
                let Some(fi) = fspecs[i] else { continue };
                let fs = &self.spec.fields[fi];
                if fs.types == spec::T_TEXT || !fs.indexable() || fdatas[i].is_null {
                    continue;
                }
                let _ = f;
                if let Err(e) = self.bulk_add(fi, index_as[i], &fdatas[i], id, set) {
                    self.record_error(Some(fi), &e, &doc.key);
                    break;
                }
            }
        }

        // `writeExistingDocs`.
        if self.spec.rule.index_all {
            if self.existing.is_none() {
                let (ii, size) = invidx::new_inverted_index(iflags::DOC_IDS_ONLY, set.raw_doc_ids, false).unwrap();
                self.existing = Some(ii);
                self.add_inverted(size);
            }
            let r = self.existing.as_mut().unwrap().add_record(&Record::doc(id));
            self.stats.inverted_size += r.mem_growth as i64;
            self.stats.total_blocks += r.blocks_added as i64;
        }

        // `writeMissingFieldDocs`: INDEXMISSING fields the document lacks, and those with a TTL.
        let mut missing: Vec<Bytes> = self
            .spec
            .fields
            .iter()
            .filter(|fs| fs.has(spec::F_INDEX_MISSING) && !doc.fields.iter().any(|f| f.name == fs.name))
            .map(|fs| fs.name.clone())
            .collect();
        if let Some(d) = self.docs.get(id) {
            for &(fi, _) in &d.field_expirations {
                let fs = &self.spec.fields[fi];
                if fs.has(spec::F_INDEX_MISSING) && !missing.contains(&fs.name) {
                    missing.push(fs.name.clone());
                }
            }
        }
        for name in missing {
            if !self.missing.contains_key(&name) {
                let (ii, size) = invidx::new_inverted_index(iflags::DOC_IDS_ONLY, set.raw_doc_ids, false).unwrap();
                self.add_inverted(size);
                self.missing.insert(name.clone(), ii);
            }
            let r = self.missing.get_mut(&name).unwrap().add_record(&Record::doc(id));
            self.stats.inverted_size += r.mem_growth as i64;
            self.stats.total_blocks += r.blocks_added as i64;
        }
    }

    /// `IndexerBulkAdd` + `IndexerBulkApply` for one field.
    fn bulk_add(&mut self, fi: usize, index_as: u8, fd: &FieldData, id: u64, set: &Settings) -> Result<(), QErr> {
        for &t in &IX_TYPES {
            if index_as & t == 0 {
                continue;
            }
            match t {
                spec::T_TAG => {
                    let with_suffix = self.spec.fields[fi].has(spec::F_SUFFIX_TRIE);
                    let tags = fd.tags.as_deref().unwrap_or(&[]);
                    let field = &mut self.fields[fi];
                    let idx = field.tag.get_or_insert_with(|| TagIndex { values: TrieMap::new(), suffix: with_suffix.then(TrieMap::new) });
                    let mut records = 0;
                    let mut size = 0i64;
                    let mut blocks = 0i64;
                    for t in tags.iter().flatten() {
                        let mut created = 0;
                        if idx.values.get(t).is_none() {
                            let (ii, sz) = invidx::new_inverted_index(iflags::DOC_IDS_ONLY, set.raw_doc_ids, false).unwrap();
                            idx.values.insert(t, ii);
                            created = sz;
                        }
                        let ii = idx.values.get_mut(t).unwrap();
                        let before = ii.unique_docs();
                        let r = ii.add_record(&Record::doc(id));
                        if ii.unique_docs() > before {
                            records += 1;
                        }
                        blocks += r.blocks_added as i64;
                        size += r.mem_growth as i64 + created as i64;
                    }
                    // `TagIndex_Commit`.
                    if let Some(s) = &mut idx.suffix {
                        for t in tags.iter().flatten() {
                            if !t.is_empty() {
                                add_suffix(s, t);
                            }
                        }
                    }
                    self.stats.inverted_size += size;
                    self.stats.total_blocks += blocks;
                    self.stats.num_records += records;
                }
                spec::T_NUMERIC | spec::T_GEO => {
                    if self.fields[fi].numeric.is_none() {
                        // `open_numeric_or_geo_index`: the new tree's empty root range counts.
                        let tree = super::numtree::NumericRangeTree::new(set.compress_floats);
                        self.stats.inverted_size += tree.initial_size() as i64;
                        self.fields[fi].numeric = Some(tree);
                    }
                    let tree = self.fields[fi].numeric.as_mut().unwrap();
                    let values: Vec<(f64, bool)> = match &fd.multi {
                        Some(v) => v.iter().map(|&x| (x, true)).collect(),
                        None => vec![(fd.numeric, false)],
                    };
                    for (v, multi) in values {
                        let r = tree.add(id, v, multi, set.numeric_max_depth);
                        self.stats.inverted_size += r.size_delta;
                        self.stats.num_records += r.num_records_delta as i64;
                        self.stats.total_blocks += r.block_count_delta as i64;
                    }
                }
                spec::T_VECTOR => {
                    self.fields[fi].vectors.get_or_insert_with(HashMap::new).insert(id, fd.vectors.clone());
                }
                spec::T_GEOMETRY => {
                    if let Some(g) = &fd.geometry {
                        self.fields[fi].geometry.get_or_insert_with(HashMap::new).insert(id, g.clone());
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// `RSSortingVector_PutStrNormalize`: case-folded, or an error for invalid UTF-8.
fn sort_normalized(s: &[u8]) -> Result<SortVal, QErr> {
    match std::str::from_utf8(crate::util::cstr(s)) {
        Ok(s) => Ok(SortVal::Str(Bytes::from(super::casefold::fold(s)))),
        Err(_) => Err(QErr::new(Code::BadVal, "Invalid UTF-8")),
    }
}

/// A sortable string slot: as is with UNF, else normalized.
fn put_sort_str(fs: &Field, sv: &mut [SortVal], s: &[u8]) -> Result<(), QErr> {
    let v = if fs.has(spec::F_UNF) { SortVal::Str(Bytes::copy_from_slice(crate::util::cstr(s))) } else { sort_normalized(s)? };
    sv[fs.sort_idx as usize] = v;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// `fulltextPreprocessor`.
fn fulltext(
    fs: &Field,
    f: &mut DocField,
    sv: &mut [SortVal],
    tok: &mut Tokenizer,
    fw: &mut Forward,
    offsets: bool,
    set: &Settings,
    total_tokens: &mut u32,
    byte_offsets: &mut Option<ByteOffsets>,
    syn: Option<&super::rsdict::RsDict<Vec<Bytes>>>,
) -> Result<(), QErr> {
    let values: Vec<Bytes> = match &f.value {
        Value::Null => return Ok(()),
        Value::Str(s) => vec![s.clone()],
        Value::Array(a) => a.clone(),
        _ => return Err(QErr::code(Code::Generic)),
    };
    if fs.sortable() {
        match &f.value {
            Value::Str(s) => put_sort_str(fs, sv, s)?,
            _ => {
                if let Some(m) = f.multi_sort.take() {
                    sv[fs.sort_idx as usize] = m;
                }
            }
        }
    }
    if !fs.indexable() {
        return Ok(());
    }
    let ft_id = fs.ft_id.unwrap_or(0);
    let mut options = 0;
    if fs.has(spec::F_NOSTEM) {
        options |= tokenize::NOSTEM;
    }
    if fs.has(spec::F_PHONETICS) {
        options |= tokenize::PHONETICS;
    }
    let delta = if values.len() > 1 && set.multi_text_slop > 0 { set.multi_text_slop - 1 } else { 0 };
    let index_empty = fs.has(spec::F_INDEX_EMPTY);
    let field_entry = byte_offsets.as_mut().map(|b| {
        b.fields.push((ft_id, *total_tokens + 1, 0));
        b.fields.len() - 1
    });
    for v in &values {
        tok.start(v, options);
        while let Some(t) = tok.next_token() {
            if let Some(b) = byte_offsets.as_mut() {
                b.offsets.push(t.raw_start as u32);
            }
            if !index_empty && t.tok.is_empty() {
                continue;
            }
            fw.handle(&t.tok, t.pos, fs.weight, ft_id, false, true, offsets);
            if let Some(stem) = &t.stem {
                fw.handle(stem, t.pos, fs.weight, ft_id, true, false, offsets);
            }
            // `forwardIndexTokenFunc`: the token's synonym groups (`~id`), then its phonetics.
            if let Some(ids) = syn.and_then(|m| m.get(crate::util::cstr(&t.tok))) {
                for id in ids {
                    fw.handle(crate::util::cstr(id), t.pos, fs.weight, ft_id, false, false, offsets);
                }
            }
            if let Some(p) = &t.phonetic {
                fw.handle(p, t.pos, fs.weight, ft_id, false, false, offsets);
            }
        }
        let last = tok.last_offset;
        if let (Some(b), Some(i)) = (byte_offsets.as_mut(), field_entry) {
            b.fields[i].2 = last;
        }
        *total_tokens = last;
        tok.last_offset += delta;
    }
    tok.last_offset -= delta;
    Ok(())
}

/// `RedisModule_StringToDouble`: the whole string, no spaces, not NaN.
pub fn string_to_double(s: &[u8]) -> Option<f64> {
    crate::commands::zsets::zfloat(s)
}

/// `numericPreprocessor`.
fn numeric(fs: &Field, f: &mut DocField, fd: &mut FieldData, sv: &mut [SortVal]) -> Result<(), QErr> {
    match &f.value {
        Value::Str(s) => match string_to_double(s) {
            Some(v) => fd.numeric = v,
            None => {
                return Err(QErr::user(Code::NumericValueInvalid, "Invalid numeric value", format!(": '{}'", spec::text(s))));
            }
        },
        Value::Num(n) => fd.numeric = *n,
        Value::Null => {
            fd.is_null = true;
            return Ok(());
        }
        Value::NumArray(a) => fd.multi = Some(a.clone()),
        _ => return Err(QErr::code(Code::Generic)),
    }
    if fs.sortable() {
        if fd.multi.is_none() {
            sv[fs.sort_idx as usize] = SortVal::Num(fd.numeric);
        } else if let Some(m) = f.multi_sort.take() {
            sv[fs.sort_idx as usize] = m;
        }
    }
    Ok(())
}

/// `parseGeo`: "lon,lat" or "lon lat".
pub fn parse_geo(s: &[u8]) -> Result<(f64, f64), QErr> {
    let bad = |m: String| QErr::new(Code::ParseArgs, m);
    let s = std::str::from_utf8(s).map_err(|_| bad("Invalid geo string: not valid UTF-8".into()))?;
    let (lon, lat) = s.split_once([',', ' ']).ok_or_else(|| bad("Invalid geo string: missing separator".into()))?;
    let num = |x: &str| -> Result<f64, QErr> {
        let x = x.trim();
        let v = x.parse::<f64>().map_err(|e| bad(format!("Invalid geo string {x:?}: {e}")))?;
        if v.is_finite() { Ok(v) } else { Err(bad("Geo coordinates must be finite".into())) }
    };
    let lon = num(lon)?;
    let lat = num(lat)?;
    Ok((lon, lat))
}

/// `encodeGeo`: the point's 52-bit geohash, or the invalid coordinates error.
pub fn encode_geo(lon: f64, lat: f64) -> Result<f64, QErr> {
    super::geo::encode_geo(lon, lat)
        .map(|b| b as f64)
        .ok_or_else(|| QErr::user(Code::GeoCoordinatesInvalid, "Invalid geo coordinates", format!(": {lon:.6}, {lat:.6}")))
}

/// `geoPreprocessor`.
fn geo(fs: &Field, f: &mut DocField, fd: &mut FieldData, sv: &mut [SortVal]) -> Result<(), QErr> {
    let strs: Vec<Bytes> = match &f.value {
        Value::Str(s) => vec![s.clone()],
        Value::Array(a) => a.clone(),
        Value::Null => {
            fd.is_null = true;
            return Ok(());
        }
        _ => return Err(QErr::code(Code::Generic)),
    };
    let mut first: Option<Bytes> = None;
    if strs.len() == 1 {
        let (lon, lat) = parse_geo(&strs[0])?;
        fd.numeric = encode_geo(lon, lat)?;
        first = Some(strs[0].clone());
    } else if strs.len() > 1 {
        let mut arr = Vec::with_capacity(strs.len());
        for s in &strs {
            let (lon, lat) = parse_geo(s)?;
            arr.push(encode_geo(lon, lat)?);
        }
        fd.multi = Some(arr);
        first = Some(strs[0].clone());
    }
    if let Some(s) = first
        && fs.sortable()
    {
        match &f.value {
            Value::Array(_) => {
                if let Some(m) = f.multi_sort.take() {
                    sv[fs.sort_idx as usize] = m;
                }
            }
            _ => put_sort_str(fs, sv, &s)?,
        }
    }
    Ok(())
}

/// `TagIndex_SepString` over a C string: the next trimmed token, `None` when done.
fn sep_string<'a>(s: &mut &'a [u8], sep: u8, index_empty: bool) -> Option<&'a [u8]> {
    let is_space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r');
    let mut start = 0;
    let b = *s;
    if !index_empty {
        while start < b.len() && (is_space(b[start]) || b[start] == sep) {
            start += 1;
        }
    } else {
        let found_space = !b.is_empty() && is_space(b[0]);
        while start < b.len() && is_space(b[start]) {
            start += 1;
        }
        if start < b.len() && b[start] == sep {
            *s = &b[start + 1..];
            return Some(&[]);
        } else if start == b.len() && found_space {
            *s = &b[start..];
            return Some(&[]);
        }
    }
    if start == b.len() {
        *s = &b[start..];
        return None;
    }
    let mut end = start;
    let mut last = start;
    while end < b.len() {
        if b[end] == sep {
            end += 1;
            break;
        }
        if !is_space(b[end]) {
            last = end;
        }
        end += 1;
    }
    *s = &b[end..];
    Some(&b[start..=last])
}

/// `tokenizeTagString`.
fn tag_tokens(s: &[u8], fs: &Field, out: &mut Vec<Option<Vec<u8>>>) {
    let s = crate::util::cstr(s);
    let index_empty = fs.has(spec::F_INDEX_EMPTY);
    let norm = |t: &[u8]| -> Vec<u8> {
        if fs.tag_case_sensitive {
            t[..t.len().min(MAX_TAG_LEN)].to_vec()
        } else {
            let l = tokenize::lowercase(t);
            if l.len() > t.len() { l } else { l[..l.len().min(MAX_TAG_LEN)].to_vec() }
        }
    };
    if fs.tag_sep == 0 {
        // JSON's default: the whole value is one tag.
        out.push(Some(norm(s)));
        return;
    }
    let last_is_sep = s.last() == Some(&fs.tag_sep);
    let mut p = s;
    let mut advanced = false;
    while let Some(t) = sep_string(&mut p, fs.tag_sep, index_empty) {
        advanced = true;
        out.push(Some(norm(t)));
    }
    if index_empty {
        let untouched = !advanced && p.len() == s.len();
        out.push(if untouched || last_is_sep { Some(Vec::new()) } else { None });
    }
}

/// `tagPreprocessor`.
fn tag(fs: &Field, f: &mut DocField, fd: &mut FieldData, sv: &mut [SortVal]) -> Result<(), QErr> {
    let mut tags = Vec::new();
    match &f.value {
        Value::Str(s) => tag_tokens(s, fs, &mut tags),
        Value::Array(a) => {
            for s in a {
                tag_tokens(s, fs, &mut tags);
            }
        }
        Value::Null => {
            fd.is_null = true;
            fd.tags = Some(tags);
            return Ok(());
        }
        _ => return Err(QErr::code(Code::Generic)),
    }
    fd.tags = Some(tags);
    if fs.sortable() {
        match &f.value {
            Value::Array(_) => {
                if let Some(m) = f.multi_sort.take() {
                    sv[fs.sort_idx as usize] = m;
                }
            }
            v => {
                let s = v.cstr().to_vec();
                put_sort_str(fs, sv, &s)?;
            }
        }
    }
    Ok(())
}

/// `vectorPreprocessor`.
fn vector(fs: &Field, f: &mut DocField, fd: &mut FieldData) -> Result<(), QErr> {
    let expected = fs.vector.as_ref().map_or(0, |v| v.blob_size());
    let (len, vecs) = match &f.value {
        Value::Str(s) => (s.len(), vec![s.to_vec()]),
        Value::Blobs { data, size, count } => (*size, data.chunks(*size.max(&1)).take(*count).map(|c| c.to_vec()).collect()),
        Value::Null => {
            fd.is_null = true;
            return Ok(());
        }
        _ => (0, Vec::new()),
    };
    if len != expected {
        return Err(QErr::user(
            Code::VectorBlobSizeMismatch,
            "Could not add vector with blob size",
            format!(" {len} (expected size {expected})"),
        ));
    }
    fd.vectors = vecs;
    Ok(())
}

/// `geometryPreprocessor`: WKT strings are kept for the geometry index.
fn geometry(f: &mut DocField, fd: &mut FieldData) -> Result<(), QErr> {
    match &f.value {
        Value::Str(s) => fd.geometry = Some(s.clone()),
        Value::Num(_) | Value::Null | Value::Array(_) => {}
        _ => return Err(QErr::code(Code::Generic)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_doc(key: &str, fields: &[(&str, &str)]) -> Document {
        let mut d = Document::new(key.as_bytes(), DocType::Hash);
        for (n, v) in fields {
            d.fields.push(DocField { name: Bytes::copy_from_slice(n.as_bytes()), value: Value::Str(Bytes::copy_from_slice(v.as_bytes())), multi_sort: None });
        }
        d
    }

    /// The reference's FT.INFO for `t TEXT SORTABLE n NUMERIC tg TAG SEPARATOR ; CASESENSITIVE g GEO`
    /// over two hashes, to the byte.
    #[test]
    fn reference_info_numbers() {
        let cfg = crate::config::Config::default();
        let args: Vec<Bytes> = "SCHEMA t TEXT SORTABLE n NUMERIC tg TAG SEPARATOR ; CASESENSITIVE g GEO"
            .split(' ')
            .map(|a| Bytes::copy_from_slice(a.as_bytes()))
            .collect();
        let spec = Spec::parse(Bytes::from_static(b"idx"), &args, &cfg).unwrap();
        let mut ix = Index::new(spec, 0, &cfg);
        let set = Settings::from(&cfg);
        ix.add_document(hash_doc("zzd1", &[("t", "hello world running"), ("n", "5"), ("tg", "a;B"), ("g", "1.5,2.5")]), &set);
        ix.add_document(hash_doc("zzd2", &[("t", "foo bar"), ("n", "7")]), &set);
        let s = &ix.stats;
        assert_eq!((s.num_documents, ix.docs.max_doc_id, s.num_terms, s.num_records), (2, 2, 6, 11));
        assert_eq!((s.inverted_size, s.total_blocks, s.offset_vecs_size, s.offset_vec_records), (968, 10, 6, 6));
        assert_eq!((ix.docs.memsize, ix.docs.sortables_size, ix.docs.dim.mem_usage()), (8212, 106, 120));
        assert_eq!((ix.tags_overhead(), ix.text_overhead()), (112, 258));
        assert_eq!(ix.total_mem(0), 10033);
    }

    fn tags(s: &str, sep: u8, empty: bool) -> Vec<Option<String>> {
        let mut fs = Field::test("t", spec::T_TAG);
        fs.tag_sep = sep;
        if empty {
            fs.options |= spec::F_INDEX_EMPTY;
        }
        let mut out = Vec::new();
        tag_tokens(s.as_bytes(), &fs, &mut out);
        out.into_iter().map(|t| t.map(|t| String::from_utf8(t).unwrap())).collect()
    }

    #[test]
    fn tag_split() {
        let s = |v: &[&str]| v.iter().map(|x| Some(x.to_string())).collect::<Vec<_>>();
        assert_eq!(tags("a;B", b';', false), s(&["a", "b"]));
        assert_eq!(tags(" foo , bar ,, baz ", b',', false), s(&["foo", "bar", "baz"]));
        assert_eq!(tags("", b',', false), s(&[]));
        assert_eq!(tags("", b',', true), s(&[""]));
        assert_eq!(tags("a,", b',', true), s(&["a", ""]));
        // `TagIndex_Preprocess` appends the loop's final NULL unless the value was empty or
        // ended with a separator; indexing skips it.
        let with_null = |v: &[&str]| {
            let mut v = s(v);
            v.push(None);
            v
        };
        assert_eq!(tags("a,,b", b',', true), with_null(&["a", "", "b"]));
        assert_eq!(tags(" ", b',', true), with_null(&[""]));
        assert_eq!(tags("a,b", b',', true), with_null(&["a", "b"]));
    }

    #[test]
    fn geo_strings() {
        assert_eq!(parse_geo(b"1.5,2").unwrap(), (1.5, 2.0));
        assert_eq!(parse_geo(b"1.5 2").unwrap(), (1.5, 2.0));
        assert_eq!(parse_geo(b"abc,def").unwrap_err().text(), "SEARCH_PARSE_ARGS Invalid geo string \"abc\": invalid float literal");
        assert_eq!(parse_geo(b"12").unwrap_err().text(), "SEARCH_PARSE_ARGS Invalid geo string: missing separator");
        assert_eq!(
            encode_geo(500.0, 10.0).unwrap_err().text(),
            "SEARCH_GEO_COORDINATES_INVALID Invalid geo coordinates: 500.000000, 10.000000"
        );
    }
}
