# Shared by sdk.sh and e2e.sh: go_build OUT CMD ARGS... runs `go CMD -o OUT ARGS` in the Terraform provider's
# module, with the host's go or, without one, in Docker's golang image (static, so OUT runs here).
# ROLL_GO_CACHE is a directory for Docker's module and build caches (none: a fresh download).
provider_src=$(cd "$(dirname "${BASH_SOURCE[0]}")/../terraform-provider-roll" && pwd)

go_build() {
  local out=$1 cmd=$2
  shift 2
  mkdir -p "$(dirname "$out")"
  if command -v go >/dev/null; then
    (cd "$provider_src" && CGO_ENABLED=0 go "$cmd" -o "$out" "$@")
    return
  fi
  local dir cache=()
  dir=$(cd "$(dirname "$out")" && pwd)
  if [ -n "${ROLL_GO_CACHE:-}" ]; then
    cache=(-v "$ROLL_GO_CACHE:/cache" -e GOCACHE=/cache/build -e GOMODCACHE=/cache/mod)
  fi
  docker run --rm "${cache[@]}" -e GOTOOLCHAIN=auto -e CGO_ENABLED=0 \
    -v "$provider_src:/src:ro" -v "$dir:$dir" -w /src golang:1.24-bookworm \
    sh -c "go $cmd -o $dir/$(basename "$out") $* && chown $(id -u):$(id -g) $dir/$(basename "$out")"
}

# The provider binary: ROLL_TF_PROVIDER when it's set (one already built), else built from source.
provider_binary() {
  if [ -n "${ROLL_TF_PROVIDER:-}" ]; then
    mkdir -p "$(dirname "$1")" && cp "$ROLL_TF_PROVIDER" "$1"
  else
    go_build "$1" build .
  fi
}
