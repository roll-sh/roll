# Knex 3.3.0 (Node query builder): its mocha integration tests (test:db's config: prep-db,
# integration-test-suite and integration2) for the postgres client only (DB=postgres), with a
# KNEX_TEST config pointing at the server's knex_test database as testuser / knextest (its
# docker-compose service). Knex has no lockfile: npm resolves package.json's ranges once, and
# the cached node_modules is reused. mocha's xunit reporter.
# image: node:22-bookworm-slim
# src: https://github.com/knex/knex/archive/refs/tags/3.3.0.tar.gz
set -eu
export npm_config_cache=/cache/home/npm npm_config_update_notifier=false npm_config_fund=false
if [ ! -d node_modules ]; then
  npm install --ignore-scripts --no-audit > "$OUT/install.log" 2>&1 ||
    { tail -30 "$OUT/install.log"; exit 1; }
fi
# The test files load sqlite3 / better-sqlite3 whatever DB is: fetch their prebuilt bindings.
if [ ! -f .sqlite-bindings ]; then
  npm rebuild sqlite3 better-sqlite3 > "$OUT/rebuild.log" 2>&1 ||
    { tail -30 "$OUT/rebuild.log"; exit 1; }
  touch .sqlite-bindings
fi
cat > "$OUT/knex-test.js" <<EOF
module.exports = { postgres: { adapter: 'postgresql', host: '$PGHOST', port: $PGPORT,
  database: 'knex_test', user: 'testuser', password: 'knextest' } };
EOF
export DB=postgres KNEX_TEST="$OUT/knex-test.js"
timeout 5400 node_modules/.bin/mocha --exit -t 10000 --config test/mocha-integration-config-test.js \
  --reporter xunit --reporter-options output="$OUT/knex.xml" "$@" > "$OUT/mocha.log" 2>&1 || true
grep -c '<testcase' "$OUT/knex.xml" || tail -30 "$OUT/mocha.log"
