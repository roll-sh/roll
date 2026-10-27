# Kysely 0.29.7 (TypeScript query builder): its node mocha tests (test:node: tsc of src and
# test/node, then test/node/dist/**/*.test.js) for the postgres dialect only
# (DIALECTS=postgres), as kysely / kysely on the kysely_test database (its docker-compose
# service). test-setup.ts hard-codes localhost:5434; the suite rewrites those two lines to read
# PGHOST / PGPORT before compiling. mocha's xunit reporter.
# image: node:22-bookworm-slim
# src: https://github.com/kysely-org/kysely/archive/refs/tags/v0.29.7.tar.gz
set -eu
export npm_config_cache=/cache/home/npm COREPACK_HOME=/cache/home/corepack
export COREPACK_ENABLE_DOWNLOAD_PROMPT=0 npm_config_store_dir=/cache/home/pnpm-store
PNPM="corepack pnpm"
mkdir -p /cache/home/bin
printf '#!/bin/sh\nexec corepack pnpm "$@"\n' > /cache/home/bin/pnpm && chmod +x /cache/home/bin/pnpm
export PATH="/cache/home/bin:$PATH"
if [ ! -d node_modules ]; then
  $PNPM install --frozen-lockfile --ignore-scripts > "$OUT/install.log" 2>&1 ||
    { tail -30 "$OUT/install.log"; exit 1; }
fi
sed -i -e "/^const POSTGRES_CONFIG/,/^}/{s/host: 'localhost'/host: process.env.PGHOST!/;s/port: 5434/port: Number(process.env.PGPORT)/}" \
  test/node/src/test-setup.ts
if [ ! -f test/node/dist/test-setup.js ] && [ ! -d test/node/dist/src ]; then
  { $PNPM run build && $PNPM run test:node:build; } > "$OUT/build.log" 2>&1 ||
    { tail -30 "$OUT/build.log"; exit 1; }
fi
export DIALECTS=postgres
[ $# -gt 0 ] || set -- "test/node/dist/**/*.test.js"
timeout 5400 node_modules/.bin/mocha --timeout 15000 --reporter xunit \
  --reporter-options output="$OUT/kysely.xml" "$@" > "$OUT/mocha.log" 2>&1 || true
grep -c '<testcase' "$OUT/kysely.xml" || tail -30 "$OUT/mocha.log"
