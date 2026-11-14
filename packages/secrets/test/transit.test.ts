import assert from "node:assert/strict";
import { test } from "node:test";
import { createClient } from "../src/index.js";

test("transit sends UTF-8 and bytes as base64 and decrypts back to the same bytes", async () => {
  const sent: Record<string, unknown>[] = [];
  const fetch = (async (url: string | URL | Request, init?: RequestInit) => {
    const u = new URL(String(url));
    const body = JSON.parse(String(init?.body)) as Record<string, string>;
    sent.push({ path: u.pathname, ...body });
    // A server that "encrypts" by echoing: enough to check the encoding both ways.
    const out = u.pathname.endsWith("/encrypt") ? { ciphertext: `roll:v1:${body.plaintext}` } : { plaintext: body.ciphertext.slice("roll:v1:".length) };
    return new Response(JSON.stringify(out), { status: 200 });
  }) as typeof globalThis.fetch;
  const cards = createClient({ api: "http://rolld.test", token: "rlst_test", project: "prj_1", fetch }).transit("cards");

  const ct = await cards.encrypt("héllo ✓", "order:42");
  assert.deepEqual(sent[0], { path: "/v1/secrets/projects/prj_1/transit/cards/encrypt", plaintext: Buffer.from("héllo ✓").toString("base64"), context: "order:42" });
  assert.equal(new TextDecoder().decode(await cards.decrypt(ct, "order:42")), "héllo ✓");
  const raw = new Uint8Array([0, 255, 128, 7]);
  assert.deepEqual(await cards.decrypt(await cards.encrypt(raw)), raw);
});
