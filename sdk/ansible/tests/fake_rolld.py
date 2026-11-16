"""rolld as the collection sees it (docs/secrets.md, "HTTP API"): a workload identity's login,
whoami, a folder's values (dynamic=false, digest entity tags and 304s), and one key's record,
stored value, write and delete. Every value here is obviously fake."""

import copy
import hashlib
import json
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOKEN = "rlt_fake_token_for_tests"
WRITE_ONLY = "rlt_fake_write_only_token"
IDENTITY, OIDC = "idn_ci", "fake-oidc-token-for-tests"
PROJECT = "shop"


def own(value):
    return {"value": value, "comment": "", "version": 1, "link": None}


def initial():
    return {
        # (environment, folder) -> key -> record
        "folders": {
            ("production", "/api"): {"DB_URL": own("fake-db-url-not-a-secret"), "API_KEY": own("fake-api-key-0000")},
            ("production", "/broken"): {"REF": own("fake-ref-${MISSING}")},
            # Ansible must never template a value.
            ("production", "/templates"): {"TEMPLATED": own("fake-{{ 7 * 6 }}-not-templated"), "OTHER": own("fake-other-value-0000")},
        },
        # Keys showing through a base (not the folder's own).
        "inherited": {("production", "/api"): {"FROM_BASE": "fake-inherited-value"}},
        # Keys holding a dynamic credential: each read that includes one issues a lease.
        "dynamic": {("production", "/api"): ["DB_PASSWORD"]},
        "protected": ["locked"],
        "seq": 0,
        "leases": 0,
        "logins": 0,
        "requests": [],
    }


class Rolld(BaseHTTPRequestHandler):
    state = initial()
    lock = threading.Lock()

    @classmethod
    def reset(cls):
        with cls.lock:
            cls.state = initial()

    @classmethod
    def secret_values(cls):
        """Every value the fake holds or issues: none may appear in Ansible's output."""
        s = cls.state
        out = [r["value"] for f in s["folders"].values() for r in f.values()]
        out += [v for f in s["inherited"].values() for v in f.values()]
        return out + [TOKEN, WRITE_ONLY, OIDC, "fake-dynamic-password-1"]

    def log_message(self, *_):
        pass

    def answer(self, code, body=None, headers=None):
        data = b"" if body is None else json.dumps(body).encode()
        self.send_response(code)
        for k, v in (headers or {}).items():
            self.send_header(k, v)
        if body is not None:
            self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def body(self):
        return json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")

    def route(self):
        u = urllib.parse.urlsplit(self.path)
        q = {k: v[-1] for k, v in urllib.parse.parse_qs(u.query).items()}
        parts = [urllib.parse.unquote(p) for p in u.path.split("/")[1:]]
        return parts, q

    def handle_any(self, method):
        parts, q = self.route()
        with Rolld.lock:
            s = Rolld.state
            s["requests"].append((method, self.path))
            if parts == ["v1", "secrets", "login"] and method == "POST":
                b = self.body()
                if (b.get("identity"), b.get("token")) != (IDENTITY, OIDC):
                    return self.answer(401, {"error": "no identity takes this token", "code": "unauthorized"})
                s["logins"] += 1
                return self.answer(200, {"token": TOKEN, "project": PROJECT, "identity": IDENTITY, "expires_at": int(time.time() * 1000) + 3_600_000})
            auth = self.headers.get("Authorization")
            if auth not in ("Bearer " + TOKEN, "Bearer " + WRITE_ONLY):
                return self.answer(401, {"error": "no such token", "code": "unauthorized"})
            if parts == ["v1", "secrets", "whoami"]:
                return self.answer(200, {"principal": "token:ci", "project": PROJECT})
            if len(parts) < 7 or parts[:3] != ["v1", "secrets", "projects"] or parts[4] != "environments":
                return self.answer(404, {"error": "no such route"})
            if parts[3] != PROJECT:
                return self.answer(404, {"error": "no such project"})
            env, path, rest = parts[5], q.get("path", "/"), parts[6:]
            where = (env, path)
            if rest == ["values"] and method == "GET":
                return self.values(s, where, q)
            if rest[0] != "secrets" or len(rest) < 2:
                return self.answer(404, {"error": "no such route"})
            key, folder = rest[1], s["folders"].setdefault(where, {})
            if rest[2:] == ["value"] and method == "GET":
                if auth != "Bearer " + TOKEN:
                    return self.answer(403, {"error": "this token may not reveal", "code": "forbidden"})
                if key not in folder:
                    return self.answer(404, {"error": f"no secret {key} in {env}:{path}"})
                assert q.get("resolve") == "false", "the module compares the stored value"
                return self.answer(200, {"value": folder[key]["value"], "problems": []})
            if rest[2:]:
                return self.answer(404, {"error": "no such route"})
            if method == "GET":
                if key in folder:
                    return self.answer(200, self.secret(key, path, folder[key]))
                if key in s["inherited"].get(where, {}):
                    inherited = dict(self.secret(key, path, own("")), source={"kind": "base", "environment": "development"})
                    return self.answer(200, inherited)
                return self.answer(404, {"error": f"no secret {key} in {env}:{path}"})
            if env in s["protected"]:
                return self.answer(409, {"error": f"{env} is protected: open a change request", "code": "approval_required"})
            if method == "PUT":
                b = self.body()
                cur = folder.get(key)
                comment = b.get("comment", cur["comment"] if cur else "")
                if cur and cur["value"] == b.get("value") and cur["comment"] == comment and cur["link"] is None:
                    return self.answer(200, {"change": None, "secret": self.secret(key, path, cur), "warnings": []})
                s["seq"] += 1
                folder[key] = {"value": b.get("value"), "comment": comment, "version": (cur["version"] + 1) if cur else 1, "link": None}
                return self.answer(200, {"change": self.change(s, env, path), "secret": self.secret(key, path, folder[key]), "warnings": []})
            if method == "DELETE":
                if key not in folder:
                    return self.answer(404, {"error": f"no secret {key} in {env}:{path}"})
                del folder[key]
                s["seq"] += 1
                return self.answer(200, {"change": self.change(s, env, path), "secret": None, "warnings": []})
            return self.answer(405, {"error": "method"})

    @staticmethod
    def secret(key, path, r):
        return {"key": key, "path": path, "version": r["version"], "comment": r["comment"], "link": r["link"], "source": {"kind": "own"}}

    @staticmethod
    def change(s, env, path):
        return {"seq": s["seq"], "environment": env, "path": path, "message": "", "by": "token:ci", "at": 0, "keys": 1, "kind": "write"}

    def values(self, s, where, q):
        own = s["folders"].get(where, {})
        values = dict(s["inherited"].get(where, {}))
        values.update({k: r["value"] for k, r in own.items()})
        problems = [{"key": k, "problem": "references a key that isn't set"} for k, v in values.items() if "${MISSING}" in v]
        for k in problems:
            values.pop(k["key"])
        dynamic = list(s["dynamic"].get(where, []))
        leases = []
        # A folder holding a dynamic credential is never answered with a 304 (a lease ends without
        # moving the digest) unless dynamic=false.
        digest = hashlib.sha256(json.dumps(sorted(values.items())).encode()).hexdigest()[:16]
        cacheable = not dynamic or q.get("dynamic") == "false"
        tag = self.headers.get("If-None-Match")
        if cacheable and tag == f'"{digest}"':
            return self.answer(304, None, {"ETag": f'"{digest}"'})
        if q.get("dynamic") != "false":
            for k in dynamic:
                s["leases"] += 1
                values[k] = f"fake-dynamic-password-{s['leases']}"
                leases.append(f"lse_{s['leases']}")
        body = {"values": values, "problems": problems, "leases": leases, "dynamic": dynamic, "linked": []}
        headers = {}
        if q.get("digest") == "1" or tag:
            body["digest"] = digest
            headers["ETag"] = f'"{digest}"'
        return self.answer(200, copy.deepcopy(body), headers)

    def do_GET(self):
        self.handle_any("GET")

    def do_PUT(self):
        self.handle_any("PUT")

    def do_POST(self):
        self.handle_any("POST")

    def do_DELETE(self):
        self.handle_any("DELETE")


def serve():
    """A fake rolld on a free port in a thread; returns (url, server)."""
    Rolld.reset()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Rolld)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{server.server_address[1]}", server
