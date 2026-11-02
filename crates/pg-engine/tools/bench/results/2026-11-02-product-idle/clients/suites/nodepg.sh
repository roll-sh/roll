# node-postgres pg@8.16.0 (JS): packages/pg's integration tests (pure JS client), each file
# run on its own (a file stops at its first failing test); also pg-cursor and
# pg-query-stream's tests.
# image: node:22-bookworm-slim
# src: https://github.com/brianc/node-postgres/archive/refs/tags/pg@8.16.0.tar.gz
set -eu
export PGDATABASE=a38_nodepg
export npm_config_cache=/cache/home/npm YARN_CACHE_FOLDER=/cache/home/yarn
if [ ! -d node_modules ]; then
  yarn install --frozen-lockfile --ignore-engines --ignore-scripts > /dev/null
  yarn build > /dev/null
fi
cd packages/pg
node script/create-test-tables.js postgres:// || true
for f in $(find test/integration -name '*-tests.js' | sort); do
  if timeout 120 node "$f" > "$OUT/log.txt" 2>&1; then
    echo "PASS pg/$f"
  else
    echo "FAIL pg/$f"
    echo "== $f" >> "$OUT/failures.txt"
    tail -25 "$OUT/log.txt" >> "$OUT/failures.txt"
  fi
done > "$OUT/pg.lst"
cd ../pg-cursor
npx mocha --reporter xunit --reporter-option output="$OUT/cursor.xml" > /dev/null 2>&1 || true
cd ../pg-query-stream
npx mocha -r ts-node/register 'test/**/*.ts' --reporter xunit --reporter-option output="$OUT/stream.xml" > /dev/null 2>&1 || true
grep -c PASS "$OUT/pg.lst"; grep FAIL "$OUT/pg.lst" || true
