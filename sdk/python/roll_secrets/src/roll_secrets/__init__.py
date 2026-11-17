"""A client for roll secrets (docs/secrets.md, "Consumption"), with the same surface as
@roll/secrets and sdk/go/secrets: an environment's values, the runtime resolve (sealed to a key only
this process holds, if asked), the digest that moves when the values would, and a watch that follows
it and renews dynamic leases.

The standard library only, but for sealed resolves, which need ``cryptography`` (the ``seal``
extra).

A client holds a roll token (``rlt_…``, roll-access's) with a rule on one project, the operator
token, or the short-lived token a workload identity's login gives (``login``, ``from_env``).
"""

from __future__ import annotations

import base64
import copy
import json
import os
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, List, Optional

__all__ = ["Client", "Values", "Problem", "SecretsError", "login", "from_env", "generate_key", "public_key", "open_sealed"]


class SecretsError(Exception):
    """An answer that wasn't a success: its status, roll's code and message. Never a value."""

    def __init__(self, message: str, status: int, code: Optional[str] = None):
        super().__init__(message)
        self.status = status
        self.code = code


@dataclass
class Problem:
    key: str
    problem: str


@dataclass
class Followed:
    """A key left out because it links a resource the environment's preview branched: roll
    previews sets ``name`` from its branch's ``field``."""

    name: str
    kind: str
    parent: str
    field: str


@dataclass
class Values:
    """An environment's values. ``dynamic`` keys hold a credential that ends with its lease
    (``leases``; ``renew`` keeps it); ``linked`` keys come from a link to a resource; ``followed``
    keys link a resource a preview branched, and aren't in ``values``."""

    values: Dict[str, str]
    problems: List[Problem] = field(default_factory=list)
    leases: List[str] = field(default_factory=list)
    dynamic: List[str] = field(default_factory=list)
    linked: List[str] = field(default_factory=list)
    followed: List[Followed] = field(default_factory=list)

    @staticmethod
    def _of(d: Dict[str, Any], values: Optional[Dict[str, str]] = None) -> "Values":
        return Values(
            values=values if values is not None else dict(d.get("values") or {}),
            problems=[Problem(p["key"], p["problem"]) for p in d.get("problems") or []],
            leases=list(d.get("leases") or []),
            dynamic=list(d.get("dynamic") or []),
            linked=list(d.get("linked") or []),
            followed=[Followed(f["name"], f["kind"], f["parent"], f["field"]) for f in d.get("followed") or []],
        )


def _q(path: str) -> str:
    return "?path=" + urllib.parse.quote(path, safe="") if path else ""


class Client:
    """Reads one project's secrets."""

    def __init__(self, api: str, token: str, project: Optional[str] = None, timeout: float = 30.0, cache_ttl: float = 0.0, clock: Callable[[], float] = time.monotonic, cache_size: int = 0):
        """``cache_ttl`` (seconds) keeps ``values`` answers that long and answers from memory
        meanwhile; after that it asks again with the digest they were read against, and an
        unchanged folder costs a 304 that reads, issues and audits nothing. A folder holding dynamic
        credentials is never kept (they end with their lease). ``cache_size`` caps how many
        folders are kept (past it, the one read longest ago goes; 0: no cap). ``clock`` is for
        tests."""
        self.api = api.rstrip("/")
        self.token = token
        self.project = project
        self.timeout = timeout
        self.cache_ttl = cache_ttl
        self.cache_size = cache_size
        self._clock = clock
        self._cache: Dict[str, Any] = {}
        self._lock = threading.Lock()
        # Renews the token of a client a workload identity's login made.
        self._login: Optional[_Relogin] = None

    def _call(self, method: str, path: str, body: Any = None) -> Any:
        return self._authed(lambda token: self._send(token, method, path, body))

    def _authed(self, do: Callable[[str], Any]) -> Any:
        """``do`` with the client's token. A logged-in client's is renewed first when it's ending, and
        when the server refuses it (revoked, ended early): one new login, then once more."""
        if self._login is None:
            return do(self.token)
        token = self._login.token(self)
        try:
            return do(token)
        except SecretsError as e:
            if e.status != 401:
                raise
            return do(self._login.token(self, stale=token))

    def _send(self, token: str, method: str, path: str, body: Any = None) -> Any:
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.api + path, data=data, method=method)
        if token:
            req.add_header("Authorization", "Bearer " + token)
        if data is not None:
            req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as res:
                text = res.read(16 << 20)
        except urllib.error.HTTPError as e:
            try:
                err = json.loads(e.read(1 << 16) or b"{}")
            except ValueError:
                err = {}
            raise SecretsError(err.get("error") or f"{e.code} {e.reason}", e.code, err.get("code")) from None
        return json.loads(text) if text else None

    def project_id(self) -> str:
        """The project: as given, or the token's own (GET /v1/secrets/whoami)."""
        with self._lock:
            if not self.project:
                who = self._call("GET", "/v1/secrets/whoami") or {}
                if not who.get("project"):
                    raise SecretsError("this token isn't limited to one project: pass project", 400, "project")
                self.project = who["project"]
            return self.project

    def _env(self, env: str) -> str:
        return f"/v1/secrets/projects/{urllib.parse.quote(self.project_id(), safe='')}/environments/{urllib.parse.quote(env, safe='')}"

    def values(self, env: str, path: str = "", fresh: bool = False) -> Values:
        """``env``'s values under ``path`` (the root by default), references resolved. ``fresh``
        skips ``cache_ttl`` (a watch that saw the digest move reads anew)."""
        url = self._env(env) + "/values" + _q(path)
        if self.cache_ttl <= 0 or fresh:
            v = Values._of(self._call("GET", url))
            if fresh:
                with self._lock:
                    self._cache.pop(url, None)
            return v
        return copy.deepcopy(self._cached(url))

    def _cached(self, url: str) -> Values:
        """Kept for ``cache_ttl``, then revalidated with the digest (a 304 keeps them)."""
        with self._lock:
            hit = self._cache.get(url)
        if hit and self._clock() - hit[2] < self.cache_ttl:
            return hit[0]
        return self._authed(lambda token: self._revalidate(url, token, hit))

    def _revalidate(self, url: str, token: str, hit: Any) -> Values:
        """One conditional read of ``url`` with ``token``: a 304 keeps ``hit``'s values."""
        req = urllib.request.Request(self.api + url + ("&" if "?" in url else "?") + "digest=1", method="GET")
        if token:
            req.add_header("Authorization", "Bearer " + token)
        if hit and hit[1]:
            req.add_header("If-None-Match", '"' + hit[1] + '"')
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as res:
                text, etag = res.read(16 << 20), res.headers.get("ETag") or ""
        except urllib.error.HTTPError as e:
            if e.code == 304 and hit:
                with self._lock:
                    self._cache[url] = (hit[0], hit[1], self._clock())
                return hit[0]
            try:
                err = json.loads(e.read(1 << 16) or b"{}")
            except ValueError:
                err = {}
            raise SecretsError(err.get("error") or f"{e.code} {e.reason}", e.code, err.get("code")) from None
        v = Values._of(json.loads(text))
        with self._lock:
            # Never kept: dynamic credentials (they end with their lease) or a key that didn't
            # resolve (the digest may not move when the link recovers).
            if v.dynamic or v.problems:
                self._cache.pop(url, None)
            else:
                if url not in self._cache and 0 < self.cache_size <= len(self._cache):
                    del self._cache[min(self._cache, key=lambda k: self._cache[k][2])]
                self._cache[url] = (v, etag.removeprefix("W/").strip('"'), self._clock())
        return v

    def resolve(self, env: str, consumer: str, path: str = "") -> Values:
        """``env``'s values as ``consumer`` (``app:web``), what a running service gets: dynamic
        credentials' leases are the consumer's, and the read is the consumer's in the audit log."""
        return Values._of(self._call("POST", self._env(env) + "/resolve", {"consumer": consumer, "path": path or "/"}))

    def resolve_sealed(self, env: str, consumer: str, key: Any, path: str = "") -> Values:
        """``resolve`` with the values sealed to ``key`` (``generate_key``) on the server and opened
        here: they are never in the clear between the two."""
        r = self._call("POST", self._env(env) + "/resolve", {"consumer": consumer, "path": path or "/", "seal_to": public_key(key)})
        opened = json.loads(open_sealed(r["sealed"], key, r["context"]))
        return Values._of(r, values=opened)

    def digest(self, env: str, path: str = "") -> str:
        """Moves whenever ``env``'s values under ``path`` would; reads no value."""
        return self._call("GET", self._env(env) + "/digest" + _q(path))["digest"]

    def transit(self, name: str) -> "Transit":
        """The project's encryption key ``name`` (docs/secrets.md, "Encryption keys"): data is
        encrypted, signed and HMACed by roll, and the key never leaves it."""
        return Transit(self, name)

    def renew(self, lease: str, ttl_minutes: Optional[int] = None) -> Dict[str, Any]:
        """Pushes a dynamic lease's expiry out by ``ttl_minutes`` (its link's TTL by default)."""
        body = {"ttl_minutes": ttl_minutes} if ttl_minutes else {}
        return self._call("POST", f"/v1/secrets/projects/{urllib.parse.quote(self.project_id(), safe='')}/leases/{urllib.parse.quote(lease, safe='')}/renew", body)

    def watch(
        self,
        env: str,
        on_change: Callable[[Values], None],
        path: str = "",
        consumer: Optional[str] = None,
        every: float = 5.0,
        renew_every: float = 600.0,
        on_error: Optional[Callable[[Exception], None]] = None,
        stop: Optional[threading.Event] = None,
    ) -> None:
        """Calls ``on_change`` with the values now, then again whenever the digest moves, renewing
        the latest values' dynamic leases meanwhile (as ``roll run --watch`` does), until ``stop``
        is set. Values that don't resolve never reach ``on_change``: what's running keeps the
        previous ones. Errors go to ``on_error`` and the watch goes on."""
        stop = stop or threading.Event()
        fail = on_error or (lambda _e: None)
        last: Optional[str] = None
        leases: List[str] = []
        renewed = 0.0
        waited = 0.0
        while not stop.is_set():
            try:
                now = self.digest(env, path)
                if now != last:
                    v = self.resolve(env, consumer, path) if consumer else self.values(env, path, fresh=True)
                    before, last = last, now
                    if v.problems:
                        if before is None:
                            last = None  # nothing running yet: try again next time
                        p = v.problems[0]
                        raise SecretsError(f"{env} doesn't resolve: {p.key}: {p.problem}", 409, "unresolved")
                    leases = v.leases
                    on_change(v)
            except Exception as e:  # noqa: BLE001 - the watch goes on, as documented
                fail(e)
            if stop.wait(every):
                break
            waited += every
            if waited - renewed >= renew_every:
                renewed = waited
                for lease in leases:
                    try:
                        self.renew(lease)
                    except Exception as e:  # noqa: BLE001
                        fail(e)


class Transit:
    """One encryption key, as ``@roll/secrets``' ``transit(name)``: bytes or text in, base64 on the
    wire, bytes back from ``decrypt``."""

    def __init__(self, client: Client, name: str):
        self._client, self.name = client, name

    def _at(self, op: str) -> str:
        return f"/v1/secrets/projects/{urllib.parse.quote(self._client.project_id(), safe='')}/transit/{urllib.parse.quote(self.name, safe='')}/{op}"

    @staticmethod
    def _bytes(data: Any) -> bytes:
        return data.encode() if isinstance(data, str) else bytes(data)

    def encrypt(self, data: Any, context: Optional[str] = None) -> str:
        """The ciphertext (``roll:v<n>:…``); ``context``, when given, must be given to decrypt it."""
        return self._client._call("POST", self._at("encrypt"), {"plaintext": base64.b64encode(self._bytes(data)).decode(), "context": context})["ciphertext"]

    def decrypt(self, ciphertext: str, context: Optional[str] = None) -> bytes:
        """The plaintext (audited, as a reveal)."""
        return base64.b64decode(self._client._call("POST", self._at("decrypt"), {"ciphertext": ciphertext, "context": context})["plaintext"])

    def sign(self, data: Any) -> str:
        """An Ed25519 signature over the bytes as given: anyone with ``public_keys`` checks it."""
        return self._client._call("POST", self._at("sign"), {"input": base64.b64encode(self._bytes(data)).decode()})["signature"]

    def verify(self, data: Any, signature: str) -> bool:
        return bool(self._client._call("POST", self._at("verify"), {"input": base64.b64encode(self._bytes(data)).decode(), "signature": signature})["valid"])

    def hmac(self, data: Any) -> str:
        """HMAC-SHA256, the same for the same input: a lookup column over encrypted fields."""
        return self._client._call("POST", self._at("hmac"), {"input": base64.b64encode(self._bytes(data)).decode()})["hmac"]

    def public_keys(self) -> List[Dict[str, Any]]:
        """An Ed25519 key's public halves, every version (``public_key``, ``pem``, ``jwk``)."""
        return self._client._call("GET", self._at("public"))


def login(api: str, identity: str, oidc_token: str) -> Client:
    """Exchanges a workload's own OIDC token for a short-lived roll token through workload identity
    (docs/secrets.md, "Workload identities"): nothing long-lived in the workload's settings. The
    client logs in again with the same OIDC token before its roll token ends (``from_env`` reads a
    fresh OIDC token each time)."""
    return _logged_in(api, identity, lambda: oidc_token)


def _logged_in(api: str, identity: str, source: Callable[[], str]) -> Client:
    c = Client(api, "")
    c._login = _Relogin(identity, source)
    c._login.token(c, stale="")
    return c


class _Relogin:
    """Keeps a logged-in client's token fresh: a new login once less than a fifth of its life (at
    least a minute) is left, or when the server refuses it."""

    def __init__(self, identity: str, source: Callable[[], str]):
        self.identity = identity
        self.source = source
        self.lock = threading.Lock()
        self.issued = self.expires = 0.0

    def token(self, c: Client, stale: Optional[str] = None) -> str:
        with self.lock:
            margin = max(60.0, (self.expires - self.issued) / 5)
            # An answer without an expiry is renewed only when the server refuses it.
            ending = self.expires > 0 and time.time() > self.expires - margin
            refused = stale == c.token
            if refused or ending:
                try:
                    r = Client(c.api, "", timeout=c.timeout)._send("", "POST", "/v1/secrets/login", {"identity": self.identity, "token": self.source().strip()})
                except Exception:
                    # Renewing early that fails keeps the token it has until it really ends.
                    if refused or time.time() >= self.expires:
                        raise
                    return c.token
                if not r or not r.get("token"):
                    raise SecretsError("the login answered no token", 502)
                c.token, self.issued, self.expires = r["token"], time.time(), (r.get("expires_at") or 0) / 1000
                if not c.project:
                    c.project = r.get("project")
            return c.token


def from_env() -> Client:
    """A client as ``roll run`` makes one: ROLL_API_URL, then ROLL_TOKEN, or ROLL_IDENTITY with the
    workload's OIDC token from ROLL_OIDC_TOKEN, ROLL_OIDC_TOKEN_FILE (a Kubernetes projected token)
    or GitHub Actions (``permissions: id-token: write``; audience ROLL_OIDC_AUDIENCE, default the
    API's URL). ROLL_PROJECT names the project when the token doesn't."""
    env = lambda k: (os.environ.get(k) or "").strip()  # noqa: E731
    api = env("ROLL_API_URL")
    if not api:
        raise SecretsError("ROLL_API_URL isn't set", 400, "config")
    if env("ROLL_TOKEN"):
        c = Client(api, env("ROLL_TOKEN"))
    elif env("ROLL_IDENTITY"):
        # The OIDC token is read again for each login (a projected token is replaced on disk).
        c = _logged_in(api, env("ROLL_IDENTITY"), lambda: _oidc_token(api, env))
    else:
        raise SecretsError("set ROLL_TOKEN, or ROLL_IDENTITY for a workload identity", 400, "config")
    if env("ROLL_PROJECT"):
        c.project = env("ROLL_PROJECT")
    return c


def _oidc_token(api: str, env: Callable[[str], str]) -> str:
    if env("ROLL_OIDC_TOKEN"):
        return env("ROLL_OIDC_TOKEN")
    if env("ROLL_OIDC_TOKEN_FILE"):
        with open(env("ROLL_OIDC_TOKEN_FILE"), encoding="utf-8") as f:
            return f.read()
    url, bearer = env("ACTIONS_ID_TOKEN_REQUEST_URL"), env("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
    if not url or not bearer:
        raise SecretsError("ROLL_IDENTITY is set but there's no OIDC token: set ROLL_OIDC_TOKEN or ROLL_OIDC_TOKEN_FILE, or run in GitHub Actions with `permissions: id-token: write`", 400, "config")
    # The server's own URL by default: a token minted for one roll server isn't good at another.
    audience = env("ROLL_OIDC_AUDIENCE") or api.rstrip("/")
    req = urllib.request.Request(url + ("&" if "?" in url else "?") + "audience=" + urllib.parse.quote(audience, safe=""))
    req.add_header("Authorization", "Bearer " + bearer)
    try:
        with urllib.request.urlopen(req, timeout=30) as res:
            value = json.loads(res.read(64 << 10)).get("value")
    except (urllib.error.URLError, ValueError):
        value = None
    if not value:
        raise SecretsError("GitHub Actions didn't give its OIDC token (does the job have `permissions: id-token: write`?)", 502, "oidc")
    return value


# roll-seal-v1 boxes, as crates/roll-secrets/src/crypto.rs makes them: X25519 between a fresh key
# pair and the recipient's key, HKDF-SHA256 with salt epk ‖ recipient and info "roll-seal-v1" into
# an AES-256-GCM key, a random 12-byte nonce, and the context as the associated data.


def _b64(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def _unb64(s: str) -> bytes:
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def generate_key() -> Any:
    """An X25519 key a sealed resolve is sealed to; keep it in the process."""
    from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PrivateKey

    return X25519PrivateKey.generate()


def public_key(key: Any) -> str:
    """The base64url public key of ``key``: what ``resolve_sealed`` sends."""
    from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

    return _b64(key.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw))


def open_sealed(box: Dict[str, Any], key: Any, context: str) -> bytes:
    """Opens a box sealed to ``key`` for ``context``; raises ValueError when it doesn't open."""
    from cryptography.hazmat.primitives import hashes
    from cryptography.hazmat.primitives.asymmetric.x25519 import X25519PublicKey
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    from cryptography.hazmat.primitives.kdf.hkdf import HKDF
    from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

    try:
        epk, nonce, ct = _unb64(box["epk"]), _unb64(box["nonce"]), _unb64(box["ct"])
        if box.get("v") != 1 or len(epk) != 32 or len(nonce) != 12:
            raise ValueError
        shared = key.exchange(X25519PublicKey.from_public_bytes(epk))
        if not any(shared):
            raise ValueError
        recipient = key.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
        k = HKDF(algorithm=hashes.SHA256(), length=32, salt=epk + recipient, info=b"roll-seal-v1").derive(shared)
        return AESGCM(k).decrypt(nonce, ct, context.encode())
    except Exception:  # noqa: BLE001 - any failure is the same answer
        raise ValueError("the box doesn't open") from None
