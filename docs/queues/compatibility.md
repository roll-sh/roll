# Client compatibility

These results come from running real, unmodified client libraries for every protocol roll queues
speaks (AMQP 0-9-1, AMQP 1.0, RabbitMQ Streams, MQTT 3.1.1/5 and STOMP) and RabbitMQ's own test
suites against roll queues. Every test program was also run against RabbitMQ 4.3.6
(`rabbitmq:4-management`) to check the test itself.

Results are from 2026-10-17, on an Apple Silicon Mac with Docker Desktop. The broker ran natively
on the host and the clients ran in Docker containers.

## How to run

```sh
export CARGO_TARGET_DIR=target/agents/shared   # any target dir works
scripts/test-queues-clients.sh                 # every client in tests/queues/clients
scripts/test-queues-clients.sh python-pika go-amqp091
scripts/test-queues-clients.sh --perf          # RabbitMQ PerfTest only
scripts/test-queues-clients.sh --suites        # the client libraries' own test suites only
scripts/test-queues-clients.sh --all           # clients + PerfTest + suites
scripts/test-queues-clients.sh --rabbitmq --all  # the same against real RabbitMQ 4 (validates the tests)
```

What the script does:

- It builds `rolld` (release) and starts `rolld --queues-only` with a temporary home directory,
  so the broker is the one that ships, with fsync on and SQLite metadata.
  - Every listener gets a free port: AMQP 0-9-1 (which also accepts AMQP 1.0), MQTT, STOMP,
    streams, the management API and the HTTP messaging API.
  - Streams advertise `host.docker.internal` as their host, so stream clients in containers can
    reconnect to it.
  - Broker logs go to `broker.log` next to the results (`RUST_LOG` overrides the level).
- With `--rabbitmq`, it starts `rabbitmq:4-management` instead, with the same ports published,
  `tests/queues/rabbitmq/enabled_plugins` (management, MQTT, STOMP, streams, shovel and
  consistent-hash) and `20-roll.conf`.
- It runs each client image against `host.docker.internal`. Containers get `AMQP_URL`,
  `MQTT_PORT`, `STOMP_PORT`, `STREAM_PORT` and `MGMT_URL`.
- A client whose scenarios fail is run once more with just those scenarios. If they pass then,
  the summary marks them `PASS (retry)`. Docker Desktop's host-gateway proxy occasionally stalls
  a TCP connect under load, and the retry keeps that from failing the run. A second failure, or
  a client that fails without naming a scenario, fails the run.
- It writes logs and a `summary.md` to `$CARGO_TARGET_DIR/queues-clients/`.
- Downloads and builds are cached in named Docker volumes (`roll-qtest-*`), so reruns are fast.
- The exit status is non-zero if any scenario, PerfTest run or suite test fails.

The cargo test `crates/mq-amqp/tests/clients.rs` runs the pika, amqplib and amqp091-go programs
against an in-process broker:

- Run it with `ROLL_E2E=1 cargo test -p mq-amqp --test clients`.
- `ROLL_E2E_CLIENTS=a,b` picks the clients.
- Without `ROLL_E2E=1`, it skips.

## Client matrix

Each client program lives in `tests/queues/clients/<name>/`. Its Dockerfile pins exact library
versions. The program prints `PASS|FAIL|SKIP <scenario>` lines and exits non-zero on any failure.

The scenarios:

| scenario | what it checks |
|---|---|
| connect | connection.open, server properties and capabilities |
| declare | exchange and queue declare, passive declare, 404/406 on inequivalent redeclare |
| publish_confirms | 100 confirmed publishes, plus a mandatory publish that comes back as basic.return |
| consume_ack | consume and ack |
| nack_requeue | nack with requeue redelivers with `redelivered=true` |
| reject_dlx | reject to a dead-letter exchange, and the `x-death` header |
| topic | topic routing with `*` and `#` |
| headers | headers exchange with `x-match` all and any |
| tx | tx.select, commit and rollback of publishes and acks |
| prefetch | basic.qos limits unacked deliveries |
| ttl | per-queue and per-message TTL expiry |
| priority | `x-max-priority` delivers higher priority messages first |
| exclusive_autodelete | another connection gets 405 on an exclusive queue; auto-delete queue goes away after its last consumer |
| direct_reply_to | RPC through `amq.rabbitmq.reply-to` |
| basic_get | basic.get and get-empty |
| cancel_notify | deleting the queue sends basic.cancel to its consumer |
| connection_blocked | the `connection.blocked` capability is advertised |
| heartbeat | a 2s heartbeat keeps an idle connection alive for 7s |
| large_message | a 5 MiB message round-trips intact |
| throughput | 10,000 messages published and consumed |

Every client passes every scenario on roll and on RabbitMQ 4.3.6.

| client | library | scenarios | roll | RabbitMQ 4.3.6 |
|---|---|---|---|---|
| python-pika | pika 1.4.4 | all 20 | 20/20 | 20/20 |
| node-amqplib | amqplib 2.2.0 | all 20 | 20/20 | 20/20 |
| go-amqp091 | amqp091-go v1.15.0 | all 20 | 20/20 | 20/20 |
| java-amqp-client | com.rabbitmq:amqp-client 5.37.0 | all 20 | 20/20 | 20/20 |
| dotnet-rabbitmq-client | RabbitMQ.Client 7.2.2 (.NET 9) | all 20 | 20/20 | 20/20 |
| ruby-bunny | bunny 3.4.0 | all 20 | 20/20 | 20/20 |
| rust-lapin | lapin 4.12.2 | all 20 | 20/20 | 20/20 |
| java-spring-amqp | Spring Boot 4.1.1 / spring-rabbit 4.1.1 | 12 framework scenarios | 12/12 | 12/12 |
| dotnet-masstransit | MassTransit.RabbitMQ 8.5.11 | 8 framework scenarios | 8/8 | 8/8 |
| python-celery | celery 5.6.3 / kombu 5.6.2 | 8 framework scenarios | 8/8 | 8/8 |
| java-amqp10-client | RabbitMQ AMQP 1.0 Java client 1.6.0 | 15 AMQP 1.0 scenarios | 15/15 | 15/15 |
| java-stream-client | RabbitMQ Stream Java client 1.12.0 | 12 stream scenarios | 12/12 | 12/12 |
| python-paho-mqtt | paho-mqtt 2.1.0 | 21 MQTT 3.1.1 and 5 scenarios | 21/21 | 21/21 |
| python-stomp | stomp.py 8.2.0 | 17 STOMP 1.2 scenarios | 17/17 | 17/17 |

The per-scenario breakdown for each run is in `summary.md`.

The framework clients run these scenarios:

- **Spring AMQP** (Spring Boot 4.1.1, spring-rabbit 4.1.1, amqp-client 5.30.0): connect, declare,
  template_send_receive, listener_ack, json_converter, publisher_confirms, returns,
  direct_reply_to, transactions, dlx, prefetch, throughput.
- **MassTransit** (MassTransit.RabbitMQ 8.5.11, RabbitMQ.Client 7.2.2): connect, publish_consume,
  send_endpoint, request_response, multiple_consumers, retry_error_queue, fault_event, throughput.
- **Celery** (celery 5.6.3, kombu 5.6.2, py-amqp 5.4.1, `rpc://` result backend, prefork pool):
  connect, task_roundtrip, many_tasks, chain, retry, task_error, acks_late, countdown.
  - The worker uses the prefork pool, not `-P threads`. With the threads pool, Celery queues acks
    from pool threads without waking its event loop. An ack can then wait up to 10s for the next
    socket event, and the worker stalls at its prefetch limit. This is a client bug and happens on
    RabbitMQ too.

The other protocols run these scenarios. Each one ends with an `amqp091_interop` or
`amqp_interop` check: messages cross between that protocol and an AMQP 0-9-1 client (pika or the
Java client) on the same queues.

- **AMQP 1.0** (`com.rabbitmq.client:amqp-client` 1.6.0, RabbitMQ's own AMQP 1.0 client, which
  uses the RabbitMQ 4 address format and HTTP-over-AMQP management): connect, topology,
  declare_mismatch, publish_outcomes (accepted, released when nothing routes, rejected by a
  full `reject-publish` queue), consume_accept, requeue (with `modified` annotations), discard_dead_letter,
  quorum_delivery_limit (`x-delivery-count`), stream_offsets, stream_filter, properties, rpc,
  single_active_consumer, amqp091_interop, throughput.
- **Streams** (`com.rabbitmq:stream-client` 1.12.0): create_delete, publish_confirm, offsets
  (first, last, next, absolute, timestamp), deduplication (publisher ids), offset_tracking,
  filtering, single_active_consumer, super_stream, sub_entry_compression (gzip),
  retention (`max-length-bytes` with small segments), amqp091_interop, throughput.
- **MQTT** (paho-mqtt 2.1.0): connect_v311, connect_v5, bad_password, qos0_qos1,
  qos2_downgraded, wildcards, retained, persistent_session, last_will, clean_disconnect_no_will,
  unsubscribe, v5_properties, v5_message_expiry, v5_session_expiry, v5_topic_alias, v5_no_local,
  v5_retain_as_published, v5_subscription_identifier, v5_no_matching_subscribers, amqp_interop,
  throughput.
- **STOMP** (stomp.py 8.2.0): connect, bad_login, queue_send_receive, topic_fanout,
  exchange_destination, amq_queue_destination, client_ack, nack_requeue, nack_dead_letter,
  transactions, durable_topic_subscription, temp_queue_reply, heartbeats, quorum_queue_header,
  large_binary, amqp_interop, throughput.

### RabbitMQ 4.3 deprecated features

Two features are denied by default in RabbitMQ 4.3: `transient_nonexcl_queues` and `global_qos`.

- roll presents itself as RabbitMQ 4.1.0, so it still allows them. Celery's `countdown` relies on
  global QoS, for example.
- The oracle runs with `tests/queues/rabbitmq/20-roll.conf`, which permits both features. This
  makes the two brokers comparable.

## RabbitMQ PerfTest

These runs use `pivotalrabbitmq/perf-test` (latest) in Docker.

- roll ran natively on macOS, with fsync done as `F_FULLFSYNC`.
- RabbitMQ 4.3.6 ran inside the Docker Desktop VM.
- The machine was shared with other builds while the runs took place.
- Treat the numbers as indicative only. They are not a benchmark.

| run | PerfTest flags | roll msg/s | roll consumer latency, median / p99 | RabbitMQ 4.3.6 msg/s | RabbitMQ median |
|---|---|---|---|---|---|
| quick | `-x 2 -y 2 -a --time 15` | 173,686 | 2.0 ms / 6.3 ms | 161,000 | 6.4 ms |
| confirms, persistent | `-x 1 -y 1 -c 100 -f persistent` | 16,331 | 6.0 ms / 10.0 ms (confirm 5.9 ms / 8.7 ms) | 36,700 | confirm 2.2 ms |
| quorum, unthrottled | `--quorum-queue` | 78,833 | 199 ms / 462 ms | 58,000 | ~1 s |
| quorum, 10k msg/s | `--quorum-queue -r 10000` | 10,529 | 4.1 ms / 22 ms | 10,300 | 3 ms |

Notes:

- **Confirms on persistent messages.** roll confirms only after a real `F_FULLFSYNC` to the
  Mac's SSD. RabbitMQ's fsync goes to the Docker VM's virtual disk, which is far cheaper. With
  `ROLL_QUEUES_FSYNC=0`, the confirm path is not disk-bound.
- **Quorum queues with an unthrottled publisher.** On both brokers, latency grows with the
  backlog. Neither broker has credit-based flow control that slows the publisher down to the
  consumer's pace. RabbitMQ throttles with credit flow, and roll does not. With `-r 10000`, both
  brokers keep latency at a few milliseconds.

## Upstream test suites

| suite | source | passed | failed | skipped | excluded |
|---|---|---|---|---|---|
| amqp091-go | amqp091-go v1.15.0 integration tests | 204 | 0 | 0 | 18 |
| pika | pika 1.4.4 `tests/acceptance` | 549 | 0 | 40 | 0 |
| java-client | rabbitmq-java-client v5.37.0 | 442 | 0 | 2 | 71 |

Runners: `tests/queues/suites/<suite>/run.sh <host> <port>`. Every exclusion is listed with its
reason in that directory's `excluded.txt`.

- **amqp091-go**: the integration tests (`-tags integration`).
  - 17 `TestConnectionRecovery*` tests are excluded. They force-close connections through the
    management HTTP API, which is hard-coded at `localhost:15672`.
  - One memory-alarm test is excluded. It needs `rabbitmqctl`.
- **pika**: `tests/acceptance`. No tests are excluded.
  - It runs through a small TCP forwarder because the tests connect to `localhost:5672`.
  - All skips are kqueue tests that can't run on Linux.
- **java-client**: the broker-facing classes from `FunctionalTestSuite`, `ServerTestSuite` and
  `ClientTestSuite`, built from source with `rabbitmq_codegen` v4.3.6.
  - A stub `rabbitmqctl` makes only teardown no-ops succeed. Every test that really needs node
    control is excluded: restart, `close_connection`, alarms, `set_policy`, users and
    permissions, or a second cluster node.
  - One exclusion is for the environment: `NoAutoRecoveryWhenTcpWindowIsFullTest` fails against
    RabbitMQ 4.3.6 too when run through the forwarder.
  - One exclusion is a deliberate difference: `SaslMechanisms#anonymousShouldSucceed` (see Known
    gaps).
  - The two skips are Netty tests that the upstream suite skips by default.
  - The same classes against RabbitMQ 4.3.6, after the same exclusions: 414 passed, 17 failed and 16 skipped. 15 of the failures cascaded from
    `PerQueueVsPerMessageTTL#restartingExpiry`, which is now excluded. The other two were
    `NoAutoRecoveryWhenTcpWindowIsFullTest` and a timing flake in
    `DeadLetterExchange#deadLetterQueueTTLPromptExpiry`. The 16 skips are global-QoS tests marked
    `@BrokerVersionAtMost(4.2)`; they run against roll, which reports 4.1.0.

## Bugs found and fixed

The suites and clients turned up these bugs in roll. Each one was fixed in `mq-amqp` unless it
says otherwise.

| # | symptom | root cause and fix |
|---|---|---|
| 1 | Broker RSS grew to 12 GB and macOS killed it (Java `FrameMax` tests) | A frame larger than frame_max was never consumed from the read buffer. The reader re-parsed the same bytes forever, and each pass queued another connection.close on the unbounded writer channel. The 3s close timeout was a `sleep` that was recreated on every loop iteration, so it never fired. Now the broker sends the close and hangs up, like RabbitMQ, and the timeout is a fixed deadline. Regression test: `compat::oversized_frame_closes_connection_once`. |
| 2 | Live Java connections were dropped as "missed heartbeats" | The Java client sends heartbeats only when the connection is idle and checks once per interval, so its frames can arrive up to 2 intervals apart. roll closed the connection after exactly 2 silent intervals. roll now uses RabbitMQ's check: every interval it looks for any received bytes (partial frames count), and closes after 2 silent checks in a row. |
| 3 | A peer that stopped reading pinned a writer task forever | Socket writes had no timeout. They now use RabbitMQ's `send_timeout` of 15s; when it expires, the connection is dropped. |
| 4 | `frame_max` below 4096, or above the server's maximum, was silently clamped | It is now refused with 530 like RabbitMQ. Frames before tune-ok are limited to 4096 bytes (`FRAME_MIN_SIZE`). |
| 5 | A content header with the wrong class id was accepted | Now 505 UNEXPECTED_FRAME. |
| 6 | A passive declare of another connection's exclusive queue succeeded | Exclusivity is now checked before the declare (405). Redeclaring with a different `exclusive` flag is also 405, not 406. |
| 7 | declare-ok `message_count` missed messages that had just been published or confirmed | The counters were read without a round trip to the queue actor. **mq-core**: `redeclare` now does a `consumers()` round trip, and the queue publishes its stats first. |
| 8 | `channel.flow{active=false}` was accepted | RabbitMQ refuses it with 540 NOT_IMPLEMENTED. roll now does the same. |
| 9 | Crossing `channel.close`s ended in a 504 connection error | When a client's close crossed the server's, roll dropped the reply. roll now answers close-ok and ignores close-ok on unknown channels. |
| 10 | An unknown exchange type was a 503 connection error | RabbitMQ 4 returns it as a 406 channel error. **mq-core** `exchange.rs` now does the same. |
| 11 | After a connection error, a channel kept answering (`basic.qos-ok` arrived before `connection.close`) | Once a channel raises a connection error, it goes silent. This affected `immediate=true`, for example. |
| 12 | Acks in a transaction were not validated until commit. `basic.recover` redelivered acked-but-uncommitted messages. A multiple ack of an unknown tag was accepted | Acks, nacks and rejects now take their tags at once and return 406 on unknown tags, like RabbitMQ's `collect_acks`. They are applied on commit and restored on rollback. If the channel closes, they requeue. |
| 13 | Global (per-channel) prefetch could overshoot | Queues check credit concurrently, so the limit could be exceeded. Other problems: basic.get deliveries counted against prefetch, no-ack consumers ignored a full window, and the same queue always won freed credit. Now the window is checked and reserved under the channel lock; overshooting deliveries are parked and sent as acks free credit. Gets don't count, and the queue that wakes first rotates. |
| 14 | Names containing CR or LF were not normalised | They are now stripped on exchange and queue declare, like RabbitMQ's `strip_cr_lf`. |
| 15 | `queue.bind` and `queue.unbind` with an empty queue name and an empty routing key used `""` as the key | The key is now the name of the last declared queue (AMQP shortcut). |
| 16 | Unbinding a missing queue, exchange or binding was a 404 | It now succeeds, like RabbitMQ 3.x and later. |
| 17 | Some arguments were not validated | A non-array `CC` or `BCC` header, a non-integer `x-priority`, and a non-string `x-dead-letter-exchange` or `x-dead-letter-routing-key` were accepted. They are now 406. |
| 18 | Redeclaring `x-message-ttl` 10 as 10.0 was accepted | Arguments of a different type class are now 406, like RabbitMQ. |
| 19 | Messages with TTL 0 didn't expire | **mq-core** `engine.rs`: a message with TTL 0 that no consumer takes at once now expires, and a requeued one expires as well. |
| 20 | basic.get could see messages that should already have gone to a waiting consumer, been dropped by `x-max-length` (including 0), or expired | The publish and the get were handled in the same batch, before the batch's deliver and limit pass. **mq-core** `engine.rs`: the queue now runs deliver, expiry and limits before serving a get. |
| 21 | Direct reply-to failed in two cases | Consuming again after a cancel failed, and a passive declare of an `amq.rabbitmq.reply-to.*` name returned 404. Both now work. |
| 22 | `x-death` headers set by the publisher became dead-letter history | Since RabbitMQ 3.13, the history belongs to the broker. Publisher-set `x-death` headers still reach consumers unchanged, but a dead-lettered copy starts a fresh history. |
| 23 | The RabbitMQ AMQP 1.0 Java client refused to connect ("broker node name is not available") | **mq-amqp10**: the `open` frame's properties now include `node`, like RabbitMQ 4. |
| 24 | The AMQP 1.0 Java client failed with "uncorrelated handle on Detach" after a refused attach | When roll closed a link, it answered the client's reply detach with another detach. **mq-amqp10** now remembers the links it closed and doesn't echo their replies. |
| 25 | The `modified` outcome lost its message annotations, and `delivery-failed` didn't count the delivery | **mq-core**: a new `Modify` queue command merges `x-opt-*` annotations into the message, counts the delivery when `delivery-failed` is set, and dead-letters when `undeliverable-here` is set, like RabbitMQ 4. A new log record keeps the changed message across a restart or crash. |
| 26 | Quorum deliveries over AMQP 1.0 had no `x-delivery-count` annotation | **mq-amqp10** now adds it when the count is non-zero. |
| 27 | Stream publishers with sub-entry compression were refused | **mq-stream** now decompresses gzip, snappy, lz4 and zstd batches (with a 64 MiB limit) and stores plain entries. |
| 28 | Stream retention never removed anything when every write went to one segment | **mq-core** `stream.rs`: segments now roll at `x-stream-max-segment-size-bytes`, and retention runs on every roll. |

## Known gaps and differences

- **SASL ANONYMOUS is not offered** (Java `SaslMechanisms#anonymousShouldSucceed`).
  - RabbitMQ 4 offers it by default and maps it to `guest`.
  - roll offers only `PLAIN` and `AMQPLAIN`. This is deliberate, so that a broker reachable from a
    network never accepts unauthenticated logins.
- **No credit-based flow control for publishers.**
  - A publisher that outruns its consumers builds a backlog, so latency grows. You can see this in
    the unthrottled quorum PerfTest run.
  - Memory and disk alarms still block publishers through `connection.blocked`, with
    RabbitMQ's default thresholds: resident memory above 60% of the memory available (a
    container's limit counts) or less than 50 MB free on the data disk. The memory alarm
    needs Linux. Alarms show in `/api/nodes` and in the `alarms` health checks.
- **.NET RabbitMQ.Client 7.2.2 connection-open stall.** About once in 10–15 full runs during
  development, opening a connection stalled for 15s. It was not reproduced in 12 of 12 focused
  runs, and the scenarios passed when it happened.
- **macOS fsync.** roll uses `F_FULLFSYNC`, so confirm latency on persistent messages on a Mac is
  far higher than on Linux or in a VM.
- **MQTT QoS 2** is downgraded to QoS 1, as in RabbitMQ (`qos2_downgraded`).
