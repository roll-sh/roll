# Sequelize 6.37.8 (the v6 line, Node ORM): its mocha unit tests and integration tests with
# DIALECT=postgres (as its CI's test-postgres job, non-native, unminified aliases), the
# sequelize_test superuser / database of its CI service, the dist built by build.js, mocha's
# xunit reporter (unit.xml and integration.xml). Its CI service image (sushantdhiman/postgres)
# has PostGIS; these servers don't, so the geometry / geography tests fail on both.
# image: node:22-bookworm-slim
# src: https://github.com/sequelize/sequelize/archive/refs/tags/v6.37.8.tar.gz
set -eu
export YARN_CACHE_FOLDER=/cache/home/yarn npm_config_cache=/cache/home/npm
if [ ! -d node_modules ]; then
  yarn install --frozen-lockfile --ignore-engines --ignore-scripts --non-interactive \
    > "$OUT/install.log" 2>&1 || { tail -30 "$OUT/install.log"; exit 1; }
fi
if [ ! -d lib ] || [ ! -f lib/index.js ]; then
  node build.js > "$OUT/build.log" 2>&1 || { tail -30 "$OUT/build.log"; exit 1; }
fi
export DIALECT=postgres SEQ_PG_HOST="$PGHOST" SEQ_PG_PORT="$PGPORT"
export SEQ_PG_USER=sequelize_test SEQ_PG_PW=sequelize_test SEQ_PG_DB=sequelize_test
MOCHA="node_modules/.bin/mocha -r ./test/registerEsbuild --reporter xunit"
if [ $# -eq 0 ]; then
  timeout 900 $MOCHA --reporter-options output="$OUT/unit.xml" "test/unit/**/*.test.[tj]s" \
    > "$OUT/unit.log" 2>&1 || true
  set -- "test/integration/**/*.test.[tj]s"
fi
timeout 5400 $MOCHA --reporter-options output="$OUT/integration.xml" "$@" \
  > "$OUT/integration.log" 2>&1 || true
grep -c '<testcase' "$OUT/unit.xml" "$OUT/integration.xml" || tail -30 "$OUT/integration.log"
