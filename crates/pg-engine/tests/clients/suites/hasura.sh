# Hasura GraphQL Engine v2.51.3 (its image; the static binary and the image's python3 for the
# driver) with the server as its metadata database (hasura_meta: hdb_catalog) and as the
# source it serves (hasura_ds, hasura.setup.sql): tracking tables, an enum table, a view,
# relationships, a function and a computed field, permissions and an event trigger (the
# source's catalog introspection runs after each change), the GraphQL schema's
# introspection, queries and mutations (suites/hasura.py). One id per step (suites/apps.py);
# answers in $OUT/responses.txt, Hasura's log in $OUT/hasura.log. APP_PORT: its port (52452).
# image: hasura/graphql-engine:v2.51.3
# src:
set -eu
export HASURA_GRAPHQL_METADATA_DATABASE_URL="postgres://postgres:pw@$PGHOST:$PGPORT/hasura_meta"
export HASURA_GRAPHQL_DATABASE_URL="postgres://postgres:pw@$PGHOST:$PGPORT/hasura_ds"
export HASURA_GRAPHQL_SERVER_HOST=127.0.0.1 HASURA_GRAPHQL_SERVER_PORT=${APP_PORT:-52452}
export HASURA_GRAPHQL_ADMIN_SECRET=secret HASURA_GRAPHQL_ENABLE_TELEMETRY=false
export HASURA_GRAPHQL_ENABLE_CONSOLE=false HASURA_GRAPHQL_DEV_MODE=true
export HASURA_GRAPHQL_ENABLED_LOG_TYPES=startup,http-log,webhook-log,query-log
/bin/graphql-engine serve > "$OUT/hasura.log" 2>&1 &
P=$!
SUITE_NAME=hasura python3 /harness/suites/hasura.py || true
kill $P 2>/dev/null || true
wait $P 2>/dev/null || true
grep -c '^PASS' "$OUT/hasura.lst" || true
