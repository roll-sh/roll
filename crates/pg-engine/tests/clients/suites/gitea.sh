# Gitea 28.2.0 (its release binary) with the server as its database (gitea_app: xorm's
# migrations and schema sync at startup, then the REST API): users, organizations, teams,
# repositories with files and branches (git through its database's metadata), labels,
# milestones, issues, comments, reactions, the database issue indexer's searches, pull
# requests, releases, stars, webhooks, admin listings, errors, deletes (suites/gitea.py).
# One id per step (suites/apps.py); answers in $OUT/responses.txt, Gitea's log in
# $OUT/gitea.log. APP_PORT: its HTTP port (52455).
# image: golang:1.24-bookworm
# src:
set -eu
L=/cache/venv/gitea-lib
sh /harness/suites/wheels.sh $L cp311
G=/cache/src/gitea/gitea-28.2.0
if [ ! -x $G ]; then
  mkdir -p /cache/src/gitea
  python3 -c 'import sys, urllib.request; open(sys.argv[2], "wb").write(urllib.request.urlopen(sys.argv[1]).read())' \
    https://github.com/go-gitea/gitea/releases/download/v28.2.0/gitea-28.2.0-linux-amd64 $G.tmp
  chmod +x $G.tmp && mv $G.tmp $G
fi
W=/tmp/gitea
rm -rf $W && mkdir -p $W/custom/conf
PORT=${APP_PORT:-52455}
cat > $W/custom/conf/app.ini <<EOF
APP_NAME = gitea
RUN_MODE = prod
WORK_PATH = $W
[server]
HTTP_ADDR = $PGHOST
HTTP_PORT = $PORT
ROOT_URL = http://$PGHOST:$PORT/
DISABLE_SSH = true
OFFLINE_MODE = true
LFS_START_SERVER = false
[database]
DB_TYPE = postgres
HOST = $PGHOST:$PGPORT
NAME = gitea_app
USER = postgres
PASSWD = pw
SSL_MODE = disable
LOG_SQL = false
[security]
INSTALL_LOCK = true
SECRET_KEY = a133-gitea-suite-secret
[service]
DISABLE_REGISTRATION = true
[repository]
ROOT = $W/repos
[indexer]
ISSUE_INDEXER_TYPE = db
REPO_INDEXER_ENABLED = false
[session]
PROVIDER = db
[cron]
ENABLED = false
[mailer]
ENABLED = false
[log]
MODE = console
LEVEL = Warn
EOF
export HOME=$W GITEA_WORK_DIR=$W GITEA_CUSTOM=$W/custom
$G web --config $W/custom/conf/app.ini > "$OUT/gitea.log" 2>&1 &
P=$!
SUITE_NAME=gitea PYTHONPATH=$L GITEA_BIN=$G GITEA_CONF=$W/custom/conf/app.ini python3 /harness/suites/gitea.py || true
kill $P 2>/dev/null || true
wait $P 2>/dev/null || true
grep -c '^PASS' "$OUT/gitea.lst" || true
grep -E '\[E\]' "$OUT/gitea.log" | head -20 || true
