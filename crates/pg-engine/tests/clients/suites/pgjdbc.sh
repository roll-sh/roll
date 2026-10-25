# pgjdbc 42.7.7 (Java): its JUnit tests for the jdbc2 / jdbc3 / jdbc4 / jdbc42 / core
# packages, against database "test" owned by user "test" (docker/postgres-server's setup).
# image: eclipse-temurin:21-jdk
# src: https://github.com/pgjdbc/pgjdbc/archive/refs/tags/REL42.7.7.tar.gz
set -eu
cat > build.local.properties <<EOF
test.url.PGHOST=$PGHOST
test.url.PGPORT=$PGPORT
test.url.PGDBNAME=test
user=test
password=test
privilegedUser=postgres
privilegedPassword=$PGPASSWORD
EOF
export GRADLE_USER_HOME=/cache/home/gradle
rm -rf pgjdbc/build/test-results
set +e
./gradlew --no-daemon -q -PskipCheckstyle -PskipAutostyle -PskipErrorprone -PenableErrorprone=false \
  :postgresql:test --continue \
  --tests 'org.postgresql.test.jdbc2.*' --tests 'org.postgresql.test.jdbc3.*' \
  --tests 'org.postgresql.test.jdbc4.*' --tests 'org.postgresql.test.jdbc42.*' \
  --tests 'org.postgresql.core.*' "$@" > "$OUT/gradle.log" 2>&1
set -e
cp -r pgjdbc/build/test-results "$OUT/" 2>/dev/null || true
tail -40 "$OUT/gradle.log"
