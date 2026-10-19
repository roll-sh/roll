import assert from "node:assert/strict";
import http from "node:http";
import type { AddressInfo } from "node:net";
import { describe, it } from "node:test";
import { SDK, createFunction, serve, signatureHeaders, toNodeHandler } from "../src/index.js";

const KEY = "whsec_" + btoa("0123456789abcdef0123456789abcdef");
const NEXT = "whsec_" + btoa("fedcba9876543210fedcba9876543210");

const hello = createFunction({ id: "hello", trigger: { event: "hi" }, retries: 1 }, async ({ event, step }) => {
  const greeting = await step.run("greet", () => `hello ${event.data.name}`);
  return greeting;
});

const payload = (steps: Record<string, unknown> = {}) =>
  JSON.stringify({ function_id: "hello", run_id: "run_1", attempt: 0, event: { id: "e", name: "hi", data: { name: "ann" }, user: null, ts: 1 }, events: [], steps, ctx: {} });

async function signed(body: string, secret = KEY, fn = "hello") {
  const headers = await signatureHeaders({ secrets: secret, id: "msg_1", body });
  return new Request(`http://app.test/api/roll?fnId=${fn}`, { method: "POST", headers: { ...headers, "content-type": "application/json" }, body });
}

describe("serve", () => {
  it("describes the app on GET", async () => {
    const handler = serve({ appId: "shop", functions: [hello], signingKeys: KEY });
    const res = await handler(new Request("http://app.test/api/roll"));
    assert.equal(res.status, 200);
    assert.deepEqual(await res.json(), {
      id: "shop",
      functions: [{ id: "hello", triggers: [{ event: "hi" }], retries: 1 }],
      sdk: SDK,
    });
    assert.match(SDK, /^roll-tasks-js\/\d+\.\d+\.\d+/);
    assert.deepEqual(handler.description.functions[0], hello.definition);
  });

  it("runs signed calls", async () => {
    const handler = serve({ appId: "shop", functions: [hello], signingKeys: [KEY, NEXT] });
    let res = await handler(await signed(payload()));
    assert.equal(res.status, 200);
    assert.deepEqual(await res.json(), { status: "steps", steps: [{ op: "run", id: "greet", name: "greet", data: "hello ann" }] });
    // Signed with the next key only (after a rotation).
    res = await handler(await signed(payload({ greet: { data: "hello ann" } }), NEXT));
    assert.deepEqual(await res.json(), { status: "done", output: "hello ann" });
  });

  it("rejects unsigned and badly signed calls", async () => {
    const handler = serve({ appId: "shop", functions: [hello], signingKeys: KEY });
    const unsigned = await handler(new Request("http://app.test/?fnId=hello", { method: "POST", body: payload() }));
    assert.equal(unsigned.status, 401);
    assert.match((await unsigned.json()).error, /signature/);
    const wrong = await handler(await signed(payload(), NEXT));
    assert.equal(wrong.status, 401);
    // A signature over another body.
    const req = await signed(payload());
    const tampered = new Request(req.url, { method: "POST", headers: req.headers, body: payload({ greet: { data: "forged" } }) });
    assert.equal((await handler(tampered)).status, 401);
  });

  it("can skip verification, and reads keys from the environment", async () => {
    const open = serve({ appId: "dev", functions: [hello], signingKeys: KEY, verify: false });
    assert.equal((await open(new Request("http://app.test/?fnId=hello", { method: "POST", body: payload() }))).status, 200);
    const noKeys = serve({ appId: "dev", functions: [hello], signingKeys: [] });
    assert.equal((await noKeys(new Request("http://app.test/?fnId=hello", { method: "POST", body: payload() }))).status, 200);
    process.env.ROLL_TASKS_SIGNING_KEY = KEY;
    try {
      const fromEnv = serve({ appId: "env", functions: [hello] });
      assert.equal((await fromEnv(new Request("http://app.test/?fnId=hello", { method: "POST", body: payload() }))).status, 401);
      assert.equal((await fromEnv(await signed(payload()))).status, 200);
    } finally {
      delete process.env.ROLL_TASKS_SIGNING_KEY;
    }
  });

  it("answers bad requests", async () => {
    const handler = serve({ appId: "shop", functions: [hello], verify: false });
    assert.equal((await handler(new Request("http://app.test/?fnId=nope", { method: "POST", body: payload() }))).status, 404);
    assert.equal((await handler(new Request("http://app.test/?fnId=hello", { method: "POST", body: "{not json" }))).status, 400);
    assert.equal((await handler(new Request("http://app.test/", { method: "PUT", body: "" }))).status, 405);
    // The function id falls back to the body's.
    assert.equal((await handler(new Request("http://app.test/", { method: "POST", body: payload() }))).status, 200);
  });

  it("validates its options", () => {
    assert.throws(() => serve({ appId: "bad id", functions: [] }), TypeError);
    assert.throws(() => serve({ appId: "a", functions: [hello, hello] }), TypeError);
    assert.throws(() => serve({ appId: "a", functions: [hello], signingKeys: [], verify: true }), TypeError);
  });

  it("works under node:http with toNodeHandler", async () => {
    const server = http.createServer(toNodeHandler(serve({ appId: "node", functions: [hello], signingKeys: KEY })));
    await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
    const base = `http://127.0.0.1:${(server.address() as AddressInfo).port}/api/roll`;
    try {
      const got = await fetch(base);
      assert.equal(got.status, 200);
      assert.equal((await got.json()).id, "node");
      const body = payload();
      const res = await fetch(`${base}?fnId=hello`, { method: "POST", headers: await signatureHeaders({ secrets: KEY, id: "m", body }), body });
      assert.equal(res.status, 200);
      assert.equal(res.headers.get("content-type"), "application/json");
      assert.equal((await res.json()).status, "steps");
      const bad = await fetch(`${base}?fnId=hello`, { method: "POST", body });
      assert.equal(bad.status, 401);
    } finally {
      server.close();
    }
  });
});
