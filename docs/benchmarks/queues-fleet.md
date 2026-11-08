# roll queues on the fleet: logs in S3

PerfTest (`pivotalrabbitmq/perf-test:latest`) against one roll queues node in a container limited to 4 CPUs
and 4g, 30s per scenario, 1 KB persistent messages to quorum queues (which deliver only
what's durable), run by `scripts/bench-queues-fleet.sh` on Linux x86_64, 2026-11-08.
Rates are received msg/s; latencies are PerfTest's confirm latency (publish → confirm) and
consumer latency (publish → delivery), p50 / p99 in ms.

| storage | scenario | msg/s | confirm p50 / p99 | consumer p50 / p99 |
| --- | --- | ---: | ---: | ---: |
| file | 1 producer → 1 consumer, confirms (200 in flight) | 75.1k | 1.7 / 12.8 | 4.0 / 44.7 |
| file | 4 producers → 4 consumers, confirms (200 in flight each) | 71.6k | 9.2 / 28.1 | 11.0 / 28.2 |
| file | 1000 msg/s, confirms (50 in flight) | 1.0k | 0.3 / 2.0 | 0.4 / 2.8 |
| file | 100 queues, 100 producers × 50 msg/s | 5.0k | 0.4 / 35.4 | 0.5 / 34.4 |
| s3-local | 1 producer → 1 consumer, confirms (200 in flight) | 7.1k | 20.3 / 57.2 | 22.8 / 59.2 |
| s3-local | 4 producers → 4 consumers, confirms (200 in flight each) | 11.7k | 74.7 / 91.9 | 75.2 / 94.0 |
| s3-local | 1000 msg/s, confirms (50 in flight) | 1.0k | 13.2 / 50.9 | 13.6 / 46.5 |
| s3-local | 100 queues, 100 producers × 50 msg/s | 5.0k | 6.9 / 147.7 | 6.8 / 140.2 |
| s3-modelled | 1 producer → 1 consumer, confirms (200 in flight) | 2.0k | 73.1 / 238.5 | 75.4 / 238.4 |
| s3-modelled | 4 producers → 4 consumers, confirms (200 in flight each) | 4.2k | 207.6 / 267.6 | 207.4 / 268.0 |
| s3-modelled | 1000 msg/s, confirms (50 in flight) | 666 | 67.7 / 225.9 | 68.5 / 223.2 |
| s3-modelled | 100 queues, 100 producers × 50 msg/s | 5.0k | 101.9 / 343.7 | 102.3 / 286.2 |

- **file**: local disk, group-committed through the shared WAL: a node alone (`rolld` on a laptop).
- **s3-local**: the fleet's code path (`s3:` storage, group commit) on MinIO in the same Docker
  network, no added network latency: what the storage code itself costs.
- **s3-modelled**: the same through Toxiproxy, which delays every answer by 25 ± 10 ms,
  standing in for S3 Standard's PUT latency in-region. A model, not a measurement of S3.
- **s3**: a real S3 Standard bucket (only when `ROLL_BENCH_S3_BUCKET` was set).

## Reading the numbers

- **One queue's throughput is its confirm window over the confirm latency.** A publisher with
  N confirms in flight gets at most N / latency messages a second: 200 in flight at ~75 ms is
  ~2.7k/s, and the 1000 msg/s scenario (50 in flight) can't reach its rate on modelled S3.
  Publishers that want more from one queue keep more in flight or use more queues.
- **A confirm costs about three PUT latencies** on the object log: a queue has one log append
  in flight at a time (the object log has no pipelined appends), and a node's shared stream
  has one PUT in flight at a time, so a message waits for the PUT before it, then its own.
  Pipelining both would bring it towards one PUT.
- **A node's throughput isn't capped by PUTs**: with many queues every stream PUT carries
  whatever arrived meanwhile (the 100-queue scenario keeps its 5k msg/s at any latency).
- **s3-local** is bounded by MinIO itself (it syncs every object to disk) on a shared host, not
  by the network; read it as the code path's cost, not as S3's.
- Why S3 Standard is still the fleet's default (over keepers on the nodes, 1–3 ms): docs/queues/storage.md,
  "On roll cloud".

Raw PerfTest output is in `target/bench-queues-fleet/`. Scenario arguments:

- 1 producer → 1 consumer, confirms (200 in flight): `-x 1 -y 1 -s 1000 -u bench-q -ad false -f persistent -c 200 -q 200 --queue-args x-queue-type=quorum`
- 4 producers → 4 consumers, confirms (200 in flight each): `-x 4 -y 4 -s 1000 -u bench-q4 -ad false -f persistent -c 200 -q 200 --queue-args x-queue-type=quorum`
- 1000 msg/s, confirms (50 in flight): `-x 1 -y 1 -s 1000 -u bench-lat -ad false -r 1000 -f persistent -c 50 -q 50 --queue-args x-queue-type=quorum`
- 100 queues, 100 producers × 50 msg/s: `-x 100 -y 100 -s 1000 --queue-pattern bench-many-%d --queue-pattern-from 1 --queue-pattern-to 100 -ad false -r 50 -f persistent -c 50 -q 50 --queue-args x-queue-type=quorum`
