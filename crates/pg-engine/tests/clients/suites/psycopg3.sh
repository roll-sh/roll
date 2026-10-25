# psycopg 3.2.9 (Python): its own tests/, binary implementation (bundled libpq).
# image: python:3.12-slim
# src: https://github.com/psycopg/psycopg/archive/refs/tags/3.2.9.tar.gz
set -eu
V=/cache/venv/psycopg3
if [ ! -x $V/bin/pytest ]; then
  python -m venv $V
  $V/bin/pip install -q "./psycopg[test]" ./psycopg_pool "psycopg-binary==3.2.9" \
    pytest-timeout==2.4.0 dnspython==2.7.0 numpy==2.2.6 shapely==2.1.1
fi
export PSYCOPG_IMPL=${PSYCOPG_IMPL:-binary}
# A fixed order (no pytest-randomly), so the engine's and PostgreSQL's runs line up.
$V/bin/pytest tests -p no:randomly -o cache_dir=/cache/home/pytest-psycopg3 -q --timeout=60 \
  -m "not slow and not timing and not flakey and not mypy" --deselect tests/test_typing.py \
  --test-dsn "host=$PGHOST port=$PGPORT user=$PGUSER password=$PGPASSWORD dbname=a38_psycopg" \
  --junitxml="$OUT/junit.xml" "$@" 2>&1 | tail -300
