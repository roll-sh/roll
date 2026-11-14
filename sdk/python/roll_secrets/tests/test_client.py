import json
import os
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock

from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey

import roll_secrets
from roll_secrets import _unb64

# A box sealed by @roll/secrets' sealTo (the same box as the server's seal_to), with a fixed key:
# the same vector as sdk/go/secrets.
KEY = X25519PrivateKey.from_private_bytes(_unb64("AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA"))
CONTEXT = "roll-secrets:prj_a:production:app:web"
BOX = {"v": 1, "epk": "aAGjVaESpknkSH_uG48K1a_unnp97Gh4uRwOYWrjHkk", "nonce": "yct4_uAUdnUKxskR", "ct": "-UbR7dzvRmoXgBHnbyoW00qyGY07tIB_0vx6VrXQhPPRNns7PU3irIJq5M5Xjto"}


class SealedBoxes(unittest.TestCase):
    def test_a_box_sealed_by_roll_opens_only_for_its_context(self):
        self.assertEqual(roll_secrets.open_sealed(BOX, KEY, CONTEXT), b'{"DB_URL":"postgres://db/shop"}')
        with self.assertRaises(ValueError):
            roll_secrets.open_sealed(BOX, KEY, "roll-secrets:prj_a:staging:app:web")
        with self.assertRaises(ValueError):
            roll_secrets.open_sealed(BOX, roll_secrets.generate_key(), CONTEXT)


class Rolld(BaseHTTPRequestHandler):
    """rolld as the client sees it: a workload identity's login, whoami, values, a sealed resolve,
    and a digest that moves when the values change."""

    state = {"digest": "d1", "value": "one"}

    def log_message(self, *_):
        pass

    def answer(self, code, body=None):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        if body is not None:
            self.wfile.write(json.dumps(body).encode())

    def body(self):
        return json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)) or b"{}")

    def do_POST(self):
        env = "/v1/secrets/projects/prj_a/environments/production"
        if self.path == "/v1/secrets/login":
            b = self.body()
            if (b.get("identity"), b.get("token")) != ("idn_ci", "the-oidc-token"):
                return self.answer(401, {"error": "no identity idn_ci takes this token", "code": "unauthorized"})
            return self.answer(200, {"token": "rlt_workload", "project": "prj_a", "expires_at": 1})
        if self.headers.get("Authorization") != "Bearer rlt_workload":
            return self.answer(401)
        if self.path == env + "/resolve":
            b = self.body()
            if (b.get("consumer"), b.get("seal_to")) != ("app:web", roll_secrets.public_key(KEY)):
                return self.answer(400)
            return self.answer(200, {"sealed": BOX, "context": CONTEXT, "problems": [], "leases": [], "dynamic": [], "linked": ["DB_URL"]})
        self.answer(404)

    def do_GET(self):
        env = "/v1/secrets/projects/prj_a/environments/production"
        if self.headers.get("Authorization") != "Bearer rlt_workload":
            return self.answer(401)
        if self.path == env + "/digest":
            return self.answer(200, {"digest": Rolld.state["digest"]})
        if self.path == env + "/values":
            return self.answer(200, {"values": {"A": Rolld.state["value"]}, "problems": [], "leases": []})
        self.answer(404)


class AWorkload(unittest.TestCase):
    def test_logs_in_reads_sealed_values_and_watches_for_changes(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Rolld)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        api = f"http://127.0.0.1:{server.server_address[1]}"
        with mock.patch.dict(os.environ, {"ROLL_API_URL": api, "ROLL_IDENTITY": "idn_ci", "ROLL_OIDC_TOKEN": "wrong"}, clear=False):
            with self.assertRaises(roll_secrets.SecretsError) as e:
                roll_secrets.from_env()
            self.assertEqual(e.exception.status, 401)
        with mock.patch.dict(os.environ, {"ROLL_API_URL": api, "ROLL_IDENTITY": "idn_ci", "ROLL_OIDC_TOKEN": "the-oidc-token"}, clear=False):
            c = roll_secrets.from_env()

        sealed = c.resolve_sealed("production", "app:web", KEY)
        self.assertEqual((sealed.values, sealed.linked), ({"DB_URL": "postgres://db/shop"}, ["DB_URL"]))

        seen, stop = [], threading.Event()
        first, changed = threading.Event(), threading.Event()

        def on_change(v):
            seen.append(v.values["A"])
            (first if len(seen) == 1 else changed).set()

        watching = threading.Thread(target=c.watch, args=("production", on_change), kwargs={"every": 0.01, "stop": stop})
        watching.start()
        self.addCleanup(watching.join)
        self.addCleanup(stop.set)
        self.assertTrue(first.wait(5))
        Rolld.state.update(digest="d2", value="two")
        self.assertTrue(changed.wait(5))
        threading.Event().wait(0.05)
        self.assertEqual(seen, ["one", "two"], "an unchanged digest delivers nothing again")


if __name__ == "__main__":
    unittest.main()
