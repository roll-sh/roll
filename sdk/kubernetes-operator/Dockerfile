# Build from sdk/ (the operator uses the Go client beside it): docker build -f kubernetes-operator/Dockerfile .
FROM golang:1.24-bookworm AS build
WORKDIR /src
COPY go/secrets go/secrets
COPY kubernetes-operator kubernetes-operator
RUN cd kubernetes-operator && CGO_ENABLED=0 go build -trimpath -ldflags=-s -o /operator .

FROM gcr.io/distroless/static:nonroot
COPY --from=build /operator /operator
ENTRYPOINT ["/operator"]
