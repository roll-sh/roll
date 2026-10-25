#!/bin/bash
# Docker HEALTHCHECK for a roll code node: GET /v1/code/health on the API port answers 200 while
# the node can serve (its database answers; in a cluster, it holds its leases and isn't draining).
# bash's /dev/tcp, so the image needs no curl.
set -eu
port=${ROLL_API##*:}
exec 3<>"/dev/tcp/127.0.0.1/${port:-7070}"
printf 'GET /v1/code/health HTTP/1.0\r\nHost: localhost\r\n\r\n' >&3
read -r -t 3 _ status _ <&3
[ "$status" = 200 ]
