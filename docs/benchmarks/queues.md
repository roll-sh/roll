# roll queues vs RabbitMQ 4

PerfTest (pivotalrabbitmq/perf-test:latest) against each broker in a container limited to 4 CPUs and 4g,
30s per run, 1 KB messages, run by `scripts/bench-queues.sh` on Darwin arm64, 2026-10-17.
RabbitMQ image: `rabbitmq:4-management`. Every run starts a fresh broker, the brokers take turns, and each
scenario runs 3 times per broker; the table shows the median of each number, and the
range of p99 over the runs. Rates are received msg/s (higher is better), with the sent rate
when the consumers fell behind; latency is consumer latency in ms (lower is better).

| scenario | roll msg/s | RabbitMQ msg/s | roll p50 / p99 ms | RabbitMQ p50 / p99 ms | p99 range: roll · RabbitMQ |
| --- | ---: | ---: | ---: | ---: | ---: |
| transient, 1 producer → 1 consumer, auto-ack | 386.2k | 283.3k | 0.1 / 0.3 | 1.3 / 5.0 | 0.2–0.3 · 3.9–7.0 |
| classic durable, confirms, manual ack | 149.3k (sent 170.2k) | 88.1k | 3077.9 / 5068.5 | 1.0 / 2.0 | 4373.1–5328.1 · 2.0–2.1 |
| quorum, confirms, manual ack | 180.2k | 71.3k (sent 80.2k) | 130.4 / 244.3 | 1879.5 / 3363.7 | 145.6–256.5 · 3009.0–3955.7 |
| quorum, 4 producers → 4 consumers | 304.9k | 110.5k | 2.6 / 7.7 | 7.6 / 18.6 | 6.2–13.1 · 18.4–29.4 |
| fan-out to 10 classic queues | 509.8k | 90.9k | 1.4 / 4.2 | 20.5 / 105.2 | 3.9–4.3 · 87.1–132.8 |
| 100 queues, 100 producers, 50 msg/s each | 5.0k | 5.0k | 0.4 / 1.2 | 0.8 / 3.5 | 0.7–1.7 · 1.6–17.1 |
| latency at 1000 msg/s (quorum) | 1.0k | 1.0k | 0.5 / 2.2 | 0.4 / 3.6 | 0.9–9.5 · 1.2–27.1 |
| classic durable, consumer acks every 10th message | 210.4k | 91.0k | 0.2 / 1.0 | 0.9 / 1.9 | 1.0–1.1 · 1.7–2.1 |

Single-consumer scenarios at full speed show what happens when the publisher outruns the
consumer: the backlog grows for the whole run and latency is its age, not delivery time. Here
that is roll's classic queue with a consumer acking every message, and RabbitMQ's quorum queue:
the rows that show a sent rate. Prefetch is a promise to the consumer, so neither broker holds
deliveries back to batch them. When the consumer acks every 10th message (`multiple`, the
last scenario), roll's keeps up.

The host's 1-minute load average ranged over 3.71–10.35 during the run. With
other work on the machine, tail latency in the rate-limited scenarios is dominated by the
host (fsync jitter of the VM's disk): compare the ranges, not just the medians.

Peak broker memory at the end of a run: roll 2290 MiB, RabbitMQ 1107 MiB.

Raw PerfTest output is in `target/bench-queues/`. Scenario arguments:

- transient, 1 producer → 1 consumer, auto-ack: `-x 1 -y 1 -s 1000 -u bench-t -f transient --auto-delete true -a`
- classic durable, confirms, manual ack: `-x 1 -y 1 -s 1000 -u bench-c -ad false -f persistent -c 200 -q 200 --queue-args x-queue-type=classic`
- quorum, confirms, manual ack: `-x 1 -y 1 -s 1000 -u bench-q -ad false -f persistent -c 200 -q 200 --queue-args x-queue-type=quorum`
- quorum, 4 producers → 4 consumers: `-x 4 -y 4 -s 1000 -u bench-q4 -ad false -f persistent -c 200 -q 200 --queue-args x-queue-type=quorum`
- fan-out to 10 classic queues: `-x 1 -y 10 -s 1000 -t fanout -e bench-fan -f persistent -c 200 -q 200`
- 100 queues, 100 producers, 50 msg/s each: `-x 100 -y 100 -s 1000 --queue-pattern bench-many-%d --queue-pattern-from 1 --queue-pattern-to 100 -ad false -r 50 -f persistent -c 50 -q 50 --queue-args x-queue-type=quorum`
- latency at 1000 msg/s (quorum): `-x 1 -y 1 -s 1000 -u bench-lat -ad false -r 1000 -f persistent -c 50 -q 50 --queue-args x-queue-type=quorum`
- classic durable, consumer acks every 10th message: `-x 1 -y 1 -s 1000 -u bench-c10 -ad false -f persistent -c 200 -q 200 --multi-ack-every 10 --queue-args x-queue-type=classic`
