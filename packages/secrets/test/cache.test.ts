import assert from "node:assert/strict";
import { test } from "node:test";
import { createClient } from "../src/index.js";
import type { Values } from "../src/index.js";

test("values are kept for cacheTtlMs, revalidated with their digest, and never kept with dynamic credentials", async () => {
  let digest = "d1";
  let served: Values = { values: { A: "1" }, problems: [], leases: [], dynamic: [], linked: [] };
  const asked: (string | null)[] = [];
  const fetch = (async (url: string | URL | Request, init?: RequestInit) => {
    const u = new URL(String(url));
    const tag = (init?.headers as Record<string, string>)["if-none-match"] ?? null;
    asked.push(tag);
    assert.equal(u.searchParams.get("digest"), "1", "asks for the digest it reads against");
    if (tag === `"${digest}"`) return new Response(null, { status: 304, headers: { etag: `"${digest}"` } });
    return new Response(JSON.stringify(served), { status: 200, headers: { "content-type": "application/json", etag: `"${digest}"` } });
  }) as typeof globalThis.fetch;
  let clock = 0;
  const client = createClient({ api: "http://rolld.test", token: "rlt_test", project: "prj_1", fetch, cacheTtlMs: 20, now: () => clock });

  assert.deepEqual((await client.values("production")).values, { A: "1" });
  await client.values("production");
  assert.deepEqual(asked, [null], "within the TTL: from memory");
  clock += 25;
  assert.deepEqual((await client.values("production")).values, { A: "1" });
  assert.deepEqual(asked, [null, '"d1"'], "then revalidated: a 304 keeps them");

  clock += 25;
  digest = "d2";
  served = { values: { A: "2" }, problems: [], leases: ["lse_1"], dynamic: ["PW"], linked: [] };
  assert.deepEqual((await client.values("production")).values, { A: "2" }, "a change is read");
  await client.values("production");
  assert.equal(asked.length, 4, "dynamic credentials are read again every time");
});

test("a watch on a caching client reads the new values when the digest moves", async () => {
  let digest = "d1";
  let served: Values = { values: { A: "1" }, problems: [], leases: [], dynamic: [], linked: [] };
  const fetch = (async (url: string | URL | Request) => {
    const u = new URL(String(url));
    if (u.pathname.endsWith("/digest")) return new Response(JSON.stringify({ digest }), { status: 200 });
    return new Response(JSON.stringify(served), { status: 200, headers: { etag: `"${digest}"` } });
  }) as typeof globalThis.fetch;
  // A TTL far longer than the test: only a fresh read can see the change.
  const client = createClient({ api: "http://rolld.test", token: "rlt_test", project: "prj_1", fetch, cacheTtlMs: 3_600_000 });
  await client.values("production");
  const seen: Values[] = [];
  const stop = client.watch("production", { intervalMs: 5, onChange: (v) => void seen.push(v) });
  for (let i = 0; i < 200 && seen.length < 1; i++) await new Promise((r) => setTimeout(r, 5));
  served = { values: { A: "2" }, problems: [], leases: [], dynamic: [], linked: [] };
  digest = "d2";
  for (let i = 0; i < 200 && seen.length < 2; i++) await new Promise((r) => setTimeout(r, 5));
  stop();
  assert.deepEqual(seen.map((v) => v.values.A), ["1", "2"]);
  assert.equal((await client.values("production")).values.A, "2", "and the kept answer is the new one");
});
