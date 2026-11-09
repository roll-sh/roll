# The Go driver suite. DRIVER: gocql (github.com/gocql/gocql) or scylla (ScyllaDB's fork, swapped
# in with a replace directive and built with `-tags scylla` for the parts whose API differs; it
# needs Go 1.25, which GOTOOLCHAIN=auto fetches).
FROM golang:1.23
ARG DRIVER=gocql
WORKDIR /suite
COPY go.mod *.go ./
RUN if [ "$DRIVER" = scylla ]; then go mod edit -go=1.25.0 -replace github.com/gocql/gocql=github.com/scylladb/gocql@v1.20.0; fi \
 && GOTOOLCHAIN=auto go build -mod=mod -tags "$DRIVER" -o /suite/suite .
ENTRYPOINT ["/suite/suite"]
