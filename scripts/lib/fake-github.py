#!/usr/bin/env python3
"""A fake GitHub API for roll's live checks (scripts/preview-apps.sh): one App installation that
sees acme/shop, its token, and the writes roll makes there (commit statuses, issue comments and
their edits, deployments and their statuses). Every write is
recorded; GET /_fake/log answers them as JSON. Any Bearer is accepted (roll signs a JWT with
the App's key, then uses the installation token).

    python3 fake-github.py <port>
"""
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOG = []
LOCK = threading.Lock()
NEXT = [1000]
REPO = {"id": 7, "name": "shop", "full_name": "acme/shop", "private": True, "default_branch": "main", "owner": {"login": "acme", "id": 1, "type": "Organization"}}
INSTALLATION = {"id": 1, "account": {"login": "acme", "id": 1, "type": "Organization"}, "repository_selection": "selected", "suspended_at": None, "app_id": 1}


class H(BaseHTTPRequestHandler):
    def answer(self, status, body):
        raw = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def body(self):
        n = int(self.headers.get("content-length") or 0)
        return json.loads(self.rfile.read(n) or b"{}") if n else {}

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/_fake/log":
            with LOCK:
                return self.answer(200, LOG)
        if path == "/app/installations":
            return self.answer(200, [INSTALLATION])
        if path == "/installation/repositories":
            return self.answer(200, {"total_count": 1, "repositories": [REPO]})
        if path == "/repos/acme/shop":
            return self.answer(200, REPO)
        return self.answer(404, {"message": "Not Found"})

    def do_POST(self):
        path = self.path.split("?")[0]
        b = self.body()
        if path.startswith("/app/installations/") and path.endswith("/access_tokens"):
            return self.answer(201, {"token": "ghs_fake", "expires_at": "2099-01-01T00:00:00Z"})
        if path.startswith("/repos/acme/shop/statuses/"):
            with LOCK:
                LOG.append({"kind": "status", "sha": path.rsplit("/", 1)[1], "state": b.get("state"), "context": b.get("context"), "description": b.get("description"), "target_url": b.get("target_url")})
            return self.answer(201, {"id": 1})
        if path == "/repos/acme/shop/deployments":
            with LOCK:
                NEXT[0] += 1
                did = NEXT[0]
                LOG.append({"kind": "deployment", "id": did, "sha": b.get("ref"), "environment": b.get("environment")})
            return self.answer(201, {"id": did})
        if path.startswith("/repos/acme/shop/deployments/") and path.endswith("/statuses"):
            with LOCK:
                LOG.append({"kind": "deployment_status", "id": int(path.split("/")[5]), "state": b.get("state")})
            return self.answer(201, {"id": 1})
        if path.startswith("/repos/acme/shop/issues/") and path.endswith("/comments"):
            with LOCK:
                NEXT[0] += 1
                cid = NEXT[0]
                LOG.append({"kind": "comment", "number": int(path.split("/")[5]), "id": cid, "body": b.get("body", "")})
            return self.answer(201, {"id": cid, "body": b.get("body", "")})
        return self.answer(404, {"message": "Not Found"})

    def do_PATCH(self):
        path = self.path.split("?")[0]
        b = self.body()
        if path.startswith("/repos/acme/shop/issues/comments/"):
            cid = int(path.rsplit("/", 1)[1])
            with LOCK:
                LOG.append({"kind": "edit", "id": cid, "body": b.get("body", "")})
            return self.answer(200, {"id": cid, "body": b.get("body", "")})
        return self.answer(404, {"message": "Not Found"})

    def log_message(self, *a):
        pass


ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
