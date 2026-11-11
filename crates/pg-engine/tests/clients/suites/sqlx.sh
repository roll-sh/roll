# sqlx 0.9.0 (Rust): the postgres* test targets of the sqlx crate (tests/postgres), with the
# features of its CI's Postgres job (tokio runtime, no TLS), against the a51_sqlx database its
# setup.sql makes. The query! macros check their SQL against DATABASE_URL at compile time, so
# the sqlx crate's own build is cleaned first: the macros see the server under test. Each
# target is built and run on its own (a target that doesn't compile is one `build` failure).
# Skipped as in CI's main run: test_pg_copy_chunked, rustsec_2024_0363 (PG_ISOLATED_TESTS).
# image: rust:1.99.1-bookworm
# src: https://github.com/launchbadge/sqlx/archive/refs/tags/v0.9.0.tar.gz
set -eu
export CARGO_HOME=/cache/cargo CARGO_TARGET_DIR=/cache/target/sqlx CARGO_TERM_COLOR=never
export RUSTUP_TOOLCHAIN=1.99.1 RUSTFLAGS='--cfg postgres="17"'
export DATABASE_URL="postgres://postgres:pw@$PGHOST:$PGPORT/a51_sqlx"
FEAT=any,postgres,macros,migrate,_unstable-all-types,runtime-tokio,tls-none
cd "$SRC"
cargo clean -q -p sqlx 2>/dev/null || true
for t in postgres postgres-types postgres-describe postgres-macros postgres-derives postgres-error \
  postgres-test-attr postgres-migrate postgres-query-builder; do
  if ! cargo test -q --no-default-features --features $FEAT --test $t --no-run \
    > "$OUT/$t.build.log" 2>&1; then
    grep -E '^error' -A8 "$OUT/$t.build.log" | head -60
    echo "FAIL $t::build" > "$OUT/$t.lst"
    continue
  fi
  cargo test --no-default-features --features $FEAT --test $t -- --test-threads=4 \
    --skip test_pg_copy_chunked --skip rustsec_2024_0363 > "$OUT/$t.log" 2>&1 || true
  awk -v p="$t" '/^test .* \.\.\. / {
      s = $4; sub(/,$/, "", s)
      st = (s == "ok") ? "PASS" : (s == "FAILED") ? "FAIL" : (s == "ignored") ? "SKIP" : ""
      if (st != "") print st, p "::" $2 }' "$OUT/$t.log" > "$OUT/$t.lst"
  grep -E '^test result' "$OUT/$t.log" | sed "s/^/$t: /"
done
