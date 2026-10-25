# postgres.js v3.4.7 (JS): tests/index.js, with the runner patched to run every test (it stops
# at the first failure) and to print PASS / FAIL per test, and the test role given a password
# (both servers use SCRAM for TCP). The bootstrap's ALTER SYSTEM / ssl steps are skipped.
# image: node:22-bookworm-slim
# src: https://github.com/porsager/postgres/archive/refs/tags/v3.4.7.tar.gz
set -eu
W=/cache/work/postgresjs
rm -rf $W && mkdir -p $W && cp -r "$SRC/." $W/
cd $W
sed -i "s/      ? (ignored++, ignore)/      ? fn()/" tests/test.js
sed -i "s/      process.stdout.write('✅')/      console.log('RESULT PASS', line + ':' + name)/" tests/test.js
sed -i "s/      tests\[line\].failed = failed = true/      console.log('RESULT FAIL', line + ':' + name, String(err).slice(0, 300).replace(\/\\\\n\/g, ' ')); tests[line].failed = failed = true/" tests/test.js
sed -i "s/^  user: 'postgres_js_test'$/  user: 'postgres_js_test', pass: 'postgres_js_test'/" tests/index.js
sed -i "s/^exec('/\/\/exec('/" tests/bootstrap.js
# (Open handles keep node running after the last test.)
sed -i "s/^  !process.exitCode \&\& (!success/  setTimeout(() => process.exit(0), 500)\n  !process.exitCode \&\& (!success/" tests/test.js
export PGUSER=postgres_js_test PGPASSWORD=postgres_js_test PGDATABASE=postgres_js_test
timeout 900 node tests/index.js > "$OUT/log.txt" 2>&1 || true
grep '^RESULT ' "$OUT/log.txt" | sed 's/^RESULT //' | awk '{print $1, $2}' > "$OUT/postgresjs.lst"
grep '^RESULT FAIL' "$OUT/log.txt" | head -100
tail -5 "$OUT/log.txt"
