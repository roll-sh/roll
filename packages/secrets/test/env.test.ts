import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { fromEnv, oidcTokenFromEnv } from "../src/index.js";

const settings = (s: Record<string, string>) => (k: string) => s[k] ?? "";

test("fromEnv: a token, or a workload identity with GitHub Actions' OIDC token, as the Go and Python clients", async () => {
  await assert.rejects(fromEnv({ getenv: settings({}) }), /ROLL_API_URL isn't set/);
  await assert.rejects(fromEnv({ getenv: settings({ ROLL_API_URL: "https://roll.example" }) }), /ROLL_TOKEN, or ROLL_IDENTITY/);

  const withToken = await fromEnv({ getenv: settings({ ROLL_API_URL: "https://roll.example", ROLL_TOKEN: "rlt_fake", ROLL_PROJECT: "prj_a" }) });
  assert.equal(await withToken.project(), "prj_a");

  // In GitHub Actions: the job's token for the API's own URL, then a login as the identity.
  const asked: string[] = [];
  const fetch = (async (url: string | URL | Request, init?: RequestInit) => {
    const u = String(url);
    asked.push(u);
    if (u.startsWith("https://actions.example/token")) {
      assert.equal(new Headers(init?.headers).get("authorization"), "Bearer fake-request-token");
      return new Response(JSON.stringify({ value: "fake-oidc" }));
    }
    const b = JSON.parse(String(init?.body)) as { identity: string; token: string };
    assert.deepEqual(b, { identity: "idn_ci", token: "fake-oidc" });
    return new Response(JSON.stringify({ token: "rlt_login", project: "prj_b", expires_at: Date.now() + 3_600_000 }));
  }) as typeof globalThis.fetch;
  const ci = await fromEnv({
    fetch,
    getenv: settings({ ROLL_API_URL: "https://roll.example/", ROLL_IDENTITY: "idn_ci", ACTIONS_ID_TOKEN_REQUEST_URL: "https://actions.example/token?x=1", ACTIONS_ID_TOKEN_REQUEST_TOKEN: "fake-request-token" }),
  });
  assert.equal(await ci.project(), "prj_b");
  assert.equal(asked[0], "https://actions.example/token?x=1&audience=https%3A%2F%2Froll.example");
  assert.equal(asked[1], "https://roll.example/v1/secrets/login");

  // A projected token is read from its file (a trailing newline isn't part of it).
  const dir = await mkdtemp(join(tmpdir(), "roll-oidc-"));
  try {
    await writeFile(join(dir, "token"), "fake-from-file\n");
    assert.equal(await oidcTokenFromEnv("https://roll.example", settings({ ROLL_OIDC_TOKEN_FILE: join(dir, "token") })), "fake-from-file");
  } finally {
    await rm(dir, { recursive: true });
  }
  await assert.rejects(oidcTokenFromEnv("https://roll.example", settings({})), /no OIDC token/);
});
