# Grafana 13.3.0 (its release tarball) with the server as its own database (grafana_app: the
# ~700 migrations it runs at startup, then what its HTTP API stores: folders, dashboards and
# their versions, users, teams, orgs, preferences, service accounts, annotations, playlists,
# library panels, short URLs, snapshots, query history, alert rules and contact points) and as
# a PostgreSQL datasource over grafana_ds (grafana.setup.sql: health, the query editor's
# metadata queries, table and time-series queries with $__ macros, annotations, variables,
# errors). One id per step, `<step>#<hash>` of its normalized answer (suites/apps.py); the
# migrated schema is one id per table. Answers in $OUT/responses.txt, Grafana's log in
# $OUT/grafana.log. APP_PORT (default 52450) is Grafana's HTTP port on the host network.
# image: python:3.12-slim
# src:
set -eu
V=/cache/venv/apps
[ -x $V/bin/python ] || { python -m venv $V && $V/bin/pip install -q "psycopg[binary]==3.2.9"; }
G=/cache/src/grafana/grafana-13.3.0
if [ ! -x $G/bin/grafana ]; then
  mkdir -p /cache/src/grafana
  python - <<'EOF'
import io, tarfile, urllib.request
url = "https://dl.grafana.com/grafana/release/13.3.0/grafana_13.3.0_40412481033_linux_amd64.tar.gz"
tarfile.open(fileobj=io.BytesIO(urllib.request.urlopen(url).read())).extractall("/cache/src/grafana")
EOF
  [ -x $G/bin/grafana ] || { ls /cache/src/grafana; exit 1; }
fi
D=/tmp/grafana-data
rm -rf $D && mkdir -p $D
export GF_PATHS_DATA=$D GF_PATHS_LOGS=$D/log GF_PATHS_PLUGINS=$D/plugins GF_PATHS_PROVISIONING=$D/prov
export GF_SERVER_HTTP_ADDR=127.0.0.1 GF_SERVER_HTTP_PORT=${APP_PORT:-52450}
export GF_DATABASE_TYPE=postgres GF_DATABASE_HOST=$PGHOST:$PGPORT GF_DATABASE_NAME=grafana_app
export GF_DATABASE_USER=postgres GF_DATABASE_PASSWORD=pw GF_DATABASE_SSL_MODE=disable
export GF_SECURITY_ADMIN_USER=admin GF_SECURITY_ADMIN_PASSWORD=admin
export GF_ANALYTICS_REPORTING_ENABLED=false GF_ANALYTICS_CHECK_FOR_UPDATES=false
export GF_ANALYTICS_CHECK_FOR_PLUGIN_UPDATES=false GF_PLUGINS_PREINSTALL_DISABLED=true
export GF_NEWS_NEWS_FEED_ENABLED=false GF_LOG_MODE=console GF_LOG_LEVEL=warn
$G/bin/grafana server --homepath $G > "$OUT/grafana.log" 2>&1 &
P=$!
SUITE_NAME=grafana $V/bin/python /harness/suites/grafana.py || true
kill $P 2>/dev/null || true
wait $P 2>/dev/null || true
grep -c '^PASS' "$OUT/grafana.lst" || true
grep -iE 'level=error|eror' "$OUT/grafana.log" | head -20 || true
