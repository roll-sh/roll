#!/usr/bin/env bash
# Drops <keyspace> on <container> and clears every snapshot that names it (including the
# automatic `dropped-*` / `pre-drop-*` snapshots a DROP takes): cleanup.sh <container> <keyspace>
set -uo pipefail
c=$1 ks=$2
docker exec "$c" cqlsh -e "DROP KEYSPACE IF EXISTS $ks" >/dev/null 2>&1
for tag in $(docker exec "$c" nodetool listsnapshots 2>/dev/null | awk -v ks="$ks" '$2 == ks {print $1}' | sort -u); do
  docker exec "$c" nodetool clearsnapshot -t "$tag" -- "$ks" >/dev/null 2>&1 || docker exec "$c" nodetool clearsnapshot -t "$tag" >/dev/null 2>&1
done
