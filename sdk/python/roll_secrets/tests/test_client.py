import json
import os
import threading
import time
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


class Printing(unittest.TestCase):
    def test_printing_never_shows_a_value_or_the_token(self):
        v = roll_secrets.Values(values={"DB_PASSWORD": "fake-pw-not-real"})
        c = roll_secrets.Client("https://api.roll.example", "rlt_fake_token_not_real", project="prj_1")
        for s in (repr(v), str(v), repr(c), str(c), f"{v!r} {c!r}"):
            self.assertNotIn("fake-pw-not-real", s)
            self.assertNotIn("rlt_fake_token_not_real", s)
        self.assertIn("DB_PASSWORD", repr(v))


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

    state = {"digest": "d1", "value": "one", "token": "rlt_workload", "logins": 0}

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
            Rolld.state["logins"] += 1
            return self.answer(200, {"token": Rolld.state["token"], "project": "prj_a", "expires_at": int(time.time() * 1000) + 3_600_000})
        if self.headers.get("Authorization") != "Bearer " + Rolld.state["token"]:
            return self.answer(401)
        if self.path == env + "/resolve":
            b = self.body()
            if (b.get("consumer"), b.get("seal_to")) != ("app:web", roll_secrets.public_key(KEY)):
                return self.answer(400)
            return self.answer(200, {"sealed": BOX, "context": CONTEXT, "problems": [], "leases": [], "dynamic": [], "linked": ["DB_URL"]})
        self.answer(404)

    def do_GET(self):
        env = "/v1/secrets/projects/prj_a/environments/production"
        if self.headers.get("Authorization") != "Bearer " + Rolld.state["token"]:
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
        # Kept for the whole test: a new login reads the OIDC token again.
        env = mock.patch.dict(os.environ, {"ROLL_API_URL": api, "ROLL_IDENTITY": "idn_ci", "ROLL_OIDC_TOKEN": "the-oidc-token"}, clear=False)
        env.start()
        self.addCleanup(env.stop)
        c = roll_secrets.from_env()

        sealed = c.resolve_sealed("production", "app:web", KEY)
        self.assertEqual((sealed.values, sealed.linked), ({"DB_URL": "postgres://db/shop"}, ["DB_URL"]))

        seen, stop = [], threading.Event()
        first, changed = threading.Event(), threading.Event()

        relogged = threading.Event()

        def on_change(v):
            seen.append(v.values["A"])
            (first if len(seen) == 1 else changed if len(seen) == 2 else relogged).set()

        watching = threading.Thread(target=c.watch, args=("production", on_change), kwargs={"every": 0.01, "stop": stop})
        watching.start()
        self.addCleanup(watching.join)
        self.addCleanup(stop.set)
        self.assertTrue(first.wait(5))
        Rolld.state.update(digest="d2", value="two")
        self.assertTrue(changed.wait(5))
        threading.Event().wait(0.05)
        self.assertEqual(seen, ["one", "two"], "an unchanged digest delivers nothing again")
        # The roll token stops working (revoked, ended): the client logs in again and keeps watching.
        Rolld.state.update(digest="d3", value="three", token="rlt_workload2")
        self.assertTrue(relogged.wait(5))
        self.assertEqual((seen[-1], Rolld.state["logins"]), ("three", 2))


if __name__ == "__main__":
    unittest.main()


class Cached(BaseHTTPRequestHandler):
    """rolld's values route with entity tags: a 304 when the digest still matches."""

    state = {"digest": "d1", "value": "1", "dynamic": [], "asked": []}

    def log_message(self, *_):
        pass

    def do_GET(self):
        tag = self.headers.get("If-None-Match")
        Cached.state["asked"].append(tag)
        assert "digest=1" in self.path
        digest = Cached.state["digest"]
        if tag == f'"{digest}"':
            self.send_response(304)
            self.send_header("ETag", f'"{digest}"')
            self.end_headers()
            return
        body = json.dumps({"values": {"A": Cached.state["value"]}, "problems": [], "leases": [], "dynamic": Cached.state["dynamic"], "linked": []}).encode()
        self.send_response(200)
        self.send_header("ETag", f'"{digest}"')
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)


class ACache(unittest.TestCase):
    def test_values_are_kept_revalidated_and_never_kept_with_dynamic_credentials(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Cached)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        clock = [0.0]
        c = roll_secrets.Client(f"http://127.0.0.1:{server.server_address[1]}", "rlt_x", project="prj_1", cache_ttl=20, clock=lambda: clock[0])
        asked = Cached.state["asked"]
        self.assertEqual((c.values("production").values["A"], c.values("production").values["A"], len(asked)), ("1", "1", 1))
        clock[0] += 25
        self.assertEqual(c.values("production").values["A"], "1")
        self.assertEqual(asked[1:], ['"d1"'], "revalidated: a 304 keeps them")
        clock[0] += 25
        Cached.state.update(digest="d2", value="2", dynamic=["PW"])
        self.assertEqual((c.values("production").values["A"], c.values("production").values["A"], len(asked)), ("2", "2", 4))


class Echo(BaseHTTPRequestHandler):
    """A transit endpoint that "encrypts" by echoing: enough to check the encoding both ways."""

    sent = []

    def log_message(self, *_):
        pass

    def do_POST(self):
        b = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)))
        Echo.sent.append({"path": self.path, **b})
        out = {"ciphertext": "roll:v1:" + b["plaintext"]} if self.path.endswith("/encrypt") else {"plaintext": b["ciphertext"][len("roll:v1:"):]}
        body = json.dumps(out).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)


class ATransitKey(unittest.TestCase):
    def test_sends_base64_and_decrypts_back_to_the_same_bytes(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Echo)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        cards = roll_secrets.Client(f"http://127.0.0.1:{server.server_address[1]}", "rlt_x", project="prj_1").transit("cards")
        ct = cards.encrypt("héllo ✓", "order:42")
        self.assertEqual(Echo.sent[0], {"path": "/v1/secrets/projects/prj_1/transit/cards/encrypt", "plaintext": "aMOpbGxvIOKckw==", "context": "order:42"})
        self.assertEqual(cards.decrypt(ct, "order:42").decode(), "héllo ✓")
        self.assertEqual(cards.decrypt(cards.encrypt(bytes([0, 255, 128, 7]))), bytes([0, 255, 128, 7]))
