# Active Record 8.1.4 (Ruby, rails repo): `rake test:postgresql` in activerecord/ (the adapter-
# independent cases plus cases/adapters/postgresql), the pg gem from the repo's Gemfile.lock
# (1.6.3), connecting through libpq's PGHOST / PGPORT / PGUSER / PGPASSWORD to the
# activerecord_unittest{,2} databases its setup.sql makes. Bundler skips the Gemfile groups the
# Active Record tests don't load. Minitest's verbose lines become a .lst.
# image: ruby:3.4.12-bookworm
# src: https://github.com/rails/rails/archive/refs/tags/v8.1.4.tar.gz
set -eu
export BUNDLE_PATH=/cache/bundle/activerecord BUNDLE_WITHOUT=rubocop:mdl:doc:job:cable:storage
export BUNDLE_JOBS=4 GEM_HOME=/cache/bundle/activerecord-gems
cd "$SRC"
bundle check > /dev/null 2>&1 || bundle install --quiet 2>&1 | tail -30
cd activerecord
rm -f test/config.yml
# A first argument starting with test/ is a file glob that replaces the task's file list (e.g.
# 'test/cases/adapters/postgresql/**/*_test.rb': the full run takes hours on the engine).
case ${1:-} in
  test/*) export TEST="$1"; shift ;;
esac
# --verbose: one `Class#test_name = 0.01 s = .|F|E|S` line per test.
bundle exec rake test:postgresql TESTOPTS="--verbose $*" > "$OUT/ar.log" 2>&1 || true
sed -n 's/^\(.*#.*\) = [0-9.]* s = \([.FES]\)$/\2 \1/p' "$OUT/ar.log" |
  awk '{ st = substr($0, 1, 1); n = substr($0, 3)
         print (st == ".") ? "PASS" : (st == "S") ? "SKIP" : "FAIL", n }' > "$OUT/ar.lst"
grep -E '^[0-9]+ runs,' "$OUT/ar.log" || tail -40 "$OUT/ar.log"
