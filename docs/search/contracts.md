# roll search — internal contracts

Conventions and cross-crate interfaces every roll search crate follows. Read `AGENTS.md` and
[architecture.md](architecture.md) first. The owner of a crate may refine its API, but must
update this file in the same change when a signature other crates use moves.

## Ports and flags

| listener                     | rolld default       | flag / env                                  |
| ---------------------------- | ------------------- | ------------------------------------------- |
| Elasticsearch REST (HTTP/S)  | `127.0.0.1:59200`   | `--search-http` / `ROLL_SEARCH_HTTP`        |
| roll API (`/v1/search/...`)  | rolld API `:7070`   | `--api` / `ROLL_API`                        |

Other `rolld` flags (`crates/rolld/src/search.rs`, `#[derive(clap::Args)] SearchArgs`):
`--search-tls-cert/-key`, `--search-domain`, `--search-idle-unload` (300 s),
`--search-cache-bytes`, `--search-sync-objects`, `--search-proxy-protocol`.

## Version and dialect

- Reported version: Elasticsearch `9.5.5` (`lucene_version` `10.5.1`,
  `minimum_wire_compatibility_version` `8.19.0`, `minimum_index_compatibility_version` `8.0.0`,
  `build_flavor` `default`), configurable per node. OpenSearch dialect: `3.9.0`,
  `distribution: "opensearch"`, OpenSearch tagline.
- Every response, errors included, carries `X-Elastic-Product: Elasticsearch` in the
  elasticsearch dialect. `Content-Type` echoes the request's vendor media type
  (`application/vnd.elasticsearch+json; compatible-with=N`); accept 7, 8 and 9, with or without
  the space before `compatible-with`, and Accept lists like `…,text/plain`.
- `GET /_capabilities` answers for the capabilities we implement; the YAML runner relies on it.

## Errors

`search-es` owns `EsError`, rendered as Elasticsearch renders exceptions:

```json
{"error":{"root_cause":[{"type":"index_not_found_exception","reason":"no such index [x]",
  "index":"x","resource.type":"index_or_alias","resource.id":"x","index_uuid":"_na_"}],
  "type":"index_not_found_exception","reason":"no such index [x]", …},"status":404}
```

Types, reasons and extra fields must match Elasticsearch's exactly where tests or clients look
at them (`catch: /regex/` in the YAML suites matches on them). Lower crates return their own
error types carrying an Elasticsearch `type` and `reason` (e.g. `search-analysis` returns
`illegal_argument_exception` with Elasticsearch's text) so `search-es` can pass them through.

## Allowed dependencies

Workspace deps only (root `Cargo.toml` `[workspace.dependencies]`). Beyond what is already there,
roll search may add: `fst` (term dictionary FSTs, automata), `memmap2`, `memchr`, `smallvec`,
`hashbrown`, `unicode-segmentation`, `unicode-normalization`, `regex-syntax` (Lucene regexp and
Painless regex parsing; the automata are ours), `hyper`/`hyper-util`/`http-body-util` (the REST
listener), `murmur3` hashing is ours (Elasticsearch's routing variant). Anything else needs a line
in this file explaining why.

- `search-yaml` parses YAML with its own parser (`src/yaml/`), not a crate: Elasticsearch reads
  the suites with SnakeYAML, which accepts layouts the strict parsers reject (flow collections and
  quoted scalars continuing at a lower indentation) and types scalars by YAML 1.1 rules, and the
  runner needs each scalar's source text (`id: 01` is sent as `01`). It is checked against
  PyYAML on every file of the three suites (identical trees except SnakeYAML's YAML 1.1 floats). Edit the root `Cargo.toml` with targeted edits only — several
people work in it at once.
- `search-index` uses `libm` for fdlibm-exact `sin`/`cos`/`asin`/`sqrt` (Lucene's `SloppyMath`
  tables behind geo distances and sorts must round like Java's `StrictMath`, which the platform
  libm does not guarantee) and `foldhash` (fixed-seed term hashing in the indexing buffer).


## Performance rules

- No allocation per document or per posting in hot loops: reuse buffers, decode into fixed
  blocks of 128, iterate with cursors.
- Postings, doc values and points decode with SIMD where it pays: `std::arch` with runtime
  feature detection on x86-64 (SSE4.1/AVX2) and NEON on aarch64, with a scalar fallback that is
  tested against the SIMD path.
- Never size an allocation from a count or length a peer sent; bound it by the bytes actually
  present (`AGENTS.md`). Requests are limited by `http.max_content_length` (100 MB).
- Account memory per cluster (buffers, readers, caches) so the node can enforce budgets and the
  dashboard can show it.

## `search-analysis`

```rust
pub struct Token<'a> { pub term: &'a str, pub start_offset: u32, pub end_offset: u32,   // UTF-16 offsets
                       pub position: u32, pub position_increment: u32, pub position_length: u32,
                       pub kind: &'a str, pub keyword: bool, pub payload: Option<&'a [u8]> }
pub struct Analyzer;      // cheap to clone (Arc inside); Send + Sync; pooled chains, no per-token allocation
impl Analyzer {
    pub fn analyze(&self, text: &str, sink: &mut dyn FnMut(&Token));   // errors dropped
    pub fn analyze_values(&self, values: &[&str], sink: &mut dyn FnMut(&Token)); // position/offset gap between values
    pub fn try_analyze(&self, text: &str, sink: …) -> Result<(), AnalysisError>; // analysis-time failures (condition script, delimited_payload)
    pub fn try_analyze_values(&self, values: &[&str], sink: …) -> Result<(), AnalysisError>;
    pub fn terms(&self, text: &str) -> Vec<String>;
    pub fn normalize(&self, text: &str) -> String;                      // multi-term normalization chain
    pub fn explain(&self, texts: &[&str], attributes: &[String]) -> Explain;   // `_analyze` explain as data
    pub fn explain_limited(&self, texts, attributes, max_tokens: usize) -> Result<Explain, AnalysisError>;
    pub fn position_increment_gap(&self) -> i32;                        // 100 by default; may be negative, as in ES
    pub fn offset_gap(&self) -> u32;
    pub fn analysis_mode(&self) -> AnalysisMode;                        // All | IndexTime | SearchTime
    pub fn check_allowed_in_mode(&self, mode: AnalysisMode) -> Result<(), AnalysisError>; // updateable synonyms etc.
    pub fn name(&self) -> &str; pub fn is_custom(&self) -> bool;
    // Field-mapper wrappers (Elasticsearch's AnalyzerWrappers), each a new Analyzer:
    pub fn with_index_prefixes(&self, min_chars, max_chars, position_increment_gap: i32) -> Analyzer; // text `._index_prefix`
    pub fn with_index_phrases(&self, position_increment_gap: i32) -> Analyzer;                      // text `._index_phrase`
    pub fn with_shingles(&self, shingle_size /* 2..=4 */) -> Result<Analyzer, AnalysisError>;       // search_as_you_type `._Ngram`
    pub fn with_shingles_and_prefixes(&self, max_shingle_size) -> Result<Analyzer, AnalysisError>; // search_as_you_type `._index_prefix`
}
pub struct Normalizer; impl Normalizer { pub fn normalize(&self, text: &str) -> String; pub fn analyzer(&self) -> &Analyzer; }
pub struct Explain { pub custom_analyzer: bool, pub charfilters: Vec<CharFilteredText>, pub tokenizer: Option<TokenList>,
                     pub tokenfilters: Vec<TokenList>, pub analyzer: Option<TokenList> }  // .to_json() = ES `detail`
pub struct IndexOptions { pub version: IndexVersion, pub max_ngram_diff: i32, pub max_shingle_diff: i32,
                          pub scripts: Option<Arc<dyn ScriptHost>>,        // `condition` / `predicate_token_filter`
                          pub synonym_sets: Option<Arc<dyn SynonymSets>>,  // `synonyms_set`
                          pub max_token_count: usize, pub index_name: Option<String> }   // Default: ES defaults
pub trait ScriptHost: Send + Sync { fn compile_predicate(&self, script: &Value) -> Result<Arc<dyn TokenPredicate>, AnalysisError>; }
pub trait TokenPredicate: Send + Sync { fn test(&self, token: &ScriptToken<'_>) -> Result<bool, AnalysisError>; }
pub trait SynonymSets: Send + Sync { fn rules(&self, set: &str) -> Option<Vec<String>>; }   // Solr-format rules
pub struct Analysis;      // one index's analysis chain, built from `index.analysis` settings
impl Analysis {
    pub fn from_settings(analysis: &serde_json::Value, opts: &IndexOptions) -> Result<Analysis, AnalysisError>;
    pub fn global() -> &'static Analysis;                         // `_analyze` without an index
    pub fn global_with(opts: IndexOptions) -> Analysis;           // same, with script / synonym-set hooks
    pub fn analyzer(&self, name: &str) -> Option<Analyzer>;      // custom then built-in
    pub fn search_analyzer(&self, name: &str) -> Option<Analyzer>;
    pub fn default_analyzer(&self) -> Analyzer;
    pub fn default_search_analyzer(&self) -> Option<Analyzer>;       // `default_search`
    pub fn default_search_quote_analyzer(&self) -> Option<Analyzer>; // `default_search_quoted`
    pub fn normalizer(&self, name: &str) -> Option<Normalizer>;
    pub fn whitespace_normalizer(&self, name: &str) -> Option<Normalizer>; // split_queries_on_whitespace
    pub fn build_ad_hoc_json(&self, def: &serde_json::Value) -> Result<Analyzer, AnalysisError>; // {tokenizer, filter, char_filter}
    pub fn analyze(&self, req: &AnalyzeRequest, field_analyzer: Option<&Analyzer>, limits: AnalyzeLimits)
        -> Result<serde_json::Value, AnalysisError>;              // whole `_analyze` response body
}
impl AnalyzeRequest { pub fn from_json(body: &Value) -> Result<AnalyzeRequest, AnalysisError>; }  // public fields text, analyzer, tokenizer, filter, char_filter, field, explain, attributes, normalizer
pub struct ThaiDictionary;  // word list of the `thai` tokenizer
impl ThaiDictionary { pub fn from_words(words: impl IntoIterator<Item = impl AsRef<str>>) -> Self;
                      pub fn from_jdk(thai_dict: &[u8]) -> Result<Self, AnalysisError>; }   // the JDK's binary thai_dict
pub fn set_thai_dictionary(d: ThaiDictionary) -> Result<(), AnalysisError>;  // process-wide, before first use
pub fn warm_up();   // builds the lazily built global scanners (uax_url_email ~0.4 s) — call at server start, off the request path
pub struct AnalysisError { pub kind: &'static str, pub reason: String, pub status: u16,
                           pub caused_by: Option<Box<AnalysisError>> }   // .to_json(), .root_cause() (ES guessRootCauses); "" reason = Java null
pub fn is_java_exception(kind: &str) -> bool;   // plain Java exception types (not ElasticsearchException) among analysis errors
```

Covers every built-in analyzer, tokenizer, token filter and char filter Elasticsearch ships in
`analysis-common` and core, including all language analyzers (the `snowball` crate's generated
stemmers on our own allocation-free runtime, `src/stem/sb`, with the changes of the Snowball
snapshot Lucene 10.5.1 ships, plus our ports of Lucene's Snowball 2.2
Estonian/Romanian/Porter/Swedish/German; every stemmer counts UTF-16 units as Lucene does, and
all of them match Elasticsearch on the full Snowball test vocabularies),
synonyms and synonym_graph (inline and `synonyms_path` is rejected the way a node without the
file rejects it), word_delimiter(_graph), ngram/edge_ngram, shingle, stop with `_lang_` lists,
keyword_marker, hunspell is out of scope. Java regex semantics (pattern tokenizer and filters)
come from its own engine (`jregex.rs`). Checked against ES 9.5.5 `_analyze` output in
`tests/fixtures/*.json` (regenerate with `tests/fixtures/gen_fixtures.py`). The `thai` tokenizer
reproduces the JDK's chunking, sentence and dictionary-based word iterators; its built-in word
list is ICU's (Unicode License), so Thai words outside the JDK list's overlap can split
differently from Elasticsearch. Installing the JDK's `thai_dict` with `set_thai_dictionary` makes
it identical (the fixture test does so when `ROLL_THAI_JDK_DICT` points at one).

## `search-codec`

The segment format. A segment is **one file** (one object): a header, 8-byte-aligned sections,
and a footer with a table of contents and CRC32Cs (per section and whole file). Readers work over a
`Slice` (cheap-clone `bytes::Bytes` view: `Vec<u8>`, `Bytes` or `Slice::map_file`) and never copy
whole sections. Doc ids are `u32`; `NO_MORE_DOCS = i32::MAX as u32` (Lucene's sentinel),
`MAX_DOCS = i32::MAX - 128`. Errors: `search_codec::Error { Corrupt(String), InvalidArgument(String),
Io(io::Error) }` (`From<Error> for io::Error`).

```rust
// Metadata (info.rs). Field numbers are the shard's; a segment's fields are sorted by number.
pub struct SegmentInfo { pub id: [u8;16], pub max_doc: u32, pub fields: Vec<FieldInfo>,
                         pub sort: Option<IndexSort>, pub attributes: BTreeMap<String,String> }
impl SegmentInfo { fn field(&self, number) -> Option<&FieldInfo>; fn field_by_name(&self, &str) -> Option<&FieldInfo> }
pub struct FieldInfo { pub number: u32, pub name: String, pub index: IndexOptions, pub norms: bool, pub payloads: bool,
                       pub doc_values: DocValuesType, pub doc_values_skip_index: bool /* skip index is always written */,
                       pub points: Option<PointsSpec>, pub vectors: Option<VectorSpec>,
                       pub bloom_filter: bool /* Bloom filter over the terms, sized for one term per doc: use for `_id` (ES does) */,
                       pub suggest: bool /* carries suggest (completion) entries, see SuggestReader */,
                       pub attributes: BTreeMap<String,String> }
FieldInfo::new(number, name).with_index(IndexOptions, norms).with_doc_values(t).with_points(spec).with_vectors(spec).with_bloom_filter().with_suggest()
pub enum IndexOptions { None, Docs, DocsAndFreqs, DocsAndFreqsAndPositions, DocsAndFreqsAndPositionsAndOffsets }  // has_freqs()/has_positions()/has_offsets()
pub enum DocValuesType { None, Numeric, Binary, Sorted, SortedSet, SortedNumeric }
pub struct PointsSpec { pub dims: u8 /* ≤16 */, pub index_dims: u8 /* ≤8 */, pub bytes_per_dim: u8 /* ≤32 */ }  // PointsSpec::new(dims, bpd)
pub struct VectorSpec { pub dims: u32, pub element: VectorElement /* Float32|Byte|Bit */, pub similarity: VectorSimilarity
                        /* L2Norm|DotProduct|Cosine|MaxInnerProduct */, pub hnsw: Option<HnswParams { m, ef_construction }>,
                        pub quantization: VectorQuantization /* None | Scalar { bits: 8|7|4, confidence_interval } (int8/int4_hnsw/flat)
                        | Binary (bbq_hnsw/bbq_flat: 1 bit/dim + corrections, 4-bit queries) — float fields only */ }
pub struct IndexSort { pub fields: Vec<SortField { field, kind: SortKind /* Long|String */, reverse, missing: SortMissing /* First|Last, of the final order */,
                                                    selector: SortSelector /* Min|Max */ }> }

// Writing (source.rs, writer.rs). The indexing buffer and merges implement `SegmentSource`; the
// writer validates every ordering invariant and returns InvalidArgument on violations.
pub trait SegmentSource {
    fn segment_id(&self) -> [u8; 16];
    fn max_doc(&self) -> u32;
    fn field_infos(&self) -> Vec<FieldInfo>;                        // any order
    fn index_sort(&self) -> Option<IndexSort> { None }              // writer sorts the docs unless is_sorted()
    fn is_sorted(&self) -> bool { false }
    fn attributes(&self) -> BTreeMap<String, String> { .. }
    fn terms(&self, field: u32) -> Result<Option<Box<dyn TermsSource + '_>>>;      // strictly increasing bytes
    fn norms(&self, field: u32) -> Result<Option<NormsIter<'_>>>;                // (doc, SmallFloat byte), any doc order
    fn doc_values(&self, field: u32) -> Result<Option<DocValuesSource<'_>>>;
    fn points(&self, field: u32) -> Result<Option<Box<dyn BytesSource + '_>>>;   // (doc, packed value), any order
    fn vectors(&self, field: u32) -> Result<Option<Box<dyn BytesSource + '_>>>;  // (doc, raw LE f32 / i8 / bits), any doc order
    fn stored(&self, doc: u32, out: &mut Vec<u8>) -> Result<()>;                 // appends doc's StoredDocWriter bytes
    fn stored_random_access_is_cheap(&self) -> bool { false }   // the indexing buffer returns true (no cache copy on index sort)
    fn vector_graph_seed(&self, field: u32) -> Option<(VectorsReader, Vec<u32> /* its ord -> doc here */)> { None }
        // merges only: an existing HNSW graph to start from (MergeSource/RemappedSource implement it)
    fn suggest(&self, field: u32) -> Result<Option<Box<dyn SuggestSource + '_>>> { Ok(None) }  // fields with `suggest`; any order
}
pub trait SuggestSource { fn next(&mut self) -> Option<SuggestEntry<'_>>; }
pub struct SuggestEntry<'a> { pub key: &'a [u8] /* analyzed input incl. context prefix/separators: the caller's encoding */,
                              pub weight: u64 /* <= i64::MAX */, pub doc: u32, pub payload: &'a [u8] /* opaque, e.g. surface form */ }
pub trait TermsSource { fn next_term(&mut self) -> Result<Option<&[u8]>>; fn postings(&mut self) -> Result<&mut dyn PostingsSource>; }
pub trait PostingsSource { fn next_doc(&mut self) -> u32 /* NO_MORE_DOCS */; fn freq(&mut self) -> u32; fn next_position(&mut self) -> u32;
                           fn offsets(&self) -> (u32, u32) { (0,0) }; fn payload(&self) -> &[u8] { &[] } }
pub type NormsIter<'a> = Box<dyn Iterator<Item = (u32, u8)> + 'a>;
pub enum DocValuesSource<'a> { Numeric(Box<dyn Iterator<Item=(u32,i64)>+'a>), Binary(Box<dyn BytesSource+'a>),
    Sorted(Box<dyn SortedSource+'a>), SortedSet(Box<dyn SortedSetSource+'a>), SortedNumeric(Box<dyn SortedNumericSource+'a>) }
pub trait BytesSource { fn next(&mut self) -> Option<(u32, &[u8])>; }
pub trait SortedSource { fn value_count(&self) -> u64; fn value(&mut self, ord: u64) -> &[u8] /* sorted unique */; fn next_doc(&mut self) -> Option<(u32, u64)>; }
pub trait SortedSetSource { fn value_count(&self) -> u64; fn value(&mut self, ord: u64) -> &[u8]; fn next_doc(&mut self) -> Option<(u32, &[u64])> /* any order, deduped by the writer */; }
pub trait SortedNumericSource { fn next_doc(&mut self) -> Option<(u32, &[i64])> /* any order */; }
pub fn write_segment(src: &dyn SegmentSource, out: &mut dyn Write) -> Result<SegmentInfo>;
pub fn write_segment_with(src, out, &WriteOptions { stored: StoredOptions { level: 3, chunk_bytes: 16K, chunk_docs: 128, dictionary: true },
                                                    vector_threads: min(cores, 8),
                                                    spill: SpillOptions { dir: None /* std::env::temp_dir() */, memory_bytes: 64 MiB } })
    -> Result<SegmentInfo>;
// Memory while writing: the doc stream and stored chunks stream into `out`; positions, payloads and term
// dictionaries keep at most `spill.memory_bytes` each in memory and continue in unlinked temp files in
// `spill.dir` (pass the shard's cache directory: /tmp is often RAM). SpillOptions::in_memory() never spills.
// Doc values, points and suggest entries collect their per-value arrays (docs, values, ords, addresses, sort
// permutations, BKD keys) in spill-backed arrays (heap up to the budget, then a temp file mapped back: page
// cache, not heap) and their encoders stream append-only into the container; sorted/sorted-set dictionaries are
// read from the source twice (SortedSource::value is called for ords 0..count twice, in order) instead of
// copied. Raw vectors past the budget go to a temp file mapped back for the graph build. Heap that remains:
// one norm byte per doc of the field being written, quantized vector codes and the HNSW graph of one field.
// Refresh-time segments may want `dictionary: false` (zstd dictionary training costs ~ms per MB sampled).
// HNSW construction dominates writing vector fields (~55 µs/vector single-threaded for 384-d int8, m 16,
// ef_construction 100); vector_threads builds the graph concurrently (deterministic only with 1).
pub struct StoredDocWriter; // new(), add(field, StoredValue::{Str,Bytes,Int,Long,Float,Double}), bytes(), clear(), into_bytes()

// Doc maps, sorting, merging, reordering (source.rs, sort.rs, merge.rs, bp.rs)
pub struct DocMap;  // from_new_to_old(Vec<u32>, old_max_doc)?, compact(old_max_doc, live: Fn(u32)->bool),
                    // old_to_new(old) -> Option<u32>, new_to_old(new) -> u32, new_max_doc(), is_identity(), is_monotonic()
pub struct RemappedSource<'a>; // RemappedSource::new(&dyn SegmentSource, DocMap, sort: Option<IndexSort>)? — a source in the new order
                               // (postings buffered per term; norms, doc values, points, vectors, suggest stream renumbered)
                               // .set_spill(SpillOptions): a scattered order (BP) reads stored docs once in old order into a
                               // spill-backed copy instead of decompressing a chunk per doc
// Per-doc values (norms, doc values, points, vectors) may come in any doc order, each doc at most once per field
// (points: any number); doc order is cheapest (no sort). The writer sorts in place (vectors: in-place permutation).
pub fn sort_doc_map(src: &dyn SegmentSource, sort: &IndexSort) -> Result<DocMap>;   // stable, ES missing/selector semantics
pub struct MergeInput { pub reader: SegmentReader, pub live: Option<Arc<LiveDocs>> }
pub struct MergeSource; // MergeSource::new(Vec<MergeInput>, new_id)? — concatenation minus deletions; implements SegmentSource;
                        // map_doc(input, old) -> Option<u32>, source_of(new) -> (input, old), set_index_sort(..), set_attributes(..),
                        // set_spill(SpillOptions) (ordinal maps of merged dictionaries are spill-backed)
                        // Doc maps are rank structures over the live docs (~0.5 bit/doc); points stream leaf by leaf; merged
                        // sorted/sorted-set dictionaries are k-way merged lazily (never copied).
pub fn bp_reorder(src: &dyn SegmentSource, &BpOptions) -> Result<Option<DocMap>>;
pub struct BpOptions { min_doc_freq: 4096, max_doc_freq_ratio: 1.0, min_partition_size: 32, max_iters: 20, fields: Option<Vec<u32>>,
                       threads: min(cores, 8), ram_budget_bytes: 512 MiB /* bigger forward index: Ok(None) */,
                       parents: Option<Vec<u64>> /* nested docs: parent bitset; child blocks move as a whole — REQUIRED with nested */ }
// Merge recipe: let m = MergeSource::new(inputs, id)?;  index-sorted inputs: write_segment(&m, out) sorts by itself;
// otherwise: match bp_reorder(&m, &BpOptions { parents, ..Default::default() })? {
//   Some(map) => write_segment(&RemappedSource::new(&m, map, None)?, out), None => write_segment(&m, out) }.
// Vector graphs of merges start from the largest input graph without deletions (Lucene's incremental merge).

// Reading (reader.rs). Open parses headers only and checks the Meta CRC; verify() checks every byte.
pub struct SegmentReader;            // Send + Sync, Arc inside
impl SegmentReader {
    pub fn open(data: Slice) -> Result<SegmentReader>;
    pub fn verify(&self) -> Result<()>;                               // run once after download/write
    pub fn info(&self) -> &SegmentInfo; pub fn id(&self) -> [u8;16]; pub fn max_doc(&self) -> u32;
    pub fn data(&self) -> &Slice;                                     // the file bytes (zero-copy)
    pub fn size_in_bytes(&self) -> u64; pub fn sections(&self) -> Vec<(SectionKind, u32 /*field*/, u64 /*bytes*/)>;
    pub fn heap_bytes(&self) -> usize;                                // approx. heap beyond the file bytes (memory accounting)
    pub fn field_info(&self, field: u32) -> Option<&FieldInfo>;
    pub fn terms(&self, field: u32) -> Option<&TermsReader>;
    pub fn postings(&self, field: u32, term: &TermState, flags: PostingsFlags) -> PostingsCursor;  // empty cursor if no terms
    pub fn norms(&self, field: u32) -> Option<&NormsReader>;
    pub fn doc_values(&self, field: u32) -> Option<&DocValuesReader>;  // enum of the five below
    pub fn numeric(&self, f) -> Option<&NumericDocValues>; pub fn binary(&self, f) -> Option<&BinaryDocValues>;
    pub fn sorted(&self, f) -> Option<&SortedDocValues>; pub fn sorted_set(&self, f) -> Option<&SortedSetDocValues>;
    pub fn sorted_numeric(&self, f) -> Option<&SortedNumericDocValues>;
    pub fn points(&self, field: u32) -> Option<&PointsReader>;
    pub fn vectors(&self, field: u32) -> Option<&VectorsReader>;
    pub fn suggest(&self, field: u32) -> Option<&SuggestReader>;
    pub fn stored(&self) -> StoredReader;                             // one per thread (zstd context + last-chunk cache)
}

// Terms (terms.rs): blocks of 32 front-coded terms + their metadata; index = sorted 8-byte key prefixes of the
// blocks' separator keys (floor search ~60 ns). seek_exact ~350 ns (Bloom-filtered misses ~35 ns), iteration
// ~12 ns/term (+10 with state), automaton intersection seeks past dead prefixes (Automaton::next_live_byte).
impl TermsReader { fn stats(&self) -> &TermsStats { num_terms, sum_doc_freq, sum_total_term_freq, doc_count };
    fn min_term(&self) -> &[u8]; fn max_term(&self) -> &[u8]; fn has_freqs/has_positions/has_offsets/has_payloads(&self) -> bool;
    fn seek_exact(&self, term: &[u8]) -> Option<TermState>; fn term_state(&self, ord: u64) -> TermState;
    fn seek_exact_hashed(&self, term, bloom::hash64(term)) -> Option<TermState> /* hash once per lookup across segments */;
    fn bloom(&self) -> Option<&BloomFilter { may_contain(&[u8]), may_contain_hash(u64), len(), size_in_bytes() }>;
    fn postings(&self, &TermState, PostingsFlags) -> PostingsCursor;
    fn postings_reset(&self, &TermState, PostingsFlags, &mut PostingsCursor) /* reuses the cursor's buffers: no allocation per term */;
    fn iter(&self) -> TermsIter<'_>; fn seek_ceil(&self, term) -> TermsIter<'_>; fn range(&self, Bound<&[u8]>, Bound<&[u8]>) -> TermsIter<'_>;
    fn intersect<'a>(&'a self, aut: &'a dyn Automaton) -> TermsIter<'a>; fn intersect_range(&self, aut, lower, upper) -> TermsIter<'_>; }
impl TermsIter<'_> { fn next(&mut self) -> Option<&[u8]>; fn term(&self) -> &[u8]; fn ord(&self) -> u64;
    fn state(&mut self) -> TermState; fn doc_freq(&mut self) -> u32; fn total_term_freq(&mut self) -> u64; fn postings(&mut self, flags) -> PostingsCursor;
    fn postings_reset(&mut self, flags, &mut PostingsCursor); }
pub struct TermState { pub ord: u64, pub doc_freq: u32, pub total_term_freq: u64, /* file pointers… */ }  // Copy; cache it per query

// Postings (postings.rs). Lucene PostingsEnum/ImpactsEnum protocol. Dense blocks are bit sets (Lucene 10.1 rule; 128
// consecutive docs cost 0 bytes): advance within them ~8 ns, into_bitset 2–4 G docs/s; next() ~400–800 M docs/s.
PostingsFlags::{DOCS, FREQS, POSITIONS, OFFSETS, PAYLOADS, ALL} (BitOr)
impl PostingsCursor { fn doc(&self) -> u32 /* u32::MAX before first */; fn next(&mut self) -> u32; fn advance(&mut self, target) -> u32;
    fn cost(&self) -> u64; fn freq(&mut self) -> u32 /* lazy block decode */; fn next_position(&mut self) -> u32;
    fn start_offset(&self) -> u32; fn end_offset(&self) -> u32; fn payload(&self) -> &[u8];
    fn advance_shallow(&mut self, target: u32);   // moves only the impacts view; next() still returns the next doc
    fn impact_levels(&mut self) -> usize /* 1 or 2 */; fn impact_doc_upto(&mut self, level) -> u32 /* inclusive; NO_MORE_DOCS for the tail */;
    fn impacts(&mut self, level) -> &[Impact { freq: u32, norm: u8 /* unsigned SmallFloat byte; 1 if no norms */ }];  // Pareto frontier
    // (a single-doc term reports (its freq, norm 0): still a valid upper bound)
    fn max_freq_bound(&self) -> u32; fn empty() -> PostingsCursor; fn reset(&mut self, &PostingsData, &TermState, flags) /* via TermsReader::postings_reset */;
    fn into_bitset(&mut self, upto: u32, bits: &mut [u64], offset: u32) /* Lucene intoBitSet; cursor must be positioned */;
    fn doc_id_run_end(&self) -> u32 /* Lucene docIDRunEnd: docs doc()..end all match (within the current block) */;
    fn dense_block(&self) -> Option<(u32 /* start */, &[u64] /* bit i = doc start+i */, u32 /* last */)>
        /* the current block's bit set when stored dense and reached by advance (next() decodes it); conjunctions AND these */ }
// Corrupt (unverified) data never panics or hangs anywhere in the codec (fuzz-tested over every section): a postings block
// that does not decode to its header's last doc ends the cursor; term iterators never seek backwards.

// Norms: int_to_byte4(len) / byte4_to_int(b) (Lucene SmallFloat); NormsReader::get(doc) -> Option<u8>, get_or(doc, 1),
//        dense_bytes() -> Option<&[u8]> (bytes[doc] when every doc has a norm), docs() -> &DocSet.
// Doc values: values are addressed by the doc's rank in docs() (a DocSet); iterate with docs().cursor() / cursor.index().
//   NumericDocValues::{get(doc)->Option<i64>, value_at(idx), docs(), cursor(), skipper(),
//       get_batch(docs /* ascending */, found: &mut Vec<u32>, values: &mut Vec<i64>) /* appends; one dispatch per batch */,
//       values_range(first_idx, out: &mut [i64]) /* sequential decode, ~1 ns/value: scans over every doc */}
//   BinaryDocValues::{get(doc)->Option<&[u8]>, value_at(idx), docs()}
//   SortedDocValues::{ord(doc)->Option<u64>, ord_at(idx), value_count(), lookup_ord(ord, &mut Vec<u8>), lookup_term(&[u8])->Result<u64,u64>, dict()->&TermDict, skipper(),
//       ords_batch(docs, found, ords: &mut Vec<u64>), ords_range(first_idx, out: &mut [i64])}
//   SortedSetDocValues::{ords(doc)->OrdsIter (sorted, unique), ords_at(idx), is_single_valued(), value_count(), lookup_ord, lookup_term, dict(), skipper(),
//       ords_batch(docs, found, counts: &mut Vec<u32>, ords: &mut Vec<u64>) /* per found doc: count, then its ords flattened */}
//   SortedNumericDocValues::{values(doc)->ValuesIter (ascending), values_at(idx), is_single_valued(), skipper(),
//       get_batch(docs, found, counts: &mut Vec<u32>, values: &mut Vec<i64>)}
//   TermDict::{len(), term(ord, &mut Vec<u8>) /* ~100 ns random */, seek(&[u8]) -> Result<ord, insertion>, lower_bound(&[u8]) -> ord,
//              iter_from(ord) -> TermDictIter{next(), ord(), term()},
//              intersect(&dyn Automaton) -> TermDictIntersect{next() -> Option<ord>, term()}}  // include/exclude regex, keyword wildcard on dv
//   DocValuesSkipper (Lucene's): advance(target), num_levels(), min_doc_id/max_doc_id/min_value/max_value/doc_count(level), global_*()
// DocSet: rank(doc)->Option<u32> O(1), contains, next_from(doc), select(idx), len(), is_all(), cursor() -> DocSetCursor{next, advance, advance_exact, doc, index}
// Points (build ~60–80 ns/point 1-d, ~130 2-d geo; crossing leaves are re-classified in runs of 32 sorted values, so
//   narrow ranges check a few dozen values per boundary leaf; low-cardinality leaves store runs of equal values once and
//   hand each run to visit_value_docs): PointsReader::{intersect(&mut dyn PointVisitor), estimate_point_count(&mut v), estimate_doc_count(&mut v), size(), doc_count(),
//         min_packed_value(), max_packed_value(), visit_all_values(&mut FnMut(doc, &[u8])),
//         values_iter() -> PointValuesIter { next() -> Option<(doc, &[u8])>, peek_doc(), skip() } /* pull, leaf by leaf */}
//   trait PointVisitor { fn compare(&mut self, min: &[u8], max: &[u8]) -> Relation /* index dims only */; fn visit(&mut self, doc);
//                        fn visit_value(&mut self, doc, packed /* all data dims */); fn visit_docs(&mut self, &[u32]);
//                        fn visit_value_docs(&mut self, docs: &[u32], packed) /* run of equal values; default loops visit_value */;
//                        fn visit_doc_range(&mut self, start, end) /* docs start..end all inside (leaves of consecutive docs:
//                            time series in time order, index-sorted fields); default loops visit — override to set bit ranges */;
//                        fn grow(&mut self, n) }
//   points::encode::{i32_bytes/i32_from, i64_bytes/i64_from, f32_bytes, f64_bytes, f32_sortable/sortable_f32,
//       f16_bits(f32)/f16_to_f32 (Lucene HalfFloatPoint rounding), f16_bytes, f16_bits_bytes, f16_sortable/sortable_f16 (half_float dv),
//       f64_sortable/sortable_f64, lat_lon_bytes, encode_latitude(_ceil), encode_longitude(_ceil), decode_latitude/longitude}
//       (Lucene NumericUtils / GeoEncodingUtils, bit-exact: checked against ES docvalue_fields output)
// Stored: StoredReader::{visit(doc, &mut dyn StoredVisitor) -> Result<()>, document(doc) -> Result<Vec<(u32, OwnedStoredValue)>>,
//         raw(doc) -> Result<&[u8]>}  (doc >= max_doc: InvalidArgument; stored::decode_doc(bytes, visitor) decodes raw bytes)
//   trait StoredVisitor { fn needs_field(&mut self, field) -> VisitStatus /* Yes|No|Stop */; fn value(&mut self, field, StoredValue<'_>) }
// Vectors: VectorsReader::{len(), docs(), ord(doc), doc(ord), vector(ord) -> &[u8], vector_f32(ord, &mut Vec<f32>),
//     prepare(VectorQuery::{Float(&[f32]) | Byte(&[i8]) | Bits(&[u8])}) -> Result<PreparedQuery> /* dims checked */,
//     score(&q, ord) -> f32 /* Lucene VectorSimilarityFunction score; bit = (bits - hamming) / bits */, score_doc(&q, doc),
//     search(&q, k, num_candidates, accept: Option<&dyn Fn(doc)->bool>, visit_limit) -> KnnResult { hits: Vec<KnnHit{doc,score}>, visited, complete },
//     exact_search(&q, k, accept) -> KnnResult,  // complete == false: visit_limit hit, fall back to exact_search (Lucene's strategy)
//     search_filtered(&q, k, num_candidates, accept: &dyn Fn(doc)->bool, accepted: u32 /* filter cardinality */, visit_limit) -> KnnResult}
//     // filters passing < 60% of the vectors use Lucene's filtered HNSW search (ACORN-1: only accepted vectors are scored,
//     // rejected neighbors are bridges; 1% filter: ~50x fewer distance computations than search() with accept); else search().
//   Graphs: concurrent builds link each node bottom-up (no quality loss vs 1 thread); every node is reachable from the entry
//   point (Lucene's connectComponents), also with duplicate vectors.
//   Quantized fields (int8 / packed int4 / bbq) steer the graph search with quantized scores and return exact float scores (the
//   beam is rescored), so `_score` equals ES's `flat`/rescored value, not its approximate int8/int4/bbq score.
//   Quantized flat fields (int8_flat, bbq_flat): search() scans the codes for max(k, num_candidates), then rescores.
//   vectors::similarity_score(&VectorSpec, a, b); vectors::distance::{dot_f32, l2sq_f32, dot_i8, l2sq_i8, dot_u8, l2sq_u8,
//   dot_u4, l2sq_u4, hamming, prefetch} (AVX2/FMA at runtime + portable, tested equal)
// Suggest (ES `completion`; suggest.rs): entries sorted by (key, weight desc, doc, payload), a max-weight tree over blocks of 16.
//   SuggestReader::{len(), key(idx, &mut Vec<u8>), weight(idx), doc(idx), payload(idx) -> &[u8], max_weight(),
//     ranges(&dyn Automaton) -> Vec<SuggestRange { start, end, prefix_len }> /* keys with a prefix the automaton accepts; each key
//         matched by its SHORTEST accepted prefix (Lucene FSTUtil.intersectPrefixPaths); prefix_len = that length (fuzzy boost) */,
//     complete(&dyn Automaton) -> Completions, complete_ranges(Vec<SuggestRange>) -> Completions}
//   Completions::{next() -> Option<Completion { index, weight, doc, prefix_len }> /* exact descending weight, ties in key order,
//     lazy: ~0.5 µs per result */, max_remaining_weight() -> Option<u64> /* bound for early exit with boosts */, ranges()}
//   Deleted docs, skip_duplicates, contexts and boosts are the caller's (pull until N accepted). Top-10 over 1M entries: 4–6 µs.
// Live docs: LiveDocs::{all_live(max_doc), is_live(doc), delete(doc) -> bool, num_deleted(), num_live(), next_live(from), words(),
//     encode(segment_id, generation) -> Vec<u8>, decode(&[u8], segment_id) -> Result<(LiveDocs, generation)>}

// Automata (automaton/): object-safe, u32 states; DEAD = u32::MAX.
pub trait Automaton: Send + Sync { fn start(&self) -> u32; fn is_match(&self, s: u32) -> bool; fn can_match(&self, s) -> bool;
    fn will_always_match(&self, s) -> bool; fn accept(&self, s: u32, byte: u8) -> u32; fn matches(&self, &[u8]) -> bool;
    fn next_live_byte(&self, s, from: u8) -> Option<u8> /* default: scan with accept; lets dictionaries seek past dead prefixes */ }
AlwaysMatch; Prefix::new(bytes); Dfa::levenshtein(term, max_edits, prefix_len /* code points */, transpositions) (Lucene FuzzyQuery automaton);
Dfa::wildcard(pattern, case_insensitive, work_limit) -> Result<Dfa, TooComplex>; Dfa::prefix_case_insensitive(prefix, work_limit);
Dfa::term_case_insensitive(term, work_limit) /* `term` query with case_insensitive */;
    // wildcard/prefix/term case_insensitive fold ASCII letters only, and `\x` escapes match exactly (ES AutomatonQueries)
automaton::regexp::compile(pattern, flags /* regexp::ALL = ES default */, case_insensitive, work_limit /* dfa::DEFAULT_WORK_LIMIT = 10000 */)
    -> Result<Dfa, RegexpError::{Syntax(lucene IllegalArgumentException text), TooComplex(TooComplex /* Display = Lucene's message */)}>;
    // regexp case_insensitive = Lucene 10.5 CaseFolding (simple mappings + specials: k~K, ß~ẞ, σ~ς~Σ; ranges not folded).
    // ES runs regexp::collapse_consecutive_quantifiers(pattern) (`a+*` -> `a*`) before compiling regexp queries on
    // string fields: call it first so error positions match. Lucene quirk kept: `X*` of an empty language (`#*`)
    // matches nothing. TooComplex thresholds track Lucene's effort accounting (checked around the limit).
Dfa::{intersection(a, b), complement(), single_string() -> Option<Vec<u8>>, is_empty_language()}; automaton::edit_distance(a, b, transpositions)
// Builds determinize per code point first, then compile to bytes directly: `elasticsearch`~2 ≈ 0.3 ms, `kitten`~1 ≈ 20 µs,
// `*abc*` ≈ 8 µs, `[a-z]+ing` ≈ 4 µs.
```

Lucene's norm encoding (`SmallFloat.intToByte4`) is used so BM25 scores match Elasticsearch. Impacts
carry the unsigned norm byte; docs without norms are scored with norm 1 (Lucene). Fuzzy, regexp
(match sets and error texts), vector scores, geo-point quantization, half-float rounding and
BM25 field lengths (decoded norms) are checked against fixtures captured from Elasticsearch
9.5.5 (`crates/search-codec/tests/fixtures`), plus a randomized fixture of ~900 fuzzy, regexp
(also case-insensitive, stacked quantifiers, near the work limit), wildcard and prefix queries
(`gen_automata_random.py` regenerates it against a running Elasticsearch).

## `search-index`

One shard's engine. Knows nothing about JSON or mappings: `search-es` turns documents into
`IndexableDoc`s and the Query DSL into `Query`s.

```rust
// Fields are numbered (`schema::FIRST_USER_FIELD` = 8; 0..8 are _id, _seq_no, _version, _primary_term).
pub struct Schema { pub index_sort: Option<IndexSort>, pub similarity: Similarity, … }
impl Schema { fn new(); fn with_field(FieldSchema) -> Result<Schema>; fn add_field(&mut self, FieldSchema) -> Result<()>;
              fn by_name(&str) -> Option<&FieldSchema>; fn name(u32) -> String; fn with_index_sort(..); }
impl FieldSchema { fn new(number, name); fn indexed(IndexOptions, norms: bool); with_doc_values(DocValuesType); with_skip_index();
                   with_points(PointsSpec); with_vectors(VectorSpec); with_term_vectors(TermVectorOptions);
                   with_similarity(Similarity); with_payloads(); with_bloom_filter(); with_suggest() }
pub struct IndexableDoc { pub id: Bytes, pub fields: Vec<IndexableField>, pub stored: Vec<StoredField>, pub nested: Vec<IndexableDoc> }
// builders: new(id).text(field, &Analyzer, value).term(..).point(..).doc_value(..).stored(..).field(..).child(doc)
pub enum IndexableField { Text { field, analyzer, values }, Tokens { field, tokens: Vec<PreToken> }, Term { field, term: Bytes },
                          Point { field, packed: Vec<u8> }, DocValue { field, value: DocValue }, Vector { field, value: Bytes /* LE */ },
                          Suggest { field, key, weight, payload } }
pub struct Shard;
impl Shard {
    pub fn open(cfg: ShardConfig, schema: Schema, segments: Vec<SegmentHandle>) -> Result<Shard>;  pub fn in_memory(schema) -> Shard;
    pub fn index(&self, op: IndexOp) -> Result<OpResult>;   // IndexOp::new(doc) + version/version_type/if_seq_no/if_primary_term/op_type/origin
    pub fn delete(&self, op: DeleteOp) -> Result<OpResult>; // OpResult::{Indexed, Deleted, Conflict(VersionConflict), Stale}
    pub fn get(&self, id: &[u8], realtime: bool) -> Result<Option<StoredDoc>>;
    pub fn refresh(&self) -> Result<bool>;  pub fn flush(&self) -> Result<FlushOutcome>;  // segments (zero-copy bytes) + live docs + checkpoint
    pub fn maybe_merge(&self) -> Result<usize>;  pub fn force_merge(&self, max_num_segments: Option<usize>, only_expunge_deletes: bool) -> Result<()>;
    pub fn searcher(&self) -> Searcher;      // point in time; stats(), segments(), update_schema(), mark_seq_no_processed(), close()
}
pub trait Query: Send + Sync + Debug + Any { fn weight(&self, s: &Searcher, mode: ScoreMode, boost: f32) -> Result<Box<dyn Weight>>;
    fn rewrite(&self, s: &Searcher) -> Result<Option<QueryRef>>; fn lucene_string(&self, &Schema) -> String; fn visit(..); fn as_any(&self) -> &dyn Any; }
pub trait Weight: Send + Sync { fn scorer(&self, seg) -> Result<Option<Box<dyn Scorer>>>; fn bulk_scorer(..); fn explain(&self, seg, doc) -> Result<Explanation>;
                                fn count(&self, seg) -> Result<Option<u32>>; }
pub trait Scorer: Scorable { fn doc(&self) -> u32; fn next(&mut self) -> u32; fn advance(&mut self, t: u32) -> u32; fn cost(&self) -> u64;
    fn has_two_phase(&self) -> bool; fn approx_next/approx_advance; fn matches(&mut self) -> bool; fn advance_shallow; fn max_score(&mut self, upto) -> f32;
    fn next_docs_and_scores(upto, live, &mut DocScoreBuf); fn apply_required(docs, scores: &mut [f64], len) -> usize /* batch intersect + add score */;
    fn apply_filter(docs, scores, len) -> usize /* batch intersect, no score (default: per doc) */; }
// Query also has `fn cache_identity(&self, &mut dyn Hasher) -> bool` (default false): an exact structural identity of the
// doc set for the filter cache; implemented by term/phrase/multi-term (not automata)/point/doc-values/geo/exists/match_all/none
// queries and by bool/dis_max/constant_score/boost over cacheable clauses.
pub trait Collector { fn score_mode(&self) -> ScoreMode; fn leaf<'a>(&'a mut self, seg: &SegmentContext) -> Result<Box<dyn LeafCollector + 'a>>; }
impl Searcher { fn search(&self, &QueryRef, &mut dyn Collector) -> Result<()>; fn search_with(.., &SearchControl /* deadline, cancel */);
    fn search_concurrent<M: CollectorManager>(&self, &QueryRef, &M, &SearchControl) -> Result<M::Out>;
    fn count(&QueryRef) -> Result<u64> /* Weight::count per segment, the rest iterated in concurrent slices */; fn count_with(&QueryRef, &SearchControl);
    fn count_if_cheap(&QueryRef) -> Result<Option<u64>>; fn explain(&QueryRef, global_doc) -> Result<Explanation>; fn rewrite(&QueryRef);
    fn stored(global_doc); fn term_vectors(global_doc, fields, term_stats, field_stats) -> Result<Vec<FieldTermVector>>;
    fn collection_stats(field); fn term_stats(field, term); fn with_limits(SearchLimits /* max_clause_count 4096, parallelism */);
    fn point_bounds(field) -> Option<PointBounds { min, max /* per-dim packed */, doc_count }> /* isFieldWithinQuery */; }
```

Cold segments (`shard::gate`): `SegmentHandle::fetching(data /* SegmentFetch::map_partial */, Arc<dyn SegmentGate
{ is_ready(range), prioritize(ranges), wait_ready(ranges) -> io::Result<()> /* blocking */, is_complete() }>)` opens
a shard on segments still downloading. `Shard::open` waits only for header, footer, TOC (`search_codec::container::
toc_range`), every section a reader parses and the `_id` postings; other fields' doc streams, the stored-field chunks
(`search_codec::stored::open_range`) and positions/payloads stay lazy and are prioritized in that order. Readers wait
per field on first use: code outside search-index that reads postings or stored documents of a `SegmentContext`
calls `seg.ensure_postings(field)?` (docs/freqs cursors), `seg.ensure_positions(field)?` (`PostingsFlags::POSITIONS`
or more) or `seg.ensure_stored()?` first (no-ops for complete segments; search-es' `SourceValues` over `_source`
needs `ensure_stored`). Writes and realtime gets never wait on postings; background
merges skip incomplete segments, `force_merge` waits; flushes report fetching segments as not new and never copy them.
A failed download fails the open or the request. `SegmentHandle::new(data)`, `from_file(path)`, `with_live_bytes(..)`.
Concurrent search splits work like Lucene 10 (`Searcher::slices() -> Vec<Vec<search::Partition { seg, min, max }>>`):
up to `SearchLimits::parallelism` slices of at least `parallel_min_docs` docs; a segment of two slices' worth or
more is split into doc id ranges (intra-segment concurrency), so a force-merged shard still uses every thread. A
collector's `leaf(seg)` is then called once per partition, each partition in a different slice; collectors must not
assume a leaf covers its whole segment. The calling thread runs the first slice, the others run on `search::pool`
(process-wide workers, at most one per core, started on demand and exiting after 2 s idle; nested concurrent
searches inside a worker run inline), so collectors are owned: `CollectorManager::C: Collector + Send + 'static`. `search::search_partitions(parts, w,
collector, ctl)` runs explicit partitions.
`ShardStats` also has `merges_current_docs/bytes`, `merges_total_docs/bytes/time`, `refresh_calls` (every
`refresh()`, as Elasticsearch's `refresh.total`) + `refresh_time` (`refreshes_total` = published), `flush_time`,
`version_map_bytes` (Elasticsearch's `version_map_memory_in_bytes`, tombstones included; also `Shard::version_map_bytes()`).
Write failures: an `index` that fails after it got a seq no (a Lucene-level document failure: immense analyzed term,
position or doc-value limits) returns `Err(Error::Rejected { seq_no, primary_term, source })` (`e.seq_no() ->
Option<(i64, i64)>`; type, status and reason are `source`'s): the caller appends a no-op with that seq no to its WAL
(replay: `mark_seq_no_processed`), as Elasticsearch writes a translog `NoOp`. Failures before a seq no (version
conflicts, an immense `Term`, the doc limit) use none. `ShardConfig::max_docs` (default `shard::MAX_DOCS` =
2147483519, Lucene's `IndexWriter.MAX_DOCS`) bounds docs held (`Shard::pending_docs()`: segments' `max_doc` + buffered,
deleted docs until merged away); past it index and delete fail with Elasticsearch's "Number of documents in the shard
cannot exceed [..]". A flush commit is a consistent cut (segments and live docs captured with no write in flight):
it never holds the delete of an update without its new doc. `force_merge(Some(n), _)` merges only the segments present
at the call and their merge results (Lucene's `segmentsToMerge`), so it returns under steady indexing.
`search::pool::execute_held(job, hold)`: `hold` drops once the worker that ran `job` is parked again.

Queries (`query::*`): `TermQuery`, `MultiTermQuery` (prefix, wildcard, regexp, fuzzy, term range,
term set; every Elasticsearch rewrite method via `RewriteMethod::parse`), `BooleanQuery`
(must/should/filter/must_not/`with_msm`; Lucene's rewrites, MaxScore / WAND / block-max
conjunction), `PhraseQuery`, `MultiPhraseQuery` (slop), `SpanQuery` (`SpanNode`: term, near,
or, not, first, containing, within, multi, gap), `IntervalQuery` (`IntervalsSource` factories
with Lucene's rewrites; `IntervalsSource::analyzed(tokens, max_gaps, ordered) -> Result<IntervalsSource>` is the
`match` rule, token graphs included — synonym_graph paths split at articulation points; `analyzed_with_limit` takes
`max_clause_count`), `PointRangeQuery`,
`PointInSetQuery`, `DocValuesRangeQuery`, `DocValuesTermsQuery`, `IndexOrDocValuesQuery`,
`IndexSortRangeQuery { field, lower, upper, fallback }` (Lucene's `IndexSortSortedNumericDocValuesRangeQuery`: what
Elasticsearch builds for long/int/date ranges on the index's primary sort field, around the usual `IndexOrDocValuesQuery`;
sorted segments resolve to one doc id range), point/doc-values ranges rewrite as Lucene 10.5 does (no overlap → match none,
covering → `*:*` or `FieldExistsQuery`; `Searcher::point_bounds`),
`FieldExistsQuery`, `MatchAllQuery`, `MatchNoneQuery`, `ConstantScoreQuery`, `BoostQuery`,
`DisMaxQuery`, `SynonymQuery`, `FunctionScoreQuery` / `ScriptScoreQuery` (scripts through
`ScoreScript`), `KnnQuery` (approximate / exact, filter, min score; the `knn` query, rewrites to
`ScoreDocQuery::lucene` = Lucene's `DocAndScoreQuery`, "within top N docs"; the top-level `knn` section's merged
hits are `ScoreDocQuery::new(hits)` = Elasticsearch's `KnnScoreDocQuery`, "within top k documents"), `ToParentQuery` / `ToChildQuery` (`nested::root_filter()` marks parents),
`GeoDistanceQuery`, `GeoPointShapeQuery` (Lucene `LatLonPointQuery` rules: `GeoShape::{Point, Rectangle, Polygon,
Circle, Line}`, BKD-pruned; contains = every query point is one of the doc's), `geo::bounding_box`,
`GeoPointDvQuery::{geometry(field, relation, shapes), bounding_box(field, ..)} -> Result<QueryRef>` (Lucene's
`LatLonDocValuesField.newSlowGeometryQuery` / `newSlowBoxQuery`: the `dvQuery` of geo `IndexOrDocValuesQuery`s and the
query of `index: false` geo points; Lucene's strings, e.g. `g:INTERSECTS:geometries([Circle(..)])`, `g:box(minLat=..)`),
`shape::GeoShapeQuery { field, geometries: Vec<shape::QueryGeometry::{Point, Line, Polygon(GeoPolygon), Rectangle, Circle
/* meters; geo_distance on geo_shape */}>, relation }` on `geo_shape` fields (Lucene `LatLonShape` queries; a single rectangle uses the encoded-rectangle path; contains on
several geometries is a conjunction), `mlt::MoreLikeThisQuery` (like
texts as analyzed terms, like docs as term vectors; `mlt::calculate_min_should_match` is
Elasticsearch's `minimum_should_match` arithmetic), `CoveringQuery` (`terms_set`),
`FeatureQuery::{linear, log, saturation(pivot: Option), sigmoid}(field, feature, weight, ..) -> Result<QueryRef>`
(`rank_feature` / `rank_features` / `sparse_vector`; index features with `feature::feature_token(field_name, name, value)`
as `IndexableField::Tokens` on a `DocsAndFreqs` field without norms — `rank_feature` uses field `_feature`, feature =
field path, `1/value` when `positive_score_impact: false`), `CombinedFieldQuery::new(&[(field, weight >= 1)], term)`
(`combined_fields`, BM25F with the index default similarity; one per analyzed term), `DistanceFeatureQuery::{long(field,
weight, origin, pivot), geo(field, weight, lat, lon, pivot_meters)}` (`distance_feature`; fields need points and sorted
numeric doc values), `MultiPhrasePrefixQuery::new(field, terms, positions, slop, max_expansions)` (`match_phrase_prefix`),
`BinaryDvConfirmedQuery { approx, field /* binary DV */, matcher: Arc<dyn Fn(&[u8]) -> bool>, description }` (the
`wildcard` field: n-gram approximation confirmed on the doc's raw binary value; caller decodes values and runs its
automaton), `BoostingQuery::new(positive, negative, negative_boost)` (`boosting`, Lucene's `FunctionScoreQuery.boostByQuery`),
`SourceConfirmedQuery::new(query, field, index_analyzer, Arc<dyn SourceValues /* leaf(seg) -> LeafSourceValues::values(doc)
-> Vec<String> */>)` (Elasticsearch's `SourceConfirmedTextQuery` for `match_only_text`: phrase / multi-phrase /
phrase-prefix queries confirmed against the doc's re-analyzed values — wrap those in `ConstantScoreQuery` as Elasticsearch
does; term queries stay plain `ConstantScore(TermQuery)`; an `IntervalQuery` inside is Elasticsearch's
`SourceIntervalsSource`: unwrapped, scored by the intervals' own function — it rewrites to the `IntervalQuery` with the field's own
subtrees wrapped in `IntervalsSource::source_confirmed(Arc<SourceSpec::new(field, name, analyzer, values)>, src)`, while
`use_field` subtrees (`fix_field`) read the index, as Elasticsearch's per-source wrapping does; span queries are rejected by Elasticsearch), `ScriptQuery { script: Arc<dyn FilterScript> }` (`script` query; `FilterScript::{leaf(seg), describe()}`,
`LeafFilterScript::matches(doc) -> Result<bool>`), `CappedScoreQuery::new(query, max)` (the `pinned` query's organic
cap), `SliceQuery::new(SliceBy::{Terms { field: ID_FIELD, sliced_id_only } /* scroll */, DocValues { field } /* slice.field
*/, DocId /* point in time */}, id, max)` (Elasticsearch's slice queries; shard-level slice routing is the caller's),
`JoinQuery { join_field /* sorted DV "<join>#<parent type>": parents hold their id, children their parent's */,
from, to, score_mode: JoinScoreMode, min, max }` (`has_child`: from = child query filtered to the child type, to = parent
type filter; `has_parent`: from = parent query filtered to the parent type, to = child type filter, score mode Max when
`score`; Lucene's global-ordinals join, resolved on rewrite). `KnnQuery.oversample: Option<f32>` is
`rescore_vector.oversample` (hits are always rescored exactly); `KnnQuery.parent_filter: Option<QueryRef>` searches
nested vectors (best child per parent, `k` parents; wrap in `ToParentQuery` with `Max`). `Searcher::global_ordinals(field) ->
Option<Arc<search::ordinals::GlobalOrdinals { value_count, global(seg_ord, ord), segment_map(seg), lookup(..) }>>`
(sorted / sorted-set doc values, cached per searcher) is there for terms aggregations too.
Non-scoring clauses go through a filter cache (`search::cache`, Lucene's usage-tracking policy: costly clauses after
2 uses, bool/dis_max after 4, others after 5; terms/exists/match_all never; keys are `Query::cache_identity`).

`geo_shape` indexing (`query::shape`): declare the field `with_points(shape::SHAPE_POINTS)` (7 dims, 4 indexed) and add
one `IndexableField::Point` per packed triangle from `shape::{point_triangles(lat, lon), line_triangles(lats, lons),
polygon_triangles(&GeoPolygon, check_self_intersections)}`. `util::tessellator::{GeoPolygon::new(lats, lons, holes)
/* Lucene Polygon validation + messages */, tessellate}` equals Lucene 10.5.1's tessellator (triangles and edge flags) and its
errors. Elasticsearch's geometry handling stays with the caller: `GeoShapeIndexer` (dateline normalization, envelopes as
quantized polygons, `check_self_intersections = true` for polygons) and query quantization (`shape::quantize_lat/lon`).
`util::geo2d::Component` is Lucene's `Component2D` family.
`shape::geo_shape_query(field, relation, Vec<QueryGeometry>, indexed, doc_values) -> Result<QueryRef>` is
Elasticsearch's `geo_shape` query on `geo_shape` fields: `IndexOrDocValuesQuery { index: GeoShapeQuery, dv:
ShapeDvQuery }`, or one side alone. Shape doc values: a `DocValuesType::Binary` field holding
`shape::doc_value(&triangles)` (a bounding-box header, then every value's packed triangles; read them back with
`shape::doc_value_triangles(bytes)` / `shape::doc_value_bbox(bytes)`; bare concatenations still query correctly); `ShapeDvQuery::{geo(field, relation,
Vec<QueryGeometry>), xy(field, relation, Vec<XyGeometry>) -> Result}` is Elasticsearch's `ShapeDocValuesQuery`
(prints `s:INTERSECTS:[..]`).

Cartesian `point` / `shape` (`query::xy`, Lucene's `XYPointField` / `XYDocValuesField` / `XYShape`; floats, never
quantized): index a `point` value as `IndexableField::Point { packed: xy::point_bytes(x, y)? }` on a 2-dim 4-byte
points field plus `DocValue::SortedNumeric(xy::dv_value(x, y)?)` (`decode_point_bytes`, `decode_dv` invert them); a
`shape` as triangles from `shape::{xy_point_triangles(x, y), xy_line_triangles(xs, ys), polygon_triangles(&GeoPolygon::xy(xs,
ys, holes)?, check_self_intersections)}` on a `SHAPE_POINTS` field (+ `shape::doc_value` for doc values), as
Elasticsearch's `CartesianShapeIndexer` (envelopes as polygons `minX,maxX,maxX,minX,minX` / `minY,minY,maxY,maxY,minY`,
unchecked). Query geometries: `XyGeometry::{Point{x,y}, Line{xs,ys}, Polygon(GeoPolygon::xy(..)), Rectangle{min_x,
max_x, min_y, max_y}, Circle{x, y, radius}}` (Lucene validation messages; `LuceneGeometriesUtils.toXYGeometry` casts to
float, multi-geometries flatten). `xy::point_shape_query(field, relation, geoms, indexed, doc_values)` is
`XYQueriesUtils.toXYPointQuery` (intersects-only `XyPointQuery` / `XyPointDvQuery` composed into
disjoint/within/contains, `InverseXYGeometry` printed with its Java hash) and `xy::shape_query(field, relation, geoms,
indexed, doc_values)` is `toXYShapeQuery` (`XyShapeQuery` + `ShapeDvQuery::xy`). Results and profile strings match
Elasticsearch (`tests/xy_shape.rs`). `ConstantScoreQuery` rewrites as Lucene's (boosts and nested constant scores
dropped, boolean `rewriteNoScoring`: musts become filters, optional shoulds go).

Completion (`suggest::*`): `Searcher::complete(&CompletionQuery { field, automaton: &dyn Automaton /* over the caller's
key encoding: prefix, fuzzy, regex, context prefixes */, boost: Option<&dyn Fn(&[u8] /* accepted key prefix */) -> f32>,
size, skip_duplicates, one_per_doc /* ES TopSuggestGroupDocsCollector */ }) -> Result<Vec<Suggestion { doc /* global */,
key, payload /* surface form */, weight, score, prefixes /* accepted prefixes of the doc's candidates: decode contexts */ }>>`,
Lucene's NRTSuggester + TopSuggestDocsCollector order (score desc, surface form, doc) and cut-offs.
`suggest::completion_score(weight, boost)` and `suggest::fuzzy_prefix_boost(path, analyzed)` are Lucene's scoring.
Term suggester (`spell::*`): `Searcher::suggest_similar(field, token, shard_size /* max(size, 5) */, &SpellOptions {
accuracy, max_edits, max_inspections, max_term_freq, prefix_length, min_word_length, min_doc_freq, string_distance:
StringDistance::{Internal, DamerauLevenshtein, Levenshtein, JaroWinkler, NGram}, sort: SuggestSort, suggest_mode:
SuggestMode::{Missing, Popular, Always}, lowercase_terms } /* Default = Elasticsearch's */) -> Result<Vec<SuggestWord {
term, score, freq }>>` per analyzed token (Lucene's `DirectSpellChecker`, pruning included); truncate to `size`.
Phrase suggester (`phrase_suggest::*`): `Searcher::phrase_suggest(&[PhraseToken { term, position_increment, shingle,
synonym }] /* the suggest analyzer's tokens */, &PhraseSuggestOptions { field, real_word_error_likelihood, confidence,
max_errors, separator, shard_size, gram_size /* the field's max shingle size, else 1 */, require_unigram, token_limit,
smoothing: Smoothing::{StupidBackoff { discount /* implicit default: 0.4f32 as f64 */ }, Laplace, LinearInterpolation},
generators: Vec<DirectGenerator { field, spell: SpellOptions, size, pre_filter, post_filter: Option<Box<dyn Fn(&[u8]) ->
Vec<(term, pos_inc)>>> }> }) -> Result<PhraseSuggestion { corrections: Vec<Correction { terms, user_input, score }>,
cutoff_score }>`; `Correction::join(sep, Option<(pre, post)>)` builds text / highlighted; collate and `size` are the
caller's (option score = `score as f32`).

Collectors (`collector::*`): `TopScoreCollector` (`track_total_hits` threshold, search_after,
min_score), `TopFieldCollector` (multi-key `SortKey`s, missing, `SortMode`, `NestedSort`,
search_after, early termination on the index sort; `compare_field_docs` merges shards; `SortKey::GeoDistance(GeoDistanceSort
{ field, points: [(lat, lon)], distance_type: Arc|Plane, unit_meters, reverse, mode: Option /* min asc, max desc */, nested })`
= `_geo_distance` (Elasticsearch's values; no `sum` mode — reject it; missing = Infinity), `SortKey::Script(ScriptSort {
script: Arc<dyn SortScript /* leaf(seg) -> LeafSortScript::number(doc) */>, reverse, mode, nested })` = `_script` numbers),
`TotalHitCountCollector`, `MultiCollector`, `TerminateAfter`, `MinScore`, `PostFilter::new(&searcher, &filter, inner)`
(`post_filter`), `CollapseCollector`
(groups by keyword / numeric doc values; hits carry the group value for inner hits),
`QueryRescorer` (Elasticsearch's window rescoring and explanations), managers for concurrent
search (`TopScoreManager` — its slices share the hit count and best score bar, Lucene's `HitsThresholdChecker` /
`MaxScoreAccumulator` —, `TopFieldManager`, `CountManager`; `CollectorManager::new_collectors(n)` creates one search's
collectors). Highlighting:
`highlight::highlight(query, values, analyzer, field, &HighlightOptions)` — unified (default),
plain, fvh (Lucene's FastVectorHighlighter pipeline; `highlight_in(searcher, ..)` expands multi-term queries against
the index as Elasticsearch's fvh does). Explanations match Elasticsearch's text, including the
query-cache wrapper's (`1.0 = <query>` for filter clauses).

Profiling (`search::profile`): `searcher.with_profiler(Some(Profiler::new()))` times every weight built through
`Searcher::weight_of` (composites route sub-queries through it); `Profiler::tree() -> Vec<ProfileNode { kind /* Lucene
class */, description, breakdown: [(key, nanos, count)] /* Elasticsearch's 10 timing types */, time_in_nanos, children }>`,
`Profiler::rewrite_time()`; `ProfileCollector::new(c)` times a collector. Profiled searches use plain bulk scoring.

Callers own a few Elasticsearch rules: bool clauses are added in Elasticsearch's order (must,
must_not, should, filter) since explanations list them that way; `random_score` salts with
`(index name hash << 10) | shard id`; `terminate_after` reports `count_if_cheap` as the total
when there is one; `track_total_hits` totals above the threshold are lower bounds to clamp.

## `search-aggs`

Owns aggregation parsing (Elasticsearch JSON), per-segment collection, shard partials, the
cross-shard reduce and rendering. Field metadata, queries, scripts and the fetch phase come
through traits that `search-es` implements:

```rust
pub fn parse(aggs: &Json /* ordered JSON */, opts: &ParseOptions) -> Result<AggRequest, AggError>;  // index-independent
pub fn parse_value(aggs: &serde_json::Value) -> Result<AggRequest, AggError>;
pub struct ParseOptions<'a> { src: Option<&'a [u8]> /* body, for [line:col] */, base: Vec<Seg> /* ["aggs"] */,
                              check_query: Option<&dyn Fn(&Json, &[Seg]) -> Result<()>>,      // filters, background_filter
                              decimal_format: Option<&dyn Fn(&str) -> Result<Arc<dyn NumberFormatter>>> }
impl AggRequest {
    fn validate(&self) -> Vec<String>;   // action_request_validation_exception messages (pipelines, orders)
    // ShardOptions { budget, matches_all /* query matches every live doc: segment-statistics shortcuts;
    //                run() sets it for a MatchAllQuery */ } — construct with ..Default::default()
    fn collector(&self, &dyn FieldLookup, &search_index::Searcher, &ShardOptions { budget }) -> Result<AggCollector>;
    fn run(&self, &dyn FieldLookup, &Searcher, query: &QueryRef, &ShardOptions) -> Result<Aggs>;  // collector + search + finish
}
impl search_index::Collector for AggCollector;  impl AggCollector { fn finish(self) -> Result<Aggs> /* = ShardAggs */ }
pub fn reduce(parts: Vec<Aggs>, req: &AggRequest, opts: &ReduceOptions) -> Result<Aggs>;
pub struct ReduceOptions { final_reduce, max_buckets /* search.max_buckets */, has_batched_result,
                           scripts: Option<Arc<dyn ScriptHost>> /* reduce-side scripts */, budget: Option<Arc<MemoryBudget>> }
impl Aggs { fn to_json(&self, typed_keys: bool) -> Json; fn get(&self, name) -> Option<&Agg> }
pub trait FieldLookup {   // per shard; every method but `field` has a default
    fn field(&self, name) -> Result<Option<AggField /* type, SourceType (dv kind), number, format, nested path */>>;
    fn index_name(&self) -> &str; fn query(&self, &Json) -> Result<QueryRef>; fn nested(&self, path) -> NestedLookup;
    fn root_filter(&self) -> QueryRef; fn global_query(&self) -> QueryRef /* superset of significant_*, `global` */;
    fn join(&self, child_type: Option<&str>, parent_type: Option<&str>) -> Option<JoinRelation>;  // children / parent
    fn scripts(&self) -> Option<Arc<dyn ScriptHost>>; fn date_formatter(&self, pattern); fn decimal_format(&self, pattern);
    fn now_millis(&self) -> i64; fn doc_count_field(&self) -> Option<u32> /* _doc_count */;
    fn fetcher(&self) -> Option<Arc<dyn TopHitsFetcher>> /* top_hits fetch phase */; fn max_inner_result_window(&self) -> u32;
    fn hit_sort(&self, sort: &Json) -> Result<Vec<HitSort>>; fn text_terms(&self) -> Option<Arc<dyn TextTerms>> /* significant_text */;
    fn shard_id(&self) -> (u32, u32); fn index_uuid(&self) -> Option<&str> /* random_sampler's default shard seed = ShardId.hashCode() */;
    fn time_series(&self) -> Option<TimeSeriesInfo> /* time_series-mode index */;
}
pub struct TimeSeriesInfo { dimensions: Vec<AggField>, tsid_field: Option<u32> /* sorted `_tsid` dv */, timestamp_field: Option<u32> /* @timestamp */ }
// AggField.counter: a `time_series_metric: counter` field of a time_series index (rate → reset-aware counter rate;
// terms/date_histogram/missing/… refuse it, as in Elasticsearch).
pub struct JoinRelation { join_field: u32 /* "<join>#<parent>" sorted dv */, parent_filter: QueryRef, child_filter: QueryRef }
pub trait ScriptHost {   // aggs, aggs_init/map/combine/reduce, bucket_script/selector, moving_fn, script_heuristic
    fn values(&self, &Script) -> Result<Arc<dyn ValuesScript>>;
    fn scripted_metric(&self, init: Option<&Script>, map: &Script, combine: &Script, agg_params: &Json)
        -> Result<Arc<dyn ScriptedMetricScripts>>;   // script params already merged with agg params
    fn scripted_metric_reduce(&self, &Script) -> Result<Arc<dyn ScriptedMetricReduce>>;  … }
```

Doc-values encodings `search-es` indexes for aggregations are listed in `search_aggs::field`'s
docs; besides the numeric/keyword/ip/geo ones: `point` (cartesian) fields are sorted numeric
`floatToSortableInt(x) << 32 | floatToSortableInt(y)` (`SourceType::CartesianPoint`,
`metrics::cartesian::encode_xy`), and `*_range` fields are one binary value per doc holding all the
doc's ranges in `BinaryRangeUtil`'s encoding (`SourceType::Range(RangeType)`; build it with
`search_aggs::range_field::encode_{long,double,float,ip}_ranges` — integer and date ranges use the
long encoding; exclusive bounds stepped inward first) so `histogram`/`date_histogram` on ranges and
`cardinality` hashes match Elasticsearch. Pre-aggregated fields: `histogram` is one binary value per
doc in `EncodedTDigest` centroid encoding (VLong count + big-endian double per centroid with count > 0;
`search_aggs::histogram_field::encode_histogram`, `SourceType::Histogram`); `aggregate_metric_double`
is `SourceType::AggregateMetric(AggregateMetricFields { min, max, sum, value_count })`, one sorted
numeric doc-values field per stored metric (`min`/`max`/`sum` sortable doubles, `value_count` integer).
`exponential_histogram` is one binary value per doc built with `search_aggs::exp_histogram::encode(&ExpHistogramValue
{ scale, zero_threshold, zero_count, negative, positive, sum, min, max })` (`SourceType::ExponentialHistogram`; parse and
validate as `ExponentialHistogramParser` does; absent `sum`/`min`/`max` are estimated by `encode` exactly as the mapper does).
`geo_shape` / `shape` fields keep search-index's shape doc value (`search_index::query::shape::doc_value`, every triangle of
every value) and add a companion binary field with the centroid of the *original* geometry:
`search_aggs::shape_field::encode_centroid(&calc, Coords::Geo | Coords::Cartesian)` after `CentroidCalculator::add` of each
value's `shape_field::Geometry` (`SourceType::GeoShape { centroid: Some(field) }` / `CartesianShape { .. }`; without the
companion the centroid is approximated from the triangles).

Every aggregation of Elasticsearch 9.5 core, `aggregations` module, analytics, spatial and
parent-join is implemented except `categorize_text` and `frequent_item_sets` (Platinum ML features; 501
`unsupported_operation_exception`). `exponential_histogram`
fields feed `sum`/`avg`/`min`/`max`/`value_count`/`percentiles`/`percentile_ranks`/`boxplot`/`histogram`/`range`
(`HistogramUnionState`, `search_aggs::exp_histogram`); shapes feed `geo_bounds`/`geo_centroid`/`geotile_grid`/
`geohash_grid`/`geohex_grid` and `cartesian_bounds`/`cartesian_centroid`. Results match Elasticsearch bit for bit, including reduce
orders (Java `HashMap` iteration, Lucene priority queues), sketches (TDigest, HdrHistogram, HLL++),
`random_sampler`'s sample for a given `seed` / `shard_seed`, `t_test` p-values (Commons Math's
`FastMath`/t-distribution, `search_aggs::commons_math`) and `geohex_grid` cells (Elasticsearch's H3
port with its jafama trigonometry, `search_aggs::h3`).

## `search-painless`

```rust
pub struct Engine;   // Engine::new(Settings) / Default; Send + Sync. Compiled-script cache (script.cache.max_size/expire),
                     // script.max_compilations_rate token bucket, script.max_size_in_bytes, painless regex/loop settings
pub struct Settings { max_size_in_bytes, cache_max_size, cache_expire, max_compilations_rate: Option<(u32, Duration)>,
                      regex_enabled: RegexEnabled /* True|False|Limited */, regex_limit_factor, max_loop_counter,
                      max_object_bytes /* 256 MiB: largest array/string/collection a script builds → out_of_memory_error */,
                      stack_bytes /* 1 MiB of native stack per execution → stack_overflow_error */,
                      max_allocation_bytes: Vec<(Context, u64)> /* script.painless.max_allocation_bytes.context.<c>.limit;
                                                                   empty = -1b (off) */ }
pub enum Context { PainlessTest, Score, Filter, Field, Update, UpdateByQuery, Reindex, Ingest, ProcessorConditional,
                   Aggs, AggsInit, AggsMap, AggsCombine, AggsReduce, AggregationSelector, BucketAggregation,
                   NumberSort, StringSort, BytesRefSort, TermsSet, Similarity, SimilarityWeight, BooleanField, DateField,
                   DoubleField, GeoPointField, GeometryField, IpField, KeywordField, LongField, CompositeField,
                   Template, IngestTemplate, XpackTemplate, Analysis, Interval, MovingFunction, ScriptHeuristic, … }
impl Context { fn from_name(&str) -> Option<Context>; fn name(self) -> &'static str }   // Elasticsearch's context names
// contexts::{script_context_json(), script_language_json()}: `GET _script_context` / `GET _script_language` bodies
impl Engine {
    fn compile(&self, src: &str, ctx: Context) -> Result<Arc<Script>, ScriptError>;            // inline (size-checked, rate-limited)
    fn compile_stored(&self, Lang, src, ctx, options) -> Result<Compiled, ScriptError>;        // stored scripts: no size check
    fn compile_lang(&self, Lang, src, ctx, options: &[(String, String)]) -> Result<Compiled, ScriptError>;
    fn compile_template(&self, src, ctx, options) -> Result<Arc<mustache::Template>, ScriptError>;
    fn update_settings(&self, Settings); fn settings(&self) -> Settings; fn stats(&self) -> Stats;   // _nodes/stats script
}
impl Script {   // Arc, Send + Sync, immutable
    fn instance(&self) -> Instance<'_>;      // per thread / per segment: reuse across documents (no per-doc allocation)
    fn run(&self, &mut dyn Host, params: &serde_json::Value, &mut Vars) -> Result<Value, ScriptError>;  // one-shot
    fn needs_score(&self) -> bool; fn uses_var(&self, name) -> bool; fn is_deterministic(&self) -> bool;
    fn doc_fields(&self) -> &[Box<str>];     // literal doc['f'] reads, index = slot of Host::doc_values_at
}
impl Instance { fn set_params(&mut self, &Json); fn set_params_value(&mut self, Value); fn set_var(&mut self, name, Value);
                fn var(&self, name) -> Option<&Value>;   // ctx, state, states, _value, _score override, values, …
                fn execute(&mut self, &mut dyn Host) -> Result<Value, ScriptError>;
                fn execute_f64(..) -> Result<f64, ScriptError>; fn execute_bool(..) -> Result<bool, ScriptError>;
                fn emitted(&self) -> &[Value]; fn emitted_fields(&self) -> &[(String, Vec<Value>)];   // runtime fields
                fn set_field_name(&mut self, &str); fn explanation(&self) -> Option<&str>;
                fn set_cancellable(&mut self, bool) }   // search contexts (score, filter, fields, sort, aggs, runtime
                // fields) when the search binds a cancellation check: no max_loop_counter, Host::check_cancelled every
                // 1,000 loop iterations instead (Elasticsearch 9.5); off for _execute and write scripts
pub trait Host {   // every method has a default; implement what the context needs
    fn score(&mut self) -> f64;
    fn doc_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError>;   // Ok(false): unmapped
    fn doc_values_at(&mut self, slot: usize, field: &str, out: &mut DocValues) -> Result<bool, HostError>; // slot into doc_fields()
    fn field_values(&mut self, field: &str, out: &mut DocValues) -> Result<bool, HostError>; // field API (`field('f')`, `$`):
        // FielddataOperation.SCRIPT, i.e. `_source` values where doc values are missing (text fields, doc_values: false);
        // defaults to doc_values
    fn has_field(&mut self, field: &str) -> bool; fn source(&mut self) -> Option<Value>; fn term_stats(&mut self) -> Option<TermStats>;
    fn doc_id(&mut self) -> i32; fn shard(&mut self) -> (i32, String); fn now_millis(&mut self) -> i64;
    fn stored_field(&mut self, field: &str) -> Result<Option<Vec<Value>>, HostError>;   // params._fields[f]: None = unmapped
    fn check_cancelled(&mut self) -> Result<(), HostError>;   // cancellable instances: Err stops the script
    fn with_vector(&mut self, field, f: &mut dyn FnMut(&VectorView)) -> Result<Option<bool>, HostError>;
        // zero-copy dense_vector for the vector score functions (Ok(None): use doc_values)
}
pub struct DocValues { pub kind: DocKind, pub longs, pub doubles, pub geo: Vec<[f64; 2]>, pub bytes, pub vector: Vec<f32>,
                       pub vector_bytes: Vec<i8> /* byte and bit vectors (bit: packed, MSB first) */,
                       pub element: VectorElement /* Float|BFloat16|Byte|Bit */, pub vector_binary: bool /* index: false */,
                       pub magnitude /* stored magnitude (index:false vectors, cosine fields): used as is */,
                       pub vector_count, pub magnitudes /* rank_vectors: vectors flattened in vector/vector_bytes */ }
                     // clear(kind) + push / push_str per document; reused buffers
pub enum DocKind { Long, Integer, Short, Byte, UnsignedLong, Double, Float, HalfFloat, ScaledFloat, String, Boolean, Date,
                   DateNanos, GeoPoint, GeoShape /* geo: [centroid, top_left, bottom_right, label], longs: [dim type] */,
                   Ip, Binary, Version, DenseVector, RankVectors }
pub enum Value { Null, Bool, Byte, Short, Char, Int, Long, Float, Double, Str(Rc<JStr>), List, Map, Set, Obj }  // 16 bytes
                     // Value::from_json(&Json, map_class) / from_json_map, to_json(), to_java_string(),
                     // try_to_json() -> Result<Json, ToJsonError{kind, reason}> (1,000-level nesting limit like Jackson,
                     // object size limit); to_json() gives null for those
// write scripts: es::write::{ctx_value(CtxKind, source, metadata: &[(&str, Value)], now) -> Result<Value, String>,
//   ctx_of(&Value) -> Option<Rc<Ctx>>, CtxKind::{Update, Upsert, UpdateByQuery, Reindex, Ingest}}
//   Ctx: source() (new _source), metadata(), meta_get(key), op() (index|noop|delete|create, Elasticsearch's getOp rules)
// xcontent::{parse_source(json) (LinkedHashMap, document order), parse_source_unordered(json) (HashMap: upsert docs,
//   ingest, update_by_query, reindex), to_xcontent(&Value) -> Result<String, XContentError> (Jackson output, XContent
//   type rules), check_ingest_value(&Value) (IngestDocument.deepCopy rules, simulate)}
// value::zoned_date_time_millis(ms) (ingest timestamp / metadata().getNow())
// ingest `if` conditions: es::write::conditional_ctx(&ctx) -> Value (read-only UnmodifiableIngestData view of an
//   ingest ctx or a plain map; build it only when script.uses_var("ctx")), Context::ProcessorConditional, execute_bool
// pipeline aggs: Context::BucketAggregation / AggregationSelector (buckets_path values in params, NaN for gaps),
//   Context::MovingFunction (set_var("values", Value::double_array(window)))
// grok::{PatternBank::legacy()/ecs_v1()/new/extend_with, Grok::new(&bank, expr) -> Result<Grok, GrokError{kind,message}>,
//   Grok::captures(text) -> Result<Option<Vec<(field, GrokValue)>>, ConvertError>, warnings()}
//   and dissect::Dissect::new(pattern, append_separator)?.parse(Option<&str>) -> Option<HashMap>: reusable by the
//   ingest grok/dissect processors (Joni dialect and Elasticsearch's error texts)
// es::uri::{parse_uri(s), parse_url(s)} -> Option<Parts{scheme,user_info,host,port,path,query,fragment}>:
//   java.net.URI (decoded getters) / java.net.URL fallback, as the ingest uri_parts processor uses them
// context objects (es::misc): similarity_query(boost)/similarity_field(docCount,sumDocFreq,sumTTF)/
//   similarity_term(docFreq,TTF)/similarity_doc(length,freq) + set_var("weight"|"query"|"field"|"term"|"doc");
//   analysis_token(Token{term,position,position_increment,position_length,start_offset,end_offset,type_,keyword})
//   as var "token"; interval(start,end,gaps) as var "interval"; script_heuristic: params = HashMap of the
//   user params + long_accessor(v) for _subset_freq/_superset_freq/_subset_size/_superset_size, one map per
//   shard updated with set_long_accessor (scripts may write it, like Elasticsearch's)
// text::{number_format(locale, Style::{Number,Integer,Percent,Currency}) -> DecimalFmt, symbols(locale),
//   DecimalFmt::new(symbols, pattern)?.format_double/format_long/parse}: java.text.DecimalFormat with the JDK's
//   CLDR data for its 1,189 locales (locale = Locale.toString(), "" = ROOT); scripts get NumberFormat,
//   DecimalFormat(Symbols), ParsePosition, FieldPosition, Normalizer (unicode-normalization) and Currency
// ingest scripts: Processors.{bytes,lowercase,uppercase,json,jsonLenient,urlDecode,communityId,uriParts} built in;
//   Json.load and Processors.json parse like Jackson (comments, strict duplicates, exact x_content errors)
```

`ScriptError` renders as Elasticsearch's `script_exception` (`to_response()`: `{error: {root_cause, type,
reason, script_stack, script, lang, position, caused_by}, status}`; `to_json()` without the envelope),
with `status` (400, 429 for the compilation circuit breaker). Write APIs wrap it the way Elasticsearch
does (`update`: `illegal_argument_exception` "failed to execute script" caused by it; ingest and
update-by-query: as is). Mustache (`lang: mustache`, search templates): `Template::render_json(&params)`
/ `render(&Value, max_output)`. Fixture harnesses in `crates/search-painless/tests/` show how each
context is driven (`execute.rs`, `write.rs` (also ingest conditions and `Processors`), `search.rs`, `pipeline.rs`,
`similarity.rs`, `heuristic.rs`, `analysis.rs`, `call_sites.rs`, `mustache.rs`). `def` operator call sites keep
what they linked across executions of a `Script` (as Painless's do), so null-operand errors read `leftObject`
after the first execution: share one compiled `Script` per source, as Elasticsearch's script cache does.
`search-painless` enables serde_json's `float_roundtrip` feature: params and sources must parse doubles
exactly, as Jackson does (the default parser can be off by one ulp).
Straight-line per-document scripts — score, filter, sort, aggregation (`def` results, boxed), script-field and
runtime-field (`emit` of long/double/date/boolean) scripts made of locals and assignments, `if`/`return`,
`for (x : doc['f'])` over numeric/boolean doc values, `_score`, `params`, `doc[..].value/size()/isEmpty()/empty`,
`Math`, decay/saturation/sigmoid — run on a fast path specialized on an instance's first document from its
params and the fields' `DocKind`s: keep one `Instance` per segment and return the same `DocKind` for a field
on every document (any other kind still works, through the interpreter); `set_params*`/`set_var` re-specialize.
No script can take the node down: compilation runs on its own large-stack thread and fails deep sources with
Elasticsearch's `stack_overflow_error`; executions bound their native stack (`stack_bytes`; run them on threads
with at least that much spare stack), object sizes (`max_object_bytes`), and drop deeply nested values without
recursion; a panic (a bug, here or in the host) becomes a 500 `illegal_state_exception` script error.

## `search-store`

```rust
pub struct ShardId { pub index_uuid: String, pub shard: u32 }
pub struct SegmentId(pub [u8; 16]);  pub type SegmentKey = SegmentId;   // codec's SegmentInfo::id; hex in keys/JSON
pub struct LiveGen(pub u128);        // unique live-docs generation (ms << 64 | random), returned by the store
pub struct SegmentData;              // Arc'd mmap of a cached file: Deref<[u8]>, AsRef<[u8]>, to_bytes() -> Bytes (zero-copy)
pub struct Version { pub epoch: u64, pub generation: u64, pub token: u64 }   // history order of manifests/states: epoch, then generation
pub struct Fenced { cluster, epoch, owner_epoch }  pub struct WalFull { unshipped_bytes, limit }  // typed errors: anyhow downcast_ref
pub struct ShardManifest { generation, epoch /* set by store */, written_ms /* set by store */, segments: Vec<SegmentRef { id, bytes, max_doc, del_count, live: Option<LiveGen> }>,
                           max_seq_no, local_checkpoint /* -1 = none */, primary_term, mapping_version,
                           user_data: BTreeMap<String,String>, wal: Vec<WalSource> /* maintained by the store */, base: Option<BranchBase>, wal_head }
pub struct ClusterState { generation, epoch, written_ms /* set by store */, json: Bytes /* opaque JSON from search-es */ }

pub struct Store;   // one per node, Clone
impl Store {
    pub async fn open(objects: pg_storage::Objects, cache_dir: impl Into<PathBuf>, cfg: StoreConfig) -> Result<Store>;
    pub async fn open_with_wal_objects(objects, wal_objects: Objects, cache_dir, cfg) -> Result<Store>;  // WAL in its own store (S3 Express One Zone); same on every node
    pub fn objects(&self) -> &Objects;  pub fn wal_objects(&self) -> &Objects;
    pub fn wal(&self) -> &Wal;
    pub async fn close(&self) -> Result<()>;                                   // commit + ship the WAL, stop it
    pub fn set_epoch(&self, cluster: &str, epoch: u64);   // the cluster's lease epoch (default 0); newer epochs fence older writers
    pub fn epoch(&self, cluster: &str) -> u64;
    // segments: write to segment_path(id) (staging, same fs as the cache), then upload; the store takes the file
    pub fn segment_path(&self, id: &SegmentId) -> PathBuf;
    pub async fn upload_segment(&self, seg: &Path) -> Result<SegmentKey>;     // file must be named <32 hex>.seg
    pub async fn open_segment(&self, key: &SegmentKey) -> Result<SegmentData>;  // cache hit, or one shared fetch
    pub async fn open_segments(&self, keys: &[SegmentKey]) -> Result<Vec<SegmentData>>;
    pub async fn fetch_segment(&self, key: &SegmentKey) -> Result<SegmentFetch>;  // use before the download completes (below)
    pub async fn write_live_docs(&self, seg: &SegmentId, data: Bytes) -> Result<LiveGen>;
    pub async fn open_live_docs(&self, seg: &SegmentId, gen: LiveGen) -> Result<SegmentData>;
    // shard lifecycle: recover -> open segments -> replay -> attach -> append/flush -> detach
    pub async fn recover_shard(&self, cluster: &str, shard: &ShardId) -> Result<ShardRecovery>;   // .manifest, .checkpoint()
    pub async fn recover_shards(&self, cluster: &str) -> Result<Vec<ShardRecovery>>;   // wake: every shard, one LIST + concurrent GETs
    pub async fn read_latest_manifests(&self, cluster: &str) -> Result<Vec<(ShardId, ShardManifest)>>;
    pub async fn replay(&self, rec: &mut ShardRecovery, f: &mut (dyn FnMut(&WalRecord) -> Result<()> + Send)) -> Result<ReplayStats>;
    pub async fn attach_shard(&self, rec: ShardRecovery) -> Result<(WalShard, ShardManifest)>;
    pub async fn write_manifest(&self, cluster: &str, shard: &ShardId, m: &ShardManifest) -> Result<ShardManifest>; // flush; trims WAL
    pub async fn detach_shard(&self, wal: WalShard) -> Result<()>;            // unload (flush first)
    pub async fn read_manifest(&self, cluster: &str, shard: &ShardId) -> Result<Option<ShardManifest>>;  // None: none, or deleted
    pub async fn read_manifests(&self, cluster: &str, shard: &ShardId) -> Result<Vec<ShardManifest>>;
    pub async fn list_shards(&self, cluster: &str) -> Result<Vec<ShardId>>;
    pub async fn delete_shard(&self, cluster: &str, shard: &ShardId) -> Result<()>;   // index deleted; tombstone inside the PIT window
    pub async fn read_cluster(&self, cluster: &str) -> Result<Option<ClusterState>>;
    pub async fn write_cluster(&self, cluster: &str, state: &ClusterState) -> Result<u64>;  // -> generation
    pub async fn cluster_exists(&self, cluster: &str) -> Result<bool>;
    pub async fn branch(&self, parent: &str, child: &str, at_ms: Option<u64>) -> Result<()>;
    pub async fn delete_cluster(&self, cluster: &str) -> Result<()>;
    pub fn release_cluster(&self, cluster: &str);                             // forget in-memory tracking on unload
    pub async fn collect_garbage(&self, grace: Duration) -> Result<GcStats>;  // one place per bucket
    pub async fn cluster_usage(&self, cluster: &str) -> Result<ClusterUsage>;   // incl. cached_bytes on this node
    pub fn stats(&self) -> StoreStats;  pub fn set_cache_budget(&self, bytes: u64);
}
impl WalShard { pub fn append(&self, seq_no: i64, op: &[u8]) -> Result<Lsn>;
                pub fn append_many<B: AsRef<[u8]>>(&self, ops: &[(i64, B)]) -> Result<Lsn>;  // one lock, one commit, all or nothing
                pub fn stats(&self) -> ShardWalStats; }
impl Wal {
    pub async fn wait_ack(&self, lsn: Lsn) -> Result<()>;      // translog.durability=request: local fsync, or S3 with sync_objects
    pub async fn durable(&self, lsn: Lsn) -> Result<()>;       // local fsync
    pub async fn durable_remote(&self, lsn: Lsn) -> Result<()>; // in object storage (seals the file now)
    pub async fn sync_remote(&self) -> Result<()>;  pub fn flush_now(&self);  pub fn stats(&self) -> WalStats;
    pub fn cluster_stats(&self, cluster: &str) -> ClusterWalStats;   // per-tenant uncommitted ops/bytes
    pub fn pinning_shards(&self) -> Vec<PinningShard>;   // shards with uncommitted ops, oldest pinned WAL file first: flush candidates
}
pub struct WalRecord<'a> { pub at_ms: u64, pub seq_no: i64, pub cluster: &'a str, pub index_uuid: &'a str, pub shard: u32, pub payload: &'a [u8] }
impl SegmentFetch {   // Clone; resolves once the size is known (the first request also brings the last 8 MiB: footer + TOC)
    pub fn len(&self) -> u64;  pub fn downloaded(&self) -> u64;  pub fn is_ready(&self, r: Range<u64>) -> bool;  // false outside the segment
    pub fn prioritize(&self, ranges: &[Range<u64>]);                 // e.g. term dictionaries and skip data from the TOC
    pub async fn read(&self, r: Range<u64>) -> Result<Bytes>;         // waits only for the parts holding `r` (and asks for them first); zero-copy
    pub async fn wait_ready(&self, rs: &[Range<u64>]) -> Result<()>;  // prioritize + wait for all of them
    pub unsafe fn map_partial(&self) -> Result<SegmentData>;          // whole-file mmap now (missing parts read 0); read only ready ranges
    pub fn data(&self) -> Option<SegmentData>;  pub async fn complete(&self) -> Result<SegmentData>;  // the mmap once downloaded
}
// Cold large segment in search-index, no codec change: f = fetch_segment; m = unsafe { f.map_partial() };
// f.wait_ready(&[0..24 /* header */, len-24..len, toc]) ; ContainerReader::open(Slice::from_owner(m)) -> section entries;
// f.wait_ready(sections SegmentReader::open parses: meta, term index/FST, block tables) ; SegmentReader::open ;
// before a query touches postings/doc values/stored fields of a field: f.wait_ready(those sections) (or use f.data() once Some).
```

`StoreConfig { node, wal: WalConfig { flush_delay: 5ms, flush_bytes: 4 MiB, eager_below: 8, file_bytes: 64 MiB,
ship_interval: 1s, sync_objects, fsync, max_pending_bytes, ship_concurrency, zstd_level, replay_cache_bytes,
max_local_bytes: 8 GiB /* unshipped bytes past which appends fail with WalFull */ }, cache_bytes, manifest_retain (4),
manifest_retain_ms (point-in-time window, 0), state_retain (4), single_put_bytes, verify: Option<Verifier> }`.
`Verifier::new(|d: &SegmentData| -> Result<()>)` checks every *downloaded* segment (`d.name()` = `<id>.seg`) and
live-docs file (`<id>.<gen>.liv`) before the cache keeps it; search-server should pass the codec's whole-file check
for `.seg` (`ContainerReader::open(Slice::from_owner(d.clone()))?.verify()`, zero-copy) and `Ok(())` otherwise. A failure fails the download for every waiter
(`SegmentFetch` readers included), nothing is cached, and the next open fetches again (`CacheStats::verify_failures`). Replay hands records with
`seq_no > local_checkpoint` in log order (possibly out of seq_no order, possibly already in segments, as with
Elasticsearch's translog); from object storage it reads only the shard's blocks (suffix read for the directory,
ranged read per block). Segments larger than 8 MiB are fetched as parallel ranged reads (`pg_storage::Objects::
get_range` / `get_suffix`); a download reserves its whole size in the cache budget as soon as the size is known
(`CacheStats::downloading`), so concurrent cold downloads can't overrun the disk. `test_objects(name)` gives S3 (`ROLL_E2E_S3_BUCKET`, under `search-tests/`; prefixes older than 6 h are swept by later runs) or a dir under `test_dir()` (`<tmp>/roll-search-store-<pid>`,
swept once the process is gone: /tmp is RAM here).
Reads of "the latest" manifest or cluster state (`read_manifest`, `read_latest_manifests`, `recover_shard(s)`,
`read_cluster`, `branch`) read exactly the newest generation: an object-store error or a damaged object is an
error naming the key, never a silent fallback to an older generation (the next write would supersede the newer
one for good). `delete_shard` with `manifest_retain_ms > 0` writes a tombstone generation
(`<version>.deleted.json`) instead of deleting the commit points: the shard is gone for every read and for
`list_shards`, but a point-in-time branch to before the deletion restores its data; `collect_garbage` removes
the history once the tombstone is older than the window. Writes of a deleted shard (`write_manifest`,
`attach_shard`, a flush racing `DELETE /index`) fail with "shard … was deleted". Each WAL run (process) writes
`search/wal/<node>/<first file:016>.run` at startup: a range a crashed run left open ends where the node's next
run starts, for garbage collection and replay alike. A range another node froze is `WalSource::exact`: a file of
it missing at replay (lost, not skipped) fails the replay instead of leaving a hole in the shard's history.
Fencing: search-server calls `set_epoch(cluster, lease_epoch)` before recovering a cluster's shards. A write
(`write_manifest`, `attach_shard`, `detach_shard`, `write_cluster`, `delete_shard`) that finds a newer epoch
fails with `Fenced`, and the node's WAL then refuses the cluster's appends: unload the cluster. `WalFull` is
transient (object storage is behind): answer 429 `es_rejected_execution_exception`.

Object keys (never reused; a rewritten object gets a new key):

| key                                                         | what |
| ----------------------------------------------------------- | ---- |
| `search/seg/<segment id hex>.seg`                            | a segment (shared by branches) |
| `search/seg/<segment id hex>.<live gen hex32>.liv`           | live docs of a segment at a generation (unique per write, so shared by branches) |
| `search/wal/<node>/<seq:016>.zst`                            | a sealed WAL file of all clusters on the node, regrouped into one zstd block per shard plus a directory |
| `search/wal/<node>/<first file:016>.run`                     | a WAL run marker: the node (re)started appending at this file |
| `search/c/<cluster>/state/<epoch:020>-<gen:020>-<token:016x>.json.zst` | cluster metadata: indices, mappings, settings, aliases, templates, pipelines |
| `search/c/<cluster>/i/<index uuid>/<shard>/<epoch:020>-<gen:020>-<token:016x>.json` | a shard manifest (commit point); latest = highest (epoch, gen); the random token keeps keys unique when a cluster name is reused; 20 digits hold any u64 epoch |
| `search/c/<cluster>/i/<index uuid>/<shard>/<epoch:020>-<gen:020>-<token:016x>.deleted.json` | a deleted shard's tombstone (kept while the point-in-time window needs the shard's history) |
| `search/gc/pending.json`                                     | garbage collector state (first sighting of unreferenced objects) |
| `search/gc/marks.bin`                                        | garbage collector's memory of what each manifest key references (a restarted collector reads only new manifests) |

## `search-es` and `search-server`

- `search-es` exposes `Cluster` (one loaded tenant) and `pub async fn handle(cluster: &Cluster, req: EsRequest) -> EsResponse`.
  The route tables are generated from Elasticsearch's REST API spec JSON (Apache-2.0, vendored
  under `crates/search-es/spec/`) and, for the opensearch dialect, OpenSearch 3.9.0's
  (`crates/search-es/spec/opensearch/`), so every path, method and parameter of each dialect is
  known; unimplemented APIs answer the way Elasticsearch answers for an unknown or unsupported
  feature.
- `search-server` owns `Node` (`open(spec)`, `idle()`, `unload(cluster)`, `branch(…)`), a
  `Catalog` trait the control plane implements (`lookup(cluster) -> ClusterSpec`), HTTP on hyper
  with keep-alive and pipelining, auth, TLS/SNI and the PROXY protocol, following
  `redis-server`'s `node.rs`/`directory.rs` patterns.

```rust
// search-es
pub struct EsRequest { pub method: String, pub uri: String /* raw path?query */, pub headers: Vec<(String, String)>, pub body: Bytes }
pub struct EsResponse { pub status: u16, pub headers: Vec<(String, String)>, pub body: Bytes, pub head: bool /* send Content-Length, no body */ }
pub async fn handle(cluster: &Cluster, req: EsRequest) -> EsResponse;            // Send future
pub fn rest::error_response(e: &EsError, req: &EsRequest, d: Dialect) -> EsResponse; // errors before a cluster is known (auth, limits)
#[async_trait] pub trait MetaStore: Send + Sync { async fn load(&self) -> Result<Option<Bytes>>; async fn save(&self, version: u64, state: Bytes) -> Result<()>; }
pub struct MemoryStore;                                                           // MetaStore in memory
pub struct ClusterConfig { pub name, pub dialect: Dialect /* Elasticsearch|OpenSearch */, pub node_name, pub destructive_requires_name: bool,
                          pub node_settings: Vec<(String, String)> /* -E k=v: node.attr.*, node.processors, .. */, pub test_features: bool,
                          pub host_metrics: bool /* false: node stats report synthetic figures (shared nodes must not leak the host) */ }
                          // + fn node_setting(key) -> Option<&str>; fn processors() -> usize
impl Cluster { async fn open(ClusterConfig, Arc<dyn MetaStore>) -> Result<Cluster>; async fn in_memory(name) -> Cluster;
               fn state(&self) -> Arc<meta::ClusterState>;                        // published metadata (immutable snapshot)
               async fn update<R>(&self, f: impl FnOnce(&mut ClusterState) -> EsResult<R>) -> EsResult<R>; // persisted before published
               fn subscribe(&self) -> watch::Receiver<u64>; fn uuid(&self) -> String;
               fn index_analysis(&self, &ClusterState, &IndexMeta) -> EsResult<search_analysis::Analysis>; } // cached per index
// Cluster.tasks: cluster::TaskRegistry — register(action, description, parent, headers) -> TaskGuard (unregisters on drop),
//   list(), get(id) -> Option<TaskInfo { id, action, start_ms, cancellable, parent, headers, cancelled: Arc<AtomicBool> }>;
//   long-running work (search, by-query, reindex) registers here so `_tasks` / `_cat/tasks` / cancel see it.
// Metadata the engine reads: meta::ClusterState { version, state_uuid, metadata: Metadata { indices: BTreeMap<String, Arc<IndexMeta>>,
//   index_templates, component_templates, templates, persistent, transient, scripts, pipelines,
//   data_streams: BTreeMap<String, Arc<meta::data_streams::DataStream { indices /* write index last */, generation, .. }>>,
//   data_stream_aliases, synonyms: Arc<BTreeMap<String, Arc<SynonymSet { rules: BTreeMap<id, text> }>>>, synonyms_version, .. } }
//   + Metadata::data_stream_of(index) -> Option<&Arc<DataStream>>; ClusterState::check_unique_names / check_write_indices
// IndexMeta { name, uuid, state, settings: meta::settings::Settings, mapping: Arc<mapping::Mapping>, aliases, routing_num_shards, .. }
// meta::resolve::resolve(state, exprs, &IndicesOptions { include_data_streams, .. }, now) -> concrete index names (data streams expand to backing indices)
// api::indices::create_backing_index / api::data_streams::create_data_stream(st, name, now) — for auto-creating data streams on write
// analysis::{AnalysisCache, index_analysis, es_error} — search-analysis chains from index settings, errors as EsError
// meta::names::time_based_id() — 20-char auto ids (synonym rules now, document ids next)
// ClusterState.dialect — the cluster's Dialect (from its config, not persisted)
// Cluster::unassigned_reason(&st, &idx) -> Option<cluster::Unassigned> / shards_failed(&st, &idx) — primaries that can't start
//   (index/cluster allocation `none`, synonyms_set chains that don't build): red health, no stats, shards_acknowledged false
// Cluster.ops: cluster::ShardOps — record(uuid, shards, |c: &mut OpCounts| ..), get(uuid, shard) -> OpCounts { flush, refresh,
//   external_refresh }, forget(uuid); `_stats` flush/refresh counts come from here until the engine reports its own
// Metadata::cluster_setting(key) -> Option<&str> (transient over persistent); Metadata.desired_nodes, .repositories,
//   .repository_data (snapshots: meta::snapshots::{Repository, RepositoryData, Snapshot}, keyed by location)
// IndexMeta.recovery_source: Option<Arc<Map>> — set on indices restored from a snapshot (`_recovery` type SNAPSHOT)
// IndexMeta::{to_json, from_json} are public (the stored form; snapshots keep it)
// routes::{route_in(dialect, method, path), allowed_methods(dialect, path)}; Api::{spec_in(dialect), param_in(dialect, name)}
// meta::routing::{shard_for(idx, id, routing) -> u32, search_shards_for(idx, routing) -> Vec<u32>, hash_routing, murmur3_x86_32}
//   — Elasticsearch document routing (Murmur3 of UTF-16LE, floorMod(hash, number_of_shards); routing_partition_size offsets)
// meta::index_mode::IndexMode::of(&settings) (standard|time_series|logsdb|lookup|columnar|..), .default_source_mode(), .is_logs()
// meta::index_sort::sort_defaults(&settings) -> SortDefaults — the index sort a shard must apply (fields, orders, modes, missing)
// api::indices::data_stream_index_settings(md, ds_name, &mut settings, &[mappings], now_ms) -> EsResult<()> — TSDB/logsdb provider
//   settings (time bounds, dimensions, host.name sort) for a new backing index
// mapping::dynamic::dynamic_update(&Mapping, doc: &json::Map, &MappingCtx) -> Result<Option<Mapping>, DynamicError> // pure, for indexing
// json::{Value, Map, parse, parse_object} — ordered JSON with Jackson's number model and error texts.
// query — the Query DSL as typed builders (every Elasticsearch 9.5 query), parsed token by token like
//   Elasticsearch (exception types, texts, [line:col]) and printed like toXContent:
//   query::parse_query(&Value, raw_body: Option<&[u8]>, base_path: &[Seg]) -> EsResult<(Query, warnings)>;
//   query::parse_stored(&Value) -> EsResult<Query> (alias filters); Query::{name, to_value, to_value_in(dialect),
//   to_pretty_string, common() -> &Common { boost, name }}; one struct per query in query::{compound, term_level,
//   full_text, joining, geo, span, specialized} (e.g. term_level::Term { field, value, case_insensitive, common });
//   query::script::Script; query::xparser::XParser (XContentParser semantics over a Value, ObjectParser emulation).
//   Mapping-aware conversion to engine queries (toQuery) is the search phase's job.
// meta::routing::Preference::parse(pref, opensearch) -> { shards: Option<Vec<u32>>, only_nodes } — search `preference`
// document::parse_document(&Mapping, &MappingCtx, doc: &Value, raw_body: Option<&[u8]>, id: Option<&str>)
//   -> EsResult<ParsedDocument { fields: Vec<DocField { name: Arc<str> /* shared with the mapping */, value: FieldValue }>, nested: Vec<NestedDoc { path, fields }>,
//      ignored: Vec<String> /* _ignored */, doc_count, dynamic_update: Option<Mapping> }> — DocumentParser with
//   Elasticsearch's coercion and errors (document_parsing_exception texts, [line:col], previews, strict dynamic);
//   FieldValue is engine-neutral (Text/Keyword/Long/UnsignedLong/Double/Float/Boolean/Date/Ip/GeoPoint/Shape/Range/
//   DenseVector/ByteVector/Features/Feature/Flattened/Completion/TokenCount/...); analyzers and normalizers apply when indexing.
// search_source::SearchSource::parse(&Value, raw_body) -> EsResult<SearchSource> — the _search body with Elasticsearch's
//   errors: query/post_filter (query::Query), from/size/timeout/terminate_after/min_score/flags, track_total_hits,
//   _source (FetchSource), stored_fields, docvalue_fields/fields (FieldAndFormat), script_fields, sort (Sort::{Score,
//   Field, Script, GeoDistance(raw)}), search_after, indices_boost, rescore (query rescorer), stats, collapse, slice,
//   pit; aggs/highlight/suggest/knn/retriever/rank/ext/runtime_mappings kept as maps for their parsers.
// EsError { status, kind, reason, caused_by, .. } with to_body(trace); constructors carry Elasticsearch's texts.
// time::DateFormatter::{new(pattern), with_locale(DateLocale), parse(text, zone, round_up) -> Result<Instant, String>,
//   parse_detailed(..) -> Result<Instant, DateError> /* DateError::to_es(): Elasticsearch's exception chain */, format(..)};
//   time::{parse_date_math(..) -> Result<Instant, DateMathError /* to_es() */>, to_epoch_nanos(Instant), java_instant_string_nanos}
// mapping::FieldMapper::{date_formatter(ctx) -> EsResult<Arc<DateFormatter>> /* compiled once */, full_name(path) -> Arc<str>};
//   mapping::date_locale(Option<&Value>); stack::maybe_grow(f) — run recursion over untrusted nesting without overflowing

// search-server
#[async_trait] pub trait Catalog { async fn lookup(&self, cluster: &str) -> Option<ClusterSpec>; }    // StaticCatalog
#[async_trait] pub trait StoreProvider { async fn meta_store(&self, cluster) -> Result<Arc<dyn MetaStore>>;
                                         async fn branch(&self, parent, child) -> Result<()>; async fn delete(&self, cluster) -> Result<()>;
                                         async fn release(&self, cluster) /* on unload; default no-op */ }
// MemoryStores (in memory); ObjectStores::new(search_store::Store) (metadata as search-store cluster states: read_cluster/write_cluster,
// branch, delete_cluster, release_cluster on unload)
pub struct ClusterSpec { pub name, pub dialect, pub credentials: Vec<Credential { user, api_key_id, secret }>, pub idle_unload: Option<Duration>,
                         pub max_in_flight: usize, pub max_body_bytes: Option<usize>, pub destructive_requires_name: bool }
pub struct NodeConfig { pub node_name, pub domain: Option<String>, pub idle_unload: Option<Duration> /* 300 s */, pub max_body_bytes /* 100 MiB */,
                        pub default_cluster: Option<String> /* dev mode: no auth */,
                        pub node_settings: Vec<(String, String)>, pub test_features: bool /* copied into ClusterConfig */,
                        pub host_metrics: bool /* copied into ClusterConfig; search-dev sets it */ }
#[async_trait] pub trait LifecycleHooks { async fn loaded(&self, &Arc<Cluster>); async fn unloading(&self, &Arc<Cluster>); }
impl Node { fn new(catalog, stores, cfg) -> Arc<Node>; fn with_hooks(..); async fn open(spec) -> Result<Arc<Loaded>>; fn idle(older_than) -> Vec<String>;
            async fn unload(name) -> bool; async fn branch(parent, child); async fn admit(&EsRequest, sni) -> Result<Lease, Rejected>;
            async fn handle(EsRequest) -> EsResponse; fn loaded() -> Vec<String>; fn update_spec(ClusterSpec) /* credential rotation */ }
pub async fn serve(listener, Arc<Node>, ListenOptions { tls: Option<TlsAcceptor>, proxy_protocol }) -> Result<()>; // also spawn(addr, ..)
// net::{tls_config, tls_acceptor, read_proxy_header}.  Binary: search-dev --listen 127.0.0.1:59200 [--cluster] [--dialect] [--data-dir dir]
//   (--yaml-test-cluster also sets path.repo to <tmp>/roll-search-repo, so `fs` snapshot repositories can be registered)
//   [--tls-cert --tls-key] [-E key=value]... [--test-features] [--yaml-test-cluster]
//   (--data-dir: metadata through search-store in a local directory instead of memory; --yaml-test-cluster = the settings
//   Elasticsearch's YAML test cluster runs with: node.attr.testattr=test, node.processors=2, test cluster features)
```

The idle reaper task exists only while a cluster is loaded. `search-es` cluster state JSON
(`ClusterState::to_json`) is what `MetaStore::save` receives; `ObjectStores` stores it with
`search-store`'s `write_cluster`/`read_cluster`.

## roll API (`/v1/search`, served by `search-control` on the rolld API port)

| method | path                                             | |
| ------ | ------------------------------------------------ | - |
| GET    | `/v1/search/clusters`                            | list |
| POST   | `/v1/search/clusters`                            | create `{name, flavor?, parent?, at?}` → includes the password once |
| GET    | `/v1/search/clusters/{id}`                       | show: state (loaded/asleep), indices, docs, bytes, url |
| PATCH  | `/v1/search/clusters/{id}`                       | settings: idle timeout, flavor, limits |
| DELETE | `/v1/search/clusters/{id}`                       | |
| POST   | `/v1/search/clusters/{id}/branches`              | branch `{name, at?}` |
| POST   | `/v1/search/clusters/{id}/password`              | rotate |
| POST   | `/v1/search/clusters/{id}/unload`                | put it to sleep now |
| GET    | `/v1/search/clusters/{id}/indices`               | indices with docs, size, health |
| POST   | `/v1/search/clusters/{id}/request`               | run one REST request (dashboard console) |
| GET    | `/v1/search/clusters/{id}/usage`                 | metering, cost vs Elastic Cloud / Amazon OpenSearch Service |

Errors follow the other control planes: `{"error": "..."}` with 400/404/409/500.

## `search-yaml`

Runs Elasticsearch's YAML REST suites (and OpenSearch's) against any URL, with the Java runner's
semantics. Library entry point and the pieces other crates may reuse:

```rust
pub struct Config { pub url: Option<String>, pub more_urls: Vec<String>, pub suites: Vec<SuiteRoot /* label=dir */>, pub api_spec: PathBuf,
                    pub filter: Option<Regex>, pub only_ids: Option<HashSet<String>>, pub skip_list: Vec<SkipEntry>, pub jobs: usize,
                    pub flavor: Option<Flavor>, pub claimed_features: HashSet<String>, pub wipe: Option<WipeMode>,
                    pub cluster_hook: Option<ClusterHook>, pub spawn: Option<SpawnCommand>, pub fresh_per_test: bool,
                    pub headers: Vec<(String, String)>, … }
pub struct SpawnCommand { pub command: String /* sh -c, {port} {name} */, pub url: String /* http://127.0.0.1:{port} */, pub ready_timeout: Duration }
pub async fn run(cfg: Config) -> anyhow::Result<Report>;   // Report: totals, per-test results, write_json, summary, compare(baseline)
pub mod yaml  { pub fn load_documents(&str) -> Result<Vec<Node>, YamlError>; pub fn parse_value(&str) -> Result<Value, YamlError>; }
pub mod value { pub enum Value { Null, Bool, Int(i128), Float(f64), Str, List, Map(IndexMap) } }  // Java equality + toString
pub mod xcontent { pub enum Format { Json, Yaml, Cbor, Smile } /* encode(&Value, &mut Vec<u8>), decode(&[u8]) */
                   pub struct MediaType { format, vendor }; pub fn decode_cbor(&[u8]); pub fn decode_smile(&[u8]); }
```

CLI: `search-yaml run --url http[s]://… [--ca-cert pem | --insecure] --suite [label=]<dir> [--api-spec <dir>] [--filter re] [--skip-list f]
[--report out.json] [--baseline f] [--write-baseline f] [--jobs N --cluster-hook URL | --jobs N --spawn CMD [--spawn-url T] | --url u2 …] [--flavor
elasticsearch|opensearch] [--features-file f] [--wipe full|light|none] [--user u --password p]
[--ids f] [--rerun report.json] [--junit out.xml] [--content-type json|yaml|cbor|smile|vnd-<t>|random
[--seed N]] [--method-strategy natural|first|random] [--randomize]` (`--ids`: only the listed ids,
e.g. a baseline; `--rerun`: only the failures of an earlier JSON report; `--junit`: JUnit XML for
CI; `--content-type random`: like Elasticsearch's runner, every request body in a random
`XContentType` (any of 8 for one body, JSON/Smile for bulk-style bodies); the target answers in
that type and the runner decodes YAML, CBOR and Smile responses; `--method-strategy random`: the
Java runner's random best path (deprecated ones included), random method and one-in-ten
`GET ?source=` for small JSON/YAML bodies; `--randomize` = both, which is how Elasticsearch's CI
runs the suites; every choice comes from `--seed` and the test id, printed in the summary);
`search-yaml list|check --suite <dir>`; `search-yaml dump <file.yml>…` (parsed documents as JSON lines).

- **Test ids** are `[<label>:]<dir>/<file without .yml>/<test name>` (the Java runner's test path).
  `crates/search-yaml/baseline/es-9.5.5.txt` lists the ids that pass on a real single-node
  Elasticsearch 9.5.5: roll's target (`free:` = rest-resources `test/free`, security off, the
  settings of Elasticsearch's YAML test clusters; `clients:` = elasticsearch-clients-tests,
  security and HTTPS on). `es-9.5.5.skips.txt` lists environment skips with reasons. Compare a roll run
  with `--baseline crates/search-yaml/baseline/es-9.5.5.txt` (regressions fail the run).
  `opensearch-3.9.0.txt` (+ `.skips.txt`) is the same for OpenSearch's suite on OpenSearch 3.9.0:
  the opensearch dialect's target. `scripts/search-yaml-suites.sh es-baseline|os-baseline`
  regenerates them.
- **Running roll**: `scripts/search-yaml-suites.sh roll [runner args]` starts `ROLL_JOBS` (4)
  in-memory `search-dev --yaml-test-cluster` processes (ports from `ROLL_PORT`, 19310), runs
  `free` + `clients` across them (one `--url` per process) against `es-9.5.5.txt` and writes
  `target/search-yaml/roll.json`; `roll-os` does the same for the OpenSearch suite and dialect.
  `ROLL_ISOLATED=1` instead starts a fresh `search-dev` per file (`--spawn`), so state one file
  leaves behind can't fail later files.
  `SEARCH_DEV=<binary>` uses an already-built search-dev. The summary has a `target` column
  (baseline tests per area) and ranks **failure causes**: every failure carries `cause`, a key like
  `index → 501 unsupported_operation_exception: [index] is not supported by roll search yet` or
  `search: match mismatch` (digits folded), so the most common missing pieces come first.
- **Two dialects of the format.** Elasticsearch's files follow the Java runner exactly. Files with
  a top-level `requires: {stack, serverless}` document (elasticsearch-clients-tests) follow the
  official clients' runners instead: the spec's preferred `Accept`/`Content-Type` are sent,
  parameters missing from the rest-api-spec go to the query string, unexpected warnings only
  fail when the step lists `warnings`, `catch: <error_type>` checks `error.type`, and expected
  lists are unstashed. `--flavor opensearch` uses OpenSearch's runner rules (`OpenSearch-` warning
  agent, `[types removal]` warnings tolerated, no `gone` catch).
- Teardown failures are reported (`teardown_failure` in the JSON) but don't fail a test, as in
  the Java runner; a failing cluster wipe does.
- **Cluster features**: read from `GET /_cluster/state?filter_path=nodes_features` (roll should
  publish the features it implements there, like Elasticsearch with test features enabled), plus
  `--features-file` (one per line) and synthetic `gte_vX.Y.Z` from the reported version.
- **Capabilities**: `GET /_capabilities?method=&path=&parameters=&capabilities=&local_only=true`
  answering `{"supported": true|false|null}`; a 4xx means "unknown" (requires fail, skips apply).
  A 5xx, a transport error or a non-boolean `supported` fails the test (as the Java runner
  rethrows it) and is not cached, so the next test asks again.
- **Isolated clusters** (`--cluster-hook URL`, enables `--jobs N`): the runner `POST`s
  `{"name": "...", "flavor"?: "..."}` to the URL and expects JSON with `url` (or `endpoint`),
  optional `username`/`password` (Basic), `api_key`, `headers`, and `delete_url` or `id` (then
  `DELETE <URL>/<id>` when done; a relative `delete_url` goes to the hook's host); the fields may be
  top-level or under `cluster`. `--cluster-hook-header`s are sent on both the POST and the DELETE. `search-control`'s create-cluster endpoint should answer in that
  shape so `search-yaml run --cluster-hook http://127.0.0.1:7070/v1/search/clusters --jobs 8` works.
- **Spawned clusters** (`--spawn CMD`, enables `--jobs N`): for one-cluster-per-process servers.
  Per file (or test, `--fresh-per-test`) the runner picks a free loopback port, runs `sh -c CMD`
  with `{port}` and `{name}` substituted in its own process group, waits until `GET /` answers
  (`--spawn-timeout`, 60 s), runs the file against `--spawn-url` (`http://127.0.0.1:{port}`), then
  kills the group. A process that exits during startup is retried on another port; its output
  (a per-worker log in the temp dir) is part of the error.
- Every request carries `error_trace=true` (as the Java runner does) and every body is sent as
  `<media type>; charset=UTF-8`; every successful response must carry `X-Elastic-Product:
  Elasticsearch` (elasticsearch dialect), and unexpected `Warning` headers fail a test. Warning
  headers must match `HeaderWarning.WARNING_HEADER_PATTERN` (`299 Elasticsearch-<x.y.z>[-…]-<7 or
  40 hex | unknown> "…"`), otherwise they fail as unmatched.
- Responses and test files are parsed strictly, like Elasticsearch's XContent parsers: a repeated
  field fails (`Duplicate field 'f'`), a scalar JSON root reads as `{}`, and string bodies in a
  `do` must be JSON (comments allowed). A request is never resent once it may have reached the
  server: a connection that drops before the response fails the test.

## Tests

- Unit tests next to the code. Integration tests in `crates/<crate>/tests/`, in-process on
  `127.0.0.1:0`. Copy the `Capped` 1 GiB allocator guard from `crates/stream-broker/tests/common`.
- Tests needing Docker (real Elasticsearch, client libraries, MinIO) run only with `ROLL_E2E=1`.
- Object storage in tests uses S3 when `ROLL_E2E_S3_BUCKET` is set, a temp dir otherwise.
- Building on the shared dev machine: run cargo with
  `CARGO_TARGET_DIR=<repo>/target/agents/shared CARGO_BUILD_JOBS=6` and `-p <crate>`, so the
  shared target dir's lock serializes builds (RAM is limited).
- A real Elasticsearch for fixtures and differential tests:
  `docker run --rm -d -m 1536m -e discovery.type=single-node -e xpack.security.enabled=false
  -e ES_JAVA_OPTS="-Xms768m -Xmx768m" -p 127.0.0.1:<port>:9200 docker.elastic.co/elasticsearch/elasticsearch:9.5.5`.
  Expected outputs captured from it are committed as fixtures (facts about behaviour, not
  Elastic's code). Elasticsearch source past 7.10 isn't Apache-licensed: read it to learn
  behaviour, never copy it.
