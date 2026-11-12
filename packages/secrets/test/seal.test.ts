import assert from "node:assert/strict";
import { test } from "node:test";
import { createClient, generateKeyPair, openSealed, publicKey, sealTo } from "../src/index.js";
import type { SealedValues } from "../src/index.js";

// Sealed by rolld (crates/roll-secrets/src/crypto.rs `seal_to`, through POST …/resolve with
// `seal_to`) to the key whose secret is bytes 1..32.
const secretKey = new Uint8Array(32).map((_, i) => i + 1);
const fromRust: SealedValues = {
  context: "roll-secrets:prj_fzuo337olof3:sdk-vector:app:web",
  leases: [],
  problems: [],
  sealed: {
    v: 1,
    epk: "9PO9fuY7QZoMBtaL5PMjpi6H10M9S1AwI39B8G0vFwU",
    nonce: "ksbfrm7uIhUUg1Fd",
    ct: "ynCWA9hcEEbY57aJSraE_2EF7dVVVHEZcQKs45UeXnOWMVc3Fg6lLgCp2YO8kBSphzOUGSYDPA1XdwZl6A7UrOU",
  },
};

test("a box the server sealed opens here, only for its context", () => {
  assert.equal(publicKey(secretKey), "B6N8vBQgk8i3VdwbEOhstCY3StFqqFPtC9_AsrhtHHw");
  const client = createClient({ api: "http://unused", token: "t", project: "p" });
  assert.deepEqual(client.open(fromRust, secretKey), { DATABASE_URL: "postgres://app:hunter2@db/main" });
  assert.throws(() => openSealed(fromRust.sealed, secretKey, "roll-secrets:prj_fzuo337olof3:sdk-vector:app:api"), /doesn't open/);
  assert.throws(() => openSealed(fromRust.sealed, generateKeyPair().secretKey, fromRust.context), /doesn't open/);

  const mine = generateKeyPair();
  assert.equal(new TextDecoder().decode(openSealed(sealTo(mine.publicKey, "ctx", "x"), mine.secretKey, "ctx")), "x");
});
