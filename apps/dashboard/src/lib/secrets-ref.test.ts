import { describe, expect, it } from "vitest";
import { rollRef } from "./secrets-ref";

describe("rollRef", () => {
  it("writes the reference roll run --env-file resolves, folder included", () => {
    expect(rollRef("production", "/", "DATABASE_URL")).toBe("roll://production/DATABASE_URL");
    expect(rollRef("staging", "/payments/eu", "STRIPE_KEY")).toBe("roll://staging/payments/eu/STRIPE_KEY");
    expect(rollRef("staging", "/a b", "K")).toBeNull();
  });
});
