# Drizzle ORM 0.45.4 (TypeScript): integration-tests' PostgreSQL files (the node-postgres,
# postgres.js, custom and proxy drivers' shared pg-common suite, and the relational query
# tests for node-postgres and postgres.js) with PG_CONNECTION_STRING at the a51_drizzle
# database, after building the workspace packages they import (drizzle-orm and the
# seed / zod / typebox / valibot dists). vitest's junit reporter.
# image: node:22-bookworm-slim
# src: https://github.com/drizzle-team/drizzle-orm/archive/refs/tags/0.45.4.tar.gz
set -eu
export npm_config_cache=/cache/home/npm COREPACK_HOME=/cache/home/corepack
export COREPACK_ENABLE_DOWNLOAD_PROMPT=0 npm_config_store_dir=/cache/home/pnpm-store
export PRISMA_HIDE_UPDATE_MESSAGE=1 CHECKPOINT_DISABLE=1 TURBO_TELEMETRY_DISABLED=1
PNPM="corepack pnpm"
# turbo and the packages' scripts call `pnpm` by name (no corepack shims as non-root)
mkdir -p /cache/home/bin
printf '#!/bin/sh\nexec corepack pnpm "$@"\n' > /cache/home/bin/pnpm && chmod +x /cache/home/bin/pnpm
export PATH="/cache/home/bin:$PATH"
if [ ! -d node_modules ]; then
  $PNPM install --frozen-lockfile > "$OUT/install.log" 2>&1 || { tail -30 "$OUT/install.log"; exit 1; }
fi
if [ ! -d drizzle-orm/dist ] || [ ! -d drizzle-zod/dist ]; then
  $PNPM exec turbo run build --filter drizzle-orm --filter drizzle-seed --filter drizzle-zod \
    --filter drizzle-typebox --filter drizzle-valibot > "$OUT/build.log" 2>&1 ||
    { tail -40 "$OUT/build.log"; exit 1; }
fi
cd integration-tests
export PG_CONNECTION_STRING="postgres://postgres:pw@$PGHOST:$PGPORT/a51_drizzle"
export SKIP_EXTERNAL_DB_TESTS=1
timeout 3600 $PNPM exec vitest run --reporter=junit --outputFile="$OUT/drizzle.xml" \
  tests/pg/node-postgres.test.ts tests/pg/postgres-js.test.ts tests/pg/pg-custom.test.ts \
  tests/pg/pg-proxy.test.ts tests/relational/pg.test.ts tests/relational/pg.postgresjs.test.ts \
  "$@" > "$OUT/vitest.log" 2>&1 || true
grep -c '<testcase' "$OUT/drizzle.xml" || tail -30 "$OUT/vitest.log"
