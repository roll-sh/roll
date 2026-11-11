# TypeORM 1.1.2 (TypeScript ORM): packages/typeorm's mocha tests with an ormconfig.json of only
# its postgres connection (as its CI's postgres job), host / port pointed at the server, the
# compiled build, mocha's xunit reporter. Without --bail, so one failure doesn't stop the run.
# image: node:22-bookworm-slim
# src: https://github.com/typeorm/typeorm/archive/refs/tags/1.1.2.tar.gz
set -eu
export npm_config_cache=/cache/home/npm COREPACK_HOME=/cache/home/corepack
export COREPACK_ENABLE_DOWNLOAD_PROMPT=0 PNPM_HOME=/cache/home/pnpm
export npm_config_store_dir=/cache/home/pnpm-store
PNPM="corepack pnpm"
if [ ! -d node_modules ]; then
  $PNPM install --frozen-lockfile --ignore-scripts > "$OUT/install.log" 2>&1 || { tail -30 "$OUT/install.log"; exit 1; }
fi
cd packages/typeorm
if [ ! -f build/compiled/test/utils/test-setup.js ]; then
  $PNPM run compile > "$OUT/compile.log" 2>&1 || { tail -30 "$OUT/compile.log"; exit 1; }
fi
node -e '
const c = require("./ormconfig.sample.json").filter((x) => x.type === "postgres");
c[0].host = process.env.PGHOST; c[0].port = +process.env.PGPORT;
require("fs").writeFileSync("ormconfig.json", JSON.stringify(c, null, 2));'
timeout 5400 npx mocha --reporter xunit --reporter-option output="$OUT/typeorm.xml" "$@" \
  > "$OUT/mocha.log" 2>&1 || true
# Test titles with control characters (a NUL in an injection test) make the XML unparseable.
node -e '
const fs = require("fs"), f = process.argv[1];
fs.writeFileSync(f, fs.readFileSync(f, "utf8").replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f]/g, "?"));' \
  "$OUT/typeorm.xml" 2>/dev/null || true
grep -c '<testcase' "$OUT/typeorm.xml" || tail -30 "$OUT/mocha.log"
