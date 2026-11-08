# syntax=docker/dockerfile:1
# The roll cloud fleet image: `rolld` with every product, and the dashboard's production server.
# Every fleet node runs it twice, as the two containers of one ECS task (deploy/cloud/README.md):
#
#   rolld       the default command
#   dashboard   `dashboard`: the dashboard on 127.0.0.1:3000, which rolld proxies to
#
# Build from the repository root (it uses deploy/cloud/Dockerfile.dockerignore):
#   docker build -f deploy/cloud/Dockerfile -t roll-cloud .
# For the fleet (Graviton), on an arm64 host or with buildx:
#   docker buildx build --platform linux/arm64 -f deploy/cloud/Dockerfile -t roll-cloud .
#
# Layers are ordered for the cache: Rust dependencies (cargo-chef) and node_modules are only
# rebuilt when Cargo.lock or pnpm-lock.yaml changes.
ARG RUST_IMAGE=rust:1-trixie
ARG NODE_IMAGE=node:24-trixie-slim

# ── rolld ──
FROM ${RUST_IMAGE} AS chef
# rolld links roll tasks, whose SQLite session support runs bindgen (libclang) at build time.
RUN apt-get update && apt-get install -y --no-install-recommends libclang-dev && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked --version 0.1.78
WORKDIR /src

FROM chef AS plan
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY vendor vendor
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS rolld
# `dev` builds faster for a local smoke test; images the fleet runs use `release`.
ARG PROFILE=release
# Unset: every core. CI and shared machines pass a number.
ARG CARGO_BUILD_JOBS
ENV CARGO_PROFILE_DEV_DEBUG=line-tables-only CARGO_INCREMENTAL=0
COPY --from=plan /src/recipe.json recipe.json
RUN cargo chef cook --profile "$PROFILE" --recipe-path recipe.json -p rolld
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY vendor vendor
RUN cargo build --profile "$PROFILE" -p rolld --bin rolld \
 && dir=$([ "$PROFILE" = dev ] && echo debug || echo "$PROFILE") \
 && cp "target/$dir/rolld" /usr/local/bin/rolld \
 && strip --strip-debug /usr/local/bin/rolld

# ── the dashboard ──
FROM ${NODE_IMAGE} AS dashboard
WORKDIR /src
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
# The pnpm version the repository pins (`packageManager`).
RUN npm install -g "$(node -p 'require("./package.json").packageManager')"
COPY packages packages
COPY apps/dashboard/package.json apps/dashboard/package.json
RUN pnpm install --frozen-lockfile --filter dashboard...
COPY apps/dashboard apps/dashboard
# The API as browsers reach it, built into the client. The server reaches rolld at ROLL_API_URL.
ARG VITE_ROLL_API=https://api.roll.sh
RUN pnpm --filter dashboard build \
 && pnpm --filter dashboard deploy --prod --legacy /out \
 && cp -r apps/dashboard/dist /out/dist

# ── the image ──
FROM ${NODE_IMAGE}
# Amazon RDS's CA bundle, so an outside Postgres on RDS works with sslmode=verify-full.
ADD https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem /etc/roll/rds-global-bundle.pem
# tini forwards SIGTERM so a stopping node drains; curl reads instance metadata at start.
RUN chmod 755 /etc/roll && chmod 644 /etc/roll/rds-global-bundle.pem \
 && apt-get update && apt-get install -y --no-install-recommends ca-certificates tini curl && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /data --create-home --uid 10001 roll
COPY --from=rolld /usr/local/bin/rolld /usr/local/bin/rolld
COPY --from=dashboard /out /opt/roll/dashboard
COPY --chmod=755 deploy/cloud/entrypoint.sh /usr/local/bin/roll-cloud-entrypoint
COPY --chmod=755 deploy/cloud/health.sh /usr/local/bin/roll-health
USER roll
WORKDIR /data
# The rest of the node's settings come from the task definition (docs/cloud/README.md, "Node
# settings") and, for ROLL_NODE_ID, ROLL_ZONE and ROLL_ADVERTISE_URL, from instance metadata.
ENV ROLL_HOME=/data \
    ROLL_API=0.0.0.0:7070 \
    ROLL_CODE_DB_CA=/etc/roll/rds-global-bundle.pem \
    NODE_ENV=production
EXPOSE 8443 5432 6379 9092 9042 5671 8883 61614 5551 7070
# Local state is a cache; everything durable is in S3, DynamoDB and the system database.
VOLUME /data
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/roll-cloud-entrypoint"]
CMD ["rolld"]
