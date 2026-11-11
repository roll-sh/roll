# MikroORM 7.2.5 (TypeScript ORM, a yarn 4 monorepo whose tests run on vitest, not jest, since
# v7): the test files under tests/ that use the PostgreSQL driver (import @mikro-orm/postgresql,
# PostgreSqlDriver or bootstrap's initORMPostgreSql; 322 files, 18 of which also touch MySQL /
# MSSQL / Mongo and fail those parts on both servers). Its tests connect as postgres with no
# password to localhost:5432 by default; MIKRO_ORM_HOST / PORT / PASSWORD fill those defaults
# (tests that spell out port 5432 / 5433, i.e. PostGIS, fail on both; so do the 18 files' other
# databases, CockroachDB and the en_US collation). Its globalSetup (a
# MongoDB memory server, and dropping every mikro_orm_test* database on 5432-5434 and the
# MySQL / MSSQL ports) is replaced by a no-op. vitest's junit reporter, one file at a time.
# image: node:22-bookworm-slim
# src: https://github.com/mikro-orm/mikro-orm/archive/refs/tags/v7.2.5.tar.gz
set -eu
export COREPACK_HOME=/cache/home/corepack COREPACK_ENABLE_DOWNLOAD_PROMPT=0
export YARN_GLOBAL_FOLDER=/cache/home/yarn-berry YARN_ENABLE_TELEMETRY=0 YARN_ENABLE_SCRIPTS=0
export YARN_ENABLE_PROGRESS_BARS=0 MONGOMS_DISABLE_POSTINSTALL=1
# --mode=skip-build: better-sqlite3 has no prebuilt binary here and the image no compiler; the
# PostgreSQL tests don't load it.
if [ ! -f .yarn-installed ]; then
  corepack yarn install --immutable --mode=skip-build > "$OUT/install.log" 2>&1 ||
    { tail -30 "$OUT/install.log"; exit 1; }
  touch .yarn-installed
fi
printf 'export async function setup() {}\nexport async function teardown() {}\n' > tests/globalSetup.ts
export MIKRO_ORM_HOST="$PGHOST" MIKRO_ORM_PORT="$PGPORT" MIKRO_ORM_PASSWORD="$PGPASSWORD"
# Its CLI tests delete every MIKRO_ORM_* variable, and the files share one process (isolate:
# false): set them again in the setup file that runs before each test file.
grep -q ROLL_CLIENTS tests/setup.ts || cat >> tests/setup.ts <<'EOF'
// ROLL_CLIENTS: the server under test, again after tests that clear MIKRO_ORM_* variables.
Object.assign(process.env, { MIKRO_ORM_HOST: process.env.PGHOST,
  MIKRO_ORM_PORT: process.env.PGPORT, MIKRO_ORM_PASSWORD: process.env.PGPASSWORD });
EOF
# swc's native addon refuses a cache root with a group-writable parent (/cache is 775).
export SWC_NATIVE_BINDING_CACHE=/tmp/swc-cache
if [ $# -eq 0 ]; then
  grep -rlE "@mikro-orm/postgresql|initORMPostgreSql|PostgreSqlDriver" tests --include='*.test.ts' |
    grep -v '^tests/deno/' | sort > "$OUT/files.txt"
  set -- $(cat "$OUT/files.txt")
fi
timeout 5400 node_modules/.bin/vitest run --reporter=junit --outputFile="$OUT/mikroorm.xml" \
  --no-file-parallelism "$@" > "$OUT/vitest.log" 2>&1 || true
grep -c '<testcase' "$OUT/mikroorm.xml" || tail -30 "$OUT/vitest.log"
