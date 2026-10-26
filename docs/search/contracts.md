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
`--search-tls-cert/-key`, `--search-tls-self-signed` (a boot-time certificate; in a fleet the
heartbeat publishes its pin), `--search-domain`, `--search-idle-unload` (300 s),
`--search-cache-bytes`, `--search-sync-objects`, `--search-proxy-protocol`,
`--search-history` (point-in-time window, 86400 s), `--search-gc-interval` (3600 s),
`--search-test-features`, `--no-search`;
`rolld --search-only` runs only search and its API (over TLS with the REST certificate when the
REST listener serves TLS; the record then sets `Beat.api_tls`). On SIGTERM its whole stop takes at
most `--search-shutdown-timeout` (90 s; `Search::shutdown() -> Instant` returns the deadline for the
last connections, `search::stop_phases(total) -> (drain, tail)`, tail = a quarter, at most 15 s).
See [README.md](README.md).

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
- `search-es` uses `reqwest` (workspace dependency, no TLS features) for reindex from remote clusters
  (`api::remote`), only for hosts `reindex.remote.whitelist` allows.
- `search-es` uses `libc` (only `pthread_getattr_np` / `pthread_attr_getstack` in `search_es::stack`):
  the current thread's stack bounds, so deeply nested requests move to a fresh stack before they
  could overflow a tokio worker's instead of aborting the node.
- `search-server` uses `libc` (only `prctl(PR_SET_THP_DISABLE)`: transparent huge pages inflate an idle
  node's RSS about threefold; and `getrusage(RUSAGE_SELF)`: the purge governor's process CPU
  reading; see `search_server::process`) and `tokio-postgres-rustls` +
  `rustls-native-certs` (TLS to the Postgres lease store, as `mq-cluster` does).
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
    pub fn try_analyze_field(&self, field: &str, values: &[&str], sink: …) -> Result<(), AnalysisError>;
        // both: Lucene's IndexingChain position checks (Java int positions; a negative or huge
        // position_increment_gap fails with "position overflowed Integer.MAX_VALUE (…) for field 'f'"
        // or "position N is too large …" instead of emitting non-monotonic positions); the error
        // names `field` (try_analyze_values: field '')
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
come from its own engine (`jregex.rs`). No pattern can crash the process: Java patterns nested
deeper than 1000 groups/classes fail with Java's "Stack overflow during pattern compilation"
`pattern_syntax_exception`, Lucene (`simple_pattern*`) ones with an `illegal_argument_exception`
past 1000 groups or past 1M automaton states plus transitions (`a{100000000}`); Elasticsearch
itself dies on those. `mapping` rules are Java strings matched by UTF-16 units (a lone-surrogate
key maps half a character; an unpaired surrogate in the output becomes U+FFFD). `classic` and
`html_strip` stay linear on inputs whose scans never end (`a-a-a-…`, `<?<?…`), where Lucene is
quadratic. Checked against ES 9.5.5 `_analyze` output in
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
    // `field | search_codec::UNINVERTED` (bit 31): Sorted doc values rebuilt from the field's
    // terms on first use (each doc's smallest term), cached for the reader's life; `_id`
    // fielddata (search-es `search::id_fielddata::FIELD`). None for fields without terms.
    pub fn uninverted_bytes(&self, field: u32) -> usize;  // memory of the built UNINVERTED values (0 before first use)
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
    fn sorted_lookup(&self) -> SortedLookup { fn seek_exact_hashed(&mut self, term, hash) -> Option<TermState> }
        /* strictly increasing terms (TermInSet): keeps the block and scan position; 45k-term set 3.5x faster */;
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
//       values_range(first_idx, out: &mut [i64]) /* sequential decode, ~1 ns/value: scans over every doc */,
//       values() -> &NumericValues /* one per doc of docs(), for get_range / gather */}
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
//         raw(doc) -> Result<&[u8]>, holds(doc) -> bool /* doc is in the decompressed chunk: reading it decompresses nothing */}
//         (doc >= max_doc: InvalidArgument; stored::decode_doc(bytes, visitor) decodes raw bytes)
//   trait StoredVisitor { fn needs_field(&mut self, field) -> VisitStatus /* Yes|No|Stop */; fn value(&mut self, field, StoredValue<'_>) }
// Vectors: VectorsReader::{len(), docs(), ord(doc), doc(ord), vector(ord) -> &[u8], vector_f32(ord, &mut Vec<f32>),
//     prepare(VectorQuery::{Float(&[f32]) | Byte(&[i8]) | Bits(&[u8])}) -> Result<PreparedQuery> /* InvalidArgument with ES's
//         createKnnQuery texts: float input on byte/bit fields must be whole and in [-128, 127] (checked first, as
//         `element_type [byte]`), then "The query vector has a different number of dimensions [q] than the document
//         vectors [d]." (bits for bit fields), then float NaN/infinite values, dot_product unit length (±1e-3 on the
//         squared norm), cosine non-zero (byte fields: cosine only); value errors end with " Preview of invalid
//         vector: [first five, ...]" */,
//     score(&q, ord) -> f32 /* Lucene VectorSimilarityFunction score; bit = (bits - hamming) / bits */, score_doc(&q, doc),
//     search(&q, k, num_candidates, accept: Option<&dyn Fn(doc)->bool>, visit_limit) -> KnnResult { hits: Vec<KnnHit{doc,score}>, visited, complete },
//     exact_search(&q, k, accept) -> KnnResult,
//     // complete == false: fall back to exact_search (Lucene's AbstractKnnVectorQuery rule): visit_limit hit, or fewer
//     // hits than k (without accept: than min(k, len())) although more may exist
//     search_filtered(&q, k, num_candidates, accept: &dyn Fn(doc)->bool, accepted: u32 /* filter cardinality */, visit_limit) -> KnnResult}
//     // accepted <= k: exact search; fewer than k hits (e.g. an overestimated `accepted`): complete == false
//     // filters passing < 60% of the vectors use Lucene's filtered HNSW search (ACORN-1: only accepted vectors are scored,
//     // rejected neighbors are bridges; 1% filter: ~50x fewer distance computations than search() with accept); else search().
//   Graphs: concurrent builds link each node bottom-up (no quality loss vs 1 thread); every node is reachable from the entry
//   point (Lucene's connectComponents), also with duplicate vectors, and level 0 is strongly connected (every node also reaches
//   the entry: duplicates and coarse quantization otherwise strand regions), so an unbounded beam finds every vector. The
//   filtered search keeps walking rejected nodes while it holds fewer than num_candidates results.
//   Quantized fields (int8 / packed int4 / bbq) steer the graph search with quantized scores and return exact float scores (the
//   beam is rescored), so `_score` equals ES's `flat`/rescored value, not its approximate int8/int4/bbq score (deliberate:
//   ES's quantized scores depend on per-segment quantiles, so they differ with segment layout even within ES).
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
AlwaysMatch; Prefix::new(bytes); Dfa::levenshtein(term, max_edits, prefix_len /* code points */, transpositions) (Lucene FuzzyQuery automaton;
    an empty term matches only "" whatever max_edits, as FuzzyTermsEnum drops maxEdits to 0 for it);
Dfa::wildcard(pattern, case_insensitive, work_limit) -> Result<Dfa, TooComplex>; Dfa::prefix_case_insensitive(prefix, work_limit);
Dfa::term_case_insensitive(term, work_limit) /* `term` query with case_insensitive */;
    // wildcard/prefix/term case_insensitive fold ASCII letters only (ES AutomatonQueries); a wildcard `\x` escape is a
    // literal that still folds (`\a` matches `A`), escaped `*`/`?` are literal
automaton::regexp::compile(pattern, flags /* regexp::ALL = ES default */, case_insensitive, work_limit /* dfa::DEFAULT_WORK_LIMIT = 10000 */)
    -> Result<Dfa, RegexpError::{Syntax(lucene IllegalArgumentException text), TooComplex(TooComplex /* Display = Lucene's message */)}>;
    // regexp case_insensitive = Lucene 10.5 CaseFolding (simple mappings + specials: k~K, ß~ẞ, σ~ς~Σ; ranges not folded).
    // ES runs regexp::collapse_consecutive_quantifiers(pattern) (`a+*` -> `a*`) before compiling regexp queries on
    // string fields: call it first so error positions match. Lucene quirk kept: `X*` of an empty language (`#*`)
    // matches nothing. TooComplex thresholds track Lucene's effort accounting (checked around the limit); like Lucene, an
    // automaton that is already deterministic costs no effort (`a{100000}` compiles). Construction is budgeted:
    // past regexp::MAX_STATES (400 000) code point NFA states or intersection product states, compile fails fast with
    // TooComplex (`a{0,2147483647}`, `((a{1000}){1000}){1000}`; ES trips a memory circuit breaker instead).
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
    // reopen on a commit whose searcher showed only part of it (`FlushOutcome::published`, `PublishedView { hidden:
    // Vec<u32> /* segment positions written since the last refresh */, deleted: Vec<(u32, Vec<u32>)> /* docs deleted
    // since */, merged: Vec<(u32, Vec<u32>)> /* segments merged since the refresh from shown ones: the docs not shown;
    // every other doc is shown, deleted or not (traced through merges of merges, kept until the next publish, merges up
    // to 2M docs) */ }`, at most `MAX_VIEW_DELETES` docs): searches see what they saw, realtime reads everything, the next
    // refresh publishes the rest (`has_pending_changes()` meanwhile). `published: None`: the searcher showed the whole
    // commit, or something a view can't express (a shown doc a merge since dropped).
    pub fn open_published(cfg, schema, segments, published: Option<PublishedView>) -> Result<Shard>;
    pub fn index(&self, op: IndexOp) -> Result<OpResult>;   // IndexOp::new(doc) + version/version_type/if_seq_no/if_primary_term/op_type/origin
    pub fn delete(&self, op: DeleteOp) -> Result<OpResult>; // OpResult::{Indexed, Deleted, Conflict(VersionConflict), Stale}
    pub fn get(&self, id: &[u8], realtime: bool) -> Result<Option<StoredDoc>>;
    pub fn realtime_searcher(&self, id: &[u8]) -> Result<Searcher>;  // ES's internal reader (unpublished segments too), refreshing the buffers first when `id` is buffered
    pub fn note_realtime_get(&self, id: &[u8]) -> Result<()>;  // a realtime GET of a buffered `id`: ES's internal refresh, so later realtime_searcher statistics count it
    pub fn refresh(&self) -> Result<bool>;  pub fn flush(&self) -> Result<FlushOutcome>;  // segments (zero-copy bytes) + live docs + checkpoint
    pub fn maybe_merge(&self) -> Result<usize>;  pub fn force_merge(&self, max_num_segments: Option<usize>, only_expunge_deletes: bool) -> Result<()>;
    pub fn searcher(&self) -> Searcher;      // point in time; stats(), segments(), update_schema(), mark_seq_no_processed(), close()
    pub fn leaf_sort(&self) -> Option<u32>;  pub fn retention_lease(&self) -> (i64, i64, Duration);  // to persist with a commit
    pub fn seed_retention_lease(&self, from: i64, next: i64, age: Duration);  // reopen: keep retaining (never moves it back)
    pub fn set_leaf_sort(&self, field: Option<u32>);  // searchers list segments by this 1-D point field's max, descending
                                                      // (Elasticsearch's @timestamp leaf sorter); republishes the current
                                                      // searcher's segments reordered (nothing unrefreshed becomes visible)
    // durability hooks (search-server): delete tombstones of a commit (`up_to`: its checkpoint) and back at open,
    // `index.gc_deletes` (dynamic; expired tombstones no longer count for primaries' version checks, replays still see
    // them), seq_no gap filling after a replay, and the soft-deletes retention lease natural merges honour
    pub fn tombstones(&self, up_to: i64) -> Vec<(Bytes, i64 /* seq_no */, i64 /* version */, Duration /* age */)>;
    pub fn seed_tombstones(&self, impl IntoIterator<Item = (Bytes, i64, i64, Duration)>);  pub fn set_gc_deletes(&self, Duration);
    pub fn fill_seq_no_gaps(&self) -> u64;  pub fn retained_from(&self) -> i64;
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
// SearchControl { deadline, cancelled, ext: Option<Arc<dyn Any + Send + Sync>> /* request state of the embedder, carried to slice threads */ }
// search::{set_ambient_control(Option<SearchControl>) -> AmbientGuard, ambient_control()}: the thread's control; search()/count()/search_concurrent()
//   with a no-op control use it, search_partitions hands it to slice threads. search::{note_interrupted(Error), take_interrupted()}: a hook
//   (script poll) that stopped because the deadline passed records it, and the searcher reports that error (TimeExceeded keeps partial results).
// query::IntervalsSource::scripted(src, Arc<dyn IntervalScript>) /* FILTER(<script>) */; trait IntervalScript { name(), accept(start, end, gaps) -> Result<bool> }
// query::FilterScript::runtime_field() -> bool (default false): AbstractScriptFieldQuery description/explanation
    fn count_if_cheap(&QueryRef) -> Result<Option<u64>>; fn segment_counts_if_cheap(&QueryRef) -> Result<Option<Vec<u64>>> /* per segment,
      search order */; fn explain(&QueryRef, global_doc) -> Result<Explanation>; fn rewrite(&QueryRef);
    fn stored(global_doc); fn term_vectors(global_doc, fields, term_stats, field_stats) -> Result<Vec<FieldTermVector>>;
    fn collection_stats(field); fn term_stats(field, term); fn with_limits(SearchLimits /* max_clause_count 4096, parallelism */);
    fn with_global_stats(Option<Arc<dyn StatsSource>>) -> Searcher /* dfs_query_then_fetch: term / field statistics replacing
      the view's own (TermStates::build, collection_stats, term_stats consult it first) */; fn global_stats(&self) -> Option<&Arc<dyn StatsSource>>;
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
cannot exceed [..]". Natural merges follow Elasticsearch's soft-deletes retention (Lucene's `SoftDeletesRetentionMergePolicy`): deleted
docs of operations with seq no `>= retained_from()` don't count as deletes for the merge policy, so small segments with
fresh deletes aren't rewritten after a refresh and a fully deleted segment stays; the lease starts at the opening
checkpoint + 1 (0 for a new shard) and moves to the local checkpoint once per `shard::RETENTION_SYNC` (30 s,
`index.soft_deletes.retention_lease.sync_interval`), checked lazily (no timer). Forced merges still expunge everything.
Unlike Elasticsearch, a delete adds no tombstone document to a segment (`docs.deleted` counts only deleted documents).
A flush commit is a consistent cut (segments and live docs captured with no write in flight):
it never holds the delete of an update without its new doc. `force_merge(Some(n), _)` merges only the segments present
at the call and their merge results (Lucene's `segmentsToMerge`), so it returns under steady indexing.
`search::pool::execute_held(job, hold)`: `hold` drops once the worker that ran `job` is parked again.
`search::pool::cores()`: the process's usable CPUs, looked up once. Use it instead of
`std::thread::available_parallelism()` on any per-request path (the std call re-reads cgroup files each time).

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
`ScoreScript`; their bulk scorers score a block of the inner query's matches at a time, and
`LeafScoreScript::score_block(docs, subs, out) -> Result<(), (usize, Error)>` lets a script
evaluate a block — the default calls `score` per doc), `KnnQuery` (approximate / exact, filter, min score; the `knn` query, rewrites to
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
nested vectors (best child per parent, `k` parents; wrap in `ToParentQuery` with `Max`); `KnnQuery.per_segment: bool`
keeps each segment's best `k` instead of the best `k` overall (OpenSearch's native k-NN engines). `Searcher::global_ordinals(field) ->
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
`TotalHitCountCollector`, `MultiCollector`, `TerminateAfter` (`.counting(&searcher, min_score, post_filter)`: only the
hits that pass them count, as `QueryPhaseCollector` does), `MinScore`, `PostFilter::new(&searcher, &filter, inner)`
(`post_filter`), `CollapseCollector`
(groups by keyword / numeric doc values; hits carry the group value for inner hits),
`QueryRescorer` (Elasticsearch's window rescoring and explanations), managers for concurrent
search (`TopScoreManager` — its slices share the hit count and best score bar, Lucene's `HitsThresholdChecker` /
`MaxScoreAccumulator` —, `TopFieldManager`, `CountManager`; `CollectorManager::new_collectors(n)` creates one search's
collectors). Highlighting:
`highlight::highlight(query, values, analyzer, field, &HighlightOptions)` — unified (default),
plain, fvh (Lucene's FastVectorHighlighter pipeline; `highlight_in(searcher, ..)` expands multi-term queries against
the index as Elasticsearch's fvh does). Span queries report their tree through `QueryVisitor::span(&SpanNode)` (the
default forwards its terms to `term`); the highlighters match it over the doc's tokens (`highlight::spans`), and
`HighlightOptions::offsets_indexed` (postings or term vectors with offsets) picks Lucene's offset source rules.
Explanations match Elasticsearch's text, including the
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
    //                run() sets it for a MatchAllQuery */, max_buckets: Option<usize> /* search.max_buckets,
    //                None = 65536: composite's size is checked on the shard */,
    //                profiler: Option<Arc<search_aggs::profile::Profiler>> /* profile: true — after finish(),
    //                Profiler::to_json() is the shard's `profile.shards[].aggregations` array (type = Elasticsearch
    //                aggregator class, description, time_in_nanos, breakdown with *_count, debug, children) */,
    //                query: Option<QueryRef> /* the shard's query; read only by profiled opensearch-dialect requests
    //                (search_aggs::filter_rewrite replays OpenSearch's filter rewrite optimization for the profile's
    //                optimized_segments / leaf_visited / inner_visited; cardinality's dynamic pruning counts) */,
    //                filter_by_filter: bool /* search.aggs.rewrite_to_filter_by_filter (search-es passes it, default
    //                true for the elasticsearch dialect): top-level `filters` and keyword `terms` (≤ 1000 global
    //                ordinals, AggField.indexed) run filter by filter under match_all (FilterByFilterAggregator,
    //                StringTermsAggregatorFromFilters in profiles); default false */ }
    //                — construct with ..Default::default()
    fn collector(&self, &dyn FieldLookup, &search_index::Searcher, &ShardOptions { budget }) -> Result<AggCollector>;
    fn run(&self, &dyn FieldLookup, &Searcher, query: &QueryRef, &ShardOptions) -> Result<Aggs>;  // collector + search + finish
}
impl search_index::Collector for AggCollector;  impl AggCollector { fn finish(self) -> Result<Aggs> /* = ShardAggs */ }
pub fn reduce(parts: Vec<Aggs>, req: &AggRequest, opts: &ReduceOptions) -> Result<Aggs>;
pub struct ReduceOptions { final_reduce, max_buckets /* search.max_buckets */, has_batched_result,
                           scripts: Option<Arc<dyn ScriptHost>> /* reduce-side scripts */,
                           budget: Option<Arc<MemoryBudget>> /* every reduce, partial ones included, charges 1.5× its inputs'
                               estimated size as "<reduce_aggs>" (QueryPhaseResultConsumer) and releases it on return; buckets
                               are capped (max_buckets) only in the final reduce, as in Elasticsearch */ }
impl Aggs { fn to_json(&self, typed_keys: bool) -> Json; fn get(&self, name) -> Option<&Agg> }
pub trait FieldLookup {   // per shard; every method but `field` has a default
    fn field(&self, name) -> Result<Option<AggField /* type, SourceType (dv kind), number, format, nested path */>>;
    fn index_name(&self) -> &str; fn query(&self, &Json) -> Result<QueryRef>; fn nested(&self, path) -> NestedLookup;
    fn root_filter(&self) -> QueryRef; fn global_query(&self) -> QueryRef /* superset of significant_*, `global` */;
    fn join(&self, child_type: Option<&str>, parent_type: Option<&str>) -> Option<JoinRelation>;  // children / parent
    fn scripts(&self) -> Option<Arc<dyn ScriptHost>>; fn date_formatter(&self, pattern); fn decimal_format(&self, pattern);
    fn field_date_formatter(&self, field, pattern) /* a mapped date field's format in its `locale`; default date_formatter */;
    fn top_metrics_max_size(&self) -> u32 /* index.top_metrics_max_size, default 10 */;
    fn now_millis(&self) -> i64; fn doc_count_field(&self) -> Option<u32> /* _doc_count */;
    fn fetcher(&self) -> Option<Arc<dyn TopHitsFetcher>> /* top_hits fetch phase; TopHitsFetcher::rescore / rescores
      (default no-op): the search's rescorers over a score-sorted bucket's hits */; fn max_inner_result_window(&self) -> u32;
    fn hit_sort(&self, sort: &Json) -> Result<Vec<HitSort>>; fn text_terms(&self) -> Option<Arc<dyn TextTerms>> /* significant_text */;
    fn shard_id(&self) -> (u32, u32); fn index_uuid(&self) -> Option<&str> /* random_sampler's default shard seed = ShardId.hashCode() */;
    fn time_series(&self) -> Option<TimeSeriesInfo> /* time_series-mode index */;
}
pub struct TimeSeriesInfo { dimensions: Vec<AggField>, tsid_field: Option<u32> /* sorted `_tsid` dv */, timestamp_field: Option<u32> /* @timestamp */ }
// AggField.counter: a `time_series_metric: counter` field of a time_series index (rate → reset-aware counter rate;
// terms/date_histogram/missing/… refuse it, as in Elasticsearch).
// AggField.indexed: the doc-values terms are the inverted index's (an indexed keyword; filter-by-filter `terms`).
// Pipelines include `moving_percentiles` (PipelineKind::MovingPercentiles { path, window, shift }: reduces the
// window's Percentiles states; metrics::percentiles::bucket_percentiles resolves its buckets_path).
// `histogram` fields: one binary doc value per doc, histogram_field::encode_histogram(values, counts);
// `aggregate_metric_double`: sorted-numeric sub-fields per metric (sortable doubles, value_count as a long).
// MemoryBudget (search_aggs::memory): new(limit), limit(), set_limit(limit) /* dynamic breaker limits */, used(),
// tripped() /* refused reservations */, reserve(bytes, label) -> circuit_breaking_exception, force(bytes), release(bytes).
// search-es keeps one per cluster as the `request` breaker (Cluster.breakers) and passes it to collection and reduce.
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

Every aggregation of Elasticsearch 9.5 core, `aggregations` module, analytics, spatial, ml
(`categorize_text`, `frequent_item_sets`) and parent-join is implemented. `categorize_text` reads
`_source` through `TextTerms::source_strings(seg, doc, field) -> Result<Vec<String>>` (default: error) and
analyzes with the built-in standard categorization analyzer (`first_line_with_letters` + `ml_standard` +
date-word stop filter + limit 100: `search_aggs::bucket::categorize_text::{standard_tokens,
first_line_with_letters, MlStandardTokenizer}`, exported so the index's analysis can offer them too);
`categorization_filters` / `categorization_analyzer` go to
`FieldLookup::categorization_analyzer(&Json /* ES's analyzer definition */) -> Result<Arc<dyn
CategorizationAnalyzer>>` (default: error). Token weights need ML's part-of-speech dictionary, installed
process-wide with `categorize_text::set_part_of_speech_dictionary(PosDictionary::parse(text_in_ml_en_dict_format)?)`
(without it every token weighs 1 and categories can differ from Elasticsearch's).
`DateFormatter::try_format(millis, nanos, zone) -> Result<String, String /* DateTimeException message */>`
(default: `Ok(format(..))`) lets `stats`/`extended_stats` fail like Elasticsearch when a stat can't be
printed in the format (a year wider than the pattern allows: "Cannot format stat [sum] with format [...]");
`DocValueFormat::java_string()` is Java's `toString()` of the format. The built-in date formatter (used until
`FieldLookup::date_formatter` is overridden) prints Elasticsearch's named formats but only parses ISO dates
and date math.
`ParseOptions.java_regex_valid: Option<&dyn Fn(&str) -> bool>` validates `categorization_filters`
(default: a basic syntax check). `FieldLookup::field` returns `text` fields (no fielddata) with
`SourceType::None`; binding one as a values source raises Elasticsearch's "Fielddata is disabled on [f] in
[index]…" error. `exponential_histogram`
fields feed `sum`/`avg`/`min`/`max`/`value_count`/`percentiles`/`percentile_ranks`/`boxplot`/`histogram`/`range`
(`HistogramUnionState`, `search_aggs::exp_histogram`); shapes feed `geo_bounds`/`geo_centroid`/`geotile_grid`/
`geohash_grid`/`geohex_grid` and `cartesian_bounds`/`cartesian_centroid`. Results match Elasticsearch bit for bit, including reduce
orders (Java `HashMap` iteration, Lucene priority queues), sketches (TDigest, HdrHistogram, HLL++),
`random_sampler`'s sample for a given `seed` / `shard_seed`, `t_test` p-values (Commons Math's
`FastMath`/t-distribution, `search_aggs::commons_math`) and `geohex_grid` cells (Elasticsearch's H3
port with its jafama trigonometry, `search_aggs::h3`). `h3::string_to_h3` returns only valid cells
(`h3::is_valid`, `H3.h3IsValid`), so peer input never reaches the cell functions; `h3::string_to_h3_lenient`
also accepts indices whose fields are in range (the `geo_grid` processor reads `"0"`, as Elasticsearch does).

compat-es round 3 additions: `search_aggs::version_string::{encode, decode, decode_into, is_legal,
EncodeError, NUMERIC_MARKER, PRERELEASE_SEPARATOR, NO_PRERELEASE_SEPARATOR}` (x-pack's `VersionEncoder`:
byte order is semver precedence) and `DocValueFormat::Version` (picked for `version` keyword fields:
keys decode, `missing`/`include`/`after` encode); `HitSort::Computed { sort: Arc<dyn ComputedSort>, reverse }`
with the traits `field::ComputedSort` (`kind()` `number`|`string`|`version`, `segment(seg)`,
`render_bytes`, `needs_score`) and `field::SegmentComputedSort::value(doc, score) -> sort::SortVal`
(failures go through `search_index::search::defer_error`): `top_hits`/`top_metrics` keys for
`_script` and `_geo_distance` (`top_metrics` rejects non-number kinds with Elasticsearch's message);
`bucket::terms::SortVal` gained `Bytes(Vec<u8>)` and `Empty` (greatest) and is no longer `Copy`
(`cmp_sort_vals` takes references) and `TopMetrics::sort_value(key)` feeds it, so `terms` orders by
keyword/ip `top_metrics` values; `AggField { counts: Option<u32>, expand_counts: bool }` (a
`counted_keyword`'s vInt-array count doc values; `BytesReader` repeats each value by its count when
`expand_counts`), `TermsSpec.counted` and `bucket::terms::parse_counted` (`counted_terms`, parsed as
its own type and run as a byte-keyed `terms` without per-doc dedup), `values::{VIntIter,
write_vint_array}`; `multi_terms` reduce promotes a key position whose numeric type differs between
shards (`long`/`unsigned_long`/`double`) to doubles; `shape_field::ShapeValue::label_position()`.

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
    fn compile_expression(&self, src, ctx) -> Result<Arc<expression::Expression>, ScriptError>;  // lang: expression
    fn update_settings(&self, Settings); fn settings(&self) -> Settings; fn stats(&self) -> Stats;   // _nodes/stats script
}
pub enum Lang { Painless, Mustache, Expression }   // from_name("painless"|"mustache"|"expression"), name()
pub enum Compiled { Painless(Arc<Script>), Mustache(Arc<Template>), Expression(Arc<expression::Expression>) }
// expression (Lucene expressions, Elasticsearch's lang-expression): contexts expression::CONTEXTS (score, filter,
//   field, number_sort, aggs, terms_set, bucket_aggregation, aggregation_selector, double_values; others fail to
//   compile with illegal_argument_exception). Expression: Send + Sync; source(), context(), variables() (normalized,
//   first-use order), needs_score(), evaluate(&[f64]) (double_values), instance() -> expression::Instance:
//   set_params(&Json)/set_params_value (numeric params are constants; the bucket contexts' variables), set_var("_value"),
//   set_document(doc) (opt-in: executions on the same doc return the first one's value, as Lucene caches it — aggs
//   value scripts over multi-valued fields), execute(&mut dyn Host) -> Value (Double; Bool for filter /
//   aggregation_selector), execute_f64, execute_bool, link(&mut dyn Host) (link now, as Elasticsearch does per
//   segment before any document: fails searches over segments without matches too). Variables link through
//   Host::doc_values (Ok(false) = "does not exist in mappings"; a HostError, e.g. text fielddata, is the cause), so
//   compile errors come from compile_*, `link error`s from the first execute; both are script_exceptions with
//   lang "expression", script_stack [portion, pointer] and no position (queries wrap them in query_shard_exception
//   "failed to create query: …"). One Instance per segment; reads every field once per document.
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
                     // object size limit, stale subList views → concurrent_modification_exception
                     // with an empty (null) reason: Elasticsearch answers a plain 500); to_json()
                     // gives null for those; try_to_java_string(): the `_execute` painless_test
                     // result (String.valueOf), same stale-view failure
// write scripts: es::write::{ctx_value(CtxKind, source, metadata: &[(&str, Value)], now) -> Result<Value, String>,
//   ctx_of(&Value) -> Option<Rc<Ctx>>, CtxKind::{Update, Upsert, UpdateByQuery, Reindex, Ingest}}
//   Ctx: source() (new _source), metadata(), meta_get(key), op() (index|noop|delete|create, Elasticsearch's getOp rules)
// xcontent::{parse_source(json) (LinkedHashMap, document order), parse_source_unordered(json) (HashMap: upsert docs,
//   ingest, update_by_query, reindex), to_xcontent(&Value) -> Result<String, XContentError> (Jackson output, XContent
//   type rules; iterative, 1,000-level limit), check_ingest_value(&Value) (ensureNoSelfReferences with hint
//   "source document", then IngestDocument.deepCopy rules; iterative: cyclic or 300,000-deep values are
//   errors, never a stack overflow), ensure_no_self_references(&Value, hint: Option<&str>) (CollectionUtils)}
// value::zoned_date_time_millis(ms) (ingest timestamp / metadata().getNow())
// ingest `if` conditions: es::write::conditional_ctx(&ctx) -> Value (read-only UnmodifiableIngestData view of an
//   ingest ctx or a plain map; build it only when script.uses_var("ctx")), Context::ProcessorConditional, execute_bool
// pipeline aggs: Context::BucketAggregation / AggregationSelector (buckets_path values in params, NaN for gaps),
//   Context::MovingFunction (set_var("values", Value::double_array(window)))
// grok::{PatternBank::legacy()/ecs_v1()/new/extend_with, Grok::new(&bank, expr) -> Result<Grok, GrokError{kind,message}>,
//   Grok::captures(text) -> Result<Option<Vec<(field, GrokValue)>>, MatchError>, matches(text) -> Result<bool, MatchError>,
//   warnings(), with_max_execution_time(Option<Duration>) (watchdog; default DEFAULT_MAX_EXECUTION_TIME = 1s,
//   ingest.grok.watchdog.max_execution_time / runtime_fields.grok.watchdog.max_execution_time; None or zero = off)}
//   MatchError::{Convert(ConvertError), Interrupted{after_millis}}: kind() ("number_format_exception" |
//   "runtime_exception") and reason() ("grok pattern matching was interrupted after [1000] ms") are Elasticsearch's
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
`similarity.rs`, `heuristic.rs`, `analysis.rs`, `call_sites.rs`, `mustache.rs`, `expression.rs`). `def` operator call sites keep
what they linked across executions of a `Script` (as Painless's do), so null-operand errors read `leftObject`
after the first execution: share one compiled `Script` per source, as Elasticsearch's script cache does.
`mustache::Template` compiles sources over 2 KiB on its own large-stack thread, `render_json` renders templates
nested over 128 sections deep on one, and `render(&Value)` stops with `stack_overflow_error` at the execution
stack budget: hosts may call both from ordinary 2 MiB threads.
`search-painless` enables serde_json's `float_roundtrip` feature: params and sources must parse doubles
exactly, as Jackson does (the default parser can be off by one ulp).
Straight-line per-document scripts — score, filter, sort, aggregation (`def` results, boxed), script-field and
runtime-field (`emit` of long/double/date/boolean) scripts made of locals and assignments, `if`/`return`,
`for (x : doc['f'])` over numeric/boolean doc values, `_score`, `params`, `doc[..].value/size()/isEmpty()/empty`,
`Math`, decay/saturation/sigmoid (numeric and `decayDate*`), the vector functions over `params.x`,
keyword `doc['f'].value ==`/`!=`/`.equals` a literal or `params.x`, `doc['f'].contains(s)`,
`params.list.contains(doc['f'].value)`, `date` `.getMillis()`/`.millis`/`.toInstant().toEpochMilli()`
— run on a fast path specialized on an instance's first document from its
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
    pub async fn claim_cluster(&self, cluster: &str) -> Result<()>;  // ownership marker for the epoch; before reading the cluster
    pub async fn check_owner(&self, cluster: &str) -> Result<()>;    // Fenced once a newer owner claimed (one listing)
    pub fn epoch(&self, cluster: &str) -> u64;
    // segments: write to segment_path(id) (staging, same fs as the cache), then upload; the store takes the file
    pub fn segment_path(&self, id: &SegmentId) -> PathBuf;
    pub async fn upload_segment(&self, seg: &Path) -> Result<SegmentKey>;     // file must be named <32 hex>.seg
    pub async fn open_segment(&self, key: &SegmentKey) -> Result<SegmentData>;  // cache hit, or one shared fetch
    pub async fn open_segments(&self, keys: &[SegmentKey]) -> Result<Vec<SegmentData>>;
    pub async fn fetch_segment(&self, key: &SegmentKey) -> Result<SegmentFetch>;  // use before the download completes (below)
    pub fn fetch_part(&self) -> u64;   // ranged-read size of downloads (StoreConfig.fetch_part_bytes, default cache::FETCH_PART 8 MiB, >= 64 KiB)
    #[doc(hidden)] pub fn set_download_throttle(&self, bytes_per_sec: u64);   // tests: downloads share a slow link
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
    // Pins (snapshot contents): a commit point copied under search/c/<cluster>/p/<pin>/<shard>.json, outside any shard's
    // history (waking never reads them); GC keeps what a pin names (GcStats.pins); branches copy them (PIT: written <= at).
    // Written once (same segments again: no-op; other contents: error); fenced like manifests; WAL ranges and base dropped.
    pub async fn write_pin(&self, cluster: &str, pin: &ShardId, m: &ShardManifest) -> Result<()>;
    pub async fn read_pin(&self, cluster: &str, pin: &ShardId) -> Result<Option<ShardManifest>>;
    pub async fn list_pins(&self, cluster: &str) -> Result<Vec<ShardId>>;
    pub async fn delete_pins(&self, cluster: &str, pins: &[ShardId]) -> Result<()>;
    pub async fn collect_garbage(&self, grace: Duration) -> Result<GcStats>;  // one place per bucket
    pub async fn cluster_usage(&self, cluster: &str) -> Result<ClusterUsage>;   // incl. cached_bytes on this node
    pub fn stats(&self) -> StoreStats;  pub fn set_cache_budget(&self, bytes: u64);
}
impl WalShard { pub fn append(&self, seq_no: i64, op: &[u8]) -> Result<Lsn>;
                pub fn append_many<B: AsRef<[u8]>>(&self, ops: &[(i64, B)]) -> Result<Lsn>;  // one lock, one commit, all or nothing
                pub fn append_parts(&self, seq_no: i64, parts: &[&[u8]]) -> Result<Lsn>;   // one record, payload = parts concatenated (no copy)
                pub fn stats(&self) -> ShardWalStats; }
impl Wal {
    pub async fn wait_ack(&self, lsn: Lsn) -> Result<()>;      // translog.durability=request: local fsync, or S3 with sync_objects
    pub async fn durable(&self, lsn: Lsn) -> Result<()>;       // local fsync
    pub async fn durable_remote(&self, lsn: Lsn) -> Result<()>; // in object storage (seals the file now)
    pub async fn sync_remote(&self) -> Result<()>;  pub fn flush_now(&self);  pub fn stats(&self) -> WalStats;
    pub fn check_room(&self) -> Result<()>;          // WalFull when appends would be refused (check before applying an op)
    pub fn cluster_stats(&self, cluster: &str) -> ClusterWalStats;   // per-tenant uncommitted ops/bytes
    pub fn pinning_shards(&self) -> Vec<PinningShard>;   // shards with uncommitted ops, oldest pinned WAL file first: flush candidates
}
pub struct WalRecord<'a> { pub at_ms: u64, pub seq_no: i64, pub epoch: u64 /* ownership epoch it was appended under, 0: none */,
                          pub cluster: &'a str, pub index_uuid: &'a str, pub shard: u32, pub payload: &'a [u8] }
// Wal::shard(cluster, shard) = shard_at(cluster, shard, 0); Wal::shard_at(.., epoch): every record of the handle carries the epoch.
// attach_shard registers with the store's epoch of the cluster and opens a WalSource { epoch: Some(e) } (e > 0): replay of that
// range skips records of any other epoch (a deposed owner's), whatever their time or file.
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

`StoreConfig { node, wal: WalConfig { flush_delay: 5ms /* longest an unwaited record stays buffered: commits are on demand,
back to back, as soon as anyone waits (durable / wait_ack) for a buffered record */, flush_bytes: 4 MiB, file_bytes: 64 MiB,
ship_interval: 1s, sync_objects, fsync, leader_commits: true /* a waiter commits the batch on its own thread while
fsyncs average under 250 µs and no commit runs */, max_pending_bytes, ship_concurrency, zstd_level, replay_cache_bytes,
max_local_bytes: 8 GiB /* unshipped bytes past which appends fail with WalFull */ }, cache_bytes, manifest_retain (4),
manifest_retain_ms (point-in-time window, 0), state_retain (4), single_put_bytes, verify: Option<Verifier>,
fetch_part_bytes (8 MiB: parallel ranged reads of this size; a segment larger than one part can be read while it downloads) }`.
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
Fencing: search-server calls `set_epoch(cluster, lease_epoch)` and then `claim_cluster(cluster)` (writes
`search/c/<cluster>/owner/<epoch:020>`, drops older markers; `Fenced` if a newer one exists) before reading the
cluster. A write (`write_manifest`, `attach_shard`, `detach_shard`, `write_cluster`, `delete_shard`) that finds a
newer epoch — in the versions it writes over or, with an epoch set, in the ownership markers — fails with `Fenced`,
and the node's WAL then refuses the cluster's appends: unload the cluster. `check_owner(cluster)` (one listing;
no-op without an epoch; counted in `StoreStats::owner_checks`) fails the same way once a newer owner has claimed. `WalFull` is
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
  (`crates/search-es/spec/opensearch/`, plus roll's own files for the bundled plugins' APIs in
  `spec/opensearch/plugins/`), so every path, method and parameter of each dialect is
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
pub async fn handle(cluster: &Cluster, req: EsRequest) -> EsResponse;            // Send future; search APIs run in block_in_place
pub async fn rest::handle_shared(cluster: &Arc<Cluster>, req: EsRequest) -> EsResponse; // search APIs on a blocking thread,
                                                                                  // cancelled (ambient SearchControl.cancelled) when the future is dropped
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
// Document and search half (engine state lives behind an in-memory trait; persistence/idle unload plug in there):
// Cluster { engine: engine::Engine, scripts: Arc<scripting::ScriptService>, contexts: search::contexts::Contexts /* scroll, PIT */,
//           ingest: ingest::pipeline::Cache /* compiled pipelines per metadata version */,
//           knn: Arc<opensearch::knn_api::KnnRuntime> /* k-NN plugin counters, graph-cache ledger */, .. }
// opensearch::ism: the ISM plugin API (policies and managed indices in xpack::store families
// `ism.policy` / `ism.managed` / `ism.meta`, config index `.opendistro-ism-config`);
// Cluster::update calls ism::reconcile(prev, &mut next, now) on every state change of the
// OpenSearch dialect: indices gone (by uuid) stop being managed, new ones (any API: create,
// resize, restore, rollover, data streams) are matched against ism_templates.
// engine::ShardStorage: Send + Sync { fn open(&self, index_uuid, shard: u32, schema) -> EsResult<search_index::Shard>;
//   fn log(&self, index_uuid, shard) -> Option<Arc<dyn ShardLog>> /* default None; asked once per opened shard */;
//   fn snapshots(&self) -> Option<Arc<dyn SnapshotStorage>> /* default None: snapshot contents stay in memory */ }
// #[async_trait] engine::SnapshotStorage { async fn capture(snapshot_uuid, index_uuid, shards, Option<Arc<IndexEngine>>);
//   async fn load(snapshot_uuid, index_uuid, shards) -> Vec<SnapshotShard { segments: Vec<SegmentHandle>, numbers }>;
//   async fn clone_snapshot(source, target, index_uuids); async fn copies() -> Vec<SnapshotCopy { snapshot, index_uuid, shard }>;
//   async fn drop_copies(&[SnapshotCopy], older_than: Option<Duration>) /* refused once unloaded */ } — all EsResult;
//   Engine::snapshot_storage(); Engine::snapshot_searchers(cluster, st, idx, Vec<SnapshotShard>) -> Vec<Searcher> (persist_hooks).
//   api/snapshots.rs: with storage, create captures every index it includes before recording (an index that appears or is
//   recreated meanwhile is captured and recording retried), so a recorded snapshot is complete. A copy no recorded snapshot uses
//   (keyed by snapshot uuid + index uuid) is dropped at once only if its snapshot can't be recorded (deleted, or failed before
//   saving the state); otherwise only once older than a grace and never while this process is taking it (SnapshotData taking
//   set): api::snapshots::{LEFTOVER_GRACE (1 h), sweep_leftover_copies(&Cluster, grace)} (search-server runs it after a wake).
//   Restores register in SnapshotData; deleting a snapshot being restored is a 503 concurrent_snapshot_execution_exception.
//   restore loads pins when the process has no in-memory copy, clone copies pins first.
// engine::ShardLog: Send + Sync { fn check(&self) -> EsResult<()> /* before applying (WAL full: 429) */; fn append(&self, &LoggedOp) -> EsResult<()>;
//   fn flush_requested(&self) /* default no-op: `_flush` / a flushing force merge flushed this shard; the storage commits
//   it (and calls ShardEngine::committed) before the request is answered */ }
//   ShardEngine.log: Option<Arc<dyn ShardLog>>; write::apply_index/apply_delete (and copy_index) check before and append
//   after the shard applied an operation: LoggedOp::{Index { id, routing, source /* as indexed, after ingest */, seq_no,
//   primary_term, version }, Delete { id, seq_no, primary_term, version }, NoOp { seq_no, primary_term } /* Error::Rejected */};
//   an append error fails the write with it. write::Prepared gained `source: Bytes, routing: Option<String>`.
//   Engine::replay(&IndexMeta, shard, &LoggedOp) (persist_hooks.rs): applies a logged op again (Origin::Replay, parsed
//   against the current mapping; stale/duplicate ops are no-ops). engine::apply_shard_settings: `index.gc_deletes` → shards;
//   Engine::apply_settings(&IndexMeta) applies them to a loaded index at once (the storage calls it on every state change).
// Contexts::latest_expiry() -> Option<Instant> (when the last open scroll/PIT expires; None: none open)
// translog stats: ShardCounters.{translog_ops, translog_bytes, translog_modified_ms}; ShardEngine::logged(&LoggedOp) (write.rs, every
//   logged op, in memory too), translog_mark() -> (ops, bytes) before a commit's Shard::flush and committed(mark) once it is durable
//   (search-es `_flush`/close for in-memory shards, search-server's full commits for durable ones); engine::translog_op_bytes(&LoggedOp)
//   (Elasticsearch's serialized size, Uid-encoded ids) and TRANSLOG_HEADER_BYTES (55, an empty generation)
// MetaStore::save errors that are (or wrap) an EsError reach the client as that error (search-server: 503 for a save
//   racing an unload); others stay 500 `failed to persist cluster state`
//   engine::MemoryShards (default); Engine::with_storage(Arc<dyn ShardStorage>); Engine::index(cluster, st, idx) -> Arc<IndexEngine>
//   (opens shards lazily, keeps the mapping→engine field table in sync, never moving it back: FieldTable.key is
//   engine::table_key(st, idx) = (mapping_version, settings_version, synonyms_version) and metadata older in any
//   component leaves the table alone), Engine::loaded(uuid), retain(live uuids), remove(uuid)
// engine::IndexEngine { uuid, name, shards: Vec<Arc<ShardEngine>>, fielddata_loaded, .. } — table() -> Arc<fields::FieldTable>, shard(n),
//   refresh_all(), doc_count(); construct with IndexEngine::assemble(uuid, name, shards, numbers, table) (crate-internal; hooks use it),
//   load_fielddata(field, aggregation), field_stats_into(&ShardEngine, &mut Map, fielddata_fields, completion_fields) (`fielddata` /
//   global ordinals / `completion` stats); ShardEngine::group_stats_into(&mut Map, groups) (search `stats` groups)
// engine::ShardEngine { id, shard: search_index::Shard, counters: ShardCounters } — refresh(), before_search(interval_ms) (lazy
//   refresh: `index.refresh_interval` elapsed or search-idle 30 s), searcher(), stats_into(&mut Map) (`_stats` blocks)
// engine::fields::FieldTable — mapping → engine fields (stable numbers per path; dense_vector numbered per index layout),
//   analyzers, nested paths; engine::convert::to_indexable(table, &ParsedDocument, DocMeta { id, routing, source, tsid: Option<(&[u8], &str)> })
//   -> IndexableDoc (time series docs: `_tsid` and `_ts_routing_hash` sorted doc values, engine fields 13 and 15, instead of `_routing`)
// engine::tsdb — time series indices like Elasticsearch 9.5: routing_hash(idx, &source) (routing_path), build_tsid(&dimensions(idx, &parsed)),
//   create_id / create_synthetic_id (`index.mapping.synthetic_id`, default true), id_routing_hash(idx, id), encode_hash/decode_hash
//   (`_ts_routing_hash`), invalid_id / unsupported errors; meta::routing::shard_for routes time series docs by that hash
// write::{prepare (-> Prepared { .., id /* generated for time series */, tsdb }), apply_index, apply_delete, resolve_target,
//   resolve_existing, check_may_auto_create, effective_routing, RefreshPolicy, copy_index /* resize */}
//   — index/create/delete with routing, seq_no/primary_term/_version, OCC, version_type, op_type, require_alias/data_stream
// fetch::{FetchSource, source_bytes, stored_fields, render_source}; scripting::ScriptService::compile(st, &Script, context) -> Bound
// ingest::run_for_write(cluster, Target, &WriteArgs, id, Value, Bytes, now, clock: Option<&AtomicU64> /* Ctx.ingest_nanos,
//   `_bulk`'s ingest_took */) -> Outcome::{Index(Target, Value, Bytes), Dropped(index)}
//   (request/default pipeline, final pipeline, re-targeting; non-finite doubles come back as "NaN"/"Infinity" strings, as
//   Elasticsearch re-reads its own JSON: json::write::quote_non_finite); ingest::pipeline::simulate; ingest::doc::java_hash_order(map)
//   (Elasticsearch's HashMap key order for ingest output and hit `fields`); ingest::jregex::{JavaPattern::{compile, find_all,
//   split(text, limit)}, JavaReplace::{new(pattern, replacement), replace_all}} — java.util.regex semantics for gsub/split/kv,
//   run through search-analysis's engine (a one-off `pattern_replace` normalizer)
// search::aggregate_metric::{delegate, Delegate, metric_field, terms, avg_range, sort_key, exists_field} — queries and sorts
//   on `aggregate_metric_double` fields (single metric, or the sum/value_count average); document::values::aggregate_metrics(&FieldMapper);
//   fetch::SourceView { idx, table, force_synthetic, search /* search fetch keeps vectors of a forced synthetic source */ };
//   time::DateFormatter::{derive(pattern) /* the field's locale */, parse_with_year(text, zone, round_up, default_year)}
// search::{search, msearch, count, explain, run (scroll/PIT), execute(cluster, &SearchRequest) -> SearchOutcome, render};
//   search::query::QueryCtx::to_query(&query::Query) -> QueryRef (Query DSL → engine queries with Elasticsearch's rules);
//   search::aggs::{Lookup /* search_aggs::FieldLookup incl. top_hits fetcher, hit sort, scripts */, reduce};
//   search::agg_scripts::Host (search_aggs::ScriptHost over painless); search::profile (profile: true output);
//   search::dv_terms::DocValuesMultiTermQuery::query(MultiTermQuery) (prefix/wildcard/regexp/fuzzy on doc-values-only fields);
//   search::dv_distance::DocValuesDistanceFeatureQuery::query(field, Origin) (distance_feature without points)
// api::by_query_request::{parse_by_query(ctx, update), parse_reindex(ctx)} -> ByScroll (Elasticsearch's parse/validate order);
//   api::remote::{check_whitelist(node setting `reindex.remote.whitelist`, &Remote), fetch(..)} — reindex from remote (HTTP, reqwest);
//   node settings `http.publish_host` / `http.publish_port` set the advertised HTTP address (node info, remote reindex)
// engine::synthetic — synthetic _source built at index time: source_mode(idx) -> SourceMode::{Stored, Synthetic, Disabled},
//   options(idx) -> Options { keep: Keep::{None, Arrays, All}, binary_fallback }, synthesize(&mapping, &ctx, &table, Options, &doc) -> Map,
//   vectors(table, path, &mut map, exclude) / vectors_keeping(.., Option<&Globs>) (index.mapping.exclude_source_vectors);
//   write::stored_source(idx, &table, &bytes, &value) stores the synthetic source for synthetic indices
// fetch::SourceView { idx, table, force_synthetic }.apply(value, &FetchSource) and render_source_view(..) — what get/search/explain
//   fetch (force_synthetic_source, vectors left out unless `exclude_vectors: false` or included); FetchSource.exclude_vectors
// failure_store::{target(cluster, expr) -> Store::{None, Disabled, Enabled(ds)}, eligible(&err), redirect(..)} — data stream failure
//   stores (failure documents, lazy failure index creation, `failure_store: used|not_enabled`); EsError::{with_trailing, take_trailing}
// engine::completion::{context_defs, index_contexts, query_contexts, ContextAutomaton, key_context, CONTEXT_SEP} — completion contexts
// engine::convert::{encode_binary_dv, decode_binary_dv} (multi-valued `binary` doc values, sorted and unique)
// search::retriever::{apply(&mut SearchSource, url_sort), Diversify::mmr} — standard/knn/rescorer/diversify retrievers;
//   SearchSource.diversify; SearchRequest.force_synthetic_source; search::hits::vector_of(table, searcher, doc, field)
// ingest::simulate::simulate_ingest (POST _ingest/_simulate); ingest::pipeline::{with_substitutions, take_failure -> IngestFailure}
// MappingCtx.{ignore_dynamic_beyond_limit, ignore_dynamic_names_beyond_limit}; FieldTable.{has_vectors, copy_sources}
// tsds::{route(cluster, Target, create, &source, &value, now) -> Target, document_timestamp, select, past_window,
//   value_location_path} — time series data streams route creates by @timestamp (timestamp_error), past backing indices;
//   api::indices::BackingOf gained `past: Option<(start_ms, end_ms)>`
// engine::tsdb::{tsid_dv, tsid_from_dv, tsid_display, tsid_parse} — `_tsid` doc values (engine field 13) hold the _tsid as
//   lowercase hex (byte order kept); responses print base64url. search::aggs::{render_tsid_keys, mentions_tsid}
// search::runtime::{RuntimeFields::of(&mapping, Option<&runtime_mappings>), RtField, RtType, Evaluator::new(&ScriptService, st,
//   &table, shard, index, Arc<RtField>).leaf(seg)?.values(doc), render, Predicate, RuntimeQuery (FilterScript), agg_script,
//   from_agg_params, check_time_series} — runtime fields; QueryCtx.runtime_mappings; mapping::RuntimeField::parse_def
// FieldTable::keyed(path) -> Option<Arc<FieldInfo>> (keyed flattened sub-fields; FieldInfo.flattened_key);
//   Mapping::prefix_disabled(field_path) (strict columnar prefixes)
// write::{seq_no_disabled(idx), seq_no_unsupported(..)} — index.disable_sequence_numbers (default in the columnar modes);
//   write::prepare(.., dynamic_templates: Option<&Map>) — bulk items' `dynamic_templates` (MappingCtx.requested_templates)
// SearchSource.rescore_before_collapse (rescorer retrievers); search_source::Rescore.script (script rescorer)
// ingest::pipeline::failure_of(&EsError) -> Option<(IngestFailure, created data streams)> (the failure travels in the error:
//   EsError::{with_hidden, hidden}); IngestFailure.index (the target its failing pipeline started from)
// search::query_phase(cluster, st, req, t, ti, shards, n, after, aggs, knn, window: Option<EsError>) — `shards` is the request's
//   shard count (slices spread over shards), `window` the shard's result-window failure (raised after query/aggs/highlight build)
// search::aggs::Lookup::with_parent_fields(&[FieldAndFormat]); aggs::TOP_HIT_MARKER (hidden key top_hits hits carry to the
//   coordinator: highlight, matched_queries, explain, runtime fields); aggs::check_metric_scripts(st, &aggs) (metric script
//   allow lists); Lookup implements FieldLookup::{text_terms (significant_text/categorize_text), global_query (alias filters),
//   date_formatter (roll's Java-compatible date parser)}; ShardOptions.{profiler, max_buckets, matches_all} are passed
// search::highlight::highlight_hit(.., doc: Option<u32>) (stored fields of `store: true` fields); search::inner::{render(&QueryCtx,
//   ..), nested_identity(table, seg, local) -> Option<NestedIdentity { root, levels }>, NestedIdentity::{to_value, source_of}}
// search::painless_execute::execute (POST _scripts/painless/_execute: painless_test, filter, score and runtime field contexts
//   over a one-document private in-memory engine); scripting::analysis_scripts() (analysis predicate scripts)
// api::tasks::persist_result(ctx, id) — finished stored tasks are written to the `.tasks` system index; GET _tasks/<id> falls
//   back to it; api::shards::{segment_rows(ctx, idx, shard) -> Vec<SegmentRow>} (real `_segments` / `_cat/segments`)
// engine::index_sort(idx, &table) -> Option<IndexSort> (index.sort.* and mode defaults go into the shard schema)
// FieldMapper::validate_type(&ctx); MappingCtx.{array_objects_limit, dynamic_auto_text}; EsError::set_meta(key, v);
//   units::java_base64_url_error(&str) (java.util.Base64 decoder messages); engine::source::extract_values_keeping_coordinates
// Cluster::enable_background_tasks(self: &Arc<Self>) — owners holding the cluster in an Arc call it once: `wait_for_completion
//   =false` by-query requests then run on a spawned task after the response (holding a strong handle while they run; their task
//   is registered, so it pins the cluster), else they run before answering; Cluster::handle() -> Option<Arc<Cluster>>
// TaskInfo.live: Option<Arc<api::by_query::Live>> (rate for _rethrottle, progress for GET _tasks, slices as child tasks);
//   TaskRegistry::{set_live, adopt(id), record(&guard, outcome), wait_until(timeout, done) /* wait_for_completion */};
//   TaskGuard::detach() -> u64; api::tasks::{by_node(ctx, tasks, detailed), node_failure(node, err)}
// api::by_query::scan(..) -> Scanner { total, next_batch(n) } (doc ids up front, documents a batch at a time)
// join::Joiner::{of(&Mapping), from_mapper, parent_of, is_parent, is_child, children, parent_join_field, child_join_field};
//   FieldValue::{Join { name, parent }, Percolator(json, location)}; MappingCtx.join_routing; FieldTable has `<join>#<parent>`
//   entries (keyword-like, sorted doc values); search::join::{has_child, has_parent, parent_id, inner_hits, JoinInner};
//   InnerHitsDef.join; aggs::Lookup implements FieldLookup::join (children / parent aggregations)
// search::percolate::{percolate, check_query, MemoryIndex::build(cluster, st, idx, docs), Percolated, check_indexed_documents};
//   percolator fields keep their query in binary doc values; ShardResult.percolate
// search::percolate_candidates::{needs(&QueryRef) -> Needs, StoredQueries (per-segment cache "percolator"), adds_fields}
// search::fusion::{execute, rank, Fused { node: Arc<Node>, cap }, Node, Child}; retriever::{Fusion { kind, window, min_score,
//   children, names, tails }, FusedDoc, FusionKind::{Rrf { rank_constant, weights }, Linear, Pinned { pins, organic }}, Normalizer};
//   SearchSource.{fusion, sub_searches}; SearchRequest.{fused, rank_only}; SearchOutcome.ranked: Vec<(target, doc, score)>;
//   the final phase's query per shard is Fused::query(qctx, target, explain) = search::rank_docs::RankDocsQuery
//   (TopQuery + tail; RankKind::{Plain, Rrf, Linear, Pinned} explanations); search::extra::MAX_ORGANIC_SCORE
// search::knn::{knn_search (_knn_search, compatible-with=8 only), lookup_vector, resolve_lookups, decode_query_vector};
//   knn::similarity_to_score(&VectorSpec, s); FieldInfo.vector_layouts (earlier engine fields of a dense_vector whose
//   index_options changed; searched together); QueryCtx.{alias_filter, exact_knn, strict_unmapped, unmapped, percolate,
//   constant_joins}; query::contains_query(&Query, name)
// engine::{VectorStats, VectorSizes}, ShardEngine::vector_stats(&table) (dense_vector / sparse_vector stats);
//   api::disk_usage::disk_usage (_disk_usage from segment sections); api::rank_eval::rank_eval;
//   api::document::check_active_shards_for(ctx, idx, shard, request description)
// api::indices::add_index_stats(cluster, idx, &mut block, shard: Option<u32>) (engine figures into a stats_block_in block;
//   used by _stats-shaped node stats and _cat/indices: api::cat::stats::of_block(block, column));
//   search::{max_slices_per_scroll(idx), check_slice_count(idx, max)} (sliced scrolls and every by-query slice);
//   search::suggest::check(&XParser) (SuggestBuilder.fromXContent checks); sort::parse_after(v, key, format, now, pos, field)

// search-server
#[async_trait] pub trait Catalog { async fn lookup(&self, cluster: &str) -> Option<ClusterSpec>; }    // StaticCatalog
#[async_trait] pub trait StoreProvider { async fn meta_store(&self, cluster) -> Result<Arc<dyn MetaStore>>;
                                         async fn branch(&self, parent, child) -> Result<()>; async fn delete(&self, cluster) -> Result<()>;
                                         async fn release(&self, cluster) /* on unload; default no-op */;
                                         async fn branch_at(&self, parent, child, at_ms: Option<u64>) -> Result<()> /* default: branch() for None */;
                                         fn data(&self) -> Option<search_store::Store> /* default None; ObjectStores: Some — shards become durable.
                                                                                          WRAPPERS MUST FORWARD IT (search-control's GuardedStores) */ }
// MemoryStores (in memory, shards in memory too); ObjectStores::new(search_store::Store) (metadata as search-store cluster states:
// read_cluster/write_cluster, branch, delete_cluster, release_cluster on unload; shards through search_server::persist)
pub struct ClusterSpec { pub name, pub dialect, pub credentials: Vec<Credential>, pub idle_unload: Option<Duration>,
                         pub max_in_flight: usize, pub max_body_bytes: Option<usize>, pub destructive_requires_name: bool,
                         pub max_requests_per_second: Option<f64> /* token bucket, burst max(1, rate); 429 */,
                         pub max_in_flight_bytes: Option<usize> /* body bytes in flight, default 2 × body limit; 429 like indexing pressure */,
                         pub max_resident_bytes: Option<u64> /* over it the node commits the cluster's shards (buffers → files) */ }
pub struct Credential { pub user: Option<String>, pub api_key_id: Option<String>, pub secret: String, pub secret_sha256: Option<[u8; 32]> }
// Credential::{password(s), hashed(sha256), for_user(u), for_api_key(id)}; search_server::secret_hash(s) -> [u8; 32]; compared in constant time
pub struct NodeConfig { pub node_name, pub node_id: Option<String> /* lease identity, default node_name */, pub domain: Option<String>,
                        pub idle_unload: Option<Duration> /* 300 s */, pub max_body_bytes /* 100 MiB */,
                        pub default_cluster: Option<String> /* dev mode: no auth */,
                        pub node_settings: Vec<(String, String)>, pub test_features: bool /* copied into ClusterConfig */,
                        pub host_metrics: bool /* copied into ClusterConfig; search-dev sets it */,
                        pub max_running: usize /* node-wide running requests, granted round-robin across clusters; 0 = 2 × cores, usize::MAX = off */,
                        pub lease_ttl: Duration /* 10 s */,
                        pub snapshot_leftover_grace: Duration /* 1 h: age before the post-wake sweep drops unrecorded snapshot pins */,
                        pub memory_budget: Option<u64> /* resident bytes of all loaded clusters; LRU idle clusters are unloaded past it,
                                                          and a wake makes room first */,
                        pub unload_memory_only: bool /* false: with a StoreProvider whose data() is None, idle and budget unloads skip
                                                        clusters holding documents (they'd lose them; warned once); explicit unload still
                                                        unloads. true: tests / throwaway nodes */ }
#[async_trait] pub trait LifecycleHooks { async fn loaded(&self, &Arc<Cluster>); async fn unloading(&self, &Arc<Cluster>); }
impl Node { fn new(catalog, stores, cfg) -> Arc<Node>; fn with_hooks(..); fn set_leases(Arc<dyn Leases>) /* before the first request */;
            async fn open(spec) -> Result<Arc<Loaded>> /* Err downcasts to node::OpenError { error: EsError } for refusals */;
            fn idle(older_than) -> Vec<String> /* a spec's own idle_unload overrides older_than; the reaper also runs for clusters
                                                   with their own idle_unload when the node has none */;
            async fn unload(name) -> bool /* flush + detach + release lease + free memory */;
            async fn force_unload(name) -> bool /* drop without writing (lease lost); requests still running on the instance get 503
                                                   for writes no WAL record covers yet, and its metadata store refuses saves */;
            async fn branch(parent, child); async fn branch_at(parent, child, at_ms: Option<u64>) /* commits a loaded parent first */;
            async fn delete_cluster(name); async fn flush(name); async fn usage(name) -> Usage { loaded, docs, deleted_docs, store_bytes, resident_bytes, requests };
            async fn shutdown(timeout) /* refuse new requests (503 node_closed_exception, also for loaded clusters), let those in flight
                                         finish, unload all (flush, hand leases back; past `timeout` drop without flushing, lease revoked
                                         first so nothing more is acknowledged), close the WAL: SIGTERM */;
            async fn admit(&EsRequest, sni) -> Result<Lease, Rejected>; async fn enter(..) /* auth + rate */; async fn handle(EsRequest) -> EsResponse;
            fn loaded() -> Vec<String>; fn get_loaded(name) -> Option<Arc<Loaded>>;
            fn update_spec(ClusterSpec) /* live: credentials, rate, max_in_flight (permits added, or retired as they return), byte budget;
                                           also reaches a cluster whose load is in progress (applied as it is published); dialect and
                                           destructive_requires_name still need a reload */;
            async fn enforce_memory(room) -> Vec<String> /* the budget check the reaper runs every 2 s; returns the clusters unloaded */;
            fn take_request_counts() -> Vec<(String, u64)> /* metering: requests admitted per cluster since the previous call,
                                           unloaded instances included; each request reported once */;
            fn background_tasks() -> usize /* 0 whenever nothing is loaded */; fn scheduler() -> Option<&Arc<FairScheduler>>; pub wakes, unloads: AtomicU64 }
impl Loaded { pub cluster: Arc<Cluster>; pub data: Option<Arc<persist::ClusterData>>; fn spec(); fn in_flight(); fn requests() -> u64;
              fn epoch() -> u64; fn bytes_in_flight(); pub wake_time: Duration; pub recovery: persist::RecoveryStats { shards, segments, fetching, records, orphans } }
impl Lease { fn reserve_body(&mut self, &EsRequest, bytes) -> Result<(), Rejected>; fn reserve_more(..) /* grows the reservation (chunked frames,
              inflated output) */; fn over_budget(&EsRequest, used, bytes) -> Rejected; async fn acquire(&mut self, &EsRequest); async fn handle(&self, EsRequest) -> EsResponse /* + durability */ }
pub async fn serve(listener, Arc<Node>, ListenOptions { tls: Option<TlsAcceptor>, proxy_protocol, body_timeouts }) -> Result<()>; // also spawn(addr, ..)
pub async fn serve_until(listener, node, opts, Shutdown) -> Result<()>;  // Shutdown::new()/trigger(): stop accepting, finish in-flight requests, return
// lease::{Leases (acquire(cluster, node, ttl) -> Result<Result<epoch, Owner>>, renew, renew_many(&[(cluster, epoch)], node, ttl) -> Vec<bool>
//   /* the renewer's one round trip per period for every held lease; default loops over renew */, release, owner),
//   MemoryLeases, SqliteLeases::open(path), PgLeases::{connect(url), connect_with(url, ca_pem)} /* TLS per sslmode, verified */}
// process::{tune_memory() -> Tuned { thp_disabled } /* mimalloc purge delay 0 + PR_SET_THP_DISABLE; call first in main (rolld too) */,
//   on_thread_park /* tokio Builder::on_thread_park */}
// limits::{RateLimiter, ByteBudget { reserve, grow(&mut Reservation, n), .. }, Reservation { bytes(), shrink(n) }, FairScheduler};
// persist::{ClusterData { set_verify_owner, is_closed, commit_flushed /* full commits of the shards ShardLog::flush_requested
//   named; returns once they are committed, whoever commits them */, .. }, Unloaded /* error: flush_all/state saves after unload */,
//   segment_verifier() /* StoreConfig.verify */, resident_bytes(&Cluster), NUMBERS_KEY}; meta_guard::CheckedMeta::close()
// http: Netty's limits — request line > 4096 B → 400 too_long_http_line_exception, header lines > 16384 B → 400
//   too_long_http_header_exception (both close); body > max_content_length (declared, chunked or inflated) → empty
//   "413 Request Entity Too Large" + close; truncated gzip/deflate → the inflated prefix; invalid → connection dropped
//   unanswered; raw deflate accepted; HEAD never gzipped; inflate/gzip > 64 KiB on the blocking pool.
// net::{tls_config, tls_acceptor, read_proxy_header}.  Binary: search-dev --listen 127.0.0.1:59200 [--cluster] [--dialect] [--data-dir dir]
//   (--yaml-test-cluster also sets path.repo to <tmp>/roll-search-repo, so `fs` snapshot repositories can be registered)
//   [--tls-cert --tls-key] [-E key=value]... [--test-features] [--yaml-test-cluster] [--idle-unload s | --idle-unload-ms ms] [--sync-objects]
//   (--data-dir: the whole cluster — metadata and documents — durable through search-store in a local directory; SIGTERM/SIGINT
//   stop gracefully and flush; --yaml-test-cluster = the settings Elasticsearch's YAML test cluster runs with: node.attr.testattr=test,
//   node.processors=2, test cluster features)
```

A cluster isn't idle while it holds state that lives only in memory: an open scroll or point in time (exactly
until the last one is closed or expires: `Contexts::latest_expiry`) or a running task, and its idle time starts
when the pin ends (a long task's stored result gets a whole idle timeout to be fetched); memory-budget eviction
takes such clusters last. Each cluster reports a node id that is stable across wakes and hosts
(`node::stable_node_id(cluster)`, a base64url UUID derived from the name), and a cluster that never wrote its state
saves it on its first wake so `cluster_uuid` stays the same too. Wakes keep everything durable but not what lives only
in memory: `_stats`/`_nodes/stats` counters restart from zero, and documents indexed but not yet refreshed are
searchable after the wake (as after an Elasticsearch restart). Stored task results (`wait_for_completion=false`) live in
the `.tasks` index and survive. A state change on an instance that unloaded meanwhile (forced unload) answers 503
`cluster_block_exception`, never 2xx. Requests the node authenticated with the cluster's own credentials carry
`x-roll-authenticated-user` (search-es `xpack::security::AUTHENTICATED_HEADER`: the credential's user or `elastic`);
client-sent copies are stripped. Credentials that are not the cluster's own may belong to the cluster's security
(native or file-realm users, API keys, access tokens): `auth::native_authorization` turns the claim into the header a
client would send to Elasticsearch (`<user>@<cluster>` / `<key id>@<cluster>` lose the qualifier; Bearer only when
routed by host), the loaded cluster checks it with `search_es::xpack::security::authenticate_native` (never the owner
fallback: reserved users need a password of their own), and the request then runs with that `Authorization` header
instead of the trusted header, so search-es authenticates and authorizes it as that principal. Failures answer exactly
like wrong cluster credentials.

Every cluster state reaches the `StoreProvider`'s `MetaStore` through `meta_guard::CheckedMeta`, which parses it
exactly as a wake would (`ClusterState::from_json_in`, indices unchanged since the last save skipped by hash) and
refuses to save one that fails: the request gets search-es's 500 `failed to persist cluster state: the change would
leave a cluster state that cannot be loaded again: <parse error>`, nothing is published, and the cluster keeps its
last good state instead of becoming unwakeable. (`search-es` should still run the stricter check itself and answer
400 like Elasticsearch; the guard is the backstop.)

The idle reaper, lease renewer, async-durability syncer and background flushes exist only while a cluster that needs
them is loaded (`Node::background_tasks()` is 0 otherwise; the store's WAL thread sleeps without a timeout). `search-es`
cluster state JSON (`ClusterState::to_json`) is what `MetaStore::save` receives; `ObjectStores` stores it with
`search-store`'s `write_cluster`/`read_cluster`.

**Snapshot contents** are durable with shard durability: `ClusterData` implements `SnapshotStorage` by committing each
shard of a snapshotted index and pinning that commit (`Store::write_pin`, pin `snap.<snapshot uuid>.<index uuid>`, one
per non-empty shard, `user_data` reduced to the field numbering), so a restore after an unload, a crash, the index's
deletion, merges, garbage collection or in a branch gets every document with its seq_no, version and term.

**Shard durability** (`search_server::persist`, used when `StoreProvider::data()` is `Some`): the engine's
`ShardStorage` is `ClusterData`, and every opened shard's `ShardLog` appends each applied operation to the node WAL as
one record (`persist::record`: kind, seq_no, primary_term, version, id, routing, source; the source goes to the WAL as
a separate part, never copied first). Writes never cut segments: refresh, flush and merges follow the engine
(Elasticsearch's policy). After every request except GET/HEAD and read-only POSTs (`_search`, `_msearch`, `_count`,
`_mget`, `_field_caps`, term vectors, `_explain`, `_validate/query`, scroll and template searches, `_rank_eval`; never a
path with `_doc`/`_create`/`_update`/`_bulk`) the node waits for `Wal::wait_ack` of the cluster's newest record
when the indices written use `index.translog.durability: request` (default); `async` indices don't wait (records are
group-committed within `wal_flush` anyway). A shard attaches to the WAL on its first write after a wake (one manifest
write); operations logged before that are appended by `after_request` (or, for background tasks, a syncer that runs
only while tasks or pending operations exist). A *full commit* (`Shard::flush`, new segments to object storage, live
docs, delete tombstones younger than `index.gc_deletes` in `user_data["roll.tombstones"]` (newest 100 000), what the
searcher showed of the commit when it held unrefreshed writes in `user_data["roll.instance.published"]` (`persist::PUBLISHED_KEY`,
`[[hidden positions], [[position, [docs deleted since]]], [[merged position, [docs not shown]]]]`; absent for closed indices, which reopen fully searchable),
the shard's cumulative statistics in `user_data["roll.instance.stats"]` (`persist::STATS_KEY`; keys under
`search_store::INSTANCE_DATA_PREFIX` are not inherited by branches; search-es
`IndexEngine::{persisted_stats(n), restore_stats(n, json)}`: indexing/get/search counters, search groups,
request-cache hits and misses, field usage with its tracking id, the index's loaded field data; restored at wake
before replay; the unload's commit also writes when only these changed; also the request cache keys (rebuilt on use
and counted as hits, `engine::WarmCacheEntry`), the segment order the shard opened with when `@timestamp` was mapped
since (`Shard::leaf_sort`), and, at unload only and unless the shard was inactive for 5 minutes (Elasticsearch's
flush on idle), the translog figures), the soft-deletes retention lease in `user_data["roll.retention"]`
(`persist::RETENTION_KEY`, `[from, next, taken_unix_ms]`; `Shard::{retention_lease(), seed_retention_lease(..)}`, so a
reopened shard doesn't merge away deletes a running one retains), on shard 0 at unload the synonym rules the index's
analysis chain loaded when they differ from the sets (`persist::SYNONYMS_KEY` = `roll.instance.synonyms`;
`Cluster::{loaded_synonyms_json(uuid), restore_loaded_synonyms(uuid, json)}` over
`AnalysisCache::{loaded_synonyms, seed_loaded_synonyms}`: a set changed with `refresh=false` or replaced by rules the
chain can't build keeps its loaded rules across a wake),
`write_manifest` with `local_checkpoint` = the cut's, WAL trimmed) runs past `index.translog.flush_threshold_size`
(10 GB), `index.translog.flush_threshold_age` (1 m, checked on writes at most once a second), past
`persist::HEAP_COMMIT_BYTES` (64 MiB of buffers and in-memory segments, checked every 8 MiB logged), for the shards a
`_flush` or flushing force merge targeted (only those), when
an index closes (before the close is acknowledged: `_open` then sees every document, as Elasticsearch reopens on the
flushed commit), before branching and on unload. Wake: `recover_shards`, segments from the cache (an uncached segment
larger than one fetch part, `Store::fetch_part()`, opens while still downloading: `Store::fetch_segment` +
`SegmentHandle::fetching` behind `persist::FetchGate`, whose blocking waits use `block_in_place` on tokio workers; only
on a multi-thread runtime), `Engine::open_seeded(.., numbers)` with the index's field numbering (manifest
`user_data["roll.field_numbers"]`), seeded tombstones, then `ClusterData::ready` replays the WAL past the checkpoint
through `Engine::replay` (any order, duplicates and stale ops skipped: idempotent), fills seq_no gaps and refreshes, so
every acknowledged operation is back with its `_seq_no`, `_version` and `_primary_term` and searchable (a crash or a
handoff, as an Elasticsearch restart). When nothing was replayed (a graceful unload) a shard reopened with its
`roll.instance.published` view (`Shard::open_published`) is not refreshed: the wake is invisible, and
`ShardEngine::reopened_unrefreshed()` makes the next search refresh it unless refreshes are disabled. Requests wait
for `ready` (a shard that failed to replay answers 500 until it does); only `ready` may open a recovered shard
(`ShardStorage::open` answers 503 "is recovering" otherwise). `RecoveryStats { shards, segments, fetching, records,
failed_records, orphans }`. Segment files live in `<staging>/c-<cluster>/<uuid>-<shard>/` (`Store::staging_dir`) and
are uploaded from there. Errors: `WalFull` → 429 `es_rejected_execution_exception` (checked before an operation is
applied; an append that still fails keeps the record pending for the next request); `Fenced` or a lapsed lease → 503
`cluster_block_exception` and the cluster is dropped without writing; others → 500 `translog_exception`.
With leases (`Node::set_leases`), a wake takes the cluster's lease (refused: 503 + `X-Roll-Search-Owner: <node>`),
`set_epoch(cluster, epoch)` fences older writers in the store, `Store::claim_cluster` writes an ownership marker
before anything of the cluster is read, renewals run every `ttl/6` (`node::lease_renew_period`; a wake renews its
lease while it runs and once more before serving if the last renewal is older than a period), and writes are acknowledged only before
`sent + 0.8 × ttl` of the last successful renewal, so a partitioned owner stops acknowledging before another node can
take over. Multi-node deployments must use `sync_objects` (acks once the WAL is in object storage); there each
acknowledgement also waits for a `Store::check_owner` listing that started after its record was in object storage
(concurrent writers share one), so a deposed owner whose lease store wrongly vouches for it gets 503 instead of
acknowledging a write the new owner can't see. A request still running when its cluster is closed under it (lease
lost, persistence failure, shutdown timeout) is acknowledged only if a WAL record already covers its writes.
search-es hook (`crates/search-es/src/persist_hooks.rs`, declared in `engine::fields`): `Engine::{indices_snapshot(),
open_seeded(cluster, st, idx, numbers)}`, `IndexEngine::{field_numbers(), field_numbers_allocated()}`,
`Numbers::{snapshot(), allocated(), seeded(..)}`. search-es must not drop the `IndexEngine` of an index that still
exists (`ShardStorage::open` refuses to reopen a shard that is still open: an empty shard would lose its data).
Async results (async search, EQL, ES|QL, SQL async and cursors: `xpack::async_results::Results`) are durable as on
Elasticsearch, in the `.async-search` system index (`persist_hooks::ASYNC_RESULTS_INDEX`, ES 9.5.5 mapping and
settings, one document per result id): `Cluster::restore_async_results()` (search-server, at wake) reads them back
and turns on `Results::track_changes()`; `Cluster::sync_async_results()` writes the ids changed since
(`Results::{has_changes() /* one atomic load */, take_changes(now), note_changed(ids), note_all(), restore(id, entry, now)}`),
called by search-server before acknowledging any request that changed them (writes, and reads that touched a
`keep_alive` or opened a cursor; such a read fails with the persistence error when it can't be made durable) and
before an unload. xpack code only uses `Results::{put, get, touch, remove}`; in memory nothing is tracked.
Trained model deployment assignments are cluster state as on Elasticsearch: `xpack::ml::models::Deployments` notes
start/update/stop (`take_changed()`, `assignments(with_process)`, `restore(iter)`, `note_changed()`, `has_any()`),
search-server calls `Cluster::sync_ml_deployments(false)` before acknowledging a write (family `models::ASSIGNMENTS` =
`trained_model_assignment` in `Metadata.xpack`, `{model_id, params, start_ms}` per deployment id; a failure answers the
request with it), `sync_ml_deployments(true)` before an unload (adds the process's inference/cache-hit counts, last
access and cached inputs, so the wake is invisible), and `Cluster::restore_ml_deployments()` at wake (after a crash the
counters of the last unload, or none: a restarted process).

compat-es round 3 additions: `search::version_field::{encode, decode, Decoded, wildcard, prefix,
regexp, fuzzy}` (`version` fields index the `VersionEncoder` bytes; `Decoded` runs any automaton
over decoded terms so prefix/wildcard/regexp/fuzzy match the text users see);
`search::script_terms_sort::{ScriptTerms, TermsSortScript}` (`_script` sorts of type `string` and
`version`: shard-global ordinals computed once per shard, swapped back to bytes by
`sort::rescale`); `sort::ValueFormat::{Version, ScriptTerms(Arc<ScriptTerms>)}` (exhaustive matches
need arms); `search::computed_sort::{ScriptKey, GeoKey}`; `search_source::parse_sorts_value(&Value)`;
`engine::shapes::centroid_geometry` and a `<path>\0centroid` binary doc-values sub-field on
`geo_shape`/`shape` fields (`FieldInfo::subfield("centroid")`, the exact centroid Elasticsearch keeps
in its doc value header); `counted_keyword` fields get a `<path>\0count` binary sub-field
(`subfield("count")`); `Cluster::update` re-adds missing stack templates after every change (as
x-pack's registry does). search-painless: `cls::VERSION_OBJ` (`Version`, visible only in the
contexts of x-pack's version allowlist), `EsObj::Version`, `contexts::version_allowlist(ctx)` and
`Program.version_allowlist` (set by the engine after compiling).

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

Errors follow the other control planes: `{"error": "..."}` with 400/404/409/500, plus 413 when a
console request exceeds what the cluster's `max_body_mb` allows (`api::console_limit`: twice the
limit plus 1 MiB, room for JSON escapes; the cluster then enforces its exact limit). Also
`GET /v1/search` (info, pricing) and `GET /v1/search/clusters/{id}/branches`; `{id}` is the id or
the name. Full shapes: [README.md](README.md).

`search-control` (owned by the product crate) builds the node:

```rust
pub struct SearchConfig { home, node_name, region, public_host, http_port, tls, domain, idle_unload, cache_bytes, sync_objects,
                          history_retention /* StoreConfig::manifest_retain_ms */, gc_interval, metering_interval, stats_interval, test_features,
                          defaults: Defaults, pricing: Pricing }      // SearchConfig::local(home)
impl Control { async fn new(SearchConfig, pg_storage::Objects) -> Result<Arc<Control>>;   // opens search_store::Store + Node
               pub node: Arc<search_server::Node>; pub data: search_store::Store; pub store /* SQLite */; pub meter: Arc<Meter>;
               async fn shutdown(&self) -> Result<()>;  async fn sample(&self) /* one metering pass */; fn usage(id, days);
               async fn update(id, ClusterOptions); async fn rotate_password(id); async fn delete(id) }
// Changes are applied to the node also while a cluster wakes (control.rs/wake.rs: the catalog
// stamps lookups, a change of a cluster whose load is under way goes to Node::update_spec, which
// parks it until the instance is published; the node's StoreProvider is wrapped (GuardedStores,
// forwarding data()/branch_at()) to track loads and refuse deleted clusters, the load hook
// reconciles the new instance). Credentials, limits and max_in_flight (permits resized) apply in
// place; dialect and destructive_requires_name are fixed at load: changing them schedules a
// reload, done once no request runs.
// SearchConfig.public_url: the front (NLB / Lambda function URL) cluster URLs name; fleet: FleetConfig
// { meta, meta_ca, meta_password, node_id, az, advertise_http, advertise_api, tls_pin /* published in Beat.tls */,
//   api_tls /* the API listener serves TLS with the same certificate: Beat.api_tls */, heartbeat, lease_ttl, follow }.
// API calls forwarded to the owner go through Control::forwarder() -> Option<&Forwarder> (hyper client over
// search_proxy::upstream::Connector, pinned by the owner's record when it serves TLS).
pub enum ControlError { NotFound, Invalid, Conflict, TooLarge /* 413 */, Internal }
pub fn api::router(Arc<Control>) -> axum::Router;   // /v1/search
pub struct api::TlsListener;   // TlsListener::new(TcpListener, TlsAcceptor) -> io::Result: an axum::serve::Listener, handshakes on their own tasks
pub struct SqlCatalog;   // search_server::Catalog over SQLite: ClusterSpec { name, dialect from flavor, credentials: [password],
                         // idle_unload (0 = always loaded), max_in_flight, max_body_bytes, destructive_requires_name }
pub struct meter::MeterHooks(Arc<Meter>);   // metering half of the node's LifecycleHooks: exact loaded time; Meter::count_request(cluster)
pub fn meter::engine_figures(&Cluster) -> EngineFigures;   // docs (Lucene-level, nested included, = _stats docs.count),
                                                          // deleted, segment and heap bytes over open shards; complete()
pub fn meter::index_figures(&Cluster, uuid) -> Option<EngineFigures>;   pub fn meter::resident_bytes(&Cluster) -> i64;   // billed GiB-hours
```

A cluster's name is its key on the node and in object storage (Basic user, SNI label,
`cluster_name`); names are DNS labels. Metered requests are every request the node admitted
(`Node::take_request_counts`, read by `Control::sample`), console requests included.

## `search-proxy` (owned by the product crate)

The AWS front proxy and the fleet protocol the nodes share with it. It depends on nothing else in
roll search: ownership is the nodes' (`search_server::lease`); the proxy follows
`X-Roll-Search-Owner` on a 503 refusal.

```rust
// fleet: one heartbeated record per node (ages by the store's clock), table search_nodes
pub struct Beat { pub id, pub http /* host:port */, pub api: Option<String>, pub az: Option<String>, pub draining: bool, pub loaded: u32,
                  pub tls: Option<String> /* SHA-256 hex of the REST certificate: proxies connect with TLS pinned to it; None: plain */,
                  pub api_tls: bool /* the API address serves TLS with the same certificate */ }   // derives Default
// Beat::pins() -> Result<Vec<(SocketAddr, CertPin)>, String>: the pin per address (REST, and API with api_tls); Err for a malformed pin
pub struct NodeRecord { pub beat: Beat, pub age_ms: u64 }
#[async_trait] pub trait Members { async fn beat(&Beat); async fn leave(id); async fn list() -> Vec<NodeRecord>; }
// MemoryMembers::new(), SqliteMembers::open(path), PgMembers::new(Arc<pg::Pg>); fleet::open(url, ca, password) -> Arc<dyn Members>
impl Heartbeat { async fn start(members, Beat, every) -> Arc<Heartbeat> /* first beat must succeed */; fn set_loaded(n);
                 async fn drain() /* draining at once: SIGTERM */; async fn stop() /* leaves */ }
// pg::{Pg::connect(url, ca, password) /* TLS per sslmode, system roots + ca */, MetaUrl::parse(s) -> Postgres(url) | Sqlite(path),
//   libpq_url(url) /* sslmode=verify-ca|verify-full -> require (tokio-postgres can't parse them; we always verify) */}
// route::{cluster_of(authorization, host_cluster) /* the node's rules: Basic user[@cluster], ApiKey id, Bearer <cluster>_ */,
//   host_label(host, domain), weight(cluster, node), pick(cluster, &[Candidate { id, az }], az) /* rendezvous, own AZ first */}
pub struct ProxyConfig { az, domain, node_ttl /* 5 s */, refresh /* 1 s */, hold_timeout /* 60 s */, wake_every /* 10 s */,
                         connect_timeout /* 2 s */, retry_body_bytes /* 100 MiB per request */, retry_buffer_bytes /* 128 MiB per proxy */ }
impl Directory { fn new(Arc<dyn Members>, ProxyConfig) -> Arc<Directory>; async fn refresh(); fn spawn_refresh(); fn live() -> Vec<String>; fn tls(&addr) -> bool;
                 fn remember_owner(cluster, node); fn forget_owner(cluster); fn owner_of(cluster) -> Option<String> }
impl Proxy { fn new(Arc<Directory>, Option<Arc<dyn Waker>>) -> Arc<Proxy>; async fn forward(Request<B: Body + Unpin>, &ClientInfo, Port) -> Response<Body>;
             pub budget: Arc<replay::Budget> /* used(): request bytes kept for resends */;
             async fn serve(self: Arc<Self>, TcpListener, ListenOptions { port: Port::{Rest, Api}, tls, proxy_protocol, drain }, watch::Receiver<bool>);
             pub stats: Stats { requests, retries, redirects, held, wakes } }
pub trait Waker { fn wake(&self) -> BoxFuture<Result<()>> }   // EcsWaker::new(cluster, service, desired, region).with_endpoint(..) (SigV4,
                                                               // DescribeServices then UpdateService, never lowers desiredCount); CommandWaker(sh)
// proxy::{serve_health(listener) /* never touches nodes */, tls_acceptor(cert, key), read_only(method, path) /* safe to resend */};
// proxy_protocol::read_header (v1 + v2); lambda::{run(runtime_api, upstream, api_upstream, tls_seed: Option<&[u8]>), request_of, response_of}
//   (function URL 2.0; https:// upstreams pinned to upstream::seeded(tls_seed));
// aws::{ecs_availability_zone, ecs_task_id, advertise_for(bind)}
// upstream::{fingerprint(der), pem_fingerprint(pem), parse_pin(hex) -> Option<CertPin = [u8; 32]>, self_signed(names) -> SelfSigned { cert_pem, key_pem, fingerprint },
//   seeded(secret >= 16 bytes) -> SelfSigned /* deterministic Ed25519: the same certificate for the same secret */,
//   Pins { get, contains, replace(map), Pins::any(pin) /* for every address and host names */ },
//   Connector /* plain for http://, TLS for https:// accepting only the address's pinned certificate, never SNI */}
```

Retries: a request is resent only when the node provably didn't run it (connect failure, owner
refusal, `node_closed_exception`), or, once, when it is read-only and the node broke the
connection (to another node when there is one). Only a failed connect marks a node bad (3 s); a
connection broken by the client's own upload failing (`Replay::client_failed`) is never resent; the owner cache learns only from REST answers past the
node's credential check (not 401/403) and from refusals. A 503 over 64 KiB is passed on whole
without parsing. Bodies stream through and are kept as
they pass (`replay::Replay`, reference-counted chunks, no copy), so a refusal mid-upload resends
the whole body; a body over `retry_body_bytes` or the shared budget streams on but can't be
resent (the refusal reaches the client, and the owner it names is remembered for the retry). Binary: `search-proxy --meta <url>
[--listen 0.0.0.0:9200] [--api-listen] [--health] [--tls-cert/--tls-key | *_PEM env | --tls-self-signed | --tls-seed <secret>] [--proxy-protocol]
[--wake-ecs-service cluster/service --wake-count N | --wake-command sh] [--hold-timeout 120]`, or
`[--lambda] --lambda-upstream http(s)://nlb:9200 [--lambda-api-upstream http(s)://nlb:7070] [--lambda-tls-seed <secret>]` inside Lambda (the function's image command is `--lambda`, so the image's
`CMD ["rolld"]` never reaches this binary). Deployment:
[deploy-aws.md](deploy-aws.md).

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
  The differential suite (`crates/search-server/tests/differential.rs`) also runs with
  `ROLL_DIFF_ES=<url>`; its committed cases (`tests/differential/cases/*.json`, captured with
  `tests/differential/capture.py`) replay without Docker. The client matrix is
  `tests/search/clients/run.sh`.
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
