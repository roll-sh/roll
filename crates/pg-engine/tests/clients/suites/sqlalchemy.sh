# SQLAlchemy 2.0.41 (ORM): the PostgreSQL dialect tests and the backend-neutral suite
# (test/dialect/postgresql, test/sql, test/orm subset), psycopg2 driver, against database
# a38_sqla with the schemas its README.unittests asks for.
# image: python:3.12-slim
# src: https://github.com/sqlalchemy/sqlalchemy/archive/refs/tags/rel_2_0_41.tar.gz
set -eu
V=/cache/venv/sqlalchemy
if [ ! -x $V/bin/pytest ]; then
  python -m venv $V
  $V/bin/pip install -q "$SRC" psycopg2-binary==2.9.10 pytest==8.3.5 pytest-xdist==3.6.1 \
    pytest-timeout==2.4.0 typing_extensions
fi
W=/cache/work/sqlalchemy
rm -rf $W && mkdir -p $W && cp -r "$SRC/test" "$SRC/lib" "$SRC/setup.cfg" "$SRC/pyproject.toml" $W/
cd $W
URI="postgresql+psycopg2://postgres:$PGPASSWORD@$PGHOST:$PGPORT/a38_sqla"
$V/bin/pytest test/dialect/postgresql test/sql test/base/test_concurrency_py3k.py \
  -p no:cacheprovider -q --timeout=120 --dburi "$URI" \
  --junitxml="$OUT/junit.xml" "$@" 2>&1 | tail -60
