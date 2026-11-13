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

## Round 7: roll, Elasticsearch 9.5.5 and OpenSearch 3.9 (2026-11-12)

The tree merged with `main` (`search-dev --release`). All three targets in Docker on the same
CPUs (`10,11,28,29`: 2 cores / 4 hardware threads; lane b of the host's benchmark lanes, held
under its lock) and 4 GiB each, Elasticsearch and OpenSearch with 2 GiB heaps; Rally on
`12,13,30,31`. The host was busy (1-minute load 29–48 on 36 threads, other sessions building and
benchmarking), so the tables give medians over rounds with their min–max, and the server CPU
per request, which load distorts less than wall time. Runs alternate between targets per
workload (micro) and per round (Rally), so drift hits all three alike. Raw outputs:
`~/.cache/rsearch/bench/` on the machine that ran them.

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

**Rally, round 0** (`bench-search.sh rally <track> 2`). A first pass against roll and
Elasticsearch only: Rally's client refused OpenSearch at its product check, which stopped each
track after its first round. The script now skips that check inside Rally's container, and both
tracks are being run again in full for all three targets; until then these are single runs, so
treat differences under ~15% as noise.

`http_logs` (`ingest_percentage:2`, 4.95M log lines in 5-shard indices; every operation
unthrottled). roll wins every task but two, which tie (`desc_sort_timestamp_can_match_shortcut`,
`sort_status_asc`); it indexes at 131k docs/s against 96k. RSS at the end of the race: roll
330 MiB, Elasticsearch 2,338 MiB.

| task | metric | es median (min-max) | roll median (min-max) |
|---|---|---:|---:|
| index-append | median throughput (docs/s) | 96191.90 (96191.90-96191.90) | 130688.58 (130688.58-130688.58) |
| index-append | p50 service time (ms) | 358.40 (358.40-358.40) | 259.37 (259.37-259.37) |
| default | p50 service time (ms) | 5.55 (5.55-5.55) | 2.33 (2.33-2.33) |
| term | p50 service time (ms) | 5.16 (5.16-5.16) | 2.63 (2.63-2.63) |
| terms_enum | p50 service time (ms) | 3.45 (3.45-3.45) | 1.16 (1.16-1.16) |
| range | p50 service time (ms) | 3.67 (3.67-3.67) | 1.40 (1.40-1.40) |
| 200s-in-range | p50 service time (ms) | 10.54 (10.54-10.54) | 9.08 (9.08-9.08) |
| 400s-in-range | p50 service time (ms) | 6.20 (6.20-6.20) | 2.40 (2.40-2.40) |
| hourly_agg | p50 service time (ms) | 120.82 (120.82-120.82) | 81.86 (81.86-81.86) |
| scroll | p50 service time (ms) | 505.56 (505.56-505.56) | 446.29 (446.29-446.29) |
| desc_sort_timestamp | p50 service time (ms) | 15.59 (15.59-15.59) | 7.45 (7.45-7.45) |
| asc_sort_timestamp | p50 service time (ms) | 12.00 (12.00-12.00) | 6.03 (6.03-6.03) |
| desc_sort_with_after_timestamp | p50 service time (ms) | 12.66 (12.66-12.66) | 7.08 (7.08-7.08) |
| asc_sort_with_after_timestamp | p50 service time (ms) | 6.32 (6.32-6.32) | 1.92 (1.92-1.92) |
| desc_sort_timestamp_can_match_shortcut | p50 service time (ms) | 12.19 (12.19-12.19) | 12.26 (12.26-12.26) |
| desc_sort_timestamp_no_can_match_shortcut | p50 service time (ms) | 21.60 (21.60-21.60) | 12.51 (12.51-12.51) |
| sort_size_desc | p50 service time (ms) | 7.26 (7.26-7.26) | 6.56 (6.56-6.56) |
| sort_size_asc | p50 service time (ms) | 6.23 (6.23-6.23) | 3.70 (3.70-3.70) |
| sort_status_desc | p50 service time (ms) | 6.48 (6.48-6.48) | 5.75 (5.75-5.75) |
| sort_status_asc | p50 service time (ms) | 6.83 (6.83-6.83) | 6.73 (6.73-6.73) |
| sort_keyword_can_match_shortcut | p50 service time (ms) | 6.54 (6.54-6.54) | 3.62 (3.62-3.62) |
| sort_keyword_no_can_match_shortcut | p50 service time (ms) | 5.65 (5.65-5.65) | 4.59 (4.59-4.59) |
| sort_numeric_can_match_shortcut | p50 service time (ms) | 7.79 (7.79-7.79) | 5.77 (5.77-5.77) |
| sort_numeric_no_can_match_shortcut | p50 service time (ms) | 7.88 (7.88-7.88) | 5.23 (5.23-5.23) |
| desc-sort-timestamp-after-force-merge-1-seg | p50 service time (ms) | 10.53 (10.53-10.53) | 5.67 (5.67-5.67) |
| asc-sort-timestamp-after-force-merge-1-seg | p50 service time (ms) | 7.54 (7.54-7.54) | 6.32 (6.32-6.32) |
| desc-sort-with-after-timestamp-after-force-merge-1-seg | p50 service time (ms) | 9.31 (9.31-9.31) | 6.20 (6.20-6.20) |
| asc-sort-with-after-timestamp-after-force-merge-1-seg | p50 service time (ms) | 4.78 (4.78-4.78) | 1.42 (1.42-1.42) |
| sort-size-desc-after-force-merge-1-seg | p50 service time (ms) | 10.35 (10.35-10.35) | 5.20 (5.20-5.20) |
| sort-size-asc-after-force-merge-1-seg | p50 service time (ms) | 7.58 (7.58-7.58) | 5.18 (5.18-5.18) |
| sort-status-desc-after-force-merge-1-seg | p50 service time (ms) | 7.88 (7.88-7.88) | 4.08 (4.08-4.08) |
| sort-status-asc-after-force-merge-1-seg | p50 service time (ms) | 4.99 (4.99-4.99) | 4.88 (4.88-4.88) |

`geonames` (`ingest_percentage:10`, 1.1M documents, 5 shards). roll indexes at 61k docs/s
against 27k and wins most tasks, many by 2–4x. It loses the Painless script scores
(`painless_dynamic` 94.4 against 82.2 ms; `field_value_script_score` 46.2 against 43.5 ms;
`decay_geo_gauss_script_score` a tie) and two numeric sorts by `geonameid`
(`desc_sort_geonameid` 5.45 against 5.23 ms, `asc_sort_geonameid` 4.72 against 4.16 ms). RSS at
the end of the race: roll 256 MiB, Elasticsearch 2,649 MiB.

| task | metric | es median (min-max) | roll median (min-max) |
|---|---|---:|---:|
| index-append | median throughput (docs/s) | 26573.43 (26573.43-26573.43) | 60998.94 (60998.94-60998.94) |
| index-append | p50 service time (ms) | 936.06 (936.06-936.06) | 866.78 (866.78-866.78) |
| index-stats | p50 service time (ms) | 3.42 (3.42-3.42) | 1.28 (1.28-1.28) |
| node-stats | p50 service time (ms) | 6.57 (6.57-6.57) | 1.58 (1.58-1.58) |
| default | p50 service time (ms) | 4.49 (4.49-4.49) | 1.77 (1.77-1.77) |
| term | p50 service time (ms) | 2.87 (2.87-2.87) | 0.69 (0.69-0.69) |
| phrase | p50 service time (ms) | 2.90 (2.90-2.90) | 0.83 (0.83-0.83) |
| country_agg_uncached | p50 service time (ms) | 27.85 (27.85-27.85) | 22.71 (22.71-22.71) |
| country_agg_cached | p50 service time (ms) | 3.47 (3.47-3.47) | 1.36 (1.36-1.36) |
| scroll | p50 service time (ms) | 547.15 (547.15-547.15) | 486.12 (486.12-486.12) |
| expression | p50 service time (ms) | 75.07 (75.07-75.07) | 58.99 (58.99-58.99) |
| painless_static | p50 service time (ms) | 94.16 (94.16-94.16) | 78.01 (78.01-78.01) |
| painless_dynamic | p50 service time (ms) | 82.23 (82.23-82.23) | 94.40 (94.40-94.40) |
| decay_geo_gauss_function_score | p50 service time (ms) | 71.05 (71.05-71.05) | 60.93 (60.93-60.93) |
| decay_geo_gauss_script_score | p50 service time (ms) | 72.97 (72.97-72.97) | 73.79 (73.79-73.79) |
| field_value_function_score | p50 service time (ms) | 26.88 (26.88-26.88) | 12.07 (12.07-12.07) |
| field_value_script_score | p50 service time (ms) | 43.47 (43.47-43.47) | 46.22 (46.22-46.22) |
| large_terms | p50 service time (ms) | 153.36 (153.36-153.36) | 113.52 (113.52-113.52) |
| large_filtered_terms | p50 service time (ms) | 161.84 (161.84-161.84) | 126.44 (126.44-126.44) |
| large_prohibited_terms | p50 service time (ms) | 164.96 (164.96-164.96) | 110.27 (110.27-110.27) |
| desc_sort_population | p50 service time (ms) | 6.88 (6.88-6.88) | 4.28 (4.28-4.28) |
| desc_sort_population_can_match_shortcut | p50 service time (ms) | 5.99 (5.99-5.99) | 4.47 (4.47-4.47) |
| desc_sort_population_no_can_match_shortcut | p50 service time (ms) | 6.47 (6.47-6.47) | 3.51 (3.51-3.51) |
| asc_sort_population | p50 service time (ms) | 4.68 (4.68-4.68) | 3.94 (3.94-3.94) |
| asc_sort_with_after_population | p50 service time (ms) | 5.20 (5.20-5.20) | 4.16 (4.16-4.16) |
| desc_sort_geonameid | p50 service time (ms) | 5.23 (5.23-5.23) | 5.45 (5.45-5.45) |
| desc_sort_with_after_geonameid | p50 service time (ms) | 7.25 (7.25-7.25) | 7.06 (7.06-7.06) |
| asc_sort_geonameid | p50 service time (ms) | 4.16 (4.16-4.16) | 4.72 (4.72-4.72) |
| asc_sort_with_after_geonameid | p50 service time (ms) | 5.90 (5.90-5.90) | 2.90 (2.90-2.90) |
| sort_country_code_can_match_shortcut | p50 service time (ms) | 8.67 (8.67-8.67) | 5.41 (5.41-5.41) |
| sort_country_code_no_can_match_shortcut | p50 service time (ms) | 8.95 (8.95-8.95) | 6.59 (6.59-6.59) |

`geonames`, all three targets (two races each: roll and Elasticsearch on the binaries and
containers above, OpenSearch two rounds of one race after its product and version checks were
skipped). roll indexes at 2x either and wins every task but two Painless script scores:
`field_value_script_score` (46.8 ms against Elasticsearch's 41.3 and OpenSearch's 42.1) and
`decay_geo_gauss_script_score` (79.1 against 70.7 and 88.6). `painless_dynamic`, a loss in
round 0's single run, is a tie (76.5 against 78.3; roll's two runs spread 58.6-94.4). RSS at the
end: roll 256 MiB, Elasticsearch 2,649-2,756 MiB, OpenSearch 2,797-2,806 MiB.

| task | metric | roll | es | os |
|---|---|---:|---:|---:|
| index-append | median throughput (docs/s) | 57491.27 (53983.60-60998.94) | 28085.74 (26573.43-29598.04) | 28683.16 (24578.01-32788.31) |
| index-append | p50 service time (ms) | 806.99 (747.20-866.78) | 900.07 (864.09-936.06) | 989.34 (972.92-1005.76) |
| index-stats | median throughput (docs/s) | 419.23 (392.21-446.24) | 178.15 (165.76-190.54) | 213.85 (190.66-237.04) |
| index-stats | p50 service time (ms) | 1.48 (1.28-1.68) | 3.29 (3.16-3.42) | 2.72 (2.48-2.96) |
| node-stats | p50 service time (ms) | 1.65 (1.58-1.72) | 6.64 (6.57-6.70) | 4.30 (3.87-4.74) |
| default | p50 service time (ms) | 1.74 (1.70-1.77) | 4.50 (4.49-4.51) | 3.61 (3.40-3.81) |
| term | p50 service time (ms) | 0.80 (0.69-0.92) | 2.86 (2.84-2.87) | 2.37 (2.05-2.69) |
| phrase | p50 service time (ms) | 0.84 (0.83-0.86) | 2.86 (2.81-2.90) | 2.52 (2.27-2.77) |
| country_agg_uncached | p50 service time (ms) | 23.37 (22.71-24.04) | 27.60 (27.36-27.85) | 39.82 (38.88-40.76) |
| country_agg_cached | p50 service time (ms) | 1.32 (1.28-1.36) | 3.42 (3.37-3.47) | 3.14 (3.05-3.23) |
| scroll | p50 service time (ms) | 479.18 (472.25-486.12) | 568.92 (547.15-590.69) | 526.81 (511.78-541.84) |
| expression | p50 service time (ms) | 58.86 (58.73-58.99) | 71.30 (67.53-75.07) | 81.98 (78.43-85.54) |
| painless_static | p50 service time (ms) | 83.95 (78.01-89.88) | 89.85 (85.53-94.16) | 91.93 (84.21-99.66) |
| painless_dynamic | p50 service time (ms) | 76.49 (58.59-94.40) | 78.31 (74.38-82.23) | 94.16 (90.15-98.17) |
| decay_geo_gauss_function_score | p50 service time (ms) | 62.62 (60.93-64.31) | 66.90 (62.75-71.05) | 72.99 (71.85-74.13) |
| decay_geo_gauss_script_score | p50 service time (ms) | 79.10 (73.79-84.42) | 70.72 (68.46-72.97) | 88.57 (77.82-99.32) |
| field_value_function_score | p50 service time (ms) | 14.46 (12.07-16.86) | 25.77 (24.66-26.88) | 28.66 (27.67-29.65) |
| field_value_script_score | p50 service time (ms) | 46.78 (46.22-47.34) | 41.27 (39.07-43.47) | 42.07 (40.94-43.21) |
| large_terms | p50 service time (ms) | 105.47 (97.43-113.52) | 144.34 (135.32-153.36) | 196.33 (191.65-201.01) |
| large_filtered_terms | p50 service time (ms) | 118.75 (111.06-126.44) | 151.69 (141.53-161.84) | 218.25 (172.10-264.40) |
| large_prohibited_terms | p50 service time (ms) | 107.68 (105.08-110.27) | 155.87 (146.78-164.96) | 203.94 (188.57-219.30) |
| desc_sort_population | p50 service time (ms) | 4.13 (3.99-4.28) | 6.37 (5.86-6.88) | 6.54 (6.33-6.74) |
| desc_sort_population_can_match_shortcut | p50 service time (ms) | 4.23 (3.99-4.47) | 6.41 (5.99-6.83) | 8.11 (7.11-9.11) |
| desc_sort_population_no_can_match_shortcut | p50 service time (ms) | 3.61 (3.51-3.72) | 6.64 (6.47-6.81) | 7.99 (7.12-8.86) |
| asc_sort_population | p50 service time (ms) | 3.96 (3.94-3.98) | 4.56 (4.45-4.68) | 4.58 (4.31-4.86) |
| asc_sort_with_after_population | p50 service time (ms) | 3.90 (3.64-4.16) | 5.47 (5.20-5.74) | 5.02 (5.00-5.03) |
| desc_sort_geonameid | p50 service time (ms) | 5.79 (5.45-6.13) | 6.69 (5.23-8.15) | 8.31 (8.06-8.57) |
| desc_sort_with_after_geonameid | p50 service time (ms) | 6.33 (5.60-7.06) | 7.60 (7.25-7.96) | 8.11 (8.03-8.19) |
| asc_sort_geonameid | p50 service time (ms) | 4.61 (4.50-4.72) | 4.77 (4.16-5.37) | 5.70 (5.54-5.85) |
| asc_sort_with_after_geonameid | p50 service time (ms) | 3.38 (2.90-3.87) | 6.29 (5.90-6.69) | 5.45 (5.34-5.57) |
| sort_country_code_can_match_shortcut | p50 service time (ms) | 4.85 (4.30-5.41) | 7.68 (6.68-8.67) | 9.25 (7.66-10.85) |
| sort_country_code_no_can_match_shortcut | p50 service time (ms) | 5.72 (4.85-6.59) | 7.48 (6.01-8.95) | 9.30 (7.69-10.91) |

`nyc_taxis` (`RALLY_DOCS=2000000`: the first 2M trips, one shard), all three targets (two races
each, as geonames), on the binaries before the date histogram work below. roll indexes fastest
(49.7k docs/s against 45.8k and 40.6k) and wins the searches 1.3-8x, but loses every
aggregation: `date_histogram_agg` 46.3 ms against 10.8 and 8.9, `autohisto_agg` 109.0 against
32.2 and 13.0 (OpenSearch counts it from the points tree), `distance_amount_agg` 161.6 against
86.3 and 144.1. RSS at the end: roll 198-361 MiB, Elasticsearch 2,502-2,629 MiB, OpenSearch
2,682-2,692 MiB. Fixed since, measured on the same 2M documents (tuning log): date histograms
count from the points tree (`date_histogram_agg` 65.9 -> 3.4 ms), `auto_date_histogram` rounds
through a prepared array, constant-score queries collect in blocks and numeric histograms cache
bucket ordinals (`autohisto_agg` 138 -> 65-74 ms, `distance_amount_agg` 256 -> 194 ms on the
loaded host, where Rally's service times are about 0.6x these).

| task | metric | roll | es | os |
|---|---|---:|---:|---:|
| index | median throughput (docs/s) | 49696.77 (47460.11-51933.43) | 45766.77 (33557.60-57975.95) | 40594.88 (35717.17-45472.59) |
| index | p50 service time (ms) | 1557.29 (1474.58-1639.99) | 1229.05 (1189.13-1268.97) | 1474.31 (1440.62-1507.99) |
| default_no_target | p50 service time (ms) | 1.15 (1.09-1.21) | 9.37 (6.22-12.51) | 7.30 (6.44-8.15) |
| default_1k | p50 service time (ms) | 10.71 (10.01-11.41) | 17.98 (14.61-21.35) | 21.79 (19.77-23.80) |
| range | p50 service time (ms) | 5.39 (5.33-5.45) | 10.26 (7.65-12.87) | 7.23 (6.54-7.93) |
| distance_amount_agg | p50 service time (ms) | 161.64 (152.04-171.24) | 86.33 (77.99-94.67) | 144.14 (131.36-156.91) |
| autohisto_agg | p50 service time (ms) | 109.00 (77.63-140.37) | 32.20 (29.22-35.18) | 12.99 (7.87-18.11) |
| date_histogram_agg | p50 service time (ms) | 46.27 (45.35-47.19) | 10.82 (10.31-11.34) | 8.88 (7.72-10.05) |

`http_logs`, all three targets (two races each). roll indexes at 140k docs/s against 94k and 95k
and beats Elasticsearch on every task but `sort-status-asc-after-force-merge-1-seg` (a tie,
5.26 against 5.11 ms). OpenSearch wins two: `terms_enum` (0.57 against 1.01 ms) and
`hourly_agg` (14.0 against 72.7 ms: its filter rewrite counts the hourly buckets from the points
tree; roll's date histograms do the same now, measured below). RSS at the end: roll 330-348 MiB,
Elasticsearch 2,338-2,756 MiB, OpenSearch 2,682-2,709 MiB.

| task | metric | roll | es | os |
|---|---|---:|---:|---:|
| index-append | median throughput (docs/s) | 139566.64 (130688.58-148444.71) | 93681.85 (91171.81-96191.90) | 95421.78 (94317.75-96525.81) |
| index-append | p50 service time (ms) | 260.19 (259.37-261.02) | 360.05 (358.40-361.69) | 385.83 (368.82-402.83) |
| default | p50 service time (ms) | 2.27 (2.21-2.33) | 5.68 (5.55-5.81) | 4.83 (4.55-5.12) |
| term | p50 service time (ms) | 2.64 (2.63-2.65) | 5.43 (5.16-5.70) | 5.04 (4.93-5.16) |
| terms_enum | p50 service time (ms) | 1.01 (0.86-1.16) | 3.43 (3.40-3.45) | 0.57 (0.41-0.73) |
| range | p50 service time (ms) | 1.31 (1.23-1.40) | 3.54 (3.41-3.67) | 3.21 (3.15-3.28) |
| 200s-in-range | p50 service time (ms) | 8.05 (7.03-9.08) | 10.18 (9.82-10.54) | 9.38 (8.48-10.29) |
| 400s-in-range | p50 service time (ms) | 2.20 (2.00-2.40) | 6.22 (6.20-6.23) | 6.15 (6.03-6.26) |
| hourly_agg | p50 service time (ms) | 72.68 (63.50-81.86) | 121.10 (120.82-121.38) | 14.00 (9.99-18.01) |
| scroll | p50 service time (ms) | 447.67 (446.29-449.05) | 489.04 (472.52-505.56) | 483.48 (466.32-500.63) |
| desc_sort_timestamp | p50 service time (ms) | 9.44 (7.45-11.43) | 14.97 (14.35-15.59) | 13.33 (12.78-13.88) |
| asc_sort_timestamp | p50 service time (ms) | 5.81 (5.59-6.03) | 12.42 (12.00-12.85) | 8.90 (8.31-9.50) |
| desc_sort_with_after_timestamp | p50 service time (ms) | 7.12 (7.08-7.17) | 14.17 (12.66-15.68) | 7.47 (7.45-7.50) |
| asc_sort_with_after_timestamp | p50 service time (ms) | 1.82 (1.72-1.92) | 6.14 (5.95-6.32) | 4.59 (4.53-4.65) |
| desc_sort_timestamp_can_match_shortcut | p50 service time (ms) | 9.62 (6.98-12.26) | 12.45 (12.19-12.72) | 21.01 (20.44-21.59) |
| desc_sort_timestamp_no_can_match_shortcut | p50 service time (ms) | 9.53 (6.55-12.51) | 22.09 (21.60-22.57) | 22.96 (22.45-23.47) |
| sort_size_desc | p50 service time (ms) | 6.21 (5.87-6.56) | 7.34 (7.26-7.41) | 6.84 (6.34-7.33) |
| sort_size_asc | p50 service time (ms) | 3.88 (3.70-4.05) | 6.75 (6.23-7.27) | 6.19 (5.72-6.65) |
| sort_status_desc | p50 service time (ms) | 5.34 (4.93-5.75) | 7.51 (6.48-8.54) | 7.06 (6.93-7.19) |
| sort_status_asc | p50 service time (ms) | 5.95 (5.17-6.73) | 7.02 (6.83-7.22) | 6.53 (6.38-6.67) |
| sort_keyword_can_match_shortcut | p50 service time (ms) | 4.02 (3.62-4.43) | 6.67 (6.54-6.80) | 6.63 (6.60-6.67) |
| sort_keyword_no_can_match_shortcut | p50 service time (ms) | 4.69 (4.59-4.78) | 5.97 (5.65-6.29) | 6.13 (6.03-6.23) |
| sort_numeric_can_match_shortcut | p50 service time (ms) | 5.63 (5.49-5.77) | 8.23 (7.79-8.67) | 8.85 (8.28-9.41) |
| sort_numeric_no_can_match_shortcut | p50 service time (ms) | 4.99 (4.75-5.23) | 7.65 (7.41-7.88) | 8.37 (7.64-9.10) |
| desc-sort-timestamp-after-force-merge-1-seg | p50 service time (ms) | 6.11 (5.67-6.56) | 18.19 (10.53-25.85) | 14.72 (12.12-17.33) |
| asc-sort-timestamp-after-force-merge-1-seg | p50 service time (ms) | 6.72 (6.32-7.13) | 6.79 (6.05-7.54) | 7.08 (5.69-8.48) |
| desc-sort-with-after-timestamp-after-force-merge-1-seg | p50 service time (ms) | 6.13 (6.06-6.20) | 17.53 (9.31-25.74) | 6.77 (6.26-7.28) |
| asc-sort-with-after-timestamp-after-force-merge-1-seg | p50 service time (ms) | 1.44 (1.42-1.47) | 4.84 (4.78-4.89) | 4.62 (4.37-4.87) |
| sort-size-desc-after-force-merge-1-seg | p50 service time (ms) | 5.41 (5.20-5.61) | 9.26 (8.17-10.35) | 6.66 (6.22-7.11) |
| sort-size-asc-after-force-merge-1-seg | p50 service time (ms) | 5.12 (5.07-5.18) | 6.50 (5.41-7.58) | 6.42 (6.01-6.83) |
| sort-status-desc-after-force-merge-1-seg | p50 service time (ms) | 4.10 (4.08-4.12) | 6.89 (5.90-7.88) | 7.03 (6.41-7.66) |
| sort-status-asc-after-force-merge-1-seg | p50 service time (ms) | 5.26 (4.88-5.64) | 5.11 (4.99-5.23) | 6.31 (5.70-6.92) |

**Rally after the aggregation work** (roll only, two rounds of each track on a binary with the
date histogram counting, the `auto_date_histogram` array, block collection, numeric histogram
slots and the `stats` fast path, but not yet the Painless change; Elasticsearch and OpenSearch
columns are the races above). Measured while builds and YAML suites ran on other cores, so
differences under ~10% are noise. Where roll is still behind the best of the two:
`autohisto_agg` (39.6 ms against Elasticsearch's 32.2 and OpenSearch's 13.0: OpenSearch counts
it from the points tree; Elasticsearch's shard-side rounding choice depends on document order, so
roll can only count it in the opensearch dialect), `distance_amount_agg` (98.1 against 86.3),
http_logs `hourly_agg` (32.9 against OpenSearch's 14.0, down from 72.7) and `terms_enum` (1.14
against 0.57), geonames `decay_geo_gauss_script_score` (78.2 against 70.7). RSS at the end:
roll 145-394 MiB.

nyc_taxis:

| task | metric | roll | es | os |
|---|---|---:|---:|---:|
| index | median throughput (docs/s) | 47335.52 (46952.45-47718.59) | 45766.77 (33557.60-57975.95) | 40594.88 (35717.17-45472.59) |
| index | p50 service time (ms) | 1500.58 (1498.64-1502.52) | 1229.05 (1189.13-1268.97) | 1474.31 (1440.62-1507.99) |
| default_no_target | p50 service time (ms) | 1.25 (0.91-1.58) | 9.37 (6.22-12.51) | 7.30 (6.44-8.15) |
| default_1k | p50 service time (ms) | 12.97 (12.49-13.45) | 17.98 (14.61-21.35) | 21.79 (19.77-23.80) |
| range | p50 service time (ms) | 6.98 (6.96-6.99) | 10.26 (7.65-12.87) | 7.23 (6.54-7.93) |
| distance_amount_agg | p50 service time (ms) | 98.07 (89.32-106.81) | 86.33 (77.99-94.67) | 144.14 (131.36-156.91) |
| autohisto_agg | p50 service time (ms) | 39.57 (38.59-40.56) | 32.20 (29.22-35.18) | 12.99 (7.87-18.11) |
| date_histogram_agg | p50 service time (ms) | 8.09 (7.88-8.29) | 10.82 (10.31-11.34) | 8.88 (7.72-10.05) |

http_logs:

| task | metric | roll | es | os |
|---|---|---:|---:|---:|
| index-append | median throughput (docs/s) | 149038.21 (144669.65-153406.76) | 93681.85 (91171.81-96191.90) | 95421.78 (94317.75-96525.81) |
| index-append | p50 service time (ms) | 254.44 (246.96-261.92) | 360.05 (358.40-361.69) | 385.83 (368.82-402.83) |
| default | p50 service time (ms) | 2.45 (2.09-2.81) | 5.68 (5.55-5.81) | 4.83 (4.55-5.12) |
| term | p50 service time (ms) | 2.07 (1.88-2.26) | 5.43 (5.16-5.70) | 5.04 (4.93-5.16) |
| terms_enum | p50 service time (ms) | 1.14 (0.76-1.52) | 3.43 (3.40-3.45) | 0.57 (0.41-0.73) |
| range | p50 service time (ms) | 1.36 (1.29-1.43) | 3.54 (3.41-3.67) | 3.21 (3.15-3.28) |
| 200s-in-range | p50 service time (ms) | 9.14 (7.78-10.50) | 10.18 (9.82-10.54) | 9.38 (8.48-10.29) |
| 400s-in-range | p50 service time (ms) | 2.89 (2.72-3.05) | 6.22 (6.20-6.23) | 6.15 (6.03-6.26) |
| hourly_agg | p50 service time (ms) | 32.89 (31.02-34.76) | 121.10 (120.82-121.38) | 14.00 (9.99-18.01) |
| scroll | p50 service time (ms) | 445.20 (442.40-447.99) | 489.04 (472.52-505.56) | 483.48 (466.32-500.63) |
| desc_sort_timestamp | p50 service time (ms) | 7.40 (6.91-7.90) | 14.97 (14.35-15.59) | 13.33 (12.78-13.88) |
| asc_sort_timestamp | p50 service time (ms) | 6.42 (6.19-6.66) | 12.42 (12.00-12.85) | 8.90 (8.31-9.50) |
| desc_sort_with_after_timestamp | p50 service time (ms) | 8.08 (7.18-8.98) | 14.17 (12.66-15.68) | 7.47 (7.45-7.50) |
| asc_sort_with_after_timestamp | p50 service time (ms) | 1.70 (1.64-1.76) | 6.14 (5.95-6.32) | 4.59 (4.53-4.65) |
| desc_sort_timestamp_can_match_shortcut | p50 service time (ms) | 8.31 (7.96-8.65) | 12.45 (12.19-12.72) | 21.01 (20.44-21.59) |
| desc_sort_timestamp_no_can_match_shortcut | p50 service time (ms) | 7.98 (7.41-8.55) | 22.09 (21.60-22.57) | 22.96 (22.45-23.47) |
| sort_size_desc | p50 service time (ms) | 7.26 (6.14-8.39) | 7.34 (7.26-7.41) | 6.84 (6.34-7.33) |
| sort_size_asc | p50 service time (ms) | 5.57 (5.23-5.90) | 6.75 (6.23-7.27) | 6.19 (5.72-6.65) |
| sort_status_desc | p50 service time (ms) | 6.19 (4.86-7.52) | 7.51 (6.48-8.54) | 7.06 (6.93-7.19) |
| sort_status_asc | p50 service time (ms) | 5.53 (4.25-6.80) | 7.02 (6.83-7.22) | 6.53 (6.38-6.67) |
| sort_keyword_can_match_shortcut | p50 service time (ms) | 4.59 (4.07-5.12) | 6.67 (6.54-6.80) | 6.63 (6.60-6.67) |
| sort_keyword_no_can_match_shortcut | p50 service time (ms) | 5.68 (5.33-6.02) | 5.97 (5.65-6.29) | 6.13 (6.03-6.23) |
| sort_numeric_can_match_shortcut | p50 service time (ms) | 8.82 (5.81-11.83) | 8.23 (7.79-8.67) | 8.85 (8.28-9.41) |
| sort_numeric_no_can_match_shortcut | p50 service time (ms) | 6.62 (6.53-6.71) | 7.65 (7.41-7.88) | 8.37 (7.64-9.10) |
| desc-sort-timestamp-after-force-merge-1-seg | p50 service time (ms) | 5.47 (5.15-5.79) | 18.19 (10.53-25.85) | 14.72 (12.12-17.33) |
| asc-sort-timestamp-after-force-merge-1-seg | p50 service time (ms) | 5.96 (5.51-6.41) | 6.79 (6.05-7.54) | 7.08 (5.69-8.48) |
| desc-sort-with-after-timestamp-after-force-merge-1-seg | p50 service time (ms) | 6.78 (6.43-7.13) | 17.53 (9.31-25.74) | 6.77 (6.26-7.28) |
| asc-sort-with-after-timestamp-after-force-merge-1-seg | p50 service time (ms) | 1.55 (1.51-1.58) | 4.84 (4.78-4.89) | 4.62 (4.37-4.87) |
| sort-size-desc-after-force-merge-1-seg | p50 service time (ms) | 5.49 (4.93-6.06) | 9.26 (8.17-10.35) | 6.66 (6.22-7.11) |
| sort-size-asc-after-force-merge-1-seg | p50 service time (ms) | 4.26 (4.23-4.30) | 6.50 (5.41-7.58) | 6.42 (6.01-6.83) |
| sort-status-desc-after-force-merge-1-seg | p50 service time (ms) | 4.97 (4.37-5.56) | 6.89 (5.90-7.88) | 7.03 (6.41-7.66) |
| sort-status-asc-after-force-merge-1-seg | p50 service time (ms) | 4.19 (3.96-4.41) | 5.11 (4.99-5.23) | 6.31 (5.70-6.92) |

geonames:

| task | metric | roll | es | os |
|---|---|---:|---:|---:|
| index-append | median throughput (docs/s) | 59895.71 (48677.46-71113.95) | 28085.74 (26573.43-29598.04) | 28683.16 (24578.01-32788.31) |
| index-append | p50 service time (ms) | 714.47 (558.24-870.70) | 900.07 (864.09-936.06) | 989.34 (972.92-1005.76) |
| index-stats | median throughput (docs/s) | 348.56 (315.09-382.04) | 178.15 (165.76-190.54) | 213.85 (190.66-237.04) |
| index-stats | p50 service time (ms) | 1.67 (1.60-1.75) | 3.29 (3.16-3.42) | 2.72 (2.48-2.96) |
| node-stats | p50 service time (ms) | 1.87 (1.78-1.96) | 6.64 (6.57-6.70) | 4.30 (3.87-4.74) |
| default | p50 service time (ms) | 2.31 (2.26-2.37) | 4.50 (4.49-4.51) | 3.61 (3.40-3.81) |
| term | p50 service time (ms) | 0.95 (0.69-1.20) | 2.86 (2.84-2.87) | 2.37 (2.05-2.69) |
| phrase | p50 service time (ms) | 1.05 (0.80-1.30) | 2.86 (2.81-2.90) | 2.52 (2.27-2.77) |
| country_agg_uncached | p50 service time (ms) | 25.48 (20.40-30.56) | 27.60 (27.36-27.85) | 39.82 (38.88-40.76) |
| country_agg_cached | p50 service time (ms) | 1.61 (1.50-1.73) | 3.42 (3.37-3.47) | 3.14 (3.05-3.23) |
| scroll | p50 service time (ms) | 483.73 (473.49-493.97) | 568.92 (547.15-590.69) | 526.81 (511.78-541.84) |
| expression | p50 service time (ms) | 60.77 (51.59-69.95) | 71.30 (67.53-75.07) | 81.98 (78.43-85.54) |
| painless_static | p50 service time (ms) | 79.77 (79.65-79.90) | 89.85 (85.53-94.16) | 91.93 (84.21-99.66) |
| painless_dynamic | p50 service time (ms) | 79.31 (61.55-97.07) | 78.31 (74.38-82.23) | 94.16 (90.15-98.17) |
| decay_geo_gauss_function_score | p50 service time (ms) | 45.76 (43.83-47.70) | 66.90 (62.75-71.05) | 72.99 (71.85-74.13) |
| decay_geo_gauss_script_score | p50 service time (ms) | 78.24 (76.57-79.90) | 70.72 (68.46-72.97) | 88.57 (77.82-99.32) |
| field_value_function_score | p50 service time (ms) | 14.50 (11.11-17.89) | 25.77 (24.66-26.88) | 28.66 (27.67-29.65) |
| field_value_script_score | p50 service time (ms) | 42.26 (39.32-45.21) | 41.27 (39.07-43.47) | 42.07 (40.94-43.21) |
| large_terms | p50 service time (ms) | 102.92 (81.33-124.52) | 144.34 (135.32-153.36) | 196.33 (191.65-201.01) |
| large_filtered_terms | p50 service time (ms) | 98.00 (89.16-106.84) | 151.69 (141.53-161.84) | 218.25 (172.10-264.40) |
| large_prohibited_terms | p50 service time (ms) | 102.30 (87.42-117.19) | 155.87 (146.78-164.96) | 203.94 (188.57-219.30) |
| desc_sort_population | p50 service time (ms) | 3.79 (3.29-4.29) | 6.37 (5.86-6.88) | 6.54 (6.33-6.74) |
| desc_sort_population_can_match_shortcut | p50 service time (ms) | 4.22 (3.80-4.64) | 6.41 (5.99-6.83) | 8.11 (7.11-9.11) |
| desc_sort_population_no_can_match_shortcut | p50 service time (ms) | 3.70 (3.16-4.24) | 6.64 (6.47-6.81) | 7.99 (7.12-8.86) |
| asc_sort_population | p50 service time (ms) | 3.12 (2.80-3.43) | 4.56 (4.45-4.68) | 4.58 (4.31-4.86) |
| asc_sort_with_after_population | p50 service time (ms) | 3.29 (3.07-3.52) | 5.47 (5.20-5.74) | 5.02 (5.00-5.03) |
| desc_sort_geonameid | p50 service time (ms) | 4.86 (4.22-5.50) | 6.69 (5.23-8.15) | 8.31 (8.06-8.57) |
| desc_sort_with_after_geonameid | p50 service time (ms) | 5.27 (4.83-5.71) | 7.60 (7.25-7.96) | 8.11 (8.03-8.19) |
| asc_sort_geonameid | p50 service time (ms) | 3.81 (3.60-4.02) | 4.77 (4.16-5.37) | 5.70 (5.54-5.85) |
| asc_sort_with_after_geonameid | p50 service time (ms) | 2.83 (2.48-3.17) | 6.29 (5.90-6.69) | 5.45 (5.34-5.57) |
| sort_country_code_can_match_shortcut | p50 service time (ms) | 5.49 (5.39-5.59) | 7.68 (6.68-8.67) | 9.25 (7.66-10.85) |
| sort_country_code_no_can_match_shortcut | p50 service time (ms) | 5.42 (5.14-5.70) | 7.48 (6.01-8.95) | 9.30 (7.69-10.91) |

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

**Still open** (performance): the two geonames script scores above. (Fetching hits decompressed a
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
