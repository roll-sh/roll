#!/bin/sh
# Frameworks, ORMs and tools against roll and against PostgreSQL 18.6, side by side, test by test.
#
#   tools/compat/compat.sh run <pg|engine|both> [suite ...]   # default: every suite in SUITES
#   tools/compat/compat.sh table [suite ...]                  # per suite: tests, identical, differing
#   tools/compat/compat.sh diff <suite>                       # the tests whose outcome differs
#
# The suites are tests/clients/suites/<suite>.sh (tests/clients/run.sh runs one): each fetches a
# pinned release of a third-party test suite or tool into $COMPAT_CACHE/src and runs it inside a
# container on the host network (never on the host), against the server named by
#   COMPAT_PG       PostgreSQL 18.6, host:port (password pw, user postgres; the pinned image of
#                   tools/ref-containers.sh with default settings and
#                   POSTGRES_INITDB_ARGS="--locale=C --encoding=UTF8")
#   COMPAT_ENGINE   roll, host:port: a plain dist build (tools/dist.sh), started with
#                   roll-engine --data <dir> --listen <host:port> --password pw --allow-server-programs \
#                     --allow-outbound local --hba "local all all trust" --hba "host all all all scram-sha-256"
#                   (the reference image's rules: its Unix socket is trusted, so postgres_fdw's
#                   loopback in ActiveRecord's ForeignTableTest connects as on PostgreSQL)
# and writes one `PASS|FAIL|SKIP <test id>` line per test to $COMPAT_CACHE/results/<suite>-<pg|engine>.txt.
# A difference is a bug in roll unless PostgreSQL's run fails the same way.
#   COMPAT_CACHE    sources, package caches (pip, npm, Go, Gradle, Maven, gems; ~10 GB) and
#                   results (default ~/.cache/pg-compat)
#   COMPAT_NAME     container name prefix (default compat; agents use a<NNN>)
#   COMPAT_CPUSET   the suites' CPUs (default 0-5,18-23: off the benchmark lanes)
# Run each suite against a fresh engine where the counts matter: some suites leave databases and
# roles behind that others' catalog queries list (metaqueries above all).
# Logical replication PostgreSQL <-> roll is tools/bench/repl/e2e.sh. tests/clients/STATUS.md
# "Round 9" has the table and what each difference was (TypeORM's 12 pgvector tests fail on
# the reference image, which has no pgvector: the environment's).
set -eu
if ! docker info > /dev/null 2>&1; then exec sg docker -c "exec $0 $*"; fi
HERE=$(cd "$(dirname "$0")" && pwd)
CLIENTS=$HERE/../../tests/clients
export CLIENTS_CACHE=${COMPAT_CACHE:-$HOME/.cache/pg-compat}
export CLIENTS_NAME=${COMPAT_NAME:-compat}
export CLIENTS_CPUSET=${COMPAT_CPUSET:-0-5,18-23}
PG=${COMPAT_PG:-127.0.0.1:55838}
ENGINE=${COMPAT_ENGINE:-127.0.0.1:55839}
export PG_HOST=${PG%:*} PG_PORT=${PG##*:} ENGINE_HOST=${ENGINE%:*} ENGINE_PORT=${ENGINE##*:}

# The suites and the arguments each runs with.
SUITES="django sqlalchemy sqla_psycopg sqla_asyncpg activerecord prisma drizzle drizzlekit asyncpg pgx spring alembic pgrestore metaqueries hibernate knex typeorm"
args() {
  case $1 in
    django) echo "backends migrations schema queries aggregation postgres_tests basic expressions model_fields
      transactions introspection inspectdb custom_lookups lookup ordering update delete get_or_create
      select_for_update bulk_create db_functions indexes constraints model_inheritance many_to_one
      m2m_through raw_query timezones" ;;
    activerecord) echo "test/cases/adapters/postgresql/**/*_test.rb" ;;
    # (The dialect's own tests, types and the schema tools; all of hibernate-core takes hours.)
    hibernate) echo "--tests org.hibernate.orm.test.dialect.* --tests org.hibernate.orm.test.type.*
      --tests org.hibernate.orm.test.schemaupdate.* --tests org.hibernate.orm.test.schemavalidation.*" ;;
    # The archive is PostgreSQL's for both servers: a database moved from PostgreSQL to roll.
    pgrestore) echo "--from $PG" ;;
  esac
}

run_one() {
  # (No globbing: activerecord's argument is a pattern for the suite.)
  set -f
  # shellcheck disable=SC2046
  "$CLIENTS/run.sh" "$1" "$2" $(args "$1")
  set +f
}

cmd=${1:-}
[ $# -gt 0 ] && shift
case $cmd in
  run)
    which=${1:?pg, engine or both}; shift
    [ $# -gt 0 ] || set -- $SUITES
    for s in "$@"; do
      case $which in
        pg | engine) run_one "$s" "$which" || true ;;
        both) run_one "$s" pg || true; run_one "$s" engine || true ;;
        *) echo "pg, engine or both" >&2; exit 2 ;;
      esac
    done ;;
  table)
    [ $# -gt 0 ] || set -- $SUITES
    exec python3 -I "$HERE/table.py" "$CLIENTS_CACHE/results" "$@" ;;
  diff)
    exec python3 -I "$CLIENTS/compare.py" "$CLIENTS_CACHE/results/${1:?suite}-engine.txt" "$CLIENTS_CACHE/results/$1-pg.txt" ;;
  *)
    sed -n '2,6p' "$0" >&2; exit 2 ;;
esac
