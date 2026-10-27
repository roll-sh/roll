# r2dbc-postgresql 1.1.3.RELEASE (Java, reactive driver): its own integration tests
# (failsafe: *IntegrationTests, PostgresqlTestKit), against database "test" owned by superuser
# "test" (what its Testcontainers PostgreSQLContainer makes; r2dbc.setup.sql), through its
# ".testrc preference=external" switch. The External server class hard-codes localhost:5432 and
# postgres/postgres, so this script points it at $PGHOST:$PGPORT and test/test. Not run: the
# PgBouncer and HA-cluster tests (they start their own containers). Tests that need SSL, a
# logical-replication setup, PostGIS or pgvector fail on both servers (the environment's).
# Extra args go to Maven (e.g. -Dit.test=PostgresqlStatementIntegrationTests).
# image: eclipse-temurin:21-jdk
# src: https://github.com/pgjdbc/r2dbc-postgresql/archive/refs/tags/v1.1.3.RELEASE.tar.gz
set -eu
F=src/test/java/io/r2dbc/postgresql/util/PostgresqlServerExtension.java
sed -i -e 's/return "localhost";/return System.getenv("PGHOST");/' \
  -e 's/return 5432;/return Integer.parseInt(System.getenv("PGPORT"));/' \
  -e '/class External /,/^    }$/ s/return "postgres";/return "test";/' "$F"
grep -q 'getenv("PGPORT")' "$F"
echo preference=external > .testrc
export MAVEN_USER_HOME=/cache/home/m2wrapper
rm -rf target/failsafe-reports
set +e
./mvnw -B -Dmaven.repo.local=/cache/home/m2 -Dmaven.javadoc.skip=true -Dgpg.skip=true test-compile \
  failsafe:integration-test -Dfailsafe.failIfNoSpecifiedTests=false \
  '-Dit.test=**/*IntegrationTests,**/*TestKit,!**/PgBouncerIntegrationTests,!**/HighAvailabilityClusterIntegrationTests' \
  "$@" > "$OUT/maven.log" 2>&1
set -e
cp -r target/failsafe-reports "$OUT/" 2>/dev/null || true
grep -E 'Tests run:|BUILD|ERROR\] Failed' "$OUT/maven.log" | tail -5
