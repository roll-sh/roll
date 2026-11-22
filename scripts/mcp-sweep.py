#!/usr/bin/env python3
"""Every MCP tool a fresh rolld lists, called through /v1/mcp as an agent would (scripts/test-mcp-sweep.sh
starts the rolld). It first makes fixtures with the write tools (a workspace project, a bucket, a
stream, NoSQL and Redis databases, a search cluster and index, a Postgres project, a queue, an apps
service, a roll code repo, a tasks message, a token), then calls each read-only tool with ids from
them (a stand-in that doesn't exist for what none made), then, with --writes, each write tool with values made from its schema (non-destructive first).

A failure is a mapping bug: a 5xx, an argument the route rejects (unknown, missing or of another type),
or an empty 404 (no route answers the path). "Not found" for a missing resource, a refusal about a
made-up value and a product that's off are fine.

Every answer must also fit the output schema its tool publishes (crates/roll-mcp/src/output-schemas.json);
with --record FILE, the answers are kept there for scripts/mcp_output_schemas.py to widen them.

    mcp-sweep.py <api> <operator token file> [--writes] [-v] [--tool NAME] [--record FILE]
"""
import importlib.util, json, os, re, sys, urllib.error, urllib.request

# Loaded by path: `python3 -I` leaves this directory off the import path.
_spec = importlib.util.spec_from_file_location("mcp_output_schemas", os.path.join(os.path.dirname(os.path.abspath(__file__)), "mcp_output_schemas.py"))
schemas_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(schemas_mod)
try:
    published = json.load(open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", schemas_mod.SCHEMAS)))
except FileNotFoundError:
    published = {}
record = sys.argv[sys.argv.index("--record") + 1] if "--record" in sys.argv else None
samples, unfit = {}, []

api, tok = sys.argv[1], open(sys.argv[2]).read().strip()
only = sys.argv[sys.argv.index("--tool") + 1] if "--tool" in sys.argv else None
n = 0


def rpc(method, params, token=None):
    global n
    n += 1
    req = urllib.request.Request(f"{api}/v1/mcp", data=json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}).encode(),
                                 headers={"authorization": f"Bearer {token or tok}", "content-type": "application/json", "accept": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        out = json.load(r)
    # What a tool answers, against the output schema it publishes (and kept, with --record).
    res = out.get("result") if isinstance(out, dict) else None
    if method == "tools/call" and isinstance(res, dict) and not res.get("isError") and isinstance(res.get("structuredContent"), dict):
        name, sc = params.get("name", ""), res["structuredContent"]
        if name in published:
            bad = schemas_mod.check(sc, published[name])
            if bad and len(unfit) < 50:
                unfit.append(f"{name}: {bad} (its outputSchema; rerun with --record if the new shape is right)")
        if record and len(samples.setdefault(name, [])) < 8:
            words = sorted({v for v in params.get("arguments", {}).values() if isinstance(v, str)})
            samples[name].append({"answer": sc, "words": words})
    return out


listed = rpc("tools/list", {})["result"]
tools = listed["tools"]
# What a client takes in before it does anything (docs/mcp.md, "Toolsets"): the tools and their size.
print(f"tools/list: {len(tools)} tools, {len(json.dumps(listed)) // 1024} KB")
if "-v" in sys.argv:
    with urllib.request.urlopen(urllib.request.Request(f"{api}/v1/mcp/toolsets", headers={"authorization": f"Bearer {tok}"}), timeout=30) as r:
        costs = json.load(r)
        print("toolsets:", json.dumps(costs["products"])[:1500])
        print("areas:", ", ".join(f"{a['toolset']} {a['tools']} tools ~{a['context_tokens']} tokens" for a in costs.get("areas", [])))
reads = [t for t in tools if t.get("annotations", {}).get("readOnlyHint") and (only is None or t["name"] == only)]
FAIL_WORDS = ("unknown argument", "is required", "can't be empty", "no route", "method not allowed", "invalid type", "missing field", "unknown field",
              "Invalid URL: Cannot parse")  # a path segment of a type the route doesn't take


def items_of(sc):
    if isinstance(sc, dict):
        for k in ("items", "projects", "databases", "buckets", "clusters", "services", "streams", "remotes", "previews"):
            if isinstance(sc.get(k), list):
                return sc[k]
    return sc if isinstance(sc, list) else []


def judge(name, res):
    if "error" in res:
        return f"RPC {json.dumps(res['error'])[:200]}"
    r = res["result"]
    if not r.get("isError"):
        return None
    sc = r.get("structuredContent") or {}
    status = sc.get("http_status") if isinstance(sc, dict) else None
    text = json.dumps(sc) + " ".join(c.get("text", "") for c in r.get("content", []))
    if status and status >= 500:
        return f"{status}: {text[:200]}"
    if any(w in text for w in FAIL_WORDS):
        return f"{status}: {text[:200]}"
    # A path no route answers is an empty 404; a missing resource names itself.
    if status == 404 and not (isinstance(sc, dict) and any(isinstance(v, str) and v.strip() for k, v in sc.items() if k != "http_status")):
        return f"404 with no message (no such route?): {text[:200]}"
    return None  # 404, 403 (a product's own rule), 409, 400 about the resource: fine for a fresh rolld


# Fixtures, made with the write tools a person's agent would use, so tools that need an id have one.
# Every credential the fixtures are answered with (a token's secret, a database's or cluster's
# password, a connection string's): no read tool may show one to a token that can't reveal.
SECRET_KEYS = {"password", "secret", "token", "secret_access_key", "api_key", "connection_string", "connection_uri", "dsn"}
secrets = {tok}
origin = {}  # a credential's value -> where it was seen (a key path, never printed with the value)
def collect(v, at="", where=""):
    if isinstance(v, dict):
        for k, x in v.items():
            if isinstance(x, str) and k in SECRET_KEYS and len(x) >= 10:
                secrets.add(x)
                origin.setdefault(x, f"{where} {at}.{k}")
            collect(x, f"{at}.{k}", where)
    elif isinstance(v, list):
        for x in v:
            collect(x, f"{at}[]", where)
    elif isinstance(v, str) and "://" in v:
        userinfo = v.split("://", 1)[1].split("/", 1)[0].rsplit("@", 1)
        if len(userinfo) == 2 and ":" in userinfo[0] and len(userinfo[0].split(":", 1)[1]) >= 10:
            pw = userinfo[0].split(":", 1)[1]
            secrets.add(pw)
            origin.setdefault(pw, f"{where} {at} (a URL's password)")
def call(name, args):
    r = rpc("tools/call", {"name": name, "arguments": args}).get("result", {})
    if not r.get("isError"):
        collect(r.get("structuredContent"), "", name)
    return None if r.get("isError") else r.get("structuredContent")
made = {}
for name, args in [("workspace_create_project", {"name": "sweep"}), ("storage_create_bucket", {"name": "sweep-bkt"}), ("streams_create", {"name": "sweep"}),
                   ("nosql_create_database", {"name": "sweep"}), ("redis_create_database", {"name": "sweep"}), ("search_create_cluster", {"name": "sweep"}),
                   ("postgres_create_project", {"name": "sweep"}), ("queues_create_vhost", {"name": "sweep"})]:
    made[name] = call(name, args)
    print(("made " if made[name] is not None else "could not make ") + name)
def by_name(list_tool, args, name):
    for it in items_of(call(list_tool, args)):
        if isinstance(it, dict) and it.get("name") == name:
            return it
ws = (made.get("workspace_create_project") or by_name("workspace_list_projects", {}, "sweep") or {}).get("id")
pgp = made.get("postgres_create_project") or by_name("postgres_list_projects", {}, "sweep") or {}
pg = pgp.get("id") or (pgp.get("project") or {}).get("id")
full = call("postgres_get_project", {"project": pg}) if pg else None
# A branch's primary endpoint (`branches[].endpoint`; no compute runs without Docker, but its reads answer).
ep = next(((b.get("endpoint") or {}).get("id") for b in ((full or {}).get("branches") or []) if isinstance(b, dict) and isinstance(b.get("endpoint"), dict)), None)
br = next((b.get("id") for b in ((full or {}).get("branches") or []) if isinstance(b, dict)), None)
# A token to look at (its secret is never printed).
tok_req = urllib.request.Request(f"{api}/v1/access/tokens", data=json.dumps({"name": "sweep", "rules": [{"product": "storage", "resources": "*", "actions": {"read": "allow"}}]}).encode(),
                                 headers={"authorization": f"Bearer {tok}", "content-type": "application/json"})
try:
    with urllib.request.urlopen(tok_req, timeout=30) as r:
        grant = json.load(r).get("grant", {}).get("id")
except Exception:
    grant = None
call("search_create_index", {"cluster": "sweep", "index": "sweep-idx"})
call("streams_create_topic", {"stream": "sweep", "name": "t1"})
call("streams_reset_group", {"stream": "sweep", "group": "g1", "topic": "t1", "to": "earliest"})  # committed offsets make a group
call("queues_declare_queue", {"vhost": "sweep", "name": "q1"})
seen = {("storage", "bucket"): "sweep-bkt", ("streams", "stream"): "sweep", ("streams", "topic"): "t1", ("streams", "group"): "g1", ("nosql", "database"): "sweep",
        ("redis", "database"): "sweep", ("search", "cluster"): "sweep", ("search", "index"): "sweep-idx", ("queues", "vhost"): "sweep",
        ("queues", "queue"): "q1", ("secrets", "env"): "development", ("apps", "env"): "production"}
if ws:
    for k in [("workspace", "id"), ("secrets", "project"), ("apps", "project"), ("previews", "project"), ("access", "project")]:
        seen[k] = ws
if pg:
    seen[("postgres", "project")] = pg
if ep:
    seen[("postgres", "endpoint")] = ep
if br:
    seen[("postgres", "branch")] = br
if grant:
    seen[("access", "id")] = grant
def rest(method, path, body):
    req = urllib.request.Request(f"{api}{path}", method=method, data=json.dumps(body).encode(), headers={"authorization": f"Bearer {tok}", "content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            answer = json.load(r)
            collect(answer, "", f"{method} {path}")
            return answer
    except urllib.error.HTTPError as e:
        # A fixture that isn't made leaves its tools on stand-ins: say why (an id or a message, never a secret).
        print(f"fixture {method} {path}: {e.code} {e.read()[:200].decode(errors='replace')}")
        return None
    except Exception as e:
        print(f"fixture {method} {path}: {e}")
        return None
# roll code: an owner and a repo (code's and agents' tools name them).
rest("POST", "/v1/code/owners", {"name": "sweep"})
repo_made = rest("POST", "/v1/code/repos", {"owner": "sweep", "name": "app"}) is not None or call("code_list_repos", {}) is not None
# A first commit on the repo's main, pushed over git as a person would (the token in git's
# environment, never its command line).
if repo_made:
    import os, subprocess, tempfile
    with tempfile.TemporaryDirectory() as d:
        env = dict(os.environ, GIT_CONFIG_COUNT="1", GIT_CONFIG_KEY_0="http.extraHeader", GIT_CONFIG_VALUE_0=f"Authorization: Bearer {tok}",
                   GIT_AUTHOR_NAME="sweep", GIT_AUTHOR_EMAIL="sweep@example.invalid", GIT_COMMITTER_NAME="sweep", GIT_COMMITTER_EMAIL="sweep@example.invalid", GIT_TERMINAL_PROMPT="0")
        open(os.path.join(d, "README.md"), "w").write("sweep\n")
        for cmd in (["git", "init", "-q", "-b", "main"], ["git", "add", "README.md"], ["git", "commit", "-q", "-m", "sweep"], ["git", "push", "-q", f"{api}/git/sweep/app.git", "main"]):
            r = subprocess.run(cmd, cwd=d, env=env, capture_output=True, text=True, timeout=60)
            if r.returncode != 0:
                print(f"fixture {' '.join(cmd[:2])}: {r.stderr.strip()[:200]}")
                break
# previews: the repo on the workspace project's canvas, then a preview of it (its deploy needs
# Docker and fails, but the preview and its reads answer).
pv_id = None
if repo_made and ws:
    rest("PUT", f"/v1/workspace/projects/{ws}/resources/repo/sweep%2Fapp", {"x": 0, "y": 0})
    rest("PUT", f"/v1/previews/projects/{ws}/settings", {"enabled": True})
    pv = rest("POST", "/v1/previews", {"project": ws, "repo": "sweep/app", "branch": "main"}) or {}
    pv_id = pv.get("id") or (pv.get("preview") or {}).get("id")
# tasks: a message (its delivery to an address that never resolves fails, which is fine).
msg = call("tasks_publish", {"url": "https://example.invalid/sweep", "body": "{}"}) or {}
msg_id = msg.get("id") or (msg.get("ids") or [None])[0] or msg.get("message_id")
svc = None
if ws:
    sv = call("apps_create_service", {"project": ws, "name": "sweep", "source": {"kind": "upload"}}) or by_name("apps_list_services", {}, "sweep") or {}
    svc = sv.get("id") or (sv.get("service") or {}).get("id")
if svc:
    seen[("apps", "service")] = svc
print(f"fixtures: service {bool(svc)}, workspace {bool(ws)}, postgres {bool(pg)}, endpoint {bool(ep)}, branch {bool(br)}, token {bool(grant)}")
if repo_made:
    for k in [("code", "repo"), ("workspace", "repo")]:
        seen[k] = "sweep/app"
    seen[("agents", "owner")] = "sweep"
    seen[("code", "owner")] = "sweep"
if msg_id:
    seen[("tasks", "id")] = msg_id
if pv_id:
    seen[("previews", "id")] = pv_id
print(f"fixtures: repo {repo_made}, tasks message {bool(msg_id)}, preview {bool(pv_id)}")

# Values for arguments no fixture names: the repo's one file and its branch, a query, a statement,
# a route to check; for an id nothing made, one that doesn't exist. A route that's there answers
# "not found" with a message; a path no route answers is an empty 404, which fails. Tools called
# with one are counted apart, so they read as mapping checks, not as answers.
VALUES = {"path": "README.md", "query": "sweep", "base": "main", "head": "main", "sql": "select 1", "text": "hello sweep",
          "method": "GET", "key": "sweep.txt", "host": "sweep.example.invalid", "repo": "sweep/app", "name": "SWEEP", "reason": "sweep", "trace": "0" * 32}
BY_PRODUCT = {("access", "path"): "/v1/storage/buckets", ("access", "product"): "storage", ("access", "action"): "read",
              ("secrets", "from"): "development", ("secrets", "key"): "SWEEP", ("secrets", "to"): "production", ("apps", "from"): "development"}
def stand_in(t, prod, a):
    s = t["inputSchema"].get("properties", {}).get(a, {})
    ty = s.get("type")
    if s.get("enum"):
        return s["enum"][0]
    if ty in ("integer", "number"):
        return 1
    if ty == "boolean":
        return False
    if ty == "array":
        return ["sweep"] if (s.get("items") or {}).get("type") == "string" else []
    if ty == "object":
        return {}
    return BY_PRODUCT.get((prod, a)) or VALUES.get(a) or f"sweep-none-{a}"

fails, ran, stood_in = [], 0, []
# Two passes: tools without required arguments first (their answers give ids), then the others.
for pass_ in (0, 1):
    for t in reads:
        req = t["inputSchema"].get("required", [])
        if (pass_ == 0) != (not req):
            continue
        args = {}
        prod = t.get("_meta", {}).get("roll/product", "")
        if req:
            args = {a: seen[(prod, a)] if (prod, a) in seen else stand_in(t, prod, a) for a in req}
            missing = [a for a in req if (prod, a) not in seen]
            if missing:
                stood_in.append(f"{t['name']} ({', '.join(f'{a}={json.dumps(args[a])}' for a in missing)})")
        res = rpc("tools/call", {"name": t["name"], "arguments": args})
        ran += 1
        if req and stood_in and stood_in[-1].startswith(t["name"] + " "):
            sc = res.get("result", {}).get("structuredContent")
            stood_in[-1] += f" -> {sc.get('http_status', 200) if isinstance(sc, dict) and res['result'].get('isError') else 200}: {json.dumps(sc)[:120] if res.get('result', {}).get('isError') else 'ok'}"
        bad = judge(t["name"], res)
        # A Postgres tool whose SQL is its own (no sql argument) failing with a class-42 error
        # (syntax, an unknown column, function or table) is the tool's SQL that's wrong: a 400
        # would otherwise pass as a fresh rolld's.
        if not bad and t.get("_meta", {}).get("roll/product") == "postgres" and "sql" not in t["inputSchema"].get("properties", {}) and res.get("result", {}).get("isError"):
            text = json.dumps(res["result"].get("structuredContent")) + " ".join(c.get("text", "") for c in res["result"].get("content", []))
            # (pg_stat_statements is made once by the caller, as postgres_slow_queries says.)
            if re.search(r"\b42[0-9A-Z]{3}: ", text) and "pg_stat_statements" not in text:
                bad = f"its own SQL fails: {text[:200]}"
        if bad:
            fails.append(f"{t['name']} {json.dumps(args)}: {bad}")
        else:
            for it in items_of(res.get("result", {}).get("structuredContent")):
                if isinstance(it, dict) and isinstance(it.get("id"), str):
                    prod = t.get("_meta", {}).get("roll/product", "")
                    key = {"postgres": "project", "redis": "database", "nosql": "database", "search": "cluster", "streams": "stream", "apps": "service", "previews": "id", "workspace": "id", "secrets": "project"}.get(prod)
                    if key and (prod, key) not in seen and t["name"].split("_")[1] == "list":
                        seen[(prod, key)] = it["id"]
print(f"{len(reads)} read-only tools, {ran} called ({len(stood_in)} with a stand-in for what no fixture made), {len(fails)} failed")

# Leaks: every read tool that isn't a reveal, called by a token that may read everything and
# reveal nothing. Any credential the fixtures made, or anything shaped like a roll token or a
# private key, in its answer is a read showing what only reveal may (a missing `.reveals` field in
# the catalogue, or a product answering more than its row says). The values are never printed.
import re
reader = (rest("POST", "/v1/access/tokens", {"name": "sweep-reader", "rules": [{"product": "*", "resources": "*", "actions": {"read": "allow"}}]}) or {}).get("token")
SHAPES = [("a roll token", re.compile(r"\brl[a-z]_[a-z2-7]{40}\b")), ("a private key", re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----"))]
leaks, checked, shown = [], 0, 0
if reader:
    secrets.discard(reader)
    for t in reads:
        prod = t.get("_meta", {}).get("roll/product", "")
        req = t["inputSchema"].get("required", [])
        if any((prod, a) not in seen for a in req):
            continue
        args = {a: seen[(prod, a)] for a in req}
        reveal = "reveal" in (t.get("_meta", {}).get("roll/actions") or [])
        # The operator first: what this tool shows to someone who may see everything.
        as_operator = json.dumps(rpc("tools/call", {"name": t["name"], "arguments": args}))
        known = [x for x in secrets if x != tok and x in as_operator]
        shown += bool(known)
        if reveal:
            continue  # a reveal tool shows credentials by design; the reader is refused it
        text = json.dumps(rpc("tools/call", {"name": t["name"], "arguments": args}, token=reader))
        checked += 1
        why = next((f"a credential the operator sees here too ({len(x)} characters, from {origin.get(x, '?').strip()})" for x in secrets if x in text), None)
        why = why or next((kind for kind, rx in SHAPES if rx.search(text)), None)
        if why:
            leaks.append(f"{t['name']}: {why}")
        if "-v" in sys.argv and known:
            print(f"redacted for the reader: {t['name']} ({len(known)} credential(s) the operator sees)")
if "-v" in sys.argv:
    for x, w in origin.items():
        print(f"credential from {w.strip()} ({len(x)} characters)")
print(f"leaks: {checked} read tools called as a reader that can't reveal, {len(secrets)} known credentials, {shown} tools showed the operator one, {len(leaks)} showed the reader one")
for l in leaks:
    print("LEAK", l)
if reader and shown == 0:
    leaks.append("no tool showed even the operator a known credential: the check sees nothing (are the fixtures' secrets collected?)")

# A member's own client (docs/mcp.md, "A person's own connection"): a person signed in by email
# code, a viewer in the sweep's project, with a token that names every action. Their client lists
# a member product's tools as their role allows (within their session, as the gate decides calls),
# so none it lists is refused by their role, and a write their role can't make isn't listed.
import os, glob, time
def http(method, path, body=None, headers=None):
    req = urllib.request.Request(f"{api}{path}", data=json.dumps(body).encode() if body is not None else None, method=method,
                                 headers={"content-type": "application/json", **(headers or {})})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else None), r.headers
    except urllib.error.HTTPError as e:
        raw = e.read()
        try:
            return e.code, json.loads(raw), e.headers
        except ValueError:
            return e.code, raw.decode(errors="replace"), e.headers
member_fails = []
outbox = os.environ.get("MCP_SWEEP_OUTBOX")
def member_token():
    op = {"authorization": f"Bearer {tok}"}
    s, inv, _ = http("POST", "/v1/accounts/invites", {"email": "viewer@example.com"}, op)
    if s >= 300:
        return f"inviting a person: {s} {inv}"
    s, start, _ = http("POST", "/v1/accounts/email/start", {"email": "viewer@example.com", "invite": inv["token"]})
    if s >= 300:
        return f"starting a sign-in: {s} {start}"
    code = None
    for _ in range(100):
        for f in glob.glob(os.path.join(outbox, "*")):
            try:
                subject = json.load(open(f)).get("subject", "")
            except (ValueError, OSError):
                continue
            if "sign-in code" in subject:
                code = subject[:6]
        if code:
            break
        time.sleep(0.1)
    if not code:
        return "no sign-in mail in the outbox"
    s, _, h = http("POST", "/v1/accounts/email/verify", {"challenge": start["challenge"], "code": code})
    if s != 200:
        return f"verifying the code: {s}"
    cookie = h.get("set-cookie", "").split(";")[0]
    s, session, _ = http("GET", "/v1/accounts/session", headers={"cookie": cookie})
    me = {"cookie": cookie, "origin": api, "x-roll-csrf": (session or {}).get("csrf", "")}
    s, pinv, _ = http("POST", f"/v1/workspace/projects/{ws}/invites", {"email": "viewer@example.com", "role": "viewer"}, op)
    if s >= 300:
        return f"inviting them to the project: {s} {pinv}"
    s, acc, _ = http("POST", f"/v1/workspace/invites/{pinv['url'].split('token=')[1]}/accept", None, me)
    if s >= 300:
        return f"accepting: {s} {acc}"
    every = [{"product": "*", "resources": "*", "actions": {a: "allow" for a in ("read", "write", "delete", "reveal", "deploy", "admin")}}]
    s, made, _ = http("POST", "/v1/access/tokens", {"name": "sweep-viewer", "rules": every}, me)
    if s >= 300:
        return f"minting their token: {s} {made}"
    secrets.discard(made["token"])
    return made
viewer = member_token() if outbox and ws else "no outbox or project"
if isinstance(viewer, dict):
    vt = viewer["token"]
    vtools = rpc("tools/list", {}, token=vt).get("result", {}).get("tools", [])
    names = {t["name"] for t in vtools}
    if "secrets_list" not in names or names & {"secrets_set", "secrets_get_values"}:
        member_fails.append(f"a viewer's client should list secrets_list and not secrets_set or secrets_get_values: {sorted(n for n in names if n.startswith('secrets_'))[:20]}")
    called = 0
    for t in vtools:
        prod = t.get("_meta", {}).get("roll/product", "")
        if prod not in ("secrets", "previews", "workspace") or not t.get("annotations", {}).get("readOnlyHint"):
            continue
        req = t["inputSchema"].get("required", [])
        args = {a: seen[(prod, a)] if (prod, a) in seen else stand_in(t, prod, a) for a in req}
        res = rpc("tools/call", {"name": t["name"], "arguments": args}, token=vt).get("result", {})
        called += 1
        text = json.dumps(res.get("structuredContent")) + " ".join(c.get("text", "") for c in res.get("content", []))
        if res.get("isError") and ("your role in project" in text or "your roles can't" in text):
            member_fails.append(f"{t['name']} is listed for a viewer but refused by their role: {text[:160]}")
    print(f"member: a viewer's client lists {len(vtools)} tools; {called} read tools of member products called, {len(member_fails)} refused by the role that listed them")
else:
    member_fails.append(f"the member pass couldn't start: {viewer}")
for f in member_fails:
    print("MFAIL", f)
fails += [f"leak {l}" for l in leaks]
fails += [f"member {f}" for f in member_fails]

# Choices: a tool called without an id it needs, by a client that fills in forms (2026-07-28 with
# form elicitation), answers a form whose choices are what the connection may read
# (`roll_mcp::ask`). For every kind of id the fixtures made, the form must offer the fixture's,
# or the kind is below with why it has none: a list answer of another shape offers nothing
# without failing anything, and a new kind of id with no choices fails here until it has some.
NO_CHOICES = {("postgres", "endpoint"): "an endpoint sits inside its branch, and no tool lists every project's"}
# Where a name means different things by tool, the one the fixture's id is: tasks' `id` is a
# message's in the fixture (a run's, a schedule's… by their own tools' routes).
CHOICE_TOOL = {("tasks", "id"): "tasks_get_message"}
def form_call(name, args):
    global n
    n += 1
    meta = {"io.modelcontextprotocol/protocolVersion": "2026-07-28", "io.modelcontextprotocol/clientInfo": {"name": "sweep", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {"elicitation": {"form": {}}}}
    body = {"jsonrpc": "2.0", "id": n, "method": "tools/call", "params": {"name": name, "arguments": args, "_meta": meta}}
    req = urllib.request.Request(f"{api}/v1/mcp", data=json.dumps(body).encode(), headers={"authorization": f"Bearer {tok}", "content-type": "application/json",
                                 "accept": "application/json", "mcp-protocol-version": "2026-07-28", "mcp-method": "tools/call", "mcp-name": name})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)
offered, no_form = 0, []
for (prod, param), value in sorted(seen.items()):
    if (prod, param) in NO_CHOICES:
        continue
    tool = next((t for t in reads if t.get("_meta", {}).get("roll/product") == prod and param in t["inputSchema"].get("required", [])
                 and all((prod, a) in seen for a in t["inputSchema"]["required"]) and t["name"] == CHOICE_TOOL.get((prod, param), t["name"])), None)
    if tool is None:
        continue
    args = {a: seen[(prod, a)] for a in tool["inputSchema"]["required"] if a != param}
    r = form_call(tool["name"], args).get("result", {})
    field = (r.get("inputRequests", {}).get(f"missing:{param}", {}).get("params", {}).get("requestedSchema", {}).get("properties", {}).get("value", {}))
    if r.get("resultType") != "input_required" or not field:
        no_form.append(f"{prod} {param} ({tool['name']} answered {r.get('resultType')}: {json.dumps(r.get('structuredContent'))[:120]})")
    elif value not in field.get("enum", [value] if "description" in field else []):
        no_form.append(f"{prod} {param} ({tool['name']}'s form offers {json.dumps(field.get('enum'))[:160]}, not the fixture's {value})")
    else:
        offered += 1
        if "-v" in sys.argv:
            print(f"choices: {prod} {param} from {tool['name']}: {json.dumps(field.get('enum'))[:160]}")
print(f"choices: {offered} kinds of id offered as a form's choices, {len(NO_CHOICES)} with none by design, {len(no_form)} missing")
fails += [f"choices {c}" for c in no_form]

# The rest of what a client reads: every prompt (its arguments from the fixtures), every resource
# listed, a project's page, every skill. An error is a failure: they're all read-only.
surface = []
PROMPT_ARGS = {"project": seen.get(("workspace", "id")), "service": seen.get(("apps", "service")), "env": "production", "branch": seen.get(("postgres", "branch")),
               "preview": seen.get(("previews", "id")), "vhost": seen.get(("queues", "vhost")), "queue": seen.get(("queues", "queue")),
               "stream": seen.get(("streams", "stream")), "group": seen.get(("streams", "group"))}
for p in rpc("prompts/list", {}).get("result", {}).get("prompts", []):
    args = {a["name"]: (seen.get(("postgres", "project")) if a["name"] == "project" and "Postgres" in a.get("description", "") else PROMPT_ARGS.get(a["name"]) or "sweep")
            for a in p.get("arguments", [])}
    surface.append((f"prompt {p['name']}", rpc("prompts/get", {"name": p["name"], "arguments": args})))
    # Each argument a fixture made completes to it (the others as context, as a client sends them).
    for a in p.get("arguments", []):
        want = args[a["name"]]
        if a["name"] not in PROMPT_ARGS or want in (None, "sweep"):
            continue
        r = rpc("completion/complete", {"ref": {"type": "ref/prompt", "name": p["name"]}, "argument": {"name": a["name"], "value": ""},
                                        "context": {"arguments": {k: v for k, v in args.items() if k != a["name"]}}})
        got = r.get("result", {}).get("completion", {}).get("values", [])
        surface.append((f"completion {p['name']} {a['name']}", r if want in got else {"error": f"offers {got[:5]}, not the fixture's {want}"}))
uris = [r["uri"] for r in rpc("resources/list", {}).get("result", {}).get("resources", [])]
if seen.get(("workspace", "id")):
    uris.append(f"roll://projects/{seen[('workspace', 'id')]}")
if repo_made:
    uris.append("roll://code/sweep/app/README.md")  # a roll code file, read through code_read_file
for uri in uris:
    r = rpc("resources/read", {"uri": uri})
    if uri.startswith("roll://code/") and "error" not in r and "sweep" not in json.dumps(r.get("result", {})):
        r = {"error": f"a code file's resource doesn't hold the file: {json.dumps(r)[:200]}"}
    surface.append((f"resource {uri}", r))
for sk in rpc("skills/list", {}).get("result", {}).get("skills", []):
    surface.append((f"skill {sk.get('uri') or sk.get('name')}", rpc("skills/get", {"uri": sk.get("uri", "")})))
bad = [f"{what}: {json.dumps(r.get('error'))[:200]}" for what, r in surface if "error" in r]
print(f"surfaces: {len(surface)} prompts, completions, resources and skills read, {len(bad)} failed")
fails += bad
if "--writes" in sys.argv:
    def fake(name, schema, prod):
        if (prod, name) in seen:
            return seen[(prod, name)]
        ty = schema.get("type")
        if ty == "integer" or ty == "number":
            return 1
        if ty == "boolean":
            return False
        if ty == "object":
            return {}
        if ty == "array":
            return []
        if any(w in name for w in ("url", "endpoint", "webhook", "target")):
            return "https://example.invalid/sweep"
        return f"sweep-{name}"
    # Arguments a tool can't require because only some of another's values need them, which its
    # default does: a notification's url (a webhook's; email, PagerDuty and Opsgenie take none), a
    # drain's (a bucket drain takes none).
    REQUIRED_BY_DEFAULT = {"apps_add_notification": ["url"], "apps_add_drain": ["url"]}
    writes = [t for t in tools if not t.get("annotations", {}).get("readOnlyHint") and "wait" not in t["name"] and (only is None or t["name"] == only)]
    writes.sort(key=lambda t: bool(t.get("annotations", {}).get("destructiveHint")))
    wran, wfails, wslow = 0, [], []
    for t in writes:
        prod = t.get("_meta", {}).get("roll/product", "")
        props = t["inputSchema"].get("properties", {})
        args = {a: fake(a, props.get(a, {}), prod) for a in t["inputSchema"].get("required", []) + REQUIRED_BY_DEFAULT.get(t["name"], [])}
        try:
            res = rpc("tools/call", {"name": t["name"], "arguments": args})
        except Exception as e:
            wslow.append(f"{t['name']}: {e}")
            continue
        wran += 1
        bad = judge(t["name"], res)
        # A field missing inside an object the sweep made up ({}) is about the made-up value.
        if bad and "missing field" in bad and any(v == {} for v in args.values()):
            bad = None
        if bad:
            wfails.append(f"{t['name']} {json.dumps(args)[:160]}: {bad}")
    print(f"{len(writes)} write tools, {wran} called, {len(wslow)} timed out, {len(wfails)} failed")
    for f in wfails:
        print("WFAIL", f)
    for f in wslow:
        print("SLOW", f)
    fails += wfails
if record:
    with open(record, "w") as f:
        json.dump(samples, f)
    print(f"recorded answers of {len(samples)} tools in {record}")
fails += unfit
for f in fails:
    print("FAIL", f)
if "-v" in sys.argv:
    for s in stood_in:
        print("stand-in", s)
sys.exit(1 if fails else 0)
