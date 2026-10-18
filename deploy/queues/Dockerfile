# roll queues in one small image: broker node (`rolld`, queues only), front proxy (`mq-proxy`)
# and log keeper (`roll-keeper`).
# Build from the repository root:
#   docker build -f deploy/queues/Dockerfile -t roll-queues .
# Multi-arch for Graviton:
#   docker buildx build --platform linux/arm64 -f deploy/queues/Dockerfile -t roll-queues .
FROM rust:1-bookworm AS build
# `dev` builds faster for tests; images you deploy use `release`.
ARG PROFILE=release
# rolld also links roll tasks, whose SQLite session support runs bindgen (libclang) at build time.
RUN apt-get update && apt-get install -y --no-install-recommends libclang-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} cargo build --profile "$PROFILE" -p rolld -p mq-proxy -p mq-storage --bins \
 && dir=$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE") \
 && cp "target/$dir/rolld" "target/$dir/mq-proxy" "target/$dir/roll-keeper" /usr/local/bin/

FROM debian:bookworm-slim
# Amazon RDS's CA bundle, so metadata on RDS works with sslmode=require (added to the system roots).
ADD https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem /etc/roll/rds-global-bundle.pem
RUN chmod 755 /etc/roll && chmod 644 /etc/roll/rds-global-bundle.pem \
 && apt-get update && apt-get install -y --no-install-recommends ca-certificates tini && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /data --create-home roll \
 && mkdir -p /shared && chown roll /shared
COPY --from=build /usr/local/bin/rolld /usr/local/bin/mq-proxy /usr/local/bin/roll-keeper /usr/local/bin/
USER roll
WORKDIR /data
ENV ROLL_HOME=/data \
    ROLL_QUEUES_ONLY=1 \
    ROLL_API=0.0.0.0:7070 \
    ROLL_QUEUES_AMQP=0.0.0.0:5672 \
    ROLL_QUEUES_STREAM=0.0.0.0:5552 \
    ROLL_QUEUES_MANAGEMENT=0.0.0.0:15672 \
    ROLL_QUEUES_STOMP=0.0.0.0:61613 \
    ROLL_QUEUES_WEB_STOMP=0.0.0.0:15674 \
    ROLL_QUEUES_MQTT=0.0.0.0:1883 \
    ROLL_QUEUES_WEB_MQTT=0.0.0.0:15675 \
    ROLL_QUEUES_HTTP=0.0.0.0:8080 \
    ROLL_QUEUES_RPC=0.0.0.0:25672 \
    ROLL_QUEUES_CLUSTER_META_CA=/etc/roll/rds-global-bundle.pem
EXPOSE 5672 5552 15672 61613 15674 1883 15675 8080 7070 25672
VOLUME /data
# tini forwards SIGTERM so a stopping task drains: queues move, connections close gradually.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["rolld"]
