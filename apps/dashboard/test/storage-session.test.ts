// Who the storage dashboard acts as (src/lib/storage-session.ts): the account / session headers on
// every storage request, 401 handling, console sign-in and the accounts API, with `fetch` stubbed.
// Node has no localStorage here, so the module's in-memory stand-in is what's exercised.

import assert from "node:assert/strict";
import { afterEach, beforeEach, describe, it } from "node:test";
import { ROLL_API } from "../src/lib/roll-api";
import { storageApi, uploadObject } from "../src/lib/storage-api";
import {
  accountLabel,
  accountQuery,
  accountsApi,
  acknowledgeEnded,
  clearSession,
  consoleApi,
  errorCode,
  getAccount,
  getScope,
  getSession,
  isAccountId,
  isOperatorOnly,
  parseSession,
  sessionEnded,
  setAccount,
  setSession,
  signinBody,
  signinStepFor,
  storageCall,
  storageHeaders,
  StorageError,
  subscribe,
  type ConsoleSession,
} from "../src/lib/storage-session";

interface Call {
  method: string;
  raw: string;
  url: URL;
  headers: Record<string, string>;
  body: unknown;
  init: RequestInit;
}

const json = (v: unknown, status = 200, statusText = "") =>
  new Response(JSON.stringify(v), { status, statusText, headers: { "content-type": "application/json" } });

const realFetch = globalThis.fetch;
let calls: Call[] = [];
let reply: (c: Call) => Response | Promise<Response> = () => json({});

const SESSION: ConsoleSession = {
  token: "tok-1",
  expires_at: Date.now() + 3_600_000,
  account_id: "222233334444",
  account_alias: "acme",
  user: "alice",
  arn: "arn:aws:iam::222233334444:user/alice",
  mfa: true,
};

beforeEach(() => {
  calls = [];
  reply = () => json({});
  clearSession();
  acknowledgeEnded();
  setAccount(null);
  globalThis.fetch = (async (input: string | URL | Request, init: RequestInit = {}) => {
    const raw = String(input);
    const c: Call = {
      method: init.method ?? "GET",
      raw,
      url: new URL(raw),
      headers: { ...((init.headers as Record<string, string> | undefined) ?? {}) },
      body: typeof init.body === "string" ? JSON.parse(init.body) : undefined,
      init,
    };
    calls.push(c);
    return reply(c);
  }) as typeof fetch;
});

afterEach(() => {
  globalThis.fetch = realFetch;
});

describe("headers", () => {
  it("operator in the default account: no extra headers at all", async () => {
    await storageApi.buckets();
    assert.equal(calls.length, 1);
    assert.equal(calls[0].init.headers, undefined);
    assert.equal(calls[0].init.cache, "no-store");
    assert.equal(getScope(), "default");
  });

  it("operator in another account: x-roll-account on every request", async () => {
    setAccount("111122223333");
    assert.equal(getAccount(), "111122223333");
    await storageApi.buckets();
    await storageApi.createBucket("b-1");
    assert.deepEqual(calls[0].headers, { "x-roll-account": "111122223333" });
    assert.deepEqual(calls[1].headers, { "x-roll-account": "111122223333", "content-type": "application/json" });
    assert.equal(getScope(), "account:111122223333");
  });

  it("only 12-digit account ids are kept", () => {
    setAccount("12345");
    assert.equal(getAccount(), null);
    setAccount("abcdefghijkl");
    assert.equal(getAccount(), null);
    assert.ok(isAccountId("000000000000"));
    assert.ok(!isAccountId("0000000000001"));
  });

  it("a console session sends its bearer token and not the account", async () => {
    setAccount("111122223333");
    setSession(SESSION);
    assert.deepEqual(storageHeaders(), { authorization: "Bearer tok-1" });
    await storageApi.info();
    assert.deepEqual(calls[0].headers, { authorization: "Bearer tok-1" });
    assert.equal(getScope(), "session:222233334444:alice");
  });

  it("an expired session ends: no request goes out as the operator instead", async () => {
    setSession({ ...SESSION, expires_at: Date.now() - 1 });
    assert.equal(getSession(), null);
    await assert.rejects(storageApi.info(), { status: 401, code: "ExpiredToken" });
    assert.equal(calls.length, 0);
    assert.equal(sessionEnded(), true);
    // Ended once; then it's plain operator mode until someone signs in.
    await storageApi.info();
    assert.equal(calls[0].init.headers, undefined);
  });

  it("getSession is stable between changes (useSyncExternalStore)", () => {
    setSession(SESSION);
    assert.equal(getSession(), getSession());
  });

  it("subscribers hear about every change", () => {
    let n = 0;
    const off = subscribe(() => n++);
    setAccount("111122223333");
    setSession(SESSION);
    clearSession();
    off();
    setAccount(null);
    assert.equal(n, 3);
  });

  it("download links name the operator's account; with a session they don't (fetched with the header instead)", () => {
    assert.equal(storageApi.downloadUrl("b", "k"), `${ROLL_API}/v1/storage/buckets/b/download?key=k`);
    setAccount("111122223333");
    assert.deepEqual(accountQuery(), { account: "111122223333" });
    assert.equal(storageApi.downloadUrl("b", "k", "v1"), `${ROLL_API}/v1/storage/buckets/b/download?key=k&version_id=v1&account=111122223333`);
    setSession(SESSION);
    assert.deepEqual(accountQuery(), {});
    assert.equal(storageApi.downloadUrl("b", "k"), `${ROLL_API}/v1/storage/buckets/b/download?key=k`);
  });
});

describe("errors", () => {
  it("rejects with a StorageError carrying the status and code", async () => {
    reply = () => json({ error: "User: alice is not authorized", code: "AccessDenied" }, 403);
    await assert.rejects(storageCall("GET", "/v1/storage/buckets"), (e: unknown) => {
      assert.ok(e instanceof StorageError);
      assert.equal(e.message, "User: alice is not authorized");
      assert.equal(e.status, 403);
      assert.equal(e.code, "AccessDenied");
      assert.equal(errorCode(e), "AccessDenied");
      return true;
    });
  });

  it("falls back to the status line, with no code", async () => {
    reply = () => new Response("", { status: 502, statusText: "Bad Gateway" });
    await assert.rejects(storageCall("GET", "/v1/storage"), { message: "502 Bad Gateway", status: 502, code: null });
  });

  it("OperatorOnly is recognised", async () => {
    reply = () => json({ error: "operator only", code: "OperatorOnly" }, 403);
    await assert.rejects(storageApi.keys(), (e: unknown) => isOperatorOnly(e));
    assert.equal(isOperatorOnly(new StorageError("x", 403, "AccessDenied")), false);
  });

  it("a 401 ends the session that was sent, and says so", async () => {
    setSession(SESSION);
    reply = () => json({ error: "The security token expired", code: "ExpiredToken" }, 401);
    await assert.rejects(storageApi.buckets(), { status: 401, code: "ExpiredToken" });
    assert.equal(getSession(), null);
    assert.equal(sessionEnded(), true);
    acknowledgeEnded();
    assert.equal(sessionEnded(), false);
  });

  it("a 401 without a session changes nothing", async () => {
    setAccount("111122223333");
    reply = () => json({ error: "no" }, 401);
    await assert.rejects(storageApi.buckets());
    assert.equal(getAccount(), "111122223333");
    assert.equal(sessionEnded(), false);
  });

  it("signing out isn't 'ended'", () => {
    setSession(SESSION);
    consoleApi.signout();
    assert.equal(getSession(), null);
    assert.equal(sessionEnded(), false);
  });

  it("a network failure rejects as fetch does (not a StorageError: rolld is offline)", async () => {
    reply = () => {
      throw new TypeError("fetch failed");
    };
    await assert.rejects(storageApi.info(), (e: unknown) => e instanceof TypeError && !(e instanceof StorageError));
  });

  it("a non-JSON success is an error", async () => {
    reply = () => new Response("<html>", { status: 200 });
    await assert.rejects(storageCall("GET", "/v1/storage"), /isn't JSON/);
  });
});

describe("console sign-in", () => {
  it("signs in anonymously, keeps the session and remembers account and user", async () => {
    setSession({ ...SESSION, token: "old" });
    reply = () => json({ ...SESSION, token: "new" });
    const s = await consoleApi.signin({ account: " acme ", user: " alice ", password: "pw  " });
    const c = calls[0];
    assert.equal(c.method, "POST");
    assert.equal(c.url.pathname, "/v1/storage/console/signin");
    assert.equal(c.headers.authorization, undefined);
    assert.deepEqual(c.body, { account: "acme", user: "alice", password: "pw  " });
    assert.equal(s.token, "new");
    assert.equal(getSession()?.token, "new");
    assert.deepEqual(consoleApi.lastSignin(), { account: "acme", user: "alice" });
  });

  it("a failed sign-in doesn't touch an existing session", async () => {
    setSession(SESSION);
    reply = () => json({ error: "Invalid credentials", code: "InvalidCredentials" }, 401);
    await assert.rejects(consoleApi.signin({ account: "a", user: "u", password: "p" }), { code: "InvalidCredentials" });
    assert.equal(getSession()?.token, "tok-1");
    assert.equal(sessionEnded(), false);
  });

  it("MFA and password-reset steps come from the codes", async () => {
    reply = () => json({ error: "MFA code required", code: "MfaRequired" }, 401);
    await assert.rejects(consoleApi.signin({ account: "a", user: "u", password: "p" }), (e: unknown) => signinStepFor(errorCode(e)) === "mfa");
    assert.equal(signinStepFor("PasswordResetRequired"), "new-password");
    assert.equal(signinStepFor("InvalidMfaCode"), null);
    assert.equal(signinStepFor(null), null);
  });

  it("signinBody sends the MFA code and new password only when given", () => {
    assert.deepEqual(signinBody({ account: "a", user: "u", password: "p", mfa_code: " 123456 ", new_password: "N3w!" }), {
      account: "a",
      user: "u",
      password: "p",
      mfa_code: "123456",
      new_password: "N3w!",
    });
    assert.deepEqual(signinBody({ account: "a", user: "u", password: "p", mfa_code: "", new_password: "" }), { account: "a", user: "u", password: "p" });
  });

  it("session() asks with the token", async () => {
    setSession(SESSION);
    reply = () => json({ expires_at: 1, account_id: "222233334444", account_alias: null, user: "alice", arn: "x", mfa: false });
    await consoleApi.session();
    assert.equal(calls[0].url.pathname, "/v1/storage/console/session");
    assert.equal(calls[0].headers.authorization, "Bearer tok-1");
  });

  it("parseSession rejects what isn't a session", () => {
    assert.equal(parseSession(null), null);
    assert.equal(parseSession("{"), null);
    assert.equal(parseSession(JSON.stringify({ token: "", expires_at: 1, account_id: "1", user: "u" })), null);
    assert.equal(parseSession(JSON.stringify({ token: "t", expires_at: "1", account_id: "1", user: "u" })), null);
    assert.deepEqual(parseSession(JSON.stringify({ token: "t", expires_at: 5, account_id: "1", user: "u" })), {
      token: "t",
      expires_at: 5,
      account_id: "1",
      account_alias: null,
      user: "u",
      arn: "",
      mfa: false,
    });
  });
});

describe("accounts API", () => {
  it("list, get, create, update, remove", async () => {
    await accountsApi.list();
    await accountsApi.get("111122223333");
    await accountsApi.create("acme", { email: " ops@acme.dev ", root_key: false });
    await accountsApi.create("bare");
    await accountsApi.update("111122223333", { email: null });
    await accountsApi.remove("111122223333");
    assert.deepEqual(
      calls.map((c) => [c.method, c.url.pathname, c.body]),
      [
        ["GET", "/v1/storage/accounts", undefined],
        ["GET", "/v1/storage/accounts/111122223333", undefined],
        ["POST", "/v1/storage/accounts", { name: "acme", email: "ops@acme.dev", root_key: false }],
        ["POST", "/v1/storage/accounts", { name: "bare" }],
        ["PATCH", "/v1/storage/accounts/111122223333", { email: null }],
        ["DELETE", "/v1/storage/accounts/111122223333", undefined],
      ],
    );
  });

  it("deleting an account with buckets is a 409 with its message", async () => {
    reply = () => json({ error: "the account still has buckets" }, 409);
    await assert.rejects(accountsApi.remove("111122223333"), { message: "the account still has buckets", status: 409 });
  });

  it("accountLabel prefers the alias", () => {
    assert.equal(accountLabel({ name: "Acme", alias: "acme-prod" }), "acme-prod");
    assert.equal(accountLabel({ name: "Acme", alias: null }), "Acme");
  });
});

describe("uploads", () => {
  class FakeXhr {
    static last: FakeXhr;
    headers: Record<string, string> = {};
    status = 0;
    statusText = "";
    responseText = "";
    upload = { onprogress: null as unknown };
    onload: (() => void) | null = null;
    onerror: (() => void) | null = null;
    onabort: (() => void) | null = null;
    constructor() {
      FakeXhr.last = this;
    }
    open() {}
    setRequestHeader(k: string, v: string) {
      this.headers[k] = v;
    }
    send() {}
    abort() {}
  }
  const g = globalThis as { XMLHttpRequest?: unknown };
  const realXhr = g.XMLHttpRequest;
  beforeEach(() => {
    g.XMLHttpRequest = FakeXhr;
  });
  afterEach(() => {
    g.XMLHttpRequest = realXhr;
  });

  it("carry the session, and a 401 ends it", async () => {
    setSession(SESSION);
    const p = uploadObject("b", "k", new Blob(["x"]), () => {});
    assert.equal(FakeXhr.last.headers.authorization, "Bearer tok-1");
    FakeXhr.last.status = 401;
    FakeXhr.last.responseText = JSON.stringify({ error: "expired", code: "ExpiredToken" });
    FakeXhr.last.onload?.();
    await assert.rejects(p, { message: "expired", status: 401, code: "ExpiredToken" });
    assert.equal(getSession(), null);
  });

  it("carry the operator's account", async () => {
    setAccount("111122223333");
    const p = uploadObject("b", "k", new Blob(["x"]), () => {});
    assert.equal(FakeXhr.last.headers["x-roll-account"], "111122223333");
    FakeXhr.last.status = 200;
    FakeXhr.last.responseText = "{}";
    FakeXhr.last.onload?.();
    await p;
  });
});
