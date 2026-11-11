"""Keycloak against the server: its database (Liquibase migrations, then Hibernate's reads and
writes) as the admin REST API and logins drive it: realms, clients, client scopes, roles
(composite), groups (nested), users with attributes, searches, role mappings, logins with the
password grant, sessions, token introspection, admin events, a partial export, deletes.
See keycloak.sh."""
import json
import os
import sys
import time
import urllib.parse

sys.path.insert(0, os.path.dirname(__file__))
import apps  # noqa: E402

PORT = int(os.environ.get("APP_PORT", "52453"))
BASE = "http://127.0.0.1:%d" % PORT
k = apps.Http(BASE)
DROP = lambda key: key in apps.VOLATILE or key in (  # noqa: E731
    "containerId", "createdTimestamp", "secret", "access_token", "refresh_token", "id_token",
    "session_state", "sid", "jti", "nbf", "auth_time", "lastAccess", "start", "ipAddress",
    "notBefore", "parentId", "realmId", "authDetails", "representation", "time")


def token(realm, user, password, client="admin-cli", secret=None):
    form = {"grant_type": "password", "client_id": client, "username": user, "password": password}
    if secret:
        form["client_secret"] = secret
    st, t = k.call("POST", "/realms/%s/protocol/openid-connect/token" % realm, form=form)
    return st, t


def step(name, method, path, body=None, keep=None):
    return k.step(name, method, "/admin/realms" + path, body, drop=DROP, keep=keep)


def names(field="name"):
    return lambda b: sorted(x.get(field) for x in b) if isinstance(b, list) else b


def main():
    t = time.time()
    if not apps.wait_http(BASE + "/realms/master", 1200):
        apps.record("startup", "keycloak never answered", ok=False)
        return
    apps.record("startup", "ok")
    print("keycloak up after %.1fs" % (time.time() - t), flush=True)
    st, tk = token("master", "admin", "admin")
    k.headers["Authorization"] = "Bearer %s" % (tk.get("access_token") if isinstance(tk, dict) else "")
    # (The admin token lives 60 s by default; the steps take longer on a loaded machine.)
    k.call("PUT", "/admin/realms/master", {"accessTokenLifespan": 3600})
    st, tk = token("master", "admin", "admin")
    k.headers["Authorization"] = "Bearer %s" % (tk.get("access_token") if isinstance(tk, dict) else "")
    step("realm:create", "POST", "", {"realm": "shop", "enabled": True, "displayName": "Shop",
                                      "eventsEnabled": True, "adminEventsEnabled": True,
                                      "adminEventsDetailsEnabled": True, "enabledEventTypes": ["LOGIN", "LOGIN_ERROR"],
                                      "passwordPolicy": "length(8)", "attributes": {"note": "a122"}})
    step("realm:get", "GET", "/shop", keep=lambda b: {x: b.get(x) for x in (
        "realm", "displayName", "enabled", "passwordPolicy", "eventsEnabled", "attributes")} if isinstance(b, dict) else b)
    step("client:create", "POST", "/shop/clients", {
        "clientId": "web", "enabled": True, "publicClient": False, "secret": "web-secret",
        "directAccessGrantsEnabled": True, "serviceAccountsEnabled": True, "standardFlowEnabled": True,
        "redirectUris": ["http://localhost/*"], "attributes": {"pkce.code.challenge.method": "S256"}})
    st, cl = k.call("GET", "/admin/realms/shop/clients?clientId=web")
    cid = cl[0]["id"] if isinstance(cl, list) and cl else ""
    step("client:get", "GET", "/shop/clients/%s" % cid, keep=lambda b: {x: b.get(x) for x in (
        "clientId", "publicClient", "directAccessGrantsEnabled", "redirectUris", "protocol")} if isinstance(b, dict) else b)
    step("client:role", "POST", "/shop/clients/%s/roles" % cid, {"name": "reader", "description": "reads"})
    step("scope:create", "POST", "/shop/client-scopes", {"name": "orders", "protocol": "openid-connect",
                                                         "attributes": {"include.in.token.scope": "true"}})
    step("scopes", "GET", "/shop/client-scopes", keep=names())
    for r in ("buyer", "seller", "admin"):
        step("role:" + r, "POST", "/shop/roles", {"name": r, "description": "the %s role" % r})
    st, roles = k.call("GET", "/admin/realms/shop/roles")
    rid = {x["name"]: x for x in roles} if isinstance(roles, list) else {}
    step("role:composite", "POST", "/shop/roles/admin/composites", [rid.get("buyer", {}), rid.get("seller", {})])
    step("roles", "GET", "/shop/roles?briefRepresentation=true", keep=names())
    step("role:composites", "GET", "/shop/roles/admin/composites", keep=names())
    step("group:create", "POST", "/shop/groups", {"name": "staff", "attributes": {"floor": ["2"]}})
    st, gs = k.call("GET", "/admin/realms/shop/groups?search=staff")
    gid = gs[0]["id"] if isinstance(gs, list) and gs else ""
    step("group:child", "POST", "/shop/groups/%s/children" % gid, {"name": "night"})
    step("groups", "GET", "/shop/groups?briefRepresentation=false",
         keep=lambda b: [(g.get("name"), g.get("subGroupCount"), g.get("attributes")) for g in b] if isinstance(b, list) else b)
    step("group:roles", "POST", "/shop/groups/%s/role-mappings/realm" % gid, [rid.get("seller", {})])
    for i in range(12):
        u = {"username": "user%02d" % i, "enabled": True, "email": "user%02d@example.com" % i,
             "firstName": "First%d" % (i % 3), "lastName": "Last%d" % i, "emailVerified": i % 2 == 0,
             "attributes": {"tier": ["gold" if i % 4 == 0 else "basic"], "n": [str(i)]},
             "credentials": [{"type": "password", "value": "password-%02d" % i, "temporary": False}]}
        step("user:%02d" % i, "POST", "/shop/users", u)
    step("users:count", "GET", "/shop/users/count")
    step("users:search", "GET", "/shop/users?search=user0&max=50", keep=names("username"))
    step("users:first", "GET", "/shop/users?firstName=First1&briefRepresentation=true", keep=names("username"))
    step("users:email", "GET", "/shop/users?email=user1&exact=false", keep=names("username"))
    step("users:attr", "GET", "/shop/users?q=tier:gold", keep=names("username"))
    step("users:page", "GET", "/shop/users?first=3&max=4", keep=names("username"))
    st, us = k.call("GET", "/admin/realms/shop/users?username=user01&exact=true")
    uid = us[0]["id"] if isinstance(us, list) and us else ""
    step("user:roles", "POST", "/shop/users/%s/role-mappings/realm" % uid, [rid.get("admin", {})])
    step("user:effective", "GET", "/shop/users/%s/role-mappings/realm/composite" % uid, keep=names())
    step("user:group", "PUT", "/shop/users/%s/groups/%s" % (uid, gid))
    step("group:members", "GET", "/shop/groups/%s/members" % gid, keep=names("username"))
    step("role:users", "GET", "/shop/roles/admin/users", keep=names("username"))
    step("user:update", "PUT", "/shop/users/%s" % uid, {"firstName": "Renamed", "attributes": {"tier": ["silver"]}})
    step("user:get", "GET", "/shop/users/%s" % uid, keep=lambda b: {x: b.get(x) for x in (
        "username", "firstName", "lastName", "email", "attributes", "emailVerified")} if isinstance(b, dict) else b)
    # Logins (sessions, events), a failed one, refresh, introspection.
    st, t1 = token("shop", "user01", "password-01", "web", "web-secret")
    apps.record("login:ok", {"status": st, "keys": sorted(t1) if isinstance(t1, dict) else t1})
    st, bad = token("shop", "user01", "wrong-password", "web", "web-secret")
    apps.record("login:bad", {"status": st, "body": apps.norm(bad, DROP)})
    for i in range(2, 5):
        token("shop", "user%02d" % i, "password-%02d" % i, "web", "web-secret")
    if isinstance(t1, dict) and t1.get("refresh_token"):
        st, r = k.call("POST", "/realms/shop/protocol/openid-connect/token",
                       form={"grant_type": "refresh_token", "client_id": "web", "client_secret": "web-secret",
                             "refresh_token": t1["refresh_token"]})
        apps.record("login:refresh", {"status": st})
        st, ins = k.call("POST", "/realms/shop/protocol/openid-connect/token/introspect",
                         form={"client_id": "web", "client_secret": "web-secret", "token": t1["access_token"]})
        apps.record("introspect", {"status": st, "body": apps.norm(
            {x: ins.get(x) for x in ("active", "username", "client_id", "realm_access", "scope")}, DROP)
            if isinstance(ins, dict) else ins})
    step("sessions:client", "GET", "/shop/clients/%s/user-sessions" % cid, keep=names("username"))
    step("sessions:stats", "GET", "/shop/client-session-stats",
         keep=lambda b: [(x.get("clientId"), x.get("active")) for x in b] if isinstance(b, list) else b)
    step("events", "GET", "/shop/events?max=100",
         keep=lambda b: sorted((e.get("type"), e.get("error")) for e in b) if isinstance(b, list) else b)
    step("admin-events", "GET", "/shop/admin-events?max=200",
         keep=lambda b: sorted((e.get("operationType"), e.get("resourceType")) for e in b) if isinstance(b, list) else b)
    step("export", "POST", "/shop/partial-export?exportClients=true&exportGroupsAndRoles=true",
         keep=lambda b: {"roles": sorted(r.get("name") for r in (b.get("roles") or {}).get("realm", [])),
                         "groups": sorted(g.get("name") for g in b.get("groups") or []),
                         "clients": sorted(c.get("clientId") for c in b.get("clients") or [])}
         if isinstance(b, dict) else b)
    step("user:logout", "POST", "/shop/users/%s/logout" % uid)
    step("user:delete", "DELETE", "/shop/users/%s" % uid)
    step("users:after", "GET", "/shop/users/count")
    step("group:delete", "DELETE", "/shop/groups/%s" % gid)
    step("realm:delete", "DELETE", "/shop")
    step("realms", "GET", "", keep=names("realm"))


if __name__ == "__main__":
    main()
