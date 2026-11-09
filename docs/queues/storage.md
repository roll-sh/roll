# roll queues — storage (`mq-storage`)

Nodes hold no state of their own. Every durable queue partition, stream and delayed exchange is
a log (`mq_core::log::Log`), and `mq-storage` provides the production backends of
`mq_core::log::LogStore`:

| backend          | where the data lives                                         | use (`--queues-storage`)    |
| ---------------- | ------------------------------------------------------------ | --------------------------- |
| `S3ExpressStore` over S3 Standard | objects in a regular bucket, group-committed (3 AZs) | roll cloud: what a cluster uses by default (`s3:`, the node's bucket under `queues/logs`) |
| `S3ExpressStore` | the same in an S3 Express One Zone directory bucket (1 AZ)   | lower latency where one zone is acceptable (`s3express:`) |
| `QuorumStore`    | 3 self-hosted `roll-keeper`s (one per AZ, EBS) + optional S3 tier | self-hosted clusters (`quorum:`) |
| `FileStore`      | local disk (`mq-core`)                                       | a node alone (`rolld`; the default) |

## On roll cloud: S3 Standard, group commit

roll cloud has no servers of its own for queues (docs/cloud/README.md): every node runs them, and
nothing a confirmed message depends on may live on one node's disk or in one zone. The fleet's
log store is the object log below on S3 Standard, in the deployment's data bucket.

- **Why S3 and not keepers on the nodes.** The alternative was keepers embedded in the nodes, a
  2-of-3 quorum across zones on their local disks, with automatic re-replication when a node goes
  away. It has the lower confirm latency (a network round trip and an fsync, about 1–3 ms) but
  needs dynamic quorum membership, re-replication and repair while nodes come and go every
  deploy and Spot interruption, and node disks become something to lose. S3 Standard keeps every
  object in three zones before a PUT returns, survives losing a node or a zone with nothing to
  repair, and is already the source of truth for every other product. Its cost is confirm
  latency: about three PUT latencies today (p50 ~70 ms, p99 ~240 ms with S3's latency modelled at
  25 ± 10 ms per request) instead of a few milliseconds, so one queue's throughput is its
  publishers' confirm window over that ([docs/benchmarks/queues-fleet.md](../benchmarks/queues-fleet.md)). Group commit keeps
  throughput: every queue of a node shares one stream of objects, and each PUT carries whatever
  arrived while the previous one was in flight, so a busier node writes bigger objects, not more
  of them. The default is S3 Standard; never a single-zone store (`s3express:` stays an opt-in).
- **Confirms** are sent only after the stream object holding the message is written (S3 answers
  only once the object is durable in three zones). Quorum queues deliver only what's durable;
  classic queues may deliver first, as RabbitMQ's do, and never lose a confirmed message.
- **Fencing.** Every object is written with `If-None-Match: *`. A new owner (holding the queue's
  roll-cluster lease) claims the log's tail with a fence marker, and drains the previous owner's
  stream (writing its next object first), so a deposed owner's next write collides and it can't
  confirm anything after the takeover. Lease epochs map onto log epochs, and an older lease can't
  open the log again.
- **Handoff.** A node handing a queue off (a drain, rebalancing) stops its actor, closes the log
  (moving what it staged into the log's own objects and writing the closing marker) and only
  then releases the lease, so the next owner opens a cleanly closed log without draining
  anything.
- **Requests.** An idle node makes no PUTs. A busy one makes at most one stream PUT in flight at a
  time (plus a link per log per stream, and the batched moves into each log's own objects every
  few minutes or MiB). S3 Standard bills $0.005 per 1,000 PUTs: a node confirming continuously
  with 25 ms PUTs makes about 40 a second, ~$0.70 an hour at full tilt, whatever its message rate
  and number of queues.
- **Conditional writes** need S3 (since 2024), MinIO, R2 or another store with `If-None-Match` on
  PUT. A concurrent conditional write to the same key (S3's 409) is treated as "already exists",
  which is what it means for a fence.
- **A versioned bucket** (roll cloud's data bucket is one) works: a key whose current version is a
  delete marker counts as absent, and nothing is ever written again at a deleted key (writers only
  move forward, and fence markers are never deleted). Deleted log objects would linger as
  noncurrent versions, so the stack expires those under `queues/logs/` after a day.

## Quorum log

```
 broker (writer, owns the partition lease)
   │ append(batch) ──► keeper az1 ─┐
   │                ──► keeper az2 ─┼─ ack after W=2 fsyncs ──► publisher confirms
   │                ──► keeper az3 ─┘
   │ sealed segments ──► s3://bucket/{prefix}/{log}/{first:020}.seg ──► keepers drop them
```

- **Appends** go to every keeper in parallel and return once `write_quorum` keepers have
  fsynced them: one network round trip plus one fsync. Keepers group-commit: each shard thread
  drains all queued requests, writes them to the logs' segment files, appends the same data to
  the shard's WAL and fsyncs only the WAL, then replies. A batch costs one fsync however many
  logs it touched (see [Shared WAL](#shared-wal)).
- **Fencing.** Each `open` takes a new, strictly higher keeper epoch and fences `N - W + 1`
  keepers with it. A deposed owner reaches at most `W - 1` keepers, so its appends fail with
  `FencedError` instead of being confirmed. `open(key, 0)` takes the log over from whoever had
  it; `open(key, lease)` with a lease epoch refuses to go back to an older lease and maps lease
  `e` to keeper epochs `[e << 16, (e + 1) << 16)`.
- **Recovery.** `open` adopts the log of the fenced keeper with the greatest
  `(epoch of last entry, tail)` — which contains every acknowledged entry — copies it to a
  write quorum, and truncates divergent, never-committed suffixes on other keepers using
  per-keeper epoch histories. The invariants and their proof sketch are in the module docs of
  `crates/mq-storage/src/quorum.rs`.
- **Failures.** A keeper that is down is skipped; its replicator task retries with backoff and,
  when it returns, re-fences it, truncates it to the common prefix and catches it up from the
  writer's retransmit buffer or from another keeper (or resets it to the hot start if what it
  missed was truncated or tiered). With fewer than `W` keepers available, appends fail fast and
  are never confirmed. Failed batches keep their indices and are retransmitted unchanged, so
  they may still become durable later (at-least-once, like RabbitMQ after a nack).
- **Reads** are served from the writer's in-memory tail when possible, otherwise from a keeper
  that holds the range (same AZ first), otherwise from object storage through an LRU cache.
- **Tiering** (`QuorumConfig::tier`). When the active segment passes `segment_bytes` (or on
  `roll()`), it is sealed; sealed, committed segments are uploaded once the sealed hot bytes
  exceed `keep_hot_bytes`, and the keepers are told to drop them (`Tiered`). Objects use the
  keepers' frame format plus an index and a footer (see `crates/mq-storage/src/tier.rs`).
  `truncate_prefix` deletes cold objects too; `bytes()` counts hot plus cold.

### Wiring it into `rolld`

```rust
use mq_storage::{QuorumConfig, QuorumStore, TierConfig};

let tier = TierConfig {
    keep_hot_bytes: 256 << 20,
    ..TierConfig::new(Arc::new(object_store::aws::AmazonS3Builder::from_env().with_bucket_name("roll-queues").build()?), "logs")
};
let logs = QuorumStore::connect(QuorumConfig {
    keepers: vec![(addr1, "use1-az1".into()), (addr2, "use1-az2".into()), (addr3, "use1-az4".into())],
    write_quorum: 2,
    az: "use1-az1".into(),          // this broker's AZ
    tier: Some(tier),
    ..Default::default()
})
.await?;
let broker = mq_core::Broker::start(config, meta, logs.clone()).await?;
// metrics: logs.stats() → StorageStats { bytes_hot, bytes_cold, append_latency, keepers, .. }
```

A keeper can also run in-process (tests, single binary): `mq_storage::Keeper::start(listener,
KeeperConfig { dir, az, fsync: true, shards: 8, ..Default::default() })`, stopped with `keeper.stop().await`.

## Shared WAL

Hundreds of queue logs that each fsync their own files cost one fsync per log per write, and the
tail latency of a many-queue workload follows. Both the keepers and the single-node `FileStore`
avoid that the way RabbitMQ's quorum queues do, with one write-ahead log shared by many logs
(`mq_core::wal`):

- A log writes its segment file **without** fsync and hands the same bytes to the WAL. One
  writer (a keeper shard, or the `FileStore`'s WAL thread) takes everything queued, writes it
  in one go and fsyncs once; only then are the appends acknowledged.
- When a WAL file passes 256 MB, the writer starts a new one. A checkpoint thread fsyncs every
  log with writes the old file covers, then deletes it. Appends never wait for checkpoints:
  the checkpoint only collects file handles under a log's lock and fsyncs without it, and a
  segment that fills up is closed without an fsync (the WAL still covers it) and synced by the
  next checkpoint, so no append waits for a large fsync.
- On start-up, whatever WAL is left is replayed into the logs before anything else runs.
  - On keepers, each log's `meta.json` carries a WAL generation, and any operation that removes
    entries (prefix or suffix truncation, tiering, reset) first fsyncs the log and then moves
    to a new generation. Replay skips records from older generations, so it can never bring
    truncated entries back.
  - `FileStore` logs truncations and deletions in the WAL too.
- Acks (settle records) don't wait for an fsync at all: losing one in a crash only means a
  redelivery. They become durable with the next write that does wait.
- A drained queue keeps reusing its open segment until it holds 1 MB, instead of deleting
  it and creating a new file for every message.

`rolld` uses the WAL for local storage unless it is clustered: on a shared `file:` directory,
the node that takes a log over couldn't replay another node's WAL, so clustered `file:` storage
fsyncs each write.

## `roll-keeper`

```
roll-keeper [--listen 0.0.0.0:9580] [--dir /var/lib/roll-keeper] [--az use1-az1]
            [--shards 8] [--no-fsync] [--secret …] [--tls-cert cert.pem --tls-key key.pem]
```

Every flag also reads an environment variable (`ROLL_KEEPER_LISTEN`, `ROLL_KEEPER_DIR`,
`ROLL_KEEPER_AZ`, `ROLL_KEEPER_SHARDS`, `ROLL_KEEPER_NO_FSYNC`, `ROLL_KEEPER_SECRET`,
`ROLL_KEEPER_TLS_CERT`, `ROLL_KEEPER_TLS_KEY`). Logs go to stderr
(`RUST_LOG=mq_storage=debug` for detail). Data layout: `{dir}/logs/{encoded key}/` holds
`meta.json` (fencing epoch, truncation points, epoch history) and `{first:020}.seg` files.
Torn tails are truncated on load (they were never acknowledged).

The protocol is binary and length-prefixed with request ids, so one TCP connection per
broker–keeper pair is pipelined: `Fence`, `Append`, `Read`, `State`, `TruncatePrefix`,
`TruncateSuffix`, `Reset`, `Tiered`, `Destroy`, `ListLogs`, `Ping`
(`crates/mq-storage/src/proto.rs`).

### Authentication and TLS

Every connection starts with a mutual challenge-response handshake on a shared secret
(`crates/mq-core/src/transport.rs`, also used between brokers). Each side proves it knows the
secret with an HMAC-SHA256 over fresh nonces, so the secret never crosses the wire and a
recorded handshake can't be replayed. A connection that fails the handshake is closed before
it can send a request.

- Set the same value on the keepers (`ROLL_KEEPER_SECRET`) and the brokers
  (`ROLL_QUEUES_KEEPER_SECRET`). Without one, both sides use the empty secret and the keeper
  logs a warning at start-up.
- TLS is optional. With `--tls-cert`/`--tls-key`, the keeper serves TLS 1.2/1.3 (rustls), and
  brokers connect with `ROLL_QUEUES_KEEPER_TLS=1`. They verify the keeper's certificate against
  `ROLL_QUEUES_KEEPER_TLS_CA` (or the system roots), for the keeper's IP address or for
  `ROLL_QUEUES_KEEPER_TLS_NAME`.
- On AWS, Nitro instances already encrypt traffic between each other inside a VPC, so the
  stack uses the secret plus security groups and leaves TLS off. Turn it on if your policy
  needs TLS on every hop.

## Self-hosted keepers on AWS

- **Three keepers, one per AZ**, `write_quorum: 2`. Any single keeper (or AZ) can fail without
  losing acknowledged data or availability; losing two stops appends (no false confirms) until
  one returns.
- **Instances.** Small Graviton instances (`m7g.large`/`c7g.large`; network matters more than
  CPU). Keepers are disk- and network-bound; one keeper serves many brokers.
- **Disk.** A dedicated gp3 volume mounted at `--dir` (XFS or ext4). gp3's baseline 3,000 IOPS /
  125 MB/s covers most clusters thanks to group commit; raise throughput before IOPS. Size it
  for the hot tail only: with tiering on, `keep_hot_bytes` per log plus the active segments.
  Without tiering, size it for the whole retained backlog.
- **Never run with `--no-fsync`** outside tests: acknowledged appends could be lost on a power
  failure of two keepers.
- **Object storage.** A regular S3 Standard bucket in the same region; brokers need
  `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject`, `s3:ListBucket` on the prefix. Use an S3
  gateway VPC endpoint so tiering traffic is free. Don't add lifecycle expiration on the prefix
  (truncation deletes what queues no longer need).
- **Cross-AZ cost.** Brokers send each append to all three keepers; the two cross-AZ copies are
  the price of the durability guarantee. Reads prefer the broker's own AZ (`QuorumConfig::az`).
- **Compression.** Brokers and keepers agree on zstd in the first `Ping`/`Pong` of a connection
  (`FEATURE_ZSTD`), then compress frames of 1 KiB or more in both directions. Frames that don't
  shrink by at least 10% go as they are, and the sender stops trying for the next 64 frames, so
  already-compressed payloads cost no CPU. Older peers advertise no features and get plain
  frames. `QuorumConfig::compress` (`--queues-keeper-compress`, on by default) turns it off,
  e.g. for keepers in the broker's own AZ. `StorageStats::keepers` reports raw and wire bytes
  per keeper, so the saving is visible.
- **Rolling keeper upgrades.** Restart one keeper at a time and wait until it is caught up
  (`QuorumLog::replicas()` / `StorageStats::keepers`) before the next.

## Objects in S3 (no keepers)

The object log (`S3ExpressStore`) works on any bucket with conditional writes: S3 Standard on
roll cloud (`s3:<bucket>[/prefix]`), an S3 Express One Zone directory bucket
(`s3express:<bucket>[/prefix]`), MinIO in tests.

```rust
let os = mq_storage::S3ExpressStore::directory_bucket("roll-logs--use1-az4--x-s3", "us-east-1")?;
let logs = mq_storage::S3ExpressStore::new(mq_storage::ExpressConfig::new(os, "logs"));
```

- Each append batch is one object `{prefix}/{log}/{index:020}` written with put-if-absent, so
  appends cost one S3 Express PUT (single-digit ms). Fencing is by index: a new owner claims
  the tail with a 24-byte *fence marker* object (it uses one index; stream offsets skip one value
  per ownership change), and the old owner's next put collides with it. Fence markers are never
  deleted. `{prefix}/{log}/epoch` records the last epoch and the truncation point.
- Background compaction merges batches older than `compact_after` into
  `{prefix}/{log}/seg/{first:020}-{end:020}` objects and deletes the batches.

### Group commit (`ExpressConfig::group`)

One PUT per append per queue makes requests scale with the number of busy queues. With
`group: Some(GroupConfig::default())` (what `rolld` uses, `--queues-express-group`), the appends
of every log on a broker go to one shared *stream* first:

- The stream is `{prefix}/_streams/{id:016x}/{seq:020}`: objects written in sequence with
  put-if-absent, each holding the records `(log, epoch, writer, first index, entries)` that
  arrived within `linger` (4 ms). An append is confirmed when its stream object is written, so
  one PUT confirms the appends of every queue in that window.
- Before a log's first record in a stream, the log writes a 40-byte *link* object at its next
  index (a marker plus the stream id and position). Only the last link of a log matters.
- Staged entries stay in the log's tail in memory and move to the log's own objects in large
  batches: after `flush_after` (5 min), `flush_bytes` (1 MiB) per log, `max_staged_bytes` (256
  MiB) per broker, or when the log closes. A clean close also writes a closing marker, so the
  next owner needn't look at the stream. Stream objects are deleted once every log that used
  them has flushed.
- **Recovery.** A new owner that finds a link without a closing marker after it *drains* the
  stream first: it fences the stream (writes its next object), copies every record into the
  per-log objects of its logs (put-if-absent, so a racing flush by the old writer is harmless),
  marks the stream `drained` and deletes it. Nothing confirmed is lost; the old writer's next
  write to that stream fails, and it continues on a fresh stream for its other logs. A busy
  stream holds one object per group commit since its logs last flushed (thousands after a few
  minutes), and the new owner waits for the drain, so its reads, copies and deletes go 32 at a
  time (1040 objects took 12.7 s one at a time on the fleet test's MinIO).
- **Fencing** is unchanged: per-log objects are still put-if-absent at an index, and the new
  owner's fence marker sits after everything drained.
- Truncation in grouped mode records the new start in memory and writes the `epoch` marker with
  the next flush. A reopened log may then start inside a batch object; reads skip what was cut.

Requests then grow with brokers and traffic, not with the number of queues: 100 queues each
appending 5 times cost 500 PUTs one by one, and 105 grouped: a link per queue, written once per
stream, plus 5 stream objects (`grouped_appends_share_objects`).
- With S3 Express One Zone, data lives in one AZ: an AZ outage makes the logs unavailable until
  it recovers. Use it where that is acceptable; S3 Standard otherwise.
- `object_store` 0.14 supports directory buckets via `AmazonS3Builder::with_s3_express(true)`
  (session auth, zonal endpoint); conditional puts use `If-None-Match`, which S3 Express
  supports.

## Tests

- `cargo test -p mq-storage` runs three in-process keepers per test on `127.0.0.1:0`:
  replication, a keeper killed mid-stream and repaired, two keepers down (no confirms), fencing,
  recovery after a writer crashed mid-append (partial and divergent tails), truncation, tiering
  to `LocalFileSystem`, S3 Express semantics on `InMemory`/`LocalFileSystem` (grouped: shared
  objects, draining after a crash, zombie fencing, truncation), compressed replication, and a broker end to
  end test (10k confirmed messages via `lapin`, keeper killed, broker restarted with a new
  epoch, everything consumed).
- `ROLL_E2E=1 cargo test -p mq-storage --test minio` runs tiering and `S3ExpressStore` against
  MinIO in Docker (`minio/minio`, or `cgr.dev/chainguard/minio` when Docker Hub has no MinIO
  image; override with `ROLL_MINIO_IMAGE`).

## Known gaps

- Keepers keep every log they have seen open in memory (offsets index of the hot tail); idle
  logs are not evicted yet.
- `destroy` on a keeper that is down leaves its copy behind; `QuorumStore::list_logs` exists for
  a future garbage collector.
- Keeper membership is static (`QuorumConfig::keepers`); replacing a keeper means pointing the
  brokers at the new address, which then gets repaired log by log as they are written. (roll cloud
  doesn't run keepers: it uses S3, above.)
