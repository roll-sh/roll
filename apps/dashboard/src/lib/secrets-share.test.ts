import { describe, expect, it } from "vitest";
import { b64url } from "./agents-seal";
import { openSealed, sealShare, shareKey } from "./secrets-share";

describe("one-time shares", () => {
  it("opens with the link's key and not with another", () => {
    const { ciphertext, key } = sealShare("postgres://app:hunter2@db/main\n");
    const k = shareKey(`#${key}`);
    expect(k).not.toBeNull();
    expect(openSealed(ciphertext, k!)).toBe("postgres://app:hunter2@db/main\n");
    expect(() => openSealed(ciphertext, crypto.getRandomValues(new Uint8Array(32)))).toThrow(/doesn't open/);
    expect(shareKey(b64url(new Uint8Array(16)))).toBeNull();
  });
});
