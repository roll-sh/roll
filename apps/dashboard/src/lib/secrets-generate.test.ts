import { describe, expect, it } from "vitest";
import { generateValue } from "./secrets-generate";

describe("a generated value", () => {
  it("is the length asked for, letters and digits only, and new every time", () => {
    const a = generateValue();
    expect(a).toMatch(/^[A-Za-z0-9]{48}$/);
    expect(generateValue(12)).toHaveLength(12);
    expect(generateValue()).not.toBe(a);
  });

  it("follows a key's password policy: its length, one of each class, nothing else", () => {
    for (let i = 0; i < 20; i++) {
      const v = generateValue(48, { length: 16, classes: ["lower", "digit", "symbol"], symbols: "-_" });
      expect(v).toMatch(/^[a-z0-9_-]{16}$/);
      expect(v).toMatch(/[a-z]/);
      expect(v).toMatch(/[0-9]/);
      expect(v).toMatch(/[-_]/);
    }
  });
});
