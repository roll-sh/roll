# Metabase 0.64.3 (its jar, on Temurin's JRE 21) with the server as its application database
# (metabase_app: the Liquibase migrations it runs at startup, then what setup, the sync and
# the API store) and as a PostgreSQL database it syncs and queries (metabase_ds,
# metabase.setup.sql): the synced tables and fields (types, keys, descriptions, JSON
# unfolding, fingerprints, field values), native and MBQL queries (time breakouts, binning,
# implicit joins, filters, expressions, cumulative and percentile aggregations, nested
# queries), a saved question, search and an X-ray. One id per step (suites/apps.py); answers
# in $OUT/responses.txt, Metabase's log in $OUT/metabase.log. APP_PORT: its port (52451).
# image: python:3.12-slim
# src:
set -eu
V=/cache/venv/apps
[ -x $V/bin/python ] || { python -m venv $V && $V/bin/pip install -q "psycopg[binary]==3.2.9"; }
M=/cache/src/metabase
mkdir -p $M
if [ ! -x $M/jre/bin/java ]; then
  python - <<'EOF'
import io, tarfile, urllib.request
url = "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.13%2B9/OpenJDK21U-jre_x64_linux_hotspot_21.0.13_9.tar.gz"
tarfile.open(fileobj=io.BytesIO(urllib.request.urlopen(url).read())).extractall("/cache/src/metabase")
EOF
  mv $M/jdk-21*-jre $M/jre
fi
[ -f $M/metabase-0.64.3.jar ] || python -c "
import urllib.request; urllib.request.urlretrieve('https://downloads.metabase.com/v0.64.3/metabase.jar', '$M/metabase-0.64.3.jar')"
W=/tmp/metabase-run
rm -rf $W && mkdir -p $W && cd $W
export MB_DB_TYPE=postgres MB_DB_HOST=$PGHOST MB_DB_PORT=$PGPORT MB_DB_DBNAME=metabase_app
export MB_DB_USER=postgres MB_DB_PASS=pw MB_JETTY_HOST=127.0.0.1 MB_JETTY_PORT=${APP_PORT:-52451}
export MB_CHECK_FOR_UPDATES=false MB_ANON_TRACKING_ENABLED=false MB_PLUGINS_DIR=$W/plugins
export MB_SEND_NEW_SSO_USER_ADMIN_EMAIL=false MB_LOAD_SAMPLE_CONTENT=false MB_ENABLE_EMBEDDING=false
$M/jre/bin/java -Xmx1500m -XX:+UseSerialGC -jar $M/metabase-0.64.3.jar > "$OUT/metabase.log" 2>&1 &
P=$!
SUITE_NAME=metabase $V/bin/python /harness/suites/metabase.py || true
kill $P 2>/dev/null || true
wait $P 2>/dev/null || true
grep -c '^PASS' "$OUT/metabase.lst" || true
grep -E ' ERROR ' "$OUT/metabase.log" | head -20 || true
