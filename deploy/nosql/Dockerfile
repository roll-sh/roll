# roll/nosql: a drop-in for `cassandra:5.0` and `scylladb/scylla` in local development, CI and
# Testcontainers (docs/nosql/local-dev.md). One database in single mode, ready in well under a
# second, data in /var/lib/roll (local directory, or S3 with ROLL_S3_BUCKET).
#
# Build from the repository root:
#   docker build -f deploy/nosql/Dockerfile -t roll/nosql .
# Multi-arch:
#   docker buildx build --platform linux/amd64,linux/arm64 -f deploy/nosql/Dockerfile -t roll/nosql .
# Locally, deploy/nosql/build-local.sh builds the binaries with a capped container and packs
# them with Dockerfile.prebuilt (the same final stage).
FROM rust:1-bookworm AS build
# `dev` builds faster for tests; images you publish use `release`.
ARG PROFILE=release
# rolld also links roll tasks, whose SQLite session support runs bindgen (libclang) at build time.
RUN apt-get update && apt-get install -y --no-install-recommends libclang-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS:-4} cargo build --locked --profile "$PROFILE" -p rolld -p roll-cli --bin rolld --bin roll \
 && dir=$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE") \
 && mkdir -p /out && cp "target/$dir/rolld" "target/$dir/roll" /out/ \
 && strip /out/rolld /out/roll

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tini && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 999 --user-group --home-dir /var/lib/roll roll \
 && mkdir -p /var/lib/roll /etc/cassandra /etc/scylla && chown roll:roll /var/lib/roll
COPY --from=build /out/rolld /out/roll /usr/local/bin/
COPY deploy/nosql/docker-entrypoint.sh deploy/nosql/cqlsh deploy/nosql/healthcheck /usr/local/bin/
USER roll
# / as in the reference images: Testcontainers copies init scripts to a relative path.
WORKDIR /
ENV ROLL_HOME=/var/lib/roll RUST_LOG=info NO_COLOR=1
# 9042 CQL, 19042 ScyllaDB shard-aware CQL, 8000 Alternator (DynamoDB API), 10000 ScyllaDB REST
# (nodetool), 9180 Prometheus, 7070 the roll API (/v1/nosql, the dashboard and `roll nosql`).
EXPOSE 9042 19042 8000 10000 9180 7070
VOLUME /var/lib/roll
HEALTHCHECK --interval=1s --timeout=3s --start-period=10s --retries=30 CMD ["healthcheck"]
# tini forwards SIGTERM: rolld makes every acknowledged write durable before it exits.
ENTRYPOINT ["/usr/bin/tini", "--", "docker-entrypoint.sh"]
