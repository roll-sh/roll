# SeaORM 2.0.4 (Rust): its CI's Postgres job: every integration test target (`--test '*'`) of
# sea-orm with tests-features,sqlx-postgres,runtime-tokio-native-tls, then sea-orm-migration's
# integration tests. DATABASE_URL names the server without a database: each test makes and drops
# its own. Results are <crate>/<test binary>::<test>.
# image: rust:1.99.1-bookworm
# src: https://github.com/SeaQL/sea-orm/archive/refs/tags/2.0.4.tar.gz
set -eu
export CARGO_HOME=/cache/cargo CARGO_TERM_COLOR=never
export RUSTUP_TOOLCHAIN=1.99.1 CARGO_BUILD_JOBS=4 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0
export DATABASE_URL="postgres://postgres:pw@$PGHOST:$PGPORT"
cd "$SRC"
run() { # run <name> <cargo test args...>
  n=$1; shift
  if ! cargo test "$@" --no-run > "$OUT/$n.build.log" 2>&1; then
    grep -E '^error' -A8 "$OUT/$n.build.log" | head -60
    echo "FAIL $n::build" > "$OUT/$n.build.lst"
  fi
  cargo test "$@" --no-fail-fast > "$OUT/$n.log" 2>&1 || true
  awk -v p="$n" '/^ *Running / { b = $NF; gsub(/[()]/, "", b); sub(/.*\//, "", b); sub(/-[0-9a-f]+$/, "", b) }
    /^test .* \.\.\. / {
      s = $4; sub(/,$/, "", s); st = (s == "ok") ? "PASS" : (s == "FAILED") ? "FAIL" : (s == "ignored") ? "SKIP" : ""
      if (st != "") print st, p "/" b "::" $2 }' "$OUT/$n.log" > "$OUT/$n.lst"
  echo "$n: pass $(grep -c ^PASS "$OUT/$n.lst"), fail $(grep -c ^FAIL "$OUT/$n.lst")"
  grep -E '^test .* FAILED$' "$OUT/$n.log" | head -40
}
CARGO_TARGET_DIR=/cache/target/seaorm run sea-orm --test '*' \
  --features tests-features,sqlx-postgres,runtime-tokio-native-tls
CARGO_TARGET_DIR=/cache/target/seaorm-migration run sea-orm-migration \
  --manifest-path sea-orm-migration/Cargo.toml --test '*' \
  --features sqlx-postgres,runtime-tokio-native-tls
