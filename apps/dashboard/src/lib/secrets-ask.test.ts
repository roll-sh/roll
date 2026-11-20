import { gcm } from "@noble/ciphers/aes";
import { x25519 } from "@noble/curves/ed25519";
import { hkdf } from "@noble/hashes/hkdf";
import { sha256 } from "@noble/hashes/sha2";
import { describe, expect, it } from "vitest";
import type { SealedSecret } from "./agents-api";
import { b64url, fromB64url } from "./agents-seal";
import { answerContext, sealAnswer } from "./secrets-ask";

// A request's key pair, as roll would make one (a test key: the bytes 1…32).
const SECRET = Uint8Array.from({ length: 32 }, (_, i) => i + 1);
const PUBLIC = b64url(x25519.getPublicKey(SECRET));

/** What roll does when the answer lands (`crypto::open_sealed`). */
function open(box: SealedSecret, context: string): string {
  const epk = fromB64url(box.epk);
  const shared = x25519.getSharedSecret(SECRET, epk);
  const salt = new Uint8Array(64);
  salt.set(epk, 0);
  salt.set(fromB64url(PUBLIC), 32);
  const key = hkdf(sha256, shared, salt, new TextEncoder().encode("roll-seal-v1"), 32);
  return new TextDecoder().decode(gcm(key, fromB64url(box.nonce), new TextEncoder().encode(context)).decrypt(fromB64url(box.ct)));
}

describe("secret requests' answers", () => {
  it("seals each value so it opens only as that key of that request", async () => {
    const boxes = await sealAnswer("vrq_test", PUBLIC, { VENDOR_KEY: "fake-key", VENDOR_SECRET: "fake-secret" });
    expect(Object.keys(boxes).sort()).toEqual(["VENDOR_KEY", "VENDOR_SECRET"]);
    expect(JSON.stringify(boxes)).not.toContain("fake-");
    expect(open(boxes.VENDOR_KEY, answerContext("vrq_test", "VENDOR_KEY"))).toBe("fake-key");
    expect(open(boxes.VENDOR_SECRET, answerContext("vrq_test", "VENDOR_SECRET"))).toBe("fake-secret");
    // Swapped between keys, or replayed on another request, it doesn't open.
    expect(() => open(boxes.VENDOR_KEY, answerContext("vrq_test", "VENDOR_SECRET"))).toThrow();
    expect(() => open(boxes.VENDOR_KEY, answerContext("vrq_other", "VENDOR_KEY"))).toThrow();
  });

  it("seals, byte for byte, the box roll's test opens (a fixed ephemeral key and nonce)", async () => {
    // `crates/roll-secrets/src/value_requests.rs`'s `opens_the_answer_pages_box`.
    const fixed = { ephemeral: new Uint8Array(32).fill(7), nonce: new Uint8Array(12).fill(9) };
    const boxes = await sealAnswer("vrq_test", PUBLIC, { VENDOR_KEY: "fake-key" }, fixed);
    expect(boxes.VENDOR_KEY).toEqual(VECTOR);
  });
});

const VECTOR: SealedSecret = { v: 1, epk: "E75P6uryBMf9M1j8nAByGIHRdCeBKCJ-xnTzf3_pe20", nonce: "CQkJCQkJCQkJCQkJ", ct: "1HuLapmEhhMzGSncJaIeQhH3fUE_nFEU" };
