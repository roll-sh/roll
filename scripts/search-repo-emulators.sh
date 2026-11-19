#!/usr/bin/env bash
# The `gcs` and `azure` snapshot repositories' tests against their emulators in Docker:
# fsouza/fake-gcs-server and Azurite, on 127.0.0.1 with 1 GiB each, removed on exit. No test
# calls Google or Azure.
#
#   scripts/search-repo-emulators.sh                 # the repository tests in search-control
#   scripts/search-repo-emulators.sh <command...>    # any command, with the emulators' env set
#
# The tests read ROLL_SEARCH_GCS_TEST_ENDPOINT and ROLL_SEARCH_AZURE_TEST_ENDPOINT (and skip
# themselves without them).
set -euo pipefail
cd "$(dirname "$0")/.."

docker=(docker)
if ! docker info >/dev/null 2>&1 && command -v sg >/dev/null && sg docker -c 'docker info' >/dev/null 2>&1; then
  docker=(sg docker -c)
fi
d() {
  if [[ ${docker[0]} == sg ]]; then sg docker -c "docker $(printf '%q ' "$@")"; else docker "$@"; fi
}

free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
tag="roll-search-repo-$$"
gcs_port=$(free_port)
azure_port=$(free_port)
cleanup() { d rm -f "$tag-gcs" "$tag-azurite" >/dev/null 2>&1 || true; }
trap cleanup EXIT

d run -d --rm --name "$tag-gcs" -m 1g -p "127.0.0.1:$gcs_port:4443" fsouza/fake-gcs-server:latest \
  -scheme http -port 4443 -backend memory -external-url "http://127.0.0.1:$gcs_port" -public-host "127.0.0.1:$gcs_port" >/dev/null
d run -d --rm --name "$tag-azurite" -m 1g -p "127.0.0.1:$azure_port:10000" mcr.microsoft.com/azure-storage/azurite:latest \
  azurite-blob --blobHost 0.0.0.0 --blobPort 10000 --inMemoryPersistence --skipApiVersionCheck >/dev/null

for _ in $(seq 1 60); do
  curl -fs "http://127.0.0.1:$gcs_port/storage/v1/b" >/dev/null 2>&1 && curl -s -o /dev/null "http://127.0.0.1:$azure_port/" && break
  sleep 1
done

export ROLL_SEARCH_GCS_TEST_ENDPOINT="http://127.0.0.1:$gcs_port"
export ROLL_SEARCH_AZURE_TEST_ENDPOINT="http://127.0.0.1:$azure_port/devstoreaccount1"
if (( $# == 0 )); then
  set -- cargo test -p search-control --test control -- a_gcs_repository an_azure_repository --test-threads 2
fi
"$@"
