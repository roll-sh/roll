# Keycloak 26.8.1 (its release tarball, on Temurin's JRE 21) with the server as its database
# (keycloak_app: Liquibase's changelog at startup, then Hibernate and pgjdbc as the admin
# REST API and logins use it): realms, clients, client scopes, composite roles, nested groups,
# users with attributes and their searches, role mappings, password-grant logins, sessions,
# token introspection, login and admin events, a partial export and deletes
# (suites/keycloak.py). One id per step (suites/apps.py); answers in $OUT/responses.txt,
# Keycloak's log in $OUT/keycloak.log. APP_PORT: its HTTP port (52453).
# image: python:3.12-slim
# src:
set -eu
J=/cache/src/jre21
if [ ! -x $J/bin/java ]; then
  mkdir -p /cache/src
  python - <<'EOF'
import io, tarfile, urllib.request
url = "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.13%2B9/OpenJDK21U-jre_x64_linux_hotspot_21.0.13_9.tar.gz"
tarfile.open(fileobj=io.BytesIO(urllib.request.urlopen(url).read())).extractall("/cache/src/jre-tmp")
EOF
  mv /cache/src/jre-tmp/jdk-21*-jre $J && rm -rf /cache/src/jre-tmp
fi
K=/cache/src/keycloak/keycloak-26.8.1
if [ ! -x $K/bin/kc.sh ]; then
  mkdir -p /cache/src/keycloak
  python - <<'EOF'
import io, tarfile, urllib.request
url = "https://github.com/keycloak/keycloak/releases/download/26.8.1/keycloak-26.8.1.tar.gz"
tarfile.open(fileobj=io.BytesIO(urllib.request.urlopen(url).read())).extractall("/cache/src/keycloak")
EOF
fi
# (A copy per run: the build step writes into the distribution.)
W=/tmp/keycloak
rm -rf $W && cp -r $K $W
export JAVA_HOME=$J PATH=$J/bin:$PATH
export KC_DB=postgres KC_DB_URL="jdbc:postgresql://$PGHOST:$PGPORT/keycloak_app" KC_DB_USERNAME=postgres
export KC_DB_PASSWORD=pw KC_HTTP_PORT=${APP_PORT:-52453} KC_HTTP_HOST=127.0.0.1 KC_HEALTH_ENABLED=false
export KC_BOOTSTRAP_ADMIN_USERNAME=admin KC_BOOTSTRAP_ADMIN_PASSWORD=admin KC_CACHE=local
export JAVA_OPTS_KC_HEAP="-Xms256m -Xmx1g"
$W/bin/kc.sh start-dev > "$OUT/keycloak.log" 2>&1 &
P=$!
SUITE_NAME=keycloak python /harness/suites/keycloak.py || true
kill $P 2>/dev/null || true
wait $P 2>/dev/null || true
grep -c '^PASS' "$OUT/keycloak.lst" || true
grep -E ' ERROR ' "$OUT/keycloak.log" | head -20 || true
