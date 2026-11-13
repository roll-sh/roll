import { describe, expect, it } from "vitest";
import { generateValue } from "./secrets-generate";

describe("a generated value", () => {
  it("is the length asked for, letters and digits only, and new every time", () => {
    const a = generateValue();
    expect(a).toMatch(/^[A-Za-z0-9]{48}$/);
    expect(generateValue(12)).toHaveLength(12);
    expect(generateValue()).not.toBe(a);
  });
});
