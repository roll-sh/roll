export const meta = {
  name: 'roll-search-phase-b',
  description: 'roll search phase B: build search-index then search-aggs; finish es-meta, yaml runner, painless; review and fix codec and analysis (3 lanes, retries on rate limits)',
  phases: [
    { title: 'Build', detail: 'owner agents per crate, multiple rounds' },
    { title: 'Review', detail: 'adversarial review against Elasticsearch behaviour and performance' },
    { title: 'Fix', detail: 'apply confirmed review findings' },
  ],
}

const REPO = '/home/meszmate/.nsh/roll/hardy-badger-fgmu'
const COMMON = `
You are one of several engineers building **roll search**, an Elasticsearch-compatible search engine written from scratch in Rust, in the repo at ${REPO} (a git worktree; work only there). Other agents are editing OTHER search-* crates in this same tree right now: never run git stash/commit/checkout/reset/clean, never touch crates you don't own.

Read first: docs/search/architecture.md, docs/search/contracts.md, AGENTS.md conventions (you already have AGENTS.md in context).

MANDATORY build rules (the machine has limited RAM):
- Every cargo command: \`CARGO_TARGET_DIR=${REPO}/target/agents/shared CARGO_BUILD_JOBS=4 cargo ... -p <your crate>\`. The shared target dir's lock makes builds run one at a time across agents; if cargo says "Blocking waiting for file lock", just wait. Never build the whole workspace, never \`cargo clean\`, avoid --release except brief benchmarks.
- MEMORY GUARD (other projects build on this machine too): before every cargo build/test/clippy and every docker run, wait until at least 6 GB is available: \`timeout 900 bash -c 'until [ $(free -g | awk "/^Mem/{print \\$7}") -ge 6 ]; do sleep 20; done'\`. Never run more than one cargo command at a time yourself. Keep test parallelism modest for memory-heavy tests.
- A shared reference Elasticsearch 9.5.5 already runs at http://127.0.0.1:19200 (container roll-es-ref, security off). Use it for checking behaviour and capturing fixtures. Prefix every index/template/pipeline/script name you create with your crate key (e.g. \`analysis-\`) and delete them when done; never delete others' objects, never wipe the cluster, never change cluster settings persistently. Do NOT start other Elasticsearch containers unless your task explicitly says so. Any other container you start: \`--rm\`, a memory limit, stop it when done.

Root Cargo.toml: only targeted Edit-tool edits to [workspace.dependencies] (never Write the file); only deps allowed in contracts.md, or add a one-line justification to contracts.md "Allowed dependencies". If you change a public API that contracts.md documents, update your crate's section of contracts.md with a targeted Edit.

Quality bar: production grade. Match Elasticsearch 9.5.5 behaviour exactly (exception types, reason texts, defaults, edge cases). When unsure, read Elasticsearch/Lucene source on GitHub (curl raw files into a fresh dir under /tmp/es-src/<topic>/, never execute it) or ask the real Elasticsearch in Docker. Elasticsearch source after 7.10 is not Apache-licensed: learn behaviour from it, never copy code. Lucene is Apache-2.0 but still write our own Rust. Performance matters: this must beat Lucene/Elasticsearch — no allocation in hot loops, cache-friendly layouts, SIMD where it pays (std::arch with runtime detection + tested scalar fallback). Every module starts with a \`//!\` doc explaining its design; comments explain why, sparingly. Thorough unit tests (edge cases, round trips, Elasticsearch-derived expected outputs captured as committed fixtures). Finish with \`cargo clippy -p <crate> --all-targets -- -D warnings\` and \`cargo test -p <crate>\` green.

Work as long as needed: this is a large crate and you should get as far as possible toward complete, correct and fast. If you run low on context, stop at a clean compiling state. Your final answer is a structured report (not prose for a human): what exists, the public API other crates use, what's still missing (concrete, ordered by importance), test status.`

const CRATES = [
  { key: 'analysis', crates: 'crates/search-analysis', prompt: `You own crates/search-analysis.
Implement Elasticsearch's complete text analysis (core + analysis-common module), per contracts.md \`search-analysis\`:
- Tokenizers: standard (Lucene StandardTokenizer, UAX#29 word break rules incl. its emoji/numeric/ideographic handling and max_token_length splitting), letter, lowercase, whitespace, uax_url_email, classic, thai, ngram, edge_ngram (token_chars, custom_token_chars), keyword (buffer_size), pattern (group, flags), simple_pattern, simple_pattern_split, char_group, path_hierarchy (delimiter, replacement, skip, reverse).
- Char filters: html_strip (escaped_tags), mapping (mappings/mappings_path rejected like a missing file), pattern_replace.
- Token filters: apostrophe, asciifolding (preserve_original), cjk_bigram, cjk_width, classic, common_grams, condition (script — define a hook trait so search-es can plug Painless later), decimal_digit, delimited_payload, dictionary_decompounder, edge_ngram, elision, fingerprint, flatten_graph, keep_types, keep, keyword_marker, keyword_repeat, kstem, length, limit, lowercase (+greek/irish/turkish), min_hash, multiplexer, ngram, the normalization filters (arabic, bengali, german, hindi, indic, persian, scandinavian, serbian, sorani, scandinavian_folding), pattern_capture, pattern_replace, porter_stem, predicate_token_filter (script hook), remove_duplicates, reverse, shingle, snowball, stemmer (every language name ES accepts incl. Lucene's light_/minimal_/possessive_ stemmers, implemented by us), stemmer_override, stop (all _lang_ lists, ignore_case, remove_trailing), synonym and synonym_graph (solr + wordnet formats, inline synonyms, lenient, expand, updateable), trim, truncate, unique, uppercase, word_delimiter, word_delimiter_graph. Reuse the in-repo \`snowball\` crate for snowball stemmers.
- Analyzers: standard, simple, whitespace, stop, keyword, pattern, fingerprint, and every language analyzer (arabic … thai) with their stopwords/stem_exclusion; custom analyzers; normalizers (custom with only per-character filters, as ES enforces); position_increment_gap; search_analyzer/search_quote_analyzer support.
- Settings validation with Elasticsearch's exact errors (unknown tokenizer/filter, wrong params, filters not allowed in normalizers, etc.).
- \`_analyze\` explain output (charfilters/tokenizer/tokenfilters stages with all token attributes ES shows: token, start_offset, end_offset, type, position, positionLength, bytes, keyword, termFrequency…) as data the REST layer renders.
- Graph token streams (position_length) so synonym_graph/word_delimiter_graph produce correct graphs for query-time phrase building.
Generate expected outputs by calling the real ES _analyze for a broad matrix (every component, tricky Unicode, CJK, emoji, URLs/emails, numbers) and commit them as JSON fixtures under crates/search-analysis/tests/fixtures/; a test asserts our output equals them. Performance: token streams must not allocate per token; add a simple throughput benchmark test (ignored by default) and report MB/s for standard analyzer vs what Lucene does (~tens of MB/s/core).` },
  { key: 'codec', crates: 'crates/search-codec', prompt: `You own crates/search-codec: the immutable segment format, per contracts.md \`search-codec\`. Design for maximum query speed and compactness, informed by Lucene 10's codecs (Lucene101PostingsFormat, BlockTree terms, Lucene90 doc values, BKD, stored fields, HNSW/scalar quantization) and by what search-benchmark-game shows (block-max pruning, bipartite reordering help most), but improve where you can.
Must include:
- Segment container: one file, header + sections + footer TOC + CRC32C; readers over a \`Slice\` (cheap-clone owned bytes view usable over memmap2::Mmap or Vec<u8>/bytes) — zero-copy, no whole-section copies.
- FieldInfos (index options docs/freqs/positions/offsets, norms, payloads, doc values type, points spec, vector spec, attributes).
- Term dictionary: fast seek_exact/seek_ceil, ordered iteration, prefix/automaton intersection (define the Automaton trait used by prefix, wildcard, regexp, fuzzy/Levenshtein — implement Levenshtein automata (with transpositions) and a Lucene-regexp→DFA compiler here or expose the trait so search-index can), term stats (doc_freq, total_term_freq), per-field stats (sum_doc_freq, sum_total_term_freq, doc_count), min/max term. fst crate allowed.
- Postings: docs as delta-coded 128-blocks with bitpacking/PFOR, freqs, positions, offsets, payloads in separate streams; low-df terms inlined; multi-level skip data carrying impacts (competitive (freq, norm) pairs) for block-max WAND/MaxScore; cursors: next/advance/advance_shallow/impacts; SIMD bit-unpacking with scalar fallback and equivalence tests.
- Norms (Lucene SmallFloat.intToByte4 encoding so BM25 matches ES exactly; sparse when not all docs have the field).
- Doc values: numeric (GCD/delta/table/bitpacked, sparse via an IndexedDISI-like jump structure), binary, sorted, sorted_set (prefix-compressed value dictionary + ordinals, lookupTerm/termsEnum on ords), sorted_numeric; random access + iterators; skip indexes (min/max per block) for range filtering.
- Points: BKD trees for 1–8 dims (numeric, date, ip, geo), bulk-loaded from sorted points, intersect(visitor with cell relations), estimate_point_count.
- Stored fields: blocks compressed with zstd (with a trained/shared dictionary option), fast random doc access with a small block cache; field visitor API.
- Vectors: float32/byte/bit, cosine/dot_product/l2_norm/max_inner_product; HNSW graph build (m, ef_construction) and search with an accept-docs filter and exact fallback; int8 scalar quantization (+ optional int4/BBQ) with rescoring.
- LiveDocs bitset object (separate, generational).
- Writing: \`SegmentSource\` trait pulled by \`write_segment\`; merges need a SegmentSource over N readers with a DocMap (deleted docs removed, optional reordering permutation) — provide that merge source here.
- Index sorting support (segment sorted by doc values keys).
Tests: round-trip property tests for every structure (random data, edge sizes 0/1/127/128/129/huge, sparse/dense), corrupted-file detection, SIMD vs scalar equivalence, and ignored-by-default micro benchmarks reporting decode throughput (ints/s) and seek latency. Report the final public API precisely — search-index will be written against it next.` },
  { key: 'painless', crates: 'crates/search-painless', prompt: `You own crates/search-painless, per contracts.md \`search-painless\`. Use the shared ES; use \`POST /_scripts/painless/_execute\` and update/script_fields/script_score requests to capture expected outputs and error shapes as committed fixtures.
Implement the Painless language completely and fast:
- Lexer/parser for the full grammar (Java-like: declarations with types and \`def\`, all operators incl. \`?.\`, \`?:\`, \`=~\`, \`==~\`, \`instanceof\`, casts and implicit/explicit conversion rules, compound assignment, ++/--, ternary, list/map initializers \`[]\`/\`[:]\`, array creation, string/char literals, regex literals /.../flags, lambdas, method references, functions declared in the script, if/else, while/do/for/for-each, break/continue, return, throw, try/catch).
- Semantic analysis: static types + def dynamic dispatch, Painless casting rules, boxing, the allowlisted API (java.lang: Object, String (all common methods), Math, Integer/Long/Double/Float/Boolean/Character/Byte/Short, StringBuilder; java.util: List/ArrayList/Map/HashMap/Set/HashSet/Collections/Arrays/Iterator/Optional/stream basics used in scripts, Objects; java.util.regex Pattern/Matcher; java.time: Instant, ZonedDateTime, LocalDate(Time), Duration, ChronoUnit, ZoneId/ZoneOffset, DateTimeFormatter, temporal arithmetic; java.util.function lambdas), ES extensions (ScriptDocValues: doc['f'].value/.values/.size()/.empty, getValue, dates as ZonedDateTime, GeoPoint, JodaCompatibleZonedDateTime behaviours, emit() for runtime fields, decay functions decayNumericLinear/Exp/Gauss, decayGeo*, decayDate*, saturation, sigmoid, randomScore, cosineSimilarity/dotProduct/l1norm/l2norm for vectors, Debug.explain).
- Compiler to a compact register/stack bytecode + VM (or closure compilation) — no AST walking in hot loops; script_score over millions of docs must be fast. Compiled-script cache with ES limits.
- Script contexts with their variables & return types: update (ctx._source, ctx.op, ctx._now, ctx._index…), update_by_query/reindex, ingest (ctx as the doc), score (_score, doc, params), field/script_fields (doc, params, _source access), filter, aggs (map/combine/reduce scripted_metric states), sort, number_sort/string_sort, terms_set, runtime fields (emit), similarity, painless_test (_execute with context_setup).
- Limits: script.painless.regex.enabled=limited with limit factor, max loop counter (1,000,000), max size.
- Errors exactly as ES: compile errors → script_exception with script_stack, script, lang, position {offset,start,end}, caused_by (illegal_argument_exception reasons etc.); runtime errors (NPE, ClassCast, index out of bounds) with ES's texts.
- Mustache (lang: mustache) for search templates: sections, inverted sections, {{#toJson}}, {{#join}}, {{#url}}, escaping rules as ES.
Host interface: a \`Host\` trait (doc values access, _source access, score, params, ctx) so search-es can wire it; values as an efficient enum. Exhaustive tests from fixtures plus language unit tests.` },
  { key: 'store', crates: 'crates/search-store', prompt: `You own crates/search-store, per contracts.md \`search-store\` and architecture.md (write path, idle cost, branching). Study crates/redis-server/src/persist.rs (Shipper group commit, manifests, branching via base/history, collect_garbage, test_objects) and crates/stream-log (fsync group commit) and crates/pg-storage/src/objects.rs, and follow those patterns.
Implement:
- The node WAL: one append stream shared by every shard of every cluster on the node; group commit every wal_flush (5 ms) or 4 MiB to local segment files with fsync; \`append(cluster, shard, bytes) -> Lsn\`, \`durable(lsn)\` future (local durability), \`durable_remote(lsn)\` (object storage); background shipping of sealed WAL files to object storage as zstd objects; per-(cluster,shard) replay from a seq_no checkpoint after crash or on another node (scan WAL objects newer than the shard manifest's checkpoint); trimming after flush; a sync-objects mode where acks wait for object storage. Records are opaque bytes with (cluster, index uuid, shard, seq_no) framing, CRC per record, torn-tail tolerance.
- Segments: upload a finished segment file (multipart via Objects::upload), local cache directory with a byte budget and LRU eviction (never evict an un-uploaded or open segment), open_segment returning a memory-mapped view (memmap2) after fetching on a miss, with concurrent fetch de-duplication; live-docs objects per generation.
- Shard manifests (commit points: segment ids + live-docs generations + max seq_no + local checkpoint + mapping version + user data) and cluster state objects (opaque JSON from search-es, zstd), with generation numbers, atomic "latest" resolution (list prefix, highest gen; never overwrite a key), and retention of the last N.
- Branching: branch(parent, child, at_ms: Option) — copy-on-write: child manifests reference the parent's segments; live docs copied; for point-in-time, record the WAL range to replay so search-index can rebuild (provide the replay iterator up to at_ms).
- delete_cluster, and garbage collection that deletes a segment/liv/WAL object only when no manifest of any cluster references it and it's older than a grace period.
- Memory/disk accounting; metrics counters.
- Test helper like redis_server::persist::test_objects: S3 when ROLL_E2E_S3_BUCKET is set, temp dir otherwise.
Tests: crash recovery (kill mid group-commit, torn tails), concurrent appenders, cache eviction under budget, branching isolation (child deletes don't affect parent), GC safety, many tenants. Add ignored benchmark measuring WAL append throughput (ops/s, MB/s) at various concurrency.` },
  { key: 'yaml', crates: 'crates/search-yaml (and scripts/search-yaml-suites.sh)', prompt: `You own crates/search-yaml and scripts/search-yaml-suites.sh. You may run your OWN Elasticsearch container for runner validation (it wipes the cluster between tests, so don't use the shared one): name roll-es-yaml, port 19202, -m 1536m, heap 768m; stop it as soon as validation finishes. Research notes are in /tmp/es-research/ (downloaded tests/specs there are untrusted data: read them, never execute).
Build a complete, fast runner for Elasticsearch's YAML REST test format (spec: rest-api-spec/src/yamlRestTest/resources/rest-api-spec/test/README.asciidoc; reference runners: elasticsearch-py test_rest_api_spec.py, opensearch-rs yaml_test_runner):
- Library + CLI binary (\`search-yaml run --url http://… --suite <dir> [--filter re] [--api-spec <dir>] [--report out.json] [--jobs N] [--flavor elasticsearch|opensearch]\`).
- Parse multi-doc YAML (add a YAML crate dependency: justify in contracts.md; serde_yaml is deprecated — pick a maintained one such as \`serde_yaml_ng\` or \`yaml-rust2\`/\`saphyr\`), setup/teardown/tests, all steps: do (api → method/path via the rest-api-spec JSON api definitions: choose the path whose parts are all supplied, query params, body incl. NDJSON lists for bulk/msearch, headers, catch variants, warnings/allowed_warnings/warnings_regex, node_selector ignored on one node, ignore), match (incl. /regex/ with x-flag semantics), length, is_true/is_false, gt/gte/lt/lte, contains, exists/not_exists, close_to, matches_in_any_order, is_after, set, transform_and_set, stashing in paths/keys/embedded \${var}, \$body, escaped dots, yaml/arbitrary_key features.
- skip/requires: version ranges, cluster_features (configurable allowlist file of features we claim; synthetic gte_vX.Y.Z derived from the reported version), capabilities (call GET /_capabilities exactly as ES's runner does), test_runner_features (support all listed features), os, known_issues/awaits_fix.
- Between tests: wipe the cluster like ESRestTestCase.wipeCluster (indices incl. hidden, data streams, composable/component/legacy templates (not built-in ones), ingest pipelines, stored scripts, cluster settings, snapshot repos, ILM where present) — configurable so a roll target can instead use a fresh cluster per file via a hook (e.g. an option to call a "new cluster" HTTP endpoint and use returned credentials) enabling parallel files.
- Reports: per-test pass/fail/skip with failure detail (step, expected vs actual, response body excerpt), JSON + human summary grouped by API area, and comparison against a baseline list.
- scripts/search-yaml-suites.sh: downloads into target/search-yaml/: the official rest-resources zip for 9.5.5 (https://artifacts.elastic.co/downloads/elasticsearch/rest-resources-zip-9.5.5.zip — test/free + rest-api-spec/api; do NOT commit these, they aren't Apache-licensed), the OpenSearch 3.9.0 rest-api-spec tests + api (Apache-2.0). Vendor elastic/elasticsearch-clients-tests (Apache-2.0, matching 9.5 branch) under crates/search-yaml/suites/clients-tests/ with its LICENSE/NOTICE and a ROLL.md stating source commit.
- Validate the runner against the real Elasticsearch 9.5.5 in Docker: run test/free (and clients-tests). Every failure on real ES is a runner bug or an environment limit (multi-node, x-pack/license, snapshots repo path, etc.) — fix runner bugs until real ES passes essentially everything that can pass on one node. Commit crates/search-yaml/baseline/es-9.5.5.txt: the ids (file path + test name) of tests that pass on real ES, plus a short environment-skip list with reasons. That baseline is roll's target.
- Make it fast (parallel files with --jobs when the target supports isolated clusters).
Tests: unit tests for parsing, stash, matching semantics, path selection. Report the pass/fail numbers on real ES.` },
  { key: 'es-meta', crates: 'crates/search-es and crates/search-server', prompt: `You own crates/search-es and crates/search-server for this phase.
Build the REST layer foundation and the metadata half of Elasticsearch; the document/search half comes in the next phase once search-index exists (don't depend on search-index/search-aggs/search-painless yet — remove them from search-es's Cargo.toml for now if they block compiling; search-store is being written concurrently, so persist through a small trait you define for now and keep an in-memory implementation).
search-es:
- Vendor Elasticsearch's REST API spec JSON (Apache-2.0; the Maven artifact org.elasticsearch:rest-api-spec:9.5.5 from repo1.maven.org contains exactly the api/*.json files) under crates/search-es/spec/ with ROLL.md (source, license). Build a fast route table from it (build.rs codegen or startup parse — choose the fastest request-time matching), with methods, path parts, and typed query params; unknown params → ES's illegal_argument_exception "request [...] contains unrecognized parameter: [x]" (with its did-you-mean); wrong methods → 405 with ES's body; unknown paths → ES's 400 "no handler found" shape.
- Request/response framing: EsRequest/EsResponse types; compat media types (compatible-with 7|8|9, space-insensitive, Accept lists), X-Elastic-Product on every response (elasticsearch dialect), OpenSearch dialect switch; common params: pretty, human, error_trace, filter_path (full ES semantics incl. wildcards, ** and exclusions), format=json|yaml|cbor|smile? (json+yaml at least), source/source_content_type in query string; gzip request/response; HEAD semantics; X-Opaque-Id echo; deprecation Warning headers with ES's exact text format (the YAML tests assert warnings).
- EsError: every exception type ES returns for these APIs with exact reason texts and extra fields; root_cause; caused_by; error_trace stack omission; status codes.
- Cluster metadata model (one tenant): indices (name validation rules + texts, uuid, creation_date, version.created, settings with defaults, index.* setting validation incl. unknown setting errors, number_of_shards/replicas/routing_shards, closed indices, blocks (read_only, write, metadata...), hidden), mappings (complete: every field type ES 9 core supports: text, match_only_text, keyword, constant_keyword, wildcard, numeric types incl. scaled_float/unsigned_long/half_float, date/date_nanos with formats (ES's java-time format names & patterns), boolean, binary, ip, ip_range & ranges, object/flattened/nested (include_in_parent/root, limits), join, alias, geo_point/geo_shape/point/shape, dense_vector/sparse_vector/rank_feature(s), completion, search_as_you_type, token_count, percolator(reject later), version, murmur3(plugin—no), semantic_text (reject), runtime fields; all params with validation & ES errors; multi-fields; copy_to; _source (enabled/includes/excludes/mode synthetic), _routing required, _meta, dynamic (true/false/strict/runtime), dynamic_templates (match/unmatch/path_match/match_mapping_type/match_pattern), date_detection/dynamic_date_formats, numeric_detection; mapping merge rules and conflict texts; total_fields/depth/nested limits; serialization exactly as GET _mapping prints (defaults omitted as ES does)). Implement dynamic mapping inference from a JSON document as a pure function (doc → mapping update) for the next phase.
- Aliases (filters, index/search routing, is_write_index, is_hidden, must_exist), composable index templates + component templates + legacy templates (priority, overlap validation & errors, simulate APIs, data_stream flag stored), index resolution (wildcards, expand_wildcards, ignore_unavailable, allow_no_indices, date math <name-{now/d}>, exclusions with -, remote cluster syntax rejected), cluster settings (persistent/transient, validation), stored scripts storage, ingest pipeline storage (CRUD + validation of processor names), index blocks.
- APIs (exact responses): GET / and HEAD /, indices.create/delete/exists/get/get_mapping/put_mapping/get_field_mapping/get_settings/put_settings/open/close/add_block/get_alias/put_alias/delete_alias/exists_alias/update_aliases/resolve_index/get_index_template/put_index_template/delete/exists/simulate_index_template/simulate_template/component templates/legacy templates/stats (shape with zeros for now), cluster.health/state (filtered metrics)/stats/get_settings/put_settings/allocation_explain basics, nodes.info/stats basic shape, _capabilities, cat.* (indices, aliases, health, nodes, shards, templates, count stub, master, allocation… with v, h, s, help, bytes, time, format=json/yaml/txt and ES's column sets/aliases), put/get/delete_script, ingest.put/get/delete_pipeline. Document/search APIs return a clear placeholder error for now.
search-server:
- Node with tenants (Catalog trait; in-memory catalog for now), HTTP/1.1 on hyper with keep-alive/pipelining, auth & tenant routing per architecture.md (Basic, ApiKey, Bearer, Host/SNI label; constant-time compare; default single-cluster dev mode without auth), body size limit, request concurrency per cluster, idle-unload skeleton (unload/wake hooks; zero background work when nothing is loaded), TLS (reuse redis_server::net patterns or copy small helpers).
- A dev binary \`search-dev\` (src/bin) serving one cluster on a port with no auth, so the YAML runner can target it: \`search-dev --listen 127.0.0.1:59200\`.
- Integration tests in crates/search-server/tests/ hitting the HTTP API (copy the Capped 1 GiB allocator guard from crates/stream-broker/tests/common).
Verify behaviour against the real ES for each API and capture tricky responses as fixtures.` },
]


const REPORT = {
  type: 'object',
  properties: {
    summary: { type: 'string' },
    public_api: { type: 'string', description: 'precise public API other crates use' },
    done: { type: 'array', items: { type: 'string' } },
    remaining: { type: 'array', items: { type: 'string' }, description: 'concrete missing work, most important first; empty if complete' },
    tests: { type: 'string', description: 'test/clippy status and counts' },
    notes_for_others: { type: 'array', items: { type: 'string' } },
  },
  required: ['summary', 'public_api', 'done', 'remaining', 'tests', 'notes_for_others'],
}
const FINDINGS = {
  type: 'object',
  properties: {
    findings: { type: 'array', items: { type: 'object', properties: {
      title: { type: 'string' }, file: { type: 'string' }, detail: { type: 'string' }, severity: { type: 'string', enum: ['critical', 'major', 'minor'] },
    }, required: ['title', 'file', 'detail', 'severity'] } },
  },
  required: ['findings'],
}


// ---- phase B ----
const REPORTS = '~/.cache/roll-search/reports'

const INDEX = { key: 'index', crates: 'crates/search-index', prompt: `You own crates/search-index: one shard's engine, per contracts.md \`search-index\` and architecture.md (write path, read path, idle cost). It knows nothing about JSON or mappings: search-es turns documents into IndexableDocs and the Query DSL into Queries. Build on search-codec (read its crate docs and the report ${REPORTS}/codec.json) and search-analysis (report ${REPORTS}/analysis.json). If a codec API is missing something you need, make a small additive change in search-codec with tests (you may edit search-codec for that only; keep its tests green).
Implement, production-grade and faster than Lucene:
- Field model/schema, IndexableDoc (tokens from analyzers or pre-analyzed, terms, points, doc values, stored fields, vectors, nested child docs as a block: children before parent, parent bitset).
- Indexing: concurrent in-memory buffers (per-thread writers like Lucene's DWPT for throughput) implementing codec SegmentSource; RAM accounting; refresh = turn buffers into immutable segments readable through codec readers (in memory), with a new point-in-time reader; flush = hand finished segment files to the caller (search-store uploads them; search-index must not depend on search-store — expose paths/bytes and accept a caller-provided directory) ; open from existing segments + live docs.
- Deletes/updates by _id (term), live docs generations, soft-delete style tombstones as needed for realtime get; live version map (id → seq_no, primary_term, version, location) serving realtime get and optimistic concurrency checks (expose outcomes; search-es renders ES errors); seq_no/local checkpoint tracking.
- Merges: tiered merge policy with Lucene/ES defaults (index.merge.policy.*), bounded background merge threads that run only while there's work, force merge (max_num_segments, only_expunge_deletes), BP (bipartite graph partitioning) doc reordering on merges when no index sort, index sorting (index.sort.*).
- Searching: Query/Weight/Scorer/Collector model with two-phase iteration, score modes, query rewriting (multi-term rewrite methods: constant_score_blended (default), constant_score, constant_score_boolean, scoring_boolean, top_terms_N, top_terms_boost_N, top_terms_blended_freqs_N), query caching of filters only if it pays.
- Every query in contracts.md: term, term-in-set, boolean (MaxScore/WAND block-max for top-k disjunctions, block-max conjunctions, minimum_should_match, coord-free like Lucene 10), phrase/multi-phrase with slop (sloppy phrase scoring as Lucene), span family (term, near, or, not, first, containing, within, multi, field_masking), intervals (match, prefix, wildcard, fuzzy, all_of, any_of, filters), automaton queries (prefix, wildcard, regexp with Lucene syntax and flags, fuzzy with transpositions/prefix_length/max_expansions), point and doc-values ranges (with IndexOrDocValues choosing the cheaper), exists, match_all/none, constant_score, boost, dis_max (tie_breaker), function score (weight, field_value_factor, decay gauss/exp/linear on numeric/date/geo, random_score with seed+field, script hook; score_mode/boost_mode/max_boost/min_score), script score (callback trait), kNN (approximate HNSW with k/num_candidates/filter/similarity threshold, exact brute force, rescoring of quantized), nested block-join (to-parent with score_mode none/avg/sum/min/max, to-child), geo (distance, bounding box, polygon, shape relations for geo_point; geo_shape basic), more_like_this, term-in-set for ids, doc-values term filters.
- Similarities: BM25 exactly as Lucene 10 (idf = ln(1 + (N - n + 0.5)/(n + 0.5)), docCount-based avgdl from field stats, norm decoding cache table, k1/b per field, discount_overlaps), plus DFR, DFI, IB, LMDirichlet, LMJelinekMercer, boolean, and a scripted-similarity hook — with Explanation trees whose values and descriptions match Lucene's text exactly (ES explain tests compare them).
- Collectors: top score docs with total-hits threshold (track_total_hits semantics: true/false/N, relation eq/gte), top field docs (multi-key sort on score/_doc/numeric/keyword/date doc values, missing _first/_last/value, mode min/max/sum/avg/median, nested sort with filter/max_children, unmapped_type handled by caller), search_after, early termination on index sort, terminate_after, timeout checks, min_score, field collapsing (collapse on keyword/numeric with inner hits hook), rescoring (query rescorer: window_size, query_weight, rescore_query_weight, score_mode), multi-collector composition, a parallel per-segment search option for large segments.
- Highlighting: unified highlighter (default; offsets from postings when index_options=offsets, from term vectors, or re-analysis), plain, fvh (served by unified with the same output where ES does); fragment_size, number_of_fragments (0 = whole field), no_match_size, pre/post_tags, tags_schema styled, encoder html, order score, boundary_scanner sentence/word/chars with boundary_chars/max_scan/locale, fragmenter simple/span, require_field_match, highlight_query, matched_fields, max_analyzed_offset; term vectors storage and retrieval for the termvectors API (term stats, field stats, positions, offsets, payloads).
- Zero idle cost: no thread or timer when nothing is being written or merged; memory accounting per shard.
Tests (must cover everything): for every query type, a randomized oracle test comparing results (doc sets AND scores) against a brute-force evaluator over random corpora and random segment layouts (multiple segments, deletes, merges); BM25 numbers checked against hand-computed Lucene values and against the real Elasticsearch via \`_explain\` on the shared reference ES (prefix indices "index-"); concurrency tests for indexing/refresh/merge/search races; crash-free behavior on huge/hostile inputs. Benchmarks (ignored by default): indexing docs/s, refresh latency, query latency for term/AND/OR/phrase/top-k on a synthetic Zipfian corpus; report numbers.` }

const AGGS = { key: 'aggs', crates: 'crates/search-aggs', prompt: `You own crates/search-aggs, per contracts.md \`search-aggs\`: Elasticsearch aggregations — parsing (ES JSON with ES's exact parse errors), per-segment collection over search-index doc values (implement search-index's Collector), shard partials, cross-shard (and batched partial) reduce, and rendering (keyed, typed_keys, format/value_as_string, meta). Field metadata and scripts through traits search-es implements (FieldLookup, a script hook). Read search-index's crate docs first.
Implement all of: bucket — terms (shard_size, order by _count/_key/sub-agg incl. multi-level paths, min_doc_count, shard_min_doc_count, include/exclude (regex, exact lists, partitions), missing, execution_hint map/global_ordinals, collect_mode breadth_first/depth_first, show_term_doc_count_error, doc_count_error_upper_bound and sum_other_doc_count exactly as ES), multi_terms, significant_terms (JLH, mutual_information, chi_square, gnd, percentage, script_heuristic; background_filter), significant_text, rare_terms, histogram, date_histogram (calendar_interval/fixed_interval, time_zone with DST, offset, extended_bounds, hard_bounds, min_doc_count, keyed, format, missing), auto_date_histogram, variable_width_histogram, range, date_range, ip_range, ip_prefix, filter, filters (keyed, other_bucket, other_bucket_key), adjacency_matrix, global, missing, nested, reverse_nested, children/parent, composite (terms/histogram/date_histogram/geotile_grid sources, missing_bucket, order, after, after_key, index-sort early termination), sampler, diversified_sampler, random_sampler, geohash_grid, geotile_grid, geo_distance; metrics — avg, sum (Kahan), min, max, value_count, stats, extended_stats (sigma, variance variants), cardinality (HyperLogLog++ with ES's precision_threshold, murmur3 hashing and linear counting so counts match ES exactly), percentiles and percentile_ranks (ES's TDigest (MergingDigest, compression, execution_hint) and HDR so numbers match ES), median_absolute_deviation, top_hits (via a fetch hook), top_metrics, weighted_avg, scripted_metric (script hook), geo_bounds, geo_centroid, string_stats, boxplot, rate, matrix_stats; pipelines — avg/sum/min/max/stats/extended_stats/percentiles_bucket, bucket_script, bucket_selector, bucket_sort, cumulative_sum, cumulative_cardinality, derivative, moving_fn (MovingFunctions incl. ewma/holt/holtWinters, linearWeightedAvg, unweightedAvg, stdDev, max, min, sum), serial_diff, normalize; gap_policy skip/insert_zeros/keep_values; buckets_path syntax (>, ., [key], _count, _key, _bucket_count). search.max_buckets limit (65536) with ES's too_many_buckets_exception. Memory accounting so a cluster can't blow the node.
Tests (must cover everything): randomized oracle tests per aggregation against a brute-force evaluator over random docs (incl. multi-segment, multi-shard reduce vs single-shard), and fixtures captured from the shared reference Elasticsearch (prefix "aggs-") for exact numbers/rendering (TDigest, HLL, date_histogram time zones, error texts). Benchmarks (ignored): terms/date_histogram/cardinality over millions of docs.` }

const byKey = Object.fromEntries(CRATES.map(c => [c.key, c]))

async function run(prompt, opts) {
  for (let i = 0; i < 8; i++) {
    const r = await agent(prompt, opts)
    if (r) return r
    log(`${opts.label}: no result (rate limit?), retry ${i + 1}`)
    if (typeof setTimeout === 'function') await new Promise(res => setTimeout(res, 300000))
  }
  return null
}

async function build(c, intro, rounds) {
  let report = await run(`${COMMON}\n\n## Your task (${c.key})\n${intro}\n\nAssignment:\n${c.prompt}`, { label: `build:${c.key}`, phase: 'Build', schema: REPORT, effort: 'high' })
  for (let r = 2; report && report.remaining.length > 0 && r <= rounds; r++) {
    const next = await run(`${COMMON}\n\n## Your task (${c.key}, round ${r})\nYou continue work on ${c.crates}. Original assignment:\n${c.prompt}\n\nPrevious report:\n${JSON.stringify(report, null, 1)}\n\nRead the current code first, then complete the remaining items (most important first), fixing anything you find broken. Tests must cover everything important. Keep the crate compiling and tests green.`, { label: `build:${c.key}:r${r}`, phase: 'Build', schema: REPORT, effort: 'high' })
    if (!next) break
    report = next
  }
  return report
}

async function reviewFix(c, report) {
  const review = await run(`${COMMON}\n\n## Review task (${c.key})\nAdversarially review ${c.crates}. Do NOT edit code. Find real defects: behaviour that differs from Elasticsearch 9.5.5 (verify against ES source or the shared reference ES at 127.0.0.1:19200 using names prefixed review-${c.key}-), correctness bugs (panics on peer input, unbounded allocations, data loss, races, off-by-one), performance problems in hot paths, important behaviour without tests, and violations of AGENTS.md / contracts.md. Run the crate's tests. Only report findings you have verified with a concrete failing input or code path.\n\nAssignment was:\n${c.prompt}\n\nOwner report:\n${JSON.stringify(report, null, 1)}`, { label: `review:${c.key}`, phase: 'Review', schema: FINDINGS, effort: 'high' })
  const fs = (review && review.findings) || []
  if (!fs.length) return { key: c.key, report, findings: [] }
  const fix = await run(`${COMMON}\n\n## Fix task (${c.key})\nYou own ${c.crates}. A reviewer reported these verified findings. For each: confirm it, fix it with a regression test, or explain why it's not a defect. Then make sure clippy and tests are green.\n\nFindings:\n${JSON.stringify(fs, null, 1)}\n\nOwner report for context:\n${JSON.stringify(report, null, 1)}`, { label: `fix:${c.key}`, phase: 'Fix', schema: REPORT, effort: 'high' })
  return { key: c.key, report: fix || report, findings: fs }
}

const savedIntro = k => `You continue work on ${byKey[k] ? byKey[k].crates : k}. The previous engineer's latest report is ${REPORTS}/${k}.json (read it first; its "remaining" list is your starting point), or, if that file doesn't exist, the previous engineer was interrupted before reporting: read the code and run the tests to learn the state first (fix half-finished edits).`

// Three lanes; each runs its work in order, so at most 3 agents run at once (RAM, rate limits).
const lanes = [
  async () => {
    const out = []
    const ri = await build(INDEX, 'Start the crate (only a stub exists).', 5)
    out.push(await reviewFix(INDEX, ri))
    const ra = await build(AGGS, 'Start the crate (only a stub exists). search-index is now built; read its docs and public API first.', 4)
    out.push(await reviewFix(AGGS, ra))
    return out
  },
  async () => {
    const out = []
    const re = await build(byKey['es-meta'], savedIntro('es-meta'), 3)
    out.push(await reviewFix(byKey['es-meta'], re))
    const ry = await build(byKey['yaml'], savedIntro('yaml'), 2)
    out.push(await reviewFix(byKey['yaml'], ry))
    return out
  },
  async () => {
    const out = []
    const rp = await build(byKey['painless'], savedIntro('painless'), 3)
    out.push(await reviewFix(byKey['painless'], rp))
    out.push(await reviewFix(byKey['codec'], { note: `see ${REPORTS}/codec.json` }))
    out.push(await reviewFix(byKey['analysis'], { note: `see ${REPORTS}/analysis.json` }))
    return out
  },
]
const results = await parallel(lanes)
return results.filter(Boolean).flat()
