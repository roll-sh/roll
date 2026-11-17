import json
import os
import threading
import time
import unittest
import urllib.parse
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


class Writes(BaseHTTPRequestHandler):
    """rolld's one-key writes and a values route with a dynamic key (PW) that dynamic=false leaves
    out; a protected environment answers approval_required."""

    state = {"stored": {"A": "1"}, "generation": 0, "reads": 0, "protected": False, "writes": []}

    def log_message(self, *_):
        pass

    def answer(self, code, body):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("ETag", '"g%d"' % Writes.state["generation"])
        self.end_headers()
        self.wfile.write(json.dumps(body).encode())

    def do_GET(self):
        s, q = Writes.state, urllib.parse.urlsplit(self.path).query
        s["reads"] += 1
        no_dynamic = "dynamic=false" in q
        values = dict(s["stored"]) if no_dynamic else {**s["stored"], "PW": "fake-issued"}
        self.answer(200, {"values": values, "problems": [], "leases": [] if no_dynamic else ["lse_1"], "dynamic": ["PW"], "linked": []})

    def write(self):
        s, u = Writes.state, urllib.parse.urlsplit(self.path)
        assert urllib.parse.parse_qs(u.query) == {"path": ["/api"]}, u.query
        if s["protected"]:
            return None, self.answer(409, {"error": "production is protected: open a change request", "code": "approval_required"})
        s["generation"] += 1
        return u.path.rsplit("/", 1)[1], {"seq": s["generation"], "environment": "production", "path": "/api", "kind": "write"}

    def do_PUT(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        key, change = self.write()
        if key:
            Writes.state["writes"].append(body)
            Writes.state["stored"][key] = body["value"]
            self.answer(200, {"change": change, "secret": {"key": key, "version": 2}, "warnings": []})

    def do_DELETE(self):
        key, change = self.write()
        if key and key not in Writes.state["stored"]:
            self.answer(404, {"error": f"no secret {key} in production:/api"})
        elif key:
            del Writes.state["stored"][key]
            self.answer(200, {"change": change, "secret": None, "warnings": []})


class OneKeyWrites(unittest.TestCase):
    def test_set_and_delete_write_one_key_and_drop_the_cache(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Writes)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        c = roll_secrets.Client(f"http://127.0.0.1:{server.server_address[1]}", "rlt_x", project="prj_1", cache_ttl=3600)
        s = Writes.state

        v = c.values("production", dynamic=False)
        self.assertEqual((v.values, v.dynamic, v.leases), ({"A": "1"}, ["PW"], []), "dynamic=false leaves the dynamic key out")
        c.values("production", dynamic=False)
        self.assertEqual(s["reads"], 1, "and holds no credential, so it's kept")

        w = c.set("production", "B", "fake-value-1", path="/api", comment="made up")
        self.assertEqual((w["secret"]["version"], s["writes"]), (2, [{"value": "fake-value-1", "link": None, "comment": "made up"}]))
        self.assertEqual(c.values("production", dynamic=False).values.get("B"), "fake-value-1", "a set drops the cache")

        self.assertEqual(c.delete("production", "B", path="/api"), {"deleted": True, "change": {"seq": 2, "environment": "production", "path": "/api", "kind": "write"}})
        self.assertNotIn("B", c.values("production", dynamic=False).values, "a delete drops the cache")
        self.assertEqual(c.delete("production", "B", path="/api"), {"deleted": False, "change": None}, "a missing key is no change")

        s["protected"] = True
        with self.assertRaises(roll_secrets.SecretsError) as e:
            c.set("production", "C", "fake-value-2", path="/api")
        self.assertEqual((e.exception.code, e.exception.status), ("approval_required", 409))
        self.assertNotIn("fake-value-2", repr(e.exception) + str(e.exception) + repr(e.exception.__cause__) + repr(e.exception.__context__))


class Reads(BaseHTTPRequestHandler):
    """rolld's one-key reads after a workload identity's login: a key's record (A its own, B a
    link's through the base) and A's value, stored as a reference that resolve=true follows."""

    state = {"logins": 0, "values": 0}
    prefix = "/v1/secrets/projects/prj_a/environments/staging/secrets/"
    records = {
        "A": {"key": "A", "path": "/api", "type": "shared", "version": 3, "comment": "made up", "tags": [], "link": None, "source": {"kind": "own"}, "updated_at": 1, "updated_by": "rlt_x", "expires_at": None},
        "B": {"key": "B", "path": "/api", "type": "shared", "version": 1, "comment": "", "tags": [], "link": {"kind": "postgres", "ref": "pg_1", "field": "url", "mode": "live"}, "source": {"kind": "base", "environment": "development"}, "updated_at": 2, "updated_by": "rlt_x", "expires_at": None},
    }

    def log_message(self, *_):
        pass

    def answer(self, code, body):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(body).encode())

    def do_POST(self):
        b = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if (self.path, b) != ("/v1/secrets/login", {"identity": "idn_ci", "token": "fake-oidc"}):
            return self.answer(401, {"error": "no identity idn_ci takes this token"})
        Reads.state["logins"] += 1
        self.answer(200, {"token": "rlt_reads", "project": "prj_a", "expires_at": int(time.time() * 1000) + 3_600_000})

    def do_GET(self):
        u = urllib.parse.urlsplit(self.path)
        q = urllib.parse.parse_qs(u.query)
        key = u.path[len(Reads.prefix):]
        if self.headers.get("Authorization") != "Bearer rlt_reads" or q.get("path") != ["/api"]:
            return self.answer(401, {"error": "who"})
        if key == "A/value":
            Reads.state["values"] += 1
            return self.answer(200, {"value": "fake-resolved" if q.get("resolve") != ["false"] else "${development.FAKE_URL}", "problems": []})
        if key in Reads.records:
            return self.answer(200, Reads.records[key])
        self.answer(404, {"error": f"no secret {key} in staging:/api"})


class OneKeyReads(unittest.TestCase):
    def test_a_keys_record_and_stored_value_after_a_login_from_a_token_source(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Reads)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        sourced = []
        c = roll_secrets.login_with_identity(f"http://127.0.0.1:{server.server_address[1]}", "idn_ci", lambda: sourced.append(1) or "fake-oidc", cache_ttl=3600)
        self.assertEqual((len(sourced), Reads.state["logins"], c.project), (1, 1, "prj_a"), "the source gives the login its OIDC token")
        self.assertIs(roll_secrets._logged_in, roll_secrets.login_with_identity)

        a = c.secret("staging", "A", path="/api")
        self.assertEqual((a.version, a.comment, a.link_kind, a.source.kind), (3, "made up", None, "own"))
        b = c.secret("staging", "B", path="/api")
        self.assertEqual((b.link_kind, b.source), ("postgres", roll_secrets.Source("base", "development")), "a link shows its kind only")
        self.assertIsNone(c.secret("staging", "NOPE", path="/api"), "a missing key is None")

        self.assertEqual(c.stored_value("staging", "A", path="/api"), "${development.FAKE_URL}", "the reference as written")
        c.stored_value("staging", "A", path="/api")
        self.assertEqual(Reads.state["values"], 2, "never kept, whatever cache_ttl says")
        with self.assertRaises(roll_secrets.SecretsError) as e:
            c.stored_value("staging", "NOPE", path="/api")
        self.assertEqual(e.exception.status, 404)
