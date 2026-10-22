# roll search — architecture

roll search is an Elasticsearch-compatible search engine written in Rust from scratch. Clients
speak the Elasticsearch REST API to it (or the OpenSearch dialect of it): the official clients,
searchkick, Laravel Scout, Spring Data, django-elasticsearch-dsl and Rally work unchanged, and
switch by changing the URL. It is built for what Elasticsearch is worst at: many small clusters
(one per app, branch and preview environment) that cost nothing while idle, wake in milliseconds,
branch instantly and keep their data on object storage.

## Goals

- **Compatibility.** The Elasticsearch 9.x REST API, accepting `compatible-with=7|8|9`, and the
  OpenSearch 2.x/3.x dialect. Measured by Elastic's own YAML REST suites, the official clients and
  a differential suite against a real Elasticsearch in Docker (see [testing](#testing)).
- **Zero idle cost.** A cluster nobody calls holds no memory, runs no task and costs only its
  stored bytes. A node with no loaded cluster runs no timer at all.
- **Faster than Elasticsearch** on the same hardware: query latency at or below Lucene (block-max
  pruning, SIMD decoding, document reordering on merge), higher indexing throughput (group-commit
  WAL instead of a per-request translog fsync, no JVM), a fraction of the memory, and cold starts
  in milliseconds instead of tens of seconds. Measured with Rally against Elasticsearch 9 in Docker
  ([docs/benchmarks/search.md](../benchmarks/search.md)).
- **Branching.** A branch of a cluster is a manifest copy. Segments are immutable objects shared
  by the parent and every branch, so a preview environment gets production-shaped indexes without
  reindexing.

## Shape

```
          clients (Elasticsearch / OpenSearch REST, official clients, Rally)
                                   │  HTTP/1.1 + TLS, Basic / ApiKey / Bearer
                    ┌──────────────┴──────────────┐
                    │ search-server (node)        │  tenants → clusters, auth, wake-on-request,
                    │  search-es: REST semantics  │  idle unload, quotas
                    └──────────────┬──────────────┘
          ┌────────────────────────┼─────────────────────────────┐
          │ search-index           │ search-aggs  search-painless │
          │ writer · reader · queries · scoring · collectors      │
          │ search-codec: segment format    search-analysis       │
          └────────────────────────┬─────────────────────────────┘
                                   │ WAL group commit · segment upload · manifests
                         ┌─────────┴─────────┐
                         │ search-store       │  local disk = cache (byte budget)
                         └─────────┬─────────┘
                                   │ pg_storage::Objects
                          S3 (or a local directory)
```

## Crates

| crate             | role |
| ----------------- | ---- |
| `search-analysis` | char filters, tokenizers, token filters, analyzers and normalizers with Elasticsearch's names, settings and output (`_analyze` is byte-for-byte comparable) |
| `search-codec`    | the immutable segment format: term dictionary, postings with block-max impacts, norms, doc values, points (BKD), stored fields, vectors (HNSW + quantization), live docs |
| `search-index`    | one shard's engine: in-memory indexing buffer, refresh, segment flush and merges, deletes and updates, the query/weight/scorer model, BM25, collectors, highlighting |
| `search-aggs`     | aggregations over doc values: buckets, metrics, pipelines, with Elasticsearch's semantics and response shapes |
| `search-painless` | the Painless scripting language: parser, type checker, compiler to a compact bytecode, VM, and the script contexts Elasticsearch exposes |
| `search-store`    | durability and storage: the write-ahead log (group commit to object storage), segment upload, per-shard manifests, the local cache, branching and garbage collection |
| `search-es`       | Elasticsearch semantics: mappings, settings, index templates, aliases, the Query DSL, the document and search APIs, `_cat`, cluster and node APIs, errors with Elasticsearch's types and texts |
| `search-server`   | the node: HTTP listener, tenants (clusters), authentication, wake-on-request, idle unload, request limits, TLS |
| `search-control`  | the roll API (`/v1/search/...`) for the dashboard and CLI: clusters, credentials, branches, usage, metering and pricing |
| `search-proxy`    | the front proxy on AWS: routes requests to the node owning a cluster, wakes the fleet |
| `search-yaml`     | a runner for Elasticsearch's YAML REST test format, usable against any URL (roll or a real Elasticsearch) |
| `rolld`           | runs everything on one node (`crates/rolld/src/search.rs`) |

`search-analysis`, `search-codec` and `search-painless` depend on nothing else in roll search.
`search-index` builds on codec and analysis; `search-aggs` on index; `search-es` on all of them;
`search-server` on es and store; `search-control` on server.

## Data model

- **Cluster** — the tenant. One per app, branch or preview environment. It has its own indexes,
  aliases, templates, settings, credentials and a URL. It reports itself in `GET /` with its own
  `cluster_name` and `cluster_uuid`. Its dialect (`elasticsearch` or `opensearch`) is a cluster
  setting.
- **Index** → **shards** (`index.number_of_shards`, default 1). Documents are routed exactly like
  Elasticsearch (`murmur3(_routing or _id)` with the routing factor), so `_routing`, `preference`
  and per-shard scoring behave the same. Replicas are accepted and reported (as unassigned on a
  single node, matching a one-node Elasticsearch) but not materialized: durability comes from the
  WAL and object storage, not copies.
- **Shard** → **segments**. A segment is one immutable object. Deletes and updates mark documents
  in a small per-segment live-docs object written next to it (new generation per commit).
- **Sequence numbers.** Every operation on a shard gets a `_seq_no` and the shard's
  `_primary_term`; documents carry `_version`. Optimistic concurrency (`if_seq_no`,
  `if_primary_term`, `version_type=external|external_gte`) works as in Elasticsearch.

## Write path

1. `_bulk`/`_doc` requests are parsed, mapped (dynamic mapping updates the index mapping exactly
   as Elasticsearch does) and analyzed.
2. The operation is appended to the cluster's WAL. The WAL is group-committed: every shard of
   every cluster on the node shares one append stream, flushed every `wal_flush` (5 ms) or 4 MiB,
   to local disk (fsync) and, in the background, shipped to object storage. A request returns once
   its records are durable on local disk (`index.translog.durability: request`, the default) or
   immediately (`async`). With `--search-sync-objects` the ack waits for object storage instead,
   for nodes with ephemeral disks.
3. The document goes into the shard's in-memory buffer (a writable segment: hash-based term
   dictionary, growable postings) and the live version map (`_id → seq_no, version, location`),
   which serves realtime `GET`.
4. **Refresh** turns the buffer into an immutable in-memory segment and opens a new reader. It
   runs on `_refresh`, `?refresh=true|wait_for`, or every `index.refresh_interval` — but only for
   shards that were searched in the last `index.search.idle.after` (30 s), as in Elasticsearch's
   search-idle mode. A shard nobody searches does no periodic work.
5. **Flush** writes refreshed segments to the local cache and object storage, writes the shard
   manifest (the commit point: segments, live-docs generations, max seq_no, mapping version) and
   trims the WAL. It runs when the WAL holds `index.translog.flush_threshold_size` (512 MiB) for
   the shard, on `_flush`, and before a cluster unloads.
6. **Merges** (tiered policy, Lucene's defaults) run in the background only while a shard is
   being written. Merged segments are reordered (bipartite graph partitioning) when
   `index.sort` is not set, which shrinks postings and speeds up queries.

## Read path

- A searcher is a point-in-time set of segments plus live docs. Searches across shards run in
  parallel on a bounded pool: query phase per shard (top-k plus aggregation partials), reduce,
  fetch phase (`_source`, stored fields, highlighting, `docvalue_fields`, `fields`, inner hits).
- Segments are read through `search-codec` readers over memory-mapped files in the local cache.
  A shard opening on a node where its segments aren't cached fetches them from object storage in
  parallel first; small segments are fetched whole, and large segments' term dictionaries and
  skip data come first so a query can start before the postings finish downloading.
- Scoring is Lucene's BM25 (`k1 = 1.2`, `b = 0.75`, the same norm encoding and idf), so scores
  and orderings match Elasticsearch to the float. Top-k disjunctions use block-max MaxScore and
  WAND; conjunctions use block-max skipping; `track_total_hits` defaults to 10 000 as in
  Elasticsearch, which lets the pruning work.

## Idle cost

- **A cluster** with no request for `--search-idle-unload` seconds (300) is unloaded: the node
  waits for its WAL to be durable, flushes any unflushed shard, drops its readers, buffers and
  caches, and keeps only a directory entry. The next request wakes it: read its metadata, open
  shard manifests (a few small objects), memory-map cached segments. Waking a small cluster takes
  a few milliseconds when its segments are in the local cache and one object-store round trip per
  segment when they are not.
- **A node** with no loaded cluster runs no background loop: refresh, merge, WAL and metering
  timers exist only while a cluster that needs them is loaded. `rolld` returns freed memory to
  the OS (mimalloc collect on park).
- **Local disk** is a cache with a byte budget (`--search-cache-bytes`). Evicting a segment is
  always safe: every segment in a manifest is already in object storage.

## Branching

`branch(parent, child, at?)`: flush the parent's shards, then write the child's cluster metadata
and shard manifests pointing at the parent's segments and live-docs generations. Nothing is
copied: live-docs objects are immutable and uniquely keyed, so a side that deletes afterwards
writes a new generation and the other keeps reading its own. Point-in-time
branches (`at`) replay the parent's WAL up to that time into new segments for the child. Garbage
collection deletes a segment object only when no manifest of any cluster references it.

## Multi-tenancy and authentication

A request finds its cluster by, in order: Basic auth username (`<cluster>` with the cluster's
password, or `<user>@<cluster>`), `Authorization: ApiKey base64(id:secret)` where the id names the
cluster, `Authorization: Bearer <cluster>_<secret>`, or the TLS SNI/`Host` label under the node's
domain (`<cluster>.search.<domain>`). Secrets are compared in constant time. Each cluster has
request-rate and in-flight byte budgets so one tenant can't starve the node.

## On AWS

Same shape as roll tasks and roll queues (`deploy/aws`, `deploy/queues/aws`): one image,
a CloudFormation stack (`deploy/search/aws/roll-search.yaml`), arm64 Graviton, Spot where it is
safe, no NAT (S3 gateway endpoint).

```
 clients ──► NLB (TLS) or Lambda function URL
               │
               ▼
 search-proxy (Fargate arm64, ¼ vCPU, per AZ) ── routes each request to the node that owns its
               │                                cluster; holds it and wakes the fleet if none run
               ▼
 search nodes: rolld --search-only (ECS on Graviton, on-demand base + Spot) ×N
   │ ownership leases, cluster catalog                │ WAL group commits, segments, manifests
   ▼                                                  ▼
 Postgres (roll postgres / RDS) or S3 leases     S3 Standard (+ S3 Express One Zone for the WAL)
```

- **Nodes are stateless.** Local disk is only a cache. A cluster is owned by one node at a time
  through a lease with an epoch (compare-and-set in the metadata store); the epoch is written into
  every WAL batch and manifest, so a deposed owner can't commit. Owners are picked by rendezvous
  hashing over live nodes, preferring the client's AZ.
- **Acks on ephemeral disks.** On AWS, writes are acknowledged once the WAL batch is in object
  storage (`--search-sync-objects`): S3 Express One Zone when configured (single-digit ms), S3
  Standard otherwise. Batches are shared by every cluster on the node, so request costs follow
  nodes and traffic, not tenants.
- **Failover and deploys.** A node that stops heartbeating loses its leases; the next owner opens
  the cluster from its manifests and replays the WAL after them. SIGTERM (deploy, Spot reclaim)
  flushes and hands clusters over before exit, and the proxy retries in-flight requests.
- **Zero idle.** Clusters unload after `IdleClusterSeconds`. The node fleet scales on CPU down to
  `MinNodes`, which may be 0: with no node running, the proxy holds the first request, starts a
  node (ECS `UpdateService`) and forwards once it heartbeats, as `mq-proxy` does.
- **Metering** per cluster: requests, stored GB-months, loaded GB-hours; the dashboard compares
  the cost with Elastic Cloud and Amazon OpenSearch Service for the same workload.

## Testing

- **Unit tests** in every crate, next to the code.
- **YAML REST suites** (`search-yaml`): Elastic's `elasticsearch-clients-tests` (Apache-2.0,
  vendored), the Elasticsearch 9.x core suite from the official `rest-resources-zip` (downloaded
  at test time into `target/`, never committed — it isn't Apache-licensed) and the OpenSearch
  suite (Apache-2.0). The runner itself is validated against a real Elasticsearch in Docker.
  Every skipped test is listed with a reason in `crates/search-server/tests/yaml/skips.txt`;
  pass counts are tracked in [compatibility.md](compatibility.md).
- **Differential suite**: the same requests against roll and `elasticsearch:9` in Docker,
  comparing hits, scores, aggregations and errors.
- **Client matrix**: unmodified official clients (Python, JavaScript, Java, Go, Ruby, PHP) and
  searchkick, in Docker.
- **Benchmarks**: Rally tracks against both, `scripts/bench-search.sh`.

`scripts/test-search.sh` (`pnpm test:search`) runs all of it and skips itself when nothing
search-related changed.
