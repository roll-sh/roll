#!/bin/sh
# Job B: record the a165 golden, crate tests in debug and release, clippy, the dist engine
# (bin/new2) and the prof engine.
. $HOME/.cache/a165/env.sh
cd $W
git -C $W log --oneline -1
echo "== record"
GOLDEN_RECORD=127.0.0.1:53310 GOLDEN_PASSWORD=pw GOLDEN_FILTER=a165_cursor_stream cargo test -q -p pg-engine --test golden 2>&1 | tail -5
wc -l crates/pg-engine/tests/sql/a165_cursor_stream.out
echo "== debug tests"
cargo test -p pg-engine --no-fail-fast > $HOME/.cache/a165/logs/test-debug.out 2>&1
echo "debug exit $?"
grep -E '^test result|FAILED|^failures:|panicked at|^error' $HOME/.cache/a165/logs/test-debug.out | grep -v 'test result: ok' | head -40
echo "== release tests"
cargo test --release -p pg-engine --no-fail-fast > $HOME/.cache/a165/logs/test-release.out 2>&1
echo "release exit $?"
grep -E '^test result|FAILED|^failures:|panicked at|^error' $HOME/.cache/a165/logs/test-release.out | grep -v 'test result: ok' | head -40
echo "== clippy"
cargo clippy -p pg-engine --release 2>&1 | grep -E -A4 '^(warning|error)' | grep -E '^(warning|error)|-->' | head -40
echo "== dist"
cargo build --profile dist -p pg-engine --bin roll-engine && mkdir -p $HOME/.cache/a165/bin/new2 && cp $CARGO_TARGET_DIR/dist/roll-engine $HOME/.cache/a165/bin/new2/
echo "dist exit $?"
echo "== prof"
$HOME/.cache/a165/scripts/build-prof.sh
echo "prof exit $?"
