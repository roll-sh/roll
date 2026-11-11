# asyncpg 0.30.0 (Python): its own tests/ against a running cluster (PGHOST set), with the
# released wheel. The tests run from a copy, so `import asyncpg` finds the wheel.
# image: python:3.12-slim
# src: https://github.com/MagicStack/asyncpg/archive/refs/tags/v0.30.0.tar.gz
set -eu
V=/cache/venv/asyncpg
if [ ! -x $V/bin/pytest ]; then
  python -m venv $V
  $V/bin/pip install -q asyncpg==0.30.0 pytest==8.3.5 pytest-timeout==2.4.0 \
    uvloop==0.21.0 distro==1.9.0 "gssapi==1.9.0" || \
  $V/bin/pip install -q asyncpg==0.30.0 pytest==8.3.5 pytest-timeout==2.4.0 uvloop==0.21.0 distro==1.9.0
fi
W=/cache/work/asyncpg
rm -rf $W && mkdir -p $W && cp -r "$SRC/tests" $W/
cd $W
export PGDATABASE=postgres
$V/bin/pytest tests -p no:cacheprovider -q --timeout=120 --junitxml="$OUT/junit.xml" "$@" 2>&1 | tail -300
