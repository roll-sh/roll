import assert from "node:assert/strict";
import { test } from "node:test";
import { loginWithIdentity, SecretsError } from "../src/index.js";

test("a key's record and stored value, after a login whose OIDC token comes from a source", async () => {
  let logins = 0;
  let valueReads = 0;
  let current = "rlt_reads";
  const records: Record<string, unknown> = {
    A: { key: "A", path: "/api", type: "shared", version: 3, comment: "made up", tags: [], link: null, source: { kind: "own" }, updated_at: 1, updated_by: "rlt_x", expires_at: null },
    B: { key: "B", path: "/api", type: "shared", version: 1, comment: "", tags: [], link: { kind: "postgres", ref: "pg_1", field: "url", mode: "live" }, source: { kind: "base", environment: "development" }, updated_at: 2, updated_by: "rlt_x", expires_at: null },
  };
  const fetch = (async (url: string | URL | Request, init?: RequestInit) => {
    const u = new URL(String(url));
    const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status });
    if (u.pathname === "/v1/secrets/login") {
      const b = JSON.parse(String(init?.body)) as { identity: string; token: string };
      if (b.identity !== "idn_ci" || b.token !== "fake-oidc") return json(401, { error: "no identity idn_ci takes this token" });
      logins++;
      return json(200, { token: current, project: "prj_a", expires_at: Date.now() + 3_600_000 });
    }
    const prefix = "/v1/secrets/projects/prj_a/environments/staging/secrets/";
    const auth = new Headers(init?.headers).get("authorization");
    if (!u.pathname.startsWith(prefix) || auth !== `Bearer ${current}` || u.searchParams.get("path") !== "/api") return json(401, { error: "who" });
    const key = decodeURIComponent(u.pathname.slice(prefix.length));
    if (key === "A/value") {
      valueReads++;
      return json(200, { value: u.searchParams.get("resolve") === "false" ? "${development.FAKE_URL}" : "fake-resolved", problems: [] });
    }
    if (key in records) return json(200, records[key]);
    return json(404, { error: `no secret ${key} in staging:/api` });
  }) as typeof globalThis.fetch;

  let sourced = 0;
  const client = await loginWithIdentity({ api: "http://rolld.test", identity: "idn_ci", tokenSource: () => (sourced++, "fake-oidc"), fetch, cacheTtlMs: 3_600_000 });
  assert.deepEqual([sourced, logins, await client.project()], [1, 1, "prj_a"], "the source gives the login its OIDC token");

  const a = await client.secret("staging", "A", { path: "/api" });
  assert.deepEqual([a?.version, a?.comment, a?.link_kind, a?.source], [3, "made up", null, { kind: "own" }]);
  const b = await client.secret("staging", "B", { path: "/api" });
  assert.deepEqual([b?.link_kind, b?.source, "link" in (b ?? {})], ["postgres", { kind: "base", environment: "development" }, false], "a link shows its kind only");
  assert.equal(await client.secret("staging", "NOPE", { path: "/api" }), null, "a missing key is null");

  assert.equal(await client.storedValue("staging", "A", { path: "/api" }), "${development.FAKE_URL}", "the reference as written");
  await client.storedValue("staging", "A", { path: "/api" });
  assert.equal(valueReads, 2, "never kept, whatever cacheTtlMs says");
  await assert.rejects(client.storedValue("staging", "NOPE", { path: "/api" }), (e: unknown) => e instanceof SecretsError && e.status === 404);

  // The roll token stops working (revoked, ended): the client logs in again, asking the source anew.
  current = "rlt_reads2";
  assert.equal((await client.secret("staging", "A", { path: "/api" }))?.version, 3);
  assert.deepEqual([sourced, logins], [2, 2]);
});
