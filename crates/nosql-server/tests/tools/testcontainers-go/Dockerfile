# Testcontainers for Go (cassandra and scylladb modules) with the roll image swapped in.
FROM golang:1.23@sha256:60deed95d3888cc5e4d9ff8a10c54e5edc008c6ae3fba6187be6fb592e19e8c0
WORKDIR /tc
COPY go.mod main.go init.cql ./
RUN GOTOOLCHAIN=auto GOFLAGS=-mod=mod go mod tidy && GOTOOLCHAIN=auto go build -o /tc/tccheck .
ENTRYPOINT ["/tc/tccheck"]
