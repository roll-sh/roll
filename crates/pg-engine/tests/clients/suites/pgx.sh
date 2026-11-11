# pgx v5.7.5 (Go): the root package, pgconn, pgtype, pgxpool and stdlib tests, against the
# pgx_test database its testsetup/postgresql_setup.sql makes.
# image: golang:1.24-bookworm
# src: https://github.com/jackc/pgx/archive/refs/tags/v5.7.5.tar.gz
set -eu
export GOPATH=/cache/go GOCACHE=/cache/go-build GOFLAGS=-mod=mod
DSN="host=$PGHOST port=$PGPORT user=postgres password=$PGPASSWORD dbname=pgx_test sslmode=disable"
export PGX_TEST_DATABASE="$DSN"
export PGX_TEST_TCP_CONN_STRING="$DSN"
# -p 1: packages one at a time (they share the database); a fixed order.
go test -p 1 -count=1 -timeout 30m -json ./ ./pgconn/... ./pgtype/... ./pgxpool/... ./stdlib/... \
  "$@" > "$OUT/pgx.gojson" 2>&1 || true
grep -E '"Action":"fail"' "$OUT/pgx.gojson" | grep -o '"Test":"[^"]*"' | sort -u | head -200
grep -E '^\{"Time[^}]*"Action":"(pass|fail)","Package":"[^"]*","Elapsed' "$OUT/pgx.gojson" | grep -v '"Test"' || true
