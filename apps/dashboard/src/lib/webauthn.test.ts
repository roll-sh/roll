import { describe, expect, it } from "vitest";
import { creationOptions, fromB64url, requestOptions, toB64url } from "./webauthn";

describe("passkey options", () => {
  it("round-trips base64url, padding or not", () => {
    for (const bytes of [[], [0], [0xfb, 0xff], [1, 2, 3, 4, 5]]) {
      const s = toB64url(new Uint8Array(bytes));
      expect(s).not.toMatch(/[+/=]/);
      expect([...new Uint8Array(fromB64url(s))]).toEqual(bytes);
    }
  });
  it("turns roll's JSON into what navigator.credentials takes", () => {
    const create = creationOptions({ challenge: "AQID", rp: { id: "roll.sh", name: "roll" }, user: { id: "dXNy", name: "ada@acme.com", displayName: "Ada" }, excludeCredentials: [{ type: "public-key", id: "BAU" }] });
    expect([...new Uint8Array(create.challenge as ArrayBuffer)]).toEqual([1, 2, 3]);
    expect(new TextDecoder().decode(create.user.id as ArrayBuffer)).toBe("usr");
    expect([...new Uint8Array(create.excludeCredentials![0].id as ArrayBuffer)]).toEqual([4, 5]);
    expect(create.rp).toEqual({ id: "roll.sh", name: "roll" });
    const get = requestOptions({ challenge: "AQID", rpId: "roll.sh", userVerification: "required" });
    expect(get.allowCredentials).toEqual([]);
    expect(get.userVerification).toBe("required");
  });
});
