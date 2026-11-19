"""A client for roll secrets (docs/secrets.md, "Consumption"), with the same surface as
@roll/secrets and sdk/go/secrets: an environment's values, the runtime resolve (sealed to a key only
this process holds, if asked), the digest that moves when the values would, a watch that follows
it and renews dynamic leases, one key's record and stored value, and one key's writes.

The standard library only, but for sealed resolves, which need ``cryptography`` (the ``seal``
extra).

A client holds a roll token (``rlt_…``, roll-access's) with a rule on one project, the operator
token, or the short-lived token a workload identity's login gives (``login``,
``login_with_identity``, ``from_env``).
"""

from __future__ import annotations

import base64
import copy
import hashlib
import hmac as _hmac
import json
import os
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Any, Callable, Dict, List, Optional

__all__ = [
    "Client",
    "Values",
    "Problem",
    "SecretInfo",
    "Source",
    "SecretsError",
    "Totp",
    "totp_at",
    "login",
    "login_with_identity",
    "login_aws",
    "aws_signed_request",
    "oidc_token_from_env",
    "from_env",
    "generate_key",
    "public_key",
    "open_sealed",
]


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


@dataclass(repr=False)
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

    def __repr__(self) -> str:
        # Never a value: a logged or printed Values names its keys only.
        return f"Values(keys={sorted(self.values)!r}, problems={len(self.problems)}, leases={len(self.leases)})"

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


@dataclass
class Source:
    """Where a key's record comes from: ``own`` (set in the folder itself), ``base`` (the
    environment's base: its ``environment``) or ``import`` (an imported folder: its
    ``environment`` and ``path``, and ``project`` when another project shares it)."""

    kind: str
    environment: Optional[str] = None
    path: Optional[str] = None
    project: Optional[str] = None


@dataclass
class SecretInfo:
    """One key's record as a folder sees it, never its value. ``link_kind`` is the kind of the link
    the value comes from (``postgres``, ``github``…), None for a value stored in roll."""

    key: str
    path: str
    type: str
    version: int
    comment: str
    tags: List[str]
    link_kind: Optional[str]
    source: Source
    updated_at: int
    updated_by: str
    expires_at: Optional[int] = None

    @staticmethod
    def _of(d: Dict[str, Any]) -> "SecretInfo":
        src, link = d.get("source") or {}, d.get("link") or None
        return SecretInfo(
            key=d.get("key") or "",
            path=d.get("path") or "/",
            type=d.get("type") or "shared",
            version=int(d.get("version") or 0),
            comment=d.get("comment") or "",
            tags=list(d.get("tags") or []),
            link_kind=link.get("kind") if link else None,
            source=Source(src.get("kind") or "own", src.get("environment"), src.get("path"), src.get("project")),
            updated_at=int(d.get("updated_at") or 0),
            updated_by=d.get("updated_by") or "",
            expires_at=d.get("expires_at"),
        )


def _q(path: str) -> str:
    return "?path=" + urllib.parse.quote(path, safe="") if path else ""


class Client:
    """Reads one project's secrets."""

    def __repr__(self) -> str:
        # Never the token.
        return f"Client(api={self.api!r}, project={self.project!r})"

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

    def values(self, env: str, path: str = "", fresh: bool = False, dynamic: bool = True) -> Values:
        """``env``'s values under ``path`` (the root by default), references resolved. ``fresh``
        skips ``cache_ttl`` (a watch that saw the digest move reads anew). ``dynamic=False`` issues
        no dynamic credential (for a reader that can't renew a lease): those keys are left out and
        named in ``Values.dynamic``, and such an answer can be kept."""
        url = self._env(env) + "/values" + _q(path)
        if not dynamic:
            url += ("&" if "?" in url else "?") + "dynamic=false"
        if self.cache_ttl <= 0 or fresh:
            v = Values._of(self._call("GET", url))
            if fresh:
                with self._lock:
                    self._cache.pop(url, None)
            return v
        return copy.deepcopy(self._cached(url, keep_dynamic=not dynamic))

    def _cached(self, url: str, keep_dynamic: bool = False) -> Values:
        """Kept for ``cache_ttl``, then revalidated with the digest (a 304 keeps them).
        ``keep_dynamic``: read with ``dynamic=false``, so naming dynamic keys holds no credential."""
        with self._lock:
            hit = self._cache.get(url)
        if hit and self._clock() - hit[2] < self.cache_ttl:
            return hit[0]
        return self._authed(lambda token: self._revalidate(url, token, hit, keep_dynamic))

    def _secret(self, env: str, key: str, path: str) -> str:
        return self._env(env) + "/secrets/" + urllib.parse.quote(key, safe="") + _q(path)

    def secret(self, env: str, key: str, path: str = "") -> Optional[SecretInfo]:
        """``key``'s record as ``path``'s folder (the root by default) sees it, without its value:
        its version, comment, link kind and where it's set (``source``: its own, a base's or an
        import's). None when the folder has no such key."""
        try:
            return SecretInfo._of(self._call("GET", self._secret(env, key, path)) or {})
        except SecretsError as e:
            if e.status != 404:
                raise
            return None

    def stored_value(self, env: str, key: str, path: str = "") -> str:
        """``key``'s value as stored, references left as written (``resolve=false``). A reveal: the
        token needs ``reveal``, the read is audited, and it's never kept by ``cache_ttl``. A linked
        key's value still comes from its link. A key that doesn't read (sensitive, a link that
        failed) is a ``SecretsError`` with the code ``unresolved``; errors never hold the value."""
        url = self._env(env) + "/secrets/" + urllib.parse.quote(key, safe="") + "/value" + (_q(path) or "?")
        r = self._call("GET", url + ("" if url.endswith("?") else "&") + "resolve=false") or {}
        problems = r.get("problems") or []
        if problems:
            raise SecretsError(f"{key} doesn't read: {problems[0].get('problem')}", 409, "unresolved")
        value = r.get("value")
        if not isinstance(value, str):
            raise SecretsError(f"the answer for {key} has no value", 502)
        return value

    def set(self, env: str, key: str, value: str, path: str = "", comment: Optional[str] = None, message: Optional[str] = None) -> Dict[str, Any]:
        """Sets ``key`` to ``value`` in ``path``'s folder (the root by default), replacing a link the
        key had. Answers ``{change, secret, warnings}``: the change (None when the same value writes
        nothing) and the key's new version. A protected environment, or a folder that needs
        approval, is a ``SecretsError`` with the code ``approval_required`` (open a change request
        instead). Errors never hold the value. Drops the cached reads."""
        body: Dict[str, Any] = {"value": value, "link": None}
        if comment is not None:
            body["comment"] = comment
        if message:
            body["message"] = message
        try:
            return self._call("PUT", self._secret(env, key, path), body) or {}
        finally:
            self._drop_cache()

    def delete(self, env: str, key: str, path: str = "") -> Dict[str, Any]:
        """Deletes ``key`` from ``path``'s folder. Answers ``{deleted, change}``: a key that isn't
        there (or only shows through a base or an import) is ``deleted: False``, not an error;
        ``approval_required`` as for ``set``. Drops the cached reads."""
        try:
            w = self._call("DELETE", self._secret(env, key, path)) or {}
        except SecretsError as e:
            if e.status != 404:
                raise
            return {"deleted": False, "change": None}
        finally:
            self._drop_cache()
        return {"deleted": True, "change": w.get("change")}

    def _drop_cache(self) -> None:
        # Every kept read: a reference or an import elsewhere may follow the key.
        with self._lock:
            self._cache.clear()

    def _revalidate(self, url: str, token: str, hit: Any, keep_dynamic: bool = False) -> Values:
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
            # Never kept: dynamic credentials (they end with their lease; with dynamic=false there
            # are none, only their names) or a key that didn't resolve (the digest may not move
            # when the link recovers).
            if (v.dynamic and not keep_dynamic) or v.problems:
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

    def digest(self, env: str, path: str = "", since: Optional[str] = None, wait: float = 0) -> str:
        """Moves whenever ``env``'s values under ``path`` would; reads no value. With ``since`` and
        ``wait`` (seconds, at most 60), a long poll: it answers as soon as the digest differs from
        ``since``, or with the same one when ``wait`` runs out."""
        q = _q(path)
        if since and wait >= 1:
            # Under this client's own timeout, and the server's 25.
            q += ("&" if q else "?") + "since=" + urllib.parse.quote(since, safe="") + "&wait=" + str(int(max(1, min(wait, 25, self.timeout - 5))))
        return self._call("GET", self._env(env) + "/digest" + q)["digest"]

    def totp(self, env: str, key: str, path: str = "") -> "Totp":
        """The current one-time code of ``env``'s TOTP key ``key`` (docs/secrets.md, "TOTP keys").
        It needs reveal, as reading the seed does, and is audited without the code."""
        d = self._call("GET", self._env(env) + "/secrets/" + urllib.parse.quote(key, safe="") + "/totp" + _q(path))
        return Totp(code=d["code"], expires_at=d["expires_at"], period=d["period"])

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
        renewed = time.monotonic()
        while not stop.is_set():
            # A long poll: a change arrives within a second or two; ``every`` paces a server
            # without them. It never outlasts the next renewal.
            held = False
            asked = time.monotonic()
            try:
                until_renew = max(1.0, renew_every - (asked - renewed))
                now = self.digest(env, path, since=last, wait=min(25.0, until_renew)) if last else self.digest(env, path)
                held = last is not None and (now != last or time.monotonic() - asked >= 1)
                if stop.is_set():
                    break  # stopped while the poll was held: nothing more is called back
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
            if not held and stop.wait(every):
                break
            if time.monotonic() - renewed >= renew_every:
                renewed = time.monotonic()
                for lease in leases:
                    try:
                        self.renew(lease)
                    except Exception as e:  # noqa: BLE001
                        fail(e)


@dataclass(frozen=True)
class Totp:
    """A TOTP key's code and when it stops being valid (``expires_at`` in ms since the epoch: the
    end of its step). The code is a credential: don't log it."""

    code: str = field(repr=False)
    expires_at: int
    period: int


_TOTP_ALGORITHMS = {"SHA1": hashlib.sha1, "SHA256": hashlib.sha256, "SHA512": hashlib.sha512}


def totp_at(seed: str, at: Optional[float] = None) -> Totp:
    """The code of ``seed`` (an ``otpauth://totp/`` URI or bare base32: SHA1, 6 digits, 30 s) at
    ``at`` (Unix seconds; now by default), as the server computes it (RFC 6238): for a seed a sealed
    resolve already gave. Errors never quote the seed."""
    seed = seed.strip()
    alg, digits, period, raw = "SHA1", 6, 30, seed
    if seed[:10].lower() == "otpauth://":
        if seed[:15].lower() != "otpauth://totp/":
            raise ValueError("an otpauth URI must be otpauth://totp/… (HOTP isn't supported)")
        q = urllib.parse.parse_qs(urllib.parse.urlsplit(seed).query)
        raw = (q.get("secret") or [""])[0]
        if not raw:
            raise ValueError("an otpauth URI needs ?secret=")
        alg = (q.get("algorithm") or ["SHA1"])[0].upper()
        if alg not in _TOTP_ALGORITHMS:
            raise ValueError("algorithm must be SHA1, SHA256 or SHA512")
        try:
            digits = int((q.get("digits") or ["6"])[0])
            period = int((q.get("period") or ["30"])[0])
        except ValueError:
            raise ValueError("digits and period are numbers") from None
        if not 6 <= digits <= 8:
            raise ValueError("digits must be 6–8")
        if not 15 <= period <= 120:
            raise ValueError("period must be 15–120 seconds")
    raw = raw.replace(" ", "").upper().rstrip("=")
    try:
        secret = base64.b32decode(raw + "=" * (-len(raw) % 8))
    except (ValueError, TypeError):
        raise ValueError("a TOTP seed is an otpauth://totp/ URI or base32") from None
    if not 10 <= len(secret) <= 128:
        raise ValueError("a TOTP secret is 16–205 base32 characters (80–1024 bits)")
    unix = int(time.time() if at is None else at)
    h = _hmac.new(secret, (unix // period).to_bytes(8, "big"), _TOTP_ALGORITHMS[alg]).digest()
    o = h[-1] & 0x0F
    n = (int.from_bytes(h[o : o + 4], "big") & 0x7FFFFFFF) % 10**digits
    return Totp(code=str(n).zfill(digits), expires_at=(unix // period + 1) * period * 1000, period=period)


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
    fresh OIDC token each time; ``login_with_identity`` takes where to get one)."""
    return login_with_identity(api, identity, lambda: oidc_token)


def login_with_identity(api: str, identity: str, token_source: Callable[[], str], project: Optional[str] = None, timeout: float = 30.0, cache_ttl: float = 0.0) -> Client:
    """A client logged in as the workload identity ``identity``, as ``from_env`` makes one:
    ``token_source`` is called for a fresh OIDC token at each login (this first one, then before the
    roll token ends and when the server refuses it), so a token file that's replaced on disk keeps
    working. ``oidc_token_from_env`` is the source ``from_env`` uses. Raises ``SecretsError`` when
    the first login fails."""
    return _login_with_proof(api, identity, lambda: {"token": token_source().strip()}, project, timeout, cache_ttl)


def login_aws(api: str, identity: str, region: str = "", server_id: str = "") -> Client:
    """Logs in as an AWS identity (docs/secrets.md, "AWS IAM") with the machine's own AWS credentials
    (the standard chain, read again for each login): a GetCallerIdentity signed for ``region``
    (empty: STS's global endpoint) naming ``server_id`` (empty: the API's URL), which roll sends to
    STS."""
    env = lambda k: (os.environ.get(k) or "").strip()  # noqa: E731
    return _login_with_proof(api, identity, _aws_proof(api, region, server_id, env))


def _aws_proof(api: str, region: str, server_id: str, env: Callable[[str], str]) -> Callable[[], Dict[str, Any]]:
    server_id = server_id or api.rstrip("/")
    return lambda: {"aws": aws_signed_request(_aws_credentials(env), region, server_id, time.time())}


def _login_with_proof(api: str, identity: str, source: Callable[[], Dict[str, Any]], project: Optional[str] = None, timeout: float = 30.0, cache_ttl: float = 0.0) -> Client:
    """A client logged in with what ``source`` gives at each login: an OIDC token or a signed AWS request."""
    c = Client(api, "", project=project, timeout=timeout, cache_ttl=cache_ttl)
    c._login = _Relogin(identity, source)
    c._login.token(c, stale="")
    return c


# The names the Ansible collection used before these were public.
_logged_in = login_with_identity


class _Relogin:
    """Keeps a logged-in client's token fresh: a new login once less than a fifth of its life (at
    least a minute) is left, or when the server refuses it."""

    def __init__(self, identity: str, source: Callable[[], Dict[str, Any]]):
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
                    r = Client(c.api, "", timeout=c.timeout)._send("", "POST", "/v1/secrets/login", {"identity": self.identity, **self.source()})
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
    API's URL); without one (or with ROLL_IDENTITY_KIND=aws), a GetCallerIdentity signed with the
    machine's AWS credentials, naming ROLL_AWS_SERVER_ID (default the API's URL). ROLL_PROJECT names
    the project when the token doesn't."""
    env = lambda k: (os.environ.get(k) or "").strip()  # noqa: E731
    api = env("ROLL_API_URL")
    if not api:
        raise SecretsError("ROLL_API_URL isn't set", 400, "config")
    if env("ROLL_TOKEN"):
        c = Client(api, env("ROLL_TOKEN"))
    elif env("ROLL_IDENTITY"):
        # The OIDC token is read again for each login (a projected token is replaced on disk).
        kind = env("ROLL_IDENTITY_KIND")
        if kind not in ("", "aws", "oidc"):
            raise SecretsError("ROLL_IDENTITY_KIND is aws or oidc, not " + kind, 400, "config")
        oidc = any(env(k) for k in ("ROLL_OIDC_TOKEN", "ROLL_OIDC_TOKEN_FILE", "ACTIONS_ID_TOKEN_REQUEST_URL"))
        if kind == "aws" or (kind == "" and not oidc):
            c = _login_with_proof(api, env("ROLL_IDENTITY"), _aws_proof(api, _aws_region(env), env("ROLL_AWS_SERVER_ID"), env))
        else:
            c = login_with_identity(api, env("ROLL_IDENTITY"), lambda: oidc_token_from_env(api, env))
    else:
        raise SecretsError("set ROLL_TOKEN, or ROLL_IDENTITY for a workload identity", 400, "config")
    if env("ROLL_PROJECT"):
        c.project = env("ROLL_PROJECT")
    return c


def oidc_token_from_env(api: str, getenv: Optional[Callable[[str], str]] = None) -> str:
    """The workload's OIDC token as ``from_env`` finds it: ROLL_OIDC_TOKEN, the file
    ROLL_OIDC_TOKEN_FILE names (read now), or GitHub Actions' for the audience ROLL_OIDC_AUDIENCE
    (default ``api``). ``getenv`` reads a setting (the process's environment, stripped, by
    default). Raises ``SecretsError`` with the code ``config`` when there's none."""
    env = getenv or (lambda k: (os.environ.get(k) or "").strip())
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


_oidc_token = oidc_token_from_env


# An AWS identity's login (docs/secrets.md, "AWS IAM"): the machine's own AWS credentials sign an
# sts:GetCallerIdentity that roll sends to STS, as Vault's AWS auth (IAM method) does. Credentials
# come from the standard chain: the environment, the shared files' profile (static keys), a web
# identity token (IRSA), the container endpoint (ECS task roles, EKS Pod Identity), then IMDSv2.

_AWS_BODY = "Action=GetCallerIdentity&Version=2011-06-15"
_AWS_CONTENT_TYPE = "application/x-www-form-urlencoded; charset=utf-8"


def _sigv4(creds: Dict[str, str], region: str, service: str, method: str, path: str, query: str, headers: Dict[str, str], payload: str, amz_date: str) -> str:
    """The Authorization header of a Signature Version 4 request: every header (lowercase names) is
    signed, and ``query`` is already canonical."""
    names = sorted(headers)
    canonical = "".join(f"{k}:{headers[k].strip()}\n" for k in names)
    signed = ";".join(names)
    request = "\n".join([method, path, query, canonical, signed, hashlib.sha256(payload.encode()).hexdigest()])
    day = amz_date[:8]
    scope = f"{day}/{region}/{service}/aws4_request"
    to_sign = "\n".join(["AWS4-HMAC-SHA256", amz_date, scope, hashlib.sha256(request.encode()).hexdigest()])
    key = ("AWS4" + creds["secret"]).encode()
    for part in (day, region, service, "aws4_request"):
        key = _hmac.new(key, part.encode(), hashlib.sha256).digest()
    signature = _hmac.new(key, to_sign.encode(), hashlib.sha256).hexdigest()
    return f"AWS4-HMAC-SHA256 Credential={creds['key']}/{scope}, SignedHeaders={signed}, Signature={signature}"


def _sts_endpoint(region: str) -> "tuple[str, str]":
    if not region:
        return "sts.amazonaws.com", "us-east-1"
    if region.startswith("cn-"):
        return f"sts.{region}.amazonaws.com.cn", region
    return f"sts.{region}.amazonaws.com", region


def aws_signed_request(creds: Dict[str, str], region: str, server_id: str, at: float) -> Dict[str, Any]:
    """A signed GetCallerIdentity for roll to send on (``creds``: ``key``, ``secret``, ``token``),
    naming ``server_id`` in X-Roll-Server-ID (signed, so it can't be replayed at another server)."""
    host, signing_region = _sts_endpoint(region)
    amz_date = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime(at))
    headers = {"content-type": _AWS_CONTENT_TYPE, "host": host, "x-amz-date": amz_date, "x-roll-server-id": server_id}
    if creds.get("token"):
        headers["x-amz-security-token"] = creds["token"]
    auth = _sigv4(creds, signing_region, "sts", "POST", "/", "", headers, _AWS_BODY, amz_date)
    return {"method": "POST", "url": f"https://{host}/", "headers": {**headers, "authorization": auth}, "body": _AWS_BODY}


def _aws_file(env: Callable[[str], str], var: str, name: str) -> str:
    return env(var) or os.path.join(os.path.expanduser("~"), ".aws", name)


def _profile(env: Callable[[str], str]) -> str:
    return env("AWS_PROFILE") or env("AWS_DEFAULT_PROFILE") or "default"


def _config_section(env: Callable[[str], str]) -> str:
    p = _profile(env)
    return "default" if p == "default" else "profile " + p


def _profile_value(path: str, section: str, key: str) -> str:
    try:
        with open(path, encoding="utf-8") as f:
            lines = f.read().splitlines()
    except OSError:
        return ""
    inside = False
    for line in (l.strip() for l in lines):
        if line.startswith("[") and line.endswith("]"):
            inside = line[1:-1].strip() == section
        elif inside and "=" in line and line.split("=", 1)[0].strip() == key:
            return line.split("=", 1)[1].strip()
    return ""


def _aws_region(env: Callable[[str], str]) -> str:
    return env("AWS_REGION") or env("AWS_DEFAULT_REGION") or _profile_value(_aws_file(env, "AWS_CONFIG_FILE", "config"), _config_section(env), "region")


def _xml_text(doc: str, tag: str) -> str:
    start = doc.find(f"<{tag}>")
    end = doc.find(f"</{tag}>", start)
    return doc[start + len(tag) + 2:end].strip() if start >= 0 and end >= 0 else ""


def _container_url_ok(url: str) -> bool:
    import ipaddress

    u = urllib.parse.urlsplit(url)
    if u.scheme == "https":
        return True
    if u.scheme != "http" or not u.hostname:
        return False
    if u.hostname == "localhost":
        return True
    try:
        ip = ipaddress.ip_address(u.hostname)
    except ValueError:
        return False
    return ip.is_loopback or str(ip) in ("169.254.170.2", "169.254.170.23", "fd00:ec2::23")


def _fetch(req: urllib.request.Request, what: str, timeout: float) -> bytes:
    try:
        with urllib.request.urlopen(req, timeout=timeout) as res:
            return res.read(64 << 10)
    except urllib.error.HTTPError as e:
        raise SecretsError(f"{what} answered {e.code}", 502, "aws") from None
    except (urllib.error.URLError, OSError) as e:
        raise SecretsError(f"can't reach {what}: {e}", 502, "aws") from None


def _creds_json(body: bytes, what: str) -> Dict[str, str]:
    try:
        v = json.loads(body)
    except ValueError:
        v = {}
    if not v.get("AccessKeyId") or not v.get("SecretAccessKey"):
        raise SecretsError(f"{what} answered no credentials", 502, "aws")
    return {"key": v["AccessKeyId"], "secret": v["SecretAccessKey"], "token": v.get("Token") or ""}


def _aws_credentials(env: Callable[[str], str]) -> Dict[str, str]:
    if env("AWS_ACCESS_KEY_ID") and env("AWS_SECRET_ACCESS_KEY"):
        return {"key": env("AWS_ACCESS_KEY_ID"), "secret": env("AWS_SECRET_ACCESS_KEY"), "token": env("AWS_SESSION_TOKEN")}
    shared, cfg, section = _aws_file(env, "AWS_SHARED_CREDENTIALS_FILE", "credentials"), _aws_file(env, "AWS_CONFIG_FILE", "config"), _config_section(env)
    value = lambda k: _profile_value(shared, _profile(env), k) or _profile_value(cfg, section, k)  # noqa: E731
    if value("aws_access_key_id") and value("aws_secret_access_key"):
        return {"key": value("aws_access_key_id"), "secret": value("aws_secret_access_key"), "token": value("aws_session_token")}
    if env("AWS_PROFILE") and not env("AWS_WEB_IDENTITY_TOKEN_FILE"):
        raise SecretsError(f"AWS profile {_profile(env)} has no access keys: roll reads a profile's static keys (for SSO, `eval \"$(aws configure export-credentials --format env)\"` first)", 400, "config")
    # IRSA: the pod's projected token for the role, exchanged with STS (unsigned).
    if env("AWS_WEB_IDENTITY_TOKEN_FILE") and env("AWS_ROLE_ARN"):
        with open(env("AWS_WEB_IDENTITY_TOKEN_FILE"), encoding="utf-8") as f:
            token = f.read().strip()
        host, _ = _sts_endpoint(_aws_region(env))
        form = {"Action": "AssumeRoleWithWebIdentity", "Version": "2011-06-15", "RoleArn": env("AWS_ROLE_ARN"), "RoleSessionName": env("AWS_ROLE_SESSION_NAME") or f"roll-{int(time.time())}", "WebIdentityToken": token}
        req = urllib.request.Request(f"https://{host}/", data=urllib.parse.urlencode(form).encode(), headers={"Content-Type": _AWS_CONTENT_TYPE}, method="POST")
        doc = _fetch(req, "STS (the web identity)", 10).decode("utf-8", "replace")
        creds = _xml_text(doc, "Credentials")
        if not creds:
            raise SecretsError("STS answered no credentials for the web identity", 502, "aws")
        return {"key": _xml_text(creds, "AccessKeyId"), "secret": _xml_text(creds, "SecretAccessKey"), "token": _xml_text(creds, "SessionToken")}
    # ECS task roles (relative) and EKS Pod Identity (full, with a token).
    rel = env("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI")
    endpoint = "http://169.254.170.2" + rel if rel else env("AWS_CONTAINER_CREDENTIALS_FULL_URI")
    if endpoint:
        if not _container_url_ok(endpoint):
            raise SecretsError(f"the container credentials endpoint is local or https, not {endpoint}", 400, "config")
        req = urllib.request.Request(endpoint)
        auth = env("AWS_CONTAINER_AUTHORIZATION_TOKEN")
        if env("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"):
            with open(env("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"), encoding="utf-8") as f:
                auth = f.read().strip()
        if auth:
            req.add_header("Authorization", auth)
        return _creds_json(_fetch(req, "the container credentials endpoint", 5), "the container endpoint")
    if env("AWS_EC2_METADATA_DISABLED").lower() == "true":
        raise SecretsError("no AWS credentials (and the instance metadata service is off)", 400, "config")
    # IMDSv2: a session token, the instance profile's role, its credentials.
    base = (env("AWS_EC2_METADATA_SERVICE_ENDPOINT") or "http://169.254.169.254").rstrip("/")
    what = "the instance metadata service (no AWS credentials in the environment, a profile, a web identity or a container)"
    token = _fetch(urllib.request.Request(base + "/latest/api/token", method="PUT", headers={"X-aws-ec2-metadata-token-ttl-seconds": "300"}), what, 2).decode()
    path = base + "/latest/meta-data/iam/security-credentials/"
    roles = _fetch(urllib.request.Request(path, headers={"X-aws-ec2-metadata-token": token}), what, 2).decode().split("\n")
    if not roles[0].strip():
        raise SecretsError("the instance has no instance profile", 400, "config")
    body = _fetch(urllib.request.Request(path + urllib.parse.quote(roles[0].strip()), headers={"X-aws-ec2-metadata-token": token}), what, 2)
    return _creds_json(body, "the instance metadata service")


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
