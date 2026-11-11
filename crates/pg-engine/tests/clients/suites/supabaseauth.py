"""Supabase Auth against the server: its migrations, then sign-ups, logins, tokens, the user's
record, admin listings and changes, MFA enrolment, errors and the tables they leave.
See supabaseauth.sh."""
import base64
import hashlib
import hmac
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(__file__))
import apps  # noqa: E402
import psycopg  # noqa: E402

PORT = int(os.environ.get("APP_PORT", "52456"))
PGHOST, PGPORT = os.environ["PGHOST"], os.environ["PGPORT"]
BASE = "http://%s:%d" % (PGHOST, PORT)
SECRET = os.environ["GOTRUE_JWT_SECRET"].encode()


def b64(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def jwt(claims):
    head = b64(json.dumps({"alg": "HS256", "typ": "JWT"}).encode())
    body = b64(json.dumps(claims).encode())
    sig = hmac.new(SECRET, ("%s.%s" % (head, body)).encode(), hashlib.sha256).digest()
    return "%s.%s.%s" % (head, body, b64(sig))


SERVICE = jwt({"role": "service_role", "iss": "supabase", "exp": int(time.time()) + 7200})
anon = apps.Http(BASE)
admin = apps.Http(BASE, headers={"Authorization": "Bearer " + SERVICE})


def drop(k):
    return (k in apps.VOLATILE or k.endswith("_at") or k.endswith("_id") or "token" in k
            or k in ("expires_in", "totp", "secret", "qr_code", "uri", "sub", "session_id", "weak_password"))


def step(http, name, method, path, body=None, keep=None, **kw):
    return http.step(name, method, path, body, drop=drop, keep=keep, **kw)


def user(token):
    return apps.Http(BASE, headers={"Authorization": "Bearer " + token})


def main():
    t = time.time()
    if not apps.wait_http(BASE + "/health", 900):
        apps.record("startup", "auth never answered", ok=False)
        return
    apps.record("startup", "ok")
    print("auth up after %.1fs" % (time.time() - t), flush=True)
    app = psycopg.connect(host=PGHOST, port=PGPORT, user="postgres", password="pw", dbname="auth_app")
    app.autocommit = True
    apps.db_rows(app, "db:migrations", "SELECT count(*) FROM auth.schema_migrations")
    apps.schema_digest(app, ("auth",))
    step(anon, "settings", "GET", "/settings")

    tokens = {}
    for i, name in enumerate(("ann", "ben", "cy")):
        st, out = step(anon, "signup:" + name, "POST", "/signup", {
            "email": "%s@example.com" % name, "password": "Pw-%s-123456" % name,
            "data": {"display": name.title(), "n": i}})
        if isinstance(out, dict):
            tokens[name] = out.get("access_token")
    step(anon, "signup:dup", "POST", "/signup", {"email": "ann@example.com", "password": "Pw-ann-123456"})
    step(anon, "signup:weak", "POST", "/signup", {"email": "weak@example.com", "password": "1"})
    step(anon, "signup:bad-email", "POST", "/signup", {"email": "not-an-email", "password": "Pw-x-123456"})
    st, login = step(anon, "token:password", "POST", "/token?grant_type=password",
                     {"email": "ann@example.com", "password": "Pw-ann-123456"})
    step(anon, "token:wrong-password", "POST", "/token?grant_type=password",
         {"email": "ann@example.com", "password": "nope-nope"})
    step(anon, "token:no-user", "POST", "/token?grant_type=password",
         {"email": "nobody@example.com", "password": "Pw-x-123456"})
    refresh = login.get("refresh_token") if isinstance(login, dict) else None
    access = login.get("access_token") if isinstance(login, dict) else None
    st, ref = step(anon, "token:refresh", "POST", "/token?grant_type=refresh_token", {"refresh_token": refresh})
    step(anon, "token:refresh-reused", "POST", "/token?grant_type=refresh_token", {"refresh_token": refresh})
    if isinstance(ref, dict) and ref.get("access_token"):
        access = ref["access_token"]
    me = user(access or "")
    step(me, "user:get", "GET", "/user")
    step(me, "user:update", "PUT", "/user", {"data": {"display": "Ann B", "tags": ["a", "b"]}})
    step(me, "user:after", "GET", "/user")
    step(me, "mfa:enroll", "POST", "/factors", {"factor_type": "totp", "friendly_name": "phone"})
    step(me, "mfa:list", "GET", "/user", keep=lambda b: b.get("factors") if isinstance(b, dict) else b)

    created = {}
    for name in ("dee", "eve"):
        st, out = step(admin, "admin:create:" + name, "POST", "/admin/users", {
            "email": "%s@example.com" % name, "password": "Pw-%s-123456" % name, "email_confirm": True,
            "user_metadata": {"display": name}, "app_metadata": {"plan": "pro" if name == "dee" else "free"}})
        if isinstance(out, dict):
            created[name] = out.get("id")
    step(admin, "admin:create-dup", "POST", "/admin/users", {"email": "dee@example.com", "password": "Pw-x-123456"})
    step(admin, "admin:list", "GET", "/admin/users?page=1&per_page=50",
         keep=lambda b: sorted((u.get("email"), u.get("role"), json.dumps({k: v for k, v in (u.get("user_metadata") or {}).items() if k != "sub"}, sort_keys=True))
                               for u in b.get("users", [])) if isinstance(b, dict) else b)
    step(admin, "admin:list-page", "GET", "/admin/users?page=2&per_page=2",
         keep=lambda b: len(b.get("users", [])) if isinstance(b, dict) else b)
    step(admin, "admin:get", "GET", "/admin/users/%s" % created.get("dee"))
    step(admin, "admin:get-missing", "GET", "/admin/users/00000000-0000-0000-0000-000000000000")
    step(admin, "admin:update", "PUT", "/admin/users/%s" % created.get("dee"),
         {"user_metadata": {"display": "Dee 2"}, "app_metadata": {"plan": "team"}})
    step(admin, "admin:ban", "PUT", "/admin/users/%s" % created.get("eve"), {"ban_duration": "24h"})
    step(anon, "token:banned", "POST", "/token?grant_type=password",
         {"email": "eve@example.com", "password": "Pw-eve-123456"})
    step(admin, "admin:delete", "DELETE", "/admin/users/%s" % created.get("eve"))
    step(admin, "admin:after-delete", "GET", "/admin/users/%s" % created.get("eve"))
    step(admin, "admin:anon-denied", "GET", "/admin/users", headers={"Authorization": "Bearer " + (tokens.get("ben") or "")})
    step(me, "logout", "POST", "/logout")
    step(me, "user:after-logout", "GET", "/user")

    apps.db_rows(app, "db:counts", """SELECT (SELECT count(*) FROM auth.users), (SELECT count(*) FROM auth.identities),
        (SELECT count(*) FROM auth.sessions), (SELECT count(*) FROM auth.refresh_tokens),
        (SELECT count(*) FROM auth.refresh_tokens WHERE revoked), (SELECT count(*) FROM auth.mfa_factors)""")
    apps.db_rows(app, "db:users", """SELECT email, role, aud, raw_user_meta_data - 'sub', raw_app_meta_data,
        email_confirmed_at IS NOT NULL, banned_until IS NOT NULL, is_anonymous FROM auth.users ORDER BY email""")
    apps.db_rows(app, "db:identities", "SELECT provider, identity_data->>'email', email FROM auth.identities ORDER BY 2")
    apps.db_rows(app, "db:audit", """SELECT payload->>'action', count(*) FROM auth.audit_log_entries
        GROUP BY 1 ORDER BY 1""")


if __name__ == "__main__":
    main()
