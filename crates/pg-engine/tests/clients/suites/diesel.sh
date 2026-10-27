# Diesel v2.3.14 (Rust): its CI's Postgres job (`cargo xtask run-tests postgres` on stable):
# diesel_cli's `migration run` of migrations/postgres, then every workspace test target (lib,
# bins, tests; nextest in CI, `cargo test` here) with the postgres features, excluding the
# MySQL and SQLite examples: diesel (its unit tests use the database), diesel_tests,
# diesel_cli (each test makes its own database and runs the diesel binary),
# diesel_migrations, diesel-dynamic-schema and the Postgres examples. Doc tests aren't run.
# Results are <test binary>::<test> (binary names without their hash).
# image: rust:1.99.1-bookworm
# src: https://github.com/diesel-rs/diesel/archive/refs/tags/v2.3.14.tar.gz
set -eu
export CARGO_HOME=/cache/cargo CARGO_TARGET_DIR=/cache/target/diesel CARGO_TERM_COLOR=never
export RUSTUP_TOOLCHAIN=1.99.1 CARGO_BUILD_JOBS=4 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
URL="postgres://postgres:pw@$PGHOST:$PGPORT"
export PG_DATABASE_URL="$URL/diesel_test" DATABASE_URL="$URL/diesel_test"
export PG_EXAMPLE_DATABASE_URL="$URL/diesel_example"
# diesel_cli's tests look for the binary in <workspace>/target unless told.
export DIESEL_TEST_BIN=$CARGO_TARGET_DIR/debug/diesel
cd "$SRC"
EXCLUDE=$(for f in examples/mysql/*/Cargo.toml examples/sqlite/*/Cargo.toml; do
  printf -- '--exclude %s ' "$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$f" | head -1)"; done)
FEAT="-F diesel/postgres -F diesel/extras -F diesel_derives/postgres -F diesel_cli/postgres \
  -F migrations_macros/postgres -F diesel_migrations/postgres -F diesel_tests/postgres \
  -F diesel-dynamic-schema/postgres -F diesel/with-deprecated"
cargo run -q -p diesel_cli --no-default-features -F postgres -- migration run \
  --migration-dir migrations/postgres --database-url "$PG_DATABASE_URL" > "$OUT/migrate.log" 2>&1 \
  && echo "PASS xtask::migration_run" > "$OUT/migrate.lst" || echo "FAIL xtask::migration_run" > "$OUT/migrate.lst"
tail -3 "$OUT/migrate.log"
# shellcheck disable=SC2086
if ! cargo test --workspace --no-default-features $EXCLUDE $FEAT --lib --bins --tests --no-run \
  > "$OUT/build.log" 2>&1; then
  grep -E '^error' -A8 "$OUT/build.log" | head -60
  echo "FAIL workspace::build" > "$OUT/build.lst"
fi
# shellcheck disable=SC2086
cargo test --workspace --no-default-features $EXCLUDE $FEAT --lib --bins --tests --no-fail-fast \
  -- --test-threads=8 "$@" > "$OUT/test.log" 2>&1 || true
awk '/^ *Running / { b = $NF; gsub(/[()]/, "", b); sub(/.*\//, "", b); sub(/-[0-9a-f]+$/, "", b) }
  /^test .* \.\.\. / {
    s = $4; sub(/,$/, "", s); st = (s == "ok") ? "PASS" : (s == "FAILED") ? "FAIL" : (s == "ignored") ? "SKIP" : ""
    if (st != "") print st, b "::" $2 }' "$OUT/test.log" > "$OUT/test.lst"
grep -E '^test result: FAILED' "$OUT/test.log" | head -20
echo "pass $(grep -c ^PASS "$OUT/test.lst"), fail $(grep -c ^FAIL "$OUT/test.lst")"
