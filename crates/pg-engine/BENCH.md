# pg-engine benchmarks

The engine against PostgreSQL 18.6 on identical workloads: over the wire for both (a fair
comparison: same client, same protocol), and the engine in-process as roll embeds it. The
harness is `tools/bench/`; this file has the method, the latest numbers and what is still
slower and why. Correctness stays first: every optimisation below kept `cargo test -p
pg-engine`, clippy and a full regression run unit for unit (see "Verification").

## Harness

- `tools/bench/bench.rs` (`cargo build --release -p pg-engine --example bench`): the
  workloads, against a server (`--target wire --port N`, tokio-postgres, the extended
  protocol with prepared statements for OLTP, simple queries for the rest) or in-process
  (`--target inproc --data <dir>`: `Engine::open`, `Engine::connect`, `Session` with
  `parse_message` / `bind_message` / `execute_portal` as a driver would). Prints `RESULT`
  lines: throughput and latency percentiles (p50/p95/p99), peak memory, per-query min /
  median / max and a hash of the result rows (the engine's rows are checked against
  PostgreSQL's while being timed).
- `tools/bench/tpch.rs`: a deterministic TPC-H-like generator (dbgen's schema, row counts,
  key formulas and value distributions closely enough for the 22 queries to select and join
  what they do; streamed through COPY in 100k-row chunks) and the 22 queries with the
  specification's validation parameters (Q15's view as a CTE).
- `tools/bench/run.sh`: starts the reference (`pg-start`, container `a36-pg`) and the engine
  server (`engine-start`), runs pgbench from the pinned image on the host network, the
  bench client, the in-process runs, disk and memory probes. `tools/bench/suite.sh <pg|engine|inproc>
  <label>` runs everything; `tools/bench/report.py <dir> <labels...>` makes the tables
  below. `tools/bench/perf.sh` profiles a running engine with perf from a privileged
  helper container (the host has no perf and keeps perf_event from users).
- Workloads:
  - **pgbench** (PostgreSQL's own client): TPC-B-like read/write and select-only (`-S`),
    scale 1 (100k accounts) and 10 (1M), 1 / 8 / 32 clients, `-M prepared`, 15 s each,
    data from `pgbench -i -I dtGvp` (server-side generate).
  - **OLTP mix** (1M-row table with a secondary index): point select (prepared and as a
    simple query), 100-row range scan, secondary-index lookup, insert, indexed update,
    upsert (`ON CONFLICT DO UPDATE`), a 3-statement transaction, `SELECT 1`; 1 and 8
    clients, 8 s each.
  - **Analytic** at 200k rows (a table small enough for the engine to model its heap) and
    2M (too big to model): filtered scan, full float and text sorts, top-N, hash aggregate,
    `count(DISTINCT)`, hash join + aggregate, window functions, index range scan, numeric
    sum; and **TPC-H** at SF1 (6M lineitems), each query 3 times.
  - **Bulk** at 100k and 1M rows: COPY in, CREATE INDEX (plain and unique), COPY out,
    INSERT … SELECT, UPDATE of half the rows, DELETE of a quarter, VACUUM, ANALYZE, a
    count after.
  - **Streaming** (`stream-load --rows 10000000`, `stream`): a 10M-row SELECT and COPY TO
    read as they come: time to the first row, the whole time, the server's peak memory.
  - **DDL and catalogs**: 500 tables (serial key, check, default, index, added column),
    `information_schema.columns` / `.tables`, psql's `\d` queries, a `pg_class` ×
    `pg_attribute` join, an ORM-style `pg_constraint` query, a `regclass` cast, DROP.
  - **Startup and idle**: connection setup, memory per idle session (100 sessions) and per
    database (10 databases with a table and a session each), `Engine::open` and the first
    query on a database of TPC-H SF1 (in-process).

## Setup and fairness

- Machine: 36 cores, 125 GB RAM, SATA SSDs in RAID 0, shared with other agents' builds
  (load 5–15 during the runs). Both servers are pinned to the same 4 cores (28–31: the
  engine with `taskset`, PostgreSQL with `docker --cpuset-cpus`), every client to 4 others
  (32–35); in-process runs get both sets (the client is in the process). Each number is the
  best (throughput) or median (time) of the repeats given; differences under ~10% are noise.
- PostgreSQL 18.6, the pinned image (`postgres@sha256:77f5851…`, `--locale=C`), data on a
  docker volume (disk): `shared_buffers=2GB`, `maintenance_work_mem=256MB`,
  `max_wal_size=8GB`, `checkpoint_timeout=30min`, `max_connections=300`, `fsync` and
  `synchronous_commit` on, parallel query at its defaults (2 workers per gather). Settings
  that change plans (`work_mem`, `effective_cache_size`, `random_page_cost`, …) stay at the
  defaults the engine also has, so both run the same plans.
- The engine: `roll-engine --cache-mb 2048` (the block cache, as shared_buffers), WAL fsync
  on commit (the default), data in `~/.cache/a36/engine` on the same disk.
- Memory: PostgreSQL's is the container cgroup's `anon + shmem` (backends' private memory
  plus the touched shared buffers); the engine's is the process RSS.

Reproduce (from `crates/pg-engine`; release builds through `tools/heavy.sh`):

```sh
tools/heavy.sh cargo build --release -p pg-engine --bin roll-engine --example bench
tools/bench/run.sh pg-start && tools/bench/suite.sh pg pg            # ~1 h
tools/bench/run.sh engine-start && SUITE=pgbench,oltp,analytic,tpch,bulk,ddl,idle \
  tools/bench/suite.sh engine engine                                  # ~1.5 h
tools/bench/run.sh engine-stop && SUITE=oltp,ddl,tpch tools/bench/suite.sh inproc inproc
python3 tools/bench/report.py ~/.cache/a36/results pg engine inproc
tools/bench/run.sh pg-stop
```

`BENCH_BIN` / `BENCH_CLIENT` pick other engine / client binaries (a baseline copied aside),
`BENCH_DATA` another data directory, `SERVER_CPUS` / `CLIENT_CPUS` / `PG_PORT` /
`ENGINE_PORT` the pinning and ports, `CACHE_MB` the engine's block cache (2048).

## Results (2026-10-24)

Columns: **pg** PostgreSQL 18.6; **before** the engine at `b1917d6` (this round's start);
**after** the engine with this round's changes, over the wire; **in-proc** the same engine
embedded (the client in the process: no sockets, no protocol encoding). Ratios are against
PostgreSQL: above 1x the engine is faster (throughput, or PostgreSQL's time over the
engine's). Engine rows agree with PostgreSQL's in every query the harness hashes (all of
TPC-H; the analytic set), except two float sums (`hash_agg`, `hash_join`) where PostgreSQL ran
a parallel plan and added its workers' partial sums in another order: the engine's sums equal
PostgreSQL's with `max_parallel_workers_per_gather = 0` digit for digit.

### pgbench (TPS, `-M prepared`, 15 s)

| case | clients | pg | before | after | after / pg |
|---|---|---|---|---|---|
| select-only, scale 1 | 1 | 5,935 | 2,961 | 14,096 | 2.38x |
| select-only, scale 1 | 8 | 57,741 | 12,310 | 63,374 | 1.10x |
| select-only, scale 1 | 32 | 56,844 | 21,373 | 74,938 | 1.32x |
| TPC-B, scale 1 | 1 | 929 | 183 | 1,963 | 2.11x |
| TPC-B, scale 1 | 8 | 2,680 | 239 | 4,721 | 1.76x |
| TPC-B, scale 1 | 32 | 2,171 | 188 | 3,117 | 1.44x |
| select-only, scale 10 | 1 | 6,070 | 0.9 | 15,569 | 2.56x |
| select-only, scale 10 | 8 | 51,688 | 4.0 | 70,616 | 1.37x |
| select-only, scale 10 | 32 | 70,410 | 3.6 | 101,685 | 1.44x |
| TPC-B, scale 10 | 1 | 1,009 | 0.6 | 1,776 | 1.76x |
| TPC-B, scale 10 | 8 | 6,944 | 2.2 | 5,999 | 0.86x |
| TPC-B, scale 10 | 32 | 7,408 | 1.8 | 7,060 | 0.95x |

(Before, scale 10's 1M accounts were too big to model, had no size, and every lookup was a
sequential scan: optimisation 1.)

### OLTP mix (TPS, 1M-row table with a secondary index, 8 s)

| case | clients | pg | before | after | after / pg | in-proc | in-proc / pg |
|---|---|---|---|---|---|---|---|
| point select (prepared) | 1 | 5,802 | 1 | 11,202 | 1.93x | 73,608 | 12.7x |
| point select (simple query) | 1 | 4,812 | 1 | 5,431 | 1.13x | 10,856 | 2.26x |
| range scan, 100 rows | 1 | 3,108 | 1 | 1,299 | 0.42x | 1,735 | 0.56x |
| secondary-index lookup | 1 | 4,695 | 1 | 4,846 | 1.03x | 9,627 | 2.05x |
| insert | 1 | 5,226 | 1,221 | 7,613 | 1.46x | 15,933 | 3.05x |
| indexed update | 1 | 4,904 | 1 | 7,859 | 1.60x | 14,732 | 3.00x |
| upsert | 1 | 4,394 | 2,210 | 7,957 | 1.81x | 15,657 | 3.56x |
| 3-statement transaction | 1 | 1,183 | 0 | 2,276 | 1.92x | 8,900 | 7.52x |
| `SELECT 1` | 1 | 9,492 | 5,011 | 15,658 | 1.65x | 255,786 | 26.9x |
| point select (prepared) | 8 | 59,189 | 1 | 75,427 | 1.27x | 236,386 | 3.99x |
| point select (simple query) | 8 | 34,945 | 1 | 21,936 | 0.63x | 59,988 | 1.72x |
| range scan, 100 rows | 8 | 24,026 | 1 | 4,124 | 0.17x | 9,213 | 0.38x |
| secondary-index lookup | 8 | 37,862 | 2 | 14,371 | 0.38x | 34,344 | 0.91x |
| insert | 8 | 32,093 | 3,496 | 19,804 | 0.62x | 44,916 | 1.40x |
| indexed update | 8 | 22,315 | 3 | 17,596 | 0.79x | 33,762 | 1.51x |
| upsert | 8 | 21,888 | 4,749 | 17,337 | 0.79x | 40,572 | 1.85x |
| 3-statement transaction | 8 | 10,230 | 1 | 7,829 | 0.77x | 17,469 | 1.71x |
| `SELECT 1` | 8 | 89,919 | 86,951 | 133,417 | 1.48x | 345,234 | 3.84x |

Latency percentiles are in the raw results (`RESULT` lines: p50 / p95 / p99); e.g. prepared
point select at 8 clients: PostgreSQL p50 0.123 ms / p99 0.279 ms, the engine 0.092 / 0.289.

### Analytic (median of 3, seconds)

| query | rows | pg | before | after | after / pg |
|---|---|---|---|---|---|
| filtered scan | 200k | 0.017 | 0.617 | 0.149 | 0.12x |
| full sort, float | 200k | 0.083 | 0.990 | 0.323 | 0.26x |
| full sort, text | 200k | 0.084 | 1.274 | 0.453 | 0.19x |
| top-100 | 200k | 0.026 | 0.527 | 0.153 | 0.17x |
| hash aggregate (1000 groups) | 200k | 0.031 | 0.645 | 0.230 | 0.14x |
| count(DISTINCT) | 200k | 0.047 | 0.809 | 0.388 | 0.12x |
| hash join + aggregate | 200k | 0.078 | 1.017 | 0.435 | 0.18x |
| window functions | 200k | 0.156 | 1.132 | 0.613 | 0.25x |
| index range (49k rows) | 200k | 0.011 | 0.320 | 0.151 | 0.07x |
| numeric sum / avg | 200k | 0.017 | 0.343 | 0.123 | 0.14x |
| filtered scan | 2M | 0.122 | 3.125 | 1.365 | 0.09x |
| full sort, float | 2M | 1.020 | 10.18 | 3.265 | 0.31x |
| full sort, text | 2M | 0.911 | 15.68 | 5.357 | 0.17x |
| top-100 | 2M | 0.123 | 3.423 | 1.443 | 0.09x |
| hash aggregate | 2M | 0.182 | 4.401 | 2.032 | 0.09x |
| count(DISTINCT) | 2M | 0.479 | 9.262 | 4.014 | 0.12x |
| hash join + aggregate | 2M | 0.477 | 99.83 | 3.147 | 0.15x |
| window functions | 2M | 1.698 | 18.98 | 7.998 | 0.21x |
| index range (49k rows) | 2M | 0.009 | 5.028 | 0.102 | 0.09x |
| numeric sum / avg | 2M | 0.101 | 2.880 | 1.275 | 0.08x |

### TPC-H, SF1 (median of 3, seconds; in-process: one run, the first after opening)

| query | pg | after | after / pg | in-proc |
|---|---|---|---|---|
| q01 | 1.81 | 18.36 | 0.10x | 30.57 |
| q02 | 0.21 | 1.42 | 0.15x | 2.46 |
| q03 | 0.38 | 8.37 | 0.05x | 9.34 |
| q04 | 0.13 | 6.97 | 0.02x | 6.72 |
| q05 | 0.25 | 3.09 | 0.08x | 2.83 |
| q06 | 0.35 | 6.19 | 0.06x | 5.63 |
| q07 | 0.32 | 5.41 | 0.06x | 4.44 |
| q08 | 0.15 | 1.36 | 0.11x | 1.34 |
| q09 | 1.14 | 11.44 | 0.10x | 10.35 |
| q10 | 0.55 | 8.79 | 0.06x | 7.56 |
| q11 | 0.08 | 0.39 | 0.21x | 0.31 |
| q12 | 0.45 | 10.64 | 0.04x | 8.88 |
| q13 | 0.65 | 6.30 | 0.10x | 5.20 |
| q14 | 0.14 | 6.71 | 0.02x | 5.96 |
| q15 | 0.28 | 6.28 | 0.05x | 5.76 |
| q16 | 0.21 | 3.05 | 0.07x | 2.61 |
| q17 | 0.77 | 7.88 | 0.10x | 6.85 |
| q18 | 2.59 | 21.28 | 0.12x | 16.91 |
| q19 | 0.06 | 0.47 | 0.12x | 0.44 |
| q20 | 0.10 | 2.94 | 0.03x | 2.96 |
| q21 | 0.44 | 9.26 | 0.05x | 8.11 |
| q22 | 0.09 | 1.54 | 0.06x | 1.28 |
| load: COPY | 27.4 | 155.3 | 0.18x | |
| load: keys and indexes | n/a | 185.5 | | |
| load: VACUUM ANALYZE | 1.9 | 55.0 | | |

Every query's rows agree with PostgreSQL's (same row counts and result hashes). The
before-engine wasn't run at SF1: it planned every query over the 6M-row lineitem as if the
table were empty.

### Bulk (median of 3 at 100k rows, of 2 at 1M; seconds)

| step | 100k pg | 100k before | 100k after | 1M pg | 1M before | 1M after | 1M after / pg |
|---|---|---|---|---|---|---|---|
| COPY in | 0.083 | 2.032 | 0.770 | 0.761 | 16.52 | 11.52 | 0.07x |
| CREATE INDEX | 0.041 | 1.136 | 0.286 | 0.357 | 10.75 | 4.67 | 0.08x |
| CREATE UNIQUE INDEX | 0.022 | 1.651 | 0.443 | 0.245 | 11.85 | 4.26 | 0.06x |
| COPY out | 0.036 | 0.986 | 0.323 | 0.389 | 8.89 | 2.25 | 0.17x |
| INSERT … SELECT | 0.064 | 0.936 | 0.512 | 0.653 | 10.01 | 4.79 | 0.14x |
| UPDATE half | 0.331 | 1.408 | 0.916 | 4.101 | 10.55 | 4.83 | 0.85x |
| DELETE a quarter | 0.018 | 0.845 | 0.376 | 0.188 | 8.80 | 4.20 | 0.04x |
| VACUUM | 0.040 | 0.871 | 0.283 | 0.222 | 7.93 | 2.19 | 0.10x |
| ANALYZE | 0.050 | 1.021 | 0.230 | 0.097 | 8.47 | 2.00 | 0.05x |
| count(*) after | 0.005 | 0.224 | 0.075 | 0.041 | 1.57 | 0.59 | 0.07x |

### DDL and catalogs (500 tables; ms per statement)

| case | pg | before | after | after / pg | in-proc |
|---|---|---|---|---|---|
| CREATE TABLE (serial key, check, default) | 2.93 | 14.09 | 4.26 | 0.69x | 4.15 |
| CREATE INDEX | 0.93 | 23.84 | 9.10 | 0.10x | 8.27 |
| ALTER TABLE ADD COLUMN | 0.44 | 25.85 | 12.08 | 0.04x | 9.92 |
| information_schema.columns of a table | 2.87 | 98.81 | 20.17 | 0.14x | 16.10 |
| information_schema.tables count | 1.69 | 3.24 | 0.55 | 3.05x | 0.38 |
| psql `\d t`: lookup | 0.35 | 22.56 | 5.78 | 0.06x | 4.81 |
| psql `\d t`: columns | 0.32 | 139.1 | 13.42 | 0.02x | 11.38 |
| psql `\d t`: indexes | 0.50 | 57.33 | 7.30 | 0.07x | 5.95 |
| pg_class × pg_attribute join | 5.08 | 588.3 | 30.07 | 0.17x | 25.00 |
| ORM constraints query | 28.6 | 162.9 | 30.6 | 0.94x | 24.5 |
| `'t'::regclass` | 0.13 | 1.13 | 0.09 | 1.41x | 0.03 |
| DROP TABLE | 4.20 | 39.31 | 16.28 | 0.26x | 14.02 |

### Disk, memory, startup

| | pg | before | after |
|---|---|---|---|
| on disk, TPC-H SF1 with keys and indexes | 1,602 MB (+ WAL) | | 666 MB |
| on disk, the OLTP + pgbench + analytic database | 234 MB | | 170 MB |
| peak memory, TPC-H SF1 load (COPY, indexes, VACUUM ANALYZE) | 1.2 GB | | 18.1 GB |
| peak memory, bulk 1M | 2.1 GB | | 9.6 GB |
| memory per idle session (100 sessions, each after a catalog query) | 1.5 MB | 0.1–0.6 MB | 2.2–2.7 MB |
| connection setup (SCRAM) | 22.2 ms | 8.7 ms | 6.6–8.0 ms |
| `Engine::open` (in-process) | | 0.007 s | 0.0004 s |
| first `count(*)` of lineitem after opening (in-process) | | 11.5 s | 37–51 s |

(PostgreSQL's memory includes the shared buffers it touched; the engine's is its RSS. The
per-database figure the harness also reports is noise under mimalloc, which returns memory
to the system as databases close.)

## Optimisations (round 17, agent 36)

Each with the numbers measured just before and after it (engine over the wire unless said;
the runs between steps were on a busy machine, so read them as ratios, not absolutes).

1. **Heap sizes of tables too big to model** (`exec/sizing.rs` `EstHeap`). A table with more
   than 400,000 row versions isn't modelled page by page (`MAX_VERSIONS`), and its heap then
   had no size at all: the planner's current page count was 0, so every query over a big
   table costed 0 and became a sequential scan (`pgbench_accounts WHERE aid = $1` at scale
   10: `Seq Scan … cost=0.00..0.00`). Such a table's heap is now estimated as Postgres's
   file would grow (each version written goes after the last, VACUUM's free space reused
   first, fillfactor kept), read by the planner, VACUUM / ANALYZE relpages and
   `pg_relation_size`, and measured a row at a time (not held in memory). pgbench
   select-only, scale 10, 1 client: **2.2 → 14,700 tps**; the plan is PostgreSQL's (an
   index scan). Small tables (the regression suite's) are unaffected: they are modelled
   exactly as before.
2. **Text parameters of reg\* types** (`session/mod.rs` `bind_inner`): a Bind's text value
   for a `regclass` parameter was parsed without the catalog, so `pgbench -i` (`SELECT
   relkind FROM pg_class WHERE oid = $1::regclass`) failed with `"pgbench_branches" does not
   exist`. A correctness fix found by the harness (`tests/wire.rs`
   `text_parameters_of_reg_types`).
3. **Sequential scans** (`exec/scan.rs` `heap_order`, `storage` `Scan::next`,
   `sizing::note_read`): a scan put its rows in heap order by looking every row's key up in
   the heap model's hash map (cloning every key) and sorting; it now reads the TID each row
   is stored with and only sorts when the rows aren't in that order already (an
   insert-only table's are). The storage scan made two allocations per row (now one), and
   every row read was recorded for the compression model even in tables that never stored
   a compressed or out-of-line value (now skipped for them). 200k-row analytic queries:
   `seq_filter` 0.385 → 0.233 s, `sort_float` 0.717 → 0.477, `hash_agg` 0.525 → 0.315,
   `numeric_sum` 0.349 → 0.213; `SELECT count(*)` after a bulk load 0.225 → 0.058 s.
4. **get_actual_variable_range from the index's ends** (`exec/scan.rs`
   `column_range_by_index`, `storage::RevScan`, `Txn::scan_rev`): the planner's lookup of
   a column's current min / max (a range or inequality on an indexed column) read and
   decoded every row of the table; it now reads the first and last non-null entries of a
   btree index on the column, as PostgreSQL does, with a new descending storage scan.
   Planning `WHERE id BETWEEN 1000 AND 50000` over 200k rows: **163 ms → 1.3 ms**.
5. **Group commit** (`storage` `Store::commit`, `wait_durable`): a commit appended its WAL
   record and fsynced under the commit lock, so commits were serialised on fsync. Now the
   record and the memtable entries go in under the lock and the commit waits outside it
   for an fsync that one waiting commit does for all appended so far; a commit becomes
   visible with its fsync (in timestamp order), before it returns, as a transaction whose
   commit record isn't flushed isn't visible in PostgreSQL. DDL commits (a new catalog
   published with the data) keep the old path.
6. **Row versions skipped by seeking, and pruned** (`storage` `Scan::next`, `Cursor::seek`,
   `Scan::prune`): a hot row (pgbench_branches) has a version per update in the memtable,
   and every read of it walked all of them. A scan now seeks past a key's older versions
   (and past versions newer than its snapshot), and drops memtable versions older than the
   newest one no snapshot can need (`gc_horizon`; snapshots are now pinned atomically with
   reading the visible timestamp, `Store::pin_visible`). pgbench TPC-B scale 10, 8 / 32
   clients: **790 / ~900 → 6,090 / 6,640 tps** (with 5).
7. **mimalloc** as the server's allocator (`bin/roll-engine.rs`; embedders choose their
   own): TPC-B 1 client 975 → 1,330 tps; 200k analytic queries 1.3–2.0x faster
   (`sort_float` 0.477 → 0.254, `top_n` 0.339 → 0.151, `seq_filter` 0.233 → 0.131).
8. **A fast hasher for the executor's hash tables** (`exec/fasthash.rs`: hash joins,
   aggregates, DISTINCT, subquery caches, the heap model, the write buffer) instead of
   SipHash; nothing observable depends on their order (PostgreSQL's hash orders are
   modelled apart, `pghash`). `hash_agg` 0.241 → 0.180 s, `hash_join` 0.425 → 0.382.
9. **WAL zero-filled ahead** (`storage/wal.rs` `PREALLOC`): records are written into space
   already allocated (1 MB at a time), so a commit's fdatasync flushes data, not file-size
   metadata, as PostgreSQL's preallocated segments do. Replay stops at the zero length
   past the last record. TPC-B scale 10, 1 client (fsync-bound): 1,330 → 1,776 tps (with 10).
10. **One write-buffer map** (`txn` `Txn::writes`): a write's command id lived in a second
    hash map keyed by a second copy of the key; it is now in the buffer's entry.
11. **Catalog rescans** (`exec/nodes.rs` `sys_rescan`): the check that a catalog column's
    values all have the outer value's type ran over every row once per outer row
    (quadratic); now once per column and type. A `pg_class` × `pg_attribute` join over 500
    tables: **214 → 41 ms**.
12. **System catalogs' rows reused across a session's statements**
    (`catalog/system.rs` `SysRowsCache`): `pg_class`, `pg_attribute`, `pg_attrdef`,
    `pg_type`, `pg_namespace`, `pg_index`, `pg_constraint` and `pg_proc` are generated
    from the catalog for each statement; a session now keeps the last ones, keyed by the
    catalog (a `Weak`), current and session user, search path and the settings' generation
    (`Settings::generation`), so psql's `\d` and ORM introspection don't rebuild them per
    query (`tests/session_state.rs` `catalog_rows_follow_ddl_roles_and_settings`).
13. **Parse trees reused** (`sql/parse.rs` `cached_parse`): parsing is a pure function of
    the text and the parser options; the last 2,048 texts' trees (per thread) are kept,
    which the stored defaults, checks and view bodies deparsed for every catalog read, and
    a client's repeated simple queries, hit. With 12, over 500 tables: psql `\d t` column
    query **33 → 9.7 ms**, its table lookup 11.5 → 4.4, its index query 20 → 5.5, an
    ORM's `pg_constraint` query 49 → 23, `information_schema.columns` 23 → 13.
14. **Statistics slots parsed once** (`sql/costs.rs` `slot_values`): every estimate of a
    clause over a column parsed its histogram / MCV texts into values again (a custom plan
    per execution of a prepared range query did so each time); the last 64 parsed slots are
    kept (by settings generation, type and texts; plain scalar types only).

## Round 18 (agent 42): analytic queries

The executor's side of "Where it is still slower" item 1. Measured on the same machine with
the same harness and settings as above, but on cores 20–23 (both servers) and 24–27 (the
client), with the machine busier than in round 17 (load 25–40): PostgreSQL's own times are
up to 1.7x round 17's, so compare within these tables, not with the ones above. **pg** is
PostgreSQL 18.6 (container `a42-pg`, 2 workers per Gather); **before** the engine at this
round's start (`ca0c077`); **after** this round's engine; times are medians of 3 (seconds),
the best of two runs; **spread** is how far the two runs' medians differ.

### Analytic

| query | rows | pg | before | after | after / pg | after / before | spread |
|---|---|---|---|---|---|---|---|
| filtered scan | 200k | 0.025 | 0.225 | 0.109 | 0.23x | 2.1x | 48% |
| full sort, float | 200k | 0.099 | 0.386 | 0.182 | 0.54x | 2.1x | 28% |
| full sort, text | 200k | 0.112 | 0.584 | 0.204 | 0.55x | 2.9x | 4% |
| top-100 | 200k | 0.032 | 0.178 | 0.098 | 0.33x | 1.8x | 0% |
| hash aggregate (1000 groups) | 200k | 0.039 | 0.465 | 0.166 | 0.23x | 2.8x | 24% |
| count(DISTINCT) | 200k | 0.068 | 0.915 | 0.243 | 0.28x | 3.8x | 8% |
| hash join + aggregate | 200k | 0.120 | 0.581 | 0.436 | 0.28x | 1.3x | 4% |
| window functions | 200k | 0.227 | 0.876 | 0.450 | 0.51x | 1.9x | 5% |
| index range (49k rows) | 200k | 0.008 | 0.161 | 0.115 | 0.07x | 1.4x | 4% |
| numeric sum / avg | 200k | 0.026 | 0.205 | 0.095 | 0.27x | 2.2x | 3% |
| filtered scan | 2M | 0.134 | 2.030 | 0.254 | 0.53x | 8.0x | 48% |
| full sort, float | 2M | 1.122 | 5.783 | 1.953 | 0.57x | 3.0x | 40% |
| full sort, text | 2M | 1.011 | 7.777 | 2.546 | 0.40x | 3.1x | 72% |
| top-100 | 2M | 0.105 | 1.625 | 0.650 | 0.16x | 2.5x | 7% |
| hash aggregate (1000 groups) | 2M | 0.186 | 2.823 | 0.605 | 0.31x | 4.7x | 36% |
| count(DISTINCT) | 2M | 0.530 | 7.971 | 3.110 | 0.17x | 2.6x | 43% |
| hash join + aggregate | 2M | 0.406 | 3.751 | 2.241 | 0.18x | 1.7x | 97% |
| window functions | 2M | 1.857 | 11.76 | 5.005 | 0.37x | 2.3x | 60% |
| index range (49k rows) | 2M | 0.011 | 0.203 | 0.078 | 0.14x | 2.6x | 64% |
| numeric sum / avg | 2M | 0.095 | 1.968 | 0.290 | 0.33x | 6.8x | 7% |

### TPC-H SF1

| query | pg | before | after | after / pg | after / before | spread |
|---|---|---|---|---|---|---|
| q01 | 1.948 | 23.55 | 3.298 | 0.59x | 7.1x | 16% |
| q02 | 0.258 | 3.693 | 1.327 | 0.19x | 2.8x | 12% |
| q03 | 0.563 | 19.22 | 8.457 | 0.07x | 2.3x | 11% |
| q04 | 0.171 | 12.27 | 4.043 | 0.04x | 3.0x | 13% |
| q05 | 0.500 | 3.993 | 3.107 | 0.16x | 1.3x | 23% |
| q06 | 0.547 | 6.870 | 0.963 | 0.57x | 7.1x | 52% |
| q07 | 0.508 | 11.22 | 5.258 | 0.10x | 2.1x | 34% |
| q08 | 0.289 | 3.349 | 1.277 | 0.23x | 2.6x | 70% |
| q09 | 1.775 | 24.05 | 10.87 | 0.16x | 2.2x | 46% |
| q10 | 0.834 | 12.24 | 5.910 | 0.14x | 2.1x | 47% |
| q11 | 0.139 | 0.637 | 0.366 | 0.38x | 1.7x | 3% |
| q12 | 0.692 | 20.21 | 7.659 | 0.09x | 2.6x | 6% |
| q13 | 0.860 | 11.64 | 5.612 | 0.15x | 2.1x | 3% |
| q14 | 0.249 | 15.35 | 1.494 | 0.17x | 10.3x | 0% |
| q15 | 0.427 | 9.206 | 0.971 | 0.44x | 9.5x | 1% |
| q16 | 0.352 | 4.519 | 2.643 | 0.13x | 1.7x | 7% |
| q17 | 0.899 | 17.68 | 3.662 | 0.25x | 4.8x | 5% |
| q18 | 2.862 | 34.29 | 5.219 | 0.55x | 6.6x | 4% |
| q19 | 0.082 | 1.624 | 0.463 | 0.18x | 3.5x | 1% |
| q20 | 0.138 | 5.501 | 3.060 | 0.05x | 1.8x | 5% |
| q21 | 0.763 | 24.25 | 6.453 | 0.12x | 3.8x | 21% |
| q22 | 0.157 | 3.884 | 1.417 | 0.11x | 2.7x | 20% |
| all 22 | 15.01 | 269.3 | 83.5 | 0.18x | 3.2x | |

Every result hash agrees with PostgreSQL's (as before: the two float sums PostgreSQL adds in
its workers' order differ from its parallel plan; they equal `max_parallel_workers_per_gather =
0`'s digit for digit, checked with psql, as the window query's float running sum does).

### What changed

1. **Projection pushdown** (`exec/need.rs`, `nodes::open_need`, `scan::scan_need`,
   `storage::row::decode_row_need` / `skip_value`): a node opened by a parent passes down the
   columns anything above reads (`ExecCtx::need`, taken as the child opens): filters and sorts
   add their expressions' columns to what is asked of them, projections, aggregates and
   window calls ask for exactly what they evaluate, joins for the left side's needed columns
   with their keys, parameters and condition. A table scan decodes those columns and skips the
   others (NULL in the row); its own filter's columns are always decoded. Conservative where
   it can't see: no mask through LockRows, DML, CTEs, set operations, sub-selects' parameters
   are columns like any other, nothing under EXPLAIN ANALYZE (whose sizes read whole rows),
   none for a table whose compressed values are tracked (`sizing::note_read`), none for the
   build side of hash joins (its rows' sizes model PostgreSQL's bucket chunks) or for scans
   with row marks, samples or ordered index paths. Lineitem's 16 columns → q01's 7.
2. **Big tables read as they're asked for** (`ScanNode::streamed`, `sizing::heap_modelled`):
   a table past the heap model's limit has no heap order to put rows in; its sequential scan
   no longer collects every (key, row) before the first row.
3. **Parallel sequential scans** (`exec/parscan.rs`): such a scan (a statement snapshot, no
   row marks, sample or system columns, not under EXPLAIN ANALYZE, max_parallel_workers_per_gather
   > 0) is cut into 64 chunks per worker by its key range; max_parallel_workers_per_gather + 1
   threads (from a process-wide pool of max_parallel_workers) read, decode and filter chunks
   while the executor's thread takes their rows in key order, so the rows and their order are
   the serial scan's. pg_stat's tuples read count each row as the serial scan does, filtered
   ones included, when the executor takes it.
4. **Kernels** (`exec/kernel.rs`, `ops::pure`, `string::pure`): the filters, group keys and
   aggregate arguments the workers evaluate, compiled from `Expr`: columns, constants, AND /
   OR / NOT, NULL tests, searched CASE, IN lists, `op ANY (array)` and binary built-ins that read
   only their arguments (comparisons without a collation of their own, bpchar and date /
   timestamp comparisons, integer, float and numeric + − ×, LIKE / NOT LIKE), each the same
   code the executor's implementation calls. A kernel never fails: where `eval` would raise
   an error (or meets a value it leaves to `eval`: an extension type's, a record, a
   timestamptz compared with a date), the row goes to the executor's thread unfiltered and
   `eval` runs the original expression there, so the first error is the serial scan's, at the
   same row.
5. **Partial aggregation** (`exec/partial.rs`, `AggNode::partial_run`): a plain GROUP BY right
   over such a scan, whose keys and arguments are kernels and whose aggregates split exactly
   (count, sum / avg of numeric and integers, min / max by the type's order, and float
   aggregates whose values the workers collect in order and the executor's thread sums in
   the serial order), runs as PostgreSQL's Partial / Finalize Aggregate: the workers
   aggregate their chunks into parts, the executor's thread merges the parts in key order,
   numbering groups in their order of first appearance and looking each row up in the hash
   table model (`pghash::GroupOrder`, which takes each group's hash once now), so groups come
   out in the serial order. A row a kernel can't do comes as a row, in its place, for the
   serial path.
6. **Numeric** (`types/numeric.rs`, `ops`, `agg`): add, sub, mul, cmp, `checked` and hashing of
   values fitting an i64 in i128 arithmetic without allocating (the general BigInt path
   otherwise; `small_fast_paths_match_bigint` checks they agree); operands borrowed rather
   than cloned; sum / avg(numeric, int8) accumulate in an i128 at the largest scale seen,
   spilling to the exact BigInt sum past it; stored numerics up to 8 bytes decode straight
   from an i64.
7. **Fast sorts** (`exec/fastsort.rs`): sort keys whose values encode as bytes ordering like
   `compare_values` (no USING operator; integers, floats, dates, times, booleans, text by its
   bytes) are sorted by those bytes with Rust's sort, also for bounded (LIMIT) sorts, window
   sorts and the presort of DISTINCT aggregates; the result is PostgreSQL's quicksort's when no
   two rows with equal keys differ, which is checked after sorting (then the rows go through
   `pgsort` from their original order). Keys that are plain columns aren't copied per row.
8. **Per-row costs**: calls evaluate up to four arguments on the stack; aggregates with one
   argument feed it without a vector (a column without a copy); the hash aggregate makes its
   key in one reused vector and hashes each group once for the order model, DISTINCT too;
   hash join probes reuse their key vector and share candidate lists (`Rc`) instead of
   cloning them; LIKE compiles its pattern once per pattern and matches ASCII strings by
   bytes; window partitions no longer format each key's text per row; DISTINCT aggregates
   deduplicate with a hash set.

### Where analytic work is still slower

- **Index scans over many rows** (q03, q04, q09, q10, q12, q20, q21: most of their time).
  Nested loops over inner index scans and merge joins over index scans fetch each row with a
  point read through every LSM source; agent 43's storage work (batched index fetches). And
  for SF1's big tables the engine's index cost estimates (round 17 item 7) give some plans
  PostgreSQL doesn't choose (q12: a merge join over orders' primary key where PostgreSQL
  hashes a parallel scan): EXPLAIN differs there.
- **Joins and aggregates above them run on one thread**: only a scan, its filter and an
  aggregate right over it are parallel. Hash join probes (hash_join, q05, q07, q08) and
  aggregates over joins run on the executor's thread, a row (`Vec<Value>`) at a time, with a
  clone of both sides' values per joined row. The next steps: probe a shared, read-only build
  table from the scan workers (PostgreSQL's Parallel Hash Join) and aggregate the joined rows
  in parts as in 5; and single-integer join keys in a map of their own.
- **Sorts with ties between different rows** fall back to the PostgreSQL-order quicksort over
  `Value`s, and sorts of numeric keys always do (no byte encoding yet).
- **Window functions** sort and run on one thread; `count(DISTINCT)` sorts all rows first.
- **Numeric values** still allocate (an `Arc` and a BigInt per value): decode, arithmetic and
  aggregation of numeric columns are most of what q01's workers do.

### Verification (round 18)

- `cargo test -p pg-engine` (all of it, including the new `tests/parallel_scan.rs`: every
  kind of filter, aggregate and error over a 420,000-row table, serial against parallel, row
  for row with the same statistics; `exec::fastsort` encodings against `compare_values`;
  numeric fast paths against the BigInt arithmetic; partial row decoding), and `cargo clippy
  -p pg-engine --all-targets` with no new warnings.
- The regression suite in full, with its own reference container, at this round's start and
  end: **51,134 / 51,150 both, identical per file and per differing unit**.
- `difftest smoke`: the same counts as `smoke_known.txt`'s run; and 150 seeds × 120
  statements with planner-setting variants (`DIFFTEST_PLAN=0.5`) at the start and the end:
  the same 1,144 mismatch records, statement for statement.
- Every benchmark query's result hash, as above; and 19 queries whose row order shows (no
  ORDER BY: hash aggregates, joins, scans, DISTINCT; sorts with ties; bounded sorts) over the
  2M-row and TPC-H tables, row for row against the round's starting engine. (One of them
  showed an old difference from PostgreSQL, not this round's: `GROUP BY k % 50` over the 2M
  rows comes out in another hash order than PostgreSQL's, from both engines.)

## Round 19 (agent 48): joins, DISTINCT aggregates, sorts; hash order fixes

The same harness and settings, on cores 8–11 (both servers) and 12–15 (the client), the
machine as busy as in round 18 (load 25–35; PostgreSQL's times are within 0.6–1.2x of round
18's). **pg** is PostgreSQL 18.6 (container `a48-pg`); **before** the engine at `52f786e`;
**after** this round's; each engine run after a restart and a first read of the big tables
(a table loaded by the running process keeps its heap model and isn't scanned in parallel:
see "Where analytic work is still slower"); medians of 3, the better of two interleaved runs
per side; **spread** the larger of the two sides' run-to-run differences (one run of each
shared the machine with a difftest run).

### Analytic, 2M rows

| query | pg | before | after | after / pg | after / before | spread |
|---|---|---|---|---|---|---|
| filtered scan | 0.102 | 0.246 | 0.276 | 0.37x | 0.9x | 47% |
| full sort, float | 1.238 | 2.413 | 2.482 | 0.50x | 1.0x | 42% |
| full sort, text | 1.030 | 3.032 | 2.371 | 0.43x | 1.3x | 41% |
| top-100 | 0.145 | 0.417 | 0.453 | 0.32x | 0.9x | 50% |
| hash aggregate (1000 groups) | 0.211 | 0.642 | 0.393 | 0.54x | 1.6x | 109% |
| count(DISTINCT) | 0.624 | 3.617 | 0.518 | **1.20x** | 7.0x | 96% |
| hash join + aggregate | 0.384 | 2.176 | 0.720 | 0.53x | 3.0x | 54% |
| window functions | 1.921 | 4.927 | 4.878 | 0.39x | 1.0x | 37% |
| index range (49k rows) | 0.011 | 0.081 | 0.097 | 0.11x | 0.8x | 32% |
| numeric sum / avg | 0.143 | 0.263 | 0.209 | 0.68x | 1.3x | 84% |

### TPC-H SF1

| query | pg | before | after | after / pg | after / before | spread |
|---|---|---|---|---|---|---|
| q01 | 2.361 | 8.437 | 8.127 | 0.29x | 1.0x | 9% |
| q02 | 0.326 | 1.430 | 1.295 | 0.25x | 1.1x | 23% |
| q03 | 0.518 | 7.171 | 7.114 | 0.07x | 1.0x | 26% |
| q04 | 0.221 | 3.281 | 3.367 | 0.07x | 1.0x | 41% |
| q05 | 0.399 | 2.733 | 2.950 | 0.14x | 0.9x | 12% |
| q06 | 0.507 | 0.943 | 1.022 | 0.50x | 0.9x | 36% |
| q07 | 0.436 | 5.267 | 5.144 | 0.08x | 1.0x | 17% |
| q08 | 0.207 | 1.366 | 1.446 | 0.14x | 0.9x | 18% |
| q09 | 1.508 | 11.99 | 10.19 | 0.15x | 1.2x | 31% |
| q10 | 0.689 | 6.389 | 5.672 | 0.12x | 1.1x | 7% |
| q11 | 0.117 | 0.359 | 0.336 | 0.35x | 1.1x | 16% |
| q12 | 0.624 | 8.100 | 7.234 | 0.09x | 1.1x | 7% |
| q13 | 1.161 | 5.710 | 4.949 | 0.23x | 1.2x | 27% |
| q14 | 0.239 | 1.536 | 1.400 | 0.17x | 1.1x | 21% |
| q15 | 0.473 | 0.988 | 0.971 | 0.49x | 1.0x | 26% |
| q16 | 0.379 | 2.869 | 2.363 | 0.16x | 1.2x | 10% |
| q17 | 1.178 | 3.461 | 3.226 | 0.37x | 1.1x | 29% |
| q18 | 2.823 | 5.383 | 4.027 | 0.70x | 1.3x | 20% |
| q19 | 0.083 | 0.445 | 0.413 | 0.20x | 1.1x | 34% |
| q20 | 0.129 | 2.973 | 2.754 | 0.05x | 1.1x | 26% |
| q21 | 0.548 | 6.342 | 2.670 | 0.21x | 2.4x | 9% |
| q22 | 0.200 | 1.392 | 1.275 | 0.16x | 1.1x | 6% |
| all 22 | 15.13 | 88.6 | 78.0 | 0.19x | 1.14x | |

Every result hash agrees with PostgreSQL's, as in round 18 (the two float sums PostgreSQL adds
in its parallel workers' order equal its serial plan's digit for digit). Changes under 30% are
within this machine's noise; the clear gains are count(DISTINCT), the hash join, the hash
aggregate and q21.

### What changed

1. **Hash join probes in the scan's workers** (`exec/pipe.rs`, PostgreSQL's Parallel Hash
   Join, its probe side). A hash join (inner, left, semi, anti; not a merge join's, not one
   returning unmatched inner rows) whose outer keys and condition compile to kernels builds
   its table on the executor's thread as before, then shares it read-only (`pipe::Built`,
   in an `Arc`) with the workers of a parallel scan below it, which probe it for every row
   they read and filter, in the scan's order. Joins over joins stack their probes the same way
   (`pipe::Probe` stages, bottom first: `Node::pipeline` asks down the tree), and an
   aggregation above them aggregates the joined rows in parts (`partial`) without making
   them: the condition and the partial aggregates read the outer and inner rows in place
   (`pipe::Joined`, `kernel::Cols`), and groups are looked up by borrowed keys
   (`hashbrown`'s `Equivalent`: no shared refcounts touched per row). Every row comes to the
   executor's thread tagged with the stages it went through (`pipe::Item`); one a kernel
   left to `eval` (a key, the condition) comes out of the stage before, in its place, and the
   join node it belongs to runs it as the serial join does, so errors come at the same row.
   The scan's statistics count each scan row once, at the first item it produced.
2. **Joined rows copy only what's read** (`pipe::joined`, `HashJoinNode::keep`): a join's
   output columns nothing above reads stay NULL (the need mask, plus the join condition's
   columns), serial or parallel; candidate lists are ranges of one array, not `Rc` lists.
3. **A map of its own for single-integer join keys** (`pipe::KeyMap::Int`): when every inner
   key is one int8 (any integer type, normalised), keys are `i64` in a map keyed by them.
4. **DISTINCT aggregates without a sort** (`agg::hash_distinct`): count(DISTINCT x) over
   integers, numerics, floats, dates and times, booleans, uuids, text in a deterministic
   collation, and sum / avg(DISTINCT x) over integers and numerics give the same result in
   any order, so each group keeps the values seen in a hash set and feeds each once, as the
   rows come (no input buffered and sorted). The groups' order stays the GroupAggregate's.
5. **Sorts**: numeric keys encode as order-preserving bytes (`fastsort::numeric_key`: the
   decimal exponent and significant digits, so 1.0 and 1.00 are equal); and when rows with
   equal keys differ, PostgreSQL's quicksort (`pgsort::tuplesort`) now runs over the encoded
   bytes rather than over `Value`s (the same comparisons' outcomes, so the same order).

6. **Window functions** (a second commit): plain-column sort and partition keys are read from
   the rows in place (other keys computed as before, in the same order), the ORDER BY keys
   once per window rather than per call, each partition's peer groups by the window's first
   call for all of them, the results in one array. The window benchmark, measured apart
   (interleaved, two runs each): 5.7 s -> 4.0 s (PostgreSQL 1.9 s), the same rows.

### Correctness fixes (each with tests; observable before)

- **Hash aggregation's group order** (the `GROUP BY k % 50` order round 18 found): the hash
  table model was sized with a rough entry width and the largest power of two that fits
  hash_mem; PostgreSQL 18 sizes it from hash_agg_entry_size (the input's width, the
  transition states and their space: `paths::parallel::agg_transitions`), takes half of what
  fits the memory limit (`hash_choose_num_buckets`' `>>= 1`), and takes the spill partitions'
  buffers out of that limit when it expects to spill (`pghash::AggSizing`). Grouping sets
  share the limit among their tables; a HashSetOp's table is sized for its estimate, capped
  at what hash_mem holds of its entries (`pghash::setop_nbuckets`). Of 19 probe queries over
  the 2M-row table (hashed groupings, DISTINCT, set operations, grouping sets, other key
  types, work_mem), 14 came out otherwise than PostgreSQL's; 7 match now (with the next fix);
  of the others, 3 differ through the planner's group estimates (agents 46 / 47), 3 spill
  (below), and 1 is PostgreSQL's parallel float sum.
- **Sorted groupings came out in hash order**: where PostgreSQL's plan (EXPLAIN's) is a
  GroupAggregate or Group (enable_hashagg off, groups that would spill, presorted input), the
  executor still ran a hash aggregation and emitted its hash table's order; its groups now
  come out in the plan's sort order (`AggEstimate::sorted`, the keys and directions EXPLAIN
  sorts on). And where that plan sorts its whole input (a Sort below; for a parallel plan: the
  serial plan's), aggregates whose results depend on their rows' order (float sums,
  array_agg / string_agg without ORDER BY, min / max ties of values equal but different)
  aggregate the rows in PostgreSQL's sort order (`AggEstimate::presort`).
- **Hash join keys of different types**: the probe normalised integers only; key values
  `Value` finds equal but hashes apart (an int8 and a numeric, a float and an integer) could
  meet only through a hash collision. Both sides are now normalised per key for their types'
  pair (`pipe::norm_for`: integers as int8, a float pair as float8, an integer meeting a
  numeric as a numeric, bpchar without its padding), so equal values hash alike
  (`pipe::tests::keys_of_both_sides_hash_alike`). No SQL found made the planner join such
  keys (it casts first), so no result changed.

### Where analytic work is still slower (round 19)

- **Index scans over many rows** (q03, q04, q09, q10, q12, q20, q21; agent 43's storage work)
  and **the engine's TPC-H plans**: at SF1 its EXPLAIN differs from PostgreSQL's in most join
  queries (nested loops over index scans where PostgreSQL has parallel hash joins over
  parallel scans: q03, q05, q07–q10, q12–q14, …), the planner's estimates for the big tables,
  so the parallel probes of item 1 rarely apply there.
- **Spilling hash aggregation isn't modelled**: when PostgreSQL's table outgrows hash_mem at
  run time it spills new groups to partitions and emits them after the in-memory ones, batch
  by batch; the engine emits one table's order (e.g. `work_mem = 64kB`, 3,000 groups). Doing it
  exactly needs PostgreSQL's memory accounting (the bucket array, the bump and AllocSet
  blocks of entries and transition values) as hash_agg_check_limits sees it.
- **External sorts aren't modelled**: a Sort past work_mem merges runs in PostgreSQL, which
  orders rows with equal keys otherwise than the in-memory quicksort the engine reproduces.
- Joins whose build side is the big table, Parallel Hash's parallel build, right / full
  joins and merge joins still run on the executor's thread; so do window functions (copies
  per row and frame bookkeeping, item 6 took the key vectors out), sorts and
  the merge of partial aggregates (each part's lookups in the hash table model, float values
  summed in order).
- **Numeric values still allocate** (an `Arc` and a BigInt each): q01's workers.
- **A table loaded by the running server** keeps the heap model the load built, which keeps its
  scans serial until the server restarts (q06: 6.0 s right after the load, 1.4 s after a
  restart); persisting the size estimate with the table, or dropping the model past its
  limit, would let them run in parallel (agent 43's area).

### Verification (round 19)

- `cargo test -p pg-engine` (all of it; new: `tests/parallel_scan.rs`
  `parallel_joins_match_serial`, 17 joins over a 420,000-row table with keys, conditions and
  errors the kernels can and can't do, serial against parallel with the same statistics; the
  serial / parallel check now allows the groups' order to differ only where the two settings'
  plans group differently, as PostgreSQL's do; unit tests of the table sizing against
  PostgreSQL's bucket counts, key normalisation, the integer map, numeric sort keys and
  PostgreSQL's order for ties; golden `a48_hashagg_order`, recorded from PostgreSQL), and
  `cargo clippy -p pg-engine --all-targets` with no new warnings.
- The regression suite in full, each with its own reference container, at `c904737c` and with
  this round's changes on it: **51,144 / 51,150 both, identical per file and per differing
  unit** (and before the later rebases, at `a319ebb`: 51,141 both; `52f786e`: 51,140;
  `75986d8`: 51,138; identical each time).
- `difftest smoke` passes (after the last rebase too); 150 seeds × 120 statements with
  planner-setting variants (`DIFFTEST_PLAN=0.5`): the same mismatch records as the engine at
  `75986d8`, statement for statement, and again at `a319ebb`.
- Stack: in debug builds the EXPLAIN builder's `convert` has a 272 kB frame and recurses per
  plan node, so EXPLAIN's estimate (which the executor asks for a hash table's size and a
  grouping's order) runs near a 2 MB thread's end over a nested plan
  (`pg_stat_user_tables` in `parallel_scan.rs`); this round's per-node data went into
  `Detail::GroupSort` rather than an `ExNode` field to keep those frames as they were (a field
  made that test overflow by under 128 kB). Release frames are under 4 kB.
- Every benchmark query's result hash, as above.

## Round 19 (agent 65): streaming results

A SELECT's rows and COPY TO's data now go to the client as the executor makes them (HANDOFF.md,
"Result rows stream"). Before, a statement's rows were collected, then sent. Workload `stream`
(`tools/bench/bench.rs`; `stream-load --rows N` makes `stream_t (id int, v int, t text)`,
10M rows of `g, g % 1000, md5(g::text)`). It reads `SELECT * FROM stream_t` and
`COPY stream_t TO STDOUT` over the wire as they come (tokio-postgres `simple_query_raw` /
`copy_out`), keeping no rows, and reports the time to the first row, the whole time, and the
server's peak memory over what it had before. Release builds of `dd758b2` (before) and of
this change, on one data directory; PostgreSQL 18.6 with the harness's settings; server on
CPUs 28-31, client on 32-35; load average about 20 from other agents, so times vary by
about 15%.

### 10M rows, over the wire (block cache warm; seconds, MB)

| | first row | whole result | server memory for the result |
|---|---:|---:|---:|
| PostgreSQL, SELECT | 0.0005–0.0012 | 2.9–3.0 | +0–1.5 (cgroup anon + shmem) |
| before, SELECT | 1.1–3.8 | 3.8–7.0 | +1,926 (VmHWM 1,687 → 3,613) |
| after, SELECT | 0.0012–0.013 | 3.5–4.7 | +68 (VmHWM 1,685 → 1,753) |
| PostgreSQL, COPY TO | 0.0007–0.0010 | 2.7–3.0 | +0 |
| before, COPY TO | 5.2–8.0 | 6.2–9.1 | +3,079 (VmHWM 1,687 → 4,766) |
| after, COPY TO | 0.0008–0.014 | 5.2–6.2 | +66 (VmHWM 1,686 → 1,752) |

- The memory after is bounded: the same scan sending 100k / 1M / 10M rows peaked at +15 / +66 /
  +68 MB (the parallel scan's queues fill while the client reads; a client that stops
  reading stops the statement, as a full socket stops a backend). Before, the result was held
  whole: `Vec<Value>` rows, and COPY TO's lines on top of them.
- The 1.7 GB both start from is the block cache (`--cache-mb 2048`), filled by the first
  statement that plans `stream_t`, not by the rows.
- In-process (`Session::query_each`, `run.sh inproc stream`): the first row in 1 ms, the
  whole 10M rows in 1.4 s, +1–24 MB. In-process COPY TO still collects (`Outcome::CopyOut`).

Found, not output-path work: **the first statement that plans a big table after the engine
starts takes 2.2–2.7 s** (cold `EXPLAIN SELECT * FROM stream_t` 2.7 s, VmRSS 54 MB → 1.7 GB;
then `LIMIT 1` takes 2 ms). Planning reads the whole table into the block cache on a cold
start. That is the first-row time of a fresh engine's first big SELECT (2.4–2.6 s), with or
without parallel workers.

### Small results: pgbench select-only (scale 10, `-M prepared`, 10 s; TPS, medians of 3)

| clients | before | after |
|---:|---:|---:|
| 1 | 13,064 / 11,455 | 11,907 / 12,094 |
| 8 | 60,713 / 70,402 | 61,356 / 62,813 |
| 32 | 63,850 / 70,058 | 67,456 / 69,919 |

(Two sets of three rounds, interleaved; five more `-c 8` rounds: before 45–52k, after
45–62k.) Wall-clock TPS on this machine varies by more than any difference between the
builds. So, as a steadier measure: engine CPU per transaction over a fixed 160,000
(`/proc/<pid>/stat`, three rounds each). `-c 1`: before 49.5–57.5 µs, after 50.6–55.9 µs;
`-c 8`: before 52.1–57.3 µs, after 52.6–57.6 µs. Those ranges overlap, so there is no
measurable regression. A streamed small result costs a sink (one allocation per statement)
and, per row, a settings-generation check. The rendering context is still built once per
statement, as before.

## Round 20 (agent 64): TPC-H plans, first batch

The same harness and settings, both servers on cores 24-27 and the client on 16-19, the
machine shared with other agents (load 11-20). **pg** is PostgreSQL 18.6 (container
`a64-pg`); **before** the engine at `61e57157` (this round's base, after agent 43's storage
round); **after** with this batch (`ec57b622`). Each engine run follows a restart and a first
read of lineitem, orders and partsupp; medians of 3, the better of two interleaved runs per
side; **spread** the larger of the two sides' run-to-run differences. Every result hash
agrees with PostgreSQL's.

| query | pg | before | after | after / pg | after / before | spread |
|---|---|---|---|---|---|---|
| q01 | 1.631 | 8.237 | 8.276 | 0.20x | 1.00x | 9% |
| q02 | 0.193 | 1.294 | 1.175 | 0.16x | 1.10x | 10% |
| q03 | 0.357 | 2.505 | 2.455 | 0.15x | 1.02x | 46% |
| q04 | 0.155 | 4.600 | 1.068 | 0.14x | 4.31x | 10% |
| q05 | 0.302 | 4.886 | 1.333 | 0.23x | 3.67x | 35% |
| q06 | 0.368 | 18.41 | 1.836 | 0.20x | 10.02x | 32% |
| q07 | 0.358 | 5.490 | 7.716 | 0.05x | 0.71x | 3% |
| q08 | 0.170 | 2.212 | 2.826 | 0.06x | 0.78x | 9% |
| q09 | 1.158 | 12.50 | 18.61 | 0.06x | 0.67x | 21% |
| q10 | 0.571 | 9.181 | 5.865 | 0.10x | 1.57x | 25% |
| q11 | 0.085 | 1.464 | 1.005 | 0.08x | 1.46x | 7% |
| q12 | 0.453 | 6.579 | 6.621 | 0.07x | 0.99x | 19% |
| q13 | 0.649 | 5.328 | 5.066 | 0.13x | 1.05x | 11% |
| q14 | 0.159 | 11.14 | 1.943 | 0.08x | 5.74x | 39% |
| q15 | 0.280 | 9.654 | 1.466 | 0.19x | 6.59x | 24% |
| q16 | 0.204 | 1.494 | 1.426 | 0.14x | 1.05x | 29% |
| q17 | 0.801 | 3.677 | 3.510 | 0.23x | 1.05x | 5% |
| q18 | 2.416 | 5.472 | 10.64 | 0.23x | 0.51x | 9% |
| q19 | 0.059 | 0.521 | 0.502 | 0.12x | 1.04x | 29% |
| q20 | 0.106 | 2.988 | 2.855 | 0.04x | 1.05x | 3% |
| q21 | 0.511 | 3.261 | 2.801 | 0.18x | 1.16x | 9% |
| q22 | 0.114 | 0.699 | 0.633 | 0.18x | 1.10x | 13% |
| all 22 | 11.10 | 121.6 | 89.6 | 0.12x | 1.36x | |

The base is slower than round 19's engine on the queries with bitmap heap scans over
lineitem (q06, q14, q15: agent 43 measured q06's plan at 20-23 s there); this batch makes
them 6-10x faster. q07-q09 vary with the machine's load more than the spread shows: run
alone, interleaved, twice each, q07 took 8.6-9.0 s before and 5.4-5.6 s after, q09 12.4-12.9
s before and 13.7-14.0 s after. q18 is slower for a real reason: it now has PostgreSQL's
plan, its IN sub-select grouped along a full index scan of lineitem_pkey (6M rows in key
order), which the engine runs on one thread with a row fetch per entry batch where
PostgreSQL reads the heap in order.

### Plans

`EXPLAIN (COSTS ON)` of the 22 queries on both servers, compared node by node (shapes;
costs can't agree to the digit: PostgreSQL's ANALYZE samples at random). Before: 6 of 22 had
PostgreSQL's shape (q01, q06, q09, q11, q12, q14). After: 13 (q01-q04, q06, q09-q14, q17,
q22). What differed and why:

1. **n_distinct of l_orderkey** (q03, q04, q10, q18 and every join on it): PostgreSQL's
   ANALYZE samples 30,000 of lineitem's 112,456 pages and then rows within them; an order's
   lines share a page, so the sample repeats orders and its n_distinct is about 390,000. The
   engine sampled rows uniformly (about 1.2M), so it expected fewer lineitem rows per order.
   ANALYZE now samples pages as acquire_sample_rows does (`analyze.rs` `sampled_blocks`;
   414,838 here, PostgreSQL 392,494; its own varies run to run).
2. **Index sizes of tables too big to model**: the btree height was 0 (index scans started at
   0.18 for PostgreSQL's 0.43; `sizing::btree_level`), and deduplicated indexes' inner pages
   held whole posting tuples as downlinks (l_shipdate_idx 6,047 pages for 5,207;
   `relsize::btree_pages` now takes pivots without posting lists: 5,209).
3. **A cold heap model forgot the visibility map**: after a restart ANALYZE wrote
   relallvisible 0 for modelled tables (customer, part, supplier), pricing index-only scans
   as heap fetches; the model now starts from the catalog's relallvisible.
4. **Parallel plans**: a join with a correlated sub-select in its clauses got partial paths
   (q17; PostgreSQL's joinrel isn't consider_parallel); a merge join whose sorted input
   became a Gather Merge lost that input's partial plans, so the Parallel Hash Join
   PostgreSQL picks was never built (q12); partial and finalized hashed aggregations left
   out their spill costs.
5. **Grouping**: an index path in the grouping's order competed with hashing without the
   hash's spill (q18's sub-select: 621,298 hashed against 386,872 sorted along the index);
   an expression key estimated 200 groups for its Vars' 744 (q22); a group key that is an
   aggregate of the grouping below printed in parentheses (q13).

Left (the second batch has some): q05 (the engine finds a cheaper nested loop the other
way round, orders outside and a Memoize of customer_pkey inside, which PostgreSQL costs
higher), q07 / q08 (join order), q15 (a CTE's sort order), q16 (group estimate: each
relation's own filtered rows), q19 (an OR whose arms share the join clause), q20 / q21
(IN / EXISTS forms), q18 (the semi join as an inner join over the unique sub-select).

### What changed in the executor

- **Dense bitmap heap scans** (`exec/scan.rs` `dense_bitmap_scan`): a bitmap heap scan of a
  table too big to model selecting at least one row in 32 runs as a scan of the table
  checking the bitmap's conditions and then the filter, in the parallel scan's workers
  where it can: the same rows in the same (heap) order. Not under EXPLAIN ANALYZE (its
  counters are the bitmap's) or predicate locking (which locks what a scan reads); the
  cumulative statistics count the bitmap's index scan and fetches as before. Otherwise a
  dense bitmap rechecks its rows from one ordered scan of the table (`rows_in_order`)
  instead of a lookup per entry. q06: 18-26 s -> 1.8 s.
- **Planning reads no table**: the first statement touching a table too big to model after
  its database opened read all of it to size it (2-3 s for lineitem, in the first EXPLAIN
  after a restart: agent 65's finding); a table whose last VACUUM / ANALYZE counted more rows
  than the model holds is now sized from pg_class (17 ms).
- ANALYZE decodes only the rows it keeps in the sample.

### Verification (round 20, first batch)

- `cargo test -p pg-engine` (golden `a64_tpch_planner`, recorded from PostgreSQL 18.6) and
  `cargo clippy -p pg-engine --all-targets` with no new warnings.
- The regression suite in full (debug build, private reference): 51,148 / 51,150, no unit
  changed against the committed baseline.
- `difftest smoke` passes; 150 seeds x 120 statements with planner-setting variants
  (`DIFFTEST_PLAN=0.5`): the same mismatch records as the base, statement for statement.
- Every TPC-H result hash agrees with PostgreSQL's.

### Second batch (round 20, agent 64)

The same method as the first batch's table, run after it: **pg** PostgreSQL 18.6; **before**
the first batch (`ec57b622`); **after** with the second batch. Every result hash agrees with
PostgreSQL's.

| query | pg | before | after | after / pg | after / before | spread |
|---|---|---|---|---|---|---|
| q01 | 1.764 | 8.051 | 7.924 | 0.22x | 1.02x | 24% |
| q02 | 0.196 | 1.144 | 1.162 | 0.17x | 0.98x | 9% |
| q03 | 0.365 | 2.279 | 2.203 | 0.17x | 1.03x | 14% |
| q04 | 0.169 | 0.991 | 1.006 | 0.17x | 0.99x | 16% |
| q05 | 0.346 | 1.201 | 1.242 | 0.28x | 0.97x | 28% |
| q06 | 0.349 | 1.776 | 1.806 | 0.19x | 0.98x | 11% |
| q07 | 0.359 | 7.078 | 7.268 | 0.05x | 0.97x | 31% |
| q08 | 0.172 | 2.823 | 2.791 | 0.06x | 1.01x | 36% |
| q09 | 1.236 | 19.71 | 18.79 | 0.07x | 1.05x | 5% |
| q10 | 0.588 | 6.270 | 6.088 | 0.10x | 1.03x | 5% |
| q11 | 0.084 | 1.069 | 0.935 | 0.09x | 1.14x | 5% |
| q12 | 0.539 | 6.793 | 6.216 | 0.09x | 1.09x | 21% |
| q13 | 0.677 | 5.396 | 4.864 | 0.14x | 1.11x | 21% |
| q14 | 0.173 | 2.317 | 1.994 | 0.09x | 1.16x | 25% |
| q15 | 0.289 | 1.653 | 1.438 | 0.20x | 1.15x | 51% |
| q16 | 0.236 | 1.492 | 1.388 | 0.17x | 1.08x | 21% |
| q17 | 0.805 | 3.587 | 3.273 | 0.25x | 1.10x | 20% |
| q18 | 2.393 | 11.02 | 10.31 | 0.23x | 1.07x | 15% |
| q19 | 0.068 | 0.565 | 0.430 | 0.16x | 1.31x | 20% |
| q20 | 0.105 | 3.177 | 2.848 | 0.04x | 1.12x | 20% |
| q21 | 0.491 | 2.955 | 2.836 | 0.17x | 1.04x | 35% |
| q22 | 0.111 | 0.621 | 0.675 | 0.16x | 0.92x | 20% |
| all 22 | 11.51 | 92.0 | 87.5 | 0.13x | 1.05x | |


Plans (the same comparison as above): **15 of 22** have PostgreSQL's shape, q15 and q19 added
(q22 counted in the first batch's 13). Changes:

1. **Duplicate ORs** (canonicalize_qual's process_duplicate_ors, `binder/expr.rs`
   `factor_ors`): a clause every arm of an OR has, equal as written, comes out in front of
   it. q19's three arms each have `p_partkey = l_partkey`, which now drives the index scan
   of lineitem_l_partkey_l_suppkey_idx PostgreSQL has (it was a BitmapOr of three).
2. **Sorts past work_mem** cost by the session's work_mem (`costs::work_mem`, set by the
   settings readers as planning starts; it was 4MB always), with tuplesort_merge_order's
   merge order.
3. **Group estimates per relation** (`extstat::num_groups`): each relation's distinct values
   scaled by its own filtered rows, not the grouping's input rows (q16: 16,008 groups for
   PostgreSQL's 15,848, was 19,334); a parameterized scan's per-loop rows aren't taken for
   its relation's.
4. **A grouped CTE's order** (PostgreSQL 17's CTE pathkeys): its CTE Scan returns the
   grouping's sort order (q15 has no Sort over its join); a grouped CTE's columns take no
   base statistics (examine_simple_variable punts on GROUP BY and set operations).

Left, with what was found:

- q05: the engine prefers a nested loop with orders outside and a Memoize of a
  customer_pkey scan inside (52,158 against PostgreSQL's 52,990 for customer outside and an
  index scan of orders_o_custkey_idx); Memoize's cost (its hit ratio over o_custkey's
  n_distinct, its cache entries in hash_mem) is the place to compare.
- q16: PostgreSQL sorts the partial join below a Gather Merge for the GroupAggregate (its
  count(DISTINCT) has no partial form); the engine gathers, then sorts.
- q18: PostgreSQL plans the IN as an inner join over the unique-ified sub-select (its GROUP
  BY output is distinct already) with orders' partial scan below; the engine keeps a Hash
  Semi Join, serial.
- q20: PostgreSQL pulls the outer IN up to a semi join; the engine keeps it as a SubPlan
  filter on supplier.
- q07, q08, q21: join order and form not examined yet.
- Executor: q18's full index scan of lineitem_pkey under a GroupAggregate (6M rows in key
  order, one thread), index-only scans of big tables that need the heap, window functions
  and sorts on one thread, numeric values allocating.

## Round 20 (agent 77): analytic executor, first batch

The same harness and settings: both servers on cores 28-31, the client on 16-19, the
machine shared with other agents' builds and test runs (load 14-18). **pg** is PostgreSQL
18.6 (container `a77-pg`); **before** the engine at `c7f59d74` (agent 64's second batch);
**after** with this batch. Each engine run follows a restart and a first read of lineitem,
orders and partsupp; medians of 3, the better of two interleaved runs per side; **spread**
the larger of the two sides' run-to-run differences (high here: the runs shared the machine
with other agents' builds). Every result hash agrees with PostgreSQL's.

| query | pg | before | after | after / pg | after / before | spread |
|---|---|---|---|---|---|---|
| q01 | 1.969 | 8.180 | 2.179 | 0.90x | 3.75x | 47% |
| q02 | 0.202 | 1.151 | 1.197 | 0.17x | 0.96x | 26% |
| q03 | 0.389 | 2.355 | 1.141 | 0.34x | 2.06x | 53% |
| q04 | 0.142 | 1.012 | 0.609 | 0.23x | 1.66x | 50% |
| q05 | 0.291 | 1.263 | 1.069 | 0.27x | 1.18x | 61% |
| q06 | 0.376 | 2.012 | 1.797 | 0.21x | 1.12x | 50% |
| q07 | 0.471 | 7.915 | 7.783 | 0.06x | 1.02x | 18% |
| q08 | 0.211 | 3.043 | 3.080 | 0.07x | 0.99x | 13% |
| q09 | 1.300 | 18.39 | 5.370 | 0.24x | 3.42x | 25% |
| q10 | 0.679 | 6.266 | 3.957 | 0.17x | 1.58x | 20% |
| q11 | 0.088 | 1.071 | 1.068 | 0.08x | 1.00x | 14% |
| q12 | 0.549 | 6.569 | 3.727 | 0.15x | 1.76x | 34% |
| q13 | 0.844 | 5.382 | 4.109 | 0.21x | 1.31x | 32% |
| q14 | 0.139 | 1.980 | 1.940 | 0.07x | 1.02x | 34% |
| q15 | 0.338 | 1.518 | 1.448 | 0.23x | 1.05x | 42% |
| q16 | 0.227 | 1.524 | 0.797 | 0.28x | 1.91x | 29% |
| q17 | 0.842 | 3.561 | 1.754 | 0.48x | 2.03x | 20% |
| q18 | 2.186 | 10.36 | 4.990 | 0.44x | 2.08x | 40% |
| q19 | 0.053 | 0.458 | 0.497 | 0.11x | 0.92x | 90% |
| q20 | 0.107 | 3.045 | 0.914 | 0.12x | 3.33x | 40% |
| q21 | 0.480 | 2.982 | 2.732 | 0.18x | 1.09x | 46% |
| q22 | 0.120 | 0.639 | 0.789 | 0.15x | 0.81x | 34% |
| all 22 | 12.00 | 90.7 | 52.9 | 0.23x | 1.71x | |

### What the profiles showed

`perf` of every query (`tools/bench/perf.sh`'s helper, user-space cycles: the host's kernel
symbols break `perf report`) and per-thread shares. In 15 of the 22 queries the executor's
thread did most of the work while the three scan workers waited, for these reasons, by
total time:

1. **Nested loops over inner index scans on the executor's thread** (q09, q03, q04, q18's
   outer loop, q21, q05, q19, q22): an index scan per outer row, each reading the index
   entries and fetching rows through every LSM source; q09 alone 18 s. PostgreSQL runs these
   loops in its parallel workers.
2. **bpchar filters and keys without kernels** (q01, q10, q12, q19, ...): `l_shipmode =
   'MAIL'` compares `l_shipmode::bpchar`, an implicit length coercion the kernels didn't do,
   so every row went to the executor's thread unfiltered; GROUP BY a bpchar (q01's
   l_returnflag, q12's l_shipmode) kept partial aggregation off.
3. **Full index scans serial**: q18's grouping along lineitem_pkey (6M entries), count(*) by
   index-only scan (3 s for lineitem).
4. **Numeric intermediate values**: two allocations per value per operator in q01's
   `l_extendedprice * (1 - l_discount) * (1 + l_tax)`.
5. **Joins kept off the workers**: right joins (q13), joins with a sub-select in the join
   condition (q17: 6M probes on the executor's thread to find 60k candidates).
6. **`= ANY (subquery)` compared against every row** of the sub-select per outer row (q20's
   hashed SubPlan, 10k x 5k comparisons).

Storage, outside this round's scope (agent 74's): the block cache holds decompressed blocks
and TPC-H SF1's don't fit in `--cache-mb 2048`, so random index lookups decompress most of the
blocks they read (zstd was 10-15% of q07 / q09 / q18; q09 at `--cache-mb 8192` took 10.8 s
instead of 19.6 with the base executor); a worker decodes a lineitem row in about 450 ns
(count with a numeric filter: 0.93 s against PostgreSQL's 0.33).

### What changed

- **Index lookups in the workers** (`exec/lookup.rs`): a nested loop whose inner side is a
  plain btree index scan by equality, over a parallel scan, becomes a pipeline stage next to
  the hash probes (`pipe::Stage`): each worker encodes the index key from its outer row
  (kernels over the loop's parameters), reads the entries and fetches their rows from the
  store at the statement's snapshot (`Txn::store_reader`: only while the transaction has
  written nothing, when its reads are the store's), decodes the columns read, and applies the
  inner filter and join condition: inner, left, semi and anti loops. A worker keeps the rows
  of a small (modelled) table's keys it looked up. Rows a kernel can't do go to the executor's
  thread in their place, where the loop runs them serially. Not for modelled tables unless
  the index is unique and fully matched and every page is all-visible (a fetch could prune a
  page or kill entries otherwise: `sizing::fetches_change_nothing`), Memoize (its cache
  changes the scan counts), EXPLAIN ANALYZE, predicate locks. The index statistics are
  counted as the serial loop counts them.
- **Full secondary-index scans in the workers** (`ScanNode::start_parallel_index`,
  `parscan::IndexChunk`): for a pipeline above (partial aggregation, probes), the index's
  entry range is cut into chunks the workers read in index order, fetching rows a batch at a
  time (an index-only scan decodes its entries); a big table only (not modelled: equal keys'
  rows in storage order).
- **Right and full hash joins** probe in the workers, the matched inner rows in shared flags;
  a join condition with no kernel defers only outer rows that have candidates.
- **Kernels**: casts that read no setting (length coercions, string to string, number to
  number, date to timestamp), EXTRACT / date_part of dates and timestamps, numeric + - x
  computed exactly in an i128 at the operands' scales without making intermediate values
  (`K::num`), which partial sums take directly (`SumNum::add_wide`).
- **Partial aggregation** groups bpchar keys (trimmed; the group keeps its first row's
  value) and no longer feeds the hash table order model row by row when groups come out
  sorted (a GroupAggregate).
- **Hashed ANY**: an uncorrelated `x = ANY (subquery)` whose type's `=` is equality of values
  (`hashable_type`) looks x up in a set of the sub-select's values (ExecHashSubPlan's
  answers for NULLs and no rows).

### Where TPC-H is still slower

- q07, q08 (serial nested-loop plans unlike PostgreSQL's), q13 (serial plan), q20, q21:
  plans differ (agent 78's work); q07's fully serial plan is 7.8 s.
- Storage reads: index lookups mostly decompress blocks (above); scans decode rows 3x slower
  than PostgreSQL's workers read tuples (q06, q14, q15 are scan-bound in the workers).
- Numeric columns still decode into an `Arc` and a `BigInt` per value; sorts and window
  functions run on one thread; the hash join's build side is hashed on the executor's thread.
- Small modelled tables (customer, part, supplier) aren't scanned in parallel (q02, q11,
  q16, q22 run serial plans whose per-row costs are point reads).

### Verification (round 20, agent 77)

- `cargo test -p pg-engine` (all of it, `tests/parallel_scan.rs`'s new
  `parallel_lookups_match_serial`: inner, left, semi and anti loops over index lookups,
  several rows per key, inner filters, join conditions, a unique index of a modelled table,
  loops stacked, an error at a late row, full index and index-only scans under an
  aggregation, serial against parallel row for row with the same index statistics; right and
  full joins, bpchar keys and filters, casts in the other two) and `cargo clippy -p pg-engine
  --all-targets` with no new warnings.
- The regression suite in full (debug build, private reference): 51,148 / 51,150, no unit
  changed against the committed baseline.
- `difftest smoke` passes; 150 seeds x 120 statements (`DIFFTEST_PLAN=0.5`) against the
  base's run (`c7f59d74`): 22 of its 140 mismatch records gone (the upstream difftest round 3
  fixes) and two new ones, neither through this batch's paths (tiny tables, no parallel scan:
  a missing error POSITION in the binder's ungrouped-column check, and an array-length
  error under GROUPING SETS).
- Every TPC-H result hash agrees with PostgreSQL's; pgbench select-only at scale 10 (8
  clients, 5 interleaved runs each) within noise of the base (medians 52.1k / 53.0k TPS).

## Round 21 (agent 78): TPC-H plans, the tail

The same comparison as round 20's: `EXPLAIN` (costs on) of the 22 queries at SF1 on
PostgreSQL 18.6 (container `a78-pg`) and the engine, loaded with the same data by
`tools/bench` and analyzed, compared node by node, with parallel query at its defaults and
again with `max_parallel_workers_per_gather = 0`. Before (this round's base, `c7f59d74`):
14 of 22 had PostgreSQL's shape (agent 64's 15 less q09, which flips with PostgreSQL's own
ANALYZE: re-analyzing part flips its plan there too); after: **16 of 22**, and **19 of 22**
serial. Every result hash agrees with PostgreSQL's. Timings weren't the point (one run on a
shared machine, load 15, other rounds' executor changes in between): q20 took 0.9 s and q07
4.1 s (2.8 s and 7.3 s in round 20), the rest within round 20's spread.

What differed and why (STATUS.md, round 21):

1. **q07**: the query's FROM subquery is pulled up with its scans planned on their own level
   first, and those weren't plannable when the group took the OR clause's restrictions out
   for each nation (extract_restriction_or_clauses), so the OR kept its whole selectivity
   besides them: the two nations' join 1 row, the query 40 for PostgreSQL's 6,454, and a
   nested-loop plan from the nations outwards.
2. **q18**: reduce_unique_semijoins ran only for a semi join of two items; in a group (the IN
   among customer, orders and lineitem) the sub-select, grouped by l_orderkey, makes it an
   inner join (Hash Join over the GroupAggregate, no Unique).
3. **q20**: the outer IN's sub-select has its own correlated sub-select (`ps_availqty >
   (SELECT 0.5 * sum(l_quantity) ... WHERE l_partkey = ps_partkey ...)`), which the pull-up
   refused; such a qual stays with its relations.
4. **q16**: count(DISTINCT) has no partial form; the GroupAggregate's presorted input may be
   the parallel join sorted in the workers under a Gather Merge, which PostgreSQL finds
   cheaper than sorting the gathered rows.
5. **q02** (its plan matched by luck): EXPLAIN's hash join scaled the inner key's distinct
   values by the hashed side's rows (a join's 159,537) instead of its relation's, a bucket
   of 4 rows for 1, so each probe ran the correlated SubPlan of the hash clause 4 times:
   107,845 for PostgreSQL's 56,375. That also decided q20's Gather (a serial join that isn't
   parallel safe and the join over a gathered input are kept as add_path keeps them).

Left: q05 / q08 (partial paths only along the serial join tree: PostgreSQL's best partial
plan for q05 joins customer ⋈ nation ⋈ region first and orders by orders_o_custkey_idx, a
tree whose serial form costs 197,316 against the engine's serial best 97,786; it needs
partial paths per join relation in the join search), q18 (a GroupAggregate over an
Incremental Sort on c_custkey from a merge join: group keys reordered by the input's order
and that order kept at the join), q21 (two joins over 1 row, 0.5 apart), q13 after an engine
restart (customer's cold heap model has 3,588 pages for 3,640: 147,857 rows, and the
HashAggregate then fits hash_mem by 1%), q09 / q10 serial (ANALYZE's sample).

Other workloads, against the same reference: pgbench's statements (scale 10) and twenty
ORM-style queries over its tables (point lookups with LIMIT, counts, IN lists, joins with
ORDER BY / OFFSET, EXISTS and NOT IN, correlated scalar sub-selects, GROUP BY with MIN /
MAX, row comparisons) have PostgreSQL's plans; their estimates differ only by ANALYZE's
sample (a range over pgbench_accounts.aid from each side's own histogram). An INSERT that
left columns to their defaults differed: PostgreSQL's source computes every column
(`Result (width=60)` where the engine showed 4, and nextval's cost per row; second batch:
the engine's source counts them now). Left: an INSERT ... SELECT with ORDER BY keeps
`Subquery Scan on "*SELECT*"` over the Sort.

## Round 21 (agent 90): analytic executor, second batch: the read path

The same harness, both servers on cores 28-31, the client on 4-5 and 12-13, the machine
shared with other agents (load 20-32 during these runs; 2 when it was quiet for an hour, too
short for a full set). **pg** is PostgreSQL 18.6 (`a90-pg`, the bench settings);
**before** the engine at `b6659eb9` (this round's base); **after** with this batch. Each
engine run follows a restart and a first read of lineitem, orders and partsupp. Times are
the median of three interleaved rounds (pg / after / before, before / pg / after, after /
before / pg) of each round's median of 3; every result hash agrees with PostgreSQL's.
**Peak memory**: the engine's VmHWM over the round (the warm-up scans included) and its
sampled RSS; PostgreSQL's container `anon + shmem` sampled during the queries.

| query | pg | before | after | after / pg | after / before |
|---|---|---|---|---|---|
| q01 | 2.289 | 3.338 | 2.826 | 0.81x | 1.18x |
| q02 | 0.247 | 1.415 | 1.404 | 0.18x | 1.01x |
| q03 | 0.515 | 1.706 | 1.465 | 0.35x | 1.16x |
| q04 | 0.214 | 0.863 | 0.683 | 0.31x | 1.26x |
| q05 | 0.404 | 1.419 | 1.215 | 0.33x | 1.17x |
| q06 | 0.494 | 2.331 | 1.570 | 0.31x | 1.48x |
| q07 | 0.461 | 7.152 | 3.676 | 0.13x | 1.95x |
| q08 | 0.229 | 3.747 | 1.485 | 0.15x | 2.52x |
| q09 | 1.613 | 7.678 | 3.163 | 0.51x | 2.43x |
| q10 | 0.730 | 4.716 | 3.629 | 0.20x | 1.30x |
| q11 | 0.109 | 1.320 | 0.371 | 0.29x | 3.56x |
| q12 | 0.706 | 5.126 | 2.429 | 0.29x | 2.11x |
| q13 | 0.888 | 5.430 | 5.852 | 0.15x | 0.93x |
| q14 | 0.227 | 2.782 | 1.867 | 0.12x | 1.49x |
| q15 | 0.393 | 1.993 | 1.336 | 0.29x | 1.49x |
| q16 | 0.321 | 1.209 | 1.167 | 0.28x | 1.04x |
| q17 | 0.983 | 2.833 | 2.879 | 0.34x | 0.98x |
| q18 | 2.906 | 5.974 | 6.300 | 0.46x | 0.95x |
| q19 | 0.089 | 0.597 | 0.496 | 0.18x | 1.20x |
| q20 | 0.124 | 1.048 | 0.837 | 0.15x | 1.25x |
| q21 | 0.769 | 2.850 | 3.192 | 0.24x | 0.89x |
| q22 | 0.164 | 0.808 | 0.777 | 0.21x | 1.04x |
| all 22 | 14.88 | 66.33 | 48.62 | 0.31x | 1.36x |
| peak memory (MB) | 1,711 | 4,380 | 4,110 | | |

q13, q17, q18 and q21 spend their time on the executor's thread (groupings, sorts, the
serial plan's joins), which this batch leaves as it was; their differences are within the
machine's spread. The engine's CPU seconds per run, two alternating pairs: q13 9.2 / 11.3 before, 10.8 / 11.3
after; q18 14.8 / 17.6 and 14.2 / 16.1; q21 8.5 / 9.9 and 8.6 / 9.7; q17 6.7 / 8.1 and 7.3 /
7.0. q01 9.8 / 10.3 before and 8.6 / 10.1 after (an earlier build that read a column's number
again for every aggregate taking it was 22% over: `LazyBuf::nums` keeps it per row).

**pgbench select-only** (scale 10, `-M prepared`, 8 clients, 10 s, three interleaved runs
after a restart each): before 41,307 / 37,737 / 40,881 TPS (median 40.9k), after 48,047 /
41,072 / 44,110 (44.1k); peak RSS 346 MB before, 254 MB after (the cache's blocks are
smaller).

### What the profiles showed

- **Block cache** (q07, q08, q09, q11, q12, q18): zstd and the allocation, page faults and
  copies of decoding blocks were 25-35% of q09's CPU. TPC-H SF1's SST entries are 2.0 GB
  decompressed (608 MB on disk), but a cached block also held a copy of every key and an
  offset table: 3.2 GB in all, so the 2 GB cache held two thirds, and random index fetches
  mostly decompressed. With the cache at 8 GB the base took 52.9 s in all instead of 80.4 s
  (q09 10.4 -> 3.6, q07 8.6 -> 3.5, q11 1.9 -> 0.44); the RSS was 4.5 GB.
- **Row decoding** (q06, q14, q15, q01): a worker made a `Vec<Value>` of every row it read,
  decoding every needed column (a numeric: an `Arc` and a `BigInt` per value) before the
  filter ran; with 2% of lineitem passing q06's filter, 98% of that went straight to
  `drop`.
- **Serial index scans under merge joins** (q12): the executor runs the serial plan, and
  q12's is a merge join over a full `orders_pkey` scan: 1.5M point fetches on the executor's
  thread while the workers waited (the EXPLAIN shows PostgreSQL's parallel hash join).

### What changed

- **Cached blocks hold the file's entries** (`storage/sst.rs` `Block`, `BlockPos`): a whole
  key every 16 entries (restarts; the writer emits them, an older file's block is rewritten
  with them in one pass as it is read), a seek searches the restarts, a cursor rebuilds keys
  as it moves; reverse scans and spill files use `Block::expand` (the old layout, per read).
  A block now costs its decompressed bytes plus 4 bytes per restart. The file format is the
  same (a restart is an entry with `shared = 0`).
- **Rows read in place in the scan workers** (`exec/parscan.rs` `Lazy`): the column offsets
  walked once, a column decoded when the filter, a join key or an aggregate first reads it,
  numbers read without making values (`Cols::num`, `row::peek_num`, once per row); a row
  the filter rejects or the partial aggregate takes is never made. Numeric comparisons in
  kernels compare exact i128s (`K::NumCmp`), partial sums of a numeric column add its number
  (`Partials::cols`).
- **Parallel fetches**: full btree index scans of big tables run in the workers for the
  executor's thread too (q12 5.1 -> 2.4 s), and a bitmap heap scan that isn't dense fetches
  its sorted row keys in the workers (`start_parallel_keys`).
- `get_varint`'s one-byte case inline; `tools/bench/run.sh` takes `CACHE_MB`.

### Where TPC-H is still slower

- The executor runs the serial plan and parallelises its scans and probes: where the serial
  plan's join order or grouping differs from the parallel one (q13's grouping over a right
  join, q18's 1.5M-group GroupAggregate merged on the executor's thread, q10 / q03's sorts and
  merge joins), the executor's thread does PostgreSQL's workers' work. Sorts, window
  functions and hash builds still run on it.
- Modelled tables (customer, part, supplier) are read serially (q02, q11, q16, q22).
- numeric values still take two allocations when made; rows reaching the executor's thread
  make them.
- A block of lineitem is still decompressed again when a full scan cycles it out (q01, q06,
  q14: zstd 6-7%); a scan-resistant cache (a sequential scan's blocks not displacing hot
  ones, as PostgreSQL's ring buffer) is the next step for the cache.

### Verification (round 21, agent 90)

- `cargo test -p pg-engine` (with `parallel_scan.rs`'s new `parallel_fetches_match_serial`
  and `sst.rs`'s `block_positions`), `cargo clippy -p pg-engine --all-targets` with no new
  warnings, `difftest smoke`.
- The regression suite in full (debug build, private reference): 51,148 / 51,150, no unit
  changed against the committed baseline (the first batch, and again with the second).

## Round 21 (agent 101): executor memory and parallel work above the scan, first batch

Same harness and settings as agent 90's round; both servers on cores 4-7, the client on
2-3, the machine shared (load 18-25 throughout). **pg** is PostgreSQL 18.6 (`a101-pg`);
**before** is the branch at `a04b5d87`, **after** this batch. Two interleaved full rounds
each (before, after, after, before); each cell is the mean of the two rounds' medians of
3. **CPU** is the engine's user + system seconds per run (its 3 reps / 3, the first
rep's planning included). Every result hash agrees with PostgreSQL's.

| query | pg | before | after | after / pg | after / before | CPU s before | CPU s after |
|---|---|---|---|---|---|---|---|
| q01 | 2.250 | 3.244 | 3.208 | 0.70x | 1.01x | 10.1 | 10.5 |
| q02 | 0.265 | 1.587 | 1.847 | 0.14x | 0.86x | 1.9 | 2.4 |
| q03 | 0.619 | 1.876 | 1.995 | 0.31x | 0.94x | 5.6 | 5.5 |
| q04 | 0.212 | 0.955 | 1.130 | 0.19x | 0.85x | 2.2 | 2.4 |
| q05 | 0.492 | 1.441 | 1.714 | 0.29x | 0.84x | 3.0 | 3.5 |
| q06 | 0.702 | 2.206 | 2.418 | 0.29x | 0.91x | 4.5 | 4.9 |
| q07 | 0.512 | 4.776 | 1.570 | 0.33x | 3.04x | 4.8 | 4.3 |
| q08 | 0.242 | 2.030 | 0.837 | 0.29x | 2.43x | 2.0 | 1.6 |
| q09 | 1.706 | 4.040 | 4.169 | 0.41x | 0.97x | 13.7 | 12.4 |
| q10 | 0.755 | 4.495 | 3.875 | 0.19x | 1.16x | 9.3 | 9.2 |
| q11 | 0.146 | 0.501 | 0.431 | 0.34x | 1.16x | 0.5 | 0.4 |
| q12 | 0.828 | 2.871 | 2.820 | 0.29x | 1.02x | 8.7 | 8.5 |
| q13 | 1.053 | 5.877 | 2.646 | 0.40x | 2.22x | 11.0 | 6.6 |
| q14 | 0.263 | 2.598 | 2.400 | 0.11x | 1.08x | 5.1 | 4.7 |
| q15 | 0.487 | 2.105 | 1.689 | 0.29x | 1.25x | 4.6 | 4.4 |
| q16 | 0.350 | 1.400 | 1.267 | 0.28x | 1.11x | 1.8 | 1.7 |
| q17 | 1.106 | 2.985 | 2.589 | 0.43x | 1.15x | 8.7 | 7.6 |
| q18 | 3.513 | 6.798 | 4.574 | 0.77x | 1.49x | 16.8 | 13.2 |
| q19 | 0.094 | 0.573 | 0.261 | 0.36x | 2.20x | 0.6 | 0.5 |
| q20 | 0.136 | 1.096 | 1.263 | 0.11x | 0.87x | 1.1 | 1.3 |
| q21 | 0.799 | 4.796 | 3.345 | 0.24x | 1.43x | 12.0 | 9.7 |
| q22 | 0.124 | 1.401 | 0.916 | 0.14x | 1.53x | 1.4 | 1.0 |
| all 22 | 16.65 | 59.65 | 46.96 | 0.35x | 1.27x | 129.8 | 116.3 |
| peak memory (MB) | 1,711 | 4,131 | 2,975 | | | | |

q02, q04, q05 and q20 vary by more than their differences: alternating fresh engines
(base, new, base, new; 3 reps each) gave q02 8.0 / 8.9 / 8.8 / 8.9 CPU s, q04 12.6 / 12.1 /
12.5 / 12.7, q05 16.5 / 17.0 / 14.9 / 14.5, q01 35.8 / 35.5 / 33.8 / 34.6.

**pgbench** (scale 10, `-M prepared`, 8 clients, 10 s, fresh engine each, before / after /
after / before): select-only 28.1k / 37.1k / 46.2k / 47.8k TPS, tpcb-like 3.42k / 3.73k /
3.73k / 3.54k; peak RSS 397 / 414 / 399 / 415 MB. No difference beyond the spread.

### What held the 4.1 GB

Measured per query on a fresh engine with `--cache-mb 64` (so the cache isn't the peak),
then over the whole round:
- **q18's grouping**: 1.5M groups of a GroupAggregate over `lineitem_pkey`, all held
  (the engine hashed every grouping, then sorted the groups), 1.4 GB. Now streamed.
- **The block cache**: `--cache-mb 2048`, filled to its limit. A full scan's blocks could
  push out everything else (and did, for the warm-up and q01); index lookups of q03, q07,
  q09, q21 touch most of lineitem anyway, so the cache ends full either way (PostgreSQL's
  shared buffers end at about 1.6 GB of its 2 GB). TPC-H SF1's entries decompressed are
  2.2 GB, more than PostgreSQL's 1.6 GB of pages: the cache holding the same working set
  takes more bytes than shared buffers do.
- **Size models** of the modelled tables (part, customer, supplier): about 120 MB kept for
  part, and the model's build held every row decoded at once (+150 MB transient).
- Per-query working memory otherwise 100-450 MB (hash tables of wide rows: q02's partsupp
  ⋈ supplier side, q10, q13).

### What changed

- **Streaming GroupAggregate** (`exec/agg.rs` `Stream`): a grouping PostgreSQL runs as a
  GroupAggregate directly over an ordered path (no Sort under it), whose keys are its
  input's ordered columns (`Node::key_order`: a forward btree scan's key columns in the
  index's directions), finishes each group when the next one starts, as
  agg_retrieve_direct does, the last group finalized before the next group's first row is
  fed. One group is held instead of all (q18: 1.4 GB less), and a LIMIT above stops the
  input: `SELECT k, sum(10 / (k - 700)) ... GROUP BY k LIMIT 3` now returns its 3 rows as
  PostgreSQL does instead of the error of a later group (golden `a101_stream_group`).
  Parts from a parallel scan's workers merge into the open group in order.
- **Scan-resistant block cache** (`storage::bulk_read`, `Store::bulk_range`,
  `BlockCache::insert_if_room`): a parallel scan of a range whose blocks exceed a quarter
  of the cache (PostgreSQL's ring rule, NBuffers / 4) is a bulk read; its blocks go into the
  cache only where it has room and evict nothing. The probes and lookups the workers run
  over those rows fill the cache as before.
- **Projection moves** (`ProjectNode::moves`): a projected plain column nothing else reads
  is moved out of the input row instead of copied (agent 90's left-out change). q13 spent
  28% of its CPU in `Value::clone` there.
- **Partial aggregation through a projection** (`Node::projection`,
  `pipeline_projected`): an aggregation over a FROM subquery's computed outputs (q07, q08)
  now runs in the workers with the projection's expressions put in for its columns; only
  when every computed expression is read by a key or argument whatever the row (not only
  in a CASE branch), so the workers compute it for every row the projection would. Rows a
  kernel leaves to the serial path come out projected.
- **Modelled tables in parallel** (`start_parallel`): a modelled heap whose rows aren't all
  stored in key order (customer: 42 rows went into earlier pages' free space at load) is
  read on the executor's thread, put in heap order as the serial scan does, and its runs
  handed to the workers when joins or an aggregation above run there (q07's lookups of
  orders and lineitem per customer, q08, q19's part). A heap laid out afresh
  (`unstamped`) whose keys are in heap order splits by keys when the scan reads no TIDs.
- **Lighter structures**: hash join chains flat (`pghash::JoinTable`: a start per bucket and
  a row index per tuple, instead of a vector per bucket and the tuple list kept); a heap
  model's build decodes one row at a time; a hashed grouping's output order no longer
  copies every group key.

### Tried and left out

A two-tier cache (decompressed blocks in a quarter of the budget, the files' compressed
bytes of blocks read before in the rest), with zstd and with LZ4 blocks (`lz4_flex`; disk
686 MB zstd, 885 MB LZ4, PostgreSQL 1,602 MB). Whole rounds on one engine, CPU seconds over
the round (3 reps each) and peak:

| cache | CPU s | peak MB |
|---|---|---|
| zstd, one tier | 393 | 2,906 |
| LZ4, one tier | 378 | 2,903 |
| LZ4, decompressed tier 25% | 418 | 2,214 |
| zstd, decompressed tier 25% | 669 | 2,018 |

The compressed tier holds the working set in a third of the bytes, but a 16 kB block
decompressed for each lookup that misses the smaller first tier costs q07, q09, q11 and
q20 30-100% more CPU (zstd) or 30-60% (LZ4). Cheaper misses (smaller blocks for point
lookups, or a row cache) would make it worth it; LZ4 alone saves 4% CPU (q01 -14%) for
29% more disk and a format addition older binaries can't read.

### Where memory and time still go

- Peak 2.98 GB against PostgreSQL's 1.71 GB: the cache (2 GB of decompressed blocks, full by
  q09) plus 0.8-1.0 GB of models, allocator pages and per-query working memory.
  Below PostgreSQL needs the working set cached in fewer bytes (above) and hash tables of
  only the columns read.
- The serial plan's hash builds, sorts and right-join groupings still run on the
  executor's thread (q10, q13's c_custkey grouping, q21); numeric values still allocate twice.

### Verification (round 21, agent 101)

- `cargo test -p pg-engine` in debug and release (deep_input and the parallel tests
  included), `cargo clippy -p pg-engine --all-targets` with no warning in the files changed,
  the difftest smoke, the isolation suite 121 / 122 specs (1,647 / 1,648 permutations), the
  regression suite in full (debug, private reference) 51,148 / 51,150 with no unit changed
  against `results.tsv`; every TPC-H result hash equal to PostgreSQL's in all four rounds.

## Round 22 (agent 112): executor memory, round 4

Same harness and settings; both servers on cores 28-31, the client on 6-7, the machine
shared (load 20-32 throughout). **pg** is PostgreSQL 18.6 (`a112-pg`); **base** the branch
at `7bb31b96`. Each engine round is a fresh start, a first read of lineitem, orders and
partsupp, then the 22 queries 3 times each; rounds interleaved (pg, base, new, pg, base,
new). **CPU** is the server's user + system seconds per run (`bench` reports it now:
`cpu_s`, the process's or the container cgroup's). Every result hash agrees with
PostgreSQL's in every round. Peak memory is the engine's VmHWM over the round and
PostgreSQL's sampled `anon + shmem`.

### Batch 1: cached blocks in a compact form

| | pg | base | batch 1 |
|---|---|---|---|
| all 22, s (two rounds) | 14.96 / 14.27 | 44.66 / 42.83 | 40.37 / 50.21 |
| CPU s | 35.4 / 35.2 | 106.3 / 102.6 | 95.5 / 120.5 |
| peak memory (MB) | 1,846 / 1,850 | 2,990 / 3,000 | **2,053 / 2,027** |

Times are within the machine's spread (the second batch-1 round ran during a load spike;
an interleaved run of q10, q12, q14, q15 and q18 alone gave 10.52 s / 28.3 CPU s base and
10.73 / 28.8 batch 1). **pgbench** (scale 10, `-M prepared`, 8 clients, 10 s, fresh engine,
base / new / base / new): select-only 43.7k / 45.2k / 46.4k / 45.0k TPS, tpcb-like 2.67k /
2.99k / 2.74k / 2.83k; peak RSS 287 / 266 / 295 / 274 MB.

**What the cache held.** TPC-H SF1's SST entries are 2.14 GB decompressed (`a112`'s
scan of every file): row values 1.56 GB, key suffixes 0.47 GB, entry headers 0.11 GB. The
tables alone (1.31 GB) are PostgreSQL's size (1.34 GB of heap); the excess was the
indexes: 830 MB against PostgreSQL's 330 MB. Two things each entry repeats: the 8-byte
version at the end of every key (33.1M of the 33.3M entries carry their block's), and a
secondary index entry's value, which is the row's storage key (17 bytes: the table's
prefix and the row key the index key already ends with; 420 MB in all).

**What changed** (`storage/sst.rs`, `Block::decode`): a block read into the cache is
written in a compact form of the file's: an entry whose version is its block's leaves it
out (`F_TS`, put back as the cursor rebuilds the key), and an index entry whose value is
derivable from its key stores only the length of the key's tail (`F_DERIVED`; the cursor
makes the value). The file format is unchanged. The working set now takes about 1.45 GB of
cache instead of 2.14 GB, so the 2 GB cache no longer fills: the peak fell 1.0 GB, and
random lookups (q07, q09, q20, q21) stop decompressing blocks the cache dropped.

**What else holds memory** (fresh engine, `--cache-mb 64`, RSS after each statement): 96 MB
after `select 1` (catalogs, sessions); the size models of part (+~70 MB) and customer
(+~65 MB), whose btree models took ~180 bytes an entry (`btsim::Key` in four allocations
and a TID vector each); per-query working memory 100-250 MB above that (q10's and q13's
hash builds and sorts of wide rows).

### Batch 2: bitmap quals on index entries, date ranges bounded by timestamps, compact hash tables and btree models

**before** is the branch at `7bdb4584` (agent 102's batch, with batch 1), **after** this
batch on top of it; the first pair of rounds (the second ran during a load spike, load 44:
58.0 / 81.8 s before, 48.3 / 98.2 s after, every query slower in both binaries):

| query | pg | before | after | CPU s pg | CPU s before | CPU s after |
|---|---|---|---|---|---|---|
| q14 | 0.25 | 3.01 | **1.08** | 0.6 | 5.7 | **1.3** |
| q04 | 0.25 | 1.86 | 0.78 | 0.7 | 3.5 | 2.2 |
| q06 | 0.67 | 4.32 | 2.03 | 1.7 | 7.9 | 4.9 |
| q20 | 0.18 | 0.90 | 1.06 | 0.2 | 0.9 | 1.1 |
| all 22 | 19.54 | 57.97 | 48.26 | 49.4 | 139.3 | 118.3 |
| peak memory (MB) | 1,847 | 2,077 | 2,051 | | | |

Every result hash equal to PostgreSQL's in all rounds. **pgbench** (scale 10, `-M
prepared`, 8 clients, 10 s, fresh engine, before / after / before / after): select-only
60.7k / 65.0k / 55.8k / 52.6k TPS, tpcb-like 4.24k / 4.47k / 4.48k / 3.70k (the spread of
this machine; neither workload runs the paths changed); peak RSS 285 / 252 / 273 / 288 MB.

- **q14's root cause** (profile: 80% in a parallel full scan of lineitem): the bitmap's
  range on `l_shipdate` stopped at `1995-09-01` only. The upper bound is a timestamp (`date
  '1995-09-01' + interval '1' month`), and a date column took no bound of another type, so
  the range ran to the end of the index (2.4M entries), counted as 40% of the table, and
  `dense_bitmap_scan` read all 6M rows. Now (`sql/paths.rs`) a date column compared with a
  timestamp takes the timestamp's date as an inclusive bound (a superset; the quals are
  checked on each row), and the density count checks the quals on the entries. q04 and
  q10's `o_orderdate` ranges have the same shape.
- **Bitmap quals checked on the entries** (`exec/scan.rs` `entry_columns`, `entry_row`):
  `bitmap_keys` read every row its index range selected (a point lookup and a decode of
  every column each) to check the index quals, and the heap scan read the rows again.
  When the quals read only btree columns of fixed-width types whose entries give their
  values back (`index_only_columns`' conditions), they are checked on the entries. The
  80,409-row date range of q14 alone: 580 -> 169 ms.
- **Hash tables keep only the columns read** (`exec/pipe.rs` `Built::compact`, `Inner`): a
  hash join's inner rows keep the columns its output, condition and keys read; the others
  read as NULL. q10's lineitem ⋈ orders build (114K rows of 25 columns, 3 read) took 800
  bytes a row for the values alone.
- **btree models in fewer allocations** (`catalog/btsim.rs`): a `Key` is one allocation
  (counts, ends and sizes as u16s, then the bytes) instead of four, and an item's single
  TID is inline (`Tids`). Models of part, customer and supplier: 23 MB less (fresh engine,
  `--cache-mb 64`: 312 -> 289 MB after reading the three).

**Agent 102's inline scans** (`7bdb4584^` against `7bdb4584`, q03 / q05 / q06 / q10, three
interleaved rounds): wall times varied 2x between rounds of one binary (load 40), so only
CPU says anything: 33.7 CPU s before and 35.9 after on average (+6.5%), q06 +12% (7.5 ->
8.4), q10 +8%, q03 and q05 the same. q06 is a parallel scan of lineitem past 1,023 pages,
so neither scan policy applies to it; the per-row decode (the out-of-line checks) is the
likelier cause, which agent 117's per-row scan work covers.

### Where memory and time still go

- Peak 2.05 GB against PostgreSQL's 1.71-1.85 GB: about 1.45 GB of cache (everything
  TPC-H reads), 96 MB after the first statement, ~110 MB of size models, and per-query
  working memory. The largest working sets are sorts of wide rows (q10's Sort over a
  37-column join keeps every column, NULLs included: compact them as the hash tables are)
  and q13's grouping.
- Point lookups cost 5-7 µs (q02's 158K partsupp fetches by `ps_suppkey`, q20's 8,332
  SubPlan runs): `Store::get_versions` per key over the memtable and every L0 file (bloom,
  block index, cache shard lock, a block seek of up to 15 entries), then a decode of every
  column.
- Merge joins build their whole inner side (agent 103's finding): a streaming merge with
  mark / restore would read only what a LIMIT needs and hold nothing.
- Not done this round: parallel hash builds, sorts with gather merge, groupings over
  right joins in the workers, numeric values' two allocations.

## Round 22 (agent 91): partial paths in the join search

The same plan comparison as round 21's (`EXPLAIN` of the 22 TPC-H queries at SF1, PostgreSQL
18.6 in `a91-pg` and the engine loaded with the same data by `tools/bench` and analyzed).
q05 and q08 now have PostgreSQL's parallel plans: the join search keeps partial paths per
join relation, and the cheapest partial path's tree (customer x nation x region in the
workers, orders by orders_o_custkey_idx, for q05; customer by customer_pkey for q08) is the
one EXPLAIN's parallel plans follow (STATUS.md round 22). With the engine restarted before
the run: **19 of 22** parallel (q13 differs after a restart, agent 85's; q18, q21 as before),
**19 of 22** serial (q18, q21; q10 by ANALYZE's sample). Every result hash agrees with
PostgreSQL's. `EXPLAIN ANALYZE` shows (and runs) the same plan: q05's partial tree with
its own row counts.

**Planning time.** CPU time of the engine process per statement, 2,000 statements per query
in one session, the base (`69743b6c`) and this batch interleaved three times, the lower of
each (read from `/proc/<pid>/stat`, which the machine's load of 20-30 moves less than wall
time, whose run-to-run spread was 30% per query):

| workload | base | batch | batch / base |
|---|---|---|---|
| pgbench's statements and ten ORM-style queries over pgbench_accounts (scale 10), EXPLAINed | 8,665 us | 8,545 us | 0.99 |
| the same, executed (the eight that run in under a millisecond) | 15,415 us | 15,245 us | 0.99 |
| TPC-H's 22 queries, EXPLAINed (200 each) | 140.3 ms | 141.9 ms | 1.01 |

OLTP planning is unchanged: a join of a lookup to a big table makes its leaves' partial scans
(from the serial paths already made) and stops there, since no parallel plan could beat the
serial one by the bound in `partial_pass`. The first version, which made the partial join
paths inline and named the partial trees' scans by a walk of every plan in EXPLAIN, cost
12% more CPU per OLTP join statement (38% for a three-way join); both are gone. The TPC-H
queries that run the pass pay for PostgreSQL's search, less the join pairs whose outer side's
cheapest partial path already costs more than the bound (any partial join over it costs at
least that: without that, the TPC-H total was 1.14x, q05 1.50x): q05 13.9 ms -> 15.2 ms
(1.09x), q08 23.3 -> 24.7 (1.06x), q07 and q09 the same. Per-query noise is about 20% at this
load; the TPC-H row is a later run than the OLTP rows, at another load.

Memory: the pass's partial paths live with the search (as the serial ones); a skipped pass
keeps one record per join pair (its relations and clause list), dropped with the search.

## Catalog, DDL and sessions (round 18, agent 44)

Measured on the same machine and harness as above (PostgreSQL in `a44-pg`, the pinned image
and settings; both servers on cores 32–35, clients on 14–17), while other agents' builds and
regression runs loaded the machine (load 20–30): read differences under ~30% as noise. Each
cell is the median of the runs given; **pg** PostgreSQL 18.6, **before** the engine at this
round's start (`ca0c077`, and `6dd2469` after a rebase), **after** this round's engine.
New workloads: `ddl` gained `*_after_ddl` cases (a `COMMENT ON TABLE` before the query, so it
reads a catalog version no statement has read yet) and `setup=0` / `keep=1` / `only=` for
profiling; `scale` (many tables in many schemas, a partitioned table with many partitions,
DDL timed per step as the catalog grows, catalog queries at the end); `startup-catalog`
(in-process: a database's first connection and catalog query, then a second session's);
`idle --settle-ms` (how long the sessions sit idle before memory is read).

### DDL and catalogs, 500 tables (ms per statement; pg 3 runs, before 2, after 3)

| case | pg | before | after | after / pg |
|---|---|---|---|---|
| CREATE TABLE (serial key, check, default) | 3.98 | 7.17 | 0.74 | 5.4x |
| CREATE INDEX | 1.29 | 13.4 | 0.85 | 1.5x |
| ALTER TABLE ADD COLUMN | 0.55 | 18.6 | 1.30 | 0.42x |
| DROP TABLE | 7.17 | 24.6 | 10.3 | 0.70x |
| information_schema.columns of a table | 6.33 | 25.6 | 17.4 | 0.36x |
| information_schema.tables count | 3.84 | 0.73 | 0.73 | 5.3x |
| psql `\d t`: lookup | 0.71 | 9.1 | 1.31 | 0.54x |
| psql `\d t`: columns | 0.50 | 18.7 | 2.48 | 0.20x |
| psql `\d t`: indexes | 0.94 | 11.5 | 1.33 | 0.71x |
| `pg_class` × `pg_attribute` join | 7.04 | 43.7 | 20.0 | 0.35x |
| ORM constraints query | 41.7 | 49.1 | 30.0 | 1.39x |
| `'t'::regclass` | 0.20 | 0.16 | 0.09 | 2.2x |
| psql `\d t` columns, after DDL | 0.90 | 64.3 | 27.5 | 0.03x |
| psql `\d t` lookup, after DDL | 0.52 | 36.3 | 6.72 | 0.08x |

(A catalog query's figure is the mean over 20 runs in one session, the first of which, after
the setup's DDL, generates the catalog's rows: `psql \d t: columns` runs in 0.4–0.6 ms once
generated, as PostgreSQL's does, `EXPLAIN ANALYZE` planning 0.41 ms and execution 0.56 ms.)

### Many tables: 10,000 tables in 20 schemas and a table of 1,000 partitions (ms per statement)

| case | pg | after | after / pg |
|---|---|---|---|
| CREATE TABLE, at 2,000 / 6,000 / 10,000 tables | 3.24 / 3.02 / 2.73 | 1.91 / 6.24 / 12.8 | 1.7x / 0.48x / 0.21x |
| CREATE TABLE ... PARTITION OF (1,000) | 4.88 | 17.3 | 0.28x |
| ALTER TABLE ADD COLUMN | 0.51 | 23.1 | 0.02x |
| CREATE INDEX | 1.00 | 13.0 | 0.08x |
| DROP TABLE | 4.75 | 77.3 | 0.06x |
| DROP SCHEMA ... CASCADE, 20 schemas (s, total) | 15.8 | 3.03 | 5.2x |
| psql `\d t`: lookup | 1.36 | 9.69 | 0.14x |
| psql `\d t`: columns | 0.99 | 20.0 | 0.05x |
| psql `\dt` of a schema | 9.34 | 24.0 | 0.39x |
| information_schema.columns of a table | 3.23 | 65.4 | 0.05x |
| `count(*)` of pg_class | 5.11 | 17.3 | 0.30x |
| `'t'::regclass`, a point select | 0.18, 0.24 | 0.11, 0.14 | 1.6x, 1.7x |

The engine before this round couldn't be run to 10,000 tables in reasonable time (every DDL
wrote the whole catalog: 69 ms per CREATE TABLE at 5,000 tables and growing). At 5,000 tables
(500 partitions; pg / before / after): CREATE TABLE 4.8 / 69.0 / 11.3, ALTER TABLE ADD COLUMN
0.69 / 104 / 11.8, CREATE INDEX 1.78 / 101 / 21.6, DROP TABLE 9.0 / 103 / 34.4, psql `\d t`
columns 1.07 / 105 / 15.3, the 10 schemas' DROP SCHEMA CASCADE 12.0 s / 312 s / 3.0 s. (The
"after" at 5,000 tables is an earlier build of this round, before optimisations 2 and 6.)

### Memory per idle session, startup

| | pg | before | after |
|---|---|---|---|
| memory per idle session (100 sessions after a `pg_class` query; 3 s idle) | 1,544–1,554 kB | 1,925–3,138 kB | 533–543 kB |
| the same, 0.5 s idle (before the idle trim) | | 3,138 kB | 769–810 kB |
| connection setup (SCRAM) | 16.5 ms | 7.1 ms | 7.6 ms |
| in-process, 2,000 tables: a database's first connection (its catalog loaded) | | 30–34 ms | 70–75 ms (231 ms once, rewriting a catalog of the old format) |
| ... its first `count(*)` of pg_class | | 98–114 ms | 37–82 ms |
| in-process, 10,000 tables: first connection; first `count(*)` of pg_class | | | 138–155 ms; 70 ms |

`Engine::open` stays under 1 ms (databases open on first use). The per-database figure of the
idle workload stays noise (mimalloc returns memory as databases close).

### Optimisations

1. **The catalog stored as a record per object** (`catalog/persist.rs`, catalog format 1): a
   header record under the old key (scalar fields and lists) and one record per entry of each
   per-object map and set (a tag for the map, the oid). A DDL commit writes the header and only
   the records that changed (`persist::writes`: maps of `Arc`s compared by pointer, which a DDL
   statement's copy shares for every object it didn't touch, the rest by value), computed at
   commit against the catalog published then (when other DDL committed since the
   transaction's began, agent 37's `txn::catmerge` merges the catalogs under the commit lock,
   and the records are those of the merged catalog over the current one), in the commit's one
   WAL record, so header and records change together and a crash leaves either version. Rollbacks and savepoints are unaffected (the
   catalog under construction lives in the transaction as before; `Txn::writes` holds a
   stand-in for it). `Database::open` loads the header and the records, and stores what
   `reindex` brings up to date at once. The bytes a statement writes no longer grow with the
   catalog (`tests/catalog_storage.rs` `ddl_writes_dont_grow_with_the_catalog`). Every DDL
   statement had serialised the whole catalog as JSON (and zstd-compressed and checksummed it
   in the WAL and SSTs): 500 tables, CREATE TABLE 6.85 → 3.44 ms, CREATE INDEX 13.4 → 7.2,
   ALTER TABLE 18.6 → 13.0.
2. **Copy-on-write catalog maps**: the maps with an entry per relation or function (`tables`,
   `indexes`, `views`, `sequences`, `functions`, `relstats`, `frozen_xids`, `class_order`, and
   the name and inheritance indexes) are `Arc`s a catalog's copies share until one changes them
   (`Arc::make_mut`); a DDL statement copies the catalog a few times (binding defaults and
   checks over the statement's catalog), each copy was as big as the catalog. The commit's diff
   skips maps the two versions share.
3. **System catalogs' rows shared by a database's sessions** (`catalog/system.rs`
   `rows_shared`, `Database::sys_rows`): agent 36's per-session cache, keyed now by the
   settings' values (`Settings::values_key`) instead of a per-session generation, and by the
   session's temporary schema when it has one; a catalog's rows are generated once per catalog
   version for all sessions, and each session no longer holds its own copies. A scan reads the
   shared rows and copies only those it returns (it copied the whole catalog per statement).
4. **Equalities served by a hash** (`SysRows::lookup`): a scan whose first conjunct is
   `column = constant` (`attrelid = 'x'::regclass`, `oid = 16385`), or whose index path's
   conditions are equalities (`Index Cond`), checks only the rows of that value (and, for the
   first-conjunct rule, those where the column is NULL, so the conjunction's evaluation order is
   as before), through a hash on the column kept with the shared rows; parameterised scans
   (`sys_rescan`) use the shared rows and hashes too. A scan in an index's order sorts only its
   candidates (it sorted the whole catalog).
5. **Quadratic catalog generation and lookups** (each was a scan of a map per row or per
   object): TOAST owners, composite types by relation, schemas by name, ACLs, tables' row
   types, user type names, and the row-level-security tables by name every statement's
   binding looked for (it scanned every table and view per statement) are hashed once per
   frozen catalog (`Catalog::lookups`: a catalog published in an `Arc` no longer changes; a
   copy DDL edits starts unfrozen and scans); the built-in tables read per row are hashed once
   per process (`type_procs`, `SysTable::by_oid` / `columns` / `is_view`, built-in operator
   classes and access methods, the catalogs' index list `sys_indexes.tsv`, which planning a
   catalog query parsed per column).
6. **DDL's whole-catalog passes made cheaper or linear**: ALTER TABLE's search for stored
   expressions naming the table (`follow_renames`) copied every table and index (now only
   those whose texts may name it, found without copying the texts); foreign keys' internal
   triggers looked up every constraint by scanning all tables (`ri_triggers`), and every DDL
   statement made all of them to learn which tables have triggers (`triggered_tables` now
   lists the tables only, checked against the full list in debug builds); `depends` scanned
   all tables per sequence; dropping cascaded objects searched the deleted list and all tables
   per object (`delete_object`), and the dependency walk (`findDependentObjects`) scanned every
   dependency and every target per visited object (now indexed by object): DROP SCHEMA ...
   CASCADE of 10 schemas over 5,000 tables 312 s → 3 s.
7. **Idle sessions** (`bin/roll-engine.rs`, `server::set_idle_hook`): transparent huge pages
   off for the server process (with the system's `always` mode every connection thread's
   first heap pages took a 2 MB page: 3.1 → 1.2 MB per idle session alone), and a connection
   idle for 2 s returns its thread's free mimalloc pages and the stack its statements touched
   to the system (`mi_collect`, `madvise(MADV_DONTNEED)` below the live frames,
   `stack::unused_range`). Embedders keep their own allocator and settings.

Also fixed on the way: `format_type(0, ...)` is `-` as in PostgreSQL (it was `???`).

### Upgrade implications

- Catalog format 1: a database whose catalog is one JSON value (every database before this
  round) is rewritten as header and records the first time this version opens it, in one
  commit (231 ms for 2,000 tables, once; `tests/catalog_storage.rs`
  `legacy_catalog_is_rewritten_as_records`). `Engine::upgrade_storage` and the index-key
  upgrade are unchanged and run after it.
- No downgrade: an older binary reading the new header fails with "corrupt catalog" (the
  header lacks the maps it requires) rather than seeing an empty catalog.
- Nothing else in the storage format changed.

### Where it is still slower (catalog, DDL, sessions), and why

- **Every DDL statement still does O(objects) work**, with small constants now: copying the
  maps it changes (`Arc::make_mut` of `tables` copies the map's nodes), the commit's diff over
  a changed map (pointer comparisons), and whole-catalog passes run per statement:
  `prepare_save` (TOAST tables, default privileges, ACL pruning, frozen xids, relhastriggers
  bookkeeping), `dodge_array_type_names`, constraint name choice (scans the schema's tables),
  ALTER TABLE's `follow_renames` (every stored text) and `column_using` (every column's type),
  and DROP's dependency graph (`depends` binds every stored expression of the catalog; DROP
  TABLE 77 ms at 10,000 tables). Linear per statement means quadratic for loading a schema of
  N tables; PostgreSQL's per-statement cost is O(log N) through its catalog indexes. The fix
  is persistent (structurally shared) maps for the catalog (O(log N) copies and edits) and
  indexes for what these passes look for (dependents by object, constraint names by schema,
  stored texts by the names they mention), so each pass touches only the objects involved.
- **A new catalog version regenerates a system catalog's rows whole** the first time a
  statement reads it (psql `\d` after DDL: 27 ms at 500 tables, PostgreSQL 0.9): rows are
  shared and hashed per version, not maintained incrementally. The fix: generate per object
  and reuse the rows of objects a DDL didn't change (pg_attribute's rows per relation keyed by
  its `Arc`, with the catalog-wide inputs, ACLs and types, compared between versions), or
  generate only the rows an equality asks for.
- **Catalog joins and counts copy rows**: the executor's rows are `Vec<Value>`s, and a nested
  loop concatenates whole rows (pg_class × pg_attribute: 60 columns per output row). Agent
  42's projection pushdown (round 18, above) decodes only the columns a plan reads from
  tables; a catalog scan's rows are generated whole still, and carrying only the needed
  columns out of a catalog scan would remove most of it.
- **information_schema views** run their SQL over the catalogs per statement (12–65 ms):
  their rows could be cached by catalog version like the base catalogs'.
- **Opening a database with a big catalog** loads every record (JSON per object): about 2x
  the old single value's parse (70 ms for 2,000 tables, 150 ms for 10,000), once per database
  open. A binary encoding, or loading objects lazily, would cut it.

## Catalog and DDL at scale (round 18, agent 49)

Measured on the same harness (PostgreSQL in `a49-pg`, the pinned image and settings; both
servers on cores 28–31, clients on 2–5) while other agents' builds and two regression runs
loaded the machine (load 25–35): read differences under ~30% as noise. **pg** PostgreSQL 18.6
(3 runs), **before** the engine at `52f786e` (this round's base after its last rebase; 2 runs),
**after** this round's engine (2 runs), base and change interleaved. New workload `scale-ops`
(`tools/bench/bench.rs`): over the catalog `scale --keep 1` leaves (10,000 tables in 20
schemas, a table of 1,000 partitions), 100 new tables are created, altered, indexed,
commented and dropped, and the catalog queries run 5 times each (their figure is the mean,
the first run included); every engine run starts from a copy of the same data directory.

### 10,000 tables (ms per statement)

| case | pg | before | after | after / pg |
|---|---|---|---|---|
| CREATE TABLE (key, NOT NULL, default) | 3.17 | 14.7 | 1.30 | 2.4x |
| ALTER TABLE ADD COLUMN | 0.36 | 16.1 | 2.04 | 0.18x |
| CREATE INDEX | 0.89 | 14.3 | 1.00 | 0.89x |
| COMMENT ON TABLE | 0.26 | 11.0 | 0.22 | 1.2x |
| DROP TABLE | 4.90 | 92.9 | 19.5 | 0.25x |
| psql `\d t` columns (5 runs, the first after the setup's DDL) | 0.88 | 52.8 | 11.0 | 0.08x |
| psql `\d t` columns, after DDL | 0.47 | 249 | 38.6 | 0.01x |
| psql `\d t` indexes, after DDL | 0.95 | 123 | 40.2 | 0.02x |
| psql `\d t` lookup, after DDL | 0.47 | 111 | 106 | 0.004x |
| information_schema.columns of a table, after DDL | 4.51 | 146 | 145 | 0.03x |
| `pg_class` × `pg_attribute` join (5 runs) | 37.3 | 10.8 | 43.5 | 0.86x |

(Steady state, the join is 7.6–7.9 ms for both builds and the ORM constraints query 23 ms
for both; their 5-run means above are dominated by the first run, which generates the
catalog.)

### Restoring a large schema (`pg_dump -s` of that catalog: 230,840 lines, 11,001 CREATE TABLE, 22,001 ALTER TABLE, `psql -f` into an empty database)

| | pg | before | after |
|---|---|---|---|
| seconds (errors) | 42.4 (0) | 254 (0) | 21.0 (0) |

The engine before this round restored it 6x slower than PostgreSQL (every statement's cost
grew with the catalog); it is now 2x faster than PostgreSQL.

The regression database's dump (`pg_dump --no-owner --no-privileges --no-tablespaces` of the
suite's database on the reference after a full run: 445,982 lines, about 1,500 objects and
their data) restores in 3.46 s on PostgreSQL and 7.3 s / 7.7 s on the engine before / after
(release builds; its catalog is small, so per-statement catalog cost isn't what dominates;
agent 37's hour-long restore was a debug build, and before 9e1e0a7 a CREATE INDEX over big
keys hung in the B-tree size model). The engine fails 8 of its statements that PostgreSQL
runs (identity sequences' `UNLOGGED` / `LOGGED` options, the `inet_minmax_ops` default for
cidr under BRIN, a rule's view, ...): the same 8 before and after.

### Optimisations

1. **Structurally shared catalog maps** (`catalog/pmap.rs`): `PMap<V>`, an ordered map from
   oids in chunks of 64 consecutive oids, each behind an `Arc`, under an `Arc`'d index of the
   chunks. A catalog copy shares everything; a change copies the chunk index and one chunk
   (it copied the whole `BTreeMap`); `diff` compares chunk pointers first, so the commit's
   record diff (`persist::diff_into`) and the statement passes below visit only the chunks
   that changed. `PHash` does the same for the relation-name index (256 buckets; every
   CREATE copied every name). Both serialise as the maps they replace (records, the header,
   `txn::catmerge`'s JSON are unchanged). Used for `tables`, `indexes`, `views`, `sequences`,
   `functions`, `relstats`, `frozen_xids`, `class_order` and `names`.
2. **Secondary indexes kept on mutation** (`pmap::IndexedMap`, `catalog/relindex.rs`): the
   tables, views, indexes and sequences maps keep, as entries come and go, the tables
   internal triggers go on (foreign keys' ends, deferrable keys), constraint names per schema,
   constraint and default owners, the tables referencing each table by foreign keys, TOAST
   owners, row types, the tables holding each composite type in a column, row-level-security
   tables and security-barrier views by name, partitioned indexes' attached indexes, tables
   with expression indexes and tables' owned sequences. What every DDL statement (and every
   first statement over a new catalog version) found by scanning every relation is now a
   lookup: `triggered_tables`, constraint name choice, `column_using` (find_composite_type_
   dependencies), `toast_owner`, `rel_rowtype`, `user_type_name`, `rls_named`, a dropped
   table's owned sequences and its cascaded deletions' owners. In debug builds each lookup is
   checked against the scan it replaces.
3. **Per-statement passes over what the statement changed**: `prepare_save` (qualifying new
   expressions, TOAST tables, frozen xids, relhastriggers bookkeeping, ACL pruning) and
   `dodge_array_type_names` iterate `PMap::changes` between the statement's catalog and the
   one it started from; the array-name pass runs over every type only when a type the
   statement made or renamed could clash. relhastriggers after a statement now remembers only
   the tables that lose their last trigger in it (`trigger::lost_triggers`): tables that still
   have triggers showed it set anyway (a debug check compares with keeping every table that
   had triggers). ALTER TABLE's `follow_renames` skips stored expressions that can't hold FROM
   items when only columns were added or dropped (their column aliases are all it edits).
4. **DROP TABLE's dependencies** (`depend::drop_scope`, `depends_in`, `closed`): dropping
   plain tables derives the dependencies of every object but the tables that can't depend on
   them (only the roots, the tables referencing them by foreign keys, inheriting from them,
   holding their row types, or whose stored expressions may name them, their sequences or
   indexes are derived); when the walk finds only the roots' own objects (and foreign keys
   referencing them) that is the answer, otherwise the whole catalog's dependencies are
   derived as before. Debug builds compare every scoped walk with the full one.
5. **One relation's catalog rows** (`system::rows_of_rel`, `exec/nodes.rs` `sys_rel_rows`):
   a scan of `pg_class`, `pg_attribute` or `pg_attrdef` whose index conditions or first
   conjunct pin the relation column (`attrelid = 'x'::regclass`, a nested loop's `adrelid =
   a.attrelid`) generates that relation's rows alone when the shared rows of the catalog
   version aren't generated yet (a rescan does so for its first 16 outer rows), instead of
   the whole catalog; the rows are the full generation's, in its order (checked in debug
   builds). The planner's row count of those catalogs (`explain::sys_tuples`) counts without
   generating rows (`system::row_count`).
6. **Catalog records parsed in parallel** on a database's first open (`persist::parse_all`).
   It made no measurable difference to the first connection (below): parsing isn't what
   that time is.

### First connection to a database (in-process, 10,000 tables)

| | before (`242734d`) | after |
|---|---|---|
| first connection (catalog loaded) | 125–143 ms | 136–139 ms |
| its first `count(*)` of pg_class | 65–71 ms | 70–78 ms |

### Upgrade implications

None: the stored catalog (format 1, a header and a record per object), the WAL and the
storage format are unchanged; `PMap` / `IndexedMap` serialise as the `BTreeMap`s they
replace, and their indexes are rebuilt as records load.

### Where it is still slower (catalog, DDL), and why

- **DROP TABLE (4x PostgreSQL at 10,000 tables)**: the scoped walk still derives every
  object's dependencies but the unrelated tables' (views, functions, types, sequences, all
  binding their stored texts) and scans every table's stored expressions for the dropped
  names; partitioned tables and anything a view or function depends on take the full
  derivation. The fix is a dependency index kept like `relindex` (pg_depend by referenced
  object), with stored texts' references recorded when they are stored.
- **ALTER TABLE ADD COLUMN (0.18x)**: binding the new column's default and the table's
  checks over the statement's catalog, `follow_renames`' scan of view texts naming the table,
  and the per-statement catalog copies (now cheap) remain; not profiled further this round.
- **Catalog queries right after DDL**: psql's `\d t` lookup matches `relname ~ '^(t)$'`,
  which PostgreSQL turns into an index range on `pg_class_relname_nsp_index`; the engine
  generates pg_class whole for the new version (100 ms) and evaluates the regex per row (28
  ms even when generated). `\d` columns / indexes after DDL (39–40 ms) spend it in
  `row_count` (pg_class / pg_attribute counted per relation, indexes' columns through
  `rel_columns`) and in pg_index / pg_constraint, which aren't generated per relation yet.
  The fix: per-relation counts kept in `relindex`, `rows_of_rel` for pg_index, pg_constraint
  and pg_type, and regex prefixes as name ranges over the name index.
- **information_schema views** (145 ms for one table's columns at 10,000 tables) still run
  over the whole catalog per query; see HANDOFF ("information_schema") for the assessment of
  binding their real definitions.
- **The first connection to a database** (136 ms at 10,000 tables) is unchanged: not the
  records' JSON parsing (parallel now, no difference). Not profiled yet.

### Verification (round 18, agent 49)

- `cargo test -p pg-engine` (all 18 test binaries) and clippy (no new warnings).
- The regression suite in full with a private reference cluster per run, base `85ec54f`
  against the change rebased onto it: **51,141 / 51,150 both, identical per file, per unit
  and in every differing unit's hunk** (on earlier bases, `52f786e`: 51,140 both; `242734d` /
  `75986d8`: 51,138 both; identical each time).
- `difftest smoke` passes (before and after the last rebase).
- New tests: `catalog::pmap` unit tests (random edits against a `BTreeMap` model: contents,
  order, serde form, `diff` against earlier copies; `PHash` against a `HashMap`), golden
  `a49_catalog_indexes` (recorded from PostgreSQL: relhastriggers across dropped keys and
  VACUUM, constraint names chosen in a schema, row-type users, one relation's catalog rows
  after DDL inside transactions and savepoints, DROP TABLE with and without dependents
  outside the table), and the debug cross-checks named above, which every golden and test
  statement exercises.

## Catalog work kept to what a statement touches (round 18, agent 58)

Same harness and catalog as agent 49's (`scale --keep 1`: 10,000 tables in 20 schemas and a
table of 1,000 partitions; `scale-ops`, 100 new tables, catalog queries 5 times each, the
first after the setup's DDL included). PostgreSQL in `a58-pg` (pinned image and settings),
both servers on cores 20–23, the client on 24–27, load 20–25 from other agents' builds and
regression runs. **pg** 3 runs, **before** agent 49's engine (`8ef2f59`) 3 runs, **after**
this round's (rebased on `0a8ec4e`) 3 runs, interleaved; medians.

| case | pg | before | after | after / pg |
|---|---|---|---|---|
| CREATE TABLE (key, NOT NULL, default) | 2.87 | 3.16 | 1.47 | 1.95x |
| ALTER TABLE ADD COLUMN | 0.38 | 3.17 | 0.62 | 0.61x |
| CREATE INDEX | 0.82 | 1.88 | 0.69 | 1.18x |
| COMMENT ON TABLE | 0.24 | 0.86 | 0.20 | 1.22x |
| DROP TABLE | 4.88 | 20.28 | 1.72 | 2.84x |
| psql `\d t` lookup (5 runs) | 1.22 | 24.7 | 3.75 | 0.33x |
| psql `\d t` columns (5 runs) | 0.77 | 11.9 | 2.45 | 0.31x |
| psql `\d t` indexes (5 runs) | 0.84 | 9.39 | 1.53 | 0.55x |
| psql `\d t` columns, after DDL | 0.45 | 38.6 | 1.34 | 0.34x |
| psql `\d t` lookup, after DDL | 0.44 | 100 | 10.5 | 0.04x |
| psql `\d t` indexes, after DDL | 0.57 | 41.7 | 1.62 | 0.35x |
| information_schema.columns of a table | 4.39 | 81.3 | 0.92 | 4.8x |
| information_schema.columns of a table, after DDL | 3.63 | 128 | 1.56 | 2.3x |
| information_schema.tables count of a schema | 5.47 | 7.15 | 2.42 | 2.3x |
| `count(*)` of pg_class (5 runs) | 7.31 | 18.0 | 34.5 | 0.21x |
| `pg_class` × `pg_attribute` join (5 runs) | 8.15 | 43.9 | 46.9 | 0.17x |
| ORM constraints query (5 runs) | 32.4 | 208 | 219 | 0.15x |

Notes. The `\d` lookups' 5-run figures include a new connection's first statements
(per-thread caches: 5–30 ms once); over 40 runs in one connection the lookup is 1.2 ms and
1.4–1.6 ms after DDL. `count(*)` of pg_class and the join generate pg_class whole the first
time a catalog version is read: before, the `\d` lookup had already done so in the same
sequence, so its cost showed there instead; steady state is as before (agent 49's 7.6 ms
for the join). The ORM query reads a whole schema's constraints right after other queries
generated nothing it uses: the catalogs it joins are generated whole.

First connection to a database (in-process, 10,000 tables, 3 runs each): before 134–178 ms,
after 134–161 ms: unchanged within the noise. Profiled (timers around `Database::open`):
reading the records from storage 25–35 ms, parsing 11,001 table records (8 threads) 27–60
ms, inserting them into the maps with their indexes 26 ms (37–50 ms before this round's
in-place index updates, below), index records 9–13 ms, `reindex` 10 ms.

### Optimisations

1. **DROP's dependents by referenced object** (`depend::scoped_dependents`): what
   `findDependentObjects` reads through pg_depend's index on the referenced object, the
   engine derives for a `depend::Scope` of objects only: the roots and, for every object the
   walk reaches, what may refer to it (`Wanted::add`: its indexes, triggers, rules,
   policies, statistics and publication memberships, partitions and inheritors, tables
   referencing it by foreign keys, owned sequences, attached partition indexes, the views
   reading it through FROM functions' composite results, the tables, views and functions
   holding its row type, and the objects whose stored texts name it or the functions taking
   or returning its row type), deriving again until the walk reaches nothing new. Any root
   and any cascade of tables, views, indexes, sequences, constraints, defaults, triggers,
   policies, rules and statistics stays scoped, partitioned tables and view dependents
   included (agent 49's scope covered plain tables without such dependents); reaching a
   function, schema, extension or a type other than a row type, or a walk error, derives the
   whole catalog as before. `derived_depends` takes the scope and iterates its members (in
   oid order, so rows keep the full derivation's order), internal triggers made from the
   scope's tables' constraints only (`trigger::internal_triggers_of`). Debug builds compare
   every scoped walk with the full one (as agent 49's did).
2. **Word indexes of stored texts** (`relindex::words_in`, `TableIx` / `IndexIx` /
   `ViewIx::words`): every identifier word (ASCII-lowercased, not starting with a digit) of
   tables' defaults, generation expressions, checks and partition keys, indexes'
   expressions and predicates, and views' queries, to the objects: `ddl::mentions` of a name
   becomes a lookup (a unit test checks the two agree). Used by the scoped DROP and by
   ALTER's `follow_renames` (views, tables and indexes whose texts may name what changed).
3. **More relation map indexes** (`relindex`): views' column row types and FROM-function
   relations, views' `_RETURN` rules, partitioned indexes' attached indexes, foreign keys'
   `ri_check`s; pg_class / pg_attribute / pg_attrdef / pg_index / information_schema row
   counts per map (`RelRows`), so the planner's estimate of those catalogs (`row_count`,
   `explain::sys_tuples`, now also for information_schema.columns and .tables) never
   generates rows.
4. **In-place index updates**: a changed table updates only the parts of its index entries
   that differ (`TableParts`, `MapIndex::replace`: an added column leaves its constraints'
   entries alone), and sets are changed in place (`PHash::get_mut`) instead of copied out,
   changed and put back (which copied their shared chunks each time).
5. **Lookups that scanned the catalog**: a constraint's or default's description through
   the owner indexes, a view rule's through the rules index, an internal trigger by oid from
   the tables that can own it (`trigger::ri_trigger`), a table's foreign key triggers from
   the table and its referencers (`ri_triggers(Some(t))`, which every DML binding of a table
   ran over all tables), the dependency walk's sort keys from the scope; DDL's
   `lock_references` over the maps' changes (`PMap::changes`) instead of every function and
   table.
6. **pg_class through its name index** (`exec::nodes::class_name_bounds`,
   `system::class_rows_named`): an index scan on `pg_class_relname_nsp_index` (psql's `\d
   name`: `relname ~ '^(name)$'`, whose fixed prefix the planner already made the index
   condition, `relname = 'name'` or a `>=` / `<` range for a pattern) generates only the
   relations of those names (found through the catalog's name index, up to 64) when the
   catalog version's rows aren't generated, and picks them by name from the shared rows when
   they are (it evaluated the regex on all 11,000). EXPLAIN is unchanged.
7. **One relation's rows of pg_index** (`rows_of_rel` by `indrelid`), as agent 49's for
   pg_class / pg_attribute / pg_attrdef.
8. **information_schema by relation** (`system::is_rows_named`, `exec::nodes::is_named_rows`):
   a scan of information_schema.columns or .tables whose filter starts with `table_schema =`
   / `table_name =` constants generates the rows of those relations alone (as PostgreSQL's
   view reads pg_class through its name index), in the full generation's order (checked in
   debug builds).

### information_schema: the real view definitions

Not switched. The virtual tables now serve the per-table queries clients send (columns,
tables) in about 1 ms at 10,000 tables, below PostgreSQL's 3.6–4.4 ms, which was the speed
argument for binding the real definitions. What remains is the plan shape: binding
PostgreSQL's definitions would make every information_schema query a 6–10-way join planned
over catalog estimates that differ from the reference's (its come from autovacuum's
relpages / reltuples), touching the 22 regression files that read information_schema to fix
one EXPLAIN unit (join 368). Trialling it needs PostgreSQL's `information_schema.sql` bound as
views (the `_pg_*` functions and the domains are there; the views aren't), behind a switch,
with the 22 files compared unit by unit: a round of its own. Agent 49's assessment in
HANDOFF stands, with its prerequisite (per-relation catalog rows) now covering pg_index too.

### Upgrade implications

None: the stored catalog, the WAL and the storage format are unchanged; the new indexes and
counts are built as records load, like agent 49's.

### Where it is still slower, and why

- **Catalog queries that read whole catalogs after DDL** (`count(*)` of pg_class, joins over
  a schema, ORM constraint queries): a new catalog version generates each catalog's rows
  whole the first time a statement needs more than one relation's (100 ms for pg_class at
  10,000 tables). PostgreSQL reads its heap. The fix is reusing the rows of objects a DDL
  didn't change (per-object rows keyed by the object's `Arc`) or generating catalogs a
  schema at a time; pg_constraint and pg_type have no per-relation generation yet.
- **ALTER TABLE ADD COLUMN (0.6x)**: what is left is per-statement work around the change
  (binding, the commit's record diff and header, the catalog copies' drops), no longer
  O(catalog) passes.
- **`\d` steady state (0.3–0.55x)**: a new connection's first statements (thread-local
  caches) and the joins' per-row work; over many runs in one connection the lookup is
  1.2 ms against PostgreSQL's 0.4–1.2 ms.
- **First connection to a database (about 135–160 ms at 10,000 tables)**: every object's
  record is read, parsed and indexed when the database opens. Near-constant needs loading
  objects lazily (the catalog as an on-demand view of the records) or a binary snapshot of
  the catalog; neither is done.
- The regression database's dump still fails the same 8 statements on restore (agent 54's).

### Verification (round 18, agent 58)

See HANDOFF "Where it stands". `cargo test -p pg-engine` (every test binary) and clippy (no
new warnings); `difftest smoke` passes. The regression suite in full, each run with a
private reference, base `0a8ec4e` against this change rebased onto it: **51,148 / 51,150
both, identical per file and in every differing unit's hunk**. New tests: `relindex::tests::words_match_mentions`, golden
`a58_catalog_scoped` (recorded from PostgreSQL: DROP refused and cascaded over views on
views, functions returning a row type, views over them, SQL-standard bodies, domains over
row types, partitions with keys, indexes, triggers, policies, rules and statistics, a view
relying on a key, a view's rule, sequences in defaults, inside transactions and savepoints;
pg_class name lookups and pattern ranges after DDL and across savepoints; pg_index of one
table; information_schema.columns / .tables of one relation, mixed case, conflicting
equalities, system catalogs, after DDL in a rolled-back transaction), and the debug
cross-checks: scoped walk against full, name lookups and per-relation rows against full
generation, row counts against generated rows, internal triggers by oid against the full
list, view rules index against a scan.

## Round 18 (agent 43): storage, bulk paths and memory

"Where it is still slower" items 2, 3 and 7 (bulk work, index scans over many rows, the first
`count(*)` of a big table). Same harness and settings; servers on cores 20–23, clients on
24–27 (container `a43-pg`), on an even busier machine (load 25–40, other agents' builds and
regression runs unpinned on the same cores): over the wire, two runs of the same binary
differ by up to 2–3x, so the in-process numbers (no sockets) are the ones to compare for
OLTP. **pg** is PostgreSQL 18.6; **before** the engine without this round's storage work
(`44adff0` for OLTP and the TPC-H load; `7896c328` for the bulk rows, measured back to back
with **after**); **after** this round. Bulk times are medians of 3 from a fresh
data directory; OLTP is the median of 2–3 interleaved runs, 10–20 s each.

### Bulk, 1M rows (seconds; peak memory of the whole run)

| step | pg | before | after | after / pg |
|---|---|---|---|---|
| COPY in | 1.21 | 6.96 | 7.37 | 0.16x |
| CREATE INDEX | 0.55 | 4.25 | 4.03 | 0.14x |
| CREATE UNIQUE INDEX | 0.32 | 3.58 | 2.67 | 0.12x |
| COPY out | 0.55 | 1.85 | 1.94 | 0.28x |
| INSERT … SELECT | 1.04 | 3.67 | 2.97 | 0.35x |
| UPDATE half | 6.93 | 4.47 | 3.88 | 1.79x |
| DELETE a quarter | 0.33 | 3.36 | 2.81 | 0.12x |
| VACUUM | 0.31 | 2.16 | 0.82 | 0.38x |
| ANALYZE | 0.16 | 1.85 | 1.52 | 0.11x |
| count(*) after | 0.05 | 0.14 | 0.17 | 0.29x |
| peak memory | 331 MB | 2,725 MB | 2,297 MB | |

(Earlier in the round, against `44adff0` on a busier machine, this batch also had COPY FROM
reading its lines as the insert took them, 14.2 → 8.0 s and a 1.8 GB peak; that feed was
dropped when `0a8ec4ee` made COPY FROM parse its data as it arrives, for the client suites:
see item 6.)

### TPC-H SF1 load (COPY in 100k-row chunks, keys and indexes, VACUUM ANALYZE)

| | before | after |
|---|---|---|
| peak memory | > 14 GB (stopped by the run's 14 GB cap in the index phase; 18.1 GB in round 17) | 4.8 GB |
| COPY / keys and indexes / VACUUM ANALYZE (s) | 202 / — / — | 163 / 150 / 27 |
| first `count(*)` of lineitem after opening (in-process) | 37–51 s (round 17) | 6.6–18 s (5.7 s the second time) |
| on disk | 666 MB | 666 MB |

PostgreSQL's peak for the same load was 1.2 GB (round 17).

### OLTP, in-process (TPS, 1M-row table; median of 2 interleaved runs)

| case | clients | before | after | after / before |
|---|---|---|---|---|
| point select (prepared) | 1 | 46,580 | 58,440 | 1.25x |
| range scan, 100 rows | 1 | 1,890 | 2,210 | 1.17x |
| secondary-index lookup | 1 | 6,505 | 9,325 | 1.43x |
| point select (prepared) | 8 | 214,150 | 224,160 | 1.05x |
| range scan, 100 rows | 8 | 10,300 | 12,925 | 1.25x |
| secondary-index lookup | 8 | 45,470 | 60,100 | 1.32x |

Over the wire the same cases came out within the noise (8-client secondary lookups: 7,097
before, 8,048 after, medians of 3; range scans 2,627 / 2,658), the rest of the OLTP mix too
(inserts and updates at 8 clients ran from 4,181 to 16,527 TPS in two runs of one binary).

### What changed

1. **SST blocks decoded compactly** (`storage/sst.rs` `Block`): a block was a `Vec` of
   (key `Vec`, value `Bytes`) pairs, two allocations an entry and about three times the bytes
   the block cache counted; it is now the decompressed bytes, every full key laid end to end
   and a 12-byte offset entry per key (three allocations a block), and the cache counts what
   it holds. The merging cursors borrow the entry at their head and copy only what a scan
   returns; compaction copies nothing.
2. **Batched heap fetches for index scans** (`exec/scan.rs` `next_indexed`, `fetch_rows`;
   `Txn::get_many_snap`; `Store::get_versions`): an index scan reads its entries a batch ahead
   (4 at first, doubling to 128, so a LIMIT reads little more than it needs) and fetches their
   rows together: one view of the store's sources, sorted keys, each SST block read once for
   all the keys in it. The rows are read at the statement's snapshot, so reading ahead reads
   what reading one at a time would; nothing a client can see happens on fetching (counts,
   pruning, locks and EXPLAIN ANALYZE's heap fetches stay where the rows are returned).
   Bitmap heap scans fetch the same way.
3. **Index-only scans from the index entries** (`index_only_columns`, `index_row`,
   `storage::keys::decode_index_value`): the engine's index entries are versions of their own,
   written in the commit that writes the row and deleted with it, so an entry visible to a
   snapshot points at a row version visible to it; that is the visibility map's job here,
   with every page all-visible. A btree index-only scan over plain columns whose key encoding
   gives the value back exactly (integers, bool, date, timestamps, uuid, text types; not
   numeric or floats, whose keys drop scale or the sign of zero) builds its rows from the
   entries without reading the table, for tables too big to model (whose equal keys have no
   modelled heap order either) that never stored a compressed or out-of-line value. EXPLAIN
   ANALYZE's "Heap Fetches" still comes from the heap model, as before. A `count(*)` of
   lineitem's 6M rows by index-only scan: 5.7 s, from 37–51 s.
4. **Big commits ingested as SSTs** (`Store::ingest_locked`): a commit of more than a quarter
   of a memtable (8 MB by default) is written as an L0 SST of its own (fsynced, then in the
   manifest, then visible: the same durability as a WAL record's fsync) instead of one WAL
   record built in memory and a memtable copy of every entry. The memtable is frozen first,
   so every earlier commit is in an older source; the SST waits among the frozen memtables
   until those older than it are flushed. After a crash, the WAL's commits are replayed in
   groups between the ingested commits' timestamps (`Store::open`), so L0 keeps one range of
   commit timestamps per file, in order (it is kept ordered by timestamp now, not by insertion).
5. **Partial compaction** (`Store::compact`): L0 → L1 rewrites only the L1 files whose user
   keys overlap L0's, not the whole database (a bulk load into a new table rewrites nothing
   else).
6. **COPY FROM (not landed).** A feed reading COPY FROM's lines as the insert took them (no
   record, expression or line copies of the whole input: about ten times the input in
   memory) was dropped when `0a8ec4ee` made COPY FROM parse records as the data arrives, so
   a bad line fails while the client is still sending (pgx). Its rows are still parsed whole
   into expressions before they go in: "Where it is still slower" item 2.
7. **CREATE INDEX, VACUUM, ANALYZE and ALTER TABLE's checks stream the table**
   (`Ddl::scan_rows`, `measure_rows`): they decoded the whole table into memory (several
   times per statement: VACUUM ANALYZE measured, sampled and counted it apart). Now one pass
   keeps only what each needs: heap tuple sizes when the table is too big to model, each
   index's (key, size) when its pages aren't modelled, row counts without decoding.
   ANALYZE's reservoir is filled as rows are read (the same picks from the same seed, so
   pg_stats is unchanged). A unique index build keeps a 64-bit hash of each key and the
   first row's data key (`UniqueKeys`: ~45 bytes a row, from ~170), re-reading the row on a
   hash match. CHECK, NOT NULL and foreign key validation, and duplicate detection, stream
   too.
8. **Only GIN and GiST indexes keep their build's row set** (`sizing::models_build`): every
   index build kept a set of all the table's row keys for the GIN pending-list model, and
   every VACUUM added another set of the keys it found new: for a big table, hundreds of MB
   per index, never freed.
9. **Compaction's reads stay out of the block cache** (`SstReader::block_with`,
   `Cursor::level_uncached`): every block a compaction read went into the cache, which a
   load's compactions filled with blocks nobody reads again (the TPC-H load's COPY phase
   peaked at 2.5 GB, now 1.9).
10. **An index build's entries and a commit's undo log** (`Txn::put_new_index_entry`,
    `Txn::commit`): a new index's entries go into the write buffer without an undo record
    when no savepoint is open (no earlier snapshot can read an index that didn't exist, and
    nothing can roll back to before it but the whole transaction); the undo log is freed
    before a commit copies the buffer. The TPC-H load's peak (lineitem's primary key) 5.3 →
    4.8 GB.
11. **Index sizes where the planner reads them** (`sizing::btree_build_blocks`): an index
    measured without its model (CREATE INDEX, ANALYZE) of a table small enough to model is
    sized by the model's own sorted build, not a rougher estimate (a 300k-row date index 329
    → 255 pages, PostgreSQL's 255: the planner reads relpages); `pg_relation_size` of an
    index on a table too big to model is its measured pages, not 1. Bigger tables keep the
    cheap estimate (the sorted build cost CREATE INDEX a third more over 1M rows). A heap
    laid out again when a database reopens still packs its rows afresh (2,250 pages where
    PostgreSQL keeps 2,521): putting them back at their stored TIDs changed where new rows
    go. Both are accepted internal differences (HANDOFF).
12. **A COPY past the model's size** stops modelling the table when its rows go in
    (`CopyLoad::flush` runs the size check single inserts run), so a bulk-loaded table's
    scans go parallel and its index-only scans read entries without a restart (agent 48's
    q06 finding; `big_tables` `copy_past_the_model`). The TPC-H load here (CREATE INDEX and
    VACUUM ANALYZE after the COPY, which already dropped the model) doesn't show it: q06 by
    parallel seq scan is 1.1–1.7 s right after the load both before and after. Seen on the
    way, for the planner's owners: q06's default plan on that load is a Parallel Bitmap Heap
    Scan over `lineitem_l_shipdate_idx`, 20–23 s with 3.5 s of planning, against 1.1–1.5 s
    with bitmap and index scans off.

### Durability, crash recovery and the storage format

- No new keys or value layouts: existing data opens unchanged, and `Engine::upgrade_storage`,
  `tests/storage_format.rs` and `storage_upgrade_batches.rs` are unaffected.
- New in the files: SSTs written by ingested commits (the same SST format, an L0 file holding
  one commit's timestamp) and L0 kept in timestamp order in the MANIFEST. A database closed
  cleanly opens in older binaries; after a crash, it must be opened once by this version
  (whose recovery replays the WAL around ingested commits and flushes everything) before an
  older binary opens it: an older binary would replay the unflushed commits on top of an
  ingested one, and it can't read the zero-filled WAL tails of round 17 either (an older
  binary's replay takes the zeros for a record). That is the documented downgrade rule now:
  **downgrade only after a clean shutdown, or after this version has recovered the database.**
- Tests: `storage::tests::ingested_commits_and_crash_recovery` (random small and big commits,
  flushes, compactions and crashes, the store dropped unclosed with background work off so
  it is left as a crash leaves it, including an ingested commit over older ones only in the
  WAL; every scan, reverse scan, point and batched read at the latest and every pinned
  snapshot against a model of every version), `compaction_leaves_other_files`,
  `storage::keys::tests::index_values_decode`, `tests/big_tables.rs` (index-only scans from
  entries against the same queries over the rows, with uncommitted writes of the session and
  of another, and a cursor's snapshot).

### Verification (round 18, agent 43)

- `cargo test -p pg-engine` (all of it) and `cargo clippy -p pg-engine --all-targets` (no new
  warnings), before and after the rebase.
- The regression suite in full with its own reference container (`a43-ref`): 51,130 / 51,150
  at round 17's end (`ca0c077`) and with this batch before the rebase, identical per file;
  after the rebases, the base and this round on it identical per file each time: 51,138
  (`44adff0`, `3461c02`, `281308c`), 51,140 (`9e1e0a7`, `bae6c68`), 51,141 (`85ec54f`),
  51,143 (`f3fc889`), 51,144 (`93f1876`, `6be2da44`), 51,148 (`7896c328`); with item 11, on
  `0a8ec4ee` (see the commit).
- `difftest smoke` passes before and after the rebase.

## Round 19 (agent 69): bounded memory for big transactions and COPY

Agent 43's open items: big transactions buffered their writes whole until commit, COPY FROM
parsed all of its data before the first row went in, and index builds inserted one entry at a
time. Same harness. The servers were on cores 4–7 and the clients on 12–15, on a loaded
machine (load 10–20, other agents' builds unpinned on every core). Bulk times are medians of
3 interleaved runs from a fresh data directory: **before** is `8d724fe7` (this round's
start) and **after** is this round on `fb4b987c`. PostgreSQL ran in the same window, 3
repetitions.

### Bulk, 1M rows (seconds; peak memory of the whole run)

| step | pg | before | after | after / pg |
|---|---|---|---|---|
| COPY in | 0.84 | 5.17 | 4.82 | 0.17x |
| CREATE INDEX | 0.37 | 3.84 | 2.52 | 0.15x |
| CREATE UNIQUE INDEX | 0.25 | 2.30 | 2.23 | 0.11x |
| COPY out | 0.42 | 1.63 | 1.61 | 0.26x |
| INSERT … SELECT | 0.72 | 2.90 | 2.99 | 0.24x |
| UPDATE half | 4.36 | 3.74 | 3.40 | 1.28x |
| DELETE a quarter | 0.21 | 2.68 | 2.35 | 0.09x |
| VACUUM | 0.24 | 0.79 | 0.82 | 0.29x |
| ANALYZE | 0.10 | 1.47 | 1.27 | 0.08x |
| count(*) after | 0.04 | 0.12 | 0.16 | 0.25x |
| peak memory | 343–363 MB | 1,754 MB | 879 MB | |

COPY in's own peak, by RSS sampled during the step: 1,488 MB before, about 200 MB after.
PostgreSQL's peak is a fresh container's (`anon + shmem`). One transaction of 5M rows
(`INSERT INTO big SELECT g, g % 1000, md5(g::text) FROM generate_series(1, 5000000) g`, a
primary key) peaks at 6.0 GB before and 2.6 GB after. Most of what is left is a lock per
inserted key (see "Where it is still slower" item 2).

### Restoring agent 54's scale case (`tests/pg_dump/scale_setup.sql`, `-v big=20000000`)

The case is 3,000 tables, 1,000 views and a 20M-row table with two indexes. It was built in
PostgreSQL, dumped once with `pg_dump -Fc` (598 MB), then restored serially with
`pg_restore` into an empty database on each server. Each engine had a fresh data directory
(`--cache-mb 2048`). All three restores give the same `count(*)` and `sum(a)` of the big
table, with no errors.

| | PostgreSQL | before | after |
|---|---|---|---|
| `pg_restore` (s) | 60 | 308 | 299 |
| peak memory | | 29.8 GB | 4.3 GB |

Agent 54 measured 453 s for the engine's restore on a busier machine. The time left is the
20M-row COPY and the two index builds (item 2 below). The memory left is mostly the 2 GB
block cache filled by those scans, plus the unique index build's key hashes
(`UniqueKeys`, about 45 bytes a row).

### TPC-H SF1 load (`bench tpch-load --sf 1`: COPY in 100k-row chunks, keys and indexes, VACUUM ANALYZE)

| | before | after |
|---|---|---|
| COPY / keys and indexes / VACUUM ANALYZE (s) | 146 / 155 / 32 | 103 / 144 / 20 |
| peak memory | 3.6 GB | 2.5 GB |

PostgreSQL's peak for the same load was 1.2 GB (round 17). Each run was one, on a fresh data
directory.

### What changed

1. **A transaction's writes spill to disk** (`txn/spill.rs`). Past `txn_spill_bytes`
   (64 MB) the write buffer and its undo journal go to a sorted run and the buffer starts
   empty. A run is a private, unsynced SST under `<db>/spill/<txn>/`, and every version in it
   is stamped with its journal position. Reads go through the buffer, then the runs (as of a
   statement snapshot's or cursor's mark, with the store's own version rules), then the
   store. `ROLLBACK TO` a savepoint taken before a spill rewrites the one run that straddles
   it; one taken after works on the buffer as before. Runs merge in tiers of 4, so a
   transaction of n spills keeps O(log n) of them.
2. **Spilled commits stream** (`Store::commit_sorted`). The buffer merged over the runs goes
   straight into the commit's ingested SST, and the snapshot-isolation conflict check runs
   per key as it is written. Ingested SSTs are written uncompressed (compaction compresses
   them when they move to L1), with the bloom filter sized up front instead of a hash per
   key held to the end.
3. **COPY FROM reads its data as the insert takes it** (`Session::copy_in_stream`,
   `CopyReader`). The rows go in a batch at a time, CopyFrom's buffer (1,000 rows or 64 kB),
   each batch parsed whole first; a NOT NULL violation still stops the reading at its row.
   The error context is `Frame::Copy`, formatted only when shown (it was a `format!` per row).
4. **Index builds write sorted runs** (`Txn::put_new_index_entries`). A build past half the
   spill threshold (32 MB of entries) sorts each batch and hands it to the transaction as a
   run instead of one `BTreeMap` insert per entry. Smaller builds are unchanged.
5. **Per-row costs on the write path:**
   - `changed_since_snapshot` answers without reading the store when nothing has committed
     since the snapshot (`Store::last_assigned_ts`). It was a bloom probe of every SST per
     row a DELETE or UPDATE locks.
   - The lock manager's lock and row maps use the crate's fast hasher, and a row lock no
     longer clones its holder list.
   - `track_counts` and `session_replication_role` are derived settings, not a String-keyed
     lookup per row written.
   - Bloom filter bits are computed by multiplication (`sst::FastMod`, the same bits as `%`:
     `fast_mod_is_rem`).
   - ANALYZE decodes only the rows its sample keeps (the same picks, the same statistics).

### Durability, crash recovery and the storage format

- No storage format change: runs never enter the MANIFEST, and `Store::open` deletes the
  `spill/` directory, which only a crash can leave behind. A spilled transaction commits as
  one ingested SST, as agent 43's big commits do.
- `tests/crash_bulk.rs`: a child process (the test binary run again) commits batches of
  COPY plus an UPDATE of the same rows, each spilling (a 256 kB threshold), and is killed
  with SIGKILL at three moments. Each reopen holds every batch whose COMMIT returned and all
  or none of the one cut short, and index scans count what the table holds. No spilled
  files are left.
- `tests/txn_spill.rs`: one script with a 2 kB buffer (hundreds of spills) and with the
  default one gives the same results. It covers the transaction's own writes, an index
  built after a spill, a cursor opened before an UPDATE, savepoints rolled back across spills,
  unique violations of spilled keys, reverse scans, REPEATABLE READ, rollback, and a reopen.
- The whole crate suite passes with `ROLL_TXN_SPILL_BYTES=4096`, where nearly every
  transaction with more than a few rows spills: goldens in both modes, wire, storage format
  and upgrade, catalog storage, big tables, parallel scans.

### Verification (round 19, agent 69)

- `cargo test -p pg-engine` in full, at the default threshold and with
  `ROLL_TXN_SPILL_BYTES=4096`; `cargo clippy -p pg-engine --all-targets` adds no warnings.
- The regression suite in full (debug, a private reference): 51,148 / 51,150, with no unit
  newly differing or newly matching against the committed `results.tsv`.
- `difftest smoke` passes. The isolation suite in full matches STATUS.md's current score:
  109 / 122 specs, 1,616 / 1,648 permutations, 12,154 / 12,238 steps.
## pgvector (round 20, agent 70)

The distance kernels (`src/ext/vector/dist.rs`) must return pgvector's bits: the reference build
(the official image's `OPTFLAGS=""`) sums in GCC's 4-lane SSE2 order, so the engine keeps that
order (four `f32` accumulators, the same tail and the same horizontal reduction), which LLVM
compiles to the same `subps` / `mulps` / `addps` chain. Wider vectors or several independent
chains would be faster and would change the last bits of distances (and with them the order of
near ties), so they aren't used; halfvec uses the reference's own F16C / FMA 8-lane kernel on
CPUs that have it, as pgvector does. Release build, 1,536 dimensions, one pair: vector l2² 493
ns, inner product 386 ns, cosine 629 ns, halfvec (F16C) about 250 ns: the latency of one
4-lane dependency chain, which is also the reference's. Build, insert and query speed of the
indexes against the reference: the `vector` workload of `tools/bench` (`bench.rs`
`vector_bench`).

### Index build, insert and query (round 20, agent 70)

100,000 rows of `vector(128)` (deterministic values in [-1, 1)), 200 nearest-neighbour queries
`ORDER BY embedding <-> q LIMIT 10` over the simple protocol, one client; hnsw m 16,
ef_construction 64, ef_search 40; ivfflat lists 100, probes 10. The reference is PostgreSQL
18.6 with pgvector 0.8.7 built with `-DIVFFLAT_BENCH -DHNSW_MEMORY` and the engine (release)
runs with `ROLL_VECTOR_BUILD_SEED=42`, so both build the same indexes: every query case's
result hash and the full-scan sums are equal on both sides. Each server pinned to the same 4
CPUs in turn (client on 4 others), default settings (maintenance_work_mem 64 MB), a shared
machine at load about 10.

| case | PostgreSQL + pgvector | engine | engine speed |
|---|---|---|---|
| COPY 100k rows | 3.90 s | 2.53 s | 1.5x |
| CREATE INDEX hnsw | 185.9 s | 107.5 s | 1.7x |
| hnsw queries (qps, p50) | 754 qps, 1.28 ms | 652 qps, 1.38 ms | 0.86x |
| COPY 10k more rows into the hnsw index | 34.5 s | 17.6 s | 2.0x |
| hnsw queries after the inserts | 591 qps, 1.61 ms | 845 qps, 1.15 ms | 1.4x |
| CREATE INDEX ivfflat | 1.49 s | 1.43 s | 1.0x |
| ivfflat queries (qps, p50) | 227 qps, 4.34 ms | 147 qps, 6.70 ms | 0.65x |
| exact k-NN (seq scan + top-N sort) | 26 ms | 103 ms | 0.25x |
| `sum(embedding <-> q)` over all rows | 30 ms | 75 ms | 0.40x |

hnsw builds and inserts are faster: contiguous vector storage, a visited set without
allocation per step, pages kept as records rather than buffers. Queries are about even (the
graph walk does the same distance computations; the engine's statement overhead is a larger
share at 1 ms). Still slower: ivfflat queries (every candidate of the probed lists is copied
into the emulated tuplesort, which has to sort all of them to keep PostgreSQL's quicksort tie
order; not profiled yet), and anything that reads every row (the row-at-a-time executor
decoding 128-float values: "Where it is still slower").

Recall@10 (the engine's, equal to the reference's since the results are): ivfflat over 20k
rows of 32 dimensions, lists 100: probes 1 / 2 / 5 / 10 / 20: 0.528 / 0.794 / 0.944 / 0.966 /
0.980; hnsw over 20k Gaussian rows of 32 dimensions: ef_search 10 / 20 / 40 / 100 / 200:
0.762 / 0.900 / 0.974 / 0.998 / 1.000.

## pgvector round 2 (round 21, agent 89)

What changed (HANDOFF "pgvector", speed): sequential scans of in-order heaps streamed and, at
8 MB or more, read by the parallel scan's workers, which compute pgvector's distances
(filters, float sums, a projection's distances over the stored bytes, a bounded sort's
per-chunk top-k); a streaming bounded sort; four-row distance kernels (independent sums,
each in the reference's order) in ivfflat scans and builds and in hnsw searches; ivfflat
scans over shared pages; index scans that copy no vector they don't check; filters decoded
before the vectors they don't read. Every result is still pgvector's bit for bit: each
query case's result hash below is equal on both sides.

Method: `tools/bench` `vector` workload (`--type`, `--dim`, `--rows`, `--parts`), the
reference PostgreSQL 18.6 + pgvector 0.8.7 built with `-DIVFFLAT_BENCH -DHNSW_MEMORY` (seeded
builds, as `ROLL_VECTOR_BUILD_SEED=42` makes the engine's), each server on the same 4 CPUs
(client on 4 others), `shared_buffers=2GB` and the engine's `--cache-mb 2048`, default
maintenance_work_mem (64 MB) and work_mem. Engine and reference runs interleaved, the
engine restarted for each and every row read once before the cases (both caches warm).
Medians of 3 rounds for the 128-dimension table, 1 round for the others; a shared machine
(load 15-40), so single-round numbers move by about 15%. Queries: 200 per case, ivfflat
lists 100 (50 at 1,536 dimensions: 100 needs more than maintenance_work_mem on both sides),
probes 10; hnsw m 16, ef_construction 64, ef_search 40; filtered queries `WHERE cat = i %
50` (2%) with iterative scans; scans 5 queries each. "x": the engine's speed against the
reference (above 1: faster). MB: peak memory above the warm server's while the case runs
(the engine's RSS; the reference container's anon + shmem). Index sizes are
`pg_relation_size`, equal on both sides (the engine models pgvector's pages byte for byte).

### vector(128), 100,000 rows

| case | PostgreSQL + pgvector | engine | engine speed | peak MB (pg / engine) |
|---|---|---|---|---|
| CREATE INDEX hnsw | 222.2 s | 134.2 s | 1.66x | 64 / 130 |
| hnsw size | 79.3 MB | 79.3 MB | | |
| hnsw queries (qps, p50) | 378 qps, 2.38 ms | 538 qps, 1.69 ms | 1.42x | 0.2 / 5.5 |
| hnsw, filtered, strict order | 35.8 qps, 28.2 ms | 65.9 qps, 15.1 ms | 1.84x | 4.1 / 39.9 |
| hnsw, filtered, relaxed order | 53.0 qps, 18.6 ms | 84.7 qps, 11.4 ms | 1.60x | 3.1 / 0.0 |
| COPY 10k more rows into hnsw | 51.1 s | 24.3 s | 2.10x | |
| hnsw queries after the inserts | 433 qps, 2.25 ms | 627 qps, 1.36 ms | 1.45x | 0.0 / 0.0 |
| CREATE INDEX ivfflat | 1.52 s | 1.95 s | 0.78x | 57 / 31 |
| ivfflat size | 52.5 MB | 52.5 MB | | |
| ivfflat queries (qps, p50) | 160 qps, 5.81 ms | 150 qps, 6.67 ms | 0.94x | 0.8 / 1.8 |
| ivfflat, filtered (plans as a scan + top-N sort) | 150 qps, 6.22 ms | 116 qps, 8.50 ms | 0.77x | 0.6 / 39.3 |
| exact k-NN (seq scan + top-N sort) | 34.2 ms | 43.3 ms | 0.79x | 0.3 / 0.5 |
| exact k-NN, filtered | 19.4 ms | 36.7 ms | 0.53x | 0.0 / 0.1 |
| `sum(embedding <-> q)` | 30.3 ms | 53.7 ms | 0.56x | 0.0 / 0.2 |
| `sum(embedding <=> q)` | 36.5 ms | 36.3 ms | 1.01x | 0.0 / 0.1 |
| `sum(embedding <#> q)` | 31.1 ms | 29.6 ms | 1.05x | 0.0 / 0.1 |

Medians of 3 rounds (hnsw: 1). The full-scan rows moved most between rounds on this machine
(the round before this batch: exact k-NN 0.81x, filtered 0.89x, l2 sum 0.94x, the other sums
0.93-0.96x; with the engine's scans run in parallel on 4 CPUs, its numbers follow the
machine's load more than PostgreSQL's serial sums do). Against round 20's engine on the same
data (interleaved, round 21's first batch): ivfflat queries 92 -> 144 qps, exact k-NN 127 ->
61 ms, the sums 116-123 -> 29-31 ms, hnsw queries 1.86 -> 1.39 ms p50.

### Other types and sizes (1 round each)

| case | halfvec(768) 30k | vector(768) 30k | vector(1536) 20k | bit(1024) 50k | sparsevec(30000), 64 nnz, 50k |
|---|---|---|---|---|---|
| CREATE INDEX hnsw | 1.09x (39 s; 112 / 64 MB) | 1.25x (112 s; 105 / 64 MB) | 1.41x (135 s; 83 / 64 MB) | 3.35x (18 s; 62 / 41 MB) | 1.00x (49 s; 131 / 58 MB) |
| hnsw queries | 0.95x (2.36 / 2.71 ms p50) | 1.24x | 1.06x | 1.04x | 0.99x |
| hnsw, filtered, strict / relaxed | 1.60x / 1.46x | 0.57x / 0.70x | 0.58x / 0.70x | 1.63x / 1.66x | 0.91x / 0.89x |
| COPY 10% more into hnsw | 1.39x | 1.40x | 1.35x | 3.76x | 0.92x |
| hnsw queries after the inserts | 1.06x | 1.46x | 1.15x | 1.24x | 1.27x |
| CREATE INDEX ivfflat | 1.09x (14 / 46 MB) | 1.73x (31 / 77 MB) | 1.55x (25 / 66 MB) | 1.37x (20 / 9 MB) | (no ivfflat) |
| ivfflat queries | 1.22x | 1.20x | 1.15x | 1.33x | |
| exact k-NN (seq scan + top-N) | 1.88x | 9.6x | 7.0x | 1.22x | 0.75x |
| exact k-NN, filtered | 0.71x | 0.58x | 0.63x | | 0.53x |
| `sum(distance)` over every row | 1.28x / 1.13x / 1.42x | 8.5x / 7.1x / 7.0x | 8.4x / 8.0x / 9.5x | 0.95x / 1.50x (hamming / jaccard) | 1.54x / 1.35x / 1.12x |

(Sums: l2 / cosine / inner product. Memory: engine / reference, peak above the warm
server's. "Filtered" ivfflat queries plan as a sequential scan and a top-N sort on both
sides, the filtered exact k-NN row.)

Where the engine is faster: index builds and inserts (contiguous vectors, the four-row
kernels in the build's assignment and k-means, no buffer manager), unfiltered index queries,
and every full scan of vectors over PostgreSQL's TOAST threshold (768 and 1,536
dimensions: PostgreSQL detoasts each row, the engine reads them in place, 7-10x).

Where it is slower, and why:
- Filtered queries over big vectors (768 / 1,536 dimensions): PostgreSQL's heap holds only
  the TOAST pointers, so its scan reads 148 pages for 20,000 rows and detoasts the 2% the
  filter keeps; the engine stores the vectors inline, so its scan reads every row's bytes
  (decoding only the filter's columns now). The fix is out-of-line storage for big values in
  the engine's row format, a storage format change, not done.
- Index builds' peak memory is above pgvector's for hnsw (the engine's graph and its
  records both in memory while the build writes them; maintenance_work_mem bounds
  pgvector's) and for ivfflat at 128 dimensions. Open.
- ivfflat at 128 dimensions: its build (sampling and the build's heap pass, per row) and its
  queries are about even with pgvector's; above 128 dimensions the kernels dominate and the
  engine is ahead.
- Scans of small tables (under 8 MB: no parallel scan) and the remaining per-row cost of the
  engine's storage iteration (merge of the LSM's sources, block lookups) and row decoding;
  sparsevec's filtered scans and bit's sums are where that shows most.

## Out-of-line storage, inline scans, parallel-worker CPU (round 21, agent 102)

What changed (HANDOFF "Out-of-line storage of large values, inline scans"): big values of
every type are stored apart from their rows; plain sequential scans read the rows as stored
and only the values of the columns they decode, for the rows their filter keeps; scans under
8 MB run the parallel workers' code (kernels, in-place decoding, partial aggregation) on the
executor's thread; scans whose workers would only decode rows stay serial.

Method: release builds, the baseline (`69743b6c` + inert parts) and this round's engine each
over a data directory it loaded itself, against PostgreSQL 18.6 + pgvector 0.8.7 (the
`a89-pgvector` image, `shared_buffers=2GB`); servers on CPUs 16-19, clients on 20-23, the
engine restarted and warmed before each round; rounds interleaved. CPU: the server's user +
system time over the statements (the engine process with its worker threads, in 10 ms
ticks; the container's cgroup `usage_usec`), per statement. The machine was at load 25–35
on all 36 CPUs throughout: times and CPU moved by up to ±30% between rounds, so each figure is
a median of 5–9 statements, two or more rounds; differences under ~25% aren't findings.
Scripts: `tools/bench` `vector` (it now reports `cpu_ms` / `cpu_s` / `cpu_ms_per_query`),
plus psql loops for the cases below.

### vector(1536), 20,000 rows (the heap: 148 pages of TOAST pointers in PostgreSQL)

| case (CPU ms / wall ms per statement) | PostgreSQL | engine before | engine now |
|---|---|---|---|
| `count(*) WHERE cat = 7` | 3 / 1.2 | 14–21 / 16–21 | 6–8 / 5–9 |
| exact k-NN, `WHERE cat = 7` | 9–10 / 7.8 | 20–21 / 18–23 | 12–15 / 14–16 |
| exact k-NN (every vector) | 286–337 / 272–355 | 46–60 / 40–60 | 42–51 / 37–57 |
| `sum(embedding <-> q)` | 319–386 / 297–337 | 44–58 / 42–59 | 54–62 / 55–62 |

The filtered scans read 20,000 small rows and 400 vectors instead of 120 MB of rows. What is
left against PostgreSQL there is per row: the engine reads a row through a merge of the LSM's
sources at ~250–400 ns of CPU (PostgreSQL ~50 ns from a shared buffer page). Full reads
of the vectors stay 5–6x faster than PostgreSQL (which detoasts each); the sum is within the
noise of the baseline (its rounds: 44–58 against 54–62).

### Parallel workers' CPU where PostgreSQL runs serially

PostgreSQL's plans: on vector(128) × 100,000 (7,168 pages) only the unfiltered exact k-NN
runs parallel; a filtered count over a 1,600-page table is serial; over 8,000 pages
(600,000 rows) its aggregates go parallel. The engine reads in workers from 1,024 pages.

| case (CPU ms / wall ms) | PostgreSQL | engine, workers | engine, serial path |
|---|---|---|---|
| 128-d, `count(*) WHERE cat = 7` (PG serial) | 19 / 17 | 42 / 15 | 55 / 56 |
| 128-d, `sum(embedding <-> q)` (PG serial) | 35 / 33 | 71 / 25 | 68 / 67 |
| 128-d, k-NN `WHERE cat = 7` (PG serial) | 20 / 18 | 51 / 23 | 60 / 58 |
| 1,600 pages, `count(*) WHERE a = 7` (PG serial) | 12 / 11 | 47 / 19 | 74 / 79 |
| 600,000 rows, `GROUP BY a % 10` (PG parallel) | 184 / 71 | 513 / 347 | 363 / 329 |

Where the workers filter by kernels or aggregate, they cost *less* CPU than the engine's own
serial path (it decodes every row into values and evaluates the filter with `eval`) and run
2–4x faster: raising the threshold would add CPU and time. The engine's CPU above
PostgreSQL's there is per row, in both paths, not the threads. Where the workers only decode
rows for the executor's thread (the `GROUP BY a % 10`: no kernel for its grouping, no filter),
they bought nothing (347 against 329 ms) for 41% more CPU: such scans now stay serial. And
the serial path's cost is why tables under 8 MB now run the workers' code inline: the
vector(1536) filtered count above went from 14–21 to 6–8 ms of CPU that way.

### The bench's `vector` workload again: vector(128), 100,000 rows (1 round each)

128-dimension vectors stay in their rows (516 bytes, under the threshold), and a 7,168-page
table reads in workers as before: this is the check that nothing else moved. Every query
case's result hash is equal on the three sides.

| case | PostgreSQL | engine before | engine now |
|---|---|---|---|
| COPY 100k | 4.27 s | 2.43 s | 2.74 s |
| CREATE INDEX hnsw (time; peak MB, sampled RSS / anon + shmem; CPU s) | 204 s; 108; 203 | 126 s; 85; 128 | 134 s; 110; 134 |
| hnsw queries (qps; CPU ms / query) | 503; 1.8 | 555; 3.7 | 542; 3.4 |
| hnsw filtered, strict / relaxed (qps) | 45.6 / 71.0 | 51.7 / 72.4 | 56.9 / 72.6 |
| COPY 10k more into hnsw | 37.8 s | 27.0 s | 26.6 s |
| CREATE INDEX ivfflat (time; peak MB) | 1.13 s; 55 | 2.06 s; 6 | 1.77 s; 10 |
| ivfflat queries / filtered (qps) | 200 / 160 | 157 / 100 | 164 / 119 |
| exact k-NN / filtered (ms; CPU ms) | 28.5; 70 / 17.5; 17 | 27.8; 62 / 24.4; 54 | 36.2; 80 / 27.5; 70 |
| `sum` l2 / cosine / ip (ms) | 31.0 / 29.9 / 29.2 | 26.8 / 27.7 / 25.8 | 30.0 / 28.2 / 37.0 |

(The full scans' single rounds sit inside this machine's ±30%; the interleaved rounds above
had them even.) The engine's CPU per hnsw query is about twice PostgreSQL's at a higher qps,
before and after this round (not profiled here).

### pgbench tpcb-like (scale 10, 30 s, prepared)

Interleaved rounds (engine before / now / before / now), CPU per transaction from the server's
CPU over the run:

| clients | PostgreSQL (tps; CPU ms / txn) | engine before | engine now |
|---|---|---|---|
| 1 | 1,133; 0.44 | 735, 721; 1.17, 1.03 | 466, 664; 1.18, 1.15 |
| 8 | 4,529; 0.44 | 2,310, 2,691; 1.25, 1.08 | 2,217, 2,624; 1.16, 1.13 |

The same within the machine's noise (tps moved 30–40% between rounds of one build; CPU per
transaction is the steadier figure). A first, sequential round had the new build 18–35%
behind on tps; interleaving showed it was the load. Scans of pgbench's small tables keep the
serial path (`INLINE_MIN_PAGES`), and now skip the heap-model check that preceded the
parallel decision for them.

### TPC-H subset, SF 0.2 (median of 3 runs; server CPU s per run)

Two interleaved rounds per engine (each on a data directory it loaded), PostgreSQL once.
Every query's result hash is equal on the three sides.

| query | PostgreSQL | engine before | engine now |
|---|---|---|---|
| q01 | 0.56 s; 1.5 | 0.88, 0.68 s; 2.6, 2.3 | 0.88, 0.88 s; 2.7, 2.5 |
| q03 | 0.12 s; 0.3 | 1.40, 0.97 s; 2.3, 2.3 | 1.46, 1.36 s; 2.9, 2.3 |
| q05 | 0.09 s; 0.1 | 1.72, 1.53 s; 1.7, 1.6 | 1.82, 1.70 s; 1.8, 1.6 |
| q06 | 0.06 s; 0.1 | 0.57, 0.48 s; 1.0, 0.9 | 0.62, 0.62 s; 1.1, 1.0 |
| q10 | 0.18 s; 0.4 | 1.64, 1.69 s; 2.4, 2.6 | 1.88, 1.77 s; 2.8, 2.8 |
| q12 | 0.16 s; 0.4 | 0.78, 1.08 s; 1.4, 1.6 | 0.81, 0.64 s; 1.5, 1.3 |
| q14 | 0.05 s; 0.06 | 0.48, 0.95 s; 1.0, 1.2 | 0.59, 0.50 s; 1.1, 1.0 |
| q19 | 0.02 s; 0.02 | 0.16, 0.29 s; 0.16, 0.18 | 0.11, 0.16 s; 0.12, 0.15 |

q03, q05, q06 and q10 came out 5–15% behind in both rounds, the others level or ahead; that
is at the edge of this machine's noise, and the new engine's data directory was the younger
(less compacted) one. Not resolved: if real, the candidates are the per-value pointer check
in the workers' decode (`Lazy::decode`) and the raw chunk scans' value source.

### hnsw build memory (item 2: measured, not changed)

vector(128) × 100,000, m 16, ef_construction 64, maintenance_work_mem 64 MB on both sides:
PostgreSQL + pgvector 243 s, 242 s CPU, peak +66 MB (anon + shmem; the graph outgrows
maintenance_work_mem after 54,687 tuples and the rest go in on disk). The engine 124–147 s,
123–146 s CPU (the same switch, 54,681–54,709 tuples, the levels unseeded), peak by its RSS
high-water mark reset before the statement: +38 MB and +48 MB in fresh processes (one after
the index was dropped and the engine restarted), +123 MB for a second build in a process
that had built and dropped one (memory the first build left to the allocator and the block
cache). The engine's index is an object model held whole in memory (`hnsw::graph::Graph`:
every vector and neighbor list) while the build's on-disk phase inserts into it; bounding it
at maintenance_work_mem needs that phase to page the graph through `AmStore` (records read
and evicted on demand), which is the next step.

## Catalog round 4: racing DDL commits, catalogs after DDL (round 19, agent 62)

Same harness and catalogs as agent 58's: PostgreSQL in `a62-pg` (pinned image and settings),
both servers on cores 16-19, the client on 20-23, other agents' builds and regression runs
loading the machine (load 15-25). **before** the engine at `47d67b87`, **after** this round's,
release builds, each run from a copy of the same data directory.

### pg_restore of 3,000 tables (`tests/pg_dump/scale_setup.sql` without the big table, `pg_dump -Fc` from PostgreSQL; seconds, 0 errors, 3,069 tables restored each time)

| | pg | before | after |
|---|---|---|---|
| `pg_restore` | 25.6 | 67.8 | 24.9 |
| `pg_restore -j 4` | 25.3 | 227.1 | 43.5 |

Every DDL commit that raced another (pg_restore -j's indexes, constraints and comments)
merged its catalog onto the current one under the commit lock, and that was O(catalog):
the merge rebuilt the relation name index and children lists of every relation
(`Catalog::reindex`) and checked every name, serialized all three catalogs' headers to JSON
(their comment lists hold one item per commented object) and merged lists that both sides
changed pair by pair (quadratic: every COMMENT ON against every other). Now
(`txn/catmerge.rs`) only the relations whose records this transaction changed are reindexed
and name-checked (`Catalog::reindex_rels`, checked against the full rebuild in debug builds),
only the header fields it changed are merged (`persist::header_changed`, compared as values),
comments by object (`merge_comments`), lists through hash sets. Per statement and
transaction, outside the merge: `Catalog::comments` is shared by a catalog's copies until one
changes it (every DDL statement copied every comment), dropped relations' access methods and
file numbers are found from the statement's changes (every statement built a set of every
relation), the shared statistics drop dropped objects' entries once per catalog version
(every transaction's end visited every table's entry under the lock all sessions share), and
`referencing_fks` reads the foreign key index. A serial restore is now PostgreSQL's speed;
`pg_restore -j 4` is 5x faster than before but still slower than a serial one: racing commits
take turns under the commit lock, the header (with every comment) is still serialized,
compressed and written by every DDL commit, and `SharedCatalog::extract` reads the comments
per commit (comments as records of their own is the fix: a catalog format change).

### 10,000 tables (ms per statement, medians of 3 runs; `scale-ops`, queries 5 times each)

| case | pg | before | after |
|---|---|---|---|
| `count(*)` of pg_class, after DDL (COMMENT, then the count) | 11.4 | 78.6 | 71.4 |
| pg_class × pg_attribute of a schema, after ALTER TABLE | 14.8 | 282 | 200 |
| ORM constraints query, after DDL | 29.3 | 340 | 278 |
| psql `\d t` lookup, after DDL | 0.57 | 12.9 | 1.42 |
| `count(*)` of pg_class (the first after the setup's DDL included) | 7.7 | 40.8 | 39.9 |
| DROP TABLE | 4.9 | 2.3 | 1.5 |
| ALTER TABLE ADD COLUMN | 0.44 | 1.00 | 0.98 |
| CREATE TABLE | 2.9 | 2.2 | 2.6 |

pg_class, pg_attribute and pg_attrdef of a new catalog version are spliced from the last
generated version's rows (`catalog/sysdelta.rs`): only the relations whose entries differ
(found through the relation maps' `PMap::changes`, with what each change implies: TOAST
tables, indexes, parents, partitions, foreign key targets, owned sequences) are generated
again; a change to anything every row reads (schemas, roles, privileges, types, collations,
triggers, rules, ...) generates the catalog whole. That removes the generation, not the
work proportional to the catalog: the spliced rows are copied, and a scan clones each row it
returns, so `count(*)` of pg_class after DDL is still 6x PostgreSQL's. pg_constraint and
pg_type (the ORM query) and pg_index aren't spliced (their rows read more than their
relation's entry). Rows as shared `Arc`s (a version reusing an unchanged relation's rows by
pointer) are the next step.

The first connection to a database (agent 58's 135-160 ms at 10,000 tables) and ALTER TABLE
ADD COLUMN's per-statement overhead weren't worked on this round.

### Verification (round 19, agent 62)

`cargo test -p pg-engine` (every test binary), clippy (no new warnings), `difftest smoke`; the
isolation suite on a private reference: 120 / 122 specs, 1,646 / 1,648 permutations, as
committed; the regression suite in full on a private reference against the committed
`results.tsv`: 51,148 / 51,150, no unit changed. New tests: golden `a62_depend_gaps` (recorded from PostgreSQL),
`tests/ddl_scaling.rs` `whole_catalogs_after_ddl_are_spliced`,
`racing_ddl_commits_dont_grow_with_the_catalog`, `racing_create_and_drop_keep_catalog_counts`,
and the debug cross-checks (spliced rows against the full generation, a merged catalog's name
index against `reindex`, dropped relations' access methods and file numbers against a scan,
`referencing_fks` against a scan).

## Catalog round 5: catalog rows shared across versions, comments as records, group-committed DDL (round 20, agent 76)

Same harness and catalogs as agents 58 and 62 (`scale --keep 1`: 10,000 tables in 20 schemas
and a table of 1,000 partitions; `scale-ops`, queries 5 times each). PostgreSQL in `a76-pg`
(pinned image and settings), both servers on cores 8-11, the client on 12-15, other agents'
builds loading the machine. **before** the engine at `80fa7310` (agent 62's), **after** this
round's, release builds, each run from a copy of the same data directory.

### 10,000 tables (ms per statement; `scale-ops`)

Medians of 3 interleaved runs (pg, before, after), queries 5 times each: the machine was
loaded (load 12-20; PostgreSQL's own times are 1.5-4x agent 62's), so compare within a row.
In-process, without the protocol, `count(*)` of pg_class is 0.05 ms once its rows are shared
and 0.9-1.2 ms after COMMENT or CREATE TABLE (a splice), the pg_class x pg_attribute join 8.6
ms (17 ms right after ALTER TABLE), the ORM query 10.5 ms (11 ms after COMMENT, 24 after ADD
COLUMN); pg_type's first generation 1,344 ms before, 44-54 ms after.

| case | pg | before | after |
|---|---|---|---|
| `count(*)` of pg_class, after DDL (COMMENT, then the count) | 11.75 | 88.63 | 0.48 |
| pg_class x pg_attribute of a schema, after ALTER TABLE | 14.43 | 315.03 | 30.35 |
| ORM constraints query, after DDL | 47.01 | 526.54 | 21.38 |
| `count(*)` of pg_class (the first after the setup's DDL included) | 12.24 | 48.59 | 22.41 |
| pg_class x pg_attribute of a schema (the first included) | 15.01 | 75.83 | 69.04 |
| ORM constraints query (the first included) | 58.91 | 366.45 | 31.72 |
| psql `\d t` lookup, after DDL | 0.67 | 4.01 | 2.36 |
| psql `\d t` indexes, after DDL | 1.64 | 4.88 | 2.90 |
| information_schema.columns of a table, after DDL | 5.79 | 2.99 | 2.06 |
| COMMENT ON TABLE | 0.57 | 1.53 | 0.32 |
| ALTER TABLE ADD COLUMN | 0.69 | 4.03 | 1.36 |
| CREATE INDEX | 1.38 | 3.46 | 1.97 |
| CREATE TABLE | 4.74 | 4.61 | 4.11 |
| DROP TABLE | 7.47 | 6.38 | 3.25 |

ALTER TABLE ... DISABLE / ENABLE TRIGGER ALL of a table without triggers (in-process, 50
tables): 1.71 / 1.40 ms before (agent 66 measured 1.4 ms over the wire against PostgreSQL's
0.1), 0.60 / 0.60 ms after; ALTER TABLE ADD COLUMN in-process 0.69 ms before, 0.79-0.85 after
on the more loaded machine (no change: what is left is the statement's own work, below).

### pg_restore of 3,000 tables (`tests/pg_dump/scale_setup.sql`, `-v big=10`, `pg_dump -Fc` from PostgreSQL; seconds, 0 errors, 3,069 tables each time)

Two rounds, each interleaved (the second on a less loaded machine):

| | pg | before | after |
|---|---|---|---|
| `pg_restore`, round 1 | 35.4 | 126.4 | 30.0 |
| `pg_restore -j 4`, round 1 | 30.8 | 122.5 | 24.1 |
| `pg_restore`, round 2 | 26.5 | 51.9 | 22.3 |
| `pg_restore -j 4`, round 2 | 24.2 | 85.1 | 18.6 |

`pg_restore -j 4` now beats a serial restore by about 20%, as on PostgreSQL, and both are
faster than PostgreSQL's. What the parallel restore's commits spent before (commit timers,
an earlier build of this round, -j 4: 32,000 commits, 8,500 of them merged onto a racing
one): merging 7.5 s under the commit lock, 4.7 s of it comparing and copying `filenodes`,
which held a file number for each of the 3,000 tables pg_restore -j truncates before their
COPY; waiting for that lock 5 s; the shared catalogs' lock, held through every DDL commit,
serialized the rest.

### What changed

1. **Catalog rows in shared chunks** (`catalog/sysrows.rs`, `catalog/sysdelta.rs`). A system
   catalog's shared rows (`Database::sys_rows`) are kept in chunks of about 64 rows that end
   at an object's last row, each chunk an `Arc` with the runs of rows of one object it holds.
   A new catalog version's rows are the last generated version's chunks by pointer, the
   chunks holding the objects its DDL changed made again (the objects found by binary
   searches over the runs' positions, so the splice asks the catalog about the changed
   objects only, and touches no row of any other). A version in which nothing a catalog's
   rows read changed (COMMENT, statistics) reuses the earlier rows whole, their hashes too.
   Agent 62's splice (pg_class, pg_attribute, pg_attrdef) copied every row of the catalog per
   version (a `Vec` of deep copies: 221,000 rows of pg_attribute); per-row `Arc`s, tried
   first, cost a cache miss per row to count and to splice, about as much. Spliced now too:
   pg_index (TOAST tables' indexes included), pg_constraint (the tables' constraints; those
   of constraint triggers and domains are kept in place when nothing they read changed),
   pg_type (the relations' row types and their arrays). Kept whole when unchanged: pg_depend
   and information_schema's constraint views (table_constraints, key_column_usage,
   referential_constraints, check_constraints, constraint_column_usage), cacheable now
   (`system::rows_cacheable`; pg_depend keyed by the session's search path, which its
   stored texts are read under). Debug builds compare every splice and every reuse with the
   full generation, and every chunk's runs with its rows.
2. **Scans copy only what is read** (`system::project`, the plan's `exec::need` mask: the
   other columns NULL), and `count(*)` over a catalog scan counts the shared rows without
   making any (`Node::count_rest`, `AggNode`). An equality on the column naming a row's
   object (`attrelid`, `adrelid`, `indexrelid`, pg_class's `oid`) is served from where the
   object's rows lie, without a hash of every row (`SysRows::run_of`).
3. **Hash joins restrict a catalog scan to the inner side's keys** (`Node::restrict_keys`):
   an inner or semi hash join on one oid column, its inner side built with at most 1,024
   rows, has its outer catalog scan read only the rows of those keys through the scan's hash
   (`pg_class JOIN pg_namespace ... WHERE nspname = ...`: 1,000 of 41,000 rows), when the
   scan's filter can't raise or call anything (columns and constants under built-in
   comparisons), so nothing left out would have shown.
4. **Quadratic catalog generation**: pg_type asked every table whose row type each row is
   (`Catalog::user_data_type`, the owner pass: 1.3 s at 10,000 tables, now through the row
   type indexes, `relindex`), pg_description looked every built-in description up in the
   comment list; `pg_get_constraintdef` scanned every table per call (the ORM query's 1,500
   calls: 225 ms; now `con_owner`).
5. **Comments are records of their own** (catalog format 2, `catalog/comments.rs`): per
   object, by sub-id, each with the tick of when it was set (`Catalog::comment_tick`, in
   the header: pg_description's order, a comment set again a new row at the end). A DDL
   commit writes the records of the objects whose comments it changed (the header held every
   comment and was written whole by every commit); obj_description / col_description and the
   shared catalogs' sync look comments up by object (`SharedCatalog::extract`, every DDL
   commit, scanned them all); a racing commit's comments merge by object
   (`catmerge::merge_comments`), the ones it set ticked again after the current ones.
6. **Racing DDL commits** (`pg_restore -j`): a commit of a database's own objects whose copy
   of the shared catalogs is current no longer takes the shared catalogs' lock, which every
   DDL commit held through its whole commit, merge and WAL write included
   (`Engine::commit`); catalog commits are group-committed (`Store::commit_grouped`: appended
   under the commit lock, the next commit checking against and merging onto the appended
   catalog, `Database::latest_catalog`; the fsync waited for without the lock; published once
   durable, never over a later commit's, `Database::publish_catalog`); the merge copies the
   entries only this transaction changed by pointer and merges as JSON only what both changed
   (`persist::changed_record_keys` / `same_record` / `copy_record`); the shared statistics'
   prune of dropped objects looks at what changed since the version pruned last
   (`pgstat::prune_since`; every DDL commit visited every table's entry under the lock all
   sessions share). And `filenodes`, `rel_am` and `rel_tablespace` are persistent maps
   (`PMap`): pg_restore -j truncates every table before its COPY, which gives each a new file
   number, and every racing merge compared and copied the whole map (4.7 s of a 3,000-table
   restore's 7.5 s of merging).
7. **Per-statement DDL overhead**: `has_detach_pending`, which every statement after DDL
   asked of every table (a scan per catalog version), is an index of the tables map
   (`TableIx::detach_pending`); ALTER TABLE ... ENABLE / DISABLE TRIGGER ALL built every
   table's deferrable keys' triggers to keep one table's (`recheck_triggers_from`).

### Catalog format 2: upgrade and downgrade

Comments moved from the catalog header into records under tag 50 (`persist::COMMENTS_TAG`).
A database stored in format 1 (or 0) is rewritten when it is first opened, in one commit
(its comments ticked in their header order, so pg_description's order is unchanged).
Downgrading: stop the server cleanly, run `roll-engine --data <dir> --downgrade-catalog`
(`Engine::downgrade_catalogs`: every database's catalog stored as format 1 has it, comments
in the header again, their records deleted), then start the older build. It refuses nothing
itself: run it on a cleanly stopped data directory (an older build replaying this one's WAL
after a crash isn't supported, round 18's rule), and don't open the directory with this build
in between (that upgrades it again). Test `tests/catalog_format.rs`.

### First connection

In-process, 10,000 tables, 3 interleaved runs (`bench --target inproc startup-catalog`, the
connect and its first `count(*)` of pg_class): before 126-184 ms, after 130-161 ms (a
database already in format 2); the first open of a format-1 database pays its rewrite once
(290-350 ms). Not near-constant: a database's records are now scanned and parsed per map on
up to 8 threads (`persist::load`), which only overlaps what was serial, and inserting the
11,000 tables' entries with their indexes, `reindex` and the first catalog generation are
still O(catalog). A near-constant open needs the catalog's secondary indexes (names,
`relindex`) stored and its entries parsed on first use (a lazy `PMap` chunk holding its
records' bytes), or a binary snapshot of the built catalog: neither is done.

### Verification (round 20, agent 76)

`cargo test -p pg-engine` (every test binary), clippy (no new warnings), `difftest smoke`
(passes, against `a76-pg`); the isolation suite on a private reference: 120 / 122 specs,
1,646 / 1,648 permutations, as committed; the regression suite in full (debug build, private
reference `a76-ref1`) against the committed `results.tsv`: **51,148 / 51,150, no unit
changed**; over that run's database, `pg_dump_roundtrip` (dumps in every format alike to the
last round's but one entry each, restores of the reference's dump into the engine 0 errors
every way; the engine's dump restored into the reference fails 4 statements, the domain over
a domain with a type modifier, `mynums2`, whose `typtypmod` the engine shows as its base's:
the same with the engine before this round, `80fa7310`) and `pg_dump_tap` (14,817 / 14,817,
002_pg_dump.pl 14,609 / 14,609). New tests: `tests/catalog_format.rs` (format 1 upgraded with
its comments and their order, downgraded with `downgrade_catalogs`, a COMMENT's bytes written
not growing with the comments), `tests/catalog_storage.rs`
`group_committed_ddl_keeps_every_change` (four sessions' racing DDL with the WAL fsynced, read
back after a restart and after a crash), `tests/ddl_scaling.rs`
`more_catalogs_after_ddl_are_spliced_or_kept`, and the debug cross-checks (every splice and
reuse against the full generation, every chunk's runs against its rows, tables pending detach
and `pg_get_constraintdef`'s owner and `user_data_type`'s relation against scans).

## Catalog round 6: first batch (round 21, agent 92)

Same catalog as rounds 4-5 (`scale --keep 1`: 10,000 tables in 20 schemas, a table of 1,000
partitions), release builds in-process (`bench --target inproc startup-catalog`, a fresh copy
of the data directory per run), the machine quiet (load 2). **before** `b6659eb9`, **after**
this batch.

| case | before | after |
|---|---|---|
| first connection: connect (opens the database) | 100-250 ms | 95-180 ms (unchanged: the next batch) |
| first connection: its first `count(*)` of pg_class | 74-131 ms | 2.9-7.4 ms |
| second connection: its first `count(*)` of pg_class | 70-145 ms | 0.1 ms |

The second session's count was a bug: every session generated its own copy of the shared
catalog rows, because each session's search path starts with its own `pg_temp_<pid>` and the
path keyed the cache. A whole catalog read without a filter now counts what the relation maps
keep (`system::row_count`) and makes rows only if one is read. Catalog scans' filters run as
compiled kernels where they compile (`relkind = ANY (...)` went through `eval` per row).

### Verification (round 21, agent 92, first batch)

`cargo test -p pg-engine` (every test binary), clippy (no new warnings), `difftest smoke`; the
isolation suite on a private reference: 121 / 122 specs, 1,647 / 1,648 permutations; the
regression suite in full (debug build, private reference `a92-ref1`) against the committed
`results.tsv`: 51,148 / 51,150, no unit changed; `pg_dump_tap` 14,817 / 14,817;
`pg_dump_roundtrip` over that run's database: 0 errors restoring either side's dump into the
other, every way (the engine's dump into PostgreSQL failed 4 statements before), dumps
2,720 / 2,733 entries alike. Golden `a92_domain_typmods`.

## Catalog round 6, second batch: DDL at scale, catalog format 3, 90,000 objects (round 21, agent 92)

PostgreSQL in `a92-bpg` (`run.sh pg-start`: pinned image and settings), both servers on cores
24-27, the client on 28-31, release builds, a fresh copy of each data directory per run.
**A** is the first batch (`7afe80e8`), **B** this one.

### DDL over the wire, 10,000 tables (`scale-ops --only ddl --n 200`, ms per statement)

Medians of 3 interleaved runs (load 18-20 from other agents' builds):

| case | PostgreSQL | A | B |
|---|---|---|---|
| CREATE TABLE | 3.10 | 1.29 | 0.60 |
| ALTER TABLE ADD COLUMN | 0.45 | 0.57 | 0.34 |
| CREATE INDEX | 0.91 | 0.60 | 0.41 |
| COMMENT ON TABLE | 0.24 | 0.17 | 0.20 |
| DROP TABLE | 5.58 | 1.28 | 0.76 |

(A's figures from an earlier interleaved round on a quieter machine, PostgreSQL then 0.33-0.47
for ALTER TABLE.) What ALTER TABLE spent was the catalog maps' chunk index: 11,000 tables'
oids made a flat index of about 1,100 chunks, which every change copied and dropped and every
one of a statement's diffs walked (`PMap::changes`, `persist::diff_records`: about 40% of the
server's time). `PMap` now indexes its chunks by groups of 64, each behind an `Arc`, every
level a sorted vector (cheaper to copy than tree nodes, and small slice iterators).

### 90,000 objects (`scale --tables 25000 --parts 10000`, then `scale-more`: 10,000 functions, 10,000 views, 10,000 more indexes)

Per statement, in steps of 5,000 (2,500 for `scale-more`); the engine in-process, PostgreSQL over
the wire (its in-process figure would be lower by about 0.1 ms):

| case | PostgreSQL | B |
|---|---|---|
| CREATE TABLE, at 5,000 / 25,000 tables | 2.41 / 2.71 | 0.31 / 0.46 |
| CREATE TABLE ... PARTITION OF, partitions 0-5,000 / 5,000-10,000 | 15.3 / 33.7 | 2.4 / 8.8 |
| ALTER TABLE ADD COLUMN, 25,000 tables | 0.56 | 0.30 |
| CREATE INDEX, 25,000 tables | 1.13 | 0.37 |
| CREATE FUNCTION (0-10,000 functions) | 0.28-0.30 | 0.35 (0.45-0.63 before `FuncIx`) |
| CREATE VIEW | 1.10-1.14 | 1.07-1.67 |

Both grow with a parent's partitions (each new partition's bound is checked against its
siblings'); the engine 4-6x less.

Client tools' and ORMs' catalog queries over that catalog (`catalog-tools`, over the wire, ms;
first run, then the mean of 5):

| query | PostgreSQL first / then | B first / then |
|---|---|---|
| psql `\dt` (a schema's 1,250 tables) | 48.6 / 27.8 | 271 / 41.6 |
| psql `\dv` | 21.5 / 21.2 | 23.8 / 23.8 |
| psql `\df` | 9.7 / 2.7 | 17.6 / 2.9 |
| psql `\d t` lookup | 0.45 / 0.31 | 19.7 / 6.8 |
| psql `\d t` indexes | 2.5 / 0.67 | 103 / 14.3 |
| pgAdmin's table list | 463 / 435 | 61 / 47 |
| DBeaver's table list | 57 / 45 | 12.2 / 11.8 |
| Django's table list | 285 / 83 | 83 / 85 |
| Django's constraints of a table | 2.1 / 0.63 | 64 / 2.4 |
| Rails' columns of a table | 1.8 / 0.86 | 174 / 2.1 |
| Prisma's columns of a schema | 324 / 252 | 1,135 / 1,111 |
| the ORM constraints query | 97 / 65 | 45 / 27 |
| information_schema.tables of a schema | 35 / 35 | 39 / 30 |
| information_schema.columns of a table | 4.1 / 3.4 | 2.7 / 1.8 |
| `count(*)` of pg_class | 8.6 / 8.5 | 0.28 / 0.12 |

Before this batch (same catalog, B without the last fixes): Rails' columns 12.3 ms
(a nested loop's join condition copied 2,742 pairs of wide catalog rows into one row each to
reject them; now checked in place by its compiled kernel), `\d t` lookup 13.9 (pg_class's name
bounds checked against all 140,000 rows although an index equality had picked one), `\d t`
indexes 30, `\df` 11.1, Prisma 2,326. The engine's first runs generate the catalogs they read
whole (pg_type, pg_attribute, pg_constraint: 100-300 ms here); PostgreSQL reads them from its
heap.

### Resources

| | PostgreSQL | A | B |
|---|---|---|---|
| on disk, 10,000 tables (database / its catalogs) | 265 MB / 145 MB | 1.7 MB | 1.1 MB |
| on disk, 90,000 objects | 785 MB / 422 MB | | 3.6 MB |
| memory per idle connection (`idle --sessions 100`) | 1,546 kB | | 279 kB |
| connection setup | 14.2 ms | | 8.7 ms |
| engine RSS, 10,000 tables: started / database open / pg_attribute, pg_type, pg_class generated | | | 25 / 129 / 500 MB |
| engine RSS, 90,000 objects: the same | | | 25 / 359 / 1,484 MB |

Catalog format 3 leaves fields at their default values out of each record (a table's record
1.9 kB → about 0.7 kB of JSON): smaller writes per DDL commit, a third less on disk. The
generated catalog rows are what is heavy: shared rows of every catalog a query has read, a
`Value` per column of every row. Making them compact
(columns stored as the catalog entries they come from, rows made per scan) is the next
resource item.

### Verification (round 21, agent 92, second batch)

`cargo test -p pg-engine` (every test binary; `tests/wire.rs`' racing CREATEs of one name now
wait and fail as PostgreSQL's do), clippy (no new warnings), `difftest smoke`; the isolation
suite on a private reference: 121 / 122 specs, 1,647 / 1,648 permutations, and the engine's own
specs 88 / 88 (`roll-concurrent-create` new); the regression suite in full (debug build,
private reference `a92-ref1`) against the committed `results.tsv`: 51,148 / 51,150, no unit
changed; `pg_dump_tap` 14,817 / 14,817; `pg_dump_roundtrip` over that run's database: 0 errors
restoring either side's dump into the other, every way; dumps 2,720 / 2,733 entries alike.
New tests: `catalog_format.rs` `records_leave_defaults_out_and_downgrade_whole`, spec
`roll-concurrent-create`, the debug cross-check of `functions_named` against a scan.


### Third batch: Prisma's columns query (round 21, agent 92)

`information_schema.columns` / `.tables` scans reading only the relations their filter names
(`system::is_rows_named`) took `table_schema = 'x'` but not `table_schema = ANY ('{x}')`, which
Prisma sends, so its columns query generated every relation's columns (90,000 objects: the
view's scan alone 1.1 s, against 84 ms with `=`). A one-element `= ANY` now counts as the
equality. Prisma's columns query at 90,000 objects, over the wire, first / then: 1,135 / 1,111
ms → 842 / 427 (PostgreSQL 324 / 252). Golden `a92_is_any`. Verified as the second batch was (crate tests, clippy,
smoke, isolation 121 / 122, regress 51,148 / 51,150 with no unit changed, pg_dump_tap, round trip
0 errors both ways).

## Deep input (round 19, agent 63)

The stack grows on demand at recursion points (HANDOFF.md, "Deep input"). This section
measures what the guards cost on ordinary statements. The guards sit on hot paths
(`eval`, executor `open`, the binder); the fast path is a thread-local load and two
compares.

The A/B ran in-process (`bench --target inproc`, the client threads 2 MB stacks) against
the base commit `dd758b2b`, alternating base and new runs on the same loaded machine, over
the same data:

- **TPC-H SF1:** the 22 queries' median times, base against new, are within -6% to +9% of each
  other. The geometric mean of the new engine's speed is 1.005.
- **OLTP** (1 and 8 clients):
  - The first version mapped a stack segment for every parse on a 2 MB thread, because
    libpg_query was given 3 MB below it. The simple-query point select ran at 0.71× and
    0.65×.
  - Now a parse on the thread's own stack only needs 1 MB, since libpg_query sizes its
    limit from what is left. Segments are kept per statement, and one small segment
    between statements. With that, the 18 OLTP results' best-of-3 speed against the base
    has a geometric mean of 1.04, from 0.92 to 1.35. The spread is machine noise: other
    agents' builds ran meanwhile.

The server's connection threads (16 MB) never grow at ordinary depths. A statement that
nests deep pays for its segments: a mapping per segment for each statement, and the page
faults of the stack it touches. For example, 4,000 `+` terms touch about 30 MB in release.

## Round 19 (agent 74): the write path and lock memory

Agent 69 bounded a big transaction's write buffer; the per-row cost of the bulk steps was
left. Profiled first (`tools/bench/perf.sh`, a frame-pointer release build:
`RUSTFLAGS="-C force-frame-pointers=yes"`, `perf record --call-graph fp`, one statement at a
time). Same harness: servers on cores 20–23, clients on 24–27, on a loaded machine (load
15–20). Bulk times are medians of 3 interleaved runs, each engine from a fresh data
directory: **before** is `0d8c976c` (this round's base), PostgreSQL in the same window.

### Bulk, 1M rows (seconds; peak memory of the whole run)

| step | pg | before | batch 1 | batch 1 / pg |
|---|---|---|---|---|
| COPY in | 0.81 | 4.80 | 2.45 | 0.33x |
| CREATE INDEX | 0.32 | 2.41 | 2.49 | 0.13x |
| CREATE UNIQUE INDEX | 0.27 | 2.09 | 2.07 | 0.13x |
| COPY out | 0.40 | 1.47 | 0.59 | 0.67x |
| INSERT … SELECT | 0.71 | 3.04 | 2.15 | 0.33x |
| UPDATE half | 4.29 | 3.43 | 3.41 | 1.26x |
| DELETE a quarter | 0.20 | 2.19 | 2.09 | 0.10x |
| VACUUM | 0.24 | 0.82 | 0.75 | 0.32x |
| ANALYZE | 0.11 | 1.19 | 1.04 | 0.11x |
| count(*) after | 0.04 | 0.14 | 0.15 | 0.25x |
| peak memory | 334 MB | 856 MB | 848 MB | |

### What the profiles showed (batch 1)

- COPY in, before: the heap model 26% (the first 400,000 rows modelled, multi-inserted and
  their TIDs written into the stored rows again, then the whole model dropped when the load
  passed its size), the write buffer 25%, parsing 18%, pg_stat_io 5% (three `String`s and two
  SipHash lookups under a global mutex per row), the commit 9%.
- COPY out: `float8out` 31% (two `format!` calls, a parse and a big-integer check per
  value), escaping and a `String` per field.
- INSERT … SELECT: the heap model 24% (its `HashMap` of keys alone 5%), pg_stat_io 8%, the
  write buffer 17%.

### What changed (batch 1)

1. **The heap model of a big statement waits for its end** (`sizing::Defer`). COPY's multi-
   insert batches and an INSERT … SELECT's rows past the statement's first 10,000 are put in
   the model when it ends, in order and the way they went in, and their TIDs stamped then. A
   statement taking the table past the model's size (400,000 row versions) stops modelling
   it at that row, as before, but places only the waiting rows' heap pages (for the size
   estimate) and drops them: none of their index entries, TOAST rows or TIDs are made. A
   failed statement's waiting rows are modelled when it fails (dead tuples, as before).
   INSERT … SELECT defers only when nothing it runs can read the model or the rows' TIDs
   before it ends: no BEFORE ROW trigger, RETURNING, ON CONFLICT, RLS check, volatile default
   or check, and a source that runs nothing volatile and doesn't scan the table
   (`dml::model_deferrable`). `tests/big_tables.rs` `deferred_model_matches` runs the same
   loads deferred from the first row, from the 50th and never: identical TIDs, relpages,
   reltuples and relation sizes (past the model's size too, and a failed COPY).
2. **TIDs are stamped in place** (`Txn::restamp`): a row the current command wrote and whose
   version is still in the write buffer gets its TID without a second journal entry and
   buffered copy.
3. **Floats print by Ryu** (`ryu` crate; `text::shortest_digits`), the algorithm Postgres runs,
   except that Postgres doesn't take a decimal exactly on a midpoint to a neighbour (the crate
   does for even mantissas): such a result goes to the old search. A test compares the two
   over 200,000 random bit patterns, every power of two and short decimals: they differ only
   at powers of two, where the old search was wrong (2^-24: Postgres prints
   `5.960464477539063e-08`, the search printed 17 digits).
4. **COPY parsing borrows**: a text-format line without a backslash splits into slices of the
   data (`InField` holds `Cow`s); integers and plain decimals parse without lowercasing a copy;
   COPY TO writes integers (`itoa`) and text needing no escape straight into the line.
5. **Per-row costs**: pg_stat_io's keys are `&'static str`s in a fast-hash map; the tuple
   length is worked out once per inserted row (it was three times: compression bookkeeping,
   layout, estimate) without collecting the column types or copying the row; a row with every
   column given goes in as it is (`new_row`); no clone of each row for ON CONFLICT's retry when
   there is no ON CONFLICT; stored rows are encoded into one buffer; DELETE's and UPDATE's
   "seen" sets use the fast hasher; a row lock no longer copies the table's name.
6. **ANALYZE** takes the row count from its own scan instead of counting the table again, and
   sorts index keys unstably (the same order: equal (key, size) pairs are interchangeable).

### Verification (batch 1)

Every crate test binary at the default spill threshold and with `ROLL_TXN_SPILL_BYTES=4096`
(25 of 25 each); clippy adds no warnings; `difftest smoke` passes; the regression suite in
full on a private reference against the committed `results.tsv`: 51,148 / 51,150, no unit
newly differing or newly matching.

### Batch 2: compact locks

A transaction held a lock-manager entry per row it locked or wrote (the key twice, a holder
list, its own list of what to release: about 200 bytes) and, for a table with a unique index,
a heavyweight lock with its key holder per inserted key (the key in the lock table, in the
transaction's held-lock map and in its journal: a few hundred bytes). Past 2,048 row locks
(and, apart, 2,048 unique keys) a transaction's next ones go into compact sets
(`txn/bulklocks.rs`), per (sub)transaction: each key once in an arena with a byte of what is
held (strength, whether an update's or delete's), under an open-addressing index whose slots
carry 32 bits of the key's hash, about 20 bytes plus the key. Postgres keeps both kinds in the
tuples (xmax) and the index entries; the engine's writes stay private until commit, so others
must still find them through the lock manager, which consults the sets wherever it looks a row
or a unique key up. A unique key another writer asks for moves into the main table first
(`State::materialize`), so queueing, waits, deadlock detection and pg_locks go through the
usual paths; the sets are released with their transaction or subtransaction, and PREPARE
TRANSACTION expands them into the main tables (`Txn::expand_compact_locks`) to be recorded and
moved as any. UPDATE and DELETE over a source that returns each row once (scans under filters,
projections, sorts or appends of distinct tables) no longer remember every row they changed
(`dml::rows_once`).

One transaction of 5M rows (`INSERT INTO big SELECT g, g % 1000, md5(g::text) FROM
generate_series(1, 5000000) g`, a primary key), then `UPDATE big SET b = b + 1` in another,
the engine's peak RSS (fresh server each; seconds, MB):

| | before | batch 2 |
|---|---|---|
| INSERT … SELECT | 76 s, 2,645 MB | 48 s, 501 MB |
| UPDATE of all 5M (peak so far) | 41–45 s, 3,475 MB | 27 s, 1,029 MB |

The 1M bulk run, the same method as above (3 interleaved runs; this window's medians):

| step | pg | before | batch 1 | batch 2 |
|---|---|---|---|---|
| COPY in | 0.81 | 4.43 | 2.39 | 2.53 |
| CREATE INDEX | 0.39 | 2.24 | 2.22 | 2.18 |
| CREATE UNIQUE INDEX | 0.23 | 1.96 | 2.04 | 2.42 |
| COPY out | 0.38 | 1.42 | 0.60 | 0.54 |
| INSERT … SELECT | 0.68 | 2.73 | 2.50 | 2.01 |
| UPDATE half | 4.15 | 3.14 | 3.13 | 2.12 |
| DELETE a quarter | 0.21 | 2.06 | 1.93 | 1.70 |
| VACUUM | 0.22 | 0.79 | 0.72 | 0.74 |
| ANALYZE | 0.10 | 1.18 | 0.98 | 0.99 |
| count(*) after | 0.04 | 0.13 | 0.16 | 0.10 |
| peak memory | 349 MB | 855 MB | 839 MB | 799 MB |

(The index builds take no row locks: their spread is the machine's load.)

### Verification (batch 2)

The isolation suite on a private reference, with every lock that can be compact made compact
(`ROLL_COMPACT_LOCKS_AFTER=0`) and at the default: 120 / 122 specs, 1,646 / 1,648
permutations, 12,232 / 12,238 steps both times, the same two specs differing as committed.
`tests/compact_locks.rs`: NOWAIT, SKIP LOCKED, lock_timeout and lock strengths against compact
row locks, waits for compact in-progress unique keys (a waiting writer getting the unique
violation at commit), savepoints rolled back and released, a key inserted again by its own
transaction, PREPARE TRANSACTION and COMMIT PREPARED; `bulklocks::tests::keyset_matches_a_map`.
Every crate test binary at the default, with `ROLL_TXN_SPILL_BYTES=4096` and with
`ROLL_COMPACT_LOCKS_AFTER=0`; clippy adds no warnings; the regression suite in full on a
private reference against the committed `results.tsv`: 51,148 / 51,150, no unit changed.
(pgrowlocks, added meanwhile, reads row locks too: it consults the compact sets.)

### Batch 3: measuring without decoding

VACUUM, ANALYZE and CREATE INDEX measure a table too big to model row by row
(`Ddl::measure_rows`). Now a row's heap tuple is sized from its stored bytes when every value
is a null, a boolean, a fixed-size number, a date, a timestamp or ASCII text of its column's
type (`storage::row::stored_tuple_len`; debug builds check each such size against the decoded
row's, and `tests/big_tables.rs` `measured_sizes_from_stored_rows` runs it over each kind), and
plain-column indexes decode only their columns. Index keys sort by an 8-byte prefix first. A
DELETE without BEFORE triggers checks the transaction's own writes once per row, not twice.
The same numbers as before (relpages, reltuples, index pages). On a quiet machine, 3
interleaved runs:

| step | pg | before | batch 2 | batch 3 |
|---|---|---|---|---|
| COPY in | 0.75 | 4.52 | 2.42 | 2.51 |
| CREATE INDEX | 0.29 | 2.23 | 2.09 | 2.13 |
| CREATE UNIQUE INDEX | 0.19 | 1.86 | 1.82 | 1.88 |
| COPY out | 0.40 | 1.48 | 0.54 | 0.54 |
| INSERT … SELECT | 0.60 | 2.88 | 2.02 | 2.04 |
| UPDATE half | 4.05 | 3.15 | 2.15 | 2.11 |
| DELETE a quarter | 0.19 | 2.05 | 1.88 | 1.64 |
| VACUUM | 0.22 | 0.77 | 0.61 | 0.66 |
| ANALYZE | 0.10 | 1.13 | 0.94 | 0.90 |
| count(*) after | 0.04 | 0.10 | 0.13 | 0.10 |
| peak memory | 351 MB | 847 MB | 729 MB | 788 MB |

What the measurement still costs is the scan itself and, per index, a key per row encoded and
sorted (the build already did both: sharing them is the next step).

Verified (batch 3): every crate test binary at the default and with `ROLL_TXN_SPILL_BYTES=4096`
(27 of 27 each), clippy adds no warnings, the regression suite in full on a private reference
against the committed `results.tsv`: 51,148 / 51,150, no unit changed.

## Recursive functions and deep views (round 20, agent 82)

HANDOFF.md, "Deep input", has the design. These are release times on the shared machine (`q`,
one statement each, the engine's start included):

| `x + x + … + x`, terms | 1,000 | 2,000 | 4,000 |
|---|---|---|---|
| SELECT, before | 106 ms | 343 ms | 1,283 ms |
| SELECT, now | 40 ms | 66 ms | 119 ms |
| CREATE VIEW, before | 26 s | 4.5 min (agent 63) | — |
| CREATE VIEW, now | 200 ms | 549 ms | 2,509 ms |

- The binder was quadratic: a binary operator copied both operands for the user-operator
  lookup, and every subexpression rechecked its collations over its subtree.
- CREATE VIEW was cubic: nodetree asked every node for its collation, and `collation_at`
  copied each subtree it visited.
- What was left was quadratic in a dependency (fixed in round 21, "Native deparser" below).
  The view's text is deparsed through
  `pg_query::deparse`, which encodes the tree as protobuf with prost, and prost's
  `encoded_len` measures each nested message's whole subtree again at every level. At 4,000
  terms, 80% of the time is there. 4,875 terms is the most the depth limit allows.
- A recursive SQL function's call level takes about 70 kB of stack in a release build. Each
  call adds 128 kB to the stack budget, so PostgreSQL's 8,422 levels touch about 280 MB of
  stack segments. They are unmapped when the statement ends.

## Custom plans (round 21, agent 93)

HANDOFF.md, "Custom plans", has the design. Release builds over the wire (pgbench in a
container on the host network, scale 2, one client unless noted), base `b6659eb9` against
the change, alternated three times on a loaded shared machine. Wall-clock tps moved ±20%
between runs, so the measure is the engine process's CPU time for a fixed number of
transactions (5,000; `/proc/<pid>/stat` utime + stime, in ticks), median of three:

| workload | base | custom plans |
|---|---|---|
| `-M prepared -S` | 82 | 73 |
| `-M prepared -S`, 4 clients | 249 | 232 |
| `-M prepared` TPC-B | 294 | 275 |
| PL/pgSQL loop of 10 `SELECT ... INTO` by a variable, prepared | 203 | 176 |
| the same, simple protocol | 239 | 208 |
| `sum(f(g))` over 100 calls of a SQL function with a FROM | 970 | 845 |
| one call of a SQL function with a FROM | 153 | 153 |

- No long-session workload got slower: after five custom plans a session takes the generic
  plan (bound with the statement, or PL/pgSQL's cached one), and the choice per call is a
  hash lookup and a comparison.
- A new session plans each statement again for its first five executions, as PostgreSQL
  does. With a connection per transaction (`-C`, 1,000 transactions) the PL/pgSQL loop took
  183 ticks before and 343 after, the 100 SQL function calls 359 and 447: a session that runs
  a statement only a few times pays up to five more bindings of it. PostgreSQL itself
  managed 47 and 42 tps there (SCRAM authentication and a backend per connection), the engine
  250 and 210; with long sessions PostgreSQL did 2,529 tps on the PL/pgSQL loop and 420 on the
  SQL function calls, the engine about 2,600 and 560.
- Function and PL/pgSQL plans are costed by `explain::estimate`, not through EXPLAIN's JSON
  text as prepared statements still are: that took the `-C` PL/pgSQL loop from about 405
  ticks to 343.

## Round 20 (agent 85): write path round 4

Profiled first (frame-pointer release build, `perf record --call-graph fp` of the engine during
one statement at a time, stacks folded and grouped by caller). Shares of the engine's samples
at `ea038e8e`, the round's base:

- COPY in: the insert path 58% (`Txn::put` 20%: the write buffer's map insert, its journal
  entry and spills; the heap model 15%: per-row sizing and, as the load passes the model's size,
  the waiting rows placed to estimate the heap; pg_stat_io 4.3%, a global mutex and two hash
  lookups per row), parsing 16%, the commit 12%, L0 → L1 compaction in the background 11%.
- CREATE INDEX: writing the entries as runs 34% (their sort 11.5%), measuring the table again
  26%, the scan 15%, the commit 12%.
- DELETE a quarter: the three tombstones per row (row, two index entries) 27%, the scan 18%
  plus its workers, row locks 13%.
- ANALYZE: measuring every row and index key 71% (the keys' sort 14%), sampling 27%.

What changed:

1. **CREATE INDEX measures in its own pass** (`Ddl::measures_in_build`, `measure_rows_with`):
   for a btree of a table too big to model, the build's scan is the measuring scan; each row's
   key values are computed once and handed to the build (unique check, entry). The table and
   the index get the same numbers as a separate measure (`tests/big_tables.rs`
   `index_build_measures_as_analyze`: plain, INCLUDE, expression, partial, unique).
2. **Index entries sort by 16-byte words** (`txn::sort_by_key_after`): the bytes after the
   index's prefix as a `u128`, with the entries' places, ties by the whole key; then the entries
   move into that order. The build's sort went from 11.5% to 5% of its samples.
3. **DELETE's index entries go in at the statement's end** (`Txn::defer_entry_delete`,
   `DeleteNode::defers_entries`): a DELETE without BEFORE ROW triggers, RETURNING or anything
   volatile collects its index entry keys and writes them sorted as one run of the
   transaction's spilled writes (when the transaction has written nothing else in those
   indexes; else, and under a savepoint, as before). Nothing the statement runs can read them
   before it ends (its snapshot doesn't see its writes); the next statement, AFTER triggers and
   the commit see them. Snapshot reads of the runs now go by the snapshot's journal mark
   instead of the newest version (`snap_get`, `scan_snap`, `scan_rev`), so a cursor opened
   before the DELETE doesn't see a run written after it. Tested by `deferred_entry_deletes`
   and `index_only_scans_from_entries` (the cursor case). Postgres sets xmax and leaves the
   entries; leaving them in the engine would need every reader of index entries (unique
   checks, index-only scans from the entries) to check the row: not done.
4. **pg_stat_io counts combine** (`pgstat::count_io`): consecutive counts of one key add up
   in the backend and go into the backend's and the shared counts together, when another key
   is counted, before pg_stat_io / pg_stat_get_backend_io / a reset reads them, and at the
   transaction's end (pgstat_report_stat's point). COPY's per-row 4.3% went to 0.6%.
5. **Estimates kept across restarts** (`sizing::store_estimate`, `persist_estimates`): a table
   becoming too big to model, and every such table when its database closes, has its
   estimated heap pages stored (outside transactions, with its relfilenode). Its first use
   after a restart, never measured by VACUUM / ANALYZE / CREATE INDEX, takes that instead of
   reading the table: the first EXPLAIN of an unanalyzed 1M-row table after a restart 0.73 s
   → 0.12 s (the same as a warm one), with the plan the engine showed before the restart
   (`estimate_kept_across_restart`). (The read estimated the rows past the first 400,000 by
   their stored lengths, so its pages differed slightly from the warm engine's.)
6. The spill keeps its journal's room (`journal.clear()`).
7. **A modelled heap keeps its pages across a restart** (`heapsim::restored`): its first model
   after the database opens puts each row back on its stored block instead of packing the rows
   afresh, so the planner's page counts don't change with a restart (agent 78 found TPC-H
   customer coming back with 3,588 pages for 3,640, which changed q13's plan; not re-measured
   at SF1 here: `heap_pages_kept_across_restart` checks a table with rows deleted mid-heap).

Tried and dropped: sorting ANALYZE's measured keys by their prefixes apart from the keys
(their grouping then reads the keys out of order; slower than the in-place sort).

### Bulk, 1M rows

The machine was loaded through the round (load 30–65 on 36 cores, from other agents' builds
and test runs), so wall times are about twice agent 74's; the CPU column is the server's CPU
time over the statement (the engine process's, the PostgreSQL container's cgroup), which the
load moves less. Medians of 3 interleaved runs (PostgreSQL, base, this round; each engine
from a fresh data directory), the bulk benchmark's steps run through psql (`\copy` of the same
1M-row file), servers on cores 16–19, seconds:

| step | pg wall | pg cpu | base wall | base cpu | round 4 wall | round 4 cpu |
|---|---|---|---|---|---|---|
| COPY in | 1.56 | 1.48 | 4.50 | 4.29 | 4.73 | 4.47 |
| CREATE INDEX | 0.81 | 1.18 | 3.79 | 3.72 | 3.21 | 3.13 |
| CREATE UNIQUE INDEX | 0.56 | 0.67 | 3.53 | 4.32 | 3.14 | 3.99 |
| COPY out | 0.92 | 0.71 | 1.10 | 1.40 | 1.30 | 1.73 |
| INSERT … SELECT | 1.37 | 1.23 | 4.05 | 4.44 | 4.40 | 4.19 |
| UPDATE half | 6.92 | 6.85 | 3.61 | 4.14 | 3.64 | 4.19 |
| DELETE a quarter | 0.50 | 0.40 | 3.43 | 5.34 | 2.38 | 5.10 |
| VACUUM | 0.52 | 0.56 | 1.27 | 1.14 | 1.26 | 1.77 |
| ANALYZE | 0.30 | 0.20 | 1.86 | 1.77 | 1.49 | 1.35 |
| count(*) after | 0.19 | 0.16 | 0.31 | 0.53 | 0.42 | 0.49 |
| DROP TABLE (both) | 0.19 | 0.09 | 7.20 | 6.76 | 6.23 | 6.10 |

CREATE INDEX 15%, DELETE 30% (wall; its CPU is mostly the parallel scan's workers), ANALYZE
20% faster; COPY, INSERT … SELECT, COPY out, UPDATE and VACUUM within the load's noise. Found:
`DROP TABLE` of the two 1M-row tables takes 6–7 s (PostgreSQL 0.2 s): it isn't in the bulk
benchmark's timed steps, and is the biggest single gap the run showed.

Again after the rebase onto `d8facea5`, on a quieter machine (load about 20), the same method
with resources added: **up** is `d8facea5` alone, **round 4** is it with this round's commits.
Peak memory is the engine's VmHWM (PostgreSQL: its container's memory.peak, shared_buffers
2 GB included); on disk is the data directory after the load (and a CHECKPOINT) and at the
end (PostgreSQL's includes its WAL); written is what the server wrote to storage over the load
and over the whole run (`/proc/<pid>/io` write_bytes; the container's io.stat), for 49 MB of
COPY data:

| step | pg wall | pg cpu | up wall | up cpu | round 4 wall | round 4 cpu |
|---|---|---|---|---|---|---|
| COPY in | 1.55 | 1.48 | 3.92 | 3.78 | 3.45 | 3.27 |
| CREATE INDEX | 0.66 | 1.11 | 3.63 | 3.46 | 2.25 | 2.18 |
| CREATE UNIQUE INDEX | 0.45 | 0.60 | 2.65 | 3.12 | 2.31 | 2.75 |
| COPY out | 1.02 | 0.81 | 0.74 | 1.26 | 0.95 | 1.49 |
| INSERT … SELECT | 1.14 | 1.03 | 2.93 | 3.24 | 2.81 | 3.07 |
| UPDATE half | 6.38 | 6.40 | 3.34 | 3.91 | 2.95 | 3.53 |
| DELETE a quarter | 0.42 | 0.33 | 2.25 | 4.85 | 2.06 | 4.79 |
| VACUUM | 0.42 | 0.47 | 0.98 | 0.89 | 1.18 | 1.15 |
| ANALYZE | 0.25 | 0.17 | 1.14 | 1.04 | 1.30 | 1.19 |
| count(*) after | 0.14 | 0.13 | 0.29 | 0.42 | 0.32 | 0.44 |
| DROP TABLE (both) | 0.17 | 0.07 | 4.93 | 4.85 | 5.28 | 5.17 |

| resources | pg | up | round 4 |
|---|---|---|---|
| peak memory | 952 MB | 420 MB | 425 MB |
| on disk after the load | 1,099 MB | 58 MB | 58 MB |
| written by the load | 165 MB | 100 MB | 100 MB |
| on disk at the end | 1,097 MB | 64 MB | 64 MB |
| written over the run | 884 MB | 567 MB | 570 MB |

CREATE INDEX 1.6x, COPY in 12%, UPDATE 12% faster here; ANALYZE, VACUUM and COPY out moved
the other way by about as much as the first run moved them this way (none of their code paths
changed but the snapshot reads, which take the same path when there are no runs): the load's
noise. Memory, disk and writes are unchanged: the load writes 2.0x the COPY data (the commit's
SST, then L0 → L1 compaction compressing it to 58 MB), PostgreSQL 3.4x (WAL and heap, 1.1 GB
on disk with its WAL), and the whole run writes 1.5x less than PostgreSQL's.

Reads (no regression): the OLTP and 200k analytic suites (`bench oltp`, `bench analytic`),
4 interleaved runs each, both orders: every OLTP read within ±10% (point selects 0.99–1.08x,
range scans 0.99–1.09x, secondary lookups 1.01–1.11x); the analytic queries' wall medians
moved ±12% with spreads of 2x between runs, so their engine CPU was measured over the whole
suite (5 reps, 3 interleaved runs): 9.75 s up, 9.66 s round 4.

### pgbench tpcb-like: the starting point (agent 85, at `624d619e`)

Scale 10, `-M prepared`, 30 s per client count, release build, servers on cores 16–19 and
pgbench on 20–23, load 30–38 on the machine. CPU per transaction is the server's CPU time over
the run divided by the transactions (PostgreSQL: its container's cgroup, every backend and
background process); peak memory is the engine's VmHWM, PostgreSQL's container memory.peak
(shared_buffers 2 GB, touched as used):

| clients | pg tps | pg CPU ms / txn | pg peak | engine tps | engine CPU ms / txn | engine peak |
|---|---|---|---|---|---|---|
| 1 | 1,187 | 0.43 | 598 MB | 865 | 0.92 | 403 MB |
| 8 | 5,074 | 0.45 | 695 MB | 3,143 | 0.94 | 403 MB |
| 32 | 6,068 | 0.59 | 837 MB | 3,062 | 1.16 | 473 MB |

The engine spends 2.1x PostgreSQL's CPU per transaction. Its profile at 8 clients (`perf
record --call-graph fp`, shares of the engine's samples):

- **The UPDATEs' scans, 32%.** pgbench_branches (10 rows) and pgbench_tellers (100) are
  updated by sequential scans, as PostgreSQL plans them (one page each). Their rows are hot:
  every transaction writes a new version, and the store keeps the versions (memtable, then
  L0 files) until a flush and compaction drop them. The scan's merge over the sources and the
  seeks past each key's older versions are 14% (`Scan::next`, `Merge::min`), the heap-order
  bookkeeping for the modelled heap (`heap_order`, `row_tids`, `heap_modelled`) another 6%,
  decoding 3%. The accounts UPDATE's index scan is 3%. Directions: a seq scan of a small table
  that answers only one row by a unique equality could read it by its index and still count
  as a seq scan (seq_scan, seq_tup_read and SIRead relation locks as Postgres); cheaper
  version skipping for hot keys (the memtable could keep a key's newest committed version
  apart, or GC versions older than the oldest snapshot on insert).
- **Socket writes, 12%** (`ready_for_query` → `write_all`): a write per ReadyForQuery, one per
  statement (seven per transaction: BEGIN, five statements, END); reads 5%.
- **Bind, 5%**: `describe_here` re-describes the prepared statement on every Bind (2.2% in
  clones of what it describes): cacheable per statement and catalog version.
- **Plan opening, 5%** (`open_update` 3.3%): the UPDATE node set up per execution.
- **Commit, 5%**: the WAL fsync wait 2.2%, memtable inserts 0.9%.
- RLS checks on update (`update_row_rls`) 2.7%, row locks 1.8%, permission checks 1.3%.

Not optimized in this round: the numbers are the next round's baseline.

## Native deparser (round 21, agent 93)

`ast::deparse_expr` / `deparse_stmt` (views, rules, defaults, checks, index expressions,
function bodies, ...) now write their text with `sql/deparse`, a port of libpg_query's
deparser, instead of `pg_query::deparse` and its protobuf encoding. CREATE VIEW over
`x + x + … + x`, release, in-process (`tests/deep_input.rs`'s `viewdef` shape, the
statement alone), base 31bb2756 against this:

| terms | 1,000 | 2,000 | 4,000 | 4,875 |
|---|---|---|---|---|
| before | 198 ms | 598 ms | 2.7 s, then fails | 5.1 s, then fails |
| now | 85 ms | 146 ms | 295 ms | 373 ms |

- "Fails": libpg_query's protobuf decoder stops at 10,000 nested messages, so somewhere
  between 2,000 and 4,000 terms CREATE VIEW ended in `XX000 deparse: Invalid statement:
  could not parse protobuf`. Those views are created now (PostgreSQL takes 4,032 terms).
- What is left is linear: about 75 µs a term, mostly binding and the stored text's parse.
- Reading such a view's text back (`ast::parse_stmt` / `parse_exprs`: `pg_query::parse`
  outside `sql::parse`) had no stack for decoding the tree and overflowed a 2 MB thread at
  4,875 terms: it runs with `tree_headroom` now (`ast::stored_parse`; test
  `deparse_identity.rs deep_view_on_2mb_thread`).
- Identity with libpg_query's text: `tests/deparse_identity.rs` (the goldens, a fixed corpus,
  and PostgreSQL's regress SQL with `DEPARSE_REGRESS_SQL`): 43,000 statements and 225,000
  expressions, no difference, nothing left to libpg_query. `ROLL_DEPARSE_CHECK=<file>` makes
  every call compare the two and log differences and fallbacks there.

## Index access (round 22, agent 103)

What changed (HANDOFF "Index access at index cost"): btree scans read only the entries they
return (backward scans in descending key order, LP_DEAD marking as rows come out, EXISTS
initplans stop at their first row); GiST, SP-GiST, BRIN and pg_trgm indexes are searched
through structures derived from the table instead of reading every row, and so are
exclusion checks without a leading `=`.

Method: `tools/bench` workload `access` (`access-load`: a 1M-row `items` table with its
pkey and `(cat, id)`, 10,000-job que / Oban / graphile-worker / solid_queue tables with
their fetch indexes, 100,000 GiST boxes, 100,000 md5 words under GIN and GiST trigram
indexes, a 1M-row time series under BRIN, two exclusion-constrained booking tables). Release
builds, the server on 4 CPUs and the client on 4 others, the reference PostgreSQL 18.6 with
`shared_buffers=2GB`, the engine `--cache-mb 2048`. Rounds interleaved (reference, before,
after, after, before, reference), 100 queries per case after 5 warm-up ones, simple query
protocol; ms is the median latency, cpu the server's CPU per query, MB the server's peak
memory during the case (the engine's RSS; the reference container's anon + shmem). A
shared machine (load 20-40): about 15% between rounds. "before" is `043e8c2d`, "after" the
same with this round's change. Bulk rows: one statement inserting 3,000 bookings
(`booking_bulk`, no `=` column) and a PL/pgSQL loop inserting 3,000 one by one in one
transaction (`booking_loop`, `room WITH =`), ms per run.

| case | PostgreSQL ms (cpu, MB) | before | after |
|---|---|---|---|
| que fetch (`LIMIT 1 FOR UPDATE SKIP LOCKED`) | 0.436 (0.33, 166) | 0.829 (0.80, 74) | 0.446 (0.40, 74) |
| Oban fetch (`UPDATE ... IN (SELECT ... LIMIT 10 SKIP LOCKED)`) | 0.837 (0.71, 166) | 4.348 (4.05, 88) | 2.120 (2.15, 82) |
| graphile-worker fetch (CTE + UPDATE) | 0.651 (0.56, 166) | 2.800 (2.65, 85) | 1.719 (1.75, 87) |
| solid_queue fetch | 0.370 (0.26, 166) | 0.907 (0.80, 92) | 0.403 (0.40, 94) |
| keyset page `id > k ORDER BY id LIMIT 20` | 0.418 (0.23, 166) | 0.773 (0.65, 96) | 0.435 (0.40, 99) |
| keyset page backward `id < k ORDER BY id DESC LIMIT 20` | 0.302 (0.19, 166) | 827.9 (836, 440) | 0.584 (0.55, 101) |
| `ORDER BY id DESC LIMIT 10`, 1M rows | 0.203 (0.12, 166) | 695.5 (1786, 480) | 0.368 (0.30, 101) |
| `max(id)`, 1M rows | 0.245 (0.17, 166) | 286.0 (578, 481) | 0.545 (0.60, 101) |
| `cat = c ORDER BY id LIMIT 5` | 0.323 (0.20, 166) | 0.532 (0.60, 470) | 0.402 (0.45, 105) |
| `id = ANY (5 values)` | 0.427 (0.24, 166) | 0.912 (0.90, 389) | 0.613 (0.60, 112) |
| nested loop, 100 pkey lookups | 0.746 (0.68, 166) | 3.099 (2.85, 389) | 1.477 (1.60, 116) |
| `EXISTS (... cat = c AND id > 10)` | 0.307 (0.26, 166) | 2.207 (2.10, 389) | 0.540 (0.45, 117) |
| GiST box `&&` (100,000 boxes) | 0.265 (0.20, 166) | 237.2 (210, 295) | 0.748 (0.85, 170) |
| pg_trgm GIN `LIKE '%xxxxx%'` | 0.890 (0.56, 166) | 129.9 (127, 319) | 17.04 (19.2, 218) |
| pg_trgm GiST `LIKE '%xxxxx%'` | 14.60 (14.0, 166) | 65.45 (64.3, 340) | 0.901 (0.80, 254) |
| pg_trgm GIN `%` similarity | 6.591 (6.75, 167) | 448.3 (468, 340) | 388.6 (402, 259) |
| BRIN one hour of a 1M-row series | 3.324 (3.54, 166) | 342.7 (551, 361) | 8.025 (8.45, 286) |
| booking insert, `room WITH =, during WITH &&` | 0.263 (0.20, 167) | 0.682 (0.65, 373) | 0.515 (0.50, 294) |
| booking insert, `during WITH &&` | 0.271 (0.20, 167) | 1.570 (1.50, 375) | 0.507 (0.55, 296) |
| 3,000 bookings in one INSERT (`during WITH &&`) | 91.2 (93, 167) | 14,841 (17,860, 358) | 612 (615, 288) |
| 3,000 bookings in a loop, one transaction (`room WITH =`) | 101.8 (102, 168) | 1,477 (1,978, 442) | 1,401 (1,783, 341) |

The single bookings were inserted into tables of 200 rows; their cost grew with the table
before (an exclusion check read every row), as the bulk insert shows. Peak memory falls
where the old scans read whole tables (440-480 MB before for the backward scans, about
100 MB after). The engine's RSS grows through the run as it caches the tables and builds
structures (cases run in table order: 74 MB at the first, 296 MB at the last, against the
reference's 166 MB); the structures are estimates of about 70 bytes per row placed one by
one (GiST, SP-GiST; trigram posting lists 4 bytes per distinct trigram of a row) and a few
bytes per 128 rows for BRIN zones. Not measured separately.

Left, and why: pg_trgm `%` similarity still reads every row (it would need
`pg_trgm.similarity_threshold`'s bound on shared trigrams); the GIN `LIKE` case's remaining
17 ms is the GIN size model the planner and the bitmap scan still build (cached per
snapshot for page counts only); the job-queue fetches that UPDATE (Oban, graphile-worker)
and the nested loop spend their time outside the scans (the UPDATE's row versions and index
entries, a scan's open per outer row); the loop of single bookings is PL/pgSQL and insert
cost, not the check (its leading `=` already read only the room's rows).

pgbench (scale 10, 8 clients, 20 s, interleaved before / after / before / after twice,
release builds): select-only 19,634 / 18,048 / 18,832 / 17,142 then 16,286 (after) /
18,871 (before) / 16,228 / 17,140 tps; tpcb-like 2,409 / 1,758 / 1,545 / 1,632 then 1,742 /
1,482 / 1,677 / 2,334. A profile of each under select-only (perf, 10 s, 62,000 samples:
19,752 tps after, 18,791 before in that run) shows the same functions in the same shares; the
new code (`Kills`) is 0.08% where `index_scanned` was 0.13%. The spread is the shared
machine's. Peak RSS 369-388 MB select-only, both builds.

## XML: the tree, input checks and building (round 21, agent 107)

`tools/bench/xml.sh` measures every XML entry point against the reference on one document
shape at five sizes (1 KB, 100 KB, 1 MB, 10 MB, 50 MB: `<rows>` of `<row id="i"><a>md5</a>
<b>i</b><c>x &amp; y</c></row>` lines, one single-row table per size; the building cases
over `generate_series`). Each case runs in a fresh connection and reads its data once first.
Peaks: the server process's peak RSS reset (`/proc/<pid>/clear_refs`) before the statement,
the peak over the RSS before it; for the engine a fresh engine per case with
`MIMALLOC_PURGE_DELAY=0`, so freed memory leaves RSS and an earlier statement's pages don't
hide a later one's peak. Times: the minimum of 5 runs (2 at 10 MB, 1 at 50 MB), client-side,
the engine running for a while. Release builds on the same shared, loaded machine, the
reference and the engine interleaved case by case. "Before" is `33a341fe` (its peaks
measured without the purge setting, so small ones are undercounted); "after" is this round's
second batch.

| entry point | 1 MB ms (PG / engine) | 1 MB peak MB (PG / engine) | big | ms (PG / before / after) | peak MB (PG / before / after) |
|---|---|---|---|---|---|
| reading the row (`length(t)`) | 4.9 / 17.1 | 0.9 / 21.9 | 50 MB | 285 / 46 / 81 | 73 / 11 / 102 |
| `t::xml` (xml_in) | 49.8 / 15.8 | 19.5 / 25.8 | 50 MB | 2,997 / 3,744 / 1,103 | 1,001 / 2,957 / 149 |
| XMLPARSE(DOCUMENT) | 43.3 / 67.2 | 15.6 / 22.8 | 50 MB | 3,172 / 3,916 / 1,361 | 816 / 2,779 / 150 |
| XMLPARSE(CONTENT) | 69.3 / 17.0 | 18.1 / 27.4 | 50 MB | 2,452 / 3,933 / 998 | 918 / 2,780 / 151 |
| xml_is_well_formed_document | 49.0 / 44.9 | 17.8 / 23.1 | 50 MB | 3,410 / 3,555 / 1,388 | 890 / 2,955 / 150 |
| `d::text` | 6.8 / 1.7 | 1.1 / 27.2 | 50 MB | 272 / 40 / 118 | 73 / 11 / 150 |
| XMLSERIALIZE(DOCUMENT) | 43.6 / 49.9 | 17.2 / 23.5 | 50 MB | 2,521 / 3,662 / 1,023 | 888 / 2,956 / 201 |
| XMLSERIALIZE(... INDENT) | 59.7 / 41.3 | 19.6 / 27.2 | 50 MB | 4,239 / 3,798 / 2,207 | 988 / 2,856 / 550 |
| xmlconcat(d, d) | 18.3 / 28.4 | 5.6 / 22.7 | 50 MB | 791 / 133 / 296 | 303 / 105 / 304 |
| xpath() of 11,600 text nodes | 58.3 / 32.6 | 19.3 / 27.0 | 50 MB | 4,059 / 4,599 / 2,669 | 976 / 3,071 / 576 |
| xpath('count(...)') | 69.6 / 80.0 | 18.2 / 24.8 | 50 MB | 2,400 / 3,751 / 1,892 | 894 / 3,075 / 577 |
| xpath_exists | 79.2 / 26.3 | 18.3 / 26.7 | 50 MB | 2,896 / 4,280 / 1,613 | 893 / 3,070 / 576 |
| XMLTABLE, 3 scalar columns | 71.9 / 74.0 | 19.6 / 24.2 | 50 MB | 4,159 / 5,532 / 2,704 | 948 / 3,120 / 698 |
| XMLTABLE, an xml column (the row) | 181 / 61.4 | 19.7 / 25.6 | 50 MB | 7,779 / 7,593 / 3,829 | 947 / 3,119 / 688 |
| xmlelement(xmlagg(xmlelement(xmlattributes, xmlforest))) | 7,629 / 35.6 | 2.9 / 0.2 | 1 MB | (the reference's xmlagg is quadratic; before 835) | |
| xmlagg(xmlelement) | 4,255 / 23.5 | 2.6 / 1.5 | 1 MB | (before 460) | |
| string_agg(format(...)), for comparison | 21.9 / 27.9 | 3.6 / 1.0 | 50 MB | 1,147 / – / 2,071 | 168 / – / 166 |
| query_to_xml over generate_series | 63.0 / 29.2 | 73.1 / 2.5 | 50 MB | 3,082 / – / 1,927 | 3,648 / – / 168 |
| table_to_xml | 60.2 / 72.8 | 82.7 / 0.1 | 50 MB | 3,522 / 3,260 / 1,270 | 4,226 / 337 / 172 |
| xml2 xpath_string | 41.5 / 23.7 | 18.1 / 26.3 | 50 MB | 2,118 / 3,571 / 1,430 | 845 / 3,070 / 574 |
| xml2 xpath_nodeset | 62.4 / 67.1 | 19.7 / 25.6 | 50 MB | 3,247 / 4,358 / 2,103 | 894 / 3,069 / 580 |
| xml2 xpath_list | 51.8 / 46.8 | 19.1 / 27.1 | 50 MB | 3,090 / 4,095 / 1,957 | 862 / 3,068 / 576 |
| xml2 xpath_number | 42.0 / 85.2 | 19.1 / 26.1 | 50 MB | 2,414 / 3,877 / 1,683 | 845 / 3,062 / 576 |
| xml2 xpath_bool | 85.9 / 37.6 | 18.5 / 28.6 | 50 MB | 3,552 / 4,079 / 1,566 | 845 / 3,073 / 576 |
| xml2 xpath_table | 65,534 / 51.9 | 2,258 / 30.5 | 1 MB | (before 110,772) | |
| xslt_process, identity | 138 / 115 | 36.9 / 29.4 | 50 MB | 7,147 / 7,931 / 5,229 | 1,740 / 4,976 / 1,010 |

At 1 MB every engine number carries the first row: reading a row with two 1 MB values costs
the engine 10–45 ms and ~22 MB the first time in a connection (`length(t)` above; PostgreSQL
5 ms, 1 MB), and the times spread by tens of ms run to run. The XML work itself is in the heap
peaks at 1 MB of `tests/xml_memory.rs` (a counting allocator, heap bytes; it fails if one
goes over the reference's RSS growth for the same statement): xml_in and XMLPARSE 2.9 MB
(PostgreSQL 22.6 / 17.3), XMLSERIALIZE 3.9 (20.0), with INDENT 12.8 (21.8), xpath /
xpath_exists / the xml2 functions 11.6 (20.5), XMLTABLE 13.8 (20.5), xslt_process 20.5
(57.0), the xmlagg build 3.2 (5.0). Before this round, parsing that document took about
200 MB (agent 98).

What changed:
- **The tree** (`types/xml/tree.rs`). A node was ~300 bytes plus a heap string for its name,
  one for its content, a `Vec` for its children and one for its attributes, and two strings
  per namespace. It is 72 bytes and allocates nothing: libxml's parent / first / last /
  next / prev links and the first attribute as `u32` indexes into the arena (the sibling
  positions XPath kept in side vectors are gone), names shared through an interner (`Name`,
  an `Arc<str>`), namespaces as shared `xmlNs` (`NsRef`; an element's namespace is one of
  its ancestors' declarations, as in libxml), content as a span of the document's one text
  buffer (`Doc::text`, `push_text`, `set_text`), and namespace declarations / DTD
  declarations boxed out of line. The arena is chunks of 4,096 nodes (`Nodes`), so a big
  document never copies its nodes into an allocation twice their size (17 → 11.6 MB at
  1 MB). Copies within a document (`Doc::dup`, `copy_node`) and across documents
  (`Doc::import`, `absorb` for XSLT's result tree fragments) are explicit, since a span means
  nothing in another document. Deep copies and text content no longer recurse.
- **Checking without a tree** (`Parser::tree = false`): xml_in, XMLPARSE, the
  xml_is_well_formed functions, xml_recv, XMLSERIALIZE without INDENT and IS DOCUMENT parse
  without building content nodes (the DTD's declarations are still kept: they change how the
  rest parses). Errors, warnings and their positions come from the parse, so they are the
  same; the parser's open-element stack keeps its depth.
- **xmlagg** (`xml::concat_into`): each row's xmlconcat2 appended in place unless the merged
  declaration changes, instead of copying everything aggregated so far (quadratic, like the
  reference's own: 1 MB takes it 7.6 s).
- **xml2's xpath_table** evaluates each path once per document; the reference evaluates every
  path again for each row it returns (quadratic: 65 s for 1 MB, 52 ms now). The values and
  the NOTICEs per row are the same.
- The parser's character data goes straight from the input buffer to the tree (it was copied
  twice on the way).

Left, measured but not XML:
- Reading a row with large values (above) and the memory of it: the 50 MB rows' `d::text`
  and `length(t)` peak at 150 / 102 MB against 73, which puts `d::text` over the reference
  (the cast copies the text too). A scan decodes every column of the row, needed or not
  ("Where it is still slower" item 1). Reading one row of a table of all five documents
  raised the engine's RSS by ~380 MB, so the bench keeps one document per table. A row read
  from a table written before the engine (re)started was 40–50 ms where a table made since
  read in 1 ms, the first time in each connection.
- Index range reads over the 580,000-row `xb_rows` (two int, one text column, made by CREATE
  TABLE AS and a later primary key): `id <= 11` takes 190 ms in its Aggregate after a 0.2 ms
  index scan and 45 ms to plan, `id <= 116000` 4.6 s against 25 ms; a count of
  `xb_t_10m` (116,000 such rows) took 1.6 s against 7 ms. table_to_xml at 10 MB pays that.
- string_agg(format(...)) building the same document as text is 1.8x slower at 50 MB.

## Logical replication apply (round 21, agent 108)

One PostgreSQL 18.6 publisher stream (`a108-pub`) of 60,000 pgbench-like transactions (UPDATE
of a 10,000-row table by key, INSERT into an indexed history table, every third a DELETE of
its oldest row; 4 pgbench clients) made with both subscriptions disabled, then each enabled
alone and timed until its slot confirmed the end. Release build, fsync on both sides,
default synchronous_commit of a subscription (off: PostgreSQL's apply doesn't wait for its
WAL flush; the engine still does, see below). Both end identical (row counts, sums, md5 of
every row). Machine at load 25-40; rounds interleaved.

| subscriber | text | binary = true | apply worker RSS |
|---|---|---|---|
| PostgreSQL 18.6 | 3.5-3.8 s (16-17k tx/s) | 4.0-5.2 s (loaded rounds) | 34 MB (+ shared buffers) |
| engine, base adda959c | 9.0-9.3 s (6.5k tx/s) | (values came as text) | whole process 69 → 125 MB |
| engine, this round | 7.0-8.1 s (7.4-8.6k tx/s) | about as text | whole process 69 → 125 MB |

- The gain is BEGIN / COMMIT without SQL text per applied transaction.
- The engine's growth is its memtable (the store's, not the worker's); PostgreSQL's worker
  RSS excludes the shared buffers the changes land in.
- Where the engine's apply thread goes (perf, 6 s, dwarf stacks): the WAL fsync of each
  commit 11% (synchronous_commit = off, agent 105's, takes it away), the btree and heap size
  models 11% (`btsim::insert` / `cmp_keys`, `heapsim::locate`), memtable skiplist searches
  ~8%, allocation ~8%, WAL writes 3%; the apply layer itself (message parsing, value input,
  row binding) a few percent. The rest is the engine's ordinary write path, so beating
  PostgreSQL's subscriber needs that path faster; binary values change little because value
  input was never a large share.

## Where it is still slower, and why

In rough order of what it costs users:

1. **Analytic queries and TPC-H (5–20x slower in round 17; 1.7–10x since round 18, which
   did the first steps below: see its section).** The executor pulls one `Row` (a
   `Vec<Value>`, a heap allocation per text value) at a time through boxed nodes, evaluates
   expressions by walking `Expr` trees, aggregates `numeric` as `BigInt`s, and runs every plan
   on one thread (PostgreSQL ran two parallel workers on the same four cores). A scan
   materialises the table to put it in heap order and decodes every column of every row,
   needed or not. The fixes, biggest first: decode only the columns a plan reads (a
   projection mask passed down to scans), a batch (vector) interface between scans, filters,
   projections and aggregates with typed column arrays, `i128` / scaled-integer numeric
   accumulators, and running a plan's Gather over worker threads.
2. **Bulk loads, CREATE INDEX, DELETE, VACUUM, ANALYZE (3–10x slower).** Round 18 (agent 43)
   streamed the index builds, VACUUM, ANALYZE and ALTER TABLE's checks, and ingests big
   commits as SSTs. Round 19 (agent 69) bounded the memory: a transaction's write buffer
   spills to sorted runs on disk, COPY FROM reads its data a batch at a time, and index builds
   write sorted runs (the 1M bulk run's peak 1.75 GB → 0.85 GB). Round 19 (agent 74) defers
   a big statement's heap model, parses and prints COPY data without a copy per field and
   floats by Ryu, and keeps bulk locks compact (COPY in 2x, COPY out 2.5x; a 5M-row INSERT …
   SELECT 2.6 GB → 0.5 GB). What is left:
   - Per-row executor and storage work dominates the time. COPY spends ~2.4 µs a row against
     PostgreSQL's ~0.8: a row goes through `Expr` constants and `InsertNode`'s general path
     (a `Frame` per row, defaults, checks), a `BTreeMap` buffer insert with its journal entry
     (the key copied twice), the sorted run it spills to and the commit's SST it is merged
     into (each with a bloom filter bit set per key, cache misses on a big filter). DELETE
     writes a tombstone per row and per index entry (Postgres sets xmax only) and checks the
     transaction's own writes twice per row.
   - Locks: compact past 2,048 per transaction (`txn/bulklocks.rs`, about 20 bytes plus the
     key). What a big UPDATE or DELETE still holds per row is the write buffer's (spilled)
     and, over a join, the set of rows done.
   - ANALYZE of a table too big to model measures every row for the planner's relpages and
     index pages: the text width of every value and an index sort key per row, then sorted.
     The sample itself is cheap (agent 64 samples by pages). Estimating those from the sample
     would change relpages (TPC-H plans read them; agent 64). CREATE INDEX measures the same
     way (`measure_rows`), repeating its own build's key extraction and sort.
   - An index build writes its entries twice: as sorted runs, then into the commit's SST.
     Linking runs in as L0 files would need a per-file commit timestamp in the SST format.
3. **Index scans over many rows (range scans, secondary lookups: 0.2–0.4x at 8 clients over
   the wire in round 17).** Round 18 (agent 43) batched an index scan's heap fetches and
   answers index-only scans of big tables from the entries (1.2–1.4x in-process). What is
   left is per row: each fetched row is decoded into a `Vec<Value>` and goes through the
   scan node's run and statistics bookkeeping; over the wire the protocol encoding adds as
   much again.
4. **DDL at scale.** Since round 18's agent 44 the catalog is stored per object, and since
   agent 49 its maps are structurally shared with secondary indexes kept on mutation ("Catalog
   and DDL at scale" above): at 10,000 tables CREATE TABLE is 2.4x PostgreSQL's speed, CREATE
   INDEX and COMMENT about even, and restoring an 11,000-table schema dump 2x faster than
   PostgreSQL; ALTER TABLE ADD COLUMN (0.18x) and DROP TABLE (0.25x) are still slower.
5. **Catalog introspection (`\d`, ORMs).** Rows are shared per catalog version and looked up
   through hashes (above): 1.3–2.5 ms for psql's `\d` queries at 500 tables, ORM constraint
   queries ahead of PostgreSQL; the first query after DDL regenerates a catalog whole.
6. **Multi-client writes (0.6–0.8x at 8 clients over the wire, ahead in-process).** CPU per
   statement is about twice PostgreSQL's under concurrency: the remaining profile is flat
   (allocation, protocol, binding, lock-manager waits on hot rows).
7. **First read of a big table after the database opens.** The heap model is decided on
   first use: a table past `MAX_VERSIONS` is read once to estimate its size (before: every
   row held in memory, then dropped; now the first 400k decoded and the rest's stored lengths
   summed, nothing held). Persisting the estimate with the table would remove the read. (The
   in-process first `count(*)` of lineitem took 11.5 s in round 16 (a sequential scan of a
   table the planner thought empty), 37–51 s in round 17 (an index-only scan fetching every
   row) and 6.6–18 s since round 18, agent 43: 1–8 s of it this read, 5.7 s the index-only scan
   from the entries. PostgreSQL plans a parallel index-only scan of `l_shipdate`'s index
   there; the engine's estimate of the big table's indexes picks another, so that EXPLAIN
   still differs.)
8. **Memory per idle session**: 0.5 MB since round 18's agent 44 (PostgreSQL 1.5 MB; it was
   2–3 MB: transparent huge pages under mimalloc's per-thread heaps, and per-session catalog
   rows).

Where the engine is ahead: single-client latency everywhere in OLTP (no process per
connection, no shared-buffer locking, statement plans cached), select-only and TPC-B pgbench
up to 32 clients at scale 1, read-mostly multi-client point selects, connection setup (3x),
on-disk size (2.4x smaller: zstd-compressed SSTs), and, embedded, everything OLTP (no
protocol: 2–27x PostgreSQL over the wire).

## Verification

- `cargo test -p pg-engine` (all of it) and `cargo clippy -p pg-engine --all-targets` (no
  new warnings) after every batch and after the rebase.
- The regression suite in full, with its own reference container, for this round's start
  (`b1917d6`) and its end before the rebase: **51,129 / 51,150 both, identical per file**
  (`tests/regress/REPORT.md` format, compared file by file).
- New tests: `storage::tests::reverse_scan_matches_forward` (descending scans, seeks over
  many versions, pruning under pinned snapshots, against a model of every version written),
  `exec::fasthash::tests::row_keys_spread`, `tests/wire.rs` `text_parameters_of_reg_types`,
  `tests/session_state.rs` `catalog_rows_follow_ddl_roles_and_settings`.
- Every benchmark query's result rows are hashed and compared with PostgreSQL's (above).
- After the final rebase: agent 35's differential tester, `difftest smoke`, passes.
- Storage format: unchanged (no new keys or value layouts); WAL files are zero-filled past
  their last record, which this version's replay stops at. (An older binary replaying a WAL
  this version left after a crash fails on the zeros: downgrades after a crash aren't
  supported. Round 18, agent 43, made that the rule: see its "Durability" section.)
- Round 18, agent 44: `cargo test -p pg-engine` and clippy (no new warnings) on the tree as
  committed; the regression suite in full, each run with a reference cluster of its own
  (HANDOFF, "A private reference per run"), for the base (`44adff0`) and for the change:
  **51,138 / 51,150 both, identical per file and in every differing unit's hunk** (earlier
  bases, `ca0c077`, `6dd2469` and `948d0e8`, against the change at the time: identical too);
  `difftest smoke` passes. New tests: `tests/catalog_storage.rs` (the catalog
  after restarts, rollbacks and savepoints, after a crash replayed from the WAL, a catalog of
  the old format rewritten, the bytes DDL writes not growing with the catalog, and
  a merged concurrent DDL commit stored whole), `tests/session_state.rs`
  `catalog_rows_shared_by_sessions_follow_their_state` (sessions of
  other settings, roles, paths and temporary schemas reading the shared rows),
  `tests/wire.rs` `idle_hook_runs_and_the_connection_goes_on`, golden `catalog_lookups`
  (lookups by name and oid across DDL, rollbacks and savepoints, recorded from PostgreSQL),
  and debug assertions (`SysTable`'s table order, `triggered_tables` against the full list
  of internal triggers).
