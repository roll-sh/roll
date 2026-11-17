# Build from sdk/ (the provider uses the Go client beside it): docker build -f csi-provider/Dockerfile .
FROM golang:1.24-bookworm AS build
WORKDIR /src
COPY go/secrets go/secrets
COPY csi-provider csi-provider
RUN cd csi-provider && CGO_ENABLED=0 go build -trimpath -ldflags=-s -o /roll-csi-provider .

FROM gcr.io/distroless/static:nonroot
COPY --from=build /roll-csi-provider /roll-csi-provider
ENTRYPOINT ["/roll-csi-provider"]
