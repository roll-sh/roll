# roll search

Elasticsearch-compatible search clusters that cost nothing while idle, wake in milliseconds,
branch instantly and keep their data on object storage. Clients speak the Elasticsearch REST API
(or OpenSearch's): the official clients, searchkick, Laravel Scout and Spring Data work by
changing the URL. Semantic and hybrid search work as on Elasticsearch 9: `semantic_text` fields
embed through the model provider you configure (`_inference`: OpenAI or any compatible server,
Cohere, Anthropic; the key sealed in the cluster), and `rrf`/`linear` retrievers mix them with
lexical search.

Design: [architecture.md](architecture.md). Cross-crate interfaces: [contracts.md](contracts.md).
On AWS search runs on the roll cloud fleet with every other product (docs/cloud/README.md).

## Run it

`rolld` serves search next to the other roll services:

```sh
cargo run -p rolld                      # REST on 127.0.0.1:59200, roll API on 127.0.0.1:7070
pnpm dev                                # dashboard: /dashboard/search
```

| flag (env)                                         | default           | |
| -------------------------------------------------- | ----------------- | - |
| `--search-http` (`ROLL_SEARCH_HTTP`)               | `127.0.0.1:59200` | Elasticsearch REST listener |
| `--search-tls-cert`, `--search-tls-key`            |                   | serve HTTPS (PEM) |
| `--search-domain` (`ROLL_SEARCH_DOMAIN`)           |                   | clusters at `<name>.<domain>` (SNI / `Host` routing) |
| `--search-idle-unload` (`ROLL_SEARCH_IDLE_UNLOAD`) | `300`             | seconds without requests before a cluster sleeps (default for new clusters) |
| `--search-cache-bytes`                             | 10 GiB            | local disk cache for segments |
| `--search-sync-objects`                            | off               | acknowledge writes once the WAL is in object storage (ephemeral disks) |
| `--search-proxy-protocol`                          | off               | PROXY protocol headers (behind an NLB or HAProxy) |
| `--search-history` (`ROLL_SEARCH_HISTORY`)         | `86400`           | point-in-time branching window, seconds (0: off) |
| `--search-gc-interval`                             | `3600`            | garbage collection of unreferenced objects (0: off; one node per bucket) |
| `--search-test-features`                           | off               | publish Elasticsearch's test features (YAML suites through `--cluster-hook`; test deployments only) |
| `--no-search`, `--search-only`                     |                   | shorthands for `ROLL_PRODUCTS` without search, or with only search |
| `--search-public-url` (`ROLL_SEARCH_PUBLIC_URL`)   |                   | the front clients use (a load balancer): cluster URLs name it |

Search is one of rolld's products (`ROLL_PRODUCTS`, `crates/rolld/src/products.rs`): `/v1/search`
sits behind rolld's operator token (`ROLL_ADMIN_TOKEN`) like every product's API. On a cluster
(roll cloud: `ROLL_CLUSTER_SECRET`, a cluster backend, `ROLL_SYSTEM_DATABASE`, S3) every search
cluster is served by the holder of its lease `search:cluster:<name>`, its catalog is in the system
database and any node's REST port forwards a request to the owner (architecture.md, "On the
fleet").

Storage is rolld's: `--s3-bucket` (with `--s3-prefix`, `--s3-endpoint`), or `<home>/objects`.
Local disk under `<home>/search/` holds `search.db` (the control plane's SQLite) and the cache.

## Use it

```sh
roll search clusters create app            # prints the URL, user and password (shown once)
curl -u app:<password> http://127.0.0.1:59200/
roll search request app PUT /products '{"mappings":{"properties":{"title":{"type":"text"}}}}'
roll search request app POST /products/_bulk @products.ndjson
roll search request app GET '/products/_search?q=title:shoes'
roll search clusters branch app app-preview              # instant copy, its own password
roll search clusters branch app app-yesterday --at 1d    # as of a day ago
roll search clusters indices app
roll search clusters usage app                           # cost vs Elastic Cloud / Amazon OpenSearch
roll search clusters password app | unload app | delete app | url app | show app | list
roll search index create app books '{"mappings":{"properties":{"title":{"type":"text"}}}}'
roll search docs put app books 1 '{"title":"dune"}' --refresh
roll search index bulk app books @books.ndjson --refresh
roll search index search app books --q title:dune
roll search docs get app books 1 | delete app books 1
roll search keys create app worker --expires-in 30d   # a data-only API key, shown once (--access admin for all)
roll search keys list app | revoke app <id>
roll search index show app books | mapping … | delete-by-query … | delete app books
```

Semantic search (or the dashboard's Inference tab):

```sh
OPENAI_API_KEY=… roll search inference create app emb --model text-embedding-3-small   # key from the environment, sealed in the cluster
roll search request app PUT /articles '{"mappings":{"properties":{"body":{"type":"semantic_text","inference_id":"emb"}}}}'
roll search request app POST '/articles/_doc?refresh=true' '{"body":"Passwords can be reset from the sign-in page."}'
roll search request app POST /articles/_search '{"query":{"semantic":{"field":"body","query":"forgot my password"}}}'
roll search inference list app | test app emb "some text" | delete app emb
```

A cluster's name is its identity: the Basic auth user, the host label under `--search-domain`,
`cluster_name` in `GET /`. Names are DNS labels (lowercase letters, digits, `-`) and can't
change. Clients authenticate with Basic auth (`<name>:<password>`, or any user at the cluster's
own host), `Authorization: ApiKey base64(<name>:<password>)` or
`Authorization: Bearer <name>_<password>`.

## roll API

`/v1/search` on the rolld API port (`crates/search-control/src/api.rs`). Errors are
`{"error": "..."}` with 400, 404, 409, 413 (a console request over the cluster's body limit) or
500.

| method | path | |
| ------ | ---- | - |
| GET | `/v1/search` | version, endpoint, storage, defaults, pricing, loaded clusters |
| GET | `/v1/search/clusters` | list |
| POST | `/v1/search/clusters` | create `{name, flavor?, parent?, at?, idle_timeout_s?, max_in_flight?, max_body_mb?, destructive_requires_name?}`; answers with `url`, `username`, `password` (once) and `id` — the shape `search-yaml run --cluster-hook` expects |
| GET | `/v1/search/clusters/{id or name}` | settings, `status` (`loaded` with live index count and requests in flight, or `asleep` with `wakes_at`, unix seconds, when a background job is due to wake it), last metered `indices`/`docs`/`stored_bytes` and `own_bytes` (what a branch references that its parent's latest commit points don't; all of `stored_bytes` for a cluster that isn't a branch), `url`, `branches` |
| PATCH | `/v1/search/clusters/{id}` | `flavor`, `idle_timeout_s` (0: always loaded), `max_in_flight`, `max_body_mb`, `destructive_requires_name`; the idle timeout, body limit and `max_in_flight` apply at once (also to a cluster that is waking up); a new flavor or `destructive_requires_name` needs a reload: the cluster is put to sleep now, or as soon as no request runs (always-on clusters included), and keeps its current ones until then |
| DELETE | `/v1/search/clusters/{id}` | waits for running requests and for a wake in progress (which is locked out and unloaded, never left serving); branches keep working |
| GET, POST | `/v1/search/clusters/{id}/branches` | list; branch `{name, at?}` (`at`: Unix seconds within `--search-history`) |
| POST | `/v1/search/clusters/{id}/password` | rotate; the new password is in the answer, once. From the answer on, the old one fails, also on a cluster that was waking up |
| POST | `/v1/search/clusters/{id}/unload` | sleep now (`{"unloaded": false}` while requests run) |
| GET | `/v1/search/clusters/{id}/indices` | `_cat/indices` as JSON numbers: health, status, shards, docs, sizes; docs and sizes of open indices come from the engine (as `_stats` reports them: Lucene documents, nested ones included, unflushed writes included). Wakes the cluster |
| POST | `/v1/search/clusters/{id}/request` | console: `{method, path, body?}` (a JSON body, sent byte for byte so `_source` keeps its key order, or a string such as NDJSON, sent as is) → `{status, headers, body, took_ms, truncated}`. Accepts bodies up to the cluster's `max_body_mb` (the request may be up to twice that plus 1 MiB, for JSON escapes) |
| GET | `/v1/search/clusters/{id}/usage?days=30` | loaded seconds, GiB-hours, requests, stored GiB-months, cost, and the same workload on Elastic Cloud Hosted, an Amazon OpenSearch Service domain and OpenSearch Serverless |
| POST | `/v1/search/clusters/{id}/keys` | a new API key of the cluster `{name, expires_in_s?, access?}` (`access`: `admin`, the default, or `data`: a link's role, data, templates, pipelines and lifecycle policies without security) → `{id, name, api_key, url, expires_at?}`: `api_key` is `base64(<key id>@<cluster>:<key>)` for `Authorization: ApiKey …`, in this answer only (roll secrets' managed links use it instead of sharing the password); on an OpenSearch-flavor cluster (whose clients sign in with a user) the key is an internal user of its own: `{id, name, username, password, url, expires_at?}` without `api_key`, `username` being `<user>@<cluster>`, refused once `expires_in_s` passes |
| DELETE | `/v1/search/clusters/{id}/keys/{key}` | revoke it (204; 404 when it's gone) |
| GET, PUT, DELETE | `/v1/search/clusters/{id}/indices/{index}` | Elasticsearch's `GET`/`PUT`/`DELETE /<index>`: mappings, settings and aliases; create (body: `mappings`, `settings`); delete |
| GET, PUT | `/v1/search/clusters/{id}/indices/{index}/mapping` | `GET`/`PUT /<index>/_mapping` |
| GET | `/v1/search/clusters/{id}/indices/{index}/settings` | `GET /<index>/_settings` |
| POST | `/v1/search/clusters/{id}/indices/{index}/refresh` | `POST /<index>/_refresh` |
| POST | `/v1/search/clusters/{id}/indices/{index}/analyze` | `POST /<index>/_analyze` (a read) |
| POST | `/v1/search/clusters/{id}/indices/{index}/search` | `POST /<index>/_search` (reads take index expressions: `logs-*`, `a,b`) |
| POST | `/v1/search/clusters/{id}/indices/{index}/docs` | `POST /<index>/_doc`: index a document under a new id |
| GET, PUT, DELETE | `/v1/search/clusters/{id}/indices/{index}/docs/{doc}` | `/<index>/_doc/<id>` |
| POST | `/v1/search/clusters/{id}/indices/{index}/bulk` | `POST /<index>/_bulk` (NDJSON) |
| POST | `/v1/search/clusters/{id}/indices/{index}/delete_by_query` | `POST /<index>/_delete_by_query` |

The per-index routes are one Elasticsearch operation each, so the cluster, the index and the kind
of access (read, write, delete) are in the route: roll's scoped tokens and MCP tools map onto
them. A route's request runs as the cluster's owner narrowed to the index names in its path
(`x-roll-index-scope`: like an API key of the owner whose one role grants every privilege on
those names), so it reaches no other index: a bulk line's `_index` or an `mget` doc naming
another index is refused (403), and so is a lookup inside a query (a `terms` lookup, an indexed
`geo_shape`, `percolate`'s stored document, `more_like_this` documents, a `lookup` runtime
field): those are read as the caller, as Elasticsearch authorizes them. A document an ingest
pipeline moves (`_index`, `reroute`) is authorized again where it lands. ES|QL and SQL read as their client:
their field resolution and searches carry its credentials (wildcards expand to the indices it
may read, a lookup join's index is checked like `FROM`'s), and where its roles restrict
documents or fields the direct shard reads give way to searches, which apply them. Field
level security covers every read path a field's content or name can come out of: `_source`
(hits, gets, inner hits), `fields`, scripts' `params._source` and `doc` (script fields, runtime
fields, sort scripts), term vectors, field caps and mappings (`control.rs`
`field_security_hides_a_field_on_every_read_path`, 22 requests and 3 listings). They answer with Elasticsearch's own status and body, and pass the query string through
(`refresh`, `routing`, `size`, …). A write names exactly one index; a name starting with `_` is
refused (those are Elasticsearch's own APIs: use the console). Bodies may be up to the cluster's
`max_body_mb`.

To run the YAML REST suites against clusters made through the API, start rolld with
`--search-test-features` and point the runner at the create endpoint:
`search-yaml run --cluster-hook http://127.0.0.1:7070/v1/search/clusters --jobs 8 …` (each worker
gets its own cluster, deleted afterwards).

The dashboard (`/dashboard/search/<cluster>`) lists indices; selecting one opens its documents
(paged `_search` with a query string, as rows or as a table: the most common fields by default,
a column picker over the page's paths and the mapping's leaf fields, kept per index in the
browser, and click-to-sort on sortable columns, `.keyword` sub-fields for text; a time-based
index adds a time range and documents over time, newest first), mapping (a
field table: types, inference endpoints, vector settings, analyzers; or the JSON) and settings,
with refresh and delete. Import fills an index from a file (NDJSON, a JSON array, or CSV whose
columns are typed from the first 1,000 rows), previewed first and sent in `_bulk` batches through
the index's own route (`lib/search-import.ts`). Documents can be added, edited and deleted in place
(`refresh=wait_for`, routing kept), and any view opens in the Dev Tools console. The console
completes as you type (Ctrl+Space to ask): methods, paths and index names on the first line,
then the API's URL query parameters after `?` and their allowed values after `=` (from
Elasticsearch's REST API spec, `scripts/gen-search-params.mjs`);
body keys, query and aggregation types, mapping parameters, field types and the index's field
names (from its mapping) in the body, line by line for `_bulk` and `_msearch`. It highlights JSON
and copies a request as curl; a profiled search opens on its profile tree. The dashboard never re-encodes JSON it sends or shows: console and
document bodies go out as typed (`1.0` stays a float to dynamic mapping, longs above 2^53 stay
exact, key order is kept), `_source` is cut from the response text, and responses are re-indented
token by token (`lib/search-docs.ts`: `formatJson`, `hitSources`, `sourceCells`). Index names and mappings are fetched only when a completion needs
them, so opening the console never wakes a sleeping cluster. The dashboard reaches `/v1/search`
through rolld's API like every product (`VITE_ROLL_API`, the operator token from sign-in), and
search clusters appear in workspace projects (the `search` resource kind).

## Metering and pricing

Every 60 s the control plane accounts each loaded cluster's loaded time exactly (load and unload
come from the node's lifecycle hooks; an unload accounts the remainder, rounded up to a second),
its requests (every request the node admitted for it: clients', the console's and the per-index
routes') and its resident size, measured from the engine: the heap its shards hold (segments still in
memory, readers' heap and caches, indexing buffers, the version map) plus 16 MiB for the cluster
itself. Size is measured at every tick and as the cluster unloads, and each interval is billed at
the larger of the measurements at its two ends. While a cluster is loaded with every index open,
its document count is the engine's (unflushed writes included); otherwise it comes from the
commit points. Both count Lucene documents (nested ones included), as `_stats` does, so the figure
doesn't jump when the cluster unloads. Stored bytes are re-read from the cluster's commit points after every load and
unload and every 10 minutes while loaded; storage accrues from a change log, so a sleeping
cluster adds no rows. Prices are AWS list prices in `crates/search-control/src/pricing.rs`, each
with its source.

## Tests

```sh
pnpm test:search              # scripts/test-search.sh: skips itself when nothing changed
scripts/test-search.sh --unit # no Docker
```

`scripts/test-search.sh` builds and lints every `search-*` crate, `rolld` and `roll-cli`, runs
their tests (the control plane's integration tests in `crates/search-control/tests` drive the
API, the node and an Elasticsearch client over HTTP, and `cluster.rs` two nodes of a fleet;
`crates/roll-cli/tests/search.rs` runs the real `roll` binary against an in-process control
plane), the dashboard's console completion and document helpers
(`apps/dashboard/src/lib/search-*.test.ts`, vitest), then the YAML REST suites against
`search-dev` for both dialects. A suite passes when its pass count is at least the minimum in
[compatibility.md](compatibility.md), written as

```
<!-- test-search: roll >= 1234 -->
<!-- test-search: roll-os >= 567 -->
```

Raise the minimum whenever the count goes up, so it can't silently go back down. The
differential suite against a real Elasticsearch (`crates/search-server/tests/differential.rs`)
and the client matrix (`tests/search/clients/run.sh`) run when present. CI:
`.github/workflows/search.yml`, triggered from `ci.yml` when search code changes.

## Status

Measured on the tree merged with `main` (2026-11-12), on ai-server (a shared 18-core host):

| check | result |
| --- | --- |
| build, `cargo clippy --all-targets -D warnings` (search crates, rolld, roll-cli, roll-workspace, roll-edge) | clean |
| `cargo test`, every search crate + rolld + roll-cli | 1,949 passed, 8 failed, 46 ignored (134 binaries) on the merged tree; every failure fixed since (below) |
| Elasticsearch 9.5.5 YAML REST suite (`test/free` + clients-tests) | **3,810 of 3,905 pass**, 0 fail, 95 skipped; 0 regressions against the baseline |
| OpenSearch 3.9.0 YAML REST suite | **1,455 of 1,506 pass**, 0 fail, 51 skipped |
| OpenSearch SQL plugin's PPL suite | **204 of 204** |
| extended suites (67 module, plugin and x-pack suites, each on its test cluster's settings) | **5,016 of the 5,541 tests that pass on Elasticsearch** (6,349 in all); 33 suites at 100% (EQL 39 of 39, ent-search 371 of 371, ES\|QL 549 of 549, KQL 34 of 34, `otel-data` 68 of 68, transforms 108 of 108, watcher 89 of 89, `watcher-security` 9 of 9), runtime fields 247 of 248, `logsdb` 225 of 234; round 6 passed 3,463. Largest gaps: x-pack `platinum` (1,064 of 1,412: rollup 52 of 52, security 100 of 101), downsampling (58 of 59) |
| differential suite against Elasticsearch 9.5.5 (seed 1, 60 random workloads) | 2,325 of 2,333 steps equal (99.7%); the 8 others are 2 known differences (soft-delete tombstones in phrase-suggester and term statistics), none new or open; 69 replayed fixtures equal |
| client matrix (unmodified official clients in Docker) | **11 of 11**: Python, JavaScript, Java, Go, Ruby and PHP clients, the OpenSearch Python and JavaScript clients, django-elasticsearch-dsl, Laravel Scout, and searchkick's own suite (540 runs, 0 failures) |
| x-pack REST fixtures (`search-es/tests/xpack_fixtures.rs`) | 1,097 of 1,097 steps equal Elasticsearch |
| inference API (`_inference`, against a private Elasticsearch 9.5.5 and a mock of the services) | OpenAI flows, 42 error and update cases and region policies answer the same; `platinum/inference` 22 of 23 (was 1); `inference` suite (the test plugin's services, no Elasticsearch baseline) 161 of 305; `semantic_text` through a mock as a writer with only index privileges |

The failures on the merged tree, all fixed: rolld tests that collided on search's default port;
two tests that depended on timing; three behaviours the uncommitted round-6 work had broken (SQL
`CAST … AS VARCHAR`, ES|QL keys under a field `flattened` in another index, connector key order
after an update, checked against the reference Elasticsearch); a by-query task test that read a
filtered listing; and the auth matrix's probe of storage's nested route (also broken on `main`).

## Next

Ranked; the top item is in progress. Updated as work lands (the watchdog reads this list).

1. **Indexing CPU and benchmarks on a quiet host** (docs/benchmarks/search.md). Indexing leads
   both engines with 8 clients (nyc_taxis 90k docs/s against Elasticsearch's 66k and
   OpenSearch's 60k; an ECS log 74.5k against 59.0k and 41.3k), but with one client roll spends
   more CPU per document than Elasticsearch (51.8 against 34.6 µs on the ECS log), about a quarter
   of it BP reordering at merge. Landed: top-k numeric sorts collect by bands when documents arrive
   against the sort (`@timestamp` desc on logs indexed oldest first, without BP: 84.9 → 1.4 ms;
   with BP, nyc_taxis term + sort 14.7 → 1.4, http_logs range + sort 6.2 → 1.9; a BP index's `so`
   range + sort +13%). BP over text fields only now costs no log sort (ECS indexing 61.0 → 41.5
   µs/doc) but loses keyword-filtered and other sorts up to 60% (not adopted); next is applying it
   only to time-series and log indices (`index.mode: logsdb`, data streams), measured. Then the
   request path's allocations (malloc/free are ~10% of the bulk threads), a full rerun of every
   Rally track with variance (after the vector lane's benchmarks: `autohisto_agg`, counted from the
   points tree, 27.8 → 3.0 ms in an A/B), one allocator policy (rolld's against search-dev's purge
   governor), `terms_enum`'s per-request overhead.
2. **Language analysis: the plugins Elastic Cloud and Amazon OpenSearch Service ship** (in
   progress). roll ships none of `analysis-icu` (ICU tokenizer, normalizer, folding, collation,
   transforms: the word segmentation Thai, Lao, Khmer, Burmese and CJK text needs),
   `analysis-kuromoji` (Japanese), `analysis-nori` (Korean), `analysis-smartcn` (Chinese),
   `analysis-phonetic`, `analysis-stempel` (Polish) or `analysis-ukrainian`: 39 tests of the
   extended suites, and without them roll can't search those languages well. Built in Rust on
   permissive data (ICU4X, the IPADIC and mecab-ko-dic dictionaries Lucene uses), tokens matching
   Lucene's exactly; dictionaries load only when an index uses them, so an idle cluster and the
   binary's resident set don't grow.
3. **Vector search's footprint** (docs/benchmarks/search.md, "Vector search": 1M × 384 against
   Elasticsearch 9.5.5 and OpenSearch 3.9). Roll leads on indexing (4,735 docs/s against 3,585
   and 2,978 at 8 clients), merged `knn` (1.9 ms CPU against 6.1 and 4.9), `rrf` (3.2 against 13.8
   and 8.3), cold start (36 ms to the first `knn` against a ~25-29 s node start) and memory
   (34 MiB idle against 2.5 and 4.1 GiB); filtered `knn` now reads its filter as a bitset (about
   half the CPU, identical hits). Store bytes are now Elasticsearch's (1.74 GB force-merged
   against 1.71, was 2.67): `_source` is stored without indexed vectors under
   `index.mapping.exclude_source_vectors` (Elasticsearch 9's default) and every reader patches
   them back as Elasticsearch prints them; fetching `_source` got cheaper too (vectors hidden 7.9
   → 2.0 ms CPU per `knn`, shown 13.5 → 8.7). Left in `_source`: vectors in `nested` objects and
   `sparse_vector` values. Next: 1-bit quantization quality (rescoring only the best candidates
   halves CPU but drops recall to 0.43), AVX-512 distances for float HNSW indexing (1,250 against
   1,174 µs/doc), searching unmerged segments in parallel (filtered latency 2-3x Elasticsearch's
   there).
4. **The dashboard against Elastic Cloud and OpenSearch Dashboards** (in progress). Landed: the
   index browser and console on the per-index routes, an API keys panel in Settings, the mapping
   view as a field table, OpenSearch-flavor clusters' own clients and `OPENSEARCH_*` variables,
   and importing a file into an index from the Indices tab (Kibana's "Upload a file"): NDJSON, a
   JSON array or CSV with typed columns, previewed before anything is sent, read in 1 MiB chunks
   and sent as `_bulk` batches of at most 4 MiB through the index's own route, with progress,
   cancel and the first failures (100,000 ECS log lines, 42 MB, in 24 s on a debug build), and a
   new index from a field editor (types and their options, objects from dotted names,
   `dense_vector` or OpenSearch's `knn_vector`, `semantic_text` only on an endpoint the cluster can
   run: the default ELSER needs machine learning nodes; the request shown or opened in the
   console), and a time-based index (`@timestamp`, `timestamp`, or its only top-level date field)
   browsed as Discover does: range presets, documents over time (`auto_date_histogram`, counted
   from the points tree), a click on a bar narrows to it, newest first unless a column sorts; and
   a Data streams panel (backing indices, documents, size, the lifecycle's retention set in place
   or the ILM policy that manages the stream), and a profiled console search (`"profile": true`)
   drawn as Kibana's Search Profiler draws it (per shard: the query tree, collectors,
   aggregations and fetch with times and shares), and an Index templates panel (what each matches,
   its priority and what it sets; the built-in ones on request). On OpenSearch-flavor clusters a
   data stream's retention is set in the same place, as an ISM policy roll keeps for the stream
   (`roll-retention-<stream>`: roll over, then delete each backing index once it has been rolled
   over for the retention, so the write index is never deleted; new backing indices join through
   its `ism_template`). Next: saved console requests. Signed-in users who aren't admins follow
   accounts' phase 2: today every product but code, agents and projects answers them 403
   (docs/accounts.md), search included.
5. **Compatibility gaps** from the extended suites (5,016 of the 5,541 Elasticsearch passes; a full run on 2026-11-14),
   largest first. Landed: ent-search's last 5 (a `rule` retriever over `rrf`/`linear`, inference
   inside a rule query's organic, workflow-restricted API keys), ES|QL's last 5 (`TOP_SNIPPETS`,
   `CATEGORIZE`, approximation's columns, `LIMIT n BY`, `unsigned_long` past `i64::MAX`),
   file-based service tokens, ES|QL's `METRICS_INFO` and `TS_INFO` (security's last 4, under
   field level security), and OpenSearch Security's `whoami`, `permissionsinfo`, `tenants`,
   `securityconfig`, `audit`, `ssl` and cache flush (`os_security_config.json`). Next:
   platinum's machine learning (278, a product decision: anomaly detection and
   trained models). `logsdb`'s last 9 are
   decisions (`_ignored_source` binary entries, `@timestamp` doc-values skipper); runtime fields'
   last 5 (`geo_bounds` on geo_point runtime fields, ip terms tie order, highlighting runtime
   fields); downsample's last one (allocation filtering). In the opensearch dialect, pick
   `auto_date_histogram`'s rounding from the bounds as OpenSearch's rewrite does where it differs
   from collecting (roll counts with Elasticsearch's choice in both dialects).
6. **Elasticsearch's lifecycles: what's left.** The data stream lifecycle (rollover, retention,
   failure stores, the error store) and ILM run now (`api/data_stream_lifecycle.rs`,
   `xpack/ilm_run.rs`; compatibility.md, "Known gaps"). They run time-based work at its due time
   and wake a sleeping cluster only for that. Left: ILM's `shrink` to fewer shards and
   `downsample` (roll has both APIs; the steps that swap the new index in aren't written),
   `searchable_snapshot`, `wait_for_snapshot` (needs SLM), and the `ilm-history-*` data stream.
   The data stream lifecycle's force merges and downsampling rounds aren't run either.
7. **Semantic search in roll's surfaces**: the dashboard's Inference tab lists a cluster's
   endpoints, creates them (OpenAI or any compatible URL, Cohere, Anthropic; the API key sent
   once, sealed, never shown back), tests them with a sample, deletes them, and opens a
   `semantic_text` quickstart in the console. The index browser's mapping view is a field table
   (type, inference endpoint, vector dims, similarity and index type, analyzers, what isn't
   indexed; the JSON a toggle away). Left: a roll secret as an endpoint's key (after #20), MCP
   tools (they follow the routes; the MCP thread).
8. **Semantic search** (the largest gap against Elasticsearch 9 and Elastic Cloud after machine
   learning). Landed (compatibility.md, "Inference" and "`semantic_text`"): the `_inference` API
   (`openai` with `url`, `cohere`, `anthropic`; keys sealed, `roll_net::PublicOnly`; `rate_limit`
   enforced; the preconfigured endpoints as a cluster without ML nodes; the test plugin's services
   on YAML test clusters) and `semantic_text` fields (embedded at index time in batches, chunked,
   stored as `_inference_fields`, `copy_to` sources; `semantic`, `match`, `knn`, `sparse_vector`
   and `exists` on them in `_search`, `_msearch` and templates; the semantic highlighter; `rrf`
   and `linear`'s simplified form; `text_similarity_reranker`). Left: `semantic_text` as a
   multi-field, `index_options` validation, the `embedding` task type and `semantic` field type,
   `random_reranker`, rerankers inside compound retrievers (landed 2026-11-14; the rest is low
   priority).
9. **Integration with the stacked branches**: previews (search brancher: branch, find, reset,
   delete, `own_bytes`, `ELASTICSEARCH_URL`) and apps are on this branch; `scripts/preview-cost.sh`
   now branches a search cluster too (the branch reads the parent's document, its writes stay
   in it, the service gets its `ELASTICSEARCH_URL`, landing deletes it). Apps' links to search:
   search's side is `Control::mint_link` / `revoke_link` (an API key with a data-only role,
   `ELASTICSEARCH_URL` + `ELASTICSEARCH_API_KEY`; on an OpenSearch-flavor cluster an internal
   user of OpenSearch Security with a data-only role of its own, `OPENSEARCH_URL` with
   `<user>@<cluster>:<password>@`; never the cluster's password or security);
   rolld's `RollResources` glue goes on apps' `Resources::link` / `unlink` with whichever of #29
   and #26 reaches main second. Secrets' link provider (#20) asks for `access: "data"` keys; on
   OpenSearch-flavor clusters the keys route mints an internal user (`{username, password}`
   instead of `api_key`; secrets' side is #51, checked live once both are in one rolld). A
   flavor switch doesn't carry keys across (an API key isn't an OpenSearch user): links made
   before one are reissued. The fleet test with every product on passed on this branch
   (2026-11-13: search 8,077 writes acknowledged and verified, no errors, longest stall 1.2 s in
   the rolling restart).
10. **Fleet takeover after a SIGKILL**: the fleet test's longest search stall (11.1 s at a
   SIGKILL) is the shared lease TTL (`ClusterConfig::lease_ttl`, 10 s: redis 10.4, nosql 10.6 in
   the same run) plus ~1 s to load the cluster from S3 and replay its WAL. Search's part is the
   load; the TTL is roll cloud's fleet-wide choice.
11. **CI**: the merge gate runs `search-codec`'s lib tests (`rust (search)`, 54 s on PR #29 with
   main's cache: under the minute, no room for another crate). (`search-store`'s mass-recovery
   WAL test flaked on a loaded host: a replay counted itself in the shared replay cache only
   after listing, so one could finish and empty it while the others listed; it now counts first.
   12 of 40 runs on one CPU failed before, 0 of 40 after.)
12. **Security**: a second review of what landed since the first (the `_inference` API, `semantic_text`,
   the new retrievers, workflow-restricted keys, service tokens, OpenSearch Security's new
   endpoints, ES|QL's new functions) landed in 7288684b9: `semantic_text` under field level
   security (queries and the semantic highlighter, `copy_to` sources included), an inference key
   given already sealed is sealed again (it was opened and sent to the client's `url`; watches
   already refuse one, connector and Fleet secrets always seal), and chunking, the semantic
   highlighter and `TOP_SNIPPETS` are linear in the text. Sealing binds the kind, not the
   cluster: branches copy a cluster's state under a new id. Left, low: file service tokens are
   node-wide (off unless the operator sets `path.conf`), the inference `timeout` has no upper
   bound. (A simplified `rrf`/`linear` now expands only against indices the caller may read.)
