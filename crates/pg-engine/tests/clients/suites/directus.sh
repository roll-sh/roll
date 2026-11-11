# Directus 12.6.0 (npm) with the server as its database (directus_app): its migrations and
# system collections at bootstrap, then what its API does to the database: collections and
# fields made, altered and dropped (DDL through its schema layer, knex), relations (foreign
# keys), items read with filters, sorts, deep relational fields, search, aggregates and
# groupings, written and deleted, the errors constraints raise, and the schema snapshot it
# reads from the catalog (suites/directus.mjs). One id per step; answers in
# $OUT/responses.txt, Directus's log in $OUT/directus.log. APP_PORT: its port (52454).
# image: node:26-bookworm-slim
# src:
set -eu
D=/cache/src/directus
if [ ! -x $D/node_modules/.bin/directus ]; then
  mkdir -p $D && cd $D
  [ -f package.json ] || echo '{"name":"directus-run","private":true}' > package.json
  npm install --no-audit --no-fund --loglevel=error directus@12.6.0 > "$OUT/install.log" 2>&1 || { tail -20 "$OUT/install.log"; exit 1; }
fi
W=/tmp/directus
rm -rf $W && mkdir -p $W/uploads $W/extensions && cd $W
export DB_CLIENT=pg DB_HOST=$PGHOST DB_PORT=$PGPORT DB_DATABASE=directus_app DB_USER=postgres DB_PASSWORD=pw
export KEY=a122-key SECRET=a122-secret-0123456789abcdef PUBLIC_URL=http://127.0.0.1:${APP_PORT:-52454}
export HOST=127.0.0.1 PORT=${APP_PORT:-52454} ADMIN_EMAIL=admin@example.com ADMIN_PASSWORD=admin-pass-123
export TELEMETRY=false STORAGE_LOCATIONS=local STORAGE_LOCAL_ROOT=$W/uploads EXTENSIONS_PATH=$W/extensions
export CACHE_ENABLED=false RATE_LIMITER_ENABLED=false LOG_LEVEL=warn ACCESS_TOKEN_TTL=2h
$D/node_modules/.bin/directus bootstrap > "$OUT/directus.log" 2>&1 || { tail -20 "$OUT/directus.log"; }
$D/node_modules/.bin/directus start >> "$OUT/directus.log" 2>&1 &
P=$!
node /harness/suites/directus.mjs || true
kill $P 2>/dev/null || true
wait $P 2>/dev/null || true
grep -c '^PASS' "$OUT/directus.lst" || true
