# Supabase Auth 2.197.0 (GoTrue; its release binary) with the server as its database (auth_app:
# its 75 migrations as supabase_auth_admin, then its REST API): sign-ups, password logins and
# refresh tokens, the user's own record and metadata updates, admin user listings, searches,
# bans and deletes, MFA factor enrolment, the audit log, errors (suites/supabaseauth.py).
# One id per step (suites/apps.py); answers in $OUT/responses.txt, its log in $OUT/auth.log.
# APP_PORT: its HTTP port (52456).
# image: python:3.12-slim
# src:
set -eu
V=/cache/venv/apps
[ -x $V/bin/python ] || { python -m venv $V && $V/bin/pip install -q "psycopg[binary]==3.2.9"; }
D=/cache/src/supabaseauth/auth-2.197.0
if [ ! -x $D/auth ]; then
  mkdir -p $D
  python - $D <<'EOF'
import io, sys, tarfile, urllib.request
url = "https://github.com/supabase/auth/releases/download/v2.197.0/auth-v2.197.0-x86.tar.gz"
tarfile.open(fileobj=io.BytesIO(urllib.request.urlopen(url).read())).extractall(sys.argv[1])
EOF
fi
PORT=${APP_PORT:-52456}
export GOTRUE_JWT_SECRET=a133-supabase-auth-suite-secret-0123456789 GOTRUE_JWT_EXP=3600
export GOTRUE_JWT_AUD=authenticated GOTRUE_JWT_DEFAULT_GROUP_NAME=authenticated
export GOTRUE_JWT_ADMIN_ROLES=supabase_admin,service_role GOTRUE_DB_DRIVER=postgres DB_NAMESPACE=auth
export DATABASE_URL="postgres://supabase_auth_admin:pw@$PGHOST:$PGPORT/auth_app?sslmode=disable"
export API_EXTERNAL_URL=http://$PGHOST:$PORT GOTRUE_API_HOST=$PGHOST PORT=$PORT
export GOTRUE_SITE_URL=http://$PGHOST:3000 GOTRUE_MAILER_AUTOCONFIRM=true GOTRUE_DISABLE_SIGNUP=false
export GOTRUE_EXTERNAL_EMAIL_ENABLED=true GOTRUE_EXTERNAL_PHONE_ENABLED=false
export GOTRUE_MFA_TOTP_ENROLL_ENABLED=true GOTRUE_MFA_TOTP_VERIFY_ENABLED=true
export GOTRUE_RATE_LIMIT_HEADER="" GOTRUE_RATE_LIMIT_TOKEN_REFRESH=100000 GOTRUE_RATE_LIMIT_VERIFY=100000
export GOTRUE_SECURITY_REFRESH_TOKEN_ROTATION_ENABLED=true GOTRUE_LOG_LEVEL=warn
export GOTRUE_DB_MIGRATIONS_PATH=$D/migrations
cd $D
./auth > "$OUT/auth.log" 2>&1 &
P=$!
SUITE_NAME=supabaseauth $V/bin/python /harness/suites/supabaseauth.py || true
kill $P 2>/dev/null || true
wait $P 2>/dev/null || true
grep -c '^PASS' "$OUT/supabaseauth.lst" || true
grep -iE '"level":"(error|fatal)"' "$OUT/auth.log" | head -20 || true
