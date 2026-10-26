# roll search vs Elasticsearch 9.5.5

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
  1-segment sorts. Left out: `nyc_taxis`, `pmc`, `so` and `noaa` ship as single 4.5–9.5 GB
  compressed files that Rally decompresses whole (up to 80 GB) before indexing any subset, too
  much for this shared machine; their query mix (term/range filters, sorts, aggregations) is
  covered by the two tracks above.
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

**Rally http_logs 2%** (4,948,746 documents, 10 shards per search; every operation unthrottled):

| task | metric | roll | Elasticsearch |
|---|---|---:|---:|
| index-append | docs/s | 240,090 (239,090–241,089) | 250,295 (193,413–307,177) |
| index-append | p50 ms | 137.6 (137.1–138.1) | 104.4 (99.7–109.1) |
| default | p50 ms | 2.57 (2.57–2.58) | 4.53 (3.72–5.34) |
| term | p50 ms | 2.25 (2.23–2.27) | 4.67 (4.00–5.33) |
| terms_enum | p50 ms | 1.57 (1.51–1.63) | 3.56 (3.45–3.67) |
| range | p50 ms | 1.61 (1.61–1.61) | 2.44 (2.23–2.65) |
| 200s-in-range | p50 ms | 8.09 (6.98–9.20) | 9.55 (9.29–9.81) |
| 400s-in-range | p50 ms | 2.72 (2.59–2.86) | 4.81 (4.68–4.94) |
| hourly_agg | p50 ms | 43.61 (43.54–43.69) | 67.86 (60.24–75.48) |
| scroll | p50 ms | 453.4 (453.4–453.4) | 478.7 (464.6–492.7) |
| desc_sort_timestamp | p50 ms | 7.11 (6.61–7.61) | 14.07 (12.31–15.83) |
| asc_sort_timestamp | p50 ms | 6.34 (6.15–6.52) | 10.71 (9.74–11.68) |
| desc_sort_with_after_timestamp | p50 ms | 7.40 (7.20–7.61) | 14.45 (12.24–16.66) |
| asc_sort_with_after_timestamp | p50 ms | 1.71 (1.71–1.71) | 5.45 (5.39–5.52) |
| desc_sort_timestamp_can_match_shortcut | p50 ms | 8.81 (8.63–9.00) | 13.23 (11.73–14.72) |
| desc_sort_timestamp_no_can_match_shortcut | p50 ms | 8.47 (8.34–8.61) | 15.84 (15.75–15.94) |
| sort_size_desc | p50 ms | 4.83 (4.54–5.12) | 7.53 (7.52–7.54) |
| sort_size_asc | p50 ms | 3.68 (3.55–3.81) | 6.51 (6.48–6.55) |
| sort_status_desc | p50 ms | 4.20 (4.14–4.25) | 6.83 (6.79–6.87) |
| sort_status_asc | p50 ms | 3.58 (3.53–3.63) | 6.36 (6.34–6.38) |
| sort_keyword_can_match_shortcut | p50 ms | 3.58 (3.54–3.62) | 5.85 (5.74–5.97) |
| sort_keyword_no_can_match_shortcut | p50 ms | 3.58 (3.54–3.61) | 4.89 (4.84–4.95) |
| sort_numeric_can_match_shortcut | p50 ms | 6.03 (6.03–6.04) | 8.20 (8.10–8.29) |
| sort_numeric_no_can_match_shortcut | p50 ms | 5.92 (5.84–6.00) | 7.59 (7.48–7.70) |
| desc-sort-timestamp-after-force-merge-1-seg | p50 ms | 6.52 (6.09–6.95) | 15.19 (14.64–15.73) |
| asc-sort-timestamp-after-force-merge-1-seg | p50 ms | 4.82 (4.62–5.02) | 5.92 (5.56–6.29) |
| desc-sort-with-after-timestamp-after-force-merge-1-seg | p50 ms | 7.46 (7.45–7.47) | 14.70 (13.73–15.67) |
| asc-sort-with-after-timestamp-after-force-merge-1-seg | p50 ms | 1.98 (1.94–2.01) | 4.47 (4.42–4.52) |
| sort-size-desc-after-force-merge-1-seg | p50 ms | 5.28 (5.18–5.38) | 8.08 (7.94–8.23) |
| sort-size-asc-after-force-merge-1-seg | p50 ms | 3.57 (3.52–3.62) | 5.79 (5.71–5.86) |
| sort-status-desc-after-force-merge-1-seg | p50 ms | 4.61 (4.43–4.79) | 6.65 (6.65–6.66) |
| sort-status-asc-after-force-merge-1-seg | p50 ms | 3.44 (3.42–3.47) | 5.39 (5.33–5.44) |

roll wins every query task (1.06x for `scroll`, otherwise 1.2–3.2x); indexing is even (240k against 250k docs/s, inside
Elasticsearch's 193k–307k spread).

**Rally geonames 10%** (1,103,000 documents, 5 shards). As in round 3, roll's `index-append`
finished inside Rally's 10 s warm-up in one round, so the first row is the whole load's rate
(1,103,000 documents / the slowest client's task time in Rally's log) for both targets.

| task | metric | roll | Elasticsearch |
|---|---|---:|---:|
| index-append | docs/s, whole load | 108.9k (107.2k–110.5k) | 74.8k (73.2k–76.4k) |
| index-stats | p50 ms | 1.95 (1.94–1.96) | 2.84 (2.60–3.08) |
| node-stats | p50 ms | 1.70 (1.67–1.74) | 6.21 (6.04–6.37) |
| default | p50 ms | 2.04 (2.04–2.04) | 3.33 (2.95–3.72) |
| term | p50 ms | 1.15 (1.15–1.15) | 2.31 (2.20–2.42) |
| phrase | p50 ms | 1.22 (1.21–1.22) | 2.22 (2.21–2.23) |
| country_agg_uncached | p50 ms | 10.22 (10.16–10.29) | 16.89 (16.62–17.17) |
| country_agg_cached | p50 ms | 1.83 (1.82–1.84) | 2.61 (2.57–2.65) |
| scroll | p50 ms | 485.1 (476.7–493.5) | 697.8 (695.1–700.4) |
| expression | p50 ms | 30.84 (30.67–31.02) | 37.20 (36.38–38.02) |
| painless_static | p50 ms | 35.51 (34.92–36.10) | 43.40 (37.24–49.57) |
| painless_dynamic | p50 ms | 34.30 (34.04–34.56) | 38.72 (36.43–41.02) |
| decay_geo_gauss_function_score | p50 ms | 23.70 (23.55–23.85) | 33.02 (31.70–34.34) |
| decay_geo_gauss_script_score | p50 ms | 37.11 (36.70–37.51) | 34.74 (31.77–37.70) |
| field_value_function_score | p50 ms | 6.67 (6.60–6.74) | 15.24 (14.51–15.97) |
| field_value_script_score | p50 ms | 21.76 (21.72–21.79) | 20.06 (19.07–21.04) |
| large_terms | p50 ms | 56.46 (54.93–57.98) | 75.13 (71.84–78.43) |
| large_filtered_terms | p50 ms | 58.64 (57.87–59.41) | 76.16 (74.21–78.12) |
| large_prohibited_terms | p50 ms | 58.76 (58.74–58.79) | 76.27 (75.00–77.55) |
| desc_sort_population | p50 ms | 4.14 (4.06–4.23) | 5.83 (5.79–5.87) |
| desc_sort_population_can_match_shortcut | p50 ms | 3.78 (3.44–4.13) | 5.45 (5.31–5.59) |
| desc_sort_population_no_can_match_shortcut | p50 ms | 3.99 (3.35–4.63) | 5.35 (5.29–5.41) |
| asc_sort_population | p50 ms | 2.73 (2.69–2.77) | 7.52 (3.33–11.71) |
| asc_sort_with_after_population | p50 ms | 3.58 (3.50–3.65) | 4.53 (4.45–4.62) |
| desc_sort_geonameid | p50 ms | 4.76 (4.04–5.49) | 12.64 (10.41–14.87) |
| desc_sort_with_after_geonameid | p50 ms | 6.64 (6.30–6.97) | 9.14 (9.05–9.24) |
| asc_sort_geonameid | p50 ms | 4.04 (3.72–4.36) | 4.98 (4.83–5.12) |
| asc_sort_with_after_geonameid | p50 ms | 5.25 (5.22–5.29) | 6.52 (5.51–7.53) |
| sort_country_code_can_match_shortcut | p50 ms | 6.49 (6.13–6.85) | 7.83 (7.59–8.06) |
| sort_country_code_no_can_match_shortcut | p50 ms | 6.62 (6.22–7.02) | 7.29 (7.18–7.39) |

roll wins every task but two script scores: `decay_geo_gauss_script_score` (37.1 vs 34.7 ms,
inside Elasticsearch's 31.8–37.7 spread) and `field_value_script_score` (21.8 vs 20.1 ms, just
above its 19.1–21.0). The two `search_after` sorts on `geonameid` that sat at the edge in round
3 now win (6.6 vs 9.1 and 5.3 vs 6.5 ms), and the load is 1.5x faster (Elasticsearch indexed
this subset at 117k docs/s in round 3 and 75k here; roll at 111k and 109k).

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

**Still open** (performance): fetching hits decompresses a whole 16 KiB zstd stored-fields chunk
per hit (36% of a 10-hit `match`'s CPU; Lucene's LZ4 blocks decompress only up to the document);
a 10-hit fetch still costs about 0.1 ms more CPU than in round 2; the two geonames script scores
above.

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
`6-9,24-27`, two rounds per track (roll first in round 0, Elasticsearch first in round 1),
median (min–max) over rounds. The host was quieter than in round 2 (load 5–20), so absolute
times are lower on both sides. roll = `r9c`, the tree at the end of the round.

**geonames 10%** (1,103,000 documents). roll finished `index-append` inside Rally's 10 s bulk
warm-up in both rounds, so Rally has no throughput sample for it; the first row is the whole
load's rate from Rally's log instead (documents / the slowest client's task time), for both
targets alike.

| task | metric | roll `r9c` | Elasticsearch | roll `r8e` (start of the round's last stretch) |
|---|---|---:|---:|---:|
| index-append | docs/s, whole load (1,103,000 docs / slowest client) | 111.4k (110.6k–112.1k) | 117.3k (108.0k–126.6k) | 90.9k (Rally median, 87.6k–94.1k) |
| index-stats | p50 ms | 1.94 (1.94–1.94) | 2.89 (2.88–2.90) | 1.37 (1.06–1.67) |
| node-stats | p50 ms | 1.68 (1.68–1.68) | 6.37 (6.37–6.38) | 1.22 (1.14–1.31) |
| default | p50 ms | 2.20 (2.20–2.20) | 3.10 (3.07–3.13) | 1.35 (1.30–1.40) |
| term | p50 ms | 1.37 (1.36–1.38) | 2.17 (2.16–2.17) | 0.64 (0.63–0.66) |
| phrase | p50 ms | 1.41 (1.34–1.47) | 2.29 (2.27–2.31) | 0.64 (0.58–0.69) |
| country_agg_uncached | p50 ms | 10.94 (10.88–11.00) | 18.47 (18.11–18.83) | 13.71 (12.93–14.48) |
| country_agg_cached | p50 ms | 1.94 (1.93–1.95) | 2.57 (2.56–2.59) | 1.17 (1.14–1.21) |
| scroll | p50 ms | 501.9 (498.6–505.2) | 686.0 (670.0–702.1) | 805.6 (800.3–810.8) |
| expression | p50 ms | 29.72 (29.37–30.06) | 35.17 (34.16–36.19) | 35.68 (34.48–36.88) |
| painless_static | p50 ms | 35.60 (35.44–35.77) | 39.25 (38.52–39.99) | 43.47 (43.18–43.77) |
| painless_dynamic | p50 ms | 35.33 (35.05–35.61) | 39.16 (38.48–39.85) | 42.25 (41.46–43.05) |
| decay_geo_gauss_function_score | p50 ms | 23.62 (23.44–23.81) | 35.84 (34.59–37.09) | 28.15 (27.31–28.98) |
| decay_geo_gauss_script_score | p50 ms | 37.46 (37.40–37.52) | 35.96 (33.54–38.38) | 46.42 (45.80–47.05) |
| field_value_function_score | p50 ms | 8.12 (7.05–9.18) | 16.79 (16.10–17.48) | 7.57 (6.98–8.17) |
| field_value_script_score | p50 ms | 23.11 (23.01–23.22) | 21.43 (20.72–22.14) | 28.00 (27.61–28.40) |
| large_terms | p50 ms | 55.80 (54.95–56.65) | 108.8 (106.4–111.3) | 72.60 (68.26–76.94) |
| large_filtered_terms | p50 ms | 56.92 (55.20–58.64) | 110.3 (108.4–112.3) | 67.16 (58.60–75.73) |
| large_prohibited_terms | p50 ms | 58.65 (55.76–61.54) | 109.1 (107.7–110.4) | 78.34 (76.85–79.83) |
| desc_sort_population | p50 ms | 4.78 (4.69–4.87) | 5.87 (5.84–5.89) | 3.79 (3.24–4.34) |
| desc_sort_population_can_match_shortcut | p50 ms | 4.12 (3.67–4.56) | 5.70 (5.62–5.79) | 3.08 (2.85–3.32) |
| desc_sort_population_no_can_match_shortcut | p50 ms | 3.99 (3.70–4.28) | 5.40 (5.38–5.42) | 2.32 (2.25–2.38) |
| asc_sort_population | p50 ms | 3.01 (2.91–3.11) | 3.74 (3.63–3.85) | 2.65 (2.58–2.72) |
| asc_sort_with_after_population | p50 ms | 3.85 (3.83–3.87) | 4.62 (4.59–4.64) | 3.17 (2.74–3.60) |
| desc_sort_geonameid | p50 ms | 6.02 (5.95–6.10) | 8.79 (8.18–9.40) | 4.35 (3.11–5.59) |
| desc_sort_with_after_geonameid | p50 ms | 7.68 (7.54–7.81) | 7.55 (7.27–7.83) | 5.06 (4.42–5.70) |
| asc_sort_geonameid | p50 ms | 3.68 (3.50–3.86) | 4.35 (4.28–4.41) | 3.62 (3.56–3.67) |
| asc_sort_with_after_geonameid | p50 ms | 6.13 (5.87–6.40) | 5.32 (5.10–5.54) | 3.18 (2.43–3.93) |
| sort_country_code_can_match_shortcut | p50 ms | 5.42 (5.32–5.53) | 7.17 (6.58–7.76) | 4.50 (4.38–4.62) |
| sort_country_code_no_can_match_shortcut | p50 ms | 6.16 (6.11–6.22) | 7.31 (6.90–7.72) | 5.77 (4.01–7.53) |

roll wins every task but two script scores and two `search_after` sorts, which are within the
spread of the Elasticsearch rounds or close to it (`decay_geo_gauss_script_score` 37.5 vs
33.5–38.4 ms, `field_value_script_score` 23.1 vs 20.7–22.1 ms, `asc_sort_with_after_geonameid`
6.1 vs 5.1–5.5 ms, `desc_sort_with_after_geonameid` 7.7 vs 7.3–7.8 ms; the same binaries on a
copy of Rally's roll index measure the same before and after this round's sort changes).
`scroll` went from a 1.5x loss (806 vs 521 ms in `r8e`) to a 1.4x win: Rally scrolls with
`sort: ["_doc"]`, which now stops each segment once the page is full.

**http_logs 2%** (4,948,746 documents in 2 of the track's 7 daily indices, 5 shards each, 10
shards per search; every operation unthrottled). `r9a` is the tree before this round's
`http_logs` fixes (absent-field sort pruning, `search_after` pruning start, inverse point
ranges, constant-score early termination).

| task | metric | roll `r9c` | Elasticsearch | roll `r9a` (before) |
|---|---|---:|---:|---:|
| index-append | docs/s | 246,852 (244,544–249,161) | 262,978 (224,076–301,881) | 245,058 (244,519–245,597) |
| index-append | p50 ms | 130.1 (127.1–133.0) | 103.4 (102.7–104.1) | 132.8 (131.7–134.0) |
| default | p50 ms | 2.80 (2.77–2.82) | 3.76 (3.14–4.38) | 2.49 (2.30–2.67) |
| term | p50 ms | 2.33 (2.32–2.34) | 4.53 (4.28–4.78) | 2.20 (2.02–2.39) |
| terms_enum | p50 ms | 1.48 (1.28–1.67) | 3.49 (3.45–3.54) | 1.15 (0.77–1.52) |
| range | p50 ms | 1.81 (1.80–1.82) | 2.35 (2.14–2.56) | 1.58 (1.38–1.79) |
| 200s-in-range | p50 ms | 9.70 (8.89–10.51) | 9.89 (9.46–10.32) | 9.69 (9.19–10.19) |
| 400s-in-range | p50 ms | 2.78 (2.74–2.81) | 4.77 (4.66–4.87) | 2.73 (2.58–2.87) |
| hourly_agg | p50 ms | 43.53 (43.18–43.87) | 91.75 (80.76–102.7) | 43.04 (42.80–43.27) |
| scroll | p50 ms | 453.3 (451.1–455.5) | 486.0 (483.3–488.7) | 437.6 (424.1–451.1) |
| desc_sort_timestamp | p50 ms | 8.67 (8.58–8.75) | 13.63 (13.49–13.76) | 8.80 (8.31–9.28) |
| asc_sort_timestamp | p50 ms | 6.00 (5.79–6.21) | 12.59 (11.64–13.53) | 5.96 (5.88–6.03) |
| desc_sort_with_after_timestamp | p50 ms | 8.47 (8.36–8.58) | 14.22 (14.07–14.36) | 8.85 (8.43–9.27) |
| asc_sort_with_after_timestamp | p50 ms | 1.82 (1.81–1.83) | 5.50 (5.46–5.54) | 10.86 (9.37–12.36) |
| desc_sort_timestamp_can_match_shortcut | p50 ms | 7.80 (6.75–8.85) | 13.08 (13.07–13.08) | 7.54 (7.43–7.65) |
| desc_sort_timestamp_no_can_match_shortcut | p50 ms | 8.28 (8.08–8.49) | 15.03 (14.49–15.57) | 7.52 (7.36–7.68) |
| sort_size_desc | p50 ms | 5.46 (5.10–5.82) | 7.46 (7.17–7.76) | 4.50 (3.97–5.03) |
| sort_size_asc | p50 ms | 4.16 (4.14–4.18) | 6.39 (6.19–6.59) | 3.74 (3.37–4.11) |
| sort_status_desc | p50 ms | 4.57 (4.44–4.71) | 6.50 (6.46–6.55) | 4.31 (4.07–4.56) |
| sort_status_asc | p50 ms | 4.10 (4.08–4.11) | 6.03 (5.90–6.16) | 3.73 (3.33–4.12) |
| sort_keyword_can_match_shortcut | p50 ms | 3.71 (3.69–3.73) | 5.83 (5.68–5.97) | 33.97 (32.67–35.28) |
| sort_keyword_no_can_match_shortcut | p50 ms | 3.74 (3.69–3.80) | 4.89 (4.76–5.02) | 33.70 (32.33–35.08) |
| sort_numeric_can_match_shortcut | p50 ms | 6.71 (6.38–7.03) | 8.19 (7.92–8.46) | 6.49 (6.08–6.90) |
| sort_numeric_no_can_match_shortcut | p50 ms | 6.36 (6.34–6.38) | 7.64 (7.41–7.86) | 6.19 (6.15–6.23) |
| desc-sort-timestamp-after-force-merge-1-seg | p50 ms | 7.00 (6.90–7.11) | 24.99 (21.22–28.77) | 8.69 (8.28–9.11) |
| asc-sort-timestamp-after-force-merge-1-seg | p50 ms | 5.24 (5.05–5.42) | 8.50 (6.26–10.73) | 5.00 (4.92–5.09) |
| desc-sort-with-after-timestamp-after-force-merge-1-seg | p50 ms | 8.53 (8.42–8.64) | 23.91 (19.47–28.35) | 8.82 (7.20–10.45) |
| asc-sort-with-after-timestamp-after-force-merge-1-seg | p50 ms | 2.12 (2.11–2.14) | 4.63 (4.62–4.64) | 24.04 (23.50–24.58) |
| sort-size-desc-after-force-merge-1-seg | p50 ms | 5.65 (5.36–5.94) | 8.01 (7.74–8.27) | 5.99 (5.53–6.44) |
| sort-size-asc-after-force-merge-1-seg | p50 ms | 4.20 (4.12–4.28) | 5.65 (5.55–5.75) | 3.94 (3.73–4.15) |
| sort-status-desc-after-force-merge-1-seg | p50 ms | 4.64 (4.43–4.85) | 6.28 (6.27–6.30) | 4.32 (4.17–4.47) |
| sort-status-asc-after-force-merge-1-seg | p50 ms | 4.13 (4.12–4.15) | 5.21 (5.15–5.27) | 3.76 (3.42–4.10) |

roll wins every query task; indexing is even (247k vs 263k docs/s, with Elasticsearch's two
rounds 224k and 302k). The two tasks roll lost before this round, `sort_keyword_*` (34 vs 5 ms:
the sort field is absent from these documents) and `asc_sort_with_after_timestamp` (11 vs 4.7
ms, 24 vs 4.0 ms force-merged: the bound lies after every value), now win 3.7 vs 5.8 ms and 1.8
vs 5.5 ms (2.1 vs 4.6 ms force-merged).

## Round 2 of the perf lane

Targets pinned to the 8 hardware threads `14-17,32-35` (`BENCH_CPUS`), Rally on `6-9,24-27`,
same Docker limits as below. roll = `r5i` (this round's tuning, before the analyzer pool fix
that came after it). Rally geonames 10%: two roll races and three Elasticsearch races (a Rally
worker crashed in one roll race, which was rerun as a third round: roll, then Elasticsearch);
median (min-max) over races.

| task | metric | roll median (min-max) | es median (min-max) |
|---|---|---:|---:|
| index-append | median throughput (docs/s) | 48624.34 (44970.41-52278.27) | 89181.91 (33742.30-93806.94) |
| index-append | p50 service time (ms) | 883.96 (791.45-976.47) | 164.53 (112.01-523.98) |
| index-stats | p50 service time (ms) | 1.57 (1.50-1.64) | 1.80 (1.24-2.13) |
| node-stats | p50 service time (ms) | 1.56 (1.51-1.61) | 5.27 (4.40-5.30) |
| default | p50 service time (ms) | 2.11 (1.72-2.50) | 2.53 (2.29-2.86) |
| term | p50 service time (ms) | 1.12 (0.86-1.37) | 1.51 (1.07-1.60) |
| phrase | p50 service time (ms) | 1.24 (1.00-1.48) | 1.24 (1.03-1.25) |
| country_agg_uncached | p50 service time (ms) | 22.17 (15.82-28.53) | 19.52 (18.20-21.36) |
| country_agg_cached | p50 service time (ms) | 1.69 (1.50-1.88) | 1.88 (1.42-1.97) |
| scroll | p50 service time (ms) | 1233.93 (1075.92-1391.94) | 509.53 (478.21-574.96) |
| expression | p50 service time (ms) | 61.26 (58.04-64.49) | 54.49 (44.75-65.31) |
| painless_static | p50 service time (ms) | 67.68 (65.90-69.45) | 65.56 (57.25-66.16) |
| painless_dynamic | p50 service time (ms) | 63.96 (60.99-66.92) | 61.88 (56.51-62.69) |
| decay_geo_gauss_function_score | p50 service time (ms) | 35.32 (31.56-39.09) | 53.61 (42.74-54.05) |
| decay_geo_gauss_script_score | p50 service time (ms) | 73.46 (71.86-75.07) | 58.20 (49.38-61.30) |
| field_value_function_score | p50 service time (ms) | 12.58 (12.24-12.91) | 25.32 (23.81-29.45) |
| field_value_script_score | p50 service time (ms) | 43.93 (40.99-46.86) | 38.12 (34.89-38.68) |
| large_terms | p50 service time (ms) | 102.77 (92.00-113.54) | 125.71 (124.81-178.64) |
| large_filtered_terms | p50 service time (ms) | 125.03 (117.98-132.08) | 134.10 (127.24-140.79) |
| large_prohibited_terms | p50 service time (ms) | 115.41 (112.05-118.76) | 121.76 (108.42-125.45) |
| desc_sort_population | p50 service time (ms) | 3.37 (3.27-3.47) | 4.48 (4.44-6.77) |
| desc_sort_population_can_match_shortcut | p50 service time (ms) | 3.71 (3.39-4.04) | 4.98 (4.34-7.08) |
| desc_sort_population_no_can_match_shortcut | p50 service time (ms) | 3.88 (3.79-3.97) | 4.20 (3.88-4.46) |
| asc_sort_population | p50 service time (ms) | 2.78 (2.14-3.42) | 3.08 (2.79-4.43) |
| asc_sort_with_after_population | p50 service time (ms) | 4.18 (3.84-4.53) | 3.62 (3.62-5.04) |
| desc_sort_geonameid | p50 service time (ms) | 5.63 (5.43-5.83) | 8.65 (7.26-10.59) |
| desc_sort_with_after_geonameid | p50 service time (ms) | 6.33 (5.07-7.59) | 6.71 (6.21-6.91) |
| asc_sort_geonameid | p50 service time (ms) | 3.95 (2.98-4.93) | 3.93 (3.59-8.37) |
| asc_sort_with_after_geonameid | p50 service time (ms) | 6.26 (4.39-8.13) | 4.73 (3.85-8.18) |
| sort_country_code_can_match_shortcut | p50 service time (ms) | 5.90 (5.58-6.22) | 6.67 (6.64-7.71) |
| sort_country_code_no_can_match_shortcut | p50 service time (ms) | 5.35 (4.36-6.33) | 6.57 (6.31-7.33) |

Against the previous round's run (`r4g`, the table further down): Painless `painless_static` /
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

## Rally geonames (10%)

Two rounds per target, interleaved (roll first in round 0, Elasticsearch first in round 1);
median of the per-round values with the min–max over rounds. roll = `r4g` (this round's
tuning, before the `search_after` fast path); the last column is the same run of the tree
before this round (`r4a`, with the Elasticsearch median of that run in parentheses: load
differed between the runs, compare within a run). `index-append` is 8 Rally clients sending
5,000-doc bulks; its p50 service time is per bulk request.

| task | metric | Elasticsearch | roll | roll before this round (ES in that run) |
|---|---|---:|---:|---:|
| index-append | docs/s, median | 51283.99 (39849.78-62718.20) | 49477.75 (47007.71-51947.79) | 50488.97 (56032.07) |
| index-append | p50 ms | 449.49 (419.28-479.69) | 929.42 (898.22-960.63) | 837.41 (423.89) |
| index-stats | p50 ms | 1.85 (1.44-2.25) | 1.84 (1.78-1.90) | 1.64 (2.08) |
| node-stats | p50 ms | 5.02 (4.63-5.42) | 1.57 (1.48-1.66) | 1.55 (5.94) |
| default | p50 ms | 3.02 (2.00-4.04) | 2.20 (1.91-2.48) | 2.41 (2.59) |
| term | p50 ms | 1.90 (1.22-2.58) | 1.10 (0.74-1.45) | 0.93 (1.21) |
| phrase | p50 ms | 1.90 (1.05-2.74) | 1.19 (1.11-1.28) | 0.87 (1.25) |
| country_agg_uncached | p50 ms | 26.05 (18.96-33.14) | 16.03 (15.59-16.46) | 15.56 (17.37) |
| country_agg_cached | p50 ms | 2.19 (1.51-2.88) | 1.38 (1.10-1.67) | 1.45 (1.93) |
| scroll | p50 ms | 527.42 (521.53-533.31) | 1463.42 (1306.33-1620.52) | 1999.59 (498.86) |
| expression | p50 ms | 63.58 (48.13-79.03) | 112.23 (89.70-134.76) | 81.63 (44.10) |
| painless_static | p50 ms | 71.93 (57.16-86.71) | 439.95 (425.40-454.50) | 888.64 (51.16) |
| painless_dynamic | p50 ms | 62.77 (55.38-70.16) | 371.06 (371.01-371.11) | 738.86 (51.47) |
| decay_geo_gauss_function_score | p50 ms | 48.34 (43.04-53.64) | 76.88 (67.17-86.60) | 55.86 (42.75) |
| decay_geo_gauss_script_score | p50 ms | 48.44 (44.38-52.50) | 273.39 (269.15-277.63) | 264.04 (46.95) |
| field_value_function_score | p50 ms | 21.51 (18.23-24.80) | 37.35 (36.98-37.72) | 36.32 (21.49) |
| field_value_script_score | p50 ms | 30.46 (25.64-35.29) | 65.80 (65.29-66.30) | 63.05 (29.24) |
| large_terms | p50 ms | 122.56 (121.63-123.50) | 145.36 (137.97-152.74) | 187.66 (110.92) |
| large_filtered_terms | p50 ms | 120.44 (117.37-123.52) | 160.52 (145.82-175.22) | 208.03 (126.84) |
| large_prohibited_terms | p50 ms | 116.87 (103.67-130.08) | 158.53 (153.76-163.29) | 217.47 (121.08) |
| desc_sort_population | p50 ms | 5.32 (4.48-6.16) | 3.54 (3.00-4.08) | 4.59 (4.17) |
| desc_sort_population_can_match_shortcut | p50 ms | 5.08 (5.00-5.16) | 3.18 (2.73-3.63) | 3.63 (4.81) |
| desc_sort_population_no_can_match_shortcut | p50 ms | 3.98 (3.53-4.43) | 3.44 (2.78-4.10) | 3.72 (4.05) |
| asc_sort_population | p50 ms | 6.05 (3.53-8.58) | 2.45 (1.77-3.13) | 2.67 (2.71) |
| asc_sort_with_after_population | p50 ms | 3.63 (3.54-3.72) | 3.39 (3.17-3.61) | 4.05 (3.42) |
| desc_sort_geonameid | p50 ms | 6.97 (6.19-7.75) | 4.30 (3.80-4.81) | 4.18 (6.16) |
| desc_sort_with_after_geonameid | p50 ms | 7.23 (5.88-8.58) | 11.65 (7.68-15.63) | 11.02 (7.75) |
| asc_sort_geonameid | p50 ms | 3.83 (3.76-3.90) | 3.83 (3.02-4.64) | 3.43 (3.56) |
| asc_sort_with_after_geonameid | p50 ms | 4.29 (4.21-4.36) | 5.59 (3.98-7.21) | 7.31 (3.83) |
| sort_country_code_can_match_shortcut | p50 ms | 7.90 (7.41-8.39) | 5.71 (4.43-6.99) | 7.40 (8.80) |
| sort_country_code_no_can_match_shortcut | p50 ms | 7.65 (7.54-7.76) | 6.99 (6.91-7.08) | 7.21 (7.73) |

Where roll stands (r4g run):

- **Wins**: every term, phrase and match task, `country_agg` cached and uncached (1.6x),
  node/index stats, and all sorts without `search_after` (1.1–2.5x).
- **Even**: indexing throughput (49.5k vs 51.3k docs/s with overlapping spreads; Elasticsearch
  ranged 39.8k–62.7k), `asc_sort_with_after_population`.
- **Losses**: Painless scripts (`painless_*` 5–6x, `decay_geo_gauss_script_score` 5.6x,
  `field_value_script_score` 2.2x): roll interprets Painless bytecode while Elasticsearch
  JIT-compiles it to JVM bytecode; this round halved them (889 → 440 ms and 739 → 371 ms).
  `expression` (1.8x) and the function scores (`field_value_function_score` 1.7x,
  `decay_geo_gauss_function_score` 1.6x) spend their time in per-document doc-value reads
  and scoring calls (no block-at-a-time scoring yet). `scroll` (2.8x, from 3.8x): fetching 1,000
  hits per page parses each `_source` into a JSON tree and renders it again where
  Elasticsearch copies the bytes. The 45,586-term `terms` queries lose by 1.2–1.3x (from 1.7x).
  `search_after` on `geonameid` (1.3–1.6x; the fast path that came after this run takes
  15–20% off).

RSS of the server process at the end of a round: roll 379–428 MiB, Elasticsearch 2.6–2.7 GiB
(2 GiB of it is the configured heap).

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
numbers yet.
