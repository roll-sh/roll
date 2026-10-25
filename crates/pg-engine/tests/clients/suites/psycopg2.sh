# psycopg2 2.9.10 (Python): its own tests/ (unittest, run by pytest), against the
# psycopg2-binary wheel of the same version. The tests run from a copy, so `import psycopg2`
# finds the wheel and not the unbuilt source tree.
# image: python:3.12-slim
# src: https://github.com/psycopg/psycopg2/archive/refs/tags/2.9.10.tar.gz
set -eu
V=/cache/venv/psycopg2
if [ ! -x $V/bin/pytest ]; then
  python -m venv $V
  $V/bin/pip install -q psycopg2-binary==2.9.10 pytest==8.3.5 pytest-timeout==2.4.0
fi
W=/cache/work/psycopg2
rm -rf $W && mkdir -p $W && cp -r "$SRC/tests" $W/
cd $W
export PSYCOPG2_TESTDB=a38_psycopg2
$V/bin/pytest tests -p no:cacheprovider -q --timeout=60 --junitxml="$OUT/junit.xml" "$@" 2>&1 | tail -300
