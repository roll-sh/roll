# roll search: Elasticsearch compatibility

How closely roll search (`crates/search-es`, served by `search-dev` / `rolld`) follows
Elasticsearch 9.5.5. Two sources of truth:

1. **The Elasticsearch YAML REST suites** (`rest-resources` `test/free`, plus
   `elasticsearch-clients-tests`), run by `crates/search-yaml` against roll. The baseline is
   the set of tests that pass on a real single-node Elasticsearch 9.5.5
   (`crates/search-yaml/baseline/es-9.5.5.txt`); a test counts for roll when it passes there.
2. **Differential fixtures**: request sequences replayed against roll and compared with the
   responses a real Elasticsearch 9.5.5 gave, byte for byte after masking values that differ
   by nature (`took`, UUIDs, timestamps): `crates/search-es/tests/rest_fixtures.rs`
   (`tests/fixtures/rest`, admin APIs) and `crates/search-es/tests/doc_fixtures.rs`
   (`tests/fixtures/docs`: documents, OCC, update, bulk, search, aggregations, ingest,
   by-query APIs). Re-capture with `ROLL_ES_CAPTURE=<es url> cargo test -p search-es --test
   doc_fixtures -- --ignored`.
3. **The differential suite and the client matrix** (below): seeded random workloads against
   roll and a real Elasticsearch side by side, and the unmodified official clients against
   search-dev.

Reproduce the YAML numbers with `scripts/search-yaml-suites.sh roll` (set `ROLL_PORT`).

## Totals

- Tests run: 3905; passed 3809, failed 1, skipped 95.
- Of the 3808 tests that pass on Elasticsearch 9.5.5 (and run here), roll passes **3808** (100%); every x-pack clients test passes.
  <!-- test-search: roll >= 3809 -->
- Module suites the `rest-resources` artifact doesn't ship, run from the Elasticsearch 9.5.5
  sources (`modules/*/src/yamlRestTest`): `parent-join` 24 of 24 (3 more are skipped by
  Elasticsearch itself), `percolator` 6 of 6; from the x-pack plugin sources, `rank-rrf` 96 of
  139 (the other 43 need inference endpoints or a non-trial license) and
  `search-business-rules` 16 of 16. The other module suites of the sources
  (`runtime-fields-common`, `lang-painless`, `ingest-common`, `lang-mustache`, `mapper-extras`,
  `rank-eval`, `user-agent`, `ingest-attachment`, `lang-expression`, `dot-prefix-validation`,
  `rest-root`): roll passes 581 of the 760 tests that pass on a private Elasticsearch 9.5.5 —
  `painless` 195 of 209 (script cancellation by search timeouts and client disconnects, loop
  statement limits per script context as Elasticsearch 9.5 sets them, `_termStats`, interval
  script filters, runtime fields read through `doc[...]`), `runtime_fields` 169 of 245 (composite
  sub-fields, mapping runtime fields shadowing concrete ones, every query type with each type's
  value parsing, sorts with `mode`/`missing`, request `runtime_mappings` validated per shard),
  `ingest` 90 of 149; missing:
  index-time scripts (`script` on mapped fields), `geo_point` runtime fields in geo queries,
  `geo_bounds` and distance sorts, `params._fields`, nested runtime field errors as cause
  chains, the `registered_domain`, `user_agent` and `attachment` processors, ingest node
  statistics and verbose simulate details.
- The `rest-resources` `test/platinum` suites of the default distribution's free x-pack mappers
  and aggregations (not part of the baseline above; compared with the 307 of their tests that
  pass on a private Elasticsearch 9.5.5 with security off and a trial license): roll passes 270
  — `aggregate-metrics` 26 of 26, `analytics` 103 of 104 (usage stats), `constant_keyword` 12 of
  13, `wildcard` 21 of 21, `spatial` 85 of 97, `data_streams` 9 of 11 (rollup), `counted_keyword`
  6 of 6, `versionfield` 6 of 6, `mustache` 2 of 11 and `painless` 0 of 11 (Watcher), `logsdb` 0
  of 1 (usage stats). Missing besides: `geo_shape` runtime fields (10 `spatial` tests),
  `geo_bounds` of shapes crossing the dateline,
  dateline-crossing `geohex` cells as multipolygons in the `geo_grid` processor, synthetic
  `_source` order of `counted_keyword` values.
- The OpenSearch 3.9.0 suite (opensearch dialect): 1455 of 1506 pass — every test that passes
  on a real single-node OpenSearch 3.9.0 (`crates/search-yaml/baseline/opensearch-3.9.0.txt`,
  re-checked against OpenSearch 3.9.0 in Docker); the other 51 are environment skips
  (`opensearch-3.9.0.skips.txt`). Dialect differences are also pinned by differential fixtures
  captured from OpenSearch (`tests/fixtures/rest/os_*.json`), and the unmodified opensearch-py
  and opensearch-js clients pass their product check and scenario (`tests/search/clients`).
  <!-- test-search: roll-os >= 1455 -->
- Beyond the YAML suite, the opensearch dialect implements the k-NN plugin every OpenSearch
  distribution bundles: `knn_vector` mappings (validation and printed form as OpenSearch's,
  flat fields without `index.knn`), derived `_source` vectors, the `knn` query (per-segment `k`
  on the native engines, lucene's global `k`, filters, radial `max_distance` / `min_score`,
  float / byte / binary / half_float vectors, l2 / cosinesimil / innerproduct / hamming) and the
  `knn_score` script, pinned by `os_knn.json` (208 steps captured from OpenSearch 3.9.0);
  k-NN explanations (approximate, exact filtered, radial, disk-based; faiss and lucene engines),
  `_validate` texts and profiles (`KNNQuery`, `LuceneEngineKnnVectorQuery`, concurrent segment
  search for k-NN queries as the plugin's decider votes) by `os_knn_explain.json` (33 steps);
  the node APIs `_plugins/_knn/stats` (every stat, filters, node selection, request counters
  and the graph-cache ledger), `warmup` and `clear_cache` by `os_knn_api.json` (60 steps).
  Not yet: `l1` / `linf` approximate search (only through nmslib, rank by L2), trained models
  (`model_id`, `_plugins/_knn/models/*`), the k-NN painless functions (`l2Squared`, …).
- The bundled neural-search plugin's hybrid search: the `hybrid` query (up to five
  sub-queries, `filter` pushed into each, `pagination_depth`), the search pipelines'
  `normalization-processor` (`min_max` with lower / upper bounds, `l2`, `z_score`, `rrf`;
  arithmetic / geometric / harmonic means with weights) and `score-ranker-processor` (`rrf`),
  named, inline or index-default; the raw per-shard format without a processor, totals and
  aggregations over the sub-queries' union, `min_score`, field sorts, `explain` (with the
  plugin's double normalization) and `hybrid_score_explanation`, `HybridQuery` profiles, and
  the errors (nested hybrid, scroll, dfs, `from` without `pagination_depth`, ...): pinned by
  `os_hybrid.json` (111 steps on one and three shards, float-exact against OpenSearch 3.9.0).
  `collapse` picks the same groups but scores them from the full sub-query lists (the plugin
  normalizes collapsed lists).
- `_id` fielddata (`indices.id_field_data.enabled`, on by default in OpenSearch, off in
  Elasticsearch): sorts, `search_after`, keyword aggregations (`terms`, `composite`,
  `multi_terms`, `cardinality`, `value_count`, `significant_terms`; `min`/`range` refuse the
  `_id` type), `docvalue_fields` and, in OpenSearch, `doc['_id']` in scripts read `_id` through
  sorted doc values rebuilt from its terms on first use (search-codec `UNINVERTED` fields, no
  index-time cost). Elasticsearch adds its deprecation warnings and keeps refusing scripts.
  Collapse takes keywords and numbers only in both dialects, with each dialect's message.
  Scripts of aggregations, runtime fields and `_execute` follow the setting too; `_id`
  fielddata shows in fielddata stats and `_cache/clear?fielddata` releases it from them.
- Sorting across indices that map a field differently: per-shard value formats,
  `numeric_type: long` truncating floating fields, Elasticsearch's `SortFieldValidation`
  (integral and floating types merge as doubles, other mixes fail), the `unsigned_long` mix
  refused in both dialects, and OpenSearch's cast failures (`es_sortmix_meta.json`, 124
  steps; `os_fixes_r4.json`). Data stream backing indices are named without a date in the
  OpenSearch dialect. OpenSearch's `filter_path` exclusions drop the containers they leave
  empty, its `fields` option skips metadata fields, and field sorts take no `format` there.
  Script fields leave `_source` out unless asked for (search and `top_hits`, which now runs
  its `script_fields`). Pinned by `os_id_fielddata.json` (73 steps) and `es_id_fielddata.json`
  (65 steps, captured from a private Elasticsearch 9.5.5 since they change a cluster setting).
- Index State Management (`_plugins/_ism/*` and `_opendistro/_ism/*`): policy CRUD with the
  plugin's normalized form, sequence-number concurrency and every validation message,
  `ism_template` conflicts and auto-management of new indices and backing indices, and
  `add`/`remove`/`change_policy`/`retry`/`explain` over index expressions, pinned by
  `os_ism.json` (115 steps from a fresh OpenSearch 3.9.0). Every cluster state change is
  swept like the plugin's coordinator (resized, restored, rolled-over and data stream indices
  are managed or released; templates apply to indices created after their last update;
  exclusion patterns; the restricted config index), templates are validated and conflict
  checked by pattern-automaton intersection, explain-all and the policy list honour
  `size`/`from`/`sortField`/`sortOrder`/`queryString` with the plugin's errors, and the action
  parsers' validations match (`os_fixes_r4.json`, 363 steps). Policies are not executed: explain
  reports the state OpenSearch reports before the plugin's first job run (a serverless cluster
  runs no background sweeps), and long explicit index lists may print in another order.
- OpenSearch search request extras (`os_search_ext.json`, 21 steps): `phase_took`,
  `cancel_after_time_interval`, the bundled plugins' `ext` sections (`rerank`, `ml_inference`,
  `ubi` with its response `query_id`; other names fail as OpenSearch fails them), the `template`
  query, search-time `derived` fields (run as runtime fields: `text` ones are not analyzed,
  sorting on them works where OpenSearch refuses), and no `runtime_mappings`.
  `verbose_pipeline` is not implemented.
  `os_errors.json` pins OpenSearch's field caps, error metadata
  order, missing-index resources, can-match/DFS sort failures and stats shapes;
  `os_search.json` its suggestion order, nested inner hit sources, t-digest 3.3 percentile
  ranks, significant terms background and `_msearch` header rules.

## Differential suite

`crates/search-server/tests/differential.rs` runs seeded random workloads
(`tests/differential/workload.rs`: random mappings over every common field type, multi-fields,
nested and object fields, completion fields, 1–5 shards and one or two indices; bulk loads with
refreshes in between; Query DSL trees, aggregation trees, sorts, pagination, highlighting,
suggesters, scroll, point in time, `dfs_query_then_fetch`, msearch, explain, field caps;
index / update / delete / by-query writes between searches) against an in-process roll node and
a real Elasticsearch 9.5.5, comparing status, the headers clients act on and the body of every
step, float-exact. Run it with `ROLL_DIFF_ES=<url>` (or `ROLL_E2E=1`, which starts
`roll-es-clients`); `scripts/test-search.sh` and the `differential` job of
`.github/workflows/search.yml` do. A new difference is minimized into a replayable fixture in
`target/differential/failures/`; fixed ones are committed to `tests/differential/cases/`
(replayed by `replay_cases` without Docker), and differences that are by design are
listed with their reason in `tests/differential/known.json`.

| run | cases | steps compared | equal |
|---|---:|---:|---:|
| seed 1 (start of this round) | 20 | 722 | 98.2% |
| seed 2 | 80 | 2669 | 97.9% |
| seed 3 | 80 | 2810 | 98.7% |
| seed 4 | 100 | 3472 | 99.2% |
| seed 5 | 100 | 3423 | 99.3% |
| seed 6 (next round, before its fixes) | 80 | 2844 | 99.5% |
| seed 6 (after) | 80 | 2844 | 99.8% |
| seed 7 (fresh) | 150 | 5182 | 99.2% |
| seed 8 (fresh) | 150 | 5306 | 99.3% |
| seed 8 (after the fixes below) | 150 | 5306 | 99.5% |
| seed 9 (fresh) | 150 | 5358 | 99.6% |
| seed 10 (fresh, after the fixes below) | 150 | 5241 | 99.5% |
| seed 10 (third round, after its fixes) | 150 | 5241 | 99.8% |
| seed 11 (fresh) | 150 | 5206 | 99.6% |
| seed 11 (after) | 150 | 5206 | 99.7% |
| seed 12 (fresh) | 150 | 5073 | 99.7% |
| seed 13 (fresh) | 150 | 5063 | 99.6% |
| seed 1 (the CI default, 60 cases) | 60 | 2056 | 99.6%, known differences only |
| seed 1 with the extended grammar (fourth round, fresh) | 60 | 2329 | 98.2% (20 new signatures) |
| seed 1 extended (after its fixes) | 60 | 2329 | 99.7%, known differences only |
| seed 15 extended (fresh) | 150 | 5552 | 98.7% (34 new signatures) |
| seed 23 extended (review round, fresh) | 150 | 5638 | 99.1% (37 new signatures) |
| seed 1 extended (after the review fixes) | 60 | 2329 | 99.7%, known differences only |
| seed 23 extended (after the review fixes) | 150 | 5638 | 99.5% (15 new signatures, 2 fixed since) |
| seed 1 extended (final integration run, every lane merged) | 60 | 2329 | 99.7%, known differences only |

Fixed from its findings: Elasticsearch's can-match phase (a primary field sort turns it on,
`_shards.skipped`, all-skipped responses, point-in-time searches) and shard-level `bool`
rewrites to `match_none`; `dfs_query_then_fetch` statistics summed over shards (it used each
shard's own), with Lucene's per-weight statistics requests and `phase: dfs` errors; `top_hits`
applying the search's rescorers and reducing in shard order; completion suggestions fetched like
hits (`_source` filtering, `_ignored`, `_routing`, `fields`, versions), completion fields
searchable by `term` / `match` / `query_string` (their keys are indexed terms), fuzzy completion
(`min_length`, `prefix_length`, AUTO, the prefix boost); phrase smoothing validation;
`minimum_should_match` above the clause count matching nothing; `match_only_text` scoring
constantly; `terms` explanations; `combined_fields` requiring fields; `size: 0` with `from`;
`_id` fielddata errors in aggregations; grouped shard failures; empty `suggest`; scrolls
dropping failed shards and freeing contexts on a continuation without `scroll`; collapse inner
hits keeping the post filter; `_msearch` failing whole on a pipeline-aggregation validation
error; `phase: dfs_query` for query-phase failures of DFS searches.

Fixed in the next round (seeds 6–8; fixtures `index-alias-…`, `track-total-hits-…`,
`highlight-weight-matches-…`, `bool-prefix-…`, `inner-hits-extraction-…`): `term(s)` on `_index`
through an alias or wildcard in the can-match / shard rewrite (searchkick's multi-model search
matched nothing); suggest-only searches collecting nothing; `bool_prefix` scoring like
`most_fields` (`tie_breaker` 1.0, `multi_match` and `query_string`) and its last term a prefix
only when it ends the text; open ranges as `exists`; `ids` / `_field_names` explanations
(`_id:([fe 2f])`, Uid-encoded); a `size: 0` total at exactly `track_total_hits` (`gte` when the
collector stopped before the last segment, and always for `track_total_hits: 0`); duplicate
`inner_hits` names rejected before the query is built, `must_not` contributing none; the
collapse field fetched into, and highlighting applied to, completion options; the unified
highlighter's weight matches (clauses that miss the hit highlight nothing; a phrase prefix is
one span); `size: 0` collecting no top docs (no collapse, no rescore window, no scores);
can-match skipped shards counting as successful when the others fail; `half_float` range bounds
and Java's tie-to-even float digits; integral `term` queries as Lucene's exact queries
(explanations and profiles); span term statistics in term order; shard failures in shard
request order, and in `_count`; `_msearch` items not marking `top_hits` sequence numbers;
clearing a scroll whose shards were all skipped; Elasticsearch's `match_none` reasons for
queries on unmapped fields; numeric `terms` explanations; rescorer queries built before the
collapse and in the DFS phase; and a hang: a 0-boosted `match_none` (a filter that matches
nothing) ping-ponged with its constant-score wrapper in the query rewrite, which nesting
multiplied into minutes of CPU per search (found by the minimizer, fixed in search-index).

Fixed in the third round (seeds 10–13; 26 more fixtures, 51 in all, among them hand-written
ones captured with `capture.py`: `wildcard-constant-keyword-query-strings-…`,
`span-queries-highlight-matching-spans-only`, `suggesters-ignore-dfs-statistics`): an integer
`minimum_should_match` on an all-required `match` (`operator: and`) matching nothing; a
`filter` / `filters` aggregation validating its sub-aggregations before building its queries;
the DFS phase building the post filter, and the post filter built before the sort; `nested`
queries filtering their child query only when it may match other documents
(`NestedHelper.mightMatchNonNestedDocs`, which also drops roll's special case for integral
`term` queries); `simple_query_string` per-field queries under a `dis_max` with tie breaker
1.0 in `HashMap` field order, and `query_string` fields in that order too; pattern field
expansion (`*`, `text*`) skipping fields no segment of the shard holds
(`MappedFieldType.mayExistInIndex`); `term` / `terms` / `match` on `constant_keyword` printing
Elasticsearch's rewrite reasons; `size: 0` building no scorer (a `function_score` that would
fail on negative scores, with a `_score` sort too); completion options fetched with the
search's `explain` and `inner_hits`; `wildcard` field queries printing as Elasticsearch's
`BinaryDvConfirmed*Query` and `fuzzy` on `wildcard` fields (it failed); geo point queries bare
(no constant-score wrapper), one circle / rectangle as Lucene's distance / box queries;
`_geo_distance` sort values bit-identical (Lucene's `SloppyMath` tables, which the JVM fills
with HotSpot's `Math.sin` / `Math.cos`: 36 entries differ from libm by an ulp, patched in
`search-index` `util::geo`); suggesters reading the shard's statistics under DFS (the phrase
suggester found nothing) and skipping shards whose query phase failed; span queries
highlighting only the terms of a matching span (unified: Lucene's sub-matches; plain: the
`WeightedSpanTermExtractor` ranges; fvh: `span_term` only; re-analysis drops tokens of terms
the query doesn't extract, so a `span_not` exclude excludes nothing there); date `range`
queries rewritten from the shard's indexed bounds (`isFieldWithinQuery`: none in range →
`match_none`, all → `exists`); a `function_score` whose query rewrites to `match_none` being
that `match_none`; `query_string` `*` as the default field's `exists` (all-fields: `*:*`); a
partial update keeping the document's routing; and Lucene's `Weight.count` rules where roll
answered more often (`FieldExistsQuery`: doc values alone give no count; `boosting`: none),
which decide the `size: 0` `track_total_hits` boundary. The harness learnt: `"open": true` entries in `known.json`
(roll bugs not fixed yet; tolerated unless `ROLL_DIFF_STRICT=1`), `ROLL_DIFF_REPLAY=<dir>`
(replay a run's minimized failures), comparing query descriptions with their clauses sorted
(the clause order of `dis_max` and of its tie-breaker-1.0 boolean rewrite follows hashes
salted per JVM), masking the doc id of a `function_score` invalid-score error, and no
`_doc`-sorted scrolls after deletes (Elasticsearch's delete tombstones shift doc ids).

Fixed in the fourth round (the extended grammar of `tests/differential/extended.rs`: more
field types, the rest of the Query DSL and aggregations, term vectors, `_validate`,
`_terms_enum`, runtime fields; seeds 1 and 15; 6 hand-written fixtures, 64 in all):
`query_string` syntax errors as Lucene's `ParseException`s (JavaCC's "Encountered … at line 1,
column N. Was expecting one of: …" with the expected tokens of each parser state, lexical
errors as `token_mgr_error`, both wrapped as plain Java exceptions so the root cause is the
`query_shard_exception`); root causes stopping before an `XContentParseException` (which is
not an Elasticsearch exception: `pinned` with an empty `organic`); aggregation parse errors
located in the request body (they were located in the re-serialized `aggs` object) and
failing a whole `_msearch`; term vectors' `filter` (`TermVectorsFilter`: tf-idf over the
shard's statistics, Lucene's priority queue ties, the stale document frequency a lone
segment's enum reports for a term it lacks), statistics read through the internal reader a
realtime get refreshes, zero `field_statistics` for fields the shard lacks and no term
statistics for missing terms; `_validate` with `all_shards` (shard ids, explanations), the
non-nested and alias filters in its explanations, and only the "failed to create query"
wrapper printing `[index/uuid]`; `terms_set` (no terms → `match_none`, unmapped fields as
plain terms, a missing `minimum_should_match_field` failing, the script printed as
`script(…)`); `function_score` functions printed as Java's `toString` (masked hashes) and a
`field_value_factor` on an unmapped field with `missing` scoring `missing` (roll matched
nothing); `_source` filtering of a parsed source (patched vectors, `fields` reading the
source) with map semantics, where `*` spans dots; `_shard_doc` named in `search_after`
errors; `numeric_type: double` missing values as infinities; `_terms_enum` counting shards
and selecting indices by their `can_match` (full-text queries on unmapped fields don't
rewrite to `match_none`, in field caps either); `size: 0` with aggregations and
`terminate_after` counting whole segments `Weight.count` answers for until the limit;
`unsigned_long` doc values outside x-pack's allowlist contexts (runtime fields) failing as a
list shortcut; `stored_fields: _none_` keeping `_id` for highlighting; knn queries on `flat`
indices scoring every vector; a `simple_query_string` phrase slop at the end of the query;
`exists` on an object covering multi-fields and `search_as_you_type` subfields; and
`span_near` clauses on different fields failing with Lucene's message. The harness masks
`[index/uuid]` in `_validate` errors, Java identity hashes and `[line:col]` in errors of
bodies with a substituted scroll / PIT id; `capture.py` sends bodies as the suite does
(compact, sorted keys), so locations in hand-written fixtures match.

Fixed in the review round (seeds 23 and 31; 5 hand-written fixtures, 69 in all): by-query
runs with `max_docs` on several shards (`_delete_by_query` / `_update_by_query` interleave
the shards by doc id, as the `_doc`-sorted search merges them; `_reindex` reads them one after
the other, as Elasticsearch 9.5.5 does); nested `inner_hits` sorts (`missing`, `mode`,
`_score`, printed missing values); collapse `inner_hits` beside nested ones and seeing
`runtime_mappings`; conjunctions scoring only docs every required clause matches (a
`function_score` lead no longer fails a shard with a NaN score); `query_string` edge cases
(`field:>` is a plain term, `!` ends a term, empty groups, boost and quote errors); `boosting`
and `function_score` over a query that rewrites to `match_none`; `significant_terms`
background frequencies within one segment; `diversified_sampler` and docs without the field;
term vector statistics after a realtime get; null vectors dropped from filtered `_source`;
completion options with `script_fields`, without `_explanation`, and their shard failures
(a non-completion or unmapped field, a failing fetch: partial results or "all shards
failed", options of a failed fetch kept as text and score); the `exception` type of
`ElasticsearchException`; `terminate_after` counting only the hits that pass `post_filter`
and `min_score`; `terminated_early: false` on count-only requests (size 0, accurate totals,
no aggregations / suggest / knn / profile); `numeric_type: long | date` on `scaled_float`
(truncated longs, `search_after` bounds included; a shard's top hits are still picked by the
scaled value, which only differs on ties at the page boundary); `exists` on a root
`flattened` field printed on `._keyed`; `_count` and `_explain` wrapping body parse errors
as `parsing_exception` "Failed to parse"; "Partial shards failure" without a `caused_by`;
`profile.request` printing `track_total_hits` as an int. The harness no longer sorts the words
of explanations: only the operands of ` | ` groups and runs of clauses sharing a value on
different fields (multi-field expansions) are sorted.

Open from seeds 15, 23 and 31 (not fixed yet): `unsigned_long` sort values in `top_hits`
(Elasticsearch prints the raw sortable long) and with `numeric_type: double`; a composite
`histogram` `after_key` over several indices; empty clauses inside aggregation filters
(`filters`, `adjacency_matrix`: Elasticsearch fails at parse time with the
`x_content_parse_exception` chain, roll fails each shard); `_explain` / `_validate`
descriptions of several rewrites (`ids` rewritten to `match_none`, `combined_fields` on
unmapped fields, `BinaryDocValuesRangeQuery`, `CaseInsensitiveWildcardQuery`, masked fields
in `span_field_masking`); `_msearch` failure order across indices; `rescore` queries' inner hits
(Elasticsearch extracts none); `fields` with `stored_fields` returning `_source`; DFS with a
`knn` query feeding aggregations; `min_score` with a `filter` aggregation; `more_like_this`
on fields of a string type roll calls unsupported; the term suggester's candidates for short
misspellings; empty-clause errors inside `dis_max` and `filters`; a completion suggester with
a `bool` sort and `track_scores` failing a shard in Elasticsearch; `significant_terms`
`background_filter` with `more_like_this` (cachability error in Elasticsearch).

Open differences (most frequent first): the phrase suggester's candidates and scores after
deletes (`options.length`, `options[].score`: Elasticsearch's `maxDoc` counts a tombstone per
delete; by design, see `known.json`); fuzzy `match` under DFS (blended top-terms statistics);
`intervals` highlighting (Elasticsearch highlights the minimal interval's positions only);
`exists` on a field every live doc
has after deletes (Elasticsearch's soft-deleted docs keep it from rewriting to `*:*`); weight
matches stay off for `nested` with `ignore_unmapped` on an unmapped path; `query_string` with
`type: bool_prefix`; `exists` on `*` (Elasticsearch fails on `_doc_count`); query profiles'
per-term children (`TermQuery` under a term set); `collapse` validated before `rescore`;
and, by design (`known.json`), statistics that count
Elasticsearch's soft-deleted documents and delete tombstones (term / phrase suggester
frequencies after updates and merges, `_shard_doc` and `_doc` sort values, Lucene doc ids in
collapse errors).

## Client matrix

`tests/search/clients/run.sh [client…]` builds each client's image and runs it against two
in-memory search-dev instances (the elasticsearch dialect on `$ROLL_PORT`, the opensearch
dialect on `$ROLL_PORT+1`) with `--network host`; `CLIENTS_ES_URL` / `CLIENTS_OS_URL` point it
at running servers instead (a real Elasticsearch makes a baseline). Each client runs its product
check and a representative scenario: index with mappings, bulk (and the bulk helpers), search
with aggregations, scroll / scan and point in time, update and delete by query, errors.
Logs: `target/search-clients/<client>.log`.

| client | version | result |
|---|---|---|
| elasticsearch-py | 9.5.1 | pass |
| opensearch-py (opensearch dialect) | 3.2.0 | pass |
| @elastic/elasticsearch | 9.5.1 | pass |
| @opensearch-project/opensearch (opensearch dialect) | 3.9.0 | pass |
| elasticsearch-java | 9.5.5 | pass |
| go-elasticsearch | v9.5.2 | pass |
| elasticsearch-ruby | ~> 9.5.0 | pass |
| elasticsearch-php | 9.5.* | pass |
| django-elasticsearch-dsl (Django 5.2, elasticsearch 9.0) | 9.0 | pass |
| Laravel Scout 11 with babenkoivan/elastic-scout-driver 6 (Laravel 13, elasticsearch-php 9) | 11.x / 6.x | pass |
| searchkick (its own test suite, v6.1.2) | 6.1.2 | 540 runs, 1693 assertions, 0 failures, 0 errors (17 skips of its own) |

## Performance

Full methodology, hardware and every number: [docs/benchmarks/search.md](../benchmarks/search.md)
(`scripts/bench-search.sh`: both targets in Docker on the same 4 cores / 8 threads and 4 GiB,
runs interleaved, medians over rounds). Final run of the round (2026-10-26, the merged tree on a
quiet host). Micro suite (one shard, 50,000 generated documents, per request, lower is better),
roll `search-dev --release` vs Elasticsearch 9.5.5, wall time and server CPU:

| workload | roll | Elasticsearch 9.5.5 | roll CPU | Elasticsearch CPU |
|---|---:|---:|---:|---:|
| bulk 50k docs (1k-doc batches) | 0.94 s | 1.99 s | 1.29 s | 3.51 s |
| match | 1.517 ms | 2.465 ms | 1.139 ms | 4.300 ms |
| match, 2 terms (or) | 2.139 ms | 3.059 ms | 1.699 ms | 3.614 ms |
| bool_filter_range | 0.567 ms | 2.312 ms | 0.401 ms | 2.834 ms |
| terms_agg (no cache) | 1.846 ms | 7.781 ms | 1.505 ms | 10.272 ms |
| date_histogram (no cache) | 2.118 ms | 7.885 ms | 1.744 ms | 8.089 ms |
| sort_field | 1.745 ms | 2.828 ms | 1.346 ms | 3.275 ms |
| match_all_count (no cache) | 0.299 ms | 1.471 ms | 0.215 ms | 1.069 ms |
| get | 0.365 ms | 1.406 ms | 0.176 ms | 1.303 ms |

roll wins all 17 micro rows (1.4–4.9x wall, 2.1–7.8x CPU). Rally (same pinning, two rounds
each): `http_logs` (2%, 4.95M docs, 10 shards per search) roll wins every query task (sorts
1.2–2.3x, `asc_sort_with_after_timestamp` 1.7 vs 5.5 ms, `hourly_agg` 43.6 vs 67.9 ms, `scroll`
453 vs 479 ms) and indexes at par (240k vs 250k docs/s, Elasticsearch's rounds 193k–307k).
`geonames` (10%, 1.1M docs): roll wins every task but two script scores
(`decay_geo_gauss_script_score` 37.1 vs 34.7 ms, inside Elasticsearch's spread;
`field_value_script_score` 21.8 vs 20.1 ms), loads 1.5x faster (109k vs 75k docs/s), and
`scroll` takes 485 vs 698 ms. Footprint: 24 MiB idle RSS against 2.6 GiB, 192 against 263 store
bytes per document, a wake after idle unload answers in 27 ms (an Elasticsearch node restart
takes 21 s). This run found a per-search cost the lanes had added (`available_parallelism`
re-reading cgroup files several times per request, 28% of a `size: 0` search's CPU), fixed by
looking it up once.

### Durable writes

Every index, delete and update is one record in the node's group-committed WAL and is
acknowledged per `index.translog.durability` (default `request`: after the fsync that holds
it); a write never cuts a segment, so refresh, flush and merge follow Elasticsearch's policy.
`cargo test --release -p search-server --test bench durable_indexing_throughput -- --ignored
--nocapture` (in-process node, documents per second, higher is better; same busy 16-core
machine, before = the per-write-group segment flush of the previous round):

| workload | before, tmpfs | now, tmpfs | before, disk | now, disk | in memory |
|---|---:|---:|---:|---:|---:|
| single docs, 1 client | 1,079 | 5,197 | 3 | 56 | 32,515 |
| single docs, 16 clients | 3,224 | 13,014 | 232 | 330 | 33,470 |
| bulk, 1k-doc requests | 6,519 | 24,693 | 4,093 | 18,484 | 37,480 |

On disk (a shared, heavily loaded md RAID: 10–18 ms per fsync) a lone client is bound by one
fsync per request; concurrent writers share fsyncs (16 clients: 8,000 writes in 3,471 commits)
and a bulk request is one commit.

Leader commits (round 2): while fsyncs average under 250 µs, a writer commits the WAL batch on
its own thread instead of handing it to the flusher thread and being woken again. Same bench,
tmpfs, release build, after other lanes' engine speedups, machine load 30–65, five runs each
(`WalConfig.leader_commits` off → on): single docs, 1 client 14,000–21,000 → 28,700–30,000/s
(in memory 44,000–48,000/s); 16 clients and bulk unchanged within the noise (103k–170k and
71k–103k docs/s both ways). On slow disks the flusher thread keeps committing as before.

New hardware (round 2, NVMe md RAID: 0.08–0.10 ms per fsync, load 23, release build, docs/s,
flusher thread only → leader commits; in memory for reference):

| workload | tmpfs | disk | in memory |
|---|---:|---:|---:|
| single docs, 1 client | 17,678 → 37,513 | 5,640 → 7,179 | 36,107–39,403 |
| single docs, 16 clients | 110,423 → 142,330 | 43,282 → 45,271 | 192,444–202,477 |
| bulk, 1k-doc requests | 79,650 → 55,752 | 57,274 → 64,992 | 75,945–78,680 |

A lone durable client on disk pays one fsync per write (2,000 writes, 2,000 commits); 16
clients share them (8,000 writes in about 1,600 commits) and a 1,000-document bulk request is
one commit. The tmpfs bulk figures vary run to run by ±30% at this load.

Round 3 (commits now also record the published view and statistics; same disk, release build,
machine load 35, one run): flusher only → leader commits, 1 client 3,127 → 6,112, 16 clients
40,731 → 52,288, bulk 62,108 → 56,121 docs/s; in memory 41,957 / 171,390 / 62,115. Within the
noise of round 2 (the write path is unchanged; the extra work is per commit, not per write).

Round 4 (same disk, release build, machine load 23, two runs; the write path is unchanged, the
new work is per commit and per snapshot): flusher only 1 client 5,602–6,195, leader commits
3,749–6,563 (the leader's first run was an outlier), 16 clients 57,503–61,852, bulk
53,991–86,878 docs/s; in memory 54,578–60,375 / 210,488–210,583 / 81,035–86,857.

Object-storage acknowledgements (`sync_objects`, leased cluster, SeaweedFS S3 on the same busy
machine, `bench sync_objects_ownership_check_cost`): the ownership check adds one LIST after the
WAL upload of each acknowledgement round (concurrent writers share it: 16 clients × 50 writes
took 350–450 checks), so 16 clients reach about half the rate without it (761 vs 1,323–1,558
docs/s); a lone client is bound by upload latency either way (90–320 docs/s).

Wake fidelity: the ES suite against `search-dev --data-dir` passes exactly the tests it passes in
memory (3809 of 3905, rounds 2 to 4). An idle unload is meant to be invisible, as if the node had
never stopped. A commit records what the searcher showed (`roll.instance.published`), so writes
nobody refreshed stay unsearchable across a graceful unload (`refresh_interval: -1`,
`realtime=false`) and become searchable when Elasticsearch would publish them; shard statistics
(indexing, get, search and search groups, request-cache hits, field usage, loaded field data)
carry over in the manifest; trained model deployments are cluster state as on Elasticsearch and
come back at wake, with their counters after a graceful unload. Async results (async search,
EQL, ES|QL, SQL async and cursors) are durable in `.async-search`, acknowledged with the request
that stored them. Since round 4 also: snapshot contents (each snapshotted shard's commit point is
pinned in object storage, sharing the index's segments, before the snapshot is recorded, so a
restore after an unload, a crash, the index's deletion, garbage collection or in a branch gets
every document back with its `_seq_no`, `_version` and `_primary_term`; snapshot deletions or
failed snapshots running at the same time never drop an unfinished snapshot's pins, an index
created while a snapshot is taken is pinned before the snapshot is recorded, deleting a snapshot
a restore is reading answers 503 `concurrent_snapshot_execution_exception` as Elasticsearch
does, and pins a crash left unrecorded are swept after a wake once an hour old); the synonym rules an
analysis chain loaded (a set changed with `refresh=false`, or replaced by rules the chain can't
build, keeps them until a reload, as Elasticsearch's analyzers do); the request cache (its keys:
entries are rebuilt on use and count as hits); the segment order a shard opened with; the
soft-deletes retention lease (a reopened shard no longer merges away deletes a running one
retains); the translog figures (the unload's commit is no flush Elasticsearch would make, unless
the shard was inactive for 5 minutes, when Elasticsearch flushes on idle); and the flush and
search figures of indices without documents. A crash or a handoff still behaves like an
Elasticsearch restart (everything acknowledged is back and searchable). With
`--idle-unload-ms 1` (the cluster unloads after every request and wakes from object storage for
the next) 3809 of 3905 pass in round 4, the same as in memory (3775 in round 1, 3801 in round 2,
3804–3805 in round 3). A merge installed after the last refresh is traced back to the segments
the searcher showed, so unrefreshed writes stay hidden across it too (unless the searcher showed a
doc the merge dropped, or the merge exceeds 2M docs: then the wake shows everything). Still
different after a wake: cross-cluster search statistics, and the segment order of such a merge
(ties between equal scores may come out in another order until the next refresh would reorder
them anyway).

## Per area

`roll` = baseline tests roll passes, `ES` = tests passing on Elasticsearch 9.5.5. The most
common remaining failure is listed where an area is not complete.

| suite:area | roll | ES | % | main remaining failure |
|---|---:|---:|---:|---|
| clients: | 11 | 11 | 100 |  |
| clients:async_search | 1 | 1 | 100 |  |
| clients:bulk | 1 | 1 | 100 |  |
| clients:cat | 22 | 22 | 100 |  |
| clients:cluster | 12 | 12 | 100 |  |
| clients:count | 1 | 1 | 100 |  |
| clients:create | 1 | 1 | 100 |  |
| clients:dangling_indices | 1 | 1 | 100 |  |
| clients:delete | 1 | 1 | 100 |  |
| clients:delete_by_query | 1 | 1 | 100 |  |
| clients:enrich | 2 | 2 | 100 |  |
| clients:entsearch | 5 | 5 | 100 |  |
| clients:eql | 1 | 1 | 100 |  |
| clients:esql | 4 | 4 | 100 |  |
| clients:exists | 1 | 1 | 100 |  |
| clients:exists_source | 1 | 1 | 100 |  |
| clients:explain | 1 | 1 | 100 |  |
| clients:features | 1 | 1 | 100 |  |
| clients:field_caps | 1 | 1 | 100 |  |
| clients:get | 1 | 1 | 100 |  |
| clients:get_source | 1 | 1 | 100 |  |
| clients:graph | 1 | 1 | 100 |  |
| clients:ilm | 1 | 1 | 100 |  |
| clients:index | 2 | 2 | 100 |  |
| clients:indices | 39 | 39 | 100 |  |
| clients:ingest | 3 | 3 | 100 |  |
| clients:logstash | 1 | 1 | 100 |  |
| clients:machine_learning | 27 | 27 | 100 |  |
| clients:migration | 4 | 4 | 100 |  |
| clients:mtermvectors | 1 | 1 | 100 |  |
| clients:nodes | 1 | 1 | 100 |  |
| clients:ping | 1 | 1 | 100 |  |
| clients:point_in_time | 1 | 1 | 100 |  |
| clients:query_rules | 3 | 3 | 100 |  |
| clients:reindex | 1 | 1 | 100 |  |
| clients:script | 2 | 2 | 100 |  |
| clients:scroll | 1 | 1 | 100 |  |
| clients:search | 1 | 1 | 100 |  |
| clients:search_application | 4 | 4 | 100 |  |
| clients:search_mvt | 1 | 1 | 100 |  |
| clients:search_template | 1 | 1 | 100 |  |
| clients:searchable_snapshots | 1 | 1 | 100 |  |
| clients:security | 14 | 14 | 100 |  |
| clients:simulate | 1 | 1 | 100 |  |
| clients:snapshot | 1 | 1 | 100 |  |
| clients:sql | 2 | 2 | 100 |  |
| clients:streams | 1 | 1 | 100 |  |
| clients:synonyms | 1 | 1 | 100 |  |
| clients:terms_enum | 1 | 1 | 100 |  |
| clients:termvectors | 1 | 1 | 100 |  |
| clients:text_structure | 1 | 1 | 100 |  |
| clients:transform | 3 | 3 | 100 |  |
| clients:update | 1 | 1 | 100 |  |
| clients:update_by_query | 1 | 1 | 100 |  |
| clients:validate_query | 1 | 1 | 100 |  |
| clients:watcher | 1 | 1 | 100 |  |
| free:aggregations | 640 | 640 | 100 |  |
| free:analysis-common | 131 | 131 | 100 |  |
| free:bulk | 20 | 20 | 100 |  |
| free:capabilities | 1 | 1 | 100 |  |
| free:cat.aliases | 15 | 15 | 100 |  |
| free:cat.allocation | 9 | 9 | 100 |  |
| free:cat.circuit_breaker | 2 | 2 | 100 |  |
| free:cat.component_templates | 3 | 3 | 100 |  |
| free:cat.count | 2 | 2 | 100 |  |
| free:cat.fielddata | 2 | 2 | 100 |  |
| free:cat.health | 3 | 3 | 100 |  |
| free:cat.indices | 11 | 11 | 100 |  |
| free:cat.nodeattrs | 2 | 2 | 100 |  |
| free:cat.nodes | 4 | 4 | 100 |  |
| free:cat.plugins | 1 | 1 | 100 |  |
| free:cat.recovery | 2 | 2 | 100 |  |
| free:cat.repositories | 3 | 3 | 100 |  |
| free:cat.segments | 6 | 6 | 100 |  |
| free:cat.shards | 5 | 5 | 100 |  |
| free:cat.snapshots | 2 | 2 | 100 |  |
| free:cat.tasks | 2 | 2 | 100 |  |
| free:cat.templates | 11 | 11 | 100 |  |
| free:cat.thread_pool | 1 | 1 | 100 |  |
| free:cluster.allocation_explain | 25 | 25 | 100 |  |
| free:cluster.component_template | 14 | 14 | 100 |  |
| free:cluster.desired_balance | 11 | 11 | 100 |  |
| free:cluster.desired_nodes | 42 | 42 | 100 |  |
| free:cluster.health | 10 | 10 | 100 |  |
| free:cluster.info | 6 | 6 | 100 |  |
| free:cluster.pending_tasks | 2 | 2 | 100 |  |
| free:cluster.prevalidate_node_removal | 4 | 4 | 100 |  |
| free:cluster.put_settings | 3 | 3 | 100 |  |
| free:cluster.remote_info | 1 | 1 | 100 |  |
| free:cluster.reroute | 6 | 6 | 100 |  |
| free:cluster.state | 15 | 15 | 100 |  |
| free:cluster.stats | 19 | 19 | 100 |  |
| free:cluster.voting_config_exclusions | 7 | 7 | 100 |  |
| free:count | 10 | 10 | 100 |  |
| free:create | 9 | 9 | 100 |  |
| free:data_stream | 141 | 141 | 100 |  |
| free:data_stream/lifecycle | 18 | 18 | 100 |  |
| free:delete | 14 | 14 | 100 |  |
| free:delete_by_query | 32 | 32 | 100 |  |
| free:exists | 4 | 4 | 100 |  |
| free:explain | 5 | 5 | 100 |  |
| free:features.get_features | 1 | 1 | 100 |  |
| free:features.reset_features | 1 | 1 | 100 |  |
| free:field_caps | 29 | 29 | 100 |  |
| free:get | 41 | 41 | 100 |  |
| free:get_source | 9 | 9 | 100 |  |
| free:health | 3 | 3 | 100 |  |
| free:index | 33 | 33 | 100 |  |
| free:indices.analyze | 17 | 17 | 100 |  |
| free:indices.blocks | 2 | 2 | 100 |  |
| free:indices.clear_cache | 3 | 3 | 100 |  |
| free:indices.clone | 4 | 4 | 100 |  |
| free:indices.create | 81 | 81 | 100 |  |
| free:indices.delete | 10 | 10 | 100 |  |
| free:indices.delete_alias | 11 | 11 | 100 |  |
| free:indices.exists | 3 | 3 | 100 |  |
| free:indices.exists_alias | 1 | 1 | 100 |  |
| free:indices.exists_template | 2 | 2 | 100 |  |
| free:indices.flush | 2 | 2 | 100 |  |
| free:indices.forcemerge | 3 | 3 | 100 |  |
| free:indices.get | 13 | 13 | 100 |  |
| free:indices.get_alias | 30 | 30 | 100 |  |
| free:indices.get_field_mapping | 13 | 13 | 100 |  |
| free:indices.get_index_template | 12 | 12 | 100 |  |
| free:indices.get_mapping | 24 | 24 | 100 |  |
| free:indices.get_settings | 15 | 15 | 100 |  |
| free:indices.get_template | 8 | 8 | 100 |  |
| free:indices.open | 8 | 8 | 100 |  |
| free:indices.put_alias | 13 | 13 | 100 |  |
| free:indices.put_index_template | 24 | 24 | 100 |  |
| free:indices.put_mapping | 15 | 15 | 100 |  |
| free:indices.put_settings | 13 | 13 | 100 |  |
| free:indices.put_template | 7 | 7 | 100 |  |
| free:indices.recovery | 6 | 6 | 100 |  |
| free:indices.refresh | 3 | 3 | 100 |  |
| free:indices.resolve_cluster | 7 | 7 | 100 |  |
| free:indices.resolve_index | 6 | 6 | 100 |  |
| free:indices.rollover | 15 | 15 | 100 |  |
| free:indices.segments | 4 | 4 | 100 |  |
| free:indices.shard_stores | 3 | 3 | 100 |  |
| free:indices.shrink | 3 | 3 | 100 |  |
| free:indices.simulate_index_template | 7 | 7 | 100 |  |
| free:indices.simulate_template | 5 | 5 | 100 |  |
| free:indices.sort | 11 | 11 | 100 |  |
| free:indices.split | 16 | 16 | 100 |  |
| free:indices.stats | 71 | 71 | 100 |  |
| free:indices.update_aliases | 18 | 18 | 100 |  |
| free:indices.validate_query | 3 | 3 | 100 |  |
| free:indices/validate_query | 1 | 1 | 100 |  |
| free:ingest | 10 | 10 | 100 |  |
| free:logsdb | 24 | 24 | 100 |  |
| free:mapping | 54 | 54 | 100 |  |
| free:mget | 26 | 26 | 100 |  |
| free:mlt | 4 | 4 | 100 |  |
| free:msearch | 8 | 8 | 100 |  |
| free:mtermvectors | 6 | 6 | 100 |  |
| free:nodes.hot_threads | 6 | 6 | 100 |  |
| free:nodes.info | 18 | 18 | 100 |  |
| free:nodes.reload_secure_settings | 2 | 2 | 100 |  |
| free:nodes.stats | 35 | 35 | 100 |  |
| free:range | 15 | 15 | 100 |  |
| free:reindex | 98 | 98 | 100 |  |
| free:repositories | 9 | 9 | 100 |  |
| free:scripts | 2 | 2 | 100 |  |
| free:scroll | 16 | 16 | 100 |  |
| free:search | 473 | 473 | 100 |  |
| free:search.highlight | 48 | 48 | 100 |  |
| free:search.inner_hits | 6 | 6 | 100 |  |
| free:search.query | 16 | 16 | 100 |  |
| free:search.retrievers | 21 | 21 | 100 |  |
| free:search.retrievers/result-diversification | 10 | 10 | 100 |  |
| free:search.suggest | 6 | 6 | 100 |  |
| free:search.vectors | 395 | 395 | 100 |  |
| free:search_shards | 2 | 2 | 100 |  |
| free:simulate.ingest | 10 | 10 | 100 |  |
| free:snapshot.clone | 1 | 1 | 100 |  |
| free:snapshot.create | 3 | 3 | 100 |  |
| free:snapshot.delete | 3 | 3 | 100 |  |
| free:snapshot.get | 10 | 10 | 100 |  |
| free:snapshot.get_repository | 6 | 6 | 100 |  |
| free:snapshot.restore | 2 | 2 | 100 |  |
| free:snapshot.status | 3 | 3 | 100 |  |
| free:streams/logs | 25 | 25 | 100 |  |
| free:suggest | 22 | 22 | 100 |  |
| free:synonyms | 69 | 69 | 100 |  |
| free:tasks.cancel | 1 | 1 | 100 |  |
| free:tasks.get | 1 | 1 | 100 |  |
| free:tasks.list | 2 | 2 | 100 |  |
| free:termvectors | 4 | 4 | 100 |  |
| free:tsdb | 160 | 160 | 100 |  |
| free:update | 18 | 18 | 100 |  |
| free:update_by_query | 54 | 54 | 100 |  |

## Skipped tests

Tests the runner skips, with the reason (counts are from the run above). Environment skips
come from `crates/search-yaml/baseline/es-9.5.5.skips.txt` and apply to Elasticsearch too;
the rest are skipped by the tests' own prerequisites.

| tests | reason |
|---:|---|
| 25 | needs capability `GET /_streams/status` that roll does not advertise |
| 18 | test is skipped on clusters with feature `index.time_series_synthetic_id` (like on Elasticsearch 9.5.5) |
| 12 | serverless-only test (`requires.stack: false`) |
| 11 | test is skipped on clusters with feature `gte_v8.13.0` (like on Elasticsearch 9.5.5) |
| 10 | skip list: needs two nodes (waits for green with one replica) |
| 5 | test is skipped on clusters with feature `gte_v7.4.0` (like on Elasticsearch 9.5.5) |
| 4 | skip list: ML native processes need more memory than the 1.5 GB container |
| 4 | test is skipped on clusters with feature `gte_v6.7.0` (like on Elasticsearch 9.5.5) |
| 4 | skip list: needs the ingest-geoip test fixture (custom databases in the config dir, a download service) |
| 3 | test is skipped on clusters with feature `gte_v7.2.0` (like on Elasticsearch 9.5.5) |
| 3 | test is skipped on clusters with feature `gte_v7.3.0` (like on Elasticsearch 9.5.5) |
| 2 | skip list: expects shards on the node before the test creates any |
| 2 | needs capability `POST /_search` that roll does not advertise |
| 2 | needs capability `PUT /{index}` that roll does not advertise |
| 1 | skip list: expects shards in the cluster before the test creates any |
| 1 | skip list: deploys the ELSER model (download + ML memory) |
| 1 | skip list: installs a license signed with Elastic's test key, invalid on release builds |
| 1 | skip list: needs the .security index to exist before the test starts |
| 1 | test is skipped on clusters with feature `gte_v8.10.0` (like on Elasticsearch 9.5.5) |
| 1 | awaits fix https://github.com/elastic/elasticsearch/issues/78677 |
| 1 | needs capability `GET /_get_index_template` that roll does not advertise |
| 1 | capability POST /logs_search/_rollover present |
| 1 | test is skipped on clusters with feature `gte_v8.15.0` (like on Elasticsearch 9.5.5) |
| 1 | test is skipped on clusters with feature `gte_v8.0.0` (like on Elasticsearch 9.5.5) |
| 1 | test is skipped on clusters with feature `gte_v8.11.0` (like on Elasticsearch 9.5.5) |
| 1 | awaits fix https://github.com/elastic/elasticsearch/issues/92058 |
| 1 | awaits fix https://github.com/elastic/elasticsearch/issues/107466 |

## Known gaps in the document and search APIs

- By-query tasks (`wait_for_completion=false`) run to completion before their task id is
  returned (their results are stored in the `.tasks` system index), so they are not
  throttled and `_rethrottle` doesn't reach them; synchronous requests are throttled by
  `requests_per_second`.
- Reindex from remote reads over plain HTTP (no TLS client) and needs
  `reindex.remote.whitelist` (node setting, `search-dev --yaml-test-cluster` sets
  `127.0.0.1:*`); an empty whitelist rejects every remote, as in Elasticsearch.
- Segment generations in `_segments` / `_cat/segments` number the current segments.
- Aggregations and sorting on keyed flattened sub-fields, `_index_mode` aggregations,
  nested sorts inside `top_hits`, and shard-level aggregation failures as partial results.
- Compound retrievers (`rrf`, `linear`, `pinned`, `rescorer` over them) follow
  Elasticsearch's rank-docs final phase, explanations and profiles; the `rrf`/`linear`
  simplified `query`/`fields` form needs inference endpoints for `semantic_text` fields, and
  a `knn` child's explanation reports its search score as the similarity (Elasticsearch
  recomputes it exactly) without its pre-filters.
- Percolation pre-selects candidates by the terms each stored query requires (per-segment
  cache); stored queries without extractable terms (ranges, wildcards, scripts) are always
  verified.
- Highlighting: weighted unified matches, `matched_fields` masks, inner hits of nested-in-
  nested objects; `exists` on fields without doc values or norms (`_field_names` is not
  indexed).
- x-pack (`crates/search-es/src/xpack/`): security (native realm, API keys, tokens, role
  mappings, privileges, request authorization), ES|QL, SQL, EQL, enrich, transforms, async
  search, ILM, search applications, query rules, connectors, graph, terms enum, text structure
  run for real; ML jobs/datafeeds/trained models, watcher, logstash pipelines, searchable
  snapshots, migration, SSL certificates and streams keep their stored objects, validation,
  state transitions and stats shapes as Elasticsearch reports them, without the native
  processes behind them. ES|QL: ~110 scalar functions with Elasticsearch's arity/type errors and
  evaluation warnings; `FROM ROW SHOW WHERE EVAL STATS (… WHERE, BY BUCKET) INLINE STATS SORT
  LIMIT KEEP DROP RENAME MV_EXPAND DISSECT GROK ENRICH LOOKUP JOIN SAMPLE`, `SET` query settings
  (`unmapped_fields` nullify/load, `time_zone` with zoned date functions and output,
  `column_metadata`) and the body `settings`, `METADATA _source`, constant folding of function
  and aggregation parameters, spatial types (`geo_point`, `cartesian_point`, `geo_shape`,
  `cartesian_shape` as Elasticsearch's WKT, `TO_GEOPOINT` and its siblings, `ST_X`/`ST_Y` on
  the index's quantized coordinates, `ST_GEOMETRYTYPE`, `ST_DIMENSION`, `ST_ISEMPTY`, a WKT
  reader with Elasticsearch's grammar, coordinate checks and per-value warnings, `geo_match`
  `ENRICH` on any shape with `INTERSECTS` semantics), union types (fields typed differently per index are
  `unsupported` with `original_types`/`suggested_cast` until a conversion function or `::` cast
  that takes every type resolves them) and an emulated `profile` (Elasticsearch's drivers and
  phases with roll's counts, two-phase aggregation, projection and limit operators, plans).
  `COUNT_DISTINCT` is Elasticsearch's HyperLogLog++ (same hashes, so the same estimates and
  fixed memory), doubles group and dedupe by their bits like Elasticsearch's block hashes
  (`-0.0` and `0.0` apart, but `-0.0 == 0.0`). `PERCENTILE`/`MEDIAN`/`MEDIAN_ABSOLUTE_DEVIATION` use
  Elasticsearch's hybrid t-digest (compression 1000), `STD_DEV`/`VARIANCE` Welford's algorithm,
  `VALUES` first-appearance order. `FROM` reads doc values straight from the shards
  (`esql/columnar.rs`; text and fields without doc values come from the stored `_source`, text
  through its keyword multi-field or own doc values as Elasticsearch loads it); `SORT | LIMIT`
  over source fields selects the top `n` documents from the sort columns alone and reads only
  those whole (Elasticsearch's Lucene top-n pushdown), and `STATS` groups hash values in place.
  On a 50k-document index (release build, same machine) every benchmark query is at or below
  Elasticsearch 9.5.5: `STATS` 9–30 ms against 18–40 ms, `SORT n DESC | LIMIT 10` 7 ms against
  19 ms, `EVAL` + `STATS BY` 21 ms against 23 ms, a filtered `STATS` 2–12 ms against 17–20 ms.
  Not yet: `FORK`, `CHANGE_POINT`, `COMPLETION`/`RERANK`, `TS`/PromQL and
  `aggregate_metric_double`, flattened and dense_vector columns, `unsigned_long` above
  `i64::MAX`, `EXPLAIN`, other `ST_*` functions (distance, relations, centroid/extent
  aggregations). roll claims the 40 ES|QL capabilities whose tests pass, so of the 619 platinum
  ES|QL YAML tests (not in the baseline) 331 pass, 13 fail and 275 skip. `_terms_enum` reads
  the segments' term dictionaries (keyword, version, flattened subfields; ip from its doc
  values dictionary), so `search_after` reaches any term.
  Differential fixtures against Elasticsearch 9.5.5:
  `crates/search-es/tests/fixtures/xpack/` (`xpack_fixtures.rs`).
