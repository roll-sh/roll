# psycopg_pool (Python connection pools, sync and asyncio): tests/pool from the psycopg 3.2.9
# source, with that tree's psycopg_pool and psycopg-binary 3.2.9 (as suites/psycopg3.sh pins).
# Unlike psycopg3.sh, this runs the pool tests marked slow and timing too (most of them are);
# a timing test failing on one server only is worth a second run before it counts.
# image: python:3.12-slim
# src: https://github.com/psycopg/psycopg/archive/refs/tags/3.2.9.tar.gz
set -eu
V=/cache/venv/psycopgpool
if [ ! -x $V/bin/pytest ]; then
  python -m venv $V
  $V/bin/pip install -q "./psycopg[test]" ./psycopg_pool "psycopg-binary==3.2.9" \
    pytest-timeout==2.4.0
fi
# The reconnection tests put pproxy between the pool and the server.
[ -x $V/bin/pproxy ] || $V/bin/pip install -q pproxy==2.7.9
export PSYCOPG_IMPL=binary PATH=$V/bin:$PATH
$V/bin/pytest tests/pool -p no:randomly -o cache_dir=/cache/home/pytest-psycopgpool -q \
  --timeout=120 -m "not flakey" \
  --test-dsn "host=$PGHOST port=$PGPORT user=$PGUSER password=$PGPASSWORD dbname=a81_pool" \
  --junitxml="$OUT/junit.xml" "$@" 2>&1 | tail -60
