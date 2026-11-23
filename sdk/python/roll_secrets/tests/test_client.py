import base64
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


class Totp(unittest.TestCase):
    def test_codes_match_rfc_6238(self):
        # RFC 6238, Appendix B: each algorithm's own test seed, 8 digits, 30 s steps.
        seeds = {"SHA1": b"12345678901234567890", "SHA256": b"12345678901234567890123456789012", "SHA512": b"1234567890" * 6 + b"1234"}
        vectors = [(59, ("94287082", "46119246", "90693936")), (1111111109, ("07081804", "68084774", "25091201")), (1234567890, ("89005924", "91819424", "93441116")), (20000000000, ("65353130", "77737706", "47863826"))]
        for i, (alg, seed) in enumerate(seeds.items()):
            uri = f"otpauth://totp/roll:test?secret={base64.b32encode(seed).decode()}&algorithm={alg}&digits=8"
            for at, codes in vectors:
                self.assertEqual(roll_secrets.totp_at(uri, at).code, codes[i], f"{alg} at {at}")
        # Bare base32: SHA1, 6 digits, 30 s.
        self.assertEqual(roll_secrets.totp_at("gezd gnbv gy3t qojq gezd gnbv gy3t qojq", 59), roll_secrets.Totp("287082", 60_000, 30))
        with self.assertRaises(ValueError) as e:
            roll_secrets.totp_at("otpauth://totp/x?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&digits=9")
        self.assertNotIn("GEZD", str(e.exception))
        self.assertNotIn("287082", repr(roll_secrets.totp_at("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ", 59)))


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
        if self.path.split("?")[0] == env + "/digest":
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


class Login(BaseHTTPRequestHandler):
    """roll's login route: keeps what it was sent."""

    sent: list = []

    def log_message(self, *_):
        pass

    def do_POST(self):
        Login.sent.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
        body = json.dumps({"token": "rlt_aws", "project": "prj_shop", "expires_at": 0}).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class AnAWSIdentity(unittest.TestCase):
    def test_signs_as_aws_published_example(self):
        # The IAM ListUsers request in AWS's "Examples of the complete Signature Version 4 signing
        # process", with its documented example keys.
        creds = {"key": "AKIDEXAMPLE", "secret": "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY"}
        headers = {"content-type": "application/x-www-form-urlencoded; charset=utf-8", "host": "iam.amazonaws.com", "x-amz-date": "20150830T123600Z"}
        auth = roll_secrets._sigv4(creds, "us-east-1", "iam", "GET", "/", "Action=ListUsers&Version=2010-05-08", headers, "", "20150830T123600Z")
        self.assertEqual(auth, "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7")

    def test_logs_in_with_a_signed_get_caller_identity(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Login)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        api = f"http://127.0.0.1:{server.server_address[1]}"
        env = {"ROLL_API_URL": api, "ROLL_IDENTITY": "idn_web", "AWS_ACCESS_KEY_ID": "AKIAIOSFODNN7EXAMPLE", "AWS_SECRET_ACCESS_KEY": "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "AWS_SESSION_TOKEN": "EXAMPLETOKEN", "AWS_REGION": "eu-west-1"}
        unset = ["ROLL_TOKEN", "ROLL_OIDC_TOKEN", "ROLL_OIDC_TOKEN_FILE", "ACTIONS_ID_TOKEN_REQUEST_URL", "ROLL_IDENTITY_KIND", "ROLL_AWS_SERVER_ID"]
        with mock.patch.dict(os.environ, {**env, **{k: "" for k in unset}}, clear=False):
            c = roll_secrets.from_env()
        self.assertEqual(c.token, "rlt_aws")
        sent = Login.sent[-1]
        aws = sent["aws"]
        self.assertEqual((sent["identity"], aws["url"], aws["body"]), ("idn_web", "https://sts.eu-west-1.amazonaws.com/", "Action=GetCallerIdentity&Version=2011-06-15"))
        self.assertEqual((aws["headers"]["x-roll-server-id"], aws["headers"]["x-amz-security-token"]), (api, "EXAMPLETOKEN"))
        self.assertIn("/eu-west-1/sts/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-roll-server-id, ", aws["headers"]["authorization"])


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


class Delta(BaseHTTPRequestHandler):
    """rolld's delta route as a watcher sees it: each key's stamp is its version, the basis is
    fixed, and a claim with it gets only the keys whose stamp moved; ``route`` off is an older
    rolld (a 404)."""

    state = {"values": {"A": "1", "B": "2"}, "versions": {"A": 1, "B": 1}, "route": True, "deltas": [], "full": 0}

    def log_message(self, *_):
        pass

    def answer(self, code, body):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps(body).encode())

    def digest(self):
        return "d" + "".join(f"{k}{v}" for k, v in sorted(Delta.state["versions"].items()))

    def do_GET(self):
        u = urllib.parse.urlsplit(self.path)
        if u.path.endswith("/digest"):
            return self.answer(200, {"digest": self.digest()})
        Delta.state["full"] += 1
        self.answer(200, {"values": dict(Delta.state["values"]), "problems": [], "leases": [], "dynamic": [], "linked": []})

    def do_POST(self):
        s = Delta.state
        claim = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if not s["route"] or not self.path.endswith("/values/delta"):
            return self.answer(404, {"error": "no route"})
        stamps = {k: str(v) for k, v in s["versions"].items()}
        held = claim.get("stamps") or {}
        delta = claim.get("basis") == "b1"
        keys = [k for k in s["values"] if not delta or held.get(k) != stamps[k]]
        removed = [k for k in held if k not in s["values"]] if delta else []
        s["deltas"].append(sorted(keys))
        self.answer(200, {"delta": delta, "digest": self.digest(), "basis": "b1", "values": {k: s["values"][k] for k in keys}, "removed": removed, "stamps": {k: stamps[k] for k in keys}, "problems": [], "leases": [], "dynamic": [], "linked": []})


class AWatchDelta(unittest.TestCase):
    def test_a_change_reads_through_the_delta_and_an_older_rolld_reads_in_full(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Delta)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.shutdown)
        c = roll_secrets.Client(f"http://127.0.0.1:{server.server_address[1]}", "rlt_x", project="prj_1")
        s, seen, stop = Delta.state, [], threading.Event()
        got = [threading.Event() for _ in range(3)]

        def on_change(v):
            seen.append(dict(v.values))
            got[len(seen) - 1].set()

        watching = threading.Thread(target=c.watch, args=("production", on_change), kwargs={"every": 0.01, "stop": stop})
        watching.start()
        self.addCleanup(watching.join)
        self.addCleanup(stop.set)
        self.assertTrue(got[0].wait(5))
        s["values"]["B"], s["versions"]["B"] = "3", 2
        self.assertTrue(got[1].wait(5))
        self.assertEqual((seen[1], s["deltas"], s["full"]), ({"A": "1", "B": "3"}, [["A", "B"], ["B"]], 0), "only B came back, merged into what the watch held")

        s["route"] = False
        s["values"]["A"], s["versions"]["A"] = "9", 2
        self.assertTrue(got[2].wait(5))
        self.assertEqual((seen[2], s["full"]), ({"A": "9", "B": "3"}, 1), "a 404 reads in full")
