# GORM v1.31.2 (Go) with gorm.io/driver/postgres v1.6.3 (pgx v5's database/sql stdlib layer):
# its tests/ package with GORM_DIALECT=postgres, against the gorm database and role its
# compose.yml makes (setup.sql). Run twice: as CI runs it (PreferSimpleProtocol: true, the
# simple query protocol) and once more with PreferSimpleProtocol off and pgx's describe_exec
# mode (the extended protocol: Parse, Describe, Bind, Execute per query, no statement cache,
# which goes stale when the tests recreate tables: "cached plan must not change result type"
# then aborts the package on PostgreSQL), reported as gorm.io/gorm/tests[extended]. Without -race (CI uses
# it; it only slows the Go side). CI's `go get -u` is replaced by pinning the driver, which
# needs Go >= 1.25: go fetches the pinned toolchain (GOTOOLCHAIN) into /cache/go.
# image: golang:1.24-bookworm
# src: https://github.com/go-gorm/gorm/archive/refs/tags/v1.31.2.tar.gz
set -eu
export GOPATH=/cache/go GOCACHE=/cache/go-build GOFLAGS=-mod=mod GOTOOLCHAIN=go1.26.10
cd "$SRC/tests"
if [ ! -f .pinned ]; then
  go mod edit -require=gorm.io/driver/postgres@v1.6.3
  go mod tidy
  touch .pinned
fi
# The extended-protocol switch (an env var the test helper reads).
sed -i 's/PreferSimpleProtocol: true,/PreferSimpleProtocol: os.Getenv("GORM_EXTENDED") == "",/' tests_test.go
export GORM_DIALECT=postgres
export GORM_DSN="user=gorm password=gorm dbname=gorm host=$PGHOST port=$PGPORT sslmode=disable TimeZone=Asia/Shanghai"
go test -count=1 -timeout 30m -json ./... "$@" > "$OUT/gorm.gojson" 2>&1 || true
GORM_EXTENDED=1 GORM_DSN="$GORM_DSN default_query_exec_mode=describe_exec" go test -count=1 -timeout 30m -json ./... "$@" 2>&1 \
  | sed 's|"Package":"gorm.io/gorm/tests"|"Package":"gorm.io/gorm/tests[extended]"|' \
  > "$OUT/gorm-extended.gojson" || true
for f in "$OUT"/*.gojson; do
  echo "$f: $(grep -c '"Action":"pass","Package[^}]*"Test"' "$f") pass, $(grep -c '"Action":"fail","Package[^}]*"Test"' "$f") fail"
  grep -E '"Action":"fail"' "$f" | grep -o '"Test":"[^"]*"' | sort -u | head -60
done
