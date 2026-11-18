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
   every cluster on the node shares one append stream, committed to local disk (fsync) as soon as
   a request waits for one of its records (back to back with the previous commit, so concurrent
   writers share an fsync and a bulk request is one commit), records nobody waits for within
   `wal_flush` (5 ms) or 4 MiB, and, in the background, shipped to object storage. A request returns once
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
- Field sorts prune with the first key's points tree or terms (Lucene's competitive iterators).
  Docs that arrive against the sort (logs indexed oldest first, sorted `@timestamp` desc) would
  each beat the queue's worst entry, so a numeric first key is collected in bands of its values:
  the points tree names an edge with about twice the queue's expected matches past it, only docs
  inside it compare against the queue, the pruner skips the rest once the count passes its
  threshold, and the segment runs again for the next band if this one came up short. Later
  passes count nothing, so hits, ties and totals are a single pass's (`search-index`
  `collector/top_field.rs`, `Band`).

## Language analysis data

The analysis plugins' data ships inside the binary and is read in place: a table nobody uses is
never paged in, and none of it is decoded onto the heap at startup.

| data | used by | size | licence |
| ---- | ------- | ---: | ------- |
| Apache Commons Codec 1.15's Beider-Morse rules and Daitch-Mokotoff rules (`phonetic/data/`) | `phonetic` (parsed on first use per name type) | 234 KB | Apache-2.0 |
| Lucene's Polish stemmer table `stemmer_20000.tbl`, converted by `tools/gen_stempel.py` | `polish_stem`, `polish` | 763 KB | Apache-2.0 |
| Lucene's Polish stop words (from Carrot2) | `polish_stop`, `polish` | 2 KB | BSD |
| `morfologik-ukrainian-search` 3.7.5's `ukrainian.dict` (from dict_uk; GPL/LGPL, CC BY-NC-SA 4.0 or Apache-2.0, used under Apache-2.0) | `ukrainian` | 6.6 MB | Apache-2.0 |
| Lucene's Ukrainian stop words | `ukrainian` | 16 KB | Apache-2.0 |
| Lucene 10.5.1's Kuromoji dictionaries (built from IPADIC mecab-ipadic-2.7.0-20070801), packed by `examples/gen_cjk_dicts.rs` (`kuromoji/ipadic.dict`) | `kuromoji_tokenizer` and the `kuromoji` filters and analyzers | 3.9 MB | IPADIC licence (NAIST), Apache-2.0 |
| Lucene 10.5.1's Nori dictionaries (built from mecab-ko-dic 2.1.1-20180720), packed the same way (`nori/mecab-ko-dic.dict`) | `nori_tokenizer` and the `nori` filters and analyzer | 6.1 MB | Apache-2.0 |
| Lucene 10.5.1's smartcn dictionaries (`coredict.mem`, `bigramdict.mem`, derived from ICTCLAS) (`smartcn/hhmm.dict`) | `smartcn_tokenizer`, `smartcn` | 2.8 MB | Apache-2.0 |
| Lucene's Japanese stop words and stop tags, romaji map and Chinese stop words | `ja_stop`, `kuromoji_part_of_speech`, `kuromoji_completion`, `smartcn_stop` | 23 KB | Apache-2.0 |
| ICU4J 77.1's normalization data (`nfc`, `nfkc`, `nfkc_cf`, `nfkc_scf`, `uts46` `.nrm`) and Lucene's `utr30.nrm` (`icu/data/`) | `icu_normalizer`, `icu_folding`, `icu_analyzer`, the CJK engine's NFKC | 321 KB | Unicode-3.0; Apache-2.0 |
| ICU4J 77.1's root word break rules (`word.brk`) and Lucene's `Default.brk` | `icu_tokenizer` | 46 KB | Unicode-3.0; Apache-2.0 |
| ICU4J 77.1's dictionaries `thaidict`, `laodict`, `khmerdict`, `burmesedict`, `cjdict` | `icu_tokenizer` (Thai, Lao, Khmer, Burmese, Chinese and Japanese runs; Korean runs use `cjdict` too) | 3.0 MB | Unicode-3.0 |
| ICU4X 2.3's compiled collation data (`icu_collator_data`) | `icu_collation`, `icu_collation_keyword` | compiled in (1.9 MB with the ported code) | Unicode-3.0 |

Measured on release `search-dev` builds (2026-11-14): `analysis-phonetic`, `-stempel` and
`-ukrainian` add 7.9 MB to the binary and `analysis-icu` 5.3 MB; an idle node's resident set is
the same with and without them (25.9 to 26.7 MiB across builds); the first use of every
phonetic encoder, the `polish` and the `ukrainian` analyzers adds 4.1 MiB (mostly the
Beider-Morse rules, parsed once per name type), and of every ICU component 2.6 MiB.

The code that reads them ports Apache Commons Codec (Apache-2.0), Lucene's Egothor stemmer
(Apache-2.0), Morfologik 2.1.1 (BSD) and ICU4J 77.1's `Normalizer2Impl`, `RuleBasedBreakIterator`,
dictionary break engines, `BytesTrie`, `CharsTrie` and `CodePointTrie` (Unicode-3.0). New crates:
`icu_collator`, `icu_locale_core`, `icu_collections` and `icu_properties` (with its `unstable`
`UnicodeSet` parser) from ICU4X 2.3, all Unicode-3.0.

The three CJK dictionaries are zstd-compressed (12.8 MB of the binary) and decompressed once per
process on first use, into memory every index and cluster on the node shares: a node that never
analyzes Japanese, Korean or Chinese text keeps its resident set (24 MiB idle for `search-dev`,
unchanged), and the first use adds about 24 MiB (Japanese), 38 MiB (Korean) and 5 MiB (Chinese).
`tools/cjk_dicts.sh` regenerates them from Lucene's jars: its `DumpDicts.java` reads out what only
Lucene can (the FSTs, smartcn's serialized tables), the FSTs are rebuilt with the `fst` crate
(MIT/Unlicense) and the connection costs are stored with duplicate rows and columns folded
(mecab-ko-dic's 3,822 × 2,693 matrix has 1,902 × 1,908 distinct values: 7.3 MB in memory instead of
20.6); every other Lucene dictionary file is kept byte for byte. The analyzers port Lucene's
`org.apache.lucene.analysis.morph`, `.ja`, `.ko` and `.cn.smart` packages (Apache-2.0).

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
- **Background jobs** (a started transform's triggers, OpenSearch ISM jobs) run on the cluster's
  job scheduler while it is loaded, and keep their schedule in the cluster state. When a cluster
  unloads, its earliest due time goes in the catalog (`search_clusters.wake_at`); every
  `job_wake_interval` (15 s) the node a due cluster's lease is placed on loads it, the scheduler
  runs what came due, and the idle unload puts it back to sleep, billed as loaded time. A
  cluster without jobs is never woken: the only standing cost is one catalog query per node.
- **Local disk** is a cache with a byte budget (`--search-cache-bytes`). Evicting a segment is
  always safe: every segment in a manifest is already in object storage.

## Branching

`branch(parent, child, at?)`: flush the parent's shards, then write the child's cluster metadata
and shard manifests pointing at the parent's segments and live-docs generations. Nothing is
copied: live-docs objects are immutable and uniquely keyed, so a side that deletes afterwards
writes a new generation and the other keeps reading its own. Point-in-time
branches (`at`) replay the parent's WAL up to that time into new segments for the child. Garbage
collection deletes a segment object only when no manifest of any cluster references it.

## Deleting and restoring

A cluster's storage is keyed by its name (`search/c/<name>/`), and a deleted cluster's name is
free at once, so a delete moves the storage aside (`search-control` `Control::delete`):

1. **Delete.** The cluster is hidden, locked out and unloaded (flushed) as before. Its storage
   is branched to a name no cluster can have, `deleted_<id>_<random>` (new commit points naming
   the same segments, plus its pins and state; nothing is copied). Then, in one catalog
   transaction, its row becomes deleted: `name` takes the kept name (which frees the real one),
   with `deleted_name`, `deleted_at` and `purge_at` (`--search-deleted-retention-days`, 7). Then
   the commit points under the old name are removed. The row flip is the commit point: before it
   the cluster still exists and the copy is removed; after it, leftovers under the old name only
   keep the name from being reused (`check_free`). With a window of 0 the delete is today's:
   storage and row go at once.
2. **Meanwhile** the row and its storage log stay, so the kept bytes are billed as storage;
   lookups, lists, wakes and the always-on loop see only live rows. Branches keep working: they
   name segments, not the parent.
3. **Restore** (`POST …/{id}/restore`) claims the row in one transaction (`claim`, `claimed_at`)
   and reserves the name it restores to in `name`, so no create takes it meanwhile; it fails
   with 409 when another cluster has it. It branches the kept copy back to the name, flips the
   row live (only if its claim still holds), then removes the kept copy. Under another name the
   password and API keys stay: the name is only the user, the key id's suffix and the host label.
   A restored cluster's point-in-time history starts at the restore (`restored_at`).
   **Keys revoked meanwhile stay revoked.** The restore brings the cluster's security back as
   it was, so a key revoked while the cluster was deleted (roll secrets or roll apps unlinking:
   `DELETE …/{id}/keys/{key}`, `Control::revoke_link`) would work again. So that route, on a
   deleted cluster its caller may see, keeps the revoke with the row (`search_revokes`, checked
   against the row in the same transaction, at most 1,000 per cluster) and answers 204; the
   restore applies them right after the row goes live, before it answers, and forgets each once
   done. One that fails stays, and the purge sweep retries it every 10 minutes. This was chosen
   over invalidating every link key at the restore: that would also kill the keys still linked,
   whose managed values roll secrets keeps until their next rotation, and a user's own data
   keys. It fails safe the same way: the route answers 204 only once the revoke is committed,
   otherwise an error that roll secrets' sweeper retries (as for a live cluster).
4. **Purge.** After `purge_at` a sweep (every 10 minutes; on a fleet one node at a time,
   `singleton:search-purge`, as garbage collection is placed) claims each due row the same way,
   removes the kept copy with `Store::delete_cluster` (commit points, state and pins; segments
   only through garbage collection, which keeps whatever a branch's manifests name), then the
   row and its usage, only if its claim still holds.

Restore and purge never run on one cluster at once: each needs the claim, taken by a
conditional update in a serializable catalog transaction shared by every node, and each
finishes only while its claim holds. A claim lapses after 15 minutes (its node died); a
restore that crashed after reserving a name leaves storage only under that name, which the next
restore or the purge removes.

## Multi-tenancy and authentication

A request finds its cluster by, in order: Basic auth username (`<cluster>` with the cluster's
password, or `<user>@<cluster>`), `Authorization: ApiKey base64(id:secret)` where the id names the
cluster, `Authorization: Bearer <cluster>_<secret>`, or the TLS SNI/`Host` label under the node's
domain (`<cluster>.search.<domain>`). Secrets are compared in constant time. Each cluster has
request-rate and in-flight byte budgets so one tenant can't starve the node.

APIs built on other APIs (EQL, ES|QL, SQL, PPL, graph, vector tiles, `terms_enum`, transforms'
preview, rollup search, downsampling, search applications, …) make their inner requests as
the client (`xpack::internal::as_caller`, set for every x-pack and opensearch-dialect API at
dispatch): those requests are authorized, and read under document and field level security,
as the client; only work on the cluster's own system indices uses `call_system`. Background
work runs as the user who set it up (`as_principal`): a watch as its user, a rollup job as its
creator, a transform as the user who put or last updated it. A superuser's requests and jobs
run with the cluster's authority, as before.

Secrets a cluster stores for later use are sealed in its state (`search-es` `seal.rs`): a
watch's HTTP passwords and `Authorization` headers, connector secrets and Fleet secrets.
AES-256-GCM under a key derived from the deployment's `ROLL_SECRET_KEY` (rolld; search-dev
reads the same variable), a fresh nonce per value and the value's kind as associated data. They
travel sealed through cluster states, snapshots and branches, and are opened only to send a
request or to answer that secret's own read API; a node with another key refuses to open them
without naming either. With no key they are stored as given, as Elasticsearch does without
`xpack.watcher.encrypt_sensitive_data`. ES|QL data sources still refuse S3 secrets, as
Elasticsearch does without cluster state encryption. Users' passwords are hashes and API keys
are kept as hashes, never sealed.

## Snapshots

- **Where they live.** A repository's snapshots are recorded with the cluster's metadata, keyed
  by the repository's location, so registering it again finds them. In roll's own repositories
  (`fs`, `url`, and the default repository) each snapshotted shard is a pin
  (`search/c/<cluster>/p/snap.<snapshot>.<index>/<shard>.json`): a copy of its commit point
  sharing the index's segments, so a snapshot copies no data and garbage collection keeps what a
  pin names. Branches copy pins; deleting a cluster deletes them with it (segments a branch
  shares stay).
- **The default repository** (`found-snapshots`, `SearchConfig::default_repository`) is
  registered in memory whenever a cluster with object storage opens or changes, never stored
  once, so every cluster has it without a write, a branch sees its parent's snapshots there, and
  an idle cluster costs nothing more (no policy of its own: a tenant's SLM policy wakes the
  cluster only when due). It shows as an `s3` repository with client `roll`, a client no tenant
  can name, and can't be changed or unregistered. A deleted cluster's snapshots are kept with
  its data for the restore window and go with it at the purge ([Deleting and restoring](#deleting-and-restoring));
  backups meant to outlive a cluster go to an `s3` repository in the tenant's own bucket.
- **`s3` repositories** (`search-es` `api/snapshots/s3.rs`, client `s3.rs`) keep everything in
  the tenant's bucket under `<base_path>/roll/`: the repository data at each generation
  (`index-N`, written only if absent, so two clusters never commit the same generation), and per
  shard snapshot the segments it read, each segment's live documents as gzipped records (id,
  routing, source with its vectors, sequence number, primary term, version) keyed by the
  segment id and live count, so later snapshots reuse what didn't change. The cluster keeps a
  copy of the repository data, read again before each operation on the repository. A restore
  parses those records with bounds and indexes them again; it never opens segment files from a
  bucket. Everything read from a bucket is a peer's input: the repository data is bounded
  inflated, in parsed values and in snapshots and indices, a document's source by
  `http.max_content_length` (charged to the breakers as its write was), listings by pages and
  objects, and a restore checks names and global state as the APIs that set them do. Taking a snapshot reads the open shards' searchers (a closed index's last commit,
  pinned only while it is read), so nothing stays in roll's storage.
- **Objects and parts** (`api/snapshots/transfer.rs`). A segment's records are one gzip stream
  cut into objects of `chunk_size` (default and most 5tb, least 5mb, never more than
  `buffer_size` × `max_multipart_parts`); an object larger than `buffer_size` (5mb to 5gb;
  default 100mb or 5% of the node's heap, whichever is less) goes up as a multipart upload of
  `buffer_size` parts, the settings validated with Elasticsearch's messages. One part at a time is
  held, charged to the `request` breaker, and a restore inflates and indexes records as the
  bytes arrive, so neither side holds a whole file. An upload that fails is aborted; one a crash
  left is aborted by `_cleanup` and after snapshot deletions (as Elasticsearch does then), listed
  by `ListMultipartUploads` under the repository's prefix, at most
  `max_multipart_upload_cleanup_size` (1,000) at a time, never one running on this node. MinIO
  lists uploads by exact key only, so it gets none listed and relies on its own 24 h stale-upload
  expiry. Snapshots written before `chunk_size` (a gzip member per ~8 MiB chunk) read the same.
- **Throttling.** `max_snapshot_bytes_per_sec` (default 40mb) paces uploads and
  `max_restore_bytes_per_sec` (default unlimited) a restore's reads, each through one limiter
  per repository on the node that every transfer shares (as Lucene's `SimpleRateLimiter`, which
  Elasticsearch uses: bytes pay as they are produced, each payment due one byte-time after the
  previous one, so producing them overlaps the wait and a burst is at most one payment). A
  restore also pays the cluster's `indices.recovery.max_bytes_per_sec` (default 40mb), as
  Elasticsearch's restores do: roll has no peer recovery, so this is the setting's only effect.
  Repository analysis pays the same limits. `_nodes/stats/repositories` reports
  Elasticsearch's counters: throttled read and write time, shard snapshots started, completed and
  in progress, blobs and bytes uploaded, upload and read time. Elasticsearch has no throttle time
  in `_snapshot/_status` and none from repositories in `_recovery`, and neither has roll.
- **Analysis** (`POST _snapshot/<repo>/_analyze`, `api/snapshots/analyze.rs`). On an `s3`
  repository it exercises the bucket under `roll/temp-analysis-<id>/`: a contended register
  (`register_operation_count` compare-and-exchange increments by `If-Match`, all of which must
  land) and an uncontended one, overwrite protection (of concurrent `If-None-Match: *` writers
  exactly one wins), then Elasticsearch's blob sizes (`getBlobSizes`, ported), each blob written
  (in parts past `buffer_size`), sometimes read early, overwritten or written and aborted
  (`rare_action_probability`), and read back whole or as a range against a regenerated
  checksum; then listed with their sizes and deleted. A failed check answers
  Elasticsearch's `repository_verification_exception` chain after removing what it wrote. It
  writes no more than `max_total_data_size`, runs at most 16 blobs at once, holds a part per
  blob in flight (breaker-charged), pays the repository's throttles and stops at `timeout`. On
  roll's own repositories it plans and answers without storage, as before.
- **Credentials.** Elasticsearch takes an `s3` client's keys from its keystore; a roll tenant
  has none, so `access_key`, `secret_key` and `session_token` come with the repository's
  settings and are moved out of them at once into the repository's sealed secrets (`seal.rs`,
  the deployment's `ROLL_SECRET_KEY`), opened only to sign a request (SigV4), never returned by
  `GET _snapshot` or the cluster state and never logged. Without credentials requests are
  anonymous: roll's own credentials are never used for a tenant's bucket. The endpoint is a URL
  the tenant chose: public addresses only (`roll_net::PublicOnly`, `literal_private_host`), no
  redirects, no proxy, bounded reads (`--search-repository-allow-private` for local MinIO).

## On the fleet (roll cloud)

Search runs on the same rolld fleet as every other product (docs/cloud/README.md): no image,
stack, proxy or metadata database of its own. On one node (the memory backend) nothing below
applies: every cluster is this node's.

- **Ownership**: a cluster is served by the holder of the lease `search:cluster:<id>`, taken from
  rolld's roll-cluster handle. A cluster is loaded only under a guard of that lease and unloaded
  when the guard's tenure ends (a handoff, a drain, the lease lost). An owner gives an idle
  cluster's lease up (`Cluster::release_if_idle`) once it has unloaded, so a sleeping cluster
  belongs to nobody and costs no node anything.
- **Requests**: the Elasticsearch REST listener and the edge's `<cluster>.search.<region>.<domain>`
  read only which cluster a request names (`Host`, the Basic or ApiKey user, the Bearer token's
  prefix), never its credentials, and run it on the owner: here, or forwarded with
  `Cluster::dispatch` to the owner's `/internal/search/rest/…`, signed, bodies streamed. The owner
  authenticates. `/v1/search/clusters/{id}/…` routes that touch a cluster's data run on its owner
  the same way.
- **Fencing is in object storage, not in the lease.** Leases are about where a cluster runs. When a
  node loads a cluster it writes an ownership marker `search/c/<cluster>/owner/<epoch>` with
  `epoch = 1 + the highest marker present` (only the lease holder does this, so two nodes never
  pick the same epoch); every manifest, cluster state and WAL batch carries the epoch, and a node
  whose epoch is below the newest marker gets `Fenced` and stops acknowledging. roll-cluster's own
  epochs aren't used: they restart at 1 after `release_if_idle` deletes the lease row.
- **Acknowledgements** wait for the WAL batch in object storage (`--search-sync-objects`, on by
  default on a fleet) and for a valid guard (`Guard::is_valid`) at the moment of the ack.
- **Catalog**: the shared system database (`ROLL_SYSTEM_DATABASE`, schema `search`), through
  `code-sql` like the other products' catalogs; `search.db` (SQLite) on one node.
- **Background work** on owners: metering of loaded clusters; garbage collection of unreferenced
  objects on one node (`singleton:search-gc`), and the purge of deleted clusters whose window
  ended on one node (`singleton:search-purge`); always-on clusters taken by the node they're
  placed on. A deleted cluster has no owner: its restore runs on whichever node gets the call.
- **Drains** (SIGTERM, deploys, Spot): roll-cluster hands every lease off; each cluster flushes and
  unloads as its guard ends, and the next request loads it on its new owner from its manifests and
  the WAL after them.
- **Metering** per cluster: loaded GB-hours, requests, stored GB-months; the dashboard compares the
  cost with Elastic Cloud and Amazon OpenSearch Service for the same workload.

## Testing

- **Unit tests** in every crate, next to the code.
- **YAML REST suites** (`search-yaml`): Elastic's `elasticsearch-clients-tests` (Apache-2.0,
  vendored), the Elasticsearch 9.x core suite from the official `rest-resources-zip` (downloaded
  at test time into `target/`, never committed — it isn't Apache-licensed) and the OpenSearch
  suite (Apache-2.0). The runner itself is validated against a real Elasticsearch in Docker.
  Every skipped test is listed with a reason in `crates/search-server/tests/yaml/skips.txt`;
  pass counts are tracked in [compatibility.md](compatibility.md).
- **Differential suite** (`crates/search-server/tests/differential.rs`): seeded random
  workloads against roll and `elasticsearch:9` in Docker, comparing hits, scores (float-exact),
  aggregations, errors and headers; differences are minimized into replayable fixtures.
- **Client matrix** (`tests/search/clients/run.sh`): unmodified official clients (Python,
  JavaScript, Java, Go, Ruby, PHP, the OpenSearch clients, django-elasticsearch-dsl) and
  searchkick's own test suite, in Docker.
- **Benchmarks**: Rally tracks against both, `scripts/bench-search.sh`.

`scripts/test-search.sh` (`pnpm test:search`) runs all of it and skips itself when nothing
search-related changed.
