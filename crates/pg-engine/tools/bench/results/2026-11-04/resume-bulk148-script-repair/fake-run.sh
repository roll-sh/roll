#!/bin/sh
set -eu
case "$1" in
pg-start) [ "${FAIL_RUN:-}" != 1 ] || exit 31 ;;
engine-start) mkdir -p "$DATA/engine"; echo 1 > "$DATA/engine.pid" ;;
pg-stop|engine-stop) : ;;
*) exit 92 ;;
esac
