# roll search — handoff (2026-10-26)

Goal: an Elasticsearch-compatible search engine in Rust, written from scratch. The targets:
- 1:1 compatibility with Elasticsearch 9.5.5, plus an OpenSearch 3.9 dialect;
- faster than Elasticsearch;
- zero idle cost;
- runs on AWS like roll tasks and roll queues;
- tests cover everything.

Design: [../architecture.md](../architecture.md), [../contracts.md](../contracts.md). Measured
compatibility: [../compatibility.md](../compatibility.md). Benchmarks:
[../../benchmarks/search.md](../../benchmarks/search.md). AWS: [../deploy-aws.md](../deploy-aws.md).

The per-lane agent reports (`*.json`: `summary`, `public_api`, `remaining`, `notes_for_others`)
say exactly what each part does and what is left. The newest are this round's lanes
(`durability`, `compat-es`, `compat-xpack`, `compat-os`, `clients`, `aws`, `perf`, and `final`
for the integration pass); `aggs`, `analysis`, `codec`, `index`, `store` and `yaml` are the
phase-B reports of those crates, still accurate for their design but older than the code.

## State

Lines are Rust source without the `tests/` directories (unit tests inside `src/` included);
the second number is the crate's `tests/` directory.

| crate | lines (src / tests) | state |
| --- | --- | --- |
| search-analysis | 92k / 1k | Done. 19,050 `_analyze` fixture cases match ES; analyzer chain pools are sharded per thread. |
| search-codec | 27k / 2k | Done: segment format, block-max postings, BKD, doc values (plus uninverted `_id` doc values), HNSW with quantization, NEON kernels (tested under qemu; no native Graviton numbers yet). |
| search-store | 7k / 3k | Done: WAL group commit with leader commits, S3 segments, manifests, cache, branching, GC, snapshot pins. |
| search-index | 44k / 11k | Done: every query type, BM25 and similarities matching ES `_explain`, highlighting, collectors with Lucene's competitive pruning, published views for wake fidelity. |
| search-aggs | 54k / 4k | Done: all aggregations, verified by oracle tests and ES fixtures (filter-by-filter, breaker budgets). |
| search-painless | 80k / 3k | Done: interpreter with closure fast paths for doc-value scripts; ES's loop limits, cancellation and error frames. |
| search-es | 144k / 2k (+ fixtures) | ES 9.5.5 baseline at 100% (3809 of 3905 run, every baseline test passes); x-pack APIs in `src/xpack/` (ES\|QL, SQL, EQL, security, transforms, …), OpenSearch dialect in `src/opensearch/` (k-NN, hybrid search, ISM API). |
| search-server | 7k / 7k | Done: multi-tenant hyper node, durable WAL acks, idle unload invisible to the ES suite, crash recovery, ownership fencing, snapshots durable through pins, differential suite. |
| search-control | 4k / 1k | Done: `/v1/search` API, catalog in SQLite or Postgres, leases with epochs, metering and pricing. |
| search-proxy | 4k / 1k | Done: owner routing, replayable bodies, TLS to pinned node certificates, Lambda mode. Never deployed to real AWS. |
| search-yaml | 11k / 1k | Done: YAML REST runner validated against real ES; baselines in `crates/search-yaml/baseline/`. |
| rolld, roll-cli | search modules | `rolld` search module (flags in `../README.md`), `roll search …` CLI; dashboard pages at `/dashboard/search`. |

## Test status (2026-10-26, tree at the end of the round)

`scripts/test-search.sh --force` (with Docker; `ROLL_PORT=19900 ROLL_JOBS=8`) is green, both
on the merged tree as the lanes left it and again after the integration fix:
- build and `cargo clippy --all-targets -D warnings` for every search crate, `rolld` and
  `roll-cli`: clean;
- `cargo test`: 1,704 tests in 110 test binaries pass (41 ignored: benchmarks and capture
  tools), `rolld` + `roll-cli` 36; the same suite ran three times in a row with no failure;
- dashboard helpers (`node --test`): 20 pass;
- ES 9.5.5 YAML: 3905 tests, 3809 passed, 1 failed (`search.vectors/210_knn_search_profile`,
  not in the baseline), 95 skipped: 0 regressions, threshold raised to 3809;
- OpenSearch 3.9.0 YAML: 1506 tests, 1455 passed, 0 failed, 51 skipped: 0 regressions,
  threshold 1455;
- differential suite: 10 of 10 (replay of 69 fixtures; seed 1, 60 cases: 2321 of 2329 steps
  equal, known differences only);
- client matrix: 11 of 11 (searchkick 540 runs, 0 failures);
- fleet tests on Postgres 9 of 9; `cfn-lint` of the AWS template clean.

## What the last round did (phase 3–5 lanes, 2026-10-23 to 2026-10-25)

Seven lanes worked in parallel on one tree, each with review and fix rounds:

- **compat-es**: the ES 9.5.5 YAML baseline went from 3610 of 3808 to all 3808. Then the module
  suites the `rest-resources` artifact doesn't ship (runtime fields, painless, ingest, mustache,
  …): 581 of the 760 that pass on ES. Filter-by-filter aggregations, a request circuit
  breaker, locale-aware date formats, the version field type, runtime fields with every
  query type, search timeouts that cancel scripts, searches off the reactor and cancelled on
  client disconnect.
- **compat-xpack**: the x-pack REST APIs (31 modules): every clients test passes. ES|QL runs
  for real on a columnar reader (STATS 9–30 ms vs ES 18–40 ms on 50k docs), with
  HyperLogLog++ `COUNT_DISTINCT`, t-digest percentiles, spatial types, ENRICH, LOOKUP JOIN,
  INLINE STATS; SQL, EQL, transforms, terms enum.
- **compat-os**: the OpenSearch dialect at its 3.9.0 baseline (1455 of 1506) plus the bundled
  k-NN plugin, neural-search hybrid queries, `_id` fielddata and the ISM API (policies are not
  executed yet).
- **durability**: op-level WAL and replay, durable async results and `.tasks`, published views
  so an idle unload is invisible (the ES suite passes the same 3809 with `--idle-unload-ms 1`),
  snapshots durable through object-storage pins with race-free sweeps.
- **clients**: the differential suite (seeded random workloads against a real ES, minimized
  fixtures, 69 committed) and the client matrix (11 unmodified clients in Docker); about 150
  compatibility fixes from their findings.
- **aws**: search-proxy with replayable bodies, TLS on every hop with pinned certificates and
  no CA, the CloudFormation stack (cfn-lint clean), Lambda front, EC2 data volumes.
- **perf**: bulk documents parsed straight from request bytes, raw `_source` splicing,
  Lucene-style competitive pruning for sorts, block scoring for function and script scores,
  Painless fast paths, zlib-rs gzip, a mimalloc purge governor. roll wins every micro row and
  every Rally `http_logs` query task (see the benchmarks).
- **final** (integration): the full `scripts/test-search.sh --force` run was green on the
  merged tree at the first attempt, and the whole cargo suite passed three times in a row (no
  flaky tests). The final benchmarks then found a per-search cost the lanes had added:
  `std::thread::available_parallelism` re-reads cgroup files on every call and the search path
  called it several times per request (28% of a `size: 0` search's CPU). It is now looked up once
  (`search_index::search::pool::cores()`, in contracts.md). Final benchmarks and this handoff.

## Performance (final run, quiet host)

[../../benchmarks/search.md](../../benchmarks/search.md), "Final run". roll wins all 17 micro rows
(1.4–4.9x wall, 2.1–7.8x server CPU; bulk 0.94 vs 1.99 s per 50k docs); Rally `http_logs` 2%:
every query task, indexing at par (240k vs 250k docs/s); Rally `geonames` 10%: every task but two
script scores (`decay_geo_gauss_script_score` 37.1 vs 34.7 ms, `field_value_script_score` 21.8
vs 20.1 ms), loading 1.5x faster. Idle RSS 24 MiB vs 2.6 GiB; a wake after idle unload answers
in 27 ms.

## Next phases

Ordered by importance; deduplicated from every lane's `remaining` list.

1. **First real AWS deployment** (aws). Nothing has run on AWS yet. Check the EC2 data-volume
   user data (device detection on Graviton/AL2023, mounted before the ECS agent starts tasks),
   `distinctInstance` with managed scaling during deploys (MaximumPercent 200 needs spare
   instances), the Lambda `Command [--lambda]` override, `ToZeroPolicy` really scaling to 0,
   Spot draining inside 120 s, a rolling upgrade from a pre-`api_tls` fleet (proxies first).
   Commit an image smoke test (the old one lived in /tmp). Fix
   `deploy/queues/aws/roll-queues.yaml:623` (`MetricIntervalUpperBound: 0` step never matches;
   use `MetricIntervalLowerBound: 0`, as `roll-search.yaml` now does).
2. **Search operations safety** (compat-es, server). Register searches as tasks
   (`indices:data/read/search` in `_tasks`, `_tasks/_cancel`; wire `SearchControl.cancelled`
   to `TaskRegistry`) so graceful shutdown doesn't wait on a search without timeout; a bounded
   search thread pool that queues and rejects with 429 `es_rejected_execution_exception`
   (today only tokio's 512 blocking threads bound it); answer a corrupt gzip/deflate request
   body with a 400 instead of dropping the connection (`search-server` `http.rs`).
3. **Result differences in common queries** (compat-es, clients):
   - `query_string` doesn't group consecutive bare terms like Lucene with
     `splitOnWhitespace=false`: `foo bar` on a keyword field must be the term `foo bar`
     (fix in `search/query_string.rs` `Parser::query` by merging bare-term runs into one
     `field_text` call);
   - timed-out searches report the total up to the interruption where ES's count shortcut
     reports all;
   - `numeric_type: long` on floating fields picks top hits by the untruncated value (ties at
     the page edge; truncation belongs in search-index's comparator);
   - `terms` under `terminate_after` with `match_all` (ES counts whole segments);
   - empty clauses in `filters` / `adjacency_matrix` filters must fail at parse time;
   - `top_hits` / `top_metrics` sorts on `unsigned_long` (raw sortable long, missing as Long
     extremes);
   - `_termStats` for a duplicate-valued text `terms` query (ES 0 unique terms, roll 2);
   - the 13 open differential signatures of seed 23 (`_explain`/`_validate` descriptions,
     `_msearch` shard failure order, highlight on `must_not`-only clauses, DFS over several
     indices, sort on a completion field) and the older items in compatibility.md.
4. **Durability edges** (durability). Write a marker pin for empty shards so a missing pin is
   an error instead of a silently empty restore; show snapshots as `IN_PROGRESS` to concurrent
   `GET _snapshot` / `_status`; real file sizes in `_snapshot/_status`; the published view
   falls back to showing everything after a merge over 2M docs (or dropping a shown doc);
   cross-cluster search statistics reset at a wake; an idle (5 min+) unload doesn't count the
   flush ES's `flushOnIdle` would.
5. **OpenSearch ISM execution** (compat-os). Policies are stored and validated but never run
   (explain has no state/action/info; retry reports no metadata). Also: POST explain filter
   bodies, explain ordering over index lists, a fixture for the OS `SnapshotInfo` shape (needs
   a `path.repo` cluster), `_id` fielddata circuit breaker and real release on
   `_cache/clear`, `docvalue_fields` wildcard order over metadata fields, `verbose_pipeline`,
   trained k-NN models, hybrid collapse scoring, k-NN stats fetch counts, analyzed derived
   text fields.
6. **Performance** (perf). Hit fetching decompresses a whole 16 KiB zstd stored-fields chunk
   per hit (36% of a 10-hit `match`'s CPU; Lucene's LZ4 stops at the document's end): a faster
   block codec or partial decompression for stored fields, and the ~0.1 ms more CPU a 10-hit
   fetch costs than in round 2. The two geonames script scores (`field_value_script_score`,
   `decay_geo_gauss_script_score`). `collector/collapse.rs` still calls `natives_of` per comparison
   (apply the per-entry natives + generation scheme of `top_field.rs`); TopFieldCollector's
   sorted-Vec queue memmoves on insert (a heap needs an ordinal-plus-bytes fallback
   comparator); intra-shard concurrent search (slices when shards < cores); `search_after` sorts on many-segment indices; bulk p50 service time;
   Rally `nyc_taxis` / `pmc` / `so` / `noaa` subsets (they need ~80 GB of scratch to
   decompress); native Graviton numbers. Smaller: build the runtime-field evaluator map once
   per mapping instead of per shard.
7. **ES|QL and x-pack depth** (compat-xpack). `FORK`, `CHANGE_POINT`, `COMPLETION`/`RERANK`,
   `TS`/PromQL and `aggregate_metric_double`, flattened and `dense_vector` columns,
   `unsigned_long` above `i64::MAX`, `EXPLAIN`, `ST_*` distance/relations/aggregations; a
   less emulated `profile` (evaluator details, attribute ids, maxPageSize); `MV_DEDUPE` of
   constant `[0.0, -0.0]`; transform dest-mapping key order with several object aggs and
   range keys (`*-1` vs `*-1.0`); ip `terms_enum` prefixes and bad `search_after` errors.
8. **Scripting and ingest gaps** (compat-es). `geo_shape` runtime fields; `geo_point` runtime
   fields in geo queries, `geo_bounds` and distance sorts; index-time scripts on mapped
   fields; `params._fields`; nested runtime-field cause chains; the `registered_domain`,
   `user_agent` and `attachment` processors; ingest node stats and verbose simulate details;
   processor-built maps in insertion order (ES `LinkedHashMap`); dateline-crossing H3 cells as
   MultiPolygon in `geo_grid`; aggregation parse error locations.
9. **Known gaps listed in compatibility.md** ("Known gaps in the document and search APIs"):
   by-query tasks not throttled once async, reindex from remote without TLS, keyed flattened
   aggregations, nested sorts inside `top_hits`, shard-level aggregation failures as partial
   results, weighted unified highlighting, and the rest of that list.

## Rules for the machine

- Many agents can build in parallel: give each its own `CARGO_TARGET_DIR`
  (`target/agents/<key>`) and its own `ROLL_PORT` range, build only the crates it needs
  (`-p`), and never `cargo clean` shared dirs. Delete an agent's target dir when it is done.
- The suites are downloaded by `scripts/search-yaml-suites.sh fetch` into `target/search-yaml/`
  (never commit them; they are not Apache-licensed). Don't leave a `search-dev` listening
  inside the `ROLL_PORT` range a YAML run uses: the runner treats it as its own server.
- One shared reference Elasticsearch for probes and fixture captures (prefix every object
  with your key, clean up, never send pathological requests to it — use a private container):
  ```sh
  docker run --rm -d --name roll-es-ref -m 4g -e discovery.type=single-node \
    -e xpack.security.enabled=false -e xpack.license.self_generated.type=trial \
    -e ES_JAVA_OPTS="-Xms2g -Xmx2g" -e path.repo=/tmp -e action.destructive_requires_name=false \
    -p 127.0.0.1:19200:9200 docker.elastic.co/elasticsearch/elasticsearch:9.5.5
  ```
- The REST fixture capture harness deletes every `esm-*` object on its target: capture against
  a private container.
- Benchmarks: `scripts/bench-search.sh` pins both targets to the same CPU set; run them only
  while nothing else builds, and keep the YAML runner's ports away from the bench ports.
- Workflows must retry agents that get rate-limited (HTTP 429). `workflow-phase-*.js` in this
  directory are the phase B/C scripts, kept for reference.
