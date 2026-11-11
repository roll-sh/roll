# Hibernate ORM 7.4.14 (Java): hibernate-core's tests with -Pdb=pgsql_ci (PostgreSQLDialect, the
# pgjdbc of its version catalog), 4 test forks on hibernate_orm_test_1..4 (its setup.sql); dbHost
# carries the port. Extra args go to gradle (e.g. --tests 'org.hibernate.orm.test.query.*').
# Java assertions stay on for Hibernate but are off for pgjdbc (an init script): with -ea,
# pgjdbc's castNonNull asserts on a NULL from its getSQLKeywords() query, which the dialect
# runs at bootstrap, so one catalog gap (pg_get_keywords() empty on the engine) would fail every
# test instead of showing as one difference. hibernate-testing's own tests are not run.
# image: eclipse-temurin:25-jdk
# src: https://github.com/hibernate/hibernate-orm/archive/refs/tags/7.4.14.tar.gz
set -eu
export GRADLE_USER_HOME=/cache/home/gradle
rm -rf hibernate-core/target/test-results
mkdir -p /cache/work/hibernate
cat > /cache/work/hibernate/init.gradle <<'EOF'
allprojects {
  tasks.withType(Test).configureEach {
    enableAssertions = false
    jvmArgs '-ea', '-da:org.postgresql...'
  }
}
EOF
set +e
./gradlew --no-daemon --console=plain -I /cache/work/hibernate/init.gradle -Pdb=pgsql_ci \
  -Ptest.threads=4 -DdbHost="$PGHOST:$PGPORT" -x :hibernate-testing:test \
  :hibernate-core:test --continue "$@" > "$OUT/gradle.log" 2>&1
set -e
cp -r hibernate-core/target/test-results "$OUT/" 2>/dev/null || true
grep -E 'tests completed|BUILD|FAILURE:' "$OUT/gradle.log" | head; tail -20 "$OUT/gradle.log"
