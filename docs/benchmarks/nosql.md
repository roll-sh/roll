# roll nosql vs ScyllaDB 2026.3.3 and Cassandra 5.0.9

The pitch is "the same behaviour, zero idle cost, instant branches", not "faster than Scylla"
(`docs/nosql/architecture.md`). These numbers show where roll stands on one machine with the same
CPU budget as the references. They are not a claim about clusters.

Everything below was run by `scripts/nosql-bench.sh` on 2026-10-21.

- **Host:** Intel Core Ultra 5 125H (4 P-cores with 2 threads each, 8 E-cores, 2 LP E-cores), 30 GiB
  RAM, Samsung 990 PRO NVMe with btrfs, Linux 7.2.8 (Arch), Docker 29.8.2. Other containers ran on
  the host, so the 1-minute load average ranged over 0.8–21.6.
- **CPU budget:** two hardware threads on two different P-cores (CPUs 4 and 6).
  - rolld runs under `taskset -c 4,6` (release build, `--nosql-only --nosql-single`, local-directory
    storage under `--home`). No S3 endpoint was available. On S3, cache misses add a range GET and
    commit log segments ship every ~100 ms; neither was measured here.
  - Each reference runs in its own container with `--cpuset-cpus 4,6 --memory 4g`.
    - ScyllaDB: `--smp 2 --memory 2G --overprovisioned 0 --developer-mode 1 --reactor-backend io_uring`.
      io_uring is used because the host's `fs.aio-max-nr` was used up by other ScyllaDB containers.
    - Cassandra: `MAX_HEAP_SIZE=2G HEAP_NEWSIZE=400M`.
  - roll has no memory cap; its RSS is reported.
  - The shared `nosql-ref-*` containers (`--smp 1 --memory 1G`, 640M heap) are deliberately small
    and serve the conformance tests, so they were not used.
- **Load generators:** one at a time, in containers on CPUs 8–15 (E-cores), `--memory 2g`, host
  network.
  - cassandra-stress 5.0.9: Java driver 3, protocol v4, 64 threads, 8 connections, prepared
    statements unless stated.
  - latte 0.54.0-scylladb: the Rust driver 1.9, 2 threads × 32 requests in flight.
  - scylla-bench (gocql, client compression on): 8 requests in flight.
- **Runs:** each workload runs for 30 s after 1M rows are loaded, in the order shown. The 2-CPU table
  is the mean of 2 rounds (roll first in round 1, the references first in round 2); the range is
  over the rounds.

## Workloads

| workload | tool | what it does |
| --- | --- | --- |
| write | cassandra-stress `write n=1000000` | loads 1M rows (10-byte key, 5 × 34-byte columns), sequential keys |
| read | cassandra-stress `read`, uniform over the 1M keys | point reads |
| unprepared | as read, with `-mode unprepared` | QUERY with values instead of EXECUTE |
| mixed | cassandra-stress `mixed ratio(write=1,read=1)` | 50/50 point reads and overwrites |
| lwt | latte, `scripts/nosql-bench/lwt.rn` | `UPDATE … IF EXISTS` on 100k loaded rows (always applied) |
| batch | cassandra-stress user profile `scripts/nosql-bench/batch.yaml` | LOGGED batches of 10 rows over 5 partitions |
| wide | scylla-bench `-mode read -rows-per-request 10000 -no-lower-bound -page-size 5000` | 20 partitions of 10k 100-byte rows; each request reads one whole partition (1 MB, 2 pages) |

## Results: 2 CPUs

Throughput: operations per second (one batch is one op; one wide op is 10k rows). Higher is
better. The range over the rounds is in parentheses.

| workload | roll (scylla dialect) | roll (cassandra dialect) | ScyllaDB | Cassandra |
| --- | ---: | ---: | ---: | ---: |
| write | 85,239 (85,228–85,250) | 87,217 (68,575–105,859) | 63,874 (40,432–87,316) | 28,808 (21,287–36,330) |
| read | 119,650 (111,688–127,611) | 119,017 (106,743–131,291) | 121,906 (110,960–132,852) | 25,844 (21,021–30,667) |
| unprepared | 89,084 (84,747–93,421) | 79,977 (76,091–83,863) | 66,984 (52,696–81,273) | 17,317 (13,167–21,467) |
| mixed | 48,856 (44,064–53,647) | 66,104 (65,564–66,644) | 69,863 (54,289–85,437) | 41,706 (33,928–49,484) |
| lwt | 84,453 (75,629–93,277) | 90,150 (88,222–92,078) | 10,076 (9,201–10,952) | 8,702 (7,636–9,769) |
| batch | 23,506 (23,309–23,703) | 22,812 (22,231–23,394) | 7,082 (6,840–7,324) | 8,340 (7,393–9,288) |
| wide | 215 (205–225) | 183 (159–207) | 194 (154–235) | 126 (98–154) |

Latency in ms, as p50 / p99 / p99.9 (the mean over the rounds). Lower is better.

| workload | roll (scylla dialect) | roll (cassandra dialect) | ScyllaDB | Cassandra |
| --- | ---: | ---: | ---: | ---: |
| write | 0.45 / 4.60 / 9.30 | 0.50 / 4.65 / 9.65 | 0.55 / 5.05 / 56.50 | 1.80 / 9.10 / 56.20 |
| read | 0.45 / 1.25 / 5.20 | 0.45 / 1.45 / 5.40 | 0.45 / 1.40 / 5.40 | 1.85 / 12.95 / 33.40 |
| unprepared | 0.65 / 1.70 / 4.80 | 0.70 / 2.35 / 5.50 | 0.70 / 4.30 / 6.70 | 2.85 / 24.40 / 49.50 |
| mixed | 0.95 / 5.40 / 11.25 | 0.70 / 3.90 / 7.35 | 0.55 / 4.85 / 11.40 | 1.15 / 6.65 / 30.90 |
| lwt | 0.64 / 2.59 / 5.76 | 0.62 / 2.14 / 4.23 | 5.36 / 44.66 / 84.70 | 5.89 / 31.34 / 116.16 |
| batch | 1.75 / 11.40 / 57.60 | 1.90 / 11.00 / 58.05 | 3.95 / 77.55 / 1033.10 | 3.80 / 84.30 / 288.40 |
| wide | 36.44 / 58.56 / 74.91 | 42.91 / 82.84 / 100.83 | 43.60 / 99.45 / 122.13 | 55.07 / 211.81 / 269.03 |

Peak memory during each workload: rolld's RSS, and each container's memory for the references.

| workload | roll (scylla dialect) | roll (cassandra dialect) | ScyllaDB | Cassandra |
| --- | ---: | ---: | ---: | ---: |
| write | 539 MiB | 464 MiB | 1938 MiB | 2456 MiB |
| read | 764 MiB | 822 MiB | 1938 MiB | 2465 MiB |
| unprepared | 730 MiB | 765 MiB | 1938 MiB | 2468 MiB |
| mixed | 1007 MiB | 1012 MiB | 1940 MiB | 2483 MiB |
| lwt | 1224 MiB | 1138 MiB | 1944 MiB | 2509 MiB |
| batch | 2984 MiB | 2656 MiB | 1982 MiB | 2545 MiB |
| wide | 2972 MiB | 2521 MiB | 1981 MiB | 2576 MiB |

The memory columns measure different things:

- ScyllaDB takes its `--memory` up front, and Cassandra pre-touches its heap.
- roll grows with what it holds: memtables (1 GiB budget across databases, 64 MiB per table), a
  256 MiB block cache, and allocator pages that busy threads keep. Every workload adds tables, so
  later rows include the earlier data.
- In the batch and wide rows, roll used more memory than either reference.
- After an unload, roll returns to ~50 MiB (see the cold start section).
- These rows predate the memory budgets. [Memory budgets](#memory-budgets) has the numbers
  with them.

## Results: 1 CPU

This round compares roll with a dedicated ScyllaDB on CPU 4 alone. roll ran under `taskset -c 4`
and ScyllaDB with `--smp 1 --cpuset-cpus 4`. It is one round, so read it as ±15%.

| workload | roll op/s | ScyllaDB op/s | roll p50 / p99 / p99.9 ms | ScyllaDB p50 / p99 / p99.9 ms |
| --- | ---: | ---: | ---: | ---: |
| write | 51,028 | 48,280 | 0.50 / 6.80 / 37.40 | 0.90 / 3.60 / 10.60 |
| read | 84,879 | 110,752 | 0.70 / 1.50 / 4.90 | 0.50 / 1.20 / 4.80 |
| unprepared | 53,955 | 56,544 | 1.10 / 2.40 / 5.20 | 1.10 / 2.30 / 4.70 |
| mixed | 35,755 | 59,656 | 1.10 / 10.20 / 43.80 | 1.00 / 3.20 / 5.40 |
| lwt | 66,384 | 8,701 | 0.78 / 3.84 / 12.38 | 6.80 / 16.84 / 57.93 |
| batch | 12,136 | 6,933 | 4.70 / 16.50 / 76.70 | 5.70 / 106.80 / 488.40 |
| wide | 123 | 168 | 67.11 / 91.36 / 97.98 | 46.56 / 64.26 / 75.50 |

## Scale to zero: cold start

The cold start is the time from `Cluster.connect()` to the first row, measured with the Python
driver 3.30 (`scripts/nosql-bench/cold.py`). It includes the driver's own schema and topology
queries. Storage is local; data blocks are read lazily.

For roll, the database is unloaded through the control API (`POST /v1/nosql/databases/<id>/unload`)
before each sample. The unload flushes memtables and releases memory. The next connection then
loads the manifest and schema. "Warm" is the same connection with the database already loaded.
There were 5 samples per row.

| data | warm (loaded) | unloaded → first row | unload | rolld RSS 20 s after unload |
| --- | ---: | ---: | ---: | ---: |
| one row (3.6 KB of SSTables) | 15–19 ms | 20–24 ms | 6–13 ms | 46 MiB |
| 1 GB (5.4M rows, 1.09 GB of SSTables; the first row of a scan) | 16–22 ms | 29–44 ms | 2.3–6.8 s (the first unload, which flushes the memtables), then 6–8 ms | 58 MiB |

The references cannot unload a database. Their closest equivalent is a process start: the time
from the container's start to the first row, with one row of data and polling every ~1 s.

| reference | warm | process start → first row |
| --- | ---: | ---: |
| ScyllaDB | 17–20 ms | 4.5–6.8 s |
| Cassandra | 49–59 ms | 12.8–14.3 s |

Before the memory budgets, memory came back in two steps after an unload (with them it comes
back within a second, see [Memory budgets](#memory-budgets)):

- Right after the unload, roll's RSS with 1 GB of data is still 1.5–1.7 GB. Flush and compaction ran
  on tokio's blocking-pool threads, and their allocator heaps are only returned when those threads
  exit after tokio's 10 s keep-alive.
- After that, RSS is 46–58 MiB (measured: 2.6 GB at 5 s, 697 MiB at 10 s, 46 MiB at 15 s).

So zero idle cost held after about 15 s, not immediately.

## Memory budgets

The runs above found roll's memory unbounded: 2.7–3 GB in the batch and wide rows, and 1.5–1.7
GB held for ~10 s after an unload. Three things held it:

1. **Allocator heaps of blocking-pool threads.** Memory one thread frees that another thread
   allocated stays in the allocating thread's mimalloc heap. It is returned only when that
   thread allocates in the same size class again, collects, or exits. Local-storage reads
   (object_store's `spawn_blocking`) allocate each GET's buffer on a pool thread, and
   compaction frees it on its own thread. With tokio's 10 s keep-alive the pool threads lived
   on, so a compaction left about 3x its working set pinned, and an unload's memory came back
   only when they exited.
   - Measured in a test with mimalloc: a 44 MB compaction held 25 MB live but grew RSS by
     130 MB. With a 50 ms keep-alive the growth was 30 MB, and the compaction was no slower.
   - Calling `mi_collect` on the compaction thread changed nothing, because the memory was not
     in that thread's heap.
2. **SSTable index keys pinned whole blocks.** An index entry's first key, and the statistics'
   first and last keys and clustering bounds, were `Bytes` slices of the data they were written
   from. For a compaction output, that data is the decoded 64 KiB blocks of its inputs. So
   every live SSTable produced by compaction kept up to one decoded input block per index entry
   alive. The writer now copies them.
3. **Memtables beyond what they were charged for.** The accounting counted about half of the
   real heap: partition and row structures, and the growth slack of the encoded frames, were
   missing. Writes were never held back either: a table's next flush could only start after
   the previous one finished, and the node budget was checked once a second.

The fixes are in `nosql_storage::memory` and `docs/nosql/architecture.md` (Tenancy and
routing):

- Memtable accounting is within 0.9–1.4x of the heap, checked against a counting allocator.
- There is a node budget and a cap per database. Pressure flushes run at half of either. At the
  cap or the fair share, writes wait, and after 2 s they get the references' `WriteTimeout`.
- Flushes and compactions have concurrency limits, and compaction reads ahead within a 16 MiB
  budget.
- Exact-size commit log frames, and a cap on the commit log backlog.
- The tokio blocking pool's keep-alive is 50 ms, and blocks are decoded on the compaction's
  own thread.
- An unload stops compactions at the next partition, flushes in parallel and calls
  `mi_collect(true)`.

`scripts/nosql-bench.sh` with `TARGETS=roll-scylla` (2 CPUs, the default budgets: 512 MiB of
memtables, all of it for the single database, and a 256 MiB block cache) gave these numbers.
There was one round per binary on the same host, and the workloads are cumulative as above.
The table shows peak RSS in MiB, and op/s in parentheses.

| workload | before | with budgets |
| --- | ---: | ---: |
| write | 400 (62,738) | 164 (96,279) |
| read | 732 (76,575) | 533 (137,550) |
| unprepared | 662 (42,201) | 515 (99,021) |
| mixed | 926 (44,305) | 777 (51,951) |
| lwt | 953 | 392 |
| batch | 1495 (17,294) | 551 (22,736) |
| wide | 1438 (229) | 416 (217) |

- The read and mixed rows now sit at about the budgets: up to 512 MiB of memtables, plus the
  256 MiB cache, plus about 50 MiB.
- Throughput did not drop. The "before" round ran while the host was busier (load 6–10), so
  read its op/s as no regression, not as a gain.
- The batch workload alone on a fresh node peaked at 950 MiB before and 182 MiB with budgets.
- After unloading that database, RSS was 700 MiB for 10 s, then 43 MiB. With budgets it fell
  from 110 MiB to 45 MiB within one 1 s sample.

## Analysis

- **Point reads (prepared):** roll matches ScyllaDB on 2 CPUs and is ~4.6x Cassandra. With 1 CPU,
  ScyllaDB is ~30% ahead.
  - A read is a bloom check per SSTable, a binary search of the block index, and a binary search
    of the partition headers inside a cached 64 KiB block.
  - With the fixes below, about a third of roll's profile is the connection read and write loops,
    mostly `send` and `recv`.
- **Writes:** roll is ahead of ScyllaDB at 2 CPUs and on a par at 1 CPU.
  - A write is acknowledged once it is in the memtable (`durability = async`). The commit log
    ships every ~100 ms; Cassandra's default periodic commit log syncs every 10 s.
  - Compaction, with zstd block compression, uses 20–33% of roll's CPU during the load.
  - roll's p99.9 at 1 CPU (37 ms) is worse than ScyllaDB's (11 ms): requests queue behind flush and
    compaction on the same core.
- **Mixed:** this is roll's weak spot.
  - On 2 CPUs the scylla dialect is 30% behind ScyllaDB; the cassandra dialect's rounds were close
    to ScyllaDB's. At 1 CPU roll is 40% behind.
  - Two costs dominate the profile:
    - compaction (24% of CPU)
    - decoding blocks from new SSTables (10%): blocks are decompressed when first read, and a
      flush or compaction makes new SSTables whose blocks are not cached yet
  - ScyllaDB's row cache survives flushes. Filling roll's block cache when an SSTable is written
    is the obvious next step; it is not done here.
- **Unprepared:** roll parses and prepares every QUERY (~10% of CPU: the lexer, the parser and the
  plan), as both references do. Its relative cost is lower than ScyllaDB's.
- **LWT:** this is not like for like.
  - Each partition has exactly one writer, and object storage is the replication. A conditional
    write therefore reads and writes under a per-partition lock, with no Paxos rounds and no
    `system.paxos` writes.
  - Both references run Paxos even on one node.
  - The client-visible behaviour is the same, and the cost is 8–10x lower.
- **Batches:** a LOGGED batch in roll is one atomic write. The references write the batch log
  first. roll is ~3x faster, and its tail is far lower (ScyllaDB p99.9 1 s, Cassandra 288 ms).
- **Wide partitions:** roll is on a par with ScyllaDB at 2 CPUs (2.1M rows/s) and 25% behind it at
  1 CPU. Two costs are visible in the profile:
  - the per-cell `Bytes` reference counts (13%)
  - the client-requested snappy compression of the 1 MB responses (12%)
- **Variance:** the references, ScyllaDB in particular, varied more between rounds (write
  40–87k op/s) than roll did. Part of that is the host: other containers were running. Compare the
  ranges, not only the means.

## Profiles and optimisations

The profiles were taken with `scripts/nosql-bench.sh profile`. It runs `perf record` with LBR call
stacks (in a container, because the host has no perf) on rolld for 15 s of each workload. The
fixes below target the hot spots that were clearly roll's own.

Each fix was measured in A/B runs: roll on 2 CPUs, 2 rounds each, alternating binaries. The table
shows op/s.

| workload | before | storage fixes | + response coalescing |
| --- | ---: | ---: | ---: |
| write | 76,156 | 77,347 | 99,252 (+30%) |
| read | 94,772 | 107,988 (+14%) | 123,166 (+30%) |
| unprepared | 74,560 | 69,520 | 91,071 (+22%) |
| mixed | 50,830 | 52,904 | 55,234 (+9%) |
| wide | 214 | 224 | 221 (+3%) |

1. **Response coalescing** (`nosql-server/src/conn.rs`, `write_loop`):
   - Before, each connection's writer flushed as soon as its queue was empty. That meant one `send`
     per response, ~20% of CPU in the read profile.
   - Now it lets the tasks that are already ready run once (`yield_now`) before flushing. The
     responses that finish in the same scheduler pass then share one write.
   - Alone on the runtime the yield returns at once, so a lone response is not delayed.
   - Result: writes +28%, reads +14%. Write p99.9 rose from 7 to 10 ms.
   - Tests: `write_loop_coalesces_answers_of_one_scheduler_pass` (fails without the change) and
     `write_loop_writes_a_lone_answer_at_once`.
2. **Point-read lookup in a block** (`nosql-storage/src/sstable`):
   - Before, every point read rebuilt and checked the block's whole partition-offset table (~300
     entries in a `Vec`). It also built a `Bytes` key, with two atomic refcount operations, for
     each binary-search probe.
   - The trailer is now checked once, when the block is decoded into the cache. Reads use the
     offsets in place (`HeaderOffsets`) and compare keys without copying them (`header_key_cmp`).
   - Tests: `trailer_is_checked_at_decode_and_split_in_place`.
3. **Block cache insert** (`nosql-storage/src/cache.rs`):
   - Before, `Lru::insert` scanned every entry of its shard to drop an older copy of the key: O(n)
     per cache miss, under the shard lock. It is now a direct `remove`.
   - Tests: `lru_replaces_evicts_and_removes_by_key`.

Fixes 2 and 3 together give reads +14%.

Hot spots that remain, and are not changed here:

- **The allocator settings in `rolld/src/main.rs`.** `mi_collect` runs every time a worker parks,
  and the purge delay is 0. Together they cost 5–8% of CPU under load: `mi_heap_collect_ex` was
  4.6% of the read profile, and `madvise` was 2.5–3.7%. They are deliberate: they buy the small
  idle footprint for every rolld product.
- **Block decode on the blocking pool.** On a cache miss, handing a ~50 µs zstd decode to the
  blocking pool costs about as much as the decode (~10% of the unprepared profile).
- **Compaction CPU.** This is zstd and merging. The codec was chosen for object-storage bytes; see
  `sstable/mod.rs`.

## Reproducing

```sh
cargo build --release -p rolld                           # the script runs target/release/rolld
scripts/nosql-bench.sh                                   # 2 CPUs (4,6): every target and workload
OUT=target/bench-nosql/r2 TARGETS="cassandra scylla roll-cassandra roll-scylla" scripts/nosql-bench.sh
CPUS=4 TARGETS="scylla roll-scylla" OUT=target/bench-nosql/cpu1 scripts/nosql-bench.sh
scripts/nosql-bench.sh cold                              # scale-to-zero and restart timings
scripts/nosql-bench.sh profile                           # perf profiles of rolld per workload
scripts/nosql-bench.sh table target/bench-nosql target/bench-nosql/r2
```

Settings come from environment variables:

- `CPUS` and `LOAD_CPUS`: which CPUs the servers and the load generators use.
- `ROWS`, `DURATION` and `THREADS`: the data size, the seconds per workload, and the
  cassandra-stress threads.
- `TARGETS` and `WORKLOADS`: what to run.
- `ROLLD`: the rolld binary.

The script waits for free memory before every server and load generator. It removes every
container it starts and drops the `bench_*` keyspaces. Raw tool output goes to
`target/bench-nosql/<target>/<workload>.txt`.
