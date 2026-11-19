# roll secrets' Lambda extension: `make` writes layer-amd64.zip and layer-arm64.zip (the binary at
# extensions/roll-secrets). Needs Go 1.24; without it, see the README's Docker line.
NAME    := roll-secrets
VERSION ?= dev
SOURCES := $(wildcard *.go) $(wildcard ../go/secrets/*.go)

layers: layer-amd64.zip layer-arm64.zip

layer-%.zip: $(SOURCES)
	GOOS=linux GOARCH=$* CGO_ENABLED=0 go build -trimpath -ldflags='-s -w -X main.version=$(VERSION)' -o build/$*/$(NAME) .
	go run ./pack -o $@ build/$*/$(NAME)

test:
	test -z "$$(gofmt -l .)"
	go vet ./...
	go test -race -count=1 ./...

clean:
	rm -rf build layer-*.zip

.PHONY: layers test clean
