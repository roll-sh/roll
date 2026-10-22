# scylla-bench (ScyllaDB's load generator, on ScyllaDB's shard-aware gocql). Built from the tag:
# its go.mod replaces gocql, which `go install …@version` refuses.
FROM golang:1.23
RUN git clone -q --depth 1 --branch v1.3.3 https://github.com/scylladb/scylla-bench /src \
 && cd /src && GOTOOLCHAIN=auto go build -o /usr/local/bin/scylla-bench .
ENTRYPOINT ["/usr/local/bin/scylla-bench"]
