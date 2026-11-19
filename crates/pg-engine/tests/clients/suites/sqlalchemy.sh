# SQLAlchemy 2.0.41 (ORM): the PostgreSQL dialect tests and the backend-neutral suite
# (test/dialect/postgresql, test/sql, test/orm subset), psycopg2 driver, against database
# a38_sqla with the schemas its README.unittests asks for. sqla_psycopg.sh and sqla_asyncpg.sh run
# it with SQLA_DRIVER psycopg (psycopg 3) and asyncpg.
# image: python:3.12-slim
# src: https://github.com/sqlalchemy/sqlalchemy/archive/refs/tags/rel_2_0_41.tar.gz
set -eu
D=${SQLA_DRIVER:-psycopg2}
case $D in
  psycopg2) V=/cache/venv/sqlalchemy PKG=psycopg2-binary==2.9.10 ;;
  psycopg) V=/cache/venv/sqlalchemy-psycopg PKG="psycopg[binary]==3.2.9" ;;
  asyncpg) V=/cache/venv/sqlalchemy-asyncpg PKG="asyncpg==0.30.0 greenlet==3.2.4" ;;
esac
if [ ! -x $V/bin/pytest ]; then
  python -m venv $V
  $V/bin/pip install -q "$SRC" $PKG pytest==8.3.5 pytest-xdist==3.6.1 \
    pytest-timeout==2.4.0 typing_extensions
fi
W=/cache/work/sqlalchemy-$D
rm -rf $W && mkdir -p $W && cp -r "$SRC/test" "$SRC/lib" "$SRC/setup.cfg" "$SRC/pyproject.toml" $W/
cd $W
URI="postgresql+$D://postgres:$PGPASSWORD@$PGHOST:$PGPORT/a38_sqla"
$V/bin/pytest test/dialect/postgresql test/sql test/base/test_concurrency_py3k.py \
  -p no:cacheprovider -q --timeout=120 --dburi "$URI" \
  --junitxml="$OUT/junit.xml" "$@" 2>&1 | tail -60
