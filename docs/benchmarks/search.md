# roll search vs Elasticsearch 9.5.5 and OpenSearch 3.9

`scripts/bench-search.sh` runs every benchmark here against roll search (`search-dev`, release
build, `--data-dir` so it persists like Elasticsearch) and Elasticsearch 9.5.5 (the official
image, 2 GiB heap, security off), each in a Docker container pinned to the **same CPU set and
memory limit**. Raw outputs are in `target/bench-search/` on the machine that ran them.

## Hardware and software

| | |
|---|---|
| CPU | Intel Xeon W-2295, 18 cores / 36 threads, 24.75 MiB L3 (one socket) |
| memory | 125 GiB |
| disk | md RAID (shared, busy) |
| OS | Debian 13, Linux 6.12, Docker 29.9.2 |
| targets | each `--cpuset-cpus 10-13,28-31` (4 cores, 8 hardware threads; `14-17,32-35` since round 2), `-m 4g` |
| Rally | elastic/rally 2.13.0 (`--pipeline=benchmark-only`), `--cpuset-cpus 6-9,24-27`, `-m 4g` |
| roll | rustc 1.99.1, `cargo build --release`, mimalloc |
| Elasticsearch | `docker.elastic.co/elasticsearch/elasticsearch:9.5.5`, `-Xms2g -Xmx2g` |

The machine is shared with many other build and test jobs: during these runs its 1-minute
load average was 40–49 on 36 threads, and every CPU, the pinned ones included, was 75–100%
busy. Absolute numbers are therefore slower and noisier than on an idle host. What keeps the
comparison fair is that both targets get the identical CPU set and memory and **run
alternately**: the micro suite alternates per workload, Rally per round (and who goes first
alternates between rounds), so drift over minutes hits both alike. Each table reports medians
over rounds with the min–max spread; trust a difference only where the spreads do not overlap.

## Methodology

- **Micro suite** (`scripts/bench-search.sh micro`, `scripts/bench-search.py`): one shard, 50,000
  generated documents (5-word `title`, 30-word `body` from a 5,000-word Zipf vocabulary, keyword
  `cat`, `long`, `double`, `date`), 3 rounds; each round recreates the index on both targets
  (bulk timed with bodies pre-serialized) and runs every query workload 200 times (after 30
  warm-up requests) on roll, then on Elasticsearch, before the next workload. Both answer the
  identical seeded query sequence. Plain aggregation rows repeat the same body, so both serve
  them from the shard request cache; `(no cache)` rows pass `request_cache=false`. The suite
  also reads each container's cgroup `cpu.stat`: server CPU time per request, which a busy
  host distorts much less than wall time.
- **Rally** (`scripts/bench-search.sh rally geonames 2`): the standard `geonames` track,
  default challenge (`append-no-conflicts`), `ingest_percentage:10` (1,103,000 documents,
  about 330 MB of JSON; 5 shards, 0 replicas as the track sets them), no target throughput (so
  service time is what each side can do), bulk warm-up shortened to 10 s (the track's 120 s is
  longer than the whole subset takes to index).
  `http_logs` (`scripts/bench-search.sh rally http_logs 2` with
  `RALLY_PARAMS=ingest_percentage:2 RALLY_INDICES='logs-*,reindexed-logs'`): the default
  challenge on the first 2% of the corpus (4,948,746 log lines: all of `logs-181998` and the
  first 2.24M of `logs-191998`; 5 shards each), every operation unthrottled (the script drops
  the track's hard-coded `target-throughput`s, which are sized for the full corpus). It adds
  what geonames lacks: time-series sorts with `search_after`, sorts on a keyword the documents
  don't have, `match_only_text`, ip fields, an hourly `date_histogram` and force-merged
  1-segment sorts. The other standard tracks ship as single 1–10 GB compressed files that Rally
  decompresses whole before indexing any subset; `RALLY_DOCS=<n>` (`bench-search.py
  rally-subset`) instead streams the first n documents of each large corpus once into the data
  directory and points the local track copy at them, so `nyc_taxis` (10M docs), `so` (3M),
  `noaa` (5M), `nested` (2M), `pmc` (100k), `so_vector` (200k), `dense_vector` (1M) and the whole
  `percolator` corpus run here (round 6).
- **Indexing** (`scripts/bench-search.sh ingest`, `bench-search.py ingest`): one Rally corpus
  file in 5,000-document bulks at 1, 8 and 32 clients against both targets, interleaved per
  round: throughput, per-bulk service time percentiles, the final refresh, and server CPU per
  document during the load and again after merges settle.
- **Footprint** (`scripts/bench-search.sh footprint`): RSS of the server process (largest
  `VmRSS` in the container's cgroup) idle and with a loaded, force-merged 50k-doc index; store
  bytes per document; cold start: roll unloads an idle cluster after 2 s
  (`--idle-unload-ms 2000`) and the next request wakes it from its data directory; Elasticsearch
  has no unload, so its cold start is a node restart until the first search answers.
- **Profiles**: `perf record --call-graph dwarf` from a privileged container (`perf` is not
  usable from the host shell) against a `search-dev` pinned to other CPUs, with the geonames
  subset loaded by a small script; A/B numbers in the tuning log are server CPU time per
  request (sum of the process's threads' `schedstat` run time) of two binaries loaded with the
  same data and measured interleaved.

## Round 8: every Rally track again (2026-11-14, `83fac5982`)

The branch head `83fac5982` (sort by bands, `auto_date_histogram` counted from the points tree,
filtered k-NN as a bitset, `_source` stored without vectors, the direct document parser for
objects, arrays and scaled floats), `search-dev` built in release (line tables) in a clean
detached worktree. The harness and its parameters are Round 7's: `scripts/bench-search.sh rally
<track> 3`, all three targets in Docker on the same CPUs (`10,11,28,29`: 2 cores / 4 hardware
threads, lane b, held under its lock) and 4 GiB each, Elasticsearch 9.5.5 and OpenSearch 3.9 with
2 GiB heaps, roll on a fresh data directory every race, Rally 2.13.0 on `12,13,30,31`. Three
rounds per track, the order rotating every round (roll, Elasticsearch, OpenSearch; then
Elasticsearch first; then OpenSearch first). Tables: Rally's p50 service time (ms), or the bulk
task's median throughput, median (min-max) over the three rounds, and roll against the best of
the other two (below 1: roll is faster or indexes more). Raw outputs:
`~/.cache/rsearch/rally-rerun/` on the machine that ran them.

**Summary** (this round's Rally medians; footprint from Round 7's `bench-search.sh footprint`;
p50 service time in ms unless noted):

| measure | roll | Elasticsearch 9.5.5 | OpenSearch 3.9 |
|---|---:|---:|---:|
| RSS, idle node (Round 7) | **24 MiB** | 2,519 MiB | 2,517 MiB |
| RSS at the end of a Rally race | **202-529 MiB** | 2,218-2,706 MiB | 2,242-2,811 MiB |
| store bytes per document (force-merged; Round 7) | **199** | 263 | 262 |
| server CPU per race, geonames (s) | **769** | 1,223 | 1,274 |
| indexing, http_logs (docs/s) | **130k** | 95k | 82k |
| indexing, geonames (docs/s) | **67.7k** | 45.6k | 37.0k |
| indexing, so (docs/s) | **35.7k** | 27.7k | 26.2k |
| indexing, nyc_taxis (docs/s) | **61.3k** | 58.4k | 44.4k |
| http_logs `default` | **2.99** | 4.36 | 3.86 |
| http_logs `hourly_agg` | **7.34** | 147.3 | 17.5 |
| http_logs `desc_sort_timestamp` | **6.35** | 12.93 | 12.37 |
| http_logs `terms_enum` | **1.34** | 2.86 | (no API) |
| nyc_taxis `date_histogram_agg` | **6.67** | 11.79 | 7.57 |
| nyc_taxis `autohisto_agg` | 9.04 | 29.30 | **7.58** |
| nyc_taxis `distance_amount_agg` | 103.2 | **81.07** | 109.7 |
| nyc_taxis `range` | **5.31** | 8.34 | 7.37 |
| geonames `country_agg_uncached` | **18.96** | 29.37 | 43.53 |
| geonames `large_terms` | **98.62** | 163.0 | 163.6 |
| geonames `painless_static` | **46.31** | 95.57 | 107.5 |
| geonames `field_value_script_score` | **30.13** | 44.96 | 42.19 |
| geonames `decay_geo_gauss_script_score` | **63.29** | 78.73 | 80.53 |

How the numbers were checked:

- **The work happened.** After every race the harness logged the indices: every engine indexed
  every document in every round (nyc_taxis 2,000,000, so 1,000,000, http_logs 4,948,746,
  geonames 1,160,000), and Rally recorded no errors except OpenSearch's `terms_enum` (below).
- **Load.** The host ran other sessions' builds and benchmarks (1-minute load 17-53 on 36
  threads). A sampler of the lane's CPUs found the server CPUs 26-83% and the client CPUs
  38-79% busy before the first race had started (other sessions' unpinned processes, and
  OpenSearch's JVM starting), so a difference whose ranges overlap is a tie.
- **What limits roll.** Server CPU per race (the container's cgroup, indexing and every query)
  is lower for roll on every track (table below), and the slow tasks were measured
  directly on the server with CPU per request (below).

**Corrections to Round 7's tables**, found by this run:

- OpenSearch has no `_terms_enum` API ("no handler found"): every one of its `terms_enum`
  requests failed, in Round 7's races as in these (Rally's error rate 100%). Its 0.57 ms was the
  time to an error, not a win; roll leads Elasticsearch on `terms_enum` 1.34 ms to 2.86.
- `autohisto_agg` and `date_histogram_agg` are not counted from the points tree in Rally. The
  A/Bs in the tuning log (`date_histogram_agg` 65.9 → 3.4 ms, `autohisto_agg` 27.8 → 3.0) loaded
  2M nyc_taxis documents with a script, which leaves segments of up to ~530k documents. Rally's
  8 clients and the merges after them leave a segment of 0.8-0.9M, and on a segment that large
  the count costs what collecting does. Server CPU per request (p50): on the index of a Rally
  race, `autohisto_agg` 10.6 ms and `date_histogram_agg` 7.0; the same documents loaded by the
  script as four segments of 0.27-0.53M, 2.5 and 1.4 ms; those merged into one segment, 6.7 and
  6.1 ms. Why a large segment isn't counted is the next thing to find.
  Roll still leads Elasticsearch on both (9.0 against 29.3 ms, 6.7 against 11.8); OpenSearch's
  rewrite holds at 7.6 ms on both.
- `distance_amount_agg` (a 1-wide `histogram` with a `stats` sub-aggregation over 2M trips): Round
  7's "roll leads" set roll's best quiet run (83.9 ms) against Elasticsearch's earlier race
  (86.3). Run alternately, Elasticsearch leads, 81.1 ms to 103.2. Not a regression: the same query
  on the same 8-client load costs 149 ms of server CPU on `91e1ef318` (the last commit before
  Round 7's roll-only runs) and 153 on `83fac5982`, the same buckets. Roll runs it on one thread
  per shard (CPU = wall); Elasticsearch 9 splits the shard's segments across its search threads:
  on the same 2M documents (16 segments) it spends 308 ms of CPU in 186 ms of wall time, twice
  roll's CPU.

**Regressions against Round 7**, each found with release builds and measured on the server:

- **http_logs indexing: `dbe79f8f7`** ("semantic_text documents are embedded at index time").
  Rally's `index-append` fell from 162k docs/s (Round 7's roll-only run) to 130k. The same 1M
  `logs-181998` documents, 8 clients, 5 shards, server CPU per document over 3-8 interleaved
  rounds per pair: `91e1ef318` (before Round 7's roll-only runs) 26-28 µs, `83fac5982` 29-31. A
  bisect over the write-path commits put it at `dbe79f8f7` against its parent: 28.9-30.4 µs against
  24.5-27.8 (medians 29.6 and 27.3, +8%, identical aggregations); `4a1c20485`, `f31e5c9af` and
  `f40dd0264` measure as `91e1ef318`. The likely cost (from the diff, not profiled): every bulk
  item now runs `embed_ahead`, which resolves its index and walks the whole mapping with
  `semantic::fields` (a `String` per field path), and `bulk_ahead::prepare_one` walks it again,
  for every document of a mapping with no `semantic_text` field. The rest of the drop is the
  host: Round 7's 162k was a roll-only run; in this alternating run roll still leads
  Elasticsearch 130k to 95k. nyc_taxis and so are not affected (so: 118-121 µs per document at
  `83fac5982` against 122-125 at `91e1ef318`; nyc_taxis 48-50 against 74-78, the direct
  parser's scaled floats).
- **geonames population sorts: `5942eea08`** (top-k numeric sorts collect by bands). Rally's
  `desc_sort_population` sat at 5.5-5.7 ms (Round 7's roll runs: 3.3-5.0) and
  `asc_sort_population` rose against Elasticsearch's. The same 1.16M documents in one data
  directory opened by `ea811df7e` (the parent), `5942eea08` and `83fac5982`, server CPU per
  request, 60 interleaved requests each, identical hits: `desc_sort_population` 3.42 → 3.83 ms
  (+12%), `asc_sort_population` 2.89 → 3.34 (+16%), `asc_sort_with_after_population` 3.26 →
  3.63 (+11%); the head measures as `5942eea08`. The same change halves `desc_sort_geonameid`
  (9.23 → 4.48 ms; Rally 4.56 against Elasticsearch's 6.39), where the documents do arrive
  against the sort: the bands cost something on every sort whose values don't follow the doc
  order. With `_source: false` the head still costs 11% more than Round 7's binary, so it is the
  collection, not the fetch.

**Where roll loses** (roll / best of the other two above 1, ranked by how much; "tie" where the
ranges overlap):

1. nyc_taxis `distance_amount_agg`: 1.27x Elasticsearch (103.2 ms against 81.1; the ranges are
   apart). One thread per shard against Elasticsearch's concurrent segments (above).
2. nyc_taxis `autohisto_agg`: 1.19x OpenSearch (9.0 against 7.6, tie). The points-tree count
   doesn't apply to Rally's 0.8M+ document segments (above).
3. http_logs `sort-size-desc-after-force-merge-1-seg`: 1.12x OpenSearch (6.37 against 5.69, tie).
4. geonames `asc_sort_population`: 1.11x Elasticsearch (4.25 against 3.83, tie), and
   `desc_sort_population`: 1.07x (5.59 against 5.25, tie). The bands' cost (above).
5. The bulk tasks' p50 service time on nyc_taxis and so: 1.04x Elasticsearch (ties; roll's
   throughput leads on both).
6. http_logs `scroll`: 1.03x OpenSearch (456.9 against 442.4, tie; Elasticsearch 455.9).

Everything else in the four tables roll leads, most by 1.3-4x. Indexing leads Elasticsearch by
1.05-1.48x in docs/s (nyc_taxis 61.3k against 58.4k is a tie: Elasticsearch's three rounds
spread 34.7k-73.8k), on 0.63-0.95x its server CPU per race.

**Server CPU per race** (the container's CPU time over the whole race: indexing, merges and every
query task; Elasticsearch's and OpenSearch's include their JIT and GC), median (min-max) of 3, in
seconds; and **RSS at the end of a race** (largest `VmRSS` in the container), MiB:

| track | roll CPU | Elasticsearch CPU | OpenSearch CPU | roll RSS | Elasticsearch RSS | OpenSearch RSS |
|---|---:|---:|---:|---:|---:|---:|
| nyc_taxis | **167** (164-173) | 192 (189-256) | 224 (222-253) | **202-445** | 2,636-2,661 | 2,650-2,690 |
| so | **138** (136-139) | 145 (134-171) | 167 (164-170) | **371-529** | 2,484-2,706 | 2,693-2,811 |
| http_logs | **413** (409-422) | 656 (571-689) | 535 (497-561) | **325-327** | 2,218-2,452 | 2,242-2,378 |
| geonames | **769** (746-802) | 1,223 (1,131-1,252) | 1,274 (1,230-1,356) | **243-254** | 2,247-2,460 | 2,576-2,613 |

`nyc_taxis` (`RALLY_DOCS=2000000`: the first 2M trips, one shard):

| task | metric | roll | Elasticsearch | OpenSearch | roll / best |
|---|---|---:|---:|---:|---:|
| index | docs/s | 61272 (61010-64533) | 58373 (34683-73808) | 44403 (30990-46660) | 0.95 (tie) |
| index | p50 ms | 1198 (1129-1205) | 1157 (1012-1428) | 1518 (1517-1560) | 1.04 (tie) |
| default_no_target | p50 ms | 1.46 (1.34-1.57) | 8.01 (7.61-9.78) | 6.73 (4.88-7.96) | 0.22 |
| default_1k | p50 ms | 11.72 (9.39-12.02) | 16.45 (16.23-17.38) | 18.29 (16.41-23.13) | 0.71 |
| range | p50 ms | 5.31 (4.81-5.46) | 8.34 (8.23-10.37) | 7.37 (5.79-7.49) | 0.72 |
| distance_amount_agg | p50 ms | 103.2 (97.59-139.9) | 81.07 (63.14-82.39) | 109.7 (76.31-140.7) | 1.27 |
| autohisto_agg | p50 ms | 9.04 (8.47-10.54) | 29.30 (22.55-30.18) | 7.58 (4.79-9.04) | 1.19 (tie) |
| date_histogram_agg | p50 ms | 6.67 (5.60-7.03) | 11.79 (9.23-14.14) | 7.57 (4.57-8.21) | 0.88 (tie) |

`so` (`RALLY_DOCS=1000000`: 1M posts, 5 shards; the track only indexes):

| task | metric | roll | Elasticsearch | OpenSearch | roll / best |
|---|---|---:|---:|---:|---:|
| index-append | docs/s | 35731 (34108-35848) | 27661 (18509-32755) | 26201 (24005-26232) | 0.77 |
| index-append | p50 ms | 1359 (1303-1537) | 1304 (1179-1388) | 1560 (1452-1623) | 1.04 (tie) |

`http_logs` (`ingest_percentage:2`: 4,948,746 log lines in two 5-shard indices; every operation
unthrottled):

| task | metric | roll | Elasticsearch | OpenSearch | roll / best |
|---|---|---:|---:|---:|---:|
| index-append | docs/s | 129978 (127869-139148) | 95333 (90392-108395) | 82403 (78527-92458) | 0.73 |
| index-append | p50 ms | 287.5 (278.2-291.3) | 338.1 (311.2-358.8) | 427.3 (394.3-442.3) | 0.85 |
| default | p50 ms | 2.99 (2.85-3.29) | 4.36 (4.24-6.61) | 3.86 (3.77-5.62) | 0.77 |
| term | p50 ms | 2.78 (1.73-2.79) | 5.17 (4.99-5.83) | 4.35 (4.02-4.52) | 0.64 |
| terms_enum | p50 ms | 1.34 (0.99-1.37) | 2.86 (2.84-2.99) | every request failed (no `_terms_enum` API) | 0.47 |
| range | p50 ms | 1.43 (1.32-1.44) | 2.98 (2.53-3.10) | 2.97 (2.74-2.97) | 0.48 |
| 200s-in-range | p50 ms | 6.45 (5.99-8.24) | 9.56 (8.69-10.95) | 8.51 (8.28-9.28) | 0.76 |
| 400s-in-range | p50 ms | 2.20 (1.99-2.58) | 6.12 (5.98-6.30) | 5.62 (5.55-5.77) | 0.39 |
| hourly_agg | p50 ms | 7.34 (6.46-7.86) | 147.3 (133.2-162.1) | 17.49 (9.87-20.00) | 0.42 |
| scroll | p50 ms | 456.9 (445.2-459.0) | 455.9 (441.9-516.4) | 442.4 (418.6-506.3) | 1.03 (tie) |
| desc_sort_timestamp | p50 ms | 6.35 (5.43-6.93) | 12.93 (11.90-21.20) | 12.37 (11.69-12.70) | 0.51 |
| asc_sort_timestamp | p50 ms | 6.38 (4.98-6.57) | 10.28 (10.04-12.24) | 7.16 (5.77-9.10) | 0.89 (tie) |
| desc_sort_with_after_timestamp | p50 ms | 3.26 (2.95-3.28) | 14.46 (13.02-23.29) | 6.46 (5.39-7.89) | 0.50 |
| asc_sort_with_after_timestamp | p50 ms | 1.75 (1.64-1.87) | 6.35 (5.93-6.68) | 4.28 (4.16-4.63) | 0.41 |
| desc_sort_timestamp_can_match_shortcut | p50 ms | 5.17 (4.45-7.67) | 16.14 (11.37-19.29) | 19.02 (17.61-20.65) | 0.32 |
| desc_sort_timestamp_no_can_match_shortcut | p50 ms | 5.95 (5.01-7.33) | 20.82 (17.44-25.39) | 23.44 (20.20-24.77) | 0.29 |
| sort_size_desc | p50 ms | 5.24 (5.21-9.32) | 8.64 (7.99-11.08) | 6.64 (6.44-10.32) | 0.79 (tie) |
| sort_size_asc | p50 ms | 5.12 (4.64-6.18) | 6.58 (6.33-7.51) | 6.15 (5.85-7.59) | 0.83 (tie) |
| sort_status_desc | p50 ms | 4.40 (4.18-5.44) | 7.52 (7.05-8.06) | 5.28 (5.15-7.12) | 0.83 (tie) |
| sort_status_asc | p50 ms | 4.26 (3.83-4.89) | 6.94 (6.67-7.26) | 6.38 (5.33-7.15) | 0.67 |
| sort_keyword_can_match_shortcut | p50 ms | 4.29 (3.87-4.89) | 6.79 (6.21-6.80) | 6.48 (6.36-6.89) | 0.66 |
| sort_keyword_no_can_match_shortcut | p50 ms | 4.05 (3.87-4.33) | 5.67 (5.34-5.96) | 6.13 (4.92-6.36) | 0.71 |
| sort_numeric_can_match_shortcut | p50 ms | 5.01 (4.93-7.81) | 9.16 (7.71-11.10) | 6.83 (6.60-7.85) | 0.73 (tie) |
| sort_numeric_no_can_match_shortcut | p50 ms | 5.42 (4.51-7.42) | 9.09 (7.35-9.60) | 7.18 (6.84-8.39) | 0.76 (tie) |
| desc-sort-timestamp-after-force-merge-1-seg | p50 ms | 5.42 (5.20-6.28) | 20.08 (12.07-22.26) | 11.70 (11.27-11.92) | 0.46 |
| asc-sort-timestamp-after-force-merge-1-seg | p50 ms | 6.94 (5.57-6.96) | 8.52 (6.73-9.70) | 7.22 (5.66-7.82) | 0.96 (tie) |
| desc-sort-with-after-timestamp-after-force-merge-1-seg | p50 ms | 3.43 (3.26-4.36) | 22.23 (17.81-22.23) | 5.94 (5.53-6.92) | 0.58 |
| asc-sort-with-after-timestamp-after-force-merge-1-seg | p50 ms | 1.84 (1.74-2.00) | 5.61 (5.09-6.57) | 4.06 (3.59-4.08) | 0.45 |
| sort-size-desc-after-force-merge-1-seg | p50 ms | 6.37 (5.46-6.37) | 8.66 (8.41-14.13) | 5.69 (5.34-6.06) | 1.12 (tie) |
| sort-size-asc-after-force-merge-1-seg | p50 ms | 4.79 (4.73-5.13) | 7.01 (5.49-7.99) | 5.91 (4.91-5.92) | 0.81 (tie) |
| sort-status-desc-after-force-merge-1-seg | p50 ms | 5.33 (4.70-5.37) | 5.96 (5.96-7.38) | 5.90 (5.58-6.49) | 0.90 |
| sort-status-asc-after-force-merge-1-seg | p50 ms | 4.72 (4.68-5.52) | 5.76 (5.11-6.26) | 5.65 (4.80-5.91) | 0.83 (tie) |

`geonames` (`ingest_percentage:10`: 1,160,000 documents, 5 shards):

| task | metric | roll | Elasticsearch | OpenSearch | roll / best |
|---|---|---:|---:|---:|---:|
| index-append | docs/s | 67688 (59614-67801) | 45602 (36908-48614) | 37008 (32160-48037) | 0.67 |
| index-append | p50 ms | 695.4 (686.7-755.6) | 759.1 (677.2-1169) | 852.4 (817.3-1009) | 0.92 (tie) |
| index-stats | p50 ms | 1.56 (1.51-1.58) | 2.28 (2.21-2.54) | 2.52 (2.18-2.64) | 0.69 |
| node-stats | p50 ms | 1.44 (1.42-1.82) | 5.60 (5.35-6.53) | 3.82 (3.75-3.85) | 0.38 |
| default | p50 ms | 1.97 (1.42-2.52) | 3.61 (3.22-3.82) | 2.73 (2.50-3.30) | 0.72 (tie) |
| term | p50 ms | 0.66 (0.66-0.67) | 2.84 (2.01-2.96) | 1.87 (1.82-2.09) | 0.36 |
| phrase | p50 ms | 0.72 (0.68-0.91) | 2.49 (2.05-3.12) | 2.18 (1.94-2.27) | 0.33 |
| country_agg_uncached | p50 ms | 18.96 (18.29-23.99) | 29.37 (25.03-35.98) | 43.53 (39.63-85.80) | 0.65 |
| country_agg_cached | p50 ms | 1.21 (1.21-1.25) | 2.78 (2.59-3.16) | 2.70 (2.45-2.98) | 0.45 |
| scroll | p50 ms | 487.5 (446.5-501.4) | 627.5 (530.2-641.7) | 552.3 (491.0-582.1) | 0.88 (tie) |
| expression | p50 ms | 58.08 (50.44-68.26) | 81.10 (67.45-99.94) | 85.17 (75.30-88.57) | 0.72 (tie) |
| painless_static | p50 ms | 46.31 (44.02-47.51) | 95.57 (77.79-120.7) | 107.5 (79.41-173.0) | 0.48 |
| painless_dynamic | p50 ms | 44.86 (44.68-51.78) | 103.8 (86.41-162.1) | 93.16 (85.89-96.35) | 0.48 |
| decay_geo_gauss_function_score | p50 ms | 44.01 (42.25-44.29) | 79.52 (66.51-87.04) | 81.96 (72.05-89.39) | 0.55 |
| decay_geo_gauss_script_score | p50 ms | 63.29 (59.82-71.64) | 78.73 (65.87-97.99) | 80.53 (76.90-82.35) | 0.80 (tie) |
| field_value_function_score | p50 ms | 10.12 (9.40-10.61) | 30.26 (26.62-36.91) | 31.82 (30.12-34.44) | 0.33 |
| field_value_script_score | p50 ms | 30.13 (28.49-31.46) | 44.96 (40.41-56.76) | 42.19 (40.37-42.55) | 0.71 |
| large_terms | p50 ms | 98.62 (89.30-100.2) | 163.0 (140.5-171.6) | 163.6 (160.2-187.7) | 0.60 |
| large_filtered_terms | p50 ms | 120.7 (91.13-124.6) | 179.4 (148.7-210.1) | 163.5 (155.1-176.9) | 0.74 |
| large_prohibited_terms | p50 ms | 100.6 (91.38-125.4) | 163.9 (145.9-200.7) | 235.0 (174.2-251.5) | 0.61 |
| desc_sort_population | p50 ms | 5.59 (5.48-5.71) | 5.25 (5.00-7.24) | 6.71 (5.75-7.12) | 1.07 (tie) |
| desc_sort_population_can_match_shortcut | p50 ms | 3.23 (2.94-3.29) | 5.85 (5.40-6.63) | 7.10 (6.95-10.02) | 0.55 |
| desc_sort_population_no_can_match_shortcut | p50 ms | 3.26 (2.46-3.35) | 5.57 (5.15-5.90) | 8.90 (7.35-9.53) | 0.59 |
| asc_sort_population | p50 ms | 4.25 (3.46-4.29) | 3.83 (3.76-4.09) | 4.70 (4.46-4.83) | 1.11 (tie) |
| asc_sort_with_after_population | p50 ms | 4.21 (3.74-4.86) | 4.64 (4.13-4.91) | 4.86 (4.59-6.09) | 0.91 (tie) |
| desc_sort_geonameid | p50 ms | 4.56 (3.91-5.29) | 6.39 (6.13-7.85) | 8.08 (6.47-8.39) | 0.71 |
| desc_sort_with_after_geonameid | p50 ms | 2.18 (2.07-2.28) | 9.21 (6.75-12.23) | 5.82 (5.10-6.17) | 0.37 |
| asc_sort_geonameid | p50 ms | 4.56 (4.13-4.76) | 5.15 (4.64-5.82) | 5.00 (4.30-6.23) | 0.91 (tie) |
| asc_sort_with_after_geonameid | p50 ms | 1.94 (1.71-2.01) | 5.17 (4.24-8.83) | 4.96 (4.54-5.97) | 0.39 |
| sort_country_code_can_match_shortcut | p50 ms | 5.95 (5.56-6.22) | 6.32 (6.27-10.87) | 8.49 (7.38-12.34) | 0.94 |
| sort_country_code_no_can_match_shortcut | p50 ms | 5.44 (4.72-6.66) | 6.70 (6.01-9.43) | 7.41 (6.24-10.12) | 0.81 (tie) |

The other standard tracks of Round 6's table (`percolator`, `nested`, `noaa`, `pmc`,
`so_vector`, `dense_vector`) are running with the same harness; their tables replace Round 6's
when they finish.

## Round 7: roll, Elasticsearch 9.5.5 and OpenSearch 3.9 (2026-11-12)

The tree merged with `main` (`search-dev --release`). All three targets in Docker on the same
CPUs (`10,11,28,29`: 2 cores / 4 hardware threads; lane b of the host's benchmark lanes, held
under its lock) and 4 GiB each, Elasticsearch and OpenSearch with 2 GiB heaps; Rally on
`12,13,30,31`. The host was busy (1-minute load 29–48 on 36 threads, other sessions building and
benchmarking), so the tables give medians over rounds with their min–max, and the server CPU
per request, which load distorts less than wall time. Runs alternate between targets per
workload (micro) and per round (Rally), so drift hits all three alike. Raw outputs:
`~/.cache/rsearch/bench/` on the machine that ran them.

Round 8's summary, above, replaces this round's.

**Footprint** (`bench-search.sh footprint`):

| measure | roll | Elasticsearch 9.5.5 | OpenSearch 3.9 |
|---|---:|---:|---:|
| RSS, idle node (MiB) | **24** | 2,519 | 2,517 |
| RSS, one loaded 50k-doc index (MiB) | **46** | 2,591 | 2,579 |
| store bytes per document (force-merged) | **199** | 263 | 262 |
| cold start to the first search hit | **22.6 ms** (wake after idle unload; warm 16.8 ms) | 40.4 s (node restart) | 38.4 s (node restart) |
| RSS after idle unload (MiB) | 44 | - | - |

The 44 MiB after an unload is mostly the binary's code paged in by the work and memory the
allocator is still giving back: anonymous memory measured separately came back to 15.8 MiB
after three load/unload cycles, the same after each cycle (nothing kept per cluster).

**Micro suite** (`bench-search.sh micro --rounds 3`, 50,000 generated documents, one shard; 200
requests per workload after 30 warm-up ones, every target answering the same seeded sequence):

| workload | roll median (min-max) | es median (min-max) | os median (min-max) | roll p50 / p99 | es p50 / p99 | os p50 / p99 |
|---|---:|---:|---:|---:|---:|---:|
| bulk 50k docs, 1 client | 1.367 s (1.306-2.131) | 3.053 s (2.805-7.545) | 3.007 s (2.422-5.855) | - | - | - |
| match | 0.572 ms (0.555-0.860) | 3.083 ms (2.532-7.025) | 2.849 ms (2.571-6.630) | 0.570 / 3.422 | 3.045 / 12.173 | 2.696 / 12.187 |
| match_or_2 | 0.786 ms (0.760-0.981) | 3.593 ms (2.774-6.858) | 3.412 ms (2.647-7.004) | 0.752 / 2.427 | 3.383 / 13.199 | 3.281 / 13.165 |
| match_or_3 | 1.044 ms (1.019-1.174) | 3.921 ms (2.936-6.898) | 3.470 ms (3.166-6.441) | 1.017 / 2.596 | 3.682 / 12.962 | 3.503 / 14.035 |
| match_and_2 | 0.854 ms (0.748-0.858) | 3.022 ms (2.501-5.515) | 3.148 ms (2.600-5.126) | 0.679 / 2.007 | 3.074 / 10.828 | 3.118 / 10.284 |
| bool_filter_range | 0.577 ms (0.518-0.596) | 2.865 ms (2.125-5.458) | 3.040 ms (2.474-4.794) | 0.528 / 1.135 | 2.801 / 9.777 | 2.819 / 9.537 |
| terms_agg | 0.550 ms (0.392-0.573) | 2.325 ms (1.900-4.634) | 2.109 ms (1.851-5.096) | 0.501 / 0.826 | 2.399 / 8.671 | 2.093 / 10.705 |
| date_histogram | 0.511 ms (0.489-0.623) | 2.291 ms (2.102-4.098) | 1.990 ms (1.476-3.479) | 0.504 / 0.935 | 2.184 / 7.561 | 1.939 / 6.719 |
| sort_field | 0.584 ms (0.487-0.787) | 2.931 ms (2.452-4.132) | 3.204 ms (2.488-3.766) | 0.554 / 1.421 | 3.035 / 7.742 | 3.000 / 8.101 |
| sort_match_all | 1.256 ms (1.062-1.330) | 3.168 ms (3.151-4.191) | 3.040 ms (2.924-4.014) | 1.091 / 2.205 | 3.318 / 8.035 | 2.980 / 7.818 |
| range_count | 0.440 ms (0.421-0.457) | 1.818 ms (1.231-2.788) | 1.610 ms (1.055-2.377) | 0.411 / 1.146 | 1.667 / 8.395 | 1.541 / 5.055 |
| match_all_count | 0.349 ms (0.288-0.486) | 2.117 ms (1.398-2.334) | 1.696 ms (1.273-1.991) | 0.349 / 0.606 | 1.875 / 5.946 | 1.582 / 3.506 |
| terms_agg (no cache) | 3.640 ms (3.637-3.968) | 6.734 ms (6.480-7.779) | 7.288 ms (6.551-8.494) | 3.627 / 7.720 | 6.680 / 12.568 | 6.822 / 17.494 |
| date_histogram (no cache) | 3.499 ms (3.487-4.358) | 5.921 ms (5.866-6.603) | 3.514 ms (2.856-3.662) | 3.604 / 6.861 | 5.812 / 11.736 | 2.877 / 7.876 |
| range_count (no cache) | 0.469 ms (0.414-0.544) | 1.558 ms (1.408-2.618) | 2.184 ms (1.963-2.374) | 0.431 / 1.369 | 1.631 / 4.621 | 1.877 / 6.725 |
| match_all_count (no cache) | 0.398 ms (0.390-0.554) | 1.415 ms (0.901-2.253) | 1.071 ms (0.982-1.837) | 0.368 / 2.925 | 1.377 / 6.211 | 1.093 / 4.402 |
| get | 0.282 ms (0.205-0.292) | 1.283 ms (1.045-1.619) | 0.858 ms (0.658-1.091) | 0.254 / 0.502 | 1.106 / 5.730 | 0.637 / 4.872 |

Server CPU time per request (bulk: per load), median over rounds (min-max):

| workload | roll CPU | es CPU | os CPU |
|---|---:|---:|---:|
| bulk 50k docs, 1 client | 1.639 s (1.614-2.174) | 5.695 s (5.376-17.344) | 6.055 s (4.288-16.228) |
| match | 0.396 ms (0.394-0.478) | 7.912 ms (2.851-13.674) | 5.565 ms (2.400-12.278) |
| match_or_2 | 0.592 ms (0.568-0.789) | 8.234 ms (2.330-16.929) | 8.907 ms (3.617-18.001) |
| match_or_3 | 0.839 ms (0.837-0.881) | 5.429 ms (5.005-16.317) | 6.884 ms (6.151-11.926) |
| match_and_2 | 0.568 ms (0.568-0.606) | 5.912 ms (4.540-7.917) | 6.197 ms (5.633-9.828) |
| bool_filter_range | 0.368 ms (0.296-0.486) | 3.189 ms (3.031-13.031) | 4.777 ms (2.507-12.106) |
| terms_agg | 0.347 ms (0.297-0.387) | 4.014 ms (2.401-9.447) | 3.380 ms (2.252-8.670) |
| date_histogram | 0.322 ms (0.316-0.501) | 3.483 ms (3.420-8.902) | 3.774 ms (2.554-6.551) |
| sort_field | 0.402 ms (0.353-0.594) | 5.592 ms (4.767-9.504) | 4.657 ms (2.940-9.619) |
| sort_match_all | 1.041 ms (0.859-1.202) | 5.429 ms (4.770-7.119) | 5.533 ms (5.134-8.059) |
| range_count | 0.249 ms (0.233-0.267) | 2.229 ms (0.972-4.096) | 2.427 ms (0.996-5.057) |
| match_all_count | 0.214 ms (0.169-0.314) | 3.823 ms (0.990-4.443) | 2.176 ms (0.945-3.652) |
| terms_agg (no cache) | 3.299 ms (3.241-3.513) | 11.714 ms (9.603-21.484) | 14.051 ms (8.047-15.886) |
| date_histogram (no cache) | 3.142 ms (3.085-3.929) | 8.667 ms (6.854-8.873) | 7.452 ms (3.915-7.619) |
| range_count (no cache) | 0.296 ms (0.261-0.297) | 2.562 ms (1.439-4.986) | 2.839 ms (1.713-3.969) |
| match_all_count (no cache) | 0.216 ms (0.212-0.231) | 1.092 ms (0.661-3.796) | 1.353 ms (0.690-2.727) |
| get | 0.113 ms (0.094-0.127) | 1.463 ms (1.139-2.737) | 1.787 ms (0.695-2.022) |

roll wins every row: 1.7–6.3x Elasticsearch's median latency and 2.6–15x less server CPU per
request; bulk loads in 1.37 s against 3.05 s on 3.5x less CPU. The closest row is an uncached
`date_histogram`: roll's median (3.50 ms) and CPU (3.14 ms) are better than OpenSearch's (3.51 ms,
7.45 ms CPU), but OpenSearch's p50 is lower (2.88 against 3.60 ms).

**Rally.** This round's Rally races (two rounds per target, then roll-only reruns on newer
binaries) are superseded by Round 8, which reran every track with all three targets alternating,
three rounds each, and lists what these had wrong.

## Round 6: every Rally track, indexing, memory and cold start (perf-bench lane, 2026-10-28)

A busy host this time (1-minute load 30–58 on 36 threads, other lanes building): Rally ran
with both targets on `14-17,32-35` and Rally on `6-9,24-27` as before; the indexing and
footprint measurements used a second pair of containers on `10-13,28-31` (`BENCH_CPUS`), every
comparison interleaved. `base` is the tree at the start of the round, `r5` the tree after this
lane's indexing changes (both `search-dev --release`). Raw outputs:
`~/.cache/roll-search/perf-bench/out{,2}/` on the machine that ran them.

**Indexing** (`bench-search.sh ingest`, the first 1M documents of `http_logs`' `logs-181998`,
5 shards, 5,000-doc bulks, 3 rounds; CPU per document includes the merges that finish within
10 s of the load):

| clients | metric | roll `base` | roll `r5` | Elasticsearch 9.5.5 |
|---|---|---:|---:|---:|
| 1 | docs/s | 91.5k | 103.6k (102.3k–128.0k) | 89.4k (51.5k–116.3k) |
| 1 | bulk p50 / p99 ms | 48.3 / 241 | 47.1 / 93.5 | 55.9 / 117 |
| 8 | docs/s | 274k | 246k (232k–319k) | 192k (171k–289k) |
| 8 | bulk p50 / p99 ms | 134.5 / 252 | 157.3 / 274 | 188.9 / 427 |
| 32 | docs/s | 277k | 274k (237k–303k) | 227k (152k–243k) |
| 32 | bulk p50 / p99 ms | 508 / 1,021 | 545 / 1,013 | 660 / 898 |
| 8 | CPU µs/doc (settled) | 33.6 | 28.0 | 32.5 |
| 8 | final refresh ms | 444 | 259 | 209 |

The `base` column ran earlier on the quieter `14-17,32-35` pair against an Elasticsearch that
indexed at 288k (8 clients) and 316k (32 clients) there, so compare each column with its own
run: against the Elasticsearch of its run, `base` lost at 1 and 32 clients and on CPU per
document; `r5` wins every row but the final refresh. The same A/B on one pair of CPUs, `r4` →
`r5` (the fast-path and version-map rows below): 8 clients 225k → 293k docs/s, p50 174 → 133
ms, 36.2 → 26.6 µs/doc. Rally's `so` corpus (1M posts, text-heavy), 8 clients: roll `r5`
56.4k docs/s, p50 684 ms, 149 µs/doc against Elasticsearch's 32.0k, 1,015 ms, 191 µs/doc
(`base` lost this track in Rally: 34.2k vs 43.1k docs/s).

Tuning log (each row identical results: unit, oracle and REST-fixture tests, both YAML suites
with 0 regressions, the ES suite also with every bulk through the queued path):

| change | where | before | after |
|---|---|---:|---:|
| BP keeps each term's gain `log2(right) - log2(left)` current instead of computing both logarithms for every posting on every iteration (the same `f32` sums in the same order: a bit-identical doc map, tested against the old algorithm) | `search-codec` `bp.rs` | BP of 913k http_logs docs 3.26 s | 2.73 s |
| natural merges BP-reorder only results of at least 2^18 docs (Lucene's suggested `minNaturalMergeNumDocs`; forced merges still from 2^16): BP plus writing the reordered segment were about half of all merge CPU while indexing, and merges 56% of all CPU (http_logs, 8 clients) | `search-index` `shard/mod.rs` | 8 clients, settled CPU 48.4 µs/doc, 5 merges of 1M docs in 8.4 s | 38.5 µs/doc, merges without BP |
| one-dimensional points of 13–16 bytes (ip) are built from one sort by (value, doc) as integers instead of a median selection per tree level and a sort per leaf over `memcmp`ed slices (same bytes: a 1-d tree is the sorted points cut into leaves) | `search-codec` `points.rs` | `memcmp` from the points builder ~2.8% of bulk CPU | (in the rows below) |
| `_bulk` applies index items prepared ahead with the other items of their shard, the shards in parallel on the pool, as Elasticsearch runs a request's shard-level bulks; deletes, updates and anything not prepared first wait for the queued writes, so each shard sees request order (a 600-item mixed bulk equals the same items sent one at a time, item results and final versions, seq nos and sources) | `search-es` `api/bulk.rs`, `api/document.rs`, `api/bulk_ahead.rs` | 1 client 54.2k docs/s, p50 84.3 ms | 80.7k docs/s, 56.5 ms |
| `_refresh` (and `refresh=true` writes) refresh their shards in parallel instead of one after another | `search-es` `engine/mod.rs`, `write.rs` | final refresh after 1M docs 673 ms | 294 ms |
| the write that crosses the indexing buffer budget hands the buffer's conversion to a thread of its own (writers far over budget still wait: stall control) | `search-index` `shard/mod.rs` | 8 clients p99 364 ms | 239 ms |
| `ip` (from a string) and `match_only_text` take the bulk fast path: every http_logs document used to fall back to the JSON tree walk (`json::parse`, `json::locate`, `Walker`, an `IndexMap` per document) | `search-es` `document/fast.rs` | 8 clients 225k docs/s, 36.2 µs/doc | 293k docs/s, 26.6 µs/doc (with the next row) |
| version-map stripes hash ids with foldhash (randomly seeded per map) instead of SipHash | `search-index` `shard/mod.rs` | SipHash ~1% of bulk CPU | 0 |
| `percolate` verifies each distinct stored query once (stored queries deduplicated by their bytes per segment; a match applies to every document storing it) | `search-es` `search/percolate*.rs` | Rally `percolator_with_content_ignore_me` (2M stored queries, most of them one `match`) 5,903 ms, filter form 4,980 ms | 15.2 ms, 5.6 ms (Elasticsearch 4,618 and 8.6 ms) |

What indexing still spends (8 clients, `r5`): stored-fields zstd at level 3 with a trained
dictionary at every buffer conversion ~10% of all CPU (Elasticsearch's default stored-fields
codec is LZ4; handed to the fetch lane, which is changing that format), the allocator ~9%,
the term hash 2.6%, points 4.6%, the WAL's zstd 3%.

**Memory and cold start** (`r5` unless noted):

| measure | roll | Elasticsearch 9.5.5 |
|---|---:|---:|
| idle node, 10 min, a loaded 200k-doc index: CPU / syscalls / context switches (`perf stat`) | `base`: 58 ms / 4,790 / 2,409 (all from the purge governor's 4 Hz tick); `r5`: 0 ms / 0 / 0 | - |
| sustained load: 12 back-to-back loads of 1M docs with searches running | RSS 375–410 MiB between loads (no upward trend), 961 MiB peak, 384 MiB 30 s after | - |
| wake from object storage: a fresh node (empty cache) on MinIO, 12M http_logs docs (878 MB, 5 shards), launch to first search hit | p50 582 ms, max 650 ms (12 runs); 742–878 MB in the cache at the first hit; next search 13–32 ms | node restart 21 s (round 3) |
| RSS at the end of a Rally race (heaviest track per side) | 0.3–2.0 GiB | 1.9–3.5 GiB |
| `bench-search.sh footprint`: RSS idle node / one loaded 50k-doc index / after idle unload | 24 / 45 / 43 MiB | 2,584 / 2,675 MiB / - |
| `bench-search.sh footprint`: wake after idle unload (local cache) / warm request | 19.7 / 15.0 ms | node restart 30.6 s |
| store bytes per document, force-merged (micro corpus) | 199 (perf-fetch's 2 KiB stored chunks; was 192) | 263 |

The governor now parks once it has collected an idle process's memory (12 s idle) and the
first request wakes it, so an idle node makes no periodic system calls. The wake measurement
runs MinIO on the same host (no network latency): nearly the whole index is downloaded before
the first search answers, so on S3 the wake is bounded by object throughput (about 1.5 GB/s
here); opening shards on their headers and fetching only what the first query reads is the
next step for large indices.

(Round 8 is rerunning these tracks with all three targets; its tables replace this one when
they finish.)

**Rally, the other standard tracks** (`base`, two rounds, p50 service time in ms unless noted;
subsets as listed under Methodology; vector tracks with the tracks' default index types):

| track | task | roll | Elasticsearch |
|---|---|---:|---:|
| percolator | index (docs/s, p50) | 170k, 205 ms | 127k, 234 ms |
| percolator | president_bush / saddam_hussein / hurricane_katrina / highlighting | 10.1 / 3.0 / 8.5 / 5.5 | 20.6 / 6.6 / 15.4 / 12.1 |
| percolator | google / no_score_google | 52.9 / 51.5 | 46.9 / 6.9 |
| percolator | ignore_me / no_score_ignore_me | 6,794 / 6,824 (`r5` on the same index: 15.2 / 5.6) | 4,618 / 8.6 |
| pmc | index-append (docs/s, p50) | 1,157, 3,133 ms | 1,050, 3,409 ms |
| pmc | default / term / phrase | 3.69 / 3.35 / 3.39 | 6.20 / 5.85 / 6.47 |
| pmc | articles_monthly_agg uncached / cached | 4.71 / 0.97 | 6.17 / 2.74 |
| pmc | scroll | 441 | 477 |
| nested | index-append (docs/s, p50) | 81.1k, 239 ms | 78.0k, 199 ms |
| nested | randomized-term-queries / match-all | 1.59 / 1.37 | 2.10 / 2.44 |
| nested | randomized-nested-queries / sorted-term-queries | 7.70 / 15.06 | 6.98 / 14.47 |
| nested | nested-date-histo | 426.6 | 163.6 |
| nested | inner hits default / big size | 11.8 / 46.7 | 8.8 / 20.5 |
| noaa | index (docs/s, p50) | 103.2k, 373 ms | 106.5k, 280 ms |
| noaa | range_field (`double_range`) big / small range | 24.2 / 24.9 | 6.8 / 5.8 |
| noaa | range_field conjunctions / disjunctions | 26.9–93.5 / 26.2–26.4 | 6.2–7.6 / 6.5–6.8 |
| so | index-append (docs/s, p50) | 34.2k, 1,226 ms (`r5`: see Indexing) | 43.1k, 711 ms |
| nyc_taxis | index (docs/s, p50) | 95.3k, 782 ms | 89.3k, 711 ms |
| nyc_taxis | default / default_1k | 1.55 / 7.54 | 6.12 / 14.75 |
| nyc_taxis | range / date_histogram_agg | 25.7 / 57.2 | 9.1 / 10.4 |
| nyc_taxis | autohisto_agg / distance_amount_agg | 107.0 / 829.0 | 31.2 / 311.1 |
| so_vector | index-append (docs/s, p50) | 1,406, 190 ms | 1,568, 361 ms |
| so_vector | knn 10-50 / 100-300 (match_all) | 26.8 / 85.1 | 4.8 / 10.7 |
| so_vector | knn force-merged 10-50 / 100-300 | 16.1 / 69.2 | 4.0 / 9.5 |
| so_vector | knn filtered (10-50: acceptedAnswerId, java, css, random 10/20%) | 22.7–30.7 | 4.2–5.9 |
| dense_vector | knn 10-100 / 100-1000 | 4.50 / 7.41 | 2.47 / 4.29 |
| dense_vector | knn multiple segments 10-100 / 100-1000 | 6.73 / 24.3 | 3.17 / 5.07 |

(`script-score-query` tasks of the vector tracks fail on both sides with the track's
parameters.) roll wins `pmc` outright and every term, match-all and plain `default` task;
the losses are in five query families, handed to the query lanes with profiles: k-NN search
on HNSW (4–9x on `so_vector`'s BBQ index, 2–5x on `dense_vector`'s int8), `range` queries on
range fields (`noaa`, 3.5–12x), histogram and date-histogram aggregations over range filters
(`nyc_taxis`, 2.7–5.5x), nested aggregations and inner hits (`nested`, 1.3–2.6x), and the
percolator's verified-match shortcut, fixed here.

## Final run (end of the phase 3–5 round, 2026-10-26)

The tree at the end of the round (every lane merged, plus the integration fix below), run once
nothing else was changing the code and the host was quiet: 1-minute load 1–3 on 36 threads,
instead of the 5–49 of the earlier runs. Same setup as rounds 2 and 3: both targets pinned to
`14-17,32-35` with `-m 4g`, Rally on `6-9,24-27`, runs interleaved, medians (min–max) over
rounds. roll = `final2`. Raw outputs: `target/bench-search/{micro-final2,rally-http_logs-final,
rally-geonames-final,footprint-final}*`.

On a quiet host the cores sleep between requests, so a sequential client's wall time includes
waking them (on both sides); server CPU per request is the steadier comparison.

**Micro suite** (5 rounds of 200 requests per workload; wall time per request, and server CPU
per request from the containers' cgroups):

| workload | roll wall | Elasticsearch wall | roll CPU | Elasticsearch CPU |
|---|---:|---:|---:|---:|
| bulk 50k docs, 1 client | 0.937 s (0.700-1.048) | 1.988 s (1.825-3.109) | 1.289 s | 3.507 s |
| match | 1.517 ms (0.778-1.650) | 2.465 ms (1.938-5.884) | 1.139 ms | 4.300 ms |
| match_or_2 | 2.139 ms (2.126-2.189) | 3.059 ms (2.694-4.546) | 1.699 ms | 3.614 ms |
| match_or_3 | 2.221 ms (2.097-2.272) | 3.201 ms (3.163-5.117) | 1.777 ms | 4.207 ms |
| match_and_2 | 1.799 ms (1.655-1.875) | 2.748 ms (2.369-4.202) | 1.399 ms | 3.643 ms |
| bool_filter_range | 0.567 ms (0.509-0.587) | 2.312 ms (1.765-3.201) | 0.401 ms | 2.834 ms |
| terms_agg | 0.498 ms (0.369-0.524) | 2.159 ms (1.585-3.607) | 0.347 ms | 2.363 ms |
| date_histogram | 0.499 ms (0.414-0.525) | 2.206 ms (1.766-2.779) | 0.343 ms | 2.579 ms |
| sort_field | 1.745 ms (1.697-1.850) | 2.828 ms (2.663-3.145) | 1.346 ms | 3.275 ms |
| sort_match_all | 1.852 ms (0.668-1.984) | 3.211 ms (3.034-4.030) | 1.441 ms | 3.776 ms |
| range_count | 0.394 ms (0.355-0.452) | 1.831 ms (1.545-2.227) | 0.263 ms | 1.321 ms |
| match_all_count | 0.379 ms (0.290-0.426) | 1.504 ms (1.378-2.081) | 0.249 ms | 1.943 ms |
| terms_agg (no cache) | 1.846 ms (1.789-2.046) | 7.781 ms (7.566-8.102) | 1.505 ms | 10.272 ms |
| date_histogram (no cache) | 2.118 ms (2.024-2.172) | 7.885 ms (7.645-8.548) | 1.744 ms | 8.089 ms |
| range_count (no cache) | 0.483 ms (0.468-0.514) | 1.606 ms (1.071-1.981) | 0.325 ms | 2.141 ms |
| match_all_count (no cache) | 0.299 ms (0.267-0.366) | 1.471 ms (0.931-1.750) | 0.215 ms | 1.069 ms |
| get | 0.365 ms (0.287-0.368) | 1.406 ms (1.103-1.542) | 0.176 ms | 1.303 ms |

roll wins every row: 1.4–4.9x in wall time and 2.1–7.8x in server CPU. Bulk is now 2.1x faster
(it was the thinnest margin in round 2, 2.48 vs 2.80 s), and uncached aggregations 3.7–4.2x.
Queries that fetch hits are where roll's lead is smallest (1.4–1.7x): see "Still open" below.

(This run's Rally tables are superseded by Round 8.)

**Footprint** (`scripts/bench-search.sh footprint`):

| measure | roll | Elasticsearch 9.5.5 |
|---|---:|---:|
| RSS, idle node | 24 MiB | 2,608 MiB |
| RSS, one loaded 50k-doc index | 65 MiB | 2,723 MiB |
| store bytes per document (force-merged) | 192 | 263 |
| data directory bytes per document (incl. WAL, metadata) | 485 | - |
| cold start to first search hit | 26.9 ms (wake after idle unload; warm request 21.6 ms) | 21,166 ms (node restart) |
| RSS after idle unload | 42 MiB | - |

**Integration fix found by this run.** On the quiet host, query rows of the micro suite
came out 0.3–0.6 ms slower than in round 2 while `get` was unchanged. An A/B of the round-2
binary (`r5l`) against later ones (same data, interleaved) placed the change between `r5l` and
`r8e`, and a profile of a `size: 0` search showed `std::thread::available_parallelism` at 28%
of its CPU: the standard library re-reads the cgroup's `cpu.max` and the mount table on every
call, and the search path had come to call it several times per request (shard query phases,
the parallel fetch check, the OpenSearch profile, gzip). It is now looked up once
(`search_index::search::pool::cores()`): server CPU of a `size: 0` search 0.50 → 0.24 ms (round
2: 0.34), of a 10-hit `match` 1.57 → 1.23 ms.

**Still open** (performance): two geonames script scores in this run's Rally races,
`decay_geo_gauss_script_score` (37.1 vs 34.7 ms) and `field_value_script_score` (21.8 vs 20.1);
both lead in Round 8. (Fetching hits decompressed a
whole 16 KiB zstd stored-fields chunk per hit, 36% of a 10-hit `match`'s CPU, and a 10-hit fetch
cost about 0.1 ms more CPU than in round 2: round 6 cut the server CPU of that `match` from 0.78
to 0.43 ms, below round 2's, see "Fetch phase (round 6)".)

## Micro suite (round 1 of the perf lane)

Per request, lower is better (bulk: per 50k-doc load). roll = `r4a` (the tree before this
round's tuning; the tuning below does not touch these paths except bulk merging).

| workload | roll median (min-max) | Elasticsearch median (min-max) | roll p50 / p99 | ES p50 / p99 |
|---|---:|---:|---:|---:|
| bulk 50k docs, 1 client | 2.001 s (1.738-2.442) | 3.202 s (2.310-5.262) | - | - |
| match | 1.142 ms (1.053-1.425) | 2.805 ms (1.888-10.127) | 1.139 / 2.023 | 2.718 / 19.979 |
| match_or_2 | 1.472 ms (1.340-1.866) | 2.858 ms (2.119-7.529) | 1.500 / 2.828 | 2.782 / 15.394 |
| match_or_3 | 1.693 ms (1.568-1.742) | 3.145 ms (2.341-5.762) | 1.680 / 2.436 | 3.168 / 10.768 |
| match_and_2 | 1.183 ms (1.125-1.212) | 2.621 ms (2.034-4.276) | 1.142 / 2.698 | 2.695 / 5.863 |
| bool_filter_range | 0.439 ms (0.402-0.504) | 3.577 ms (1.672-4.096) | 0.433 / 0.714 | 2.764 / 8.707 |
| terms_agg | 0.368 ms (0.366-0.524) | 2.140 ms (1.226-3.442) | 0.389 / 0.646 | 2.121 / 4.760 |
| date_histogram | 0.411 ms (0.395-0.428) | 1.971 ms (1.442-3.164) | 0.404 / 0.632 | 2.007 / 4.431 |
| sort_field | 1.061 ms (1.033-1.159) | 2.773 ms (1.903-3.766) | 1.075 / 1.408 | 2.631 / 6.521 |
| sort_match_all | 1.566 ms (1.498-1.598) | 2.980 ms (2.211-3.420) | 1.533 / 2.574 | 2.876 / 4.867 |
| range_count | 0.343 ms (0.305-0.448) | 1.106 ms (1.084-1.278) | 0.326 / 0.610 | 1.072 / 2.740 |
| match_all_count | 0.311 ms (0.284-0.460) | 1.130 ms (0.893-1.254) | 0.304 / 0.535 | 1.028 / 2.213 |
| terms_agg (no cache) | 3.662 ms (3.596-4.103) | 6.293 ms (6.208-11.541) | 3.961 / 4.918 | 6.409 / 29.141 |
| date_histogram (no cache) | 3.525 ms (3.504-4.002) | 5.446 ms (5.129-7.698) | 3.673 / 4.187 | 5.470 / 14.082 |
| range_count (no cache) | 0.426 ms (0.369-0.696) | 1.241 ms (0.828-2.097) | 0.418 / 0.808 | 1.069 / 4.621 |
| match_all_count (no cache) | 0.292 ms (0.276-0.370) | 1.006 ms (0.628-1.691) | 0.292 / 0.418 | 0.930 / 2.413 |
| get | 0.268 ms (0.258-0.272) | 0.740 ms (0.598-1.471) | 0.262 / 0.398 | 0.709 / 3.870 |

Server CPU per request: roll uses 2.8 s of CPU per 50k-doc bulk load against Elasticsearch's
7.8 s, and 0.15–3.4 ms per query against 2.2–13.3 ms (JIT, GC and background work included on
the JVM side). roll wins every row; the rows it lost in the previous round's ad-hoc run (bulk,
`match` with 2 terms, `sort_field`, `match_all_count` without cache) now win by 1.4–3.5x.

## Round 3 of the perf lane

Same setup as round 2: both targets pinned to `14-17,32-35` with `-m 4g`, Rally on
`6-9,24-27`, two rounds per track. roll = `r9c`, the tree at the end of the round. Its Rally
tables are superseded by Round 8; what the round changed, measured in Rally: geonames `scroll`
went from a 1.5x loss (806 vs 521 ms in `r8e`) to a 1.4x win (502 vs 686 ms): Rally scrolls
with `sort: ["_doc"]`, which now stops each segment once the page is full. On http_logs the two
tasks roll had lost, `sort_keyword_*` (34 vs 5 ms: the sort field is absent from these
documents) and `asc_sort_with_after_timestamp` (11 vs 4.7 ms, 24 vs 4.0 ms force-merged: the
bound lies after every value), went to 3.7 vs 5.8 ms and 1.8 vs 5.5 ms (2.1 vs 4.6 ms
force-merged).

## Round 2 of the perf lane

Targets pinned to the 8 hardware threads `14-17,32-35` (`BENCH_CPUS`), Rally on `6-9,24-27`,
same Docker limits as below. roll = `r5i` (this round's tuning, before the analyzer pool fix
that came after it). Rally geonames 10%: two roll races and three Elasticsearch races; the
Rally table is superseded by Round 8.

Against the previous round's run (`r4g`): Painless `painless_static` /
`painless_dynamic` 440 / 371 ms → 68 / 64 ms (Elasticsearch 66 / 62: even),
`decay_geo_gauss_script_score` 273 → 73 ms (Elasticsearch 58), `field_value_script_score` 66 → 44
ms (38), `expression` 112 → 61 ms (54), `field_value_function_score` 37 → 12.6 ms (Elasticsearch
25: roll 2x faster), `decay_geo_gauss_function_score` 77 → 35 ms (54), `large_terms` 145 → 103 ms
(126: roll ahead), `scroll` 1,463 → 1,234 ms (510: still 2.4x behind, see below). Indexing lost
this run: 48.6k docs/s against Elasticsearch's 89.2k (Elasticsearch ranged 33.7k–93.8k over its
races; last round it measured 51.3k against roll's 49.5k). A profile of the 8-client load found
a quarter of the indexing threads' context switches queued on one lock in every analyzer's chain
pool; with the pool sharded by thread (after this run, `r5l`) the same 8-client geonames load of
1.1M docs (5,000-doc bulks, data directory) went from 52.5k to 64.4k docs/s (median of 3,
interleaved), with server CPU 133 → 128 s per load against Elasticsearch's 85 s: roll still
spends ~50% more CPU per document while indexing, spread over JSON parsing, the document walk,
analysis, the in-memory term hash and segment flushes (no single hot spot left).

`scroll`: the server side of a 1,000-hit continuation page went from 23.6 to 11.8 ms
(Elasticsearch 6.5–11 ms on the same CPUs); the rest of Rally's 2.4x is client time spent on
roll's responses and mimalloc returning freed pages at once (`purge_delay` 0, for idle RSS):
with `ROLL_MI_PURGE_DELAY_MS=100` (a new opt-in) the same scroll used 18% less server CPU.

Micro suite (`r5k`, same pinning, 3 rounds; per request, lower is better):

| workload | roll median (min-max) | Elasticsearch median (min-max) |
|---|---:|---:|
| bulk 50k docs, 1 client | 2.477 s (2.021-2.736) | 2.798 s (2.508-2.948) |
| match | 1.081 ms (1.037-1.386) | 2.268 ms (1.438-2.640) |
| match_or_2 | 1.491 ms (1.363-1.913) | 2.575 ms (1.823-3.246) |
| match_or_3 | 1.887 ms (1.627-2.234) | 2.718 ms (2.438-3.457) |
| match_and_2 | 1.340 ms (1.090-1.415) | 2.482 ms (1.861-2.857) |
| bool_filter_range | 0.612 ms (0.489-0.672) | 2.118 ms (1.067-2.684) |
| terms_agg | 0.590 ms (0.542-0.701) | 1.494 ms (0.968-2.472) |
| date_histogram | 0.567 ms (0.477-0.722) | 1.932 ms (0.934-2.730) |
| sort_field | 1.382 ms (1.084-1.663) | 2.134 ms (1.470-2.959) |
| sort_match_all | 1.814 ms (1.532-1.982) | 2.557 ms (2.204-3.304) |
| range_count | 0.375 ms (0.345-0.593) | 1.023 ms (0.825-1.320) |
| match_all_count | 0.390 ms (0.364-0.505) | 0.970 ms (0.696-1.336) |
| terms_agg (no cache) | 3.816 ms (3.786-4.260) | 6.858 ms (6.237-8.347) |
| date_histogram (no cache) | 3.878 ms (3.544-3.955) | 6.337 ms (5.744-6.910) |
| range_count (no cache) | 0.548 ms (0.429-0.591) | 1.330 ms (0.811-1.913) |
| match_all_count (no cache) | 0.454 ms (0.360-0.503) | 1.113 ms (0.655-1.606) |
| get | 0.312 ms (0.276-0.363) | 0.863 ms (0.773-1.807) |

roll wins every row; bulk is the thinnest (2.48 vs 2.80 s per 50k docs, server CPU 3.1 vs 2.8 s).

## Indexing nyc_taxis (2026-11-13)

`scripts/bench-search.sh ingest --file …/nyc_taxis/documents-2000000.json --body <the track's
mapping, 1 shard> --docs 1000000 --clients 1,8 --rounds 3`: each target in Docker on the same 4
hardware threads (`10,11,28,29`) and 4 GiB, rotating, on a host at load 20-29. Median of 3 (min-max);
CPU per document is the container's CPU time over the documents (then after the refresh settles).
The run before this round's parser change had roll at 60-66k docs/s with 8 clients and 58-65 µs
per document, behind Elasticsearch: nyc_taxis' `scaled_float` fields kept every document off the
direct parser (tuning log).

| clients | target | docs/s | p50 bulk ms | p99 bulk ms | CPU µs/doc |
|---|---|---:|---:|---:|---:|
| 1 | roll | **66,319** (51,465-73,882) | **72.6** | **174.2** | 39.7 (35.5-41.3) |
| 1 | Elasticsearch 9.5.5 | 38,039 (29,139-43,840) | 123.3 | 226.7 | 34.9 (30.3-82.7) |
| 1 | OpenSearch 3.9 | 32,187 (26,741-33,275) | 147.5 | 301.1 | 40.1 (35.0-75.1) |
| 8 | roll | **90,195** (89,109-92,896) | **423.2** | **790.8** | **42.0** (39.9-42.9) |
| 8 | Elasticsearch 9.5.5 | 66,346 (61,663-78,655) | 510.6 | 1,257.9 | 51.6 (47.8-58.7) |
| 8 | OpenSearch 3.9 | 59,610 (49,214-64,886) | 641.4 | 1,200.7 | 62.5 (56.4-75.6) |

### An ECS access log

The same harness on `scripts/bench-search-ecs.py`'s corpus (1M documents, objects up to three
levels deep), at host load 28-36 (wider spreads):

| clients | target | docs/s | p50 bulk ms | p99 bulk ms | CPU µs/doc |
|---|---|---:|---:|---:|---:|
| 1 | roll | **51,478** (46,717-52,096) | **87.0** | **240.9** | 51.8 (49.4-56.4) |
| 1 | Elasticsearch 9.5.5 | 35,118 (32,638-35,277) | 129.2 | 317.6 | **34.6** (32.2-55.5) |
| 1 | OpenSearch 3.9 | 24,342 (22,421-27,141) | 183.5 | 393.7 | 53.8 (42.1-78.8) |
| 8 | roll | **74,523** (61,551-84,847) | **520.7** | **935.1** | **50.9** (45.1-59.1) |
| 8 | Elasticsearch 9.5.5 | 59,031 (51,791-72,111) | 608.5 | 1,434.1 | 57.3 (51.8-63.7) |
| 8 | OpenSearch 3.9 | 41,291 (34,073-44,665) | 921.7 | 1,647.7 | 91.0 (82.9-92.6) |

With one client Elasticsearch spends less CPU per document than roll (it waits on each request
longer); that is the next thing to look at.

## Footprint and cold start

`scripts/bench-search.sh footprint` (r4g, same pinning):

| measure | roll | Elasticsearch 9.5.5 |
|---|---:|---:|
| RSS, idle node | 23 MiB | 2,395 MiB |
| RSS, one loaded 50k-doc index | 42 MiB | 2,438 MiB |
| store bytes per document (force-merged) | 192 | 263 |
| data directory bytes per document (incl. WAL, metadata) | 485 | - |
| cold start to first search hit | 19.8 ms (wake after idle unload) | 35,661 ms (node restart) |
| RSS after idle unload | 40 MiB | - |

The cold-start figures are wall time of a `curl` from the host (process start included; the
same `curl` on the woken cluster took 15.0 ms on this loaded machine), so roll's wake costs
about 5 ms over a warm request. Elasticsearch cannot unload an index; its comparable cold
start is a node start (JVM, cluster state, shard recovery).

## Vector search (2026-11-14)

`scripts/bench-search-vectors.py` generates the corpus, computes the exact answers and drives
the three targets; `scripts/bench-search.sh start` starts them in Docker as for the other
rounds, here with `BENCH_CPUS=10,11,28,29` (2 cores, 4 hardware threads) and `BENCH_MEM=6g`
for each target (4 GiB left Elasticsearch and OpenSearch too little page cache for 1.5 GB of
float vectors beside their 2 GiB heaps). Clients ran on `12,13,30,31`; one target was under load
at a time, every target settled (under 2% of a CPU for 5 s) before the next one was measured,
and search rounds rotated the targets. The host's load average was 20–30 on 36 threads
throughout, so trust server CPU per request over wall time, and differences only where the
spreads (min–max over 3 rounds of 200 queries after 30 warm-up queries) don't overlap.

- **Corpus** (`bench-search-vectors.py gen`, seed 1): 1,000,000 documents with a 384-dim
  `dense_vector` `v`, a keyword `tag` (100 values, uniform), a date `ts` and a three-word text
  `title` drawn from its cluster's topic words. Vectors are a Gaussian mixture of 300 clusters of
  unequal weight, each spread mostly along 16 random directions of its own plus a little
  isotropic noise, normalized and rounded to 4 decimals (2.96 GB of JSON). 200 queries come
  from the same mixture (another seed). The truth is the exact cosine top 100 of every query,
  computed with numpy on the same float32 values the engines parse, for no filter, a 10% filter
  (`terms` on ten `tag` values) and a 1% filter (`term` on one).
- **Queries**, one client, `_source: false`, `k` 10, `num_candidates` 100 (OpenSearch:
  `k` 10, `method_parameters.ef_search` 100): `knn` (the top-level section), `knn` with the 10%
  and the 1% filter, and `rrf` of a `match` on `title` and the same `knn` (Elasticsearch and
  roll: the `rrf` retriever, `rank_window_size` 100; OpenSearch: a `hybrid` query through a
  search pipeline with the `score-ranker-processor` `rrf` technique). Recall@10 is against the
  exact truth (`rrf` has none).
- **Indexing**: all 1M documents in 2,000-doc bulks prepared in memory, 8 clients, then a
  refresh; server CPU per document from the container's cgroup. Then the target settles and the
  searches run on the segments the engine left; then `_forcemerge?max_num_segments=1` (timed),
  and the searches again.
- **Index settings** (one shard, no replicas, `m` 16 and `ef_construction` 100 everywhere):

| config | roll | Elasticsearch 9.5.5 | OpenSearch 3.9.0 (`index.knn: true`, `space_type: cosinesimil`) |
|---|---|---|---|
| default (no `index_options`) | `bbq_hnsw`, `rescore_vector.oversample` 3 (Elasticsearch's default at 384 dims) | the same | `knn_vector`: faiss HNSW, float32 |
| `hnsw` | float HNSW | float HNSW | faiss HNSW, float32 |
| `int8_hnsw` | `int8_hnsw` (confidence interval default), no rescore option | the same | Lucene engine HNSW with its `sq` encoder, `bits` 7 (Lucene's int7) |
| `bbq_hnsw` | = default | = default | `mode: on_disk`, `compression_level: 32x` (not run) |

Roll ran `49178a66c` (plus the sort lane's uncommitted work in the tree) for the three rounds
and `f00ed2095` for the rerun below, release builds. In the `hnsw` and `int8_hnsw` rounds roll's
container ran with `--idle-unload-ms 2000` (left over from the cold-start measurement): an
unloaded shard stops merging, so roll kept 60–64 segments there against 18 in the default
round, which inflates its unmerged filtered-search CPU in those two rounds; the footprint
rows marked † are after an unload.

Reproduce (numpy for `gen`; the rounds above used small driver loops around these commands):

```
python3 scripts/bench-search-vectors.py gen ~/.cache/rsearch/vectors/c1m
python3 scripts/bench-search-vectors.py body es default 384 > body.json     # or os; hnsw, int8_hnsw, bbq_hnsw
BENCH_CPUS=10,11,28,29 BENCH_MEM=6g scripts/bench-search.sh start
python3 scripts/bench-search-vectors.py ingest http://127.0.0.1:19801 ~/.cache/rsearch/vectors/c1m \
  --index vec --body body.json --clients 8 --cgroup /sys/fs/cgroup/system.slice/docker-<id>.scope
python3 scripts/bench-search-vectors.py search http://127.0.0.1:19801 ~/.cache/rsearch/vectors/c1m \
  --index vec --dialect es --kind knn_f10 --cgroup ...                    # knn, knn_f10, knn_f1, rrf
```

In short:

- **Indexing**: with quantized graphs roll indexes fastest and cheapest (`bbq_hnsw` 4,735 docs/s
  at 8 clients against 3,585 and 2,978, 787 µs of CPU per document against 1,088 and 1,109;
  one client 4,574 against 1,776 and 2,869). Float HNSW costs roll more CPU than Elasticsearch
  (1,250 against 1,174 µs/doc).
- **Search on one segment**: roll uses the least CPU for `knn` (1.8–2.4 ms against 6.1–9.4 for
  Elasticsearch and 3.5–5.3 for OpenSearch) and `rrf` (2.8–3.3 against 8.4–14.1), at equal or
  better recall than Elasticsearch in most rows: roll rescores the whole beam with floats.
  Filtered `knn` was up to 1.4× Elasticsearch's CPU and is now at or below it (change below).
- **Search on the segments indexing left**: plain `knn` costs about what Elasticsearch's does;
  filtered `knn` still costs roll more CPU and 2–3× the latency, since roll searches segments
  one after another.
- **Footprint**: an idle roll node holds 34 MiB and answers the first `knn` 36 ms after a wake
  (213 ms with the page cache dropped), against 25–29 s for a node start of the others. roll's
  store was 56% larger than Elasticsearch's, all of it vectors kept in `_source`; since roll
  stores `_source` without them (below) it is 1.74 GB against Elasticsearch's 1.71.


### Default index options (roll and Elasticsearch: `bbq_hnsw`; OpenSearch: faiss HNSW float32)

#### Indexing and footprint

| measure | roll | Elasticsearch | OpenSearch |
|---|---:|---:|---:|
| docs/s, 1 client (first 200,000 docs) | 4,574 | 1,776 | 2,869 |
| server CPU µs/doc, 1 client | 675 | 686 | 544 |
| docs/s, 8 clients | 4,735 | 3,585 | 2,978 |
| server CPU µs/doc (load) | 787 | 1,088 | 1,109 |
| merges settling after the load (s) | 25 | 86 | 173 |
| segments after settling | 18 | 33 | 44 |
| peak RSS during ingest (MiB) | 2,316 | 3,675 | 4,674 |
| RSS after ingest (MiB) | 962 | 3,062 | 3,805 |
| RSS after the search rounds (MiB) | 2,181 | 2,827 | 5,420 |
| store bytes, not merged | 2,659,961,089 | 1,697,391,404 | 3,276,053,557 |
| force-merge (s) | 372 | 247 | 585 |
| peak RSS incl. force-merge (MiB) | 5,470 | 5,908 | 6,105 |
| store bytes, force-merged | 2,674,020,066 | 1,709,711,301 | 3,276,936,751 |
| RSS after merged search rounds (MiB) | 1,679 | 2,598 | 4,927 |
| JVM heap used after search rounds (MiB) | – | 866 | 863 |

#### Search, as indexed (not merged)

| query | measure | roll | Elasticsearch | OpenSearch |
|---|---|---:|---:|---:|
| knn | p50 ms | 21.7 (19.0–25.8) | 12.9 (8.4–13.6) | 15.2 (14.2–20.2) |
| knn | p99 ms | 27.8 (25.8–72.5) | 26.9 (21.5–48.3) | 25.7 (23.1–53.7) |
| knn | CPU ms/query | 19.78 (18.32–20.20) | 16.80 (11.54–28.25) | 21.52 (20.19–30.24) |
| knn | recall@10 | 0.988 | 0.843 | 0.988 |
| knn_f10 | p50 ms | 67.4 (62.0–74.3) | 13.4 (11.9–13.9) | 14.5 (13.4–17.2) |
| knn_f10 | p99 ms | 104.0 (77.4–104.7) | 26.2 (24.2–26.2) | 25.8 (20.6–44.0) |
| knn_f10 | CPU ms/query | 61.60 (58.68–68.38) | 16.14 (16.09–34.25) | 16.39 (13.98–33.45) |
| knn_f10 | recall@10 | 1.000 | 0.927 | 0.983 |
| knn_f1 | p50 ms | 41.2 (37.5–45.2) | 13.9 (13.2–15.9) | 102.2 (95.9–102.7) |
| knn_f1 | p99 ms | 62.1 (49.1–65.0) | 31.4 (27.4–46.3) | 249.3 (219.9–497.6) |
| knn_f1 | CPU ms/query | 37.48 (34.53–41.42) | 19.58 (19.13–35.25) | 33.58 (32.94–33.92) |
| knn_f1 | recall@10 | 0.910 | 0.772 | 1.000 |
| rrf | p50 ms | 21.0 (20.7–26.2) | 14.6 (12.3–17.5) | 21.8 (18.5–26.2) |
| rrf | p99 ms | 29.2 (28.1–40.0) | 25.4 (23.4–35.7) | 39.7 (29.6–51.2) |
| rrf | CPU ms/query | 19.36 (19.28–22.07) | 20.71 (18.80–41.89) | 24.80 (22.33–29.31) |

#### Search, force-merged to one segment

| query | measure | roll | Elasticsearch | OpenSearch |
|---|---|---:|---:|---:|
| knn | p50 ms | 2.5 (2.0–3.9) | 9.6 (4.4–10.9) | 4.5 (4.3–4.9) |
| knn | p99 ms | 4.8 (4.6–16.1) | 31.5 (13.4–51.2) | 10.5 (8.6–11.6) |
| knn | CPU ms/query | 1.89 (1.70–3.62) | 6.08 (5.70–7.35) | 4.94 (4.69–6.93) |
| knn | recall@10 | 0.824 | 0.757 | 0.919 |
| knn_f10 | p50 ms | 8.5 (6.6–8.8) | 5.0 (4.8–5.9) | 4.9 (4.7–5.0) |
| knn_f10 | p99 ms | 15.3 (15.2–15.8) | 14.5 (12.3–16.6) | 9.3 (8.0–13.8) |
| knn_f10 | CPU ms/query | 7.28 (6.79–8.10) | 5.29 (5.13–8.36) | 5.49 (4.57–8.28) |
| knn_f10 | recall@10 | 0.855 | 0.757 | 0.922 |
| knn_f1 | p50 ms | 9.2 (9.2–9.8) | 7.3 (7.0–8.3) | 5.3 (5.0–5.5) |
| knn_f1 | p99 ms | 16.6 (13.6–17.9) | 14.9 (12.0–15.7) | 15.1 (11.5–356.8) |
| knn_f1 | CPU ms/query | 8.11 (7.72–8.63) | 8.37 (6.59–10.62) | 6.73 (6.62–16.03) |
| knn_f1 | recall@10 | 0.686 | 0.788 | 0.806 |
| rrf | p50 ms | 3.8 (3.6–4.0) | 6.8 (6.5–6.9) | 6.0 (5.9–6.3) |
| rrf | p99 ms | 7.4 (7.1–9.6) | 14.0 (11.3–20.1) | 13.6 (11.9–13.8) |
| rrf | CPU ms/query | 3.18 (3.09–3.34) | 13.75 (9.62–16.22) | 8.25 (6.72–8.60) |

### `int8_hnsw` (OpenSearch: Lucene HNSW, `sq` 7 bits)
#### Indexing and footprint

| measure | roll | Elasticsearch | OpenSearch |
|---|---:|---:|---:|
| docs/s, 8 clients | 5,286 | 3,662 | 2,076 |
| server CPU µs/doc (load) | 718 | 1,072 | 1,891 |
| merges settling after the load (s) | 20 | 51 | 51 |
| segments after settling | 64 | 47 | 50 |
| peak RSS during ingest (MiB) | 2,918 | 3,953 | 5,766 |
| RSS after ingest (MiB) | 182 † | 2,843 | 2,522 |
| RSS after the search rounds (MiB) | 102 † | 3,050 | 2,968 |
| store bytes, not merged | 2,984,768,319 | 3,276,405,727 | 3,085,448,743 |
| force-merge (s) | 137 | 256 | 445 |
| peak RSS incl. force-merge (MiB) | 4,709 | 4,533 | 5,766 |
| store bytes, force-merged | 2,998,686,635 | 2,047,669,403 | 2,032,157,748 |
| RSS after merged search rounds (MiB) | 84 † | 3,015 | 2,926 |
| JVM heap used after search rounds (MiB) | – | 1,093 | 957 |

#### Search, as indexed (not merged)

| query | measure | roll | Elasticsearch | OpenSearch |
|---|---|---:|---:|---:|
| knn | p50 ms | 6.6 (5.4–24.2) | 6.9 (6.9–15.0) | 7.3 (7.2–19.2) |
| knn | p99 ms | 12.5 (9.1–54.9) | 13.1 (9.0–33.7) | 16.2 (11.8–193.6) |
| knn | CPU ms/query | 5.63 (4.95–79.76) | 17.49 (13.12–39.45) | 16.15 (14.28–28.90) |
| knn | recall@10 | 0.993 | 0.959 | 0.259 |
| knn_f10 | p50 ms | 23.6 (19.6–81.5) | 8.2 (8.1–10.9) | 15.7 (14.1–23.6) |
| knn_f10 | p99 ms | 39.2 (34.6–176.9) | 17.9 (15.8–29.1) | 30.9 (30.5–87.4) |
| knn_f10 | CPU ms/query | 22.93 (21.47–295.91) | 21.05 (19.41–37.36) | 46.02 (41.88–66.75) |
| knn_f10 | recall@10 | 0.998 | 0.951 | 0.466 |
| knn_f1 | p50 ms | 24.4 (23.9–37.4) | 9.3 (9.3–12.3) | 11.3 (10.3–12.8) |
| knn_f1 | p99 ms | 47.7 (43.2–80.2) | 20.4 (15.1–20.4) | 20.1 (19.3–24.4) |
| knn_f1 | CPU ms/query | 26.84 (26.39–112.84) | 21.64 (18.00–36.94) | 23.35 (22.86–28.91) |
| knn_f1 | recall@10 | 0.946 | 0.823 | 0.774 |
| rrf | p50 ms | 7.0 (6.2–24.9) | 9.8 (9.7–12.8) | 9.6 (9.1–10.4) |
| rrf | p99 ms | 11.4 (11.3–51.6) | 27.7 (17.8–37.3) | 14.9 (14.6–22.1) |
| rrf | CPU ms/query | 6.40 (5.98–62.00) | 22.82 (20.29–43.99) | 18.89 (18.68–25.98) |

#### Search, force-merged to one segment

| query | measure | roll | Elasticsearch | OpenSearch |
|---|---|---:|---:|---:|
| knn | p50 ms | 2.5 (2.4–3.7) | 5.3 (4.7–5.6) | 4.9 (4.9–9.3) |
| knn | p99 ms | 4.9 (4.6–66.8) | 8.7 (8.2–9.7) | 10.9 (8.2–21.9) |
| knn | CPU ms/query | 2.04 (1.85–3.71) | 6.27 (5.36–8.04) | 5.34 (4.86–7.12) |
| knn | recall@10 | 0.942 | 0.924 | 0.719 |
| knn_f10 | p50 ms | 5.8 (5.6–5.8) | 5.5 (5.5–5.8) | 8.6 (8.2–9.1) |
| knn_f10 | p99 ms | 13.7 (12.4–13.9) | 12.6 (10.9–14.5) | 16.1 (15.9–19.3) |
| knn_f10 | CPU ms/query | 5.56 (5.50–5.77) | 6.70 (5.04–8.58) | 11.25 (10.01–11.64) |
| knn_f10 | recall@10 | 0.908 | 0.871 | 0.829 |
| knn_f1 | p50 ms | 6.0 (5.9–6.1) | 8.3 (8.2–8.5) | 22.8 (22.5–23.2) |
| knn_f1 | p99 ms | 14.2 (12.5–15.2) | 12.1 (11.4–12.7) | 40.7 (40.0–66.9) |
| knn_f1 | CPU ms/query | 6.03 (5.68–6.03) | 8.97 (8.88–10.76) | 24.88 (22.57–29.21) |
| knn_f1 | recall@10 | 0.822 | 0.874 | 0.920 |
| rrf | p50 ms | 3.3 (3.0–3.6) | 7.3 (7.1–7.8) | 6.5 (6.1–7.3) |
| rrf | p99 ms | 7.0 (6.2–7.0) | 13.2 (12.8–13.4) | 12.0 (10.7–17.4) |
| rrf | CPU ms/query | 2.77 (2.72–3.02) | 14.11 (10.49–14.15) | 8.79 (8.04–9.38) |

OpenSearch's Lucene-engine rows (recall 0.26–0.47 before the merge) look like it took `k` as the
candidate count and ignored `method_parameters.ef_search`; they are as measured.

### `hnsw` (float32 everywhere)

#### Indexing and footprint

| measure | roll | Elasticsearch | OpenSearch |
|---|---:|---:|---:|
| docs/s, 8 clients | 3,104 | 3,312 | 3,007 |
| server CPU µs/doc (load) | 1,250 | 1,174 | 1,074 |
| merges settling after the load (s) | 40 | 97 | 173 |
| segments after settling | 60 | 30 | 46 |
| peak RSS during ingest (MiB) | 4,709 | 4,533 | 5,766 |
| RSS after ingest (MiB) | 427 † | 2,971 | 3,358 |
| RSS after the search rounds (MiB) | 55 † | 3,612 | 6,031 |
| store bytes, not merged | 2,593,431,615 | 1,635,510,662 | 3,275,795,512 |
| force-merge (s) | 667 | 463 | 612 |
| peak RSS incl. force-merge (MiB) | 5,288 | 5,319 | 6,045 |
| store bytes, force-merged | 2,610,193,618 | 1,647,653,338 | 3,276,890,045 |
| RSS after merged search rounds (MiB) | 38 † | 3,797 | 5,385 |
| JVM heap used after search rounds (MiB) | – | 1,196 | 296 |

#### Search, as indexed (not merged)

| query | measure | roll | Elasticsearch | OpenSearch |
|---|---|---:|---:|---:|
| knn | p50 ms | 9.2 (9.2–48.4) | 6.0 (5.5–34.4) | 10.6 (10.2–11.5) |
| knn | p99 ms | 16.9 (15.7–101.4) | 14.0 (11.4–107.3) | 17.5 (16.8–23.8) |
| knn | CPU ms/query | 9.35 (8.95–198.88) | 9.51 (9.34–34.02) | 17.74 (17.60–22.36) |
| knn | recall@10 | 1.000 | 0.986 | 0.991 |
| knn_f10 | p50 ms | 28.3 (27.0–93.1) | 7.6 (7.6–11.8) | 15.6 (14.4–16.2) |
| knn_f10 | p99 ms | 44.9 (41.0–197.3) | 19.0 (15.6–280.6) | 27.6 (22.9–28.7) |
| knn_f10 | CPU ms/query | 29.18 (27.59–389.94) | 14.57 (12.85–24.11) | 18.87 (15.18–22.48) |
| knn_f10 | recall@10 | 1.000 | 0.961 | 0.988 |
| knn_f1 | p50 ms | 29.9 (29.8–49.5) | 9.3 (8.1–15.7) | 47.0 (40.2–51.6) |
| knn_f1 | p99 ms | 49.1 (49.0–99.9) | 18.7 (14.6–49.4) | 106.2 (94.7–181.0) |
| knn_f1 | CPU ms/query | 31.75 (31.73–167.63) | 18.96 (16.48–24.35) | 17.36 (16.57–18.39) |
| knn_f1 | recall@10 | 0.960 | 0.796 | 1.000 |
| rrf | p50 ms | 9.8 (9.7–39.5) | 8.5 (7.9–10.1) | 13.8 (12.4–13.9) |
| rrf | p99 ms | 16.8 (16.0–100.8) | 21.1 (19.9–28.6) | 24.2 (23.1–25.6) |
| rrf | CPU ms/query | 9.53 (9.49–151.34) | 16.72 (15.61–18.28) | 22.57 (18.88–25.18) |

#### Search, force-merged to one segment

| query | measure | roll | Elasticsearch | OpenSearch |
|---|---|---:|---:|---:|
| knn | p50 ms | 3.0 (2.8–3.9) | 5.1 (4.2–78.2) | 3.7 (3.6–4.5) |
| knn | p99 ms | 7.7 (5.2–11.1) | 157.8 (130.8–206.4) | 8.0 (6.9–9.4) |
| knn | CPU ms/query | 2.44 (2.36–3.19) | 9.35 (4.98–16.88) | 3.52 (3.50–4.14) |
| knn | recall@10 | 0.953 | 0.939 | 0.948 |
| knn_f10 | p50 ms | 8.0 (7.2–8.8) | 4.9 (4.9–19.7) | 4.2 (4.1–4.2) |
| knn_f10 | p99 ms | 13.8 (13.6–17.7) | 12.3 (8.0–49.3) | 8.0 (7.6–9.9) |
| knn_f10 | CPU ms/query | 7.08 (6.99–7.79) | 4.12 (3.97–11.28) | 3.83 (3.23–5.08) |
| knn_f10 | recall@10 | 0.942 | 0.826 | 0.940 |
| knn_f1 | p50 ms | 8.1 (6.7–8.9) | 7.9 (7.9–10.2) | 4.8 (4.5–5.2) |
| knn_f1 | p99 ms | 14.8 (14.1–20.8) | 33.4 (12.7–376.9) | 12.4 (10.6–511.7) |
| knn_f1 | CPU ms/query | 7.61 (7.23–7.92) | 9.03 (8.52–16.56) | 6.33 (6.11–15.76) |
| knn_f1 | recall@10 | 0.851 | 0.897 | 0.831 |
| rrf | p50 ms | 3.8 (3.8–4.1) | 6.9 (6.3–7.0) | 5.6 (5.3–6.4) |
| rrf | p99 ms | 7.9 (7.2–8.7) | 10.9 (9.9–12.3) | 9.7 (8.9–13.0) |
| rrf | CPU ms/query | 3.33 (3.29–3.47) | 8.38 (8.21–8.64) | 5.25 (4.74–5.47) |

### Cold start

The default (`bbq_hnsw`) index force-merged. roll: the container restarted on the same data
directory with `--idle-unload-ms 2000`; each sample waits 4 s (the cluster unloads), then times
the first `knn` (the wake included) and a second one, `curl` wall time from the host, five
samples each with the data directory in the page cache and with it dropped first
(`posix_fadvise(DONTNEED)` on every file). Elasticsearch and OpenSearch cannot unload an index;
their cold start is a node restart until the first `knn` answers (two samples each).

| | roll, page cache warm | roll, page cache dropped | Elasticsearch (node restart) | OpenSearch (node restart) |
|---|---:|---:|---:|---:|
| first `knn` answer (ms) | 36 (22–194) | 213 (190–229) | 29,214 (28,962–29,214) | 25,133 (24,416–25,133) |
| second `knn` (ms) | 19 (11–106) | 99 (88–107) | 84–204 | 34–36 |
| RSS before the query (MiB) | 34 (unloaded) | 34 (unloaded) | – | – |
| RSS after both queries (MiB) | 118 | 119 | 2,520 (heap 216) | 4,134 (heap 248) |

A woken roll cluster maps its segments and faults in only what the query reads: 118 MiB after
two queries over a 2.7 GB index. With the page cache dropped the first query reads from the
(shared, busy) RAID: about 180 ms of random 4 KiB faults in the codes, the graph and the 100
float vectors it rescores, which is still two orders of magnitude below a node start.

### Footprint, in short

- **RSS.** Loaded and searched, roll's process holds 1.7–2.2 GiB in the default round, almost
  all of it file pages of its mapped segments (a woken node that has answered queries holds 13
  MiB of anonymous memory; right after a bulk load, segments not yet flushed are on the heap); Elasticsearch 2.6–3.1 GiB and OpenSearch 3.8–6.0 GiB, with 0.9–1.6 GiB of heap
  in use. Idle, roll unloads: 34 MiB. A force-merge of the 1M vectors peaks at 880 MiB of
  anonymous memory in roll (`RssAnon` sampled every 0.5 s on a native instance; the 5.3 GiB
  `VmHWM` above is mostly the mapped inputs, the spilled raw vectors and the output in the
  page cache).
- **Store.** In these rounds roll's index was 2.66–2.67 GB against Elasticsearch's 1.70 GB and
  OpenSearch's 3.28 GB (default config). The difference to Elasticsearch was the vectors kept
  in roll's stored `_source`: Elasticsearch 9 takes them out of `_source` at index time
  (`index.mapping.exclude_source_vectors`) and rebuilds them from the vector field when asked;
  roll stored them and hid them at fetch. They cost 967 bytes per document zstd-compressed
  (20,000 documents in 16 KiB chunks: 980 bytes per document with the vector, 13 without),
  0.97 GB here. roll now does as Elasticsearch does: 1.74 GB force-merged ("Storing vectors
  once" below). In object storage (the data directory's `objects/`) the bytes are the same plus
  segments that merges replaced, which the collector keeps for its one-hour grace period.

### What changed

| change | where | before | after |
|---|---|---:|---:|
| filtered k-NN reads its filter as a bitset over ordinals (`VectorsReader::ord_filter`): the filter's cardinality is a popcount of its words (was a cursor over every vector of the segment), and the graph search tests it inline (was a `dyn` closure per neighbor mapping the ordinal to a doc and checking live docs and the filter); bridges' adjacency rows are prefetched while the candidate's own neighbors are sorted out, and queued in a plain vector (was a `VecDeque`, whose `push_back` didn't inline). Hits and scores identical (all 600 responses compared byte for byte) | `search-codec` `vectors/mod.rs`, `vectors/hnsw.rs`; `search-index` `query/knn.rs` | 24 segments, server CPU per query, in two steps: the bitset, 10% filter 33.4 (32.6–33.6) ms and 1% filter 36.2 (34.6–37.1); then the prefetch and the queue, 23.8 (23.7–25.6) and 27.7 (27.7–30.1). One segment, release builds of the start and the end: 9.0 (8.7–12.7), 9.2 (9.1–9.7) | 26.2 (25.9–27.6) and 30.2 (30.1–31.3); then 22.9 (22.1–23.7) and 25.4 (25.2–26.8). One segment: 3.4 (3.4–3.6), 4.5 (4.4–4.6) |

The unmerged numbers are interleaved A/B runs of two builds on a native `search-dev` over the
same data directory (CPUs `0-5,18-23`, measured from `/proc/<pid>/stat`), so they are lower
than the Docker rows above; plain `knn` stayed at 2.0 ms on one segment.

Rerun in the Docker harness with `f00ed2095` (roll only, a fresh index of the same corpus, so
the graph differs: HNSW construction runs on several threads and isn't deterministic, and recall
moves both ways with it). Server CPU ms per query, median (min–max) of 3 rounds; Elasticsearch's
column repeats the default round above:

| query | segments | roll `49178a66c` | roll `f00ed2095` | Elasticsearch |
|---|---|---:|---:|---:|
| knn | 18 / 17 / 13 | 19.78 (18.32–20.20) | 16.59 (14.77–17.87) | 16.80 (11.54–28.25) |
| knn_f10 | 18 / 17 / 13 | 61.60 (58.68–68.38) | 41.13 (40.74–44.41) | 16.14 (16.09–34.25) |
| knn_f1 | 18 / 17 / 13 | 37.48 (34.53–41.42) | 24.36 (22.48–24.47) | 19.58 (19.13–35.25) |
| knn | 1 | 1.89 (1.70–3.62) | 1.83 (1.83–2.47) | 6.08 (5.70–7.35) |
| knn_f10 | 1 | 7.28 (6.79–8.10) | 2.98 (2.80–3.00) | 5.29 (5.13–8.36) |
| knn_f1 | 1 | 8.11 (7.72–8.63) | 4.03 (4.02–4.18) | 8.37 (6.59–10.62) |

(Recall@10 of the rerun: 0.995 / 0.998 / 0.888 not merged, 0.866 / 0.880 / 0.720 merged. Its
8-client load ran at 4,162 docs/s and 898 µs/doc against 4,735 and 787 in the first round; the
change touches no indexing code, and the lane's host load drifted between the two.)

### Storing vectors once (`_source` without them)

roll now stores `_source` without the values of indexed `dense_vector` fields outside `nested`
objects when `index.mapping.exclude_source_vectors` is true (Elasticsearch 9's default), and
patches them back from the vector field for the readers that need them, as Elasticsearch does
(`search-es` `engine/source_vectors.rs`): gets (realtime from the indexing buffer too),
`_update`, update by query, reindex, hits that show vectors or read them for `fields`, script
fields' `params._source`, `top_hits` and `inner_hits`, the knn `lookup` builder, and the
copies resize and snapshot restore make. Values print as Elasticsearch's: at the end of their
object (an array of objects: in its first object), a `cosine` field's floats normalized and
multiplied back by their magnitude, `bfloat16` rounded, `sparse_vector` features sorted and as
their postings keep them (checked against Elasticsearch 9.5.5 on 1,400 random cosine vectors of
3 to 384 dims, all identical; `tests/fixtures/docs/source_vectors.json`). Documents written
before keep their vectors in the stored source and read as before.

The same harness as the rounds above (one `search-dev` container on `10,11,28,29`, 6 GiB,
clients on `12,13,30,31`), roll only, default config: release builds of `370d29a04` (before)
and the same tree with only this change (after), each a fresh 1M load at 8 clients, settled,
force-merged to one segment. Fetch: `knn` (k 10, `num_candidates` 100) on the merged index,
200 queries after 30 warm-up queries, 3 rounds alternating the two builds; server CPU ms per
query, min–max:

| measure | before | after |
|---|---:|---:|
| store bytes, not merged | 2,670,866,087 | 1,724,865,995 |
| store bytes, force-merged | 2,673,566,106 | 1,740,417,321 (Elasticsearch: 1,709,711,301) |
| data directory (with objects merges replaced) | 13.3 GB | 10.1 GB |
| 1M docs at 8 clients: docs/s, CPU µs/doc | 4,026, 926.5 | 5,496, 703.2 |
| 200k docs at 8 clients, 2 runs: CPU µs/doc | 657.3, 642.3 | 660.9, 544.0 |
| force-merge: s, CPU s | 112, 213 | 105, 311 |
| fetch, `_source: false` | 2.04–2.34 | 2.13–2.70 |
| fetch, `_source` (vectors left out) | 7.42–8.70 | 1.92–2.11 |
| fetch, `exclude_vectors: false` (vectors shown) | 12.21–15.60 | 8.48–8.98 |

Indexing costs no more (a stored source a third the size, less to compress and merge; the
1M load was faster, the 200k runs overlap). Fetching `_source` got cheaper both ways: with
vectors hidden there are no 384 numbers to parse and drop per hit, and with vectors shown each
float is converted to its printed form once (the first version of the change converted it three
times and cost 12.5–16.9 ms there against 9.3–10.0 before; fixed before landing). The host was
busier during the fetch rounds than in the earlier ones, so compare the columns, not the rows
above. Recall@10 was 0.825 before and 0.863 after on the merged index: HNSW construction isn't
deterministic, the graph differs between loads.

### Measured, not adopted

| idea | result |
|---|---|
| Lucene's own filtered-search bookkeeping: mark every rejected neighbor visited when first seen and cap two-hop exploration at `m · 2 · multiplier` (roll explores up to twice that and lets a rejected node be reached again) | 24 segments: 10% filter 35 → 19.5 ms CPU, 1% filter 37.5 → 30.5, but recall 0.995 → 0.970 and 0.874 → 0.803 (23% fewer vectors scored). Still above Elasticsearch's 0.927 and 0.772, but lower recall at the same settings: not adopted |
| `MADV_RANDOM` on every mapped segment file (no readahead on faults) | page cache dropped, one segment: first `knn` after a wake 274–359 ms against 296–326 (no change), file RSS after three queries 55 MiB against 124, but a cold `match` query 157–197 ms against 85–137: not adopted. Advising only the vector sections would keep the RSS half without hurting text queries; it isn't worth it while RSS is page cache the kernel reclaims |
| rescore only the best `k · oversample` (30) of each segment's beam with floats, as Elasticsearch does (roll rescores the whole beam of 100) | one segment: CPU 4.2 → 2.1 ms, file RSS after 400 queries 1.15 → 0.44 GiB, recall 0.856 → 0.427 (Elasticsearch: 0.757 with 30 rescored). roll's one-bit estimates rank worse than Elasticsearch's optimized scalar quantization, so it needs the wider rescore |

### Where roll still loses

- **Store bytes**: no longer: 1.74 GB against Elasticsearch's 1.71 since `_source` is stored
  without its vectors ("Storing vectors once"). Still kept in `_source`: vectors inside `nested`
  objects and `sparse_vector` values (Elasticsearch rebuilds those from the nested documents and
  the feature postings).
- **Float HNSW indexing**: 1,250 µs of CPU per document against Elasticsearch's 1,174 (3,104 vs
  3,312 docs/s); with quantized graphs roll is ahead (bbq 787 vs 1,088, int8 718 vs 1,072).
  The float distance kernels are AVX2; this Xeon has AVX-512, which Elasticsearch's Panama
  kernels use.
- **BBQ estimate quality**: see the rescore row above. A better one-bit quantizer (per-vector
  optimized intervals, as Elasticsearch's) would let roll steer with fewer float reads.
- **Filtered latency on many segments**: roll searches segments one after another, so with
  18–64 segments its filtered p50 is 2–3× Elasticsearch's (which searches segments in
  parallel) even where CPU per query is close; on one segment, with the change above, roll's
  filtered `bbq_hnsw` search costs less CPU than Elasticsearch's.

## Tuning log

Each change keeps results identical (unit/oracle tests, both YAML suites with 0 regressions).
Numbers: geonames subset (1.1M docs, 5 shards, force-merged), server CPU time per request
(wall time where noted), old and new binary loaded alike and measured interleaved.

| change | where | before | after |
|---|---|---:|---:|
| Painless doc-value accessors build their Java stack-trace frames only when they fail (they were `format!`ted on every `doc[...]` read) | `search-painless` `es/doc.rs` | `painless_dynamic` wall 659 ms, `painless_static` 640 ms | 397 ms, 464 ms |
| `def` member reads (`doc.location.lon`) cache the resolved getter per (class, name) instead of formatting `get<Name>` and hashing two lookups per document | `search-painless` `dynamic.rs`, `vm.rs` | `painless_dynamic` 1,836 ms, `painless_static` 2,430 ms | 1,318 ms, 1,697 ms |
| `terms` (TermInSet) lookups walk the dictionary with one forward cursor (`TermsReader::sorted_lookup`): sorted terms reuse the block, its header and the scan position instead of a floor search and a block rescan per term | `search-codec` `terms.rs`, `search-index` `query/multiterm.rs` | `large_terms` (45,586 terms) 930 ms (wall 290) | 265 ms (wall 115) |
| scroll continuation pages reuse the first page's exact total (Elasticsearch's `ScrollContext.totalHits`) instead of counting every match again, so collection stops once the page is full | `search-es` `search/contexts.rs` | `scroll` (25 pages of 1,000) wall 1,730 ms | 960 ms |
| the fetch phase keeps one stored-fields reader per segment: each hit used to open a fresh reader and decompress its whole 16 KiB chunk (up to 128 docs) again | `search-index` `SegmentCore::stored_document` | (with the above) zstd was 22% of scroll CPU | 1.4% |
| `_source` moves into the hit instead of being cloned when no `fields` option reads it again | `search-es` `search/hits.rs` | `Value::clone` 6% of scroll CPU | 0 |
| one-numeric-key sorts with `search_after` take the fast path too: a doc worse than the queue's bottom costs one value read and one comparison | `search-index` `collector/top_field.rs` | `asc_sort_with_after_geonameid` 14.8 ms, `desc_…` 33 ms | 12.1 ms, 30 ms |
| merges BP-reorder only segments of at least 65,536 docs (Lucene's `BPReorderingMergePolicy` default) instead of 16,384: while indexing, BP was 42% of merge CPU and merges 31% of all CPU, every merge level reordering the same docs again | `search-index` `shard/mod.rs` | see Rally `index-append` | |
| `_source` stored without indexed vectors (`index.mapping.exclude_source_vectors`), patched back from the vector field where a reader needs them; a hit converts each shown float once (vector search corpus, 1M × 384, "Storing vectors once") | `search-es` `engine/source_vectors.rs`, `search/hits.rs`, `write.rs`; `search-index` `Shard::get_with_vectors` | store 2.67 GB force-merged; fetch CPU 7.4–8.7 ms (vectors hidden), 12.2–15.6 (shown) | 1.74 GB; 1.9–2.1 ms, 8.5–9.0 |


Round 2 of this lane (geonames subset as above, roll pinned to the 8 hardware threads
`14-17,32-35` with an Elasticsearch 9.5.5 container on the same CPUs; old and new binary
loaded alike, measured interleaved; server CPU time per request, wall time where noted;
`r4h` is the tree at the start of the round):

| change | where | before | after |
|---|---|---:|---:|
| `function_score` and `script_score` score a block of the inner query's matches at a time: the inner scorer fills a batch, each function decodes the block's doc values in one batch (a contiguous run of a dense field decodes sequentially), the score mode and boost mode are chosen once per block, and logarithm modifiers reuse the previous doc's result for an equal input; a doc whose function fails is handed to the collector with a lazy score, so the per-doc path raises the exact error | `search-index` `query/function_score.rs` (`FsBulkScorer`, `SsBulkScorer`, `LeafScoreScript::score_block`) | `field_value_function_score` 95 ms (wall 22), `decay_geo_gauss_function_score` 187 ms | 30 ms (wall 7–10), 100–117 ms |
| decay functions on geo points measure from a precomputed origin (its radians and sloppy cosine once per query, bit-identical) and the sloppy-math tables are interleaved (`[cos, sin]`, `[asin, d1..d4]`: one cache line per lookup) | `search-index` `util/geo.rs` (`HaversinFrom`) | (in the row above) | |
| the Painless fast path (closures over unboxed values instead of the interpreter) covers geo points (`doc['f'].lat` / `.lon`, `getLat()`, `doc.f.lon`), `((List) doc.f).get(0)` and `decayGeo*`: all three geonames Painless scripts now specialize | `search-painless` `fast.rs`, `compile.rs`, `es/score.rs` | `painless_static` 1,265 ms (wall 296), `painless_dynamic` 1,050 ms (wall 244), `decay_geo_gauss_script_score` 825 ms (wall 193) | 248 ms (wall 58), 220 ms (wall 53), 287 ms (wall 67) |
| Painless `haversin` no longer evaluates `sin(73°)` three times per distance (the asin table bounds were functions calling `libm::sin`); the origin's half of the formula is computed once per binding | `search-painless` `es/score.rs` | `sin` + `rem_pio2` 8% of `decay_geo_gauss_script_score` | 0 |
| `SegmentReader` finds a field by a direct slot table instead of a binary search over 88-byte field infos (half the cost of every per-document doc-values read in scripts); script hosts keep each literal field's reader per segment; expressions read through those slots | `search-codec` `reader.rs`, `search-es` `search/scripts.rs`, `search-painless` `expression.rs` | `field_value_script_score` 200 ms, `expression` 325 ms | 150 ms, 200–240 ms |
| the response takes the hits instead of cloning them (`render_take`); the fetch phase of a page of 64 hits or more renders on pool threads, one unit per shard (and per ~page/threads hits), outcomes merged in page order so failures and skipped shards are exactly the serial loop's; each segment keeps a pool of stored-fields readers and hands out the one whose decompressed chunk holds the doc; doubles in `[1e-3, 1e7)` render without allocating | `search-es` `search/mod.rs`, `search/contexts.rs`, `json/write.rs`; `search-index` `SegmentCore::stored_document`; `search-codec` `StoredReader::holds` | scroll continuation page (1,000 hits), server side 23.6 ms; first page 15.5 ms | 11.8 ms (Elasticsearch 6.5–11 ms); 11.1 ms (Elasticsearch 11.0 ms) |
| every analyzer's pool of chain instances is sharded by thread: indexing threads took and returned a chain per analyzed value through one `Mutex` per analyzer, and an off-CPU profile of the 8-client bulk load showed 24% of all context switches queued there | `search-analysis` `analyzer.rs` (`ChainPool`) | 8-client geonames load (1.1M docs, data dir) 52.5k docs/s, 133 s CPU | 64.4k docs/s, 128 s CPU |
| boolean rewrites compare clause keys by type first and build a clause's Lucene string only against a clause of the same type; weights build their description strings only when an explanation asks; a `terms` query borrows the request's values instead of copying them per shard | `search-index` `query/boolean.rs`, `query/basic.rs`, `query/multiterm.rs`, `search/mod.rs` (`LazyDesc`); `search-es` `search/query.rs` | `large_terms` 250 ms (wall 96) | 186 ms (wall 83; Elasticsearch 120 ms wall, 330 ms CPU) |

Round 3 of this lane (same setup as round 2: geonames 1.1M docs, roll and an Elasticsearch
9.5.5 container both pinned to `14-17,32-35`, binaries measured interleaved; bulk rows are the
8-client load of all 1.1M docs in 5,000-doc bulks into a data directory, CPU per document
including the merges that finish after the load; Elasticsearch on the same load: 111–122k
docs/s, 59–71 µs/doc):

| change | where | before | after |
|---|---|---:|---:|
| bulk documents go from the request bytes straight to the parsed document (`document/fast.rs`: no intermediate JSON tree for plain documents; anything unusual falls back to the tree walk, differential tests against it), one mapping context and conversion plan per bulk chunk, conversion keeps one text buffer per document, `FieldTable` lookups hash with foldhash, the buffer keeps single-valued texts by reference, postings buffers are one array of structs and the term hash compares term bytes only on a tag match | `search-es` `document/fast.rs`, `api/bulk_ahead.rs`, `engine/convert.rs`, `engine/fields.rs`; `search-index` `buffer/` | 109–119 µs/doc | 81–88 µs/doc, 84–96k docs/s |
| BP reordering builds its forward index only from terms frequent enough to matter (`next_term_min_doc_freq` skips the rest using the input doc counts as a bound) and reads docs-only postings | `search-codec` `merge.rs`, `source.rs`, `bp.rs` | BP 42% of merge CPU | (in the row above) |
| a stored `_source` is spliced into compact `_search` / scroll output as its raw bytes (pretty, YAML, CBOR and `filter_path` responses still materialize the tree) | `search-es` `json/raw.rs`, `search/hits.rs` | scroll (25 × 1,000 hits, identity) wall 281–367 ms, CPU 590–790 ms | 147–179 ms, 310–390 ms (Elasticsearch 198–208 ms, 355–494 ms) |
| one-numeric-key sorts with `search_after` reject docs before the `after` value while the queue is still filling; the competitive iterator retries pruning a fixed number of docs after a bottom change instead of on every change | `search-index` `collector/top_field.rs` | `desc_sort_with_after_geonameid` wall 12.1 ms, `asc_…` 8.0 ms | 4.6 ms, 6.2 ms (Elasticsearch 8.0, 9.0) |
| bulk responses render every item straight into one raw JSON array instead of building a tree per item | `search-es` `write.rs` (`WriteOutcome::write_bulk_item`), `api/bulk.rs`, `json/raw.rs` | 78–84k docs/s, 88–92 µs/doc | 89–91k docs/s, 80–82 µs/doc |
| a purge governor runs mimalloc with a 100 ms purge delay while the process is busy and returns to 0 (with a forced collect on every worker) after 2.5 s / 12 s idle, so idle RSS is unchanged | `search-server` `process.rs` (`start_purge_governor`, started by both search-dev and rolld) | 89–101k docs/s, 78–84 µs/doc | 95–109k docs/s, 75–78 µs/doc; idle RSS after unload 289–343 vs 320–327 MiB |
| the Painless `decayGeo*` fast path reuses the script instance's class binding (no argument values per document) and squares as `x * x` (HotSpot's `Math.pow(x, 2)`) | `search-painless` `es/score.rs`, `fast.rs` | `decay_geo_gauss_script_score` wall 52 ms, CPU 222 ms | 43 ms, 191 ms (Elasticsearch 40, 212) |
| script scores decode the block's single-valued dense doc values in one run before evaluating each document (`DocHost::prefetch`, `ScoreLeaf::score_block`) | `search-es` `search/scripts.rs` | CPU `field_value_script_score` 148, `painless_static` 267, `expression` 218 ms | 117 (ES 114–139), 197 (ES 243–277), 147 (ES 187–204) ms |
| gzip responses (Rally's client asks for them): zlib-rs instead of miniz_oxide, level 2 (zlib-ng's level 3 is the slower `deflate_medium`; its level 2 is stock zlib's level 3 `deflate_fast`, which Netty uses: ratio 4.96 vs 5.01), and a response of 256 KiB or more is deflated in 128 KiB chunks on up to 4 pool threads (one gzip member, sync-flushed chunks, combined CRC) | `search-server` `http.rs` | scroll with gzip wall 351–503 ms, CPU 440–670 ms | 258–333 ms, 440–560 ms (Elasticsearch 300–347, 270–340) |
| exact totals (`track_total_hits: true`, every scroll's first page) over constant-score blocks count the rest of a block at once once the queue can't change; `_score` floats convert without formatting through a `String`; the raw-source splice scans strings with `memchr` | `search-index` `collector/top_docs.rs`; `search-es` `json/write.rs` (`java_float_value`), `json/raw.rs` | scroll first page 17–18 ms | 8–10 ms (Elasticsearch 9–10) |
| a sort whose first key is `_doc` ascending (Rally's `scroll` sends `sort: ["_doc"]`) stops a segment once the queue is full and the hit threshold passed, as Lucene's `DocComparator` does; with an exact total (a scroll's first page) a constant-score block past the queue's last doc is counted at once; a doc after every queued one is appended instead of binary-searched; docs before a `_doc` `search_after` are skipped on the first key | `search-index` `collector/top_field.rs` (`doc_first`) | `_doc` scroll (25 × 1,000 hits) wall 305–319 ms, CPU 1,090–1,120 ms; with gzip 493–601 ms, CPU 1,570–1,880 ms | 100–129 ms, 210–270 ms (Elasticsearch 174–176, 387–401); gzip 348–351 ms, 580–590 ms (Elasticsearch 415–420, 558–570) |
| a sort's first field with no value in a segment (absent, or unmapped with `unmapped_type`) and missing values last: once the queue is full the segment ends, as Lucene's comparators give it an empty competitive iterator (Rally `http_logs` sorts on `geoip.country_name`, which its documents don't have) | `search-index` `collector/top_field.rs` (`Pruner::absent`) | `sort_keyword_can_match_shortcut` (http_logs 4.95M docs, 10 shards) wall 30–32 ms | 3.0 ms (Elasticsearch 2.1–3.3) |
| competitive pruning starts on the doc that passes the hit threshold even when that doc sorts before `search_after` (it used to wait for the next doc that both passed the threshold and followed the bound: with a bound past every value, never) | `search-index` `collector/top_field.rs` | `asc_sort_with_after_timestamp` (bound after the newest value) wall 23.5 ms | 0.9 ms (Elasticsearch 1.8–2.9) |
| a point range over a segment where every doc has one value and most match starts from all docs and clears those outside (cells inside the range cost nothing: Lucene's inverse visitor); a constant-score scorer over a doc set ends once the collector's minimum competitive score passes its score (Lucene's `ConstantScoreScorer`), and `constant_score` forwards a sorted collector's competitive iterator | `search-index` `query/range.rs` (`InverseVisitor`), `query/basic.rs` (`SetScorer`, `ConstLeaf`), `util/bitset.rs` (`clear_range`) | `match: {status: 200}` (an integer term: a point range matching 87% of docs) wall 16.2 ms, `range` on `@timestamp` 9.1 ms | 3.0 ms (Elasticsearch 3.9), 4.5 ms (4.8) |
| sorted queue entries keep their natives with the segment they were computed in; an entry from an earlier segment is translated (one terms-dictionary lookup per keyword key) the first time a segment compares against it, and a competitive doc reuses the evicted entry's buffers, so it costs no allocation and no repeated lookups (was: every binary-search probe re-resolved the entry's keyword terms into a new vector, and every bottom change cloned the bottom's bytes). A keyword `missing` value a segment lacks gets its own slot between that segment's ordinals, so foreign values in the same gap order against it by their bytes (a correctness fix found by review) | `search-index` `collector/top_field.rs` (`Entry`, `translate`, `ORD_SCALE`) | geonames 1.1M (`r9c`, 8 pinned threads, median of 15): `name.raw` size 1000 53–71 ms, desc + exact total 46–55, `cc2.raw` missing `M` 41–44, with `search_after` 58–69, `country_code.raw` + `population` size 100 6.8–7.0 | `r10a`: 21–24, 20–29, 21–22, 22–24, 5.1–5.7 ms (identical hits) |

Round 6, query execution (perf-query lane). Collector rows: `cargo test -p search-index --release
--test bench bench_sorted_collectors -- --ignored` (300,000 docs in 3 segments, doc-values-only
fields so no competitive pruning helps; best of 7). Query rows: geonames 1.1M docs, 5 shards,
`search-dev` and a private Elasticsearch 9.5.5 container pinned to the same 8 hardware threads
(`0-3,18-21`, 4 GiB), measured interleaved on a saturated host (load 33–36 on 36 threads), so
server CPU per request is the number to compare; hits identical between the binaries (md5 of
totals, ids, scores, sort values and aggregations).

| change | where | before | after |
|---|---|---:|---:|
| the sorted queue keeps its entries in fixed slots and orders 4-byte slot numbers, so an insert moves at most `4 * n` bytes instead of 64-byte entries | `search-index` `collector/top_field.rs` | a sort whose every doc enters at the head: 438–507 ms (`size: 1000`), 79–108 ms (`size: 100`) | 69 ms, 46 ms |
| `collapse` keeps each group head's natives with its segment generation (translated once per segment, as the sorted queue does) instead of re-deriving them, with a keyword lookup and allocations, on every comparison; group keys are looked up borrowed (no allocation), an evicted group's buffers are reused, a head that improves moves by a binary search and a rotate of the range it crosses, and the maps hash with foldhash | `search-index` `collector/collapse.rs` | collapse by a 20k-value keyword, every doc a new head: 902 ms (`size: 1000`), 475 ms (`size: 100`); sorted by a keyword, `size: 1000`: 120 ms; two keys: 104 ms; geonames `collapse` on `country_code.raw` CPU 78 ms | 191 ms, 125 ms; 21–38 ms; 28 ms; 53 ms (Elasticsearch 68) |
| `cardinality` on ordinals hashes each segment's seen terms in one forward walk of the dictionary (`TermDictIter::seek_ord`) instead of decoding each term from its 16-term block's start | `search-aggs` `metrics/cardinality.rs`, `search-codec` `docvalues.rs` | geonames `cardinality` of `name.raw` CPU 305 ms (wall 71) | 94 ms (wall 22; Elasticsearch 176, wall 49) |
| `composite` whose sources all read keyword ordinals caches, per segment, the queue slot or the final rejection of each combination of ordinals; a doc with one value per source whose combination was seen costs an ordinal read per source and a table lookup (slots record the combination they are cached under, so a recycled slot is a miss) | `search-aggs` `bucket/composite.rs` (`OrdKeys`) | geonames `composite` (`country_code.raw` x `feature_class.raw`, `size: 1000`) CPU 291 ms (wall 68) | 100 ms (wall 27; Elasticsearch 153, wall 38) |
| top hits by score of a `function_score` / `script_score` query (no score bound, every match scored) search a shard in slices when the request has fewer shards than cores (up to `2 * cores / shards` slices of at least 50,000 docs); a search that runs on a pool worker (a shard of a parallel request) splits too, claiming slices with any free worker (`pool::help_each_index`) | `search-es` `search/mod.rs` (`sliced`), `search-index` `search/mod.rs`, `search/pool.rs` | geonames script scores, serial shards (`ROLL_PERF_SERIAL_SLICES=1`) | CPU unchanged (±2%), identical hits; wall 3–15% lower on the saturated host (`painless_static` 47.5 → 40.5 ms); a quiet-host run should show up to 8/5 on 5 shards and 8 threads |
| term dictionary intersections step a table DFA (`Automaton::as_dfa`) with inlined lookups instead of three virtual calls per byte | `search-codec` `automaton/`, `terms.rs` | geonames `wildcard` `*berg*` on `name.raw` CPU 117–127 ms | 84–86 ms (Elasticsearch 78–93); wall 20 vs 22 ms |
| batched doc-values reads of a sparse field rank a batch's docs in one pass (`DocSet::for_each_rank`: each block looked up once, a running count from word to word in dense blocks, a forward search in sparse ones) instead of a block lookup, a rank-table read and up to eight popcounts per doc | `search-codec` `docset.rs`, `docvalues.rs` (`batch_ranks`) | geonames `histogram` of `elevation` (sparse) with `avg` and `max` sub-aggregations CPU 60 ms (wall 19), `rank` 80% of it | 32 ms (wall 13; Elasticsearch 53, wall 16) |
| `significant_terms` on a keyword with ordinals maps each (owning bucket, segment ordinal) to its key once per segment: a seen term costs one hash probe instead of decoding the term and building and cloning its identity | `search-aggs` `bucket/significant.rs` (`SegOrds`) | geonames `significant_terms` of `feature_code.raw` under a `term` query CPU 30–32 ms (wall 12–20) | 12 ms (wall 5.2; Elasticsearch 13–18, wall 5.8–8.7) |
| a top-level `date_histogram` without sub-aggregations under a `match_all`, an `exists` or a range on its field (UTC, no offset) counts each bucket from the points tree on every segment without deletions or `_doc_count` values, collecting none of its documents (Elasticsearch's `FromDateRange` rewrite); the shard query now reaches the aggregations whenever it alone decides what is collected | `search-aggs` `filter_rewrite.rs` (`FilterRewrite::counting`), `bucket/histogram.rs`; `search-index` `query/range.rs` (`PointRangeQuery::count_points`); `search-es` `search/mod.rs` | nyc_taxis (2M docs, one shard, wall p50) `date_histogram_agg` 65.9 ms, `date_histogram_calendar_interval` 102.8, `date_histogram_fixed_interval` 98.7 | 3.4, 2.5, 2.5 ms (Elasticsearch in Rally: 11.3 ms for the first) |
| `auto_date_histogram` with one owning bucket rounds through the segment's prepared rounding array (re-prepared when it moves to a coarser rounding) with a per-slot bucket ordinal cache cleared on every rebucket, and decodes a batch's long doc values in one run | `search-aggs` `bucket/auto_histogram.rs` | nyc_taxis `autohisto_agg` 138.4 ms, `auto_date_histogram` 246.0 ms (wall p50) | 92.4, 156.2 ms |
| constant-score scorers (a point range's doc set, `constant_score` and filter-only `bool` wrappers) hand collectors that skip nothing blocks of 256 matches (`LeafCollector::collect_block`, as `match_all` did) instead of a call per doc through the multi-collector; a numeric `histogram` caches each key's bucket ordinal in a slot per key of the segment's value range (at most 4,096) instead of a hash lookup per value | `search-index` `search/mod.rs` (`Scorer::constant_score`, `DefaultBulkScorer`), `query/basic.rs`; `search-aggs` `bucket/histogram.rs` (`add_numeric`) | nyc_taxis `autohisto_agg` 90.6 ms, `auto_date_histogram` 159.2, `distance_amount_agg` 256.3, `date_histogram_calendar_interval` 2.75 (wall p50) | 64.9-74.1, 109.8, 194.3, 1.47 ms |
| a numeric `histogram` floors its keys without a libm call (baseline x86-64 has no rounding instruction; exact below 2^52, `f64::floor` otherwise), and `stats` adds a doc's single value in straight-line code (count, compensated sum, Java min and max) instead of the generic loop over a one-value slice | `search-aggs` `bucket/histogram.rs` (`floor`), `metrics/simple.rs` (`add_one`) | nyc_taxis `distance_amount_agg` 187.3 ms (wall p50) | 152.1 ms |
| the Painless fast path reads `doc['f'].value` of a numeric field straight from the block the script host decoded ahead (`Host::doc_long_at` / `doc_double_at`), not through the per-document doc-values buffer and its generation check; anything not decoded ahead (sparse fields, runtime fields, other kinds) takes the old path | `search-painless` `host.rs`, `fast.rs`; `search-es` `search/scripts.rs` (`DocHost::prefetched`) | geonames (force-merged, wall p50, loaded host) `field_value_script_score` 81.8 ms, `painless_static` 136.6, `painless_dynamic` 132.2 | 63.8, 116.3, 116.1 ms |
| ... and geo points (`doc['location'].lat` / `.lon`, `decayGeo*`'s point) the same way (`Host::doc_geo_at`) | `search-painless` `fast.rs`; `search-es` `search/scripts.rs` | geonames `painless_dynamic` 69.8 and 68.3 ms, `decay_geo_gauss_script_score` 76.9 and 83.4 (two runs with the binaries swapped between two force-merged copies; the host was loaded) | 56.8 and 61.9, 72.5 and 77.7 ms |
| `auto_date_histogram` with one owning bucket, no sub-aggregations, no `_doc_count` and one value per doc counts a value of a bucket already seen with a range check (the previous value's bucket first, then the prepared array's search) and an increment, in a loop over the decoded batch; anything else takes the general path | `search-aggs` `bucket/auto_histogram.rs`, `rounding.rs` (`PreparedRounding::holds`) | nyc_taxis `autohisto_agg` 53.6 ms, `auto_date_histogram` 86.2 (wall p50, both binaries with line tables) | 23.8, 28.2 ms |
| a segment sorted by the request's sort (an index sort: TSDB, logsdb) skips the docs sorting before `search_after` once the total hits passed their threshold: a binary search over their sort values finds the first doc after it (Lucene's competitive iterator past `track_total_hits`), so a deep page reads only its own docs; exact totals still count every match | `search-index` `collector/top_field.rs` (`skip_before`) | 1M nyc_taxis docs sorted by `dropoff_datetime` desc, force-merged, `size: 100` page 90% down with `search_after` (wall p50) | 36.8 -> 3.1 ms (exact totals: unchanged, 40.7 / 45.0) |
| a top-level `auto_date_histogram` on a `date` field without sub-aggregations, under a `match_all`, an `exists` or a range on its field (UTC), counts its buckets from the points tree when every segment can be and Elasticsearch's rounding is certain: the smallest and largest matching values come from a binary search of range counts, each rounding certainly fails (too many buckets, or the shortest span its mixed-rounding min and max could have is too long) or certainly holds, and only the rounding where it stops is counted (`target × maxInner` ranges or so); anything uncertain is collected. 1,500 random requests (1–3 segments, 1–2 shards, spans from seconds to centuries, `minimum_interval`) answer the same counted and collected | `search-aggs` `bucket/auto_histogram.rs` (`count_from_points`), `filter_rewrite.rs` (`count_ranges`, `extreme`) | nyc_taxis 2M docs, wall p50 of 60 interleaved: `autohisto_agg` 27.8 ms, `auto_date_histogram` 27.4 (23.9 and 34.4 in a second run) | 3.0 and 3.3 ms (2.9 and 3.2) |
| the direct document parser (`document::fast`, bytes to fields without the JSON tree, the dynamic-mapping pass and the token walker) takes `scaled_float`, `float` and `half_float` fields too, converting as the general path does (Java's `Math.round` for scaled values, a double narrowed to a float, half precision): nyc_taxis' 8 `scaled_float` fields had sent every one of its documents down the general path. Agreement tests on edge values (ties, past a long, past a float or a half float) | `search-es` `document/fast.rs`, `document/values.rs` (`scale`) | nyc_taxis 1M docs, 8 clients, server on 4 hardware threads (3 rounds, alternating): 62.3–62.8k docs/s, 63.2–64.0 µs CPU per doc | 89.5–96.7k docs/s, 40.6–41.8 µs (aggregations over every numeric, date and point field identical) |
| ... and in the opensearch dialect, which had never taken it: the agreement tests run under both dialects (the opensearch general path's field names, points and dates are the reference there) | `search-es` `document/fast.rs` | nyc_taxis on an opensearch-dialect node, as above: 63.2–65.0k docs/s, 60.3–62.8 µs CPU per doc | 96.3–102.1k docs/s, 38.2–39.7 µs (peak RSS while loading 1.1–1.3 GiB against 1.0–1.2; aggregations identical) |
| ... and the fields of plain objects (not nested, enabled, with sub-objects), walked in document order with their full names: a document with any object (`host.name`, `http.response.status_code`: ECS logs, Beats and Elastic Agent data) had gone down the general path whole. 200,000 random documents with one- and two-level objects agree with the general path in both dialects | `search-es` `document/fast.rs` (`Scope`, `FastPlan::object`) | a synthetic ECS access log (`scripts/bench-search-ecs.py`: 6 objects up to 3 levels deep, 13 leaves), 1M docs, 8 clients, as above: 46.4–50.6k docs/s, 79.5–85.7 µs CPU per doc | 77.9–89.0k docs/s, 43.9–50.5 µs (aggregations identical) |
| ... and arrays of plain objects (inner arrays flatten; each object counts toward `index.mapping.array_objects.limit`, past which the general path fails the document; a `null` or a value in the array stays with the general path): 200,000 random documents with arrays of one- and two-level objects agree in both dialects | `search-es` `document/fast.rs` (`FastPlan::object_array`) | the ECS log with 1-3 `related` objects per document (`scripts/bench-search-ecs.py --arrays`), 1M docs, 8 clients, as above: 35.4–37.2k docs/s, 107.3–112.2 µs CPU per doc | 57.3–72.7k docs/s, 53.8–64.8 µs (aggregations over the array's fields identical) |

### When to BP-reorder at merge time (2026-11-13): measured, nothing adopted

BP (`search-codec` `bp.rs`) costs a lot of indexing CPU on logs. It cost a third of the server's
CPU on the synthetic ECS access log with one client. Elasticsearch 9.5.5 never reorders regular
indices: no class in its distribution references `BPReorderingMergePolicy` or
`BPIndexReorderer`, which only `lucene-misc` ships. Each candidate policy below was measured
against today's (every inverted field, natural merges from 2^18 docs, forced from 2^16). It used
a temporary env switch in a local build, and none of them is in the tree.

The method: 1M docs into one shard; server on CPUs 10,11,28,29 and clients on 12,13,30,31;
modes alternated, with 2–3 rounds each. CPU per doc is "settled": the merges are counted until
the server has been idle for 4 s. The queries were measured on one force-merged segment, as
server CPU per request (the median of 100). Request and query caches were off. In every mode,
the totals, the aggregations and the sort values of every query were identical. Medians are
followed by the range in brackets. Raw outputs and scripts are in
`~/.cache/rsearch/bp-2026-11-13/` on the machine that ran them.

| | BP on (today) | BP off | text fields only |
|---|---:|---:|---:|
| ECS, 1 client: settled CPU µs/doc | 61.4 (55.0–63.8) | 41.5 (41.3–44.4) | 44.1 (39.7–44.4) |
| ECS: `_forcemerge` CPU s (bisection) | 9.6 (9.9) | 3.3 (0) | 4.6 (0.7) |
| ECS: store MB | 153.4 | 155.5 | 155.8 |
| ECS: range + `date_histogram` | 2.18 ms | 1.19 | 1.39 |
| ECS: `match` on `message` | 0.94 | 0.85 | 0.73 |
| ECS: **sort `@timestamp` desc, size 10** | **1.30** (1.18–1.47) | **31.2** (25.7–60.2) | **55.1** (27.9–56.7) |
| ECS: term + sort desc / range + sort desc | 1.08 / 7.51 | 2.15 / 13.7 | 2.73 / 11.1 |
| nyc_taxis, 8 clients: settled µs/doc (natural merges barely reach 2^18 at 1M docs) | 50.5 | 47.5 | |
| http_logs, 8 clients: settled µs/doc, store MB | 27.3, 77.6 | 25.6, 78.3 | |
| `so`, 8 clients: settled µs/doc | 145.7 (127.0–155.8) | 115.4 (107.0–138.9) | 119.1 (112.9–125.2) |
| `so`: `match` / `match` title / phrase on `body` | 2.11 / 2.77 / 1.48 ms | 2.39 / 2.29 / 1.07 | 1.40 / 1.95 / 1.00 |
| `so`: sort `creationDate` desc, size 10 | 0.86 | 19.9 | 1.50 |

BP's real search win on time-ordered data is a desc sort, not a smaller index or faster
postings. Docs arrive in ascending time order. With that order, `@timestamp` desc is the worst
case for the competitive iterator (Lucene's too). The first 10,000 hits it counts are the oldest
ones, so the queue's bottom rises only as the scan reaches newer docs, and almost nothing is
skipped. Scattering the docs puts recent values among the first hits, so pruning starts at once.
On ECS, the scattering comes from the keyword terms. BP over text fields only leaves the time
order mostly intact (the sort is even slower than with no BP at all). Turning BP off makes logs
24x slower on their most common query ("newest first") for 32% less indexing CPU. So off, text
fields only and forced merges only were not adopted.

Cheaper BP that keeps scattering the keywords, on ECS and `so` force-merged (2 rounds each):

| | ECS settled µs/doc | ECS sort desc | `so` settled µs/doc | `so` `match` / `match_and` / title |
|---|---:|---:|---:|---:|
| today | 59.1 | 0.84 ms | 145.5 | 1.49 / 0.64 / 1.98 ms |
| `max_doc_freq_ratio` 0.5 | 50.5 | 1.44 | 150.4 | 1.97 / 0.98 / 2.48 |
| `min_partition_size` 256 | 52.0 | 1.72 | 132.2 | 1.98 / 1.01 / 2.43 |
| `max_iters` 10 | 60.5 | 1.00 | 122.2 | 1.76 / 0.99 / 2.41 |

Each variant saves 10–15% on ECS, but costs `so`'s text queries 20–55% more CPU. Dropping terms
in more than half the docs also drops the frequent text terms. None was adopted.

What would make dropping BP on logs a win: a top-k sort that prunes well when docs are already
ordered by the sort field the other way. Such a sort would visit doc windows newest-first, or
take a threshold from the points tree, check it and widen it if too few docs match. With that
sort, BP over text fields only would save about 28% of indexing CPU on logs (ECS: 61.4 → 44.1
µs/doc). On `so`, text fields only also scored text queries better than every field did (above).
But a second session's "today" runs gave `match` 1.49 and title 1.98 ms, so that difference is
within the drift between sessions and is no reason by itself.
Built and measured on 2026-11-14: "Sorts against the doc order" below.

Measured and not adopted (2026-11-14): refreshed segments' stored fields at zstd level 1 (merges
keep level 3 and rewrite them). Stored-field compression at flush is about 11% of indexing CPU on
the ECS log, but over 6 alternating rounds (1M docs, 8 clients) the median CPU per document was
55.4 µs against 58.1 (spread ±10% per round) with segments 3-10% larger until merged: not a
measured win.

Measured and not adopted in round 6: slicing field sorts and queries with score bounds. Their
serial competitive pruning is so effective that slices only add work: on geonames
`desc_sort_geonameid` CPU 3.5 → 10.3 ms, `sort_country_code` 5.6 → 12.6 ms, a 1,000-hit
`name.raw` sort 137 → 359 ms, with longer wall times too.

Tried and reverted in round 3: an adaptive merge IO throttle modelled on Elasticsearch's merge
executor (no gain: total CPU per document unchanged); a custom open-addressing term hash with
term headers in the arena (warm lookups 38 → 59 ns, cold hits equal); a fixed 100 ms mimalloc
purge delay (+7–15% bulk throughput but 2.4x the RSS after an idle unload, which the governor
above avoids).

Earlier rounds (same lane): parallel bulk preparation with ordered per-shard apply
(`search-es` `api/bulk_ahead.rs`), query phases of a request's shards in parallel, `match_all`
top hits stopping once nothing can compete, numeric and keyword competitive pruning for sorts
(points tree / terms postings), `size: 0` counts from segment statistics, and scroll
continuation by last hit.

Vector search (2026-11-14; 1M × 384 `bbq_hnsw`, see "Vector search" for the setup; native
`search-dev` builds over the same data directory, measured interleaved, server CPU per query;
hits and scores compared byte for byte):

| change | where | before | after |
|---|---|---:|---:|
| filtered k-NN: the filter as a bitset over ordinals (cardinality by popcount, tested inline in the graph search), bridges' adjacency rows prefetched, a plain vector as the bridge queue | `search-codec` `vectors/mod.rs` (`OrdFilter`), `vectors/hnsw.rs`; `search-index` `query/knn.rs` | 24 segments: 10% filter 33.4 ms, 1% 36.2 (bitset: 26.2, 30.2); 1 segment: 9.0, 9.2 | 22.9, 25.4; 3.4, 4.5 |
| (not adopted) Lucene's visited marking and exploration cap in the filtered search | `vectors/hnsw.rs` | 10% filter 35 ms, recall 0.995; 1% 37.5 ms, 0.874 | 19.5 ms, 0.970; 30.5 ms, 0.803 |
| (not adopted) `MADV_RANDOM` on mapped segments | `search-store` `cache.rs` | cold wake, first `knn` 296–326 ms, cold `match` 85–137 ms | 274–359 ms, 157–197 ms |
| (not adopted) rescore only the best `k · oversample` per segment | `vectors/mod.rs` | 1 segment: 4.2 ms, recall 0.856 | 2.1 ms, 0.427 |

### Sorts against the doc order (2026-11-14): adopted; BP over text fields only: not adopted

Logs arrive oldest first, and their most common query sorts `@timestamp` descending (Kibana's
Discover also asks for `track_total_hits: true`). Docs in ascending order are the competitive
pruning's worst case, Lucene's `NumericComparator`'s too: every doc beats the queue's worst
entry, so the competitive docs are all those still to come and nothing is skipped. Until now BP
saved that sort by scattering recent docs among the first hits (above).

What changed (`search-index` `collector/top_field.rs` `Band`, `search/mod.rs`
`LeafCollector::next_pass`, `search-codec` `points.rs` `rank_bound`):

- The collector takes a numeric first key's values in bands. A band starts in a segment once it
  counted 256 hits while the queue still changes (four queues' worth of the hits since the last
  power of two entered it; in random order about `n ln 2` do whatever the count), or when the hit
  count passes its threshold and the pruner found no competitive set. The points tree's split
  values name an edge with about twice the queue's expected hits past it, at the hits per doc
  since the segment's first hit, and past `search_after` and any range the query requires
  (`required_range`: the range of Kibana's time picker). Only docs inside the band compare
  against the queue; the pruner skips the others once the count passes its threshold. A
  single-valued field's pruner is also bounded by the required range.
- When the pass ends with the queue not full of hits inside the band, the docs outside it that
  were counted before the threshold passed (buffered, up to 65,536) are offered to the queue,
  and the segment runs again (`search_partitions` makes a new bulk scorer) from the doc where the
  pruner began to skip, for the next band; the fifth is unbounded. Later passes count nothing,
  so totals, hits and tie order are a single pass's. Only collectors that are run alone get
  passes: aggregations, `terminate_after`, post filters, profiles and concurrent partitions keep
  one pass.
- With exact totals the first pass still counts every hit, but only the band's compare.
- Exactness: `tests/sort_pruning.rs` (`sorts_against_doc_order_equal_exhaustive_sorts`: rising
  and falling values, ties, missing first and last, long, int, float and double, filters of every
  density, ranges on the sort field, `search_after`, a second key, deletes, several segments,
  every threshold up to exact totals; `ranges_on_multi_valued_sort_fields_equal_exhaustive_sorts`)
  against one-pass collection. They fail if the rescans, the deferred docs or the multi-valued
  guard are taken out. Every query below returned the same totals, hits (`_id`), sort values,
  scores and aggregations on both binaries.

Method: 1M docs per corpus, force-merged to one segment, for each BP mode (a local switch, as in
the section above). Base (d5328596d) and new ran at once on two copies of the same data
directory (servers on CPUs 10,11,28,29; clients on 12,13,30,31), with requests alternating
between them. Each cell is server CPU per request, the median of 100, then the median of 2
rounds with the servers restarted. Caches were off. An A/A run (one binary on both copies) agreed
within 2–6%. Queries: `sort_*` (size 10 on the time field), `_tth` (`track_total_hits: true`),
`range_sort` (the middle 10% of the time span, then sorted), `after_*` (`search_after` the middle
of the span, `track_total_hits: false`), `term_sort` and friends (a term or two, then sorted by
time), plus controls without sorts. Scripts and raw results (every iteration's, `iter-*.jsonl`)
are in `~/.cache/rsearch/sort-2026-11-14/` on the machine that ran them.

| ECS log, ms: base → new | BP on | BP off | text fields only |
|---|---:|---:|---:|
| `sort_desc` | 1.44 → 1.44 | 84.88 → 1.39 | 45.54 → 1.14 |
| `sort_asc` | 1.28 → 1.29 | 3.35 → 1.21 | 1.90 → 0.79 |
| `sort_desc_tth` | 39.24 → 37.98 | 84.81 → 36.02 | 46.89 → 20.36 |
| `range_sort` | 9.87 → 10.10 | 10.54 → 1.27 | 7.42 → 1.16 |
| `after_desc` | 2.71 → 0.80 | 29.12 → 0.66 | 27.22 → 0.56 |
| `term_sort` | 1.46 → 1.49 | 4.30 → 1.66 | 3.20 → 1.30 |
| `term_sort_rare` | 1.81 → 1.82 | 5.61 → 1.93 | 5.89 → 1.94 |
| `bool_sort` | 2.20 → 2.26 | 7.20 → 3.57 | 6.82 → 3.64 |
| `sort_bytes_desc` | 1.48 → 1.49 | 1.43 → 1.49 | 1.88 → 1.97 |
| `match` | 1.02 → 1.04 | 1.11 → 1.19 | 1.31 → 1.31 |

| nyc_taxis, ms: base → new | BP on | BP off | text fields only |
|---|---:|---:|---:|
| `sort_desc` | 9.72 → 1.23 | 13.84 → 0.90 | 19.52 → 1.07 |
| `sort_asc` | 4.09 → 1.03 | 8.07 → 1.12 | 1.44 → 1.60 |
| `sort_desc_tth` | 26.44 → 28.24 | 29.11 → 21.35 | 24.34 → 27.33 |
| `range_sort` | 5.55 → 5.76 | 7.69 → 2.24 | 6.47 → 1.89 |
| `range_sort_asc` | 5.40 → 1.73 | 5.79 → 1.61 | 5.29 → 5.28 |
| `after_desc` | 4.63 → 0.81 | 1.94 → 0.91 | 10.23 → 0.76 |
| `term_sort` | 14.72 → 1.44 | 9.95 → 1.00 | 14.06 → 1.02 |
| `desc_sort_tip_amount` | 10.33 → 1.15 | 1.91 → 1.99 | 1.89 → 1.96 |
| `asc_sort_passenger_count` | 0.94 → 0.94 | 1.05 → 1.06 | 1.08 → 1.11 |
| `term` | 1.08 → 1.16 | 0.76 → 0.77 | 1.08 → 1.10 |

| http_logs, ms: base → new | BP on | BP off | text fields only |
|---|---:|---:|---:|
| `sort_desc` | 2.54 → 1.45 | 22.27 → 1.46 | 24.36 → 1.08 |
| `sort_asc` | 1.13 → 1.19 | 18.17 → 1.02 | 1.96 → 2.14 |
| `sort_desc_tth` | 26.20 → 25.94 | 29.86 → 30.56 | 28.25 → 21.12 |
| `range_sort` | 6.22 → 1.90 | 9.03 → 1.93 | 7.35 → 1.81 |
| `after_desc` | 2.50 → 0.69 | 3.57 → 1.38 | 7.96 → 0.66 |
| `term_sort` | 2.92 → 1.97 | 23.16 → 1.85 | 27.03 → 1.69 |
| `term_sort_rare` | 1.01 → 1.03 | 0.94 → 0.95 | 1.04 → 1.06 |
| `sort_size_desc` | 1.44 → 1.55 | 1.48 → 1.54 | 1.20 → 1.25 |
| `match` | 1.07 → 1.12 | 1.08 → 1.12 | 0.99 → 1.03 |

| `so`, ms: base → new | BP on | BP off | text fields only |
|---|---:|---:|---:|
| `sort_desc` | 1.43 → 1.46 | 7.08 → 2.43 | 1.49 → 1.57 |
| `sort_asc` | 1.57 → 1.70 | 22.73 → 1.07 | 1.04 → 1.07 |
| `sort_desc_tth` | 22.21 → 24.18 | 27.23 → 28.97 | 31.74 → 33.74 |
| `range_sort` | 8.07 → 9.16 | 5.91 → 1.66 | 7.33 → 7.58 |
| `after_desc` | 1.61 → 0.66 | 14.28 → 0.71 | 2.29 → 0.79 |
| `term_sort` | 2.23 → 2.35 | 2.78 → 2.26 | 1.66 → 1.76 |
| `match_sort` | 5.32 → 5.50 | 5.65 → 5.39 | 4.88 → 5.35 |
| `match` | 2.34 → 2.34 | 2.54 → 2.44 | 1.86 → 1.93 |
| `match_title` | 2.42 → 2.43 | 1.67 → 1.67 | 1.88 → 1.93 |
| `bool_text_term` | 0.94 → 0.96 | 1.30 → 1.36 | 1.20 → 1.21 |

Most of the gain is on time-ordered data, and BP-reordered indices gain too: `search_after`,
`term_sort` on nyc_taxis and http_logs, a range on http_logs. Not neutral: in BP-reordered
indices (random time order, where no band starts), `range_sort` on `so` (+13%), and exact totals
on nyc_taxis and `so` (+7–9%; their spread across rounds is about as large). Every other
difference is within the spread across rounds.

Tried and not kept (each measured the same way):

- Starting the band when the count passes its threshold: the first 10,000 hits still each enter
  the queue (ECS BP off, `sort_desc` 2.9 against 1.3 ms).
- Starting at the segment's first doc with sixteen queues' worth of points: a wider band inserts
  more hits, and every insert re-derives the pruner's set from a leaf of the points tree
  (`after_desc` 0.61 → 0.94 ms); rare queries rescanned (http_logs `status: 404` + sort 0.95 →
  1.76).
- Starting when the queue fills: a range's hits lie together, so hits per doc from doc 0 sized
  its band too wide (ECS `range_sort` +11%).
- Gating on the share of all hits that entered the queue (half), then on the last 128 hits:
  force-merged segments are runs of merged segments, not one rising sequence, and the gates
  turned bands off where they won (nyc_taxis `sort_desc` 8.85 against 1.25 ms).
- Keeping a pruner set of up to 1,024 docs instead of deriving it again on every bottom: the
  pruner then never reached the empty set when the bottom hit the smallest value
  (`asc_sort_passenger_count` 1.07 → 1.28). Kept as sampling (every 32nd bottom) inside a band
  only.
- Rescanning instead of buffering the docs counted before the threshold: rare matches were
  scored twice (http_logs `term_sort_rare` with BP on 0.89 → 1.49).
- Visiting doc windows newest first: not built. It needs a scorer per window, and a range query
  builds its points bitset for each one.

BP over text fields only (`BpOptions::fields`), with the new sort: indexing gets cheaper on the
logs (settled CPU, medians of 12 ingests per mode; `so` 4–6):

| | BP on | text fields only |
|---|---:|---:|
| ECS µs/doc, `_forcemerge` CPU | 61.0, 9.6 s | 41.5, 3.6 s |
| nyc_taxis | 51.0, 8.6 s | 45.3, 3.9 s |
| http_logs | 32.3, 4.3 s | 28.7, 3.2 s |
| `so` | 152.6, 46.0 s | 148.8, 45.5 s |

Some sorts and keyword filters lose BP's clustering, beyond the spread across ingests (ms, new
sort, BP on → text fields only): ECS `bool_sort` (two keyword filters, sorted) 2.26 → 3.64,
`sort_bytes_desc` 1.49 → 1.97, `match` 1.04 → 1.31; nyc_taxis `sort_asc` 1.03 → 1.60,
`desc_sort_tip_amount` 1.15 → 1.96; http_logs `sort_asc` 1.19 → 2.14. The time sorts themselves
gain (ECS `range_sort` 10.1 → 1.2, exact totals 38.0 → 20.4). `so`'s text queries come out about
even across sessions (`match` 2.34 → 1.93, title 2.43 → 1.93 here; the reverse in another
session). A third less indexing CPU on ECS costs those queries, and `so` saves nothing: not
adopted. Applying it to log indices only (an index setting) would be the next thing to measure.

## Fetch phase (round 6, perf-fetch lane)

Profiles (`perf record --call-graph lbr` from the privileged `roll-perf-tools` container
against a pinned `search-dev`) of the micro suite's 10-hit `match` put 46% of its CPU in zstd
(`ZSTD_decompressSequences` alone 35%, table building another 10%): every hit decompressed its
whole 16 KiB stored-fields chunk, about 64 documents of this corpus, to read one.

**Stored-fields layout.** `cargo test --release -p search-codec --lib bench_stored -- --ignored
--nocapture` with `ROLL_STORED_CORPORA` set to the first 200,000 lines of Rally's geonames and
http_logs corpora, 1,000 pmc articles and the micro suite's 50,000 documents (each stored with
its `_id`; per document: stored bytes, then the time to read a random document; busy host,
the absolute times vary by up to 2x between runs, the ratios between rows don't):

| layout | geonames (338 B) | http_logs (144 B) | micro (251 B) | pmc (38 KB) |
|---|---:|---:|---:|---:|
| zstd 3, trained dictionary, 16 KiB chunks (before) | 66.1 B, 20.9 µs | 20.0 B, 17.7 µs | 86.9 B, 33.8 µs | 13,143 B, 63 µs |
| zstd 3, trained dictionary, 4 KiB chunks | 73.4 B, 7.5 µs | 22.2 B, 5.3 µs | 90.5 B, 8.6 µs | 13,190 B, 66 µs |
| zstd 3, trained dictionary, 2 KiB chunks (merges now) | 79.5 B, 4.0 µs | 24.1 B, 2.9 µs | 92.6 B, 5.0 µs | 13,191 B, 60 µs |
| zstd 3, trained dictionary, 1 KiB chunks | 89.0 B, 2.5 µs | 27.4 B, 1.9 µs | 96.4 B, 3.4 µs | 13,192 B, 82 µs |
| zstd 3, sampled dictionary, 2 KiB chunks (refreshes now) | 79.8 B, 4.3 µs | 24.4 B, 3.4 µs | 96.7 B, 6.4 µs | (none: 14,201 B) |
| zstd 3, no dictionary, 16 KiB chunks (refreshes before) | 67.1 B, 18.8 µs | 21.7 B, 20.1 µs | 89.8 B, 25.9 µs | 14,124 B, 64 µs |
| zstd 3, no dictionary, 2 KiB chunks | 102.3 B, 6.5 µs | 36.7 B, 7.0 µs | 106.4 B, 6.9 µs | 14,201 B, 57 µs |

An earlier run of the same corpora (without `_id`) measured the alternatives the assignment
named: LZ4 (`lz4_flex` block format with the segment dictionary's content as its prefix,
decompressing whole blocks; stopping at the document's end would halve the block cost) is
ratio 3.41–3.72 on geonames at 16–4 KiB blocks (zstd 2 KiB: 4.49), 4.65–5.09 on http_logs
(6.73) and 1.67–1.71 on the micro corpus (2.83), at 3–8.6 µs, 3.9–9 µs and 7–24 µs per random
document; one zstd frame per document with the dictionary costs 0.6–0.9 µs but stores 117
(geonames) and 51 (http_logs) bytes per document, 1.6x and 2.4x the 2 KiB chunks. A
dictionary of raw sampled documents without zstd's entropy tables decodes 2 KiB chunks no
faster than no dictionary; finalized with the tables (`ZDICT_finalizeDictionary`) it compresses
within 1–4% of a trained one and takes 4–9 ms instead of 30–130 ms (training on pmc's few large
samples takes 3.7–7 s; for documents averaging over 4 KiB no sampled dictionary is built: it
was 0.4 s and a worse ratio than none). Chunks are now 2 KiB with a dictionary in every
segment that has 64 KiB of samples: trained at merges, sampled at refreshes (which had none
before). The segment format is unchanged (chunk size is a writer choice), so segments written
before read as they are and shrink to the new chunks when merged.

**Micro suite** (roll `base` = the tree before this round's changes, `c` = after; both on
`14-17,32-35`, each loaded with the same 50,000 documents, 3 rounds of 300 requests,
interleaved; server CPU per request):

| workload | before | after |
|---|---:|---:|
| match | 0.782 ms | 0.433 ms |
| match_or_2 | 0.941 ms | 0.665 ms |
| match_or_3 | 1.131 ms | 0.781 ms |
| match_and_2 | 0.721 ms | 0.447 ms |
| sort_field | 0.731 ms | 0.368 ms |
| sort_match_all | 1.005 ms | 0.701 ms |
| get | 0.146 ms | 0.106 ms |
| terms_agg, range_count, bool_filter_range (no or few hits) | 0.290, 0.203, 0.248 ms | 0.273, 0.189, 0.268 ms |
| bulk 50k docs (4 rounds) | 1.945 s CPU, 1.98 s wall | 1.997 s CPU, 1.91 s wall |
| stored fields of the loaded index | 4.49 MB | 4.85 MB (store 9.61 → 9.97 MB) |

The same match with other fetch options (server CPU per request, two runs each):
`size: 100` 4.9–5.1 → 1.57 ms, `highlight` on `body` 0.96–1.16 → 0.62–0.64 ms, `fields: [n, cat,
title]` 0.78–0.80 → 0.44–0.45 ms, `docvalue_fields` 0.74 → 0.38–0.47 ms, `_source.includes`
0.83 → 0.39–0.50 ms.

| change | where | before | after |
|---|---|---:|---:|
| 2 KiB stored-fields chunks, a sampled (finalized) dictionary for refreshed segments | `search-codec` `stored.rs` (`CHUNK_BYTES`, `build_dictionary`, `StoredOptions::train`), `writer.rs`; `search-index` `shard/mod.rs` | zstd 46% of a 10-hit `match` | 15–18% |
| the unified highlighter segments ASCII text with its own UAX #29 word and sentence rules (class tables, a run fast path, no sentence scan without a terminator; checked against `unicode-segmentation` on every string of up to 4 symbols over a 25-symbol alphabet and 100,000 random ones), builds sentence boundaries only for values with matches and word boundaries only for sentences longer than `fragment_size`, and maps UTF-16 offsets of ASCII text as the identity | `search-index` `highlight/breaks.rs`, `unified.rs` | words + sentences of a 130-char value 7.0–7.9 µs (generic segmenter) | 1.2–1.6 µs |
| `_score` floats whose shortest digits end odd test for an exact decimal tie in `u128` instead of formatting 40 more digits (`java_float` ran for about half of all scores) | `search-es` `json/write.rs` (`is_decimal_tie`) | `java_float_value` 2.6–2.9% of a 10-hit fetch with `fields` / `docvalue_fields` | (only true ties) |
| runtime-field scopes share the per-mapping field set, slot table and shadowing set across shards (they were rebuilt per shard per search: O(runtime fields) string clones), evaluators in a slot array | `search-es` `search/runtime.rs` (`ScopeFields`) | | |

## aarch64 (Graviton)

`search-codec` has NEON kernels for float dot product / L2, int8 and uint8 (`sdot`/`udot` with
the Armv8.2 `dotprod` extension, widening multiplies without it), int4, BBQ bit-plane popcounts
and bit unpacking; NEON is baseline on aarch64, `dotprod` is detected at runtime.
`scripts/search-arm-test.sh` cross-compiles and runs the tests under qemu-user: with
`QEMU_CPU=max` (dotprod) 119 of 120 `search-codec` tests pass, including
`neon_paths_match_portable`; the one failure is a wall-clock bound (`huge_repetitions_fail_fast`
must finish in 5 s; it took 5.06 s emulated). With `QEMU_CPU=cortex-a72` (no dotprod) the
distance, bit-unpacking and BBQ tests pass (13 of 13). Round 3, `QEMU_CPU=max`: every
`search-codec` test passes under emulation (lib 122 of 122, integration 8 of 8, segment 14 of
14). `search-index` under emulation (`QEMU_CPU=max`, `CPUSET` keeps the container off the
benchmark CPUs): 21 test binaries, 183 tests pass, among them the bit-unpacking, BBQ and
vector-distance paths the queries use; one durability test that depends on wall-clock timing
(`tombstones_are_accounted_and_pruned_by_deletes`: 5,000 deletes must finish inside a 500 ms
`gc_deletes` window) fails under emulation only and was skipped there. No native Graviton
numbers yet. Round 6 (`QEMU_CPU=max`, with this round's BP and points changes): `search-codec` lib 124 of 124
(the wall-clock-bound `huge_repetitions_fail_fast` skipped), `es_fixtures` 8 of 8, `segment` 14
of 14. Native Graviton numbers are still pending (no arm64 hardware here).
