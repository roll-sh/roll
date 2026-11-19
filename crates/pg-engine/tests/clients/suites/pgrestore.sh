# pg_restore 18.6 into the target: a custom-format archive pg_dump made of a263_restore_src
# (pgrestore.setup.sql: extensions, enums, domains, composite and range types, an ICU collation,
# identity and generated columns, partitioned tables two levels deep with a default partition,
# inheritance, exclusion constraints, partial / expression / GIN / GiST / covering indexes,
# extended statistics, functions, a procedure, an aggregate, an operator, triggers, views, a
# materialized view, a rule, RLS, grants, default privileges, a text search configuration, a
# large object, data), restored in several ways, each into a fresh database:
#   full, jobs (-j 4), clean (restored twice, the second --clean --if-exists), sections (pre-data,
#   data, post-data one at a time), schemadata (-s, then -a --disable-triggers), list (-l, the
#   materialized view's data left out of the list, -L), noowner (-O -x), single (-1), plain
#   (pg_dump -Fp through psql -v ON_ERROR_STOP=1); and self / selfdir: the target's own pg_dump of
#   its a263_restore_src (custom format; directory format with -j 4) restored into it.
# The archive comes from `--from host:port` (tools/compat runs it with PostgreSQL's address for
# both servers: a database moved from PostgreSQL), else from the target itself.
# Checks (checks.lst): restore::<mode> (every step exited 0: no error pg_restore ignored),
# obj::<mode>::<type>:<schema>:<name>#<hash> per object in a schema-only pg_dump of the result,
# data::<mode>::<table>#<rows>:<hash> per table / materialized view (rows as text, sorted),
# seq::<mode>::<sequence>#<state>, lo::<mode>::<oid>#<hash>. An object restored differently shows as
# one id per server.
# image: postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
set -u
FH=$PGHOST FP=$PGPORT
if [ "${1:-}" = --from ]; then FH=${2%:*} FP=${2##*:}; shift 2; fi
W=/cache/work/pgrestore-$TARGET
rm -rf "$W" && mkdir -p "$W" && cd "$W"
L=$OUT/checks.lst
: > "$L"
SRC=a263_restore_src
pass() { echo "PASS $1" >> "$L"; }
fail() { echo "FAIL $1" >> "$L"; }
# step <mode> <command...>: run one restore step, its output in <mode>.log; a failing step fails the mode.
step() {
  m=$1; shift
  echo "+ $*" >> "$W/$m.log"
  "$@" >> "$W/$m.log" 2>&1 || { echo "exit $?" >> "$W/$m.log"; touch "$W/$m.failed"; }
}
fresh() {
  dropdb --if-exists "a263_r_$1" > /dev/null 2>&1
  createdb -T template0 "a263_r_$1"
}
q() { psql -X -q -At -v ON_ERROR_STOP=1 "$@"; }
# snapshot <mode> <db>: the check ids of what <db> holds.
snapshot() {
  m=$1 db=$2
  pg_dump -s -d "$db" > "$W/$m.schema.sql" 2> "$W/$m.dump.err" || { fail "dump::$m"; return; }
  awk -v m="$m" -v out="$W/$m.objs" '
    function flush() { if (key != "") { printf "%s\t%s\n", key, body > out } }
    /^\\(un)?restrict / || /^-- Dumped (from|by)/ { next }
    /^-- Name: / {
      flush(); key = $0; sub(/^-- Name: /, "", key); body = ""; next }
    /^-- (Data for )?Name: / { next }
    { if (key != "") body = body $0 "\\n" }
    END { flush() }' "$W/$m.schema.sql"
  while IFS="$(printf '\t')" read -r key body; do
    # "-- Name: x; Type: T; Schema: s; Owner: o" -> T:s:x (the owner is in the body's ALTER ... OWNER)
    name=${key%%; Type: *}; rest=${key#*; Type: }; typ=${rest%%; Schema: *}
    rest=${rest#*; Schema: }; sch=${rest%%; Owner: *}
    h=$(printf '%s' "$body" | md5sum | cut -c1-10)
    pass "obj::$m::$typ:$sch:$name#$h"
  done < "$W/$m.objs"
  q -d "$db" -F ' ' -c "select format('%I.%I', n.nspname, c.relname) from pg_class c join pg_namespace n on n.oid = c.relnamespace
      where c.relkind in ('r', 'm', 'p') and n.nspname not in ('pg_catalog', 'information_schema') and n.nspname !~ '^pg_toast'
      order by 1" > "$W/$m.tables" 2>> "$W/$m.dump.err"
  while read -r t; do
    r=$(q -d "$db" -c "select count(*) || ':' || md5(coalesce(string_agg(x::text, E'\n' order by x::text), '')) from only $t x" 2>&1 | tr -d '\n' | cut -c1-80)
    pass "data::$m::$t#$r"
  done < "$W/$m.tables"
  q -d "$db" -F '#' -c "select schemaname || '.' || sequencename, coalesce(last_value::text, 'null') from pg_sequences order by 1" 2>&1 |
    while read -r s; do pass "seq::$m::$s"; done
  q -d "$db" -F '#' -c "select oid, md5(lo_get(oid)) from pg_largeobject_metadata order by 1" 2>&1 |
    while read -r s; do pass "lo::$m::$s"; done
}
finish() {
  m=$1
  if [ -e "$W/$m.failed" ]; then
    fail "restore::$m"
    grep -E 'error|ERROR|exit [0-9]' "$W/$m.log" | head -40 | sed "s/^/$m: /" >> "$OUT/errors.txt"
  else
    pass "restore::$m"
  fi
  snapshot "$m" "a263_r_$m"
}

A=$W/src.dump
if ! pg_dump -Fc -h "$FH" -p "$FP" -d $SRC -f "$A" 2> "$W/src.err"; then
  fail "archive"; cat "$W/src.err" >> "$OUT/errors.txt"; exit 0
fi
pass archive
pg_dump -Fp -h "$FH" -p "$FP" -d $SRC -f "$W/src.sql" 2>> "$W/src.err"

fresh full; step full pg_restore -d a263_r_full "$A"; finish full
fresh jobs; step jobs pg_restore -j 4 -d a263_r_jobs "$A"; finish jobs
fresh clean; step clean pg_restore -d a263_r_clean "$A"
step clean pg_restore --clean --if-exists -d a263_r_clean "$A"; finish clean
fresh sections
for s in pre-data data post-data; do step sections pg_restore --section=$s -d a263_r_sections "$A"; done
finish sections
fresh schemadata; step schemadata pg_restore -s -d a263_r_schemadata "$A"
step schemadata pg_restore -a --disable-triggers -d a263_r_schemadata "$A"; finish schemadata
fresh list
pg_restore -l "$A" | grep -v 'MATERIALIZED VIEW DATA' > "$W/list.txt"
step list pg_restore -L "$W/list.txt" -d a263_r_list "$A"; finish list
fresh noowner; step noowner pg_restore -O -x -d a263_r_noowner "$A"; finish noowner
fresh single; step single pg_restore -1 -d a263_r_single "$A"; finish single
fresh plain; step plain psql -X -q -v ON_ERROR_STOP=1 -d a263_r_plain -f "$W/src.sql"; finish plain
# The target's own dumps.
fresh self; step self pg_dump -Fc -d $SRC -f "$W/self.dump"
step self pg_restore -d a263_r_self "$W/self.dump"; finish self
fresh selfdir; step selfdir pg_dump -Fd -j 4 -d $SRC -f "$W/self.dir"
step selfdir pg_restore -j 4 -d a263_r_selfdir "$W/self.dir"; finish selfdir
snapshot src $SRC
for m in full jobs clean sections schemadata list noowner single plain self selfdir; do
  dropdb --if-exists "a263_r_$m" > /dev/null 2>&1
done
grep -c '^PASS' "$L"; grep '^FAIL' "$L" | head -40
cat "$OUT/errors.txt" 2> /dev/null | head -80
