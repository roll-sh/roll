import assert from "node:assert/strict";
import { test } from "node:test";
import { createClient, SecretsError } from "../src/index.js";

test("set and delete write one key and drop the cache; approval_required is an error without the value", async () => {
  const stored: Record<string, string> = { A: "1" };
  let generation = 0;
  let reads = 0;
  let protectedEnv = false;
  const writes: unknown[] = [];
  const fetch = (async (url: string | URL | Request, init?: RequestInit) => {
    const u = new URL(String(url));
    const json = (status: number, body: unknown) => new Response(JSON.stringify(body), { status, headers: { etag: `"g${generation}"` } });
    if (u.pathname.endsWith("/values")) {
      reads++;
      // PW is a dynamic link's: dynamic=false leaves it out and names it.
      const noDynamic = u.searchParams.get("dynamic") === "false";
      return json(200, { values: noDynamic ? { ...stored } : { ...stored, PW: "fake-issued" }, problems: [], leases: noDynamic ? [] : ["lse_1"], dynamic: ["PW"], linked: [] });
    }
    const key = decodeURIComponent(u.pathname.split("/").pop()!);
    assert.equal(u.searchParams.get("path"), "/api");
    if (protectedEnv) return json(409, { error: "production is protected: open a change request", code: "approval_required" });
    const change = { seq: ++generation, environment: "production", path: "/api", message: "", by: "rlt_test", at: 0, keys: 1, kind: "write" };
    if (init?.method === "PUT") {
      const body = JSON.parse(String(init.body)) as { value: string };
      writes.push(body);
      stored[key] = body.value;
      return json(200, { change, secret: { key, version: 2 }, warnings: [] });
    }
    if (!(key in stored)) return json(404, { error: `no secret ${key} in production:/api` });
    delete stored[key];
    return json(200, { change, secret: null, warnings: [] });
  }) as typeof globalThis.fetch;
  const client = createClient({ api: "http://rolld.test", token: "rlt_test", project: "prj_1", fetch, cacheTtlMs: 3_600_000 });

  const before = await client.values("production", { dynamic: false });
  assert.deepEqual([before.values, before.dynamic, before.leases], [{ A: "1" }, ["PW"], []], "dynamic=false leaves the dynamic key out");
  await client.values("production", { dynamic: false });
  assert.equal(reads, 1, "and holds no credential, so it's kept");

  const w = await client.set("production", "B", "fake-value-1", { path: "/api", comment: "made up" });
  assert.equal(w.secret?.version, 2);
  assert.deepEqual(writes, [{ value: "fake-value-1", link: null, comment: "made up" }]);
  assert.equal((await client.values("production", { dynamic: false })).values.B, "fake-value-1", "a set drops the cache");

  assert.deepEqual(await client.delete("production", "B", { path: "/api" }), { deleted: true, change: { ...w.change!, seq: 2 } });
  assert.equal((await client.values("production", { dynamic: false })).values.B, undefined, "a delete drops the cache");
  assert.deepEqual(await client.delete("production", "B", { path: "/api" }), { deleted: false, change: null }, "a missing key is no change");

  protectedEnv = true;
  await assert.rejects(client.set("production", "C", "fake-value-2", { path: "/api" }), (e: unknown) => {
    assert.ok(e instanceof SecretsError && e.code === "approval_required" && e.status === 409);
    assert.ok(!`${e.message}${e.stack}`.includes("fake-value-2"), "the value is in no error");
    return true;
  });
});
