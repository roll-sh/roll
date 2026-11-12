"""A stand-in for the two AWS endpoints a search-proxy wake touches, for the image smoke test.

- `GET /creds` (and `/creds/<caller>`, recorded with its path): a container credentials
  endpoint named by a full URI (AWS_CONTAINER_CREDENTIALS_FULL_URI), so the proxy (and with --s3
  node b's S3 client) take that path: temporary credentials with a session token, fetched with
  the container authorization token.
- `GET /v2/credentials/<id>`: the ECS agent's task credentials endpoint, which ECS tasks reach
  at 169.254.170.2 with AWS_CONTAINER_CREDENTIALS_RELATIVE_URI (no authorization token): the
  nodes' S3 client takes this path, served on the optional third port.
- `POST /`: the ECS JSON API. `DescribeServices` and `UpdateService` only, each request's SigV4
  signature checked against the credentials handed out above (a wrong signature is a 403, as on
  AWS). The service's desired count lives in `<state>/desired`, which the smoke test reads to
  start a node (it plays the ECS scheduler) and writes to stop one (the ToZeroPolicy).
  Every call is appended to `<state>/calls` as one JSON line.

Usage: python3 fake_ecs.py <port> <state dir> [<task credentials port>]. Standard library only.
"""

import hashlib
import hmac
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

KEY, SECRET, TOKEN = "ASIASMOKETEST", "smoke-secret-key", "smoke-session-token"
AUTH = "smoke-container-auth"
TARGET = "AmazonEC2ContainerServiceV20141113."


def signature_ok(method, path, headers, body):
    """Recomputes the SigV4 signature of a request (AWS's documented algorithm)."""
    auth = headers.get("authorization", "")
    if not auth.startswith("AWS4-HMAC-SHA256 "):
        return False, "no SigV4 authorization"
    parts = dict(p.strip().split("=", 1) for p in auth[len("AWS4-HMAC-SHA256 "):].split(","))
    key, date, region, service, term = parts["Credential"].split("/")
    if key != KEY or term != "aws4_request" or service != "ecs":
        return False, f"unexpected credential scope {parts['Credential']}"
    if headers.get("x-amz-security-token") != TOKEN:
        return False, "session token missing"
    signed = parts["SignedHeaders"].split(";")
    canonical_headers = "".join(f"{h}:{' '.join(headers.get(h, '').split())}\n" for h in signed)
    canonical = "\n".join([method, path, "", canonical_headers, parts["SignedHeaders"], hashlib.sha256(body).hexdigest()])
    amz_date = headers.get("x-amz-date", "")
    scope = f"{date}/{region}/{service}/aws4_request"
    to_sign = "\n".join(["AWS4-HMAC-SHA256", amz_date, scope, hashlib.sha256(canonical.encode()).hexdigest()])
    k = ("AWS4" + SECRET).encode()
    for part in (date, region, service, "aws4_request"):
        k = hmac.new(k, part.encode(), hashlib.sha256).digest()
    want = hmac.new(k, to_sign.encode(), hashlib.sha256).hexdigest()
    return hmac.compare_digest(want, parts["Signature"]), "signature mismatch"


class Handler(BaseHTTPRequestHandler):
    state = "."

    def log_message(self, *args):
        pass

    def reply(self, status, doc):
        data = json.dumps(doc).encode()
        self.send_response(status)
        self.send_header("content-type", "application/x-amz-json-1.1")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def record(self, entry):
        with open(os.path.join(self.state, "calls"), "a") as f:
            f.write(json.dumps(entry) + "\n")

    def desired(self, value=None):
        path = os.path.join(self.state, "desired")
        if value is not None:
            with open(path + ".tmp", "w") as f:
                f.write(f"{value}\n")
            os.replace(path + ".tmp", path)
        try:
            with open(path) as f:
                return int(f.read().strip() or 0)
        except FileNotFoundError:
            return 0

    def do_GET(self):
        if self.path.startswith("/v2/credentials/"):
            self.record({"action": "task-creds", "ok": True})
            return self.reply(200, {"AccessKeyId": KEY, "SecretAccessKey": SECRET, "Token": TOKEN, "Expiration": "2099-01-01T00:00:00Z",
                                    "RoleArn": "arn:aws:iam::000000000000:role/smoke-task"})
        if self.path != "/creds" and not self.path.startswith("/creds/"):
            return self.reply(404, {"message": "not found"})
        if self.headers.get("authorization") != AUTH:
            self.record({"action": "creds", "path": self.path, "ok": False})
            return self.reply(401, {"message": "bad container authorization token"})
        self.record({"action": "creds", "path": self.path, "ok": True})
        self.reply(200, {"AccessKeyId": KEY, "SecretAccessKey": SECRET, "Token": TOKEN, "Expiration": "2099-01-01T00:00:00Z"})

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", 0)))
        headers = {k.lower(): v for k, v in self.headers.items()}
        action = headers.get("x-amz-target", "").removeprefix(TARGET)
        ok, why = signature_ok("POST", self.path, headers, body)
        if not ok:
            self.record({"action": action, "ok": False, "why": why})
            return self.reply(403, {"__type": "InvalidSignatureException", "message": why})
        req = json.loads(body or b"{}")
        if action == "DescribeServices":
            self.record({"action": action, "ok": True, "services": req.get("services"), "desired": self.desired()})
            return self.reply(200, {"services": [{"serviceName": s, "desiredCount": self.desired()} for s in req.get("services", [])], "failures": []})
        if action == "UpdateService":
            n = int(req["desiredCount"])
            self.desired(n)
            self.record({"action": action, "ok": True, "cluster": req.get("cluster"), "service": req.get("service"), "desiredCount": n})
            return self.reply(200, {"service": {"serviceName": req.get("service"), "desiredCount": n}})
        self.record({"action": action, "ok": False, "why": "unsupported"})
        self.reply(400, {"__type": "InvalidParameterException", "message": f"unsupported action {action}"})


if __name__ == "__main__":
    Handler.state = sys.argv[2]
    if len(sys.argv) > 3:
        import threading
        extra = ThreadingHTTPServer(("0.0.0.0", int(sys.argv[3])), Handler)
        threading.Thread(target=extra.serve_forever, daemon=True).start()
    ThreadingHTTPServer(("0.0.0.0", int(sys.argv[1])), Handler).serve_forever()
