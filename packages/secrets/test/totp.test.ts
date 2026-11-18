import assert from "node:assert/strict";
import { test } from "node:test";
import { totpAt } from "../src/index.js";

const b32 = (s: string) => {
  let bits = "";
  for (const b of Buffer.from(s)) bits += b.toString(2).padStart(8, "0");
  return (bits.match(/.{1,5}/g) ?? []).map((c) => "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567"[parseInt(c.padEnd(5, "0"), 2)]).join("");
};

test("codes match RFC 6238's vectors for SHA1, SHA256 and SHA512", () => {
  // RFC 6238, Appendix B: each algorithm's own test seed, 8 digits, 30 s steps.
  const seeds = { SHA1: "12345678901234567890", SHA256: "12345678901234567890123456789012", SHA512: "1234567890".repeat(6) + "1234" };
  const vectors: [number, string[]][] = [
    [59, ["94287082", "46119246", "90693936"]],
    [1111111109, ["07081804", "68084774", "25091201"]],
    [1234567890, ["89005924", "91819424", "93441116"]],
    [20000000000, ["65353130", "77737706", "47863826"]],
  ];
  Object.entries(seeds).forEach(([alg, seed], i) => {
    const uri = `otpauth://totp/roll:test?secret=${b32(seed)}&algorithm=${alg}&digits=8`;
    for (const [at, codes] of vectors) assert.equal(totpAt(uri, at * 1000).code, codes[i], `${alg} at ${at}`);
  });
  // Bare base32: SHA1, 6 digits, 30 s.
  assert.deepEqual(totpAt("gezd gnbv gy3t qojq gezd gnbv gy3t qojq", 59_500), { code: "287082", expires_at: 60_000, period: 30 });
  assert.throws(() => totpAt("otpauth://totp/x?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ&digits=9"), (e: Error) => e.message === "digits must be 6–8");
});
