import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { Receiver, SignatureError, deliveryInfo, sign, signatureHeaders, verifySignature } from "../src/index.js";

const SECRET = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw";
const ID = "msg_p5jXN8AQM9LWM0D4loKWxJek";
const TS = 1614265330;
const BODY = '{"test": 2432232314}';
const EXPECTED = "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=";
const OTHER = "whsec_" + btoa("another secret, 32 bytes long!!!");

const headers = (sig = EXPECTED, ts = String(TS), id = ID) => ({ "webhook-id": id, "webhook-timestamp": ts, "webhook-signature": sig });

async function rejects(p: Promise<unknown>, reason: string) {
  await assert.rejects(p, (e: unknown) => {
    assert.ok(e instanceof SignatureError, `expected SignatureError, got ${String(e)}`);
    assert.equal(e.reason, reason);
    assert.equal(e.name, "SignatureError");
    return true;
  });
}

describe("signatures", () => {
  it("matches the Standard Webhooks test vector", async () => {
    assert.equal(await sign({ secret: SECRET, id: ID, timestamp: TS, body: BODY }), EXPECTED);
    // Without the prefix, and with a byte body.
    assert.equal(await sign({ secret: SECRET.slice(6), id: ID, timestamp: TS, body: new TextEncoder().encode(BODY) }), EXPECTED);
  });

  it("verifies the vector", async () => {
    await verifySignature({ secrets: SECRET, headers: headers(), body: BODY, now: TS });
    await verifySignature({ secrets: [OTHER, SECRET], headers: new Headers(headers()), body: new TextEncoder().encode(BODY), now: TS + 299 });
    await verifySignature({ secrets: SECRET, headers: Object.entries(headers()), body: BODY, now: TS });
    // Header names are case-insensitive; any of several signatures may match.
    await verifySignature({
      secrets: SECRET,
      headers: { "Webhook-Id": ID, "WEBHOOK-TIMESTAMP": String(TS), "webhook-signature": `v1,AAAA ${EXPECTED} v2,whatever` },
      body: BODY,
      now: TS,
    });
  });

  it("rejects a tampered body", async () => {
    await rejects(verifySignature({ secrets: SECRET, headers: headers(), body: '{"test": 2432232315}', now: TS }), "mismatch");
  });

  it("rejects the wrong secret", async () => {
    await rejects(verifySignature({ secrets: OTHER, headers: headers(), body: BODY, now: TS }), "mismatch");
  });

  it("rejects a different id or timestamp", async () => {
    await rejects(verifySignature({ secrets: SECRET, headers: headers(EXPECTED, String(TS), "msg_other"), body: BODY, now: TS }), "mismatch");
    await rejects(verifySignature({ secrets: SECRET, headers: headers(EXPECTED, String(TS + 1)), body: BODY, now: TS }), "mismatch");
  });

  it("rejects expired and future timestamps", async () => {
    await rejects(verifySignature({ secrets: SECRET, headers: headers(), body: BODY, now: TS + 301 }), "expired");
    await rejects(verifySignature({ secrets: SECRET, headers: headers(), body: BODY, now: TS - 301 }), "expired");
    await verifySignature({ secrets: SECRET, headers: headers(), body: BODY, now: TS + 1000, toleranceSeconds: 1000 });
  });

  it("rejects missing or malformed headers", async () => {
    await rejects(verifySignature({ secrets: SECRET, headers: {}, body: BODY, now: TS }), "missing");
    await rejects(verifySignature({ secrets: SECRET, headers: { "webhook-id": ID, "webhook-timestamp": String(TS) }, body: BODY, now: TS }), "missing");
    await rejects(verifySignature({ secrets: SECRET, headers: { "webhook-signature": EXPECTED, "webhook-timestamp": String(TS) }, body: BODY, now: TS }), "missing");
    await rejects(verifySignature({ secrets: SECRET, headers: headers(EXPECTED, "soon"), body: BODY, now: TS }), "invalid");
    await rejects(verifySignature({ secrets: SECRET, headers: headers("v1,not base64!!"), body: BODY, now: TS }), "mismatch");
    await rejects(verifySignature({ secrets: SECRET, headers: headers(EXPECTED.replace("v1,", "v2,")), body: BODY, now: TS }), "mismatch");
    await rejects(verifySignature({ secrets: [], headers: headers(), body: BODY, now: TS }), "invalid");
  });

  it("signs with every secret and round-trips", async () => {
    const h = await signatureHeaders({ secrets: [SECRET, OTHER], id: "msg_1", body: "hello" });
    assert.equal(h["webhook-signature"].split(" ").length, 2);
    await verifySignature({ secrets: OTHER, headers: h, body: "hello" });
    await verifySignature({ secrets: SECRET, headers: h, body: "hello" });
  });

  it("Receiver verifies Fetch requests and reports validity", async () => {
    const r = new Receiver({ secrets: [OTHER, SECRET] });
    const h = await signatureHeaders({ secrets: SECRET, id: "msg_2", body: "payload" });
    const req = new Request("http://x/hook", { method: "POST", headers: h, body: "payload" });
    assert.equal(await r.verifyRequest(req), "payload");
    assert.equal(await r.isValid({ headers: h, body: "payload" }), true);
    assert.equal(await r.isValid({ headers: h, body: "tampered" }), false);
    const bad = new Request("http://x/hook", { method: "POST", headers: h, body: "nope" });
    await rejects(r.verifyRequest(bad), "mismatch");
    assert.throws(() => new Receiver({ secrets: [] }), TypeError);
  });

  it("Receiver reads secrets from the environment", async () => {
    process.env.ROLL_TASKS_SIGNING_KEY = SECRET;
    try {
      const r = new Receiver();
      assert.deepEqual(r.secrets, [SECRET]);
      await r.verify({ headers: headers(), body: BODY, now: TS });
    } finally {
      delete process.env.ROLL_TASKS_SIGNING_KEY;
    }
  });

  it("reads delivery headers", () => {
    const info = deliveryInfo({
      "webhook-id": "msg_1",
      "roll-message-id": "msg_1",
      "roll-retried": "2",
      "roll-retries": "3",
      "roll-queue": "emails",
      "roll-label": "welcome",
    });
    assert.deepEqual(info, {
      id: "msg_1",
      messageId: "msg_1",
      retried: 2,
      retries: 3,
      queue: "emails",
      topic: undefined,
      scheduleId: undefined,
      label: "welcome",
      callbackFor: undefined,
    });
    assert.equal(deliveryInfo({}).retried, 0);
  });
});
