# Doctrine DBAL 4.5.0 (PHP): its PHPUnit suite with CI's ci/github/phpunit/pdo_pgsql.xml (the
# functional tests under tests/Functional run against the server; the rest are unit tests),
# its db_host / db_password pointed at the server as postgres / pw plus a db_port. The tag
# tarball leaves tests/ and ci/ out (export-ignore), so the suite fetches them and
# phpunit.xml.dist once, file by file, from the tag's git tree. Dependencies as its CI
# (ramsey/composer-install, highest: composer update --prefer-dist; no lockfile, resolved once
# and reused from the cache). PHPUnit's JUnit log.
# image: serversideup/php@sha256:033909684109ac1386d48a1184493f093a748fa28c1d1f6d78da80474e4a10ba
# src: https://github.com/doctrine/dbal/archive/refs/tags/4.5.0.tar.gz
set -eu
export COMPOSER_HOME=/cache/home/composer COMPOSER_CACHE_DIR=/cache/home/composer-cache
export COMPOSER_NO_INTERACTION=1
REPO=doctrine/dbal REF=4.5.0
if [ ! -f .tests-fetched ]; then
  curl -fsSL "https://api.github.com/repos/$REPO/git/trees/$REF?recursive=1" -o "$OUT/tree.json"
  php -r '
    $t = json_decode(file_get_contents($argv[1]), true);
    foreach ($t["tree"] as $e) {
      if ($e["type"] !== "blob") continue;
      $p = $e["path"];
      if (strncmp($p, "tests/", 6) && strncmp($p, "ci/", 3) && $p !== "phpunit.xml.dist") continue;
      printf("url = \"https://raw.githubusercontent.com/%s/%s/%s\"\noutput = \"%s\"\n",
        $argv[2], $argv[3], $p, $p);
    }' "$OUT/tree.json" "$REPO" "$REF" > "$OUT/fetch.cfg"
  curl -fsS --retry 3 --parallel --parallel-max 16 --create-dirs -K "$OUT/fetch.cfg"
  touch .tests-fetched
fi
if [ ! -f vendor/autoload.php ]; then
  composer update --prefer-dist --no-progress --ignore-platform-req=php+ > "$OUT/install.log" 2>&1 ||
    { tail -30 "$OUT/install.log"; exit 1; }
fi
sed -e "s|name=\"db_host\" value=\"localhost\"|name=\"db_host\" value=\"$PGHOST\"|" \
  -e "s|name=\"db_password\" value=\"postgres\" />|name=\"db_password\" value=\"$PGPASSWORD\" />\n        <var name=\"db_port\" value=\"$PGPORT\" />|" \
  ci/github/phpunit/pdo_pgsql.xml > ci/github/phpunit/roll-pdo_pgsql.xml
timeout 5400 vendor/bin/phpunit -c ci/github/phpunit/roll-pdo_pgsql.xml --log-junit "$OUT/dbal.xml" \
  --do-not-cache-result "$@" > "$OUT/phpunit.log" 2>&1 || true
grep -c '<testcase' "$OUT/dbal.xml" || tail -30 "$OUT/phpunit.log"
