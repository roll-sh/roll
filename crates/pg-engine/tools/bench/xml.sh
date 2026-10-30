#!/bin/sh
# XML entry points against PostgreSQL 18.6: time, CPU and peak memory per entry point and
# document size. See BENCH.md, "XML".
#
#   tools/bench/xml.sh engine-start | engine-stop     (the release engine, XB_BIN)
#   tools/bench/xml.sh setup <pg|engine>              (database xmlbench: documents, tables)
#   tools/bench/xml.sh run <pg|engine> [case-regex] [size-regex]   (TSV on stdout)
#   tools/bench/xml.sh compare <pg.tsv> <engine.tsv>  (one table)
#   tools/bench/xml.sh psql <pg|engine>              (SQL on stdin, database xmlbench)
#   tools/bench/xml.sh cleanup                        (removes the psql container)
#
# Each case runs in a fresh connection: once with the server process's peak RSS reset
# (/proc/<pid>/clear_refs), reporting the peak over the RSS before it, then XB_REPS more times
# for the time (the minimum, client-side, psql \timing) and the CPU per run (utime + stime of
# the backend, or of the engine process). psql runs in a container of the pinned image on the
# host network and pid namespace (xmlmem.sh). Every query returns one short row.
set -eu
if [ -z "${XB_SG:-}" ] && ! docker info >/dev/null 2>&1; then
  q=""; for a in "$0" "$@"; do q="$q '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"; done
  XB_SG=1 exec sg docker -c "XB_SG=1 sh $q"
fi
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../../../.." && pwd)
P=${XB_PREFIX:-a107}
DATA=${XB_DATA:-$HOME/.cache/$P/xb}
PG=${XB_PG:-$P-ref}                 # a private reference container (HANDOFF recipe)
PG_PORT=${XB_PG_PORT:-52140}         # its published port on 127.0.0.1
ENGINE_PORT=${XB_ENGINE_PORT:-52141}
BIN=${XB_BIN:-${CARGO_TARGET_DIR:-$ROOT/target}/release/roll-engine}
HELPER=$P-xbpsql
IMG=postgres@sha256:77f585114c32fbca283dc835b0596f4e52b51b4c6662d7810b2f4084f60a1873
SIZES=${XB_SIZES:-1k 100k 1m 10m 50m}
mkdir -p "$DATA"

port() { if [ "$1" = pg ]; then echo "$PG_PORT"; else echo "$ENGINE_PORT"; fi; }
helper() {
  docker inspect "$HELPER" >/dev/null 2>&1 || docker run -d --name "$HELPER" --network host \
    --pid host --user 0 -v "$HERE:/xb:ro" --entrypoint sleep "$IMG" infinity >/dev/null
}
psql_in() { # psql_in <pg|engine> <db>: stdin to psql in the helper
  if [ "$1" = pg ]; then env="-e XB_PG_INIT=$(docker inspect -f '{{.State.Pid}}' "$PG")"
  else env="-e XB_ENGINE_PID=$(cat "$DATA/engine.pid")"; fi
  # shellcheck disable=SC2086
  docker exec -i $env -e PGPASSWORD=pw "$HELPER" psql -X -q -At -v ON_ERROR_STOP=0 \
    -h 127.0.0.1 -p "$(port "$1")" -U postgres -d "$2" 2>&1
}
# rows per size: each row is about 90 bytes
rows() { case $1 in 1k) echo 11;; 100k) echo 1130;; 1m) echo 11600;; 10m) echo 116000;; 50m) echo 580000;; esac; }

cases() { cat <<'EOF'
baseline|select length(t) from xb_d_SIZE
parse_cast|select t::xml is not null from xb_d_SIZE
xmlparse_document|select xmlparse(document t) is not null from xb_d_SIZE
xmlparse_content|select xmlparse(content t) is not null from xb_d_SIZE
well_formed|select xml_is_well_formed_document(t) from xb_d_SIZE
xml_out|select length(d::text) from xb_d_SIZE
xmlserialize|select length(xmlserialize(document d as text)) from xb_d_SIZE
xmlserialize_indent|select length(xmlserialize(document d as text indent)) from xb_d_SIZE
xmlconcat|select length(xmlconcat(d, d)::text) from xb_d_SIZE
xpath_nodes|select array_length(xpath('/rows/row/a/text()', d), 1) from xb_d_SIZE
xpath_count|select (xpath('count(/rows/row)', d))[1]::text from xb_d_SIZE
xpath_exists|select xpath_exists('/rows/row[b = 5]', d) from xb_d_SIZE
xmltable|select count(*), sum(x.b) from xb_d_SIZE, xmltable('/rows/row' passing d columns id int path '@id', a text path 'a', b int path 'b') x
xmltable_xml|select count(*), sum(length(x.r::text)) from xb_d_SIZE, xmltable('/rows/row' passing d columns r xml path '.') x
build_xmlagg|select length(xmlelement(name rows, xmlagg(xmlelement(name row, xmlattributes(id as id), xmlforest(a, b))))::text) from xb_rows where id <= ROWS
build_xmlagg_flat|select length(xmlagg(xmlelement(name r, a))::text) from xb_rows where id <= ROWS
query_to_xml|select length(query_to_xml('select i id, md5(i::text) a, i * 2 b from generate_series(1, ROWS) i', true, false, '')::text)
table_to_xml|select length(table_to_xml('xb_t_SIZE', true, false, '')::text)
xml2_xpath_string|select length(xpath_string(t, '/rows/row[last()]/a')) from xb_d_SIZE
xml2_xpath_nodeset|select length(xpath_nodeset(t, '/rows/row/a', 'r', 'i')) from xb_d_SIZE
xml2_xpath_list|select length(xpath_list(t, '/rows/row/b')) from xb_d_SIZE
xml2_xpath_number|select xpath_number(t, 'count(/rows/row)') from xb_d_SIZE
xml2_xpath_bool|select xpath_bool(t, '/rows/row[b = 5]') from xb_d_SIZE
xml2_xpath_table|select count(*) from xpath_table('k', 't', 'xb_d_SIZE', '/rows/row/a|/rows/row/b', 'true') as x(k text, a text, b text)
xslt_identity|select length(xslt_process(t, (select s from xb_style))) from xb_d_SIZE
EOF
}

engine_start() {
  if ss -ltn | grep -q ":$ENGINE_PORT "; then echo "port $ENGINE_PORT busy" >&2; exit 1; fi
  mkdir -p "$DATA/engine"
  "$BIN" --data "$DATA/engine" --listen 127.0.0.1:$ENGINE_PORT --password pw >>"$DATA/engine.log" 2>&1 &
  echo $! >"$DATA/engine.pid"
  i=0; until ss -ltn | grep -q ":$ENGINE_PORT "; do sleep 0.2; i=$((i+1)); [ $i -lt 300 ]; done
}
engine_stop() {
  if [ -f "$DATA/engine.pid" ]; then
    kill "$(cat "$DATA/engine.pid")" 2>/dev/null || true
    while kill -0 "$(cat "$DATA/engine.pid")" 2>/dev/null; do sleep 0.2; done
    rm -f "$DATA/engine.pid"
  fi
}

case ${1:-} in
engine-start)
  engine_start
  echo "engine pid $(cat "$DATA/engine.pid")"
  ;;
engine-stop)
  engine_stop
  ;;
setup)
  helper
  echo "drop database if exists xmlbench; create database xmlbench;" | psql_in "$2" postgres
  {
    echo "create extension xml2;"
    echo "create table xb_docs(k text primary key, t text, d xml);"
    echo "create table xb_rows as select i id, md5(i::text) a, i * 2 b from generate_series(1, $(rows 50m)) i;"
    echo "alter table xb_rows add primary key (id); analyze xb_rows;"
    echo "create table xb_style(s text);"
    echo "insert into xb_style values ('<xsl:stylesheet version=\"1.0\" xmlns:xsl=\"http://www.w3.org/1999/XSL/Transform\"><xsl:template match=\"@*|node()\"><xsl:copy><xsl:apply-templates select=\"@*|node()\"/></xsl:copy></xsl:template></xsl:stylesheet>');"
    for s in $SIZES; do
      echo "insert into xb_docs select '$s', s, s::xml from (select '<rows>' || string_agg(format('<row id=\"%s\"><a>%s</a><b>%s</b><c>x &amp; y</c></row>', i, md5(i::text), i) || chr(10), '' order by i) || '</rows>' s from generate_series(1, $(rows "$s")) i) q;"
      echo "create table xb_t_$s as select * from xb_rows where id <= $(rows "$s");"
      echo "create table xb_d_$s as select * from xb_docs where k = '$s';"
    done
    echo "select k, length(t) from xb_docs order by length(t);"
  } | psql_in "$2" xmlbench
  ;;
run)
  helper
  side=$2; cre=${3:-.*}; sre=${4:-.*}
  cases | grep -E "^($cre)\|" | while IFS='|' read -r name q; do
    for s in $SIZES; do
      echo "$s" | grep -qE "^($sre)$" || continue
      # PostgreSQL's xmlagg is quadratic (each row concatenates onto the result so far: 1 MB
      # takes seconds, 10 MB most of an hour), and so is xml2's xpath_table (100 KB 0.2 s,
      # 1 MB over a minute).
      case $name:$s in build_xmlagg*:10m | build_xmlagg*:50m | xml2_xpath_table:10m | xml2_xpath_table:50m) continue ;; esac
      sq=$(echo "$q" | sed "s/SIZE/$s/g; s/ROWS/$(rows "$s")/g")
      case $s in 10m) reps=2;; 50m) reps=0;; *) reps=${XB_REPS:-5};; esac
      # The engine is one process whose allocator keeps freed memory for reuse: a fresh one
      # per case, so a case's peak isn't hidden by what an earlier one left resident.
      if [ "$side" = engine ] && [ "${XB_RESTART:-1}" = 1 ]; then engine_stop; engine_start; fi
      out=$({
        # printf, not echo: dash's echo reads \t in \timing as a tab
        # Read the case's data once first: the peak is the XML work's, not the first read
        # of a row (the engine's scan decodes every row's large values on the way).
        printf '%s\n' 'select pg_backend_pid() as bpid \gset' '\setenv XB_BPID :bpid' '\o /dev/null' \
          "select length(t), length(d::text) from xb_d_$s;" \
          "select sum(length(a)) from xb_rows where id <= $(rows "$s");" \
          "select sum(length(a)) from xb_t_$s;" 'select length(s) from xb_style;' '\timing on' \
          '\! sh /xb/xmlmem.sh reset $XB_BPID' "$sq;" '\! sh /xb/xmlmem.sh read $XB_BPID'
        if [ "$reps" -gt 0 ]; then
          printf '%s\n' '\! sh /xb/xmlmem.sh reset $XB_BPID'
          i=0; while [ $i -lt "$reps" ]; do printf '%s\n' "$sq;"; i=$((i+1)); done
          printf '%s\n' '\! sh /xb/xmlmem.sh read $XB_BPID'
        fi
      } | psql_in "$side" xmlbench)
      echo "$out" | awk -v n="$name" -v s="$s" -v r="$reps" -v side="$side" '
        /^MEM / { m++; if (m == 1) { peak = $2; cpu = $3 } else cpu = $3 / r }
        /^Time: / { t = $2; if (min == "" || t < min) min = t }
        /ERROR/ && err == "" { err = $0 }
        END { if (err != "") printf "%s\t%s\t%s\tERR\t\t\t%s\n", side, n, s, err
              else printf "%s\t%s\t%s\t%.1f\t%s\t%s\n", side, n, s, min, cpu, peak }'
    done
  done
  ;;
compare)
  # case, size, then ms / cpu ms / peak MB for PostgreSQL and the engine
  awk -F'\t' 'NR == FNR { pg[$2 "\t" $3] = $4 "\t" $5 "\t" $6; next }
    { k = $2 "\t" $3; printf "%s\t%s\t%s\t%s\t%s\n", k, pg[k], $4, $5, $6 }' "$2" "$3"
  ;;
psql)
  helper
  psql_in "$2" xmlbench
  ;;
cleanup)
  docker rm -f "$HELPER" >/dev/null 2>&1 || true
  ;;
*) sed -n '2,15p' "$0"; exit 1 ;;
esac
