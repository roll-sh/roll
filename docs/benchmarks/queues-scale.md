# What idle queues and connections cost: roll queues vs RabbitMQ 4

Run by `scripts/scale-queues.sh` on Darwin arm64, 2026-10-17. Each broker runs alone in
a container limited to 4 CPUs and 8g. Queues are durable classic queues on one vhost, declared
over one connection; connections are idle AMQP 0-9-1 connections with one open channel each and
heartbeats off. Memory is the container's anonymous memory (process heaps, not page cache). Idle
CPU is CPU-seconds per minute over 30s with no traffic.

| broker | queues | connections | empty | with queues | with queues + connections | per queue | per connection | declare rate |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| RabbitMQ 4 | 20000 | 5000 | 121 MiB | 1124 MiB | 1421 MiB | 51.3 KiB | 60.9 KiB | 1176/s |
| roll queues | 20000 | 5000 | 20 MiB | 156 MiB | 332 MiB | 7.0 KiB | 36.0 KiB | 2222/s |

Idle CPU, in CPU-seconds per minute with no traffic. The method has a floor of about 0.02: a
container that only sleeps measures that much, because reading its counters runs `docker exec`
inside it. A roll broker with no clients parks all background work (see
[architecture.md](../queues/architecture.md#idle-cost)); with clients connected its samplers and
alarm checks run.

| broker | empty | with queues | with queues + idle connections | clients gone, queues loaded |
| --- | ---: | ---: | ---: | ---: |
| RabbitMQ 4 | 0.25 | 3.65 | 6.58 | 3.94 |
| roll queues | 0.02 | 0.02 | 0.22 | 0.03 |

Raw numbers are in `target/scale-queues/`. A sleeping vhost (scale to zero) holds no memory
for its queues at all; they are loaded again on the next connection.
