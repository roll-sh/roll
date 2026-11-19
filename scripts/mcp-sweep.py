#!/usr/bin/env python3
"""Every MCP tool a fresh rolld lists, called through /v1/mcp as an agent would (scripts/test-mcp-sweep.sh
starts the rolld). It first makes fixtures with the write tools (a workspace project, a bucket, a
stream, NoSQL and Redis databases, a search cluster and index, a Postgres project, a queue, an apps
service, a roll code repo, a tasks message, a token), then calls each read-only tool with ids from
them, then, with --writes, each write tool with values made from its schema (non-destructive first).

A failure is a mapping bug: a 5xx, an argument the route rejects (unknown, missing or of another type),
or an empty 404 (no route answers the path). "Not found" for a missing resource, a refusal about a
made-up value and a product that's off are fine.

    mcp-sweep.py <api> <operator token file> [--writes] [-v] [--tool NAME]
"""
import json, sys, urllib.error, urllib.request

api, tok = sys.argv[1], open(sys.argv[2]).read().strip()
only = sys.argv[sys.argv.index("--tool") + 1] if "--tool" in sys.argv else None
n = 0


def rpc(method, params):
    global n
    n += 1
    req = urllib.request.Request(f"{api}/v1/mcp", data=json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}).encode(),
                                 headers={"authorization": f"Bearer {tok}", "content-type": "application/json", "accept": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)


tools = rpc("tools/list", {})["result"]["tools"]
reads = [t for t in tools if t.get("annotations", {}).get("readOnlyHint") and (only is None or t["name"] == only)]
FAIL_WORDS = ("unknown argument", "is required", "can't be empty", "no route", "method not allowed", "invalid type", "missing field", "unknown field")


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
def call(name, args):
    r = rpc("tools/call", {"name": name, "arguments": args}).get("result", {})
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
call("queues_declare_queue", {"vhost": "sweep", "name": "q1"})
seen = {("storage", "bucket"): "sweep-bkt", ("streams", "stream"): "sweep", ("streams", "topic"): "t1", ("nosql", "database"): "sweep",
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
            return json.load(r)
    except urllib.error.HTTPError as e:
        # A fixture that isn't made leaves its tools skipped: say why (an id or a message, never a secret).
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

fails, ran, skipped = [], 0, []
# Two passes: tools without required arguments first (their answers give ids), then the others.
for pass_ in (0, 1):
    for t in reads:
        req = t["inputSchema"].get("required", [])
        if (pass_ == 0) != (not req):
            continue
        args = {}
        prod = t.get("_meta", {}).get("roll/product", "")
        if req:
            missing = [a for a in req if (prod, a) not in seen]
            if missing:
                skipped.append(f"{t['name']} (needs {', '.join(missing)})")
                continue
            args = {a: seen[(prod, a)] for a in req}
        res = rpc("tools/call", {"name": t["name"], "arguments": args})
        ran += 1
        bad = judge(t["name"], res)
        if bad:
            fails.append(f"{t['name']} {json.dumps(args)}: {bad}")
        else:
            for it in items_of(res.get("result", {}).get("structuredContent")):
                if isinstance(it, dict) and isinstance(it.get("id"), str):
                    prod = t.get("_meta", {}).get("roll/product", "")
                    key = {"postgres": "project", "redis": "database", "nosql": "database", "search": "cluster", "streams": "stream", "apps": "service", "previews": "id", "workspace": "id", "secrets": "project"}.get(prod)
                    if key and (prod, key) not in seen and t["name"].split("_")[1] == "list":
                        seen[(prod, key)] = it["id"]
print(f"{len(reads)} read-only tools, {ran} called, {len(skipped)} skipped for want of an id, {len(fails)} failed")
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
    writes = [t for t in tools if not t.get("annotations", {}).get("readOnlyHint") and "wait" not in t["name"] and (only is None or t["name"] == only)]
    writes.sort(key=lambda t: bool(t.get("annotations", {}).get("destructiveHint")))
    wran, wfails, wslow = 0, [], []
    for t in writes:
        prod = t.get("_meta", {}).get("roll/product", "")
        props = t["inputSchema"].get("properties", {})
        args = {a: fake(a, props.get(a, {}), prod) for a in t["inputSchema"].get("required", [])}
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
for f in fails:
    print("FAIL", f)
if "-v" in sys.argv:
    for s in skipped:
        print("skip", s)
sys.exit(1 if fails else 0)
