# Alembic 1.20.1 (Python migrations; autogenerate reflects the catalogs): its own tests/ with
# SQLAlchemy 2.1.6 and psycopg2, against database a81_alembic (schemas test_schema /
# test_schema_2, as SQLAlchemy's README.unittests asks), serially. Tests that only run on other
# backends skip themselves; the backend-neutral ones run on PostgreSQL.
# image: python:3.12-slim
# src: https://github.com/sqlalchemy/alembic/archive/refs/tags/rel_1_20_1.tar.gz
set -eu
V=/cache/venv/alembic
if [ ! -x $V/bin/pytest ]; then
  python -m venv $V
  $V/bin/pip install -q "$SRC" sqlalchemy==2.1.6 psycopg2-binary==2.9.13 pytest==9.1.1 \
    pytest-timeout==2.4.0 black==26.5.1 zimports==0.7.0 tzdata junitparser mako==1.4.3
fi
W=/cache/work/alembic
rm -rf $W && mkdir -p $W && cp -r "$SRC/tests" "$SRC/alembic" "$SRC/setup.cfg" "$SRC/pyproject.toml" "$SRC/tools" $W/
cd $W
URI="postgresql+psycopg2://postgres:$PGPASSWORD@$PGHOST:$PGPORT/a81_alembic"
$V/bin/pytest tests -p no:cacheprovider -q --timeout=120 "--dburi=$URI" \
  --junitxml="$OUT/junit.xml" "$@" 2>&1 | tail -40
