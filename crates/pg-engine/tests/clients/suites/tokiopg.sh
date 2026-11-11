# tokio-postgres 0.7.18 (Rust, rust-postgres repo): tokio-postgres's tests/test (all features),
# the sync `postgres` crate's tests and postgres-derive-test, from a patched copy: port 5433 ->
# $PGPORT, `user=postgres` gets password and the a51_tokiopg database (its setup.sql makes it as
# docker/sql_setup.sh does). The servers here authenticate every role with SCRAM and have no
# SSL, so the plain / md5 password tests are skipped and the TLS crates are not run.
# image: rust:1.99.1-bookworm
# src: https://github.com/sfackler/rust-postgres/archive/refs/tags/tokio-postgres-v0.7.18.tar.gz
set -eu
export CARGO_HOME=/cache/cargo CARGO_TARGET_DIR=/cache/target/tokiopg CARGO_TERM_COLOR=never
W=/cache/work/tokiopg
rm -rf $W && mkdir -p $W && cp -a "$SRC/." $W/
cd $W
DB=a51_tokiopg
for f in tokio-postgres/tests/test/*.rs tokio-postgres/tests/test/types/*.rs postgres/src/test.rs \
  postgres-derive-test/src/*.rs; do
  sed -i -e "s/5433/$PGPORT/g" -e "s/user=postgres/user=postgres password=pw dbname=$DB/g" \
    -e "s/dbname=postgres/dbname=$DB/g" "$f"
done
SKIP="--skip plain_password_ --skip md5_password_"
# run <crate> <cargo args>: one crate's tests as PASS/FAIL/SKIP <crate>::<test> lines.
run() {
  c=$1; shift
  if ! cargo test -q -p "$c" "$@" --no-run > "$OUT/$c.build.log" 2>&1; then
    tail -40 "$OUT/$c.build.log"
    echo "FAIL $c::build" > "$OUT/$c.lst"
    return
  fi
  cargo test -p "$c" "$@" -- --test-threads=4 $SKIP > "$OUT/$c.log" 2>&1 || true
  awk -v p="$c" '/^test .* \.\.\. / {
      s = $4; sub(/,$/, "", s)
      st = (s == "ok") ? "PASS" : (s == "FAILED") ? "FAIL" : (s == "ignored") ? "SKIP" : ""
      if (st != "") print st, p "::" $2 }' "$OUT/$c.log" > "$OUT/$c.lst"
  grep -E '^test result|panicked' "$OUT/$c.log" | head -60
}
run tokio-postgres --all-features --lib --tests
run postgres --lib --tests
run postgres-derive-test --lib --tests
