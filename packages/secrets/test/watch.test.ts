import assert from "node:assert/strict";
import { test } from "node:test";
import { createClient } from "../src/index.js";
import type { Values } from "../src/index.js";

const json = (body: unknown) => new Response(JSON.stringify(body), { status: 200, headers: { "content-type": "application/json" } });
const until = async (ok: () => boolean) => {
  for (let i = 0; i < 200 && !ok(); i++) await new Promise((r) => setTimeout(r, 5));
  assert.ok(ok(), "timed out");
};

test("watch follows the digest, keeps values that don't resolve, renews leases and stops", async () => {
  let digest = "d1";
  let served: Values = { values: { A: "1" }, problems: [], leases: ["lse_1"], dynamic: ["PW"], linked: [] };
  const calls: string[] = [];
  const fetch = (async (url: string | URL | Request, init?: RequestInit) => {
    const u = new URL(String(url));
    calls.push(`${init?.method} ${u.pathname}`);
    assert.equal((init?.headers as Record<string, string>).authorization, "Bearer rlst_test");
    if (u.pathname === "/v1/secrets/whoami") return json({ principal: "token:tok_1", project: "prj_1" });
    if (u.pathname.endsWith("/digest")) return json({ digest });
    if (u.pathname.endsWith("/values")) return json(served);
    if (u.pathname.endsWith("/renew")) return json({ id: "lse_1" });
    return new Response(JSON.stringify({ error: "no" }), { status: 404 });
  }) as typeof globalThis.fetch;

  const client = createClient({ api: "http://rolld.test/", token: "rlst_test", fetch });
  const seen: Values[] = [];
  const errors: unknown[] = [];
  const stop = client.watch("staging", { intervalMs: 5, renewMs: 15, onChange: (v) => void seen.push(v), onError: (e) => errors.push(e) });

  await until(() => seen.length === 1);
  assert.deepEqual(seen[0].values, { A: "1" });
  assert.ok(calls.includes("GET /v1/secrets/projects/prj_1/environments/staging/digest"), "the token's own project");
  await until(() => calls.includes("POST /v1/secrets/projects/prj_1/leases/lse_1/renew"));

  // A change that doesn't resolve keeps the values; the next change goes through.
  served = { values: {}, problems: [{ key: "B", problem: "${X} doesn't exist" }], leases: [], dynamic: [], linked: [] };
  digest = "d2";
  await until(() => errors.length === 1);
  assert.equal(seen.length, 1);
  served = { values: { A: "2" }, problems: [], leases: [], dynamic: [], linked: [] };
  digest = "d3";
  await until(() => seen.length === 2);
  assert.deepEqual(seen[1].values, { A: "2" });

  stop();
  const n = calls.length;
  await new Promise((r) => setTimeout(r, 40));
  assert.equal(calls.length, n, "nothing after stop");
});
