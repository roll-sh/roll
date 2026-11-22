#!/bin/sh
# The split views' rows of pg_catalog's and information_schema's objects (`system::split_view`,
# `catalog/split_rows.rs`), exported from a PostgreSQL 18.6 container of the pinned image made
# with --locale=C --encoding=UTF8 (tools/ref-containers.sh) into src/catalog/data/split_rows/:
#
#   tools/export-split-views.sh <container>
#
# Each line: "t" when a role of no memberships sees the row (the database owner sees the
# same: checked), else "f" (the superuser's alone), then the view's columns in COPY text
# format. Exported in database a_split as the bootstrap superuser postgres; the *_catalog
# columns hold the database's name and pg_tables.tableowner the superuser's, which the engine
# puts in when it reads them (both checked here).
set -eu
C=${1:?usage: export-split-views.sh <container>}
OUT=$(cd "$(dirname "$0")/.." && pwd)/src/catalog/data/split_rows
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT
psql() { docker exec -i "$C" psql -X -q -v ON_ERROR_STOP=1 "$@"; }
psql -U postgres -c 'drop database if exists a_split' -c 'drop role if exists a_split_owner' \
  -c 'drop role if exists a_split_plain' -c 'create role a_split_owner login' \
  -c 'create role a_split_plain login' -c 'create database a_split owner a_split_owner'
for v in information_schema.tables:table_schema information_schema.columns:table_schema \
  information_schema.table_constraints:table_schema information_schema.key_column_usage:table_schema \
  information_schema.constraint_column_usage:table_schema \
  information_schema.check_constraints:constraint_schema \
  pg_catalog.pg_tables:schemaname pg_catalog.pg_indexes:schemaname; do
  n=${v%%:*}; c=${v#*:}
  for u in postgres a_split_plain a_split_owner; do
    psql -U "$u" -d a_split -c "copy (select * from $n where $c in ('pg_catalog', 'information_schema')) to stdout" > "$TMP/${n#*.}.$u"
    psql -U "$u" -d a_split -At -c "select string_agg(attname, ',' order by attnum) from pg_attribute where attrelid = '$n'::regclass and attnum > 0" > "$TMP/${n#*.}.cols"
  done
done
mkdir -p "$OUT"
python3 -I - "$TMP" "$OUT" <<'EOF'
import os, subprocess, sys
tmp, out = sys.argv[1:]
for f in sorted(os.listdir(tmp)):
    if not f.endswith('.postgres'):
        continue
    v = f[:-len('.postgres')]
    rd = lambda u: open(f'{tmp}/{v}.{u}').read().splitlines()
    sup, plain, owner = rd('postgres'), rd('a_split_plain'), rd('a_split_owner')
    cols = open(f'{tmp}/{v}.cols').read().strip().split(',')
    assert sorted(plain) == sorted(owner), f'{v}: the owner sees other rows than a plain role'
    assert len(set(sup)) == len(sup) and set(plain) <= set(sup), f'{v}: plain rows not the superuser\'s'
    for l in sup:
        for name, x in zip(cols, l.split('\t'), strict=True):
            if name.endswith('_catalog'):
                assert x in ('a_split', '\\N'), f'{v}.{name}: {x}'
            elif name == 'tableowner':
                assert x == 'postgres', f'{v}.{name}: {x}'
            else:
                assert x != 'a_split', f'{v}.{name} holds the database name'
    seen = set(plain)
    text = ''.join(('t' if l in seen else 'f') + '\t' + l + '\n' for l in sup)
    open(f'{out}/{v}.tsv.zst', 'wb').write(subprocess.run(['zstd', '-19', '-q', '-c'], input=text.encode(), stdout=subprocess.PIPE, check=True).stdout)
    print(f'{v}: {len(sup)} rows, {len(plain)} a plain role\'s')
EOF
psql -U postgres -c 'drop database a_split' -c 'drop role a_split_owner' -c 'drop role a_split_plain'
