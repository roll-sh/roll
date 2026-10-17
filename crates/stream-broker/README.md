# stream-broker

The Kafka-compatible broker behind roll streams. Any Kafka client, tool or framework connects to it
over the Kafka protocol. You can also skip the client and produce and consume over plain HTTP. Storage
is bottomless: records are fsynced locally, uploaded to object storage (S3 or a local directory) and
read back from there when they're no longer on disk. Partitions nobody uses close and cost nothing but
their objects.

`rolld` runs it next to the Postgres side. `examples/serve.rs` runs it on its own, which is what the
numbers below use.

```sh
cargo run --release -p stream-broker --example serve -- /tmp/roll-streams   # Kafka :9092, HTTP :7071
```

## How it works

```
 Kafka clients ──TCP :9092──┐                         ┌── HTTP clients, SSE, Confluent REST v3
                            ▼                         ▼
                      handlers/ (wire)          http/ (data + control plane)
                            └──────────┬──────────────┘
                                       ▼
                 Broker: streams → topics → partitions (opened on demand)
                     │            │                │
            groups.rs · txn.rs    │          stream_log::Log  ── segments on local disk
            quotas · SASL         │                │     (fsync, high watermark, tail cache)
                                  ▼                ▼
                    stream.db (SQLite catalog)   uploader ──► object storage (chunks)
                                                   ▲              │
                                       compaction.rs ─────────────┘  remote reads via RemoteCache
```

**Write path.** A produce request is validated and appended as received. The checks cover the CRC,
the idempotent sequence, the size, the timestamps and, for compacted topics, that every record has a
key. The only change is patching the base offset into the batch header; records are never decoded.
Appends from every connection on a partition are group-committed into one fsync. `acks=all` returns
after that fsync, and the high watermark only moves to durable data. So consumers never see a record a
crash could take back.

**Read path.** Fetch returns stored bytes as they are. Recent batches come from a per-partition tail
cache in memory, so tailing consumers don't touch the disk. Older ones are read from segment files
using the offset and time indexes. Anything below the local range comes from object storage through
an LRU `RemoteCache`. Long-polling fetches park on the high watermark and wake when it moves, and
responses are pipelined in request order per connection.

**Tiering.** The uploader runs every second. For each open partition it uploads sealed segments, and
the active segment once 8 MB is pending or its oldest pending byte is 10 s old. Each chunk is whole
batches, at most 32 MB, written to object storage and recorded in the catalog. After upload, local
copies are dropped by `local.retention.ms/bytes` (default 1 h / 256 MB per partition). `retention.*`,
`DeleteRecords` and topic deletion remove the objects too.

**Partition lifecycle.** Nothing opens at startup. A partition opens on its first produce or fetch.
The newest segments are checked (a header-only scan) and the torn tail of a crash is cut at the last
valid batch. After `idle_unload_ms` (10 min) without traffic, once everything is durable and uploaded,
the partition closes:

- It drops its local copies, since object storage has everything. Reads of it come from there until
  it's busy again (`--stream-keep-local` turns this off).
- It writes a clean-close marker so the next open can skip validation.
- It frees its memory.
- It records when retention, local retention or compaction will next have work.

The cleaner (every 30 s) leaves a closed partition alone until that time. So idle partitions cost no
CPU, memory, disk or object-storage requests. All open partitions share one tail-cache budget
(`tail_cache_total_bytes`, 256 MB), and each gets at most `tail_cache_bytes` (1 MB) of it.

**Compaction.** `cleanup.policy=compact` (`compaction.rs`) keeps the latest record per key. It drops
keyless records, aborted transactional data and, after `delete.retention.ms`, tombstones. It reads
uploaded chunks, writes the survivors to new objects (`.c<generation>.kb`, never reusing a key) and
swaps them into the catalog atomically. Offsets don't change. The triggers are
`min.cleanable.dirty.ratio`, `min.compaction.lag.ms` and `max.compaction.lag.ms`. Old objects are
deleted after a grace period, so in-flight reads still find them.

**Semantics.**

- Idempotent producers, including KIP-360 epoch bumps.
- Transactions with KIP-890 verification, the last stable offset and `read_committed`.
- Exactly-once consume-transform-produce.
- Classic consumer groups, including static membership.
- Byte-rate quotas with KIP-219 throttling.
- SASL SCRAM-SHA-256/512 and PLAIN, plus TLS.

Every stream is its own isolated cluster with its own topics, groups, transactions, keys and quotas.

**HTTP** (`http/`).

- Produce JSON, NDJSON or raw bodies. Keys are partitioned with Kafka's murmur2, so records land on the
  same partition a Kafka client would pick.
- Read with stateless cursors, with long polling, or as Server-Sent Events.
- Server-managed groups lease partitions to consumers and commit for them. These are the same
  committed offsets that Kafka groups see.
- Confluent REST v3 and the control plane (streams, keys, topics, configs, usage) are under the same
  router.

**Catalog.** `stream.db` (SQLite) holds streams, API keys, topics, the chunk index, committed offsets,
transaction state and usage. Partition data lives only in the logs and in object storage.

## Cold start

No partitions are loaded when the broker starts, and no compute boots per tenant. Startup reads the
catalog and recovers open transactions, so its cost doesn't depend on how much data there is.

The measurements below are from a release build on an M-series Mac with an APFS SSD, using
`examples/serve.rs`. Object storage was a local directory. On S3, add its first-byte latency
(typically tens of ms) to every row that reads from object storage; we haven't measured that here.

| what | time |
| --- | --- |
| broker start → ready (in-process: catalog, transaction recovery, listeners) | 1–4 ms, empty or with 600 partitions and 300 MB |
| process exec → first HTTP answer, measured from a shell polling loop | 25–340 ms (mostly exec and the polling loop) |
| first read of a closed partition, small / 256 MB (from object storage, HTTP round trip with curl) | 1.3 ms / 5 ms |
| the same with `--stream-keep-local` (read from local disk) | 1–2 ms / 2.3–3.1 ms |
| the same read with the partition already open | 0.4–0.8 ms / 1.8–2.3 ms |
| first read after a restart, 256 MB partition | 5 ms (2.4–2.8 ms keeping local copies) |
| local data deleted (catalog kept): first read of a chunk, then reads nearby | 5–7 ms, then 1.2–1.9 ms |

Producing to a closed partition never touches object storage: it opens an empty segment at the next
offset.

`examples/coldopen.rs` times `Log::open` plus the first read, with no HTTP in the way. After a crash,
the last two segments are fully validated. After a clean close, only their headers are read.

| partition size on local disk | after a clean close | after a crash |
| --- | --- | --- |
| 1 MB | 0.12 ms | 0.18 ms |
| 64 MB | 1.7 ms | 9.0 ms |
| 256 MB | 8.5 ms | 22 ms |
| 1 GB | 34 ms | 42 ms |

These sizes only apply to open partitions or `--stream-keep-local`. An open partition keeps at most
`local.retention.bytes` (256 MB) plus the active segment. A closed one keeps nothing, so opening it
costs the same as the 1 MB row.

## Idle

These were measured with `ROLL_IDLE_UNLOAD_MS=5000`. The data was 100 topics × 6 partitions with a few
KB each, plus one topic with 256 MB: 304 MB on disk and 601 partitions in total.

| state | CPU | memory (physical footprint) | local disk |
| --- | --- | --- | --- |
| empty broker | 0.03–0.07 % of a core | 9 MB | 0 |
| just after writing, 601 partitions open | — | 75–100 MB (42 MB of it tail cache) | 304 MB |
| idle, every partition closed | 0.03–0.07 % of a core | 28–35 MB | 2.3 MB (empty files) |
| idle, closed, `--stream-keep-local` | same | same | 307 MB |

A closed partition costs about 3.4 KB of memory: 6,000 closed partitions add 20 MB in total. The
idle footprint above is mostly the allocator holding on to memory from the 256 MB burst.

- **Wake-ups at idle.** Group heartbeat checks every 100 ms, the uploader and transaction timeouts
  every second, the cleaner every 30 s, usage metering every 60 s. None of them do work for closed
  partitions, and none of them make object-storage requests.
- **Why anything wakes up.** Each timer is needed while something could be live. The group check
  expires dead consumers and runs rebalances, which the Kafka protocol requires. The transaction check
  aborts transactions whose producer vanished, otherwise `read_committed` readers would stall. The
  uploader bounds what a lost host can lose. The cleaner applies retention. The meter records usage.
  At idle each wake-up loops over empty lists in microseconds. They could become event-driven to reach
  zero wake-ups, but that's worth about 0.03 % of a core.
- **Allocator.** `rolld` and `serve` use mimalloc. They purge freed pages immediately (the default
  waits 1 s), and each worker collects its heap when it goes idle, so memory freed after a burst
  doesn't stay with parked threads. The same idle run held 334 MB with the macOS system allocator
  (live allocations were about 50 MB) and 84–101 MB with mimalloc's defaults. Neither change cost
  measurable throughput. Collecting more aggressively, with a forced collection on every park,
  brings the idle footprint down to 7–11 MB. But it costs 15–30 % more CPU at moderate request rates,
  so it's off.
- **RSS on macOS.** It overstates use because it counts pages the OS hasn't reclaimed yet. The table
  uses the physical footprint from `vmmap`. We haven't measured Linux yet.

### Compared with stock Kafka

Stock Kafka is `apache/kafka:4.1.0`: single-node KRaft, default config (`-Xms1G -Xmx1G`), in Docker
Desktop's Linux VM. It got the same workload, 601 partitions and 310 MB written with
`kafka-producer-perf-test`. Its CPU comes from the container's cgroup over 60 s and its memory is the
JVM's RSS.

| | Kafka | roll | difference |
| --- | --- | --- | --- |
| idle CPU, empty | 2.3 % of a core | 0.03–0.07 % | 30–75× less |
| idle memory, empty | 312 MB | 10 MB | ~30× less |
| idle CPU, 601 partitions | 1.6 % of a core | 0.03–0.07 % | 25–50× less |
| idle memory, 601 partitions | 644 MB RSS (1.7 GB with page cache) | 28–35 MB footprint | ~20× less |
| local disk, 601 partitions, 310 MB written | 310 MB × replication factor | 2.3 MB once closed | ~130× less with one replica |

Kafka keeps every partition's log and memory-mapped indexes open, reserves a 1 GB heap up front, and
keeps its background threads running (KRaft heartbeats, log maintenance). roll closes partitions
nobody uses, and for those its periodic tasks have no work to do. The two ran on different kernels
(Kafka under Linux in Docker's VM, roll on macOS), so read the ratios as orders of magnitude.

## Local disk (EBS)

Object storage is the system of record; local disk is only a write buffer and a cache. What it holds:

- data not yet uploaded: at most 10 s or 8 MB per busy partition;
- open partitions' recent segments, up to each topic's `local.retention.*` (1 h / 256 MB);
- `stream.db`, the catalog: small, but it has to persist (see Limits).

Closed partitions keep nothing. That leaves open partitions as the only part that grows with data,
and `--stream-disk-mb` (`ROLL_STREAM_DISK_MB`) caps it across all of them. Over the cap, the cleaner
drops the oldest uploaded segments first, from whichever partitions hold them, and those reads go to
object storage. Data that isn't uploaded yet is never dropped. Sizing a volume is then the budget plus
a few seconds of peak ingest plus the catalog, however much data the streams retain.

With that, the costs are:

- **EBS.** gp3 bills provisioned size ($0.08/GB-month), so the cap is what you pay for. A 20 GB volume
  is $1.60 a month for any amount of retained data.
- **Throughput.** gp3's baseline is 125 MB/s and 3,000 IOPS. Sustained ingest above that needs
  provisioned throughput, or instance-store NVMe. Group commit keeps IOPS low: one fsync per batch of
  appends, not one per request.
- **Instance store.** NVMe instance storage costs nothing extra and is faster. The catch: on host loss
  it goes along with the un-uploaded tail and `stream.db`. An EBS volume survives the instance and can
  be reattached, so keep `stream.db` on EBS either way.

In the measurement above, the 128 MB budget held 601 open partitions with 304 MB written at 113 MB of
local data, and every record stayed readable.

## Performance

These were measured with a release build on an M-series Mac, with a single broker, 6 partitions and
`acks=all` (so every acknowledgement waits for fsync). The Java tools ran in Docker Desktop, so their
traffic also crosses Docker's VM network.

| workload | result |
| --- | --- |
| librdkafka produce, 200 B records (`examples/bench.rs`) | 0.63–1.1M rec/s (126–225 MB/s) |
| librdkafka produce, 1 KB records | 200–330k rec/s (200–330 MB/s) |
| librdkafka consume, 200 B records | 0.6–2.5M rec/s (120–503 MB/s) |
| librdkafka consume, 1 KB records | 0.89–1.09M rec/s (0.89–1.09 GB/s) |
| `kafka-producer-perf-test`, 200 B, uncompressed / lz4 (Docker) | 321k / 300k rec/s (Docker's network is the limit) |
| `kafka-producer-perf-test` at 1,000 rec/s, `linger.ms=0` (Docker) | p50 14 ms, p99 63 ms, fsync included |
| `kafka-consumer-perf-test` (Docker) | 509k rec/s (97 MB/s) |
| HTTP produce, 100k records (20 MB) in one NDJSON request | durable after 158 ms (630k rec/s) |

The data path never decodes records, so cost follows bytes and batches rather than record count.
Bigger batches, or compression done on the client, get closer to disk speed. The ranges cover
several runs on a laptop, where the client and the broker compete for the same cores. The benchmark
fills the client's queue on purpose, so its average acknowledgement latency measures queueing, not the
broker. The latency to quote is the 1,000 rec/s row.

### Reproduce

```sh
cargo build --release -p stream-broker --examples
./target/release/examples/serve /tmp/roll-streams &              # or: cargo run --release -p rolld
./target/release/examples/bench 127.0.0.1:9092 5000000 200       # produce, then consume, N × size
./target/release/examples/coldopen                               # partition open by size
curl -s localhost:7071/v1/kafka                                  # open_partitions, tail_cache_bytes, …
vmmap --summary $(pgrep -f examples/serve) | grep 'Physical footprint'
```

`ROLL_KAFKA` and `ROLL_HTTP` set the listen addresses for `serve`. `ROLL_IDLE_UNLOAD_MS` and
`ROLL_CLEAN_INTERVAL_MS` make partitions close sooner, which helps when measuring idle use.
`ROLL_STREAM_DISK_MB` and `ROLL_STREAM_KEEP_LOCAL` work as they do for `rolld`.

## Limits

These are planned work, not design limits:

- **One broker per cluster.** One process serves every partition. There's no leadership across
  brokers or replication yet.
- **Durability on host loss.** Acknowledged data is fsynced locally and reaches object storage within
  10 s or 8 MB per partition. Losing the host's disk loses what wasn't uploaded yet.
- **The catalog is local.** `stream.db` has to survive with the host (a persistent volume), or the
  chunk index and offsets are gone. Replicating it to object storage is next.
- **Producer state across a close.** Idempotent-producer state is rebuilt from local segments when a
  partition opens. So a retry of a batch sent before the partition closed (after 10 idle minutes)
  isn't recognised as a duplicate. Open transactions keep their segments local, so their state is
  always rebuilt. Snapshotting producer state to object storage would close this gap.
- **Not yet supported:** the KIP-848 consumer protocol and share groups (classic groups only), fetch
  sessions (KIP-227; clients fall back to full fetches) and ACLs (keys scope a whole stream).

## Code map

| path | what |
| --- | --- |
| `src/server.rs` | TCP/TLS accept loop, framing, in-order pipelining |
| `src/handlers/` | one module per API family. `mod.rs` lists the supported versions (`APIS`) |
| `src/broker.rs` | streams, topics, partitions, the uploader, cleaner, idle unload and remote reads |
| `src/compaction.rs` | log compaction over uploaded chunks |
| `src/groups.rs`, `src/txn.rs` | group and transaction coordinators |
| `src/auth.rs`, `src/tls.rs` | SASL and per-stream keys, TLS |
| `src/records.rs` | record decoding for the few places that need it (compaction, timestamps, HTTP, key checks) |
| `src/store.rs` | the SQLite catalog |
| `src/http/` | the control plane, the HTTP data plane, managed groups, Confluent REST v3 |
| `../stream-log` | the partition log: segments, indexes, group commit, producer state, tiering hooks |

## Tests

`pnpm test:stream` runs clippy and every suite. It skips itself when no stream crate changed since the
last green run. The librdkafka tests build librdkafka from source, which needs `cmake` and, on Linux,
the curl and SASL headers (`apt-get install cmake libcurl4-openssl-dev libsasl2-dev`).

- `tests/protocol.rs`: every API at every version, malformed frames, SASL, tenancy, quotas.
- `tests/rdkafka.rs`: librdkafka produce and consume, codecs, idempotence, groups, transactions, admin.
- `tests/java.rs`: the Kafka 4.1 CLI tools, perf tools, verifiable clients and Kafka Streams with
  exactly-once, all in Docker.
- `tests/storage.rs`: tiered reads, retention, torn writes, unclean stops, scale to zero, closed
  partitions keeping nothing on disk, the local disk budget.
- `tests/compaction.rs`: latest value per key, tombstones, reading across gaps.
- `tests/http.rs`: the HTTP API and how it interoperates with Kafka clients.

Set `ROLL_E2E_S3_BUCKET` (plus `ROLL_E2E_S3_ENDPOINT` and AWS credentials) to run them against S3.
