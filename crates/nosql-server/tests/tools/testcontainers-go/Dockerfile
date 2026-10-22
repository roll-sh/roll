# Testcontainers for Go (cassandra and scylladb modules) with the roll image swapped in.
FROM golang:1.23
WORKDIR /tc
COPY go.mod main.go init.cql ./
RUN GOTOOLCHAIN=auto GOFLAGS=-mod=mod go mod tidy && GOTOOLCHAIN=auto go build -o /tc/tccheck .
ENTRYPOINT ["/tc/tccheck"]
