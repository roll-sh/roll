# roll code in one small image: `rolld --code-only`, one node of a roll code cluster (or a single
# node). Every node runs this same image against the same Postgres and S3 (deploy/code/README.md).
# Build from the repository root:
#   docker build -f deploy/code/Dockerfile -t roll-code .
# Multi-arch for Graviton:
#   docker buildx build --platform linux/arm64 -f deploy/code/Dockerfile -t roll-code .
FROM rust:1-trixie AS build
# `dev` builds faster for tests; images you deploy use `release`.
ARG PROFILE=release
ARG CARGO_BUILD_JOBS=8
# rolld also links roll tasks, whose SQLite session support runs bindgen (libclang) at build time.
RUN apt-get update && apt-get install -y --no-install-recommends libclang-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
# Dev builds use line tables only (a smaller build cache); the binary is stripped of debug info
# either way to keep the image small (backtraces still name functions).
RUN --mount=type=cache,id=roll-code-target,sharing=locked,target=/src/target \
    --mount=type=cache,id=roll-cargo-registry,sharing=locked,target=/usr/local/cargo/registry \
    CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} \
    cargo build --profile "$PROFILE" -p rolld --bin rolld \
 && dir=$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE") \
 && cp "target/$dir/rolld" /usr/local/bin/rolld \
 && strip --strip-debug /usr/local/bin/rolld

FROM debian:trixie-slim
# Amazon RDS's CA bundle, so Postgres on RDS works with sslmode=require/verify-full
# (ROLL_CODE_DB_CA adds it to the system roots).
ADD https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem /etc/roll/rds-global-bundle.pem
# tini forwards SIGTERM so a stopping node drains. No docker CLI: roll's own checks and previews
# (`roll.toml`) don't run on these nodes (README.md, "Checks and previews").
RUN chmod 755 /etc/roll && chmod 644 /etc/roll/rds-global-bundle.pem \
 && apt-get update && apt-get install -y --no-install-recommends ca-certificates tini && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /data --create-home --uid 10001 roll
COPY --from=build /usr/local/bin/rolld /usr/local/bin/rolld
# The health check: GET /v1/code/health on the API port, without curl (bash's /dev/tcp).
COPY --chmod=755 deploy/code/health.sh /usr/local/bin/roll-code-health
USER roll
WORKDIR /data
# The API (and git) port. Set ROLL_CODE_DATABASE, ROLL_S3_BUCKET (+ AWS_REGION, credentials or
# the instance role), ROLL_CODE_TOKEN, ROLL_SECRET_KEY and, for a cluster,
# ROLL_CODE_CLUSTER_SECRET and ROLL_CODE_ADVERTISE_URL.
ENV ROLL_HOME=/data \
    ROLL_CODE_ONLY=1 \
    ROLL_API=0.0.0.0:7070 \
    ROLL_CODE_DB_CA=/etc/roll/rds-global-bundle.pem
EXPOSE 7070
# Local state is a cache (work trees, the code index); everything durable is in Postgres and S3.
VOLUME /data
HEALTHCHECK --interval=5s --timeout=3s --start-period=30s --retries=2 CMD ["/usr/local/bin/roll-code-health"]
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["rolld"]
