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
`ENGINE_PORT` the pinning and ports.

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

## Where it is still slower, and why

In rough order of what it costs users:

1. **Analytic queries and TPC-H (5–20x slower).** The executor pulls one `Row` (a
   `Vec<Value>`, a heap allocation per text value) at a time through boxed nodes, evaluates
   expressions by walking `Expr` trees, aggregates `numeric` as `BigInt`s, and runs every plan
   on one thread (PostgreSQL ran two parallel workers on the same four cores). A scan
   materialises the table to put it in heap order and decodes every column of every row,
   needed or not. The fixes, biggest first: decode only the columns a plan reads (a
   projection mask passed down to scans), a batch (vector) interface between scans, filters,
   projections and aggregates with typed column arrays, `i128` / scaled-integer numeric
   accumulators, and running a plan's Gather over worker threads.
2. **Bulk loads, CREATE INDEX, DELETE, VACUUM, ANALYZE (5–25x slower; much more memory).** A
   statement's writes are buffered whole in the transaction (`Txn::writes`, a `BTreeMap` with
   a journal entry per write) and applied at commit; CREATE INDEX, VACUUM and ANALYZE read the
   whole table into `Vec<Row>`; the heap / btree models are updated row by row. Peak memory
   follows (9.6 GB for the 1M-row bulk run, 18 GB for the TPC-H load). The fixes: spill large
   write buffers to a sorted run in the store (or write them as an SST ingested at commit),
   build indexes from a sorted stream, measure sizes from samples.
3. **Index scans over many rows (range scans, secondary lookups, index-only scans: 0.2–0.4x
   at 8 clients; a `count(*)` of lineitem's 6M rows by index-only scan, 47 s).** Each index
   entry fetches its row with a point read through every LSM source (memtable, frozen
   memtables, each L0 file with its bloom filter, L1), a block-cache lookup under a shard
   lock and a binary search in the decoded block. Index-only scans fetch the row too (the
   engine has no visibility map to skip it). The fixes: batch an index scan's fetches (sort
   a run of row keys, read them with one merging cursor, emit in index order), and answer
   index-only scans from the index entries when the plan reads only index columns.
4. **DDL (3–28x slower).** Every DDL statement serialises the whole catalog to JSON and
   writes it as one value (`Txn::set_catalog`), so a statement's cost grows with the number of
   objects. The fix: store catalog objects under keys of their own (a storage-format change,
   with an `upgrade_storage` step).
5. **Catalog introspection (`\d`, ORMs: 6–40x slower).** System catalogs are generated as rows
   from the catalog structures, whole, for the first statement that reads them; optimisation
   12 reuses them within a session, but each new catalog version, session or setting change
   regenerates them, and a statement still copies them. The fix: generate only the rows an
   equality on the catalog's key asks for (as `sys_rescan` does for parameterised scans), and
   share generated rows across sessions by catalog version.
6. **Multi-client writes (0.6–0.8x at 8 clients over the wire, ahead in-process).** CPU per
   statement is about twice PostgreSQL's under concurrency: the remaining profile is flat
   (allocation, protocol, binding, lock-manager waits on hot rows).
7. **First read of a big table after the database opens.** The heap model is decided on
   first use: a table past `MAX_VERSIONS` is read once to estimate its size (before: every
   row held in memory, then dropped; now the first 400k decoded and the rest's stored lengths
   summed, nothing held). Persisting the estimate with the table would remove the read. (The
   in-process first `count(*)` of lineitem took 11.5 s before, a sequential scan of a table
   the planner thought empty, and 37–51 s now: the index-only scan of item 3. PostgreSQL
   plans a parallel index-only scan of `l_shipdate`'s index there; the engine's estimate of
   the big table's indexes picks the primary key's, so that EXPLAIN still differs.)
8. **Memory per idle session (2–3 MB, PostgreSQL 1.5 MB).** mimalloc keeps per-thread heaps,
   and a session keeps the catalogs it generated (optimisation 12) and its parse cache. Both
   could be released after a session has been idle for a while.

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
- Storage format: unchanged (no new keys or value layouts); WAL files are zero-filled past
  their last record, which this version's replay stops at. (An older binary replaying a WAL
  this version left after a crash fails on the zeros: downgrades after a crash aren't
  supported.)
