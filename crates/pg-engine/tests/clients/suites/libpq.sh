# lib/pq v1.10.9 (Go): its own tests against $PGHOST.
# image: golang:1.24-bookworm
# src: https://github.com/lib/pq/archive/refs/tags/v1.10.9.tar.gz
set -eu
export GOPATH=/cache/go GOCACHE=/cache/go-build GOFLAGS=-mod=mod
export PGSSLMODE=disable PGDATABASE=a38_pqgotest
go test -p 1 -count=1 -timeout 20m -json ./... "$@" > "$OUT/pq.gojson" 2>&1 || true
grep -E '"Action":"fail"' "$OUT/pq.gojson" | grep -o '"Test":"[^"]*"' | sort -u | head -200
