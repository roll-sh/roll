# sqlc v1.31.1 (Go): the tests that use a PostgreSQL server (POSTGRESQL_SERVER_URI), with the
# flags of its CI's test job (CGO_ENABLED=0, -tags=examples): the generated pgx/v5 example
# programs' tests (examples/{authors,batch,booktest,ondeck}/postgresql), TestValidSchema (every
# endtoend schema loaded into a fresh database), TestExamplesVet, TestReplay/managed-db (sqlc
# generate with a managed database: the server prepares each query and describes its
# parameters and columns) and TestExpandPostgreSQL. MySQL isn't provided (MYSQL_SERVER_URI is
# invalid), so the MySQL cases fail on both sides.
# image: golang:1.24-bookworm
# src: https://github.com/sqlc-dev/sqlc/archive/refs/tags/v1.31.1.tar.gz
set -eu
export GOPATH=/cache/go GOCACHE=/cache/go-build GOFLAGS=-mod=mod GOTOOLCHAIN=go1.26.10 CGO_ENABLED=0
export POSTGRESQL_SERVER_URI="postgres://postgres:pw@$PGHOST:$PGPORT/postgres?sslmode=disable"
export MYSQL_SERVER_URI=invalid
# sqlc caches a managed database's analysis of each query under SQLCCACHE, keyed by its
# version, config, schema and query text but not the server: without a fresh cache per run,
# one server's answers (or an older engine's) are replayed against the other.
export SQLCCACHE=/tmp/sqlc-cache-$TARGET
rm -rf "$SQLCCACHE"
cd "$SRC"
T="go test -tags=examples -count=1 -timeout 40m -json"
$T ./examples/authors/postgresql ./examples/batch/postgresql ./examples/booktest/postgresql \
  ./examples/ondeck/postgresql "$@" > "$OUT/examples.gojson" 2>&1 || true
$T ./internal/endtoend -run '^(TestValidSchema|TestExamplesVet)$' "$@" > "$OUT/schema.gojson" 2>&1 || true
$T ./internal/endtoend -run '^TestReplay$/^managed-db$' "$@" > "$OUT/replay.gojson" 2>&1 || true
$T ./internal/x/expander -run '^TestExpandPostgreSQL$' "$@" > "$OUT/expander.gojson" 2>&1 || true
for f in "$OUT"/*.gojson; do
  echo "$f: $(grep -c '"Action":"pass","Package[^}]*"Test"' "$f") pass, $(grep -c '"Action":"fail","Package[^}]*"Test"' "$f") fail"
  grep -E '"Action":"fail"' "$f" | grep -o '"Test":"[^"]*"' | grep -iv mysql | sort -u | head -40
done
