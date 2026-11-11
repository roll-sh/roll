# Laravel 13.38.0 (laravel/framework, PHP): tests/Integration/Database with DB_CONNECTION=pgsql
# (its CI's PostgreSQL job: vendor/bin/phpunit tests/Integration/Database as forge / password on
# the laravel database; the PostgreSQL-specific Postgres/ tests and the general Eloquent / schema
# / query builder ones). GitHub's tag tarball leaves tests/ out (export-ignore), so the suite
# fetches tests/ and phpunit.xml.dist once, file by file, from the tag's git tree. Dependencies
# as its CI: `composer config version 13.x-dev` and `composer update --prefer-stable
# --prefer-dist`, ignoring ext-gmp / ext-ftp, which the image lacks and only non-database tests
# use (no lockfile; resolved once, then reused from the cache). PHPUnit's JUnit log.
# image: serversideup/php@sha256:033909684109ac1386d48a1184493f093a748fa28c1d1f6d78da80474e4a10ba
# src: https://github.com/laravel/framework/archive/refs/tags/v13.38.0.tar.gz
set -eu
export COMPOSER_HOME=/cache/home/composer COMPOSER_CACHE_DIR=/cache/home/composer-cache
export COMPOSER_NO_INTERACTION=1 COMPOSER_ALLOW_SUPERUSER=1
REPO=laravel/framework REF=v13.38.0
if [ ! -f .tests-fetched ]; then
  curl -fsSL "https://api.github.com/repos/$REPO/git/trees/$REF?recursive=1" -o "$OUT/tree.json"
  php -r '
    $t = json_decode(file_get_contents($argv[1]), true);
    foreach ($t["tree"] as $e) {
      if ($e["type"] !== "blob") continue;
      if (strncmp($e["path"], "tests/", 6) && $e["path"] !== "phpunit.xml.dist") continue;
      printf("url = \"https://raw.githubusercontent.com/%s/%s/%s\"\noutput = \"%s\"\n",
        $argv[2], $argv[3], $e["path"], $e["path"]);
    }' "$OUT/tree.json" "$REPO" "$REF" > "$OUT/fetch.cfg"
  curl -fsS --retry 3 --parallel --parallel-max 16 --create-dirs -K "$OUT/fetch.cfg"
  touch .tests-fetched
fi
if [ ! -f vendor/autoload.php ]; then
  { composer config version 13.x-dev &&
    composer update --prefer-stable --prefer-dist --no-progress \
      --ignore-platform-req=ext-gmp --ignore-platform-req=ext-ftp; } > "$OUT/install.log" 2>&1 ||
    { tail -30 "$OUT/install.log"; exit 1; }
fi
export DB_CONNECTION=pgsql DB_HOST="$PGHOST" DB_PORT="$PGPORT" DB_DATABASE=laravel
export DB_USERNAME=forge DB_PASSWORD=password
[ $# -gt 0 ] || set -- tests/Integration/Database
timeout 5400 php -d memory_limit=2G vendor/bin/phpunit --log-junit "$OUT/laravel.xml" \
  --do-not-cache-result "$@" > "$OUT/phpunit.log" 2>&1 || true
grep -c '<testcase' "$OUT/laravel.xml" || tail -30 "$OUT/phpunit.log"
