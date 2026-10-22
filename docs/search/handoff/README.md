# roll search — handoff (2026-10-21)

Goal: an Elasticsearch-compatible search engine in Rust, written from scratch. The targets:
- 1:1 compatibility with Elasticsearch 9.5.5, plus an OpenSearch dialect;
- faster than Elasticsearch;
- zero idle cost;
- runs on AWS like roll tasks and roll queues;
- tests cover everything.

Design: [../architecture.md](../architecture.md), [../contracts.md](../contracts.md). The per-crate agent reports (`*.json`, field `remaining`) list exactly what is left in each crate.

## State

| crate | lines | state |
| --- | --- | --- |
| search-analysis | 91k | Done. 19,050 `_analyze` fixture cases match ES. The standard analyzer runs at about 150 MB/s per core. |
| search-codec | 27k | Done: segment format, block-max postings, BKD, doc values, HNSW with quantization. NEON kernels are missing. |
| search-store | 9k | Done and reviewed: WAL group commit, S3 segments, manifests, cache, branching, GC. |
| search-index | 50k | Done and reviewed: every query type, BM25 and similarities matching ES `_explain`, highlighting, collectors. |
| search-aggs | 48k | Done: all aggregations, verified by oracle tests and ES fixtures. A round-3 agent was still running at handoff. |
| search-painless | 74k | Mostly done. The round-2 agent was still running at handoff; review not done yet. |
| search-es | 51k | Metadata half done and reviewed: 820 of 1200 metadata YAML tests pass. Document and search APIs are still 501. |
| search-server | 2k | Skeleton: multi-tenant hyper node and the `search-dev` binary. No persistence or idle unload yet. |
| search-yaml | 12k | Done and reviewed: a YAML REST runner validated against real ES. Baseline is in `crates/search-yaml/baseline/`. |
| search-control | stub | not started |

## Next phases

1. Finish phase B (`workflow-phase-c1.js` and `workflow-phase-c2.js` are ready-made scripts for steps 2 and 4; they read reports from `~/.cache/roll-search/reports/`, so copy `docs/search/handoff/*.json` there first):
   - painless round 2/3, then review and fix;
   - aggs round 3, then review and fix;
   - reviews of codec and analysis.
2. **Wire the engine.** Build the document and search APIs in search-es on top of search-index, search-aggs and search-painless:
   - the document APIs: index, bulk, get/mget, update, delete, by-query, reindex;
   - the search APIs: search, msearch, count, explain, scroll/PIT, termvectors, search templates;
   - ingest pipeline execution.

   In search-server, add real persistence through search-store (WAL acks, refresh, flush, recovery on wake) and idle unload.
3. **Compatibility.**
   - Raise YAML pass rates to the baseline (`scripts/search-yaml-suites.sh`).
   - Add a differential suite against ES in Docker.
   - Add a client matrix of unmodified clients in Docker (py, js, java, go, ruby, php, searchkick).
   - Complete the OpenSearch dialect.
4. **Product.**
   - search-control: the `/v1/search` API in contracts.md, metering and pricing.
   - rolld: `crates/rolld/src/search.rs`.
   - CLI: `roll search`.
   - Dashboard: `/dashboard/search`.
   - Tests: `scripts/test-search.sh`, `pnpm test:search`, `.github/workflows/search.yml` and a line in ci.yml.
5. **AWS and performance.**
   - A `search-proxy` crate, cluster ownership leases with epochs, `deploy/search/aws/roll-search.yaml` plus a Dockerfile, and `docs/search/deploy-aws.md`. Follow the patterns in `deploy/queues/aws` and `deploy/aws`; the architecture section is "On AWS".
   - Rally runs against ES 9.5.5 written up in `docs/benchmarks/search.md`, with tuning, including aarch64 NEON.

## Rules for the machine (RAM)

- Run at most one cargo build at a time. Every agent uses `CARGO_TARGET_DIR=<repo>/target/agents/shared CARGO_BUILD_JOBS=4 cargo … -p <crate>`; the lock serializes the builds.
- Before every build or docker run, wait until at least 6 GB of RAM is available.
- Run at most 3 agents at once, and keep one shared reference ES:
  ```sh
  docker run --rm -d --name roll-es-ref -m 1536m \
    -e discovery.type=single-node -e xpack.security.enabled=false \
    -e ES_JAVA_OPTS="-Xms768m -Xmx768m" -p 127.0.0.1:19200:9200 \
    docker.elastic.co/elasticsearch/elasticsearch:9.5.5
  ```
- The ES 8/9 YAML suites are downloaded at test time by `scripts/search-yaml-suites.sh` into `target/`. Never commit them; they are not Apache-licensed.
- Workflows must retry agents that get rate-limited (HTTP 429), as `workflow-phase-b.js` does.
