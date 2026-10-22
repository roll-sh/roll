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

Reproduce the YAML numbers with `scripts/search-yaml-suites.sh roll` (set `ROLL_PORT`).

## Totals

- Tests run: 3905; passed 3613, failed 174, skipped 118.
- Of the 3808 tests that pass on Elasticsearch 9.5.5, roll passes **3610** (94%).
- Module suites the `rest-resources` artifact doesn't ship, run from the Elasticsearch 9.5.5
  sources (`modules/*/src/yamlRestTest`): `parent-join` 24 of 24 (3 more are skipped by
  Elasticsearch itself), `percolator` 6 of 6.
- The OpenSearch 3.9.0 suite (opensearch dialect): 1276 of 1506 pass.

## Performance

Single client, sequential requests, one shard, 50,000 generated documents (5-word `title`,
30-word `body` from a 5,000-word vocabulary, keyword `cat`, `long`, `double`, `date`); roll is
`search-dev` built with `--release`, Elasticsearch 9.5.5 the reference container, both on the
same (shared, busy) machine. Plain
rows reuse the same aggregation body, so both sides answer them from the shard request cache;
`(no cache)` rows pass `request_cache=false`. Per-request latency, lower is better:

| workload | roll | Elasticsearch 9.5.5 |
|---|---:|---:|
| bulk 50k docs (1k-doc batches, incl. client-side JSON generation) | 6.65s | 6.36s |
| match | 1.544 ms | 2.777 ms |
| match, 2 terms (or) | 2.722 ms | 2.547 ms |
| bool_filter_range | 1.136 ms | 1.827 ms |
| terms_agg | 0.768 ms | 1.449 ms |
| date_histogram | 0.681 ms | 1.456 ms |
| sort_field | 2.616 ms | 2.482 ms |
| match_all_count | 0.504 ms | 1.027 ms |
| terms_agg (no cache) | 6.396 ms | 10.542 ms |
| date_histogram (no cache) | 6.716 ms | 9.759 ms |
| match_all_count (no cache) | 1.851 ms | 1.527 ms |
| get | 0.353 ms | 1.019 ms |

## Per area

`roll` = baseline tests roll passes, `ES` = tests passing on Elasticsearch 9.5.5. The most
common remaining failure is listed where an area is not complete.

| suite:area | roll | ES | % | main remaining failure |
|---|---:|---:|---:|---|
| clients: | 9 | 11 | 81 | ssl.certificates → 501 unsupported_operation_exception: [..] is not supported by roll sear |
| clients:async_search | 0 | 1 | 0 | async_search.submit → 501 unsupported_operation_exception: [..] is not supported by roll s |
| clients:bulk | 1 | 1 | 100 |  |
| clients:cat | 18 | 22 | 81 | cat.component_templates: match mismatch |
| clients:cluster | 12 | 12 | 100 |  |
| clients:count | 1 | 1 | 100 |  |
| clients:create | 1 | 1 | 100 |  |
| clients:dangling_indices | 0 | 1 | 0 | dangling_indices.list_dangling_indices → 501 unsupported_operation_exception: [..] is not  |
| clients:delete | 1 | 1 | 100 |  |
| clients:delete_by_query | 1 | 1 | 100 |  |
| clients:enrich | 0 | 2 | 0 | enrich.put_policy → 501 unsupported_operation_exception: [..] is not supported by roll sea |
| clients:entsearch | 0 | 5 | 0 | connector.put → 501 unsupported_operation_exception: [..] is not supported by roll search  |
| clients:eql | 0 | 1 | 0 | eql.search → 501 unsupported_operation_exception: [..] is not supported by roll search yet |
| clients:esql | 0 | 4 | 0 | esql.query → 501 unsupported_operation_exception: [..] is not supported by roll search yet |
| clients:exists | 1 | 1 | 100 |  |
| clients:exists_source | 1 | 1 | 100 |  |
| clients:explain | 1 | 1 | 100 |  |
| clients:features | 1 | 1 | 100 |  |
| clients:field_caps | 1 | 1 | 100 |  |
| clients:get | 1 | 1 | 100 |  |
| clients:get_source | 1 | 1 | 100 |  |
| clients:graph | 0 | 1 | 0 | graph.explore → 501 unsupported_operation_exception: [..] is not supported by roll search  |
| clients:ilm | 0 | 1 | 0 | ilm.put_lifecycle → 501 unsupported_operation_exception: [..] is not supported by roll sea |
| clients:index | 2 | 2 | 100 |  |
| clients:indices | 37 | 39 | 94 | indices.downsample → 501 unsupported_operation_exception: [..] is not supported by roll se |
| clients:ingest | 1 | 3 | 33 | ingest.geo_ip_stats → 501 unsupported_operation_exception: [..] is not supported by roll s |
| clients:logstash | 0 | 1 | 0 | logstash.put_pipeline → 501 unsupported_operation_exception: [..] is not supported by roll |
| clients:machine_learning | 0 | 27 | 0 | ml.put_job → 501 unsupported_operation_exception: [..] is not supported by roll search yet |
| clients:migration | 0 | 4 | 0 | migration.get_feature_upgrade_status → 501 unsupported_operation_exception: [..] is not su |
| clients:mtermvectors | 1 | 1 | 100 |  |
| clients:nodes | 0 | 1 | 0 | nodes.get_repositories_metering_info → 501 unsupported_operation_exception: [..] is not su |
| clients:ping | 1 | 1 | 100 |  |
| clients:point_in_time | 1 | 1 | 100 |  |
| clients:query_rules | 0 | 3 | 0 | query_rules.put_ruleset → 501 unsupported_operation_exception: [..] is not supported by ro |
| clients:reindex | 1 | 1 | 100 |  |
| clients:script | 2 | 2 | 100 |  |
| clients:scroll | 1 | 1 | 100 |  |
| clients:search | 1 | 1 | 100 |  |
| clients:search_application | 0 | 4 | 0 | search_application.put → 501 unsupported_operation_exception: [..] is not supported by rol |
| clients:search_mvt | 0 | 1 | 0 | search_mvt → 501 unsupported_operation_exception: [..] is not supported by roll search yet |
| clients:search_template | 1 | 1 | 100 |  |
| clients:searchable_snapshots | 0 | 1 | 0 | searchable_snapshots.mount → 501 unsupported_operation_exception: [..] is not supported by |
| clients:security | 0 | 14 | 0 | security.put_role → 501 unsupported_operation_exception: [..] is not supported by roll sea |
| clients:simulate | 1 | 1 | 100 |  |
| clients:snapshot | 0 | 1 | 0 | snapshot.repository_analyze → 501 unsupported_operation_exception: [..] is not supported b |
| clients:sql | 0 | 2 | 0 | sql.query → 501 unsupported_operation_exception: [..] is not supported by roll search yet |
| clients:streams | 0 | 1 | 0 | streams.logs_enable → 501 unsupported_operation_exception: [..] is not supported by roll s |
| clients:synonyms | 1 | 1 | 100 |  |
| clients:terms_enum | 0 | 1 | 0 | terms_enum → 501 unsupported_operation_exception: [..] is not supported by roll search yet |
| clients:termvectors | 1 | 1 | 100 |  |
| clients:text_structure | 0 | 1 | 0 | text_structure.find_field_structure → 501 unsupported_operation_exception: [..] is not sup |
| clients:transform | 0 | 3 | 0 | transform.put_transform → 501 unsupported_operation_exception: [..] is not supported by ro |
| clients:update | 1 | 1 | 100 |  |
| clients:update_by_query | 1 | 1 | 100 |  |
| clients:validate_query | 1 | 1 | 100 |  |
| clients:watcher | 0 | 1 | 0 | watcher.stats → 501 unsupported_operation_exception: [..] is not supported by roll search  |
| free:aggregations | 622 | 640 | 97 | search: match mismatch |
| free:analysis-common | 130 | 131 | 99 | indices.create: match mismatch |
| free:bulk | 19 | 20 | 95 | bulk: match mismatch |
| free:capabilities | 1 | 1 | 100 |  |
| free:cat.aliases | 15 | 15 | 100 |  |
| free:cat.allocation | 9 | 9 | 100 |  |
| free:cat.circuit_breaker | 2 | 2 | 100 |  |
| free:cat.component_templates | 3 | 3 | 100 |  |
| free:cat.count | 2 | 2 | 100 |  |
| free:cat.fielddata | 1 | 2 | 50 | cat.fielddata: match mismatch |
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
| free:cluster.stats | 17 | 19 | 89 | search → 400 illegal_argument_exception: Cross-cluster calls are not supported in this con |
| free:cluster.voting_config_exclusions | 7 | 7 | 100 |  |
| free:count | 10 | 10 | 100 |  |
| free:create | 9 | 9 | 100 |  |
| free:data_stream | 138 | 141 | 97 | indices.put_index_template → 400 invalid_index_template_exception: index_template [..] inv |
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
| free:indices.clone | 3 | 4 | 75 | indices.clone → 500 illegal_state_exception: index source must be read-only to resize inde |
| free:indices.create | 79 | 81 | 97 | index: match mismatch |
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
| free:indices.stats | 66 | 71 | 92 | indices.stats: match mismatch |
| free:indices.update_aliases | 18 | 18 | 100 |  |
| free:indices.validate_query | 3 | 3 | 100 |  |
| free:indices/validate_query | 1 | 1 | 100 |  |
| free:ingest | 10 | 10 | 100 |  |
| free:logsdb | 24 | 24 | 100 |  |
| free:mapping | 54 | 54 | 100 |  |
| free:mget | 26 | 26 | 100 |  |
| free:mlt | 4 | 4 | 100 |  |
| free:msearch | 7 | 8 | 87 | msearch → 200 expected catch /circuit_breaking_exception/ |
| free:mtermvectors | 6 | 6 | 100 |  |
| free:nodes.hot_threads | 6 | 6 | 100 |  |
| free:nodes.info | 18 | 18 | 100 |  |
| free:nodes.reload_secure_settings | 2 | 2 | 100 |  |
| free:nodes.stats | 35 | 35 | 100 |  |
| free:range | 15 | 15 | 100 |  |
| free:reindex | 97 | 98 | 98 | reindex → 400 script_exception: runtime error |
| free:repositories | 9 | 9 | 100 |  |
| free:scripts | 2 | 2 | 100 |  |
| free:scroll | 15 | 16 | 93 | scroll: match mismatch |
| free:search | 456 | 473 | 96 | search: match mismatch |
| free:search.highlight | 41 | 48 | 85 | search: match mismatch |
| free:search.inner_hits | 5 | 6 | 83 | search: match mismatch |
| free:search.query | 14 | 16 | 87 | search: match mismatch |
| free:search.retrievers | 21 | 21 | 100 |  |
| free:search.retrievers/result-diversification | 10 | 10 | 100 |  |
| free:search.suggest | 2 | 6 | 33 | search: match mismatch |
| free:search.vectors | 392 | 395 | 99 | indices.field_usage_stats → 501 unsupported_operation_exception: [..] is not supported by  |
| free:search_shards | 2 | 2 | 100 |  |
| free:simulate.ingest | 10 | 10 | 100 |  |
| free:snapshot.clone | 1 | 1 | 100 |  |
| free:snapshot.create | 3 | 3 | 100 |  |
| free:snapshot.delete | 3 | 3 | 100 |  |
| free:snapshot.get | 10 | 10 | 100 |  |
| free:snapshot.get_repository | 6 | 6 | 100 |  |
| free:snapshot.restore | 2 | 2 | 100 |  |
| free:snapshot.status | 3 | 3 | 100 |  |
| free:streams/logs | 0 | 25 | 0 |  |
| free:suggest | 22 | 22 | 100 |  |
| free:synonyms | 68 | 69 | 98 | search: match mismatch |
| free:tasks.cancel | 1 | 1 | 100 |  |
| free:tasks.get | 1 | 1 | 100 |  |
| free:tasks.list | 2 | 2 | 100 |  |
| free:termvectors | 4 | 4 | 100 |  |
| free:tsdb | 151 | 160 | 94 | search: match mismatch |
| free:update | 18 | 18 | 100 |  |
| free:update_by_query | 54 | 54 | 100 |  |
| **total** | **3610** | **3808** | **94** | |

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
- Segments are not ordered by `@timestamp` for searches (`380_sort_segments_on_timestamp`),
  and segment generations in `_segments` / `_cat/segments` number the current segments.
- Aggregations and sorting on keyed flattened sub-fields, `_index_mode` aggregations,
  nested sorts inside `top_hits`, and shard-level aggregation failures as partial results.
- Join fields (`has_child`, `has_parent`, `parent_id`), `percolate`, the `rrf` / `linear`
  retrievers, `rank_eval`, the `_knn_search` endpoint, the lookup `query_vector_builder`,
  dense vector node stats, `indices.disk_usage`, `indices.field_usage_stats` and the phrase
  suggester's collate/smoothing models are not implemented.
- Highlighting: weighted unified matches, `matched_fields` masks, inner hits of nested-in-
  nested objects; `exists` on fields without doc values or norms (`_field_names` is not
  indexed).
- ML, security, ES|QL, transforms, search applications and other licensed features are out
  of scope (`unsupported_operation_exception`, HTTP 501).
